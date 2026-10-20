# OMEN 控制面板 (Rust + GTK4)

> **仅支持 HP OMEN 16-k0xxx（主板 8A1A，本机 BIOS F.23）。**
> 其它机型的写操作会被内核模块直接拒绝（只读模式）；没有第二台机器验证过的功能，
> 本项目不会声称支持。

HP OMEN 笔记本在 Linux 上的性能与散热控制面板 + 监视浮窗。

> ## 安全边界（2026-10-20 更正）
>
> 早期文档写过「**零风险**、全程不读写 EC 寄存器」——**风扇功能实装后这句话不成立**。
> 风扇曲线经 `hp-wmi-omen` 的 WMI `GM12` 写 EC 的 `SFS1`/`SFS2`，是**真实的硬件写**；
> 写错值的最坏情况是风扇被钉在某个转速（本机踩过 255 = 满速且不回落），
> 不过 `echo release > fan_speed` 能交回固件。
>
> 其余功能（CPU 上限 / 睿频 / EPP / GPU 时钟锁）走内核官方接口，可逆。
>
> **未知机型默认只读**：内核模块带 DMI 白名单，非 `8A1A` 主板的所有写接口直接
> 返回 `-EPERM`。要冒险需显式 `modprobe hp-wmi-omen allow_unsafe_writes=1`。

### 支持机型

| 机型 | 状态 |
| :- | :- |
| HP OMEN 16-k0xxx（board `8A1A`，BIOS F.23） | ✅ 真机实测全部功能 |
| 其它 HP OMEN / Victus | ❌ 未验证；模块强制只读 |

配套内核模块：[`hp-wmi-omen`](../omen-wmi-omen)（GPL-2.0）。

> **仓库布局**：两个仓库请并排放置（安装脚本按同级目录找模块源码）：
> ```bash
> git clone https://github.com/lilyrosary-eng/omen-wmi-omen.git
> git clone https://github.com/lilyrosary-eng/omen-control-rs.git
> ```
> 也可用 `OMEN_KERNEL_REPO=/path/to/omen-wmi-omen` 指定别的位置。

> ## 本项目的两条硬规则
>
> **1. 界面上不出现无效控件。** 任何"点了没反应"的开关都不算功能，而是缺陷。
> 每个可操作项都必须在真机上做过 **写 → 读回 → 带负载验证**；实测无效或不可靠的
> （ACPI 档位、GPU 功耗墙）一律改为**只读展示**，并在界面上写清原因。
>
> **2. 不假装能做到做不到的事。** 键盘 RGB、GPU TGP 在本机是内核/vBIOS 层面就
> 封死的（证据见下），工具不做，也不给一个"看起来能调"的滑块。
>
> **⚠ 2026-10-11 更正**：本条曾把「风扇曲线」也列为封死项 —— **那是错的**。
> 项目自带的 `hp-wmi-omen` 内核模块绕过 `platform_profile`，直接发 HP 私有 WMI
> 命令 `GM12`，**实测可连续调速**（1992~4950 RPM，写 0 交回固件）。
> 风扇曲线与温度限制（PL1+PL2、GPU 时钟锁）现已实装，由 `omen-fand` 守护持续执行。

## 能控制什么（都经真机验证）

| 项目 | 接口 | 验证方式 |
| :- | :- | :- |
| **CPU 性能上限**（20–100%，步进 5） | `intel_pstate/max_perf_pct` | 写 80 → 读回 80 ✔ 且**负载下降温 23 °C**（见下表） |
| **CPU 长时功耗墙 PL1**（步进 5 W） | `intel-rapl:0/constraint_0_power_limit_uw` | 写 55 W → 读回 55 且保持 ✔（范围＝内核自报的可写上限） |
| CPU 睿频开关 | `intel_pstate/no_turbo` | 写 off → 读回 1 ✔ |
| CPU 频率上限（步进 100 MHz） | `cpufreq/scaling_max_freq`（24 核批量） | 写 3.0G → 24 核全部同步 ✔ |
| 能效偏好 EPP | `cpufreq/energy_performance_preference` | 写 power → 读回 power ✔ |
| 最大风扇 | `hp-wmi hwmon/pwm1_enable`（0=最大 / 2=自动） | 写 0 → 读回 0，风扇实测 2015 → 3343 RPM ✔ |

**只读监视**（浮窗 + 面板）：CPU 温度/频率、GPU 温度/频率/占用/显存、双风扇 RPM、主板/NVMe 温度。

### 实测：限频到底能不能降温

用 16 线程 `yes` 负载跑 40 秒到稳态，读 `coretemp` 的 `Package id 0`：

| 设置 | 负载稳态 CPU 温度 | 稳态平均频率 |
| :- | :- | :- |
| `max_perf_pct = 20` | **43 °C** | 0.92 GHz |
| `max_perf_pct = 100` | **66 °C** | 2.55 GHz |

