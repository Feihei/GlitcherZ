//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    /// Edit tab: the clip editor as a main view. Its target is the timeline's
    /// clip selection (see [`Self::sync_edit_target`]), so this panel never
    /// opens or closes anything itself — with no editable clip selected it
    /// states that and points back at the Timeline, like the Inspector does.
    pub(crate) fn edit_panel(&mut self, ui: &mut egui::Ui) {
        // Cloned rather than borrowed: the body needs `&mut self` (focus chip
        // strip, roll state), and a roll subject is a handful of indices.
        let Some(target) = self.edit_clip.clone() else {
            let audio_only = !self.timeline_state.selection.is_empty();
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.heading("Edit");
                ui.add_space(8.0);
                if audio_only {
                    ui.label("Audio clips have no editor.");
                    ui.add_space(4.0);
                    ui.weak(
                        "Select a MIDI clip (piano roll) or a curve clip (curve editor) \
                         in the Timeline.",
                    );
                } else {
                    ui.label("No clip selected.");
                    ui.add_space(4.0);
                    ui.weak(
                        "Go back to the Timeline and select a clip — it opens here, \
                         exactly like a double-click.",
                    );
                }
            });
            return;
        };
        egui::ScrollArea::vertical().show(ui, |ui| match target {
            EditTarget::Midi(subject) => self.midi_editor_body(ui, &subject),
            EditTarget::Control { track, clip } => self.control_editor_body(ui, track, clip),
        });
    }

    /// U9 E2: thin banner shown while the startup plugin scan still has
    /// modules queued.
    ///
    /// It names the module the frame will block on at its end (spec D14), so
    /// a plugin that hangs inside its metadata read leaves its own name on
    /// screen — the only diagnostic in-process scanning can offer.
    pub(crate) fn plugin_scan_banner(&self, ui: &mut egui::Ui) {
        let Some(upcoming) = self.plugin_catalog.upcoming() else {
            return;
        };
        let (done, total) = self.plugin_catalog.progress();
        let name = upcoming
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| upcoming.path.display().to_string());
        egui::Panel::top("glitcherz_plugin_scan").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                        .desired_width(140.0)
                        .text(format!("{done}/{total}")),
                );
                ui.weak(format!("Scanning plug-ins — {name}"));
            });
        });
    }

    /// U9 E2: scans exactly one module at the **end** of the frame (spec D14).
    ///
    /// A module that fails to load yields nothing; progress advances either
    /// way so the queue always drains.
    pub(crate) fn plugin_scan_step(&mut self) {
        let Some(candidate) = self.plugin_catalog.take_next() else {
            return;
        };
        let entries = plugin_catalog::scan_module(&candidate);
        self.plugin_catalog.accept(entries);
    }

    pub(crate) fn status_panel(&mut self, ui: &mut egui::Ui) {
        let info = self.engine.info().clone();
        let position = self.engine.position_seconds();
        let note_text = self
            .current_note
            .map(note_name)
            .unwrap_or_else(|| "—".to_string());
        let track_name = self
            .project()
            .midi_tracks
            .get(self.selected_track)
            .map(|t| {
                if t.name.is_empty() {
                    format!("Track {}", self.selected_track + 1)
                } else {
                    t.name.clone()
                }
            })
            .unwrap_or_else(|| "—".to_string());
        // G6: where the computer keyboard goes (the on-screen keyboard panel
        // is gone — the patch decides, so the status bar says where).
        let keys_text = if self.vk_targets.is_empty() && self.vk_track_targets.is_empty() {
            "none".to_string()
        } else {
            let mut parts = Vec::new();
            if !self.vk_track_targets.is_empty() {
                let list = self
                    .vk_track_targets
                    .iter()
                    .map(|t| format!("T{}", t + 1))
                    .collect::<Vec<_>>()
                    .join(" ");
                parts.push(format!(
                    "{} rec track(s) {list}",
                    self.vk_track_targets.len()
                ));
            }
            if !self.vk_targets.is_empty() {
                let mut tracks: Vec<usize> = self
                    .vk_targets
                    .iter()
                    .filter_map(|doc| self.project().graph.node_by_id(*doc).map(NodeDoc::track))
                    .map(|track| track + 1)
                    .collect();
                tracks.sort_unstable();
                tracks.dedup();
                let list = tracks
                    .iter()
                    .map(|t| format!("T{t}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                parts.push(format!("{} synth(s) {list}", self.vk_targets.len()));
            }
            parts.join(" · ")
        };
        // Status message (disappears automatically after the timeout). This
        // used to be `take_if(.. elapsed() < TTL)`, which *consumed* the
        // message on the first frame that drew it: every notice — including
        // "sound font failed to load" — painted for one frame and read as if
        // the action had failed silently. Expire it, don't steal it.
        if let Some((_, at, kind)) = self.status_message
            && at.elapsed()
                >= match kind {
                    MessageKind::Info => STATUS_MESSAGE_TTL,
                    MessageKind::Error => STATUS_ERROR_TTL,
                }
        {
            self.status_message = None;
        }
        let message = self.status_message.clone();

        let response = egui::Panel::bottom("glitcherz_status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("Device: {}", info.device_name));
                ui.separator();
                ui.label(format!(
                    "{} Hz / {} ch",
                    info.sample_rate, info.out_channels
                ));
                ui.separator();
                ui.label(format!("Editing: {track_name}"));
                ui.separator();
                ui.label(format!("Note: {note_text}"));
                ui.separator();
                ui.label(format!("Keys → {keys_text}"));
                ui.separator();
                // Musical transport position (bar.beat, from the sequencer's tick counter —
                // frozen while stopped). The raw seconds counter is engine uptime (audio
                // callback keeps rendering silent blocks), kept as a secondary diagnostic.
                let ticks = self.engine.musical_ticks();
                // U3a: bar length and beat unit both come from the document's
                // meter, so 6/8 counts six beats while 3/4 counts three.
                let meter = self.project().metadata.time_signature;
                let tpb = meter.ticks_per_bar() as f32;
                let bar = (ticks / tpb).floor().max(0.0);
                let beat = (ticks - bar * tpb) / meter.ticks_per_beat() as f32 + 1.0;
                ui.label(format!("Pos {}.{}", bar as u32 + 1, beat));
                ui.separator();
                ui.label(format!("Uptime {:.1} s", position));
                if std::env::var_os("GLITCHERZ_FOCUS_DEBUG").is_some() {
                    ui.separator();
                    let ctx = ui.ctx().clone();
                    let (pos, down) =
                        ctx.input(|i| (i.pointer.latest_pos(), i.pointer.any_down()));
                    let focused = ctx.memory(|m| m.focused());
                    let (kb, nz) = self.focus_router.registry_size();
                    let retained = self.focus_router.last_known_pos();
                    ui.monospace(format!(
                        "focus: live={pos:?} retained={retained:?} down={down} owner={focused:?} kb={kb} nz={nz}"
                    ));
                }
                if let Some((text, _, kind)) = message {
                    ui.separator();
                    let color = match kind {
                        MessageKind::Info => egui::Color32::LIGHT_BLUE,
                        MessageKind::Error => egui::Color32::from_rgb(255, 120, 110),
                    };
                    ui.colored_label(color, text);
                }
            });
        });
        // Chrome: hovering the status bar releases any editor's focus.
        self.focus_router.register_neutral(response.response.rect);
    }
}
