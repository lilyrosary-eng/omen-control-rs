# system/ 脚本地图

> 目录里堆了 20+ 个脚本，大部分是**探索过程留下的**，不需要跑。
> 下面按「现在该跑什么」排序。

> ⚠ **2026-10-11 重要变更**：目录里 35 个实验/历史脚本**已被删除**
> （`ec-probe*`、`fan-curve*`、`thermoguard`、`stress-guard`、`thermal-hold`、
> `profile-curve`、`perf-mode-*`、`sign-ec-sys`、`verify-*` …）。
> **本文档保留作实验方法记录 —— 文中提到的那些脚本名现在已经不存在，别照着跑。**
> 移除前的完整副本：`omen-control-rs/.removed-20261011.tgz`。
>
> 当前目录只剩 8 个文件：`omen-control-helper`、`omen-fand.service.in`、
> `install-fand.sh`、`setup-nopasswd.sh`、`self-test.sh`、`diagnose.sh`、
> `limit-bench.sh`、`README.md`。

---

## ✅ 已封闭：`0xEC` 不是档位，只是模式标志（2026-10-10 实测定案）

> 实验脚本：`sudo bash ".../system/ec-boost-map.sh" set 6 1`（已跑完）
> 数据：`/tmp/ec-boost-map.tsv`

| 写入 | 固件改写为 | 风扇 | 结论 |
| :- | :- | :- | :- |
| `0x06`(6) | 0x0E → **0x0C**(12)，保持 40 秒 | 1985 → **4627** | 冲顶 ⇒ 满速 |
| `0x01`(1) | **0x03**(3)，保持 38 秒 | 1969 → 2018 | 不动 |

**二进制一看就清楚：**
```
6  = 0b0110   12 = 0b1100     两者都有 bit2(0x04) → 满速
1  = 0b0001    3 = 0b0011     两者都没有 bit2      → 不动
```

⇒ **`0xEC` 是「模式标志字节」，不是转速档位** —— 数值大小不决定转速，**某个位**决定。

### ⇒ EC 直写控制风扇到此封版

风扇控制的最终形态（全部实测有效）：

| 手段 | 范围 | 类型 |
| :- | :- | :- |
| **BIOS 曲线**（被动，不用做任何事） | **1793 → 3506 RPM**，随温度连续 | 连续，但不可编程 |
| `fan_unthrottle=1` | → **4597 RPM**（26 秒软启动斜坡） | 两态 |
| CPU `max_perf_pct` / `no_turbo` / GPU `-lgc` | 压热源（源头） | 连续 |

**⇒ 「任意稳态转速」（如固定 3000 RPM）做不到。**
但这不是妥协：**BIOS 曲线已经覆盖 1793–3506 的连续区间**，缺的只是 3506–4597 之间的稳态中间值。

> ⚠ 数据质量坑（本次踩到）：`0xEC` 偶发读到**上一个偏移的值**（`观36s` 读到 `0x29`=41，
> 前后都是 `0x03`）—— ACPI EC 读会偶发失败/返回脏值。**结论不要建立在单次采样上。**

> 唯一还没试过的候选：`0x3A/0x3B/0x3C`（固件自己在 `45/90/110` 与 `55/135/215` 两套值之间切，
> 社区说 `0x3C` 是「最大速度」）。它们像是**固件自己的曲线参数**，写它大概也会被改回；
> 而且当前值已经是高的那套（215 > 实际用到的 93），不是限制因素。**不打算再试。**

---

## ⛰ EC 的百分比是**断崖**，不是线性（2026-10-10 真机实测）

| `fan_speed` 写入 | 实测 RPM | |
| :- | :- | :- |
| `5%` | **87** | ≈停转 ⚠ |
| `30%` | 3000 | |
| `100%` | 5020 | |

分段斜率：**5~30% ≈ 116 RPM/%**（极陡）；**30~100% ≈ 29 RPM/%**（平缓）。
⇒ 1800 RPM 大约对应 **20%**。

