//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    /// Applies a new output setup (S1-5): builds the replacement engine first,
    /// then swaps it in and rebuilds the document at the new sample rate. On
    /// failure the old engine keeps running and the reason stays in the window.
    pub(crate) fn apply_output_config(&mut self, config: OutputConfig) {
        if output_config_matches(self.engine.info(), &config) {
            self.output_config = config;
            self.preferences_error = None;
            self.preferences_open = false;
            return;
        }
        self.close_plugin_editor();
        match AudioEngine::start_with(config.clone()) {
            Ok(engine) => {
                let old = std::mem::replace(&mut self.engine, engine);
                // Stop the old stream before releasing its plugin guards: the
                // old graph (and every node in it) dies with the stream.
                drop(old);
                self.voice_guards.clear();
                self.retiring_guards.clear();
                self.node_map.clear();
                self.output_config = config;
                self.preferences_error = None;
                self.preferences_open = false;
                self.last_stream_error = None;
                // The document is sample-rate agnostic (tick domain); the
                // rebuild re-instantiates every processor at the new rate.
                self.apply_document();
                if self.audio_device.is_some() {
                    let source = self.record_source();
                    if let Err(err) = self.engine.reopen_input(&source) {
                        self.set_error(format!(
                            "Audio output applied, but the input device failed: {err}"
                        ));
                    }
                }
                self.set_message(format!(
                    "Audio output: {} ({} Hz)",
                    self.engine.info().device_name,
                    self.engine.info().sample_rate
                ));
            }
            Err(err) => {
                self.preferences_error = Some(err.to_string());
            }
        }
    }

    /// Preferences window (S1-5): output device / sample rate / buffer size.
    /// Edits live in a draft; Apply is the only commit point.
    pub(crate) fn preferences_panel(&mut self, ctx: &egui::Context) {
        if !self.preferences_open {
            return;
        }
        if self.preferences_devices.is_empty() {
            self.preferences_devices = glitcherz_engine::available_outputs();
        }
        let mut open = self.preferences_open;
        let mut apply = false;
        let devices = self.preferences_devices.clone();
        egui::Window::new("Preferences")
            .id(egui::Id::new("glitcherz_preferences"))
            .open(&mut open)
            .default_width(380.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.heading("Audio output");
                if let Some(err) = &self.last_stream_error {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 140, 120),
                        format!("Last output error: {err}"),
                    );
                }
                ui.add_space(4.0);
                egui::Grid::new("audio_output_grid")
                    .num_columns(2)
                    .spacing([14.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Device");
                        let device_label = self
                            .preferences_draft
                            .device
                            .clone()
                            .unwrap_or_else(|| "System default".to_string());
                        egui::ComboBox::from_id_salt("output_device")
                            .selected_text(device_label)
                            .width(240.0)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(
                                        self.preferences_draft.device.is_none(),
                                        "System default",
                                    )
                                    .clicked()
                                {
                                    self.preferences_draft.device = None;
                                }
                                for device in &devices {
                                    let selected = self.preferences_draft.device.as_deref()
                                        == Some(device.as_str());
                                    if ui.selectable_label(selected, device).clicked() {
                                        self.preferences_draft.device = Some(device.clone());
                                    }
                                }
                            });
                        ui.end_row();

                        ui.label("Sample rate");
                        let rate_label = self.preferences_draft.sample_rate.map_or_else(
                            || "Device default".to_string(),
                            |rate| format!("{rate} Hz"),
                        );
                        egui::ComboBox::from_id_salt("output_rate")
                            .selected_text(rate_label)
                            .width(240.0)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(
                                        self.preferences_draft.sample_rate.is_none(),
                                        "Device default",
                                    )
                                    .clicked()
                                {
                                    self.preferences_draft.sample_rate = None;
                                }
                                for rate in [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000] {
                                    let selected = self.preferences_draft.sample_rate == Some(rate);
                                    if ui
                                        .selectable_label(selected, format!("{rate} Hz"))
                                        .clicked()
                                    {
                                        self.preferences_draft.sample_rate = Some(rate);
                                    }
                                }
                            });
                        ui.end_row();

                        ui.label("Buffer size");
                        let buffer_label = self.preferences_draft.buffer_size.map_or_else(
                            || "Device default".to_string(),
                            |frames| format!("{frames} frames"),
                        );
                        egui::ComboBox::from_id_salt("output_buffer")
                            .selected_text(buffer_label)
                            .width(240.0)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(
                                        self.preferences_draft.buffer_size.is_none(),
                                        "Device default",
                                    )
                                    .clicked()
                                {
                                    self.preferences_draft.buffer_size = None;
                                }
                                for frames in [64u32, 128, 256, 512, 1024] {
                                    let selected =
                                        self.preferences_draft.buffer_size == Some(frames);
                                    if ui
                                        .selectable_label(selected, format!("{frames} frames"))
                                        .clicked()
                                    {
                                        self.preferences_draft.buffer_size = Some(frames);
                                    }
                                }
                            });
                        ui.end_row();
                    });
                ui.add_space(6.0);
                ui.weak(
                    "Applying rebuilds the audio graph and re-instantiates plug-ins. \
                     A device that cannot run the chosen rate keeps the previous output.",
                );
                if let Some(err) = &self.preferences_error {
                    ui.add_space(4.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 140, 120),
                        egui::RichText::new(err).strong(),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        apply = true;
                    }
                    if ui.button("Close").clicked() {
                        self.preferences_open = false;
                    }
                    if ui.button("Refresh devices").clicked() {
                        self.preferences_devices = glitcherz_engine::available_outputs();
                    }
                });
            });
        if !open {
            self.preferences_open = false;
        }
        if apply {
            let config = self.preferences_draft.clone();
            self.apply_output_config(config);
        }
    }

    /// Undo History panel (S1-4): every retained step plus the redo tail, newest
    /// first. Clicking a row restores the document state after that step.
    pub(crate) fn undo_history_panel(&mut self, ctx: &egui::Context) {
        if !self.undo_history_open {
            return;
        }
        let rows = self.session.history_rows();
        let mut open = self.undo_history_open;
        let mut jump = None;
        egui::Window::new("Undo History")
            .id(egui::Id::new("glitcherz_undo_history"))
            .open(&mut open)
            .default_width(320.0)
            .min_width(240.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.weak("Click a step to jump to the state after it.");
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .show(ui, |ui| {
                        for row in rows.iter().rev() {
                            let text = match &row.label {
                                Some(label) => format!("{}. {label}", row.depth),
                                None => "0. Initial state".to_string(),
                            };
                            let text = if row.future {
                                // Undone steps stay reachable: grey, not gone.
                                egui::RichText::new(text).weak()
                            } else {
                                egui::RichText::new(text)
                            };
                            let response = ui.selectable_label(row.current, text);
                            if response.clicked() && !row.current {
                                jump = Some(row.depth);
                            }
                        }
                    });
            });
        self.undo_history_open = open;
        if let Some(depth) = jump {
            self.jump_history_to(depth);
        }
    }

    /// Recording-source picker (Audio In node + its Inspector panel). One input
    /// stream means one source for every audio take — the label always shows
    /// what the stream is actually bound to, not just what was picked, so a
    /// fallback (unplugged device) is impossible to miss.
    ///
    /// R9: the list carries the host's render endpoints too, marked `↩`. Those
    /// are WASAPI loopback sources: they record what the machine plays.
    pub(crate) fn audio_device_menu(&mut self, ui: &mut egui::Ui) {
        if self.audio_sources.is_empty() {
            self.audio_sources = AudioEngine::record_sources();
        }
        let live = self.engine.input_info().cloned();
        // `🎙` / `🔊` (U+1F399 / U+1F50A) are not in egui's default font
        // set and paint as tofu boxes; `↩` (U+21A9) and plain text are.
        let label = match &live {
            Some(info) if info.loopback => {
                format!("↩ {}", short_device_name(&info.device_name))
            }
            Some(info) => format!("In · {}", short_device_name(&info.device_name)),
            None => "In · no input".to_string(),
        };
        let current = self.record_source();
        let mut pick: Option<RecordSource> = None;
        ui.menu_button(label, |ui| {
            ui.set_min_width(320.0);
            // R4: the picker lists devices, but the *stream* is the truth:
            // show what is open right now above the choices.
            match &live {
                Some(info) => {
                    ui.label(egui::RichText::new(&info.device_name).strong());
                    let kind = if info.loopback {
                        "WASAPI loopback (what this output plays)"
                    } else {
                        "capture input"
                    };
                    ui.weak(format!(
                        "{} · {} ch @ {} Hz",
                        kind, info.channels, info.sample_rate
                    ));
                }
                None => {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 120, 110),
                        "No input stream is open",
                    );
                    if let Some(err) = self.engine.input_error() {
                        ui.weak(err);
                    }
                }
            }
            ui.separator();
            if ui
                .selectable_label(
                    current == RecordSource::Default,
                    "System default (microphone)",
                )
                .clicked()
            {
                pick = Some(RecordSource::Default);
            }

            let sources = self.audio_sources.clone();
            let captures: Vec<&String> = sources
                .iter()
                .filter_map(|source| match source {
                    RecordSource::Capture(name) => Some(name),
                    _ => None,
                })
                .collect();
            let loopbacks: Vec<&String> = sources
                .iter()
                .filter_map(|source| match source {
                    RecordSource::Loopback(name) => Some(name),
                    _ => None,
                })
                .collect();

            ui.add_space(2.0);
            ui.weak("Inputs");
            if captures.is_empty() {
                ui.weak("(none found)");
            }
            for name in captures {
                let source = RecordSource::Capture(name.clone());
                if ui.selectable_label(current == source, name).clicked() {
                    pick = Some(source);
                }
            }

            ui.add_space(2.0);
            ui.weak("System playback (loopback)");
            if loopbacks.is_empty() {
                ui.weak("(none found)");
            }
            for name in loopbacks {
                let source = RecordSource::Loopback(name.clone());
                let hint = "WASAPI loopback: records everything this output device plays — \
                            other apps, and this project's own playback if the engine outputs \
                            to it.";
                if ui
                    .selectable_label(current == source, format!("↩ {name}"))
                    .on_hover_text(hint)
                    .clicked()
                {
                    pick = Some(source);
                }
            }

            ui.separator();
            if ui.button("Refresh devices").clicked() {
                self.audio_sources = AudioEngine::record_sources();
            }
        })
        .response
        .on_hover_text(
            "Recording source (one stream: applies to every audio take). ↩ entries record what \
             the machine plays.",
        );

        if let Some(source) = pick {
            self.pick_audio_source(source);
        }
    }
}
