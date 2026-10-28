//! Timeline clip scheduling (audio thread side, block-boundary sample clock,
//! multi-track) — timeline spec T2 (D14-D16).
//!
//! The scheduling model is **pure window scanning**: the sequencer only keeps
//! a musical position (in ticks) and the per-track clip tables; each block
//! schedules in one pass all note occurrences falling due within the absolute
//! tick window `[t0, t1)`. Clips are placed at absolute positions
//! (`start_ticks`), stay audible for `span_ticks`, and their note content
//! repeats every `loop_ticks` within the span (D14 tiling semantics — the
//! UI presents one period for now).
//!
//! BPM changes only affect future rates (the position is counted in ticks, so
//! the musical position is naturally preserved); no incremental clock
//! bookkeeping is kept.
//!
//! Arrangement end (D16): the latest clip end rounded up to a bar; playback
//! stops automatically when the transport reaches it (pending note offs keep
//! draining so the last notes release naturally). An empty arrangement
//! (no clips at all) is unbounded and free-runs. An active song loop (U1)
//! suppresses the auto-stop: the transport wraps from the loop end back to
//! the loop start forever, and a block that straddles the wrap point
//! schedules both segments of itself.
//!
//! Transport semantics (U1): `play` resumes from the current position,
//! `stop` rewinds to zero, `pause` keeps the position, `seek` moves it.

use crate::realtime::MAX_MIDI_TRACKS;
use crate::realtime::automation::{MAX_AUTO_TRACKS, RtControlClip};
use crate::realtime::commands::NoteEvent;
use crate::realtime::context::ProcessContext;

/// Note time base: ticks per quarter note.
///
/// The engine crate deliberately does **not** depend on `glitcherz-model` (the
/// audio thread must stay independent of the document layer), so this mirrors
/// `glitcherz_model::TICKS_PER_QUARTER`. The workspace test
/// `glitcherz-ui/tests/tick_base_consistency.rs` locks the two in step — 960
/// PPQ is the Standard MIDI File convention and divides evenly for 1/64
/// triplets.
pub const TICKS_PER_QUARTER: u32 = 960;

/// Ticks per step (a sixteenth note).
pub const TICKS_PER_STEP: u32 = TICKS_PER_QUARTER / 4;

/// Ticks per bar in **4/4** — the default clip span/loop length and the
/// fallback meter. The live bar length is [`Sequencer::bar_ticks`] (U3a).
pub const TICKS_PER_BAR: u32 = TICKS_PER_STEP * 16;

/// Largest beats-per-bar the engine accepts (`TimeSignature`'s mirror).
pub const MAX_TIME_SIGNATURE_NUMERATOR: u8 = 16;

/// Lowest transport BPM (mirrors the UI's `BPM_RANGE`; U3b keeps the tempo
/// curve inside the same bounds so a curve can never demand a rate the
/// transport itself would reject).
pub const MIN_BPM: f32 = 20.0;
/// Highest transport BPM.
pub const MAX_BPM: f32 = 300.0;

/// Bar length in ticks for a meter: `TICKS_PER_QUARTER × 4 / denominator ×
/// numerator`.
///
/// Mirrors `glitcherz_model::TimeSignature::ticks_per_bar` (the engine crate
/// cannot depend on the model); `tick_base_consistency.rs` keeps the two
/// formulas in step. Both fields are clamped: a malformed value must never
/// yield a zero-length bar (that would make `div_ceil` panic-free but every
/// arrangement end zero).
pub fn ticks_per_bar_of(numerator: u8, denominator: u8) -> u32 {
    let numerator = u32::from(numerator.clamp(1, MAX_TIME_SIGNATURE_NUMERATOR));
    let denominator = u32::from(denominator.clamp(1, 32).next_power_of_two().min(32));
    TICKS_PER_QUARTER * 4 / denominator * numerator
}

/// Lowest note-on velocity (a note-off; silence).
pub const MIN_VELOCITY: u8 = 1;

/// Maximum number of clips per track (D15; mirrors `glitcherz_model::MAX_CLIPS_PER_TRACK`).
pub const MAX_TRACK_CLIPS: usize = 8;

/// Trash-queue capacity for displaced MIDI clip tables (D15 revision,
/// 2026-10-28). The control thread drains it every UI frame; 64 slots absorb
/// an edit storm before updates start parking.
pub const MIDI_CLIP_TRASH_CAPACITY: usize = 64;

/// Creates the MIDI-clip trash channel pair (D15 revision): the producer end
/// goes to the block runner (the audio thread pushes displaced tables), the
/// consumer end to the engine (the control thread drains and drops them).
/// **Not RT-safe**; call only before the stream starts.
pub fn midi_clip_trash_channel() -> (rtrb::Producer<MidiClipTable>, rtrb::Consumer<MidiClipTable>) {
    rtrb::RingBuffer::new(MIDI_CLIP_TRASH_CAPACITY)
}

/// Audio-thread-side controller kind (mirrors `glitcherz_model::CtrlKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtCtrlKind {
    /// Control change number 0..=127.
    Cc(u8),
    /// Pitch bend, value -8192..=8191 (0 = no bend).
    PitchBend,
    /// Channel (mono) aftertouch, value 0..=127.
    ChannelPressure,
    /// Polyphonic aftertouch for the given pitch, value 0..=127.
    PolyPressure(u8),
    /// Program change, value 0..=127.
    Program,
}

impl Default for RtCtrlKind {
    /// CC 0 — what a zeroed [`RtCtrl`] slot holds. `#[default]` cannot be used
    /// here: the first variant carries a payload.
    fn default() -> Self {
        Self::Cc(0)
    }
}

/// Audio-thread-side controller event (mirrors `glitcherz_model::CtrlEvent`).
///
/// A controller is an **event**, not a parameter: it must be delivered through
/// the `midi_*` channels of [`Processor`](crate::realtime::Processor), never
/// through `set_param` (spec `control-tracks.md` D3).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RtCtrl {
    /// Position within the clip (ticks, relative to the clip start).
    pub tick_ticks: u32,
    /// MIDI channel 0..=15.
    pub channel: u8,
    /// Which controller this drives.
    pub kind: RtCtrlKind,
    /// Value in the controller's own range (see [`RtCtrlKind`]).
    pub value: i32,
}

impl RtCtrl {
    /// Builds the audio-thread form from raw document values, clamping the
    /// tick range and the MIDI domains the same way [`RtNote::new`] does.
    pub fn new(tick_ticks: i64, channel: u8, kind: RtCtrlKind, value: i32) -> Self {
        Self {
            tick_ticks: tick_ticks.clamp(0, i64::from(u32::MAX)) as u32,
            channel: channel.min(15),
            kind,
            value,
        }
    }

    /// Value clamped into the 7-bit controller range.
    pub fn value_7bit(&self) -> u8 {
        self.value.clamp(0, 127) as u8
    }

    /// Value clamped into the centred 14-bit pitch-bend range.
    pub fn value_i14(&self) -> i16 {
        self.value.clamp(-8192, 8191) as i16
    }
}

/// Receives the events [`Sequencer::advance`] produces.
///
/// Notes and controllers travel side by side but must not share one enum: a
/// note carries pitch+velocity semantics, a controller carries a controller id
/// in its own domain (spec `control-tracks.md` D3). A trait — rather than two
/// closures — lets both kinds reach the graph through **one** `&mut` borrow,
/// which two closures capturing the same graph could not do.
pub trait SequencerSink {
    /// Note on/off for `track`.
    fn note(&mut self, track: usize, event: NoteEvent);

    /// Controller event for `track` (default: dropped, so note-only sinks stay
    /// single-method implementations).
    fn ctrl(&mut self, _track: usize, _ctrl: RtCtrl) {}
}

/// Audio-thread-side representation of a piano-roll note (mirrors `glitcherz_model::Note`).
///
/// Unsigned ticks and a normalized velocity: the audio thread never sees the
/// signed drag intermediates, and clamping happens once at the document edge.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RtNote {
    /// Start position (in ticks, relative to the clip start).
    pub start_ticks: u32,
    /// Length (in ticks), always `>= 1`.
    pub length_ticks: u32,
    /// MIDI note number.
    pub note: u8,
    /// MIDI channel.
    pub channel: u8,
    /// Velocity, 1..=127 (MIDI wire range).
    pub velocity: u8,
}

impl RtNote {
    /// Builds the audio-thread form from raw document values, clamping the
    /// signed tick range and validating the MIDI ranges. The caller (the UI
    /// layer, which does see `glitcherz_model`) converts types; the audio thread
    /// only ever sees non-negative, in-range values.
    pub fn new(start_ticks: i64, length_ticks: i64, note: u8, channel: u8, velocity: u8) -> Self {
        Self {
            start_ticks: start_ticks.clamp(0, i64::from(u32::MAX)) as u32,
            length_ticks: length_ticks.clamp(1, i64::from(u32::MAX)) as u32,
            note: note.min(127),
            channel: channel.min(15),
            velocity: velocity.clamp(MIN_VELOCITY, 127),
        }
    }

