//! Automation curves and automation track entities (evaluated on the audio thread side).
//!
//! D7 (spec `docs/spec/node-graph.md`): **automation tracks are independent
//! timeline entities parallel to MIDI tracks** — a curve is just a pure anchor
//! list with no target parameter; the delivery target is entirely determined
//! by the node graph's automation routing table (`ExecPlan::auto_routes`).

/// Anchor capacity limit for a single curve (capacity constraint via `Copy`; truncated beyond capacity).
pub const MAX_AUTO_POINTS: usize = 64;

/// Number of automation tracks per MIDI track (stride of the timeline entity index).
pub const MAX_AUTOMATIONS: usize = 4;

/// Total number of automation track entities (`NUM_TRACKS × MAX_AUTOMATIONS`).
pub const MAX_AUTO_TRACKS: usize = 16;

/// Default automation track loop length (in ticks): one bar (96 TPQN × 4 beats).
pub const DEFAULT_AUTO_LOOP_TICKS: u32 = 384;

/// Automation anchor (mirrors `resoz_model::AutoPoint`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RtAutoPoint {
    /// Position (in ticks, relative to the loop start).
    pub tick: u32,
    /// Parameter value (in real units).
    pub value: f32,
}

/// An automation curve: **a pure anchor list** (D7, no embedded target parameter anymore).
///
/// Anchors are sorted by tick ascending; values are linearly interpolated
/// between adjacent anchors and held at the endpoint values outside the range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtAutomation {
    /// Whether enabled.
    pub enabled: bool,
    /// Anchors (the first `point_count` are valid, sorted by tick ascending).
    pub points: [RtAutoPoint; MAX_AUTO_POINTS],
    /// Number of valid anchors.
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
    /// Appends an anchor (the caller guarantees tick-ascending order; silently dropped beyond capacity).
    pub fn push(&mut self, point: RtAutoPoint) {
        if self.point_count < MAX_AUTO_POINTS {
            self.points[self.point_count] = point;
            self.point_count += 1;
        }
    }

    /// Whether the curve is active (enabled and has anchors).
    pub fn is_active(&self) -> bool {
        self.enabled && self.point_count > 0
    }

    /// Returns the value at `tick` (a position already reduced modulo the
    /// automation track's loop length).
    /// Returns `None` for an empty/disabled curve; linear interpolation;
    /// endpoint values held outside the range. RT-safe.
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

/// An automation track entity: independent loop length + pure anchor curve (D7).
///
/// `loop_ticks` is carried by the track itself and is **decoupled** from the
/// loop of the MIDI track's sound source (the old `Track.automations`
/// follow-the-source-loop semantics is retired); `0` = no loop (follows the
/// global transport position).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtAutoTrack {
    /// Loop length (in ticks).
    pub loop_ticks: u32,
    /// Curve (pure anchors; target determined by the routing table).
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
        assert!(
            !track.curve.is_active(),
            "default curve is empty (unrouted and has no anchors)"
        );
    }
}
