//! Real-time sample-playback voice processor (spec
//! `.agents/spec/sample-player.md` SP2 / D3–D5).
//!
//! [`SampleVoiceProcessor`] renders an already-decoded
//! [`SampleData`](crate::sample::SampleData) (f32 stereo frames, built on
//! the control thread) behind the [`Processor`] trait, so a wav file can be
//! installed as a graph-node instrument via the usual voice-swap path.
//!
//! RT safety: every method that can run on the audio callback — `process`,
//! `note_event`, `set_param`, `midi_cc` — performs only plain field writes
//! and reads into pre-allocated memory. The fixed-length voice pool
//! ([`SAMPLE_POLYPHONY`]) and the stereo render scratch are allocated in
//! [`SampleVoiceProcessor::new`] on the control thread; finished voices are
//! recycled with an in-place swap (no `Vec::remove`, no reallocation).
//!
//! Playback math (D3): the per-engine-frame step through the source frames
//! folds pitch ratio and sample-rate compensation into one factor —
//! `rate = 2^((note − root)/12) · wav_rate / engine_rate` — so no separate
//! resampling pass ever runs. Source reads use linear interpolation.

use std::sync::Arc;

use crate::realtime::FIXED_BLOCK_SIZE;
use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::smoothed::coeff_for;
use crate::realtime::processor::Processor;
use crate::sample::SampleData;

/// Voice budget of one sample node. 16 matches a small drum kit's natural
/// overlap; the pool is fixed-length and pre-allocated.
pub const SAMPLE_POLYPHONY: usize = 16;

/// Default root note of a sample (MIDI 60 = C4, matching the engine's
/// `note 69 = A4` convention).
pub const SAMPLE_DEFAULT_ROOT: u8 = 60;

/// Release fade length in seconds (D2): just enough to avoid a hard-cut
/// click; the sample itself is the timbre.
const RELEASE_SECONDS: f32 = 0.01;

/// One-pole time constant for pitch changes (ms), same value as the sine
/// source so the two built-in instruments glide alike (P1/D11).
const PITCH_SMOOTHING_MS: f32 = 5.0;

/// Pitch-offset span accepted by [`ParamId::PitchOffset`] (semitones).
const PITCH_OFFSET_LIMIT: f32 = 24.0;

/// One playing instance of the sample.
///
/// `rate` and `one_shot` are **frozen at trigger time** (D4): changing the
/// node's root note or play mode mid-flight only affects voices spawned
/// afterwards, which is the sampler-standard behaviour. The play window
/// (D12) is frozen the same way — a loop point that moved under a sounding
/// voice would jump its cursor.
#[derive(Debug, Clone, Copy)]
struct Voice {
    /// Slot is occupied by a playing (or releasing) voice.
    active: bool,
    /// The MIDI note that spawned this voice (pitch-matched release).
    note: u8,
    /// Blocks this voice has been alive (steal priority; the oldest loses).
    age: u32,
    /// Read cursor in source frames (fractional — linear interpolation).
    pos: f64,
    /// Source frames advanced per engine frame (pitch × rate compensation).
    rate: f64,
    /// Trigger-time velocity gain (0..1 linear).
    vel_gain: f32,
    /// In the release fade (D2).
    releasing: bool,
    /// Fade progress in frames (0..release_frames).
    fade_pos: f32,
    /// Engine frames still to render before the fade starts (D15: the tail
    /// plays out first, and only its last ~10 ms fade).
    fade_wait: f32,
    /// Play mode frozen at trigger (D1).
    one_shot: bool,
    /// Exclusive end of the window, in source frames.
    end: f64,
    /// Loop window, in source frames (`loop_end` = start of the tail).
    loop_start: f64,
    loop_end: f64,
    /// The loop is live for this voice: mode != Off and not OneShot. Cleared
    /// when a release jumps to the tail, so the tail never wraps.
    wrapping: bool,
    /// `PingPong` instead of `Forward` (frozen with the rest of the window).
    pingpong: bool,
    /// Cursor direction: `1.0` forward, `-1.0` reversed (ping-pong only).
    dir: f64,
}

