#![forbid(unsafe_code)]
// The model is pure data and must never panic: a panic inside a command is
// caught by glitcherz-command and rolled back, but preventing it here is cheaper.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

//! # glitcherz-model
//!
//! GlitcherZ data model: MIDI tracks, automation tracks, the node graph, and the
//! project file (document layer).
//!
//! Conventions (see AGENTS.md "Data Model"):
//! - All types must be `Serialize + Deserialize` (RON format);
//! - New fields always get `#[serde(default)]` to keep old project files
//!   forward compatible;
//! - The model is the "document", the engine is the "player": this crate does
//!   not depend on `glitcherz-engine`, and the engine does not depend on this
//!   crate either -- `glitcherz-app` / `glitcherz-ui` do the conversion glue.
//!
//! v4 (spec `.agents/spec/timeline.md` §4, D14-D15):
//! - `MidiTrack { name, clips: Vec<MidiClip> }` — a track holds up to
//!   [`MAX_CLIPS_PER_TRACK`] clips placed on the arrangement timeline
//!   (absolute `start_ticks`, `span_ticks`, content loops every
//!   `loop_ticks` within the span);
//! - `AutomationTrack` becomes `ControlTrack { name, clips:
//!   Vec<ControlClip> }`, same clip placement semantics;
//! - the step sequencer is retired (`Step` / `Pattern` / `TrackSource` /
//!   per-track single `Clip` / project `gate` are gone);
//! - validation rejects over-capacity documents (report, never truncate).
//!
//! v3 (spec `.agents/spec/node-graph.md` §7, D11-D13, unchanged in v4):
//! - `GraphDoc{nodes, cables}` fully explicit: the default topology is also
//!   written into the document, and the loader rebuilds the engine graph
//!   purely from the document (D13);
//! - `node_id` is persisted with the file (D11); id 0 is invalid,
//!   id 1 = master output reserved ([`MASTER_NODE_ID`]), document nodes
//!   start from 2.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub mod file;
pub mod ops;
pub mod smf;

pub use file::{
    ModelError, PROJECT_VERSION, ProjectFile, load_project, parse_project, save_project,
};
pub use ops::{
    DEFAULT_SEED, DurationOp, QuantizeOptions, VelocityOp, apply_duration, apply_to_selected,
    apply_velocity, quantize, scale_notes, transpose,
};

/// MIDI track capacity limit (D12; the engine side uses the same value).
pub const MAX_MIDI_TRACKS: usize = 16;

/// Automation track capacity limit (D12; the engine-side `MAX_AUTO_TRACKS`
/// uses the same value).
pub const MAX_AUTO_TRACKS: usize = 16;

/// Node graph capacity limit (D1; same value on the engine side).
pub const MAX_NODES: usize = 64;

/// Cable capacity limit (D1; same value on the engine side; P5f: 256 — a
/// full 32-channel mixer consumes 32 cables on its own).
pub const MAX_CABLES: usize = 256;

/// Mixer node capacity limits (P5f D15/D21): channel count 2..=32.
pub const MIN_MIXER_CHANNELS: usize = 2;
pub const MAX_MIXER_CHANNELS: usize = 32;

/// Clips per track capacity limit (D15; the engine side uses the same value).
pub const MAX_CLIPS_PER_TRACK: usize = 8;

/// Notes per MIDI clip capacity limit (D15; the engine side
/// `MAX_CLIP_NOTES` uses the same value).
pub const MAX_CLIP_NOTES: usize = 64;

/// Anchor points per control clip capacity limit (the engine-side
/// `MAX_AUTO_POINTS` uses the same value).
pub const MAX_CURVE_POINTS: usize = 64;

/// Arrangement length safety bound in bars (D16; guards pathological
/// documents).
pub const MAX_ARRANGEMENT_BARS: u32 = 256;

/// Number of MIDI tracks in the default (new) project.
pub const DEFAULT_MIDI_TRACKS: usize = 4;

/// Reserved node_id of the master output (D6: always present, cannot be
/// deleted, not listed in the document node table).
pub const MASTER_NODE_ID: u64 = 1;

/// Default note velocity (used by serde field defaults).
///
/// Velocities are integers 1..=127, matching the MIDI wire format; the audio
/// thread divides by 127 when synthesizing.
pub const DEFAULT_VELOCITY: u8 = 100;

/// Lowest MIDI velocity (a note-off; silence).
pub const MIN_VELOCITY: u8 = 1;

/// Per-track synthesizer timbre parameters.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SynthParams {
    /// Filter cutoff frequency (Hz).
    #[serde(default = "default_cutoff")]
    pub cutoff_hz: f32,
    /// Filter resonance (Simper SVF res, 0..0.98).
    #[serde(default = "default_resonance")]
    pub resonance: f32,
    /// Filter mode: 0 = low-pass, 1 = high-pass, 2 = band-pass.
    #[serde(default)]
    pub filter_mode: f32,
    /// Envelope attack (seconds).
    #[serde(default = "default_attack")]
    pub attack_s: f32,
    /// Envelope decay (seconds).
    #[serde(default = "default_decay")]
    pub decay_s: f32,
    /// Envelope sustain (0..1).
    #[serde(default)]
    pub sustain: f32,
    /// Envelope release (seconds).
    #[serde(default = "default_release")]
    pub release_s: f32,
}

fn default_cutoff() -> f32 {
    12_000.0
}

fn default_resonance() -> f32 {
    0.2
}

fn default_attack() -> f32 {
    0.01
}

fn default_decay() -> f32 {
    0.10
}

fn default_sustain() -> f32 {
    0.8
}

fn default_release() -> f32 {
    0.20
}

impl Default for SynthParams {
    fn default() -> Self {
        Self {
            cutoff_hz: default_cutoff(),
            resonance: default_resonance(),
            filter_mode: 0.0,
            attack_s: default_attack(),
            decay_s: default_decay(),
            sustain: default_sustain(),
            release_s: default_release(),
        }
    }
}

/// Per-track delay effect parameters (`mix = 0` means bypass).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DelayParams {
    /// Delay time (seconds, 0.005..1).
    #[serde(default = "default_delay_time")]
    pub time_s: f32,
    /// Feedback amount (0..0.9).
    #[serde(default = "default_delay_feedback")]
    pub feedback: f32,
    /// Wet mix ratio (0..1).
    #[serde(default)]
    pub mix: f32,
}

fn default_delay_time() -> f32 {
    0.3
}

fn default_delay_feedback() -> f32 {
    0.35
}

impl Default for DelayParams {
    fn default() -> Self {
        Self {
            time_s: default_delay_time(),
            feedback: default_delay_feedback(),
            mix: 0.0,
        }
    }
}

/// Note time base: ticks per quarter note.
///
/// 960 PPQ is the Standard MIDI File convention and the de-facto industry
/// standard (`PPQ` in most DAWs). It divides evenly for 1/64 triplets (80
/// ticks), which a coarser base cannot express without rounding error.
pub const TICKS_PER_QUARTER: u32 = 960;

/// Ticks per step (16th note).
pub const TICKS_PER_STEP: u32 = TICKS_PER_QUARTER / 4;

/// Ticks per bar (4/4) = 16 steps × 240.
///
/// The 4/4 default, kept as the span of a freshly created clip. Reading the
/// **document's** meter goes through [`TimeSignature::ticks_per_bar`] (U3a) —
/// this constant is no longer the only source of a bar length.
pub const TICKS_PER_BAR: u32 = TICKS_PER_STEP * 16;

/// Largest number of beats per bar a [`TimeSignature`] can express.
pub const MAX_TIME_SIGNATURE_NUMERATOR: u8 = 16;

/// Musical meter (U3a): how the timeline is grouped into bars.
///
/// A meter is **notation plus a bar length**, never a data transform: changing
/// it moves no note, clip, marker or loop endpoint. Only the derived
/// [`TimeSignature::ticks_per_bar`] changes, and with it everything that
/// measures in bars — grid lines, loop snapping, arrangement rounding, the
/// ruler's bar numbers and the SMF `FF 58` meta event.
///
/// Two meters can share a bar length yet group differently (3/4 and 6/8 are
/// both 2880 ticks here): the grouping is notation, the tick count is what
/// other code reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TimeSignature {
    /// Beats per bar (`1..=`[`MAX_TIME_SIGNATURE_NUMERATOR`]).
    #[serde(default = "default_meter_numerator")]
    pub numerator: u8,
    /// The note value that counts as one beat: a power of two (1, 2, 4, 8, 16
    /// or 32 — the range the SMF `FF 58` denominator exponent can express).
    #[serde(default = "default_meter_denominator")]
    pub denominator: u8,
}

fn default_meter_numerator() -> u8 {
    4
}

fn default_meter_denominator() -> u8 {
    4
}

impl TimeSignature {
    /// 4/4 — the project default and what old (meter-less) documents load with.
    pub const COMMON: Self = Self {
        numerator: 4,
        denominator: 4,
    };

    /// Builds a meter, clamping both fields into range. The file layer and the
    /// UI both funnel through here, so a malformed value can never produce a
    /// zero-length bar; [`TimeSignature::is_valid`] is the strict check
    /// `Project::validate` runs on load.
    pub fn new(numerator: u8, denominator: u8) -> Self {
        Self {
            numerator: numerator.clamp(1, MAX_TIME_SIGNATURE_NUMERATOR),
            denominator: denominator.clamp(1, 32).next_power_of_two().min(32),
        }
    }

    /// Whether both fields are in range (no clamping performed).
    pub fn is_valid(&self) -> bool {
        (1..=MAX_TIME_SIGNATURE_NUMERATOR).contains(&self.numerator)
            && self.denominator.is_power_of_two()
            && (1..=32).contains(&self.denominator)
    }

    /// Ticks per bar: `TICKS_PER_QUARTER × 4 / denominator × numerator`.
    ///
    /// 4/4 = 3840, 3/4 = 2880, 6/8 = 2880 (the same span, grouped in three),
    /// 7/8 = 3360. Always `>= 1` (the fields are clamped on construction).
    pub fn ticks_per_bar(&self) -> u32 {
        TICKS_PER_QUARTER * 4 / u32::from(self.denominator.max(1))
            * u32::from(self.numerator.max(1))
    }

    /// Ticks in one beat (the denominator's note value) — the bar.beat readout
    /// steps by this, so 6/8 counts six beats while 3/4 counts three.
    pub fn ticks_per_beat(&self) -> u32 {
        TICKS_PER_QUARTER * 4 / u32::from(self.denominator.max(1))
    }
}

impl Default for TimeSignature {
    fn default() -> Self {
        Self::COMMON
    }
}

impl std::fmt::Display for TimeSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.numerator, self.denominator)
    }
}

/// A note block in the piano roll.
///
/// Tick positions are signed so that drag gestures can carry a negative
/// intermediate value without a clamp round-trip; they are clamped to `>= 0`
/// before reaching the audio thread (see `Note::start_clamped`).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Note {
    /// Start position (tick, relative to clip start).
    #[serde(default)]
    pub start: i64,
    /// Length (tick), always `>= 1`.
    #[serde(default = "default_note_length")]
    pub length: i64,
    /// MIDI note number.
    #[serde(default = "default_note")]
    pub note: u8,
    /// Note-on velocity, 1..=127 (MIDI wire range).
    #[serde(default = "default_velocity")]
    pub velocity: u8,
    /// MIDI channel, 0..=15.
    #[serde(default)]
    pub channel: u8,
}

fn default_note_length() -> i64 {
    i64::from(TICKS_PER_STEP)
}

fn default_note() -> u8 {
    60
}

fn default_velocity() -> u8 {
    DEFAULT_VELOCITY
}

impl Note {
    /// Tick at which the note ends (`start + length`, saturating).
    pub fn end(&self) -> i64 {
        self.start.saturating_add(self.length)
    }

    /// Start clamped into the valid range for the audio thread.
    pub fn start_clamped(&self) -> u32 {
        self.start.clamp(0, i64::from(u32::MAX)) as u32
    }

    /// Length clamped to at least one tick and into `u32` range.
    pub fn length_clamped(&self) -> u32 {
        self.length.clamp(1, i64::from(u32::MAX)) as u32
    }

    /// Velocity normalized to 0.0..=1.0 for the synthesizer.
    pub fn velocity_f32(&self) -> f32 {
        f32::from(self.velocity.clamp(MIN_VELOCITY, 127)) / 127.0
    }
}

impl Default for Note {
    fn default() -> Self {
        Self {
            start: 0,
            length: default_note_length(),
            note: default_note(),
            velocity: DEFAULT_VELOCITY,
            channel: 0,
        }
    }
}

/// Kind of a continuous / channel controller event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CtrlKind {
    /// Control change with the controller number 0..=127. Value 0..=127.
    Cc(u8),
    /// Pitch bend. Value is centred: -8192..=8191 (0 = no bend).
    PitchBend,
    /// Channel (mono) aftertouch. Value 0..=127.
    ChannelPressure,
    /// Polyphonic aftertouch for the given pitch. Value 0..=127.
    PolyPressure(u8),
    /// Program change. Value is the program number 0..=127.
    Program,
}