**差 23 °C** —— 这是"限制 CPU 功耗降温"真正有效的那个旋钮，完全可逆。

> 注意"没降温"的常见误判：**空载时 CPU 不是主要热源**。本机空载实测
> CPU 52 °C / 主板 54 °C / NVMe 44 °C / GPU 49 °C，风扇 2400 RPM。
> 限 CPU 频只影响**负载下**的温度峰值；空载温度主要由 dGPU 与机身风道决定。
> 用浮窗在**跑负载时**看数值才看得出效果。

## 「设定值」与「当前生效值」是两件事（2026-10-11 修正）

面板 CPU 组里，**「性能上限」和「长时功耗墙 PL1」两个滑块显示的是你设定的基准**
（存进 `~/.config/omen-control/config` 的 `cpu.pct` / `cpu.pl1`），
**不会**被高温限流守护改写；守护此刻真正写进硬件的值，显示在紧邻的**只读**行
「当前生效上限」/「当前生效功耗墙」里，并在限流生效时标注「高温限流中 · 设定 xx」。

> **为什么改**：原先滑块直接跟随 sysfs，而守护高温时会把 `max_perf_pct` 100→62、
> PL1 135→30 来压热源，温度回落再写回 ⇒ 滑块**自己来回跳**，用户设的值 1.2 s 后
> 就被刷掉，表现为「长时功耗墙一直跳、根本没法限制」。现在跳只发生在只读那一行。
> 同样地，**档位高亮**也从"实时状态恰好等于哪档"改成以你选的那一档为准 ——
> 否则限流一生效，高亮就会闪掉。

> **守护也认这个基准了（2026-10-11 实测血案）**：守护原先只认**档位**声明的
> `cpu_pct`。于是你在面板把「性能上限」拉到 50%，守护限流一次、温度回落后就把它
> 写回档位的 100% —— journalctl 里 `helper set-cpu-pct 100` 就是它，
> 表现成「能砍的都砍了，温度照样冲 92 °C」。现在守护的**每轮基准**与**收尾还原**
> 都走 `hw::cpu_base_pct()`（`cpu.pct` → 档位 → 100），PL1 同理优先还到 `cpu.pl1`。
>
> **「频率上限」不是长期设置**：写 `max_perf_pct` 会让 intel_pstate 重算各核
> `scaling_max_freq`，把你的频率上限一并抹掉（实测各核回到硬件最高）。
> 想长期限频请用**「性能上限」**——同一条轴上，而且守护也认它。
>
> **还有两处会把设定值悄悄冲掉的路径，一并修了**：
> · `auto_apply=1` 的**登录自动套用**原先无条件套用档位声明的 `cpu_pct`
>   ⇒ 每登录一次就把你自定义的性能上限冲回档位值。现在走 `cpu_base_pct()`，
>   并且顺带套用 `cpu.pl1`（否则重启机器后 BIOS 会把功耗墙复位成 135 W）。
> · 守护**启动时不再只"存着"基准**：现在会主动断言一次 `cpu.pct` / `cpu.pl1`。
>   否则手改配置或上次会话设的值，在守护重启后根本不会生效
>   （实测：`echo cpu.pct=50` + 重启守护，`max_perf_pct` 仍是 100）。
>
> `--reset`（恢复出厂）会**先清掉** `cpu.pct` / `cpu.pl1` 再套用「均衡」，
> 否则"恢复出厂"会把你的自定义值原样套回来。

> **界面文案有硬上限**：所有行的副标题都设了 `subtitle-lines = 3`
> （`spin_row` / `value_row` 里）。这是一道护栏而不是美化 —— 实测 5 行的副标题会
> **把整页顶出去**（右侧数值和加减按钮被挤出屏幕）。写文案时请自觉短；
> 别用 Markdown 的 `**加粗**`，GTK 渲染的是 Pango 标记，`**` 会原样显示成星号。

⇒ 想用一个**固定功耗墙**把 CPU 压住不再冲 100 °C（Windows 侧的做法）：
把「长时功耗墙 PL1」设成 45 W 之类，它就钉在那里不再被改写；
再把「高温时自动压 CPU/GPU」组里的「CPU 功耗墙（PL1）」也设成同一个 45 W，
就等于高温时不再额外动它（那一项没有"关闭"档，设成与设定值相同即为不干预）。

## 档位设计

档位只用**三个真正起作用的旋钮**：`max_perf_pct`（主力）、EPP、睿频开关。

| 档位 | CPU 上限 | 频率墙 | 睿频 | EPP | 实际感受 |
| :- | :- | :- | :- | :- | :- |
| 省电 | **40%** | **2.4 GHz** | 关 | power | 明显变慢、明显变凉 |
| 安静 | **65%** | **3.6 GHz** | 开 | balance_power | 日常办公够用，风扇基本不吵 |
| 均衡 | 100% | 放开 | 开 | balance_performance | **出厂原始状态**（顶栏「恢复默认」回到这里） |
| 性能 | 100% | 放开 | 开 | performance | 持续高负载时频率更稳 |