impl Voice {
    fn idle() -> Self {
        Self {
            active: false,
            note: 0,
            age: 0,
            pos: 0.0,
            rate: 0.0,
            vel_gain: 0.0,
            releasing: false,
            fade_pos: 0.0,
            fade_wait: 0.0,
            one_shot: false,
            end: 0.0,
            loop_start: 0.0,
            loop_end: 0.0,
            wrapping: false,
            pingpong: false,
            dir: 1.0,
        }
    }
}

/// Loop mode values carried by [`ParamId::SampleLoopMode`]: `0` = off,
/// `1` = forward (the renderer's default when a loop exists and is not
/// ping-pong), `2` = ping-pong.
const LOOP_OFF: u8 = 0;
const LOOP_PINGPONG: u8 = 2;

/// Play window resolved against the loaded sample, in source frames (D12).
///
/// The document-side twin is `glitcherz_model::SampleRegion::resolve`; the
/// engine crate stays document-free, so the clamping rule lives in both
/// places — the model's copy drives the waveform handles, this one drives the
/// renderer, and both must agree on "0 means the end".
#[derive(Debug, Clone, Copy)]
struct Region {
    start: f64,
    loop_start: f64,
    loop_end: f64,
    end: f64,
    mode: u8,
}

impl Region {
    /// A loop needs a mode **and** a window wider than one frame.
    fn loops(&self) -> bool {
        self.mode != LOOP_OFF && self.loop_end - self.loop_start > 1.0
    }
}

/// The raw parameter values, kept as integers so a re-resolve never drifts.
#[derive(Debug, Clone, Copy, Default)]
struct RawRegion {
    start: u32,
    loop_start: u32,
    loop_end: u32,
    end: u32,
    mode: u8,
}

/// Polyphonic sample player: fixed voice pool + interpolated reads.
///
/// Note tracking mirrors the SF2 processor: the transport sweep releases
/// held notes with a blanket `NoteEvent::Off { note: 0 }`, so a `held`
/// bitset (one bit per MIDI key) lets `Off { note: 0 }` mean "release
/// everything" — except while key 0 itself is held, which stays
/// pitch-matched. The blanket sweep stops OneShot voices too (D5: Stop /
/// Panic must always silence the node); only a pitch-matched note-off is
/// ignored by OneShot.
#[derive(Debug)]
pub struct SampleVoiceProcessor {
    data: Arc<SampleData>,
    voices: Vec<Voice>,
    /// Voices `[0, active_count)` are playing; finished ones are swapped
    /// out (swap-compact pool, same recycling shape as the SF2
    /// VoiceCollection).
    active_count: usize,
    held: u128,
    root_note: u8,
    one_shot: bool,
    gain: f32,
    engine_rate: f64,
    release_frames: f32,
    /// Document values of the play window (D12), and their resolution
    /// against the loaded sample (recomputed on every region write, never
    /// per block).
    raw_region: RawRegion,
    region: Region,
    left: Box<[f32]>,
    right: Box<[f32]>,
    /// Current pitch multiplier (`2^(offset/12)`), advanced **per frame**
    /// (P1/D11). `1.0` = the note's own rate.
    pitch_ratio: f32,
    /// Where the multiplier is heading (one curve sample per block).
    pitch_target: f32,
    /// Per-frame one-pole coefficient.
    pitch_coeff: f32,
    /// Per-frame multipliers for the current block. Computed once and shared
    /// by every voice, so a 16-voice chord still glides as one unit.
    pitch_ramp: Box<[f32]>,
}

