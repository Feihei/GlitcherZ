//! State-variable filter (TPT / Simper SVF).

use std::f32::consts::PI;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;

/// Filter output mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterMode {
    /// Low-pass (default).
    #[default]
    LowPass,
    /// High-pass.
    HighPass,
    /// Band-pass.
    BandPass,
}

/// State-variable filter (zero-delay feedback / TPT topology, Simper SVF).
///
/// Second order (12 dB/oct), providing low-pass/high-pass/band-pass outputs
/// simultaneously; resonance is parameterized with Simper's `res` (0..1,
/// k = 2 - 2·res), approaching self-oscillation as `res → 1`.
///
/// The cutoff frequency is clamped at process time to `[20, min(20000, 0.45·sr)]`;
/// state is flushed of denormals when silent to avoid CPU spikes. All paths RT-safe.
///
/// State is kept per channel, up to 2 channels (matching the actual channel
/// count of [`AudioBuffer`] in this pipeline); channels beyond 2 are not processed.
#[derive(Debug)]
pub struct SvfProcessor {
    sample_rate: f32,
    mode: FilterMode,
    cutoff_hz: f32,
    resonance: f32,
    /// Per-channel state (up to 2).
    ic1: [f32; 2],
    ic2: [f32; 2],
    /// Coefficients recomputed per block.
    a1: f32,
    a2: f32,
    a3: f32,
    k: f32,
}

/// Upper bound preventing resonance from reaching self-oscillation.
const MAX_RESONANCE: f32 = 0.98;
/// Threshold for flushing denormals.
const DENORMAL_EPS: f32 = 1e-18;

