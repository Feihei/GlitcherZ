//! Plugin picker: the filterable popup behind `Add ▸ Instrument…` /
//! `Add ▸ Effect…`.
//!
//! Spec: `.agents/spec/plugin-browser.md` (U9 E3, v3 E6). A popup rather than
//! a `ui.menu_button` submenu, because the leaf layer needs a text field, a
//! scrolling list and keyboard navigation — none of which egui submenus
//! handle well (spec D4/D5).
//!
//! One list, two data sources (spec D15): the **built-in registry** (Synth /
//! SoundFont / Sample instruments, the built-in effects) and the plugin
//! **catalog** the startup scan filled. Every row carries a [`PickTag`], so
//! the list is grouped `Built-in → CLAP → VST3`.
//!
//! Two filters run over it (spec D19): the free-text query and the row's own
//! tag, exposed as a strip of `BUILT-IN / CLAP / VST3` toggles under the
//! query field. They AND together; both are pure functions
//! ([`rows`]) so they can be tested without a display.
//!
//! The popup never touches the disk and knows nothing about node creation: a
//! row already holds the [`NodeSource`] it stands for.

use serde::{Deserialize, Serialize};

use eframe::egui;

use glitcherz_model::BuiltinEffectKind;

use crate::patch::NodeSource;
use crate::plugin_catalog::{PluginEntry, PluginFormat};

/// How many entries each category keeps in the recent list (spec D10).
pub const RECENT_MAX: usize = 5;

const POPUP_WIDTH: f32 = 420.0;
const LIST_MAX_HEIGHT: f32 = 300.0;
const ROW_HEIGHT: f32 = 22.0;
/// Width reserved for the right-aligned source tag (`BUILT-IN` is the widest).
const TAG_WIDTH: f32 = 58.0;

/// Stable id for the query field (an auto-generated id would move with the
/// popup and drop focus on open).
fn query_id() -> egui::Id {
    egui::Id::new("glitcherz_plugin_picker_query")
}

/// Where a row comes from (spec D15): the right-aligned tag, and the key the
/// list groups on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PickTag {
    /// Built into the app — no module to load.
    BuiltIn,
    /// CLAP module.
    Clap,
    /// VST3 module.
    Vst3,
}

impl PickTag {
    /// Every tag, in list order (`Built-in → CLAP → VST3`).
    pub const ALL: [Self; 3] = [Self::BuiltIn, Self::Clap, Self::Vst3];

    /// Bit standing for this tag inside a [`PickTags`] set.
    fn bit(self) -> u8 {
        1 << (self as u8)
    }

    /// Row chip text.
    pub fn label(self) -> &'static str {
        match self {
            PickTag::BuiltIn => "BUILT-IN",
            PickTag::Clap => "CLAP",
            PickTag::Vst3 => "VST3",
        }
    }

    /// Group heading above the first row of this tag.
    fn heading(self) -> &'static str {
        match self {
            PickTag::BuiltIn => "Built-in",
            PickTag::Clap => "CLAP",
            PickTag::Vst3 => "VST3",
        }
    }
}

/// A set of source tags — the tag toggles beside the query field (spec D19).
///
/// A small bitmask rather than three bools: `PickFilter` stays `Copy`, and
/// "is this tag on" is one question at every filter site instead of a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickTags(u8);

impl Default for PickTags {
    fn default() -> Self {
        Self::ALL
    }
}

impl PickTags {
    /// Every source on — the state a picker opens with.
    pub const ALL: Self = Self(0b111);

    /// No source at all (start of a "collect what exists" pass).
    pub const NONE: Self = Self(0);

    /// `true` while this source is allowed to appear.
    pub fn has(self, tag: PickTag) -> bool {
        self.0 & tag.bit() != 0
    }

    /// Turns one source on or off.
    pub fn set(&mut self, tag: PickTag, on: bool) {
        if on {
            self.0 |= tag.bit();
        } else {
            self.0 &= !tag.bit();
        }
    }

    /// Flips one source (what a chip click does).
    pub fn toggle(&mut self, tag: PickTag) {
        self.set(tag, !self.has(tag));
    }

    /// How many sources are on (the chip strip hides itself below 2).
    pub fn count(self) -> u32 {
        self.0.count_ones()
    }

    /// `true` when every source is on (the empty state offers a reset then).
    pub fn is_all(self) -> bool {
        self == Self::ALL
    }
}

/// One pickable row: what it creates, plus how it is labelled and grouped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickChoice {
    /// Row caption.
    pub name: String,
    /// Source tag (chip + section key).
    pub tag: PickTag,
    /// Blueprint of the node the pick creates — no second mapping step.
    pub source: NodeSource,
}

impl PickChoice {
    /// Hover text: the module path for a plugin, a one-liner for a built-in.
    pub fn tooltip(&self) -> String {
        match &self.source {
            NodeSource::ClapInstrument { path, .. }
            | NodeSource::Vst3Instrument { path, .. }
            | NodeSource::ClapEffect { path, .. }
            | NodeSource::Vst3Effect { path, .. } => path.clone(),
            NodeSource::BuiltinSynth => "Built-in synth (sine + filter + ADSR)".to_string(),
            NodeSource::SoundFont { .. } => {
                "SoundFont (.sf2 / .sf3) — the file is picked on the node".to_string()
            }
            NodeSource::Sample { .. } => {
                "Wav sample player — the file is picked on the node".to_string()
            }
            NodeSource::Builtin(kind) => format!("Built-in effect: {}", kind.label()),
            NodeSource::Mixer | NodeSource::Feedback | NodeSource::Keyboard => String::new(),
        }
    }
}

/// Built-in instrument rows, in menu order (spec D15). A new built-in
/// instrument is one entry here — neither the menu nor the picker changes.
pub fn builtin_instruments() -> Vec<PickChoice> {
    vec![
        PickChoice {
            name: "Synth".to_string(),
            tag: PickTag::BuiltIn,
            source: NodeSource::BuiltinSynth,
        },
        // Born without a file on purpose: the node carries its own
        // `Load File…` button, so adding one never blocks on a dialog and a
        // file can be swapped later without rebuilding the node (and its
        // cables).
        PickChoice {
            name: "SoundFont".to_string(),
            tag: PickTag::BuiltIn,
            source: NodeSource::SoundFont {
                path: String::new(),
            },
        },
        PickChoice {
            name: "Sample".to_string(),
            tag: PickTag::BuiltIn,
            source: NodeSource::Sample {
                path: String::new(),
            },
        },
    ]
}

