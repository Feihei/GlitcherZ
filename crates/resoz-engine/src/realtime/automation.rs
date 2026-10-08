//! Automation curves and control clips (evaluated on the audio thread side).
//!
//! D7/D17 (spec `docs/spec/node-graph.md`, `docs/spec/timeline.md`): control
//! tracks are independent timeline entities parallel to MIDI tracks — a curve
//! is just a pure anchor list with no target parameter; the delivery target is
//! entirely determined by the node graph's automation routing table
//! (`ExecPlan::auto_routes`), and the delivery window is decided by the
//! [`RtControlClip`] placement (nothing is delivered outside the span).

/// Anchor capacity limit for a single curve (capacity constraint via `Copy`; truncated beyond capacity).
pub const MAX_AUTO_POINTS: usize = 64;

/// Total number of control track entities (D12; the model layer's
/// `Vec<ControlTrack>` shares this limit).
pub const MAX_AUTO_TRACKS: usize = 16;

/// Automation anchor (mirrors `resoz_model::AutoPoint`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RtAutoPoint {
    /// Position (in ticks, relative to the clip start).
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
    /// clip's loop length).
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

/// A control (automation) clip placed on the arrangement timeline
/// (mirrors `resoz_model::ControlClip`, fixed-capacity `Copy`; D15/D17).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtControlClip {
    /// Absolute start position on the arrangement (in ticks).
    pub start_ticks: u32,
    /// How long the clip stays active (in ticks).
    pub span_ticks: u32,
    /// Content loop length (in ticks).
    pub loop_ticks: u32,
    /// Anchor curve (pure anchors; target determined by the routing table).
    pub curve: RtAutomation,
}

impl Default for RtControlClip {
    fn default() -> Self {
        Self {
            start_ticks: 0,
            // span 0 = inactive: an all-default clip table is an EMPTY control
            // track (nothing is delivered); every real clip carries its own
            // placement.
            span_ticks: 0,
            loop_ticks: 384,
            curve: RtAutomation::default(),
        }
    }
}

impl RtControlClip {
    /// Samples the value at the absolute transport position: `None` outside
    /// `[start, start + span)` (D17: nothing is delivered outside the span,
    /// the target keeps its last value); inside, the local position is
    /// reduced modulo `loop_ticks` (periodic tiling, D14) and mapped through
    /// the curve. RT-safe.
    pub fn value_at_absolute(&self, position_ticks: f64) -> Option<f32> {
        if self.span_ticks == 0 {
            return None;
        }
        let start = f64::from(self.start_ticks);
        let end = start + f64::from(self.span_ticks);
        if position_ticks < start || position_ticks >= end {
            return None;
        }
        let local = position_ticks - start;
        let phase = if self.loop_ticks > 0 {
            local % f64::from(self.loop_ticks)
        } else {
            local
        };
        self.curve.value_at(phase)
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_AUTO_POINTS, RtAutoPoint, RtAutomation, RtControlClip};

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
    fn control_clip_default_is_inactive() {
        let clip = RtControlClip::default();
        assert_eq!(clip.start_ticks, 0);
        assert_eq!(
            clip.span_ticks, 0,
            "the default clip is inactive (an all-default table = empty track)"
        );
        assert_eq!(clip.loop_ticks, 384);
        assert!(
            !clip.curve.is_active(),
            "default curve is empty (unrouted and has no anchors)"
        );
    }

    #[test]
    fn control_clip_delivers_only_inside_span() {
        let mut clip = RtControlClip {
            start_ticks: 384, // second bar
            span_ticks: 384,
            loop_ticks: 384,
            curve: curve(&[(0, 1.0), (384, 0.0)]),
        };
        // Before the clip: nothing is delivered (D17).
        assert!(clip.value_at_absolute(0.0).is_none());
        assert!(clip.value_at_absolute(383.9).is_none());
        // Inside: loop phase maps the curve.
        assert!((clip.value_at_absolute(384.0).unwrap() - 1.0).abs() < 1e-6);
        assert!((clip.value_at_absolute(576.0).unwrap() - 0.5).abs() < 1e-6);
        // From the clip end on: nothing again.
        assert!(clip.value_at_absolute(768.0).is_none());

        clip.span_ticks = 0; // degenerate clip delivers nothing
        assert!(clip.value_at_absolute(400.0).is_none());
    }

    #[test]
    fn control_clip_tiles_curve_within_span() {
        // span = 2 × loop: the same phase value repeats in the second period.
        let clip = RtControlClip {
            start_ticks: 0,
            span_ticks: 768,
            loop_ticks: 384,
            curve: curve(&[(0, 1.0), (96, 0.5)]),
        };
        assert!((clip.value_at_absolute(48.0).unwrap() - 0.75).abs() < 1e-6);
        assert!(
            (clip.value_at_absolute(432.0).unwrap() - 0.75).abs() < 1e-6,
            "second period should map the same phase"
        );
    }
}