impl SampleVoiceProcessor {
    /// Builds the voice for an already-decoded sample. **Control thread
    /// only** (voice-pool + scratch allocation).
    ///
    /// A non-finite or non-positive sample rate falls back to 48 kHz
    /// (construction-time tolerance, same as `SynthProcessor`).
    pub fn new(data: Arc<SampleData>, sample_rate: f32) -> Self {
        let rate = if sample_rate.is_finite() && sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        let raw_region = RawRegion::default();
        let region = resolve_region(raw_region, data.frames().len());
        Self {
            data,
            voices: vec![Voice::idle(); SAMPLE_POLYPHONY],
            active_count: 0,
            held: 0,
            root_note: SAMPLE_DEFAULT_ROOT,
            one_shot: false,
            gain: 1.0,
            engine_rate: f64::from(rate),
            release_frames: (RELEASE_SECONDS * rate).max(1.0),
            raw_region,
            region,
            left: vec![0.0; FIXED_BLOCK_SIZE].into_boxed_slice(),
            right: vec![0.0; FIXED_BLOCK_SIZE].into_boxed_slice(),
            pitch_ratio: 1.0,
            pitch_target: 1.0,
            pitch_coeff: coeff_for(PITCH_SMOOTHING_MS, rate),
            pitch_ramp: vec![1.0; FIXED_BLOCK_SIZE].into_boxed_slice(),
        }
    }

    /// The sample this processor renders (diagnostics / tests).
    pub fn sample(&self) -> &SampleData {
        &self.data
    }

    /// Step factor for a note (D3): pitch ratio × sample-rate compensation.
    fn rate_for(&self, note: u8) -> f64 {
        let semitones = f64::from(note) - f64::from(self.root_note);
        2.0f64.powf(semitones / 12.0) * f64::from(self.data.sample_rate()) / self.engine_rate
    }

