//! Clip panel — the properties of the *active* clip (F-046 R2).
//!
//! A bottom strip **inside the timeline view** (D3), with the same folded /
//! expanded pair as the Edit tab's Event List and the same draggable divider.
//! It replaces the pool table of M1–M3: a shared-data list is a place to
//! browse, not a place to work, and the thing a person edits while arranging
//! is a clip.
//!
//! Blender's Properties is the model. The active clip is the last one picked
//! (`selection_anchor`); a multi-selection still shows one clip — the one the
//! gestures act on — with the count in the caption. One row of that panel is
//! the clip's **data block**: a `▾` picker listing every data entry of the
//! same kind, the entry's name (editing it renames it for every clip that
//! shares it, like renaming a material), and the reference count, which is a
//! button that gives this clip a private copy.
//!
//! Read-only over the document: the panel borrows `Project` and leaves every
//! edit through [`ClipIntent`], so the app keeps the undo boundaries. Nothing
//! here mutates the project.

use eframe::egui;
use glitcherz_model::{CurveData, MidiData, Project};

use crate::timeline::{ClipSel, TimelineLane};

/// Share of the tab the strip takes when first opened (Event List's value).
pub const CLIP_PANEL_HEIGHT_FRACTION: f32 = 1.0 / 3.0;
/// Caption row plus a couple of property rows: the divider's floor.
pub const CLIP_PANEL_MIN_H: f32 = 26.0;
/// Ceiling as a share of the tab, so the arrangement keeps a usable band.
const CLIP_PANEL_MAX_FRACTION: f32 = 0.6;
/// Height of one property row.
const ROW_H: f32 = 20.0;
/// Width of the label column.
const LABEL_W: f32 = 64.0;
/// Height of one entry in the data picker.
const PICK_ROW_H: f32 = 18.0;

/// Which pool a data block belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataKind {
    Midi,
    Curve,
}

impl DataKind {
    fn label(self) -> &'static str {
        match self {
            DataKind::Midi => "MIDI",
            DataKind::Curve => "Curve",
        }
    }
}

/// The panel's own view state (never written to the project).
#[derive(Debug, Clone, Default)]
pub struct ClipPanelState {
    /// Expanded / collapsed (D4). Collapsed on first run.
    pub open: bool,
    /// The field currently being edited — clip name or data name.
    pub rename: Option<RenameDraft>,
    /// Filter text of the data picker popup.
    pub pick_search: String,
}

/// What a name field edits. Only one field is in flight at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameTarget {
    Clip(ClipSel),
    Data(DataKind, u32),
}

/// One in-progress name edit.
#[derive(Debug, Clone)]
pub struct RenameDraft {
    pub target: RenameTarget,
    pub draft: String,
    /// The editor is created one frame after the click, so the first frame
    /// that draws it has to ask for focus explicitly.
    pub focus: bool,
}

/// What the panel wants the document to do. Every variant is one undo step,
/// applied by the app; the panel itself stays a view.
#[derive(Debug, Clone, PartialEq)]
pub enum ClipIntent {
    /// Rename the active clip.
    RenameClip { sel: ClipSel, name: String },
    /// Rename the shared data — every clip that points at it sees the change.
    RenameData {
        kind: DataKind,
        index: u32,
        name: String,
    },
    /// Point the clip at another data block of the same kind.
    Relink {
        sel: ClipSel,
        kind: DataKind,
        index: u32,
    },
    /// Give the clip a private copy of its shared data (`refs > 1` only).
    MakeSingleUser { sel: ClipSel },
    /// Toggle the clip's own mute (B2).
    SetMuted { sel: ClipSel, muted: bool },
    /// Drop pool entries nothing points at.
    Purge,
}

/// The data block a MIDI / curve clip points at, resolved for display.
#[derive(Debug, Clone, PartialEq)]
pub struct DataInfo {
    pub kind: DataKind,
    pub index: u32,
    /// The user's name; empty when the entry was never named.
    pub name: String,
    /// Notes (MIDI) or anchors (curve).
    pub items: usize,
    pub length_ticks: u32,
    /// How many clips point at this entry.
    pub refs: usize,
}

impl DataInfo {
    /// Shown when the entry has no name: the slot label, same wording the
    /// clip badges use.
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            format!("{} #{}", self.kind.label(), self.index)
        } else {
            self.name.clone()
        }
    }
}

