//! Arrangement timeline view (timeline spec T3, `.agents/spec/timeline.md`).
//!
//! Rows = tracks (MIDI + control); each row shows its clip blocks on an
//! absolute-tick ruler (bars). Interactions:
//!
//! - drag a clip body → move (snapped to bars);
//! - drag its right edge → resize (≥ 1 bar, snapped to bars);
//! - double-click an empty lane spot → insert a one-bar clip;
//! - double-click a clip → jump to the Edit tab (piano roll / curve lane);
//! - right-click a clip → delete;
//! - header: click to select the track (mute lives on the mixer node);
//! - ruler (U1): click/drag → seek; Ctrl+drag → set the song loop range;
//!   drag a loop edge → reshape it; right-click → add a marker (on a
//!   marker's flag → remove it).
//!
//! Pure view + intent: position/span edits are applied directly to the passed
//! track slices (pure document data), while everything that needs the engine
//! (re-push clips) or cross-panel state (selection, editor target) is reported
//! back as [`TimelineAction`]s.

use eframe::egui;
use egui::{
    Align2, Color32, EventFilter, FontId, Id, Key, Modifiers, Pos2, Rect, Sense, Stroke,
    StrokeKind, Vec2, pos2,
};
use glitcherz_model::{
    AudioClip, AudioTrack, AutoPoint, ControlClip, ControlTrack, CtrlEvent, MAX_ARRANGEMENT_TICKS,
    MAX_CLIPS_PER_TRACK, Marker, MidiClip, MidiTrack, Note, SongLoop,
};

/// Absolute arrangement ceiling in ticks (U3a). The bound is a **tick** count,
/// not a bar count: 256 bars at 4/4, 292 at 7/8, 341 at 3/4 — placing content
/// is limited by time, not by how the time is grouped.
const ARRANGEMENT_LIMIT_TICKS: u32 = MAX_ARRANGEMENT_TICKS as u32;

/// Fixed width of the row-header column.
pub const HEADER_WIDTH: f32 = 108.0;
/// Height of one track row at `row_zoom` = 1.
pub const ROW_HEIGHT: f32 = 28.0;
/// Row-height zoom bounds (multiplier over [`ROW_HEIGHT`]).
const ROW_ZOOM_MIN: f32 = 0.5;
const ROW_ZOOM_MAX: f32 = 3.0;
/// Horizontal zoom bounds (pixels per bar; the toolbar +/- buttons share
/// them).
pub const PX_PER_BAR_MIN: f32 = 14.0;
pub const PX_PER_BAR_MAX: f32 = 280.0;
/// Height of the bar-number ruler.
const RULER_HEIGHT: f32 = 20.0;
/// Half-width of the grab zone at a loop brace's edges (U1).
const LOOP_EDGE_GRAB: f32 = 5.0;
/// Half-width of the hit zone around a marker's flag line (U1).
const MARKER_GRAB: f32 = 6.0;
/// Half-width of the grab zone at a clip's right edge (resize handle).
const EDGE_GRAB: f32 = 6.0;

/// Accent color for selected clips (matches the roll's selection family).
const SELECTED: Color32 = Color32::from_rgb(240, 190, 80);

/// One selected clip, addressed across the arrangement (M4b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipSel {
    pub lane: TimelineLane,
    pub track: usize,
    pub clip: usize,
}

/// Which track table a clip belongs to (U6: three kinds now — the old
/// `is_control: bool` collapsed under the third value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineLane {
    Midi,
    Control,
    Audio,
}

impl TimelineLane {
    /// Kept for the sites that still reason in two tables.
    pub fn is_control(self) -> bool {
        matches!(self, TimelineLane::Control)
    }

    /// Row-key index matching [`RowKind::key`] (0 MIDI / 1 control / 2 audio).
    fn row_code(self) -> u8 {
        match self {
            TimelineLane::Midi => 0,
            TimelineLane::Control => 1,
            TimelineLane::Audio => 2,
        }
    }
}

/// One move of the G modal (M4b.3): the clip at `from` lands on `to_track`
/// (same kind only) at `start_ticks`. One action = one undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipMove {
    pub from: ClipSel,
    pub to_track: usize,
    pub start_ticks: u32,
}

/// One copy of the Shift+D modal (M4b.4): the content of the clip at
/// `from` is cloned onto `to_track` (same kind only) at `start_ticks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipDup {
    pub from: ClipSel,
    pub to_track: usize,
    pub start_ticks: u32,
}

/// One span edit of the T modal (M4b.3): the clip at `sel` gets a new
/// `start_ticks`/`span_ticks` (bar-aligned, ≥ 1 bar, same track).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipSpanEdit {
    pub sel: ClipSel,
    pub start_ticks: u32,
    pub span_ticks: u32,
}

/// Which axis a Grab / Duplicate is constrained to (Blender convention,
/// unified with the roll: X = time only, Y = rows only). Pressing the
/// same key again releases the lock; the other axis takes over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Time,
    Row,
}

/// Modal transform kind (Blender-style, M4b.3): armed by G/T, the ghost
/// follows the pointer, LMB finalizes, RMB/Esc cancels with zero cost
/// (the document is not touched until the finalize).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModalKind {
    /// G: free move — bar delta in time + row delta (same kind only).
    /// X / Y lock an axis mid-modal (unified with the roll).
    Grab { locked: Option<Axis> },
    /// Shift+D: duplicate the selection — the clones enter a grab-style
    /// modal; the originals stay untouched until the drop.
    Duplicate { locked: Option<Axis> },
    /// T: stretch the end edge of every selected clip; S mid-modal flips
    /// to the start edge (unified with the roll's t+s). Bar delta only.
    Stretch { start_edge: bool },
}

/// Live modal state. `origin` snapshots the selection at arm time; since
/// nothing mutates the document during the modal, those indices stay valid.
#[derive(Debug, Clone)]
struct Modal {
    kind: ModalKind,
    origin: Vec<ClipSel>,
    /// Pointer position when the modal was armed.
    grab: Pos2,
}

/// Which row kind a drag belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Midi(usize),
    Control(usize),
    Audio(usize),
}

impl RowKind {
    fn key(self) -> (u8, usize) {
        match self {
            RowKind::Midi(t) => (0, t),
            RowKind::Control(t) => (1, t),
            RowKind::Audio(t) => (2, t),
        }
    }

    fn lane(self) -> TimelineLane {
        match self {
            RowKind::Midi(_) => TimelineLane::Midi,
            RowKind::Control(_) => TimelineLane::Control,
            RowKind::Audio(_) => TimelineLane::Audio,
        }
    }

    fn track(self) -> usize {
        match self {
            RowKind::Midi(t) | RowKind::Control(t) | RowKind::Audio(t) => t,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    Move {
        row: RowKind,
        clip: usize,
        grab_ticks: f32,
    },
    Resize {
        row: RowKind,
        clip: usize,
    },
}

/// Rubber band in progress (screen coordinates; may span several rows).
#[derive(Debug, Clone, Copy)]
struct Marquee {
    origin: Pos2,
    current: Pos2,
    /// Shift/Ctrl held when the band started: union with the pre-existing
    /// selection instead of replacing it.
    additive: bool,
}

/// A song-loop drag on the ruler (U1): what the drag is doing plus the
/// anchor/context it needs. The final range is committed once, on
/// `drag_stopped` — one drag = one undo step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoopDrag {
    /// Ctrl+drag dragging out a brand-new range; the anchor is the press
    /// tick.
    New { anchor: f64 },
    /// Dragging the loop's start edge (the end edge stays put).
    StartEdge,
    /// Dragging the loop's end edge (the start edge stays put).
    EndEdge,
}

/// Cross-frame interaction state.
#[derive(Debug)]
pub struct TimelineState {
    /// Horizontal zoom: pixels per bar.
    pub px_per_bar: f32,
    /// Row-height zoom (multiplier over [`ROW_HEIGHT`]).
    pub row_zoom: f32,
    drag: Option<Drag>,
    /// Selected clips across tracks (M4b).
    pub selection: Vec<ClipSel>,
    /// Rubber band in progress.
    marquee: Option<Marquee>,
    /// Set by the row that sees `drag_stopped` while a band is live; the
    /// band is cleared only after show() has folded that frame's hits
    /// (later rows must still report on the final frame).
    marquee_ending: bool,
    /// Whether any lane row owned keyboard focus last frame (toolbar hint).
    had_focus: bool,
    /// The focused lane row's widget id — the app asks egui whether it
    /// still holds focus to gate the virtual piano against the command
    /// keys (G/T/K/D collide with the FL layout).
    focus_id: Option<Id>,
    /// Knife armed via K (M4b.2): the next LMB click cuts the clip under
    /// the pointer at the bar-snapped position. Stays armed for multiple
    /// cuts; Esc / RMB disarms.
    knife_armed: bool,
    /// Transient status message (why an action was refused) with an
    /// expiry time. The old one-frame version was unreadable in practice —
    /// the message flashed by faster than the eye could catch it.
    status: Option<(String, std::time::Instant)>,
    /// Live modal transform (G/T), if any.
    modal: Option<Modal>,
    /// Set when a lane row sees the modal-confirming LMB click; show()
    /// finalizes after all rows have run and clears the flag.
    modal_confirm: bool,
    /// Set when a lane row sees a modal-cancelling RMB; show() clears the
    /// modal after all rows have run.
    modal_cancel: bool,
    /// Last frame's row area in screen coordinates (header column through
    /// the last lane) — the wheel/pan navigation reads it to decide when
    /// the pointer is over the rows, so zoom can be applied *before* the
    /// ScrollArea lays out (zoom + offset compensation in one frame).
    rows_rect: Option<Rect>,
    /// Last frame's scroll viewport origin (screen coordinates) — the
    /// anchor math for pointer-anchored horizontal zoom.
    rows_viewport_min: Pos2,

    /// Track-reorder drag (header grab handle): the MIDI track index the
    /// grab started on.
    reorder: Option<usize>,
    /// Song-loop drag on the ruler (U1): `None` means an in-progress ruler
    /// drag is a plain seek scrub instead.
    loop_drag: Option<LoopDrag>,
    /// Inline track-rename editor (right-click a track name, type, Enter
    /// commits): which lane and track, plus the draft text.
    rename: Option<(TimelineLane, usize, String)>,
    /// The right-click that opens the editor lands one frame before the text
    /// field exists, so the first frame that draws it asks for focus.
    rename_focus: bool,
}

impl Default for TimelineState {
    fn default() -> Self {
        Self {
            px_per_bar: 56.0,
            row_zoom: 1.0,
            drag: None,
            selection: Vec::new(),
            marquee: None,
            marquee_ending: false,
            had_focus: false,
            focus_id: None,
            knife_armed: false,
            status: None,
            modal: None,
            modal_confirm: false,
            modal_cancel: false,
            rows_rect: None,
            rows_viewport_min: Pos2::ZERO,
            reorder: None,
            loop_drag: None,
            rename: None,
            rename_focus: false,
        }
    }
}

impl TimelineState {
    /// Whether a lane row owns keyboard focus *right now*, per egui (no
    /// frame lag, no staleness once the timeline stops being laid out).
    ///
    /// Both parts matter. egui keeps focus ON ITS ID even after the row
    /// that claimed it moved to another id (positional auto ids — see
    /// `lane_row`), so a focus-id-only check would report "focused" while
    /// every row's `has_focus` is false: the focus is parked on a phantom,
    /// all focus-gated keys are dead, and the toolbar chip — which reads
    /// the rows, not this — disagrees with what this says. Requiring a row
    /// to have reported focus keeps the three views consistent.
    pub fn owns_keyboard_focus(&self, ctx: &egui::Context) -> bool {
        self.had_focus
            && self
                .focus_id
                .is_some_and(|id| ctx.memory(|m| m.has_focus(id)))
    }
    /// Whether a modal transform (G / T / Shift+D) is live right now.
    /// The integration tests drive real egui frames and can only reach
    /// state through public accessors — same observability as the roll.
    pub fn modal_active(&self) -> bool {
        self.modal.is_some()
    }

    /// Shows a transient status message in the toolbar for a few seconds
    /// (or until another status replaces it).
    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((
            msg.into(),
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        ));
    }
}

/// One action for the caller (engine pushes / cross-panel state changes).
///
/// Clip edits carry their data (`lane` picks the track table): the
/// timeline is a view and must not mutate the document — the caller turns
/// these into undoable edits (M1.4).
#[derive(Debug, Clone, PartialEq)]
pub enum TimelineAction {
    /// A MIDI track's clips changed; re-push to the engine.
    MidiEdited(usize),
    /// A control track's clips changed; re-push to the engine.
    ControlEdited(usize),
    /// An audio track's clips changed; re-push the RT clip table (U6).
    AudioEdited(usize),
    /// Create a clip on a track at the snapped start tick.
    AddClip {
        lane: TimelineLane,
        track: usize,
        start_ticks: u32,
    },
    /// Double-click on empty audio-track space (U6 D2): the caller opens a
    /// wav file dialog and creates the clip at the (bar-snapped) click
    /// position with the span derived from the file's duration.
    AddAudioClip {
        track: usize,
        start_ticks: u32,
    },
    /// Move and/or resize a clip (one frame of a drag; the caller merges the
    /// gesture into a single undo step).
    MoveResizeClip {
        lane: TimelineLane,
        track: usize,
        clip: usize,
        start_ticks: u32,
        span_ticks: u32,
    },
    /// Delete the clip at `clip`.
    RemoveClip {
        lane: TimelineLane,
        track: usize,
        clip: usize,
    },
    /// Delete every clip in the list (one undo step; per track the indices
    /// are applied descending by the caller).
    RemoveClipsBatch {
        clips: Vec<ClipSel>,
    },
    /// Split the clip at `clip` at `at_ticks` (bar-snapped by the view):
    /// the left half keeps the start, the right half clones the content
    /// (loop-based clips both play the same loop). One undo step.
    SplitClip {
        lane: TimelineLane,
        track: usize,
        clip: usize,
        at_ticks: u32,
    },

