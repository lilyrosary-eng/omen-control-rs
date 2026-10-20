//! 风扇曲线编辑器 —— GTK4 `DrawingArea` + cairo 自定义绘制。
//!
//! 横轴是温度（30~100°C），纵轴是转速（1992~4597 RPM，就是实测可控区间）。
//!
//! 交互（都做了可发现的提示，见 `hint` 行）：
//!
//! - **拖动圆点** = 改这个点的温度/转速（自动夹取到合法范围，且保持温度单调）
//! - **双击空白** = 在该处加一个点
//! - **双击圆点** = 删掉它（至少保留 1 个点）
//!
//! 为什么纵轴卡在 1992~4597：这是实测标定出来的**有效可控区间**。
//! 低于 1992（20%）是断崖区（实测 5% 只剩 87 RPM），高于 4597（46%）是饱和区
//! （60% 与 100% 完全等价）。把范围锁死在这里，用户拖不出无意义的曲线。

use std::cell::RefCell;
use std::rc::Rc;

use gtk::cairo;
use gtk::prelude::*;

use crate::curve::{CurvePoint, FanCurve};

/// 三栏边距：左侧留给纵轴刻度文字，下方留给横轴刻度文字。
const PAD_L: f64 = 48.0;
const PAD_R: f64 = 16.0;
const PAD_T: f64 = 16.0;
const PAD_B: f64 = 32.0;

const T_MIN: f64 = FanCurve::MIN_TEMP as f64;
const T_MAX: f64 = FanCurve::MAX_TEMP as f64;
const R_MIN: f64 = FanCurve::MIN_RPM as f64;
const R_MAX: f64 = FanCurve::MAX_RPM as f64;

/// 命中半径（像素）：拖拽和双击都以它判定"点到了某个圆点"。
const HIT_R: f64 = 14.0;

/// 画布高度。宽度交给布局。
const AREA_H: i32 = 210;

struct Colors {
    bg: (f64, f64, f64),
    grid: (f64, f64, f64),
    axis: (f64, f64, f64),
    text: (f64, f64, f64),
    line: (f64, f64, f64),
    dot: (f64, f64, f64),
    sel: (f64, f64, f64),
    off: (f64, f64, f64),
}

fn colors() -> Colors {
    // 深色/浅色各一套。只求"两套主题下都读得清"，不做花哨配色。
    let dark = adw::StyleManager::default().is_dark();
    if dark {
        Colors {
            bg: (0.11, 0.11, 0.12),
            grid: (0.22, 0.22, 0.24),
            axis: (0.35, 0.35, 0.38),
            text: (0.78, 0.78, 0.80),
            line: (0.42, 0.62, 0.98),
            dot: (0.55, 0.72, 1.00),
            sel: (1.00, 0.78, 0.35),
            off: (0.30, 0.30, 0.32),
        }
    } else {
        Colors {
            bg: (0.99, 0.99, 0.99),
            grid: (0.90, 0.90, 0.91),
            axis: (0.72, 0.72, 0.74),
            text: (0.28, 0.28, 0.30),
            line: (0.11, 0.38, 0.76),
            dot: (0.16, 0.45, 0.86),
            sel: (0.85, 0.55, 0.05),
            off: (0.55, 0.55, 0.57),
        }
    }
}

struct State {
    points: Vec<CurvePoint>,
    /// 正在拖的点（drag_begin 时确定，drag 过程中不变）
    drag: Option<usize>,
    /// 该点的原始值 —— GTK4 的 drag_update 给的是**偏移量**，必须自己记起点。
    drag_orig: Option<CurvePoint>,
}

pub struct CurveEditor {
    pub widget: gtk::Box,
    area: gtk::DrawingArea,
    st: Rc<RefCell<State>>,
    hint: gtk::Label,
    on_changed: Rc<RefCell<Option<Box<dyn Fn(&[CurvePoint])>>>>,
}

