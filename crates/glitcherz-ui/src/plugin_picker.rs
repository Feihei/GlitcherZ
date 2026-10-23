//! Plugin picker: the filterable popup behind `Add ▸ Instrument ▸ CLAP…`.
//!
//! Spec: `.agents/spec/plugin-browser.md` (U9 E3). A popup rather than an
//! `ui.menu_button` submenu, because the leaf layer needs a text field, a
//! scrolling list and keyboard navigation — none of which egui submenus
//! handle well (spec D4/D5).
//!
//! The popup never touches the disk: it reads the catalog that the startup
//! scan already filled ([`crate::plugin_catalog`]) and returns the chosen
//! entry. Filtering, the recent list and the query match are pure functions
//! so they can be tested without a display.

use serde::{Deserialize, Serialize};

use eframe::egui;

use crate::plugin_catalog::{PluginEntry, PluginFormat};

/// How many entries each category keeps in the recent list (spec D10).
pub const RECENT_MAX: usize = 5;

const POPUP_WIDTH: f32 = 420.0;
const LIST_MAX_HEIGHT: f32 = 300.0;
const ROW_HEIGHT: f32 = 22.0;

/// Stable id for the query field (an auto-generated id would move with the
/// popup and drop focus on open).
fn query_id() -> egui::Id {
    egui::Id::new("glitcherz_plugin_picker_query")
}

/// What a picker instance may show (spec §6.1: opened from a specific leaf).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PickFilter {
    /// `Some(true)` = instruments only, `Some(false)` = effects only,
    /// `None` = the global `Search Plug-ins…` entry (both).
    pub want_instrument: Option<bool>,
    /// Restrict to one binary format (`None` = both).
    pub format: Option<PluginFormat>,
}

impl PickFilter {
    fn admits(&self, format: PluginFormat, is_instrument: bool) -> bool {
        self.format.is_none_or(|wanted| wanted == format)
            && self
                .want_instrument
                .is_none_or(|wanted| wanted == is_instrument)
    }

    /// Heading shown above the filter field.
    fn heading(&self) -> &'static str {
        match (self.want_instrument, self.format) {
            (Some(true), Some(PluginFormat::Clap)) => "CLAP instruments",
            (Some(true), Some(PluginFormat::Vst3)) => "VST3 instruments",
            (Some(true), None) => "Instruments",
            (Some(false), Some(PluginFormat::Clap)) => "CLAP effects",
            (Some(false), Some(PluginFormat::Vst3)) => "VST3 effects",
            (Some(false), None) => "Effects",
            (None, Some(PluginFormat::Clap)) => "CLAP plug-ins",
            (None, Some(PluginFormat::Vst3)) => "VST3 plug-ins",
            (None, None) => "All plug-ins",
        }
    }
}

/// A pickable plugin: a catalog entry, or a remembered one from an earlier
/// session (recents must survive a plugin disappearing from disk — selecting
/// a stale entry simply fails to instantiate and reports it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickChoice {
    /// Binary format.
    pub format: PluginFormat,
    /// Module path.
    pub path: String,
    /// CLAP plugin id / VST3 class id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Instrument or effect — decides the node category for the global search
    /// entry (spec D5).
    pub is_instrument: bool,
}

impl PickChoice {
    /// Short right-aligned tag in the list row.
    pub fn format_tag(&self) -> &'static str {
        match self.format {
            PluginFormat::Clap => "CLAP",
            PluginFormat::Vst3 => "VST3",
        }
    }
}

impl From<&PluginEntry> for PickChoice {
    fn from(entry: &PluginEntry) -> Self {
        Self {
            format: entry.format,
            path: entry.path.clone(),
            id: entry.id.clone(),
            name: entry.name.clone(),
            is_instrument: entry.is_instrument,
        }
    }
}

/// One remembered pick (spec D10).
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

impl From<&PickChoice> for RecentPlugin {
    fn from(choice: &PickChoice) -> Self {
        Self {
            format: choice.format,
            path: choice.path.clone(),
            id: choice.id.clone(),
            name: choice.name.clone(),
            is_instrument: choice.is_instrument,
        }
    }
}

impl RecentPlugin {
    fn as_choice(&self) -> PickChoice {
        PickChoice {
            format: self.format,
            path: self.path.clone(),
            id: self.id.clone(),
            name: self.name.clone(),
            is_instrument: self.is_instrument,
        }
    }
}