档位现在有**两个真实维度**（性能百分比 + 频率墙），彼此差别立刻可测量。

四个档位之间的差别立刻可测量，不是"看着变了、实际没变"。

## 设置会不会"关掉就没了"

**默认行为：关掉就清零。** 程序启动时拍一份"我进来时系统是什么样"的快照，
退出时把它写回去 —— 于是**所有设置只在程序存活期间生效**：

| 退出方式 | 会发生什么 |
| :- | :- |
| 点「退出」按钮 | 后台线程执行还原（4~6 次提权，约 0.3 s），完成后弹窗告知失败项并退出 |
| 关闭主面板（未开隐藏模式） | 同上 |
| 关闭主面板（开了隐藏模式） | **不退出、不还原**，只是隐藏；浮窗继续运行 |
| 被强杀 / 崩溃 / 断电 | `pending-restore` 标记残留，**下次启动自动还原**并打印提示 |
| 关机 / 重启 | 由 BIOS 复位（本来就还原了） |

> 实现要点：快照在**启动时**就落盘为 `pending-restore`，只有干净退出才删除它。
> 这样"上次是不是被强杀"就能在下次启动时判断出来。

面板「启动与还原」组里有两个开关：

| 开关 | 作用 |
| :- | :- |
| 退出时还原为启动时的状态 | 关掉它 = 设置会一直留着（老行为） |
| 关闭主面板时隐藏到后台 | 程序继续在后台跑（会确保浮窗已启动，否则窗口会"找不到"） |
| 登录时自动套用所选档位 | 把当前档位记到 `~/.config/omen-control/config`，每次登录自动应用 |
| 登录时显示监控浮窗 | 立即创建 / 删除 `~/.config/autostart/omen-control-hud.desktop` |

> **正因有了"退出即还原"，PL1 那个"降下去调不回来"的顾虑消失了** ——
> 它是本工具里唯一不可逆的旋钮，但退出时会自动写回启动时的值。

## 监控浮窗

顶栏第一个按钮（或 `omen-control --hud`）打开一个小窗口：

```
┌ OMEN 监控 ────────────── [⧉] [×] ┐
│ CPU        56 °C · 2.41 GHz      │
│ GPU        49 °C · 0 % · 210 MHz │
│ 风扇       2409 / 2184 RPM       │
│ 主板       54 °C                 │
└──────────────────────────────────┘
```

1 秒刷新。它是**独立进程 / 独立 application id**，所以能和主面板同时开，
也可以单独设开机自启。

| 透明度 | 30% ~ 100% 可调，实时生效并记住设置（存 `~/.config/omen-control/config`） |

> **无法自动摆到左上角**：GTK4 移除了窗口定位 API（`gtk_window_set_position` 已无），
> 而且 Wayland 的安全模型**不允许应用自己决定窗口初始位置** —— 位置由合成器决定。
> 这是平台限制，不是没做。请手动拖到想要的位置；GNOME 的「置于顶层」也请一并设置：
> 在窗口上右键标题栏 → 「**置于顶层**」。

## 系统托盘（顶栏常驻）

装好 deb 后**登录即自动出现在顶栏**（`/etc/xdg/autostart/omen-control-tray.desktop`），
不必先开面板。点图标弹出的菜单：

```
打开控制面板
监控浮窗
────────────
◉ 省电     ○ 安静     ○ 均衡     ○ 性能        ← 单选，选中态跟随当前档位
────────────
☑ 自动风扇控制（曲线 + 温控）                    ← 开关 omen-fand.service
────────────
CPU 62°C  ·  GPU 44°C  ·  风扇 2685 RPM         ← 2 秒刷新的实时读数
────────────
关闭面板与托盘                                  ← 先关面板、再退托盘（面板没开时置灰）
退出托盘                                        ← **只**退托盘，不碰面板
```

> **「关闭面板与托盘」走的是干净退出**（2026-10-11 修正）：面板接管了
> `SIGTERM` / `SIGINT` / `SIGHUP`，转成与点「退出」**完全一样**的流程
> （清 `pending-restore` 标记 + 按「退出时还原」的设置决定是否还原）。
> 不接这几个信号的话，被杀掉的面板会**留下 `pending-restore` 标记**，于是
> **下次打开会误判成"上次是崩溃"**，白白做一次"还原到快照"并弹一句警告。
> 踩到它的不止托盘菜单 —— **每次 deb 升级**（prerm / postinst 收掉旧面板）也是
> SIGTERM，同样会留这个尾巴。实测：修之前 kill 掉面板，标记留在磁盘上、下次
> 打开真的打印了「检测到上次未正常退出…」；修之后 SIGTERM 能把标记清干净。