/// The audio clip's source, resolved for display.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioInfo {
    pub path: String,
    pub source_len_secs: f64,
}

/// The active clip, resolved into owned display data for one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveClip {
    pub sel: ClipSel,
    pub track_name: String,
    /// The clip's own name; empty when unnamed.
    pub name: String,
    pub muted: bool,
    pub group: Option<u32>,
    pub start_ticks: u32,
    pub span_ticks: u32,
    pub data: Option<DataInfo>,
    pub audio: Option<AudioInfo>,
}

impl ActiveClip {
    pub fn lane_label(&self) -> &'static str {
        match self.sel.lane {
            TimelineLane::Midi => "MIDI",
            TimelineLane::Control => "Control",
            TimelineLane::Audio => "Audio",
        }
    }

    /// The track's name, or `MIDI 2` when it was never named.
    pub fn track_label(&self) -> String {
        if self.track_name.is_empty() {
            format!("{} {}", self.lane_label(), self.sel.track + 1)
        } else {
            self.track_name.clone()
        }
    }

    /// What the caption calls this clip: its name, else the data name, else
    /// the lane. Never the pool slot — that one moves on save.
    pub fn display_name(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        if let Some(data) = &self.data {
            return data.label();
        }
        self.audio
            .as_ref()
            .and_then(|audio| {
                std::path::Path::new(&audio.path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| self.lane_label().to_string())
    }
}

/// Blender's active object: the last clip that was picked, provided it is
/// still part of the selection. `selection_anchor` is written on every plain /
/// Ctrl click, so it is exactly "what the next gesture acts on".
pub fn active_sel(selection: &[ClipSel], anchor: Option<ClipSel>) -> Option<ClipSel> {
    match anchor {
        Some(anchor) if selection.contains(&anchor) => Some(anchor),
        _ => selection.last().copied(),
    }
}

/// Resolves the active clip against the document. `None` when nothing is
/// selected (or the addressed clip has gone away).
pub fn active_clip(
    project: &Project,
    selection: &[ClipSel],
    anchor: Option<ClipSel>,
) -> Option<ActiveClip> {
    let sel = active_sel(selection, anchor)?;
    match sel.lane {
        TimelineLane::Midi => {
            let track = project.midi_tracks.get(sel.track)?;
            let clip = track.clips.get(sel.clip)?;
            Some(ActiveClip {
                sel,
                track_name: track.name.clone(),
                name: clip.name.clone(),
                muted: clip.muted,
                group: clip.group,
                start_ticks: clip.start_ticks,
                span_ticks: clip.span_ticks,
                data: Some(DataInfo {
                    kind: DataKind::Midi,
                    index: clip.data,
                    name: project
                        .midi_data
                        .get(clip.data as usize)
                        .map(|data| data.name.clone())
                        .unwrap_or_default(),
                    items: project
                        .midi_data
                        .get(clip.data as usize)
                        .map_or(0, |data| data.note_count()),
                    length_ticks: project
                        .midi_data
                        .get(clip.data as usize)
                        .map_or(0, |data| data.length_ticks),
                    refs: project.midi_data_refs(clip.data),
                }),
                audio: None,
            })
        }
        TimelineLane::Control => {
            let track = project.control_tracks.get(sel.track)?;
            let clip = track.clips.get(sel.clip)?;
            Some(ActiveClip {
                sel,
                track_name: track.name.clone(),
                name: clip.name.clone(),
                muted: clip.muted,
                group: clip.group,
                start_ticks: clip.start_ticks,
                span_ticks: clip.span_ticks,
                data: Some(DataInfo {
                    kind: DataKind::Curve,
                    index: clip.data,
                    name: project
                        .curve_data
                        .get(clip.data as usize)
                        .map(|data| data.name.clone())
                        .unwrap_or_default(),
                    items: project
                        .curve_data
                        .get(clip.data as usize)
                        .map_or(0, |data| data.points.len()),
                    length_ticks: project
                        .curve_data
                        .get(clip.data as usize)
                        .map_or(0, |data| data.length_ticks),
                    refs: project.curve_data_refs(clip.data),
                }),
                audio: None,
            })
        }
        TimelineLane::Audio => {
            let track = project.audio_tracks.get(sel.track)?;
            let clip = track.clips.get(sel.clip)?;
            Some(ActiveClip {
                sel,
                track_name: track.name.clone(),
                name: clip.name.clone(),
                muted: clip.muted,
                group: clip.group,
                start_ticks: clip.start_ticks,
                span_ticks: (clip.source_len / project.sec_per_tick().max(1e-9)) as u32,
                data: None,
                audio: Some(AudioInfo {
                    path: clip.path.clone(),
                    source_len_secs: clip.source_len,
                }),
            })
        }
    }
}

/// The caption summary: which clip the panel is showing (D4).
pub fn caption(active: Option<&ActiveClip>, selected: usize) -> String {
    let Some(active) = active else {
        return "nothing selected".to_string();
    };
    let mut text = format!(
        "{} · {} · {}",
        active.track_label(),
        active.lane_label(),
        active.display_name()
    );
    if selected > 1 {
        text.push_str(&format!(" · {selected} selected"));
    }
    text
}

/// A data entry as offered by the picker.
struct PickEntry {
    index: u32,
    label: String,
    items: usize,
}

/// Every entry of one pool, with the counts the picker shows. Reference
/// counts come from one pass over the clips, not one pass per row.
fn pick_entries(project: &Project, kind: DataKind, needle: &str) -> Vec<PickEntry> {
    let needle = needle.trim().to_lowercase();
    let mut entries = Vec::new();
    match kind {
        DataKind::Midi => {
            for (index, data) in project.midi_data.iter().enumerate() {
                let label = midi_label(data, index);
                if needle.is_empty() || label.to_lowercase().contains(&needle) {
                    entries.push(PickEntry {
                        index: index as u32,
                        label,
                        items: data.note_count(),
                    });
                }
            }
        }
        DataKind::Curve => {
            for (index, data) in project.curve_data.iter().enumerate() {
                let label = curve_label(data, index);
                if needle.is_empty() || label.to_lowercase().contains(&needle) {
                    entries.push(PickEntry {
                        index: index as u32,
                        label,
                        items: data.points.len(),
                    });
                }
            }
        }
    }
    entries
}

fn midi_label(data: &MidiData, index: usize) -> String {
    if data.name.is_empty() {
        format!("MIDI #{index}")
    } else {
        data.name.clone()
    }
}

fn curve_label(data: &CurveData, index: usize) -> String {
    if data.name.is_empty() {
        format!("Curve #{index}")
    } else {
        data.name.clone()
    }
}

/// Shows the strip and returns at most one intent per frame.
///
/// Must be called **before** `timeline::show`: a bottom panel takes its space
/// out of the available rect, so declaring it afterwards would push it
/// off-screen.
pub fn show(
    ui: &mut egui::Ui,
    project: &Project,
    selection: &[ClipSel],
    anchor: Option<ClipSel>,
    state: &mut ClipPanelState,
) -> Option<ClipIntent> {
    let mut intent = None;
    let avail_h = ui.available_height();
    let first_h = (avail_h * CLIP_PANEL_HEIGHT_FRACTION).clamp(CLIP_PANEL_MIN_H, avail_h);
    let max_h = (avail_h * CLIP_PANEL_MAX_FRACTION).max(CLIP_PANEL_MIN_H);
    let collapsed = egui::Panel::bottom("glitcherz_clip_panel_collapsed");
    // Distinct ids per fold state: `show_switched` persists the expanded
    // height under the expanded id only, so folding does not feed the floor
    // back in on the next open (the Event List's lesson).
    let expanded = egui::Panel::bottom("glitcherz_clip_panel_expanded")
        .resizable(true)
        .default_size(first_h)
        .min_size(CLIP_PANEL_MIN_H)
        .max_size(max_h);
    let active = active_clip(project, selection, anchor);
    let caption = caption(active.as_ref(), selection.len());
    let (unused_midi, unused_curve) = project.unused_data_count();
    let has_orphans = unused_midi + unused_curve > 0;
    let mut open_state = state.open;
    let mut toggle = false;
    egui::Panel::show_switched(ui, &mut open_state, collapsed, expanded, |ui, expanded| {
        ui.horizontal(|ui| {
            if ui
                .selectable_label(expanded, "Clip")
                .on_hover_text(if expanded {
                    "Click to collapse · drag the divider to resize"
                } else {
                    "Click to open the active clip's properties"
                })
                .clicked()
            {
                toggle = true;
            }
            ui.weak(&caption);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // D15: the pool has no per-entry delete — compaction is the
                // one remapping implementation, and it runs on save too.
                let purge = ui
                    .add_enabled(
                        has_orphans,
                        egui::Button::new(format!("Purge {} unused", unused_midi + unused_curve))
                            .small(),
                    )
                    .on_hover_text("Remove data that no clip points at (the save-time compaction)");
                if purge.clicked() {
                    intent = Some(ClipIntent::Purge);
                }
            });
        });
        if !expanded {
            return;
        }
        let Some(active) = &active else {
            ui.add_space(2.0);
            ui.weak("Select a clip to see its properties.");
            return;
        };
        // The pad can be dragged short; the rows scroll rather than be cut.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(next) = properties(ui, project, active, state) {
                    intent = Some(next);
                }
            });
    });
    if toggle {
        open_state = !open_state;
    }
    state.open = open_state;
    intent
}