**三条推论**：

1. **绝不能用"从 1% 往上爬"来探下限** —— 低区是断崖，会直接停转。
   必须从**已知安全的 30% 锚点向下小步**逼近，并设硬地板（本脚本用 15%）。
2. **不能用全局线性插值**。D 阶段必须拿实际扫描点做**分段线性插值**（脚本就是这么做的），
   且低端要密（脚本 20~40% 用 2% 步长，40% 以上用 5%）。
3. 我们需要的那段 **3506~4597 RPM** 落在约 **48~85%**（按上面的斜率估算，真机以实测为准）。

> 停转保护：`wait_stable` 检测到 `<600` 或 `>5400` 会**立即返回**（采样 2s，比看门狗 3s 快），
> `probe` 随即写回自动并把该点标记为无效、结束本段 —— 不让看门狗来杀整个流程。

---

## 🚀 可控曲线守护：`fan-dynamic.sh`

**这是标定的最终用途** —— 用真实曲线驱动风扇，替代所有"只能两态"的旧脚本。

```bash
sudo bash fan-dynamic.sh --dry-run     # 预演：完全不写硬件，只看决策
sudo bash fan-dynamic.sh               # 前台跑，Ctrl-C 立刻交回固件
sudo bash fan-dynamic.sh --cpu-full 80 --gpu-full 70
nohup sudo bash fan-dynamic.sh > /var/log/fan-dynamic.log 2>&1 &
```

**策略**（CPU / GPU 分别判，取更激进的那个）：

| 温度 | 动作 |
| :- | :- |
| ≤ `ENTER`（CPU 60 / GPU 55）| 写 `0` ⇒ **交回 BIOS**（这一段它给的曲线本就平滑、安静）|
| `ENTER` ~ `FULL` | 线性：`ENTER`→1992 RPM，`FULL`→4597 RPM |
| ≥ `FULL`（CPU 80 / GPU 70）| **4597 RPM 满速** |
| ≥ 95°C | **无条件满速**（独立于曲线，防曲线被改坏）|

迟滞 5°C（接管中把 `ENTER` 下调）；目标变化 <150 RPM 不写（防抖 + 少写 EC）。

**⚠ 曲线判据用「三点中位数」，不是各核最高值**
—— 空载时单个核心也会瞬时冲到 93~100°C（实测：某次前三大核温 `73,72,56`，中位数只有 **52**）。
只有"临界温度"用原始值（求快）。

**安全**：任何退出路径（含 Ctrl-C）都执行「写 0 + unthrottle 1→0」交回固件；
接管中转速异常（<600 / >5400）连续 3 次也会交回并退出。

> ⚠ **`fan-curve.sh` 已作废** —— 它走「写 EC `0x2E/0x2F` 的 PWM 占空比」，
> 实测证明那两个字节是固件**观测值**、写不进去。`fan-dynamic.sh` 才走有效通道（GM12）。

---

## 📊 真实标定曲线（2026-10-10 真机实测，fan0）

| pct | 20 | 22 | 24 | 26 | 28 | 30 | 32 | 34 | 36 | 38 | 40 | 45 | **48+** |
| :- | :- | :- | :- | :- | :- | :- | :- | :- | :- | :- | :- | :- | :- |
| RPM | 1992 | 2189 | 2411 | 2610 | 2807 | 3007 | 3185 | 3411 | 3605 | 3796 | 4007 | **4482** | **~4950 饱和** |

- **斜率 ≈ 100 RPM/%**（20~45% 段近乎完美线性）
- **fan1 曲线与 fan0 基本重合**（20%→2002 / 30%→3007 / 40%→4000 / 45%→4482）
  ⇒ 两个风扇可独立寻址、增益一致，同一张曲线表可复用到两边
- ⚠ **有效控制范围 = 20%~48%（1992~4950 RPM）**。**48% 以上是死区：60% 与 100% 完全等价**。
  （血案：E1 曾用 `setboth 60` 想验证"与温度解耦"，结果两个风扇都直接冲顶，
   什么都测不出来 —— 任何"手动固定"的测试都必须选**非饱和值**，本脚本用 `THERMAL_PCT=35`）
