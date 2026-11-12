//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    /// Rename one track (any lane). Shared by the arrangement header and the
    /// patch view's timeline outlets, so both surfaces record the same single
    /// undo step. Presentation only: no engine push, no node retarget.
    pub(crate) fn rename_track(
        &mut self,
        lane: timeline::TimelineLane,
        track: usize,
        name: String,
    ) {
        let current = match lane {
            timeline::TimelineLane::Midi => self
                .project()
                .midi_tracks
                .get(track)
                .map(|t| t.name.clone()),
            timeline::TimelineLane::Control => self
                .project()
                .control_tracks
                .get(track)
                .map(|t| t.name.clone()),
            timeline::TimelineLane::Audio => self
                .project()
                .audio_tracks
                .get(track)
                .map(|t| t.name.clone()),
        };
        if current.is_some_and(|n| n != name) {
            let before = self.begin_edit();
            match lane {
                timeline::TimelineLane::Midi => {
                    if let Some(t) = self.project_mut().midi_tracks.get_mut(track) {
                        t.name = name;
                    }
                }
                timeline::TimelineLane::Control => {
                    if let Some(t) = self.project_mut().control_tracks.get_mut(track) {
                        t.name = name;
                    }
                }
                timeline::TimelineLane::Audio => {
                    if let Some(t) = self.project_mut().audio_tracks.get_mut(track) {
                        t.name = name;
                    }
                }
            }
            self.commit_edit("Rename Track", before);
        }
    }

    /// Rename one mixer channel's custom name. Shared by the Inspector console
    /// and the patch view's mixer inlets. An empty name clears the override,
    /// so the derived track / `Ch N` label comes back.
    pub(crate) fn rename_mixer_channel(&mut self, node: u64, channel: u8, name: String) {
        let current = self.project().graph.node_by_id(node).and_then(|n| match n {
            NodeDoc::Mixer { channels, .. } => {
                channels.get(channel as usize).map(|c| c.name.clone())
            }
            _ => None,
        });
        if current.is_some_and(|n| n != name) {
            let before = self.begin_edit();
            if let Some(NodeDoc::Mixer { channels, .. }) =
                self.project_mut().graph.node_by_id_mut(node)
                && let Some(c) = channels.get_mut(channel as usize)
            {
                c.name = name;
            }
            self.commit_edit("Rename Channel", before);
        }
    }

    pub(crate) fn timeline_panel(&mut self, ui: &mut egui::Ui) {
        // Audio-only projects still own the arrangement row: returning here
        // left the panel with no keyboard surface at all (and no way to use
        // its shortcuts).
        if self.project().midi_tracks.is_empty()
            && self.project().control_tracks.is_empty()
            && self.project().audio_tracks.is_empty()
        {
            return;
        }

        let playhead = self.engine.musical_ticks();
        let playing = self.engine.is_playing();
        let selected_track = self.selected_track;
        let selected_auto = self.selected_auto;
        let selected_audio = self.selected_audio;
        let edit_target = self.edit_clip.as_ref().and_then(|t| match t {
            // P3: the outline follows the *focus* clip, not the whole context.
            EditTarget::Midi(subject) => {
                let focus = subject.focus();
                Some(timeline::EditHighlight {
                    track: focus.track,
                    clip: focus.clip,
                })
            }
            // The MIDI row consumes this outline; a control target has no
            // MIDI outline (its curve editor is separate, U12).
            EditTarget::Control { .. } => None,
        });
        // U6: audio clip bodies read peaks from the session sample cache
        // (read-only lookup — decoding happens on the edit paths).
        //
        // The cache is keyed by the **resolved** path: the playback table
        // looks clips up through `resolve_media_path` (project directory +
        // relative path), and a recorded take stores exactly that relative
        // form. Looking it up raw missed every clip under a saved project —
        // the clip drew as an empty box while playback still had sound.
        let media_base = self.project().base_dir.clone();
        let waveform_of = {
            let library = &self.sample_library;
            move |path: &str| {
                let raw = std::path::Path::new(path);
                let resolved = match (&media_base, raw.is_absolute()) {
                    (Some(dir), false) => dir.join(raw),
                    _ => raw.to_path_buf(),
                };
                library.get(&resolved)
            }
        };
        let actions = {
            let Self {
                session,
                timeline_state: state,
                focus_router: router,
                ..
            } = self;
            // Read-only: the timeline is a view now; its edit actions come
            // back as data and are applied (undoably) below.
            let project = session.project();
            // Met M4: one meter table per frame — the ruler, grid and
            // snapping all read it (D4 keeps the table tiny and the queries
            // binary-search).
            let meter = project.meter_map();
            let (midi, control, audio) = (
                &project.midi_tracks,
                &project.control_tracks,
                &project.audio_tracks,
            );
            timeline::show(
                ui,
                state,
                midi,
                control,
                audio,
                timeline::TimelineParams {
                    selected_track,
                    selected_auto,
                    selected_audio,
                    playhead_ticks: playhead,
                    playing,
                    edit_target,
                    loop_range: &project.metadata.loop_range,
                    markers: &project.metadata.markers,
                    meter_markers: &project.metadata.meter_markers,
                    // Met M4: the piecewise meter table, not a single bar.
                    meter: &meter,
                    // U11 D3: audio clip spans are derived at the base tempo.
                    sec_per_tick: project.sec_per_tick(),
                    waveform_of: &waveform_of,
                    clip_groups: &project.clip_groups,
                    midi_data: &project.midi_data,
                    curve_data: &project.curve_data,
                },
                router,
            )
        };
        for action in actions {
            self.apply_timeline_action(action);
        }
    }

    /// Applies one timeline view action (engine pushes / cross-panel state).
    pub(crate) fn apply_timeline_action(&mut self, action: timeline::TimelineAction) {
        match action {
            timeline::TimelineAction::MidiEdited(track) => self.send_track_clips(track),
            timeline::TimelineAction::ControlEdited(index) => self.send_control_clips(index),
            timeline::TimelineAction::AudioEdited(track) => self.send_audio_clips_for_track(track),
            // Selecting a track row no longer closes the editor: the Edit tab
            // follows the **clip** selection, which this action does not touch.
            timeline::TimelineAction::SelectTrack(track) => {
                self.selected_track = track;
            }
            timeline::TimelineAction::SelectControlTrack(index) => {
                self.selected_auto = index;
            }
            timeline::TimelineAction::SelectAudioTrack(track) => {
                self.selected_audio = track;
            }
            timeline::TimelineAction::OpenMidiEditor { track, clip } => {
                self.selected_track = track;
                // Double-click jumps straight to the editor.
                self.set_edit_target(track, clip);
                self.view = MainView::Edit;
            }
            // U12: a control clip opens the Edit tab's curve editor.
            timeline::TimelineAction::OpenControlEditor { track, clip } => {
                self.selected_auto = track;
                self.set_control_edit_target(track, clip);
                self.view = MainView::Edit;
            }
            timeline::TimelineAction::AddMidiTrack => self.add_midi_track(),
            timeline::TimelineAction::RemoveLastMidiTrack => self.remove_last_midi_track(),
            timeline::TimelineAction::AddControlTrack => self.add_control_track(),
            timeline::TimelineAction::RemoveControlTrack => self.remove_control_track(),
            timeline::TimelineAction::AddAudioTrack => self.add_audio_track(),
            timeline::TimelineAction::RemoveLastAudioTrack => self.remove_last_audio_track(),
            timeline::TimelineAction::ReorderTrack { lane, from, to } => {
                self.reorder_track(lane, from, to)
            }
            // Inline header rename (right-click → type → Enter). Presentation
            // only: no engine push, no node retarget — one undo step.
            timeline::TimelineAction::RenameTrack { lane, track, name } => {
                self.rename_track(lane, track, name);
            }
            // W3a: inline clip rename (F2 / Alt+double-click). Presentation
            // only, so no engine push — one undo step.
            timeline::TimelineAction::RenameClip {
                lane,
                track,
                clip,
                name,
            } => {
                let current = match lane {
                    timeline::TimelineLane::Midi => self
                        .project()
                        .midi_tracks
                        .get(track)
                        .and_then(|t| t.clips.get(clip))
                        .map(|c| c.name.clone()),
                    timeline::TimelineLane::Control => self
                        .project()
                        .control_tracks
                        .get(track)
                        .and_then(|t| t.clips.get(clip))
                        .map(|c| c.name.clone()),
                    timeline::TimelineLane::Audio => self
                        .project()
                        .audio_tracks
                        .get(track)
                        .and_then(|t| t.clips.get(clip))
                        .map(|c| c.name.clone()),
                };
                if current.is_some_and(|n| n != name) {
                    let before = self.begin_edit();
                    match lane {
                        timeline::TimelineLane::Midi => {
                            if let Some(c) = self
                                .project_mut()
                                .midi_tracks
                                .get_mut(track)
                                .and_then(|t| t.clips.get_mut(clip))
                            {
                                timeline::Placement::set_name(c, name.clone());
                            }
                        }
                        timeline::TimelineLane::Control => {
                            if let Some(c) = self
                                .project_mut()
                                .control_tracks
                                .get_mut(track)
                                .and_then(|t| t.clips.get_mut(clip))
                            {
                                timeline::Placement::set_name(c, name.clone());
                            }
                        }
                        timeline::TimelineLane::Audio => {
                            if let Some(c) = self
                                .project_mut()
                                .audio_tracks
                                .get_mut(track)
                                .and_then(|t| t.clips.get_mut(clip))
                            {
                                timeline::Placement::set_name(c, name.clone());
                            }
                        }
                    }
                    self.commit_edit("Rename Clip", before);
                }
            }
            // W3b: create / dissolve a cross-track, cross-lane clip group.
            timeline::TimelineAction::GroupClips { clips } => self.timeline_group_clips(clips),
            timeline::TimelineAction::UngroupClips { clips } => self.timeline_ungroup_clips(clips),
            // B2: temporary clip mute (Ctrl+middle click / H / Alt+H).
            timeline::TimelineAction::SetClipMuted { clips, muted } => {
                self.timeline_set_clip_muted(clips, muted)
            }
            timeline::TimelineAction::AddClip {
                lane,
                track,
                start_ticks,
            } => self.timeline_add_clip(lane, track, start_ticks),
            timeline::TimelineAction::AddAudioClip { track, start_ticks } => {
                self.timeline_add_audio_clip(track, start_ticks)
            }
            timeline::TimelineAction::MoveResizeClip {
                lane,
                track,
                clip,
                start_ticks,
                span_ticks,
                loop_ticks,
            } => self.timeline_move_resize_clip(
                lane,
                track,
                clip,
                start_ticks,
                span_ticks,
                loop_ticks,
            ),
            timeline::TimelineAction::TrimClipHead {
                lane,
                track,
                clip,
                start_ticks,
                span_ticks,
                delta_ticks,
            } => self.timeline_trim_clip_head(
                lane,
                track,
                clip,
                start_ticks,
                span_ticks,
                delta_ticks,
            ),
            timeline::TimelineAction::EndClipTrim => self.timeline_end_clip_trim(),
            timeline::TimelineAction::RemoveClip { lane, track, clip } => {
                self.timeline_remove_clip(lane, track, clip)
            }
            timeline::TimelineAction::RemoveClipsBatch { clips } => {
                self.timeline_remove_clips_batch(clips)
            }
            timeline::TimelineAction::SplitClip {
                lane,
                track,
                clip,
                at_ticks,
            } => self.timeline_split_clip(lane, track, clip, at_ticks),
            timeline::TimelineAction::MoveClipsBatch { moves } => {
                self.timeline_move_clips_batch(moves)
            }
            timeline::TimelineAction::StretchClipsBatch { spans } => {
                self.timeline_stretch_clips_batch(spans)
            }
            timeline::TimelineAction::TileClipsBatch { tiles } => {
                self.timeline_tile_clips_batch(tiles)
            }
            timeline::TimelineAction::DuplicateClipsBatch { dups } => {
                self.timeline_duplicate_clips_batch(dups)
            }
            timeline::TimelineAction::DuplicateClipsWithNewData { dups } => {
                self.timeline_duplicate_clips_with_data_batch(dups)
            }
            timeline::TimelineAction::CopySelection => self.copy_timeline_selection(),
            timeline::TimelineAction::PasteClips => self.paste_timeline_clips(),
            timeline::TimelineAction::Seek { ticks } => {
                // Not a document edit: straight to the engine.
                self.engine.seek_ticks(ticks);
            }
            timeline::TimelineAction::SetLoop {
                start_ticks,
                end_ticks,
            } => self.timeline_set_loop(start_ticks, end_ticks),
            timeline::TimelineAction::ToggleLoop(enabled) => self.timeline_toggle_loop(enabled),
            timeline::TimelineAction::AddMarker { tick } => self.timeline_add_marker(tick),
            timeline::TimelineAction::RemoveMarker { index } => self.timeline_remove_marker(index),
            // Met M5a: meter changes are a sorted document table; each
            // action below is one undo step (a drag / numerator gesture is
            // merged by the parameter-edit key).
            timeline::TimelineAction::AddMeterMarker { tick } => {
                self.timeline_add_meter_marker(tick)
            }
            timeline::TimelineAction::MoveMeterMarker { index, tick } => {
                self.timeline_move_meter_marker(index, tick)
            }
            timeline::TimelineAction::SetMeterMarker { index, signature } => {
                self.timeline_set_meter_marker(index, signature)
            }
            timeline::TimelineAction::EndMeterMarkerEdit => self.end_param_edit(),
            timeline::TimelineAction::RemoveMeterMarker { index } => {
                self.timeline_remove_meter_marker(index)
            }
        }
    }

    /// Adds a meter change at `tick` (Met M5a): sorted insert, initial
    /// signature = the containing segment's meter, and the inline editor opens
    /// on the new marker. Tick 0 belongs to the base meter (D3).
    pub(crate) fn timeline_add_meter_marker(&mut self, tick: u32) {
        if tick == 0 {
            self.set_message(
                "Meter changes start after bar 1 — tick 0 is the base meter".to_string(),
            );
            return;
        }
        let markers = &self.project().metadata.meter_markers;
        if markers.iter().any(|marker| marker.tick == tick) {
            self.set_message("A meter change already sits on that bar".to_string());
            return;
        }
        if markers.len() >= glitcherz_model::MAX_METER_MARKERS {
            self.set_message(format!(
                "Meter change limit reached ({})",
                glitcherz_model::MAX_METER_MARKERS
            ));
            return;
        }
        let signature = self.project().meter_map().at(tick);
        let before = self.begin_edit();
        let markers = &mut self.project_mut().metadata.meter_markers;
        let index = markers
            .iter()
            .position(|marker| marker.tick > tick)
            .unwrap_or(markers.len());
        markers.insert(index, MeterMarker { tick, signature });
        self.commit_edit("Add Meter Change", before);
        self.push_time_signature();
        self.timeline_state.meter_edit = Some(index);
    }

    /// Drags a meter capsule (Met M5a). The view already snapped the tick and
    /// clamped it between the neighbours; one gesture = one undo step.
    pub(crate) fn timeline_move_meter_marker(&mut self, index: usize, tick: u32) {
        let Some(current) = self.project().metadata.meter_markers.get(index).copied() else {
            return;
        };
        if current.tick == tick {
            return;
        }
        let before = self.begin_param_edit();
        let Some(marker) = self.project_mut().metadata.meter_markers.get_mut(index) else {
            return;
        };
        marker.tick = tick;
        self.commit_param_edit("Move Meter Change", before, "meter-marker-move");
        self.push_time_signature();
    }

    /// Edits a meter marker's signature (Met M5a). A numerator drag reports
    /// every frame; the key merges them into one undo step.
    pub(crate) fn timeline_set_meter_marker(
        &mut self,
        index: usize,
        signature: glitcherz_model::TimeSignature,
    ) {
        let Some(current) = self.project().metadata.meter_markers.get(index).copied() else {
            return;
        };
        if current.signature == signature {
            return;
        }
        let before = self.begin_param_edit();
        let Some(marker) = self.project_mut().metadata.meter_markers.get_mut(index) else {
            return;
        };
        marker.signature = signature;
        self.commit_param_edit("Meter Change", before, "meter-marker-value");
        self.push_time_signature();
        self.set_message(format!("Meter change {signature}"));
    }

    /// Removes the meter change at `index` (Met M5a). One undo step; the
    /// segment falls back to the previous signature.
    pub(crate) fn timeline_remove_meter_marker(&mut self, index: usize) {
        if index >= self.project().metadata.meter_markers.len() {
            return;
        }
        let before = self.begin_edit();
        self.project_mut().metadata.meter_markers.remove(index);
        self.commit_edit("Remove Meter Change", before);
        self.push_time_signature();
        self.timeline_state.meter_edit = None;
    }

    /// Sets the song loop range (U1): one undoable document edit, then the
    /// engine push (a disabled/degenerate range disables looping).
    pub(crate) fn timeline_set_loop(&mut self, start_ticks: u32, end_ticks: u32) {
        let before = self.begin_edit();
        self.project_mut().metadata.loop_range = SongLoop {
            enabled: true,
            start_ticks,
            end_ticks,
        };
        self.commit_edit("Set Song Loop", before);
        self.push_loop_range();
    }

    /// Toggles the song loop on/off (U1); the range itself is kept.
    pub(crate) fn timeline_toggle_loop(&mut self, enabled: bool) {
        let before = self.begin_edit();
        self.project_mut().metadata.loop_range.enabled = enabled;
        self.commit_edit("Toggle Song Loop", before);
        self.push_loop_range();
    }

    /// Adds a timeline marker (U1): auto-named "M<n>", inserted in tick
    /// order. One undo step; the marker cap refuses with a status message.
    pub(crate) fn timeline_add_marker(&mut self, tick: u32) {
        if self.project().metadata.markers.len() >= MAX_MARKERS {
            self.set_message("Marker limit reached (32)".to_string());
            return;
        }
        let before = self.begin_edit();
        let markers = &mut self.project_mut().metadata.markers;
        let name = format!("M{}", markers.len() + 1);
        let index = markers
            .iter()
            .position(|m| m.tick > tick)
            .unwrap_or(markers.len());
        markers.insert(index, Marker { tick, name });
        self.commit_edit("Add Marker", before);
    }

    /// Removes the marker at `index` (U1). One undo step.
    pub(crate) fn timeline_remove_marker(&mut self, index: usize) {
        let before = self.begin_edit();
        let markers = &mut self.project_mut().metadata.markers;
        if index >= markers.len() {
            return;
        }
        markers.remove(index);
        self.commit_edit("Remove Marker", before);
    }

    /// Forwards the document's loop range to the engine (disabled or
    /// degenerate ranges disable looping there; U1).
    pub(crate) fn push_loop_range(&mut self) {
        let range = self.project().metadata.loop_range;
        let (start, end) = if range.is_active() {
            (range.start_ticks, range.end_ticks)
        } else {
            (0, 0)
        };
        self.engine.set_loop_range(start, end);
    }

    /// Pushes the document meter to the engine (U3a / Met M5b): the base meter
    /// **and** the full piecewise segment table. Idempotent, so every caller
    /// (meter edit, marker edit, load, undo) can just call this.
    pub(crate) fn push_time_signature(&mut self) {
        let meter = self.project().metadata.time_signature;
        self.send_sequencer(SequencerCommand::SetTimeSignature {
            numerator: meter.numerator,
            denominator: meter.denominator,
        });
        let map = self.project().meter_map();
        let mut segs = [MeterSeg::DEFAULT; MAX_METER_SEGS];
        let mut count = 0usize;
        for (index, seg) in map.segments().iter().take(MAX_METER_SEGS).enumerate() {
            segs[index] = MeterSeg {
                tick: seg.tick,
                bar: seg.bar,
                numerator: seg.signature.numerator,
                denominator: seg.signature.denominator,
            };
            count += 1;
        }
        self.send_sequencer(SequencerCommand::SetMeterMarkers {
            count: count as u8,
            segs,
        });
    }

    /// Meter change from the transport (U3a / Met): the caller passes the
    /// gesture snapshot, so a numerator drag is one merged undo step and a
    /// dropdown click is a step of its own. **No event moves** — only the
    /// bar-derived measures (grid, loop snapping, arrangement end, the ruler's
    /// bar numbers) change. Existing clips, markers and the loop range keep
    /// their ticks, so a loop may stop sitting on a bar line; the next manual
    /// drag snaps to the new grid.
    pub(crate) fn set_meter(
        &mut self,
        meter: glitcherz_model::TimeSignature,
        before: std::sync::Arc<Project>,
    ) {
        if meter == self.project().metadata.time_signature {
            return;
        }
        self.project_mut().metadata.time_signature = meter;
        self.commit_param_edit("Meter", before, "meter");
        self.push_time_signature();
        self.set_message(format!("Meter {meter}"));
    }

    /// Timeline clip edits: the view sends data, these apply it undoably.
    /// A drag frame goes through `commit_param_edit` with the gesture key, so
    /// one drag is one undo step.
    pub(crate) fn timeline_add_clip(
        &mut self,
        lane: timeline::TimelineLane,
        track: usize,
        start_ticks: u32,
    ) {
        let before = self.begin_edit();
        // A new clip is one bar long — of the meter segment the clip starts in
        // (Met M4), not the 4/4 constant the model default carries.
        let bar = self.project().ticks_per_bar_at(start_ticks);
        let sec_per_tick = self.project().sec_per_tick();
        let added = match lane {
            timeline::TimelineLane::Control => self
                .project_mut()
                .add_control_clip(track, start_ticks)
                .is_some(),
            timeline::TimelineLane::Audio => self
                .project_mut()
                .audio_tracks
                .get_mut(track)
                .map(|t| {
                    // Audio clips are born one bar; the caller replaces the
                    // empty path with a wav right after (U11: a seconds window).
                    if t.clips.len() < MAX_CLIPS_PER_TRACK {
                        t.clips.push(glitcherz_model::AudioClip {
                            path: String::new(),
                            start_ticks,
                            source_offset: 0.0,
                            source_len: f64::from(bar) * sec_per_tick,
                            gain: 1.0,
                            ..glitcherz_model::AudioClip::default()
                        });
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false),
            timeline::TimelineLane::Midi => self
                .project_mut()
                .add_midi_clip(track, start_ticks)
                .is_some(),
        };
        if added {
            self.commit_edit("Add Clip", before);
            self.sync_clip_row(lane, track);
        }
    }

    /// U6 D2: double-click on empty audio-track space asks for an audio file.
    /// The modal dialog runs here (the timeline is a view); the clip is born
    /// at the clicked tick with the span derived from the file's duration at
    /// the current tempo (D1: stretched across the span on playback).
    pub(crate) fn timeline_add_audio_clip(&mut self, track: usize, start_ticks: u32) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Audio", AUDIO_FILE_EXTENSIONS)
            .pick_file()
        else {
            return;
        };
        let path = path.display().to_string();
        let Ok(data) = self.sample_library.load(std::path::Path::new(&path)) else {
            self.set_message("Audio clip unreadable: not a supported audio file".to_string());
            return;
        };
        // R4 D11: store the path relative to the project when it lives under
        // the project directory; absolute paths keep working untouched.
        let stored_path = self
            .project()
            .media_path_string(std::path::Path::new(&path));
        // U11: the window IS the wav's natural duration (native-rate playback
        // — no stretch). Clamp the resulting tick footprint to ≥ one bar and
        // within the arrangement bound.
        let seconds = data.frames().len() as f64 / f64::from(data.sample_rate());
        let sec_per_tick = self.project().sec_per_tick();
        let min_len = f64::from(self.project().ticks_per_bar_at(start_ticks)) * sec_per_tick;
        let max_len = f64::from(MAX_ARRANGEMENT_TICKS as u32 - start_ticks) * sec_per_tick;
        let source_len = if max_len > min_len {
            seconds.clamp(min_len, max_len)
        } else {
            min_len
        };
        let before = self.begin_edit();
        let added = self
            .project_mut()
            .audio_tracks
            .get_mut(track)
            .map(|t| {
                if t.clips.len() < MAX_CLIPS_PER_TRACK {
                    t.clips.push(glitcherz_model::AudioClip {
                        path: stored_path.clone(),
                        start_ticks,
                        source_offset: 0.0,
                        source_len,
                        gain: 1.0,
                        ..glitcherz_model::AudioClip::default()
                    });
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if added {
            self.commit_edit("Add Audio Clip", before);
            self.send_audio_clips_for_track(track);
        }
    }

    pub(crate) fn timeline_move_resize_clip(
        &mut self,
        lane: timeline::TimelineLane,
        track: usize,
        clip: usize,
        start_ticks: u32,
        span_ticks: u32,
        _loop_ticks: Option<u32>,
    ) {
        let before = self.begin_edit();
        let sec_per_tick = self.project().sec_per_tick();
        let moved = match lane {
            timeline::TimelineLane::Control => self
                .project_mut()
                .control_tracks
                .get_mut(track)
                .and_then(|t| t.clips.get_mut(clip))
                .map(|c| {
                    let changed = c.start_ticks != start_ticks || c.span_ticks != span_ticks;
                    c.start_ticks = start_ticks;
                    c.span_ticks = span_ticks;
                    changed
                })
                .unwrap_or(false),
            timeline::TimelineLane::Audio => self
                .project_mut()
                .audio_tracks
                .get_mut(track)
                .and_then(|t| t.clips.get_mut(clip))
                .map(|c| {
                    let old_span = (c.source_len / sec_per_tick).round().max(1.0) as u32;
                    let old_start = c.start_ticks;
                    let changed = old_start != start_ticks || old_span != span_ticks;
                    // U11: resize = trim. Only a length change rewrites the
                    // window; a move or a right-edge drag keeps the window start.
                    if span_ticks != old_span {
                        // A left-edge drag (the start moved too) advances the
                        // window start so the remaining material stays put.
                        if start_ticks != old_start {
                            let delta = i64::from(start_ticks) - i64::from(old_start);
                            c.source_offset =
                                (c.source_offset + delta as f64 * sec_per_tick).max(0.0);
                        }
                        c.source_len = f64::from(span_ticks).max(1.0) * sec_per_tick;
                    }
                    c.start_ticks = start_ticks;
                    changed
                })
                .unwrap_or(false),
            timeline::TimelineLane::Midi => self
                .project_mut()
                .midi_tracks
                .get_mut(track)
                .and_then(|t| t.clips.get_mut(clip))
                .map(|c| {
                    let changed = c.start_ticks != start_ticks || c.span_ticks != span_ticks;
                    c.start_ticks = start_ticks;
                    c.span_ticks = span_ticks;
                    changed
                })
                .unwrap_or(false),
        };
        if moved {
            // One drag = one undo step, even across dozens of frames.
            self.commit_param_edit(
                "Move Clip",
                before,
                &format!("timeline:{}:{}:{}", lane as u8, track, clip),
            );
            // Every kind must reach the engine immediately. The old code
            // pushed only audio: dragging a MIDI/control clip left the
            // sequencer playing the previous placement until the next
            // unrelated edit, so the new span never sounded.
            self.sync_clip_row(lane, track);
        }
    }

    /// I6: one frame of a head trim (dragging a clip's left edge).
    ///
    /// The content is re-based from the drag-origin snapshot: `retain_from`
    /// rewrites the note/curve positions, so rebuilding from the live clip
    /// every frame would remove a little more material on every frame. MIDI
    /// and control clips with a single period get a real cut; a tiled clip
    /// has no "start of the content" to cut, so only its container shrinks
    /// and the repeat phase is kept.
    pub(crate) fn timeline_trim_clip_head(
        &mut self,
        lane: timeline::TimelineLane,
        track: usize,
        clip: usize,
        start_ticks: u32,
        span_ticks: u32,
        delta_ticks: u32,
    ) {
        use crate::timeline::Placement;

        let sel = timeline::ClipSel { lane, track, clip };
        if self
            .timeline_state
            .head_trim
            .as_ref()
            .is_none_or(|t| t.sel != sel)
        {
            let origin = match lane {
                timeline::TimelineLane::Midi => self
                    .project()
                    .midi_tracks
                    .get(track)
                    .and_then(|t| t.clips.get(clip))
                    .map(|c| timeline::HeadTrimOrigin::Midi(Box::new(c.clone()))),
                timeline::TimelineLane::Control => self
                    .project()
                    .control_tracks
                    .get(track)
                    .and_then(|t| t.clips.get(clip))
                    .map(|c| timeline::HeadTrimOrigin::Control(Box::new(c.clone()))),
                timeline::TimelineLane::Audio => self
                    .project()
                    .audio_tracks
                    .get(track)
                    .and_then(|t| t.clips.get(clip))
                    .map(|c| timeline::HeadTrimOrigin::Audio {
                        offset: c.source_offset,
                    }),
            };
            let Some(origin) = origin else {
                return;
            };
            self.timeline_state.head_trim = Some(timeline::HeadTrim { sel, origin });
        }

        let sec_per_tick = self.project().sec_per_tick();
        let before = self.begin_edit();
        let changed = {
            let Some(trim) = self.timeline_state.head_trim.clone() else {
                return;
            };
            let project = self.project_mut();
            match (&trim.origin, lane) {
                (timeline::HeadTrimOrigin::Midi(origin), timeline::TimelineLane::Midi) => project
                    .midi_tracks
                    .get_mut(track)
                    .and_then(|t| t.clips.get_mut(clip))
                    .map(|c| {
                        let mut next = origin.clone();
                        // F-041: the head trim only moves the window start;
                        // the shared content is never rewritten.
                        next.retain_from(delta_ticks, sec_per_tick);
                        next.start_ticks = start_ticks;
                        next.span_ticks = span_ticks;
                        let changed = *c != *next;
                        *c = *next;
                        changed
                    })
                    .unwrap_or(false),
                (timeline::HeadTrimOrigin::Control(origin), timeline::TimelineLane::Control) => {
                    project
                        .control_tracks
                        .get_mut(track)
                        .and_then(|t| t.clips.get_mut(clip))
                        .map(|c| {
                            let mut next = origin.clone();
                            // F-041: move the window, keep the content.
                            next.retain_from(delta_ticks, sec_per_tick);
                            next.start_ticks = start_ticks;
                            next.span_ticks = span_ticks;
                            let changed = *c != *next;
                            *c = *next;
                            changed
                        })
                        .unwrap_or(false)
                }
                (timeline::HeadTrimOrigin::Audio { offset, .. }, timeline::TimelineLane::Audio) => {
                    project
                        .audio_tracks
                        .get_mut(track)
                        .and_then(|t| t.clips.get_mut(clip))
                        .map(|c| {
                            // U11: the trim window advances with the head so the
                            // remaining material stays where it is on screen.
                            let offset = (offset + delta_ticks as f64 * sec_per_tick).max(0.0);
                            let len = f64::from(span_ticks).max(1.0) * sec_per_tick;
                            let changed = c.start_ticks != start_ticks
                                || (c.source_offset - offset).abs() > f64::EPSILON
                                || (c.source_len - len).abs() > f64::EPSILON;
                            c.start_ticks = start_ticks;
                            c.source_offset = offset;
                            c.source_len = len;
                            changed
                        })
                        .unwrap_or(false)
                }
                _ => false,
            }
        };
        if changed {
            self.commit_param_edit(
                "Trim Clip Head",
                before,
                &format!("timeline-trim:{}:{}:{}", lane as u8, track, clip),
            );
            self.sync_clip_row(lane, track);
        }
    }

    /// I6: the head-trim gesture ended — drop its origin snapshot so the
    /// next drag starts from the then-current clip.
    pub(crate) fn timeline_end_clip_trim(&mut self) {
        self.timeline_state.head_trim = None;
    }

    /// W3b: puts every selected clip into one new group (auto-named
    /// `Group N`, palette colour). The selection may span MIDI / control /
    /// audio lanes and several tracks. One undo step.
    pub(crate) fn timeline_group_clips(&mut self, clips: Vec<timeline::ClipSel>) {
        if clips.is_empty() {
            self.set_message("Select clips to group".to_string());
            return;
        }
        if self.project().clip_groups.len() >= glitcherz_model::MAX_CLIP_GROUPS {
            self.set_error("Clip group limit reached (64)".to_string());
            return;
        }
        let id = self
            .project()
            .clip_groups
            .iter()
            .map(|group| group.id)
            .max()
            .unwrap_or(0)
            + 1;
        let palette = glitcherz_model::CLIP_GROUP_COLORS;
        let color = palette[(id as usize - 1) % palette.len()];
        let before = self.begin_edit();
        self.project_mut()
            .clip_groups
            .push(glitcherz_model::ClipGroup {
                id,
                name: format!("Group {id}"),
                color,
            });
        let count = clips.len();
        for sel in clips {
            self.set_clip_group(sel, Some(id));
        }
        self.commit_edit("Group Clips", before);
        self.set_message(format!("Grouped {count} clips as Group {id}"));
    }

    /// W3b: dissolves every group touched by the selection. Clearing the tag
    /// walks the whole project, so members outside the selection dissolve
    /// too — a group is one object, not a per-clip flag. One undo step.
    pub(crate) fn timeline_ungroup_clips(&mut self, clips: Vec<timeline::ClipSel>) {
        let project = self.project();
        let mut ids: Vec<u32> = Vec::new();
        for sel in &clips {
            let group = match sel.lane {
                timeline::TimelineLane::Midi => project
                    .midi_tracks
                    .get(sel.track)
                    .and_then(|track| track.clips.get(sel.clip))
                    .and_then(timeline::Placement::group),
                timeline::TimelineLane::Control => project
                    .control_tracks
                    .get(sel.track)
                    .and_then(|track| track.clips.get(sel.clip))
                    .and_then(timeline::Placement::group),
                timeline::TimelineLane::Audio => project
                    .audio_tracks
                    .get(sel.track)
                    .and_then(|track| track.clips.get(sel.clip))
                    .and_then(timeline::Placement::group),
            };
            if let Some(id) = group
                && !ids.contains(&id)
            {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            self.set_message("Selection has no group".to_string());
            return;
        }
        let before = self.begin_edit();
        {
            let project = self.project_mut();
            for track in &mut project.midi_tracks {
                for clip in &mut track.clips {
                    if clip.group.is_some_and(|id| ids.contains(&id)) {
                        clip.group = None;
                    }
                }
            }
            for track in &mut project.control_tracks {
                for clip in &mut track.clips {
                    if clip.group.is_some_and(|id| ids.contains(&id)) {
                        clip.group = None;
                    }
                }
            }
            for track in &mut project.audio_tracks {
                for clip in &mut track.clips {
                    if clip.group.is_some_and(|id| ids.contains(&id)) {
                        clip.group = None;
                    }
                }
            }
            project.clip_groups.retain(|group| !ids.contains(&group.id));
        }
        self.commit_edit("Ungroup Clips", before);
    }

    /// B2: sets the temporary mute flag of every listed clip — one undo step.
    ///
    /// Clips already in the target state are skipped; when nothing changes no
    /// history entry is recorded and no table is re-pushed. Mute lives in the
    /// clip tables themselves (the engine has no separate mute command), so
    /// every touched track is re-sent afterwards.
    pub(crate) fn timeline_set_clip_muted(&mut self, clips: Vec<timeline::ClipSel>, muted: bool) {
        if clips.is_empty() {
            return;
        }
        let project = self.project();
        let changes = clips.iter().any(|sel| {
            let current = match sel.lane {
                timeline::TimelineLane::Midi => project
                    .midi_tracks
                    .get(sel.track)
                    .and_then(|track| track.clips.get(sel.clip))
                    .map(timeline::Placement::muted),
                timeline::TimelineLane::Control => project
                    .control_tracks
                    .get(sel.track)
                    .and_then(|track| track.clips.get(sel.clip))
                    .map(timeline::Placement::muted),
                timeline::TimelineLane::Audio => project
                    .audio_tracks
                    .get(sel.track)
                    .and_then(|track| track.clips.get(sel.clip))
                    .map(timeline::Placement::muted),
            };
            current.is_some_and(|current| current != muted)
        });
        if !changes {
            return;
        }
        // One push per touched track, in table order; a lane may be touched
        // by several clips, so the sets keep that deduplicated.
        let mut midi: Vec<usize> = Vec::new();
        let mut control: Vec<usize> = Vec::new();
        let mut audio: Vec<usize> = Vec::new();
        let before = self.begin_edit();
        for sel in &clips {
            match sel.lane {
                timeline::TimelineLane::Midi => {
                    if let Some(clip) = self
                        .project_mut()
                        .midi_tracks
                        .get_mut(sel.track)
                        .and_then(|track| track.clips.get_mut(sel.clip))
                    {
                        timeline::Placement::set_muted(clip, muted);
                        midi.push(sel.track);
                    }
                }
                timeline::TimelineLane::Control => {
                    if let Some(clip) = self
                        .project_mut()
                        .control_tracks
                        .get_mut(sel.track)
                        .and_then(|track| track.clips.get_mut(sel.clip))
                    {
                        timeline::Placement::set_muted(clip, muted);
                        control.push(sel.track);
                    }
                }
                timeline::TimelineLane::Audio => {
                    if let Some(clip) = self
                        .project_mut()
                        .audio_tracks
                        .get_mut(sel.track)
                        .and_then(|track| track.clips.get_mut(sel.clip))
                    {
                        timeline::Placement::set_muted(clip, muted);
                        audio.push(sel.track);
                    }
                }
            }
        }
        self.commit_edit(if muted { "Mute Clips" } else { "Unmute Clips" }, before);
        midi.sort_unstable();
        midi.dedup();
        control.sort_unstable();
        control.dedup();
        audio.sort_unstable();
        audio.dedup();
        for track in midi {
            self.send_track_clips(track);
        }
        for index in control {
            self.send_control_clips(index);
        }
        for track in audio {
            self.send_audio_clips_for_track(track);
        }
    }

    /// Sets one clip's group tag (all three lane tables share the shape).
    pub(crate) fn set_clip_group(&mut self, sel: timeline::ClipSel, group: Option<u32>) {
        match sel.lane {
            timeline::TimelineLane::Midi => {
                if let Some(clip) = self
                    .project_mut()
                    .midi_tracks
                    .get_mut(sel.track)
                    .and_then(|track| track.clips.get_mut(sel.clip))
                {
                    timeline::Placement::set_group(clip, group);
                }
            }
            timeline::TimelineLane::Control => {
                if let Some(clip) = self
                    .project_mut()
                    .control_tracks
                    .get_mut(sel.track)
                    .and_then(|track| track.clips.get_mut(sel.clip))
                {
                    timeline::Placement::set_group(clip, group);
                }
            }
            timeline::TimelineLane::Audio => {
                if let Some(clip) = self
                    .project_mut()
                    .audio_tracks
                    .get_mut(sel.track)
                    .and_then(|track| track.clips.get_mut(sel.clip))
                {
                    timeline::Placement::set_group(clip, group);
                }
            }
        }
    }

    pub(crate) fn timeline_remove_clip(
        &mut self,
        lane: timeline::TimelineLane,
        track: usize,
        clip: usize,
    ) {
        let before = self.begin_edit();
        let removed = match lane {
            timeline::TimelineLane::Control => self
                .project_mut()
                .control_tracks
                .get_mut(track)
                .map(|t| t.clips.remove(clip))
                .is_some(),
            timeline::TimelineLane::Audio => self
                .project_mut()
                .audio_tracks
                .get_mut(track)
                .map(|t| t.clips.remove(clip))
                .is_some(),
            timeline::TimelineLane::Midi => self
                .project_mut()
                .midi_tracks
                .get_mut(track)
                .map(|t| t.clips.remove(clip))
                .is_some(),
        };
        if removed {
            self.commit_edit("Delete Clip", before);
            self.sync_clip_row(lane, track);
        }
    }

    /// Curve editor (U12): inserts an anchor, or moves the one at `from` to
    /// `tick` (one frame of a drag).
    ///
    /// `dragging` frames go through the merged undo gesture (one Ctrl+Z for a
    /// whole drag); a click lands a single step.
    pub(crate) fn curve_upsert(
        &mut self,
        track: usize,
        clip: usize,
        from: Option<u32>,
        tick: u32,
        value: f32,
        dragging: bool,
    ) {
        let Some(loop_ticks) = self
            .project()
            .control_tracks
            .get(track)
            .and_then(|t| t.clips.get(clip))
            .and_then(|c| self.project().curve_data_of(c))
            .map(|data| data.length_ticks.max(1))
        else {
            return;
        };
        // Anchors live in loop space: a tick past the loop would neither draw
        // nor play.
        let tick = tick.min(loop_ticks - 1);
        let before = self.begin_edit();
        let Some(curve) = self
            .project_mut()
            .control_tracks
            .get(track)
            .and_then(|t| t.clips.get(clip))
            .map(|c| c.data)
            .and_then(|data| self.project_mut().curve_data_mut(data))
        else {
            return;
        };
        // A move drops the old anchor first; on the same tick the upsert
        // below already overwrites it.
        if let Some(from) = from
            && from != tick
        {
            curve.remove_near(from, 0);
        }
        curve.upsert_point(AutoPoint { tick, value });
        if dragging {
            self.commit_param_edit("Edit Anchor", before, &format!("curve:{track}:{clip}"));
        } else {
            self.commit_edit("Add Anchor", before);
        }
        self.send_control_clips(track);
    }

    /// Curve editor: right-click deletes the nearest anchor within half a snap
    /// step of `tick` (clip-relative).
    pub(crate) fn curve_remove_point(&mut self, track: usize, clip: usize, tick: u32) {
        let before = self.begin_edit();
        let data = self
            .project()
            .control_tracks
            .get(track)
            .and_then(|t| t.clips.get(clip))
            .map(|c| c.data);
        let removed = data
            .and_then(|data| self.project_mut().curve_data_mut(data))
            .is_some_and(|curve| curve.remove_near(tick, crate::curve::SNAP_TICKS / 2));
        if removed {
            self.commit_edit("Delete Anchor", before);
            self.send_control_clips(track);
        }
    }

    /// Curve editor: sets a clip's interpolation (U12). One undo step; a no-op
    /// when the value is unchanged.
    pub(crate) fn set_clip_interp(&mut self, track: usize, clip: usize, interp: CurveInterp) {
        let current = self
            .project()
            .control_tracks
            .get(track)
            .and_then(|t| t.clips.get(clip))
            .and_then(|c| self.project().curve_data_of(c))
            .map(|data| data.interp);
        if current == Some(interp) {
            return;
        }
        let before = self.begin_edit();
        let data = self
            .project()
            .control_tracks
            .get(track)
            .and_then(|t| t.clips.get(clip))
            .map(|c| c.data);
        if let Some(target) = data.and_then(|data| self.project_mut().curve_data_mut(data)) {
            target.interp = interp;
        }
        self.commit_edit("Curve Type", before);
        self.send_control_clips(track);
    }

    /// Deletes every clip in the list as ONE undo step. Per track the
    /// indices are applied descending so earlier removals cannot shift
    /// later ones (the timeline reports pre-removal indices).
    pub(crate) fn timeline_remove_clips_batch(&mut self, clips: Vec<timeline::ClipSel>) {
        if clips.is_empty() {
            return;
        }
        let before = self.begin_edit();
        let mut by_key: Vec<(timeline::TimelineLane, usize, Vec<usize>)> = Vec::new();
        for c in clips {
            if let Some((_, _, idxs)) = by_key
                .iter_mut()
                .find(|(k, t, _)| *k == c.lane && *t == c.track)
            {
                idxs.push(c.clip);
            } else {
                by_key.push((c.lane, c.track, vec![c.clip]));
            }
        }
        let mut touched = Vec::new();
        for (lane, track, mut idxs) in by_key {
            idxs.sort_unstable();
            idxs.dedup();
            let removed = match lane {
                timeline::TimelineLane::Control => self
                    .project_mut()
                    .control_tracks
                    .get_mut(track)
                    .map(|t| {
                        let mut n = 0;
                        for idx in idxs.iter().rev() {
                            if *idx < t.clips.len() {
                                t.clips.remove(*idx);
                                n += 1;
                            }
                        }
                        n
                    })
                    .unwrap_or(0),
                timeline::TimelineLane::Audio => self
                    .project_mut()
                    .audio_tracks
                    .get_mut(track)
                    .map(|t| {
                        let mut n = 0;
                        for idx in idxs.iter().rev() {
                            if *idx < t.clips.len() {
                                t.clips.remove(*idx);
                                n += 1;
                            }
                        }
                        n
                    })
                    .unwrap_or(0),
                timeline::TimelineLane::Midi => self
                    .project_mut()
                    .midi_tracks
                    .get_mut(track)
                    .map(|t| {
                        let mut n = 0;
                        for idx in idxs.iter().rev() {
                            if *idx < t.clips.len() {
                                t.clips.remove(*idx);
                                n += 1;
                            }
                        }
                        n
                    })
                    .unwrap_or(0),
            };
            if removed > 0 {
                touched.push((lane, track));
            }
        }
        if !touched.is_empty() {
            self.commit_edit("Delete Clips", before);
            for (lane, track) in touched {
                self.sync_clip_row(lane, track);
            }
        }
    }

    /// Splits one clip at `at_ticks` as ONE undo step: the left half keeps
    /// start + the shortened span, the right half is inserted right after
    /// with cloned content (loop-based clips both play the same loop). The
    /// selection is patched for the index shift (split entry dropped, later
    /// indices on the row bumped).
    pub(crate) fn timeline_split_clip(
        &mut self,
        lane: timeline::TimelineLane,
        track: usize,
        clip: usize,
        at_ticks: u32,
    ) {
        let sec_per_tick = self.project().sec_per_tick();
        let before = self.begin_edit();
        let split = match lane {
            timeline::TimelineLane::Control => self
                .project_mut()
                .control_tracks
                .get_mut(track)
                .map(|t| timeline::split_clip_in(&mut t.clips, at_ticks, clip, sec_per_tick))
                .unwrap_or(false),
            timeline::TimelineLane::Audio => self
                .project_mut()
                .audio_tracks
                .get_mut(track)
                .map(|t| timeline::split_clip_in(&mut t.clips, at_ticks, clip, sec_per_tick))
                .unwrap_or(false),
            timeline::TimelineLane::Midi => self
                .project_mut()
                .midi_tracks
                .get_mut(track)
                .map(|t| timeline::split_clip_in(&mut t.clips, at_ticks, clip, sec_per_tick))
                .unwrap_or(false),
        };
        if split {
            self.commit_edit("Split Clip", before);
            self.sync_clip_row(lane, track);
            let sel = &mut self.timeline_state.selection;
            sel.retain(|s| s.lane != lane || s.track != track || s.clip != clip);
            for s in sel.iter_mut() {
                if s.lane == lane && s.track == track && s.clip > clip {
                    s.clip += 1;
                }
            }
        }
    }

    /// G-modal finalize: moves every clip in ONE undo step (may cross
    /// tracks of the same kind). Sources are removed descending per track,
    /// then re-appended at their destinations with the new start —
    /// appending cannot shift any remaining index.
    pub(crate) fn timeline_move_clips_batch(&mut self, moves: Vec<timeline::ClipMove>) {
        if moves.is_empty() {
            return;
        }
        let before = self.begin_edit();
        let mut touched = Vec::new();
        {
            let project = self.project_mut();
            for lane in [
                timeline::TimelineLane::Midi,
                timeline::TimelineLane::Control,
                timeline::TimelineLane::Audio,
            ] {
                let kind_moves: Vec<_> = moves.iter().filter(|m| m.from.lane == lane).collect();
                if kind_moves.is_empty() {
                    continue;
                }
                let spec: Vec<(usize, usize, usize, u32)> = kind_moves
                    .iter()
                    .map(|m| (m.from.track, m.from.clip, m.to_track, m.start_ticks))
                    .collect();
                let rows = match lane {
                    timeline::TimelineLane::Control => {
                        let mut tables: Vec<&mut Vec<ControlClip>> = project
                            .control_tracks
                            .iter_mut()
                            .map(|t| &mut t.clips)
                            .collect();
                        relocate_in(tables.as_mut_slice(), &spec)
                    }
                    timeline::TimelineLane::Midi => {
                        let mut tables: Vec<&mut Vec<MidiClip>> = project
                            .midi_tracks
                            .iter_mut()
                            .map(|t| &mut t.clips)
                            .collect();
                        relocate_in(tables.as_mut_slice(), &spec)
                    }
                    timeline::TimelineLane::Audio => {
                        let mut tables: Vec<&mut Vec<glitcherz_model::AudioClip>> = project
                            .audio_tracks
                            .iter_mut()
                            .map(|t| &mut t.clips)
                            .collect();
                        relocate_in(tables.as_mut_slice(), &spec)
                    }
                };
                for r in rows {
                    touched.push((lane, r));
                }
            }
        }
        if !touched.is_empty() {
            self.commit_edit("Move Clips", before);
            for (lane, track) in touched {
                self.sync_clip_row(lane, track);
            }
        }
    }

    /// T-modal finalize: stretches every clip in ONE undo step (same
    /// track, bar-aligned spans validated by the view).
    pub(crate) fn timeline_stretch_clips_batch(&mut self, spans: Vec<timeline::ClipSpanEdit>) {
        if spans.is_empty() {
            return;
        }
        let sec_per_tick = self.project().sec_per_tick();
        let before = self.begin_edit();
        let mut touched = Vec::new();
        {
            let project = self.project_mut();
            for e in &spans {
                let changed = match e.sel.lane {
                    timeline::TimelineLane::Control => project
                        .control_tracks
                        .get_mut(e.sel.track)
                        .and_then(|t| t.clips.get_mut(e.sel.clip))
                        .map(|c| {
                            c.start_ticks = e.start_ticks;
                            c.span_ticks = e.span_ticks;
                        })
                        .is_some(),
                    timeline::TimelineLane::Midi => project
                        .midi_tracks
                        .get_mut(e.sel.track)
                        .and_then(|t| t.clips.get_mut(e.sel.clip))
                        .map(|c| {
                            c.start_ticks = e.start_ticks;
                            c.span_ticks = e.span_ticks;
                        })
                        .is_some(),
                    // U11: a stretch trims the seconds window (a left-edge
                    // stretch also advances the window start).
                    timeline::TimelineLane::Audio => project
                        .audio_tracks
                        .get_mut(e.sel.track)
                        .and_then(|t| t.clips.get_mut(e.sel.clip))
                        .map(|c| {
                            let old_span = (c.source_len / sec_per_tick).round().max(1.0) as u32;
                            let old_start = c.start_ticks;
                            if e.span_ticks != old_span {
                                if e.start_ticks != old_start {
                                    let delta = i64::from(e.start_ticks) - i64::from(old_start);
                                    c.source_offset =
                                        (c.source_offset + delta as f64 * sec_per_tick).max(0.0);
                                }
                                c.source_len = f64::from(e.span_ticks).max(1.0) * sec_per_tick;
                            }
                            c.start_ticks = e.start_ticks;
                        })
                        .is_some(),
                };
                if changed && !touched.contains(&(e.sel.lane, e.sel.track)) {
                    touched.push((e.sel.lane, e.sel.track));
                }
            }
        }
        if !touched.is_empty() {
            self.commit_edit("Stretch Clips", before);
            for (lane, track) in touched {
                self.sync_clip_row(lane, track);
            }
        }
    }

    /// Tile-modal finalize: grows every clip's container to `span_ticks`
    /// while the content period stays `loop_ticks` (the clip's armed
    /// length), in ONE undo step. MIDI and control clips only; audio clips
    /// have no repeat semantics.
    pub(crate) fn timeline_tile_clips_batch(&mut self, tiles: Vec<timeline::ClipTileEdit>) {
        if tiles.is_empty() {
            return;
        }
        let before = self.begin_edit();
        let mut touched = Vec::new();
        {
            let project = self.project_mut();
            for e in &tiles {
                let changed = match e.sel.lane {
                    timeline::TimelineLane::Control => project
                        .control_tracks
                        .get_mut(e.sel.track)
                        .and_then(|t| t.clips.get_mut(e.sel.clip))
                        .map(|c| {
                            let new_span = e.span_ticks.max(1);
                            let changed = c.span_ticks != new_span;
                            c.span_ticks = new_span;
                            changed
                        })
                        .unwrap_or(false),
                    timeline::TimelineLane::Midi => project
                        .midi_tracks
                        .get_mut(e.sel.track)
                        .and_then(|t| t.clips.get_mut(e.sel.clip))
                        .map(|c| {
                            let new_span = e.span_ticks.max(1);
                            let changed = c.span_ticks != new_span;
                            c.span_ticks = new_span;
                            changed
                        })
                        .unwrap_or(false),
                    timeline::TimelineLane::Audio => false,
                };
                if changed && !touched.contains(&(e.sel.lane, e.sel.track)) {
                    touched.push((e.sel.lane, e.sel.track));
                }
            }
        }
        if !touched.is_empty() {
            self.commit_edit("Tile Clips", before);
            for (lane, track) in touched {
                self.sync_clip_row(lane, track);
            }
        }
    }

    /// Shift+D finalize: clones every source clip's content onto its
    /// target row/start in ONE undo step; the originals are untouched.
    /// The selection then moves to the new copies (they are appended, so
    /// their indices are the destination table's former lengths).
    pub(crate) fn timeline_duplicate_clips_batch(&mut self, dups: Vec<timeline::ClipDup>) {
        if dups.is_empty() {
            return;
        }
        let before = self.begin_edit();
        let mut new_sel = Vec::new();
        let mut touched = Vec::new();
        {
            let project = self.project_mut();
            for d in &dups {
                match d.from.lane {
                    timeline::TimelineLane::Control => {
                        let Some(mut c) = project
                            .control_tracks
                            .get(d.from.track)
                            .and_then(|t| t.clips.get(d.from.clip))
                            .cloned()
                        else {
                            continue;
                        };
                        let Some(t) = project.control_tracks.get_mut(d.to_track) else {
                            continue;
                        };
                        if t.clips.len() >= MAX_CLIPS_PER_TRACK {
                            continue;
                        }
                        c.start_ticks = d.start_ticks;
                        let index = t.clips.len();
                        t.clips.push(c);
                        new_sel.push(timeline::ClipSel {
                            lane: timeline::TimelineLane::Control,
                            track: d.to_track,
                            clip: index,
                        });
                        if !touched.contains(&(timeline::TimelineLane::Control, d.to_track)) {
                            touched.push((timeline::TimelineLane::Control, d.to_track));
                        }
                    }
                    timeline::TimelineLane::Midi => {
                        let Some(mut c) = project
                            .midi_tracks
                            .get(d.from.track)
                            .and_then(|t| t.clips.get(d.from.clip))
                            .cloned()
                        else {
                            continue;
                        };
                        let Some(t) = project.midi_tracks.get_mut(d.to_track) else {
                            continue;
                        };
                        if t.clips.len() >= MAX_CLIPS_PER_TRACK {
                            continue;
                        }
                        c.start_ticks = d.start_ticks;
                        let index = t.clips.len();
                        t.clips.push(c);
                        new_sel.push(timeline::ClipSel {
                            lane: timeline::TimelineLane::Midi,
                            track: d.to_track,
                            clip: index,
                        });
                        if !touched.contains(&(timeline::TimelineLane::Midi, d.to_track)) {
                            touched.push((timeline::TimelineLane::Midi, d.to_track));
                        }
                    }
                    // U11: an audio copy clones the seconds window (same path,
                    // same wav — no file duplication) at the new start.
                    timeline::TimelineLane::Audio => {
                        let Some(mut c) = project
                            .audio_tracks
                            .get(d.from.track)
                            .and_then(|t| t.clips.get(d.from.clip))
                            .cloned()
                        else {
                            continue;
                        };
                        let Some(t) = project.audio_tracks.get_mut(d.to_track) else {
                            continue;
                        };
                        if t.clips.len() >= MAX_CLIPS_PER_TRACK {
                            continue;
                        }
                        c.start_ticks = d.start_ticks;
                        let index = t.clips.len();
                        t.clips.push(c);
                        new_sel.push(timeline::ClipSel {
                            lane: timeline::TimelineLane::Audio,
                            track: d.to_track,
                            clip: index,
                        });
                        if !touched.contains(&(timeline::TimelineLane::Audio, d.to_track)) {
                            touched.push((timeline::TimelineLane::Audio, d.to_track));
                        }
                    }
                }
            }
        }
        if !new_sel.is_empty() {
            self.commit_edit("Duplicate Clips", before);
            for (lane, track) in touched {
                self.sync_clip_row(lane, track);
            }
            self.timeline_state.selection = new_sel;
        }
    }

    /// F-041 Ctrl+Shift+D: clone every clip and give each copy its own copy of
    /// the shared content. One undo step.
    pub(crate) fn timeline_duplicate_clips_with_data_batch(
        &mut self,
        dups: Vec<timeline::ClipDup>,
    ) {
        if dups.is_empty() {
            return;
        }
        let before = self.begin_edit();
        let mut new_sel = Vec::new();
        let mut touched = Vec::new();
        {
            let project = self.project_mut();
            for d in &dups {
                match d.from.lane {
                    timeline::TimelineLane::Midi => {
                        let Some(mut clip) = project
                            .midi_tracks
                            .get(d.from.track)
                            .and_then(|t| t.clips.get(d.from.clip))
                            .cloned()
                        else {
                            continue;
                        };
                        if project
                            .midi_tracks
                            .get(d.to_track)
                            .is_none_or(|t| t.clips.len() >= MAX_CLIPS_PER_TRACK)
                        {
                            continue;
                        }
                        let Some(data) = project.midi_data.get(clip.data as usize).cloned() else {
                            continue;
                        };
                        clip.data = project.push_midi_data(data);
                        clip.start_ticks = d.start_ticks;
                        let index = project.midi_tracks[d.to_track].clips.len();
                        project.midi_tracks[d.to_track].clips.push(clip);
                        new_sel.push(timeline::ClipSel {
                            lane: timeline::TimelineLane::Midi,
                            track: d.to_track,
                            clip: index,
                        });
                        if !touched.contains(&(timeline::TimelineLane::Midi, d.to_track)) {
                            touched.push((timeline::TimelineLane::Midi, d.to_track));
                        }
                    }
                    timeline::TimelineLane::Control => {
                        let Some(mut clip) = project
                            .control_tracks
                            .get(d.from.track)
                            .and_then(|t| t.clips.get(d.from.clip))
                            .cloned()
                        else {
                            continue;
                        };
                        if project
                            .control_tracks
                            .get(d.to_track)
                            .is_none_or(|t| t.clips.len() >= MAX_CLIPS_PER_TRACK)
                        {
                            continue;
                        }
                        let Some(data) = project.curve_data.get(clip.data as usize).cloned() else {
                            continue;
                        };
                        clip.data = project.push_curve_data(data);
                        clip.start_ticks = d.start_ticks;
                        let index = project.control_tracks[d.to_track].clips.len();
                        project.control_tracks[d.to_track].clips.push(clip);
                        new_sel.push(timeline::ClipSel {
                            lane: timeline::TimelineLane::Control,
                            track: d.to_track,
                            clip: index,
                        });
                        if !touched.contains(&(timeline::TimelineLane::Control, d.to_track)) {
                            touched.push((timeline::TimelineLane::Control, d.to_track));
                        }
                    }
                    timeline::TimelineLane::Audio => {
                        let Some(mut clip) = project
                            .audio_tracks
                            .get(d.from.track)
                            .and_then(|t| t.clips.get(d.from.clip))
                            .cloned()
                        else {
                            continue;
                        };
                        if project
                            .audio_tracks
                            .get(d.to_track)
                            .is_none_or(|t| t.clips.len() >= MAX_CLIPS_PER_TRACK)
                        {
                            continue;
                        }
                        clip.start_ticks = d.start_ticks;
                        let index = project.audio_tracks[d.to_track].clips.len();
                        project.audio_tracks[d.to_track].clips.push(clip);
                        new_sel.push(timeline::ClipSel {
                            lane: timeline::TimelineLane::Audio,
                            track: d.to_track,
                            clip: index,
                        });
                        if !touched.contains(&(timeline::TimelineLane::Audio, d.to_track)) {
                            touched.push((timeline::TimelineLane::Audio, d.to_track));
                        }
                    }
                }
            }
        }
        if !new_sel.is_empty() {
            self.commit_edit("Duplicate Clips with New Data", before);
            for (lane, track) in touched {
                self.sync_clip_row(lane, track);
            }
            self.timeline_state.selection = new_sel;
        }
    }

    /// F-040: snapshot the selected clips into the app clipboard. A detached
    /// value copy, not selection indices (undo would invalidate those).
    pub(crate) fn copy_timeline_selection(&mut self) {
        let selection = self.timeline_state.selection.clone();
        if selection.is_empty() {
            self.set_message("Copy: nothing selected".to_string());
            return;
        }
        let mut copies = Vec::new();
        {
            let project = self.project();
            for sel in selection {
                let clip = match sel.lane {
                    timeline::TimelineLane::Midi => project
                        .midi_tracks
                        .get(sel.track)
                        .and_then(|t| t.clips.get(sel.clip))
                        .and_then(|clip| project.midi_data_of(clip).map(|data| (clip, data)))
                        .map(|(clip, data)| {
                            clipboard::ClipSnapshot::Midi(clip.clone(), data.clone())
                        }),
                    timeline::TimelineLane::Control => project
                        .control_tracks
                        .get(sel.track)
                        .and_then(|t| t.clips.get(sel.clip))
                        .and_then(|clip| project.curve_data_of(clip).map(|data| (clip, data)))
                        .map(|(clip, data)| {
                            clipboard::ClipSnapshot::Control(clip.clone(), data.clone())
                        }),
                    timeline::TimelineLane::Audio => project
                        .audio_tracks
                        .get(sel.track)
                        .and_then(|t| t.clips.get(sel.clip))
                        .cloned()
                        .map(clipboard::ClipSnapshot::Audio),
                };
                let Some(clip) = clip else { continue };
                copies.push(clipboard::ClipCopy {
                    lane: sel.lane,
                    track: sel.track,
                    start: clip.start_ticks(),
                    clip,
                });
            }
        }
        if copies.is_empty() {
            self.set_message("Copy: nothing selected".to_string());
            return;
        }
        let count = copies.len();
        self.clipboard = Some(clipboard::Clipboard::Clips(copies));
        // F-040: leave a sentinel in the OS clipboard. egui-winit only
        // produces `Event::Paste` when the OS clipboard is non-empty.
        self.egui_ctx
            .copy_text(clipboard::sentinel("clip(s)", count));
        self.set_message(format!("Copied {count} clip(s)"));
    }

    /// F-040: paste the clipboard's clips at the playhead (spec §4.1). The
    /// earliest source clip lands exactly on the playhead; the rest keep their
    /// relative offsets. One undo step.
    pub(crate) fn paste_timeline_clips(&mut self) {
        let copies = match self.clipboard.as_ref() {
            Some(clipboard::Clipboard::Clips(copies)) => copies.clone(),
            Some(other) => {
                let label = other.label();
                self.set_message(format!("Paste: the clipboard holds {label} (not clips)"));
                return;
            }
            None => {
                self.set_message("Paste: the clipboard is empty".to_string());
                return;
            }
        };
        let anchor = self.engine.musical_ticks().max(0.0) as u32;
        let starts = clipboard::pasted_starts(&copies, anchor);
        let before = self.begin_edit();
        let mut new_sel = Vec::new();
        let mut touched = Vec::new();
        {
            let project = self.project_mut();
            for (copy, start_ticks) in copies.iter().zip(starts) {
                let lane = copy.lane;
                match &copy.clip {
                    clipboard::ClipSnapshot::Midi(source, data) => {
                        if project
                            .midi_tracks
                            .get(copy.track)
                            .is_none_or(|t| t.clips.len() >= MAX_CLIPS_PER_TRACK)
                        {
                            continue;
                        }
                        // The clipboard is a value snapshot: the paste gets a
                        // fresh pool entry, never a cross-project index.
                        let data_index = project.push_midi_data(data.clone());
                        let mut clip = source.clone();
                        clip.start_ticks = start_ticks;
                        clip.data = data_index;
                        let index = project.midi_tracks[copy.track].clips.len();
                        project.midi_tracks[copy.track].clips.push(clip);
                        new_sel.push(timeline::ClipSel {
                            lane,
                            track: copy.track,
                            clip: index,
                        });
                        let touched_row = (lane, copy.track);
                        if !touched.contains(&touched_row) {
                            touched.push(touched_row);
                        }
                    }
                    clipboard::ClipSnapshot::Control(source, data) => {
                        if project
                            .control_tracks
                            .get(copy.track)
                            .is_none_or(|t| t.clips.len() >= MAX_CLIPS_PER_TRACK)
                        {
                            continue;
                        }
                        let data_index = project.push_curve_data(data.clone());
                        let mut clip = source.clone();
                        clip.start_ticks = start_ticks;
                        clip.data = data_index;
                        let index = project.control_tracks[copy.track].clips.len();
                        project.control_tracks[copy.track].clips.push(clip);
                        new_sel.push(timeline::ClipSel {
                            lane,
                            track: copy.track,
                            clip: index,
                        });
                        let touched_row = (lane, copy.track);
                        if !touched.contains(&touched_row) {
                            touched.push(touched_row);
                        }
                    }
                    clipboard::ClipSnapshot::Audio(source) => {
                        let Some(t) = project.audio_tracks.get_mut(copy.track) else {
                            continue;
                        };
                        if t.clips.len() >= MAX_CLIPS_PER_TRACK {
                            continue;
                        }
                        let mut clip = source.clone();
                        clip.start_ticks = start_ticks;
                        let index = t.clips.len();
                        t.clips.push(clip);
                        new_sel.push(timeline::ClipSel {
                            lane,
                            track: copy.track,
                            clip: index,
                        });
                        let touched_row = (lane, copy.track);
                        if !touched.contains(&touched_row) {
                            touched.push(touched_row);
                        }
                    }
                }
            }
        }
        if new_sel.is_empty() {
            self.set_message("Paste: no destination tracks".to_string());
            return;
        }
        self.commit_edit("Paste Clips", before);
        for (lane, track) in touched {
            self.sync_clip_row(lane, track);
        }
        self.timeline_state.selection = new_sel;
        self.set_message("Pasted clips at the playhead".to_string());
    }

    /// Re-pushes one timeline row's clip table after an undoable edit.
    pub(crate) fn sync_clip_row(&mut self, lane: timeline::TimelineLane, index: usize) {
        match lane {
            timeline::TimelineLane::Control => self.send_control_clips(index),
            timeline::TimelineLane::Audio => self.send_audio_clips_for_track(index),
            timeline::TimelineLane::Midi => self.send_track_clips(index),
        }
    }

    /// Re-pushes every distinct row touched by a cross-lane selection.
    fn sync_selection_rows(&mut self, selection: &[timeline::ClipSel]) {
        let mut seen = Vec::new();
        for sel in selection {
            let key = (sel.lane, sel.track);
            if !seen.contains(&key) {
                seen.push(key);
                self.sync_clip_row(sel.lane, sel.track);
            }
        }
    }

    /// F-041: shared-content summary and the Share / Unlink entry points.
    pub(crate) fn clip_data_section(&mut self, ui: &mut egui::Ui) {
        let selection = self.timeline_state.selection.clone();
        let Some(first) = selection.first().copied() else {
            return;
        };
        let (data, refs, kind) = match first.lane {
            timeline::TimelineLane::Midi => {
                let Some(clip) = self
                    .project()
                    .midi_tracks
                    .get(first.track)
                    .and_then(|t| t.clips.get(first.clip))
                else {
                    return;
                };
                (clip.data, self.project().midi_data_refs(clip.data), "MIDI")
            }
            timeline::TimelineLane::Control => {
                let Some(clip) = self
                    .project()
                    .control_tracks
                    .get(first.track)
                    .and_then(|t| t.clips.get(first.clip))
                else {
                    return;
                };
                (
                    clip.data,
                    self.project().curve_data_refs(clip.data),
                    "curve",
                )
            }
            timeline::TimelineLane::Audio => return,
        };
        ui.separator();
        ui.label(format!(
            "Clip data · {kind} #{data} · shared by {refs} clip(s)"
        ));
        if refs > 1 {
            let unlink = ui
                .button("Unlink")
                .on_hover_text("Give this clip its own copy of the content");
            if unlink.clicked() {
                // Do not leave the keyboard parked on this button: the
                // timeline / roll surfaces only regain it on pointer hover.
                unlink.surrender_focus();
                self.unlink_selected_clip_data();
            }
        }
        if selection.len() > 1 && selection.iter().all(|sel| sel.lane == first.lane) {
            let link = ui
                .button("Link Data")
                .on_hover_text("Point every selected clip at the first one's content");
            if link.clicked() {
                link.surrender_focus();
                self.link_selected_clip_data();
            }
        }
    }

    /// F-041: point every selected clip at the first clip's content.
    pub(crate) fn link_selected_clip_data(&mut self) {
        let selection = self.timeline_state.selection.clone();
        let Some(first) = selection.first().copied() else {
            return;
        };
        let Some(data) = (match first.lane {
            timeline::TimelineLane::Midi => self
                .project()
                .midi_tracks
                .get(first.track)
                .and_then(|t| t.clips.get(first.clip))
                .map(|clip| clip.data),
            timeline::TimelineLane::Control => self
                .project()
                .control_tracks
                .get(first.track)
                .and_then(|t| t.clips.get(first.clip))
                .map(|clip| clip.data),
            timeline::TimelineLane::Audio => None,
        }) else {
            return;
        };
        let needs = selection.iter().any(|sel| match sel.lane {
            timeline::TimelineLane::Midi => self
                .project()
                .midi_tracks
                .get(sel.track)
                .and_then(|t| t.clips.get(sel.clip))
                .is_some_and(|clip| clip.data != data),
            timeline::TimelineLane::Control => self
                .project()
                .control_tracks
                .get(sel.track)
                .and_then(|t| t.clips.get(sel.clip))
                .is_some_and(|clip| clip.data != data),
            timeline::TimelineLane::Audio => false,
        });
        if !needs {
            return;
        }
        let before = self.begin_edit();
        {
            let project = self.project_mut();
            for sel in &selection {
                match sel.lane {
                    timeline::TimelineLane::Midi => {
                        if let Some(clip) = project
                            .midi_tracks
                            .get_mut(sel.track)
                            .and_then(|t| t.clips.get_mut(sel.clip))
                        {
                            clip.data = data;
                        }
                    }
                    timeline::TimelineLane::Control => {
                        if let Some(clip) = project
                            .control_tracks
                            .get_mut(sel.track)
                            .and_then(|t| t.clips.get_mut(sel.clip))
                        {
                            clip.data = data;
                        }
                    }
                    timeline::TimelineLane::Audio => {}
                }
            }
        }
        self.commit_edit("Link Clip Data", before);
        self.sync_selection_rows(&selection);
    }

    /// F-041: give every selected shared clip its own copy of the content.
    pub(crate) fn unlink_selected_clip_data(&mut self) {
        let selection = self.timeline_state.selection.clone();
        if selection.is_empty() {
            return;
        }
        // Only clips whose content is still shared are touched; a lone clip
        // must not spend an undo step (and must not get a pointless copy).
        let needs = selection.iter().any(|sel| match sel.lane {
            timeline::TimelineLane::Midi => self
                .project()
                .midi_tracks
                .get(sel.track)
                .and_then(|t| t.clips.get(sel.clip))
                .is_some_and(|clip| self.project().midi_data_refs(clip.data) > 1),
            timeline::TimelineLane::Control => self
                .project()
                .control_tracks
                .get(sel.track)
                .and_then(|t| t.clips.get(sel.clip))
                .is_some_and(|clip| self.project().curve_data_refs(clip.data) > 1),
            timeline::TimelineLane::Audio => false,
        });
        if !needs {
            return;
        }
        let before = self.begin_edit();
        {
            let project = self.project_mut();
            for sel in &selection {
                match sel.lane {
                    timeline::TimelineLane::Midi => {
                        let Some(data) = project
                            .midi_tracks
                            .get(sel.track)
                            .and_then(|t| t.clips.get(sel.clip))
                            .map(|clip| clip.data)
                        else {
                            continue;
                        };
                        let refs = project
                            .midi_tracks
                            .iter()
                            .flat_map(|t| t.clips.iter())
                            .filter(|clip| clip.data == data)
                            .count();
                        if refs <= 1 {
                            continue;
                        }
                        let Some(source) = project.midi_data.get(data as usize).cloned() else {
                            continue;
                        };
                        let new_data = project.push_midi_data(source);
                        if let Some(clip) = project
                            .midi_tracks
                            .get_mut(sel.track)
                            .and_then(|t| t.clips.get_mut(sel.clip))
                        {
                            clip.data = new_data;
                        }
                    }
                    timeline::TimelineLane::Control => {
                        let Some(data) = project
                            .control_tracks
                            .get(sel.track)
                            .and_then(|t| t.clips.get(sel.clip))
                            .map(|clip| clip.data)
                        else {
                            continue;
                        };
                        let refs = project
                            .control_tracks
                            .iter()
                            .flat_map(|t| t.clips.iter())
                            .filter(|clip| clip.data == data)
                            .count();
                        if refs <= 1 {
                            continue;
                        }
                        let Some(source) = project.curve_data.get(data as usize).cloned() else {
                            continue;
                        };
                        let new_data = project.push_curve_data(source);
                        if let Some(clip) = project
                            .control_tracks
                            .get_mut(sel.track)
                            .and_then(|t| t.clips.get_mut(sel.clip))
                        {
                            clip.data = new_data;
                        }
                    }
                    timeline::TimelineLane::Audio => {}
                }
            }
        }
        self.commit_edit("Unlink Clip Data", before);
        self.sync_selection_rows(&selection);
    }
}
