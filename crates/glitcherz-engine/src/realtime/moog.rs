//! Moog-style 4-pole ladder low-pass (spec `instrument.md` D14).
//!
//! Four trapezoidal (TPT) one-pole low passes in series with the output fed
//! back into the input. Unlike the SVF it is not a "clean" filter: the stage
//! that carries the resonance saturates, which is where the ladder's growl and
//! its self-oscillation limit cycle come from.
//!
//! The feedback is resolved **without** a unit delay: the four stages are
//! linear, so the cascade output is a closed-form function of the input
//! (`y4 = G⁴·u + σ`, with `σ` the four stages' state contributions), which
//! can be substituted into `u = x − k·y4` directly. That keeps the resonant
//! peak at the cutoff the user asked for even at high frequencies, where a
//! one-sample-delayed feedback would detune and eventually blow up.
//!
//! The saturation is applied *outside* that solve (`u = x − k·tanh(y4)`), i.e.
//! it is a single Newton-free step rather than an implicit solve. Because
//! `|tanh(y)| ≤ |y|`, saturation can only *reduce* the loop gain, so a driven
//! ladder is bounded and a self-oscillating one settles into a limit cycle
//! instead of running away.
//!
//! Everything is allocation-free and bounded: two channels × four states.

use std::f32::consts::PI;

/// Feedback amount at the top of the resonance slider.
///
/// Four one-pole stages each contribute `1/√2` and −45° at cutoff, so the loop
/// gain is `k/4` there: the ladder starts to self-oscillate at `k = 4`. `4.3`
/// puts the top of the slider just past that threshold, where the saturator
/// fixes the amplitude — the classic "resonance at 10 screams" behaviour.
const MAX_FEEDBACK: f32 = 4.3;

/// Resonance value `MAX_FEEDBACK` is calibrated against (`RES_RANGE` in the
/// UI).
const RESONANCE_MAX: f32 = 0.98;

/// The ladder loses `1/(1+k)` of its passband — the famous Moog bass droop.
/// Half of that is given back on the output: enough that switching filter
/// modes is not a level jump, little enough that the thinning at high
/// resonance (which *is* the sound) survives.
const MAKEUP: f32 = 0.5;

/// Threshold for flushing denormals in the stage states.
const DENORMAL_EPS: f32 = 1e-18;

/// Four-pole ladder low-pass, one independent state chain per channel.
#[derive(Debug, Clone)]
pub struct Ladder {
    /// Per-channel integrator states, stage 0 → stage 3.
    state: [[f32; 4]; 2],
    /// Trapezoidal coefficient `G = g/(1+g)` with `g = tan(π·fc/sr)`.
    big_g: f32,
    /// Feedback amount after the resonance mapping (0..`MAX_FEEDBACK`).
    k: f32,
    /// Output makeup (`1 + k/2`) compensating half of the passband loss.
    makeup: f32,
}

impl Default for Ladder {
    fn default() -> Self {
        Self::new()
    }
}

impl Ladder {
    /// A silent ladder at the SVF's default cutoff (12 kHz) and a low
    /// resonance, so switching modes never produces a jump in level.
    pub fn new() -> Self {
        let mut ladder = Self {
            state: [[0.0; 4]; 2],
            big_g: 0.0,
            k: 0.0,
            makeup: 1.0,
        };
        ladder.set_coeffs(12_000.0, 0.0, 48_000.0);
        ladder
    }

    /// Recomputes the per-block coefficients. RT-safe (pure floating point).
    ///
    /// The cutoff is clamped exactly like the SVF's, so dragging both to the
    /// same place keeps them comparable. Resonance maps **quadratically**:
    /// `k` is what actually drives the feedback, and a linear map would
    /// already damp the passband by several dB at the middle of the slider.
    pub fn set_coeffs(&mut self, cutoff_hz: f32, resonance: f32, sample_rate: f32) {
        let nyquist = sample_rate * 0.5;
        let fc = cutoff_hz.clamp(20.0, 20_000.0).min(nyquist * 0.9).max(1.0);
        let g = (PI * fc / sample_rate).tan();
        self.big_g = g / (1.0 + g);
        let res = resonance.clamp(0.0, RESONANCE_MAX) / RESONANCE_MAX;
        self.k = res * res * MAX_FEEDBACK;
        self.makeup = 1.0 + MAKEUP * self.k;
    }

    /// Current feedback amount (0 = flat, > 4 = self-oscillating). RT-safe.
    pub fn feedback(&self) -> f32 {
        self.k
    }

    /// Clears the integrator states (mode switches, panic).
    pub fn reset(&mut self) {
        self.state = [[0.0; 4]; 2];
    }

