//! 底层硬件访问层。
//!
//! **安全边界**：本模块只访问内核导出的 sysfs 节点，
//! - 风扇读数: `/sys/devices/platform/hp-wmi/hwmon/hwmon*/fan*_input`
//! - 最大风扇: `/sys/devices/platform/hp-wmi/hwmon/hwmon*/pwm1_enable`（2=自动, 0=最大）
//! - CPU: `intel_pstate/{max_perf_pct,no_turbo}`、`cpufreq/{scaling_max_freq,energy_performance_preference}`
//! - CPU 功耗墙: `/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw`
//!
//! 全程**不读写 EC 寄存器**，因此不存在 omen-fan 那类"写 EC 可能损坏硬件"的风险。
//!
//! 唯一的写操作需要 root 权限，由 `escalate` 完成，两级策略：
//! 1. 已装免密 helper（`system/setup-nopasswd.sh`）→ `sudo -n` 调用它，**零弹窗**；
//! 2. 未装 → 回退 `pkexec`，弹一次图形密码框。
//! 温度读数全部来自 `/sys/class/hwmon`，只读，无需提权。
//!
//! # 实测后移除的两个功能（详见 README「实测结论」）
//!
//! 1. **ACPI 性能档（`platform_profile`）**：本机主板 `8A1A` 未被内核 hp-wmi 识别
//!    （`setup_active_thermal_profile_params()` 提前 return），切档位时内核并不下发到硬件。
//!    实测切换 `cool/quiet/balanced/performance` 后，EPP / 频率上限 / 睿频 / `max_perf_pct` /
//!    GPU 功耗墙**全部不变**，EC `0x95` 恒为 `0x30`。因此它在本机只是个软件标签，已从界面移除。
//! 2. **GPU 功耗墙（`nvidia-smi -pl`）**：命令返回 0 但**完全无效**——写 40 W 时 GPU 仍跑到
//!    80.2 W 峰值，`Current Power Limit` 三次读数（118.4/118.3/112.7）不跟随写入值。
//!    TGP 由平台 vBIOS 锁定（`Default 115 W` / `Max 175 W`，含 Dynamic Boost 动态浮动）。
//!    本模块改为**只读展示**，不再提供写入。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub const PLATFORM_PROFILE: &str = "/sys/firmware/acpi/platform_profile";
const HWMON_GLOB: &str = "/sys/devices/platform/hp-wmi/hwmon";

/// 专用白名单 helper 的候选路径（按优先级）。
///
/// 它只接受固定几个子命令，参数逐项字面量 / 范围校验（见 `system/omen-control-helper`）。
///
/// - **deb 包**装到 `/usr/libexec/omen-control/`（FHS 规范位置）；
/// - 历史的 `system/setup-nopasswd.sh` 装到 `/usr/local/libexec/`。
///
/// 两个都认、谁在就用谁 ⇒ "deb 已装但旧配置没清理"或"源码安装"的过渡期
/// 都不会丢免密通道。
pub const HELPER_CANDIDATES: [&str; 2] = [
    "/usr/libexec/omen-control/omen-control-helper",
    "/usr/local/libexec/omen-control-helper",
];

/// 当前实际生效的 helper 路径；都没有则 `None`（会回退 pkexec 弹密码框）。
pub fn helper_path() -> Option<&'static str> {
    HELPER_CANDIDATES.into_iter().find(|p| Path::new(p).is_file())
}

fn read_trim(path: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 当前内核上报的 ACPI 档位（**只读展示用**）。
///
/// 注意：本机该值不驱动任何硬件行为，仅作信息展示（见模块文档）。
pub fn current_profile() -> Option<String> {
    read_trim(PLATFORM_PROFILE)
}

/// 列出所有 hp-wmi 的 hwmon 目录（hwmonN 编号会随启动变化，不能硬编码）。
fn hwmon_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(HWMON_GLOB)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// 各风扇转速 (RPM)，按 fanN_input 排序。读不到返回空 Vec。
pub fn fan_rpms() -> Vec<u32> {
    let mut out = Vec::new();
    for dir in hwmon_dirs() {
        let mut files: Vec<PathBuf> = fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("fan") && n.ends_with("_input"))
                    .unwrap_or(false)
            })
            .collect();
        files.sort();
        for f in files {
            if let Some(rpm) = read_trim(&f.to_string_lossy()).and_then(|s| s.parse().ok()) {
                out.push(rpm);
            }
        }
    }
    out
}

/// 最大风扇状态：`Some(true)` = 强制最大（pwm1_enable=0），`Some(false)` = 自动（=2）。
pub fn max_fan() -> Option<bool> {
    let node = pwm_nodes().into_iter().next()?;
    match read_trim(&node.to_string_lossy())?.as_str() {
        "0" => Some(true),
        _ => Some(false),
    }
}

fn pwm_nodes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in hwmon_dirs() {
        for n in ["pwm1_enable", "pwm2_enable"] {
            let p = dir.join(n);
            if p.exists() {
                out.push(p);
            }
        }
    }
    out
}

/// 以 root 身份执行白名单操作。
///
/// 两级策略：
/// 1. 已装免密 helper → `sudo -n <helper> <args…>`，**不弹任何窗口**；
/// 2. 否则回退 `pkexec`（优先仍走 helper，helper 不存在才用旧的 `sh -c`）。
///
/// 返回 `Ok(true)` = 成功；`Ok(false)` = 未获授权（用户取消），不是错误。
fn escalate(args: &[&str], legacy_cmd: &str) -> io::Result<bool> {
    if let Some(helper) = helper_path() {
        // 1) 免密通道。sudo 未配 NOPASSWD 时会以非 0 退出，这里自然落到下一步。
        if let Ok((ok, msg)) = run_capture("sudo", &prepend(helper, args)) {
            if ok {
                return Ok(true);
            }
            // 失败要分清两种：
            //   · stderr 以 `sudo:` 开头 ⇒ 是 sudo 没放行（用户取消/未配免密），走授权流程；
            //   · 否则就是 helper 自己的报错（如"未安装 omen-thermal.service"），
            //     直接把它当错误返回，界面才能显示真正的原因。
            if !msg.is_empty() && !msg.starts_with("sudo:") && !msg.contains("sudo:") {
                return Err(io::Error::new(io::ErrorKind::Other, msg));
            }
        }
        // 2) 免密未就绪：用 pkexec 跑同一个 helper（弹密码框，但参数校验一致）
        if which("pkexec").is_some() {
            return run_status("pkexec", &prepend(helper, args));
        }
        return Ok(false);
    }

    // 没有 helper：只有带 legacy 命令的旧功能还能用
    if legacy_cmd.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "未安装免密组件（找过 {}）；deb 包自带，源码安装请执行 system/setup-nopasswd.sh",
                HELPER_CANDIDATES.join(" / ")
            ),
        ));
    }
    if which("pkexec").is_some() {
        return run_status("pkexec", &["sh", "-c", legacy_cmd]);
    }
    Ok(false)
}

/// 把 `prog` 与 `args` 拼成一个参数表，便于直接交给 `Command::args`。
fn prepend<'a>(prog: &'a str, args: &[&'a str]) -> Vec<&'a str> {
    let mut v = Vec::with_capacity(args.len() + 1);
    v.push(prog);
    v.extend_from_slice(args);
    v
}

fn which(prog: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(prog))
        .find(|p| p.is_file())
}

/// 运行外部命令，返回是否成功（不关心输出，也不需要区分失败原因）。
/// 运行命令并**保留 stderr** —— 用来把 helper 的真实失败原因带回界面。
///
/// 为什么不能只用 `run_status`：它把 stderr 丢进 /dev/null，于是无论
/// 「未安装 omen-thermal.service」还是「参数越界」，界面上一律显示"未获授权"，
/// 用户根本不知道哪里错了（2026-10-10 实测踩到）。
fn run_capture(prog: &str, args: &[&str]) -> io::Result<(bool, String)> {
    let out = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let msg = if !err.is_empty() {
        err
    } else {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    Ok((out.status.success(), msg))
}

fn run_status(prog: &str, args: &[&str]) -> io::Result<bool> {
    let status = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.success())
}

/// 运行外部命令并返回退出码（-1 表示被信号杀死/无法获取）。
fn run_code(prog: &str, args: &[&str]) -> io::Result<i32> {
    let status = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.code().unwrap_or(-1))
}

/// 免密通道是否已就绪。
///
/// 判据：helper 存在，且 `sudo -n <helper>`（不带参数）能被**真正执行**。
/// helper 无参数时以退出码 2 报用法；若 sudo 拿不到授权，它会以 1 退出且根本不执行 helper。
pub fn nopasswd_ready() -> bool {
    helper_path().is_some_and(|h| run_code("sudo", &["-n", h]).unwrap_or(-1) == 2)
}


// ---------------------------------------------------------------- 温度读数

/// 读第 `idx` 路温度（毫摄氏度 → 摄氏度，保留一位小数）。
fn read_temp(dir: &Path, idx: u32) -> Option<f64> {
    let raw = read_trim(&dir.join(format!("temp{idx}_input")).to_string_lossy())?;
    let milli: f64 = raw.parse().ok()?;
    if !(milli.is_finite()) || milli <= 0.0 {
        return None;
    }
    Some((milli / 1000.0 * 10.0).round() / 10.0)
}

/// 该目录下所有温度通道里的最高值（°C）。封装温度读不到时的兜底。
fn max_temp(dir: &Path) -> Option<f64> {
    let mut best: Option<f64> = None;
    for idx in 1..=64u32 {
        if let Some(v) = read_temp(dir, idx) {
            best = Some(best.map_or(v, |b: f64| b.max(v)));
        }
    }
    best
}

/// 按 label 关键字找一路温度（如 coretemp 的 "Package id 0"）。
fn find_temp_by_label(dir: &Path, needle: &str) -> Option<f64> {
    for idx in 1..=64u32 {
        let label = read_trim(&dir.join(format!("temp{idx}_label")).to_string_lossy());
        if let Some(l) = label {
            if l.contains(needle) {
                return read_temp(dir, idx);
            }
        }
    }
    None
}