/// Pushes a pick to the front of the recent list, deduplicating by identity
/// and capping each category at [`RECENT_MAX`] (spec D10).
pub fn remember(recents: &mut Vec<RecentPlugin>, pick: &PickChoice) {
    recents.retain(|recent| {
        !(recent.format == pick.format && recent.path == pick.path && recent.id == pick.id)
    });
    recents.insert(0, RecentPlugin::from(pick));
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

/// Catalog entries admitted by `filter` and matching `query`, in catalog order.
pub fn visible<'a>(
    entries: &'a [PluginEntry],
    filter: PickFilter,
    query: &'a str,
) -> impl Iterator<Item = &'a PluginEntry> + 'a {
    entries
        .iter()
        .filter(move |entry| filter.admits(entry.format, entry.is_instrument))
        .filter(move |entry| matches_query(&entry.name, &entry.path, query))
}

/// Recent entries admitted by `filter` (`All` rows still obey the query).
pub fn visible_recents<'a>(
    recents: &'a [RecentPlugin],
    filter: PickFilter,
    query: &'a str,
) -> impl Iterator<Item = &'a RecentPlugin> + 'a {
    recents
        .iter()
        .filter(move |recent| filter.admits(recent.format, recent.is_instrument))
        .filter(move |recent| matches_query(&recent.name, &recent.path, query))
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

        // Rows: recents first (empty query only), then the catalog matches.
        // The list is rebuilt each frame — a few hundred owned clones are
        // cheaper than caching it against every catalog mutation.
        let mut rows: Vec<PickChoice> = Vec::new();
        if self.query.trim().is_empty() {
            rows.extend(
                visible_recents(recents, self.filter, "")
                    .take(RECENT_MAX)
                    .map(RecentPlugin::as_choice),
            );
        }
        rows.extend(visible(entries, self.filter, &self.query).map(PickChoice::from));
        self.row_count = rows.len();
        if self.selected >= rows.len() {
            self.selected = rows.len().saturating_sub(1);
        }

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
                            if self.query.trim().is_empty() && !rows.is_empty() {
                                let recent_count = visible_recents(recents, self.filter, "")
                                    .take(RECENT_MAX)
                                    .count();
                                if recent_count > 0 {
                                    ui.weak("Recent");
                                    for choice in rows.iter().take(recent_count) {
                                        if row(
                                            ui,
                                            choice,
                                            index == self.selected,
                                            self.reveal,
                                            index,
                                        ) {
                                            clicked = Some(index);
                                        }
                                        index += 1;
                                    }
                                    ui.add_space(2.0);
                                    ui.weak("All plug-ins");
                                }
                            }
                            for choice in rows.iter().skip(index) {
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

    /// Empty list: either the scan is still running, or the query missed.
    fn empty_state(&mut self, ui: &mut egui::Ui, scan_progress: Option<(usize, usize)>) {
        if scan_progress.is_some() {
            ui.weak("Still scanning plug-ins — the list grows as modules finish.");
            return;
        }
        ui.add_space(4.0);
        ui.weak(format!("No match for \"{}\"", self.query.trim()));
        // Spec D9 兜底: a misclassified plugin must never be unreachable.
        if ui.button("Show all plug-ins…").clicked() {
            self.query.clear();
            self.selected = 0;
            self.reveal = true;
        }
    }

    /// Number of rows drawn on the last frame (footer / tests).
    pub fn row_count(&self) -> usize {
        self.row_count
    }
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
    let tag_width = 44.0;
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
            egui::RichText::new(choice.format_tag())
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
    response.on_hover_text(&choice.path).clicked()
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
    fn visible_honours_filter_and_query() {
        let entries = vec![
            entry(PluginFormat::Clap, "C:/clap/a.clap", "a", "Alpha", true),
            entry(PluginFormat::Vst3, "C:/vst3/b.vst3", "b", "Beta", false),
            entry(PluginFormat::Clap, "C:/clap/c.clap", "c", "Gamma", false),
        ];
        let instruments = visible(
            &entries,
            PickFilter {
                want_instrument: Some(true),
                format: None,
            },
            "",
        )
        .map(|entry| entry.id.as_str())
        .collect::<Vec<_>>();
        assert_eq!(instruments, vec!["a"]);

        let clap_effects = visible(
            &entries,
            PickFilter {
                want_instrument: Some(false),
                format: Some(PluginFormat::Clap),
            },
            "gam",
        )
        .map(|entry| entry.id.as_str())
        .collect::<Vec<_>>();
        assert_eq!(clap_effects, vec!["c"]);

        let all_vst3 = visible(
            &entries,
            PickFilter {
                want_instrument: None,
                format: Some(PluginFormat::Vst3),
            },
            "",
        )
        .count();
        assert_eq!(all_vst3, 1);
    }

    #[test]
    fn remember_dedups_and_caps_each_category() {
        let mut recents = Vec::new();
        for index in 0..7 {
            let choice = PickChoice {
                format: PluginFormat::Clap,
                path: format!("C:/clap/{index}.clap"),
                id: format!("id{index}"),
                name: format!("Plugin {index}"),
                is_instrument: index % 2 == 0,
            };
            remember(&mut recents, &choice);
        }
        // 4 even (instrument) + 3 odd (effect): both fit under the cap.
        assert_eq!(recents.len(), 7);

        // Re-picking an existing entry moves it to the front without growing.
        let existing = PickChoice {
            format: PluginFormat::Clap,
            path: "C:/clap/0.clap".to_string(),
            id: "id0".to_string(),
            name: "Plugin 0".to_string(),
            is_instrument: true,
        };
        remember(&mut recents, &existing);
        assert_eq!(recents.len(), 7);
        assert_eq!(recents[0].id, "id0");

        // Cap: 6 instruments, only the newest 5 survive.
        for index in 10..16 {
            remember(
                &mut recents,
                &PickChoice {
                    format: PluginFormat::Vst3,
                    path: format!("C:/vst3/{index}.vst3"),
                    id: format!("v{index}"),
                    name: format!("Inst {index}"),
                    is_instrument: true,
                },
            );
        }
        assert_eq!(
            recents.iter().filter(|recent| recent.is_instrument).count(),
            RECENT_MAX
        );
    }

    #[test]
    fn recents_filter_like_entries() {
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
        let instruments: Vec<&str> = visible_recents(
            &recents,
            PickFilter {
                want_instrument: Some(true),
                format: None,
            },
            "",
        )
        .map(|recent| recent.id.as_str())
        .collect();
        assert_eq!(instruments, vec!["a"]);

        let hits = visible_recents(&recents, PickFilter::default(), "beta").count();
        assert_eq!(hits, 1);
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
        picker.open(PickFilter {
            want_instrument: Some(true),
            format: None,
        });
        // Opening frame: no events, nothing picked, the popup stays open.
        assert!(frame(&ctx, &mut picker, &entries, &[], vec![], 0.0).is_none());
        assert!(picker.is_open());
        // ↓ moves the highlight to the second row…
        assert!(
            frame(
                &ctx,
                &mut picker,
                &entries,
                &[],
                vec![key_event(egui::Key::ArrowDown)],
                0.1
            )
            .is_none()
        );
        // …and Enter takes it.
        let outcome = frame(
            &ctx,
            &mut picker,
            &entries,
            &[],
            vec![key_event(egui::Key::Enter)],
            0.2,
        );
        match outcome {
            Some(PickOutcome::Picked(choice)) => {
                assert_eq!(choice.id, "b");
                assert_eq!(choice.path, "C:/clap/b.clap");
                assert!(choice.is_instrument);
            }
            other => panic!("expected a pick, got {other:?}"),
        }
        assert!(!picker.is_open());
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
        picker.open(PickFilter {
            want_instrument: Some(true),
            format: None,
        });
        // Opening frame: the menu's own click must not land on the list.
        assert!(frame(&ctx, &mut picker, &entries, &[], vec![], 0.0).is_none());
        // Settle frame (the opening frame is a sizing pass — rows may skip
        // drawing while the popup size is still unknown).
        assert!(frame(&ctx, &mut picker, &entries, &[], vec![], 0.05).is_none());

        // Rows carry explicit ids, so the test can address the first one
        // directly and click its centre — the same gesture as clicking the
        // plugin name with the mouse.
        let row_rect = ctx
            .read_response(egui::Id::new(("plugin_picker_row", 0)))
            .expect("row 0 drew once the popup settled")
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
            Some(PickOutcome::Picked(choice)) => assert_eq!(choice.name, "Alpha"),
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
        // 1 recent + 2 catalog rows.
        assert!(frame(&ctx, &mut picker, &entries, &recents, vec![], 0.0).is_none());
        assert_eq!(picker.row_count(), 3);
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
            Some(PickOutcome::Picked(choice)) => assert_eq!(choice.id, "z"),
            other => panic!("expected the recent pick, got {other:?}"),
        }
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