- 我们真正需要的 **3506~4597 RPM 落在 34%~46%**，正好在有效区内 ✅

**⇒ 结论：GM12 提供了 1992~4950 RPM 的连续可编程区间**，
覆盖并延伸到 BIOS 曲线（1773~3506）之上 —— 最初的需求（可视曲线 / 按温度拉满 / 平滑过渡）全部可编程实现。

---

## 🎯 整合：唯一守护 = `omen-fand`（2026-10-10）

**为什么**：清点时发现**同一个硬件节点有三个写入方**（性能档位 / UI 手动 / `omen-thermal` 分级），
没有仲裁 ⇒ 最后写的赢。风扇更是被 `fan_unthrottle`（两态）和 `fan_speed`（连续）**抢同一个硬件**。

**怎么做**：以 `omen-fand` 为主体整合，按**通道**分工 ——

| 维度 | 谁管 | 通道 | 权限 |
| :- | :- | :- | :- |
| 风扇（连续 20~48% = 1992~4950 RPM） | 曲线 `curve.<档位>` | `fan_speed` | plugdev，**免提权** |
| CPU 上限 / 睿频 / GPU 时钟锁 | 限制 `limit.*` | helper → `intel_pstate` / `nvidia-smi` | **需提权** |

两者由**同一个守护**读**同一份温度**，各走各的通道 ⇒ 天生不会互相覆盖。

```
preset=balanced                              # 基准（用户选的档位）
curve.balanced=60:2200,72:3200,82:4597       # 风扇：连续（唯一来源）
limit.enabled=1                              # 限制：在基准之上收紧
limit.cpu_temp=80                            # CPU 达到 80°C 开始压（50~100，步幅 5）
limit.cpu_pct=62                             # 最多压到 62%（"最低性能"）
limit.gpu_temp=75                            # GPU 达到 75°C 开始锁（50~90，步幅 5）
limit.gpu_mhz=1200                           # 最多锁到 1200 MHz（"最低频率"）
limit.cool_down=4                            # 迟滞带宽
limit.confirm=3                              # 连续 N 次同向才写硬件
```

> **渐进**：达到温度上限后，温度每再升 5°C 多压一档（共 4 档，跨度 15°C），
> 回落时按同一套档位反向恢复。分档而非连续，是为了避免每轮采样都去 fork `sudo helper`。
>
> **风扇不受限制策略影响** —— 限制只写 `intel_pstate` / `nvidia-smi`。
> 守护里原先「临界温度无条件拉满风扇」的兜底已删除（它绕过曲线，且曲线末点本就是满速）。

**语义**：档位是**基准**，限制只在其上**收紧**；温度回落就写回基准
（所以不需要"退出守护才还原"）。`limit.enabled` 默认 **0** —— 没显式打开就一次 helper 都不调。

**预览（不碰硬件）**

```bash
./target/debug/omen-control --fan-daemon --simulate 60,75,79,85,89
```

**已退役**：`omen-thermal`（分级守护，写 `fan_unthrottle` 两态）、
`fan-dynamic.sh`、`thermoguard.sh`、`gpu-thermoguard.sh` 等。
`install-fand.sh` 现在会在安装时检测它们并给出退役命令。

> 完整清单（含 18 个 EC 探索期作废脚本、每项的证据等级）见
> `../docs/功能总清单与整合设计.md`。

---

## 🔬 标定与验收工具：`fan-calibrate.sh`

```bash
sudo bash "/mnt/win_d/新建文件夹/wb_records/omen-control-rs/system/fan-calibrate.sh"
```

7 个阶段（全跑约 25~35 分钟；数据写 `/tmp/fan-calib.tsv`，**agent 直接读，不用粘贴**）：