impl CtrlKind {
    /// Sort/group key so events of the same kind sit together.
    pub fn group_key(self) -> (u8, u8) {
        match self {
            CtrlKind::Cc(n) => (0, n),
            CtrlKind::PitchBend => (1, 0),
            CtrlKind::ChannelPressure => (2, 0),
            CtrlKind::PolyPressure(p) => (3, p),
            CtrlKind::Program => (4, 0),
        }
    }
}

/// A controller / channel event at a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CtrlEvent {
    /// Absolute position within the sequence (tick).
    pub tick: i64,
    /// MIDI channel 0..=15.
    pub channel: u8,
    /// Which controller this drives.
    pub kind: CtrlKind,
    /// Value in the controller's own range (see [`CtrlKind`]).
    pub value: i32,
}

/// The MIDI content of a clip: notes plus controller events (M1.3).
///
/// Controller events are stored alongside notes rather than inside them
/// because they are independent of pitch and length — the same model serves
/// a note-only roll today and CC lanes later.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Sequence {
    /// Notes, kept sorted by [`Sequence::sort`].
    #[serde(default)]
    pub notes: Vec<Note>,
    /// Controller events, kept sorted by tick.
    #[serde(default)]
    pub ctrls: Vec<CtrlEvent>,
}

impl Sequence {
    /// Restores the sort invariant: notes by
    /// `(start, pitch, channel, length, velocity)`, controller events by tick.
    ///
    /// **Callers holding note indices across a mutation must re-resolve them
    /// afterwards** — sorting permutes the vector, so an index-based selection
    /// is only valid for the pre-sort order.
    pub fn sort(&mut self) {
        self.notes
            .sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
        // Stable, so same-tick controller events keep their relative order.
        self.ctrls.sort_by_key(|c| c.tick);
    }

    /// The last tick covered by any note end or controller event (0 when
    /// empty).
    pub fn end_tick(&self) -> i64 {
        let notes = self.notes.iter().map(Note::end).max();
        let ctrls = self.ctrls.iter().map(|c| c.tick).max();
        match (notes, ctrls) {
            (Some(a), Some(b)) => a.max(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => 0,
        }
    }

    /// Notes whose start lies in `[start, end)`.
    pub fn notes_in(&self, start: i64, end: i64) -> impl Iterator<Item = &Note> {
        self.notes
            .iter()
            .filter(move |n| n.start >= start && n.start < end)
    }

    /// Notes that sound anywhere inside `[start, end)`.
    pub fn notes_overlapping(&self, start: i64, end: i64) -> impl Iterator<Item = &Note> {
        self.notes
            .iter()
            .filter(move |n| n.start < end && n.end() > start)
    }

    /// Drops events a synthesizer could never reproduce: duplicate notes (same
    /// channel, pitch and start — the longest wins) and duplicate controller
    /// events (same tick, channel and kind — the last wins, matching what a
    /// synth would end up playing). Returns how many events were removed.
    ///
    /// Imported files routinely contain these from layered takes or quantized
    /// collisions; without this pass the editor shows notes that cannot be
    /// deleted by any selection-based operation.
    pub fn remove_duplicates(&mut self) -> usize {
        let before = self.notes.len() + self.ctrls.len();

        // Longest first inside each (start, pitch, channel) group, then keep
        // the head of each run.
        self.notes.sort_by(|a, b| {
            (a.start, a.note, a.channel, b.length).cmp(&(b.start, b.note, b.channel, a.length))
        });
        self.notes.dedup_by(|later, kept| {
            later.start == kept.start && later.note == kept.note && later.channel == kept.channel
        });

        let mut kept: Vec<CtrlEvent> = Vec::with_capacity(self.ctrls.len());
        for c in self.ctrls.drain(..) {
            // Only events at the same tick (the tail of `kept`) can match.
            let slot = kept
                .iter_mut()
                .rev()
                .take_while(|k| k.tick == c.tick)
                .find(|k| k.channel == c.channel && k.kind.group_key() == c.kind.group_key());
            match slot {
                Some(slot) => *slot = c,
                None => kept.push(c),
            }
        }
        self.ctrls = kept;

        self.sort();
        before - (self.notes.len() + self.ctrls.len())
    }
}

/// A MIDI clip placed on the arrangement timeline (D14/D15).
///
/// `start_ticks` is the absolute position on the arrangement; the clip is
/// audible for `span_ticks` from there, and its note content repeats every
/// `loop_ticks` within the span (D14 future tiling semantics: the scheduler
/// already tiles, the UI presents one period for now). Sequence positions are
/// relative to the clip start.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MidiClip {
    /// Absolute start position on the arrangement (tick).
    #[serde(default)]
    pub start_ticks: u32,
    /// How long the clip stays active (tick), ≥ `loop_ticks`.
    #[serde(default = "default_clip_span")]
    pub span_ticks: u32,
    /// Content loop length (tick), defaults to one bar (4/4).
    #[serde(default = "default_clip_loop")]
    pub loop_ticks: u32,
    /// Note and controller content (relative to the clip start).
    ///
    /// Nested rather than flattened: `serde(flatten)` breaks RON's
    /// tuple-struct form, which the file tests and hand-written fixtures rely
    /// on.
    #[serde(default)]
    pub sequence: Sequence,
    /// v5 documents stored notes directly on the clip. Read-only: the loader
    /// moves them into [`MidiClip::sequence`] and rescales the ticks
    /// (96 PPQ → 960 PPQ). Never written back.
    #[serde(default, rename = "notes", skip_serializing)]
    pub legacy_notes: Vec<LegacyNote>,
}

/// A v5 note, kept only so old files can be read.
///
/// v5 stored `start`/`length` as `u32` ticks at 96 PPQ and `velocity` as
/// `f32` in 0..1; v6 uses `i64` ticks at 960 PPQ and `u8` in 1..=127.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LegacyNote {
    /// Start position (tick, relative to clip start).
    #[serde(default)]
    pub start: u32,
    /// Length (tick).
    #[serde(default = "default_legacy_note_length")]
    pub length: u32,
    /// MIDI note number.
    #[serde(default = "default_note")]
    pub note: u8,
    /// Velocity (0..1).
    #[serde(default = "default_legacy_velocity")]
    pub velocity: f32,
}

/// v5's `TICKS_PER_STEP`: a 16th note at 96 PPQ.
pub const LEGACY_TICKS_PER_STEP: u32 = 24;

fn default_legacy_note_length() -> u32 {
    LEGACY_TICKS_PER_STEP
}

fn default_legacy_velocity() -> f32 {
    0.9
}

impl LegacyNote {
    /// Rescales to the current model: ticks ×10, velocity 0..1 → 1..=127.
    pub fn upgrade(&self) -> Note {
        Note {
            start: i64::from(self.start) * 10,
            length: (i64::from(self.length) * 10).max(1),
            note: self.note,
            velocity: if self.velocity.is_finite() {
                let scaled = (self.velocity.clamp(0.0, 1.0) * 127.0).round() as i64;
                scaled.clamp(i64::from(MIN_VELOCITY), 127) as u8
            } else {
                DEFAULT_VELOCITY
            },
            channel: 0,
        }
    }
}

fn default_clip_span() -> u32 {
    TICKS_PER_BAR
}

fn default_clip_loop() -> u32 {
    TICKS_PER_BAR
}

impl Default for MidiClip {
    fn default() -> Self {
        Self {
            start_ticks: 0,
            span_ticks: default_clip_span(),
            loop_ticks: default_clip_loop(),
            sequence: Sequence::default(),
            legacy_notes: Vec::new(),
        }
    }
}

impl MidiClip {
    /// Convenience constructor: one empty bar clip starting at `start_ticks`.
    pub fn new(start_ticks: u32) -> Self {
        Self {
            start_ticks,
            ..MidiClip::default()
        }
    }

    /// Read-only access to the clip's notes.
    pub fn notes(&self) -> &[Note] {
        &self.sequence.notes
    }

    /// Mutable access to the clip's notes.
    ///
    /// The caller is responsible for restoring [`Sequence::sort`]'s invariant
    /// afterwards if the mutation can break it.
    pub fn notes_mut(&mut self) -> &mut Vec<Note> {
        &mut self.sequence.notes
    }

    /// Number of notes in the clip.
    pub fn note_count(&self) -> usize {
        self.sequence.notes.len()
    }

    /// Restores the sequence sort invariant.
    pub fn sort(&mut self) {
        self.sequence.sort();
    }
}

/// A control (automation) clip placed on the arrangement timeline (D17).
///
/// Same placement semantics as [`MidiClip`]; outside the span nothing is
/// delivered (the target keeps its last value). The delivery endpoint is
/// decided by the node graph's automation cable, not by the clip.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ControlClip {
    /// Absolute start position on the arrangement (tick).
    #[serde(default)]
    pub start_ticks: u32,
    /// How long the clip stays active (tick), ≥ `loop_ticks`.
    #[serde(default = "default_clip_span")]
    pub span_ticks: u32,
    /// Content loop length (tick), defaults to one bar (4/4).
    #[serde(default = "default_clip_loop")]
    pub loop_ticks: u32,
    /// Anchor curve (ticks relative to the clip start, ascending).
    #[serde(default)]
    pub curve: Vec<AutoPoint>,
}

impl Default for ControlClip {
    fn default() -> Self {
        Self {
            start_ticks: 0,
            span_ticks: default_clip_span(),
            loop_ticks: default_clip_loop(),
            curve: Vec::new(),
        }
    }
}

impl ControlClip {
    /// Convenience constructor: one empty bar clip starting at `start_ticks`.
    pub fn new(start_ticks: u32) -> Self {
        Self {
            start_ticks,
            ..ControlClip::default()
        }
    }

    /// Samples the value at a clip-relative tick: returns `None` when the
    /// curve is empty; linear interpolation between anchors; clamps to the
    /// endpoint values outside both ends. Requires anchors to be ordered
    /// (editing interfaces maintain this).
    pub fn value_at(&self, tick: f64) -> Option<f32> {
        if self.curve.is_empty() {
            return None;
        }
        let first = self.curve[0];
        let last = *self.curve.last()?;
        if tick <= f64::from(first.tick) {
            return Some(first.value);
        }
        if tick >= f64::from(last.tick) {
            return Some(last.value);
        }
        // Linear scan is enough: the number of anchors is finite, and the
        // call frequency is once per block per curve.
        for window in self.curve.windows(2) {
            let (a, b) = (window[0], window[1]);
            if tick < f64::from(b.tick) {
                let span = f64::from(b.tick) - f64::from(a.tick);
                let ratio = if span > 0.0 {
                    (tick - f64::from(a.tick)) / span
                } else {
                    0.0
                };
                return Some(a.value + (b.value - a.value) * ratio as f32);
            }
        }
        Some(last.value)
    }

    /// Inserts or updates an anchor (overwrites the same tick), keeping
    /// ascending tick order.
    pub fn upsert_point(&mut self, point: AutoPoint) {
        self.curve.retain(|p| p.tick != point.tick);
        let pos = self.curve.partition_point(|p| p.tick < point.tick);
        self.curve.insert(pos, point);
    }

    /// Removes the nearest anchor within the `tick` tolerance; returns
    /// `true` on a hit.
    pub fn remove_near(&mut self, tick: u32, tolerance: u32) -> bool {
        let Some(best) = self
            .curve
            .iter()
            .enumerate()
            .filter(|(_, p)| p.tick.abs_diff(tick) <= tolerance)
            .min_by_key(|(_, p)| p.tick.abs_diff(tick))
            .map(|(index, _)| index)
        else {
            return false;
        };
        self.curve.remove(best);
        true
    }
}

/// Node voice: built-in (synthesizer/delay) or CLAP / VST3 plugin.
///
/// Semantics depend on the node type: `Builtin` on an `Instrument` node =
/// built-in synthesizer, `Builtin` on an `Effect` node = built-in delay.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VoiceSpec {
    /// Built-in (default, forward compatible with old project files).
    #[default]
    Builtin,
    /// SoundFont file (.sf2/.sf3): file path + `bank:program` preset
    /// address (spec `sf2-loader.md` D7). Sample data stays out of the
    /// document; a missing file falls back to the built-in voice with the
    /// fields kept (D8).
    SoundFont {
        /// Path to the .sf2/.sf3 file.
        #[serde(default)]
        path: String,
        /// Preset bank number (0..=255).
        #[serde(default)]
        bank: u8,
        /// Preset program number (0..=255).
        #[serde(default)]
        program: u8,
    },
    /// CLAP plugin: dynamic library path + plugin ID (instantiated on the
    /// non-real-time thread).
    Clap {
        #[serde(default)]
        path: String,
        #[serde(default)]
        plugin_id: String,
        /// Full plugin state blob (state extension), captured at save time;
        /// empty = plugin defaults. Plugin-private format (CLAP spec), the
        /// plugin itself owns any versioning inside it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        state: Vec<u8>,
    },
    /// VST3 plugin: .vst3 file/bundle path + class CID (16-byte hex).
    Vst3 {
        #[serde(default)]
        path: String,
        #[serde(default)]
        class_id: String,
        /// Full plugin state blob (FState stream), captured at save time;
        /// empty = plugin defaults. Plugin-private format.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        state: Vec<u8>,
    },
}

