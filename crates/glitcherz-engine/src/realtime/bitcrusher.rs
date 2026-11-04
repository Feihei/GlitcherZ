//! Bitcrusher (spec `.agents/spec/effects.md` E5).
//!
//! Two classic lo-fi degradations in one stage: **quantization** to a reduced
//! word length (fractional bit values are allowed, the step size simply gets
//! continuous) and **sample & hold** downsampling. Both are driven per block;
//! only the mix ramps per sample.
//!
//! All state pre-allocated; `process` is allocation-, lock- and panic-free.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::EffectParamBank;
use crate::realtime::effect::descriptor::{EffectDescriptor, ParamSpec, StereoClass, Unit};
use crate::realtime::mod_delay::sampler;
use crate::realtime::processor::Processor;

/// Slot of the input gain before quantization.
pub const SLOT_DRIVE: u8 = 0;
/// Slot of the quantization depth (bits, fractional).
pub const SLOT_BITS: u8 = 1;
/// Slot of the sample & hold ratio (1 = off).
pub const SLOT_DOWNSAMPLE: u8 = 2;
/// Slot of the dry/wet mix.
pub const SLOT_MIX: u8 = 3;

static BITCRUSHER_PARAMS: [ParamSpec; 4] = [
    ParamSpec::ratio("drive", 1.0, 16.0, 1.0).inline_row(),
    sampler("bits", Unit::Ratio, 2.0, 16.0, 8.0, 10.0).inline_row(),
    sampler("downsample", Unit::Ratio, 1.0, 32.0, 2.0, 10.0).inline_row(),
    ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 1.0).inline_row(),
];

/// Static parameter table (spec `.agents/spec/effects.md` E5).
pub static DESCRIPTOR: EffectDescriptor = EffectDescriptor {
    name: "Bitcrusher",
    params: &BITCRUSHER_PARAMS,
    stereo: StereoClass::Mono,
};

/// Bitcrusher (1 in/1 out per channel, zero latency).
pub struct BitcrusherProcessor {
    params: EffectParamBank,
    /// Held (quantized) value per channel, refreshed every `ratio` samples.
    hold: [f32; 2],
    /// Position inside the current hold period; continuous across blocks.
    phase: u32,
}

impl BitcrusherProcessor {
    /// Construction tolerance mirrors the other built-in effects.
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            params: EffectParamBank::new(&DESCRIPTOR, sample_rate),
            hold: [0.0; 2],
            phase: 0,
        }
    }

    /// Current (settled) value of a slot, for tests and read-back.
    pub fn value(&self, slot: u8) -> f32 {
        self.params.value(slot)
    }
}

