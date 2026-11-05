//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    /// Mixer console (P5f D15/D21, stage-console metaphor): one column per
    /// mixer channel — solo / mute / level meter / fader / pan — plus channel
    /// add/remove buttons (2..=32, D21) and the output pan/mute stage. The
    /// channel a track feeds is labeled with the track name.
    ///
    /// The strip row scrolls horizontally (17+ channels overflow any panel):
    /// wheel / Shift+wheel scroll it, middle-drag pans it, and the add/remove
    /// + output column stays pinned on the right.
    ///
    /// Only rendered while the selected patch node **is** a mixer: the
    /// console edits that node's fields, so it must not appear next to an
    /// instrument's parameters. Track add/remove lives in the timeline
    /// toolbar (it is track management, not a mixer parameter).
    pub(crate) fn mixer_section(&mut self, ui: &mut egui::Ui, mixer_doc: u64) {
        ui.separator();
        ui.add_space(4.0);
        let mixer_engine = self.engine_id(mixer_doc);
        let NodeDoc::Mixer {
            inputs,
            channels,
            out_pan,
            out_muted,
            ..
        } = self
            .project()
            .graph
            .node_by_id(mixer_doc)
            .cloned()
            .unwrap_or_else(|| NodeDoc::Mixer {
                node_id: mixer_doc,
                track: 0,
                inputs: MIN_MIXER_CHANNELS as u8,
                channels: vec![Default::default(); MIN_MIXER_CHANNELS],
                out_pan: 0.0,
                out_muted: false,
            })
        else {
            return;
        };

        // Reverse map: channel → the track feeding it (for the column label +
        // the level meter, whose point is the track's chain end).
        let mut channel_track: Vec<Option<usize>> = vec![None; inputs as usize];
        for (t, _) in self.project().midi_tracks.iter().enumerate() {
            if let Some((mid, ch)) = self.track_channel_target(t)
                && mid == mixer_doc
                && (ch as usize) < channel_track.len()
            {
                channel_track[ch as usize] = Some(t);
            }
        }

        // Collect edits first; write back and send after the loop (avoids borrow conflicts).
        let n = inputs as usize;
        let mut solo_edits: Vec<Option<bool>> = vec![None; n];
        let mut mute_edits: Vec<Option<bool>> = vec![None; n];
        let mut gain_edits: Vec<Option<f32>> = vec![None; n];
        let mut pan_edits: Vec<Option<f32>> = vec![None; n];
        let mut add_ch_clicked = false;
        let mut remove_ch_clicked = false;
        let mut out_mute_edit: Option<bool> = None;
        let mut out_pan_edit: Option<f32> = None;
        // Inline title editor state: taken out of `self` so the strips closure
        // can hold a `&mut` to the draft, put back once the row has laid out.
        let mut rename = self.channel_rename.take();
        let mut rename_commit: Option<(usize, String)> = None;

        // ---- Strip row: horizontally scrollable ----
        // A console with 17+ channels is wider than any panel, so the strips
        // live in their own horizontal ScrollArea. Deliberately **not**
        // `ScrollArea::both` on the whole panel: a horizontally scrolling
        // content Ui hands every child an infinite available width, which
        // would stretch the parameter stack above the console. Navigation
        // matches the timeline / piano roll / patch viewports — wheel /
        // Shift+wheel scroll, middle-drag pans.
        //
        // The pan runs against **last frame's** viewport rect so the offset
        // and the pointer move in the same frame (same approach as the
        // timeline's wheel/pan block).
        if let Some((scroll_id, view_rect)) = self.mixer_strip_view
            && ui
                .ctx()
                .pointer_hover_pos()
                .is_some_and(|p| view_rect.contains(p))
        {
            let (panning, delta) = ui.ctx().input(|i| {
                (
                    i.pointer.button_down(egui::PointerButton::Middle),
                    i.pointer.delta(),
                )
            });
            if panning
                && delta != egui::Vec2::ZERO
                && let Some(mut s) = egui::containers::scroll_area::State::load(ui.ctx(), scroll_id)
            {
                s.offset.x = (s.offset.x - delta.x).max(0.0);
                s.store(ui.ctx(), scroll_id);
            }
        }

        let strip_width = (ui.available_width() - MIXER_STRIP_CONTROLS_W).max(160.0);
        // The row is a child Ui with an **explicit height**, not a
        // `ui.horizontal`: that helper hands its child a one-row-tall
        // max_rect, which collapses the scroll area's viewport — and with it
        // the middle-drag hit box and the scroll bar position — to ~18 px
        // (the strips themselves still overflow and paint, so it only shows
        // up as "pan does nothing"). The height is last frame's measured
        // strip band (self-correcting, since the strips are partly
        // font-sized), with the constant as the first-frame bootstrap.
        let row_rect = egui::Rect::from_min_size(
            ui.cursor().min,
            egui::vec2(ui.available_width(), self.mixer_strip_h),
        );
        let mut row = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(row_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Min))
                .id_salt(MIXER_STRIP_ROW),
        );
        let (strip_id, strip_band, strip_content_h) = {
            // One line of body text — the same style the old `ui.weak(name)`
            // used — measured so the cell never clips at another DPI/font.
            let title_h = row
                .text_style_height(&egui::TextStyle::Body)
                .max(MIXER_STRIP_TITLE_MIN_H);
            let strips = egui::ScrollArea::horizontal()
                .id_salt(MIXER_STRIP_SCROLL)
                .max_width(strip_width)
                .auto_shrink([true, true])
                .show(&mut row, |ui| {
                    ui.horizontal(|ui| {
                        for (ch_idx, ch) in channels.iter().enumerate() {
                            ui.vertical(|ui| {
                                // Default label from the channel→track map;
                                // a typed-in channel name wins over it.
                                let default_label =
                                    match channel_track.get(ch_idx).copied().flatten() {
                                        Some(t)
                                            if self.project().midi_tracks[t].name.is_empty() =>
                                        {
                                            format!("Ch {} · T{}", ch_idx + 1, t + 1)
                                        }
                                        Some(t) => {
                                            format!(
                                                "Ch {} · {}",
                                                ch_idx + 1,
                                                self.project().midi_tracks[t].name
                                            )
                                        }
                                        None => format!("Ch {}", ch_idx + 1),
                                    };
                                let custom = !ch.name.is_empty();
                                let display = if custom {
                                    ch.name.clone()
                                } else {
                                    default_label.clone()
                                };
                                let editing = rename
                                    .as_ref()
                                    .is_some_and(|r| r.node == mixer_doc && r.channel == ch_idx);
                                // Title cell, one fixed-size text line. A plain
                                // wrapping label is what blew the header up
                                // ("Ch 10"+ re-flowed to one glyph per line):
                                // the strip is a child of the scrolling row,
                                // so its available width shrinks with every
                                // column drawn before it, and the label wrapped
                                // to fit whatever was left. A fixed-size cell
                                // with truncating text is immune.
                                //
                                // The whole cell is a frameless Button — the
                                // same widget the timeline header uses. A bare
                                // `Label` is selectable text by default
                                // (`Interaction::selectable_labels`), so its
                                // glyph run stole right-clicks and showed the
                                // I-beam cursor; only the blank tail of the
                                // strip still answered. A Button owns the
                                // entire rect, so right-clicking the name
                                // itself opens the rename editor.
                                if editing {
                                    let mut cancel = false;
                                    let mut finish = false;
                                    if let Some(r) = rename.as_mut() {
                                        let resp = ui.add_sized(
                                            [MIXER_STRIP_W, title_h],
                                            egui::TextEdit::singleline(&mut r.draft)
                                                .hint_text(default_label.clone()),
                                        );
                                        if std::mem::take(&mut r.needs_focus) {
                                            resp.request_focus();
                                        }
                                        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                            cancel = true;
                                        } else if resp.lost_focus() {
                                            // Enter, Tab and a click elsewhere
                                            // all end the edit; empty clears
                                            // the custom name (back to default).
                                            rename_commit =
                                                Some((ch_idx, r.draft.trim().to_owned()));
                                            finish = true;
                                        }
                                    }
                                    if cancel || finish {
                                        rename = None;
                                    }
                                } else {
                                    let resp = ui
                                        .add_sized(
                                            [MIXER_STRIP_W, title_h],
                                            egui::Button::new(
                                                egui::RichText::new(&display).size(11.0),
                                            )
                                            .frame(false)
                                            .fill(egui::Color32::TRANSPARENT)
                                            .truncate(),
                                        )
                                        .on_hover_text(format!(
                                            "{display}\nRight-click to rename this channel"
                                        ));
                                    if resp.secondary_clicked() {
                                        rename = Some(ChannelRename {
                                            node: mixer_doc,
                                            channel: ch_idx,
                                            draft: ch.name.clone(),
                                            needs_focus: true,
                                        });
                                    }
                                }

                                // Solo / mute row.
                                ui.horizontal(|ui| {
                                    let mut solo_btn =
                                        egui::Button::new(if ch.solo { "S" } else { "s" });
                                    if ch.solo {
                                        solo_btn =
                                            solo_btn.fill(egui::Color32::from_rgb(200, 170, 60));
                                    }
                                    if ui.add_sized([32.0, 20.0], solo_btn).clicked() {
                                        solo_edits[ch_idx] = Some(!ch.solo);
                                    }
                                    let mut mute_btn =
                                        egui::Button::new(if ch.muted { "M" } else { "m" });
                                    if ch.muted {
                                        mute_btn =
                                            mute_btn.fill(egui::Color32::from_rgb(140, 60, 50));
                                    }
                                    if ui.add_sized([32.0, 20.0], mute_btn).clicked() {
                                        mute_edits[ch_idx] = Some(!ch.muted);
                                    }
                                });

                                // Post-fader level bar (metering point = the feeding track's chain end).
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(96.0, 10.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter()
                                    .rect_filled(rect, 2.0, egui::Color32::from_gray(40));
                                let level = channel_track
                                    .get(ch_idx)
                                    .copied()
                                    .flatten()
                                    .map(|t| self.engine.track_level(t).clamp(0.0, 1.0))
                                    .unwrap_or(0.0);
                                let filled = egui::Rect::from_min_size(
                                    rect.min,
                                    egui::vec2(rect.width() * level, rect.height()),
                                );
                                ui.painter().rect_filled(
                                    filled,
                                    2.0,
                                    egui::Color32::from_rgb(70, 160, 90),
                                );

                                // Fader + pan.
                                let mut gain = ch.gain;
                                let fader = ui.add_sized(
                                    [96.0, 110.0],
                                    egui::Slider::new(&mut gain, TRACK_GAIN_RANGE).vertical(),
                                );
                                if fader.changed() {
                                    gain_edits[ch_idx] = Some(gain);
                                }
                                let mut pan = ch.pan;
                                let pan_resp = ui.add_sized(
                                    [96.0, 20.0],
                                    egui::Slider::new(&mut pan, PAN_RANGE).text("Pan"),
                                );
                                if pan_resp.changed() {
                                    pan_edits[ch_idx] = Some(pan);
                                }
                            });
                            if ch_idx + 1 < channels.len() {
                                ui.separator();
                            }
                        }
                    });
                });
            // Hit box for the middle-drag pan: the strip band as it is on
            // screen (viewport width, strip height). The row height is
            // explicit, so this is the real band — scroll bar included.
            (
                strips.id,
                egui::Rect::from_min_size(
                    strips.inner_rect.min,
                    egui::vec2(
                        strips.inner_rect.width(),
                        strips.content_size.y.min(strips.inner_rect.height()),
                    ),
                ),
                strips.content_size.y,
            )
        };

        // ---- Channel add / remove (D21) — pinned right ----
        // Outside the ScrollArea: reachable at every scroll offset, which
        // is exactly how a channel gets added once the row overflows.
        row.separator();
        row.vertical(|ui| {
            ui.weak("Channels");
            if ui
                .add_enabled(
                    (inputs as usize) < MAX_MIXER_CHANNELS,
                    egui::Button::new("+ Ch"),
                )
                .clicked()
            {
                add_ch_clicked = true;
            }
            if ui
                .add_enabled(
                    (inputs as usize) > MIN_MIXER_CHANNELS,
                    egui::Button::new("- Ch"),
                )
                .clicked()
            {
                remove_ch_clicked = true;
            }

            // Output stage.
            ui.add_space(8.0);
            ui.weak("Out");
            let mut out_btn = egui::Button::new(if out_muted { "M" } else { "m" });
            if out_muted {
                out_btn = out_btn.fill(egui::Color32::from_rgb(140, 60, 50));
            }
            if ui.add_sized([72.0, 22.0], out_btn).clicked() {
                out_mute_edit = Some(!out_muted);
            }
            let mut op = out_pan;
            let op_resp = ui.add_sized(
                [96.0, 20.0],
                egui::Slider::new(&mut op, PAN_RANGE).text("Pan"),
            );
            if op_resp.changed() {
                out_pan_edit = Some(op);
            }
        });
        // The child Ui does not advance the parent cursor by itself
        // (`ui.horizontal` would); without this the next section draws on top
        // of the console.
        ui.advance_cursor_after_rect(row.min_rect());
        // Remember the viewport for next frame's middle-drag pan, and the
        // strip band's height so next frame's row viewport is exact (content
        // + the scroll bar it allocates — `allocated_width` is the bar's
        // thickness, and 0 when the bar floats over the content).
        self.mixer_strip_view = Some((strip_id, strip_band));
        self.mixer_strip_h = (strip_content_h + ui.spacing().scroll.allocated_width()).max(1.0);
        // Put the (possibly still open) title editor back — unless it belongs
        // to another console or to a channel that has since been removed.
        self.channel_rename = rename.filter(|r| r.node == mixer_doc && r.channel < n);
        if let Some((idx, name)) = rename_commit {
            // Same undoable document edit the patch view's inlet rename uses.
            self.rename_mixer_channel(mixer_doc, idx as u8, name);
        }

        // ---- Apply edits (document + engine node-param path, D20) ----
        let send_mixer = |ui_app: &mut Self, id: ParamId, value: f32| {
            if let Some(engine_id) = mixer_engine {
                ui_app.engine.send(EngineCommand::SetNodeParam {
                    node_id: engine_id,
                    id,
                    value,
                });
            }
        };
        let apply_channel = |ui_app: &mut Self, label: &str, f: &dyn Fn(&mut NodeDoc)| {
            let before = ui_app.begin_edit();
            if let Some(node) = ui_app.project_mut().graph.node_by_id_mut(mixer_doc) {
                f(node);
            }
            ui_app.commit_edit(label, before);
        };
        for i in 0..n {
            let i8 = i as u8;
            if let Some(solo) = solo_edits[i] {
                apply_channel(self, "Mixer Solo", &|node| {
                    if let NodeDoc::Mixer { channels, .. } = node
                        && let Some(c) = channels.get_mut(i)
                    {
                        c.solo = solo;
                    }
                });
                send_mixer(self, ParamId::MixerSolo(i8), if solo { 1.0 } else { 0.0 });
            }
            if let Some(muted) = mute_edits[i] {
                apply_channel(self, "Mixer Mute", &|node| {
                    if let NodeDoc::Mixer { channels, .. } = node
                        && let Some(c) = channels.get_mut(i)
                    {
                        c.muted = muted;
                    }
                });
                send_mixer(self, ParamId::MixerMute(i8), if muted { 1.0 } else { 0.0 });
            }
            if let Some(gain) = gain_edits[i] {
                apply_channel(self, "Mixer Gain", &|node| {
                    if let NodeDoc::Mixer { channels, .. } = node
                        && let Some(c) = channels.get_mut(i)
                    {
                        c.gain = gain;
                    }
                });
                send_mixer(self, ParamId::MixerGain(i8), gain);
            }
            if let Some(pan) = pan_edits[i] {
                apply_channel(self, "Mixer Pan", &|node| {
                    if let NodeDoc::Mixer { channels, .. } = node
                        && let Some(c) = channels.get_mut(i)
                    {
                        c.pan = pan;
                    }
                });
                send_mixer(self, ParamId::MixerPan(i8), pan);
            }
        }
        if let Some(muted) = out_mute_edit {
            apply_channel(self, "Mixer Out Mute", &|node| {
                if let NodeDoc::Mixer { out_muted: m, .. } = node {
                    *m = muted;
                }
            });
            send_mixer(self, ParamId::MixerOutMute, if muted { 1.0 } else { 0.0 });
        }
        if let Some(pan) = out_pan_edit {
            apply_channel(self, "Mixer Out Pan", &|node| {
                if let NodeDoc::Mixer { out_pan: p, .. } = node {
                    *p = pan;
                }
            });
            send_mixer(self, ParamId::MixerOutPan, pan);
        }
        if add_ch_clicked {
            self.mixer_add_channel(mixer_doc);
        }
        if remove_ch_clicked {
            self.mixer_remove_channel(mixer_doc);
        }
    }

    /// Grow the mixer by one channel (D21): document inputs+channels (+1
    /// default channel) + engine param sync so the new slot's weights match.
    pub(crate) fn mixer_add_channel(&mut self, mixer_doc: u64) {
        let before = self.begin_edit();
        let Some(NodeDoc::Mixer { inputs, .. }) =
            self.project_mut().graph.node_by_id_mut(mixer_doc)
        else {
            return;
        };
        if *inputs as usize >= MAX_MIXER_CHANNELS {
            self.set_message(format!("Channel limit reached ({MAX_MIXER_CHANNELS})"));
            return;
        }
        *inputs += 1;
        if let Some(NodeDoc::Mixer { channels, .. }) =
            self.project_mut().graph.node_by_id_mut(mixer_doc)
        {
            channels.push(Default::default());
        }
        // Engine sync: the mixer processor has fixed 32 slots — push the new
        // channel's state so its summing weights are (re)computed.
        if let (Some(engine_id), Some(NodeDoc::Mixer { channels, .. })) = (
            self.engine_id(mixer_doc),
            self.project().graph.node_by_id(mixer_doc),
        ) {
            let ch = (channels.len() - 1) as u8;
            if let Some(c) = channels.last() {
                for (id, value) in [
                    (ParamId::MixerGain(ch), c.gain),
                    (ParamId::MixerPan(ch), c.pan),
                    (ParamId::MixerMute(ch), if c.muted { 1.0 } else { 0.0 }),
                    (ParamId::MixerSolo(ch), if c.solo { 1.0 } else { 0.0 }),
                ] {
                    self.engine.send(EngineCommand::SetNodeParam {
                        node_id: engine_id,
                        id,
                        value,
                    });
                }
            }
        }
        self.commit_edit("Add Mixer Channel", before);
    }

    /// Shrink the mixer by one channel (D21): rejected when the last channel
    /// still has an audio cable landed on it (occupied ports are protected).
    ///
    /// U4c (spec `control-tracks.md` §9 D20): an automation cable addressing
    /// the removed channel is dropped in the same undo step — a channel index
    /// that no longer exists would otherwise fail the next save's validation,
    /// and the engine would keep writing into an unused slot.
    pub(crate) fn mixer_remove_channel(&mut self, mixer_doc: u64) {
        // Read the channel count first (immutable borrow; the mutation happens
        // after the cable scan).
        let Some(NodeDoc::Mixer { inputs, .. }) = self.project().graph.node_by_id(mixer_doc) else {
            return;
        };
        if *inputs as usize <= MIN_MIXER_CHANNELS {
            return;
        }
        let last = *inputs - 1;
        let occupied = self.project().graph.cables.iter().any(|c| {
            c.kind == CableKind::Audio && c.to == PortRef::Node(mixer_doc) && c.in_port == last
        });
        if occupied {
            self.set_message(format!(
                "Cannot remove channel {}: a cable is connected to it",
                last + 1
            ));
            return;
        }
        // Control tracks whose curve drives this channel (their routes must be
        // cleared on the engine side too).
        let orphaned: Vec<usize> = self
            .project()
            .graph
            .cables
            .iter()
            .filter(|c| {
                c.kind == CableKind::Automation
                    && c.to == PortRef::Node(mixer_doc)
                    && matches!(
                        c.param,
                        AutoParam::Node(NodeParam::MixerChannel { channel, .. }) if channel == last
                    )
            })
            .filter_map(|c| match c.from {
                PortRef::AutoTrack(index) => Some(index),
                _ => None,
            })
            .collect();
        let before = self.begin_edit();
        if let Some(NodeDoc::Mixer {
            inputs, channels, ..
        }) = self.project_mut().graph.node_by_id_mut(mixer_doc)
        {
            *inputs = last;
            channels.pop();
        }
        self.project_mut().graph.cables.retain(|c| {
            !(c.kind == CableKind::Automation
                && c.to == PortRef::Node(mixer_doc)
                && matches!(
                    c.param,
                    AutoParam::Node(NodeParam::MixerChannel { channel, .. }) if channel == last
                ))
        });
        self.commit_edit("Remove Mixer Channel", before);
        let dropped = orphaned.len();
        for index in orphaned {
            let _ = self.engine.clear_auto_route(index);
        }
        if dropped > 0 {
            self.set_message(format!(
                "Removed channel {} and its {dropped} automation cable(s)",
                last + 1
            ));
        }
    }

    /// 新增一条 MIDI 轨：只增加一条空轨道位（文档 `MidiTrack` + 引擎轨占用），
    /// 不建 Instrument / delay 节点、不接线、不扩 mixer——音源与路由交给用户
    /// 在 patch 里自行搭建。一步 undo。
    pub(crate) fn add_midi_track(&mut self) {
        let track = self.project().midi_tracks.len();
        if track >= MAX_MIDI_TRACKS {
            return;
        }
        let before = self.begin_edit();
        self.project_mut()
            .midi_tracks
            .push(glitcherz_model::MidiTrack::default());
        self.engine
            .set_midi_track_count(self.project().midi_tracks.len());
        self.selected_track = track;
        self.commit_edit("Add MIDI Track", before);
    }

    /// 删除最后一条 MIDI 轨（中间轨删除需重排轨道表与文档 track 字段，
    /// 随 T3 编曲视图一起做）。引擎逐节点 bypass 移除 + 轨道表清理 +
    /// 活跃轨数收缩；文档删节点与触及连线（含 MIDI 自连）。mixer 节点
    /// （track 0）不属于任何轨，必须排除（P5f D15）。
    pub(crate) fn remove_last_midi_track(&mut self) {
        let Some(last) = self.project().midi_tracks.len().checked_sub(1) else {
            return;
        };
        let before = self.begin_edit();
        let doc_ids: Vec<u64> = self
            .project()
            .graph
            .nodes
            .iter()
            .filter(|n| {
                n.track() == last
                    && !n.is_mixer()
                    // Audio-track nodes reuse `track` for the audio lane index;
                    // removing a MIDI track must not take them with it.
                    && !matches!(n, NodeDoc::AudioTrack { .. })
            })
            .map(|n| n.node_id())
            .collect();
        for doc_id in &doc_ids {
            if let Some(&engine_id) = self.node_map.get(doc_id) {
                if !self.engine.remove_node(engine_id) {
                    log::warn!("Track node {engine_id} removal failed");
                }
            }
            self.node_map.remove(doc_id);
            self.effect_meters.remove(doc_id);
            // Node-keyed session caches (G4) go with the node.
            self.clap_effect_params.remove(doc_id);
            self.clap_effect_values.remove(doc_id);
            self.vst3_effect_params.remove(doc_id);
            self.vst3_effect_values.remove(doc_id);
            self.voice_params.remove(doc_id);
            self.clap_voice_params.remove(doc_id);
            self.clap_voice_values.remove(doc_id);
            self.vst3_voice_values.remove(doc_id);
            self.voice_default_state.remove(doc_id);
            self.voice_preset_selected.remove(doc_id);
        }
        if !self.engine.remove_midi_track(last) {
            log::warn!("engine remove_midi_track({last}) failed");
        }
        self.engine.set_midi_track_count(last);
        // 文档：删节点 + 删触及连线（含 MidiTrack(last) 自连；外部连到
        // 本轨节点的扇出线因触及被删节点一并移除）。
        let removed: HashSet<u64> = doc_ids.into_iter().collect();
        self.project_mut().graph.nodes.retain(|n| {
            !(n.track() == last && !n.is_mixer() && !matches!(n, NodeDoc::AudioTrack { .. }))
        });
        self.project_mut().graph.cables.retain(|c| {
            let touches_removed = match (&c.from, &c.to) {
                (PortRef::Node(f), _) => removed.contains(f),
                (_, PortRef::Node(t)) => removed.contains(t),
                _ => false,
            };
            !touches_removed && c.from != PortRef::MidiTrack(last)
        });
        self.project_mut().midi_tracks.pop();
        self.selected_track = self.selected_track.min(last.saturating_sub(1));
        // The editor may have targeted the removed track's clip.
        if let Some(EditTarget::Midi(subject)) = &self.edit_clip
            && subject.focus().track == last
        {
            self.edit_clip = None;
        }
        self.commit_edit("Remove MIDI Track", before);
    }

    /// 新增一条音频轨（U6 D3）：文档 `AudioTrack` + `NodeDoc::AudioTrack`
    /// 节点 + mixer 空闲通道连线（自动扩容），引擎侧经 `install_node` 出生
    /// 生成器节点。一步 undo（内部 mixer 扩容被 `suspend_history` 吸收）。
    pub(crate) fn add_audio_track(&mut self) {
        let track = self.project().audio_tracks.len();
        if track >= glitcherz_model::MAX_AUDIO_TRACKS {
            return;
        }
        // Engine slot check up front: a full node table must not leave a
        // half-built document track behind.
        if self.project().graph.nodes.len() >= MAX_NODES {
            self.set_message("Add audio track failed: node table full".to_string());
            return;
        }
        let before = self.begin_edit();
        self.session.suspend_history();
        let node_id = self
            .project()
            .graph
            .nodes
            .iter()
            .map(|n| n.node_id())
            .max()
            .unwrap_or(1)
            + 1;
        self.project_mut()
            .audio_tracks
            .push(glitcherz_model::AudioTrack::default());
        self.project_mut()
            .graph
            .nodes
            .push(NodeDoc::AudioTrack { node_id, track });
        // D18 (same wiring as MIDI tracks): the generator feeds the global
        // mixer's first free channel (growing the mixer when full). The chosen
        // channel is remembered so the engine wire-up reuses the exact channel
        // the doc cable records — recomputing it after the cable push would see
        // the channel as taken and pick a different one (or none).
        let mut wire: Option<(u64, u8)> = None;
        if let Some(mixer_doc) = self
            .project()
            .graph
            .nodes
            .iter()
            .find(|n| n.is_mixer())
            .map(NodeDoc::node_id)
        {
            let ch = match self.project().graph.first_free_channel(mixer_doc) {
                Some(ch) => ch,
                None => {
                    self.mixer_add_channel(mixer_doc);
                    self.project()
                        .graph
                        .first_free_channel(mixer_doc)
                        .unwrap_or(u8::MAX)
                }
            };
            if ch != u8::MAX {
                self.project_mut()
                    .graph
                    .cables
                    .push(glitcherz_model::CableDoc::audio_to_channel(
                        node_id,
                        PortRef::Node(mixer_doc),
                        ch,
                    ));
                wire = Some((mixer_doc, ch));
            }
        }
        // Engine side: install the node (processor + engine id + mapping);
        // the cable dispatch follows the same route `apply_document` uses.
        let sample_rate = self.engine.info().sample_rate;
        let node_doc = self
            .project()
            .graph
            .node_by_id(node_id)
            .cloned()
            .expect("just pushed");
        self.install_node(&node_doc, sample_rate);
        if let Some((mixer_doc, ch)) = wire {
            if let (Some(&audio_e), Some(&mixer_e)) =
                (self.node_map.get(&node_id), self.node_map.get(&mixer_doc))
            {
                self.engine.connect_nodes(audio_e, mixer_e, ch);
            }
        }
        self.send_audio_clips_for_track(track);
        self.session.resume_history();
        self.commit_edit("Add Audio Track", before);
    }

    /// 删除最后一条音频轨（U6）：引擎节点移除 + 文档轨/节点/触及连线清
    /// 理，一步 undo。空的 clip 表随节点一起被回收。
    pub(crate) fn remove_last_audio_track(&mut self) {
        let Some(last) = self.project().audio_tracks.len().checked_sub(1) else {
            return;
        };
        let before = self.begin_edit();
        let doc_ids: Vec<u64> = self
            .project()
            .graph
            .nodes
            .iter()
            .filter(|n| matches!(n, NodeDoc::AudioTrack { track: t, .. } if *t == last))
            .map(|n| n.node_id())
            .collect();
        for doc_id in &doc_ids {
            if let Some(&engine_id) = self.node_map.get(doc_id)
                && !self.engine.remove_node(engine_id)
            {
                log::warn!("Audio track node {engine_id} removal failed");
            }
            self.node_map.remove(doc_id);
        }
        let removed: HashSet<u64> = doc_ids.into_iter().collect();
        self.project_mut()
            .graph
            .nodes
            .retain(|n| !matches!(n, NodeDoc::AudioTrack { track: t, .. } if *t == last));
        self.project_mut().graph.cables.retain(|c| {
            let touches_removed = match (&c.from, &c.to) {
                (PortRef::Node(f), _) => removed.contains(f),
                (_, PortRef::Node(t)) => removed.contains(t),
                _ => false,
            };
            !touches_removed
        });
        self.project_mut().audio_tracks.pop();
        self.commit_edit("Remove Audio Track", before);
    }

    /// Reorders the MIDI tracks (the track at `from` is re-inserted so it
    /// ends up just before original index `to`): the track table, the
    /// per-track node `track` fields and the `MidiTrack` cable indices all
    /// move together as one undo step, then the engine is rebuilt from the
    /// document — the same path undo/redo already takes. Mixer / Keyboard /
    /// Feedback `track` fields are patch-layout grouping only and stay put
    /// (the same exclusion `remove_last_midi_track` applies).
    pub(crate) fn reorder_midi_track(&mut self, from: usize, to: usize) {
        let len = self.project().midi_tracks.len();
        if from >= len || to > len || to == from || to == from + 1 {
            return; // identity drop (the view already filters these)
        }
        let perm = crate::timeline::track_permutation(len, from, to);
        let before = self.begin_edit();
        {
            let project = self.project_mut();
            // 1) Track table: new[perm[old]] = tracks[old].
            let old_tracks = std::mem::take(&mut project.midi_tracks);
            let mut new_tracks = vec![glitcherz_model::MidiTrack::default(); len];
            for (old, track) in old_tracks.into_iter().enumerate() {
                new_tracks[perm[old]] = track;
            }
            project.midi_tracks = new_tracks;
            // 2) Per-track node `track` fields (Instrument / Effect / the
            // legacy ChannelStrip are track members; the fixed entities
            // above are not).
            for node in &mut project.graph.nodes {
                match node {
                    NodeDoc::Instrument { track, .. }
                    | NodeDoc::Effect { track, .. }
                    | NodeDoc::ChannelStrip { track, .. }
                        if *track < len =>
                    {
                        *track = perm[*track];
                    }
                    _ => {}
                }
            }
            // 3) `MidiTrack` cable outlets follow the table.
            for cable in &mut project.graph.cables {
                if let PortRef::MidiTrack(t) = &mut cable.from
                    && *t < len
                {
                    *t = perm[*t];
                }
            }
        }
        // App-side track indices follow the permutation: the selected
        // track, the editor target and the timeline's clip selection.
        if self.selected_track < len {
            self.selected_track = perm[self.selected_track];
        }
        // P3: every context member follows the permutation, not just the
        // focus — a ghost left on the old index would point at another track.
        if let Some(EditTarget::Midi(subject)) = &mut self.edit_clip {
            for clip in subject.context_mut() {
                if clip.track < len {
                    clip.track = perm[clip.track];
                }
            }
        }
        for sel in &mut self.timeline_state.selection {
            if matches!(sel.lane, timeline::TimelineLane::Midi) && sel.track < len {
                sel.track = perm[sel.track];
            }
        }
        // The subject was remapped in place, so its snapshot has to follow:
        // an out-of-date snapshot would re-assemble (and reset the focus).
        self.edit_selection
            .clone_from(&self.timeline_state.selection);
        self.apply_document();
        self.commit_edit("Reorder Tracks", before);
    }

    /// Reorders one track family by the lane that owns the grabbed header.
    pub(crate) fn reorder_track(&mut self, lane: timeline::TimelineLane, from: usize, to: usize) {
        match lane {
            timeline::TimelineLane::Midi => self.reorder_midi_track(from, to),
            timeline::TimelineLane::Control => self.reorder_control_track(from, to),
            timeline::TimelineLane::Audio => self.reorder_audio_track(from, to),
        }
    }

    /// Reorders the control (curve) tracks: the track table moves together
    /// with every `PortRef::AutoTrack` cable outlet and every curve recording
    /// input (`Timeline In`), then the engine is rebuilt from the document.
    pub(crate) fn reorder_control_track(&mut self, from: usize, to: usize) {
        let len = self.project().control_tracks.len();
        if from >= len || to > len || to == from || to == from + 1 {
            return;
        }
        let perm = crate::timeline::track_permutation(len, from, to);
        let before = self.begin_edit();
        {
            let project = self.project_mut();
            let old_tracks = std::mem::take(&mut project.control_tracks);
            let mut new_tracks = vec![ControlTrack::default(); len];
            for (old, track) in old_tracks.into_iter().enumerate() {
                new_tracks[perm[old]] = track;
            }
            project.control_tracks = new_tracks;
            for cable in &mut project.graph.cables {
                if let PortRef::AutoTrack(t) = &mut cable.from
                    && *t < len
                {
                    *t = perm[*t];
                }
                if let PortRef::TrackIn {
                    kind: TrackKind::Curve,
                    track,
                } = &mut cable.to
                    && *track < len
                {
                    *track = perm[*track];
                }
            }
        }
        if self.selected_auto < len {
            self.selected_auto = perm[self.selected_auto];
        }
        if let Some(EditTarget::Control { track, .. }) = &mut self.edit_clip
            && *track < len
        {
            *track = perm[*track];
        }
        for sel in &mut self.timeline_state.selection {
            if matches!(sel.lane, timeline::TimelineLane::Control) && sel.track < len {
                sel.track = perm[sel.track];
            }
        }
        self.edit_selection
            .clone_from(&self.timeline_state.selection);
        self.apply_document();
        // The sequencer holds the control clip tables by track index; the
        // graph rebuild does not re-push them, so re-send their new slots.
        for index in 0..self.project().control_tracks.len() {
            self.send_control_clips(index);
        }
        self.commit_edit("Reorder Curve Tracks", before);
    }

    /// Reorders the audio tracks: the track table moves together with every
    /// `NodeDoc::AudioTrack` generator and every audio recording input
    /// (`Timeline In`), then the engine is rebuilt from the document.
    pub(crate) fn reorder_audio_track(&mut self, from: usize, to: usize) {
        let len = self.project().audio_tracks.len();
        if from >= len || to > len || to == from || to == from + 1 {
            return;
        }
        let perm = crate::timeline::track_permutation(len, from, to);
        let before = self.begin_edit();
        {
            let project = self.project_mut();
            let old_tracks = std::mem::take(&mut project.audio_tracks);
            let mut new_tracks = vec![glitcherz_model::AudioTrack::default(); len];
            for (old, track) in old_tracks.into_iter().enumerate() {
                new_tracks[perm[old]] = track;
            }
            project.audio_tracks = new_tracks;
            for node in &mut project.graph.nodes {
                if let NodeDoc::AudioTrack { track, .. } = node
                    && *track < len
                {
                    *track = perm[*track];
                }
            }
            for cable in &mut project.graph.cables {
                if let PortRef::TrackIn {
                    kind: TrackKind::Audio,
                    track,
                } = &mut cable.to
                    && *track < len
                {
                    *track = perm[*track];
                }
            }
        }
        if self.selected_audio < len {
            self.selected_audio = perm[self.selected_audio];
        }
        for sel in &mut self.timeline_state.selection {
            if matches!(sel.lane, timeline::TimelineLane::Audio) && sel.track < len {
                sel.track = perm[sel.track];
            }
        }
        self.edit_selection
            .clone_from(&self.timeline_state.selection);
        self.apply_document();
        self.commit_edit("Reorder Audio Tracks", before);
    }

    /// `Track N` or the track's own name — the label every clip reference
    /// shows (edit header, roll focus chips).
    pub(crate) fn track_label(&self, track: usize) -> String {
        match self.project().midi_tracks.get(track) {
            Some(t) if !t.name.is_empty() => t.name.clone(),
            _ => format!("Track {}", track + 1),
        }
    }
}
