//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    /// Save to the current path; if never saved, pop up the "Save As" dialog.
    /// Returns whether the document reached the disk — the unsaved-changes
    /// modal (U2) only proceeds when it did.
    pub(crate) fn save_project(&mut self) -> bool {
        match self.project_path.clone() {
            Some(path) => self.do_save(path),
            None => self.save_project_as(),
        }
    }

    /// "Save As": pop up the save dialog.
    pub(crate) fn save_project_as(&mut self) -> bool {
        let Some(mut path) = rfd::FileDialog::new()
            .add_filter("GlitcherZ Project", &["glz"])
            .set_file_name("Untitled.glz")
            .save_file()
        else {
            return false;
        };
        path.set_extension("glz");
        self.do_save(path)
    }

    /// "Open": pop up the file picker dialog (unsaved changes ask first).
    pub(crate) fn open_project_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("GlitcherZ Project", &["glz"])
            .pick_file()
        else {
            return;
        };
        self.request_file_action(FileAction::Open(path));
    }

    /// A destructive file action (new / open / quit) requested by the menu or
    /// a shortcut: run it while the document is clean, otherwise park it
    /// behind the unsaved-changes modal (U2).
    pub(crate) fn request_file_action(&mut self, action: FileAction) {
        if self.pending_file_action.is_some() {
            return;
        }
        if self.session.is_dirty() {
            self.pending_file_action = Some(action);
        } else {
            self.perform_file_action(action);
        }
    }

    /// Runs a file action the user has already approved.
    pub(crate) fn perform_file_action(&mut self, action: FileAction) {
        match action {
            FileAction::New => self.do_new(),
            FileAction::Open(path) => self.do_open(path),
            // `ui()` sends the viewport close once `allow_close` is up, so
            // this needs no egui context.
            FileAction::Exit => {
                // S3a: reaching Exit means the user answered the unsaved
                // prompt — saved (snapshot already gone) or discarded. Either
                // way there is nothing left to recover.
                self.clear_autosave();
                self.allow_close = true;
            }
        }
    }

    /// New project (U2): a fresh default document replaces the current one and
    /// the engine graph is rebuilt from it. Never called directly — go through
    /// [`Self::request_file_action`] so unsaved work is protected.
    pub(crate) fn do_new(&mut self) {
        self.close_plugin_editor();
        self.editor_user_open = false;
        // A take belongs to the document that is about to be replaced.
        self.cancel_recording();
        self.send_sequencer(SequencerCommand::Stop);
        self.session.replace(Project::default());
        self.project_path = None;
        // S3a: the snapshot clock belongs to the document that just went away.
        self.autosave_clock.reset(Instant::now());
        // S3b: a fresh document references no media.
        self.missing_media.clear();
        self.selected_track = 0;
        self.selected_auto = 0;
        self.edit_clip = None;
        self.edit_selection.clear();
        self.roll_state = piano_roll::RollState::new();
        self.timeline_state = timeline::TimelineState::default();
        self.patch_state = patch::PatchState::default();
        self.held_notes.clear();
        self.push_all_params();
        // U1: the fresh document's (default) loop range reaches the engine.
        self.push_loop_range();
        self.set_message("New project".to_string());
        log::info!("New project");
    }

    /// Unsaved-changes modal for a parked file action (U2). The backdrop and
    /// Escape cancel; the three buttons are drawn here.
    pub(crate) fn unsaved_changes_dialog(&mut self, ctx: &egui::Context) {
        let Some(action) = self.pending_file_action.clone() else {
            return;
        };
        let mut choice = None;
        let modal =
            egui::Modal::new(egui::Id::unique("glitcherz_unsaved_changes")).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.heading("Unsaved changes");
                ui.label(format!(
                    "The project has unsaved changes. Save them before you {}?",
                    action.verb()
                ));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        choice = Some(ConfirmChoice::Save);
                    }
                    if ui.button("Discard changes").clicked() {
                        choice = Some(ConfirmChoice::Discard);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(ConfirmChoice::Cancel);
                    }
                });
            });
        if modal.should_close() {
            choice = Some(ConfirmChoice::Cancel);
        }
        match choice {
            Some(ConfirmChoice::Save) => {
                // A failed or cancelled save keeps the dialog (and the action)
                // pending: nothing is discarded behind the user's back.
                if self.save_project() {
                    self.pending_file_action = None;
                    self.perform_file_action(action);
                }
            }
            Some(ConfirmChoice::Discard) => {
                self.pending_file_action = None;
                self.perform_file_action(action);
            }
            Some(ConfirmChoice::Cancel) => self.pending_file_action = None,
            None => {}
        }
    }

    /// Startup prompt for crash snapshots (S3a). A modal, not a status line:
    /// losing work silently is the failure this feature exists to prevent, so
    /// the decision has to be in front of the user.
    ///
    /// The snapshots stay on disk until the answer says otherwise — "Later"
    /// just closes the prompt (they are offered again next launch), and
    /// recovering one leaves the others alone.
    pub(crate) fn recovery_dialog(&mut self, ctx: &egui::Context) {
        if self.recovery.is_empty() {
            return;
        }
        let mut recover: Option<usize> = None;
        let mut discard_all = false;
        let mut later = false;
        let now = std::time::SystemTime::now();
        let modal = egui::Modal::new(egui::Id::unique("glitcherz_recovery")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading("Recover unsaved work");
            ui.label(
                "GlitcherZ did not shut down cleanly. These snapshots still hold edits that \
                 were never saved:",
            );
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .max_height(220.0)
                .show(ui, |ui| {
                    for (index, candidate) in self.recovery.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.button("Recover").clicked() {
                                recover = Some(index);
                            }
                            ui.label(candidate.display_name());
                            ui.weak(format!(
                                "{} · {}",
                                autosave::age_label(candidate.saved_at, now),
                                human_bytes(candidate.bytes)
                            ));
                        });
                    }
                });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Discard all").clicked() {
                    discard_all = true;
                }
                if ui.button("Later").clicked() {
                    later = true;
                }
            });
            ui.add_space(4.0);
            ui.weak("Recovering replaces the current document. Discarding deletes the files.");
        });
        if modal.should_close() {
            later = true;
        }
        if discard_all {
            for candidate in std::mem::take(&mut self.recovery) {
                autosave::discard(&candidate.autosave);
            }
            self.set_message("Discarded unsaved snapshots".to_string());
            return;
        }
        if let Some(index) = recover {
            let candidate = self.recovery.remove(index);
            // The other snapshots belong to other projects; leaving them alone
            // keeps them available on the next launch.
            self.recovery.clear();
            self.recover_snapshot(candidate);
            return;
        }
        if later {
            self.recovery.clear();
        }
    }

    /// "Import MIDI": pick a file and append its tracks to the project (U2).
    pub(crate) fn import_midi_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("MIDI File", &["mid", "midi", "smf"])
            .pick_file()
        else {
            return;
        };
        match project_files::read_midi(&path) {
            Ok(imported) => {
                self.import_midi_tracks(
                    imported.tracks,
                    imported.time_signature,
                    &imported.meter_markers,
                    &path,
                );
            }
            Err(err) => {
                self.set_message(format!("MIDI import failed: {err}"));
                log::error!("MIDI import failed: {err}");
            }
        }
    }

    /// Appends imported tracks: empty tracks on screen are filled first, the
    /// rest get a fresh MIDI track (nodes + cables). The whole import is one
    /// undo step — the per-track steps must not split the history.
    ///
    /// U3a: the file's meter is adopted only when the project has no content
    /// yet (that import is really "open"); otherwise the project keeps its own
    /// meter and the status line says so — re-measuring the grid under existing
    /// clips is not something an import may do silently.
    pub(crate) fn import_midi_tracks(
        &mut self,
        imported: Vec<MidiTrack>,
        file_meter: Option<glitcherz_model::TimeSignature>,
        file_meter_markers: &[glitcherz_model::MeterMarker],
        source: &std::path::Path,
    ) {
        let imported_len = imported.len();
        if imported_len == 0 {
            self.set_message(format!("No notes found in {}", display_name(source)));
            return;
        }
        // D15 revision: a dense track arrives as several clips (chunked by the
        // importer). Say so — a sudden row of clips must not look like a bug.
        let split_tracks = imported.iter().filter(|t| t.clips.len() > 1).count();
        let split_clips = imported
            .iter()
            .filter(|t| t.clips.len() > 1)
            .map(|t| t.clips.len())
            .sum::<usize>();
        let unwritten = project_files::is_unwritten(self.project());
        let plan = project_files::plan_import(&self.project().midi_tracks, imported_len);
        let before = self.begin_edit();
        self.session.suspend_history();
        let mut queue = imported.into_iter();
        let mut first_target = None;
        let mut placed = 0usize;
        for index in &plan.reuse {
            let Some(track) = queue.next() else { break };
            self.project_mut().midi_tracks[*index] = track;
            self.send_track_clips(*index);
            first_target.get_or_insert(*index);
            placed += 1;
        }
        for _ in 0..plan.append {
            let Some(track) = queue.next() else { break };
            let before_len = self.project().midi_tracks.len();
            self.add_midi_track();
            if self.project().midi_tracks.len() <= before_len {
                break; // engine slots or the command queue are full
            }
            self.project_mut().midi_tracks[before_len] = track;
            self.send_track_clips(before_len);
            first_target.get_or_insert(before_len);
            placed += 1;
        }
        self.session.resume_history();
        self.commit_edit("Import MIDI", before);
        if let Some(index) = first_target {
            self.selected_track = index;
        }
        let skipped = imported_len.saturating_sub(placed);
        let mut message = format!("Imported {placed} track(s) from {}", display_name(source));
        if split_tracks > 0 {
            message.push_str(&format!(
                " — {split_tracks} dense track(s) split into {split_clips} clips"
            ));
        }
        if skipped > 0 {
            message.push_str(&format!(
                " — {skipped} skipped (track limit {MAX_MIDI_TRACKS})"
            ));
        }
        // Meter (U3a / Met M5b): same undo step as the tracks, so one Ctrl+Z
        // undoes the import as a whole. A base meter and meter changes are
        // adopted together — the file's grid only makes sense as one table.
        let current = self.project().metadata.time_signature;
        if unwritten && (file_meter.is_some() || !file_meter_markers.is_empty()) {
            if let Some(meter) = file_meter {
                if meter != current {
                    message.push_str(&format!(" — meter set to {meter}"));
                }
                self.project_mut().metadata.time_signature = meter;
            }
            if !file_meter_markers.is_empty() {
                message.push_str(&format!(" — {} meter change(s)", file_meter_markers.len()));
                self.project_mut().metadata.meter_markers = file_meter_markers.to_vec();
            }
            self.push_time_signature();
        } else if let Some(meter) = file_meter {
            if meter != current {
                message.push_str(&format!(" — file is {meter}, project stays {current}"));
            }
        }
        self.set_message(message);
        log::info!("MIDI imported: {}", source.display());
    }

    /// "Export MIDI": write the arrangement as a format-1 SMF (U2). Not an
    /// edit — the file on disk changes, the document does not.
    pub(crate) fn export_midi_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("MIDI File", &["mid"])
            .set_file_name(project_files::midi_export_name(
                self.project_path.as_deref(),
            ))
            .save_file()
        else {
            return;
        };
        let mut path = path;
        path.set_extension("mid");
        match project_files::write_midi(&path, self.project()) {
            Ok(bytes) => {
                self.set_message(format!("Exported {} ({bytes} bytes)", display_name(&path)));
                log::info!("MIDI exported: {}", path.display());
            }
            Err(err) => {
                self.set_message(format!("MIDI export failed: {err}"));
                log::error!("MIDI export failed: {err}");
            }
        }
    }

    /// Captures live plugin state blobs into the document before saving
    /// (save-time only, not an undoable edit — same class as the patch
    /// layout, spec/plugin-state.md D3). Individual failures are logged and
    /// skipped: a broken plugin must not block the whole save (D7).
    pub(crate) fn capture_plugin_states(&mut self) {
        let blobs = self.plugin_state_blobs();
        apply_plugin_state(self.project_mut(), blobs);
    }

    /// Live plugin state blobs, one per installed plugin node (S3a: shared by
    /// the manual save and the auto-save snapshot).
    pub(crate) fn plugin_state_blobs(&mut self) -> Vec<(u64, Vec<u8>)> {
        let doc_by_engine: std::collections::HashMap<u64, u64> = self
            .node_map
            .iter()
            .map(|(doc_id, engine_id)| (*engine_id, *doc_id))
            .collect();
        // Two passes: the guard loop borrows `voice_guards` mutably, the doc
        // write needs `project_mut` — collect blobs first, apply afterwards.
        let mut blobs: Vec<(u64, Vec<u8>)> = Vec::new();
        for entry in &mut self.voice_guards {
            let Some(doc_id) = doc_by_engine.get(&entry.node_id).copied() else {
                continue;
            };
            let blob = match &mut entry._guard {
                PluginGuard::Clap(guard) => match guard.save_state() {
                    Ok(blob) => Some(blob),
                    Err(glitcherz_clap_host::HostError::NoState) => None,
                    Err(err) => {
                        log::warn!("State capture skipped for {}: {err}", entry.plugin_id);
                        None
                    }
                },
                PluginGuard::Vst3(guard) => match guard.save_state() {
                    Ok(blob) => Some(blob),
                    Err(err) => {
                        log::warn!("State capture skipped for {}: {err}", entry.plugin_id);
                        None
                    }
                },
            };
            if let Some(blob) = blob {
                blobs.push((doc_id, blob));
            }
        }
        blobs
    }

    /// Write to disk and update recent files / status message. Returns
    /// whether the write succeeded (U2: the title's dirty marker and the
    /// unsaved-changes modal depend on it).
    pub(crate) fn do_save(&mut self, path: PathBuf) -> bool {
        // S3a: a "Save As" retires the previous path's snapshot as well.
        let previous = self.project_path.clone();
        // Persist the patch canvas layout (view data) with the project.
        let layout = self.patch_state.export_layout(&self.project().graph);
        self.project_mut().graph.layout = layout;
        // R4 D11: media paths resolve against the project directory.
        self.project_mut().base_dir = path.parent().map(std::path::Path::to_path_buf);
        self.capture_plugin_states();
        match glitcherz_model::save_project(self.project(), &path) {
            Ok(()) => {
                self.session.mark_clean();
                self.project_path = Some(path.clone());
                self.push_recent(path.clone());
                // S3a: the document is on disk, so the crash snapshot has
                // nothing left to recover (it also covers the untitled one a
                // "Save As" just retired).
                self.clear_autosave();
                if let Some(previous) = previous.filter(|p| p != &path) {
                    autosave::discard(&glitcherz_model::autosave_path(&previous));
                }
                self.autosave_clock.reset(Instant::now());
                self.set_message(format!("Saved {}", path.display()));
                log::info!("Project saved: {}", path.display());
                true
            }
            Err(err) => {
                self.set_message(format!("Save failed: {err}"));
                log::error!("Failed to save project: {err}");
                false
            }
        }
    }

    /// Where the current document's crash snapshot belongs (S3a).
    pub(crate) fn autosave_target(&self) -> Option<PathBuf> {
        autosave::target(
            self.project_path.as_deref(),
            self.autosave_app_dir.as_deref(),
        )
    }

    /// Deletes the crash snapshot of the current document — plus the untitled
    /// one, which a "Save As" just made obsolete.
    pub(crate) fn clear_autosave(&mut self) {
        if let Some(path) = self.autosave_target() {
            autosave::discard(&path);
        }
        if let Some(path) = autosave::untitled_path(self.autosave_app_dir.as_deref()) {
            autosave::discard(&path);
        }
    }

    /// End-of-frame snapshot tick (S3a). The clock decides; the write happens
    /// here on the UI thread (disk IO, never inside the audio callback).
    pub(crate) fn tick_autosave(&mut self, now: Instant) {
        self.autosave_clock
            .observe(self.session.revision(), self.session.is_dirty(), now);
        if !self.autosave_clock.due(now) {
            return;
        }
        let Some(path) = self.autosave_target() else {
            return;
        };
        // Snapshot a clone with the live plugin state folded in: capturing into
        // the live document would bump the revision and dirty flag on every
        // tick, which turns recovery into a write loop.
        let mut snapshot = self.project().clone();
        let blobs = self.plugin_state_blobs();
        apply_plugin_state(&mut snapshot, blobs);
        match autosave::write(&snapshot, &path) {
            Ok(()) => {
                log::info!("Auto-saved to {}", path.display());
            }
            Err(err) => {
                // Report once per interval, not per frame: `mark_written`
                // below is what keeps a full disk from spamming the bar. The
                // rest of the session works, so this is a warning, not a stop.
                log::warn!("Auto-save failed: {err}");
                self.set_error(format!("Auto-save failed: {err}"));
            }
        }
        self.autosave_clock.mark_written(now);
    }

    /// Load from disk, replace the document, rebuild the engine graph per the document, and
    /// re-push all parameters.
    pub(crate) fn do_open(&mut self, path: PathBuf) {
        // U13 D14: opening a project installs every node (`apply_document`),
        // which parses SoundFonts and instantiates every external plugin —
        // seconds on a big session. Same banner mechanism as the font swap:
        // queue it, let the banner paint, then run it.
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.queue_blocking_job(format!("Opening {name}…"), move |app| {
            app.do_open_now(path)
        });
    }

    /// The blocking half of [`Self::do_open`].
    pub(crate) fn do_open_now(&mut self, path: PathBuf) {
        // A take belongs to the document being replaced (the session is about
        // to be swapped and the engine graph rebuilt).
        self.cancel_recording();
        match glitcherz_model::load_project(&path) {
            Ok(project) => {
                self.adopt_document(project, Some(path.clone()), false);
                self.set_message(format!("Opened {}", path.display()));
                log::info!("Project opened: {}", path.display());
            }
            Err(err) => {
                self.set_message(format!("Open failed: {err}"));
                log::error!("Failed to open project: {err}");
            }
        }
    }

    /// Replaces the live document with `project` and points the view, the
    /// engine and the snapshot clock at it (open / crash recovery).
    ///
    /// `path` is the project the document belongs to (`None` = untitled);
    /// `dirty` marks it as unsaved work — a recovered snapshot has never been
    /// written as this project, so the title must show `*` and closing must ask.
    /// `base_dir` is the caller's business: it depends on where the bytes came
    /// from, not on where they are going.
    pub(crate) fn adopt_document(&mut self, project: Project, path: Option<PathBuf>, dirty: bool) {
        self.session.replace(project);
        if dirty {
            self.session.mark_dirty();
        }
        // Seed the patch view from the persisted layout (session positions of
        // the previous document are dropped). Snapshot first: `project()`
        // borrows all of `self`, which would clash with `patch_state`.
        let layout = self.project().graph.layout.clone();
        self.patch_state.import_layout(&layout);
        self.project_path = path.clone();
        if let Some(path) = path {
            self.push_recent(path);
        }
        self.selected_track = 0;
        self.selected_auto = 0;
        self.edit_clip = None;
        // The selection indexes the *previous* document.
        self.edit_selection.clear();
        self.timeline_state.selection.clear();
        self.roll_state = piano_roll::RollState::new();
        self.autosave_clock.reset(Instant::now());
        self.push_all_params();
        // U1: the opened document's song loop reaches the engine.
        self.push_loop_range();
        // S3b: a project that travelled without its media says so right away
        // instead of playing silence with no explanation.
        self.refresh_missing_media();
    }

    /// Recomputes which referenced media files cannot be found (S3b).
    pub(crate) fn refresh_missing_media(&mut self) {
        self.missing_media = self.project().missing_media();
        if !self.missing_media.is_empty() {
            log::warn!("Missing media: {:?}", self.missing_media);
        }
    }

    /// Copies every referenced media file that lives outside
    /// `<stem>.media/` into it and rewrites the document paths — one undo step.
    /// The project must be on disk first: the folder is defined beside it.
    pub(crate) fn collect_media_files(&mut self) {
        let Some(project_path) = self.project_path.clone() else {
            self.set_error("Save the project first — the media folder lives beside it".to_string());
            return;
        };
        let Some(base) = project_path.parent().map(Path::to_path_buf) else {
            self.set_error("The project path has no folder".to_string());
            return;
        };
        let stem = project_path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "Untitled".to_string());
        let media_dir = base.join(format!("{stem}.media"));

        let mut map: HashMap<String, String> = HashMap::new();
        let mut copied = 0usize;
        let mut failed = 0usize;
        for stored in self.project().media_paths() {
            let source = self.project().resolve_media_path(&stored);
            if !source.is_file() {
                // Missing files are the relocation dialog's business; copying
                // cannot help.
                failed += 1;
                continue;
            }
            if source.starts_with(&media_dir) {
                continue; // already collected
            }
            let target = media::collect_target(&media_dir, &source);
            let target_len = std::fs::metadata(&target).map(|meta| meta.len()).ok();
            let source_len = std::fs::metadata(&source).map(|meta| meta.len()).ok();
            if target_len.is_none() || target_len != source_len {
                let result = std::fs::create_dir_all(&media_dir)
                    .and_then(|()| std::fs::copy(&source, &target).map(|_| ()));
                if let Err(err) = result {
                    log::error!("Collect media: copying {} failed: {err}", source.display());
                    failed += 1;
                    continue;
                }
                copied += 1;
            }
            let new_stored = self.project().media_path_string(&target);
            if new_stored != stored {
                map.insert(stored, new_stored);
            }
        }

        if map.is_empty() {
            self.set_message(
                "Nothing to collect — the media already lives in the project folder".to_string(),
            );
        } else {
            let moved = map.len();
            let before = self.begin_edit();
            self.project_mut().rewrite_media_paths(&map);
            self.commit_edit("Collect Media", before);
            self.set_message(format!(
                "Collected {copied} file(s) ({moved} path(s) rewritten) into {}",
                media_dir.display()
            ));
            log::info!("Collected media into {}", media_dir.display());
        }
        if failed > 0 {
            self.set_error(format!("{failed} media file(s) could not be copied"));
        }
        self.refresh_missing_media();
    }

    /// Points missing files at a folder the user picks: every missing path is
    /// matched by file name anywhere under that folder and rewritten — one undo
    /// step. The engine graph is rebuilt, because the nodes hold the old
    /// (unloadable) sources.
    pub(crate) fn relocate_missing_media(&mut self) {
        let Some(dir) = rfd::FileDialog::new()
            .set_title("Folder containing the missing media")
            .pick_folder()
        else {
            return;
        };
        let mut map: HashMap<String, String> = HashMap::new();
        for stored in self.missing_media.clone() {
            let Some(name) = Path::new(&stored)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            let Some(found) = media::find_by_name(&dir, &name, media::SEARCH_DEPTH) else {
                continue;
            };
            let new_stored = self.project().media_path_string(&found);
            if new_stored != stored {
                map.insert(stored, new_stored);
            }
        }
        if map.is_empty() {
            self.set_error("No file with a matching name in that folder".to_string());
            return;
        }
        let relocated = map.len();
        let before = self.begin_edit();
        self.project_mut().rewrite_media_paths(&map);
        self.commit_edit("Relocate Media", before);
        // Every installed sample / audio table still points at the old path.
        self.apply_document();
        self.refresh_missing_media();
        if self.missing_media.is_empty() {
            self.set_message(format!("Relocated {relocated} media file(s)"));
            log::info!("Relocated {relocated} media file(s) into the project");
        } else {
            self.set_error(format!(
                "Relocated {relocated} file(s); {} still missing",
                self.missing_media.len()
            ));
        }
    }

    /// S3b: prompt for media the project references but cannot find. Two ways
    /// out — point at a folder, or carry on without the files.
    pub(crate) fn missing_media_dialog(&mut self, ctx: &egui::Context) {
        if self.missing_media.is_empty() {
            return;
        }
        let mut continue_without = false;
        let mut locate = false;
        let modal = egui::Modal::new(egui::Id::unique("glitcherz_missing_media")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.heading(format!("Missing media ({})", self.missing_media.len()));
            ui.label("The project refers to these files, but they are not where it expects them:");
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .max_height(200.0)
                .show(ui, |ui| {
                    for path in &self.missing_media {
                        ui.weak(path);
                    }
                });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .button("Locate Folder…")
                    .on_hover_text("Pick the folder they moved to — files are matched by name")
                    .clicked()
                {
                    locate = true;
                }
                if ui.button("Continue without them").clicked() {
                    continue_without = true;
                }
            });
        });
        if modal.should_close() {
            continue_without = true;
        }
        if locate {
            // Keeps the dialog alive: `relocate` refreshes the list, so any
            // file it could not find stays visible.
            self.relocate_missing_media();
        }
        if continue_without {
            self.missing_media.clear();
        }
    }

    /// S3a: loads a crash snapshot through the same blocking banner a normal
    /// open uses (plugins and SoundFonts are just as slow to install).
    pub(crate) fn recover_snapshot(&mut self, candidate: autosave::Candidate) {
        let name = candidate.display_name();
        self.queue_blocking_job(format!("Recovering {name}…"), move |app| {
            app.do_recover_now(candidate)
        });
    }

    /// The blocking half of [`Self::recover_snapshot`].
    pub(crate) fn do_recover_now(&mut self, candidate: autosave::Candidate) {
        self.cancel_recording();
        match glitcherz_model::load_project(&candidate.autosave) {
            Ok(mut project) => {
                // `load_project` rooted relative media at the snapshot's own
                // folder; the document belongs to the project it came from.
                project.base_dir = candidate
                    .project
                    .as_deref()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf);
                let name = candidate.display_name();
                self.adopt_document(project, candidate.project, true);
                self.set_message(format!("Recovered unsaved work from {name}"));
                log::info!("Recovered snapshot {}", candidate.autosave.display());
            }
            Err(err) => {
                self.set_error(format!("Recovery failed: {err}"));
                log::error!(
                    "Failed to read snapshot {}: {err}",
                    candidate.autosave.display()
                );
            }
        }
    }
}
