//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    // ---- MIDI recording (spec `recording.md` R2) ----

    /// Per-frame recording housekeeping: takes whatever the capture tap
    /// produced, feeds the state machine, and retries a capture toggle the
    /// command queue refused.
    pub(crate) fn poll_recording(&mut self) {
        if let Some(desired) = self.capture_pending
            && self.engine.set_capture(desired)
        {
            self.capture_pending = None;
        }
        self.drain_capture();
        // R8: the control ring is separate; drain it every frame so a curve
        // take never starts with stale samples.
        let mut control_batch = std::mem::take(&mut self.control_scratch);
        self.engine.drain_captured_controls(&mut control_batch);
        self.take_points = self.take_points.saturating_add(control_batch.len() as u64);
        for captured in control_batch.drain(..) {
            self.control_recorder.push(captured);
        }
        self.control_scratch = control_batch;
        // R4: move the take ring's samples into the in-flight take every frame
        // (a bounded queue must be drained continuously or it drops).
        if self.audio_take.is_some() {
            let mut scratch = std::mem::take(&mut self.audio_scratch);
            let moved = self.engine.drain_recorded_audio(&mut scratch);
            self.take_samples = self.take_samples.saturating_add(moved as u64);
            if let Some(take) = &mut self.audio_take {
                take.samples.append(&mut scratch);
            }
            scratch.clear();
            self.audio_scratch = scratch;
        }
        // R5: an input overflow is user-visible (the take is missing samples),
        // not a silent loss. Report once per burst like the capture counter.
        if let Some(stats) = self.engine.input_stats() {
            let dropped = stats.dropped();
            if dropped > self.input_dropped_seen {
                let lost = dropped - self.input_dropped_seen;
                self.input_dropped_seen = dropped;
                self.set_message(format!(
                    "Audio input dropped {lost} samples (monitor/record ring full)"
                ));
            }
        }
        // The transport position only makes sense as a clock while it runs:
        // a stopped (or auto-stopped) transport reads 0, which would look like
        // a loop wrap and wipe the take.
        if self.engine.is_playing() {
            let position = f64::from(self.engine.musical_ticks().max(0.0));
            self.recorder.observe_position(position);
            self.control_recorder.observe_position(position);
        }
        self.reconcile_take_tracks();
    }

    /// R6 D33: removing a track's recording-input cable mid-take ends **that**
    /// take on this frame (no half-take left behind); the other armed tracks
    /// keep recording. Adding a cable mid-take does not arm it — a take's set
    /// of tracks is fixed at Rec.
    pub(crate) fn reconcile_take_tracks(&mut self) {
        if !self.recorder.is_active() {
            return;
        }
        let current: std::collections::BTreeSet<usize> = self
            .project()
            .graph
            .track_input_tracks(TrackKind::Midi)
            .into_iter()
            .collect();
        let removed: Vec<usize> = self
            .recorder
            .tracks()
            .iter()
            .copied()
            .filter(|track| !current.contains(track))
            .collect();
        if removed.is_empty() {
            return;
        }
        let end =
            f64::from(self.engine.musical_ticks().max(0.0)).max(self.recorder.last_position());
        let mut takes = Vec::new();
        for track in removed {
            if let Some(notes) = self.recorder.finish_track(track, end) {
                takes.push((track, notes));
            }
        }
        let label = format!("Record MIDI (input removed: {} track(s))", takes.len());
        let written = self.commit_take_clips(&label, &takes);
        if !self.recorder.is_active() {
            self.engine.set_capture(false);
            self.capture_pending = None;
            self.engine.set_record_mute(0);
        }
        match written {
            Some((tracks, _)) => self.set_message(format!(
                "Recording input removed: take closed on {tracks} track(s)"
            )),
            None => self.set_message("Recording input removed: closed an empty take".to_string()),
        }
    }

    /// Tracks whose recording-input cable names a source type the engine can
    /// actually capture today (R6 D26). Returns `(armed, unsupported)` in
    /// track order; `unsupported` carries one human-readable reason per wired
    /// port, so a rejected Rec press can say exactly what is missing.
    pub(crate) fn armed_recording_tracks(
        &self,
    ) -> (Vec<usize>, Vec<usize>, Vec<usize>, Vec<String>) {
        let doc = &self.project().graph;
        let mut midi = Vec::new();
        let mut audio = Vec::new();
        let mut curve = Vec::new();
        let mut unsupported = Vec::new();
        for (kind, tracks) in [
            (TrackKind::Midi, doc.track_input_tracks(TrackKind::Midi)),
            (TrackKind::Curve, doc.track_input_tracks(TrackKind::Curve)),
            (TrackKind::Audio, doc.track_input_tracks(TrackKind::Audio)),
        ] {
            for track in tracks {
                let source = doc.track_input_source(kind, track);
                match (kind, source.map(|c| &c.from)) {
                    // R6: a device / computer keyboard records through the
                    // command tap; R7 bakes a MIDI effect's *output* at the
                    // event-dispatch tap.
                    (TrackKind::Midi, Some(PortRef::Node(id)))
                        if doc
                            .node_by_id(*id)
                            .is_some_and(|node| node.is_keyboard() || node.has_midi_out()) =>
                    {
                        midi.push(track);
                    }
                    (TrackKind::Midi, Some(PortRef::Node(_))) => unsupported.push(format!(
                        "Track {}: the MIDI input needs a keyboard or MIDI-effect source",
                        track + 1
                    )),
                    (TrackKind::Midi, _) => unsupported.push(format!(
                        "Track {}: the MIDI input needs a keyboard source",
                        track + 1
                    )),
                    // R8: a control-output node's block value is baked into
                    // the curve track. Timeline-curve outlets are not sampled
                    // yet (their evaluation happens outside the node bus).
                    (TrackKind::Curve, Some(PortRef::Node(id)))
                        if doc.node_by_id(*id).is_some_and(NodeDoc::has_control_out) =>
                    {
                        curve.push(track);
                    }
                    (TrackKind::Curve, Some(PortRef::AutoTrack(_))) => unsupported.push(format!(
                        "Track {}: curve-to-curve baking is not implemented yet",
                        track + 1
                    )),
                    (TrackKind::Curve, _) => unsupported.push(format!(
                        "Track {}: the curve input needs a control source (LFO / envelope / portamento / map)",
                        track + 1
                    )),
                    // R4: a live input node is the only audio capture source.
                    // Instruments / mixers are allowed to be wired (D26) but
                    // their post-buffer tap is not implemented.
                    (TrackKind::Audio, Some(PortRef::Node(id)))
                        if doc
                            .node_by_id(*id)
                            .is_some_and(|node| matches!(node, NodeDoc::Input { .. })) =>
                    {
                        audio.push(track);
                    }
                    (TrackKind::Audio, _) => unsupported.push(format!(
                        "Track {}: audio recording needs a live input source (Input node)",
                        track + 1
                    )),
                }
            }
        }
        midi.sort_unstable();
        midi.dedup();
        audio.sort_unstable();
        audio.dedup();
        curve.sort_unstable();
        curve.dedup();
        (midi, audio, curve, unsupported)
    }

    /// Whether any take (MIDI, audio or curve) is running.
    pub(crate) fn is_recording(&self) -> bool {
        self.recorder.is_active() || self.audio_take.is_some() || self.control_recorder.is_active()
    }

    /// Moves everything the capture tap has queued into the recorder.
    pub(crate) fn drain_capture(&mut self) {
        // The scratch buffer is taken out so the engine drain (which needs
        // `&mut self.engine`) and the recorder (which needs `&mut self`) never
        // borrow `self` at the same time.
        let mut batch = std::mem::take(&mut self.capture_scratch);
        self.engine.drain_captured(&mut batch);
        self.take_events = self.take_events.saturating_add(batch.len() as u64);
        let overflow = self.engine.capture_overflow();
        if overflow > self.capture_overflow_seen {
            let lost = overflow - self.capture_overflow_seen;
            self.capture_overflow_seen = overflow;
            self.set_message(format!(
                "Recording dropped {lost} input events (UI fell behind)"
            ));
        }
        for captured in batch.drain(..) {
            self.recorder.push(captured);
        }
        self.capture_scratch = batch;
    }

    /// Reports a `⏺ Rec` problem **loudly**: a refused press (D22 wiring, R4
    /// device, D27 dry region) and a take that captured nothing both leave the
    /// user staring at a button that "did nothing". A status-line notice alone
    /// is too easy to miss, so the reason also becomes a card. Logged too — the
    /// log outlives the card.
    pub(crate) fn recording_problem(&mut self, reason: String) {
        log::warn!("Recording: {reason}");
        self.status_message = Some((reason.clone(), Instant::now(), MessageKind::Error));
        self.record_refusal = Some((reason, Instant::now()));
    }

    /// One-line summary of the armed recording inputs (`T1 A2`), or `None`
    /// when nothing is wired into any `Timeline In` port.
    pub(crate) fn armed_recording_chip(&self) -> Option<String> {
        let (midi, audio, curve, _) = self.armed_recording_tracks();
        if midi.is_empty() && audio.is_empty() && curve.is_empty() {
            return None;
        }
        let mut names = Vec::new();
        names.extend(midi.iter().map(|&track| format!("T{}", track + 1)));
        names.extend(audio.iter().map(|&track| format!("A{}", track + 1)));
        names.extend(curve.iter().map(|&track| format!("C{}", track + 1)));
        Some(names.join(" "))
    }

    /// Floating card with the last refusal reason, dismissed by its close
    /// button or after [`RECORD_REFUSAL_TTL`].
    pub(crate) fn recording_refusal_card(&mut self, ui: &mut egui::Ui) {
        let expired = self
            .record_refusal
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= RECORD_REFUSAL_TTL);
        if expired {
            self.record_refusal = None;
        }
        let Some((reason, _)) = self.record_refusal.clone() else {
            return;
        };
        let mut open = true;
        egui::Window::new("Recording problem")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 72.0])
            .show(ui.ctx(), |ui| {
                ui.set_max_width(420.0);
                ui.colored_label(
                    egui::Color32::from_rgb(255, 140, 120),
                    egui::RichText::new(&reason).strong(),
                );
                ui.add_space(4.0);
                if reason.to_lowercase().contains("timeline in") {
                    ui.weak(
                        "Timeline In is the recording input: open the Patch tab and drag a source \
                         into it.",
                    );
                } else {
                    ui.weak("Fix the cause above, then press ⏺ again.");
                }
            });
        if !open {
            self.record_refusal = None;
        }
        ui.ctx().request_repaint();
    }

    /// The picker state as an engine source.
    pub(crate) fn record_source(&self) -> RecordSource {
        match (&self.audio_device, self.audio_loopback) {
            (Some(name), true) => RecordSource::Loopback(name.clone()),
            (Some(name), false) => RecordSource::Capture(name.clone()),
            (None, _) => RecordSource::Default,
        }
    }

    /// Opens `source` and rebuilds the graph so the `Input` node latches onto
    /// the new stream's ring.
    pub(crate) fn pick_audio_source(&mut self, source: RecordSource) {
        if self.is_recording() {
            self.recording_problem(
                "End the take before switching the recording source".to_string(),
            );
            return;
        }
        match self.engine.reopen_input(&source) {
            Ok(()) => {
                self.audio_device = source.name().map(str::to_string);
                self.audio_loopback = source.is_loopback();
                self.audio_sources = AudioEngine::record_sources();
                // The Input node holds a clone of the *old* ring; a rebuild is
                // what makes the monitor path follow the new stream.
                self.apply_document();
                let described = self.engine.input_info().map(|info| {
                    format!(
                        "{}{} ({} ch @ {} Hz)",
                        if info.loopback { "↩ " } else { "" },
                        info.device_name,
                        info.channels,
                        info.sample_rate
                    )
                });
                self.set_message(format!(
                    "Recording source: {}",
                    described.unwrap_or_else(|| "system default".to_string())
                ));
            }
            Err(err) => {
                self.recording_problem(format!("Cannot open the recording source: {err}"));
            }
        }
    }

    /// Starts a take into every track armed by the `Timeline In` wiring (R6 D22).
    ///
    /// Recording starts on a bar line: the transport jumps back by the
    /// pre-roll, plays that many bars as a count-in (material and click are
    /// audible, nothing is captured), then capture begins at the bar line
    /// itself. At the very start of the song the pre-roll cannot go before
    /// tick 0, so the take simply begins after the first bar of pre-roll.
    pub(crate) fn start_recording(&mut self) {
        if self.is_recording() {
            return;
        }
        let (midi_armed, audio_armed, curve_armed, unsupported) = self.armed_recording_tracks();
        log::info!(
            "Record pressed: midi={midi_armed:?} audio={audio_armed:?} curve={curve_armed:?} \
             unsupported={} position={:.1}",
            unsupported.len(),
            self.engine.musical_ticks()
        );

        if midi_armed.is_empty() && audio_armed.is_empty() && curve_armed.is_empty() {
            if let Some(reason) = unsupported.first() {
                self.recording_problem(format!("Rec refused: {reason}"));
            } else {
                self.recording_problem(
                    "No recording input is armed. Open the Patch tab and drag a source into \
                     Timeline In (MIDI out → T1, Input node → A1)."
                        .to_string(),
                );
            }
            return;
        }
        // R4: audio needs a live input stream; refuse up front rather than
        // writing an empty take.
        if !audio_armed.is_empty() && self.engine.input_info().is_none() {
            // The engine knows *why* (missing device / unsupported rate); a
            // generic "no input device" sends the user hunting in the OS.
            let detail = self.engine.input_error().map_or_else(
                || ": pick a device next to In, or check the microphone".to_string(),
                |err| format!(": {err}"),
            );
            self.recording_problem(format!("Cannot record audio{detail}"));
            return;
        }
        if !audio_armed.is_empty() && self.project().base_dir.is_none() {
            self.recording_problem(
                "Cannot record audio: save the project first (takes live in <stem>.media/)"
                    .to_string(),
            );
            return;
        }
        let bar = f64::from(self.project().ticks_per_bar().max(1));
        let now = f64::from(self.engine.musical_ticks().max(0.0));
        // Forward to the next bar line (a position already on one stays).
        let start = (now / bar).ceil() * bar;
        let pre_roll = f64::from(self.record_pre_roll_bars) * bar;
        let from = (start - pre_roll).max(0.0);
        let record_start = from + pre_roll;
        // R6 D27 (dry region): nothing may already occupy the take's span on
        // any armed track. The check is an interval overlap, not "track is
        // empty", so recording after existing material keeps working.
        let bar_ticks = self.project().ticks_per_bar().max(1);
        let record_start_tick = record_start.round().max(0.0) as u64;
        for &track in &midi_armed {
            let Some(existing) = self.project().midi_tracks.get(track) else {
                continue;
            };
            if let Some(clip) = existing.clips.iter().find(|clip| {
                u64::from(clip.start_ticks) + u64::from(clip.span_ticks) > record_start_tick
            }) {
                let bar_no = clip.start_ticks / bar_ticks + 1;
                self.recording_problem(format!(
                    "Track {} already has a clip at bar {bar_no}. Move the transport past it \
                     (or delete that clip), then press Rec again.",
                    track + 1
                ));
                return;
            }
        }
        {
            let project = self.project();
            for &track in &audio_armed {
                let Some(existing) = project.audio_tracks.get(track) else {
                    continue;
                };
                let clash = existing.clips.iter().find(|clip| {
                    let span = u64::from(project.audio_clip_span_ticks(clip));
                    u64::from(clip.start_ticks) + span > record_start_tick
                });
                if let Some(clip) = clash {
                    let bar_no = clip.start_ticks / bar_ticks + 1;
                    self.recording_problem(format!(
                        "Audio track {} already has a clip at bar {bar_no}. Move the transport \
                         past it (or delete that clip), then press Rec again.",
                        track + 1
                    ));
                    return;
                }
            }
        }
        // Fresh take counters (see `take_events`): the empty-take report says
        // which stage of the pipeline actually saw data.
        self.take_events = 0;
        self.take_samples = 0;
        self.take_points = 0;
        if !midi_armed.is_empty() {
            self.recorder.begin(midi_armed.clone(), record_start, from);
            if !self.engine.set_capture(true) {
                self.capture_pending = Some(true);
            }
        }
        if !curve_armed.is_empty() {
            self.control_recorder
                .begin(curve_armed.clone(), record_start, from);
            if !self.engine.set_capture(true) {
                self.capture_pending = Some(true);
            }
        }
        // R7/R8: arm the in-graph taps. Keyboard MIDI tracks already go
        // through the command-level tap, so only MIDI-effect sources need a
        // bake entry.
        let (midi_bakes, ctrl_bakes) = {
            let doc = &self.project().graph;
            let mut midi_bakes: std::collections::BTreeMap<u64, u16> = Default::default();
            let mut ctrl_bakes: std::collections::BTreeMap<u64, u16> = Default::default();
            for &track in &midi_armed {
                if let Some(cable) = doc.track_input_source(TrackKind::Midi, track)
                    && let PortRef::Node(id) = cable.from
                    && !doc.node_by_id(id).is_some_and(NodeDoc::is_keyboard)
                {
                    *midi_bakes.entry(id).or_default() |= 1 << track;
                }
            }
            for &track in &curve_armed {
                if let Some(cable) = doc.track_input_source(TrackKind::Curve, track)
                    && let PortRef::Node(id) = cable.from
                {
                    *ctrl_bakes.entry(id).or_default() |= 1 << track;
                }
            }
            (midi_bakes, ctrl_bakes)
        };
        for (node_id, tracks) in midi_bakes {
            self.engine.set_midi_bake(node_id, tracks);
        }
        for (node_id, tracks) in ctrl_bakes {
            self.engine.set_ctrl_bake(node_id, tracks);
        }
        if !audio_armed.is_empty() {
            let info = self
                .engine
                .input_info()
                .cloned()
                .expect("guarded by input_info above");
            // The take ring starts filling at `from` (pre-roll included), so
            // the pre-roll has to be skipped when the take is closed.
            let skip_frames = pre_roll_skip_frames(
                record_start - from,
                self.project().sec_per_tick(),
                info.sample_rate,
            );
            self.audio_take = Some(AudioTake {
                tracks: audio_armed.clone(),
                start_ticks: record_start,
                skip_interleaved: skip_frames * info.channels.max(1),
                channels: info.channels.max(1),
                sample_rate: info.sample_rate,
                samples: Vec::new(),
            });
            self.engine.set_audio_capture(true);
        }
        // R6 D29: optionally silence every other MIDI track while recording.
        let mut mute_bits = 0u32;
        if self.record_mute_other {
            for track in 0..self.project().midi_tracks.len().min(32) {
                if !midi_armed.contains(&track) {
                    mute_bits |= 1 << track;
                }
            }
        }
        self.engine.set_record_mute(mute_bits);
        self.engine.seek_ticks(from);
        self.send_sequencer(SequencerCommand::Play);
        self.record_started_at = Some(Instant::now());
        log::info!(
            "Recording started on midi={midi_armed:?} audio={audio_armed:?} curve={curve_armed:?} \
             from={from:.1} capture={record_start:.1}"
        );
        let mut names = Vec::new();
        names.extend(midi_armed.iter().map(|&track| format!("T{}", track + 1)));
        names.extend(audio_armed.iter().map(|&track| format!("A{}", track + 1)));
        names.extend(curve_armed.iter().map(|&track| format!("C{}", track + 1)));
        let list = names.join(", ");
        // D30: the track outlet is the only audible path once the device is
        // taken over, so a missing instrument is a real "you hear nothing"
        // warning, not a hint.
        let silent: Vec<String> = midi_armed
            .iter()
            .filter(|&&track| !midi_outlet_has_instrument(&self.project().graph, track))
            .map(|&track| format!("T{}", track + 1))
            .collect();
        // Same warning for audio: the `AudioTrack` generator ships unwired
        // (the default graph leaves the routing to the user), so a take can be
        // recorded perfectly and stay inaudible forever.
        let silent_audio: Vec<String> = audio_armed
            .iter()
            .filter(|&&track| !audio_outlet_reaches_master(&self.project().graph, track))
            .map(|&track| format!("A{}", track + 1))
            .collect();
        let mut message = if record_start > from {
            format!(
                "Recording {list} after {} bar pre-roll",
                self.record_pre_roll_bars
            )
        } else {
            format!("Recording {list}")
        };
        if !silent.is_empty() {
            message.push_str(&format!(
                " — {} has no instrument on its track outlet (you will hear nothing)",
                silent.join(", ")
            ));
        }
        if !silent_audio.is_empty() {
            message.push_str(&format!(
                " — {} is not wired to the master: connect its AudioTrack node to a mixer \
                 channel in Patch or playback stays silent",
                silent_audio.join(", ")
            ));
        }
        // R9: a loopback source hears the machine, including this project when
        // the engine outputs to that same endpoint — the one failure mode a
        // user cannot guess from the meters moving.
        if !audio_armed.is_empty()
            && let Some(info) = self.engine.input_info()
            && info.loopback
        {
            message.push_str(&format!(
                " — recording WASAPI loopback of {}: this project's own playback is part of \
                 the take if the engine outputs there",
                info.device_name
            ));
        }
        self.set_message(message);
    }

    /// Writes finished takes as clips, one undo step for the whole session.
    /// Returns `(tracks written, notes written)`, or `None` when nothing was
    /// played (no undo step, no clip).
    pub(crate) fn commit_take_clips(
        &mut self,
        label: &str,
        takes: &[(usize, Vec<Note>)],
    ) -> Option<(usize, usize)> {
        let bar = self.project().ticks_per_bar();
        let start = self.recorder.start_ticks;
        let clips: Vec<(usize, MidiClip, usize)> = takes
            .iter()
            .filter_map(|(track, notes)| {
                recorder::take_clip(start, notes, bar).map(|clip| (*track, clip, notes.len()))
            })
            .collect();
        if clips.is_empty() {
            return None;
        }
        let before = self.begin_edit();
        let mut written_tracks = Vec::new();
        let mut written_notes = 0usize;
        let mut full = Vec::new();
        for (track, clip, note_count) in clips {
            let pushed = self
                .project_mut()
                .midi_tracks
                .get_mut(track)
                .map(|t| {
                    if t.clips.len() >= MAX_CLIPS_PER_TRACK {
                        return false;
                    }
                    t.clips.push(clip);
                    true
                })
                .unwrap_or(false);
            if pushed {
                written_tracks.push(track);
                written_notes += note_count;
            } else {
                full.push(track);
            }
        }
        if written_tracks.is_empty() {
            self.session.restore(before);
            self.set_message("Recording discarded: the target track has no room".to_string());
            return None;
        }
        self.commit_edit(label, before);
        for &track in &written_tracks {
            self.sync_clip_row(timeline::TimelineLane::Midi, track);
        }
        if !full.is_empty() {
            self.set_message(format!(
                "Recording discarded for track {} (no room)",
                full[0] + 1
            ));
        }
        Some((written_tracks.len(), written_notes))
    }

    /// Cuts a finished audio take into per-track wav files (R4 D11) and returns
    /// the clips to insert. Empty when nothing was captured or the project has no
    /// directory to write into.
    ///
    /// Returns `(clips, reason)`; `reason` explains a discarded take (the
    /// caller shows it, because it also knows why the other take kinds were
    /// empty). Samples that fall inside the pre-roll are dropped, never
    /// placed at a wrong position.
    pub(crate) fn finish_audio_take(
        &mut self,
        take: AudioTake,
    ) -> (Vec<(usize, glitcherz_model::AudioClip)>, Option<String>) {
        let channels = take.channels.max(1);
        let skip = take.skip_interleaved.min(take.samples.len());
        let data = &take.samples[skip..];
        let frames = data.len() / channels;
        if frames == 0 {
            return if take.samples.is_empty() {
                (
                    Vec::new(),
                    Some(
                        "Audio take discarded: the input device delivered no samples. Check \
                         the microphone / interface and that the In meter moves."
                            .to_string(),
                    ),
                )
            } else {
                (
                    Vec::new(),
                    Some(format!(
                        "Audio take discarded: all {} sample(s) fall inside the pre-roll. \
                         Record past the pre-roll, or set it to 0 bar next to Rec.",
                        take.samples.len()
                    )),
                )
            };
        }
        let Some(base) = self.project().base_dir.clone() else {
            return (
                Vec::new(),
                Some(
                    "Audio take discarded: save the project first (takes live in <stem>.media/)"
                        .to_string(),
                ),
            );
        };
        let stem = self
            .project_path
            .as_ref()
            .and_then(|path| path.file_stem())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "untitled".to_string());
        let dir = base.join(format!("{stem}.media"));
        if let Err(err) = std::fs::create_dir_all(&dir) {
            return (
                Vec::new(),
                Some(format!(
                    "Audio take discarded: cannot create media folder ({err})"
                )),
            );
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let sample_rate = take.sample_rate.max(1);
        let source_len = frames as f64 / f64::from(sample_rate);
        // D13: manual latency compensation moves the clip earlier (positive
        // samples) or later (negative) on the timeline.
        let sec_per_tick = self.project().sec_per_tick().max(1e-9);
        let offset_ticks =
            self.record_latency_samples as f64 / f64::from(sample_rate) / sec_per_tick;
        let start_ticks = (take.start_ticks - offset_ticks).round().max(0.0) as u32;
        let mut clips = Vec::new();
        for &track in &take.tracks {
            let path = dir.join(format!("rec-{stamp}-a{}.wav", track + 1));
            if let Err(err) = write_mono_wav(&path, data, channels, sample_rate) {
                return (
                    Vec::new(),
                    Some(format!("Audio take discarded: wav write failed ({err})")),
                );
            }
            let stored = self.project().media_path_string(&path);
            clips.push((
                track,
                glitcherz_model::AudioClip {
                    name: format!(
                        "Take {} · bar {}",
                        track + 1,
                        start_ticks / self.project().ticks_per_bar() + 1
                    ),
                    group: None,
                    muted: false,
                    path: stored,
                    start_ticks,
                    source_offset: 0.0,
                    source_len,
                    gain: 1.0,
                    legacy_span_ticks: 0,
                },
            ));
        }
        (clips, None)
    }

    /// Ends the take and writes it as clips (one undo step, spec D15/R4/R6).
    ///
    /// Called by the record button and by every transport stop / pause path —
    /// the take must be closed while the transport position still means
    /// something, which is why `Stop` is sent only after this returns.
    pub(crate) fn finish_recording(&mut self) {
        if !self.is_recording() {
            return;
        }
        // Pull the last frames' events / samples out of the rings before
        // closing the takes.
        self.drain_capture();
        let mut audio_take = self.audio_take.take();
        if let Some(take) = &mut audio_take {
            let mut scratch = std::mem::take(&mut self.audio_scratch);
            let moved = self.engine.drain_recorded_audio(&mut scratch);
            self.take_samples = self.take_samples.saturating_add(moved as u64);
            take.samples.append(&mut scratch);
            self.audio_scratch = scratch;
        }
        // R8: drain the last controls before closing the curve takes.
        let mut control_batch = std::mem::take(&mut self.control_scratch);
        self.engine.drain_captured_controls(&mut control_batch);
        self.take_points = self.take_points.saturating_add(control_batch.len() as u64);
        for captured in control_batch.drain(..) {
            self.control_recorder.push(captured);
        }
        self.control_scratch = control_batch;
        // After a rewind the live position is 0, so the last observed playing
        // position is the fallback (that is the real end of the take).
        let end =
            f64::from(self.engine.musical_ticks().max(0.0)).max(self.recorder.last_position());
        // A take that never left pre-roll captured by design — the report has
        // to say that, or the user blames the wiring.
        let never_captured = self.recorder.is_active() && !self.recorder.is_capturing();
        let midi_takes = if self.recorder.is_active() {
            self.recorder.finish(end)
        } else {
            Vec::new()
        };
        let control_takes = if self.control_recorder.is_active() {
            self.control_recorder.finish(0.0)
        } else {
            Vec::new()
        };
        self.engine.set_capture(false);
        self.engine.clear_bakes();
        self.capture_pending = None;
        self.engine.set_record_mute(0);
        self.engine.set_audio_capture(false);

        let bar = self.project().ticks_per_bar();
        let midi_start = self.recorder.start_ticks;
        let midi_clips: Vec<(usize, MidiClip, usize)> = midi_takes
            .iter()
            .filter_map(|(track, notes)| {
                recorder::take_clip(midi_start, notes, bar).map(|mut clip| {
                    clip.name = format!(
                        "Take {} · bar {}",
                        track + 1,
                        clip.start_ticks / bar.max(1) + 1
                    );
                    (*track, clip, notes.len())
                })
            })
            .collect();
        let (audio_clips, audio_problem) =
            audio_take.map_or_else(|| (Vec::new(), None), |take| self.finish_audio_take(take));
        let control_start = self.control_recorder.start_ticks;
        let control_clips: Vec<(usize, ControlClip)> = control_takes
            .iter()
            .filter_map(|(track, points)| {
                recorder::control_take_clip(control_start, points, bar).map(|mut clip| {
                    clip.name = format!(
                        "Bake {} · bar {}",
                        track + 1,
                        clip.start_ticks / bar.max(1) + 1
                    );
                    (*track, clip)
                })
            })
            .collect();
        if midi_clips.is_empty() && audio_clips.is_empty() && control_clips.is_empty() {
            let events = self.take_events;
            let samples = self.take_samples;
            let points = self.take_points;
            log::warn!(
                "Take ended empty: {events} MIDI event(s), {samples} audio sample(s), \
                 {points} control point(s)"
            );
            let hint = if let Some(reason) = audio_problem.clone() {
                // The audio path knows exactly what went wrong — prefer it.
                self.recording_problem(reason);
                return;
            } else if never_captured {
                "the take ended during the pre-roll, so nothing was ever captured. Set the \
                 pre-roll to 0 bar next to Rec, or record past it."
            } else if events == 0 && samples == 0 && points == 0 {
                "the engine's capture taps returned nothing. Check the source is cabled into \
                 the track's Timeline In input (for audio, the In meter must move)."
            } else if samples == 0 && points == 0 {
                "events arrived but none became a note — they were either before the take \
                 start or addressed to an instrument instead of a track."
            } else {
                "material arrived but no take could be closed — try a longer take."
            };
            self.recording_problem(format!("Nothing recorded: {hint}"));
            return;
        }
        let before = self.begin_edit();
        let mut midi_written = Vec::new();
        let mut midi_notes = 0usize;
        for (track, clip, note_count) in midi_clips {
            let pushed = self
                .project_mut()
                .midi_tracks
                .get_mut(track)
                .map(|t| {
                    if t.clips.len() >= MAX_CLIPS_PER_TRACK {
                        return false;
                    }
                    t.clips.push(clip);
                    true
                })
                .unwrap_or(false);
            if pushed {
                midi_written.push(track);
                midi_notes += note_count;
            }
        }
        let mut audio_written = Vec::new();
        for (track, clip) in audio_clips {
            let pushed = self
                .project_mut()
                .audio_tracks
                .get_mut(track)
                .map(|t| {
                    if t.clips.len() >= MAX_CLIPS_PER_TRACK {
                        return false;
                    }
                    t.clips.push(clip);
                    true
                })
                .unwrap_or(false);
            if pushed {
                audio_written.push(track);
            }
        }
        let mut control_written = Vec::new();
        for (track, clip) in control_clips {
            let pushed = self
                .project_mut()
                .control_tracks
                .get_mut(track)
                .map(|t| {
                    if t.clips.len() >= MAX_CLIPS_PER_TRACK {
                        return false;
                    }
                    t.clips.push(clip);
                    true
                })
                .unwrap_or(false);
            if pushed {
                control_written.push(track);
            }
        }
        if midi_written.is_empty() && audio_written.is_empty() && control_written.is_empty() {
            self.session.restore(before);
            self.set_message("Recording discarded: the target track has no room".to_string());
            return;
        }
        self.commit_edit("Record", before);
        for &track in &midi_written {
            self.sync_clip_row(timeline::TimelineLane::Midi, track);
        }
        for &track in &audio_written {
            self.send_audio_clips_for_track(track);
            self.sync_clip_row(timeline::TimelineLane::Audio, track);
        }
        for &track in &control_written {
            self.send_control_clips(track);
            self.sync_clip_row(timeline::TimelineLane::Control, track);
        }
        // R5: land the user on the take they just recorded.
        let mut new_selection = Vec::new();
        for &track in &midi_written {
            if let Some(len) = self
                .project()
                .midi_tracks
                .get(track)
                .map(|track| track.clips.len())
                .filter(|len| *len > 0)
            {
                new_selection.push(timeline::ClipSel {
                    lane: timeline::TimelineLane::Midi,
                    track,
                    clip: len - 1,
                });
            }
        }
        for &track in &audio_written {
            if let Some(len) = self
                .project()
                .audio_tracks
                .get(track)
                .map(|track| track.clips.len())
                .filter(|len| *len > 0)
            {
                new_selection.push(timeline::ClipSel {
                    lane: timeline::TimelineLane::Audio,
                    track,
                    clip: len - 1,
                });
            }
        }
        for &track in &control_written {
            if let Some(len) = self
                .project()
                .control_tracks
                .get(track)
                .map(|track| track.clips.len())
                .filter(|len| *len > 0)
            {
                new_selection.push(timeline::ClipSel {
                    lane: timeline::TimelineLane::Control,
                    track,
                    clip: len - 1,
                });
            }
        }
        if !new_selection.is_empty() {
            self.timeline_state.selection = new_selection;
            self.view = MainView::Timeline;
        }
        let first_track = midi_written.first().copied();
        if let Some(track) = first_track {
            self.selected_track = track;
        }
        let mut parts = Vec::new();
        if !midi_written.is_empty() {
            parts.push(format!(
                "{midi_notes} notes into {} MIDI track{}",
                midi_written.len(),
                if midi_written.len() == 1 { "" } else { "s" }
            ));
        }
        if !audio_written.is_empty() {
            parts.push(format!(
                "audio into {} track{}",
                audio_written.len(),
                if audio_written.len() == 1 { "" } else { "s" }
            ));
        }
        if !control_written.is_empty() {
            parts.push(format!(
                "curves into {} track{}",
                control_written.len(),
                if control_written.len() == 1 { "" } else { "s" }
            ));
        }
        // A take that recorded but dropped its audio still has to say why.
        let mut summary = format!("Recorded {}", parts.join(" and "));
        if let Some(reason) = audio_problem {
            summary.push_str(&format!(" — {reason}"));
        }
        self.set_message(summary);
    }

    /// Drops the take in flight without writing a clip (new / open project).
    pub(crate) fn cancel_recording(&mut self) {
        if !self.is_recording() {
            return;
        }
        self.recorder.reset();
        self.control_recorder.reset();
        self.audio_take = None;
        self.engine.set_capture(false);
        self.engine.clear_bakes();
        self.engine.set_audio_capture(false);
        self.engine.set_record_mute(0);
        self.capture_pending = None;
        self.record_started_at = None;
        self.capture_scratch.clear();
        self.control_scratch.clear();
        self.audio_scratch.clear();
    }

    /// Record button: start a take, or end the one in flight.
    pub(crate) fn toggle_recording(&mut self) {
        if self.is_recording() {
            self.finish_recording();
        } else {
            self.start_recording();
        }
    }
}
