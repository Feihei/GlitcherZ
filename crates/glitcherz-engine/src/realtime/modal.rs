//! Modal strike instrument (spec `.agents/spec/modal-instrument.md`).
//!
//! A bank of quadrature resonators driven by a single filtered-noise strike.
//! One engine covers the four presets (mallet / vibraphone / bell / singing
//! bowl): they differ only in their mode tables, which is exactly how STK
//! ModalBar and LMMS Mallets separate their kits.
//!
//! Two properties drive the DSP choices:
//!
//! - **Struck, not bowed** (user decision Q2): `Off { note }` is ignored, the
//!   tail rings to its own decay. Only the blanket sentinel `Off { 0 }`
//!   (Stop / Panic) fades the pool out.
//! - **Long tails stay stable in f32**: each mode is a rotator
//!   (`re' = c·re − s·im`, `im' = s·re + c·im`) whose decay is applied
//!   **once per block** (`exp(−frames / τ)`). The textbook two-pole form
//!   needs `r = exp(−1/τ)` per sample, which for a 26 s T60 sits at
//!   `0.9999979` — right at f32's resolution near 1.0.
//!
//! Real-time contract: the voice pool, the resonator table and the mono
//! scratch are allocated in [`ModalProcessor::new`] on the control thread.
//! `process` / `note_event` / `set_param` only do plain arithmetic over
//! pre-allocated memory (no allocation, no locks, no panic).

use std::f32::consts::TAU;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;
use crate::realtime::synth::note_to_frequency;

/// Document capacity of one preset's partial table (B1-D3).
pub const MAX_PARTIALS: usize = 8;

/// Resonator slots per voice. A partial with a beating pair consumes two
/// (B1-D3); the largest preset (Bell) uses 12.
pub const MAX_RESONATORS: usize = 16;

/// Voice budget, matching the built-in synthesizer's pool so the note
/// semantics stay identical.
pub const MAX_VOICES: usize = 16;

/// Upper bound of the strike burst. `hardness = 0` asks for 8.5 ms; the
/// ceiling only protects a huge host block from being truncated mid-burst.
const MAX_EXCITATION_FRAMES: usize = 512;

/// Mono scratch ceiling, same role as the synthesizer's.
const SCRATCH_FRAMES: usize = 1024;

/// A voice is released once every mode is quieter than this (≈ −60 dB), which
/// is also the T60 definition used by the preset tables (B1-D8).
const IDLE_EPS: f32 = 1.0e-3;

/// Blanket-stop fade (Stop / Panic), identical to the drum kit's K6.
const RELEASE_SECONDS: f32 = 0.01;

/// Pitch-offset bound in semitones, same as the other built-in sources.
const PITCH_OFFSET_LIMIT: f32 = 24.0;

/// `ln(1000)` — T60 → time constant conversion (B1-D8).
const LN_1000: f32 = 6.907_755_3;

/// One row of a preset table.
#[derive(Clone, Copy)]
struct PartialSpec {
    /// Frequency relative to the note's fundamental.
    ratio: f32,
    /// Relative peak amplitude.
    gain: f32,
    /// −60 dB decay time (seconds).
    decay_s: f32,
    /// Total beating offset (Hz) between the pair; `0` = one resonator.
    beat_hz: f32,
}

/// A named mode table.
struct Preset {
    /// Display name (diagnostics / tests).
    #[allow(dead_code)]
    name: &'static str,
    partials: &'static [PartialSpec],
}

const MALLET: [PartialSpec; 4] = [
    PartialSpec {
        ratio: 1.00,
        gain: 1.00,
        decay_s: 1.2,
        beat_hz: 0.0,
    },
    PartialSpec {
        ratio: 3.93,
        gain: 0.45,
        decay_s: 0.7,
        beat_hz: 0.0,
    },
    PartialSpec {
        ratio: 9.24,
        gain: 0.18,
        decay_s: 0.35,
        beat_hz: 0.0,
    },
    PartialSpec {
        ratio: 16.00,
        gain: 0.07,
        decay_s: 0.20,
        beat_hz: 0.0,
    },
];

const VIBRAPHONE: [PartialSpec; 4] = [
    PartialSpec {
        ratio: 1.00,
        gain: 1.00,
        decay_s: 6.0,
        beat_hz: 1.6,
    },
    PartialSpec {
        ratio: 4.00,
        gain: 0.35,
        decay_s: 3.0,
        beat_hz: 1.6,
    },
    PartialSpec {
        ratio: 10.00,
        gain: 0.15,
        decay_s: 1.5,
        beat_hz: 0.0,
    },
    PartialSpec {
        ratio: 15.90,
        gain: 0.06,
        decay_s: 0.8,
        beat_hz: 0.0,
    },
];

