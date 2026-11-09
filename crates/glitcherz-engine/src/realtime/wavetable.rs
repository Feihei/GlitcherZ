//! Wavetable synthesizer (spec `.agents/spec/wavetable-instrument.md`).
//!
//! One phase accumulator reads a bank of single-cycle tables. The bank holds
//! [`FRAMES`] anchor timbres at [`MIP_LEVELS`] band limits each, so an
//! arbitrary note can always read a table whose top harmonic stays below
//! Nyquist.
//!
//! Three properties drive the DSP choices:
//!
//! - **Tables are generated from harmonic spectra, not drawn.** Each frame is a
//!   list of `(k, amplitude)` partials; a band-limited level simply stops
//!   summing at its harmonic ceiling (S3-D2). That is why the layering costs a
//!   loop counter instead of an FFT or a filter.
//! - **One normalisation per frame, shared by every level.** Normalising per
//!   level would make high notes quieter-but-louder: with only a few partials
//!   left, scaling each to full peak raises the perceived level (S3-D6).
//! - **`position` is the only timbre control.** There is no filter and no
//!   unison; sweeping between frames is what shaping the sound means here
//!   (S3-D15).
//!
//! The bank is built once per process ([`OnceLock`]) and shared read-only: a
//! node construction never pays for it, and the audio thread only ever indexes
//! a `&'static` slice.

use std::f32::consts::TAU;
use std::sync::OnceLock;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::envelope::AdsrProcessor;
use crate::realtime::processor::Processor;
use crate::realtime::synth::note_to_frequency;

/// Voice budget, matching the other built-in instruments.
pub const MAX_VOICES: usize = 16;

/// Anchor timbres per bank: sine, triangle, saw, square.
pub const FRAMES: usize = 4;

/// Band-limited levels per frame: 256, 128, …, 1 harmonic (one per octave).
pub const MIP_LEVELS: usize = 9;

/// Samples per single-cycle table. **Must be a power of two**: table
/// generation indexes the sine LUT with `(k·i) & (TABLE_SIZE - 1)` (S3-D11).
pub const TABLE_SIZE: usize = 4096;

/// Harmonics kept by the widest level (`level 0`).
const BASE_HARMONICS: usize = 256;

/// Mono scratch ceiling, same role as the other generators'.
const SCRATCH_FRAMES: usize = 1024;

/// Nyquist guard: a partial this close to `fs/2` is dropped (S3-D3).
const NYQUIST_LIMIT: f32 = 0.45;

/// Node output trim: the widest frame peaks at 1.0 after normalisation, so
/// this puts one voice in the same range as the other built-in sources.
const OUTPUT_SCALE: f32 = 0.35;

/// Pitch-offset bound in semitones, same as the other built-in sources.
const PITCH_OFFSET_LIMIT: f32 = 24.0;

/// One-pole time constant for `PitchOffset` (ms), copied from `osc.rs`.
const PITCH_SMOOTHING_MS: f32 = 5.0;

/// One-pole time constant for `position` (ms) — a block-rate automation
/// staircase becomes a ramp (S3-D9).
const POSITION_SMOOTHING_MS: f32 = 10.0;

/// Blanket-stop fade (Stop / Panic), same as the other generators.
const BLANKET_SECONDS: f32 = 0.01;

/// Default key-switch ramp (S3-D15).
const DEFAULT_ATTACK_S: f32 = 0.005;
const DEFAULT_RELEASE_S: f32 = 0.12;

/// Amplitude of harmonic `k` in `frame`, zero when the frame has no such
/// partial. A negative amplitude is a phase of π, which is why no cosine table
/// is needed.
fn frame_amplitude(frame: usize, k: usize) -> f32 {
    let kf = k as f32;
    match frame {
        // Sine: fundamental only.
        0 => {
            if k == 1 {
                1.0
            } else {
                0.0
            }
        }
        // Triangle: odd partials with 1/k² and alternating sign.
        1 => {
            if k % 2 == 1 {
                let sign = if ((k - 1) / 2) % 2 == 0 { 1.0 } else { -1.0 };
                sign / (kf * kf)
            } else {
                0.0
            }
        }
        // Saw: every partial with 1/k.
        2 => 1.0 / kf,
        // Square: odd partials with 1/k.
        _ => {
            if k % 2 == 1 {
                1.0 / kf
            } else {
                0.0
            }
        }
    }
}

/// One frame's partial amplitudes, indexed by `k - 1`.
fn frame_amplitudes(frame: usize) -> [f32; BASE_HARMONICS] {
    let mut amps = [0.0f32; BASE_HARMONICS];
    for (index, amp) in amps.iter_mut().enumerate() {
        *amp = frame_amplitude(frame, index + 1);
    }
    amps
}

/// Generated table data, shared by every wavetable node in the process.
struct Bank {
    /// `((frame · MIP_LEVELS) + level) · TABLE_SIZE + sample`.
    data: Vec<f32>,
    /// Peak normalisation applied to each frame, reused by
    /// [`preview_waveform`] so the drawing matches what is heard.
    scales: [f32; FRAMES],
}