/// The note plane of one MIDI track (D12 v3 / v4 timeline).
///
/// Timbre/mix/effects are no longer track fields -- they are the
/// [`NodeDoc::Instrument`] / [`NodeDoc::Effect`] / [`NodeDoc::ChannelStrip`]
/// nodes in the node graph. Since v4 the track holds clips placed on the
/// arrangement timeline (the per-track single `Clip` / step pattern /
/// `TrackSource` are retired, D14).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct MidiTrack {
    /// Track name (if empty the UI shows "Track N").
    #[serde(default)]
    pub name: String,
    /// Clips on the arrangement timeline (≤ [`MAX_CLIPS_PER_TRACK`], D15).
    #[serde(default)]
    pub clips: Vec<MidiClip>,
}

/// Automatable / cable-connected node parameter (automation cable endpoint
/// payload).
///
/// D7: the automation curve itself carries no target -- the target
/// `(node_id, param)` is carried by [`CableDoc`]; this enum is the document
/// representation of `param` in it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub enum AutoParam {
    /// Track gain (linear, 0..1.5).
    #[default]
    TrackGain,
    /// Track pan (-1..1).
    TrackPan,
    /// Filter cutoff frequency (Hz, 20..18000, log axis in the UI).
    FilterCutoff,
    /// CLAP plugin parameter (bridged to automation `ParamValueEvent`; name
    /// for UI display).
    PluginParam {
        /// Plugin-side parameter ID.
        #[serde(default)]
        param_id: u32,
        /// Parameter display name (edit-time snapshot, serialized and saved).
        #[serde(default)]
        name: String,
    },
    /// Song tempo in BPM (U3b, spec `.agents/spec/tempo-meter.md`).
    ///
    /// Only valid on a cable targeting [`PortRef::TimelineIn`] — tempo is a
    /// clock, not a node parameter, so it must never land on a node's
    /// `set_param` path (D1/D7). The curve value **overrides** the transport
    /// BPM rather than adding to it (D3).
    Tempo,
}

impl AutoParam {
    /// Whether this parameter is only legal on the Timeline In endpoint.
    pub fn is_timeline_in_only(&self) -> bool {
        matches!(self, AutoParam::Tempo)
    }
}

/// Automation anchor: tick position + real parameter value.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct AutoPoint {
    /// Position (tick, relative to loop start).
    #[serde(default)]
    pub tick: u32,
    /// Parameter value (real units, not normalized).
    #[serde(default)]
    pub value: f32,
}

/// One control (automation) track (D7/D12/D17: top-level time container).
///
/// The track itself carries no target parameter and no global loop anymore --
/// delivery endpoints are decided by [`CableKind::Automation`] cables in the
/// node graph, and time is expressed by the [`ControlClip`]s placed on it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct ControlTrack {
    /// Track name (if empty the UI shows "Control N").
    #[serde(default)]
    pub name: String,
    /// Clips on the arrangement timeline (≤ [`MAX_CLIPS_PER_TRACK`], D15).
    #[serde(default)]
    pub clips: Vec<ControlClip>,
}

/// One mixer channel's console state (P5f D15/D21: a virtual channel strip —
/// the stage-console metaphor; channel state is decoupled from cabling).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MixerChannel {
    /// Channel gain (linear, 0..1.5).
    #[serde(default = "default_track_gain")]
    pub gain: f32,
    /// Channel pan (-1..1, linear pan law).
    #[serde(default)]
    pub pan: f32,
    /// Channel mute.
    #[serde(default)]
    pub muted: bool,
    /// Channel solo (any solo mutes all non-soloed channels of the mixer).
    #[serde(default)]
    pub solo: bool,
}

impl Default for MixerChannel {
    fn default() -> Self {
        Self {
            gain: default_track_gain(),
            pan: 0.0,
            muted: false,
            solo: false,
        }
    }
}

/// Document node (spec §7 `NodeDoc`): node identity in the engine graph +
/// parameter persistence.
///
/// `track` is the node's **owning track** (which track's panel controls it) --
/// the engine-side track routing table (manual params/level/keyboard
/// addressing) is assembled by it; the actual signal flow is decided by
/// [`GraphDoc::cables`], and the two are decoupled.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeDoc {
    /// Instrument node (audio source, MIDI entry). `Builtin` = built-in
    /// synthesizer.
    Instrument {
        /// Persisted node ID (≥ 2; 1 = master output reserved).
        #[serde(default)]
        node_id: u64,
        /// Owning track.
        #[serde(default)]
        track: usize,
        /// Voice specification.
        #[serde(default)]
        voice: VoiceSpec,
        /// Built-in synthesizer timbre (ignored for external plugins).
        #[serde(default)]
        synth: SynthParams,
    },
    /// Effect node (1 in, 1 out). `Builtin` = built-in delay.
    Effect {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track.
        #[serde(default)]
        track: usize,
        /// Effect specification.
        #[serde(default)]
        voice: VoiceSpec,
        /// Built-in delay parameters (ignored for external plugins).
        #[serde(default)]
        delay: DelayParams,
    },
    /// Mixer node (P5f D15/D21): the explicit summing point. Absorbs the old
    /// ChannelStrip — per-channel gain/pan/mute/solo faders + an output
    /// pan/mute stage; channels are managed by add/remove buttons
    /// ([`MIN_MIXER_CHANNELS`]..=[`MAX_MIXER_CHANNELS`]) and decoupled from
    /// cabling (unplugging never loses fader settings).
    Mixer {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track (grouping only; a mixer usually serves many tracks).
        #[serde(default)]
        track: usize,
        /// Channel (input port) count, 2..=32 (D21: add/remove buttons).
        #[serde(default = "default_mixer_inputs")]
        inputs: u8,
        /// Per-channel console state; `len() == inputs as usize`.
        #[serde(default)]
        channels: Vec<MixerChannel>,
        /// Output-stage pan (-1..1, linear law).
        #[serde(default)]
        out_pan: f32,
        /// Output-stage mute.
        #[serde(default)]
        out_muted: bool,
    },
    /// Virtual keyboard node (G6, spec `plugin-gui.md` §8): a **UI-only
    /// routing node**. It has no real-time processor and never enters the
    /// engine graph (the document node_id simply has no engine counterpart);
    /// its MIDI out cable points at an instrument so that selecting the node
    /// routes the computer / on-screen keyboard to that instrument.
    ///
    /// The node still carries `node_id` (cable endpoints address it) and
    /// `track` (patch auto-placement grouping only).
    Keyboard {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track (auto-placement grouping only).
        #[serde(default)]
        track: usize,
    },
    /// Feedback tap node (P6, spec `node-graph.md` D10): the explicit,
    /// self-documenting loop breaker. Its in-edges are summed **one block
    /// late** (the engine orders it before its tap sources), which is what
    /// exempts it from the cycle rejection — so it has N inputs (taps) and
    /// one output. `gain` (0..1) scales the delayed sum, which keeps a bare
    /// loop from diverging.
    Feedback {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track (grouping only).
        #[serde(default)]
        track: usize,
        /// Feedback amount applied to the delayed sum (0..1).
        #[serde(default = "default_feedback_gain")]
        gain: f32,
    },
    /// Legacy v4 channel strip — load-only (the v4→v5 migration converts it
    /// into a 2-channel [`NodeDoc::Mixer`]); new documents never emit it.
    ChannelStrip {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track.
        #[serde(default)]
        track: usize,
        /// Track gain (linear, 0..1.5).
        #[serde(default = "default_track_gain")]
        gain: f32,
        /// Pan (-1..1, linear pan law).
        #[serde(default)]
        pan: f32,
        /// Mute.
        #[serde(default)]
        muted: bool,
    },
}

fn default_mixer_inputs() -> u8 {
    MIN_MIXER_CHANNELS as u8
}

fn default_track_gain() -> f32 {
    0.8
}

fn default_feedback_gain() -> f32 {
    0.5
}

impl NodeDoc {
    /// Node ID.
    pub fn node_id(&self) -> u64 {
        match self {
            NodeDoc::Instrument { node_id, .. }
            | NodeDoc::Effect { node_id, .. }
            | NodeDoc::Mixer { node_id, .. }
            | NodeDoc::Feedback { node_id, .. }
            | NodeDoc::Keyboard { node_id, .. }
            | NodeDoc::ChannelStrip { node_id, .. } => *node_id,
        }
    }

    /// Owning track.
    pub fn track(&self) -> usize {
        match self {
            NodeDoc::Instrument { track, .. }
            | NodeDoc::Effect { track, .. }
            | NodeDoc::Mixer { track, .. }
            | NodeDoc::Feedback { track, .. }
            | NodeDoc::Keyboard { track, .. }
            | NodeDoc::ChannelStrip { track, .. } => *track,
        }
    }

    /// Whether this node is a mixer (the only multi-input node, D14/D15).
    pub fn is_mixer(&self) -> bool {
        matches!(self, NodeDoc::Mixer { .. })
    }

    /// Whether this node is a virtual keyboard (G6: document-only routing
    /// node — no engine processor, no audio ports).
    pub fn is_keyboard(&self) -> bool {
        matches!(self, NodeDoc::Keyboard { .. })
    }
}

/// Cable port reference (spec §7 `PortRef`).
///
/// `Master` / `TimelineIn` / `MidiTrack` / `AutoTrack` are reserved identities
/// of fixed entities (not listed in the node table; the loader/engine fills
/// them in implicitly); `Node` references a document node.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortRef {
    /// Document node (node_id).
    Node(u64),
    /// Master output (fixed node, D6).
    #[default]
    Master,
    /// Timeline MIDI track outlet (track index).
    MidiTrack(usize),
    /// Timeline automation track outlet (track index).
    AutoTrack(usize),
    /// Timeline input terminal (U3b, spec `tempo-meter.md` D1): the fixed
    /// **pure sink** mirroring the `Timeline` source. It has no node_id, no
    /// engine processor and no user-creatable instance — identity alone makes
    /// it a singleton (D2), so `validate` only constrains the cables, not the
    /// entity. v1 has a single port ([`AutoParam::Tempo`]).
    TimelineIn,
}

/// Cable type (D3): bound to the port type; type mismatches are rejected at
/// load time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CableKind {
    /// Audio cable: node output → node input / master output.
    #[default]
    Audio,
    /// MIDI cable: timeline MIDI track → instrument node.
    Midi,
    /// Automation cable: timeline automation track → node parameter port.
    Automation,
}

/// Document cable (spec §7 `CableDoc`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CableDoc {
    /// Cable type.
    #[serde(default)]
    pub kind: CableKind,
    /// Source port.
    #[serde(default)]
    pub from: PortRef,
    /// Target port.
    #[serde(default)]
    pub to: PortRef,
    /// Endpoint parameter (only used when `kind = Automation`; D7).
    #[serde(default)]
    pub param: AutoParam,
    /// Destination channel (P5f D17): mixer input port index when `to` is a
    /// mixer node (persisted so fader ↔ cable binding survives edits); 0 for
    /// every other target.
    #[serde(default)]
    pub in_port: u8,
}

impl CableDoc {
    /// Convenience constructor for audio cables.
    pub fn audio(from: impl Into<PortRef>, to: impl Into<PortRef>) -> Self {
        Self {
            kind: CableKind::Audio,
            from: from.into(),
            to: to.into(),
            param: AutoParam::default(),
            in_port: 0,
        }
    }

    /// Audio cable landing on a specific mixer channel (P5f D17).
    pub fn audio_to_channel(from: impl Into<PortRef>, to: impl Into<PortRef>, in_port: u8) -> Self {
        Self {
            in_port,
            ..CableDoc::audio(from, to)
        }
    }

    /// Convenience constructor for MIDI cables.
    pub fn midi(from_track: usize, to_node: u64) -> Self {
        Self {
            kind: CableKind::Midi,
            from: PortRef::MidiTrack(from_track),
            to: PortRef::Node(to_node),
            param: AutoParam::default(),
            in_port: 0,
        }
    }

    /// Convenience constructor for a virtual-keyboard MIDI cable (G6: vk
    /// node → instrument node; the only node→node MIDI edge).
    pub fn keyboard_midi(from_node: u64, to_node: u64) -> Self {
        Self {
            kind: CableKind::Midi,
            from: PortRef::Node(from_node),
            to: PortRef::Node(to_node),
            param: AutoParam::default(),
            in_port: 0,
        }
    }

    /// Convenience constructor for automation cables.
    pub fn automation(from_track: usize, to_node: u64, param: AutoParam) -> Self {
        Self {
            kind: CableKind::Automation,
            from: PortRef::AutoTrack(from_track),
            to: PortRef::Node(to_node),
            param,
            in_port: 0,
        }
    }

