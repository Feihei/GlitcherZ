//! Piano roll editor component (egui self-drawn).
//!
//! Interaction conventions:
//! - Left-click on empty space: create a note snapped to the grid, and immediately drag it
//!   around;
//! - Left-drag on an existing note: move it (start position + pitch);
//! - Right-click: delete the note under the pointer;
//! - Mouse wheel: scroll the pitch view vertically.
//!
//! The timeline is fixed to "stretch the whole loop across the available width"
//! (fit-to-width); no zoom yet. The time snap granularity is [`SNAP_TICKS`]. The playhead is
//! driven by the musical position read back from the engine.

use eframe::egui;

use resoz_model::{Clip, Note, TICKS_PER_QUARTER, TICKS_PER_STEP};

/// Height of each semitone row (pixels).
pub const ROW_HEIGHT: f32 = 9.0;

/// Roll area height (pixels, 32 semitone rows).
pub const ROLL_HEIGHT: f32 = ROW_HEIGHT * 32.0;

/// Number of visible rows.
const VISIBLE_ROWS: i32 = (ROLL_HEIGHT / ROW_HEIGHT) as i32;

/// Time snap granularity (tick) — a 1/64 note.
pub const SNAP_TICKS: u32 = 6;

/// Default duration of a new note (tick) — a 16th note.
const DEFAULT_NOTE_LENGTH: u32 = TICKS_PER_STEP;

/// State persisted across frames (scroll position + note being dragged).
#[derive(Debug, Default)]
pub struct RollState {
    /// Top visible MIDI note number (f32 for smooth scrolling).
    pub top_note: f32,
    /// Note being dragged (index within the clip + grab point's time offset relative to the
    /// note start).
    drag: Option<DragNote>,
}

#[derive(Debug)]
struct DragNote {
    index: usize,
    grab_offset_ticks: f32,
}

impl RollState {
    /// Initial view top at C6 (84), showing about two and a half octaves downward.
    pub fn new() -> Self {
        Self {
            top_note: 84.0,
            drag: None,
        }
    }
}

fn is_black_key(note: i32) -> bool {
    matches!(note.rem_euclid(12), 1 | 3 | 6 | 8 | 10)
}

/// Draw and interact one frame of the roll. Modifies `clip` in place and returns whether an edit
/// happened this frame (the caller then re-pushes the clip to the audio thread).
pub fn show(
    ui: &mut egui::Ui,
    clip: &mut Clip,
    state: &mut RollState,
    playhead_ticks: f32,
    playing: bool,
) -> bool {
    let loop_ticks = clip.loop_ticks.max(TICKS_PER_STEP).max(1);
    let mut edited = false;

    // Toolbar: scrolling + hints.
    ui.horizontal(|ui| {
        if ui.button("▲").clicked() {
            state.top_note = (state.top_note + 12.0).min(127.0);
        }
        if ui.button("▼").clicked() {
            state.top_note = (state.top_note - 12.0).max(VISIBLE_ROWS as f32);
        }
        ui.weak("LMB draw/drag · RMB delete · wheel scrolls pitch");
    });

    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, ROLL_HEIGHT),
        egui::Sense::click_and_drag(),
    );

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

    // 2. Vertical grid: thin lines per step, thick lines per beat.
    let mut tick = 0u32;
    while tick <= loop_ticks {
        let x = rect.left() + tick as f32 * ticks_to_x;
        let is_beat = tick % TICKS_PER_QUARTER == 0;
        let stroke = egui::Stroke::new(
            if is_beat { 1.5 } else { 1.0 },
            if is_beat {
                egui::Color32::from_gray(95)
            } else {
                egui::Color32::from_gray(60)
            },
        );
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            stroke,
        );
        tick += TICKS_PER_STEP;
    }

    // 3. Note blocks.
    for (index, note) in clip.notes.iter().enumerate() {
        let note_i = i32::from(note.note);
        if !(min_note..=top_note).contains(&note_i) {
            continue;
        }
        let y = rect.top() + (top_note as f32 - note.note as f32) * ROW_HEIGHT;
        let x = rect.left() + note.start as f32 * ticks_to_x;
        let w = (note.length as f32 * ticks_to_x).max(3.0);
        let dragging = state.drag.as_ref().is_some_and(|d| d.index == index);
        let fill = if dragging {
            egui::Color32::from_rgb(120, 200, 140)
        } else {
            egui::Color32::from_rgb(64, 160, 96)
        };
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x, y + 1.0),
                egui::pos2(x + w, y + ROW_HEIGHT - 1.0),
            ),
            2.0,
            fill,
        );
    }

    // 4. Playhead (musical position modulo loop length).
    if playing {
        let pos = playhead_ticks.rem_euclid(loop_ticks as f32);
        let x = rect.left() + pos * ticks_to_x;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(2.0, egui::Color32::YELLOW),
        );
    }

    // 5. Interaction.
    let pointer_tick = |pos: egui::Pos2| ((pos.x - rect.left()) / ticks_to_x).max(0.0);
    let pointer_note = |pos: egui::Pos2| {
        (top_note as f32 - (pos.y - rect.top()) / ROW_HEIGHT)
            .round()
            .clamp(0.0, 127.0) as u8
    };
    let hit_note = |pos: egui::Pos2, clip: &Clip| -> Option<usize> {
        let tick = pointer_tick(pos) as u32;
        let note = pointer_note(pos);
        clip.notes
            .iter()
            .position(|n| n.note == note && tick >= n.start && tick < n.start + n.length)
    };
    let snap = |ticks: f32| -> u32 { (ticks / SNAP_TICKS as f32).round() as u32 * SNAP_TICKS };

    if response.drag_started() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(index) = hit_note(pos, clip) {
                let grab_offset = pointer_tick(pos) - clip.notes[index].start as f32;
                state.drag = Some(DragNote {
                    index,
                    grab_offset_ticks: grab_offset,
                });
            } else {
                // Press on empty space: create a note and start dragging.
                let start = snap(pointer_tick(pos)).min(loop_ticks - 1);
                clip.notes.push(Note {
                    start,
                    length: DEFAULT_NOTE_LENGTH,
                    note: pointer_note(pos),
                    velocity: 0.9,
                });
                state.drag = Some(DragNote {
                    index: clip.notes.len() - 1,
                    grab_offset_ticks: 0.0,
                });
                edited = true;
            }
        }
    }

    if response.dragged() {
        if let (Some(pos), Some(drag)) = (response.interact_pointer_pos(), &state.drag) {
            if let Some(note) = clip.notes.get_mut(drag.index) {
                let start = snap(pointer_tick(pos) - drag.grab_offset_ticks)
                    .min(loop_ticks.saturating_sub(1));
                if note.start != start || note.note != pointer_note(pos) {
                    note.start = start;
                    note.note = pointer_note(pos);
                    edited = true;
                }
            }
        }
    }

    if response.drag_stopped() {
        state.drag = None;
    }

    if response.secondary_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(index) = hit_note(pos, clip) {
                clip.notes.remove(index);
                state.drag = None;
                edited = true;
            }
        }
    }

    edited
}

#[cfg(test)]
mod tests {
    use super::{RollState, is_black_key};

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
        assert!(state.drag.is_none());
    }
}
