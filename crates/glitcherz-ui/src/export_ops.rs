//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    /// Builds one document node's **processor** for the offline export
    /// engine (U7 D4): the sound path only — spawn + persisted state +
    /// construction-time parameter replay. Unlike [`Self::install_node`]
    /// this touches **no session caches and no guard list**: the guard
    /// lands in the caller's collection and dies with the export job.
    pub(crate) fn spawn_offline_node(
        &mut self,
        engine: &mut glitcherz_engine::AudioEngine,
        node_map: &mut HashMap<u64, u64>,
        guards: &mut Vec<PluginGuard>,
        node: &NodeDoc,
        export_rate: u32,
    ) {
        if node.is_keyboard() {
            return; // document-only routing node, no engine counterpart
        }
        // S3c: the export can run at a rate the device is not on, so every
        // processor must be built for the **offline engine's** rate — reading
        // the live engine here is what kept the export glued to the device.
        let sr = convert::sample_rate_to_f32(export_rate);
        let sample_rate = export_rate;
        let (processor, guard): (Box<dyn Processor>, Option<PluginGuard>) = match node {
            NodeDoc::Instrument { voice, synth, .. } => match voice {
                VoiceSpec::Builtin => {
                    // Spec `instrument.md`: the export engine must hear the
                    // same timbre as the live one (waveform / filter / ADSR).
                    let mut processor = SynthProcessor::new(sr);
                    for (id, value) in synth_param_values(synth) {
                        processor.set_param(id, value);
                    }
                    (Box::new(processor), None)
                }
                VoiceSpec::Sample {
                    path,
                    root_note,
                    play_mode,
                    gain,
                    region,
                    ..
                } => match self.obtain_sample_voice(path, sr) {
                    Ok(mut processor) => {
                        processor.set_param(ParamId::SampleRootNote, f32::from(*root_note));
                        processor.set_param(
                            ParamId::SamplePlayMode,
                            if matches!(play_mode, SamplePlayMode::OneShot) {
                                1.0
                            } else {
                                0.0
                            },
                        );
                        processor.set_param(ParamId::SampleGain, *gain);
                        // D12: the offline render must hear the same window as
                        // the live graph, or an export of a trimmed loop would
                        // play material the user never hears.
                        processor.set_param(ParamId::SampleStart, region.start as f32);
                        processor.set_param(ParamId::SampleLoopStart, region.loop_start as f32);
                        processor.set_param(ParamId::SampleLoopEnd, region.loop_end as f32);
                        processor.set_param(ParamId::SampleEnd, region.end as f32);
                        processor.set_param(
                            ParamId::SampleLoopMode,
                            f32::from(region.loop_mode.index()),
                        );
                        (Box::new(processor), None)
                    }
                    Err(err) => {
                        log::error!("Export: sample {path} failed to load: {err}");
                        (Box::new(SynthProcessor::new(sr)), None)
                    }
                },
                VoiceSpec::DrumKit { slots, gain } => {
                    if slots.is_empty() {
                        (Box::new(SilentProcessor::new()), None)
                    } else {
                        let processor = self.obtain_drum_kit_voice(slots, *gain, sr);
                        (Box::new(processor), None)
                    }
                }
                VoiceSpec::SoundFont {
                    path,
                    bank,
                    program,
                    gain,
                    bend_range,
                } => {
                    if path.is_empty() {
                        // U13: empty node renders silence on export too.
                        (Box::new(SilentProcessor::new()), None)
                    } else {
                        match self.obtain_sf2_voice(path, sr) {
                            Ok(mut processor) => {
                                processor.set_param(ParamId::Sf2BankSelect, f32::from(*bank));
                                processor.set_param(ParamId::Sf2ProgramChange, f32::from(*program));
                                processor.set_param(ParamId::Sf2Gain, *gain);
                                processor.set_param(ParamId::Sf2BendRange, *bend_range);
                                (Box::new(processor), None)
                            }
                            Err(err) => {
                                log::error!("Export: sound font {path} failed to load: {err}");
                                (Box::new(SynthProcessor::new(sr)), None)
                            }
                        }
                    }
                }
                VoiceSpec::Clap {
                    path,
                    plugin_id,
                    state,
                } => match ClapInstance::spawn_rt(
                    std::path::Path::new(path),
                    plugin_id,
                    ClapInstanceConfig {
                        sample_rate,
                        max_frames: 128,
                    },
                ) {
                    Ok(mut instance) => {
                        if !state.is_empty()
                            && let Err(err) = instance.guard.load_state(state)
                        {
                            log::warn!("Export: CLAP state restore failed for {plugin_id}: {err}");
                        }
                        let ClapInstance { guard, node } = instance;
                        (Box::new(node), Some(PluginGuard::Clap(guard)))
                    }
                    Err(err) => {
                        log::error!("Export: CLAP {plugin_id} failed to spawn: {err}");
                        (Box::new(SynthProcessor::new(sr)), None)
                    }
                },
                VoiceSpec::Vst3 {
                    path,
                    class_id,
                    state,
                    ..
                } => match glitcherz_vst3_host::Vst3Instance::spawn_rt(
                    std::path::Path::new(path),
                    class_id,
                    glitcherz_vst3_host::Vst3InstanceConfig {
                        sample_rate,
                        max_frames: 128,
                    },
                ) {
                    Ok(instance) => {
                        if !state.is_empty()
                            && let Err(err) = instance.guard.load_state(state)
                        {
                            log::warn!("Export: VST3 state restore failed for {class_id}: {err}");
                        }
                        let glitcherz_vst3_host::Vst3Instance { guard, node } = instance;
                        (Box::new(node), Some(PluginGuard::Vst3(guard)))
                    }
                    Err(err) => {
                        log::error!("Export: VST3 {class_id} failed to spawn: {err}");
                        (Box::new(SynthProcessor::new(sr)), None)
                    }
                },
            },
            NodeDoc::Effect { voice, params, .. } => match voice {
                VoiceSpec::Builtin
                | VoiceSpec::SoundFont { .. }
                | VoiceSpec::Sample { .. }
                | VoiceSpec::DrumKit { .. } => {
                    (crate::effect::processor(params, sr).processor, None)
                }
                VoiceSpec::Clap {
                    path,
                    plugin_id,
                    state,
                } => match ClapInstance::spawn_rt(
                    std::path::Path::new(path),
                    plugin_id,
                    ClapInstanceConfig {
                        sample_rate,
                        max_frames: 128,
                    },
                ) {
                    Ok(mut instance) => {
                        if !state.is_empty()
                            && let Err(err) = instance.guard.load_state(state)
                        {
                            log::warn!("Export: CLAP effect state restore failed: {err}");
                        }
                        let ClapInstance { guard, node } = instance;
                        (Box::new(node), Some(PluginGuard::Clap(guard)))
                    }
                    Err(err) => {
                        log::error!("Export: CLAP effect {plugin_id} failed to spawn: {err}");
                        (crate::effect::processor(params, sr).processor, None)
                    }
                },
                VoiceSpec::Vst3 {
                    path,
                    class_id,
                    state,
                    ..
                } => match glitcherz_vst3_host::Vst3Instance::spawn_rt(
                    std::path::Path::new(path),
                    class_id,
                    glitcherz_vst3_host::Vst3InstanceConfig {
                        sample_rate,
                        max_frames: 128,
                    },
                ) {
                    Ok(instance) => {
                        if !state.is_empty()
                            && let Err(err) = instance.guard.load_state(state)
                        {
                            log::warn!("Export: VST3 effect state restore failed: {err}");
                        }
                        let glitcherz_vst3_host::Vst3Instance { guard, node } = instance;
                        (Box::new(node), Some(PluginGuard::Vst3(guard)))
                    }
                    Err(err) => {
                        log::error!("Export: VST3 effect {class_id} failed to spawn: {err}");
                        (crate::effect::processor(params, sr).processor, None)
                    }
                },
            },
            NodeDoc::Mixer {
                channels,
                out_pan,
                out_muted,
                ..
            } => {
                let eng: Vec<EngineMixerChannel> = channels
                    .iter()
                    .map(|c| EngineMixerChannel {
                        gain: c.gain,
                        pan: c.pan,
                        muted: c.muted,
                        solo: c.solo,
                    })
                    .collect();
                (
                    Box::new(MixerProcessor::with_channels(&eng, *out_pan, *out_muted)),
                    None,
                )
            }
            NodeDoc::Feedback { gain, .. } => {
                let mut processor = FeedbackProcessor::new();
                processor.set_param(ParamId::FeedbackGain, *gain);
                (Box::new(processor), None)
            }
            NodeDoc::ChannelStrip { .. } => (Box::new(ChannelStripProcessor::new()), None),
            // R4: an offline render has no live input; the node is silent.
            NodeDoc::Input { .. } => (Box::new(SilentProcessor::new()), None),
            NodeDoc::AudioTrack { track, .. } => {
                let table = self.build_audio_clip_table(*track);
                (Box::new(AudioTrackProcessor::new(table)), None)
            }
            // Unreachable (returned above); kept for exhaustiveness.
            NodeDoc::Keyboard { .. } => (Box::new(SynthProcessor::new(sr)), None),
            // M3/M4: arpeggiator / portamento / envelope are live.
            NodeDoc::Arpeggiator { params, .. } => {
                let mut processor = ArpeggiatorProcessor::new(node.node_id() as u32);
                processor.apply_params(
                    params.division,
                    params.gate,
                    params.mode.index(),
                    params.octaves,
                    params.latch,
                    params.harmony.index(),
                    params.skip,
                );
                (Box::new(processor), None)
            }
            NodeDoc::Portamento { params, .. } => {
                let mut processor = PortamentoProcessor::new();
                processor.apply_params(params.time_ms, params.min, params.max, params.legato_only);
                (Box::new(processor), None)
            }
            NodeDoc::MidiEnvelope { params, .. } => {
                let mut processor = MidiEnvelopeProcessor::new();
                processor.apply_params(params.attack, params.decay, params.sustain, params.release);
                processor.set_param(ParamId::EnvMin, params.min);
                processor.set_param(ParamId::EnvMax, params.max);
                (Box::new(processor), None)
            }
            NodeDoc::CurveMap { params, .. } => {
                let mut processor = CurveMapProcessor::new();
                processor.apply_params(params.min, params.max, engine_mode(params.mode));
                (Box::new(processor), None)
            }
            NodeDoc::Lfo { params, .. } => {
                let mut processor = LfoProcessor::new(node.node_id() as u32);
                processor.apply_params(
                    params.waveform.index(),
                    params.rate_hz,
                    params.sync,
                    params.division,
                    params.depth,
                );
                (Box::new(processor), None)
            }
        };
        let engine_id = engine.fresh_node_id();
        let kind = match node {
            NodeDoc::Mixer { .. } => NodeKind::Mixer,
            NodeDoc::Feedback { .. } => NodeKind::Feedback,
            _ => NodeKind::Plain,
        };
        if engine.add_node(engine_id, kind, processor) {
            node_map.insert(node.node_id(), engine_id);
            if let Some(guard) = guard {
                guards.push(guard);
            }
        } else if let Some(guard) = guard {
            drop(guard); // never entered the graph: release immediately
        }
    }

    /// Assembles the offline export engine from the current document (U7
    /// D4: the mirror of `apply_document` — nodes, track addressing,
    /// cables, the three clip-table pushes, the arrangement floor — minus
    /// every session-cache side effect) and returns the started job.
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
        let sample_rate = format.sample_rate;
        let mut engine = glitcherz_engine::AudioEngine::start_offline(sample_rate)
            .expect("the offline engine needs no audio device");
        let mut node_map: HashMap<u64, u64> = HashMap::new();
        let mut guards: Vec<PluginGuard> = Vec::new();
        let nodes = self.project().graph.nodes.clone();
        for node in &nodes {
            self.spawn_offline_node(&mut engine, &mut node_map, &mut guards, node, sample_rate);
        }
        // Track addressing (the mirror of register_track_chain).
        for track in 0..self.project().midi_tracks.len() {
            let resolve = |doc_id: Option<u64>| -> Option<u64> {
                doc_id.and_then(|id| node_map.get(&id).copied())
            };
            let voice = resolve(
                self.project()
                    .graph
                    .instrument_of_track(track)
                    .map(NodeDoc::node_id),
            );
            let delay = resolve(
                self.project()
                    .graph
                    .builtin_delay_of_track(track)
                    .map(NodeDoc::node_id),
            );
            let _ = engine.register_track_chain(track, voice, delay, delay);
        }
        // Cables (the mirror of apply_document's dispatch, minus vk
        // document-only routing — the export never routes the keyboard).
        let master = engine.master_node_id();
        let graph = self.project().graph.clone();
        let cables = graph.cables.clone();
        for cable in &cables {
            let from = match &cable.from {
                PortRef::Node(id) => node_map.get(id).copied(),
                _ => None,
            };
            let to_node = match &cable.to {
                PortRef::Node(id) => node_map.get(id).copied(),
                PortRef::Master => Some(master),
                _ => None,
            };
            match cable.kind {
                CableKind::Audio => {
                    if let (Some(from), Some(to)) = (from, to_node) {
                        let _ = engine.connect_nodes(from, to, cable.in_port);
                    }
                }
                CableKind::Midi => match &cable.from {
                    PortRef::MidiTrack(track) => {
                        if let Some(to) = to_node {
                            let _ = engine.connect_midi_node(*track, to);
                        }
                    }
                    PortRef::Node(id) => {
                        if let (Some(from), Some(to)) = (node_map.get(id).copied(), to_node) {
                            let _ = engine.connect_event(
                                from,
                                to,
                                EventKind::Midi,
                                AutoDest::default(),
                                EngineMapSpec::UNIT,
                            );
                        }
                    }
                    _ => {}
                },
                CableKind::Automation => {
                    if let PortRef::Node(id) = &cable.from {
                        if cable.to == PortRef::TimelineIn {
                            continue;
                        }
                        if let (Some(from), Some(to)) = (node_map.get(id).copied(), to_node) {
                            let _ = engine.connect_event(
                                from,
                                to,
                                EventKind::Control,
                                event_dest_of(&cable.param, &graph, &cable.to),
                                engine_map(cable_map(cable, &graph)),
                            );
                        }
                        continue;
                    }
                    let PortRef::AutoTrack(index) = &cable.from else {
                        continue;
                    };
                    if cable.to == PortRef::TimelineIn {
                        let _ = engine
                            .set_timeline_in_route(*index, engine_map(cable_map(cable, &graph)));
                        continue;
                    }
                    let Some(to) = to_node else {
                        continue;
                    };
                    if let Some((controller, channel)) = cable.param.cc_of() {
                        let _ = engine.set_auto_cc_route(
                            *index,
                            to,
                            channel,
                            controller,
                            engine_map(cable_map(cable, &graph)),
                        );
                        continue;
                    }
                    if let Some((channel, _)) = cable.param.pitch_bend_of() {
                        let _ = engine.set_auto_pitch_bend_route(
                            *index,
                            to,
                            channel,
                            cable.param.bend_range(&graph, &cable.to),
                            engine_map(cable_map(cable, &graph)),
                        );
                        continue;
                    }
                    if let Some(id) = param_id_of(&cable.param) {
                        let _ = engine.set_auto_route(
                            *index,
                            to,
                            id,
                            engine_map(cable_map(cable, &graph)),
                        );
                    }
                }
            }
        }
        engine.set_midi_track_count(self.project().midi_tracks.len());
        // The three clip-table pushes (control clips keep automation — and
        // the tempo curve — live in the export; audio clips included).
        for track in 0..self.project().midi_tracks.len() {
            let _ = engine.send_midi_clips(track, self.rt_clips(track));
        }
        for index in 0..self.project().control_tracks.len() {
            let Some(control_track) = self.project().control_tracks.get(index) else {
                continue;
            };
            let mut clips = [RtControlClip::default(); MAX_TRACK_CLIPS];
            for (slot, clip) in clips.iter_mut().zip(control_track.clips.iter()) {
                let mut curve = RtAutomation {
                    interp: rt_curve_interp(clip.interp),
                    ..RtAutomation::default()
                };
                for point in clip.curve.iter().take(MAX_AUTO_POINTS) {
                    curve.push(RtAutoPoint {
                        tick: point.tick,
                        value: point.value,
                    });
                }
                *slot = RtControlClip {
                    start_ticks: clip.start_ticks,
                    span_ticks: clip.span_ticks,
                    loop_ticks: clip.loop_ticks,
                    curve,
                };
            }
            let _ = engine.set_control_clips(index, clips);
        }
        for track in 0..self.project().audio_tracks.len() {
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
            let table = self.build_audio_clip_table(track);
            let _ = engine.send_audio_clips(engine_id, table);
        }
        // U7 D5: the auto-stop must not fire before the latest clip end
        // across ALL track kinds (audio is not sequencer content).
        let _ = engine.set_arrangement_floor(export_audio::export_end_ticks(self.project()));
        // No loop range is pushed — the offline render never wraps.
        engine.seek_ticks(window.0);
        let _ = engine.send(EngineCommand::Sequencer(SequencerCommand::Play));
        match export_audio::ExportJob::new(engine, guards, node_map, format, window, passes) {
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
