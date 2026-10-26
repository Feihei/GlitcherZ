//! One-pole parameter smoother (spec `.agents/spec/effects.md` D9–D12).
//!
//! Every continuous parameter of a built-in effect runs through one of these:
//! a block-boundary `set` writes a target, and the processor advances the
//! value while it renders. No allocation, no locks, no panics — RT-safe once
//! constructed.
//!
//! Domain: [`Curve::Linear`] smooths the value itself; [`Curve::Log`] keeps the
//! internal state in `ln(value)` so equal ratios take equal time (a move from
//! 20 → 40 Hz feels like 10 → 20 kHz). The conversion back to a linear value
//! (`exp`) happens once per read, so log parameters are meant to be consumed at
//! [`Rate::Block`](crate::realtime::effect::descriptor::Rate).
//!
//! Start semantics (D12): `set` before the first `tick`/`advance` **snaps**.
//! That keeps constructor-time document values exact (no fade-in on node birth,
//! on project load, or in offline rendering) while every later change ramps.

use crate::realtime::effect::descriptor::{Curve, ParamSpec};

/// One-pole smoother whose time constant is fixed at construction.
#[derive(Debug, Clone, Copy)]
pub struct Smoothed {
    /// Internal state, in the working domain (value or `ln(value)`).
    state: f32,
    /// Target, in the same domain.
    target: f32,
    /// Per-sample one-pole coefficient (`1.0` = snap).
    coeff: f32,
    /// Whether the state is kept in the log domain.
    log: bool,
    /// Set by the first `tick`/`advance`; `set` snaps while this is false.
    started: bool,
    /// Clamp bounds (also keep the log domain away from zero).
    min: f32,
    max: f32,
}

/// Per-sample coefficient for a one-pole smoother.
///
/// `ms` is the 63 % time constant; a step settles to within ~1 % after `5·ms`.
/// Non-positive or non-finite input yields `1.0` (snap).
pub fn coeff_for(ms: f32, sample_rate: f32) -> f32 {
    // `is_finite` first: it also rejects NaN, so the comparisons below cannot
    // be fooled by it.
    if !ms.is_finite() || !sample_rate.is_finite() || ms <= 0.0 || sample_rate <= 0.0 {
        return 1.0;
    }
    1.0 - (-1.0 / (ms * 0.001 * sample_rate)).exp()
}

impl Smoothed {
    /// Builds a smoother from a spec, snapped to `spec.default`.
    /// Construction time only.
    pub fn new(spec: &ParamSpec, sample_rate: f32) -> Self {
        let mut smoothed = Self {
            state: 0.0,
            target: 0.0,
            coeff: coeff_for(spec.smoothing_ms, sample_rate),
            log: matches!(spec.curve, Curve::Log),
            started: false,
            min: spec.min,
            max: spec.max,
        };
        smoothed.snap_value(spec.default);
        smoothed
    }

    /// A slot filler for banks smaller than capacity: never set, always 0.
    pub fn inert() -> Self {
        Self {
            state: 0.0,
            target: 0.0,
            coeff: 1.0,
            log: false,
            started: false,
            min: 0.0,
            max: 0.0,
        }
    }

    /// Whether the smoother has been advanced at least once.
    pub fn started(&self) -> bool {
        self.started
    }

    /// Sets the target (clamped, non-finite ignored).
    ///
    /// Snaps while the node has not rendered yet (D12) **and** for discrete
    /// parameters (`smoothing_ms = 0`), so switches take effect at the block
    /// boundary without interpolating through intermediate states. RT-safe.
    pub fn set(&mut self, value: f32) {
        if !value.is_finite() {
            return;
        }
        let value = value.clamp(self.min, self.max);
        if self.started && self.coeff < 1.0 {
            self.target = self.domain(value);
        } else {
            self.snap_value(value);
        }
    }

    /// Jumps to `value` immediately, regardless of the started state.
    /// RT-safe.
    pub fn snap(&mut self, value: f32) {
        if value.is_finite() {
            self.snap_value(value.clamp(self.min, self.max));
        }
    }

    /// Advances one sample and returns the new value. RT-safe.
    pub fn tick(&mut self) -> f32 {
        self.started = true;
        let delta = self.target - self.state;
        if delta != 0.0 {
            let next = self.state + delta * self.coeff;
            // f32 stall guard: once `delta * coeff` falls below half an ULP of
            // the state, the addition rounds back to itself and the ramp would
            // sit a few 1e-5 short of the target forever. Snap on no progress.
            self.state = if next == self.state {
                self.target
            } else {
                next
            };
        }
        self.out(self.state)
    }

    /// Advances `frames` samples and returns the new value (block-rate
    /// consumption). RT-safe.
    pub fn advance(&mut self, frames: usize) -> f32 {
        self.started = true;
        let target = self.target;
        for _ in 0..frames {
            let delta = target - self.state;
            if delta == 0.0 {
                break;
            }
            let next = self.state + delta * self.coeff;
            self.state = if next == self.state { target } else { next };
        }
        self.out(self.state)
    }