    /// Velocity normalized to 0.0..=1.0 for the synthesizer.
    pub fn velocity_f32(&self) -> f32 {
        f32::from(self.velocity.clamp(MIN_VELOCITY, 127)) / 127.0
    }
}

/// Audio-thread-side representation of a MIDI clip placed on the arrangement
/// timeline (mirrors `glitcherz_model::MidiClip`; D15 as revised 2026-10-28).
///
/// **Ownership protocol (D15 revision)**: note and controller collections are
/// `Vec`s *owned by the control side*. A table is built on the control thread
/// (where allocation is allowed), travels through the **structure queue** (not
/// the fixed-size command queue), and is swapped in on the audio thread with
/// `mem::replace`; the displaced table travels back through the trash queue and
/// is dropped on the control thread. The audio thread only ever **reads** these
/// vectors — no allocation, no deallocation, no locks.
///
/// This is why there is no per-clip note cap any more: the old design capped
/// clips at 64 notes so a whole clip table fit in a `Copy` command slot.
#[derive(Debug, Clone, PartialEq)]
pub struct RtMidiClip {
    /// Absolute start position on the arrangement (in ticks).
    pub start_ticks: u32,
    /// How long the clip stays active (in ticks).
    pub span_ticks: u32,
    /// Content loop length (in ticks).
    pub loop_ticks: u32,
    /// Notes, relative to the clip start.
    pub notes: Vec<RtNote>,
    /// Controller events, relative to the clip start (P0).
    pub ctrls: Vec<RtCtrl>,
}

impl Default for RtMidiClip {
    fn default() -> Self {
        Self {
            start_ticks: 0,
            // span 0 = inactive: an all-default clip table is an EMPTY track
            // (the scheduler skips span-0 clips); every real clip carries its
            // own placement.
            span_ticks: 0,
            loop_ticks: TICKS_PER_BAR,
            notes: Vec::new(),
            ctrls: Vec::new(),
        }
    }
}

impl RtMidiClip {
    /// Appends a note.
    ///
    /// **Control thread only** — this may grow the vector, which the real-time
    /// rules forbid on the audio thread.
    pub fn push(&mut self, note: RtNote) {
        self.notes.push(note);
    }

    /// Appends a controller event. **Control thread only** (see [`Self::push`]).
    pub fn push_ctrl(&mut self, ctrl: RtCtrl) {
        self.ctrls.push(ctrl);
    }

    /// Whether the clip carries no content at all.
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty() && self.ctrls.is_empty()
    }
}

/// Clip table of one MIDI track: at most [`MAX_TRACK_CLIPS`] clips, indexed
/// exactly like the document's `MidiTrack::clips` order (D15).
pub type MidiClipTable = [RtMidiClip; MAX_TRACK_CLIPS];

/// An empty table (no clips — silent track).
pub fn empty_midi_clip_table() -> MidiClipTable {
    std::array::from_fn(|_| RtMidiClip::default())
}

/// Multi-track sequencer over the arrangement timeline: one global transport
/// clock (BPM / musical position), per-track clip tables and gate tails,
/// converting due events into `(MIDI track, NoteEvent)` at block boundaries on
/// the sample clock.
///
/// Control (automation) clips are independent entities parallel to MIDI
/// tracks (D7/D17): [`Sequencer`] only holds their placements and curves; the
/// delivery target is determined by the node graph's automation routing table
/// and the delivery window by the clip span — this struct knows nothing about
/// either.
///
/// The clock is counted in ticks (musical position), converted to the sample
/// clock via BPM; note offs are recorded as absolute sample positions and
/// emitted at the block boundary containing them.
///
/// Precision note: events are emitted uniformly at the **block boundary**
/// containing their moment (≤ 1 block ≈ 2.7 ms jitter @48 kHz / 128-block),
/// per-sample offsets are left for a future version.
///
/// All methods RT-safe: no allocation, no locks, no panics (`advance` events
/// are sent through a callback, never collected or heap-allocated).
/// Max simultaneously-gated note-offs tracked per MIDI track (a chord): each
/// sounding note keeps its own slot so no pending off overwrites another.
pub const MAX_PENDING_OFFS_PER_TRACK: usize = 16;

pub struct Sequencer {
    sample_rate: f32,
    playing: bool,
    /// The BPM the user set (transport field). U3b splits this from
    /// [`Sequencer::bpm_override`] so a tempo curve can override it per block
    /// and cleanly fall back outside the curve (D3).
    base_bpm: f32,
    /// Per-block tempo override sampled from a Timeline In curve (U3b). `None`
    /// = no curve covers the current position, use `base_bpm`. Rebuilt every
    /// block by the block runner before `advance` (D4).
    bpm_override: Option<f32>,
    /// Ticks per bar for the document's meter (U3a). Everything that measures
    /// in bars — arrangement-end rounding, loop snapping on the control side —
    /// reads this instead of the 4/4 [`TICKS_PER_BAR`] constant. Set by
    /// `SetTimeSignature`; changing it never moves the transport.
    bar_ticks: u32,
    position_ticks: f64,
    /// Active MIDI track count (D12 dynamic tracks: sent from the control side
    /// via `SetMidiTrackCount`, scheduling only scans `0..track_count`;
    /// unconfigured tracks have default-empty clip tables).
    track_count: usize,
    /// Per-track clip tables (D15 revision: ≤ [`MAX_TRACK_CLIPS`] clips each,
    /// replaced one track at a time through the structure queue).
    clips: [MidiClipTable; MAX_MIDI_TRACKS],
    /// Per-control-track clip tables (D15/D17).
    control_clips: [[RtControlClip; MAX_TRACK_CLIPS]; MAX_AUTO_TRACKS],
    /// Arrangement end in ticks (D16: the latest clip end rounded up to a
    /// bar); `0` = unbounded (empty arrangement, free run).
    arrangement_end_ticks: u32,
    /// Arrangement-end **floor** (U7: the offline export sets it to the
    /// latest clip end across ALL track kinds — audio clips are not
    /// sequencer content, so the auto-stop would otherwise cut audio that
    /// ends past the MIDI/control arrangement end). The live engine never
    /// sets it (`0` = no floor).
    arrangement_floor_ticks: u32,
    /// Song loop range (U1). Invariant: `loop_end == 0 || loop_end >
    /// loop_start` — anything else means looping is disabled.
    loop_start_ticks: u32,
    loop_end_ticks: u32,
    /// Per-track pending NoteOffs (absolute sample position where the length
    /// ends + note), several slots per track so a chord's notes each keep
    /// their own off (a single slot orphans all but the last note).
    pending_offs: [[Option<(u64, u8)>; MAX_PENDING_OFFS_PER_TRACK]; MAX_MIDI_TRACKS],
}