托盘**自己不改任何硬件**，每个动作都转发给现成入口 —— 切档位走
`omen-control --set-preset <档位>`（与面板按钮同一套逻辑：先记选择、再套用，
当前状态已匹配就跳过重复提权）；守护开关走 `helper set-fan-daemon on|off`；
CPU 温度与风扇转速读 `/sys/class/hwmon`（与 `--check` 同源）；
GPU 温度走 `nvidia-smi`（本机及多数笔记本 N 卡**没有 nvidia 的 hwmon 节点**，
它是唯一来源；因为要起进程，放在后台线程里刷，不占主循环）。

| 项 | 说明 |
| :- | :- |
| 实现 | Python + PyGObject + AyatanaAppIndicator3（GTK3）。**为什么不是 Rust**：托盘要 GTK3 的 appindicator，而面板是 GTK4；本机 cargo 缓存里没有 dbus / tray crate，离线装不进来 —— 用系统自带的 Python 绑定零编译、零新增下载 |
| 依赖 | `python3-gi` / `gir1.2-gtk-3.0` / `gir1.2-ayatanaappindicator3-0.1`（deb 的 `Depends` 会自动补齐） |
| 位置 | `/usr/libexec/omen-control/omen-tray`（源码安装则是 `~/.local/bin/omen-tray`） |
| 单实例 | 启动时扫 `/proc`，已有 `omen-tray` 就直接退出 —— 顶栏不会出现两个图标 |
| 升级/重启 | deb 的 postinst 调 `omen-tray-restart`：**从旧进程 `/proc/<pid>/environ` 捞出会话环境 → 杀掉旧托盘 → 用同一套环境拉起新的**。所以升级不会丢托盘，也不会留下跑旧代码的进程。手工执行：`sudo omen-tray-restart`（在 `/usr/libexec/omen-control/`） |
| 前提 | GNOME 下需要 `ubuntu-appindicators@ubuntu.com` 扩展（Ubuntu 默认启用）；无该扩展的桌面不会显示 |
| 面板开关 | 「启动与还原」组里有 **「顶栏托盘」** 开关：开 = 立刻拉起（没跑时）+ 登录自启；关 = 立刻结束 + 下次登录也不启动。**关掉再打开 = 重启托盘**，不用重新登录。副标题只报事实（`运行中（PID x）` / `已启用但没在运行：<原因>`），`--check` 里也有一条 `顶栏托盘:`。关自启用的是 XDG 的 `~/.config/autostart/omen-control-tray.desktop`（`Hidden=true` 屏蔽系统那条），可逆、不需要 root |

> 不经安装直接试跑：`python3 packaging/omen-tray`（前台运行，Ctrl-C 退出）。

## 实测结论：两类"看起来能用、实际不能"的功能

这三项都已在真机上做过带负载的实证，不是推断。

### ① ACPI 性能档（`platform_profile`）——对硬件零影响

依次切 `cool / quiet / balanced / performance`，逐项读数：

| 档位 | EPP | 频率上限 | no_turbo | max_perf_pct |
| :- | :- | :- | :- | :- |
| cool | performance | 4900000 | 0 | 100 |
| quiet | performance | 4900000 | 0 | 100 |
| balanced | performance | 4900000 | 0 | 100 |
| performance | performance | 4900000 | 0 | 100 |

**全无变化**（GPU 功耗墙那列只有 Dynamic Boost 的随机浮动）。

**根因**（内核源码）：`setup_active_thermal_profile_params()` 里

```c
id = dmi_first_match(victus_s_thermal_profile_boards);
if (!id) return;      // ← 本机主板 8A1A 不在表里，直接 return
```

主板不在 DMI 表 → 内核**不会**把档位下发到硬件。旁证：EC 档位寄存器 `0x95` 在四档下
恒为 `48`（= 内核常量 `HP_OMEN_V1_THERMAL_PROFILE_DEFAULT = 0x30`）。

⇒ 界面上改为**只读展示**。

### ② GPU 功耗墙（`nvidia-smi -pl`）——命令返回 0，但是空操作

用 `ffmpeg` 的 NVENC（4K@120）造真实 GPU 负载，预热 25 s 后逐项写入、采样 20 s：

| 写入 | `power.draw` 均值 / 峰值 | `Current Power Limit` 读回 |
| :- | :- | :- |
| 不写（基线） | 57.6 / 59.6 W | 118.4 W |
| **40 W** | **61.8 / 80.2 W** | 118.3 W |
| 175 W | 66.3 / 82.8 W | 112.7 W |

**写了 40 W，GPU 照样跑出 80.2 W 峰值** ⇒ TGP 由平台 vBIOS 锁定。
（`nvidia-powerd` 在跑但日志一直刷 `ERROR! JPAC is not created/already destroyed`——
Dynamic Boost 的 ACPI 接口在 Linux 上没建好。）

