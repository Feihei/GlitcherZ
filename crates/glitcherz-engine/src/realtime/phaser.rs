//! Phaser (spec `.agents/spec/effects.md` E5).
//!
//! A cascade of first-order all-pass sections whose corner frequencies are
//! spread geometrically (`f_k = F · spread^k`) and swept by a sine LFO. The
//! dry/wet blend of an all-pass cascade is what creates the comb notches;
//! feedback from the chain output back to its input deepens them.
//!
//! All-pass sections run in TPT (zero-delay-feedback) form: a one-pole low
//! pass with `AP = 2·LP − x`, which is the exact bilinear all-pass. Section
//! coefficients are computed at block boundaries (start and end) and
//! linearly interpolated per sample, so the LFO moves smoothly without a
//! per-sample `tan`.
//!
//! All state pre-allocated; `process` is allocation-, lock- and panic-free.

use std::f32::consts::PI;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::EffectParamBank;
use crate::realtime::effect::descriptor::{EffectDescriptor, ParamSpec, StereoClass, Unit};
use crate::realtime::mod_delay::{sampler, sine};
use crate::realtime::processor::Processor;

/// Slot of the LFO rate (Hz).
pub const SLOT_RATE: u8 = 0;
/// Slot of the sweep depth (0..1).
pub const SLOT_DEPTH: u8 = 1;
/// Slot of the chain feedback.
pub const SLOT_FEEDBACK: u8 = 2;
/// Slot of the geometric frequency ratio between stages.
pub const SLOT_SPREAD: u8 = 3;
/// Slot of the all-pass stage count (enum index).
pub const SLOT_STAGES: u8 = 4;
/// Slot of the dry/wet mix.
pub const SLOT_MIX: u8 = 5;

/// Maximum number of all-pass sections (the largest enum choice).
pub const MAX_STAGES: usize = 8;
/// Base corner frequency (Hz) at the LFO's zero crossing.
const F_CENTER: f32 = 120.0;
/// Sweep span at `depth = 1` (octaves, symmetric around the LFO zero).
const SWEEP_OCTAVES: f32 = 3.5;
/// Never let a section corner reach Nyquist (tan diverges).
const NYQUIST_MARGIN: f32 = 0.45;

static PHASER_PARAMS: [ParamSpec; 6] = [
    ParamSpec::freq("rate", 0.02, 8.0, 0.5).inline_row(),
    sampler("depth", Unit::Percent, 0.0, 1.0, 0.7, 10.0).inline_row(),
    sampler("feedback", Unit::Percent, -0.9, 0.9, 0.3, 10.0).inline_row(),
    ParamSpec::ratio("spread", 1.1, 2.6, 1.4),
    ParamSpec::enumeration("stages", &["2", "4", "6", "8"], 1),
    ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 0.5).inline_row(),
];

/// Static parameter table (spec `.agents/spec/effects.md` E5).
pub static DESCRIPTOR: EffectDescriptor = EffectDescriptor {
    name: "Phaser",
    params: &PHASER_PARAMS,
    stereo: StereoClass::Mono,
};

/// Stereo phaser (1 in/1 out per channel, zero latency).
pub struct PhaserProcessor {
    sample_rate: f32,
    params: EffectParamBank,
    /// TPT low-pass state per stage per channel.
    state: [[f32; MAX_STAGES]; 2],
    /// Last chain output per channel (fed back into the chain input).
    fb_state: [f32; 2],
    /// Normalized LFO phase in `0..1`.
    phase: f32,
    /// Block-start section coefficients (shared by both channels).
    g_start: [f32; MAX_STAGES],
    /// Block-end section coefficients the block interpolates toward.
    g_end: [f32; MAX_STAGES],
    stages: usize,
}

