//! 温度限制策略 —— 在「性能档位」基准之上，按温度自动**收紧** CPU 与 GPU。
//!
//! ## 与 `curve.rs` 的分工（整合设计的核心）
//!
//! | 维度 | 谁管 | 通道 | 权限 |
//! | :- | :- | :- | :- |
//! | 风扇（连续） | [`crate::curve`] | `fan_speed` | plugdev，**免提权** |
//! | CPU 上限 / GPU 锁 | **本模块** | helper → `intel_pstate` / `nvidia-smi` | **需提权** |
//!
//! 两者各走各的通道、由**同一个守护**读**同一份温度** ⇒ 天生不会互相覆盖。
//!
//! **本模块绝不碰风扇。** 风扇只有曲线一个来源 —— 用户明确要求
//! 「当前的风扇策略已经够用了」，所以"临界温度无条件拉满风扇"那种兜底已从守护里删除。
//!
//! ## 配置形态
//!
//! ```text
//! limit.enabled=1
//! limit.cpu_temp=80      # CPU 达到 80°C → 收紧（可设 50~100，步幅 5）
//! limit.cpu_pct=62       # 频率/性能上限收到 62%（设 100 = 不动这一项）
//! limit.cpu_w=45         # 同时把 PL1（封装功耗墙）收到 45 W（不设 = 不动）
//! limit.gpu_temp=75      # GPU 达到 75°C → 锁 SM 时钟（可设 50~90，步幅 5）
//! limit.gpu_mhz=1200     # 锁到 1200 MHz
//!
//! ## 为什么要有 `cpu_w`（功耗墙）而不只是 `cpu_pct`（百分比）
//!
//! 本机是 i9-12900HX（16 核 / PL1 135 W）—— **温度是毫秒级跳变的**：
//! 睿频一上来就能在 2 秒内从 60°C 冲到 96°C，而风扇有 ~26 秒软启动惯性，
//! 永远追不上 ⇒ **纯靠风扇调转速必然反复冲顶**（实测日志里 46%/4597 RPM 反复出现）。
//!
//! 从源头压热源才是解法 —— 用户在 Windows 侧就是这么干的：把 CPU 限到 30 W / 40 W。
//! `cpu_pct` 与 `cpu_w` 是**两条独立**的手段，可以只用一个：
//!   - 只想限功耗（照 Windows 的做法）：`cpu_pct=100` + `cpu_w=45`
//!   - 只想限频率：`cpu_w` 不设
//!   - 两个都要：都设
//!
//! **功耗墙不分档**：瓦数是硬墙，分档意义不大，且每档都要提权写 helper。
//! 一触发就收到 `cpu_w`，温度回落就还原到「启动时的值」。
//! limit.cool_down=4      # 低于「触发点 − 它」才回退（迟滞）
//! limit.confirm=3        # 连续 N 次同向才真正写硬件（防抖）
//! ```
//!
//! ## 语义要点
//!
//! - **只朝保守方向**：CPU 上限只降不升，GPU 只锁不放
//! - [`LimitPolicy::target`] 返回 `None` ⇒ 调用方写回**档位基准**（不是"什么都不做"）
//! - [`Limiter`] 负责迟滞与防抖；`LimitPolicy` 本身是纯数据

/// CPU 触发温度的可设下界（°C）。
pub const CPU_TEMP_MIN: u8 = 50;
/// CPU 触发温度的可设上界（°C）。设成 100 基本等于不限制。
pub const CPU_TEMP_MAX: u8 = 100;
/// GPU 触发温度的可设下界（°C）。
pub const GPU_TEMP_MIN: u8 = 50;
/// GPU 触发温度的可设上界（°C）。
pub const GPU_TEMP_MAX: u8 = 90;
/// 温度输入的步幅（°C）—— 仅约束 UI 录入；手写配置只做范围**夹取**，不做对齐。
pub const TEMP_STEP: u8 = 5;
/// 渐进跨度（°C）：从触发温度起，再升高这么多度就压到「最低值」。
///
/// 定 15°C 是按步幅 5 取整数倍 —— 正好分成 4 档（每档 5°C）。
pub const SPAN: u8 = 15;
/// 跨度内分几档。
///
/// **为什么要分档而不是连续**：连续值会让每一轮采样都得到不同的目标值 ⇒
/// 守护每 2 秒都要 fork 一次 `sudo helper` 去写 CPU/GPU（提权代价高、日志刷屏）。
/// 分 4 档后，一次升温过程最多写 4 次，且温度回落时按同样的档位反向恢复。
const STEPS: u32 = 4;