impl Bank {
    fn table(&self, frame: usize, level: usize) -> &[f32] {
        let base = (frame * MIP_LEVELS + level) * TABLE_SIZE;
        &self.data[base..base + TABLE_SIZE]
    }
}

fn build_bank() -> Bank {
    // `sin(2π·k·i/N)` lands exactly on this grid, so the whole generation
    // needs no trigonometry at all (S3-D11).
    let mut sin_lut = vec![0.0f32; TABLE_SIZE];
    for (i, slot) in sin_lut.iter_mut().enumerate() {
        *slot = (TAU * i as f32 / TABLE_SIZE as f32).sin();
    }

    let mut data = vec![0.0f32; FRAMES * MIP_LEVELS * TABLE_SIZE];
    let mut scales = [1.0f32; FRAMES];

    for (frame, scale_slot) in scales.iter_mut().enumerate() {
        let amps = frame_amplitudes(frame);

        // Level 0 first: its peak is the normalisation reference for the whole
        // frame, so switching levels never changes the level (S3-D6).
        let scale = {
            let base = frame * MIP_LEVELS * TABLE_SIZE;
            let slice = &mut data[base..base + TABLE_SIZE];
            let mut peak = 0.0f32;
            for (i, slot) in slice.iter_mut().enumerate() {
                let mut acc = 0.0f32;
                for (index, amp) in amps.iter().enumerate() {
                    if *amp != 0.0 {
                        acc += *amp * sin_lut[((index + 1) * i) & (TABLE_SIZE - 1)];
                    }
                }
                *slot = acc;
                peak = peak.max(acc.abs());
            }
            let scale = if peak > 1.0e-6 { 1.0 / peak } else { 1.0 };
            for slot in slice.iter_mut() {
                *slot *= scale;
            }
            scale
        };
        *scale_slot = scale;

        for level in 1..MIP_LEVELS {
            let limit = BASE_HARMONICS >> level;
            let base = (frame * MIP_LEVELS + level) * TABLE_SIZE;
            let slice = &mut data[base..base + TABLE_SIZE];
            for (i, slot) in slice.iter_mut().enumerate() {
                let mut acc = 0.0f32;
                for (index, amp) in amps[..limit].iter().enumerate() {
                    if *amp != 0.0 {
                        acc += *amp * sin_lut[((index + 1) * i) & (TABLE_SIZE - 1)];
                    }
                }
                *slot = acc * scale;
            }
        }
    }

    Bank { data, scales }
}

/// The process-wide bank. Built on first use, never freed.
fn bank() -> &'static Bank {
    static BANK: OnceLock<Bank> = OnceLock::new();
    BANK.get_or_init(build_bank)
}

/// Lowest band-limited level whose harmonic ceiling still fits under Nyquist.
///
/// `None` means even the fundamental is past the guard band — the voice stays
/// silent rather than aliasing (S3-D3).
fn select_level(f0: f32, ratio: f32, sample_rate: f32) -> Option<usize> {
    if !f0.is_finite() || f0 <= 0.0 || !sample_rate.is_finite() || sample_rate <= 0.0 {
        return None;
    }
    let max_harmonic = NYQUIST_LIMIT * sample_rate / (f0 * ratio.max(1.0e-6));
    // `is_finite` also rejects NaN, which would slip past a `>=` comparison.
    if !max_harmonic.is_finite() || max_harmonic < 1.0 {
        return None;
    }
    let mut level = 0;
    while level < MIP_LEVELS && (BASE_HARMONICS >> level) as f32 > max_harmonic {
        level += 1;
    }
    (level < MIP_LEVELS).then_some(level)
}

/// One wavetable voice: one phase, one band-limited level, one A/R ramp.
struct Voice {
    /// MIDI note this voice is playing; kept after release so the tail keeps
    /// rendering at the right pitch.
    note: Option<u8>,
    /// Whether the key is still down. Releasing clears this but not `note`.
    held: bool,
    /// Monotonic allocation stamp (steal = smallest).
    age: u64,
    env: AdsrProcessor,
    /// Cycle phase, `0..1`.
    phase: f32,
    /// Phase increment for this note (`f0/fs`), before the pitch multiplier.
    base_inc: f32,
    /// Current per-sample pitch multiplier, smoothed toward the target.
    pitch: f32,
    /// Band-limited level chosen for the note's pitch.
    level: usize,
    /// `false` when even the fundamental is past the Nyquist guard.
    audible: bool,
}

impl Voice {
    fn new(sample_rate: f32) -> Self {
        let mut env = AdsrProcessor::new(sample_rate);
        // A/R only: zero-length decay, full sustain (S3-D7).
        env.set_param(ParamId::Decay, 0.0);
        env.set_param(ParamId::Sustain, 1.0);
        env.set_param(ParamId::Attack, DEFAULT_ATTACK_S);
        env.set_param(ParamId::Release, DEFAULT_RELEASE_S);
        Self {
            note: None,
            held: false,
            age: 0,
            env,
            phase: 0.0,
            base_inc: 0.0,
            pitch: 1.0,
            level: 0,
            audible: false,
        }
    }

    /// A slot is free only after its release tail has fully decayed.
    fn is_free(&self) -> bool {
        self.env.is_idle()
    }
}

