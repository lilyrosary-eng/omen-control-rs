//! 监控浮窗（HUD）。
//!
//! 一个独立的小窗口，只做**只读监视**：CPU 温度/频率、GPU 温度/频率/占用、
//! 双风扇转速、主板温度。1 秒刷新一次。
//!
//! 设计要点：
//! 1. **独立的 application_id**（`org.rosary.OmenControl.Hud`）。这样它可以作为
//!    独立进程运行、单独设开机自启，也不会和主面板的单实例机制互相干扰
//!    （同一个 app id 的话，第二次启动只会激活第一个实例，`--hud` 参数会被丢掉）。
//! 2. **GPU 采样仍在后台线程**（`nvidia-smi` 约 60 ms，不能放主线程）。
//! 3. **不能自己置顶、也不能自己定位**：GNOME/Wayland 的安全模型不允许普通应用
//!    强制置顶或指定窗口初始位置（GTK4 也移除了 `set_keep_above` / `set_position`）。
//!    要置顶请右键标题栏 →「置于顶层」。这一点已写进 README，不假装能做到。
//!
//! # 透明度的两条独立通道
//!
//! 用户要求把透明度拆成两条，因为**它们必须解耦**：窗口底色可以很淡，
//! 但真正的读数仍要清晰可读。
//!
//! - **有效读数** → 直接给承载数字的 `Label` 设 `set_opacity()`；
//! - **窗口底色 + 框架控件 + 无效占位（"—"）** → 底色走 **CSS**
//!   （`alpha(@theme_bg_color, x)`，只改背景色的 alpha），
//!   其余控件各自设 `set_opacity()`。
//!
//! 为什么不用 `window.set_opacity()` 一把梭：GTK4 的 opacity 是**递归**的，
//! 设一次会把整棵控件树一起变淡——就无法做到"底色很淡、数字很实"。
//! 同理，"有效字体"和"无效字体"必须是**两个不同的 Label** 才能分别设透明度，
//! 所以每行指标拆成两个标签，用 `set_visible` 切换。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use adw::{ApplicationWindow, HeaderBar, ToolbarView};
use gtk::glib;
use gtk::{Box, Button, Grid, Label, Orientation, Scale, Widget};
use std::process::Command;

use crate::hw;

const HUD_APP_ID: &str = "org.rosary.OmenControl.Hud";
/// 刷新间隔（毫秒）。比主面板快一些，浮窗就是要"随时看一眼"。
const REFRESH_MS: u64 = 1000;
/// GPU 后台采样间隔（秒）。
const GPU_POLL_SECS: u64 = 2;
/// 底色 CSS 的类名。
const BG_CLASS: &str = "omen-hud";

/// 一行指标。**有效读数与无效占位是两个独立 Label**，
/// 这样"有效字体"和"无效字体"才能分别调透明度（同一个 Label 做不到）。
struct Metric {
    valid: Label,
    invalid: Label,
}

impl Metric {
    /// `Some(文本)` = 有读数，显示正常字号的 Label；
    /// `None` = 无数据，显示"—"占位（更淡）。
    fn set(&self, text: Option<String>) {
        match text {
            Some(t) => {
                self.valid.set_text(&t);
                self.valid.set_visible(true);
                self.invalid.set_visible(false);
            }
            None => {
                self.valid.set_visible(false);
                self.invalid.set_visible(true);
            }
        }
    }
}

/// 建一行指标。`chrome` 里收集"应该跟着窗口/框架透明度走"的控件。
fn metric(grid: &Grid, row: i32, name: &str, chrome: &mut Vec<Widget>) -> Metric {
    let n = Label::new(Some(name));
    n.set_xalign(0.0);
    n.add_css_class("dim-label");
    chrome.push(n.clone().upcast());

    let valid = Label::new(Some("--"));
    valid.set_xalign(1.0);
    valid.set_hexpand(true);
    valid.add_css_class("numeric");

    let invalid = Label::new(Some("—"));
    invalid.set_xalign(1.0);
    invalid.set_hexpand(true);
    invalid.add_css_class("dim-label");
    invalid.set_visible(false);
    chrome.push(invalid.clone().upcast());

    let cell = Box::new(Orientation::Horizontal, 0);
    cell.append(&valid);
    cell.append(&invalid);

    grid.attach(&n, 0, row, 1, 1);
    grid.attach(&cell, 1, row, 1, 1);
    Metric { valid, invalid }
}

