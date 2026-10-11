//! Piano roll editor component (egui self-drawn).
//!
//! Interaction conventions:
//! - Left-click on empty space: create a note snapped to the grid (selected);
//! - Left-drag on empty space: rubber-band selection (Shift/Ctrl adds to the
//!   current selection);
//! - Left-drag on a note: move the whole selection (uniform delta, so the
//!   sorted indices of the selection stay valid — see `MoveNotes`);
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
use resoz_model::{MidiClip, TICKS_PER_BAR, TICKS_PER_QUARTER, TICKS_PER_STEP};

pub use crate::grid::NoteValue;

/// Height of each semitone row (pixels).
pub const ROW_HEIGHT: f32 = 9.0;

/// Roll area height (pixels, 32 semitone rows).
pub const ROLL_HEIGHT: f32 = ROW_HEIGHT * 32.0;

/// Velocity lane height (pixels), below the roll.
pub const LANE_HEIGHT: f32 = 48.0;

/// Number of visible rows.
const VISIBLE_ROWS: i32 = (ROLL_HEIGHT / ROW_HEIGHT) as i32;

/// Pointer travel (pixels) that turns a press on empty space into a marquee
/// instead of a note creation.
const MARQUEE_THRESHOLD: f32 = 4.0;

/// Default duration of a new note (tick) — a 16th note.
const DEFAULT_NOTE_LENGTH: i64 = TICKS_PER_STEP as i64;

/// Accent color for selected notes / velocity bars.
const SELECTED: egui::Color32 = egui::Color32::from_rgb(240, 190, 80);