> **nvidia-smi 没有任何字段能反映"设置值"**：
>
> | 字段 | 实测 | 性质 |
> | :- | :- | :- |
> | `--query-gpu=power.limit` | `[N/A]` | 笔记本恒为空 |
> | `--query-gpu=enforced.power.limit` | 115（空闲）/ 122~136（负载） | 含 Dynamic Boost 的动态值 |
> | `-q -d POWER` → `Requested Power Limit` | `N/A` | 同样为空 |
> | `-q -d POWER` → `Current Power Limit` | 连续读 `110/110/113/113/116` | **也含 DB，会浮动** |
>
> ⇒ 用"读回值 == 写入值"当判据必然误报。
>
> 另外 `power.draw` 偶发返回**物理不可能的垃圾值**（实测读到 `751.97 W`，而上限只有
> 175 W）。`hw.rs` 里已加上限校验：超过上限的读数直接判为无效，界面显示 `—`。

⇒ 界面上改为**只读展示**。

### CPU PL1 的两个怪癖（为什么可写范围只到 55 W）

| 事实 | 实测值 |
| :- | :- |
| `constraint_0` 的角色 | `long_term`（真正的 PL1） |
| 内核声明可调上限 `constraint_0_max_power_uw` | **55 W** |
| BIOS 实设值 `constraint_0_power_limit_uw` | **135 W**（高于声明上限） |
| 写入 55 W 后观测 24 s | 保持 ✔（写入机械上有效） |
| 该值会不会被系统服务改回 | ❌ **不会**（2026-10-11 实测：写 30 W 后 3 分钟纹丝不动，thermald 全程无日志） |

> ⚠ 这一段曾得出两个**后来被实测推翻**的结论（"可写范围只有 55 W"、
> "实际值高于上限时禁用滑块"）。真相是：**intel-rapl 写入不 clamp**，那个 55 W 只是
> BIOS 报的能力值。所以控件给的是 **5~200 W**、永远可调，写入是否生效交给 helper
> 的**读回校验**回答（详见下一段）。

⇒ 它是**真实可用**的控件。**2026-10-11 实测更正**：旧文这句"写 55 W 才保持"其实源自
一个误判 —— 可写范围是 5~200 W，`constraint_0_max_power_uw` 报的 55 W 只是 BIOS
**能力值**、不是钳位上限；写下去**也不会**被 thermald/ppd 改回（写 30 W 观测 3 分钟
纹丝不动，thermald 全程无日志）。

它是限 CPU 功耗的**主力**，但**必须同时压 PL1 与 PL2**：PL1 是约 56 秒窗口的平均值
（窗口节点只读、改不了），只压 PL1 时 CPU 仍能按 PL2 的 135 W 连续睿频几十秒
—— 实测只写 PL1=30 时前 24 秒稳在 84°C，第 27 秒才掉到 58°C；PL1+PL2 一起压则
**2 秒内**生效（54~57°C）。

## 真正做不到的（内核 / 固件层面，不是没做到）

| 需求 | 证据 |
| :- | :- |
| **只用内核 `hp-wmi` 调风扇** | 它的 hwmon 只有 4 个节点：`fan1_input`、`fan2_input`、`name`、`pwm1_enable`。**没有 `pwm1`**，也没有 `fan*_target` ⇒ 官方接口只有"自动 / 最大"两态 |
| 解锁 5000+ RPM | 最大档实测峰值约 3340 RPM，更高档在 EC 固件里 |
| 键盘 RGB | `/sys/class/leds/` 下无任何 kbd/rgb 节点 |
| GPU TGP 解到 175 W | 平台 vBIOS 锁定（见上） |
| 直接写 EC 改风扇设定值 | 固件是闭环，设定值在它自己的内部变量里，**不暴露在 EC 空间**（写 `0x2E/0x2F` 会被 2 秒内改回，RPM 不动） |

### 风扇曲线为什么仍然做得到（走的是另一条路）

| 路径 | 结论 | 依据 |
| :- | :- | :- |
| **本项目自带的 `hp-wmi-omen` 模块**（`fan_speed` 节点 → WMI `GM12`） | ✅ **有效** | 实测 20~48% 连续给出 1992~4950 RPM；写 `0` 交回固件；`固件 rc=0` |
| 内核 `hp-wmi-extended`（DKMS，提供 `fan*_target` 精确 RPM） | ❌ 排除 OMEN | 源码注释：*Omen boards are excluded unconditionally… because Omen hardware does not support the Victus S GM fan speed commands (0x2D/0x2E/0x2F)*；其 DMI 表不含本机 `8A1A` |
| 用户态写 EC（`ec_sys write_support=1`） | ❌ 内核拒绝 | **Secure Boot → Lockdown integrity**：`Lockdown: modprobe: unsafe module parameters is restricted`，`ec_sys: '1' invalid for parameter 'write_support'` |
| 厂商工具 OmenSuperHub | ❌ 平台不符 | 走 `HPQOEM_WmiInterface`，Windows 专有 |