const BELL: [PartialSpec; MAX_PARTIALS] = [
    PartialSpec {
        ratio: 0.56,
        gain: 0.70,
        decay_s: 14.0,
        beat_hz: 0.6,
    },
    PartialSpec {
        ratio: 0.92,
        gain: 0.90,
        decay_s: 11.0,
        beat_hz: 0.5,
    },
    PartialSpec {
        ratio: 1.19,
        gain: 1.00,
        decay_s: 9.0,
        beat_hz: 0.4,
    },
    PartialSpec {
        ratio: 1.71,
        gain: 0.55,
        decay_s: 7.0,
        beat_hz: 0.3,
    },
    PartialSpec {
        ratio: 2.00,
        gain: 0.50,
        decay_s: 5.0,
        beat_hz: 0.0,
    },
    PartialSpec {
        ratio: 2.74,
        gain: 0.35,
        decay_s: 4.0,
        beat_hz: 0.0,
    },
    PartialSpec {
        ratio: 3.00,
        gain: 0.30,
        decay_s: 3.5,
        beat_hz: 0.0,
    },
    PartialSpec {
        ratio: 4.07,
        gain: 0.20,
        decay_s: 2.5,
        beat_hz: 0.0,
    },
];

const SINGING_BOWL: [PartialSpec; 5] = [
    PartialSpec {
        ratio: 1.00,
        gain: 1.00,
        decay_s: 26.0,
        beat_hz: 1.2,
    },
    PartialSpec {
        ratio: 2.71,
        gain: 0.45,
        decay_s: 18.0,
        beat_hz: 0.9,
    },
    PartialSpec {
        ratio: 5.15,
        gain: 0.22,
        decay_s: 12.0,
        beat_hz: 0.7,
    },
    PartialSpec {
        ratio: 7.90,
        gain: 0.10,
        decay_s: 8.0,
        beat_hz: 0.5,
    },
    PartialSpec {
        ratio: 11.60,
        gain: 0.05,
        decay_s: 5.0,
        beat_hz: 0.0,
    },
];

/// Preset tables in menu order. The model's `ModalPreset` enum indexes this
/// array, so the two must stay in the same order (`modal-instrument.md` B1-D7).
static PRESETS: [Preset; 4] = [
    Preset {
        name: "Mallet",
        partials: &MALLET,
    },
    Preset {
        name: "Vibraphone",
        partials: &VIBRAPHONE,
    },
    Preset {
        name: "Bell",
        partials: &BELL,
    },
    Preset {
        name: "Singing Bowl",
        partials: &SINGING_BOWL,
    },
];

/// Number of presets (the model enum mirrors this).
pub fn preset_count() -> usize {
    PRESETS.len()
}

/// One quadrature resonator. `re` is the audible component; `im` is the
/// quadrature state that keeps the rotation lossless.
#[derive(Clone, Copy)]
struct Resonator {
    re: f32,
    im: f32,
    /// cos θ for the current mode frequency.
    c: f32,
    /// sin θ for the current mode frequency.
    s: f32,
    /// Peak amplitude of this mode.
    gain: f32,
    /// Time constant in **samples** (`T60 / ln 1000 · fs`).
    tau: f32,
    /// `false` when the mode is above Nyquist or disabled (B1-D9).
    on: bool,
}

impl Resonator {
    const IDLE: Self = Self {
        re: 0.0,
        im: 0.0,
        c: 1.0,
        s: 0.0,
        gain: 0.0,
        tau: 1.0,
        on: false,
    };
}

#[derive(Clone, Copy)]
struct Voice {
    /// MIDI note currently assigned; kept for same-note retriggers.
    note: Option<u8>,
    /// Monotonic allocation stamp (steal = smallest).
    age: u64,
    active: bool,
    releasing: bool,
    release_pos: f32,
    /// Strike burst cursor / length in samples.
    exc_pos: usize,
    exc_len: usize,
    /// Excitation amplitude and one-pole state.
    exc_amp: f32,
    exc_lp: f32,
    exc_lp_a: f32,
    /// Strike phase (cos / sin): the burst enters the rotating frame at a
    /// random angle, so a second hit adds energy instead of coherently
    /// cancelling the ringing state.
    exc_cos: f32,
    exc_sin: f32,
    res: [Resonator; MAX_RESONATORS],
    res_count: usize,
}

