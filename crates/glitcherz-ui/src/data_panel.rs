//! Data panel — the pool of shared MIDI / curve content (F-046 M1).
//!
//! A bottom strip **inside the timeline view**, above which the toolbar and
//! the rows keep their full height (D3): the panel declares itself before
//! `timeline::show`, so the arrangement gets whatever is left. The Edit tab's
//! Event List is the model — same `Panel::show_switched` folded/expanded pair,
//! same draggable divider, same "click the caption to collapse".
//!
//! Read-only over the document: the pool is borrowed from `Project`, and
//! anything that edits it (rename, purge, reference jump) belongs to M2.
//!
//! Why a pool table at all: `clip.data` is a **slot index**, and
//! `compact_clip_data` renumbers slots on save, so `#3` is not an identity.
//! The name (D1) is what a person can point at.

use eframe::egui;
use glitcherz_model::{CurveData, MidiData, Project};

/// Share of the tab the strip takes when first opened (Event List's value).
pub const DATA_PANEL_HEIGHT_FRACTION: f32 = 1.0 / 3.0;
/// Caption row plus one data row: the floor the divider can be dragged to.
pub const DATA_PANEL_MIN_H: f32 = 26.0;
/// Ceiling as a share of the tab, so the arrangement keeps a usable band.
const DATA_PANEL_MAX_FRACTION: f32 = 0.6;
/// One table row.
const ROW_H: f32 = 18.0;
/// Fixed column widths; the name column takes the rest.
const ID_W: f32 = 56.0;
const KIND_W: f32 = 62.0;
const ITEMS_W: f32 = 74.0;
const LENGTH_W: f32 = 70.0;
const REFS_W: f32 = 62.0;

/// Which pool rows the table shows (D10; view state, never saved).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DataFilter {
    #[default]
    All,
    Midi,
    Curve,
}

/// Which pool a row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataKind {
    Midi,
    Curve,
}

/// One table row. Borrows the name so a frame copies nothing.
pub struct DataRow<'a> {
    pub kind: DataKind,
    /// Pool slot — display-only (D2/§8: it moves when the pool compacts).
    pub index: u32,
    pub name: &'a str,
    /// Notes (MIDI) or anchors (curve) — the `内容量` column.
    pub items: usize,
    pub length_ticks: u32,
    pub refs: usize,
}

impl DataRow<'_> {
    /// The name to show: the user's, or the pool label (`MIDI #3`) when the
    /// entry was never named — including every entry of a pre-F-046 project.
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            match self.kind {
                DataKind::Midi => format!("MIDI #{}", self.index),
                DataKind::Curve => format!("Curve #{}", self.index),
            }
        } else {
            self.name.to_string()
        }
    }

    /// Type badge text.
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            DataKind::Midi => "MIDI",
            DataKind::Curve => "Curve",
        }
    }
}

/// Reference counts per pool slot, in one pass over the clips.
fn ref_counts(project: &Project) -> (Vec<usize>, Vec<usize>) {
    let mut midi = vec![0usize; project.midi_data.len()];
    for track in &project.midi_tracks {
        for clip in &track.clips {
            if let Some(slot) = midi.get_mut(clip.data as usize) {
                *slot += 1;
            }
        }
    }
    let mut curve = vec![0usize; project.curve_data.len()];
    for track in &project.control_tracks {
        for clip in &track.clips {
            if let Some(slot) = curve.get_mut(clip.data as usize) {
                *slot += 1;
            }
        }
    }
    (midi, curve)
}

/// Builds the visible rows for the current filter. Order is pool order — the
/// slots themselves are not a stable identity, so there is nothing better to
/// sort on until M2 adds the name column's search.
pub fn rows(project: &Project, filter: DataFilter) -> Vec<DataRow<'_>> {
    let (midi_refs, curve_refs) = ref_counts(project);
    let mut rows = Vec::with_capacity(project.midi_data.len() + project.curve_data.len());
    if filter != DataFilter::Curve {
        for (index, data) in project.midi_data.iter().enumerate() {
            rows.push(midi_row(
                data,
                index,
                midi_refs.get(index).copied().unwrap_or(0),
            ));
        }
    }
    if filter != DataFilter::Midi {
        for (index, data) in project.curve_data.iter().enumerate() {
            rows.push(curve_row(
                data,
                index,
                curve_refs.get(index).copied().unwrap_or(0),
            ));
        }
    }
    rows
}