impl Processor for BitcrusherProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        let channels = buffer.channels().min(2);
        if frames == 0 || channels == 0 {
            return;
        }

        let drive = self.params.advance(SLOT_DRIVE, frames);
        let bits = self.params.advance(SLOT_BITS, frames);
        let downsample = self.params.advance(SLOT_DOWNSAMPLE, frames);
        // Fractional bits keep the step size continuous; 2^(bits-1) is the
        // half-range of a signed integer of that depth.
        let levels = 2f32.powf(bits.clamp(2.0, 16.0) - 1.0);
        let ratio = downsample.round().max(1.0) as usize;
        if self.phase >= ratio as u32 {
            self.phase = 0;
        }

        for index in 0..frames {
            let mix = self.params.tick(SLOT_MIX).clamp(0.0, 1.0);

            if self.phase == 0 {
                for ch in 0..channels {
                    let input = buffer
                        .channel(ch)
                        .and_then(|channel| channel.get(index).copied())
                        .unwrap_or(0.0);
                    self.hold[ch] = (input * drive * levels).round() / levels;
                }
            }
            self.phase = if self.phase + 1 >= ratio as u32 {
                0
            } else {
                self.phase + 1
            };

            for ch in 0..channels {
                let input = buffer
                    .channel(ch)
                    .and_then(|channel| channel.get(index).copied())
                    .unwrap_or(0.0);
                if let Some(sample) = buffer
                    .channel_mut(ch)
                    .and_then(|channel| channel.get_mut(index))
                {
                    *sample = input * (1.0 - mix) + self.hold[ch] * mix;
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
        BitcrusherProcessor, DESCRIPTOR, SLOT_BITS, SLOT_DOWNSAMPLE, SLOT_DRIVE, SLOT_MIX,
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

    fn run_mono(crusher: &mut BitcrusherProcessor, signal: &[f32]) -> Vec<f32> {
        let mut buffer = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(BLOCK) {
            let frames = chunk.len();
            buffer.set_len(frames);
            if let Some(channel) = buffer.channel_mut(0) {
                channel[..frames].copy_from_slice(chunk);
            }
            crusher.process(&ctx(), &mut buffer);
            if let Some(channel) = buffer.channel(0) {
                out.extend_from_slice(&channel[..frames]);
            }
        }
        out
    }

    fn sine(freq: f32, seconds: f32) -> Vec<f32> {
        let n = (SR * seconds) as usize;
        (0..n).map(|i| (TAU * freq * i as f32 / SR).sin()).collect()
    }

    fn set(crusher: &mut BitcrusherProcessor, slot: u8, value: f32) {
        assert!(
            crusher.set_param(ParamId::BuiltinParam(slot), value),
            "slot {slot} must be accepted"
        );
    }

    #[test]
    fn descriptor_is_well_formed() {
        assert_eq!(validate_descriptor(&DESCRIPTOR), Ok(()));
        assert_eq!(DESCRIPTOR.name, "Bitcrusher");
        assert_eq!(DESCRIPTOR.inline_slots().count(), 4);
    }

    #[test]
    fn silence_in_silence_out() {
        let mut crusher = BitcrusherProcessor::new(SR);
        set(&mut crusher, SLOT_DRIVE, 16.0);
        set(&mut crusher, SLOT_BITS, 2.0);
        set(&mut crusher, SLOT_DOWNSAMPLE, 32.0);
        set(&mut crusher, SLOT_MIX, 1.0);
        let out = run_mono(&mut crusher, &vec![0.0; BLOCK * 64]);
        assert!(out.iter().all(|s| s.abs() < 1e-9));
    }

    #[test]
    fn full_scale_stays_finite() {
        let mut crusher = BitcrusherProcessor::new(SR);
        set(&mut crusher, SLOT_DRIVE, 16.0);
        set(&mut crusher, SLOT_BITS, 2.0);
        set(&mut crusher, SLOT_DOWNSAMPLE, 32.0);
        set(&mut crusher, SLOT_MIX, 1.0);
        let out = run_mono(&mut crusher, &sine(440.0, 1.0));
        assert!(out.iter().all(|s| s.is_finite()));
        // drive 16 pushes the signal far past full scale by design.
        let peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(peak < 20.0, "peak {peak} escapes the drive range");
    }

    #[test]
    fn mix_zero_is_a_sample_exact_passthrough() {
        let mut crusher = BitcrusherProcessor::new(SR);
        set(&mut crusher, SLOT_MIX, 0.0);
        set(&mut crusher, SLOT_DRIVE, 8.0);
        set(&mut crusher, SLOT_DOWNSAMPLE, 7.0);
        let input = sine(330.0, 0.5);
        let out = run_mono(&mut crusher, &input);
        assert_eq!(out, input, "mix = 0 must return the dry signal bit for bit");
    }

    #[test]
    fn clean_settings_are_transparent() {
        // drive 1, 16 bits, no hold: the quantizer's step is 2^-15, so the
        // round trip stays within one step of the input.
        let mut crusher = BitcrusherProcessor::new(SR);
        set(&mut crusher, SLOT_DRIVE, 1.0);
        set(&mut crusher, SLOT_BITS, 16.0);
        set(&mut crusher, SLOT_DOWNSAMPLE, 1.0);
        set(&mut crusher, SLOT_MIX, 1.0);
        let input = sine(330.0, 0.25);
        let out = run_mono(&mut crusher, &input);
        let worst = input
            .iter()
            .zip(out.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-4, "clean settings drifted by {worst}");
    }

    #[test]
    fn downsample_creates_stair_steps() {
        // With a hold ratio of 8 the output repeats each value 8 times; some
        // run of three identical samples must exist, and a plain sine never
        // produces one.
        let mut crusher = BitcrusherProcessor::new(SR);
        set(&mut crusher, SLOT_DRIVE, 1.0);
        set(&mut crusher, SLOT_BITS, 16.0);
        set(&mut crusher, SLOT_DOWNSAMPLE, 8.0);
        set(&mut crusher, SLOT_MIX, 1.0);
        let input = sine(330.0, 0.25);
        let out = run_mono(&mut crusher, &input);
        let repeated = out
            .windows(3)
            .any(|w| w[0] == w[1] && w[1] == w[2] && w[0] != 0.0);
        assert!(repeated, "sample & hold must flatten the signal");
    }

    #[test]
    fn params_are_clamped_and_unknown_ids_rejected() {
        let mut crusher = BitcrusherProcessor::new(SR);
        set(&mut crusher, SLOT_BITS, 99.0);
        assert!((crusher.value(SLOT_BITS) - 16.0).abs() < 1e-6);
        set(&mut crusher, SLOT_DOWNSAMPLE, 0.0);
        assert!((crusher.value(SLOT_DOWNSAMPLE) - 1.0).abs() < 1e-6);
        set(&mut crusher, SLOT_DRIVE, f32::NAN);
        assert!((crusher.value(SLOT_DRIVE) - 1.0).abs() < 1e-6);
        assert!(!crusher.set_param(ParamId::BuiltinParam(42), 1.0));
        assert!(!crusher.set_param(ParamId::MasterGain, 1.0));
    }
}
