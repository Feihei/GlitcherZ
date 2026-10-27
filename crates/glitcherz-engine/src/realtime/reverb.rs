//! Feedback delay network reverb (spec `.agents/spec/effects.md` E3).
//!
//! An 8-line FDN (feedback delay network) instead of a Schroeder/Freeverb
//! stack: eight delay lines of mutually non-harmonic length feed a normalized
//! Hadamard mixing matrix, and each line carries a one-pole damping low-pass
//! plus a first-order high-pass (the low cut) **inside the loop** so the tail
//! loses highs and rumbles naturally instead of piling up low end.
//!
//! Feedback gain per line is solved from the desired RT60:
//!
//! ```text
//! g_i = 10^(−3 · len_i / (decay · sr))
//! ```
//!
//! so no long buffers are needed: eight lines of ≈21–65 ms hold the whole
//! tail, whatever `decay` is set to. A stereo pre-delay (one ring per channel)
//! delays the wet signal before it enters the network, and `width` collapses
//! or spreads the L/R wet pair (`width = 0` is fully mono).
//!
//! `mix = 0` stays a sample-exact dry passthrough (the last `mix` multiply
//! carries `wet` into `dry` only when `mix > 0`). All state is pre-allocated at
//! construction; `process` is allocation-, lock- and panic-free.

use std::f32::consts::PI;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::EffectParamBank;
use crate::realtime::effect::descriptor::{
    Curve, EffectDescriptor, ParamFlags, ParamSpec, Rate, StereoClass, Unit,
};
use crate::realtime::processor::Processor;

/// Slot of the stereo pre-delay (ms).
pub const SLOT_PRE_DELAY: u8 = 0;
/// Slot of the RT60 decay time (seconds).
pub const SLOT_DECAY: u8 = 1;
/// Slot of the damping (high-frequency loss in the loop).
pub const SLOT_DAMPING: u8 = 2;
/// Slot of the low-cut corner (Hz).
pub const SLOT_LOW_CUT: u8 = 3;
/// Slot of the stereo width.
pub const SLOT_WIDTH: u8 = 4;
/// Slot of the dry/wet mix.
pub const SLOT_MIX: u8 = 5;

/// Number of FDN delay lines.
const FDN_LINES: usize = 8;

/// Base line lengths in milliseconds, deliberately non-harmonic so the modes
/// do not stack into a metallic ring (the Freeverb weakness this design avoids).
const LINE_MS: [f32; FDN_LINES] = [21.1, 27.7, 33.4, 39.9, 46.3, 52.7, 58.9, 65.1];

/// Maximum pre-delay (spec E3: 0..200 ms).
pub const MAX_PRE_DELAY_MS: f32 = 200.0;

/// Input gain per line before the network.
const INPUT_GAIN: f32 = 0.5;
/// Wet output gain after summing one stereo half of the network.
const OUTPUT_GAIN: f32 = 0.3;
/// Damping knob at 1.0 maps to this much one-pole smoothing (never fully open,
/// so the loop cannot become a pure delayed feedback that rings forever).
const DAMP_MAX: f32 = 0.9;
/// Lowest frequency any filter may be tuned to.
const MIN_HZ: f32 = 20.0;

/// Linear-domain, block-rate parameter helper.
const fn block_spec(
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
        rate: Rate::Block,
        flags: ParamFlags::AUTOMATABLE,
        choices: &[],
    }
}

static REVERB_PARAMS: [ParamSpec; 6] = [
    block_spec("pre_delay", Unit::Ms, 0.0, MAX_PRE_DELAY_MS, 0.0, 20.0).short("pre"),
    ParamSpec::seconds("decay", 0.2, 12.0, 2.0).inline_row(),
    block_spec("damping", Unit::Percent, 0.0, 1.0, 0.5, 20.0)
        .short("damp")
        .inline_row(),
    ParamSpec::freq("low_cut", MIN_HZ, 500.0, MIN_HZ).short("cut"),
    block_spec("width", Unit::Percent, 0.0, 1.0, 1.0, 15.0),
    ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 0.3).inline_row(),
];

