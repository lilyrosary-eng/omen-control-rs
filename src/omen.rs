//! # OMEN 私有 WMI 接入层 —— 对接 `hp_wmi_omen` 内核模块
//!
//! 这层**不读 EC、不写 EC、不碰任何寄存器**，全部走我们自己的内核模块暴露的
//! sysfs 节点。那条路径是：用户态 → `wmi_evaluate_method()` → ACPI → HP 固件。
//!
//! ## 为什么不走内核的 `platform_profile`
//!
//! 实测结论（见 `hw.rs` 顶部注释）：本机主板 `8A1A` 未被内核 `hp-wmi` 识别
//! （`setup_active_thermal_profile_params()` 提前 return），所以
//! `/sys/firmware/acpi/platform_profile` 切档时**内核并不下发到硬件** ——
//! EPP / 频率上限 / 睿频 / `max_perf_pct` / GPU 功耗墙全部不变，
//! EC `0x95` 恒为 `0x30`。它在��机只是个软件标签。
//!
//! 我们的模块**绕过了 `platform_profile`**，直接给固件发 GM 命令 `0x1A`，
//! 固件返回 `rc=0`，且风扇转速有可观测变化（`fan1: 0 → 1808`）。
//! ⇒ 这条路径是真实生效的，不是标签。
//!
//! ## ⚠ 别把这条读成"与 README 的档位结论矛盾"
//!
//! 两者说的是**两条完全不同的通道**，并不冲突：
//!
//! | | 通道 | 实测结果 |
//! | :- | :- | :- |
//! | README 的"ACPI 档位零影响" | `/sys/firmware/acpi/platform_profile` → 内核 `hp-wmi` | ❌ 本机主板 `8A1A` 不在内核的板子表里，`setup_active_thermal_profile_params()` 提前 return ⇒ 切档**不下发硬件** |
//! | 本模块的档位 | `performance_mode` 节点 → WMI `GM 0x1A` → HP 固件 | ✅ `固件 rc=0`，且风扇转速有可观测变化 |
//!
//! 一句话：**"内核那条路没接上"不等于"硬件没有这个能力"** ——
//! 这与风扇曲线的情形完全一样（内核 `hp-wmi` 只有两态，而 `GM12` 能连续调速）。
//!
//! 即便如此，**档位仍没进 UI**：它的实际收益（降温 / 降噪）尚未量化，
//! 按项目铁律 1，没做过"写 → 读回 → 带负载验证"的东西不给开关。
//! 想手工试：`omen-control --omen set performance|default|cool`。
//!
//! ## 提权
//!
//! 内核模块已把可写接口设为 **0664** 并通过 udev 规则把属组交给 `plugdev`
//! （桌面登录用户都在该组内）⇒ **本层所有写操作都不需要 sudo**。
//! 这与本项目其它功能（走 `omen-control-helper` + pkexec）不同，
//! 是本模块刻意设计的免提权通道。
//!
//! ## 平台可用性
//!
//! 模块未加载时本层全部函数返回 `None`/`Err`，GUI 据此隐藏相关控件，
//! **不显示"点了没反应"的开关**（项目铁律 1）。

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// 内核模块注册的 platform device 路径。
///
/// 这里**不用通配符**：`hwmonN` 的编号会随启动变化，必须遍历匹配，
/// 但 platform device 的名字由 `platform_device_register_simple()` 固定，
/// 所以是确定路径。
pub const BASE: &str = "/sys/devices/platform/omen-wmi";

/// 热管理档位。注意与 `hw.rs` 里那个空转的 `platform_profile` 区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// 平衡 / 默认（v1 值 `0x30`）
    Balanced,
    /// 性能 / turbo（v1 值 `0x31`）
    Performance,
    /// 冷静 / quiet（v1 值 `0x50`）
    Cool,
}