/// CPU 功耗墙（PL1）的可设下界（W）。低于此值系统基本不可用。
pub const CPU_W_MIN: u32 = 5;
/// CPU 功耗墙（PL1）的可设上界（W）。
///
/// 定 200 而不是读 `constraint_0_max_power_uw` —— 那个节点只是 BIOS 报的**能力值**，
/// 不是可写上限（本机它读 55 W，而实际 PL1 一直是 135 W；写入并不 clamp）。
pub const CPU_W_MAX: u32 = 200;

/// `nvidia-smi -lgc` 的下界。NVIDIA 的 SM 最小时钟都是 210 MHz。
pub const GPU_MIN_MHZ: u32 = 210;
/// SM 时钟上限的合理上界（本机 3080 Ti Laptop 最大 2100，留些余量）。
pub const GPU_MAX_MHZ: u32 = 2500;

/// 策略数据本身 —— **纯数据、可解析、可序列化**，不含任何状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitPolicy {
    /// 总开关。默认 **关闭**（不在配置里显式打开就绝不碰提权节点）。
    pub enabled: bool,
    /// CPU 触发温度：`CPU ≥ 它` 就把上限收紧到 `cpu_pct`。`None` = 不启用 CPU 侧。
    pub cpu_temp: Option<u8>,
    /// CPU 上限收紧到多少（%）。**设 100 = 不动这一项**（只限功耗时用）。
    pub cpu_pct: u8,
    /// CPU 功耗墙（PL1，W）：达到 `cpu_temp` 就把封装功耗收到这个值。
    ///
    /// `None` = 不动功耗墙（默认）。这是**与 `cpu_pct` 独立**的第二条降温手段。
    pub cpu_w: Option<u32>,
    /// GPU 触发温度：`GPU ≥ 它` 就把 SM 时钟锁到 `gpu_mhz`。`None` = 不启用 GPU 侧。
    pub gpu_temp: Option<u8>,
    /// GPU SM 时钟锁到多少（MHz）
    pub gpu_mhz: u32,
    /// 迟滞带宽：低于「触发点 − cool_down」才允许回退
    pub cool_down: u8,
    /// 连续多少次同向才真正写硬件
    pub confirm: u8,
}

impl Default for LimitPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            cpu_temp: None,
            cpu_pct: 62,
            cpu_w: None,
            gpu_temp: None,
            gpu_mhz: 1200,
            cool_down: 4,
            confirm: 3,
        }
    }
}

/// 要写到硬件上的「收紧」动作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitAction {
    /// `Some(n)` = 把 CPU 上限收紧到 n%；`None` = 不干预
    pub pct: Option<u32>,
    /// `Some(mhz)` = 锁 GPU SM 上限；`None` = 不锁
    pub gpu_lock: Option<u32>,
    /// `Some(w)` = 把 PL1（封装功耗墙）收到 w W；`None` = 不动
    pub pl1_w: Option<u32>,
}

/// 上一轮"已写进硬件"的状态 —— 迟滞判据需要它。
///
/// `Default` = 当前处于**档位基准态**（没有任何收紧）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prev {
    pub pct: Option<u32>,
    pub gpu_lock: Option<u32>,
    pub pl1_w: Option<u32>,
}

impl LimitPolicy {
    /// 一组"体感合理"的推荐值（UI 首次打开开关时套用它）。
    ///
    /// CPU 80°C → 62%、GPU 75°C → 1200 MHz。
    ///
    /// > ⚠ 这两个值**尚未经真机量化** —— `system/limit-bench.sh` 跑完才能定稿。
    pub fn recommended() -> Self {
        Self {
            enabled: true,
            cpu_temp: Some(80),
            cpu_pct: 62,
            cpu_w: Some(45),
            gpu_temp: Some(75),
            gpu_mhz: 1200,
            cool_down: 4,
            confirm: 3,
        }
    }