/// Recomputes a voice's increment and band-limited level.
///
/// Called on note-on and on every tuning change. Phase and envelope are left
/// alone: a stolen voice glides to the new note instead of restarting.
fn configure(voice: &mut Voice, ratio: f32, sample_rate: f32) {
    let Some(note) = voice.note else {
        voice.audible = false;
        return;
    };
    let f0 = note_to_frequency(note);
    voice.base_inc = f0 / sample_rate;
    // Judged with the *larger* of the current and target ratio so a glide
    // cannot alias on the way (same rule as the organ, S3-D3).
    let judged = voice.pitch.max(ratio);
    match select_level(f0, judged, sample_rate) {
        Some(level) => {
            voice.level = level;
            voice.audible = true;
        }
        None => voice.audible = false,
    }
}

/// Polyphonic wavetable source.
///
/// Note semantics (S3-D8, identical to the organ's):
/// - `On { note, velocity }`: a voice already playing `note` is **reused** (a
///   key is a key — no second voice, no +6 dB doubling), otherwise a free slot
///   takes the note, otherwise the oldest voice is stolen;
/// - `Off { note }` with `note != 0`: releases the voices playing that note;
/// - `Off { note: 0 }`: blanket release sentinel used by
///   `RtGraph::note_off_all`.
pub struct WavetableProcessor {
    sample_rate: f32,
    bank: &'static Bank,
    voices: [Voice; MAX_VOICES],
    scratch: AudioBuffer,
    /// Per-sample smoothed `position`, refreshed once per block and shared by
    /// every voice.
    positions: Vec<f32>,
    stamps: u64,
    position: f32,
    position_target: f32,
    attack_s: f32,
    release_s: f32,
    gain: f32,
    pitch_ratio: f32,
    /// Set when a parameter that changes partial counts or masks moved.
    levels_dirty: bool,
}