/// What a picker instance may show (spec §6.1: opened from `Instrument…`,
/// `Effect…`, or the global `Search Nodes…`).
///
/// Two independent axes: the **category** the menu leaf asked for, and the
/// **sources** the user left switched on in the chip strip (D19). The query
/// is the third, kept on [`PluginPicker`] because it is per-popup state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickFilter {
    /// `Some(true)` = instruments only, `Some(false)` = effects only,
    /// `None` = the global `Search Nodes…` entry (both).
    pub want_instrument: Option<bool>,
    /// Sources allowed to appear (`BUILT-IN` / `CLAP` / `VST3`).
    pub tags: PickTags,
}

impl Default for PickFilter {
    fn default() -> Self {
        Self {
            want_instrument: None,
            tags: PickTags::ALL,
        }
    }
}

impl PickFilter {
    /// A category leaf (`Instrument…` / `Effect…`) or the global entry
    /// (`None`), with every source switched on.
    pub fn category(want_instrument: Option<bool>) -> Self {
        Self {
            want_instrument,
            ..Self::default()
        }
    }

    fn admits(&self, is_instrument: bool) -> bool {
        self.want_instrument
            .is_none_or(|wanted| wanted == is_instrument)
    }

    /// Heading shown above the filter field.
    fn heading(&self) -> &'static str {
        match self.want_instrument {
            Some(true) => "Instruments",
            Some(false) => "Effects",
            None => "All nodes",
        }
    }
}

/// Which chip a catalog entry belongs to.
fn tag_of(format: PluginFormat) -> PickTag {
    match format {
        PluginFormat::Clap => PickTag::Clap,
        PluginFormat::Vst3 => PickTag::Vst3,
    }
}

/// A pickable plugin remembered from an earlier session (recents must survive
/// a plugin disappearing from disk — selecting a stale entry simply fails to
/// instantiate and reports it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentPlugin {
    /// Binary format.
    pub format: PluginFormat,
    /// Module path.
    pub path: String,
    /// CLAP plugin id / VST3 class id.
    pub id: String,
    /// Display name at the time of the pick.
    pub name: String,
    /// Instrument or effect.
    pub is_instrument: bool,
}

impl RecentPlugin {
    fn as_choice(&self) -> PickChoice {
        plugin_choice(
            self.name.clone(),
            self.format,
            self.path.clone(),
            self.id.clone(),
            self.is_instrument,
        )
    }
}

/// One plugin row, built from the `(format, is_instrument)` pair the catalog
/// classifies on (spec D9/D15).
fn plugin_choice(
    name: String,
    format: PluginFormat,
    path: String,
    id: String,
    is_instrument: bool,
) -> PickChoice {
    let tag = tag_of(format);
    let source = match (format, is_instrument) {
        (PluginFormat::Clap, true) => NodeSource::ClapInstrument {
            path,
            plugin_id: id,
        },
        (PluginFormat::Vst3, true) => NodeSource::Vst3Instrument { path, class_id: id },
        (PluginFormat::Clap, false) => NodeSource::ClapEffect {
            path,
            plugin_id: id,
        },
        (PluginFormat::Vst3, false) => NodeSource::Vst3Effect { path, class_id: id },
    };
    PickChoice { name, tag, source }
}

impl From<&PluginEntry> for PickChoice {
    fn from(entry: &PluginEntry) -> Self {
        plugin_choice(
            entry.name.clone(),
            entry.format,
            entry.path.clone(),
            entry.id.clone(),
            entry.is_instrument,
        )
    }
}

/// Pushes a pick to the front of the recent list, deduplicating by identity
/// and capping each category at [`RECENT_MAX`] (spec D10).
///
/// Built-in picks are not remembered: they are always two rows away, and
/// making them recent would push the plugins they compete with out of the
/// (short) list.
pub fn remember(recents: &mut Vec<RecentPlugin>, pick: &PickChoice) {
    let (format, path, id) = match &pick.source {
        NodeSource::ClapInstrument { path, plugin_id }
        | NodeSource::ClapEffect { path, plugin_id } => {
            (PluginFormat::Clap, path.clone(), plugin_id.clone())
        }
        NodeSource::Vst3Instrument { path, class_id }
        | NodeSource::Vst3Effect { path, class_id } => {
            (PluginFormat::Vst3, path.clone(), class_id.clone())
        }
        _ => return,
    };
    recents.retain(|recent| !(recent.format == format && recent.path == path && recent.id == id));
    recents.insert(
        0,
        RecentPlugin {
            format,
            path,
            id,
            name: pick.name.clone(),
            is_instrument: pick_is_instrument(pick),
        },
    );
    let mut instruments = 0;
    let mut effects = 0;
    recents.retain(|recent| {
        let count = if recent.is_instrument {
            &mut instruments
        } else {
            &mut effects
        };
        *count += 1;
        *count <= RECENT_MAX
    });
}

/// Whether a choice creates an instrument node.
fn pick_is_instrument(pick: &PickChoice) -> bool {
    matches!(
        pick.source,
        NodeSource::BuiltinSynth
            | NodeSource::SoundFont { .. }
            | NodeSource::Sample { .. }
            | NodeSource::ClapInstrument { .. }
            | NodeSource::Vst3Instrument { .. }
    )
}

/// Query match (spec §6.2): whitespace-separated tokens must all appear in
/// either the display name or the module path, case-insensitively.
///
/// The path is part of the haystack so two vendors' same-named plugins can be
/// told apart by typing either vendor name.
pub fn matches_query(name: &str, path: &str, query: &str) -> bool {
    let name = name.to_lowercase();
    let path = path.to_lowercase();
    query.split_whitespace().all(|token| {
        let token = token.to_lowercase();
        name.contains(&token) || path.contains(&token)
    })
}