/// Static parameter table (spec `.agents/spec/effects.md` E3).
pub static DESCRIPTOR: EffectDescriptor = EffectDescriptor {
    name: "Reverb",
    params: &REVERB_PARAMS,
    stereo: StereoClass::Mono,
};

/// Length of one FDN line in samples (at least one).
fn line_samples(sample_rate: f32, index: usize) -> usize {
    let ms = LINE_MS.get(index).copied().unwrap_or(30.0);
    (ms * 0.001 * sample_rate).round().max(1.0) as usize
}

/// In-place normalized Hadamard butterfly for [`FDN_LINES`] values.
///
/// `H·Hᵀ = I`, so the matrix is energy-preserving: the network stays stable for
/// any feedback gain below 1.
fn hadamard8(values: &mut [f32; FDN_LINES]) {
    let mut step = 1;
    while step < FDN_LINES {
        let mut base = 0;
        while base < FDN_LINES {
            for offset in 0..step {
                let a = values[base + offset];
                let b = values[base + offset + step];
                values[base + offset] = a + b;
                values[base + offset + step] = a - b;
            }
            base += step * 2;
        }
        step *= 2;
    }
    let scale = 1.0 / (FDN_LINES as f32).sqrt();
    for value in values.iter_mut() {
        *value *= scale;
    }
}

/// Per-block filter/feedback settings.
#[derive(Clone, Copy)]
struct Settings {
    /// Damping one-pole weight of the *previous* sample (`0..0.9`).
    damp: f32,
    /// TPT low-pass coefficient of the in-loop high-pass.
    cut_a1: f32,
    /// Per-line feedback gain solved from RT60.
    feedback: [f32; FDN_LINES],
}

/// Stereo FDN-8 reverb (1 in/1 out per channel, zero latency).
pub struct ReverbProcessor {
    sample_rate: f32,
    params: EffectParamBank,
    /// One mono delay line per network node.
    lines: [Box<[f32]>; FDN_LINES],
    /// Ring write pointer per line.
    write: [usize; FDN_LINES],
    /// Damping low-pass state per line.
    damp_state: [f32; FDN_LINES],
    /// Interior low-pass integrator of the in-loop high-pass, per line.
    hp_state: [f32; FDN_LINES],
    /// Stereo pre-delay rings.
    pre_l: Box<[f32]>,
    pre_r: Box<[f32]>,
    pre_capacity: usize,
    pre_write: usize,
}

impl ReverbProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction tolerance;
    /// allocation only happens here).
    pub fn new(sample_rate: f32) -> Self {
        let sr = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        let lines: [Box<[f32]>; FDN_LINES] =
            std::array::from_fn(|index| vec![0.0; line_samples(sr, index)].into_boxed_slice());
        let pre_capacity = (MAX_PRE_DELAY_MS * 0.001 * sr).round() as usize + 1;
        Self {
            sample_rate: sr,
            params: EffectParamBank::new(&DESCRIPTOR, sr),
            lines,
            write: [0; FDN_LINES],
            damp_state: [0.0; FDN_LINES],
            hp_state: [0.0; FDN_LINES],
            pre_l: vec![0.0; pre_capacity].into_boxed_slice(),
            pre_r: vec![0.0; pre_capacity].into_boxed_slice(),
            pre_capacity,
            pre_write: 0,
        }
    }

    /// Current (settled) value of a slot, for tests and read-back.
    pub fn value(&self, slot: u8) -> f32 {
        self.params.value(slot)
    }

    /// Length of FDN line `index` in samples (exposed for tests).
    pub fn line_len(&self, index: usize) -> usize {
        self.lines.get(index).map(|line| line.len()).unwrap_or(0)
    }

    /// Per-block coefficients from the smoothed parameter values.
    fn settings(&mut self, frames: usize) -> Settings {
        let decay = self.params.advance(SLOT_DECAY, frames).max(0.05);
        let damping = self.params.advance(SLOT_DAMPING, frames).clamp(0.0, 1.0);
        let low_cut = self.params.advance(SLOT_LOW_CUT, frames);
        let sample_rate = self.sample_rate;

        let fc = low_cut.clamp(MIN_HZ, sample_rate * 0.45);
        let g = (PI * fc / sample_rate).tan();
        let cut_a1 = g / (1.0 + g);

        let feedback = std::array::from_fn(|index| {
            let len = line_samples(sample_rate, index) as f32;
            // RT60: the loop must lose 60 dB over `decay` seconds.
            10f32.powf(-3.0 * len / (decay * sample_rate))
        });

        Settings {
            damp: damping * DAMP_MAX,
            cut_a1,
            feedback,
        }
    }
}