fn to_px(t: f64, r: f64, w: f64, h: f64) -> (f64, f64) {
    let iw = w - PAD_L - PAD_R;
    let ih = h - PAD_T - PAD_B;
    (
        PAD_L + (t - T_MIN) / (T_MAX - T_MIN) * iw,
        PAD_T + (R_MAX - r) / (R_MAX - R_MIN) * ih,
    )
}

fn to_data(px: f64, py: f64, w: f64, h: f64) -> (f64, f64) {
    let iw = w - PAD_L - PAD_R;
    let ih = h - PAD_T - PAD_B;
    (
        T_MIN + (px - PAD_L) / iw * (T_MAX - T_MIN),
        R_MAX - (py - PAD_T) / ih * (R_MAX - R_MIN),
    )
}

impl CurveEditor {
    pub fn new(points: Vec<CurvePoint>) -> Rc<Self> {
        let mut curve = FanCurve { points };
        curve.normalize();

        let area = gtk::DrawingArea::new();
        area.set_content_height(AREA_H);
        area.set_hexpand(true);
        area.set_draw_func(|_, _, _, _| {}); // 真正的绘制函数在下面替换（需要先拿到 st）

        let hint = gtk::Label::new(None);
        hint.set_xalign(0.0);
        hint.set_wrap(true);
        hint.set_margin_top(6);
        hint.add_css_class("dim-label");
        hint.add_css_class("caption");

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&area);
        widget.append(&hint);

        let st = Rc::new(RefCell::new(State {
            points: curve.points,
            drag: None,
            drag_orig: None,
        }));

        let ed = Rc::new(CurveEditor {
            widget,
            area: area.clone(),
            st: st.clone(),
            hint: hint.clone(),
            on_changed: Rc::new(RefCell::new(None)),
        });

        // ── 绘制 ──
        {
            let st2 = st.clone();
            area.set_draw_func(move |_a, cr, w, h| draw(cr, w, h, &st2));
        }

        // ── 拖动圆点 ──
        {
            let st2 = st.clone();
            let area2 = area.clone();
            let ed2 = ed.clone();
            let drag = gtk::GestureDrag::new();
            drag.connect_drag_begin(move |_, x, y| {
                let (w, h) = (area2.width() as f64, area2.height() as f64);
                let mut s = st2.borrow_mut();
                s.drag = nearest(&s.points, x, y, w, h);
                s.drag_orig = s.drag.map(|i| s.points[i]);
                if let Some(i) = s.drag {
                    let p = s.points[i];
                    ed2.set_hint_for(p);
                }
                drop(s);
                area2.queue_draw();
            });
            let st3 = st.clone();
            let area3 = area.clone();
            let ed3 = ed.clone();
            drag.connect_drag_update(move |_, ox, oy| {
                let (w, h) = (area3.width() as f64, area3.height() as f64);
                let mut s = st3.borrow_mut();
                let Some(i) = s.drag else { return };
                let Some(orig) = s.drag_orig else { return };
                // GTK4 的 drag_update 给的是**相对起点的偏移（像素）**，不是绝对坐标。
                // 所以：该点的原始像素位置 + 偏移 → 再换回数据坐标。
                let (sx, sy) = to_px(orig.temp as f64, orig.rpm as f64, w, h);
                let (nt, nr) = to_data(sx + ox, sy + oy, w, h);
                let mut t = nt.round().clamp(T_MIN, T_MAX) as u8;
                let r = nr.round().clamp(R_MIN, R_MAX) as u16;
                // 保持温度严格递增：夹在左右邻点之间（否则插值会除零、语义也乱）
                let lo = if i > 0 { s.points[i - 1].temp + 1 } else { FanCurve::MIN_TEMP };
                let hi = if i + 1 < s.points.len() {
                    s.points[i + 1].temp.saturating_sub(1)
                } else {
                    FanCurve::MAX_TEMP
                };
                if lo <= hi {
                    t = t.clamp(lo, hi);
                } else {
                    t = orig.temp; // 空间不够就不动横轴，只让纵轴动
                }
                s.points[i] = CurvePoint { temp: t, rpm: r };
                let p = s.points[i];
                drop(s);
                ed3.set_hint_for(p);
                area3.queue_draw();
            });
            let st4 = st.clone();
            let ed4 = ed.clone();
            drag.connect_drag_end(move |_, _, _| {
                let mut s = st4.borrow_mut();
                s.drag = None;
                s.drag_orig = None;
                let pts = s.points.clone();
                drop(s);
                ed4.set_hint_default();
                ed4.fire_changed(&pts);
            });
            area.add_controller(drag);
        }