/// The property rows of one clip (R3).
fn properties(
    ui: &mut egui::Ui,
    project: &Project,
    active: &ActiveClip,
    state: &mut ClipPanelState,
) -> Option<ClipIntent> {
    let mut intent = None;
    let meter = project.meter_map();
    let (bar, beat) = meter.bar_beat_at(active.start_ticks);
    let bar_ticks = meter.ticks_per_bar_at(active.start_ticks).max(1);

    row(ui, "Name", |ui| {
        let width = (ui.available_width() - 8.0).max(90.0);
        if let Some(next) = name_field(
            ui,
            egui::Id::unique(("clip_name", active.sel.track, active.sel.clip)),
            &active.name,
            RenameTarget::Clip(active.sel),
            state,
            width,
            "Clip name",
        ) {
            intent = Some(next);
        }
    });
    row(ui, "Track", |ui| {
        ui.add_sized(
            [ui.available_width(), ROW_H],
            egui::Label::new(egui::RichText::new(active.track_label()).small().weak()),
        );
    });
    row(ui, "Start", |ui| {
        ui.weak(format!("{}.{}", bar + 1, beat + 1))
            .on_hover_text("Bar.beat, per the meter in effect at the clip start");
    });
    row(ui, "Length", |ui| {
        ui.weak(format!("{} bars", bars_of(active.span_ticks, bar_ticks)));
    });
    row(ui, "Muted", |ui| {
        let mut muted = active.muted;
        let text = if active.muted { "muted" } else { "on" };
        if ui
            .checkbox(&mut muted, text)
            .on_hover_text("Silences this clip without moving or trimming it")
            .changed()
        {
            intent = Some(ClipIntent::SetMuted {
                sel: active.sel,
                muted,
            });
        }
    });
    row(ui, "Group", |ui| match active.group {
        Some(group) => {
            ui.weak(format!("G{group}"));
        }
        None => {
            ui.weak("—");
        }
    });

    if let Some(data) = &active.data {
        row(ui, "Data", |ui| {
            if let Some(next) = data_row(ui, project, active, data, state) {
                intent = Some(next);
            }
        });
        // R7: the old table's `Contents` / `Length` columns become the caption
        // of the row they belonged to.
        row(ui, "", |ui| {
            ui.add_sized(
                [ui.available_width(), ROW_H],
                egui::Label::new(
                    egui::RichText::new(format!(
                        "{} · {} {} · {}",
                        data.kind.label(),
                        data.items,
                        if data.kind == DataKind::Midi {
                            "notes"
                        } else {
                            "anchors"
                        },
                        bars_label(data.length_ticks, glitcherz_model::TICKS_PER_BAR.max(1))
                    ))
                    .small()
                    .weak(),
                )
                .truncate(),
            );
        });
    } else if let Some(audio) = &active.audio {
        row(ui, "File", |ui| {
            let name = std::path::Path::new(&audio.path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&audio.path);
            ui.add_sized(
                [ui.available_width(), ROW_H],
                egui::Label::new(egui::RichText::new(name).small()).truncate(),
            )
            .on_hover_text(&audio.path);
        });
        row(ui, "Source", |ui| {
            ui.weak(format!("{:.2} s", audio.source_len_secs));
        });
    }
    intent
}