impl Processor for ReverbProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        let channels = buffer.channels().min(2);
        if frames == 0 || channels == 0 {
            return;
        }

        let settings = self.settings(frames);

        // Block-rate pre-delay (integer samples; block-rate stepping is the
        // documented v1 behaviour, spec E3).
        let pre_delay_ms = self.params.advance(SLOT_PRE_DELAY, frames);
        let pre_delay = if pre_delay_ms.is_finite() {
            (pre_delay_ms * 0.001 * self.sample_rate).round() as usize
        } else {
            0
        }
        .min(self.pre_capacity - 1);

        for index in 0..frames {
            // Per-sample parameters, ticked once per frame so stereo does not
            // double the ramp.
            let mix = self.params.tick(SLOT_MIX);
            let width = self.params.tick(SLOT_WIDTH);

            let dry_l = buffer
                .channel(0)
                .and_then(|channel| channel.get(index).copied())
                .unwrap_or(0.0);
            let dry_r = if channels > 1 {
                buffer
                    .channel(1)
                    .and_then(|channel| channel.get(index).copied())
                    .unwrap_or(dry_l)
            } else {
                dry_l
            };

            // Stereo pre-delay: write, then read `pre_delay` samples back.
            if let Some(slot) = self.pre_l.get_mut(self.pre_write) {
                *slot = dry_l;
            }
            if let Some(slot) = self.pre_r.get_mut(self.pre_write) {
                *slot = dry_r;
            }
            let read_pos = (self.pre_write + self.pre_capacity - pre_delay) % self.pre_capacity;
            let in_l = self.pre_l.get(read_pos).copied().unwrap_or(0.0) * INPUT_GAIN;
            let in_r = self.pre_r.get(read_pos).copied().unwrap_or(0.0) * INPUT_GAIN;
            self.pre_write = (self.pre_write + 1) % self.pre_capacity;

            // Read every line through its damping low-pass and low cut.
            let mut nodes = [0.0f32; FDN_LINES];
            let mut read_positions = [0usize; FDN_LINES];
            for (line_index, node) in nodes.iter_mut().enumerate() {
                let write = self.write.get(line_index).copied().unwrap_or(0);
                read_positions[line_index] = write;
                let raw = self
                    .lines
                    .get(line_index)
                    .and_then(|line| line.get(write).copied())
                    .unwrap_or(0.0);

                let damped = self.damp_state.get(line_index).copied().unwrap_or(0.0)
                    + (raw - self.damp_state.get(line_index).copied().unwrap_or(0.0))
                        * (1.0 - settings.damp);
                if let Some(state) = self.damp_state.get_mut(line_index) {
                    *state = damped;
                }

                let integrator = self.hp_state.get(line_index).copied().unwrap_or(0.0);
                let v = (damped - integrator) * settings.cut_a1;
                let low = v + integrator;
                if let Some(state) = self.hp_state.get_mut(line_index) {
                    *state = low + v;
                }
                *node = damped - low;

                if let Some(len) = self.lines.get(line_index).map(|line| line.len()) {
                    if len > 0 {
                        if let Some(cursor) = self.write.get_mut(line_index) {
                            *cursor = (write + 1) % len;
                        }
                    }
                }
            }

            hadamard8(&mut nodes);

            // Write the input plus the feedback mix back into each line.
            for (line_index, node) in nodes.iter().enumerate() {
                let input = if line_index < FDN_LINES / 2 {
                    in_l
                } else {
                    in_r
                };
                let feedback = settings.feedback.get(line_index).copied().unwrap_or(0.0);
                let value = input + feedback * *node;
                if let Some(position) = read_positions.get(line_index).copied() {
                    if let Some(line) = self.lines.get_mut(line_index) {
                        if let Some(slot) = line.get_mut(position) {
                            *slot = value;
                        }
                    }
                }
            }

            let wet_half = FDN_LINES / 2;
            let wet_l = nodes[..wet_half].iter().sum::<f32>() * OUTPUT_GAIN;
            let wet_r = nodes[wet_half..].iter().sum::<f32>() * OUTPUT_GAIN;

            // Width: collapse toward mono at 0, full spread at 1.
            let mid = 0.5 * (wet_l + wet_r);
            let side = 0.5 * (wet_l - wet_r);
            let wet_l = mid + width * side;
            let wet_r = mid - width * side;

            let out_l = dry_l + (wet_l - dry_l) * mix;
            let out_r = dry_r + (wet_r - dry_r) * mix;

            if channels > 1 {
                if let Some(channel) = buffer.channel_mut(0) {
                    if let Some(sample) = channel.get_mut(index) {
                        *sample = out_l;
                    }
                }
                if let Some(channel) = buffer.channel_mut(1) {
                    if let Some(sample) = channel.get_mut(index) {
                        *sample = out_r;
                    }
                }
            } else if let Some(channel) = buffer.channel_mut(0) {
                if let Some(sample) = channel.get_mut(index) {
                    *sample = 0.5 * (out_l + out_r);
                }
            }
        }

        self.flush_denormals();
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

