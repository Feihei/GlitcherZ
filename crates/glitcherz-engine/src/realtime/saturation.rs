//! Saturation (spec `.agents/spec/effects.md` E5).
//!
//! A memoryless waveshaper run at **2× oversampling** so the harmonics it
//! generates fold back instead of aliasing: a 33-tap windowed-sinc halfband
//! filter (polyphase even/odd arms) upsamples, the shaper runs per 2× sample,
//! and the same halfband filters back down. Three shapes: `Soft` (tanh),
//! `Hard` (clip) and `Fold` (asin/sin wavefolder). A trailing one-pole tone
//! low pass and an output trim sit behind the shaper.
//!
//! The upsampling + downsampling pair delays the wet path by 16 samples
//! (independent of sample rate), reported through `latency_samples` so the
//! graph's PDC compensation (spec `node-graph.md` §11) aligns parallel paths.
//! The dry tap is *not* delayed to match: at low mix values the dry and wet
//! paths form a comb, which parallel-drive designs accept; `mix = 0` remains
//! a bit-exact passthrough.
//!
//! All state pre-allocated; `process` is allocation-, lock- and panic-free.

use std::f32::consts::{FRAC_PI_2, PI, TAU};

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::EffectParamBank;
use crate::realtime::effect::descriptor::{EffectDescriptor, ParamSpec, StereoClass, Unit};
use crate::realtime::processor::Processor;

/// Slot of the shaper mode (enum index into `MODE_CHOICES`).
pub const SLOT_MODE: u8 = 0;
/// Slot of the input drive.
pub const SLOT_DRIVE: u8 = 1;
/// Slot of the trailing tone low pass (Hz).
pub const SLOT_TONE: u8 = 2;
/// Slot of the output trim (dB).
pub const SLOT_TRIM: u8 = 3;
/// Slot of the dry/wet mix.
pub const SLOT_MIX: u8 = 4;

/// Halfband taps per polyphase arm: even arm carries the center tap.
const UP_EVEN_LEN: usize = 17;
const UP_ODD_LEN: usize = 16;
/// Wet-path delay in samples at the engine rate (two halfband group delays:
/// 16 taps at 2× each = 8 + 8).
pub const WET_LATENCY: usize = 16;

/// Labels for [`SLOT_MODE`]; the index is the DSP dispatch value.
pub const MODE_CHOICES: [&str; 3] = ["Soft", "Hard", "Fold"];

static SATURATION_PARAMS: [ParamSpec; 5] = [
    ParamSpec::enumeration("mode", &MODE_CHOICES, 0).inline_row(),
    ParamSpec::ratio("drive", 1.0, 16.0, 2.0).inline_row(),
    ParamSpec::freq("tone", 1_000.0, 20_000.0, 20_000.0),
    ParamSpec::level("trim", -24.0, 24.0, 0.0).inline_row(),
    ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 1.0).inline_row(),
];

/// Static parameter table (spec `.agents/spec/effects.md` E5).
pub static DESCRIPTOR: EffectDescriptor = EffectDescriptor {
    name: "Saturation",
    params: &SATURATION_PARAMS,
    stereo: StereoClass::Mono,
};

/// `sinc(x) = sin(πx) / (πx)`, stable at the origin.
fn sinc(x: f32) -> f32 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

/// Blackman-windowed halfband prototype split into its two polyphase arms.
///
/// The ideal halfband `0.5·sinc(n/2)` has zeros at every even offset except
/// the center; the symmetric window preserves them, so the odd arm holds all
/// the transition energy. Normalizing the even-arm sum to 1 pins the DC gain.
fn halfband_arms() -> ([f32; UP_EVEN_LEN], [f32; UP_ODD_LEN]) {
    const TAPS: usize = UP_EVEN_LEN + UP_ODD_LEN;
    let mut h = [0.0f32; TAPS];
    let center = (UP_EVEN_LEN - 1) as f32;
    for (index, coeff) in h.iter_mut().enumerate() {
        let n = index as f32 - center;
        let m = index as f32 / (TAPS - 1) as f32;
        let window = 0.42 - 0.5 * (TAU * m).cos() + 0.08 * (2.0 * TAU * m).cos();
        *coeff = 0.5 * sinc(n * 0.5) * window;
    }
    // Normalize the **total** sum to unity: the upsampler and downsampler
    // each contribute their DC gain, and the cascade must be transparent
    // (Σh = 1 each). Normalizing the even arm alone would double the gain.
    let dc: f32 = h.iter().sum();
    for coeff in &mut h {
        *coeff /= dc;
    }
    let mut even = [0.0f32; UP_EVEN_LEN];
    let mut odd = [0.0f32; UP_ODD_LEN];
    for (k, slot) in even.iter_mut().enumerate() {
        *slot = h[2 * k];
    }
    for (k, slot) in odd.iter_mut().enumerate() {
        *slot = h[2 * k + 1];
    }
    (even, odd)
}