impl SvfProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction-time tolerance).
    pub fn new(sample_rate: f32) -> Self {
        let mut svf = Self {
            sample_rate: if sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            mode: FilterMode::LowPass,
            cutoff_hz: 12_000.0,
            resonance: 0.2,
            ic1: [0.0; 2],
            ic2: [0.0; 2],
            a1: 0.0,
            a2: 0.0,
            a3: 0.0,
            k: 2.0,
        };
        svf.update_coeffs();
        svf
    }

    /// Recomputes per-block coefficients from the current parameters. RT-safe (pure floating point, no pathological branching).
    fn update_coeffs(&mut self) {
        let nyquist = self.sample_rate * 0.5;
        let fc = self
            .cutoff_hz
            .clamp(20.0, 20_000.0)
            .min(nyquist * 0.9)
            .max(1.0);
        let g = (PI * fc / self.sample_rate).tan();
        self.k = (2.0 - 2.0 * self.resonance).max(0.0);
        self.a1 = 1.0 / (1.0 + g * (g + self.k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }

    /// Flushes denormal state.
    fn flush(state: &mut f32) {
        if state.abs() < DENORMAL_EPS {
            *state = 0.0;
        }
    }
}

impl Processor for SvfProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        self.update_coeffs();
        let (a1, a2, a3, k) = (self.a1, self.a2, self.a3, self.k);
        let mode = self.mode;
        for ch in 0..buffer.channels().min(2) {
            let (Some(mut ic1), Some(mut ic2)) = (Some(self.ic1[ch]), Some(self.ic2[ch])) else {
                continue;
            };
            if let Some(channel) = buffer.channel_mut(ch) {
                for sample in channel.iter_mut() {
                    let input = *sample;
                    let v3 = input - ic2;
                    let v1 = a1 * ic1 + a2 * v3;
                    let v2 = ic2 + a2 * ic1 + a3 * v3;
                    ic1 = 2.0 * v1 - ic1;
                    ic2 = 2.0 * v2 - ic2;
                    *sample = match mode {
                        FilterMode::LowPass => v2,
                        // Band-pass normalized by multiplying k: unity gain at the
                        // center frequency (the analog prototype s/(s²+ks+1) has a
                        // raw band-pass gain of 1/k, which varies with Q).
                        FilterMode::BandPass => v1 * k,
                        FilterMode::HighPass => input - k * v1 - v2,
                    };
                    Self::flush(&mut ic1);
                    Self::flush(&mut ic2);
                }
            }
            self.ic1[ch] = ic1;
            self.ic2[ch] = ic2;
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        if !value.is_finite() {
            return matches!(
                id,
                ParamId::FilterCutoff | ParamId::FilterResonance | ParamId::FilterMode
            );
        }
        match id {
            ParamId::FilterCutoff => {
                self.cutoff_hz = value.clamp(20.0, 20_000.0);
                true
            }
            ParamId::FilterResonance => {
                self.resonance = value.clamp(0.0, MAX_RESONANCE);
                true
            }
            ParamId::FilterMode => {
                self.mode = if value < 0.5 {
                    FilterMode::LowPass
                } else if value < 1.5 {
                    FilterMode::HighPass
                } else {
                    FilterMode::BandPass
                };
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::{FilterMode, SvfProcessor};
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn ctx() -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: 0,
        }
    }

    /// Feeds `signal` through the filter in fixed blocks and returns the filtered signal.
    fn run_through(svf: &mut SvfProcessor, signal: &[f32]) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(BLOCK) {
            buf.set_len(chunk.len());
            buf.channel_mut(0).unwrap().copy_from_slice(chunk);
            svf.process(&ctx(), &mut buf);
            out.extend_from_slice(buf.channel(0).unwrap());
        }
        out
    }

    fn sine(freq: f32, seconds: f32) -> Vec<f32> {
        let n = (SR * seconds) as usize;
        (0..n).map(|i| (TAU * freq * i as f32 / SR).sin()).collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| *s * *s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    // Standard test 1: silent input → silent output.
    #[test]
    fn silence_in_silence_out() {
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterResonance, 0.95); // must not self-oscillate even at high resonance
        let out = run_through(&mut svf, &vec![0.0; BLOCK * 8]);
        assert!(out.iter().all(|s| s.abs() < 1e-9));
    }

    // Standard test 2: full-scale input → no NaN/Inf.
    #[test]
    fn full_scale_stays_finite_even_at_high_resonance() {
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterCutoff, 500.0);
        svf.set_param(ParamId::FilterResonance, 0.98);
        let signal = sine(1000.0, 1.0);
        let out = run_through(&mut svf, &signal);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(
            out.iter().fold(0.0f32, |a, s| a.max(s.abs())) < 100.0,
            "resonant output must be bounded"
        );
    }

    #[test]
    fn impulse_at_high_resonance_decays() {
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterCutoff, 800.0);
        svf.set_param(ParamId::FilterResonance, 0.98);
        let mut signal = vec![0.0; SR as usize];
        signal[0] = 1.0;
        let out = run_through(&mut svf, &signal);
        let peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(
            peak.is_finite() && peak < 10.0,
            "impulse response must be bounded, peak {peak}"
        );
        let tail = &out[out.len() - BLOCK..];
        assert!(
            tail.iter().all(|s| s.abs() < 1e-3),
            "high-Q impulse response must decay cleanly"
        );
    }

    #[test]
    fn low_pass_attenuates_above_cutoff_and_passes_below() {
        // Passband: 440 Hz @ fc 8000 → essentially unattenuated.
        // (Sine RMS = 0.707; the tail window covers about 1.17 periods, plus margin.)
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterCutoff, 8_000.0);
        svf.set_param(ParamId::FilterResonance, 0.1);
        let passed = run_through(&mut svf, &sine(440.0, 0.5));
        let tail_rms = rms(&passed[passed.len() - BLOCK..]);
        assert!(
            tail_rms > 0.6,
            "passband RMS should be near 0.707, got {tail_rms}"
        );

        // Stopband: 1000 Hz @ fc 200 → strong second-order attenuation.
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterCutoff, 200.0);
        svf.set_param(ParamId::FilterResonance, 0.1);
        let blocked = run_through(&mut svf, &sine(1_000.0, 0.5));
        let tail_rms = rms(&blocked[blocked.len() - BLOCK..]);
        assert!(
            tail_rms < 0.25,
            "stopband RMS should be far below 0.707, got {tail_rms}"
        );
    }

    #[test]
    fn high_pass_blocks_low_frequencies() {
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterMode, 1.0); // HighPass
        svf.set_param(ParamId::FilterCutoff, 2_000.0);
        svf.set_param(ParamId::FilterResonance, 0.1);
        let blocked = run_through(&mut svf, &sine(200.0, 0.5));
        let tail_rms = rms(&blocked[blocked.len() - BLOCK..]);
        assert!(
            tail_rms < 0.2,
            "high-pass must strongly attenuate low frequencies, got {tail_rms}"
        );
    }

    #[test]
    fn band_pass_peaks_at_center_frequency() {
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterMode, 2.0); // BandPass
        svf.set_param(ParamId::FilterCutoff, 440.0);
        svf.set_param(ParamId::FilterResonance, 0.3);
        let centered = run_through(&mut svf, &sine(440.0, 0.5));
        let center_rms = rms(&centered[centered.len() - BLOCK..]);
        assert!(
            center_rms > 0.6,
            "band-pass RMS at the center frequency should be near 0.707 after normalization, got {center_rms}"
        );

        // Far end (4000 Hz) rolls off at first-order -6 dB/oct.
        let far = run_through(&mut svf, &sine(4_000.0, 0.5));
        let far_rms = rms(&far[far.len() - BLOCK..]);
        assert!(
            far_rms < 0.2,
            "band-pass must clearly attenuate far-off frequencies, got {far_rms}"
        );
    }

    #[test]
    fn dc_gain_is_unity_for_low_pass_and_zero_for_high_pass() {
        let n = SR as usize; // 1 second, fully converged

        let mut lp = SvfProcessor::new(SR);
        lp.set_param(ParamId::FilterCutoff, 1_000.0);
        let out = run_through(&mut lp, &vec![1.0; n]);
        assert!(
            (out[out.len() - 1] - 1.0).abs() < 1e-3,
            "low-pass DC gain should be 1, got {}",
            out[out.len() - 1]
        );

        let mut hp = SvfProcessor::new(SR);
        hp.set_param(ParamId::FilterMode, 1.0);
        hp.set_param(ParamId::FilterCutoff, 1_000.0);
        let out = run_through(&mut hp, &vec![1.0; n]);
        assert!(
            out[out.len() - 1].abs() < 1e-3,
            "high-pass DC gain should be 0, got {}",
            out[out.len() - 1]
        );
    }

    #[test]
    fn params_are_clamped() {
        let mut svf = SvfProcessor::new(SR);
        assert!(svf.set_param(ParamId::FilterCutoff, -100.0));
        assert!((svf.cutoff_hz - 20.0).abs() < 1e-6);
        svf.set_param(ParamId::FilterCutoff, 1_000_000.0);
        assert!((svf.cutoff_hz - 20_000.0).abs() < 1e-3);

        svf.set_param(ParamId::FilterResonance, 5.0);
        assert!((svf.resonance - 0.98).abs() < 1e-6);

        svf.set_param(ParamId::FilterMode, 1.0);
        assert_eq!(svf.mode, FilterMode::HighPass);
        assert!(
            !svf.set_param(ParamId::MasterGain, 1.0),
            "unhandled parameters should return false"
        );
    }
}
