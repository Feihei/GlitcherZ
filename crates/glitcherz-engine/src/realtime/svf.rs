//! State-variable filter (TPT / Simper SVF).

use std::f32::consts::PI;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::smoothed::coeff_for;
use crate::realtime::moog::Ladder;
use crate::realtime::processor::Processor;

/// Time constant of the cutoff / resonance smoothers (ms). Spec
/// `instrument.md` D4: a dragged cutoff must not step at block boundaries.
const SMOOTHING_MS: f32 = 10.0;

/// Filter output mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterMode {
    /// Low-pass (default), 12 dB/oct.
    #[default]
    LowPass,
    /// High-pass.
    HighPass,
    /// Band-pass.
    BandPass,
    /// Moog-style 4-pole ladder low-pass, 24 dB/oct, saturating resonance.
    Moog,
}

/// State-variable filter (zero-delay feedback / TPT topology, Simper SVF).
///
/// Second order (12 dB/oct), providing low-pass/high-pass/band-pass outputs
/// simultaneously; resonance is parameterized with Simper's `res` (0..1,
/// k = 2 - 2·res), approaching self-oscillation as `res → 1`.
///
/// [`FilterMode::Moog`] swaps the topology for a 4-pole ladder
/// ([`crate::realtime::moog::Ladder`]) fed by the same smoothed cutoff and
/// resonance; the rest of the pipeline cannot tell the two apart.
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
    /// Cutoff **target** (Hz).
    cutoff_hz: f32,
    /// Resonance **target** (Simper `res`).
    resonance: f32,
    /// Smoothed cutoff the coefficients actually use (Hz).
    cutoff_now: f32,
    /// Smoothed resonance the coefficients actually use.
    res_now: f32,
    /// Per-sample one-pole coefficient of both smoothers.
    coeff: f32,
    /// Set by the first render; `set_param` snaps while this is false
    /// (same start semantics as `Smoothed`, spec `effects.md` D12).
    started: bool,
    /// Per-channel state (up to 2).
    ic1: [f32; 2],
    ic2: [f32; 2],
    /// Coefficients recomputed per block.
    a1: f32,
    a2: f32,
    a3: f32,
    k: f32,
    /// Ladder state, advanced only while the mode is [`FilterMode::Moog`]. It
    /// shares the smoothed cutoff / resonance, so the mode switch is seamless:
    /// both algorithms are already running at the same `fc`.
    ladder: Ladder,
}

/// Upper bound preventing resonance from reaching self-oscillation.
const MAX_RESONANCE: f32 = 0.98;
/// Threshold for flushing denormals.
const DENORMAL_EPS: f32 = 1e-18;