/// Fixed-capacity sample history; `get(k)` reads `k` samples ago.
struct History<const N: usize> {
    buf: [f32; N],
    pos: usize,
}

impl<const N: usize> History<N> {
    fn new() -> Self {
        Self {
            buf: [0.0; N],
            pos: 0,
        }
    }

    fn push(&mut self, value: f32) {
        self.buf[self.pos] = value;
        self.pos = (self.pos + 1) % N;
    }

    fn get(&self, steps: usize) -> f32 {
        self.buf[(self.pos + N - 1 - steps) % N]
    }
}

/// Per-channel oversampling state.
struct Chain {
    input: History<UP_EVEN_LEN>,
    even: History<UP_EVEN_LEN>,
    /// The downsampler's odd arm reads one sample further back than the even
    /// arm (`z[2n−(2k+1)] = z_odd[n−k−1]`), so it needs the extra slot.
    odd: History<UP_EVEN_LEN>,
}

impl Chain {
    fn new() -> Self {
        Self {
            input: History::new(),
            even: History::new(),
            odd: History::new(),
        }
    }
}

/// The memoryless shaper; `mode` follows the `MODE_CHOICES` index.
fn shape(mode: u8, drive: f32, x: f32) -> f32 {
    let driven = x * drive;
    match mode {
        1 => driven.clamp(-1.0, 1.0),
        2 => {
            // asin(sin(π/2·x)) / (π/2) folds ℝ into −1..1 and is the identity
            // for |x| ≤ 1.
            (FRAC_PI_2 * driven).sin().asin() / FRAC_PI_2
        }
        _ => driven.tanh(),
    }
}

/// Saturation (1 in/1 out per channel, 16 samples of wet latency).
pub struct SaturationProcessor {
    sample_rate: f32,
    params: EffectParamBank,
    chains: [Chain; 2],
    tone_lp: [f32; 2],
    up_even: [f32; UP_EVEN_LEN],
    up_odd: [f32; UP_ODD_LEN],
}

impl SaturationProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction tolerance).
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        let (up_even, up_odd) = halfband_arms();
        Self {
            sample_rate,
            params: EffectParamBank::new(&DESCRIPTOR, sample_rate),
            chains: [Chain::new(), Chain::new()],
            tone_lp: [0.0; 2],
            up_even,
            up_odd,
        }
    }

    /// Current (settled) value of a slot, for tests and read-back.
    pub fn value(&self, slot: u8) -> f32 {
        self.params.value(slot)
    }
}