/// 关键温度读数，单位 °C，只读 `/sys/class/hwmon`，无需提权。
///
/// 顺序：CPU 封装 → NVMe → ACPI 主板。读不到的项直接省略，不会报错。
pub fn temps() -> Vec<(&'static str, f64)> {
    let mut cpu: Option<f64> = None;
    let mut nvme: Option<f64> = None;
    let mut board: Option<f64> = None;

    let Ok(entries) = fs::read_dir("/sys/class/hwmon") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let name = read_trim(&dir.join("name").to_string_lossy()).unwrap_or_default();
        // 注意：内核在存在多个同类设备时会给 name 加序号后缀，
        // 本机实测就有 `acpitz_0`、`iwlwifi_1_2`，所以必须用前缀匹配而非相等。
        if name.starts_with("coretemp") || name.starts_with("k10temp") || name.starts_with("zenpower")
        {
            // 优先取**封装温度**（`Package id 0`）—— 它在物理上就是全芯片最热处，
            // 是风扇策略该依据的值。取不到才退到各核里的最高者。
            //
            // 不能反过来先读 temp1：不同内核/平台的通道编号不保证 1 就是封装
            // （本机 12900HX 上 temp1 恰好是，但那是巧合，不是约定）。
            cpu = cpu.or_else(|| find_temp_by_label(&dir, "Package"));
            cpu = cpu.or_else(|| max_temp(&dir));
        } else if name.starts_with("nvme") {
            nvme = nvme.or_else(|| read_temp(&dir, 1));
        } else if name.starts_with("acpitz") {
            board = board.or_else(|| read_temp(&dir, 1));
        }
    }

    let mut out = Vec::new();
    if let Some(v) = cpu {
        out.push(("CPU", v));
    }
    if let Some(v) = nvme {
        out.push(("NVMe", v));
    }
    if let Some(v) = board {
        out.push(("主板", v));
    }
    out
}

/// **显示用**温度：只对 CPU 那一路做指数平滑（EMA），其余原样。
///
/// ## 为什么需要
///
/// coretemp 的 `Package id 0` 是**瞬时值** —— 实测空闲态都会 54 → 80 → … 地跳，
/// 重载时更是 50↔99。直接显示给人的观感就是"温度跳来跳去、像坏了"，
/// 而用户对照的 Windows 侧工具显示的是平滑值，所以显得"平稳缓慢"。
///
/// ## 为什么只平滑显示、不平滑读数本身
///
/// **温度限制的判定必须用原始值**（见 `fan_daemon`）—— 平滑会带来秒级滞后，
/// 真高温时来不及压热源。因此这里另开一个入口专供界面，[`temps`] 保持原样，
/// 自检（`preflight`）也继续用原值。
///
/// 平滑系数 `α = 0.3`（约 3 秒时间常数）。主面板 1.2s、浮窗 1s 各刷一次，
/// 差异可忽略。状态是**每进程**的静态量，主面板与浮窗互不干扰。
pub fn temps_smooth() -> Vec<(&'static str, f64)> {
    const ALPHA: f64 = 0.3;
    static CPU_EMA: OnceLock<Mutex<Option<f64>>> = OnceLock::new();

    let ema = CPU_EMA.get_or_init(|| Mutex::new(None));
    temps()
        .into_iter()
        .map(|(k, v)| {
            if k != "CPU" {
                return (k, v);
            }
            let Ok(mut g) = ema.lock() else { return (k, v) };
            let sm = match *g {
                None => v,
                Some(p) => p + ALPHA * (v - p),
            };
            *g = Some(sm);
            (k, (sm * 10.0).round() / 10.0)
        })
        .collect()
}

/// 自检：能否访问这些节点。用于 `--check` 与 UI 启动提示。
pub fn preflight() -> Vec<(&'static str, bool, String)> {
    let mut v = Vec::new();
    // 只读展示：本机该值不驱动硬件（见模块文档），因此固定标记为「信息」
    v.push((
        "内核 ACPI 档位",
        true,
        match current_profile() {
            Some(p) => format!("{p}（只读信息；本机不驱动硬件）"),
            None => "读不到 platform_profile（hp-wmi 未加载？）".into(),
        },
    ));
    let fans = fan_rpms();
    v.push((
        "风扇读数",
        !fans.is_empty(),
        if fans.is_empty() {
            "无数据".into()
        } else {
            fans.iter()
                .enumerate()
                .map(|(i, r)| format!("fan{}={} RPM", i + 1, r))
                .collect::<Vec<_>>()
                .join("  ")
        },
    ));
    v.push((
        "最大风扇开关",
        !pwm_nodes().is_empty(),
        if pwm_nodes().is_empty() {
            "无 pwm 节点".into()
        } else {
            format!(
                "当前 {}",
                if max_fan() == Some(true) { "最大" } else { "自动" }
            )
        },
    ));
    let tlist = temps();
    v.push((
        "温度读数",
        !tlist.is_empty(),
        if tlist.is_empty() {
            "无数据".into()
        } else {
            tlist
                .iter()
                .map(|(k, t)| format!("{k} {t:.1}°C"))
                .collect::<Vec<_>>()
                .join("  ")
        },
    ));
    let ready = nopasswd_ready();
    v.push((
        "免密通道",
        ready,
        if ready {
            "已启用（改档位不再弹密码框）".into()
        } else {
            "未启用；执行 sudo bash system/setup-nopasswd.sh 可开启".to_string()
        },
    ));

    // ---- CPU 限制项
    let pct = cpu_max_pct().unwrap_or(100);
    let turbo = turbo_on().unwrap_or(true);
    let cap = cpu_cap_mhz().unwrap_or(0.0);
    v.push((
        "CPU 限制",
        true,
        format!(
            "上限 {pct}% · 睿频{} · 频率上限 {:.2} GHz",
            if turbo { "开" } else { "关" },
            cap / 1000.0
        ),
    ));
    if let Some(w) = rapl_w() {
        let p2 = rapl_pl2_w()
            .map(|x| format!("{x} W"))
            .unwrap_or_else(|| "读不到".into());
        // ⚠ 这条以前写的是"高于内核可写上限 → 写入会被钳到上限、界面已禁用" ——
        //   那是**基于误判的旧结论**，与现在的事实（写入不 clamp、控件可调、
        //   写后由 helper 读回校验）直接矛盾，会让用户以为控件坏了。已按实测重写。
        v.push((
            "CPU 功耗墙",
            true,
            format!(
                "PL1 {w} W · PL2 {p2}（可调 5~200 W，写入后由 helper 读回校验）。\
                 注：`constraint_0_max_power_uw` 报的 {} W 只是 BIOS 能力值、不是钳位上限；\
                 且只压 PL1 压不住睿频，必须连 PL2 一起压",
                rapl_max_w().unwrap_or(0)
            ),
        ));
    }

    // ---- GPU
    match gpu() {
        Some(g) => {
            let t = g
                .temp_c
                .map(|x| format!("{x}°C"))
                .unwrap_or_else(|| "—".into());
            let lim = g
                .power_limit_w
                .map(|x| format!("{x}W"))
                .unwrap_or_else(|| "—".into());
            let mx = g
                .power_max_w
                .map(|x| format!("{x}W"))
                .unwrap_or_else(|| "—".into());
            let draw = g
                .power_draw_w
                .map(|x| format!("{x:.1}W"))
                .unwrap_or_else(|| "—".into());
            v.push((
                "GPU",
                true,
                format!(
                    "{} · {t} · 实时 {draw} · 功耗墙 {lim} / 硬件上限 {mx}\
                     （功耗墙被 vBIOS 锁定 ⇒ -pl 无效；降温请用「锁定核心频率」-lgc）",
                    g.name
                ),
            ));
        }
        None => v.push(("GPU", false, "未检测到 nvidia-smi 或独显".into())),
    }

    // ---- 顶栏托盘：让它"可自证"（用户反馈过它玄学失灵）
    {
        let problem = tray_problem();
        let detail = match &problem {
            Some(why) => format!("⚠ {why}"),
            None => format!(
                "{} · 登录自启{}",
                match tray_pid() {
                    Some(p) => format!("运行中 PID {p}"),
                    None => "未运行".into(),
                },
                if tray_autostart_enabled() { "开" } else { "关" }
            ),
        };
        v.push(("顶栏托盘", problem.is_none(), detail));
    }

    v
}

/// `--check` 子命令输出（供脚本 / 排障用）。
pub fn print_check() {
    println!("=== OMEN 控制面板自检 ===");
    for (name, ok, detail) in preflight() {
        println!("{} {name}: {detail}", if ok { "[OK]" } else { "[!!]" });
    }
    match gpu_clk_lock() {
        Some((lo, hi)) => println!("[OK] GPU 时钟锁: 已锁定 {lo}–{hi} MHz（解锁：面板里的开关或 nvidia-smi -rgc）"),
        None => println!("[--] GPU 时钟锁: 未锁（上限不受限）"),
    }
}

// ============================================================ CPU 控制层
//
// 这一层是"真正能降温"的部分。实测：单靠 platform_profile 对本机毫无作用
// （主板 8A1A 未被内核识别），真正拉开差距的是下面这几个 intel_pstate / cpufreq 旋钮。
// 全部为内核官方接口，完全可逆，不碰 EC。

const PSTATE: &str = "/sys/devices/system/cpu/intel_pstate";
const CPU0_CPUFREQ: &str = "/sys/devices/system/cpu/cpu0/cpufreq";
const RAPL_PL1: &str = "/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw";
const RAPL_PL2: &str = "/sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw";
const RAPL_PL1_MAX: &str = "/sys/class/powercap/intel-rapl:0/constraint_0_max_power_uw";

/// CPU 最大性能百分比。这是限功耗最直接、最安全的旋钮。
pub fn cpu_max_pct() -> Option<u32> {
    read_trim(&format!("{PSTATE}/max_perf_pct"))?.parse().ok()
}

/// 内核允许的最小百分比（intel_pstate 的下限，本机为 17）。
pub fn cpu_min_pct() -> Option<u32> {
    read_trim(&format!("{PSTATE}/min_perf_pct"))?.parse().ok()
}

/// 设置 CPU 最大性能百分比。完全可逆，是"限功耗降温"的首选手段。
pub fn set_cpu_max_pct(pct: u32) -> io::Result<bool> {
    escalate(&["set-cpu-pct", &pct.to_string()], "")
}

/// 当前 EPP（能效偏好）。由档位预设统一设置，不单独暴露给界面。
pub fn epp() -> Option<String> {
    read_trim(&format!("{CPU0_CPUFREQ}/energy_performance_preference"))
}

pub fn set_epp(v: &str) -> io::Result<bool> {
    escalate(&["set-epp", v], "")
}

/// 睿频是否可用。
pub fn turbo_on() -> Option<bool> {
    Some(read_trim(&format!("{PSTATE}/no_turbo"))? == "0")
}

pub fn set_turbo(on: bool) -> io::Result<bool> {
    escalate(&["set-turbo", if on { "on" } else { "off" }], "")
}

/// CPU0 当前频率 (MHz)。
pub fn cpu_freq_mhz() -> Option<f64> {
    Some(read_trim(&format!("{CPU0_CPUFREQ}/scaling_cur_freq"))?.parse::<f64>().ok()? / 1000.0)
}

/// 硬件标称最高频率 (MHz)。
pub fn cpu_hw_max_mhz() -> Option<f64> {
    Some(read_trim(&format!("{CPU0_CPUFREQ}/cpuinfo_max_freq"))?.parse::<f64>().ok()? / 1000.0)
}

/// 当前生效的频率上限 (MHz)。
pub fn cpu_cap_mhz() -> Option<f64> {
    Some(read_trim(&format!("{CPU0_CPUFREQ}/scaling_max_freq"))?.parse::<f64>().ok()? / 1000.0)
}

