//! 界面层（GTK4 + libadwaita 原生 Adwaita 风格）。
//!
//! 结构：`AdwApplicationWindow` → `AdwToolbarView` → `AdwPreferencesPage`
//!   - 性能档位：一键预设（省电 / 安静 / 均衡 / 性能）
//!   - 温度限制：高温时自动压 CPU 上限 / 锁 GPU 时钟（与风扇曲线同属 `omen-fand` 守护）
//!   - CPU：性能上限滑块、长时功耗墙 PL1、频率上限、睿频开关、实时频率
//!   - GPU：只读状态 + **时钟锁**（本机唯一实测有效的 GPU 降温手段）
//!   - 风扇：双风扇转速 + 风扇曲线
//!   - 温度：CPU / NVMe / 主板
//!
//! **界面上不出现无效控件**——这是本项目的一条硬规则：
//! ACPI 档位（`platform_profile`）与 GPU **功耗墙**（`nvidia-smi -pl`）在本机实测无效，
//! 因此只以只读信息的形式呈现，不给用户"点了应该有反应"的错觉。
//!
//! ⚠ 2026-10-09 更正：GPU **时钟锁**（`-lgc`）与功耗墙是**两个不同机制**，
//! 它**实测有效**（130.5 W → 57.4 W，可 `-rgc` 解锁），所以它是真控件。
//! 别因为它和"功耗墙"同属 nvidia-smi 就当成也不行 —— 早先版本正是这么误判的。
//!
//! 所有写操作都走 `hw::set_*`（免密 helper，失败才回退 pkexec），
//! 全部为内核官方接口，**不读写 EC 寄存器**。
//!
//! 线程与状态模型（这几条是踩过坑才定下来的）：
//!   1. 提权调用约 100–200 ms → 一律放后台线程，主线程只收结果；
//!   2. **每个滑块各自持有去抖定时器**。共用一个会导致"450 ms 内拖第二个滑块
//!      把第一个的操作取消掉"；
//!   3. **提权操作串行化**（`busy` 标志）。快速连点档位会让两组写入交错，
//!      最终状态不可预期；
//!   4. **用户刚动过控件时，刷新不得回写该控件**。否则拖动中的滑块会被
//!      1.2 s 一次的刷新弹回旧值；
//!   5. GPU 查询（nvidia-smi 约 60 ms）由常驻线程每 2 s 采样进缓存，
//!      主线程只读缓存；线程随窗口关闭退出。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::{
    AboutDialog, ActionRow, ApplicationWindow, ComboRow, HeaderBar, PreferencesGroup, Toast,
    ToastOverlay, ToastPriority,
    PreferencesPage, SpinRow, SwitchRow, Toggle, ToggleGroup, ToolbarView,
};
use gtk::glib;
use gtk::{Align, Button, Label};

use crate::hw;

const APP_ID: &str = "org.rosary.OmenControl";
/// 只读读数刷新间隔（毫秒）。
const REFRESH_MS: u64 = 1200;
/// GPU 后台采样间隔（秒）。
const GPU_POLL_SECS: u64 = 2;
/// 滑块去抖：停止拖动多久后才真正写入。
const DEBOUNCE_MS: u64 = 450;
/// 用户操作后多久内，刷新不回写可操作控件（避免打断拖动）。
const TOUCH_GUARD: Duration = Duration::from_secs(2);

/// 「未激活」遮罩的 CSS 类名。
const LOCK_MASK_CLASS: &str = "lock-mask";
/// 遮罩样式：用主题背景色做半透明雾化，深浅色主题都成立。
const LOCK_MASK_CSS: &str = ".lock-mask { background-color: alpha(@window_bg_color, 0.80); }";

/// 温度行的固定顺序（与 `hw::temps()` 的标签对应）。
const TEMP_NAMES: [&str; 3] = ["CPU", "NVMe", "主板"];

/// 去抖定时器槽。每个滑块一个，互不影响。
type Slot = Rc<Cell<Option<glib::SourceId>>>;

fn new_slot() -> Slot {
    Rc::new(Cell::new(None))
}

/// 造一个「锁定遮罩」：半透明铺满控件 + 居中说明文字，默认隐藏。
///
/// 用途：某个功能**还没激活**时（例如守护没跑，曲线改了也不生效），
/// 光置灰会让人以为是坏了；盖一层说明"为什么不可用、怎么启用"才讲得清。
/// 把限制策略渲染成一行副标题。
///
/// 注意 GTK 的 `set_subtitle` 走 Pango markup，文案里**不能有裸的 `<` `>` `&`** ——
/// 这里的 `→`（U+2192）是安全的。
fn limit_subtitle(p: &crate::limit::LimitPolicy, on: bool) -> String {
    if !on {
        return "关闭：守护只按曲线调风扇。打开后按下面的温度上限自动压 CPU 上限 / 锁 GPU 时钟（不碰风扇）"
            .into();
    }
    let cpu = match p.cpu_temp {
        Some(t) => match p.cpu_w {
            Some(w) => format!("CPU {t}°C 起压，最低 {}% + PL1 {w} W", p.cpu_pct),
            None => format!("CPU {t}°C 起压，最低 {}%", p.cpu_pct),
        },
        None => "CPU 未启用".to_string(),
    };
    let gpu = match p.gpu_temp {
        Some(t) => format!("GPU {t}°C 起锁，最低 {} MHz", p.gpu_mhz),
        None => "GPU 未启用".to_string(),
    };
    format!(
        "{cpu} · {gpu}\n渐进：每升 5°C 多压一档（共 4 档），回落同样分档 · 迟滞 {}°C / 连续 {} 次确认\n只压热源、不改风扇；需「自动风扇控制」的守护在跑才生效",
        p.cool_down, p.confirm
    )
}

/// 当前该显示哪条曲线的点 —— 由「编辑哪条曲线」的选择决定。
///
/// - 选 CPU ⇒ 该档的 CPU 曲线
/// - 选 GPU ⇒ 该档的 GPU 曲线；**没单独配就回落到 CPU 曲线**（与守护同一套规则），
///   这样"只想要一条曲线"的用法看到的就是那条，不会产生"我改了但生效的是另一条"的困惑。
fn current_curve_points(
    which: &ComboRow,
    preset_key: &str,
) -> Vec<crate::curve::CurvePoint> {
    let cfg = hw::load_config();
    if which.selected() == 1 {
        cfg.gpu_curve_for(preset_key).points
    } else {
        cfg.curve_for(preset_key).points
    }
}

fn build_lock_mask(markup: &str) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
    b.add_css_class(LOCK_MASK_CLASS);
    b.set_visible(false);
    let l = Label::new(None);
    l.set_markup(markup);
    l.set_justify(gtk::Justification::Center);
    l.set_valign(Align::Center);
    l.set_halign(Align::Center);
    l.set_wrap(true);
    l.add_css_class("caption-heading");
    b.append(&l);
    b
}

/// 跨闭包共享的界面句柄与状态。
struct Ui {
    /// 程序化更新控件时置位，避免 `set_*` 反触发写操作（信号回环）。
    suppress: Cell<bool>,
    /// 窗口是否仍存活；关闭后停止刷新。
    running: Cell<bool>,
    /// 正在执行一次提权操作；期间忽略新的操作请求，保证写入串行。
    busy: Cell<bool>,
    /// 最后一次用户操作控件的时间点（用于 `TOUCH_GUARD` 保护）。
    last_touch: Cell<Option<Instant>>,

    slot_cpu_pct: Slot,
    slot_rapl: Slot,
    slot_cpu_cap: Slot,

    /// GPU 状态缓存（由后台线程更新）。
    gpu: Arc<Mutex<Option<hw::Gpu>>>,

    preset_group: ToggleGroup,
    /// 风扇曲线编辑器（切档位 / 切 CPU-GPU 时用它换曲线）
    curve: Rc<crate::curve_editor::CurveEditor>,
    /// 「编辑哪条曲线」选择：0 = CPU，1 = GPU
    curve_which: ComboRow,
    /// 风扇灵敏度档位（安静/标准/强冷）
    fan_lag: ComboRow,
    /// 曲线图的「未激活」遮罩（守护没跑时显示）
    curve_mask: gtk::Box,
    /// 「自动风扇控制」总开关
    fand: SwitchRow,
    /// 「顶栏托盘」开关：同时管"现在起不起"和"登录时起不起"。
    tray_sw: SwitchRow,
    /// 托盘"该跑却跑不起来"的原因（面板启动时探一次，别放进 1.2s 刷新里）。
    tray_problem: Option<String>,
    /// CPU 限制组 —— 自动散热守护运行时整组置灰（两者不可两全）
    cpu_group: PreferencesGroup,
    cpu_pct: SpinRow,
    rapl: SpinRow,
    /// **只读**：CPU 上限的当前生效值（会被高温限流改写 —— 正因为它会跳，
    /// 才不能把滑块绑在它上面；见 `refresh` 的说明）。
    cpu_eff: Label,
    /// **只读**：PL1 的当前生效值（同上）。
    rapl_eff: Label,
    cpu_cap: SpinRow,
    cpu_freq: Label,
    turbo: SwitchRow,
    /// 内核上报的 ACPI 档位（只读信息，本机不驱动硬件）。
    acpi_profile: Label,
    gpu_limit: Label,
    gpu_temp: Label,
    gpu_draw: Label,
    gpu_clk: Label,
    gpu_util: Label,
    gpu_mem: Label,
    fan_values: Vec<Label>,
    temp_values: Vec<Label>,

    // ---- 高温限制的四个输入（刷新时按启停状态开/关）----
    lim_cpu: SpinRow,
    lim_cpu_floor: SpinRow,
    /// 高温时把 PL1（封装功耗墙）收到多少 W。不设 = 不动功耗墙。
    lim_cpu_w: SpinRow,
    lim_gpu: SpinRow,
    lim_gpu_floor: SpinRow,

    // ---- GPU 时钟锁（本机唯一实测有效的 GPU 降温手段）----
    gpu_lock: SwitchRow,
    gpu_lock_mhz: SpinRow,
    /// 时钟锁 MHz 滑块自己的去抖槽（每个滑块一个，不能共用）
    slot_gpu_lock: Slot,
}

pub fn run() {
    adw::init().ok();

    // 崩溃时尽量还原（panic 展开不会走 close_request）
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = hw::recover_if_pending();
        prev_hook(info);
    }));

    // Application 必须先绑定到变量再连信号，否则临时值会在 run() 返回后析构，
    // activate 处理器随之失效——窗口永远不会出现。
    let app = adw::Application::builder().application_id(APP_ID).build();
    // 单窗口：重复激活（点图标、浮窗的「主面板」按钮、单实例转发）都只 `present`
    // 同一个窗口。少了这一层，每点一次图标就会多开一个窗口。
    // ⚠ 崩溃恢复**必须**挂在 `startup` 上，不能放在 run() 开头。
    // `startup` 只在**成为主实例**的那个进程里发一次；而单实例机制下，
    // 后续启动的进程会立刻退出（activate 被转发给已有实例）——
    // 如果在 run() 开头无条件做恢复，那些"秒退的第二个进程"会把主实例刚写下的
    // pending 标记消费掉，导致真正需要恢复时已经没有标记了。
    app.connect_startup(|_| {
        if let Some(snap) = hw::recover_if_pending() {
            eprintln!(
                "[omen-control] 检测到上次未正常退出，已把系统还原为上次启动时的状态 \
                 (CPU {}% / 睿频{} / 频率上限 {} MHz)",
                snap.cpu_pct.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
                match snap.turbo {
                    Some(true) => "开",
                    Some(false) => "关",
                    None => "?",
                },
                snap.cpu_cap_mhz
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".into()),
            );
        }
    });

    let win: Rc<RefCell<Option<ApplicationWindow>>> = Rc::new(RefCell::new(None));
    app.connect_activate({
        let win = Rc::clone(&win);
        move |app| {
            // 注意：不能写成 `match win.borrow().as_ref() { ... borrow_mut() ... }` ——
            // match 的不可变借用在整个分支期间都活着，分支里再 borrow_mut() 会 panic
            // ("RefCell already borrowed")。所以先取出克隆，再重新借。
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
    // 必须用 run_with_args 传一个"干净的" argv。直接用 run() 会把进程的真实
    // 命令行参数交给 GApplication 解析，于是 `--hud` / `--check` 这类我们自己的
    // 参数会被当成它的未知选项而报 "未知选项 --xxx" 并退出。
    app.run_with_args::<&str>(&["omen-control"]);
}

/// 拉起监控浮窗（独立 app id，重复调用只会激活已有实例）。
///
/// 关闭主面板时选择"隐藏到后台"就必须靠它当锚点——否则窗口被隐藏后用户
/// 就再也找不回来了。
fn ensure_hud_running() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).arg("--hud").spawn();
    }
}

