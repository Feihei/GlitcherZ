//! Piano roll editor component (egui self-drawn).
//!
//! Interaction model (M4, see `.agents/spec/editing-model.md`): the semantic
//! decision source is **discrete state**, not the pointer position.
//!
//! - **Persistent state** — [`RollMode::Select`] (a left-drag tweaks: moves
//!   the selection, trims edges, or marquees on empty space) and
//!   [`RollMode::Draw`] (sticky `d`, Esc exits: a drag on empty space
//!   paints a note). The mode never changes by itself.
//! - **Modal command family** — `g` Grab (X/Y axis lock), `s` Scale,
//!   `t` Stretch (`t+s` = start edge), `k` Cut, `Shift+d` duplicate +
//!   Grab. G/S/T/Shift+D enter a Blender-style modal transform the moment
//!   they are pressed: the selection follows the pointer live (drawn as
//!   ghosts — nothing is applied), LMB confirms, RMB/Esc cancels. `k`
//!   arms the knife: its stroke cuts every note it crosses, where it
//!   crosses (no pre-selection needed). Commands work in both modes.
//! - **Gesture shortcuts** — dragging a note moves, dragging its edge
//!   trims, Ctrl+drag copies: the unlimited forms of G/T/Shift+D, so the
//!   highest-frequency edits never need a key press. Dragging *is* the
//!   tweak gesture — the modal commands never wait for one.
//!
//! Mainstream DAW muscle memory preserved: double-click on empty space
//! inserts a default-length note; Shift/Ctrl click toggles selection;
//! right-click deletes; arrow keys nudge (up/down a semitone, Shift an
//! octave, left/right one grid cell); mouse wheel scrolls the pitch view.
//!
//! The time snap granularity is a [`grid::GridValue`]. Grid lines re-anchor
//! on every bar line rather than being counted from tick 0, so they stay
//! aligned under any meter. The playhead is driven by the musical position
//! read back from the engine.
//!
//! **This module does not mutate the document.** It draws the clip read-only
//! and returns [`RollIntent`]s, which the caller runs as commands so that
//! every edit is undoable (M1.4). Drags emit one intent per frame and are
//! expected to be run through a merged gesture, so a whole drag is a single
//! undo step.
//!
//! **Several clips share one time axis** (P3, spec `roll-multiclip.md` D4–D7):
//! a [`RollSubject`] carries the *focus* clip (writable, fully interactive)
//! plus a *context* of further clips drawn as read-only ghosts. Ghosts are
//! paint-only — they never enter a hit test — and the focus is switched from
//! the caller's chip strip, never by clicking a ghost.

use eframe::egui;

use crate::grid::GridValue;
use glitcherz_model::{ControlClip, MidiClip, MidiTrack, Note, TICKS_PER_QUARTER, TICKS_PER_STEP};

pub use crate::grid::NoteValue;

/// How many clips one roll may show at once (spec `roll-multiclip.md` D4).
/// Beyond this the context is truncated in timeline-selection order with a
/// status hint — the cap exists so the per-frame paint cost stays bounded.
pub const MAX_ROLL_CONTEXT: usize = 8;

/// Height of each semitone row (pixels).
pub const ROW_HEIGHT: f32 = 9.0;

/// Roll area height (pixels, 32 semitone rows).
pub const ROLL_HEIGHT: f32 = ROW_HEIGHT * 32.0;

/// Velocity lane height (pixels), below the roll.
pub const LANE_HEIGHT: f32 = 48.0;

/// Height of one overlay curve lane (pixels), stacked under the velocity
/// lane. Taller than the velocity strip because it carries a real value axis
/// the user drags within.
pub const CURVE_LANE_HEIGHT: f32 = 64.0;

/// Anchor hit radius (px) inside a curve lane.
const ANCHOR_RADIUS: f32 = 8.0;

/// Height of a collapsed curve lane: just its caption strip (P5).
pub const CURVE_LANE_COLLAPSED_HEIGHT: f32 = 18.0;

/// Side of the collapse/expand toggle square in a lane's caption.
const LANE_TOGGLE: f32 = 12.0;

/// Overlay-lane curve stroke and anchor colour: cool blue, distinct from the
/// green notes and the yellow playhead so a curve never reads as a selection.
const CURVE_LINE: egui::Color32 = egui::Color32::from_rgb(110, 170, 235);
const ANCHOR: egui::Color32 = egui::Color32::from_rgb(190, 220, 255);

/// Horizontal zoom bounds (multiplier over the fit-to-width scale; 1.0 shows
/// the whole loop).
const ZOOM_X_MIN: f32 = 0.2;
const ZOOM_X_MAX: f32 = 64.0;

/// Vertical zoom bounds (multiplier over [`ROW_HEIGHT`]; 1.0 = 9 px rows).
const ZOOM_Y_MIN: f32 = 0.4;
const ZOOM_Y_MAX: f32 = 6.0;

/// Default duration of a new note (tick) — a 16th note.
const DEFAULT_NOTE_LENGTH: i64 = TICKS_PER_STEP as i64;

/// Accent color for selected notes / velocity bars.
const SELECTED: egui::Color32 = egui::Color32::from_rgb(240, 190, 80);

/// Ghost clip notes: the focus clip's note colour at ~22% alpha, premultiplied
/// so this can stay a `const` (P3, D6). Deliberately *not* velocity-shaded — a
/// ghost answers "what else is around here", not "how loud is it".
const GHOST_NOTE: egui::Color32 = egui::Color32::from_rgba_premultiplied(14, 35, 21, 56);

/// Pointer distance (px) to a note's start/end that turns a grab into an
/// edge trim instead of a move.
///
/// Sized against [`ROW_HEIGHT`] (9 px): a note body only has to clear this
/// on *both* sides to offer a move zone. The grab is classified by the
/// *press* point (see the `drag_started` branch below), so this width is
/// the true grab target — no slack for `drag_started`'s late report is
/// needed here.
const EDGE_PX: f32 = 8.0;

/// Extra forgiveness around a note's drawn rectangle, for hit-testing only.
/// A note's tail is a 1–2 px target, so a press aimed at it routinely
/// lands a few pixels *outside* the drawn rect; without slack that reads
/// as empty space and paints a new note. The x slack is generous because
/// the tail is hard to hit; the y slack only fills the 1 px gutter
/// between pitch rows.
const HIT_SLOP: egui::Vec2 = egui::vec2(4.0, 1.0);

/// Floor for the Scale command's live factor (a 50× compression is already
/// past the point where every note has clamped to its one-tick minimum).
/// Keeps the emitted increment strictly positive, which the command
/// requires.
const MIN_SCALE_K: f32 = 0.02;

/// One frame's relevant key presses (the M4 command family).
#[derive(Default)]
struct Keys {
    shift_d: bool,
    escape: bool,
    d: bool,
    g: bool,
    s: bool,
    t: bool,
    k: bool,
    x: bool,
    y: bool,
    delete: bool,
}

/// An edit the roll wants performed.
#[derive(Debug, Clone, PartialEq)]
pub enum RollIntent {
    /// Create a note and return its index (after sorting).
    ///
    /// `gesture` ties the creation to the draw-drag that may follow it
    /// (`ResizeEnd` frames carry the same id), so one press-draw-release
    /// collapses into a single undo step.
    AddNote {
        note: u8,
        start: i64,
        length: i64,
        gesture: u64,
    },
    /// One frame of an end trim (draw gesture or right-edge drag): stretch
    /// the end of every selected note by `delta_ticks`. The command clamps
    /// per note (length ≥ 1) and reports where each note ended up.
    ResizeEnd { delta_ticks: i64, gesture: u64 },
    /// One frame of a start trim (left-edge drag): shift the start of every
    /// selected note by `delta_ticks`, ends following (the command applies
    /// the mirrored length delta). Clamped per note (start ≥ 0, length ≥ 1).
    ResizeStart { delta_ticks: i64, gesture: u64 },
    /// Move the notes at `indices` by a uniform delta (ticks / semitones).
    ///
    /// `gesture` is `Some` for drag frames (each drag is its own undo step)
    /// and `None` for arrow-key nudges (a burst of nudges merges into one).
    /// A uniform delta can still permute the sort against *unselected*
    /// notes, so the command reports the moved notes' new indices and the
    /// caller re-derives the selection from them.
    MoveNotes {
        indices: Vec<usize>,
        delta_start: i64,
        delta_note: i8,
        gesture: Option<u64>,
    },
    /// Copy the selected notes (Ctrl + drag). The command reports the
    /// copies' indices; the caller selects them, and the move frames of the
    /// same drag share `gesture`, so copy + move is a single undo step.
    Duplicate { indices: Vec<usize>, gesture: u64 },
    /// One frame of a time scale (the Scale command's drag): stretch the
    /// selection by `factor` around `pivot_tick` (the selection bbox
    /// *start*, frozen at grab time — scaling from the start keeps the
    /// notes' relative layout intact). The UI emits only the increment
    /// still owed (`target_k / applied_k`), so the frames do not compound;
    /// the command clamps per note (start ≥ 0, length ≥ 1) and reports
    /// where the notes landed.
    ScaleNotes {
        pivot_tick: i64,
        factor: f32,
        gesture: u64,
    },
    /// Cut notes at ticks (the Cut command / the knife stroke's batch):
    /// every `(index, tick)` splits the note at `index` (still spanning
    /// `tick` at execution time) into two halves, pitch/velocity/channel
    /// preserved. One invocation is one undo step; the pointer tick is
    /// taken unsnapped — cutting is a precision operation, not a grid one.
    SplitNotes {
        cuts: Vec<(usize, i64)>,
        gesture: u64,
    },
    /// Remove the notes at these indices.
    DeleteNotes { indices: Vec<usize> },
    /// Set absolute velocities (index, velocity) pairs.
    SetVelocities { updates: Vec<(usize, u8)> },
    /// One frame of an overlay-lane anchor edit (P4, D16): upsert `tick` (clip
    /// relative) at `value`, first removing the anchor at `from` when the drag
    /// is moving one. `dragging` selects the merged undo gesture, exactly like
    /// the standalone curve editor — one drag, one Ctrl+Z.
    CurveUpsert {
        track: usize,
        clip: usize,
        from: Option<u32>,
        tick: u32,
        value: f32,
        dragging: bool,
    },
    /// Delete the nearest anchor within half a snap step of `tick`.
    CurveRemove {
        track: usize,
        clip: usize,
        tick: u32,
    },
    /// The lane drag ended: close the merged undo gesture.
    CurveDragEnd,
}

/// Which table a [`ClipRef`] points into (P3, spec `roll-multiclip.md` D4).
/// P4 adds `Control`, whose clips render in overlay lanes rather than in the
/// main area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollLane {
    Midi,
    /// An automation (control) clip: its curve gets an overlay lane below the
    /// roll (P4, spec `roll-multiclip.md` D8/D16).
    Control,
}

/// One curve lane drawn under the roll (P4, D8).
///
/// The roll stays ignorant of cables and parameters: the caller resolves the
/// vertical axis from the clip's automation destination (`curve::axis`) and
/// supplies a caption. What the roll owns is the **time axis**, which is the
/// same [`RollGeom`] the notes use — that is the whole point of the feature
/// (a pitch curve lining up with the notes it bends).
pub struct OverlayLane<'a> {
    /// The control clip this lane edits. Writing targets this clip and only
    /// this clip (D16: the roll writes notes, lanes write curves).
    pub clip: ClipRef,
    /// The clip's curve, borrowed for this frame.
    pub data: &'a ControlClip,
    /// Caption drawn in the lane's top-left corner.
    pub label: String,
}

impl OverlayLane<'_> {
    /// Anchors are normalized (`curve-map.md` D1), so the lane's geometry is
    /// the same for every destination: -1 at the bottom, +1 at the top. The
    /// caption carries the target, not the mapping.
    fn value_to_y(&self, value: f32, rect: egui::Rect) -> f32 {
        rect.bottom() - crate::curve::y_of(value) * rect.height()
    }

    fn y_to_value(&self, y: f32, rect: egui::Rect) -> f32 {
        crate::curve::norm_of_y((rect.bottom() - y) / rect.height().max(1.0))
    }
}

/// One clip addressed inside the project tables — the roll's unit of context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipRef {
    pub lane: RollLane,
    pub track: usize,
    pub clip: usize,
}

impl ClipRef {
    /// A MIDI clip at `(track, clip)`.
    pub fn midi(track: usize, clip: usize) -> Self {
        Self {
            lane: RollLane::Midi,
            track,
            clip,
        }
    }

    /// A control clip at `(track, clip)` — P4: shown as an overlay lane.
    pub fn control(track: usize, clip: usize) -> Self {
        Self {
            lane: RollLane::Control,
            track,
            clip,
        }
    }
}

/// What one roll frame shows: the writable **focus** clip plus the read-only
/// **context** drawn behind it as ghosts (P3, spec `roll-multiclip.md` D4).
///
/// The focus is always a member of the context. Notes and commands stay
/// clip-local, so nothing above the geometry layer needs to know that more
/// than one clip is on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollSubject {
    /// The clip being edited (index into `context`).
    focus: usize,
    /// Every visible clip, in timeline-selection order, focus included.
    context: Vec<ClipRef>,
    /// How many live selection entries did not fit under
    /// [`MAX_ROLL_CONTEXT`] — the caller surfaces it as a status hint.
    dropped: usize,
}

impl RollSubject {
    /// A subject showing exactly one clip — the pre-P3 behaviour, and what an
    /// empty timeline selection degenerates to.
    pub fn single(track: usize, clip: usize) -> Self {
        Self {
            focus: 0,
            context: vec![ClipRef::midi(track, clip)],
            dropped: 0,
        }
    }

    /// Builds a subject from a selection: `focus` is the double-clicked clip,
    /// which joins the context at the front when the selection does not
    /// already contain it. Truncated to [`MAX_ROLL_CONTEXT`]; the focus is
    /// always kept.
    ///
    /// `clips` is the number of MIDI clips per track, used to drop stale
    /// entries (a selection outlives a clip deletion).
    pub fn assemble(
        focus: ClipRef,
        selection: &[ClipRef],
        midi_clips: impl Fn(usize) -> usize,
        control_clips: impl Fn(usize) -> usize,
    ) -> Self {
        // P4: a control clip joins the context as an overlay lane. `live`
        // asks each table for its clip count, so a selection that outlived a
        // deleted clip is dropped instead of pointing at a stranger's clip.
        let live = |c: &ClipRef| match c.lane {
            RollLane::Midi => c.clip < midi_clips(c.track),
            RollLane::Control => c.clip < control_clips(c.track),
        };
        let mut context: Vec<ClipRef> = Vec::with_capacity(MAX_ROLL_CONTEXT);
        context.push(focus);
        let mut dropped = 0;
        for clip in selection {
            if *clip == focus || !live(clip) || context.contains(clip) {
                continue;
            }
            if context.len() == MAX_ROLL_CONTEXT {
                dropped += 1;
                continue;
            }
            context.push(*clip);
        }
        Self {
            focus: 0,
            context,
            dropped,
        }
    }

    /// Selection entries that did not fit in the context (status hint).
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// The writable clip.
    pub fn focus(&self) -> ClipRef {
        // `focus` indexes `context`, which `assemble`/`single` keep non-empty.
        self.context[self.focus.min(self.context.len() - 1)]
    }

    /// Every visible clip, focus first.
    pub fn context(&self) -> &[ClipRef] {
        &self.context
    }

    /// Mutable view of the context — for app-side index remapping (track
    /// reorder) where every member has to move together.
    pub fn context_mut(&mut self) -> &mut [ClipRef] {
        &mut self.context
    }

    /// Moves the focus to `clip` (a context member) — D7: a pure view-state
    /// change, never a document edit. Returns `false` when `clip` is not in
    /// the context, leaving the subject untouched.
    pub fn set_focus(&mut self, clip: ClipRef) -> bool {
        match self.context.iter().position(|c| *c == clip) {
            Some(index) => {
                self.focus = index;
                true
            }
            None => false,
        }
    }

    /// The next focus in context order, wrapping — `Tab` forward, `Shift+Tab`
    /// backward (D7). A one-clip subject stays put.
    pub fn cycle_focus(&mut self, backward: bool) -> ClipRef {
        let len = self.context.len();
        if len > 1 {
            self.focus = if backward {
                (self.focus + len - 1) % len
            } else {
                (self.focus + 1) % len
            };
        }
        self.focus()
    }
}