/// 设置频率上限（MHz）。
pub fn set_cpu_cap_mhz(mhz: u32) -> io::Result<bool> {
    escalate(&["set-cpu-freq", &(mhz.saturating_mul(1000)).to_string()], "")
}

/// RAPL 长时功耗墙 PL1 的**当前值**（W）。
///
/// ⚠ 2026-10-11 实测更正 —— 此前这里写的两条都是**推测，且已被证伪**：
///
/// 1. 旧注释说"实际值高于可写上限，所以往 135 写会被钳回 55" —— **错**。
///    intel-rapl **写入并不 clamp**。`constraint_0_max_power_uw` 读 55 而同一时刻
///    `power_limit_uw` 实际是 135，本身就说明 55 只是 BIOS 报的能力值、不是钳位上限。
///    拿它当判据会把控件误禁成"PL1 锁死 55W 不可调"（用户实测踩到过）。
/// 2. 旧注释说"可能被 thermald / power-profiles-daemon 改回" —— **错**。
///    实测写 30 W 后连续观测 3 分钟纹丝不动，期间 thermald 一句日志都没有
///    （它的 rapl_controller 在本机根本没注册）。
///
/// ⇒ 它是个**写得进去、也留得住**的长期设置。真被覆盖时由 helper 的读回校验报出来。
pub fn rapl_w() -> Option<u32> {
    Some(read_trim(RAPL_PL1)?.parse::<u64>().ok()? as u32 / 1_000_000)
}

/// RAPL short_term（PL2）的当前值（W）。
///
/// **为什么要单独显示它**：只压 PL1 是不够的。PL1 是**约 56 秒窗口的平均值**
/// （`constraint_0_time_window_us` 是只读节点，改不了窗口长度），所以 CPU 仍可按
/// BIOS 的 PL2（本机 135 W）连续睿频几十秒 —— 实测只写 PL1=30 时前 24 秒稳在 84°C，
/// 到第 27 秒才掉到 58°C。**PL2 也压下去，"30 W"才是真的硬墙**（实测 2 秒内生效）。
pub fn rapl_pl2_w() -> Option<u32> {
    Some(read_trim(RAPL_PL2)?.parse::<u64>().ok()? as u32 / 1_000_000)
}

/// 内核声明的 PL1 能力值（W）。
///
/// ⚠ 这只是 BIOS 报的**能力值**，不是可写上限（见 [`rapl_w`] 的说明）。
/// 仅用于自检时作参考展示，**不要**拿它去判断可写性或禁用控件。
pub fn rapl_max_w() -> Option<u32> {
    Some(read_trim(RAPL_PL1_MAX)?.parse::<u64>().ok()? as u32 / 1_000_000)
}

/// 设置 CPU 功耗墙（W）—— **同时写 PL1（long_term）与 PL2（short_term）**。
///
/// 为什么必须两个都写：PL1 是约 56 秒窗口的平均，窗口节点只读改不了；
/// 只压 PL1 时 CPU 仍能按 PL2 的 135 W 连续睿频几十秒（实测前 24 秒等于没限）。
/// helper 写完会**读回校验**：被固件/服务覆盖时明确报错，而不是静默成功 ——
/// "选了不生效却显示成功"正是这个控件历史上最容易骗人的地方。
pub fn set_rapl_w(w: u32) -> io::Result<bool> {
    escalate(&["set-rapl", &w.to_string()], "")
}

// ================================================================ GPU 时钟锁
//
// **本机唯一实测有效的 GPU 降温杠杆。**
//
// ⚠ 更正本文件早先的结论：当时试了 `nvidia-smi -pl`（功耗墙）发现是空操作，
//   就把**整条 GPU 写路径**关掉了。那是过度保守 —— 锁 SM 时钟（`-lgc`）
//   是完全不同的机制，实测完全有效且可逆：
//       210,1500 → GPU 功耗 130.5 W 降到 57.4 W（−56%），71°C；`-rgc` 解锁
//   ⇒ 功耗墙走不通 ≠ GPU 管不了。

/// 读当前 GPU 时钟锁：取 `nvidia-smi -q -d CLOCK` 里的 `GPU Locked Clocks` 小节。
///
/// 返回 `Some((min_mhz, max_mhz))`。**未锁定时该小节不出现**，所以 `None` 是正常值。
pub fn gpu_clk_lock() -> Option<(u32, u32)> {
    if !gpu_present() {
        return None;
    }
    let out = Command::new(NVIDIA_SMI)
        .args(["-q", "-d", "CLOCK"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if !line.contains("Locked") || line.contains("N/A") {
            continue;
        }
        let nums: Vec<u32> = line
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<u32>().ok())
            .collect();
        if nums.len() >= 2 {
            return Some((nums[0], nums[1]));
        }
    }
    None
}

/// 锁定 / 解锁 GPU 核心时钟。`Some("210,1500")` 锁；`None` 解锁。
pub fn set_gpu_clk_lock(spec: Option<&str>) -> io::Result<bool> {
    match spec {
        Some(s) => escalate(&["set-gpu-clk", s], ""),
        None => escalate(&["set-gpu-clk", "off"], ""),
    }
}


/// 重启守护 —— **改过配置之后必须调它**，否则守护还拿着旧配置在跑
/// （`systemctl start` 对已在运行的服务是空操作，不会重载配置）。
///
/// 温度限制的阈值也是守护读的，所以改完温度同样要调它。
pub fn restart_fan_daemon() -> io::Result<bool> {
    escalate(&["set-fan-daemon", "restart"], "")
}

/// 开 / 关守护（`systemctl start|stop omen-fand`）。
///
/// 一个服务同时承载**风扇曲线**与**温度限制** —— 这是整合后的设计：
/// 两者由同一份温度驱动，但走不同通道（曲线写 `fan_speed`，限制经 helper），
/// 所以不会互相覆盖。
pub fn set_fan_daemon(on: bool) -> io::Result<bool> {
    escalate(&["set-fan-daemon", if on { "on" } else { "off" }], "")
}

/// 风扇曲线守护（`--fan-daemon`）是否正在运行。
///
/// 为什么扫 `/proc` 而不调 `systemctl is-active`：
/// ① 它在**每次界面刷新**时都会被调用，`systemctl` 一次要几十毫秒；
/// ② 用户可能手动 `nohup omen-control --fan-daemon &`，没装服务也要认出来。
/// 服务模式由 systemd 起的进程同样出现在 `/proc` 里，所以这一条覆盖两种情况。
pub fn fan_daemon_running() -> bool {
    let Ok(dir) = fs::read_dir("/proc") else {
        return false;
    };
    for ent in dir.flatten() {
        let name = ent.file_name();
        let Some(pid) = name.to_str() else { continue };
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(raw) = fs::read(ent.path().join("cmdline")) else {
            continue;
        };
        // ⚠ 必须锚定 **argv[0] 的文件名**，不能用"参数列表里出现过 omen-control"来判 ——
        //   2026-10-10 实测踩坑：一个正在执行 `grep --fan-daemon ...` 或
        //   `bash -c '... omen-control --fan-daemon ...'` 的进程，
        //   它的 cmdline **整段都是脚本文本**，两个字串都会命中 ⇒ 误判成"守护在跑"，
        //   表现为界面上的开关与遮罩自相矛盾。
        let mut args = raw.split(|b| *b == 0).filter(|a| !a.is_empty());
        let Some(argv0) = args.next() else { continue };
        let Ok(a0) = std::str::from_utf8(argv0) else {
            continue;
        };
        // 取路径最后一段：`/usr/local/bin/omen-control` 与 `./omen-control` 都要认。
        let base = a0.rsplit('/').next().unwrap_or(a0);
        if base != "omen-control" {
            continue;
        }
        for arg in args {
            if arg == b"--fan-daemon" {
                return true;
            }
        }
    }
    false
}

// ================================================================ GPU 层
//
// 读值是主路径；**写入口只有时钟锁**（`set_gpu_clk_lock`）。
// 功耗墙（`-pl`）在本机是空操作（vBIOS 锁定 TGP），因此不提供写入 —— 只展示。

/// GPU 状态。字段取不到时为 `None`（笔记本 GPU 的很多项驱动不给）。
#[derive(Clone, Debug, Default)]
pub struct Gpu {
    pub name: String,
    pub temp_c: Option<u32>,
    /// **当前生效的功耗墙**，取自 `nvidia-smi -q -d POWER` 的 `Current Power Limit`。
    ///
    /// ⚠ 这是**含 Dynamic Boost 的动态值**（实测空闲 115 W、有负载时 111~136 W 浮动），
    /// 不是"我们设置的值"——因为本机根本无法设置（见模块文档）。
    /// 另外两个字段同样不能用：`power.limit` 在笔记本上恒为 `[N/A]`；
    /// `Requested Power Limit` 也恒为 `N/A`。
    pub power_limit_w: Option<u32>,
    pub power_default_w: Option<u32>,
    pub power_max_w: Option<u32>,
    /// 实时功耗 (W)。
    pub power_draw_w: Option<f64>,
    pub util_pct: Option<u32>,
    pub mem_util_pct: Option<u32>,
    pub clock_sm_mhz: Option<u32>,
    pub clock_mem_mhz: Option<u32>,
    pub mem_used_mb: Option<u32>,
    pub mem_total_mb: Option<u32>,
    /// 性能状态，如 `P8`（空闲）～ `P0`（满载）。
    pub pstate: Option<String>,
    /// **当前生效的 GPU 时钟锁**（`Some((min,max))` = 锁着）；未锁 = `None`。
    ///
    /// 由后台采样线程一并取回，界面直接读缓存（nvidia-smi 不能在主线程跑）。
    pub clk_lock: Option<(u32, u32)>,
}

const NVIDIA_SMI: &str = "/usr/bin/nvidia-smi";

/// nvidia-smi 的 CSV 单元解析：`[N/A]`、`N/A`、空串都算缺值。
fn parse_num(s: &str) -> Option<u32> {
    let t = s.trim();
    if t.is_empty() || t.starts_with('[') || t.eq_ignore_ascii_case("n/a") {
        return None;
    }
    t.parse::<f64>().ok().map(|v| v.round() as u32)
}

fn parse_flt(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() || t.starts_with('[') || t.eq_ignore_ascii_case("n/a") {
        return None;
    }
    t.parse::<f64>().ok()
}

fn parse_txt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() || t.starts_with('[') || t.eq_ignore_ascii_case("n/a") {
        None
    } else {
        Some(t.to_string())
    }
}

/// 是否存在 NVIDIA GPU。
pub fn gpu_present() -> bool {
    Path::new(NVIDIA_SMI).is_file()
}