/// 构建主窗口（不进入主循环，便于单独测试与复用）。
pub fn build_window(app: &adw::Application) -> ApplicationWindow {
    let win = ApplicationWindow::builder()
        .application(app)
        .title("OMEN 控制面板")
        .default_width(560)
        .default_height(800)
        .build();

        // 遮罩样式：只加载一次。用 display 级入口而不是 `widget.style_context().add_provider()`
        // —— 后者自 GTK 4.10 起已废弃（与 hud.rs 保持同一写法）。
        if let Some(display) = gtk::gdk::Display::default() {
            let css = gtk::CssProvider::new();
            css.load_from_string(LOCK_MASK_CSS);
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

    // ------------------------------------------------------------ 顶栏
    let header = HeaderBar::new();

    // 「监控浮窗」：以 `--hud` 启动同一个可执行文件。浮窗是**独立 app id**，
    // 因此不会和主面板的单实例机制冲突；重复点击只会激活已开着的那个浮窗。
    let hud_btn = Button::from_icon_name("utilities-system-monitor-symbolic");
    hud_btn.set_tooltip_text(Some("打开监控浮窗（温度 / 频率 / 风扇）"));
    hud_btn.connect_clicked(|_| {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).arg("--hud").spawn();
        }
    });
    header.pack_start(&hud_btn);

    // 「恢复默认」：回到「均衡」档（= 出厂原始状态）。事件在 ui 建好后连接。
    let reset_btn = Button::from_icon_name("edit-undo-symbolic");
    reset_btn.set_tooltip_text(Some("恢复默认（均衡档：CPU 不限 · 睿频开 · 频率墙放开）"));
    header.pack_start(&reset_btn);

    // 「退出」：显式退出入口。与"关闭窗口"不同——它一定会走还原流程。
    let quit_btn = Button::from_icon_name("application-exit-symbolic");
    quit_btn.set_tooltip_text(Some("退出程序（并按设置还原系统状态）"));
    header.pack_start(&quit_btn);

    let about = AboutDialog::builder()
        .application_name("OMEN 控制面板")
        // 图标名必须与安装的图标文件名一致（hicolor 主题里的 omen-control.svg），
