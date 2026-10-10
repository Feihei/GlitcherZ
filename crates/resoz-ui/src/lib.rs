#![forbid(unsafe_code)]

//! # resoz-ui
//!
//! ResoZ's egui UI: transport controls, arrangement timeline, piano roll,
//! mixer, patch board, and automation lanes.
//!
//! P5c (spec `docs/spec/node-graph.md` D13): the engine graph is
//! **rebuilt purely from the project document** — `ResoZApp` owns the
//! `Project` document; on load/new, `apply_document` builds the graph
//! step by step at `add_node` / `connect_nodes` granularity. Document
//! node_ids are persisted identities (D11); engine-side identities are
//! monotonically assigned `engine_id`s within a session (see the
//! `node_map` mapping; recycled ids are never reused in a session).
//!
//! Layering: this crate talks directly to the engine's non-real-time
//! control side — `AudioEngine` is only touched on the UI thread and
//! never enters the audio callback. Model and engine are independent;
//! document → engine representation conversion happens here before
//! sending.

use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

use resoz_clap_host::{ClapInstance, ClapInstanceConfig};
use resoz_engine::AudioEngine;
use resoz_engine::convert;
use resoz_engine::realtime::{
    ChannelStripProcessor, DelayProcessor, EngineCommand, MAX_AUTO_POINTS, MAX_CLIP_NOTES,
    MAX_TRACK_CLIPS, MixerChannel as EngineMixerChannel, MixerProcessor, NodeKind, NoteEvent,
    ParamId, Processor, RtAutoPoint, RtAutomation, RtControlClip, RtMidiClip, RtNote,
    SequencerCommand, SynthProcessor,
};
use resoz_model::{
    AutoParam, CableKind, ControlClip, ControlTrack, DelayParams, MAX_AUTO_TRACKS,
    MAX_CLIPS_PER_TRACK, MAX_MIDI_TRACKS, MAX_MIXER_CHANNELS, MAX_NODES, MIN_MIXER_CHANNELS,
    MidiClip, NodeDoc, PortRef, Project, SynthParams, VoiceSpec,
};

pub mod lane;
pub mod patch;
pub mod piano_roll;
pub mod timeline;

/// Filter cutoff frequency slider range (logarithmic scale).
pub const CUTOFF_RANGE: RangeInclusive<f32> = 20.0..=18_000.0;

/// Resonance slider range (Simper SVF res).
pub const RES_RANGE: RangeInclusive<f32> = 0.0..=0.98;

/// Attack / Decay slider range (seconds).
pub const AD_RANGE: RangeInclusive<f32> = 0.0..=2.0;

/// Sustain slider range (relative level).
pub const S_RANGE: RangeInclusive<f32> = 0.0..=1.0;

/// Release slider range (seconds).
pub const R_RANGE: RangeInclusive<f32> = 0.0..=3.0;

/// Delay time slider range (seconds).
pub const DELAY_TIME_RANGE: RangeInclusive<f32> = 0.005..=1.0;

/// Delay feedback slider range.
pub const DELAY_FB_RANGE: RangeInclusive<f32> = 0.0..=0.9;

/// Delay wet mix slider range.
pub const DELAY_MIX_RANGE: RangeInclusive<f32> = 0.0..=1.0;

/// Track fader range (linear).
pub const TRACK_GAIN_RANGE: RangeInclusive<f32> = 0.0..=1.5;

/// Track pan slider range.
pub const PAN_RANGE: RangeInclusive<f32> = -1.0..=1.0;

/// BPM adjustable range (f64 in the document: time computation convention).
pub const BPM_RANGE: RangeInclusive<f64> = 20.0..=300.0;

/// Computer keyboard → semitone offset (relative to the octave base, FL Studio-style layout).
const KEYMAP: &[(egui::Key, u8)] = &[
    (egui::Key::A, 0),
    (egui::Key::W, 1),
    (egui::Key::S, 2),
    (egui::Key::E, 3),
    (egui::Key::D, 4),
    (egui::Key::F, 5),
    (egui::Key::T, 6),
    (egui::Key::G, 7),
    (egui::Key::Y, 8),
    (egui::Key::H, 9),
    (egui::Key::U, 10),
    (egui::Key::J, 11),
    (egui::Key::K, 12),
    (egui::Key::O, 13),
    (egui::Key::L, 14),
    (egui::Key::P, 15),
    (egui::Key::Semicolon, 16),
];

/// Semitone positions of black keys among the 13 keys of an octave (C to the next C).
const BLACK_SEMIS: [u8; 5] = [1, 3, 6, 8, 10];

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// MIDI note number → note name (e.g. 69 → "A4").
fn note_name(note: u8) -> String {
    let octave = note as i32 / 12 - 1;
    format!("{}{octave}", NOTE_NAMES[(note % 12) as usize])
}

/// Path → display name (file name; full path on failure).
fn display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Document automation parameter → engine parameter ID.
fn param_id_of(param: &AutoParam) -> ParamId {
    match param {
        AutoParam::TrackGain => ParamId::TrackGain,
        AutoParam::TrackPan => ParamId::TrackPan,
        AutoParam::FilterCutoff => ParamId::FilterCutoff,
        AutoParam::PluginParam { param_id, .. } => ParamId::PluginParam(*param_id),
    }
}

/// UI startup parameters.
#[derive(Debug, Clone, Default)]
pub struct UiConfig {
    /// Initial master gain, clamped into 0..1.5.
    pub gain: f64,
    /// Note to trigger automatically at startup (for demos / smoke tests; `None` = silent start).
    pub auto_play_note: Option<u8>,
    /// Auto-close the window after startup (for smoke tests; `None` = manual close).
    pub auto_close_after: Option<Duration>,
    /// Project file to load at startup (`--open`).
    pub open_path: Option<PathBuf>,
}

/// UI state saved via eframe persisted storage (`persistence` feature).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct UiState {
    /// Recently opened project files (newest first, at most [`RECENT_FILES_MAX`] entries).
    recent_files: Vec<PathBuf>,
}

/// Capacity limit of the recent files list.
const RECENT_FILES_MAX: usize = 8;

/// Key for [`UiState`] in eframe storage.
const UI_STATE_KEY: &str = "resoz_ui_state";

/// Status bar message display duration.
const STATUS_MESSAGE_TTL: Duration = Duration::from_secs(8);

/// External plugin instance guard (CLAP / VST3).
#[allow(dead_code)] // guard held only to extend lifetime; the field itself is never read
enum PluginGuard {
    Clap(resoz_clap_host::ClapInstanceGuard),
    Vst3(resoz_vst3_host::Vst3InstanceGuard),
}

/// A loaded external plugin instrument/effect guard (node_id is the **engine-side** identity).
///
/// Guards **must not be dropped early** — unloading the dynamic library would leave the real-time
/// side node dangling. Guards swapped out or removed with the document move into
/// `retiring_guards`, released in order via recycling receipts.
#[allow(dead_code)]
struct PluginGuardEntry {
    /// Paired with the engine-side node ID (recycling receipts match on this).
    node_id: u64,
    #[allow(dead_code)]
    track: usize,
    #[allow(dead_code)]
    plugin_id: String,
    _guard: PluginGuard,
}

/// Peak level meter: the peak holds for 600 ms before falling back, which is easier to read visually.
#[derive(Debug)]
struct PeakMeter {
    held: f32,
    hold_until: Instant,
}

impl PeakMeter {
    fn new() -> Self {
        Self {
            held: 0.0,
            hold_until: Instant::now(),
        }
    }

    /// Refresh the held value with the engine's latest peak (called each frame). UI-thread only.
    fn update(&mut self, current: f32) {
        let now = Instant::now();
        if current >= self.held || now >= self.hold_until {
            self.held = current;
            self.hold_until = now + Duration::from_millis(600);
        }
    }
}

/// Which clip the floating editor window (piano roll / curve lane) targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditTarget {
    Midi { track: usize, clip: usize },
    Control { track: usize, clip: usize },
}

/// Main view switch (tab bar under the transport panel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum MainView {
    /// Arrangement timeline (default view: the DAW's home screen).
    #[default]
    Timeline,
    /// Node patch graph (full-height, dedicated view).
    Patch,
    /// Synth params + mixer + clip/automation editors + keyboard.
    Synth,
}

/// Main window app. Owns the [`AudioEngine`]: closing the window drops the engine and stops the
/// audio stream.
pub struct ResoZApp {
    engine: AudioEngine,
    /// Project document + undo history (the UI owns model state; see
    /// "Threading Model" in AGENTS.md). Mutations go through
    /// [`Session::execute`] so they are undoable; `project_mut` is the escape
    /// hatch for the many places not yet moved to commands.
    session: resoz_command::Session,
    /// Document node_id → engine node_id (monotonic within a session; reloading the document
    /// replaces them all).
    node_map: HashMap<u64, u64>,
    /// Track being edited (target of the synth/sequencer/keyboard).
    selected_track: usize,
    /// Automation track being edited.
    selected_auto: usize,
    /// Arrangement timeline cross-frame state (zoom/drag).
    timeline_state: timeline::TimelineState,
    /// Which clip the floating editor window is targeting (`None` = closed).
    edit_clip: Option<EditTarget>,
    /// Piano roll cross-frame state (scroll/drag).
    roll_state: piano_roll::RollState,
    /// Automation lane cross-frame state (drag).
    lane_state: lane::LaneState,
    /// Node patch cross-frame state (pan/zoom/selection/drag).
    patch_state: patch::PatchState,
    /// Active main view (tab bar).
    view: MainView,
    /// Octave base (MIDI number of C, default 60 = C4).
    octave_base: u8,
    /// Currently held notes (shared by on-screen keys and computer keys, for highlighting and
    /// de-duplication).
    held_notes: HashSet<u8>,
    /// On-screen key notes held by the mouse this frame (for press/release edge detection).
    mouse_down_notes: HashSet<u8>,
    /// Most recently triggered note (shown in the status bar).
    current_note: Option<u8>,
    meter: PeakMeter,
    opened_at: Instant,
    auto_close_after: Option<Duration>,
    /// Current project file path (`None` = never saved; a dialog pops up on save).
    project_path: Option<PathBuf>,
    /// Recently opened project files (persisted).
    recent: Vec<PathBuf>,
    /// Status bar message (text + time shown; disappears automatically after a timeout).
    status_message: Option<(String, Instant)>,
    /// Desired window title.
    window_title: String,
    /// Title already sent to the windowing system (avoids resending the command every frame).
    title_sent: String,
    /// Transport state of the previous frame (to detect the stop edge → restore manual
    /// parameters).
    was_playing: bool,
    /// Active external plugin guards (CLAP/VST3): dropping a guard unloads the plugin's dynamic
    /// library and leaves the real-time node dangling, so guards are held until the node is
    /// swapped out.
    voice_guards: Vec<PluginGuardEntry>,
    /// Swapped-out / removed-with-old-document guards: dropped only after the real-time recycling
    /// receipt arrives (the dynamic library must stay alive until the old node truly leaves the
    /// processing graph).
    retiring_guards: Vec<PluginGuardEntry>,
    /// Scanned CLAP instrument list (lazy scan cache).
    clap_plugins: Option<Vec<resoz_clap_host::ClapPluginInfo>>,
    /// Scanned CLAP effect list (lazy scan cache).
    clap_effects: Option<Vec<resoz_clap_host::ClapPluginInfo>>,
    /// Scanned VST3 instrument list (lazy scan cache).
    vst3_plugins: Option<Vec<resoz_vst3_host::Vst3PluginInfo>>,
    /// Scanned VST3 effect list (lazy scan cache).
    vst3_effects: Option<Vec<resoz_vst3_host::Vst3PluginInfo>>,
    /// Per-track VST3 instrument parameter lists (read at instantiation, keyed by track number).
    /// For diagnostics/display only; actual parameter control still goes through the
    /// `PluginParam` bridge.
    voice_params: HashMap<usize, Vec<resoz_vst3_host::Vst3ParamInfo>>,
    /// Per-track CLAP instrument parameter lists (read at instantiation, keyed by track number).
    /// Data source for automation cable targets; curve value semantics = raw plugin-range values.
    clap_voice_params: HashMap<usize, Vec<resoz_clap_host::ClapParamInfo>>,
    /// Diagnostic flag: whether `ui()` has been entered (logging only).
    first_frame_logged: bool,
    /// Diagnostic flag: whether the close command has been sent (logging only).
    close_sent: bool,
}

