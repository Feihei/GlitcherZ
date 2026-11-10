//! Arrangement timeline view (timeline spec T3, `.agents/spec/timeline.md`).
//!
//! Rows = tracks (MIDI + control); each row shows its clip blocks on an
//! absolute-tick ruler (bars). Interactions:
//!
//! - drag a clip body → move (snapped to bars);
//! - drag its right edge → extend/trim the clip time; a single-period clip
//!   grows its content with the container, a tiled clip keeps its repeat
//!   count and scales the period;
//! - drag its left edge (I6) → trim the head: the start moves right and the
//!   content after the cut stays in place (a single-period MIDI/control clip
//!   is really cut); a tiled clip only shortens its container;
//! - Alt+T → tile the selection: drag to set the content period
//!   (`loop_ticks`), bar-aligned; one drop = one undo step;
//! - double-click an empty lane spot → insert a one-bar clip;
//! - double-click a clip → jump to the Edit tab (piano roll / curve lane);
//!   Alt+double-click renames it in place, `F2` renames the selected clip;
//! - Ctrl+middle-click a clip toggles its temporary mute, `H` mutes the
//!   selection and `Alt+H` reveals (unmutes) everything; a muted clip is
//!   drawn faded and stays fully editable;
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
    AudioClip, AudioTrack, AutoPoint, ClipGroup, ControlClip, ControlTrack, CtrlEvent,
    MAX_ARRANGEMENT_TICKS, MAX_CLIPS_PER_TRACK, Marker, MidiClip, MidiTrack, Note, SongLoop,
};

/// Absolute arrangement ceiling in ticks (U3a). The bound is a **tick** count,
/// not a bar count: 256 bars at 4/4, 292 at 7/8, 341 at 3/4 — placing content
/// is limited by time, not by how the time is grouped.
const ARRANGEMENT_LIMIT_TICKS: u32 = MAX_ARRANGEMENT_TICKS as u32;

/// Fixed width of the row-header column.
pub const HEADER_WIDTH: f32 = 108.0;
/// Width of the reorder grab handle at the left of every row header.
const REORDER_GRIP_WIDTH: f32 = 12.0;
/// Height of one track row at `row_zoom` = 1.
///
/// 40 px is the comfortable default: a track header has to hold its name at
/// 11 pt, and a clip's label plus its waveform / curve preview need the
/// vertical room. Ctrl+Alt+wheel scales it (0.5×–3×).
pub const ROW_HEIGHT: f32 = 40.0;
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

/// One tile edit of the tile modal (Shift+T / Alt+T): the container grows to
/// `span_ticks` while the content period stays `loop_ticks` — the clip's
/// full length at arm time, so the WHOLE clip repeats as one unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipTileEdit {
    pub sel: ClipSel,
    pub span_ticks: u32,
    pub loop_ticks: u32,
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
    /// Alt+T: set the content period of every selected clip, bar-aligned.
    /// The container length is unchanged, so this changes the repeat count.
    Tile,
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

/// Which boundary of a clip follows the pointer during a resize drag (I6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeEdge {
    /// Left boundary: trim the head. The content is re-based from the drag
    /// origin so the material after the cut stays where it is.
    Start,
    /// Right boundary: extend / trim the tail (the pre-I6 behavior).
    End,
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
        edge: ResizeEdge,
        /// Snapshot at drag start so per-frame stretching scales from the
        /// original placement (not from the previous frame's result).
        origin_start: u32,
        origin_span: u32,
        origin_loop: u32,
    },
}

/// Drag-origin snapshot of a head trim (I6).
///
/// `retain_from` rewrites the content, so a frame that started from the
/// *live* clip would remove a little more material on every frame. The
/// snapshot is taken by the first trim frame and dropped when the drag
/// ends; each frame rebuilds the target from it.
#[derive(Debug, Clone)]
pub(crate) struct HeadTrim {
    pub(crate) sel: ClipSel,
    pub(crate) origin: HeadTrimOrigin,
}