// 不能用 APP_ID —— 我们没有名为 org.rosary.OmenControl 的图标文件，
// 那样「关于」对话框会退化成系统默认的占位图。
        .application_icon("omen-control")
        .version(env!("CARGO_PKG_VERSION"))
        .comments(
            "通过内核官方接口控制 CPU 性能上限、睿频、频率上限、能效偏好与 CPU 功耗墙，\
             并实时监视风扇 / 温度 / GPU。\n\
             全程不读写 EC 寄存器，没有硬件损坏风险，所有设置都可逆。\n\n\
             实测结论（本机主板 8A1A）：\n\
             · ACPI 性能档对硬件无影响，故仅作只读信息展示；\n\
             · GPU 功耗墙被 vBIOS 锁定，nvidia-smi -pl 无效，故仅作只读展示；\n\
             · 真正有效的是 CPU 性能上限 / 睿频 / 频率上限 / 能效偏好。\n\n\
             设置是内核状态：面板关闭后依然生效，重启才由 BIOS 复位。\n\
             风扇曲线走 hp_wmi_omen 的 WMI 命令 GM12（写 EC 的 MMIO 寄存器，不是 ACPI EC RAM）：
               实测 20~48% 可连续给出 1992~4950 RPM，写 0 交回固件，
               由 omen-control --fan-daemon 独立进程持续应用，关掉面板也生效。",
        )
        .license_type(gtk::License::MitX11)
        // ⚠ 曾误设 `can_close(false)` —— 那是给"嵌入式 About 视图"用的（没有标题栏
        //   的场景）。这里是**独立对话框**（`present(Some(&win))`），
        //   设成 false ⇒ 不显示关闭按钮 ⇒ 用户点进「关于」后关不掉（实测确认的 bug）。
        .can_close(true)
        .build();
    let about_btn = Button::from_icon_name("help-about-symbolic");
    about_btn.set_tooltip_text(Some("关于"));
    {
        let about = about.clone();
        let win_weak = win.downgrade();
        about_btn.connect_clicked(move |_| {
            if let Some(w) = win_weak.upgrade() {
                about.present(Some(&w));
            }
        });
    }
    header.pack_end(&about_btn);

    // ------------------------------------------------------------ 页面
    let page = PreferencesPage::new();

    // ---- 风扇曲线编辑器 ----
    // 必须在 `Ui` 之前建：`Ui` 要 clone 它，档位回调也要用它刷新。
    let curve_key0 = hw::load_config()
        .preset
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "balanced".to_string());
    let curve_editor = crate::curve_editor::CurveEditor::new(
        hw::load_config().curve_for(&curve_key0).points.clone(),
    );
    let curve_group = PreferencesGroup::new();
    curve_group.set_title("风扇曲线");
    curve_group.set_description(Some(
        // 走 Pango markup ⇒ 粗体用 <b>，**Markdown 的星号会原样显示出来**（实测踩过）。
        "CPU 与 GPU <b>各有一条</b>曲线：分别按各自温度插值出目标转速，取两者中更高的那个。\
         首点左侧的区间交回 BIOS —— 那一段它给的曲线本来就平滑、安静。\
         纵轴锁在 1992~4597 RPM：以下没有使用价值，以上是饱和区（写多少都一样）。",
    ));

    // 选择正在编辑的是 CPU 还是 GPU 曲线。
    // 不单独配 GPU 曲线时它**跟随 CPU 曲线**（守护回落处理），所以这里也给这个选项。
    let curve_which = ComboRow::new();
    curve_which.set_title("编辑哪条曲线");
    curve_which.set_subtitle("两条都按各自的温度判；最终取转速更高的那个");
    let which_model = gtk::StringList::new(&["CPU", "GPU（跟随 CPU）"]);
    curve_which.set_model(Some(&which_model));
    curve_group.add(&curve_which);

    // ---- 风扇迟滞（三档）----
    //
    // 只改"温度变化要稳定多久才被采纳"，**不改动曲线本身** ——
    // 用户编辑好的曲线形状原样保留。
    let fan_lag = ComboRow::new();
    fan_lag.set_title("风扇迟滞");
    let g0 = hw::FanLag::default();
    fan_lag.set_subtitle(&g0.hint());
    // 显示名从 `FanLag::label()` 取，别在这里再写一份中文 ——
    // 两处字符串不一致时用户会看到"配置里的名字"和"界面上的名字"对不上。
    let lag_labels: Vec<&str> = [hw::FanLag::Long, hw::FanLag::Mid, hw::FanLag::Short]
        .iter()
        .map(|g| g.label())
        .collect();
    let lag_model = gtk::StringList::new(&lag_labels);
    fan_lag.set_model(Some(&lag_model));
    fan_lag.set_selected(g0 as u32); // Long=0, Mid=1, Short=2
    curve_group.add(&fan_lag);

    // 曲线图外面套一层 Overlay：守护没跑时盖遮罩 + 置灰。
    // 只置灰不给理由，用户会以为界面坏了 —— 遮罩上写明"为什么"和"怎么启用"。
    let curve_overlay = gtk::Overlay::new();
    curve_overlay.set_child(Some(&curve_editor.widget));
    let curve_mask = build_lock_mask(
        "风扇曲线<b>当前不生效</b>\n请先开启下方的「自动风扇控制」",
    );
    curve_overlay.add_overlay(&curve_mask);
    curve_group.add(&curve_overlay);

    // 「自动风扇控制」—— 曲线是否生效的总闸。
    let fand = SwitchRow::new();
    fand.set_title("自动风扇控制");
    fand.set_subtitle(
        "常驻守护按温度持续应用上面的曲线；由 systemd 服务 omen-fand 承载，\
         开机自启、崩溃自动重启。关掉它曲线就不再生效（风扇交回 BIOS）。",
    );
    curve_group.add(&fand);

    // ---- 高温限制 ----
    //
    // 与风扇**彻底分离**：这里只写 CPU 上限与 GPU 时钟（经 helper，需提权），
    // 风扇的唯一来源仍是上面的曲线。两者由同一个守护 `omen-fand` 驱动。
    let limit_group = PreferencesGroup::new();
    limit_group.set_title("高温限制");
    limit_group.set_description(Some(
        "达到温度上限后，温度每再升 5°C 多压一档（共 4 档）直到「最低值」，\
         回落时按同一套档位反向恢复 —— 所以不会有突变。\
         它不碰风扇（风扇只由上面的曲线决定）。\
         开启后守护只会在你手设的值<b>之上收紧</b>，不会放宽、也不会覆盖 ——\
         比如上面设了 30 W，守护只会调到 30 或更低，不会给你调回 135。",
    ));

    let lim_cfg = hw::load_config();
    let limit_row = SwitchRow::new();
    limit_row.set_title("高温时自动压 CPU/GPU");
    limit_row.set_subtitle(&limit_subtitle(&lim_cfg.limit, lim_cfg.limit.enabled));
    limit_row.set_active(lim_cfg.limit.enabled);
    limit_group.add(&limit_row);
    {
        let row = limit_row.clone();
        row.connect_active_notify(move |r| {
            let on = r.is_active();
            let mut c = hw::load_config();
            // 首次打开、且还没配过任何阈值 ⇒ 套用推荐值（否则开关开了却什么都不做）
            if on && c.limit.cpu_temp.is_none() && c.limit.gpu_temp.is_none() {
                let rec = crate::limit::LimitPolicy::recommended();
                c.limit.cpu_temp = rec.cpu_temp;
                c.limit.cpu_pct = rec.cpu_pct;
                c.limit.gpu_temp = rec.gpu_temp;
                c.limit.gpu_mhz = rec.gpu_mhz;
            }
            let v = if on { "1" } else { "0" };
            c.limit.set_key("enabled", v);
            let _ = hw::save_config(&c);
            // 守护必须重载配置才生效 —— 只改文件它不会知道
            if hw::fan_daemon_running() {
                match hw::restart_fan_daemon() {
                    Ok(true) => println!("[ui] 限制开关 {v} → 已重启守护载入新配置"),
                    Ok(false) => {
                        eprintln!("[ui] 限制已保存，但重启守护未获授权（需先装免密 helper）")
                    }
                    Err(e) => eprintln!("[ui] 重启守护失败：{e}"),
                }
            }
            r.set_subtitle(&limit_subtitle(&c.limit, on));
        });
    }

    // 温度的范围与步幅按用户指定：CPU 50~100、GPU 50~90、步幅 5。
    // 常量全部取自 `limit.rs`，界面不另写一套数字。
    let cpu_t = lim_cfg.limit.cpu_temp.unwrap_or(80);
    let lim_cpu = spin_row(
        "CPU 温度上限",
        &format!(
            "达到就开始压；压到 {}°C 时到最低值。设成 {}°C 基本等于不限制",
            cpu_t + crate::limit::SPAN,
            crate::limit::CPU_TEMP_MAX
        ),
        crate::limit::CPU_TEMP_MIN as f64,
        crate::limit::CPU_TEMP_MAX as f64,
        cpu_t as f64,
        crate::limit::TEMP_STEP as f64,
    );
    lim_cpu.set_sensitive(lim_cfg.limit.enabled);
    limit_group.add(&lim_cpu);

    let lim_cpu_floor = spin_row(
        "CPU 最低性能",
        &format!(
            "最多压到这个上限；温度到 {}°C 时就是它（当前 {}%）",
            cpu_t + crate::limit::SPAN,
            lim_cfg.limit.cpu_pct
        ),
        30.0,
        100.0,
        lim_cfg.limit.cpu_pct as f64,
        5.0,
    );
    lim_cpu_floor.set_sensitive(lim_cfg.limit.enabled);
    limit_group.add(&lim_cpu_floor);

    // PL1（封装功耗墙）—— **从源头压热源**，是本机最有效的降温手段。
    // i9-12900HX 的睿频能在 2 秒内把某核从 60°C 推到 96°C，而风扇软启动要 ~26 秒，
    // 纯靠调转速必然反复冲顶。功耗墙是硬上限，一限住温度就上不去。
    let lim_cpu_w = spin_row(
        "CPU 功耗墙（PL1）",
        &format!(
            "达到 {}°C 时把封装功耗收到这个值 —— 降温最直接（Windows 侧常用 30~40 W）。\n\
             当前 {} W；不动就设成启动时的值",
            lim_cfg.limit.cpu_temp.unwrap_or(80),
            lim_cfg.limit.cpu_w.unwrap_or(hw::rapl_w().unwrap_or(135))
        ),
        crate::limit::CPU_W_MIN as f64,
        crate::limit::CPU_W_MAX as f64,
        lim_cfg.limit.cpu_w.unwrap_or(hw::rapl_w().unwrap_or(135)) as f64,
        5.0,
    );
    lim_cpu_w.set_sensitive(lim_cfg.limit.enabled);
    limit_group.add(&lim_cpu_w);

    let gpu_t = lim_cfg.limit.gpu_temp.unwrap_or(75);
    let lim_gpu = spin_row(
        "GPU 温度上限",
        &format!(
            "达到就开始锁 GPU 的 SM 时钟；压到 {}°C 时到最低值",
            gpu_t + crate::limit::SPAN
        ),
        crate::limit::GPU_TEMP_MIN as f64,
        crate::limit::GPU_TEMP_MAX as f64,
        gpu_t as f64,
        crate::limit::TEMP_STEP as f64,
    );
    lim_gpu.set_sensitive(lim_cfg.limit.enabled);
    limit_group.add(&lim_gpu);

    let lim_gpu_floor = spin_row(
        "GPU 最低频率",
        &format!(
            "最狠锁到这个时钟。实测 1200 MHz 时 GPU 功耗从 130 W 降到约 57 W（当前 {} MHz）",
            lim_cfg.limit.gpu_mhz
        ),
        500.0,
        2100.0,
        lim_cfg.limit.gpu_mhz as f64,
        50.0,
    );
    lim_gpu_floor.set_sensitive(lim_cfg.limit.enabled);
    limit_group.add(&lim_gpu_floor);

    // ---- 档位预设 ----
    let preset_group = PreferencesGroup::new();
    preset_group.set_title("性能档位");
    preset_group
        .set_description(Some("一键同时设置 CPU 上限 / 睿频 / 能效偏好 —— 效果立刻可测量"));
    let toggle = ToggleGroup::new();
    toggle.set_homogeneous(true);
    toggle.set_css_classes(&["round"]);
    for p in hw::PRESETS.iter() {
        let t = Toggle::new();
        t.set_label(Some(p.label));
        t.set_name(Some(p.key));
        toggle.add(t);
    }
    // 悬停显示各档实际参数（由 PRESETS 单一数据源生成，避免文案与实现不一致）
    toggle.set_tooltip_text(Some(
        &hw::PRESETS
            .iter()
            .map(|p| format!("{}：{}", p.label, p.hint))
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    preset_group.add(&toggle);
    // 只读信息行：内核上报的 ACPI 档位。本机切换它对硬件无任何影响（实测），
    // 所以不做成控件——避免出现"点了却没反应"的假控件。
    let (acpi_row, acpi_profile) = value_row("内核 ACPI 档位");
    acpi_row.set_subtitle(
        "只读。主板 8A1A 未被内核识别，切换该档位对本机硬件无影响，故不提供控件",
    );
    preset_group.add(&acpi_row);
    page.add(&preset_group);
    page.add(&curve_group);
    page.add(&limit_group);

    // ---- 启动与持久化 ----
    let boot_group = PreferencesGroup::new();
    boot_group.set_title("启动与还原");
    boot_group.set_description(Some(
        "设置写入的是内核状态。默认行为：程序退出时把系统还原成启动时的样子，\
         即设置只在程序存活期间生效",
    ));

    let cfg = hw::load_config();
    // 启动时要套用的档位（先取出来，后面 cfg 还会被开关读用）
    let boot_preset = cfg.preset.clone();
    // 默认开启"退出时还原"（用户明确要求"关掉就清零"）；老配置没有这个键，
    // 首次读到 false 会让行为突变，所以按"键缺失 = 默认开"处理。
    let restore_default = cfg.restore_on_exit || !hw::config_has_key("restore_on_exit");

    let restore_on_exit = SwitchRow::new();
    restore_on_exit.set_title("退出时还原为启动时的状态");
    restore_on_exit.set_subtitle(
        "关掉面板 / 点「退出」时自动写回启动时的参数。\
         若被强杀或崩溃，下次启动会自动检测并还原",
    );
    restore_on_exit.set_active(restore_default);
    boot_group.add(&restore_on_exit);

    let hide_on_close = SwitchRow::new();
    hide_on_close.set_title("关闭主面板时隐藏到后台");
    hide_on_close
        .set_subtitle("程序继续在后台运行（会确保浮窗已启动，否则窗口会找不到）");
    hide_on_close.set_active(cfg.hide_on_close);
    boot_group.add(&hide_on_close);

    let auto_apply = SwitchRow::new();
    auto_apply.set_title("登录时自动套用所选档位");
    auto_apply.set_subtitle("把当前档位记到配置文件，下次登录自动重新应用");
    auto_apply.set_active(cfg.auto_apply);
    boot_group.add(&auto_apply);

    let hud_switch = SwitchRow::new();
    hud_switch.set_title("登录时显示监控浮窗");
    hud_switch.set_subtitle("一个小窗口实时显示 CPU/GPU 温度与频率、风扇转速（透明度在浮窗内调整）");
    hud_switch.set_active(cfg.hud);
    boot_group.add(&hud_switch);

    // 「顶栏托盘」：用户要求（2026-10-11）—— 托盘过去会"玄学失灵"，
    // 现在把它做成一个**看得到事实、点得动**的开关：开关 = "托盘该不该跑"，
    // 副标题如实报告进程在不在、跑不起来是什么原因。
    let tray_problem = hw::tray_problem();
    let tray_switch = SwitchRow::new();
    tray_switch.set_title("顶栏托盘");
    tray_switch.set_subtitle(&tray_subtitle(tray_problem.as_deref()));
    tray_switch.set_active(hw::tray_autostart_enabled());
    boot_group.add(&tray_switch);
    page.add(&boot_group);

    // ---- CPU ----
    let cpu_group = PreferencesGroup::new();
    cpu_group.set_title("CPU");
    // ⚠ 2026-10-11 重写这段文案与交互（用户实测反馈）：
    //   原来的说法是「守护在设定之上收紧，两者协同、不会互相覆盖」—— 对硬件成立，
    //   对**界面**不成立：滑块直接跟随 sysfs，于是守护一收紧，滑块就跟着跳到 62%，
    //   温度一回落又跳回 100%，用户既读不出"我设的是多少"，也改不动。
    //   ⇒ 现在明确分成两层：**上面的滑块 = 我设定的基准（不会跳）**，
    //     **「当前生效」= 守护此刻写进硬件的值（只读，会跳是正常的）**。
    // 两行以内。详情在 README，别把长篇解释塞进界面。
    cpu_group.set_description(Some(
        "100% = 不限制。滑块是<b>你的设定值</b>（不会被高温限流改写）；\
         守护此刻真正写进硬件的值见「当前生效」—— 那里会跳是正常的。",
    ));

    // 下限与 helper 的白名单（20..100）保持一致，避免滑块能拖到会被拒绝的值
    let cpu_min = hw::cpu_min_pct().unwrap_or(20).max(20);
    // 初值优先级：用户设定的基准 → 当前档位声明的上限 → 硬件当前值。
    // ⚠ 中间那一层不能省：若此刻守护正在限流，sysfs 读到的 62% 是**守护的值**，
    //   不是用户的设定（用户选的「性能」档意味着一开始就该显示 100%）。
    let pct_init = cfg
        .cpu_pct
        .or_else(|| cfg.preset.as_deref().and_then(hw::preset).map(|p| p.cpu_pct))
        .or_else(hw::cpu_max_pct)
        .unwrap_or(100);
    let cpu_pct = spin_row(
        "性能上限",
        "拉低可显著降温降噪，代价是变慢（实测 20% 时负载温度低约 23°C）",
        cpu_min as f64,
        100.0,
        pct_init as f64,
        5.0,
    );
    cpu_group.add(&cpu_pct);

    let (cpu_eff_row, cpu_eff) = value_row("当前生效上限");
    cpu_eff_row.set_subtitle("守护此刻真正写进硬件的值；高温限流生效时它会低于上面的设定值");
    cpu_group.add(&cpu_eff_row);

    // CPU PL1。
    //
    // ⚠⚠ 原实现把 `constraint_0_max_power_uw` 当成"内核可写上限"，并据此把控件
    //     **永久禁用**（本机它读 55 W）。但那个判断是错的：
    //     同一时刻 `power_limit_uw` 实际是 **135 W** —— 如果 55 真是钳位上限，
    //     135 根本不可能存在。intel-rapl 写入并不 clamp，那个节点只是 BIOS 报的能力值。
    //     ⇒ 用户实测「PL1 锁死 55W 且不可调」就是被这个误判禁掉的。
    //
    // 改成：范围给一个宽窗口，**写入后由 helper 读回校验** ——
    // 若真被内核/固件钳位，会明确报出来，而不是把控件灰掉让用户猜。
    // ⚠ 初值同样优先取**用户设定的基准**：守护限流时 sysfs 里是它的 30 W，
    //   不是用户的设定（用户想钉的可能是 45 W）。
    let rapl_cur = cfg.cpu_pl1.or_else(hw::rapl_w).unwrap_or(135);
    let rapl_top = 200u32;
    // ⚠ 2026-10-11 实测修正：以前这里写着"thermald 与 ppd 可能把 PL1 改回去"——
    //   那是**推测，已被证伪**。90 W 在 8 线程负载、pkg 93°C 下持续 50 秒纹丝不动；
    //   thermald 的 rapl_controller 本机内核根本没注册（cooling_device 里不存在），
    //   ppd 二进制里没有 RAPL 相关符号。⇒ 写下去就是你的，不会再提心吊胆。
    // ⚠ 文案要短。这里原先写了 5 行（把页面顶出去，用户实测反馈），
    //   细节挪进注释/README：实测写下去不会被 thermald/ppd 改回；
    //   设成 45 W 之类的固定墙能压住 100°C 尖峰（因为它是硬瓦数墙，不依赖反应速度）。
    let rapl_sub = format!("设定 {rapl_cur} W（5~200）· 只有你能改它，高温限流不会动");
    let rapl = spin_row(
        "长时功耗墙 PL1（设定值）",
        &rapl_sub,
        5.0,
        rapl_top as f64,
        rapl_cur.min(rapl_top) as f64,
        5.0,
    );
    rapl.set_sensitive(true);
    cpu_group.add(&rapl);

    let (rapl_eff_row, rapl_eff) = value_row("当前生效功耗墙");
    rapl_eff_row.set_subtitle("守护高温时实际写进硬件的值；它会跳，是限流在动作，与上面的设定无关");
    cpu_group.add(&rapl_eff_row);

    let hw_max_mhz = hw::cpu_hw_max_mhz().unwrap_or(4900.0).round() as u32;
    let cap_cur = hw::cpu_cap_mhz().unwrap_or(hw_max_mhz as f64).round() as u32;
    let cpu_cap = spin_row(
        "频率上限",
        // 必须短（用户实测：长副标题会把页面顶出去）。
        // ⚠ 别用 `**加粗**` —— GTK 渲染的是 Pango 标记，`**` 会**原样显示**成星号。
        &format!(
            "硬件最高 {hw_max_mhz} MHz · 写「性能上限」时会被重算抹掉，长期限频用上面那项"
        ),
        400.0,
        hw_max_mhz as f64,
        cap_cur as f64,
        100.0,
    );
    cpu_group.add(&cpu_cap);

    let turbo = SwitchRow::new();
    turbo.set_title("睿频加速");
    turbo.set_subtitle("关闭后 CPU 不超基础频率，降温最明显");
    turbo.set_active(hw::turbo_on().unwrap_or(true));
    cpu_group.add(&turbo);

    let (freq_row, cpu_freq) = value_row("实时频率 / 上限");
    cpu_group.add(&freq_row);
    page.add(&cpu_group);

    // ---- GPU ----
    let gpu_group = PreferencesGroup::new();
    gpu_group.set_title("GPU");

    let gpu_now = hw::gpu();
    if let Some(g) = &gpu_now {
        let def = g
            .power_default_w
            .map(|v| format!("{v}W"))
            .unwrap_or_else(|| "—".into());
        let mx = g
            .power_max_w
            .map(|v| format!("{v}W"))
            .unwrap_or_else(|| "—".into());
        gpu_group.set_description(Some(&format!(
            "{} · 出厂 {def} / 硬件上限 {mx}。功耗墙由平台 vBIOS 锁定，\
             nvidia-smi -pl 实测为空操作（只读）；但下面的时钟锁是另一个机制，实测有效",
            g.name
        )));
    } else {
        gpu_group.set_description(Some("未检测到 NVIDIA GPU"));
    }

    let (r_limit, gpu_limit) = value_row("当前功耗墙");
    let (r_temp, gpu_temp) = value_row("温度");
    let (r_draw, gpu_draw) = value_row("实时功耗");
    let (r_clk, gpu_clk) = value_row("核心 / 显存频率");
    let (r_util, gpu_util) = value_row("GPU / 显存利用率");
    let (r_mem, gpu_mem) = value_row("显存占用");
    for r in [&r_limit, &r_temp, &r_draw, &r_clk, &r_util, &r_mem] {
        gpu_group.add(r);
    }

    // ---- GPU 时钟锁：本机唯一实测有效的 GPU 降温手段 ----
    // `-lgc`（锁 SM 时钟）与 `-pl`（功耗墙，空操作）是**两个不同机制**，别混。
    // 早先版本因为 `-pl` 不通就把整条 GPU 写路径关了 —— 那是过度保守。
    let lock_cur = gpu_now.as_ref().and_then(|g| g.clk_lock);
    let lock_max = lock_cur.map(|(_, mx)| mx).unwrap_or(1500);

    let gpu_lock = SwitchRow::new();
    gpu_lock.set_title("锁定核心频率");
    gpu_lock.set_subtitle(
        "限制 GPU 核心（SM）时钟上限 —— 实测功耗 130.5 W → 57.4 W，降温立竿见影，\
         解锁即恢复。注意：无效的是功耗墙（-pl），不是这个",
    );
    gpu_lock.set_active(lock_cur.is_some());
    gpu_lock.set_sensitive(gpu_now.is_some());
    gpu_group.add(&gpu_lock);

    let gpu_lock_mhz = spin_row(
        "锁定频率上限",
        "越低越凉：1500 MHz 约 60 W；1200 MHz 更凉但游戏帧数会降",
        500.0,
        2100.0,
        lock_max as f64,
        50.0,
    );
    // ⚠ 2026-10-11 修正：原先只在你**已经锁上**时才让这个值可调
    //   （`set_sensitive(lock_cur.is_some())`），结果用户想"先选个频率再开锁"时
    //   它一直是灰的 —— 那正是"GPU 不可调"的另一半原因。
    //   ⇒ 只要检测到 GPU 就始终可编辑。
    gpu_lock_mhz.set_sensitive(gpu_now.is_some());
    gpu_group.add(&gpu_lock_mhz);

    page.add(&gpu_group);

    // ---- 风扇 ----
    let fan_group = PreferencesGroup::new();
    fan_group.set_title("风扇");
    fan_group.set_description(Some(
        "转速来自 hp-wmi；副风扇低负载停转属正常。\
         风扇曲线由 --fan-daemon 按 CPU/GPU 温度持续应用；
           温度低于曲线首点时交回 BIOS（那一段它本来就平滑、安静）。\
         曲线直接改配置文件：curve.&lt;档位&gt;=温度:转速,…",
    ));
    let mut fan_values = Vec::new();
    for i in 1..=2 {
        let (row, val) = value_row(&format!("风扇 {i}"));
        fan_group.add(&row);
        fan_values.push(val);
    }
    page.add(&fan_group);

    // ---- 温度 ----
    let temp_group = PreferencesGroup::new();
    temp_group.set_title("温度");
    let mut temp_values = Vec::new();
    for name in TEMP_NAMES {
        let (row, val) = value_row(name);
        temp_group.add(&row);
        temp_values.push(val);
    }
    page.add(&temp_group);

    // ------------------------------------------------------------ 组装
    let root = ToolbarView::new();
    root.add_top_bar(&header);
    root.set_content(Some(&page));

    // 界面提示统一走 Toast（自动消失、非模态）—— 见 `alert()` 的注释：
    // 之前用的 AlertDialog 没加响应按钮，弹出来就关不掉。
    let toasts = ToastOverlay::new();
    toasts.set_child(Some(&root));
    win.set_content(Some(&toasts));

    // ------------------------------------------------------------ 后台 GPU 采样
    // nvidia-smi 一次约 60 ms，绝不能放主线程；线程随窗口关闭退出。
    let gpu_alive = Arc::new(AtomicBool::new(true));
    let gpu_cache = hw::spawn_gpu_poller(Arc::clone(&gpu_alive), GPU_POLL_SECS);

    let ui = Rc::new(Ui {
        suppress: Cell::new(false),
        running: Cell::new(true),
        busy: Cell::new(false),
        last_touch: Cell::new(None),
        slot_cpu_pct: new_slot(),
        slot_rapl: new_slot(),
        slot_cpu_cap: new_slot(),
        gpu: gpu_cache,
        preset_group: toggle.clone(),
        cpu_pct: cpu_pct.clone(),
        rapl: rapl.clone(),
        cpu_eff,
        rapl_eff,
        cpu_cap: cpu_cap.clone(),
        cpu_freq,
        turbo: turbo.clone(),
        acpi_profile,
        gpu_limit,
        gpu_temp,
        gpu_draw,
        gpu_clk,
        gpu_util,
        gpu_mem,
        fan_values,
        temp_values,
        lim_cpu: lim_cpu.clone(),
        lim_cpu_floor: lim_cpu_floor.clone(),
        lim_cpu_w: lim_cpu_w.clone(),
        lim_gpu: lim_gpu.clone(),
        lim_gpu_floor: lim_gpu_floor.clone(),
        gpu_lock: gpu_lock.clone(),
        gpu_lock_mhz: gpu_lock_mhz.clone(),
        slot_gpu_lock: new_slot(),
        curve: curve_editor.clone(),
        curve_which: curve_which.clone(),
        fan_lag: fan_lag.clone(),
        curve_mask: curve_mask.clone(),
        fand: fand.clone(),
        tray_sw: tray_switch.clone(),
        tray_problem,
        cpu_group: cpu_group.clone(),
    });
    let ui = ui.clone();

// 「自动风扇控制」开关 → 启停 systemd 服务 omen-fand
{
    let (weak, win_weak) = (Rc::downgrade(&ui), win.downgrade());
    fand.connect_active_notify(move |row| {
        let Some(ui) = weak.upgrade() else { return };
        if ui.suppress.get() {
            return;
        }
        record_touch(&ui);
        let want = row.is_active();
        if hw::fan_daemon_running() == want {
            return;
        }
        let (weak2, win_weak2) = (weak.clone(), win_weak.clone());
        start_op(
            &ui,
            move || match hw::set_fan_daemon(want) {
                Ok(true) => Ok(()),
                Ok(false) => Err("未获授权".into()),
                Err(e) => Err(e.to_string()),
            },
            move |res| report(&weak2, &win_weak2, res, "风扇守护启停失败"),
        );
    });
}


// 曲线被拖动/增删后写回配置 —— 按**当前档位**存（配置文件里的 preset）。
// 现读配置而不是把档位缓存进闭包：切档位就不需要重绑回调。
// ⚠ 捕获的是 `curve_which`（一个 ComboRow 的克隆），**不是 `ui`** ——
//   一旦这里捕获 `Rc<Ui>`，整个 `ui` 就被 move 进闭包，后面十几个回调全都用不了。
//   只捕获需要的最小对象，是这类长构建函数的通用做法。
let which_for_cb = curve_which.clone();
curve_editor.set_on_changed(move |pts| {
    let mut c = hw::load_config();
    let key = c
        .preset
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "balanced".to_string());
    let mut f = crate::curve::FanCurve { points: pts.to_vec() };
    f.normalize();
    // 按用户当前选择的那条写：0 = CPU，1 = GPU（空曲线 ⇒ 删掉、回落到 CPU）
    if which_for_cb.selected() == 1 {
        c.set_gpu_curve(&key, f);
    } else {
        c.set_curve(&key, f);
    }
    let _ = hw::save_config(&c);
    // ⚠⚠ 必须让守护重载曲线。
    // 守护在**启动时**把曲线读进内存；只改配置文件它不会知道 ——
    // 用户实测「拖完曲线风扇不动」就是漏了这一步（曲线改了但不生效）。
    if hw::fan_daemon_running() && hw::restart_fan_daemon().is_err() {
        eprintln!("[ui] 曲线已保存，但重启守护失败 —— 需要先装免密 helper");
    }
});

    // ---- 交互：「编辑哪条曲线」切换 ----
    // 注意：CurveEditor::set_points 只改内部状态并重绘，**不会**触发 on_changed，
    // 所以这里切换不会因为"载入新曲线"而把 A 曲线误写进 B。
    {
        let weak = Rc::downgrade(&ui);
        curve_which.connect_selected_notify(move |w| {
            let Some(ui) = weak.upgrade() else { return };
            let key = hw::load_config()
                .preset
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "balanced".to_string());
            ui.curve.set_points(current_curve_points(w, &key));
            // 副标题说明这一条目前是"独立"还是"跟随"，比改选项文字安全
            // （改 StringList 会重置 selected，导致切到 GPU 又跳回 CPU）。
            let own = hw::load_config().has_own_gpu_curve(&key);
            w.set_subtitle(if w.selected() == 1 {
                if own {
                    "GPU 独立曲线 —— 按 GPU 温度判"
                } else {
                    "还没单独配置，当前跟随 CPU 曲线；一动它就会创建独立曲线"
                }
            } else {
                "CPU 曲线 —— 按 CPU 温度判"
            });
        });
    }

    // ---- 交互：风扇迟滞 ----
    //
    // ⚠ 这一段**必须**放在 `ui` 构造之后 ——
    //   `fan_lag` 是在曲线组里造的控件，但回调需要读 `ui.suppress`。
    //   若在造控件时就 `Rc::downgrade(&ui)`，会因为 `ui` 还没定义而编译失败。
    {
        let weak = Rc::downgrade(&ui);
        fan_lag.connect_selected_notify(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.suppress.get() {
                return; // refresh() 正在回写控件值，别把它当成用户操作
            }
            let g = match row.selected() {
                0 => hw::FanLag::Long,
                2 => hw::FanLag::Short,
                _ => hw::FanLag::Mid,
            };
            let mut c = hw::load_config();
            if c.fan_lag == g {
                return;
            }
            c.fan_lag = g;
            let _ = hw::save_config(&c);
            row.set_subtitle(&g.hint());
            // 守护把时间常数读在内存里 ⇒ 必须重启它才会用上新参数
            if hw::fan_daemon_running() && hw::restart_fan_daemon().is_err() {
                eprintln!("[ui] 迟滞已保存，但重启守护失败");
            }
        });
    }

    // ------------------------------------------------------------ 交互：档位
    {
        let weak = Rc::downgrade(&ui);
        let win_weak = win.downgrade();
        toggle.connect_active_name_notify(move |g| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.suppress.get() {
                return;
            }
            record_touch(&ui);
            let Some(key) = g.active_name() else { return };
            // 曲线编辑器跟着切到这一档的曲线（CPU 还是 GPU 取决于上面的选择）
            ui.curve
                .set_points(current_curve_points(&ui.curve_which, &key));
            let Some(preset) = hw::preset(&key) else { return };
            // 记住用户的选择（供「登录时自动套用」使用）。
            // 即使当前状态已经匹配也要记——用户是在"声明想要哪一档"。
            {
                let mut cfg = hw::load_config();
                let mut dirty = false;
                if cfg.preset.as_deref() != Some(preset.key) {
                    cfg.preset = Some(preset.key.to_string());
                    dirty = true;
                }
                // 档位本身就声明了它的"设定上限"（省电 40 / 均衡 100…），一并落盘。
                // ⚠ 必须在这里同步：滑块现在只跟 config 走、不再跟 sysfs 走，
                //   漏掉这一句，切档位后上面的滑块会停在上一档的值上。
                if cfg.cpu_pct != Some(preset.cpu_pct) {
                    cfg.cpu_pct = Some(preset.cpu_pct);
                    dirty = true;
                }
                if dirty {
                    let _ = hw::save_config(&cfg);
                }
            }
            if hw::cur_state().matches(preset) {
                return; // 已经是这个状态，不重复提权
            }
            let (weak2, win_weak2) = (weak.clone(), win_weak.clone());
            start_op(
                &ui,
                move || {
                    let (_ok, errs) = hw::apply_preset(preset);
                    if errs.is_empty() {
                        Ok(())
                    } else {
                        Err(errs.join("\n"))
                    }
                },
                move |res| report(&weak2, &win_weak2, res, "部分设置未生效"),
            );
        });
    }

    // ------------------------------------------------------------ 交互：三个滑块
    connect_slider(
        &ui,
        &cpu_pct,
        Rc::clone(&ui.slot_cpu_pct),
        {
            let (weak, win_weak) = (Rc::downgrade(&ui), win.downgrade());
            move |v: u32| {
                let (weak, win_weak) = (weak.clone(), win_weak.clone());
                move || {
                    // 先落盘**用户设定值**。刻意放在早退之前：即便硬件此刻恰好等于 v
                    // （例如守护也正好限到 v），"我设的是 v"这件事也必须被记住 ——
                    // 否则温度一回落，sysfs 被守护还成基准值，滑块又会跟着跳回去。
                    {
                        let mut c = hw::load_config();
                        if c.cpu_pct != Some(v) {
                            c.cpu_pct = Some(v);
                            let _ = hw::save_config(&c);
                        }
                    }
                    if hw::cpu_max_pct() == Some(v) {
                        return;
                    }
                    let (weak, win_weak) = (weak.clone(), win_weak.clone());
                    let Some(ui) = weak.upgrade() else { return };
                    start_op(
                        &ui,
                        move || match hw::set_cpu_max_pct(v) {
                            Ok(true) => Ok(()),
                            Ok(false) => Err("未获授权".into()),
                            Err(e) => Err(e.to_string()),
                        },
                        move |res| report(&weak, &win_weak, res, "CPU 上限设置失败"),
                    );
                }
            }
        },
    );

    connect_slider(
        &ui,
        &rapl,
        Rc::clone(&ui.slot_rapl),
        {
            let (weak, win_weak) = (Rc::downgrade(&ui), win.downgrade());
            move |v: u32| {
                let (weak, win_weak) = (weak.clone(), win_weak.clone());
                move || {
                    // ⚠⚠ 2026-10-11 修掉的真凶：这里原有一条
                    //     `cur <= rapl_max_w()` 的守卫。本机 `constraint_0_max_power_uw`
                    //     读 55 W 而实际值是 135 W ⇒ `cur <= max` 恒为 false ⇒
                    //     **每一次 PL1 调整都在这里被静默丢弃**，界面 1.2s 刷新再把真实值
                    //     写回滑块 ⇒ 用户看到的是"调完就跳回 135 W，怎么样都不行"。
                    //     那个节点只是 BIOS 报的能力值，不是钳位上限 —— 同文件控件构造处
                    //     已经把它判定为误判并解除了禁用，这里却漏删守卫，于是成了
                    //     "看着能调、其实空操作"（正是本项目铁律 1 要消灭的缺陷）。
                    //     写入是否真生效，交给 helper 的「写后读回校验」回答。
                    //
                    // 同 cpu_pct：先落盘用户设定值（早退之前），它是不会跳的那一层。
                    {
                        let mut c = hw::load_config();
                        if c.cpu_pl1 != Some(v) {
                            c.cpu_pl1 = Some(v);
                            let _ = hw::save_config(&c);
                        }
                    }
                    if hw::rapl_w() == Some(v) {
                        return;
                    }
                    let (weak, win_weak) = (weak.clone(), win_weak.clone());
                    let Some(ui) = weak.upgrade() else { return };
                    start_op(
                        &ui,
                        move || match hw::set_rapl_w(v) {
                            Ok(true) => Ok(()),
                            Ok(false) => Err("未获授权".into()),
                            Err(e) => Err(e.to_string()),
                        },
                        move |res| report(&weak, &win_weak, res, "PL1 设置失败"),
                    );
                }
            }
        },
    );

    connect_slider(
        &ui,
        &cpu_cap,
        Rc::clone(&ui.slot_cpu_cap),
        {
            let (weak, win_weak) = (Rc::downgrade(&ui), win.downgrade());
            move |v: u32| {
                let (weak, win_weak) = (weak.clone(), win_weak.clone());
                move || {
                    if hw::cpu_cap_mhz().map(|c| c.round() as u32) == Some(v) {
                        return;
                    }
                    let (weak, win_weak) = (weak.clone(), win_weak.clone());
                    let Some(ui) = weak.upgrade() else { return };
                    start_op(
                        &ui,
                        move || match hw::set_cpu_cap_mhz(v) {
                            Ok(true) => Ok(()),
                            Ok(false) => Err("未获授权".into()),
                            Err(e) => Err(e.to_string()),
                        },
                        move |res| report(&weak, &win_weak, res, "频率上限设置失败"),
                    );
                }
            }
        },
    );

    // ------------------------------------------------------------ 交互：两个开关
    {
        let (weak, win_weak) = (Rc::downgrade(&ui), win.downgrade());
        turbo.connect_active_notify(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.suppress.get() {
                return;
            }
            record_touch(&ui);
            let on = row.is_active();
            if hw::turbo_on() == Some(on) {
                return;
            }
            let (weak2, win_weak2) = (weak.clone(), win_weak.clone());
            start_op(
                &ui,
                move || match hw::set_turbo(on) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err("未获授权".into()),
                    Err(e) => Err(e.to_string()),
                },
                move |res| report(&weak2, &win_weak2, res, "睿频切换失败"),
            );
        });
    }


    // ------------------------------------------------------------ 交互：限制温度输入
    //
    // 温度只写**配置文件**（不写硬件）—— 真正的应用由守护完成。
    // 所以改完必须让守护重载（`systemctl start` 对运行中的服务是空操作，得 restart）。
    for (row, key) in [
        (&lim_cpu, "cpu_temp"),
        (&lim_cpu_floor, "cpu_pct"),
        (&lim_cpu_w, "cpu_w"),
        (&lim_gpu, "gpu_temp"),
        (&lim_gpu_floor, "gpu_mhz"),
    ] {
        let weak = Rc::downgrade(&ui);
        let key = key.to_string();
        row.connect_notify_local(Some("value"), move |r, _| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.suppress.get() {
                return;
            }
            record_touch(&ui);
            let v = r.value().round() as u32;
            let mut c = hw::load_config();
            let cur = match key.as_str() {
                "cpu_temp" => c.limit.cpu_temp.map(u32::from),
                "cpu_pct" => Some(u32::from(c.limit.cpu_pct)),
                "gpu_temp" => c.limit.gpu_temp.map(u32::from),
                _ => Some(c.limit.gpu_mhz),
            };
            if cur == Some(v) {
                return; // 程序性回写（刷新同步）不该触发保存
            }
            c.limit.set_key(&key, &v.to_string());
            match hw::save_config(&c) {
                Ok(()) => {
                    if hw::fan_daemon_running() {
                        let _ = hw::restart_fan_daemon();
                    }
                }
                Err(e) => eprintln!("[ui] 保存高温限制失败：{e}"),
            }
        });
    }


    // ------------------------------------------------------------ 交互：GPU 时钟锁
    {
        let (weak, win_weak) = (Rc::downgrade(&ui), win.downgrade());
        gpu_lock.connect_active_notify(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.suppress.get() {
                return;
            }
            record_touch(&ui);
            let on = row.is_active();
            // 解锁时把频率滑块设为不可操作（它是"锁定值"，解锁后无意义）
            ui.gpu_lock_mhz.set_sensitive(on);
            let mhz = ui.gpu_lock_mhz.value().round() as u32;
            let (weak2, win_weak2) = (weak.clone(), win_weak.clone());
            start_op(
                &ui,
                move || {
                    // 下限固定 210（GPU 空闲时本来就到 210，不必再低）
                    let spec = if on { Some(format!("210,{mhz}")) } else { None };
                    match hw::set_gpu_clk_lock(spec.as_deref()) {
                        Ok(true) => Ok(()),
                        Ok(false) => Err("未获授权".into()),
                        Err(e) => Err(e.to_string()),
                    }
                },
                move |res| report(&weak2, &win_weak2, res, "GPU 时钟锁设置失败"),
            );
        });
    }

    // 锁定频率：走和别的滑块一样的去抖通道；只在"已上锁"时真正写入
    connect_slider(
        &ui,
        &gpu_lock_mhz,
        Rc::clone(&ui.slot_gpu_lock),
        {
            let (weak, win_weak) = (Rc::downgrade(&ui), win.downgrade());
            move |v: u32| {
                let (weak, win_weak) = (weak.clone(), win_weak.clone());
                move || {
                    let Some(ui) = weak.upgrade() else { return };
                    if !ui.gpu_lock.is_active() {
                        return; // 还没上锁，拖动只是在挑一个预备值
                    }
                    let (weak, win_weak) = (weak.clone(), win_weak.clone());
                    start_op(
                        &ui,
                        move || match hw::set_gpu_clk_lock(Some(&format!("210,{v}"))) {
                            Ok(true) => Ok(()),
                            Ok(false) => Err("未获授权".into()),
                            Err(e) => Err(e.to_string()),
                        },
                        move |res| report(&weak, &win_weak, res, "GPU 锁定频率设置失败"),
                    );
                }
            }
        },
    );

    // ------------------------------------------------------------ 交互：启动项
    {
        let (weak, win_weak) = (Rc::downgrade(&ui), win.downgrade());
        restore_on_exit.connect_active_notify(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.suppress.get() {
                return;
            }
            let mut cfg = hw::load_config();
            cfg.restore_on_exit = row.is_active();
            if let Err(e) = hw::save_config(&cfg) {
                if let Some(w) = win_weak.upgrade() {
                    alert(&w, "保存设置失败", &e.to_string());
                }
            }
        });
    }

    {
        let (weak, win_weak) = (Rc::downgrade(&ui), win.downgrade());
        hide_on_close.connect_active_notify(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.suppress.get() {
                return;
            }
            let mut cfg = hw::load_config();
            cfg.hide_on_close = row.is_active();
            if let Err(e) = hw::save_config(&cfg) {
                if let Some(w) = win_weak.upgrade() {
                    alert(&w, "保存设置失败", &e.to_string());
                }
            }
        });
    }

    {
        let (weak, win_weak) = (Rc::downgrade(&ui), win.downgrade());
        auto_apply.connect_active_notify(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.suppress.get() {
                return;
            }
            let mut cfg = hw::load_config();
            cfg.auto_apply = row.is_active();
            if let Err(e) = hw::save_config(&cfg) {
                if let Some(w) = win_weak.upgrade() {
                    alert(&w, "保存设置失败", &e.to_string());
                }
            }
        });
    }

    {
        let (weak, win_weak) = (Rc::downgrade(&ui), win.downgrade());
        hud_switch.connect_active_notify(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.suppress.get() {
                return;
            }
            let on = row.is_active();
            let mut cfg = hw::load_config();
            cfg.hud = on;
            if let Err(e) = hw::save_config(&cfg) {
                if let Some(w) = win_weak.upgrade() {
                    alert(&w, "保存设置失败", &e.to_string());
                }
            }
            // 自启项立即生效，不需要重跑 --install
            if let Err(e) = hw::set_hud_autostart(on) {
                if let Some(w) = win_weak.upgrade() {
                    alert(&w, "写入开机自启项失败", &e.to_string());
                }
            }
        });
    }

    // ------------------------------------------------------------ 交互：顶栏托盘
    //
    // 这个开关同时管两件事，因为用户要的是"别再玄学"：
    //   1) **将来**：开/关「登录时自动启动」（写 XDG 的 Hidden 覆盖文件，不需要 root）；
    //   2) **现在**：开=没跑就立刻拉起；关=立刻结束它。
    // ⇒ 于是"关掉再打开"就等于**重启托盘**，不用重新登录。这正是治"托盘没了"最直接的手法。
    {
        let weak = Rc::downgrade(&ui);
        let win_weak = win.downgrade();
        tray_switch.connect_active_notify(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.suppress.get() {
                return;
            }
            record_touch(&ui);
            let on = row.is_active();

            if let Err(e) = hw::set_tray_autostart(on) {
                if let Some(w) = win_weak.upgrade() {
                    alert(&w, "写入托盘自启项失败", &e.to_string());
                }
            }

            let res = if on {
                if hw::tray_pid().is_some() {
                    Ok(()) // 已经在跑，不重复拉起（托盘自己也有单实例保护）
                } else {
                    hw::start_tray().map(|_| ())
                }
            } else {
                hw::stop_tray()
            };
            if let Err(e) = res {
                if let Some(w) = win_weak.upgrade() {
                    alert(&w, "启动 / 结束托盘失败", &e.to_string());
                }
            }

            row.set_subtitle(&tray_subtitle(ui.tray_problem.as_deref()));
            // 进程起来或退出要几十~几百毫秒，稍后再把真实状态读回来。
            let weak2 = weak.clone();
            glib::timeout_add_local_once(Duration::from_millis(700), move || {
                if let Some(ui) = weak2.upgrade() {
                    refresh(&ui);
                }
            });
        });
    }

    // ------------------------------------------------------------ 交互：恢复默认
    {
        let (weak, win_weak) = (Rc::downgrade(&ui), win.downgrade());
        reset_btn.connect_clicked(move |_| {
            let Some(ui) = weak.upgrade() else { return };
            record_touch(&ui);
            let Some(preset) = hw::preset("balanced") else { return };
            {
                let mut cfg = hw::load_config();
                cfg.preset = Some(preset.key.to_string());
                let _ = hw::save_config(&cfg);
            }
            let (weak2, win_weak2) = (weak.clone(), win_weak.clone());
            start_op(
                &ui,
                move || {
                    let (_ok, errs) = hw::apply_preset(preset);
                    if errs.is_empty() {
                        Ok(())
                    } else {
                        Err(errs.join("\n"))
                    }
                },
                move |res| report(&weak2, &win_weak2, res, "恢复默认失败"),
            );
        });
    }

    // ------------------------------------------------------------ 启动时套用保存的档位
    // 这些设置是内核状态，重启会被 BIOS 复位；用户勾了「登录时自动套用」就在这里补上。
    if cfg.auto_apply {
        if let Some(preset) = boot_preset.as_deref().and_then(hw::preset) {
            if !hw::cur_state().matches(preset) {
                let weak = Rc::downgrade(&ui);
                start_op(
                    &ui,
                    move || {
                        let (_ok, errs) = hw::apply_preset(preset);
                        if errs.is_empty() {
                            Ok(())
                        } else {
                            Err(errs.join("\n"))
                        }
                    },
                    move |_res| {
                        // 启动阶段失败就静默（多半是免密还没配好），不弹框打扰
                        if let Some(ui) = weak.upgrade() {
                            refresh(&ui);
                        }
                    },
                );
            }
        }
    }

    // ------------------------------------------------------------ 生命周期：退出与关闭
    //
    // 用户要求"关掉就清零"：启动时拍一份快照，退出时写回，于是设置只在程序存活期间生效。
    //   · 正常退出（「退出」按钮 / 关闭窗口且未开隐藏模式）
    //       → 后台线程执行 snap.restore()（4~6 次提权，200~400 ms），完成后退出应用；
    //   · 被强杀 / 崩溃 / 断电 → pending 标记残留，下次启动自动还原（hw::recover_if_pending）；
    //   · 用户关掉「退出时还原」→ 不做任何还原，只清标记。
    // 用 Arc 而非 Rc：还原要放到后台线程执行，Snapshot 是纯数据（自动 Send/Sync）。
    let snap: Arc<hw::Snapshot> = Arc::new(hw::Snapshot::capture());
    hw::mark_pending(&snap);

    // 老配置没有 restore_on_exit 这个键时按"默认开"处理，避免升级后行为突变
    let want_restore = {
        let c = hw::load_config();
        c.restore_on_exit || !hw::config_has_key("restore_on_exit")
    };
    let hide_on_close = hw::load_config().hide_on_close;

    // 停止后台线程（1.2 s 刷新定时器 + GPU 采样线程）
    let stop_backends = {
        let ui = ui.clone();
        let alive = Arc::clone(&gpu_alive);
        move || {
            ui.running.set(false);
            alive.store(false, Ordering::Relaxed);
        }
    };

    // 统一退出流程。所有捕获都可克隆 ⇒ 闭包自动实现 Clone，可供多处共用。
    let shutdown = {
        let snap = Arc::clone(&snap);
        let app = app.clone();
        let win_weak = win.downgrade();
        move || {
            if !want_restore {
                hw::clear_pending();
                app.quit();
                return;
            }
            let snap = Arc::clone(&snap);
            // channel 传 String：空串 = 全部成功，非空 = 失败说明列表
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            std::thread::spawn(move || {
                let msg = if snap.differs_from_now() {
                    let (_ok, errs) = snap.restore();
                    errs.join("\n")
                } else {
                    String::new()
                };
                hw::clear_pending();
                let _ = tx.send(msg);
            });
            // 还原期间把窗口藏起来，避免"卡住"的观感
            if let Some(w) = win_weak.upgrade() {
                w.set_visible(false);
            }
            // 主线程轮询等还原完成再退出（GLib 要求在主线程结束主循环）
            // 注意 `WeakRef::upgrade(self)` 消耗 self，所以每次都要用 clone 后的副本，
            // 否则这个闭包会退化成 FnOnce（而定时器回调要求 FnMut）。
            let app_q = app.clone();
            let ww = win_weak.clone();
            glib::timeout_add_local(Duration::from_millis(80), move || {
                let msg = match rx.try_recv() {
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return glib::ControlFlow::Continue
                    }
                    Ok(m) => m,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        "还原线程异常退出".to_string()
                    }
                };
                if !msg.is_empty() {
                    if let Some(w) = ww.clone().upgrade() {
                        alert(&w, "退出时还原未完全成功", &msg);
                    }
                }
                app_q.quit();
                glib::ControlFlow::Break
            });
        }
    };

    // 包一层 Rc<dyn Fn()>：下面两个信号处理器的闭包是 `Fn`（可能被多次调用），
    // 不能把捕获的闭包 `move` 走，否则 E0507。
    let shutdown: Rc<dyn Fn()> = Rc::new(shutdown);
    let stop_backends: Rc<dyn Fn()> = Rc::new(stop_backends);

    // 「退出」按钮：显式退出入口（与"关闭窗口"不同，它一定走还原流程）
    {
        let sd = Rc::clone(&shutdown);
        let sb = Rc::clone(&stop_backends);
        quit_btn.connect_clicked(move |_| {
            sb();
            sd();
        });
    }

    // ── SIGTERM / SIGINT / SIGHUP：让"被别的进程结束"也走**干净退出** ────────────
    //
    // 为什么必须有（2026-10-11 实测踩到）：
    //   面板启动时会 `mark_pending()` 落一个"本次运行尚未干净退出"的标记，
    //   **只有干净退出才清掉它**。而 SIGTERM 的默认动作是**立刻终止**，根本不走
    //   close-request ⇒ 标记残留 ⇒ **下次打开面板会以为上次是崩溃**，白白做一次
    //   "还原到快照"（把 cpu_pct / EPP / 睿频 / 频率上限写回旧值），还会弹一句
    //   "检测到上次未正常退出"。
    //   踩到它的路径有两条：
    //     · 托盘菜单「关闭面板与托盘」—— 我给面板发的就是 SIGTERM；
    //     · **每次 deb 升级** —— prerm / postinst 收掉旧面板用的也是 SIGTERM。
    //   接过来转成同一个 `shutdown` 流程后，"被杀"与"点退出"行为一致，不再留尾巴。
    {
        // `static` 直接放在函数体里：进程内单例、无初始化成本、不用加锁。
        static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
        extern "C" fn on_signal(_sig: libc::c_int) {
            // 信号处理器里只能做**异步信号安全**的事：置一个原子标志就够，
            // 真正的收尾交给下面主循环里的定时器（GLib 要求在主线程结束主循环）。
            SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
        }
        unsafe {
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            // 关终端 / 会话结束时来的是 SIGHUP —— 同样别留下半路状态
            libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
        }
        let sd = Rc::clone(&shutdown);
        let sb = Rc::clone(&stop_backends);
        glib::timeout_add_local(Duration::from_millis(200), move || {
            if !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                return glib::ControlFlow::Continue;
            }
            sb();
            sd();
            glib::ControlFlow::Break
        });
    }

    // 关闭请求：**只能注册一个**。GTK 的 `close-request` 用
    // `g_signal_accumulator_true_handled`——一旦某个 handler 返回 Stop，
    // 后续 handler 不会再被调用。所以"停线程 / 隐藏 / 退出"必须合并在同一处。
    {
        let sd = Rc::clone(&shutdown);
        let sb = Rc::clone(&stop_backends);
        let win_weak = win.downgrade();
        win.connect_close_request(move |_| {
            if hide_on_close {
                // 隐藏到后台：窗口**不销毁**（返回 Stop），刷新与采样继续跑。
                // 必须确保浮窗在运行，否则用户会把窗口"弄丢"；浮窗是独立 app id，
                // 重复拉起只会激活已有实例，所以这里可以无脑启动。
                ensure_hud_running();
                if let Some(w) = win_weak.upgrade() {
                    w.set_visible(false);
                }
                return glib::Propagation::Stop;
            }
            sb();
            sd();
            glib::Propagation::Stop
        });
    }


    // ------------------------------------------------------------ 定时刷新
    {
        let ui = ui.clone();
        glib::timeout_add_local(Duration::from_millis(REFRESH_MS), move || {
            if !ui.running.get() {
                return glib::ControlFlow::Break;
            }
            refresh(&ui);
            glib::ControlFlow::Continue
        });
    }

    // 注意：这里**不再**注册第二个 `close_request`。
    // GTK 的该信号用 `g_signal_accumulator_true_handled`：一旦前面某个 handler
    // 返回 `Stop`，后面的就不会再跑。停线程 / 隐藏 / 退出已合并进上面那一个 handler。

    refresh(&ui);
    win
}