        // ── 双击：加点 / 删点 ──
        {
            let st2 = st.clone();
            let area2 = area.clone();
            let ed2 = ed.clone();
            let click = gtk::GestureClick::new();
            click.connect_pressed(move |_, n_press, x, y| {
                if n_press != 2 {
                    return;
                }
                let (w, h) = (area2.width() as f64, area2.height() as f64);
                let mut s = st2.borrow_mut();
                match nearest(&s.points, x, y, w, h) {
                    Some(i) if s.points.len() > 1 => {
                        s.points.remove(i);
                        ed2.set_hint_text("已删除一个点。双击空白处可再加。");
                    }
                    Some(_) => {
                        ed2.set_hint_text("至少要保留一个点 —— 想完全不接管就把曲线清空。");
                    }
                    None => {
                        if s.points.len() >= FanCurve::MAX_POINTS {
                            ed2.set_hint_text("点数已达上限（12 个）。");
                        } else {
                            let (t, r) = to_data(x, y, w, h);
                            let p = CurvePoint {
                                temp: (t.round() as u8).clamp(FanCurve::MIN_TEMP, FanCurve::MAX_TEMP),
                                rpm: (r.round() as u16).clamp(FanCurve::MIN_RPM, FanCurve::MAX_RPM),
                            };
                            // 同温度已有别的点 ⇒ 拒绝（避免出现两个点叠在一条竖线上）
                            if s.points.iter().any(|q| q.temp == p.temp) {
                                ed2.set_hint_text("该温度已有圆点，直接拖它就行。");
                            } else {
                                s.points.push(p);
                                let mut c = FanCurve { points: s.points.clone() };
                                c.normalize();
                                s.points = c.points;
                                ed2.set_hint_text("已加一个点。");
                            }
                        }
                    }
                }
                let pts = s.points.clone();
                drop(s);
                area2.queue_draw();
                ed2.fire_changed(&pts);
            });
            area.add_controller(click);
        }

        ed.set_hint_default();
        ed
    }

    /// 找出落在 `(x,y)` 命中半径内、且**离得最近**的那个点。
    fn nearest_in(points: &[CurvePoint], x: f64, y: f64, w: f64, h: f64) -> (Option<usize>, f64) {
        let mut best: (Option<usize>, f64) = (None, HIT_R);
        for (i, p) in points.iter().enumerate() {
            let (px, py) = to_px(p.temp as f64, p.rpm as f64, w, h);
            let d = ((px - x).powi(2) + (py - y).powi(2)).sqrt();
            if d < best.1 {
                best = (Some(i), d);
            }
        }
        best
    }

    fn set_hint_for(&self, p: CurvePoint) {
        let pct = crate::curve::rpm_to_pct(p.rpm);
        self.hint.set_text(&format!(
            "当前点：{}°C → {} RPM（写入 {}%）",
            p.temp, p.rpm, pct
        ));
    }

    fn set_hint_text(&self, s: &str) {
        self.hint.set_text(s);
    }

    fn set_hint_default(&self) {
        self.hint.set_text(
            "拖动圆点改曲线；双击空白加点，双击圆点删点。\
             首点左侧的区间交回 BIOS —— 那一段它给的曲线本来就平滑、安静。",
        );
    }

    fn fire_changed(&self, pts: &[CurvePoint]) {
        if let Some(f) = self.on_changed.borrow().as_ref() {
            f(pts);
        }
    }

    /// 替换整条曲线（切档位时用）。
    pub fn set_points(&self, points: Vec<CurvePoint>) {
        let mut c = FanCurve { points };
        c.normalize();
        self.st.borrow_mut().points = c.points;
        self.area.queue_draw();
    }

    /// 读回当前曲线（目前只有测试/将来的「手动保存」会用）。
    #[allow(dead_code)]
    pub fn points(&self) -> Vec<CurvePoint> {
        self.st.borrow().points.clone()
    }

    pub fn set_on_changed<F: Fn(&[CurvePoint]) + 'static>(&self, f: F) {
        *self.on_changed.borrow_mut() = Some(Box::new(f));
    }

    /// 置灰 / 恢复整块编辑器 —— 守护没跑时由 `ui.rs` 调用（曲线此时不生效）。
    pub fn set_sensitive_editor(&self, on: bool) {
        self.area.set_sensitive(on);
    }
}