/// Built-in rows admitted by `filter` and matching `query`, in menu order.
pub fn builtin_choices(filter: PickFilter, query: &str) -> Vec<PickChoice> {
    let mut rows = Vec::new();
    // D19: the built-in chip is a filter like any other — switched off, the
    // built-ins disappear even though no module is involved.
    if !filter.tags.has(PickTag::BuiltIn) {
        return rows;
    }
    if filter.admits(true) {
        rows.extend(builtin_instruments());
    }
    if filter.admits(false) {
        rows.extend(BuiltinEffectKind::ALL.iter().map(|kind| PickChoice {
            name: kind.label().to_string(),
            tag: PickTag::BuiltIn,
            source: NodeSource::Builtin(*kind),
        }));
    }
    rows.retain(|row| matches_query(&row.name, "", query));
    rows
}

/// Catalog entries admitted by `filter` and matching `query`, grouped by tag
/// then name (spec §6.2 ordering).
pub fn plugin_choices(entries: &[PluginEntry], filter: PickFilter, query: &str) -> Vec<PickChoice> {
    let mut rows: Vec<PickChoice> = entries
        .iter()
        .filter(|entry| filter.admits(entry.is_instrument))
        .filter(|entry| filter.tags.has(tag_of(entry.format)))
        .filter(|entry| matches_query(&entry.name, &entry.path, query))
        .map(PickChoice::from)
        .collect();
    rows.sort_by_cached_key(|row| (row.tag, row.name.to_lowercase()));
    rows
}

/// Sources that have at least one candidate under `filter`'s **category**,
/// ignoring both the query and the tag toggles (spec D19).
///
/// The strip shows only these, so a machine without VST3 plug-ins never
/// offers a dead VST3 chip; ignoring the toggles keeps the strip from
/// collapsing while the user clicks it, and ignoring the query keeps it from
/// flickering per keystroke.
pub fn available_tags(entries: &[PluginEntry], filter: PickFilter) -> PickTags {
    // Both categories always have built-ins (3 instruments / 6 effects).
    let mut tags = PickTags::NONE;
    tags.set(PickTag::BuiltIn, true);
    for entry in entries
        .iter()
        .filter(|entry| filter.admits(entry.is_instrument))
    {
        tags.set(tag_of(entry.format), true);
    }
    tags
}

/// Full row list of one frame plus the number of leading recent rows.
///
/// Recents lead only while the query is empty; the rest is the built-in block
/// followed by the plugins, so tags never interleave and the group headings
/// can be drawn by watching for the tag to change. Recents obey both filters
/// too — a switched-off source can't sneak back in through the recent list.
pub fn rows(
    entries: &[PluginEntry],
    recents: &[RecentPlugin],
    filter: PickFilter,
    query: &str,
) -> (Vec<PickChoice>, usize) {
    let mut rows: Vec<PickChoice> = Vec::new();
    if query.trim().is_empty() {
        rows.extend(
            recents
                .iter()
                .filter(|recent| filter.admits(recent.is_instrument))
                .filter(|recent| filter.tags.has(tag_of(recent.format)))
                .filter(|recent| matches_query(&recent.name, &recent.path, query))
                .take(RECENT_MAX)
                .map(RecentPlugin::as_choice),
        );
    }
    let recent_count = rows.len();
    rows.extend(builtin_choices(filter, query));
    rows.extend(plugin_choices(entries, filter, query));
    (rows, recent_count)
}

/// Result of one frame of the popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickOutcome {
    /// The user chose an entry (the popup is already closed).
    Picked(PickChoice),
    /// Esc / click outside (the popup is already closed).
    Cancelled,
}

/// Popup state, owned by the app and drawn once per frame.
#[derive(Debug, Default)]
pub struct PluginPicker {
    open: bool,
    filter: PickFilter,
    query: String,
    selected: usize,
    /// Screen position the popup anchors to (set on open).
    anchor: Option<egui::Pos2>,
    /// `request_focus` + select-all on the next draw.
    focus_query: bool,
    /// Scroll the keyboard-selected row into view on the next draw.
    reveal: bool,
    /// Rows shown last frame — only used to refresh the footer count.
    row_count: usize,
    /// Set while the frame that opened the popup is still being drawn: the
    /// click that opened it must not be read as a click-outside dismissal.
    just_opened: bool,
}

impl PluginPicker {
    /// `true` while the popup owns the keyboard (the app suspends its global
    /// shortcuts then — spec §6.3).
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Opens the popup with a fresh query. The anchor is resolved on the first
    /// draw from the pointer position (the click that opened it).
    pub fn open(&mut self, filter: PickFilter) {
        self.open = true;
        self.filter = filter;
        self.query.clear();
        self.selected = 0;
        self.anchor = None;
        self.focus_query = true;
        self.reveal = true;
        self.just_opened = true;
    }