/// State persisted across frames (scroll, grid, selection, gesture).
#[derive(Debug)]
pub struct RollState {
    /// Top visible MIDI note number (f32 for smooth scrolling).
    pub top_note: f32,
    /// Horizontal zoom (multiplier over the fit-to-width scale; 1.0 = the
    /// whole clip loop fits the roll width).
    pub zoom_x: f32,
    /// Vertical zoom (multiplier over [`ROW_HEIGHT`]; 1.0 = 9 px rows).
    pub zoom_y: f32,
    /// Horizontal scroll offset: the **absolute** (arrangement) tick at the
    /// roll's left edge (P2, spec `roll-multiclip.md` D1/D2). Absolute rather
    /// than clip-local so several clips can share one time axis.
    pub scroll_x: f32,
    /// The focus clip's arrangement start the current `scroll_x` was anchored
    /// on. The state outlives target switches, so a new clip re-anchors the
    /// view to its own start instead of inheriting a scroll that may lie
    /// outside its range.
    view_origin: Option<i64>,
    /// Snap granularity / grid density.
    pub grid: GridValue,
    /// Indices into `clip.notes()` of the selected notes.
    pub selection: Vec<usize>,
    /// Persistent mode: what a left-drag on the roll means (M4).
    pub mode: RollMode,
    /// The live modal transform (G/S/T/Shift+D), if any: the selection
    /// follows the pointer as ghosts until LMB confirms or RMB/Esc cancels.
    modal: Option<Modal>,
    /// Whether the knife (K) is armed: its next stroke cuts what it crosses.
    pub knife_armed: bool,
    /// One-frame status message for a no-op (empty selection, Cut with
    /// nothing under the pointer); the caller drains it into the app's
    /// status bar via [`RollState::take_status`].
    status: Option<String>,
    /// Whether the roll owned keyboard focus on the previous frame. The
    /// toolbar (drawn before the roll is allocated) reads it one frame
    /// late to surface the invisible focus gate: every keyboard shortcut
    /// is focus-gated, so a dead D/G/T/S is either this or no key events
    /// reaching egui at all (OS / IME level).
    had_focus: bool,
    /// The roll widget's id, registered each frame so the app can ask
    /// egui whether the roll owns keyboard focus right now — command
    /// keys (G/T/S/D/K...) must not also trigger the virtual piano.
    focus_id: Option<egui::Id>,
    /// Selection drag in progress (uniform move of all selected notes, a
    /// trim of either edge, a time scale, or a draw gesture setting a
    /// fresh note's length).
    drag: Option<DragSelection>,
    /// Rubber band in progress (drag on empty space in Select mode;
    /// Shift/Ctrl makes it additive).
    marquee: Option<Marquee>,
    /// The active knife stroke (armed `k` + a drag): cuts gather along the
    /// path and land as one batch on release.
    knife: Option<KnifeStroke>,
    /// A note was just requested on empty space; the roll does not know its
    /// sorted index until the command reports back, but the draw-drag must
    /// start immediately — the gesture id is kept here.
    awaiting_create: Option<AwaitCreate>,
    /// Monotonic id for press-draw gestures: each draw (AddNote + its
    /// ResizeEnd frames) becomes one undo step.
    draw_seq: u64,
    /// In-flight overlay-lane anchor drag (P4): `(lane index, the anchor it
    /// grabbed)`. `None` inside the tuple means the press **created** the
    /// anchor, so subsequent frames only move it.
    curve_drag: Option<(usize, Option<u32>)>,
    /// Screen rect of each overlay lane, in the order they were handed to
    /// [`show`]. Published so callers (and tests) can map pointer positions
    /// onto a lane without re-deriving the stack geometry.
    pub lane_rects: Vec<egui::Rect>,
    /// Control **track** indices whose overlay lane is collapsed to its
    /// caption (P5). Sorted, view-state only — collapsing is not a document
    /// edit and never enters undo.
    pub collapsed_lanes: Vec<usize>,
}

/// Persistent mode: what a left-drag on the roll means (M4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollMode {
    /// Drag = move the selection / trim edges / marquee on empty space.
    Select,
    /// Sticky draw (`d` to enter, Esc or `d` again to exit): a drag on
    /// empty space paints a note (note on at the press, note off at the
    /// release). Exists because note input is bursty — one drag per note
    /// without re-arming anything.
    Draw,
}

impl RollMode {
    /// The other mode (`d` toggles).
    fn toggled(self) -> Self {
        match self {
            RollMode::Select => RollMode::Draw,
            RollMode::Draw => RollMode::Select,
        }
    }
}

/// Axis lock for Grab (Blender convention: X = time only, Y = pitch only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Time,
    Pitch,
}

/// Which edge Stretch pulls. Bare `t` defaults to the end edge — fixing a
/// note's length is the more common trim (M4, unified rule: the bare
/// command is the most common variant, the qualifier the less common one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StretchEdge {
    Start,
    End,
}

/// A live modal transform (M4a.4, Blender-style): armed by `g`/`s`/`t`/
/// `Shift+d`, it follows the pointer as soon as it exists — drawn as
/// ghosts, applying nothing — and LMB confirms it (one final intent),
/// RMB/Esc cancels it (nothing to undo: the preview never touched the
/// document). Because nothing is applied mid-modal, the selection indices
/// stay valid throughout and the final intent can be computed in one shot.
#[derive(Debug)]
pub struct Modal {
    kind: ModalKind,
    /// Pointer tick at arm time (the move's zero point).
    grab_tick: f32,
    /// Pointer pitch at arm time.
    grab_note: u8,
    /// One modal = one undo step; Shift+D's duplicate shares the id.
    gesture: u64,
}

/// What a [`Modal`] does to the selection.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ModalKind {
    /// Move the selection; `axis` is the live lock (X/Y, Blender
    /// convention: X = time only, Y = pitch only).
    Move { axis: Option<Axis> },
    /// Time-scale the selection around the bbox *start* (frozen at arm).
    Scale {
        pivot_tick: i64,
        /// Pointer distance to the pivot at arm time (ticks, ≥ 1), so the
        /// live factor starts at 1.
        grab_dist: f32,
        max_k: f32,
    },
    /// Trim the selection bbox edge.
    Stretch { edge: StretchEdge, lo: i64, hi: i64 },
}

impl Modal {
    /// Short chip label for the toolbar (qualifiers included).
    fn label(&self) -> &'static str {
        match self.kind {
            ModalKind::Move { axis: None } => "G",
            ModalKind::Move {
                axis: Some(Axis::Time),
            } => "G·X",
            ModalKind::Move {
                axis: Some(Axis::Pitch),
            } => "G·Y",
            ModalKind::Scale { .. } => "S",
            ModalKind::Stretch {
                edge: StretchEdge::End,
                ..
            } => "T",
            ModalKind::Stretch {
                edge: StretchEdge::Start,
                ..
            } => "T·S",
        }
    }

    /// Applies an axis qualifier (X/Y) to the move. Pressing the active
    /// axis again releases the lock; the other axis takes over.
    fn apply_axis(&mut self, axis: Axis) {
        if let ModalKind::Move { axis: locked } = &mut self.kind {
            *locked = if *locked == Some(axis) {
                None
            } else {
                Some(axis)
            };
        }
    }

    /// The transform's live geometry for one pointer position: what the
    /// ghosts should show, and what a confirming click would apply.
    fn frame(
        &self,
        clip: &MidiClip,
        selection: &[usize],
        pointer_tick: f32,
        pointer_note: u8,
        grid_ticks: i64,
        loop_ticks: i64,
    ) -> ModalFrame {
        match self.kind {
            ModalKind::Move { axis } => {
                let want_start =
                    snap_ticks(pointer_tick, grid_ticks) - snap_ticks(self.grab_tick, grid_ticks);
                let want_note = i64::from(pointer_note) - i64::from(self.grab_note);
                // An axis lock zeroes that axis' target (pressing it again
                // walks the preview back to the grab position).
                let (want_start, want_note) = match axis {
                    Some(Axis::Time) => (want_start, 0),
                    Some(Axis::Pitch) => (0, want_note),
                    None => (want_start, want_note),
                };
                let (delta_start, delta_note) =
                    clamped_move_deltas(clip, selection, want_start, want_note, loop_ticks);
                ModalFrame {
                    delta_start,
                    delta_note,
                    scale_k: 1.0,
                    pivot_tick: 0,
                    edge: None,
                }
            }
            ModalKind::Scale {
                pivot_tick,
                grab_dist,
                max_k,
            } => {
                // Unsnapped on purpose — scale is continuous, and the spec
                // forbids grid snapping for it.
                let dist = (pointer_tick - pivot_tick as f32).abs();
                let scale_k = (dist / grab_dist).clamp(MIN_SCALE_K, max_k);
                ModalFrame {
                    delta_start: 0,
                    delta_note: 0,
                    scale_k,
                    pivot_tick,
                    edge: None,
                }
            }
            ModalKind::Stretch { edge, lo, hi } => {
                let delta = match edge {
                    StretchEdge::End => draw_end(lo, pointer_tick, grid_ticks, loop_ticks) - hi,
                    StretchEdge::Start => trim_start(hi, pointer_tick, grid_ticks) - lo,
                };
                ModalFrame {
                    delta_start: 0,
                    delta_note: 0,
                    scale_k: 1.0,
                    pivot_tick: 0,
                    edge: Some((edge, delta)),
                }
            }
        }
    }

    /// The single intent that applies the whole transform — or `None` when
    /// the pointer never left the grab point (a no-op must not spend an
    /// undo step).
    fn finalize(
        &self,
        clip: &MidiClip,
        selection: &[usize],
        frame: ModalFrame,
        loop_ticks: i64,
    ) -> Option<RollIntent> {
        match self.kind {
            ModalKind::Move { .. } if frame.delta_start != 0 || frame.delta_note != 0 => {
                Some(clamped_move(
                    clip,
                    selection,
                    frame.delta_start,
                    frame.delta_note,
                    loop_ticks,
                    Some(self.gesture),
                ))
            }
            ModalKind::Scale { pivot_tick, .. } if (frame.scale_k - 1.0).abs() > f32::EPSILON => {
                Some(RollIntent::ScaleNotes {
                    pivot_tick,
                    factor: frame.scale_k,
                    gesture: self.gesture,
                })
            }
            ModalKind::Stretch {
                edge: StretchEdge::End,
                ..
            } if frame.edge.is_some_and(|(_, delta)| delta != 0) => Some(RollIntent::ResizeEnd {
                delta_ticks: frame.edge.unwrap().1,
                gesture: self.gesture,
            }),
            ModalKind::Stretch {
                edge: StretchEdge::Start,
                ..
            } if frame.edge.is_some_and(|(_, delta)| delta != 0) => Some(RollIntent::ResizeStart {
                delta_ticks: frame.edge.unwrap().1,
                gesture: self.gesture,
            }),
            _ => None,
        }
    }
}

/// One pointer position's worth of [`Modal`] geometry (for the ghost
/// preview and the confirming click's final intent).
#[derive(Debug, Clone, Copy)]
struct ModalFrame {
    /// Move: the clamped total offset from the grab point.
    delta_start: i64,
    delta_note: i8,
    /// Scale: the factor around `pivot_tick` (1.0 = untouched).
    scale_k: f32,
    pivot_tick: i64,
    /// Stretch: the edge and its snapped total delta (0 = untouched).
    edge: Option<(StretchEdge, i64)>,
}

impl ModalFrame {
    /// Whether the frame would leave the notes where they are.
    fn is_identity(&self) -> bool {
        self.delta_start == 0
            && self.delta_note == 0
            && (self.scale_k - 1.0).abs() <= f32::EPSILON
            && self.edge.is_none_or(|(_, delta)| delta == 0)
    }
}

/// An armed knife (`k`) stroke: the path the pointer sweeps cuts every
/// note it crosses, where it crosses — no pre-selection needed. The cuts
/// gather during the stroke and land as one batch on release (one stroke
/// = one undo step): split commands re-sort the clip, so applying
/// mid-stroke would invalidate the very indices the next frame's segment
/// test uses.
#[derive(Debug)]
struct KnifeStroke {
    /// Where the previous frame's segment ended.
    last: egui::Pos2,
    gesture: u64,
    /// `(note index, cut tick)` pairs gathered so far — the first crossing
    /// of a note wins. Indices are safe: nothing mutates mid-stroke.
    cuts: Vec<(usize, i64)>,
    /// The swept path, for the stroke's visual feedback.
    path: Vec<egui::Pos2>,
}

/// Which kind of drag a [`DragSelection`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragKind {
    /// Move the whole selection (a grab on an existing note). `axis` is
    /// the live lock; `None` is a free move (X/Y applies mid-drag too).
    Move { axis: Option<Axis> },
    /// Trim the start edge (left-edge drag).
    ResizeStart,
    /// Draw the length of a just-created note (note on at the press point,
    /// note off at the release point) or trim the end edge (right-edge
    /// drag): the end follows the pointer.
    ResizeEnd,
}

/// One in-flight drag (move, trim, or scale).
///
/// The grabbed geometry is **frozen here at grab time** and the per-frame
/// intent is the increment still owed (`target - applied`). Both halves
/// matter:
///
/// - *Frozen anchor* — every command re-sorts the clip, so a held index
///   stops pointing at the grabbed note after the first frame. Deriving
///   the target from the anchor instead of re-reading `clip.notes()[i]`
///   keeps a trim attached to the note the user grabbed.
/// - *Incremental* — commands apply *relative* deltas (`start + ticks`).
///   Re-emitting the grab-relative total every frame would compound
///   (1 + 2 + 3 + … grid cells), so the note would run away from the
///   pointer instead of tracking it.
///
/// A direct edge grab anchors on the *grabbed note's* edge; the armed
/// Stretch command anchors on the *selection bbox* edge (no note was
/// grabbed, so the selection as a whole is the target).
#[derive(Debug)]
struct DragSelection {
    /// Start tick of the grabbed note at grab time. A move offsets from
    /// it; a right-edge trim holds it still (it is the base for the
    /// minimum-length floor).
    anchor_start: i64,
    /// End tick of the grabbed note at grab time. A right-edge trim
    /// offsets from it; a left-edge trim holds it still (the command
    /// mirrors the length delta, so the end never moves).
    anchor_end: i64,
    /// Pointer tick at grab time (f32; snapping happens per frame).
    grab_tick: f32,
    /// Pointer pitch at grab time.
    grab_note: u8,
    /// Cumulative start shift (ticks) already handed to the command.
    applied_start: i64,
    /// Cumulative end shift (ticks) already handed to the command.
    applied_end: i64,
    /// Cumulative pitch shift (semitones) already handed to the command.
    applied_note: i64,
    kind: DragKind,
    /// Gesture id: one drag is one undo step (the id is part of the undo
    /// merge key, and `AddNote`/`Duplicate` frames carry the same id).
    gesture: u64,
}

impl DragSelection {
    /// Anchors a gesture on `note`, frozen as it is at grab time.
    fn anchored(note: &Note, grab_tick: f32, grab_note: u8, kind: DragKind, gesture: u64) -> Self {
        Self::at(
            note.start,
            note.start + note.length,
            grab_tick,
            grab_note,
            kind,
            gesture,
        )
    }

    /// Anchors a gesture on explicit start/end ticks (a bbox-anchored
    /// gesture that names no single note).
    fn at(
        anchor_start: i64,
        anchor_end: i64,
        grab_tick: f32,
        grab_note: u8,
        kind: DragKind,
        gesture: u64,
    ) -> Self {
        Self {
            anchor_start,
            anchor_end,
            grab_tick,
            grab_note,
            applied_start: 0,
            applied_end: 0,
            applied_note: 0,
            kind,
            gesture,
        }
    }

    /// The grabbed note's start / end / pitch as the document has them
    /// *right now* (anchor plus everything already applied).
    fn current_start(&self) -> i64 {
        self.anchor_start + self.applied_start
    }

    fn current_end(&self) -> i64 {
        self.anchor_end + self.applied_end
    }
}

#[derive(Debug)]
struct Marquee {
    origin: egui::Pos2,
    current: egui::Pos2,
    /// Whether the band extends the existing selection (Shift/Ctrl) or
    /// replaces it (a plain empty drag in Select mode).
    additive: bool,
}

#[derive(Debug)]
struct AwaitCreate {
    grab_tick: f32,
    grab_note: u8,
    gesture: u64,
}

impl RollState {
    /// Initial view top at C6 (84), showing about two and a half octaves downward.
    pub fn new() -> Self {
        Self {
            top_note: 84.0,
            zoom_x: 1.0,
            zoom_y: 1.0,
            scroll_x: 0.0,
            view_origin: None,
            grid: GridValue::Note(NoteValue {
                division: 16,
                dotted: false,
                triplet: false,
            }),
            selection: Vec::new(),
            mode: RollMode::Select,
            modal: None,
            knife_armed: false,
            status: None,
            had_focus: false,
            focus_id: None,
            drag: None,
            marquee: None,
            knife: None,
            awaiting_create: None,
            draw_seq: 0,
            curve_drag: None,
            lane_rects: Vec::new(),
            collapsed_lanes: Vec::new(),
        }
    }

    /// Replaces the selection with `index` (the note a gesture just
    /// created) and, if that creation came from a press-and-draw on empty
    /// space, starts drawing the new note's length right away.
    ///
    /// `start` / `length` describe the note as it was just created, so
    /// the draw-drag can anchor on it without waiting for (or indexing
    /// into) the re-sorted clip.
    pub fn resolve_created(&mut self, index: usize, start: i64, length: i64) {
        self.selection = vec![index];
        if let Some(grab) = self.awaiting_create.take() {
            let anchor = Note {
                start,
                length,
                ..Note::default()
            };
            self.drag = Some(DragSelection::anchored(
                &anchor,
                grab.grab_tick,
                grab.grab_note,
                DragKind::ResizeEnd,
                grab.gesture,
            ));
        }
    }

    /// Fresh id for a press-draw gesture (one draw = one undo step).
    fn next_gesture(&mut self) -> u64 {
        self.draw_seq += 1;
        self.draw_seq
    }

    /// Whether a drag, marquee, modal transform or knife stroke is in
    /// progress.
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
            || self.marquee.is_some()
            || self.modal.is_some()
            || self.knife.is_some()
    }

    /// Drops every gesture, the modal transform, the knife and the
    /// selection.
    ///
    /// Called before undo/redo (and after deletions): the held indices refer
    /// to a document that changed underneath them. The persistent mode
    /// survives — it is a preference, not gesture state.
    pub fn clear_gesture(&mut self) {
        self.drag = None;
        self.marquee = None;
        self.awaiting_create = None;
        self.modal = None;
        self.knife = None;
        self.knife_armed = false;
        self.curve_drag = None;
        self.selection.clear();
    }

    /// Takes the pending status message (a no-op explanation), if any.
    pub fn take_status(&mut self) -> Option<String> {
        self.status.take()
    }

    /// Whether the roll widget owns keyboard focus *right now*, per egui's
    /// own memory (no frame lag; `false` once the roll stops being laid
    /// out — egui's dead-mans-switch drops its focus). The app gates the
    /// virtual piano on this: while the roll is focused, the computer
    /// keyboard drives the command family, not the notes.
    pub fn owns_keyboard_focus(&self, ctx: &egui::Context) -> bool {
        self.focus_id
            .is_some_and(|id| ctx.memory(|m| m.has_focus(id)))
    }

    /// The roll widget's id as of the most recent frame, for tests and
    /// focus surgery (`Memory::surrender_focus`).
    pub fn focus_id(&self) -> Option<egui::Id> {
        self.focus_id
    }

    /// Whether a modal transform is live (G/S/T/Shift+D following the
    /// pointer, awaiting LMB confirm or RMB/Esc cancel).
    pub fn modal_active(&self) -> bool {
        self.modal.is_some()
    }

    /// Records a one-frame status message for the app's status bar.
    fn note_status(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
    }

    /// Esc: cancel the modal transform (nothing has been applied — the
    /// preview never touched the document); else cancel the knife; else
    /// leave Draw mode; else clear the selection. The priority keeps a
    /// running interaction cancellable without side effects on the mode or
    /// selection it was about to act on.
    fn escape(&mut self) {
        if self.modal.take().is_some() || self.knife.take().is_some() || self.knife_armed {
            self.knife_armed = false;
            return;
        }
        if self.mode == RollMode::Draw {
            self.mode = RollMode::Select;
        } else {
            self.selection.clear();
        }
    }

    /// Applies an axis qualifier (X/Y) to the running modal move or the
    /// running tweak drag. Pressing the active axis again releases the
    /// lock (Blender convention); the other axis takes over.
    fn apply_axis(&mut self, axis: Axis) {
        if let Some(modal) = &mut self.modal {
            modal.apply_axis(axis);
        } else if let Some(drag) = &mut self.drag
            && let DragKind::Move { axis: locked } = &mut drag.kind
        {
            *locked = if *locked == Some(axis) {
                None
            } else {
                Some(axis)
            };
        }
    }
}