    /// 解析一个 `limit.*` 键。返回 `false` 表示不是本模块的键。
    ///
    /// 解析失败**不报错**，只是把该项置空/回默认 —— 脏配置宁可不动硬件。
    pub fn set_key(&mut self, key: &str, val: &str) -> bool {
        let v = val.trim();
        match key {
            "enabled" => {
                self.enabled = matches!(v, "1" | "true" | "yes" | "on");
                true
            }
            "cpu_temp" => {
                self.cpu_temp = parse_temp(v, CPU_TEMP_MIN, CPU_TEMP_MAX);
                true
            }
            "gpu_temp" => {
                self.gpu_temp = parse_temp(v, GPU_TEMP_MIN, GPU_TEMP_MAX);
                true
            }
            "cpu_pct" => {
                // 0% 会让 CPU 基本停摆，直接当非法
                self.cpu_pct = v
                    .parse::<u8>()
                    .ok()
                    .filter(|p| (1..=100).contains(p))
                    .unwrap_or(62);
                true
            }
            "cpu_w" => {
                // 空串 / 0 / 越界 ⇒ 关闭功耗墙（不动它）
                self.cpu_w = v
                    .parse::<u32>()
                    .ok()
                    .filter(|w| (CPU_W_MIN..=CPU_W_MAX).contains(w));
                true
            }
            "gpu_mhz" => {
                self.gpu_mhz = v
                    .parse::<u32>()
                    .ok()
                    .filter(|m| (GPU_MIN_MHZ..=GPU_MAX_MHZ).contains(m))
                    .unwrap_or(1200);
                true
            }
            "cool_down" => {
                self.cool_down = v.parse().unwrap_or(4).min(30);
                true
            }
            "confirm" => {
                self.confirm = v.parse().unwrap_or(3).clamp(1, 60);
                true
            }
            // ── 兼容早期格式 ──
            // 曾用一串「温度:值」多级阈值（如 `78:85,84:75,88:62`）。
            // 现在简化成单一触发点，这里取**最后一点**（最热的那个）迁移过来，
            // 免得老配置一升级就失效。
            "cpu" => {
                if let Some((t, p)) = last_of_pairs(v) {
                    self.cpu_temp = Some(t.clamp(CPU_TEMP_MIN, CPU_TEMP_MAX));
                    if (1..=100).contains(&p) {
                        self.cpu_pct = p as u8;
                    }
                }
                true
            }
            "gpu" => {
                if let Some((t, m)) = last_of_pairs(v) {
                    self.gpu_temp = Some(t.clamp(GPU_TEMP_MIN, GPU_TEMP_MAX));
                    if (GPU_MIN_MHZ..=GPU_MAX_MHZ).contains(&m) {
                        self.gpu_mhz = m;
                    }
                }
                true
            }
            // 已移除的旧能力：静默忽略（老配置不报错）
            "turbo_off_at" => true,
            _ => false,
        }
    }

    /// 序列化成 `(键, 值)` 列表，交给 `save_config` 写文件。
    pub fn encode_into(&self, out: &mut Vec<(String, String)>) {
        out.push(("limit.enabled".into(), u8::from(self.enabled).to_string()));
        if let Some(t) = self.cpu_temp {
            out.push(("limit.cpu_temp".into(), t.to_string()));
        }
        out.push(("limit.cpu_pct".into(), self.cpu_pct.to_string()));
        if let Some(w) = self.cpu_w {
            out.push(("limit.cpu_w".into(), w.to_string()));
        }
        if let Some(t) = self.gpu_temp {
            out.push(("limit.gpu_temp".into(), t.to_string()));
        }
        out.push(("limit.gpu_mhz".into(), self.gpu_mhz.to_string()));
        out.push(("limit.cool_down".into(), self.cool_down.to_string()));
        out.push(("limit.confirm".into(), self.confirm.to_string()));
    }