    /// Closes without a pick.
    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.anchor = None;
    }

    /// Row list for the current filter + query, with the highlight clamped
    /// into it. Also refreshes the footer count.
    fn rebuild(
        &mut self,
        entries: &[PluginEntry],
        recents: &[RecentPlugin],
    ) -> (Vec<PickChoice>, usize) {
        let (rows, recent_count) = rows(entries, recents, self.filter, &self.query);
        self.row_count = rows.len();
        if self.selected >= rows.len() {
            self.selected = rows.len().saturating_sub(1);
        }
        (rows, recent_count)
    }

    /// Draws the popup (a no-op while closed) and reports a pick.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        entries: &[PluginEntry],
        recents: &[RecentPlugin],
        scan_progress: Option<(usize, usize)>,
    ) -> Option<PickOutcome> {
        if !self.open {
            return None;
        }
        // The frame that opened the popup must ignore its own click/Escape:
        // the click that picked the menu leaf is the same event a
        // click-outside check would see. Taking the flag up front also covers
        // the early returns below.
        let opening = std::mem::take(&mut self.just_opened);

        // Rows: recents first (empty query only), then the built-in block,
        // then the catalog matches — one row type for all three sources
        // (spec D15). The list is rebuilt each frame: a few hundred owned
        // clones are cheaper than caching it against every catalog mutation.
        // `rebuild` is called again if a chip flips the source filter, so the
        // click lands in the same frame (no stale list, no extra repaint).
        let (mut rows, mut recent_count) = self.rebuild(entries, recents);

        // Keys are read before the text field is drawn so the list owns the
        // arrows even while the query field has focus (spec §6.3).
        let (down, up, enter, escape) = ctx.input_mut(|input| {
            (
                input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
            )
        });
        if escape && !opening {
            self.close();
            return Some(PickOutcome::Cancelled);
        }
        if down && !rows.is_empty() {
            self.selected = (self.selected + 1).min(rows.len() - 1);
            self.reveal = true;
        }
        if up && !rows.is_empty() {
            self.selected = self.selected.saturating_sub(1);
            self.reveal = true;
        }
        if enter && let Some(choice) = rows.get(self.selected).cloned() {
            self.close();
            return Some(PickOutcome::Picked(choice));
        }

        let anchor = *self.anchor.get_or_insert_with(|| {
            ctx.pointer_latest_pos()
                .unwrap_or_else(|| ctx.content_rect().center())
        });
        // The chip strip is computed once and reused by the empty state: a
        // source with no candidate under this category is not offered at all
        // (spec D19).
        let available = available_tags(entries, self.filter);
        let mut picked: Option<PickChoice> = None;
        let area = egui::Area::new(egui::Id::new("glitcherz_plugin_picker"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor)
            .constrain_to(ctx.content_rect())
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(POPUP_WIDTH);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(self.filter.heading()).strong());
                        if let Some((done, total)) = scan_progress {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.weak(format!("Scanning {done}/{total}"));
                                },
                            );
                        }
                    });
                    let text_edit = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .id(query_id())
                            .hint_text("Search by name or path…")
                            .desired_width(f32::INFINITY)
                            // Enter belongs to the list (spec §6.3), not to
                            // the text field's "end input" behaviour.
                            .return_key(Option::<egui::KeyboardShortcut>::None),
                    );
                    if self.focus_query {
                        self.focus_query = false;
                        select_all(ctx, text_edit.id, &self.query);
                        text_edit.request_focus();
                    }
                    if text_edit.changed() {
                        // A new query makes the old highlight meaningless.
                        self.selected = 0;
                        self.reveal = true;
                    }
                    // Source toggles (spec D19): the tag on each row doubles
                    // as a filter. Only sources that exist under this category
                    // are offered, and the strip hides itself when there is
                    // nothing to choose between.
                    if available.count() > 1 {
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            for tag in PickTag::ALL {
                                if !available.has(tag) {
                                    continue;
                                }
                                if tag_chip(ui, tag, self.filter.tags.has(tag)) {
                                    self.filter.tags.toggle(tag);
                                    // Same frame: the list must already
                                    // reflect the click.
                                    self.selected = 0;
                                    self.reveal = true;
                                    (rows, recent_count) = self.rebuild(entries, recents);
                                }
                            }
                        });
                    }
                    ui.separator();

                    if rows.is_empty() {
                        self.empty_state(ui, scan_progress);
                        return;
                    }

                    let mut clicked: Option<usize> = None;
                    egui::ScrollArea::vertical()
                        .max_height(LIST_MAX_HEIGHT)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            let mut index = 0usize;
                            if recent_count > 0 {
                                ui.weak("Recent");
                                for choice in rows.iter().take(recent_count) {
                                    if row(ui, choice, index == self.selected, self.reveal, index) {
                                        clicked = Some(index);
                                    }
                                    index += 1;
                                }
                                ui.add_space(2.0);
                            }
                            // Group headings follow the tag, which the row
                            // builder keeps contiguous (spec §6.2). Headings
                            // are not rows: the keyboard index only counts
                            // entries.
                            let mut shown_tag: Option<PickTag> = None;
                            for choice in rows.iter().skip(index) {
                                if shown_tag != Some(choice.tag) {
                                    if shown_tag.is_some() {
                                        ui.add_space(2.0);
                                    }
                                    shown_tag = Some(choice.tag);
                                    ui.weak(choice.tag.heading());
                                }
                                if row(ui, choice, index == self.selected, self.reveal, index) {
                                    clicked = Some(index);
                                }
                                index += 1;
                            }
                        });
                    self.reveal = false;
                    if let Some(index) = clicked {
                        picked = rows.get(index).cloned();
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.weak(format!("{} match(es)", rows.len()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.weak("↑↓ select · Enter add · Esc close");
                        });
                    });
                });
            });

        if let Some(choice) = picked {
            self.close();
            return Some(PickOutcome::Picked(choice));
        }

        // Click outside the popup closes it (the click itself still reaches
        // the canvas — same as Blender's popups).
        let clicked_at = ctx.input(|input| {
            if input.pointer.any_click() {
                input.pointer.latest_pos()
            } else {
                None
            }
        });
        if let Some(pos) = clicked_at
            && !opening
            && !area.response.rect.contains(pos)
        {
            self.close();
            return Some(PickOutcome::Cancelled);
        }
        None
    }

    /// Empty list: the scan is still running, the query missed, or the source
    /// chips are all off.
    fn empty_state(&mut self, ui: &mut egui::Ui, scan_progress: Option<(usize, usize)>) {
        if scan_progress.is_some() {
            ui.weak("Still scanning plug-ins — the list grows as modules finish.");
            return;
        }
        ui.add_space(4.0);
        if self.query.trim().is_empty() {
            ui.weak("Every source is switched off.");
        } else {
            ui.weak(format!("No match for \"{}\"", self.query.trim()));
        }
        // Two escape hatches, each undoing one of the two filters. The
        // cross-category one is the menu's `Search Nodes…` (spec D9): inside a
        // category there is nothing wider to offer than an empty query.
        ui.horizontal(|ui| {
            let searching = !self.query.trim().is_empty();
            if searching && ui.button("Clear search").clicked() {
                self.query.clear();
                self.selected = 0;
                self.reveal = true;
            }
            if !self.filter.tags.is_all() && ui.button("Show all sources").clicked() {
                self.filter.tags = PickTags::ALL;
                self.selected = 0;
                self.reveal = true;
            }
        });
    }

    /// Number of rows drawn on the last frame (footer / tests).
    pub fn row_count(&self) -> usize {
        self.row_count
    }
}