impl ResoZApp {
    /// Build the window app, restore persisted recent files, and push the whole project document
    /// down via incremental graph building + the command queue (takes effect at the next audio
    /// block boundary).
    pub fn new(cc: &eframe::CreationContext<'_>, engine: AudioEngine, config: UiConfig) -> Self {
        let ui_state: UiState = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, UI_STATE_KEY))
            .unwrap_or_default();
        let mut app = Self {
            engine,
            session: resoz_command::Session::new(Project {
                master_gain: config.gain.clamp(0.0, 1.5) as f32,
                ..Project::default()
            }),
            node_map: HashMap::new(),
            selected_track: 0,
            selected_auto: 0,
            timeline_state: timeline::TimelineState::default(),
            edit_clip: None,
            roll_state: piano_roll::RollState::new(),
            lane_state: lane::LaneState::default(),
            patch_state: patch::PatchState::default(),
            view: MainView::default(),
            octave_base: 60,
            held_notes: HashSet::new(),
            mouse_down_notes: HashSet::new(),
            current_note: None,
            meter: PeakMeter::new(),
            opened_at: Instant::now(),
            auto_close_after: config.auto_close_after,
            project_path: None,
            recent: ui_state.recent_files,
            status_message: None,
            window_title: "ResoZ".to_string(),
            title_sent: String::new(),
            was_playing: false,
            voice_guards: Vec::new(),
            retiring_guards: Vec::new(),
            clap_plugins: None,
            clap_effects: None,
            vst3_plugins: None,
            vst3_effects: None,
            voice_params: HashMap::new(),
            clap_voice_params: HashMap::new(),
            first_frame_logged: false,
            close_sent: false,
        };
        app.push_all_params();
        if let Some(note) = config.auto_play_note {
            app.send_note_on(note, 1.0);
        }
        if let Some(path) = config.open_path {
            app.do_open(path);
        }
        app
    }

    // ---- Document → engine (D13 fully explicit: the loader rebuilds purely from the document) ----

    /// Rebuild the current project document into the engine: remove old nodes → install document
    /// nodes → assemble track addressing tables → connect cables per the document → push the
    /// active track count.
    fn apply_document(&mut self) {
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

        // 2) Install document nodes (external plugin failure → fall back to built-in, keeping the
        //    chain intact).
        let sample_rate = self.engine.info().sample_rate;
        let nodes = self.project().graph.nodes.clone();
        for node in &nodes {
            self.install_node(node, sample_rate);
        }
        //(install_node instantiates plugins at the u32 sample rate; built-in processors convert
        // to f32 via convert.)

        // 3) Assemble track addressing tables (track-coordinate addressing for manual
        //    params/levels/keyboard).
        for track in 0..self.project().midi_tracks.len() {
            self.register_track_chain(track);
        }

        // 4) Connect cables per the document.
        let master = self.engine.master_node_id();
        let cables = self.project().graph.cables.clone();
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
                    let PortRef::MidiTrack(track) = &cable.from else {
                        continue;
                    };
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
                CableKind::Automation => {
                    let PortRef::AutoTrack(index) = &cable.from else {
                        continue;
                    };
                    let Some(to) = to_node else {
                        log::warn!("Automation cable target node missing, skipping: {cable:?}");
                        continue;
                    };
                    if !self
                        .engine
                        .set_auto_route(*index, to, param_id_of(&cable.param))
                    {
                        log::warn!("Automation cable (track {index} → {to}) dispatch failed");
                    }
                }
            }
        }

        // 5) Active MIDI track count (D12).
        self.engine
            .set_midi_track_count(self.project().midi_tracks.len());
    }

    /// Install a document node: build the processor (external plugins instantiated
    /// non-real-time) → engine `add_node` (new engine-side id) → register the mapping and guard.
    fn install_node(&mut self, node: &NodeDoc, sample_rate: u32) {
        let sr = convert::sample_rate_to_f32(sample_rate);
        let doc_id = node.node_id();
        let track = node.track();
        let (processor, guard, plugin_label): (Box<dyn Processor>, Option<PluginGuard>, String) =
            match node {
                NodeDoc::Instrument { voice, .. } => match voice {
                    VoiceSpec::Builtin => (Box::new(SynthProcessor::new(sr)), None, String::new()),
                    VoiceSpec::Clap { path, plugin_id } => {
                        match ClapInstance::spawn_rt(
                            std::path::Path::new(path),
                            plugin_id,
                            ClapInstanceConfig {
                                sample_rate,
                                max_frames: 128,
                            },
                        ) {
                            Ok(instance) => {
                                // Parameter enumeration must happen before the instance is
                                // destructured (the guard holds the plugin instance).
                                let params = instance.list_params();
                                let ClapInstance { guard, node } = instance;
                                self.clap_voice_params.insert(track, params);
                                (
                                    Box::new(node),
                                    Some(PluginGuard::Clap(guard)),
                                    plugin_id.clone(),
                                )
                            }
                            Err(err) => {
                                log::error!("Failed to instantiate CLAP plugin {plugin_id}: {err}");
                                self.clap_voice_params.remove(&track);
                                (Box::new(SynthProcessor::new(sr)), None, String::new())
                            }
                        }
                    }
                    VoiceSpec::Vst3 { path, class_id } => {
                        match resoz_vst3_host::Vst3Instance::spawn_rt(
                            std::path::Path::new(path),
                            class_id,
                            resoz_vst3_host::Vst3InstanceConfig {
                                sample_rate,
                                max_frames: 128,
                            },
                        ) {
                            Ok(instance) => {
                                let resoz_vst3_host::Vst3Instance { guard, node } = instance;
                                let params = guard.parameters();
                                self.voice_params.insert(track, params);
                                (
                                    Box::new(node),
                                    Some(PluginGuard::Vst3(guard)),
                                    class_id.clone(),
                                )
                            }
                            Err(err) => {
                                log::error!("Failed to instantiate VST3 plugin {class_id}: {err}");
                                self.voice_params.remove(&track);
                                (Box::new(SynthProcessor::new(sr)), None, String::new())
                            }
                        }
                    }
                },
                NodeDoc::Effect { voice, .. } => match voice {
                    VoiceSpec::Builtin => (Box::new(DelayProcessor::new(sr)), None, String::new()),
                    VoiceSpec::Clap { path, plugin_id } => {
                        match ClapInstance::spawn_rt(
                            std::path::Path::new(path),
                            plugin_id,
                            ClapInstanceConfig {
                                sample_rate,
                                max_frames: 128,
                            },
                        ) {
                            Ok(instance) => {
                                let ClapInstance { guard, node } = instance;
                                (
                                    Box::new(node),
                                    Some(PluginGuard::Clap(guard)),
                                    plugin_id.clone(),
                                )
                            }
                            Err(err) => {
                                log::error!("Failed to instantiate CLAP effect {plugin_id}: {err}");
                                (Box::new(DelayProcessor::new(sr)), None, String::new())
                            }
                        }
                    }
                    VoiceSpec::Vst3 { path, class_id } => {
                        match resoz_vst3_host::Vst3Instance::spawn_rt(
                            std::path::Path::new(path),
                            class_id,
                            resoz_vst3_host::Vst3InstanceConfig {
                                sample_rate,
                                max_frames: 128,
                            },
                        ) {
                            Ok(instance) => {
                                let resoz_vst3_host::Vst3Instance { guard, node } = instance;
                                (
                                    Box::new(node),
                                    Some(PluginGuard::Vst3(guard)),
                                    class_id.clone(),
                                )
                            }
                            Err(err) => {
                                log::error!("Failed to instantiate VST3 effect {class_id}: {err}");
                                (Box::new(DelayProcessor::new(sr)), None, String::new())
                            }
                        }
                    }
                },
                NodeDoc::Mixer {
                    channels,
                    out_pan,
                    out_muted,
                    ..
                } => {
                    // P5f D15/D21: engine mixer from the document's console state.
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
                        String::new(),
                    )
                }
                NodeDoc::ChannelStrip { .. } => {
                    (Box::new(ChannelStripProcessor::new()), None, String::new())
                }
            };
        let engine_id = self.engine.fresh_node_id();
        // D15: the mixer is the only multi-input node; the engine compiler
        // uses the kind for fan-in validation + cable→channel binding.
        let kind = if node.is_mixer() {
            NodeKind::Mixer
        } else {
            NodeKind::Plain
        };
        if self.engine.add_node(engine_id, kind, processor) {
            self.node_map.insert(doc_id, engine_id);
            if let Some(guard) = guard {
                self.voice_guards.push(PluginGuardEntry {
                    node_id: engine_id,
                    track,
                    plugin_id: plugin_label,
                    _guard: guard,
                });
            }
        } else {
            drop(guard); // Node never entered the graph: drop the guard immediately (safe unload
            // of the dynamic library).
            log::error!("Document node {doc_id} installation failed (slots full / queue full)");
        }
    }

    /// Re-assemble one track's engine addressing table (manual params/level/keyboard
    /// addressing) from the document roles + `node_map`. Called after document rebuild and
    /// whenever a node filling a track role is added or removed.
    fn register_track_chain(&mut self, track: usize) {
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

    /// Move a node's active guard into the retiring zone (called when a node is swapped out or
    /// removed; retiring guards are released once recycling receipts arrive).
    fn retire_guard(&mut self, engine_id: u64) {
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
    fn engine_id(&self, doc_id: u64) -> Option<u64> {
        self.node_map.get(&doc_id).copied()
    }

    /// Push the full set of project document parameters (commands dropped on queue full are safe
    /// to ignore).
    fn push_all_params(&mut self) {
        self.apply_document();
        self.engine.send(EngineCommand::SetParam {
            id: ParamId::MasterGain,
            value: self.project().master_gain,
        });
        self.send_sequencer(SequencerCommand::SetBpm(convert::to_f32(
            self.project().metadata.tempo_bpm,
        )));
        for track in 0..self.project().midi_tracks.len() {
            self.push_track_params(track);
            self.send_track_clips(track);
        }
        self.push_mixer_params();
        for index in 0..self.project().control_tracks.len() {
            self.send_control_clips(index);
        }
    }

    /// Push all parameters of one track (timbre / delay; track addressing tables already
    /// assembled). Fader state no longer rides the track path — it lives on
    /// mixer channels (pushed by [`Self::push_mixer_params`]).
    fn push_track_params(&mut self, track: usize) {
        let synth = self.synth_of_track(track).unwrap_or_default();
        let delay = self.delay_of_track(track).unwrap_or_default();
        let params = [
            (ParamId::FilterCutoff, synth.cutoff_hz),
            (ParamId::FilterResonance, synth.resonance),
            (ParamId::FilterMode, synth.filter_mode),
            (ParamId::Attack, synth.attack_s),
            (ParamId::Decay, synth.decay_s),
            (ParamId::Sustain, synth.sustain),
            (ParamId::Release, synth.release_s),
            (ParamId::DelayTime, delay.time_s),
            (ParamId::DelayFeedback, delay.feedback),
            (ParamId::DelayMix, delay.mix),
        ];
        for (id, value) in params {
            self.engine
                .send(EngineCommand::SetTrackParam { track, id, value });
        }
    }

    /// Push every mixer node's console state to the engine (P5f D15/D20:
    /// fader moves ride the node param path, `ParamId::MixerGain(ch)` etc.).
    fn push_mixer_params(&mut self) {
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

    // ---- Convenient accessors for document fields (data source for track panels in node-graph
    // ---- form) ----

    /// Read-only document access.
    pub fn project(&self) -> &Project {
        self.session.project()
    }

    /// Mutable document access.
    ///
    /// Bare access records no undo step. Wrap the edit in
    /// [`ResoZApp::begin_edit`] / [`ResoZApp::commit_edit`] (or run a command
    /// via [`ResoZApp::execute`]); the AGENTS.md rule "UI 层不得直接修改
    /// model" means every call site must be inside such a guard.
    pub fn project_mut(&mut self) -> &mut Project {
        self.session.project_mut()
    }

    /// Captures a snapshot ahead of an out-of-band edit.
    ///
    /// ```text
    /// let before = self.begin_edit();
    /// ... mutate through self.project_mut() ...
    /// self.commit_edit("Add Node", before);
    /// ```
    pub fn begin_edit(&self) -> std::sync::Arc<Project> {
        self.session.project_arc()
    }

    /// Records the edit started by [`ResoZApp::begin_edit`] as one undo step.
    ///
    /// A no-op edit (the document did not actually change) records nothing.
    /// Does **not** touch the engine: keep each call site's existing
    /// engine-sync calls, which know what part of the state to refresh.
    pub fn commit_edit(&mut self, label: &str, before: std::sync::Arc<Project>) {
        self.session.push_undo(label, before);
    }

    /// Captures a snapshot for a parameter gesture (fader/drag-value edits).
    ///
    /// Consecutive changed-frames sharing `key` collapse into one undo step.
    /// End the gesture with [`ResoZApp::end_param_edit`] when the drag stops.
    pub fn begin_param_edit(&self) -> std::sync::Arc<Project> {
        self.session.project_arc()
    }

    /// Records one frame of a parameter gesture (a no-op mid-gesture).
    pub fn commit_param_edit(&mut self, label: &str, before: std::sync::Arc<Project>, key: &str) {
        self.session.push_undo_merged(label, before, key);
    }

    /// Ends a parameter gesture started via [`ResoZApp::commit_param_edit`].
    pub fn end_param_edit(&mut self) {
        self.session.end_merge();
    }

    /// Runs a command transactionally (undoable, panic-safe).
    ///
    /// Only the MIDI commands exist so far, so a successful run re-pushes the
    /// clip tables. Commands that change the node graph must call
    /// [`ResoZApp::apply_document`] themselves.
    pub fn execute(
        &mut self,
        id: &str,
        params: &serde_json::Value,
    ) -> resoz_command::Result<serde_json::Value> {
        let outcome = self.session.execute(id, params)?;
        self.push_all_clips();
        Ok(outcome)
    }

    /// Runs a command as one step of a continuous gesture.
    pub fn execute_merged(
        &mut self,
        id: &str,
        params: &serde_json::Value,
        key: &str,
    ) -> resoz_command::Result<serde_json::Value> {
        self.session.execute_merged(id, params, key)
    }

    /// Built-in synth timbre parameters of a track.
    fn synth_of_track(&self, track: usize) -> Option<SynthParams> {
        match self.project().graph.instrument_of_track(track)? {
            NodeDoc::Instrument { synth, .. } => Some(*synth),
            _ => None,
        }
    }

    /// Built-in synth timbre parameters of a track (mutable).
    fn synth_mut(&mut self, track: usize) -> Option<&mut SynthParams> {
        match self.project_mut().graph.instrument_of_track_mut(track)? {
            NodeDoc::Instrument { synth, .. } => Some(synth),
            _ => None,
        }
    }

    /// Built-in delay parameters of a track.
    fn delay_of_track(&self, track: usize) -> Option<DelayParams> {
        match self.project().graph.builtin_delay_of_track(track)? {
            NodeDoc::Effect { delay, .. } => Some(*delay),
            _ => None,
        }
    }

    /// Built-in delay parameters of a track (mutable).
    fn delay_mut(&mut self, track: usize) -> Option<&mut DelayParams> {
        match self.project_mut().graph.builtin_delay_of_track_mut(track)? {
            NodeDoc::Effect { delay, .. } => Some(delay),
            _ => None,
        }
    }

    /// Track → mixer channel mapping (P5f D17/D18): walk the track's audio
    /// chain from the built-in delay downstream until a cable lands on a
    /// mixer node; returns `(mixer doc node_id, channel index)`. `None` when
    /// the track is not routed into any mixer.
    fn track_channel_target(&self, track: usize) -> Option<(u64, u8)> {
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

    /// State of the mixer channel a track feeds: `(muted, gain, pan)`;
    /// defaults when the track is not routed into a mixer.
    fn track_mixer_channel_state(&self, track: usize) -> (bool, f32, f32) {
        match self.track_channel_target(track) {
            Some((mixer_doc, ch)) => match self.project().graph.node_by_id(mixer_doc) {
                Some(NodeDoc::Mixer { channels, .. }) => channels
                    .get(ch as usize)
                    .map(|c| (c.muted, c.gain, c.pan))
                    .unwrap_or((false, 0.8, 0.0)),
                _ => (false, 0.8, 0.0),
            },
            None => (false, 0.8, 0.0),
        }
    }

    // ---- Automation track dispatch (D7/D12: curve and cable endpoint are separated) ----

    /// Push the clip table of one control track (its delivery endpoints are decided by the graph's
    /// automation cable table, set via `apply_document` / `set_auto_endpoint`).
    fn send_control_clips(&mut self, index: usize) {
        let Some(track) = self.project().control_tracks.get(index) else {
            return;
        };
        // Document clips → engine representation (D15 batch Copy; over-capacity
        // content is truncated, the document validator rejects it on save).
        let mut clips = [RtControlClip::default(); MAX_TRACK_CLIPS];
        for (slot, clip) in clips.iter_mut().zip(track.clips.iter()) {
            let mut curve = RtAutomation::default();
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
        self.send_sequencer(SequencerCommand::SetControlClips { index, clips });
    }

    /// Redirect an automation track's cable endpoint (the document cable stores the **document**
    /// node_id; engine routing maps to the engine-side id via node_map).
    fn set_auto_endpoint(&mut self, index: usize, doc_id: u64, param: AutoParam) {
        let before = self.begin_edit();
        self.project_mut().graph.remove_auto_cable(index);
        self.project_mut()
            .graph
            .cables
            .push(resoz_model::CableDoc::automation(
                index,
                doc_id,
                param.clone(),
            ));
        self.commit_edit("Route Automation", before);
        if let Some(engine_id) = self.engine_id(doc_id) {
            self.engine
                .set_auto_route(index, engine_id, param_id_of(&param));
        }
    }

    // ---- Node patch (P5d, spec §9 P5d) ----

    /// One frame of the node patch: view + interaction-intent collection; actions are applied to
    /// document + engine in one place.
    fn graph_patch(&mut self, ui: &mut egui::Ui) {
        let track_count = self.project().midi_tracks.len();
        let auto_count = self.project().control_tracks.len();
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
                ..
            } = self;
            let doc = &session.project().graph;
            let options = move |node_id: u64| -> Vec<(String, AutoParam)> {
                let mut v = Vec::new();
                let Some(node) = doc.node_by_id(node_id) else {
                    return v;
                };
                let track = node.track();
                if let NodeDoc::Instrument { .. } = node {
                    v.push(("Cutoff".to_string(), AutoParam::FilterCutoff));
                    if let Some(params) = clap.get(&track) {
                        for p in params {
                            let label = if p.name.is_empty() {
                                format!("Param {}", p.id)
                            } else {
                                p.name.clone()
                            };
                            v.push((
                                format!("CLAP {label}"),
                                AutoParam::PluginParam {
                                    param_id: p.id,
                                    name: label,
                                },
                            ));
                        }
                    }
                    if let Some(params) = vst3.get(&track) {
                        for p in params {
                            let label = if p.title.is_empty() {
                                format!("Param {}", p.id)
                            } else {
                                p.title.clone()
                            };
                            v.push((
                                format!("VST3 {label}"),
                                AutoParam::PluginParam {
                                    param_id: p.id,
                                    name: label,
                                },
                            ));
                        }
                    }
                }
                v
            };
            patch::show(
                ui,
                patch_state,
                patch::PatchDeps {
                    doc,
                    track_count,
                    auto_count,
                    auto_options: &options,
                },
            )
        };
        self.apply_patch_actions(actions);
    }

    /// Apply patch actions: sync document and engine (reusing existing graph-build/connect
    /// methods).
    fn apply_patch_actions(&mut self, actions: Vec<patch::PatchAction>) {
        for action in actions {
            match action {
                patch::PatchAction::AddNode { kind, at } => {
                    self.add_patch_node(kind, at);
                }
                patch::PatchAction::InsertNode { cable, kind, at } => {
                    self.insert_node_in_cable(cable, kind, at);
                }
                patch::PatchAction::ConnectAudio {
                    from_doc,
                    to,
                    in_port,
                } => {
                    self.connect_audio(from_doc, to, in_port);
                }
                patch::PatchAction::ConnectMidi { track, to_doc } => {
                    let dup = self.project().graph.cables.iter().any(|c| {
                        c.kind == CableKind::Midi
                            && c.from == PortRef::MidiTrack(track)
                            && c.to == PortRef::Node(to_doc)
                    });
                    if !dup {
                        let before = self.begin_edit();
                        self.project_mut()
                            .graph
                            .cables
                            .push(resoz_model::CableDoc::midi(track, to_doc));
                        self.commit_edit("Connect MIDI", before);
                    }
                    if let Some(to_engine) = self.node_map.get(&to_doc).copied() {
                        if !self.engine.connect_midi_node(track, to_engine) {
                            self.set_message(
                                "MIDI connect failed: fan-out limit / queue full".to_string(),
                            );
                        }
                    }
                }
                patch::PatchAction::ConnectAuto {
                    index,
                    to_doc,
                    param,
                } => {
                    self.set_auto_endpoint(index, to_doc, param);
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
            }
        }
    }

    /// Add a new document node from the patch context menu: allocate the next document
    /// node_id, install the processor into the engine (born dangling — no cables; D6),
    /// place it on the patch at the click position, and refresh the owning track's
    /// addressing table. The new node's owning track is the currently selected track.
    fn add_patch_node(&mut self, kind: patch::AddNodeKind, at: egui::Pos2) {
        self.create_patch_node(kind, at);
    }

    /// Create + install a patch node (returns the new document node_id; `None` when the
    /// node table is full or the engine install failed — the document is rolled back so
    /// doc and engine stay in sync).
    fn create_patch_node(&mut self, kind: patch::AddNodeKind, at: egui::Pos2) -> Option<u64> {
        if self.project().graph.nodes.len() >= MAX_NODES {
            self.set_message("Add node failed: node table full (64)".to_string());
            return None;
        }
        let node_id = self.project().graph.next_node_id();
        let track = self
            .selected_track
            .min(self.project().midi_tracks.len().saturating_sub(1));
        let node = match kind {
            patch::AddNodeKind::Instrument => NodeDoc::Instrument {
                node_id,
                track,
                voice: VoiceSpec::Builtin,
                synth: SynthParams::default(),
            },
            patch::AddNodeKind::Effect => NodeDoc::Effect {
                node_id,
                track,
                voice: VoiceSpec::Builtin,
                delay: DelayParams::default(),
            },
            patch::AddNodeKind::Mixer => NodeDoc::Mixer {
                node_id,
                track,
                inputs: MIN_MIXER_CHANNELS as u8,
                channels: vec![Default::default(); MIN_MIXER_CHANNELS],
                out_pan: 0.0,
                out_muted: false,
            },
        };
        let sample_rate = self.engine.info().sample_rate;
        let before = self.begin_edit();
        self.project_mut().graph.nodes.push(node.clone());
        self.install_node(&node, sample_rate);
        if !self.node_map.contains_key(&node_id) {
            // Engine slot/queue full: roll the document back so doc and engine stay one.
            self.session.restore(before);
            self.set_message("Add node failed (engine slots or queue full)".to_string());
            return None;
        }
        self.commit_edit("Add Node", before);
        self.patch_state.place_node(node_id, at);
        self.register_track_chain(track);
        Some(node_id)
    }

    /// Insert a new node into an existing audio cable: `A → B` becomes `A → new → B`
    /// (cable-midpoint menu). The new node is placed where the user clicked.
    fn insert_node_in_cable(
        &mut self,
        cable_index: usize,
        kind: patch::AddNodeKind,
        at: egui::Pos2,
    ) {
        let Some(cable) = self.project().graph.cables.get(cable_index).cloned() else {
            return;
        };
        if cable.kind != CableKind::Audio {
            self.set_message("Insert failed: only audio cables accept inserted nodes".to_string());
            return;
        }
        if !matches!(kind, patch::AddNodeKind::Effect | patch::AddNodeKind::Mixer) {
            self.set_message("Insert failed: only effects / mixers can be inserted".to_string());
            return;
        }
        let PortRef::Node(from_doc) = cable.from else {
            self.set_message("Insert failed: cable has no node source".to_string());
            return;
        };
        let Some(new_id) = self.create_patch_node(kind, at) else {
            return;
        };
        self.delete_cable(cable_index);
        self.connect_audio(from_doc, PortRef::Node(new_id), 0);
        // Preserve the original channel binding when the downstream target is
        // a mixer (D17).
        self.connect_audio(new_id, cable.to.clone(), cable.in_port);
    }

    /// Connect an audio cable (document deduplication + engine edge; both stay in sync).
    /// `in_port` binds the cable to the target's input port (the mixer channel
    /// for mixers, D17; 0 for single-input nodes / master).
    fn connect_audio(&mut self, from_doc: u64, to: PortRef, in_port: u8) {
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
                .push(resoz_model::CableDoc::audio_to_channel(
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

    /// Delete a cable (document index i; document and engine stay in sync).
    fn delete_cable(&mut self, i: usize) {
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
            CableKind::Midi => {
                if let (PortRef::MidiTrack(track), PortRef::Node(to_doc)) = (&cable.from, &cable.to)
                {
                    if let Some(to_engine) = self.node_map.get(to_doc).copied() {
                        self.engine.disconnect_midi_node(*track, to_engine);
                    }
                }
            }
            CableKind::Automation => {
                if let PortRef::AutoTrack(index) = cable.from {
                    self.engine.clear_auto_route(index);
                }
            }
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
    fn delete_graph_node(&mut self, doc_id: u64) {
        let Some(node) = self.project().graph.node_by_id(doc_id) else {
            return;
        };
        let track = node.track();
        let audio_in_count = self
            .project()
            .graph
            .cables
            .iter()
            .filter(|c| c.kind == CableKind::Audio && c.to == PortRef::Node(doc_id))
            .count();
        // Document bypass pairs: in-cable sources × out-cable targets (audio
        // only; skipped for multi-input nodes — D19 mixer/master deletion).
        let ins: Vec<PortRef> = if audio_in_count <= 1 {
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
        let Some(&engine_id) = self.node_map.get(&doc_id) else {
            return;
        };
        if !self.engine.remove_node(engine_id) {
            self.set_message("Node removal failed (queue full)".to_string());
            return;
        }
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
                    .push(resoz_model::CableDoc::audio_to_channel(
                        from.clone(),
                        to.clone(),
                        port,
                    ));
            }
        }
        self.node_map.remove(&doc_id);
        self.retire_guard(engine_id);
        self.commit_edit("Delete Node", before);
        // The deleted node may have filled a role (voice / delay / channel) in the track
        // addressing table — refresh it from the remaining document nodes.
        self.register_track_chain(track);
    }

    /// Document clip table → engine representation (D15 batch, truncated to capacity, `Copy`
    /// structs, zero heap allocation).
    fn rt_clips(&self, track: usize) -> [RtMidiClip; MAX_TRACK_CLIPS] {
        let mut clips = [RtMidiClip::default(); MAX_TRACK_CLIPS];
        for (slot, clip) in clips
            .iter_mut()
            .zip(self.project().midi_tracks[track].clips.iter())
        {
            let mut rt = RtMidiClip {
                start_ticks: clip.start_ticks,
                span_ticks: clip.span_ticks,
                loop_ticks: clip.loop_ticks,
                ..RtMidiClip::default()
            };
            for note in clip.notes().iter().take(MAX_CLIP_NOTES) {
                rt.push(RtNote::new(
                    note.start,
                    note.length,
                    note.note,
                    note.channel,
                    note.velocity,
                ));
            }
            *slot = rt;
        }
        clips
    }

    /// Push the current clip table of one track (D15 batch).
    fn send_track_clips(&mut self, track: usize) {
        let clips = self.rt_clips(track);
        self.send_sequencer(SequencerCommand::SetTrackClips { track, clips });
    }

    /// Re-push every track's clip table to the sequencer.
    ///
    /// Called after a command edits the document; the audio thread holds its
    /// own copy of the clip tables, so they must be refreshed even when the
    /// edit was a single note.
    fn push_all_clips(&mut self) {
        for track in 0..self.project().midi_tracks.len() {
            self.send_track_clips(track);
        }
    }

    /// Reverts the last document edit (Ctrl+Z).
    ///
    /// Only command-recorded steps are undoable; edits made through
    /// [`ResoZApp::project_mut`] are not in the history yet.
    pub fn undo(&mut self) {
        self.roll_state.clear_drag();
        self.lane_state.clear_drag();
        if self.session.undo() {
            // Undo can restore any kind of edit (a deleted node, a rerouted
            // cable), so the engine gets the full rebuild, not just clip
            // tables.
            self.apply_document();
        } else {
            self.set_message("Nothing to undo".to_string());
        }
    }

    /// Reapplies the last undone edit (Ctrl+Y).
    pub fn redo(&mut self) {
        self.roll_state.clear_drag();
        self.lane_state.clear_drag();
        if self.session.redo() {
            self.apply_document();
        } else {
            self.set_message("Nothing to redo".to_string());
        }
    }

    /// Save to the current path; if never saved, pop up the "Save As" dialog.
    fn save_project(&mut self) {
        match self.project_path.clone() {
            Some(path) => self.do_save(path),
            None => self.save_project_as(),
        }
    }

    /// "Save As": pop up the save dialog.
    fn save_project_as(&mut self) {
        let Some(mut path) = rfd::FileDialog::new()
            .add_filter("ResoZ Project", &["rez"])
            .set_file_name("Untitled.rez")
            .save_file()
        else {
            return;
        };
        path.set_extension("rez");
        self.do_save(path);
    }

    /// "Open": pop up the file picker dialog.
    fn open_project_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("ResoZ Project", &["rez"])
            .pick_file()
        else {
            return;
        };
        self.do_open(path);
    }

    /// Write to disk and update recent files / title / status message.
    fn do_save(&mut self, path: PathBuf) {
        // Persist the patch canvas layout (view data) with the project.
        let layout = self.patch_state.export_layout(&self.project().graph);
        self.project_mut().graph.layout = layout;
        match resoz_model::save_project(self.project(), &path) {
            Ok(()) => {
                self.session.mark_clean();
                self.project_path = Some(path.clone());
                self.push_recent(path.clone());
                self.window_title = format!("ResoZ — {}", display_name(&path));
                self.set_message(format!("Saved {}", path.display()));
                log::info!("Project saved: {}", path.display());
            }
            Err(err) => {
                self.set_message(format!("Save failed: {err}"));
                log::error!("Failed to save project: {err}");
            }
        }
    }

    /// Load from disk, replace the document, rebuild the engine graph per the document, and
    /// re-push all parameters.
    fn do_open(&mut self, path: PathBuf) {
        match resoz_model::load_project(&path) {
            Ok(project) => {
                self.session.replace(project);
                // Seed the patch view from the persisted layout (session positions of
                // the previous document are dropped). Snapshot first: `project()`
                // borrows all of `self`, which would clash with `patch_state`.
                let layout = self.project().graph.layout.clone();
                self.patch_state.import_layout(&layout);
                self.project_path = Some(path.clone());
                self.push_recent(path.clone());
                self.selected_track = 0;
                self.selected_auto = 0;
                self.edit_clip = None;
                self.roll_state = piano_roll::RollState::new();
                self.push_all_params();
                self.window_title = format!("ResoZ — {}", display_name(&path));
                self.set_message(format!("Opened {}", path.display()));
                log::info!("Project opened: {}", path.display());
            }
            Err(err) => {
                self.set_message(format!("Open failed: {err}"));
                log::error!("Failed to open project: {err}");
            }
        }
    }

    /// De-duplicate the recent files, move to front, and truncate.
    fn push_recent(&mut self, path: PathBuf) {
        self.recent.retain(|p| p != &path);
        self.recent.insert(0, path);
        self.recent.truncate(RECENT_FILES_MAX);
    }

    /// Set a status bar message (disappears automatically after a timeout).
    fn set_message(&mut self, text: String) {
        self.status_message = Some((text, Instant::now()));
    }

    /// Restore the selected track's instrument to the built-in synth (in-place replacement,
    /// node identity unchanged).
    fn set_builtin_instrument(&mut self, track: usize) {
        let before = self.begin_edit();
        if let Some(NodeDoc::Instrument { voice, .. }) =
            self.project_mut().graph.instrument_of_track_mut(track)
        {
            *voice = VoiceSpec::Builtin;
        }
        self.voice_params.remove(&track);
        self.clap_voice_params.remove(&track);
        let Some(doc_id) = self
            .project()
            .graph
            .instrument_of_track(track)
            .map(NodeDoc::node_id)
        else {
            return;
        };
        let Some(engine_id) = self.engine_id(doc_id) else {
            return;
        };
        let sample_rate = convert::sample_rate_to_f32(self.engine.info().sample_rate);
        if self
            .engine
            .replace_node(engine_id, Box::new(SynthProcessor::new(sample_rate)))
            .is_none()
        {
            log::error!(
                "Failed to restore instrument on track {} (missing node or queue full)",
                track + 1
            );
        }
        self.commit_edit("Set Instrument Voice", before);
    }

    /// Load a CLAP instrument voice into the track: non-real-time instantiation → in-place engine
    /// replacement → guard registration. On success, also enumerate plugin parameters (data
    /// source for automation cable targets).
    fn load_clap_voice(&mut self, track: usize, path: &str, plugin_id: &str) {
        let Some(doc_id) = self
            .project()
            .graph
            .instrument_of_track(track)
            .map(NodeDoc::node_id)
        else {
            return;
        };
        let Some(engine_id) = self.engine_id(doc_id) else {
            return;
        };
        self.retire_guard(engine_id);
        match ClapInstance::spawn_rt(
            std::path::Path::new(path),
            plugin_id,
            ClapInstanceConfig {
                sample_rate: self.engine.info().sample_rate,
                max_frames: 128,
            },
        ) {
            Ok(instance) => {
                // Parameter enumeration must happen before the instance is destructured (the
                // guard holds the plugin instance).
                let params = instance.list_params();
                let ClapInstance { guard, node } = instance;
                if self
                    .engine
                    .replace_node(engine_id, Box::new(node))
                    .is_some()
                {
                    self.voice_guards.push(PluginGuardEntry {
                        node_id: engine_id,
                        track,
                        plugin_id: plugin_id.to_string(),
                        _guard: PluginGuard::Clap(guard),
                    });
                    self.clap_voice_params.insert(track, params);
                    log::info!("Track {} loaded CLAP voice: {plugin_id}", track + 1);
                } else {
                    log::error!("Voice queue full: failed to load track {}", track + 1);
                }
            }
            Err(err) => {
                log::error!("Failed to instantiate CLAP plugin {plugin_id}: {err}");
                self.set_message(format!("Plugin load failed: {err}"));
            }
        }
    }

    /// Load a VST3 instrument voice into the track: non-real-time instantiation → in-place engine
    /// replacement → guard registration.
    fn load_vst3_voice(&mut self, track: usize, path: &str, class_id: &str) {
        let Some(doc_id) = self
            .project()
            .graph
            .instrument_of_track(track)
            .map(NodeDoc::node_id)
        else {
            return;
        };
        let Some(engine_id) = self.engine_id(doc_id) else {
            return;
        };
        self.retire_guard(engine_id);
        match resoz_vst3_host::Vst3Instance::spawn_rt(
            std::path::Path::new(path),
            class_id,
            resoz_vst3_host::Vst3InstanceConfig {
                sample_rate: self.engine.info().sample_rate,
                max_frames: 128,
            },
        ) {
            Ok(instance) => {
                let resoz_vst3_host::Vst3Instance { guard, node } = instance;
                // Enumerate the parameter list on the control side (read before the guard moves
                // into the registry).
                let params = guard.parameters();
                if self
                    .engine
                    .replace_node(engine_id, Box::new(node))
                    .is_some()
                {
                    self.voice_params.insert(track, params);
                    self.voice_guards.push(PluginGuardEntry {
                        node_id: engine_id,
                        track,
                        plugin_id: class_id.to_string(),
                        _guard: PluginGuard::Vst3(guard),
                    });
                    log::info!("Track {} loaded VST3 voice: {class_id}", track + 1);
                } else {
                    log::error!("Voice queue full: failed to load track {}", track + 1);
                }
            }
            Err(err) => {
                log::error!("Failed to instantiate VST3 plugin {class_id}: {err}");
                self.set_message(format!("VST3 plugin load failed: {err}"));
            }
        }
    }

    /// Insert an external effect node at the end of the track chain (before the
    /// mixer channel / master landing): engine `insert_node_between` + document
    /// sync (node + splice rewiring preserving the downstream in_port, D17).
    fn insert_effect_common(
        &mut self,
        track: usize,
        voice: VoiceSpec,
        plugin_label: String,
        node: Box<dyn Processor>,
        guard: PluginGuard,
    ) {
        let Some(delay_doc) = self
            .project()
            .graph
            .builtin_delay_of_track(track)
            .map(NodeDoc::node_id)
        else {
            log::error!(
                "Track {} has no built-in delay node; effect insertion failed",
                track + 1
            );
            return;
        };
        // Walk the audio chain from the delay to its LAST node (P5f: the chain
        // ends at the mixer channel / master landing, not a channel strip).
        let mut last_doc = delay_doc;
        let downstream = loop {
            let Some(cable) = self
                .project()
                .graph
                .cables
                .iter()
                .find(|c| c.kind == CableKind::Audio && c.from == PortRef::Node(last_doc))
                .cloned()
            else {
                log::error!(
                    "Track {} chain is dangling after node {last_doc}; effect insertion failed",
                    track + 1
                );
                return;
            };
            match &cable.to {
                PortRef::Node(next_id)
                    if !self
                        .project()
                        .graph
                        .node_by_id(*next_id)
                        .is_some_and(|n| n.is_mixer()) =>
                {
                    last_doc = *next_id;
                }
                _ => break cable, // mixer / master landing: splice here
            }
        };
        let PortRef::Node(downstream_doc) = downstream.to else {
            log::error!("Track {} chain landing is not a node", track + 1);
            return;
        };
        let Some(last_engine) = self.engine_id(last_doc) else {
            return;
        };
        let Some(downstream_engine) = self.engine_id(downstream_doc) else {
            return;
        };
        let new_doc_id = self.project().graph.next_node_id();
        let new_engine_id = self.engine.fresh_node_id();
        if !self
            .engine
            .insert_node_between(new_engine_id, last_engine, downstream_engine, node)
        {
            log::error!(
                "Structure queue full: failed to insert effect on track {}",
                track + 1
            );
            return;
        }
        // Document sync: node + splice rewiring (the downstream cable keeps its
        // in_port — the mixer channel binding, D17).
        let before = self.begin_edit();
        self.project_mut().graph.nodes.push(NodeDoc::Effect {
            node_id: new_doc_id,
            track,
            voice,
            delay: DelayParams::default(),
        });
        self.project_mut().graph.cables.retain(|c| {
            !(c.kind == CableKind::Audio
                && c.from == PortRef::Node(last_doc)
                && c.to == PortRef::Node(downstream_doc)
                && c.in_port == downstream.in_port)
        });
        self.project_mut()
            .graph
            .cables
            .push(resoz_model::CableDoc::audio(last_doc, new_doc_id));
        self.project_mut()
            .graph
            .cables
            .push(resoz_model::CableDoc::audio_to_channel(
                new_doc_id,
                PortRef::Node(downstream_doc),
                downstream.in_port,
            ));
        self.node_map.insert(new_doc_id, new_engine_id);
        self.voice_guards.push(PluginGuardEntry {
            node_id: new_engine_id,
            track,
            plugin_id: plugin_label,
            _guard: guard,
        });
        self.commit_edit("Insert Effect", before);
        log::info!("Track {} inserted external effect", track + 1);
    }

    /// Insert a CLAP effect node into the track (non-real-time instantiation → engine insertion →
    /// guard registration).
    fn load_clap_effect(&mut self, track: usize, path: &str, plugin_id: &str) {
        match ClapInstance::spawn_rt(
            std::path::Path::new(path),
            plugin_id,
            ClapInstanceConfig {
                sample_rate: self.engine.info().sample_rate,
                max_frames: 128,
            },
        ) {
            Ok(instance) => {
                let ClapInstance { guard, node } = instance;
                self.insert_effect_common(
                    track,
                    VoiceSpec::Clap {
                        path: path.to_string(),
                        plugin_id: plugin_id.to_string(),
                    },
                    plugin_id.to_string(),
                    Box::new(node),
                    PluginGuard::Clap(guard),
                );
            }
            Err(err) => {
                log::error!("Failed to instantiate CLAP effect {plugin_id}: {err}");
                self.set_message(format!("Plugin load failed: {err}"));
            }
        }
    }

    /// Insert a VST3 effect node into the track (non-real-time instantiation → engine insertion →
    /// guard registration).
    fn load_vst3_effect(&mut self, track: usize, path: &str, class_id: &str) {
        match resoz_vst3_host::Vst3Instance::spawn_rt(
            std::path::Path::new(path),
            class_id,
            resoz_vst3_host::Vst3InstanceConfig {
                sample_rate: self.engine.info().sample_rate,
                max_frames: 128,
            },
        ) {
            Ok(instance) => {
                let resoz_vst3_host::Vst3Instance { guard, node } = instance;
                self.insert_effect_common(
                    track,
                    VoiceSpec::Vst3 {
                        path: path.to_string(),
                        class_id: class_id.to_string(),
                    },
                    class_id.to_string(),
                    Box::new(node),
                    PluginGuard::Vst3(guard),
                );
            }
            Err(err) => {
                log::error!("Failed to instantiate VST3 effect {class_id}: {err}");
                self.set_message(format!("VST3 plugin load failed: {err}"));
            }
        }
    }

    /// Clear all external effects of a track (built-in delay kept): engine bypass removal +
    /// document sync.
    fn clear_external_effects(&mut self, track: usize) {
        let before = self.begin_edit();
        while let Some(effect) = self.project().graph.nodes.iter().find(|n| {
            n.track() == track
                && match n {
                    NodeDoc::Effect { voice, .. } => !matches!(voice, VoiceSpec::Builtin),
                    _ => false,
                }
        }) {
            let effect_doc = effect.node_id();
            let Some(&effect_engine) = self.node_map.get(&effect_doc) else {
                break;
            };
            // Document cables: in-cable source and out-cable target (bypass direct pair).
            let in_from = self.project().graph.cables.iter().find_map(|c| {
                if c.kind == CableKind::Audio && c.to == PortRef::Node(effect_doc) {
                    match c.from {
                        PortRef::Node(id) => Some(id),
                        _ => None,
                    }
                } else {
                    None
                }
            });
            let out_to = self.project().graph.cables.iter().find_map(|c| {
                if c.kind == CableKind::Audio && c.from == PortRef::Node(effect_doc) {
                    Some(c.to.clone())
                } else {
                    None
                }
            });
            let (Some(in_from), Some(out_to)) = (in_from, out_to) else {
                log::warn!(
                    "Track {track} effect {effect_doc} has incomplete cables; stopping cleanup"
                );
                break;
            };
            if !self.engine.remove_node(effect_engine) {
                break;
            }
            self.project_mut()
                .graph
                .nodes
                .retain(|n| n.node_id() != effect_doc);
            self.project_mut().graph.cables.retain(|c| {
                !(c.kind == CableKind::Audio
                    && (c.from == PortRef::Node(effect_doc) || c.to == PortRef::Node(effect_doc)))
            });
            self.project_mut()
                .graph
                .cables
                .push(resoz_model::CableDoc::audio(in_from, out_to));
            self.node_map.remove(&effect_doc);
            self.retire_guard(effect_engine);
        }
        self.commit_edit("Clear External Effects", before);
    }

    /// Scanned CLAP instruments (lazy).
    fn scanned_instruments(&mut self) -> &[resoz_clap_host::ClapPluginInfo] {
        if self.clap_plugins.is_none() {
            self.clap_plugins = Some(
                resoz_clap_host::list_clap_plugins()
                    .into_iter()
                    .filter(|info| info.is_instrument)
                    .collect(),
            );
        }
        self.clap_plugins.as_deref().unwrap_or(&[])
    }

    /// Scanned CLAP effects (lazy).
    fn scanned_effects(&mut self) -> &[resoz_clap_host::ClapPluginInfo] {
        if self.clap_effects.is_none() {
            self.clap_effects = Some(
                resoz_clap_host::list_clap_plugins()
                    .into_iter()
                    .filter(|info| !info.is_instrument)
                    .collect(),
            );
        }
        self.clap_effects.as_deref().unwrap_or(&[])
    }

    /// Scanned VST3 instruments (lazy).
    fn scanned_vst3_instruments(&mut self) -> &[resoz_vst3_host::Vst3PluginInfo] {
        if self.vst3_plugins.is_none() {
            self.vst3_plugins = Some(
                resoz_vst3_host::list_vst3_plugins()
                    .into_iter()
                    .filter(|info| info.is_instrument)
                    .collect(),
            );
        }
        self.vst3_plugins.as_deref().unwrap_or(&[])
    }

    /// Scanned VST3 effects (lazy).
    fn scanned_vst3_effects(&mut self) -> &[resoz_vst3_host::Vst3PluginInfo] {
        if self.vst3_effects.is_none() {
            self.vst3_effects = Some(
                resoz_vst3_host::list_vst3_plugins()
                    .into_iter()
                    .filter(|info| !info.is_instrument)
                    .collect(),
            );
        }
        self.vst3_effects.as_deref().unwrap_or(&[])
    }

    fn send_sequencer(&mut self, cmd: SequencerCommand) {
        self.engine.send(EngineCommand::Sequencer(cmd));
    }

    fn send_note_on(&mut self, note: u8, velocity: f32) {
        let track = self.selected_track;
        self.engine.send(EngineCommand::Note {
            track,
            event: NoteEvent::On { note, velocity },
        });
        self.held_notes.insert(note);
        self.current_note = Some(note);
    }

    fn send_note_off(&mut self, note: u8) {
        let track = self.selected_track;
        self.engine.send(EngineCommand::Note {
            track,
            event: NoteEvent::Off { note },
        });
        self.held_notes.remove(&note);
    }

    /// Handle computer keyboard playing (FL Studio-style layout; Z/X shift octaves).
    ///
    /// TODO: once text input widgets appear in the window, keyboard focus must be checked first
    /// so typing does not trigger notes.
    fn handle_keyboard_input(&mut self, ctx: &egui::Context) {
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
            if repeat {
                continue;
            }
            // Shortcuts take precedence over the playing map (Ctrl+S save / Ctrl+O open /
            // Ctrl+Z undo).
            if modifiers.ctrl && pressed {
                match key {
                    egui::Key::S => {
                        self.save_project();
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
                    _ => {
                        continue; // other Ctrl combos do not trigger notes
                    }
                }
            }
            match (key, pressed) {
                (egui::Key::Z, true) => {
                    self.octave_base = (self.octave_base.saturating_sub(12)).max(24);
                }
                (egui::Key::X, true) => {
                    self.octave_base = (self.octave_base + 12).min(84);
                }
                _ => {
                    if let Some((_, semi)) = KEYMAP.iter().find(|(k, _)| *k == key) {
                        let note = self.octave_base + semi;
                        if pressed {
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

    fn transport_panel(&mut self, ui: &mut egui::Ui) {
        let playing = self.engine.is_playing();
        egui::Panel::top("resoz_transport").show(ui, |ui| {
            ui.horizontal(|ui| {
                let mut play_btn = egui::Button::new("▶ Play");
                if playing {
                    play_btn = play_btn.fill(egui::Color32::DARK_GREEN);
                }
                if ui.add(play_btn).clicked() {
                    self.send_sequencer(SequencerCommand::Play);
                }
                if ui.button("■ Stop").clicked() {
                    self.send_sequencer(SequencerCommand::Stop);
                }
                ui.separator();
                ui.label("Tempo");
                let before = self.begin_param_edit();
                let bpm = ui.add(
                    egui::DragValue::new(&mut self.project_mut().metadata.tempo_bpm)
                        .range(*BPM_RANGE.start()..=*BPM_RANGE.end())
                        .speed(0.5)
                        .suffix(" BPM"),
                );
                if bpm.changed() {
                    // One drag = one undo step; keyboard nudges are separate steps.
                    if bpm.dragged() {
                        self.commit_param_edit("Tempo", before, "tempo");
                    } else {
                        self.commit_edit("Tempo", before);
                    }
                    self.send_sequencer(SequencerCommand::SetBpm(convert::to_f32(
                        self.project().metadata.tempo_bpm,
                    )));
                }
                if bpm.drag_stopped() {
                    self.end_param_edit();
                }
                ui.separator();
                if ui.button("📂 Open").clicked() {
                    self.open_project_dialog();
                }
                if ui.button("💾 Save").clicked() {
                    self.save_project();
                }
                ui.menu_button("Recent Files", |ui| {
                    if self.recent.is_empty() {
                        ui.weak("(empty)");
                    }
                    let recent = self.recent.clone();
                    for path in recent {
                        if ui.button(display_name(&path)).clicked() {
                            self.do_open(path);
                        }
                    }
                });
                ui.separator();
                ui.weak("ResoZ — Rust DAW");
            });
        });
    }

    /// Arrangement timeline panel (timeline spec T3): rows = tracks, clip
    /// blocks with move/resize/insert/delete, double-click opens the editor.
    /// Main view tab bar (stacks under the transport panel).
    fn view_tabs(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("resoz_views").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view, MainView::Timeline, "Timeline");
                ui.selectable_value(&mut self.view, MainView::Patch, "Patch");
                ui.selectable_value(&mut self.view, MainView::Synth, "Synth / Mixer");
            });
        });
    }

    fn timeline_panel(&mut self, ui: &mut egui::Ui) {
        if self.project().midi_tracks.is_empty() && self.project().control_tracks.is_empty() {
            return;
        }
        let muted: Vec<bool> = (0..self.project().midi_tracks.len())
            .map(|i| self.track_mixer_channel_state(i).0)
            .collect();
        let playhead = self.engine.musical_ticks();
        let playing = self.engine.is_playing();
        let selected_track = self.selected_track;
        let selected_auto = self.selected_auto;
        let edit_target = self.edit_clip.map(|t| match t {
            EditTarget::Midi { track, clip } => timeline::EditHighlight {
                is_control: false,
                track,
                clip,
            },
            EditTarget::Control { track, clip } => timeline::EditHighlight {
                is_control: true,
                track,
                clip,
            },
        });
        let actions = {
            let Self {
                session,
                timeline_state: state,
                ..
            } = self;
            // Read-only: the timeline is a view now; its edit actions come
            // back as data and are applied (undoably) below.
            let project = session.project();
            let (midi, control) = (&project.midi_tracks, &project.control_tracks);
            timeline::show(
                ui,
                state,
                midi,
                control,
                timeline::TimelineParams {
                    muted: &muted,
                    selected_track,
                    selected_auto,
                    playhead_ticks: playhead,
                    playing,
                    edit_target,
                },
            )
        };
        for action in actions {
            self.apply_timeline_action(action);
        }
    }

    /// Applies one timeline view action (engine pushes / cross-panel state).
    fn apply_timeline_action(&mut self, action: timeline::TimelineAction) {
        match action {
            timeline::TimelineAction::MidiEdited(track) => self.send_track_clips(track),
            timeline::TimelineAction::ControlEdited(index) => self.send_control_clips(index),
            timeline::TimelineAction::SelectTrack(track) => {
                self.selected_track = track;
                self.edit_clip = None;
            }
            timeline::TimelineAction::SelectControlTrack(index) => {
                self.selected_auto = index;
                self.edit_clip = None;
            }
            timeline::TimelineAction::ToggleMute(track, muted) => {
                // P5f: mute lives on the track's mixer channel (D15), not a strip.
                if let Some((mixer_doc, ch)) = self.track_channel_target(track) {
                    let before = self.begin_edit();
                    if let Some(NodeDoc::Mixer { channels, .. }) =
                        self.project_mut().graph.node_by_id_mut(mixer_doc)
                        && let Some(c) = channels.get_mut(ch as usize)
                    {
                        c.muted = muted;
                    }
                    self.commit_edit("Toggle Mute", before);
                    if let Some(&engine_id) = self.node_map.get(&mixer_doc) {
                        self.engine.send(EngineCommand::SetNodeParam {
                            node_id: engine_id,
                            id: ParamId::MixerMute(ch),
                            value: if muted { 1.0 } else { 0.0 },
                        });
                    }
                }
            }
            timeline::TimelineAction::OpenMidiEditor { track, clip } => {
                self.selected_track = track;
                self.edit_clip = Some(EditTarget::Midi { track, clip });
            }
            timeline::TimelineAction::OpenControlEditor { track, clip } => {
                self.selected_auto = track;
                self.edit_clip = Some(EditTarget::Control { track, clip });
            }
            timeline::TimelineAction::AddMidiTrack => self.add_midi_track(),
            timeline::TimelineAction::RemoveLastMidiTrack => self.remove_last_midi_track(),
            timeline::TimelineAction::AddClip {
                is_control,
                track,
                start_ticks,
            } => self.timeline_add_clip(is_control, track, start_ticks),
            timeline::TimelineAction::MoveResizeClip {
                is_control,
                track,
                clip,
                start_ticks,
                span_ticks,
            } => self.timeline_move_resize_clip(is_control, track, clip, start_ticks, span_ticks),
            timeline::TimelineAction::RemoveClip {
                is_control,
                track,
                clip,
            } => self.timeline_remove_clip(is_control, track, clip),
        }
    }

    /// Timeline clip edits: the view sends data, these apply it undoably.
    /// A drag frame goes through `commit_param_edit` with the gesture key, so
    /// one drag is one undo step.
    fn timeline_add_clip(&mut self, is_control: bool, track: usize, start_ticks: u32) {
        let before = self.begin_edit();
        let added = if is_control {
            self.project_mut()
                .control_tracks
                .get_mut(track)
                .map(|t| {
                    if t.clips.len() < MAX_CLIPS_PER_TRACK {
                        t.clips.push(ControlClip::new(start_ticks));
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false)
        } else {
            self.project_mut()
                .midi_tracks
                .get_mut(track)
                .map(|t| {
                    if t.clips.len() < MAX_CLIPS_PER_TRACK {
                        t.clips.push(MidiClip::new(start_ticks));
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false)
        };
        if added {
            self.commit_edit("Add Clip", before);
            self.sync_clip_row(is_control, track);
        }
    }

    fn timeline_move_resize_clip(
        &mut self,
        is_control: bool,
        track: usize,
        clip: usize,
        start_ticks: u32,
        span_ticks: u32,
    ) {
        let before = self.begin_edit();
        let moved = if is_control {
            self.project_mut()
                .control_tracks
                .get_mut(track)
                .and_then(|t| t.clips.get_mut(clip))
                .map(|c| {
                    let changed = c.start_ticks != start_ticks || c.span_ticks != span_ticks;
                    c.start_ticks = start_ticks;
                    c.span_ticks = span_ticks;
                    changed
                })
                .unwrap_or(false)
        } else {
            self.project_mut()
                .midi_tracks
                .get_mut(track)
                .and_then(|t| t.clips.get_mut(clip))
                .map(|c| {
                    let changed = c.start_ticks != start_ticks || c.span_ticks != span_ticks;
                    c.start_ticks = start_ticks;
                    c.span_ticks = span_ticks;
                    changed
                })
                .unwrap_or(false)
        };
        if moved {
            // One drag = one undo step, even across dozens of frames.
            self.commit_param_edit(
                "Move Clip",
                before,
                &format!("timeline:{}:{}:{}", is_control, track, clip),
            );
        }
    }

    fn timeline_remove_clip(&mut self, is_control: bool, track: usize, clip: usize) {
        let before = self.begin_edit();
        let removed = if is_control {
            self.project_mut()
                .control_tracks
                .get_mut(track)
                .map(|t| t.clips.remove(clip))
                .is_some()
        } else {
            self.project_mut()
                .midi_tracks
                .get_mut(track)
                .map(|t| t.clips.remove(clip))
                .is_some()
        };
        if removed {
            self.commit_edit("Delete Clip", before);
            self.sync_clip_row(is_control, track);
        }
    }

    /// Re-pushes one timeline row's clip table after an undoable edit.
    fn sync_clip_row(&mut self, is_control: bool, index: usize) {
        if is_control {
            self.send_control_clips(index);
        } else {
            self.send_track_clips(index);
        }
    }

    /// Floating editor window for the clip targeted from the arrangement
    /// timeline (double-click). Closing it (X / ×) clears the edit target.
    fn editor_window(&mut self, ctx: &egui::Context) {
        let Some(target) = self.edit_clip else {
            return;
        };
        let title = match target {
            EditTarget::Midi { .. } => "Piano Roll",
            EditTarget::Control { .. } => "Curve Editor",
        };
        let mut open = true;
        egui::Window::new(title)
            .open(&mut open)
            .default_width(760.0)
            .default_height(360.0)
            .show(ctx, |ui| match target {
                EditTarget::Midi { track, clip } => self.midi_editor_body(ui, track, clip),
                EditTarget::Control { track, clip } => self.control_editor_body(ui, track, clip),
            });
        if !open {
            self.edit_clip = None;
        }
    }

    fn status_panel(&mut self, ui: &mut egui::Ui) {
        let info = self.engine.info().clone();
        let position = self.engine.position_seconds();
        let note_text = self
            .current_note
            .map(note_name)
            .unwrap_or_else(|| "—".to_string());
        let track_name = self
            .project()
            .midi_tracks
            .get(self.selected_track)
            .map(|t| {
                if t.name.is_empty() {
                    format!("Track {}", self.selected_track + 1)
                } else {
                    t.name.clone()
                }
            })
            .unwrap_or_else(|| "—".to_string());
        // Status message (disappears automatically after a timeout).
        let message = self
            .status_message
            .take_if(|(_, at)| at.elapsed() < STATUS_MESSAGE_TTL);

        egui::Panel::bottom("resoz_status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("Device: {}", info.device_name));
                ui.separator();
                ui.label(format!(
                    "{} Hz / {} ch",
                    info.sample_rate, info.out_channels
                ));
                ui.separator();
                ui.label(format!("Editing: {track_name}"));
                ui.separator();
                ui.label(format!("Note: {note_text}"));
                ui.separator();
                // Musical transport position (bar.beat, from the sequencer's tick counter —
                // frozen while stopped). The raw seconds counter is engine uptime (audio
                // callback keeps rendering silent blocks), kept as a secondary diagnostic.
                let ticks = self.engine.musical_ticks();
                let tpb = resoz_model::TICKS_PER_BAR as f32;
                let bar = (ticks / tpb).floor().max(0.0);
                let beat = (ticks - bar * tpb) / (tpb / 4.0) + 1.0;
                ui.label(format!("Pos {}.{}", bar as u32 + 1, beat));
                ui.separator();
                ui.label(format!("Uptime {:.1} s", position));
                if let Some((text, _)) = message {
                    ui.separator();
                    ui.colored_label(egui::Color32::LIGHT_BLUE, text);
                }
            });
        });
    }

    fn synth_panel(&mut self, ui: &mut egui::Ui) {
        self.meter.update(self.engine.peak_level());
        let peak = self.meter.held;

        // In eframe 0.36 the root Ui has no margins; wrap a CentralPanel to keep the default look.
        egui::CentralPanel::default().show(ui, |ui| {
            if self.project().midi_tracks.is_empty() {
                ui.heading("Synth");
                ui.weak("(no MIDI tracks in project)");
                return;
            }
            // Long parameter stack: without a scroll area the content clips with no scrollbar.
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let sel = self.selected_track.min(self.project().midi_tracks.len() - 1);
            self.selected_track = sel;
            let track_title = if self.project().midi_tracks[sel].name.is_empty() {
                format!("Track {}", sel + 1)
            } else {
                self.project().midi_tracks[sel].name.clone()
            };
            ui.heading(format!("Synth — {track_title}"));
            ui.horizontal(|ui| {
                ui.label("Instrument");
                let is_external = self
                    .project()
                    .graph
                    .instrument_of_track(sel)
                    .and_then(|n| match n {
                        NodeDoc::Instrument { voice, .. } => Some(matches!(
                            voice,
                            VoiceSpec::Clap { .. } | VoiceSpec::Vst3 { .. }
                        )),
                        _ => None,
                    })
                    .unwrap_or(false);
                if ui.selectable_label(!is_external, "Built-in Synth").clicked() {
                    self.set_builtin_instrument(sel);
                }
                // Collect the choice first; load after the menu closure ends (avoids borrow
                // conflicts).
                let mut picked: Option<(String, String)> = None;
                ui.menu_button("CLAP Instruments…", |ui| {
                    let empty: Vec<resoz_clap_host::ClapPluginInfo> = Vec::new();
                    let plugins = self.scanned_instruments();
                    let list = if plugins.is_empty() { &empty } else { plugins };
                    if list.is_empty() {
                        ui.weak("(no instrument plugins found; set RESOZ_CLAP_PATH to add dirs)");
                    }
                    for info in list {
                        if ui.button(&info.name).clicked() {
                            picked = Some((info.path.display().to_string(), info.id.clone()));
                            ui.close();
                        }
                    }
                });
                if let Some((path, plugin_id)) = picked {
                    let before = self.begin_edit();
                    if let Some(NodeDoc::Instrument { voice, .. }) =
                        self.project_mut().graph.instrument_of_track_mut(sel)
                    {
                        *voice = VoiceSpec::Clap {
                            path: path.clone(),
                            plugin_id: plugin_id.clone(),
                        };
                    }
                    self.load_clap_voice(sel, &path, &plugin_id);
                    self.commit_edit("Load CLAP Instrument", before);
                }
                let mut vst3_picked: Option<(String, String)> = None;
                ui.menu_button("VST3 Instruments…", |ui| {
                    let empty: Vec<resoz_vst3_host::Vst3PluginInfo> = Vec::new();
                    let plugins = self.scanned_vst3_instruments();
                    let list = if plugins.is_empty() { &empty } else { plugins };
                    if list.is_empty() {
                        ui.weak("(no VST3 plugins found; set RESOZ_VST3_PATH to add dirs)");
                    }
                    for info in list {
                        if ui.button(&info.name).clicked() {
                            vst3_picked = Some((info.path.display().to_string(), info.cid.clone()));
                            ui.close();
                        }
                    }
                });
                if let Some((path, class_id)) = vst3_picked {
                    let before = self.begin_edit();
                    if let Some(NodeDoc::Instrument { voice, .. }) =
                        self.project_mut().graph.instrument_of_track_mut(sel)
                    {
                        *voice = VoiceSpec::Vst3 {
                            path: path.clone(),
                            class_id: class_id.clone(),
                        };

                    }
                    self.load_vst3_voice(sel, &path, &class_id);
                    self.commit_edit("Load VST3 Instrument", before);
                }
                if is_external {
                    ui.weak("(external instrument plugin)");
                }
                ui.separator();
                ui.label("Effects");
                // Collect the choice first; load after the menu closure ends.
                let mut effect_picked: Option<(String, String)> = None;
                let mut clear_effects = false;
                ui.menu_button("CLAP Effects…", |ui| {
                    if ui.button("Clear All Effects").clicked() {
                        clear_effects = true;
                        ui.close();
                    }
                    let empty: Vec<resoz_clap_host::ClapPluginInfo> = Vec::new();
                    let plugins = self.scanned_effects();
                    let list = if plugins.is_empty() { &empty } else { plugins };
                    if list.is_empty() {
                        ui.weak("(no effect plugins found)");
                    }
                    for info in list {
                        if ui.button(&info.name).clicked() {
                            effect_picked =
                                Some((info.path.display().to_string(), info.id.clone()));
                            ui.close();
                        }
                    }
                });
                if clear_effects {
                    self.clear_external_effects(sel);
                }
                if let Some((path, plugin_id)) = effect_picked {
                    self.load_clap_effect(sel, &path, &plugin_id);
                }
                let mut vst3_effect_picked: Option<(String, String)> = None;
                ui.menu_button("VST3 Effects…", |ui| {
                    let empty: Vec<resoz_vst3_host::Vst3PluginInfo> = Vec::new();
                    let plugins = self.scanned_vst3_effects();
                    let list = if plugins.is_empty() { &empty } else { plugins };
                    if list.is_empty() {
                        ui.weak("(no VST3 effect plugins found)");
                    }
                    for info in list {
                        if ui.button(&info.name).clicked() {
                            vst3_effect_picked =
                                Some((info.path.display().to_string(), info.cid.clone()));
                            ui.close();
                        }
                    }
                });
                if let Some((path, class_id)) = vst3_effect_picked {
                    self.load_vst3_effect(sel, &path, &class_id);
                }
            });

            // VST3 plugin parameter list (read on the control side at instantiation; displays
            // default normalized values).
            if let Some(params) = self.voice_params.get(&sel) {
                if !params.is_empty() {
                    egui::CollapsingHeader::new(format!("Plugin Params ({})", params.len()))
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::Grid::new("vst3_params")
                                .num_columns(3)
                                .spacing([16.0, 4.0])
                                .show(ui, |ui| {
                                    for param in params {
                                        ui.label(&param.title);
                                        ui.weak(format!("id {}", param.id));
                                        ui.weak(format!(
                                            "Default {:.3}{}",
                                            param.default_normalized,
                                            if param.can_automate {
                                                ""
                                            } else {
                                                "(not automatable)"
                                            }
                                        ));
                                        ui.end_row();
                                    }
                                });
                        });
                }
            }

            // CLAP plugin parameter list (enumerated via the params extension at instantiation).
            if let Some(params) = self.clap_voice_params.get(&sel) {
                if !params.is_empty() {
                    egui::CollapsingHeader::new(format!("Plugin Params CLAP ({})", params.len()))
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::Grid::new("clap_params")
                                .num_columns(3)
                                .spacing([16.0, 4.0])
                                .show(ui, |ui| {
                                    for param in params {
                                        ui.label(&param.name);
                                        ui.weak(format!("id {}", param.id));
                                        ui.weak(format!(
                                            "Default {:.3} [{:.3}..{:.3}]",
                                            param.default_value, param.min_value, param.max_value
                                        ));
                                        ui.end_row();
                                    }
                                });
                        });
                }
            }
            ui.weak("Node graph: instrument -> delay -> mixer channels -> master (doc-driven, changes go live)");
            ui.add_space(8.0);

            // Filter / envelope (bound to the selected track's document node fields).
            egui::Grid::new("synth_params")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Cutoff Freq");
                    let mut cutoff = self.synth_of_track(sel).unwrap_or_default().cutoff_hz;
                    let r = ui.add(
                        egui::Slider::new(&mut cutoff, CUTOFF_RANGE)
                            .logarithmic(true)
                            .text("Hz"),
                    );
                    if r.changed() {
                        let before = self.begin_param_edit();
                        if let Some(s) = self.synth_mut(sel) {
                            s.cutoff_hz = cutoff;
                        }
                        self.commit_param_edit("Filter Cutoff", before, "synth-cutoff");
                        self.send_track_param(sel, ParamId::FilterCutoff, cutoff);
                    }
                    ui.end_row();

                    ui.label("Resonance");
                    let mut resonance = self.synth_of_track(sel).unwrap_or_default().resonance;
                    let r = ui.add(egui::Slider::new(&mut resonance, RES_RANGE));
                    if r.changed() {
                        let before = self.begin_param_edit();
                        if let Some(s) = self.synth_mut(sel) {
                            s.resonance = resonance;
                        }
                        self.commit_param_edit("Filter Resonance", before, "synth-res");
                        self.send_track_param(sel, ParamId::FilterResonance, resonance);
                    }
                    ui.end_row();

                    ui.label("Filter Mode");
                    ui.horizontal(|ui| {
                        let mode = self.synth_of_track(sel).unwrap_or_default().filter_mode;
                        for (name, value) in [("Low-pass", 0.0), ("High-pass", 1.0), ("Band-pass", 2.0)] {
                            if ui
                                .selectable_label((mode - value).abs() < 0.5, name)
                                .clicked()
                            {
                                let before = self.begin_edit();
                                if let Some(s) = self.synth_mut(sel) {
                                    s.filter_mode = value;
                                }
                                self.commit_edit("Filter Mode", before);
                                self.send_track_param(sel, ParamId::FilterMode, value);
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("Volume Envelope");
                    self.adsr_sliders(ui);
                    ui.end_row();

                    ui.label("Delay");
                    self.delay_sliders(ui);
                    ui.end_row();

                    ui.label("Master Gain");
                    let before = self.begin_param_edit();
                    let r = ui.add(
                        egui::Slider::new(&mut self.project_mut().master_gain, 0.0..=1.5).text("Linear"),
                    );
                    if r.changed() {
                        self.commit_param_edit("Master Gain", before, "master-gain");
                        self.engine.send(EngineCommand::SetParam {
                            id: ParamId::MasterGain,
                            value: self.project().master_gain,
                        });
                    }
                    if r.drag_stopped() {
                        self.end_param_edit();
                    }
                    ui.end_row();
                });

            ui.separator();

            self.mixer_section(ui);
            self.clip_section(ui);
            self.automation_section(ui);
            self.keyboard_section(ui);

            ui.add_space(10.0);
            ui.label("Master Out Level");
            ui.add(
                egui::ProgressBar::new(peak.clamp(0.0, 1.0))
                    .desired_height(16.0)
                    .show_percentage(),
            );
            ui.weak(format!("Peak {peak:.3} (AtomicF32 feedback, 600 ms hold)"));
            });
        });
    }

    /// ADSR four sliders (local copies + write-back, avoiding borrow conflicts).
    fn adsr_sliders(&mut self, ui: &mut egui::Ui) {
        let sel = self.selected_track;
        let synth = self.synth_of_track(sel).unwrap_or_default();
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
                    self.send_track_param(sel, id, *value);
                }
            }
        });
        let before = self.begin_param_edit();
        if let Some(synth) = self.synth_mut(sel) {
            synth.attack_s = a;
            synth.decay_s = d;
            synth.sustain = s;
            synth.release_s = r;
        }
        self.commit_param_edit("Envelope", before, "synth-adsr");
    }

    /// Delay three sliders (wet mix / time / feedback).
    fn delay_sliders(&mut self, ui: &mut egui::Ui) {
        let sel = self.selected_track;
        let delay = self.delay_of_track(sel).unwrap_or_default();
        let mut mix = delay.mix;
        let mut time = delay.time_s;
        let mut feedback = delay.feedback;
        ui.horizontal(|ui| {
            let sliders = [
                ("Wet", &mut mix, DELAY_MIX_RANGE, ParamId::DelayMix),
                ("Time", &mut time, DELAY_TIME_RANGE, ParamId::DelayTime),
                (
                    "Feedback",
                    &mut feedback,
                    DELAY_FB_RANGE,
                    ParamId::DelayFeedback,
                ),
            ];
            for (label, value, range, id) in sliders {
                let resp = ui.add_sized([104.0, 20.0], egui::Slider::new(value, range).text(label));
                if resp.changed() {
                    self.send_track_param(sel, id, *value);
                }
            }
        });
        let before = self.begin_param_edit();
        if let Some(delay) = self.delay_mut(sel) {
            delay.mix = mix;
            delay.time_s = time;
            delay.feedback = feedback;
        }
        self.commit_param_edit("Delay Params", before, "delay-params");
    }

    /// Send one track parameter to the audio thread.
    fn send_track_param(&mut self, track: usize, id: ParamId, value: f32) {
        self.engine
            .send(EngineCommand::SetTrackParam { track, id, value });
    }

    /// Mixer console (P5f D15/D21, stage-console metaphor): one column per
    /// mixer channel — solo / mute / level meter / fader / pan — plus channel
    /// add/remove buttons (2..=32, D21) and the output pan/mute stage. The
    /// channel a track feeds is labeled with the track name; data source =
    /// the first (global) mixer document node.
    fn mixer_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.add_space(4.0);
        let can_add_track = self.project().midi_tracks.len() < MAX_MIDI_TRACKS;
        let can_remove_track = !self.project().midi_tracks.is_empty();
        let mut add_track_clicked = false;
        let mut remove_track_clicked = false;
        ui.horizontal(|ui| {
            ui.label("Mixer");
            if ui
                .add_enabled(can_add_track, egui::Button::new("+ Track"))
                .clicked()
            {
                add_track_clicked = true;
            }
            if ui
                .add_enabled(can_remove_track, egui::Button::new("- Track"))
                .clicked()
            {
                remove_track_clicked = true;
            }
        });
        if add_track_clicked {
            self.add_midi_track();
        }
        if remove_track_clicked {
            self.remove_last_midi_track();
        }

        let Some(mixer_doc) = self
            .project()
            .graph
            .nodes
            .iter()
            .find(|n| n.is_mixer())
            .map(NodeDoc::node_id)
        else {
            ui.weak("(no mixer node — add one in the Patch view)");
            return;
        };
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

        ui.horizontal(|ui| {
            for (ch_idx, ch) in channels.iter().enumerate() {
                ui.vertical(|ui| {
                    let name = match channel_track.get(ch_idx).copied().flatten() {
                        Some(t) if self.project().midi_tracks[t].name.is_empty() => {
                            format!("Ch {} · T{}", ch_idx + 1, t + 1)
                        }
                        Some(t) => {
                            format!("Ch {} · {}", ch_idx + 1, self.project().midi_tracks[t].name)
                        }
                        None => format!("Ch {}", ch_idx + 1),
                    };
                    ui.weak(name);

                    // Solo / mute row.
                    ui.horizontal(|ui| {
                        let mut solo_btn = egui::Button::new(if ch.solo { "S" } else { "s" });
                        if ch.solo {
                            solo_btn = solo_btn.fill(egui::Color32::from_rgb(200, 170, 60));
                        }
                        if ui.add_sized([32.0, 20.0], solo_btn).clicked() {
                            solo_edits[ch_idx] = Some(!ch.solo);
                        }
                        let mut mute_btn = egui::Button::new(if ch.muted { "M" } else { "m" });
                        if ch.muted {
                            mute_btn = mute_btn.fill(egui::Color32::from_rgb(140, 60, 50));
                        }
                        if ui.add_sized([32.0, 20.0], mute_btn).clicked() {
                            mute_edits[ch_idx] = Some(!ch.muted);
                        }
                    });

                    // Post-fader level bar (metering point = the feeding track's chain end).
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(96.0, 10.0), egui::Sense::hover());
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
                    ui.painter()
                        .rect_filled(filled, 2.0, egui::Color32::from_rgb(70, 160, 90));

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

            // ---- Channel add / remove (D21) ----
            ui.separator();
            ui.vertical(|ui| {
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
        });

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
    fn mixer_add_channel(&mut self, mixer_doc: u64) {
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
    fn mixer_remove_channel(&mut self, mixer_doc: u64) {
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
        let before = self.begin_edit();
        if let Some(NodeDoc::Mixer {
            inputs, channels, ..
        }) = self.project_mut().graph.node_by_id_mut(mixer_doc)
        {
            *inputs = last;
            channels.pop();
        }
        self.commit_edit("Remove Mixer Channel", before);
    }

    /// 新增一条 MIDI 轨：文档两件套节点（voice + delay，P5f D18 不再有
    /// strip）+ 连线（voice→delay + delay→mixer.ch[i] + MIDI 自连）+
    /// 引擎默认链（`add_midi_track`）+ node_map 映射 + 活跃轨数下发。
    /// mixer 通道满员时自动扩容（D21 上限 32）。
    fn add_midi_track(&mut self) {
        let track = self.project().midi_tracks.len();
        if track >= MAX_MIDI_TRACKS {
            return;
        }
        let Some([voice_e, delay_e]) = self.engine.add_midi_track() else {
            self.set_message("Add track failed (engine slots or queue full)".to_string());
            return;
        };
        // One undo step for the whole operation: the mixer may grow inside
        // (mixer_add_channel is itself undoable), and its sub-step must not
        // split the history.
        let before = self.begin_edit();
        self.session.suspend_history();
        // 文档侧新 id：从现有最大 node_id 之后分配（1 = 主输出保留）。
        let next = self
            .project()
            .graph
            .nodes
            .iter()
            .map(|n| n.node_id())
            .max()
            .unwrap_or(1)
            + 1;
        let (voice_id, delay_id) = (next, next + 1);
        self.project_mut()
            .midi_tracks
            .push(resoz_model::MidiTrack::default());
        self.project_mut().graph.nodes.push(NodeDoc::Instrument {
            node_id: voice_id,
            track,
            voice: VoiceSpec::Builtin,
            synth: SynthParams::default(),
        });
        self.project_mut().graph.nodes.push(NodeDoc::Effect {
            node_id: delay_id,
            track,
            voice: VoiceSpec::Builtin,
            delay: DelayParams::default(),
        });
        self.project_mut()
            .graph
            .cables
            .push(resoz_model::CableDoc::audio(voice_id, delay_id));
        self.project_mut()
            .graph
            .cables
            .push(resoz_model::CableDoc::midi(track, voice_id));
        // D18: wire the chain end into the global mixer's first free channel
        // (growing the mixer when it is full).
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
                    (self.project().graph.first_free_channel(mixer_doc)).unwrap_or(u8::MAX)
                }
            };
            if ch != u8::MAX {
                self.project_mut()
                    .graph
                    .cables
                    .push(resoz_model::CableDoc::audio_to_channel(
                        delay_id,
                        PortRef::Node(mixer_doc),
                        ch,
                    ));
                if let Some(mixer_engine) = self.engine_id(mixer_doc) {
                    self.engine.connect_nodes(delay_e, mixer_engine, ch);
                }
            }
        }
        self.node_map.insert(voice_id, voice_e);
        self.node_map.insert(delay_id, delay_e);
        self.engine
            .set_midi_track_count(self.project().midi_tracks.len());
        self.selected_track = track;
        self.push_track_params(track);
        self.session.resume_history();
        self.commit_edit("Add MIDI Track", before);
    }

    /// 删除最后一条 MIDI 轨（中间轨删除需重排轨道表与文档 track 字段，
    /// 随 T3 编曲视图一起做）。引擎逐节点 bypass 移除 + 轨道表清理 +
    /// 活跃轨数收缩；文档删节点与触及连线（含 MIDI 自连）。mixer 节点
    /// （track 0）不属于任何轨，必须排除（P5f D15）。
    fn remove_last_midi_track(&mut self) {
        let Some(last) = self.project().midi_tracks.len().checked_sub(1) else {
            return;
        };
        let before = self.begin_edit();
        let doc_ids: Vec<u64> = self
            .project()
            .graph
            .nodes
            .iter()
            .filter(|n| n.track() == last && !n.is_mixer())
            .map(|n| n.node_id())
            .collect();
        for doc_id in &doc_ids {
            if let Some(&engine_id) = self.node_map.get(doc_id) {
                if !self.engine.remove_node(engine_id) {
                    log::warn!("Track node {engine_id} removal failed");
                }
            }
            self.node_map.remove(doc_id);
        }
        if !self.engine.remove_midi_track(last) {
            log::warn!("engine remove_midi_track({last}) failed");
        }
        self.engine.set_midi_track_count(last);
        // 文档：删节点 + 删触及连线（含 MidiTrack(last) 自连；外部连到
        // 本轨节点的扇出线因触及被删节点一并移除）。
        let removed: HashSet<u64> = doc_ids.into_iter().collect();
        self.project_mut().graph.nodes.retain(|n| n.track() != last);
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
        self.commit_edit("Remove MIDI Track", before);
    }

    /// Piano roll hint (the editor itself lives in a floating window opened
    /// by double-clicking a clip on the arrangement timeline).
    fn clip_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.add_space(4.0);
        ui.weak(
            "Piano roll: double-click a MIDI clip in the Arrangement timeline \
             (bottom panel) to open its editor window.",
        );
    }

    /// Body of the floating piano-roll window: edits the targeted clip,
    /// pushes edits to the audio thread immediately.
    fn midi_editor_body(&mut self, ui: &mut egui::Ui, track: usize, clip_index: usize) {
        let sel = track.min(self.project().midi_tracks.len().saturating_sub(1));
        if self.project().midi_tracks[sel].clips.is_empty() {
            let before = self.begin_edit();
            self.project_mut().midi_tracks[sel]
                .clips
                .push(MidiClip::new(0));
            self.commit_edit("New Clip", before);
            self.send_track_clips(sel);
        }
        let clip_count = self.project().midi_tracks[sel].clips.len();
        let clip_index = if clip_index < clip_count {
            clip_index
        } else {
            0
        };
        let track_name = if self.project().midi_tracks[sel].name.is_empty() {
            format!("Track {}", sel + 1)
        } else {
            self.project().midi_tracks[sel].name.clone()
        };
        ui.horizontal(|ui| {
            ui.label(format!(
                "{track_name} · clip {} (of {clip_count})",
                clip_index + 1
            ));
            if ui.button("× Close editor").clicked() {
                self.edit_clip = None;
            }
        });
        let playhead = self.engine.musical_ticks();
        let playing = self.engine.is_playing();
        let intent = {
            // Destructured: `project()` borrows all of `self`, which would
            // collide with the `&mut self.roll_state` below.
            let Self {
                session,
                roll_state,
                ..
            } = self;
            let clip = &session.project().midi_tracks[sel].clips[clip_index];
            piano_roll::show(ui, clip, roll_state, playhead, playing)
        };
        // Intents run as commands, so every roll edit is undoable. A drag
        // emits one intent per frame under the same gesture key: one drag,
        // one undo step.
        if let Some(intent) = intent {
            self.apply_roll_intent(sel, clip_index, intent);
        }
    }

    /// Runs a piano-roll edit as a command (M1.4: no direct document mutation).
    fn apply_roll_intent(&mut self, track: usize, clip: usize, intent: piano_roll::RollIntent) {
        use piano_roll::RollIntent;
        use serde_json::json;

        let result = match intent {
            RollIntent::AddNote {
                note,
                start,
                length,
            } => {
                let outcome = self.execute(
                    "midi.note_add",
                    &json!({
                        "track": track, "clip": clip,
                        "note": note, "start_ticks": start, "length_ticks": length,
                    }),
                );
                // The new note's sorted index becomes the drag target.
                if let Ok(value) = outcome {
                    if let Some(index) = value["index"].as_u64() {
                        self.roll_state.set_drag_target(index as usize, 0.0);
                    }
                }
                return;
            }
            RollIntent::EditNote { index, note, start } => {
                // Merged: consecutive frames of one drag collapse into a single
                // undo step that restores the note as it was before the drag.
                self.execute_merged(
                    "midi.note_edit",
                    &json!({
                        "track": track, "clip": clip, "index": index,
                        "note": note, "start_ticks": start,
                    }),
                    &format!("roll:{track}:{clip}:{index}"),
                )
            }
            RollIntent::DeleteNotes { indices } => self.execute(
                "midi.note_delete",
                &json!({ "track": track, "clip": clip, "indices": indices }),
            ),
        };
        if result.is_ok() {
            self.send_track_clips(track);
        } else if let Err(error) = result {
            self.set_message(format!("Edit failed: {error}"));
        }
    }

    /// Automation track section (D12 top-level entities; each curve's delivery endpoint is
    /// decided by node-graph cables).
    fn automation_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.add_space(4.0);
        let auto_count = self.project().control_tracks.len();
        ui.horizontal(|ui| {
            ui.label("Automation Tracks");
            for i in 0..auto_count {
                if ui
                    .selectable_label(self.selected_auto == i, format!("{}", i + 1))
                    .clicked()
                {
                    self.selected_auto = i;
                }
            }
            if ui.button("+ Add").clicked() && auto_count < MAX_AUTO_TRACKS {
                let before = self.begin_edit();
                self.project_mut()
                    .control_tracks
                    .push(ControlTrack::default());
                self.commit_edit("Add Automation Track", before);
                self.selected_auto = auto_count;
                self.send_control_clips(self.selected_auto);
            }
            if ui.button("- Remove").clicked() && auto_count > 0 {
                let index = self.selected_auto.min(auto_count - 1);
                let before = self.begin_edit();
                self.project_mut().control_tracks.remove(index);
                self.commit_edit("Remove Automation Track", before);
                let _ = self.engine.clear_auto_route(index);
                // Engine side: clear that index to an empty clip table.
                self.send_sequencer(SequencerCommand::SetControlClips {
                    index,
                    clips: [RtControlClip::default(); MAX_TRACK_CLIPS],
                });
                self.selected_auto =
                    index.min(self.project().control_tracks.len().saturating_sub(1));
            }
            ui.weak("Click lane to add anchor · drag to move · right-click to delete");
        });

        if auto_count == 0 {
            ui.weak("(no automation tracks; click '+ Add' to create one)");
            return;
        }
        let index = self.selected_auto.min(auto_count - 1);
        let sel = self.selected_track;

        // Endpoint selection: this track's instrument (cutoff / plugin
        // parameters). Mixer-fader automation is deferred (P5f decision).
        // Options carry the **document** node_id (the cable's persisted identity), mapped when
        // dispatching.
        let mut options: Vec<(String, u64, AutoParam)> = Vec::new();
        if let Some(voice_doc) = self
            .project()
            .graph
            .instrument_of_track(sel)
            .map(NodeDoc::node_id)
        {
            options.push(("Cutoff".to_string(), voice_doc, AutoParam::FilterCutoff));
            if let Some(params) = self.clap_voice_params.get(&sel) {
                for param in params {
                    let label = if param.name.is_empty() {
                        format!("Param {}", param.id)
                    } else {
                        param.name.clone()
                    };
                    options.push((
                        format!("CLAP {label}"),
                        voice_doc,
                        AutoParam::PluginParam {
                            param_id: param.id,
                            name: label,
                        },
                    ));
                }
            }
            if let Some(params) = self.voice_params.get(&sel) {
                for param in params {
                    let label = if param.title.is_empty() {
                        format!("Param {}", param.id)
                    } else {
                        param.title.clone()
                    };
                    options.push((
                        format!("VST3 {label}"),
                        voice_doc,
                        AutoParam::PluginParam {
                            param_id: param.id,
                            name: label,
                        },
                    ));
                }
            }
        }

        let mut picked: Option<(u64, AutoParam)> = None;
        ui.horizontal(|ui| {
            ui.label("Target");
            ui.menu_button("Pick Parameter…", |ui| {
                for (label, doc_id, param) in &options {
                    if ui.button(label).clicked() {
                        picked = Some((*doc_id, param.clone()));
                        ui.close();
                    }
                }
            });
            if let Some((doc_id, param)) = picked {
                self.set_auto_endpoint(index, doc_id, param);
            }
            // Current endpoint display.
            let current = self
                .project()
                .graph
                .auto_endpoint(index)
                .map(|param| match &param {
                    AutoParam::TrackGain => "Volume".to_string(),
                    AutoParam::TrackPan => "Pan".to_string(),
                    AutoParam::FilterCutoff => "Cutoff".to_string(),
                    AutoParam::PluginParam { name, .. } => name.clone(),
                })
                .unwrap_or_else(|| "Not connected".to_string());
            ui.weak(format!("-> {current}"));
        });

        // The curve editor lives in a floating window (double-click a control
        // clip on the arrangement timeline); the clip's own `loop_ticks` sets
        // the editing width.
        ui.weak(
            "Curve editor: double-click a control clip in the Arrangement \
             timeline (bottom panel) to open its editor window.",
        );
    }

    /// Body of the floating curve-editor window: edits the targeted control
    /// clip, pushes edits to the audio thread immediately.
    fn control_editor_body(&mut self, ui: &mut egui::Ui, index: usize, clip_index: usize) {
        let playhead = self.engine.musical_ticks();
        let playing = self.engine.is_playing();
        let axis_param = self
            .project_mut()
            .graph
            .auto_endpoint(index)
            .unwrap_or_default();
        if self
            .project()
            .control_tracks
            .get(index)
            .is_none_or(|t| t.clips.is_empty())
        {
            let before = self.begin_edit();
            self.project_mut().control_tracks[index]
                .clips
                .push(resoz_model::ControlClip::new(0));
            self.commit_edit("New Clip", before);
            self.send_control_clips(index);
            return;
        }
        let clip_count = self.project().control_tracks[index].clips.len();
        let clip_index = clip_index.min(clip_count - 1);
        let intent = {
            // Destructured: `project()` borrows all of `self`, which would
            // collide with the `&mut self.lane_state` below.
            let Self {
                session,
                lane_state,
                ..
            } = self;
            let clip = &session.project().control_tracks[index].clips[clip_index];
            lane::show(
                ui,
                clip,
                &axis_param,
                clip.loop_ticks,
                playhead,
                playing,
                lane_state,
            )
        };
        let Some(intent) = intent else {
            return;
        };
        // Apply the intent undoably. A drag frame merges into one undo step;
        // the new anchor index is fed back so the drag keeps tracking it.
        let before = self.begin_edit();
        let new_drag = match intent {
            lane::LaneIntent::UpsertPoint { tick, value } => {
                let clip = &mut self.project_mut().control_tracks[index].clips[clip_index];
                clip.upsert_point(resoz_model::AutoPoint { tick, value });
                let at = clip.curve.iter().position(|p| p.tick == tick);
                let dragging = self.lane_state.is_dragging();
                if dragging {
                    self.commit_param_edit(
                        "Edit Anchor",
                        before,
                        &format!("lane:{index}:{clip_index}"),
                    );
                } else {
                    self.commit_edit("Add Anchor", before);
                }
                at
            }
            lane::LaneIntent::RemoveNear { tick } => {
                let clip = &mut self.project_mut().control_tracks[index].clips[clip_index];
                clip.remove_near(tick, lane::SNAP_TICKS / 2);
                self.commit_edit("Delete Anchor", before);
                None
            }
        };
        self.lane_state.set_drag(new_drag);
        self.send_control_clips(index);
    }

    /// Virtual keyboard: on-screen keys (press to sound, release to stop) + octave switching.
    fn keyboard_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.add_space(4.0);
        let sel = self.selected_track;
        ui.horizontal(|ui| {
            ui.label(format!("Keyboard — Track {}", sel + 1));
            if ui.button("− Octave").clicked() {
                self.octave_base = (self.octave_base.saturating_sub(12)).max(24);
            }
            let low = note_name(self.octave_base);
            let high = note_name(self.octave_base + 12);
            ui.monospace(format!("{low} – {high}"));
            if ui.button("+ Octave").clicked() {
                self.octave_base = (self.octave_base + 12).min(84);
            }
            ui.weak("PC keys A W S E D F T G Y H U J K O L P ; (Z / X octave shift)");
        });

        ui.horizontal(|ui| {
            for semi in 0..=12u8 {
                let note = self.octave_base + semi;
                let black = BLACK_SEMIS.contains(&semi);
                let label = note_name(note);
                let held = self.held_notes.contains(&note);
                let text = if held {
                    egui::RichText::new(&label).strong()
                } else if black {
                    egui::RichText::new(&label).color(egui::Color32::WHITE)
                } else {
                    egui::RichText::new(&label)
                };
                let mut button = egui::Button::new(text);
                if held {
                    button = button.fill(egui::Color32::DARK_GREEN);
                } else if black {
                    button = button.fill(egui::Color32::from_gray(40));
                }
                let response = ui.add(button);
                // Edge detection via the held state (0.36 removed pressed/released): press sounds
                // the note, release (including dragging off the button) stops it.
                let down = response.is_pointer_button_down_on();
                if down && !self.mouse_down_notes.contains(&note) {
                    self.mouse_down_notes.insert(note);
                    self.send_note_on(note, 0.9);
                } else if !down && self.mouse_down_notes.contains(&note) {
                    self.mouse_down_notes.remove(&note);
                    self.send_note_off(note);
                }
            }
        });
    }
}

