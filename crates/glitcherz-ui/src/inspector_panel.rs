//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;

impl GlitcherZApp {
    /// Main view: the **Inspector** — the selected node's parameters and
    /// nothing structural. Adding / deleting / wiring nodes belongs to the
    /// patch (spec `plugin-browser.md` D2), the mixer console appears only
    /// while a mixer node is selected, and automation curves belong to the
    /// timeline.
    pub(crate) fn inspector_panel(&mut self, ui: &mut egui::Ui) {
        // In eframe 0.36 the root Ui has no margins; wrap a CentralPanel to keep the default look.
        egui::CentralPanel::default().show(ui, |ui| {
            if self.project().midi_tracks.is_empty() {
                ui.heading("Inspector");
                ui.weak("(no MIDI tracks in project)");
                return;
            }
            let sel = self.selected_track.min(self.project().midi_tracks.len() - 1);
            self.selected_track = sel;
            // R3 (spec `recording.md` §3.2): a keyboard input node has exactly
            // one property — which source it listens to — so the Inspector is
            // its binding panel rather than a "no parameters" placeholder.
            if let Some(node) = self.selected_keyboard_node() {
                self.keyboard_source_panel(ui, node);
                return;
            }
            // Panel editing target (spec `plugin-gui.md` §6): the patch
            // selection's first editable document node, falling back to the
            // selected track's first-match instrument (pre-G4 behavior).
            let target = self.panel_target_node();
            let has_any_selection = self.patch_state.has_any_selection();
            let target_doc = target.and_then(|id| self.project().graph.node_by_id(id).cloned());
            let title = match (&target_doc, target) {
                (Some(NodeDoc::Effect { .. }), Some(id)) => {
                    format!("Effect #{id} · Track {}", sel + 1)
                }
                (Some(NodeDoc::Mixer { .. }), Some(id)) => format!("Mixer #{id}"),
                (Some(NodeDoc::Instrument { .. }), Some(id)) => {
                    format!("Instrument #{id} · Track {}", sel + 1)
                }
                (Some(NodeDoc::Arpeggiator { .. }), Some(id)) => format!("Arpeggiator #{id}"),
                (Some(NodeDoc::Portamento { .. }), Some(id)) => format!("Portamento #{id}"),
                (Some(NodeDoc::MidiEnvelope { .. }), Some(id)) => {
                    format!("MIDI Envelope #{id}")
                }
                (Some(NodeDoc::Lfo { .. }), Some(id)) => format!("LFO #{id}"),
                (Some(NodeDoc::CurveMap { .. }), Some(id)) => format!("Curve Map #{id}"),
                (Some(NodeDoc::Input { .. }), Some(id)) => format!("Audio In #{id}"),
                (None, None) if has_any_selection => "Selection".to_string(),
                _ => {
                    if self.project().midi_tracks[sel].name.is_empty() {
                        format!("Track {}", sel + 1)
                    } else {
                        self.project().midi_tracks[sel].name.clone()
                    }
                }
            };
            ui.heading(title);
            if target.is_none() && has_any_selection {
                ui.weak("This node has no editable parameters.");
            }
            // R4: the live input node's panel — device picker, stream status,
            // per-channel meter, channel and output trim. The transport no
            // longer carries the device/meter.
            if let (Some(NodeDoc::Input { channel, gain, .. }), Some(id)) = (&target_doc, target)
            {
                self.input_section(ui, id, *channel, *gain);
                return;
            }
            // M5: MIDI FX / control-source parameter stack (spec
            // `midi-nodes.md` §5). These nodes own no plugin editor, so the
            // panel is just their parameters.
            if let Some(id) = target
                && matches!(
                    target_doc,
                    Some(
                        NodeDoc::Arpeggiator { .. }
                            | NodeDoc::Portamento { .. }
                            | NodeDoc::MidiEnvelope { .. }
                            | NodeDoc::Lfo { .. }
                            | NodeDoc::CurveMap { .. }
                    )
                )
            {
                self.midi_fx_params(ui, id);
                return;
            }
            // Embedded plugin editor (spec `plugin-gui.md` §4): CLAP
            // instruments only (v1). A collapsing header owns the toggle —
            // its triangle sits on a row of its own **above** the reserved
            // GUI rect, so the native container window (which always paints
            // on top of egui) can never cover the control that opened it.
            // The framed area reserves layout space whose rect drives the
            // native container window position.
            // Deliberately **outside** the ScrollArea: a native child
            // window cannot be clipped by egui, so a scrolling editor
            // would smear over neighbouring UI (ghosting) — the editor
            // stays pinned at the top and only the parameter stack below
            // scrolls.
            // `open(Some(..))` makes `editor_user_open` the single source of
            // truth: egui would otherwise remember the open state itself and
            // fight the "plugin closed its own GUI" path (which clears the
            // flag outside this panel).
            if target.is_some_and(|t| self.node_guard_has_editor(t)) {
                let user_wants = self.editor_user_open;
                let resp = egui::CollapsingHeader::new("Editor")
                    .id_salt("plugin_editor_header")
                    .open(Some(user_wants))
                    .show(ui, |ui| {
                        // The body only lays out while open; the session may
                        // still be one frame behind the click, hence the
                        // `None` arm.
                        if let Some(editor) = self.plugin_editor.as_mut()
                            && Some(editor.doc_node) == target
                        {
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(
                                        editor.size.0.max(1.0),
                                        editor.size.1.max(1.0),
                                    ),
                                    egui::Sense::hover(),
                                );
                                editor.rect = Some(rect);
                            });
                        }
                    });
                if resp.header_response.clicked() {
                    if user_wants {
                        self.editor_user_open = false;
                        self.close_plugin_editor();
                    } else {
                        self.editor_user_open = true;
                        self.plugin_editor_wants_open = true;
                    }
                    // The forced `open` state only changes next frame.
                    ui.ctx().request_repaint();
                }
            }
            // Long parameter stack: without a scroll area the content clips with no scrollbar.
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            // G4 dispatch (spec `plugin-gui.md` §6): instrument targets (patch
            // selection or the track fallback — the fallback IS the track's
            // instrument node) get the preset / parameter / editor stack.
            if matches!(target_doc, Some(NodeDoc::Instrument { .. }))
                && let Some(instr_id) = target
            {
                // No source selector here (spec `plugin-browser.md` D1/D2/D3):
                // an instrument is born with its final voice from the patch Add
                // menu, and "switch source" means delete + add — parameters are
                // never migrated. What stays is the **preset** picker, since a
                // SoundFont bank/program is a preset, not a source.
                // SoundFont preset picker (visible while the track voice is
                // SoundFont): presets enumerated from the shared session
                // library; the selection is one undoable edit + live
                // node-param delivery (D7).
                // SoundFont panel (U13): file row, preset, output gain.
                // Every control here is the *same* function the node's own
                // body calls, so the two entry points can never drift.
                let sf2_state = self.project().graph.node_by_id(instr_id).and_then(|n| {
                    match n {
                        NodeDoc::Instrument {
                            voice:
                                VoiceSpec::SoundFont {
                                    path,
                                    bank,
                                    program,
                                    gain,
                                    bend_range,
                                    ..
                                },
                            ..
                        } => Some((path.clone(), *bank, *program, *gain, *bend_range)),
                        _ => None,
                    }
                });
                if let Some((sf2_path, sf2_bank, sf2_program, sf2_gain, sf2_bend_range)) =
                    sf2_state
                {
                    self.sf2_section(
                        ui,
                        instr_id,
                        &sf2_path,
                        sf2_bank,
                        sf2_program,
                        sf2_gain,
                        sf2_bend_range,
                    );
                }
                // Sample instrument parameters (U5, spec `sample-player.md`
                // D10): root note / play mode / gain. The root DragValue and
                // gain slider follow the adsr_sliders pattern (live
                // node-param delivery + one merged-undo document write);
                // the play-mode combo is a discrete one-step edit.
                let sample_state = self
                    .project()
                    .graph
                    .node_by_id(instr_id)
                    .and_then(|n| match n {
                        NodeDoc::Instrument {
                            voice:
                                VoiceSpec::Sample {
                                    path,
                                    root_note,
                                    play_mode,
                                    gain,
                                    region,
                                },
                            ..
                        } => Some((
                            path.clone(),
                            *root_note,
                            matches!(play_mode, SamplePlayMode::OneShot),
                            *gain,
                            *region,
                        )),
                        _ => None,
                    });
                if let Some((sample_path, root_note, one_shot, gain, region)) = sample_state {
                    self.sample_section(
                        ui,
                        instr_id,
                        &sample_path,
                        root_note,
                        one_shot,
                        gain,
                        region,
                    );
                }
                // W1: drum kit table (load / gain / per-slot trim).
                let drum_state = self.drum_kit_fields(instr_id);
                if let Some((slots, gain)) = drum_state {
                    self.drum_section(ui, instr_id, &slots, gain);
                }
                // B1: modal strike macros (preset + decay / beating /
                // hardness / brightness / gain).
                let modal_state = self
                    .project()
                    .graph
                    .node_by_id(instr_id)
                    .and_then(|n| match n {
                        NodeDoc::Instrument {
                            voice: VoiceSpec::Modal { params },
                            ..
                        } => Some(*params),
                        _ => None,
                    });
                if let Some(params) = modal_state {
                    self.modal_section(ui, instr_id, &params);
                }
                // S1: plucked-string macros (decay / damping / brightness /
                // release / gain).
                let pluck_state = self
                    .project()
                    .graph
                    .node_by_id(instr_id)
                    .and_then(|n| match n {
                        NodeDoc::Instrument {
                            voice: VoiceSpec::Pluck { params },
                            ..
                        } => Some(*params),
                        _ => None,
                    });
                if let Some(params) = pluck_state {
                    self.pluck_section(ui, instr_id, &params);
                }
                // S2: drawbar organ (drawbar grid + percussion + ramps).
                let organ_state = self
                    .project()
                    .graph
                    .node_by_id(instr_id)
                    .and_then(|n| match n {
                        NodeDoc::Instrument {
                            voice: VoiceSpec::Organ { params },
                            ..
                        } => Some(*params),
                        _ => None,
                    });
                if let Some(params) = organ_state {
                    self.organ_section(ui, instr_id, &params);
                }
                // Effects are added from the graph only (Add ▸ Effect, or the
                // cable-midpoint insert — spec `plugin-browser.md` D12); the
                // track-level "clear" shortcut sits above the effect list.
            }

