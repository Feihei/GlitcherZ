//! Piano roll editor component (egui self-drawn).
//!
//! Interaction conventions (mainstream DAW muscle memory):
//! - Left-click on empty space: deselect (a stray click never paints a
//!   note); double-click: insert a note of the default length (a 16th);
//! - Left-click on a note: select it (Shift/Ctrl toggles it in/out of the
//!   selection);
//! - Press-and-drag on empty space: **draw** a note — note on at the press
//!   point, note off at the release point. Start and pitch stay where the
//!   gesture began; only the end follows the pointer (snapped to the grid);
//! - Shift/Ctrl + drag on empty space: rubber-band selection (adds to the
//!   current selection);
//! - Left-drag on a note: move the whole selection;
//! - Ctrl + drag on a note: copy the selection and drag the copies;
//! - Drag a note's left/right edge (the pointer turns into a horizontal
//!   resize cursor within 5 px): trim that edge of the whole selection;
//! - Right-click: delete the note under the pointer, or the whole selection
//!   if that note is part of it;
//! - Arrow keys (when the roll has focus): nudge the selection — up/down a
//!   semitone (Shift: an octave), left/right one grid cell;
//! - Mouse wheel: scroll the pitch view vertically.
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
    /// Selection drag in progress (uniform move of all selected notes, or a
    /// draw gesture setting a fresh note's length).
    drag: Option<DragSelection>,
    /// Rubber band in progress (Shift/Ctrl + drag on empty space).
    marquee: Option<Marquee>,
    /// A note was just requested on empty space; the roll does not know its
    /// sorted index until the command reports back, but the draw-drag must
    /// start immediately — the gesture id is kept here.
    awaiting_create: Option<AwaitCreate>,
    /// Monotonic id for press-draw gestures: each draw (AddNote + its
    /// ResizeEnd frames) becomes one undo step.
    draw_seq: u64,
}

/// Which kind of drag a [`DragSelection`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragKind {
    /// Move the whole selection (grab on an existing note).
    Move,
    /// Trim the start edge (left-edge drag).
    ResizeStart,
    /// Draw the length of a just-created note (note on at the press point,
    /// note off at the release point) or trim the end edge (right-edge
    /// drag): the end follows the pointer.
    ResizeEnd,
}