    /// **纯函数**：当前温度 → 该收紧到什么程度。
    ///
    /// - `None` ⇒ 不需要任何收紧，调用方写回**档位基准**
    /// - `Some(action)` ⇒ 按 action 里非空字段覆盖基准
    ///
    /// `base_pct` 是**当前档位**给的 CPU 上限（如 eco=40 / balanced=100）。
    /// ⇒ 本策略只在它之上**收紧**：目标值取 `min(base_pct, cpu_pct)`，
    ///   绝不会把 eco 的 40% 放宽到 62%（守护若放宽就走反了方向）。
    ///
    /// `prev` 用于迟滞：已经处在收紧状态时，要温度低于 `触发点 − cool_down`
    /// 才会放行回退，避免在阈值上反复横跳。
    pub fn target(
        &self,
        cpu: u8,
        gpu: Option<u8>,
        base_pct: u32,
        prev: &Prev,
    ) -> Option<LimitAction> {
        if !self.enabled {
            return None;
        }

        // ── CPU ──
        let mut pct = None;
        if let Some(t) = self.cpu_temp {
            let trig = trig_of(t, prev.pct.is_some(), self.cool_down);
            if cpu >= trig {
                let floor = (self.cpu_pct as u32).min(base_pct); // 只收紧，不放宽
                let mut v = ramp(base_pct, floor, cpu - trig);
                // ⚠ 也不得比「当前已压到的」更宽松：温度在两个档位之间波动时，
                //   ramp 会算出更大的值 ⇒ 不加这一层就会来回写（实测由单测抓出）。
                //   回退唯一入口是 `cpu < trig`（此时上面不进入分支，返回 None ⇒ 回基准）。
                if let Some(cur) = prev.pct {
                    v = v.min(cur);
                }
                pct = Some(v);
            }
        }

        // ── CPU 功耗墙（PL1）──
        // 与上面的 `cpu_pct` **共用触发温度**，但**不分档**：
        // 瓦数是硬墙，分档意义不大，而且每档都要提权写 helper。
        let mut pl1_w = None;
        if let (Some(t), Some(w)) = (self.cpu_temp, self.cpu_w) {
            let trig = trig_of(t, prev.pl1_w.is_some(), self.cool_down);
            if cpu >= trig {
                pl1_w = Some(w);
            }
        }

        // ── GPU：同理（取不到温度就整体跳过，不影响 CPU 判定）──
        let mut gpu_lock = None;
        if let (Some(t), Some(g)) = (self.gpu_temp, gpu) {
            let trig = trig_of(t, prev.gpu_lock.is_some(), self.cool_down);
            if g >= trig {
                let floor = self.gpu_mhz.min(GPU_MAX_MHZ);
                let mut v = ramp(GPU_MAX_MHZ, floor, g - trig);
                if let Some(cur) = prev.gpu_lock {
                    v = v.min(cur); // 同上：只朝收紧方向走
                }
                gpu_lock = Some(v);
            }
        }

        if pct.is_none() && gpu_lock.is_none() && pl1_w.is_none() {
            return None;
        }
        Some(LimitAction { pct, gpu_lock, pl1_w })
    }
}

/// 带状态的决策器：把 `target()` 的瞬时输出变成"要不要真的写硬件"。
///
/// 作用：**防抖** —— 温度在阈值附近抖动时，必须连续 `confirm` 次都指向同一个新状态才放行。
#[derive(Debug)]
pub struct Limiter {
    policy: LimitPolicy,
    /// 已经写进硬件的状态（`None` = 基准态）
    applied: Option<LimitAction>,
    /// 正在攒的候选
    candidate: Option<LimitAction>,
    streak: u8,
}

/// 一次 `update()` 的结论。
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// 需要写硬件。`None` = 写回档位基准。
    Apply(Option<LimitAction>),
    /// 还不到时候（攒确认次数中）。
    Hold,
}

impl Limiter {
    pub fn new(policy: LimitPolicy) -> Self {
        Self {
            policy,
            applied: None,
            candidate: None,
            streak: 0,
        }
    }

    /// 喂一次温度，返回该不该动硬件。
    ///
    /// `base_pct` = 当前档位给的 CPU 上限（守护每轮现读，因为档位会变）。
    pub fn update(&mut self, cpu: u8, gpu: Option<u8>, base_pct: u32) -> Step {
        let prev = Prev {
            pct: self.applied.as_ref().and_then(|a| a.pct),
            gpu_lock: self.applied.as_ref().and_then(|a| a.gpu_lock),
            pl1_w: self.applied.as_ref().and_then(|a| a.pl1_w),
        };
        let want = self.policy.target(cpu, gpu, base_pct, &prev);

        if want == self.applied {
            self.candidate = None;
            self.streak = 0;
            return Step::Hold;
        }

        if self.candidate == want {
            self.streak = self.streak.saturating_add(1);
        } else {
            self.candidate = want.clone();
            self.streak = 1;
        }

        if self.streak >= self.policy.confirm.max(1) {
            self.applied = want.clone();
            self.candidate = None;
            self.streak = 0;
            Step::Apply(want)
        } else {
            Step::Hold
        }
    }
}