    /// Move every clip in the list (G modal finalize; may cross tracks of
    /// the same kind). One undo step.
    MoveClipsBatch {
        moves: Vec<ClipMove>,
    },
    /// Stretch every clip in the list (T modal finalize). One undo step.
    StretchClipsBatch {
        spans: Vec<ClipSpanEdit>,
    },
    /// Clone every clip in the list onto its target (Shift+D finalize;
    /// originals untouched). One undo step.
    DuplicateClipsBatch {
        dups: Vec<ClipDup>,
    },
    SelectTrack(usize),
    SelectControlTrack(usize),
    /// Transport seek (U1): a ruler click/drag moves the playhead. Not a
    /// document edit — the caller forwards it to the engine directly.
    Seek {
        ticks: f64,
    },
    /// Set the song loop range (U1; bar-snapped by the view). One undo step;
    /// also enables the loop.
    SetLoop {
        start_ticks: u32,
        end_ticks: u32,
    },
    /// Toggle the song loop on/off (U1); the range itself is kept.
    ToggleLoop(bool),
    /// Add a marker at `tick` (U1; bar-snapped by the view, name
    /// auto-assigned "M<n>"). One undo step.
    AddMarker {
        tick: u32,
    },
    /// Remove the marker at `index` (U1). One undo step.
    RemoveMarker {
        index: usize,
    },
    /// Reorder the MIDI tracks: the track at `from` is re-inserted so it
    /// ends up just before original index `to` (an insertion index,
    /// 0..=len; `to == from` / `to == from + 1` are identity drops the
    /// caller skips). One undo step — the document track table, the
    /// per-track node `track` fields and the `MidiTrack` cable indices all
    /// move together.
    ReorderMidiTrack {
        from: usize,
        to: usize,
    },
    /// Rename a track from the inline header editor (one undo step; works for
    /// MIDI, control and audio rows). An empty name is legal: the header
    /// falls back to the generated "Track N" / "Curve N" / "Audio N" label.
    RenameTrack {
        lane: TimelineLane,
        track: usize,
        name: String,
    },
    OpenMidiEditor {
        track: usize,
        clip: usize,
    },
    /// Double-click on a control clip: open it in the Edit tab's curve editor
    /// (U12, spec `control-tracks.md` §8 D14).
    OpenControlEditor {
        track: usize,
        clip: usize,
    },
    AddMidiTrack,
    RemoveLastMidiTrack,
    /// Add a control (curve) track. One undo step.
    AddControlTrack,
    /// Remove the selected control (curve) track. One undo step.
    RemoveControlTrack,
    /// Add an audio track (U6): document track + generator node + mixer
    /// channel wiring, one undo step.
    AddAudioTrack,
    /// Remove the last audio track (U6). One undo step.
    RemoveLastAudioTrack,
    /// An audio row was clicked / double-clicked: select that track (the
    /// `- Audio` button and the wave-row highlight follow it).
    SelectAudioTrack(usize),
}

/// The MIDI clip currently open in the Edit tab, outlined on the timeline so
/// a double-click gives immediate visual feedback. Control clips edit their
/// curve in the Edit tab too (U12) but the outline is MIDI-only, so
/// `edit_target` is `None` for a control target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditHighlight {
    pub track: usize,
    pub clip: usize,
}

/// Per-frame view inputs (immutable).
pub struct TimelineParams<'a> {
    pub selected_track: usize,
    pub selected_auto: usize,
    /// Selected audio track (U6): highlights the row and receives
    /// `- Audio`.
    pub selected_audio: usize,
    pub playhead_ticks: f32,
    pub playing: bool,
    /// Clip to outline as "being edited" (from the floating editor window).
    pub edit_target: Option<EditHighlight>,
    /// Song loop range for the ruler brace (U1).
    pub loop_range: &'a SongLoop,
    /// Timeline markers for the ruler (U1).
    pub markers: &'a [Marker],
    /// Bar length in ticks from the document's meter (U3a). Everything that
    /// measures in bars — grid, snapping, knife edges, resize minimums — reads
    /// this instead of the 4/4 [`TICKS_PER_BAR`] constant.
    pub bar_ticks: u32,
    /// Seconds per tick at the project's **base** tempo (U11 D3): audio clips
    /// derive their tick footprint from their seconds window with this, and
    /// the modal geometry uses it for the same reason.
    pub sec_per_tick: f64,
    /// Waveform lookup for audio clip bodies (U6 D6): path → decoded sample
    /// (the app's session cache). `None` renders a flat placeholder body.
    pub waveform_of: &'a dyn Fn(&str) -> Option<std::sync::Arc<glitcherz_engine::SampleData>>,
}

/// Placement semantics shared by MIDI and control clips.
pub(crate) trait Placement {
    fn start(&self) -> u32;
    /// Tick extent of the clip. `sec_per_tick` is the project's **base-tempo**
    /// seconds-per-tick (U11 D3): audio derives its span from the seconds
    /// trim window, MIDI/control ignore the argument.
    fn span(&self, sec_per_tick: f64) -> u32;
    fn set_start(&mut self, v: u32);
    /// Sets the tick extent. For audio this rewrites the seconds window
    /// (native-rate playback: it changes how much material is exposed, never
    /// its speed).
    fn set_span(&mut self, v: u32, sec_per_tick: f64);
    fn set_loop(&mut self, v: u32, sec_per_tick: f64);
    /// Keeps only the content at/after `offset` (tick, relative to the clip
    /// start) and shifts it to the clip origin — the right half of a split.
    /// Notes crossing the offset continue at the cut with their tail; audio
    /// trims its head (start + offset, window offset += offset·sec_per_tick,
    /// window length −= offset·sec_per_tick).
    fn retain_from(&mut self, offset: u32, sec_per_tick: f64);
}

impl Placement for MidiClip {
    fn start(&self) -> u32 {
        self.start_ticks
    }
    fn span(&self, _sec_per_tick: f64) -> u32 {
        self.span_ticks
    }
    fn set_start(&mut self, v: u32) {
        self.start_ticks = v;
    }
    fn set_span(&mut self, v: u32, _sec_per_tick: f64) {
        self.span_ticks = v;
    }
    fn set_loop(&mut self, v: u32, _sec_per_tick: f64) {
        self.loop_ticks = v;
    }
    fn retain_from(&mut self, offset: u32, _sec_per_tick: f64) {
        let o = i64::from(offset);
        self.sequence.notes = self
            .sequence
            .notes
            .iter()
            .filter_map(|n| {
                if n.start >= o {
                    Some(Note {
                        start: n.start - o,
                        ..*n
                    })
                } else if n.end() > o {
                    Some(Note {
                        start: 0,
                        length: n.end() - o,
                        ..*n
                    })
                } else {
                    None
                }
            })
            .collect();
        self.sequence.ctrls = self
            .sequence
            .ctrls
            .iter()
            .filter(|&c| c.tick >= o)
            .map(|c| CtrlEvent {
                tick: c.tick - o,
                ..*c
            })
            .collect();
        self.sort();
    }
}

impl Placement for ControlClip {
    fn start(&self) -> u32 {
        self.start_ticks
    }
    fn span(&self, _sec_per_tick: f64) -> u32 {
        self.span_ticks
    }
    fn set_start(&mut self, v: u32) {
        self.start_ticks = v;
    }
    fn set_span(&mut self, v: u32, _sec_per_tick: f64) {
        self.span_ticks = v;
    }
    fn set_loop(&mut self, v: u32, _sec_per_tick: f64) {
        self.loop_ticks = v;
    }
    fn retain_from(&mut self, offset: u32, _sec_per_tick: f64) {
        self.curve = self
            .curve
            .iter()
            .filter(|&p| p.tick >= offset)
            .map(|p| AutoPoint {
                tick: p.tick - offset,
                ..*p
            })
            .collect();
    }
}

impl Placement for AudioClip {
    fn start(&self) -> u32 {
        self.start_ticks
    }
    // U11 D3: the tick extent is *derived* from the seconds trim window at
    // the base tempo — the document never stores it.
    fn span(&self, sec_per_tick: f64) -> u32 {
        let ticks = (self.source_len / sec_per_tick).round();
        ticks.max(1.0) as u32
    }
    fn set_start(&mut self, v: u32) {
        self.start_ticks = v;
    }
    // U11: writing the tick extent rewrites the seconds window (native-rate
    // playback → this exposes more/less material, it does not stretch it).
    fn set_span(&mut self, v: u32, sec_per_tick: f64) {
        self.source_len = f64::from(v).max(1.0) * sec_per_tick;
    }
    // Audio clips have no loop semantics (U6 D5 / U11): `loop == span` always
    // held, so the setter is a no-op kept for the shared resize path.
    fn set_loop(&mut self, _v: u32, _sec_per_tick: f64) {}
    // The right half of a split plays the wav's second half (U11): trim the
    // head — start shifts by the cut, the window start advances and the
    // window length shrinks by the same seconds.
    fn retain_from(&mut self, offset: u32, sec_per_tick: f64) {
        let secs = f64::from(offset) * sec_per_tick;
        self.start_ticks += offset;
        self.source_offset += secs;
        self.source_len = (self.source_len - secs).max(sec_per_tick);
    }
}

/// Splits `clips[index]` at `at` in place (M4b.2): the left half keeps the
/// start with a shortened span, the right half is inserted at `index + 1`
/// with the content from the cut onward shifted to its origin (notes
/// crossing the cut continue in both halves). Both halves are single-period
/// clips (`loop == span`, D14), so neither half tiles. Returns false
/// untouched when the index is out of bounds, the track is full, or `at` is
/// not strictly inside the clip.
pub(crate) fn split_clip_in<T: Placement + Clone>(
    clips: &mut Vec<T>,
    at: u32,
    index: usize,
    sec_per_tick: f64,
) -> bool {
    let Some(c) = clips.get(index) else {
        return false;
    };
    let (start, end) = (c.start(), c.start() + c.span(sec_per_tick));
    if at <= start || at >= end || clips.len() >= MAX_CLIPS_PER_TRACK {
        return false;
    }
    let left_span = at - start;
    let mut right = clips[index].clone();
    right.set_start(at);
    right.set_span(end - at, sec_per_tick);
    right.set_loop(end - at, sec_per_tick);
    right.retain_from(left_span, sec_per_tick);
    clips[index].set_span(left_span, sec_per_tick);
    clips[index].set_loop(left_span, sec_per_tick);
    clips.insert(index + 1, right);
    true
}

/// Looks up a selected clip's (start, span) across both track kinds.
fn clip_geometry(
    midi_tracks: &[MidiTrack],
    control_tracks: &[ControlTrack],
    audio_tracks: &[AudioTrack],
    sec_per_tick: f64,
    sel: ClipSel,
) -> Option<(u32, u32)> {
    match sel.lane {
        TimelineLane::Control => {
            let c = control_tracks.get(sel.track)?.clips.get(sel.clip)?;
            Some((c.start(), c.span(sec_per_tick)))
        }
        TimelineLane::Midi => {
            let c = midi_tracks.get(sel.track)?.clips.get(sel.clip)?;
            Some((c.start(), c.span(sec_per_tick)))
        }
        TimelineLane::Audio => {
            let c = audio_tracks.get(sel.track)?.clips.get(sel.clip)?;
            Some((c.start(), c.span(sec_per_tick)))
        }
    }
}

/// Clamps a row delta so the whole selection stays inside its kind's rows
/// (rigid move: one delta for everyone, clamped globally, not per clip).
fn clamp_row_delta(row_delta: i32, sel: &[ClipSel], rows: usize) -> i32 {
    if sel.is_empty() || rows == 0 {
        return 0;
    }
    let min_t = sel.iter().map(|s| s.track).min().unwrap_or(0);
    let max_t = sel.iter().map(|s| s.track).max().unwrap_or(0);
    let lo = -(min_t as i32);
    let hi = (rows - 1) as i32 - max_t as i32;
    row_delta.clamp(lo.min(0), hi.max(0))
}

/// Whether `target` (start, span) completely covers any clip in `existing`
/// (start, span) — the refused case (M4b decision: partial overlap is
/// allowed for pickups, total coverage buries an existing clip).
fn fully_covers(target: (u32, u32), existing: &[(u32, u32)]) -> bool {
    let t_end = target.0 + target.1;
    existing
        .iter()
        .any(|(s, sp)| *s >= target.0 && *s + *sp <= t_end)
}

/// All clips of a track as (start, span) pairs, skipping every index in
/// `skip` (the moving set's own originals do not block their targets).
fn track_spans<T: Placement>(clips: &[T], skip: &[usize], sec_per_tick: f64) -> Vec<(u32, u32)> {
    clips
        .iter()
        .enumerate()
        .filter(|(i, _)| !skip.contains(i))
        .map(|(_, c)| (c.start(), c.span(sec_per_tick)))
        .collect()
}

/// The G / Shift+D modal's per-clip targets (moves and clones share the
/// grab math). `None` when the pointer is unavailable (no ghost this
/// frame) or the modal is a Stretch.
#[allow(clippy::too_many_arguments)]
fn grab_targets(
    modal: &Modal,
    ptr: Option<Pos2>,
    px_per_bar: f32,
    bar_ticks: u32,
    row_h: f32,
    rows_midi: usize,
    rows_control: usize,
    rows_audio: usize,
    midi_tracks: &[MidiTrack],
    control_tracks: &[ControlTrack],
    audio_tracks: &[AudioTrack],
    sec_per_tick: f64,
) -> Option<Vec<ClipMove>> {
    let (ModalKind::Grab { locked } | ModalKind::Duplicate { locked }) = modal.kind else {
        return None;
    };
    let ptr = ptr?;
    let mut bar_delta = ((ptr.x - modal.grab.x) / px_per_bar).round() as i64;
    let mut row_delta = ((ptr.y - modal.grab.y) / row_h).round() as i32;
    // The modal moves within one track kind (arm_modal enforces a single
    // kind): MIDI / control / audio all clamp against their own row count.
    let rows = match modal.origin.first()?.lane {
        TimelineLane::Midi => rows_midi,
        TimelineLane::Control => rows_control,
        TimelineLane::Audio => rows_audio,
    };
    row_delta = clamp_row_delta(row_delta, &modal.origin, rows);
    // An axis lock zeroes the OTHER axis' delta (Blender convention: the
    // named axis is the one that still moves).
    match locked {
        Some(Axis::Time) => row_delta = 0,
        Some(Axis::Row) => bar_delta = 0,
        None => {}
    }
    Some(
        modal
            .origin
            .iter()
            .filter_map(|sel| {
                let (start, _) = clip_geometry(
                    midi_tracks,
                    control_tracks,
                    audio_tracks,
                    sec_per_tick,
                    *sel,
                )?;
                let to_track = (sel.track as i32 + row_delta).max(0) as usize;
                let new_start = (start as i64 + bar_delta * i64::from(bar_ticks)).max(0) as u32;
                Some(ClipMove {
                    from: *sel,
                    to_track,
                    start_ticks: new_start,
                })
            })
            .collect(),
    )
}

/// The T modal's per-clip span edits (bar-aligned, ≥ 1 bar, within the
/// arrangement). `None` when the pointer is unavailable or not a Stretch.
#[allow(clippy::too_many_arguments)]
fn stretch_targets(
    modal: &Modal,
    ptr: Option<Pos2>,
    px_per_bar: f32,
    bar_ticks: u32,
    midi_tracks: &[MidiTrack],
    control_tracks: &[ControlTrack],
    audio_tracks: &[AudioTrack],
    sec_per_tick: f64,
) -> Option<Vec<ClipSpanEdit>> {
    let ModalKind::Stretch { start_edge } = modal.kind else {
        return None;
    };
    let ptr = ptr?;
    let bar_delta = ((ptr.x - modal.grab.x) / px_per_bar).round() as i64;
    let bar = i64::from(bar_ticks);
    let tick_delta = bar_delta * bar;
    let max_end = i64::from(ARRANGEMENT_LIMIT_TICKS);
    Some(
        modal
            .origin
            .iter()
            .filter_map(|sel| {
                let (start, span) = clip_geometry(
                    midi_tracks,
                    control_tracks,
                    audio_tracks,
                    sec_per_tick,
                    *sel,
                )?;
                let (new_start, new_span) = if start_edge {
                    let d = tick_delta.clamp(-(start as i64), span as i64 - bar);
                    ((start as i64 + d) as u32, (span as i64 - d) as u32)
                } else {
                    let d = tick_delta.clamp(bar - span as i64, max_end - (start + span) as i64);
                    (start, (span as i64 + d) as u32)
                };
                Some(ClipSpanEdit {
                    sel: *sel,
                    start_ticks: new_start,
                    span_ticks: new_span,
                })
            })
            .collect(),
    )
}

