//! Automation curves and control clips (evaluated on the audio thread side).
//!
//! D7/D17 (spec `.agents/spec/node-graph.md`, `.agents/spec/timeline.md`): control
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

/// Automation anchor (mirrors `glitcherz_model::AutoPoint`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RtAutoPoint {
    /// Position (in ticks, relative to the clip start).
    pub tick: u32,
    /// Parameter value (in real units).
    pub value: f32,
}

/// Curve interpolation between anchors (U12; mirrors
/// `glitcherz_model::CurveInterp`). The two sides must stay behaviorally
/// identical — mirror tests assert the same sample points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RtCurveInterp {
    /// Piecewise-linear segments (freehand / polyline).
    #[default]
    Linear,
    /// Monotone cubic Hermite (Fritsch–Carlson / PCHIP style), overshoot-free.
    Smooth,
}

/// An automation curve: **a pure anchor list** (D7, no embedded target parameter anymore).
///
/// Anchors are sorted by tick ascending; values are interpolated between
/// adjacent anchors (per [`RtCurveInterp`]) and held at the endpoint values
/// outside the range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtAutomation {
    /// Whether enabled.
    pub enabled: bool,
    /// Anchors (the first `point_count` are valid, sorted by tick ascending).
    pub points: [RtAutoPoint; MAX_AUTO_POINTS],
    /// Number of valid anchors.
    pub point_count: usize,
    /// Interpolation between anchors (U12).
    pub interp: RtCurveInterp,
}

impl Default for RtAutomation {
    fn default() -> Self {
        Self {
            enabled: true,
            points: [RtAutoPoint::default(); MAX_AUTO_POINTS],
            point_count: 0,
            interp: RtCurveInterp::Linear,
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
    /// Returns `None` for an empty/disabled curve; interpolation follows
    /// `self.interp`; endpoint values held outside the range. RT-safe: fixed
    /// loops over pre-allocated memory, no allocation.
    pub fn value_at(&self, tick: f64) -> Option<f32> {
        if !self.is_active() {
            return None;
        }
        sample_curve(&self.points[..self.point_count], self.interp, tick)
    }
}

/// Samples an ordered anchor slice at `tick` (mirror of
/// `glitcherz_model::sample_curve`, U12). `None` for an empty slice;
/// endpoint values held outside the range. RT-safe.
fn sample_curve(points: &[RtAutoPoint], interp: RtCurveInterp, tick: f64) -> Option<f32> {
    let first = *points.first()?;
    let last = *points.last()?;
    if tick <= f64::from(first.tick) {
        return Some(first.value);
    }
    if tick >= f64::from(last.tick) {
        return Some(last.value);
    }
    match interp {
        RtCurveInterp::Linear => sample_linear(points, tick),
        RtCurveInterp::Smooth => sample_smooth(points, tick),
    }
}

/// Piecewise-linear segment lookup (the pre-U12 behavior).
fn sample_linear(points: &[RtAutoPoint], tick: f64) -> Option<f32> {
    for window in points.windows(2) {
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
    points.last().map(|p| p.value)
}

/// One secant slope between two anchors (0 for a degenerate/duplicate tick).
fn secant(a: RtAutoPoint, b: RtAutoPoint) -> f32 {
    let h = f64::from(b.tick) - f64::from(a.tick);
    if h <= 0.0 {
        0.0
    } else {
        ((b.value - a.value) as f64 / h) as f32
    }
}

/// Monotone-cubic (PCHIP / Fritsch–Carlson style) tangent at anchor `k`.
/// Mirror of the model implementation; reads only `k-1..=k+1`.
fn smooth_tangent(points: &[RtAutoPoint], k: usize) -> f32 {
    let n = points.len();
    if n < 2 {
        return 0.0;
    }
    if k == 0 {
        return secant(points[0], points[1]);
    }
    if k == n - 1 {
        return secant(points[n - 2], points[n - 1]);
    }
    let d_prev = secant(points[k - 1], points[k]);
    let d_next = secant(points[k], points[k + 1]);
    if d_prev * d_next <= 0.0 {
        return 0.0;
    }
    let h_prev = (f64::from(points[k].tick) - f64::from(points[k - 1].tick)) as f32;
    let h_next = (f64::from(points[k + 1].tick) - f64::from(points[k].tick)) as f32;
    let w1 = 2.0 * h_next + h_prev;
    let w2 = h_next + 2.0 * h_prev;
    let denom = w1 / d_prev + w2 / d_next;
    if denom.abs() < f32::EPSILON {
        0.0
    } else {
        (w1 + w2) / denom
    }
}

/// Monotone cubic Hermite segment (U12 `Smooth`); falls back to linear on a
/// degenerate (zero-width) segment.
fn sample_smooth(points: &[RtAutoPoint], tick: f64) -> Option<f32> {
    for i in 0..points.len().saturating_sub(1) {
        let (a, b) = (points[i], points[i + 1]);
        if tick >= f64::from(b.tick) {
            continue;
        }
        let h = f64::from(b.tick) - f64::from(a.tick);
        if h <= 0.0 {
            return Some(a.value);
        }
        let t = ((tick - f64::from(a.tick)) / h) as f32;
        let m0 = smooth_tangent(points, i);
        let m1 = smooth_tangent(points, i + 1);
        let t2 = t * t;
        let t3 = t2 * t;
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        let hf = h as f32;
        return Some(h00 * a.value + h10 * hf * m0 + h01 * b.value + h11 * hf * m1);
    }
    points.last().map(|p| p.value)
}

/// A control (automation) clip placed on the arrangement timeline
/// (mirrors `glitcherz_model::ControlClip`, fixed-capacity `Copy`; D15/D17).
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
    use super::{MAX_AUTO_POINTS, RtAutoPoint, RtAutomation, RtControlClip, RtCurveInterp};

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

    /// U12: the default interpolation is `Linear` (old behavior), so an
    /// unset curve samples exactly as before.
    #[test]
    fn default_interp_is_linear() {
        assert_eq!(RtCurveInterp::default(), RtCurveInterp::Linear);
        let auto = curve(&[(0, 1.0), (96, 0.5), (192, 0.0)]);
        assert_eq!(auto.interp, RtCurveInterp::Linear);
        assert!((auto.value_at(48.0).unwrap() - 0.75).abs() < 1e-6);
    }

    /// U12: `Smooth` is a monotone cubic — overshoot-free, hits the peak
    /// anchor exactly, holds endpoints. This mirrors the model-side
    /// `smooth_interp_is_overshoot_free_and_hits_anchors`.
    #[test]
    fn smooth_interp_is_overshoot_free_and_hits_anchors() {
        let mut auto = curve(&[(0, 0.0), (50, 10.0), (100, 0.0)]);
        auto.interp = RtCurveInterp::Smooth;
        assert!((auto.value_at(-1.0).unwrap()).abs() < 1e-6, "head held");
        assert!((auto.value_at(200.0).unwrap()).abs() < 1e-6, "tail held");
        assert!(
            (auto.value_at(50.0).unwrap() - 10.0).abs() < 1e-6,
            "peak anchor hit exactly"
        );
        for k in 0..=400 {
            let v = auto.value_at(f64::from(k) * 0.25).unwrap();
            assert!(
                (-1e-4..=10.0 + 1e-4).contains(&v),
                "overshoot at tick {}: {v}",
                f64::from(k) * 0.25
            );
        }
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