fn midi_row(data: &MidiData, index: usize, refs: usize) -> DataRow<'_> {
    DataRow {
        kind: DataKind::Midi,
        index: index as u32,
        name: &data.name,
        items: data.note_count(),
        length_ticks: data.length_ticks,
        refs,
    }
}

fn curve_row(data: &CurveData, index: usize, refs: usize) -> DataRow<'_> {
    DataRow {
        kind: DataKind::Curve,
        index: index as u32,
        name: &data.name,
        items: data.points.len(),
        length_ticks: data.length_ticks,
        refs,
    }
}

/// The caption summary: how much of the pool is orphaned (D15).
pub fn summary(project: &Project) -> String {
    let (unused_midi, unused_curve) = project.unused_data_count();
    let unused = unused_midi + unused_curve;
    let mut text = format!(
        "{} MIDI · {} curve",
        project.midi_data.len(),
        project.curve_data.len()
    );
    if unused > 0 {
        text.push_str(&format!(" · {unused} unused"));
    }
    text
}

/// Shows the strip. `open` is the expanded/collapsed flag (D4, view state).
///
/// Must be called **before** `timeline::show`: a bottom panel takes its space
/// out of the available rect, so declaring it afterwards would push it
/// off-screen.
pub fn show(ui: &mut egui::Ui, project: &Project, open: &mut bool, filter: &mut DataFilter) {
    let avail_h = ui.available_height();
    let first_h = (avail_h * DATA_PANEL_HEIGHT_FRACTION).clamp(DATA_PANEL_MIN_H, avail_h);
    let max_h = (avail_h * DATA_PANEL_MAX_FRACTION).max(DATA_PANEL_MIN_H);
    let collapsed = egui::Panel::bottom("glitcherz_data_panel_collapsed");
    // Distinct ids per fold state: `show_switched` persists the expanded
    // height under the expanded id only, so folding does not feed the floor
    // back in on the next open (the Event List's lesson).
    let expanded = egui::Panel::bottom("glitcherz_data_panel_expanded")
        .resizable(true)
        .default_size(first_h)
        .min_size(DATA_PANEL_MIN_H)
        .max_size(max_h);
    let mut open_state = *open;
    let mut toggle = false;
    let caption = summary(project);
    egui::Panel::show_switched(ui, &mut open_state, collapsed, expanded, |ui, expanded| {
        ui.horizontal(|ui| {
            if ui
                .selectable_label(expanded, "Data")
                .on_hover_text(if expanded {
                    "Click to collapse · drag the divider to resize"
                } else {
                    "Click to open the shared data pool"
                })
                .clicked()
            {
                toggle = true;
            }
            ui.weak(&caption);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Right-to-left: listed Curve → MIDI → All so they read
                // All | MIDI | Curve on screen.
                for (value, label) in [
                    (DataFilter::Curve, "Curve"),
                    (DataFilter::Midi, "MIDI"),
                    (DataFilter::All, "All"),
                ] {
                    if ui.selectable_label(*filter == value, label).clicked() {
                        *filter = value;
                    }
                }
            });
        });
        if !expanded {
            return;
        }
        let rows = rows(project, *filter);
        if rows.is_empty() {
            ui.add_space(2.0);
            ui.weak(match filter {
                DataFilter::All => "No shared data yet — draw in a clip to create some.",
                DataFilter::Midi => "No MIDI data.",
                DataFilter::Curve => "No curve data.",
            });
            return;
        }
        let name_w =
            (ui.available_width() - (ID_W + KIND_W + ITEMS_W + LENGTH_W + REFS_W + 24.0)).max(80.0);
        header(ui, name_w);
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H, rows.len(), |ui, range| {
                for row in &rows[range] {
                    table_row(ui, row, name_w);
                }
            });
    });
    if toggle {
        open_state = !open_state;
    }
    *open = open_state;
}

