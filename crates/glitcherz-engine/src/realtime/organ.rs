//! Drawbar organ (spec `.agents/spec/organ-instrument.md`).
//!
//! Nine sine partials per voice at fixed footages (16' … 1'), one decaying
//! percussion partial, and a plain A/R ramp on the key switch.
//!
//! Three properties drive the DSP choices:
//!
//! - **One phase accumulator, not nine.** Every drawbar ratio is an integer
//!   multiple of `0.5·f0` (the 16' rate), so the nine partials are
//!   `sin(TAU · k · p)` over a single `p`. Nine independent accumulators would
//!   drift apart, and a 16' / 8' pair that slowly beats is instantly audible
//!   (S2-D2).
//! - **Drawbars are a registration, not an envelope.** Levels are applied live
//!   to sounding voices; there is no filter and no ADSR — only enough ramp on
//!   the key switch to avoid a click (S2-D5).
//! - **Velocity is ignored.** A tonewheel key contact is a switch (S2-D6);
//!   loudness comes from the registration and `gain`. `velocity == 0` still
//!   means "no note", matching the other engines.
//!
//! Nothing allocates after [`OrganProcessor::new`]: nine `f32` per voice plus
//! one shared mono scratch.

use std::f32::consts::TAU;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::envelope::AdsrProcessor;
use crate::realtime::processor::Processor;
use crate::realtime::synth::note_to_frequency;

/// Voice budget, matching the other built-in instruments.
pub const MAX_VOICES: usize = 16;

/// Number of drawbars (16', 5⅓', 8', 4', 2⅔', 2', 1⅗', 1⅓', 1').
pub const DRAWBAR_COUNT: usize = 9;

/// Mono scratch ceiling, same role as the other generators'.
const SCRATCH_FRAMES: usize = 1024;

/// Phase multiplier of each drawbar relative to the **16'** base rate.
///
/// `0.5·f0` is the common unit: 16' is 1×, 5⅓' is 3×, 8' is 2× … 1' is 16×.
const DRAWBAR_K: [f32; DRAWBAR_COUNT] = [1.0, 3.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 16.0];

/// Percussion partial multipliers: 2nd harmonic is 4' (4×), 3rd is 2⅔' (6×).
const PERCUSSION_K_SECOND: f32 = 4.0;
const PERCUSSION_K_THIRD: f32 = 6.0;

/// Drawbar taper: 3 dB per step, level 8 = 0 dB, level 0 = off (S2-D3).
const LEVEL_GAIN: [f32; 9] = [0.0, 0.089, 0.126, 0.178, 0.251, 0.355, 0.501, 0.708, 1.0];

/// Sum of nine drawbars is scaled down so the default `888000000` registration
/// peaks at ≈ 0.75 per voice — the same order as the built-in synth's unity
/// oscillator (S2-D3).
const OUTPUT_SCALE: f32 = 0.25;

/// Pitch-offset bound in semitones, same as the other built-in sources.
const PITCH_OFFSET_LIMIT: f32 = 24.0;

/// One-pole time constant for `PitchOffset` (ms), copied from `osc.rs` so a
/// per-block automation curve arrives as a ramp (S2-D9).
const PITCH_SMOOTHING_MS: f32 = 5.0;

/// Blanket-stop fade (Stop / Panic), same as the other generators.
const BLANKET_SECONDS: f32 = 0.01;

/// `ln(1000)` — T60 → per-sample coefficient.
const LN_1000: f32 = 6.907_755_3;

/// Nyquist guard: a partial this close to `fs/2` is dropped (S2-D4).
const NYQUIST_LIMIT: f32 = 0.45;

/// Where a partial is allowed to start sounding, given the pitch it will play.
fn partial_allowed(f0: f32, ratio: f32, k: f32, sample_rate: f32) -> bool {
    0.5 * f0 * ratio * k < NYQUIST_LIMIT * sample_rate
}

/// Drawbar gain for one level (3 dB per step, `0` = off).
fn level_gain(level: u8) -> f32 {
    LEVEL_GAIN[(level as usize).min(LEVEL_GAIN.len() - 1)]
}

