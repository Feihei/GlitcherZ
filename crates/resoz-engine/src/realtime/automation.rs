//! 自动化曲线（音频线程侧求值）。

use crate::realtime::commands::ParamId;

/// 单条曲线的锚点容量上限（`Copy` 表示的容量约束，超容截断）。
pub const MAX_AUTO_POINTS: usize = 64;

/// 每条轨道最多同时容纳的曲线条数。
pub const MAX_AUTOMATIONS: usize = 4;

/// 自动化锚点（镜像 `resoz_model::AutoPoint`）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RtAutoPoint {
    /// 位置（tick，相对循环起点）。
    pub tick: u32,
    /// 参数值（真实单位）。
    pub value: f32,
}

/// 一条自动化曲线（镜像 `resoz_model::Automation`，定容 `Copy`）。
///
/// 锚点按 tick 升序；取值在相邻锚点间线性插值，两端外保持端值。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtAutomation {
    /// 作用的目标参数（轨道定向参数：增益/声像/截止）。
    pub param: ParamId,
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
            param: ParamId::TrackGain,
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

    /// 在 `tick`（已按轨道循环长度取模后的位置）处取值。
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

#[cfg(test)]
mod tests {
    use super::{MAX_AUTO_POINTS, RtAutoPoint, RtAutomation};
    use crate::realtime::commands::ParamId;

    fn curve(points: &[(u32, f32)]) -> RtAutomation {
        let mut auto = RtAutomation {
            param: ParamId::TrackGain,
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
}