fn nearest(points: &[CurvePoint], x: f64, y: f64, w: f64, h: f64) -> Option<usize> {
    CurveEditor::nearest_in(points, x, y, w, h).0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 像素 <-> 数据 必须能来回换算（拖动就靠这条：像素偏移 -> 数据坐标）
    #[test]
    fn px_data_roundtrip() {
        let (w, h) = (600.0, AREA_H as f64);
        for &(t, r) in &[(30.0, R_MIN), (55.0, 2400.0), (80.0, R_MAX), (100.0, 3000.0)] {
            let (px, py) = to_px(t, r, w, h);
            let (t2, r2) = to_data(px, py, w, h);
            assert!((t2 - t).abs() < 0.01, "temp roundtrip failed");
            assert!((r2 - r).abs() < 0.01, "rpm roundtrip failed");
        }
    }

    /// 四个角要正好落在绘图区边框上（否则曲线会画到轴外面）
    #[test]
    fn corners_map_to_plot_edges() {
        let (w, h) = (600.0, AREA_H as f64);
        let (x0, y0) = to_px(T_MIN, R_MAX, w, h);
        assert!((x0 - PAD_L).abs() < 0.01);
        assert!((y0 - PAD_T).abs() < 0.01);
        let (x1, y1) = to_px(T_MAX, R_MIN, w, h);
        assert!((x1 - (w - PAD_R)).abs() < 0.01);
        assert!((y1 - (h - PAD_B)).abs() < 0.01);
    }
}