    /// Spawns a voice; a full pool steals the oldest active slot (D4).
    fn spawn(&mut self, note: u8, vel_gain: f32) {
        let region = self.region;
        let voice = Voice {
            active: true,
            note,
            age: 0,
            pos: region.start,
            rate: self.rate_for(note),
            vel_gain,
            releasing: false,
            fade_pos: 0.0,
            fade_wait: 0.0,
            one_shot: self.one_shot,
            end: region.end,
            loop_start: region.loop_start,
            loop_end: region.loop_end,
            // D14: one-shot voices run the window once (head → loop → tail),
            // so a drum hit cannot start cycling forever; only a held Gate
            // note keeps the loop alive.
            wrapping: region.loops() && !self.one_shot,
            pingpong: region.mode == LOOP_PINGPONG,
            dir: 1.0,
        };
        if self.active_count < SAMPLE_POLYPHONY {
            self.voices[self.active_count] = voice;
            self.active_count += 1;
        } else {
            // Pool full: steal the voice with the largest age (the oldest
            // sound). Ties resolve to the lowest index deterministically.
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

    /// Pitch-matched release (D1): enters Gate voices into the fade and
    /// leaves OneShot voices running. A voice that was looping leaves the
    /// loop for the **tail** first (D15) and only fades over the tail's last
    /// ~10 ms, so the tail is actually heard.
    fn release_matching(&mut self, note: u8) {
        let release_frames = self.release_frames;
        let pitch = self.pitch_ratio.max(0.01);
        for i in 0..self.active_count {
            let voice = &mut self.voices[i];
            if voice.active && voice.note == note && !voice.one_shot && !voice.releasing {
                voice.releasing = true;
                voice.fade_pos = 0.0;
                voice.fade_wait = 0.0;
                let tail = voice.end - voice.loop_end;
                // Only a voice that has actually reached the loop gets the
                // tail treatment; a release inside the head keeps its
                // ordinary 10 ms fade (jumping forward would cut the attack).
                if voice.wrapping && tail > 1.0 && voice.pos >= voice.loop_start {
                    // Jump to the tail start and stop wrapping. The fade is
                    // deferred by however long the tail runs (in engine
                    // frames) minus its own length: a tail shorter than the
                    // fade just starts fading at once.
                    voice.pos = voice.loop_end;
                    voice.wrapping = false;
                    let rate = (voice.rate * f64::from(pitch)).max(1e-9);
                    let engine_frames = (tail / rate).min(f64::from(f32::MAX)) as f32;
                    voice.fade_wait = (engine_frames - release_frames).max(0.0);
                }
            }
        }
    }

    /// Blanket release (D5): every sounding voice enters the fade, OneShot
    /// included — Stop / Panic must always silence the node.
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

impl Processor for SampleVoiceProcessor {
    /// Renders one block into the node buffer (generator semantics: the
    /// output overwrites the buffer). RT-safe: fixed loops over
    /// pre-allocated memory, plain float arithmetic.
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len().min(self.left.len());
        self.left[..frames].fill(0.0);
        self.right[..frames].fill(0.0);

        // P1: one shared pitch ramp per block (see `pitch_ramp`'s doc).
        let mut ratio = self.pitch_ratio;
        for slot in self.pitch_ramp[..frames].iter_mut() {
            *slot = ratio;
            ratio += (self.pitch_target - ratio) * self.pitch_coeff;
        }
        self.pitch_ratio = ratio;

        for i in 0..self.active_count {
            render_voice(
                &mut self.voices[i],
                &self.data,
                frames,
                self.release_frames,
                self.gain,
                &self.pitch_ramp[..frames],
                &mut self.left[..frames],
                &mut self.right[..frames],
            );
        }

        // Age the survivors, then swap-compact finished voices back into
        // the idle region (no Vec::remove, no reallocation).
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

        copy_channel(buffer, 0, &self.left[..frames]);
        copy_channel(buffer, 1, &self.right[..frames]);
    }

    /// RT-safe: plain field writes and pool-slot arithmetic.
    /// Velocity semantics match the SF2 processor: non-finite or ≤ 0
    /// velocity degrades to a note-off; the trigger gain is the linear
    /// engine velocity (0..1).
    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { note, velocity } => {
                let vel = if velocity.is_finite() {
                    velocity.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if vel <= 0.0 {
                    // MIDI semantics: note-on with velocity 0 is a note-off.
                    if note < 128 {
                        self.held &= !(1u128 << note);
                    }
                    self.release_matching(note);
                } else {
                    if note < 128 {
                        self.held |= 1u128 << note;
                    }
                    self.spawn(note, vel);
                }
            }
            NoteEvent::Off { note } => {
                if note == 0 && self.held & 1 == 0 {
                    // Blanket transport sweep with no held key 0: release
                    // everything, OneShot included (fixed 128-iteration
                    // loop; the bitmap only ever holds keys 0..=127).
                    for key in 0..128 {
                        if self.held >> key & 1 != 0 {
                            self.release_matching(key);
                        }
                    }
                    self.held = 0;
                    self.release_all();
                } else {
                    if note < 128 {
                        self.held &= !(1u128 << note);
                    }
                    self.release_matching(note);
                }
            }
        }
        true
    }

    /// A control-track curve may drive a CC. The sample player has no CC
    /// surface, so the event is consumed as a no-op (returning `false`
    /// would make the graph warn on every automation block).
    fn midi_cc(&mut self, _channel: u8, _controller: u8, _value: u8) -> bool {
        true
    }

    /// Applies sample parameters. RT-safe: plain field writes.
    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            // Root note only affects subsequently spawned voices (D4
            // trigger-time freeze).
            ParamId::SampleRootNote => {
                self.root_note = param_to_u8(value, 127);
            }
            // Non-finite values are dropped instead of poisoning the render
            // gain (NaN would propagate into every sample).
            ParamId::SampleGain => {
                if value.is_finite() {
                    self.gain = value.clamp(0.0, 2.0);
                } else {
                    return false;
                }
            }
            // NaN compares false → Gate, the safe default.
            ParamId::SamplePlayMode => {
                self.one_shot = value >= 0.5;
            }
            // D12: the play window. Positions are frozen into voices at
            // trigger (D4's freeze rule), so a write here only changes what
            // the *next* note-on plays — no cursor jumps under a sounding
            // voice.
            ParamId::SampleStart
            | ParamId::SampleLoopStart
            | ParamId::SampleLoopEnd
            | ParamId::SampleEnd
            | ParamId::SampleLoopMode => {
                let frame = param_to_u32(value);
                match id {
                    ParamId::SampleStart => self.raw_region.start = frame,
                    ParamId::SampleLoopStart => self.raw_region.loop_start = frame,
                    ParamId::SampleLoopEnd => self.raw_region.loop_end = frame,
                    ParamId::SampleEnd => self.raw_region.end = frame,
                    _ => self.raw_region.mode = frame.min(2) as u8,
                }
                self.region = resolve_region(self.raw_region, self.data.frames().len());
            }
            // P1/D11: unlike `SampleRootNote`, the offset applies to
            // **sounding** voices too — a bend that only affected new notes
            // could not slide.
            ParamId::PitchOffset if value.is_finite() => {
                let semitones = value.clamp(-PITCH_OFFSET_LIMIT, PITCH_OFFSET_LIMIT);
                self.pitch_target = (semitones / 12.0).exp2();
            }
            // A NaN offset would poison every sounding voice's rate; refuse it
            // the way the gain path does.
            ParamId::PitchOffset => return false,
            _ => return false,
        }
        true
    }
}

