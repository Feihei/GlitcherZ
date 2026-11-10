//! `GlitcherZApp` methods for this panel.
//!
//! Split out of `lib.rs`; the struct fields and helper free
//! functions stay there and reach here through the glob import.

use super::*;
use crate::patch::strip_key_for;

/// I1: the note the computer keyboard starts on.
///
/// The keyboard node owns the register: its strip brackets
/// `G(octave)..G(octave+2)` and the computer keys start on that window's C,
/// so the 17 computer keys always land **inside** the strip the user sees —
/// that is also what makes the pressed key light up on the strip. Without a
/// node the global base stands alone.
pub(crate) fn computer_base_note_for(octave: Option<i8>, fallback: u8) -> u8 {
    match octave {
        // C of the window; the top key (base + 16) must stay below 127.
        Some(octave) => (i32::from(octave) * 12 + 12).clamp(0, 127 - 16) as u8,
        None => fallback,
    }
}

impl GlitcherZApp {
    pub(crate) fn send_note_on(&mut self, note: u8, velocity: f32) {
        // I1: the keyboard node's transpose applies here too — the computer
        // keyboard and a virtual keyboard box share one musical meaning.
        let sounding = shifted_note(note, self.vk_transpose()).unwrap_or(note);
        self.send_to_vk_targets(NoteEvent::On {
            note: sounding,
            velocity,
        });
        self.held_notes.insert(note);
        self.current_note = Some(sounding);
        // I1: the pressed key lights up on the node's strip — the strip on the
        // box and the keys under the fingers are one instrument.
        if let Some((node, key)) = self.computer_key_to_strip(note) {
            self.patch_state.keyboard_held_keys.insert((node, key));
        }
    }

    pub(crate) fn send_note_off(&mut self, note: u8) {
        let sounding = shifted_note(note, self.vk_transpose()).unwrap_or(note);
        self.send_to_vk_targets(NoteEvent::Off { note: sounding });
        self.held_notes.remove(&note);
        if let Some((node, key)) = self.computer_key_to_strip(note) {
            self.patch_state.keyboard_held_keys.remove(&(node, key));
        }
    }