impl SvfProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction-time tolerance).
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        let mut svf = Self {
            sample_rate,
            mode: FilterMode::LowPass,
            cutoff_hz: 12_000.0,
            resonance: 0.2,
            cutoff_now: 12_000.0,
            res_now: 0.2,
            coeff: coeff_for(SMOOTHING_MS, sample_rate),
            started: false,
            ic1: [0.0; 2],
            ic2: [0.0; 2],
            a1: 0.0,
            a2: 0.0,
            a3: 0.0,
            k: 2.0,
            ladder: Ladder::new(),
        };
        svf.update_coeffs();
        svf
    }

    /// Smoothed cutoff the filter is currently running at (Hz). RT-safe.
    pub fn current_cutoff(&self) -> f32 {
        self.cutoff_now
    }

    /// Smoothed resonance the filter is currently running at. RT-safe.
    pub fn current_resonance(&self) -> f32 {
        self.res_now
    }

    /// Advances both smoothers by `frames` samples.
    ///
    /// The cutoff moves in the **log domain** (equal ratios take equal time,
    /// the same rule `Smoothed` applies to frequency parameters); resonance is
    /// linear. Before the first render the state snaps, so a restored project
    /// starts at its saved values instead of sweeping from the defaults.
    /// RT-safe: bounded loop, no allocation.
    fn advance(&mut self, frames: usize) {
        if !self.started {
            self.started = true;
            self.cutoff_now = self.cutoff_hz;
            self.res_now = self.resonance;
            return;
        }
        let coeff = self.coeff;
        let cutoff_target = self.cutoff_hz.max(1.0).ln();
        let mut cutoff = self.cutoff_now.max(1.0).ln();
        for _ in 0..frames {
            let delta = cutoff_target - cutoff;
            if delta == 0.0 {
                break;
            }
            let next = cutoff + delta * coeff;
            cutoff = if next == cutoff { cutoff_target } else { next };
        }
        self.cutoff_now = cutoff.exp();

        let res_target = self.resonance;
        let mut res = self.res_now;
        for _ in 0..frames {
            let delta = res_target - res;
            if delta == 0.0 {
                break;
            }
            let next = res + delta * coeff;
            res = if next == res { res_target } else { next };
        }
        self.res_now = res;
    }

    /// Recomputes per-block coefficients from the current parameters. RT-safe (pure floating point, no pathological branching).
    fn update_coeffs(&mut self) {
        let nyquist = self.sample_rate * 0.5;
        let fc = self
            .cutoff_now
            .clamp(20.0, 20_000.0)
            .min(nyquist * 0.9)
            .max(1.0);
        let g = (PI * fc / self.sample_rate).tan();
        self.k = (2.0 - 2.0 * self.res_now).max(0.0);
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
        self.advance(buffer.len());
        self.update_coeffs();
        let (a1, a2, a3, k) = (self.a1, self.a2, self.a3, self.k);
        let mode = self.mode;
        if mode == FilterMode::Moog {
            // Same smoothing, same Nyquist clamp, different topology. The SVF
            // integrators are left untouched so returning to a 2-pole mode
            // resumes from the state it had (see the round-trip test).
            self.ladder
                .set_coeffs(self.cutoff_now, self.res_now, self.sample_rate);
            for ch in 0..buffer.channels().min(2) {
                if let Some(channel) = buffer.channel_mut(ch) {
                    self.ladder.process(ch, channel);
                }
            }
            return;
        }
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
                        // Unreachable: the ladder returns before this loop.
                        // Degrading to the 2-pole low pass (rather than a
                        // passthrough) keeps a future reordering audible-safe.
                        FilterMode::Moog => v2,
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
                if !self.started {
                    self.cutoff_now = self.cutoff_hz;
                }
                true
            }
            ParamId::FilterResonance => {
                self.resonance = value.clamp(0.0, MAX_RESONANCE);
                if !self.started {
                    self.res_now = self.resonance;
                }
                true
            }
            ParamId::FilterMode => {
                self.mode = if value < 0.5 {
                    FilterMode::LowPass
                } else if value < 1.5 {
                    FilterMode::HighPass
                } else if value < 2.5 {
                    FilterMode::BandPass
                } else {
                    FilterMode::Moog
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
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
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

    /// Spec `instrument.md` D4: a cutoff change must be a ramp, not a step at
    /// the block boundary, and it must settle in the documented time.
    #[test]
    fn cutoff_changes_are_smoothed() {
        let mut svf = SvfProcessor::new(SR);
        // One block marks the filter as started (set_param snaps before that).
        let _ = run_through(&mut svf, &vec![0.0; BLOCK]);
        assert!((svf.current_cutoff() - 12_000.0).abs() < 1.0);

        svf.set_param(ParamId::FilterCutoff, 200.0);
        let _ = run_through(&mut svf, &vec![0.0; BLOCK]);
        let after_one = svf.current_cutoff();
        assert!(
            after_one > 200.0 && after_one < 12_000.0,
            "one 128-frame block must not jump to the target (got {after_one})"
        );

        // 10 time constants (100 ms at 48 kHz) settle within a fraction of a Hz.
        let _ = run_through(&mut svf, &vec![0.0; SR as usize / 10]);
        assert!(
            (svf.current_cutoff() - 200.0).abs() < 1.0,
            "settled at {}",
            svf.current_cutoff()
        );
    }

    /// D12 start semantics: values set before the first render are exact, so
    /// a restored project never sweeps from the constructor defaults.
    #[test]
    fn params_set_before_the_first_render_snap() {
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterCutoff, 500.0);
        svf.set_param(ParamId::FilterResonance, 0.7);
        assert!((svf.current_cutoff() - 500.0).abs() < 1e-3);
        assert!((svf.current_resonance() - 0.7).abs() < 1e-6);
    }

    /// D14: `FilterMode::Moog` is index 3 on the wire, and it has to sound
    /// like a 4-pole next to the 2-pole low pass: same passband, far darker
    /// stopband. This is the pairwise comparison that keeps the two modes from
    /// silently collapsing into one another.
    #[test]
    fn the_ladder_mode_is_darker_than_the_two_pole_low_pass() {
        fn mode(mode: f32) -> SvfProcessor {
            let mut svf = SvfProcessor::new(SR);
            svf.set_param(ParamId::FilterMode, mode);
            svf.set_param(ParamId::FilterCutoff, 1_000.0);
            svf.set_param(ParamId::FilterResonance, 0.1);
            svf
        }

        let mut ladder = mode(3.0);
        assert_eq!(ladder.mode, FilterMode::Moog);
        let mut two_pole = mode(0.0);

        // Passband: both stay close to the 0.707 sine RMS.
        let below_ladder = run_through(&mut ladder, &sine(200.0, 0.5));
        let below_two_pole = run_through(&mut two_pole, &sine(200.0, 0.5));
        let (a, b) = (
            rms(&below_ladder[below_ladder.len() - BLOCK..]),
            rms(&below_two_pole[below_two_pole.len() - BLOCK..]),
        );
        assert!(a > 0.6 && b > 0.6, "passband: ladder {a}, 2-pole {b}");
        assert!(
            (a - b).abs() < 0.1,
            "the passband must stay comparable: ladder {a}, 2-pole {b}"
        );

        // Stopband at 4× cutoff: 24 dB/oct against 12 dB/oct.
        let mut ladder = mode(3.0);
        let mut two_pole = mode(0.0);
        let above_ladder = run_through(&mut ladder, &sine(4_000.0, 0.5));
        let above_two_pole = run_through(&mut two_pole, &sine(4_000.0, 0.5));
        let (a, b) = (
            rms(&above_ladder[above_ladder.len() - BLOCK..]),
            rms(&above_two_pole[above_two_pole.len() - BLOCK..]),
        );
        assert!(
            a < b * 0.5,
            "the ladder must roll off faster: ladder {a}, 2-pole {b}"
        );
    }

    /// Switching to Moog and back must not disturb the SVF integrators — the
    /// two topologies keep separate state, so a mode detour is bit-exact in
    /// the 2-pole modes.
    #[test]
    fn a_detour_through_the_ladder_leaves_the_two_pole_modes_bit_exact() {
        let probe = sine(440.0, 0.2);
        let silence = vec![0.0; BLOCK * 8];

        let mut plain = SvfProcessor::new(SR);
        plain.set_param(ParamId::FilterCutoff, 700.0);
        let mut detoured = SvfProcessor::new(SR);
        detoured.set_param(ParamId::FilterCutoff, 700.0);

        let _ = run_through(&mut plain, &silence);
        detoured.set_param(ParamId::FilterMode, 3.0);
        let _ = run_through(&mut detoured, &silence);
        detoured.set_param(ParamId::FilterMode, 0.0);

        assert_eq!(
            run_through(&mut plain, &probe),
            run_through(&mut detoured, &probe)
        );
    }

    /// The resonance slider keeps meaning "more" in the ladder mode: driving
    /// it at the top must ring, and the output has to stay finite.
    #[test]
    fn the_ladder_resonance_is_bounded_at_the_top_of_the_slider() {
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterMode, 3.0);
        svf.set_param(ParamId::FilterCutoff, 400.0);
        svf.set_param(ParamId::FilterResonance, 0.98);
        let mut signal = sine(400.0, 1.0);
        for s in signal.iter_mut() {
            *s *= 2.0;
        }
        let out = run_through(&mut svf, &signal);
        assert!(out.iter().all(|s| s.is_finite()));
        let peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(
            peak < 16.0,
            "resonant ladder must stay bounded, peak {peak}"
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