/// 一行「标题 + 右侧数值」。数值标签交给调用方持有，以便定时刷新。
fn value_row(title: &str) -> (ActionRow, Label) {
    let row = ActionRow::new();
    row.set_title(title);
    // 与 `spin_row` 同样的护栏：副标题最多 3 行，别让长文案把窗口顶出去。
    row.set_subtitle_lines(3);
    let val = Label::new(Some("--"));
    val.set_css_classes(&["numeric", "dim-label"]);
    val.set_valign(Align::Center);
    row.add_suffix(&val);
    (row, val)
}

/// 创建一个带指定步进的数值行。
///
/// **⚠ 2026-10-07 修复的闪退 bug（用户实测：调5 步/100MHz 时第一下生效、第二下闪退）**
///
/// 根因在 `connect_slider` 的防抖 slot：定时器触发后 `SourceId` 已被 glib 移除，
/// 但 slot 里仍存着那个**已失效的 id**。下次点击 `slot.take()` 拿到它 →
/// `id.remove()` 对已移除的 `SourceId` 调用 → gtk4-rs 内部 panic → 进程闪退。
/// 修法：定时器回调里先 `slot2.set(None)` 把 slot 清空。
///
/// **这个坑的通用形态**：`timeout_add_local_once` + 用 `Cell<Option<SourceId>>`
/// 做防抖时，**必须在回调里清空 slot**，否则第二次操作必崩。
///
///
/// **步进来自 `AdwSpinRow` 内部的 `GtkAdjustment`**，不是 `climb_rate`
/// （后者只控制"按住时的加速"）。默认步进是 1，意味着拖动加减按钮每下只动 1，
/// 对 20..100 的百分比或频率来说太琐碎——所以这里统一显式设置。
///
/// 参数里的 `None::<&gtk::Adjustment>` 必须显式标注类型：`SpinRow::new` 的第一参数是
/// `Option<&impl IsA<Adjustment>>`，直接写 `None` 无法推断（E0283）。
fn spin_row(
    title: &str,
    subtitle: &str,
    min: f64,
    max: f64,
    value: f64,
    step: f64,
) -> SpinRow {
    let row = SpinRow::new(None::<&gtk::Adjustment>, 0.0, 0);
    row.set_title(title);
    row.set_subtitle(subtitle);
    // 副标题行数上限 —— 硬护栏，不是美化。
    // 用户实测反馈（2026-10-11）：PL1 那条副标题写到 5 行，**把整个页面顶出去了**
    // （右侧数值/加减按钮被挤到屏幕外）。文案该短是另一回事，这里是兜底：
    // 无论以后谁再往副标题里塞长句，最多占 3 行。
    row.set_subtitle_lines(3);
    // 先设步进再设范围：set_range 只改上下界，不会覆盖 increment
    let adj = row.adjustment();
    adj.set_step_increment(step);
    adj.set_page_increment(step * 5.0);
    row.set_range(min, max);
    row.set_value(value.clamp(min, max));
    row
}