impl Default for RollState {
    fn default() -> Self {
        Self::new()
    }
}

fn is_black_key(note: i32) -> bool {
    matches!(note.rem_euclid(12), 1 | 3 | 6 | 8 | 10)
}

/// Time / pitch mapping shared by every paint and hit test (P2, spec
/// `roll-multiclip.md` D1).
///
/// `t0_x` maps **absolute** tick 0 to the screen, and `origin_tick` is the
/// arrangement start of the clip whose local ticks are being drawn, so a local
/// tick `t` lands at `t0_x + (origin_tick + t) * ticks_to_x`. Keeping that one
/// conversion in a struct is what lets several clips — each with its own
/// origin — share a single time axis in P3/P4 without touching the note-level
/// math (commands, modals and clamps all keep working in local ticks).
#[derive(Debug, Clone, Copy)]
struct RollGeom {
    /// Arrangement tick of the clip the local ticks belong to.
    origin_tick: i64,
    /// Screen x of absolute tick 0.
    t0_x: f32,
    ticks_to_x: f32,
    /// Pitch of the row at `area.top()`.
    top_note: i32,
    row_h: f32,
    /// The strip whose rows this geometry maps (`top()` anchors the pitches).
    area: egui::Rect,
}

impl RollGeom {
    /// Screen x of a **clip-local** tick.
    fn x_of(&self, local_tick: f32) -> f32 {
        self.t0_x + (self.origin_tick as f32 + local_tick) * self.ticks_to_x
    }

    /// Clip-local tick under a screen x (negative left of the clip origin;
    /// callers that create notes clamp it themselves).
    fn local_at(&self, x: f32) -> f32 {
        (x - self.t0_x) / self.ticks_to_x - self.origin_tick as f32
    }

    /// Screen y of the row for `note`.
    fn y_of(&self, note: u8) -> f32 {
        self.area.top() + (self.top_note as f32 - f32::from(note)) * self.row_h
    }

    /// Same time axis over another strip (the velocity lane, later the
    /// overlay curve lanes): only the vertical anchor and the left edge move.
    fn over(&self, area: egui::Rect) -> Self {
        Self {
            t0_x: self.t0_x + (area.left() - self.area.left()),
            area,
            ..*self
        }
    }

    /// Same time axis for a clip with another arrangement origin — the only
    /// thing a ghost needs (P3, spec `roll-multiclip.md` D6).
    fn at_origin(&self, origin_tick: i64) -> Self {
        Self {
            origin_tick,
            ..*self
        }
    }

    /// The on-screen rectangle a note is painted into — the single source of
    /// truth for both painting and hit-testing, so the two cannot drift.
    fn note_rect(&self, note: &Note) -> egui::Rect {
        let x = self.x_of(note.start as f32);
        let w = (note.length as f32 * self.ticks_to_x).max(3.0);
        let y = self.y_of(note.note);
        egui::Rect::from_min_max(
            egui::pos2(x, y + 1.0),
            egui::pos2(x + w, y + self.row_h - 1.0),
        )
    }

    /// Left / right pixel edges of a note's drawn rectangle. Edge trims are
    /// measured against these — the *visual* edges, without slop.
    fn note_edges(&self, note: &Note) -> (f32, f32) {
        let x = self.x_of(note.start as f32);
        (x, x + (note.length as f32 * self.ticks_to_x).max(3.0))
    }

    /// The hit-testing rectangle: the drawn one plus [`HIT_SLOP`]. Kept
    /// separate from [`Self::note_rect`] so painting and the edge zones stay
    /// exact — only *landing on* a note is forgiving.
    fn note_hit_rect(&self, note: &Note) -> egui::Rect {
        self.note_rect(note).expand2(HIT_SLOP)
    }

    /// The index of the note whose hit-testing rectangle covers `pos`.
    ///
    /// Hit-testing mirrors **drawing**: the same pixels, not a tick range. A
    /// tick-range test (`start <= tick < end`) disagrees with the drawn width
    /// in two ways that both read as bugs to the user — the minimum 3 px width
    /// makes short notes look wider than their ticks, and the half-open range
    /// excludes the right edge itself, so grabbing a note's tail lands on
    /// empty space and paints a *new* note there. The slop keeps the last few
    /// pixels of aim error on the note instead of falling through to the
    /// draw-on-empty gesture.
    fn note_at(&self, clip: &MidiClip, pos: egui::Pos2) -> Option<usize> {
        clip.notes()
            .iter()
            .position(|n| self.note_hit_rect(n).contains(pos))
    }
}

/// The absolute tick range the roll may scroll within (P2/P3, spec
/// `roll-multiclip.md` D2). It is the whole context — focus plus every ghost
/// clip — so scrolling can leave the focus and look at a neighbour. With one
/// clip it is exactly that clip.
#[derive(Debug, Clone, Copy)]
struct ViewWindow {
    start: f32,
    end: f32,
}

impl ViewWindow {
    /// The absolute tick range the whole roll context occupies (P3, spec
    /// `roll-multiclip.md` D2/D3): the earliest visible clip start to the
    /// latest `start + loop`. A stale clip reference (deleted underneath the
    /// editor) is skipped; with no live clip the window collapses to zero at
    /// the focus origin, which the callers never reach because `show` bails
    /// out first.
    fn of(tracks: &[MidiTrack], subject: &RollSubject) -> Self {
        let mut start = f32::INFINITY;
        let mut end = f32::NEG_INFINITY;
        for ctx in subject.context() {
            let Some(clip) = track_clip(tracks, *ctx) else {
                continue;
            };
            let origin = clip.start_ticks as f32;
            let span = clip.loop_ticks.max(TICKS_PER_STEP) as f32;
            start = start.min(origin);
            end = end.max(origin + span);
        }
        if start.is_finite() && end.is_finite() {
            Self { start, end }
        } else {
            Self {
                start: 0.0,
                end: 0.0,
            }
        }
    }

    /// Widens the window by one more clip's occupied ticks (P4: an overlay
    /// lane may sit outside every MIDI clip, and the first frame must show it).
    fn union(self, start: f32, end: f32) -> Self {
        Self {
            start: self.start.min(start),
            end: self.end.max(end),
        }
    }

    /// Clamps the left-edge tick so the window stays inside `[start, end]`. A
    /// window wider than the range (zoomed past fit) pins to `start`, which is
    /// the existing "zoom out shows the whole clip, no blank on the left"
    /// behaviour.
    fn clamp_scroll(self, scroll: f32, width: f32, ticks_to_x: f32) -> f32 {
        let max = (self.end - width / ticks_to_x).max(self.start);
        scroll.clamp(self.start, max)
    }
}

/// The clip a [`ClipRef`] points at, or `None` when the document moved on
/// (a clip deleted while the editor was open).
fn track_clip(tracks: &[MidiTrack], clip: ClipRef) -> Option<&MidiClip> {
    match clip.lane {
        RollLane::Midi => tracks.get(clip.track)?.clips.get(clip.clip),
        // A control clip is not a MIDI clip: the roll never asks for one here
        // (the focus is always MIDI; control clips only feed overlay lanes).
        RollLane::Control => None,
    }
}