impl Voice {
    const IDLE: Self = Self {
        note: None,
        age: 0,
        active: false,
        releasing: false,
        release_pos: 0.0,
        exc_pos: 0,
        exc_len: 0,
        exc_amp: 0.0,
        exc_lp: 0.0,
        exc_lp_a: 0.0,
        exc_cos: 1.0,
        exc_sin: 0.0,
        res: [Resonator::IDLE; MAX_RESONATORS],
        res_count: 0,
    };

    fn is_silent(&self) -> bool {
        self.exc_pos >= self.exc_len
            && self.res[..self.res_count]
                .iter()
                .all(|r| !r.on || (r.re.abs() < IDLE_EPS && r.im.abs() < IDLE_EPS))
    }
}

/// Modal strike instrument (generator semantics: the block is overwritten).
pub struct ModalProcessor {
    sample_rate: f32,
    voices: [Voice; MAX_VOICES],
    /// Mono scratch; one voice renders here per block, then sums into the
    /// output.
    scratch: AudioBuffer,
    stamps: u64,
    preset: usize,
    decay_scale: f32,
    beating: f32,
    hardness: f32,
    brightness: f32,
    gain: f32,
    pitch_ratio: f32,
    /// Sum of the active preset's partial gains, used to normalise the strike
    /// so the four presets sit at a similar level.
    sum_gain: f32,
    /// White-noise state (xorshift32); deterministic for a given event order.
    rng: u32,
    /// Set whenever preset / decay / beating / pitch changes; the pool is
    /// retuned at the next block boundary.
    tables_dirty: bool,
}

impl ModalProcessor {
    /// Control-thread construction. A non-positive sample rate falls back to
    /// 48 kHz (construction tolerance, not a real-time path).
    pub fn new(sample_rate: f32) -> Self {
        let sr = if sample_rate.is_finite() && sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            sample_rate: sr,
            voices: [Voice::IDLE; MAX_VOICES],
            scratch: AudioBuffer::new(1, SCRATCH_FRAMES),
            stamps: 0,
            preset: 0,
            decay_scale: 1.0,
            beating: 1.0,
            hardness: 0.7,
            brightness: 0.7,
            gain: 1.0,
            pitch_ratio: 1.0,
            sum_gain: partial_gain_sum(&PRESETS[0]),
            rng: 0x1234_5678,
            tables_dirty: false,
        }
    }

    /// Voices holding energy (sounding or fading). RT-safe.
    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|voice| voice.active).count()
    }

    /// Active preset index (tests / diagnostics). RT-safe.
    pub fn preset_index(&self) -> usize {
        self.preset
    }

    /// Number of live resonators of the most recently triggered voice.
    pub fn resonator_count(&self) -> usize {
        self.voices
            .iter()
            .filter(|voice| voice.active)
            .map(|voice| voice.res_count)
            .max()
            .unwrap_or(0)
    }

    fn next_stamp(&mut self) -> u64 {
        self.stamps = self.stamps.wrapping_add(1);
        self.stamps
    }

    /// First silent slot, else the oldest voice (B1-D5).
    fn allocate(&self) -> usize {
        if let Some(index) = self.voices.iter().position(|voice| !voice.active) {
            return index;
        }
        let mut oldest = 0;
        for (i, voice) in self.voices.iter().enumerate() {
            if voice.age < self.voices[oldest].age {
                oldest = i;
            }
        }
        oldest
    }

    /// (burst length, low-pass coefficient, excitation amplitude).
    fn excitation_settings(&self, velocity: f32) -> (usize, f32, f32) {
        let soft = 1.0 - self.hardness;
        let len_s = 0.0005 + soft * soft * 0.008;
        let len = ((len_s * self.sample_rate).round() as usize).clamp(1, MAX_EXCITATION_FRAMES);
        let cutoff = (300.0 * (self.brightness * 6.0).exp2()).min(0.45 * self.sample_rate);
        let coeff = 1.0 - (-TAU * cutoff / self.sample_rate).exp();
        let amp = 0.5 * velocity.powf(1.5) / self.sum_gain.max(0.001);
        (len, coeff.clamp(0.0, 1.0), amp)
    }

    /// Retunes every voice after a table-level parameter change.
    fn retune_all(&mut self) {
        let preset = &PRESETS[self.preset];
        for voice in &mut self.voices {
            retune_voice(
                voice,
                preset,
                self.beating,
                self.decay_scale,
                self.pitch_ratio,
                self.sample_rate,
            );
        }
    }

    fn trigger(&mut self, note: u8, velocity: f32) {
        let velocity = if velocity.is_finite() {
            velocity.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if velocity <= 0.0 {
            return;
        }
        // Same note already ringing: strike it again (B1-D5). Otherwise take
        // a free slot or steal the oldest one.
        let index = self
            .voices
            .iter()
            .position(|voice| voice.active && voice.note == Some(note))
            .unwrap_or_else(|| self.allocate());
        let age = self.next_stamp();
        let (exc_len, exc_lp_a, exc_amp) = self.excitation_settings(velocity);
        // Random strike phase: two hits at different times land at different
        // points of the cycle, so retriggers add energy the way a real strike
        // does (see `exc_cos`).
        let phase = next_noise(&mut self.rng) * std::f32::consts::PI;
        let (exc_cos, exc_sin) = (phase.cos(), phase.sin());
        let preset = &PRESETS[self.preset];
        let voice = &mut self.voices[index];
        voice.note = Some(note);
        voice.age = age;
        voice.active = true;
        voice.releasing = false;
        voice.release_pos = 0.0;
        voice.exc_pos = 0;
        voice.exc_len = exc_len;
        voice.exc_lp = 0.0;
        voice.exc_lp_a = exc_lp_a;
        voice.exc_amp = exc_amp;
        voice.exc_cos = exc_cos;
        voice.exc_sin = exc_sin;
        // States are deliberately kept: a retrigger is additive and a stolen
        // tail glides to the new pitch instead of clicking (B1-D5).
        retune_voice(
            voice,
            preset,
            self.beating,
            self.decay_scale,
            self.pitch_ratio,
            self.sample_rate,
        );
    }

    fn release_all(&mut self) {
        for voice in &mut self.voices {
            if voice.active && !voice.releasing {
                voice.releasing = true;
                voice.release_pos = 0.0;
            }
        }
    }
}