/// 从 `nvidia-smi -q -d CLOCK` 的文本里取 `GPU Locked Clocks`。
///
/// **未锁定时该小节根本不出现**，所以 `None` 是正常值（不是读失败）。
fn parse_locked_clocks(text: &str) -> Option<(u32, u32)> {
    for line in text.lines() {
        if !line.contains("Locked") || line.contains("N/A") {
            continue;
        }
        let nums: Vec<u32> = line
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<u32>().ok())
            .collect();
        if nums.len() >= 2 {
            return Some((nums[0], nums[1]));
        }
    }
    None
}

/// 一次 `nvidia-smi -q -d POWER,CLOCK` 同时取回：
/// 功耗墙**设置值**（`Current Power Limit`）与**时钟锁**。
///
/// 这两项都无法通过 `--query-gpu` 取得，必须单独调一次 `-q`；
/// 合并成一次调用是为了不让采样线程的耗时翻倍。
fn gpu_extra() -> (Option<u32>, Option<(u32, u32)>) {
    let out = match Command::new(NVIDIA_SMI)
        .args(["-q", "-d", "POWER,CLOCK"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return (None, None),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let limit = text
        .lines()
        .find(|l| l.trim_start().starts_with("Current Power Limit"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v.round() as u32);
    (limit, parse_locked_clocks(&text))
}

/// 启动一个常驻后台线程，每 `secs` 秒把 `gpu()` 的结果写进共享缓存。
///
/// `nvidia-smi` 一次查询约 60 ms，**绝不能放主线程**（会把界面拖卡）。
/// 线程以 `alive` 为存活标志；窗口关闭时把它置 `false` 即可让线程退出，
/// 否则进程会残留一个空转线程。
pub fn spawn_gpu_poller(alive: Arc<AtomicBool>, secs: u64) -> Arc<Mutex<Option<Gpu>>> {
    let cache: Arc<Mutex<Option<Gpu>>> = Arc::new(Mutex::new(gpu()));
    let c = Arc::clone(&cache);
    std::thread::spawn(move || {
        while alive.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(secs));
            if !alive.load(Ordering::Relaxed) {
                break;
            }
            if let Ok(mut slot) = c.lock() {
                *slot = gpu();
            }
        }
    });
    cache
}

/// 查询 GPU 状态。
///
/// 需要两次 nvidia-smi 调用（合计约 60 ms），因此**必须放在后台线程**——
/// 界面里由常驻采样线程负责，主线程只读缓存。
pub fn gpu() -> Option<Gpu> {
    if !gpu_present() {
        return None;
    }
    let out = Command::new(NVIDIA_SMI)
        .args([
            "--query-gpu=name,temperature.gpu,power.draw,power.default_limit,power.max_limit,\
             utilization.gpu,utilization.memory,clocks.sm,clocks.mem,memory.used,memory.total,pstate",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    if f.len() < 12 {
        return None;
    }
    // 先解析功耗上限，用于给 power.draw 做合理性校验
    let power_max_w = parse_num(f[4]);
    // 驱动偶发返回垃圾值（实测读到过 751.97 W，而物理上限只有 175 W）。
    // 超过上限本身即判为无效——宁可显示 "—" 也不能把假数据摆到界面上。
    let draw = parse_flt(f[2]).filter(|d| {
        d.is_finite() && *d >= 0.0 && power_max_w.map(|m| *d <= m as f64 + 1.0).unwrap_or(*d <= 300.0)
    });
    let (power_limit_w, clk_lock) = gpu_extra();
    Some(Gpu {
        name: f[0].to_string(),
        temp_c: parse_num(f[1]),
        power_draw_w: draw,
        power_default_w: parse_num(f[3]),
        power_max_w,
        util_pct: parse_num(f[5]),
        mem_util_pct: parse_num(f[6]),
        clock_sm_mhz: parse_num(f[7]),
        clock_mem_mhz: parse_num(f[8]),
        mem_used_mb: parse_num(f[9]),
        mem_total_mb: parse_num(f[10]),
        pstate: parse_txt(f[11]),
        power_limit_w,
        clk_lock,
    })
}

// ============================================================ 档位预设
//
// 一个档位 = 一组**同时生效、且本机实测有效**的参数。
// 刻意只用三个真正起作用的旋钮：max_perf_pct（主）、能效偏好 EPP、睿频开关。
// 不包含 platform_profile 与 GPU 功耗墙——那两项在本机实测无效（见模块文档）。

pub struct Preset {
    pub key: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    /// intel_pstate max_perf_pct —— 拉开档位差距的主力
    pub cpu_pct: u32,
    /// 能效偏好
    pub epp: &'static str,
    /// 睿频
    pub turbo: bool,
    /// 各核 scaling_max_freq (MHz) —— 档位的第二个真实维度
    pub cpu_cap_mhz: u32,
}

/// 频率上限的兜底值：读不到硬件上限时用 4900。
const CPU_HW_MAX_FALLBACK: u32 = 4900;

pub const PRESETS: [Preset; 4] = [
    Preset {
        key: "eco",
        label: "省电",
        hint: "CPU 限 40% · 睿频关 · 频率墙 2.4 GHz · EPP 省电优先 —— 最凉最安静",
        cpu_pct: 40,
        epp: "power",
        turbo: false,
        cpu_cap_mhz: 2400,
    },
    Preset {
        key: "quiet",
        label: "安静",
        hint: "CPU 限 65% · 频率墙 3.6 GHz · EPP 偏省电 —— 日常办公够用，风扇基本不吵",
        cpu_pct: 65,
        epp: "balance_power",
        turbo: true,
        cpu_cap_mhz: 3600,
    },
    Preset {
        key: "balanced",
        label: "均衡",
        hint: "CPU 不限 · 睿频开 · 频率墙放开 · EPP 偏性能 —— 出厂原始状态",
        cpu_pct: 100,
        epp: "balance_performance",
        turbo: true,
        cpu_cap_mhz: 0, // 0 = 不限，用硬件上限
    },
    Preset {
        key: "performance",
        label: "性能",
        hint: "CPU 不限 · 睿频开 · 频率墙放开 · EPP 性能优先 —— 与「均衡」的差别在持续负载下的频率稳定性",
        cpu_pct: 100,
        epp: "performance",
        turbo: true,
        cpu_cap_mhz: 0,
    },
];

pub fn preset(key: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.key == key)
}

/// **用户基准的 CPU 性能上限**（%）—— 界面与守护**唯一**该依据的那个值。
///
/// 优先级：用户在面板上设的基准（`config.cpu_pct`）
///         → 当前档位声明的上限（`preset.cpu_pct`）
///         → 100。
///
/// # 为什么必须由这里统一给（2026-10-11 实测血案）
///
/// 守护原先**只认档位**的 `cpu_pct`。于是用户在面板把「性能上限」拉到 50%，
/// 守护只要限流一次、温度回落后就写回档位的 100% ——
/// journalctl 里那行 `helper set-cpu-pct 100` 就是它，
/// 用户侧的感受是"我明明把能砍的都砍了，温度照样冲 92°C"。
///
/// 更隐蔽的是：写 `max_perf_pct` 会让 intel_pstate **重算各核 `scaling_max_freq`**，
/// 于是用户设的「频率上限」也一起被抹掉（实测：P 核全部回到 4900000 kHz）。
/// ⇒ 修好这一个来源，等于同时救回"性能上限"和"频率上限"两个设置。
pub fn cpu_base_pct() -> u32 {
    let c = load_config();
    c.cpu_pct
        .or_else(|| c.preset.as_deref().and_then(preset).map(|p| p.cpu_pct))
        .unwrap_or(100)
}

/// 预设的频率上限（0 表示不限 → 折算成硬件上限）。
pub fn preset_cap_mhz(p: &Preset) -> u32 {
    if p.cpu_cap_mhz == 0 {
        cpu_hw_max_mhz().unwrap_or(CPU_HW_MAX_FALLBACK as f64).round() as u32
    } else {
        p.cpu_cap_mhz
    }
}

/// 依次套用一组设置。返回 (成功项数, 失败说明列表)。
///
/// 注意：内部会发起多次提权调用，耗时约 100–200 ms，
/// 因此 UI 层必须放到后台线程执行，不要在主线程直接调。
pub fn apply_preset(p: &Preset) -> (usize, Vec<String>) {
    let mut ok = 0usize;
    let mut errs: Vec<String> = Vec::new();

    // ⚠⚠ 「CPU 上限」这一步用 [`cpu_base_pct`]，**不是** `p.cpu_pct`。
    //
    // 为什么（2026-10-11 实测血案之二）：`auto_apply=1` 会在**每次登录**跑
    // `omen-control --apply`。原先它无条件套用档位声明的 `cpu_pct`，于是
    // 用户在面板上单独设的「性能上限」（50%）每登录一次就被冲回 100% ——
    // 和守护的错误基准一起，构成了"我砍了但没用"的第二个来源。
    //
    // 面板切档位时会把 `cpu.pct` 同步成该档位的值，所以对"正常切档"这条路径
    // 取值与旧行为完全一致；只有"用户自定义得比档位更低"时才会不同（这正是要的）。
    let pct = cpu_base_pct();

    let mut steps: Vec<(&str, io::Result<bool>)> = vec![
        ("CPU 上限", set_cpu_max_pct(pct)),
        ("能效偏好", set_epp(p.epp)),
        ("睿频", set_turbo(p.turbo)),
        // 频率上限放最后：写 `max_perf_pct` 会让 intel_pstate 重算各核
        // `scaling_max_freq`，顺序反了就等于没写。
        ("频率上限", set_cpu_cap_mhz(preset_cap_mhz(p))),
    ];
    // 固定功耗墙（`cpu.pl1`）一并套用。不套的话**重启机器后 BIOS 会把它复位成
    // 135 W**，"钉住的墙"就没了 —— 而用户设它的目的正是要它长期在。
    // 没设过（`None`）就完全不动功耗墙。
    if let Some(w) = load_config().cpu_pl1 {
        steps.push(("功耗墙 PL1", set_rapl_w(w)));
    }
    for (name, r) in steps {
        match r {
            Ok(true) => ok += 1,
            Ok(false) => errs.push(format!("{name}：未获授权")),
            Err(e) => errs.push(format!("{name}：{e}")),
        }
    }

    (ok, errs)
}

/// 当前关键状态的一次性快照。
///
/// 界面每 1.2 s 要判断"当前是否匹配某个预设"，若对 4 个预设各调一次匹配函数，
/// 会重复读十几次 sysfs。改为先取一次快照、再逐个比对。
pub struct CurState {
    pub cpu_pct: Option<u32>,
    pub epp: Option<String>,
    pub turbo: Option<bool>,
    pub cpu_cap_mhz: Option<u32>,
}

impl CurState {
    /// 是否与某个预设完全一致。
    pub fn matches(&self, p: &Preset) -> bool {
        self.cpu_pct == Some(p.cpu_pct)
            && self.epp.as_deref() == Some(p.epp)
            && self.turbo == Some(p.turbo)
            && self.cpu_cap_mhz == Some(preset_cap_mhz(p))
    }
}

/// 读取一次当前状态快照。
pub fn cur_state() -> CurState {
    CurState {
        cpu_pct: cpu_max_pct(),
        epp: epp(),
        turbo: turbo_on(),
        cpu_cap_mhz: cpu_cap_mhz().map(|v| v.round() as u32),
    }
}

// ======================================================== 设置持久化
//
// 这些 sysfs 写入是**内核状态**：程序退出后依然生效，关机/重启后由 BIOS 复位。
// 所以"关掉面板会不会恢复"的答案是：**不会**——改动会留在系统里，直到重启。
// 要让设置在重启后也自动恢复，就得把选择存下来、开机时重新套用——就是下面这套。

/// 风扇**时间迟滞**档位：三档只改"多久才认一次温度变化"。
///
/// ## 为什么是时间而不是温度阈值
///
/// 防"风扇跳来跳去"的机制有两个，这里只用后者：
/// - **温度迟滞**（宽限带）：升温到 X 才动作，降到 X−N 才解除。
///   需要给每个曲线点单独配一个 N，配置复杂且和曲线形状耦合。
/// - **时间迟滞**（本实现）：温度要**连续稳定一小段时间**，其结果才被采纳。
///   一个参数管全部曲线点，实现简单、行为可预测。
///
/// 效果：CPU 在 60↔62°C 之间抖时，两档迟滞都判"还没稳定" ⇒ 风扇不动。
/// 温度真的一路爬到 90，哪怕一格一格地爬，累积到阈值时间也会跟上 —— 不会漏保护。
///
/// **与温度平滑的关系**：`fan_daemon::smooth()` 是同一件事的连续版本
/// （非对称一阶滤波）。本枚举只提供**参数**（时间常数与升温步进），
/// 具体滤波在守护里做。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FanLag {
    /// 安静：时间常数 22 秒。温度变化要持续约 22 秒才影响风扇 —— 最不容易抖。
    Long,
    /// 标准：时间常数 8 秒（≈ 采样 2s × 4 拍）。
    #[default]
    Mid,
    /// 灵敏：时间常数 3 秒。几乎实时跟随。
    Short,
}

impl FanLag {
    /// 一阶滤波的时间常数 τ（秒）。采样间隔 `dt` 下，
    /// 一步的权重 `α = 1 - exp(-dt/τ)`。
    pub fn tau_secs(self) -> f64 {
        match self {
            // ⚠ τ 的上界由**升温跟踪能力**决定，不是随便取的：
            //   实测 τ=40 时，跟随一路爬升的 97°C 只到 78°C（差 19°C）——
            //   对 i9-12900HX（PL1 135W，2 秒能从 60 冲到 96）来说太危险。
            //   22 秒是权衡点：抖动 60↔62 被压到 <0.5°C，而升温能跟到 90+。
            FanLag::Long => 22.0,
            FanLag::Mid => 8.0,
            FanLag::Short => 3.0,
        }
    }

    /// 单步升温上限（°C/拍）—— 迟滞再大也必须保留的安全通道。
    ///
    /// 没有它，τ=40 秒意味着封装冲到 97°C 时风扇要几十秒才全速，
    /// 那段时间足以让 CPU 进入降频/过热区间。
    pub fn rise_max(self) -> f64 {
        match self {
            FanLag::Long => 12.0,
            FanLag::Mid => 20.0,
            FanLag::Short => 30.0,
        }
    }

    /// 由采样间隔与时间常数算出一阶滤波系数。
    pub fn alpha(self, dt_secs: f64) -> f64 {
        let tau = self.tau_secs().max(0.1);
        1.0 - (-dt_secs / tau).exp()
    }

    pub fn key(self) -> &'static str {
        match self {
            FanLag::Long => "long",
            FanLag::Mid => "mid",
            FanLag::Short => "short",
        }
    }

    /// 解析配置里的 key；无法识别一律回落到 `Mid`（宁可标准，别让守护跑飞）。
    pub fn from_key(s: &str) -> Self {
        match s.trim() {
            "long" | "quiet" | "low" => FanLag::Long,
            "short" | "fast" | "high" => FanLag::Short,
            _ => FanLag::Mid,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FanLag::Long => "安静",
            FanLag::Mid => "标准",
            FanLag::Short => "灵敏",
        }
    }

    pub fn hint(self) -> String {
        format!(
            "温度变化要持续约 {:.0} 秒才影响风扇 —— 越久越不容易反复启停。             升温最多每拍 +{:.0}°C，真热了不会被迟滞拖住。",
            self.tau_secs(),
            self.rise_max()
        )
    }
}

