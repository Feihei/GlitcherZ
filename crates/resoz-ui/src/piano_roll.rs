//! Piano roll editor component (egui self-drawn).
//!
//! Interaction model (M4, see `docs/spec/editing-model.md`): the semantic
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

use eframe::egui;

use crate::grid::GridValue;
use resoz_model::{MidiClip, Note, TICKS_PER_BAR, TICKS_PER_QUARTER, TICKS_PER_STEP};

pub use crate::grid::NoteValue;

/// Height of each semitone row (pixels).
pub const ROW_HEIGHT: f32 = 9.0;

/// Roll area height (pixels, 32 semitone rows).
pub const ROLL_HEIGHT: f32 = ROW_HEIGHT * 32.0;

/// Velocity lane height (pixels), below the roll.
pub const LANE_HEIGHT: f32 = 48.0;

/// Number of visible rows.
const VISIBLE_ROWS: i32 = (ROLL_HEIGHT / ROW_HEIGHT) as i32;

/// Default duration of a new note (tick) — a 16th note.
const DEFAULT_NOTE_LENGTH: i64 = TICKS_PER_STEP as i64;

/// Accent color for selected notes / velocity bars.
const SELECTED: egui::Color32 = egui::Color32::from_rgb(240, 190, 80);

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
}

/// State persisted across frames (scroll, grid, selection, gesture).
#[derive(Debug)]
pub struct RollState {
    /// Top visible MIDI note number (f32 for smooth scrolling).
    pub top_note: f32,
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
            drag: None,
            marquee: None,
            knife: None,
            awaiting_create: None,
            draw_seq: 0,
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
        self.selection.clear();
    }

    /// Takes the pending status message (a no-op explanation), if any.
    pub fn take_status(&mut self) -> Option<String> {
        self.status.take()
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

/// Draw one frame of the roll + velocity lane and return the edit the gesture
/// produced (at most one per frame).
pub fn show(
    ui: &mut egui::Ui,
    clip: &MidiClip,
    state: &mut RollState,
    playhead_ticks: f32,
    playing: bool,
) -> Option<RollIntent> {
    let loop_ticks = i64::from(clip.loop_ticks.max(TICKS_PER_STEP)).max(1);
    let grid_ticks = state
        .grid
        .ticks(TICKS_PER_QUARTER, 120.0, 48_000.0, 30.0)
        .unwrap_or(i64::from(TICKS_PER_STEP))
        .max(1);

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
                    egui::RichText::new("no keyboard focus — click the roll")
                        .color(egui::Color32::YELLOW),
                ),
            );
        }
        ui.add(
            egui::Label::new(
                egui::RichText::new(
                    "click: select · drag: tweak · dbl-click: insert · edges: trim · Ctrl+drag: copy · d: draw · G move · S scale · T stretch (LMB ok · RMB/Esc cancel) · K knife · Del del · RMB: delete · arrows: nudge",
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
    // The roll owns keyboard focus while in use (arrow-key nudging); the
    // click itself is handled in the interaction section below (it needs
    // the pointer→tick/pitch helpers defined there). Any press counts —
    // a slow press-and-hold is neither a click (too long) nor yet a drag
    // (no movement), and focus must not depend on gesture classification.
    if response.clicked() || response.is_pointer_button_down_on() {
        response.request_focus();
    }
    state.had_focus = response.has_focus();

    // Wheel: scroll the pitch view vertically.
    if response.hovered() {
        let scroll = ui.ctx().input(|i| i.smooth_scroll_delta().y);
        if scroll != 0.0 {
            state.top_note =
                (state.top_note + scroll / ROW_HEIGHT).clamp(VISIBLE_ROWS as f32 - 1.0, 127.0);
            ui.ctx().request_repaint();
        }
    }

    let ticks_to_x = width / loop_ticks as f32;
    let painter = ui.painter_at(rect);
    let top_note = state.top_note.round() as i32;
    let min_note = top_note - VISIBLE_ROWS + 1;

    // ---- Pointer → tick/pitch helpers (the keyboard section needs them
    // for the modal grab point; the interaction sections below reuse them).
    let pointer_tick = |pos: egui::Pos2| ((pos.x - rect.left()) / ticks_to_x).max(0.0);
    let pointer_note = |pos: egui::Pos2| {
        (top_note as f32 - (pos.y - rect.top()) / ROW_HEIGHT)
            .round()
            .clamp(0.0, 127.0) as u8
    };
    let hit_note = |pos: egui::Pos2| note_at(clip, pos, rect, ticks_to_x, top_note);
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
        let y = rect.top() + (top_note - note) as f32 * ROW_HEIGHT;
        let color = if is_black_key(note) {
            egui::Color32::from_gray(26)
        } else {
            egui::Color32::from_gray(40)
        };
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(rect.left(), y), egui::vec2(width, ROW_HEIGHT)),
            0.0,
            color,
        );
    }

    // 2. Vertical grid, re-anchored on every bar line (M2.1): lines are
    //    counted from each bar start, so they stay aligned with the bars
    //    whatever the meter does to a bar's length.
    let bars = (loop_ticks + i64::from(TICKS_PER_BAR) - 1) / i64::from(TICKS_PER_BAR);
    for bar in 0..bars {
        let bar_start = bar * i64::from(TICKS_PER_BAR);
        let bar_end = (bar_start + i64::from(TICKS_PER_BAR)).min(loop_ticks);
        let mut tick = bar_start;
        while tick < bar_end {
            let is_bar_line = tick % i64::from(TICKS_PER_BAR) == 0;
            let x = rect.left() + tick as f32 * ticks_to_x;
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

    // 3. Note blocks, shaded by velocity (M2.5); selected notes in the accent color.
    // During a non-identity modal the selected originals are faded out so the
    // ghost on top stays readable where the two overlap (Blender convention:
    // origin dimmed, transform preview solid).
    let modal_active = modal_frame.as_ref().is_some_and(|f| !f.is_identity());
    for (index, note) in clip.notes().iter().enumerate() {
        let note_i = i32::from(note.note);
        if !(min_note..=top_note).contains(&note_i) {
            continue;
        }
        let y = rect.top() + (top_note as f32 - note.note as f32) * ROW_HEIGHT;
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
        painter.rect_filled(note_rect(rect, note, ticks_to_x, y), 2.0, fill);
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
            let y = rect.top() + (top_note as f32 - f32::from(ghost.note)) * ROW_HEIGHT;
            painter.rect_filled(
                note_rect(rect, &ghost, ticks_to_x, y),
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

    // 5. Playhead (musical position modulo loop length).
    if playing {
        let pos = playhead_ticks.rem_euclid(loop_ticks as f32);
        let x = rect.left() + pos * ticks_to_x;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(2.0, egui::Color32::YELLOW),
        );
    }

    // ---- Velocity lane (M2.4): bars under the roll, drag to set. ----
    let (lane_rect, lane_response) =
        ui.allocate_exact_size(egui::vec2(width, LANE_HEIGHT), egui::Sense::drag());
    let lane_painter = ui.painter_at(lane_rect);
    lane_painter.rect_filled(lane_rect, 0.0, egui::Color32::from_gray(20));
    for (index, note) in clip.notes().iter().enumerate() {
        let x = lane_rect.left() + note.start as f32 * ticks_to_x;
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
        let x = lane_rect.left() + pos * ticks_to_x;
        lane_painter.line_segment(
            [
                egui::pos2(x, lane_rect.top()),
                egui::pos2(x, lane_rect.bottom()),
            ],
            egui::Stroke::new(2.0, egui::Color32::YELLOW),
        );
    }

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
                let (x0, x1) = note_edges(rect, note, ticks_to_x);
                if edge_zone(pos.x, x0, x1).is_some() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
            } else if state.mode == RollMode::Draw {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            }
        }
    }

    if response.drag_started() {
        // Keyboard commands must work right after the first drag, even if
        // the roll never saw a plain click.
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
                    let (x0, x1) = note_edges(rect, note, ticks_to_x);
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
                        if additive {
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
    if response.dragged() && !(response.drag_started() && intent.is_some()) {
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
                    if let Some(tick) = knife_crossing(from, pos, rect, note, ticks_to_x, top_note)
                    {
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

    if response.drag_stopped() {
        // A finished marquee selects what it covers.
        if let Some(marquee) = state.marquee.take() {
            let band = egui::Rect::from_two_pos(marquee.origin, marquee.current);
            let hits: Vec<usize> = clip
                .notes()
                .iter()
                .enumerate()
                .filter(|(_, n)| {
                    let y0 = rect.top() + (top_note as f32 - n.note as f32) * ROW_HEIGHT;
                    band.intersects(note_rect(rect, n, ticks_to_x, y0))
                })
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
    if lane_response.dragged() || lane_response.drag_started() {
        if let Some(pos) = lane_response.interact_pointer_pos() {
            let velocity = ((lane_rect.bottom() - pos.y) / (LANE_HEIGHT - 4.0) * 127.0)
                .round()
                .clamp(1.0, 127.0) as u8;
            let updates: Vec<(usize, u8)> = if state.selection.is_empty() {
                let tick = ((pos.x - lane_rect.left()) / ticks_to_x).max(0.0) as i64;
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

    intent
}

/// Snap a tick position to the grid (nearest line, never negative).
fn snap_ticks(ticks: f32, grid_ticks: i64) -> i64 {
    ((ticks / grid_ticks as f32).round() as i64).max(0) * grid_ticks
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
fn knife_crossing(
    from: egui::Pos2,
    to: egui::Pos2,
    rect: egui::Rect,
    note: &Note,
    ticks_to_x: f32,
    top: i32,
) -> Option<i64> {
    let row_top = rect.top() + (top as f32 - f32::from(note.note)) * ROW_HEIGHT;
    let centre = row_top + ROW_HEIGHT / 2.0;
    let x_left = rect.left() + note.start as f32 * ticks_to_x;
    let x_right = x_left + (note.length as f32 * ticks_to_x).max(3.0);
    let cut_at = |x: f32| -> Option<i64> {
        let tick = ((x - rect.left()) / ticks_to_x).round() as i64;
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
    let band = row_top..=(row_top + ROW_HEIGHT);
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

/// The note whose hit-testing rectangle covers `pos`, if any.
///
/// Hit-testing mirrors **drawing** (`note_rect`) plus [`HIT_SLOP`]: the
/// same pixels, not a tick range. A tick-range test (`start <= tick <
/// end`) disagrees with the drawn width in two ways that both read as
/// bugs to the user — the minimum 3 px width makes short notes look wider
/// than their ticks, and the half-open range excludes the right edge
/// itself, so grabbing a note's tail lands on empty space and paints a
/// *new* note there. The slop keeps the last few pixels of aim error on
/// the note instead of falling through to the draw-on-empty gesture.
fn note_at(
    clip: &MidiClip,
    pos: egui::Pos2,
    rect: egui::Rect,
    ticks_to_x: f32,
    top: i32,
) -> Option<usize> {
    clip.notes().iter().position(|n| {
        let y = rect.top() + (top as f32 - f32::from(n.note)) * ROW_HEIGHT;
        note_hit_rect(rect, n, ticks_to_x, y).contains(pos)
    })
}

/// The on-screen rectangle a note is painted into. The single source of
/// truth for both painting and hit-testing, so the two can never drift.
fn note_rect(rect: egui::Rect, note: &Note, ticks_to_x: f32, y: f32) -> egui::Rect {
    let x = rect.left() + note.start as f32 * ticks_to_x;
    let w = (note.length as f32 * ticks_to_x).max(3.0);
    egui::Rect::from_min_max(
        egui::pos2(x, y + 1.0),
        egui::pos2(x + w, y + ROW_HEIGHT - 1.0),
    )
}

/// Left / right pixel edges of a note's drawn rectangle. Edge trims are
/// measured against these — the *visual* edges, without slop.
fn note_edges(rect: egui::Rect, note: &Note, ticks_to_x: f32) -> (f32, f32) {
    let x = rect.left() + note.start as f32 * ticks_to_x;
    (x, x + (note.length as f32 * ticks_to_x).max(3.0))
}

/// The hit-testing rectangle: the drawn one plus [`HIT_SLOP`]. Kept
/// separate from [`note_rect`] so painting and the edge zones stay exact
/// — only *landing on* a note is forgiving.
fn note_hit_rect(rect: egui::Rect, note: &Note, ticks_to_x: f32, y: f32) -> egui::Rect {
    note_rect(rect, note, ticks_to_x, y).expand2(HIT_SLOP)
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
        AwaitCreate, Axis, DragKind, DragSelection, EDGE_PX, HIT_SLOP, Modal, ModalFrame,
        ModalKind, ROLL_HEIGHT, ROW_HEIGHT, RollIntent, RollMode, RollState, StretchEdge,
        clamped_move, draw_end, edge_zone, is_black_key, knife_crossing, modal_ghost, note_at,
        note_rect, scale_max_k, selection_bbox, snap_ticks, trim_start,
    };
    use crate::grid::{GridValue, NoteValue};
    use eframe::egui;
    use resoz_model::{MidiClip, Note, TICKS_PER_STEP};

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
    #[test]
    fn a_note_hit_test_matches_the_rectangle_it_is_drawn_into() {
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 200.0), egui::vec2(400.0, 200.0));
        let mut clip = MidiClip::default();
        // start 0, length 240; ticks_to_x = 1.0 → drawn 100.0 .. 340.0.
        clip.sequence.notes = vec![note(0, 240, 60)];
        let ticks_to_x = 1.0f32;
        let top = 60i32;
        let y = rect.top();

        let drawn = note_rect(rect, &clip.notes()[0], ticks_to_x, y);
        assert_eq!(drawn.left(), 100.0);
        assert_eq!(drawn.right(), 340.0);

        // Anywhere inside the drawn rectangle hits, including both edges.
        for x in [drawn.left(), drawn.center().x, drawn.right() - 0.5] {
            let pos = egui::pos2(x, drawn.center().y);
            assert_eq!(
                note_at(&clip, pos, rect, ticks_to_x, top),
                Some(0),
                "x={x} is inside the drawn rect and must hit"
            );
        }
        // HIT_SLOP keeps an off-by-a-few-pixels tail grab on the note
        // instead of letting it fall through to "draw a new note".
        assert_eq!(
            note_at(
                &clip,
                egui::pos2(drawn.right() + 1.0, drawn.center().y),
                rect,
                ticks_to_x,
                top
            ),
            Some(0),
            "a press a pixel past the tail still hits (aim error is not empty space)"
        );
        // Past the slack it really is empty space.
        assert_eq!(
            note_at(
                &clip,
                egui::pos2(drawn.right() + HIT_SLOP.x + 0.5, drawn.center().y),
                rect,
                ticks_to_x,
                top
            ),
            None
        );
        // A neighbouring pitch row does not hit either (the y slack only
        // fills this note's own row gutter).
        assert_eq!(
            note_at(
                &clip,
                egui::pos2(drawn.center().x, drawn.center().y + ROW_HEIGHT),
                rect,
                ticks_to_x,
                top
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
        let ticks_to_x = 0.1f32;
        let y = rect.top();
        let drawn = note_rect(rect, &clip.notes()[0], ticks_to_x, y);
        assert_eq!(drawn.width(), 3.0, "the minimum width is what gets drawn");
        for x in [drawn.left() + 0.5, drawn.right() - 0.5] {
            assert_eq!(
                note_at(&clip, egui::pos2(x, drawn.center().y), rect, ticks_to_x, 60),
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
        let ttx = 0.1f32;
        let top = 60;
        let n = &clip.notes()[0];
        let row_top = rect.top();
        let centre = row_top + ROW_HEIGHT / 2.0;

        // A vertical slash at tick 120 crosses the row centreline inside.
        let to = egui::pos2(12.0, centre + 20.0);
        let from = egui::pos2(12.0, centre - 20.0);
        assert_eq!(knife_crossing(from, to, rect, n, ttx, top), Some(120));

        // Crossing the row outside the note's span cuts nothing (the
        // centreline meet is at x = 35 px → tick 350, past the note).
        let from = egui::pos2(40.0, centre - 20.0);
        let to_outside = egui::pos2(30.0, centre + 20.0);
        assert_eq!(knife_crossing(from, to_outside, rect, n, ttx, top), None);

        // Touching the note's boundary would cut off a zero-length half
        // (the centreline meet is at tick 0 exactly).
        let from = egui::pos2(-12.0, centre - 20.0);
        let to_edge = egui::pos2(12.0, centre + 20.0);
        assert_eq!(knife_crossing(from, to_edge, rect, n, ttx, top), None);

        // A sweep along the note's own row cuts in the middle of the
        // covered span: overlap [2..20] px → tick 110.
        let from = egui::pos2(2.0, centre);
        let to = egui::pos2(20.0, centre);
        assert_eq!(knife_crossing(from, to, rect, n, ttx, top), Some(110));

        // A segment nowhere near the row.
        let from = egui::pos2(2.0, centre + 5.0 * ROW_HEIGHT);
        let to = egui::pos2(20.0, centre + 5.0 * ROW_HEIGHT);
        assert_eq!(knife_crossing(from, to, rect, n, ttx, top), None);
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
}