/// Draw one frame of the roll + velocity lane + overlay curve lanes and return
/// the edit the gesture produced (at most one per frame).
// Nine arguments is the component's public surface (document table, overlay
// lanes, subject, state, transport readout, meter, focus router); bundling the
// scalars into a struct would only move the count, not reduce it.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    tracks: &[MidiTrack],
    lanes: &[OverlayLane<'_>],
    subject: &RollSubject,
    state: &mut RollState,
    playhead_ticks: f32,
    playing: bool,
    bar_ticks: u32,
    router: &mut crate::focus::FocusRouter,
) -> Option<RollIntent> {
    let focus = subject.focus();
    let clip = track_clip(tracks, focus)?;
    let loop_ticks = i64::from(clip.loop_ticks.max(TICKS_PER_STEP)).max(1);
    // P3: the focus clip's origin, and the visible window = the whole context
    // (`roll-multiclip.md` D2/D3). With one clip both degenerate to exactly
    // the P2 behaviour.
    let origin_tick = i64::from(clip.start_ticks);
    // P4: overlay lanes may extend past every MIDI clip, so the view window
    // (and therefore the fit scale) has to cover them too.
    let window = lanes
        .iter()
        .fold(ViewWindow::of(tracks, subject), |window, lane| {
            let start = lane.data.start_ticks as f32;
            let span = lane.data.loop_ticks.max(TICKS_PER_STEP) as f32;
            window.union(start, start + span)
        });
    // `scroll_x` is absolute now, and the state outlives target switches: a
    // clip starting at bar 9 must open on its own left edge, not inherit a
    // scroll borrowed from the previous clip.
    if state.view_origin != Some(origin_tick) {
        state.view_origin = Some(origin_tick);
        state.scroll_x = window.start;
    }
    let grid_ticks = state
        .grid
        .ticks(TICKS_PER_QUARTER, 120.0, 48_000.0, 30.0)
        .unwrap_or(i64::from(TICKS_PER_STEP))
        .max(1);

    // While a modal transform (G/S/T/Shift+D) or the armed knife owns the
    // pointer, the keyboard stays with the roll — the pointer may travel
    // outside the roll without the focus-follow handing the keys to someone
    // else. Same guarantee a real drag gets from the router's any-down
    // freeze; set every frame so it clears itself when the modal ends.
    router.set_focus_hold(state.modal.is_some() || state.knife_armed);

    // Toolbar: grid menu, mode / armed-command chips, hints.
    let mut intent = None;

    ui.horizontal(|ui| {
        let current = state.grid;
        egui::ComboBox::from_id_salt("roll_grid")
            .selected_text(grid_label(&current))
            .width(64.0)
            .show_ui(ui, |ui| {
                for value in GridValue::note_presets() {
                    let grid = GridValue::Note(value);
                    if ui
                        .selectable_label(current == grid, grid_label(&grid))
                        .clicked()
                    {
                        state.grid = grid;
                    }
                }
            });
        // Mode chip (clickable: mouse users never need the keyboard), and
        // the armed one-shot command beside it with its qualifiers.
        let mode_text = match state.mode {
            RollMode::Select => "SELECT",
            RollMode::Draw => "DRAW",
        };
        let mode_color = if state.mode == RollMode::Draw {
            SELECTED
        } else {
            ui.visuals().weak_text_color()
        };
        if ui
            .add(
                egui::Label::new(egui::RichText::new(mode_text).color(mode_color).strong())
                    .sense(egui::Sense::click()),
            )
            .on_hover_text("Persistent mode (d toggles): what a drag does")
            .clicked()
        {
            state.mode = state.mode.toggled();
        }
        // The live modal transform beside the mode chip (with its
        // qualifiers); clicking it cancels. Otherwise, the armed knife.
        let modal_label = state.modal.as_ref().map(Modal::label);
        if let Some(label) = modal_label {
            if ui
                .add(
                    egui::Label::new(
                        egui::RichText::new(label).color(egui::Color32::YELLOW).strong(),
                    )
                    .sense(egui::Sense::click()),
                )
                .on_hover_text("Modal transform — the selection follows the pointer · LMB confirms · RMB/Esc cancels")
                .clicked()
            {
                state.modal = None;
            }
        } else if state.knife_armed
            && ui
                .add(
                    egui::Label::new(
                        egui::RichText::new("K").color(egui::Color32::YELLOW).strong(),
                    )
                    .sense(egui::Sense::click()),
                )
                .on_hover_text("Knife armed — a stroke cuts every note it crosses · Esc cancels")
                .clicked()
        {
            state.knife_armed = false;
        }
        // The hint must never overflow the panel: an overflowing child
        // expands the ui's available rect (egui grows `max_rect` to include
        // every allocated frame), and the roll below would then be laid out
        // wider than the panel itself. Truncating keeps it on one row.
        if !state.had_focus {
            // Keyboard shortcuts are focus-gated; make the invisible gate
            // visible so "the keys do nothing" is diagnosable at a glance.
            ui.add(
                egui::Label::new(
                    egui::RichText::new("no keyboard focus — hover the roll")
                        .color(egui::Color32::YELLOW),
                ),
            );
        }
        ui.add(
            egui::Label::new(
                egui::RichText::new(
                    "click: select · drag: tweak · dbl-click: insert · edges: trim · Ctrl+drag: copy · d: draw · G move · S scale · T stretch (LMB ok · RMB/Esc cancel) · K knife · Del del · RMB: delete · arrows: nudge · wheel: pitch · Shift+wheel: time · Ctrl+wheel: zoom time · Ctrl+Alt+wheel: zoom pitch · middle-drag: pan",
                )
                .weak(),
            )
            .truncate(),
        );
    });

    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, ROLL_HEIGHT),
        egui::Sense::click_and_drag(),
    );
    // The roll owns keyboard focus while hovered (the app-level FocusRouter
    // assigns it before layout); the click itself is handled in the
    // interaction section below (it needs the pointer→tick/pitch helpers
    // defined there). Registration is enough for hover-follow focus; the
    // explicit click is a fallback that also pulls focus out of an inline
    // DragValue edit, which the router is frozen on.
    router.register_keyboard(response.id, response.rect);
    if response.clicked() {
        response.request_focus();
    }
    state.had_focus = response.has_focus();
    state.focus_id = Some(response.id);

    // ---- View navigation: Ctrl+wheel zooms the time axis, Ctrl+Alt+wheel
    // zooms the pitch axis (egui's vertical-only zoom), Shift+wheel scrolls
    // the time axis, the plain wheel scrolls pitch, middle-drag pans both
    // axes. Zoom is anchored on the pointer so the note under it stays put.
    //
    // The fit basis is the **context** span (P3, D3), so a multi-clip subject
    // opens with every ghost on screen; one clip degenerates to its own loop
    // length, which keeps P2's behaviour byte-identical.
    let fit_ticks = (window.end - window.start).max(bar_ticks.max(1) as f32);
    let fit_ticks_to_x = width / fit_ticks.max(1.0);
    let (zoom2, wheel, nav_mods) = ui
        .ctx()
        .input(|i| (i.zoom_delta_2d(), i.smooth_scroll_delta, i.modifiers));
    if response.hovered() {
        let ctrl = nav_mods.ctrl || nav_mods.command;
        if ctrl {
            if let Some(sp) = response.hover_pos() {
                let (zx, zy) = (zoom2.x, zoom2.y);
                // Pure Ctrl gives a uniform factor; Ctrl+Alt gives a
                // vertical-only factor. Treat the time axis as the default
                // (a uniform zoom only ever moves time), and the pitch axis
                // only when the factor is vertical-only.
                let uniform = (zx - zy).abs() < 1e-4;
                if zx != 1.0 {
                    let new_zoom = (state.zoom_x * zx).clamp(ZOOM_X_MIN, ZOOM_X_MAX);
                    let scroll = zoom_scroll_anchor(
                        state.zoom_x,
                        state.scroll_x,
                        new_zoom,
                        fit_ticks_to_x,
                        rect.left(),
                        sp.x,
                    );
                    state.zoom_x = new_zoom;
                    let t_x = fit_ticks_to_x * new_zoom;
                    state.scroll_x = window.clamp_scroll(scroll, width, t_x);
                }
                if zy != 1.0 && !uniform {
                    let new_zoom = (state.zoom_y * zy).clamp(ZOOM_Y_MIN, ZOOM_Y_MAX);
                    let top = zoom_top_note_anchor(
                        state.zoom_y,
                        state.top_note,
                        new_zoom,
                        rect.top(),
                        sp.y,
                    );
                    state.zoom_y = new_zoom;
                    let rows = (ROLL_HEIGHT / (ROW_HEIGHT * new_zoom)).max(1.0);
                    state.top_note = top.clamp(rows - 1.0, 127.0);
                }
                ui.ctx().request_repaint();
            }
        } else if wheel.x != 0.0 {
            let t_x = fit_ticks_to_x * state.zoom_x;
            state.scroll_x = window.clamp_scroll(state.scroll_x - wheel.x / t_x, width, t_x);
            ui.ctx().request_repaint();
        } else if wheel.y != 0.0 {
            let row_h = ROW_HEIGHT * state.zoom_y;
            let rows = (ROLL_HEIGHT / row_h).max(1.0);
            state.top_note = (state.top_note + wheel.y / row_h).clamp(rows - 1.0, 127.0);
            ui.ctx().request_repaint();
        }
    }

    // Middle-drag pans both axes (grab-and-drag scrolling).
    if response.dragged_by(egui::PointerButton::Middle) {
        let d = ui.ctx().input(|i| i.pointer.delta());
        let t_x = fit_ticks_to_x * state.zoom_x;
        let row_h = ROW_HEIGHT * state.zoom_y;
        state.scroll_x = window.clamp_scroll(state.scroll_x - d.x / t_x, width, t_x);
        let rows = (ROLL_HEIGHT / row_h).max(1.0);
        state.top_note = (state.top_note + d.y / row_h).clamp(rows - 1.0, 127.0);
        ui.ctx().request_repaint();
    }

    let ticks_to_x = fit_ticks_to_x * state.zoom_x;
    let row_h = ROW_HEIGHT * state.zoom_y;
    let visible_rows = (ROLL_HEIGHT / row_h).ceil().max(1.0) as i32;
    let top_note = state.top_note.round() as i32;
    let min_note = top_note - visible_rows + 1;
    // Absolute tick 0's screen x (scroll + zoom applied). Every note / grid
    // draw and every hit test derives from this one pair, so painting and
    // hit-testing can never drift apart.
    let geom = RollGeom {
        origin_tick,
        t0_x: rect.left() - state.scroll_x * ticks_to_x,
        ticks_to_x,
        top_note,
        row_h,
        area: rect,
    };
    let painter = ui.painter_at(rect);

    // ---- Pointer → tick/pitch helpers (the keyboard section needs them
    // for the modal grab point; the interaction sections below reuse them).
    // Ticks stay **clip-local**: every command, modal and clamp is expressed
    // in local ticks, so only the geometry knows about the origin.
    let pointer_tick = |pos: egui::Pos2| geom.local_at(pos.x).max(0.0);
    let pointer_note = |pos: egui::Pos2| {
        (top_note as f32 - (pos.y - rect.top()) / row_h)
            .round()
            .clamp(0.0, 127.0) as u8
    };
    let hit_note = |pos: egui::Pos2| geom.note_at(clip, pos);
    // D2: the view may scroll past the focus clip (to show a ghost), but
    // editing stops at its edges — a click out there must not conjure a note
    // clamped to tick 0.
    let over_focus = |pos: egui::Pos2| (0.0..loop_ticks as f32).contains(&geom.local_at(pos.x));
    let snap = |ticks: f32| -> i64 { snap_ticks(ticks, grid_ticks) };
    let modifiers = ui.ctx().input(|i| i.modifiers);
    let additive = modifiers.shift || modifiers.ctrl;

    // Live modal geometry: computed from the pointer's latest position (a
    // modal transform follows the mouse even outside the roll), feeding the
    // ghost preview drawn below and the confirming click's final intent.
    let modal_frame = state.modal.as_ref().map(|modal| {
        let pos = ui
            .ctx()
            .input(|i| i.pointer.latest_pos())
            .unwrap_or_else(|| rect.center());
        modal.frame(
            clip,
            &state.selection,
            pointer_tick(pos),
            pointer_note(pos),
            grid_ticks,
            loop_ticks,
        )
    });

    // 1. Row background (black keys darker).
    for note in min_note..=top_note {
        if !(0..=127).contains(&note) {
            continue;
        }
        let y = rect.top() + (top_note - note) as f32 * row_h;
        let color = if is_black_key(note) {
            egui::Color32::from_gray(26)
        } else {
            egui::Color32::from_gray(40)
        };
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(rect.left(), y), egui::vec2(width, row_h)),
            0.0,
            color,
        );
    }

    // 2. Vertical grid, re-anchored on every bar line (M2.1): lines are
    //    counted from each bar start, so they stay aligned with the bars
    //    whatever the meter does to a bar's length. `bar_ticks` is the
    //    document's meter (U3a), not the 4/4 constant.
    let bar_ticks = i64::from(bar_ticks.max(1));
    let bars = (loop_ticks + bar_ticks - 1) / bar_ticks;
    for bar in 0..bars {
        let bar_start = bar * bar_ticks;
        let bar_end = (bar_start + bar_ticks).min(loop_ticks);
        let mut tick = bar_start;
        while tick < bar_end {
            let is_bar_line = tick % bar_ticks == 0;
            let x = geom.x_of(tick as f32);
            let stroke = egui::Stroke::new(
                if is_bar_line { 1.5 } else { 1.0 },
                if is_bar_line {
                    egui::Color32::from_gray(95)
                } else {
                    egui::Color32::from_gray(60)
                },
            );
            painter.line_segment(
                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                stroke,
            );
            tick += grid_ticks;
        }
    }

    // 2b. Ghost clips (P3, D6): the rest of the context, painted *behind* the
    //     focus so the writable clip always reads as the foreground. Paint
    //     only — ghosts never enter `note_at`, the marquee, the knife or any
    //     drag, which is what keeps "empty-space drag = marquee" unambiguous.
    for ctx in subject.context() {
        if *ctx == focus {
            continue;
        }
        let Some(ghost_clip) = track_clip(tracks, *ctx) else {
            continue;
        };
        let ghost_geom = geom.at_origin(i64::from(ghost_clip.start_ticks));
        let ghost_loop = i64::from(ghost_clip.loop_ticks.max(TICKS_PER_STEP)).max(1);
        for note in ghost_clip.notes() {
            // Past a shortened span: kept in the data, never drawn (same rule
            // as the focus clip).
            if note.start >= ghost_loop {
                continue;
            }
            let note_i = i32::from(note.note);
            if !(min_note..=top_note).contains(&note_i) {
                continue;
            }
            painter.rect_filled(ghost_geom.note_rect(note), 2.0, GHOST_NOTE);
        }
    }

    // 3. Note blocks, shaded by velocity (M2.5); selected notes in the accent color.
    // During a non-identity modal the selected originals are faded out so the
    // ghost on top stays readable where the two overlap (Blender convention:
    // origin dimmed, transform preview solid).
    let modal_active = modal_frame.as_ref().is_some_and(|f| !f.is_identity());
    for (index, note) in clip.notes().iter().enumerate() {
        // Notes past a shortened span stay in the data but are never drawn.
        if note.start >= loop_ticks {
            continue;
        }
        let note_i = i32::from(note.note);
        if !(min_note..=top_note).contains(&note_i) {
            continue;
        }
        let selected = state.selection.contains(&index);
        let dragging = state.drag.is_some() && selected;
        let base = if dragging {
            egui::Color32::from_rgb(120, 200, 140)
        } else if selected {
            SELECTED
        } else {
            egui::Color32::from_rgb(64, 160, 96)
        };
        let k = 0.45 + 0.55 * f32::from(note.velocity) / 127.0;
        let fill = egui::Color32::from_rgb(
            (base.r() as f32 * k) as u8,
            (base.g() as f32 * k) as u8,
            (base.b() as f32 * k) as u8,
        );
        let fill = if modal_active && selected {
            egui::Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), 60)
        } else {
            fill
        };
        painter.rect_filled(geom.note_rect(note), 2.0, fill);
    }

    // 3b. Modal ghosts: the selection at its transformed geometry. Preview
    // only — nothing is applied until the confirming click, so cancelling
    // costs nothing.
    if let Some(frame) = &modal_frame
        && !frame.is_identity()
    {
        for index in &state.selection {
            let Some(note) = clip.notes().get(*index) else {
                continue;
            };
            let ghost = modal_ghost(note, frame);
            painter.rect_filled(
                geom.note_rect(&ghost),
                2.0,
                egui::Color32::from_rgba_unmultiplied(240, 190, 80, 170),
            );
        }
    }

    // 4. Rubber band.
    if let Some(marquee) = &state.marquee {
        let band = egui::Rect::from_two_pos(marquee.origin, marquee.current);
        painter.rect_filled(
            band,
            0.0,
            egui::Color32::from_rgba_unmultiplied(80, 140, 220, 40),
        );
        painter.rect_stroke(
            band,
            0.0,
            egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 140, 220)),
            egui::StrokeKind::Inside,
        );
    }

    // 4b. Knife stroke trail (the cuts themselves land on release).
    if let Some(knife) = &state.knife
        && knife.path.len() > 1
    {
        painter.add(egui::Shape::line(
            knife.path.clone(),
            egui::Stroke::new(1.5, egui::Color32::from_rgb(240, 190, 80)),
        ));
    }

    // 5. Playhead: the transport position wrapped into the clip's content.
    //    The clip loops from its own origin, so the wrapped position is the
    //    one that actually plays there.
    if playing {
        let pos = playhead_ticks.rem_euclid(loop_ticks as f32);
        let x = geom.x_of(pos);
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(2.0, egui::Color32::YELLOW),
        );
    }

    // ---- Velocity lane (M2.4): bars under the roll, drag to set. ----
    let (lane_rect, lane_response) =
        ui.allocate_exact_size(egui::vec2(width, LANE_HEIGHT), egui::Sense::drag());
    // Same time axis, its own strip: `over` moves only the left edge and the
    // vertical anchor, so the bars line up with the notes above them.
    let lane_geom = geom.over(lane_rect);
    let lane_painter = ui.painter_at(lane_rect);
    lane_painter.rect_filled(lane_rect, 0.0, egui::Color32::from_gray(20));
    for (index, note) in clip.notes().iter().enumerate() {
        let x = lane_geom.x_of(note.start as f32);
        let w = (note.length as f32 * ticks_to_x).clamp(3.0, 8.0);
        let h = (LANE_HEIGHT - 4.0) * f32::from(note.velocity) / 127.0;
        let selected = state.selection.contains(&index);
        let color = if selected {
            SELECTED
        } else {
            egui::Color32::from_rgb(64, 160, 96)
        };
        lane_painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x, lane_rect.bottom() - h),
                egui::pos2(x + w, lane_rect.bottom()),
            ),
            1.0,
            color,
        );
    }
    if playing {
        let pos = playhead_ticks.rem_euclid(loop_ticks as f32);
        let x = lane_geom.x_of(pos);
        lane_painter.line_segment(
            [
                egui::pos2(x, lane_rect.top()),
                egui::pos2(x, lane_rect.bottom()),
            ],
            egui::Stroke::new(2.0, egui::Color32::YELLOW),
        );
    }

    // ---- Overlay curve lanes (P4, D8/D16): one strip per control clip in
    // the context, sharing the roll's time axis and carrying its own value
    // axis. They are writable and write **their own** clip, never the focus.
    let lane_intent = curve_lanes(ui, width, &geom, lanes, state);

    // ---- Keyboard: mode + the modal command family (M4a). ----
    // Gated on focus so the keys never leak into text fields elsewhere.
    // Axis qualifiers also apply mid-transform (Blender convention); the
    // rest of the family is ignored while a modal or a stroke owns the
    // pointer.
    if response.has_focus() {
        // Claim the keys egui's focus machinery would otherwise steal:
        // Escape makes a focused widget *surrender* focus before any
        // widget is laid out (cleared in `Memory::begin_pass`), and the
        // arrows move focus instead of reaching the widget (the roll
        // nudges with them). The filter is read next frame, which is fine
        // — focus persists until something else takes it.
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    escape: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    ..Default::default()
                },
            );
        });
        // Every handled key is *consumed*, so it cannot bubble into egui's
        // focus machinery or other widgets (an unconsumed press is what
        // produces the system beep on some platforms). `consume_key`
        // matches modifiers logically (extra Shift/Alt ignored), so the
        // more specific Shift+D must be consumed before the plain `d`.
        let keys = ui.ctx().input_mut(|i| Keys {
            shift_d: i.consume_key(egui::Modifiers::SHIFT, egui::Key::D),
            escape: i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
            d: i.consume_key(egui::Modifiers::NONE, egui::Key::D),
            g: i.consume_key(egui::Modifiers::NONE, egui::Key::G),
            s: i.consume_key(egui::Modifiers::NONE, egui::Key::S),
            t: i.consume_key(egui::Modifiers::NONE, egui::Key::T),
            k: i.consume_key(egui::Modifiers::NONE, egui::Key::K),
            x: i.consume_key(egui::Modifiers::NONE, egui::Key::X),
            y: i.consume_key(egui::Modifiers::NONE, egui::Key::Y),
            delete: i.consume_key(egui::Modifiers::NONE, egui::Key::Delete),
        });
        if keys.x {
            state.apply_axis(Axis::Time);
        }
        if keys.y {
            state.apply_axis(Axis::Pitch);
        }
        // The modal grabs from wherever the pointer is right now.
        let arm_pos = ui
            .ctx()
            .input(|i| i.pointer.latest_pos())
            .or_else(|| response.hover_pos())
            .unwrap_or_else(|| response.rect.center());
        if keys.escape {
            state.escape();
        } else if let Some(modal) = &mut state.modal {
            // A modal transform owns the keyboard: only the qualifiers
            // answer (`s` flips a Stretch to its start edge), everything
            // else is ignored — the command family never chains.
            if keys.s && matches!(modal.kind, ModalKind::Stretch { .. }) {
                if let ModalKind::Stretch { lo, hi, .. } = modal.kind {
                    modal.kind = ModalKind::Stretch {
                        edge: StretchEdge::Start,
                        lo,
                        hi,
                    };
                }
            }
        } else if state.knife.is_some() || state.drag.is_some() {
            // A knife stroke or a tweak drag owns the pointer; the family
            // never chains.
        } else {
            if keys.shift_d {
                if state.selection.is_empty() {
                    state.note_status("Duplicate: nothing selected");
                } else {
                    // Shift+D: copy the selection, then move the copies in
                    // a modal Grab. The duplicate command and the confirming
                    // move share one gesture id — one undo step.
                    let gesture = state.next_gesture();
                    intent = Some(RollIntent::Duplicate {
                        indices: state.selection.clone(),
                        gesture,
                    });
                    state.modal = Some(Modal {
                        kind: ModalKind::Move { axis: None },
                        grab_tick: pointer_tick(arm_pos),
                        grab_note: pointer_note(arm_pos),
                        gesture,
                    });
                }
            } else if keys.d {
                // A state command cancels the armed knife first, then
                // toggles the mode.
                state.knife_armed = false;
                state.mode = state.mode.toggled();
            } else if keys.g {
                if state.selection.is_empty() {
                    state.note_status("Grab: nothing selected");
                } else {
                    state.modal = Some(Modal {
                        kind: ModalKind::Move { axis: None },
                        grab_tick: pointer_tick(arm_pos),
                        grab_note: pointer_note(arm_pos),
                        gesture: state.next_gesture(),
                    });
                }
            } else if keys.s {
                match selection_bbox(clip, &state.selection) {
                    Some((lo, hi)) => {
                        let grab_tick = pointer_tick(arm_pos);
                        state.modal = Some(Modal {
                            kind: ModalKind::Scale {
                                // The pivot is the bbox *start*: scaling from
                                // the start keeps the notes' relative layout
                                // intact (nothing sits left of it).
                                pivot_tick: lo,
                                grab_dist: (grab_tick - lo as f32).abs().max(1.0),
                                max_k: scale_max_k(lo, hi, loop_ticks),
                            },
                            grab_tick,
                            grab_note: pointer_note(arm_pos),
                            gesture: state.next_gesture(),
                        });
                    }
                    None => state.note_status("Scale: nothing selected"),
                }
            } else if keys.t {
                match selection_bbox(clip, &state.selection) {
                    Some((lo, hi)) => {
                        let grab_tick = pointer_tick(arm_pos);
                        state.modal = Some(Modal {
                            kind: ModalKind::Stretch {
                                edge: StretchEdge::End,
                                lo,
                                hi,
                            },
                            grab_tick,
                            grab_note: pointer_note(arm_pos),
                            gesture: state.next_gesture(),
                        });
                    }
                    None => state.note_status("Stretch: nothing selected"),
                }
            } else if keys.k {
                state.knife_armed = true;
            } else if keys.delete && !state.selection.is_empty() {
                intent = Some(RollIntent::DeleteNotes {
                    indices: state.selection.clone(),
                });
            }
        }
    }

    // ---- Interaction → intents. ----
    // (The pointer helpers live above the keyboard section, which needs
    // them for the modal grab point.)

    // A plain click (no drag): a modal transform consumes it as the
    // *confirmation* (LMB anywhere fixes the geometry); an armed knife
    // consumes it as a zero-length stroke (one cut through every note
    // spanning the pointer tick, no pre-selection needed); otherwise it
    // selects the note under the pointer, or on empty space deselects —
    // or inserts a default note on the second click of a double-click.
    // Creation deliberately requires a drag (draw mode) or a double-click,
    // so a stray click meant as "clear the selection" never paints a note.
    // (egui only reports drag_started once the pointer moved, so click and
    // drag are already disambiguated.)
    if response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(modal) = state.modal.take() {
                let frame = modal.frame(
                    clip,
                    &state.selection,
                    pointer_tick(pos),
                    pointer_note(pos),
                    grid_ticks,
                    loop_ticks,
                );
                intent = modal.finalize(clip, &state.selection, frame, loop_ticks);
            } else if state.knife_armed {
                let tick = pointer_tick(pos).round() as i64;
                let cuts: Vec<(usize, i64)> = clip
                    .notes()
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| n.start < tick && tick < n.end())
                    .map(|(index, _)| (index, tick))
                    .collect();
                if cuts.is_empty() {
                    state.note_status("Cut: nothing under the knife");
                } else {
                    intent = Some(RollIntent::SplitNotes {
                        cuts,
                        gesture: state.next_gesture(),
                    });
                }
                state.knife_armed = false;
            } else {
                match hit_note(pos) {
                    Some(index) => {
                        if additive {
                            if let Some(at) = state.selection.iter().position(|i| *i == index) {
                                state.selection.remove(at);
                            } else {
                                state.selection.push(index);
                            }
                        } else {
                            state.selection = vec![index];
                        }
                    }
                    None if additive => {} // modifier click on empty: leave the selection alone
                    None if !over_focus(pos) => {
                        // D2: outside the focus clip — explain the no-op
                        // instead of silently doing nothing.
                        state.selection.clear();
                        if response.double_clicked() {
                            state.note_status(
                                "Outside the clip being edited — switch focus from the strip above",
                            );
                        }
                    }
                    None => {
                        if response.double_clicked() {
                            // No awaiting_create: a follow-up drag would have
                            // fired drag_started (draw) instead of a click.
                            intent = Some(RollIntent::AddNote {
                                note: pointer_note(pos),
                                start: snap(pointer_tick(pos)).min(loop_ticks - 1),
                                length: DEFAULT_NOTE_LENGTH,
                                gesture: state.next_gesture(),
                            });
                        } else {
                            state.selection.clear();
                        }
                    }
                }
            }
        }
    }

    // Arrow-key nudging (M2.3) — only while the roll holds focus. The keys
    // are *consumed*: with the focus lock filter above they cannot move
    // focus away from the roll, and consuming them keeps egui's built-in
    // arrow navigation from firing at all, so nudging works continuously.
    // (consume_key matches modifiers logically — Shift+arrow still reads
    // as the bare arrow here, with `modifiers` supplying the octave.)
    if response.has_focus() {
        let (down, up, left, right, shift) = ui.ctx().input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight),
                i.modifiers.shift,
            )
        });
        let delta_note: i8 = if down {
            -(if shift { 12 } else { 1 })
        } else if up {
            if shift { 12 } else { 1 }
        } else {
            0
        };
        let delta_start: i64 = if left {
            -grid_ticks
        } else if right {
            grid_ticks
        } else {
            0
        };
        if (delta_note != 0 || delta_start != 0) && !state.selection.is_empty() {
            intent = Some(clamped_move(
                clip,
                &state.selection,
                0,
                delta_note,
                loop_ticks,
                None,
            ));
        }
    }

    // Cursor affordances: a modal transform shows what it does (move /
    // resize); hovering near a note's start/end turns the pointer into a
    // horizontal resize cursor; draw mode and the armed knife show a
    // crosshair (precision aiming).
    if let Some(modal) = &state.modal {
        ui.ctx().set_cursor_icon(match modal.kind {
            ModalKind::Move { .. } => egui::CursorIcon::Move,
            ModalKind::Stretch { .. } | ModalKind::Scale { .. } => {
                egui::CursorIcon::ResizeHorizontal
            }
        });
    } else if response.hovered() && state.drag.is_none() {
        if let Some(pos) = response.hover_pos() {
            if state.knife_armed {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            } else if let Some(index) = hit_note(pos) {
                let note = &clip.notes()[index];
                let (x0, x1) = geom.note_edges(note);
                if edge_zone(pos.x, x0, x1).is_some() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
            } else if state.mode == RollMode::Draw {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            }
        }
    }

    // Left-button gestures only: the middle button owns panning, and egui's
    // unfiltered `drag_started`/`dragged`/`drag_stopped` fire for ANY button
    // (the patch hit the same trap and filters by button everywhere).
    if response.drag_started_by(egui::PointerButton::Primary) {
        // Belt-and-suspenders: hover-follow focus (the router) already covers
        // this, but a drag on the roll must never run without focus.
        response.request_focus();
        // Classify the grab by where the press *landed*, never by where the
        // pointer is on this frame. egui only reports `drag_started` once
        // the pointer has travelled past `max_click_dist` (6 pt), so
        // `interact_pointer_pos()` here is already 6+ pt past the press:
        // a grab that began on a note's edge reads as a body grab (→ move),
        // and one that began just past the tail as empty space (→ draw a
        // new note). `press_origin` is the press point itself.
        let press = ui
            .ctx()
            .input(|i| i.pointer.press_origin())
            .or_else(|| response.interact_pointer_pos());
        if let Some(pos) = press {
            if state.modal.is_some() {
                // A modal transform follows the pointer by itself; a drag
                // during one is ignored — LMB confirms, RMB/Esc cancels.
            } else if state.knife_armed {
                // Knife stroke start: cuts gather along the swept path and
                // land as one batch on release.
                state.knife = Some(KnifeStroke {
                    last: pos,
                    gesture: state.next_gesture(),
                    cuts: Vec::new(),
                    path: vec![pos],
                });
            } else {
                // No modal, no knife: classify by the press point × mode.
                // Edge-trim affordance is checked first: within EDGE_PX of a
                // note's start/end the grab trims that edge instead of moving.
                let edge = hit_note(pos).and_then(|index| {
                    let note = &clip.notes()[index];
                    let (x0, x1) = geom.note_edges(note);
                    edge_zone(pos.x, x0, x1)
                });
                match hit_note(pos) {
                    Some(index) => {
                        let grabbed = clip.notes()[index];
                        // Ctrl + drag (no edge): duplicate the selection — or the
                        // grabbed note alone — and drag the copies. A plain
                        // Ctrl+click still toggles the selection (that is a
                        // click, handled above; a drag never is).
                        if modifiers.ctrl && edge.is_none() {
                            if !state.selection.contains(&index) {
                                state.selection = vec![index];
                            }
                            let gesture = state.next_gesture();
                            intent = Some(RollIntent::Duplicate {
                                indices: state.selection.clone(),
                                gesture,
                            });
                            state.drag = Some(DragSelection::anchored(
                                &grabbed,
                                pointer_tick(pos),
                                pointer_note(pos),
                                DragKind::Move { axis: None },
                                gesture,
                            ));
                        } else if let Some(kind) = edge {
                            // Edge grab on an unselected note selects just it; on
                            // a selected note it trims the whole selection.
                            if !state.selection.contains(&index) {
                                state.selection = vec![index];
                            }
                            state.drag = Some(DragSelection::anchored(
                                &grabbed,
                                pointer_tick(pos),
                                pointer_note(pos),
                                kind,
                                state.next_gesture(),
                            ));
                        } else {
                            // Plain grab: move. Shift toggles the grabbed note
                            // into/out of the selection first.
                            if modifiers.shift {
                                if let Some(at) = state.selection.iter().position(|i| *i == index) {
                                    state.selection.remove(at);
                                } else {
                                    state.selection.push(index);
                                }
                            } else if !state.selection.contains(&index) {
                                state.selection = vec![index];
                            }
                            if !state.selection.is_empty() {
                                state.drag = Some(DragSelection::anchored(
                                    &grabbed,
                                    pointer_tick(pos),
                                    pointer_note(pos),
                                    DragKind::Move { axis: None },
                                    state.next_gesture(),
                                ));
                            }
                        }
                    }
                    None => {
                        if !over_focus(pos) && !additive {
                            // D2: drawing stops at the focus clip's edges. A
                            // marquee would be pointless out here (ghosts are
                            // not selectable), so a plain drag is a no-op.
                            state.note_status(
                                "Outside the clip being edited — switch focus from the strip above",
                            );
                        } else if additive {
                            // Shift/Ctrl + drag on empty space: additive
                            // rubber band.
                            state.marquee = Some(Marquee {
                                origin: pos,
                                current: pos,
                                additive: true,
                            });
                        } else if state.mode == RollMode::Draw {
                            // Draw mode: press-drag on empty space creates the
                            // note right away (note on at the press point) and
                            // draws its length until release (note off at the
                            // pointer). The note's sorted index is unknown
                            // until the command reports back, so the gesture
                            // parks in `awaiting_create` and `resolve_created`
                            // starts the draw-drag.
                            let gesture = state.next_gesture();
                            let start = snap(pointer_tick(pos)).min(loop_ticks - 1);
                            intent = Some(RollIntent::AddNote {
                                note: pointer_note(pos),
                                start,
                                length: DEFAULT_NOTE_LENGTH,
                                gesture,
                            });
                            state.awaiting_create = Some(AwaitCreate {
                                grab_tick: pointer_tick(pos),
                                grab_note: pointer_note(pos),
                                gesture,
                            });
                        } else {
                            // Select mode: a plain empty drag is a marquee that
                            // replaces the selection. (M3.8's "empty drag
                            // always draws" boundary rule is abolished — a
                            // stray drag must never paint a note.)
                            state.marquee = Some(Marquee {
                                origin: pos,
                                current: pos,
                                additive: false,
                            });
                        }
                    }
                }
            }
        }
    }

    // One intent per frame: a frame that already spent its slot on the
    // gesture's *setup* intent (Ctrl-drag's Duplicate, draw-mode's AddNote,
    // Shift+D's copy) must not also emit the drag's first increment, or the
    // setup edit is silently dropped and the copy-drag degrades into a
    // plain move. The increment lands on the next frame instead (a real
    // drag has many; the grab-relative delta is computed from the press
    // point, so nothing is lost).
    if response.dragged_by(egui::PointerButton::Primary)
        && !(response.drag_started_by(egui::PointerButton::Primary) && intent.is_some())
    {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(knife) = &mut state.knife {
                // Knife stroke: the frame's segment cuts every note it
                // crosses, where it crosses. Cuts only gather here — they
                // land as one batch on release (see `drag_stopped`).
                let from = knife.last;
                knife.last = pos;
                knife.path.push(pos);
                for (index, note) in clip.notes().iter().enumerate() {
                    if knife.cuts.iter().any(|(cut, _)| *cut == index) {
                        continue; // one cut per note per stroke
                    }
                    if let Some(tick) = knife_crossing(from, pos, note, &geom) {
                        knife.cuts.push((index, tick));
                    }
                }
            } else if let Some(drag) = &mut state.drag {
                // Every branch below computes the *absolute* target the
                // gesture is asking for, then emits only the increment
                // still owed (`target - applied`). Commands apply relative
                // deltas, so emitting the whole grab-relative offset every
                // frame would compound (1 + 2 + 3 + …) and the note would
                // run away from the pointer instead of tracking it.
                match drag.kind {
                    DragKind::Move { axis } => {
                        // Uniform move of the whole selection. The delta is
                        // clamped on the UI side so no note hits the engine's
                        // clamp asymmetrically (that would permute the sort
                        // and invalidate the selection).
                        let want_start = snap(pointer_tick(pos)) - snap(drag.grab_tick);
                        let want_note = i64::from(pointer_note(pos)) - i64::from(drag.grab_note);
                        // An axis lock zeroes that axis' target: locking
                        // mid-drag walks it back to the grab position (the
                        // increment below cancels whatever was applied).
                        let (want_start, want_note) = match axis {
                            Some(Axis::Time) => (want_start, 0),
                            Some(Axis::Pitch) => (0, want_note),
                            None => (want_start, want_note),
                        };
                        let delta_start = want_start - drag.applied_start;
                        let delta_note = want_note - drag.applied_note;
                        if delta_start != 0 || delta_note != 0 {
                            let step = clamped_move(
                                clip,
                                &state.selection,
                                delta_start,
                                delta_note
                                    .clamp(i16::from(i8::MIN) as i64, i16::from(i8::MAX) as i64)
                                    as i8,
                                loop_ticks,
                                Some(drag.gesture),
                            );
                            // Accumulate what was *actually* applied (the
                            // clamp may have swallowed part of it), so the
                            // next frame's increment stays exact.
                            if let RollIntent::MoveNotes {
                                delta_start,
                                delta_note,
                                ..
                            } = &step
                            {
                                drag.applied_start += delta_start;
                                drag.applied_note += i64::from(*delta_note);
                            }
                            intent = Some(step);
                        }
                    }
                    DragKind::ResizeEnd => {
                        // Drawing / right-edge trim: the grabbed note's end
                        // follows the pointer (snapped, at least one grid
                        // cell, never past the loop); the whole selection
                        // stretches by the same delta. The start is the
                        // frozen anchor — a right-edge trim never moves it.
                        let start = drag.current_start();
                        let target = draw_end(start, pointer_tick(pos), grid_ticks, loop_ticks);
                        let delta = target - drag.current_end();
                        if delta != 0 {
                            drag.applied_end += delta;
                            intent = Some(RollIntent::ResizeEnd {
                                delta_ticks: delta,
                                gesture: drag.gesture,
                            });
                        }
                    }
                    DragKind::ResizeStart => {
                        // Left-edge trim: the grabbed note's start follows the
                        // pointer (snapped, never past its own end); the whole
                        // selection shifts its starts by the same delta. The
                        // end stays put — the command mirrors the length.
                        let end = drag.current_end();
                        let target = trim_start(end, pointer_tick(pos), grid_ticks);
                        let delta = target - drag.current_start();
                        if delta != 0 {
                            drag.applied_start += delta;
                            intent = Some(RollIntent::ResizeStart {
                                delta_ticks: delta,
                                gesture: drag.gesture,
                            });
                        }
                    }
                }
            } else if let Some(marquee) = &mut state.marquee {
                marquee.current = pos;
            }
        }
    }

    if response.drag_stopped_by(egui::PointerButton::Primary) {
        // A finished marquee selects what it covers.
        if let Some(marquee) = state.marquee.take() {
            let band = egui::Rect::from_two_pos(marquee.origin, marquee.current);
            let hits: Vec<usize> = clip
                .notes()
                .iter()
                .enumerate()
                .filter(|(_, n)| band.intersects(geom.note_rect(n)))
                .map(|(i, _)| i)
                .collect();
            if marquee.additive {
                for index in hits {
                    if !state.selection.contains(&index) {
                        state.selection.push(index);
                    }
                }
            } else {
                state.selection = hits;
            }
        }
        state.drag = None;
        // A knife stroke ends here: the gathered cuts land as one batch
        // (one undo step). One stroke per arming — a stroke that crossed
        // nothing just disarms.
        if let Some(knife) = state.knife.take() {
            state.knife_armed = false;
            if !knife.cuts.is_empty() {
                intent = Some(RollIntent::SplitNotes {
                    cuts: knife.cuts,
                    gesture: knife.gesture,
                });
            }
        }
        // A press-draw that never actually moved (or whose note creation
        // failed) leaves a parked grab point; drop it so no drag starts
        // from a stale gesture.
        state.awaiting_create = None;
    }

    if response.secondary_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if state.modal.take().is_some() || state.knife.take().is_some() || state.knife_armed {
                // RMB cancels a modal transform or the knife (Blender), and
                // never deletes while one is running.
                state.knife_armed = false;
            } else if let Some(index) = hit_note(pos) {
                let indices = if state.selection.contains(&index) {
                    state.selection.clone()
                } else {
                    vec![index]
                };
                intent = Some(RollIntent::DeleteNotes { indices });
            }
        }
    }

    // Velocity lane: drag sets absolute velocities. With a selection, every
    // selected note follows the pointer; without one, the notes under the
    // pointer do (a sweep across the lane "paints" velocities).
    if lane_response.dragged_by(egui::PointerButton::Primary)
        || lane_response.drag_started_by(egui::PointerButton::Primary)
    {
        if let Some(pos) = lane_response.interact_pointer_pos() {
            let velocity = ((lane_rect.bottom() - pos.y) / (LANE_HEIGHT - 4.0) * 127.0)
                .round()
                .clamp(1.0, 127.0) as u8;
            let updates: Vec<(usize, u8)> = if state.selection.is_empty() {
                let tick = lane_geom.local_at(pos.x).max(0.0) as i64;
                clip.notes()
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| tick >= n.start && tick < n.end())
                    .map(|(i, _)| (i, velocity))
                    .collect()
            } else {
                state.selection.iter().map(|i| (*i, velocity)).collect()
            };
            if !updates.is_empty() {
                intent = Some(RollIntent::SetVelocities { updates });
            }
        }
    }

    // A live modal also confirms / cancels on clicks that land OUTSIDE the
    // roll (Blender semantics: the modal owns the whole input while it runs
    // — by the time the user clicks, the pointer has often travelled past
    // the roll, e.g. chasing the ghost far right of the content). Clicks
    // inside the roll were already consumed by the paths above (which take
    // the modal, so this only fires for the outside case). The confirming
    // frame uses the live pointer position — the same one the ghosts
    // preview, so what you see is exactly what lands.
    if state.modal.is_some() {
        let (lmb, rmb) = ui.ctx().input(|i| {
            (
                i.pointer.button_clicked(egui::PointerButton::Primary),
                i.pointer.button_clicked(egui::PointerButton::Secondary),
            )
        });
        if lmb && let Some(modal) = state.modal.take() {
            let pos = ui
                .ctx()
                .input(|i| i.pointer.latest_pos())
                .unwrap_or_else(|| rect.center());
            let frame = modal.frame(
                clip,
                &state.selection,
                pointer_tick(pos),
                pointer_note(pos),
                grid_ticks,
                loop_ticks,
            );
            intent = modal.finalize(clip, &state.selection, frame, loop_ticks);
        }
        if rmb {
            state.modal = None;
        }
    }

    // A lane edit and a note gesture cannot start in the same frame (one
    // pointer), but the lane is drawn first — let it win if both are set.
    lane_intent.or(intent)
}