/// 托盘开关的副标题 —— **只说事实**：进程在不在、登录自启开没开、跑不起来是什么原因。
///
/// 这是"治玄学"的核心：过去托盘失灵时界面上没有任何线索，只能重新登录碰运气；
/// 现在任何一种状态都有对应的、可执行的说法。
fn tray_subtitle(problem: Option<&str>) -> String {
    let autostart = hw::tray_autostart_enabled();
    let pid = hw::tray_pid();
    match (autostart, pid) {
        (true, Some(p)) => format!("运行中（PID {p}）· 登录时会自动启动"),
        (true, None) => match problem {
            Some(why) => format!("已启用但没在运行：{why}"),
            // 没有可诊断的硬故障 ⇒ 让用户用一个动作解决，而不是让他猜
            None => "已启用但当前没在运行 —— 把它关掉再打开，就会立刻拉起".into(),
        },
        (false, Some(_)) => "登录自启已关闭；它现在还在跑（把开关关掉时会一并结束）".into(),
        (false, None) => "已关闭：不启动，登录时也不会启动".into(),
    }
}

/// 记录"用户刚动过控件"，供 `refresh` 判断是否该跳过回写。
fn record_touch(ui: &Ui) {
    ui.last_touch.set(Some(Instant::now()));
}

/// 开始一次提权操作。
///
/// 若已有操作在执行则**直接忽略**——宁可丢一次点击（1.2 s 后刷新会把界面拉回真实状态），
/// 也不要让两组写入交错执行导致最终状态不可预期。
fn start_op<F, D>(ui: &Rc<Ui>, work: F, on_done: D)
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
    D: FnOnce(Result<(), String>) + 'static,
{
    if ui.busy.get() {
        return;
    }
    ui.busy.set(true);
    let ui2 = Rc::clone(ui);
    run_priv(
        work,
        move |res| {
            ui2.busy.set(false);
            on_done(res);
        },
    );
}