> **关键区别，别再混**：`GM12`（本项目用）与 Victus S 的 `0x2D/0x2E/0x2F` 是**不同命令**。
> 上游把 OMEN 从 *Victus S* 的 fan speed 命令里排除是对的，但这**不等于** OMEN 没有
> 风扇控制 —— 本机实测 `fan_speed` 写入被固件接受、转速可观测变化
> （`fan0: set=22% → 2169 RPM`）。早先把"上游排除 OMEN"直接读成"OMEN 风扇不可控"，
> 导致整条风扇控制链被误判封死，是本项目最大的一次误判。

**已确认的硬件事实**：

- EC `0xB0-0xB1` / `0xB2-0xB3` = 双风扇转速（小端 u16），三次采样与 `fan1_input`/`fan2_input` 完全吻合
- EC `0xEC` = 最大风扇标志（`0`=自动，`12`=最大）
- EC `0x95` = ACPI thermal profile 档位寄存器（内核常量 `HP_OMEN_EC_THERMAL_PROFILE_OFFSET`）
- 本机切 `platform_profile` 时内核**不写**这个寄存器

> `modinfo -p ec_sys` 的原话值得记住：
> `write_support:Dangerous, reboot and removal of battery may be needed. (bool)`
> —— 即使绕过 Secure Boot，这个风险依然存在。

**静音怎么做（2026-10-11 更新）**：以前只能靠「安静 / 省电」档压功耗、盼 EC 跟着降速
—— 那是**风扇不可控时代的妥协**。现在有两条真正的通道：

1. **风扇曲线**（首选、最直接）：在面板「风扇曲线」里把低中温段转速调低，`omen-fand`
   会持续按曲线写 `fan_speed`；温度低于曲线首点时自动交回 BIOS（那一段它本来就平滑）。
2. **压热源**：性能上限 / PL1+PL2。
   注意 `fan_lag`（安静 τ=22s / 灵敏 τ=3s）只决定风扇**跟温度跟得多紧**，不改曲线形状。

## 安装 / 升级（deb，推荐）

```bash
cd omen-control-rs
bash build.sh --deb
sudo apt install ./dist/omen-control_<版本>_amd64.deb

dpkg -s omen-control | head -3     # 包里是什么版本
omen-control --version             # 正在跑那份的身份（版本/构建时间/路径/helper/模块）
```

deb 把二进制**只装在** `/usr/bin/omen-control`（helper 与托盘在 `/usr/libexec/omen-control/`），
另装图标、桌面条目与托盘自启条目（`/etc/xdg/autostart/omen-control-tray.desktop`）。
postinst 还会：写免密规则（只放开该 helper、只给 `sudo` 组，`visudo` 预校验 + 失败回滚）、
按安装用户生成 `omen-fand.service`、**移开历史的同名副本**
（`/usr/local/bin`、`~/.local/bin`、用户级 `.desktop` 与用户级托盘自启
—— 它们会在 PATH / 菜单 / 会话里盖住新版或拉起第二个托盘）。

> **内核模块现在随包安装**：`/usr/src/hp-wmi-omen-0.1.0/` 是模块源码，postinst 用
> **DKMS** 针对当前内核编译并签名；以后内核升级由 DKMS 自动重建。
>
> 能不能"装完就可用"，取决于 Secure Boot：
>
> | 机器状态 | 装完 deb 后 |
> | :- | :- |
> | Secure Boot 关闭 | ✅ 直接可用 |
> | Secure Boot 开启 + 已有已注册 MOK（`/var/lib/shim-signed/mok/`） | ✅ DKMS 自动签名，直接可用 |
> | Secure Boot 开启 + 没有任何 MOK（**本机就是这种**，该目录为空） | ⚠️ postinst **跳过**模块安装（避免用未签名模块覆盖已有可用版本），需一次性：`sudo bash /usr/src/hp-wmi-omen-0.1.0/install.sh --genkey` → 重启并在蓝色界面确认 → `sudo dpkg-reconfigure omen-control` |
>
> **为什么 `~/.local/bin/omen-control` 是个符号链接**：postinst 不再把那里的旧副本
> "移开"，而是把它做成指向 `/usr/bin/omen-control` 的链接。除了消灭"两份二进制"，
> 更关键的是治 **GNOME Shell 的启动项缓存** —— shell 会把见过的启动项路径缓存在
> 内存里，即使那个 `.desktop` 后来被删掉（全盘只剩系统级那一个），点「全部应用」
> 仍会去 exec 老路径，报「程序未在 $PATH 路径中找到」。留一个**永远指向真身**的
> 链接，缓存就再也指不到空路径上，用户也不用重新登录。
>
> **升级时"杀掉上一个同类进程"要放两个地方**：dpkg 升级时调用的是**旧包自己的
> prerm** —— 所以只改 prerm 要**晚一个版本**才生效。postinst 属于新包、当次就有效，
> 因此两处都有（prerm 先 SIGTERM、2 秒后仍活着就 SIGKILL；postinst 兜底再收一次）。