/// Draws the overlay curve lanes and returns their edit for this frame.
///
/// Layout: one strip per lane, stacked directly under the velocity lane, all
/// using the roll's time axis (`geom.over`) and the lane's clip origin
/// (`at_origin`). Nothing here touches the note machinery — a lane edit is a
/// curve command on its own control clip (D16).
fn curve_lanes(
    ui: &mut egui::Ui,
    width: f32,
    geom: &RollGeom,
    lanes: &[OverlayLane<'_>],
    state: &mut RollState,
) -> Option<RollIntent> {
    state.lane_rects.clear();
    if lanes.is_empty() {
        // A stale drag index must not survive the lanes disappearing.
        state.curve_drag = None;
        return None;
    }
    let mut intent = None;
    for (index, lane) in lanes.iter().enumerate() {
        let collapsed = state.collapsed_lanes.contains(&lane.clip.track);
        let height = if collapsed {
            CURVE_LANE_COLLAPSED_HEIGHT
        } else {
            CURVE_LANE_HEIGHT
        };
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click_and_drag());
        // Time axis of *this* clip: same pixels per tick, own origin.
        state.lane_rects.push(rect);
        let lane_geom = geom.at_origin(i64::from(lane.data.start_ticks)).over(rect);
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::from_gray(16));
        painter.line_segment(
            [rect.left_top(), rect.right_top()],
            egui::Stroke::new(1.0, egui::Color32::from_gray(52)),
        );
        // Collapse toggle (P5): a 12 px square in the caption. A stack of
        // eight lanes is 512 px of roll the notes do not get, and the usual
        // case is "one curve matters, the rest are scenery".
        let toggle = egui::Rect::from_min_size(
            rect.left_top() + egui::vec2(3.0, 3.0),
            egui::vec2(LANE_TOGGLE, LANE_TOGGLE),
        );
        painter.rect_filled(
            toggle,
            2.0,
            if response.hovered() {
                egui::Color32::from_gray(70)
            } else {
                egui::Color32::from_gray(40)
            },
        );
        painter.text(
            toggle.center(),
            egui::Align2::CENTER_CENTER,
            if collapsed { "+" } else { "-" },
            egui::FontId::monospace(10.0),
            egui::Color32::from_gray(190),
        );
        painter.text(
            rect.left_top() + egui::vec2(20.0, 3.0),
            egui::Align2::LEFT_TOP,
            &lane.label,
            egui::FontId::proportional(10.0),
            egui::Color32::from_gray(150),
        );
        // The toggle is checked before any curve interaction: a click there
        // folds the lane, it does not add an anchor.
        if response.clicked_by(egui::PointerButton::Primary)
            && response
                .interact_pointer_pos()
                .is_some_and(|pos| toggle.contains(pos))
        {
            if collapsed {
                state.collapsed_lanes.retain(|t| *t != lane.clip.track);
            } else {
                state.collapsed_lanes.push(lane.clip.track);
                state.collapsed_lanes.sort_unstable();
            }
            continue;
        }
        if collapsed {
            continue;
        }

        let loop_ticks = i64::from(lane.data.loop_ticks.max(1));
        let span_ticks = i64::from(lane.data.span_ticks.max(1));
        // Only the clip's active span is drawn: outside it the engine plays
        // nothing, so drawing a held value would claim otherwise.
        let x_start = lane_geom.x_of(0.0).max(rect.left());
        let x_end = lane_geom.x_of(span_ticks as f32).min(rect.right());

        // Curve: one sample per pixel, folded into the loop the same way the
        // sequencer tiles control clips (D17). A seam between tiles is a jump
        // and is drawn as one — no smoothing across it.
        let mut points: Vec<egui::Pos2> = Vec::new();
        let mut x = x_start;
        while x <= x_end {
            let local = lane_geom.local_at(x);
            let phase = local.rem_euclid(loop_ticks as f32);
            match lane.data.value_at(f64::from(phase)) {
                Some(value) => points.push(egui::pos2(x, lane.value_to_y(value, rect))),
                // An empty curve has nothing to draw; leave the gap.
                None => {
                    if points.len() > 1 {
                        painter.add(egui::Shape::line(
                            std::mem::take(&mut points),
                            egui::Stroke::new(1.5, CURVE_LINE),
                        ));
                    }
                    points.clear();
                }
            }
            x += 1.0;
        }
        if points.len() > 1 {
            painter.add(egui::Shape::line(
                points,
                egui::Stroke::new(1.5, CURVE_LINE),
            ));
        }

        // Anchors. Only the first loop's anchors are real; the tiled repeats
        // are the same points played again.
        let mut anchor_positions: Vec<(u32, egui::Pos2)> = Vec::new();
        for anchor in &lane.data.curve {
            let pos = egui::pos2(
                lane_geom.x_of(anchor.tick as f32),
                lane.value_to_y(anchor.value, rect),
            );
            if pos.x < rect.left() - 4.0 || pos.x > rect.right() + 4.0 {
                continue;
            }
            anchor_positions.push((anchor.tick, pos));
            painter.circle_filled(pos, 3.0, ANCHOR);
        }

        // ---- Interaction: click adds, drag moves, right-click deletes ----
        let snap = crate::curve::SNAP_TICKS.max(1);
        let mapped = |pos: egui::Pos2| -> (u32, f32) {
            let local = lane_geom.local_at(pos.x).max(0.0);
            let tick = ((local / snap as f32).round() as u32) * snap;
            (
                tick.min(loop_ticks as u32 - 1),
                lane.y_to_value(pos.y, rect),
            )
        };
        let hit_anchor = |pos: egui::Pos2| -> Option<u32> {
            anchor_positions
                .iter()
                .find(|(_, p)| (*p - pos).length() <= ANCHOR_RADIUS)
                .map(|(tick, _)| *tick)
        };
        let pointer = response.interact_pointer_pos();
        // A drag is classified by the **press** point (the same rule the note
        // gestures follow): by the time egui reports `drag_started` the
        // pointer has already moved, and grabbing the anchor under the moved
        // pointer would let go of the one the user actually pressed on.
        let press_pos = ui.ctx().input(|i| i.pointer.press_origin()).or(pointer);
        let dragging = state.curve_drag.is_some_and(|(i, _)| i == index);
        if response.drag_started_by(egui::PointerButton::Primary)
            && let Some(pos) = press_pos
        {
            let (tick, value) = mapped(pos);
            let from = hit_anchor(pos);
            state.curve_drag = Some((index, from));
            intent = Some(RollIntent::CurveUpsert {
                track: lane.clip.track,
                clip: lane.clip.clip,
                from,
                tick,
                value,
                dragging: true,
            });
        } else if dragging
            && response.dragged_by(egui::PointerButton::Primary)
            && let Some(pos) = pointer
        {
            let (tick, value) = mapped(pos);
            let from = state.curve_drag.and_then(|(_, from)| from);
            // The anchor now lives at `tick`; the next frame moves *that* one.
            state.curve_drag = Some((index, Some(tick)));
            intent = Some(RollIntent::CurveUpsert {
                track: lane.clip.track,
                clip: lane.clip.clip,
                from,
                tick,
                value,
                dragging: true,
            });
        } else if response.drag_stopped_by(egui::PointerButton::Primary) && dragging {
            state.curve_drag = None;
            intent = Some(RollIntent::CurveDragEnd);
        } else if response.clicked_by(egui::PointerButton::Primary)
            && let Some(pos) = pointer
        {
            let (tick, value) = mapped(pos);
            intent = Some(RollIntent::CurveUpsert {
                track: lane.clip.track,
                clip: lane.clip.clip,
                from: None,
                tick,
                value,
                dragging: false,
            });
        } else if response.secondary_clicked()
            && let Some(pos) = pointer
        {
            let (tick, _) = mapped(pos);
            intent = Some(RollIntent::CurveRemove {
                track: lane.clip.track,
                clip: lane.clip.clip,
                tick,
            });
        }
    }
    intent
}