/// 上报一次操作结果（失败则弹框），随后刷新界面。
fn report(
    weak: &std::rc::Weak<Ui>,
    win_weak: &glib::WeakRef<ApplicationWindow>,
    res: Result<(), String>,
    title: &'static str,
) {
    if let Err(msg) = res {
        if let Some(w) = win_weak.upgrade() {
            alert(&w, title, &msg);
        }
    }
    if let Some(ui) = weak.upgrade() {
        refresh(&ui);
    }
}

/// 给滑块接上「记录触点 → 去抖 → 提交」的完整链路。
///
/// `slot` 必须是**该滑块专属**的去抖槽（`Rc<Cell<…>>`），否则两个滑块会互相取消。
/// `submit` 收到目标值后返回一个待执行闭包，由去抖定时器在静止 450 ms 后调用。
fn connect_slider<P, S>(ui: &Rc<Ui>, row: &SpinRow, slot: Slot, submit: S)
where
    P: FnOnce() + 'static,
    S: Fn(u32) -> P + 'static,
{
    let weak = Rc::downgrade(ui);
    let submit = Rc::new(submit);
    row.connect_value_notify(move |r| {
        let Some(ui) = weak.upgrade() else { return };
        // 程序化 set_value 触发的通知直接忽略，否则会形成回环
        if ui.suppress.get() {
            return;
        }
        record_touch(&ui);
        let action = (submit)(r.value().round() as u32);
        if let Some(id) = slot.take() {
            id.remove();
        }
        let holder = Rc::new(RefCell::new(Some(action)));
        // slot2 是本次定时器的"身份"，回调里用它把 slot 清成 None。
        //
        // ⚠⚠ 不清空会导致**第二次点击闪退**（用户实测：调 5 步/100MHz 时
        // 第一下生效、第二下闪退）：
        //   timeout_add_local_once 触发后，SourceId 已被 glib 移除，
        //   但 slot 里**仍存着那个已失效的 SourceId**。
        //   下次点击 `slot.take()` 拿到它 → `id.remove()` 对已移除的
        //   SourceId 调用 ⇒ gtk4-rs 内部 panic ⇒ 进程闪退。
        let slot2 = slot.clone();
        let id = glib::timeout_add_local_once(Duration::from_millis(DEBOUNCE_MS), move || {
            // 先清 slot（此刻它还存着本次 id），再执行动作
            slot2.set(None);
            if let Some(f) = holder.borrow_mut().take() {
                f();
            }
        });
        slot.set(Some(id));
    });
}