/// One modal target awaiting overlap validation: the clip at `sel` wants
/// the (start, span) rectangle on row `to_track` of track kind `lane`.
type ModalTarget = (ClipSel, (u32, u32), usize, TimelineLane);

/// Post-finalize validation for a move/stretch batch (M4b decision ②):
/// clips whose target completely covers an existing (non-moving) clip are
/// refused and stay at their origin; the rest go through.
fn filter_covered(
    targets: Vec<ModalTarget>,
    midi_tracks: &[MidiTrack],
    control_tracks: &[ControlTrack],
    audio_tracks: &[AudioTrack],
    sec_per_tick: f64,
) -> (Vec<ModalTarget>, usize) {
    let moving: Vec<ClipSel> = targets.iter().map(|(sel, _, _, _)| *sel).collect();
    let mut kept = Vec::new();
    let mut refused = 0;
    for (sel, target, to_track, lane) in targets {
        let skip: Vec<usize> = moving
            .iter()
            .filter(|m| m.lane == lane && m.track == to_track)
            .map(|m| m.clip)
            .collect();
        let existing = match lane {
            TimelineLane::Control => {
                track_spans(&control_tracks[to_track].clips, &skip, sec_per_tick)
            }
            TimelineLane::Midi => track_spans(&midi_tracks[to_track].clips, &skip, sec_per_tick),
            TimelineLane::Audio => track_spans(&audio_tracks[to_track].clips, &skip, sec_per_tick),
        };
        if fully_covers(target, &existing) {
            refused += 1;
        } else {
            kept.push((sel, target, to_track, lane));
        }
    }
    (kept, refused)
}

struct RowColors {
    lane_bg: Color32,
    grid: Color32,
    clip_fill: Color32,
    clip_stroke: Color32,
    clip_text: Color32,
    /// Outline for the clip currently open in the editor window.
    edit_highlight: Color32,
}

impl RowColors {
    fn midi(selected: bool) -> Self {
        Self {
            lane_bg: Color32::from_gray(if selected { 34 } else { 28 }),
            grid: Color32::from_gray(46),
            clip_fill: if selected {
                Color32::from_rgb(72, 126, 210)
            } else {
                Color32::from_rgb(58, 102, 176)
            },
            clip_stroke: Color32::from_rgb(140, 180, 240),
            clip_text: Color32::from_rgb(225, 235, 250),
            edit_highlight: Color32::from_rgb(120, 225, 255),
        }
    }

    fn control(selected: bool) -> Self {
        Self {
            lane_bg: Color32::from_gray(if selected { 34 } else { 28 }),
            grid: Color32::from_gray(46),
            clip_fill: if selected {
                Color32::from_rgb(196, 132, 52)
            } else {
                Color32::from_rgb(160, 108, 44)
            },
            clip_stroke: Color32::from_rgb(235, 195, 130),
            clip_text: Color32::from_rgb(250, 240, 220),
            edit_highlight: Color32::from_rgb(255, 215, 110),
        }
    }

    /// U6: audio rows share the sample-player family's warm amber-gray
    /// (distinct from the control track's orange-brown at a glance).
    fn audio(selected: bool) -> Self {
        Self {
            lane_bg: Color32::from_gray(if selected { 34 } else { 28 }),
            grid: Color32::from_gray(46),
            clip_fill: if selected {
                Color32::from_rgb(150, 120, 66)
            } else {
                Color32::from_rgb(112, 92, 54)
            },
            clip_stroke: Color32::from_rgb(215, 190, 130),
            clip_text: Color32::from_rgb(245, 240, 225),
            edit_highlight: Color32::from_rgb(235, 205, 130),
        }
    }
}

/// Header cell contents. All three row kinds are renameable (right-click the
/// name), so each carries the document's raw name next to the display label.
enum HeaderKind {
    Midi {
        index: usize,
        /// Display label ("Track N" when the document name is empty).
        name: String,
        /// The document's raw name — the inline rename editor starts from
        /// this, so pressing Enter on an untouched editor keeps "empty =
        /// default label" instead of freezing the generated name.
        raw_name: String,
        selected: bool,
    },
    Control {
        index: usize,
        /// Display label ("Curve N" when the document name is empty).
        name: String,
        raw_name: String,
        selected: bool,
    },
    Audio {
        index: usize,
        /// Display label ("Audio N" when the document name is empty).
        name: String,
        raw_name: String,
        selected: bool,
    },
}