/// An edit the roll wants performed.
#[derive(Debug, Clone, PartialEq)]
pub enum RollIntent {
    /// Create a note and return its index (after sorting).
    AddNote { note: u8, start: i64, length: i64 },
    /// Move the notes at `indices` by a uniform delta (ticks / semitones).
    ///
    /// A uniform delta preserves the sort order (the sort is stable and every
    /// key shifts equally), so the caller's selection indices stay valid
    /// across frames — the invariant the whole selection UX rests on.
    MoveNotes {
        indices: Vec<usize>,
        delta_start: i64,
        delta_note: i8,
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
    /// Selection drag in progress (uniform move of all selected notes).
    drag: Option<DragSelection>,
    /// Rubber band in progress.
    marquee: Option<Marquee>,
    /// Left button pressed on empty space, not yet decided: a click creates a
    /// note, a drag becomes a marquee.
    press: Option<Press>,
}

#[derive(Debug)]
struct DragSelection {
    /// Pointer tick at grab time (f32; snapping happens per frame).
    grab_tick: f32,
    /// Pointer pitch at grab time.
    grab_note: u8,
}

#[derive(Debug)]
struct Marquee {
    origin: egui::Pos2,
    current: egui::Pos2,
    /// Shift/Ctrl held at press time: extend the existing selection.
    additive: bool,
}

#[derive(Debug)]
struct Press {
    origin: egui::Pos2,
    additive: bool,
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
            press: None,
        }
    }

    /// Replaces the selection with `index` (the note a gesture just created).
    pub fn select_only(&mut self, index: usize) {
        self.selection = vec![index];
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
        self.press = None;
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
        ui.weak("draw · drag marquee · RMB delete · arrows nudge · wheel pitch");
    });

    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, ROLL_HEIGHT),
        egui::Sense::click_and_drag(),
    );
    // The roll owns keyboard focus while in use (arrow-key nudging).
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
        let x = rect.left() + note.start as f32 * ticks_to_x;
        let w = (note.length as f32 * ticks_to_x).max(3.0);
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
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x, y + 1.0),
                egui::pos2(x + w, y + ROW_HEIGHT - 1.0),
            ),
            2.0,
            fill,
        );
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
    let hit_note = |pos: egui::Pos2| -> Option<usize> {
        let tick = pointer_tick(pos) as i64;
        let note = pointer_note(pos);
        clip.notes()
            .iter()
            .position(|n| n.note == note && tick >= n.start && tick < n.end())
    };
    let snap =
        |ticks: f32| -> i64 { ((ticks / grid_ticks as f32).round() as i64).max(0) * grid_ticks };
    let modifiers = ui.ctx().input(|i| i.modifiers);
    let additive = modifiers.shift || modifiers.ctrl;

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
            ));
        }
    }

    if response.drag_started() {
        if let Some(pos) = response.interact_pointer_pos() {
            match hit_note(pos) {
                Some(index) => {
                    // Clicking a note (re)selects it — or toggles it into an
                    // existing selection with a modifier held.
                    if additive {
                        if let Some(at) = state.selection.iter().position(|i| *i == index) {
                            state.selection.remove(at);
                        } else {
                            state.selection.push(index);
                        }
                    } else if !state.selection.contains(&index) {
                        state.selection = vec![index];
                    }
                    if !state.selection.is_empty() {
                        state.drag = Some(DragSelection {
                            grab_tick: pointer_tick(pos),
                            grab_note: pointer_note(pos),
                        });
                    }
                }
                None => {
                    state.press = Some(Press {
                        origin: pos,
                        additive,
                    });
                }
            }
        }
    }

    if response.dragged() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(drag) = &state.drag {
                // Uniform move of the whole selection. The delta is clamped on
                // the UI side so no note hits the engine's clamp asymmetrically
                // (that would permute the sort and invalidate the selection).
                let raw_start_delta = snap(pointer_tick(pos)) - snap(drag.grab_tick);
                let raw_note_delta = i32::from(pointer_note(pos)) - i32::from(drag.grab_note);
                if raw_start_delta != 0 || raw_note_delta != 0 {
                    intent = Some(clamped_move(
                        clip,
                        &state.selection,
                        raw_start_delta,
                        raw_note_delta.clamp(i16::from(i8::MIN) as i32, i16::from(i8::MAX) as i32)
                            as i8,
                        loop_ticks,
                    ));
                }
            } else if let Some(press) = &mut state.press {
                if pos.distance(press.origin) > MARQUEE_THRESHOLD {
                    state.marquee = Some(Marquee {
                        origin: press.origin,
                        current: pos,
                        additive: press.additive,
                    });
                    state.press = None;
                }
            } else if let Some(marquee) = &mut state.marquee {
                marquee.current = pos;
            }
        }
    }

    if response.drag_stopped() {
        // A press that never became a marquee was a click: create a note.
        if let Some(press) = state.press.take() {
            let start = snap(pointer_tick(press.origin)).min(loop_ticks - 1);
            intent = Some(RollIntent::AddNote {
                note: pointer_note(press.origin),
                start,
                length: DEFAULT_NOTE_LENGTH,
            });
        }
        // A finished marquee selects what it covers.
        if let Some(marquee) = state.marquee.take() {
            let band = egui::Rect::from_two_pos(marquee.origin, marquee.current);
            let hits: Vec<usize> = clip
                .notes()
                .iter()
                .enumerate()
                .filter(|(_, n)| {
                    let x0 = rect.left() + n.start as f32 * ticks_to_x;
                    let x1 = x0 + (n.length as f32 * ticks_to_x).max(3.0);
                    let y0 = rect.top() + (top_note as f32 - n.note as f32) * ROW_HEIGHT;
                    let note_rect = egui::Rect::from_min_max(
                        egui::pos2(x0, y0),
                        egui::pos2(x1, y0 + ROW_HEIGHT),
                    );
                    band.intersects(note_rect)
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

/// A uniform move clamped on the UI side so no note crosses the engine's
/// clamp boundary: asymmetric clamping would permute the sort order and
/// invalidate the selection indices (see `MoveNotes`).
fn clamped_move(
    clip: &MidiClip,
    indices: &[usize],
    raw_start_delta: i64,
    raw_note_delta: i8,
    loop_ticks: i64,
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
    use super::{ROLL_HEIGHT, ROW_HEIGHT, RollIntent, RollState, clamped_move, is_black_key};
    use crate::grid::{GridValue, NoteValue};
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
        let intent = clamped_move(&clip, &[0, 1, 2], -480, 0, loop_ticks);
        let RollIntent::MoveNotes { delta_start, .. } = intent else {
            panic!("expected a move");
        };
        assert_eq!(delta_start, 0, "the note at 0 blocks the whole move");

        // ...or right past the loop end.
        let intent = clamped_move(&clip, &[0, 1, 2], 480, 0, loop_ticks);
        let RollIntent::MoveNotes { delta_start, .. } = intent else {
            panic!("expected a move");
        };
        assert_eq!(delta_start, 960 - 1 - 480, "limited by the latest note");

        // Pitch: the 0-note blocks downward, the 127-note blocks upward.
        let intent = clamped_move(&clip, &[0, 1, 2], 0, -5, loop_ticks);
        let RollIntent::MoveNotes { delta_note, .. } = intent else {
            panic!("expected a move");
        };
        assert_eq!(delta_note, 0);
        let intent = clamped_move(&clip, &[0, 1, 2], 0, 5, loop_ticks);
        let RollIntent::MoveNotes { delta_note, .. } = intent else {
            panic!("expected a move");
        };
        assert_eq!(delta_note, 0);
    }

    #[test]
    fn uniform_move_preserves_sort_order() {
        // The invariant the selection UX rests on: shifting every note by the
        // same delta must not change the stable-sorted order, so the
        // selection's indices survive the command's re-sort.
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
}