/// One source toggle: a small pill that reads like the row's own tag chip
/// (spec D19). Hand-drawn for the same reason as [`row`] — the test needs a
/// stable id, and egui's auto ids follow widget order.
fn tag_chip(ui: &mut egui::Ui, tag: PickTag, on: bool) -> bool {
    let font = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(
        tag.label().to_owned(),
        font.clone(),
        egui::Color32::PLACEHOLDER,
    );
    let size = egui::vec2(galley.size().x + 14.0, ROW_HEIGHT - 4.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return false;
    }
    let response = ui.interact(
        rect,
        egui::Id::new(("plugin_picker_tag", tag.label())),
        egui::Sense::click(),
    );
    let visuals = ui.visuals();
    let (fill, text) = if on {
        (visuals.selection.bg_fill, visuals.selection.stroke.color)
    } else if response.hovered() {
        (visuals.widgets.hovered.weak_bg_fill, visuals.text_color())
    } else {
        (
            visuals.widgets.inactive.weak_bg_fill,
            visuals.weak_text_color(),
        )
    };
    ui.painter().rect_filled(rect, 3.0, fill);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        tag.label(),
        font,
        text,
    );
    response
        .on_hover_text(if on {
            "Showing — click to hide this source"
        } else {
            "Hidden — click to show this source"
        })
        .clicked()
}

/// Paints one 22 px row and reports a click. `index` feeds the row's explicit
/// id — a stable id keeps the click assigned to the same widget across frames
/// (and makes the row addressable from headless tests).
fn row(ui: &mut egui::Ui, choice: &PickChoice, selected: bool, reveal: bool, index: usize) -> bool {
    let width = ui.available_width();
    // Hover-only placeholder reserves the row; the click sense attaches to an
    // explicitly identified `interact` registered **after** the content — see
    // below for why the order matters.
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, ROW_HEIGHT), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return false;
    }
    let hovered = ui.rect_contains_pointer(rect);
    if selected || hovered {
        ui.painter()
            .rect_filled(rect, 2.0, ui.visuals().selection.bg_fill);
    }
    if selected && reveal {
        ui.scroll_to_rect(rect, Some(egui::Align::Center));
    }
    let text_color = if selected {
        ui.visuals().selection.stroke.color
    } else {
        ui.visuals().text_color()
    };
    let tag_width = TAG_WIDTH;
    let mut content = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(4.0, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    content.add_sized(
        egui::vec2((width - tag_width - 12.0).max(16.0), ROW_HEIGHT),
        egui::Label::new(egui::RichText::new(&choice.name).color(text_color)).truncate(),
    );
    content.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(
            egui::RichText::new(choice.tag.label())
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    });
    // egui's hit-testing stops at the **topmost** widget under the pointer —
    // a hover-sense Label painted over the row absorbs the click instead of
    // passing it through. Registering the click widget last puts the row on
    // top of its own labels, so a mouse click on the name reaches it (the
    // Enter path always worked because keys bypass hit-testing).
    let response = ui.interact(
        rect,
        egui::Id::new(("plugin_picker_row", index)),
        egui::Sense::click(),
    );
    response.on_hover_text(choice.tooltip()).clicked()
}