// ─────────────────────────────────────────────────────── 渐进

/// 触发点：已经处在收紧态时，要低于 `t − cool_down` 才允许回退。
fn trig_of(t: u8, engaged: bool, cool_down: u8) -> u8 {
    if engaged {
        t.saturating_sub(cool_down)
    } else {
        t
    }
}

/// 从 `high`（基准）线性过渡到 `low`（最低值），`over` = 超过触发点的度数。
///
/// 分 `STEPS` 档量化（见 `STEPS` 的注释：避免连续值导致每轮都写硬件）。
/// 刚超过触发点就落到第 1 档（`over = 0` 也返回一个已经收紧的值），
/// 到 `SPAN` 及以上即为 `low`。
fn ramp(high: u32, low: u32, over: u8) -> u32 {
    let over = over.min(SPAN) as f64;
    // ceil ⇒ over=0 时也是第 1 档（立刻开始压一点，而不是等到 5°C 才动）
    let step = ((over / SPAN as f64) * STEPS as f64).ceil().max(1.0) as u32;
    let f = step as f64 / STEPS as f64;
    let v = high as f64 - (high.saturating_sub(low)) as f64 * f;
    (v.round() as u32).clamp(1, 100_000)
}

// ─────────────────────────────────────────────────────── 解析

/// 解析温度：必须是纯数字，越界时**夹取**而不是丢弃
/// （用户手写 `cpu_temp=110` 时更希望它能用，而不是静默失效）。
fn parse_temp(v: &str, lo: u8, hi: u8) -> Option<u8> {
    Some(v.parse::<u8>().ok()?.clamp(lo, hi))
}

/// 从 `温度:值,温度:值,...` 里取最后一项（兼容旧的多级配置）。
///
/// ⚠ 第二段必须是 `u32`：旧配置里 GPU 的值是 **MHz**（`1200`/`1500`），
/// 用 `u8` 解析会**整项失败**，表现为「GPU 侧静默失效」—— 实测踩过。
fn last_of_pairs(s: &str) -> Option<(u8, u32)> {
    s.split(',')
        .filter_map(|tok| {
            let (a, b) = tok.trim().split_once(':')?;
            Some((a.trim().parse::<u8>().ok()?, b.trim().parse::<u32>().ok()?))
        })
        .next_back()
}

