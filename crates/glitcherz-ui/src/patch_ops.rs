//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    // ---- Node patch (P5d, spec §9 P5d) ----

    /// One frame of the node patch: view + interaction-intent collection; actions are applied to
    /// document + engine in one place.
    pub(crate) fn graph_patch(&mut self, ui: &mut egui::Ui) {
        let track_count = self.project().midi_tracks.len();
        let auto_count = self.project().control_tracks.len();
        let audio_count = self.project().audio_tracks.len();
        let actions = {
            // Borrow splitting: an immutable snapshot (document / plugin param tables) goes to
            // the patch while the mutable patch state is borrowed separately; actions are
            // applied in bulk after collection. Destructured rather than using
            // `self.project()`, because a method call borrows all of `self` and would
            // collide with the `&mut self.patch_state` below.
            let Self {
                session,
                patch_state,
                clap_voice_params: clap,
                voice_params: vst3,
                clap_effect_params: clap_fx,
                vst3_effect_params: vst3_fx,
                clap_voice_values: clap_values,
                vst3_voice_values: vst3_values,
                sf2_preset_names,
                ..
            } = self;
            // Immutable reborrow aliases: two closures (auto_options /
            // plugin_inline) capture the same tables simultaneously.
            let doc = &session.project().graph;
            // S3b: media paths are resolved against the project directory; the
            // readers below only get the graph, so root them here.
            let sf2_media_base = session.project().base_dir.clone();
            // Live track names for the patch's timeline / mixer port captions.
            // The patch only borrows the graph document, so the names ride a
            // reader; a rename in the arrangement is visible on the next frame.
            let midi_names: Vec<String> = session
                .project()
                .midi_tracks
                .iter()
                .map(|t| t.name.clone())
                .collect();
            let auto_names: Vec<String> = session
                .project()
                .control_tracks
                .iter()
                .map(|t| t.name.clone())
                .collect();
            let audio_names: Vec<String> = session
                .project()
                .audio_tracks
                .iter()
                .map(|t| t.name.clone())
                .collect();
            let track_name = move |kind: TrackKind, index: usize| -> String {
                match kind {
                    TrackKind::Midi => &midi_names,
                    TrackKind::Curve => &auto_names,
                    TrackKind::Audio => &audio_names,
                }
                .get(index)
                .cloned()
                .unwrap_or_default()
            };
            let clap = &*clap;
            let vst3 = &*vst3;
            let clap_fx = &*clap_fx;
            let vst3_fx = &*vst3_fx;
            let clap_values = &*clap_values;
            let vst3_values = &*vst3_values;
            let options = move |node_id: u64| -> Vec<(String, AutoParam)> {
                let (clap_params, vst3_params) =
                    plugin_params_of(doc, node_id, clap, clap_fx, vst3, vst3_fx);
                let mut v = node_auto_options(doc, node_id, clap_params, vst3_params);
                if matches!(doc.node_by_id(node_id), Some(NodeDoc::Instrument { .. })) {
                    // U4a: CC is a separate destination family for the
                    // instrument; list the presets alongside its parameters.
                    for (number, name) in CC_PRESETS {
                        v.push((format!("CC {number} {name}"), AutoParam::cc(number, 0)));
                    }
                    // P1: pitch bend is the other event-stream destination. Built-in /
                    // sample voices take the pitch offset directly; SF2 uses
                    // its explicit bend range; plugins keep ±2 st.
                    if let Some(NodeDoc::Instrument { voice, .. }) = doc.node_by_id(node_id) {
                        match voice {
                            VoiceSpec::Builtin | VoiceSpec::Sample { .. } => v.push((
                                "Pitch ±24 st".to_string(),
                                AutoParam::Node(NodeParam::PitchOffset),
                            )),
                            VoiceSpec::SoundFont { bend_range, .. } => v.push((
                                format!("Pitch Bend ±{bend_range:.0} st"),
                                AutoParam::pitch_bend(0, *bend_range),
                            )),
                            _ => v.push(("Pitch Bend".to_string(), AutoParam::pitch_bend(0, 2.0))),
                        }
                    }
                }
                v
            };
            // Plugin-node inline controls (G5 spec §7): the first two
            // enumerated parameters of the node's plugin voice. CLAP rows
            // carry the raw plugin range, VST3 rows the normalized 0..1
            // domain — the same split as the panel sliders, so inline edits
            // stay interchangeable with panel edits.
            let plugin_inline = move |node_id: u64| -> Vec<patch::InlinePluginControl> {
                let voice = match doc.node_by_id(node_id) {
                    Some(NodeDoc::Instrument { voice, .. } | NodeDoc::Effect { voice, .. }) => {
                        Some(voice)
                    }
                    _ => None,
                };
                let mut v = Vec::new();
                match voice {
                    Some(VoiceSpec::Clap { .. }) => {
                        if let Some(params) = clap.get(&node_id) {
                            for p in params.iter().take(2) {
                                v.push(patch::InlinePluginControl {
                                    param_id: p.id,
                                    label: inline_plugin_label(&p.name, p.id),
                                    value: clap_values
                                        .get(&node_id)
                                        .and_then(|m| m.get(&p.id))
                                        .copied()
                                        .unwrap_or(p.default_value),
                                    min: p.min_value,
                                    max: p.max_value,
                                });
                            }
                        }
                    }
                    Some(VoiceSpec::Vst3 { .. }) => {
                        if let Some(params) = vst3.get(&node_id) {
                            for p in params.iter().take(2) {
                                v.push(patch::InlinePluginControl {
                                    param_id: p.id,
                                    label: inline_plugin_label(&p.title, p.id),
                                    value: vst3_values
                                        .get(&node_id)
                                        .and_then(|m| m.get(&p.id))
                                        .copied()
                                        .unwrap_or(p.default_normalized),
                                    min: 0.0,
                                    max: 1.0,
                                });
                            }
                        }
                    }
                    _ => {}
                }
                v
            };
            // SoundFont preset table for the node's bank / patch combos
            // (U13): read from the resident library of the node's own path,
            // so the patch and the Inspector list exactly the same entries.
            // Empty = not resident (or no file yet) → the combos stay
            // disabled instead of showing a lie.
            //
            // S3b: the table is keyed by the **resolved** path (that is what
            // the loader caches), so a project-relative library is rooted here.
            let sf2_presets = move |node_id: u64| -> Vec<(u8, u8, String)> {
                let Some(NodeDoc::Instrument {
                    voice: VoiceSpec::SoundFont { path, .. },
                    ..
                }) = doc.node_by_id(node_id)
                else {
                    return Vec::new();
                };
                if path.is_empty() {
                    return Vec::new();
                }
                let stored = path.replace('\\', "/");
                let resolved = match (&sf2_media_base, std::path::Path::new(&stored).is_absolute())
                {
                    (Some(dir), false) => dir.join(&stored),
                    _ => std::path::PathBuf::from(&stored),
                };
                sf2_preset_names.get(&resolved).cloned().unwrap_or_default()
            };
            // R4: live input snapshot for the Audio In node's device chip and
            // per-channel meter. The cloned handles/strings outlive the
            // `&dyn Fn` readers stored in `PatchDeps`.
            let input_info = self.engine.input_info().cloned();
            let input_error = self.engine.input_error().map(str::to_string);
            let input_stats = self.engine.input_stats();
            let input_peak = move |channel: u8| {
                input_stats
                    .as_ref()
                    .map_or(0.0, |stats| stats.peak_of(usize::from(channel)))
            };
            // Read the app's cached device list: the patch captures it once per
            // menu open, so this closure must never enumerate devices itself.
            let input_source_cache = self.audio_sources.clone();
            let input_sources = move || {
                input_source_cache
                    .iter()
                    .map(|source| patch::InputSourceItem {
                        name: source.name().map(str::to_string),
                        loopback: source.is_loopback(),
                    })
                    .collect::<Vec<_>>()
            };
            patch::show(
                ui,
                patch_state,
                patch::PatchDeps {
                    doc,
                    track_count,
                    auto_count,
                    audio_count,
                    track_name: &track_name,
                    auto_options: &options,
                    plugin_inline: &plugin_inline,
                    sf2_presets: &sf2_presets,
                    input: patch::InputDeps {
                        open: input_info.is_some(),
                        device: input_info.as_ref().map(|info| info.device_name.as_str()),
                        loopback: input_info.as_ref().is_some_and(|info| info.loopback),
                        channels: input_info.as_ref().map_or(0, |info| info.channels),
                        error: input_error.as_deref(),
                        peak: Some(&input_peak),
                        sources: Some(&input_sources),
                    },
                },
            )
        };
        self.apply_patch_actions(actions);
    }

    /// Apply patch actions: sync document and engine (reusing existing graph-build/connect
    /// methods).
    pub(crate) fn apply_patch_actions(&mut self, actions: Vec<patch::PatchAction>) {
        for action in actions {
            match action {
                patch::PatchAction::AddNode { source, at } => {
                    self.add_patch_node(source, at);
                }
                patch::PatchAction::OpenPluginPicker { category, place } => {
                    self.open_plugin_picker(category, place);
                }
                patch::PatchAction::LoadSoundFont { node } => {
                    // U13: the node's own `Load File…` button. Same function
                    // as the Inspector button — one dialog, one undo step.
                    self.pick_sound_font(node);
                }
                patch::PatchAction::LoadSample { node } => {
                    // v3: the sample node's own `Load File…` / `Replace…`.
                    // Mirrors the SoundFont swap — the node keeps its id,
                    // cables and parameters.
                    self.pick_sample_file(node);
                }
                patch::PatchAction::LoadDrumSamples { node } => {
                    // W1: append wav files as kit slots with free MIDI keys.
                    self.pick_drum_samples(node);
                }
                patch::PatchAction::RescanPlugins => {
                    self.plugin_catalog.rescan();
                    self.set_message("Rescanning plug-ins…".to_string());
                }
                patch::PatchAction::InsertNode { cable, source, at } => {
                    self.insert_node_in_cable(cable, source, at);
                }
                patch::PatchAction::ConnectAudio {
                    from_doc,
                    to,
                    in_port,
                } => {
                    self.connect_audio(from_doc, to, in_port);
                }
                patch::PatchAction::ConnectMidi { track, to_doc } => {
                    self.connect_midi_source(track, to_doc);
                }
                patch::PatchAction::ConnectKeyboardMidi { from_doc, to_doc } => {
                    self.connect_keyboard_midi(from_doc, to_doc);
                }
                patch::PatchAction::ConnectEventMidi { from_doc, to_doc } => {
                    self.connect_event_cable(
                        from_doc,
                        to_doc,
                        CableKind::Midi,
                        AutoParam::default(),
                    );
                }
                patch::PatchAction::ConnectControl {
                    from_doc,
                    to_doc,
                    param,
                } => {
                    self.connect_event_cable(from_doc, to_doc, CableKind::Automation, param);
                }
                patch::PatchAction::ConnectAuto {
                    index,
                    to_doc,
                    param,
                } => {
                    self.set_auto_endpoint(index, AutoTarget::Node(to_doc), param);
                }
                patch::PatchAction::ConnectTempoIn { index } => {
                    self.set_auto_endpoint(index, AutoTarget::TimelineIn, AutoParam::Tempo);
                }
                patch::PatchAction::ConnectTrackIn { from, kind, track } => {
                    self.connect_track_input(from, kind, track);
                }
                patch::PatchAction::DeleteCable(i) => {
                    self.delete_cable(i);
                }
                patch::PatchAction::DeleteNode(doc_id) => {
                    self.delete_graph_node(doc_id);
                }
                patch::PatchAction::SelectTrack(track) => {
                    if track < self.project().midi_tracks.len() {
                        self.selected_track = track;
                    }
                }
                patch::PatchAction::OpenNode(doc_id) => {
                    // The double-click's first click already selected the node
                    // (and linked the owning track); the jump itself just
                    // switches the view (spec `plugin-gui.md` §6).
                    if self.project().graph.node_by_id(doc_id).is_some() {
                        self.view = MainView::Inspector;
                    }
                }
                patch::PatchAction::OpenTimeline => {
                    self.view = MainView::Timeline;
                }
                patch::PatchAction::AddMidiTrack => self.add_midi_track(),
                patch::PatchAction::RemoveLastMidiTrack => self.remove_last_midi_track(),
                patch::PatchAction::AddControlTrack => self.add_control_track(),
                patch::PatchAction::RemoveControlTrack => self.remove_control_track(),
                patch::PatchAction::AddAudioTrack => self.add_audio_track(),
                patch::PatchAction::RemoveLastAudioTrack => self.remove_last_audio_track(),
                patch::PatchAction::AddMixerSlot(mixer_doc) => {
                    self.mixer_add_channel(mixer_doc);
                }
                patch::PatchAction::RemoveMixerSlot(mixer_doc) => {
                    self.mixer_remove_channel(mixer_doc);
                }
                patch::PatchAction::SetInlineParam { node, param, value } => {
                    self.apply_inline_param(node, param, value);
                }
                patch::PatchAction::PickInputSource(item) => {
                    let source = match item.name {
                        None => RecordSource::Default,
                        Some(name) if item.loopback => RecordSource::Loopback(name),
                        Some(name) => RecordSource::Capture(name),
                    };
                    self.pick_audio_source(source);
                }
                patch::PatchAction::RefreshInputSources => {
                    self.audio_sources = AudioEngine::record_sources();
                }
                // Inline rename on a timeline outlet / mixer inlet. Both go
                // through the same undoable document edits as the arrangement
                // header and the Inspector console.
                patch::PatchAction::RenameTrack { kind, track, name } => {
                    let lane = match kind {
                        TrackKind::Midi => timeline::TimelineLane::Midi,
                        TrackKind::Curve => timeline::TimelineLane::Control,
                        TrackKind::Audio => timeline::TimelineLane::Audio,
                    };
                    self.rename_track(lane, track, name);
                }
                patch::PatchAction::RenameChannel {
                    node,
                    channel,
                    name,
                } => {
                    self.rename_mixer_channel(node, channel, name);
                }
            }
        }
    }

    /// Open the filterable picker for a leaf of the `Add` menu / a cable
    /// midpoint (U9 E3, spec `plugin-browser.md` §6). `category` narrows the
    /// list to instruments or effects (built-in rows and plugins alike, v3
    /// D15/D16); `None` is the global `Search Nodes…` entry, whose pick
    /// decides the node category from the row's own classification (spec D5).
    pub(crate) fn open_plugin_picker(
        &mut self,
        category: Option<patch::NodeCategory>,
        place: patch::PickerPlace,
    ) {
        self.picker_place = Some(place);
        self.picker
            .open(plugin_picker::PickFilter::from_category(category));
    }

    /// Draw the plugin popup and create the picked node (U9 E3), or splice it
    /// into a cable (U9 E4 — same popup, different receiver).
    ///
    /// The popup is drawn above every panel (`Order::Foreground`); a row
    /// carries its **final** node source, so the creation path installs the
    /// plugin exactly once (spec D1) and a built-in row needs no mapping.
    pub(crate) fn plugin_picker_frame(&mut self, ctx: &egui::Context) {
        let scan_progress = self
            .plugin_catalog
            .is_scanning()
            .then(|| self.plugin_catalog.progress());
        let outcome = self.picker.show(
            ctx,
            self.plugin_catalog.entries(),
            &self.recent_plugins,
            scan_progress,
        );
        match outcome {
            Some(plugin_picker::PickOutcome::Picked(choice)) => {
                // Only plugins are remembered; built-ins are always in the list.
                plugin_picker::remember(&mut self.recent_plugins, &choice);
                let Some(place) = self.picker_place.take() else {
                    return;
                };
                let source = choice.source;
                match place {
                    patch::PickerPlace::Add { at } => {
                        self.add_patch_node(source, at);
                    }
                    patch::PickerPlace::Insert { cable, at } => {
                        self.insert_node_in_cable(cable, source, at);
                    }
                }
                // The status bar text changed; repaint immediately instead of
                // waiting for the 50 ms display tick.
                ctx.request_repaint();
            }
            Some(plugin_picker::PickOutcome::Cancelled) => {
                self.picker_place = None;
            }
            None => {}
        }
    }

    /// Add a new document node from the patch context menu: allocate the next document
    /// node_id, install the processor into the engine (born dangling — no cables; D6),
    /// place it on the patch at the click position, and refresh the owning track's
    /// addressing table. The new node's owning track is the currently selected track.
    pub(crate) fn add_patch_node(
        &mut self,
        source: patch::NodeSource,
        at: egui::Pos2,
    ) -> Option<u64> {
        self.create_patch_node(source, at)
    }

    /// Create + install a patch node (returns the new document node_id; `None` when the
    /// node table is full or the engine install failed — the document is rolled back so
    /// doc and engine stay in sync).
    ///
    /// The node is born with its **final** source (spec `plugin-browser.md` D1): a
    /// plugin source is instantiated by this single `install_node` call, so there is no
    /// "create a built-in, then swap the voice" double install (and therefore one
    /// install, one undo step).
    pub(crate) fn create_patch_node(
        &mut self,
        source: patch::NodeSource,
        at: egui::Pos2,
    ) -> Option<u64> {
        let track = self
            .selected_track
            .min(self.project().midi_tracks.len().saturating_sub(1));
        self.create_patch_node_on(source, at, track)
    }

    /// [`GlitcherZApp::create_patch_node`] with an explicit owning track.
    ///
    /// Needed by the cable-midpoint insert: the new processor belongs to the
    /// track that owns the cable it was spliced into, **not** to whichever
    /// track happens to be selected (they differ when the user inserts into
    /// another track's chain). The `track` field drives per-track addressing
    /// (`register_track_chain`) and the mixer panel grouping.
    pub(crate) fn create_patch_node_on(
        &mut self,
        source: patch::NodeSource,
        at: egui::Pos2,
        track: usize,
    ) -> Option<u64> {
        if self.project().graph.nodes.len() >= MAX_NODES {
            self.set_message("Add node failed: node table full (64)".to_string());
            return None;
        }
        // G6 (A design): an input source belongs to exactly one node, and the
        // only wired source today is the computer keyboard — so a second
        // input node has nothing to listen to. Refuse instead of writing a
        // document `validate` would reject.
        if matches!(source, patch::NodeSource::Keyboard)
            && self.project().graph.computer_keyboard().is_some()
        {
            self.set_message(
                "The computer keyboard already has an input node (one source per node)".to_string(),
            );
            return None;
        }
        let node_id = self.project().graph.next_node_id();
        let track = track.min(self.project().midi_tracks.len().saturating_sub(1));
        let node = match source {
            patch::NodeSource::BuiltinSynth => NodeDoc::Instrument {
                node_id,
                track,
                voice: VoiceSpec::Builtin,
                synth: SynthParams::default(),
            },
            patch::NodeSource::Builtin(kind) => NodeDoc::Effect {
                node_id,
                track,
                voice: VoiceSpec::Builtin,
                params: crate::effect::default_params(kind),
                delay: None,
            },
            patch::NodeSource::SoundFont { path } => NodeDoc::Instrument {
                node_id,
                track,
                voice: VoiceSpec::SoundFont {
                    path,
                    bank: 0,
                    program: 0,
                    gain: 1.0,
                    bend_range: 2.0,
                },
                synth: SynthParams::default(),
            },
            patch::NodeSource::Sample { path } => NodeDoc::Instrument {
                node_id,
                track,
                voice: VoiceSpec::Sample {
                    path,
                    root_note: SAMPLE_DEFAULT_ROOT,
                    play_mode: SamplePlayMode::Gate,
                    gain: 1.0,
                    region: glitcherz_model::SampleRegion::default(),
                },
                synth: SynthParams::default(),
            },
            patch::NodeSource::DrumKit => NodeDoc::Instrument {
                node_id,
                track,
                voice: VoiceSpec::DrumKit {
                    slots: Vec::new(),
                    gain: 1.0,
                },
                synth: SynthParams::default(),
            },
            patch::NodeSource::Modal => NodeDoc::Instrument {
                node_id,
                track,
                voice: VoiceSpec::Modal {
                    params: glitcherz_model::ModalParams::default(),
                },
                synth: SynthParams::default(),
            },
            patch::NodeSource::ClapInstrument { path, plugin_id } => NodeDoc::Instrument {
                node_id,
                track,
                voice: VoiceSpec::Clap {
                    path,
                    plugin_id,
                    state: Vec::new(),
                },
                synth: SynthParams::default(),
            },
            patch::NodeSource::Vst3Instrument { path, class_id } => NodeDoc::Instrument {
                node_id,
                track,
                voice: VoiceSpec::Vst3 {
                    path,
                    class_id,
                    state: Vec::new(),
                },
                synth: SynthParams::default(),
            },
            patch::NodeSource::ClapEffect { path, plugin_id } => NodeDoc::Effect {
                node_id,
                track,
                voice: VoiceSpec::Clap {
                    path,
                    plugin_id,
                    state: Vec::new(),
                },
                params: EffectParams::default(),
                delay: None,
            },
            patch::NodeSource::Vst3Effect { path, class_id } => NodeDoc::Effect {
                node_id,
                track,
                voice: VoiceSpec::Vst3 {
                    path,
                    class_id,
                    state: Vec::new(),
                },
                params: EffectParams::default(),
                delay: None,
            },
            patch::NodeSource::Mixer => NodeDoc::Mixer {
                node_id,
                track,
                inputs: MIN_MIXER_CHANNELS as u8,
                channels: vec![Default::default(); MIN_MIXER_CHANNELS],
                out_pan: 0.0,
                out_muted: false,
            },
            // G6: document-only input node — no engine processor, so the
            // usual "installed into the engine?" check does not apply. It
            // owns the computer keyboard (the only wired source in v1).
            patch::NodeSource::Keyboard => NodeDoc::Keyboard {
                node_id,
                track,
                source: MidiSource::Computer,
            },
            // R4: a live input node (no owning track: recording ownership is
            // the `Timeline In` cable).
            patch::NodeSource::AudioInput => NodeDoc::Input {
                node_id,
                channel: 0,
                gain: 1.0,
            },
            patch::NodeSource::Feedback => NodeDoc::Feedback {
                node_id,
                track,
                gain: 0.5,
            },
            // M5: MIDI FX / control-source nodes (spec `midi-nodes.md` §5).
            patch::NodeSource::Arpeggiator => NodeDoc::Arpeggiator {
                node_id,
                track,
                params: ArpeggiatorParams::default(),
            },
            patch::NodeSource::Portamento => NodeDoc::Portamento {
                node_id,
                track,
                params: PortamentoParams::default(),
            },
            patch::NodeSource::MidiEnvelope => NodeDoc::MidiEnvelope {
                node_id,
                track,
                params: MidiEnvelopeParams::default(),
            },
            // C3: the control map (spec `curve-map.md` D5).
            patch::NodeSource::CurveMap => NodeDoc::CurveMap {
                node_id,
                track,
                params: CurveMapParams::default(),
            },
            // Spec `lfo.md`: free-running control source.
            patch::NodeSource::Lfo => NodeDoc::Lfo {
                node_id,
                track,
                params: LfoParams::default(),
            },
        };
        if node.is_keyboard() {
            let before = self.begin_edit();
            self.project_mut().graph.nodes.push(node);
            // No auto-cable: an input node starts silent on purpose (the
            // default project's node is the one that is routed).
            self.commit_edit("Add Input Node", before);
            self.patch_state.place_node(node_id, at);
            return Some(node_id);
        }
        // A plugin source must leave the engine with a real instance: the
        // installer silently falls back to the built-in processor, which would
        // leave a document claiming a plugin voice the engine does not run.
        // Such a node must not exist at all (spec §7 step 2). SoundFonts are
        // exempt: a missing file already falls back to the built-in voice by
        // design (spec `sf2-loader.md` D8), and no guard is involved.
        let expects_guard = matches!(
            node,
            NodeDoc::Instrument {
                voice: VoiceSpec::Clap { .. } | VoiceSpec::Vst3 { .. },
                ..
            } | NodeDoc::Effect {
                voice: VoiceSpec::Clap { .. } | VoiceSpec::Vst3 { .. },
                ..
            }
        );
        let sample_rate = self.engine.info().sample_rate;
        let before = self.begin_edit();
        self.project_mut().graph.nodes.push(node.clone());
        self.install_node(&node, sample_rate);
        let engine_id = self.node_map.get(&node_id).copied();
        let installed = engine_id.is_some_and(|engine_id| {
            !expects_guard
                || self
                    .voice_guards
                    .iter()
                    .any(|entry| entry.node_id == engine_id)
        });
        if !installed {
            // Engine slot/queue full, or the plugin failed to instantiate:
            // roll the document back so doc and engine stay one.
            self.session.restore(before);
            self.set_message(if expects_guard {
                "Add node failed: the plug-in could not be loaded".to_string()
            } else {
                "Add node failed (engine slots or queue full)".to_string()
            });
            return None;
        }
        self.commit_edit("Add Node", before);
        // R4: the input stream only feeds the monitor ring while the graph has
        // an Input node (the processor holds the other end).
        if matches!(node, NodeDoc::Input { .. }) {
            self.engine.set_input_monitor(true);
        }
        self.patch_state.place_node(node_id, at);
        self.register_track_chain(track);
        Some(node_id)
    }

    /// Insert a new node into an existing audio cable: `A → B` becomes `A → new → B`
    /// (cable-midpoint menu). The new node is placed where the user clicked.
    ///
    /// One undo step for the whole splice (spec `plugin-browser.md` E4): the
    /// three sub-operations (create, cut the old cable, two new cables) each
    /// record their own step, so history is suspended for their duration and a
    /// single outer snapshot is committed at the end.
    pub(crate) fn insert_node_in_cable(
        &mut self,
        cable_index: usize,
        source: patch::NodeSource,
        at: egui::Pos2,
    ) {
        let Some(cable) = self.project().graph.cables.get(cable_index).cloned() else {
            return;
        };
        // Spec `midi-nodes.md` §8: a MIDI cable takes a MIDI effect, so the
        // splice runs on the event path instead of the audio one.
        if cable.kind == CableKind::Midi {
            self.insert_midi_fx_in_cable(cable_index, &cable, source, at);
            return;
        }
        let (from_doc, to, in_port) = match splice_endpoints(&cable) {
            Ok(endpoints) => endpoints,
            Err(reason) => {
                self.set_message(format!("Insert failed: {reason}"));
                return;
            }
        };
        if !source.fits_audio_insert() {
            // Unreachable from the menu (its leaves are effects / mixer only),
            // but the picker could hand back anything if a filter is ever
            // widened — an instrument here would be born without a MIDI feed.
            self.set_message("Insert failed: only effects / mixers can be inserted".to_string());
            return;
        }
        // The processor joins the track that owns the cable, not the selected
        // one (see `create_patch_node_on`).
        let track = self
            .project()
            .graph
            .node_by_id(from_doc)
            .map_or(self.selected_track, |node| node.track());
        let before = self.begin_edit();
        self.session.suspend_history();
        let Some(new_id) = self.create_patch_node_on(source, at, track) else {
            self.session.resume_history();
            return;
        };
        self.delete_cable(cable_index);
        self.connect_audio(from_doc, PortRef::Node(new_id), 0);
        // Preserve the original channel binding when the downstream target is
        // a mixer (D17) — the new node takes over the exact same slot.
        self.connect_audio(new_id, to, in_port);
        self.session.resume_history();
        self.commit_edit("Insert Node", before);
    }

    /// Splice a MIDI effect into a MIDI cable: `source → new → target`
    /// (spec `midi-nodes.md` §8). The upstream source may be the timeline, a
    /// virtual keyboard or another MIDI effect.
    pub(crate) fn insert_midi_fx_in_cable(
        &mut self,
        cable_index: usize,
        cable: &glitcherz_model::CableDoc,
        source: patch::NodeSource,
        at: egui::Pos2,
    ) {
        if !source.fits_midi_insert() {
            self.set_message("Insert failed: only MIDI FX nodes can be inserted".to_string());
            return;
        }
        let PortRef::Node(to_doc) = cable.to else {
            self.set_message("Insert failed: MIDI cable target is not a node".to_string());
            return;
        };
        let track = self
            .project()
            .graph
            .node_by_id(to_doc)
            .map_or(self.selected_track, |node| node.track());
        let before = self.begin_edit();
        self.session.suspend_history();
        let Some(new_id) = self.create_patch_node_on(source, at, track) else {
            self.session.resume_history();
            return;
        };
        self.delete_cable(cable_index);
        match &cable.from {
            PortRef::MidiTrack(track) => self.connect_midi_source(*track, new_id),
            PortRef::Node(from_doc) => {
                if self
                    .project()
                    .graph
                    .node_by_id(*from_doc)
                    .is_some_and(NodeDoc::is_keyboard)
                {
                    self.connect_keyboard_midi(*from_doc, new_id);
                } else {
                    self.connect_event_cable(
                        *from_doc,
                        new_id,
                        CableKind::Midi,
                        AutoParam::default(),
                    );
                }
            }
            _ => {}
        }
        self.connect_event_cable(new_id, to_doc, CableKind::Midi, AutoParam::default());
        self.session.resume_history();
        self.commit_edit("Insert MIDI FX", before);
    }

    /// Connect an audio cable (document deduplication + engine edge; both stay in sync).
    /// `in_port` binds the cable to the target's input port (the mixer channel
    /// for mixers, D17; 0 for single-input nodes / master).
    pub(crate) fn connect_audio(&mut self, from_doc: u64, to: PortRef, in_port: u8) {
        let before = self.begin_edit();
        // Strict single-input slots (D14): a new cable into an occupied
        // `(to, in_port)` slot disconnects the old cable (engine `add_cable`
        // mirrors this). Same source → same slot is an idempotent no-op.
        self.project_mut().graph.cables.retain(|c| {
            !(c.kind == CableKind::Audio
                && c.to == to
                && c.in_port == in_port
                && c.from != PortRef::Node(from_doc))
        });
        let dup = self.project().graph.cables.iter().any(|c| {
            c.kind == CableKind::Audio
                && c.from == PortRef::Node(from_doc)
                && c.to == to
                && c.in_port == in_port
        });
        if !dup {
            self.project_mut()
                .graph
                .cables
                .push(glitcherz_model::CableDoc::audio_to_channel(
                    from_doc,
                    to.clone(),
                    in_port,
                ));
        }
        let Some(&from_engine) = self.node_map.get(&from_doc) else {
            self.set_message("Connect failed: source node not in engine graph".to_string());
            return;
        };
        let to_engine = match &to {
            PortRef::Node(id) => self.node_map.get(id).copied(),
            PortRef::Master => Some(self.engine.master_node_id()),
            _ => None,
        };
        let Some(to_engine) = to_engine else {
            self.set_message("Connect failed: target node not in engine graph".to_string());
            return;
        };
        if !self.engine.connect_nodes(from_engine, to_engine, in_port) {
            self.set_message("Connect failed: cycle / fan-in limit / queue full".to_string());
        }
        self.commit_edit("Connect Audio", before);
    }

    /// Removes every MIDI in-edge of `to_doc` from the document and the engine
    /// (one target = one source, spec `midi-nodes.md` §7). The caller owns the
    /// undo step; this helper never commits.
    pub(crate) fn clear_midi_sources(&mut self, to_doc: u64) {
        let to_engine = self.node_map.get(&to_doc).copied();
        let stale: Vec<glitcherz_model::CableDoc> = self
            .project()
            .graph
            .cables
            .iter()
            .filter(|cable| cable.kind == CableKind::Midi && cable.to == PortRef::Node(to_doc))
            .cloned()
            .collect();
        for cable in stale {
            match (&cable.from, to_engine) {
                (PortRef::MidiTrack(track), Some(to)) => {
                    self.engine.disconnect_midi_node(*track, to);
                }
                (PortRef::Node(from_doc), Some(to)) => {
                    if let Some(from) = self.node_map.get(from_doc).copied() {
                        self.engine.disconnect_event(from, to, EventKind::Midi);
                    }
                }
                _ => {}
            }
        }
        self.project_mut()
            .graph
            .cables
            .retain(|cable| !(cable.kind == CableKind::Midi && cable.to == PortRef::Node(to_doc)));
    }

    /// Removes node-to-node control in-edges of `to_doc` (timeline automation
    /// cables are untouched — they keep their looser existing rules).
    pub(crate) fn clear_control_sources(&mut self, to_doc: u64) {
        let to_engine = self.node_map.get(&to_doc).copied();
        let stale: Vec<glitcherz_model::CableDoc> = self
            .project()
            .graph
            .cables
            .iter()
            .filter(|cable| {
                cable.kind == CableKind::Automation
                    && matches!(cable.from, PortRef::Node(_))
                    && cable.to == PortRef::Node(to_doc)
            })
            .cloned()
            .collect();
        for cable in stale {
            if let (PortRef::Node(from_doc), Some(to)) = (&cable.from, to_engine)
                && let Some(from) = self.node_map.get(from_doc).copied()
            {
                self.engine.disconnect_event(from, to, EventKind::Control);
            }
        }
        self.project_mut().graph.cables.retain(|cable| {
            !(cable.kind == CableKind::Automation
                && matches!(cable.from, PortRef::Node(_))
                && cable.to == PortRef::Node(to_doc))
        });
    }

    /// Timeline MIDI track → node with the same replace-the-old-source rule
    /// as every other cable (spec `midi-nodes.md` §7).
    pub(crate) fn connect_midi_source(&mut self, track: usize, to_doc: u64) {
        let before = self.begin_edit();
        self.session.suspend_history();
        self.clear_midi_sources(to_doc);
        self.project_mut()
            .graph
            .cables
            .push(glitcherz_model::CableDoc::midi(track, to_doc));
        let connected = self
            .node_map
            .get(&to_doc)
            .copied()
            .is_some_and(|to| self.engine.connect_midi_node(track, to));
        if !connected {
            self.set_message("MIDI connect failed: fan-out limit / queue full".to_string());
        }
        self.session.resume_history();
        self.commit_edit("Connect MIDI", before);
    }

    /// Node-to-node event / control cable (spec `midi-nodes.md` D6/D10):
    /// document cable + engine edge, deduplicated like every other connection.
    /// The MIDI variant ignores `param` (a MIDI cable carries events, not a
    /// destination).
    pub(crate) fn connect_event_cable(
        &mut self,
        from_doc: u64,
        to_doc: u64,
        kind: CableKind,
        param: AutoParam,
    ) {
        let duplicate = self.project().graph.cables.iter().any(|cable| {
            cable.kind == kind
                && cable.from == PortRef::Node(from_doc)
                && cable.to == PortRef::Node(to_doc)
                && (kind == CableKind::Midi || cable.param == param)
        });
        if duplicate {
            self.set_message("Already connected".to_string());
            return;
        }
        let before = self.begin_edit();
        // Normal cable rule: a new connection replaces whatever occupied the
        // destination. MIDI keeps one event source per node (§7) and control
        // keeps one node source per destination.
        match kind {
            CableKind::Midi => self.clear_midi_sources(to_doc),
            CableKind::Automation => self.clear_control_sources(to_doc),
            CableKind::Audio => {}
        }
        let cable = glitcherz_model::CableDoc {
            kind,
            from: PortRef::Node(from_doc),
            to: PortRef::Node(to_doc),
            param: if kind == CableKind::Midi {
                AutoParam::default()
            } else {
                param.clone()
            },
            in_port: 0,
            map: None,
        };
        // Control cables derive their axis from the target (`map: None`), so
        // the curve keeps its shape and follows the target's range; a Curve Map
        // source overrides that derivation in `cable_map`. A MIDI cable
        // delivers events, so there is nothing to map.
        let map = if kind == CableKind::Midi {
            glitcherz_model::MapSpec::UNIT
        } else {
            cable_map(&cable, &self.project().graph)
        };
        self.project_mut().graph.cables.push(cable);
        self.commit_edit(
            if kind == CableKind::Midi {
                "Connect MIDI FX"
            } else {
                "Connect Control"
            },
            before.clone(),
        );
        let event_kind = if kind == CableKind::Midi {
            EventKind::Midi
        } else {
            EventKind::Control
        };
        // Resolved before the engine call: `self.project()` is an immutable
        // borrow that cannot overlap the mutable `self.engine`.
        let dest = event_dest_of(&param, &self.project().graph, &PortRef::Node(to_doc));
        if let (Some(from_engine), Some(to_engine)) = (
            self.node_map.get(&from_doc).copied(),
            self.node_map.get(&to_doc).copied(),
        ) && !self.engine.connect_event(
            from_engine,
            to_engine,
            event_kind,
            dest,
            engine_map(map),
        ) {
            // The engine refused (cycle / second MIDI source / caps): roll the
            // document back so it never claims a cable the audio thread does
            // not have.
            self.session.restore(before);
            self.set_message(
                "Connect failed: cycle / one MIDI source per node / queue full".to_string(),
            );
        }
    }

    /// Connect a keyboard-input MIDI cable (G6, spec `plugin-gui.md` §8):
    /// **document-only** — the input node has no engine node, so there is no
    /// edge to dispatch; the live routes are resolved per frame from the
    /// document (`GraphDoc::computer_keyboard` + `keyboard_routes_from`).
    ///
    /// A MIDI source is one out jack with any number of cables (multi-out /
    /// single-in — the same rule the timeline's track outlets follow), so a
    /// new cable **adds** a destination instead of replacing the previous one:
    /// that is what makes synth layering possible. Duplicate cables are
    /// ignored, and the fan-out stops at [`MAX_MIDI_FANOUT`].
    pub(crate) fn connect_keyboard_midi(&mut self, from_doc: u64, to_doc: u64) {
        let is_vk = self
            .project()
            .graph
            .node_by_id(from_doc)
            .is_some_and(NodeDoc::is_keyboard);
        if !is_vk {
            self.set_message("Connect failed: source is not a virtual keyboard".to_string());
            return;
        }
        let existing: Vec<u64> = self
            .project()
            .graph
            .cables
            .iter()
            .filter(|c| c.kind == CableKind::Midi && c.from == PortRef::Node(from_doc))
            .filter_map(|c| match c.to {
                PortRef::Node(id) => Some(id),
                _ => None,
            })
            .collect();
        if existing.contains(&to_doc) {
            self.set_message("Already connected".to_string());
            return;
        }
        if existing.len() >= MAX_MIDI_FANOUT {
            self.set_message(format!(
                "Connect failed: a keyboard feeds at most {MAX_MIDI_FANOUT} instruments"
            ));
            return;
        }
        let before = self.begin_edit();
        self.clear_midi_sources(to_doc);
        self.project_mut()
            .graph
            .cables
            .push(glitcherz_model::CableDoc::keyboard_midi(from_doc, to_doc));
        self.commit_edit("Connect Keyboard", before);
    }

    /// Connects one source into a timeline track's **recording input** (R6 D21-D24).
    ///
    /// A recording input is a **pure sink** on the fixed `Timeline In`
    /// terminal: the cable is a document fact (it arms the track, D22) and adds
    /// no engine edge. The model's `validate` is the single authority for type
    /// matching, track ranges and the single-source rule, so the edit is pushed
    /// first and rolled back with its own message when the graph refuses it —
    /// the same pattern as an engine-refused event cable.
    pub(crate) fn connect_track_input(&mut self, from: PortRef, kind: TrackKind, track: usize) {
        if matches!(
            from,
            PortRef::TrackIn { .. } | PortRef::TimelineIn | PortRef::Master
        ) {
            self.set_message(
                "Connect failed: the source port carries no recording signal".to_string(),
            );
            return;
        }
        let cable_kind = match kind {
            TrackKind::Midi => CableKind::Midi,
            TrackKind::Curve => CableKind::Automation,
            TrackKind::Audio => CableKind::Audio,
        };
        // A recording input is single-source: reconnect replaces the previous
        // cable, except when the very same source is already there.
        let duplicate = self
            .project()
            .graph
            .cables
            .iter()
            .any(|cable| cable.to == (PortRef::TrackIn { kind, track }) && cable.from == from);
        if duplicate {
            self.set_message("Already connected".to_string());
            return;
        }
        let in_port = glitcherz_model::timeline_in_port(
            kind,
            track,
            self.project().midi_tracks.len(),
            self.project().control_tracks.len(),
            self.project().audio_tracks.len(),
        )
        .unwrap_or(0);
        let before = self.begin_edit();
        self.project_mut()
            .graph
            .cables
            .retain(|cable| cable.to != (PortRef::TrackIn { kind, track }));
        self.project_mut()
            .graph
            .cables
            .push(glitcherz_model::CableDoc {
                kind: cable_kind,
                from,
                to: PortRef::TrackIn { kind, track },
                param: AutoParam::default(),
                in_port,
                map: None,
            });
        if let Err(err) = self.project().validate() {
            self.session.restore(before);
            self.set_message(format!("Connect failed: {err}"));
            return;
        }
        self.commit_edit("Connect Recording Input", before);
        let label = match kind {
            TrackKind::Midi => "MIDI",
            TrackKind::Curve => "curve",
            TrackKind::Audio => "audio",
        };
        self.set_message(format!(
            "Recording input: track {} {label} armed by wiring",
            track + 1
        ));
    }

    /// Ctrl+L (G6 quick audition): toggle MIDI cables from the
    /// computer-keyboard input node to every **selected instrument node**.
    ///
    /// Unlinked selection → link it; fully linked selection → unlink exactly
    /// those. Cables to other instruments are left untouched, so the shortcut
    /// never silently rewires a hand-built patch (layering survives), and the
    /// `MAX_MIDI_FANOUT` ceiling still applies when linking.
    pub(crate) fn toggle_audition_route(&mut self) {
        let selected = self.patch_state.selected_doc_nodes();
        let instruments: Vec<u64> = self
            .project()
            .graph
            .nodes
            .iter()
            .filter(|n| matches!(n, NodeDoc::Instrument { .. }) && selected.contains(&n.node_id()))
            .map(NodeDoc::node_id)
            .collect();
        if instruments.is_empty() {
            self.set_message("Select an instrument node in the patch to audition it".to_string());
            return;
        }
        let Some(vk) = self.project().graph.computer_keyboard() else {
            self.set_message(
                "No keyboard input node: add one from the patch context menu".to_string(),
            );
            return;
        };
        let linked: Vec<u64> = self
            .project()
            .graph
            .cables
            .iter()
            .filter(|c| c.kind == CableKind::Midi && c.from == PortRef::Node(vk))
            .filter_map(|c| match c.to {
                PortRef::Node(id) => Some(id),
                _ => None,
            })
            .collect();
        let missing: Vec<u64> = instruments
            .iter()
            .copied()
            .filter(|id| !linked.contains(id))
            .collect();
        if missing.is_empty() {
            let before = self.begin_edit();
            self.project_mut().graph.cables.retain(|c| {
                !(c.kind == CableKind::Midi
                    && c.from == PortRef::Node(vk)
                    && matches!(c.to, PortRef::Node(id) if instruments.contains(&id)))
            });
            self.commit_edit("Unlink Audition", before);
            self.set_message("Keyboard unlinked from the selected synth".to_string());
            return;
        }
        if linked.len() + missing.len() > MAX_MIDI_FANOUT {
            self.set_message(format!(
                "Link failed: a keyboard feeds at most {MAX_MIDI_FANOUT} instruments"
            ));
            return;
        }
        let before = self.begin_edit();
        for to in &missing {
            self.project_mut()
                .graph
                .cables
                .push(glitcherz_model::CableDoc::keyboard_midi(vk, *to));
        }
        self.commit_edit("Link Audition", before);
        self.set_message(format!(
            "Keyboard linked to {} synth{}",
            missing.len(),
            if missing.len() == 1 { "" } else { "s" }
        ));
    }

    /// Delete a cable (document index i; document and engine stay in sync).
    pub(crate) fn delete_cable(&mut self, i: usize) {
        let Some(cable) = self.project().graph.cables.get(i).cloned() else {
            return;
        };
        match cable.kind {
            CableKind::Audio => {
                if let (PortRef::Node(from), Some(to_engine)) = (
                    &cable.from,
                    match &cable.to {
                        PortRef::Node(id) => self.node_map.get(id).copied(),
                        PortRef::Master => Some(self.engine.master_node_id()),
                        _ => None,
                    },
                ) {
                    if let Some(from_engine) = self.node_map.get(from).copied() {
                        self.engine.disconnect_nodes(from_engine, to_engine);
                    }
                }
            }
            CableKind::Midi => match (&cable.from, &cable.to) {
                (PortRef::MidiTrack(track), PortRef::Node(to_doc)) => {
                    if let Some(to_engine) = self.node_map.get(to_doc).copied() {
                        self.engine.disconnect_midi_node(*track, to_engine);
                    }
                }
                // Node-to-node MIDI cable (MIDI effect / keyboard source).
                // The keyboard is document-only, so its cable has no engine
                // edge to drop.
                (PortRef::Node(from_doc), PortRef::Node(to_doc)) => {
                    if let (Some(from_engine), Some(to_engine)) = (
                        self.node_map.get(from_doc).copied(),
                        self.node_map.get(to_doc).copied(),
                    ) {
                        self.engine
                            .disconnect_event(from_engine, to_engine, EventKind::Midi);
                    }
                }
                _ => {}
            },
            CableKind::Automation => match (&cable.from, &cable.to) {
                (PortRef::AutoTrack(index), _) => {
                    self.engine.clear_auto_route(*index);
                }
                (PortRef::Node(from_doc), PortRef::Node(to_doc)) => {
                    if let (Some(from_engine), Some(to_engine)) = (
                        self.node_map.get(from_doc).copied(),
                        self.node_map.get(to_doc).copied(),
                    ) {
                        self.engine
                            .disconnect_event(from_engine, to_engine, EventKind::Control);
                    }
                }
                _ => {}
            },
        }
        let before = self.begin_edit();
        self.project_mut().graph.cables.remove(i);
        self.commit_edit("Delete Cable", before);
    }

    /// Delete a document node (any instrument / effect / mixer; the master output is
    /// not a document node and is never deletable) — engine `remove_node` bypasses
    /// single-input nodes (in-edge sources × out-edge targets become direct
    /// connections) and **drops the cables of multi-input nodes** (D19: only
    /// nodes with ≤1 audio in-edge get bypass rewiring; deleting a mixer just
    /// disconnects its feeders). The document mirrors the same semantics. MIDI /
    /// automation cables touching the node are simply removed (a deleted
    /// instrument loses its MIDI feed; a deleted parameter target loses its
    /// automation cable). The owning track's addressing table is refreshed afterwards.
    pub(crate) fn delete_graph_node(&mut self, doc_id: u64) {
        let Some(node) = self.project().graph.node_by_id(doc_id) else {
            return;
        };
        let track = node.track();
        // G6: the vk node is document-only (no engine counterpart).
        let is_keyboard = node.is_keyboard();
        let is_input = matches!(node, NodeDoc::Input { .. });
        let audio_in_count = self
            .project()
            .graph
            .cables
            .iter()
            .filter(|c| c.kind == CableKind::Audio && c.to == PortRef::Node(doc_id))
            .count();
        // Z0 Z-D9: a dual-input node never bypasses — its single in-edge may
        // be the Mod port, which has no meaningful direct replacement.
        let is_dual = node.audio_inputs() == 2;
        // Document bypass pairs: in-cable sources × out-cable targets (audio
        // only; skipped for multi-input nodes — D19 mixer/master deletion).
        let ins: Vec<PortRef> = if audio_in_count <= 1 && !is_dual {
            self.project()
                .graph
                .cables
                .iter()
                .filter(|c| c.kind == CableKind::Audio && c.to == PortRef::Node(doc_id))
                .map(|c| c.from.clone())
                .collect()
        } else {
            Vec::new()
        };
        let outs: Vec<PortRef> = self
            .project()
            .graph
            .cables
            .iter()
            .filter(|c| c.from == PortRef::Node(doc_id))
            .map(|c| c.to.clone())
            .collect();
        // The port the deleted node occupied on its downstream target (the
        // mixer channel to preserve in bypass pairs, D17).
        let out_port = self
            .project()
            .graph
            .cables
            .iter()
            .find(|c| c.kind == CableKind::Audio && c.from == PortRef::Node(doc_id))
            .map(|c| c.in_port)
            .unwrap_or(0);
        // G6: the vk node has no engine counterpart — only the document
        // loses it (its MIDI cables go with it, no bypass rewiring).
        let engine_id = if is_keyboard {
            None
        } else {
            let Some(&engine_id) = self.node_map.get(&doc_id) else {
                return;
            };
            if !self.engine.remove_node(engine_id) {
                self.set_message("Node removal failed (queue full)".to_string());
                return;
            }
            Some(engine_id)
        };
        let before = self.begin_edit();
        self.project_mut()
            .graph
            .nodes
            .retain(|n| n.node_id() != doc_id);
        self.project_mut()
            .graph
            .cables
            .retain(|c| c.from != PortRef::Node(doc_id) && c.to != PortRef::Node(doc_id));
        for from in &ins {
            if !matches!(from, PortRef::Node(_)) {
                continue; // Audio sources can only be nodes.
            }
            for to in &outs {
                // Preserve the mixer channel binding when the bypass target is
                // a mixer (D17); non-mixer targets must carry in_port 0.
                let port = match to {
                    PortRef::Node(to_id)
                        if self
                            .project()
                            .graph
                            .node_by_id(*to_id)
                            .is_some_and(|n| n.is_mixer()) =>
                    {
                        out_port
                    }
                    _ => 0,
                };
                self.project_mut()
                    .graph
                    .cables
                    .push(glitcherz_model::CableDoc::audio_to_channel(
                        from.clone(),
                        to.clone(),
                        port,
                    ));
            }
        }
        self.node_map.remove(&doc_id);
        self.effect_meters.remove(&doc_id);
        if let Some(engine_id) = engine_id {
            self.retire_guard(engine_id);
        }
        // Param caches are node-keyed (G4): dropping them for a deleted
        // node is always safe.
        self.clap_effect_params.remove(&doc_id);
        self.clap_effect_values.remove(&doc_id);
        self.vst3_effect_params.remove(&doc_id);
        self.vst3_effect_values.remove(&doc_id);
        self.voice_params.remove(&doc_id);
        self.clap_voice_params.remove(&doc_id);
        self.clap_voice_values.remove(&doc_id);
        self.vst3_voice_values.remove(&doc_id);
        self.voice_default_state.remove(&doc_id);
        self.voice_preset_selected.remove(&doc_id);
        self.commit_edit("Delete Node", before);
        // R4: stop feeding the monitor ring when the last Input node leaves.
        if is_input {
            self.engine.set_input_monitor(
                self.project()
                    .graph
                    .nodes
                    .iter()
                    .any(|node| matches!(node, NodeDoc::Input { .. })),
            );
        }
        // The deleted node may have filled a role (voice / delay / channel) in the track
        // addressing table — refresh it from the remaining document nodes.
        self.register_track_chain(track);
    }
}