| 阶段 | 内容 |
| :- | :- |
| **A** | 前提检查 + **下限探测**（找第一个 ≥1800 RPM 的百分比，低于它的点跳过） |
| **B** | 风扇1（`sel=0`）百分比扫描 —— 建立「百分比 → RPM」表 |
| **C** | 风扇2（`sel=1`）百分比扫描 —— 验证两风扇**独立控制** |
| **D** | **100 RPM 梯度**（1800~5000）—— 用 B/C 的表插值出目标 RPM 的百分比，逐个验证误差 |
| **E** | 温控实时 —— 手动固定时应"与温度解耦"，自动时应"跟随温度" |
| **F** | 灵敏度 —— 1% 步长（扫 60~70%，正是我们关心的 3800~4200 RPM 区） |
| **G** | 验收 —— release + 等回落 + 确认 **`fan0 > fan1`** 的正常关系 |

**为什么下限是 1800 RPM**：BIOS 曲线**自己就覆盖 1773~3506**（45°C→1773 / 97°C→3506，随温度连续），
比它更慢的手动转速没有使用价值（只会让机器更热）。
我们真正缺的只有 **3506~4597** 之间的稳态中间值 ⇒ 标定从 1800 起。

**参数**

```bash
--fast              粗扫（约 12 分钟）
--only B,C          只跑指定阶段（A B C D E F G，可逗号分隔）
--grid-step 200     D 阶段梯度步长（默认 100）
--grid-both         D 阶段两个风扇都测（默认只测风扇1）
--load              E 阶段施加 20 秒 CPU 负载以制造温度变化（默认不加）
```

**数据保命（2026-10-10 真机首跑丢数据后加的）**

- 每个测点**写两行**：`<stage>-set`（写入瞬间，`settle_s=0`）+ `<stage>`（稳态）⇒ 中断也留得下
- **事件行**：`watchdog-fan-low` / `watchdog-fan-high` / `watchdog-temp` / `exit`
  ⇒ 看门狗触发或 Ctrl-C 时，tsv 里能直接看出"发生了什么"
- 阶段 A **从 5% 起**（不测 1~4%）：低于 1800 RPM 的点没价值，没必要为"精确下限"停在极低速

**可靠性设计（全部经替身验证）**

- 看门狗：`fan0 < 600`（停转）/ `> 5400`（失控）/ 温度 `> 95°C` → **连续 3 次**才触发，触发即交回固件并终止
  （单次采样会误触发：EC 偶发返回 0 或"上一个偏移"的值；包温空载也会瞬时冲到 93~100）
- `trap EXIT/INT/TERM` ⇒ **任何退出路径**（含 Ctrl-C）都执行「写 0 + unthrottle 1→0」
- 稳态判据：相邻两次采样差 ≤25 RPM 连续 3 次（单点最长等 32 秒）
- 阶段 B/C 开始前先 `setboth 0`，保证**只有一个变量在动**

---

## 🛠 服务化：`install-fand.sh` + `omen-fand.service`

**为什么**：没有服务化时，曲线只是"手动 nohup 着的脚本" —— 关终端就没了、崩溃不重启、
重启机器回到 BIOS 曲线、日志不落盘。

```bash
sudo bash install-fand.sh install     # 安装 + 启动 + 开机自启
sudo bash install-fand.sh status      # 状态 + 当前风扇（不需要 root）
sudo bash install-fand.sh uninstall   # 停止 + 卸载
     bash install-fand.sh follow      # 跟踪日志
```

**unit 的几个关键设计**