/// One `label | content` line. A hand-rolled row rather than a `Grid`: the
/// content widths here depend on what is left of the row (text fields fill
/// it), and a grid sizes its columns to content instead.
fn row(ui: &mut egui::Ui, text: &str, content: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [LABEL_W, ROW_H],
            egui::Label::new(egui::RichText::new(text).small().weak()),
        );
        content(ui);
    });
}

/// The data block row (R4/R5/R6): picker, editable name, reference count.
fn data_row(
    ui: &mut egui::Ui,
    project: &Project,
    active: &ActiveClip,
    data: &DataInfo,
    state: &mut ClipPanelState,
) -> Option<ClipIntent> {
    let mut intent = None;
    ui.horizontal(|ui| {
        let arrow = ui
            .small_button("▾")
            .on_hover_text("Pick another data block of the same kind");
        let refs_w = 34.0;
        let width = (ui.available_width() - refs_w - 12.0).max(90.0);
        if let Some(next) = name_field(
            ui,
            egui::Id::unique(("clip_data_name", data.kind.label(), data.index)),
            &data.name,
            RenameTarget::Data(data.kind, data.index),
            state,
            width,
            "Data name — renames it for every clip that shares it",
        ) {
            intent = Some(next);
        }
        let refs = ui
            .add_enabled(
                data.refs > 1,
                egui::Button::new(format!("{}", data.refs)).small(),
            )
            .on_hover_text(if data.refs > 1 {
                "Make a private copy (unlink from the other clips)"
            } else {
                "Only this clip uses this data"
            });
        if refs.clicked() {
            intent = Some(ClipIntent::MakeSingleUser { sel: active.sel });
        }

        // R4: every entry of the same pool, searchable, since a project can
        // hold thousands (`MAX_CLIP_DATA_ENTRIES`).
        egui::Popup::menu(&arrow)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_min_width(240.0);
                ui.add(
                    egui::TextEdit::singleline(&mut state.pick_search)
                        .hint_text("Search…")
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Small),
                );
                let entries = pick_entries(project, data.kind, &state.pick_search);
                if entries.is_empty() {
                    ui.weak("No match.");
                    return;
                }
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .auto_shrink([false, true])
                    .show_rows(ui, PICK_ROW_H, entries.len(), |ui, range| {
                        for entry in &entries[range] {
                            let current = entry.index == data.index;
                            let text = format!("{} · {}", entry.label, entry.items);
                            if ui
                                .selectable_label(current, egui::RichText::new(text).small())
                                .clicked()
                            {
                                if !current {
                                    intent = Some(ClipIntent::Relink {
                                        sel: active.sel,
                                        kind: data.kind,
                                        index: entry.index,
                                    });
                                }
                                egui::Popup::close_all(ui.ctx());
                            }
                        }
                    });
            });
    });
    intent
}

