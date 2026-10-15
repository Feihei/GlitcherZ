//! Event List — a spreadsheet view of a clip's notes (M3.6, from the
//! SoundCraft survey's Event List).
//!
//! One row per note; Start / Note / Velocity / Length are editable in place
//! with drag values, and a delete button per row. This is the precision
//! counterpart to the piano roll: drag in the roll, type here.
//!
//! **Read-only over the document**, like every editor component: it returns
//! [`EventListIntent`]s and the caller runs them as commands. A drag on one
//! of the values emits one intent per frame and is expected to run through a
//! merged gesture, so one drag is one undo step.

use eframe::egui;

use glitcherz_model::MidiClip;

/// Absolute field changes for one note (unset fields keep their value).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NoteEdit {
    pub index: usize,
    pub start: Option<i64>,
    pub note: Option<u8>,
    pub velocity: Option<u8>,
    pub length: Option<i64>,
}

impl NoteEdit {
    fn empty(index: usize) -> Self {
        Self {
            index,
            ..Self::default()
        }
    }
}

/// An edit the event list wants performed.
#[derive(Debug, Clone, PartialEq)]
pub enum EventListIntent {
    /// Apply the changed fields of one note.
    Edit(NoteEdit),
    /// Remove the note at `index`.
    Delete { index: usize },
}

/// Renders the list and returns at most one intent per frame.
pub fn show(ui: &mut egui::Ui, clip: &MidiClip) -> Option<EventListIntent> {
    let mut intent = None;
    // All drag values of a frame funnel their changes into one edit.
    let pending: std::cell::RefCell<Option<NoteEdit>> = std::cell::RefCell::default();

    egui::ScrollArea::vertical()
        .max_height(180.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Grid::new("event_list_grid")
                .num_columns(6)
                .striped(true)
                .min_col_width(52.0)
                .show(ui, |ui| {
                    for label in ["Start", "Note", "Vel", "Length", "Ch", ""] {
                        ui.strong(label);
                    }
                    ui.end_row();

                    for (index, note) in clip.notes().iter().enumerate() {
                        drag_i64(
                            ui,
                            &pending,
                            index,
                            note.start,
                            |edit, value| edit.start = Some(value.max(0)),
                            "tick",
                        );
                        drag_u8_note(ui, &pending, index, note.note);
                        drag_i64(
                            ui,
                            &pending,
                            index,
                            i64::from(note.velocity),
                            |edit, value| edit.velocity = Some(value.clamp(1, 127) as u8),
                            "",
                        );
                        drag_i64(
                            ui,
                            &pending,
                            index,
                            note.length,
                            |edit, value| edit.length = Some(value.max(1)),
                            "tick",
                        );
                        ui.weak(note.channel.to_string());
                        if ui.button("×").clicked() {
                            intent = Some(EventListIntent::Delete { index });
                        }
                        ui.end_row();
                    }
                });
        });

    if let Some(edit) = pending.into_inner() {
        intent = Some(EventListIntent::Edit(edit));
    }
    intent
}

/// A drag value over an integer field; `apply` records the clamped change.
fn drag_i64(
    ui: &mut egui::Ui,
    pending: &std::cell::RefCell<Option<NoteEdit>>,
    index: usize,
    value: i64,
    mut apply: impl FnMut(&mut NoteEdit, i64),
    suffix: &str,
) {
    let mut drag = egui::DragValue::from_get_set(|new| match new {
        Some(v) => {
            let rounded = v.round() as i64;
            let mut slot = pending.borrow_mut();
            let edit = slot.get_or_insert_with(|| NoteEdit::empty(index));
            apply(edit, rounded);
            rounded as f64
        }
        None => value as f64,
    })
    .speed(5.0);
    if !suffix.is_empty() {
        drag = drag.suffix(suffix);
    }
    ui.add(drag);
}

/// Pitch drag that *displays* a note name ("C4") while editing semitones.
fn drag_u8_note(
    ui: &mut egui::Ui,
    pending: &std::cell::RefCell<Option<NoteEdit>>,
    index: usize,
    value: u8,
) {
    ui.add(
        egui::DragValue::from_get_set(|new| match new {
            Some(v) => {
                let clamped = v.round().clamp(0.0, 127.0) as u8;
                let mut slot = pending.borrow_mut();
                let edit = slot.get_or_insert_with(|| NoteEdit::empty(index));
                edit.note = Some(clamped);
                f64::from(clamped)
            }
            None => f64::from(value),
        })
        .speed(1.0)
        .range(0..=127)
        .custom_formatter(|v, _| note_name(v.round().clamp(0.0, 127.0) as u8)),
    );
}

/// MIDI note number → "C4" style name (middle C = C4).
fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!(
        "{}{}",
        NAMES[usize::from(note) % 12],
        i32::from(note) / 12 - 1
    )
}

#[cfg(test)]
mod tests {
    use super::note_name;

    #[test]
    fn note_names_are_scientific_pitch() {
        assert_eq!(note_name(60), "C4", "middle C");
        assert_eq!(note_name(61), "C#4");
        assert_eq!(note_name(0), "C-1");
        assert_eq!(note_name(127), "G9");
    }
}