fn header(ui: &mut egui::Ui, name_w: f32) {
    ui.horizontal(|ui| {
        let strong = |ui: &mut egui::Ui, w: f32, text: &str| {
            ui.add_sized(
                [w, ROW_H],
                egui::Label::new(egui::RichText::new(text).small().strong()).truncate(),
            );
        };
        strong(ui, name_w, "Name");
        strong(ui, ID_W, "#id");
        strong(ui, KIND_W, "Type");
        strong(ui, ITEMS_W, "Contents");
        strong(ui, LENGTH_W, "Length");
        strong(ui, REFS_W, "Refs");
    });
}

fn table_row(ui: &mut egui::Ui, row: &DataRow<'_>, name_w: f32) {
    ui.horizontal(|ui| {
        // Orphans are greyed rather than hidden (D15): the row is still the
        // only place `Purge Unused` and a future row-click can act on.
        let text = |s: String| {
            let rich = egui::RichText::new(s).small().monospace();
            if row.refs == 0 { rich.weak() } else { rich }
        };
        ui.add_sized(
            [name_w, ROW_H],
            egui::Label::new(text(row.label())).truncate(),
        );
        ui.add_sized(
            [ID_W, ROW_H],
            egui::Label::new(text(format!("#{}", row.index))).truncate(),
        );
        ui.add_sized(
            [KIND_W, ROW_H],
            egui::Label::new(text(row.kind_label().to_string())).truncate(),
        );
        ui.add_sized(
            [ITEMS_W, ROW_H],
            egui::Label::new(text(row.items.to_string())).truncate(),
        );
        ui.add_sized(
            [LENGTH_W, ROW_H],
            egui::Label::new(text(format!("{} bars", bars(row.length_ticks)))).truncate(),
        );
        ui.add_sized(
            [REFS_W, ROW_H],
            egui::Label::new(text(row.refs.to_string())).truncate(),
        );
    });
}

/// `length_ticks` shown in bars. The pool has no meter of its own (a window
/// is placed per clip), so the figure is the 4/4 reading — it exists to
/// compare entries, not to place anything.
fn bars(length_ticks: u32) -> String {
    let ticks = length_ticks.max(1) as f64;
    let bars = ticks / glitcherz_model::TICKS_PER_BAR as f64;
    if (bars - bars.round()).abs() < 1e-3 {
        format!("{}", bars.round() as u32)
    } else {
        format!("{bars:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unnamed_entries_fall_back_to_the_pool_slot() {
        let midi = MidiData::default();
        let row = midi_row(&midi, 3, 1);
        assert_eq!(row.label(), "MIDI #3");
    }

    #[test]
    fn named_entries_show_the_name() {
        let midi = MidiData {
            name: "Verse drums".into(),
            ..MidiData::default()
        };
        let row = midi_row(&midi, 3, 1);
        assert_eq!(row.label(), "Verse drums");
    }

    #[test]
    fn summary_counts_orphans() {
        let mut project = Project::default();
        project.midi_tracks.push(Default::default());
        let data = project.push_midi_data(MidiData::default());
        project.push_midi_data(MidiData::default());
        project.midi_tracks[0]
            .clips
            .push(glitcherz_model::MidiClip {
                data,
                ..Default::default()
            });
        assert_eq!(project.unused_data_count(), (1, 0));
        assert!(summary(&project).contains("1 unused"));
    }

    #[test]
    fn filter_hides_the_other_pool() {
        let mut project = Project::default();
        project.push_midi_data(MidiData::default());
        project.push_curve_data(CurveData::default());
        assert_eq!(rows(&project, DataFilter::All).len(), 2);
        assert_eq!(rows(&project, DataFilter::Midi).len(), 1);
        assert_eq!(rows(&project, DataFilter::Curve).len(), 1);
    }

    #[test]
    fn reference_counts_follow_every_clip() {
        let mut project = Project::default();
        // `Project::default()` already ships tracks; start from a known count
        // so the reference tally below is about this clip pair only.
        project.midi_tracks.clear();
        project.midi_tracks.push(Default::default());
        project.midi_tracks.push(Default::default());
        let data = project.push_midi_data(MidiData::default());
        for track in project.midi_tracks.iter_mut() {
            track.clips.push(glitcherz_model::MidiClip {
                data,
                ..Default::default()
            });
        }
        let rows = rows(&project, DataFilter::All);
        assert_eq!(rows[0].refs, 2);
        assert_eq!(project.midi_data_users(data), vec![(0, 0), (1, 0)]);
    }
}
