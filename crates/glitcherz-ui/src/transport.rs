//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    pub(crate) fn send_sequencer(&mut self, cmd: SequencerCommand) {
        self.engine.send(EngineCommand::Sequencer(cmd));
    }

    /// Transport stop, with the recording edge: the take is closed **before**
    /// the transport rewinds (its end position would otherwise read 0).
    pub(crate) fn stop_transport(&mut self) {
        if self.is_recording() {
            self.finish_recording();
        }
        self.send_sequencer(SequencerCommand::Stop);
    }

    /// Transport pause, with the same recording edge: a take cannot survive a
    /// pause (the gap would become one endless note), so it is closed here and
    /// the position is kept for the next Play.
    pub(crate) fn pause_transport(&mut self) {
        if self.is_recording() {
            self.finish_recording();
        }
        self.engine.pause();
    }

    pub(crate) fn transport_panel(&mut self, ui: &mut egui::Ui) {
        let playing = self.engine.is_playing();
        let response = egui::Panel::top("glitcherz_transport").show(ui, |ui| {
            ui.horizontal(|ui| {
                // U2: the file menu is the single entry point for project
                // work (the old inline Open / Save / Recent buttons live
                // here now).
                ui.menu_button("File", |ui| {
                    if ui
                        .add(egui::Button::new("New").shortcut_text("Ctrl+N"))
                        .clicked()
                    {
                        ui.close();
                        self.request_file_action(FileAction::New);
                    }
                    if ui
                        .add(egui::Button::new("Open…").shortcut_text("Ctrl+O"))
                        .clicked()
                    {
                        ui.close();
                        self.open_project_dialog();
                    }
                    ui.menu_button("Open Recent", |ui| {
                        if self.recent.is_empty() {
                            ui.weak("(empty)");
                        }
                        let recent = self.recent.clone();
                        for path in recent {
                            if ui.button(display_name(&path)).clicked() {
                                ui.close();
                                self.request_file_action(FileAction::Open(path));
                            }
                        }
                    });
                    ui.separator();
                    if ui
                        .add(egui::Button::new("Save").shortcut_text("Ctrl+S"))
                        .clicked()
                    {
                        ui.close();
                        self.save_project();
                    }
                    if ui
                        .add(egui::Button::new("Save As…").shortcut_text("Ctrl+Shift+S"))
                        .clicked()
                    {
                        ui.close();
                        self.save_project_as();
                    }
                    ui.separator();
                    if ui.button("Import MIDI…").clicked() {
                        ui.close();
                        self.import_midi_dialog();
                    }
                    if ui.button("Export MIDI…").clicked() {
                        ui.close();
                        self.export_midi_dialog();
                    }
                    if ui.button("Export Audio…").clicked() {
                        ui.close();
                        // U7: one export at a time — a running job keeps its
                        // snapshot; the menu simply refuses while it runs.
                        if self.export_job.is_none() {
                            self.export_dialog_open = true;
                        } else {
                            self.set_message("An export is already running".to_string());
                        }
                    }
                    ui.separator();
                    // S3b: make the project folder self-contained, and fix up
                    // paths after a move.
                    if ui
                        .button("Collect Media Files…")
                        .on_hover_text(
                            "Copy every referenced sample / SoundFont / wav into <project>.media/",
                        )
                        .clicked()
                    {
                        ui.close();
                        self.collect_media_files();
                    }
                    if ui
                        .add_enabled(
                            !self.missing_media.is_empty(),
                            egui::Button::new("Relocate Missing Media…"),
                        )
                        .clicked()
                    {
                        ui.close();
                        self.relocate_missing_media();
                    }
                });
                ui.menu_button("Edit", |ui| {
                    let undo_text = match self.session.undo_label() {
                        Some(label) => format!("Undo {label}"),
                        None => "Undo".to_string(),
                    };
                    let redo_text = match self.session.redo_label() {
                        Some(label) => format!("Redo {label}"),
                        None => "Redo".to_string(),
                    };
                    if ui
                        .add_enabled(
                            self.session.can_undo(),
                            egui::Button::new(undo_text).shortcut_text("Ctrl+Z"),
                        )
                        .clicked()
                    {
                        ui.close();
                        self.undo();
                    }
                    if ui
                        .add_enabled(
                            self.session.can_redo(),
                            egui::Button::new(redo_text).shortcut_text("Ctrl+Y"),
                        )
                        .clicked()
                    {
                        ui.close();
                        self.redo();
                    }
                    ui.separator();
                    if ui.button("Undo History…").clicked() {
                        ui.close();
                        self.undo_history_open = true;
                    }
                    ui.separator();
                    if ui.button("Preferences…").clicked() {
                        ui.close();
                        self.preferences_draft = self.output_config.clone();
                        self.preferences_error = None;
                        self.preferences_devices.clear();
                        self.preferences_open = true;
                    }
                });
                ui.separator();
                // Transport group (U1): rewind / play-pause / stop /
                // skip-to-end, then the song-loop toggle and marker jumps.
                if ui
                    .button("⏮")
                    .on_hover_text("Return to start (Home)")
                    .clicked()
                {
                    self.engine.seek_ticks(0.0);
                }
                let play_label = if playing { "⏸ Pause" } else { "▶ Play" };
                let mut play_btn = egui::Button::new(play_label);
                if playing {
                    play_btn = play_btn.fill(egui::Color32::DARK_GREEN);
                }
                if ui
                    .add(play_btn)
                    .on_hover_text(if self.is_recording() {
                        "Punch out: end the take and keep playing (Space)"
                    } else {
                        "Space"
                    })
                    .clicked()
                {
                    if playing {
                        // R6 D32: punch out instead of pausing while a take runs.
                        if self.is_recording() {
                            self.finish_recording();
                        } else {
                            self.pause_transport();
                        }
                    } else {
                        self.send_sequencer(SequencerCommand::Play);
                    }
                }
                if ui
                    .button("■ Stop")
                    .on_hover_text(if self.is_recording() {
                        "End the take and rewind (a take stopped during the pre-roll has nothing to keep)"
                    } else {
                        "Stop and rewind"
                    })
                    .clicked()
                {
                    self.stop_transport();
                }
                // Record (spec `recording.md` R2): one take into the selected
                // track, starting on the next bar line after the pre-roll.
                let rec_active = self.is_recording();
                // Pre-roll is dead time for capture — label it, or the user
                // plays through the count-in and concludes nothing records.
                let rec_pre_roll = rec_active && !self.recorder.is_capturing();
                // `⏺` (U+23FA) ships in egui's icon font; `●` (U+25CF) only
                // exists in the monospace face, so it renders as a tofu box in
                // the proportional (button) font. Colour the dot separately:
                // the icon font is monochrome and follows the text format.
                let rec_label = if rec_pre_roll {
                    " Pre-roll…"
                } else if rec_active {
                    " Rec…"
                } else {
                    " Rec"
                };
                let rec_font = egui::TextStyle::Button.resolve(ui.style());
                let dot_color = if rec_active {
                    egui::Color32::from_rgb(255, 120, 120)
                } else {
                    egui::Color32::from_rgb(220, 60, 60)
                };
                let mut rec_text = egui::text::LayoutJob::default();
                rec_text.append(
                    "⏺",
                    0.0,
                    egui::TextFormat {
                        font_id: rec_font.clone(),
                        color: dot_color,
                        valign: egui::Align::Center,
                        ..Default::default()
                    },
                );
                rec_text.append(
                    rec_label,
                    0.0,
                    egui::TextFormat {
                        font_id: rec_font,
                        color: egui::Color32::PLACEHOLDER,
                        ..Default::default()
                    },
                );
                let mut rec_btn = egui::Button::new(rec_text);
                if rec_active {
                    rec_btn = rec_btn.fill(egui::Color32::from_rgb(150, 40, 40));
                }
                let rec_tip = if rec_pre_roll {
                    format!(
                        "Counting in — capture starts after {} bar(s) of pre-roll. \
                         Anything played now is not recorded.",
                        self.record_pre_roll_bars
                    )
                } else if rec_active {
                    let list = self
                        .recorder
                        .tracks()
                        .iter()
                        .map(|&track| format!("T{}", track + 1))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("Recording {list} — click to end the take")
                } else {
                    "Record into every track wired to a Timeline In recording port — R".to_string()
                };
                if ui.add(rec_btn).on_hover_text(rec_tip).clicked() {
                    self.toggle_recording();
                }
                if !rec_active {
                    // Arm indicator: the app's own answer to "will Rec do
                    // anything?" — read *before* the press, not from a refusal
                    // card after it.
                    match self.armed_recording_chip() {
                        Some(armed) => {
                            ui.weak(armed)
                                .on_hover_text("Armed recording inputs (Timeline In)");
                        }
                        None => {
                            ui.colored_label(
                                egui::Color32::from_rgb(235, 125, 115),
                                egui::RichText::new("no input").small(),
                            )
                            .on_hover_text(
                                "No track has a source wired into its Timeline In recording \
                                 input — open the Patch tab and drag one in (MIDI out → T1, \
                                 Input node → A1).",
                            );
                        }
                    }
                    let pre = ui.add(
                        egui::DragValue::new(&mut self.record_pre_roll_bars)
                            .range(0..=2)
                            .speed(0.1)
                            .suffix(" bar"),
                    );
                    pre.on_hover_text("Pre-roll before recording starts (no capture)");
                }
                ui.checkbox(&mut self.record_mute_other, "Mute others")
                    .on_hover_text(
                        "While recording, silence the sequenced notes of every other MIDI track",
                    );
                // R4/R5: the device picker and the input meter live on the Audio In
                // node and its Inspector panel — the transport keeps only the
                // take settings (pre-roll, mute others, manual alignment).
                ui.label("Latency").on_hover_text(
                    "Manual input latency offset in samples: positive moves audio takes \
                     earlier on the timeline. Session-only — it depends on this device and \
                     buffer size, so it is not saved with the project.",
                );
                ui.add(
                    egui::DragValue::new(&mut self.record_latency_samples)
                        .range(-48_000..=48_000)
                        .speed(1.0)
                        .suffix(" smp"),
                )
                .on_hover_text(
                    "Manual input latency offset: positive moves audio takes earlier on the timeline",
                );
                if ui.button("⏭").on_hover_text("Skip to end (End)").clicked() {
                    self.engine
                        .seek_ticks(f64::from(self.arrangement_end_ticks()));
                }
                let loop_on = self.project().metadata.loop_range.is_active();
                let loop_btn = egui::Button::new("Loop").selected(loop_on);
                if ui
                    .add(loop_btn)
                    .on_hover_text("Song loop (drag it out on the ruler with Ctrl)")
                    .clicked()
                {
                    let enable = !loop_on;
                    self.timeline_toggle_loop(enable);
                }
                // Metronome (spec `recording.md` R1): the click is generated
                // inside the engine after the graph, so it keeps sounding with
                // every track muted and never reaches the exported file.
                let metro_btn = egui::Button::new("Click").selected(self.metronome_on);
                if ui
                    .add(metro_btn)
                    .on_hover_text("Metronome (audible while the transport plays, not exported)")
                    .clicked()
                {
                    self.metronome_on = !self.metronome_on;
                    self.metronome_pending = true;
                }
                if self.metronome_on {
                    let level = ui
                        .add(
                            egui::Slider::new(&mut self.metronome_gain, 0.0..=1.0)
                                .show_value(false)
                                .max_decimals(2),
                        )
                        .on_hover_text("Click level");
                    if level.changed() {
                        self.metronome_pending = true;
                    }
                }
                // Retry a toggle that hit a full command queue (the queue
                // drains every block, so one frame is usually enough).
                if self.metronome_pending
                    && self
                        .engine
                        .set_metronome(self.metronome_on, self.metronome_gain)
                {
                    self.metronome_pending = false;
                }
                if ui
                    .button("‹")
                    .on_hover_text("Previous marker (,)")
                    .clicked()
                {
                    self.seek_to_previous_marker();
                }
                if ui.button("›").on_hover_text("Next marker (.)").clicked() {
                    self.seek_to_next_marker();
                }
                ui.separator();
                ui.label("Tempo");
                // U2: the value is read into a local first — writing the
                // document every frame would keep `Session::is_dirty` (and
                // the title's `*`) permanently true.
                let before = self.begin_param_edit();
                let mut tempo = self.project().metadata.tempo_bpm;
                let bpm = ui.add(
                    egui::DragValue::new(&mut tempo)
                        .range(*BPM_RANGE.start()..=*BPM_RANGE.end())
                        .speed(0.5)
                        .suffix(" BPM"),
                );
                if bpm.changed() {
                    self.project_mut().metadata.tempo_bpm = tempo;
                    // One drag = one undo step; keyboard nudges are separate steps.
                    if bpm.dragged() {
                        self.commit_param_edit("Tempo", before, "tempo");
                    } else {
                        self.commit_edit("Tempo", before);
                    }
                    self.send_sequencer(SequencerCommand::SetBpm(convert::to_f32(tempo)));
                }
                if bpm.drag_stopped() {
                    self.end_param_edit();
                }
                // U3b: a tempo curve overrides the transport, so show the
                // effective value it is actually running at (read-only — the
                // field above stays the user's base tempo).
                let live_bpm = self.engine.live_bpm();
                if self.engine.is_playing() && (live_bpm - convert::to_f32(tempo)).abs() > 0.05 {
                    ui.label(
                        egui::RichText::new(format!("→ {live_bpm:.1} auto"))
                            .small()
                            .color(egui::Color32::from_rgb(230, 190, 70)),
                    )
                    .on_hover_text("Tempo curve (Timeline In) is driving the clock");
                }
                ui.separator();
                // U3a: meter. Same read-local-then-write rule as tempo — the
                // dropdown must not touch the document every frame.
                ui.label("Meter");
                let meter = self.project().metadata.time_signature;
                let mut chosen = meter;
                egui::ComboBox::from_id_salt("glitcherz_meter")
                    .selected_text(meter.to_string())
                    .width(52.0)
                    .show_ui(ui, |ui| {
                        for (numerator, denominator) in METER_PRESETS {
                            let candidate =
                                glitcherz_model::TimeSignature::new(numerator, denominator);
                            ui.selectable_value(&mut chosen, candidate, candidate.to_string());
                        }
                    });
                if chosen != meter {
                    self.set_meter(chosen);
                }
                ui.separator();
                self.master_section(ui);
            });
        });
        // Chrome: hovering the transport releases any editor's focus.
        self.focus_router.register_neutral(response.response.rect);
    }

    /// Master group on the transport bar: output gain, stereo peak meter and
    /// the panic button. It lives **outside** the view tabs because it
    /// belongs to the song, not to the selected node — the Inspector only
    /// edits node parameters.
    pub(crate) fn master_section(&mut self, ui: &mut egui::Ui) {
        let [left, right] = self.engine.master_peak_lr();
        self.meter_l.update(left);
        self.meter_r.update(right);

        ui.label("Master");
        // Same rule as the tempo field: no document write unless the slider
        // actually moved, or the title claims unsaved changes forever.
        let before = self.begin_param_edit();
        let mut gain = self.project().master_gain;
        let slider_width = ui.spacing().slider_width;
        ui.spacing_mut().slider_width = 96.0;
        let response = ui.add(
            egui::Slider::new(&mut gain, MASTER_GAIN_RANGE)
                .custom_formatter(|value, _| gain_db_text(value as f32)),
        );
        ui.spacing_mut().slider_width = slider_width;
        if response.changed() {
            self.project_mut().master_gain = gain;
            self.commit_param_edit("Master Gain", before, "master-gain");
            self.engine.send(EngineCommand::SetParam {
                id: ParamId::MasterGain,
                value: gain,
            });
        }
        if response.drag_stopped() {
            self.end_param_edit();
        }

        // Panic first, meter last: the meter stretches over the remaining
        // row width (long run-out) to the right of the Panic button.
        if ui
            .button("Panic")
            .on_hover_text("Release every sounding note — the transport keeps running")
            .clicked()
        {
            self.engine.send(EngineCommand::Panic);
        }
        master_meter(ui, [self.meter_l.held, self.meter_r.held]);
    }

    /// Arrangement end in ticks (U1: the skip-to-end target) — the latest
    /// clip end across all tracks, rounded up to a bar (mirrors the
    /// engine's D16 rule); `0` when the arrangement is empty.
    pub(crate) fn arrangement_end_ticks(&self) -> u32 {
        let bar = u64::from(self.project().ticks_per_bar());
        let mut latest = 0u64;
        for track in &self.project().midi_tracks {
            for clip in &track.clips {
                latest = latest.max(u64::from(clip.start_ticks) + u64::from(clip.span_ticks));
            }
        }
        for track in &self.project().control_tracks {
            for clip in &track.clips {
                latest = latest.max(u64::from(clip.start_ticks) + u64::from(clip.span_ticks));
            }
        }
        let end = if latest == 0 {
            0
        } else {
            latest.div_ceil(bar) * bar
        };
        end.min(u64::from(u32::MAX)) as u32
    }

    /// Moves the playhead by whole bars at the current meter (Ctrl+←/→).
    /// Never goes before the arrangement start; moving past the end is
    /// allowed, like dragging the playhead there.
    pub(crate) fn seek_by_bars(&mut self, bars: i64) {
        let bar_ticks = f64::from(self.project().ticks_per_bar());
        let pos = f64::from(self.engine.musical_ticks());
        let target = (pos + bar_ticks * bars as f64).max(0.0);
        self.engine.seek_ticks(target);
    }

    /// Seeks to the nearest marker strictly before the playhead (U1);
    /// without one, back to the arrangement start.
    pub(crate) fn seek_to_previous_marker(&mut self) {
        let pos = f64::from(self.engine.musical_ticks());
        let target = self
            .project()
            .metadata
            .markers
            .iter()
            .rev()
            .find(|m| f64::from(m.tick) < pos - 1e-6)
            .map_or(0.0, |m| f64::from(m.tick));
        self.engine.seek_ticks(target);
    }

    /// Seeks to the nearest marker strictly after the playhead (U1);
    /// without one, to the arrangement end.
    pub(crate) fn seek_to_next_marker(&mut self) {
        let pos = f64::from(self.engine.musical_ticks());
        let target = self
            .project()
            .metadata
            .markers
            .iter()
            .find(|m| f64::from(m.tick) > pos + 1e-6)
            .map_or_else(
                || f64::from(self.arrangement_end_ticks()),
                |m| f64::from(m.tick),
            );
        self.engine.seek_ticks(target);
    }

    /// Arrangement timeline panel (timeline spec T3): rows = tracks, clip
    /// blocks with move/resize/insert/delete, double-click opens the editor.
    /// Main view tab bar (stacks under the transport panel).
    pub(crate) fn view_tabs(&mut self, ui: &mut egui::Ui) {
        let response = egui::Panel::top("glitcherz_views").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view, MainView::Timeline, "Timeline");
                ui.selectable_value(&mut self.view, MainView::Edit, "Edit");
                ui.selectable_value(&mut self.view, MainView::Patch, "Patch");
                ui.selectable_value(&mut self.view, MainView::Inspector, "Inspector");
            });
        });
        // Chrome: switching tabs releases the previous view's focus.
        self.focus_router.register_neutral(response.response.rect);
    }
}
