//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    /// Points the Edit tab at the MIDI clip `(track, clip)`, dropping any
    /// in-flight roll gesture: selection indices refer to the clip that was
    /// being edited.
    ///
    /// P3 (spec `roll-multiclip.md` D5): the roll's ghost context is the
    /// timeline's clip selection, so whatever was multi-selected in the
    /// arrangement is what shows up behind the focus.
    pub(crate) fn set_edit_target(&mut self, track: usize, clip: usize) {
        // Clears the gesture *and* the note selection (the indices belong to
        // whichever clip used to be focused).
        self.roll_state.clear_gesture();
        let focus = piano_roll::ClipRef::midi(track, clip);
        // P4: control clips in the selection ride along as overlay lanes; the
        // focus itself stays a MIDI clip (a control clip opens the curve
        // editor instead).
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
        self.edit_clip = Some(EditTarget::Midi(subject));
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
        let Some(EditTarget::Midi(subject)) = &mut self.edit_clip else {
            return;
        };
        if subject.focus() == clip || !subject.set_focus(clip) {
            return;
        }
        self.roll_state.clear_gesture();
    }

    /// Points the Edit tab at the control clip `(track, clip)` and syncs the
    /// transient curve tool to the clip's persisted interpolation (U12).
    pub(crate) fn set_control_edit_target(&mut self, track: usize, clip: usize) {
        self.curve_tool = self
            .project()
            .control_tracks
            .get(track)
            .and_then(|t| t.clips.get(clip))
            .map(|c| CurveTool::of(c.interp))
            .unwrap_or_default();
        self.edit_clip = Some(EditTarget::Control { track, clip });
        self.edit_selection
            .clone_from(&self.timeline_state.selection);
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

    /// Body of the curve editor (Edit tab, U12): edits the anchors of the
    /// targeted control clip. The canvas shows **one loop period** (anchors
    /// are loop-relative); the interpolation follows the selected tool
    /// (Draw/Line → Linear, Curve → Smooth).
    pub(crate) fn control_editor_body(
        &mut self,
        ui: &mut egui::Ui,
        track: usize,
        clip_index: usize,
    ) {
        if self.project().control_tracks.is_empty() {
            // The target track was removed underneath the editor.
            self.edit_clip = None;
            return;
        }
        let sel = track.min(self.project().control_tracks.len() - 1);
        if self.project().control_tracks[sel].clips.is_empty() {
            let before = self.begin_edit();
            let bar = self.project().ticks_per_bar();
            let mut clip = ControlClip::new(0);
            clip.span_ticks = bar;
            clip.loop_ticks = bar;
            self.project_mut().control_tracks[sel].clips.push(clip);
            self.commit_edit("New Clip", before);
            self.send_control_clips(sel);
        }
        let clip_count = self.project().control_tracks[sel].clips.len();
        // Same rule as the roll body: a vanished clip closes the editor rather
        // than retargeting a neighbour (see `sync_edit_target`).
        if clip_index >= clip_count {
            self.edit_clip = None;
            return;
        }

        // ---- Header: identity + clip navigation + tool selector ----
        let name = if self.project().control_tracks[sel].name.is_empty() {
            format!("Curve {}", sel + 1)
        } else {
            self.project().control_tracks[sel].name.clone()
        };
        let mut goto: Option<usize> = None;
        let mut tool_pick: Option<CurveTool> = None;
        let curve_tool = self.curve_tool;
        ui.horizontal(|ui| {
            ui.label(format!(
                "{name} · clip {} (of {clip_count})",
                clip_index + 1
            ));
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
            ui.separator();
            for (tool, label, hover) in [
                (CurveTool::Draw, "Draw", "Freehand draw (linear)"),
                (
                    CurveTool::Line,
                    "Line",
                    "Control points, straight segments (linear)",
                ),
                (
                    CurveTool::Curve,
                    "Curve",
                    "Control points, smooth monotone curve",
                ),
            ] {
                if ui
                    .selectable_label(curve_tool == tool, label)
                    .on_hover_text(hover)
                    .clicked()
                {
                    tool_pick = Some(tool);
                }
            }
        });
        if let Some(next) = goto {
            self.set_control_edit_target(sel, next);
            return;
        }
        if let Some(tool) = tool_pick {
            self.curve_tool = tool;
            self.set_clip_interp(sel, clip_index, tool.interp());
        }

        // ---- Snapshot the clip (avoid holding a borrow of `self` across the
        // widget calls below) ----
        // The axis needs the endpoint *and* its target node (an effect slot's
        // range lives in the node's payload), and it is now only a **caption**:
        // the curve itself is stored normalized (spec `curve-map.md` D1/D11).
        let (map, axis_label) = {
            let graph = &self.project().graph;
            match graph.auto_target(sel) {
                Some((port, param)) => (
                    crate::curve::map_in(&param, graph, &port),
                    crate::patch::auto_param_label_at(&param, graph, &port),
                ),
                None => (glitcherz_model::MapSpec::UNIT, "not connected".to_string()),
            }
        };
        let (loop_ticks, interp, curve) = {
            let clip = &self.project().control_tracks[sel].clips[clip_index];
            (clip.loop_ticks.max(1), clip.interp, clip.curve.clone())
        };

        // ---- Canvas ----
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width().max(200.0), 320.0),
            egui::Sense::click_and_drag(),
        );
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 3.0, egui::Color32::from_rgb(20, 22, 26));
        let inner = rect.shrink(2.0);
        // Normalized geometry: -1 at the bottom, 0 mid, +1 at the top (D1).
        // The axis caption is the only thing that speaks in real units.
        let value_y = |v: f32| inner.bottom() - crate::curve::y_of(v) * inner.height();
        let y_value = |y: f32| crate::curve::norm_of_y((inner.bottom() - y) / inner.height());
        let x_at = |tick: f32| inner.left() + (tick / loop_ticks as f32) * inner.width();
        let tick_at = |x: f32| (x - inner.left()) / inner.width() * loop_ticks as f32;

        // Vertical grid (quarter notes) + value midline.
        let quarter = glitcherz_model::TICKS_PER_QUARTER.max(1);
        let mut t = 0u32;
        while t < loop_ticks {
            let x = x_at(t as f32);
            painter.line_segment(
                [egui::pos2(x, inner.top()), egui::pos2(x, inner.bottom())],
                egui::Stroke::new(1.0, egui::Color32::from_gray(38)),
            );
            t += quarter;
        }
        // The centre line is the axis centre — always normalized 0.
        painter.line_segment(
            [
                egui::pos2(inner.left(), value_y(0.0)),
                egui::pos2(inner.right(), value_y(0.0)),
            ],
            egui::Stroke::new(1.0, egui::Color32::from_gray(46)),
        );
        painter.text(
            egui::pos2(inner.left() + 4.0, inner.top() + 2.0),
            egui::Align2::LEFT_TOP,
            format!("{axis_label} · {}", crate::curve::axis_caption(map)),
            egui::FontId::proportional(11.0),
            egui::Color32::from_gray(150),
        );

        // Curve, sampled with the **same** math the engine uses.
        let color = egui::Color32::from_rgb(150, 200, 250);
        let mut prev: Option<egui::Pos2> = None;
        let mut x = inner.left();
        while x <= inner.right() {
            let phase = f64::from(tick_at(x).max(0.0));
            if let Some(v) = glitcherz_model::sample_curve(&curve, interp, phase) {
                let p = egui::pos2(x, value_y(v));
                if let Some(prev) = prev {
                    painter.line_segment([prev, p], egui::Stroke::new(2.0, color));
                }
                prev = Some(p);
            }
            x += 1.5;
        }
        // Anchors.
        for a in &curve {
            let p = egui::pos2(x_at(a.tick as f32), value_y(a.value));
            painter.circle_filled(p, 4.0, egui::Color32::WHITE);
            painter.circle_stroke(p, 4.0, egui::Stroke::new(1.0, egui::Color32::BLACK));
        }

        // ---- Interaction ----
        let snap = crate::snap::Snap::on(i64::from(crate::curve::SNAP_TICKS));
        let mapped = |pos: egui::Pos2| -> (u32, f32) {
            let phase = tick_at(pos.x).max(0.0);
            let limit = i64::from(loop_ticks.saturating_sub(1));
            let tick = snap.ticks(phase).clamp(0, limit) as u32;
            (tick, y_value(pos.y))
        };
        let hit_anchor = |pos: egui::Pos2| -> Option<u32> {
            curve
                .iter()
                .find(|a| {
                    let p = egui::pos2(x_at(a.tick as f32), value_y(a.value));
                    (pos - p).length() <= 8.0
                })
                .map(|a| a.tick)
        };

        let pointer = response.interact_pointer_pos();
        let started = response.drag_started_by(egui::PointerButton::Primary);
        let mut pending: Option<(Option<u32>, u32, f32, bool)> = None;
        let mut remove: Option<u32> = None;
        let mut drag_end = false;
        if curve_tool == CurveTool::Draw {
            if response.dragged_by(egui::PointerButton::Primary)
                && let Some(pos) = pointer
            {
                let (tick, value) = mapped(pos);
                pending = Some((None, tick, value, true));
            } else if response.clicked_by(egui::PointerButton::Primary)
                && let Some(pos) = pointer
            {
                let (tick, value) = mapped(pos);
                pending = Some((None, tick, value, false));
            }
            if response.drag_stopped_by(egui::PointerButton::Primary) {
                drag_end = true;
            }
        } else {
            if started && let Some(pos) = pointer {
                let (tick, value) = mapped(pos);
                let from = hit_anchor(pos);
                self.curve_drag = Some(tick);
                pending = Some((from, tick, value, true));
            }
            if response.clicked_by(egui::PointerButton::Primary)
                && let Some(pos) = pointer
                && hit_anchor(pos).is_none()
            {
                let (tick, value) = mapped(pos);
                pending = Some((None, tick, value, false));
            }
            if !started
                && response.dragged_by(egui::PointerButton::Primary)
                && let Some(pos) = pointer
                && self.curve_drag.is_some()
            {
                let (tick, value) = mapped(pos);
                pending = Some((self.curve_drag, tick, value, true));
            }
            if response.drag_stopped_by(egui::PointerButton::Primary)
                && self.curve_drag.take().is_some()
            {
                drag_end = true;
            }
        }
        if response.secondary_clicked()
            && let Some(pos) = pointer
        {
            let (tick, _) = mapped(pos);
            remove = Some(tick);
        }

        // ---- Apply ----
        if let Some((from, tick, value, dragging)) = pending {
            if curve_tool == CurveTool::Draw && interp != CurveInterp::Linear {
                self.set_clip_interp(sel, clip_index, CurveInterp::Linear);
            }
            self.curve_upsert(sel, clip_index, from, tick, value, dragging);
            if dragging && curve_tool != CurveTool::Draw {
                self.curve_drag = Some(tick);
            }
        }
        if let Some(tick) = remove {
            self.curve_remove_point(sel, clip_index, tick);
        }
        if drag_end {
            self.end_param_edit();
        }
        ui.weak("Click to add an anchor · drag to move · right-click to delete · Draw paints as you drag");
    }

    /// Body of the piano-roll editor (Edit tab): edits the targeted clip,
    /// pushes edits to the audio thread immediately.
    /// Edit tab body for a MIDI subject (P3, spec `roll-multiclip.md` D4–D7):
    /// the focus clip's roll, the rest of the context as read-only ghosts, and
    /// a chip strip to switch focus.
    pub(crate) fn midi_editor_body(
        &mut self,
        ui: &mut egui::Ui,
        subject: &piano_roll::RollSubject,
    ) {
        if self.project().midi_tracks.is_empty() {
            // The target track was removed underneath the editor.
            self.edit_clip = None;
            return;
        }
        let focus = subject.focus();
        let sel = focus.track.min(self.project().midi_tracks.len() - 1);
        if self.project().midi_tracks[sel].clips.is_empty() {
            let before = self.begin_edit();
            let bar = self.project().ticks_per_bar();
            let mut clip = MidiClip::new(0);
            clip.span_ticks = bar;
            clip.loop_ticks = bar;
            self.project_mut().midi_tracks[sel].clips.push(clip);
            self.commit_edit("New Clip", before);
            self.send_track_clips(sel);
        }
        let clip_count = self.project().midi_tracks[sel].clips.len();
        // A clip deleted underneath the editor (an undo, or a removal from a
        // path that did not clear the selection): close instead of silently
        // editing a neighbour — `sync_edit_target` re-derives the target from
        // the selection on the next frame, and falls back to the hint when
        // nothing live is left there.
        if focus.clip >= clip_count {
            self.edit_clip = None;
            return;
        }
        let clip_index = focus.clip;
        // U3a: the roll's bar lines follow the document meter.
        let bar_ticks = self.project().ticks_per_bar();
        let track_name = self.track_label(sel);
        ui.horizontal(|ui| {
            ui.label(format!(
                "{track_name} · clip {} (of {clip_count})",
                clip_index + 1
            ));
            if ui
                .add_enabled(clip_index > 0, egui::Button::new("←"))
                .on_hover_text("Previous clip")
                .clicked()
            {
                self.set_edit_target(sel, clip_index - 1);
            }
            if ui
                .add_enabled(clip_index + 1 < clip_count, egui::Button::new("→"))
                .on_hover_text("Next clip")
                .clicked()
            {
                self.set_edit_target(sel, clip_index + 1);
            }
        });
        // P3 (D7): the focus chip strip. Ghosts are read-only and never take
        // clicks, so focus is switched *here* — a chip click or Tab/Shift+Tab.
        // Clicking a ghost would re-open the "empty drag = marquee" ambiguity
        // M4 spent a whole spec closing.
        if subject.context().len() > 1 {
            let mut switch: Option<piano_roll::ClipRef> = None;
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Label::new(egui::RichText::new("showing").weak()));
                for clip in subject.context() {
                    let Some(c) = self
                        .project()
                        .midi_tracks
                        .get(clip.track)
                        .and_then(|t| t.clips.get(clip.clip))
                    else {
                        continue;
                    };
                    let label = format!(
                        "{} · bar {}",
                        self.track_label(clip.track),
                        c.start_ticks / bar_ticks.max(1) + 1
                    );
                    let focused = *clip == focus;
                    if ui
                        .selectable_label(focused, label)
                        .on_hover_text(if focused {
                            "Editing this clip (writable)"
                        } else {
                            "Click (or Ctrl+Tab) to edit this clip · its notes are ghosts until then"
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
                let next = match &mut self.edit_clip {
                    Some(EditTarget::Midi(subject)) => Some(subject.cycle_focus(backward)),
                    _ => None,
                };
                if next.is_some() {
                    self.roll_state.clear_gesture();
                }
            }
        }
        // Event List (M3.6): spreadsheet-style precision editing. It is a
        // resizable bottom strip, not a block stacked under the roll — the roll
        // takes the rest of the tab now, so anything placed after it would be
        // laid out off-screen. The split defaults to a third of the tab
        // (two thirds stay with the roll) and the divider is draggable; pulling
        // it below the minimum collapses the strip to its caption row, as does
        // clicking the caption.
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
                let count = self.project().midi_tracks[sel].clips[clip_index]
                    .notes()
                    .len();
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
                let clip = &self.project().midi_tracks[sel].clips[clip_index];
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| event_list::show(ui, clip))
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
            // P4: one overlay lane per control clip in the context. The
            // vertical axis comes from the *automation destination* of that
            // track's cable — the same mapping the standalone curve editor
            // uses, so a curve looks identical in both places.
            let lanes: Vec<piano_roll::OverlayLane<'_>> = subject
                .context()
                .iter()
                .filter(|c| matches!(c.lane, piano_roll::RollLane::Control))
                .filter_map(|c| {
                    let data = project.control_tracks.get(c.track)?.clips.get(c.clip)?;
                    // The caption names the destination (its range comes from
                    // the same map table the engine uses); the lane geometry
                    // itself is normalized now, so no axis travels with it.
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
                    Some(piano_roll::OverlayLane {
                        clip: *c,
                        data,
                        label: format!("{name} · {dest}"),
                    })
                })
                .collect();
            piano_roll::show(
                ui,
                &project.midi_tracks,
                &lanes,
                subject,
                roll_state,
                playhead,
                playing,
                bar_ticks,
                router,
            )
        };
        // I2: audition events bypass the command layer entirely — no undo
        // entry, no document change. They go to the focus track's
        // instrument (the same fan-out the recording path listens to).
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
        // Intents run as commands, so every roll edit is undoable. A drag
        // emits one intent per frame under the same gesture key: one drag,
        // one undo step.
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
            self.send_track_clips(track);
        } else if let Err(error) = result {
            self.set_message(format!("Edit failed: {error}"));
        }
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
            _ => {}
        }

        let mut selection_changed = false;
        // Draw gestures share one undo key across their frames: the creating
        // `note_add` and every resize frame collapse into a single step (one
        // gesture = one undo).
        let draw_key = |gesture: u64| format!("roll-draw:{track}:{clip}:{gesture}");
        let mut created: Option<(usize, i64, i64)> = None;
        let result = match intent {
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
            self.send_track_clips(track);
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