> 源码安装（`sudo bash system/setup-nopasswd.sh` + `bash build.sh --install`）仍保留，
> 但**已被 deb 取代**：它会往 `~/.local/bin` 再放一份副本，而 `~/.local/bin` 优先于
> `/usr/bin`，"到底跑的是哪一份"会重新变成谜。所以 `--install` 现在会**主动拒绝**
> 在包安装环境下执行。

> ⚠ **helper 的子命令集会随版本变化**。当前是 **8 个**：`set-maxfan` / `set-cpu-pct` /
> `set-epp` / `set-turbo` / `set-cpu-freq` / `set-rapl` / `set-gpu-clk` / `set-fan-daemon`。
> 历史版本曾含已移除的 `set-profile`、`set-gpu-power`、`set-governor`（第三个是旧
> `omen-thermal` 守护的开关 —— 界面早已删掉该分组，2026-10-11 按"不为做不到的事保留
> root 权限"的规则一并删除）。

## 构建

```bash
sudo apt install libgtk-4-dev libadwaita-1-dev libglib2.0-dev pkg-config
sudo apt install cargo rustc        # 国内镜像快；官方 rustup 很慢

./build.sh              # 仅编译
./build.sh --install    # 编译 + 装进应用菜单 + 开机自启
```

子命令：

```bash
omen-control              # 启动图形面板
omen-control --hud        # 启动监控浮窗（独立进程）
omen-control --check      # 硬件接口自检
omen-control --reset      # 恢复出厂（均衡档）
omen-control --apply      # 套用配置里保存的档位
omen-control --set-preset <eco|quiet|balanced|performance>   # 直接切档（托盘菜单用的就是它）
omen-control --install    # 装进应用菜单 + 开机自启（源码安装；含托盘）
```

## 功能自检（装好后跑一次）

```bash
sudo bash system/self-test.sh
```

逐项**真实读写一次并立即恢复原值**：免密通道（含 helper 版本检查）/
CPU 性能上限 / 睿频 / 频率上限（含 24 核同步校验）/ EPP / 最大风扇 /
**GPU 频率锁定探测**（决定能否恢复 GPU 侧降温能力）。
调用路径与面板完全一致（`sudo -u <你的账号> sudo -n <helper>`），因此同时验证了免密通道。

刻意**不测**的两项（都在上面"实测结论"里，不是遗漏）：ACPI 性能档、GPU 功耗墙。

另有 `system/diagnose.sh`，用于在**新机器**上实测哪些接口可写（每个写测试自动还原）。

## 免密是怎么做到的

1. `/usr/libexec/omen-control/omen-control-helper`（deb 安装）或
   `/usr/local/libexec/omen-control-helper`（历史的 `setup-nopasswd.sh`）—— 白名单脚本，
   `root:root 0755`，**8 个子命令**。参数逐项字面量 / 范围校验，其余一律拒绝
   （已实测 18+ 种非法输入，含注入与越界）。
   程序侧**两个路径都认**（deb 优先），升级过渡期不会丢免密通道。
2. `/etc/sudoers.d/omen-control` —— **只放开这一个脚本**。写盘前用 `visudo -cf`
   预校验，失败立即回滚，绝不留下可能损坏 `sudo` 的配置。
3. 程序侧先 `sudo -n <helper>`（零弹窗），失败才回退 `pkexec`。

**不扩大权限上限**：账号本就在 `sudo` 组，此规则只是把"改这些节点"从输密码变成免密。
每移除一个功能就同步删除对应的 helper 子命令——**不为做不到的事保留 root 权限**。

## 界面层的稳定性设计

这几条都是踩过坑才补上的（都是真实缺陷，不是假设）：

| 措施 | 不这样做会怎样 |
| :- | :- |
| **每个滑块各自持有去抖定时器** | 共用一个 → 450 ms 内拖第二个滑块会**取消第一个的操作** |
| **用户操作后 2 s 内不回写该控件** | 否则拖动中的滑块会被 1.2 s 一次的刷新**弹回旧值** |
| **提权操作串行化**（`busy` 标志） | 快速连点两个档位 → 两组写入**交错**，最终状态不可预期 |
| **滑块步进显式设为 5**（频率 100 MHz） | `AdwSpinRow` 默认步进是 1，拖按钮每下只动 1，太琐碎 |
| **`run_with_args` 传干净 argv** | 用 `run()` 会把 `--hud` 交给 GApplication 解析 → 报"未知选项"直接退出 |
| **GPU 采样线程带存活标志** | 关窗后线程常驻不退出 |
| 主面板与浮窗用**不同 application id** | 同 id 的话第二次启动只会激活第一个实例，`--hud` 参数被丢掉 |
| **单窗口**：`activate` 只 present 已有窗口 | 否则每点一次应用图标就多开一个窗口 |
| **崩溃恢复挂在 `startup` 信号上**，不能放进程开头 | 放开头的话，单实例机制下"秒退的第二个进程"会把主实例的 pending 标记消费掉 |
| `close_request` 只注册**一个** handler | 该信号用 `accumulator_true_handled`：一个返回 Stop，后面的 handler 不再执行 |
| 单窗口闭包里先 `borrow().clone()` 再 `borrow_mut()` | `match win.borrow() { … borrow_mut() … }` 会 panic（RefCell already borrowed） |

