//! Real-time multi-sample drum kit (spec `.agents/spec/drum-kit.md` K1–K9).
//!
//! One node owns a 128-entry **key → sample** table and a fixed voice pool.
//! Unlike [`SampleVoiceProcessor`](crate::realtime::sample::SampleVoiceProcessor),
//! the key does **not** pitch-shift the sample: it selects which wav to play
//! (K5). Every hit is one-shot (K6); `choke` groups fade older hits when a
//! new one lands (K7).
//!
//! RT safety: the table (one `Arc<SampleData>` per key), the voice pool and
//! the stereo scratch are allocated once in [`DrumKitProcessor::new`] on the
//! control thread. `process` / `note_event` / `set_param` only do plain
//! arithmetic over pre-allocated memory; finished voices are recycled with an
//! in-place swap (no `Vec::remove`).

use std::sync::Arc;

use crate::realtime::FIXED_BLOCK_SIZE;
use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;
use crate::sample::SampleData;

/// Document capacity of one kit (mirrors the model constant).
pub const DRUM_MAX_SLOTS: usize = 32;

/// Voice budget: a busy kit (choked hats, overlapping toms) needs more
/// overlap than a melodic sampler.
pub const DRUM_POLYPHONY: usize = 24;

/// Release fade length in seconds — shared by choke and the blanket stop.
const RELEASE_SECONDS: f32 = 0.01;

/// Pitch trim bound (semitones) for a slot.
const TUNE_LIMIT: f32 = 24.0;

/// Pitch-offset bound (semitones), same as the other built-in sources.
const PITCH_OFFSET_LIMIT: f32 = 24.0;

/// One slot handed to the engine (already decoded on the control thread).
#[derive(Debug, Clone)]
pub struct DrumSlotSpec {
    pub note: u8,
    pub data: Arc<SampleData>,
    pub gain: f32,
    pub pan: f32,
    pub tune_semitones: f32,
    pub choke: Option<u8>,
}

/// Live slot state. `data` is never replaced on the audio thread; the
/// mutable fields are plain scalars written by `set_param`.
#[derive(Debug, Clone)]
struct Slot {
    data: Arc<SampleData>,
    gain: f32,
    pan: f32,
    /// `2^(tune/12) · wav_rate / engine_rate`, frozen into each new voice.
    base_rate: f64,
    /// `-1` = no choke group.
    choke: i16,
}

#[derive(Debug, Clone, Copy)]
struct Voice {
    active: bool,
    /// Trigger key — also the table index for live gain/pan reads.
    note: u8,
    age: u32,
    pos: f64,
    /// Trigger-time step factor (slot tune + sample-rate compensation).
    rate: f64,
    /// Trigger-time velocity scale (0..1).
    vel: f32,
    /// Choke group frozen at trigger (K7).
    choke: i16,
    releasing: bool,
    fade_pos: f32,
}

impl Voice {
    fn idle() -> Self {
        Self {
            active: false,
            note: 0,
            age: 0,
            pos: 0.0,
            rate: 0.0,
            vel: 0.0,
            choke: -1,
            releasing: false,
            fade_pos: 0.0,
        }
    }
}

/// Multi-sample drum machine (generator semantics: the block is overwritten).
#[derive(Debug)]
pub struct DrumKitProcessor {
    slots: Box<[Option<Slot>]>,
    voices: Vec<Voice>,
    active_count: usize,
    held: u128,
    gain: f32,
    engine_rate: f64,
    release_frames: f32,
    /// Block-level pitch multiplier from `PitchOffset` (K8); a bend curve
    /// reaches sounding voices at block granularity.
    pitch_ratio: f32,
    left: Box<[f32]>,
    right: Box<[f32]>,
}