/// 面板的持久化状态。
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// 用户最后选择的档位 key（对应 `PRESETS`）。
    pub preset: Option<String>,
    /// 登录时自动套用该档位。
    pub auto_apply: bool,
    /// 登录时自动显示监控浮窗。
    pub hud: bool,
    /// 退出/关闭面板时把系统恢复成启动时的样子。
    pub restore_on_exit: bool,
    /// 关闭主面板时隐藏到后台（而不是退出）。
    pub hide_on_close: bool,
    /// 用户设定的 **CPU 性能上限基准**（%）。`None` = 还没设过（界面取当前档位/硬件值）。
    ///
    /// # 为什么要有它（2026-10-11 用户实测反馈）
    ///
    /// 温度限制守护在高温时会把 `max_perf_pct` 收紧（如 100 → 62），温度回落再写回。
    /// 界面的滑块若**直接跟随 sysfs**，用户看到的就是"控件自己在 62/100 之间来回跳"，
    /// 既读不出"我设的是多少"，也没法稳定地改（每次改完 1.2 s 就被回写）。
    ///
    /// ⇒ 把两者拆开：**用户设定**存这里（稳定，只有用户能动），
    ///   **当前生效值**读 sysfs（会被守护改写，单独做只读展示，见 `ui.rs`）。
    pub cpu_pct: Option<u32>,
    /// 用户设定的 **PL1（长时功耗墙）基准**（W）。语义同 [`Config::cpu_pct`]。
    ///
    /// 特别重要：用户想做的正是"钉一个固定功耗墙把 CPU 压住"——
    /// 那条路必须先让这个滑块**能被稳定设定**。
    pub cpu_pl1: Option<u32>,
    /// 浮窗「有效读数」的不透明度（0.3 ~ 1.0）。
    pub hud_opacity_text: f64,
    /// 浮窗「窗口底色 + 框架 + 无效读数」的不透明度（0.15 ~ 1.0）。
    pub hud_opacity_chrome: f64,
    /// 每个预设各一条风扇曲线（key 与 `PRESETS` 一致）。
    ///
    /// 用 `Vec` 而不是 `HashMap`：只有 4 条、顺序稳定、写进配置文件时更好读。
    /// **显式存空曲线**表示"该预设不接管风扇"（区别于"还没配过"——后者取内置默认）。
    /// **CPU 温度 → 转速**曲线（key 与 `PRESETS` 一致）。
    ///
    /// 用 `Vec` 而不是 `HashMap`：只有 4 条、顺序稳定、写进配置文件时更好读。
    /// **显式存空曲线**表示"该预设不接管风扇"（区别于"还没配过"——后者取内置默认）。
    pub curves: Vec<(String, crate::curve::FanCurve)>,
    /// **GPU 温度 → 转速**曲线（可选）。
    ///
    /// 配置键是 `curve.<预设>.gpu`。**不配就用 CPU 那条**（行为与旧版一致），
    /// 这样老配置升级后不需要补一条 GPU 曲线。
    pub gpu_curves: Vec<(String, crate::curve::FanCurve)>,
    /// 风扇曲线守护是否启用（`--fan-daemon` 读它）。
    pub fan_daemon: bool,
    /// 风扇迟滞档位：`long`（安静）/ `mid`（标准）/ `short`（灵敏），缺省 `mid`。
    ///
    /// **只影响"多久才认一次温度变化"，不改动曲线本身** ——
    /// 用户编辑的曲线原样保留，切档只是换一组时间常数。
    pub fan_lag: FanLag,
    /// 温度限制策略 —— 在档位基准之上按温度自动收紧 CPU/GPU。
    ///
    /// 与 `curves` 的分工：曲线走 `fan_speed`（plugdev，免提权）；
    /// 这里的三项（CPU 上限 / 睿频 / GPU 锁）是 `root:root` 节点，必须经 helper。
    /// 两者由同一个守护读同一份温度，**各走各的通道，不会互相覆盖**。
    pub limit: crate::limit::LimitPolicy,
}

impl Config {
    /// 取某个预设的风扇曲线。
    ///
    /// 配置里**有**这个 key（哪怕是空的）就用它——空表示"显式不接管"；
    /// 配置里**没有**才回落到内置默认曲线，这样老配置文件升级后也有曲线可编辑。
    pub fn curve_for(&self, preset_key: &str) -> crate::curve::FanCurve {
        match self.curves.iter().find(|(k, _)| k == preset_key) {
            Some((_, c)) => c.clone(),
            None => crate::curve::default_curve(preset_key),
        }
    }

    /// 设置某个预设的 CPU 曲线（空曲线 = 不接管，会被如实写进配置文件）。
    pub fn set_curve(&mut self, preset_key: &str, c: crate::curve::FanCurve) {
        self.curves.retain(|(k, _)| k != preset_key);
        self.curves.push((preset_key.to_string(), c));
    }

    /// 取某个预设的 **GPU** 曲线。
    ///
    /// **没有单独配就回落到 CPU 那条** —— 这样"只想要一条曲线"的用法完全不受影响，
    /// 也保证老配置升级后行为不变。
    pub fn gpu_curve_for(&self, preset_key: &str) -> crate::curve::FanCurve {
        match self.gpu_curves.iter().find(|(k, _)| k == preset_key) {
            Some((_, c)) => c.clone(),
            None => self.curve_for(preset_key),
        }
    }

    /// 设置某个预设的 GPU 曲线。传**空曲线**表示"删掉这条，回落到 CPU 曲线"。
    pub fn set_gpu_curve(&mut self, preset_key: &str, c: crate::curve::FanCurve) {
        self.gpu_curves.retain(|(k, _)| k != preset_key);
        if !c.points.is_empty() {
            self.gpu_curves.push((preset_key.to_string(), c));
        }
    }

    /// 该预设**有没有单独配** GPU 曲线（UI 用它显示"独立/跟随 CPU"）。
    pub fn has_own_gpu_curve(&self, preset_key: &str) -> bool {
        self.gpu_curves.iter().any(|(k, _)| k == preset_key)
    }

    /// 有效读数不透明度；老配置里只有 `hud_opacity` 时用它兜底。
    fn text_opacity(&self) -> f64 {
        let v = if self.hud_opacity_text > 0.0 {
            self.hud_opacity_text
        } else {
            0.9
        };
        if v.is_finite() {
            v.clamp(0.3, 1.0)
        } else {
            0.9
        }
    }