    /// Current value without advancing. RT-safe.
    pub fn value(&self) -> f32 {
        self.out(self.state)
    }

    /// Current target in linear value. RT-safe.
    pub fn target(&self) -> f32 {
        self.out(self.target)
    }

    fn snap_value(&mut self, value: f32) {
        let domain = self.domain(value);
        self.state = domain;
        self.target = domain;
    }

    fn domain(&self, value: f32) -> f32 {
        if self.log {
            // `validate_descriptor` guarantees `min > 0` for log parameters;
            // the floor keeps a malformed spec from producing `-inf`.
            value.max(self.min).max(1e-9).ln()
        } else {
            value
        }
    }

    fn out(&self, state: f32) -> f32 {
        if self.log { state.exp() } else { state }
    }
}

#[cfg(test)]
mod tests {
    use super::{Smoothed, coeff_for};
    use crate::realtime::effect::descriptor::{ParamSpec, Unit};

    const SR: f32 = 48_000.0;

    fn gain_spec() -> ParamSpec {
        ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 0.0)
    }

    #[test]
    fn set_before_first_tick_snaps() {
        let mut s = Smoothed::new(&gain_spec(), SR);
        s.set(1.0);
        assert_eq!(s.value(), 1.0, "pre-start set must snap (D12)");
        assert_eq!(s.tick(), 1.0);
    }

    #[test]
    fn step_response_settles_within_five_time_constants() {
        let spec = ParamSpec::gain("gain", Unit::Percent, 0.0, 1.0, 0.0);
        let mut s = Smoothed::new(&spec, SR);
        s.tick(); // mark started
        s.set(1.0);
        // 10 ms at 48 kHz = 480 samples; 5 tau = 2400 samples.
        let mut last = 0.0;
        for _ in 0..2_400 {
            let next = s.tick();
            assert!(next >= last, "one-pole must be monotonic");
            assert!(next <= 1.0 + 1e-6, "one-pole must not overshoot");
            last = next;
        }
        assert!(
            (last - 1.0).abs() < 0.01,
            "must reach the target, got {last}"
        );
    }

    #[test]
    fn log_domain_moves_in_equal_ratios() {
        let spec = ParamSpec::freq("cutoff", 20.0, 20_000.0, 20.0);
        let mut s = Smoothed::new(&spec, SR);
        s.tick();
        s.set(20_000.0);
        // After exactly one time constant the value sits at
        // `exp(ln(20) + (ln(20000) - ln(20)) * (1 - e^-1))`.
        let samples = (spec.smoothing_ms * 0.001 * SR) as usize;
        let mut value = 0.0;
        for _ in 0..samples {
            value = s.tick();
        }
        let expected = 20.0f32 * (1000.0f32).powf(1.0 - (-1.0f32).exp());
        assert!(
            (value - expected).abs() / expected < 0.02,
            "expected ~{expected} Hz at one time constant, got {value}"
        );
        assert!(value > 20.0 && value < 20_000.0);
    }

    #[test]
    fn block_advance_matches_per_sample_ticks() {
        let spec = ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 0.0);
        let mut block = Smoothed::new(&spec, SR);
        let mut sample = Smoothed::new(&spec, SR);
        block.tick();
        sample.tick();
        block.set(1.0);
        sample.set(1.0);
        let a = block.advance(128);
        let mut b = 0.0;
        for _ in 0..128 {
            b = sample.tick();
        }
        assert!((a - b).abs() < 1e-9, "{a} vs {b}");
    }

    #[test]
    fn discrete_parameter_snaps() {
        let spec = ParamSpec::switch("link", 0.0);
        let mut s = Smoothed::new(&spec, SR);
        s.tick();
        s.set(1.0);
        assert_eq!(s.value(), 1.0, "ms = 0 must apply at once");
        assert_eq!(s.target(), 1.0);
    }

    #[test]
    fn out_of_range_and_non_finite_values_are_contained() {
        let mut s = Smoothed::new(&gain_spec(), SR);
        s.tick();
        s.set(5.0);
        assert_eq!(s.target(), 1.0);
        s.set(-3.0);
        assert_eq!(s.target(), 0.0);
        s.set(f32::NAN);
        assert_eq!(s.target(), 0.0);
        s.set(f32::INFINITY);
        assert_eq!(s.target(), 0.0);
    }

    #[test]
    fn coeff_is_one_for_disabled_smoothing() {
        assert_eq!(coeff_for(0.0, SR), 1.0);
        assert_eq!(coeff_for(f32::NAN, SR), 1.0);
        assert_eq!(coeff_for(10.0, 0.0), 1.0);
        let c = coeff_for(10.0, SR);
        assert!(c > 0.0 && c < 1.0);
    }
}