impl DrumKitProcessor {
    /// Builds the kit. **Control thread only** (table / pool / scratch
    /// allocation). A duplicate key keeps the last spec (the document's
    /// validator rejects duplicates first); slots past the cap are ignored.
    pub fn new(specs: &[DrumSlotSpec], sample_rate: f32) -> Self {
        let rate = if sample_rate.is_finite() && sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        let mut slots: Vec<Option<Slot>> = (0..128).map(|_| None).collect();
        for spec in specs.iter().take(DRUM_MAX_SLOTS) {
            let tune = if spec.tune_semitones.is_finite() {
                spec.tune_semitones.clamp(-TUNE_LIMIT, TUNE_LIMIT)
            } else {
                0.0
            };
            let gain = if spec.gain.is_finite() {
                spec.gain.clamp(0.0, 2.0)
            } else {
                1.0
            };
            let pan = if spec.pan.is_finite() {
                spec.pan.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            let base_rate = f64::from(2.0f32.powf(tune / 12.0))
                * f64::from(spec.data.sample_rate())
                / f64::from(rate);
            slots[usize::from(spec.note & 0x7F)] = Some(Slot {
                data: Arc::clone(&spec.data),
                gain,
                pan,
                base_rate,
                choke: spec.choke.map_or(-1, i16::from),
            });
        }
        Self {
            slots: slots.into_boxed_slice(),
            voices: vec![Voice::idle(); DRUM_POLYPHONY],
            active_count: 0,
            held: 0,
            gain: 1.0,
            engine_rate: f64::from(rate),
            release_frames: (RELEASE_SECONDS * rate).max(1.0),
            pitch_ratio: 1.0,
            left: vec![0.0; FIXED_BLOCK_SIZE].into_boxed_slice(),
            right: vec![0.0; FIXED_BLOCK_SIZE].into_boxed_slice(),
        }
    }

    /// Number of loaded slots (diagnostics / tests).
    pub fn slot_count(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    /// Spawns one voice for `note`, or returns silently when the key is
    /// unmapped (a drum kit is a lookup table, not a synthesizer).
    fn spawn(&mut self, note: u8, vel: f32) {
        let Some(slot) = self
            .slots
            .get(usize::from(note))
            .and_then(|slot| slot.as_ref())
        else {
            return;
        };
        let (rate, choke) = (slot.base_rate, slot.choke);
        if choke >= 0 {
            self.choke_group(choke);
        }
        let voice = Voice {
            active: true,
            note,
            age: 0,
            pos: 0.0,
            rate,
            vel,
            choke,
            releasing: false,
            fade_pos: 0.0,
        };
        if self.active_count < DRUM_POLYPHONY {
            self.voices[self.active_count] = voice;
            self.active_count += 1;
        } else {
            // Steal the oldest voice (ties resolve to the lowest index).
            let mut victim = 0;
            let mut best_age = self.voices[0].age;
            for i in 1..self.active_count {
                if self.voices[i].age > best_age {
                    best_age = self.voices[i].age;
                    victim = i;
                }
            }
            self.voices[victim] = voice;
        }
    }

    /// Fades every sounding voice of `group` (K7).
    fn choke_group(&mut self, group: i16) {
        for i in 0..self.active_count {
            let voice = &mut self.voices[i];
            if voice.active && voice.choke == group && !voice.releasing {
                voice.releasing = true;
                voice.fade_pos = 0.0;
            }
        }
    }

    /// Blanket stop: every sounding voice fades (Stop / Panic, K6).
    fn release_all(&mut self) {
        for i in 0..self.active_count {
            let voice = &mut self.voices[i];
            if voice.active && !voice.releasing {
                voice.releasing = true;
                voice.fade_pos = 0.0;
            }
        }
    }
}

impl Processor for DrumKitProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len().min(self.left.len());
        self.left[..frames].fill(0.0);
        self.right[..frames].fill(0.0);

        for i in 0..self.active_count {
            render_voice(
                &mut self.voices[i],
                &self.slots,
                frames,
                self.release_frames,
                self.gain,
                self.pitch_ratio,
                &mut self.left[..frames],
                &mut self.right[..frames],
            );
        }

        let mut i = 0;
        while i < self.active_count {
            if self.voices[i].active {
                self.voices[i].age += 1;
                i += 1;
            } else {
                self.active_count -= 1;
                self.voices.swap(i, self.active_count);
            }
        }

        if let Some(dst) = buffer.channel_mut(0) {
            let n = dst.len().min(frames);
            dst[..n].copy_from_slice(&self.left[..n]);
        }
        if let Some(dst) = buffer.channel_mut(1) {
            let n = dst.len().min(frames);
            dst[..n].copy_from_slice(&self.right[..n]);
        }
    }