impl ReverbProcessor {
    /// Flushes denormal residue in every filter state (keeps denormals out of
    /// the feedback loop when the tail has decayed to near silence).
    fn flush_denormals(&mut self) {
        const EPS: f32 = 1e-18;
        for state in self.damp_state.iter_mut().chain(self.hp_state.iter_mut()) {
            if state.abs() < EPS {
                *state = 0.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::{
        DESCRIPTOR, FDN_LINES, ReverbProcessor, SLOT_DAMPING, SLOT_DECAY, SLOT_LOW_CUT, SLOT_MIX,
        SLOT_PRE_DELAY, SLOT_WIDTH, hadamard8,
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

    /// Renders a mono signal through the reverb in fixed blocks.
    fn run_mono(reverb: &mut ReverbProcessor, signal: &[f32]) -> Vec<f32> {
        let mut buffer = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(BLOCK) {
            let frames = chunk.len();
            buffer.set_len(frames);
            if let Some(channel) = buffer.channel_mut(0) {
                channel[..frames].copy_from_slice(chunk);
            }
            reverb.process(&ctx(), &mut buffer);
            if let Some(channel) = buffer.channel(0) {
                out.extend_from_slice(&channel[..frames]);
            }
        }
        out
    }

    /// Renders a stereo signal through the reverb in fixed blocks.
    fn run_stereo(
        reverb: &mut ReverbProcessor,
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
            reverb.process(&ctx(), &mut buffer);
            if let Some(channel) = buffer.channel(0) {
                out_l.extend_from_slice(&channel[..frames]);
            }
            if let Some(channel) = buffer.channel(1) {
                out_r.extend_from_slice(&channel[..frames]);
            }
        }
        (out_l, out_r)
    }

    fn impulse(samples: usize) -> Vec<f32> {
        let mut signal = vec![0.0f32; samples];
        if let Some(first) = signal.first_mut() {
            *first = 1.0;
        }
        signal
    }

    fn sine(freq: f32, seconds: f32) -> Vec<f32> {
        let n = (SR * seconds) as usize;
        (0..n).map(|i| (TAU * freq * i as f32 / SR).sin()).collect()
    }

    /// Deterministic pseudo-noise (xorshift) so tests are reproducible.
    fn noise(samples: usize) -> Vec<f32> {
        let mut state = 0x1234_5678u32;
        (0..samples)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state as f32 / u32::MAX as f32) * 2.0 - 1.0
            })
            .collect()
    }

    fn set(reverb: &mut ReverbProcessor, slot: u8, value: f32) {
        assert!(
            reverb.set_param(ParamId::BuiltinParam(slot), value),
            "slot {slot} must be accepted"
        );
    }

    #[test]
    fn descriptor_is_well_formed() {
        assert_eq!(validate_descriptor(&DESCRIPTOR), Ok(()));
        assert_eq!(DESCRIPTOR.inline_slots().count(), 3);
        assert_eq!(DESCRIPTOR.params.len(), 6);
    }