## 放在 D 盘（NTFS）上编译

`build.sh` 已自动处理三个坑：

1. NTFS 上可执行位会丢（`./build.sh` 报权限不够）→ 脚本自补 `chmod +x`。
2. **target 不能放 NTFS**（缺 POSIX 文件锁语义）→ 自动改到 `~/.cache/omen-control-target`。
   判断文件系统用 `findmnt -n -o FSTYPE -T <目录>`（`stat -f -c %T` 在 ntfs3 上返回
   `UNKNOWN`，`mountinfo` 是 ` - ntfs3 ` 格式，都不可靠）。
3. `~/.cargo` 属主异常 → 自动改用 `~/.cargo-local` 并继承镜像配置。

## 代码结构

```
src/main.rs        子命令分发（GUI / --hud / --fan-daemon / --check / --reset / --apply
                              / --set-preset / --install / --omen / --version / --help）
src/hw.rs          硬件层：sysfs + CPU 控制 + GPU 只读层 + 温度读数 + 预设 + 配置 + 快照/还原
src/ui.rs          主面板：Adwaita PreferencesPage / ToggleGroup / SpinRow / 后台线程提权
src/hud.rs         监控浮窗：独立 app id，1 s 刷新，只读
src/limit.rs       温度限制策略（纯数据 + 纯函数，带单测；PL1/PL2、CPU 上限、GPU 锁）
src/curve.rs       风扇曲线：温度 → 转速插值
src/curve_editor.rs 曲线编辑器控件（拖点改曲线）
src/omen.rs        配套内核模块 hp_wmi_omen 的接入层（模块派生自上游 hp-wmi）（风扇转速/百分比、档位、GPU 模式）
src/fan_daemon.rs  风扇曲线守护（+ 温度限制的执行者）

system/omen-control-helper   免密白名单 helper（8 个子命令；参数逐项校验）
system/omen-fand.service.in  守护的 systemd unit 模板（install-fand.sh 代入用户后生成）
system/install-fand.sh       源码安装：二进制 + unit + 桌面启动器（**deb 已取代它**）
system/setup-nopasswd.sh     源码安装免密（**deb 的 postinst 已含同样规则**）
system/self-test.sh          端到端功能自检（逐项真实读写并立即还原）
system/diagnose.sh           能力实测（换新机器时用，可回滚）
system/limit-bench.sh        限制效果量化（基线 / max_perf_pct / no_turbo / PL1 四场景对比）

packaging/                   deb 打包树（make-deb.sh + postinst/prerm/postrm + unit/desktop/图标）
packaging/omen-tray          系统托盘脚本（Python，deb 与源码安装共用这一份）
packaging/omen-control-tray.desktop  托盘自启条目模板（装到 /etc/xdg/autostart）
```

> **2026-10-11 清理**：`system/` 下 35 个实验/历史脚本（`ec-probe*`、`fan-curve*`、
> `thermoguard`、`stress-guard`、`thermal-hold`、`profile-curve`、`perf-mode-*`、
> `sign-ec-sys`、`verify-*` …）已移除 —— 它们是"另一版实现"，留着只会制造
> "哪份是活的"这类困惑。移除前的完整副本在 `.removed-20261011.tgz`（224 KB，可复原）。

线程模型：提权调用（约 100–200 ms）与 GPU 查询（约 60 ms）都在后台线程执行，
主线程只更新界面。

---

> 📋 **功能全量清点与整合设计**：`docs/功能总清单与整合设计.md`
> （五层架构、功能矩阵、重复冲突分析、已废脚本清单、以及以 `omen-fand` 为主体的整合方案）

## 许可证

- 本仓库（用户态）：**MIT**，见 `LICENSE`
- 配套内核模块 `hp-wmi-omen`：**GPL-2.0**（`MODULE_LICENSE("GPL")` 要求）。
  它的 WMI 传输层与命令码表**派生自 Linux 内核 `drivers/platform/x86/hp/hp-wmi.c`**，
  版权归上游作者；本仓库只新增 GM12 写风扇、8A1A 探测与 DMI 白名单。

`OMEN` / `HP` 是 HP Inc. 的商标。本项目与 HP 无任何关联，名称仅作机型说明用途。