impl Processor for ModalProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        if frames == 0 || buffer.channels() == 0 {
            return;
        }
        for ch in 0..buffer.channels() {
            if let Some(dst) = buffer.channel_mut(ch) {
                dst.fill(0.0);
            }
        }
        if self.tables_dirty {
            self.tables_dirty = false;
            self.retune_all();
        }

        let release_frames = (RELEASE_SECONDS * self.sample_rate).max(1.0);
        let gain = self.gain;
        let Self {
            voices,
            scratch,
            rng,
            ..
        } = self;
        scratch.set_len(frames.min(SCRATCH_FRAMES));
        let rendered = scratch.len();
        for voice in voices.iter_mut() {
            // Silent voices cost nothing.
            if !voice.active {
                continue;
            }
            scratch.clear();
            if let Some(mono) = scratch.channel_mut(0) {
                render_voice(voice, release_frames, gain, &mut *rng, mono);
            }
            let Some(mono) = scratch.channel(0) else {
                continue;
            };
            for ch in 0..buffer.channels() {
                if let Some(dst) = buffer.channel_mut(ch) {
                    for (out, sample) in dst.iter_mut().zip(mono.iter()) {
                        *out += *sample;
                    }
                }
            }
        }
        // Defensive: a host block larger than the scratch stays silent rather
        // than keeping stale samples.
        if rendered < frames {
            for ch in 0..buffer.channels() {
                if let Some(dst) = buffer.channel_mut(ch) {
                    for sample in &mut dst[rendered..] {
                        *sample = 0.0;
                    }
                }
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        if !value.is_finite() {
            return false;
        }
        match id {
            ParamId::ModalPreset => {
                let index = value.round().clamp(0.0, (PRESETS.len() - 1) as f32) as usize;
                if index != self.preset {
                    self.preset = index;
                    self.sum_gain = partial_gain_sum(&PRESETS[index]);
                    self.tables_dirty = true;
                }
            }
            ParamId::ModalDecay => {
                let decay = value.clamp(0.25, 4.0);
                if decay != self.decay_scale {
                    self.decay_scale = decay;
                    self.tables_dirty = true;
                }
            }
            ParamId::ModalBeating => {
                let beating = value.clamp(0.0, 2.0);
                if beating != self.beating {
                    self.beating = beating;
                    self.tables_dirty = true;
                }
            }
            ParamId::ModalHardness => self.hardness = value.clamp(0.0, 1.0),
            ParamId::ModalBrightness => self.brightness = value.clamp(0.0, 1.0),
            ParamId::ModalGain => self.gain = value.clamp(0.0, 2.0),
            ParamId::PitchOffset => {
                let semitones = value.clamp(-PITCH_OFFSET_LIMIT, PITCH_OFFSET_LIMIT);
                self.pitch_ratio = (semitones / 12.0).exp2();
                self.tables_dirty = true;
            }
            _ => return false,
        }
        true
    }

    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { note, velocity } => self.trigger(note, velocity),
            // Struck semantics (B1-D4): a pitch-matched note-off must not cut
            // the tail; `Off { 0 }` is the blanket Stop / Panic sentinel.
            NoteEvent::Off { note: 0 } => self.release_all(),
            NoteEvent::Off { .. } => {}
        }
        true
    }

    /// The modal engine has no CC surface; consuming the event keeps the
    /// graph quiet (same rationale as the drum kit).
    fn midi_cc(&mut self, _channel: u8, _controller: u8, _value: u8) -> bool {
        true
    }
}