impl eframe::App for ResoZApp {
    // eframe persistence: called on window close and periodically, saving the recent files list.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(
            storage,
            UI_STATE_KEY,
            &UiState {
                recent_files: self.recent.clone(),
            },
        );
    }

    // eframe 0.36: the App trait entry point changed from update(ctx) to ui(&mut Ui);
    // top/bottom panels act directly on the root Ui, and the remaining space is the central
    // region.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            log::info!("UI first frame rendered");
        }

        self.handle_keyboard_input(ui.ctx());

        // For smoke tests: request exit once the auto-close deadline is reached.
        if let Some(after) = self.auto_close_after {
            if !self.close_sent && self.opened_at.elapsed() >= after {
                self.close_sent = true;
                log::info!("Auto-close deadline reached; requesting exit");
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // Window title is resent only when changed (updated on open/save).
        if self.window_title != self.title_sent {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Title(self.window_title.clone()));
            self.title_sent = self.window_title.clone();
        }

        // Recycle unloaded plugin nodes: released on the control thread, and the paired guard in
        // the retiring zone is dropped by node_id (safe unload of the dynamic library; active
        // guards are unaffected).
        for node_id in self.engine.reclaim_nodes() {
            if let Some(pos) = self
                .retiring_guards
                .iter()
                .position(|entry| entry.node_id == node_id)
            {
                self.retiring_guards.remove(pos);
            }
        }

        // Transport stop edge: automation no longer overrides; restore manual parameter values
        // per track.
        let playing = self.engine.is_playing();
        if self.was_playing && !playing {
            for track in 0..self.project().midi_tracks.len() {
                self.push_track_params(track);
            }
        }
        self.was_playing = playing;

        self.transport_panel(ui);
        self.status_panel(ui);
        self.view_tabs(ui);

        // Central content per view. Every view owns the full central area; the
        // tab bar decides which one is shown (fixes the old "patch buried
        // mid-scroll in the synth panel" layout that had no scrollbar).
        match self.view {
            MainView::Timeline => {
                egui::CentralPanel::default().show(ui, |ui| self.timeline_panel(ui));
            }
            MainView::Patch => {
                egui::CentralPanel::default().show(ui, |ui| self.graph_patch(ui));
            }
            MainView::Synth => self.synth_panel(ui),
        }
        self.editor_window(ui.ctx());

        // Audio-driven displays (level meter, playback position, envelope) need periodic
        // repaints; without this, egui sleeps when idle and auto-close (smoke test) never fires.
        ui.ctx().request_repaint_after(Duration::from_millis(50));
    }
}

/// Start the main window (blocks until the window closes). **Must be called on the main thread.**
pub fn run(engine: AudioEngine, config: UiConfig) -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ResoZ")
            .with_inner_size([920.0, 760.0])
            .with_min_inner_size([760.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ResoZ",
        native_options,
        Box::new(move |cc| Ok(Box::new(ResoZApp::new(cc, engine, config)))),
    )
}
