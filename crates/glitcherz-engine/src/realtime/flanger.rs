//! Stereo flanger (spec `.agents/spec/effects.md` E4).
//!
//! A single fractional-delay tap around a 2.75 ms centre (0.5–5 ms) with
//! **feedback**, producing the classic comb sweep. The right channel's LFO is
//! offset by `width · 0.5` of a period (`width = 0` → both channels in phase).
//!
//! `feedback` is clamped to ±0.95 (spec E4): the loop still interpolates on
//! read, so it needs a margin below ±1 to stay bounded.
//!
//! All state pre-allocated; `process` is allocation-, lock- and panic-free.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::EffectParamBank;
use crate::realtime::effect::descriptor::{EffectDescriptor, ParamSpec, StereoClass, Unit};
use crate::realtime::mod_delay::{Lfo, ModDelayLine, sampler, sine};
use crate::realtime::processor::Processor;

/// Slot of the LFO rate (Hz).
pub const SLOT_RATE: u8 = 0;
/// Slot of the modulation depth.
pub const SLOT_DEPTH: u8 = 1;
/// Slot of the feedback amount (`-0.95..0.95`).
pub const SLOT_FEEDBACK: u8 = 2;
/// Slot of the stereo width.
pub const SLOT_WIDTH: u8 = 3;
/// Slot of the dry/wet mix.
pub const SLOT_MIX: u8 = 4;

/// Centre delay of the tap (ms).
const CENTER_MS: f32 = 2.75;
/// Modulation swing at `depth = 1` (ms): the tap sweeps 0.5..5 ms.
const SWING_MS: f32 = 2.25;
/// Delay-line capacity (ms), enough for the centre plus the full swing.
const MAX_DELAY_MS: f32 = 10.0;
/// Feedback is clamped to this magnitude (spec E4: keep 5 % margin below ±1).
pub const MAX_FEEDBACK: f32 = 0.95;

static FLANGER_PARAMS: [ParamSpec; 5] = [
    ParamSpec::freq("rate", 0.02, 5.0, 0.5).inline_row(),
    sampler("depth", Unit::Percent, 0.0, 1.0, 0.7, 10.0).inline_row(),
    sampler(
        "feedback",
        Unit::Ratio,
        -MAX_FEEDBACK,
        MAX_FEEDBACK,
        0.5,
        10.0,
    ),
    sampler("width", Unit::Percent, 0.0, 1.0, 0.5, 15.0),
    ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 0.5).inline_row(),
];

/// Static parameter table (spec `.agents/spec/effects.md` E4).
pub static DESCRIPTOR: EffectDescriptor = EffectDescriptor {
    name: "Flanger",
    params: &FLANGER_PARAMS,
    stereo: StereoClass::Mono,
};

/// Stereo flanger (1 in/1 out per channel, zero latency).
pub struct FlangerProcessor {
    sample_rate: f32,
    params: EffectParamBank,
    lines: [ModDelayLine; 2],
    lfo: Lfo,
}

impl FlangerProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction tolerance).
    pub fn new(sample_rate: f32) -> Self {
        let sr = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        let capacity = (MAX_DELAY_MS * 0.001 * sr).round() as usize + 2;
        Self {
            sample_rate: sr,
            params: EffectParamBank::new(&DESCRIPTOR, sr),
            lines: [
                ModDelayLine::with_capacity(capacity),
                ModDelayLine::with_capacity(capacity),
            ],
            lfo: Lfo::default(),
        }
    }

    /// Current (settled) value of a slot, for tests and read-back.
    pub fn value(&self, slot: u8) -> f32 {
        self.params.value(slot)
    }
}