    #[test]
    fn hadamard_is_orthonormal() {
        // H·Hᵀ = I: applying the transform twice returns the input scaled by
        // the matrix twice; instead just check that the norm is preserved.
        let mut values = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let before: f32 = values.iter().map(|v| v * v).sum();
        hadamard8(&mut values);
        let after: f32 = values.iter().map(|v| v * v).sum();
        assert!(
            (before - after).abs() / before < 1e-6,
            "{before} vs {after}"
        );
    }

    #[test]
    fn feed_lines_are_mutually_non_harmonic_and_allocated() {
        let reverb = ReverbProcessor::new(SR);
        let mut lengths: Vec<usize> = (0..FDN_LINES).map(|i| reverb.line_len(i)).collect();
        lengths.sort_unstable();
        lengths.dedup();
        assert_eq!(lengths.len(), FDN_LINES, "line lengths must all differ");
    }

    #[test]
    fn silence_in_silence_out() {
        let mut reverb = ReverbProcessor::new(SR);
        set(&mut reverb, SLOT_MIX, 1.0);
        set(&mut reverb, SLOT_DECAY, 12.0);
        set(&mut reverb, SLOT_DAMPING, 0.0);
        let out = run_mono(&mut reverb, &vec![0.0; BLOCK * 64]);
        assert!(out.iter().all(|s| s.abs() < 1e-9));
    }

    #[test]
    fn full_scale_stays_finite_with_extreme_settings() {
        let mut reverb = ReverbProcessor::new(SR);
        set(&mut reverb, SLOT_MIX, 1.0);
        set(&mut reverb, SLOT_DECAY, 12.0);
        set(&mut reverb, SLOT_DAMPING, 0.0);
        set(&mut reverb, SLOT_LOW_CUT, 20.0);
        let out = run_mono(&mut reverb, &sine(220.0, 2.0));
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(out.iter().fold(0.0f32, |a, s| a.max(s.abs())) < 8.0);
    }

    #[test]
    fn mix_zero_is_a_sample_exact_passthrough() {
        let mut reverb = ReverbProcessor::new(SR);
        set(&mut reverb, SLOT_MIX, 0.0);
        set(&mut reverb, SLOT_DECAY, 4.0);
        let input = sine(330.0, 0.5);
        let out = run_mono(&mut reverb, &input);
        assert_eq!(out, input, "mix = 0 must return the dry signal bit for bit");
    }

    #[test]
    fn tail_decays_and_longer_decay_rings_longer() {
        let samples = SR as usize * 3;

        let mut short = ReverbProcessor::new(SR);
        set(&mut short, SLOT_MIX, 1.0);
        set(&mut short, SLOT_DECAY, 0.5);
        set(&mut short, SLOT_DAMPING, 0.0);
        let short_tail = run_mono(&mut short, &impulse(samples));

        let mut long = ReverbProcessor::new(SR);
        set(&mut long, SLOT_MIX, 1.0);
        set(&mut long, SLOT_DECAY, 8.0);
        set(&mut long, SLOT_DAMPING, 0.0);
        let long_tail = run_mono(&mut long, &impulse(samples));

        let window = BLOCK * 32;
        let energy =
            |signal: &[f32]| -> f32 { signal[signal.len() - window..].iter().map(|s| s * s).sum() };
        let short_energy = energy(&short_tail);
        let long_energy = energy(&long_tail);

        assert!(
            long_energy > short_energy * 4.0,
            "8 s decay must ring far longer than 0.5 s ({long_energy} vs {short_energy})"
        );
        assert!(
            long_energy < 1.0,
            "the tail must be bounded, got {long_energy}"
        );
    }

