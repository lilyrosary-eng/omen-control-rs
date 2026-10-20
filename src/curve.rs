//! 风扇曲线的数据模型 + 实测标定换算。
//!
//! # 依据（2026-10-10 真机实测，见 `system/README.md`）
//!
//! 通道是 DSDT 反编译出来的 WMI 命令 `GM12`（sysfs `fan_speed`）：
//!
//! | 写入 | 结果 |
//! |---|---|
//! | `0` | **交回固件**（BIOS 曲线，安静平滑）——注意不是"0 转速" |
//! | `1..100` | 手动百分比 |
//! | `0xFF` | EC 当作"最大速度"，风扇钉死满速且 **DSDT 里没有退路** ⇒ 本模块禁止 |
//!
//! 20%~48% 段**近线性**（17 个测点，斜率 ≈ 100 RPM/%），48% 以上是饱和死区
//! （60% 与 100% 完全等价）。手动模式**与温度完全解耦**
//! （实测温度 39~47°C 波动时 RPM 恒定 3500±15）。
//!
//! # 为什么反查要用实测表而不是 `rpm / 100`
//!
//! 低区是**断崖**：实测 5% → 87 RPM、20% → 1992 RPM。全局线性外推会严重高估低区转速
//! （按 30%→3007 / 100%→5020 外推会得到"1% ≈ 2160"，而实测 5% 只有 87）。
//! 所以必须对实测点做**分段线性插值**，并把下限夹到 [`FanCurve::MIN_RPM`]。

/// 曲线上的一个点：温度(°C) → 目标转速(RPM)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CurvePoint {
    pub temp: u8,
    pub rpm: u16,
}

/// 一条风扇曲线。
///
/// 点按 `temp` 升序保存。**空曲线表示"不接管"**——此时守护会把控制权交回固件。
///
/// 语义：温度低于首点时**交回固件**（而不是给一个很低的转速）——
/// 因为低区是断崖（5% 就只剩 87 RPM），"前面平滑"这一段 BIOS 做得比我们好。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FanCurve {
    pub points: Vec<CurvePoint>,
}

impl FanCurve {
    /// 点数上限（配置里一行放得下，UI 也好画）。
    pub const MAX_POINTS: usize = 12;

    /// 最低可控转速 —— 对应 20%。低于它没有使用价值（BIOS 曲线已覆盖 1773~3506，
    /// 更慢的手动转速只会让机器更热）。
    pub const MIN_RPM: u16 = 1992;

    /// 建议的满速目标 —— 对应 46%。再往上（48%+）是饱和死区，写多少都一样。
    pub const MAX_RPM: u16 = 4597;

    pub const MIN_TEMP: u8 = 30;
    pub const MAX_TEMP: u8 = 100;