impl Processor for SaturationProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        let channels = buffer.channels().min(2);
        if frames == 0 || channels == 0 {
            return;
        }

        let mode = self
            .params
            .advance(SLOT_MODE, frames)
            .round()
            .clamp(0.0, 2.0) as u8;
        let drive = self.params.advance(SLOT_DRIVE, frames).max(0.0);
        let tone = self
            .params
            .advance(SLOT_TONE, frames)
            .clamp(1_000.0, 0.45 * self.sample_rate);
        let trim = 10f32.powf(self.params.advance(SLOT_TRIM, frames) / 20.0);
        let tone_coeff = (-TAU * tone / self.sample_rate).exp();
        let tone_gain = 1.0 - tone_coeff;

        for index in 0..frames {
            let mix = self.params.tick(SLOT_MIX).clamp(0.0, 1.0);

            for ch in 0..channels {
                let input = buffer
                    .channel(ch)
                    .and_then(|channel| channel.get(index).copied())
                    .unwrap_or(0.0);
                let chain = &mut self.chains[ch];
                chain.input.push(input);

                // Upsample: the even arm reconstructs samples, the odd arm
                // synthesizes the midpoints. Zero-stuffing halves the
                // spectral energy, so the reconstruction filter runs at
                // twice the shared coefficient sum (Σh_up = 2, Σh_down = 1).
                let mut y_even = 0.0;
                for (k, coeff) in self.up_even.iter().enumerate() {
                    y_even += coeff * chain.input.get(k);
                }
                let mut y_odd = 0.0;
                for (k, coeff) in self.up_odd.iter().enumerate() {
                    y_odd += coeff * chain.input.get(k);
                }
                y_even += y_even;
                y_odd += y_odd;

                let z_even = shape(mode, drive, y_even);
                let z_odd = shape(mode, drive, y_odd);
                chain.even.push(z_even);
                chain.odd.push(z_odd);

                // Downsample through the same halfband. Tap `j` of the 2×
                // convolution reads `z[2n−j]`: even taps land on the even
                // history at offset k, odd taps on the odd history at
                // offset k+1 (the midpoint sample trails by one).
                let mut wet = 0.0;
                for (k, coeff) in self.up_even.iter().enumerate() {
                    wet += coeff * chain.even.get(k);
                }
                for (k, coeff) in self.up_odd.iter().enumerate() {
                    wet += coeff * chain.odd.get(k + 1);
                }
                self.tone_lp[ch] += tone_gain * (wet - self.tone_lp[ch]);
                let shaped = self.tone_lp[ch] * trim;

                if let Some(sample) = buffer
                    .channel_mut(ch)
                    .and_then(|channel| channel.get_mut(index))
                {
                    *sample = input * (1.0 - mix) + shaped * mix;
                }
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::BuiltinParam(slot) => self.params.set(slot, value),
            _ => false,
        }
    }

    fn latency_samples(&self) -> usize {
        WET_LATENCY
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::{
        DESCRIPTOR, SLOT_DRIVE, SLOT_MIX, SLOT_MODE, SLOT_TONE, SLOT_TRIM, SaturationProcessor,
        WET_LATENCY,
    };
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::effect::validate_descriptor;
    use crate::realtime::processor::Processor;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn ctx() -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_active: true,
            transport_dt_ticks: 0.0,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        }
    }

    fn run_mono(shaper: &mut SaturationProcessor, signal: &[f32]) -> Vec<f32> {
        let mut buffer = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(BLOCK) {
            let frames = chunk.len();
            buffer.set_len(frames);
            if let Some(channel) = buffer.channel_mut(0) {
                channel[..frames].copy_from_slice(chunk);
            }
            shaper.process(&ctx(), &mut buffer);
            if let Some(channel) = buffer.channel(0) {
                out.extend_from_slice(&channel[..frames]);
            }
        }
        out
    }

    fn sine(freq: f32, seconds: f32, amplitude: f32) -> Vec<f32> {
        let n = (SR * seconds) as usize;
        (0..n)
            .map(|i| amplitude * (TAU * freq * i as f32 / SR).sin())
            .collect()
    }

    fn set(shaper: &mut SaturationProcessor, slot: u8, value: f32) {
        assert!(
            shaper.set_param(ParamId::BuiltinParam(slot), value),
            "slot {slot} must be accepted"
        );
    }

    #[test]
    fn descriptor_is_well_formed() {
        assert_eq!(validate_descriptor(&DESCRIPTOR), Ok(()));
        assert_eq!(DESCRIPTOR.name, "Saturation");
        assert_eq!(DESCRIPTOR.inline_slots().count(), 4);
    }

    #[test]
    fn silence_in_silence_out() {
        let mut shaper = SaturationProcessor::new(SR);
        set(&mut shaper, SLOT_DRIVE, 16.0);
        set(&mut shaper, SLOT_MIX, 1.0);
        let out = run_mono(&mut shaper, &vec![0.0; BLOCK * 64]);
        assert!(out.iter().all(|s| s.abs() < 1e-9));
    }

    #[test]
    fn full_scale_stays_finite() {
        let mut shaper = SaturationProcessor::new(SR);
        set(&mut shaper, SLOT_DRIVE, 16.0);
        set(&mut shaper, SLOT_MODE, 2.0);
        set(&mut shaper, SLOT_MIX, 1.0);
        let out = run_mono(&mut shaper, &sine(440.0, 1.0, 1.0));
        assert!(out.iter().all(|s| s.is_finite()));
        let peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        // tanh and folding both bound the wet path near unity; the halfband
        // ringing can overshoot slightly.
        assert!(peak < 1.5, "shaper must bound the signal, peak {peak}");
    }

    #[test]
    fn mix_zero_is_a_sample_exact_passthrough() {
        let mut shaper = SaturationProcessor::new(SR);
        set(&mut shaper, SLOT_MIX, 0.0);
        set(&mut shaper, SLOT_DRIVE, 8.0);
        let input = sine(330.0, 0.5, 0.7);
        let out = run_mono(&mut shaper, &input);
        assert_eq!(out, input, "mix = 0 must return the dry signal bit for bit");
    }

    /// With the shaper near-identity (hard clip at unity drive never engages
    /// on a ±0.5 signal) the wet path must reproduce the input delayed by
    /// exactly `WET_LATENCY` samples — this is what PDC aligns to.
    #[test]
    fn wet_path_delays_by_the_reported_latency() {
        let mut shaper = SaturationProcessor::new(SR);
        set(&mut shaper, SLOT_MIX, 1.0);
        set(&mut shaper, SLOT_DRIVE, 1.0);
        set(&mut shaper, SLOT_MODE, 1.0);
        set(&mut shaper, SLOT_TONE, 20_000.0);
        set(&mut shaper, SLOT_TRIM, 0.0);
        let input = sine(997.0, 0.5, 0.5);
        let out = run_mono(&mut shaper, &input);
        let settle = WET_LATENCY + 4_096;
        let mut worst = 0.0f32;
        for i in settle..input.len() - 64 {
            worst = worst.max((out[i] - input[i - WET_LATENCY]).abs());
        }
        assert!(
            worst < 0.02,
            "wet path must track the input at the reported latency, worst {worst}"
        );
    }

    /// The impulse lands exactly one latency later — the PDC contract.
    #[test]
    fn impulse_peaks_at_the_reported_latency() {
        let mut shaper = SaturationProcessor::new(SR);
        set(&mut shaper, SLOT_MIX, 1.0);
        set(&mut shaper, SLOT_DRIVE, 1.0);
        set(&mut shaper, SLOT_MODE, 1.0);
        let mut input = vec![0.0; BLOCK * 4];
        input[0] = 1.0;
        let out = run_mono(&mut shaper, &input);
        let (peak_index, _) = out
            .iter()
            .enumerate()
            .fold((0usize, 0.0f32), |(bi, bv), (i, s)| {
                if s.abs() > bv { (i, s.abs()) } else { (bi, bv) }
            });
        assert_eq!(
            peak_index, WET_LATENCY,
            "impulse peak must sit at the latency the processor reports"
        );
    }

    /// Soft saturation at high drive must bound the output near unity while
    /// keeping harmonic energy (a sine comes out richer, not quieter-boring).
    #[test]
    fn soft_mode_bounds_high_drive() {
        let mut shaper = SaturationProcessor::new(SR);
        set(&mut shaper, SLOT_MIX, 1.0);
        set(&mut shaper, SLOT_DRIVE, 16.0);
        set(&mut shaper, SLOT_MODE, 0.0);
        let out = run_mono(&mut shaper, &sine(220.0, 0.5, 1.0));
        let settle = 8_192;
        let peak = out[settle..].iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(peak < 1.2, "tanh must soft-clip, peak {peak}");
        assert!(peak > 0.5, "saturated tone must stay audible, peak {peak}");
    }

    #[test]
    fn params_are_clamped_and_unknown_ids_rejected() {
        let mut shaper = SaturationProcessor::new(SR);
        set(&mut shaper, SLOT_DRIVE, 99.0);
        assert!((shaper.value(SLOT_DRIVE) - 16.0).abs() < 1e-6);
        set(&mut shaper, SLOT_TONE, f32::NAN);
        // The tone smoother works in the log domain, so the round trip back
        // to linear carries f32 noise: compare relatively.
        let tone = shaper.value(SLOT_TONE);
        assert!(
            (tone / 20_000.0 - 1.0).abs() < 1e-5,
            "tone fell back to {tone}, expected ≈ 20 kHz"
        );
        assert!(!shaper.set_param(ParamId::BuiltinParam(42), 1.0));
        assert!(!shaper.set_param(ParamId::MasterGain, 1.0));
    }
}