            // External effect parameter panels: sliders per effect node,
            // delivered through node addressing (`set_node_param` — the
            // effect node's own `PluginParam` bridge). CLAP sliders show raw
            // plugin-range values, VST3 sliders normalized 0..1 (the same
            // domain split as the instruments). Current values live in the
            // UI session only (no undo, same class as the instrument sliders).
            // G4: an Effect panel target narrows the list to that node;
            // otherwise the track's effects show (pre-G4 behavior).
            let fx_list: Vec<(u64, String, bool)> = match &target_doc {
                Some(NodeDoc::Effect { node_id, voice, .. }) => match voice {
                    VoiceSpec::Clap { plugin_id, .. } => {
                        vec![(*node_id, plugin_id.clone(), true)]
                    }
                    VoiceSpec::Vst3 { path, .. } => vec![(
                        *node_id,
                        std::path::Path::new(path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "VST3".to_string()),
                        false,
                    )],
                    _ => Vec::new(),
                },
                _ if panel_shows_track_extras(&target_doc, has_any_selection) => {
                    self.track_external_fx(sel)
                }
                _ => Vec::new(),
            };
            // Track-level "clear" shortcut at the head of the effect list
            // (spec `plugin-browser.md` §8.1): the batch removal of a track's
            // external effects. Adding effects stays a graph operation (D12),
            // so this is the only structural action left in the Inspector.
            // Hidden when the panel is narrowed to one effect node — there the
            // header would read as "clear this one".
            let fx_track_view = !matches!(&target_doc, Some(NodeDoc::Effect { .. }));
            if fx_track_view && !fx_list.is_empty() {
                ui.horizontal(|ui| {
                    ui.label("Effects");
                    if ui.button("Clear All Effects").clicked() {
                        self.clear_external_effects(sel);
                    }
                });
            }
            for (fx_doc, fx_label, is_clap) in fx_list {
                let Some(&fx_engine) = self.node_map.get(&fx_doc) else {
                    continue;
                };
                if is_clap {
                    let Some(params) = self.clap_effect_params.get(&fx_doc).cloned() else {
                        continue;
                    };
                    if params.is_empty() {
                        continue;
                    }
                    egui::CollapsingHeader::new(format!("FX CLAP {fx_label} ({})", params.len()))
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(280.0)
                                .show(ui, |ui| {
                                    egui::Grid::new(format!("clap_fx_{fx_doc}"))
                                        .num_columns(3)
                                        .spacing([16.0, 4.0])
                                        .show(ui, |ui| {
                                            for param in params {
                                                let label = if param.module.is_empty() {
                                                    param.name.clone()
                                                } else {
                                                    format!("{} / {}", param.module, param.name)
                                                };
                                                ui.label(label);
                                                let mut value = self
                                                    .clap_effect_values
                                                    .get(&fx_doc)
                                                    .and_then(|by_id| by_id.get(&param.id))
                                                    .copied()
                                                    .unwrap_or(param.default_value);
                                                let slider = ui.add(
                                                    egui::Slider::new(
                                                        &mut value,
                                                        param.min_value..=param.max_value,
                                                    )
                                                    .text(format!("id {}", param.id)),
                                                );
                                                if slider.changed() {
                                                    self.clap_effect_values
                                                        .entry(fx_doc)
                                                        .or_default()
                                                        .insert(param.id, value);
                                                    self.engine.set_node_param(
                                                        fx_engine,
                                                        ParamId::PluginParam(param.id),
                                                        value as f32,
                                                    );
                                                }
                                                ui.weak(format!(
                                                    "[{:.3}..{:.3}] default {:.3}",
                                                    param.min_value, param.max_value,
                                                    param.default_value
                                                ));
                                                ui.end_row();
                                            }
                                        });
                                });
                        });
                } else {
                    let Some(params) = self.vst3_effect_params.get(&fx_doc).cloned() else {
                        continue;
                    };
                    if params.is_empty() {
                        continue;
                    }
                    egui::CollapsingHeader::new(format!("FX VST3 {fx_label} ({})", params.len()))
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(280.0)
                                .show(ui, |ui| {
                                    egui::Grid::new(format!("vst3_fx_{fx_doc}"))
                                        .num_columns(3)
                                        .spacing([16.0, 4.0])
                                        .show(ui, |ui| {
                                            for param in params {
                                                ui.label(&param.title);
                                                let mut value = self
                                                    .vst3_effect_values
                                                    .get(&fx_doc)
                                                    .and_then(|by_id| by_id.get(&param.id))
                                                    .copied()
                                                    .unwrap_or(param.default_normalized);
                                                let slider = ui.add(
                                                    egui::Slider::new(&mut value, 0.0..=1.0)
                                                        .text(format!("id {}", param.id)),
                                                );
                                                if slider.changed() {
                                                    self.vst3_effect_values
                                                        .entry(fx_doc)
                                                        .or_default()
                                                        .insert(param.id, value);
                                                    self.engine.set_node_param(
                                                        fx_engine,
                                                        ParamId::PluginParam(param.id),
                                                        value as f32,
                                                    );
                                                    self.sync_vst3_host_param(
                                                        fx_doc, param.id, value,
                                                    );
                                                }
                                                ui.weak(format!(
                                                    "normalized{}",
                                                    if param.can_automate {
                                                        ""
                                                    } else {
                                                        " (not automatable)"
                                                    }
                                                ));
                                                ui.end_row();
                                            }
                                        });
                                });
                        });
                }
            }

            // Preset row: host-side preset library over the plugin state
            // blob (`presets/<plugin-key>/<name>.glzpreset`, raw state
            // bytes). Load/Reset ride the S5 state plumbing and are undoable
            // document edits; Save As is a file operation. Hidden for the
            // built-in synth (no presets yet).
            // G4: instrument-targeted (patch selection or track fallback).
            if matches!(target_doc, Some(NodeDoc::Instrument { .. }))
                && let Some(instr_id) = target
            {
            // The plugin parameter panels below are gated on the voice kind:
            // the VST3 panel exists for VST3 instruments, the CLAP panel for
            // CLAP ones, and built-in / SoundFont voices show neither (they
            // have no plugin parameters — same rule as the effect panels,
            // which appear only when parameter data exists).
            let voice_is_vst3 = self.project().graph.node_by_id(instr_id).is_some_and(|n| {
                matches!(n, NodeDoc::Instrument { voice: VoiceSpec::Vst3 { .. }, .. })
            });
            let voice_is_clap = self.project().graph.node_by_id(instr_id).is_some_and(|n| {
                matches!(n, NodeDoc::Instrument { voice: VoiceSpec::Clap { .. }, .. })
            });
            if self.voice_plugin_key(instr_id).is_some() {
                ui.horizontal(|ui| {
                    ui.label("Preset");
                    let presets = self
                        .voice_plugin_key(instr_id)
                        .map(|key| list_voice_presets_in(&preset_library_root(), &key))
                        .unwrap_or_default();
                    let selected = self.voice_preset_selected.get(&instr_id).cloned();
                    egui::ComboBox::from_id_salt(format!("voice_preset_{instr_id}"))
                        .selected_text(selected.unwrap_or_else(|| "(factory default)".to_string()))
                        .show_ui(ui, |ui| {
                            for name in &presets {
                                let is_selected = self
                                    .voice_preset_selected
                                    .get(&instr_id)
                                    .is_some_and(|s| s == name);
                                if ui.selectable_label(is_selected, name).clicked() {
                                    let name = name.clone();
                                    self.load_voice_preset(instr_id, &name);
                                }
                            }
                        });
                    if ui.button("Reset").clicked() {
                        self.reset_voice_preset(instr_id);
                    }
                    ui.separator();
                    ui.add(
                        egui::TextEdit::singleline(&mut self.preset_name_input)
                            .hint_text("preset name")
                            .desired_width(120.0),
                    );
                    if ui.button("Save As").clicked() {
                        self.save_voice_preset_as(instr_id);
                    }
                    let can_delete = self.voice_preset_selected.contains_key(&instr_id);
                    if ui
                        .add_enabled(can_delete, egui::Button::new("Delete"))
                        .clicked()
                    {
                        if let Some(name) = self.voice_preset_selected.get(&instr_id).cloned() {
                            if let Some(key) = self.voice_plugin_key(instr_id) {
                                match delete_voice_preset_in(&preset_library_root(), &key, &name)
                                {
                                    Ok(()) => {
                                        self.voice_preset_selected.remove(&instr_id);
                                        self.set_message(format!("Preset deleted: {name}"));
                                    }
                                    Err(err) => {
                                        self.set_message(format!("Preset delete failed: {err}"))
                                    }
                                }
                            }
                        }
                    }
                });
            }

            // VST3 plugin parameter panel (normalized 0..1 sliders; the
            // bridge passes the normalized value through as-is). Rows are
            // **virtualized** (`ScrollArea::show_rows` — only visible
            // sliders are laid out, so 2000+ parameter plugins stay
            // responsive) and a filter narrows the list. Current values are
            // seeded from the controller read-back at load time, UI session
            // only. G4: keyed to the target instrument node.
            // Only rendered when the target's voice **is** VST3.
            if voice_is_vst3 {
                egui::CollapsingHeader::new(format!(
                    "Plugin Params ({})",
                    self.voice_params.get(&instr_id).map(Vec::len).unwrap_or(0)
                ))
            .default_open(false)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.vst3_param_filter)
                        .hint_text("Filter params…"),
                );
                let needle = self.vst3_param_filter.to_lowercase();
                let mut changed: Vec<(u32, f64)> = Vec::new();
                if let Some(params) = self.voice_params.get(&instr_id) {
                    let visible: Vec<usize> = params
                        .iter()
                        .enumerate()
                        .filter(|(_, p)| {
                            needle.is_empty() || p.title.to_lowercase().contains(&needle)
                        })
                        .map(|(i, _)| i)
                        .collect();
                    let row_h = ui.text_style_height(&egui::TextStyle::Body);
                    if visible.is_empty() {
                        ui.weak("(no match)");
                    }
                    egui::ScrollArea::vertical()
                        .max_height(280.0)
                        .show_rows(ui, row_h, visible.len(), |ui, range| {
                            for row in range {
                                let param = &params[visible[row]];
                                ui.horizontal(|ui| {
                                    ui.add_sized(
                                        [200.0, row_h],
                                        egui::Label::new(&param.title).truncate(),
                                    );
                                    let mut value = self
                                        .vst3_voice_values
                                        .get(&instr_id)
                                        .and_then(|by_id| by_id.get(&param.id))
                                        .copied()
                                        .unwrap_or(param.default_normalized);
                                    let slider = ui.add_sized(
                                        [240.0, row_h],
                                        egui::Slider::new(&mut value, 0.0..=1.0)
                                            .text(format!("id {}", param.id)),
                                    );
                                    if slider.changed() {
                                        changed.push((param.id, value));
                                    }
                                    ui.weak(format!(
                                        "normalized{}",
                                        if param.can_automate {
                                            ""
                                        } else {
                                            " (not automatable)"
                                        }
                                    ));
                                });
                            }
                        });
                }
                // Mutations applied after the params borrow ends (NLL).
                for (id, value) in changed {
                    self.vst3_voice_values
                        .entry(instr_id)
                        .or_default()
                        .insert(id, value);
                    self.send_node_param(instr_id, ParamId::PluginParam(id), value as f32);
                    self.sync_vst3_host_param(instr_id, id, value);
                }
            });
            }

            // CLAP plugin parameter panel: enumerated via the params extension at instantiation.
            // Sliders send raw plugin-range values through the `PluginParam` bridge (applied at
            // the next `process`, block granularity). Rows are virtualized (`show_rows`) and
            // grouped by the CLAP `module` path (Dexed's OP1/OP2/… sections) — the label shows
            // `module / name` and an empty-filter view sorts by module. Current values live in
            // the UI session only; a fresh plugin load resets them to the plugin defaults.
            // Only rendered when the target's voice **is** CLAP.
            if voice_is_clap {
            egui::CollapsingHeader::new(format!(
                "Plugin Params CLAP ({})",
                self.clap_voice_params.get(&instr_id).map(Vec::len).unwrap_or(0)
            ))
            .default_open(false)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.clap_param_filter)
                        .hint_text("Filter params (module / name)…"),
                );
                let needle = self.clap_param_filter.to_lowercase();
                let mut changed: Vec<(u32, f64)> = Vec::new();
                if let Some(params) = self.clap_voice_params.get(&instr_id) {
                    let mut visible: Vec<usize> = (0..params.len())
                        .filter(|&i| {
                            needle.is_empty() || {
                                let p = &params[i];
                                format!("{} {}", p.module, p.name).to_lowercase().contains(&needle)
                            }
                        })
                        .collect();
                    if needle.is_empty() {
                        // Stable sort by module: sections keep their
                        // original enumeration order within the group.
                        visible.sort_by(|&a, &b| params[a].module.cmp(&params[b].module));
                    }
                    let row_h = ui.text_style_height(&egui::TextStyle::Body);
                    if visible.is_empty() {
                        ui.weak("(no match)");
                    }
                    egui::ScrollArea::vertical()
                        .max_height(280.0)
                        .show_rows(ui, row_h, visible.len(), |ui, range| {
                            for row in range {
                                let param = &params[visible[row]];
                                let label = if param.module.is_empty() {
                                    param.name.clone()
                                } else {
                                    format!("{} / {}", param.module, param.name)
                                };
                                ui.horizontal(|ui| {
                                    ui.add_sized(
                                        [220.0, row_h],
                                        egui::Label::new(label).truncate(),
                                    );
                                    let mut value = self
                                        .clap_voice_values
                                        .get(&instr_id)
                                        .and_then(|by_id| by_id.get(&param.id))
                                        .copied()
                                        .unwrap_or(param.default_value);
                                    let slider = ui.add_sized(
                                        [240.0, row_h],
                                        egui::Slider::new(
                                            &mut value,
                                            param.min_value..=param.max_value,
                                        )
                                        .text(format!("id {}", param.id)),
                                    );
                                    if slider.changed() {
                                        changed.push((param.id, value));
                                    }
                                    ui.weak(format!(
                                        "[{:.2}..{:.2}]",
                                        param.min_value, param.max_value
                                    ));
                                });
                            }
                        });
                }
                // Mutations applied after the params borrow ends (NLL).
                for (id, value) in changed {
                    self.clap_voice_values
                        .entry(instr_id)
                        .or_default()
                        .insert(id, value);
                    self.send_node_param(instr_id, ParamId::PluginParam(id), value as f32);
                }
            });
            }
            }
            ui.weak("Node graph: instrument -> delay -> mixer channels -> master (doc-driven, changes go live)");
            ui.add_space(8.0);

            // Filter / envelope: instrument-targeted (G4 — the panel
            // target's node fields; the no-selection fallback resolves to
            // the selected track's instrument node anyway).
            //
            // Only the **built-in synth** reads `SynthParams`: the sample
            // player, drum kit, SoundFont and plugin voices each own their
            // sound and their own panel below, and the engine never consumes
            // the timbre fields for them. Offering waveform / cutoff / ADSR
            // there was a panel of knobs wired to nothing. `patch.rs`
            // `inline_params_of` splits the canvas rows the same way, so the
            // two surfaces stay in step.
            let synth_target = match (&target_doc, target) {
                (Some(NodeDoc::Instrument { voice, .. }), Some(id))
                    if voice_has_synth_params(voice) =>
                {
                    Some(id)
                }
                _ => None,
            };
            // Effect sliders: an Effect panel target edits that node; the
            // instrument target (and the track fallback) edits the track's
            // built-in delay (pre-G4 behavior); a Mixer target shows none
            // (the console below owns mixer parameters).
            let effect_target: Option<u64> = match (&target_doc, target) {
                (Some(NodeDoc::Effect { .. }), Some(id)) => Some(id),
                (Some(NodeDoc::Mixer { .. }), _) => None,
                _ if panel_shows_track_extras(&target_doc, has_any_selection) => self
                    .project()
                    .graph
                    .builtin_delay_of_track(sel)
                    .map(NodeDoc::node_id),
                _ => None,
            };
            // Everything below belongs to the panel's **node** — the
            // instrument's filter / amp envelope, the effect's delay. Master
            // gain and the output meter live on the transport bar (always
            // visible), so they must not look like part of this node.
            if synth_target.is_some() || effect_target.is_some() {
                ui.horizontal(|ui| {
                    ui.strong("Node Parameters");
                    ui.weak("— filter / envelope / effect of the node above, not the master bus");
                });
            }
            egui::Grid::new("synth_params")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    if let Some(instr_id) = synth_target {
                    ui.label("Waveform");
                    ui.horizontal(|ui| {
                        let waveform = self.synth_of_node(instr_id).unwrap_or_default().waveform;
                        for wave in SynthWaveform::ALL {
                            if ui
                                .selectable_label(waveform == wave, wave.label())
                                .clicked()
                            {
                                let before = self.begin_edit();
                                if let Some(s) = self.synth_mut_node(instr_id) {
                                    s.waveform = wave;
                                }
                                self.commit_edit("Waveform", before);
                                self.send_node_param(
                                    instr_id,
                                    ParamId::OscWaveform,
                                    f32::from(wave.index()),
                                );
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("Cutoff Freq");
                    let mut cutoff = self.synth_of_node(instr_id).unwrap_or_default().cutoff_hz;
                    let r = ui.add(
                        egui::Slider::new(&mut cutoff, CUTOFF_RANGE)
                            .logarithmic(true)
                            .text("Hz"),
                    );
                    if r.changed() {
                        let before = self.begin_param_edit();
                        if let Some(s) = self.synth_mut_node(instr_id) {
                            s.cutoff_hz = cutoff;
                        }
                        self.commit_param_edit("Filter Cutoff", before, "synth-cutoff");
                        self.send_node_param(instr_id, ParamId::FilterCutoff, cutoff);
                    }
                    ui.end_row();

                    ui.label("Resonance");
                    let mut resonance = self.synth_of_node(instr_id).unwrap_or_default().resonance;
                    let r = ui.add(egui::Slider::new(&mut resonance, RES_RANGE));
                    if r.changed() {
                        let before = self.begin_param_edit();
                        if let Some(s) = self.synth_mut_node(instr_id) {
                            s.resonance = resonance;
                        }
                        self.commit_param_edit("Filter Resonance", before, "synth-res");
                        self.send_node_param(instr_id, ParamId::FilterResonance, resonance);
                    }
                    ui.end_row();

                    ui.label("Filter Mode");
                    ui.horizontal_wrapped(|ui| {
                        let mode = self.synth_of_node(instr_id).unwrap_or_default().filter_mode;
                        // Four buttons: wrapped so a narrow Inspector pushes
                        // "Moog" to a second line instead of clipping it.
                        for (name, value, hint) in [
                            ("Low-pass", 0.0, "12 dB/oct state-variable low pass"),
                            ("High-pass", 1.0, "12 dB/oct state-variable high pass"),
                            ("Band-pass", 2.0, "12 dB/oct state-variable band pass"),
                            (
                                "Moog",
                                3.0,
                                "24 dB/oct 4-pole ladder low pass, saturation in the resonance path",
                            ),
                        ] {
                            if ui
                                .selectable_label((mode - value).abs() < 0.5, name)
                                .on_hover_text(hint)
                                .clicked()
                            {
                                let before = self.begin_edit();
                                if let Some(s) = self.synth_mut_node(instr_id) {
                                    s.filter_mode = value;
                                }
                                self.commit_edit("Filter Mode", before);
                                self.send_node_param(instr_id, ParamId::FilterMode, value);
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("Volume Envelope");
                    self.adsr_sliders(ui, instr_id);
                    ui.end_row();
                    }

                    if let Some(effect_id) = effect_target {
                        let name = self
                            .effect_params_of_node(effect_id)
                            .map(|p| crate::effect::descriptor(&p).name)
                            .unwrap_or("Effect");
                        ui.label(name);
                        self.effect_sliders(ui, effect_id);
                        ui.end_row();
                    }
                });

            // The console belongs to the mixer node alone: it shows when (and
            // only when) the patch selection is a mixer. Track add / remove
            // moved to the timeline toolbar, automation to the timeline
            // strip — the Inspector does not manage tracks.
            if let (Some(NodeDoc::Mixer { .. }), Some(mixer_doc)) = (&target_doc, target) {
                self.mixer_section(ui, mixer_doc);
            }
            });
        });
    }

    /// ADSR four sliders (local copies + write-back, avoiding borrow
    /// conflicts), bound to the target instrument node (G4).
    /// MIDI FX / control-source parameter stack (spec `midi-nodes.md` §5).
    ///
    /// Same live-delivery + merged-undo pattern as [`Self::adsr_sliders`]:
    /// the engine hears every slider tick, the document records one step per
    /// drag.
    pub(crate) fn midi_fx_params(&mut self, ui: &mut egui::Ui, doc_id: u64) {
        /// Arpeggiator division labels, in the engine's `ARP_DIVISION_TICKS`
        /// order (spec `midi-nodes.md` §5.1).
        const ARP_DIVISION_LABELS: [&str; 8] =
            ["1/2", "1/4", "1/8", "1/8T", "1/16", "1/16T", "1/32", "1/64"];
        let Some(node) = self.project().graph.node_by_id(doc_id).cloned() else {
            return;
        };
        ui.weak("MIDI / control only — this node has no audio output");
        ui.add_space(4.0);
        match node {
            NodeDoc::Arpeggiator { params, .. } => {
                let mut p = params;
                let mut changed = false;
                egui::Grid::new("arp_params")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Rate");
                        egui::ComboBox::from_id_salt("arp_division")
                            .selected_text(
                                ARP_DIVISION_LABELS
                                    [usize::from(p.division).min(ARP_DIVISION_LABELS.len() - 1)],
                            )
                            .show_ui(ui, |ui| {
                                for (index, label) in ARP_DIVISION_LABELS.iter().enumerate() {
                                    if ui
                                        .selectable_label(usize::from(p.division) == index, *label)
                                        .clicked()
                                    {
                                        p.division = index as u8;
                                        changed = true;
                                    }
                                }
                            });
                        ui.end_row();
                        ui.label("Gate");
                        changed |= ui.add(egui::Slider::new(&mut p.gate, 0.05..=1.0)).changed();
                        ui.end_row();
                        ui.label("Mode");
                        egui::ComboBox::from_id_salt("arp_mode")
                            .selected_text(p.mode.label())
                            .show_ui(ui, |ui| {
                                for mode in glitcherz_model::ArpMode::ALL {
                                    if ui.selectable_label(p.mode == mode, mode.label()).clicked() {
                                        p.mode = mode;
                                        changed = true;
                                    }
                                }
                            });
                        ui.end_row();
                        ui.label("Harmony");
                        egui::ComboBox::from_id_salt("arp_harmony")
                            .selected_text(p.harmony.label())
                            .show_ui(ui, |ui| {
                                for harmony in glitcherz_model::ArpHarmony::ALL {
                                    if ui
                                        .selectable_label(p.harmony == harmony, harmony.label())
                                        .clicked()
                                    {
                                        p.harmony = harmony;
                                        changed = true;
                                    }
                                }
                            });
                        ui.end_row();
                        ui.label("Skip");
                        changed |= ui.add(egui::Slider::new(&mut p.skip, 0.0..=0.9)).changed();
                        ui.end_row();
                        ui.label("Octaves");
                        changed |= ui.add(egui::Slider::new(&mut p.octaves, 1..=4)).changed();
                        ui.end_row();
                        ui.label("Latch");
                        changed |= ui.checkbox(&mut p.latch, "").changed();
                        ui.end_row();
                    });
                if changed {
                    self.send_node_param(doc_id, ParamId::ArpDivision, f32::from(p.division));
                    self.send_node_param(doc_id, ParamId::ArpGate, p.gate);
                    self.send_node_param(doc_id, ParamId::ArpMode, f32::from(p.mode.index()));
                    self.send_node_param(doc_id, ParamId::ArpOctaves, f32::from(p.octaves));
                    self.send_node_param(
                        doc_id,
                        ParamId::ArpLatch,
                        if p.latch { 1.0 } else { 0.0 },
                    );
                    self.send_node_param(doc_id, ParamId::ArpHarmony, f32::from(p.harmony.index()));
                    self.send_node_param(doc_id, ParamId::ArpSkip, p.skip);
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Arpeggiator { params, .. }) =
                        self.project_mut().graph.node_by_id_mut(doc_id)
                    {
                        *params = p;
                    }
                    self.commit_param_edit("Arpeggiator", before, "midi-fx-params");
                }
            }
            NodeDoc::Portamento { params, .. } => {
                let mut p = params;
                let mut changed = false;
                egui::Grid::new("portamento_params")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Glide Time");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut p.time_ms)
                                    .range(0.0..=2000.0)
                                    .suffix(" ms"),
                            )
                            .changed();
                        ui.end_row();
                        ui.label("Low End");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut p.min)
                                    .range(-24.0..=24.0)
                                    .suffix(" st"),
                            )
                            .changed();
                        ui.end_row();
                        ui.label("High End");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut p.max)
                                    .range(-24.0..=24.0)
                                    .suffix(" st"),
                            )
                            .changed();
                        ui.end_row();
                        ui.label("Legato Only");
                        changed |= ui.checkbox(&mut p.legato_only, "").changed();
                        ui.end_row();
                    });
                if changed {
                    self.send_node_param(doc_id, ParamId::PortTime, p.time_ms);
                    self.send_node_param(doc_id, ParamId::PortMin, p.min);
                    self.send_node_param(doc_id, ParamId::PortMax, p.max);
                    self.send_node_param(
                        doc_id,
                        ParamId::PortLegato,
                        if p.legato_only { 1.0 } else { 0.0 },
                    );
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Portamento { params, .. }) =
                        self.project_mut().graph.node_by_id_mut(doc_id)
                    {
                        *params = p;
                    }
                    self.commit_param_edit("Portamento", before, "midi-fx-params");
                }
            }
            NodeDoc::MidiEnvelope { params, .. } => {
                let mut p = params;
                let mut changed = false;
                egui::Grid::new("midi_env_params")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Attack");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut p.attack)
                                    .range(0.0..=5.0)
                                    .suffix(" s"),
                            )
                            .changed();
                        ui.end_row();
                        ui.label("Decay");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut p.decay)
                                    .range(0.0..=5.0)
                                    .suffix(" s"),
                            )
                            .changed();
                        ui.end_row();
                        ui.label("Sustain");
                        changed |= ui
                            .add(egui::Slider::new(&mut p.sustain, 0.0..=1.0))
                            .changed();
                        ui.end_row();
                        ui.label("Release");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut p.release)
                                    .range(0.0..=10.0)
                                    .suffix(" s"),
                            )
                            .changed();
                        ui.end_row();
                        ui.label("Low End");
                        changed |= ui
                            .add(egui::DragValue::new(&mut p.min).range(-127.0..=127.0))
                            .changed();
                        ui.end_row();
                        ui.label("High End");
                        changed |= ui
                            .add(egui::DragValue::new(&mut p.max).range(-127.0..=127.0))
                            .changed();
                        ui.end_row();
                    });
                if changed {
                    self.send_node_param(doc_id, ParamId::EnvAttack, p.attack);
                    self.send_node_param(doc_id, ParamId::EnvDecay, p.decay);
                    self.send_node_param(doc_id, ParamId::EnvSustain, p.sustain);
                    self.send_node_param(doc_id, ParamId::EnvRelease, p.release);
                    self.send_node_param(doc_id, ParamId::EnvMin, p.min);
                    self.send_node_param(doc_id, ParamId::EnvMax, p.max);
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::MidiEnvelope { params, .. }) =
                        self.project_mut().graph.node_by_id_mut(doc_id)
                    {
                        *params = p;
                    }
                    self.commit_param_edit("MIDI Envelope", before, "midi-fx-params");
                }
            }
            // Spec `lfo.md` §6: waveform + time base + depth, with a
            // two-cycle preview so the shape is visible before wiring it up.
            NodeDoc::Lfo { params, .. } => {
                let mut p = params;
                let mut changed = false;
                egui::Grid::new("lfo_params")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Waveform");
                        egui::ComboBox::from_id_salt("lfo_waveform")
                            .selected_text(p.waveform.label())
                            .show_ui(ui, |ui| {
                                for wave in LfoWaveform::ALL {
                                    changed |= ui
                                        .selectable_value(&mut p.waveform, wave, wave.label())
                                        .changed();
                                }
                            });
                        ui.end_row();
                        ui.label("Sync");
                        changed |= ui.checkbox(&mut p.sync, "Follow tempo").changed();
                        ui.end_row();
                        ui.label("Rate");
                        if p.sync {
                            egui::ComboBox::from_id_salt("lfo_division")
                                .selected_text(
                                    LFO_DIVISION_LABELS[usize::from(p.division)
                                        .min(LFO_DIVISION_LABELS.len() - 1)],
                                )
                                .show_ui(ui, |ui| {
                                    for (index, label) in LFO_DIVISION_LABELS.iter().enumerate() {
                                        if ui
                                            .selectable_label(
                                                usize::from(p.division) == index,
                                                *label,
                                            )
                                            .clicked()
                                        {
                                            p.division = index as u8;
                                            changed = true;
                                        }
                                    }
                                });
                        } else {
                            changed |= ui
                                .add(
                                    egui::DragValue::new(&mut p.rate_hz)
                                        .range(0.01..=40.0)
                                        .speed(0.01)
                                        .suffix(" Hz"),
                                )
                                .changed();
                        }
                        ui.end_row();
                        ui.label("Depth");
                        changed |= ui.add(egui::Slider::new(&mut p.depth, 0.0..=1.0)).changed();
                        ui.end_row();
                    });
                lfo_wave_preview(ui, p);
                if changed {
                    self.send_node_param(
                        doc_id,
                        ParamId::LfoWaveform,
                        f32::from(p.waveform.index()),
                    );
                    self.send_node_param(doc_id, ParamId::LfoSync, if p.sync { 1.0 } else { 0.0 });
                    self.send_node_param(doc_id, ParamId::LfoRate, p.rate_hz);
                    self.send_node_param(doc_id, ParamId::LfoDivision, f32::from(p.division));
                    self.send_node_param(doc_id, ParamId::LfoDepth, p.depth);
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Lfo { params, .. }) =
                        self.project_mut().graph.node_by_id_mut(doc_id)
                    {
                        *params = p;
                    }
                    self.commit_param_edit("LFO", before, "lfo-params");
                }
            }
            // C3: the mapping card (spec `curve-map.md` §5). Three controls,
            // plus a live read-out of where the three anchor values land —
            // that read-out is what makes an axis understandable without
            // opening a curve.
            NodeDoc::CurveMap { params, .. } => {
                let mut p = params;
                let mut changed = false;
                egui::Grid::new("curve_map_params")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Low");
                        changed |= ui
                            .add(egui::DragValue::new(&mut p.min).speed(0.01))
                            .changed();
                        ui.end_row();
                        ui.label("High");
                        changed |= ui
                            .add(egui::DragValue::new(&mut p.max).speed(0.01))
                            .changed();
                        ui.end_row();
                        ui.label("Mode");
                        egui::ComboBox::from_id_salt("curve_map_mode")
                            .selected_text(p.mode.label())
                            .show_ui(ui, |ui| {
                                for mode in glitcherz_model::MapMode::ALL {
                                    changed |= ui
                                        .selectable_value(&mut p.mode, mode, mode.label())
                                        .changed();
                                }
                            });
                        ui.end_row();
                    });
                let spec = p.map().sanitized();
                ui.small(format!(
                    "−1 → {}    0 → {}    +1 → {}",
                    crate::patch::axis_number(spec.map(-1.0)),
                    crate::patch::axis_number(spec.map(0.0)),
                    crate::patch::axis_number(spec.map(1.0)),
                ));
                if changed {
                    self.send_node_param(doc_id, ParamId::CurveMapMin, p.min);
                    self.send_node_param(doc_id, ParamId::CurveMapMax, p.max);
                    self.send_node_param(
                        doc_id,
                        ParamId::CurveMapMode,
                        curve_mode_value(engine_mode(p.mode)),
                    );
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::CurveMap { params, .. }) =
                        self.project_mut().graph.node_by_id_mut(doc_id)
                    {
                        *params = p;
                    }
                    self.commit_param_edit("Curve Map", before, "curve-map-params");
                }
            }
            _ => {}
        }
    }

    pub(crate) fn adsr_sliders(&mut self, ui: &mut egui::Ui, doc_id: u64) {
        let synth = self.synth_of_node(doc_id).unwrap_or_default();
        let mut a = synth.attack_s;
        let mut d = synth.decay_s;
        let mut s = synth.sustain;
        let mut r = synth.release_s;
        ui.horizontal(|ui| {
            let sliders = [
                ("A", &mut a, AD_RANGE, ParamId::Attack),
                ("D", &mut d, AD_RANGE, ParamId::Decay),
                ("S", &mut s, S_RANGE, ParamId::Sustain),
                ("R", &mut r, R_RANGE, ParamId::Release),
            ];
            for (label, value, range, id) in sliders {
                let resp = ui.add_sized([104.0, 20.0], egui::Slider::new(value, range).text(label));
                if resp.changed() {
                    self.send_node_param(doc_id, id, *value);
                }
            }
        });
        let before = self.begin_param_edit();
        if let Some(synth) = self.synth_mut_node(doc_id) {
            synth.attack_s = a;
            synth.decay_s = d;
            synth.sustain = s;
            synth.release_s = r;
        }
        self.commit_param_edit("Envelope", before, "synth-adsr");
    }

    /// Delay three sliders (wet mix / time / feedback), bound to the target
    /// delay/effect node (G4).
    /// Sample instrument parameter section (U5): file name, root note
    /// DragValue, play-mode combo, gain slider. Root/gain edits deliver the
    /// node param immediately and write the document with one merged undo
    /// step per drag (the `adsr_sliders` pattern); the play-mode combo is a
    /// discrete one-step undoable edit.
    /// SoundFont instrument panel (U13 D10–D13).
    ///
    /// Three things, in the order they matter: which file this node plays,
    /// which preset inside it, and how loud it comes out. The file row is
    /// the node's own `Load File…` — a swap here and a swap on the canvas
    /// node run the same [`Self::pick_sound_font`], so both write the same
    /// document fields and record the same single undo step.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn sf2_section(
        &mut self,
        ui: &mut egui::Ui,
        doc_id: u64,
        path: &str,
        bank: u8,
        program: u8,
        gain: f32,
        bend_range: f32,
    ) {
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if path.is_empty() {
                ui.weak("No sound font loaded");
            } else {
                let name = Path::new(path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string());
                // File name on the label, full path on hover: short enough
                // to read, complete enough to tell two "General.sf2" apart.
                ui.label(format!("File: {name}")).on_hover_text(path);
            }
            let caption = if path.is_empty() {
                "Load File…"
            } else {
                "Replace…"
            };
            if ui
                .button(caption)
                .on_hover_text("Pick a .sf2 / .sf3 file for this node")
                .clicked()
            {
                self.pick_sound_font(doc_id);
            }
        });

        // Bank groups the patch list (LMMS style): a GM font has one bank
        // with 128 patches, a drum library one bank per kit. Entries come
        // from the *resident* library of this node's own path — the same
        // table the node's inline combos show.
        match self.sf2_presets_of(path) {
            Some(presets) => {
                let mut banks: std::collections::BTreeMap<u8, Vec<(u8, String)>> =
                    std::collections::BTreeMap::new();
                for (bank, program, name) in &presets {
                    banks
                        .entry(*bank)
                        .or_default()
                        .push((*program, name.clone()));
                }
                for patches in banks.values_mut() {
                    patches.sort_unstable_by_key(|(program, _)| *program);
                }
                let bank_list: Vec<(u8, Vec<(u8, String)>)> = banks.into_iter().collect();
                let current_name = |bank: u8, program: u8| -> Option<String> {
                    bank_list
                        .iter()
                        .find(|(b, _)| *b == bank)
                        .and_then(|(_, patches)| {
                            patches
                                .iter()
                                .find(|(p, _)| *p == program)
                                .map(|(_, name)| name.clone())
                        })
                };
                ui.horizontal(|ui| {
                    ui.label("Preset");
                    let mut bank_picked: Option<u8> = None;
                    let mut preset_picked: Option<(u8, u8)> = None;
                    egui::ComboBox::from_id_salt(format!("sf2_bank_{doc_id}"))
                        .selected_text(format!("Bank {bank}"))
                        .show_ui(ui, |ui| {
                            for (choice, _) in &bank_list {
                                if ui
                                    .selectable_label(*choice == bank, format!("Bank {choice}"))
                                    .clicked()
                                {
                                    bank_picked = Some(*choice);
                                }
                            }
                        });
                    let patch_label = match current_name(bank, program) {
                        Some(name) => format!("{program}. {name}"),
                        None => format!("{bank}:{program}"),
                    };
                    egui::ComboBox::from_id_salt(format!("sf2_preset_{doc_id}"))
                        .selected_text(patch_label)
                        .show_ui(ui, |ui| {
                            if let Some((_, patches)) = bank_list.iter().find(|(b, _)| *b == bank) {
                                egui::ScrollArea::vertical()
                                    .max_height(240.0)
                                    .show(ui, |ui| {
                                        for (candidate, name) in patches {
                                            if ui
                                                .selectable_label(
                                                    *candidate == program,
                                                    format!("{candidate}. {name}"),
                                                )
                                                .clicked()
                                            {
                                                preset_picked = Some((bank, *candidate));
                                            }
                                        }
                                    });
                            }
                        });
                    if let Some(next) = bank_picked {
                        if next != bank {
                            // Keep the program number when the new bank has
                            // it; otherwise fall back to the bank's first.
                            let target = bank_list.iter().find(|(b, _)| *b == next).and_then(
                                |(_, patches)| {
                                    patches
                                        .iter()
                                        .find(|(p, _)| *p == program)
                                        .map(|_| program)
                                        .or_else(|| patches.first().map(|(p, _)| *p))
                                },
                            );
                            if let Some(program) = target {
                                preset_picked = Some((next, program));
                            }
                        }
                    }
                    if let Some((bank, program)) = preset_picked {
                        self.set_soundfont_preset(doc_id, bank, program);
                    }
                });
            }
            None => {
                ui.weak(if path.is_empty() {
                    "(load a file to pick a preset)"
                } else {
                    "(not loaded this session — Replace… re-reads it)"
                });
            }
        }

        // Output gain. A font's level is a **source** property, so the trim
        // belongs to the node rather than to an extra gain node in the
        // graph: it travels with the document, undoes with it, and cannot
        // be accidentally left behind when the voice is swapped. Default is
        // 1.0 — the engine forces rustysynth's own default back from 0.5,
        // which was silently costing every SoundFont 6 dB.
        ui.horizontal(|ui| {
            ui.label("Gain");
            let mut value = gain;
            if ui
                .add(egui::Slider::new(&mut value, 0.0..=2.0).fixed_decimals(2))
                .changed()
            {
                self.send_node_param(doc_id, ParamId::Sf2Gain, value);
                let before = self.begin_param_edit();
                if let Some(NodeDoc::Instrument {
                    voice: VoiceSpec::SoundFont { gain, .. },
                    ..
                }) = self.project_mut().graph.node_by_id_mut(doc_id)
                {
                    *gain = value.clamp(0.0, 2.0);
                }
                self.commit_param_edit("SoundFont Gain", before, "sf2-gain");
            }
        });

        // Pitch-bend range (spec `curve-map.md` D8/D9): the instrument's
        // 14-bit bend spans this many semitones. A control curve mapped to
        // `Pitch Bend` therefore covers exactly this range.
        ui.horizontal(|ui| {
            ui.label("Bend Range");
            let mut value = bend_range;
            if ui
                .add(
                    egui::Slider::new(&mut value, 1.0..=24.0)
                        .fixed_decimals(0)
                        .suffix(" st"),
                )
                .changed()
            {
                self.send_node_param(doc_id, ParamId::Sf2BendRange, value);
                let before = self.begin_param_edit();
                if let Some(NodeDoc::Instrument {
                    voice: VoiceSpec::SoundFont { bend_range, .. },
                    ..
                }) = self.project_mut().graph.node_by_id_mut(doc_id)
                {
                    *bend_range = value.clamp(1.0, 24.0);
                }
                self.commit_param_edit("SoundFont Bend Range", before, "sf2-bend-range");
                // The axis of every bend cable on this node is the range
                // (D12), so it has to be re-sent, not just stored.
                self.refresh_maps_for(doc_id);
            }
        });
    }

    /// Preset table of the node's font, when it is resident (U13). `None`
    /// means "not loaded this session" — the panel says so instead of
    /// offering an empty list.
    pub(crate) fn sf2_presets_of(&self, path: &str) -> Option<Vec<(u8, u8, String)>> {
        if path.is_empty() {
            return None;
        }
        self.sf2_preset_names.get(Path::new(path)).cloned()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn sample_section(
        &mut self,
        ui: &mut egui::Ui,
        doc_id: u64,
        path: &str,
        root_note: u8,
        one_shot: bool,
        gain: f32,
        region: glitcherz_model::SampleRegion,
    ) {
        // File row first, exactly like the SoundFont panel: which file this
        // node plays is the question that matters most, and the node's own
        // button runs the same `pick_sample_file`.
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if path.is_empty() {
                ui.weak("No sample loaded");
            } else {
                let name = std::path::Path::new(path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string());
                ui.label(format!("File: {name}")).on_hover_text(path);
            }
            let caption = if path.is_empty() {
                "Load File…"
            } else {
                "Replace…"
            };
            if ui
                .button(caption)
                .on_hover_text("Pick an audio file for this node")
                .clicked()
            {
                self.pick_sample_file(doc_id);
            }
        });
        let mut root = f32::from(root_note);
        let mut gain_value = gain;
        ui.horizontal(|ui| {
            ui.label("Root note");
            if ui
                .add(
                    egui::DragValue::new(&mut root)
                        .range(0.0..=127.0)
                        .speed(1.0),
                )
                .changed()
            {
                self.send_node_param(doc_id, ParamId::SampleRootNote, root.round());
            }
        });
        ui.horizontal(|ui| {
            ui.label("Play mode");
            let mut picked: Option<bool> = None;
            egui::ComboBox::from_id_salt(format!("sample_mode_{doc_id}"))
                .selected_text(if one_shot { "One-shot" } else { "Gate" })
                .show_ui(ui, |ui| {
                    if ui.selectable_label(!one_shot, "Gate").clicked() {
                        picked = Some(false);
                    }
                    if ui.selectable_label(one_shot, "One-shot").clicked() {
                        picked = Some(true);
                    }
                });
            if let Some(next) = picked {
                if next != one_shot {
                    self.set_sample_play_mode(doc_id, next);
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Gain");
            if ui
                .add(egui::Slider::new(&mut gain_value, 0.0..=2.0))
                .changed()
            {
                self.send_node_param(doc_id, ParamId::SampleGain, gain_value);
            }
        });
        let before = self.begin_param_edit();
        if let Some(NodeDoc::Instrument {
            voice: VoiceSpec::Sample {
                root_note, gain, ..
            },
            ..
        }) = self.project_mut().graph.node_by_id_mut(doc_id)
        {
            *root_note = root.round().clamp(0.0, 127.0) as u8;
            *gain = gain_value.clamp(0.0, 2.0);
        }
        self.commit_param_edit("Sample Params", before, "sample-params");

        // D12: the play window. Drawn from the cached decode, so it appears as
        // soon as the file is loaded and carries no extra UI state.
        if !path.is_empty() {
            self.sample_region_section(ui, doc_id, path, region);
        }
    }

    /// Play window editor (spec `sample-player.md` D12): a peak strip with
    /// four draggable handles, the loop mode, and the exact frame numbers.
    ///
    /// The strip is the gesture and the numbers are the truth — dragging is
    /// coarse, typing is exact, and both write the same document fields
    /// through [`Self::apply_sample_region`]: one merged undo step per drag
    /// stroke, five live `set_node_param` writes per change (no rebuild, the
    /// window is frozen into voices at trigger time).
    fn sample_region_section(
        &mut self,
        ui: &mut egui::Ui,
        doc_id: u64,
        path: &str,
        region: glitcherz_model::SampleRegion,
    ) {
        let Some(data) = self.sample_data_of(path) else {
            ui.weak("Trim needs the decoded file — load it first.");
            return;
        };
        let frames = data.frames().len() as u32;
        if frames < 2 {
            return;
        }
        let resolved = region.resolve(frames);
        let editing = resolved.to_region();
        let last = frames - 1;

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.strong("Play window");
            ui.weak(format!(
                "{:.2} s · {frames} frames",
                f64::from(frames) / f64::from(data.sample_rate())
            ));
            if !region.is_full_sample()
                && ui
                    .small_button("Full")
                    .on_hover_text("Play the whole file again")
                    .clicked()
            {
                self.apply_sample_region(doc_id, glitcherz_model::SampleRegion::default());
                self.end_param_edit();
            }
        });

        let width = ui.available_width().max(160.0);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, 76.0), egui::Sense::click_and_drag());
        draw_sample_region(ui, rect, &data, &editing, resolved);

        // A drag grabs the nearest handle **once** and keeps it for the whole
        // stroke: a handle that slides under the pointer must not steal it.
        let mut dragged: Option<glitcherz_model::SampleRegion> = None;
        if response.drag_started()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let handle = (0..REGION_HANDLES)
                .min_by(|a, b| {
                    let da = (region_handle_x(rect, *a, &editing, last) - pos.x).abs();
                    let db = (region_handle_x(rect, *b, &editing, last) - pos.x).abs();
                    da.total_cmp(&db)
                })
                .unwrap_or(0);
            ui.data_mut(|d| d.insert_temp(response.id, handle as u8));
        }
        if response.dragged()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let handle = ui.data(|d| d.get_temp::<u8>(response.id)).unwrap_or(0) as usize;
            let frame = region_frame_at(rect, pos.x, last);
            dragged = Some(move_region_handle(&editing, handle, frame, last));
        }
        if let Some(next) = dragged {
            self.apply_sample_region(doc_id, next);
        }
        if response.drag_stopped() {
            self.end_param_edit();
        }

        let mut picked: Option<glitcherz_model::SampleLoopMode> = None;
        ui.horizontal(|ui| {
            ui.label("Loop");
            egui::ComboBox::from_id_salt(format!("sample_loop_{doc_id}"))
                .selected_text(editing.loop_mode.label())
                .show_ui(ui, |ui| {
                    for mode in glitcherz_model::SampleLoopMode::ALL {
                        if ui
                            .selectable_label(editing.loop_mode == mode, mode.label())
                            .clicked()
                        {
                            picked = Some(mode);
                        }
                    }
                });
            ui.weak(match editing.loop_mode {
                glitcherz_model::SampleLoopMode::Off => "plays straight through",
                glitcherz_model::SampleLoopMode::Forward => "repeats while the key is held",
                glitcherz_model::SampleLoopMode::PingPong => "bounces while the key is held",
            });
        });
        if let Some(mode) = picked
            && mode != editing.loop_mode
        {
            let mut next = editing;
            next.loop_mode = mode;
            self.apply_sample_region(doc_id, next);
            self.end_param_edit();
        }

        // Exact values. Each handle is clamped against its neighbours, so the
        // four stay ordered no matter what gets typed.
        let mut values = [
            editing.start,
            editing.loop_start,
            editing.loop_end,
            editing.end,
        ];
        let mut typed = false;
        ui.horizontal(|ui| {
            for (index, label) in ["Start", "Loop A", "Loop B", "End"].iter().enumerate() {
                ui.label(*label);
                typed |= ui
                    .add(
                        egui::DragValue::new(&mut values[index])
                            .range(0..=last)
                            .speed(4.0),
                    )
                    .on_hover_text(format!(
                        "{:.1} ms",
                        f64::from(values[index]) * 1000.0 / f64::from(data.sample_rate())
                    ))
                    .changed();
            }
        });
        if typed {
            let mut next = editing;
            for (index, frame) in values.iter().enumerate() {
                next = move_region_handle(&next, index, *frame, last);
            }
            self.apply_sample_region(doc_id, next);
            self.end_param_edit();
        }
    }

    /// Decoded sample behind a node path (session cache; decodes on first
    /// use, control thread only).
    fn sample_data_of(&mut self, path: &str) -> Option<Arc<glitcherz_engine::sample::SampleData>> {
        if path.is_empty() {
            return None;
        }
        let raw = Path::new(path);
        let resolved = match (&self.project().base_dir, raw.is_absolute()) {
            (Some(dir), false) => dir.join(raw),
            _ => raw.to_path_buf(),
        };
        self.sample_library
            .get(&resolved)
            .or_else(|| self.sample_library.load(&resolved).ok())
    }

    /// Writes the play window and delivers it to the sounding node.
    ///
    /// Merged undo (`sample-region`): a drag is one step; the typed and combo
    /// paths call [`Self::end_param_edit`] right after, so each of those is a
    /// step of its own.
    fn apply_sample_region(&mut self, doc_id: u64, region: glitcherz_model::SampleRegion) {
        let before = self.begin_param_edit();
        if let Some(NodeDoc::Instrument {
            voice: VoiceSpec::Sample {
                region: current, ..
            },
            ..
        }) = self.project_mut().graph.node_by_id_mut(doc_id)
        {
            *current = region;
        }
        self.commit_param_edit("Sample Window", before, "sample-region");
        self.send_node_param(doc_id, ParamId::SampleStart, region.start as f32);
        self.send_node_param(doc_id, ParamId::SampleLoopStart, region.loop_start as f32);
        self.send_node_param(doc_id, ParamId::SampleLoopEnd, region.loop_end as f32);
        self.send_node_param(doc_id, ParamId::SampleEnd, region.end as f32);
        self.send_node_param(
            doc_id,
            ParamId::SampleLoopMode,
            f32::from(region.loop_mode.index()),
        );
    }

    /// Drum kit table (W1, spec `drum-kit.md` K11): timeline-style overall
    /// trim, the multi-select load button and one row per slot. Gain / pan /
    /// tune / choke are **live** `set_node_param` writes (no rebuild); the
    /// trigger key is a note-name drag that rebuilds the kit once per drop —
    /// the RT table is indexed by key at construction, so a moved slot needs
    /// a new processor, and `rebuild_drum_kit` swaps it in place.
    /// Modal strike instrument (B1, spec `modal-instrument.md`): preset plus
    /// the four macros. Every control is one undo step and goes live through
    /// `send_node_param`, the same path the inline rows use.
    pub(crate) fn modal_section(
        &mut self,
        ui: &mut egui::Ui,
        doc_id: u64,
        params: &glitcherz_model::ModalParams,
    ) {
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.strong("Modal Strike");
            ui.weak("— struck metal; note-off does not damp the tail");
        });

        let mut preset = params.preset;
        ui.horizontal(|ui| {
            ui.label("Preset");
            egui::ComboBox::from_id_salt(format!("modal_preset_{doc_id}"))
                .selected_text(preset.label())
                .show_ui(ui, |ui| {
                    for option in glitcherz_model::ModalPreset::ALL {
                        ui.selectable_value(&mut preset, option, option.label());
                    }
                });
        });
        if preset != params.preset {
            let before = self.begin_edit();
            if let Some(NodeDoc::Instrument {
                voice: VoiceSpec::Modal { params },
                ..
            }) = self.project_mut().graph.node_by_id_mut(doc_id)
            {
                params.preset = preset;
            }
            self.commit_edit("Modal Preset", before);
            self.send_node_param(doc_id, ParamId::ModalPreset, f32::from(preset.index()));
        }

        let rows: [(&str, f32, f32, ParamId, &str, &str); 5] = [
            (
                "Decay",
                0.25,
                4.0,
                ParamId::ModalDecay,
                "modal-decay",
                "Ring time multiplier over every mode (T60)",
            ),
            (
                "Beating",
                0.0,
                2.0,
                ParamId::ModalBeating,
                "modal-beating",
                "Detune between paired modes · 0 = perfectly harmonic",
            ),
            (
                "Hardness",
                0.0,
                1.0,
                ParamId::ModalHardness,
                "modal-hardness",
                "Strike length · affects the next hit only",
            ),
            (
                "Brightness",
                0.0,
                1.0,
                ParamId::ModalBrightness,
                "modal-brightness",
                "Strike low-pass · affects the next hit only",
            ),
            (
                "Gain",
                0.0,
                2.0,
                ParamId::ModalGain,
                "modal-gain",
                "Node output trim",
            ),
        ];
        let mut values = [
            params.decay_scale,
            params.beating,
            params.hardness,
            params.brightness,
            params.gain,
        ];
        let mut changed: Vec<(usize, f32)> = Vec::new();
        egui::Grid::new(format!("modal_grid_{doc_id}"))
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                for (index, (label, min, max, _, _, tooltip)) in rows.iter().enumerate() {
                    ui.label(*label);
                    let response = ui
                        .add(egui::Slider::new(&mut values[index], *min..=*max).fixed_decimals(2))
                        .on_hover_text(*tooltip);
                    if response.changed() {
                        changed.push((index, values[index]));
                    }
                    ui.end_row();
                }
            });
        for (index, value) in changed {
            let before = self.begin_param_edit();
            if let Some(NodeDoc::Instrument {
                voice: VoiceSpec::Modal { params },
                ..
            }) = self.project_mut().graph.node_by_id_mut(doc_id)
            {
                match index {
                    0 => params.decay_scale = value.clamp(0.25, 4.0),
                    1 => params.beating = value.clamp(0.0, 2.0),
                    2 => params.hardness = value.clamp(0.0, 1.0),
                    3 => params.brightness = value.clamp(0.0, 1.0),
                    4 => params.gain = value.clamp(0.0, 2.0),
                    _ => {}
                }
            }
            self.commit_param_edit(rows[index].0, before, rows[index].4);
            self.send_node_param(doc_id, rows[index].3, value);
        }
    }

    /// Plucked string (S1, spec `pluck-instrument.md`): five macros. `decay`
    /// and `damping` are also the two inline rows; the rest live here only.
    pub(crate) fn pluck_section(
        &mut self,
        ui: &mut egui::Ui,
        doc_id: u64,
        params: &glitcherz_model::PluckParams,
    ) {
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.strong("Plucked String");
            ui.weak("— Karplus-Strong; note-off damps the string");
        });

        let rows: [(&str, f32, f32, ParamId, &str, &str); 5] = [
            (
                "Decay",
                0.1,
                20.0,
                ParamId::PluckDecay,
                "pluck-decay",
                "Fundamental T60 in seconds",
            ),
            (
                "Damping",
                0.0,
                1.0,
                ParamId::PluckDamping,
                "pluck-damping",
                "Loop low-pass · darker and shorter as it rises",
            ),
            (
                "Brightness",
                0.0,
                1.0,
                ParamId::PluckBrightness,
                "pluck-brightness",
                "Excitation low-pass · affects the next pluck only",
            ),
            (
                "Release",
                0.005,
                2.0,
                ParamId::PluckRelease,
                "pluck-release",
                "Note-off damper T60",
            ),
            (
                "Gain",
                0.0,
                2.0,
                ParamId::PluckGain,
                "pluck-gain",
                "Node output trim",
            ),
        ];
        let mut values = [
            params.decay_s,
            params.damping,
            params.brightness,
            params.release_s,
            params.gain,
        ];
        let mut changed: Vec<(usize, f32)> = Vec::new();
        egui::Grid::new(format!("pluck_grid_{doc_id}"))
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                for (index, (label, min, max, _, _, tooltip)) in rows.iter().enumerate() {
                    ui.label(*label);
                    let response = ui
                        .add(egui::Slider::new(&mut values[index], *min..=*max).fixed_decimals(2))
                        .on_hover_text(*tooltip);
                    if response.changed() {
                        changed.push((index, values[index]));
                    }
                    ui.end_row();
                }
            });
        for (index, value) in changed {
            let before = self.begin_param_edit();
            if let Some(NodeDoc::Instrument {
                voice: VoiceSpec::Pluck { params },
                ..
            }) = self.project_mut().graph.node_by_id_mut(doc_id)
            {
                match index {
                    0 => params.decay_s = value.clamp(0.1, 20.0),
                    1 => params.damping = value.clamp(0.0, 1.0),
                    2 => params.brightness = value.clamp(0.0, 1.0),
                    3 => params.release_s = value.clamp(0.005, 2.0),
                    4 => params.gain = value.clamp(0.0, 2.0),
                    _ => {}
                }
            }
            self.commit_param_edit(rows[index].0, before, rows[index].4);
            self.send_node_param(doc_id, rows[index].3, value);
        }
    }

    /// Drawbar organ (S2, spec `organ-instrument.md`): the full registration
    /// plus percussion and the key-switch ramps. The 8' bar and the percussion
    /// level are also the two inline rows; everything else lives here.
    pub(crate) fn organ_section(
        &mut self,
        ui: &mut egui::Ui,
        doc_id: u64,
        params: &glitcherz_model::OrganParams,
    ) {
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.strong("Drawbar Organ");
            ui.weak("— 3 dB per step; velocity does not change the level");
        });

        // 3×3 drawbar grid: footage above, level below (0..8, integer steps).
        let bars = params.bars;
        let mut bar_change: Option<(usize, u8)> = None;
        egui::Grid::new(format!("organ_bars_{doc_id}"))
            .num_columns(3)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                for (index, stored) in bars.iter().enumerate() {
                    ui.vertical(|ui| {
                        ui.label(crate::ORGAN_FOOTAGES[index]);
                        let mut level = i32::from(*stored);
                        let response = ui
                            .add(egui::Slider::new(&mut level, 0..=8).show_value(true))
                            .on_hover_text("Drawbar level · 0 = off, 8 = full");
                        if response.changed() {
                            bar_change = Some((index, level.clamp(0, 8) as u8));
                        }
                    });
                    if index % 3 == 2 {
                        ui.end_row();
                    }
                }
            });
        if let Some((index, level)) = bar_change {
            let before = self.begin_param_edit();
            if let Some(NodeDoc::Instrument {
                voice: VoiceSpec::Organ { params },
                ..
            }) = self.project_mut().graph.node_by_id_mut(doc_id)
            {
                params.bars[index] = level;
            }
            self.commit_param_edit(
                &format!("Organ {} Bar", crate::ORGAN_FOOTAGES[index]),
                before,
                "organ-bar",
            );
            self.send_node_param(doc_id, ParamId::OrganBar(index as u8), f32::from(level));
        }

        // Percussion: discrete harmonic, level and decay.
        let mut third = params.percussion_third;
        let harmonic_before = third;
        ui.horizontal(|ui| {
            ui.label("Percussion");
            egui::ComboBox::from_id_salt(format!("organ_perc_{doc_id}"))
                .selected_text(if third { "3rd (2⅔')" } else { "2nd (4')" })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut third, false, "2nd (4')");
                    ui.selectable_value(&mut third, true, "3rd (2⅔')");
                });
        });
        if third != harmonic_before {
            let before = self.begin_edit();
            if let Some(NodeDoc::Instrument {
                voice: VoiceSpec::Organ { params },
                ..
            }) = self.project_mut().graph.node_by_id_mut(doc_id)
            {
                params.percussion_third = third;
            }
            self.commit_edit("Organ Percussion Harmonic", before);
            self.send_node_param(
                doc_id,
                ParamId::OrganPercussionHarmonic,
                if third { 1.0 } else { 0.0 },
            );
        }

        let rows: [(&str, f32, f32, ParamId, &str, &str); 5] = [
            (
                "Perc Level",
                0.0,
                1.0,
                ParamId::OrganPercussion,
                "organ-percussion",
                "Percussion partial level · affects the next note only",
            ),
            (
                "Perc Decay",
                0.1,
                2.0,
                ParamId::OrganPercussionDecay,
                "organ-percussion-decay",
                "Percussion T60 · affects the next note only",
            ),
            (
                "Attack",
                0.001,
                0.2,
                ParamId::OrganAttack,
                "organ-attack",
                "Key-switch ramp in · too short clicks",
            ),
            (
                "Release",
                0.005,
                1.0,
                ParamId::OrganRelease,
                "organ-release",
                "Key-switch ramp out",
            ),
            (
                "Gain",
                0.0,
                2.0,
                ParamId::OrganGain,
                "organ-gain",
                "Node output trim",
            ),
        ];
        let mut values = [
            params.percussion,
            params.percussion_decay_s,
            params.attack_s,
            params.release_s,
            params.gain,
        ];
        let mut changed: Vec<(usize, f32)> = Vec::new();
        egui::Grid::new(format!("organ_grid_{doc_id}"))
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                for (index, (label, min, max, _, _, tooltip)) in rows.iter().enumerate() {
                    ui.label(*label);
                    let response = ui
                        .add(egui::Slider::new(&mut values[index], *min..=*max).fixed_decimals(3))
                        .on_hover_text(*tooltip);
                    if response.changed() {
                        changed.push((index, values[index]));
                    }
                    ui.end_row();
                }
            });
        for (index, value) in changed {
            let before = self.begin_param_edit();
            if let Some(NodeDoc::Instrument {
                voice: VoiceSpec::Organ { params },
                ..
            }) = self.project_mut().graph.node_by_id_mut(doc_id)
            {
                match index {
                    0 => params.percussion = value.clamp(0.0, 1.0),
                    1 => params.percussion_decay_s = value.clamp(0.1, 2.0),
                    2 => params.attack_s = value.clamp(0.001, 0.2),
                    3 => params.release_s = value.clamp(0.005, 1.0),
                    4 => params.gain = value.clamp(0.0, 2.0),
                    _ => {}
                }
            }
            self.commit_param_edit(rows[index].0, before, rows[index].4);
            self.send_node_param(doc_id, rows[index].3, value);
        }
    }

    pub(crate) fn drum_section(
        &mut self,
        ui: &mut egui::Ui,
        doc_id: u64,
        slots: &[glitcherz_model::DrumSlot],
        gain: f32,
    ) {
        ui.separator();
        ui.add_space(4.0);
        let mut gain_value = gain;
        ui.horizontal(|ui| {
            ui.strong(format!("Drum Kit · {} slots", slots.len()));
            if ui
                .button("Load Samples…")
                .on_hover_text("Append wav files as slots with free MIDI keys")
                .clicked()
            {
                self.pick_drum_samples(doc_id);
            }
            ui.label("Gain");
            let gain_response = ui
                .add(
                    egui::Slider::new(&mut gain_value, 0.0..=2.0)
                        .custom_formatter(|value, _| gain_db_text(value as f32)),
                )
                .on_hover_text("Kit output trim · double-click the rail for 0 dB");
            if gain_response.double_clicked() {
                gain_value = 1.0;
                self.send_node_param(doc_id, ParamId::DrumGain, gain_value);
            } else if gain_response.changed() {
                self.send_node_param(doc_id, ParamId::DrumGain, gain_value);
            }
        });
        let before = self.begin_param_edit();
        if let Some(NodeDoc::Instrument {
            voice: VoiceSpec::DrumKit { gain, .. },
            ..
        }) = self.project_mut().graph.node_by_id_mut(doc_id)
        {
            *gain = gain_value.clamp(0.0, 2.0);
        }
        self.commit_param_edit("Drum Gain", before, "drum-gain");

        if slots.is_empty() {
            ui.weak("No samples loaded — pick wav files to build the kit.");
            return;
        }
        let mut edits: Vec<(u8, u8, f32)> = Vec::new();
        let mut remove: Option<u8> = None;
        let mut move_key: Option<(u8, u8)> = None;
        egui::Grid::new(format!("drum_grid_{doc_id}"))
            .num_columns(7)
            .spacing([8.0, 2.0])
            .show(ui, |ui| {
                ui.strong("Key");
                ui.strong("File");
                ui.strong("Gain");
                ui.strong("Pan");
                ui.strong("Tune");
                ui.strong("Choke");
                ui.label("");
                ui.end_row();
                for slot in slots {
                    // The key reads as a note name (`C2 · 36`) and drags like a
                    // number: a drum slot is identified by where it sounds, and
                    // a bare 36 has no visible row in the roll. The rebuild is
                    // deferred to the drop so one drag is one rebuild and one
                    // undo step.
                    let mut key = u16::from(slot.note);
                    let key_response = ui
                        .add_sized(
                            [64.0, ui.spacing().interact_size.y],
                            egui::DragValue::new(&mut key)
                                .range(0..=127)
                                .speed(1.0)
                                .custom_formatter(|value, _| {
                                    let note = value.round().clamp(0.0, 127.0) as u8;
                                    format!("{} · {note}", glitcherz_model::note_name(note))
                                }),
                        )
                        .on_hover_text(
                            "Trigger key — drag to move this slot. Keys already in \
                             use snap to the nearest free one.",
                        );
                    // The drop frame carries no value change (egui clears its
                    // drag accumulator on release), so the last dragged key is
                    // remembered per widget id until the button comes up.
                    if key_response.drag_stopped() {
                        let target = ui
                            .data(|data| data.get_temp::<u8>(key_response.id))
                            .unwrap_or(slot.note);
                        ui.data_mut(|data| data.remove::<u8>(key_response.id));
                        move_key = Some((slot.note, target));
                    } else if key_response.changed() && !key_response.dragged() {
                        move_key = Some((slot.note, key.min(127) as u8));
                    } else if key_response.dragged() && key_response.changed() {
                        ui.data_mut(|data| data.insert_temp(key_response.id, key.min(127) as u8));
                    }
                    let stem = std::path::Path::new(&slot.path)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "?".to_string());
                    ui.label(stem).on_hover_text(&slot.path);
                    // Gain is stored linear but reads in dB: "0.63" tells
                    // nobody anything, "-4.0 dB" does. Same formatter as the
                    // master fader, so the two speak one language. A
                    // double-click on the rail returns the slot to unity —
                    // the one value everybody resets to.
                    let mut gain = slot.gain;
                    let gain_response = ui.add(
                        egui::Slider::new(&mut gain, 0.0..=2.0)
                            .custom_formatter(|value, _| gain_db_text(value as f32)),
                    );
                    if gain_response.double_clicked() {
                        edits.push((slot.note, 0, 1.0));
                    } else if gain_response.changed() {
                        edits.push((slot.note, 0, gain));
                    }
                    let mut pan = slot.pan;
                    if ui
                        .add(egui::DragValue::new(&mut pan).range(-1.0..=1.0).speed(0.01))
                        .changed()
                    {
                        edits.push((slot.note, 1, pan));
                    }
                    let mut tune = slot.tune_semitones;
                    if ui
                        .add(
                            egui::DragValue::new(&mut tune)
                                .range(-24.0..=24.0)
                                .speed(0.1)
                                .suffix(" st"),
                        )
                        .changed()
                    {
                        edits.push((slot.note, 2, tune));
                    }
                    let mut choke = slot.choke.map_or(-1.0, f32::from);
                    if ui
                        .add(
                            egui::DragValue::new(&mut choke)
                                .range(-1.0..=127.0)
                                .speed(0.2),
                        )
                        .on_hover_text(
                            "-1 = no choke group; a new hit in the same group cuts the old one",
                        )
                        .changed()
                    {
                        edits.push((slot.note, 3, choke));
                    }
                    if ui
                        .small_button("×")
                        .on_hover_text("Remove this slot")
                        .clicked()
                    {
                        remove = Some(slot.note);
                    }
                    ui.end_row();
                }
            });
        for (note, field, value) in edits {
            self.apply_drum_slot_edit(doc_id, note, field, value);
        }
        if let Some((from, to)) = move_key {
            self.move_drum_slot(doc_id, from, to);
        }
        if let Some(note) = remove {
            self.remove_drum_slot(doc_id, note);
        }
    }

    /// Writes one slot parameter (0 gain / 1 pan / 2 tune / 3 choke) and
    /// forwards it to the live processor. Undo merges per slot + field.
    pub(crate) fn apply_drum_slot_edit(&mut self, doc_id: u64, note: u8, field: u8, value: f32) {
        let before = self.begin_param_edit();
        if let Some(NodeDoc::Instrument {
            voice: VoiceSpec::DrumKit { slots, .. },
            ..
        }) = self.project_mut().graph.node_by_id_mut(doc_id)
            && let Some(slot) = slots.iter_mut().find(|slot| slot.note == note)
        {
            match field {
                0 => slot.gain = value.clamp(0.0, 2.0),
                1 => slot.pan = value.clamp(-1.0, 1.0),
                2 => slot.tune_semitones = value.clamp(-24.0, 24.0),
                3 => {
                    slot.choke = if value < 0.0 {
                        None
                    } else {
                        Some(value.round().clamp(0.0, 127.0) as u8)
                    }
                }
                _ => {}
            }
        }
        let (id, key) = match field {
            0 => (ParamId::DrumSlotGain(note), "drum-slot-gain"),
            1 => (ParamId::DrumSlotPan(note), "drum-slot-pan"),
            2 => (ParamId::DrumSlotTune(note), "drum-slot-tune"),
            _ => (ParamId::DrumSlotChoke(note), "drum-slot-choke"),
        };
        self.commit_param_edit("Drum Slot", before, &format!("{key}:{doc_id}:{note}"));
        self.send_node_param(doc_id, id, value);
    }

    pub(crate) fn effect_sliders(&mut self, ui: &mut egui::Ui, doc_id: u64) {
        let Some(initial) = self.effect_params_of_node(doc_id) else {
            return;
        };
        let descriptor = crate::effect::descriptor(&initial);

        // Gain-reduction row (E2b): only effects that publish meters have an
        // entry in the map, so the row appears exactly where it makes sense.
        if let Some(meters) = self.effect_meters.get(&doc_id) {
            let reading = meters.read();
            let reduction = (-reading.gr_db).max(0.0);
            ui.horizontal(|ui| {
                ui.label("GR");
                ui.add(
                    egui::ProgressBar::new((reduction / 24.0).clamp(0.0, 1.0))
                        .desired_width(120.0)
                        .text(format!("-{reduction:.1} dB")),
                );
                ui.label(format!(
                    "peak {:.1} / {:.1} dB",
                    level_db(reading.peak_l),
                    level_db(reading.peak_r)
                ));
            });
        }

        let slots = descriptor.params.len();
        let mut values: Vec<f32> = (0..slots)
            .map(|slot| crate::effect::value(&initial, slot as u8))
            .collect();
        ui.horizontal_wrapped(|ui| {
            for (slot, spec) in descriptor.params.iter().enumerate() {
                let id = ParamId::BuiltinParam(slot as u8);
                if spec.unit == Unit::Enum {
                    // Discrete choice (e.g. delay tempo-sync division): a combo
                    // box instead of a slider (the raw value is an index).
                    let current = values[slot].round().max(0.0) as usize;
                    let selected = spec.choices.get(current).copied().unwrap_or("-");
                    egui::ComboBox::from_id_salt(format!("effect_enum_{doc_id}_{slot}"))
                        .selected_text(format!("{}: {}", spec.name, selected))
                        .show_ui(ui, |ui| {
                            for (index, choice) in spec.choices.iter().enumerate() {
                                if ui.selectable_label(index == current, *choice).clicked() {
                                    values[slot] = index as f32;
                                    self.send_node_param(doc_id, id, values[slot]);
                                }
                            }
                        });
                    continue;
                }
                if spec.is_discrete() {
                    let mut on = values[slot] >= 0.5;
                    if ui.checkbox(&mut on, spec.name).changed() {
                        values[slot] = if on { 1.0 } else { 0.0 };
                        self.send_node_param(doc_id, id, values[slot]);
                    }
                    continue;
                }
                let resp = ui.add_sized(
                    [132.0, 20.0],
                    egui::Slider::new(&mut values[slot], spec.min..=spec.max)
                        .logarithmic(matches!(spec.curve, Curve::Log))
                        .text(spec.name),
                );
                if resp.changed() {
                    self.send_node_param(doc_id, id, values[slot]);
                }
            }
        });
        let before = self.begin_param_edit();
        if let Some(params) = self.effect_params_mut_node(doc_id) {
            for (slot, value) in values.iter().enumerate() {
                crate::effect::set_value(params, slot as u8, *value);
            }
        }
        self.commit_param_edit("Effect Params", before, "effect-params");
    }

    /// Send one document node's parameter to the audio thread (G4 node
    /// addressing; no-op when the node has no engine counterpart).
    pub(crate) fn send_node_param(&mut self, doc_id: u64, id: ParamId, value: f32) {
        if let Some(&engine_id) = self.node_map.get(&doc_id) {
            self.engine.send(EngineCommand::SetNodeParam {
                node_id: engine_id,
                id,
                value,
            });
        }
    }

    /// Tell a VST3 plugin's edit controller about a host-driven parameter
    /// change so the plugin's own GUI follows (spec `plugin-gui.md` §9 —
    /// the `IEditControllerHostEditing` gesture). CLAP needs no equivalent
    /// (D10: the CLAP contract syncs on `set_value`). `value` is the VST3
    /// normalized value. No-op for non-VST3 nodes.
    pub(crate) fn sync_vst3_host_param(&mut self, doc_id: u64, id: u32, value: f64) {
        if let Some(guard) = self.node_guard(doc_id)
            && let PluginGuard::Vst3(g) = guard
        {
            g.set_param_from_host(id, value);
        }
    }

    /// Apply one inline parameter change from a node-body slider (G5 spec
    /// §7): document write (undo-merged under the same gesture key as the
    /// matching panel slider) + the non-rebuilding `set_node_param` path.
    /// Plugin rows additionally update the session value cache so the panel
    /// sliders and inline sliders read the same current value.
    pub(crate) fn apply_inline_param(
        &mut self,
        node: u64,
        param: patch::InlineParamId,
        value: f32,
    ) {
        match param {
            patch::InlineParamId::BuiltIn(p) => match p {
                patch::InlineParam::Cutoff => {
                    let before = self.begin_param_edit();
                    if let Some(s) = self.synth_mut_node(node) {
                        s.cutoff_hz = value;
                    }
                    self.commit_param_edit("Filter Cutoff", before, "synth-cutoff");
                    self.send_node_param(node, inline_param_id(p), value);
                }
                patch::InlineParam::Resonance => {
                    let before = self.begin_param_edit();
                    if let Some(s) = self.synth_mut_node(node) {
                        s.resonance = value;
                    }
                    self.commit_param_edit("Filter Resonance", before, "synth-res");
                    self.send_node_param(node, inline_param_id(p), value);
                }
                patch::InlineParam::EffectSlot(slot) => {
                    let before = self.begin_param_edit();
                    if let Some(params) = self.effect_params_mut_node(node) {
                        crate::effect::set_value(params, slot, value);
                    }
                    self.commit_param_edit("Effect Params", before, "effect-params");
                    self.send_node_param(node, inline_param_id(p), value);
                }
                patch::InlineParam::MixerOutPan => {
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Mixer { out_pan, .. }) =
                        self.project_mut().graph.node_by_id_mut(node)
                    {
                        *out_pan = value;
                    }
                    self.commit_param_edit("Mixer Out Pan", before, "mixer-out-pan");
                    self.send_node_param(node, inline_param_id(p), value);
                }
                patch::InlineParam::MixerGain(ch) => {
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Mixer { channels, .. }) =
                        self.project_mut().graph.node_by_id_mut(node)
                        && let Some(c) = channels.get_mut(ch as usize)
                    {
                        c.gain = value;
                    }
                    // Key carries the channel: two faders dragged in a row are
                    // two undo steps, not one merged step.
                    self.commit_param_edit("Mixer Gain", before, &format!("mixer-gain-{ch}"));
                    self.send_node_param(node, inline_param_id(p), value);
                }
                patch::InlineParam::MixerMute(ch) => {
                    let muted = value >= 0.5;
                    let before = self.begin_edit();
                    if let Some(NodeDoc::Mixer { channels, .. }) =
                        self.project_mut().graph.node_by_id_mut(node)
                        && let Some(c) = channels.get_mut(ch as usize)
                    {
                        c.muted = muted;
                    }
                    self.commit_edit("Mixer Mute", before);
                    self.send_node_param(node, inline_param_id(p), if muted { 1.0 } else { 0.0 });
                }
                patch::InlineParam::MixerSolo(ch) => {
                    let solo = value >= 0.5;
                    let before = self.begin_edit();
                    if let Some(NodeDoc::Mixer { channels, .. }) =
                        self.project_mut().graph.node_by_id_mut(node)
                        && let Some(c) = channels.get_mut(ch as usize)
                    {
                        c.solo = solo;
                    }
                    self.commit_edit("Mixer Solo", before);
                    self.send_node_param(node, inline_param_id(p), if solo { 1.0 } else { 0.0 });
                }
                patch::InlineParam::FeedbackGain => {
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Feedback { gain, .. }) =
                        self.project_mut().graph.node_by_id_mut(node)
                    {
                        *gain = value;
                    }
                    self.commit_param_edit("Feedback Gain", before, "feedback-gain");
                    self.send_node_param(node, inline_param_id(p), value);
                }
                patch::InlineParam::InputGain => {
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Input { gain, .. }) =
                        self.project_mut().graph.node_by_id_mut(node)
                    {
                        *gain = value.clamp(0.0, 2.0);
                    }
                    self.commit_param_edit("Input Gain", before, "input-gain");
                    self.send_node_param(node, ParamId::InputGain, value);
                }
                // One discrete edit: the engine's `InputProcessor` clamps the
                // channel to the device's count, so no graph rebuild is needed.
                patch::InlineParam::InputChannel => {
                    let channel = value.round().clamp(0.0, 1.0);
                    let before = self.begin_edit();
                    if let Some(NodeDoc::Input { channel: slot, .. }) =
                        self.project_mut().graph.node_by_id_mut(node)
                    {
                        *slot = channel as u8;
                    }
                    self.commit_edit("Input Channel", before);
                    self.send_node_param(node, ParamId::InputChannel, channel);
                }
                patch::InlineParam::SampleRoot => {
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Instrument {
                        voice: VoiceSpec::Sample { root_note, .. },
                        ..
                    }) = self.project_mut().graph.node_by_id_mut(node)
                    {
                        *root_note = value.round().clamp(0.0, 127.0) as u8;
                    }
                    self.commit_param_edit("Sample Root Note", before, "sample-root");
                    self.send_node_param(node, inline_param_id(p), value);
                }
                patch::InlineParam::SampleGain => {
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Instrument {
                        voice: VoiceSpec::Sample { gain, .. },
                        ..
                    }) = self.project_mut().graph.node_by_id_mut(node)
                    {
                        *gain = value.clamp(0.0, 2.0);
                    }
                    self.commit_param_edit("Sample Gain", before, "sample-gain");
                    self.send_node_param(node, inline_param_id(p), value);
                }
                patch::InlineParam::ModalDecay => {
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Instrument {
                        voice: VoiceSpec::Modal { params },
                        ..
                    }) = self.project_mut().graph.node_by_id_mut(node)
                    {
                        params.decay_scale = value.clamp(0.25, 4.0);
                    }
                    self.commit_param_edit("Modal Decay", before, "modal-decay");
                    self.send_node_param(node, inline_param_id(p), value);
                }
                patch::InlineParam::ModalBrightness => {
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Instrument {
                        voice: VoiceSpec::Modal { params },
                        ..
                    }) = self.project_mut().graph.node_by_id_mut(node)
                    {
                        params.brightness = value.clamp(0.0, 1.0);
                    }
                    self.commit_param_edit("Modal Brightness", before, "modal-brightness");
                    self.send_node_param(node, inline_param_id(p), value);
                }
                patch::InlineParam::PluckDecay => {
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Instrument {
                        voice: VoiceSpec::Pluck { params },
                        ..
                    }) = self.project_mut().graph.node_by_id_mut(node)
                    {
                        params.decay_s = value.clamp(0.1, 20.0);
                    }
                    self.commit_param_edit("Pluck Decay", before, "pluck-decay");
                    self.send_node_param(node, inline_param_id(p), value);
                }
                patch::InlineParam::PluckDamping => {
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Instrument {
                        voice: VoiceSpec::Pluck { params },
                        ..
                    }) = self.project_mut().graph.node_by_id_mut(node)
                    {
                        params.damping = value.clamp(0.0, 1.0);
                    }
                    self.commit_param_edit("Pluck Damping", before, "pluck-damping");
                    self.send_node_param(node, inline_param_id(p), value);
                }
                // S2: the 8' row is a drawbar level, so it rounds to an
                // integer step (the engine rounds too — this keeps the
                // document honest before the next reload).
                patch::InlineParam::OrganBar8 => {
                    let before = self.begin_param_edit();
                    let level = value.round().clamp(0.0, 8.0) as u8;
                    if let Some(NodeDoc::Instrument {
                        voice: VoiceSpec::Organ { params },
                        ..
                    }) = self.project_mut().graph.node_by_id_mut(node)
                    {
                        params.bars[2] = level;
                    }
                    self.commit_param_edit("Organ 8'", before, "organ-bar-8");
                    self.send_node_param(node, inline_param_id(p), f32::from(level));
                }
                patch::InlineParam::OrganPercussion => {
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Instrument {
                        voice: VoiceSpec::Organ { params },
                        ..
                    }) = self.project_mut().graph.node_by_id_mut(node)
                    {
                        params.percussion = value.clamp(0.0, 1.0);
                    }
                    self.commit_param_edit("Organ Percussion", before, "organ-percussion");
                    self.send_node_param(node, inline_param_id(p), value);
                }
                patch::InlineParam::Sf2Gain => {
                    let before = self.begin_param_edit();
                    if let Some(NodeDoc::Instrument {
                        voice: VoiceSpec::SoundFont { gain, .. },
                        ..
                    }) = self.project_mut().graph.node_by_id_mut(node)
                    {
                        *gain = value.clamp(0.0, 2.0);
                    }
                    self.commit_param_edit("SoundFont Gain", before, "sf2-gain");
                    self.send_node_param(node, inline_param_id(p), value);
                }
                // Bank / patch are one discrete edit each and have to keep a
                // playable address, so both route through the same preset
                // setter the Inspector uses (U13 "equal effect").
                patch::InlineParam::Sf2Bank => {
                    let Some((path, program)) =
                        self.project().graph.node_by_id(node).and_then(|n| match n {
                            NodeDoc::Instrument {
                                voice: VoiceSpec::SoundFont { path, program, .. },
                                ..
                            } => Some((path.clone(), *program)),
                            _ => None,
                        })
                    else {
                        return;
                    };
                    let bank = value.round().clamp(0.0, 255.0) as u8;
                    // Keep the program number when the new bank has it,
                    // otherwise take the bank's lowest program.
                    let next = self
                        .sf2_presets_of(&path)
                        .and_then(|presets| {
                            presets
                                .iter()
                                .find(|(b, p, _)| *b == bank && *p == program)
                                .map(|(_, p, _)| *p)
                                .or_else(|| {
                                    presets
                                        .iter()
                                        .filter(|(b, _, _)| *b == bank)
                                        .map(|(_, p, _)| *p)
                                        .min()
                                })
                        })
                        .unwrap_or(program);
                    self.set_soundfont_preset(node, bank, next);
                }
                patch::InlineParam::Sf2Program => {
                    let bank = self.sf2_voice_fields(node).map_or(0, |(bank, _, _)| bank);
                    self.set_soundfont_preset(node, bank, value.round().clamp(0.0, 255.0) as u8);
                }
            },
            patch::InlineParamId::Plugin(id) => {
                let is_clap = matches!(
                    self.project().graph.node_by_id(node),
                    Some(
                        NodeDoc::Instrument {
                            voice: VoiceSpec::Clap { .. },
                            ..
                        } | NodeDoc::Effect {
                            voice: VoiceSpec::Clap { .. },
                            ..
                        },
                    )
                );
                if is_clap {
                    self.clap_voice_values
                        .entry(node)
                        .or_default()
                        .insert(id, value as f64);
                } else {
                    self.vst3_voice_values
                        .entry(node)
                        .or_default()
                        .insert(id, value as f64);
                    // Host slider → plugin GUI (G7 three-phase edit).
                    self.sync_vst3_host_param(node, id, value as f64);
                }
                self.send_node_param(node, ParamId::PluginParam(id), value);
            }
        }
    }
}