/// Focuses and selects the whole query field on open (spec §6.3).
fn select_all(ctx: &egui::Context, id: egui::Id, text: &str) {
    let Some(mut state) = egui::TextEdit::load_state(ctx, id) else {
        return;
    };
    let chars = text.chars().count();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(chars),
        )));
    state.store(ctx, id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        format: PluginFormat,
        path: &str,
        id: &str,
        name: &str,
        is_instrument: bool,
    ) -> PluginEntry {
        PluginEntry {
            format,
            path: path.to_string(),
            id: id.to_string(),
            name: name.to_string(),
            is_instrument,
            mtime_ms: 0,
            size: 0,
        }
    }

    #[test]
    fn query_tokens_are_anded_over_name_and_path() {
        assert!(matches_query("Serum", "C:/vst3/Serum.vst3", "ser"));
        assert!(matches_query(
            "Serum",
            "C:/VendorX/Serum.vst3",
            "vendor serum"
        ));
        assert!(!matches_query(
            "Serum",
            "C:/VendorX/Serum.vst3",
            "vendor missing"
        ));
        assert!(matches_query("Serum", "C:/VendorX/Serum.vst3", "   "));
    }

    #[test]
    fn rows_put_builtins_first_then_group_by_tag() {
        let entries = vec![
            entry(PluginFormat::Vst3, "C:/vst3/b.vst3", "b", "Beta", true),
            entry(PluginFormat::Clap, "C:/clap/c.clap", "c", "Gamma", false),
            entry(PluginFormat::Clap, "C:/clap/a.clap", "a", "Alpha", true),
        ];
        let (rows, recents) = rows(&entries, &[], PickFilter::default(), "");
        assert_eq!(recents, 0);
        // Built-in block, then CLAP, then VST3 — tags never interleave.
        let tags: Vec<PickTag> = rows.iter().map(|row| row.tag).collect();
        let builtins = builtin_instruments().len() + BuiltinEffectKind::ALL.len();
        assert_eq!(&tags[..builtins], &vec![PickTag::BuiltIn; builtins][..]);
        assert!(tags[builtins..].windows(2).all(|pair| pair[0] <= pair[1]));
        // Names inside the CLAP group are ordered ignoring case.
        let clap: Vec<&str> = rows
            .iter()
            .filter(|row| row.tag == PickTag::Clap)
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(clap, vec!["Alpha", "Gamma"]);
    }

    #[test]
    fn category_filter_splits_builtins_and_plugins() {
        let entries = vec![
            entry(PluginFormat::Clap, "C:/clap/a.clap", "a", "Alpha", true),
            entry(PluginFormat::Vst3, "C:/vst3/b.vst3", "b", "Beta", false),
        ];
        let instruments = plugin_choices(&entries, PickFilter::category(Some(true)), "");
        assert_eq!(instruments.len(), 1);
        assert_eq!(instruments[0].name, "Alpha");
        assert!(matches!(
            instruments[0].source,
            NodeSource::ClapInstrument { .. }
        ));

        let effects = builtin_choices(PickFilter::category(Some(false)), "delay");
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].name, "Delay");
        assert_eq!(effects[0].tag, PickTag::BuiltIn);
        assert!(matches!(effects[0].source, NodeSource::Builtin(_)));

        // Instruments offer the three built-ins, all tagged built-in.
        let all_instruments = builtin_choices(PickFilter::category(Some(true)), "");
        assert_eq!(all_instruments.len(), 3);
        assert!(
            all_instruments
                .iter()
                .all(|row| row.tag == PickTag::BuiltIn)
        );
        assert!(matches!(
            all_instruments[1].source,
            NodeSource::SoundFont { .. }
        ));
        assert!(matches!(
            all_instruments[2].source,
            NodeSource::Sample { .. }
        ));
    }

    #[test]
    fn builtin_rows_obey_the_query() {
        let rows = builtin_choices(PickFilter::default(), "sound");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "SoundFont");
        // `vst3` used to be a menu leaf; as a query it matches nothing here.
        assert!(builtin_choices(PickFilter::default(), "vst3").is_empty());
    }

    // ---- Source (tag) toggles: spec D19 ----

    #[test]
    fn tag_filter_keeps_only_the_chosen_sources() {
        let entries = vec![
            entry(PluginFormat::Clap, "C:/clap/a.clap", "a", "Alpha", false),
            entry(PluginFormat::Vst3, "C:/vst3/b.vst3", "b", "Beta", false),
        ];
        let mut filter = PickFilter::category(Some(false));
        filter.tags.set(PickTag::Vst3, false);
        let (rows, _) = rows(&entries, &[], filter, "");
        assert_eq!(
            rows.iter().filter(|row| row.tag == PickTag::Clap).count(),
            1
        );
        assert!(!rows.iter().any(|row| row.tag == PickTag::Vst3));
        // Built-ins are unaffected by a plugin-only toggle.
        assert!(rows.iter().any(|row| row.tag == PickTag::BuiltIn));
    }

    #[test]
    fn tag_filter_can_hide_the_builtins() {
        let mut filter = PickFilter::category(Some(true));
        filter.tags.toggle(PickTag::BuiltIn);
        assert!(builtin_choices(filter, "").is_empty());
        // Nothing else is installed, so the instrument list is now empty —
        // the empty state offers "Show all sources".
        let (rows, _) = rows(&[], &[], filter, "");
        assert!(rows.is_empty());
        assert!(!filter.tags.is_all());
    }

    #[test]
    fn tag_filter_applies_to_recents() {
        let recents = vec![
            RecentPlugin {
                format: PluginFormat::Clap,
                path: "C:/clap/a.clap".to_string(),
                id: "a".to_string(),
                name: "Alpha".to_string(),
                is_instrument: true,
            },
            RecentPlugin {
                format: PluginFormat::Vst3,
                path: "C:/vst3/b.vst3".to_string(),
                id: "b".to_string(),
                name: "Beta".to_string(),
                is_instrument: true,
            },
        ];
        let mut filter = PickFilter::category(Some(true));
        filter.tags.set(PickTag::Clap, false);
        let (rows, recent_count) = rows(&[], &recents, filter, "");
        assert_eq!(recent_count, 1);
        assert_eq!(rows[0].name, "Beta");
        assert!(!rows.iter().any(|row| row.tag == PickTag::Clap));
    }

    #[test]
    fn available_tags_follow_the_category_and_catalog() {
        let entries = vec![
            entry(PluginFormat::Clap, "C:/clap/a.clap", "a", "Alpha", true),
            entry(PluginFormat::Vst3, "C:/vst3/b.vst3", "b", "Beta", false),
        ];
        // Instruments: built-ins + the CLAP instrument; no VST3 exists there.
        let instruments = available_tags(&entries, PickFilter::category(Some(true)));
        assert!(instruments.has(PickTag::BuiltIn));
        assert!(instruments.has(PickTag::Clap));
        assert!(!instruments.has(PickTag::Vst3));
        assert_eq!(instruments.count(), 2);

        // The toggles themselves never change what is offered — otherwise the
        // strip would collapse under the user's own click.
        let mut toggled = PickFilter::category(Some(true));
        toggled.tags.set(PickTag::Clap, false);
        assert_eq!(available_tags(&entries, toggled), instruments);
    }

    #[test]
    fn remember_dedups_caps_and_skips_builtins() {
        let mut recents = Vec::new();
        for index in 0..7 {
            let choice = plugin_choice(
                format!("Plugin {index}"),
                PluginFormat::Clap,
                format!("C:/clap/{index}.clap"),
                format!("id{index}"),
                index % 2 == 0,
            );
            remember(&mut recents, &choice);
        }
        // 4 even (instrument) + 3 odd (effect): both fit under the cap.
        assert_eq!(recents.len(), 7);

        // Re-picking an existing entry moves it to the front without growing.
        remember(
            &mut recents,
            &plugin_choice(
                "Plugin 0".to_string(),
                PluginFormat::Clap,
                "C:/clap/0.clap".to_string(),
                "id0".to_string(),
                true,
            ),
        );
        assert_eq!(recents.len(), 7);
        assert_eq!(recents[0].id, "id0");

        // Built-in picks never enter the list: they are always two rows away
        // and would push the plugins they compete with out of it.
        remember(
            &mut recents,
            &PickChoice {
                name: "SoundFont".to_string(),
                tag: PickTag::BuiltIn,
                source: NodeSource::SoundFont {
                    path: String::new(),
                },
            },
        );
        assert_eq!(recents.len(), 7);
        assert!(recents.iter().all(|recent| recent.name != "SoundFont"));

        // Cap: 8 instruments total, only the newest 5 survive.
        for index in 10..16 {
            remember(
                &mut recents,
                &plugin_choice(
                    format!("Inst {index}"),
                    PluginFormat::Vst3,
                    format!("C:/vst3/{index}.vst3"),
                    format!("v{index}"),
                    true,
                ),
            );
        }
        assert_eq!(
            recents.iter().filter(|recent| recent.is_instrument).count(),
            RECENT_MAX
        );
    }

    #[test]
    fn recents_lead_only_while_the_query_is_empty() {
        let recents = vec![
            RecentPlugin {
                format: PluginFormat::Clap,
                path: "C:/clap/a.clap".to_string(),
                id: "a".to_string(),
                name: "Alpha".to_string(),
                is_instrument: true,
            },
            RecentPlugin {
                format: PluginFormat::Vst3,
                path: "C:/vst3/b.vst3".to_string(),
                id: "b".to_string(),
                name: "Beta".to_string(),
                is_instrument: false,
            },
        ];
        let (rows, recent_count) = super::rows(&[], &recents, PickFilter::category(Some(true)), "");
        assert_eq!(recent_count, 1);
        assert_eq!(rows[0].name, "Alpha");
        assert!(matches!(rows[0].source, NodeSource::ClapInstrument { .. }));

        // A query drops the recent block entirely (it would be a stale list)
        // and searches the catalog instead.
        let entries = vec![entry(
            PluginFormat::Vst3,
            "C:/vst3/b.vst3",
            "b",
            "Beta",
            false,
        )];
        let (rows, recent_count) = super::rows(&entries, &recents, PickFilter::default(), "beta");
        assert_eq!(recent_count, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Beta");
        assert_eq!(rows[0].tag, PickTag::Vst3);
        assert!(matches!(rows[0].source, NodeSource::Vst3Effect { .. }));
    }

    // ---- Headless frames: the popup must draw and pick without a display ----

    const SCREEN: egui::Vec2 = egui::vec2(800.0, 600.0);

    fn key_event(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn frame(
        ctx: &egui::Context,
        picker: &mut PluginPicker,
        entries: &[PluginEntry],
        recents: &[RecentPlugin],
        events: Vec<egui::Event>,
        t: f64,
    ) -> Option<PickOutcome> {
        let mut viewports = egui::ViewportIdMap::default();
        viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                inner_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
                ..Default::default()
            },
        );
        let raw = egui::RawInput {
            events,
            time: Some(t),
            viewports,
            ..Default::default()
        };
        let mut outcome = None;
        let mut output = ctx.run_ui(raw, |ctx| {
            outcome = picker.show(ctx, entries, recents, None);
        });
        output.textures_delta.clear();
        outcome
    }

    fn sample_entries() -> Vec<PluginEntry> {
        vec![
            entry(PluginFormat::Clap, "C:/clap/a.clap", "a", "Alpha", true),
            entry(PluginFormat::Clap, "C:/clap/b.clap", "b", "Beta", true),
        ]
    }

    #[test]
    fn keyboard_navigates_and_picks() {
        let ctx = egui::Context::default();
        let entries = sample_entries();
        let mut picker = PluginPicker::default();
        picker.open(PickFilter::category(Some(true)));
        // Opening frame: no events, nothing picked, the popup stays open.
        assert!(frame(&ctx, &mut picker, &entries, &[], vec![], 0.0).is_none());
        assert!(picker.is_open());
        // Rows: Synth / SoundFont / Sample, then Alpha / Beta (spec D15) — the
        // highlight starts on the first built-in.
        assert_eq!(picker.row_count(), 5);
        // ↓ moves the highlight down one row at a time…
        for step in 1..5 {
            assert!(
                frame(
                    &ctx,
                    &mut picker,
                    &entries,
                    &[],
                    vec![key_event(egui::Key::ArrowDown)],
                    0.05 * f64::from(step)
                )
                .is_none()
            );
        }
        // …and Enter takes the fourth step's row (the second plugin).
        let outcome = frame(
            &ctx,
            &mut picker,
            &entries,
            &[],
            vec![key_event(egui::Key::Enter)],
            0.3,
        );
        match outcome {
            Some(PickOutcome::Picked(choice)) => {
                assert_eq!(choice.name, "Beta");
                assert_eq!(choice.tag, PickTag::Clap);
                assert!(matches!(
                    choice.source,
                    NodeSource::ClapInstrument { plugin_id, .. } if plugin_id == "b"
                ));
            }
            other => panic!("expected a pick, got {other:?}"),
        }
        assert!(!picker.is_open());
    }

    #[test]
    fn builtin_rows_are_pickable() {
        let ctx = egui::Context::default();
        let mut picker = PluginPicker::default();
        picker.open(PickFilter::category(Some(true)));
        assert!(frame(&ctx, &mut picker, &sample_entries(), &[], vec![], 0.0).is_none());
        let outcome = frame(
            &ctx,
            &mut picker,
            &sample_entries(),
            &[],
            vec![key_event(egui::Key::Enter)],
            0.1,
        );
        match outcome {
            Some(PickOutcome::Picked(choice)) => {
                assert_eq!(choice.name, "Synth");
                assert_eq!(choice.tag, PickTag::BuiltIn);
                assert!(matches!(choice.source, NodeSource::BuiltinSynth));
            }
            other => panic!("expected the built-in synth, got {other:?}"),
        }
    }

    #[test]
    fn escape_closes_without_a_pick() {
        let ctx = egui::Context::default();
        let entries = sample_entries();
        let mut picker = PluginPicker::default();
        picker.open(PickFilter::default());
        assert!(frame(&ctx, &mut picker, &entries, &[], vec![], 0.0).is_none());
        let outcome = frame(
            &ctx,
            &mut picker,
            &entries,
            &[],
            vec![key_event(egui::Key::Escape)],
            0.1,
        );
        assert_eq!(outcome, Some(PickOutcome::Cancelled));
        assert!(!picker.is_open());
    }

    #[test]
    fn mouse_click_picks_a_row() {
        fn hover(pos: egui::Pos2) -> egui::Event {
            egui::Event::PointerMoved(pos)
        }
        fn press(pos: egui::Pos2, pressed: bool) -> egui::Event {
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }
        }

        let ctx = egui::Context::default();
        let entries = sample_entries();
        let mut picker = PluginPicker::default();
        picker.open(PickFilter::category(Some(true)));
        // Opening frame: the menu's own click must not land on the list.
        assert!(frame(&ctx, &mut picker, &entries, &[], vec![], 0.0).is_none());
        // Settle frame (the opening frame is a sizing pass — rows may skip
        // drawing while the popup size is still unknown).
        assert!(frame(&ctx, &mut picker, &entries, &[], vec![], 0.05).is_none());

        // Rows carry explicit ids, so the test can address one directly and
        // click its centre — the same gesture as clicking the name with the
        // mouse. Index 3 is the first plugin: the three built-ins lead.
        let row_rect = ctx
            .read_response(egui::Id::new(("plugin_picker_row", 3)))
            .expect("row 3 drew once the popup settled")
            .rect;
        let pos = row_rect.center();
        assert!(frame(&ctx, &mut picker, &entries, &[], vec![hover(pos)], 0.1).is_none());
        assert!(
            frame(
                &ctx,
                &mut picker,
                &entries,
                &[],
                vec![hover(pos), press(pos, true)],
                0.2
            )
            .is_none()
        );
        let outcome = frame(
            &ctx,
            &mut picker,
            &entries,
            &[],
            vec![hover(pos), press(pos, false)],
            0.3,
        );
        match outcome {
            Some(PickOutcome::Picked(choice)) => {
                assert_eq!(choice.name, "Alpha");
                assert!(matches!(choice.source, NodeSource::ClapInstrument { .. }));
            }
            other => panic!("expected a pick from a mouse click, got {other:?}"),
        }
        assert!(!picker.is_open());
    }

    #[test]
    fn recents_lead_the_list_when_the_query_is_empty() {
        let ctx = egui::Context::default();
        let entries = sample_entries();
        let recents = vec![RecentPlugin {
            format: PluginFormat::Vst3,
            path: "C:/vst3/z.vst3".to_string(),
            id: "z".to_string(),
            name: "Zeta".to_string(),
            is_instrument: true,
        }];
        let mut picker = PluginPicker::default();
        picker.open(PickFilter::default());
        // 1 recent + 3 built-in instruments + 6 built-in effects + 2 plugins.
        assert!(frame(&ctx, &mut picker, &entries, &recents, vec![], 0.0).is_none());
        assert_eq!(
            picker.row_count(),
            1 + builtin_instruments().len() + BuiltinEffectKind::ALL.len() + entries.len()
        );
        // Enter on the initial highlight takes the most recent plugin.
        let outcome = frame(
            &ctx,
            &mut picker,
            &entries,
            &recents,
            vec![key_event(egui::Key::Enter)],
            0.1,
        );
        match outcome {
            Some(PickOutcome::Picked(choice)) => {
                assert_eq!(choice.name, "Zeta");
                assert_eq!(choice.tag, PickTag::Vst3);
            }
            other => panic!("expected the recent pick, got {other:?}"),
        }
    }

    #[test]
    fn clicking_a_tag_chip_filters_the_list() {
        fn hover(pos: egui::Pos2) -> egui::Event {
            egui::Event::PointerMoved(pos)
        }
        fn press(pos: egui::Pos2, pressed: bool) -> egui::Event {
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }
        }

        let ctx = egui::Context::default();
        let entries = vec![
            entry(PluginFormat::Clap, "C:/clap/a.clap", "a", "Alpha", true),
            entry(PluginFormat::Vst3, "C:/vst3/b.vst3", "b", "Beta", true),
        ];
        let mut picker = PluginPicker::default();
        picker.open(PickFilter::category(Some(true)));
        assert!(frame(&ctx, &mut picker, &entries, &[], vec![], 0.0).is_none());
        assert!(frame(&ctx, &mut picker, &entries, &[], vec![], 0.05).is_none());
        // 3 built-ins + Alpha + Beta.
        assert_eq!(picker.row_count(), 5);

        // Chips carry explicit ids, so the test can click one like the mouse
        // would: BUILT-IN / CLAP / VST3 all exist under "instruments" here.
        let chip = ctx
            .read_response(egui::Id::new(("plugin_picker_tag", "VST3")))
            .expect("the VST3 chip drew once the popup settled")
            .rect;
        let pos = chip.center();
        assert!(frame(&ctx, &mut picker, &entries, &[], vec![hover(pos)], 0.1).is_none());
        assert!(
            frame(
                &ctx,
                &mut picker,
                &entries,
                &[],
                vec![hover(pos), press(pos, true)],
                0.2
            )
            .is_none()
        );
        // Flipping a chip filters, it never picks: the popup stays open and
        // the list already reflects the click on this very frame.
        assert!(
            frame(
                &ctx,
                &mut picker,
                &entries,
                &[],
                vec![hover(pos), press(pos, false)],
                0.3
            )
            .is_none()
        );
        assert!(picker.is_open());
        assert!(!picker.filter.tags.has(PickTag::Vst3));
        assert_eq!(picker.row_count(), 4);

        // Clicking it again brings Beta back.
        assert!(
            frame(
                &ctx,
                &mut picker,
                &entries,
                &[],
                vec![hover(pos), press(pos, true)],
                0.4
            )
            .is_none()
        );
        assert!(
            frame(
                &ctx,
                &mut picker,
                &entries,
                &[],
                vec![hover(pos), press(pos, false)],
                0.5
            )
            .is_none()
        );
        assert!(picker.filter.tags.has(PickTag::Vst3));
        assert_eq!(picker.row_count(), 5);
    }

    #[test]
    fn the_opening_click_does_not_close_the_popup() {
        let ctx = egui::Context::default();
        let entries = sample_entries();
        let mut picker = PluginPicker::default();
        picker.open(PickFilter::default());
        // Pin the popup so the click lands outside it: in the app the anchor
        // is the pointer position (inside the popup), but `constrain_to` can
        // shift a popup opened near a screen edge — that is the case this
        // guard covers.
        picker.anchor = Some(egui::pos2(50.0, 50.0));
        let far = egui::pos2(700.0, 560.0);
        let press = |pressed: bool| egui::Event::PointerButton {
            pos: far,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        // Opening frame: it carries the click that picked the menu leaf.
        assert!(
            frame(
                &ctx,
                &mut picker,
                &entries,
                &[],
                vec![egui::Event::PointerMoved(far), press(true), press(false)],
                0.0,
            )
            .is_none()
        );
        assert!(picker.is_open(), "the opening click closed the popup");
        // A later click outside closes it.
        assert!(
            frame(
                &ctx,
                &mut picker,
                &entries,
                &[],
                vec![egui::Event::PointerMoved(far), press(true)],
                0.1,
            )
            .is_none()
        );
        let outcome = frame(&ctx, &mut picker, &entries, &[], vec![press(false)], 0.2);
        assert_eq!(outcome, Some(PickOutcome::Cancelled));
        assert!(!picker.is_open());
    }
}
