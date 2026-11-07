//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    /// Assembles the offline export engine from the current document through
    /// the shared headless assembly (S0), then wraps it in the pumping job.
    ///
    /// S3c: `format` decides sample rate + bit depth (the engine is built at
    /// `format.sample_rate`, not the device's), and `passes` is the file list —
    /// the mix first, then optionally one file per audible track.
    pub(crate) fn start_export(
        &mut self,
        range: export_audio::ExportRange,
        format: export_audio::WavFormat,
        passes: Vec<export_audio::ExportPass>,
    ) -> Option<export_audio::ExportJob> {
        let Some(window) = range.window(self.project()) else {
            self.set_message("No song loop set — draw one on the ruler first".to_string());
            return None;
        };
        let project = self.project().clone();
        let mut media = headless::AppMedia(&mut *self);
        let graph = headless::assemble_offline(&project, format.sample_rate, window, &mut media);
        match export_audio::ExportJob::new(
            graph.engine,
            graph.guards,
            graph.node_map,
            format,
            window,
            passes,
        ) {
            Ok(job) => Some(job),
            Err(err) => {
                self.set_message(format!("Export failed to create the wav: {err}"));
                None
            }
        }
    }

    /// S3c: the pass list for one export. The mix is always first; with stems
    /// the job then renders one file per track that actually has content, in
    /// document order (MIDI tracks, then audio tracks).
    pub(crate) fn export_passes(
        &self,
        mix: &std::path::Path,
        stems: bool,
    ) -> Vec<export_audio::ExportPass> {
        let mut passes = vec![export_audio::ExportPass::mix(mix.to_path_buf())];
        if !stems {
            return passes;
        }
        let mut used: Vec<String> = Vec::new();
        let mut push = |name: String, target: export_audio::TrackSel, passes: &mut Vec<_>| {
            let base = export_audio::sanitize_file_part(&name);
            let mut unique = base.clone();
            let mut n = 2;
            while used.contains(&unique) {
                unique = format!("{base} {n}");
                n += 1;
            }
            used.push(unique.clone());
            passes.push(export_audio::ExportPass::stem(
                export_audio::stem_path(mix, &unique),
                name,
                target,
            ));
        };
        for (index, track) in self.project().midi_tracks.iter().enumerate() {
            let audible = track
                .clips
                .iter()
                .any(|clip| !clip.sequence.notes.is_empty() || !clip.sequence.ctrls.is_empty());
            if !audible {
                continue;
            }
            let name = if track.name.trim().is_empty() {
                format!("MIDI {}", index + 1)
            } else {
                track.name.clone()
            };
            push(name, export_audio::TrackSel::Midi(index), &mut passes);
        }
        for (index, track) in self.project().audio_tracks.iter().enumerate() {
            if !track.clips.iter().any(|clip| !clip.path.is_empty()) {
                continue;
            }
            let name = if track.name.trim().is_empty() {
                format!("Audio {}", index + 1)
            } else {
                track.name.clone()
            };
            push(name, export_audio::TrackSel::Audio(index), &mut passes);
        }
        passes
    }

    /// S3c: re-addresses the offline engine's clip tables for one pass — the
    /// target track keeps its content, every other audible track goes empty.
    /// The graph (instruments, effects, mixer) and the control tracks stay:
    /// automation and tempo rides are part of what a stem sounds like.
    pub(crate) fn set_export_pass_tables(
        &mut self,
        engine: &mut AudioEngine,
        node_map: &HashMap<u64, u64>,
        target: Option<export_audio::TrackSel>,
    ) {
        use export_audio::TrackSel;
        for track in 0..self.project().midi_tracks.len() {
            let keep = match target {
                None => true,
                Some(TrackSel::Midi(t)) => t == track,
                Some(TrackSel::Audio(_)) => false,
            };
            let table = if keep {
                self.rt_clips(track)
            } else {
                empty_midi_clip_table()
            };
            let _ = engine.send_midi_clips(track, table);
        }
        for track in 0..self.project().audio_tracks.len() {
            let keep = match target {
                None => true,
                Some(TrackSel::Audio(t)) => t == track,
                Some(TrackSel::Midi(_)) => false,
            };
            let Some(&engine_id) = self
                .project()
                .graph
                .nodes
                .iter()
                .find(|n| matches!(n, NodeDoc::AudioTrack { track: t, .. } if *t == track))
                .map(NodeDoc::node_id)
                .and_then(|doc_id| node_map.get(&doc_id))
            else {
                continue;
            };
            let table = if keep {
                self.build_audio_clip_table(track)
            } else {
                empty_clip_table()
            };
            let _ = engine.send_audio_clips(engine_id, table);
        }
    }

    /// Pumps the running export at the end of the UI frame (U7 D1, the
    /// plugin-scan slicing precedent) and finishes/cancels it.
    pub(crate) fn pump_export(&mut self) {
        let Some(mut job) = self.export_job.take() else {
            return;
        };
        let done = {
            let mut setup = |engine: &mut AudioEngine,
                             map: &HashMap<u64, u64>,
                             target: Option<export_audio::TrackSel>| {
                self.set_export_pass_tables(engine, map, target);
            };
            job.pump(export_audio::BLOCKS_PER_FRAME, &mut setup)
        };
        if !done {
            self.export_job = Some(job);
            return;
        }
        if job.cancelled {
            // The unfinished file is a partial wav — deleting it is kinder
            // than leaving a file that looks like a render.
            let partial = job.path().to_path_buf();
            if !job.outputs().contains(&partial) {
                let _ = std::fs::remove_file(&partial);
            }
            self.set_message(format!(
                "Export cancelled ({} file(s) kept)",
                job.outputs().len()
            ));
            log::info!("Audio export cancelled");
            return;
        }
        let outputs: Vec<PathBuf> = job.outputs().to_vec();
        match outputs.as_slice() {
            [] => self.set_message("Export finished with no output".to_string()),
            [single] => {
                self.set_message(format!("Exported to {}", single.display()));
                log::info!("Audio export written to {}", single.display());
            }
            [mix, stems @ ..] => {
                self.set_message(format!(
                    "Exported {} + {} stem(s) to {}",
                    display_name(mix),
                    stems.len(),
                    mix.parent().map_or_else(
                        || mix.display().to_string(),
                        |dir| dir.display().to_string()
                    )
                ));
                log::info!(
                    "Audio export written to {} (+{} stems)",
                    mix.display(),
                    stems.len()
                );
            }
        }
    }

    /// Export progress banner above the transport (U7 D1: same slot the
    /// plugin-scan banner uses).
    pub(crate) fn export_banner(&mut self, ui: &mut egui::Ui) {
        let Some(job) = self.export_job.as_mut() else {
            return;
        };
        let progress = job.progress();
        let mut cancel = false;
        let (index, total) = job.pass_number();
        // A single pass is just "Exporting"; stems name the file being
        // rendered so a long run does not look stuck on one file.
        let label = if total > 1 {
            format!(
                "Exporting {index}/{total} — {} — {}",
                job.label(),
                job.path().display()
            )
        } else {
            format!("Exporting — {}", job.path().display())
        };
        egui::Panel::top("glitcherz_export").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::ProgressBar::new(progress)
                        .desired_width(160.0)
                        .text(format!("{}%", (progress * 100.0) as u32)),
                );
                ui.weak(label);
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if cancel && let Some(job) = self.export_job.as_mut() {
            job.cancelled = true;
        }
    }

    /// Range-selection modal before an export starts (U7 D2).
    pub(crate) fn export_dialog(&mut self, ctx: &egui::Context) {
        if !self.export_dialog_open {
            return;
        }
        let mut close = false;
        let mut start = false;
        let loop_valid = self.project().metadata.loop_range.enabled
            && self.project().metadata.loop_range.end_ticks
                > self.project().metadata.loop_range.start_ticks
            && self.project().metadata.loop_range.end_ticks > 0;
        let modal = egui::Modal::new(egui::Id::unique("glitcherz_export_range")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.heading("Export Audio");
            ui.label(format!(
                "{} · {} kHz · stereo wav",
                self.export_bits.label(),
                f64::from(self.export_sample_rate) / 1000.0
            ));
            ui.add_space(8.0);
            ui.radio_value(
                &mut self.export_range_choice,
                export_audio::ExportRange::FullSong,
                "Full song",
            );
            ui.radio_value(
                &mut self.export_range_choice,
                export_audio::ExportRange::SongLoop,
                "Song loop",
            )
            .on_disabled_hover_text(if loop_valid {
                ""
            } else {
                "No song loop set — Ctrl+drag on the ruler to draw one"
            })
            .on_hover_text("The enabled loop range on the ruler");
            ui.add_space(10.0);
            // S3c: format controls. 24-bit is the delivery default; the rate
            // list leads with the device rate (the offline engine is built at
            // whatever is chosen, so a 96 kHz bounce does not need a 96 kHz
            // device).
            let rates = export_audio::sample_rate_choices(self.engine.info().sample_rate);
            ui.horizontal(|ui| {
                ui.label("Depth");
                for bits in export_audio::Bits::ALL {
                    ui.radio_value(&mut self.export_bits, bits, bits.label());
                }
            });
            ui.horizontal(|ui| {
                ui.label("Rate");
                egui::ComboBox::from_id_salt("glitcherz_export_rate")
                    .selected_text(format!(
                        "{} kHz",
                        f64::from(self.export_sample_rate) / 1000.0
                    ))
                    .show_ui(ui, |ui| {
                        for rate in rates {
                            ui.selectable_value(
                                &mut self.export_sample_rate,
                                rate,
                                format!("{} kHz", f64::from(rate) / 1000.0),
                            );
                        }
                    });
            });
            ui.checkbox(
                &mut self.export_stems,
                "Also write stems (one file per track)",
            )
            .on_hover_text("Renders the mix, then every track that has content on its own");
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    close = true;
                }
                let can_start =
                    self.export_range_choice == export_audio::ExportRange::FullSong || loop_valid;
                if ui
                    .add_enabled(can_start, egui::Button::new("Choose File…"))
                    .clicked()
                {
                    start = true;
                }
            });
        });
        if modal.should_close() {
            close = true;
        }
        if close || start {
            self.export_dialog_open = false;
        }
        if start {
            let range = self.export_range_choice;
            let format = export_audio::WavFormat {
                sample_rate: self.export_sample_rate,
                bits: self.export_bits,
            };
            let stems = self.export_stems;
            let stem = self
                .project_path
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".to_string());
            if let Some(path) = rfd::FileDialog::new()
                .set_file_name(format!("{stem}.wav"))
                .add_filter("Wave", &["wav"])
                .save_file()
            {
                let passes = self.export_passes(&path, stems);
                let pass_count = passes.len();
                if let Some(job) = self.start_export(range, format, passes) {
                    self.export_job = Some(job);
                    log::info!(
                        "Audio export started: {} ({pass_count} pass(es), {} · {} Hz)",
                        path.display(),
                        format.bits.label(),
                        format.sample_rate
                    );
                }
            }
        }
    }
}