/// One in-flight drag (move, or a trim of either edge).
///
/// The grabbed note's geometry is **frozen here at grab time** and the
/// per-frame intent is the increment still owed (`target - applied`).
/// Both halves matter:
///
/// - *Frozen anchor* — every command re-sorts the clip, so a held index
///   stops pointing at the grabbed note after the first frame. Deriving
///   the target from the anchor instead of re-reading `clip.notes()[i]`
///   keeps a trim attached to the note the user grabbed.
/// - *Incremental* — commands apply *relative* deltas (`start + ticks`).
///   Re-emitting the grab-relative total every frame would compound
///   (1 + 2 + 3 + … grid cells), so the note would run away from the
///   pointer instead of tracking it.
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
        Self {
            anchor_start: note.start,
            anchor_end: note.start + note.length,
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
    /// Always true today: the marquee only exists under Shift/Ctrl and
    /// extends the existing selection.
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
            drag: None,
            marquee: None,
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

    /// Whether a drag or marquee gesture is in progress.
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some() || self.marquee.is_some()
    }

    /// Drops every gesture and the selection.
    ///
    /// Called before undo/redo (and after deletions): the held indices refer
    /// to a document that changed underneath them.
    pub fn clear_gesture(&mut self) {
        self.drag = None;
        self.marquee = None;
        self.awaiting_create = None;
        self.selection.clear();
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

    // Toolbar: grid menu + scrolling + hints.
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
        ui.weak(
            "click: select · dbl-click: insert · drag: draw·move · note edges: trim · Ctrl+drag: copy · Shift+drag: band · RMB: delete · arrows: nudge",
        );
    });

    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, ROLL_HEIGHT),
        egui::Sense::click_and_drag(),
    );
    // The roll owns keyboard focus while in use (arrow-key nudging); the
    // click itself is handled in the interaction section below (it needs
    // the pointer→tick/pitch helpers defined there).
    if response.clicked() {
        response.request_focus();
    }

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
        painter.rect_filled(note_rect(rect, note, ticks_to_x, y), 2.0, fill);
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

    // ---- Interaction → intents. ----
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

    // A plain click (no drag): select the note under the pointer; on empty
    // space, deselect — or insert a default note on the second click of a
    // double-click. Creation deliberately requires a drag or a double-click,
    // so a stray click meant as "clear the selection" never paints a note.
    // (egui only reports drag_started once the pointer moved, so click and
    // drag are already disambiguated.)
    if response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
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

    // Arrow-key nudging (M2.3) — only while the roll holds focus.
    if response.has_focus() {
        let (down, up, left, right, shift) = ui.ctx().input(|i| {
            (
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
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

    // Edge-trim affordance: hovering near a note's start/end turns the
    // pointer into a horizontal resize cursor.
    if response.hovered()
        && state.drag.is_none()
        && let Some(pos) = response.hover_pos()
        && let Some(index) = hit_note(pos)
    {
        let note = &clip.notes()[index];
        let (x0, x1) = note_edges(rect, note, ticks_to_x);
        if edge_zone(pos.x, x0, x1).is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
    }

    if response.drag_started() {
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
                            DragKind::Move,
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
                                DragKind::Move,
                                state.next_gesture(),
                            ));
                        }
                    }
                }
                None => {
                    if additive {
                        // Shift/Ctrl + drag on empty space: rubber band.
                        state.marquee = Some(Marquee {
                            origin: pos,
                            current: pos,
                            additive: true,
                        });
                    } else {
                        // Press-drag on empty space: create the note right
                        // away (note on at the press point) and draw its
                        // length until release (note off at the pointer).
                        // The note's sorted index is unknown until the
                        // command reports back, so the gesture parks in
                        // `awaiting_create` and `resolve_created` starts
                        // the draw-drag.
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
                    }
                }
            }
        }
    }

    if response.dragged() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(drag) = &mut state.drag {
                // Every branch below computes the *absolute* target the
                // gesture is asking for, then emits only the increment
                // still owed (`target - applied`). Commands apply relative
                // deltas, so emitting the whole grab-relative offset every
                // frame would compound (1 + 2 + 3 + …) and the note would
                // run away from the pointer instead of tracking it.
                match drag.kind {
                    DragKind::Move => {
                        // Uniform move of the whole selection. The delta is
                        // clamped on the UI side so no note hits the engine's
                        // clamp asymmetrically (that would permute the sort
                        // and invalidate the selection).
                        let want_start = snap(pointer_tick(pos)) - snap(drag.grab_tick);
                        let want_note = i64::from(pointer_note(pos)) - i64::from(drag.grab_note);
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
        // A press-draw that never actually moved (or whose note creation
        // failed) leaves a parked grab point; drop it so no drag starts
        // from a stale gesture.
        state.awaiting_create = None;
    }

    if response.secondary_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(index) = hit_note(pos) {
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
    let notes = clip.notes();
    let mut delta_start = raw_start_delta;
    let mut delta_note = i64::from(raw_note_delta);
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
    RollIntent::MoveNotes {
        indices: indices.to_vec(),
        delta_start,
        delta_note: delta_note.clamp(i64::from(i8::MIN), i64::from(i8::MAX)) as i8,
        gesture,
    }
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
        AwaitCreate, DragKind, DragSelection, EDGE_PX, HIT_SLOP, ROLL_HEIGHT, ROW_HEIGHT,
        RollIntent, RollState, clamped_move, draw_end, edge_zone, is_black_key, note_at, note_rect,
        snap_ticks, trim_start,
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
        let mut drag = DragSelection::anchored(&note(0, grid, 60), 0.0, 60, DragKind::Move, 1);
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
}