/// 在后台线程执行可能耗时的提权操作，完成后回主线程执行 `on_done`。
///
/// `work` 必须 `Send`，因此只能捕获值类型，不能捕获 GTK 对象。
fn run_priv<F, D>(work: F, on_done: D)
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
    D: FnOnce(Result<(), String>) + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    let rx = Rc::new(RefCell::new(rx));
    // 闭包本身是 FnMut（可能在结果到达前被多次调用），而 on_done 是 FnOnce，
    // 用 Option 包一层，在真正拿到结果时 take() 出来调用。
    let mut on_done = Some(on_done);
    glib::timeout_add_local(Duration::from_millis(60), move || {
        match rx.borrow().try_recv() {
            Ok(res) => {
                if let Some(f) = on_done.take() {
                    f(res);
                }
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
        }
    });
}

/// 刷新界面。
///
/// 分两部分：
/// - **可操作控件**（滑块/开关/档位）受 `TOUCH_GUARD` 保护——用户刚动过就不回写，
///   否则会把正在拖动的滑块弹回旧值；
/// - **只读项**（频率/GPU/风扇/温度）总是更新。
fn refresh(ui: &Ui) {
    ui.suppress.set(true);

    let touch_recent = ui
        .last_touch
        .get()
        .map(|t| t.elapsed() < TOUCH_GUARD)
        .unwrap_or(false);

    // GPU 时钟锁的当前状态取自后台采样缓存 —— **绝不能在这里调 nvidia-smi**
    // （一次 60 ms，会把界面拖卡，这是本文件开头就定下的纪律）。
    let cached_lock = ui
        .gpu
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|g| g.clk_lock));

    if !touch_recent {
        // ---- 档位高亮：以**用户选的那一档**（`config.preset`）为准 ----
        // ⚠ 2026-10-11 用户实测反馈：原实现比对的是**实时 sysfs 状态**，于是高温限流
        //   一收紧，实时状态就与任何档位都不匹配 ⇒ 高亮消失；温度回落又亮回来 ——
        //   和滑块一样来回跳。档位是"我选的"、不是"此刻恰好是什么"，
        //   所以改读 config，与下面两个滑块用同一套语义。
        let cfg_now = hw::load_config();
        let want = cfg_now.preset.as_deref().filter(|k| hw::preset(k).is_some());
        if ui.preset_group.active_name().as_deref() != want {
            ui.preset_group.set_active_name(want);
        }

        // ---- 两个「设定值」滑块：只跟 config 里的基准走，**绝不跟 sysfs** ----
        // ⚠⚠ 这是本次修复的核心。sysfs 上的 `max_perf_pct` / PL1 会被高温限流守护
        //   改写（100→62、135→30），跟着它刷新就等于「控件自己在跳」，而且用户每次
        //   设完 1.2 s 就被回写、根本改不动（用户 2026-10-11 实测反馈）。
        //   守护此刻真正写进硬件的值，在下面「只读项」里单独展示（cpu_eff / rapl_eff）。
        let cfg_pct = cfg_now
            .cpu_pct
            .or_else(|| cfg_now.preset.as_deref().and_then(hw::preset).map(|p| p.cpu_pct));
        if let Some(v) = cfg_pct {
            if (ui.cpu_pct.value().round() as u32) != v {
                ui.cpu_pct.set_value(v as f64);
            }
        }
        if let Some(w) = cfg_now.cpu_pl1 {
            let w = w.min(200);
            if (ui.rapl.value().round() as u32) != w {
                ui.rapl.set_value(w as f64);
            }
        }

        let st = hw::cur_state();
        if let Some(v) = st.turbo {
            if ui.turbo.is_active() != v {
                ui.turbo.set_active(v);
            }
        }
        if let Some(v) = hw::cpu_cap_mhz() {
            let v = v.round() as u32;
            if (ui.cpu_cap.value().round() as u32) != v {
                ui.cpu_cap.set_value(v as f64);
            }
        }

        // GPU 时钟锁：锁没锁以 **nvidia-smi 实际报告**为准（不是"我们以为设了"）
        match cached_lock {
            Some((_lo, hi)) => {
                if !ui.gpu_lock.is_active() {
                    ui.gpu_lock.set_active(true);
                }
                ui.gpu_lock_mhz.set_sensitive(true);
                if (ui.gpu_lock_mhz.value().round() as u32) != hi {
                    ui.gpu_lock_mhz.set_value(hi as f64);
                }
            }
            None => {
                if ui.gpu_lock.is_active() {
                    ui.gpu_lock.set_active(false);
                }
                ui.gpu_lock_mhz.set_sensitive(false);
            }
        }

        // ⚠ 2026-10-11 修正：**不再**因为「高温限制」启用而锁住 CPU 组 / GPU 组。
        //   旧逻辑是一打开开关就把两组全灰掉，用户看到的就是"CPU 和 GPU 都不可调"。
        //   那是**互斥**思路，与"整合"相悖 —— 整合后两者应当协同：
        //     手动设的是**基准**，守护只在高温时在其上**收紧**（`min(基准, 目标)`），
        //     温度回落就写回基准。
        //   ⇒ 代价是"守护生效期间手动改动可能被它覆盖"，这用**文案说明**，
        //     而不是把控件灰掉（灰掉连"能不能改"都无法回答）。
        let lim_on = cfg_now.limit.enabled;
        ui.cpu_group.set_sensitive(true);
        ui.lim_cpu.set_sensitive(lim_on);
        ui.lim_cpu_floor.set_sensitive(lim_on);
        ui.lim_cpu_w.set_sensitive(lim_on);
        ui.lim_gpu.set_sensitive(lim_on);
        ui.lim_gpu_floor.set_sensitive(lim_on);
        // GPU 时钟锁不再被锁住：用户可以先设频率再开锁（旧逻辑要求先锁才能调，反直觉）。
        // 守护接管时会以「GPU 组当前设的值」为基准只收紧，不会凭空放宽。

            // 风扇曲线守护：没跑 ⇒ 曲线不生效 ⇒ 盖遮罩 + 置灰 + 把开关回写正确
            let fand_on = hw::fan_daemon_running();
            ui.curve_mask.set_visible(!fand_on);
            ui.curve.set_sensitive_editor(fand_on);
            // ⚠ 操作进行中（busy）**不要**回写开关：
            //   开守护要 modprobe / rmmod + systemctl，耗时 1~3 秒；
            //   这期间本函数每 1.2s 跑一次，会把用户刚拨上的开关按"事实"拨回去，
            //   看起来就是"点了自己弹回、开不了"。等操作结束由 report() 再刷新。
            if !ui.busy.get() && ui.fand.is_active() != fand_on {
                let prev = ui.suppress.replace(true);
                ui.fand.set_active(fand_on);
                ui.suppress.set(prev);
        }
        // 顶栏托盘：开关状态**只看事实**（自启文件在不在 + 进程在不在），
        // 并把结果如实写进副标题 —— 用户要的就是"别再玄学"，
        // 所以这里绝不能显示"我们希望的状态"，只能显示真实状态。
        let tray_on = hw::tray_autostart_enabled();
        if ui.tray_sw.is_active() != tray_on {
            let prev = ui.suppress.replace(true);
            ui.tray_sw.set_active(tray_on);
            ui.suppress.set(prev);
        }
        ui.tray_sw.set_subtitle(&tray_subtitle(ui.tray_problem.as_deref()));

        // 风扇迟滞：配置文件可能被守护的收尾、或另一个窗口改过 ⇒ 每次回读并同步显示。
        // 必须走 suppress，否则会把"回写"当成"用户切换"而白重启一次守护。
        let lag_now = cfg_now.fan_lag;
        if ui.fan_lag.selected() != lag_now as u32 {
            let prev = ui.suppress.replace(true);
            ui.fan_lag.set_selected(lag_now as u32);
            ui.fan_lag.set_subtitle(&lag_now.hint());
            ui.suppress.set(prev);
        }
        // ⚠ 2026-10-11 **删除**：这里原先把 `hw::rapl_w()`（实时 sysfs）回写进 PL1 滑块。
        //   那正是「PL1 一直跳、用户改不动」的直接原因 —— 高温限流守护每收紧/还原一次，
        //   滑块就跟着跳一次，用户设的值 1.2 s 后就被覆盖。
        //   现在滑块只跟 config 里的**设定值**走（见本函数上方），
        //   实时值改在只读项 `ui.rapl_eff` 里展示 —— 跳就跳在那里，不影响设定。
    }

    // ---- 只读项：总是更新
    //
    // 「当前生效」两行就是**故意会跳**的那一层：高温限流守护收紧/还原时，这里跟着变。
    // 把跳放在只读项里、而不是滑块上，用户才能同时看到"我设了多少"和"此刻实际多少"。
    let set_pct = ui.cpu_pct.value().round() as u32;
    ui.cpu_eff.set_text(&match hw::cpu_max_pct() {
        Some(v) if v != set_pct => format!("{v} %　（高温限流中 · 设定 {set_pct}%）"),
        Some(v) => format!("{v} %"),
        None => "—".into(),
    });
    let set_w = ui.rapl.value().round() as u32;
    ui.rapl_eff.set_text(&match hw::rapl_w() {
        Some(w) if w != set_w => format!("{w} W　（高温限流中 · 设定 {set_w} W）"),
        Some(w) => format!("{w} W"),
        None => "—".into(),
    });

    ui.acpi_profile
        .set_text(&fmt_opt(hw::current_profile()));

    let cur = hw::cpu_freq_mhz().unwrap_or(0.0) / 1000.0;
    let cap = hw::cpu_cap_mhz().unwrap_or(0.0) / 1000.0;
    let hw_max = hw::cpu_hw_max_mhz().unwrap_or(0.0) / 1000.0;
    ui.cpu_freq
        .set_text(&format!("{cur:.2} / {cap:.2} GHz  (硬件 {hw_max:.1})"));

    if let Ok(g) = ui.gpu.lock() {
        if let Some(g) = g.as_ref() {
            // 功耗墙是平台动态值（含 Dynamic Boost），随负载/温度浮动，只作展示
            ui.gpu_limit.set_text(&match (g.power_limit_w, g.power_max_w) {
                (Some(l), Some(m)) => format!("{l} W / 上限 {m} W"),
                (Some(l), None) => format!("{l} W"),
                _ => "—".into(),
            });
            ui.gpu_temp.set_text(&fmt_opt(g.temp_c.map(|t| format!("{t} °C"))));
            // 实时功耗带上 pstate（P8=空闲 … P0=满载），能一眼看出 GPU 是否在工作
            ui.gpu_draw.set_text(&match (g.power_draw_w, g.pstate.as_deref()) {
                (Some(w), Some(p)) => format!("{w:.1} W  ({p})"),
                (Some(w), None) => format!("{w:.1} W"),
                _ => "—".into(),
            });
            ui.gpu_clk.set_text(&format!(
                "{} / {}",
                fmt_opt(g.clock_sm_mhz.map(|c| format!("{c} MHz"))),
                fmt_opt(g.clock_mem_mhz.map(|c| format!("{c} MHz")))
            ));
            ui.gpu_util.set_text(&format!(
                "{} / {}",
                fmt_opt(g.util_pct.map(|u| format!("{u}%"))),
                fmt_opt(g.mem_util_pct.map(|u| format!("{u}%"))),
            ));
            ui.gpu_mem.set_text(&match (g.mem_used_mb, g.mem_total_mb) {
                (Some(u), Some(t)) => format!("{u} / {t} MB"),
                (Some(u), None) => format!("{u} MB"),
                _ => "—".into(),
            });
        } else {
            for l in [
                &ui.gpu_limit,
                &ui.gpu_temp,
                &ui.gpu_draw,
                &ui.gpu_clk,
                &ui.gpu_util,
                &ui.gpu_mem,
            ] {
                l.set_text("—");
            }
        }
    }

    let rpms = hw::fan_rpms();
    for (i, val) in ui.fan_values.iter().enumerate() {
        let text = rpms
            .get(i)
            .map(|r| format!("{r} RPM"))
            .unwrap_or_else(|| "--".to_string());
        val.set_text(&text);
    }

    // 显示用平滑值（CPU 那一路做 EMA）—— 原始瞬时值跳得太厉害，见 `hw::temps_smooth`
    let temps = hw::temps_smooth();
    for (val, name) in ui.temp_values.iter().zip(TEMP_NAMES) {
        let text = temps
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, t)| format!("{t:.1} °C"))
            .unwrap_or_else(|| "--".to_string());
        val.set_text(&text);
    }

    ui.suppress.set(false);
}