    #[test]
    fn damping_removes_high_frequency_tail_energy() {
        let samples = SR as usize * 2;
        let excitations = noise(samples);

        let high_freq_energy = |signal: &[f32]| -> f32 {
            signal
                .windows(2)
                .map(|pair| (pair[1] - pair[0]) * (pair[1] - pair[0]))
                .sum()
        };

        let mut bright = ReverbProcessor::new(SR);
        set(&mut bright, SLOT_MIX, 1.0);
        set(&mut bright, SLOT_DAMPING, 0.0);
        let bright_out = run_mono(&mut bright, &excitations);

        let mut dark = ReverbProcessor::new(SR);
        set(&mut dark, SLOT_MIX, 1.0);
        set(&mut dark, SLOT_DAMPING, 0.95);
        let dark_out = run_mono(&mut dark, &excitations);

        assert!(
            high_freq_energy(&dark_out) < high_freq_energy(&bright_out) * 0.5,
            "damping must absorb highs: dark {} bright {}",
            high_freq_energy(&dark_out),
            high_freq_energy(&bright_out)
        );
    }

    #[test]
    fn pre_delay_pushes_the_wet_onset_back() {
        let pre_samples = (SR * 0.1) as usize;
        let samples = SR as usize;

        let mut reverb = ReverbProcessor::new(SR);
        set(&mut reverb, SLOT_MIX, 1.0);
        set(&mut reverb, SLOT_PRE_DELAY, 100.0);
        let out = run_mono(&mut reverb, &impulse(samples));

        let early = out[..pre_samples - BLOCK]
            .iter()
            .fold(0.0f32, |a, s| a.max(s.abs()));
        let late = out[pre_samples..]
            .iter()
            .fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(
            early < 1e-6,
            "no wet energy before the pre-delay, got {early}"
        );
        assert!(late > 1e-4, "the tail must arrive after the pre-delay");
    }

    #[test]
    fn width_zero_collapses_to_mono() {
        let left = sine(440.0, 1.0);
        let right = sine(660.0, 1.0);

        let mut mono = ReverbProcessor::new(SR);
        set(&mut mono, SLOT_MIX, 1.0);
        set(&mut mono, SLOT_WIDTH, 0.0);
        let (mono_l, mono_r) = run_stereo(&mut mono, &left, &right);
        for (l, r) in mono_l.iter().zip(mono_r.iter()) {
            assert!(
                (l - r).abs() < 1e-6,
                "width = 0 must give identical channels"
            );
        }

        let mut wide = ReverbProcessor::new(SR);
        set(&mut wide, SLOT_MIX, 1.0);
        set(&mut wide, SLOT_WIDTH, 1.0);
        let (wide_l, wide_r) = run_stereo(&mut wide, &left, &right);
        let spread: f32 = wide_l
            .iter()
            .zip(wide_r.iter())
            .map(|(l, r)| (l - r).abs())
            .sum();
        assert!(spread > 1.0, "width = 1 must keep the L/R wet pair apart");
    }

    #[test]
    fn params_are_clamped_and_unknown_ids_rejected() {
        let mut reverb = ReverbProcessor::new(SR);
        set(&mut reverb, SLOT_DECAY, 999.0);
        assert!((reverb.value(SLOT_DECAY) - 12.0).abs() < 1e-6);
        set(&mut reverb, SLOT_PRE_DELAY, -5.0);
        assert!(reverb.value(SLOT_PRE_DELAY).abs() < 1e-6);
        set(&mut reverb, SLOT_WIDTH, 5.0);
        assert!((reverb.value(SLOT_WIDTH) - 1.0).abs() < 1e-6);
        set(&mut reverb, SLOT_LOW_CUT, f32::NAN);
        assert!((reverb.value(SLOT_LOW_CUT) - 20.0).abs() < 1e-6);
        assert!(!reverb.set_param(ParamId::BuiltinParam(42), 1.0));
        assert!(!reverb.set_param(ParamId::MasterGain, 1.0));
    }

    #[test]
    fn runtime_change_ramps_instead_of_jumping() {
        let mut reverb = ReverbProcessor::new(SR);
        run_mono(&mut reverb, &vec![0.0; BLOCK]);
        set(&mut reverb, SLOT_MIX, 1.0);
        assert!(
            (reverb.value(SLOT_MIX) - 0.3).abs() < 1e-6,
            "the smoother must not jump on a runtime change"
        );
        for _ in 0..200 {
            run_mono(&mut reverb, &vec![0.0; BLOCK]);
        }
        assert!((reverb.value(SLOT_MIX) - 1.0).abs() < 0.05);
    }
}