impl Profile {
    /// sysfs `performance_mode` 接受的字符串（由 [`set_profile_verified`] 使用）。
    ///
    /// ⚠ UI 里**没有**档位选择器（档位的实际收益尚未量化，见 README 实测结论），
    /// 所以这条路径目前只有 CLI `omen-control --omen set <档位>` 会走。
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Balanced => "default",
            Profile::Performance => "performance",
            Profile::Cool => "cool",
        }
    }

    /// 界面上显示的名字。
    pub fn label(self) -> &'static str {
        match self {
            Profile::Balanced => "平衡",
            Profile::Performance => "性能",
            Profile::Cool => "冷静",
        }
    }

    pub const ALL: [Profile; 3] = [Profile::Balanced, Profile::Performance, Profile::Cool];
}

/// 模块是否已加载（sysfs 目录存在）。
pub fn available() -> bool {
    Path::new(BASE).join("profile_version").exists()
}

/// 读一个只读属性，失败返回 `None`。
fn read_node(name: &str) -> Option<String> {
    fs::read_to_string(Path::new(BASE).join(name))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 写一个 write-only 属性。
///
/// **不走 helper、不提权** —— 内核侧已按 0664 + plugdev 属组授权。
///
/// ⚠ 必须用 `write_all` 写裸字节，**不要用 `writeln!` 带占位符**。
///   实测：`writeln!(f, "{val}")` 会让内核 store 返回 `EINVAL`（os error 22），
///   而 `writeln!(f, "performance")` 与 `f.write_all(b"performance\n")` 都成功。
///   sysfs 的 `store` 回调对写入字节**极其敏感**，任何多余的格式处理都会改变内容，
///   导致内核侧的字符串解析失败。裸字节是唯一可靠的写法。
fn write_node(name: &str, val: &str) -> io::Result<bool> {
    let p = PathBuf::from(BASE).join(name);
    let mut f = fs::OpenOptions::new().write(true).open(&p)?;
    // ⚠⚠ 必须**一次性**写完（含换行），绝不能分两次 write_all。
    //
    // 实测踩过：`f.write_all(b"performance")` 后再 `f.write_all(b"\n")`
    // 会让内核 store 返回 EINVAL（os error 22）—— 因为 sysfs/kernfs 的
    // `store` 回调是**每次 write 系统调用触发一次**，而不是 close 时才触发。
    // 于是第一次 write 送进去 `"performance"`（无换行）能被 sscanf 接受，
    // 紧接着第二次 write 送进去 `"\n"` ⇒ 解析不出任何词 ⇒ 内核返回 -EINVAL，
    // **这个错误覆盖了上一次成功的结果**。
    //
    // shell 的 `echo x > f` 之所以没事：单次 write(2) 就把整行发出去了。
    let mut payload = String::with_capacity(val.len() + 1);
    payload.push_str(val);
    payload.push('\n');
    f.write_all(payload.as_bytes())?;
    Ok(true)
}

// ───────────────────────────────────────────────── 档位值表（只读）

/// 本机该用哪套档位值。
///
/// 内核模块从固件 `0x28`（design data）的 `buffer[3]` 读出 thermal policy
/// version，v0/v1 两套值不同，写错那套固件不认。模块内部已按此自动映射，
/// 这里只是把结论读出来给界面显示。
#[derive(Debug, Clone, Copy)]
pub struct ProfileTable {
    /// true = v1（`default=0x30 performance=0x31 cool=0x50`），false = v0。
    pub is_v1: bool,
    /// 固件报告的原始 version 字节。
    pub version: u8,
}

pub fn profile_table() -> Option<ProfileTable> {
    let s = read_node("profile_version")?;
    let is_v1 = s.starts_with("v1 ");
    // 输出形如：
    //   "v1 default=0x30 performance=0x31 cool=0x50 (design_data[3]=0x01)"
    //
    // ⚠ 字符串里有 **5 个** "0x"，不能用 `split("0x").nth(1)` ——
    //   那会拿到 `default=0x30` 里的 30（= 0x30 = 48），
    //   而真正的 version 在**最后一个** 0x 后面（design_data[3]=0x01 → 1）。
    //   实测踩过：version 被读成 48 而非 1。
    //   ⇒ 必须按 `design_data[3]=0x` 这个精确锚点解析。
    let version = s
        .split_once("design_data[3]=0x")
        .and_then(|(_, tail)| tail.get(..2))
        .and_then(|h| u8::from_str_radix(h, 16).ok())
        .unwrap_or(if is_v1 { 1 } else { 0 });
    Some(ProfileTable { is_v1, version })
}

/// 该档位在这台机器上对应的原始字节值（用于界面显示 / 日志）。
pub fn profile_value(p: Profile) -> Option<u8> {
    let t = profile_table()?;
    Some(match (t.is_v1, p) {
        (true, Profile::Balanced) => 0x30,
        (true, Profile::Performance) => 0x31,
        (true, Profile::Cool) => 0x50,
        (false, Profile::Balanced) => 0x00,
        (false, Profile::Performance) => 0x01,
        (false, Profile::Cool) => 0x02,
    })
}

// ───────────────────────────────────────────────── 读取（免 sudo）

/// 风扇数量。
pub fn fan_count() -> Option<u32> {
    read_node("fan_count")?.parse().ok()
}

/// 各风扇转速（RPM），顺序即风扇序号。
///
/// 内核模块已修���一个坑：这条 WMI 命令**必须传风扇索引**，
/// 返回值取字节 `[2][3]`（大端 u16），且**不乘任何系数**。
/// 实测与我方读数与 `hp-wmi` hwmon 的 `fan*_input` 逐个数字完全一致。
pub fn fan_rpms() -> Vec<u32> {
    let Some(s) = read_node("fan_rpm") else {
        return Vec::new();
    };
    // 格式： "fan0: 1790\nfan1: 0"
    s.lines()
        .filter_map(|l| {
            let v = l.rsplit(':').next()?.trim();
            v.parse::<u32>().ok()
        })
        .collect()
}

/// 是否已解除风扇限速。
pub fn fan_unthrottled() -> Option<bool> {
    read_node("fan_unthrottle")?.parse::<u32>().ok().map(|v| v != 0)
}

/// GPU 功耗模式（`ctgp` / `ppab` / `dstate` / `slowdown`）。
///
/// 原始输出：`ctgp=0 ppab=1 dstate=1 slowdown=0`。
/// 注意：本机 TGP 由 vBIOS 锁定，**写入可能无效**（见 README 实测结论），
/// 因此本模块默认只读展示，不在界面上提供开关。
pub fn gpu_modes() -> Option<(u32, u32, u32, u32)> {
    let s = read_node("gpu_modes")?;
    let get = |k: &str| -> Option<u32> {
        s.split_whitespace()
            .find_map(|t| t.strip_prefix(&format!("{k}=")))?
            .parse()
            .ok()
    };
    Some((get("ctgp")?, get("ppab")?, get("dstate")?, get("slowdown")?))
}

/// 原始设计数据（8 字节十六进制），调试用。
pub fn design_data() -> Option<String> {
    read_node("design_data")
}

// ───────────────────────────────────────────────── 写入（免 sudo）

/// 解除 / 恢复风扇限速。
///
/// 对应 GM 命令 `0x27`（读是 `0x26`）。注意这**不是**"设转速"，
/// 而是固件层面允许风扇跑满（上游用它实现 Fn+C 临时全速）。
pub fn set_fan_unthrottle(on: bool) -> io::Result<bool> {
    write_node("fan_unthrottle", if on { "1" } else { "0" })
}

/// 按百分比设置风扇转速（WMI 命令 `GM12`，见 DSDT 反编译）。
///
/// - `sel`：`Some(0)` 只设风扇1、`Some(1)` 只设风扇2、`None` 两个都设
/// - `pct`：**`0` = 交回固件**（不是"0 转速"）；`1..=100` = 手动百分比
///
/// # 为什么禁止 `0xFF`
///
/// 实测（2026-10-10）：EC 把 `SFS = 255` 当作**「最大速度」**，风扇会被钉死满速，
/// 而且 DSDT 里**没有任何 ASL 路径**能把 `SFS1/SFS2` 设回自动
/// （`GM12` 的 `0xFF/0xFF` 分支只清 `FFFS`，根本不动 SFS）。
/// 当初就是照 DSDT 猜"0xFF 是 release"导致用户遇到「风扇满速不回落」。
/// 真正的"交回固件"值是 **0**。这里直接拒绝，免得再踩。
pub fn set_fan_speed(sel: Option<u8>, pct: u8) -> io::Result<bool> {
    if pct == 0xFF {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "0xFF 会被 EC 当作「最大速度」且无 ASL 退路，已拒绝；交回固件请传 0",
        ));
    }
    if pct > 100 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("百分比越界：{pct}（有效 0..=100）"),
        ));
    }
    let val = match sel {
        Some(0) => format!("0 {pct}"),
        Some(1) => format!("1 {pct}"),
        Some(other) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("风扇编号越界：{other}（只支持 0 或 1，或 None 表示两个都设）"),
            ))
        }
        None => format!("{pct}"),
    };
    write_node("fan_speed", &val)
}