    /// Convenience constructor for a tempo cable (U3b): control track →
    /// the fixed Timeline In endpoint's `Tempo` port.
    pub fn tempo(from_track: usize) -> Self {
        Self {
            kind: CableKind::Automation,
            from: PortRef::AutoTrack(from_track),
            to: PortRef::TimelineIn,
            param: AutoParam::Tempo,
            in_port: 0,
        }
    }
}

impl From<u64> for PortRef {
    fn from(node_id: u64) -> Self {
        PortRef::Node(node_id)
    }
}

/// Document representation of the node graph (v3 final state, spec §7):
/// fully explicit nodes + cables.
///
/// The default topology (per track `voice → delay → channel strip → master
/// output` + MIDI self-connection) is also fully written into the document
/// (D13): the loader **rebuilds the engine graph purely from the document**;
/// document and engine are one and the same. The master output and the
/// timeline are fixed entities and do not appear in `nodes`; cables reference
/// them via [`PortRef::Master`] / [`PortRef::MidiTrack`] /
/// [`PortRef::AutoTrack`].
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct GraphDoc {
    /// Node table (≤ [`MAX_NODES`]; node_id unique and ≥ 2).
    #[serde(default)]
    pub nodes: Vec<NodeDoc>,
    /// Cable table (≤ [`MAX_CABLES`]).
    #[serde(default)]
    pub cables: Vec<CableDoc>,
    /// Patch canvas layout (pure view data, not part of the signal topology;
    /// missing entries fall back to UI auto-placement).
    #[serde(default)]
    pub layout: LayoutDoc,
}

/// Patch canvas layout: entity positions in graph space, egui-free
/// (`[f32; 2]` = (x, y) graph units).
///
/// Entries referencing nonexistent node ids are tolerated (the UI prunes
/// them); [`BTreeMap`] keeps serialized output deterministic.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct LayoutDoc {
    /// Document node positions (keyed by node_id).
    #[serde(default)]
    pub nodes: BTreeMap<u64, [f32; 2]>,
    /// Timeline box position.
    #[serde(default)]
    pub timeline: Option<[f32; 2]>,
    /// Master output box position.
    #[serde(default)]
    pub master: Option<[f32; 2]>,
    /// Timeline In box position (U3b).
    #[serde(default)]
    pub timeline_in: Option<[f32; 2]>,
}

impl GraphDoc {
    /// Next available node_id in the document (max + 1, at least 2).
    pub fn next_node_id(&self) -> u64 {
        self.nodes
            .iter()
            .map(NodeDoc::node_id)
            .max()
            .unwrap_or(MASTER_NODE_ID)
            .max(MASTER_NODE_ID)
            + 1
    }

    /// Finds a node by node_id.
    pub fn node_by_id(&self, node_id: u64) -> Option<&NodeDoc> {
        self.nodes.iter().find(|n| n.node_id() == node_id)
    }

    /// Finds a node by node_id (mutable).
    pub fn node_by_id_mut(&mut self, node_id: u64) -> Option<&mut NodeDoc> {
        self.nodes.iter_mut().find(|n| n.node_id() == node_id)
    }