    /// One-shot semantics (K6): a pitch-matched note-off is ignored; the
    /// blanket sweep (`Off { note: 0 }` with key 0 not held) always stops the
    /// node. RT-safe: bitmap and fixed loops only.
    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { note, velocity } => {
                let vel = if velocity.is_finite() {
                    velocity.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if note < 128 {
                    if vel <= 0.0 {
                        self.held &= !(1u128 << note);
                    } else {
                        self.held |= 1u128 << note;
                    }
                }
                if vel > 0.0 {
                    self.spawn(note, vel);
                }
            }
            NoteEvent::Off { note } => {
                if note == 0 && self.held & 1 == 0 {
                    self.held = 0;
                    self.release_all();
                } else if note < 128 {
                    self.held &= !(1u128 << note);
                }
            }
        }
        true
    }

    /// A drum kit has no CC surface; consuming the event keeps the graph
    /// quiet (returning `false` would warn on every automation block).
    fn midi_cc(&mut self, _channel: u8, _controller: u8, _value: u8) -> bool {
        true
    }

    /// RT-safe: plain scalar writes.
    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::DrumGain => {
                if !value.is_finite() {
                    return false;
                }
                self.gain = value.clamp(0.0, 2.0);
            }
            ParamId::PitchOffset => {
                if !value.is_finite() {
                    return false;
                }
                let semitones = value.clamp(-PITCH_OFFSET_LIMIT, PITCH_OFFSET_LIMIT);
                self.pitch_ratio = (semitones / 12.0).exp2();
            }
            ParamId::DrumSlotGain(note) => {
                if !value.is_finite() {
                    return false;
                }
                if let Some(slot) = self.slots.get_mut(usize::from(note & 0x7F))
                    && let Some(slot) = slot.as_mut()
                {
                    slot.gain = value.clamp(0.0, 2.0);
                }
            }
            ParamId::DrumSlotPan(note) => {
                if !value.is_finite() {
                    return false;
                }
                if let Some(slot) = self.slots.get_mut(usize::from(note & 0x7F))
                    && let Some(slot) = slot.as_mut()
                {
                    slot.pan = value.clamp(-1.0, 1.0);
                }
            }
            ParamId::DrumSlotTune(note) => {
                if !value.is_finite() {
                    return false;
                }
                let tune = value.clamp(-TUNE_LIMIT, TUNE_LIMIT);
                if let Some(slot) = self.slots.get_mut(usize::from(note & 0x7F))
                    && let Some(slot) = slot.as_mut()
                {
                    slot.base_rate = f64::from(2.0f32.powf(tune / 12.0))
                        * f64::from(slot.data.sample_rate())
                        / self.engine_rate;
                }
            }
            ParamId::DrumSlotChoke(note) => {
                if !value.is_finite() {
                    return false;
                }
                let group = if value < 0.0 {
                    -1
                } else {
                    value.round().clamp(0.0, 127.0) as i16
                };
                if let Some(slot) = self.slots.get_mut(usize::from(note & 0x7F))
                    && let Some(slot) = slot.as_mut()
                {
                    slot.choke = group;
                }
            }
            _ => return false,
        }
        true
    }
}

/// Equal-power pan gains for `pan` in -1..=1.
fn pan_gains(pan: f32) -> (f32, f32) {
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (angle.cos(), angle.sin())
}

/// Renders one voice. Gain and pan are read **per block** from the live slot
/// table, so slider moves apply to sounding hits (K8); the step rate stays
/// frozen at trigger time (except the block-level `PitchOffset`).
// RT hot loop: splitting the arguments into a struct would allocate or force
// borrow gymnastics for no benefit.
#[allow(clippy::too_many_arguments)]
fn render_voice(
    voice: &mut Voice,
    slots: &[Option<Slot>],
    frames: usize,
    release_frames: f32,
    master_gain: f32,
    pitch_ratio: f32,
    out_l: &mut [f32],
    out_r: &mut [f32],
) {
    let Some(slot) = slots
        .get(usize::from(voice.note))
        .and_then(|slot| slot.as_ref())
    else {
        voice.active = false;
        return;
    };
    let src = slot.data.frames();
    let last = src.len().saturating_sub(1);
    if last == 0 {
        voice.active = false;
        return;
    }
    let (pan_l, pan_r) = pan_gains(slot.pan);
    let g_l = master_gain * slot.gain * voice.vel * pan_l;
    let g_r = master_gain * slot.gain * voice.vel * pan_r;
    let rate = voice.rate * f64::from(pitch_ratio);
    for f in 0..frames {
        if !voice.active || voice.pos >= last as f64 {
            voice.active = false;
            break;
        }
        let mut fade = 1.0;
        if voice.releasing {
            fade = 1.0 - voice.fade_pos / release_frames;
            voice.fade_pos += 1.0;
            if voice.fade_pos >= release_frames {
                voice.active = false;
            }
        }
        let i = voice.pos as usize;
        let frac = (voice.pos - i as f64) as f32;
        let a = src[i];
        let b = src[i + 1];
        out_l[f] += (a[0] + (b[0] - a[0]) * frac) * g_l * fade;
        out_r[f] += (a[1] + (b[1] - a[1]) * frac) * g_r * fade;
        voice.pos += rate;
    }
    if voice.pos >= last as f64 {
        voice.active = false;
    }
}