impl WavetableProcessor {
    /// When `sample_rate <= 0` the module falls back to 48 kHz (construction
    /// -time tolerance, not on the real-time path).
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = if sample_rate.is_finite() && sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            sample_rate,
            bank: bank(),
            voices: std::array::from_fn(|_| Voice::new(sample_rate)),
            scratch: AudioBuffer::new(1, SCRATCH_FRAMES),
            positions: vec![0.0; SCRATCH_FRAMES],
            stamps: 0,
            position: 0.0,
            position_target: 0.0,
            attack_s: DEFAULT_ATTACK_S,
            release_s: DEFAULT_RELEASE_S,
            gain: 1.0,
            pitch_ratio: 1.0,
            levels_dirty: true,
        }
    }

    /// Number of voices that have not reached Idle yet. RT-safe.
    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|voice| !voice.is_free()).count()
    }

    /// The smoothed `position` after the last processed block. RT-safe.
    pub fn position(&self) -> f32 {
        self.position
    }

    /// The `position` the ramp is heading for. RT-safe.
    pub fn position_target(&self) -> f32 {
        self.position_target
    }

    /// First free slot, else the oldest one (S3-D8). RT-safe: fixed scan.
    fn allocate(&self) -> usize {
        if let Some(index) = self.voices.iter().position(Voice::is_free) {
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

    fn next_stamp(&mut self) -> u64 {
        self.stamps = self.stamps.wrapping_add(1);
        self.stamps
    }

    fn retune_all(&mut self) {
        let ratio = self.pitch_ratio;
        let sample_rate = self.sample_rate;
        for voice in &mut self.voices {
            if voice.is_free() {
                continue;
            }
            configure(voice, ratio, sample_rate);
        }
    }

    fn trigger(&mut self, note: u8, velocity: f32) {
        let velocity = if velocity.is_finite() {
            velocity.clamp(0.0, 1.0)
        } else {
            0.0
        };
        // `velocity == 0` is "no note", not a silent note (S3-D7).
        if velocity <= 0.0 {
            return;
        }
        let index = self
            .voices
            .iter()
            .position(|voice| voice.held && voice.note == Some(note))
            .unwrap_or_else(|| self.allocate());
        let age = self.next_stamp();
        let ratio = self.pitch_ratio;
        let sample_rate = self.sample_rate;

        let voice = &mut self.voices[index];
        voice.note = Some(note);
        voice.held = true;
        voice.age = age;
        voice.pitch = ratio;
        // The phase is deliberately kept: resetting it would step the
        // waveform while the ramped envelope is still above zero.
        configure(voice, ratio, sample_rate);
        // `AdsrProcessor::trigger` clamps its level into `[0, velocity]` and
        // ramps from the current value, so velocity is the envelope peak and a
        // reused or stolen slot never clicks (S3-D7).
        voice.env.note_event(NoteEvent::On { note, velocity });
    }

    /// Releases every voice holding `note`.
    fn release_note(&mut self, note: u8) {
        for voice in &mut self.voices {
            if voice.held && voice.note == Some(note) {
                voice.held = false;
                voice.env.note_event(NoteEvent::Off { note });
            }
        }
    }

    /// Releases everything with a fixed ramp, regardless of `release`.
    fn release_all(&mut self, seconds: f32) {
        let restore = self.release_s;
        for voice in &mut self.voices {
            if voice.is_free() {
                continue;
            }
            voice.held = false;
            voice.env.set_param(ParamId::Release, seconds);
            voice.env.note_event(NoteEvent::Off { note: 0 });
            voice.env.set_param(ParamId::Release, restore);
        }
    }
}

/// Renders one voice into a mono scratch (overwrite), without the envelope.
fn render_voice(
    voice: &mut Voice,
    out: &mut [f32],
    pitch_target: f32,
    pitch_coeff: f32,
    bank: &Bank,
    positions: &[f32],
) {
    if !voice.audible {
        out.fill(0.0);
        return;
    }
    let base_inc = voice.base_inc;
    let level = voice.level;
    let mut phase = voice.phase;
    let mut pitch = voice.pitch;
    let span = (FRAMES - 1) as f32;
    let last_index = FRAMES - 2;

    for (index, sample) in out.iter_mut().enumerate() {
        pitch += (pitch_target - pitch) * pitch_coeff;
        phase += base_inc * pitch;
        if phase >= 1.0 {
            phase -= phase.floor();
        }

        let x = phase * TABLE_SIZE as f32;
        let i0 = (x as usize).min(TABLE_SIZE - 1);
        let frac = x - i0 as f32;
        let i1 = (i0 + 1) & (TABLE_SIZE - 1);

        let position = positions.get(index).copied().unwrap_or(0.0);
        // `position` spans FRAMES - 1 gaps; the lower frame of the pair is
        // clamped so `j + 1` stays inside the bank.
        let f = position * span;
        let j = (f as usize).min(last_index);
        let t = (f - j as f32).clamp(0.0, 1.0);

        let a_slice = bank.table(j, level);
        let b_slice = bank.table(j + 1, level);
        let a = a_slice[i0] + (a_slice[i1] - a_slice[i0]) * frac;
        let b = b_slice[i0] + (b_slice[i1] - b_slice[i0]) * frac;
        *sample = (a + (b - a) * t) * OUTPUT_SCALE;
    }

    voice.phase = phase;
    voice.pitch = pitch;
}

impl Processor for WavetableProcessor {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        if frames == 0 || buffer.channels() == 0 {
            return;
        }
        // The wavetable is a source: it owns its output entirely.
        for ch in 0..buffer.channels() {
            if let Some(dst) = buffer.channel_mut(ch) {
                dst.fill(0.0);
            }
        }
        if self.levels_dirty {
            self.levels_dirty = false;
            self.retune_all();
        }

        let rendered = frames.min(SCRATCH_FRAMES);
        // One position ramp per block, shared by every voice (S3-D9).
        let coeff = position_coefficient(self.sample_rate);
        let target = self.position_target;
        let mut position = self.position;
        for slot in &mut self.positions[..rendered] {
            position += (target - position) * coeff;
            *slot = position;
        }
        self.position = position;

        let output_gain = self.gain;
        let pitch_target = self.pitch_ratio;
        let pitch_coeff = pitch_coefficient(self.sample_rate);
        let bank = self.bank;
        let Self {
            voices,
            scratch,
            positions,
            ..
        } = self;
        scratch.set_len(rendered);
        for voice in voices.iter_mut() {
            if voice.is_free() {
                continue;
            }
            scratch.clear();
            if let Some(mono) = scratch.channel_mut(0) {
                render_voice(
                    voice,
                    mono,
                    pitch_target,
                    pitch_coeff,
                    bank,
                    &positions[..rendered],
                );
            }
            // The key-switch ramp is the only envelope: the raw table read has
            // no attack, and velocity lives in the envelope peak.
            voice.env.process(ctx, scratch);
            let Some(mono) = scratch.channel(0) else {
                continue;
            };
            for ch in 0..buffer.channels() {
                if let Some(dst) = buffer.channel_mut(ch) {
                    for (out, sample) in dst.iter_mut().zip(mono.iter()) {
                        *out += *sample * output_gain;
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
            ParamId::WavetablePosition => self.position_target = value.clamp(0.0, 1.0),
            // The attack ramp is read live by the envelope; `release` is
            // sampled when the key is let go.
            ParamId::WavetableAttack => {
                self.attack_s = value.clamp(0.001, 2.0);
                for voice in &mut self.voices {
                    voice.env.set_param(ParamId::Attack, self.attack_s);
                }
            }
            ParamId::WavetableRelease => {
                self.release_s = value.clamp(0.005, 5.0);
                for voice in &mut self.voices {
                    voice.env.set_param(ParamId::Release, self.release_s);
                }
            }
            ParamId::WavetableGain => self.gain = value.clamp(0.0, 2.0),
            ParamId::PitchOffset => {
                let semitones = value.clamp(-PITCH_OFFSET_LIMIT, PITCH_OFFSET_LIMIT);
                self.pitch_ratio = (semitones / 12.0).exp2();
                // The level depends on the sounding pitch, which the voices
                // hold, so it has to be re-picked per voice.
                self.levels_dirty = true;
            }
            _ => return false,
        }
        true
    }

    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { note, velocity } => self.trigger(note, velocity),
            NoteEvent::Off { note: 0 } => self.release_all(BLANKET_SECONDS),
            NoteEvent::Off { note } => self.release_note(note),
        }
        true
    }

    /// The wavetable has no CC surface; consuming the event keeps the graph
    /// quiet (same rationale as the other built-in sources).
    fn midi_cc(&mut self, _channel: u8, _controller: u8, _value: u8) -> bool {
        true
    }
}

/// Per-sample one-pole coefficient for pitch smoothing (S3-D9).
fn pitch_coefficient(sample_rate: f32) -> f32 {
    one_pole(PITCH_SMOOTHING_MS, sample_rate)
}

/// Per-sample one-pole coefficient for position smoothing (S3-D9).
fn position_coefficient(sample_rate: f32) -> f32 {
    one_pole(POSITION_SMOOTHING_MS, sample_rate)
}

fn one_pole(milliseconds: f32, sample_rate: f32) -> f32 {
    let tau = milliseconds * 0.001;
    (1.0 - (-1.0 / (tau * sample_rate)).exp()).clamp(1.0e-4, 1.0)
}

/// Fills `out` with one cycle of the waveform at `position`, for the
/// inspector's preview.
///
/// Control-thread only (not real-time): it re-sums the same harmonic spectra
/// instead of reading the playback bank, because the bank's level depends on a
/// note frequency and a preview has none. Band-limiting to `out.len() / 2`
/// keeps the drawing free of aliasing at any preview width.
pub fn preview_waveform(position: f32, out: &mut [f32]) {
    if out.is_empty() {
        return;
    }
    let position = if position.is_finite() {
        position.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let span = (FRAMES - 1) as f32;
    let last_index = FRAMES - 2;
    let f = position * span;
    let j = (f as usize).min(last_index);
    let t = (f - j as f32).clamp(0.0, 1.0);

    let bank = bank();
    let a_amps = frame_amplitudes(j);
    let b_amps = frame_amplitudes(j + 1);
    let a_scale = bank.scales[j];
    let b_scale = bank.scales[j + 1];
    let limit = (out.len() / 2).clamp(1, BASE_HARMONICS);
    let n = out.len() as f32;

    for (i, slot) in out.iter_mut().enumerate() {
        let mut a = 0.0f32;
        let mut b = 0.0f32;
        for k in 1..=limit {
            let angle = TAU * (k * i) as f32 / n;
            let sine = angle.sin();
            a += a_amps[k - 1] * sine;
            b += b_amps[k - 1] * sine;
        }
        *slot = (a * a_scale) * (1.0 - t) + (b * b_scale) * t;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FRAMES, MAX_VOICES, MIP_LEVELS, WavetableProcessor, preview_waveform, select_level,
    };
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::{NoteEvent, ParamId};
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;
    /// A4 = 440 Hz.
    const A4: u8 = 69;
    /// A1 = 55 Hz: the widest level still fits under Nyquist.
    const A1: u8 = 33;
    /// A3 = 220 Hz.
    const A3: u8 = 45;
    /// A5 = 880 Hz.
    const A5: u8 = 81;
    /// A6 = 1760 Hz: only 8 partials survive there.
    const A6: u8 = 93;
    /// `position` that lands exactly on the saw frame.
    const SAW: f32 = 2.0 / 3.0;

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

    fn render(processor: &mut WavetableProcessor, blocks: usize) -> Vec<f32> {
        let mut buffer = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(blocks * BLOCK);
        for _ in 0..blocks {
            buffer.set_len(BLOCK);
            processor.process(&ctx(), &mut buffer);
            out.extend_from_slice(buffer.channel(0).expect("mono"));
        }
        out
    }

    fn note_on(note: u8, velocity: f32) -> NoteEvent {
        NoteEvent::On { note, velocity }
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs()))
    }

    /// Largest absolute difference between neighbouring samples.
    fn max_step(samples: &[f32]) -> f32 {
        samples
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0f32, f32::max)
    }

    /// Slice by seconds, clamped to the rendered length.
    fn window(samples: &[f32], from: f32, to: f32) -> &[f32] {
        let start = ((from * SR) as usize).min(samples.len());
        let end = ((to * SR) as usize).min(samples.len());
        &samples[start..end.max(start)]
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

    fn set(processor: &mut WavetableProcessor, id: ParamId, value: f32) {
        assert!(processor.set_param(id, value), "{id:?} rejected {value}");
    }

    /// Renders a held note at `position` and returns the steady-state window.
    fn steady(note: u8, position: f32) -> Vec<f32> {
        let mut source = WavetableProcessor::new(SR);
        set(&mut source, ParamId::WavetablePosition, position);
        source.note_event(note_on(note, 1.0));
        let out = render(&mut source, blocks_for(0.6));
        window(&out, 0.1, 0.5).to_vec()
    }

    #[test]
    fn silent_without_a_note() {
        let mut source = WavetableProcessor::new(SR);
        let out = render(&mut source, 40);
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(source.active_voices(), 0);
    }

    #[test]
    fn a_held_note_produces_sound() {
        let mut source = WavetableProcessor::new(SR);
        source.note_event(note_on(A4, 1.0));
        let out = render(&mut source, blocks_for(0.5));
        let level = rms(window(&out, 0.05, 0.4));
        assert!(
            level > 0.05 && level < 0.6,
            "the default sine frame should sit in the instrument range: {level}"
        );
        // The default attack is 5 ms: the first millisecond is still ramping.
        assert!(rms(window(&out, 0.02, 0.3)) > level * 0.9);
    }

    #[test]
    fn pitch_follows_the_note_and_the_offset() {
        let mut source = WavetableProcessor::new(SR);
        source.note_event(note_on(A4, 1.0));
        let out = render(&mut source, blocks_for(0.6));
        let span = window(&out, 0.1, 0.5);
        let centre = goertzel(span, 440.0);
        let below = goertzel(span, 415.30);
        let above = goertzel(span, 466.16);
        assert!(
            centre > below * 4.0 && centre > above * 4.0,
            "A4 must land on 440 Hz: centre {centre}, neighbours {below} / {above}"
        );

        let mut shifted = WavetableProcessor::new(SR);
        set(&mut shifted, ParamId::PitchOffset, 12.0);
        shifted.note_event(note_on(A4, 1.0));
        let out = render(&mut shifted, blocks_for(0.6));
        let span = window(&out, 0.1, 0.5);
        assert!(
            goertzel(span, 880.0) > goertzel(span, 440.0) * 4.0,
            "+12 semitones must move the fundamental to 880 Hz"
        );
    }

    #[test]
    fn position_zero_is_a_pure_sine() {
        let span = steady(A4, 0.0);
        let f0 = goertzel(&span, 440.0);
        let f2 = goertzel(&span, 880.0);
        let f3 = goertzel(&span, 1320.0);
        assert!(
            f0 > f2 * 50.0 && f0 > f3 * 50.0,
            "the sine frame must have nothing above the fundamental: {f0} / {f2} / {f3}"
        );
    }

    #[test]
    fn position_one_is_a_square() {
        let span = steady(A4, 1.0);
        let f2 = goertzel(&span, 880.0);
        let f3 = goertzel(&span, 1320.0);
        let f5 = goertzel(&span, 2200.0);
        assert!(
            f3 > f2 * 10.0 && f5 > f2 * 10.0,
            "the square frame is odd-only: 2f0 {f2} must lose to 3f0 {f3} and 5f0 {f5}"
        );
    }

    #[test]
    fn position_sweeps_the_spectrum() {
        // 3·f0: absent in a sine, 1/9 in a triangle, 1/3 in a saw.
        let sine = goertzel(&steady(A4, 0.0), 1320.0);
        let triangle = goertzel(&steady(A4, 1.0 / 3.0), 1320.0);
        let saw = goertzel(&steady(A4, SAW), 1320.0);
        assert!(
            sine < triangle && triangle < saw,
            "3f0 must grow along the sweep: {sine} / {triangle} / {saw}"
        );
        assert!(
            saw > triangle * 1.3,
            "the saw's 3f0 is 1/3 against the triangle's 1/9: {saw} vs {triangle}"
        );
    }

    #[test]
    fn band_limited_tables_reject_aliasing() {
        // A6 = 1760 Hz → max_harmonic ≈ 12.3 → level 5 keeps 8 partials.
        let span = steady(A6, SAW);
        let inside = goertzel(&span, 8.0 * 1760.0);
        let outside = goertzel(&span, 9.0 * 1760.0);
        assert!(
            inside > outside * 10.0,
            "the 9th partial sits above the chosen level: inside {inside}, outside {outside}"
        );
    }

    #[test]
    fn lower_notes_keep_more_harmonics() {
        // Both notes are measured relative to their own fundamental, so this
        // compares harmonic coverage rather than absolute level.
        let low = steady(A3, SAW);
        let low_ratio = goertzel(&low, 20.0 * 220.0) / goertzel(&low, 220.0);
        let high = steady(A5, SAW);
        let high_ratio = goertzel(&high, 20.0 * 880.0) / goertzel(&high, 880.0);
        assert!(
            low_ratio > 0.005,
            "A3 should still carry its 20th partial: {low_ratio}"
        );
        assert!(
            high_ratio < 0.001,
            "A5 stops at its 16th partial, so the 20th must be gone: {high_ratio}"
        );
    }

    #[test]
    fn select_level_is_monotonic_in_pitch() {
        let level = |f0: f32| select_level(f0, 1.0, SR);
        assert_eq!(level(55.0), Some(0));
        assert_eq!(level(220.0), Some(2));
        assert_eq!(level(880.0), Some(4));
        assert_eq!(level(1760.0), Some(5));
        assert_eq!(level(20_000.0), Some(8));
        assert_eq!(
            level(22_000.0),
            None,
            "the fundamental itself is past 0.45·fs"
        );

        let mut previous = 0;
        for step in 1..=40 {
            let f0 = 40.0 * 1.12f32.powi(step);
            if let Some(current) = level(f0) {
                assert!(
                    current >= previous,
                    "level must not fall as pitch rises: {f0} Hz → {current} after {previous}"
                );
                previous = current;
            }
        }
    }

    #[test]
    fn level_switch_keeps_the_level() {
        // A1 uses the widest level (256 partials), A6 only 8. Both frames are
        // normalised once, so the audible level must not jump.
        let low = rms(&steady(A1, SAW));
        let high = rms(&steady(A6, SAW));
        let ratio = low / high;
        assert!(
            (0.5..2.0).contains(&ratio),
            "switching levels must not change the level: A1 {low}, A6 {high}"
        );
    }

    #[test]
    fn crossing_a_layer_boundary_stays_continuous() {
        // The handover from level L to L+1 is a hard cut at block rate (S3-D4).
        // Everything it drops sits just under 0.45·fs with a 1/k amplitude, so
        // the seam must not step further than the slew the waveform already
        // produces at those pitches. A4 → level 3 (32 partials) hands over to
        // level 4 (16 partials) at 21600 / 440 / 32 × … ≈ +7.37 semitones.
        let boundary_ratio = super::NYQUIST_LIMIT * SR / (440.0 * 32.0);
        let boundary = 12.0 * boundary_ratio.log2();
        let margin = 1.0;

        let steady_step = |semitones: f32| {
            let mut source = WavetableProcessor::new(SR);
            set(&mut source, ParamId::WavetablePosition, 1.0);
            set(&mut source, ParamId::PitchOffset, semitones);
            source.note_event(note_on(A4, 1.0));
            let out = render(&mut source, blocks_for(0.6));
            max_step(window(&out, 0.2, 0.5))
        };

        let mut source = WavetableProcessor::new(SR);
        set(&mut source, ParamId::WavetablePosition, 1.0);
        set(&mut source, ParamId::PitchOffset, boundary - margin);
        source.note_event(note_on(A4, 1.0));
        let lead_in = blocks_for(0.1);
        let sweep = blocks_for(0.6);
        let mut buffer = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity((lead_in + sweep) * BLOCK);
        for block in 0..lead_in + sweep {
            let progress = ((block as f32 - lead_in as f32) / sweep as f32).clamp(0.0, 1.0);
            let offset = boundary - margin + 2.0 * margin * progress;
            set(&mut source, ParamId::PitchOffset, offset);
            buffer.set_len(BLOCK);
            source.process(&ctx(), &mut buffer);
            out.extend_from_slice(buffer.channel(0).expect("mono"));
        }

        let seam = max_step(&out[lead_in * BLOCK..]);
        let steady = steady_step(boundary - margin).max(steady_step(boundary + margin));
        assert!(
            seam <= steady * 1.25,
            "the layer handover must stay inside the waveform's own slew: seam {seam}, steady {steady}"
        );
    }

    #[test]
    fn attack_and_release_shape_the_envelope() {
        let mut source = WavetableProcessor::new(SR);
        set(&mut source, ParamId::WavetableAttack, 0.05);
        source.note_event(note_on(A4, 1.0));
        let out = render(&mut source, blocks_for(0.3));
        let early = rms(window(&out, 0.0, 0.01));
        let late = rms(window(&out, 0.06, 0.1));
        assert!(
            early < late * 0.5,
            "a 50 ms attack must still be ramping at 10 ms: {early} vs {late}"
        );

        source.note_event(NoteEvent::Off { note: A4 });
        let out = render(&mut source, blocks_for(0.4));
        assert!(
            rms(window(&out, 0.2, 0.35)) < 1.0e-4,
            "the release must reach silence"
        );
    }

    #[test]
    fn blanket_off_stops_everything() {
        let mut source = WavetableProcessor::new(SR);
        source.note_event(note_on(A4, 1.0));
        source.note_event(note_on(A4 + 4, 1.0));
        let _ = render(&mut source, blocks_for(0.2));
        assert_eq!(source.active_voices(), 2);

        source.note_event(NoteEvent::Off { note: 0 });
        let out = render(&mut source, blocks_for(0.2));
        assert!(
            peak(window(&out, 0.05, 0.15)) < 1.0e-4,
            "the blanket sentinel must silence every voice"
        );
    }

    #[test]
    fn velocity_scales_the_level() {
        let level = |velocity: f32| {
            let mut source = WavetableProcessor::new(SR);
            source.note_event(note_on(A4, velocity));
            let out = render(&mut source, blocks_for(0.4));
            rms(window(&out, 0.1, 0.35))
        };
        let full = level(1.0);
        let half = level(0.5);
        let ratio = half / full;
        assert!(
            (0.45..0.55).contains(&ratio),
            "half velocity should halve the level: {ratio}"
        );
    }

    #[test]
    fn zero_velocity_is_silent() {
        let mut source = WavetableProcessor::new(SR);
        source.note_event(note_on(A4, 0.0));
        let out = render(&mut source, blocks_for(0.2));
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(
            source.active_voices(),
            0,
            "zero velocity must not take a slot"
        );
    }

    #[test]
    fn retrigger_reuses_the_voice() {
        let mut source = WavetableProcessor::new(SR);
        source.note_event(note_on(A4, 1.0));
        let _ = render(&mut source, blocks_for(0.1));
        source.note_event(note_on(A4, 1.0));
        let _ = render(&mut source, blocks_for(0.1));
        assert_eq!(
            source.active_voices(),
            1,
            "a key is a key — a repeated note must not double the voice"
        );
    }

    #[test]
    fn pool_is_bounded_and_stealing_stays_finite() {
        let mut source = WavetableProcessor::new(SR);
        for step in 0..(MAX_VOICES as u8 + 6) {
            source.note_event(note_on(40 + step, 1.0));
            let _ = render(&mut source, blocks_for(0.02));
        }
        let out = render(&mut source, blocks_for(0.3));
        assert!(source.active_voices() <= MAX_VOICES);
        assert!(
            out.iter().all(|s| s.is_finite() && s.abs() <= 4.0),
            "voice stealing must stay bounded"
        );
    }

    #[test]
    fn position_changes_are_smoothed() {
        let mut source = WavetableProcessor::new(SR);
        source.note_event(note_on(A4, 1.0));
        assert!((source.position() - 0.0).abs() < 1.0e-6);

        set(&mut source, ParamId::WavetablePosition, 1.0);
        let _ = render(&mut source, 1);
        let after_one_block = source.position();
        assert!(
            after_one_block > 0.0 && after_one_block < 0.5,
            "one block must not jump the position: {after_one_block}"
        );

        let _ = render(&mut source, blocks_for(0.3));
        assert!(
            source.position() > 0.95,
            "the ramp must arrive: {}",
            source.position()
        );
    }

    #[test]
    fn params_reject_non_finite() {
        let mut source = WavetableProcessor::new(SR);
        for id in [
            ParamId::WavetablePosition,
            ParamId::WavetableAttack,
            ParamId::WavetableRelease,
            ParamId::WavetableGain,
            ParamId::PitchOffset,
        ] {
            assert!(!source.set_param(id, f32::NAN), "{id:?} accepted NaN");
            assert!(!source.set_param(id, f32::INFINITY), "{id:?} accepted Inf");
        }
        assert_eq!(source.position_target(), 0.0);
    }

    #[test]
    fn extreme_parameters_stay_finite() {
        let mut source = WavetableProcessor::new(SR);
        set(&mut source, ParamId::WavetableGain, 2.0);
        set(&mut source, ParamId::WavetableAttack, 0.001);
        set(&mut source, ParamId::WavetableRelease, 5.0);
        set(&mut source, ParamId::WavetablePosition, 1.0);
        set(&mut source, ParamId::PitchOffset, 24.0);
        for note in [0u8, 60, 108, 127] {
            source.note_event(note_on(note, 1.0));
        }
        let out = render(&mut source, blocks_for(0.4));
        assert!(
            out.iter().all(|s| s.is_finite()),
            "extreme settings must stay finite"
        );
    }

    #[test]
    fn renders_are_reproducible() {
        let run = || {
            let mut source = WavetableProcessor::new(SR);
            set(&mut source, ParamId::WavetablePosition, SAW);
            source.note_event(note_on(A4, 0.8));
            let _ = render(&mut source, blocks_for(0.05));
            source.note_event(note_on(A4 + 7, 0.6));
            render(&mut source, blocks_for(0.15))
        };
        assert_eq!(run(), run(), "two instances must render identically");
    }

    #[test]
    fn constructor_tolerates_a_bad_sample_rate() {
        for rate in [f32::NAN, 0.0, -48_000.0, f32::INFINITY] {
            let mut source = WavetableProcessor::new(rate);
            source.note_event(note_on(A4, 1.0));
            let out = render(&mut source, 20);
            assert!(
                out.iter().all(|s| s.is_finite()),
                "sample rate {rate} produced non-finite output"
            );
        }
    }

    #[test]
    fn preview_follows_the_position() {
        let mut sine = [0.0f32; 256];
        preview_waveform(0.0, &mut sine);
        let mut square = [0.0f32; 256];
        preview_waveform(1.0, &mut square);
        assert!(
            (peak(&sine) - 1.0).abs() < 0.05,
            "sine peak {}",
            peak(&sine)
        );
        assert!(
            (peak(&square) - 1.0).abs() < 0.25,
            "square peak {}",
            peak(&square)
        );
        // A sine is symmetric about its midpoint; a square spends most of its
        // time at the extremes, which shows up as a much larger mean |value|.
        let mean_abs = |s: &[f32]| s.iter().map(|v| v.abs()).sum::<f32>() / s.len() as f32;
        assert!(
            mean_abs(&square) > mean_abs(&sine) * 1.2,
            "the preview must change shape with the position: {} vs {}",
            mean_abs(&square),
            mean_abs(&sine)
        );
        let mut nan = [0.0f32; 64];
        preview_waveform(f32::NAN, &mut nan);
        assert!(nan.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn the_bank_is_complete() {
        // Every frame/level pair exists and stays inside ±1.4 after
        // normalisation. The headroom is real physics, not slack: a square
        // normalised by its full-band peak (π/4) leaves its fundamental at
        // 4/π ≈ 1.27, so the narrow levels legitimately peak above 1 — their
        // RMS still stays below the wide levels'.
        let bank = super::bank();
        assert_eq!(bank.data.len(), FRAMES * MIP_LEVELS * super::TABLE_SIZE);
        for frame in 0..FRAMES {
            for level in 0..MIP_LEVELS {
                let table = bank.table(frame, level);
                assert!(
                    table.iter().all(|s| s.is_finite() && s.abs() <= 1.4),
                    "frame {frame} level {level} left the normalised range"
                );
            }
        }
    }
}