| 项 | 值 | 为什么 |
| :- | :- | :- |
| `User=` | 安装时的实际用户（非 root）| daemon 只读温度 + 写 `fan_speed`（0664 root:plugdev）⇒ 最小权限 |
| `Environment=OMEN_CONFIG_DIR=` | 用户真实的配置目录 | ⚠ system 服务里 `$HOME` 是 `/root`，不显式给就会读 `/root/.config/omen-control` ⇒ 用户的曲线"消失" |
| `Restart=on-failure` + `RestartSec=5` | — | 崩溃自动重启 |
| `StartLimitIntervalSec/Burst` | 60 / 3 | 坏配置不会无限重启 |
| `ExecStopPost=` | `printf 0 > fan_speed` | 收尾双保险（覆盖被 SIGKILL、来不及收尾的情况）|
| `ConditionPathExists=` | `fan_speed` | omen-wmi 没加载时直接跳过，不刷日志 |
| `Documentation=` | `/usr/local/share/doc/omen-fand/README.md` | ⚠ **路径必须纯 ASCII** —— 含中文的 `file://` 会被 systemd 判为 `Invalid URL`（实测）|

> ⚠ **GUI 与 daemon 会打架**：「一键满速」（`fan_unthrottle`）与 daemon 的曲线是两条通道。
> 两者同时用会互相拉扯（daemon 每 2 秒把转速拉回曲线值）。
> 目前需手动避免；将来 UI 应在"一键满速"开启时暂停 daemon。

---

## 📉 CPU/GPU 限制的**效果量化**：`limit-bench.sh`

手段早就有（`max_perf_pct` / `no_turbo` / `scaling_max_freq` / PL1 / `-lgc`），
但**从来没量化过**：降几度？代价多少性能？

```bash
sudo bash limit-bench.sh cpu            # 四场景矩阵，约 6 分钟
sudo bash limit-bench.sh cpu --quick    # 约 3 分钟
sudo bash limit-bench.sh gpu            # GPU 时钟锁（需要 glxgears）
```

| 场景 | 看什么 |
| :- | :- |
| ① 基线 | 参考点 |
| ② `max_perf_pct=50` | 温度/功耗/频率 vs 基线 |
| ③ `no_turbo=1` | 同上（只牺牲单核睿频）|
| ④ PL1=45 W | 同上（硬功耗墙）|

**安全设计（全部经替身验证）**：每个场景前先还原成原值（互不叠加）；
`trap ... EXIT INT TERM HUP` ⇒ 任何退出路径都还原；`apply_*` 都做**写后读回校验**。
温度用 **Package**，不用"各核最高"（后者是毛刺指标，实测可差 30°C）。

数据 → `/tmp/limit-bench.tsv`（agent 可直接读）。

---

## 🧹 `fan_speed` 的 `rate=` 字段已移除（2026-10-10）

**旧输出**：`fan0: rate=35 rpm=2782` —— `rate` 取自 GM11 返回的第 0 字节（`FMR`），
**在自动模式下由固件填写，实测恒定 35 而与实际转速无关**。
第一次看到会直接误判成「手动 35% 没交回固件」（我们真踩过：以为出了 P0 故障，
实际风扇是自动的、转速正常跟随温度）。

**新输出**：

```
fan0: rpm=2782 set=auto      ← 未接管（交回固件）
fan0: rpm=3494 set=35%       ← 我们写入了 35%
```

`set` 是本模块**自己记的**设定值（EC 侧没有可读的 `SFS` —— DSDT 里 GM12 只写不读）；
`release` 会把它清零。

---

## 📡 退出时交回固件：bash 与 Rust 的差别（实测）

| 组件 | 机制 | 直接关终端窗口（SIGHUP）|
| :- | :- | :- |
| `system/fan-dynamic.sh` | `trap '_on_exit' EXIT INT TERM HUP` | ✅ 交回（**bash 的 EXIT trap 本就会因致命信号运行**，加 HUP 只是让意图显式）|
| `src/fan_daemon.rs` | `libc::signal(SIGINT/SIGTERM/SIGHUP)` | ✅ 交回（**Rust 默认对 SIGHUP 是直接终止、不走任何收尾** ⇒ 不注册就是真缺口）|

**验证方式（正反对照，都是实测）**：把改动临时移除 → 重新编译 → 重测一遍。

| 场景 | 含 HUP | 移除 HUP |
| :- | :- | :- |
| bash 脚本 | 收尾 ✅ | **收尾 ✅**（EXIT trap 覆盖）|
| Rust daemon | 收尾 ✅ | **无收尾 ❌**（被 HUP 直接杀死）|