    /// Filters one channel in place. `ch` is clamped to the two channels the
    /// graph can carry. RT-safe: no allocation, no unbounded loop.
    pub fn process(&mut self, ch: usize, samples: &mut [f32]) {
        let g = self.big_g;
        let k = self.k;
        let makeup = self.makeup;
        // `shat = s·(1 − G)` is each stage's state contribution to its output
        // (`y = G·x + shat`), the form the closed-form solve needs.
        let one_minus_g = 1.0 - g;
        let g2 = g * g;
        let g3 = g2 * g;
        let g4 = g2 * g2;
        let denom = 1.0 + k * g4;
        let mut s = self.state[ch.min(1)];

        for slot in samples.iter_mut() {
            let input = *slot;
            let sigma = (g3 * s[0] + g2 * s[1] + g * s[2] + s[3]) * one_minus_g;
            // Zero-delay resolution of the feedback loop (linear part).
            let resolved = (g4 * input + sigma) / denom;
            // Saturation sits in the feedback path only: the stages stay
            // linear, so the closed form above remains valid and the loop can
            // only lose gain as the amplitude grows.
            let mut x = input - k * resolved.tanh();
            for stage in &mut s {
                let v = (x - *stage) * g;
                let y = v + *stage;
                *stage = y + v;
                if stage.abs() < DENORMAL_EPS {
                    *stage = 0.0;
                }
                x = y;
            }
            *slot = x * makeup;
        }
        self.state[ch.min(1)] = s;
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::Ladder;

    const SR: f32 = 48_000.0;

    fn sine(freq: f32, seconds: f32) -> Vec<f32> {
        let n = (SR * seconds) as usize;
        (0..n).map(|i| (TAU * freq * i as f32 / SR).sin()).collect()
    }

    fn run(ladder: &mut Ladder, signal: &[f32]) -> Vec<f32> {
        let mut out = signal.to_vec();
        ladder.process(0, &mut out);
        out
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| *s * *s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn dc_gain_is_unity_at_zero_resonance() {
        let mut ladder = Ladder::new();
        ladder.set_coeffs(1_000.0, 0.0, SR);
        let out = run(&mut ladder, &vec![1.0; SR as usize]);
        assert!(
            (out[out.len() - 1] - 1.0).abs() < 1e-3,
            "4-pole DC gain should be 1, got {}",
            out[out.len() - 1]
        );
    }

    /// 24 dB/oct of the ladder against the SVF's 12 dB/oct: at four times the
    /// cutoff the ladder must be far darker while the passband stays intact.
    #[test]
    fn the_ladder_rolls_off_faster_than_a_two_pole() {
        let mut ladder = Ladder::new();
        ladder.set_coeffs(1_000.0, 0.1, SR);
        let pass = run(&mut ladder, &sine(200.0, 0.5));
        let pass_rms = rms(&pass[pass.len() - 4_800..]);
        assert!(
            pass_rms > 0.6,
            "passband RMS should stay near 0.707, got {pass_rms}"
        );

        let mut ladder = Ladder::new();
        ladder.set_coeffs(1_000.0, 0.1, SR);
        let stop = run(&mut ladder, &sine(4_000.0, 0.5));
        let stop_rms = rms(&stop[stop.len() - 4_800..]);
        // A 2-pole SVF at the same settings leaves ≈ 0.02 (−30 dB); four
        // poles leave ≈ 0.0009 (−57 dB).
        assert!(
            stop_rms < 0.005,
            "four poles must roll off hard, got {stop_rms}"
        );
    }

    /// Top of the slider: the ladder rings on after the input stops, and the
    /// saturator keeps that limit cycle bounded instead of letting it grow.
    #[test]
    fn self_oscillation_is_bounded() {
        let mut ladder = Ladder::new();
        ladder.set_coeffs(500.0, 0.98, SR);
        assert!(ladder.feedback() > 4.0, "top of the slider must ring");
        let mut signal = vec![0.0; SR as usize];
        signal[0] = 1.0;
        let out = run(&mut ladder, &signal);
        assert!(out.iter().all(|s| s.is_finite()), "no NaN/Inf");
        let peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(peak < 8.0, "limit cycle must be bounded, peak {peak}");
        let tail = rms(&out[out.len() - 4_800..]);
        assert!(tail > 0.05, "it must still be ringing, tail RMS {tail}");
    }

    /// Sweeping the cutoff across the whole range at maximum resonance — the
    /// case where a one-sample-delayed feedback detunes and then explodes.
    #[test]
    fn a_full_cutoff_sweep_stays_finite() {
        let mut ladder = Ladder::new();
        let mut signal = vec![0.0; SR as usize];
        for (i, slot) in signal.iter_mut().enumerate() {
            // 20 Hz → 20 kHz in a second, driven hard enough to saturate.
            let t = i as f32 / SR;
            *slot = if (t * 100.0).sin() > 0.0 { 1.0 } else { -1.0 };
        }
        for block in 0..(signal.len() / 128) {
            let cutoff = 20.0 * (1_000.0f32).powf(block as f32 * 128.0 / signal.len() as f32);
            ladder.set_coeffs(cutoff, 0.98, SR);
            let start = block * 128;
            ladder.process(0, &mut signal[start..start + 128]);
        }
        assert!(signal.iter().all(|s| s.is_finite()), "no NaN/Inf");
        let peak = signal.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(peak < 16.0, "sweep must stay bounded, peak {peak}");
    }

    #[test]
    fn the_two_channels_keep_independent_state() {
        let mut ladder = Ladder::new();
        ladder.set_coeffs(1_000.0, 0.5, SR);
        assert!(ladder.feedback() > 0.0, "resonance must reach the ladder");
        let mut left = vec![0.0; 4_800];
        left[0] = 1.0;
        let mut right = vec![0.0; 4_800];
        ladder.process(0, &mut left);
        ladder.process(1, &mut right);
        assert!(
            left.iter().any(|s| s.abs() > 1e-6),
            "channel 0 must respond to its impulse"
        );
        assert!(
            right.iter().all(|s| s.abs() < 1e-9),
            "channel 1 must stay silent"
        );
    }
}