/// A name field: always looks like a field (R5 — an inspector's names are
/// editable in place, there is no double-click ritual), commits on `Enter` or
/// on losing focus, cancels on `Esc`.
fn name_field(
    ui: &mut egui::Ui,
    id: egui::Id,
    current: &str,
    target: RenameTarget,
    state: &mut ClipPanelState,
    width: f32,
    hint: &str,
) -> Option<ClipIntent> {
    let editing = state
        .rename
        .as_ref()
        .is_some_and(|draft| draft.target == target);
    let mut text = match &state.rename {
        Some(draft) if editing => draft.draft.clone(),
        _ => current.to_string(),
    };
    let response = ui.add_sized(
        [width, ROW_H],
        egui::TextEdit::singleline(&mut text)
            .id(id)
            .hint_text(hint)
            .font(egui::TextStyle::Small)
            .margin(egui::Margin::symmetric(4, 2)),
    );
    if response.gained_focus() || response.changed() {
        state.rename = Some(RenameDraft {
            target,
            draft: text,
            focus: false,
        });
    }
    // Esc drops focus as well, so it has to be read before `lost_focus`.
    let escaped = response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape));
    if escaped {
        state.rename = None;
        return None;
    }
    if !response.lost_focus() {
        return None;
    }
    let draft = state.rename.take()?;
    if draft.target != target || draft.draft == current {
        return None;
    }
    Some(match target {
        RenameTarget::Clip(sel) => ClipIntent::RenameClip {
            sel,
            name: draft.draft,
        },
        RenameTarget::Data(kind, index) => ClipIntent::RenameData {
            kind,
            index,
            name: draft.draft,
        },
    })
}

/// `span / bar` in bars, one decimal when it is not a whole number.
fn bars_of(span_ticks: u32, bar_ticks: u32) -> String {
    let bars = span_ticks as f64 / bar_ticks.max(1) as f64;
    if (bars - bars.round()).abs() < 1e-3 {
        format!("{}", bars.round() as u32)
    } else {
        format!("{bars:.1}")
    }
}