    /// 窗口/框架/无效读数的不透明度。
    fn chrome_opacity(&self) -> f64 {
        let v = if self.hud_opacity_chrome > 0.0 {
            self.hud_opacity_chrome
        } else {
            0.45
        };
        if v.is_finite() {
            v.clamp(0.15, 1.0)
        } else {
            0.45
        }
    }
}

fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

fn config_dir() -> Option<PathBuf> {
    // `OMEN_CONFIG_DIR` 直接指定 omen-control 配置目录本身，优先级最高。
    //
    // 存在的理由：systemd **system** 服务里 `$HOME` 是 `/root`，
    // 光靠 XDG_CONFIG_HOME/HOME 推导会读到 `/root/.config/omen-control`，
    // 用户自己那条曲线就"消失"了。unit 里由安装脚本填入实际路径，例如
    // `Environment=OMEN_CONFIG_DIR=/home/<user>/.config/omen-control`。
    if let Some(d) = std::env::var_os("OMEN_CONFIG_DIR") {
        let p = PathBuf::from(d);
        if p.is_absolute() {
            return Some(p);
        }
    }
    Some(config_home()?.join("omen-control"))
}

pub fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config"))
}

/// 配置里是否**显式**存在某个键。
///
/// 用来区分"用户没配过"与"用户配成了 0/false"——新增配置项时，
/// 老配置文件里没有这个键，若直接读成 `false` 会让行为在升级后突然改变。
pub fn config_has_key(key: &str) -> bool {
    let Some(p) = config_path() else { return false };
    let Ok(text) = fs::read_to_string(&p) else { return false };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .any(|l| l.split_once('=').is_some_and(|(k, _)| k.trim() == key))
}

/// 读取配置。文件不存在或格式异常时返回默认值，**不报错**——配置文件坏了
/// 不该让程序起不来。
pub fn load_config() -> Config {
    let mut c = Config::default();
    let Some(p) = config_path() else { return c };
    let Ok(text) = fs::read_to_string(&p) else { return c };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "preset" => {
                c.preset = Some(v.to_string()).filter(|s| !s.is_empty());
            }
            "auto_apply" => c.auto_apply = matches!(v, "1" | "true" | "yes"),
            "hud" => c.hud = matches!(v, "1" | "true" | "yes"),
            "restore_on_exit" => c.restore_on_exit = matches!(v, "1" | "true" | "yes"),
            "hide_on_close" => c.hide_on_close = matches!(v, "1" | "true" | "yes"),
            // 用户设定的「基准」——高温限流守护**不会**碰这两个键（见 Config 字段说明）。
            // 越界一律当没设（宁可用界面推导值，也不要写一个会被 helper 拒绝的数）。
            "cpu.pct" => {
                c.cpu_pct = v.parse::<u32>().ok().filter(|p| (1..=100).contains(p));
            }
            "cpu.pl1" => {
                c.cpu_pl1 = v.parse::<u32>().ok().filter(|w| (5..=200).contains(w));
            }
            "hud_opacity" => c.hud_opacity_text = v.parse().unwrap_or(0.0),
            "hud_opacity_text" => c.hud_opacity_text = v.parse().unwrap_or(0.0),
            "hud_opacity_chrome" => c.hud_opacity_chrome = v.parse().unwrap_or(0.0),
            "fan_daemon" => c.fan_daemon = matches!(v, "1" | "true" | "yes"),
            "fan_lag" => c.fan_lag = FanLag::from_key(v),
            k => {
                // curve.<preset>=50:2000,60:2400,...        —— CPU 曲线
                // curve.<preset>.gpu=60:2200,72:3200,...    —— GPU 曲线（可选）
                // 解析失败就当空曲线（不接管）
                if let Some(key) = k.strip_prefix("curve.") {
                    let key = key.trim();
                    if !key.is_empty() {
                        let parsed = crate::curve::FanCurve::parse(v);
                        if let Some(base) = key.strip_suffix(".gpu") {
                            let base = base.trim();
                            if !base.is_empty() {
                                c.set_gpu_curve(base, parsed);
                            }
                        } else {
                            c.set_curve(key, parsed);
                        }
                    }
                }
                else if let Some(key) = k.strip_prefix("limit.") {
                    // limit.cpu / limit.gpu / limit.enabled / ... 交给策略自己解析，
                    // 认不出的键静默忽略（前向兼容）。
                    c.limit.set_key(key.trim(), v);
                }
            }
        }
    }
    c
}

/// 原子写入配置（先写临时文件再 rename，避免写一半被读到）。
pub fn save_config(c: &Config) -> io::Result<()> {
    let Some(dir) = config_dir() else {
        return Ok(());
    };
    fs::create_dir_all(&dir)?;
    let mut body = format!(
        "# OMEN 控制面板状态（由程序自动维护，可安全删除）\n\
         preset={}\n\
         auto_apply={}\n\
         hud={}\n\
         restore_on_exit={}\n\
         hide_on_close={}\n\
         hud_opacity_text={:.2}\n\
         hud_opacity_chrome={:.2}\n\
         cpu.pct={}\n\
         cpu.pl1={}\n",
        c.preset.clone().unwrap_or_default(),
        if c.auto_apply { 1 } else { 0 },
        if c.hud { 1 } else { 0 },
        if c.restore_on_exit { 1 } else { 0 },
        if c.hide_on_close { 1 } else { 0 },
        c.text_opacity(),
        c.chrome_opacity(),
        c.cpu_pct.map(|v| v.to_string()).unwrap_or_default(),
        c.cpu_pl1.map(|v| v.to_string()).unwrap_or_default(),
    );
    // 曲线按 PRESETS 顺序输出（稳定、可 diff）：
    // 每个预设先写 CPU 曲线，紧跟着它的 GPU 曲线（若单独配了），再补上其它 key
    for key in crate::curve::PRESET_KEYS {
        if let Some((_, cur)) = c.curves.iter().find(|(k, _)| k == key) {
            body.push_str(&format!("curve.{key}={}\n", cur.encode()));
        }
        if let Some((_, cur)) = c.gpu_curves.iter().find(|(k, _)| k == key) {
            body.push_str(&format!("curve.{key}.gpu={}\n", cur.encode()));
        }
    }
    for (key, cur) in &c.curves {
        if !crate::curve::PRESET_KEYS.contains(&key.as_str()) {
            body.push_str(&format!("curve.{key}={}\n", cur.encode()));
        }
    }
    for (key, cur) in &c.gpu_curves {
        if !crate::curve::PRESET_KEYS.contains(&key.as_str()) {
            body.push_str(&format!("curve.{key}.gpu={}\n", cur.encode()));
        }
    }
    body.push_str(&format!("fan_daemon={}\n", if c.fan_daemon { 1 } else { 0 }));
    body.push_str(&format!("fan_lag={}\n", c.fan_lag.key()));
    // 限制策略（顺序固定，便于 diff）
    {
        let mut kv: Vec<(String, String)> = Vec::new();
        c.limit.encode_into(&mut kv);
        for (k, v) in kv {
            body.push_str(&format!("{k}={v}\n"));
        }
    }
    let tmp = dir.join(".config.new");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, dir.join("config"))
}

// =============================================== 启动快照 / 退出还原
//
// 用户需求：**设置只在程序存活期间生效，关掉就清零**。
//
// 做法：启动时拍一份"我进来时系统是什么样"的快照；正常退出时把它写回去。
// 为了覆盖异常退出（崩溃 / SIGKILL / 断电），快照在启动时就落盘到
// `pending-restore`，只有**干净退出**才删除它——下次启动发现它还在，
// 就说明上次是被强杀的，先还原再继续。

/// 一次完整的状态快照。
///
/// # 为什么**不含 PL1（CPU 功耗墙）**
///
/// 用户明确要求「手动设的 PL1 要保持」—— 它是**长期功耗策略**，不是"用完要还原"的
/// 临时改动。退出面板时把它写回启动值没有安全意义，反而会造成
/// "设了 30 W、关掉面板又变回 135 W"。
///
/// （历史：早先这里记录过 PL1，但被一个 **55 W 的误判闸门**静默跳过还原，所以
/// "手动设的能保持"其实是歪打正着。那个闸门后来被认定是误判并删掉 —— 此时若仍留着
/// 这个字段就会真的开始还原，于是明确把 PL1 移出快照，把行为固定下来。
/// 2026-10-11 顺手删掉了字段与随之而来的三处死代码：还原分支、
/// `differs_from_now` 里的比较子句、以及 `to_text/from_text` 的序列化。）
///
/// 守护（`--fan-daemon`）自己改 PL1 时由它自己负责还原（它记 `base_pl1`），
/// 与本快照互不影响。
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub cpu_pct: Option<u32>,
    pub epp: Option<String>,
    pub turbo: Option<bool>,
    pub cpu_cap_mhz: Option<u32>,
}

impl Snapshot {
    /// 拍当前状态。
    pub fn capture() -> Self {
        Snapshot {
            cpu_pct: cpu_max_pct(),
            epp: epp(),
            turbo: turbo_on(),
            cpu_cap_mhz: cpu_cap_mhz().map(|v| v.round() as u32),
        }
    }

    /// 与当前状态相比是否有差异（用于决定退出时是否需要真的写回去）。
    pub fn differs_from_now(&self) -> bool {
        let cur = Snapshot::capture();
        cur.cpu_pct != self.cpu_pct
            || cur.epp != self.epp
            || cur.turbo != self.turbo
            || cur.cpu_cap_mhz != self.cpu_cap_mhz
    }

    /// 把系统写回快照状态。返回 (成功项数, 失败说明列表)。
    ///
    /// 约 4–6 次提权调用（200–400 ms），**必须放后台线程**。
    pub fn restore(&self) -> (usize, Vec<String>) {
        let mut ok = 0usize;
        let mut errs: Vec<String> = Vec::new();
        let mut step = |name: &str, r: io::Result<bool>| match r {
            Ok(true) => ok += 1,
            Ok(false) => errs.push(format!("{name}：未获授权")),
            Err(e) => errs.push(format!("{name}：{e}")),
        };
        if let Some(v) = self.cpu_pct {
            step("CPU 上限", set_cpu_max_pct(v));
        }
        if let Some(v) = self.turbo {
            step("睿频", set_turbo(v));
        }
        if let Some(v) = self.cpu_cap_mhz {
            step("频率上限", set_cpu_cap_mhz(v));
        }
        if let Some(v) = self.epp.as_deref() {
            step("能效偏好", set_epp(v));
        }
        // 注意：这里**故意不还原 PL1** —— 原因见 `Snapshot` 的文档
        // （用户要求手动设的功耗墙要长期保持，不属于"退出即还原"的范畴）。
        (ok, errs)
    }