⇒ **结论**：同一句「没 trap SIGHUP」，在 bash 里是无害的写法问题，在 Rust 里是真 bug。
**别跨语言套用信号处理的直觉，要正反对照实测。**

---

## 🚨 血案备忘：`fan_speed` 的 `release` 陷阱（2026-10-10 实测）

**症状**：跑完 `fan-speed-test.sh` 后**风扇一直满速不回落**（fan0 4979 / fan1 4878），
`fan_unthrottle` 已是 0 也没用。

**原因**：早期模块的 `release` 实现发的是 GM12 `(0xFF, 0xFF)`，
而 **EC 把 `SFS = 255` 当成「最大速度」** ⇒ 风扇钉在满速。
DSDT 里 **没有任何 ASL 路径**能把 `SFS1/SFS2` 设回自动
（`(0xFF,0xFF)` 分支只清 `FFFS`，不动 SFS；`0` 和 `1..100` 都是有效百分比）。

**实测的值→转速映射**：

| 写入 `fan_speed` | 风扇稳态 | 说明 |
| :- | :- | :- |
| `0` | **1800-2000** | **自动**（EC 自己的曲线）✅ |
| `30` | ~3000 | 手动 30%（**这就是我们一直想要的连续档位**）|
| `100` | ~5020 | 手动满速（DSDT 会同时置 FFFS=1）|
| `255` / 旧版 `release` | **~4990 钉死** | ❌ 不是「交回固件」 |

**一键救命（实测 4979 → 1980 RPM，不需要重启）**：

```bash
echo 0 | sudo tee /sys/devices/platform/omen-wmi/fan_speed
echo 1 | sudo tee /sys/devices/platform/omen-wmi/fan_unthrottle; sleep 1
echo 0 | sudo tee /sys/devices/platform/omen-wmi/fan_unthrottle
```

**验收判据**：等 60~70 秒，`fan0 ≈ 1980`、`fan1 ≈ 1790`，且 **`fan0 > fan1`**。
（正常时 fan0 总比 fan1 高 ~180 RPM；若反转成 `fan1 > fan0`，说明 fan1 没退干净。）

> 模块的 `release` 已修为「写 0 → unthrottle 1 → 0」三连（需重装模块才生效）；
> `fan-speed-test.sh` 的 `release()` 也改成显式两步，**不依赖模块实现**，新旧模块都对。

---

## ⭐ 现在能用的（唯一有效方案）

```bash
# 风扇策略：到阈值 → fan_unthrottle=1（BIOS曲线 ~2000 → 满速 4597 RPM，26 秒斜坡）
sudo bash ".../system/fan-policy.sh" aggressive    [dry]   # CPU≥80 或 GPU≥70 → 拉满
sudo bash ".../system/fan-policy.sh" conservative  [dry]   # CPU≥90 或 GPU≥80 → 拉满
sudo bash ".../system/fan-policy.sh" status / stop
```

| 脚本 | 作用 |
| :- | :- |
| `fan-policy.sh` | **双策略风扇守护**（阈值 + 迟滞），低温度段交给 BIOS 曲线 |
| `thermal-overlay.sh` | 三级散热叠加（档位+风扇+降频），L1/L2 免 sudo、L3 需 sudo |
| `thermal-hold.py` | **占空比控温台**：把 CPU 稳在任意温度（如 70°C）做对比实验，见下 |

---

## ❌ 已经证明做不到的（别再试了）

### 1. 自定义风扇曲线 —— 至今没找到「可写的设定值」

`ec-fan-reverse.sh` + `ec-map.sh` + `ec-target-probe.sh` 的实测结论
（数据在 `/tmp/ec-fan-map.tsv`、`/tmp/ec-map/low.txt`、`/tmp/ec-target-probe.tsv`）：