/// One organ voice: nine drawbars + percussion sharing a phase, one A/R ramp.
struct Voice {
    /// MIDI note this voice is playing; kept after release so the tail keeps
    /// rendering at the right pitch.
    note: Option<u8>,
    /// Whether the key is still down. Releasing clears this but not `note`.
    held: bool,
    /// Monotonic allocation stamp (steal = smallest).
    age: u64,
    env: AdsrProcessor,
    /// Phase of the **16'** partial, `0..1`.
    phase: f32,
    /// Phase increment of the 16' partial for this note (`0.5·f0/fs`).
    base_inc: f32,
    /// Current per-sample pitch multiplier, smoothed toward the target.
    pitch: f32,
    /// Per-drawbar gain and Nyquist mask, recomputed when the note or the
    /// tuning changes.
    bar_gain: [f32; DRAWBAR_COUNT],
    bar_on: [bool; DRAWBAR_COUNT],
    /// Percussion: frozen at note-on (S2-D7).
    perc_k: f32,
    perc_level: f32,
    perc_env: f32,
    perc_coeff: f32,
}

impl Voice {
    fn new(sample_rate: f32) -> Self {
        let mut env = AdsrProcessor::new(sample_rate);
        // A/R only: zero-length decay, full sustain (S2-D5).
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
            bar_gain: [0.0; DRAWBAR_COUNT],
            bar_on: [false; DRAWBAR_COUNT],
            perc_k: PERCUSSION_K_SECOND,
            perc_level: 0.0,
            perc_env: 0.0,
            perc_coeff: 1.0,
        }
    }

    /// A slot is free only after its release tail has fully decayed.
    fn is_free(&self) -> bool {
        self.env.is_idle()
    }
}

/// Default key-switch ramp (S2-D5).
const DEFAULT_ATTACK_S: f32 = 0.005;
const DEFAULT_RELEASE_S: f32 = 0.04;

/// Everything `configure` needs, bundled so the call sites cannot drift.
#[derive(Clone, Copy)]
struct Tuning {
    bars: [u8; DRAWBAR_COUNT],
    percussion: f32,
    percussion_third: bool,
    pitch_ratio: f32,
}

/// Recomputes a voice's drawbar gains, Nyquist mask and base increment.
///
/// Called on note-on and on every tuning change. Phase and envelope are left
/// alone: a stolen voice glides to the new note instead of restarting.
fn configure(voice: &mut Voice, tuning: &Tuning, sample_rate: f32) {
    let Some(note) = voice.note else {
        voice.bar_on = [false; DRAWBAR_COUNT];
        return;
    };
    let f0 = note_to_frequency(note);
    voice.base_inc = 0.5 * f0 / sample_rate;
    // The mask is judged with the *larger* of the current and target ratio so
    // a glide cannot alias on the way (S2-D4).
    let ratio = voice.pitch.max(tuning.pitch_ratio);
    for (index, k) in DRAWBAR_K.iter().enumerate() {
        let gain = level_gain(tuning.bars[index]);
        voice.bar_gain[index] = gain;
        voice.bar_on[index] = gain > 0.0 && partial_allowed(f0, ratio, *k, sample_rate);
    }
    let k = if tuning.percussion_third {
        PERCUSSION_K_THIRD
    } else {
        PERCUSSION_K_SECOND
    };
    voice.perc_k = k;
    voice.perc_level = if partial_allowed(f0, ratio, k, sample_rate) {
        tuning.percussion
    } else {
        0.0
    };
}

/// Polyphonic drawbar organ.
///
/// Note semantics (S2-D8):
/// - `On { note, velocity }`: a voice already playing `note` is **reused** (a
///   key is a key — no second voice, no +6 dB doubling), otherwise a free slot
///   takes the note, otherwise the oldest voice is stolen;
/// - `Off { note }` with `note != 0`: releases the voices playing that note;
/// - `Off { note: 0 }`: blanket release sentinel used by
///   `RtGraph::note_off_all`.
pub struct OrganProcessor {
    sample_rate: f32,
    voices: [Voice; MAX_VOICES],
    scratch: AudioBuffer,
    stamps: u64,
    tuning: Tuning,
    percussion_decay_s: f32,
    attack_s: f32,
    release_s: f32,
    gain: f32,
    pitch_ratio: f32,
    /// Set when a parameter that changes partial frequencies or masks moved.
    tables_dirty: bool,
}