/// Renders one voice into the stereo scratch. RT-safe: fixed loop, linear
/// interpolation over pre-decoded frames, no allocation.
///
/// The fade (D2) is linear over `release_frames`; a voice ends when it
/// either plays past its window's end (natural end, no fade — the sample
/// itself is the timbre) or completes its fade.
///
/// Looping (D12/D13): the cursor wraps at `loop_end` back to `loop_start`,
/// or bounces between them for ping-pong. The wrap uses `rem_euclid` over
/// the loop period instead of a `while` subtraction, so a huge pitch ratio
/// (a sample played four octaves down still steps a fraction of a frame, but
/// a rate of 10× would step ten frames) can never spin the loop.
// The voice owns its cursor/gain state, the processor its sample/gain/pitch
// ramp, and the two output slices are the destination — bundling them into a
// context struct would allocate or borrow-split what is a hot inner loop.
#[allow(clippy::too_many_arguments)]
fn render_voice(
    voice: &mut Voice,
    data: &SampleData,
    frames: usize,
    release_frames: f32,
    master_gain: f32,
    pitch_ramp: &[f32],
    out_l: &mut [f32],
    out_r: &mut [f32],
) {
    let src = data.frames();
    for f in 0..frames {
        if !voice.active || voice.pos >= voice.end {
            voice.active = false;
            break;
        }
        let mut g = voice.vel_gain * master_gain;
        if voice.releasing {
            if voice.fade_wait > 0.0 {
                // D15: the tail is still playing; the fade starts when only
                // one fade-length of it is left.
                voice.fade_wait -= 1.0;
            } else {
                g *= 1.0 - voice.fade_pos / release_frames;
                voice.fade_pos += 1.0;
                if voice.fade_pos >= release_frames {
                    voice.active = false;
                }
            }
        }
        let i = voice.pos as usize;
        let frac = (voice.pos - i as f64) as f32;
        let a = src[i];
        let b = src[i + 1];
        out_l[f] += (a[0] + (b[0] - a[0]) * frac) * g;
        out_r[f] += (a[1] + (b[1] - a[1]) * frac) * g;
        voice.pos += voice.dir * voice.rate * f64::from(pitch_ramp.get(f).copied().unwrap_or(1.0));
        if voice.wrapping {
            wrap_into_loop(voice);
        }
    }
    // Natural end inside the block: only clamp when the cursor has run
    // past the final frame pair (checked again on the next block's first
    // frame).
    if voice.pos >= voice.end {
        voice.active = false;
    }
}