    /// I1: the transposition of the computer-keyboard node (0 when the patch
    /// has no keyboard node).
    fn vk_transpose(&self) -> i8 {
        let doc = &self.project().graph;
        doc.computer_keyboard()
            .and_then(|node| match doc.node_by_id(node) {
                Some(NodeDoc::Keyboard { transpose, .. }) => Some(*transpose),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// I1: the keyboard node the computer keyboard plays through, if the
    /// patch has one (the node bound to [`MidiSource::Computer`]).
    fn computer_keyboard_node(&self) -> Option<u64> {
        self.project().graph.computer_keyboard()
    }

    /// I1: the register of the computer-keyboard node, if the patch has one.
    fn vk_octave(&self) -> Option<i8> {
        let doc = &self.project().graph;
        doc.computer_keyboard()
            .and_then(|node| match doc.node_by_id(node) {
                Some(NodeDoc::Keyboard { octave, .. }) => Some(*octave),
                _ => None,
            })
    }

    /// I1: the note the computer keyboard starts on.
    ///
    /// The keyboard node owns the register: its strip brackets
    /// `G(octave)..G(octave+2)` and the computer keys start on that window's
    /// C, so the 17 computer keys always land **inside** the strip the user
    /// sees — that is also what makes the pressed key light up on the strip.
    /// Without a node the global base (`Z` / `X`) stands alone.
    fn computer_base_note(&self) -> u8 {
        computer_base_note_for(self.vk_octave(), self.octave_base)
    }

    /// I1: the strip key a computer-keyboard note lights up, `(node, key)`.
    ///
    /// `None` when the patch has no keyboard node, or when the note falls
    /// outside the window the node currently brackets.
    fn computer_key_to_strip(&self, note: u8) -> Option<(u64, u8)> {
        let node = self.computer_keyboard_node()?;
        let octave = self.vk_octave()?;
        Some((node, strip_key_for(note, octave)?))
    }

    /// I1: moves the computer keyboard one octave down or up.
    ///
    /// The bound keyboard node owns the register, so `Z` / `X` move **that
    /// node's** octave (a document edit, like the box's `Oct -` / `Oct +`) and
    /// the strip follows. Without a node the global base stands alone.
    fn shift_computer_octave(&mut self, delta: i8) {
        if let Some(node) = self.computer_keyboard_node() {
            self.set_keyboard_octave(node, delta);
        } else if delta < 0 {
            self.octave_base = self.octave_base.saturating_sub(12).max(24);
        } else {
            self.octave_base = (self.octave_base + 12).min(84);
        }
    }

    /// I1: whether a modal transform (roll / timeline) is live.
    ///
    /// `X` / `Y` pick the axis of such a modal, so the register must not move
    /// under it — the keys would land on two commands at once.
    fn modal_transform_active(&self) -> bool {
        self.roll_state.modal_active() || self.timeline_state.modal_active()
    }

    /// Delivers one live event to every destination of the keyboard input
    /// node (G6 + R6 D31). Track destinations take the timeline fan-out path
    /// (so the same routing that plays the take records it); node destinations
    /// are addressed directly. **No fallback**: an unbound or unconnected input
    /// is silent, and the status bar says so on the first key press rather than
    /// quietly playing the selected track.
    pub(crate) fn send_to_vk_targets(&mut self, event: NoteEvent) {
        if self.vk_targets.is_empty() && self.vk_track_targets.is_empty() {
            if matches!(event, NoteEvent::On { .. }) {
                self.set_message(
                    "Keyboard is not wired: cable the input node to an instrument or a track recording input"
                        .to_string(),
                );
            }
            return;
        }
        // Track targets first: the note is recorded there, so the listener
        // should hear the same path. The lists are tiny and copied out to keep
        // the mutable borrow free for the send.
        let tracks = self.vk_track_targets.clone();
        for track in tracks {
            self.engine.send(EngineCommand::Note { track, event });
        }
        for i in 0..self.vk_targets.len() {
            let doc = self.vk_targets[i];
            self.send_note_event_at(doc, event);
        }
    }

    /// Delivers one live note event to one instrument node (G6: bypasses the
    /// track fan-out table, so a node that is not wired to the timeline can
    /// still be played). A doc id that has no engine counterpart (the node was
    /// just deleted) drops the event — there is nothing to fall back to.
    pub(crate) fn send_note_event_at(&mut self, doc: u64, event: NoteEvent) {
        let Some(node_id) = self.node_map.get(&doc).copied() else {
            return;
        };
        self.engine
            .send(EngineCommand::NoteAtNode { node_id, event });
    }

    /// Recompute the live-keyboard destinations from the document (G6, spec
    /// `plugin-gui.md` §8): the computer-keyboard **node's** cables decide,
    /// not the patch selection. Held notes are released only at destinations
    /// that just **dropped out**: a route that gains an instrument must not
    /// cut the notes already sounding on the others (the new synth simply
    /// starts on the next key press), while deleting the input node or its
    /// cables releases whatever was sounding.
    pub(crate) fn refresh_vk_targets(&mut self) {
        let (tracks, targets) = {
            let doc = &self.project().graph;
            match doc.computer_keyboard() {
                Some(vk) => keyboard_live_routes(doc, vk),
                None => (Vec::new(), Vec::new()),
            }
        };
        if targets == self.vk_targets && tracks == self.vk_track_targets {
            return;
        }
        let old = std::mem::take(&mut self.vk_targets);
        let old_tracks = std::mem::take(&mut self.vk_track_targets);
        let held: Vec<u8> = self.held_notes.iter().copied().collect();
        for doc in old {
            if targets.contains(&doc) {
                continue;
            }
            for note in held.iter().copied() {
                self.send_note_event_at(doc, NoteEvent::Off { note });
            }
        }
        for track in old_tracks {
            if tracks.contains(&track) {
                continue;
            }
            for note in held.iter().copied() {
                self.engine.send(EngineCommand::Note {
                    track,
                    event: NoteEvent::Off { note },
                });
            }
        }
        self.vk_targets = targets;
        self.vk_track_targets = tracks;
    }

    /// The keyboard node in the current patch selection, if any (R3).
    ///
    /// A keyboard node has no parameters, so [`resolve_panel_target`] never
    /// returns one — the input-source panel looks the selection up directly.
    pub(crate) fn selected_keyboard_node(&self) -> Option<u64> {
        let doc = &self.project().graph;
        self.patch_state
            .selected_doc_nodes()
            .into_iter()
            .find(|id| matches!(doc.node_by_id(*id), Some(NodeDoc::Keyboard { .. })))
    }

    /// Input-source picker for one keyboard node (R3, spec `recording.md`
    /// §3.2): the computer keyboard or a hardware port.
    ///
    /// Binding is a document edit, so it is one undo step. A source already
    /// bound elsewhere is offered as disabled — [`GraphDoc::validate`] rejects
    /// duplicate bindings, and refusing the click up front is cheaper than
    /// failing on save.
    pub(crate) fn keyboard_source_panel(&mut self, ui: &mut egui::Ui, node: u64) {
        ui.heading(format!("Keyboard #{node}"));
        ui.weak("Live input source and its destination instrument(s).");
        ui.add_space(6.0);

        let Some(current) = self.project().graph.node_by_id(node).and_then(|n| match n {
            NodeDoc::Keyboard { source, .. } => Some(source.clone()),
            _ => None,
        }) else {
            return;
        };

        // Where the node's commands land, so the panel can say whether playing
        // will be audible at all (R6: track takeover wins over direct cables).
        let (track_routes, node_routes) = keyboard_live_routes(&self.project().graph, node);

        let mut choice: Option<MidiSource> = None;
        let computer_selected = matches!(current, MidiSource::Computer);
        if ui.radio(computer_selected, "Computer keyboard").clicked() && !computer_selected {
            choice = Some(MidiSource::Computer);
        }

        ui.separator();
        if self.midi_ports.is_empty() {
            ui.weak("No MIDI input devices found.");
        } else {
            ui.label("MIDI devices");
            for port in &self.midi_ports {
                let selected = matches!(&current, MidiSource::Device(name) if name == port);
                // Another node already owns this port (the document allows one
                // binding per source).
                let taken = !selected
                    && self.project().graph.nodes.iter().any(|n| {
                        n.node_id() != node
                            && matches!(
                                n,
                                NodeDoc::Keyboard {
                                    source: MidiSource::Device(other),
                                    ..
                                } if other == port
                            )
                    });
                let label = if taken {
                    format!("{port}  (in use)")
                } else {
                    port.clone()
                };
                let response = ui.add_enabled(!taken, egui::RadioButton::new(selected, label));
                if response.clicked() && !selected {
                    choice = Some(MidiSource::Device(port.clone()));
                }
            }
        }

        ui.add_space(6.0);
        if ui.button("Refresh devices").clicked() {
            match self.refresh_midi_ports() {
                Ok(()) => self.set_message("MIDI devices refreshed".to_string()),
                Err(err) => self.set_message(format!("MIDI input: {err}")),
            }
        }

        // Reported state: a bound port that is not open is exactly what the
        // user needs to see when nothing responds.
        ui.add_space(6.0);
        if let MidiSource::Device(port) = &current {
            if self.midi_hub.is_open(port) {
                ui.colored_label(
                    egui::Color32::from_rgb(95, 205, 190),
                    format!("Connected: {port}"),
                );
            } else {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 160, 80),
                    format!("Not connected: {port}"),
                );
            }
        }
        if !track_routes.is_empty() {
            let list: Vec<String> = track_routes
                .iter()
                .map(|track| format!("Track {}", track + 1))
                .collect();
            ui.weak(format!(
                "Plays and records: {} (direct cables are dormant)",
                list.join(", ")
            ));
        } else if node_routes.is_empty() {
            ui.weak("No destination: cable this node's MIDI out to an instrument or a track recording input.");
        } else {
            let list: Vec<String> = node_routes.iter().map(|id| format!("#{id}")).collect();
            ui.weak(format!("Plays: {}", list.join(", ")));
        }

        if let Some(source) = choice {
            let before = self.begin_edit();
            if let Some(NodeDoc::Keyboard { source: slot, .. }) = self
                .project_mut()
                .graph
                .nodes
                .iter_mut()
                .find(|n| n.node_id() == node)
            {
                *slot = source;
            }
            self.commit_edit("Change MIDI input", before);
            // Re-open on the next frame: a new binding means a new connection.
            self.midi_bindings.clear();
        }
    }

    /// Inspector panel for a live `Audio In` node (R4): the recording source
    /// (one global stream), its status, the selected channel's meter, and the
    /// node's own channel / output trim.
    pub(crate) fn input_section(&mut self, ui: &mut egui::Ui, node: u64, channel: u8, gain: f32) {
        ui.weak(
            "Live input: one recording stream feeds every audio take. The channel and gain \
             below belong to this node.",
        );
        ui.add_space(6.0);
        self.audio_device_menu(ui);

        let info = self.engine.input_info().cloned();
        match &info {
            Some(info) => {
                ui.weak(format!(
                    "{} ch @ {} Hz · {}",
                    info.channels,
                    info.sample_rate,
                    if info.loopback { "loopback" } else { "capture" }
                ));
            }
            None => {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 120, 110),
                    "No input stream is open",
                );
                if let Some(err) = self.engine.input_error() {
                    ui.weak(err);
                }
            }
        }