    /// The instrument node of a track (first `Instrument` whose `track`
    /// matches).
    pub fn instrument_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::Instrument { .. }) && n.track() == track)
    }

    /// The built-in delay node of a track (document host of the delay
    /// parameters).
    pub fn builtin_delay_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes.iter().find(|n| {
            matches!(
                n,
                NodeDoc::Effect {
                    voice: VoiceSpec::Builtin,
                    ..
                }
            ) && n.track() == track
        })
    }

    /// G6 (spec `plugin-gui.md` §8): resolves the **virtual keyboard route**.
    ///
    /// Seeds are the selected [`NodeDoc::Keyboard`] nodes; their MIDI out
    /// cables (`Keyboard → node`, G6 cable rule) are walked breadth-first and
    /// the first **instrument** reached wins, picked in document order so
    /// several keyboards / several instruments resolve deterministically.
    /// Non-instrument endpoints (an effect fed by a MIDI cable, which the UI
    /// never creates) are traversed but never returned — no MIDI pass-through.
    ///
    /// `None` = the selection carries no keyboard, or no route reaches an
    /// instrument; the caller then keeps the track-driven behavior (G6: the
    /// vk route is *additive*, unselected vk nodes change nothing).
    pub fn keyboard_route(&self, selection: &[u64]) -> Option<u64> {
        let seeds: Vec<u64> = self
            .nodes
            .iter()
            .filter(|n| n.is_keyboard() && selection.contains(&n.node_id()))
            .map(NodeDoc::node_id)
            .collect();
        if seeds.is_empty() {
            return None;
        }
        let mut reached: BTreeSet<u64> = seeds.iter().copied().collect();
        let mut queue: VecDeque<u64> = seeds.into_iter().collect();
        while let Some(id) = queue.pop_front() {
            for cable in &self.cables {
                if cable.kind != CableKind::Midi || cable.from != PortRef::Node(id) {
                    continue;
                }
                if let PortRef::Node(to) = cable.to
                    && reached.insert(to)
                {
                    queue.push_back(to);
                }
            }
        }
        self.nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::Instrument { .. }) && reached.contains(&n.node_id()))
            .map(NodeDoc::node_id)
    }

    /// The first effect node of a track.
    pub fn effect_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::Effect { .. }) && n.track() == track)
    }

    /// The channel strip node of a track (legacy v4 documents only).
    pub fn channel_strip_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::ChannelStrip { .. }) && n.track() == track)
    }

    /// All mixer nodes (P5f: the explicit summing points).
    pub fn mixers(&self) -> impl Iterator<Item = &NodeDoc> {
        self.nodes.iter().filter(|n| n.is_mixer())
    }

    /// The first mixer node (the "global mixer" of the default topology).
    pub fn first_mixer(&self) -> Option<&NodeDoc> {
        self.mixers().next()
    }

    /// The first free input channel of mixer `mixer_id` (D17: cables may land
    /// on any free channel; `None` when the node is not a mixer or all
    /// channels are taken).
    pub fn first_free_channel(&self, mixer_id: u64) -> Option<u8> {
        let inputs = match self.node_by_id(mixer_id)? {
            NodeDoc::Mixer { inputs, .. } => *inputs as usize,
            _ => return None,
        };
        let used: Vec<usize> = self
            .cables
            .iter()
            .filter(|c| c.kind == CableKind::Audio && c.to == PortRef::Node(mixer_id))
            .map(|c| c.in_port as usize)
            .collect();
        (0..inputs).find(|ch| !used.contains(ch)).map(|ch| ch as u8)
    }

    /// The channel strip node of a track (mutable).
    pub fn channel_strip_of_track_mut(&mut self, track: usize) -> Option<&mut NodeDoc> {
        self.nodes
            .iter_mut()
            .find(|n| matches!(n, NodeDoc::ChannelStrip { .. }) && n.track() == track)
    }

    /// The instrument node of a track (mutable).
    pub fn instrument_of_track_mut(&mut self, track: usize) -> Option<&mut NodeDoc> {
        self.nodes
            .iter_mut()
            .find(|n| matches!(n, NodeDoc::Instrument { .. }) && n.track() == track)
    }

    /// The built-in delay node of a track (mutable).
    pub fn builtin_delay_of_track_mut(&mut self, track: usize) -> Option<&mut NodeDoc> {
        self.nodes.iter_mut().find(|n| {
            matches!(
                n,
                NodeDoc::Effect {
                    voice: VoiceSpec::Builtin,
                    ..
                }
            ) && n.track() == track
        })
    }

    /// Endpoint parameter of the cable for automation track `index`
    /// (returns `None` when not cabled).
    pub fn auto_endpoint(&self, index: usize) -> Option<AutoParam> {
        self.cables.iter().find_map(|c| {
            if c.kind == CableKind::Automation && c.from == PortRef::AutoTrack(index) {
                Some(c.param.clone())
            } else {
                None
            }
        })
    }

    /// Full endpoint of the cable for automation track `index`: the
    /// destination port (a document node, or the fixed [`PortRef::TimelineIn`])
    /// plus the parameter (U3b). `None` when not cabled.
    pub fn auto_target(&self, index: usize) -> Option<(PortRef, AutoParam)> {
        self.cables.iter().find_map(|c| {
            if c.kind == CableKind::Automation && c.from == PortRef::AutoTrack(index) {
                Some((c.to.clone(), c.param.clone()))
            } else {
                None
            }
        })
    }

    /// Whether automation track `index` is cabled to the Timeline In endpoint
    /// (U3b: song tempo).
    pub fn auto_targets_timeline_in(&self, index: usize) -> bool {
        matches!(self.auto_target(index), Some((PortRef::TimelineIn, _)))
    }

    /// Removes the cable for automation track `index` (call before changing
    /// the endpoint).
    pub fn remove_auto_cable(&mut self, index: usize) {
        self.cables
            .retain(|c| !(c.kind == CableKind::Automation && c.from == PortRef::AutoTrack(index)));
    }

    /// Load-time validation (the file layer is the last line of defense;
    /// report errors instead of truncating).
    ///
    /// `midi_count` / `auto_count` are the current track counts of the
    /// project (out-of-range port references are invalid).
    pub fn validate(&self, midi_count: usize, auto_count: usize) -> Result<(), String> {
        if self.nodes.len() > MAX_NODES {
            return Err(format!(
                "node count {} exceeds the limit {MAX_NODES}",
                self.nodes.len()
            ));
        }
        if self.cables.len() > MAX_CABLES {
            return Err(format!(
                "cable count {} exceeds the limit {MAX_CABLES}",
                self.cables.len()
            ));
        }
        for (i, node_id) in self.nodes.iter().map(NodeDoc::node_id).enumerate() {
            if node_id <= MASTER_NODE_ID {
                return Err(format!(
                    "invalid node_id = {node_id} for node #{i} (0 is invalid, 1 = master output reserved)"
                ));
            }
            if self.nodes[i + 1..].iter().any(|n| n.node_id() == node_id) {
                return Err(format!("duplicate node_id {node_id}"));
            }
        }
        let node_ids: Vec<u64> = self.nodes.iter().map(NodeDoc::node_id).collect();
        // Mixer channel integrity (P5f D15/D21).
        for node in &self.nodes {
            if let NodeDoc::Mixer {
                node_id,
                inputs,
                channels,
                ..
            } = node
            {
                let inputs = *inputs as usize;
                if !(MIN_MIXER_CHANNELS..=MAX_MIXER_CHANNELS).contains(&inputs) {
                    return Err(format!(
                        "mixer node {node_id} has {inputs} inputs (allowed {MIN_MIXER_CHANNELS}..={MAX_MIXER_CHANNELS})"
                    ));
                }
                if channels.len() != inputs {
                    return Err(format!(
                        "mixer node {node_id} has {} channels but {} inputs",
                        channels.len(),
                        inputs
                    ));
                }
            }
        }
        // Single-input discipline (P5f D14/D16): audio cables into one
        // destination — only mixers accept multiple (≤ 32 with unique
        // in_port); everything else ≤ 1, master included.
        let mut audio_in_count: std::collections::BTreeMap<u64, usize> = Default::default();
        let mut mixer_ports: std::collections::BTreeMap<u64, Vec<u8>> = Default::default();
        for cable in &self.cables {
            self.check_port(&cable.from, &node_ids, midi_count, auto_count)?;
            self.check_port(&cable.to, &node_ids, midi_count, auto_count)?;
            match cable.kind {
                CableKind::Audio => {
                    if !matches!(cable.from, PortRef::Node(_)) {
                        return Err("audio cable source port must be a node".to_string());
                    }
                    // G6: a virtual keyboard has no audio ports at all.
                    if let PortRef::Node(id) = cable.from
                        && self.node_by_id(id).is_some_and(NodeDoc::is_keyboard)
                    {
                        return Err(format!(
                            "virtual keyboard node {id} has no audio output (MIDI only)"
                        ));
                    }
                    let dest_key = match &cable.to {
                        PortRef::Node(id) => *id,
                        PortRef::Master => MASTER_NODE_ID,
                        _ => {
                            return Err(
                                "audio cable target port must be a node or the master output"
                                    .to_string(),
                            );
                        }
                    };
                    if self.node_by_id(dest_key).is_some_and(NodeDoc::is_keyboard) {
                        return Err(format!(
                            "virtual keyboard node {dest_key} has no audio input (MIDI only)"
                        ));
                    }
                    if dest_key == MASTER_NODE_ID {
                        // Master (D16): single input; in_port must be 0.
                        if cable.in_port != 0 {
                            return Err(
                                "master input has no channel ports (in_port must be 0)".to_string()
                            );
                        }
                    } else if let Some(NodeDoc::Mixer { inputs, .. }) = self.node_by_id(dest_key) {
                        if cable.in_port as usize >= *inputs as usize {
                            return Err(format!(
                                "audio cable targets mixer {dest_key} channel {} out of range (inputs = {inputs})",
                                cable.in_port
                            ));
                        }
                        let ports = mixer_ports.entry(dest_key).or_default();
                        if ports.contains(&cable.in_port) {
                            return Err(format!(
                                "mixer {dest_key} channel {} receives multiple cables",
                                cable.in_port
                            ));
                        }
                        ports.push(cable.in_port);
                    } else {
                        if cable.in_port != 0 {
                            return Err(format!(
                                "non-mixer node {dest_key} has no channel ports (in_port must be 0)"
                            ));
                        }
                    }
                    let count = audio_in_count.entry(dest_key).or_insert(0);
                    *count += 1;
                    let limit = if dest_key == MASTER_NODE_ID {
                        1
                    } else if matches!(self.node_by_id(dest_key), Some(NodeDoc::Mixer { .. })) {
                        MAX_MIXER_CHANNELS
                    } else {
                        1
                    };
                    if *count > limit {
                        return Err(format!(
                            "node {dest_key} receives {count} audio cables (limit {limit}, D14/D16: only mixers sum multiple inputs)"
                        ));
                    }
                }
                CableKind::Midi => {
                    // D3 + G6: MIDI sources are the timeline MIDI track
                    // outlets and virtual keyboard nodes (the only node kind
                    // with a MIDI out port).
                    match cable.from {
                        PortRef::MidiTrack(_) => {}
                        PortRef::Node(id) => {
                            if !self.node_by_id(id).is_some_and(NodeDoc::is_keyboard) {
                                return Err(format!(
                                    "MIDI cable source node {id} is not a virtual keyboard (D3: track outlets and vk nodes only)"
                                ));
                            }
                        }
                        _ => {
                            return Err(
                                "MIDI cable source port must be a timeline MIDI track or a virtual keyboard node"
                                    .to_string(),
                            );
                        }
                    }
                    match cable.to {
                        PortRef::Node(id) => {
                            if !self.nodes.iter().any(|n| {
                                n.node_id() == id && matches!(n, NodeDoc::Instrument { .. })
                            }) {
                                return Err(format!(
                                    "MIDI cable target node_id {id} is not an instrument node"
                                ));
                            }
                        }
                        _ => {
                            return Err(
                                "MIDI cable target port must be an instrument node".to_string()
                            );
                        }
                    }
                }
                CableKind::Automation => {
                    if !matches!(cable.from, PortRef::AutoTrack(_)) {
                        return Err(
                            "automation cable source port must be a timeline automation track"
                                .to_string(),
                        );
                    }
                    // U3b D1/D7: the only valid automation destination is a
                    // node parameter port, or the fixed Timeline In endpoint
                    // whose sole v1 port is `Tempo`. Tempo is a clock, so it
                    // must not reach a node's `set_param` path, and every
                    // other parameter must not reach the clock.
                    match cable.to {
                        PortRef::Node(_) => {
                            if cable.param.is_timeline_in_only() {
                                return Err(format!(
                                    "{:?} is only valid on the timeline input (it is a clock, not a node parameter)",
                                    cable.param
                                ));
                            }
                        }
                        PortRef::TimelineIn => {
                            if !matches!(cable.param, AutoParam::Tempo) {
                                return Err("the timeline input only accepts the tempo port (v1)"
                                    .to_string());
                            }
                        }
                        _ => {
                            return Err(
                                "automation cable target port must be a node parameter port or the timeline input"
                                    .to_string(),
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn check_port(
        &self,
        port: &PortRef,
        node_ids: &[u64],
        midi_count: usize,
        auto_count: usize,
    ) -> Result<(), String> {
        match port {
            PortRef::Node(id) => {
                if !node_ids.contains(id) {
                    return Err(format!("cable references a nonexistent node_id {id}"));
                }
            }
            PortRef::MidiTrack(index) => {
                if *index >= midi_count {
                    return Err(format!(
                        "MIDI track port {index} out of range (track count = {midi_count})"
                    ));
                }
            }
            PortRef::AutoTrack(index) => {
                if *index >= auto_count {
                    return Err(format!(
                        "automation track port {index} out of range (track count = {auto_count})"
                    ));
                }
            }
            PortRef::Master => {}
            // Fixed sink with no identity to resolve (U3b D2).
            PortRef::TimelineIn => {}
        }
        Ok(())
    }
}

/// Generates the default node graph document (P5f D18): per track
/// `voice → delay → mixer.ch[i]` + one global mixer (`inputs =
/// max(track_count, 2)`) → master output + this track's MIDI self-connection.
///
/// node_ids are allocated consecutively starting from [`MASTER_NODE_ID`] + 1.
pub fn default_graph_doc(track_count: usize) -> GraphDoc {
    let mut nodes = Vec::new();
    let mut cables = Vec::new();
    let mut next_id = MASTER_NODE_ID + 1;
    let mut delay_ids = Vec::new();
    for track in 0..track_count {
        let voice_id = next_id;
        let delay_id = next_id + 1;
        next_id += 2;
        nodes.push(NodeDoc::Instrument {
            node_id: voice_id,
            track,
            voice: VoiceSpec::Builtin,
            synth: SynthParams::default(),
        });
        nodes.push(NodeDoc::Effect {
            node_id: delay_id,
            track,
            voice: VoiceSpec::Builtin,
            delay: DelayParams::default(),
        });
        cables.push(CableDoc::audio(voice_id, delay_id));
        cables.push(CableDoc::midi(track, voice_id));
        delay_ids.push(delay_id);
    }
    // One global mixer: one channel per track (D21 minimum 2).
    let inputs = track_count.max(MIN_MIXER_CHANNELS);
    let mixer_id = next_id;
    nodes.push(NodeDoc::Mixer {
        node_id: mixer_id,
        track: 0,
        inputs: inputs as u8,
        channels: vec![MixerChannel::default(); inputs],
        out_pan: 0.0,
        out_muted: false,
    });
    for (ch, delay_id) in delay_ids.iter().enumerate() {
        cables.push(CableDoc::audio_to_channel(*delay_id, mixer_id, ch as u8));
    }
    cables.push(CableDoc::audio(mixer_id, PortRef::Master));
    GraphDoc {
        nodes,
        cables,
        layout: LayoutDoc::default(),
    }
}

/// Multi-track project document (v4).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Project {
    /// Project metadata (name, tempo, etc.).
    #[serde(default)]
    pub metadata: ProjectMetadata,
    /// Master output gain (linear).
    #[serde(default = "default_master_gain")]
    pub master_gain: f32,
    /// MIDI tracks (note plane, D12 dynamic, capped at [`MAX_MIDI_TRACKS`]).
    #[serde(default)]
    pub midi_tracks: Vec<MidiTrack>,
    /// Control (automation) tracks (D17, capped at [`MAX_AUTO_TRACKS`]).
    #[serde(default)]
    pub control_tracks: Vec<ControlTrack>,
    /// Node graph document (D13 fully explicit).
    #[serde(default)]
    pub graph: GraphDoc,
}

fn default_master_gain() -> f32 {
    0.8
}

impl Default for Project {
    fn default() -> Self {
        Self {
            metadata: ProjectMetadata::default(),
            master_gain: default_master_gain(),
            midi_tracks: vec![MidiTrack::default(); DEFAULT_MIDI_TRACKS],
            control_tracks: Vec::new(),
            graph: default_graph_doc(DEFAULT_MIDI_TRACKS),
        }
    }
}

/// Maximum arrangement length in ticks (D16 safety bound).
pub const MAX_ARRANGEMENT_TICKS: u64 = (MAX_ARRANGEMENT_BARS as u64) * (TICKS_PER_BAR as u64);

impl Project {
    /// Migrates older document shapes to the current one (v4 → v5, P5f):
    ///
    /// 1. Every legacy [`NodeDoc::ChannelStrip`] becomes a 2-channel
    ///    [`NodeDoc::Mixer`] (channel 0 carries the strip's gain/pan/mute).
    ///    Cables keep targeting the same node_id; automation cables that
    ///    targeted strip parameters (TrackGain/TrackPan) now target a mixer
    ///    node the engine ignores — acceptable for a one-time migration.
    /// 2. If the master output receives multiple audio cables (legal under
    ///    v4's ≤8 implicit summing, illegal under D16), a synthetic mixer is
    ///    inserted to absorb them: each old master cable is rewired onto its
    ///    own mixer channel.
    ///
    /// Idempotent: migrated documents pass through unchanged. Call before
    /// [`Project::validate`].
    pub fn migrate(&mut self) {
        // 1) ChannelStrip → Mixer.
        for node in &mut self.graph.nodes {
            if let NodeDoc::ChannelStrip {
                node_id,
                track,
                gain,
                pan,
                muted,
            } = node
            {
                let converted = NodeDoc::Mixer {
                    node_id: *node_id,
                    track: *track,
                    inputs: MIN_MIXER_CHANNELS as u8,
                    channels: vec![
                        MixerChannel {
                            gain: *gain,
                            pan: *pan,
                            muted: *muted,
                            solo: false,
                        },
                        MixerChannel::default(),
                    ],
                    out_pan: 0.0,
                    out_muted: false,
                };
                *node = converted;
            }
        }

        // 2) Master multi-input → synthetic mixer.
        let master_ins: Vec<usize> = self
            .graph
            .cables
            .iter()
            .enumerate()
            .filter(|(_, c)| c.kind == CableKind::Audio && c.to == PortRef::Master)
            .map(|(i, _)| i)
            .collect();
        if master_ins.len() > 1 {
            let inputs = master_ins.len().min(MAX_MIXER_CHANNELS);
            let mixer_id = self.graph.next_node_id();
            self.graph.nodes.push(NodeDoc::Mixer {
                node_id: mixer_id,
                track: 0,
                inputs: inputs as u8,
                channels: vec![MixerChannel::default(); inputs],
                out_pan: 0.0,
                out_muted: false,
            });
            for (ch, cable_idx) in master_ins.iter().enumerate().take(inputs) {
                self.graph.cables[*cable_idx].to = PortRef::Node(mixer_id);
                self.graph.cables[*cable_idx].in_port = ch as u8;
            }
            self.graph
                .cables
                .push(CableDoc::audio(mixer_id, PortRef::Master));
        }
    }

    /// Upgrades a v5 document to v6: the tick base moves from 96 PPQ to 960
    /// PPQ and notes move from the clip into [`MidiClip::sequence`].
    ///
    /// Every tick in the document is multiplied by 10 — clip placement and
    /// span, note starts and lengths, automation anchors. Values are clamped
    /// rather than rejected: a v5 document was already bounded by
    /// `MAX_ARRANGEMENT_BARS`, and the bound is expressed in bars, so the
    /// rescaled document still validates.
    ///
    /// Idempotent: a v6 document has `legacy_notes == None` and already-960
    /// ticks, so this is a no-op. Only call it for documents whose
    /// `version < 6`.
    pub fn migrate_v5_to_v6(&mut self) {
        fn scale(t: &mut u32) {
            *t = t.saturating_mul(10);
        }

        for track in &mut self.midi_tracks {
            for clip in &mut track.clips {
                scale(&mut clip.start_ticks);
                scale(&mut clip.span_ticks);
                scale(&mut clip.loop_ticks);
                if !clip.legacy_notes.is_empty() {
                    let mut seq = std::mem::take(&mut clip.sequence);
                    seq.notes = clip.legacy_notes.iter().map(LegacyNote::upgrade).collect();
                    seq.sort();
                    clip.sequence = seq;
                }
                clip.legacy_notes.clear();
            }
        }
        for track in &mut self.control_tracks {
            for clip in &mut track.clips {
                scale(&mut clip.start_ticks);
                scale(&mut clip.span_ticks);
                scale(&mut clip.loop_ticks);
                for point in &mut clip.curve {
                    scale(&mut point.tick);
                }
            }
        }
    }

    /// The document's bar length in ticks (U3a): every bar-based measure
    /// (grid, loop snapping, arrangement rounding, ruler) reads this instead
    /// of the 4/4 [`TICKS_PER_BAR`] constant.
    pub fn ticks_per_bar(&self) -> u32 {
        self.metadata.time_signature.ticks_per_bar()
    }

    /// Project-level validation: track counts, clip capacities/placement and
    /// graph structure (the file layer is the last line of defense; report
    /// errors instead of truncating).
    pub fn validate(&self) -> Result<(), String> {
        if !self.metadata.time_signature.is_valid() {
            return Err(format!(
                "invalid time signature {} (numerator 1..={MAX_TIME_SIGNATURE_NUMERATOR}, denominator a power of two up to 32)",
                self.metadata.time_signature
            ));
        }
        if self.midi_tracks.len() > MAX_MIDI_TRACKS {
            return Err(format!(
                "MIDI track count {} exceeds the limit {MAX_MIDI_TRACKS}",
                self.midi_tracks.len()
            ));
        }
        if self.control_tracks.len() > MAX_AUTO_TRACKS {
            return Err(format!(
                "control track count {} exceeds the limit {MAX_AUTO_TRACKS}",
                self.control_tracks.len()
            ));
        }
        for (ti, track) in self.midi_tracks.iter().enumerate() {
            if track.clips.len() > MAX_CLIPS_PER_TRACK {
                return Err(format!(
                    "MIDI track #{ti} holds {} clips, exceeding the limit {MAX_CLIPS_PER_TRACK}",
                    track.clips.len()
                ));
            }
            for (ci, clip) in track.clips.iter().enumerate() {
                if clip.notes().len() > MAX_CLIP_NOTES {
                    return Err(format!(
                        "MIDI track #{ti} clip #{ci} holds {} notes, exceeding the limit {MAX_CLIP_NOTES}",
                        clip.notes().len()
                    ));
                }
                check_clip_placement(ti, ci, clip.start_ticks, clip.span_ticks, clip.loop_ticks)?;
            }
        }
        for (ti, track) in self.control_tracks.iter().enumerate() {
            if track.clips.len() > MAX_CLIPS_PER_TRACK {
                return Err(format!(
                    "control track #{ti} holds {} clips, exceeding the limit {MAX_CLIPS_PER_TRACK}",
                    track.clips.len()
                ));
            }
            for (ci, clip) in track.clips.iter().enumerate() {
                if clip.curve.len() > MAX_CURVE_POINTS {
                    return Err(format!(
                        "control track #{ti} clip #{ci} holds {} anchors, exceeding the limit {MAX_CURVE_POINTS}",
                        clip.curve.len()
                    ));
                }
                check_clip_placement(ti, ci, clip.start_ticks, clip.span_ticks, clip.loop_ticks)?;
            }
        }
        if u64::from(self.metadata.loop_range.end_ticks) > MAX_ARRANGEMENT_TICKS {
            return Err(format!(
                "song loop end {} exceeds the {MAX_ARRANGEMENT_BARS}-bar arrangement bound",
                self.metadata.loop_range.end_ticks
            ));
        }
        if self.metadata.markers.len() > MAX_MARKERS {
            return Err(format!(
                "marker count {} exceeds the limit {MAX_MARKERS}",
                self.metadata.markers.len()
            ));
        }
        for (i, marker) in self.metadata.markers.iter().enumerate() {
            if marker.name.is_empty() {
                return Err(format!("marker #{i} has an empty name"));
            }
            if u64::from(marker.tick) > MAX_ARRANGEMENT_TICKS {
                return Err(format!(
                    "marker #{i} ({}) lies beyond the {MAX_ARRANGEMENT_BARS}-bar arrangement bound",
                    marker.name
                ));
            }
        }
        self.graph
            .validate(self.midi_tracks.len(), self.control_tracks.len())
    }
}

/// Shared clip placement validation (D14/D15/D16).
fn check_clip_placement(
    track: usize,
    clip: usize,
    start_ticks: u32,
    span_ticks: u32,
    loop_ticks: u32,
) -> Result<(), String> {
    if loop_ticks == 0 {
        return Err(format!(
            "track #{track} clip #{clip} has loop_ticks = 0 (must be ≥ 1)"
        ));
    }
    if span_ticks < loop_ticks {
        return Err(format!(
            "track #{track} clip #{clip} has span_ticks {span_ticks} < loop_ticks {loop_ticks}"
        ));
    }
    if u64::from(start_ticks) + u64::from(span_ticks) > MAX_ARRANGEMENT_TICKS {
        return Err(format!(
            "track #{track} clip #{clip} ends beyond the {MAX_ARRANGEMENT_BARS}-bar arrangement bound"
        ));
    }
    Ok(())
}

/// Project-level metadata.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProjectMetadata {
    /// Project name.
    #[serde(default)]
    pub name: String,
    /// Tempo (BPM). The UI and time calculations uniformly use f64.
    #[serde(default = "default_tempo")]
    pub tempo_bpm: f64,
    /// Song loop range (U1): the transport wraps from `end_ticks` back to
    /// `start_ticks` while playing. Disabled by default and whenever
    /// `end_ticks <= start_ticks`.
    #[serde(default)]
    pub loop_range: SongLoop,
    /// Timeline markers (U1): named jump targets on the arrangement ruler,
    /// kept sorted by tick. Additive field — old files load with none.
    #[serde(default)]
    pub markers: Vec<Marker>,
    /// Musical meter (U3a). Additive field: documents written before it
    /// existed load as 4/4, byte-identical to the old behavior — which is why
    /// `PROJECT_VERSION` stays at 6.
    #[serde(default)]
    pub time_signature: TimeSignature,
}

/// Song loop range (U1). See [`ProjectMetadata::loop_range`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SongLoop {
    /// Whether the range is active (the range can be kept while toggled off).
    pub enabled: bool,
    /// Loop start (ticks; inclusive).
    pub start_ticks: u32,
    /// Loop end (ticks; exclusive).
    pub end_ticks: u32,
}

impl SongLoop {
    /// Whether the range describes an actual loop (`end > start` while
    /// enabled). Mirrors the engine's degenerate-range rule.
    pub fn is_active(&self) -> bool {
        self.enabled && self.end_ticks > self.start_ticks
    }
}

/// Maximum number of timeline markers (U1).
pub const MAX_MARKERS: usize = 32;

/// One timeline marker (U1): a named jump target on the arrangement ruler.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Marker {
    /// Absolute position (ticks).
    pub tick: u32,
    /// Display name (non-empty; `Marker 1` style when created via the UI).
    pub name: String,
}

fn default_tempo() -> f64 {
    120.0
}

impl Default for ProjectMetadata {
    fn default() -> Self {
        Self {
            name: String::new(),
            tempo_bpm: default_tempo(),
            loop_range: SongLoop::default(),
            markers: Vec::new(),
            time_signature: TimeSignature::COMMON,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutoParam, AutoPoint, CableDoc, CableKind, DEFAULT_MIDI_TRACKS, GraphDoc, MASTER_NODE_ID,
        MAX_ARRANGEMENT_BARS, MAX_ARRANGEMENT_TICKS, MAX_CLIP_NOTES, MAX_CLIPS_PER_TRACK,
        MAX_NODES, MidiClip, MidiTrack, NodeDoc, PortRef, Project, SynthParams, TICKS_PER_BAR,
        TICKS_PER_QUARTER, TICKS_PER_STEP, VoiceSpec, default_graph_doc,
    };

    #[test]
    fn midi_clip_ron_roundtrip() {
        let mut clip = MidiClip {
            start_ticks: 2 * TICKS_PER_BAR,
            span_ticks: 4 * TICKS_PER_BAR,
            ..MidiClip::default()
        };
        clip.notes_mut().push(super::Note {
            start: 0,
            length: 480,
            note: 60,
            velocity: super::DEFAULT_VELOCITY,
            channel: 0,
        });
        clip.notes_mut().push(super::Note {
            start: 240,
            length: 240,
            note: 64,
            velocity: 127,
            channel: 0,
        });
        let text = ron::to_string(&clip).expect("serialization failed");
        let back: MidiClip = ron::from_str(&text).expect("deserialization failed");
        assert_eq!(clip, back);
    }

    #[test]
    fn midi_clip_partial_fields_use_defaults() {
        // Only `start_ticks` written: span/loop default to one bar.
        let back: MidiClip = ron::from_str("(start_ticks: 384)").expect("parse failed");
        assert_eq!(back.start_ticks, 384);
        assert_eq!(back.span_ticks, TICKS_PER_BAR);
        assert_eq!(back.loop_ticks, TICKS_PER_BAR);
        assert!(back.notes().is_empty());
    }

    #[test]
    fn note_partial_fields_use_defaults() {
        let back: super::Note = ron::from_str("(start: 48, note: 72)").expect("parse failed");
        assert_eq!(
            back.length,
            i64::from(TICKS_PER_STEP),
            "note length defaults to one 16th note"
        );
        assert_eq!(back.velocity, super::DEFAULT_VELOCITY);
        assert_eq!(back.channel, 0);
        assert_eq!(back.note, 72);
    }

    #[test]
    fn sequence_sort_orders_notes_deterministically() {
        use super::{CtrlEvent, CtrlKind, Sequence};
        let mut seq = Sequence {
            notes: vec![
                super::Note {
                    start: 240,
                    note: 60,
                    ..Default::default()
                },
                super::Note {
                    start: 0,
                    note: 64,
                    ..Default::default()
                },
                super::Note {
                    start: 0,
                    note: 60,
                    ..Default::default()
                },
            ],
            ctrls: vec![
                CtrlEvent {
                    tick: 480,
                    channel: 0,
                    kind: CtrlKind::Cc(7),
                    value: 100,
                },
                CtrlEvent {
                    tick: 0,
                    channel: 0,
                    kind: CtrlKind::Cc(11),
                    value: 64,
                },
            ],
        };
        seq.sort();
        let notes: Vec<(i64, u8)> = seq.notes.iter().map(|n| (n.start, n.note)).collect();
        assert_eq!(notes, vec![(0, 60), (0, 64), (240, 60)]);
        let ticks: Vec<i64> = seq.ctrls.iter().map(|c| c.tick).collect();
        assert_eq!(ticks, vec![0, 480]);
    }

    #[test]
    fn remove_duplicates_keeps_the_longest_note() {
        use super::Sequence;
        let mut seq = Sequence {
            notes: vec![
                super::Note {
                    start: 0,
                    length: 240,
                    note: 60,
                    ..Default::default()
                },
                super::Note {
                    start: 0,
                    length: 960,
                    note: 60,
                    ..Default::default()
                },
                super::Note {
                    start: 0,
                    length: 480,
                    note: 60,
                    ..Default::default()
                },
                // Same start/pitch but another channel: not a duplicate.
                super::Note {
                    start: 0,
                    length: 240,
                    note: 60,
                    channel: 1,
                    ..Default::default()
                },
            ],
            ctrls: vec![],
        };
        assert_eq!(seq.remove_duplicates(), 2, "two stacked duplicates dropped");
        assert_eq!(seq.notes.len(), 2);
        assert_eq!(seq.notes[0].length, 960, "the longest survives");
        assert_eq!(seq.notes[0].channel, 0, "sorted back to channel 0 first");
        assert_eq!(seq.notes[1].channel, 1);
    }

    #[test]
    fn remove_duplicates_keeps_the_last_controller_event() {
        use super::{CtrlEvent, CtrlKind, Sequence};
        let mut seq = Sequence {
            notes: vec![],
            ctrls: vec![
                CtrlEvent {
                    tick: 0,
                    channel: 0,
                    kind: CtrlKind::Cc(7),
                    value: 10,
                },
                CtrlEvent {
                    tick: 0,
                    channel: 0,
                    kind: CtrlKind::Cc(7),
                    value: 90,
                },
                // Different tick: kept.
                CtrlEvent {
                    tick: 0,
                    channel: 0,
                    kind: CtrlKind::Cc(8),
                    value: 50,
                },
                // Different controller: kept.
                CtrlEvent {
                    tick: 0,
                    channel: 0,
                    kind: CtrlKind::PitchBend,
                    value: 0,
                },
            ],
        };
        assert_eq!(seq.remove_duplicates(), 1);
        assert_eq!(seq.ctrls.len(), 3);
        // The later value wins — that is what a synth would end up playing.
        let cc7 = seq
            .ctrls
            .iter()
            .find(|c| c.kind == CtrlKind::Cc(7))
            .unwrap();
        assert_eq!(cc7.value, 90);
    }

    #[test]
    fn sequence_queries_respect_the_half_open_range() {
        use super::Sequence;
        let seq = Sequence {
            notes: vec![
                super::Note {
                    start: 0,
                    length: 100,
                    note: 60,
                    ..Default::default()
                },
                super::Note {
                    start: 100,
                    length: 100,
                    note: 62,
                    ..Default::default()
                },
                super::Note {
                    start: 200,
                    length: 100,
                    note: 64,
                    ..Default::default()
                },
            ],
            ctrls: vec![],
        };
        // `notes_in` keys on the start tick: the note starting exactly at the
        // upper bound is excluded.
        assert_eq!(seq.notes_in(0, 200).count(), 2);
        // `notes_overlapping` keys on sounding anywhere inside: the note
        // starting at 100 is still sounding at the boundary.
        assert_eq!(seq.notes_overlapping(0, 200).count(), 2);
        assert_eq!(seq.notes_overlapping(150, 250).count(), 2);
        assert_eq!(seq.end_tick(), 300);
    }

    #[test]
    fn empty_sequence_end_tick_is_zero() {
        assert_eq!(super::Sequence::default().end_tick(), 0);
    }

    #[test]
    fn tick_base_divides_note_values_exactly() {
        // M1: the whole reason for 960 PPQ — every common subdivision must land
        // on a whole tick, including triplets (which a 96 PPQ base cannot do).
        assert_eq!(TICKS_PER_QUARTER, 960);
        assert_eq!(TICKS_PER_STEP, 240);
        assert_eq!(TICKS_PER_BAR, 3840);
        // 1/64 triplet = quarter / 6.
        assert_eq!(TICKS_PER_QUARTER / 6, 160);
        // Dotted 1/16 = 16th * 3/2.
        assert_eq!(TICKS_PER_STEP * 3 / 2, 360);
        // Triplet 1/8 = quarter / 3.
        assert_eq!(TICKS_PER_QUARTER / 3, 320);
    }

    #[test]
    fn meter_derives_the_bar_length_from_its_own_fields() {
        use super::TimeSignature;
        // 4/4 keeps the historical constant.
        assert_eq!(TimeSignature::COMMON.ticks_per_bar(), TICKS_PER_BAR);
        assert_eq!(TimeSignature::new(3, 4).ticks_per_bar(), 2880);
        // 6/8 and 3/4 span the SAME ticks but group differently: the only
        // difference a meter makes to arithmetic is the bar length.
        assert_eq!(TimeSignature::new(6, 8).ticks_per_bar(), 2880);
        assert_eq!(TimeSignature::new(7, 8).ticks_per_bar(), 3360);
        assert_eq!(TimeSignature::new(5, 4).ticks_per_bar(), 4800);
        // Beat length is the denominator's note value.
        assert_eq!(TimeSignature::new(3, 4).ticks_per_beat(), TICKS_PER_QUARTER);
        assert_eq!(TimeSignature::new(6, 8).ticks_per_beat(), 480);
    }

    #[test]
    fn meter_clamps_and_validates() {
        use super::TimeSignature;
        // Out-of-range input snaps instead of producing a zero-length bar.
        let clamped = TimeSignature::new(0, 0);
        assert_eq!(clamped.numerator, 1);
        assert_eq!(clamped.denominator, 1);
        assert!(clamped.ticks_per_bar() >= 1);
        // Non-power-of-two denominators round up to the next power of two.
        assert_eq!(TimeSignature::new(4, 3).denominator, 4);
        assert_eq!(TimeSignature::new(9, 64).denominator, 32);
        assert_eq!(TimeSignature::new(20, 4).numerator, 16);
        // Validation is strict: the parsed document must already be in range.
        assert!(TimeSignature::COMMON.is_valid());
        assert!(
            !TimeSignature {
                numerator: 0,
                denominator: 4
            }
            .is_valid()
        );
        assert!(
            !TimeSignature {
                numerator: 4,
                denominator: 3
            }
            .is_valid()
        );
        assert!(
            !TimeSignature {
                numerator: 4,
                denominator: 64
            }
            .is_valid()
        );
    }

    #[test]
    fn meter_is_additive_for_old_documents() {
        // A document written before U3a has no `time_signature` field: it must
        // load as 4/4 (no version bump, no migration branch).
        let metadata: super::ProjectMetadata =
            ron::from_str(r#"(name: "old", tempo_bpm: 120.0)"#).expect("parse");
        assert_eq!(metadata.time_signature, super::TimeSignature::COMMON);
        assert_eq!(metadata.time_signature.ticks_per_bar(), TICKS_PER_BAR);
        // A partial object (numerator only) still fills the denominator.
        let partial: super::TimeSignature = ron::from_str("(numerator: 3)").expect("parse");
        assert_eq!(partial, super::TimeSignature::new(3, 4));
    }

    #[test]
    fn note_end_uses_saturating_add() {
        let n = super::Note {
            start: i64::MAX - 2,
            length: i64::MAX,
            ..super::Note::default()
        };
        assert_eq!(n.end(), i64::MAX, "end() saturates instead of overflowing");
    }

    #[test]
    fn note_clamps_signed_drag_values_for_the_audio_thread() {
        // Drag gestures produce negative intermediates; the audio thread only
        // ever sees non-negative, in-range values.
        let n = super::Note {
            start: -500,
            length: 0,
            note: 200,
            channel: 99,
            velocity: 0,
        };
        assert_eq!(n.start_clamped(), 0, "negative start clamps to 0");
        assert_eq!(n.length_clamped(), 1, "zero length clamps to one tick");
        assert_eq!(n.velocity_f32(), f32::from(super::MIN_VELOCITY) / 127.0);
        assert_eq!(super::Note { velocity: 127, ..n }.velocity_f32(), 1.0);
    }

    #[test]
    fn default_project_graph_is_fully_explicit() {
        // D13/D18: the default document fully describes the default topology
        // (per-track voice → delay + one global mixer → master).
        let project = Project::default();
        assert_eq!(project.midi_tracks.len(), DEFAULT_MIDI_TRACKS);
        let graph = &project.graph;
        assert_eq!(
            graph.nodes.len(),
            DEFAULT_MIDI_TRACKS * 2 + 1,
            "two nodes per track + the global mixer"
        );
        // Cables: voice→delay + midi self-connection per track, then
        // delay→mixer.ch per track + mixer→master.
        assert_eq!(graph.cables.len(), DEFAULT_MIDI_TRACKS * 3 + 1);
        for t in 0..DEFAULT_MIDI_TRACKS {
            let voice = graph.instrument_of_track(t).expect("voice should exist");
            let delay = graph.builtin_delay_of_track(t).expect("delay should exist");
            assert_eq!(voice.node_id(), (MASTER_NODE_ID + 1) + (t as u64) * 2);
            let landing = graph
                .cables
                .iter()
                .find(|c| c.from == PortRef::Node(delay.node_id()))
                .expect("delay should feed the mixer");
            assert_eq!(landing.in_port, t as u8, "track t lands on channel t");
        }
        let mixer = graph.first_mixer().expect("global mixer should exist");
        assert!(matches!(mixer, NodeDoc::Mixer { inputs: 4, .. }));
        assert!(project.validate().is_ok());
    }

    #[test]
    fn project_ron_roundtrip_v5() {
        let mut project = Project::default();
        project.metadata.name = "demo".to_string();
        project.midi_tracks[0].name = "Bass".to_string();
        let mut clip = MidiClip::new(TICKS_PER_BAR);
        clip.span_ticks = 2 * TICKS_PER_BAR;
        clip.notes_mut().push(super::Note {
            start: 0,
            length: 480,
            note: 48,
            velocity: super::DEFAULT_VELOCITY,
            channel: 0,
        });
        project.midi_tracks[0].clips.push(clip);
        project.control_tracks.push(super::ControlTrack {
            name: "Gain Ride".to_string(),
            clips: vec![super::ControlClip {
                start_ticks: 0,
                span_ticks: TICKS_PER_BAR,
                loop_ticks: TICKS_PER_BAR,
                curve: vec![AutoPoint {
                    tick: 0,
                    value: 1.0,
                }],
            }],
        });
        // Control track 0 → global mixer (channel 0 gain).
        let mixer = project.graph.first_mixer().unwrap().node_id();
        project
            .graph
            .cables
            .push(CableDoc::automation(0, mixer, AutoParam::TrackGain));
        let text = ron::to_string(&project).expect("serialization failed");
        let back: Project = ron::from_str(&text).expect("deserialization failed");
        assert_eq!(project, back);
        assert!(back.validate().is_ok());
    }

    #[test]
    fn partial_project_falls_back_to_defaults() {
        // Hand-edited/future files: only metadata is written, the rest take
        // field-level defaults (empty tracks + empty graph -- whatever the
        // document says, D13).
        let back: Project = ron::from_str("(metadata: (tempo_bpm: 140))").expect("parse failed");
        assert!((back.metadata.tempo_bpm - 140.0).abs() < 1e-9);
        assert!(back.midi_tracks.is_empty());
        assert!(back.control_tracks.is_empty());
        assert!(back.graph.nodes.is_empty());
        assert!(
            back.validate().is_ok(),
            "empty tracks + empty graph is a valid document"
        );
    }

    #[test]
    fn graph_doc_validate_rejects_bad_structure() {
        // Reserved id (1 = master output).
        let graph = GraphDoc {
            nodes: vec![NodeDoc::ChannelStrip {
                node_id: MASTER_NODE_ID,
                track: 0,
                gain: 0.8,
                pan: 0.0,
                muted: false,
            }],
            cables: Vec::new(),
            ..Default::default()
        };
        assert!(graph.validate(1, 0).is_err());

        // Duplicate node_id.
        let graph = GraphDoc {
            nodes: vec![
                NodeDoc::ChannelStrip {
                    node_id: 2,
                    track: 0,
                    gain: 0.8,
                    pan: 0.0,
                    muted: false,
                },
                NodeDoc::ChannelStrip {
                    node_id: 2,
                    track: 1,
                    gain: 0.8,
                    pan: 0.0,
                    muted: false,
                },
            ],
            cables: Vec::new(),
            ..Default::default()
        };
        assert!(graph.validate(2, 0).is_err());

        // Cable references a nonexistent node.
        let graph = GraphDoc {
            nodes: Vec::new(),
            cables: vec![CableDoc::audio(9, PortRef::Master)],
            ..Default::default()
        };
        assert!(graph.validate(0, 0).is_err());

        // MIDI cable target is not an instrument.
        let graph = GraphDoc {
            nodes: vec![NodeDoc::ChannelStrip {
                node_id: 2,
                track: 0,
                gain: 0.8,
                pan: 0.0,
                muted: false,
            }],
            cables: vec![CableDoc::midi(0, 2)],
            ..Default::default()
        };
        assert!(graph.validate(1, 0).is_err());

        // Automation cable source port out of range.
        let graph = GraphDoc {
            nodes: vec![NodeDoc::ChannelStrip {
                node_id: 2,
                track: 0,
                gain: 0.8,
                pan: 0.0,
                muted: false,
            }],
            cables: vec![CableDoc::automation(3, 2, AutoParam::TrackGain)],
            ..Default::default()
        };
        assert!(graph.validate(1, 1).is_err());

        // Node capacity exceeded.
        let nodes: Vec<NodeDoc> = (0..=MAX_NODES)
            .map(|i| NodeDoc::ChannelStrip {
                node_id: 2 + i as u64,
                track: 0,
                gain: 0.8,
                pan: 0.0,
                muted: false,
            })
            .collect();
        let graph = GraphDoc {
            nodes,
            cables: Vec::new(),
            ..Default::default()
        };
        assert!(graph.validate(1, 0).is_err());
    }

    #[test]
    fn next_node_id_and_track_lookups() {
        let graph = default_graph_doc(2);
        // ids: track0 voice=2 delay=3, track1 voice=4 delay=5, mixer=6.
        assert_eq!(graph.next_node_id(), 7);
        assert_eq!(
            graph.instrument_of_track(1).map(NodeDoc::node_id),
            Some(MASTER_NODE_ID + 3)
        );
        assert!(graph.instrument_of_track(2).is_none());
        assert_eq!(graph.auto_endpoint(0), None);
    }

    #[test]
    fn voice_spec_ron_roundtrip() {
        for voice in [
            VoiceSpec::Builtin,
            VoiceSpec::SoundFont {
                path: "General.sf2".to_string(),
                bank: 0,
                program: 40,
            },
            VoiceSpec::Clap {
                path: "a.clap".to_string(),
                plugin_id: "x".to_string(),
                state: Vec::new(),
            },
            VoiceSpec::Clap {
                path: "a.clap".to_string(),
                plugin_id: "x".to_string(),
                state: vec![1, 2, 3],
            },
            VoiceSpec::Vst3 {
                path: "b.vst3".to_string(),
                class_id: "cid".to_string(),
                state: Vec::new(),
            },
            VoiceSpec::Vst3 {
                path: "b.vst3".to_string(),
                class_id: "cid".to_string(),
                state: vec![9, 8],
            },
        ] {
            let text = ron::to_string(&voice).expect("serialization failed");
            let back: VoiceSpec = ron::from_str(&text).expect("deserialization failed");
            assert_eq!(voice, back);
        }
    }

    /// Empty state must not appear in the file; old files without the field
    /// load as empty (serde default, D2).
    #[test]
    fn voice_spec_state_serde_compatibility() {
        let clap = VoiceSpec::Clap {
            path: "a.clap".to_string(),
            plugin_id: "x".to_string(),
            state: Vec::new(),
        };
        let text = ron::to_string(&clap).expect("serialization failed");
        assert!(
            !text.contains("state"),
            "empty state must be skipped: {text}"
        );
        // Legacy form (pre-state files; internally tagged, snake_case tag).
        let legacy =
            ron::from_str::<VoiceSpec>("(type: \"clap\", path: \"a.clap\", plugin_id: \"x\")")
                .expect("legacy form must deserialize");
        assert_eq!(legacy, clap);
    }

    /// SoundFont voice (spec `sf2-loader.md` D5/D6): internally tagged as
    /// `sound_font`; files without bank/program load as 0:0 (serde default,
    /// additive field -- PROJECT_VERSION untouched).
    #[test]
    fn voice_spec_soundfont_serde_compatibility() {
        let soundfont = VoiceSpec::SoundFont {
            path: "General.sf2".to_string(),
            bank: 0,
            program: 40,
        };
        let text = ron::to_string(&soundfont).expect("serialization failed");
        assert!(
            text.contains("type:\"sound_font\""),
            "tag must be snake_case sound_font: {text}"
        );
        // Older files (or hand-written projects) without bank/program: both
        // default to 0 = preset 0:0.
        let partial: VoiceSpec = ron::from_str("(type: \"sound_font\", path: \"General.sf2\")")
            .expect("fields must default");
        assert_eq!(
            partial,
            VoiceSpec::SoundFont {
                path: "General.sf2".to_string(),
                bank: 0,
                program: 0
            }
        );
    }

    #[test]
    fn control_clip_value_at_segments() {
        let mut clip = super::ControlClip {
            curve: vec![
                AutoPoint {
                    tick: 0,
                    value: 1.0,
                },
                AutoPoint {
                    tick: 96,
                    value: 0.5,
                },
                AutoPoint {
                    tick: 192,
                    value: 0.0,
                },
            ],
            ..super::ControlClip::default()
        };
        assert!((clip.value_at(0.0).unwrap() - 1.0).abs() < 1e-6);
        assert!(
            (clip.value_at(48.0).unwrap() - 0.75).abs() < 1e-6,
            "linear midpoint"
        );
        assert!((clip.value_at(192.0).unwrap()).abs() < 1e-6);
        assert!(
            (clip.value_at(1_000.0).unwrap()).abs() < 1e-6,
            "holds the end value past the tail"
        );
        assert!(
            (clip.value_at(-5.0).unwrap() - 1.0).abs() < 1e-6,
            "holds the end value before the head"
        );

        clip.curve.clear();
        assert!(
            clip.value_at(48.0).is_none(),
            "empty curve produces no automation"
        );
    }

    #[test]
    fn control_clip_upsert_keeps_order() {
        let mut clip = super::ControlClip::default();
        clip.upsert_point(AutoPoint {
            tick: 96,
            value: 0.5,
        });
        clip.upsert_point(AutoPoint {
            tick: 0,
            value: 1.0,
        });
        clip.upsert_point(AutoPoint {
            tick: 48,
            value: 0.75,
        });
        clip.upsert_point(AutoPoint {
            tick: 48,
            value: 0.8,
        }); // Overwrites the same tick
        let ticks: Vec<u32> = clip.curve.iter().map(|p| p.tick).collect();
        assert_eq!(
            ticks,
            vec![0, 48, 96],
            "anchors stay ascending and the same tick overwrites"
        );
        assert!((clip.curve[1].value - 0.8).abs() < 1e-6);
    }

    #[test]
    fn control_clip_remove_near() {
        let mut clip = super::ControlClip::default();
        clip.upsert_point(AutoPoint {
            tick: 48,
            value: 0.5,
        });
        assert!(clip.remove_near(50, 6), "should hit within tolerance");
        assert!(clip.curve.is_empty());
        assert!(
            !clip.remove_near(0, 6),
            "removal on an empty curve returns false"
        );
    }

    #[test]
    fn validate_rejects_too_many_clips() {
        let mut project = Project::default();
        for i in 0..=MAX_CLIPS_PER_TRACK {
            project.midi_tracks[0]
                .clips
                .push(MidiClip::new((i as u32) * TICKS_PER_BAR));
        }
        let err = project.validate().expect_err("should reject");
        assert!(err.contains("clips"), "unexpected error: {err}");
    }

    #[test]
    fn validate_rejects_too_many_notes() {
        let mut project = Project::default();
        let mut clip = MidiClip::default();
        for i in 0..=MAX_CLIP_NOTES {
            clip.notes_mut().push(super::Note {
                start: (i as i64) * i64::from(TICKS_PER_STEP),
                ..super::Note::default()
            });
        }
        project.midi_tracks[0].clips.push(clip);
        let err = project.validate().expect_err("should reject");
        assert!(err.contains("notes"), "unexpected error: {err}");
    }

    #[test]
    fn validate_rejects_span_shorter_than_loop() {
        let mut project = Project::default();
        project.midi_tracks[0].clips.push(MidiClip {
            start_ticks: 0,
            span_ticks: TICKS_PER_BAR,
            loop_ticks: 2 * TICKS_PER_BAR,
            ..MidiClip::default()
        });
        let err = project.validate().expect_err("should reject");
        assert!(err.contains("span_ticks"), "unexpected error: {err}");
    }

    #[test]
    fn validate_rejects_zero_loop() {
        let mut project = Project::default();
        project.midi_tracks[0].clips.push(MidiClip {
            start_ticks: 0,
            span_ticks: TICKS_PER_BAR,
            loop_ticks: 0,
            ..MidiClip::default()
        });
        assert!(project.validate().is_err());
    }

    #[test]
    fn validate_rejects_beyond_arrangement_bound() {
        let mut project = Project::default();
        project.midi_tracks[0].clips.push(MidiClip {
            start_ticks: u32::try_from(MAX_ARRANGEMENT_TICKS).unwrap(),
            span_ticks: TICKS_PER_BAR,
            loop_ticks: TICKS_PER_BAR,
            ..MidiClip::default()
        });
        let err = project.validate().expect_err("should reject");
        assert!(err.contains("bound"), "unexpected error: {err}");
    }

    #[test]
    fn validate_accepts_arrangement_ending_exactly_at_bound() {
        let mut project = Project::default();
        let last_start = u64::from(MAX_ARRANGEMENT_BARS - 1) * u64::from(TICKS_PER_BAR);
        project.midi_tracks[0].clips.push(MidiClip {
            start_ticks: u32::try_from(last_start).unwrap(),
            span_ticks: TICKS_PER_BAR,
            loop_ticks: TICKS_PER_BAR,
            ..MidiClip::default()
        });
        assert!(project.validate().is_ok());
    }

    #[test]
    fn validate_rejects_too_many_curve_points() {
        let mut project = Project::default();
        let mut clip = super::ControlClip::default();
        for i in 0..=super::MAX_CURVE_POINTS {
            clip.curve.push(AutoPoint {
                tick: (i as u32) * TICKS_PER_STEP,
                value: 0.5,
            });
        }
        project.control_tracks.push(super::ControlTrack {
            name: String::new(),
            clips: vec![clip],
        });
        let err = project.validate().expect_err("should reject");
        assert!(err.contains("anchors"), "unexpected error: {err}");
    }

    #[test]
    fn midi_track_defaults() {
        let track = MidiTrack::default();
        assert!(track.clips.is_empty());
        let _ = SynthParams::default();
    }

    // ---- G6: virtual keyboard node (spec `plugin-gui.md` §8) ----

    /// A document with two instrument nodes (ids 2 / 4) and no cables.
    fn vk_doc() -> GraphDoc {
        GraphDoc {
            nodes: vec![
                NodeDoc::Instrument {
                    node_id: 2,
                    track: 0,
                    voice: VoiceSpec::Builtin,
                    synth: SynthParams::default(),
                },
                NodeDoc::Instrument {
                    node_id: 4,
                    track: 1,
                    voice: VoiceSpec::Builtin,
                    synth: SynthParams::default(),
                },
                NodeDoc::Keyboard {
                    node_id: 6,
                    track: 0,
                },
            ],
            cables: Vec::new(),
            layout: Default::default(),
        }
    }

    #[test]
    fn keyboard_node_is_document_only_and_keeps_identity() {
        let doc = vk_doc();
        let kb = doc.node_by_id(6).expect("keyboard node present");
        assert!(kb.is_keyboard());
        assert_eq!(kb.track(), 0);
        assert!(!doc.node_by_id(2).expect("instrument").is_keyboard());

        // serde: additive tagged variant, no extra payload required.
        let text = ron::to_string(&NodeDoc::Keyboard {
            node_id: 6,
            track: 2,
        })
        .expect("serialization failed");
        assert!(text.contains("keyboard"), "tag missing: {text}");
        let back: NodeDoc = ron::from_str(&text).expect("deserialization failed");
        assert_eq!(
            back,
            NodeDoc::Keyboard {
                node_id: 6,
                track: 2
            }
        );
    }

    #[test]
    fn keyboard_route_requires_a_selected_keyboard() {
        let mut doc = vk_doc();
        doc.cables.push(CableDoc::keyboard_midi(6, 4));
        assert_eq!(
            doc.keyboard_route(&[]),
            None,
            "empty selection routes nowhere"
        );
        assert_eq!(
            doc.keyboard_route(&[2]),
            None,
            "a selected instrument is not a vk route"
        );
    }

    #[test]
    fn keyboard_route_without_cables_is_none() {
        let doc = vk_doc();
        assert_eq!(doc.keyboard_route(&[6]), None);
    }

    #[test]
    fn keyboard_route_follows_the_midi_cable_to_the_instrument() {
        let mut doc = vk_doc();
        doc.cables.push(CableDoc::keyboard_midi(6, 4));
        assert_eq!(doc.keyboard_route(&[6]), Some(4));
    }

    #[test]
    fn keyboard_route_picks_the_first_instrument_in_document_order() {
        let mut doc = vk_doc();
        // Cables added in "reverse" document order: the instrument listed
        // first (id 2) still wins.
        doc.cables.push(CableDoc::keyboard_midi(6, 4));
        doc.cables.push(CableDoc::keyboard_midi(6, 2));
        assert_eq!(doc.keyboard_route(&[6]), Some(2));
    }

    #[test]
    fn keyboard_route_ignores_non_instrument_endpoints() {
        let mut doc = vk_doc();
        doc.nodes.push(NodeDoc::Effect {
            node_id: 8,
            track: 0,
            voice: VoiceSpec::Builtin,
            delay: Default::default(),
        });
        doc.cables.push(CableDoc::keyboard_midi(6, 8));
        assert_eq!(
            doc.keyboard_route(&[6]),
            None,
            "MIDI never passes through an effect to an instrument"
        );
    }

    #[test]
    fn validate_allows_keyboard_midi_cable_only() {
        let mut doc = vk_doc();
        doc.cables.push(CableDoc::keyboard_midi(6, 2));
        assert!(doc.validate(2, 0).is_ok(), "vk → instrument MIDI is legal");

        // Audio cables touching a virtual keyboard are rejected both ways.
        let mut doc = vk_doc();
        doc.cables.push(CableDoc::audio(6, 2));
        assert!(doc.validate(2, 0).is_err(), "vk has no audio output");

        let mut doc = vk_doc();
        doc.cables.push(CableDoc::audio(2, 6));
        assert!(doc.validate(2, 0).is_err(), "vk has no audio input");

        // A MIDI cable sourced from a non-keyboard node stays illegal.
        let mut doc = vk_doc();
        doc.cables.push(CableDoc::keyboard_midi(2, 4));
        assert!(
            doc.validate(2, 0).is_err(),
            "only vk nodes have a MIDI out port"
        );
    }

    /// U3b (spec `tempo-meter.md` D1/D2/D7): the Timeline In terminal accepts
    /// exactly one thing — an automation cable carrying `Tempo`. Everything
    /// else is rejected by `validate`, and no extra singleton rule is needed
    /// (the identity is not instantiable).
    #[test]
    fn timeline_in_accepts_only_tempo_automation() {
        let mut doc = default_graph_doc(2);
        doc.cables.push(CableDoc::tempo(0));
        assert!(
            doc.validate(2, 1).is_ok(),
            "tempo → Timeline In is a legal automation cable"
        );
        assert_eq!(
            doc.auto_target(0),
            Some((PortRef::TimelineIn, AutoParam::Tempo))
        );
        assert!(doc.auto_targets_timeline_in(0));
        assert!(!doc.auto_targets_timeline_in(1), "track 1 is not cabled");

        // Audio cables have no port there (the terminal is a pure sink).
        let mut doc = default_graph_doc(2);
        doc.cables.push(CableDoc::audio(2, PortRef::TimelineIn));
        assert!(
            doc.validate(2, 0).is_err(),
            "audio cannot enter the terminal"
        );

        // Neither do MIDI cables.
        let mut doc = default_graph_doc(2);
        doc.cables.push(CableDoc {
            kind: CableKind::Midi,
            from: PortRef::MidiTrack(0),
            to: PortRef::TimelineIn,
            param: AutoParam::default(),
            in_port: 0,
        });
        assert!(
            doc.validate(2, 0).is_err(),
            "MIDI cannot enter the terminal"
        );

        // Tempo is a clock, not a node parameter: it must not reach a node.
        let mut doc = default_graph_doc(2);
        let voice = doc
            .instrument_of_track(0)
            .expect("track 0 has an instrument")
            .node_id();
        doc.cables
            .push(CableDoc::automation(0, voice, AutoParam::Tempo));
        assert!(
            doc.validate(2, 1).is_err(),
            "Tempo only exists on the Timeline In endpoint"
        );

        // v1 has a single port: other parameters cannot enter through it.
        let mut doc = default_graph_doc(2);
        doc.cables.push(CableDoc {
            kind: CableKind::Automation,
            from: PortRef::AutoTrack(0),
            to: PortRef::TimelineIn,
            param: AutoParam::TrackGain,
            in_port: 0,
        });
        assert!(
            doc.validate(2, 1).is_err(),
            "the terminal's only v1 port is Tempo"
        );
    }

    /// U3b: the new port/parameter tags are part of the saved format, so a
    /// document written now must read back identically (no version bump —
    /// both additions are behind a serde tag).
    #[test]
    fn tempo_cable_roundtrips_through_ron() {
        let cable = CableDoc::tempo(3);
        let text = ron::to_string(&cable).expect("serialize");
        assert!(text.contains("timeline_in"), "port tag: {text}");
        assert!(text.contains("Tempo"), "parameter tag: {text}");
        let back: CableDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(back, cable);
    }
}
