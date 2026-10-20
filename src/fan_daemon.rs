//! 风扇曲线守护 —— `omen-control --fan-daemon`
//!
//! 读配置里**当前预设**对应的一条曲线（`curve.<preset>`），按温度持续应用：
//!
//! ```text
//! 温度 < 曲线首点  →  写 0，交回固件（BIOS 曲线那一段平滑、安静）
//! 曲线首点..末点   →  线性插值出目标 RPM → 反查百分比 → 写 fan_speed
//! 温度 ≥ 末点      →  末点的 RPM
//! ```
//!
//! # 为什么要有它（而不是塞进 GUI）
//!
//! GUI 关掉后曲线还得继续生效；开机也要能自启。所以做成独立子命令：
//!
//! ```text
//! omen-control --fan-daemon              # 前台跑
//! omen-control --fan-daemon --dry-run    # 预演，完全不写硬件
//! nohup omen-control --fan-daemon &      # 后台
//! ```
//!
//! # 安全设计
//!
//! - **SIGINT / SIGTERM** → 交回固件再退出。Ctrl-C 不会把风扇留在半路。
//! - **转速异常**（接管中 <600 或 >5400 RPM）连续 3 次 → 交回固件并退出。
//! - **步进节流**：目标变化 < 150 RPM 不写，避免在阈值附近抖动、也少写 EC。
//! - **曲线判据用「三点中位数」**：空载时单个核心也会瞬时冲到 93~100°C
//!   （实测：某次前三大核温 `73,72,56`，中位数只有 52），直接取最高会被毛刺骗。

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::{curve, hw, limit, omen};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    // 信号处理里只能做 async-signal-safe 的事：写一个原子量就够了
    STOP.store(true, Ordering::SeqCst);
}

fn print_help() {
    println!(
        "omen-control --fan-daemon —— 风扇曲线守护\n\n\
         读 ~/.config/omen-control/config 里当前预设对应的曲线（curve.<预设>），\n\
         按 CPU/GPU 温度持续应用；温度低于曲线首点时交回固件。\n\n\
         用法：\n\
           omen-control --fan-daemon [选项]\n\n\
         选项：\n\
           --dry-run        只打印决策，完全不写硬件\n\
           --once           跑一轮就退出（测试用）\n\
           --interval N     采样间隔秒数（默认 2）\n\
           --verbose, -v    每轮都打印，包括「保持」\n\
           --step N         目标变化超过 N RPM 才写（默认 150）\n\
           --heartbeat N    心跳打印间隔秒数（默认 30，0 = 关）\n\
           --simulate LIST  离线模拟：喂一串温度出决策表，完全不碰硬件\n\
                            例：--simulate 40,55,60,70,80,90\n\
           --curve SPEC     临时覆盖曲线（不写配置文件）\n\
                            例：--curve 50:2000,70:4000 --simulate 40,60,80\n\
           -h, --help       显示本帮助\n\n\
         退出（含 Ctrl-C）会自动交回固件：写 0 + unthrottle 1→0。\n\
         曲线在 GUI 的「性能档位 → 风扇曲线」里编辑；也可以直接改配置：\n\
           curve.balanced=60:2200,72:3200,82:4597"
    );
}

/// 各核温度（毫摄氏度 → 摄氏度），按 hwmon 里的 `coretemp*` 收集。
fn core_temps() -> Vec<u8> {
    let mut v = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") else {
        return v;
    };
    for e in entries.flatten() {
        let dir = e.path();
        let name = std::fs::read_to_string(dir.join("name")).unwrap_or_default();
        // 多路同类设备时内核会加序号后缀，所以用前缀匹配
        if !name.trim().starts_with("coretemp") {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            let Some(fname) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !fname.starts_with("temp") || !fname.ends_with("_input") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p) {
                if let Ok(milli) = text.trim().parse::<i64>() {
                    v.push((milli / 1000) as u8);
                }
            }
        }
    }
    v
}

/// CPU 温度（°C）。
///
/// **必须优先用 `Package id 0`（封装温度），不能取所有核的 max。**
///
/// 原因（本机 i9-12900HX 实测，198 次采样 × 17 通道）：
/// - 封装温度在物理上必然 ≥ 任何单核温度，它就是"整个芯片最热处"。
/// - 8 个 E-core（`Core 32~39`）空闲时恒在 44~52°C，而某个 P-core 满载时
///   单核可冲到 97°C。取 `max(所有核)` 得到的是**某一个核**的温度，
///   它随负载剧烈跳动（实测标准差 18.5°C），而封装温度平滑得多。
/// - 跳动的直接后果：守护按 97°C 决策 → 风扇拉满 → 温度回落 → 松劲 → 又冲顶，
///   形成用户观察到的"反复冲顶"振荡。
///
/// 封装温度读不到时才回落到 `max(核)`（老内核 / 非 coretemp 平台）。
fn cpu_temp() -> u8 {
    if let Some(p) = package_temp() {
        return p;
    }
    core_temps().into_iter().max().unwrap_or(0)
}