        ui.add_space(6.0);
        ui.label("Input level");
        if let Some(stats) = self.engine.input_stats() {
            let peak = stats.peak_of(usize::from(channel)).min(1.0);
            ui.add(
                egui::ProgressBar::new(peak)
                    .desired_width(180.0)
                    .text(format!("Ch {} · {:.0}%", channel + 1, peak * 100.0)),
            )
            .on_hover_text("Live peak of the channel this node takes (pre-take)");
        } else {
            ui.weak("—").on_hover_text("No input stream is open");
        }
        if let Some(info) = &info
            && usize::from(channel) >= info.channels
        {
            ui.colored_label(
                egui::Color32::from_rgb(220, 160, 80),
                format!(
                    "The stream carries {} channel(s); this node selects a channel it does not \
                     have and will be silent.",
                    info.channels
                ),
            );
        }

        ui.add_space(6.0);
        ui.label("Channel");
        let mut picked: Option<u8> = None;
        ui.horizontal(|ui| {
            for ch in 0..2u8 {
                let available = info
                    .as_ref()
                    .is_none_or(|info| usize::from(ch) < info.channels);
                let selected = ch == channel;
                if ui
                    .add_enabled(
                        available || selected,
                        egui::Button::new(format!("Ch {}", ch + 1)).selected(selected),
                    )
                    .clicked()
                    && !selected
                {
                    picked = Some(ch);
                }
            }
        });
        if let Some(ch) = picked {
            let before = self.begin_edit();
            if let Some(NodeDoc::Input { channel, .. }) =
                self.project_mut().graph.node_by_id_mut(node)
            {
                *channel = ch;
            }
            self.commit_edit("Input Channel", before);
            self.send_node_param(node, ParamId::InputChannel, f32::from(ch));
        }