/// Sum of a preset's partial gains (strike normalisation).
fn partial_gain_sum(preset: &Preset) -> f32 {
    preset.partials.iter().map(|partial| partial.gain).sum()
}

/// Rebuilds one voice's resonator coefficients from the current table.
///
/// States (`re` / `im`) are kept on purpose: a pitch or preset change glides
/// the ringing tail instead of restarting it (B1-D5).
fn retune_voice(
    voice: &mut Voice,
    preset: &Preset,
    beating: f32,
    decay_scale: f32,
    pitch_ratio: f32,
    sample_rate: f32,
) {
    let Some(note) = voice.note else {
        voice.res_count = 0;
        return;
    };
    let fundamental = note_to_frequency(note) * pitch_ratio;
    let nyquist = 0.45 * sample_rate;
    let mut count = 0;
    for partial in preset.partials {
        if count >= MAX_RESONATORS {
            break;
        }
        let offset = (partial.beat_hz * beating).max(0.0);
        let paired = offset > 1.0e-3;
        let steps = if paired { 2 } else { 1 };
        let t60 = (partial.decay_s * decay_scale).clamp(0.05, 120.0);
        let tau = (t60 / LN_1000 * sample_rate).max(1.0);
        for step in 0..steps {
            if count >= MAX_RESONATORS {
                break;
            }
            let frequency = if paired {
                fundamental * partial.ratio + (step as f32 - 0.5) * offset
            } else {
                fundamental * partial.ratio
            };
            let resonator = &mut voice.res[count];
            if frequency > 0.0 && frequency < nyquist {
                let theta = TAU * frequency / sample_rate;
                resonator.c = theta.cos();
                resonator.s = theta.sin();
                resonator.gain = partial.gain;
                resonator.tau = tau;
                resonator.on = true;
            } else {
                // Above Nyquist: drop the mode rather than alias (B1-D9).
                resonator.gain = 0.0;
                resonator.on = false;
            }
            count += 1;
        }
    }
    voice.res_count = count;
}

/// Renders one voice into `out` (additive).
fn render_voice(voice: &mut Voice, release_frames: f32, gain: f32, rng: &mut u32, out: &mut [f32]) {
    let count = voice.res_count.min(MAX_RESONATORS);
    if count == 0 {
        voice.active = false;
        return;
    }
    let frames = out.len();
    for sample in out.iter_mut().take(frames) {
        if !voice.active {
            break;
        }
        let excitation = if voice.exc_pos < voice.exc_len {
            let noise = next_noise(rng);
            voice.exc_lp += voice.exc_lp_a * (noise - voice.exc_lp);
            let t = voice.exc_pos as f32 / voice.exc_len as f32;
            let window = (1.0 - t) * (1.0 - t);
            voice.exc_pos += 1;
            voice.exc_lp * window * voice.exc_amp
        } else {
            0.0
        };
        let mut acc = 0.0;
        let exc_re = excitation * voice.exc_cos;
        let exc_im = excitation * voice.exc_sin;
        for resonator in &mut voice.res[..count] {
            if !resonator.on {
                continue;
            }
            let re = resonator.re + exc_re;
            let im = resonator.im + exc_im;
            resonator.re = re * resonator.c - im * resonator.s;
            resonator.im = re * resonator.s + im * resonator.c;
            acc += resonator.re * resonator.gain;
        }
        let fade = if voice.releasing {
            let fade = (1.0 - voice.release_pos / release_frames).max(0.0);
            voice.release_pos += 1.0;
            if voice.release_pos >= release_frames {
                voice.active = false;
            }
            fade
        } else {
            1.0
        };
        *sample += acc * gain * fade;
    }

    // Block-rate decay (B1-D2): one exp per mode per block, outside the
    // sample loop.
    let block = frames as f32;
    for resonator in &mut voice.res[..count] {
        if resonator.on {
            let decay = (-block / resonator.tau).exp();
            resonator.re *= decay;
            resonator.im *= decay;
        }
    }
    if voice.active && voice.is_silent() {
        voice.active = false;
    }
}