#[derive(Debug, Clone)]
pub(crate) enum HeadTrimOrigin {
    Midi(Box<MidiClip>),
    Control(Box<ControlClip>),
    /// Audio needs no content copy — the trim window is a pair of scalars,
    /// and only the origin offset is read back.
    Audio {
        offset: f64,
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

    /// Track-reorder drag (header grab handle): the lane and track index the
    /// grab started on.
    reorder: Option<RowKind>,
    /// Song-loop drag on the ruler (U1): `None` means an in-progress ruler
    /// drag is a plain seek scrub instead.
    loop_drag: Option<LoopDrag>,
    /// Inline track-rename editor (right-click a track name, type, Enter
    /// commits): which lane and track, plus the draft text.
    rename: Option<(TimelineLane, usize, String)>,
    /// The right-click that opens the editor lands one frame before the text
    /// field exists, so the first frame that draws it asks for focus.
    rename_focus: bool,
    /// Inline clip-rename editor (W3a: F2 on a selected clip, or Alt+double
    /// click a clip block): which clip plus the draft text.
    clip_rename: Option<(ClipSel, String)>,
    /// Same one-frame focus hand-off as `rename_focus`.
    clip_rename_focus: bool,
    /// W3b: `Alt` + click picked a single clip out of its group. The next
    /// plain click / marquee clears it, and group expansion is skipped while
    /// it is set (otherwise the expanded selection would snap back every
    /// frame).
    break_group: bool,
    /// Drag-origin snapshot of an in-flight head trim (I6). `None` between
    /// gestures; the first trim frame fills it and `drag_stopped` drops it.
    pub(crate) head_trim: Option<HeadTrim>,
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
            clip_rename: None,
            clip_rename_focus: false,
            break_group: false,
            head_trim: None,
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

    /// Whether the live modal is the Alt+T tile modal (integration
    /// observability — the kind itself stays private).
    pub fn tiling_active(&self) -> bool {
        matches!(self.modal.as_ref().map(|m| m.kind), Some(ModalKind::Tile))
    }

    /// The current selection (integration observability — used by the
    /// focus tests to assert auto-selection after a double-click insert).
    pub fn selected(&self) -> &[ClipSel] {
        &self.selection
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
        /// Explicit content period from the resize gesture, computed from the
        /// drag-start snapshot. `None` = keep the current loop (a move).
        loop_ticks: Option<u32>,
    },
    /// One frame of a head trim (I6): the left edge follows the pointer and
    /// the content is re-based from the drag-origin snapshot. The caller
    /// merges the gesture into a single undo step.
    TrimClipHead {
        lane: TimelineLane,
        track: usize,
        clip: usize,
        /// Absolute start after the trim (bar-snapped, ≥ 1 bar kept).
        start_ticks: u32,
        span_ticks: u32,
        /// Ticks removed from the head since the drag started (`> 0`).
        delta_ticks: u32,
    },
    /// End a head-trim gesture: drop the origin snapshot so the next trim
    /// starts from the then-current clip.
    EndClipTrim,
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
    /// Tile every clip in the list (tile modal finalize): each container
    /// grows to `span_ticks` while the content period stays `loop_ticks`
    /// (the clip's armed length). One undo step.
    TileClipsBatch {
        tiles: Vec<ClipTileEdit>,
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
    /// Reorder one track family (MIDI / curve / audio): the track at `from`
    /// is re-inserted so it ends up just before original index `to` (an
    /// insertion index, 0..=len; `to == from` / `to == from + 1` are identity
    /// drops the caller skips). One undo step — the document track table, the
    /// per-track node `track` fields and the lane's cable indices all move
    /// together.
    ReorderTrack {
        lane: TimelineLane,
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
    /// Rename one clip from the inline block editor (W3a): F2 on a selected
    /// clip, or Alt+double-click a block. Empty clears back to the content
    /// summary. One undo step.
    RenameClip {
        lane: TimelineLane,
        track: usize,
        clip: usize,
        name: String,
    },
    /// W3b: put every selected clip into one new named group (auto-named
    /// `Group N`, palette colour). Works across tracks and lane kinds.
    GroupClips {
        clips: Vec<ClipSel>,
    },
    /// W3b: dissolve every group touched by the selection — **all** members
    /// lose the tag, even the ones outside the selection.
    UngroupClips {
        clips: Vec<ClipSel>,
    },
    /// B2: set the temporary mute flag of every listed clip (one undo step;
    /// clips already in that state are skipped, so the action is idempotent).
    /// `muted: false` clears it — the same action serves H (mute selection),
    /// Alt+H (unmute the whole arrangement) and the Ctrl+middle-click toggle.
    SetClipMuted {
        clips: Vec<ClipSel>,
        muted: bool,
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
    /// Clip-group table (W3b): member clips draw a colour stripe and a
    /// click on one member selects the whole group.
    pub clip_groups: &'a [ClipGroup],
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
    /// Optional user-facing clip name (W3a). Empty = unnamed.
    fn name(&self) -> &str;
    fn set_name(&mut self, name: String);
    /// Content period (W2): the span repeats this every `loop_len` ticks.
    /// Audio clips have no repeat semantics — they report their own span.
    fn loop_len(&self, sec_per_tick: f64) -> u32;
    /// Optional clip-group id (W3b). Ungrouped clips return `None`.
    fn group(&self) -> Option<u32>;
    fn set_group(&mut self, group: Option<u32>);
    /// Temporary mute (B2): the clip keeps everything, playback skips it.
    fn muted(&self) -> bool;
    fn set_muted(&mut self, muted: bool);
}

/// W2: the loop length after a span edit that does NOT explicitly ask for a
/// period (currently only moves, where the span is unchanged). Kept for the
/// move path: a moved clip must not change its content period.
pub(crate) fn resolve_loop(current: u32, span: u32, requested: Option<u32>) -> u32 {
    match requested {
        Some(v) => v.clamp(1, span.max(1)),
        None => current.clamp(1, span.max(1)),
    }
}

/// T semantics (2026-11-04): the content period after a **stretch/resize**.
///
/// - Single-period clip (`loop == span`): the content grows with the
///   container, so `T` genuinely extends the clip's editable time — the
///   piano roll's range is `loop_ticks`.
/// - Tiled clip (`loop < span`): keep the repeat count, scale the period
///   with the container.
pub(crate) fn resolve_stretch_loop(current_loop: u32, old_span: u32, new_span: u32) -> u32 {
    let old_span = old_span.max(1);
    let new_span = new_span.max(1);
    let current_loop = current_loop.max(1);
    if current_loop >= old_span {
        new_span
    } else {
        let repeats = f64::from(old_span) / f64::from(current_loop);
        ((f64::from(new_span) / repeats).round() as u32)
            .max(1)
            .min(new_span)
    }
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
    fn name(&self) -> &str {
        &self.name
    }
    fn set_name(&mut self, name: String) {
        self.name = name;
    }
    fn loop_len(&self, _sec_per_tick: f64) -> u32 {
        self.loop_ticks
    }
    fn group(&self) -> Option<u32> {
        self.group
    }
    fn set_group(&mut self, group: Option<u32>) {
        self.group = group;
    }
    fn muted(&self) -> bool {
        self.muted
    }
    fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
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
    fn name(&self) -> &str {
        &self.name
    }
    fn set_name(&mut self, name: String) {
        self.name = name;
    }
    fn loop_len(&self, _sec_per_tick: f64) -> u32 {
        self.loop_ticks
    }
    fn group(&self) -> Option<u32> {
        self.group
    }
    fn set_group(&mut self, group: Option<u32>) {
        self.group = group;
    }
    fn muted(&self) -> bool {
        self.muted
    }
    fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
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
    fn name(&self) -> &str {
        &self.name
    }
    fn set_name(&mut self, name: String) {
        self.name = name;
    }
    fn loop_len(&self, sec_per_tick: f64) -> u32 {
        self.span(sec_per_tick)
    }
    fn group(&self) -> Option<u32> {
        self.group
    }
    fn set_group(&mut self, group: Option<u32>) {
        self.group = group;
    }
    fn muted(&self) -> bool {
        self.muted
    }
    fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }
}

/// Splits `clips[index]` at `at` in place (M4b.2): the left half keeps the
/// start with a shortened span, the right half is inserted at `index + 1`
/// with the content from the cut onward shifted to its origin (notes
/// crossing the cut continue in both halves). Only single-period clips
/// (`loop == span`) are splittable: cutting a tiled clip would need a
/// phase-relative loop origin, which the model does not carry (W2). Returns
/// false untouched when the index is out of bounds, the track is full, `at`
/// is not strictly inside the clip, or the clip is tiled.
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
    if at <= start
        || at >= end
        || clips.len() >= MAX_CLIPS_PER_TRACK
        || c.loop_len(sec_per_tick) < c.span(sec_per_tick)
    {
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

/// W3a: which clip `F2` renames on a row — the first selection entry that
/// belongs to this lane/track, provided the clip index still exists.
fn clip_rename_target<T: Placement>(
    selection: &[ClipSel],
    lane: TimelineLane,
    track: usize,
    clips: &[T],
) -> Option<(ClipSel, String)> {
    let sel = selection
        .iter()
        .find(|s| s.lane == lane && s.track == track)?;
    Some((*sel, clips.get(sel.clip)?.name().to_owned()))
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

/// Clamps a row delta so EVERY kind's selection group stays inside its own
/// rows (rigid move: one delta for everyone; each kind clamps against its
/// own row count and the delta must satisfy all of them at once).
fn clamp_row_delta(
    row_delta: i32,
    sel: &[ClipSel],
    rows_midi: usize,
    rows_control: usize,
    rows_audio: usize,
) -> i32 {
    if sel.is_empty() {
        return 0;
    }
    let mut lo = i32::MIN;
    let mut hi = i32::MAX;
    for (lane, rows) in [
        (TimelineLane::Midi, rows_midi),
        (TimelineLane::Control, rows_control),
        (TimelineLane::Audio, rows_audio),
    ] {
        let tracks: Vec<i32> = sel
            .iter()
            .filter(|s| s.lane == lane)
            .map(|s| s.track as i32)
            .collect();
        if tracks.is_empty() || rows == 0 {
            continue;
        }
        let min_t = *tracks.iter().min().unwrap();
        let max_t = *tracks.iter().max().unwrap();
        lo = lo.max(-min_t);
        hi = hi.min((rows as i32 - 1) - max_t);
    }
    if lo > hi {
        return 0;
    }
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
    // A mixed selection moves as one rigid group: each kind clamps the
    // delta against its own row count and all limits must hold at once.
    row_delta = clamp_row_delta(
        row_delta,
        &modal.origin,
        rows_midi,
        rows_control,
        rows_audio,
    );
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

/// Looks up a selected clip's content period. Audio clips have no repeat
/// semantics, so they report `None`.
fn clip_loop_len(
    midi_tracks: &[MidiTrack],
    control_tracks: &[ControlTrack],
    sec_per_tick: f64,
    sel: ClipSel,
) -> Option<u32> {
    match sel.lane {
        TimelineLane::Control => Some(
            control_tracks
                .get(sel.track)?
                .clips
                .get(sel.clip)?
                .loop_len(sec_per_tick),
        ),
        TimelineLane::Midi => Some(
            midi_tracks
                .get(sel.track)?
                .clips
                .get(sel.clip)?
                .loop_len(sec_per_tick),
        ),
        TimelineLane::Audio => None,
    }
}

/// The tile modal's per-clip edits (Shift+T / Alt+T). The pointer adds or
/// removes whole repeats, one per period-width of travel:
/// - a single-period clip tiles as a whole (period = its full length);
/// - an ALREADY-TILED clip keeps its content period and the pointer just
///   adjusts the repeat count — no nested tiling (2026-11-04).
///   Until the pointer moves the clip keeps its exact armed span. `None`
///   when the pointer is unavailable or the modal is not a Tile.
#[allow(clippy::too_many_arguments)]
fn tile_targets(
    modal: &Modal,
    ptr: Option<Pos2>,
    px_per_bar: f32,
    bar_ticks: u32,
    midi_tracks: &[MidiTrack],
    control_tracks: &[ControlTrack],
    sec_per_tick: f64,
) -> Option<Vec<ClipTileEdit>> {
    if !matches!(modal.kind, ModalKind::Tile) {
        return None;
    }
    let ptr = ptr?;
    let bar = f64::from(bar_ticks.max(1));
    let delta_bars = if modal.grab.x.is_finite() {
        f64::from(ptr.x - modal.grab.x) / f64::from(px_per_bar.max(0.001))
    } else {
        0.0
    };
    Some(
        modal
            .origin
            .iter()
            .filter_map(|sel| {
                let (start, span) =
                    clip_geometry(midi_tracks, control_tracks, &[], sec_per_tick, *sel)?;
                let span = span.max(1);
                let period = clip_loop_len(midi_tracks, control_tracks, sec_per_tick, *sel)
                    .unwrap_or(span)
                    .clamp(1, span);
                // The pointer adds/removes whole repeats, one per period-width
                // of travel: a single-period clip tiles as a whole, an
                // already-tiled clip just adjusts its repeat count (2026-11-04,
                // no nested tiling). Until the pointer moves the clip keeps
                // its exact armed span.
                let span_ticks = if delta_bars == 0.0 {
                    span
                } else {
                    let period_bars = (f64::from(period) / bar).max(f64::EPSILON);
                    let base = (f64::from(span) / f64::from(period)).round().max(1.0);
                    let step = (delta_bars.abs() / period_bars).floor();
                    let repeats = if delta_bars > 0.0 {
                        base + step
                    } else {
                        (base - step).max(1.0)
                    };
                    period.saturating_mul(repeats as u32)
                };
                let room = f64::from(ARRANGEMENT_LIMIT_TICKS.saturating_sub(start).max(1))
                    / f64::from(period.max(1));
                let span_ticks =
                    span_ticks.min((room.floor().max(1.0) as u32).saturating_mul(period));
                Some(ClipTileEdit {
                    sel: *sel,
                    span_ticks,
                    loop_ticks: period,
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
    // 2026-11-04: the tile shortcut (Shift+T, Alt+T as a fallback) is a
    // view-level command, not a focus-gated lane key — losing lane focus
    // while a modifier is pressed would otherwise swallow it; run it before
    // the rows. Any MIDI/curve clip can tile: periods go down to a quarter
    // bar, so even a single-bar clip halves into two repeats.
    if state.modal.is_none() && !state.knife_armed && take_tile_shortcut(ui.ctx()) {
        state.knife_armed = false;
        // Tile latches its grab on the first real pointer (the shortcut may
        // arrive while Alt has dropped the cursor on Windows).
        arm_modal(state, Arm::Tile, ui.ctx().pointer_hover_pos());
    }
    // Ctrl+A / Ctrl+Shift+A (W3c): select every clip in the arrangement, or
    // drop the selection. A view-level command like the tile shortcut — the
    // lane rows only ever see their own track, and "all" is by definition
    // cross-lane. The inline rename editor (a live text edit) keeps the combo
    // for itself.
    if state.modal.is_none() && !ui.ctx().text_edit_focused() {
        let (deselect, select_all) = ui.ctx().input_mut(|i| {
            let mut ctrl_shift = Modifiers::CTRL;
            ctrl_shift.shift = true;
            // Shift first: `consume_key` matches modifiers logically, so the
            // plain Ctrl+A pattern would eat Ctrl+Shift+A as well.
            let clear = i.consume_key(ctrl_shift, Key::A);
            let all = i.consume_key(Modifiers::CTRL, Key::A);
            (clear, all)
        });
        if deselect {
            state.selection.clear();
            state.marquee = None;
            state.knife_armed = false;
            state.set_status("Selection cleared");
        } else if select_all {
            state.selection = all_clips(midi_tracks, control_tracks, audio_tracks);
            state.marquee = None;
            state.knife_armed = false;
            state.break_group = false;
            let count = state.selection.len();
            if count == 0 {
                state.set_status("Select all: the arrangement is empty");
            } else {
                state.set_status(format!("{count} clips selected"));
            }
        }
    }
    // B2: H mutes the selected clips, Alt+H reveals (unmutes) everything in
    // the arrangement — Blender's hide / reveal-all, mapped onto clip mute.
    // View-level like Ctrl+A: the selection is cross-lane by definition.
    // Gated on the lane owning the keyboard because plain `H` is also a
    // virtual-piano key (FL layout), exactly like G / T / K.
    if state.modal.is_none()
        && !ui.ctx().text_edit_focused()
        && state.owns_keyboard_focus(ui.ctx())
        && let Some(mute_all) = take_mute_shortcut(ui.ctx())
    {
        state.knife_armed = false;
        if mute_all {
            let targets: Vec<ClipSel> = all_clips(midi_tracks, control_tracks, audio_tracks)
                .into_iter()
                .filter(|sel| clip_is_muted(sel, midi_tracks, control_tracks, audio_tracks))
                .collect();
            if targets.is_empty() {
                state.set_status("Alt+H: no muted clips");
            } else {
                let count = targets.len();
                actions.push(TimelineAction::SetClipMuted {
                    clips: targets,
                    muted: false,
                });
                state.set_status(format!("{count} clips unmuted"));
            }
        } else if state.selection.is_empty() {
            state.set_status("H mutes the selection \u{2014} nothing selected");
        } else {
            let targets: Vec<ClipSel> = state
                .selection
                .iter()
                .copied()
                .filter(|sel| !clip_is_muted(sel, midi_tracks, control_tracks, audio_tracks))
                .collect();
            if targets.is_empty() {
                state.set_status("H: the selection is already muted (Alt+H reveals all)");
            } else {
                let count = targets.len();
                actions.push(TimelineAction::SetClipMuted {
                    clips: targets,
                    muted: true,
                });
                state.set_status(format!("{count} clips muted"));
            }
        }
    }
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
    //
    // eframe 0.37 made `ScrollArea::State`'s offset private: it can be *read*
    // (`clamped_offset`) but only changed by handing the value to the
    // ScrollArea, so both gestures below produce an offset that is passed to
    // the builder instead of being written into the stored state.
    let scroll_id = ui.make_persistent_id("glitcherz_timeline_scroll");
    let mut scroll_target: Option<Vec2> = None;
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
            let offset = egui::containers::scroll_area::State::load(ui.ctx(), scroll_id)
                .map_or(Vec2::ZERO, |s| s.clamped_offset());
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
                    scroll_target = Some(Vec2::new(anchored, offset.y));
                }
            }
            // Row height: only a vertical-only factor (Ctrl+Alt) resizes
            // the rows — a uniform zoom belongs to the time axis above.
            if zoom2.y != 1.0 && (zoom2.x - zoom2.y).abs() > 1e-4 {
                state.row_zoom = (state.row_zoom * zoom2.y).clamp(ROW_ZOOM_MIN, ROW_ZOOM_MAX);
            }
            ui.ctx().request_repaint();
        }
        // Middle-drag pans the viewport (the ScrollArea's drag-to-scroll is
        // touch-only by default). Ctrl+middle is the clip-mute toggle (B2),
        // so the pan stays out of its way — otherwise the click that should
        // toggle would also nudge the view.
        if in_rows
            && !ctrl
            && ui
                .ctx()
                .input(|i| i.pointer.button_down(egui::PointerButton::Middle))
        {
            let d = ui.ctx().input(|i| i.pointer.delta());
            if d != Vec2::ZERO
                && let Some(s) = egui::containers::scroll_area::State::load(ui.ctx(), scroll_id)
            {
                let offset = s.clamped_offset();
                scroll_target = Some(Vec2::new(
                    (offset.x - d.x).max(0.0),
                    (offset.y - d.y).max(0.0),
                ));
            }
        }
    }

    // The scroll area carries an explicit id: all rows below live off its
    // content ui's positional auto ids, so if anything above it comes or
    // goes every row silently re-id's — and since egui keeps focus ON THE
    // ID, the focused lane turns into a phantom (no row reports focus
    // again, every focus-gated key dies, and the "no keyboard focus" chip
    // stays lit no matter how often you click).
    let mut view = egui::ScrollArea::both()
        .auto_shrink(false)
        .id_salt("glitcherz_timeline_scroll");
    if let Some(target) = scroll_target {
        // Only on the frames a gesture produced one: passing it every frame
        // would pin the view and the wheel would stop scrolling.
        view = view.scroll_offset(target);
    }
    view.show(ui, |ui| {
        // Toolbar: zoom + track add/remove + hints.
        let toolbar_top = ui.cursor().min.y;
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
            // I3: the one-shot tools, next to the track add/remove pair. The
            // timeline has no persistent mode (I-D5 — creating a clip here is
            // a double-click), so this row is all *actions*: they arm, never
            // latch, and an armed one is outlined. Arming from a button passes
            // a deferred grab point (see `arm_modal`), so the clips do not
            // jump when the pointer reaches the lanes.
            let armed = |state: &TimelineState| match state.modal.as_ref().map(|m| m.kind) {
                Some(ModalKind::Grab { .. }) => Some(Arm::Grab),
                Some(ModalKind::Stretch { .. }) => Some(Arm::Stretch),
                Some(ModalKind::Tile) => Some(Arm::Tile),
                _ => None,
            };
            for (label, shortcut, arm, hint) in [
                (
                    "Grab",
                    "g",
                    Arm::Grab,
                    "the clips follow the pointer · LMB drops · RMB / Esc cancels",
                ),
                (
                    "Stretch",
                    "t",
                    Arm::Stretch,
                    "pull the selection's end · s flips to the start · LMB drops",
                ),
                (
                    "Tile",
                    "Shift+T",
                    Arm::Tile,
                    "repeat the clip by dragging right · LMB drops · Esc cancels",
                ),
            ] {
                let is_armed = armed(state) == Some(arm);
                if crate::toolbar::action_button(ui, label, shortcut, is_armed, hint).clicked() {
                    if is_armed {
                        // A second click on an armed tool cancels it.
                        state.modal = None;
                    } else {
                        state.knife_armed = false;
                        arm_modal(state, arm, None);
                    }
                }
            }
            // The knife is not a modal: it arms a stroke, so it owns no grab
            // point and no ghost — only the `armed` flag.
            if crate::toolbar::action_button(
                ui,
                "Knife",
                "k",
                state.knife_armed,
                "one click cuts the clip under the pointer at the bar line · Esc cancels",
            )
            .clicked()
            {
                state.knife_armed = !state.knife_armed;
                state.modal = None;
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
                Some(Modal {
                    kind: ModalKind::Tile,
                    ..
                }) => {
                    ui.weak(
                        egui::RichText::new(
                            "Tile: drag right to repeat the whole clip · LMB drop · RMB / Esc cancel",
                        )
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
                        "Drag move · either edge: trim (head included) · T: extend time · Shift+T / Alt+T: tile · dbl-click edit · F2 / Alt+dbl-click: rename · Ctrl+G group · Ctrl+Shift+G ungroup · Alt+click: single · right-click delete · Ctrl+MMB mute clip · H mute / Alt+H unmute all · marquee select · Del · K knife · dots: reorder track · ruler: seek / Ctrl+loop / RMB marker · Ctrl+wheel zoom · Ctrl+Alt+wheel height · MMB pan",
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
        // Rows must butt against each other vertically: their clip lanes are
        // stacked with no gap, so the per-lane playhead segments join into
        // one continuous line through every track.
        ui.spacing_mut().item_spacing.y = 0.0;

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
                &|clip, painter, rect| draw_midi_preview(painter, rect, clip),
                p.clip_groups,
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
                    p.clip_groups,
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
                    p.clip_groups,
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
        // The toolbar row above the lanes belongs to the timeline's keyboard
        // surface too: a click on a tool button is a click *outside* the
        // focused lane for egui, which surrenders the lane's focus
        // (`SurrenderFocusOn::Clicks`) — and hover-follow only hands it back
        // once the pointer is inside a lane, so G / T / Shift+D / Delete /
        // Esc stayed dead with the pointer parked on the button just clicked.
        // The band is registered for whichever row holds the keyboard, and it
        // stops at the first lane so it can never steal another row's focus.
        if let Some((_, first)) = geom.first()
            && let Some(id) = state.focus_id
        {
            let band = Rect::from_min_max(
                pos2(first.left() - HEADER_WIDTH, toolbar_top),
                pos2(first.right(), first.top()),
            )
            .expand(ui.style().interaction.interact_radius);
            router.register_keyboard(id, band);
        }
        if rows_frame.is_some() {
            state.rows_viewport_min = ui.clip_rect().min;
        }

        // ---- Track reorder (header grab handle). The target is the
        // insertion row the pointer hovers among the dragged lane's rows; the
        // drop (primary release anywhere — the drag follows the pointer across
        // rows) lands as one action. The insert indicator is drawn over the
        // rows so the target is visible while dragging. ----
        if let Some(from) = state.reorder
            && let Some(frame) = rows_frame
        {
            let lane = from.lane();
            let lane_rows: Vec<Rect> = geom
                .iter()
                .filter_map(|(r, rect)| (r.lane() == lane).then_some(*rect))
                .collect();
            let ptr = ui.ctx().pointer_hover_pos();
            let mut drop: Option<usize> = None;
            if let Some(ptr) = ptr
                && let Some((to, y)) = reorder_target(ptr.y, &lane_rows)
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
                    && to != from.track()
                    && to != from.track() + 1
                {
                    actions.push(TimelineAction::ReorderTrack {
                        lane,
                        from: from.track(),
                        to,
                    });
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
            // Tile may have been armed without a pointer (holding Alt drops
            // the cursor on Windows); latch the grab on the first valid one
            // so the period starts moving from where the user actually is.
            if let (Some(m), Some(hover)) =
                (state.modal.as_mut(), ui.ctx().pointer_hover_pos())
                && !m.grab.x.is_finite()
            {
                m.grab = hover;
            }
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
                            ModalKind::Tile => {
                                if let Some(tiles) = tile_targets(
                                    m,
                                    ptr,
                                    px_per_bar,
                                    bar_ticks,
                                    midi_tracks,
                                    control_tracks,
                                    p.sec_per_tick,
                                ) && !tiles.is_empty()
                                {
                                    actions.push(TimelineAction::TileClipsBatch { tiles });
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
                                Some((mv.from.lane, mv.to_track, mv.start_ticks, span, None))
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
                            .map(|e| {
                                (e.sel.lane, e.sel.track, e.start_ticks, e.span_ticks, None)
                            })
                            .collect::<Vec<_>>()
                    }),
                    ModalKind::Tile => tile_targets(
                        m,
                        ptr,
                        px_per_bar,
                        bar_ticks,
                        midi_tracks,
                        control_tracks,
                        p.sec_per_tick,
                    )
                    .map(|tiles| {
                        tiles
                            .into_iter()
                            .filter_map(|t| {
                                let (start, _) = clip_geometry(
                                    midi_tracks,
                                    control_tracks,
                                    audio_tracks,
                                    p.sec_per_tick,
                                    t.sel,
                                )?;
                                Some((
                                    t.sel.lane,
                                    t.sel.track,
                                    start,
                                    t.span_ticks,
                                    Some(t.loop_ticks),
                                ))
                            })
                            .collect::<Vec<_>>()
                    }),
                };
                if let Some(targets) = targets {
                    for (lane_kind, track, start, span, period) in targets {
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
                        if let Some(period) = period {
                            if period < span {
                                let seam_color = ghost.gamma_multiply(0.8);
                                draw_tile_seams(&painter, rect, period, px_per_tick, seam_color);
                            }
                            // Always show the target repeat count: opening the modal must
                            // give immediate feedback even before the pointer
                            // moves. Sub-bar periods make a "N bar" label
                            // misleading, so only ×M shows.
                            let repeats = span.div_ceil(period.max(1));
                            painter.text(
                                rect.center(),
                                Align2::CENTER_CENTER,
                                format!("×{repeats}"),
                                FontId::proportional(11.0),
                                Color32::WHITE,
                            );
                        }
                    }
                }
            }
        }
        if let Some(msg) = refused_msg {
            state.set_status(msg);
        }
    });
    // W3b: expand the selection to every member of the groups it touches — a
    // click on one grouped clip therefore moves / copies / deletes the whole
    // group. `Alt`-picked selections opt out until the next plain pick, and
    // the expansion is idempotent, so running it every frame is harmless.
    if !state.break_group && !state.selection.is_empty() {
        let expanded =
            expand_group_selection(midi_tracks, control_tracks, audio_tracks, &state.selection);
        if expanded != state.selection {
            state.selection = expanded;
        }
    }
    actions
}

/// W3c: every clip in the arrangement, in table order (MIDI → control →
/// audio, by track then clip). Empty tracks contribute nothing, so the
/// result doubles as the "did anything get selected?" count.
pub(crate) fn all_clips(
    midi: &[MidiTrack],
    control: &[ControlTrack],
    audio: &[AudioTrack],
) -> Vec<ClipSel> {
    let mut out = Vec::new();
    for (track, data) in midi.iter().enumerate() {
        for clip in 0..data.clips.len() {
            out.push(ClipSel {
                lane: TimelineLane::Midi,
                track,
                clip,
            });
        }
    }
    for (track, data) in control.iter().enumerate() {
        for clip in 0..data.clips.len() {
            out.push(ClipSel {
                lane: TimelineLane::Control,
                track,
                clip,
            });
        }
    }
    for (track, data) in audio.iter().enumerate() {
        for clip in 0..data.clips.len() {
            out.push(ClipSel {
                lane: TimelineLane::Audio,
                track,
                clip,
            });
        }
    }
    out
}

/// W3b: every clip that shares a group with any selected clip, in table
/// order. Groups are cross-track and cross-lane by design, so the result can
/// mix MIDI / control / audio entries. Selections without grouped clips are
/// returned unchanged.
pub(crate) fn expand_group_selection(
    midi: &[MidiTrack],
    control: &[ControlTrack],
    audio: &[AudioTrack],
    selection: &[ClipSel],
) -> Vec<ClipSel> {
    let group_of = |sel: &ClipSel| -> Option<u32> {
        match sel.lane {
            TimelineLane::Midi => midi
                .get(sel.track)
                .and_then(|track| track.clips.get(sel.clip))
                .and_then(Placement::group),
            TimelineLane::Control => control
                .get(sel.track)
                .and_then(|track| track.clips.get(sel.clip))
                .and_then(Placement::group),
            TimelineLane::Audio => audio
                .get(sel.track)
                .and_then(|track| track.clips.get(sel.clip))
                .and_then(Placement::group),
        }
    };
    let mut groups: Vec<u32> = Vec::new();
    for sel in selection {
        if let Some(id) = group_of(sel)
            && !groups.contains(&id)
        {
            groups.push(id);
        }
    }
    if groups.is_empty() {
        return selection.to_vec();
    }
    let mut out = selection.to_vec();
    let mut add = |lane: TimelineLane, track: usize, clip: usize, group: Option<u32>| {
        if group.is_some_and(|id| groups.contains(&id)) {
            let sel = ClipSel { lane, track, clip };
            if !out.contains(&sel) {
                out.push(sel);
            }
        }
    };
    for (ti, track) in midi.iter().enumerate() {
        for (ci, clip) in track.clips.iter().enumerate() {
            add(TimelineLane::Midi, ti, ci, clip.group());
        }
    }
    for (ti, track) in control.iter().enumerate() {
        for (ci, clip) in track.clips.iter().enumerate() {
            add(TimelineLane::Control, ti, ci, clip.group());
        }
    }
    for (ti, track) in audio.iter().enumerate() {
        for (ci, clip) in track.clips.iter().enumerate() {
            add(TimelineLane::Audio, ti, ci, clip.group());
        }
    }
    out
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
    // W3b: group table for the colour stripe + name tag.
    clip_groups: &[ClipGroup],
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
            .scope_id(egui::Id::unique("glitcherz_timeline_row").with(row.key()))
            .max_rect(row_rect),
    );
    let ui = &mut row_ui;
    // Every header must occupy the same horizontal footprint as the ruler's
    // left spacer (HEADER_WIDTH plus one item gap), otherwise each lane —
    // and the playhead drawn from it — starts at a different x. Every row
    // carries a reorder grip, so the name cell is narrowed by that grip plus
    // the gap instead of letting the header grow past the column.
    let header_gap = ui.spacing().item_spacing.x;
    // Reorder grab handle: a drag on the dots moves the track. Sense::drag
    // only — a plain click on it does nothing (selecting stays with the name
    // button).
    let (grip_rect, grip_response) =
        ui.allocate_exact_size(Vec2::new(REORDER_GRIP_WIDTH, row_h - 4.0), Sense::drag());
    let grip_color = if state.reorder == Some(row) {
        SELECTED
    } else {
        Color32::from_gray(110)
    };
    let grip_painter = ui.painter_at(grip_rect);
    let mid = grip_rect.center().y;
    for dy in [-3.0, 0.0, 3.0] {
        for dx in [-3.0, 0.0] {
            grip_painter.circle_filled(pos2(grip_rect.center().x + dx, mid + dy), 1.2, grip_color);
        }
    }
    if grip_response.drag_started_by(egui::PointerButton::Primary) {
        state.reorder = Some(row);
    }
    if grip_response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }
    // Header cell.
    match header {
        HeaderKind::Midi {
            index,
            name,
            raw_name,
            selected,
        } => {
            track_header_cell(
                ui,
                state,
                actions,
                TimelineLane::Midi,
                index,
                &name,
                raw_name,
                HEADER_WIDTH - REORDER_GRIP_WIDTH - header_gap,
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
                HEADER_WIDTH - REORDER_GRIP_WIDTH - header_gap,
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
                HEADER_WIDTH - REORDER_GRIP_WIDTH - header_gap,
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
        // B2: a muted clip is drawn faded (a lane-coloured veil over the
        // whole block, see below). Its geometry, tiling and content preview
        // all stay in place, so "muted" never reads as "moved" or "deleted".
        let muted = clip.muted();
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
        // W2 / 2026-11-04: when the container is longer than one period, mark
        // every repeat seam and tint the repeated periods — the same tiling
        // the sequencer does (`start + k·loop`), so what is drawn is what
        // plays. Single-period clips stay a plain solid block.
        let span_ticks = clip.span(sec_per_tick).max(1);
        let loop_ticks = clip.loop_len(sec_per_tick).clamp(1, span_ticks);
        if loop_ticks < span_ticks {
            let seam_color = colors.clip_stroke.gamma_multiply(0.55);
            draw_tile_seams(&painter, rect, loop_ticks, px_per_tick, seam_color);
        }
        // U6: a clip with waveform data draws min/max peak columns across
        // its body (stretched to the span, exactly like playback reads it).
        if let Some(data) = waveform_of(clip) {
            draw_waveform(&painter, rect, &data, colors.clip_stroke);
        }
        // U12: per-clip preview drawn over the block (control curve).
        clip_body(clip, &painter, rect);
        // W3b: group stripe + name. Drawn after the content preview so the
        // tag is never hidden by a waveform or curve.
        if let Some(group_id) = clip.group()
            && let Some(group) = clip_groups.iter().find(|group| group.id == group_id)
        {
            let color = Color32::from_rgb(group.color[0], group.color[1], group.color[2]);
            let stripe = Rect::from_min_max(
                pos2(rect.left() + 1.0, rect.top() + 2.0),
                pos2(rect.left() + 4.0, rect.bottom() - 2.0),
            );
            painter.rect_filled(stripe, 1.0, color);
            if rect.width() > 110.0 && !group.name.is_empty() {
                painter.text(
                    pos2(rect.right() - 4.0, rect.bottom() - 2.0),
                    Align2::RIGHT_BOTTOM,
                    &group.name,
                    FontId::proportional(9.0),
                    color,
                );
            }
        }
        // B2: the mute veil. Drawn over everything the block has produced
        // (seams, waveform, curve preview, group stripe) so the clip reads
        // as "parked" at a glance without any of its content disappearing;
        // the selection / edit strokes come after and stay full strength.
        if muted {
            painter.rect_filled(
                rect,
                3.0,
                Color32::from_rgba_unmultiplied(
                    colors.lane_bg.r(),
                    colors.lane_bg.g(),
                    colors.lane_bg.b(),
                    165,
                ),
            );
        }
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
        // W3a: an inline rename field replaces the label while it is open.
        // It is a real widget on top of the painted block, so typing and
        // focus behave like the track-header editor.
        let sel = ClipSel {
            lane: lane_kind,
            track,
            clip: index,
        };
        if state
            .clip_rename
            .as_ref()
            .is_some_and(|(clip_sel, _)| *clip_sel == sel)
        {
            let mut cancel = false;
            let mut commit = false;
            if let Some((_, draft)) = state.clip_rename.as_mut() {
                let width = rect
                    .width()
                    .max(72.0)
                    .min((lane.right() - rect.left() - 4.0).max(24.0));
                let inner = Rect::from_min_size(
                    rect.min + Vec2::new(2.0, 1.0),
                    Vec2::new(width, (rect.height() - 2.0).max(14.0)),
                );
                let resp = ui.put(
                    inner,
                    egui::TextEdit::singleline(draft)
                        .font(egui::TextStyle::Small)
                        .hint_text("Clip name"),
                );
                if std::mem::take(&mut state.clip_rename_focus) {
                    resp.request_focus();
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                } else if resp.lost_focus() {
                    actions.push(TimelineAction::RenameClip {
                        lane: lane_kind,
                        track,
                        clip: index,
                        name: draft.trim().to_owned(),
                    });
                    commit = true;
                }
            }
            if cancel || commit {
                state.clip_rename = None;
            }
            continue;
        }
        // Empty name = unnamed: show the content summary instead (W3a).
        let custom_name = clip.name();
        let mut label = if custom_name.is_empty() {
            clip_label(clip)
        } else {
            custom_name.to_owned()
        };
        // W2: `×N` tells how many content periods the container covers.
        let repeats = span_ticks.div_ceil(loop_ticks);
        if repeats > 1 {
            label = format!("{label} ×{repeats}");
        }
        let text_color = if muted {
            colors.clip_text.gamma_multiply(0.5)
        } else {
            colors.clip_text
        };
        painter.text(
            pos2(rect.left() + 4.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(10.0),
            text_color,
        );
        // B2: a small `M` badge in the top-right corner — fading alone is
        // easy to mistake for a dim theme, and narrow clips lose the label.
        if muted && rect.width() > 18.0 {
            painter.text(
                pos2(rect.right() - 4.0, rect.top() + 1.0),
                Align2::RIGHT_TOP,
                "M",
                FontId::proportional(9.0),
                Color32::from_rgb(210, 160, 90),
            );
        }
    }

    // Drop a rename editor whose clip disappeared (deleted, or the track's
    // clip table was rebuilt).
    if state
        .clip_rename
        .as_ref()
        .is_some_and(|(s, _)| s.lane == lane_kind && s.track == track && s.clip >= clips.len())
    {
        state.clip_rename = None;
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
                let tiled = hit_clip(clips, tick_at(pos.x), sec_per_tick).is_some_and(|index| {
                    clips[index].loop_len(sec_per_tick) < clips[index].span(sec_per_tick)
                });
                match knife_target(clips, tick_at(pos.x), bar_ticks, sec_per_tick) {
                    // W2: a cut would need a phase-relative loop origin,
                    // which the model does not carry.
                    Some(_) if tiled => {
                        state.set_status(
                            "a tiled clip must be trimmed to one period before cutting",
                        );
                    }
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

    // I6: show the resize cursor over either edge so the head handle is
    // discoverable (before I6 the left edge read as "move").
    if state.drag.is_none()
        && state.modal.is_none()
        && !state.knife_armed
        && response.hovered()
        && let Some(pos) = response.hover_pos()
        && let Some(index) = hit_clip(clips, tick_at(pos.x), sec_per_tick)
    {
        let start_x = lane.left() + clips[index].start() as f32 * px_per_tick;
        let end_x = lane.left()
            + (clips[index].start() + clips[index].span(sec_per_tick)) as f32 * px_per_tick;
        let wide_enough = end_x - start_x >= 3.0 * EDGE_GRAB;
        if (wide_enough && (pos.x - start_x).abs() <= EDGE_GRAB)
            || (pos.x - end_x).abs() <= EDGE_GRAB
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
    }

    // Double-click: open the clip's editor, or insert a new clip on empty
    // space. Not while a modal owns the pointer.

    if response.double_clicked() && state.modal.is_none() {
        if let Some(pos) = response.interact_pointer_pos() {
            let tick = tick_at(pos.x);
            if let Some(index) = hit_clip(clips, tick, sec_per_tick) {
                // W3a: Alt+double-click renames instead of opening the
                // editor (same modifier family as the rest of the view).
                if ui.input(|i| i.modifiers.alt) {
                    state.clip_rename = Some((
                        ClipSel {
                            lane: lane_kind,
                            track,
                            clip: index,
                        },
                        clips[index].name().to_owned(),
                    ));
                    state.clip_rename_focus = true;
                } else {
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
                    // 2026-11-04: the inserted clip is selected right away so
                    // G / T / Shift+D work on it without an extra pick. The
                    // op pushes the clip, so its index is the current length.
                    let clip = clips.len();
                    actions.push(TimelineAction::AddClip {
                        lane,
                        track,
                        start_ticks: start,
                    });
                    state.selection = vec![ClipSel { lane, track, clip }];
                    // The second press may have started a rubber band; the
                    // inserted clip's selection must survive the release.
                    state.marquee = None;
                    state.marquee_ending = false;
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
                // W3b: `Alt` picks this one clip out of its group (the
                // expansion at the end of show() is skipped while it holds).
                let alt = ui.ctx().input(|i| i.modifiers.alt);
                state.break_group = alt;
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
                let origin_start = clips[index].start();
                let origin_span = clips[index].span(sec_per_tick);
                let origin_loop = clips[index].loop_len(sec_per_tick);
                let start_x = lane.left() + origin_start as f32 * px_per_tick;
                let end_x = lane.left() + (origin_start + origin_span) as f32 * px_per_tick;
                // I6: both edges resize. The left edge is only a handle when
                // the clip is wide enough to keep a body to grab for a move
                // (a narrow clip would otherwise be impossible to move).
                let wide_enough = end_x - start_x >= 3.0 * EDGE_GRAB;
                let edge = if wide_enough && (pos.x - start_x).abs() <= EDGE_GRAB {
                    Some(ResizeEdge::Start)
                } else if (pos.x - end_x).abs() <= EDGE_GRAB {
                    Some(ResizeEdge::End)
                } else {
                    None
                };
                if let Some(edge) = edge {
                    if origin_loop < origin_span {
                        state.set_status(
                            "tiled clip: the head trim shortens the container, the repeats keep their phase",
                        );
                    }
                    state.drag = Some(Drag::Resize {
                        row,
                        clip: index,
                        edge,
                        origin_start,
                        origin_span,
                        origin_loop,
                    });
                } else {
                    let grab = tick - origin_start as f32;
                    state.drag = Some(Drag::Move {
                        row,
                        clip: index,
                        grab_ticks: grab,
                    });
                }
            } else {
                let additive = ui.ctx().input(|i| i.modifiers.shift || i.modifiers.ctrl);
                // A fresh band starts from group-expanded semantics.
                state.break_group = false;
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
                                loop_ticks: None,
                            });
                        }
                    }
                    Some(Drag::Resize {
                        clip,
                        edge,
                        origin_start,
                        origin_span,
                        origin_loop,
                        ..
                    }) if clip < clips.len() => match edge {
                        ResizeEdge::Start => {
                            // I6: the left edge trims the head — the start
                            // moves right, the span shrinks, and the content
                            // is re-based from the snapshot. Bar-snapped and
                            // never closer than one bar to the tail.
                            let new_start = trim_head_start(
                                tick_at(pos.x),
                                origin_start,
                                origin_span,
                                bar_ticks,
                            );
                            let delta = new_start - origin_start;
                            if delta > 0 {
                                actions.push(TimelineAction::TrimClipHead {
                                    lane: row.lane(),
                                    track: row.track(),
                                    clip,
                                    start_ticks: new_start,
                                    span_ticks: origin_span - delta,
                                    delta_ticks: delta,
                                });
                            }
                        }
                        ResizeEdge::End => {
                            let start = clips[clip].start();
                            let limit = ARRANGEMENT_LIMIT_TICKS - start;
                            let edge = snap_bar(tick_at(pos.x) - start as f32, bar_ticks)
                                .clamp(bar_ticks, limit.max(bar_ticks));
                            // 2026-11-04: the plain right edge extends/trims
                            // the clip time. A single-period clip grows its
                            // content with the container; a tiled clip keeps
                            // its repeat count and scales its period. The
                            // origin snapshot avoids drift across the
                            // per-frame updates.
                            let span = edge;
                            let loop_ticks =
                                Some(resolve_stretch_loop(origin_loop, origin_span, span));
                            let loop_changed =
                                loop_ticks.is_some_and(|l| l != clips[clip].loop_len(sec_per_tick));
                            if clips[clip].span(sec_per_tick) != span || loop_changed {
                                let lane = row.lane();
                                let track = row.track();
                                actions.push(TimelineAction::MoveResizeClip {
                                    lane,
                                    track,
                                    clip,
                                    start_ticks: start,
                                    span_ticks: span,
                                    loop_ticks,
                                });
                            }
                        }
                    },
                    _ => {}
                }
            }
        }
    }

    if response.drag_stopped_by(egui::PointerButton::Primary) {
        state.drag = None;
        if state.head_trim.take().is_some() {
            actions.push(TimelineAction::EndClipTrim);
        }
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
    // selection alone. The SECOND click of a double-click is skipped: it
    // would clear the selection the double-click handler just set on an
    // inserted clip (2026-11-04).
    if response.clicked()
        && !response.double_clicked()
        && !state.knife_armed
        && state.modal.is_none()
    {
        if let Some(pos) = response.interact_pointer_pos() {
            let additive = ui.ctx().input(|i| i.modifiers.shift || i.modifiers.ctrl);
            // W3b: Alt keeps the selection on the clicked clip only.
            state.break_group = ui.ctx().input(|i| i.modifiers.alt);
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

    // B2: Ctrl + middle click toggles the mute flag of the clip under the
    // pointer (the single-clip counterpart of H, which acts on the
    // selection). Middle is otherwise the pan button, so `show()` skips
    // panning exactly while Ctrl is held; a plain middle click still pans and
    // never toggles.
    if response.clicked_by(egui::PointerButton::Middle)
        && state.modal.is_none()
        && ui.ctx().input(|i| i.modifiers.ctrl || i.modifiers.command)
        && let Some(pos) = response.interact_pointer_pos()
        && let Some(index) = hit_clip(clips, tick_at(pos.x), sec_per_tick)
    {
        let muted = !clips[index].muted();
        actions.push(TimelineAction::SetClipMuted {
            clips: vec![ClipSel {
                lane: lane_kind,
                track,
                clip: index,
            }],
            muted,
        });
        state.set_status(if muted { "clip muted" } else { "clip unmuted" });
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
                    if state.head_trim.take().is_some() {
                        actions.push(TimelineAction::EndClipTrim);
                    }
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
        // Alt+T must be distinguished from plain T. Scan the raw key event
        // and also accept a plain T while Alt is held: winit can deliver the
        // modifier state one frame late, and `consume_key` only matches the
        // event's own modifier snapshot.
        let (alt_t, t) = ui.ctx().input_mut(|i| {
            let alt_held = i.modifiers.alt;
            let mut alt_t = false;
            let mut plain_t = false;
            i.events.retain(|event| {
                if let egui::Event::Key {
                    key,
                    physical_key,
                    pressed: true,
                    modifiers,
                    ..
                } = event
                    && (*key == Key::T || physical_key.as_ref() == Some(&Key::T))
                {
                    if modifiers.alt || alt_held {
                        alt_t = true;
                    } else {
                        plain_t = true;
                    }
                    return false;
                }
                true
            });
            (alt_t, plain_t)
        });
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
        // W3a: F2 renames the first selected clip on this row (Windows /
        // explorer convention; Alt+double-click is the pointer equivalent).
        let f2 = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::F2));
        if f2 {
            if let Some(target) = clip_rename_target(&state.selection, lane_kind, track, clips) {
                state.clip_rename = Some(target);
                state.clip_rename_focus = true;
                state.modal = None;
            }
        }
        // W3b: Ctrl+G groups the selection (cross-track / cross-lane),
        // Ctrl+Shift+G dissolves every group it touches.
        let shift_g = {
            let mut mods = Modifiers::CTRL;
            mods.shift = true;
            ui.ctx().input_mut(|i| i.consume_key(mods, Key::G))
        };
        let ctrl_g = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::CTRL, Key::G));
        if !state.selection.is_empty() {
            if shift_g {
                actions.push(TimelineAction::UngroupClips {
                    clips: state.selection.clone(),
                });
            } else if ctrl_g {
                actions.push(TimelineAction::GroupClips {
                    clips: state.selection.clone(),
                });
            }
        }
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
                    ModalKind::Stretch { .. } | ModalKind::Tile => {}
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
        // Arm G / Shift+D / T / Alt+T (Blender-style modal; arming one
        // tool clears the other).
        if (g || shift_d || t || alt_t) && state.modal.is_none() {
            state.knife_armed = false;
            let arm = if alt_t {
                Arm::Tile
            } else if g {
                Arm::Grab
            } else if shift_d {
                Arm::Duplicate
            } else {
                Arm::Stretch
            };
            // Holding Alt can drop the egui cursor on Windows (CursorLeft →
            // PointerGone), so a missing pointer is VISIBLE rather than a
            // silent no-op — it used to fail silently here. Tile is the one
            // arm that proceeds without it: it latches its grab on the first
            // valid pointer instead (the toolbar arms the same way, I3).
            let ptr = ui.ctx().pointer_hover_pos();
            if ptr.is_none() && !matches!(arm, Arm::Tile) {
                state.set_status("move the pointer over the timeline first");
            } else {
                arm_modal(state, arm, ptr);
            }
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

/// Consumes a tile-shortcut press (Shift+T, with Alt+T as a fallback) and
/// reports whether it was one. Accepts the event's modifier snapshot and the
/// current held state (winit can deliver the modifier state one frame late,
/// and `consume_key` only checks the event's own snapshot), plus the
/// physical key for non-Latin layouts. Shift is the PRIMARY qualifier: on
/// Windows Alt+letter is `WM_SYSKEYDOWN`, and releasing Alt raises
/// `SC_KEYMENU`, whose menu mode swallows the following mouse events until
/// Esc — the reported "hint shows Tile but nothing responds" symptom (D29).
fn take_tile_shortcut(ctx: &egui::Context) -> bool {
    ctx.input_mut(|i| {
        let shift_held = i.modifiers.shift;
        let alt_held = i.modifiers.alt;
        let mut found = false;
        i.events.retain(|event| {
            if let egui::Event::Key {
                key,
                physical_key,
                pressed: true,
                modifiers,
                ..
            } = event
                && (*key == Key::T || physical_key.as_ref() == Some(&Key::T))
                && !modifiers.ctrl
                && !modifiers.command
                && (modifiers.shift || shift_held || modifiers.alt || alt_held)
            {
                found = true;
                return false;
            }
            true
        });
        found
    })
}

/// Consumes the clip-mute shortcut (B2) and classifies it.
///
/// `Some(true)` = Alt+H (reveal / unmute everything), `Some(false)` = plain
/// H (mute the selection), `None` = not pressed. Like the tile shortcut it
/// reads the event's modifier snapshot AND the currently held state: winit
/// can deliver modifiers one frame late, and `consume_key` would only see
/// the snapshot. Ctrl+H is deliberately left alone (not ours to eat).
fn take_mute_shortcut(ctx: &egui::Context) -> Option<bool> {
    ctx.input_mut(|i| {
        let alt_held = i.modifiers.alt;
        let ctrl_held = i.modifiers.ctrl || i.modifiers.command;
        let mut plain = false;
        let mut with_alt = false;
        i.events.retain(|event| {
            let egui::Event::Key {
                key,
                physical_key,
                pressed: true,
                modifiers,
                ..
            } = event
            else {
                return true;
            };
            if *key != Key::H && physical_key.as_ref() != Some(&Key::H) {
                return true;
            }
            if modifiers.alt || alt_held {
                with_alt = true;
            } else if modifiers.ctrl || modifiers.command || ctrl_held {
                return true;
            } else {
                plain = true;
            }
            false
        });
        if with_alt {
            Some(true)
        } else if plain {
            Some(false)
        } else {
            None
        }
    })
}

/// B2: one clip's mute flag, resolved across the three track tables (the
/// reveal-all / mute-selection shortcuts only act on clips whose state would
/// actually change).
fn clip_is_muted(
    sel: &ClipSel,
    midi: &[MidiTrack],
    control: &[ControlTrack],
    audio: &[AudioTrack],
) -> bool {
    match sel.lane {
        TimelineLane::Midi => midi
            .get(sel.track)
            .and_then(|track| track.clips.get(sel.clip))
            .is_some_and(Placement::muted),
        TimelineLane::Control => control
            .get(sel.track)
            .and_then(|track| track.clips.get(sel.clip))
            .is_some_and(Placement::muted),
        TimelineLane::Audio => audio
            .get(sel.track)
            .and_then(|track| track.clips.get(sel.clip))
            .is_some_and(Placement::muted),
    }
}

/// Which modal G / Shift+D / T / Alt+T arms (M4b.3/4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    Grab,
    Duplicate,
    Stretch,
    Tile,
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
///
/// `grab` is the pointer the transform measures from. `None` means **defer**:
/// the modal latches its grab on the first frame the pointer is over a lane.
/// That is what the toolbar (I3) passes — a click there lands on a button, and
/// measuring from the button's position would shove the selection sideways as
/// soon as the pointer reaches the lanes.
fn arm_modal(state: &mut TimelineState, arm: Arm, grab: Option<Pos2>) {
    if state.selection.is_empty() {
        state.set_status("nothing selected \u{2014} G / Shift+D / T need a selection");
        return;
    }
    // U11: the modal family is three-table now — audio clips arm G / T /
    // Shift+D just like MIDI/control (the geometry layer derives audio spans
    // from the seconds window at the base tempo).
    let kind = match arm {
        Arm::Grab => ModalKind::Grab { locked: None },
        Arm::Duplicate => ModalKind::Duplicate { locked: None },
        Arm::Stretch => ModalKind::Stretch { start_edge: false },
        Arm::Tile => {
            if state
                .selection
                .iter()
                .any(|s| s.lane == TimelineLane::Audio)
            {
                state.set_status("audio clips have no content period \u{2014} cannot tile");
                return;
            }
            ModalKind::Tile
        }
    };
    state.modal = Some(Modal {
        kind,
        origin: state.selection.clone(),
        // A deferred grab is the sentinel the latch looks for (see the modal
        // block in `show`): the first real hover replaces it.
        grab: grab.unwrap_or(pos2(f32::INFINITY, f32::INFINITY)),
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

/// I6: the bar-snapped start of a head trim.
///
/// Clamped to `[origin_start, origin_start + origin_span - 1 bar]`: the
/// drag never extends the clip leftwards (MIDI/control carry no content
/// offset to fill the gap) and never closes it past one bar.
pub(crate) fn trim_head_start(
    pointer_tick: f32,
    origin_start: u32,
    origin_span: u32,
    bar_ticks: u32,
) -> u32 {
    let bar = bar_ticks.max(1);
    let max_start = origin_start
        .saturating_add(origin_span)
        .saturating_sub(bar)
        .max(origin_start);
    snap_bar(pointer_tick, bar).clamp(origin_start, max_start)
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

/// Marks repeat seams inside a clip block: the first period stays plain, each
/// following period gets a faint tint and a dashed seam. Matches the
/// sequencer's `start + k·loop` tiling, so a single-period clip looks clearly
/// different from a tiled one.
fn draw_tile_seams(
    painter: &egui::Painter,
    rect: Rect,
    loop_ticks: u32,
    px_per_tick: f32,
    seam_color: Color32,
) {
    if loop_ticks == 0 || px_per_tick <= 0.0 {
        return;
    }
    let loop_px = loop_ticks as f32 * px_per_tick;
    if loop_px < 1.0 {
        return;
    }
    let mut seam = rect.left() + loop_px;
    let mut guard = 0;
    while seam < rect.right() - 0.5 && guard < 1024 {
        let band = Rect::from_min_max(
            pos2(seam, rect.top()),
            pos2((seam + loop_px).min(rect.right()), rect.bottom()),
        );
        painter.rect_filled(
            band,
            0.0,
            Color32::from_rgba_unmultiplied(255, 255, 255, 14),
        );
        let mut y = rect.top() + 1.0;
        while y < rect.bottom() - 1.0 {
            let y1 = (y + 2.5).min(rect.bottom() - 1.0);
            painter.line_segment(
                [pos2(seam, y), pos2(seam, y1)],
                Stroke::new(1.0, seam_color),
            );
            y += 5.0;
        }
        seam += loop_px;
        guard += 1;
    }
}

/// Draws a read-only note preview inside one MIDI clip block (2026-11-04,
/// same role as [`draw_control_preview`] for curve clips).
///
/// Notes are laid out at their tick positions (x) and pitch (y, adaptive
/// range) and clipped to the block. The content tiles by `loop_ticks` exactly
/// like playback. There is no editing here: notes are edited in the piano
/// roll.
fn draw_midi_preview(painter: &egui::Painter, rect: Rect, clip: &MidiClip) {
    if clip.sequence.notes.is_empty()
        || clip.span_ticks == 0
        || rect.width() < 6.0
        || rect.height() < 6.0
    {
        return;
    }
    let notes = &clip.sequence.notes;
    let mut lo = u8::MAX;
    let mut hi = 0u8;
    for n in notes {
        lo = lo.min(n.note);
        hi = hi.max(n.note);
    }
    let lo = i32::from(lo).saturating_sub(2).max(0);
    let hi = (i32::from(hi) + 2).min(127);
    let pitch_span = (hi - lo).max(11) as f32;
    let usable = (rect.height() - 2.0).max(1.0);
    let row_h = (usable / (pitch_span + 1.0)).max(1.0);
    let note_h = (row_h - 0.5).max(1.0);
    let span = clip.span_ticks.max(1);
    let period_ticks = clip.loop_ticks.max(1).min(span);
    let px_per_tick = rect.width() / span as f32;
    if px_per_tick <= 0.0 {
        return;
    }
    let color = Color32::from_rgba_unmultiplied(215, 232, 255, 220);
    let mut period = 0u32;
    let mut guard = 0;
    while period < span && guard < 1024 {
        let period_end = period.saturating_add(period_ticks).min(span);
        for n in notes {
            let start = i64::from(period) + n.start;
            if start >= i64::from(period_end) {
                continue;
            }
            let end = (start + n.length.max(1)).min(i64::from(period_end));
            let x0 = (rect.left() + start as f32 * px_per_tick).max(rect.left());
            let x1 = (rect.left() + end as f32 * px_per_tick)
                .max(x0 + 1.0)
                .min(rect.right());
            if x1 <= rect.left() || x0 >= rect.right() {
                continue;
            }
            let pitch = i32::from(n.note).clamp(lo, hi);
            let y = rect.top() + 1.0 + (hi - pitch) as f32 * row_h;
            let y1 = (y + note_h).min(rect.bottom() - 1.0);
            painter.rect_filled(Rect::from_min_max(pos2(x0, y), pos2(x1, y1)), 0.5, color);
        }
        period = period.saturating_add(period_ticks);
        guard += 1;
    }
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

    /// I6: the head-trim start is bar-snapped, never moves left of the drag
    /// origin (no leftwards extension without a content offset), and always
    /// leaves at least one bar of clip.
    #[test]
    fn head_trim_start_is_snapped_and_clamped() {
        let bar = TICKS_PER_BAR;
        let start = 2 * bar;
        let span = 4 * bar;
        // Pointer left of the origin: refused.
        assert_eq!(trim_head_start(0.0, start, span, bar), start);
        // Inside the clip: follows the bar grid.
        assert_eq!(
            trim_head_start((3 * bar) as f32 + 20.0, start, span, bar),
            3 * bar
        );
        // Past the tail: the tail keeps one bar.
        assert_eq!(
            trim_head_start((99 * bar) as f32, start, span, bar),
            start + span - bar
        );
    }

    /// I6: a single-period clip's head cut removes the material before the
    /// new start, and a note crossing the cut keeps its tail (this is the
    /// `retain_from` contract the trim relies on).
    #[test]
    fn head_trim_cuts_midi_content_without_losing_the_crossing_tail() {
        let mut clip = MidiClip {
            start_ticks: 0,
            span_ticks: 2 * TICKS_PER_BAR,
            loop_ticks: 2 * TICKS_PER_BAR,
            ..MidiClip::default()
        };
        clip.sequence.notes = vec![
            Note {
                start: 0,
                length: 480,
                note: 60,
                velocity: 100,
                channel: 0,
            },
            Note {
                start: 960,
                length: 960,
                note: 64,
                velocity: 100,
                channel: 0,
            },
        ];
        let delta = 1200;
        clip.retain_from(delta, SPT);
        assert_eq!(clip.sequence.notes.len(), 1);
        assert_eq!(clip.sequence.notes[0].note, 64);
        assert_eq!(clip.sequence.notes[0].start, 0);
        assert_eq!(clip.sequence.notes[0].end(), 1920 - i64::from(delta));
    }

    /// W3a: clip names are optional and the `Placement` helpers are the
    /// single read/write path the timeline uses for all three clip kinds.
    #[test]
    fn clip_names_are_optional_and_writable() {
        let mut midi = MidiClip::default();
        assert!(Placement::name(&midi).is_empty());
        Placement::set_name(&mut midi, "Verse".to_owned());
        assert_eq!(Placement::name(&midi), "Verse");

        let mut control = ControlClip::default();
        Placement::set_name(&mut control, "Sweep".to_owned());
        assert_eq!(Placement::name(&control), "Sweep");

        let mut audio = AudioClip::default();
        Placement::set_name(&mut audio, "Vox".to_owned());
        assert_eq!(Placement::name(&audio), "Vox");
    }

    /// W3b: one grouped clip expands the selection to every member — across
    /// tracks and lane kinds; ungrouped clips stay untouched.
    #[test]
    fn group_selection_expands_across_tracks_and_lanes() {
        let mut midi = vec![MidiTrack::default(), MidiTrack::default()];
        midi[0].clips.push(MidiClip {
            group: Some(1),
            ..MidiClip::default()
        });
        midi[1].clips.push(MidiClip {
            group: Some(1),
            ..MidiClip::default()
        });
        midi[1].clips.push(MidiClip::default());
        let mut control = vec![ControlTrack::default()];
        control[0].clips.push(ControlClip {
            group: Some(1),
            ..ControlClip::default()
        });
        let mut audio = vec![AudioTrack::default()];
        audio[0].clips.push(AudioClip {
            group: Some(2),
            ..AudioClip::default()
        });

        let start = vec![ClipSel {
            lane: TimelineLane::Midi,
            track: 0,
            clip: 0,
        }];
        let expanded = expand_group_selection(&midi, &control, &audio, &start);
        assert_eq!(expanded.len(), 3, "two MIDI + one control member");
        assert!(expanded.contains(&ClipSel {
            lane: TimelineLane::Control,
            track: 0,
            clip: 0,
        }));
        assert!(
            !expanded.iter().any(|sel| sel.lane == TimelineLane::Audio),
            "another group's clips must stay out"
        );

        // An ungrouped selection is returned unchanged.
        let lone = vec![ClipSel {
            lane: TimelineLane::Midi,
            track: 1,
            clip: 1,
        }];
        assert_eq!(expand_group_selection(&midi, &control, &audio, &lone), lone);
    }

    /// W3c: Ctrl+A takes every clip of every lane, in table order; empty
    /// tracks and empty tables contribute nothing.
    #[test]
    fn all_clips_covers_every_lane_in_table_order() {
        let mut midi = vec![MidiTrack::default(), MidiTrack::default()];
        midi[0].clips.push(MidiClip::default());
        midi[0].clips.push(MidiClip::default());
        midi[1].clips.push(MidiClip::default());
        let mut control = vec![ControlTrack::default()];
        control[0].clips.push(ControlClip::default());
        let mut audio = vec![AudioTrack::default()];
        audio[0].clips.push(AudioClip::default());

        let all = all_clips(&midi, &control, &audio);
        assert_eq!(all.len(), 5, "three MIDI + one control + one audio");
        assert_eq!(
            all[0],
            ClipSel {
                lane: TimelineLane::Midi,
                track: 0,
                clip: 0
            }
        );
        assert_eq!(
            all[2],
            ClipSel {
                lane: TimelineLane::Midi,
                track: 1,
                clip: 0
            }
        );
        assert_eq!(
            all[3],
            ClipSel {
                lane: TimelineLane::Control,
                track: 0,
                clip: 0
            }
        );
        assert_eq!(
            all[4],
            ClipSel {
                lane: TimelineLane::Audio,
                track: 0,
                clip: 0
            }
        );
        // Nothing loaded: nothing to select (the caller reports it).
        assert!(all_clips(&[], &[], &[]).is_empty());
        assert!(all_clips(&[MidiTrack::default()], &[], &[]).is_empty());
    }

    /// 2026-11-04: a mixed-kind selection clamps the shared row delta
    /// against EACH kind's own row count (intersection of all limits).
    #[test]
    fn clamp_row_delta_respects_each_kind() {
        let midi_top = ClipSel {
            lane: TimelineLane::Midi,
            track: 0,
            clip: 0,
        };
        let ctrl_top = ClipSel {
            lane: TimelineLane::Control,
            track: 0,
            clip: 0,
        };
        // Two MIDI rows, one control row, both groups at track 0: the
        // control group (single row) forbids moving in either direction.
        let mixed = vec![
            ClipSel {
                lane: TimelineLane::Midi,
                track: 0,
                clip: 0,
            },
            ClipSel {
                lane: TimelineLane::Control,
                track: 0,
                clip: 0,
            },
        ];
        assert_eq!(clamp_row_delta(1, &mixed, 2, 1, 0), 0);
        assert_eq!(clamp_row_delta(-1, &mixed, 2, 1, 0), 0);
        // MIDI-only selection: one row up is free, one row down is free.
        assert_eq!(clamp_row_delta(1, &[midi_top], 2, 1, 0), 1);
        assert_eq!(clamp_row_delta(-1, &[midi_top], 2, 1, 0), 0);
        // Control-only selection stays within its single row.
        assert_eq!(clamp_row_delta(1, &[ctrl_top], 2, 1, 0), 0);
    }

    /// W2: growing a clip keeps its content period (it tiles), shrinking trims
    /// the tail; an explicit Alt request wins but must fit the container.
    #[test]
    fn resize_loop_tiles_when_growing_and_trims_when_shrinking() {
        let bar = TICKS_PER_BAR;
        assert_eq!(resolve_loop(bar, 4 * bar, None), bar);
        assert_eq!(resolve_loop(2 * bar, bar, None), bar);
        assert_eq!(resolve_loop(bar, 4 * bar, Some(2 * bar)), 2 * bar);
        assert_eq!(resolve_loop(bar, bar, Some(8 * bar)), bar);
        // Never zero: the loop always covers at least one tick.
        assert_eq!(resolve_loop(bar, 1, None), 1);
    }

    /// 2026-11-04: T extends a single-period clip's editable time; a tiled
    /// clip keeps its repeat count and scales its period with the container.
    #[test]
    fn stretch_loop_extends_time_and_scales_tiled_periods() {
        let bar = TICKS_PER_BAR;
        // Single period: 1 bar -> 4 bars, still one period.
        assert_eq!(resolve_stretch_loop(bar, bar, 4 * bar), 4 * bar);
        // 4 bars in 2 periods: grow to 8 bars -> 4-bar period (2 repeats).
        assert_eq!(resolve_stretch_loop(2 * bar, 4 * bar, 8 * bar), 4 * bar);
        // Shrinking likewise keeps the repeat count.
        assert_eq!(resolve_stretch_loop(2 * bar, 4 * bar, 2 * bar), bar);
        // Never zero / never beyond the container.
        assert_eq!(resolve_stretch_loop(bar, bar, 1), 1);
        assert_eq!(resolve_stretch_loop(8 * bar, 8 * bar, 2), 2);
    }

    /// 2026-11-04: the tile modal adds/removes whole repeats, one per
    /// period-width of travel. A single-period clip tiles as a whole; an
    /// already-tiled clip keeps its period and just adjusts the count.
    #[test]
    fn tile_targets_adjusts_repeats() {
        let mut clip = MidiClip::new(0);
        clip.span_ticks = 4 * TICKS_PER_BAR;
        clip.loop_ticks = 4 * TICKS_PER_BAR;
        let midi = vec![MidiTrack {
            clips: vec![clip],
            ..MidiTrack::default()
        }];
        let modal = Modal {
            kind: ModalKind::Tile,
            origin: vec![ClipSel {
                lane: TimelineLane::Midi,
                track: 0,
                clip: 0,
            }],
            grab: pos2(100.0, 100.0),
        };
        let target = |dx_bars: f32, tracks: &[MidiTrack]| {
            tile_targets(
                &modal,
                Some(pos2(100.0 + dx_bars * 56.0, 100.0)),
                56.0,
                TICKS_PER_BAR,
                tracks,
                &[],
                SPT,
            )
            .unwrap()
        };
        // No movement: the ghost is the clip itself (×1).
        let tiles = target(0.0, &midi);
        assert_eq!(tiles[0].span_ticks, 4 * TICKS_PER_BAR);
        assert_eq!(tiles[0].loop_ticks, 4 * TICKS_PER_BAR);
        // Partial travel does not count until a full clip-width.
        let tiles = target(2.0, &midi);
        assert_eq!(tiles[0].span_ticks, 4 * TICKS_PER_BAR);
        // One clip-width right: the whole clip repeats once (×2).
        let tiles = target(4.0, &midi);
        assert_eq!(tiles[0].span_ticks, 8 * TICKS_PER_BAR);
        assert_eq!(tiles[0].loop_ticks, 4 * TICKS_PER_BAR);
        // Two clip-widths: ×3.
        let tiles = target(8.0, &midi);
        assert_eq!(tiles[0].span_ticks, 12 * TICKS_PER_BAR);
        // Left of the grab point never shrinks the clip.
        let tiles = target(-3.0, &midi);
        assert_eq!(tiles[0].span_ticks, 4 * TICKS_PER_BAR);

        // A single-bar clip tiles into two repeats after one bar of travel.
        let mut one_bar = midi.clone();
        one_bar[0].clips[0].span_ticks = TICKS_PER_BAR;
        one_bar[0].clips[0].loop_ticks = TICKS_PER_BAR;
        let tiles = target(1.0, &one_bar);
        assert_eq!(tiles[0].span_ticks, 2 * TICKS_PER_BAR);
        assert_eq!(tiles[0].loop_ticks, TICKS_PER_BAR);

        // An already-tiled clip adjusts its repeat count and keeps its
        // period — no nested tiling.
        let mut tiled = midi.clone();
        tiled[0].clips[0].loop_ticks = 2 * TICKS_PER_BAR;
        // No movement: unchanged (×2).
        let tiles = target(0.0, &tiled);
        assert_eq!(tiles[0].span_ticks, 4 * TICKS_PER_BAR);
        assert_eq!(tiles[0].loop_ticks, 2 * TICKS_PER_BAR);
        // Right by two periods: ×4.
        let tiles = target(4.0, &tiled);
        assert_eq!(tiles[0].span_ticks, 8 * TICKS_PER_BAR);
        assert_eq!(tiles[0].loop_ticks, 2 * TICKS_PER_BAR);
        // Left by one period: back to a single period (×1).
        let tiles = target(-2.0, &tiled);
        assert_eq!(tiles[0].span_ticks, 2 * TICKS_PER_BAR);
        assert_eq!(tiles[0].loop_ticks, 2 * TICKS_PER_BAR);
    }

    /// W2: splitting a tiled clip is refused (it would need a phase-relative
    /// loop origin the model does not carry).
    #[test]
    fn a_tiled_clip_cannot_be_split() {
        let mut clips = vec![MidiClip {
            start_ticks: 0,
            span_ticks: 2 * TICKS_PER_BAR,
            loop_ticks: TICKS_PER_BAR,
            ..MidiClip::default()
        }];
        assert!(
            !split_clip_in(&mut clips, TICKS_PER_BAR + TICKS_PER_BAR / 2, 0, SPT),
            "tiled clip must refuse the cut"
        );
        assert_eq!(clips.len(), 1);
    }

    /// W3a: F2 targets the first selection entry on the focused row and
    /// refuses stale indices / other lanes.
    #[test]
    fn f2_targets_the_selected_clip_on_its_own_row() {
        let clips = vec![
            MidiClip {
                name: "Verse".to_owned(),
                ..MidiClip::default()
            },
            MidiClip::default(),
        ];
        let sel = |lane, track, clip| ClipSel { lane, track, clip };

        assert_eq!(
            clip_rename_target(
                &[sel(TimelineLane::Midi, 1, 0)],
                TimelineLane::Midi,
                1,
                &clips
            ),
            Some((sel(TimelineLane::Midi, 1, 0), "Verse".to_owned()))
        );
        assert_eq!(
            clip_rename_target(
                &[sel(TimelineLane::Midi, 1, 1)],
                TimelineLane::Midi,
                1,
                &clips
            ),
            Some((sel(TimelineLane::Midi, 1, 1), String::new()))
        );
        // Wrong track / wrong lane: not this row's business.
        assert_eq!(
            clip_rename_target(
                &[sel(TimelineLane::Midi, 0, 0)],
                TimelineLane::Midi,
                1,
                &clips
            ),
            None
        );
        assert_eq!(
            clip_rename_target(
                &[sel(TimelineLane::Audio, 1, 0)],
                TimelineLane::Midi,
                1,
                &clips
            ),
            None
        );
        // Stale index.
        assert_eq!(
            clip_rename_target(
                &[sel(TimelineLane::Midi, 1, 9)],
                TimelineLane::Midi,
                1,
                &clips
            ),
            None
        );
        assert_eq!(clip_rename_target(&[], TimelineLane::Midi, 1, &clips), None);
    }

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

    /// 2026-11-04: the MIDI note preview is a silent no-op for degenerate
    /// inputs (empty notes, zero span, tiny rect) and tiles a tiled clip.
    #[test]
    fn midi_preview_handles_degenerate_inputs() {
        let mut clip = MidiClip::new(0);
        clip.span_ticks = 2 * TICKS_PER_BAR;
        clip.loop_ticks = TICKS_PER_BAR;
        clip.sequence.notes = vec![
            Note {
                start: 0,
                length: 240,
                note: 60,
                ..Note::default()
            },
            Note {
                start: i64::from(TICKS_PER_BAR) / 2,
                length: 480,
                note: 67,
                ..Note::default()
            },
        ];
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
                draw_midi_preview(&painter, rect, &clip);
                draw_midi_preview(&painter, rect, &MidiClip::default());
                let tiny = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(3.0, 3.0));
                draw_midi_preview(&painter, tiny, &clip);
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
        assert_eq!(clamp_row_delta(2, &sel, 3, 0, 0), 1);
        // Track 0 anchors the group: no downward move at all.
        assert_eq!(clamp_row_delta(-5, &sel, 3, 0, 0), 0);
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
        assert_eq!(clamp_row_delta(-1, &low, 3, 0, 0), -1);
        assert_eq!(clamp_row_delta(-5, &low, 3, 0, 0), -1);
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