/// Snap a tick position to the grid (nearest line, never negative).
fn snap_ticks(ticks: f32, grid_ticks: i64) -> i64 {
    ((ticks / grid_ticks as f32).round() as i64).max(0) * grid_ticks
}

/// Time-axis zoom anchored at the pointer: the tick under `anchor_x` stays
/// under it after the zoom (unclamped new scroll; the caller clamps).
fn zoom_scroll_anchor(
    old_zoom: f32,
    old_scroll: f32,
    new_zoom: f32,
    fit: f32,
    rect_left: f32,
    anchor_x: f32,
) -> f32 {
    let old_px = fit * old_zoom;
    let t0 = rect_left - old_scroll * old_px;
    let tick = ((anchor_x - t0) / old_px).max(0.0);
    let new_px = fit * new_zoom;
    (rect_left - (anchor_x - tick * new_px)) / new_px
}

/// Pitch-axis zoom anchored at the pointer: the note under `anchor_y`
/// stays under it (unclamped new top note; the caller clamps).
fn zoom_top_note_anchor(
    old_zoom: f32,
    old_top: f32,
    new_zoom: f32,
    rect_top: f32,
    anchor_y: f32,
) -> f32 {
    let old_h = ROW_HEIGHT * old_zoom;
    let note = old_top - (anchor_y - rect_top) / old_h;
    let new_h = ROW_HEIGHT * new_zoom;
    note + (anchor_y - rect_top) / new_h
}

/// End tick of a note being drawn: the pointer snapped to the grid, at least
/// one grid cell after the start (a zero-length note is invisible and cannot
/// be grabbed), and never past the loop end.
fn draw_end(start: i64, pointer_tick: f32, grid_ticks: i64, loop_ticks: i64) -> i64 {
    let snapped = snap_ticks(pointer_tick, grid_ticks);
    snapped
        .max(start + grid_ticks)
        .min(loop_ticks.max(start + 1))
}

/// Start tick of a left-edge trim: the pointer snapped to the grid, never
/// past the note's own end (min length one tick), never negative. `end` is
/// the grabbed note's *current* end — the trim's fixed boundary.
fn trim_start(end: i64, pointer_tick: f32, grid_ticks: i64) -> i64 {
    snap_ticks(pointer_tick, grid_ticks).min(end - 1).max(0)
}

/// Which edge of a note (in pixels) the pointer is on, if either — a grab
/// there trims that edge instead of moving the note.
///
/// The two zones never overlap: a note narrower than `2 * EDGE_PX` is all
/// edge, and resolves to the edge the pointer is *closer* to. Without that
/// tie-break a wide-ish grab on a short note would pick whichever branch
/// is tested first, so trimming the head would sometimes stretch the tail
/// instead.
fn edge_zone(x: f32, left: f32, right: f32) -> Option<DragKind> {
    let d_start = (x - left).abs();
    let d_end = (x - right).abs();
    if d_start <= EDGE_PX && d_start <= d_end {
        Some(DragKind::ResizeStart)
    } else if d_end <= EDGE_PX {
        Some(DragKind::ResizeEnd)
    } else {
        None
    }
}

/// Where a knife stroke segment crosses `note`, if it does: the tick at
/// the segment's crossing of the note's row centreline (a slash through
/// the row), or — for a segment travelling along the note's own row — the
/// middle of the overlap with the note's drawn span. Unsnapped: cutting is
/// a precision operation, not a grid one. `None` when the segment never
/// enters the note (including when it merely touches the note's boundary,
/// which would cut off a zero-length half).
///
/// Returns a **clip-local** tick, like every other note-level helper.
fn knife_crossing(from: egui::Pos2, to: egui::Pos2, note: &Note, geom: &RollGeom) -> Option<i64> {
    let row_top = geom.y_of(note.note);
    let centre = row_top + geom.row_h / 2.0;
    let (x_left, x_right) = geom.note_edges(note);
    let cut_at = |x: f32| -> Option<i64> {
        let tick = geom.local_at(x).round() as i64;
        (tick > note.start && tick < note.end()).then_some(tick)
    };

    let (ya, yb) = (from.y, to.y);
    if ya != yb && (ya.min(yb)..=ya.max(yb)).contains(&centre) {
        // A slash crossing the row: where the segment meets the centreline.
        let t = (centre - ya) / (yb - ya);
        let x = from.x + (to.x - from.x) * t;
        return (x_left..x_right).contains(&x).then_some(x).and_then(cut_at);
    }
    // Sweeping along the note's own row: cut in the middle of the span the
    // segment covered.
    let band = row_top..=(row_top + geom.row_h);
    if band.contains(&ya) && band.contains(&yb) {
        let lo = from.x.min(to.x).max(x_left);
        let hi = from.x.max(to.x).min(x_right);
        if hi > lo {
            return cut_at((lo + hi) / 2.0);
        }
    }
    None
}

/// Start / end of the selection's bounding box — the Stretch command's
/// anchor and the Scale command's pivot source. `None` for an empty or
/// stale selection.
fn selection_bbox(clip: &MidiClip, selection: &[usize]) -> Option<(i64, i64)> {
    let mut lo = i64::MAX;
    let mut hi = i64::MIN;
    for index in selection {
        let note = clip.notes().get(*index)?;
        lo = lo.min(note.start);
        hi = hi.max(note.end());
    }
    (lo <= hi).then_some((lo, hi))
}

/// Largest scale factor that keeps the scaled bbox inside the loop (the
/// pivot is the bbox *start*; nothing sits left of it, so there is no
/// left-side ceiling — the command's per-note clamps are all the floor the
/// gesture needs).
fn scale_max_k(lo: i64, hi: i64, loop_ticks: i64) -> f32 {
    if hi > lo {
        (loop_ticks - lo) as f32 / (hi - lo) as f32
    } else {
        f32::INFINITY
    }
}

/// The ghost of a note under one modal frame: the geometry the preview
/// draws (and the confirming click applies). Mirrors the command's per-note
/// clamps so the preview never promises more than the document will do.
fn modal_ghost(note: &Note, frame: &ModalFrame) -> Note {
    let mut ghost = *note;
    if let Some((edge, delta)) = frame.edge
        && delta != 0
    {
        match edge {
            StretchEdge::End => ghost.length = (ghost.length + delta).max(1),
            StretchEdge::Start => {
                let end = ghost.end();
                ghost.start = (ghost.start + delta).clamp(0, end - 1);
                ghost.length = end - ghost.start;
            }
        }
    }
    if (frame.scale_k - 1.0).abs() > f32::EPSILON {
        let pivot = frame.pivot_tick as f32;
        ghost.start = (pivot + (ghost.start as f32 - pivot) * frame.scale_k)
            .round()
            .max(0.0) as i64;
        ghost.length = ((ghost.length as f32 * frame.scale_k).round() as i64).max(1);
    }
    if frame.delta_start != 0 || frame.delta_note != 0 {
        ghost.start = (ghost.start + frame.delta_start).max(0);
        ghost.note = (i32::from(ghost.note) + i32::from(frame.delta_note)).clamp(0, 127) as u8;
    }
    ghost
}

/// A uniform move clamped on the UI side so no note crosses the engine's
/// clamp boundary (asymmetric clamping would permute the sort order).
/// Crossings against *unselected* notes can still happen; the command
/// reports the moved notes' new indices and the caller re-selects them.
fn clamped_move(
    clip: &MidiClip,
    indices: &[usize],
    raw_start_delta: i64,
    raw_note_delta: i8,
    loop_ticks: i64,
    gesture: Option<u64>,
) -> RollIntent {
    let (delta_start, delta_note) = clamped_move_deltas(
        clip,
        indices,
        raw_start_delta,
        i64::from(raw_note_delta),
        loop_ticks,
    );
    RollIntent::MoveNotes {
        indices: indices.to_vec(),
        delta_start,
        delta_note,
        gesture,
    }
}

/// The clamp half of [`clamped_move`], shared with the modal preview (a
/// ghost must never promise a position the confirming intent would clamp
/// away).
fn clamped_move_deltas(
    clip: &MidiClip,
    indices: &[usize],
    raw_start_delta: i64,
    raw_note_delta: i64,
    loop_ticks: i64,
) -> (i64, i8) {
    let notes = clip.notes();
    let mut delta_start = raw_start_delta;
    let mut delta_note = raw_note_delta;
    for index in indices {
        if let Some(note) = notes.get(*index) {
            delta_start = delta_start.max(-note.start);
            if delta_note < 0 {
                delta_note = delta_note.max(-i64::from(note.note));
            } else {
                delta_note = delta_note.min(i64::from(127 - note.note));
            }
        }
    }
    // A clip-relative move cannot push a note past the loop either.
    for index in indices {
        if let Some(note) = notes.get(*index) {
            delta_start = delta_start.min(loop_ticks - 1 - note.start);
        }
    }
    (
        delta_start,
        delta_note.clamp(i64::from(i8::MIN), i64::from(i8::MAX)) as i8,
    )
}