impl Sequencer {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction-time tolerance).
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate: if sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            playing: false,
            base_bpm: 120.0,
            bpm_override: None,
            bar_ticks: TICKS_PER_BAR,
            position_ticks: 0.0,
            track_count: crate::realtime::DEFAULT_MIDI_TRACKS,
            clips: std::array::from_fn(|_| empty_midi_clip_table()),
            control_clips: [[RtControlClip::default(); MAX_TRACK_CLIPS]; MAX_AUTO_TRACKS],
            arrangement_end_ticks: 0,
            arrangement_floor_ticks: 0,
            loop_start_ticks: 0,
            loop_end_ticks: 0,
            pending_offs: [[None; MAX_PENDING_OFFS_PER_TRACK]; MAX_MIDI_TRACKS],
        }
    }

    /// Whether playing. RT-safe.
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Current step index (`⌊musical position / step length⌋ mod 16`, used by
    /// the UI as a coarse playhead hint).
    /// RT-safe.
    pub fn step_index(&self) -> usize {
        (self.position_ticks / f64::from(TICKS_PER_STEP)).floor() as usize % 16
    }

    /// Musical position (in ticks, accumulated from the play start). RT-safe.
    pub fn position_ticks(&self) -> f64 {
        self.position_ticks
    }

    /// Tick width of a block of `frames` samples at the current effective
    /// tempo (U6): the same step `advance()` uses, exposed so the block
    /// runner can stamp it into [`crate::realtime::context::ProcessContext`]
    /// **after** automation evaluation (a tempo curve governing this block
    /// must already be applied). RT-safe.
    pub fn transport_dt_ticks(&self, frames: usize) -> f64 {
        frames as f64 * self.ticks_per_sample()
    }

    /// Whether the transport is playing (U6: audio clips stay silent
    /// otherwise). RT-safe.
    pub fn is_transport_active(&self) -> bool {
        self.playing
    }

    /// Seconds per tick at the **base** tempo (U11 D2): audio clips map their
    /// seconds-domain trim window onto the tick grid with this and do **not**
    /// follow the tempo curve. `base_bpm` is always clamped to a sane range
    /// by [`Self::set_base_bpm`], so this never divides by zero. RT-safe.
    pub fn base_sec_per_tick(&self) -> f64 {
        60.0 / (f64::from(self.base_bpm) * f64::from(TICKS_PER_QUARTER))
    }

    /// Ticks per sample (f64 intermediate computation).
    fn ticks_per_sample(&self) -> f64 {
        // ticks/second = BPM/60 × TPQN; divide by the sample rate to get ticks/sample.
        f64::from(self.effective_bpm()) / 60.0 * f64::from(TICKS_PER_QUARTER)
            / f64::from(self.sample_rate)
    }

    /// The BPM actually in force this block (U3b D3): the curve override when
    /// one was sampled, otherwise the user's base value. Clamped to
    /// [`MIN_BPM`]..=[`MAX_BPM`]; non-finite values fall back to 120.
    /// RT-safe.
    pub fn effective_bpm(&self) -> f32 {
        let bpm = self.bpm_override.unwrap_or(self.base_bpm);
        if bpm.is_finite() {
            bpm.clamp(MIN_BPM, MAX_BPM)
        } else {
            120.0
        }
    }

    /// Sets the curve override for the current block (U3b). Values outside
    /// [`MIN_BPM`]..=[`MAX_BPM`] are clamped, non-finite values ignored.
    /// RT-safe.
    pub fn set_bpm_override(&mut self, bpm: f32) {
        if bpm.is_finite() {
            self.bpm_override = Some(bpm.clamp(MIN_BPM, MAX_BPM));
        }
    }

    /// Drops the curve override: the transport returns to the user's base BPM
    /// (U3b D3 — outside every tempo clip, and while stopped). The block
    /// runner calls this once per block before re-sampling, so a curve that
    /// stops covering the playhead cannot leave the tempo stuck at its last
    /// value. RT-safe.
    pub fn clear_bpm_override(&mut self) {
        self.bpm_override = None;
    }

    /// Play from the **current** transport position (resume semantics, U1).
    /// Fresh-from-zero playback is `Stop` followed by `Play` (or `Seek(0)`).
    /// Pending offs from the previous run are cleared: the block runner
    /// releases them by pitch before this takes effect. RT-safe.
    pub fn play(&mut self) {
        self.playing = true;
        self.bpm_override = None;
        self.pending_offs = [[None; MAX_PENDING_OFFS_PER_TRACK]; MAX_MIDI_TRACKS];
    }

    /// Stops scheduling and rewinds to zero. The caller should send
    /// `NoteOff` to all tracks to release envelopes. A tempo override is
    /// dropped so the stopped transport reads the user's base BPM (U3b).
    /// RT-safe.
    pub fn stop(&mut self) {
        self.playing = false;
        self.bpm_override = None;
        self.position_ticks = 0.0;
        self.pending_offs = [[None; MAX_PENDING_OFFS_PER_TRACK]; MAX_MIDI_TRACKS];
    }

    /// Pauses: stops scheduling but **keeps the transport position** (U1);
    /// resume with [`Sequencer::play`]. Pending offs are cleared — the block
    /// runner releases them by pitch (sounding notes fall silent). The tempo
    /// override is cleared too: automation is only evaluated while playing
    /// (U3b D3).
    /// RT-safe.
    pub fn pause(&mut self) {
        self.playing = false;
        self.bpm_override = None;
        self.pending_offs = [[None; MAX_PENDING_OFFS_PER_TRACK]; MAX_MIDI_TRACKS];
    }

    /// Moves the transport to `ticks` (clamped to ≥ 0; non-finite values
    /// are ignored). Pending offs are cleared: nothing scheduled relative to
    /// the old position may fire at the new one. RT-safe.
    pub fn seek(&mut self, ticks: f64) {
        if ticks.is_finite() {
            self.position_ticks = ticks.max(0.0);
            self.pending_offs = [[None; MAX_PENDING_OFFS_PER_TRACK]; MAX_MIDI_TRACKS];
        }
    }

    /// Sets the song loop range (U1). While playing, the transport wraps
    /// from `end` back to `start`; `end == 0` or `end <= start` disables
    /// looping (a stale position beyond the new range keeps playing — the
    /// wrap only triggers once the transport re-enters it). RT-safe.
    pub fn set_loop_range(&mut self, start_ticks: u32, end_ticks: u32) {
        if end_ticks == 0 || end_ticks <= start_ticks {
            self.loop_start_ticks = 0;
            self.loop_end_ticks = 0;
        } else {
            self.loop_start_ticks = start_ticks;
            self.loop_end_ticks = end_ticks;
        }
    }

    /// Whether the song loop is active. RT-safe.
    fn loop_active(&self) -> bool {
        self.loop_end_ticks > 0
    }

    /// Takes the pending note offs (clearing them). The caller must release
    /// these **by note number**: external plugins match note-offs by pitch
    /// (Dexed's `keyup` looks up `midi_note == pitch`), so a blanket
    /// `Off { note: 0 }` never reaches them and leaves the voice hanging.
    /// RT-safe.
    pub fn take_pending_offs(
        &mut self,
    ) -> [[Option<(u64, u8)>; MAX_PENDING_OFFS_PER_TRACK]; MAX_MIDI_TRACKS] {
        std::mem::replace(
            &mut self.pending_offs,
            [[None; MAX_PENDING_OFFS_PER_TRACK]; MAX_MIDI_TRACKS],
        )
    }

    /// Sets the active MIDI track count (clamped to [`MAX_MIDI_TRACKS`]; when
    /// shrinking, gate tails of higher-numbered tracks are cleared too). RT-safe.
    pub fn set_track_count(&mut self, count: usize) {
        self.track_count = count.min(MAX_MIDI_TRACKS);
        if self.pending_offs.len() > self.track_count {
            for slots in self.pending_offs[self.track_count..].iter_mut() {
                *slots = [None; MAX_PENDING_OFFS_PER_TRACK];
            }
        }
    }

    /// Current active MIDI track count. RT-safe.
    pub fn track_count(&self) -> usize {
        self.track_count
    }

    /// Sets the user's base BPM (20..300, non-finite values ignored). The
    /// musical position is counted in ticks, so tempo changes do not affect
    /// already-played content, only the subsequent rate. A curve override
    /// (U3b) still wins until the next block clears it. RT-safe.
    pub fn set_bpm(&mut self, bpm: f32) {
        if bpm.is_finite() {
            self.base_bpm = bpm.clamp(MIN_BPM, MAX_BPM);
        }
    }

    /// The user's base BPM (what the transport field shows; unaffected by a
    /// tempo curve, U3b). RT-safe.
    pub fn base_bpm(&self) -> f32 {
        self.base_bpm
    }

    /// Sets the document's meter (U3a): only the bar length changes, and the
    /// arrangement end is re-rounded to whole bars of the new length. The
    /// transport position is untouched — a meter is notation, not a data
    /// transform (no event moves).
    ///
    /// RT-safe: arithmetic only, no allocation.
    pub fn set_time_signature(&mut self, numerator: u8, denominator: u8) {
        let bar_ticks = ticks_per_bar_of(numerator, denominator);
        if bar_ticks != self.bar_ticks {
            self.bar_ticks = bar_ticks;
            self.refresh_arrangement_end();
        }
    }

    /// The live bar length in ticks (U3a). RT-safe.
    pub fn bar_ticks(&self) -> u32 {
        self.bar_ticks
    }

    /// Replaces one MIDI track's clip table in one block (D15 revision; the
    /// displaced table is returned so the caller can move it to the trash
    /// queue — the audio thread must never drop it). Out-of-range tracks are
    /// ignored and return `None`. Also refreshes the arrangement end. RT-safe.
    pub fn set_track_clips(&mut self, track: usize, clips: MidiClipTable) -> Option<MidiClipTable> {
        let slot = self.clips.get_mut(track)?;
        let displaced = std::mem::replace(slot, clips);
        self.refresh_arrangement_end();
        Some(displaced)
    }

    /// Replaces one control track's clip table in one block (D15 batch;
    /// out-of-range indices ignored). Also refreshes the arrangement end. RT-safe.
    pub fn set_control_clips(&mut self, index: usize, clips: [RtControlClip; MAX_TRACK_CLIPS]) {
        if let Some(slot) = self.control_clips.get_mut(index) {
            *slot = clips;
            self.refresh_arrangement_end();
        }
    }

    /// Read-only access to all control clip tables (for the block runner to
    /// sample per the routing table). RT-safe.
    pub fn control_clips(&self) -> &[[RtControlClip; MAX_TRACK_CLIPS]; MAX_AUTO_TRACKS] {
        &self.control_clips
    }

    /// Arrangement end in ticks (D16; `0` = unbounded). RT-safe.
    pub fn arrangement_end_ticks(&self) -> u32 {
        self.arrangement_end_ticks
    }

    /// D16: arrangement end = the latest clip end (`start + span`) across all
    /// MIDI and control clips, rounded up to a whole bar of the **current**
    /// meter (U3a). The export floor (U7) keeps the end at least as far out
    /// as the caller demanded, so audio-track material ending past the
    /// MIDI content is not cut by the auto-stop.
    fn refresh_arrangement_end(&mut self) {
        let mut latest: u32 = 0;
        for track in &self.clips {
            for clip in track {
                latest = latest.max(clip.start_ticks.saturating_add(clip.span_ticks));
            }
        }
        for track in &self.control_clips {
            for clip in track {
                latest = latest.max(clip.start_ticks.saturating_add(clip.span_ticks));
            }
        }
        latest = latest.max(self.arrangement_floor_ticks);
        let bar = self.bar_ticks.max(1);
        self.arrangement_end_ticks = if latest == 0 {
            0
        } else {
            latest.div_ceil(bar) * bar
        };
    }

    /// Sets the arrangement-end floor (U7): the auto-stop never fires
    /// before this tick. `0` clears it (the live engine's state). RT-safe;
    /// the end refreshes immediately.
    pub fn set_arrangement_floor(&mut self, floor_ticks: u32) {
        self.arrangement_floor_ticks = floor_ticks;
        self.refresh_arrangement_end();
    }

    /// Advances one block: sends note offs and note ons falling due within the
    /// `[t0, t1)` (tick) window via `sink` (offs before ons within the same
    /// block; controller events fire at the block containing their tick).
    /// The window is clamped at the arrangement end
    /// (D16): when the transport reaches it, scheduling stops and the
    /// transport auto-stops; pending offs keep draining afterwards so the
    /// last notes release naturally.
    /// RT-safe.
    pub fn advance(&mut self, ctx: &ProcessContext, sink: &mut dyn SequencerSink) {
        let tps = self.ticks_per_sample();
        let samples_per_tick = 1.0 / tps;
        let block_start = ctx.playback_sample_pos;

        // 1. Due note offs — always processed, even after an auto-stop, so
        //    the arrangement's final notes still release (offs before ons,
        //    keeping each track's handoff clean).
        for (track, slots) in self.pending_offs.iter_mut().enumerate() {
            for pending in slots.iter_mut() {
                if let Some((time, note)) = *pending {
                    if time < block_start + ctx.frames as u64 {
                        sink.note(track, NoteEvent::Off { note });
                        *pending = None;
                    }
                }
            }
        }

        if !self.playing {
            return;
        }

        // 2. This block's window, split into segments at the song loop wrap
        //    (U1). A block that crosses the loop end schedules both the tail
        //    `[t0, loop_end)` and the wrapped head `[loop_start, ...)`. The
        //    window is clamped at the arrangement end (D16) — but only when
        //    the song loop is inactive: an active loop never auto-stops.
        let dt_ticks = ctx.frames as f64 * tps;
        let mut budget = dt_ticks;
        let mut t = self.position_ticks;
        let mut stop_at_end = false;
        while budget > 0.0 {
            if self.loop_active() {
                let ls = f64::from(self.loop_start_ticks);
                let le = f64::from(self.loop_end_ticks);
                if t >= le {
                    t = ls;
                }
                let seg_end = (t + budget).min(le);
                // Float-progress guard: at extreme positions `t + budget`
                // can round back to `t`; without this the loop would spin.
                if seg_end <= t {
                    break;
                }
                for track in 0..self.track_count {
                    self.schedule_track_clips(
                        track,
                        t,
                        seg_end,
                        block_start,
                        samples_per_tick,
                        sink,
                    );
                }
                budget -= (seg_end - t).min(budget);
                t = if seg_end >= le { ls } else { seg_end };
            } else {
                let mut seg_end = t + budget;
                if self.arrangement_end_ticks > 0 {
                    let end = f64::from(self.arrangement_end_ticks);
                    if t >= end {
                        stop_at_end = true;
                        break;
                    }
                    if seg_end >= end {
                        seg_end = end;
                        stop_at_end = true;
                    }
                }
                if seg_end <= t {
                    break;
                }
                for track in 0..self.track_count {
                    self.schedule_track_clips(
                        track,
                        t,
                        seg_end,
                        block_start,
                        samples_per_tick,
                        sink,
                    );
                }
                budget -= (seg_end - t).min(budget);
                t = seg_end;
                if stop_at_end {
                    break;
                }
            }
        }

        self.position_ticks = t;
        if stop_at_end {
            // Keep the pending offs: they drain in the following blocks and
            // release the arrangement's last notes.
            self.playing = false;
        }
    }

    /// One track's clip table: for every clip audible in the window, note
    /// occurrences (`start + m·loop + note.start`, strictly before the clip
    /// end) starting within the window fire an On + a length-based pending Off.
    /// Controller events (P0) tile the same way, yet carry no length: they
    /// fire once and need no pending-off bookkeeping.
    #[allow(clippy::too_many_arguments)]
    fn schedule_track_clips(
        &mut self,
        track: usize,
        t0: f64,
        t1: f64,
        block_start: u64,
        samples_per_tick: f64,
        sink: &mut dyn SequencerSink,
    ) {
        // Disjoint field borrows: the clip table is read-only while scheduling,
        // the pending-offs table is written as notes are triggered.
        let clips = &self.clips[track];
        let pending = &mut self.pending_offs[track];
        for clip in clips {
            if clip.is_empty() || clip.loop_ticks == 0 || clip.span_ticks == 0 {
                continue;
            }
            let start = f64::from(clip.start_ticks);
            let clip_end = start + f64::from(clip.span_ticks);
            let loop_ticks = f64::from(clip.loop_ticks);
            // Controller occurrences come **before** the notes of the same
            // block: a pitch bend or program change sets the state a note
            // starts in, so it must land first (mirrors the SMF export's
            // off → controller → on ordering). Same tiling as notes, but a
            // controller is a point event — no length, no pending off, no
            // retrigger guard.
            for ctrl in &clip.ctrls {
                let offset = f64::from(ctrl.tick_ticks);
                let mut m = ((t0 - start - offset) / loop_ticks).ceil();
                if m < 0.0 {
                    m = 0.0;
                }
                loop {
                    let at = start + m * loop_ticks + offset;
                    if at >= t1 || at >= clip_end {
                        break; // past the window or past the clip span
                    }
                    if at >= t0 {
                        sink.ctrl(track, *ctrl);
                    }
                    m += 1.0;
                }
            }
            for note in &clip.notes {
                let start_offset = f64::from(note.start_ticks);
                let length = f64::from(note.length_ticks.max(1));
                // The m-th occurrence is at start + m·loop + s; find the first
                // m with an on-time ≥ t0 (may be negative, clamped to 0).
                let mut m = ((t0 - start - start_offset) / loop_ticks).ceil();
                if m < 0.0 {
                    m = 0.0;
                }
                loop {
                    let on_tick = start + m * loop_ticks + start_offset;
                    if on_tick >= t1 || on_tick >= clip_end {
                        break; // past the window or past the clip span
                    }
                    if on_tick >= t0 {
                        let velocity = note.velocity_f32();
                        // Retrigger guard: a previous occurrence of this note
                        // may still be sounding (its off was scheduled past
                        // the clip end by the loop) — the new occurrence
                        // would overwrite the single pending-off slot,
                        // leaving the old voice hanging forever on
                        // pitch-matching plugins. Release it first instead.
                        for slot in pending.iter_mut() {
                            if let Some((_, pending_note)) = *slot {
                                if pending_note == note.note {
                                    *slot = None;
                                    sink.note(track, NoteEvent::Off { note: note.note });
                                }
                            }
                        }
                        sink.note(
                            track,
                            NoteEvent::On {
                                note: note.note,
                                velocity,
                            },
                        );
                        set_pending_off(
                            pending,
                            on_tick + length,
                            t0,
                            block_start,
                            samples_per_tick,
                            note.note,
                        );
                    }
                    m += 1.0;
                }
            }
        }
    }
}

