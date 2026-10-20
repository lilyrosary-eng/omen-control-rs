//! HP OMEN Linux 安全控制面板
//!
//! 通过内核官方接口控制 CPU 性能上限 / 睿频 / 频率上限 / 能效偏好 / CPU 功耗墙，
//! 并提供风扇转速、CPU/GPU 温度与频率的实时监视。**不读写 EC 寄存器**，无硬件风险。
//!
//! 子命令：
//!   （无）        启动图形面板
//!   --hud         启动监控浮窗（独立进程，可单独设开机自启）
//!   --check       打印硬件接口自检结果
//!   --reset       恢复出厂设置（套用「均衡」预设）
//!   --apply       套用配置里保存的档位（供开机自启使用）
//!   --set-preset <key>  直接切到某个档位（供托盘等非图形调用）
//!   --install     装进 ~/.local/bin + 应用菜单 + 开机自启
//!   --version     打印正在运行的这份二进制的身份（版本 / 构建时间 / 路径）
//!   --help        显示帮助

mod curve;
mod curve_editor;
mod fan_daemon;
mod hw;
mod hud;
mod limit;
mod omen;
mod ui;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = std::env::current_exe().unwrap_or_default();

    match args.first().map(String::as_str) {
        Some("--hud") => hud::run(),
        Some("--fan-daemon") => {
            let rest: Vec<String> = args.iter().skip(1).cloned().collect();
            fan_daemon::run(&rest);
        }
        Some("--check") => {
            hw::print_check();
        }
        Some("--reset") => {
            // 「恢复出厂」必须**连用户设定的基准一起清掉**：`apply_preset` 现在优先
            // 采用 `cpu.pct` / `cpu.pl1`（见 `hw::apply_preset`），不清的话
            // "恢复出厂"会把用户的自定义值又原样套回来 —— 那就名不副实了。
            let mut cfg = hw::load_config();
            if cfg.cpu_pct.is_some() || cfg.cpu_pl1.is_some() {
                cfg.cpu_pct = None;
                cfg.cpu_pl1 = None;
                let _ = hw::save_config(&cfg);
                println!("已清除自定义的「性能上限 / 功耗墙」设定值。");
            }
            let (ok, errs) = match hw::preset("balanced") {
                Some(p) => hw::apply_preset(p),
                None => (0, vec!["找不到内置预设".to_string()]),
            };
            println!("已套用「均衡」预设：{ok} 项成功，{} 项失败", errs.len());
            for e in errs {
                eprintln!("  [失败] {e}");
            }
        }
        Some("--apply") => {
            let cfg = hw::load_config();
            match cfg.preset.as_deref().and_then(hw::preset) {
                Some(p) => {
                    let (ok, errs) = hw::apply_preset(p);
                    println!("已套用「{}」预设：{ok} 项成功，{} 项失败", p.label, errs.len());
                    for e in errs {
                        eprintln!("  [失败] {e}");
                    }
                }
                None => {
                    println!("配置里没有保存的档位，未做改动。");
                }
            }
        }
        Some("--set-preset") => {
            // 托盘（omen-tray）与脚本用的入口：**一条命令切档位**，
            // 行为与面板里点那个档位按钮完全一致 —— 先记住选择、再套用，
            // 且"当前状态已经匹配"时跳过重复提权（面板里也是这么做的）。
            let Some(key) = args.get(1).map(String::as_str) else {
                eprintln!("用法: omen-control --set-preset <eco|quiet|balanced|performance>");
                std::process::exit(1);
            };
            let Some(p) = hw::preset(key) else {
                eprintln!("未知档位 {key}（可选 eco/quiet/balanced/performance）");
                std::process::exit(1);
            };
            // 记住用户的选择（供「登录时自动套用」用）。即使当前状态已匹配也要记。
            let mut cfg = hw::load_config();
            if cfg.preset.as_deref() != Some(p.key) {
                cfg.preset = Some(p.key.to_string());
                if let Err(e) = hw::save_config(&cfg) {
                    eprintln!("[警告] 档位已套用，但写入配置失败: {e}");
                }
            }
            if hw::cur_state().matches(p) {
                println!("当前状态已是「{}」，未重复写入。\n  {}", p.label, p.hint);
                return;
            }
            let (ok, errs) = hw::apply_preset(p);
            println!("已套用「{}」预设：{ok} 项成功，{} 项失败\n  {}", p.label, errs.len(), p.hint);
            for e in &errs {
                eprintln!("  [失败] {e}");
            }
            if !errs.is_empty() {
                std::process::exit(1);
            }
        }
        Some("--install") => {
            if let Err(e) = hw::self_install(&exe) {
                eprintln!("[失败] 自安装出错: {e}");
                std::process::exit(1);
            }
        }
        Some("--omen") => {
            // 自研内核模块的独立自检入口。
            // 与 --check 分开：那个查的是"内核官方接口"，这个查的是
            // "我们自己写的 hp_wmi_omen 模块"，两条路径互为对照。
            if !omen::available() {
                eprintln!(
                    "hp_wmi_omen 模块未加载。\n\
                     请先执行：\n  \
                     sudo bash /mnt/win_d/新建文件夹/wb_records/omen-wmi-omen/install.sh --build"
                );
                std::process::exit(2);
            }
            let args: Vec<String> = std::env::args().skip(2).collect();
            match args.first().map(String::as_str) {
                // 写档位：--omen set performance|default|cool
                Some("set") => {
                    let Some(key) = args.get(1) else {
                        eprintln!("用法: omen-control --omen set <default|performance|cool>");
                        std::process::exit(1);
                    };
                    let p = match key.as_str() {
                        "default" | "balanced" => omen::Profile::Balanced,
                        "performance" | "turbo" => omen::Profile::Performance,
                        "cool" | "quiet" => omen::Profile::Cool,
                        _ => {
                            eprintln!("未知档位 {key}（可选 default/performance/cool）");
                            std::process::exit(1);
                        }
                    };
                    match omen::set_profile_verified(p) {
                        Ok(r) => {
                            println!(
                                "{} → 已写入（值 0x{:02x}）\n  {}",
                                p.label(),
                                omen::profile_value(p).unwrap_or(0),
                                r.note
                            );
                            println!("  转速: {:?}", r.rpm_after);
                        }
                        Err(e) => {
                            eprintln!("[失败] 写入档位出错: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                // 解除限速：--omen unthrottle on|off
                Some("unthrottle") => {
                    let on = args.get(1).map(String::as_str) != Some("off");
                    match omen::set_fan_unthrottle(on) {
                        Ok(_) => println!("风扇限速已{}", if on { "解除" } else { "恢复" }),
                        Err(e) => {
                            eprintln!("[失败] {e}");
                            std::process::exit(1);
                        }
                    }
                }
                // 默认动作：只读状态
                _ => {
                    let t = omen::profile_table();
                    println!("hp_wmi_omen 模块：已加载，接口可用 ✅");
                    println!("  档位值表: {}", match t {
                        Some(x) if x.is_v1 => format!("v1（design_data[3]=0x{:02x}）", x.version),
                        Some(x) => format!("v0（design_data[3]=0x{:02x}）", x.version),
                        None => "读不到".into(),
                    });
                    for p in omen::Profile::ALL {
                        println!("    {:<10} 0x{:02x}", p.label(), omen::profile_value(p).unwrap_or(0));
                    }
                    println!("  风扇数:   {:?}", omen::fan_count());
                    println!("  转速:     {:?}", omen::fan_rpms());
                    println!("  解除限速: {:?}", omen::fan_unthrottled());
                    println!("  GPU 模式: {:?}", omen::gpu_modes());
                    println!("  设计数据: {}", omen::design_data().unwrap_or_default());
                    println!("\n写操作（免 sudo）:");
                    println!("  omen-control --omen set performance");
                    println!("  omen-control --omen unthrottle on");
                }
            }
        }
        Some("--version") | Some("-V") => print_identity(&exe),
        Some("--help") | Some("-h") => {
            println!(
                "OMEN 安全控制面板 {}\n\n\
                 用法:\n  \
                 omen-control              启动图形面板\n  \
                 omen-control --hud        启动监控浮窗（温度/频率/风扇）\n  \
                 omen-control --check      打印硬件接口自检\n  \
                 omen-control --reset      恢复出厂设置（套用「均衡」预设）\n  \
                 omen-control --apply      套用配置里保存的档位\n  \
                 omen-control --set-preset <eco|quiet|balanced|performance>\n    \
                 直接切到某个档位（托盘菜单用的就是它）\n  \
                 omen-control --install    装进应用菜单并开机自启\n  \
                 omen-control --omen       自研内核模块（hp_wmi_omen）状态与控制\n  \
                 omen-control --version    打印正在运行的二进制的身份（版本/构建时间/路径）\n  \
                 omen-control --help       显示本帮助\n\n\
                 说明：控制项写的是内核状态，程序退出后依然生效，重启后由 BIOS 复位。\n\
                 要让设置跨重启保持，请在面板里打开「登录时自动套用此档位」。",
                env!("CARGO_PKG_VERSION")
            );
        }
        _ => ui::run(),
    }
}

/// 打印**正在运行的这份二进制**的身份信息（`--version`）。
///
/// 为什么值得单开一个子命令 —— 这是本项目最消耗信任的一类问题：
/// 二进制曾同时存在于 `/usr/local/bin`、`~/.local/bin`、仓库 `target/release`、
/// `~/.cache/omen-control-target` 四处，于是"我改了、也编译了，但跑的到底是不是那份？"
/// 每天都在猜（这一轮就实打实踩了两次）。
/// 这里一次给全：版本号 + **编译期注入的构建时间** + 真实运行路径 + 大小 +
/// 免密组件路径 + 内核模块状态。对照 `dpkg -s omen-control` 即可确认没跑旧货。
fn print_identity(exe: &std::path::Path) {
    println!("omen-control {}", env!("CARGO_PKG_VERSION"));
    println!(
        "  构建时间 : {}",
        option_env!("OMEN_BUILD_TS").unwrap_or("(未注入 —— 直接 cargo build 不会有，用 build.sh 打包时才有)")
    );
    println!("  运行文件 : {}", exe.display());
    match std::fs::metadata(exe) {
        Ok(m) => println!("  大小     : {} 字节", m.len()),
        Err(e) => println!("  大小     : 读不到（{e}）"),
    }
    match hw::helper_path() {
        Some(h) => println!("  免密组件 : {h}"),
        None => println!("  免密组件 : 未安装（改档位会回退弹密码框）"),
    }
    println!(
        "  内核模块 : {}",
        if omen::available() {
            "hp_wmi_omen 已加载（风扇曲线可用）"
        } else {
            "hp_wmi_omen 未加载（风扇曲线不可用，见 omen-wmi-omen/install.sh）"
        }
    );
}