/// Same, with the unit spelled out (the data caption's tail).
fn bars_label(length_ticks: u32, bar_ticks: u32) -> String {
    format!("{} bars", bars_of(length_ticks.max(1), bar_ticks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use glitcherz_model::{MidiClip, MidiData};

    fn sel(lane: TimelineLane, track: usize, clip: usize) -> ClipSel {
        ClipSel { lane, track, clip }
    }

    #[test]
    fn the_anchor_is_the_active_clip() {
        let first = sel(TimelineLane::Midi, 0, 0);
        let second = sel(TimelineLane::Midi, 0, 1);
        assert_eq!(active_sel(&[first, second], Some(first)), Some(first));
    }

    #[test]
    fn a_deselected_anchor_falls_back_to_the_last_one() {
        let first = sel(TimelineLane::Midi, 0, 0);
        let second = sel(TimelineLane::Midi, 0, 1);
        // Ctrl-click removed `first` from the selection; the anchor still
        // points at it, so it must not win.
        assert_eq!(active_sel(&[second], Some(first)), Some(second));
        assert_eq!(active_sel(&[], Some(first)), None);
    }

    #[test]
    fn unnamed_data_falls_back_to_the_slot_label() {
        let info = DataInfo {
            kind: DataKind::Midi,
            index: 3,
            name: String::new(),
            items: 0,
            length_ticks: 0,
            refs: 1,
        };
        assert_eq!(info.label(), "MIDI #3");
    }

    #[test]
    fn the_active_clip_resolves_its_data_and_references() {
        let mut project = Project::default();
        project.midi_tracks.clear();
        project.midi_tracks.push(Default::default());
        project.midi_tracks.push(Default::default());
        let data = project.push_midi_data(MidiData {
            name: "Verse drums".into(),
            length_ticks: glitcherz_model::TICKS_PER_BAR,
            ..MidiData::default()
        });
        for track in project.midi_tracks.iter_mut() {
            track.clips.push(MidiClip {
                data,
                start_ticks: glitcherz_model::TICKS_PER_BAR,
                ..Default::default()
            });
        }
        let active = active_clip(
            &project,
            &[sel(TimelineLane::Midi, 1, 0)],
            Some(sel(TimelineLane::Midi, 1, 0)),
        )
        .expect("the clip resolves");
        let info = active.data.expect("a MIDI clip has a data block");
        assert_eq!(info.label(), "Verse drums");
        assert_eq!(info.refs, 2);
        assert_eq!(info.length_ticks, glitcherz_model::TICKS_PER_BAR);
        // Start is the meter-aware bar.beat the panel shows.
        let (bar, beat) = project.meter_map().bar_beat_at(active.start_ticks);
        assert_eq!((bar, beat), (1, 0));
    }

    #[test]
    fn the_picker_lists_one_pool_and_honours_the_search() {
        let mut project = Project::default();
        project.push_midi_data(MidiData {
            name: "Verse".into(),
            ..MidiData::default()
        });
        project.push_midi_data(MidiData::default());
        project.push_curve_data(CurveData::default());
        assert_eq!(pick_entries(&project, DataKind::Midi, "").len(), 2);
        assert_eq!(pick_entries(&project, DataKind::Midi, "verse").len(), 1);
        // An unnamed entry still matches its displayed slot label.
        assert_eq!(pick_entries(&project, DataKind::Midi, "midi #1").len(), 1);
        assert_eq!(pick_entries(&project, DataKind::Curve, "").len(), 1);
    }

    #[test]
    fn the_caption_names_the_clip_and_counts_the_selection() {
        let mut project = Project::default();
        project.midi_tracks.clear();
        project.midi_tracks.push(Default::default());
        let data = project.push_midi_data(MidiData {
            name: "Verse drums".into(),
            ..MidiData::default()
        });
        project.midi_tracks[0].clips.push(MidiClip {
            data,
            ..Default::default()
        });
        let selection = vec![sel(TimelineLane::Midi, 0, 0)];
        let active = active_clip(&project, &selection, selection.first().copied());
        let text = caption(active.as_ref(), 3);
        assert!(text.contains("Verse drums"), "{text}");
        assert!(text.contains("3 selected"), "{text}");
    }
}