    fn to_text(&self) -> String {
        format!(
            "cpu_pct={}\nepp={}\nturbo={}\ncpu_cap_mhz={}\n",
            self.cpu_pct.map(|v| v.to_string()).unwrap_or_default(),
            self.epp.clone().unwrap_or_default(),
            match self.turbo {
                Some(true) => "1",
                Some(false) => "0",
                None => "",
            },
            self.cpu_cap_mhz.map(|v| v.to_string()).unwrap_or_default(),
        )
    }

    fn from_text(text: &str) -> Self {
        let g = |k: &str| -> Option<String> {
            text.lines()
                .find_map(|l| l.trim().split_once('=').filter(|(a, _)| a.trim() == k).map(|(_, v)| v.trim().to_string()))
                .filter(|v| !v.is_empty())
        };
        let n = |k: &str| g(k).and_then(|v| v.parse::<u32>().ok());
        Snapshot {
            cpu_pct: n("cpu_pct"),
            epp: g("epp"),
            turbo: g("turbo").map(|v| v == "1"),
            cpu_cap_mhz: n("cpu_cap_mhz"),
            // 老版本落盘的 pending-restore 里可能还有 `rapl_w=` 一行：
            // 这里按键取值，多余的行会被自然忽略，不会解析失败。
        }
    }
}

fn pending_path() -> Option<PathBuf> {
    Some(config_dir()?.join("pending-restore"))
}

/// 启动时调用：把快照落盘（标记"本次运行尚未干净退出"）。
pub fn mark_pending(snap: &Snapshot) {
    if let Some(p) = pending_path() {
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(&p, snap.to_text());
    }
}

/// 干净退出时调用：删除标记。
pub fn clear_pending() {
    if let Some(p) = pending_path() {
        let _ = fs::remove_file(p);
    }
}

/// 启动时调用：若发现上次的 pending 标记仍在，说明上次是被强杀/崩溃的，
/// 此时把系统还原回去，避免"设置永远卡在受限状态"。
///
/// 返回 `Some(snapshot)` 表示确实做了一次崩溃恢复。
pub fn recover_if_pending() -> Option<Snapshot> {
    let p = pending_path()?;
    let text = fs::read_to_string(&p).ok()?;
    let snap = Snapshot::from_text(&text);
    let _ = snap.restore();
    let _ = fs::remove_file(&p);
    Some(snap)
}

/// 创建 / 删除浮窗的开机自启项。
///
/// 浮窗是**独立的 app id**，所以要有自己的 autostart 条目；由面板的开关实时增删，
/// 不需要重跑 `--install`。
pub fn set_hud_autostart(on: bool) -> io::Result<()> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(());
    };
    let dir = home.join(".config/autostart");
    let file = dir.join("omen-control-hud.desktop");
    if !on {
        let _ = fs::remove_file(&file);
        return Ok(());
    }
    fs::create_dir_all(&dir)?;
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("omen-control"));
    let entry = format!(
        "[Desktop Entry]\n\
         Name=OMEN 监控浮窗\n\
         Comment=CPU/GPU 温度、频率、风扇转速的小浮窗\n\
         Exec={} --hud\n\
         Icon=utilities-system-monitor\n\
         Terminal=false\n\
         Type=Application\n\
         Categories=Utility;System;\n",
        exe.display()
    );
    fs::write(&file, entry)
}

// ================================================================ 顶栏托盘
//
// 托盘（`omen-tray`）是**独立进程**，deb 用 `/etc/xdg/autostart/omen-control-tray.desktop`
// 在登录时拉起；面板里的这个开关只做两件**不需要 root** 的事：
//   · 起/停托盘进程（现在）
//   · 开/关"登录时自动启动"（将来）
//
// 为什么值得做成开关（2026-10-11 用户要求）：托盘此前会"玄学失灵" ——
// 进程被谁结束了、自启还在不在，界面上一概看不到，只能靠重新登录碰运气。
// 这里的取态原则是**只看事实**：进程在不在看 `/proc`，自启开没开看用户级覆盖文件。
//
// 关闭自启用的是 XDG 标准做法：在 `~/.config/autostart` 放一个**同名**的
// `Hidden=true` 条目去屏蔽 `/etc/xdg/autostart` 里的那条 —— 可逆、不需要 root、
// 而且"下次登录也不启动"是系统自己保证的（不是我们记在私有配置里）。

/// 托盘脚本的实际路径：deb 装到 `/usr/libexec/omen-control/`，源码安装到 `~/.local/bin`。
pub fn tray_path() -> Option<PathBuf> {
    let sys = PathBuf::from("/usr/libexec/omen-control/omen-tray");
    if sys.is_file() {
        return Some(sys);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let user = home.join(".local/bin/omen-tray");
    if user.is_file() {
        Some(user)
    } else {
        None
    }
}

fn base_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// 正在运行的托盘进程 PID。
///
/// ⚠ 与 [`fan_daemon_running`] 同一条教训：**必须锚定 argv[0] 的文件名**，不能只
/// "某个参数里有 omen-tray 就算" —— 实测一个正在跑
/// `pgrep -af /usr/libexec/omen-control/omen-tray` 或面板自己命令行里带这个路径的进程，
/// 都会被误判成"托盘在跑"，开关就会显示错的勾选状态。
/// 这里允许两种形态：shebang 直跑（argv[0] 结尾是 omen-tray），或
/// `python3 …/omen-tray`（argv[0] 是 python，脚本在后面的参数里）。
pub fn tray_pid() -> Option<u32> {
    let dir = fs::read_dir("/proc").ok()?;
    for ent in dir.flatten() {
        let name = ent.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(raw) = fs::read(ent.path().join("cmdline")) else {
            continue;
        };
        let mut args = raw.split(|b| *b == 0).filter(|a| !a.is_empty());
        let Some(argv0) = args.next() else { continue };
        let Some(a0) = std::str::from_utf8(argv0).ok().map(base_name) else {
            continue;
        };
        let hit = a0 == "omen-tray"
            || (a0.starts_with("python")
                && args
                    .filter_map(|a| std::str::from_utf8(a).ok())
                    .any(|a| base_name(a) == "omen-tray"));
        if hit {
            return Some(pid);
        }
    }
    None
}

/// 用户级的自启"覆盖"文件（存在且 `Hidden=true` ⇒ 关闭了系统那条自启）。
fn tray_autostart_override() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".config/autostart/omen-control-tray.desktop"))
}

/// 「登录时自动启动托盘」是否启用（没写覆盖文件 = 用系统那条 = 启用）。
pub fn tray_autostart_enabled() -> bool {
    let Some(p) = tray_autostart_override() else {
        return true;
    };
    match fs::read_to_string(&p) {
        Ok(text) => !text
            .lines()
            .any(|l| l.trim().eq_ignore_ascii_case("Hidden=true")),
        // 读不到（不存在/没权限）⇒ 当作没覆盖 = 启用
        Err(_) => true,
    }
}

/// 开 / 关「登录时自动启动托盘」。
pub fn set_tray_autostart(on: bool) -> io::Result<()> {
    let Some(p) = tray_autostart_override() else {
        return Ok(());
    };
    if on {
        // 删掉覆盖文件 ⇒ /etc/xdg/autostart 里那条重新生效
        match fs::remove_file(&p) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    } else {
        if let Some(d) = p.parent() {
            fs::create_dir_all(d)?;
        }
        // Hidden=true 是 XDG 规定的"屏蔽同名系统条目"的方式（不是"删掉它"）。
        fs::write(
            &p,
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=OMEN 托盘\n\
             Comment=由 OMEN 控制面板关闭（把这个文件删掉即可恢复登录自启）\n\
             Exec=/usr/libexec/omen-control/omen-tray\n\
             Icon=omen-control\n\
             Hidden=true\n",
        )
    }
}

/// 立即启动托盘。
///
/// **必须 `setsid` 脱离本进程的会话**：否则它是面板的子进程，面板一退出/被杀，
/// 托盘可能跟着走 —— 那就又变回"托盘莫名其妙没了"。
pub fn start_tray() -> io::Result<u32> {
    let p = tray_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "找不到托盘脚本 omen-tray（deb 装在 /usr/libexec/omen-control/）",
        )
    })?;
    let mut cmd = Command::new(&p);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // pre_exec 在 fork 之后、exec 之前跑；setsid 让子进程自成会话。
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    Ok(child.id())
}

/// 立即结束托盘进程（SIGTERM；它是 Python 脚本、没有信号处理，会正常退出）。
pub fn stop_tray() -> io::Result<()> {
    if let Some(pid) = tray_pid() {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    Ok(())
}

/// 托盘"该跑却跑不起来"时给出的**可执行**原因；一切正常则返回 `None`。
///
/// 只在面板启动时探一次（要读几个 typelib 路径，别放进 1.2s 的刷新里）。
pub fn tray_problem() -> Option<String> {
    if tray_path().is_none() {
        return Some("找不到托盘脚本（deb 包应装在 /usr/libexec/omen-control/）".into());
    }
    // GNOME 下托盘靠 appindicator，缺任一个 typelib 都会"图标不出现"且没有报错 ——
    // 这正是它过去显得"玄学"的原因之一，所以这里直接说出来。
    let missing = ["Gtk-3.0", "AyatanaAppIndicator3-0.1"]
        .into_iter()
        .filter(|name| !typelib_exists(name))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "缺 Python 绑定：{}（sudo apt install python3-gi gir1.2-gtk-3.0 gir1.2-ayatanaappindicator3-0.1）",
            missing.join(" / ")
        ))
    }
}

/// GIR typelib 是否存在（路径带架构三元组，所以按目录扫一遍）。
fn typelib_exists(name: &str) -> bool {
    let Ok(entries) = fs::read_dir("/usr/lib") else {
        return false;
    };
    for ent in entries.flatten() {
        let dir = ent.path().join("girepository-1.0");
        if dir.join(format!("{name}.typelib")).is_file() {
            return true;
        }
    }
    false
}

/// 应用图标（SVG，**编译期嵌进二进制**）。
///
/// 内嵌而非运行时读源码目录：`--install` 可能从任意路径的可执行文件被调用
/// （例如 deb 装在 `/usr/bin/omen-control`），那时源码树不一定还在。
/// 图标只有一个来源 `packaging/omen-control.svg`，源码安装与 deb 装的是同一张图。
const ICON_SVG: &str = include_str!("../packaging/omen-control.svg");

/// 托盘脚本（Python），同样**编译期嵌入**。
///
/// 与图标同理：`--install` 可能从任意路径调用，源码树不一定在。
/// 源码安装把它写到 `~/.local/bin/omen-tray`；deb 版由包直接装到
/// `/usr/libexec/omen-control/omen-tray`（两条路径不会同时存在，见 `postinst`）。
const TRAY_PY: &str = include_str!("../packaging/omen-tray");