/// Draws the whole arrangement (toolbar, ruler, rows) and returns the actions
/// accumulated this frame.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut TimelineState,
    midi_tracks: &[MidiTrack],
    control_tracks: &[ControlTrack],
    audio_tracks: &[AudioTrack],
    p: TimelineParams<'_>,
    router: &mut crate::focus::FocusRouter,
) -> Vec<TimelineAction> {
    let mut actions = Vec::new();
    // While a modal transform (G/T/Shift+D) or the armed knife owns the
    // pointer, the keyboard stays with the lane that armed it — the pointer
    // may travel across neutral chrome (or past the content edge) without
    // the focus-follow handing the keys to someone else. Same guarantee a
    // real drag gets from the router's any-down freeze; set every frame so
    // it clears itself when the modal ends.
    router.set_focus_hold(state.modal.is_some() || state.knife_armed);
    // U3a: the bar length comes from the document's meter, so "pixels per bar"
    // keeps its on-screen meaning while the tick↔pixel mapping follows the
    // meter (a clip's tick position therefore moves when the meter changes).
    let bar_ticks = p.bar_ticks.max(1);
    let px_per_tick = state.px_per_bar / bar_ticks as f32;
    let row_h = ROW_HEIGHT * state.row_zoom;
    // The lanes span the full arrangement bound, not just past the last clip:
    // the bar grid keeps drawing behind the content, so a G-modal ghost (or
    // any insert / drop target) always has visible bar lines to land on, and
    // the scroll range matches the tick bound every interaction clamps to.
    // The grid/ruler drawing is viewport-culled, so the span costs nothing per
    // frame.
    let content_bars = ARRANGEMENT_LIMIT_TICKS.div_ceil(bar_ticks);
    let content_px = content_bars as f32 * state.px_per_bar;

    // ---- View navigation (read before the ScrollArea lays out, using last
    // frame's row geometry, so zoom and its offset compensation land in the
    // same frame). Ctrl+wheel zooms the time axis anchored on the pointer,
    // Ctrl+Alt+wheel zooms the row height, middle-drag pans both axes. Plain
    // / Shift wheel stay with the ScrollArea (vertical / horizontal
    // scrolling).
    let scroll_id = ui.make_persistent_id("glitcherz_timeline_scroll");
    {
        let (zoom2, mods) = ui.ctx().input(|i| (i.zoom_delta_2d(), i.modifiers));
        let ctrl = mods.ctrl || mods.command;
        let ptr = ui.ctx().pointer_hover_pos();
        let in_rows = state
            .rows_rect
            .is_some_and(|r| ptr.is_some_and(|p| r.contains(p)));
        if in_rows && ctrl {
            let Some(ptr) = ptr else {
                unreachable!("in_rows implies ptr")
            };
            let vmin = state.rows_viewport_min;
            let mut scroll_state = egui::containers::scroll_area::State::load(ui.ctx(), scroll_id);
            let offset = scroll_state.as_ref().map_or(Vec2::ZERO, |s| s.offset);
            // Time axis: uniform zoom (pure Ctrl) or horizontal-only (the
            // touch pinch case). Anchor the tick under the pointer.
            if zoom2.x != 1.0 {
                let old = state.px_per_bar;
                let new = (old * zoom2.x).clamp(PX_PER_BAR_MIN, PX_PER_BAR_MAX);
                if new != old {
                    let old_ppt = old / bar_ticks as f32;
                    let tick = ((offset.x + ptr.x - vmin.x) / old_ppt).max(0.0);
                    let new_ppt = new / bar_ticks as f32;
                    let anchored = (tick * new_ppt - (ptr.x - vmin.x)).max(0.0);
                    state.px_per_bar = new;
                    if let Some(s) = scroll_state.as_mut() {
                        s.offset.x = anchored;
                    }
                }
            }
            // Row height: only a vertical-only factor (Ctrl+Alt) resizes
            // the rows — a uniform zoom belongs to the time axis above.
            if zoom2.y != 1.0 && (zoom2.x - zoom2.y).abs() > 1e-4 {
                state.row_zoom = (state.row_zoom * zoom2.y).clamp(ROW_ZOOM_MIN, ROW_ZOOM_MAX);
            }
            if let Some(s) = scroll_state {
                s.store(ui.ctx(), scroll_id);
            }
            ui.ctx().request_repaint();
        }
        // Middle-drag pans the viewport (the ScrollArea's drag-to-scroll is
        // touch-only by default).
        if in_rows
            && ui
                .ctx()
                .input(|i| i.pointer.button_down(egui::PointerButton::Middle))
        {
            let d = ui.ctx().input(|i| i.pointer.delta());
            if d != Vec2::ZERO
                && let Some(mut s) = egui::containers::scroll_area::State::load(ui.ctx(), scroll_id)
            {
                s.offset.x = (s.offset.x - d.x).max(0.0);
                s.offset.y = (s.offset.y - d.y).max(0.0);
                s.store(ui.ctx(), scroll_id);
            }
        }
    }

    // The scroll area carries an explicit id: all rows below live off its
    // content ui's positional auto ids, so if anything above it comes or
    // goes every row silently re-id's — and since egui keeps focus ON THE
    // ID, the focused lane turns into a phantom (no row reports focus
    // again, every focus-gated key dies, and the "no keyboard focus" chip
    // stays lit no matter how often you click).
    egui::ScrollArea::both()
        .auto_shrink(false)
        .id_salt("glitcherz_timeline_scroll")
        .show(ui, |ui| {
        // Toolbar: zoom + track add/remove + hints.
        ui.horizontal(|ui| {
            ui.strong("Arrangement");
            if ui.button("-").on_hover_text("Zoom out").clicked() {
                state.px_per_bar = (state.px_per_bar / 1.3).max(PX_PER_BAR_MIN);
            }
            if ui.button("+").on_hover_text("Zoom in").clicked() {
                state.px_per_bar = (state.px_per_bar * 1.3).min(PX_PER_BAR_MAX);
            }
            ui.separator();
            // Three track kinds, each with an add / remove pair — the same
            // rows (and order) the patch timeline node exposes: MIDI, then
            // Curve, then Audio.
            if ui.button("+ MIDI").on_hover_text("Add a MIDI track").clicked() {
                actions.push(TimelineAction::AddMidiTrack);
            }
            if ui
                .button("- MIDI")
                .on_hover_text("Remove the last MIDI track")
                .clicked()
            {
                actions.push(TimelineAction::RemoveLastMidiTrack);
            }
            if ui
                .button("+ Curve")
                .on_hover_text("Add a control (curve) track")
                .clicked()
            {
                actions.push(TimelineAction::AddControlTrack);
            }
            if ui
                .button("- Curve")
                .on_hover_text("Remove the selected curve track")
                .clicked()
            {
                actions.push(TimelineAction::RemoveControlTrack);
            }
            if ui.button("+ Audio").on_hover_text("Add an audio track").clicked() {
                actions.push(TimelineAction::AddAudioTrack);
            }
            if ui
                .button("- Audio")
                .on_hover_text("Remove the last audio track")
                .clicked()
            {
                actions.push(TimelineAction::RemoveLastAudioTrack);
            }
            ui.separator();
            // Every variable hint must live INSIDE this child ui. Drawn in
            // the toolbar's parent instead, appearing and disappearing
            // would shift the rows' positional auto ids (see lane_row) and
            // make every row jump down and back whenever a hint or status
            // message comes and goes.
            if !state.had_focus {
                // Keyboard shortcuts are focus-gated; make the invisible
                // gate visible so a dead key is diagnosable at a glance.
                ui.weak(
                    egui::RichText::new("no keyboard focus — hover the timeline")
                        .color(Color32::YELLOW),
                );
            }
            if state.knife_armed {
                ui.weak(
                    egui::RichText::new("Knife: one click cuts (bar-snapped) · Esc / right-click cancels")
                        .color(Color32::from_rgb(240, 190, 80)),
                );
            }
            match &state.modal {
                Some(Modal {
                    kind: ModalKind::Grab { locked },
                    ..
                }) => {
                    ui.weak(
                        egui::RichText::new(format!(
                            "Move: LMB drop · RMB / Esc cancel{}",
                            axis_hint(*locked)
                        ))
                        .color(Color32::from_rgb(140, 200, 250)),
                    );
                }
                Some(Modal {
                    kind: ModalKind::Duplicate { locked },
                    ..
                }) => {
                    ui.weak(
                        egui::RichText::new(format!(
                            "Duplicate: LMB drop the copies · RMB / Esc cancel{}",
                            axis_hint(*locked)
                        ))
                        .color(Color32::from_rgb(140, 200, 250)),
                    );
                }
                Some(Modal {
                    kind: ModalKind::Stretch { start_edge },
                    ..
                }) => {
                    ui.weak(
                        egui::RichText::new(if *start_edge {
                            "Stretch start: LMB drop · RMB / Esc cancel"
                        } else {
                            "Stretch end: LMB drop · RMB / Esc cancel · S: pull start instead"
                        })
                        .color(Color32::from_rgb(140, 200, 250)),
                    );
                }
                None => {}
            }
            // Selection feedback: a persistent count so "did my click
            // select anything?" is answerable at a glance (the selected
            // clips also get an accent stroke in the lanes).
            if !state.selection.is_empty() {
                ui.weak(
                    egui::RichText::new(format!("{} selected", state.selection.len()))
                        .color(Color32::from_rgb(120, 220, 160)),
                );
            }
            // Status messages stick for a few seconds — the old one-frame
            // drain flashed by faster than anyone could read it.
            if state
                .status
                .as_ref()
                .is_some_and(|(_, until)| std::time::Instant::now() >= *until)
            {
                state.status = None;
            }
            if let Some((msg, _)) = &state.status {
                ui.weak(egui::RichText::new(msg).color(Color32::YELLOW));
            }
            // The static hint goes last and truncates: an overflowing child
            // expands the ui's available rect, and the rows below would be
            // laid out wider than the panel itself.
            ui.add(
                egui::Label::new(
                    egui::RichText::new(
                        "Drag move · edge resize · dbl-click insert/edit · right-click delete · marquee: select · Del: delete · K: knife · header dots: reorder track · ruler: click seek / Ctrl+drag loop / RMB marker · Ctrl+wheel: zoom time · Ctrl+Alt+wheel: row height · middle-drag: pan",
                    )
                    .weak(),
                )
                .truncate(),
            );
        });

        // Ruler: bar numbers, markers, the song loop brace and the playhead
        // over the content width (viewport-culled — the ruler spans the
        // whole arrangement, only the bars in view get lines and numbers).
        // U1 interactions: bare click/drag = seek scrub; Ctrl+drag = drag
        // out a song loop range; dragging a loop edge reshapes it; RMB adds
        // a marker (RMB on a marker's flag removes it).
        ui.horizontal(|ui| {
            ui.allocate_exact_size(Vec2::new(HEADER_WIDTH, RULER_HEIGHT), Sense::hover());
            let (rect, response) = ui.allocate_exact_size(
                Vec2::new(content_px, RULER_HEIGHT),
                Sense::click_and_drag(),
            );
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 0.0, Color32::from_gray(24));
            let step = if state.px_per_bar < 28.0 { 4 } else { 1 };
            let clip = ui.clip_rect();
            let first = (((clip.left() - rect.left()) / state.px_per_bar).ceil().max(0.0) as u32)
                .div_ceil(step)
                * step;
            let last = (((clip.right() - rect.left()) / state.px_per_bar).floor().max(0.0) as u32)
                .min(content_bars);
            let mut bar = first;
            while bar <= last {
                let x = rect.left() + bar as f32 * state.px_per_bar;
                painter.line_segment(
                    [pos2(x, rect.top() + 4.0), pos2(x, rect.bottom())],
                    Stroke::new(1.0, Color32::from_gray(55)),
                );
                painter.text(
                    pos2(x + 3.0, rect.top() + 2.0),
                    Align2::LEFT_TOP,
                    format!("{}", bar + 1),
                    FontId::proportional(10.0),
                    Color32::from_gray(150),
                );
                bar += step;
            }

            // ---- U1 geometry helpers ----
            let bar_ticks_f = f64::from(bar_ticks);
            let max_tick = f64::from(content_bars) * bar_ticks_f;
            let tick_at = |x: f32| {
                (((x - rect.left()) / px_per_tick).max(0.0) as f64).min(max_tick)
            };
            let snap_bar = |tick: f64| ((tick / bar_ticks_f).round() * bar_ticks_f).max(0.0);
            let loop_has_range = p.loop_range.end_ticks > p.loop_range.start_ticks;
            let brace_y = (rect.bottom() - 5.0, rect.bottom() - 1.0);
            let start_x = rect.left() + p.loop_range.start_ticks as f32 * px_per_tick;
            let end_x = rect.left() + p.loop_range.end_ticks as f32 * px_per_tick;

            // ---- Markers: flag line + short name under the bar numbers. ----
            for marker in p.markers {
                let x = rect.left() + marker.tick as f32 * px_per_tick;
                if x < clip.left() - 8.0 || x > clip.right() {
                    continue;
                }
                painter.line_segment(
                    [pos2(x, rect.top() + 2.0), pos2(x, brace_y.0)],
                    Stroke::new(1.0, Color32::from_rgb(150, 175, 210)),
                );
                painter.text(
                    pos2(x + 2.0, rect.top() + 9.0),
                    Align2::LEFT_TOP,
                    &marker.name,
                    FontId::proportional(9.0),
                    Color32::from_rgb(170, 190, 220),
                );
            }

            // ---- Song loop brace: the bottom strip of the ruler. Enabled =
            // solid amber; kept-but-disabled = faint outline. ----
            if loop_has_range {
                let brace =
                    Rect::from_min_max(pos2(start_x, brace_y.0), pos2(end_x, brace_y.1));
                if p.loop_range.enabled {
                    painter.rect_filled(brace, 2.0, Color32::from_rgb(240, 200, 70));
                } else {
                    painter.rect_stroke(
                        brace,
                        2.0,
                        Stroke::new(1.0, Color32::from_rgb(150, 125, 50)),
                        StrokeKind::Inside,
                    );
                }
            }

            // ---- Interaction: drag intent locks in at drag start. ----
            let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
            if response.drag_started_by(egui::PointerButton::Primary) {
                if let Some(pos) = response.interact_pointer_pos() {
                    let hit_start = loop_has_range && (pos.x - start_x).abs() <= LOOP_EDGE_GRAB;
                    let hit_end = loop_has_range && (pos.x - end_x).abs() <= LOOP_EDGE_GRAB;
                    if hit_start {
                        state.loop_drag = Some(LoopDrag::StartEdge);
                    } else if hit_end {
                        state.loop_drag = Some(LoopDrag::EndEdge);
                    } else if ctrl {
                        state.loop_drag =
                            Some(LoopDrag::New { anchor: snap_bar(tick_at(pos.x)) });
                    } else {
                        state.loop_drag = None;
                        actions.push(TimelineAction::Seek {
                            ticks: tick_at(pos.x),
                        });
                    }
                }
            }
            if response.dragged_by(egui::PointerButton::Primary)
                && state.loop_drag.is_none()
                && let Some(pos) = response.interact_pointer_pos()
            {
                // Plain scrub: every frame of the drag seeks.
                actions.push(TimelineAction::Seek {
                    ticks: tick_at(pos.x),
                });
            }
            if response.drag_stopped_by(egui::PointerButton::Primary) {
                if let (Some(drag), Some(pos)) =
                    (state.loop_drag, response.interact_pointer_pos())
                {
                    let cur = snap_bar(tick_at(pos.x));
                    let lr = p.loop_range;
                    let committed: Option<(u32, u32)> = match drag {
                        LoopDrag::New { anchor } => {
                            let (a, b) = if anchor <= cur { (anchor, cur) } else { (cur, anchor) };
                            (b - a >= bar_ticks_f).then_some((a as u32, b as u32))
                        }
                        LoopDrag::StartEdge => {
                            let end = f64::from(lr.end_ticks);
                            (cur < end).then_some((cur as u32, lr.end_ticks))
                        }
                        LoopDrag::EndEdge => {
                            let start = f64::from(lr.start_ticks);
                            (cur > start).then_some((lr.start_ticks, cur as u32))
                        }
                    };
                    match committed {
                        Some((s, e)) => actions.push(TimelineAction::SetLoop {
                            start_ticks: s,
                            end_ticks: e,
                        }),
                        None => state.set_status("loop: needs at least one bar"),
                    }
                }
                state.loop_drag = None;
            }
            // RMB: add a marker at the click (bar-snapped); RMB right on a
            // marker's flag line removes it instead.
            if response.secondary_clicked()
                && let Some(pos) = response.interact_pointer_pos()
            {
                let hit = p.markers.iter().enumerate().find(|(_, m)| {
                    (rect.left() + m.tick as f32 * px_per_tick - pos.x).abs() <= MARKER_GRAB
                });
                match hit {
                    Some((index, _)) => actions.push(TimelineAction::RemoveMarker { index }),
                    None => actions.push(TimelineAction::AddMarker {
                        tick: snap_bar(tick_at(pos.x)) as u32,
                    }),
                }
            }

            // ---- Ghost preview of the loop drag in progress. ----
            if let Some(drag) = state.loop_drag
                && let Some(pos) = response.interact_pointer_pos()
            {
                let cur = snap_bar(tick_at(pos.x));
                let (a, b) = match drag {
                    LoopDrag::New { anchor } => {
                        if anchor <= cur { (anchor, cur) } else { (cur, anchor) }
                    }
                    LoopDrag::StartEdge => (cur, f64::from(p.loop_range.end_ticks)),
                    LoopDrag::EndEdge => (f64::from(p.loop_range.start_ticks), cur),
                };
                let ax = rect.left() + a as f32 * px_per_tick;
                let bx = rect.left() + b as f32 * px_per_tick;
                painter.rect_filled(
                    Rect::from_min_max(pos2(ax, brace_y.0), pos2(bx, brace_y.1)),
                    2.0,
                    Color32::from_rgba_premultiplied(240, 200, 70, 90),
                );
            }

            // ---- Playhead through the ruler. ----
            let ph_x = rect.left() + p.playhead_ticks * px_per_tick;
            painter.line_segment(
                [pos2(ph_x, rect.top()), pos2(ph_x, rect.bottom())],
                Stroke::new(1.5, Color32::from_rgb(250, 220, 90)),
            );

            let _ = response.on_hover_text(
                "Click/drag: seek · Ctrl+drag: set song loop · drag loop edges: reshape · right-click: add marker (on a marker: remove)",
            );
        });

        // Marquee selection: every row reports the clips its lane segment
        // of the band covers; show() combines them into the selection once
        // all rows have run (M4b: the band may span several rows).
        let mut marquee_hits = Vec::new();
        // Row geometry registry for the modal ghosts (they can land on any
        // row of the same kind, so they are drawn after all rows have run).
        let mut geom: Vec<(RowKind, Rect)> = Vec::new();
        state.had_focus = false;
        let rows_top = ui.cursor().min.y;

        // MIDI rows.
        for (i, track) in midi_tracks.iter().enumerate() {
            let name = if track.name.is_empty() {
                format!("Track {}", i + 1)
            } else {
                track.name.clone()
            };
            let header = HeaderKind::Midi {
                index: i,
                name,
                raw_name: track.name.clone(),
                selected: p.selected_track == i,
            };
            let highlight = p
                .edit_target
                .and_then(|e| (e.track == i).then_some(e.clip));
            lane_row(
                ui,
                state,
                router,
                &mut actions,
                &mut marquee_hits,
                &mut geom,
                header,
                RowKind::Midi(i),
                &track.clips,
                px_per_tick,
                bar_ticks,
                p.sec_per_tick,
                row_h,
                content_px,
                p.playhead_ticks,
                RowColors::midi(p.selected_track == i),
                highlight,
                &|clip| format!("{} notes", clip.notes().len()),
                &|_| None,
                &|_, _, _| {},
            );
        }

        // Control rows.
        if !control_tracks.is_empty() {
            ui.add_space(4.0);
            ui.weak("Control tracks (delivery endpoint = node-graph cable)");
            for (i, track) in control_tracks.iter().enumerate() {
                let header = HeaderKind::Control {
                    index: i,
                    name: if track.name.is_empty() {
                        format!("Curve {}", i + 1)
                    } else {
                        track.name.clone()
                    },
                    raw_name: track.name.clone(),
                    selected: p.selected_auto == i,
                };
                // U12: no inline editor band anymore — the curve is edited in
                // the Edit tab, and a read-only preview is drawn on each clip
                // block (same row as every other track kind).
                let highlight = None;
                lane_row(
                    ui,
                    state,
                    router,
                    &mut actions,
                    &mut marquee_hits,
                    &mut geom,
                    header,
                    RowKind::Control(i),
                    &track.clips,
                    px_per_tick,
                    bar_ticks,
                    p.sec_per_tick,
                    row_h,
                    content_px,
                    p.playhead_ticks,
                    RowColors::control(p.selected_auto == i),
                    highlight,
                    &|clip| {
                        if clip.curve.is_empty() {
                            "empty".to_string()
                        } else {
                            format!("{} anchors", clip.curve.len())
                        }
                    },
                    &|_| None,
                    &|clip, painter, rect| draw_control_preview(painter, rect, clip),
                );
            }
        }

        // Audio rows (U6): waveform bodies, the same drag/resize/split/
        // marquee/delete gestures (Placement), and double-click on empty
        // space asking for a wav (AddAudioClip → app-side file dialog).
        if !audio_tracks.is_empty() {
            ui.add_space(4.0);
            ui.weak("Audio tracks (double-click empty space to insert a wav)");
            for (i, track) in audio_tracks.iter().enumerate() {
                let header = HeaderKind::Audio {
                    index: i,
                    name: if track.name.is_empty() {
                        format!("Audio {}", i + 1)
                    } else {
                        track.name.clone()
                    },
                    raw_name: track.name.clone(),
                    selected: p.selected_audio == i,
                };
                let highlight = None;
                lane_row(
                    ui,
                    state,
                    router,
                    &mut actions,
                    &mut marquee_hits,
                    &mut geom,
                    header,
                    RowKind::Audio(i),
                    &track.clips,
                    px_per_tick,
                    bar_ticks,
                    p.sec_per_tick,
                    row_h,
                    content_px,
                    p.playhead_ticks,
                    RowColors::audio(p.selected_audio == i),
                    highlight,
                    &|clip: &AudioClip| {
                        std::path::Path::new(&clip.path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "clip".to_string())
                    },
                    // Adapt the path-keyed lookup (TimelineParams) to the
                    // clip-keyed parameter lane_row takes.
                    &|clip: &AudioClip| (p.waveform_of)(&clip.path),
                    &|_, _, _| {},
                );
            }
        }

        // Record this frame's row area for the next frame's navigation
        // input (see the wheel/pan block before the ScrollArea): the union
        // of the header column and every lane, plus the scroll viewport
        // origin for the zoom-anchor math.
        let rows_frame = geom.first().map(|(_, first)| {
            let left = first.left() - HEADER_WIDTH;
            let right = geom
                .iter()
                .map(|(_, r)| r.right())
                .fold(first.right(), f32::max);
            Rect::from_min_max(pos2(left, rows_top), pos2(right, ui.cursor().min.y))
        });
        state.rows_rect = rows_frame;
        if rows_frame.is_some() {
            state.rows_viewport_min = ui.clip_rect().min;
        }

        // ---- Track reorder (header grab handle). The target is the
        // insertion row the pointer hovers among the MIDI rows; the drop
        // (primary release anywhere — the drag follows the pointer across
        // rows) lands as one action. The insert indicator is drawn over
        // the rows so the target is visible while dragging. ----
        if let Some(from) = state.reorder
            && let Some(frame) = rows_frame
        {
            let midi_rows: Vec<Rect> = geom
                .iter()
                .filter_map(|(r, rect)| matches!(r, RowKind::Midi(_)).then_some(*rect))
                .collect();
            let ptr = ui.ctx().pointer_hover_pos();
            let mut drop: Option<usize> = None;
            if let Some(ptr) = ptr
                && let Some((to, y)) = reorder_target(ptr.y, &midi_rows)
            {
                drop = Some(to);
                ui.painter().line_segment(
                    [pos2(frame.left(), y), pos2(frame.right(), y)],
                    Stroke::new(2.0, SELECTED),
                );
            }
            if ui.ctx().input(|i| i.pointer.primary_released()) {
                state.reorder = None;
                if let Some(to) = drop
                    && to != from
                    && to != from + 1
                {
                    actions.push(TimelineAction::ReorderMidiTrack { from, to });
                }
            }
        }

        // Fold the rows' marquee reports into the selection: replace with
        // the band's hits, or union with what was selected when it started.
        // An ending band folds this final frame first, then clears below.
        if let Some(m) = &state.marquee {
            let mut selection = if m.additive {
                state.selection.clone()
            } else {
                Vec::new()
            };
            for (row, hits) in &marquee_hits {
                let lane = row.lane();
    let track = row.track();
                for clip in hits {
                    let sel = ClipSel {
                        lane,
                        track,
                        clip: *clip,
                    };
                    if !selection.contains(&sel) {
                        selection.push(sel);
                    }
                }
            }
            state.selection = selection;
        }
        if state.marquee_ending {
            state.marquee = None;
            state.marquee_ending = false;
        }

        // Modal transform (M4b.3): finalize on the confirming click, cancel
        // on RMB, otherwise draw the ghosts (they may land on any row of
        // the same kind, so they go through the geometry registry).
        // Refusal reasons are collected into a message and flushed after
        // the block — `state.modal` is still borrowed inside it.
        let mut refused_msg: Option<String> = None;
        if state.modal.is_some() {
            // The modal owns the whole input while it runs: the confirming
            // / cancelling click lands ANYWHERE (Blender semantics — the
            // pointer has usually travelled past the lane, e.g. past the
            // content edge, by the time the user clicks). This is the
            // global click, not a lane response.
            let (lmb, rmb) = ui.ctx().input(|i| {
                (
                    i.pointer.button_clicked(egui::PointerButton::Primary),
                    i.pointer.button_clicked(egui::PointerButton::Secondary),
                )
            });
            if lmb {
                state.modal_confirm = true;
            }
            if rmb {
                state.modal_cancel = true;
            }
            let ptr = ui.ctx().pointer_hover_pos();
            let px_per_bar = state.px_per_bar;
            if state.modal_cancel || state.modal_confirm {
                if state.modal_confirm {
                    if let Some(m) = &state.modal {
                        match m.kind {
                            ModalKind::Grab { .. } | ModalKind::Duplicate { .. } => {
                                if let Some(moves) = grab_targets(
                                    m,
                                    ptr,
                                    px_per_bar,
                                    bar_ticks,
                                    row_h,
                                    midi_tracks.len(),
                                    control_tracks.len(),
                                    audio_tracks.len(),
                                    midi_tracks,
                                    control_tracks,
                                    audio_tracks,
                                    p.sec_per_tick,
                                ) {
                                    let targets: Vec<_> = moves
                                        .into_iter()
                                        .filter_map(|mv| {
                                            let (_, span) = clip_geometry(
                                                midi_tracks,
                                                control_tracks,
                                                audio_tracks,
                                                p.sec_per_tick,
                                                mv.from,
                                            )?;
                                            Some((
                                                mv.from,
                                                (mv.start_ticks, span),
                                                mv.to_track,
                                                mv.from.lane,
                                            ))
                                        })
                                        .collect();
                                    let (kept, refused) = filter_covered(
                                        targets,
                                        midi_tracks,
                                        control_tracks,
                                        audio_tracks,
                                        p.sec_per_tick,
                                    );
                                    if refused > 0 {
                                        refused_msg = Some(format!(
                                            "{refused} clip(s) held: target fully covers an existing clip"
                                        ));
                                    }
                                    match m.kind {
                                        ModalKind::Duplicate { .. } => {
                                            if !kept.is_empty() {
                                                actions.push(TimelineAction::DuplicateClipsBatch {
                                                    dups: kept
                                                        .into_iter()
                                                        .map(|(from, (start_ticks, _), to_track, _)| {
                                                            ClipDup { from, to_track, start_ticks }
                                                        })
                                                        .collect(),
                                                });
                                            }
                                        }
                                        _ => {
                                            if !kept.is_empty() {
                                                actions.push(TimelineAction::MoveClipsBatch {
                                                    moves: kept
                                                        .into_iter()
                                                        .map(|(from, (start_ticks, _), to_track, _)| {
                                                            ClipMove { from, to_track, start_ticks }
                                                        })
                                                        .collect(),
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                            ModalKind::Stretch { .. } => {
                                if let Some(spans) = stretch_targets(
                                    m,
                                    ptr,
                                    px_per_bar,
                                    bar_ticks,
                                    midi_tracks,
                                    control_tracks,
                                    audio_tracks,
                                    p.sec_per_tick,
                                ) {
                                    let targets: Vec<_> = spans
                                        .iter()
                                        .map(|e| {
                                            (
                                                e.sel,
                                                (e.start_ticks, e.span_ticks),
                                                e.sel.track,
                                                e.sel.lane,
                                            )
                                        })
                                        .collect();
                                    let (kept, refused) = filter_covered(
                                        targets,
                                        midi_tracks,
                                        control_tracks,
                                        audio_tracks,
                                        p.sec_per_tick,
                                    );
                                    if refused > 0 {
                                        refused_msg = Some(format!(
                                            "{refused} clip(s) held: target fully covers an existing clip"
                                        ));
                                    }
                                    if !kept.is_empty() {
                                        actions.push(TimelineAction::StretchClipsBatch {
                                            spans: kept
                                                .into_iter()
                                                .map(|(sel, (start_ticks, span_ticks), _, _)| {
                                                    ClipSpanEdit { sel, start_ticks, span_ticks }
                                                })
                                                .collect(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
                state.modal = None;
                state.modal_confirm = false;
                state.modal_cancel = false;
            } else if let Some(m) = &state.modal {
                // Ghost preview: solid blocks at each target (alpha 170),
                // over dimmed originals (the rows already drew them dim).
                let targets = match m.kind {
                    ModalKind::Grab { .. } | ModalKind::Duplicate { .. } => grab_targets(
                        m,
                        ptr,
                        px_per_bar,
                        bar_ticks,
                        row_h,
                        midi_tracks.len(),
                        control_tracks.len(),
                        audio_tracks.len(),
                        midi_tracks,
                        control_tracks,
                        audio_tracks,
                        p.sec_per_tick,
                    )
                    .map(|ms| {
                        ms.into_iter()
                            .filter_map(|mv| {
                                let (_, span) = clip_geometry(
                                    midi_tracks,
                                    control_tracks,
                                    audio_tracks,
                                    p.sec_per_tick,
                                    mv.from,
                                )?;
                                Some((mv.from.lane, mv.to_track, mv.start_ticks, span))
                            })
                            .collect::<Vec<_>>()
                    }),
                    ModalKind::Stretch { .. } => stretch_targets(
                        m,
                        ptr,
                        px_per_bar,
                        bar_ticks,
                        midi_tracks,
                        control_tracks,
                        audio_tracks,
                        p.sec_per_tick,
                    )
                    .map(|es| {
                        es.into_iter()
                            .map(|e| (e.sel.lane, e.sel.track, e.start_ticks, e.span_ticks))
                            .collect::<Vec<_>>()
                    }),
                };
                if let Some(targets) = targets {
                    for (lane_kind, track, start, span) in targets {
                        let Some((_, lane_rect)) = geom
                            .iter()
                            .find(|(row, _)| row.key() == (lane_kind.row_code(), track))
                        else {
                            continue;
                        };
                        let x0 = lane_rect.left() + start as f32 * px_per_tick;
                        let x1 = lane_rect.left() + (start + span) as f32 * px_per_tick;
                        let rect = Rect::from_min_max(
                            pos2(x0 + 1.0, lane_rect.top() + 2.0),
                            pos2(x1 - 1.0, lane_rect.bottom() - 2.0),
                        );
                        let ghost = match lane_kind {
                            TimelineLane::Midi => {
                                Color32::from_rgba_unmultiplied(140, 180, 240, 170)
                            }
                            TimelineLane::Control => {
                                Color32::from_rgba_unmultiplied(235, 195, 130, 170)
                            }
                            TimelineLane::Audio => {
                                Color32::from_rgba_unmultiplied(215, 190, 130, 170)
                            }
                        };
                        let painter = ui.painter_at(*lane_rect);
                        painter.rect_filled(rect, 3.0, ghost);
                    }
                }
            }
        }
        if let Some(msg) = refused_msg {
            state.set_status(msg);
        }
    });
    actions
}

/// Draws one audio clip's waveform body (U6 D6): min/max peak columns
/// spread across the clip rect — the same proportional mapping the RT
/// reader uses (frame = (t − start)/span × wav), so what you see is what
/// you hear. The peaks cover the whole file and the rect IS the span, so
/// the mapping needs no extra inputs.
fn draw_waveform(
    painter: &egui::Painter,
    rect: Rect,
    data: &glitcherz_engine::SampleData,
    color: Color32,
) {
    let peaks = data.peaks();
    if peaks.is_empty() || rect.width() < 2.0 {
        return;
    }
    let mid = rect.center().y;
    let half = (rect.height() / 2.0).max(2.0);
    let columns = (rect.width() / 1.5).floor().max(1.0) as usize;
    let stroke = Stroke::new(1.0, color);
    for column in 0..columns {
        // Column j maps to the peaks slice [j/n, (j+1)/n) — the slice's
        // combined extent is the column's min/max.
        let from = column * peaks.len() / columns;
        let to = ((column + 1) * peaks.len() / columns).max(from + 1);
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        for &(p_min, p_max) in &peaks[from..to.min(peaks.len())] {
            lo = lo.min(p_min);
            hi = hi.max(p_max);
        }
        if lo > hi {
            continue;
        }
        let x = rect.left() + (column as f32 + 0.5) * rect.width() / columns as f32;
        let y0 = mid - hi.clamp(-1.0, 1.0) * half;
        let y1 = mid - lo.clamp(-1.0, 1.0) * half;
        let (y0, y1) = (y0.min(y1), y0.max(y1).max(y0 + 1.0));
        painter.line_segment([pos2(x, y0), pos2(x, y1)], stroke);
    }
}

/// A row header's name cell: the name button, or — once a right-click opened
/// the inline rename editor — a same-size singleline field (so opening it
/// never re-lays-out the row). Enter or a click elsewhere commits through
/// [`TimelineAction::RenameTrack`], Esc cancels, and an empty draft clears the
/// custom name back to `default_label` ("Track 3", "Curve 1", …).
#[allow(clippy::too_many_arguments)]
fn track_header_cell(
    ui: &mut egui::Ui,
    state: &mut TimelineState,
    actions: &mut Vec<TimelineAction>,
    lane: TimelineLane,
    index: usize,
    name: &str,
    raw_name: String,
    width: f32,
    height: f32,
    selected_fill: Option<Color32>,
    select_action: TimelineAction,
    default_label: &str,
) {
    if state
        .rename
        .as_ref()
        .is_some_and(|(l, i, _)| *l == lane && *i == index)
    {
        let mut cancel = false;
        let mut commit = false;
        if let Some((_, _, draft)) = state.rename.as_mut() {
            let resp = ui.add_sized(
                [width, height],
                egui::TextEdit::singleline(draft)
                    .font(egui::TextStyle::Small)
                    .hint_text(default_label.to_owned()),
            );
            if std::mem::take(&mut state.rename_focus) {
                resp.request_focus();
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel = true;
            } else if resp.lost_focus() {
                actions.push(TimelineAction::RenameTrack {
                    lane,
                    track: index,
                    name: draft.trim().to_owned(),
                });
                commit = true;
            }
        }
        if cancel || commit {
            state.rename = None;
        }
    } else {
        let mut btn = egui::Button::new(egui::RichText::new(name).size(11.0));
        if let Some(fill) = selected_fill {
            btn = btn.fill(fill);
        }
        let resp = ui
            .add_sized([width, height], btn)
            .on_hover_text("Right-click to rename this track".to_owned());
        if resp.clicked() {
            actions.push(select_action);
        }
        if resp.secondary_clicked() {
            state.rename = Some((lane, index, raw_name));
            state.rename_focus = true;
        }
    }
}

/// One row: header cell + clip lane (grid, clips, playhead, interactions).
#[allow(clippy::too_many_arguments)]
fn lane_row<T: Placement>(
    parent: &mut egui::Ui,
    state: &mut TimelineState,
    router: &mut crate::focus::FocusRouter,
    actions: &mut Vec<TimelineAction>,
    marquee_hits: &mut Vec<(RowKind, Vec<usize>)>,
    geom: &mut Vec<(RowKind, Rect)>,
    header: HeaderKind,
    row: RowKind,
    clips: &[T],
    px_per_tick: f32,
    bar_ticks: u32,
    sec_per_tick: f64,
    row_h: f32,
    content_px: f32,
    playhead_ticks: f32,
    colors: RowColors,
    highlight: Option<usize>,
    clip_label: &dyn Fn(&T) -> String,
    // Waveform source for the clip bodies (U6): `Some` renders peaks
    // columns instead of the flat fill. MIDI/control rows pass `&|_| None`.
    waveform_of: &dyn Fn(&T) -> Option<std::sync::Arc<glitcherz_engine::SampleData>>,
    // Optional per-clip body decoration drawn over the clip block (U12: the
    // read-only control curve preview). MIDI/audio rows pass a no-op.
    clip_body: &dyn Fn(&T, &egui::Painter, Rect),
) {
    // The row's widget ids must be STABLE. The lane is the keyboard focus
    // carrier, and egui auto ids are positional: anything above the rows
    // appearing or disappearing (the focus chip, the knife / modal hints,
    // the status message — all drawn after the rows were show()'d this
    // way) shifts every row's id, so the focused lane "disappears" and the
    // focus stays parked on a phantom id forever (egui's dead-man switch
    // grants one frame of grace, which only hides the problem for a frame:
    // `has_focus` reports false, so every focus-gated key is dead and the
    // "no keyboard focus" chip comes back no matter how often you click).
    // `push_id` cannot help (it salts the parent id but still mixes in the
    // positional counter); `UiBuilder::id` gives the row an id that is
    // independent of everything drawn above it.
    //
    // The max_rect must be clamped to ONE row. A raw `new_child` (unlike
    // `ui.horizontal`, which pre-allocates only `interact_size.y`) hands
    // the child the FULL remaining rect, and a Center-aligned horizontal
    // frame then fills its whole available height (`Layout::
    // next_frame_ignore_wrap`): the first row would swallow the entire
    // scroll area and every later row would land below the fold — the
    // "only the last track remains" bug.
    let mut row_rect = parent.available_rect_before_wrap();
    row_rect.max.y = row_rect.min.y + row_h;
    let mut row_ui = parent.new_child(
        egui::UiBuilder::new()
            // Same layout as `ui.horizontal`, but with an explicit id.
            .layout(egui::Layout::left_to_right(egui::Align::Center))
            .id(egui::Id::new("glitcherz_timeline_row").with(row.key()))
            .max_rect(row_rect),
    );
    let ui = &mut row_ui;
    // Header cell.
    match header {
        HeaderKind::Midi {
            index,
            name,
            raw_name,
            selected,
        } => {
            // Reorder grab handle (MIDI rows only): a drag on the dots
            // moves the track. Sense::drag only — a plain click on it does
            // nothing (selecting stays with the name button).
            let (grip_rect, grip_response) =
                ui.allocate_exact_size(Vec2::new(12.0, row_h - 4.0), Sense::drag());
            let grip_color = if state.reorder == Some(index) {
                SELECTED
            } else {
                Color32::from_gray(110)
            };
            let grip_painter = ui.painter_at(grip_rect);
            let mid = grip_rect.center().y;
            for dy in [-3.0, 0.0, 3.0] {
                for dx in [-3.0, 0.0] {
                    grip_painter.circle_filled(
                        pos2(grip_rect.center().x + dx, mid + dy),
                        1.2,
                        grip_color,
                    );
                }
            }
            if grip_response.drag_started_by(egui::PointerButton::Primary) {
                state.reorder = Some(index);
            }
            if grip_response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
            track_header_cell(
                ui,
                state,
                actions,
                TimelineLane::Midi,
                index,
                &name,
                raw_name,
                HEADER_WIDTH - 16.0,
                row_h - 4.0,
                selected.then_some(Color32::from_rgb(60, 90, 140)),
                TimelineAction::SelectTrack(index),
                &format!("Track {}", index + 1),
            );
        }
        HeaderKind::Control {
            index,
            name,
            raw_name,
            selected,
        } => {
            track_header_cell(
                ui,
                state,
                actions,
                TimelineLane::Control,
                index,
                &name,
                raw_name,
                HEADER_WIDTH,
                row_h - 4.0,
                selected.then_some(Color32::from_rgb(120, 90, 40)),
                TimelineAction::SelectControlTrack(index),
                &format!("Curve {}", index + 1),
            );
        }
        HeaderKind::Audio {
            index,
            name,
            raw_name,
            selected,
        } => {
            track_header_cell(
                ui,
                state,
                actions,
                TimelineLane::Audio,
                index,
                &name,
                raw_name,
                HEADER_WIDTH,
                row_h - 4.0,
                selected.then_some(Color32::from_rgb(60, 52, 34)),
                TimelineAction::SelectAudioTrack(index),
                &format!("Audio {}", index + 1),
            );
        }
    }

    // Lane surface.
    let (lane, response) =
        ui.allocate_exact_size(Vec2::new(content_px, row_h), Sense::click_and_drag());
    // The lane is a keyboard surface of the app-level FocusRouter: hovering
    // it grants focus, hovering another surface or chrome releases it. The
    // explicit click is a fallback that also pulls focus out of an inline
    // DragValue edit, which the router is frozen on.
    router.register_keyboard(response.id, lane);
    if response.clicked() {
        response.request_focus();
    }
    if response.has_focus() {
        state.had_focus = true;
        state.focus_id = Some(response.id);
    }
    let painter = ui.painter_at(lane);
    painter.rect_filled(lane, 0.0, colors.lane_bg);
    geom.push((row, lane));

    // Bar grid (viewport-culled: the lane spans the whole arrangement, so
    // iterating from bar 0 would walk hundreds of off-screen bars every
    // frame).
    let bar_px = px_per_tick * bar_ticks as f32;
    let bars = (content_px / bar_px).floor() as u32;
    let clip_x = ui.clip_rect();
    let first = (((clip_x.left() - lane.left()) / bar_px).ceil().max(0.0) as u32).min(bars);
    let last = (((clip_x.right() - lane.left()) / bar_px).floor().max(0.0) as u32).min(bars);
    for bar in first..=last {
        let x = lane.left() + bar as f32 * bar_px;
        painter.line_segment(
            [pos2(x, lane.top()), pos2(x, lane.bottom())],
            Stroke::new(1.0, colors.grid),
        );
    }

    let tick_at = |x: f32| ((x - lane.left()) / px_per_tick).max(0.0);

    let lane_kind = row.lane();
    let track = row.track();

    // Clip blocks.
    for (index, clip) in clips.iter().enumerate() {
        let x0 = lane.left() + clip.start() as f32 * px_per_tick;
        let x1 = lane.left() + (clip.start() + clip.span(sec_per_tick)) as f32 * px_per_tick;
        let rect = Rect::from_min_max(
            pos2(x0 + 1.0, lane.top() + 2.0),
            pos2(x1 - 1.0, lane.bottom() - 2.0),
        );
        if rect.right() < lane.left() || rect.left() > lane.right() || rect.width() < 2.0 {
            continue;
        }
        // Modal convention (M4a.5): originals dim while the ghost is
        // live, so the preview stays visible where they overlap.
        let selected = state.selection.contains(&ClipSel {
            lane: lane_kind,
            track,
            clip: index,
        });
        let fill = if selected && state.modal.is_some() {
            Color32::from_rgba_unmultiplied(
                colors.clip_fill.r(),
                colors.clip_fill.g(),
                colors.clip_fill.b(),
                60,
            )
        } else {
            colors.clip_fill
        };
        painter.rect_filled(rect, 3.0, fill);
        painter.rect_stroke(
            rect,
            3.0,
            Stroke::new(1.0, colors.clip_stroke),
            StrokeKind::Inside,
        );
        // U6: a clip with waveform data draws min/max peak columns across
        // its body (stretched to the span, exactly like playback reads it).
        if let Some(data) = waveform_of(clip) {
            draw_waveform(&painter, rect, &data, colors.clip_stroke);
        }
        // U12: per-clip preview drawn over the block (control curve).
        clip_body(clip, &painter, rect);
        if state.selection.contains(&ClipSel {
            lane: lane_kind,
            track,
            clip: index,
        }) {
            painter.rect_stroke(rect, 3.0, Stroke::new(2.0, SELECTED), StrokeKind::Inside);
        }
        if highlight == Some(index) {
            painter.rect_stroke(
                rect,
                3.0,
                Stroke::new(2.0, colors.edit_highlight),
                StrokeKind::Inside,
            );
        }
        painter.text(
            pos2(rect.left() + 4.0, rect.center().y),
            Align2::LEFT_CENTER,
            clip_label(clip),
            FontId::proportional(10.0),
            colors.clip_text,
        );
    }

    // Playhead.
    let playhead_x = lane.left() + playhead_ticks * px_per_tick;
    if lane.left() <= playhead_x && playhead_x <= lane.right() {
        painter.line_segment(
            [
                pos2(playhead_x, lane.top()),
                pos2(playhead_x, lane.bottom()),
            ],
            Stroke::new(1.5, Color32::from_rgb(235, 90, 90)),
        );
    }

    // Interactions (all edits are emitted as actions; nothing mutates the
    // document here).

    // Knife (K armed): an LMB click cuts the clip under the pointer at
    // the bar-snapped position. Single-shot, unified with the roll: the
    // arming is consumed by one click either way (cut, refused cut, or
    // empty lane); Esc / right-click cancel without consuming.
    if state.knife_armed && state.modal.is_none() && response.clicked() {
        state.knife_armed = false;
        if let Some(pos) = response.interact_pointer_pos() {
            if clips.len() >= MAX_CLIPS_PER_TRACK {
                state.set_status("this track is full — cannot split");
            } else {
                match knife_target(clips, tick_at(pos.x), bar_ticks, sec_per_tick) {
                    Some((index, at)) => {
                        actions.push(TimelineAction::SplitClip {
                            lane: lane_kind,
                            track,
                            clip: index,
                            at_ticks: at,
                        });
                        state
                            .selection
                            .retain(|s| s.lane != lane_kind || s.track != track || s.clip != index);
                    }
                    None if hit_clip(clips, tick_at(pos.x), sec_per_tick).is_some() => {
                        state.set_status("a cut needs at least one bar on both sides");
                    }
                    None => {}
                }
            }
        }
    }

    // Knife preview: while armed, mark where the cut would land (the
    // snapped bar line through the hovered lane).
    if state.knife_armed && response.hovered() {
        if let Some(pos) = response.hover_pos() {
            let at = snap_bar(tick_at(pos.x), bar_ticks);
            let x = lane.left() + at as f32 * px_per_tick;
            painter.line_segment(
                [pos2(x, lane.top()), pos2(x, lane.bottom())],
                Stroke::new(1.5, Color32::from_rgb(240, 190, 80)),
            );
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
    }

    // Double-click: open the clip's editor, or insert a new clip on empty
    // space. Not while a modal owns the pointer.

    if response.double_clicked() && state.modal.is_none() {
        if let Some(pos) = response.interact_pointer_pos() {
            let tick = tick_at(pos.x);
            if let Some(index) = hit_clip(clips, tick, sec_per_tick) {
                match row {
                    RowKind::Midi(t) => actions.push(TimelineAction::OpenMidiEditor {
                        track: t,
                        clip: index,
                    }),
                    // U12: a control clip's curve is edited in the Edit tab (no
                    // inline band anymore).
                    RowKind::Control(t) => actions.push(TimelineAction::OpenControlEditor {
                        track: t,
                        clip: index,
                    }),
                    // U6 D2: audio clips have no editor — the double-click
                    // selects the track (highlight + `- Audio` target).
                    RowKind::Audio(t) => {
                        actions.push(TimelineAction::SelectAudioTrack(t));
                    }
                }
            } else if clips.len() < MAX_CLIPS_PER_TRACK {
                let bound = ARRANGEMENT_LIMIT_TICKS - bar_ticks;
                let start = snap_bar(tick, bar_ticks).min(bound);
                if let RowKind::Audio(t) = row {
                    // U6 D2: an empty audio lane asks for a wav — the caller
                    // opens the file dialog and creates the clip.
                    actions.push(TimelineAction::AddAudioClip {
                        track: t,
                        start_ticks: start,
                    });
                } else {
                    let lane = row.lane();
                    let track = row.track();
                    actions.push(TimelineAction::AddClip {
                        lane,
                        track,
                        start_ticks: start,
                    });
                }
            }
        }
    }

    // Drag start: on a clip, pick move vs resize by the grab zone at
    // the right edge (and select it if it was not); on empty space,
    // start a rubber band (M4b selection). While the knife is armed or
    // a modal is live the pointer belongs to them: no drags, no bands.
    // Left button only: the middle button owns panning, and egui's
    // unfiltered `drag_started`/`dragged`/`drag_stopped` fire for ANY
    // button (the patch filters by button everywhere for this reason).
    if response.drag_started_by(egui::PointerButton::Primary)
        && !state.knife_armed
        && state.modal.is_none()
    {
        if let Some(pos) = response.interact_pointer_pos() {
            let tick = tick_at(pos.x);
            if let Some(index) = hit_clip(clips, tick, sec_per_tick) {
                let additive = ui.ctx().input(|i| i.modifiers.shift || i.modifiers.ctrl);
                let sel = ClipSel {
                    lane: lane_kind,
                    track,
                    clip: index,
                };
                if additive {
                    if let Some(at) = state.selection.iter().position(|s| *s == sel) {
                        state.selection.remove(at);
                    } else {
                        state.selection.push(sel);
                    }
                } else if !state.selection.contains(&sel) {
                    state.selection = vec![sel];
                }
                let edge_x = lane.left()
                    + (clips[index].start() + clips[index].span(sec_per_tick)) as f32 * px_per_tick;
                if (pos.x - edge_x).abs() <= EDGE_GRAB {
                    state.drag = Some(Drag::Resize { row, clip: index });
                } else {
                    let grab = tick - clips[index].start() as f32;
                    state.drag = Some(Drag::Move {
                        row,
                        clip: index,
                        grab_ticks: grab,
                    });
                }
            } else {
                let additive = ui.ctx().input(|i| i.modifiers.shift || i.modifiers.ctrl);
                state.marquee = Some(Marquee {
                    origin: pos,
                    current: pos,
                    additive,
                });
            }
        }
    }

    if response.dragged_by(egui::PointerButton::Primary) {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(m) = state.marquee.as_mut() {
                m.current = pos;
            } else {
                match state.drag {
                    Some(Drag::Move {
                        clip, grab_ticks, ..
                    }) if clip < clips.len() => {
                        let span = clips[clip].span(sec_per_tick);
                        let limit = ARRANGEMENT_LIMIT_TICKS - span;
                        let start = snap_bar(tick_at(pos.x) - grab_ticks, bar_ticks).min(limit);
                        if clips[clip].start() != start {
                            let lane = row.lane();
                            let track = row.track();
                            actions.push(TimelineAction::MoveResizeClip {
                                lane,
                                track,
                                clip,
                                start_ticks: start,
                                span_ticks: span,
                            });
                        }
                    }
                    Some(Drag::Resize { clip, .. }) if clip < clips.len() => {
                        let start = clips[clip].start();
                        let limit = ARRANGEMENT_LIMIT_TICKS - start;
                        let span = snap_bar(tick_at(pos.x) - start as f32, bar_ticks)
                            .clamp(bar_ticks, limit.max(bar_ticks));
                        if clips[clip].span(sec_per_tick) != span {
                            let lane = row.lane();
                            let track = row.track();
                            actions.push(TimelineAction::MoveResizeClip {
                                lane,
                                track,
                                clip,
                                start_ticks: start,
                                span_ticks: span,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if response.drag_stopped_by(egui::PointerButton::Primary) {
        state.drag = None;
        if state.marquee.is_some() {
            state.marquee_ending = true;
        }
    }

    // Marquee hit reporting: every row contributes the clips its lane
    // segment of the band covers, every frame the band is live.
    if let Some(m) = &state.marquee {
        let band = Rect::from_two_pos(m.origin, m.current);
        let hits = clips
            .iter()
            .enumerate()
            .filter(|(_, clip)| {
                let x0 = lane.left() + clip.start() as f32 * px_per_tick;
                let x1 =
                    lane.left() + (clip.start() + clip.span(sec_per_tick)) as f32 * px_per_tick;
                Rect::from_min_max(pos2(x0, lane.top()), pos2(x1, lane.bottom())).intersects(band)
            })
            .map(|(index, _)| index)
            .collect();
        marquee_hits.push((row, hits));
    }

    // The band itself, clipped to this row's lane segment.
    if let Some(m) = &state.marquee {
        let band = Rect::from_two_pos(m.origin, m.current);
        let segment = band.intersect(lane);
        if segment.is_positive() {
            painter.rect_filled(
                segment,
                0.0,
                Color32::from_rgba_unmultiplied(80, 140, 220, 40),
            );
            painter.rect_stroke(
                segment,
                0.0,
                Stroke::new(1.0, Color32::from_rgb(80, 140, 220)),
                StrokeKind::Inside,
            );
        }
    }

    // A plain click (press + release without moving past the click
    // threshold) picks the clip under the pointer. egui only reports
    // `drag_started` once the pointer MOVED, so without this a click
    // selects nothing and every command key (G / T / Shift+D) just
    // flashes "nothing selected" — the roll hit the same trap. Clicking
    // empty space deselects; a modifier click on empty space leaves the
    // selection alone.
    if response.clicked() && !state.knife_armed && state.modal.is_none() {
        if let Some(pos) = response.interact_pointer_pos() {
            let additive = ui.ctx().input(|i| i.modifiers.shift || i.modifiers.ctrl);
            match hit_clip(clips, tick_at(pos.x), sec_per_tick) {
                Some(index) => {
                    let sel = ClipSel {
                        lane: lane_kind,
                        track,
                        clip: index,
                    };
                    if additive {
                        if let Some(at) = state.selection.iter().position(|s| *s == sel) {
                            state.selection.remove(at);
                        } else {
                            state.selection.push(sel);
                        }
                    } else if !state.selection.contains(&sel) {
                        state.selection = vec![sel];
                    }
                }
                None if additive => {}
                None => state.selection.clear(),
            }
        }
    }

    // Right-click: cancel a live band, disarm the knife, otherwise
    // delete the clip under the pointer.
    if response.secondary_clicked() && state.modal.is_none() {
        if state.knife_armed {
            state.knife_armed = false;
        } else if let Some(pos) = response.interact_pointer_pos() {
            if state.marquee.take().is_none() {
                if let Some(index) = hit_clip(clips, tick_at(pos.x), sec_per_tick) {
                    actions.push(TimelineAction::RemoveClip {
                        lane: lane_kind,
                        track,
                        clip: index,
                    });
                    state.drag = None;
                }
            }
        }
    }

    // Keyboard: focus-gated, roll-style. M4b.1 wires Escape (clear the
    // selection / cancel the band) and Delete (remove the selection as
    // one undo step); the command family lands in M4b.2+.
    if response.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                EventFilter {
                    escape: true,
                    // egui's arrow-key focus navigation is the same trap
                    // the roll hit in M4a.4: unclaimed directional keys
                    // hand focus to whatever widget points that way.
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    ..Default::default()
                },
            )
        });
        let escape = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        let delete = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete));
        let knife = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::K));
        let t = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::T));
        // S is a mid-modal qualifier (flips a Stretch to its start edge,
        // unified with the roll's t+s) — never an arming key here.
        let s = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::S));
        let g = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::G));
        // Shift+D must be consumed before plain D would ever match
        // (consume_key ignores the shift qualifier when matching).
        let shift_d = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::D));
        let x = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::X));
        let y = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::Y));
        if knife {
            // Single-shot, unified with the roll: K arms, one click
            // consumes; Esc / right-click cancel. Not a toggle.
            state.knife_armed = true;
            state.modal = None;
        }
        // X / Y lock an axis mid-modal (Blender convention, unified with
        // the roll): X = time only, Y = rows only. The same key again
        // releases the lock; the other axis takes over.
        if x || y {
            if let Some(modal) = &mut state.modal {
                let axis = if x { Axis::Time } else { Axis::Row };
                match &mut modal.kind {
                    ModalKind::Grab { locked } | ModalKind::Duplicate { locked } => {
                        *locked = if *locked == Some(axis) {
                            None
                        } else {
                            Some(axis)
                        };
                    }
                    ModalKind::Stretch { .. } => {}
                }
            }
        }
        // A live modal owns the keyboard: only the qualifiers answer —
        // S flips a Stretch to its start edge (the roll's t+s).
        if let Some(modal) = &mut state.modal {
            if s && matches!(modal.kind, ModalKind::Stretch { .. }) {
                modal.kind = ModalKind::Stretch { start_edge: true };
            }
        }
        // Arm G / Shift+D / T (Blender-style modal; arming one tool
        // clears the other).
        if (g || shift_d || t) && state.modal.is_none() {
            state.knife_armed = false;
            let arm = if g {
                Arm::Grab
            } else if shift_d {
                Arm::Duplicate
            } else {
                Arm::Stretch
            };
            arm_modal(state, ui.ctx(), arm);
        }
        if escape {
            if state.modal.is_some() {
                state.modal = None;
            } else {
                state.selection.clear();
                state.marquee = None;
                state.marquee_ending = false;
                state.knife_armed = false;
            }
        }
        if delete && state.modal.is_none() && !state.selection.is_empty() {
            actions.push(TimelineAction::RemoveClipsBatch {
                clips: state.selection.clone(),
            });
            state.selection.clear();
        }
    }
    // `new_child` does not allocate any space in the parent (egui 0.36
    // docs) — `ui.horizontal` reserves its child's min_rect via
    // `scope_dyn`; a raw `new_child` must hand the parent its extent
    // explicitly or every row lays out at the parent's unmoved cursor.
    parent.advance_cursor_after_rect(row_ui.min_rect());
}

/// Which modal G / Shift+D / T arms (M4b.3/4).
enum Arm {
    Grab,
    Duplicate,
    Stretch,
}

/// The X/Y axis-lock suffix for the Move / Duplicate hints.
fn axis_hint(locked: Option<Axis>) -> &'static str {
    match locked {
        Some(Axis::Time) => " · X: time only (X again releases)",
        Some(Axis::Row) => " · Y: rows only (Y again releases)",
        None => " · X: time only · Y: rows only",
    }
}

/// Arms a modal from the current selection. G and Shift+D require a
/// single track kind (cross-kind moves are meaningless); all need a
/// non-empty selection and a pointer position to grab from.
fn arm_modal(state: &mut TimelineState, ctx: &egui::Context, arm: Arm) {
    if state.selection.is_empty() {
        state.set_status("nothing selected \u{2014} G / Shift+D / T need a selection");
        return;
    }
    // U11: the modal family is three-table now — audio clips arm G / T /
    // Shift+D just like MIDI/control (the geometry layer derives audio spans
    // from the seconds window at the base tempo).
    let Some(ptr) = ctx.pointer_hover_pos() else {
        return;
    };
    let kind = match arm {
        Arm::Grab => {
            let first = state.selection[0].lane;
            if state.selection.iter().any(|s| s.lane != first) {
                state.set_status("mixed selection: G moves one track kind at a time");
                return;
            }
            ModalKind::Grab { locked: None }
        }
        Arm::Duplicate => {
            let first = state.selection[0].lane;
            if state.selection.iter().any(|s| s.lane != first) {
                state.set_status("mixed selection: Shift+D copies one track kind at a time");
                return;
            }
            ModalKind::Duplicate { locked: None }
        }
        Arm::Stretch => ModalKind::Stretch { start_edge: false },
    };
    state.modal = Some(Modal {
        kind,
        origin: state.selection.clone(),
        grab: ptr,
    });
}

fn hit_clip<T: Placement>(clips: &[T], tick: f32, sec_per_tick: f64) -> Option<usize> {
    clips
        .iter()
        .position(|c| tick >= c.start() as f32 && tick < (c.start() + c.span(sec_per_tick)) as f32)
}

/// Knife targeting (M4b.2): resolve a click at `tick` into the clip index
/// and a bar-snapped cut position, requiring at least one bar of clip on
/// both sides of the cut. `None` when the click misses every clip or the
/// snapped position is too close to either edge.
fn knife_target<T: Placement>(
    clips: &[T],
    tick: f32,
    bar_ticks: u32,
    sec_per_tick: f64,
) -> Option<(usize, u32)> {
    let index = hit_clip(clips, tick, sec_per_tick)?;
    let clip = &clips[index];
    let at = snap_bar(tick, bar_ticks);
    let left_ok = at >= clip.start() + bar_ticks;
    let right_ok = at + bar_ticks <= clip.start() + clip.span(sec_per_tick);
    (left_ok && right_ok).then_some((index, at))
}

/// Snaps to the nearest whole bar (0 stays 0). `bar_ticks` is the document's
/// bar length (U3a), so the snap follows the meter.
fn snap_bar(ticks: f32, bar_ticks: u32) -> u32 {
    let bar = bar_ticks.max(1) as f32;
    ((ticks / bar).round().max(0.0) as u32) * bar_ticks.max(1)
}

/// The insertion row a track-reorder drag points at, plus the screen y the
/// insert indicator should draw at. `rows` are the MIDI lane rects in
/// track order; `to` is the insertion index in ORIGINAL track coordinates
/// (0..=rows.len() — above row 0, between rows, or below the last row);
/// `to == from` / `to == from + 1` are identity drops the caller skips.
/// `None` when there are no rows.
fn reorder_target(ptr_y: f32, rows: &[Rect]) -> Option<(usize, f32)> {
    let first = rows.first()?;
    if ptr_y <= first.top() {
        return Some((0, first.top()));
    }
    for (i, rect) in rows.iter().enumerate() {
        if ptr_y <= rect.bottom() {
            let (to, y) = if ptr_y < rect.center().y {
                (i, rect.top())
            } else {
                (i + 1, rect.bottom())
            };
            return Some((to, y));
        }
    }
    let last = rows.last().expect("rows is non-empty (checked above)");
    Some((rows.len(), last.bottom()))
}

/// The old→new index permutation a MIDI-track reorder produces: the track
/// at `from` is removed and re-inserted so it ends up just before original
/// index `to` (the geometry's insertion semantics). Reordering is applied
/// atomically at the document level (track table, per-track node `track`
/// fields, `MidiTrack` cable indices all move together); this permutation
/// is the shared math the app applies and the tests verify.
pub(crate) fn track_permutation(len: usize, from: usize, to: usize) -> Vec<usize> {
    let insert_at = if to > from { to - 1 } else { to };
    (0..len)
        .map(|old| {
            if old == from {
                insert_at
            } else {
                let r = if old > from { old - 1 } else { old };
                if r >= insert_at { r + 1 } else { r }
            }
        })
        .collect()
}

/// Draws a read-only curve preview inside one control clip block (U12).
///
/// The preview samples the clip's curve with the **same** interpolation the
/// engine uses (`glitcherz_model::sample_curve`) and tiles it over the clip's
/// span by `loop_ticks` — exactly what plays, mirrored on the block. There is
/// no editing here: anchors are edited in the Edit tab (spec §8 D14).
fn draw_control_preview(painter: &egui::Painter, rect: Rect, clip: &ControlClip) {
    if clip.curve.is_empty() || clip.span_ticks == 0 || rect.width() < 6.0 || rect.height() < 6.0 {
        return;
    }
    let height = rect.height() - 2.0;
    // Anchors are normalized (`curve-map.md` D1): the preview shows the stored
    // shape, the same way the Edit tab does, with no target axis involved.
    let value_y = |value: f32| rect.bottom() - 1.0 - crate::curve::y_of(value) * height;
    let color = Color32::from_rgb(150, 200, 250);
    let span = f64::from(clip.span_ticks.max(1));
    let span_px = rect.width();
    let loop_ticks = f64::from(clip.loop_ticks.max(1));
    let mut prev: Option<Pos2> = None;
    let mut x = rect.left();
    while x <= rect.right() + 0.5 {
        let offset = (f64::from(x - rect.left()) / f64::from(span_px)) * span;
        let phase = offset.rem_euclid(loop_ticks);
        if let Some(value) = glitcherz_model::sample_curve(&clip.curve, clip.interp, phase) {
            let p = pos2(x, value_y(value));
            if let Some(prev) = prev {
                painter.line_segment([prev, p], Stroke::new(1.5, color));
            }
            prev = Some(p);
        }
        x += 2.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The tests lock the 4/4 behavior: bar_ticks is the document's meter in
    // production, the constant is the 4/4 case they exercise.
    use glitcherz_model::TICKS_PER_BAR;

    /// Seconds per tick at 120 BPM / 960 PPQ — the base-tempo conversion the
    /// placement helpers take (U11 D3); MIDI/control clips ignore it.
    const SPT: f64 = 60.0 / (120.0 * 960.0);

    /// U12: the read-only curve preview on a control clip block draws a
    /// normal clip and is a silent no-op for degenerate inputs (empty curve,
    /// zero span, tiny rect) — it must never panic inside the lane draw.
    #[test]
    fn control_preview_handles_degenerate_inputs() {
        let clip = ControlClip {
            span_ticks: TICKS_PER_BAR,
            loop_ticks: TICKS_PER_BAR,
            curve: vec![
                AutoPoint {
                    tick: 0,
                    value: 0.0,
                },
                AutoPoint {
                    tick: TICKS_PER_BAR / 2,
                    value: 127.0,
                },
            ],
            ..ControlClip::default()
        };
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(360.0, 120.0));
        let mut viewports = egui::ViewportIdMap::default();
        viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                inner_rect: Some(screen),
                ..Default::default()
            },
        );
        let raw = egui::RawInput {
            screen_rect: Some(screen),
            viewports,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |root| {
            egui::CentralPanel::default().show(root, |ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(300.0, 60.0), egui::Sense::hover());
                let painter = ui.painter_at(rect);
                draw_control_preview(&painter, rect, &clip);
                // Degenerate cases: empty curve, and a rect too small to draw.
                draw_control_preview(&painter, rect, &ControlClip::default());
                let tiny = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(3.0, 3.0));
                draw_control_preview(&painter, tiny, &clip);
            });
        });
        out.textures_delta.clear();
    }

    fn clip(start: u32, span: u32) -> MidiClip {
        MidiClip {
            start_ticks: start,
            span_ticks: span,
            ..MidiClip::new(start)
        }
    }

    #[test]
    fn knife_targets_the_bar_line_under_the_pointer() {
        let clips = vec![clip(0, 8 * TICKS_PER_BAR)];
        // 2.6 bars in → snaps to bar 3.
        let tick = 2.6 * TICKS_PER_BAR as f32;
        assert_eq!(
            knife_target(&clips, tick, TICKS_PER_BAR, SPT),
            Some((0, 3 * TICKS_PER_BAR)),
            "the cut lands on the nearest bar line"
        );
    }

    #[test]
    fn knife_rejects_cuts_without_a_bar_on_both_sides() {
        let clips = vec![clip(0, 2 * TICKS_PER_BAR)];
        // Middle of a 2-bar clip snaps to bar 1 — valid.
        assert_eq!(
            knife_target(&clips, TICKS_PER_BAR as f32, TICKS_PER_BAR, SPT),
            Some((0, TICKS_PER_BAR))
        );
        // Near the start edge: snap lands on bar 0 → zero-length left half.
        assert_eq!(
            knife_target(&clips, 0.2 * TICKS_PER_BAR as f32, TICKS_PER_BAR, SPT),
            None
        );
        // Near the end edge: snap lands on the clip's end → zero right half.
        assert_eq!(
            knife_target(&clips, 1.8 * TICKS_PER_BAR as f32, TICKS_PER_BAR, SPT),
            None
        );
        // One-bar clips can never be split.
        assert_eq!(
            knife_target(
                &[clip(4 * TICKS_PER_BAR, TICKS_PER_BAR)],
                4.4 * TICKS_PER_BAR as f32,
                TICKS_PER_BAR,
                SPT
            ),
            None
        );
    }

    #[test]
    fn knife_misses_empty_lane() {
        let clips = vec![clip(0, 4 * TICKS_PER_BAR)];
        assert_eq!(
            knife_target(&clips, 6.0 * TICKS_PER_BAR as f32, TICKS_PER_BAR, SPT),
            None
        );
    }

    #[test]
    fn snapping_follows_the_document_meter() {
        // 6/8 bars are 2880 ticks: a clip of four 4/4 bars spans 15360 ticks,
        // i.e. 5.33 six-eight bars, so a pointer at 1.6 bars of 6/8 snaps to
        // bar 2 = 5760 — not to a 4/4 line.
        const SIX_EIGHT: u32 = 2880;
        let clips = vec![clip(0, 4 * TICKS_PER_BAR)];
        assert_eq!(
            knife_target(&clips, 1.6 * SIX_EIGHT as f32, SIX_EIGHT, SPT),
            Some((0, 2 * SIX_EIGHT))
        );
        assert_eq!(snap_bar(1.6 * SIX_EIGHT as f32, SIX_EIGHT), 2 * SIX_EIGHT);
        // A 3/4 clip of exactly one bar still cannot be cut (one bar minimum
        // on both sides), even though it is shorter than a 4/4 bar.
        let short = vec![clip(0, SIX_EIGHT)];
        assert_eq!(
            knife_target(&short, 0.4 * SIX_EIGHT as f32, SIX_EIGHT, SPT),
            None
        );
    }

    #[test]
    fn split_halves_keep_start_end_and_content() {
        let mut clip = MidiClip::new(2 * TICKS_PER_BAR);
        clip.span_ticks = 4 * TICKS_PER_BAR;
        clip.loop_ticks = 4 * TICKS_PER_BAR;
        clip.sequence.notes = vec![
            Note {
                start: 0,
                length: 240,
                note: 60,
                ..Note::default()
            },
            // Crosses the cut at 2 bars: continues in both halves.
            Note {
                start: 3 * i64::from(TICKS_PER_BAR) / 2,
                length: i64::from(TICKS_PER_BAR),
                note: 62,
                ..Note::default()
            },
            Note {
                start: 5 * i64::from(TICKS_PER_BAR) / 2,
                length: 240,
                note: 64,
                ..Note::default()
            },
        ];
        let mut clips = vec![clip];
        let at = 4 * TICKS_PER_BAR;
        assert!(split_clip_in(&mut clips, at, 0, SPT));
        assert_eq!(clips.len(), 2);
        // Left: same start, ends at the cut, single-period.
        assert_eq!(clips[0].start_ticks, 2 * TICKS_PER_BAR);
        assert_eq!(clips[0].span_ticks, 2 * TICKS_PER_BAR);
        assert_eq!(clips[0].loop_ticks, 2 * TICKS_PER_BAR);
        // Left keeps every note in the data (the past-span one included: it
        // stays hidden until the clip grows back over it).
        let left = clips[0].notes();
        assert_eq!(left.len(), 3);
        assert_eq!(left[1].note, 62);
        assert_eq!(left[1].length, i64::from(TICKS_PER_BAR));
        // Right: starts at the cut, runs to the old end, single-period.
        assert_eq!(clips[1].start_ticks, at);
        assert_eq!(clips[1].span_ticks, 2 * TICKS_PER_BAR);
        assert_eq!(clips[1].loop_ticks, 2 * TICKS_PER_BAR);
        let right = clips[1].notes();
        assert_eq!(right.len(), 2);
        // The crossing note's tail continues at the cut...
        assert_eq!(right[0].note, 62);
        assert_eq!(right[0].start, 0);
        assert_eq!(right[0].length, i64::from(TICKS_PER_BAR) / 2);
        // ...and the shifted note keeps its offset from the cut.
        assert_eq!(right[1].note, 64);
        assert_eq!(right[1].start, i64::from(TICKS_PER_BAR) / 2);
    }

    #[test]
    fn split_shifts_control_curve_anchors() {
        let mut clip = ControlClip::new(TICKS_PER_BAR);
        clip.span_ticks = 2 * TICKS_PER_BAR;
        clip.loop_ticks = 2 * TICKS_PER_BAR;
        clip.curve = vec![
            AutoPoint {
                tick: 0,
                value: 0.0,
            },
            AutoPoint {
                tick: TICKS_PER_BAR,
                value: 0.5,
            },
            AutoPoint {
                tick: 3 * TICKS_PER_BAR / 2,
                value: 1.0,
            },
        ];
        let mut clips = vec![clip];
        let at = 2 * TICKS_PER_BAR;
        assert!(split_clip_in(&mut clips, at, 0, SPT));
        // Left keeps its anchors untouched (data survives a shorten).
        assert_eq!(clips[0].span_ticks, TICKS_PER_BAR);
        assert_eq!(clips[0].loop_ticks, TICKS_PER_BAR);
        assert_eq!(clips[0].curve.len(), 3);
        // Right carries the anchors from the cut onward, shifted.
        assert_eq!(clips[1].start_ticks, at);
        assert_eq!(clips[1].span_ticks, TICKS_PER_BAR);
        assert_eq!(clips[1].loop_ticks, TICKS_PER_BAR);
        assert_eq!(
            clips[1].curve,
            vec![
                AutoPoint {
                    tick: 0,
                    value: 0.5
                },
                AutoPoint {
                    tick: TICKS_PER_BAR / 2,
                    value: 1.0
                },
            ]
        );
    }

    #[test]
    fn split_refuses_edges_and_out_of_bounds() {
        let mut clips = vec![clip(0, 2 * TICKS_PER_BAR)];
        assert!(!split_clip_in(&mut clips, 0, 0, SPT), "at the start");
        assert!(
            !split_clip_in(&mut clips, 2 * TICKS_PER_BAR, 0, SPT),
            "at the end"
        );
        assert!(
            !split_clip_in(&mut clips, TICKS_PER_BAR, 5, SPT),
            "bad index"
        );
        assert_eq!(clips.len(), 1, "failed splits must not mutate");
    }

    #[test]
    fn split_respects_the_per_track_cap() {
        let mut clips: Vec<MidiClip> = (0..MAX_CLIPS_PER_TRACK)
            .map(|i| clip(i as u32 * 2 * TICKS_PER_BAR, 2 * TICKS_PER_BAR))
            .collect();
        assert!(!split_clip_in(&mut clips, TICKS_PER_BAR, 0, SPT));
        assert_eq!(clips.len(), MAX_CLIPS_PER_TRACK);
    }

    #[test]
    fn row_delta_clamps_globally_not_per_clip() {
        // Selection on rows 0 and 1 of three rows: delta +2 must clamp to
        // +1 so the whole group fits, not push row 1 out.
        let sel = vec![
            ClipSel {
                lane: TimelineLane::Midi,
                track: 0,
                clip: 0,
            },
            ClipSel {
                lane: TimelineLane::Midi,
                track: 1,
                clip: 0,
            },
        ];
        assert_eq!(clamp_row_delta(2, &sel, 3), 1);
        // Track 0 anchors the group: no downward move at all.
        assert_eq!(clamp_row_delta(-5, &sel, 3), 0);
        // Selection on rows 1 and 2 can move down by one, not further.
        let low = vec![
            ClipSel {
                lane: TimelineLane::Midi,
                track: 1,
                clip: 0,
            },
            ClipSel {
                lane: TimelineLane::Midi,
                track: 2,
                clip: 0,
            },
        ];
        assert_eq!(clamp_row_delta(-1, &low, 3), -1);
        assert_eq!(clamp_row_delta(-5, &low, 3), -1);
    }

    #[test]
    fn full_coverage_is_refused_but_crossing_is_not() {
        // Target 0..8 fully covers 2..4: refused.
        assert!(fully_covers((0, 8), &[(2, 2)]));
        // Target 0..4 crosses 2..6: partial overlap, allowed.
        assert!(!fully_covers((0, 4), &[(2, 4)]));
        // Touching edges are not coverage.
        assert!(!fully_covers((0, 4), &[(4, 2)]));
    }

    #[test]
    fn grab_moves_by_bars_and_rows_within_kind() {
        let midi = vec![MidiTrack {
            clips: vec![clip(2 * TICKS_PER_BAR, 2 * TICKS_PER_BAR)],
            ..MidiTrack::default()
        }];
        let modal = Modal {
            kind: ModalKind::Grab { locked: None },
            origin: vec![ClipSel {
                lane: TimelineLane::Midi,
                track: 0,
                clip: 0,
            }],
            grab: pos2(100.0, 100.0),
        };
        // Pointer 1.6 bars right, one row down.
        let ptr = pos2(100.0 + 1.6 * 56.0, 100.0 + ROW_HEIGHT);
        let targets = grab_targets(
            &modal,
            Some(ptr),
            56.0,
            TICKS_PER_BAR,
            ROW_HEIGHT,
            2,
            1,
            0,
            &midi,
            &[],
            &[],
            SPT,
        )
        .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].to_track, 1);
        // 1.6 bars → rounds to 2 bars.
        assert_eq!(targets[0].start_ticks, 4 * TICKS_PER_BAR);
        // X locks the movement to the time axis: the row delta dies.
        let locked = Modal {
            kind: ModalKind::Grab {
                locked: Some(Axis::Time),
            },
            ..modal.clone()
        };
        let targets = grab_targets(
            &locked,
            Some(ptr),
            56.0,
            TICKS_PER_BAR,
            ROW_HEIGHT,
            2,
            1,
            0,
            &midi,
            &[],
            &[],
            SPT,
        )
        .unwrap();
        assert_eq!(targets[0].to_track, 0);
        assert_eq!(targets[0].start_ticks, 4 * TICKS_PER_BAR);
        // Y locks the movement to the rows: the bar delta dies.
        let row_locked = Modal {
            kind: ModalKind::Grab {
                locked: Some(Axis::Row),
            },
            ..modal.clone()
        };
        let targets = grab_targets(
            &row_locked,
            Some(ptr),
            56.0,
            TICKS_PER_BAR,
            ROW_HEIGHT,
            2,
            1,
            0,
            &midi,
            &[],
            &[],
            SPT,
        )
        .unwrap();
        assert_eq!(targets[0].to_track, 1);
        assert_eq!(targets[0].start_ticks, 2 * TICKS_PER_BAR);
    }

    #[test]
    fn timeline_state_defaults() {
        let state = TimelineState::default();
        assert_eq!(state.px_per_bar, 56.0);
        // View navigation starts at the identity (base row height, no
        // pending reorder, no recorded row geometry yet).
        assert_eq!(state.row_zoom, 1.0);
        assert_eq!(state.reorder, None);
        assert_eq!(state.rows_rect, None);
    }

    /// The reorder permutation: applying `perm[old] = new` to a track list
    /// must equal remove(from) + insert at the original-index target.
    #[test]
    fn track_permutation_matches_remove_insert() {
        // Track 0 dropped below track 2 ([A,B,C] → [B,A,C]).
        assert_eq!(track_permutation(3, 0, 2), vec![1, 0, 2]);
        // Track 2 lifted to the top ([A,B,C] → [C,A,B]).
        assert_eq!(track_permutation(3, 2, 0), vec![1, 2, 0]);
        // Track 1 pushed to the end ([A,B,C] → [A,C,B]).
        assert_eq!(track_permutation(3, 1, 3), vec![0, 2, 1]);
        // Identity neighbours: dropping onto yourself or just below is a
        // no-op the caller filters, and the permutation reflects that.
        assert_eq!(track_permutation(3, 1, 1), vec![0, 1, 2]);
        assert_eq!(track_permutation(3, 1, 2), vec![0, 1, 2]);
        // The permutation is a bijection for every from/to pair.
        for len in 1..=6usize {
            for from in 0..len {
                for to in 0..=len {
                    let perm = track_permutation(len, from, to);
                    let mut seen = vec![false; len];
                    for &new in &perm {
                        assert!(!seen[new], "duplicate target index in {perm:?}");
                        seen[new] = true;
                    }
                }
            }
        }
    }

    /// The reorder drop target: pointer position → insertion row +
    /// indicator y, in original track coordinates.
    #[test]
    fn reorder_target_picks_the_insertion_row() {
        let rows = vec![
            Rect::from_min_size(pos2(100.0, 0.0), Vec2::new(500.0, 28.0)),
            Rect::from_min_size(pos2(100.0, 28.0), Vec2::new(500.0, 28.0)),
            Rect::from_min_size(pos2(100.0, 56.0), Vec2::new(500.0, 28.0)),
        ];
        // Above everything: insert at the top.
        assert_eq!(reorder_target(-10.0, &rows), Some((0, 0.0)));
        // Upper half of row 1: insert before it.
        assert_eq!(reorder_target(30.0, &rows), Some((1, 28.0)));
        // Lower half of row 1: insert after it.
        assert_eq!(reorder_target(48.0, &rows), Some((2, 56.0)));
        // Below everything: insert at the end.
        assert_eq!(reorder_target(500.0, &rows), Some((3, 84.0)));
        // No rows: nothing to target.
        assert_eq!(reorder_target(10.0, &[]), None);
    }

    #[test]
    fn stretch_keeps_one_bar_minimum() {
        let midi = vec![MidiTrack {
            clips: vec![clip(2 * TICKS_PER_BAR, 2 * TICKS_PER_BAR)],
            ..MidiTrack::default()
        }];
        let grab = pos2(0.0, 0.0);
        // End edge, pointer 3 bars left: span would go negative → clamps
        // to one bar.
        let end = Modal {
            kind: ModalKind::Stretch { start_edge: false },
            origin: vec![ClipSel {
                lane: TimelineLane::Midi,
                track: 0,
                clip: 0,
            }],
            grab,
        };
        let targets = stretch_targets(
            &end,
            Some(pos2(-3.0 * 56.0, 0.0)),
            56.0,
            TICKS_PER_BAR,
            &midi,
            &[],
            &[],
            SPT,
        )
        .unwrap();
        assert_eq!(targets[0].span_ticks, TICKS_PER_BAR);
        assert_eq!(targets[0].start_ticks, 2 * TICKS_PER_BAR);
        // Start edge, pointer 3 bars right: start would pass the end →
        // clamps to one bar remaining.
        let start = Modal {
            kind: ModalKind::Stretch { start_edge: true },
            origin: vec![ClipSel {
                lane: TimelineLane::Midi,
                track: 0,
                clip: 0,
            }],
            grab,
        };
        let targets = stretch_targets(
            &start,
            Some(pos2(3.0 * 56.0, 0.0)),
            56.0,
            TICKS_PER_BAR,
            &midi,
            &[],
            &[],
            SPT,
        )
        .unwrap();
        assert_eq!(targets[0].start_ticks, 3 * TICKS_PER_BAR);
        assert_eq!(targets[0].span_ticks, TICKS_PER_BAR);
    }

    // ---- U11: audio clips take the same modal geometry ------------------

    fn audio_clip(start_ticks: u32, source_offset: f64, source_len: f64) -> AudioClip {
        AudioClip {
            path: "a.wav".to_string(),
            start_ticks,
            source_offset,
            source_len,
            gain: 1.0,
            ..AudioClip::default()
        }
    }

    /// U11 D3: the audio tick span is derived from the seconds window at the
    /// base tempo (2.0 s = one 3840-tick bar at 120 BPM).
    #[test]
    fn audio_clip_geometry_derives_the_span_from_seconds() {
        let audio = vec![AudioTrack {
            name: String::new(),
            clips: vec![audio_clip(TICKS_PER_BAR, 0.25, 4.0)],
        }];
        let sel = ClipSel {
            lane: TimelineLane::Audio,
            track: 0,
            clip: 0,
        };
        let (start, span) = clip_geometry(&[], &[], &audio, SPT, sel).unwrap();
        assert_eq!(start, TICKS_PER_BAR);
        assert_eq!(span, 2 * TICKS_PER_BAR, "4.0 s = two bars at 120 BPM");
    }

    /// U11: the G modal moves audio clips (three-table geometry).
    #[test]
    fn audio_clips_arm_the_grab_modal() {
        let audio = vec![AudioTrack {
            name: String::new(),
            clips: vec![audio_clip(TICKS_PER_BAR, 0.0, 2.0)],
        }];
        let modal = Modal {
            kind: ModalKind::Grab { locked: None },
            origin: vec![ClipSel {
                lane: TimelineLane::Audio,
                track: 0,
                clip: 0,
            }],
            grab: pos2(100.0, 100.0),
        };
        let ptr = pos2(100.0 + 2.0 * 56.0, 100.0);
        let targets = grab_targets(
            &modal,
            Some(ptr),
            56.0,
            TICKS_PER_BAR,
            ROW_HEIGHT,
            0,
            0,
            1,
            &[],
            &[],
            &audio,
            SPT,
        )
        .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].to_track, 0);
        assert_eq!(targets[0].start_ticks, 3 * TICKS_PER_BAR);
    }

    /// U11: the T modal changes an audio clip's derived span (the app layer
    /// turns it into a seconds-window trim).
    #[test]
    fn audio_clip_stretch_targets_grow_the_derived_span() {
        let audio = vec![AudioTrack {
            name: String::new(),
            clips: vec![audio_clip(TICKS_PER_BAR, 0.0, 4.0)],
        }];
        let modal = Modal {
            kind: ModalKind::Stretch { start_edge: false },
            origin: vec![ClipSel {
                lane: TimelineLane::Audio,
                track: 0,
                clip: 0,
            }],
            grab: pos2(100.0, 100.0),
        };
        let ptr = pos2(100.0 + 1.0 * 56.0, 100.0);
        let targets = stretch_targets(
            &modal,
            Some(ptr),
            56.0,
            TICKS_PER_BAR,
            &[],
            &[],
            &audio,
            SPT,
        )
        .unwrap();
        assert_eq!(targets[0].start_ticks, TICKS_PER_BAR);
        assert_eq!(targets[0].span_ticks, 3 * TICKS_PER_BAR);
    }
}
