//! Shared fractional-delay line and LFO for the modulation effects
//! (spec `.agents/spec/effects.md` E4).
//!
//! Chorus and Flanger share one core: a fixed-capacity delay line read at a
//! **fractional** position (linear interpolation) swept by a sine LFO. Integer
//! taps — what [`DelayProcessor`](crate::realtime::delay::DelayProcessor) uses —
//! would step audibly while the delay is being modulated, so modulation effects
//! must interpolate.
//!
//! Everything here is allocation-free after construction and panic-free.

use std::f32::consts::TAU;

use crate::realtime::effect::descriptor::Unit;
use crate::realtime::effect::descriptor::{Curve, ParamFlags, ParamSpec, Rate};

/// Fixed-capacity fractional delay line (linear interpolation).
pub struct ModDelayLine {
    buffer: Box<[f32]>,
    /// Next slot to write.
    write: usize,
}

impl ModDelayLine {
    /// Pre-allocates `capacity` samples (construction time only; at least 2).
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(2);
        Self {
            buffer: vec![0.0; capacity].into_boxed_slice(),
            write: 0,
        }
    }

    /// Reads `delay` samples back from the most recently written sample,
    /// interpolating between neighbours. RT-safe.
    pub fn read(&self, delay: f32) -> f32 {
        let capacity = self.buffer.len();
        let max = (capacity - 1) as f32;
        let delay = if delay.is_finite() {
            delay.clamp(0.0, max)
        } else {
            0.0
        };
        // `push` advances the write cursor every sample; reading `delay` samples
        // back means the slot `write - delay`, i.e. the sample `delay` steps
        // before the one about to be written.
        let position = self.write as f32 - delay;
        let floor = position.floor();
        let frac = position - floor;
        let index = (floor as isize).rem_euclid(capacity as isize) as usize;
        let next = (index + 1) % capacity;
        let a = self.buffer.get(index).copied().unwrap_or(0.0);
        let b = self.buffer.get(next).copied().unwrap_or(0.0);
        a + (b - a) * frac
    }

    /// Writes `value` and advances the write cursor. RT-safe.
    pub fn push(&mut self, value: f32) {
        let write = self.write;
        if let Some(slot) = self.buffer.get_mut(write) {
            // Flush denormals so a silent tail does not burn CPU.
            *slot = if value.abs() < 1e-18 { 0.0 } else { value };
        }
        self.write = (write + 1) % self.buffer.len();
    }
}

/// Sine LFO with a normalized phase in `0..1`.
#[derive(Default)]
pub struct Lfo {
    phase: f32,
}

impl Lfo {
    /// Advances one sample and returns the new normalized phase. RT-safe.
    pub fn advance(&mut self, rate_hz: f32, sample_rate: f32) -> f32 {
        let increment = if rate_hz.is_finite() && sample_rate > 0.0 {
            rate_hz / sample_rate
        } else {
            0.0
        };
        self.phase += increment;
        if self.phase >= 1.0 {
            self.phase -= self.phase.floor();
        }
        self.phase
    }
}

/// `sin(2π·phase)` for a normalized phase (the LFO waveform).
pub fn sine(phase: f32) -> f32 {
    (TAU * phase).sin()
}

/// Linear-domain, per-sample parameter helper (`rate` is per-block; these are
/// the modulation knobs that must move smoothly every sample).
pub const fn sampler(
    name: &'static str,
    unit: Unit,
    min: f32,
    max: f32,
    default: f32,
    smoothing_ms: f32,
) -> ParamSpec {
    ParamSpec {
        name,
        short: name,
        unit,
        min,
        max,
        default,
        curve: Curve::Linear,
        smoothing_ms,
        rate: Rate::Sample,
        flags: ParamFlags::AUTOMATABLE,
        choices: &[],
    }
}