impl Processor for FlangerProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        let channels = buffer.channels().min(2);
        if frames == 0 || channels == 0 {
            return;
        }

        let rate = self.params.advance(SLOT_RATE, frames);
        let center = CENTER_MS * 0.001 * self.sample_rate;
        let swing_max = SWING_MS * 0.001 * self.sample_rate;
        let sample_rate = self.sample_rate;

        for index in 0..frames {
            let depth = self.params.tick(SLOT_DEPTH).clamp(0.0, 1.0);
            let feedback = self
                .params
                .tick(SLOT_FEEDBACK)
                .clamp(-MAX_FEEDBACK, MAX_FEEDBACK);
            let width = self.params.tick(SLOT_WIDTH).clamp(0.0, 1.0);
            let mix = self.params.tick(SLOT_MIX).clamp(0.0, 1.0);

            let phase = self.lfo.advance(rate, sample_rate);
            let right_offset = width * 0.5;
            let swing = depth * swing_max;

            for ch in 0..channels {
                let input = buffer
                    .channel(ch)
                    .and_then(|channel| channel.get(index).copied())
                    .unwrap_or(0.0);
                let channel_phase = if ch == 0 { phase } else { phase + right_offset };
                let delay = center + swing * sine(channel_phase);

                let Some(line) = self.lines.get_mut(ch) else {
                    continue;
                };
                let wet = line.read(delay);
                line.push(input + wet * feedback);

                if let Some(sample) = buffer
                    .channel_mut(ch)
                    .and_then(|channel| channel.get_mut(index))
                {
                    *sample = input * (1.0 - mix) + wet * mix;
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
        0
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::{
        DESCRIPTOR, FlangerProcessor, MAX_FEEDBACK, SLOT_DEPTH, SLOT_FEEDBACK, SLOT_MIX, SLOT_RATE,
        SLOT_WIDTH,
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
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        }
    }

    fn run_mono(flanger: &mut FlangerProcessor, signal: &[f32]) -> Vec<f32> {
        let mut buffer = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(BLOCK) {
            let frames = chunk.len();
            buffer.set_len(frames);
            if let Some(channel) = buffer.channel_mut(0) {
                channel[..frames].copy_from_slice(chunk);
            }
            flanger.process(&ctx(), &mut buffer);
            if let Some(channel) = buffer.channel(0) {
                out.extend_from_slice(&channel[..frames]);
            }
        }
        out
    }

    fn run_stereo(
        flanger: &mut FlangerProcessor,
        left: &[f32],
        right: &[f32],
    ) -> (Vec<f32>, Vec<f32>) {
        let mut buffer = AudioBuffer::new(2, BLOCK);
        let mut out_l = Vec::with_capacity(left.len());
        let mut out_r = Vec::with_capacity(right.len());
        for index in 0..left.len().div_ceil(BLOCK) {
            let start = index * BLOCK;
            let end = (start + BLOCK).min(left.len());
            let frames = end - start;
            buffer.set_len(frames);
            if let Some(channel) = buffer.channel_mut(0) {
                channel[..frames].copy_from_slice(&left[start..end]);
            }
            if let Some(channel) = buffer.channel_mut(1) {
                channel[..frames].copy_from_slice(&right[start..end]);
            }
            flanger.process(&ctx(), &mut buffer);
            if let Some(channel) = buffer.channel(0) {
                out_l.extend_from_slice(&channel[..frames]);
            }
            if let Some(channel) = buffer.channel(1) {
                out_r.extend_from_slice(&channel[..frames]);
            }
        }
        (out_l, out_r)
    }

    fn sine(freq: f32, seconds: f32) -> Vec<f32> {
        let n = (SR * seconds) as usize;
        (0..n).map(|i| (TAU * freq * i as f32 / SR).sin()).collect()
    }

    fn set(flanger: &mut FlangerProcessor, slot: u8, value: f32) {
        assert!(
            flanger.set_param(ParamId::BuiltinParam(slot), value),
            "slot {slot} must be accepted"
        );
    }

    #[test]
    fn descriptor_is_well_formed() {
        assert_eq!(validate_descriptor(&DESCRIPTOR), Ok(()));
        assert_eq!(DESCRIPTOR.name, "Flanger");
        assert_eq!(DESCRIPTOR.inline_slots().count(), 3);
    }

    #[test]
    fn silence_in_silence_out() {
        let mut flanger = FlangerProcessor::new(SR);
        set(&mut flanger, SLOT_MIX, 1.0);
        set(&mut flanger, SLOT_DEPTH, 1.0);
        set(&mut flanger, SLOT_FEEDBACK, MAX_FEEDBACK);
        set(&mut flanger, SLOT_RATE, 5.0);
        let out = run_mono(&mut flanger, &vec![0.0; BLOCK * 64]);
        assert!(out.iter().all(|s| s.abs() < 1e-9));
    }

    #[test]
    fn full_scale_stays_finite_at_max_feedback() {
        let mut flanger = FlangerProcessor::new(SR);
        set(&mut flanger, SLOT_MIX, 1.0);
        set(&mut flanger, SLOT_DEPTH, 1.0);
        set(&mut flanger, SLOT_FEEDBACK, MAX_FEEDBACK);
        set(&mut flanger, SLOT_RATE, 5.0);
        let out = run_mono(&mut flanger, &sine(440.0, 2.0));
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(out.iter().fold(0.0f32, |a, s| a.max(s.abs())) < 16.0);
    }

    #[test]
    fn mix_zero_is_a_sample_exact_passthrough() {
        let mut flanger = FlangerProcessor::new(SR);
        set(&mut flanger, SLOT_MIX, 0.0);
        set(&mut flanger, SLOT_DEPTH, 1.0);
        let input = sine(330.0, 0.5);
        let out = run_mono(&mut flanger, &input);
        assert_eq!(out, input, "mix = 0 must return the dry signal bit for bit");
    }

    #[test]
    fn feedback_produces_a_decaying_comb() {
        // depth = 0 pins the tap at the 2.75 ms centre = 132 samples, so the
        // comb teeth land on exact integer multiples.
        let mut flanger = FlangerProcessor::new(SR);
        set(&mut flanger, SLOT_MIX, 1.0);
        set(&mut flanger, SLOT_DEPTH, 0.0);
        set(&mut flanger, SLOT_FEEDBACK, 0.5);

        let mut signal = vec![0.0f32; 2_000];
        signal[0] = 1.0;
        let out = run_mono(&mut flanger, &signal);
        assert!(out[0].abs() < 1e-9, "100 % wet: t = 0 is silent");
        assert!((out[132] - 1.0).abs() < 1e-3, "first tooth at 132 samples");
        assert!((out[264] - 0.5).abs() < 1e-3, "second tooth = feedback");
        assert!((out[396] - 0.25).abs() < 1e-3, "third tooth = feedback²");
    }

    #[test]
    fn width_zero_keeps_both_channels_identical() {
        let left = sine(440.0, 0.5);
        let right = sine(440.0, 0.5);

        let mut mono = FlangerProcessor::new(SR);
        set(&mut mono, SLOT_MIX, 1.0);
        set(&mut mono, SLOT_DEPTH, 1.0);
        set(&mut mono, SLOT_WIDTH, 0.0);
        let (mono_l, mono_r) = run_stereo(&mut mono, &left, &right);
        for (l, r) in mono_l.iter().zip(mono_r.iter()) {
            assert!(
                (l - r).abs() < 1e-6,
                "width = 0 must keep the channels identical"
            );
        }

        let mut wide = FlangerProcessor::new(SR);
        set(&mut wide, SLOT_MIX, 1.0);
        set(&mut wide, SLOT_DEPTH, 1.0);
        set(&mut wide, SLOT_WIDTH, 1.0);
        let (wide_l, wide_r) = run_stereo(&mut wide, &left, &right);
        let spread: f32 = wide_l
            .iter()
            .zip(wide_r.iter())
            .map(|(l, r)| (l - r).abs())
            .sum();
        assert!(spread > 1.0, "width = 1 must decorrelate the channels");
    }

    #[test]
    fn params_are_clamped_and_unknown_ids_rejected() {
        let mut flanger = FlangerProcessor::new(SR);
        set(&mut flanger, SLOT_FEEDBACK, 5.0);
        assert!((flanger.value(SLOT_FEEDBACK) - MAX_FEEDBACK).abs() < 1e-6);
        set(&mut flanger, SLOT_FEEDBACK, -5.0);
        assert!((flanger.value(SLOT_FEEDBACK) + MAX_FEEDBACK).abs() < 1e-6);
        set(&mut flanger, SLOT_RATE, f32::NAN);
        assert!((flanger.value(SLOT_RATE) - 0.5).abs() < 1e-6);
        assert!(!flanger.set_param(ParamId::BuiltinParam(42), 1.0));
        assert!(!flanger.set_param(ParamId::MasterGain, 1.0));
    }
}