impl PhaserProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction tolerance).
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            sample_rate,
            params: EffectParamBank::new(&DESCRIPTOR, sample_rate),
            state: [[0.0; MAX_STAGES]; 2],
            fb_state: [0.0; 2],
            phase: 0.0,
            g_start: [0.0; MAX_STAGES],
            g_end: [0.0; MAX_STAGES],
            stages: 4,
        }
    }

    /// Current (settled) value of a slot, for tests and read-back.
    pub fn value(&self, slot: u8) -> f32 {
        self.params.value(slot)
    }

    /// Normalized ZDF gain `tan(π·f/sr) / (1 + tan(π·f/sr))` for one section.
    fn section_gain(&self, frequency: f32) -> f32 {
        let f = frequency.clamp(1.0, NYQUIST_MARGIN * self.sample_rate);
        let t = (PI * f / self.sample_rate).tan();
        t / (1.0 + t)
    }

    /// Recomputes the block boundary coefficients for both LFO phase ends.
    fn plan_block(&mut self, frames: usize) {
        let rate = self.params.advance(SLOT_RATE, frames);
        let depth_start = self.params.value(SLOT_DEPTH);
        let depth_end = self.params.advance(SLOT_DEPTH, frames);
        let spread = self.params.advance(SLOT_SPREAD, frames).clamp(1.0, 4.0);
        let index = self
            .params
            .advance(SLOT_STAGES, frames)
            .round()
            .clamp(0.0, 3.0) as usize;
        self.stages = index * 2 + 2;

        let phase_start = self.phase;
        let phase_end = phase_start + rate / self.sample_rate * frames as f32;
        self.phase = phase_end.fract();

        for stage in 0..self.stages {
            let ratio = spread.powi(stage as i32);
            let sweep_start = F_CENTER * (depth_start * sine(phase_start) * SWEEP_OCTAVES).exp2();
            let sweep_end = F_CENTER * (depth_end * sine(phase_end) * SWEEP_OCTAVES).exp2();
            self.g_start[stage] = self.section_gain(sweep_start * ratio);
            self.g_end[stage] = self.section_gain(sweep_end * ratio);
        }
    }
}