/// Records a note off: converted to an absolute sample position, landing at
/// least in the next block (guaranteeing the off comes after the on already
/// emitted in the same block). The sample clock is continuous across a
/// song-loop wrap, so an off whose tick lands beyond the loop end still fires
/// at `on + length` — no wrap mapping needed (U1).
///
/// A free function (not a `Sequencer` method) so the caller can hold the clip
/// table and the pending-off table as disjoint borrows. RT-safe.
#[allow(clippy::too_many_arguments)]
fn set_pending_off(
    slots: &mut [Option<(u64, u8)>; MAX_PENDING_OFFS_PER_TRACK],
    off_tick: f64,
    t0: f64,
    block_start: u64,
    samples_per_tick: f64,
    note: u8,
) {
    let offset_samples = ((off_tick - t0) * samples_per_tick).max(1.0);
    let at = block_start + offset_samples as u64;
    // Reuse the slot already holding this pitch, else take the first free one;
    // if every slot is busy, fall back to slot 0.
    let mut target = None;
    for (i, slot) in slots.iter().enumerate() {
        match slot {
            Some((_, held)) if *held == note => {
                target = Some(i);
                break;
            }
            None if target.is_none() => target = Some(i),
            _ => {}
        }
    }
    slots[target.unwrap_or(0)] = Some((at, note));
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_BPM, MAX_TRACK_CLIPS, RtCtrl, RtCtrlKind, RtMidiClip, RtNote, Sequencer, SequencerSink,
        TICKS_PER_BAR, TICKS_PER_QUARTER, TICKS_PER_STEP, empty_midi_clip_table,
    };
    use crate::realtime::MAX_MIDI_TRACKS;
    use crate::realtime::automation::{MAX_AUTO_TRACKS, RtAutoPoint, RtControlClip};
    use crate::realtime::commands::NoteEvent;
    use crate::realtime::context::ProcessContext;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn ctx(pos: u64) -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: pos,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        }
    }

    type Event = (usize, NoteEvent);

    /// Advances `blocks` blocks, collecting all (track, event) pairs.
    fn run(seq: &mut Sequencer, blocks: usize) -> Vec<Event> {
        run_from(seq, 0, blocks)
    }

    /// Same as [`run`], but the fake sample clock continues from `start`
    /// (pending offs are absolute sample positions — spanning several `run`
    /// calls needs a continuous clock).
    /// Collects both event kinds so a test can assert on either.
    struct VecSink<'a> {
        notes: &'a mut Vec<Event>,
        ctrls: &'a mut Vec<(usize, RtCtrl)>,
    }

    impl SequencerSink for VecSink<'_> {
        fn note(&mut self, track: usize, event: NoteEvent) {
            self.notes.push((track, event));
        }

        fn ctrl(&mut self, track: usize, ctrl: RtCtrl) {
            self.ctrls.push((track, ctrl));
        }
    }

    fn run_both(
        seq: &mut Sequencer,
        start: u64,
        blocks: usize,
    ) -> (Vec<Event>, Vec<(usize, RtCtrl)>) {
        let mut notes = Vec::new();
        let mut ctrls = Vec::new();
        let mut pos = start;
        for _ in 0..blocks {
            seq.advance(
                &ctx(pos),
                &mut VecSink {
                    notes: &mut notes,
                    ctrls: &mut ctrls,
                },
            );
            pos += BLOCK as u64;
        }
        (notes, ctrls)
    }

    fn run_from(seq: &mut Sequencer, start: u64, blocks: usize) -> Vec<Event> {
        run_both(seq, start, blocks).0
    }

    /// Advances `blocks` blocks and collects only controller events.
    fn run_ctrls(seq: &mut Sequencer, blocks: usize) -> Vec<(usize, RtCtrl)> {
        run_both(seq, 0, blocks).1
    }

    fn clip_note(start: u32, length: u32, note: u8) -> RtNote {
        RtNote {
            start_ticks: start,
            length_ticks: length,
            note,
            channel: 0,
            velocity: 100,
        }
    }

    /// One clip on one track: `start`/`span`/`loop` in ticks + notes.
    fn place(
        seq: &mut Sequencer,
        track: usize,
        start: u32,
        span: u32,
        loop_ticks: u32,
        notes: &[RtNote],
    ) {
        let mut clip = RtMidiClip {
            start_ticks: start,
            span_ticks: span,
            loop_ticks,
            ..RtMidiClip::default()
        };
        for note in notes {
            clip.push(*note);
        }
        let mut clips = empty_midi_clip_table();
        clips[0] = clip;
        let _ = seq.set_track_clips(track, clips);
    }

    /// One clip on one track carrying only controller events (P0).
    fn place_ctrls(
        seq: &mut Sequencer,
        track: usize,
        start: u32,
        span: u32,
        loop_ticks: u32,
        ctrls: &[RtCtrl],
    ) {
        let mut clip = RtMidiClip {
            start_ticks: start,
            span_ticks: span,
            loop_ticks,
            ..RtMidiClip::default()
        };
        for ctrl in ctrls {
            clip.push_ctrl(*ctrl);
        }
        let mut clips = empty_midi_clip_table();
        clips[0] = clip;
        let _ = seq.set_track_clips(track, clips);
    }

    /// A CC event at `tick`, channel 0.
    fn ctrl_at(tick: u32, controller: u8, value: i32) -> RtCtrl {
        RtCtrl::new(i64::from(tick), 0, RtCtrlKind::Cc(controller), value)
    }

    fn ons(events: &[Event]) -> Vec<(usize, u8)> {
        events
            .iter()
            .filter_map(|(t, e)| match e {
                NoteEvent::On { note, .. } => Some((*t, *note)),
                _ => None,
            })
            .collect()
    }

    /// D15 revision: a clip's controller collection is a control-side `Vec` —
    /// nothing is dropped at any realistic density (a dense imported pitch bend
    /// used to be truncated at 32 events).
    #[test]
    fn clip_ctrl_collection_is_not_capped() {
        let mut clip = RtMidiClip::default();
        for i in 0..256 {
            clip.push_ctrl(RtCtrl::new(i, 0, RtCtrlKind::Cc(1), i as i32));
        }
        assert_eq!(clip.ctrls.len(), 256);
        assert_eq!(clip.ctrls[255].tick_ticks, 255);
    }

    /// P0: a clip's controller event is delivered once, on its own track, in
    /// the block containing its tick.
    #[test]
    fn clip_ctrls_fire_at_their_positions() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        place_ctrls(&mut seq, 0, 0, 384, 384, &[ctrl_at(192, 11, 100)]);
        seq.play();

        let ctrls = run_ctrls(&mut seq, 400); // ~2048 ticks at 120 BPM
        assert_eq!(ctrls.len(), 1, "one event per occurrence: {ctrls:?}");
        assert_eq!(ctrls[0].0, 0, "delivered on the clip's track");
        assert_eq!(ctrls[0].1.kind, RtCtrlKind::Cc(11));
        assert_eq!(ctrls[0].1.value, 100);
        assert_eq!(ctrls[0].1.tick_ticks, 192, "relative to the clip start");
    }

    /// P0: controllers tile with the clip loop exactly like notes — one
    /// occurrence per period inside the span, nothing past the span end.
    #[test]
    fn clip_ctrls_tile_with_the_loop() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // Two-bar span, one-bar loop: the same event fires once per period.
        place_ctrls(&mut seq, 0, 0, 768, 384, &[ctrl_at(0, 1, 42)]);
        seq.play();

        let ctrls = run_ctrls(&mut seq, 400);
        assert_eq!(ctrls.len(), 2, "one per loop period: {ctrls:?}");
    }

    /// P0: within one block, controllers are delivered before notes — a pitch
    /// bend must set the pitch a note starts at, not arrive a block late.
    #[test]
    fn clip_ctrls_precede_notes_in_the_same_block() {
        struct Order(Vec<&'static str>);
        impl SequencerSink for Order {
            fn note(&mut self, _track: usize, _event: NoteEvent) {
                self.0.push("note");
            }

            fn ctrl(&mut self, _track: usize, _ctrl: RtCtrl) {
                self.0.push("ctrl");
            }
        }

        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        let mut clip = RtMidiClip {
            start_ticks: 0,
            span_ticks: 384,
            loop_ticks: 384,
            ..RtMidiClip::default()
        };
        clip.push(clip_note(0, 96, 60));
        clip.push_ctrl(RtCtrl::new(0, 0, RtCtrlKind::PitchBend, 4096));
        let mut clips = empty_midi_clip_table();
        clips[0] = clip;
        let _ = seq.set_track_clips(0, clips);
        seq.play();

        let mut order = Order(Vec::new());
        seq.advance(&ctx(0), &mut order);
        assert_eq!(order.0, vec!["ctrl", "note"], "bend must land first");
    }

    /// Transport stop drains pending offs as **pitch-matched** note offs:
    /// the drained note is the clip note's number (a pitch-matching plugin
    /// such as Dexed releases by `midi_note`), and the slot clears.
    /// A long note in a looping clip retriggers cleanly: the retrigger guard
    /// releases the still-sounding occurrence **before** the next On instead
    /// of overwriting its pending off (which would leave the voice hanging
    /// forever on pitch-matching plugins).
    #[test]
    fn long_note_in_looping_clip_retriggers_with_releases() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // Two-bar span, one-bar loop, one 768-tick note.
        place(&mut seq, 0, 0, 768, 384, &[clip_note(0, 768, 60)]);
        seq.play();

        let events = run(&mut seq, 2100); // covers ticks 0..~1600 at 120 BPM
        let notes = events
            .iter()
            .filter_map(|(t, e)| match (t, e) {
                (0, NoteEvent::On { note, .. }) => Some(('o', *note)),
                (0, NoteEvent::Off { note }) => Some(('f', *note)),
                _ => None,
            })
            .collect::<Vec<_>>();
        // Occurrences at 0 and 384; occurrence 1's off must come BEFORE
        // occurrence 2's on (clean retrigger), and the last pending off
        // drains afterwards — no voice left hanging.
        assert_eq!(
            notes,
            vec![('o', 60), ('f', 60), ('o', 60), ('f', 60)],
            "retrigger must be guarded by a release: {notes:?}"
        );
    }

    #[test]
    fn stop_drains_pending_offs_by_pitch() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        place(&mut seq, 0, 0, 384, 384, &[clip_note(0, 384, 60)]);
        seq.play();

        // Advance past the note-on; the off is still pending.
        let events = run(&mut seq, 2);
        assert_eq!(ons(&events), vec![(0, 60)]);

        // take_pending_offs yields the real note number and clears the slot.
        let drained = seq.take_pending_offs();
        let notes: Vec<u8> = drained[0].iter().flatten().map(|(_, note)| *note).collect();
        assert_eq!(notes, vec![60], "pending off must carry note 60");
        assert!(
            seq.take_pending_offs()[0].iter().all(|slot| slot.is_none()),
            "slots must be cleared"
        );

        // stop() also clears, and play() resets pending offs.
        seq.take_pending_offs();
        seq.play();
        seq.stop();
        assert!(seq.take_pending_offs()[0].iter().all(|slot| slot.is_none()));
    }

    #[test]
    fn clip_notes_fire_at_their_positions_with_lengths() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        place(
            &mut seq,
            0,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[
                clip_note(0, TICKS_PER_STEP, 60),
                clip_note(TICKS_PER_STEP, 2 * TICKS_PER_STEP, 64),
            ],
        );
        seq.play();

        let events = run(&mut seq, 375);
        assert_eq!(
            ons(&events),
            vec![(0, 60), (0, 64)],
            "notes should fire in order of start"
        );
        // every note has an off (by its own length, not a gate).
        let offs = events
            .iter()
            .filter(|(_, e)| matches!(e, NoteEvent::Off { .. }))
            .count();
        assert_eq!(offs, 2);
    }

    #[test]
    fn clip_plays_at_absolute_position() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // Clip placed on the second bar (1 bar = 96_000 samples @120 BPM).
        place(
            &mut seq,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, TICKS_PER_STEP, 72)],
        );
        seq.play();

        // First bar: silence.
        assert!(
            ons(&run(&mut seq, 375)).is_empty(),
            "nothing should fire before the clip's absolute start"
        );
        // Continue into the second bar (no re-play: the transport keeps
        // running): the note fires once at its absolute position.
        let events = run(&mut seq, 400);
        assert_eq!(ons(&events), vec![(0, 72)]);
        // Position is beyond one bar and still inside the arrangement.
        assert!(seq.position_ticks() >= f64::from(TICKS_PER_BAR));
        assert!(seq.is_playing());
    }

    #[test]
    fn clip_tiles_within_span_and_stops_at_span_end() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // span = 2 × loop: the note fires twice (once per period), no third time.
        place(
            &mut seq,
            0,
            0,
            2 * TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, TICKS_PER_STEP, 60)],
        );
        seq.play();

        // 2 bars = 768 ticks = 192_000 samples = 1_500 blocks; run 3 bars worth.
        let fired: Vec<u8> = run(&mut seq, 2_255)
            .iter()
            .filter_map(|(t, e)| {
                if *t == 0 {
                    match e {
                        NoteEvent::On { note, .. } => Some(*note),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            fired,
            vec![60, 60],
            "content should tile once per loop within the span, then stop"
        );
        // Auto-stop at the arrangement end (D16: 2 bars rounded up).
        assert!(
            !seq.is_playing(),
            "transport should stop at the arrangement end"
        );
    }

    #[test]
    fn arrangement_end_is_latest_clip_end_ceiled_to_bar() {
        let mut seq = Sequencer::new(SR);
        place(
            &mut seq,
            0,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, TICKS_PER_STEP, 60)],
        );
        assert_eq!(seq.arrangement_end_ticks(), TICKS_PER_BAR);
        // A later clip on another track extends the arrangement
        // (1.25 bar + quarter = 5 bars → ceiled to... 2 bars).
        place(
            &mut seq,
            1,
            TICKS_PER_BAR + TICKS_PER_QUARTER,
            TICKS_PER_QUARTER,
            TICKS_PER_QUARTER,
            &[],
        );
        assert_eq!(
            seq.arrangement_end_ticks(),
            2 * TICKS_PER_BAR,
            "end = latest clip end ceiled to a bar"
        );
        // Control clips count too.
        let mut clips = [RtControlClip::default(); MAX_TRACK_CLIPS];
        clips[0] = RtControlClip {
            start_ticks: 5 * TICKS_PER_BAR,
            span_ticks: TICKS_PER_BAR,
            ..RtControlClip::default()
        };
        seq.set_control_clips(0, clips);
        assert_eq!(seq.arrangement_end_ticks(), 6 * TICKS_PER_BAR);
        // Removing all content makes the arrangement unbounded again.
        let _ = seq.set_track_clips(0, empty_midi_clip_table());
        let _ = seq.set_track_clips(1, empty_midi_clip_table());
        seq.set_control_clips(0, [RtControlClip::default(); MAX_TRACK_CLIPS]);
        assert_eq!(seq.arrangement_end_ticks(), 0);
    }

    #[test]
    fn meter_changes_the_bar_rounding_and_moves_nothing() {
        let mut seq = Sequencer::new(SR);
        // 4800 ticks = 1.25 bars of 4/4, ceiled to 2 bars.
        place(
            &mut seq,
            0,
            0,
            4800,
            4800,
            &[clip_note(0, TICKS_PER_STEP, 60)],
        );
        assert_eq!(seq.arrangement_end_ticks(), 2 * TICKS_PER_BAR);
        // 3/4 (2880): the same content now spans two shorter bars.
        seq.set_time_signature(3, 4);
        assert_eq!(seq.bar_ticks(), 2880);
        assert_eq!(seq.arrangement_end_ticks(), 5760);
        // 6/8 has the same bar length as 3/4 — nothing moves.
        seq.set_time_signature(6, 8);
        assert_eq!(seq.arrangement_end_ticks(), 5760);
        // 7/8 (3360) → 2 bars = 6720.
        seq.set_time_signature(7, 8);
        assert_eq!(seq.arrangement_end_ticks(), 6720);
        // A meter change never touches the transport position.
        seq.seek(1000.0);
        seq.set_time_signature(4, 4);
        assert!((seq.position_ticks() - 1000.0).abs() < 1e-9);
        assert_eq!(seq.arrangement_end_ticks(), 2 * TICKS_PER_BAR);
        // Garbage is clamped, never a zero-length bar.
        seq.set_time_signature(0, 0);
        assert!(seq.bar_ticks() >= 1);
    }

    #[test]
    fn auto_stop_keeps_pending_offs_draining() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // One note right before the arrangement end: its off lands after the stop.
        place(
            &mut seq,
            0,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(
                TICKS_PER_BAR - TICKS_PER_STEP,
                2 * TICKS_PER_STEP,
                60,
            )],
        );
        seq.play();
        // Run up to the auto-stop. The exact block is not asserted: the
        // position counter is f64 and accumulates a sub-block error, so the
        // stop can land one block either side of the arithmetic value.
        let mut blocks = 0usize;
        while seq.is_playing() && blocks < 1_500 {
            run(&mut seq, 1);
            blocks += 1;
        }
        assert!(!seq.is_playing(), "auto-stopped at the arrangement end");

        // The note's off (one bar + 1/16) lands after the stop point: it must
        // still drain in the following blocks.
        let events = run_from(&mut seq, blocks as u64 * BLOCK as u64, 100);
        assert!(
            events
                .iter()
                .any(|(_, e)| matches!(e, NoteEvent::Off { .. })),
            "the pending off should still fire after the auto-stop"
        );
        assert!(
            events
                .iter()
                .all(|(_, e)| matches!(e, NoteEvent::Off { .. })),
            "no new note ons after the auto-stop"
        );
    }

    #[test]
    fn empty_arrangement_free_runs() {
        let mut seq = Sequencer::new(SR);
        seq.play();
        run(&mut seq, 2_000);
        assert!(
            seq.is_playing(),
            "an empty arrangement has no end: no auto-stop"
        );
        assert_eq!(seq.arrangement_end_ticks(), 0);
    }

    #[test]
    fn two_tracks_fire_on_the_same_clock() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        place(
            &mut seq,
            0,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, TICKS_PER_STEP, 60)],
        );
        place(
            &mut seq,
            1,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, TICKS_PER_STEP, 72)],
        );
        seq.play();

        // 100 blocks = 12_800 samples: covers both note offs (24 and 48 ticks
        // → 6_000 and 12_000 samples).
        let events = run(&mut seq, 100);
        assert_eq!(
            ons(&events),
            vec![(0, 60), (1, 72)],
            "both tracks fire on the same clock, in track order"
        );
        let offs = events
            .iter()
            .filter(|(_, e)| matches!(e, NoteEvent::Off { .. }))
            .count();
        assert_eq!(offs, 2, "every note should have a length-based off");
    }

    /// D15 revision: no per-clip note cap — a 300-note clip (a dense imported
    /// piano track) plays every note instead of silently losing the tail.
    #[test]
    fn dense_clip_plays_every_note() {
        let mut clip = RtMidiClip {
            span_ticks: 600,
            loop_ticks: 600,
            ..RtMidiClip::default()
        };
        for i in 0..300u32 {
            clip.push(clip_note(i * 2, 1, 60));
        }
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        let mut clips = empty_midi_clip_table();
        clips[0] = clip;
        let _ = seq.set_track_clips(0, clips);
        seq.play();

        // 130 blocks ≈ 665 ticks at 120 BPM: past the clip's 600-tick span.
        let events = run(&mut seq, 130);
        let count = events
            .iter()
            .filter(|(_, e)| matches!(e, NoteEvent::On { .. }))
            .count();
        assert_eq!(count, 300, "every note of a dense clip must fire");
    }

    #[test]
    fn bpm_change_preserves_musical_position() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        seq.play();
        run(&mut seq, 100); // 12800 samples = 51.2 ticks @120BPM
        let before = seq.position_ticks();
        seq.set_bpm(240.0);
        assert!(
            (seq.position_ticks() - before).abs() < 1e-9,
            "tempo change must not jump the position"
        );
    }

    #[test]
    fn bpm_change_alters_event_rate() {
        let mut seq = Sequencer::new(SR);
        // 16 notes per bar (every 16th note), unbounded arrangement.
        let mut clip = RtMidiClip {
            span_ticks: 256 * TICKS_PER_BAR,
            loop_ticks: TICKS_PER_BAR,
            ..RtMidiClip::default()
        };
        for i in 0..16u32 {
            clip.push(clip_note(i * TICKS_PER_STEP, TICKS_PER_STEP, 60));
        }
        let mut clips = empty_midi_clip_table();
        clips[0] = clip;
        let _ = seq.set_track_clips(0, clips);

        let count_ons = |events: &[Event]| {
            events
                .iter()
                .filter(|(_, e)| matches!(e, NoteEvent::On { .. }))
                .count()
        };
        // 1500 blocks = 192_000 samples = 2 bars @120 BPM → 32 ons;
        // at 240 BPM the same samples span 4 bars → 64 ons.
        seq.set_bpm(120.0);
        seq.play();
        let slow = count_ons(&run(&mut seq, 1_500));
        seq.set_bpm(240.0);
        seq.seek(0.0); // play resumes; rewind explicitly for the second pass
        seq.play();
        let fast = count_ons(&run(&mut seq, 1_500));
        assert_eq!(slow, 32);
        assert_eq!(fast, 64, "240 BPM should fire exactly twice as many notes");
    }

    #[test]
    fn stop_quiets_every_track() {
        let mut seq = Sequencer::new(SR);
        for track in 0..4 {
            place(
                &mut seq,
                track,
                0,
                256 * TICKS_PER_BAR,
                TICKS_PER_BAR,
                &[clip_note(0, TICKS_PER_STEP, 60)],
            );
        }
        seq.play();
        let events = run(&mut seq, 1);
        assert_eq!(
            ons(&events).len(),
            4,
            "the first four tracks should fire at position 0 simultaneously"
        );

        seq.stop();
        let events = run(&mut seq, 375);
        assert!(events.is_empty(), "no more events after stopping");
        assert!(!seq.is_playing());
    }

    #[test]
    fn note_off_comes_before_next_note_on() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // Two consecutive 16th notes: on@0, on@1/16; the first off
        // must be emitted in the block before the second on.
        place(
            &mut seq,
            0,
            0,
            256 * TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[
                clip_note(0, TICKS_PER_STEP, 60),
                clip_note(TICKS_PER_STEP, TICKS_PER_STEP, 64),
            ],
        );
        seq.play();

        let events = run(&mut seq, 60);
        let kinds: Vec<&str> = events
            .iter()
            .map(|(_, e)| match e {
                NoteEvent::On { .. } => "on",
                NoteEvent::Off { .. } => "off",
            })
            .collect();
        assert_eq!(kinds, vec!["on", "off", "on"]);
    }

    #[test]
    fn playing_and_step_feedback() {
        let mut seq = Sequencer::new(SR);
        assert!(!seq.is_playing());
        seq.play();
        assert!(seq.is_playing());
        // 5760 samples = 23.04 ticks < 24: still within step 0.
        run(&mut seq, 45);
        assert_eq!(seq.step_index(), 0);
        run(&mut seq, 5); // 6400 samples = 25.6 ticks: entering step 1
        assert_eq!(seq.step_index(), 1);
    }

    #[test]
    fn control_clips_are_independent_entities() {
        let mut seq = Sequencer::new(SR);
        let mut clips = [RtControlClip::default(); MAX_TRACK_CLIPS];
        clips[5] = RtControlClip {
            start_ticks: 0,
            span_ticks: TICKS_PER_BAR,
            loop_ticks: 96, // decoupled from any MIDI clip loop (D7)
            curve: {
                let mut curve = crate::realtime::automation::RtAutomation::default();
                curve.push(RtAutoPoint {
                    tick: 0,
                    value: 0.5,
                });
                curve
            },
        };
        seq.set_control_clips(5, clips);
        seq.set_control_clips(MAX_AUTO_TRACKS, clips); // out-of-range index ignored
        let tables = seq.control_clips();
        assert_eq!(tables[5][5].loop_ticks, 96);
        assert_eq!(tables[5][5].curve.point_count, 1);
        assert!(
            tables
                .iter()
                .enumerate()
                .all(|(i, t)| i == 5 || !t[0].curve.is_active()),
            "other control tracks should keep default empty curves"
        );
        assert!(
            tables[5]
                .iter()
                .enumerate()
                .all(|(i, c)| i == 5 || !c.curve.is_active()),
            "other slots of track 5 should stay empty"
        );
        // Control clip writes do not disturb MIDI scheduling.
        place(
            &mut seq,
            0,
            0,
            256 * TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, TICKS_PER_STEP, 60)],
        );
        seq.play();
        assert_eq!(ons(&run(&mut seq, 30)), vec![(0, 60)]);
    }

    #[test]
    fn track_count_gates_scheduling() {
        let mut seq = Sequencer::new(SR);
        place(
            &mut seq,
            5,
            0,
            256 * TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, TICKS_PER_STEP, 60)],
        );
        seq.set_track_count(4);
        seq.play();
        assert!(
            ons(&run(&mut seq, 30)).is_empty(),
            "tracks beyond the active count should not schedule"
        );
        seq.set_track_count(8);
        seq.seek(0.0); // play resumes; rewind explicitly for the retry
        seq.play();
        assert_eq!(ons(&run(&mut seq, 30)), vec![(5, 60)]);
        // Clamped to the capacity limit.
        seq.set_track_count(MAX_MIDI_TRACKS + 10);
        assert_eq!(seq.track_count(), MAX_MIDI_TRACKS);
    }

    #[test]
    fn simultaneous_notes_on_one_track_both_release() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // Two notes starting together on the same track (a chord): each must
        // get its own note off. A single per-track pending-off slot loses the
        // first one, hanging it forever on pitch-matching plugins.
        place(
            &mut seq,
            0,
            0,
            256 * TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[
                clip_note(0, TICKS_PER_STEP, 60),
                clip_note(0, TICKS_PER_STEP, 64),
            ],
        );
        seq.play();
        let events = run(&mut seq, 200);
        let ons = ons(&events);
        let offs: Vec<u8> = events
            .iter()
            .filter_map(|(_, e)| match e {
                NoteEvent::Off { note } => Some(*note),
                _ => None,
            })
            .collect();
        assert!(
            offs.contains(&60) && offs.contains(&64),
            "both simultaneous notes must receive an off; ons={ons:?} offs={offs:?}"
        );
    }

    #[test]
    fn play_resumes_and_pause_keeps_position() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        place(
            &mut seq,
            0,
            0,
            256 * TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, TICKS_PER_STEP, 60)],
        );
        seq.play();
        run(&mut seq, 750); // half a bar
        let mid = seq.position_ticks();
        assert!(mid > 0.0, "the transport should have advanced");

        seq.pause();
        assert!(!seq.is_playing());
        assert_eq!(seq.position_ticks(), mid, "pause keeps the position");
        assert!(
            run(&mut seq, 100).is_empty(),
            "a paused transport schedules nothing"
        );

        // Play resumes from the paused position (U1), not from zero.
        seq.play();
        assert!(
            seq.position_ticks() >= mid,
            "resume must continue from the paused position"
        );

        // Stop rewinds to zero.
        seq.stop();
        assert_eq!(seq.position_ticks(), 0.0);
    }

    #[test]
    fn seek_repositions_the_scheduling_window() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // A single note at the start of bar 2.
        place(
            &mut seq,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, TICKS_PER_STEP, 72)],
        );
        // Seek into bar 1 (before the clip) and play: the note fires.
        seq.seek(f64::from(TICKS_PER_QUARTER));
        seq.play();
        let events = run(&mut seq, 1_600); // ~1.3 bars
        assert_eq!(ons(&events), vec![(0, 72)]);

        // Seek past the arrangement end and play: the transport is already
        // beyond the end, so it auto-stops (D16) without scheduling.
        seq.seek(4.0 * f64::from(TICKS_PER_BAR));
        seq.play();
        assert!(ons(&run(&mut seq, 100)).is_empty());
        assert!(!seq.is_playing(), "a position past the end auto-stops");
    }

    #[test]
    fn seek_clears_pending_offs_from_the_old_position() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        place(
            &mut seq,
            0,
            0,
            256 * TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, TICKS_PER_STEP, 60)],
        );
        seq.play();
        run(&mut seq, 2); // the on fired; the off is still pending
        seq.seek(4.0 * f64::from(TICKS_PER_BAR));
        // The transport keeps playing into fresh content: the clip's note
        // re-fires at its occurrence in the new window (on) and gets its own
        // off there. The OLD pre-seek pending off must be gone — exactly one
        // on and one off means nothing leaked from the old position.
        let events = run_from(&mut seq, 2 * BLOCK as u64, 100);
        let on_count = events
            .iter()
            .filter(|(_, e)| matches!(e, NoteEvent::On { .. }))
            .count();
        let off_count = events
            .iter()
            .filter(|(_, e)| matches!(e, NoteEvent::Off { .. }))
            .count();
        assert_eq!(
            (on_count, off_count),
            (1, 1),
            "only the new position's on/off pair; the pre-seek pending off must not survive: {events:?}"
        );
    }

    #[test]
    fn song_loop_wraps_and_keeps_playing() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // One-bar clip (arrangement end = 1 bar) with a note at tick 0;
        // looping the whole bar must repeat the note every pass forever.
        place(
            &mut seq,
            0,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, TICKS_PER_STEP, 60)],
        );
        seq.set_loop_range(0, TICKS_PER_BAR);
        seq.play();
        // SR 48 kHz / 128-block: one bar = 750 blocks; 3 bars = 2_250.
        let fired = ons(&run(&mut seq, 2_250)); // three passes
        assert_eq!(fired.len(), 3, "one on per loop pass: {fired:?}");
        assert!(
            seq.is_playing(),
            "an active song loop suppresses the arrangement auto-stop"
        );
        assert!(
            seq.position_ticks() < f64::from(TICKS_PER_BAR) + 1.0,
            "the position stays inside the loop after wrapping"
        );
    }

    #[test]
    fn long_note_keeps_full_duration_across_loop_wrap() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // A note starting one 16th before the loop end whose off lands
        // beyond it: the off wraps to (start + length) - (end - start) of
        // the next pass instead of being lost.
        place(
            &mut seq,
            0,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(
                TICKS_PER_BAR - TICKS_PER_STEP,
                2 * TICKS_PER_STEP,
                60,
            )],
        );
        seq.set_loop_range(0, TICKS_PER_BAR);
        seq.play();
        let events = run(&mut seq, 1_600); // two passes + tail (bar = 750 blocks)
        let kinds: Vec<&str> = events
            .iter()
            .map(|(_, e)| match e {
                NoteEvent::On { .. } => "on",
                NoteEvent::Off { .. } => "off",
            })
            .collect();
        // Each pass: an on near the loop end, its off `length` ticks later
        // (the sample clock is continuous across the wrap, so the long note
        // keeps its full duration instead of hanging or being clipped).
        assert_eq!(
            kinds,
            vec!["on", "off", "on", "off"],
            "each pass's long note must release at its natural length: {kinds:?}"
        );
    }

    #[test]
    fn degenerate_loop_range_is_disabled() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        place(
            &mut seq,
            0,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, TICKS_PER_STEP, 60)],
        );
        // end == 0 and end <= start both mean "no loop".
        seq.set_loop_range(0, 0);
        seq.play();
        run(&mut seq, 2_255); // past the 1-bar arrangement end
        assert!(!seq.is_playing(), "no loop: the auto-stop applies");

        seq.seek(0.0);
        seq.set_loop_range(TICKS_PER_BAR, TICKS_PER_BAR); // equal: disabled
        seq.play();
        run(&mut seq, 2_255);
        assert!(!seq.is_playing(), "an equal range is a disabled loop");
    }

    /// U3b D3: the curve override wins while set, and clearing it falls back
    /// to the user's base value — never to "the last curve value".
    #[test]
    fn tempo_override_overrides_base_until_cleared() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        assert!((seq.base_bpm() - 120.0).abs() < 1e-6);
        assert!((seq.effective_bpm() - 120.0).abs() < 1e-6);

        seq.set_bpm_override(240.0);
        assert!(
            (seq.effective_bpm() - 240.0).abs() < 1e-6,
            "the curve wins over the transport field"
        );
        assert!(
            (seq.base_bpm() - 120.0).abs() < 1e-6,
            "the base value is untouched"
        );

        seq.clear_bpm_override();
        assert!(
            (seq.effective_bpm() - 120.0).abs() < 1e-6,
            "outside every tempo clip the transport returns to base"
        );

        // Curve values are clamped to the transport's own range; non-finite
        // values are ignored (the previous override or base stays).
        seq.set_bpm_override(10_000.0);
        assert!((seq.effective_bpm() - MAX_BPM).abs() < 1e-6);
        seq.set_bpm_override(f32::NAN);
        assert!(
            (seq.effective_bpm() - MAX_BPM).abs() < 1e-6,
            "a non-finite curve value must not poison the clock"
        );
    }

    /// U11 D2: audio clips convert ticks↔seconds at the **base** tempo, so
    /// `base_sec_per_tick` must ignore the tempo curve override entirely.
    #[test]
    fn base_sec_per_tick_ignores_the_tempo_curve() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        let want = 60.0 / (120.0 * 960.0);
        assert!((seq.base_sec_per_tick() - want).abs() < 1e-12);

        seq.set_bpm_override(240.0);
        assert!((seq.effective_bpm() - 240.0).abs() < 1e-6);
        assert!(
            (seq.base_sec_per_tick() - want).abs() < 1e-12,
            "audio timing must follow the base tempo, not the curve"
        );

        seq.set_bpm(60.0);
        assert!((seq.base_sec_per_tick() - 60.0 / (60.0 * 960.0)).abs() < 1e-12);
    }

    /// U3b: the override only lives while the transport is running (automation
    /// is evaluated only while playing); a pause/stop drops it so the field
    /// reads the base tempo again. `seek` keeps it — the next block re-samples
    /// at the target position (D4).
    #[test]
    fn transport_edges_drop_the_tempo_override() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);

        seq.play();
        seq.set_bpm_override(180.0);
        seq.pause();
        assert!((seq.effective_bpm() - 120.0).abs() < 1e-6, "pause: base");

        seq.play();
        seq.set_bpm_override(180.0);
        seq.stop();
        assert!((seq.effective_bpm() - 120.0).abs() < 1e-6, "stop: base");

        seq.play();
        seq.set_bpm_override(180.0);
        seq.seek(2.0 * f64::from(TICKS_PER_BAR));
        assert!(
            (seq.effective_bpm() - 180.0).abs() < 1e-6,
            "seek keeps the sampled override until the next block re-samples"
        );
    }
}