| EC 偏移 | 实际性质 | 证据 |
| :- | :- | :- |
| `0x2E` / `0x2F` | **当前 PWM 报告（只读）** | 写 0x28/0x30/0x38 都被固件 2 秒内改回，**RPM 完全不动** |
| `0xB0-0xB3` | **当前转速**（小端 16 位） | 234+7×256=2026 vs 实测 2022 ✅ |
| `0x57/0x58/0x59`、`0xB7` | 温度 | 风扇加速后同步下降 |
| `0x63` | timer | — |
| `0xEC` | ⚠ **可写，且真的能驱动风扇** | 写 12 → 风扇 1998→4597（30 秒斜坡）；unthrottle 时也是 0→12 |
| `0x95` | 档位（当前值） | 随 performance_mode 变化 ✅ |

**⇒ 会随转速变化的字节里，只有 `0x2E/0x2F` 与 `0xEC` 值得写；前两者被固件独占。**

### 1b. `0xEC` 的真实身份（2026-10-10 更新，推翻上面那行的旧说法）

```
写 0xEC = 12  →  风扇 1998 → 4597（~30 秒线性斜坡），值保持住 30 秒
fan_unthrottle=1 → 风扇 1980 → 4578（26 秒线性斜坡）
```

**两者终点与过渡方式完全重合 ⇒ `0xEC` 极可能就是「强制满速」那个开关的 EC 端。**
⇒ **没有带来新能力**，除非它能停在中间值。

**已经测过了（2026-10-10，结论在文件开头「已封闭」一节）：`0xEC` 不是档位，是模式标志字节。**
写 6 → 满速（4627）；写 1 → 完全不动。数值大小不决定转速，某个位（bit2）决定。
⇒ **EC 直写控制风扇封版。**

线索：写 `0x06` 被固件改成 `0x0E`，而 **`0x06 | 0x08 = 0x0E`** —— 固件 OR 了 bit3。
但注意 `0x01` 被改成 `0x03`（OR 的是 bit1），**不是固定 OR 同一个位**，所以别只盯 bit3。

**⇒ 若 `map` 判为「开关」，则结论收敛为：风扇控制环完全在固件内部，
  我们够得着的入口只有「BIOS 曲线 + 强制满速」两态。**

> 换算公式仍有效：`RPM ≈ PWM × 49.4`（10 点实测，标准差 0.29）。

### 2. `performance_mode` 不能改变风扇

在**同一温度**（EMA 64~76°C 分箱）下对比三档：

```
温度箱     default      cool        performance
68~70°C    2801(n=7)    2802(n=14)  2797(n=12)
70~72°C    2805(n=7)    2801(n=5)   2795(n=16)
```

**差异 ±6 RPM = 纯噪声。** 连续满载下三档也都是 96-97°C / 忙核 3783~3795 MHz / 风扇 2792~2801。

⇒ 档位改的是 **EC 0x95 标志位**，对风扇与 CPU 温度都**没有可测影响**。

### 3. EC 的 `0x2D/0x2E/0x2F`（WMI，Victus S 专用）

OMEN 8A1A 返 EIO；`fan_speed_victus_s` / `fan_table` 同样。

### 4. `ec_sys`（内核自带）

Secure Boot + `lockdown=integrity` 下 `write_support` 是 *unsafe module parameter*，永远只能只读：