fn draw(cr: &cairo::Context, w: i32, h: i32, st: &Rc<RefCell<State>>) {
    let (w, h) = (w as f64, h as f64);
    if w <= PAD_L + PAD_R || h <= PAD_T + PAD_B {
        return;
    }
    let c = colors();

    // 背景
    cr.set_source_rgb(c.bg.0, c.bg.1, c.bg.2);
    cr.rectangle(0.0, 0.0, w, h);
    cr.fill().ok();

    let (iw, ih) = (w - PAD_L - PAD_R, h - PAD_T - PAD_B);

    // 网格：横轴每 10°C，纵轴每 500 RPM
    cr.set_line_width(1.0);
    cr.set_source_rgb(c.grid.0, c.grid.1, c.grid.2);
    let mut t = 40.0;
    while t < T_MAX {
        let (px, _) = to_px(t, R_MIN, w, h);
        cr.move_to(px, PAD_T);
        cr.line_to(px, PAD_T + ih);
        t += 10.0;
    }
    let mut r = (R_MIN / 500.0).ceil() * 500.0;
    while r < R_MAX {
        let (_, py) = to_px(T_MIN, r, w, h);
        cr.move_to(PAD_L, py);
        cr.line_to(PAD_L + iw, py);
        r += 500.0;
    }
    cr.stroke().ok();

    // 坐标轴
    cr.set_source_rgb(c.axis.0, c.axis.1, c.axis.2);
    cr.set_line_width(1.0);
    cr.move_to(PAD_L, PAD_T);
    cr.line_to(PAD_L, PAD_T + ih);
    cr.line_to(PAD_L + iw, PAD_T + ih);
    cr.stroke().ok();

    // 刻度文字
    cr.set_source_rgb(c.text.0, c.text.1, c.text.2);
    cr.set_font_size(10.0);
    let mut t = 40.0;
    while t <= 100.0 {
        let (px, _) = to_px(t, R_MIN, w, h);
        let s = format!("{t:.0}");
        if let Ok(e) = cr.text_extents(&s) {
            cr.move_to(px - e.width() / 2.0, PAD_T + ih + 14.0);
            cr.show_text(&s).ok();
        }
        t += 10.0;
    }
    for r in [R_MIN, 3000.0, 4000.0, R_MAX] {
        let (_, py) = to_px(T_MIN, r, w, h);
        let s = format!("{r:.0}");
        if let Ok(e) = cr.text_extents(&s) {
            cr.move_to(PAD_L - 6.0 - e.width(), py + 3.5);
            cr.show_text(&s).ok();
        }
    }
    // 轴名
    if let Ok(e) = cr.text_extents("转速 RPM") {
        cr.move_to(PAD_L, PAD_T + ih + 26.0);
        cr.show_text("温度 °C").ok();
        let _ = e;
    }

    let s = st.borrow();
    let pts = &s.points;
    if pts.is_empty() {
        drop(s);
        return;
    }

    // 首点左侧：交回固件的区间 —— 画一条虚线示意
    let first = pts[0];
    let (fx, _) = to_px(first.temp as f64, R_MIN, w, h);
    if fx > PAD_L {
        cr.set_source_rgb(c.off.0, c.off.1, c.off.2);
        cr.set_line_width(1.0);
        cr.set_dash(&[3.0, 3.0], 0.0);
        cr.move_to(fx, PAD_T);
        cr.line_to(fx, PAD_T + ih);
        cr.stroke().ok();
        cr.set_dash(&[], 0.0);
    }

    // 首点往左那一段用**灰色虚线**：它表示"低于首点温度就交回固件"，
    // 不是"给这个转速"。画成实线会被误读成"55°C 以下也是 2400 RPM"。
    let (px0, py0) = to_px(first.temp as f64, first.rpm as f64, w, h);
    cr.set_source_rgb(c.off.0, c.off.1, c.off.2);
    cr.set_line_width(1.5);
    cr.set_dash(&[4.0, 4.0], 0.0);
    cr.move_to(PAD_L + 1.0, py0);
    cr.line_to(px0, py0);
    cr.stroke().ok();
    cr.set_dash(&[], 0.0);

    // 曲线本体：从首点开始
    cr.set_source_rgb(c.line.0, c.line.1, c.line.2);
    cr.set_line_width(2.0);
    cr.move_to(px0, py0);
    for p in pts.iter().skip(1) {
        let (px, py) = to_px(p.temp as f64, p.rpm as f64, w, h);
        cr.line_to(px, py);
    }
    // 末点往右拉平（这条是真的：target_rpm 对超过末点温度返回末点转速）
    let last = *pts.last().unwrap();
    let (_, ly) = to_px(last.temp as f64, last.rpm as f64, w, h);
    cr.line_to(PAD_L + iw - 1.0, ly);
    cr.stroke().ok();

    // 圆点
    for (i, p) in pts.iter().enumerate() {
        let (px, py) = to_px(p.temp as f64, p.rpm as f64, w, h);
        let dragging = s.drag == Some(i);
        let (col, rad) = if dragging { (c.sel, 6.0) } else { (c.dot, 4.5) };
        cr.set_source_rgb(col.0, col.1, col.2);
        cr.arc(px, py, rad, 0.0, std::f64::consts::TAU);
        cr.fill().ok();
        // 描一圈背景色，避免点和大红大蓝的线糊在一起
        cr.set_source_rgb(c.bg.0, c.bg.1, c.bg.2);
        cr.set_line_width(1.5);
        cr.arc(px, py, rad, 0.0, std::f64::consts::TAU);
        cr.stroke().ok();
    }
}