    /// 从 `50:2000,60:2400` 解析。
    ///
    /// **非法项直接跳过**（而不是整体失败）——配置文件被手改坏了不该让程序起不来，
    /// 这与 `hw::load_config` 的既有约定一致。
    pub fn parse(s: &str) -> Self {
        let mut c = FanCurve::default();
        for item in s.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let Some((t, r)) = item.split_once(':') else {
                continue;
            };
            let (Ok(t), Ok(r)) = (t.trim().parse::<u8>(), r.trim().parse::<u16>()) else {
                continue;
            };
            c.points.push(CurvePoint { temp: t, rpm: r });
        }
        c.normalize();
        c
    }

    /// 序列化成 `50:2000,60:2400`。
    pub fn encode(&self) -> String {
        self.points
            .iter()
            .map(|p| format!("{}:{}", p.temp, p.rpm))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// 规范化：按温度排序 → 合并同温度（取后写入的）→ 夹取范围 → 截断点数。
    pub fn normalize(&mut self) {
        self.points
            .retain(|p| (Self::MIN_TEMP..=Self::MAX_TEMP).contains(&p.temp));
        for p in &mut self.points {
            p.rpm = p.rpm.clamp(Self::MIN_RPM, Self::MAX_RPM);
        }
        // `sort_by_key` 是稳定排序 ⇒ 同温度的点保持"谁先来谁在前"，
        // 于是下面这一趟"同温度用后者覆盖"就等于"后写入的优先"。
        // ⚠ 不能用 `dedup_by_key`：它保留的是**第一个**，与文档语义相反。
        self.points.sort_by_key(|p| p.temp);
        let mut merged: Vec<CurvePoint> = Vec::with_capacity(self.points.len());
        for p in self.points.drain(..) {
            match merged.last_mut() {
                Some(last) if last.temp == p.temp => *last = p,
                _ => merged.push(p),
            }
        }
        self.points = merged;
        self.points.truncate(Self::MAX_POINTS);
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// 该温度下的目标转速。
    ///
    /// - 低于首点温度 → `None`（**交回固件**，不是给低转速）
    /// - 高于末点温度 → 末点的转速
    /// - 中间 → 相邻两点线性插值
    pub fn target_rpm(&self, temp: u8) -> Option<u16> {
        let first = self.points.first()?;
        if temp < first.temp {
            return None;
        }
        let last = self.points.last()?;
        if temp >= last.temp {
            return Some(last.rpm);
        }
        for w in self.points.windows(2) {
            let (a, b) = (w[0], w[1]);
            if temp >= a.temp && temp <= b.temp {
                // 温度区间为 0 的情况已被 dedup_by_key 消除
                let span = (b.temp - a.temp) as u32;
                let step = (b.rpm as u32 - a.rpm as u32) * (temp - a.temp) as u32 / span;
                return Some((a.rpm as u32 + step) as u16);
            }
        }
        Some(last.rpm)
    }
}

/// 实测标定表：`fan_speed` 百分比 → 转速（真机 2026-10-10，13 个稳态测点）。
///
/// 45% 之后直接跳到 50% 是为了覆盖饱和拐点；`48%+` 一律饱和到 ~4950。
const CAL: [(u8, u16); 13] = [
    (20, 1992),
    (22, 2189),
    (24, 2411),
    (26, 2610),
    (28, 2807),
    (30, 3007),
    (32, 3185),
    (34, 3411),
    (36, 3605),
    (38, 3796),
    (40, 4007),
    (45, 4482),
    (50, 4979),
];

/// 目标转速 → `fan_speed` 百分比（对**实测标定表**做分段线性插值）。
///
/// 越界会夹到 `[20, 50]`：低于 20% 是断崖区（实测 5% → 87 RPM），
/// 高于 50% 是饱和区（写多少都一样）。
pub fn rpm_to_pct(rpm: u16) -> u8 {
    let r = rpm as i32;
    let (lo_pct, lo_rpm) = CAL[0];
    let (hi_pct, hi_rpm) = CAL[CAL.len() - 1];
    if r <= lo_rpm as i32 {
        return lo_pct;
    }
    if r >= hi_rpm as i32 {
        return hi_pct;
    }
    for w in CAL.windows(2) {
        let ((pa, ra), (pb, rb)) = (w[0], w[1]);
        let (ra, rb) = (ra as i32, rb as i32);
        if r >= ra && r <= rb {
            if rb == ra {
                return pa;
            }
            let step = (pb as i32 - pa as i32) * (r - ra) / (rb - ra);
            return (pa as i32 + step).clamp(lo_pct as i32, hi_pct as i32) as u8;
        }
    }
    hi_pct
}

/// 内置预设各自的**默认曲线**（用户可以改，这只是个起点）。
///
/// 它们都遵守两条硬约束：端点不越过 [`FanCurve::MIN_RPM`] / [`FanCurve::MAX_RPM`]，
/// 且首点温度就是"开始接管"的温度——低于它交回固件，保住 BIOS 那段平滑。
pub fn default_curve(preset_key: &str) -> FanCurve {
    let raw = match preset_key {
        // 省电：完全不接管。最凉最安静，风扇全交给 BIOS。
        "eco" => "",

        // 安静：晚接管、上限也压着，只在真的热起来时才出手。
        "quiet" => "65:1992,78:2800,88:4597",

        // 均衡：出厂那一档，接管早一点、爬得快一点。
        "balanced" => "60:2200,72:3200,82:4597",

        // 性能：最早接管、最快爬满——用户要的"80°C 拉满"就落在这一条上。
        "performance" => "55:2400,70:3800,80:4597",

        _ => "",
    };
    FanCurve::parse(raw)
}

/// 所有内置预设 key（与 `hw::PRESETS` 保持一致）。
pub const PRESET_KEYS: [&str; 4] = ["eco", "quiet", "balanced", "performance"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_encode_roundtrip() {
        let c = FanCurve::parse("70:3000, 50:2000 ,60:2400");
        assert_eq!(c.encode(), "50:2000,60:2400,70:3000"); // 解析时已排序
        assert_eq!(FanCurve::parse(&c.encode()), c);
    }

    #[test]
    fn parse_skips_garbage() {
        let c = FanCurve::parse("垃圾,50:2000,,xx:yy,70:abc,60:2400");
        assert_eq!(c.encode(), "50:2000,60:2400");
        assert!(FanCurve::parse("").is_empty());
    }

    #[test]
    fn parse_clamps_and_dedups() {
        // 低于 MIN_RPM 被抬到 MIN_RPM；同温度只留一个
        let c = FanCurve::parse("50:100,50:5000,120:3000,10:3000");
        assert_eq!(c.points.len(), 1);
        assert_eq!(c.points[0].temp, 50);
        assert_eq!(c.points[0].rpm, FanCurve::MAX_RPM);
    }

    #[test]
    fn same_temp_last_wins() {
        // 后写入的覆盖先写入的（文档语义）；与 dedup_by_key 的"保留首个"相反
        let c = FanCurve::parse("50:2100,60:2600,50:3000");
        assert_eq!(c.points.len(), 2);
        assert_eq!(c.encode(), "50:3000,60:2600");
    }

    #[test]
    fn target_below_first_is_none() {
        let c = FanCurve::parse("60:2200,80:4597");
        assert_eq!(c.target_rpm(40), None); // 交回固件
        assert_eq!(c.target_rpm(59), None);
        assert_eq!(c.target_rpm(60), Some(2200));
    }

    #[test]
    fn target_interpolates() {
        let c = FanCurve::parse("50:2000,70:3000");
        assert_eq!(c.target_rpm(60), Some(2500));
        assert_eq!(c.target_rpm(70), Some(3000));
        assert_eq!(c.target_rpm(99), Some(3000)); // 末点封顶
    }

    #[test]
    fn rpm_to_pct_matches_measured_table() {
        assert_eq!(rpm_to_pct(1992), 20);
        assert_eq!(rpm_to_pct(3007), 30);
        assert_eq!(rpm_to_pct(3411), 34);
        assert_eq!(rpm_to_pct(4007), 40);
        assert_eq!(rpm_to_pct(4482), 45);
        assert_eq!(rpm_to_pct(4597), 46);
        // 越界夹取：低区不能外推到断崖里
        assert_eq!(rpm_to_pct(0), 20);
        assert_eq!(rpm_to_pct(500), 20);
        assert_eq!(rpm_to_pct(4979), 50); // 标定表末点，正好对应 50%
        assert_eq!(rpm_to_pct(4950), 49); // 4482~4979 之间线性插值
    }

    #[test]
    fn every_default_curve_is_sane() {
        for k in PRESET_KEYS {
            let c = default_curve(k);
            if k == "eco" {
                assert!(c.is_empty(), "eco 应当不接管");
                continue;
            }
            assert!(!c.is_empty());
            for p in &c.points {
                assert!((FanCurve::MIN_RPM..=FanCurve::MAX_RPM).contains(&p.rpm));
                // 每个点都应当能反查回一个有效百分比
                let pct = rpm_to_pct(p.rpm);
                assert!((20..=50).contains(&pct));
            }
            // 单调不减（曲线不该越热越慢）
            for w in c.points.windows(2) {
                assert!(w[0].rpm <= w[1].rpm, "{k} 曲线非单调");
            }
        }
    }
}