/// Draggable handles on the play-window strip: head start, loop start, loop
/// end (tail start), window end.
const REGION_HANDLES: usize = 4;

/// Frame of one handle.
fn region_handle_frame(region: &glitcherz_model::SampleRegion, handle: usize) -> u32 {
    match handle {
        0 => region.start,
        1 => region.loop_start,
        2 => region.loop_end,
        _ => region.end,
    }
}

/// Pixel column of one handle inside the strip.
fn region_handle_x(
    rect: egui::Rect,
    handle: usize,
    region: &glitcherz_model::SampleRegion,
    last: u32,
) -> f32 {
    let last = last.max(1);
    let frame = region_handle_frame(region, handle).min(last);
    rect.left() + rect.width() * (frame as f32 / last as f32)
}

/// Frame under a pixel column (the inverse of [`region_handle_x`]).
fn region_frame_at(rect: egui::Rect, x: f32, last: u32) -> u32 {
    let t = ((x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
    (t * last as f32).round() as u32
}

/// Moves one handle and re-establishes the ordering
/// `start ≤ loop_start < loop_end ≤ end`.
///
/// The neighbours win: dragging the head past the loop start pins it at the
/// loop, so the four can never cross (a crossed window is not representable in
/// the renderer, which would then play something the handles do not describe).
pub(crate) fn move_region_handle(
    region: &glitcherz_model::SampleRegion,
    handle: usize,
    frame: u32,
    last: u32,
) -> glitcherz_model::SampleRegion {
    let mut next = *region;
    let frame = frame.min(last);
    match handle {
        0 => next.start = frame.min(next.loop_start),
        1 => {
            let upper = next.loop_end.saturating_sub(1).max(next.start);
            next.loop_start = frame.clamp(next.start, upper);
        }
        2 => {
            let lower = next.loop_start.saturating_add(1);
            next.loop_end = frame.clamp(lower, next.end.max(lower));
        }
        _ => next.end = frame.clamp(next.loop_end, last),
    }
    next
}

/// Paints the peak strip: waveform, the dimmed outside-window region, the
/// loop tint and the four handle lines with their letters.
fn draw_sample_region(
    ui: &egui::Ui,
    rect: egui::Rect,
    data: &glitcherz_engine::sample::SampleData,
    region: &glitcherz_model::SampleRegion,
    resolved: glitcherz_model::ResolvedSampleRegion,
) {
    let visuals = ui.visuals();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, visuals.extreme_bg_color);

    let frames = data.frames().len().max(1);
    let peaks = data.peaks();
    let mid = rect.center().y;
    let half = rect.height() * 0.5 - 3.0;
    let columns = rect.width().round().max(1.0) as usize;
    let wave = visuals.text_color().gamma_multiply(0.7);
    // One pixel column per frame bucket span; `peaks` is already downsampled,
    // so this is a max over a handful of (min, max) pairs.
    for col in 0..columns {
        let f0 = (col * frames / columns).min(frames - 1);
        let f1 = ((col + 1) * frames / columns).clamp(f0 + 1, frames);
        let b0 = (f0 * peaks.len() / frames).min(peaks.len() - 1);
        let b1 = (f1 * peaks.len() / frames).max(b0 + 1).min(peaks.len());
        let mut lo = 0.0f32;
        let mut hi = 0.0f32;
        for (min, max) in &peaks[b0..b1] {
            lo = lo.min(*min);
            hi = hi.max(*max);
        }
        let x = rect.left() + col as f32 + 0.5;
        painter.line_segment(
            [
                egui::pos2(x, mid - hi * half),
                egui::pos2(x, mid - lo * half),
            ],
            egui::Stroke::new(1.0, wave),
        );
    }

    let last = (frames - 1) as u32;
    let x_start = region_handle_x(rect, 0, region, last);
    let x_end = region_handle_x(rect, 3, region, last);
    // Outside the window: dimmed, so the played part is the bright part.
    let dim = egui::Color32::from_black_alpha(150);
    painter.rect_filled(
        egui::Rect::from_min_max(rect.min, egui::pos2(x_start, rect.max.y)),
        0.0,
        dim,
    );
    painter.rect_filled(
        egui::Rect::from_min_max(egui::pos2(x_end, rect.min.y), rect.max),
        0.0,
        dim,
    );
    if resolved.loops {
        let loop_l = region_handle_x(rect, 1, region, last);
        let loop_r = region_handle_x(rect, 2, region, last);
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(loop_l, rect.min.y),
                egui::pos2(loop_r, rect.max.y),
            ),
            0.0,
            egui::Color32::from_rgb(70, 120, 190).gamma_multiply(0.28),
        );
    }

    let accent = egui::Color32::from_rgb(120, 170, 235);
    let strong = visuals.strong_text_color();
    for (handle, color, label) in [
        (0usize, strong, "S"),
        (1, accent, "A"),
        (2, accent, "B"),
        (3, strong, "E"),
    ] {
        let x = region_handle_x(rect, handle, region, last);
        painter.line_segment(
            [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
            egui::Stroke::new(1.5, color),
        );
        painter.text(
            egui::pos2(x + 2.0, rect.min.y + 1.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::proportional(9.0),
            color,
        );
    }
}

#[cfg(test)]
mod region_handle_tests {
    use super::*;

    fn region(
        start: u32,
        loop_start: u32,
        loop_end: u32,
        end: u32,
    ) -> glitcherz_model::SampleRegion {
        glitcherz_model::SampleRegion {
            start,
            loop_start,
            loop_end,
            end,
            loop_mode: glitcherz_model::SampleLoopMode::Forward,
        }
    }

    /// Handles never cross: each one is clamped by its neighbours, so the
    /// window stays representable no matter what the pointer (or the keyboard)
    /// does.
    #[test]
    fn moving_a_handle_keeps_the_window_ordered() {
        let base = region(100, 200, 300, 400);

        // Head cannot pass the loop start.
        let head = move_region_handle(&base, 0, 900, 1_000);
        assert_eq!(head.start, 200);

        // Loop A stays between the head and one frame before Loop B.
        let a = move_region_handle(&base, 1, 0, 1_000);
        assert_eq!(a.loop_start, 100);
        let a2 = move_region_handle(&base, 1, 999, 1_000);
        assert_eq!(a2.loop_start, 299);

        // Loop B stays after Loop A and at most at the end.
        let b = move_region_handle(&base, 2, 0, 1_000);
        assert_eq!(b.loop_end, 201);
        let b2 = move_region_handle(&base, 2, 5_000, 1_000);
        assert_eq!(b2.loop_end, 400);

        // End never runs past the file or before the tail start.
        let end = move_region_handle(&base, 3, 5_000, 1_000);
        assert_eq!(end.end, 1_000);
        let end2 = move_region_handle(&base, 3, 0, 1_000);
        assert_eq!(end2.end, 300);
    }

    /// Pixel ↔ frame round-trips, so a drag lands on the frame under the
    /// cursor and a handle is drawn where it will be grabbed.
    #[test]
    fn pixel_and_frame_agree() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 0.0), egui::vec2(200.0, 60.0));
        let base = region(0, 250, 750, 1_000);
        for handle in 0..REGION_HANDLES {
            let x = region_handle_x(rect, handle, &base, 1_000);
            let frame = region_frame_at(rect, x, 1_000);
            let expected = region_handle_frame(&base, handle);
            assert!(
                frame.abs_diff(expected) <= 6,
                "handle {handle}: x {x} → frame {frame}, expected {expected}"
            );
        }
        // Out-of-rect pointers clamp instead of wrapping.
        assert_eq!(region_frame_at(rect, -50.0, 1_000), 0);
        assert_eq!(region_frame_at(rect, 500.0, 1_000), 1_000);
    }
}