/// `coretemp` 的 `Package id 0`（封装温度，°C）。读不到返回 None。
fn package_temp() -> Option<u8> {
    let entries = std::fs::read_dir("/sys/class/hwmon").ok()?;
    for e in entries.flatten() {
        let dir = e.path();
        let name = std::fs::read_to_string(dir.join("name")).unwrap_or_default();
        if !name.trim().starts_with("coretemp") {
            continue;
        }
        // 先按 label 找（不同内核的通道编号不一致，label 才可靠）
        for idx in 1..=64u32 {
            let lp = dir.join(format!("temp{idx}_label"));
            let Ok(label) = std::fs::read_to_string(&lp) else {
                continue;
            };
            if !label.contains("Package") {
                continue;
            }
            let ip = dir.join(format!("temp{idx}_input"));
            if let Ok(text) = std::fs::read_to_string(&ip) {
                if let Ok(milli) = text.trim().parse::<i64>() {
                    if (1000..200_000).contains(&milli) {
                        return Some((milli / 1000) as u8);
                    }
                }
            }
        }
    }
    None
}

fn gpu_temp() -> u8 {
    let Ok(out) = Command::new("nvidia-smi")
        .args([
            "--query-gpu=temperature.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
    else {
        return 0;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|l| l.trim().parse::<u8>().ok())
        .unwrap_or(0)
}

/// 温度平滑：**升温快、降温慢**（非对称），并带变化率上限。
///
/// ## 为什么非对称
///
/// 风扇控制里的经典教训：**降得比升得慢**才不振荡。
/// 对称平滑（或原来的 `med3`）会在温度一回落就立刻松劲，
/// 而 i9-12900HX 睿频一上来 2 秒就能把封装从 60 推到 96 ——
/// 于是形成「冲顶→拉满→回落→松劲→又冲顶」的循环（用户实测到的振荡）。
///
/// 升温侧**必须快**，否则真有高温时风扇还在慢慢爬，等于没保护。
/// 所以：升温用大步长上限（追上真实热量），降温用小 EMA 系数（拖住不让它松）。
///
/// ## 为什么原来的 `med3` 不够
///
/// `med3` 只比三个样本，**连续两次**采到高值就穿透。
/// 而 12900HX 满载时封装温度能连续好几秒停在 90+（实测），
/// `med3` 对这种持续冲顶完全无效，只滤单点噪声。
///
/// 升降温用**同一个时间常数**（`hw::FanLag::tau_secs`）——
/// 这正是用户要的"时间迟滞"：温度要持续稳定 τ 秒，其结果才被采纳。
/// 于是 CPU 在阈值附近来回抖时风扇不动，而温度一路爬升时一定会跟上。
///
/// 另有 `rise_max`（升温单步上限）作为**安全通道**：
/// τ 再大也不能让风扇在封装冲到 97°C 时还没反应。
/// 温度平滑：**升温快、降温慢**（非对称），并带变化率上限。
///
/// ## 为什么非对称
///
/// 风扇控制里的经典教训：**降得比升得慢**才不振荡。
/// 对称平滑（或原来的 `med3`）会在温度一回落就立刻松劲，
/// 而 i9-12900HX 睿频一上来 2 秒就能把封装从 60 推到 96 ——
/// 于是形成「冲顶→拉满→回落→松劲→又冲顶」的循环（用户实测到的振荡）。
///
/// 升温侧**必须快**，否则真有高温时风扇还在慢慢爬，等于没保护。
/// 所以：升温用大步长上限（追上真实热量），降温用小 EMA 系数（拖住不让它松）。
///
/// ## 为什么原来的 `med3` 不够
///
/// `med3` 只比三个样本，**连续两次**采到高值就穿透。
/// 而 12900HX 满载时封装温度能连续好几秒停在 90+（实测），
/// `med3` 对这种持续冲顶完全无效，只滤单点噪声。
///
/// 升降温用**同一个时间常数**（`hw::FanLag::tau_secs`）——
/// 这正是用户要的"时间迟滞"：温度要持续稳定 τ 秒，其结果才被采纳。
/// 于是 CPU 在阈值附近来回抖时风扇不动，而温度一路爬升时一定会跟上。
///
/// 另有 `rise_max`（升温单步上限）作为**安全通道**：
/// τ 再大也不能让风扇在封装冲到 97°C 时还没反应。
fn smooth(prev: Option<f64>, cur: f64, lag: hw::FanLag, dt: f64) -> f64 {
    // ⚠ 升降温用**同一个** α（对称）—— 这是时间迟滞的本质：
    //   温度要**持续**稳定 τ 秒，它的结果才被采纳。
    //
    //   早前一版做成非对称（升温直跳、降温慢走）治的是
    //   「一次冲顶后回落」；但那种形状在温度于阈值附近**反复抖动**时
    //   每次都走"升温"分支 ⇒ 一步直达 ⇒ 抖动完全没被滤掉
    //   （实测三档 spread 全是 2°C，被单测当场抓住）。
    //
    //   真正的防抖必须**两个方向同样迟钝**，否则总有一侧会把噪声放进来。
    //   真高温的快速响应交给下面的 `rise_max` 兜底，而不是靠"升得快"。
    let alpha = lag.alpha(dt);
    let rise_max = lag.rise_max();
    match prev {
        None => cur,
        Some(p) => {
            let stepped = p + alpha * (cur - p);
            if cur > p {
                // 安全通道：升温再慢也不得一步跨过 rise_max
                stepped.min(p + rise_max)
            } else {
                stepped
            }
        }
    }
}

pub fn run(args: &[String]) {
    let mut dry = false;
    let mut once = false;
    let mut verbose = false;
    let mut interval: u64 = 2;
    let mut step_rpm: u16 = 150;
    let mut heartbeat: u64 = 30;
    let mut simulate: Option<String> = None;
    let mut curve_override: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => dry = true,
            "--once" => once = true,
            "--verbose" | "-v" => verbose = true,
            "--interval" => {
                interval = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(2).max(1);
                i += 1;
            }
            "--step" => {
                step_rpm = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(150);
                i += 1;
            }
            "--heartbeat" => {
                heartbeat = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(30);
                i += 1;
            }
            "--simulate" => {
                simulate = args.get(i + 1).cloned();
                if simulate.is_none() {
                    eprintln!("--simulate 需要一个温度列表，例如 --simulate 40,55,60,70,80,90");
                    std::process::exit(2);
                }
                i += 1;
            }
            "--curve" => {
                curve_override = args.get(i + 1).cloned();
                i += 1;
            }
            "-h" | "--help" => {
                print_help();
                return;
            }
            other => {
                eprintln!("未知参数：{other}（用 --help）");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    // ── 取当前预设的曲线 ──
    let cfg = hw::load_config();
    let key = cfg
        .preset
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "balanced".to_string());
    // --curve 临时覆盖曲线（只影响本次运行，不写配置文件）——
    // 调参数时不必反复改配置：--curve "50:2000,70:4000" --simulate 40,60,80
    let cur = match &curve_override {
        Some(spec) => curve::FanCurve::parse(spec),
        None => cfg.curve_for(&key),
    };

    // ── 温度限制策略（与曲线**分工**）──
    //
    //   风扇 = 曲线，走 `fan_speed`（plugdev 可写，**免提权**）
    //   限制 = 本策略，走 helper → `intel_pstate` / `nvidia-smi`（**需提权**）
    //
    // 两条独立通道、同一个守护、同一份温度 ⇒ 不会互相覆盖。
    // 关闭时（默认）`target()` 恒返回 None，`Limiter` 也就恒为 Hold，
    // 一次 helper 都不会调用 —— 未启用时完全不碰提权节点。
    //
    // 「档位」是**基准**，「限制」只在其上收紧；温度回落就写回基准，
    // 所以不需要"退出守护才还原"。
    // ⚠ 用 `hw::cpu_base_pct()` 而不是档位自己的 `cpu_pct`：用户可能在面板上
    //   单独把「性能上限」拉到更低的值（`config.cpu_pct`），那才是基准。
    let base_pct = hw::cpu_base_pct();
    let mut limiter = limit::Limiter::new(cfg.limit.clone());

    println!("── 风扇曲线守护 ──");
    // 把"这份守护到底是哪个构建"写进日志 —— 以后看 journalctl 就能排除"旧进程"，
    // 不必再去比对二进制 md5（本轮为此连续吃了两次亏）。
    println!(
        "  版本 {} · 构建 {} · {}",
        env!("CARGO_PKG_VERSION"),
        option_env!("OMEN_BUILD_TS").unwrap_or("(未注入)"),
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    );
    println!("  预设：{key}   曲线：{}", if cur.is_empty() { "（空）".into() } else { cur.encode() });
    if dry {
        println!("  DRY-RUN：不会写任何硬件");
    } else if !omen::available() {
        eprintln!("  ✗ 找不到 /sys/devices/platform/omen-wmi —— hp_wmi_omen 模块没装？");
        std::process::exit(1);
    }
    println!(
        "  采样 {interval}s · 步进阈值 {step_rpm} RPM · Ctrl-C 交回固件"
    );
    println!(
        "  风扇迟滞：{} · 时间常数 {:.0}s · 升温最多 +{:.0}°C/拍",
        cfg.fan_lag.label(),
        cfg.fan_lag.tau_secs(),
        cfg.fan_lag.rise_max(),
    );
    println!(
        "  温度限制：{}",
        if cfg.limit.enabled {
            format!(
                "已启用（CPU {} · GPU {} · PL1 {} · 迟滞 {}°C / 连续 {} 次）",
                cfg.limit
                    .cpu_temp
                    .map(|t| format!("{t}°C 起压，最低 {}%", cfg.limit.cpu_pct))
                    .unwrap_or_else(|| "未启用".into()),
                cfg.limit
                    .gpu_temp
                    .map(|t| format!("{t}°C 起锁，最低 {} MHz", cfg.limit.gpu_mhz))
                    .unwrap_or_else(|| "未启用".into()),
                // PL1：满足温度时把封装功耗收到这个值（不动 = 不管功耗墙）
                cfg
                    .limit
                    .cpu_w
                    .map(|w| format!("{w} W"))
                    .unwrap_or_else(|| "不动".into()),
                cfg.limit.cool_down,
                cfg.limit.confirm
            )
        } else {
            "未启用（只按曲线调风扇；想让高温时自动压 CPU/GPU 就设 limit.enabled=1）".to_string()
        }
    );
    if heartbeat > 0 {
        println!("  运行中每 {heartbeat}s 打一次心跳；只在决策变化时打印动作（想全打加 -v）");
    }
    println!();

    if cur.is_empty() {
        println!("  该预设不接管风扇 → 交回固件后退出");
        if !dry {
            let _ = omen::release_fans();
        }
        return;
    }

    // ── 离线模拟：喂一串温度，直接出决策表（不读温度、不写硬件）──
    if let Some(list) = &simulate {
        // 与主循环同一套判据：CPU / GPU **各一条曲线**，取更激进的。
        // ⚠ 预览用**同一个温度**同时喂两者，所以这里看到的是"若 CPU 与 GPU 同温"
        //   的结果；真机是两个独立读数。
        let gpu_sim = cfg.gpu_curve_for(&key);
        let first = cur.points.first().map(|p| p.temp).unwrap_or(0);
        println!("  温度   目标RPM   写 fan_speed   温度限制决策");
        println!("  ────   ───────   ────────────   ──────────────────────────");
        for tok in list.split(',') {
            let Ok(t) = tok.trim().parse::<u8>() else {
                println!("  {tok:>4}   （无法解析，跳过）");
                continue;
            };
            // 限制决策：与运行时同一套判据（这里用无迟滞的 Prev::default()，
            // 因为 simulate 是在"预览阈值"，不带历史状态更直观）
            let lim_note = match cfg.limit.target(t, Some(t), base_pct, &limit::Prev::default()) {
                None => "限制：不干预".to_string(),
                Some(a) => {
                    let mut parts = Vec::new();
                    parts.push(match a.pct {
                        Some(v) => format!("CPU {v}%"),
                        None => "CPU 不干预".to_string(),
                    });
                    if let Some(m) = a.gpu_lock {
                        parts.push(format!("GPU {m} MHz"));
                    }
                    if let Some(w) = a.pl1_w {
                        parts.push(format!("PL1 {w} W"));
                    }
                    format!("限制：{}", parts.join(" · "))
                }
            };
                        let sim_target = match (cur.target_rpm(t), gpu_sim.target_rpm(t)) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            match sim_target {
                None => println!(
                    "  {t:>3}°C        —           0     低于首点 {first}°C → 交回固件   | {lim_note}"
                ),
                Some(rpm) => {
                    let pct = curve::rpm_to_pct(rpm);
                    println!("  {t:>3}°C   {rpm:>6}       {pct:>3}%                          | {lim_note}");
                }
            }
        }
        println!(
            "\n  （--simulate 不碰硬件；使用同一温度同时喂 CPU/GPU，仅为预览阈值 ——\n                   真机有两个独立读数（CPU 取封装温度），且都经过时间迟滞平滑；\n                   因此这里显示的是**未经平滑**的原始曲线值）"
        );
        return;
    }

    // ── 注册信号：保证 Ctrl-C 也走收尾路径 ──
    if !dry {
        unsafe {
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
            // 直接关掉终端窗口时收到的是 SIGHUP —— 不接它就来不及交回固件，
            // 手动百分比会留在 EC 里（2026-10-10 发现）。
            libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
        }
    }

    // ── 启动时**主动套用一次用户基准** ──────────────────────────────────────
    //
    // 为什么必须有这一步（2026-10-11 实测血案之三）：
    //   基准值（`cpu.pct` / `cpu.pl1`）原先只**存在配置里**，而守护只在
    //   「限流 → 释放」的状态跃迁时才写硬件 ⇒ 把"存着的设定"推回硬件这件事
    //   **没有任何人做**。后果有两个，用户都踩到了：
    //     · 手改配置 / 上次会话设好的值，重启守护后并不会生效
    //       （实测：`echo cpu.pct=50` + 重启守护，`max_perf_pct` 仍是 100）；
    //     · 重启机器后 BIOS 把 RAPL 复位成 135 W，"钉住的墙"当场丢失。
    //   这里在启动时断言一次，`cpu.pct` / `cpu.pl1` 才真的称得上"设定值"。
    //
    // 放在这里（模拟/干跑分支之后、主循环之前）：`--simulate` 与 `-n` 绝不写硬件。
    if !dry {
        let pct = hw::cpu_base_pct();
        match hw::set_cpu_max_pct(pct) {
            Ok(true) => println!("  已套用设定上限：CPU {pct}%"),
            Ok(false) => eprintln!("  ! 设定上限 {pct}% 未获授权（免密 helper 没装好？）"),
            Err(e) => eprintln!("  ! 设定上限失败：{e}"),
        }
        if let Some(w) = hw::load_config().cpu_pl1 {
            match hw::set_rapl_w(w) {
                Ok(true) => println!("  已套用设定功耗墙：PL1 {w} W"),
                Ok(false) => eprintln!("  ! 设定功耗墙 {w} W 未获授权"),
                Err(e) => eprintln!("  ! 设定功耗墙失败：{e}"),
            }
        }
    }

    let mut last: Option<u16> = None; // None = 还没接管（固件控制中）
    // 已写进硬件的限制（cpu 上限, gpu 锁）—— 用来避免重复提权
    // (CPU 上限%, GPU 锁 MHz, PL1 功耗墙 W) —— 记录"我们实际写下去过什么"，
    // 收尾时只还原确实改过的项，避免无谓提权。
    let mut cur_lim: Option<(u32, Option<u32>, Option<u32>)> = None;
// PL1 基准：**第一次动手收紧之前**读到的真实值。
//
// ⚠ 不能用「启动时读的值」（原实现如此）。实测踩坑（2026-10-11）：
//   守护启动时 PL1=135 → 用户在 GUI 里手动设成 45 → 温度达阈值，守护按
//   `limit.cpu_w=30` 写 30 → 温度回落，收尾**写回 135** ⇒ 用户手动设的 45
//   被无声吃掉，界面又跳回 135，表现为「设置任意值都会自动跳回 135W」。
//
//   正确语义：守护只该还原**它自己覆盖掉的**那个值。手动设置优先于守护。
let mut base_pl1: Option<u32> = None;
    let mut bad = 0u32;
    let mut tick: u64 = 0;
    // CPU / GPU 各自的平滑态（上一次的输出值）
    let mut tc_f: Option<f64> = None;
    let mut tg_f: Option<f64> = None;
    let hb_every = if heartbeat == 0 || interval == 0 {
        0
    } else {
        (heartbeat / interval).max(1)
    };

    loop {
        let tcr = cpu_temp();
        let tgr = gpu_temp();

// ── 时间迟滞平滑：温度要稳定 τ 秒，其结果才被采纳（见 `smooth` 的文档）──
        let lag = cfg.fan_lag;
        let dt = interval.max(1) as f64;
        let tc = smooth(tc_f, tcr as f64, lag, dt) as u8;
        let tg = smooth(tg_f, tgr as f64, lag, dt) as u8;
        tc_f = Some(tc as f64);
        tg_f = Some(tg as f64);

        // 风扇**只由曲线决定** —— 用户明确要求：「当前的风扇策略已经够用了」。
        //
        // 这里原先还有一条「临界温度（95°C）无条件满速」的兜底，是当年
        // 没有别的降温手段时的**迫不得已**：靠风扇硬吹。
        // 现在有了温度限制策略（压 CPU 上限 / 锁 GPU 时钟，见 `limit.rs`）——
        // 那是更有效的降温手段，而且曲线本身在高温段就会给到满速
        // （曲线的末点即 4597 RPM，温度再高也是这个值）。
        // ⇒ 那条兜底已删除，避免它**绕过曲线**去覆盖用户设定的风扇策略。
        // ── CPU 与 GPU **各有一条曲线**，取两者中更激进的（更高转速）──
        //
        // 之前是 `cur.target_rpm(tc.max(tg))`：两条温度挤进同一条曲线，
        // 结果"GPU 热但 CPU 凉"时被 CPU 的低目标压下去，反之亦然。
        // 分开之后可以表达"GPU 更怕热就让它更早起飞"这类策略。
        //
        // 没单独配 GPU 曲线时 `gpu_curve_for` 会回落到 CPU 曲线 ⇒ 行为与旧版完全一致。
        let gpu_cur = cfg.gpu_curve_for(&key);
        // 注意：迟滞只作用于**温度**（上面的 `smooth`），**不碰曲线** ——
        // 曲线输出的转速就是最终写入值，用户编辑的形状与量级都原样保留。
        let cpu_rpm = cur.target_rpm(tc);
        let gpu_rpm = if tgr > 0 { gpu_cur.target_rpm(tg) } else { None };
        let target = match (cpu_rpm, gpu_rpm) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        match target {
            None => {
                if last.is_some() {
                    if !dry {
                        let _ = omen::set_fan_speed(None, 0);
                    }
                    println!("→ 交回固件（CPU {tc}°C / GPU {tg}°C）");
                    last = None;
                } else if verbose {
                    println!("  保持固件控制（CPU {tc}°C / GPU {tg}°C）");
                }
            }
            Some(rpm) => {
                let need = match last {
                    None => true,
                    Some(l) => (rpm as i32 - l as i32).unsigned_abs() as u16 >= step_rpm,
                };
                if need {
                    let pct = curve::rpm_to_pct(rpm);
                    if dry {
                        println!("[dry] 写 {pct}%（≈{rpm} RPM，目标）  CPU {tc}°C / GPU {tg}°C（原始 {tcr}/{tgr}）");
                    } else {
                        match omen::set_fan_speed(None, pct) {
                            Ok(_) => println!(
                                "→ 写 {pct}%（≈{rpm} RPM，目标）  CPU {tc}°C / GPU {tg}°C（原始 {tcr}/{tgr}）  实际 {:?}",
                                omen::fan_rpms()
                            ),
                            Err(e) => eprintln!("  ✗ 写入失败：{e}"),
                        }
                    }
                    last = Some(rpm);
                } else if verbose {
                    println!("  保持 {rpm} RPM（当前档位已足够）CPU {tc}°C / GPU {tg}°C");
                }

                // 自检：接管中转速异常 ⇒ 交回固件并退出
                if !dry {
                    if let Some(f0) = omen::fan_rpms().first().copied() {
                        if !(600..=5400).contains(&f0) {
                            bad += 1;
                            if bad >= 3 {
                                eprintln!("  ✗ 转速异常（fan0={f0}）连续 3 次 → 交回固件并退出");
                                break;
                            }
                        } else {
                            bad = 0;
                        }
                    }
                }
            }
        }

        // ── 温度限制：CPU 上限 / 睿频 / GPU 锁 ──
        //
        // 与风扇是两条独立通道：风扇写 `fan_speed`（plugdev，免提权），
        // 这里写的是 `root:root` 节点，**必须经 helper** —— 所以只在该动的时候动
        // （`Limiter` 的 confirm 已经保证「连续 N 次同向」才放行）。
        {
            // **每轮现读**：用户可能中途切档、也可能在面板上直接改「性能上限」。
            // 若沿用启动时的快照，守护会用旧基准把新值覆盖回去。
            //
            // ⚠⚠ 2026-10-11 实测血案：这里原先只取**档位**的 `cpu_pct`。
            //   用户在面板把「性能上限」拉到 50%，守护限流一次、温度回落后
            //   就把它写回档位的 100% —— journalctl 里那行
            //   `helper set-cpu-pct 100` 就是它，用户侧表现为
            //   "能砍的都砍了，温度照样冲 92°C"。
            //   而且写 max_perf_pct 会让 intel_pstate 重算各核 scaling_max_freq，
            //   用户设的「频率上限」也被一起抹掉。⇒ 基准必须认 `config.cpu_pct`。
            let base_now = hw::cpu_base_pct();
            // ⚠ 温度限制的判定**必须用实时温度**（tcr / tgr），不能用给风扇用的平滑值（tc / tg）。
            //
            // 实测（2026-10-11）：`fan_lag=long` 的时间常数是 22 秒，而 12900HX 睿频
            // 2 秒就能把封装推到 95+。拿平滑值做判定 ⇒ 限制会在真正高温之后**十几秒**
            // 才生效，这段时间温度照样冲到 99°C —— 正是用户报的"限了 30W 还是 99°C"。
            //
            // 分工：**风扇用平滑值是对的**（防抖、防反复启停）；**限制必须用实时值**
            // （保护要快，它能立刻压掉热源，不像风扇有 26 秒软启动惯性）。
            let g_opt = if tgr > 0 { Some(tgr) } else { None };
            if let limit::Step::Apply(action) = limiter.update(tcr, g_opt, base_now) {
                let pct = action.as_ref().and_then(|a| a.pct).unwrap_or(base_now);
                let glock = action.as_ref().and_then(|a| a.gpu_lock);
                let pl1 = action.as_ref().and_then(|a| a.pl1_w);
                let want = (pct, glock, pl1);
                if cur_lim != Some(want) {
                    if dry {
                        println!(
                            "[dry] 限制：CPU 上限 {pct}% · GPU {} · PL1 {}",
                            glock
                                .map(|m| format!("锁 {m} MHz"))
                                .unwrap_or_else(|| "不锁".into()),
                            pl1.map(|w| format!("{w} W"))
                                .unwrap_or_else(|| "不动".into())
                        );
                    } else {
                        let mut done: Vec<String> = Vec::new();
                        match hw::set_cpu_max_pct(pct) {
                            Ok(true) => done.push(format!("CPU {pct}%")),
                            Ok(false) => done.push("CPU 上限：未获授权".into()),
                            Err(e) => done.push(format!("CPU 上限失败：{e}")),
                        }
                        // PL1（封装功耗墙）：这是**从源头压热源**的手段。
                        // 本机 i9-12900HX 的睿频能在 2 秒内把某个核从 60°C 推到 96°C，
                        // 而风扇有 ~26 秒软启动惯性 ⇒ 追温度必然反复冲顶。
                        // 不设 `limit.cpu_w` 时这里是 None ⇒ 还原到动手前读到的基准。
                        // PL1：动手前**现读**一次真实值存进 `base_pl1`（只在第一次收紧时）。
                        // 这样用户在 GUI 手动设的值会被尊重 —— 收尾还原到"我们动手前看到的那个"，
                        // 而不是进程启动时的旧快照。
                        if base_pl1.is_none() {
                            base_pl1 = hw::rapl_w();
                        }
                        // 基准优先级：**用户在面板上设的稳定值**（config.cpu_pl1）
                        // → 我们动手前现读到的值（base_pl1）。
                        // 前者优先：用户中途改设定就能立刻被采纳，不必重启守护；
                        // 单靠 base_pl1 的话，它是"守护第一次收紧时"的快照，会过期。
                        let pl1_back = pl1
                            .or_else(|| hw::load_config().cpu_pl1)
                            .or(base_pl1);
                        match pl1_back {
                            Some(w) => match hw::set_rapl_w(w) {
                                Ok(true) => done.push(format!("PL1 {w} W")),
                                Ok(false) => done.push("PL1：未获授权".into()),
                                Err(e) => done.push(format!("PL1 失败：{e}")),
                            },
                            None => {}
                        }
                        let gspec = glock.map(|m| format!("{},{m}", limit::GPU_MIN_MHZ));
                        match hw::set_gpu_clk_lock(gspec.as_deref()) {
                            Ok(true) => done.push(
                                glock
                                    .map(|m| format!("GPU 锁 {m} MHz"))
                                    .unwrap_or_else(|| "GPU 解锁".into()),
                            ),
                            Ok(false) => done.push("GPU：未获授权".into()),
                            Err(e) => done.push(format!("GPU 失败：{e}")),
                        }
                        // 打印**判定用的实时温度**（与 decision 一致），不是风扇用的平滑值
                        println!(
                            "→ 限制：{}   （CPU {tcr}°C / GPU {tgr}°C）",
                            done.join(" · ")
                        );
                    }
                    cur_lim = Some(want);
                }
            }
        }

        tick += 1;
        // 心跳：稳定运行期间也定期出声，否则用户会以为卡死了（实测踩过）
        if hb_every > 0 && tick % hb_every == 0 {
            println!(
                "  [心跳 {}s] CPU {tc}°C / GPU {tg}°C → {}",
                tick * interval,
                match last {
                    None => "固件控制中".to_string(),
                    Some(r) => format!("{}%（≈{r} RPM）", curve::rpm_to_pct(r)),
                }
            );
        }

        if once {
            println!("  （--once：跑完一轮退出）");
            break;
        }

        // 分段 sleep，信号来了能及时退出
        for _ in 0..interval * 4 {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if STOP.load(Ordering::SeqCst) {
            break;
        }
    }

    println!("\n收尾：交回固件（写 0 + unthrottle 1→0）…");
    if !dry {
        let _ = omen::release_fans();
    }
    // 限制只在「我们确实改过」时才还原，避免无谓提权。
    // 还原到**档位基准**而不是"出厂值"：档位是用户的选择，不该被守护抹掉。
    if !dry && cur_lim.is_some() {
        // 还原到**现读的用户基准**（不是启动时的快照 —— 用户可能中途切过档、
        // 或直接在面板上改过「性能上限」）。同上面 `base_now` 的理由：只认档位
        // 会把用户手动设的上限抹掉。
        let back = hw::cpu_base_pct();
        let _ = hw::set_cpu_max_pct(back);
        // PL1 还原到**我们动手收紧之前读到的值**（`base_pl1` 在第一次
        // 写限制时才赋值，见上面）。这样用户在 GUI 里手动设的值会被尊重 ——
        // 实测踩坑：早前记的是「进程启动时」的值，于是用户手动设的 45 W
        // 被守护收尾时的 135 W 静默吃掉，界面表现为「设置任意值都跳回 135」。
        //
        // `base_pl1 == None` = 守护从未写过 PL1 ⇒ 这里什么都不做，
        // 不去动用户自己设的值。
        // 同样优先还到用户设的稳定值（config.cpu_pl1），没有才用动手前现读的那个。
        if base_pl1.is_some() {
            if let Some(w) = hw::load_config().cpu_pl1.or(base_pl1) {
                let _ = hw::set_rapl_w(w);
            }
        }
        let _ = hw::set_gpu_clk_lock(None);
        println!("已还原温度限制（CPU {back}% · GPU 未锁）");
    }
    println!("已交回。验收：等 60~70 秒后 fan0≈1990 / fan1≈1790 且 fan0 > fan1");
}

#[cfg(test)]
mod tests {
    use super::smooth;
    use crate::hw::FanLag;

    // 【已删除：`const REAL`（24 个实测温度点）】
    //   它是一次"用真实温度序列跑平滑"的测试留下的数据，那个测试后来被改写、
    //   数据没人读了 —— `cargo test` 一直报 `constant REAL is never used`。
    //   按本项目「不留死代码」的规则删掉（同一类清理见 README 的 2026-10-11 记录）。

    const DT: f64 = 2.0; // 守护默认采样间隔

    fn chain_with(input: &[f64], lag: FanLag) -> Vec<f64> {
        let mut st = None;
        input
            .iter()
            .map(|&raw| {
                let v = smooth(st, raw, lag, DT);
                st = Some(v);
                v
            })
            .collect()
    }

    /// 标准档 —— 绝大多数断言基于它。
    fn chain(input: &[f64]) -> Vec<f64> {
        chain_with(input, FanLag::Mid)
    }




    #[test]
    fn stable_for_constant_input() {
        // 恒定输入必须恒定（不能让平滑器自己漂）
        let v = chain(&[60.0; 10]);
        assert!(v.windows(2).all(|w| (w[1] - w[0]).abs() < 1e-9), "{v:?}");
    }

    #[test]
    fn converges_under_sustained_load() {
        // 持续 85°C 负载下最终必须收敛到 85 附近，不能永远差一截。
        let input: Vec<f64> = vec![40.0; 3].into_iter().chain(vec![85.0; 40]).collect();
        let v = chain(&input);
        let last = *v.last().unwrap();
        assert!(last > 80.0, "持续高温下收敛在 {last}，会漏保护");
        assert!(last <= 85.0, "不应超过真实读数: {last}");
    }

    /// 用户实测的抖动模式：在阈值附近来回摆，持续 30 拍（60 秒）。
    /// 三档迟滞的效果差别应该很大。
    const JITTER: [f64; 30] = {
        // 无 const fn 可用，手写：60/62 交替
        let mut a = [0.0f64; 30];
        let mut i = 0;
        while i < 30 {
            a[i] = if i % 2 == 0 { 60.0 } else { 62.0 };
            i += 1;
        }
        a
    };

    #[test]
    fn longer_lag_filters_more_jitter() {
        // 核心行为：**抖动幅度在滤波后被压缩得越多，迟滞越有效。**
        let spread = |lag: FanLag| {
            let v = chain_with(&JITTER, lag);
            let mx = v.iter().cloned().fold(f64::MIN, f64::max);
            let mn = v.iter().cloned().fold(f64::MAX, f64::min);
            mx - mn
        };
        let (long, mid, short) = (spread(FanLag::Long), spread(FanLag::Mid), spread(FanLag::Short));
        assert!(
            long < mid && mid < short,
            "迟滞与滤波强度应单调：long={long} mid={mid} short={short}"
        );
        // 原始抖动幅度 2°C；长迟滞应把它压到 1°C 以内
        assert!(long < 1.0, "长迟滞仍放大了抖动: {long}°C");
    }


    /// 安全通道：τ 再大也不能让风扇在真高温时反应太慢。
    #[test]
    fn rise_max_is_a_hard_ceiling_per_beat() {
        const RAMP: [f64; 30] = {
            let mut a = [0.0f64; 30];
            let mut i = 0;
            while i < 30 {
                a[i] = 60.0 + i as f64 * 1.3; // 一路爬到 ~97°C
                i += 1;
            }
            a
        };
        for lag in [FanLag::Long, FanLag::Mid, FanLag::Short] {
            let v = chain_with(&RAMP, lag);
            // 每一步都不得越过 rise_max
            for w in v.windows(2) {
                assert!(
                    (w[1] - w[0]).abs() <= lag.rise_max() + 1e-9,
                    "{} 档单步 {w:?} 超过 rise_max {}",
                    lag.key(),
                    lag.rise_max()
                );
            }
            // 且真高温必须跟得上（不能因为迟滞而漏保护）
            let last = *v.last().unwrap();
            assert!(
                last > 80.0,
                "{} 档只跟到 {last}°C ⇒ 漏保护",
                lag.key()
            );
        }
    }

    #[test]
    fn long_lag_reaches_target_slower_but_does_get_there() {
        // 长迟滞的正确代价只是"慢"，不是"永远到不了"。
        let step: Vec<f64> = vec![50.0; 3].into_iter().chain(vec![85.0; 60]).collect();
        let short = chain_with(&step, FanLag::Short);
        let long = chain_with(&step, FanLag::Long);
        let (l, sh) = (*long.last().unwrap(), *short.last().unwrap());
        assert!(
            l > sh - 6.0,
            "长迟滞比短迟滞差了太多：long={l} short={sh}"
        );
        assert!(l > 82.0);
    }

    #[test]
    fn first_sample_passes_through() {
        // 冷启动第一次采样就 97（睡眠唤醒可能如此）—— 不该假装是 48
        assert_eq!(smooth(None, 97.0, FanLag::Mid, DT), 97.0);
    }
}