```
Lockdown: insmod: unsafe module parameters is restricted; see man kernel_lockdown.7
ec_sys: `1' invalid for parameter `write_support'
```

⇒ 已改为在 `hp-wmi-omen` 模块里调 `ec_read()`/`ec_write()`（见下）。

---

## 🔬 thermal-hold.py（解决「没法在中等温度下做实验」）

**为什么需要它**：本机在任何真实负载下 **5 秒内就冲到 97°C**，没有中间温度平台
（`profile-curve.sh` 因此失败：升温 +5s 就 97°C，立刻熔断）。
⇒ 用**占空比**（每 0.5s 忙一段、睡一段）+ PI 控温，把温度稳在目标值。

```bash
python3 thermal-hold.py hold 70 60                          # 只保温观察
python3 thermal-hold.py compare 75 default cool performance # 同温度下对比三档
python3 thermal-hold.py fixed 0 0.30 default cool           # 固定占空比（注意坑）
```

⚠ **`fixed` 模式测的是「升温过程中的瞬时温度」**，起点不同结果就不同，
**不能用来比较不同档位**（本轮就因此得出过一次错误结论，已撤回）。
要比档位请用 `compare`（控温）或连续满载。

---

## EC 读写是怎么接上的

**在 `hp-wmi-omen` 模块里直接调内核导出的 `ec_read()` / `ec_write()`**
（`Module.symvers` 里两者都是 `EXPORT_SYMBOL`，没有 `ec_sys` 那层参数门控）：

```
/sys/devices/platform/omen-wmi/ec_byte    权限 0600（仅 root）
  echo 46        > ec_byte     # 选定偏移
  cat ec_byte                  # 读该偏移
  echo 46 36     > ec_byte     # 写：偏移 46 ← 36
```

⚠ 这等于**绕过了内核为 EC 写设的安全闸门** —— 有意为之，代价是没有内核级护栏：
权限 0600、store 侧范围校验、每次写都打 dmesg 日志。
**但结论是：即使能写，EC 里也没有可用的设定值**（见上文）。

---

## 🔧 仍然可用的工具

| 脚本 | 作用 | sudo |
| :- | :- | :- |
| `stress-guard.sh` | 受控压测（绑 P 核，超温熔断） | 是 |
| `diag-load.sh` | 负载真实性诊断（RAPL 功耗 + 占用率） | 是 |
| `thermoguard.sh` | CPU 降频守护（逐核，快照还原） | 是 |
| `gpu-thermoguard.sh` | GPU 锁时钟守护（`-lgc`） | 是 |
| `gpu-load.py` | GPU 负载生成（复用 ComfyUI 的 torch） | 否 |
| `rapl-probe.sh` | 切档位看功耗墙/pstate 是否变化 | 否 |
| `perf-mode-test.sh` | 档位是否下发到固件（判据 EC 0x95）→ **结论：会下发** | 是 |
| `self-test.sh` | 环境自检 | 否 |
| `setup-nopasswd.sh` | sudo 免密 | ⚠ **被 `src/ui.rs` 引用，勿动** |

---

## 📦 历史探索（已完成使命）

| 脚本 | 当时的任务 | 结论去向 |
| :- | :- | :- |
| `fan-curve.sh` / `ec-fan-reverse.sh` | 自定义曲线 / 逆向验证 | → **结论：EC 无可写设定值，路线关闭** |
| `ec-map.sh` | 找与转速同步变化的字节 | → 找出的全是观测值 |
| `profile-curve.sh` | 中等温度下对比三档 | → **失败**：无中间温度平台 → 改用 `thermal-hold.py` |
| `perf-mode-load.sh` | 带负载验证档位 | → 被 `thermal-hold.py compare` 取代 |
| `unthrottle-semantics.sh` | unthrottle 语义 | → 结论：**强制满速 + 26s 软启动斜坡** |
| `sign-ec-sys.sh` | 给 ec_sys 补签 | → 补签成功但 lockdown 拦参数，**已不需要** |
| `ec-probe*.sh` / `find-fan-ec.sh` / `ec-write-*.sh` | EC 探测 | → 已固化进上面结论 |
| `verify-ec-link.sh` / `verify-profile.sh` | 上游 platform_profile | → 8A1A 零影响（白名单无板号） |
| `fan-pwm-write.sh` / `fan-curve-survey.sh` | 早期直写尝试 | → 已被取代 |
| `diagnose.sh` / `omen-control-helper` | 早期诊断/helper | 已取代 |

---

## 一句话

**要控温 → `fan-policy.sh`（风扇）或 `thermal-overlay.sh`（叠加）。**
**想做实验 → `thermal-hold.py`。**
**自定义连续曲线做不到 —— 已用实测排除，别再试。**