// ─────────────────────────────────────────────────────── 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn pol(pairs: &[(&str, &str)]) -> LimitPolicy {
        let mut p = LimitPolicy {
            enabled: true,
            ..Default::default()
        };
        for (k, v) in pairs {
            assert!(p.set_key(k, v), "未识别的键 {k}");
        }
        p
    }

    #[test]
    fn default_is_disabled_and_does_nothing() {
        let p = LimitPolicy::default();
        assert!(!p.enabled);
        // 关闭时必须返回 None —— 绝不能在用户没开的时候去写提权节点
        assert_eq!(p.target(99, Some(99), 100, &Prev::default()), None);
    }

    #[test]
    fn temp_range_constants_match_spec() {
        // 用户明确要求：CPU 50~100、GPU 50~90、步幅 5
        assert_eq!((CPU_TEMP_MIN, CPU_TEMP_MAX), (50, 100));
        assert_eq!((GPU_TEMP_MIN, GPU_TEMP_MAX), (50, 90));
        assert_eq!(TEMP_STEP, 5);
        // 上下界都落在步幅网格上，UI 的 SpinRow 才不会出现"起点不是 5 的倍数"
        assert_eq!(CPU_TEMP_MIN % TEMP_STEP, 0);
        assert_eq!((CPU_TEMP_MAX - CPU_TEMP_MIN) % TEMP_STEP, 0);
        assert_eq!((GPU_TEMP_MAX - GPU_TEMP_MIN) % TEMP_STEP, 0);
    }

    #[test]
    fn below_trigger_is_none() {
        let p = pol(&[("cpu_temp", "80"), ("gpu_temp", "75")]);
        assert_eq!(p.target(79, Some(74), 100, &Prev::default()), None);
    }

    #[test]
    fn at_trigger_engages() {
        let p = pol(&[("cpu_temp", "80"), ("cpu_pct", "62")]);
        let a = p.target(80, None, 100, &Prev::default()).unwrap();
        // 渐进：刚触发只是第 1 档，不是直接到底
        assert_eq!(a.pct, Some(ramp(100, 62, 0)));
        assert!(a.pct.unwrap() < 100 && a.pct.unwrap() > 62);
        assert_eq!(a.gpu_lock, None);
    }

    #[test]
    fn gpu_only_hot_does_not_touch_cpu() {
        let p = pol(&[("cpu_temp", "80"), ("gpu_temp", "75"), ("gpu_mhz", "1200")]);
        let a = p.target(50, Some(76), 100, &Prev::default()).unwrap();
        assert_eq!(a.pct, None, "CPU 不热就不该动它");
        assert_eq!(a.gpu_lock, Some(ramp(GPU_MAX_MHZ, 1200, 1)));
    }

    #[test]
    fn gpu_absent_is_tolerated() {
        let p = pol(&[("cpu_temp", "80"), ("cpu_pct", "62"), ("gpu_temp", "75")]);
        // 取不到 GPU 温度时，GPU 规则整体跳过，不影响 CPU 判定
        let a = p.target(81, None, 100, &Prev::default()).unwrap();
        assert_eq!(a.pct, Some(ramp(100, 62, 1)));
        assert_eq!(a.gpu_lock, None);
    }

    #[test]
    fn one_side_can_be_off() {
        let p = pol(&[("cpu_temp", "80")]);
        assert!(p.target(99, Some(99), 100, &Prev::default()).unwrap().gpu_lock.is_none());
        let q = pol(&[("gpu_temp", "75")]);
        assert!(q.target(99, Some(99), 100, &Prev::default()).unwrap().pct.is_none());
    }

    #[test]
    fn ramp_is_stepped_and_reaches_floor() {
        // over=0 就是第 1 档；到 SPAN 即到底
        assert_eq!(ramp(100, 60, 0), 90, "100→60 的第 1 档 = 100 - 40/4");
        assert_eq!(ramp(100, 60, SPAN), 60);
        assert_eq!(ramp(100, 60, 200), 60, "超过跨度仍到底，不越界");
        // 单调不增
        let seq: Vec<u32> = (0..=SPAN).map(|o| ramp(100, 60, o)).collect();
        assert!(seq.windows(2).all(|w| w[0] >= w[1]), "渐进必须单调递减: {seq:?}");
        // 量化成有限的几个值（避免每轮都写硬件）
        let uniq: std::collections::BTreeSet<u32> = (0..=SPAN).map(|o| ramp(100, 60, o)).collect();
        assert!(uniq.len() <= 4, "最多 4 档，实得 {uniq:?}");
    }

    #[test]
    fn never_loosens_below_base() {
        // 档位是 eco（40%），而限制写的是 62% ⇒ 必须取 40，不能放宽到 62
        let p = pol(&[("cpu_temp", "80"), ("cpu_pct", "62")]);
        let a = p.target(99, None, 40, &Prev::default()).unwrap();
        assert_eq!(a.pct, Some(40), "限制只能收紧，不能把 eco 的 40% 放宽");
    }

    #[test]
    fn ramps_within_base_and_floor() {
        let p = pol(&[("cpu_temp", "80"), ("cpu_pct", "60")]);
        // 刚触发：在 100 与 60 之间的第 1 档
        let a = p.target(80, None, 100, &Prev::default()).unwrap();
        let v = a.pct.unwrap();
        assert!(v < 100 && v > 60, "刚触发就该开始压，但还没到底：{v}");
        // 到跨度以上：到底
        let b = p.target(80 + SPAN, None, 100, &Prev::default()).unwrap();
        assert_eq!(b.pct, Some(60));
    }

    #[test]
    fn hysteresis_needs_lower_temp_to_back_off() {
        let p = pol(&[("cpu_temp", "80"), ("cool_down", "4")]);
        // 基准态 ⇒ 79°C 不该触发
        assert!(p.target(79, None, 100, &Prev::default()).is_none());
        // 已收紧 ⇒ 79°C 仍保持（80-4=76 ≤ 79，迟滞不允许回退）
        let prev = Prev {
            pct: Some(62),
            gpu_lock: None,
            pl1_w: None,
        };
        assert_eq!(p.target(79, None, 100, &prev).unwrap().pct, Some(62));
        // 降到 75°C（< 76）才允许回退
        assert!(p.target(75, None, 100, &prev).is_none());
    }

    #[test]
    fn temp_is_clamped_not_dropped() {
        let p = pol(&[("cpu_temp", "130"), ("gpu_temp", "10")]);
        assert_eq!(p.cpu_temp, Some(CPU_TEMP_MAX), "越界应夹取到上界");
        assert_eq!(p.gpu_temp, Some(GPU_TEMP_MIN), "越界应夹取到下界");
    }

    #[test]
    fn bad_numbers_fall_back_to_defaults() {
        let p = pol(&[("cpu_temp", "abc"), ("cpu_pct", "0"), ("gpu_mhz", "99999")]);
        assert_eq!(p.cpu_temp, None);
        assert_eq!(p.cpu_pct, 62, "0% 非法，回默认");
        assert_eq!(p.gpu_mhz, 1200, "超范围回默认");
    }

    #[test]
    fn legacy_multilevel_config_migrates_to_last_point() {
        // 旧格式：多级「温度:值」——取最后一点作为触发温度
        let mut p = LimitPolicy::default();
        assert!(p.set_key("cpu", "78:85,84:75,88:62"));
        assert_eq!(p.cpu_temp, Some(88));
        assert_eq!(p.cpu_pct, 62);
        let mut q = LimitPolicy::default();
        assert!(q.set_key("gpu", "70:1500,78:1200"));
        // ⚠ 必须断言 gpu_temp —— 只断言 gpu_mhz 抓不到 bug：
        //   默认值恰好就是 1200，即便整项迁移失败也会"通过"（实测踩过）。
        assert_eq!(q.gpu_temp, Some(78), "旧配置的触发温度必须迁移过来");
        assert_eq!(q.gpu_mhz, 1200);
    }

    #[test]
    fn legacy_gpu_mhz_four_digits_survives() {
        // 血案：旧格式的 GPU 值是 4 位数 MHz，用 u8 解析会整项失败 ⇒ GPU 静默失效
        let mut p = LimitPolicy {
            enabled: true, // 必须开 —— `target()` 在关闭时恒返回 None（unwrap 会 panic）
            ..LimitPolicy::default()
        };
        assert!(p.set_key("gpu", "70:1500,78:1200"));
        assert_eq!(p.gpu_temp, Some(78));
        assert_eq!(p.gpu_mhz, 1200);
        assert_eq!(
            p.target(50, Some(80), 100, &Prev::default()).unwrap().gpu_lock,
            Some(ramp(GPU_MAX_MHZ, 1200, 2)) // 80°C、阈值 78 ⇒ 超 2°C
        );
    }

    #[test]
    fn cpu_w_engages_at_trigger_and_is_not_graded() {
        // 只限功耗：cpu_pct=100（不动频率）+ cpu_w=45（收功耗墙）
        let p = pol(&[("cpu_temp", "80"), ("cpu_pct", "100"), ("cpu_w", "45")]);
        assert_eq!(p.cpu_w, Some(45));

        // 未达触发温度 ⇒ 完全不干预（功耗墙也不动）
        assert!(p.target(70, None, 100, &Prev::default()).is_none());

        // 达到触发温度 ⇒ 收功耗墙；且**不分档**（一触发就到目标值）
        let a = p.target(80, None, 100, &Prev::default()).unwrap();
        assert_eq!(a.pl1_w, Some(45));

        // 温度更高也还是 45（不分档，与 cpu_pct 的渐进不同）
        let b = p.target(95, None, 100, &Prev::default()).unwrap();
        assert_eq!(b.pl1_w, Some(45));
    }

    #[test]
    fn cpu_w_absent_means_do_not_touch_pl1() {
        let p = pol(&[("cpu_temp", "80"), ("cpu_pct", "62")]);
        assert_eq!(p.cpu_w, None, "不设 cpu_w 就不该碰功耗墙");
        let a = p.target(95, None, 100, &Prev::default()).unwrap();
        assert_eq!(a.pl1_w, None);
    }

    #[test]
    fn cpu_w_out_of_range_is_dropped() {
        let mut p = LimitPolicy::default();
        assert!(p.set_key("cpu_w", "45"));
        assert_eq!(p.cpu_w, Some(45));
        assert!(p.set_key("cpu_w", "0"));   // 0 视为关闭
        assert_eq!(p.cpu_w, None);
        assert!(p.set_key("cpu_w", "9999")); // 越界也关闭
        assert_eq!(p.cpu_w, None);
        assert!(p.set_key("cpu_w", "abc")); // 非数字同样关闭
        assert_eq!(p.cpu_w, None);
    }

    #[test]
    fn legacy_turbo_key_is_ignored_not_rejected() {
        let mut p = LimitPolicy::default();
        assert!(p.set_key("turbo_off_at", "88"), "旧键必须被识别（静默忽略）");
        assert_eq!(p, LimitPolicy::default(), "忽略即状态不变");
    }

    #[test]
    fn encode_roundtrip() {
        let a = pol(&[
            ("cpu_temp", "80"),
            ("cpu_pct", "62"),
            ("gpu_temp", "75"),
            ("gpu_mhz", "1200"),
            ("cool_down", "5"),
            ("confirm", "2"),
        ]);
        let mut kv = Vec::new();
        a.encode_into(&mut kv);
        let mut b = LimitPolicy::default();
        for (k, v) in &kv {
            let key = k.strip_prefix("limit.").unwrap();
            assert!(b.set_key(key, v), "未识别的键 {k}");
        }
        assert_eq!(a, b);
    }

    #[test]
    fn missing_side_is_not_encoded() {
        let p = pol(&[("cpu_temp", "80")]);
        let mut kv = Vec::new();
        p.encode_into(&mut kv);
        assert!(
            !kv.iter().any(|(k, _)| k == "limit.gpu_temp"),
            "未启用的一侧不写文件"
        );
    }

    #[test]
    fn limiter_requires_confirm_before_writing() {
        let p = pol(&[("cpu_temp", "80"), ("cpu_pct", "62"), ("confirm", "3")]);
        let mut lim = Limiter::new(p);
        assert_eq!(lim.update(85, None, 100), Step::Hold);
        assert_eq!(lim.update(85, None, 100), Step::Hold);
        match lim.update(85, None, 100) {
            // 85°C、阈值 80 ⇒ 超 5°C ⇒ 第 2 档
            Step::Apply(Some(a)) => assert_eq!(a.pct, Some(ramp(100, 62, 5))),
            other => panic!("期望 Apply，实得 {other:?}"),
        }
        // 之后保持稳定就不再写
        assert_eq!(lim.update(85, None, 100), Step::Hold);
    }

    #[test]
    fn limiter_resets_streak_when_target_flipflops() {
        let p = pol(&[("cpu_temp", "80"), ("confirm", "3")]);
        let mut lim = Limiter::new(p);
        for _ in 0..4 {
            assert_eq!(lim.update(85, None, 100), Step::Hold);
            assert_eq!(lim.update(70, None, 100), Step::Hold);
        }
    }

    #[test]
    fn limiter_applies_baseline_after_cooling() {
        let p = pol(&[("cpu_temp", "80"), ("confirm", "1")]);
        let mut lim = Limiter::new(p);
        assert!(matches!(lim.update(85, None, 100), Step::Apply(Some(_))));
        // 温度降到迟滞带以下 ⇒ 放行「写回基准」
        match lim.update(70, None, 100) {
            Step::Apply(None) => {}
            other => panic!("期望 Apply(None)（回基准），实得 {other:?}"),
        }
    }

    #[test]
    fn recommended_is_sane_and_enabled() {
        let r = LimitPolicy::recommended();
        assert!(r.enabled);
        let (ct, gt) = (r.cpu_temp.unwrap(), r.gpu_temp.unwrap());
        assert!((CPU_TEMP_MIN..=CPU_TEMP_MAX).contains(&ct));
        assert!((GPU_TEMP_MIN..=GPU_TEMP_MAX).contains(&gt));
        assert!(r.cpu_pct > 0 && r.cpu_pct <= 100);
        assert!((GPU_MIN_MHZ..=GPU_MAX_MHZ).contains(&r.gpu_mhz));
    }
}
