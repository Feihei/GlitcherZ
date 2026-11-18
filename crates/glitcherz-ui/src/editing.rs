//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    /// Points the Edit tab at the MIDI clip `(track, clip)`, dropping any
    /// in-flight roll gesture: selection indices refer to the clip that was
    /// being edited.
    pub(crate) fn set_edit_target(&mut self, track: usize, clip: usize) {
        self.set_edit_focus(piano_roll::ClipRef::midi(track, clip));
    }

    /// Points the Editor at the control clip `(track, clip)` — F-049: the
    /// curve is now a focus of the **same** roll canvas, not a separate form.
    pub(crate) fn set_control_edit_target(&mut self, track: usize, clip: usize) {
        self.set_edit_focus(piano_roll::ClipRef::control(track, clip));
    }

    /// Builds the roll subject around `focus` (P3, spec `roll-multiclip.md`
    /// D5): the timeline's clip selection is the ghost context, whatever lane
    /// each clip lives in (F-049 added the control focus).
    ///
    /// `set_edit_target` / `set_control_edit_target` are the two entries;
    /// both are view-state changes only — no command, no undo entry.
    fn set_edit_focus(&mut self, focus: piano_roll::ClipRef) {
        // Clears the gesture *and* the note selection (the indices belong to
        // whichever clip used to be focused).
        self.roll_state.clear_gesture();
        let selection: Vec<piano_roll::ClipRef> = self
            .timeline_state
            .selection
            .iter()
            .filter_map(|sel| match sel.lane {
                timeline::TimelineLane::Midi => {
                    Some(piano_roll::ClipRef::midi(sel.track, sel.clip))
                }
                timeline::TimelineLane::Control => {
                    Some(piano_roll::ClipRef::control(sel.track, sel.clip))
                }
                timeline::TimelineLane::Audio => None,
            })
            .collect();
        let subject = {
            let project = self.project();
            piano_roll::RollSubject::assemble(
                focus,
                &selection,
                |track| project.midi_tracks.get(track).map_or(0, |t| t.clips.len()),
                |track| {
                    project
                        .control_tracks
                        .get(track)
                        .map_or(0, |t| t.clips.len())
                },
            )
        };
        let dropped = subject.dropped();
        self.edit_clip = Some(subject);
        self.edit_selection
            .clone_from(&self.timeline_state.selection);
        if dropped > 0 {
            self.set_message(format!(
                "{dropped} clip(s) not shown: the roll holds at most {}",
                piano_roll::MAX_ROLL_CONTEXT
            ));
        }
    }

    /// Retargets the roll's focus inside its current context (P3 D7): a pure
    /// view-state change — no command, no undo entry, no document touch. The
    /// note selection is dropped because its indices belong to the old focus.
    pub(crate) fn set_roll_focus(&mut self, clip: piano_roll::ClipRef) {
        let Some(subject) = &mut self.edit_clip else {
            return;
        };
        if subject.focus() == clip || !subject.set_focus(clip) {
            return;
        }
        self.roll_state.clear_gesture();
    }

    /// Re-derives the Edit tab's target from the timeline's clip selection.
    ///
    /// The selection is the single source of truth — the same rule the
    /// Inspector follows. Opening the Edit tab therefore shows the selected
    /// clip without a double-click, and deselecting (or deleting) the clip
    /// takes the editor away instead of leaving it on a dead index.
    ///
    /// Only a **changed** selection re-assembles the roll: `edit_selection`
    /// snapshots what the current target was built from, so the roll's own
    /// focus switches (chip strip, Tab) survive (spec `roll-multiclip.md`
    /// E15 — assembly happens on open, focus moves are view state). The most
    /// recently selected clip wins, since that is the one a double-click on
    /// the same spot would have opened; audio clips have no editor and are
    /// skipped (their selection alone leaves the tab on its hint).
    pub(crate) fn sync_edit_target(&mut self) {
        if self.edit_clip.is_some() && self.timeline_state.selection == self.edit_selection {
            return;
        }
        self.edit_selection
            .clone_from(&self.timeline_state.selection);
        // Live check: a selection outlives a clip or track removal, and the
        // editor must not point at a clip that no longer exists.
        let primary = {
            let project = self.session.project();
            self.edit_selection
                .iter()
                .rev()
                .copied()
                .find(|sel| match sel.lane {
                    timeline::TimelineLane::Midi => project
                        .midi_tracks
                        .get(sel.track)
                        .is_some_and(|t| sel.clip < t.clips.len()),
                    timeline::TimelineLane::Control => project
                        .control_tracks
                        .get(sel.track)
                        .is_some_and(|t| sel.clip < t.clips.len()),
                    timeline::TimelineLane::Audio => false,
                })
        };
        let Some(primary) = primary else {
            // Nothing editable selected: the tab shows its "pick a clip" hint.
            if self.edit_clip.take().is_some() {
                self.roll_state.clear_gesture();
            }
            return;
        };
        match primary.lane {
            timeline::TimelineLane::Midi => self.set_edit_target(primary.track, primary.clip),
            timeline::TimelineLane::Control => {
                self.set_control_edit_target(primary.track, primary.clip);
            }
            // Filtered out above.
            timeline::TimelineLane::Audio => {}
        }
    }

    /// F-048: one chip per editable clip in the timeline selection — MIDI and
    /// curve clips side by side.
    ///
    /// The tab used to follow only the **last** selected clip, so a mixed
    /// selection (a MIDI clip plus a curve clip) could not reach the curve
    /// editor without re-selecting it in the Timeline. With two or more
    /// editable clips the strip appears; a single one keeps the plain header
    /// (the roll's own chip strip already covers multi-clip focus).
    pub(crate) fn edit_target_strip(&mut self, ui: &mut egui::Ui) {
        let editable: Vec<(timeline::ClipSel, String)> = {
            let project = self.project();
            self.edit_selection
                .iter()
                .filter_map(|sel| match sel.lane {
                    timeline::TimelineLane::Midi => {
                        let clip = project.midi_tracks.get(sel.track)?.clips.get(sel.clip)?;
                        let name = self.track_label(sel.track);
                        Some((
                            *sel,
                            format!(
                                "{name} · bar {}",
                                project.bar_beat_at(clip.start_ticks).0 + 1
                            ),
                        ))
                    }
                    timeline::TimelineLane::Control => {
                        let clip = project.control_tracks.get(sel.track)?.clips.get(sel.clip)?;
                        let name = project.control_tracks[sel.track].name.clone();
                        let name = if name.is_empty() {
                            format!("Curve {}", sel.track + 1)
                        } else {
                            name
                        };
                        Some((
                            *sel,
                            format!(
                                "{name} · bar {}",
                                project.bar_beat_at(clip.start_ticks).0 + 1
                            ),
                        ))
                    }
                    timeline::TimelineLane::Audio => None,
                })
                .collect()
        };
        if editable.len() < 2 {
            return;
        }
        let current = self.edit_clip.as_ref().map(|subject| subject.focus());
        let mut pick: Option<timeline::ClipSel> = None;
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::Label::new(egui::RichText::new("editing").weak()));
            for (sel, label) in &editable {
                let lane = match sel.lane {
                    timeline::TimelineLane::Midi => piano_roll::RollLane::Midi,
                    timeline::TimelineLane::Control => piano_roll::RollLane::Control,
                    timeline::TimelineLane::Audio => continue,
                };
                let chosen = current.is_some_and(|focus| {
                    focus.lane == lane && focus.track == sel.track && focus.clip == sel.clip
                });
                if ui
                    .selectable_label(chosen, label)
                    .on_hover_text("Edit this clip in the Editor")
                    .clicked()
                    && !chosen
                {
                    pick = Some(*sel);
                }
            }
        });
        if let Some(sel) = pick {
            match sel.lane {
                timeline::TimelineLane::Midi => self.set_edit_target(sel.track, sel.clip),
                timeline::TimelineLane::Control => {
                    self.set_control_edit_target(sel.track, sel.clip);
                }
                timeline::TimelineLane::Audio => {}
            }
        }
    }

    /// One-line chip text for a context clip (P3; F-049 covers both lanes):
    /// "track · bar N", so the chips read the same for notes and curves.
    fn clip_chip_label(&self, clip: piano_roll::ClipRef) -> Option<String> {
        let project = self.project();
        match clip.lane {
            piano_roll::RollLane::Midi => {
                let c = project.midi_tracks.get(clip.track)?.clips.get(clip.clip)?;
                Some(format!(
                    "{} · bar {}",
                    self.track_label(clip.track),
                    project.bar_beat_at(c.start_ticks).0 + 1
                ))
            }
            piano_roll::RollLane::Control => {
                let c = project
                    .control_tracks
                    .get(clip.track)?
                    .clips
                    .get(clip.clip)?;
                let name = &project.control_tracks[clip.track].name;
                let name = if name.is_empty() {
                    format!("Curve {}", clip.track + 1)
                } else {
                    name.clone()
                };
                Some(format!(
                    "{name} · bar {}",
                    project.bar_beat_at(c.start_ticks).0 + 1
                ))
            }
        }
    }

    /// Body of the Editor (Edit tab): **one** roll canvas for both lanes
    /// (F-049).
    ///
    /// A MIDI focus edits notes, with the other context clips as ghosts and
    /// the selected curve clips as dimmed (read-only) lanes. A control focus
    /// flips that: its curve lane is the writable subject and takes the tall
    /// strip, while every MIDI clip of the context draws as a ghost. Switching
    /// the focus is the chip strip or Ctrl+Tab — the canvas, its time axis and
    /// its scroll position stay put, exactly like several selected MIDI clips.
    pub(crate) fn editor_body(&mut self, ui: &mut egui::Ui, subject: &piano_roll::RollSubject) {
        let focus = subject.focus();
        let (track, clip_index) = (focus.track, focus.clip);
        let on_notes = matches!(focus.lane, piano_roll::RollLane::Midi);
        let clip_count = if on_notes {
            if self.project().midi_tracks.is_empty() {
                // The target track was removed underneath the editor.
                self.edit_clip = None;
                return;
            }
            let sel = track.min(self.project().midi_tracks.len() - 1);
            if self.project().midi_tracks[sel].clips.is_empty() {
                let before = self.begin_edit();
                self.project_mut().add_midi_clip(sel, 0);
                self.commit_edit("New Clip", before);
                self.send_track_clips(sel);
            }
            self.project().midi_tracks[sel].clips.len()
        } else {
            let Some(tracks) = self.project().control_tracks.get(track) else {
                self.edit_clip = None;
                return;
            };
            tracks.clips.len()
        };
        let sel = if on_notes {
            track.min(self.project().midi_tracks.len().saturating_sub(1))
        } else {
            track
        };
        // A clip deleted underneath the editor (an undo, or a removal from a
        // path that did not clear the selection): close instead of silently
        // editing a neighbour — `sync_edit_target` re-derives the target from
        // the selection on the next frame, and falls back to the hint when
        // nothing live is left there.
        if clip_index >= clip_count {
            self.edit_clip = None;
            return;
        }
        // Met M4: the roll's bar lines follow the meter segments.
        let meter = self.project().meter_map();
        let name = if on_notes {
            self.track_label(sel)
        } else {
            let name = self.project().control_tracks[track].name.clone();
            if name.is_empty() {
                format!("Curve {}", track + 1)
            } else {
                name
            }
        };
        let shared_refs = if on_notes {
            self.project()
                .midi_tracks
                .get(sel)
                .and_then(|t| t.clips.get(clip_index))
                .map(|clip| self.project().midi_data_refs(clip.data))
                .unwrap_or(1)
        } else {
            1
        };
        let mut goto: Option<usize> = None;
        ui.horizontal(|ui| {
            let mut title = format!("{name} · clip {} (of {clip_count})", clip_index + 1);
            if shared_refs > 1 {
                title = format!("{title} · shared by {shared_refs} clips");
            }
            if !on_notes {
                title = format!("{title} · curve");
            }
            ui.label(title);
            if ui
                .add_enabled(clip_index > 0, egui::Button::new("←"))
                .on_hover_text("Previous clip")
                .clicked()
            {
                goto = Some(clip_index - 1);
            }
            if ui
                .add_enabled(clip_index + 1 < clip_count, egui::Button::new("→"))
                .on_hover_text("Next clip")
                .clicked()
            {
                goto = Some(clip_index + 1);
            }
        });
        if let Some(next) = goto {
            if on_notes {
                self.set_edit_target(sel, next);
            } else {
                self.set_control_edit_target(track, next);
            }
        }
        // P3 (D7): the focus chip strip. Ghosts are read-only and never take
        // clicks, so focus is switched *here* — a chip click or Tab/Shift+Tab.
        // Clicking a ghost would re-open the "empty drag = marquee" ambiguity
        // M4 spent a whole spec closing. F-049: the chips cover both lanes.
        if subject.context().len() > 1 {
            let mut switch: Option<piano_roll::ClipRef> = None;
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Label::new(egui::RichText::new("showing").weak()));
                for clip in subject.context() {
                    let Some(label) = self.clip_chip_label(*clip) else {
                        continue;
                    };
                    let focused = *clip == focus;
                    if ui
                        .selectable_label(focused, label)
                        .on_hover_text(if focused {
                            if on_notes {
                                "Editing this clip (writable)"
                            } else {
                                "Editing this curve (writable)"
                            }
                        } else {
                            "Click (or Ctrl+Tab) to edit this clip · it is dimmed until then"
                        })
                        .clicked()
                        && !focused
                    {
                        switch = Some(*clip);
                    }
                }
            });
            if let Some(clip) = switch {
                self.set_roll_focus(clip);
            }
            // Ctrl+Tab / Ctrl+Shift+Tab cycle the focus (D7). Consumed here so
            // egui's focus traversal never steals it, and so the app-wide view
            // cycle — which owns the bare Tab — leaves this alone. The roll
            // itself never reads a bare Tab.
            let backward = ui.input_mut(|i| {
                i.consume_key(
                    egui::Modifiers {
                        ctrl: true,
                        shift: true,
                        ..egui::Modifiers::NONE
                    },
                    egui::Key::Tab,
                )
            });
            let forward =
                !backward && ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab));
            if backward || forward {
                if let Some(subject) = &mut self.edit_clip {
                    subject.cycle_focus(backward);
                }
                self.roll_state.clear_gesture();
            }
        }
        // Event List (M3.6): spreadsheet-style precision editing. It is a
        // resizable bottom strip, not a block stacked under the roll — the roll
        // takes the rest of the tab now, so anything placed after it would be
        // laid out off-screen. The split defaults to a third of the tab
        // (two thirds stay with the roll) and the divider is draggable; pulling
        // it below the minimum collapses the strip to its caption row, as does
        // clicking the caption. F-049: a curve focus has no notes to tabulate.
        if on_notes {
            let avail_h = ui.available_height();
            let first_h =
                (avail_h * EVENT_LIST_HEIGHT_FRACTION).clamp(EVENT_LIST_MIN_H, EVENT_LIST_MAX_H);
            let collapsed = egui::Panel::bottom("glitcherz_event_list_collapsed");
            // A fresh id: the old collapsible block stored its ~190 px height under
            // the plain name, and a stale size would open the strip at the floor.
            let expanded = egui::Panel::bottom("glitcherz_event_list_expanded")
                .resizable(true)
                .default_size(first_h)
                .min_size(EVENT_LIST_MIN_H)
                .max_size((avail_h * 0.75).max(EVENT_LIST_MIN_H));
            // `show_switched` owns the `bool` for the duration of the call, so the
            // caption writes into a local and the field is updated after.
            let mut open = self.event_list_open;
            let mut toggle = false;
            egui::Panel::show_switched(ui, &mut open, collapsed, expanded, |ui, expanded| {
                ui.horizontal(|ui| {
                    let count = self
                        .project()
                        .midi_clip_notes(sel, clip_index)
                        .map_or(0, |notes| notes.len());
                    if ui
                        .selectable_label(expanded, "Event List")
                        .on_hover_text(if expanded {
                            "Click to collapse · drag the divider to resize"
                        } else {
                            "Click to open the note table"
                        })
                        .clicked()
                    {
                        toggle = true;
                    }
                    ui.weak(format!("{count} notes"));
                });
                if !expanded {
                    return;
                }
                let list_intent = {
                    let notes = self
                        .project()
                        .midi_clip_notes(sel, clip_index)
                        .unwrap_or(&[]);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| event_list::show(ui, notes))
                        .inner
                };
                if let Some(list_intent) = list_intent {
                    self.apply_event_list_intent(sel, clip_index, list_intent);
                }
            });
            if toggle {
                open = !open;
            }
            self.event_list_open = open;
        }
        let playhead = self.engine.musical_ticks();
        let playing = self.engine.is_playing();
        let intent = {
            // Destructured: `project()` borrows all of `self`, which would
            // collide with the `&mut self.roll_state` below.
            let Self {
                session,
                roll_state,
                focus_router: router,
                ..
            } = self;
            let project = session.project();
            // P4: one overlay lane per control clip in the context. F-049:
            // the lane matching the focus is the writable one; every other
            // lane is dimmed scenery, so "which of these am I editing" needs
            // no guessing. The caption names the destination (its range comes
            // from the same map table the engine uses).
            let lanes: Vec<piano_roll::OverlayLane<'_>> = subject
                .context()
                .iter()
                .filter(|c| matches!(c.lane, piano_roll::RollLane::Control))
                .filter_map(|c| {
                    let data = project.control_tracks.get(c.track)?.clips.get(c.clip)?;
                    let curve = project.curve_data_of(data)?;
                    let dest = match project.graph.auto_target(c.track) {
                        Some((port, param)) => {
                            crate::patch::auto_param_label_at(&param, &project.graph, &port)
                        }
                        None => "Not connected".to_string(),
                    };
                    let name = if project.control_tracks[c.track].name.is_empty() {
                        format!("Curve {}", c.track + 1)
                    } else {
                        project.control_tracks[c.track].name.clone()
                    };
                    let focused = *c == focus;
                    // F-049: the focused lane is the old curve editor's
                    // successor, so it also carries the axis caption (the real
                    // unit range) the canvas used to print.
                    let label = if focused {
                        let map = match project.graph.auto_target(c.track) {
                            Some((port, param)) => {
                                crate::curve::map_in(&param, &project.graph, &port)
                            }
                            None => glitcherz_model::MapSpec::UNIT,
                        };
                        format!("{name} · {dest} · {}", crate::curve::axis_caption(map))
                    } else {
                        format!("{name} · {dest}")
                    };
                    Some(piano_roll::OverlayLane {
                        clip: *c,
                        data,
                        curve,
                        label,
                        focused,
                    })
                })
                .collect();
            piano_roll::show(
                ui,
                &project.midi_tracks,
                &project.midi_data,
                &lanes,
                subject,
                roll_state,
                playhead,
                playing,
                &meter,
                router,
            )
        };
        // I2: audition events bypass the command layer entirely — no undo
        // entry, no document change. They go to the focus track's
        // instrument (the same fan-out the recording path listens to). A
        // curve focus has no instrument to audition.
        if on_notes {
            for (note, velocity, on) in self.roll_state.take_preview_events() {
                self.engine.send(EngineCommand::Note {
                    track: sel,
                    event: if on {
                        NoteEvent::On { note, velocity }
                    } else {
                        NoteEvent::Off { note }
                    },
                });
            }
        } else {
            self.roll_state.take_preview_events();
        }
        // Intents run as commands, so every roll edit is undoable. A drag
        // emits one intent per frame under the same gesture key: one drag,
        // one undo step. Curve intents carry their own `(track, clip)`, so the
        // indices here only matter to note commands (never emitted with a
        // curve focus — the roll drops them).
        if let Some(intent) = intent {
            self.apply_roll_intent(sel, clip_index, intent);
        }
        // A no-op explanation (e.g. an armed transform with an empty
        // selection) reaches the app's status bar.
        if let Some(status) = self.roll_state.take_status() {
            self.set_message(status);
        }
    }

    /// Runs an Event List edit as a command (M1.4 contract).
    pub(crate) fn apply_event_list_intent(
        &mut self,
        track: usize,
        clip: usize,
        intent: event_list::EventListIntent,
    ) {
        use serde_json::json;
        let result = match intent {
            event_list::EventListIntent::Edit(edit) => {
                // Only pass the fields the drag actually touched.
                let mut params = json!({
                    "track": track, "clip": clip, "index": edit.index,
                });
                let object = params.as_object_mut().expect("object literal");
                if let Some(start) = edit.start {
                    object.insert("start_ticks".into(), json!(start));
                }
                if let Some(note) = edit.note {
                    object.insert("note".into(), json!(note));
                }
                if let Some(velocity) = edit.velocity {
                    object.insert("velocity".into(), json!(velocity));
                }
                if let Some(length) = edit.length {
                    object.insert("length_ticks".into(), json!(length));
                }
                self.execute_merged(
                    "midi.note_edit",
                    &params,
                    &format!("event:{track}:{clip}:{}", edit.index),
                )
            }
            event_list::EventListIntent::Delete { index } => self.execute(
                "midi.note_delete",
                &json!({ "track": track, "clip": clip, "indices": [index] }),
            ),
        };
        if result.is_ok() {
            self.sync_midi_data_of(track, clip);
        } else if let Err(error) = result {
            self.set_message(format!("Edit failed: {error}"));
        }
    }

    /// W6: runs one Notes-menu MIDI command and rebuilds the roll selection
    /// from its `indices` reply. The command layer owns undo (one step per
    /// click / shortcut) and re-sorts the clip, so the reply is authoritative.
    pub(crate) fn apply_midi_op(
        &mut self,
        op: piano_roll::MidiOp,
        indices: Vec<usize>,
        track: usize,
        clip: usize,
    ) {
        use serde_json::json;
        let (id, params, label) = match op {
            piano_roll::MidiOp::Quantize {
                strength,
                grid_ticks,
            } => (
                "midi.quantize",
                json!({
                    "track": track,
                    "clip": clip,
                    "indices": indices,
                    "grid_ticks": grid_ticks,
                    "strength": strength.clamp(0.0, 1.0),
                }),
                "Quantize",
            ),
            piano_roll::MidiOp::Legato { gap } => (
                "midi.legato",
                json!({ "track": track, "clip": clip, "indices": indices, "gap": gap.max(0) }),
                "Legato",
            ),
            piano_roll::MidiOp::Glue { gap } => (
                "midi.glue",
                json!({ "track": track, "clip": clip, "indices": indices, "gap": gap.max(0) }),
                "Glue",
            ),
            piano_roll::MidiOp::RemoveOverlap => (
                "midi.remove_overlap",
                json!({ "track": track, "clip": clip, "indices": indices }),
                "Remove Overlap",
            ),
            piano_roll::MidiOp::Transpose(semitones) => (
                "midi.transpose",
                json!({
                    "track": track,
                    "clip": clip,
                    "indices": indices,
                    "semitones": semitones,
                }),
                "Transpose",
            ),
        };
        match self.execute(id, &params) {
            Ok(value) => {
                if let Some(list) = value.get("indices").and_then(serde_json::Value::as_array) {
                    self.roll_state.selection = list
                        .iter()
                        .filter_map(serde_json::Value::as_u64)
                        .map(|index| index as usize)
                        .collect();
                }
                self.set_message(label.to_string());
            }
            Err(error) => self.set_message(format!("{label} failed: {error}")),
        }
    }

    /// F-040: snapshot the selected notes into the app clipboard (clip-local
    /// ticks + the earliest start, so a paste can realign them).
    pub(crate) fn copy_roll_notes(&mut self, track: usize, clip: usize) {
        let selection = self.roll_state.selection.clone();
        if selection.is_empty() {
            self.set_message("Copy: nothing selected".to_string());
            return;
        }
        let notes: Vec<glitcherz_model::Note> = {
            let Some(source) = self.project().midi_clip_notes(track, clip) else {
                return;
            };
            selection
                .iter()
                .filter_map(|index| source.get(*index))
                .copied()
                .collect()
        };
        if notes.is_empty() {
            self.set_message("Copy: nothing selected".to_string());
            return;
        }
        let min_start = notes.iter().map(|n| n.start).min().unwrap_or(0);
        let count = notes.len();
        self.clipboard = Some(clipboard::Clipboard::Notes(clipboard::NotesCopy {
            notes,
            min_start,
        }));
        // F-040: sentinel so the next Ctrl+V produces a paste event.
        self.egui_ctx
            .copy_text(clipboard::sentinel("note(s)", count));
        self.set_message(format!("Copied {count} note(s)"));
    }

    /// F-040: paste the clipboard's notes at the playhead (spec §4.2). The
    /// playhead is converted to clip-local ticks; the earliest pasted note
    /// lands there and the rest keep their relative offsets. One undo step.
    pub(crate) fn paste_roll_notes(&mut self, track: usize, clip: usize) {
        let copy = match self.clipboard.as_ref() {
            Some(clipboard::Clipboard::Notes(copy)) => copy.clone(),
            Some(other) => {
                let label = other.label();
                self.set_message(format!("Paste: the clipboard holds {label} (not notes)"));
                return;
            }
            None => {
                self.set_message("Paste: the clipboard is empty".to_string());
                return;
            }
        };
        let Some(clip_start) = self
            .project()
            .midi_tracks
            .get(track)
            .and_then(|t| t.clips.get(clip))
            .map(|c| c.start_ticks)
        else {
            return;
        };
        let anchor = self.engine.musical_ticks() as i64 - i64::from(clip_start);
        let pasted = clipboard::pasted_notes(&copy, anchor.max(0));
        let before = self.begin_edit();
        let indices = {
            let Some(data) = self.project_mut().midi_clip_data_mut(track, clip) else {
                return;
            };
            data.notes_mut().extend(pasted.iter().copied());
            data.sort();
            clipboard::locate_notes(data.notes(), &pasted)
        };
        self.commit_edit("Paste Notes", before);
        let count = indices.len();
        self.roll_state.selection = indices;
        self.set_message(format!("Pasted {count} note(s) at the playhead"));
    }

    /// Runs a piano-roll edit as a command (M1.4: no direct document mutation).
    pub(crate) fn apply_roll_intent(
        &mut self,
        track: usize,
        clip: usize,
        intent: piano_roll::RollIntent,
    ) {
        use piano_roll::RollIntent;
        use serde_json::json;

        // P4: an overlay lane writes **its own** control clip, so these never
        // fall through to the note commands (which target the focus clip) nor
        // to the MIDI clip push at the bottom.
        match &intent {
            RollIntent::Seek { tick } => {
                // Aud series: a ruler seek is transient — no document change,
                // no undo entry (it only moves the transport).
                self.engine.seek_ticks(*tick as f64);
                return;
            }
            RollIntent::CopyNotes => {
                self.copy_roll_notes(track, clip);
                return;
            }
            RollIntent::PasteNotes => {
                self.paste_roll_notes(track, clip);
                return;
            }
            RollIntent::MidiOp { op, indices } => {
                // W6: the Notes menu / shortcut path. The command layer owns
                // undo (one step per click) and returns the new selection.
                self.apply_midi_op(*op, indices.clone(), track, clip);
                return;
            }
            RollIntent::CurveUpsert {
                track,
                clip,
                from,
                tick,
                value,
                dragging,
            } => {
                self.curve_upsert(*track, *clip, *from, *tick, *value, *dragging);
                return;
            }
            RollIntent::CurveRemove { track, clip, tick } => {
                self.curve_remove_point(*track, *clip, *tick);
                return;
            }
            RollIntent::CurveDragEnd => {
                self.end_param_edit();
                return;
            }
            RollIntent::CurveInterp {
                track,
                clip,
                interp,
            } => {
                self.set_clip_interp(*track, *clip, *interp);
                return;
            }
            _ => {}
        }

        let mut selection_changed = false;
        // Draw gestures share one undo key across their frames: the creating
        // `note_add` and every resize frame collapse into a single step (one
        // gesture = one undo).
        let draw_key = |gesture: u64| format!("roll-draw:{track}:{clip}:{gesture}");
        let mut created: Option<(usize, i64, i64)> = None;
        let result = match intent {
            // Handled by the early transient-intent branch above; kept
            // explicit so a new transient intent cannot silently fall through
            // into the command layer.
            RollIntent::Seek { .. } => return,
            RollIntent::MidiOp { .. } => return,
            // F-049: the curve tool's interpolation moved to the command
            // layer's `set_clip_interp`, above.
            RollIntent::CurveInterp { .. } => return,
            RollIntent::CopyNotes | RollIntent::PasteNotes => {
                unreachable!("clipboard intents never reach the note command table")
            }
            RollIntent::AddNote {
                note,
                start,
                length,
                gesture,
            } => {
                let outcome = self.execute_merged(
                    "midi.note_add",
                    &json!({
                        "track": track, "clip": clip,
                        "note": note, "start_ticks": start, "length_ticks": length,
                    }),
                    &draw_key(gesture),
                );
                // The new note becomes the whole selection (its sorted index
                // is authoritative; the old selection's indices are stale).
                // resolve_created also starts the draw-drag if the creation
                // came from a press-draw on empty space. The geometry travels
                // with the index so the drag can anchor on it without
                // indexing into the freshly re-sorted clip.
                created = outcome
                    .as_ref()
                    .ok()
                    .and_then(|value| value["index"].as_u64())
                    .map(|index| (index as usize, start, length));
                outcome
            }
            RollIntent::ResizeEnd {
                delta_ticks,
                gesture,
            } => {
                let outcome = self.execute_merged(
                    "midi.notes_edit_delta",
                    &json!({
                        "track": track, "clip": clip, "indices": self.roll_state.selection.clone(),
                        "delta_length": delta_ticks,
                    }),
                    &draw_key(gesture),
                );
                self.reselect_from(&outcome);
                outcome
            }
            RollIntent::ResizeStart {
                delta_ticks,
                gesture,
            } => {
                // Ends follow the starts (mirrored length delta) — that is
                // what a left-edge trim means.
                let outcome = self.execute_merged(
                    "midi.notes_edit_delta",
                    &json!({
                        "track": track, "clip": clip, "indices": self.roll_state.selection.clone(),
                        "delta_start": delta_ticks, "delta_length": -delta_ticks,
                    }),
                    &draw_key(gesture),
                );
                self.reselect_from(&outcome);
                outcome
            }
            RollIntent::MoveNotes {
                indices,
                delta_start,
                delta_note,
                gesture,
            } => {
                // Merged: consecutive frames of one drag collapse into a
                // single undo step (per-gesture key); a burst of arrow-key
                // nudges merges under a shared key. The command reports where
                // the moved notes landed (crossing unselected notes permutes
                // the sort), and the selection follows them.
                let key = match gesture {
                    Some(id) => format!("roll-move:{track}:{clip}:{id}"),
                    None => format!("roll-nudge:{track}:{clip}"),
                };
                let outcome = self.execute_merged(
                    "midi.notes_move",
                    &json!({
                        "track": track, "clip": clip, "indices": indices,
                        "ticks": delta_start, "semitones": delta_note,
                    }),
                    &key,
                );
                self.reselect_from(&outcome);
                outcome
            }
            RollIntent::Duplicate { indices, gesture } => {
                // Ctrl + drag: copy the selection, select the copies (the
                // command reports their indices), and merge the copy with the
                // move frames of the same drag — one press = one undo step.
                let outcome = self.execute_merged(
                    "midi.notes_duplicate",
                    &json!({ "track": track, "clip": clip, "indices": indices }),
                    &format!("roll-move:{track}:{clip}:{gesture}"),
                );
                self.reselect_from(&outcome);
                outcome
            }
            RollIntent::ScaleNotes {
                pivot_tick,
                factor,
                gesture,
            } => {
                // The Scale command's drag: frames merge under a per-gesture
                // key (one drag = one undo step), and the command reports
                // where the scaled notes landed after the re-sort.
                let outcome = self.execute_merged(
                    "midi.notes_scale",
                    &json!({
                        "track": track, "clip": clip,
                        "indices": self.roll_state.selection.clone(),
                        "pivot_tick": pivot_tick, "factor": factor,
                    }),
                    &format!("roll-scale:{track}:{clip}:{gesture}"),
                );
                self.reselect_from(&outcome);
                outcome
            }
            RollIntent::SplitNotes { cuts, gesture } => {
                // The Cut command: one invocation (a knife stroke's batch or
                // a single click cut) is one undo step; the selection
                // follows the halves, as the command reports them. The cuts
                // name their own notes — the knife needs no pre-selection.
                let cuts: Vec<serde_json::Value> = cuts
                    .iter()
                    .map(|(index, tick)| json!({ "index": index, "tick": tick }))
                    .collect();
                let outcome = self.execute_merged(
                    "midi.notes_split",
                    &json!({
                        "track": track, "clip": clip,
                        "cuts": cuts,
                    }),
                    &format!("roll-cut:{track}:{clip}:{gesture}"),
                );
                self.reselect_from(&outcome);
                outcome
            }
            RollIntent::DeleteNotes { indices } => {
                // Deletion permutes the indices: the selection is stale after.
                selection_changed = true;
                self.execute(
                    "midi.note_delete",
                    &json!({ "track": track, "clip": clip, "indices": indices }),
                )
            }
            RollIntent::SetVelocities { updates } => {
                // Absolute per-note values; velocity changes never re-sort, so
                // the indices survive the gesture.
                let set_each: serde_json::Map<String, serde_json::Value> = updates
                    .into_iter()
                    .map(|(index, velocity)| (index.to_string(), json!(velocity)))
                    .collect();
                self.execute_merged(
                    "midi.change_velocity",
                    &json!({ "track": track, "clip": clip, "set_each": set_each }),
                    &format!("roll-vel:{track}:{clip}"),
                )
            }
            // Handled (and returned) by the early match at the top of this
            // function; listed here so the compiler keeps the enum covered.
            RollIntent::CurveUpsert { .. }
            | RollIntent::CurveRemove { .. }
            | RollIntent::CurveDragEnd => {
                unreachable!("curve intents never reach the note command table")
            }
            // W4: one-shot randomize over the selection (whole clip when the
            // selection is empty — the command owns that fallback).
            RollIntent::Randomize { options } => {
                let outcome = self.execute(
                    "midi.randomize",
                    &json!({
                        "track": track, "clip": clip,
                        "indices": self.roll_state.selection.clone(),
                        "pitch": options.pitch,
                        "start": options.start,
                        "length": options.length,
                        "velocity": options.velocity,
                        "probability": options.probability,
                    }),
                );
                self.reselect_from(&outcome);
                outcome
            }
            // W5: one-shot strum over the selection.
            RollIntent::Strum { options } => {
                let direction = match options.direction {
                    glitcherz_model::StrumDirection::Up => "up",
                    glitcherz_model::StrumDirection::Down => "down",
                    glitcherz_model::StrumDirection::Alternate => "alternate",
                    glitcherz_model::StrumDirection::Random => "random",
                };
                let outcome = self.execute(
                    "midi.strum",
                    &json!({
                        "track": track, "clip": clip,
                        "indices": self.roll_state.selection.clone(),
                        "direction": direction,
                        "spread_ticks": options.spread_ticks,
                        "group_window_ticks": options.group_window_ticks,
                        "velocity_ramp": options.velocity_ramp,
                        "jitter_ticks": options.jitter_ticks,
                    }),
                );
                self.reselect_from(&outcome);
                outcome
            }
        };
        if let Some((index, start, length)) = created {
            self.roll_state.resolve_created(index, start, length);
        }
        if selection_changed {
            self.roll_state.selection.clear();
        }
        if result.is_ok() {
            self.sync_midi_data_of(track, clip);
        } else if let Err(error) = result {
            self.set_message(format!("Edit failed: {error}"));
        }
    }

    /// Re-selects the notes a command just edited: the command reports the
    /// edited notes' new (post-sort) indices, so the selection keeps tracking
    /// the right notes even when the sort permuted them.
    pub(crate) fn reselect_from(&mut self, outcome: &glitcherz_command::Result<serde_json::Value>) {
        if let Ok(value) = outcome
            && let Some(list) = value["indices"].as_array()
        {
            let indices: Vec<usize> = list
                .iter()
                .filter_map(|entry| entry.as_u64().map(|index| index as usize))
                .collect();
            if !indices.is_empty() {
                self.roll_state.selection = indices;
            }
        }
    }

    /// Append a control track and select it (one undo step).
    pub(crate) fn add_control_track(&mut self) {
        let auto_count = self.project().control_tracks.len();
        if auto_count >= MAX_AUTO_TRACKS {
            return;
        }
        let before = self.begin_edit();
        self.project_mut()
            .control_tracks
            .push(ControlTrack::default());
        self.commit_edit("Add Automation Track", before);
        self.selected_auto = auto_count;
        self.send_control_clips(self.selected_auto);
    }

    /// Remove the selected control track and clear its engine route
    /// (one undo step).
    pub(crate) fn remove_control_track(&mut self) {
        let auto_count = self.project().control_tracks.len();
        if auto_count == 0 {
            return;
        }
        let index = self.selected_auto.min(auto_count - 1);
        let before = self.begin_edit();
        self.project_mut().control_tracks.remove(index);
        self.commit_edit("Remove Automation Track", before);
        let _ = self.engine.clear_auto_route(index);
        // Engine side: clear that index to an empty clip table.
        let _ = self
            .engine
            .set_control_clips(index, [RtControlClip::default(); MAX_TRACK_CLIPS]);
        self.selected_auto = index.min(self.project().control_tracks.len().saturating_sub(1));
    }
}