/// 判断当前进程是不是"**包安装**"的，是则返回原因（`None` = 不是）。
///
/// 三条判据命中其一即算（不依赖 dpkg 一定可用）：
/// 1. 可执行文件本身就在 `/usr/bin` 或 `/usr/libexec` —— 那是包的地盘；
/// 2. `dpkg -S` 认领它（最权威）；
/// 3. `/usr/bin/omen-control` 已存在 —— 系统里已经有一份"正统"安装。
fn packaged_install_reason(exe: &Path) -> Option<String> {
    if exe.starts_with("/usr/bin") || exe.starts_with("/usr/libexec") {
        return Some(format!("它自己就在 {}", exe.display()));
    }
    if which("dpkg").is_some() {
        if let Ok(o) = Command::new("dpkg")
            .args(["-S", &exe.to_string_lossy()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            if o.status.success() {
                let who = String::from_utf8_lossy(&o.stdout).trim().to_string();
                return Some(format!("dpkg 认出它属于：{who}"));
            }
        }
    }
    let sys = Path::new("/usr/bin/omen-control");
    if sys.exists() && sys != exe {
        return Some(format!("系统里已存在 {}", sys.display()));
    }
    None
}

/// 把可执行文件装进 ~/.local/bin + 应用菜单 + 开机自启（**源码安装专用**）。
///
/// # 为什么现在要拒绝"包安装"的情况
///
/// 本函数会在 `~/.local/bin` 再放一份二进制，而 `~/.local/bin` 在 PATH 里
/// **优先于** `/usr/bin`。于是刚因为 deb 而变"唯一"的二进制又被拆成两份，
/// "菜单/终端点开的是哪一份、是不是旧货"这个折磨了整轮的问题会立刻复活。
///
/// 本项目已经有一个更可靠的部署方式（deb：版本号由 dpkg 管、`--version` 自证身份），
/// 所以这里直接拦下来并指路，而不是"允许你再造一份副本"。
pub fn self_install(exe: &Path) -> io::Result<()> {
    if let Some(why) = packaged_install_reason(exe) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "本程序是**包安装**的（{why}），已拒绝 --install 自安装。\n\
                 原因：自安装会在 ~/.local/bin 再造一份副本，而那目录优先于 /usr/bin，\n\
                 \"到底跑的是哪一份\"会重新变成谜（本项目为此吃过多轮亏）。\n\
                 升级请改用：bash build.sh --deb && sudo apt install ./dist/omen-control_*.deb"
            ),
        ));
    }
    let home = std::env::var("HOME")
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("无法定位 HOME: {e}")))?;
    let bin = Path::new(&home).join(".local/bin");
    let apps = Path::new(&home).join(".local/share/applications");
    let autostart = Path::new(&home).join(".config/autostart");
    for d in [&bin, &apps, &autostart] {
        fs::create_dir_all(d)?;
    }
    let dest = bin.join("omen-control");
    // 原子替换：先写临时文件，再 rename 覆盖。
    // 直接用 fs::copy 覆盖正在运行的可执行文件会得到 ETXTBSY (os error 26)；
    // rename 是原子的，Linux 允许覆盖运行中的文件（旧 inode 留给运行中的进程）。
    let tmp = bin.join(".omen-control.new");
    fs::copy(exe, &tmp)?;
    if let Err(e) = fs::rename(&tmp, &dest) {
        // 极端情况（跨文件系统等）退回直接复制
        let r = fs::copy(exe, &dest);
        let _ = fs::remove_file(&tmp);
        r.map_err(|_| e)?;
    }

    // 图标：装进用户级 hicolor 主题目录，名字与 .desktop 的 Icon= 一致。
    // 用了 `include_str!`（编译期嵌入）而不是运行时读源码目录 ——
    // `--install` 可能从任意路径的可执行文件被调用，那时源码树不一定在。
    let icons = Path::new(&home).join(".local/share/icons/hicolor/scalable/apps");
    fs::create_dir_all(&icons)?;
    fs::write(icons.join("omen-control.svg"), ICON_SVG)?;

    // 托盘：写进 ~/.local/bin（与二进制同目录），再为它写一条用户级自启条目。
    // 注意它是 Python，不是这个二进制 —— 所以常规的"哪份二进制在跑"陷阱对它不适用，
    // 但仍遵守"只装一份"：deb 版装的是 /usr/libexec/omen-control/omen-tray。
    {
        use std::os::unix::fs::PermissionsExt;
        let tray = bin.join("omen-tray");
        fs::write(&tray, TRAY_PY)?;
        fs::set_permissions(&tray, std::fs::Permissions::from_mode(0o755))?;
        let tray_entry = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=OMEN 托盘\n\
             Comment=OMEN 档位 / 风扇守护的托盘开关\n\
             Exec={}\n\
             Icon=omen-control\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            tray.display()
        );
        fs::write(autostart.join("omen-control-tray.desktop"), tray_entry)?;
        println!("[OK] 托盘脚本  : {}", tray.display());
        println!(
            "[OK] 托盘自启  : {}",
            autostart.join("omen-control-tray.desktop").display()
        );
        // 托盘依赖不在 cargo 里（deb 版由 Depends 保证），源码安装这里确认一次。
        let deps_ok = Command::new("python3")
            .args([
                "-c",
                "import gi; gi.require_version('AyatanaAppIndicator3','0.1'); \
                 from gi.repository import AyatanaAppIndicator3",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if deps_ok {
            println!("[OK] 托盘依赖  : python3-gi + AyatanaAppIndicator3 就绪");
        } else {
            println!(
                "[提示] 托盘依赖缺失，托盘不会显示。安装：\n       \
                 sudo apt install python3-gi gir1.2-gtk-3.0 gir1.2-ayatanaappindicator3-0.1"
            );
        }
    }

    let entry = format!(
        "[Desktop Entry]\n\
         Name=OMEN 控制面板\n\
         Comment=HP OMEN 性能档 / 风扇曲线 / 温度限制\n\
         Exec={}\n\
         Icon=omen-control\n\
         StartupWMClass=org.rosary.OmenControl\n\
         Terminal=false\n\
         Type=Application\n\
         Categories=Utility;System;Settings;\n",
        dest.display()
    );
    for dir in [&apps, &autostart] {
        fs::write(dir.join("omen-control.desktop"), &entry)?;
    }
    println!("[OK] 可执行文件: {}", dest.display());
    println!("[OK] 图标      : {}", icons.join("omen-control.svg").display());
    println!("[OK] 应用菜单  : {}", apps.join("omen-control.desktop").display());
    println!("[OK] 开机自启  : {}", autostart.join("omen-control.desktop").display());
    println!("以后在应用菜单搜「OMEN」即可打开；下次登录会自动启动。");
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::FanLag;

    /// 「设定值」`cpu.pct` / `cpu.pl1` 必须能原样落盘并读回。
    ///
    /// 为什么值得单独测：这两个键是**滑块上那个"不会跳的值"的唯一来源** ——
    /// 一旦读写不一致，用户设的值就会在下次刷新时被 config 里的旧值覆盖，
    /// 表现成"我设了 45 W，界面又自己变回去"（正是这次要修的那类问题）。
    ///
    /// 用 `OMEN_CONFIG_DIR` 指到临时目录，**绝不碰用户真实配置**
    /// （已确认没有别的测试读配置，所以这样设环境变量不会与它们竞争）。
    #[test]
    fn cpu_baseline_roundtrips_through_config_file() {
        let dir = std::env::temp_dir().join(format!("omen-cfg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("OMEN_CONFIG_DIR", &dir);

        let mut c = super::load_config();
        c.preset = Some("balanced".into());
        c.cpu_pct = Some(80);
        c.cpu_pl1 = Some(45);
        super::save_config(&c).unwrap();

        let back = super::load_config();
        assert_eq!(back.preset.as_deref(), Some("balanced"));
        assert_eq!(back.cpu_pct, Some(80), "设定上限必须原样读回");
        assert_eq!(back.cpu_pl1, Some(45), "设定功耗墙必须原样读回");
        assert!(super::config_has_key("cpu.pct"));
        assert!(super::config_has_key("cpu.pl1"));

        // 用户设的基准优先于档位声明 —— 守护与界面都靠这条链取值（上面设的 preset 是 balanced=100）
        assert_eq!(super::cpu_base_pct(), 80);

        // 没设过基准 ⇒ 落到档位声明的上限（eco=40）
        std::fs::write(super::config_path().unwrap(), "preset=eco\n").unwrap();
        assert_eq!(super::cpu_base_pct(), 40);

        // 手写越界值 ⇒ 当"没设"，绝不能把一个会被 helper 拒绝的数带去写硬件
        std::fs::write(super::config_path().unwrap(), "cpu.pct=150\ncpu.pl1=1\n").unwrap();
        let bad = super::load_config();
        assert_eq!(bad.cpu_pct, None);
        assert_eq!(bad.cpu_pl1, None);
        assert_eq!(super::cpu_base_pct(), 100, "基准非法 ⇒ 落到 100，而不是 150");

        std::env::remove_var("OMEN_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn alpha_matches_tau() {
        // α = 1 - exp(-dt/τ)：τ 越大越平滑（α 越小）
        let a = |l: FanLag, dt: f64| l.alpha(dt);
        assert!(a(FanLag::Long, 2.0) < a(FanLag::Mid, 2.0));
        assert!(a(FanLag::Mid, 2.0) < a(FanLag::Short, 2.0));
        // 数值抽查：τ=8, dt=2 ⇒ 1-exp(-0.25) ≈ 0.2212
        let m = a(FanLag::Mid, 2.0);
        assert!((m - 0.2212).abs() < 0.001, "α 算错: {m}");
    }

    #[test]
    fn long_lag_still_protects_against_heat() {
        // 关键性质：迟滞再大，升温步进上限也必须能让风扇在**几十秒内**跟上。
        // 否则用户选"安静"就等于关掉了高温保护 —— 那不是安静，那是危险。
        for l in [FanLag::Long, FanLag::Mid, FanLag::Short] {
            let rise = l.rise_max();
            assert!(rise >= 10.0, "{} 档升温上限 {rise}°C/拍 太慢", l.key());
            // 从 60 冲到 97（37°C）所需拍数，按 2s 一拍算
            let beats = (37.0 / rise).ceil();
            assert!(
                beats * 2.0 <= 30.0,
                "{} 档要 {} 秒才跟得上 97°C，太慢",
                l.key(),
                beats * 2.0
            );
        }
    }

    #[test]
    fn key_roundtrip_and_bad_key_falls_back() {
        for l in [FanLag::Long, FanLag::Mid, FanLag::Short] {
            assert_eq!(FanLag::from_key(l.key()), l);
        }
        assert_eq!(FanLag::from_key("garbage"), FanLag::Mid);
        assert_eq!(FanLag::from_key(""), FanLag::Mid);
    }

    #[test]
    fn alpha_is_stable_for_degenerate_tau() {
        // dt=0 不该产生 NaN/inf（守护 interval 允许 0 时会走到这里）
        assert!(FanLag::Long.alpha(0.0).is_finite());
        assert!(FanLag::Short.alpha(0.0).abs() < 1e-12);
    }
}