impl OrganProcessor {
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
            voices: std::array::from_fn(|_| Voice::new(sample_rate)),
            scratch: AudioBuffer::new(1, SCRATCH_FRAMES),
            stamps: 0,
            tuning: Tuning {
                // 888000000 — the classic full registration.
                bars: [8, 8, 8, 0, 0, 0, 0, 0, 0],
                percussion: 0.0,
                percussion_third: false,
                pitch_ratio: 1.0,
            },
            percussion_decay_s: 0.6,
            attack_s: DEFAULT_ATTACK_S,
            release_s: DEFAULT_RELEASE_S,
            gain: 1.0,
            pitch_ratio: 1.0,
            tables_dirty: true,
        }
    }

    /// Number of voices that have not reached Idle yet. RT-safe.
    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|voice| !voice.is_free()).count()
    }

    /// First free slot, else the oldest one (S2-D8). RT-safe: fixed scan.
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
        let tuning = self.tuning;
        let sample_rate = self.sample_rate;
        for voice in &mut self.voices {
            if voice.is_free() {
                continue;
            }
            configure(voice, &tuning, sample_rate);
        }
    }

    fn trigger(&mut self, note: u8, velocity: f32) {
        let velocity = if velocity.is_finite() {
            velocity.clamp(0.0, 1.0)
        } else {
            0.0
        };
        // Velocity sets no level (S2-D6), but zero still means "no note".
        if velocity <= 0.0 {
            return;
        }
        let index = self
            .voices
            .iter()
            .position(|voice| voice.held && voice.note == Some(note))
            .unwrap_or_else(|| self.allocate());
        let age = self.next_stamp();
        let tuning = self.tuning;
        let sample_rate = self.sample_rate;
        // Frozen percussion settings (S2-D7).
        let decay = self.percussion_decay_s;
        let seconds = decay.max(0.001);
        let perc_coeff = (1.0 - (-LN_1000 / (seconds * sample_rate)).exp()).clamp(0.0, 1.0);

        let voice = &mut self.voices[index];
        voice.note = Some(note);
        voice.held = true;
        voice.age = age;
        voice.pitch = tuning.pitch_ratio;
        // The phase is deliberately kept: resetting it would step the
        // waveform while the ramped envelope is still above zero.
        voice.perc_env = if tuning.percussion > 0.0 { 1.0 } else { 0.0 };
        voice.perc_coeff = perc_coeff;
        configure(voice, &tuning, sample_rate);
        // `AdsrProcessor::trigger` ramps from the current level, so a reused or
        // stolen slot never clicks.
        voice.env.note_event(NoteEvent::On {
            note,
            velocity: 1.0,
        });
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
fn render_voice(voice: &mut Voice, out: &mut [f32], pitch_target: f32, pitch_coeff: f32) {
    let base_inc = voice.base_inc;
    let mut phase = voice.phase;
    let mut pitch = voice.pitch;
    let bars = voice.bar_gain;
    let on = voice.bar_on;
    let perc_k = voice.perc_k;
    let perc_level = voice.perc_level;
    let mut perc = voice.perc_env;
    let perc_coeff = voice.perc_coeff;

    for sample in out.iter_mut() {
        pitch += (pitch_target - pitch) * pitch_coeff;
        phase += base_inc * pitch;
        if phase >= 1.0 {
            phase -= phase.floor();
        }
        let mut acc = 0.0;
        for index in 0..DRAWBAR_COUNT {
            if on[index] {
                acc += bars[index] * (TAU * DRAWBAR_K[index] * phase).sin();
            }
        }
        if perc > 1.0e-6 {
            acc += perc * perc_level * (TAU * perc_k * phase).sin();
            perc -= perc * perc_coeff;
        }
        *sample = acc * OUTPUT_SCALE;
    }

    voice.phase = phase;
    voice.pitch = pitch;
    voice.perc_env = perc;
}

impl Processor for OrganProcessor {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        if frames == 0 || buffer.channels() == 0 {
            return;
        }
        // The organ is a source: it owns its output entirely.
        for ch in 0..buffer.channels() {
            if let Some(dst) = buffer.channel_mut(ch) {
                dst.fill(0.0);
            }
        }
        if self.tables_dirty {
            self.tables_dirty = false;
            self.retune_all();
        }

        let output_gain = self.gain;
        let pitch_target = self.pitch_ratio;
        let pitch_coeff = pitch_coefficient(self.sample_rate);
        let Self {
            voices, scratch, ..
        } = self;
        scratch.set_len(frames.min(SCRATCH_FRAMES));
        let rendered = scratch.len();
        for voice in voices.iter_mut() {
            if voice.is_free() {
                continue;
            }
            scratch.clear();
            if let Some(mono) = scratch.channel_mut(0) {
                render_voice(voice, mono, pitch_target, pitch_coeff);
            }
            // The key-switch ramp is the only envelope: the raw partial sum has
            // no attack, so applying it here is what stops every note from
            // clicking.
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
            ParamId::OrganBar(index) => {
                let slot = index as usize;
                if slot >= DRAWBAR_COUNT {
                    return false;
                }
                let level = value.clamp(0.0, 8.0).round() as u8;
                if self.tuning.bars[slot] != level {
                    self.tuning.bars[slot] = level;
                    self.tables_dirty = true;
                }
            }
            ParamId::OrganPercussion => self.tuning.percussion = value.clamp(0.0, 1.0),
            ParamId::OrganPercussionDecay => self.percussion_decay_s = value.clamp(0.1, 2.0),
            ParamId::OrganPercussionHarmonic => self.tuning.percussion_third = value >= 0.5,
            // The attack ramp is read live by the envelope, so editing it during
            // a held note takes effect on the next sample; `release` is sampled
            // when the key is let go.
            ParamId::OrganAttack => {
                self.attack_s = value.clamp(0.001, 0.2);
                for voice in &mut self.voices {
                    voice.env.set_param(ParamId::Attack, self.attack_s);
                }
            }
            ParamId::OrganRelease => {
                self.release_s = value.clamp(0.005, 1.0);
                for voice in &mut self.voices {
                    voice.env.set_param(ParamId::Release, self.release_s);
                }
            }
            ParamId::OrganGain => self.gain = value.clamp(0.0, 2.0),
            ParamId::PitchOffset => {
                let semitones = value.clamp(-PITCH_OFFSET_LIMIT, PITCH_OFFSET_LIMIT);
                self.pitch_ratio = (semitones / 12.0).exp2();
                self.tuning.pitch_ratio = self.pitch_ratio;
                // The Nyquist mask depends on the note's own frequency, which
                // the voices hold, so it has to be rebuilt per voice.
                self.tables_dirty = true;
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

    /// The organ has no CC surface; consuming the event keeps the graph quiet
    /// (same rationale as the drum kit / modal / pluck engines).
    fn midi_cc(&mut self, _channel: u8, _controller: u8, _value: u8) -> bool {
        true
    }
}

/// Per-sample one-pole coefficient for pitch smoothing (S2-D9).
fn pitch_coefficient(sample_rate: f32) -> f32 {
    let tau = PITCH_SMOOTHING_MS * 0.001;
    (1.0 - (-1.0 / (tau * sample_rate)).exp()).clamp(1.0e-4, 1.0)
}

#[cfg(test)]
mod tests {
    use super::{DRAWBAR_COUNT, MAX_VOICES, OrganProcessor};
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::{NoteEvent, ParamId};
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;
    /// A4 = 440 Hz.
    const A4: u8 = 69;
    /// C8 = 4186 Hz: every drawbar above 2' is past Nyquist there.
    const C8: u8 = 108;

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

    fn render(processor: &mut OrganProcessor, blocks: usize) -> Vec<f32> {
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

    fn set(processor: &mut OrganProcessor, id: ParamId, value: f32) {
        assert!(processor.set_param(id, value), "{id:?} rejected {value}");
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs()))
    }

    /// Turns every drawbar off, so a test can build its registration from zero.
    fn silence_bars(organ: &mut OrganProcessor) {
        for index in 0..DRAWBAR_COUNT {
            set(organ, ParamId::OrganBar(index as u8), 0.0);
        }
    }

    #[test]
    fn silent_without_a_note() {
        let mut organ = OrganProcessor::new(SR);
        let out = render(&mut organ, 40);
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(organ.active_voices(), 0);
    }

    #[test]
    fn a_held_note_produces_sound() {
        let mut organ = OrganProcessor::new(SR);
        organ.note_event(note_on(A4, 1.0));
        let out = render(&mut organ, blocks_for(0.5));
        let level = rms(window(&out, 0.05, 0.4));
        assert!(
            level > 0.05 && level < 0.6,
            "the default 888000000 registration should sit in the instrument range: {level}"
        );
        // The default attack is 5 ms: the first millisecond is still ramping.
        assert!(rms(window(&out, 0.02, 0.3)) > level * 0.9);
    }

    #[test]
    fn pitch_follows_the_note_and_the_offset() {
        let mut organ = OrganProcessor::new(SR);
        organ.note_event(note_on(A4, 1.0));
        let out = render(&mut organ, blocks_for(0.6));
        let span = window(&out, 0.1, 0.5);
        let centre = goertzel(span, 440.0);
        let below = goertzel(span, 415.30);
        let above = goertzel(span, 466.16);
        assert!(
            centre > below * 4.0 && centre > above * 4.0,
            "A4 must land on 440 Hz: centre {centre}, neighbours {below} / {above}"
        );

        // PitchOffset reaches sounding voices. Measured on a single 8' drawbar:
        // with the full registration an octave up just moves 16' onto the old
        // 8', which would hide the offset behind the registration.
        let mut shifted = OrganProcessor::new(SR);
        silence_bars(&mut shifted);
        set(&mut shifted, ParamId::OrganBar(2), 8.0);
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
    fn drawbars_set_the_harmonic_spectrum() {
        // Only 8' → the fundamental dominates and 2·f0 nearly vanishes.
        let mut unify = OrganProcessor::new(SR);
        silence_bars(&mut unify);
        set(&mut unify, ParamId::OrganBar(2), 8.0);
        unify.note_event(note_on(A4, 1.0));
        let out = render(&mut unify, blocks_for(0.5));
        let span = window(&out, 0.05, 0.4);
        let f0 = goertzel(span, 440.0);
        let f2 = goertzel(span, 880.0);
        assert!(
            f0 > f2 * 8.0,
            "8' alone must be nearly a sine: f0 {f0}, 2f0 {f2}"
        );

        // Only 4' → the octave takes over.
        let mut octave = OrganProcessor::new(SR);
        silence_bars(&mut octave);
        set(&mut octave, ParamId::OrganBar(3), 8.0);
        octave.note_event(note_on(A4, 1.0));
        let out = render(&mut octave, blocks_for(0.5));
        let span = window(&out, 0.05, 0.4);
        assert!(
            goertzel(span, 880.0) > goertzel(span, 440.0) * 8.0,
            "4' alone must put its energy an octave up"
        );
    }

    #[test]
    fn sixteen_foot_adds_the_sub_octave() {
        let mut organ = OrganProcessor::new(SR);
        silence_bars(&mut organ);
        set(&mut organ, ParamId::OrganBar(0), 8.0);
        organ.note_event(note_on(A4, 1.0));
        let out = render(&mut organ, blocks_for(0.5));
        let span = window(&out, 0.05, 0.4);
        assert!(
            goertzel(span, 220.0) > goertzel(span, 440.0) * 8.0,
            "16' is the sub-octave of the played note"
        );
    }

    #[test]
    fn level_taper_is_three_db_per_step() {
        let amplitude = |level: f32| {
            let mut organ = OrganProcessor::new(SR);
            silence_bars(&mut organ);
            set(&mut organ, ParamId::OrganBar(2), level);
            organ.note_event(note_on(A4, 1.0));
            let out = render(&mut organ, blocks_for(0.4));
            goertzel(window(&out, 0.05, 0.3), 440.0)
        };
        let full = amplitude(8.0);
        let half = amplitude(4.0);
        let ratio = half / full.max(1.0e-9);
        // 4 steps × 3 dB = 12 dB → 0.251.
        assert!(
            (0.21..0.30).contains(&ratio),
            "four drawbar steps should be 12 dB: {ratio}"
        );
    }

    #[test]
    fn all_drawbars_down_is_silent() {
        let mut organ = OrganProcessor::new(SR);
        silence_bars(&mut organ);
        organ.note_event(note_on(A4, 1.0));
        let out = render(&mut organ, blocks_for(0.3));
        assert!(
            out.iter().all(|s| *s == 0.0),
            "a registration with every drawbar down makes no sound"
        );
        // The voice still exists until it is released — silence is not the same
        // as a finished note.
        assert_eq!(organ.active_voices(), 1);
    }

    #[test]
    fn attack_and_release_shape_the_envelope() {
        let mut organ = OrganProcessor::new(SR);
        set(&mut organ, ParamId::OrganAttack, 0.05);
        organ.note_event(note_on(A4, 1.0));
        let out = render(&mut organ, blocks_for(0.2));
        let early = rms(window(&out, 0.0, 0.01));
        let full = rms(window(&out, 0.06, 0.15));
        assert!(
            early < full * 0.3,
            "a 50 ms attack must still be ramping at 10 ms: {early} vs {full}"
        );

        set(&mut organ, ParamId::OrganRelease, 0.05);
        organ.note_event(NoteEvent::Off { note: A4 });
        let tail = render(&mut organ, blocks_for(0.2));
        assert!(
            rms(window(&tail, 0.08, 0.15)) < full * 0.02,
            "note-off must reach silence inside the release time"
        );
    }

    #[test]
    fn blanket_off_stops_everything() {
        let mut organ = OrganProcessor::new(SR);
        // A long release would keep the key audible; the sentinel must not.
        set(&mut organ, ParamId::OrganRelease, 1.0);
        organ.note_event(note_on(A4, 1.0));
        let _ = render(&mut organ, blocks_for(0.1));
        organ.note_event(NoteEvent::Off { note: 0 });
        let out = render(&mut organ, blocks_for(0.2));
        assert!(
            rms(window(&out, 0.05, 0.2)) < 1.0e-6,
            "Off{{0}} is a blanket stop with a fixed 10 ms ramp"
        );
        assert_eq!(organ.active_voices(), 0);
    }

    #[test]
    fn percussion_adds_a_decaying_harmonic() {
        let build = |level: f32| {
            let mut organ = OrganProcessor::new(SR);
            silence_bars(&mut organ);
            set(&mut organ, ParamId::OrganBar(2), 8.0);
            set(&mut organ, ParamId::OrganPercussion, level);
            set(&mut organ, ParamId::OrganPercussionDecay, 0.3);
            organ.note_event(note_on(A4, 1.0));
            let out = render(&mut organ, blocks_for(1.0));
            (
                goertzel(window(&out, 0.0, 0.05), 880.0),
                goertzel(window(&out, 0.7, 0.95), 880.0),
                out,
            )
        };
        let (early, late, _) = build(1.0);
        assert!(
            early > late * 5.0,
            "the 2nd-harmonic percussion must decay: {early} → {late}"
        );
        let (silent_early, _, out) = build(0.0);
        assert!(
            early > silent_early * 5.0,
            "without percussion there is no early 2nd harmonic: {early} vs {silent_early}"
        );
        assert!(peak(&out) < 1.0);
    }

    #[test]
    fn percussion_harmonic_switches_the_peak() {
        let ratio = |third: f32| {
            let mut organ = OrganProcessor::new(SR);
            silence_bars(&mut organ);
            set(&mut organ, ParamId::OrganBar(2), 8.0);
            set(&mut organ, ParamId::OrganPercussion, 1.0);
            set(&mut organ, ParamId::OrganPercussionHarmonic, third);
            organ.note_event(note_on(A4, 1.0));
            let out = render(&mut organ, blocks_for(0.2));
            let span = window(&out, 0.0, 0.05);
            goertzel(span, 1320.0) / goertzel(span, 880.0).max(1.0e-9)
        };
        let second = ratio(0.0);
        let third = ratio(1.0);
        assert!(
            second < 0.2,
            "the 2nd-harmonic setting must not excite 3·f0: {second}"
        );
        assert!(
            third > 2.0,
            "the 3rd-harmonic setting must put the percussion on 3·f0: {third}"
        );
    }

    #[test]
    fn retrigger_reuses_the_voice() {
        let mut organ = OrganProcessor::new(SR);
        organ.note_event(note_on(A4, 1.0));
        let _ = render(&mut organ, 2);
        organ.note_event(note_on(A4, 1.0));
        let out = render(&mut organ, blocks_for(0.2));
        assert_eq!(
            organ.active_voices(),
            1,
            "a repeated key is the same key, not a second voice"
        );
        assert!(peak(&out) < 1.0, "re-triggering must not double the level");
    }

    #[test]
    fn pool_is_bounded_and_stealing_stays_finite() {
        let mut organ = OrganProcessor::new(SR);
        for index in 0..(MAX_VOICES as u8 + 1) {
            organ.note_event(note_on(40 + index, 1.0));
        }
        let out = render(&mut organ, blocks_for(0.5));
        assert!(organ.active_voices() <= MAX_VOICES);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(peak(&out) < 20.0, "16 voices stacked: {}", peak(&out));
    }

    #[test]
    fn upper_drawbars_stop_above_nyquist() {
        let mut organ = OrganProcessor::new(SR);
        silence_bars(&mut organ);
        set(&mut organ, ParamId::OrganBar(8), 8.0); // 1' = 16× the 16' rate
        organ.note_event(note_on(C8, 1.0));
        let out = render(&mut organ, blocks_for(0.2));
        assert!(
            out.iter().all(|s| *s == 0.0),
            "16·f0 at C8 is past Nyquist and must be masked out"
        );
    }

    #[test]
    fn params_reject_non_finite_and_bad_indices() {
        let mut organ = OrganProcessor::new(SR);
        assert!(!organ.set_param(ParamId::OrganGain, f32::NAN));
        assert!(!organ.set_param(ParamId::OrganAttack, f32::INFINITY));
        assert!(!organ.set_param(ParamId::OrganBar(0), f32::NAN));
        assert!(
            !organ.set_param(ParamId::OrganBar(DRAWBAR_COUNT as u8), 4.0),
            "an out-of-range drawbar index must be refused"
        );
        assert!(organ.set_param(ParamId::OrganBar(2), 3.4));
        assert!(
            organ.set_param(ParamId::OrganGain, 99.0),
            "clamped, not rejected"
        );
    }

    #[test]
    fn extreme_parameters_stay_finite() {
        let mut organ = OrganProcessor::new(SR);
        for index in 0..DRAWBAR_COUNT {
            set(&mut organ, ParamId::OrganBar(index as u8), 8.0);
        }
        set(&mut organ, ParamId::OrganGain, 2.0);
        set(&mut organ, ParamId::OrganAttack, 0.001);
        set(&mut organ, ParamId::OrganPercussion, 1.0);
        set(&mut organ, ParamId::PitchOffset, 24.0);
        organ.note_event(note_on(0, 1.0));
        organ.note_event(note_on(127, 1.0));
        let out = render(&mut organ, blocks_for(0.5));
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn zero_velocity_is_silent() {
        let mut organ = OrganProcessor::new(SR);
        organ.note_event(note_on(A4, 0.0));
        let out = render(&mut organ, blocks_for(0.2));
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(organ.active_voices(), 0);
    }

    #[test]
    fn renders_are_reproducible() {
        let run = || {
            let mut organ = OrganProcessor::new(SR);
            organ.note_event(note_on(A4, 1.0));
            organ.note_event(note_on(A4 + 7, 1.0));
            let mut out = render(&mut organ, blocks_for(0.3));
            organ.note_event(NoteEvent::Off { note: A4 });
            out.extend(render(&mut organ, blocks_for(0.2)));
            out
        };
        let a = run();
        let b = run();
        assert_eq!(a.len(), b.len());
        assert!(a.iter().zip(b.iter()).all(|(x, y)| x == y));
    }

    #[test]
    fn constructor_tolerates_a_bad_sample_rate() {
        for rate in [0.0, -48_000.0, f32::NAN] {
            let mut organ = OrganProcessor::new(rate);
            organ.note_event(note_on(A4, 1.0));
            let out = render(&mut organ, 8);
            assert!(out.iter().all(|s| s.is_finite()));
        }
    }
}