/// 把两个风扇都交回固件。
///
/// 实测有效的**两步**（缺一不可，见 `system/README.md`）：
/// ① 写 `fan_speed = 0` 清 `SFS1/SFS2`；② `unthrottle` 写 `1` 再写 `0` 触发 EC 重算曲线。
/// 只做 ① 时转速下降很慢，只做 ② 时只能恢复 fan0。
pub fn release_fans() -> io::Result<()> {
    let _ = write_node("fan_speed", "0");
    let _ = write_node("fan_unthrottle", "1");
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let _ = write_node("fan_unthrottle", "0");
    Ok(())
}

// ───────────────────────────────────────────────── 写后验证（项目铁律 1）

/// 写档位后的验证：**不能只看写入成功**，必须给出可观测证据。
///
/// 项目的硬规则是"每个可操作项都必须做过 写 → 读回 → 带负载验证"，
/// 否则就不该在界面上提供开关。这里给出写后观察到的状态，供 GUI 展示与判断。
#[derive(Debug, Clone)]
pub struct ApplyReport {
    /// 写入后读到的风扇转速。
    pub rpm_after: Vec<u32>,
    /// 说明文字。
    pub note: String,
}

/// 切档并返回验证信息。
///
/// ⚠ **档位本身读不回来**（内核模块拿不到固件当前档位 —— 那需要
/// `hp-wmi` 内部的 EC 通道 `0x95`，依赖未导出的私有结构体，
/// 有意没做）。所以验证依据是**固件返回码（内核 dmesg）+ 转速变化**。
pub fn set_profile_verified(p: Profile) -> io::Result<ApplyReport> {
    let before = fan_rpms();
    write_node("performance_mode", p.as_str())?;
    // 给固件一点时间让风扇曲线响应
    std::thread::sleep(std::time::Duration::from_millis(300));
    let after = fan_rpms();

    let delta = match (before.first(), after.first()) {
        (Some(a), Some(b)) => Some(*b as i64 - *a as i64),
        _ => None,
    };
    let note = match delta {
        Some(d) if d.abs() >= 50 => format!(
            "{}档已下发；风扇 0 转速变化 {d:+} RPM",
            p.label(),
        ),
        Some(_) => format!(
            "{}档已下发；风扇转速暂未明显变化（空闲状态下可能不明显，\
             需带负载才能观察）",
            p.label()
        ),
        None => format!("{}档已下发；但读不到转速，无法验证效果", p.label()),
    };

    Ok(ApplyReport {
        rpm_after: after,
        note,
    })
}