/// xorshift32 → −1..1. Deterministic and allocation-free.
fn next_noise(state: &mut u32) -> f32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    (x as f32 / u32::MAX as f32) * 2.0 - 1.0
}

#[cfg(test)]
mod tests {
    use super::{MAX_VOICES, ModalProcessor, PRESETS, SINGING_BOWL};
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::{NoteEvent, ParamId};
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;
    use std::f32::consts::TAU;

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

    fn blocks_for(seconds: f32) -> usize {
        (seconds * SR / BLOCK as f32).round() as usize
    }

    fn render(processor: &mut ModalProcessor, blocks: usize) -> Vec<f32> {
        let mut buffer = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(blocks * BLOCK);
        for _ in 0..blocks {
            buffer.set_len(BLOCK);
            processor.process(&ctx(), &mut buffer);
            out.extend_from_slice(buffer.channel(0).expect("mono"));
        }
        out
    }

    fn note_on(note: u8) -> NoteEvent {
        NoteEvent::On {
            note,
            velocity: 1.0,
        }
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// Goertzel magnitude at `frequency` (comparisons only, not calibrated).
    fn goertzel(samples: &[f32], frequency: f32) -> f32 {
        let omega = TAU * frequency / SR;
        let coeff = 2.0 * omega.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &sample in samples {
            let s0 = sample + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        power.max(0.0).sqrt() / samples.len() as f32
    }

    /// Block RMS envelope (used for the beating test).
    fn envelope(samples: &[f32], window: usize) -> Vec<f32> {
        samples.chunks(window).map(rms).collect()
    }

    /// Slow envelope: 250 ms windows, which average out the intra-mode
    /// beating (hundreds of Hz) and leave the detune beating (1–2 Hz).
    fn slow_envelope(samples: &[f32]) -> Vec<f32> {
        envelope(samples, (SR as usize / 4).max(1))
    }

    #[test]
    fn silent_without_a_note() {
        let mut modal = ModalProcessor::new(SR);
        let out = render(&mut modal, 40);
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(modal.active_voices(), 0);
    }

    #[test]
    fn presets_have_the_documented_shape() {
        assert_eq!(super::preset_count(), 4);
        assert_eq!(PRESETS[0].partials.len(), 4, "Mallet");
        assert_eq!(PRESETS[1].partials.len(), 4, "Vibraphone");
        assert_eq!(PRESETS[2].partials.len(), 8, "Bell");
        // The bowl table is the one the tail test relies on.
        assert_eq!(SINGING_BOWL.len(), 5);
    }

    #[test]
    fn strike_starts_atomically_and_modes_are_live() {
        let mut modal = ModalProcessor::new(SR);
        modal.note_event(note_on(60));
        assert_eq!(modal.active_voices(), 1);
        // Bowl: four beating pairs + one single mode.
        modal.set_param(ParamId::ModalPreset, 3.0);
        assert_eq!(modal.preset_index(), 3);
        let out = render(&mut modal, 10);
        assert_eq!(modal.resonator_count(), 9, "4 pairs + 1 single");
        assert!(rms(&out) > 1.0e-4);
    }

    #[test]
    fn bowl_still_rings_after_ten_seconds() {
        let mut modal = ModalProcessor::new(SR);
        modal.set_param(ParamId::ModalPreset, 3.0);
        modal.note_event(note_on(60));
        let out = render(&mut modal, blocks_for(11.0));
        let tail = rms(&out[out.len() - BLOCK * 8..]);
        assert!(tail > 1.0e-3, "11 s tail should still be audible: {tail}");
        assert!(tail < 0.5, "the tail must have decayed: {tail}");
    }

    #[test]
    fn mallet_tail_frees_its_voice() {
        let mut modal = ModalProcessor::new(SR);
        modal.note_event(note_on(72));
        let _ = render(&mut modal, blocks_for(4.0));
        assert_eq!(modal.active_voices(), 0, "short preset must free its slot");
    }

    /// The 26 s T60 and the −60 dB release threshold (B1-D8) meet, so a bowl
    /// voice must have released itself by 30 s — a leaked slot would keep the
    /// 16-voice pool full forever.
    #[test]
    fn bowl_tail_is_gone_within_thirty_seconds() {
        let mut modal = ModalProcessor::new(SR);
        modal.set_param(ParamId::ModalPreset, 3.0);
        modal.note_event(note_on(60));
        let mut buffer = AudioBuffer::new(1, BLOCK);
        for _ in 0..blocks_for(30.0) {
            buffer.set_len(BLOCK);
            modal.process(&ctx(), &mut buffer);
        }
        assert_eq!(
            modal.active_voices(),
            0,
            "a 26 s T60 must be released by 30 s"
        );
    }

    /// `hardness` is the contact time (B1-D4): a soft strike (0) smears the
    /// same peak amplitude over 8.5 ms, a hard one (1) over 0.5 ms, so the
    /// soft strike carries visibly more energy into the first 20 ms.
    #[test]
    fn hardness_sets_the_strike_length() {
        let energy = |hardness: f32| -> f32 {
            let mut modal = ModalProcessor::new(SR);
            modal.set_param(ParamId::ModalPreset, 0.0);
            modal.set_param(ParamId::ModalHardness, hardness);
            modal.note_event(note_on(60));
            render(&mut modal, 8).iter().map(|s| s * s).sum()
        };
        let soft = energy(0.0);
        let hard = energy(1.0);
        assert!(
            soft > hard * 1.2,
            "a soft strike must be the longer burst: soft {soft}, hard {hard}"
        );
    }

    #[test]
    fn note_off_keeps_the_tail_but_the_blanket_sweep_stops_it() {
        let mut modal = ModalProcessor::new(SR);
        modal.set_param(ParamId::ModalPreset, 3.0);
        modal.note_event(note_on(60));
        let _ = render(&mut modal, 20);
        let before = rms(&render(&mut modal, 20));
        modal.note_event(NoteEvent::Off { note: 60 });
        let after_off = rms(&render(&mut modal, 20));
        assert!(
            after_off > before * 0.5,
            "note-off must not damp a struck tail: {before} → {after_off}"
        );

        modal.note_event(NoteEvent::Off { note: 0 });
        let out = render(&mut modal, blocks_for(0.1));
        assert!(rms(&out[out.len() - BLOCK * 4..]) < 1.0e-5);
        assert_eq!(modal.active_voices(), 0);
    }

    #[test]
    fn retrigger_adds_to_the_ringing_state() {
        // Both runs are measured over the SAME time window (0.5–1.0 s); the
        // second run gets an extra strike at 0.5 s.
        let single = {
            let mut modal = ModalProcessor::new(SR);
            modal.set_param(ParamId::ModalPreset, 3.0);
            modal.note_event(note_on(60));
            let first = render(&mut modal, blocks_for(0.5));
            assert!(rms(&first) > 0.0);
            rms(&render(&mut modal, blocks_for(0.5)))
        };
        let double = {
            let mut modal = ModalProcessor::new(SR);
            modal.set_param(ParamId::ModalPreset, 3.0);
            modal.note_event(note_on(60));
            let _ = render(&mut modal, blocks_for(0.5));
            modal.note_event(note_on(60));
            rms(&render(&mut modal, blocks_for(0.5)))
        };
        assert!(
            double > single * 1.2,
            "a second strike must add energy: {single} → {double}"
        );
    }

    #[test]
    fn inharmonic_partial_peaks() {
        let mut modal = ModalProcessor::new(SR);
        modal.set_param(ParamId::ModalPreset, 3.0);
        modal.note_event(note_on(60));
        let out = render(&mut modal, blocks_for(2.0));
        let f0 = crate::realtime::synth::note_to_frequency(60);
        let at_f0 = goertzel(&out, f0);
        let at_271 = goertzel(&out, f0 * 2.71);
        let between = goertzel(&out, f0 * 1.85);
        assert!(
            at_f0 > between * 4.0 && at_271 > between * 2.0,
            "bowl peaks should sit on the mode ratios: f0={at_f0} 2.71f0={at_271} off={between}"
        );
    }

    #[test]
    fn beating_modulates_the_tail() {
        let depth = |beating: f32| {
            let mut modal = ModalProcessor::new(SR);
            modal.set_param(ParamId::ModalPreset, 3.0);
            modal.set_param(ParamId::ModalBeating, beating);
            modal.note_event(note_on(60));
            let out = render(&mut modal, blocks_for(4.0));
            let env = slow_envelope(&out);
            let mean = env.iter().sum::<f32>() / env.len() as f32;
            let min = env.iter().fold(f32::INFINITY, |a, e| a.min(*e));
            let max = env.iter().fold(0.0f32, |a, e| a.max(*e));
            (max - min) / mean.max(1.0e-6)
        };
        let beating = depth(1.0);
        let flat = depth(0.0);
        assert!(
            beating > flat * 1.5,
            "beating should visibly modulate the envelope: {beating} vs {flat}"
        );
    }

    #[test]
    fn decay_scale_controls_the_tail_length() {
        // Bowl preset: T60 26 s × 0.25 = 6.5 s vs × 4 = 104 s, measured at
        // 5 s so both are still ringing but at very different levels.
        let tail = |scale: f32| {
            let mut modal = ModalProcessor::new(SR);
            modal.set_param(ParamId::ModalPreset, 3.0);
            modal.set_param(ParamId::ModalDecay, scale);
            modal.note_event(note_on(60));
            let out = render(&mut modal, blocks_for(5.0));
            rms(&out[out.len() - BLOCK * 8..])
        };
        let long = tail(4.0);
        let short = tail(0.25);
        assert!(
            long > short * 2.0,
            "decay scale must stretch the tail: {long} vs {short}"
        );
    }

    #[test]
    fn brightness_changes_the_strike_spectrum() {
        let high_ratio = |brightness: f32| {
            let mut modal = ModalProcessor::new(SR);
            modal.set_param(ParamId::ModalBrightness, brightness);
            modal.note_event(note_on(72));
            let out = render(&mut modal, blocks_for(0.5));
            let f0 = crate::realtime::synth::note_to_frequency(72);
            goertzel(&out, f0 * 8.0) / goertzel(&out, f0).max(1.0e-9)
        };
        let bright = high_ratio(1.0);
        let dark = high_ratio(0.0);
        assert!(
            bright > dark * 2.0,
            "brightness must lift the high modes: {bright} vs {dark}"
        );
    }

    #[test]
    fn pool_is_bounded_and_stealing_stays_finite() {
        let mut modal = ModalProcessor::new(SR);
        for note in 40..40 + (MAX_VOICES as u8 + 1) {
            modal.note_event(note_on(note));
        }
        assert!(modal.active_voices() <= MAX_VOICES);
        let out = render(&mut modal, 40);
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn extreme_parameters_stay_finite() {
        let mut modal = ModalProcessor::new(SR);
        modal.set_param(ParamId::ModalGain, 2.0);
        modal.set_param(ParamId::ModalHardness, 1.0);
        modal.set_param(ParamId::ModalBrightness, 1.0);
        modal.set_param(ParamId::ModalDecay, 4.0);
        modal.set_param(ParamId::ModalBeating, 2.0);
        modal.set_param(ParamId::PitchOffset, 24.0);
        for note in [0u8, 1, 60, 127] {
            modal.note_event(NoteEvent::On {
                note,
                velocity: f32::NAN,
            });
            modal.note_event(note_on(note));
        }
        let out = render(&mut modal, 80);
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn params_clamp_and_reject_non_finite() {
        let mut modal = ModalProcessor::new(SR);
        assert!(modal.set_param(ParamId::ModalDecay, 100.0));
        assert_eq!(modal.decay_scale, 4.0);
        assert!(modal.set_param(ParamId::ModalGain, -5.0));
        assert_eq!(modal.gain, 0.0);
        assert!(!modal.set_param(ParamId::ModalDecay, f32::NAN));
        assert!(!modal.set_param(ParamId::MasterGain, 1.0));
        assert!(modal.set_param(ParamId::ModalPreset, 9.0));
        assert_eq!(modal.preset_index(), 3, "preset index is clamped");
    }

    #[test]
    fn zero_velocity_does_not_strike() {
        let mut modal = ModalProcessor::new(SR);
        modal.note_event(NoteEvent::On {
            note: 60,
            velocity: 0.0,
        });
        assert_eq!(modal.active_voices(), 0);
        let out = render(&mut modal, 10);
        assert!(out.iter().all(|s| *s == 0.0));
    }
}
