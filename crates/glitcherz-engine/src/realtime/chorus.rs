//! Stereo chorus (spec `.agents/spec/effects.md` E4).
//!
//! Two fractional-delay taps around a 21 ms centre sweep (12–30 ms) with no
//! feedback, so a dry signal gains a shimmering, detuned double rather than a
//! resonance. The right channel's LFO is offset by `width · 0.5` of a period
//! (`width = 0` → both channels modulate in phase).
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
/// Slot of the stereo width.
pub const SLOT_WIDTH: u8 = 2;
/// Slot of the dry/wet mix.
pub const SLOT_MIX: u8 = 3;

/// Centre delay of the two taps (ms).
const CENTER_MS: f32 = 21.0;
/// Modulation swing at `depth = 1` (ms): the centre sweeps 12..30 ms.
const SWING_MS: f32 = 9.0;
/// Phase offset of the second tap (a non-symmetric split sounds wider).
const TAP2_PHASE: f32 = 0.37;
/// Delay-line capacity (ms), enough for the centre plus the full swing.
const MAX_DELAY_MS: f32 = 40.0;

static CHORUS_PARAMS: [ParamSpec; 4] = [
    ParamSpec::freq("rate", 0.05, 8.0, 1.0).inline_row(),
    sampler("depth", Unit::Percent, 0.0, 1.0, 0.5, 10.0).inline_row(),
    sampler("width", Unit::Percent, 0.0, 1.0, 0.5, 15.0),
    ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 0.5).inline_row(),
];

/// Static parameter table (spec `.agents/spec/effects.md` E4).
pub static DESCRIPTOR: EffectDescriptor = EffectDescriptor {
    name: "Chorus",
    params: &CHORUS_PARAMS,
    stereo: StereoClass::Mono,
};

/// Stereo chorus (1 in/1 out per channel, zero latency).
pub struct ChorusProcessor {
    sample_rate: f32,
    params: EffectParamBank,
    lines: [ModDelayLine; 2],
    lfo: Lfo,
}

impl ChorusProcessor {
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

impl Processor for ChorusProcessor {
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
                let tap1 = center + swing * sine(channel_phase);
                let tap2 = center + swing * sine(channel_phase + TAP2_PHASE);

                let Some(line) = self.lines.get_mut(ch) else {
                    continue;
                };
                let wet = 0.5 * (line.read(tap1) + line.read(tap2));
                line.push(input);

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

    use super::{ChorusProcessor, DESCRIPTOR, SLOT_DEPTH, SLOT_MIX, SLOT_RATE, SLOT_WIDTH};
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

    fn run_mono(chorus: &mut ChorusProcessor, signal: &[f32]) -> Vec<f32> {
        let mut buffer = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(BLOCK) {
            let frames = chunk.len();
            buffer.set_len(frames);
            if let Some(channel) = buffer.channel_mut(0) {
                channel[..frames].copy_from_slice(chunk);
            }
            chorus.process(&ctx(), &mut buffer);
            if let Some(channel) = buffer.channel(0) {
                out.extend_from_slice(&channel[..frames]);
            }
        }
        out
    }

    fn run_stereo(
        chorus: &mut ChorusProcessor,
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
            chorus.process(&ctx(), &mut buffer);
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

    fn set(chorus: &mut ChorusProcessor, slot: u8, value: f32) {
        assert!(
            chorus.set_param(ParamId::BuiltinParam(slot), value),
            "slot {slot} must be accepted"
        );
    }

    #[test]
    fn descriptor_is_well_formed() {
        assert_eq!(validate_descriptor(&DESCRIPTOR), Ok(()));
        assert_eq!(DESCRIPTOR.name, "Chorus");
        assert_eq!(DESCRIPTOR.inline_slots().count(), 3);
    }

    #[test]
    fn silence_in_silence_out() {
        let mut chorus = ChorusProcessor::new(SR);
        set(&mut chorus, SLOT_MIX, 1.0);
        set(&mut chorus, SLOT_DEPTH, 1.0);
        set(&mut chorus, SLOT_RATE, 8.0);
        let out = run_mono(&mut chorus, &vec![0.0; BLOCK * 64]);
        assert!(out.iter().all(|s| s.abs() < 1e-9));
    }

    #[test]
    fn full_scale_stays_finite() {
        let mut chorus = ChorusProcessor::new(SR);
        set(&mut chorus, SLOT_MIX, 1.0);
        set(&mut chorus, SLOT_DEPTH, 1.0);
        set(&mut chorus, SLOT_RATE, 8.0);
        let out = run_mono(&mut chorus, &sine(440.0, 1.0));
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(out.iter().fold(0.0f32, |a, s| a.max(s.abs())) < 4.0);
    }

    #[test]
    fn mix_zero_is_a_sample_exact_passthrough() {
        let mut chorus = ChorusProcessor::new(SR);
        set(&mut chorus, SLOT_MIX, 0.0);
        set(&mut chorus, SLOT_DEPTH, 1.0);
        let input = sine(330.0, 0.5);
        let out = run_mono(&mut chorus, &input);
        assert_eq!(out, input, "mix = 0 must return the dry signal bit for bit");
    }

    #[test]
    fn depth_zero_leaves_a_steady_detune() {
        // With no modulation the two taps sit at fixed delays, so a sine comes
        // out as a sum of two phase-shifted copies — still a sine of the same
        // frequency but not equal to the dry signal.
        let mut chorus = ChorusProcessor::new(SR);
        set(&mut chorus, SLOT_MIX, 1.0);
        set(&mut chorus, SLOT_DEPTH, 0.0);
        let input = sine(200.0, 0.5);
        let out = run_mono(&mut chorus, &input);
        let settled = out.len() - BLOCK * 4;
        let diff: f32 = (settled..out.len())
            .map(|i| (out[i] - input[i]).abs())
            .sum();
        assert!(diff > 0.0, "two fixed taps must shift the signal");
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn modulation_moves_the_taps_over_time() {
        // A steady tone through a modulated chorus breathes: the output level
        // varies, unlike a fixed chorus.
        let mut chorus = ChorusProcessor::new(SR);
        set(&mut chorus, SLOT_MIX, 1.0);
        set(&mut chorus, SLOT_DEPTH, 1.0);
        set(&mut chorus, SLOT_RATE, 3.0);
        let out = run_mono(&mut chorus, &sine(440.0, 1.0));
        let window = 2_048;
        let rms = |from: usize| -> f32 {
            let slice = &out[from..from + window];
            (slice.iter().map(|s| s * s).sum::<f32>() / window as f32).sqrt()
        };
        let a = rms(BLOCK * 8);
        let b = rms(BLOCK * 8 + window);
        assert!(
            (a - b).abs() > 1e-4,
            "a modulated chorus must vary between windows ({a} vs {b})"
        );
    }

    #[test]
    fn width_zero_modulates_both_channels_in_phase() {
        let left = sine(440.0, 0.5);
        let right = sine(440.0, 0.5);

        let mut mono = ChorusProcessor::new(SR);
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

        let mut wide = ChorusProcessor::new(SR);
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
        let mut chorus = ChorusProcessor::new(SR);
        set(&mut chorus, SLOT_DEPTH, 9.0);
        assert!((chorus.value(SLOT_DEPTH) - 1.0).abs() < 1e-6);
        set(&mut chorus, SLOT_RATE, f32::NAN);
        assert!((chorus.value(SLOT_RATE) - 1.0).abs() < 1e-6);
        assert!(!chorus.set_param(ParamId::BuiltinParam(42), 1.0));
        assert!(!chorus.set_param(ParamId::MasterGain, 1.0));
    }
}
