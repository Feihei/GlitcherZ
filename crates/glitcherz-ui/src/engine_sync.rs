//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    // ---- Document → engine (D13 fully explicit: the loader rebuilds purely from the document) ----

    /// Rebuild the current project document into the engine: remove old nodes → install document
    /// nodes → assemble track addressing tables → connect cables per the document → push the
    /// active track count.
    pub(crate) fn apply_document(&mut self) {
        // R4: stop feeding the monitor ring while the graph is being rebuilt;
        // a later Input node must not start on stale samples.
        self.engine.set_input_monitor(false);
        // 1) Remove the nodes the previous document installed into the engine; all their guards
        //    move to the retiring zone and are released in order once recycling receipts arrive
        //    (engine ids are never reused, so pairing is unambiguous).
        let stale: Vec<u64> = self.node_map.values().copied().collect();
        for engine_id in stale {
            if self.engine.remove_node(engine_id) {
                self.retire_guard(engine_id);
            }
        }
        self.node_map.clear();
        self.voice_params.clear();
        self.clap_voice_params.clear();
        self.clap_voice_values.clear();
        self.vst3_voice_values.clear();
        self.clap_effect_params.clear();
        self.clap_effect_values.clear();
        self.vst3_effect_params.clear();
        self.vst3_effect_values.clear();
        self.voice_default_state.clear();
        self.voice_preset_selected.clear();

        // 2) Install document nodes (external plugin failure → fall back to built-in, keeping the
        //    chain intact).
        let sample_rate = self.engine.info().sample_rate;
        let nodes = self.project().graph.nodes.clone();
        for node in &nodes {
            self.install_node(node, sample_rate);
        }
        // R4: feed the monitor ring only while an Input node exists.
        self.engine.set_input_monitor(
            self.project()
                .graph
                .nodes
                .iter()
                .any(|node| matches!(node, NodeDoc::Input { .. })),
        );
        //(install_node instantiates plugins at the u32 sample rate; built-in processors convert
        // to f32 via convert.)

        // 3) Assemble track addressing tables (track-coordinate addressing for manual
        //    params/levels/keyboard).
        for track in 0..self.project().midi_tracks.len() {
            self.register_track_chain(track);
        }

        // 4) Connect cables per the document. The graph is cloned once: the
        //    engine calls below need `&mut self.engine`, which would collide
        //    with a live borrow of the document's graph (maps resolve through
        //    the target's axis, and effect slots need the node payload).
        let master = self.engine.master_node_id();
        let graph = self.project().graph.clone();
        let cables = graph.cables.clone();
        for cable in &cables {
            let from = match &cable.from {
                PortRef::Node(id) => self.node_map.get(id).copied(),
                _ => None,
            };
            let to_node = match &cable.to {
                PortRef::Node(id) => self.node_map.get(id).copied(),
                PortRef::Master => Some(master),
                _ => None,
            };
            match cable.kind {
                CableKind::Audio => {
                    let (Some(from), Some(to)) = (from, to_node) else {
                        log::warn!("Audio cable endpoint node missing, skipping: {cable:?}");
                        continue;
                    };
                    // D17: the cable binds to the target's input port (the
                    // mixer channel; 0 for single-input nodes / master).
                    if !self.engine.connect_nodes(from, to, cable.in_port) {
                        log::warn!(
                            "Audio cable ({from} → {to}) dispatch failed (cycle / limit exceeded / queue full)"
                        );
                    }
                }
                CableKind::Midi => {
                    // G6: vk → instrument cables are document-only routing
                    // (the keyboard has no engine node; the app resolves the
                    // route and addresses the instrument directly). MIDI
                    // effects are real engine nodes (spec `midi-nodes.md`
                    // D10), so their cables become event cables.
                    match &cable.from {
                        PortRef::MidiTrack(track) => {
                            let Some(to) = to_node else {
                                log::warn!("MIDI cable target node missing, skipping: {cable:?}");
                                continue;
                            };
                            if !self.engine.connect_midi_node(*track, to) {
                                log::warn!(
                                    "MIDI cable (track {track} → {to}) dispatch failed (fan-out limit exceeded / queue full)"
                                );
                            }
                        }
                        PortRef::Node(id) => {
                            let Some(from) = self.node_map.get(id).copied() else {
                                // Virtual keyboards are document-only.
                                continue;
                            };
                            let Some(to) = to_node else {
                                log::warn!("MIDI cable target node missing, skipping: {cable:?}");
                                continue;
                            };
                            if !self.engine.connect_event(
                                from,
                                to,
                                EventKind::Midi,
                                AutoDest::default(),
                                EngineMapSpec::UNIT,
                            ) {
                                log::warn!(
                                    "MIDI event cable ({from} → {to}) dispatch failed (cycle / limit exceeded / queue full)"
                                );
                            }
                        }
                        _ => {}
                    }
                }
                CableKind::Automation => {
                    // spec `midi-nodes.md` §7: node sources feed the event
                    // bus (control cables); timeline sources keep the
                    // automation-table route.
                    if let PortRef::Node(id) = &cable.from {
                        let Some(from) = self.node_map.get(id).copied() else {
                            continue;
                        };
                        if cable.to == PortRef::TimelineIn {
                            continue;
                        }
                        let Some(to) = to_node else {
                            log::warn!("Control cable target node missing, skipping: {cable:?}");
                            continue;
                        };
                        if !self.engine.connect_event(
                            from,
                            to,
                            EventKind::Control,
                            event_dest_of(&cable.param, &graph, &cable.to),
                            engine_map(cable_map(cable, &graph)),
                        ) {
                            log::warn!(
                                "Control cable ({from} → {to}) dispatch failed (cycle / queue full)"
                            );
                        }
                        continue;
                    }
                    let PortRef::AutoTrack(index) = &cable.from else {
                        continue;
                    };
                    // U3b: the conductor terminal has no engine node — the
                    // route is a sentinel slot, not a node lookup.
                    if cable.to == PortRef::TimelineIn {
                        if !self
                            .engine
                            .set_timeline_in_route(*index, engine_map(cable_map(cable, &graph)))
                        {
                            log::warn!("Tempo cable (track {index} → Timeline In) dispatch failed");
                        }
                        continue;
                    }
                    let Some(to) = to_node else {
                        log::warn!("Automation cable target node missing, skipping: {cable:?}");
                        continue;
                    };
                    // U4a: a CC cable re-establishes an event route, not a
                    // parameter route — undo / open must rebuild it the same
                    // way the picker created it.
                    if let Some((controller, channel)) = cable.param.cc_of() {
                        if !self.engine.set_auto_cc_route(
                            *index,
                            to,
                            channel,
                            controller,
                            engine_map(cable_map(cable, &graph)),
                        ) {
                            log::warn!("CC cable (track {index} → {to}) dispatch failed");
                        }
                        continue;
                    }
                    // P1: same shape for a bend — an event route, not a
                    // parameter route. Undo / open must rebuild it the same
                    // way the picker created it.
                    if let Some((channel, _)) = cable.param.pitch_bend_of() {
                        // The range is the instrument's (SF2), not the cable's
                        // copy — same reason as the axis itself (D12): an
                        // edited bend range must reach the existing route.
                        let range = cable.param.bend_range(&graph, &cable.to);
                        if !self.engine.set_auto_pitch_bend_route(
                            *index,
                            to,
                            channel,
                            range,
                            engine_map(cable_map(cable, &graph)),
                        ) {
                            log::warn!("Pitch-bend cable (track {index} → {to}) dispatch failed");
                        }
                        continue;
                    }
                    let Some(id) = param_id_of(&cable.param) else {
                        log::warn!("Automation cable carries a non-node parameter: {cable:?}");
                        continue;
                    };
                    if !self.engine.set_auto_route(
                        *index,
                        to,
                        id,
                        engine_map(cable_map(cable, &graph)),
                    ) {
                        log::warn!("Automation cable (track {index} → {to}) dispatch failed");
                    }
                }
            }
        }

        // 5) Active MIDI track count (D12).
        self.engine
            .set_midi_track_count(self.project().midi_tracks.len());

        // 5b) Audio clip tables (U6): the nodes were installed above with
        //     their initial tables; this push keeps undo/redo of clip edits
        //     and document reloads consistent through the same path.
        for track in 0..self.project().audio_tracks.len() {
            self.send_audio_clips_for_track(track);
        }

        // 6) Song loop range (U1): metadata lives outside the graph, so the
        //    full rebuild is also the undo/redo/open refresh path for it.
        self.push_loop_range();
    }

    /// Install a document node: build the processor through the shared
    /// headless builder (S0) -> engine `add_node` -> register the mapping,
    /// guard and the session-only caches that only the live graph needs.
    pub(crate) fn install_node(&mut self, node: &NodeDoc, sample_rate: u32) {
        // G6: the virtual keyboard is document-only (UI routing node) - it
        // never enters the engine graph, so it has no entry in `node_map`.
        if node.is_keyboard() {
            return;
        }
        let doc_id = node.node_id();
        let track = node.track();
        let is_effect = matches!(node, NodeDoc::Effect { .. });
        let is_clap = matches!(
            node,
            NodeDoc::Instrument {
                voice: VoiceSpec::Clap { .. },
                ..
            } | NodeDoc::Effect {
                voice: VoiceSpec::Clap { .. },
                ..
            }
        );
        let is_vst3 = matches!(
            node,
            NodeDoc::Instrument {
                voice: VoiceSpec::Vst3 { .. },
                ..
            } | NodeDoc::Effect {
                voice: VoiceSpec::Vst3 { .. },
                ..
            }
        );
        // R4: the live Input node reads the capture ring. Build it before
        // the media adapter reborrows the whole app.
        let input = match node {
            NodeDoc::Input { channel, gain, .. } => {
                match (self.engine.input_ring(), self.engine.input_info()) {
                    (Some(ring), Some(info)) => {
                        let mut processor =
                            InputProcessor::new(ring, info.channels, usize::from(*channel), *gain);
                        if let Some(stats) = self.engine.input_stats() {
                            processor = processor.with_stats(stats);
                        }
                        Some(processor)
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let mut media = headless::AppMedia(&mut *self);
        let spawned = headless::spawn_node(
            node,
            sample_rate,
            &mut media,
            headless::SpawnPolicy::Live,
            input,
        );
        let headless::Spawned {
            processor,
            guard,
            info,
        } = spawned;
        let engine_id = self.engine.fresh_node_id();
        // D15: the mixer is the only multi-input node; P6 adds the feedback
        // tap kind, which the compiler orders before its own input sources.
        let kind = match node {
            NodeDoc::Mixer { .. } => NodeKind::Mixer,
            NodeDoc::Feedback { .. } => NodeKind::Feedback,
            // Z0: the document's port profile decides the runtime kind.
            _ if node.audio_inputs() == 2 => NodeKind::Dual,
            _ => NodeKind::Plain,
        };
        if !self.engine.add_node(engine_id, kind, processor) {
            drop(guard); // never entered the graph: unload immediately
            log::error!("Document node {doc_id} installation failed (slots full / queue full)");
            return;
        }
        self.node_map.insert(doc_id, engine_id);
        if let Some(meter) = info.meter {
            self.effect_meters.insert(doc_id, meter);
        }
        if let Some(state) = info.default_state {
            self.voice_default_state.insert(doc_id, state);
        }
        // G4/G5: parameter caches are keyed by document node id, split by
        // voice vs effect. A plugin that failed to spawn clears its caches
        // (the installer falls back to a built-in processor).
        if let Some((params, values)) = info.clap {
            if is_effect {
                self.clap_effect_params.insert(doc_id, params);
                self.clap_effect_values.insert(doc_id, values);
            } else {
                self.clap_voice_params.insert(doc_id, params);
                self.clap_voice_values.insert(doc_id, values);
            }
        } else if is_clap {
            if is_effect {
                self.clap_effect_params.remove(&doc_id);
                self.clap_effect_values.remove(&doc_id);
            } else {
                self.clap_voice_params.remove(&doc_id);
                self.clap_voice_values.remove(&doc_id);
            }
        }
        if let Some((params, values)) = info.vst3 {
            if is_effect {
                self.vst3_effect_params.insert(doc_id, params);
                self.vst3_effect_values.insert(doc_id, values);
            } else {
                self.voice_params.insert(doc_id, params);
                self.vst3_voice_values.insert(doc_id, values);
            }
        } else if is_vst3 {
            if is_effect {
                self.vst3_effect_params.remove(&doc_id);
                self.vst3_effect_values.remove(&doc_id);
            } else {
                self.voice_params.remove(&doc_id);
                self.vst3_voice_values.remove(&doc_id);
            }
        }
        if let Some(guard) = guard {
            self.voice_guards.push(PluginGuardEntry {
                node_id: engine_id,
                track,
                plugin_id: info.label,
                _guard: guard,
            });
        }
        if let NodeDoc::AudioTrack { track, .. } = node {
            // S0: the shared builder installs an empty table. Direct
            // single-node installs (patch / mixer) do not run
            // `apply_document`'s step 5b, so push the real table here.
            self.send_audio_clips_for_track(*track);
        }
    }

    /// Re-assemble one track's engine addressing table (manual params/level/keyboard
    /// addressing) from the document roles + `node_map`. Called after document rebuild and
    /// whenever a node filling a track role is added or removed.
    pub(crate) fn register_track_chain(&mut self, track: usize) {
        let resolve = |doc_id: Option<u64>| -> Option<u64> {
            doc_id.and_then(|id| self.node_map.get(&id).copied())
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
        // P5f: no per-track strip anymore — the engine's per-track "channel"
        // slot is the metering point (the chain end = the built-in delay);
        // fader state lives on mixer channels addressed via node params.
        if !self.engine.register_track_chain(track, voice, delay, delay) {
            log::warn!(
                "Track {} addressing table assembly failed (missing node or queue full)",
                track + 1
            );
        }
    }

    /// The live guard owning `engine_id` (active zone only; retiring
    /// guards are on their way out and are never reopened).
    pub(crate) fn guard_by_engine_id(&mut self, engine_id: u64) -> Option<&mut PluginGuard> {
        self.voice_guards
            .iter_mut()
            .find(|e| e.node_id == engine_id)
            .map(|e| &mut e._guard)
    }

    /// Move a node's active guard into the retiring zone (called when a node is swapped out or
    /// removed; retiring guards are released once recycling receipts arrive).
    pub(crate) fn retire_guard(&mut self, engine_id: u64) {
        // An embedded editor attached to this node closes first (spec
        // `plugin-gui.md` §4.3): plugin editor down before the guard moves
        // on — an editor over a retired node would draw into a stale graph.
        if self
            .plugin_editor
            .as_ref()
            .is_some_and(|e| e.node_id == engine_id)
        {
            self.close_plugin_editor();
        }
        let mut i = 0;
        while i < self.voice_guards.len() {
            if self.voice_guards[i].node_id == engine_id {
                let entry = self.voice_guards.remove(i);
                self.retiring_guards.push(entry);
            } else {
                i += 1;
            }
        }
    }

    /// Document node_id → engine node_id.
    pub(crate) fn engine_id(&self, doc_id: u64) -> Option<u64> {
        self.node_map.get(&doc_id).copied()
    }

    /// Push the full set of project document parameters (commands dropped on queue full are safe
    /// to ignore).
    pub(crate) fn push_all_params(&mut self) {
        self.apply_document();
        self.engine.send(EngineCommand::SetParam {
            id: ParamId::MasterGain,
            value: self.project().master_gain,
        });
        self.send_sequencer(SequencerCommand::SetBpm(convert::to_f32(
            self.project().metadata.tempo_bpm,
        )));
        self.push_time_signature();
        for track in 0..self.project().midi_tracks.len() {
            self.send_track_clips(track);
        }
        self.push_node_params();
        self.push_mixer_params();
        for index in 0..self.project().control_tracks.len() {
            self.send_control_clips(index);
        }
    }

    /// Push every document node's built-in parameters to the engine (G4:
    /// node-addressed `SetNodeParam`, so multi-instrument tracks each get
    /// their own timbre instead of only the track-table first match).
    /// Fader state lives on mixer channels (pushed by
    /// [`Self::push_mixer_params`]).
    pub(crate) fn push_node_params(&mut self) {
        let nodes = self.project().graph.nodes.clone();
        for node in &nodes {
            let Some(&engine_id) = self.node_map.get(&node.node_id()) else {
                continue;
            };
            let params: Vec<(ParamId, f32)> = match node {
                NodeDoc::Instrument { voice, synth, .. } => match voice {
                    VoiceSpec::Builtin => synth_param_values(synth).to_vec(),
                    // U13: SoundFont preset / trim / bend range are parameters.
                    VoiceSpec::SoundFont {
                        bank,
                        program,
                        gain,
                        bend_range,
                        ..
                    } => vec![
                        (ParamId::Sf2BankSelect, f32::from(*bank)),
                        (ParamId::Sf2ProgramChange, f32::from(*program)),
                        (ParamId::Sf2Gain, *gain),
                        (ParamId::Sf2BendRange, *bend_range),
                    ],
                    VoiceSpec::Sample {
                        root_note,
                        play_mode,
                        gain,
                        region,
                        ..
                    } => vec![
                        (ParamId::SampleRootNote, f32::from(*root_note)),
                        (
                            ParamId::SamplePlayMode,
                            if matches!(play_mode, SamplePlayMode::OneShot) {
                                1.0
                            } else {
                                0.0
                            },
                        ),
                        (ParamId::SampleGain, *gain),
                        // D12: the play window rides the same push, so a
                        // trim edit, an undo and a fresh node all land on the
                        // processor through one path.
                        (ParamId::SampleStart, region.start as f32),
                        (ParamId::SampleLoopStart, region.loop_start as f32),
                        (ParamId::SampleLoopEnd, region.loop_end as f32),
                        (ParamId::SampleEnd, region.end as f32),
                        (ParamId::SampleLoopMode, f32::from(region.loop_mode.index())),
                    ],
                    VoiceSpec::DrumKit { slots, gain } => {
                        let mut values = vec![(ParamId::DrumGain, *gain)];
                        for slot in slots {
                            values.push((ParamId::DrumSlotGain(slot.note), slot.gain));
                            values.push((ParamId::DrumSlotPan(slot.note), slot.pan));
                            values.push((ParamId::DrumSlotTune(slot.note), slot.tune_semitones));
                            values.push((
                                ParamId::DrumSlotChoke(slot.note),
                                slot.choke.map_or(-1.0, f32::from),
                            ));
                        }
                        values
                    }
                    // Plugin parameters live in the host cache and are pushed
                    // by the parameter UI itself; undo never changes them.
                    VoiceSpec::Clap { .. } | VoiceSpec::Vst3 { .. } => Vec::new(),
                    // B1: the modal macros ride the same replay so a fresh
                    // node, an undo and a reload all land on one path.
                    VoiceSpec::Modal { params } => crate::modal_param_values(params).to_vec(),
                    VoiceSpec::Pluck { params } => crate::pluck_param_values(params).to_vec(),
                    VoiceSpec::Organ { params } => crate::organ_param_values(params).to_vec(),
                    VoiceSpec::Wavetable { params } => {
                        crate::wavetable_param_values(params).to_vec()
                    }
                },
                // Table-driven (spec `effects.md` D6): one entry per slot.
                NodeDoc::Effect {
                    voice: VoiceSpec::Builtin | VoiceSpec::SoundFont { .. },
                    params,
                    ..
                } => crate::effect::descriptor(params)
                    .params
                    .iter()
                    .enumerate()
                    .map(|(slot, _)| {
                        (
                            ParamId::BuiltinParam(slot as u8),
                            crate::effect::value(params, slot as u8),
                        )
                    })
                    .collect::<Vec<_>>(),
                NodeDoc::Arpeggiator { params, .. } => vec![
                    (ParamId::ArpDivision, f32::from(params.division)),
                    (ParamId::ArpGate, params.gate),
                    (ParamId::ArpMode, params.mode.index() as f32),
                    (ParamId::ArpOctaves, f32::from(params.octaves)),
                    (ParamId::ArpLatch, if params.latch { 1.0 } else { 0.0 }),
                    (ParamId::ArpHarmony, params.harmony.index() as f32),
                    (ParamId::ArpSkip, params.skip),
                ],
                NodeDoc::Portamento { params, .. } => vec![
                    (ParamId::PortTime, params.time_ms),
                    (ParamId::PortMin, params.min),
                    (ParamId::PortMax, params.max),
                    (
                        ParamId::PortLegato,
                        if params.legato_only { 1.0 } else { 0.0 },
                    ),
                ],
                NodeDoc::MidiEnvelope { params, .. } => vec![
                    (ParamId::EnvAttack, params.attack),
                    (ParamId::EnvDecay, params.decay),
                    (ParamId::EnvSustain, params.sustain),
                    (ParamId::EnvRelease, params.release),
                    (ParamId::EnvMin, params.min),
                    (ParamId::EnvMax, params.max),
                ],
                NodeDoc::Lfo { params, .. } => vec![
                    (ParamId::LfoWaveform, params.waveform.index() as f32),
                    (ParamId::LfoRate, params.rate_hz),
                    (ParamId::LfoSync, if params.sync { 1.0 } else { 0.0 }),
                    (ParamId::LfoDivision, f32::from(params.division)),
                    (ParamId::LfoDepth, params.depth),
                ],
                NodeDoc::CurveMap { params, .. } => vec![
                    (ParamId::CurveMapMin, params.min),
                    (ParamId::CurveMapMax, params.max),
                    (
                        ParamId::CurveMapMode,
                        curve_mode_value(engine_mode(params.mode)),
                    ),
                ],
                NodeDoc::Feedback { gain, .. } => vec![(ParamId::FeedbackGain, *gain)],
                // R4: the live input node's channel + trim.
                NodeDoc::Input { channel, gain, .. } => vec![
                    (ParamId::InputChannel, f32::from(*channel)),
                    (ParamId::InputGain, *gain),
                ],
                _ => continue,
            };
            for (id, value) in params {
                self.engine.send(EngineCommand::SetNodeParam {
                    node_id: engine_id,
                    id,
                    value,
                });
            }
        }
    }

    /// Push every mixer node's console state to the engine (P5f D15/D20:
    /// fader moves ride the node param path, `ParamId::MixerGain(ch)` etc.).
    pub(crate) fn push_mixer_params(&mut self) {
        let mixers: Vec<NodeDoc> = self
            .project()
            .graph
            .nodes
            .iter()
            .filter(|n| n.is_mixer())
            .cloned()
            .collect();
        for mixer in mixers {
            let NodeDoc::Mixer {
                node_id,
                channels,
                out_pan,
                out_muted,
                ..
            } = mixer
            else {
                continue;
            };
            let Some(&engine_id) = self.node_map.get(&node_id) else {
                continue;
            };
            for (i, ch) in channels.iter().enumerate() {
                let i = i as u8;
                for (id, value) in [
                    (ParamId::MixerGain(i), ch.gain),
                    (ParamId::MixerPan(i), ch.pan),
                    (ParamId::MixerMute(i), if ch.muted { 1.0 } else { 0.0 }),
                    (ParamId::MixerSolo(i), if ch.solo { 1.0 } else { 0.0 }),
                ] {
                    self.engine.send(EngineCommand::SetNodeParam {
                        node_id: engine_id,
                        id,
                        value,
                    });
                }
            }
            for (id, value) in [
                (ParamId::MixerOutPan, out_pan),
                (ParamId::MixerOutMute, if out_muted { 1.0 } else { 0.0 }),
            ] {
                self.engine.send(EngineCommand::SetNodeParam {
                    node_id: engine_id,
                    id,
                    value,
                });
            }
        }
    }

    /// Built-in synth timbre parameters of an instrument node.
    ///
    /// `None` for every other voice: the timbre fields exist on the document
    /// for forward compatibility, but nothing renders them (see
    /// [`voice_has_synth_params`]), so reading them out would hand callers a
    /// struct that has no effect on the sound.
    pub(crate) fn synth_of_node(&self, doc_id: u64) -> Option<SynthParams> {
        match self.project().graph.node_by_id(doc_id)? {
            NodeDoc::Instrument { voice, synth, .. } if voice_has_synth_params(voice) => {
                Some(*synth)
            }
            _ => None,
        }
    }

    /// Built-in synth timbre parameters of an instrument node (mutable).
    ///
    /// Same rule as [`Self::synth_of_node`]: writing these fields on a sample
    /// player / drum kit / SoundFont / plugin node would be a silent no-op, so
    /// the accessor refuses the write instead.
    pub(crate) fn synth_mut_node(&mut self, doc_id: u64) -> Option<&mut SynthParams> {
        match self.project_mut().graph.node_by_id_mut(doc_id)? {
            NodeDoc::Instrument { voice, synth, .. } if voice_has_synth_params(voice) => {
                Some(synth)
            }
            _ => None,
        }
    }

    /// Built-in effect payload of an effect node (ignored for plugins).
    pub(crate) fn effect_params_of_node(&self, doc_id: u64) -> Option<EffectParams> {
        match self.project().graph.node_by_id(doc_id)? {
            NodeDoc::Effect { params, .. } => Some(params.clone()),
            _ => None,
        }
    }

    /// Built-in effect payload of an effect node (mutable).
    pub(crate) fn effect_params_mut_node(&mut self, doc_id: u64) -> Option<&mut EffectParams> {
        match self.project_mut().graph.node_by_id_mut(doc_id)? {
            NodeDoc::Effect { params, .. } => Some(params),
            _ => None,
        }
    }

    /// Track → mixer channel mapping (P5f D17/D18): walk the track's audio
    /// chain from the built-in delay downstream until a cable lands on a
    /// mixer node; returns `(mixer doc node_id, channel index)`. `None` when
    /// the track is not routed into any mixer.
    pub(crate) fn track_channel_target(&self, track: usize) -> Option<(u64, u8)> {
        let mut cur = self
            .project()
            .graph
            .builtin_delay_of_track(track)
            .map(NodeDoc::node_id)?;
        for _ in 0..MAX_NODES {
            let cable = self
                .project()
                .graph
                .cables
                .iter()
                .find(|c| c.kind == CableKind::Audio && c.from == PortRef::Node(cur))?;
            let PortRef::Node(next) = cable.to else {
                return None; // routed straight to master / timeline — no mixer
            };
            if self
                .project()
                .graph
                .node_by_id(next)
                .is_some_and(|n| n.is_mixer())
            {
                return Some((next, cable.in_port));
            }
            cur = next;
        }
        None
    }

    // ---- Automation track dispatch (D7/D12: curve and cable endpoint are separated) ----

    /// Push the clip table of one control track (its delivery endpoints are decided by the graph's
    /// automation cable table, set via `apply_document` / `set_auto_endpoint`).
    pub(crate) fn send_control_clips(&mut self, index: usize) {
        let clips = headless::control_clip_table(self.project(), index);
        // D15 revision: control tables ride the structure queue (they are
        // ~17 KB at 32 clips — too big for the command queue).
        if !self.engine.set_control_clips(index, clips) {
            log::warn!("structure queue full: control clips of track {index} not updated");
        }
    }

    /// Redirect an automation track's cable endpoint (the document cable stores the **document**
    /// node_id; engine routing maps to the engine-side id via node_map).
    pub(crate) fn set_auto_endpoint(&mut self, index: usize, target: AutoTarget, param: AutoParam) {
        let before = self.begin_edit();
        self.project_mut().graph.remove_auto_cable(index);
        let cable = match target {
            AutoTarget::Node(doc_id) => {
                glitcherz_model::CableDoc::automation(index, doc_id, param.clone())
            }
            // U3b: the terminal's single port fixes the parameter to Tempo.
            AutoTarget::TimelineIn => glitcherz_model::CableDoc::tempo(index),
        };
        // The axis travels with the cable (spec `curve-map.md` D2/D4), but it is
        // *derived*, never copied: picking a target makes the normalized curve
        // read through that target's range (D1 keeps the shape), and a later
        // range change moves it along.
        let map = cable_map(&cable, &self.project().graph);
        self.project_mut().graph.cables.push(cable);
        self.commit_edit("Route Automation", before);
        match target {
            AutoTarget::TimelineIn => {
                self.engine.set_timeline_in_route(index, engine_map(map));
            }
            AutoTarget::Node(doc_id) => {
                let bend_range = param.bend_range(&self.project().graph, &PortRef::Node(doc_id));
                if let Some(engine_id) = self.engine_id(doc_id) {
                    match param.cc_of() {
                        // U4a: an event stream, not a parameter — the route
                        // carries the controller/channel instead of a ParamId.
                        Some((controller, channel)) => {
                            self.engine.set_auto_cc_route(
                                index,
                                engine_id,
                                channel,
                                controller,
                                engine_map(map),
                            );
                        }
                        None => {
                            // P1: a bend is the other event-stream destination.
                            if let Some((channel, _)) = param.pitch_bend_of() {
                                self.engine.set_auto_pitch_bend_route(
                                    index,
                                    engine_id,
                                    channel,
                                    bend_range,
                                    engine_map(map),
                                );
                            } else if let Some(id) = param_id_of(&param) {
                                self.engine
                                    .set_auto_route(index, engine_id, id, engine_map(map));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Re-derive every automation cable that ends on `doc_id` (spec
    /// `curve-map.md` D12).
    ///
    /// A parameter's *range* can change without the cable moving, and SF2's
    /// bend range is the case that matters: it scales the whole curve, so a
    /// stale axis would squeeze every bend into a fraction of the wire.
    pub(crate) fn refresh_maps_for(&mut self, doc_id: u64) {
        let Some(engine_id) = self.engine_id(doc_id) else {
            return;
        };
        let graph = self.project().graph.clone();
        for cable in &graph.cables {
            let PortRef::AutoTrack(index) = &cable.from else {
                continue;
            };
            if cable.to != PortRef::Node(doc_id) {
                continue;
            }
            let map = engine_map(cable_map(cable, &graph));
            if let Some((channel, _)) = cable.param.pitch_bend_of() {
                let range = cable.param.bend_range(&graph, &cable.to);
                let _ = self
                    .engine
                    .set_auto_pitch_bend_route(*index, engine_id, channel, range, map);
            } else if let Some((controller, channel)) = cable.param.cc_of() {
                let _ = self
                    .engine
                    .set_auto_cc_route(*index, engine_id, channel, controller, map);
            } else if let Some(id) = param_id_of(&cable.param) {
                let _ = self.engine.set_auto_route(*index, engine_id, id, map);
            }
        }
    }

    /// Document clip table → engine representation (D15 revision): the note and
    /// controller vectors are built here, on the control thread, where
    /// allocation is allowed; the audio thread only reads them. **Nothing is
    /// truncated** — a clip holds as many notes as the document does.
    pub(crate) fn rt_clips(&self, track: usize) -> MidiClipTable {
        headless::midi_clip_table(self.project(), track)
    }

    /// Push the current clip table of one track (D15 batch).
    pub(crate) fn send_track_clips(&mut self, track: usize) {
        let clips = self.rt_clips(track);
        // D15 revision: clip tables ride the structure queue (they own `Vec`s);
        // a full queue drops the update, and the next edit re-sends it.
        if !self.engine.send_midi_clips(track, clips) {
            log::warn!("structure queue full: MIDI clips of track {track} not updated");
        }
    }

    /// Pushes the document to the engine after an undo/redo, a history jump or a
    /// command (S2-5).
    ///
    /// Only a graph-changing edit (node payload, cable, track count) needs the
    /// full rebuild. When the topology is identical, clips / loop / transport
    /// fields are pushed directly, leaving every instantiated plug-in alive —
    /// a note or curve edit no longer re-spawns the world.
    pub(crate) fn sync_engine(&mut self, before: &Project) {
        if before.engine_topology_eq(self.project()) {
            self.refresh_clips_and_transport();
        } else {
            self.apply_document();
        }
    }

    /// The cheap half of [`Self::sync_engine`]: everything that is not the
    /// graph itself. Must stay aligned with `apply_document`'s non-topology
    /// pushes (master gain / tempo / meter / clip tables / loop / track count).
    pub(crate) fn refresh_clips_and_transport(&mut self) {
        self.engine.send(EngineCommand::SetParam {
            id: ParamId::MasterGain,
            value: self.project().master_gain,
        });
        self.send_sequencer(SequencerCommand::SetBpm(convert::to_f32(
            self.project().metadata.tempo_bpm,
        )));
        self.push_time_signature();
        // S2-5b: parameter-only edits keep the processors, so every value the
        // rebuild would have set at construction must be replayed here.
        self.push_node_params();
        self.push_mixer_params();
        for track in 0..self.project().midi_tracks.len() {
            self.send_track_clips(track);
        }
        for index in 0..self.project().control_tracks.len() {
            self.send_control_clips(index);
        }
        for track in 0..self.project().audio_tracks.len() {
            self.send_audio_clips_for_track(track);
        }
        self.engine
            .set_midi_track_count(self.project().midi_tracks.len());
        self.push_loop_range();
    }
}