fn grid_label(grid: &GridValue) -> String {
    match grid {
        GridValue::Note(value) => value.label(),
        GridValue::Seconds(seconds) => format!("{seconds:.3}s"),
        GridValue::Frames(frames) => format!("{frames}f"),
        GridValue::Samples(samples) => format!("{samples}smp"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AwaitCreate, Axis, ClipRef, DragKind, DragSelection, EDGE_PX, HIT_SLOP, MAX_ROLL_CONTEXT,
        Modal, ModalFrame, ModalKind, ROLL_HEIGHT, ROW_HEIGHT, RollGeom, RollIntent, RollMode,
        RollState, RollSubject, StretchEdge, ViewWindow, clamped_move, draw_end, edge_zone,
        is_black_key, knife_crossing, modal_ghost, scale_max_k, selection_bbox, snap_ticks,
        trim_start, zoom_scroll_anchor, zoom_top_note_anchor,
    };
    use crate::grid::{GridValue, NoteValue};
    use eframe::egui;
    use glitcherz_model::{MidiClip, MidiTrack, Note, TICKS_PER_STEP};

    fn note(start: i64, length: i64, note: u8) -> Note {
        Note {
            start,
            length,
            note,
            ..Note::default()
        }
    }

    #[test]
    fn black_key_detection() {
        assert!(!is_black_key(60)); // C4
        assert!(is_black_key(61)); // C#4
        assert!(is_black_key(63)); // D#4
        assert!(!is_black_key(64)); // E4
        assert!(is_black_key(70)); // A#4
        assert!(!is_black_key(0));
        assert!(is_black_key(-4)); // -4 → pitch class 8 (G#), and rem_euclid does not panic
    }

    #[test]
    fn roll_state_defaults() {
        let state = RollState::new();
        assert!(
            (state.top_note - 84.0).abs() < 1e-6,
            "initial view top is C6"
        );
        assert!(state.selection.is_empty());
        assert!(!state.is_dragging());
        // Default grid is a straight 1/16.
        assert_eq!(
            state.grid,
            GridValue::Note(NoteValue {
                division: 16,
                dotted: false,
                triplet: false
            })
        );
        // View navigation starts at the identity: fit-to-width time axis,
        // base row height, no scroll. (A derived `Default` would give 0.0
        // zoom and collapse the roll — the patch hit the same trap.)
        assert_eq!(state.zoom_x, 1.0);
        assert_eq!(state.zoom_y, 1.0);
        assert_eq!(state.scroll_x, 0.0);
    }

    /// The zoom anchor: the tick under the pointer stays under the pointer.
    #[test]
    fn time_zoom_keeps_the_pointer_tick_in_place() {
        // 400 px roll, 960-tick loop → fit scale 400/960 px per tick.
        let fit = 400.0 / 960.0;
        let rect_left = 100.0;
        // Zoomed 4× and scrolled half the loop across: tick 0 sits at
        // 100 - 480*fit*4 < 0 (scrolled past), the visible window starts
        // around tick 240.
        let (zoom, scroll) = (4.0f32, 240.0f32);
        let anchor_x = 300.0; // mid-roll
        let old_tick = {
            let px = fit * zoom;
            let t0 = rect_left - scroll * px;
            (anchor_x - t0) / px
        };
        let new_zoom = zoom * 2.0;
        let new_scroll = zoom_scroll_anchor(zoom, scroll, new_zoom, fit, rect_left, anchor_x);
        let new_px = fit * new_zoom;
        let new_t0 = rect_left - new_scroll * new_px;
        let new_tick = (anchor_x - new_t0) / new_px;
        assert!(
            (new_tick - old_tick).abs() < 1e-3,
            "anchor tick {old_tick} drifted to {new_tick}"
        );
    }

    /// Zooming out past fit clamps to scroll 0 — the view shows the whole
    /// loop instead of scrolling into blank space on the left.
    #[test]
    fn time_zoom_out_past_fit_pins_scroll_to_zero() {
        let fit = 400.0 / 960.0;
        let rect_left = 100.0;
        // Anchor at the right edge, zoom from 4× down to 0.5× (past fit).
        let scroll = zoom_scroll_anchor(4.0, 0.0, 0.5, fit, rect_left, 500.0);
        assert!(scroll <= 0.0, "scroll must clamp to 0, got {scroll}");
    }

    /// The pitch zoom anchor: the note under the pointer stays under it.
    #[test]
    fn pitch_zoom_keeps_the_pointer_note_in_place() {
        let rect_top = 50.0;
        let anchor_y = 150.0; // 100 px below the top
        let (zoom, top) = (2.0f32, 84.0f32);
        let old_h = ROW_HEIGHT * zoom;
        let note = top - (anchor_y - rect_top) / old_h;
        let new_zoom = 4.0;
        let new_top = zoom_top_note_anchor(zoom, top, new_zoom, rect_top, anchor_y);
        let new_h = ROW_HEIGHT * new_zoom;
        let new_note = new_top - (anchor_y - rect_top) / new_h;
        assert!(
            (new_note - note).abs() < 1e-3,
            "anchor note {note} drifted to {new_note}"
        );
    }

    #[test]
    fn clamped_move_keeps_notes_inside_the_clip_and_pitch_range() {
        let mut clip = MidiClip::default();
        clip.sequence.notes = vec![note(0, 240, 0), note(480, 240, 127), note(240, 240, 60)];
        let loop_ticks = 960;

        // Dragging everything left cannot pull the first note below 0...
        let intent = clamped_move(&clip, &[0, 1, 2], -480, 0, loop_ticks, None);
        let RollIntent::MoveNotes { delta_start, .. } = intent else {
            panic!("expected a move");
        };
        assert_eq!(delta_start, 0, "the note at 0 blocks the whole move");

        // ...or right past the loop end.
        let intent = clamped_move(&clip, &[0, 1, 2], 480, 0, loop_ticks, None);
        let RollIntent::MoveNotes { delta_start, .. } = intent else {
            panic!("expected a move");
        };
        assert_eq!(delta_start, 960 - 1 - 480, "limited by the latest note");

        // Pitch: the 0-note blocks downward, the 127-note blocks upward.
        let intent = clamped_move(&clip, &[0, 1, 2], 0, -5, loop_ticks, None);
        let RollIntent::MoveNotes { delta_note, .. } = intent else {
            panic!("expected a move");
        };
        assert_eq!(delta_note, 0);
        let intent = clamped_move(&clip, &[0, 1, 2], 0, 5, loop_ticks, None);
        let RollIntent::MoveNotes { delta_note, .. } = intent else {
            panic!("expected a move");
        };
        assert_eq!(delta_note, 0);
    }

    #[test]
    fn uniform_move_preserves_relative_order_within_the_selection() {
        // Shifting every note by the same delta keeps their relative order
        // (stable sort, equal key shift). Crossings against *unselected*
        // notes can still permute the global order — the command's returned
        // indices cover that case; this pins the within-selection invariant.
        let mut notes: Vec<Note> = (0..8)
            .map(|i| {
                note(
                    i * TICKS_PER_STEP as i64,
                    TICKS_PER_STEP as i64,
                    60 + i as u8,
                )
            })
            .collect();
        let before: Vec<(i64, i64, u8)> =
            notes.iter().map(|n| (n.start, n.length, n.note)).collect();
        for n in &mut notes {
            n.start += 480;
        }
        notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
        let after: Vec<(i64, i64, u8)> =
            notes.iter().map(|n| (n.start, n.length, n.note)).collect();
        assert_eq!(
            before.iter().map(|(s, _, _)| s + 480).collect::<Vec<_>>(),
            after.iter().map(|(s, _, _)| *s).collect::<Vec<_>>()
        );
    }

    #[test]
    fn lane_height_leaves_room_for_full_scale_bars() {
        assert_eq!(ROLL_HEIGHT, ROW_HEIGHT * 32.0);
    }

    #[test]
    fn snap_ticks_rounds_to_the_nearest_grid_line() {
        assert_eq!(snap_ticks(0.0, 240), 0);
        assert_eq!(snap_ticks(100.0, 240), 0);
        assert_eq!(snap_ticks(120.0, 240), 240); // exactly half → rounds away from zero
        assert_eq!(snap_ticks(359.0, 240), 240);
        assert_eq!(snap_ticks(360.0, 240), 480);
        assert_eq!(snap_ticks(-50.0, 240), 0); // negative positions clamp to 0
    }

    #[test]
    fn draw_end_follows_the_pointer_snapped_to_the_grid() {
        let grid = TICKS_PER_STEP as i64; // 1/16
        assert_eq!(draw_end(0, 240.0, grid, 3840), 240, "one cell right");
        assert_eq!(draw_end(0, 300.0, grid, 3840), 240, "rounds down");
        assert_eq!(draw_end(0, 360.0, grid, 3840), 480, "rounds up");
        assert_eq!(draw_end(480, 700.0, grid, 3840), 720, "start-relative");
    }

    #[test]
    fn draw_end_is_never_shorter_than_one_grid_cell() {
        let grid = TICKS_PER_STEP as i64;
        // Dragging left of the press point (note on) still leaves a visible
        // note — a zero-length note would be invisible and ungrabbable.
        assert_eq!(draw_end(960, 0.0, grid, 3840), 960 + grid);
        assert_eq!(draw_end(960, 1000.0, grid, 3840), 960 + grid);
    }

    #[test]
    fn draw_end_stays_inside_the_loop() {
        let grid = TICKS_PER_STEP as i64;
        assert_eq!(draw_end(0, 999_999.0, grid, 3840), 3840);
        // Even a note starting in the last grid cell keeps a positive length.
        assert_eq!(draw_end(3840 - grid, 999_999.0, grid, 3840), 3840);
    }

    #[test]
    fn resolve_created_starts_a_draw_drag_not_a_move() {
        let mut state = RollState::new();
        state.awaiting_create = Some(AwaitCreate {
            grab_tick: 12.0,
            grab_note: 60,
            gesture: 7,
        });
        state.resolve_created(3, 0, TICKS_PER_STEP as i64);
        assert_eq!(state.selection, vec![3]);
        assert!(state.is_dragging());
        let drag = state.drag.as_ref().expect("a drag started");
        assert_eq!(
            drag.kind,
            DragKind::ResizeEnd,
            "a fresh note is drawn (end follows the pointer), not moved"
        );
        assert_eq!(drag.gesture, 7, "the draw keeps the creating gesture id");
        assert_eq!(
            (drag.anchor_start, drag.anchor_end),
            (0, TICKS_PER_STEP as i64),
            "the draw anchors on the note as it was created"
        );
    }

    #[test]
    fn resolve_created_without_a_gesture_only_selects() {
        // A plain click's AddNote parks nothing, so no drag must start.
        let mut state = RollState::new();
        state.resolve_created(5, 240, 240);
        assert_eq!(state.selection, vec![5]);
        assert!(!state.is_dragging());
    }

    #[test]
    fn trim_start_snaps_and_never_crosses_the_note_end() {
        // The grabbed note ends at 720 (480 + 240).
        // Pointer well before the start: snaps to the grid line at 240.
        assert_eq!(trim_start(720, 300.0, 240), 240);
        // Pointer past the end: clamped to end - 1 (min length one tick).
        assert_eq!(trim_start(720, 900.0, 240), 719);
        // Pointer before zero: floored at 0.
        assert_eq!(trim_start(720, -50.0, 240), 0);
    }

    // ---- Drag anchoring / increment (the "note flies away" regressions) ----

    /// A drag's anchor is frozen at grab time, so a later re-sort of the
    /// clip cannot make the gesture track a different note.
    #[test]
    fn a_drag_anchors_on_the_note_as_it_was_at_grab_time() {
        let subject = note(480, 240, 60);
        let drag = DragSelection::anchored(&subject, 500.0, 60, DragKind::ResizeEnd, 3);
        assert_eq!(drag.anchor_start, 480);
        assert_eq!(drag.anchor_end, 720);
        assert_eq!(drag.current_start(), 480);
        assert_eq!(drag.current_end(), 720);
    }

    /// The move bug: each frame used to re-emit the grab-relative total,
    /// so the applied deltas compounded (1 + 2 + 3 + …) and the note ran
    /// away from the pointer. The increment owed must shrink to zero as the
    /// gesture catches up with the pointer.
    #[test]
    fn a_move_emits_only_the_increment_still_owed() {
        let grid = TICKS_PER_STEP as i64;
        let mut drag = DragSelection::anchored(
            &note(0, grid, 60),
            0.0,
            60,
            DragKind::Move { axis: None },
            1,
        );
        // The pointer walks one grid cell per frame; the note must land
        // exactly under it, so the steps must sum to the displacement.
        let mut sum = 0i64;
        for cell in 1..=5 {
            let want = cell * grid;
            let step = want - drag.applied_start;
            assert!(step > 0, "each frame still owes a positive increment");
            drag.applied_start += step;
            sum += step;
        }
        assert_eq!(
            sum,
            5 * grid,
            "the applied deltas sum to the pointer displacement, not 1+2+…+5"
        );
        // Once the pointer stops, nothing more is owed.
        assert_eq!(5 * grid - drag.applied_start, 0);
    }

    /// Same regression for the end trim: the frozen anchor plus the
    /// accumulated increments must reproduce the pointer target exactly.
    #[test]
    fn an_end_trim_tracks_the_pointer_instead_of_running_away() {
        let grid = TICKS_PER_STEP as i64;
        let mut drag = DragSelection::anchored(&note(0, grid, 60), 0.0, 60, DragKind::ResizeEnd, 1);
        for cell in 1..=5 {
            let pointer = cell as f32 * grid as f32;
            let target = draw_end(drag.current_start(), pointer, grid, 3840);
            let delta = target - drag.current_end();
            drag.applied_end += delta;
        }
        assert_eq!(
            drag.current_end(),
            5 * grid,
            "the note's end sits under the pointer after five frames"
        );
        // A sixth frame at the same spot owes nothing.
        let target = draw_end(drag.current_start(), 5.0 * grid as f32, grid, 3840);
        assert_eq!(target - drag.current_end(), 0);
    }

    /// A left-edge trim keeps the end pinned and moves only the start, so
    /// the boundary it clamps against never drifts.
    #[test]
    fn a_start_trim_pins_the_end_and_moves_only_the_start() {
        let grid = TICKS_PER_STEP as i64;
        let mut drag = DragSelection::anchored(
            &note(4 * grid, grid, 60),
            (4 * grid) as f32,
            60,
            DragKind::ResizeStart,
            1,
        );
        let end = drag.current_end();
        for cell in [3, 2, 1, 0] {
            let target = trim_start(drag.current_end(), (cell * grid) as f32, grid);
            drag.applied_start += target - drag.current_start();
            assert_eq!(drag.current_end(), end, "a left trim never moves the end");
        }
        assert_eq!(drag.current_start(), 0);
        // Pulling past the end clamps to end - 1 rather than inverting.
        let target = trim_start(drag.current_end(), 9999.0, grid);
        assert_eq!(target, end - 1);
    }

    #[test]
    fn edge_zone_only_fires_near_the_note_edges() {
        assert_eq!(edge_zone(3.0, 0.0, 100.0), Some(DragKind::ResizeStart));
        assert_eq!(edge_zone(97.0, 0.0, 100.0), Some(DragKind::ResizeEnd));
        assert_eq!(edge_zone(50.0, 0.0, 100.0), None, "the middle is a move");
        assert_eq!(
            edge_zone(EDGE_PX + 0.5, 0.0, 100.0),
            None,
            "just outside the zone is a move"
        );
    }

    /// The head/tail grabs the user reported as broken. A note narrow
    /// enough that both zones cover it must resolve to the *nearer* edge,
    /// otherwise trimming the head stretches the tail.
    #[test]
    fn edge_zone_picks_the_nearer_edge_when_the_zones_overlap() {
        // 10 px wide with EDGE_PX = 8: the zones overlap across the middle.
        assert_eq!(EDGE_PX * 2.0, 16.0, "the fixture assumes 8 px zones");
        // Grabbing at the left edge trims the start...
        assert_eq!(edge_zone(1.0, 0.0, 10.0), Some(DragKind::ResizeStart));
        // ...and at the right edge trims the end.
        assert_eq!(edge_zone(9.0, 0.0, 10.0), Some(DragKind::ResizeEnd));
        // The exact middle is equidistant; it must still resolve to one of
        // them rather than falling through to a move.
        let middle = edge_zone(5.0, 0.0, 10.0);
        assert!(
            matches!(
                middle,
                Some(DragKind::ResizeStart) | Some(DragKind::ResizeEnd)
            ),
            "the middle of a short note resolves to an edge, got {middle:?}"
        );
        // A note wide enough to have a body keeps its middle as a move.
        assert_eq!(edge_zone(50.0, 0.0, 200.0), None);
    }

    /// The tail grab used to land on empty space and paint a *new* note:
    /// the old tick-range hit test (`start <= tick < end`) excluded the
    /// right edge that the 3 px minimum width had just drawn there.
    /// Test geometry: tick 0 sits at the strip's left edge, so a clip-local
    /// tick equals the number of pixels once `ticks_to_x` is 1.
    fn geom(rect: egui::Rect, origin: i64, ticks_to_x: f32, top: i32) -> RollGeom {
        RollGeom {
            origin_tick: origin,
            t0_x: rect.left() - origin as f32 * ticks_to_x,
            ticks_to_x,
            top_note: top,
            row_h: ROW_HEIGHT,
            area: rect,
        }
    }

    #[test]
    fn a_note_hit_test_matches_the_rectangle_it_is_drawn_into() {
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 200.0), egui::vec2(400.0, 200.0));
        let mut clip = MidiClip::default();
        // start 0, length 240; ticks_to_x = 1.0 → drawn 100.0 .. 340.0.
        clip.sequence.notes = vec![note(0, 240, 60)];
        let g = geom(rect, 0, 1.0, 60);

        let drawn = g.note_rect(&clip.notes()[0]);
        assert_eq!(drawn.left(), 100.0);
        assert_eq!(drawn.right(), 340.0);

        // Anywhere inside the drawn rectangle hits, including both edges.
        for x in [drawn.left(), drawn.center().x, drawn.right() - 0.5] {
            let pos = egui::pos2(x, drawn.center().y);
            assert_eq!(
                g.note_at(&clip, pos),
                Some(0),
                "x={x} is inside the drawn rect and must hit"
            );
        }
        // HIT_SLOP keeps an off-by-a-few-pixels tail grab on the note
        // instead of letting it fall through to "draw a new note".
        assert_eq!(
            g.note_at(&clip, egui::pos2(drawn.right() + 1.0, drawn.center().y)),
            Some(0),
            "a press a pixel past the tail still hits (aim error is not empty space)"
        );
        // Past the slack it really is empty space.
        assert_eq!(
            g.note_at(
                &clip,
                egui::pos2(drawn.right() + HIT_SLOP.x + 0.5, drawn.center().y)
            ),
            None
        );
        // A neighbouring pitch row does not hit either (the y slack only
        // fills this note's own row gutter).
        assert_eq!(
            g.note_at(
                &clip,
                egui::pos2(drawn.center().x, drawn.center().y + ROW_HEIGHT)
            ),
            None
        );
    }

    /// A short note is drawn at the 3 px minimum width, so a tick-range
    /// test would place its tail short of where the user sees it.
    #[test]
    fn a_short_note_is_hit_across_its_full_drawn_width() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 200.0));
        let mut clip = MidiClip::default();
        clip.sequence.notes = vec![note(0, 1, 60)];
        // ticks_to_x = 0.1 → one tick is 0.1 px, floored to the 3 px minimum.
        let g = geom(rect, 0, 0.1, 60);
        let drawn = g.note_rect(&clip.notes()[0]);
        assert_eq!(drawn.width(), 3.0, "the minimum width is what gets drawn");
        for x in [drawn.left() + 0.5, drawn.right() - 0.5] {
            assert_eq!(
                g.note_at(&clip, egui::pos2(x, drawn.center().y)),
                Some(0),
                "x={x} within the drawn 3 px must hit the note"
            );
        }
    }

    #[test]
    fn resolve_created_starts_a_draw_drag_with_the_gesture_id() {
        let mut state = RollState::new();
        state.awaiting_create = Some(AwaitCreate {
            grab_tick: 120.0,
            grab_note: 64,
            gesture: 7,
        });
        state.resolve_created(3, 120, 240);
        assert_eq!(state.selection, vec![3]);
        let drag = state.drag.as_ref().expect("draw drag should start");
        assert_eq!(drag.kind, DragKind::ResizeEnd);
        assert_eq!(drag.gesture, 7);
        // The drag anchors on the geometry, not on an index: the clip has
        // just been re-sorted by note_add, so index 3 may name anything.
        assert_eq!(drag.anchor_start, 120);
        assert_eq!(drag.anchor_end, 360);
    }

    // ---- M4 state machine: mode, the modal command family, qualifiers ----

    fn move_modal(grab_tick: f32, grab_note: u8) -> Modal {
        Modal {
            kind: ModalKind::Move { axis: None },
            grab_tick,
            grab_note,
            gesture: 1,
        }
    }

    #[test]
    fn roll_state_starts_in_select_mode_with_nothing_running() {
        let mut state = RollState::new();
        assert_eq!(state.mode, RollMode::Select);
        assert!(!state.modal_active());
        assert!(!state.knife_armed);
        assert_eq!(state.take_status(), None);
    }

    #[test]
    fn escape_cancels_the_modal_then_the_knife_then_draw_then_clears() {
        let mut state = RollState::new();
        state.selection = vec![0];
        state.mode = RollMode::Draw;
        state.modal = Some(move_modal(120.0, 60));

        // 1. The modal goes first — cancelling costs nothing (a preview
        //    never touched the document), so the selection survives.
        state.escape();
        assert!(!state.modal_active());
        assert_eq!(state.mode, RollMode::Draw);
        assert_eq!(state.selection, vec![0]);

        // 2. Then the armed knife disarms.
        state.knife_armed = true;
        state.escape();
        assert!(!state.knife_armed);
        assert_eq!(state.mode, RollMode::Draw);

        // 3. Then Draw mode exits.
        state.escape();
        assert_eq!(state.mode, RollMode::Select);

        // 4. Then the selection clears.
        state.escape();
        assert!(state.selection.is_empty());
    }

    #[test]
    fn modal_labels_carry_their_qualifiers() {
        assert_eq!(move_modal(0.0, 60).label(), "G");
        assert_eq!(
            Modal {
                kind: ModalKind::Move {
                    axis: Some(Axis::Time)
                },
                ..move_modal(0.0, 60)
            }
            .label(),
            "G·X"
        );
        assert_eq!(
            Modal {
                kind: ModalKind::Move {
                    axis: Some(Axis::Pitch)
                },
                ..move_modal(0.0, 60)
            }
            .label(),
            "G·Y"
        );
        assert_eq!(
            Modal {
                kind: ModalKind::Scale {
                    pivot_tick: 0,
                    grab_dist: 1.0,
                    max_k: 1.0
                },
                ..move_modal(0.0, 60)
            }
            .label(),
            "S"
        );
        assert_eq!(
            Modal {
                kind: ModalKind::Stretch {
                    edge: StretchEdge::End,
                    lo: 0,
                    hi: 0
                },
                ..move_modal(0.0, 60)
            }
            .label(),
            "T"
        );
        assert_eq!(
            Modal {
                kind: ModalKind::Stretch {
                    edge: StretchEdge::Start,
                    lo: 0,
                    hi: 0
                },
                ..move_modal(0.0, 60)
            }
            .label(),
            "T·S"
        );
    }

    #[test]
    fn axis_qualifiers_toggle_on_the_modal_move_and_the_running_drag() {
        let mut state = RollState::new();
        state.modal = Some(move_modal(120.0, 60));

        state.apply_axis(Axis::Time);
        let modal = state.modal.as_ref().expect("modal alive");
        assert_eq!(
            modal.kind,
            ModalKind::Move {
                axis: Some(Axis::Time)
            }
        );
        // The same key again releases the lock...
        state.apply_axis(Axis::Time);
        let modal = state.modal.as_ref().expect("modal alive");
        assert_eq!(modal.kind, ModalKind::Move { axis: None });
        // ...and the other axis takes over.
        state.apply_axis(Axis::Pitch);
        let modal = state.modal.as_ref().expect("modal alive");
        assert_eq!(
            modal.kind,
            ModalKind::Move {
                axis: Some(Axis::Pitch)
            }
        );

        // Mid-tweak-drag: the lock lands on the running move (X/Y also
        // works without any modal).
        state.modal = None;
        state.drag = Some(DragSelection::anchored(
            &note(0, 240, 60),
            0.0,
            60,
            DragKind::Move { axis: None },
            1,
        ));
        state.apply_axis(Axis::Time);
        assert_eq!(
            state.drag.as_ref().map(|drag| drag.kind),
            Some(DragKind::Move {
                axis: Some(Axis::Time)
            })
        );
        state.apply_axis(Axis::Time);
        assert_eq!(
            state.drag.as_ref().map(|drag| drag.kind),
            Some(DragKind::Move { axis: None })
        );
    }

    #[test]
    fn a_modal_move_finalizes_with_the_clamped_total() {
        let mut clip = MidiClip::default();
        clip.sequence.notes = vec![note(0, 240, 60)];
        clip.loop_ticks = 3840;
        let modal = move_modal(240.0, 60);

        // The pointer walked one grid cell right and four semitones up.
        let frame = modal.frame(&clip, &[0], 480.0, 64, 240, 3840);
        assert_eq!((frame.delta_start, frame.delta_note), (240, 4));
        let intent = modal
            .finalize(&clip, &[0], frame, 3840)
            .expect("a real move");
        assert!(matches!(
            intent,
            RollIntent::MoveNotes {
                delta_start: 240,
                delta_note: 4,
                gesture: Some(1),
                ..
            }
        ));

        // A pointer parked on the grab point confirms nothing: a no-op
        // must not spend an undo step.
        let frame = modal.frame(&clip, &[0], 240.0, 60, 240, 3840);
        assert_eq!(modal.finalize(&clip, &[0], frame, 3840), None);

        // The move clamps against the loop end (snap(4000) = 4080 → a
        // want of 3840 ticks, clamped to 3839).
        let frame = modal.frame(&clip, &[0], 4000.0, 60, 240, 3840);
        let intent = modal.finalize(&clip, &[0], frame, 3840).expect("clamps");
        match intent {
            RollIntent::MoveNotes { delta_start, .. } => assert_eq!(delta_start, 3839),
            other => panic!("expected a move, got {other:?}"),
        }
    }

    #[test]
    fn a_modal_scale_finalizes_around_the_bbox_start() {
        // Selection bbox [0..720]; the pivot is the bbox *start* (0), so
        // scaling keeps the notes' relative layout intact. Grabbed 360
        // ticks right of the pivot; the pointer twice as far doubles the
        // selection in one final factor.
        let mut clip = MidiClip::default();
        clip.sequence.notes = vec![note(0, 240, 60), note(480, 240, 62)];
        clip.loop_ticks = 3840;
        let modal = Modal {
            kind: ModalKind::Scale {
                pivot_tick: 0,
                grab_dist: 360.0,
                max_k: scale_max_k(0, 720, 3840),
            },
            grab_tick: 360.0,
            grab_note: 60,
            gesture: 3,
        };
        let frame = modal.frame(&clip, &[0, 1], 720.0, 60, 240, 3840);
        assert!((frame.scale_k - 2.0).abs() < 1e-6);
        let intent = modal
            .finalize(&clip, &[0, 1], frame, 3840)
            .expect("a real scale");
        assert!(matches!(
            intent,
            RollIntent::ScaleNotes { pivot_tick: 0, factor, gesture: 3 }
                if (factor - 2.0).abs() < 1e-6
        ));

        // A no-op scale (factor exactly 1) spends no undo step.
        let frame = modal.frame(&clip, &[0, 1], 360.0, 60, 240, 3840);
        assert_eq!(modal.finalize(&clip, &[0, 1], frame, 3840), None);
    }

    #[test]
    fn a_modal_stretch_trims_the_snapped_bbox_edge() {
        let mut clip = MidiClip::default();
        clip.sequence.notes = vec![note(0, 240, 60), note(480, 240, 62)];
        clip.loop_ticks = 3840;
        // bbox [0..720]; bare `t` pulls the end edge.
        let mut modal = Modal {
            kind: ModalKind::Stretch {
                edge: StretchEdge::End,
                lo: 0,
                hi: 720,
            },
            grab_tick: 240.0,
            grab_note: 60,
            gesture: 4,
        };
        let frame = modal.frame(&clip, &[0, 1], 1200.0, 60, 240, 3840);
        assert_eq!(frame.edge, Some((StretchEdge::End, 480)));
        let intent = modal
            .finalize(&clip, &[0, 1], frame, 3840)
            .expect("a real stretch");
        assert!(matches!(
            intent,
            RollIntent::ResizeEnd {
                delta_ticks: 480,
                gesture: 4
            }
        ));

        // `t+s` flips the edge: the start follows the pointer, snapped to
        // the grid, never past the bbox end.
        modal.kind = ModalKind::Stretch {
            edge: StretchEdge::Start,
            lo: 0,
            hi: 720,
        };
        let frame = modal.frame(&clip, &[0, 1], 480.0, 60, 240, 3840);
        assert_eq!(frame.edge, Some((StretchEdge::Start, 480)));
        let intent = modal
            .finalize(&clip, &[0, 1], frame, 3840)
            .expect("a real trim");
        assert!(matches!(
            intent,
            RollIntent::ResizeStart {
                delta_ticks: 480,
                gesture: 4
            }
        ));

        // A pointer at the edge confirms nothing.
        let frame = modal.frame(&clip, &[0, 1], 0.0, 60, 240, 3840);
        assert_eq!(modal.finalize(&clip, &[0, 1], frame, 3840), None);
    }

    #[test]
    fn modal_ghosts_preview_the_transformed_geometry() {
        let frame = ModalFrame {
            delta_start: 240,
            delta_note: 2,
            scale_k: 1.0,
            pivot_tick: 0,
            edge: None,
        };
        let ghost = modal_ghost(&note(0, 240, 60), &frame);
        assert_eq!((ghost.start, ghost.length, ghost.note), (240, 240, 62));

        // Scale around the pivot, start kept at the pivot, length ≥ 1.
        let frame = ModalFrame {
            delta_start: 0,
            delta_note: 0,
            scale_k: 0.5,
            pivot_tick: 480,
            edge: None,
        };
        let ghost = modal_ghost(&note(480, 240, 60), &frame);
        assert_eq!((ghost.start, ghost.length), (480, 120));

        // A start-edge trim keeps the end pinned.
        let frame = ModalFrame {
            delta_start: 0,
            delta_note: 0,
            scale_k: 1.0,
            pivot_tick: 0,
            edge: Some((StretchEdge::Start, 100)),
        };
        let ghost = modal_ghost(&note(0, 240, 60), &frame);
        assert_eq!((ghost.start, ghost.end()), (100, 240));
    }

    #[test]
    fn mode_toggles_both_ways() {
        assert_eq!(RollMode::Select.toggled(), RollMode::Draw);
        assert_eq!(RollMode::Draw.toggled(), RollMode::Select);
    }

    #[test]
    fn knife_crossing_slashes_through_the_row_centreline() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 200.0));
        let mut clip = MidiClip::default();
        clip.sequence.notes = vec![note(0, 240, 60)]; // x span 0..24 px at 0.1 px/tick
        let g = geom(rect, 0, 0.1, 60);
        let n = &clip.notes()[0];
        let row_top = rect.top();
        let centre = row_top + ROW_HEIGHT / 2.0;
        let k = |from: egui::Pos2, to: egui::Pos2| knife_crossing(from, to, n, &g);

        // A vertical slash at tick 120 crosses the row centreline inside.
        let to = egui::pos2(12.0, centre + 20.0);
        let from = egui::pos2(12.0, centre - 20.0);
        assert_eq!(k(from, to), Some(120));

        // Crossing the row outside the note's span cuts nothing (the
        // centreline meet is at x = 35 px → tick 350, past the note).
        let from = egui::pos2(40.0, centre - 20.0);
        let to_outside = egui::pos2(30.0, centre + 20.0);
        assert_eq!(k(from, to_outside), None);

        // Touching the note's boundary would cut off a zero-length half
        // (the centreline meet is at tick 0 exactly).
        let from = egui::pos2(-12.0, centre - 20.0);
        let to_edge = egui::pos2(12.0, centre + 20.0);
        assert_eq!(k(from, to_edge), None);

        // A sweep along the note's own row cuts in the middle of the
        // covered span: overlap [2..20] px → tick 110.
        let from = egui::pos2(2.0, centre);
        let to = egui::pos2(20.0, centre);
        assert_eq!(k(from, to), Some(110));

        // A segment nowhere near the row.
        let from = egui::pos2(2.0, centre + 5.0 * ROW_HEIGHT);
        let to = egui::pos2(20.0, centre + 5.0 * ROW_HEIGHT);
        assert_eq!(k(from, to), None);
    }

    /// P2: the time axis is absolute — two clips at different arrangement
    /// starts land at their own absolute x on one shared pixel scale, while
    /// every note-level number (and every command) stays clip-local.
    #[test]
    fn clips_with_different_origins_share_one_time_axis() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 200.0));
        let (t_x, top) = (0.1f32, 60);
        // One scroll position for both: absolute tick 0 sits at x = 0.
        let a = RollGeom {
            origin_tick: 0,
            t0_x: 0.0,
            ticks_to_x: t_x,
            top_note: top,
            row_h: ROW_HEIGHT,
            area: rect,
        };
        let b = RollGeom {
            origin_tick: 960,
            ..a
        };
        // Local tick 0 of the second clip is one quarter note to the right…
        assert_eq!(a.x_of(0.0), 0.0);
        assert_eq!(b.x_of(0.0), 96.0);
        // …and the same note in both clips keeps its relative layout.
        let n = note(0, 240, 60);
        assert_eq!(b.note_rect(&n).left() - a.note_rect(&n).left(), 96.0);
        // Absolute positions agree: a's local 960 is where b's local 0 is.
        assert_eq!(a.x_of(960.0), b.x_of(0.0));
        // The inverse mapping is clip-local too: one pixel, two meanings.
        assert!((a.local_at(96.0) - 960.0).abs() < 1e-3);
        assert!(b.local_at(96.0).abs() < 1e-3);
    }

    /// P2: scroll is clamped to the clip's absolute span, so a clip starting
    /// at tick 960 opens on its own left edge and cannot scroll off it.
    #[test]
    fn the_scroll_window_is_the_clip_span() {
        let window = ViewWindow {
            start: 960.0,
            end: 960.0 + 3840.0,
        };
        let (width, t_x) = (400.0f32, 1.0f32);
        assert_eq!(
            window.clamp_scroll(0.0, width, t_x),
            960.0,
            "left of the clip pins to its own start"
        );
        assert_eq!(window.clamp_scroll(10_000.0, width, t_x), 4400.0);
        // Zoomed past fit the whole span fits: pin to the start, no blank.
        assert_eq!(window.clamp_scroll(2000.0, width, 0.05), 960.0);
    }

    #[test]
    fn selection_bbox_spans_the_whole_selection() {
        let mut clip = MidiClip::default();
        clip.sequence.notes = vec![note(0, 240, 60), note(480, 240, 62), note(960, 240, 64)];
        assert_eq!(selection_bbox(&clip, &[0, 2]), Some((0, 1200)));
        assert_eq!(selection_bbox(&clip, &[1]), Some((480, 720)));
        assert_eq!(selection_bbox(&clip, &[]), None);
        assert_eq!(selection_bbox(&clip, &[7]), None, "a stale index");
    }

    #[test]
    fn scale_max_k_caps_the_stretch_at_the_loop_end() {
        assert!((scale_max_k(0, 720, 3840) - 3840.0 / 720.0).abs() < 1e-3);
        assert!((scale_max_k(240, 720, 3840) - 3600.0 / 480.0).abs() < 1e-3);
    }

    // ---- P3: subject assembly, focus cycling, context-wide view window ----

    /// D5: the double-clicked clip leads; duplicates and references to clips
    /// that no longer exist are dropped without taking a context slot.
    #[test]
    fn a_subject_leads_with_the_focus_and_drops_stale_entries() {
        let focus = ClipRef::midi(1, 2);
        let selection = [
            ClipRef::midi(0, 0),
            ClipRef::midi(1, 2), // the focus itself
            ClipRef::midi(3, 9), // track 3 has one clip: stale
            ClipRef::midi(0, 0), // duplicate
        ];
        let clips = |track: usize| if track == 3 { 1 } else { 4 };
        let subject = RollSubject::assemble(focus, &selection, clips, |_| 0);
        assert_eq!(subject.focus(), focus);
        assert_eq!(subject.context(), &[focus, ClipRef::midi(0, 0)]);
        assert_eq!(
            subject.dropped(),
            0,
            "stale/duplicate entries are not truncation"
        );
    }

    /// D4: the cap truncates in selection order, keeps the focus, and reports
    /// how many clips it could not show.
    #[test]
    fn the_context_cap_keeps_the_focus_and_counts_what_it_dropped() {
        let focus = ClipRef::midi(0, 0);
        let selection: Vec<ClipRef> = (0..12).map(|clip| ClipRef::midi(0, clip)).collect();
        let subject = RollSubject::assemble(focus, &selection, |_| 16, |_| 0);
        assert_eq!(subject.context().len(), MAX_ROLL_CONTEXT);
        assert_eq!(subject.focus(), focus);
        // 12 selection entries, one of which is the focus → 11 candidates,
        // MAX_ROLL_CONTEXT − 1 of which fit beside it.
        assert_eq!(subject.dropped(), 11 - (MAX_ROLL_CONTEXT - 1));
    }

    /// D7: Tab / Shift+Tab cycle the focus within the context and wrap.
    #[test]
    fn tab_cycles_the_focus_and_wraps() {
        let focus = ClipRef::midi(0, 0);
        let selection = [ClipRef::midi(0, 1), ClipRef::midi(0, 2)];
        let mut subject = RollSubject::assemble(focus, &selection, |_| 4, |_| 0);
        assert_eq!(subject.cycle_focus(false), ClipRef::midi(0, 1));
        assert_eq!(subject.cycle_focus(false), ClipRef::midi(0, 2));
        assert_eq!(subject.cycle_focus(false), focus, "forward wraps");
        assert_eq!(
            subject.cycle_focus(true),
            ClipRef::midi(0, 2),
            "backward steps to the last"
        );
        // Focusing something outside the context is refused, not clamped.
        assert!(!subject.set_focus(ClipRef::midi(9, 9)));
    }

    /// D2/D3: the view window covers the whole context, so a ghost is on
    /// screen the first time a multi-clip subject opens.
    #[test]
    fn the_view_window_is_the_whole_context() {
        let early = MidiClip {
            start_ticks: 0,
            loop_ticks: 960,
            ..MidiClip::default()
        };
        let late = MidiClip {
            start_ticks: 1920,
            loop_ticks: 960,
            ..MidiClip::default()
        };
        let tracks = [MidiTrack {
            clips: vec![early, late],
            ..MidiTrack::default()
        }];
        let subject =
            RollSubject::assemble(ClipRef::midi(0, 1), &[ClipRef::midi(0, 0)], |_| 2, |_| 0);
        let window = ViewWindow::of(&tracks, &subject);
        assert_eq!(window.start, 0.0);
        assert_eq!(window.end, 2880.0);
    }
}