/// 把可选文本渲染出来，缺值统一显示破折号。
fn fmt_opt(v: Option<String>) -> String {
    v.unwrap_or_else(|| "—".into())
}

/// 界面提示 —— 用 **Toast**，自动消失、不打断操作。
///
/// 血案（2026-10-10，用户实测）：原来用的是 `AlertDialog`，
/// **但没有加任何响应按钮** ⇒ 弹出后没有任何关闭手段（点外面也不关），
/// 只能杀掉程序。换成 `AdwToast`：
///   · 默认 5 秒自动消失（这里设 6 秒，错误信息通常要读一下）；
///   · 点一下也能立刻关掉；
///   · 非模态，不挡住后面的操作。
///
/// 名字仍叫 `alert` 是为了不动 8 个既有调用点。
fn alert(parent: &impl IsA<gtk::Widget>, heading: &str, body: &str) {
    let w: &gtk::Widget = parent.as_ref();
    let Some(overlay) = find_toast_overlay(w) else {
        // 理论上窗口根就是 ToastOverlay；万一没有，退化成打日志，也别 panic
        eprintln!("[omen-control] {heading}：{body}");
        return;
    };
    let t = Toast::new(&format!("{heading}：{body}"));
    t.set_timeout(6);
    // 失败类的提示会顶掉当前正在显示的那条，避免排队等待
    if heading.contains("失败") || heading.contains("未完全") {
        t.set_priority(ToastPriority::High);
    }
    overlay.add_toast(t);
}

/// 在组件树里找第一个 `AdwToastOverlay`。
///
/// 窗口根节点就是它，所以第一层就能命中；递归只是为了稳妥。
fn find_toast_overlay(w: &gtk::Widget) -> Option<ToastOverlay> {
    if let Some(o) = w.downcast_ref::<ToastOverlay>() {
        return Some(o.clone());
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        if let Some(o) = find_toast_overlay(&c) {
            return Some(o);
        }
        child = c.next_sibling();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「关于」对话框必须有**关闭按钮**。
    ///
    /// 血案：曾误设 `can_close(false)` —— 那个值是给"嵌入式 About 视图"用的
    /// （没有标题栏的场景）。这里是独立对话框，设 false ⇒ 用户点进「关于」后关不掉。
    /// 这个断言把语义钉住，避免以后又被改回去。
    #[test]
    fn about_dialog_has_close_button() {
        // 没有可用的 display 就跳过（CI/无头环境）
        if gtk::init().is_err() {
            return;
        }
        let d = adw::AboutDialog::builder()
            .application_name("测试")
            .can_close(true)
            .build();
        assert!(d.can_close(), "AboutDialog 必须是可关闭的（否则用户关不掉对话框）");
    }
}