pub fn run() {
    adw::init().ok();
    let app = adw::Application::builder().application_id(HUD_APP_ID).build();
    // 单窗口：浮窗的「主面板」按钮、主面板的"隐藏时确保浮窗在跑"都会重复拉起本程序，
    // 单实例转发后 activate 会再次触发 —— 这里必须只 present 同一个窗口。
    let win: Rc<RefCell<Option<ApplicationWindow>>> = Rc::new(RefCell::new(None));
    app.connect_activate({
        let win = Rc::clone(&win);
        move |app| {
            // 同 ui.rs：不能写成 `match win.borrow()`，分支里的 borrow_mut 会 panic
            let existing = win.borrow().clone();
            if let Some(w) = existing {
                w.present();
            } else {
                let w = build_window(app);
                *win.borrow_mut() = Some(w.clone());
                w.present();
            }
        }
    });
    // 同样必须给一个干净的 argv：否则 `--hud` 会被 GApplication 当成未知选项。
    app.run_with_args::<&str>(&["omen-control-hud"]);
}

fn build_window(app: &adw::Application) -> ApplicationWindow {
    let win = ApplicationWindow::builder()
        .application(app)
        .title("OMEN 监控")
        .default_width(310)
        .default_height(250)
        .resizable(true)
        .build();

    let cfg = hw::load_config();
    let text_op = cfg.hud_opacity_text.clamp(0.3, 1.0);
    let chrome_op = cfg.hud_opacity_chrome.clamp(0.15, 1.0);

    // ---------------------------------------------------------------- 顶栏（框架）
    let header = HeaderBar::new();
    // 「打开主面板」：以普通参数启动同一个可执行文件（主面板是另一个 app id）
    let open_btn = Button::from_icon_name("window-new-symbolic");
    open_btn.set_tooltip_text(Some("打开主面板"));
    open_btn.connect_clicked(|_| {
        if let Ok(exe) = std::env::current_exe() {
            let _ = Command::new(exe).spawn();
        }
    });
    header.pack_start(&open_btn);

    let close_btn = Button::from_icon_name("window-close-symbolic");
    close_btn.set_tooltip_text(Some("关闭浮窗"));
    {
        let win_weak = win.downgrade();
        close_btn.connect_clicked(move |_| {
            if let Some(w) = win_weak.upgrade() {
                w.close();
            }
        });
    }
    header.pack_end(&close_btn);

    // ---------------------------------------------------------------- 内容
    let mut chrome: Vec<Widget> = vec![header.clone().upcast()];

    let grid = Grid::builder()
        .row_spacing(8)
        .column_spacing(18)
        .margin_top(14)
        .margin_bottom(4)
        .margin_start(16)
        .margin_end(16)
        .build();

    let cpu = metric(&grid, 0, "CPU", &mut chrome);
    let gpu = metric(&grid, 1, "GPU", &mut chrome);
    let fans = metric(&grid, 2, "风扇", &mut chrome);
    let board = metric(&grid, 3, "主板", &mut chrome);

    let body = Box::new(Orientation::Vertical, 0);
    body.append(&grid);

    // ---------------------------------------------------------------- 两条透明度
    //
    // ② 窗口 / 底色 / 无效读数：底色用 CSS 改 alpha（只影响背景色，不牵连文字），
    //    其余框架控件用 set_opacity。
    let css = gtk::CssProvider::new();
    css.load_from_string(&format!(
        "window.{BG_CLASS} {{ background-color: alpha(@theme_bg_color, {chrome_op:.3}); }}"
    ));
    win.add_css_class(BG_CLASS);
    // 用 display 级入口而不是 `widget.style_context().add_provider()`——
    // 后者自 GTK 4.10 起已废弃（开启 v4_12 feature 后会告警）。
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let mk_row = |name: &str, value: f64, min: f64, tip: &str| -> gtk::Box {
        let l = Label::new(Some(name));
        l.set_xalign(0.0);
        l.set_size_request(104, -1);
        l.add_css_class("dim-label");
        l.set_tooltip_text(Some(tip));
        let scale = Scale::with_range(Orientation::Horizontal, min, 1.0, 0.05);
        scale.set_draw_value(false);
        scale.set_value(value);
        scale.set_hexpand(true);
        let row = Box::new(Orientation::Horizontal, 10);
        row.append(&l);
        row.append(&scale);
        row
    };

    let text_row = mk_row(
        "有效读数",
        text_op,
        0.3,
        "真正有数据的那些数字。窗口底色再淡，它们也保持这个清晰度",
    );
    let chrome_row = mk_row(
        "窗口/底色",
        chrome_op,
        0.15,
        "同时作用于：窗口底色、标题栏与标签等框架、以及无数据时的「—」占位",
    );

    let text_scale = find_scale(&text_row);
    let chrome_scale = find_scale(&chrome_row);

    let sep = gtk::Separator::new(Orientation::Horizontal);
    let opta = Box::new(Orientation::Vertical, 4);
    opta.set_margin_top(2);
    opta.append(&sep);
    opta.append(&text_row);
    opta.append(&chrome_row);
    body.append(&opta);

    // 有效读数标签（唯一走「有效读数」透明度的一组）
    let valid_labels: Vec<Label> = vec![
        cpu.valid.clone(),
        gpu.valid.clone(),
        fans.valid.clone(),
        board.valid.clone(),
    ];
    let apply_text: Rc<dyn Fn(f64)> = Rc::new(move |v: f64| {
        for l in &valid_labels {
            l.set_opacity(v);
        }
    });
    let apply_chrome: Rc<dyn Fn(f64)> = {
        let widgets = chrome.clone();
        let css = css.clone();
        Rc::new(move |v: f64| {
            css.load_from_string(&format!(
                "window.{BG_CLASS} {{ background-color: alpha(@theme_bg_color, {v:.3}); }}"
            ));
            for w in &widgets {
                w.set_opacity(v);
            }
        })
    };

    text_scale.connect_value_changed({
        let apply = Rc::clone(&apply_text);
        move |s| {
        let v = s.value();
        apply(v);
        let mut c = hw::load_config();
        c.hud_opacity_text = v;
        let _ = hw::save_config(&c);
        }
    });
    chrome_scale.connect_value_changed({
        let apply = Rc::clone(&apply_chrome);
        move |s| {
            let v = s.value();
            apply(v);
            let mut c = hw::load_config();
            c.hud_opacity_chrome = v;
            let _ = hw::save_config(&c);
        }
    });

    // 初始生效
    apply_text(text_op);
    apply_chrome(chrome_op);

    let root = ToolbarView::new();
    root.add_top_bar(&header);
    root.set_content(Some(&body));
    win.set_content(Some(&root));

    // ---------------------------------------------------------------- GPU 采样线程
    let gpu_alive = Arc::new(AtomicBool::new(true));
    let gpu_cache = hw::spawn_gpu_poller(Arc::clone(&gpu_alive), GPU_POLL_SECS);

    // ---------------------------------------------------------------- 定时刷新
    let running = Rc::new(Cell::new(true));
    {
        let gpu_cache = Arc::clone(&gpu_cache);
        let running = Rc::clone(&running);
        glib::timeout_add_local(Duration::from_millis(REFRESH_MS), move || {
            if !running.get() {
                return glib::ControlFlow::Break;
            }

            // 显示用平滑值（CPU 那一路做 EMA）—— 原始瞬时值跳得太厉害，见 `hw::temps_smooth`
            let temps = hw::temps_smooth();

            // CPU：温度 + 当前频率
            let cpu_t = temps
                .iter()
                .find(|(k, _)| *k == "CPU")
                .map(|(_, t)| format!("{t:.0} °C"));
            let cpu_f = hw::cpu_freq_mhz().map(|f| format!("{:.2} GHz", f / 1000.0));
            cpu.set(match (cpu_t, cpu_f) {
                (Some(t), Some(f)) => Some(format!("{t} · {f}")),
                (Some(t), None) => Some(t),
                (None, Some(f)) => Some(f),
                (None, None) => None,
            });

            // GPU：温度 + 占用 + 核心频率
            let g = gpu_cache.lock().ok().and_then(|g| g.clone());
            gpu.set(g.as_ref().map(|g| {
                let t = g
                    .temp_c
                    .map(|v| format!("{v} °C"))
                    .unwrap_or_else(|| "—".into());
                let u = g
                    .util_pct
                    .map(|v| format!("{v} %"))
                    .unwrap_or_else(|| "—".into());
                let c = g
                    .clock_sm_mhz
                    .map(|v| format!("{v} MHz"))
                    .unwrap_or_else(|| "—".into());
                format!("{t} · {u} · {c}")
            }));

            // 风扇
            let rpms = hw::fan_rpms();
            fans.set(if rpms.is_empty() {
                None
            } else {
                Some(
                    rpms.iter()
                        .map(|r| r.to_string())
                        .collect::<Vec<_>>()
                        .join(" / ")
                        + " RPM",
                )
            });

            // 主板
            board.set(
                temps
                    .iter()
                    .find(|(k, _)| *k == "主板")
                    .map(|(_, t)| format!("{t:.0} °C")),
            );

            glib::ControlFlow::Continue
        });
    }

    // 关闭：停刷新 + 通知 GPU 线程退出
    {
        let running = Rc::clone(&running);
        let alive = Arc::clone(&gpu_alive);
        win.connect_close_request(move |_| {
            running.set(false);
            alive.store(false, Ordering::Relaxed);
            glib::Propagation::Proceed
        });
    }

    win
}

/// 从一行里找出那个滑块（该行是 Box → [Label, Scale]）。
fn find_scale(row: &gtk::Box) -> Scale {
    let mut child = row.first_child();
    while let Some(w) = child {
        if let Some(s) = w.downcast_ref::<Scale>() {
            return s.clone();
        }
        child = w.next_sibling();
    }
    // 结构固定，理论上到不了这里
    Scale::with_range(Orientation::Horizontal, 0.15, 1.0, 0.05)
}