        ui.add_space(6.0);
        ui.label("Gain");
        let mut value = gain;
        let response = ui.add(
            egui::Slider::new(&mut value, 0.0..=2.0)
                .fixed_decimals(2)
                .text("output"),
        );
        if response.changed() {
            let before = self.begin_param_edit();
            if let Some(NodeDoc::Input { gain, .. }) = self.project_mut().graph.node_by_id_mut(node)
            {
                *gain = value;
            }
            self.commit_param_edit("Input Gain", before, "input-gain");
            self.send_node_param(node, ParamId::InputGain, value);
        }
        if response.drag_stopped() {
            self.end_param_edit();
        }

        ui.add_space(6.0);
        ui.weak(
            "The recording source is shared: changing it here reopens the stream for every \
             audio take, not just this node.",
        );
    }

    /// Recomputes the hardware MIDI routing snapshots and hands them to the hub
    /// (R3/R6, spec `recording.md` §3.2 §5).
    ///
    /// The routing rule is the computer keyboard's, one level down: a device
    /// keyboard node plays the **tracks its recording input cables arm** (D33
    /// takeover), or — with no recording cable — the instruments its direct
    /// cables reach. Recording is not a modifier: the track route is permanent
    /// while the cable exists, so idle playing, recording and playback all use
    /// the same path.
    pub(crate) fn sync_midi_bindings(&mut self) {
        let bindings = {
            let doc = &self.project().graph;
            let mut out: Vec<(String, Vec<MidiTarget>)> = Vec::new();
            for node in &doc.nodes {
                let NodeDoc::Keyboard {
                    node_id,
                    source: MidiSource::Device(port),
                    ..
                } = node
                else {
                    continue;
                };
                let (tracks, nodes) = keyboard_live_routes(doc, *node_id);
                let mut targets: Vec<MidiTarget> = nodes
                    .into_iter()
                    .filter_map(|doc_id| self.node_map.get(&doc_id).copied())
                    .map(MidiTarget::Node)
                    .collect();
                targets.extend(tracks.into_iter().map(MidiTarget::Track));
                out.push((port.clone(), targets));
            }
            out
        };
        if bindings == self.midi_bindings {
            return;
        }
        self.midi_bindings = bindings.clone();
        let sender = self.engine.command_sender();
        match self.midi_hub.set_bindings(&bindings, &sender) {
            Ok(missing) => {
                // Report a change of state only: an unplugged controller is
                // not worth a status line every frame.
                if missing != self.midi_missing {
                    if !missing.is_empty() {
                        self.set_message(format!("MIDI input not found: {}", missing.join(", ")));
                    }
                    self.midi_missing = missing;
                }
            }
            Err(err) => {
                self.set_message(format!("MIDI input: {err}"));
                self.midi_missing.clear();
            }
        }
    }

    /// Handle computer keyboard playing (FL Studio-style layout; Z/X shift octaves).
    ///
    /// While the piano roll owns keyboard focus, the computer keyboard
    /// drives the roll's command family (G/T/S/D/K...) instead — note-ons
    /// are suppressed; note-offs still pass so held notes never stick.
    /// Text input widgets (none yet besides DragValue edits) still need
    /// the same treatment.
    pub(crate) fn handle_keyboard_input(&mut self, ctx: &egui::Context) {
        let roll_owns_focus = self.roll_state.owns_keyboard_focus(ctx)
            || self.timeline_state.owns_keyboard_focus(ctx);
        // Transport keys stay global under any focus owner (roll, timeline,
        // patch, buttons); only a live text edit keeps them for itself
        // (Space types a space, Home/End move the caret). `egui_wants_keyboard_input`
        // is too broad here: it is true whenever ANY widget is focused, which
        // includes our hover-follow roll / timeline surfaces.
        let text_edit_owns_keys = ctx.text_edit_focused();
        // The piano roll's clip chip strip owns `Ctrl+Tab` while the roll
        // itself holds the keyboard (spec `roll-multiclip.md` D7); the view
        // cycle leaves that combo to the roll there, and keeps bare `Tab`.
        let roll_has_keys = self.roll_state.owns_keyboard_focus(ctx);
        let events: Vec<egui::Event> = ctx.input(|i| i.events.clone());
        for event in events {
            let egui::Event::Key {
                key,
                pressed,
                repeat,
                modifiers,
                ..
            } = event
            else {
                continue;
            };
            // Global transport (Space / Home / End / marker and bar jumps). The
            // events are also consumed from egui's queue so a focused
            // click-sensing surface does not treat Space as a fake primary
            // click.
            if !text_edit_owns_keys {
                match key {
                    egui::Key::Space if pressed && !modifiers.ctrl => {
                        ctx.input_mut(|i| i.consume_key(modifiers, egui::Key::Space));
                        if !repeat {
                            self.toggle_transport();
                        }
                        continue;
                    }
                    egui::Key::Home if pressed && !modifiers.ctrl => {
                        ctx.input_mut(|i| i.consume_key(modifiers, egui::Key::Home));
                        if !repeat {
                            self.engine.seek_ticks(0.0);
                        }
                        continue;
                    }
                    egui::Key::End if pressed && !modifiers.ctrl => {
                        ctx.input_mut(|i| i.consume_key(modifiers, egui::Key::End));
                        if !repeat {
                            let end = f64::from(self.arrangement_end_ticks());
                            self.engine.seek_ticks(end);
                        }
                        continue;
                    }
                    egui::Key::Comma if pressed && !modifiers.ctrl => {
                        ctx.input_mut(|i| i.consume_key(modifiers, egui::Key::Comma));
                        if !repeat {
                            self.seek_to_previous_marker();
                        }
                        continue;
                    }
                    egui::Key::Period if pressed && !modifiers.ctrl => {
                        ctx.input_mut(|i| i.consume_key(modifiers, egui::Key::Period));
                        if !repeat {
                            self.seek_to_next_marker();
                        }
                        continue;
                    }
                    egui::Key::ArrowLeft if pressed && modifiers.ctrl => {
                        ctx.input_mut(|i| i.consume_key(modifiers, egui::Key::ArrowLeft));
                        if !repeat {
                            self.seek_by_bars(-1);
                        }
                        continue;
                    }
                    egui::Key::ArrowRight if pressed && modifiers.ctrl => {
                        ctx.input_mut(|i| i.consume_key(modifiers, egui::Key::ArrowRight));
                        if !repeat {
                            self.seek_by_bars(1);
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            // View switching. Plain `Tab` / `Shift+Tab` cycle the four tabs and
            // `Alt+1..4` jumps straight to one. The piano roll's clip chip
            // strip used to own plain `Tab` for clip focus (spec
            // `roll-multiclip.md` D7); it now lives on `Ctrl+Tab`, so the view
            // cycle keeps Tab everywhere. While the roll holds the keyboard,
            // `Ctrl+Tab` is left to it; elsewhere it cycles views as well.
            let view_target = if !pressed || text_edit_owns_keys {
                None
            } else if modifiers.alt {
                match key {
                    egui::Key::Num1 => MainView::from_index(0),
                    egui::Key::Num2 => MainView::from_index(1),
                    egui::Key::Num3 => MainView::from_index(2),
                    egui::Key::Num4 => MainView::from_index(3),
                    _ => None,
                }
            } else if key == egui::Key::Tab {
                if modifiers.ctrl && roll_has_keys {
                    None
                } else {
                    Some(self.view.cycle(!modifiers.shift))
                }
            } else {
                None
            };
            if let Some(target) = view_target {
                if !repeat {
                    ctx.input_mut(|i| i.consume_key(modifiers, key));
                    // An open menu (File / Edit) would otherwise float over
                    // the view we are switching to.
                    egui::Popup::close_all(ctx);
                    self.set_view(target);
                }
                continue;
            }
            if repeat {
                continue;
            }
            // Shortcuts take precedence over the playing map (Ctrl+S save / Ctrl+O open /
            // Ctrl+Z undo).
            if modifiers.ctrl && pressed {
                match key {
                    egui::Key::S if modifiers.shift => {
                        self.save_project_as();
                        continue;
                    }
                    egui::Key::S => {
                        self.save_project();
                        continue;
                    }
                    egui::Key::N => {
                        self.request_file_action(FileAction::New);
                        continue;
                    }
                    egui::Key::O => {
                        self.open_project_dialog();
                        continue;
                    }
                    egui::Key::Z => {
                        self.undo();
                        continue;
                    }
                    egui::Key::Y => {
                        self.redo();
                        continue;
                    }
                    // G6 quick audition: link / unlink the selected synth(s)
                    // to the computer-keyboard input node.
                    egui::Key::L => {
                        self.toggle_audition_route();
                        continue;
                    }
                    _ => {
                        continue; // other Ctrl combos do not trigger notes
                    }
                }
            }

            match (key, pressed) {
                (egui::Key::Z, true) => {
                    if !self.modal_transform_active() {
                        self.shift_computer_octave(-1);
                    }
                }
                (egui::Key::X, true) => {
                    if !self.modal_transform_active() {
                        self.shift_computer_octave(1);
                    }
                }
                (egui::Key::R, true) => {
                    // Record (spec `recording.md` R2). While the piano roll owns
                    // the keyboard (its modal command keys) `R` belongs to it,
                    // and a live text edit keeps its own letters.
                    if !roll_owns_focus && !text_edit_owns_keys {
                        self.toggle_recording();
                    }
                }
                _ => {
                    if let Some((_, semi)) = KEYMAP.iter().find(|(k, _)| *k == key) {
                        let note = self.computer_base_note() + semi;
                        if pressed {
                            if roll_owns_focus {
                                // The roll owns this key (modal command /
                                // mode toggle); do not also play a note.
                                continue;
                            }
                            if !self.held_notes.contains(&note) {
                                self.send_note_on(note, 0.9);
                            }
                        } else if self.held_notes.contains(&note) {
                            self.send_note_off(note);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// I1: the keyboard node owns the register — the computer keyboard starts
    /// on the window's C, so `Oct -` / `Oct +` on the box moves the keys under
    /// the fingers as well.
    #[test]
    fn the_computer_keyboard_plays_the_node_register() {
        // Octave 4 → C4 (the default; the window is G3..G5).
        assert_eq!(computer_base_note_for(Some(4), 60), 60);
        assert_eq!(computer_base_note_for(Some(5), 60), 72);
        assert_eq!(computer_base_note_for(Some(3), 60), 48);
        // Without a keyboard node the global base stands alone.
        assert_eq!(computer_base_note_for(None, 60), 60);
        assert_eq!(computer_base_note_for(None, 24), 24);
    }

    /// I1: every computer key lands inside the strip the node brackets — that
    /// is what makes the pressed key light up on the box.
    #[test]
    fn every_computer_key_lands_inside_the_strip() {
        for octave in 0..=MAX_KEYBOARD_OCTAVE {
            let base = computer_base_note_for(Some(octave), 60);
            for semi in 0..=16u8 {
                let note = base + semi;
                assert!(
                    note <= 127,
                    "octave {octave}: the top key must stay inside MIDI"
                );
                assert_eq!(
                    strip_key_for(note, octave),
                    Some(note),
                    "octave {octave}: {note} must light a key on the strip"
                );
            }
        }
    }

    /// I1: a note outside the window lights nothing (the register may have
    /// moved while the key was held).
    #[test]
    fn a_note_outside_the_window_lights_nothing() {
        assert_eq!(strip_key_for(60, 4), Some(60));
        assert_eq!(strip_key_for(54, 4), None);
        assert_eq!(strip_key_for(80, 4), None);
        assert_eq!(strip_key_for(55, 4), Some(55));
        assert_eq!(strip_key_for(79, 4), Some(79));
    }
}