impl Processor for PhaserProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        let channels = buffer.channels().min(2);
        if frames == 0 || channels == 0 {
            return;
        }
        self.plan_block(frames);

        let stages = self.stages;
        let g_start = self.g_start;
        let g_end = self.g_end;
        let inv_frames = 1.0 / frames as f32;

        for index in 0..frames {
            let t = index as f32 * inv_frames;
            let feedback = self.params.tick(SLOT_FEEDBACK).clamp(-0.9, 0.9);
            let mix = self.params.tick(SLOT_MIX).clamp(0.0, 1.0);

            for ch in 0..channels {
                let input = buffer
                    .channel(ch)
                    .and_then(|channel| channel.get(index).copied())
                    .unwrap_or(0.0);

                // The dry tap stays untouched; feedback only excites the chain.
                let mut x = input + self.fb_state[ch] * feedback;
                for stage in 0..stages {
                    let g = g_start[stage] + (g_end[stage] - g_start[stage]) * t;
                    let v = g * (x - self.state[ch][stage]);
                    let lp = v + self.state[ch][stage];
                    self.state[ch][stage] = lp + v;
                    x = 2.0 * lp - x;
                }
                self.fb_state[ch] = x;

                if let Some(sample) = buffer
                    .channel_mut(ch)
                    .and_then(|channel| channel.get_mut(index))
                {
                    *sample = input * (1.0 - mix) + x * mix;
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
        DESCRIPTOR, MAX_STAGES, PhaserProcessor, SLOT_DEPTH, SLOT_FEEDBACK, SLOT_MIX, SLOT_RATE,
        SLOT_SPREAD, SLOT_STAGES,
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

    fn run_mono(phaser: &mut PhaserProcessor, signal: &[f32]) -> Vec<f32> {
        let mut buffer = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(BLOCK) {
            let frames = chunk.len();
            buffer.set_len(frames);
            if let Some(channel) = buffer.channel_mut(0) {
                channel[..frames].copy_from_slice(chunk);
            }
            phaser.process(&ctx(), &mut buffer);
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

    fn rms(signal: &[f32], from: usize, len: usize) -> f32 {
        let slice = &signal[from..from + len];
        (slice.iter().map(|s| s * s).sum::<f32>() / len as f32).sqrt()
    }

    fn set(phaser: &mut PhaserProcessor, slot: u8, value: f32) {
        assert!(
            phaser.set_param(ParamId::BuiltinParam(slot), value),
            "slot {slot} must be accepted"
        );
    }

    #[test]
    fn descriptor_is_well_formed() {
        assert_eq!(validate_descriptor(&DESCRIPTOR), Ok(()));
        assert_eq!(DESCRIPTOR.name, "Phaser");
        assert_eq!(DESCRIPTOR.inline_slots().count(), 4);
        assert_eq!(MAX_STAGES, 8);
    }

    #[test]
    fn silence_in_silence_out() {
        let mut phaser = PhaserProcessor::new(SR);
        set(&mut phaser, SLOT_MIX, 1.0);
        set(&mut phaser, SLOT_FEEDBACK, 0.9);
        set(&mut phaser, SLOT_RATE, 8.0);
        set(&mut phaser, SLOT_STAGES, 3.0);
        let out = run_mono(&mut phaser, &vec![0.0; BLOCK * 64]);
        assert!(out.iter().all(|s| s.abs() < 1e-9));
    }

    #[test]
    fn full_scale_stays_finite() {
        let mut phaser = PhaserProcessor::new(SR);
        set(&mut phaser, SLOT_MIX, 1.0);
        set(&mut phaser, SLOT_FEEDBACK, 0.9);
        set(&mut phaser, SLOT_RATE, 8.0);
        set(&mut phaser, SLOT_STAGES, 3.0);
        let out = run_mono(&mut phaser, &sine(440.0, 1.0));
        assert!(out.iter().all(|s| s.is_finite()));
        let peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(peak < 10.0, "feedback runaway: peak {peak}");
    }

    #[test]
    fn mix_zero_is_a_sample_exact_passthrough() {
        let mut phaser = PhaserProcessor::new(SR);
        set(&mut phaser, SLOT_MIX, 0.0);
        set(&mut phaser, SLOT_FEEDBACK, 0.8);
        let input = sine(330.0, 0.5);
        let out = run_mono(&mut phaser, &input);
        assert_eq!(out, input, "mix = 0 must return the dry signal bit for bit");
    }

    /// With no feedback and full wet the chain is a pure all-pass cascade:
    /// every frequency must come out at (almost) exactly the input level.
    #[test]
    fn all_pass_chain_preserves_amplitude() {
        let mut phaser = PhaserProcessor::new(SR);
        set(&mut phaser, SLOT_MIX, 1.0);
        set(&mut phaser, SLOT_FEEDBACK, 0.0);
        set(&mut phaser, SLOT_DEPTH, 0.0);
        set(&mut phaser, SLOT_STAGES, 3.0);
        for freq in [80.0, 300.0, 1200.0, 6000.0] {
            let input = sine(freq, 1.0);
            let out = run_mono(&mut phaser, &input);
            let tail = out.len() - 4_096;
            let ratio = rms(&out, tail, 4_096) / rms(&input, input.len() - 4_096, 4_096);
            assert!(
                (0.95..=1.05).contains(&ratio),
                "all-pass must preserve {freq} Hz amplitude, got {ratio}"
            );
        }
    }

    /// The half-wet blend of an all-pass cascade is frequency dependent (that
    /// is the entire point of a phaser): sweeping must show peaks and notches.
    #[test]
    fn half_wet_blend_moves_the_response() {
        let mut phaser = PhaserProcessor::new(SR);
        set(&mut phaser, SLOT_MIX, 0.5);
        set(&mut phaser, SLOT_FEEDBACK, 0.0);
        set(&mut phaser, SLOT_DEPTH, 0.0);
        set(&mut phaser, SLOT_STAGES, 3.0);
        let mut lo = f32::MAX;
        let mut hi = 0.0f32;
        for freq in [60.0, 120.0, 240.0, 480.0, 960.0, 1920.0, 3840.0, 7680.0] {
            let input = sine(freq, 0.5);
            let out = run_mono(&mut phaser, &input);
            let tail = out.len() - 4_096;
            let ratio = rms(&out, tail, 4_096) / rms(&input, input.len() - 4_096, 4_096);
            lo = lo.min(ratio);
            hi = hi.max(ratio);
        }
        assert!(
            hi - lo > 0.3,
            "the blend must shape the spectrum (lo {lo} hi {hi})"
        );
    }

    #[test]
    fn params_are_clamped_and_unknown_ids_rejected() {
        let mut phaser = PhaserProcessor::new(SR);
        set(&mut phaser, SLOT_FEEDBACK, 5.0);
        assert!((phaser.value(SLOT_FEEDBACK) - 0.9).abs() < 1e-6);
        set(&mut phaser, SLOT_RATE, f32::NAN);
        assert!((phaser.value(SLOT_RATE) - 0.5).abs() < 1e-6);
        assert!(!phaser.set_param(ParamId::BuiltinParam(42), 1.0));
        assert!(!phaser.set_param(ParamId::MasterGain, 1.0));
    }
}
