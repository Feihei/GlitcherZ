//! 自动化曲线与自动化轨道实体（音频线程侧求值）。
//!
//! D7（spec `docs/spec/node-graph.md`）：**自动化轨道是与 MIDI 轨道
//! 并列的独立时间轴实体**——曲线只是纯锚点列表，不含目标参数；投递
//! 目标完全由节点图的自动化连线表（`ExecPlan::auto_routes`）决定。

/// 单条曲线的锚点容量上限（`Copy` 表示的容量约束，超容截断）。
pub const MAX_AUTO_POINTS: usize = 64;

/// 每条 MIDI 轨对应的自动化轨道数（时间轴实体索引的步长）。
pub const MAX_AUTOMATIONS: usize = 4;

/// 自动化轨道实体总数（`NUM_TRACKS × MAX_AUTOMATIONS`）。
pub const MAX_AUTO_TRACKS: usize = 16;

/// 自动化轨道默认循环长度（tick）：一小节（96 TPQN × 4 拍）。
pub const DEFAULT_AUTO_LOOP_TICKS: u32 = 384;

/// 自动化锚点（镜像 `resoz_model::AutoPoint`）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RtAutoPoint {
    /// 位置（tick，相对循环起点）。
    pub tick: u32,
    /// 参数值（真实单位）。
    pub value: f32,
}

/// 一条自动化曲线：**纯锚点列表**（D7，不再内嵌目标参数）。
///
/// 锚点按 tick 升序；取值在相邻锚点间线性插值，两端外保持端值。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtAutomation {
    /// 是否启用。
    pub enabled: bool,
    /// 锚点（前 `point_count` 个有效，按 tick 升序）。
    pub points: [RtAutoPoint; MAX_AUTO_POINTS],
    /// 有效锚点数。
    pub point_count: usize,
}

impl Default for RtAutomation {
    fn default() -> Self {
        Self {
            enabled: true,
            points: [RtAutoPoint::default(); MAX_AUTO_POINTS],
            point_count: 0,
        }
    }
}

impl RtAutomation {
    /// 追加锚点（调用方保证按 tick 升序传入；超容量静默丢弃）。
    pub fn push(&mut self, point: RtAutoPoint) {
        if self.point_count < MAX_AUTO_POINTS {
            self.points[self.point_count] = point;
            self.point_count += 1;
        }
    }

    /// 曲线是否有效（启用且有锚点）。
    pub fn is_active(&self) -> bool {
        self.enabled && self.point_count > 0
    }

    /// 在 `tick`（已按自动化轨道循环长度取模后的位置）处取值。
    /// 空曲线/未启用返回 `None`；线性插值；首尾外保持端值。RT-safe。
    pub fn value_at(&self, tick: f64) -> Option<f32> {
        if !self.is_active() {
            return None;
        }
        let first = self.points[0];
        let last = self.points[self.point_count - 1];
        if tick <= f64::from(first.tick) {
            return Some(first.value);
        }
        if tick >= f64::from(last.tick) {
            return Some(last.value);
        }
        for window in self.points[..self.point_count].windows(2) {
            let (a, b) = (window[0], window[1]);
            if tick < f64::from(b.tick) {
                let span = f64::from(b.tick) - f64::from(a.tick);
                let ratio = if span > 0.0 {
                    (tick - f64::from(a.tick)) / span
                } else {
                    0.0
                };
                return Some(a.value + (b.value - a.value) * ratio as f32);
            }
        }
        Some(last.value)
    }
}

/// 一条自动化轨道实体：独立循环长度 + 纯锚点曲线（D7）。
///
/// `loop_ticks` 自带于轨道，与 MIDI 轨道音源的循环**解耦**（旧
/// `Track.automations` 随音源循环的语义废止）；`0` = 不循环（跟随
/// 全局走带位置）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtAutoTrack {
    /// 循环长度（tick）。
    pub loop_ticks: u32,
    /// 曲线（纯锚点，目标由连线表决定）。
    pub curve: RtAutomation,
}

impl Default for RtAutoTrack {
    fn default() -> Self {
        Self {
            loop_ticks: DEFAULT_AUTO_LOOP_TICKS,
            curve: RtAutomation::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_AUTO_LOOP_TICKS, MAX_AUTO_POINTS, RtAutoPoint, RtAutoTrack, RtAutomation};

    fn curve(points: &[(u32, f32)]) -> RtAutomation {
        let mut auto = RtAutomation {
            enabled: true,
            ..RtAutomation::default()
        };
        for (tick, value) in points {
            auto.push(RtAutoPoint {
                tick: *tick,
                value: *value,
            });
        }
        auto
    }

    #[test]
    fn value_at_matches_model_semantics() {
        let auto = curve(&[(0, 1.0), (96, 0.5), (192, 0.0)]);
        assert!((auto.value_at(0.0).unwrap() - 1.0).abs() < 1e-6);
        assert!((auto.value_at(48.0).unwrap() - 0.75).abs() < 1e-6);
        assert!((auto.value_at(1_000.0).unwrap()).abs() < 1e-6);
        assert!((auto.value_at(-5.0).unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn inactive_curve_returns_none() {
        let mut auto = curve(&[(0, 1.0)]);
        auto.enabled = false;
        assert!(auto.value_at(10.0).is_none());
        auto.enabled = true;
        auto.points = [RtAutoPoint::default(); MAX_AUTO_POINTS];
        auto.point_count = 0;
        assert!(auto.value_at(10.0).is_none());
    }

    #[test]
    fn push_truncates_at_capacity() {
        let mut auto = RtAutomation::default();
        for i in 0..(MAX_AUTO_POINTS + 10) {
            auto.push(RtAutoPoint {
                tick: i as u32,
                value: 0.5,
            });
        }
        assert_eq!(auto.point_count, MAX_AUTO_POINTS);
    }

    #[test]
    fn auto_track_defaults_to_one_bar_loop() {
        let track = RtAutoTrack::default();
        assert_eq!(track.loop_ticks, DEFAULT_AUTO_LOOP_TICKS);
        assert_eq!(track.loop_ticks, 384);
        assert!(!track.curve.is_active(), "默认曲线为空（未连线且无锚点）");
    }
}
