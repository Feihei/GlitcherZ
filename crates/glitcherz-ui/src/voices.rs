//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    /// Closes the embedded plugin editor: the plugin's editor comes down
    /// first (`gui.destroy` / `IPlugView::removed` + release), then the
    /// native container window (spec `plugin-gui.md` §4.3 order — the
    /// plugin never sees its parent vanish). No-op when closed.
    pub(crate) fn close_plugin_editor(&mut self) {
        let Some(editor) = self.plugin_editor.take() else {
            return;
        };
        match self.guard_by_engine_id(editor.node_id) {
            Some(PluginGuard::Clap(g)) => g.close_editor(),
            Some(PluginGuard::Vst3(g)) => g.close_editor(),
            None => {}
        }
        drop(editor); // container destroyed after the plugin editor closed
    }

    /// Opens a plugin editor embedded under the main window for the given
    /// **document node** (spec `plugin-gui.md` §4/§6), CLAP or VST3 (G7).
    /// Needs the eframe frame (main HWND + scale) — therefore runs in the
    /// `ui()` tail, not in the panel.
    pub(crate) fn open_plugin_editor(&mut self, frame: &eframe::Frame, doc_node: u64) {
        if self.plugin_editor.is_some() {
            return;
        }
        let Some(&engine_id) = self.node_map.get(&doc_node) else {
            return;
        };
        let Some(hwnd) = plugin_window::main_window_handle(frame) else {
            self.set_message("Embedded editors need a native parent window".to_string());
            return;
        };
        let container = match plugin_window::PluginWindowContainer::new(hwnd) {
            Ok(c) => c,
            Err(err) => {
                log::warn!("Plugin editor container creation failed: {err}");
                self.set_message(format!("Editor window failed: {err}"));
                return;
            }
        };
        let ppp = frame
            .winit_window()
            .map(|w| w.scale_factor())
            .unwrap_or(1.0) as f32;
        // Plugin GUIs report sizes in pixels; the egui layout works in
        // points.
        let to_points = |width: i32, height: i32| (width as f32 / ppp, height as f32 / ppp);
        let opened: Result<(f32, f32), String> = match self.guard_by_engine_id(engine_id) {
            Some(PluginGuard::Clap(g)) => g
                .open_editor(container.raw_handle(), ppp as f64)
                .map(|s| to_points(s.width as i32, s.height as i32))
                .map_err(|e| e.to_string()),
            Some(PluginGuard::Vst3(g)) => g
                .open_editor(container.raw_handle(), ppp as f64)
                .map(|s| to_points(s.width, s.height))
                .map_err(|e| e.to_string()),
            None => Err("This node has no plugin editor (built-in voice)".to_string()),
        };
        match opened {
            Ok(size) => {
                log::info!(
                    "Embedded editor open: node {doc_node} engine {engine_id} size {size:?}pt"
                );
                self.plugin_editor = Some(PluginEditor {
                    doc_node,
                    node_id: engine_id,
                    size,
                    rect: None,
                    container,
                    pending_ack: None,
                });
            }
            Err(err) => {
                log::info!("Plugin editor open failed: {err}");
                self.set_message(format!("Plugin editor unavailable: {err}"));
                // Container dropped here — the plugin never attached to it.
            }
        }
    }

    /// The Inspector's editing target (spec `plugin-gui.md` §6): the
    /// patch selection's first editable document node, falling back to the
    /// selected track's first-match instrument (pre-G4 behavior).
    pub(crate) fn panel_target_node(&self) -> Option<u64> {
        let selected = self.patch_state.selected_doc_nodes();
        let track = self
            .selected_track
            .min(self.project().midi_tracks.len().saturating_sub(1));
        resolve_panel_target(
            &self.project().graph,
            &selected,
            self.patch_state.has_any_selection(),
            track,
        )
    }

    /// Build a SoundFont voice processor from the shared session cache
    /// (U13 D12): reuse the parsed library when the path is still resident,
    /// otherwise load it. Control thread only (D3).
    pub(crate) fn obtain_sf2_voice(
        &mut self,
        path: &str,
        sample_rate: f32,
    ) -> Result<Sf2VoiceProcessor, Sf2Error> {
        // S3b: same rule as samples — a relative library path is rooted at the
        // project directory.
        let resolved = self.project().resolve_media_path(path);
        let library = self.sf2_library_for(&resolved)?;
        Sf2VoiceProcessor::new(library.font(), sample_rate)
    }

    /// Resident library for `path`, refreshed as most-recently-used.
    pub(crate) fn sf2_cached(&mut self, path: &Path) -> Option<Arc<Sf2Library>> {
        let index = self.sf2_libraries.iter().position(|(p, _)| p == path)?;
        let entry = self.sf2_libraries.remove(index);
        let library = entry.1.clone();
        self.sf2_libraries.push(entry);
        Some(library)
    }

    /// Insert a freshly parsed library, evicting past the cap (LRU first).
    pub(crate) fn sf2_store(&mut self, path: PathBuf, library: Arc<Sf2Library>) -> Arc<Sf2Library> {
        // Names outlive the parsed font: they are cheap and losing them
        // would blind the preset pickers on a still-playing node.
        self.sf2_preset_names.insert(
            path.clone(),
            library
                .presets()
                .iter()
                .map(|p| (p.bank, p.program, p.name.clone()))
                .collect(),
        );
        self.sf2_libraries.retain(|(p, _)| p != &path);
        self.sf2_libraries.push((path, library.clone()));
        while self.sf2_libraries.len() > SF2_LIBRARY_CACHE_CAP {
            self.sf2_libraries.remove(0);
        }
        library
    }

    /// Library for `path`: cache hit, otherwise a full parse (control thread,
    /// D3). A miss on a multi-GB font blocks for seconds, so interactive
    /// callers go through [`Self::pick_sound_font`] and its banner instead.
    pub(crate) fn sf2_library_for(&mut self, path: &Path) -> Result<Arc<Sf2Library>, Sf2Error> {
        if let Some(library) = self.sf2_cached(path) {
            return Ok(library);
        }
        let library = Arc::new(Sf2Library::load(path)?);
        Ok(self.sf2_store(path.to_path_buf(), library))
    }

    /// Queue a UI-blocking job (U13). The banner paints on the next frame and
    /// the work runs at the end of the one after that, so the user sees what
    /// the window is stuck on. Jobs replace each other — the window between
    /// queueing and running is two frames.
    pub(crate) fn queue_blocking_job(
        &mut self,
        label: impl Into<String>,
        run: impl FnOnce(&mut GlitcherZApp) + 'static,
    ) {
        self.blocking_job = Some(BlockingJob {
            label: label.into(),
            phase: 0,
            run: Box::new(run),
        });
    }

    /// Thin banner naming the job the window is about to block on (same slot
    /// and shape as the plugin-scan banner).
    pub(crate) fn blocking_banner(&self, ui: &mut egui::Ui) {
        let Some(job) = self.blocking_job.as_ref() else {
            return;
        };
        egui::Panel::top("glitcherz_blocking").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.strong(&job.label);
            });
        });
    }

    /// Advance the [`BlockingJob`] state machine at the end of the frame.
    /// Returns `true` when the job ran (the caller repaints).
    pub(crate) fn run_blocking_job(&mut self) -> bool {
        let Some(job) = self.blocking_job.as_mut() else {
            return false;
        };
        if job.phase < 2 {
            job.phase += 1;
            return false;
        }
        let job = self.blocking_job.take().expect("checked above");
        (job.run)(self);
        true
    }

    /// `Load File…` on a SoundFont node (U13): pick a file, then swap the
    /// node's voice in place. The patch node's button and the Inspector call
    /// this one function, so both entry points write the same document and
    /// record the same single undo step.
    pub(crate) fn pick_sound_font(&mut self, doc_id: u64) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("SoundFont", &["sf2", "sf3"])
            .pick_file()
        else {
            return;
        };
        self.pick_sound_font_from(doc_id, path);
    }

    /// [`Self::pick_sound_font`] with the file already chosen (the dialog is
    /// the only part that needs the click).
    pub(crate) fn pick_sound_font_from(&mut self, doc_id: u64, path: PathBuf) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        // Peak RAM is the number users get bitten by, not the file size:
        // `SoundFontSampleData::read` holds the raw byte block and the i16
        // copy at the same time.
        let label = format!(
            "Loading {name} ({}, peak RAM ≈ {})…",
            human_bytes(size),
            human_bytes(size.saturating_mul(2))
        );
        self.queue_blocking_job(label, move |app| app.load_sound_font(doc_id, path));
    }

    /// `Load File…` / `Replace…` on a sample node (v3): pick an audio file,
    /// then swap the node's voice in place. Mirrors [`Self::pick_sound_font`] —
    /// the node's button and the Inspector run this one function.
    pub(crate) fn pick_sample_file(&mut self, doc_id: u64) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Audio", AUDIO_FILE_EXTENSIONS)
            .pick_file()
        else {
            return;
        };
        self.pick_sample_file_from(doc_id, path);
    }

    /// [`Self::pick_sample_file`] with the file already chosen (the dialog is
    /// the only part that needs the click).
    pub(crate) fn pick_sample_file_from(&mut self, doc_id: u64, path: PathBuf) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let label = format!("Loading {name}…");
        self.queue_blocking_job(label, move |app| app.load_sample_file(doc_id, path));
    }

    /// The blocking half of [`Self::pick_sample_file_from`]: decode through
    /// the session cache (shared with audio clips), then swap the engine
    /// processor **in place** — `replace_node` keeps the node id, so cables,
    /// ports, MIDI routing, root note, play mode and gain all survive. One
    /// undo step.
    pub(crate) fn load_sample_file(&mut self, doc_id: u64, path: PathBuf) {
        let Some((root_note, play_mode, gain)) = self.sample_voice_fields(doc_id) else {
            self.set_error("Load failed: node is not a sample instrument".to_string());
            return;
        };
        let Some(engine_id) = self.engine_id(doc_id) else {
            self.set_error("Load failed: node is not installed".to_string());
            return;
        };
        let sample_rate = self.engine.info().sample_rate as f32;
        // S3b: load from the absolute path the dialog returned, but store it
        // relative to the project folder so the project survives a move.
        let path_text = self.project().media_path_string(&path);
        let mut processor = match self.obtain_sample_voice(&path.display().to_string(), sample_rate)
        {
            Ok(processor) => processor,
            Err(err) => {
                // The document keeps its old path: a broken pick is not a
                // state change, just a failed one.
                self.set_error(format!("Sample failed to load: {err}"));
                return;
            }
        };
        processor.set_param(ParamId::SampleRootNote, f32::from(root_note));
        processor.set_param(
            ParamId::SamplePlayMode,
            if matches!(play_mode, SamplePlayMode::OneShot) {
                1.0
            } else {
                0.0
            },
        );
        processor.set_param(ParamId::SampleGain, gain);

        let before = self.begin_edit();
        if let Some(NodeDoc::Instrument {
            voice: VoiceSpec::Sample { path: doc_path, .. },
            ..
        }) = self.project_mut().graph.node_by_id_mut(doc_id)
        {
            *doc_path = path_text;
        }
        if self
            .engine
            .replace_node(engine_id, Box::new(processor))
            .is_none()
        {
            self.session.restore(before);
            self.set_error("Sample swap failed (engine busy) — try again".to_string());
            return;
        }
        self.commit_edit("Load Sample", before);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        self.set_message(format!(
            "Loaded {}",
            name.unwrap_or_else(|| path.display().to_string())
        ));
    }

    /// `(root_note, play_mode, gain)` of a sample instrument node.
    pub(crate) fn sample_voice_fields(&self, doc_id: u64) -> Option<(u8, SamplePlayMode, f32)> {
        match self.project().graph.node_by_id(doc_id)? {
            NodeDoc::Instrument {
                voice:
                    VoiceSpec::Sample {
                        root_note,
                        play_mode,
                        gain,
                        ..
                    },
                ..
            } => Some((*root_note, *play_mode, *gain)),
            _ => None,
        }
    }

    // ---- Drum kit (W1, spec `drum-kit.md`) ----

    /// Snapshot `(slots, gain)` of a drum-kit instrument node.
    pub(crate) fn drum_kit_fields(
        &self,
        doc_id: u64,
    ) -> Option<(Vec<glitcherz_model::DrumSlot>, f32)> {
        match self.project().graph.node_by_id(doc_id)? {
            NodeDoc::Instrument {
                voice: VoiceSpec::DrumKit { slots, gain },
                ..
            } => Some((slots.clone(), *gain)),
            _ => None,
        }
    }

    /// rfd multi-select behind the node's `Load Samples…` button.
    pub(crate) fn pick_drum_samples(&mut self, doc_id: u64) {
        if self.drum_kit_fields(doc_id).is_none() {
            self.set_error("Load failed: node is not a drum kit".to_string());
            return;
        }
        let Some(paths) = rfd::FileDialog::new()
            .add_filter("Audio", AUDIO_FILE_EXTENSIONS)
            .pick_files()
        else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        let label = format!("Loading {} drum samples…", paths.len());
        self.queue_blocking_job(label, move |app| app.load_drum_samples(doc_id, paths));
    }

    /// Blocking half of [`Self::pick_drum_samples`]: decode through the
    /// session cache, append slots with free MIDI keys, rebuild the processor
    /// in place. One undo step.
    pub(crate) fn load_drum_samples(&mut self, doc_id: u64, paths: Vec<PathBuf>) {
        let Some((mut slots, gain)) = self.drum_kit_fields(doc_id) else {
            self.set_error("Load failed: node is not a drum kit".to_string());
            return;
        };
        let free = glitcherz_model::MAX_DRUM_SLOTS.saturating_sub(slots.len());
        if free == 0 {
            self.set_error("Drum kit is full (32 slots)".to_string());
            return;
        }
        let taken: Vec<u8> = slots.iter().map(|slot| slot.note).collect();
        let keys = glitcherz_model::assign_drum_notes(&taken, paths.len().min(free));
        let sample_rate = self.engine.info().sample_rate as f32;
        let mut loaded = 0;
        for (path, key) in paths.iter().zip(keys) {
            // S3b: store relative to the project folder (see `media_path_string`).
            let path_text = self.project().media_path_string(path);
            match self.sample_library.load(path) {
                Ok(_) => {
                    slots.push(glitcherz_model::DrumSlot {
                        note: key,
                        path: path_text,
                        ..glitcherz_model::DrumSlot::default()
                    });
                    loaded += 1;
                }
                Err(err) => {
                    log::error!("Drum sample '{}' failed to load: {err}", path.display());
                }
            }
        }
        if loaded == 0 {
            self.set_error("No sample could be loaded".to_string());
            return;
        }
        self.rebuild_drum_kit(doc_id, slots, gain, sample_rate, "Load Drum Samples");
        self.set_message(format!("Loaded {loaded} drum samples"));
    }

    /// Writes `slots` back and swaps the engine processor in place (the node
    /// id, cables and MIDI routing survive `replace_node`). One undo step.
    pub(crate) fn rebuild_drum_kit(
        &mut self,
        doc_id: u64,
        slots: Vec<glitcherz_model::DrumSlot>,
        gain: f32,
        sample_rate: f32,
        label: &str,
    ) {
        let Some(engine_id) = self.engine_id(doc_id) else {
            self.set_error("Drum kit rebuild failed: node is not installed".to_string());
            return;
        };
        let before = self.begin_edit();
        if let Some(NodeDoc::Instrument {
            voice: VoiceSpec::DrumKit {
                slots: doc_slots, ..
            },
            ..
        }) = self.project_mut().graph.node_by_id_mut(doc_id)
        {
            *doc_slots = slots.clone();
        }
        let processor = self.obtain_drum_kit_voice(&slots, gain, sample_rate);
        if self
            .engine
            .replace_node(engine_id, Box::new(processor))
            .is_none()
        {
            self.session.restore(before);
            self.set_error("Drum kit rebuild failed (engine busy) — try again".to_string());
            return;
        }
        self.commit_edit(label, before);
    }

    /// Removes one slot by trigger key and rebuilds the kit (Inspector row ×).
    pub(crate) fn remove_drum_slot(&mut self, doc_id: u64, note: u8) {
        let Some((mut slots, gain)) = self.drum_kit_fields(doc_id) else {
            return;
        };
        let before_len = slots.len();
        slots.retain(|slot| slot.note != note);
        if slots.len() == before_len {
            return;
        }
        let sample_rate = self.engine.info().sample_rate as f32;
        self.rebuild_drum_kit(doc_id, slots, gain, sample_rate, "Remove Drum Slot");
    }

    /// Reassigns one slot's trigger key and rebuilds the kit in place (the
    /// Inspector key drag, W1b / spec K13).
    ///
    /// `reassign_drum_slot` snaps a taken key to the nearest free one instead
    /// of overwriting a neighbour (`validate` rejects duplicate notes). The
    /// rebuild preserves the node id, cables and MIDI routing — the RT table
    /// is indexed by key at construction, so a moved slot needs a fresh
    /// processor, and the clip already triggering the old key simply goes
    /// silent while the new key sounds.
    pub(crate) fn move_drum_slot(&mut self, doc_id: u64, from: u8, to: u8) {
        let Some((mut slots, gain)) = self.drum_kit_fields(doc_id) else {
            return;
        };
        // Snap-to-free, order-preserving: the model owns the rule so the
        // uniqueness invariant is testable without an engine.
        let Some(target) = glitcherz_model::reassign_drum_slot(&mut slots, from, to) else {
            return;
        };
        let sample_rate = self.engine.info().sample_rate as f32;
        self.rebuild_drum_kit(doc_id, slots, gain, sample_rate, "Move Drum Slot");
        self.set_message(format!(
            "Slot moved to {} ({target})",
            glitcherz_model::note_name(target)
        ));
    }

    /// The blocking half of [`Self::pick_sound_font_from`]: parse (cached),
    /// resolve a preset that exists in the new file, then swap the engine
    /// processor **in place** — `replace_node` keeps the node id, so cables,
    /// ports and MIDI routing are untouched. One undo step.
    pub(crate) fn load_sound_font(&mut self, doc_id: u64, path: PathBuf) {
        let started = Instant::now();
        let Some((old_bank, old_program, gain)) = self.sf2_voice_fields(doc_id) else {
            self.set_error("Load failed: node is not a SoundFont instrument".to_string());
            return;
        };
        let library = match self.sf2_library_for(&path) {
            Ok(library) => library,
            Err(err) => {
                self.set_error(format!("Sound font failed to load: {err}"));
                return;
            }
        };
        // The old `bank:program` usually does not exist in the new file
        // (U13 D13): same address → same bank → GM 0:0 → first preset.
        let (bank, program) = resolve_sf2_preset(library.presets(), old_bank, old_program);
        let Some(engine_id) = self.engine_id(doc_id) else {
            self.set_error("Load failed: node is not installed".to_string());
            return;
        };
        let sample_rate = self.engine.info().sample_rate as f32;
        let bend_range = match self.project().graph.node_by_id(doc_id) {
            Some(NodeDoc::Instrument {
                voice: VoiceSpec::SoundFont { bend_range, .. },
                ..
            }) => *bend_range,
            _ => 2.0,
        };
        let mut processor = match Sf2VoiceProcessor::new(library.font(), sample_rate) {
            Ok(processor) => processor,
            Err(err) => {
                self.set_error(format!("Sound font failed to instantiate: {err}"));
                return;
            }
        };
        processor.set_param(ParamId::Sf2BankSelect, f32::from(bank));
        processor.set_param(ParamId::Sf2ProgramChange, f32::from(program));
        processor.set_param(ParamId::Sf2Gain, gain);
        processor.set_param(ParamId::Sf2BendRange, bend_range);

        // S3b: store the library relative to the project folder when it lives
        // under it (the absolute path keeps working for shared libraries).
        let stored_path = self.project().media_path_string(&path);
        let before = self.begin_edit();
        if let Some(NodeDoc::Instrument {
            voice:
                VoiceSpec::SoundFont {
                    path: doc_path,
                    bank: doc_bank,
                    program: doc_program,
                    ..
                },
            ..
        }) = self.project_mut().graph.node_by_id_mut(doc_id)
        {
            *doc_path = stored_path.clone();
            *doc_bank = bank;
            *doc_program = program;
        }
        if self
            .engine
            .replace_node(engine_id, Box::new(processor))
            .is_none()
        {
            self.session.restore(before);
            self.set_error("Sound font swap failed (engine busy) — try again".to_string());
            return;
        }
        self.commit_edit("Load Sound Font", before);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        log::info!(
            "swapped node {doc_id} to sound font {name} at {bank}:{program} in {:.2}s",
            started.elapsed().as_secs_f32()
        );
        self.set_message(format!(
            "Loaded {name} at {bank}:{program} ({:.1} s)",
            started.elapsed().as_secs_f32()
        ));
    }

    /// `(bank, program, gain)` of a SoundFont instrument node.
    pub(crate) fn sf2_voice_fields(&self, doc_id: u64) -> Option<(u8, u8, f32)> {
        match self.project().graph.node_by_id(doc_id)? {
            NodeDoc::Instrument {
                voice:
                    VoiceSpec::SoundFont {
                        bank,
                        program,
                        gain,
                        ..
                    },
                ..
            } => Some((*bank, *program, *gain)),
            _ => None,
        }
    }

    /// Build a sample voice processor from the session sample cache (U5,
    /// spec `sample-player.md` D7): decodes once per path, then reuses the
    /// shared `Arc<SampleData>`. Control thread only (D6).
    pub(crate) fn obtain_sample_voice(
        &mut self,
        path: &str,
        sample_rate: f32,
    ) -> Result<SampleVoiceProcessor, SampleError> {
        // S3b: the document may store a project-relative path (that is what
        // makes a project folder portable); resolve it before touching disk.
        let resolved = self.project().resolve_media_path(path);
        let data = self.sample_library.load(&resolved)?;
        Ok(SampleVoiceProcessor::new(data, sample_rate))
    }

    /// Builds a drum kit processor from the document's slots (W1, spec
    /// `drum-kit.md` K9): every slot decodes through the shared sample
    /// cache; a broken file only silences that slot. An empty (or fully
    /// failing) kit renders silence — never a synth patch.
    /// **Control thread only.**
    pub(crate) fn obtain_drum_kit_voice(
        &mut self,
        slots: &[glitcherz_model::DrumSlot],
        gain: f32,
        sample_rate: f32,
    ) -> DrumKitProcessor {
        let mut specs = Vec::with_capacity(slots.len());
        for slot in slots.iter().take(glitcherz_model::MAX_DRUM_SLOTS) {
            if slot.path.is_empty() {
                continue;
            }
            // S3b: drum slots store project-relative paths too.
            let resolved = self.project().resolve_media_path(&slot.path);
            match self.sample_library.load(&resolved) {
                Ok(data) => specs.push(DrumSlotSpec {
                    note: slot.note,
                    data,
                    gain: slot.gain,
                    pan: slot.pan,
                    tune_semitones: slot.tune_semitones,
                    choke: slot.choke,
                }),
                Err(err) => log::error!(
                    "Drum slot {} failed to load '{}': {err}",
                    slot.note,
                    slot.path
                ),
            }
        }
        let mut processor = DrumKitProcessor::new(&specs, sample_rate);
        processor.set_param(ParamId::DrumGain, gain);
        processor
    }

    /// Builds the RT clip table of one audio track (U6 D4) from the
    /// document: each clip's wav decodes through the session sample cache
    /// (shared with sample-player instruments); a missing file logs and
    /// leaves the slot empty (silent clip, document kept for a retry).
    /// **Control thread only** (decodes on cache miss).
    pub(crate) fn build_audio_clip_table(
        &mut self,
        track: usize,
    ) -> glitcherz_engine::realtime::AudioClipTable {
        // S0: one clip-table builder for the live graph and the headless
        // render. Snapshot only this track's clips (not the whole document)
        // before the media adapter reborrows the app mutably.
        let (clips, base_dir) = {
            let project = self.project();
            (
                project
                    .audio_tracks
                    .get(track)
                    .map(|t| t.clips.clone())
                    .unwrap_or_default(),
                project.base_dir.clone(),
            )
        };
        let mut media = headless::AppMedia(&mut *self);
        headless::audio_clip_table(&clips, base_dir.as_deref(), &mut media)
    }

    /// Pushes one audio track's clip table to its engine node (U6 D4).
    /// No-op when the track has no node installed yet (dangling document
    /// node — the next `apply_document` rebuild installs it).
    pub(crate) fn send_audio_clips_for_track(&mut self, track: usize) {
        let Some(doc_id) = self
            .project()
            .graph
            .nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::AudioTrack { track: t, .. } if *t == track))
            .map(NodeDoc::node_id)
        else {
            return;
        };
        let Some(engine_id) = self.engine_id(doc_id) else {
            return;
        };
        let table = self.build_audio_clip_table(track);
        if !self.engine.send_audio_clips(engine_id, table) {
            log::warn!("Audio clip table push for track {track} failed (structure queue full)");
        }
    }

    /// Switch a sample instrument's play mode (U5, spec D1): document field
    /// as one undoable edit + node-param delivery.
    pub(crate) fn set_sample_play_mode(&mut self, doc_id: u64, one_shot: bool) {
        let before = self.begin_edit();
        if let Some(NodeDoc::Instrument {
            voice: VoiceSpec::Sample { play_mode, .. },
            ..
        }) = self.project_mut().graph.node_by_id_mut(doc_id)
        {
            *play_mode = if one_shot {
                SamplePlayMode::OneShot
            } else {
                SamplePlayMode::Gate
            };
        }
        if let Some(engine_id) = self.engine_id(doc_id) {
            self.engine.set_node_param(
                engine_id,
                ParamId::SamplePlayMode,
                if one_shot { 1.0 } else { 0.0 },
            );
            self.commit_edit("Sample Play Mode", before);
            return;
        }
        self.commit_edit("Sample Play Mode", before);
        self.set_message("Play mode switch failed: voice not installed".to_string());
    }

    /// Switch a SoundFont instrument node's preset: document bank/program as
    /// one undoable edit + live node-param delivery (block-granularity, D7).
    pub(crate) fn set_soundfont_preset(&mut self, doc_id: u64, bank: u8, program: u8) {
        let before = self.begin_edit();
        if let Some(NodeDoc::Instrument {
            voice:
                VoiceSpec::SoundFont {
                    bank: b,
                    program: p,
                    ..
                },
            ..
        }) = self.project_mut().graph.node_by_id_mut(doc_id)
        {
            *b = bank;
            *p = program;
        }
        if let Some(engine_id) = self.engine_id(doc_id) {
            self.engine
                .set_node_param(engine_id, ParamId::Sf2BankSelect, f32::from(bank));
            self.engine
                .set_node_param(engine_id, ParamId::Sf2ProgramChange, f32::from(program));
            self.commit_edit("Select SF2 Preset", before);
            return;
        }
        self.commit_edit("Select SF2 Preset", before);
        self.set_message("Preset switch failed: voice not installed".to_string());
    }

    // ---- Instrument presets (host-side library over the state blob) ----

    /// An instrument node's preset-library key (`None` for the built-in
    /// synth, which has no presets yet).
    pub(crate) fn voice_plugin_key(&self, doc_id: u64) -> Option<String> {
        match self.project().graph.node_by_id(doc_id)? {
            NodeDoc::Instrument { voice, .. } => match voice {
                VoiceSpec::Clap {
                    path, plugin_id, ..
                } => Some(preset_plugin_key("clap", path, plugin_id)),
                VoiceSpec::Vst3 { path, class_id, .. } => {
                    Some(preset_plugin_key("vst3", path, class_id))
                }
                VoiceSpec::Builtin
                | VoiceSpec::SoundFont { .. }
                | VoiceSpec::Sample { .. }
                | VoiceSpec::DrumKit { .. } => None,
            },
            _ => None,
        }
    }

    /// The live guard owning a document node (its own engine id).
    pub(crate) fn node_guard(&mut self, doc_id: u64) -> Option<&mut PluginGuard> {
        let engine_id = *self.node_map.get(&doc_id)?;
        self.voice_guards
            .iter_mut()
            .find(|e| e.node_id == engine_id)
            .map(|e| &mut e._guard)
    }

    /// Whether a document node's live guard is an external plugin that can
    /// embed an editor (CLAP or VST3; read-only — usable where
    /// `node_guard`'s `&mut` borrow would conflict). Whether the specific
    /// plugin actually exposes a view is only known once `open_editor`
    /// runs; this is the "show the button / reopen on target change"
    /// predicate.
    pub(crate) fn node_guard_has_editor(&self, doc_id: u64) -> bool {
        let Some(&engine_id) = self.node_map.get(&doc_id) else {
            return false;
        };
        self.voice_guards.iter().any(|e| {
            e.node_id == engine_id
                && matches!(e._guard, PluginGuard::Clap(_) | PluginGuard::Vst3(_))
        })
    }

    /// Re-seeds a node's slider cache from the plugin's live values
    /// (after a state/preset load).
    pub(crate) fn refresh_voice_slider_cache(&mut self, doc_id: u64) {
        match self.node_guard(doc_id) {
            Some(PluginGuard::Clap(g)) => {
                let values = g.read_param_values();
                self.clap_voice_values.insert(doc_id, values);
            }
            Some(PluginGuard::Vst3(g)) => {
                let values = g.read_param_values();
                self.vst3_voice_values.insert(doc_id, values);
            }
            _ => {}
        }
    }

    /// Loads a preset into an instrument node: live `load_state` first (a
    /// refusing plugin must not touch the document), slider cache re-seed,
    /// then the blob into `VoiceSpec.state` as one undoable edit.
    pub(crate) fn load_voice_preset(&mut self, doc_id: u64, name: &str) {
        let Some(key) = self.voice_plugin_key(doc_id) else {
            return;
        };
        let Some(blob) = read_voice_preset_in(&preset_library_root(), &key, name) else {
            self.set_message(format!("Preset unreadable: {name}"));
            return;
        };
        let loaded = match self.node_guard(doc_id) {
            Some(PluginGuard::Clap(g)) => g.load_state(&blob).is_ok(),
            Some(PluginGuard::Vst3(g)) => g.load_state(&blob).is_ok(),
            _ => false,
        };
        if !loaded {
            self.set_message("Preset load failed: plugin rejected the state".to_string());
            return;
        }
        self.refresh_voice_slider_cache(doc_id);
        let before = self.begin_edit();
        if let Some(NodeDoc::Instrument { voice, .. }) =
            self.project_mut().graph.node_by_id_mut(doc_id)
        {
            match voice {
                VoiceSpec::Clap { state, .. } | VoiceSpec::Vst3 { state, .. } => *state = blob,
                VoiceSpec::Builtin
                | VoiceSpec::SoundFont { .. }
                | VoiceSpec::Sample { .. }
                | VoiceSpec::DrumKit { .. } => {}
            }
        }
        self.commit_edit("Load Preset", before);
        self.voice_preset_selected.insert(doc_id, name.to_string());
        self.set_message(format!("Loaded preset {name}"));
    }

    /// Saves an instrument node's current state blob as a named preset file
    /// (file operation — not an undoable edit, same class as project save).
    pub(crate) fn save_voice_preset_as(&mut self, doc_id: u64) {
        let Some(key) = self.voice_plugin_key(doc_id) else {
            return;
        };
        let Some(name) = sanitize_preset_name(&self.preset_name_input) else {
            self.set_message("Preset name is empty".to_string());
            return;
        };
        let blob = match self.node_guard(doc_id) {
            Some(PluginGuard::Clap(g)) => g.save_state().ok(),
            Some(PluginGuard::Vst3(g)) => g.save_state().ok(),
            _ => None,
        };
        let Some(blob) = blob else {
            self.set_message("Save preset failed: plugin state unavailable".to_string());
            return;
        };
        match save_voice_preset_in(&preset_library_root(), &key, &name, &blob) {
            Ok(_) => {
                self.voice_preset_selected.insert(doc_id, name.clone());
                self.set_message(format!("Preset saved: {name}"));
                log::info!("Preset saved: {key}/{name}");
            }
            Err(err) => {
                self.set_message(format!("Preset save failed: {err}"));
                log::error!("Preset save failed: {err}");
            }
        }
    }

    /// Resets an instrument node to the factory-default state captured at
    /// spawn time: live reload + slider cache re-seed + an undoable edit
    /// that clears the persisted blob (empty state = factory default on
    /// reload).
    pub(crate) fn reset_voice_preset(&mut self, doc_id: u64) {
        let Some(default_blob) = self.voice_default_state.get(&doc_id).cloned() else {
            self.set_message("Factory default unavailable this session".to_string());
            return;
        };
        let loaded = match self.node_guard(doc_id) {
            Some(PluginGuard::Clap(g)) => g.load_state(&default_blob).is_ok(),
            Some(PluginGuard::Vst3(g)) => g.load_state(&default_blob).is_ok(),
            _ => false,
        };
        if !loaded {
            self.set_message("Reset failed: plugin rejected the state".to_string());
            return;
        }
        self.refresh_voice_slider_cache(doc_id);
        let before = self.begin_edit();
        if let Some(NodeDoc::Instrument { voice, .. }) =
            self.project_mut().graph.node_by_id_mut(doc_id)
        {
            match voice {
                VoiceSpec::Clap { state, .. } | VoiceSpec::Vst3 { state, .. } => {
                    *state = Vec::new()
                }
                VoiceSpec::Builtin
                | VoiceSpec::SoundFont { .. }
                | VoiceSpec::Sample { .. }
                | VoiceSpec::DrumKit { .. } => {}
            }
        }
        self.commit_edit("Reset Preset", before);
        self.voice_preset_selected.remove(&doc_id);
        self.set_message("Reset to factory default".to_string());
    }

    /// Clear all external effects of a track (built-in delay kept): engine bypass removal +
    /// document sync.
    pub(crate) fn clear_external_effects(&mut self, track: usize) {
        let before = self.begin_edit();
        while let Some(effect) = self.project().graph.nodes.iter().find(|n| {
            n.track() == track
                && match n {
                    NodeDoc::Effect { voice, .. } => !matches!(voice, VoiceSpec::Builtin),
                    _ => false,
                }
        }) {
            let effect_doc = effect.node_id();
            let Some(&effect_engine) = self.node_map.get(&effect_doc) else {
                break;
            };
            // Document cables: in-cable source and out-cable target (bypass direct pair).
            let in_from = self.project().graph.cables.iter().find_map(|c| {
                if c.kind == CableKind::Audio && c.to == PortRef::Node(effect_doc) {
                    match c.from {
                        PortRef::Node(id) => Some(id),
                        _ => None,
                    }
                } else {
                    None
                }
            });
            let out_to = self.project().graph.cables.iter().find_map(|c| {
                if c.kind == CableKind::Audio && c.from == PortRef::Node(effect_doc) {
                    Some(c.to.clone())
                } else {
                    None
                }
            });
            let (Some(in_from), Some(out_to)) = (in_from, out_to) else {
                log::warn!(
                    "Track {track} effect {effect_doc} has incomplete cables; stopping cleanup"
                );
                break;
            };
            if !self.engine.remove_node(effect_engine) {
                break;
            }
            self.project_mut()
                .graph
                .nodes
                .retain(|n| n.node_id() != effect_doc);
            self.project_mut().graph.cables.retain(|c| {
                !(c.kind == CableKind::Audio
                    && (c.from == PortRef::Node(effect_doc) || c.to == PortRef::Node(effect_doc)))
            });
            self.project_mut()
                .graph
                .cables
                .push(glitcherz_model::CableDoc::audio(in_from, out_to));
            self.node_map.remove(&effect_doc);
            self.effect_meters.remove(&effect_doc);
            self.retire_guard(effect_engine);
            self.clap_effect_params.remove(&effect_doc);
            self.clap_effect_values.remove(&effect_doc);
            self.vst3_effect_params.remove(&effect_doc);
            self.vst3_effect_values.remove(&effect_doc);
        }
        self.commit_edit("Clear External Effects", before);
    }

    /// The track's external effect nodes as `(doc_id, display label, is_clap)`
    /// — the shared data source for the effect param panels and the
    /// automation target selector.
    pub(crate) fn track_external_fx(&self, track: usize) -> Vec<(u64, String, bool)> {
        self.project()
            .graph
            .nodes
            .iter()
            .filter(|n| n.track() == track)
            .filter_map(|n| match n {
                NodeDoc::Effect { node_id, voice, .. } => match voice {
                    VoiceSpec::Clap { plugin_id, .. } => Some((*node_id, plugin_id.clone(), true)),
                    VoiceSpec::Vst3 { path, .. } => Some((
                        *node_id,
                        std::path::Path::new(path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "VST3".to_string()),
                        false,
                    )),
                    VoiceSpec::Builtin
                    | VoiceSpec::SoundFont { .. }
                    | VoiceSpec::Sample { .. }
                    | VoiceSpec::DrumKit { .. } => None,
                },
                _ => None,
            })
            .collect()
    }
}