/// Folds the cursor back into `[loop_start, loop_end]` (D13).
///
/// Forward mode mirrors the overshoot back to the loop start; ping-pong
/// reflects it and flips [`Voice::dir`]. Both use the loop's period so any
/// step size lands in one arithmetic step.
fn wrap_into_loop(voice: &mut Voice) {
    let span = voice.loop_end - voice.loop_start;
    if span <= 0.0 {
        voice.wrapping = false;
        return;
    }
    if !voice.pingpong {
        if voice.pos >= voice.loop_end {
            voice.pos = voice.loop_start + (voice.pos - voice.loop_start).rem_euclid(span);
        }
        return;
    }
    // Ping-pong: one full cycle is forward then backward (2 × span). A
    // cursor exactly at a boundary keeps moving in the direction it was
    // heading, which is what makes the turnaround seamless.
    let period = 2.0 * span;
    let t = (voice.pos - voice.loop_start).rem_euclid(period);
    if t <= span {
        voice.pos = voice.loop_start + t;
        voice.dir = 1.0;
    } else {
        voice.pos = voice.loop_start + (period - t);
        voice.dir = -1.0;
    }
}

/// Copies `src` into channel `ch` of `buffer`, clipped to the shorter of
/// the two lengths (leftover tail stays zero from the scratch fill).
/// RT-safe: memcpy only, out-of-range channels are skipped.
fn copy_channel(buffer: &mut AudioBuffer, ch: usize, src: &[f32]) {
    if let Some(dst) = buffer.channel_mut(ch) {
        let n = dst.len().min(src.len());
        dst[..n].copy_from_slice(&src[..n]);
    }
}

/// Reads a literal u8 (root note) from a parameter value. NaN degrades to
/// 0 via saturating cast; out-of-range values clamp.
fn param_to_u8(value: f32, max: u8) -> u8 {
    value.round().clamp(0.0, f32::from(max)) as u8
}

/// Reads a frame position from a parameter value. Non-finite and negative
/// values degrade to 0 (file start / "unset"), and the f32 channel is exact
/// up to 2^24 frames (~5.8 min at 48 kHz).
fn param_to_u32(value: f32) -> u32 {
    if !value.is_finite() {
        return 0;
    }
    value.clamp(0.0, u32::MAX as f32) as u32
}

/// Resolves the document window against the loaded sample (D12).
///
/// `last` is the last **interpolatable** frame (`len - 1`, since the reader
/// needs `i + 1`); `0` on `loop_end`/`end` means "the end of the file", and a
/// loop narrower than two frames is not a loop. Same rule as the model's
/// `SampleRegion::resolve`.
fn resolve_region(raw: RawRegion, frames: usize) -> Region {
    let last = frames.saturating_sub(1) as u32;
    let end = if raw.end == 0 {
        last
    } else {
        raw.end.min(last)
    };
    let start = raw.start.min(end);
    let loop_end = if raw.loop_end == 0 {
        end
    } else {
        raw.loop_end.min(end)
    };
    let loop_start = raw.loop_start.min(loop_end);
    Region {
        start: f64::from(start),
        loop_start: f64::from(loop_start),
        loop_end: f64::from(loop_end),
        end: f64::from(end),
        mode: raw.mode,
    }
}

#[cfg(test)]
mod tests {
    use super::{SAMPLE_DEFAULT_ROOT, param_to_u8};

    #[test]
    fn root_note_default_is_c4() {
        assert_eq!(SAMPLE_DEFAULT_ROOT, 60);
    }

    #[test]
    fn param_u8_rounds_and_clamps() {
        assert_eq!(param_to_u8(0.0, 127), 0);
        assert_eq!(param_to_u8(60.0, 127), 60);
        assert_eq!(param_to_u8(60.4, 127), 60);
        assert_eq!(param_to_u8(60.5, 127), 61);
        assert_eq!(param_to_u8(200.0, 127), 127, "over-range clamps");
        assert_eq!(param_to_u8(-1.0, 127), 0, "under-range clamps");
        assert_eq!(param_to_u8(f32::NAN, 127), 0, "NaN degrades to 0");
    }
}
