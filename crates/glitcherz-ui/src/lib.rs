// `deny` instead of `forbid`: the one sanctioned unsafe boundary is
// `plugin_window` (Win32 container plumbing, rationale + tests in that
// module; see also AGENTS.md "禁止 unsafe" exceptions).
#![deny(unsafe_code)]

//! # glitcherz-ui
//!
//! GlitcherZ's egui UI: transport controls, arrangement timeline, piano roll,
//! mixer, patch board, and automation lanes.
//!
//! P5c (spec `.agents/spec/node-graph.md` D13): the engine graph is
//! **rebuilt purely from the project document** — `GlitcherZApp` owns the
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
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use glitcherz_clap_host::{ClapInstance, ClapInstanceConfig};
use glitcherz_engine::AudioEngine;
use glitcherz_engine::Sf2Error;
use glitcherz_engine::Sf2Library;
use glitcherz_engine::convert;
use glitcherz_engine::realtime::{
    ChannelStripProcessor, DelayProcessor, EngineCommand, FeedbackProcessor, MAX_AUTO_POINTS,
    MAX_CLIP_NOTES, MAX_MIDI_FANOUT, MAX_TRACK_CLIPS, MixerChannel as EngineMixerChannel,
    MixerProcessor, NodeKind, NoteEvent, ParamId, Processor, RtAutoPoint, RtAutomation,
    RtControlClip, RtMidiClip, RtNote, SequencerCommand, Sf2VoiceProcessor, SynthProcessor,
};
use glitcherz_model::{
    AutoParam, CableKind, ControlClip, ControlTrack, DelayParams, GraphDoc, MAX_AUTO_TRACKS,
    MAX_CLIPS_PER_TRACK, MAX_MARKERS, MAX_MIDI_TRACKS, MAX_MIXER_CHANNELS, MAX_NODES,
    MIN_MIXER_CHANNELS, Marker, MidiClip, MidiSource, MidiTrack, NodeDoc, PortRef, Project,
    SongLoop, SynthParams, VoiceSpec,
};

pub mod event_list;
pub mod focus;
pub mod grid;
pub mod lane;
pub mod patch;
pub mod piano_roll;
pub mod plugin_catalog;
pub mod plugin_picker;
pub mod plugin_window;
pub mod project_files;
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

/// Short row label for one plugin inline control (G5): the node-body label
/// column is narrow, so the display name is truncated to its first few
/// characters; an empty name falls back to the parameter id.
fn inline_plugin_label(name: &str, id: u32) -> String {
    if name.is_empty() {
        format!("p{id}")
    } else {
        name.chars().take(6).collect()
    }
}

/// BPM adjustable range (f64 in the document: time computation convention).
pub const BPM_RANGE: RangeInclusive<f64> = 20.0..=300.0;

/// Meters offered by the transport dropdown (U3a). The document stores any
/// valid numerator/denominator pair; this is just the pick list.
pub const METER_PRESETS: [(u8, u8); 6] = [(4, 4), (3, 4), (2, 4), (5, 4), (6, 8), (7, 8)];

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

/// Automation cable endpoint in the UI's terms: a document node parameter, or
/// the fixed Timeline In terminal (U3b — song tempo, which is a clock rather
/// than a node parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoTarget {
    /// Document node id (mapped to the engine id when dispatching).
    Node(u64),
    /// The fixed conductor terminal (`PortRef::TimelineIn`).
    TimelineIn,
}

/// Document automation parameter → engine parameter ID.
///
/// `None` for [`AutoParam::Tempo`]: tempo never reaches a node's `set_param`
/// path — the engine route uses the Timeline In sentinel instead (U3b D1/D7).
fn param_id_of(param: &AutoParam) -> Option<ParamId> {
    match param {
        AutoParam::TrackGain => Some(ParamId::TrackGain),
        AutoParam::TrackPan => Some(ParamId::TrackPan),
        AutoParam::FilterCutoff => Some(ParamId::FilterCutoff),
        AutoParam::PluginParam { param_id, .. } => Some(ParamId::PluginParam(*param_id)),
        AutoParam::Tempo => None,
    }
}

/// Built-in inline registry entry → engine parameter ID (G5 spec §7: the
/// inline sliders address the same `set_node_param` entries as the panel
/// sliders, so both edit paths stay interchangeable).
fn inline_param_id(p: patch::InlineParam) -> ParamId {
    match p {
        patch::InlineParam::Cutoff => ParamId::FilterCutoff,
        patch::InlineParam::Resonance => ParamId::FilterResonance,
        patch::InlineParam::DelayTime => ParamId::DelayTime,
        patch::InlineParam::DelayFeedback => ParamId::DelayFeedback,
        patch::InlineParam::DelayMix => ParamId::DelayMix,
        patch::InlineParam::MixerOutPan => ParamId::MixerOutPan,
        patch::InlineParam::FeedbackGain => ParamId::FeedbackGain,
    }
}

/// Validates a cable-midpoint splice and returns the endpoints the new node
/// has to join: `(upstream node, downstream port, downstream channel)`.
///
/// A splice turns `A → B` into `A → new → B`, so the new node needs the
/// cable's **upstream node** as its feed and must reproduce the original
/// target — including its channel binding (D17: a mixer input keeps its
/// fader). Kept separate from the app so the acceptance rule ("the splice
/// never changes the downstream slot") is testable without a window.
fn splice_endpoints(cable: &glitcherz_model::CableDoc) -> Result<(u64, PortRef, u8), &'static str> {
    if cable.kind != CableKind::Audio {
        return Err("only audio cables accept inserted nodes");
    }
    let PortRef::Node(from_doc) = cable.from else {
        return Err("cable has no node source");
    };
    Ok((from_doc, cable.to.clone(), cable.in_port))
}

/// Final node source for a plugin picked in the popup (U9 E3).
///
/// The entry's own classification decides instrument vs effect (spec D9): a
/// mixed-up plugin still becomes a node of the kind it actually is, even when
/// the user reached it through the global `Search Plug-ins…` entry.
fn pick_to_source(choice: plugin_picker::PickChoice) -> patch::NodeSource {
    use plugin_catalog::PluginFormat;
    match (choice.is_instrument, choice.format) {
        (true, PluginFormat::Clap) => patch::NodeSource::ClapInstrument {
            path: choice.path,
            plugin_id: choice.id,
        },
        (true, PluginFormat::Vst3) => patch::NodeSource::Vst3Instrument {
            path: choice.path,
            class_id: choice.id,
        },
        (false, PluginFormat::Clap) => patch::NodeSource::ClapEffect {
            path: choice.path,
            plugin_id: choice.id,
        },
        (false, PluginFormat::Vst3) => patch::NodeSource::Vst3Effect {
            path: choice.path,
            class_id: choice.id,
        },
    }
}

// ---------------------------------------------------------------------------
// Host-side preset library
// ---------------------------------------------------------------------------
//
// A preset is a plugin's full state blob (`VoiceSpec.state`, spec S5)
// stored under a name: `presets/<plugin-key>/<name>.glzpreset`, the file
// content being the **raw state bytes** (plugin-private format, no host
// wrapping). The plugin key distinguishes plugin identity (format + path
// hash + id) so presets never leak across plugins.

/// Preset file extension.
const PRESET_EXT: &str = "glzpreset";

/// Root directory of the preset library (relative to the working directory;
/// matches the dev-run layout, `cargo run -p glitcherz-app`).
fn preset_library_root() -> PathBuf {
    PathBuf::from("presets")
}

/// Maps arbitrary text onto a filesystem-safe fragment (length-capped,
/// separator-stripped).
fn sanitize_filename_fragment(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .take(max_chars)
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | ' ' => c,
            _ => '_',
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "plugin".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Preset names are filenames: trimmed, non-empty, no bare dot fragments
/// (remaining separators are mapped to `_` by the fragment sanitizer).
fn sanitize_preset_name(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return None;
    }
    Some(sanitize_filename_fragment(trimmed, 64))
}

/// The preset-library key of a plugin identity: `<format>-<path hash8>-<id>`.
/// The path hash keeps same-named plugins from different installs separate.
fn preset_plugin_key(format_label: &str, path: &str, id: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    format!(
        "{format_label}-{:08x}-{}",
        hasher.finish() as u32,
        sanitize_filename_fragment(id, 24)
    )
}

/// Directory holding one plugin's presets.
fn preset_dir(root: &std::path::Path, plugin_key: &str) -> PathBuf {
    root.join(plugin_key)
}

/// Preset file path (`<name>.glzpreset`).
fn preset_file(root: &std::path::Path, plugin_key: &str, name: &str) -> PathBuf {
    preset_dir(root, plugin_key).join(format!("{name}.{PRESET_EXT}"))
}

/// Lists a plugin's preset names (sorted; unreadable entries skipped).
fn list_voice_presets_in(root: &std::path::Path, plugin_key: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(preset_dir(root, plugin_key)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .map(|x| x == PRESET_EXT)
                .unwrap_or(false)
        })
        .filter_map(|e| {
            e.path()
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    names
}

/// Writes one preset (raw state bytes). The preset name becomes the file name.
fn save_voice_preset_in(
    root: &std::path::Path,
    plugin_key: &str,
    name: &str,
    blob: &[u8],
) -> std::io::Result<PathBuf> {
    let path = preset_file(root, plugin_key, name);
    std::fs::create_dir_all(preset_dir(root, plugin_key))?;
    std::fs::write(&path, blob)?;
    Ok(path)
}

/// Reads one preset's raw state blob.
fn read_voice_preset_in(root: &std::path::Path, plugin_key: &str, name: &str) -> Option<Vec<u8>> {
    std::fs::read(preset_file(root, plugin_key, name)).ok()
}

/// Deletes one preset file.
fn delete_voice_preset_in(
    root: &std::path::Path,
    plugin_key: &str,
    name: &str,
) -> std::io::Result<()> {
    std::fs::remove_file(preset_file(root, plugin_key, name))
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
    /// Recently picked plugins (spec `plugin-browser.md` D10): per category,
    /// newest first. Persisted so the picker's `Recent` group survives restarts.
    #[serde(default)]
    recent_plugins: Vec<plugin_picker::RecentPlugin>,
}

/// Capacity limit of the recent files list.
const RECENT_FILES_MAX: usize = 8;

/// Key for [`UiState`] in eframe storage.
const UI_STATE_KEY: &str = "glitcherz_ui_state";

/// Key for the encoded plugin catalog in eframe storage (spec
/// `plugin-browser.md` D11).
const PLUGIN_CACHE_KEY: &str = "plugin_cache";

/// Status bar message display duration.
const STATUS_MESSAGE_TTL: Duration = Duration::from_secs(8);

/// External plugin instance guard (CLAP / VST3).
#[allow(dead_code)] // guard held only to extend lifetime; the field itself is never read
enum PluginGuard {
    Clap(glitcherz_clap_host::ClapInstanceGuard),
    Vst3(glitcherz_vst3_host::Vst3InstanceGuard),
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

/// Embedded plugin editor session state (spec `plugin-gui.md` §4). The
/// native container is the window the plugin attaches to; its on-screen
/// geometry is refreshed each frame from the egui layout.
struct PluginEditor {
    /// Document node whose instrument editor is open (spec `plugin-gui.md`
    /// §6 — the panel target identity, not the track).
    doc_node: u64,
    /// Engine node id of the instrument (guard identity; retirement closes
    /// the editor when this node goes away).
    node_id: u64,
    /// Desired editor size in egui points (initial = plugin-reported size;
    /// updated by the plugin's request_resize).
    size: (f32, f32),
    /// Last frame's editor rect (egui points, screen coordinates) — where
    /// the container window must be placed. `None` until the synth panel
    /// lays the editor area out (or when the view is away).
    rect: Option<egui::Rect>,
    /// Native container window (dropped = destroyed; only after the plugin
    /// editor itself is closed).
    container: plugin_window::PluginWindowContainer,
    /// VST3 only: a `resizeView` request the UI has accepted but not yet
    /// confirmed with `IPlugView::onSize`; cleared once the container has
    /// actually been repositioned to the new size (the host half of the
    /// resize handshake, spec `plugin-gui.md` §9).
    pending_ack: Option<glitcherz_vst3_host::ViewSize>,
}

/// Parameter values a plugin reported from inside its own editor, waiting
/// to be applied once the guard borrow ends (spec `plugin-gui.md` §5 for
/// CLAP, §9 for VST3).
struct GuiParamBatch {
    /// Document node whose instrument produced the values.
    doc_node: u64,
    /// CLAP raw-value domain (true) vs VST3 normalized domain (false) —
    /// selects the session slider cache the read-back seeds.
    is_clap: bool,
    /// `(param id, value)` pairs in the plugin's own domain.
    changes: Vec<(u32, f64)>,
}

/// Resolves the Synth panel's editing target (spec `plugin-gui.md` §6):
/// the first **editable** document node (instrument / effect / mixer) in the
/// patch selection, in document order; with nothing selected the panel falls
/// back to the selected track's first-match instrument (the pre-G4
/// track-driven behavior). Stale selection ids (deleted nodes) are filtered
/// by the document walk.
fn resolve_panel_target(doc: &GraphDoc, selected: &[u64], track: usize) -> Option<u64> {
    if !selected.is_empty() {
        for node in &doc.nodes {
            if selected.contains(&node.node_id())
                && matches!(
                    node,
                    NodeDoc::Instrument { .. } | NodeDoc::Effect { .. } | NodeDoc::Mixer { .. }
                )
            {
                return Some(node.node_id());
            }
        }
    }
    doc.instrument_of_track(track).map(NodeDoc::node_id)
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
    /// Clip editor (piano roll / curve lane) for the targeted clip;
    /// double-clicking a clip in the timeline jumps here.
    Edit,
    /// Node patch graph (full-height, dedicated view).
    Patch,
    /// Synth params + mixer + clip/automation editors + keyboard.
    Synth,
}

/// A destructive file action that must ask about unsaved changes first (U2).
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileAction {
    /// Replace the document with a fresh default project.
    New,
    /// Load this project file.
    Open(PathBuf),
    /// Quit the application.
    Exit,
}

impl FileAction {
    /// Verb used in the unsaved-changes prompt.
    fn verb(&self) -> &'static str {
        match self {
            Self::New => "start a new project",
            Self::Open(_) => "open another project",
            Self::Exit => "quit",
        }
    }
}

/// Answer to the unsaved-changes modal (U2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmChoice {
    Save,
    Discard,
    Cancel,
}

/// Main window app. Owns the [`AudioEngine`]: closing the window drops the engine and stops the
/// audio stream.
pub struct GlitcherZApp {
    engine: AudioEngine,
    /// Project document + undo history (the UI owns model state; see
    /// "Threading Model" in AGENTS.md). Mutations go through
    /// [`Session::execute`] so they are undoable; `project_mut` is the escape
    /// hatch for the many places not yet moved to commands.
    session: glitcherz_command::Session,
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
    /// Hover-follow keyboard focus arbiter (Blender-style context; see
    /// `focus.rs`). Keyboards surfaces register each frame; the decision is
    /// applied at the start of the next frame.
    focus_router: focus::FocusRouter,
    /// Automation lane cross-frame state (drag).
    lane_state: lane::LaneState,
    /// Node patch cross-frame state (pan/zoom/selection/drag).
    patch_state: patch::PatchState,
    /// Active main view (tab bar).
    view: MainView,
    /// Octave base (MIDI number of C, default 60 = C4).
    octave_base: u8,
    /// Live keyboard destinations (G6, spec `plugin-gui.md` §8): every
    /// instrument reachable from the **computer-keyboard input node's** cables,
    /// in document order. Empty = the input is unbound or unconnected, and the
    /// keyboard is silent — there is no track fallback. Recomputed once per
    /// frame from the document (not from the patch selection).
    ///
    /// One node may cable several instruments (one out jack, many cables, the
    /// MIDI multi-out / single-in rule), so a key press reaches all of them —
    /// cheap synth layering without a mixer.
    vk_targets: Vec<u64>,
    /// Currently held notes (shared by on-screen keys and computer keys, for highlighting and
    /// de-duplication).
    held_notes: HashSet<u8>,
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
    /// Destructive file action parked behind the unsaved-changes modal (U2).
    /// `None` = no dialog.
    pending_file_action: Option<FileAction>,
    /// The window close has been approved: let the viewport request through
    /// instead of prompting again (U2).
    allow_close: bool,
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
    /// Plugin catalog: startup scan state plus the entry table both plugin
    /// pickers read (spec `.agents/spec/plugin-browser.md`, U9 E2). Replaces
    /// the four lazy per-format scan caches.
    plugin_catalog: plugin_catalog::PluginCatalog,
    /// Filterable plugin popup behind `Add ▸ Instrument ▸ CLAP…` (U9 E3).
    picker: plugin_picker::PluginPicker,
    /// Where the picker's pick lands: a new dangling node, or spliced into an
    /// existing audio cable (taken from the click that opened the menu).
    picker_place: Option<patch::PickerPlace>,
    /// Recently picked plugins, newest first, persisted in [`UiState`].
    recent_plugins: Vec<plugin_picker::RecentPlugin>,
    /// Instrument VST3 parameter lists (read at instantiation), keyed by
    /// **document node_id** (G4: node-addressed like the effect caches, so
    /// multi-instrument tracks keep independent panels). For
    /// diagnostics/display only; actual parameter control still goes through
    /// the `PluginParam` bridge.
    voice_params: HashMap<u64, Vec<glitcherz_vst3_host::Vst3ParamInfo>>,
    /// Instrument CLAP parameter lists (read at instantiation), keyed by
    /// document node_id. Data source for automation cable targets; curve
    /// value semantics = raw plugin-range values.
    clap_voice_params: HashMap<u64, Vec<glitcherz_clap_host::ClapParamInfo>>,
    /// Current CLAP instrument parameter values (raw plugin range, keyed by
    /// document node_id → param id) — the slider cache. UI-session only; a
    /// fresh plugin load resets them to the plugin defaults.
    clap_voice_values: HashMap<u64, HashMap<u32, f64>>,
    /// Current VST3 instrument parameter values (normalized 0..1, keyed by
    /// document node_id → param id) — the slider cache; seeded from the
    /// controller read-back (`read_param_values`) at load time.
    vst3_voice_values: HashMap<u64, HashMap<u32, f64>>,
    /// External effect parameter lists, keyed by **document node_id** (the
    /// stable identity across engine rebuilds; delivery resolves the engine
    /// id via `node_map`). CLAP = raw plugin range, VST3 = normalized 0..1.
    clap_effect_params: HashMap<u64, Vec<glitcherz_clap_host::ClapParamInfo>>,
    /// CLAP effect current raw values (doc node_id → param id), UI session only.
    clap_effect_values: HashMap<u64, HashMap<u32, f64>>,
    /// VST3 external effect parameter lists (doc node_id-keyed, normalized).
    vst3_effect_params: HashMap<u64, Vec<glitcherz_vst3_host::Vst3ParamInfo>>,
    /// VST3 effect current normalized values (doc node_id → param id), UI session only.
    vst3_effect_values: HashMap<u64, HashMap<u32, f64>>,
    /// Instrument param panel filter (CLAP; matched against module + name).
    clap_param_filter: String,
    /// Instrument param panel filter (VST3; matched against title).
    vst3_param_filter: String,
    /// Automation target picker filter (matched against the option label).
    auto_param_filter: String,
    /// Captured factory-default state blobs (document node_id → blob),
    /// taken at spawn time before any document state restore — the preset
    /// "Reset" target.
    voice_default_state: HashMap<u64, Vec<u8>>,
    /// Selected preset name per instrument node (the ComboBox display +
    /// Delete target).
    voice_preset_selected: HashMap<u64, String>,
    /// "Save preset" name input.
    preset_name_input: String,
    /// Shared session SoundFont (v1: one library per session, spec
    /// `sf2-loader.md` §6). Loaded via the library panel; document restores
    /// reuse it when the path matches to avoid re-reading a large file.
    sf2_library: Option<Arc<Sf2Library>>,
    /// Embedded plugin editor state (spec `plugin-gui.md` §4): `Some` while
    /// a CLAP instrument editor is open for a document node. Closed when the
    /// guard retires, the panel target changes, or the user toggles it off.
    plugin_editor: Option<PluginEditor>,
    /// Frame-queued request from the synth panel (it has no eframe frame to
    /// create the container with); processed at the end of `ui()`.
    plugin_editor_wants_open: bool,
    /// Session-memory toggle (spec `plugin-gui.md` §6): the user wants an
    /// editor open for the current panel target. Survives target switches
    /// (the editor reopens on the new node); cleared only by an explicit
    /// close. Not persisted.
    editor_user_open: bool,
    /// Diagnostic flag: whether `ui()` has been entered (logging only).
    first_frame_logged: bool,
    /// Diagnostic flag: whether the close command has been sent (logging only).
    close_sent: bool,
}

/// One track's clip table view, for relocate_in.
type ClipTables<'a, T> = &'a mut [&'a mut Vec<T>];

/// Moves clips across the clip tables of one track kind (M4b.3 G modal).
/// Per source table the indices are removed descending, then re-appended
/// at their destinations with the new start — appending cannot shift any
/// remaining index. `moves`: (from_table, from_index, to_table, new_start).
/// Returns the touched table indices. Destinations beyond
/// MAX_CLIPS_PER_TRACK silently drop the clip.
type MoveSpec = (usize, usize, usize, u32);

fn relocate_in<T: timeline::Placement>(
    tables: ClipTables<'_, T>,
    moves: &[MoveSpec],
) -> Vec<usize> {
    // (from_table, [(from_index, to_table, new_start)])
    #[allow(clippy::type_complexity)]
    let mut by_from: Vec<(usize, Vec<(usize, usize, u32)>)> = Vec::new();
    for &(from, index, to, start) in moves {
        match by_from.iter_mut().find(|(k, _)| *k == from) {
            Some((_, v)) => v.push((index, to, start)),
            None => by_from.push((from, vec![(index, to, start)])),
        }
    }
    let mut touched = Vec::new();
    for (from, mut items) in by_from {
        items.sort_unstable_by_key(|(index, _, _)| std::cmp::Reverse(*index));
        let mut taken: Vec<(usize, T)> = Vec::new();
        {
            let src: &mut Vec<T> = tables[from];
            for (index, to, start) in items {
                if index < src.len() {
                    let mut c = src.remove(index);
                    c.set_start(start);
                    taken.push((to, c));
                    if !touched.contains(&from) {
                        touched.push(from);
                    }
                }
            }
        }
        for (to, c) in taken {
            if !touched.contains(&to) {
                touched.push(to);
            }
            if tables[to].len() < MAX_CLIPS_PER_TRACK {
                tables[to].push(c);
            }
        }
    }
    touched
}

impl GlitcherZApp {
    /// Build the window app, restore persisted recent files, and push the whole project document
    /// down via incremental graph building + the command queue (takes effect at the next audio
    /// block boundary).
    pub fn new(cc: &eframe::CreationContext<'_>, engine: AudioEngine, config: UiConfig) -> Self {
        let ui_state: UiState = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, UI_STATE_KEY))
            .unwrap_or_default();
        // U9 E2: the plugin catalog restores from storage and plans the
        // startup scan (a warm cache usually leaves nothing to do).
        let cached_plugins = cc
            .storage
            .and_then(|storage| storage.get_string(PLUGIN_CACHE_KEY));
        let mut app = Self {
            engine,
            session: glitcherz_command::Session::new(Project {
                master_gain: config.gain.clamp(0.0, 1.5) as f32,
                ..Project::default()
            }),
            node_map: HashMap::new(),
            selected_track: 0,
            selected_auto: 0,
            timeline_state: timeline::TimelineState::default(),
            edit_clip: None,
            roll_state: piano_roll::RollState::new(),
            focus_router: focus::FocusRouter::new(),
            lane_state: lane::LaneState::default(),
            patch_state: patch::PatchState::default(),
            view: MainView::default(),
            octave_base: 60,
            vk_targets: Vec::new(),
            held_notes: HashSet::new(),
            current_note: None,
            meter: PeakMeter::new(),
            opened_at: Instant::now(),
            auto_close_after: config.auto_close_after,
            project_path: None,
            recent: ui_state.recent_files,
            status_message: None,
            window_title: "GlitcherZ".to_string(),
            title_sent: String::new(),
            pending_file_action: None,
            allow_close: false,
            was_playing: false,
            voice_guards: Vec::new(),
            retiring_guards: Vec::new(),
            plugin_catalog: plugin_catalog::PluginCatalog::load(cached_plugins.as_deref()),
            picker: plugin_picker::PluginPicker::default(),
            picker_place: None,
            recent_plugins: ui_state.recent_plugins,
            voice_params: HashMap::new(),
            clap_voice_params: HashMap::new(),
            clap_voice_values: HashMap::new(),
            vst3_voice_values: HashMap::new(),
            clap_effect_params: HashMap::new(),
            clap_effect_values: HashMap::new(),
            vst3_effect_params: HashMap::new(),
            vst3_effect_values: HashMap::new(),
            clap_param_filter: String::new(),
            vst3_param_filter: String::new(),
            auto_param_filter: String::new(),
            voice_default_state: HashMap::new(),
            voice_preset_selected: HashMap::new(),
            preset_name_input: String::new(),
            sf2_library: None,
            plugin_editor: None,
            plugin_editor_wants_open: false,
            editor_user_open: false,
            first_frame_logged: false,
            close_sent: false,
        };
        app.push_all_params();
        if let Some(note) = config.auto_play_note {
            // Diagnostic path (smoke runs): bypass the patch route on purpose —
            // the flag must exercise the engine even when no input node exists,
            // and its whole point is to prove the audio path, not the routing.
            app.engine.send(EngineCommand::Note {
                track: app.selected_track,
                event: NoteEvent::On {
                    note,
                    velocity: 1.0,
                },
            });
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
                    // G6: vk → instrument cables are document-only routing
                    // (the keyboard has no engine node; the app resolves the
                    // route and addresses the instrument directly).
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
                    // U3b: the conductor terminal has no engine node — the
                    // route is a sentinel slot, not a node lookup.
                    if cable.to == PortRef::TimelineIn {
                        if !self.engine.set_timeline_in_route(*index) {
                            log::warn!("Tempo cable (track {index} → Timeline In) dispatch failed");
                        }
                        continue;
                    }
                    let Some(to) = to_node else {
                        log::warn!("Automation cable target node missing, skipping: {cable:?}");
                        continue;
                    };
                    let Some(id) = param_id_of(&cable.param) else {
                        log::warn!("Automation cable carries a non-node parameter: {cable:?}");
                        continue;
                    };
                    if !self.engine.set_auto_route(*index, to, id) {
                        log::warn!("Automation cable (track {index} → {to}) dispatch failed");
                    }
                }
            }
        }

        // 5) Active MIDI track count (D12).
        self.engine
            .set_midi_track_count(self.project().midi_tracks.len());

        // 6) Song loop range (U1): metadata lives outside the graph, so the
        //    full rebuild is also the undo/redo/open refresh path for it.
        self.push_loop_range();
    }

    /// Install a document node: build the processor (external plugins instantiated
    /// non-real-time) → engine `add_node` (new engine-side id) → register the mapping and guard.
    fn install_node(&mut self, node: &NodeDoc, sample_rate: u32) {
        // G6: the virtual keyboard is document-only (UI routing node) — it
        // never enters the engine graph, so it has no entry in `node_map`.
        if node.is_keyboard() {
            return;
        }
        let sr = convert::sample_rate_to_f32(sample_rate);
        let doc_id = node.node_id();
        let track = node.track();
        let (processor, guard, plugin_label): (Box<dyn Processor>, Option<PluginGuard>, String) =
            match node {
                NodeDoc::Instrument { voice, .. } => match voice {
                    VoiceSpec::Builtin => (Box::new(SynthProcessor::new(sr)), None, String::new()),
                    VoiceSpec::SoundFont {
                        path,
                        bank,
                        program,
                    } => {
                        // Document voice restore (D8): prefer the shared
                        // session library (avoids re-reading a large .sf2
                        // on every reinstall), otherwise load here; a
                        // missing or broken file falls back to the built-in
                        // synth (document fields kept for a later retry).
                        match self.obtain_sf2_voice(path, sr) {
                            Ok(mut processor) => {
                                processor.set_param(ParamId::Sf2BankSelect, f32::from(*bank));
                                processor.set_param(ParamId::Sf2ProgramChange, f32::from(*program));
                                (Box::new(processor), None, String::new())
                            }
                            Err(err) => {
                                log::error!("Failed to load SoundFont {path}: {err}");
                                (Box::new(SynthProcessor::new(sr)), None, String::new())
                            }
                        }
                    }
                    VoiceSpec::Clap {
                        path,
                        plugin_id,
                        state,
                    } => {
                        match ClapInstance::spawn_rt(
                            std::path::Path::new(path),
                            plugin_id,
                            ClapInstanceConfig {
                                sample_rate,
                                max_frames: 128,
                            },
                        ) {
                            Ok(mut instance) => {
                                // Factory-default snapshot for the preset
                                // "Reset" target — must precede the document
                                // state restore.
                                if let Ok(default_blob) = instance.guard.save_state() {
                                    self.voice_default_state.insert(doc_id, default_blob);
                                }
                                // Restore the persisted state blob (project
                                // reload / undo), then refresh the slider
                                // cache from the plugin's live values.
                                if !state.is_empty() {
                                    if let Err(err) = instance.guard.load_state(state) {
                                        log::warn!(
                                            "Failed to restore CLAP state for {plugin_id}: {err}"
                                        );
                                    }
                                }
                                let values = instance.read_param_values();
                                // Parameter enumeration must happen before the instance is
                                // destructured (the guard holds the plugin instance).
                                let params = instance.list_params();
                                let ClapInstance { guard, node } = instance;
                                self.clap_voice_params.insert(doc_id, params);
                                self.clap_voice_values.insert(doc_id, values);
                                (
                                    Box::new(node),
                                    Some(PluginGuard::Clap(guard)),
                                    plugin_id.clone(),
                                )
                            }
                            Err(err) => {
                                log::error!("Failed to instantiate CLAP plugin {plugin_id}: {err}");
                                self.clap_voice_params.remove(&doc_id);
                                self.clap_voice_values.remove(&doc_id);
                                (Box::new(SynthProcessor::new(sr)), None, String::new())
                            }
                        }
                    }
                    VoiceSpec::Vst3 {
                        path,
                        class_id,
                        state,
                        ..
                    } => {
                        match glitcherz_vst3_host::Vst3Instance::spawn_rt(
                            std::path::Path::new(path),
                            class_id,
                            glitcherz_vst3_host::Vst3InstanceConfig {
                                sample_rate,
                                max_frames: 128,
                            },
                        ) {
                            Ok(instance) => {
                                // Factory-default snapshot for the preset
                                // "Reset" target — must precede the document
                                // state restore.
                                if let Ok(default_blob) = instance.guard.save_state() {
                                    self.voice_default_state.insert(doc_id, default_blob);
                                }
                                // Restore the persisted state blob (project
                                // reload / undo; the CLAP D4 mirror).
                                if !state.is_empty() {
                                    if let Err(err) = instance.guard.load_state(state) {
                                        log::warn!(
                                            "Failed to restore VST3 state for {class_id}: {err}"
                                        );
                                    }
                                }
                                // Slider cache seeded from the controller
                                // read-back (normalized 0..1).
                                let values = instance.guard.read_param_values();
                                let glitcherz_vst3_host::Vst3Instance { guard, node } = instance;
                                let params = guard.parameters();
                                self.voice_params.insert(doc_id, params);
                                self.vst3_voice_values.insert(doc_id, values);
                                (
                                    Box::new(node),
                                    Some(PluginGuard::Vst3(guard)),
                                    class_id.clone(),
                                )
                            }
                            Err(err) => {
                                log::error!("Failed to instantiate VST3 plugin {class_id}: {err}");
                                self.voice_params.remove(&doc_id);
                                (Box::new(SynthProcessor::new(sr)), None, String::new())
                            }
                        }
                    }
                },
                NodeDoc::Effect { voice, .. } => match voice {
                    VoiceSpec::Builtin | VoiceSpec::SoundFont { .. } => {
                        (Box::new(DelayProcessor::new(sr)), None, String::new())
                    }
                    VoiceSpec::Clap {
                        path,
                        plugin_id,
                        state,
                    } => {
                        match ClapInstance::spawn_rt(
                            std::path::Path::new(path),
                            plugin_id,
                            ClapInstanceConfig {
                                sample_rate,
                                max_frames: 128,
                            },
                        ) {
                            Ok(mut instance) => {
                                if !state.is_empty() {
                                    if let Err(err) = instance.guard.load_state(state) {
                                        log::warn!(
                                            "Failed to restore CLAP effect state for {plugin_id}: {err}"
                                        );
                                    }
                                }
                                // Effect param caches keyed by the document
                                // node_id (panel data source; delivery goes
                                // through node addressing).
                                let fx_doc = node.node_id();
                                let params = instance.list_params();
                                let values = instance.read_param_values();
                                self.clap_effect_params.insert(fx_doc, params);
                                self.clap_effect_values.insert(fx_doc, values);
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
                    VoiceSpec::Vst3 {
                        path,
                        class_id,
                        state,
                        ..
                    } => {
                        match glitcherz_vst3_host::Vst3Instance::spawn_rt(
                            std::path::Path::new(path),
                            class_id,
                            glitcherz_vst3_host::Vst3InstanceConfig {
                                sample_rate,
                                max_frames: 128,
                            },
                        ) {
                            Ok(instance) => {
                                // Restore the persisted state blob (project
                                // reload / undo; the CLAP D4 mirror).
                                if !state.is_empty() {
                                    if let Err(err) = instance.guard.load_state(state) {
                                        log::warn!(
                                            "Failed to restore VST3 effect state for {class_id}: {err}"
                                        );
                                    }
                                }
                                // Effect param caches keyed by the document
                                // node_id (normalized 0..1 domain).
                                let fx_doc = node.node_id();
                                let params = instance.guard.parameters();
                                let values = instance.guard.read_param_values();
                                self.vst3_effect_params.insert(fx_doc, params);
                                self.vst3_effect_values.insert(fx_doc, values);
                                let glitcherz_vst3_host::Vst3Instance { guard, node } = instance;
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
                NodeDoc::Feedback { gain, .. } => {
                    // P6: the tap gain; the one-block delay is the
                    // compiler's ordering, not a buffer.
                    let mut processor = FeedbackProcessor::new();
                    processor.set_param(ParamId::FeedbackGain, *gain);
                    (Box::new(processor), None, String::new())
                }
                NodeDoc::ChannelStrip { .. } => {
                    (Box::new(ChannelStripProcessor::new()), None, String::new())
                }
                // G6: unreachable — virtual keyboards return before the match
                // (document-only node, no engine processor).
                NodeDoc::Keyboard { .. } => {
                    (Box::new(SynthProcessor::new(sr)), None, String::new())
                }
            };
        let engine_id = self.engine.fresh_node_id();
        // D15: the mixer is the only multi-input node; the engine compiler
        // uses the kind for fan-in validation + cable→channel binding. P6
        // adds the feedback tap kind, which the compiler orders before its
        // own input sources (the one-block delay).
        let kind = match node {
            NodeDoc::Mixer { .. } => NodeKind::Mixer,
            NodeDoc::Feedback { .. } => NodeKind::Feedback,
            _ => NodeKind::Plain,
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

    /// The live guard owning `engine_id` (active zone only; retiring
    /// guards are on their way out and are never reopened).
    fn guard_by_engine_id(&mut self, engine_id: u64) -> Option<&mut PluginGuard> {
        self.voice_guards
            .iter_mut()
            .find(|e| e.node_id == engine_id)
            .map(|e| &mut e._guard)
    }

    /// Closes the embedded plugin editor: the plugin's editor comes down
    /// first (`gui.destroy` / `IPlugView::removed` + release), then the
    /// native container window (spec `plugin-gui.md` §4.3 order — the
    /// plugin never sees its parent vanish). No-op when closed.
    fn close_plugin_editor(&mut self) {
        let Some(editor) = self.plugin_editor.take() else {
            return;
        };
        match self.guard_by_engine_id(editor.node_id) {
            Some(PluginGuard::Clap(g)) => g.close_editor(),
            Some(PluginGuard::Vst3(g)) => g.close_editor(),
            None => {}
        }
        drop(editor); // container destroyed after the plugin editor closed
    }

    /// Opens a plugin editor embedded under the main window for the given
    /// **document node** (spec `plugin-gui.md` §4/§6), CLAP or VST3 (G7).
    /// Needs the eframe frame (main HWND + scale) — therefore runs in the
    /// `ui()` tail, not in the panel.
    fn open_plugin_editor(&mut self, frame: &eframe::Frame, doc_node: u64) {
        if self.plugin_editor.is_some() {
            return;
        }
        let Some(&engine_id) = self.node_map.get(&doc_node) else {
            return;
        };
        let Some(hwnd) = plugin_window::main_window_handle(frame) else {
            self.set_message("Embedded editors need a native parent window".to_string());
            return;
        };
        let container = match plugin_window::PluginWindowContainer::new(hwnd) {
            Ok(c) => c,
            Err(err) => {
                log::warn!("Plugin editor container creation failed: {err}");
                self.set_message(format!("Editor window failed: {err}"));
                return;
            }
        };
        let ppp = frame
            .winit_window()
            .map(|w| w.scale_factor())
            .unwrap_or(1.0) as f32;
        // Plugin GUIs report sizes in pixels; the egui layout works in
        // points.
        let to_points = |width: i32, height: i32| (width as f32 / ppp, height as f32 / ppp);
        let opened: Result<(f32, f32), String> = match self.guard_by_engine_id(engine_id) {
            Some(PluginGuard::Clap(g)) => g
                .open_editor(container.raw_handle(), ppp as f64)
                .map(|s| to_points(s.width as i32, s.height as i32))
                .map_err(|e| e.to_string()),
            Some(PluginGuard::Vst3(g)) => g
                .open_editor(container.raw_handle(), ppp as f64)
                .map(|s| to_points(s.width, s.height))
                .map_err(|e| e.to_string()),
            None => Err("This node has no plugin editor (built-in voice)".to_string()),
        };
        match opened {
            Ok(size) => {
                log::info!(
                    "Embedded editor open: node {doc_node} engine {engine_id} size {size:?}pt"
                );
                self.plugin_editor = Some(PluginEditor {
                    doc_node,
                    node_id: engine_id,
                    size,
                    rect: None,
                    container,
                    pending_ack: None,
                });
            }
            Err(err) => {
                log::info!("Plugin editor open failed: {err}");
                self.set_message(format!("Plugin editor unavailable: {err}"));
                // Container dropped here — the plugin never attached to it.
            }
        }
    }

    /// Move a node's active guard into the retiring zone (called when a node is swapped out or
    /// removed; retiring guards are released once recycling receipts arrive).
    fn retire_guard(&mut self, engine_id: u64) {
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
    fn engine_id(&self, doc_id: u64) -> Option<u64> {
        self.node_map.get(&doc_id).copied()
    }

    /// The Synth panel's editing target (spec `plugin-gui.md` §6): the
    /// patch selection's first editable document node, falling back to the
    /// selected track's first-match instrument (pre-G4 behavior).
    fn panel_target_node(&self) -> Option<u64> {
        let selected = self.patch_state.selected_doc_nodes();
        let track = self
            .selected_track
            .min(self.project().midi_tracks.len().saturating_sub(1));
        resolve_panel_target(&self.project().graph, &selected, track)
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
    fn push_node_params(&mut self) {
        let nodes = self.project().graph.nodes.clone();
        for node in &nodes {
            let Some(&engine_id) = self.node_map.get(&node.node_id()) else {
                continue;
            };
            let params: Vec<(ParamId, f32)> = match node {
                NodeDoc::Instrument { synth, .. } => vec![
                    (ParamId::FilterCutoff, synth.cutoff_hz),
                    (ParamId::FilterResonance, synth.resonance),
                    (ParamId::FilterMode, synth.filter_mode),
                    (ParamId::Attack, synth.attack_s),
                    (ParamId::Decay, synth.decay_s),
                    (ParamId::Sustain, synth.sustain),
                    (ParamId::Release, synth.release_s),
                ],
                NodeDoc::Effect { delay, .. } => vec![
                    (ParamId::DelayTime, delay.time_s),
                    (ParamId::DelayFeedback, delay.feedback),
                    (ParamId::DelayMix, delay.mix),
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
    /// [`GlitcherZApp::begin_edit`] / [`GlitcherZApp::commit_edit`] (or run a command
    /// via [`GlitcherZApp::execute`]); the AGENTS.md rule "UI 层不得直接修改
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

    /// Records the edit started by [`GlitcherZApp::begin_edit`] as one undo step.
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
    /// End the gesture with [`GlitcherZApp::end_param_edit`] when the drag stops.
    pub fn begin_param_edit(&self) -> std::sync::Arc<Project> {
        self.session.project_arc()
    }

    /// Records one frame of a parameter gesture (a no-op mid-gesture).
    pub fn commit_param_edit(&mut self, label: &str, before: std::sync::Arc<Project>, key: &str) {
        self.session.push_undo_merged(label, before, key);
    }

    /// Ends a parameter gesture started via [`GlitcherZApp::commit_param_edit`].
    pub fn end_param_edit(&mut self) {
        self.session.end_merge();
    }

    /// Runs a command transactionally (undoable, panic-safe).
    ///
    /// Only the MIDI commands exist so far, so a successful run re-pushes the
    /// clip tables. Commands that change the node graph must call
    /// [`GlitcherZApp::apply_document`] themselves.
    pub fn execute(
        &mut self,
        id: &str,
        params: &serde_json::Value,
    ) -> glitcherz_command::Result<serde_json::Value> {
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
    ) -> glitcherz_command::Result<serde_json::Value> {
        self.session.execute_merged(id, params, key)
    }

    /// Built-in synth timbre parameters of an instrument node.
    fn synth_of_node(&self, doc_id: u64) -> Option<SynthParams> {
        match self.project().graph.node_by_id(doc_id)? {
            NodeDoc::Instrument { synth, .. } => Some(*synth),
            _ => None,
        }
    }

    /// Built-in synth timbre parameters of an instrument node (mutable).
    fn synth_mut_node(&mut self, doc_id: u64) -> Option<&mut SynthParams> {
        match self.project_mut().graph.node_by_id_mut(doc_id)? {
            NodeDoc::Instrument { synth, .. } => Some(synth),
            _ => None,
        }
    }

    /// Built-in delay parameters of an effect node.
    fn delay_of_node(&self, doc_id: u64) -> Option<DelayParams> {
        match self.project().graph.node_by_id(doc_id)? {
            NodeDoc::Effect { delay, .. } => Some(*delay),
            _ => None,
        }
    }

    /// Built-in delay parameters of an effect node (mutable).
    fn delay_mut_node(&mut self, doc_id: u64) -> Option<&mut DelayParams> {
        match self.project_mut().graph.node_by_id_mut(doc_id)? {
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
    fn set_auto_endpoint(&mut self, index: usize, target: AutoTarget, param: AutoParam) {
        let before = self.begin_edit();
        self.project_mut().graph.remove_auto_cable(index);
        let cable = match target {
            AutoTarget::Node(doc_id) => {
                glitcherz_model::CableDoc::automation(index, doc_id, param.clone())
            }
            // U3b: the terminal's single port fixes the parameter to Tempo.
            AutoTarget::TimelineIn => glitcherz_model::CableDoc::tempo(index),
        };
        self.project_mut().graph.cables.push(cable);
        self.commit_edit("Route Automation", before);
        match target {
            AutoTarget::TimelineIn => {
                self.engine.set_timeline_in_route(index);
            }
            AutoTarget::Node(doc_id) => {
                if let (Some(engine_id), Some(id)) = (self.engine_id(doc_id), param_id_of(&param)) {
                    self.engine.set_auto_route(index, engine_id, id);
                }
            }
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
                clap_voice_values: clap_values,
                vst3_voice_values: vst3_values,
                ..
            } = self;
            // Immutable reborrow aliases: two closures (auto_options /
            // plugin_inline) capture the same tables simultaneously.
            let doc = &session.project().graph;
            let clap = &*clap;
            let vst3 = &*vst3;
            let clap_values = &*clap_values;
            let vst3_values = &*vst3_values;
            let options = move |node_id: u64| -> Vec<(String, AutoParam)> {
                let mut v = Vec::new();
                let Some(node) = doc.node_by_id(node_id) else {
                    return v;
                };
                if let NodeDoc::Instrument { .. } = node {
                    v.push(("Cutoff".to_string(), AutoParam::FilterCutoff));
                    if let Some(params) = clap.get(&node_id) {
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
                    if let Some(params) = vst3.get(&node_id) {
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
            // Plugin-node inline controls (G5 spec §7): the first two
            // enumerated parameters of the node's plugin voice. CLAP rows
            // carry the raw plugin range, VST3 rows the normalized 0..1
            // domain — the same split as the panel sliders, so inline edits
            // stay interchangeable with panel edits.
            let plugin_inline = move |node_id: u64| -> Vec<patch::InlinePluginControl> {
                let voice = match doc.node_by_id(node_id) {
                    Some(NodeDoc::Instrument { voice, .. } | NodeDoc::Effect { voice, .. }) => {
                        Some(voice)
                    }
                    _ => None,
                };
                let mut v = Vec::new();
                match voice {
                    Some(VoiceSpec::Clap { .. }) => {
                        if let Some(params) = clap.get(&node_id) {
                            for p in params.iter().take(2) {
                                v.push(patch::InlinePluginControl {
                                    param_id: p.id,
                                    label: inline_plugin_label(&p.name, p.id),
                                    value: clap_values
                                        .get(&node_id)
                                        .and_then(|m| m.get(&p.id))
                                        .copied()
                                        .unwrap_or(p.default_value),
                                    min: p.min_value,
                                    max: p.max_value,
                                });
                            }
                        }
                    }
                    Some(VoiceSpec::Vst3 { .. }) => {
                        if let Some(params) = vst3.get(&node_id) {
                            for p in params.iter().take(2) {
                                v.push(patch::InlinePluginControl {
                                    param_id: p.id,
                                    label: inline_plugin_label(&p.title, p.id),
                                    value: vst3_values
                                        .get(&node_id)
                                        .and_then(|m| m.get(&p.id))
                                        .copied()
                                        .unwrap_or(p.default_normalized),
                                    min: 0.0,
                                    max: 1.0,
                                });
                            }
                        }
                    }
                    _ => {}
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
                    plugin_inline: &plugin_inline,
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
                patch::PatchAction::AddNode { source, at } => {
                    self.add_patch_node(source, at);
                }
                patch::PatchAction::OpenPluginPicker {
                    category,
                    format,
                    place,
                } => {
                    self.open_plugin_picker(category, format, place);
                }
                patch::PatchAction::PickSoundFont { at } => {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("SoundFont", &["sf2", "sf3"])
                        .pick_file()
                    {
                        self.create_patch_node(
                            patch::NodeSource::SoundFont {
                                path: path.display().to_string(),
                            },
                            at,
                        );
                    }
                }
                patch::PatchAction::RescanPlugins => {
                    self.plugin_catalog.rescan();
                    self.set_message("Rescanning plug-ins…".to_string());
                }
                patch::PatchAction::InsertNode { cable, source, at } => {
                    self.insert_node_in_cable(cable, source, at);
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
                            .push(glitcherz_model::CableDoc::midi(track, to_doc));
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
                patch::PatchAction::ConnectKeyboardMidi { from_doc, to_doc } => {
                    self.connect_keyboard_midi(from_doc, to_doc);
                }
                patch::PatchAction::ConnectAuto {
                    index,
                    to_doc,
                    param,
                } => {
                    self.set_auto_endpoint(index, AutoTarget::Node(to_doc), param);
                }
                patch::PatchAction::ConnectTempoIn { index } => {
                    self.set_auto_endpoint(index, AutoTarget::TimelineIn, AutoParam::Tempo);
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
                patch::PatchAction::OpenNode(doc_id) => {
                    // The double-click's first click already selected the node
                    // (and linked the owning track); the jump itself just
                    // switches the view (spec `plugin-gui.md` §6).
                    if self.project().graph.node_by_id(doc_id).is_some() {
                        self.view = MainView::Synth;
                    }
                }
                patch::PatchAction::SetInlineParam { node, param, value } => {
                    self.apply_inline_param(node, param, value);
                }
            }
        }
    }

    /// Open the filterable plugin popup for a leaf of the `Add` menu (U9 E3,
    /// spec `plugin-browser.md` §6). `category`/`format` narrow the list; both
    /// `None` is the global `Search Plug-ins…` entry, whose pick decides the
    /// node category from the entry's own classification (spec D5).
    fn open_plugin_picker(
        &mut self,
        category: Option<patch::NodeCategory>,
        format: Option<plugin_catalog::PluginFormat>,
        place: patch::PickerPlace,
    ) {
        self.picker_place = Some(place);
        self.picker.open(plugin_picker::PickFilter {
            want_instrument: category.map(patch::NodeCategory::is_instrument),
            format,
        });
    }

    /// Draw the plugin popup and create the picked node (U9 E3), or splice it
    /// into a cable (U9 E4 — same popup, different receiver).
    ///
    /// The popup is drawn above every panel (`Order::Foreground`); a pick is
    /// turned into the node's **final** source here, so the creation path
    /// installs the plugin exactly once (spec D1).
    fn plugin_picker_frame(&mut self, ctx: &egui::Context) {
        let scan_progress = self
            .plugin_catalog
            .is_scanning()
            .then(|| self.plugin_catalog.progress());
        let outcome = self.picker.show(
            ctx,
            self.plugin_catalog.entries(),
            &self.recent_plugins,
            scan_progress,
        );
        match outcome {
            Some(plugin_picker::PickOutcome::Picked(choice)) => {
                plugin_picker::remember(&mut self.recent_plugins, &choice);
                let Some(place) = self.picker_place.take() else {
                    return;
                };
                let source = pick_to_source(choice);
                match place {
                    patch::PickerPlace::Add { at } => {
                        self.add_patch_node(source, at);
                    }
                    patch::PickerPlace::Insert { cable, at } => {
                        self.insert_node_in_cable(cable, source, at);
                    }
                }
                // The status bar text changed; repaint immediately instead of
                // waiting for the 50 ms display tick.
                ctx.request_repaint();
            }
            Some(plugin_picker::PickOutcome::Cancelled) => {
                self.picker_place = None;
            }
            None => {}
        }
    }

    /// Add a new document node from the patch context menu: allocate the next document
    /// node_id, install the processor into the engine (born dangling — no cables; D6),
    /// place it on the patch at the click position, and refresh the owning track's
    /// addressing table. The new node's owning track is the currently selected track.
    fn add_patch_node(&mut self, source: patch::NodeSource, at: egui::Pos2) -> Option<u64> {
        self.create_patch_node(source, at)
    }

    /// Create + install a patch node (returns the new document node_id; `None` when the
    /// node table is full or the engine install failed — the document is rolled back so
    /// doc and engine stay in sync).
    ///
    /// The node is born with its **final** source (spec `plugin-browser.md` D1): a
    /// plugin source is instantiated by this single `install_node` call, so there is no
    /// "create a built-in, then swap the voice" double install (and therefore one
    /// install, one undo step).
    fn create_patch_node(&mut self, source: patch::NodeSource, at: egui::Pos2) -> Option<u64> {
        let track = self
            .selected_track
            .min(self.project().midi_tracks.len().saturating_sub(1));
        self.create_patch_node_on(source, at, track)
    }

    /// [`GlitcherZApp::create_patch_node`] with an explicit owning track.
    ///
    /// Needed by the cable-midpoint insert: the new processor belongs to the
    /// track that owns the cable it was spliced into, **not** to whichever
    /// track happens to be selected (they differ when the user inserts into
    /// another track's chain). The `track` field drives per-track addressing
    /// (`register_track_chain`) and the mixer panel grouping.
    fn create_patch_node_on(
        &mut self,
        source: patch::NodeSource,
        at: egui::Pos2,
        track: usize,
    ) -> Option<u64> {
        if self.project().graph.nodes.len() >= MAX_NODES {
            self.set_message("Add node failed: node table full (64)".to_string());
            return None;
        }
        // G6 (A design): an input source belongs to exactly one node, and the
        // only wired source today is the computer keyboard — so a second
        // input node has nothing to listen to. Refuse instead of writing a
        // document `validate` would reject.
        if matches!(source, patch::NodeSource::Keyboard)
            && self.project().graph.computer_keyboard().is_some()
        {
            self.set_message(
                "The computer keyboard already has an input node (one source per node)".to_string(),
            );
            return None;
        }
        let node_id = self.project().graph.next_node_id();
        let track = track.min(self.project().midi_tracks.len().saturating_sub(1));
        let node = match source {
            patch::NodeSource::BuiltinSynth => NodeDoc::Instrument {
                node_id,
                track,
                voice: VoiceSpec::Builtin,
                synth: SynthParams::default(),
            },
            patch::NodeSource::BuiltinDelay => NodeDoc::Effect {
                node_id,
                track,
                voice: VoiceSpec::Builtin,
                delay: DelayParams::default(),
            },
            patch::NodeSource::SoundFont { path } => NodeDoc::Instrument {
                node_id,
                track,
                voice: VoiceSpec::SoundFont {
                    path,
                    bank: 0,
                    program: 0,
                },
                synth: SynthParams::default(),
            },
            patch::NodeSource::ClapInstrument { path, plugin_id } => NodeDoc::Instrument {
                node_id,
                track,
                voice: VoiceSpec::Clap {
                    path,
                    plugin_id,
                    state: Vec::new(),
                },
                synth: SynthParams::default(),
            },
            patch::NodeSource::Vst3Instrument { path, class_id } => NodeDoc::Instrument {
                node_id,
                track,
                voice: VoiceSpec::Vst3 {
                    path,
                    class_id,
                    state: Vec::new(),
                },
                synth: SynthParams::default(),
            },
            patch::NodeSource::ClapEffect { path, plugin_id } => NodeDoc::Effect {
                node_id,
                track,
                voice: VoiceSpec::Clap {
                    path,
                    plugin_id,
                    state: Vec::new(),
                },
                delay: DelayParams::default(),
            },
            patch::NodeSource::Vst3Effect { path, class_id } => NodeDoc::Effect {
                node_id,
                track,
                voice: VoiceSpec::Vst3 {
                    path,
                    class_id,
                    state: Vec::new(),
                },
                delay: DelayParams::default(),
            },
            patch::NodeSource::Mixer => NodeDoc::Mixer {
                node_id,
                track,
                inputs: MIN_MIXER_CHANNELS as u8,
                channels: vec![Default::default(); MIN_MIXER_CHANNELS],
                out_pan: 0.0,
                out_muted: false,
            },
            // G6: document-only input node — no engine processor, so the
            // usual "installed into the engine?" check does not apply. It
            // owns the computer keyboard (the only wired source in v1).
            patch::NodeSource::Keyboard => NodeDoc::Keyboard {
                node_id,
                track,
                source: MidiSource::Computer,
            },
            patch::NodeSource::Feedback => NodeDoc::Feedback {
                node_id,
                track,
                gain: 0.5,
            },
        };
        if node.is_keyboard() {
            let before = self.begin_edit();
            self.project_mut().graph.nodes.push(node);
            // No auto-cable: an input node starts silent on purpose (the
            // default project's node is the one that is routed).
            self.commit_edit("Add Input Node", before);
            self.patch_state.place_node(node_id, at);
            return Some(node_id);
        }
        // A plugin source must leave the engine with a real instance: the
        // installer silently falls back to the built-in processor, which would
        // leave a document claiming a plugin voice the engine does not run.
        // Such a node must not exist at all (spec §7 step 2). SoundFonts are
        // exempt: a missing file already falls back to the built-in voice by
        // design (spec `sf2-loader.md` D8), and no guard is involved.
        let expects_guard = matches!(
            node,
            NodeDoc::Instrument {
                voice: VoiceSpec::Clap { .. } | VoiceSpec::Vst3 { .. },
                ..
            } | NodeDoc::Effect {
                voice: VoiceSpec::Clap { .. } | VoiceSpec::Vst3 { .. },
                ..
            }
        );
        let sample_rate = self.engine.info().sample_rate;
        let before = self.begin_edit();
        self.project_mut().graph.nodes.push(node.clone());
        self.install_node(&node, sample_rate);
        let engine_id = self.node_map.get(&node_id).copied();
        let installed = engine_id.is_some_and(|engine_id| {
            !expects_guard
                || self
                    .voice_guards
                    .iter()
                    .any(|entry| entry.node_id == engine_id)
        });
        if !installed {
            // Engine slot/queue full, or the plugin failed to instantiate:
            // roll the document back so doc and engine stay one.
            self.session.restore(before);
            self.set_message(if expects_guard {
                "Add node failed: the plug-in could not be loaded".to_string()
            } else {
                "Add node failed (engine slots or queue full)".to_string()
            });
            return None;
        }
        self.commit_edit("Add Node", before);
        self.patch_state.place_node(node_id, at);
        self.register_track_chain(track);
        Some(node_id)
    }

    /// Insert a new node into an existing audio cable: `A → B` becomes `A → new → B`
    /// (cable-midpoint menu). The new node is placed where the user clicked.
    ///
    /// One undo step for the whole splice (spec `plugin-browser.md` E4): the
    /// three sub-operations (create, cut the old cable, two new cables) each
    /// record their own step, so history is suspended for their duration and a
    /// single outer snapshot is committed at the end.
    fn insert_node_in_cable(
        &mut self,
        cable_index: usize,
        source: patch::NodeSource,
        at: egui::Pos2,
    ) {
        let Some(cable) = self.project().graph.cables.get(cable_index).cloned() else {
            return;
        };
        let (from_doc, to, in_port) = match splice_endpoints(&cable) {
            Ok(endpoints) => endpoints,
            Err(reason) => {
                self.set_message(format!("Insert failed: {reason}"));
                return;
            }
        };
        if !source.fits_audio_insert() {
            // Unreachable from the menu (its leaves are effects / mixer only),
            // but the picker could hand back anything if a filter is ever
            // widened — an instrument here would be born without a MIDI feed.
            self.set_message("Insert failed: only effects / mixers can be inserted".to_string());
            return;
        }
        // The processor joins the track that owns the cable, not the selected
        // one (see `create_patch_node_on`).
        let track = self
            .project()
            .graph
            .node_by_id(from_doc)
            .map_or(self.selected_track, |node| node.track());
        let before = self.begin_edit();
        self.session.suspend_history();
        let Some(new_id) = self.create_patch_node_on(source, at, track) else {
            self.session.resume_history();
            return;
        };
        self.delete_cable(cable_index);
        self.connect_audio(from_doc, PortRef::Node(new_id), 0);
        // Preserve the original channel binding when the downstream target is
        // a mixer (D17) — the new node takes over the exact same slot.
        self.connect_audio(new_id, to, in_port);
        self.session.resume_history();
        self.commit_edit("Insert Node", before);
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
                .push(glitcherz_model::CableDoc::audio_to_channel(
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

    /// Connect a keyboard-input MIDI cable (G6, spec `plugin-gui.md` §8):
    /// **document-only** — the input node has no engine node, so there is no
    /// edge to dispatch; the live routes are resolved per frame from the
    /// document (`GraphDoc::computer_keyboard` + `keyboard_routes_from`).
    ///
    /// A MIDI source is one out jack with any number of cables (multi-out /
    /// single-in — the same rule the timeline's track outlets follow), so a
    /// new cable **adds** a destination instead of replacing the previous one:
    /// that is what makes synth layering possible. Duplicate cables are
    /// ignored, and the fan-out stops at [`MAX_MIDI_FANOUT`].
    fn connect_keyboard_midi(&mut self, from_doc: u64, to_doc: u64) {
        let is_vk = self
            .project()
            .graph
            .node_by_id(from_doc)
            .is_some_and(NodeDoc::is_keyboard);
        if !is_vk {
            self.set_message("Connect failed: source is not a virtual keyboard".to_string());
            return;
        }
        let existing: Vec<u64> = self
            .project()
            .graph
            .cables
            .iter()
            .filter(|c| c.kind == CableKind::Midi && c.from == PortRef::Node(from_doc))
            .filter_map(|c| match c.to {
                PortRef::Node(id) => Some(id),
                _ => None,
            })
            .collect();
        if existing.contains(&to_doc) {
            self.set_message("Already connected".to_string());
            return;
        }
        if existing.len() >= MAX_MIDI_FANOUT {
            self.set_message(format!(
                "Connect failed: a keyboard feeds at most {MAX_MIDI_FANOUT} instruments"
            ));
            return;
        }
        let before = self.begin_edit();
        self.project_mut()
            .graph
            .cables
            .push(glitcherz_model::CableDoc::keyboard_midi(from_doc, to_doc));
        self.commit_edit("Connect Keyboard", before);
    }

    /// Ctrl+L (G6 quick audition): toggle MIDI cables from the
    /// computer-keyboard input node to every **selected instrument node**.
    ///
    /// Unlinked selection → link it; fully linked selection → unlink exactly
    /// those. Cables to other instruments are left untouched, so the shortcut
    /// never silently rewires a hand-built patch (layering survives), and the
    /// `MAX_MIDI_FANOUT` ceiling still applies when linking.
    fn toggle_audition_route(&mut self) {
        let selected = self.patch_state.selected_doc_nodes();
        let instruments: Vec<u64> = self
            .project()
            .graph
            .nodes
            .iter()
            .filter(|n| matches!(n, NodeDoc::Instrument { .. }) && selected.contains(&n.node_id()))
            .map(NodeDoc::node_id)
            .collect();
        if instruments.is_empty() {
            self.set_message("Select an instrument node in the patch to audition it".to_string());
            return;
        }
        let Some(vk) = self.project().graph.computer_keyboard() else {
            self.set_message(
                "No keyboard input node: add one from the patch context menu".to_string(),
            );
            return;
        };
        let linked: Vec<u64> = self
            .project()
            .graph
            .cables
            .iter()
            .filter(|c| c.kind == CableKind::Midi && c.from == PortRef::Node(vk))
            .filter_map(|c| match c.to {
                PortRef::Node(id) => Some(id),
                _ => None,
            })
            .collect();
        let missing: Vec<u64> = instruments
            .iter()
            .copied()
            .filter(|id| !linked.contains(id))
            .collect();
        if missing.is_empty() {
            let before = self.begin_edit();
            self.project_mut().graph.cables.retain(|c| {
                !(c.kind == CableKind::Midi
                    && c.from == PortRef::Node(vk)
                    && matches!(c.to, PortRef::Node(id) if instruments.contains(&id)))
            });
            self.commit_edit("Unlink Audition", before);
            self.set_message("Keyboard unlinked from the selected synth".to_string());
            return;
        }
        if linked.len() + missing.len() > MAX_MIDI_FANOUT {
            self.set_message(format!(
                "Link failed: a keyboard feeds at most {MAX_MIDI_FANOUT} instruments"
            ));
            return;
        }
        let before = self.begin_edit();
        for to in &missing {
            self.project_mut()
                .graph
                .cables
                .push(glitcherz_model::CableDoc::keyboard_midi(vk, *to));
        }
        self.commit_edit("Link Audition", before);
        self.set_message(format!(
            "Keyboard linked to {} synth{}",
            missing.len(),
            if missing.len() == 1 { "" } else { "s" }
        ));
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
        // G6: the vk node is document-only (no engine counterpart).
        let is_keyboard = node.is_keyboard();
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
        // G6: the vk node has no engine counterpart — only the document
        // loses it (its MIDI cables go with it, no bypass rewiring).
        let engine_id = if is_keyboard {
            None
        } else {
            let Some(&engine_id) = self.node_map.get(&doc_id) else {
                return;
            };
            if !self.engine.remove_node(engine_id) {
                self.set_message("Node removal failed (queue full)".to_string());
                return;
            }
            Some(engine_id)
        };
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
                    .push(glitcherz_model::CableDoc::audio_to_channel(
                        from.clone(),
                        to.clone(),
                        port,
                    ));
            }
        }
        self.node_map.remove(&doc_id);
        if let Some(engine_id) = engine_id {
            self.retire_guard(engine_id);
        }
        // Param caches are node-keyed (G4): dropping them for a deleted
        // node is always safe.
        self.clap_effect_params.remove(&doc_id);
        self.clap_effect_values.remove(&doc_id);
        self.vst3_effect_params.remove(&doc_id);
        self.vst3_effect_values.remove(&doc_id);
        self.voice_params.remove(&doc_id);
        self.clap_voice_params.remove(&doc_id);
        self.clap_voice_values.remove(&doc_id);
        self.vst3_voice_values.remove(&doc_id);
        self.voice_default_state.remove(&doc_id);
        self.voice_preset_selected.remove(&doc_id);
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
    /// [`GlitcherZApp::project_mut`] are not in the history yet.
    pub fn undo(&mut self) {
        // Undo/redo invalidates every held index: drag, selection, marquee.
        self.roll_state.clear_gesture();
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
        // Undo/redo invalidates every held index: drag, selection, marquee.
        self.roll_state.clear_gesture();
        self.lane_state.clear_drag();
        if self.session.redo() {
            self.apply_document();
        } else {
            self.set_message("Nothing to redo".to_string());
        }
    }

    /// Save to the current path; if never saved, pop up the "Save As" dialog.
    /// Returns whether the document reached the disk — the unsaved-changes
    /// modal (U2) only proceeds when it did.
    fn save_project(&mut self) -> bool {
        match self.project_path.clone() {
            Some(path) => self.do_save(path),
            None => self.save_project_as(),
        }
    }

    /// "Save As": pop up the save dialog.
    fn save_project_as(&mut self) -> bool {
        let Some(mut path) = rfd::FileDialog::new()
            .add_filter("GlitcherZ Project", &["glz"])
            .set_file_name("Untitled.glz")
            .save_file()
        else {
            return false;
        };
        path.set_extension("glz");
        self.do_save(path)
    }

    /// "Open": pop up the file picker dialog (unsaved changes ask first).
    fn open_project_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("GlitcherZ Project", &["glz"])
            .pick_file()
        else {
            return;
        };
        self.request_file_action(FileAction::Open(path));
    }

    /// A destructive file action (new / open / quit) requested by the menu or
    /// a shortcut: run it while the document is clean, otherwise park it
    /// behind the unsaved-changes modal (U2).
    fn request_file_action(&mut self, action: FileAction) {
        if self.pending_file_action.is_some() {
            return;
        }
        if self.session.is_dirty() {
            self.pending_file_action = Some(action);
        } else {
            self.perform_file_action(action);
        }
    }

    /// Runs a file action the user has already approved.
    fn perform_file_action(&mut self, action: FileAction) {
        match action {
            FileAction::New => self.do_new(),
            FileAction::Open(path) => self.do_open(path),
            // `ui()` sends the viewport close once `allow_close` is up, so
            // this needs no egui context.
            FileAction::Exit => self.allow_close = true,
        }
    }

    /// New project (U2): a fresh default document replaces the current one and
    /// the engine graph is rebuilt from it. Never called directly — go through
    /// [`Self::request_file_action`] so unsaved work is protected.
    fn do_new(&mut self) {
        self.close_plugin_editor();
        self.editor_user_open = false;
        self.send_sequencer(SequencerCommand::Stop);
        self.session.replace(Project::default());
        self.project_path = None;
        self.selected_track = 0;
        self.selected_auto = 0;
        self.edit_clip = None;
        self.roll_state = piano_roll::RollState::new();
        self.timeline_state = timeline::TimelineState::default();
        self.lane_state = lane::LaneState::default();
        self.patch_state = patch::PatchState::default();
        self.held_notes.clear();
        self.push_all_params();
        // U1: the fresh document's (default) loop range reaches the engine.
        self.push_loop_range();
        self.set_message("New project".to_string());
        log::info!("New project");
    }

    /// Unsaved-changes modal for a parked file action (U2). The backdrop and
    /// Escape cancel; the three buttons are drawn here.
    fn unsaved_changes_dialog(&mut self, ctx: &egui::Context) {
        let Some(action) = self.pending_file_action.clone() else {
            return;
        };
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("glitcherz_unsaved_changes")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.heading("Unsaved changes");
            ui.label(format!(
                "The project has unsaved changes. Save them before you {}?",
                action.verb()
            ));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    choice = Some(ConfirmChoice::Save);
                }
                if ui.button("Discard changes").clicked() {
                    choice = Some(ConfirmChoice::Discard);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(ConfirmChoice::Cancel);
                }
            });
        });
        if modal.should_close() {
            choice = Some(ConfirmChoice::Cancel);
        }
        match choice {
            Some(ConfirmChoice::Save) => {
                // A failed or cancelled save keeps the dialog (and the action)
                // pending: nothing is discarded behind the user's back.
                if self.save_project() {
                    self.pending_file_action = None;
                    self.perform_file_action(action);
                }
            }
            Some(ConfirmChoice::Discard) => {
                self.pending_file_action = None;
                self.perform_file_action(action);
            }
            Some(ConfirmChoice::Cancel) => self.pending_file_action = None,
            None => {}
        }
    }

    /// "Import MIDI": pick a file and append its tracks to the project (U2).
    fn import_midi_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("MIDI File", &["mid", "midi", "smf"])
            .pick_file()
        else {
            return;
        };
        match project_files::read_midi(&path) {
            Ok(imported) => {
                self.import_midi_tracks(imported.tracks, imported.time_signature, &path);
            }
            Err(err) => {
                self.set_message(format!("MIDI import failed: {err}"));
                log::error!("MIDI import failed: {err}");
            }
        }
    }

    /// Appends imported tracks: empty tracks on screen are filled first, the
    /// rest get a fresh MIDI track (nodes + cables). The whole import is one
    /// undo step — the per-track steps must not split the history.
    ///
    /// U3a: the file's meter is adopted only when the project has no content
    /// yet (that import is really "open"); otherwise the project keeps its own
    /// meter and the status line says so — re-measuring the grid under existing
    /// clips is not something an import may do silently.
    fn import_midi_tracks(
        &mut self,
        imported: Vec<MidiTrack>,
        file_meter: Option<glitcherz_model::TimeSignature>,
        source: &std::path::Path,
    ) {
        let imported_len = imported.len();
        if imported_len == 0 {
            self.set_message(format!("No notes found in {}", display_name(source)));
            return;
        }
        let unwritten = project_files::is_unwritten(self.project());
        let plan = project_files::plan_import(&self.project().midi_tracks, imported_len);
        let before = self.begin_edit();
        self.session.suspend_history();
        let mut queue = imported.into_iter();
        let mut first_target = None;
        let mut placed = 0usize;
        for index in &plan.reuse {
            let Some(track) = queue.next() else { break };
            self.project_mut().midi_tracks[*index] = track;
            self.send_track_clips(*index);
            first_target.get_or_insert(*index);
            placed += 1;
        }
        for _ in 0..plan.append {
            let Some(track) = queue.next() else { break };
            let before_len = self.project().midi_tracks.len();
            self.add_midi_track();
            if self.project().midi_tracks.len() <= before_len {
                break; // engine slots or the command queue are full
            }
            self.project_mut().midi_tracks[before_len] = track;
            self.send_track_clips(before_len);
            first_target.get_or_insert(before_len);
            placed += 1;
        }
        self.session.resume_history();
        self.commit_edit("Import MIDI", before);
        if let Some(index) = first_target {
            self.selected_track = index;
        }
        let skipped = imported_len.saturating_sub(placed);
        let mut message = format!("Imported {placed} track(s) from {}", display_name(source));
        if skipped > 0 {
            message.push_str(&format!(
                " — {skipped} skipped (track limit {MAX_MIDI_TRACKS})"
            ));
        }
        // Meter (U3a): same undo step as the tracks, so one Ctrl+Z undoes the
        // import as a whole.
        if let Some(meter) = file_meter {
            let current = self.project().metadata.time_signature;
            if meter != current {
                if unwritten {
                    self.project_mut().metadata.time_signature = meter;
                    self.push_time_signature();
                    message.push_str(&format!(" — meter set to {meter}"));
                } else {
                    message.push_str(&format!(" — file is {meter}, project stays {current}"));
                }
            }
        }
        self.set_message(message);
        log::info!("MIDI imported: {}", source.display());
    }

    /// "Export MIDI": write the arrangement as a format-1 SMF (U2). Not an
    /// edit — the file on disk changes, the document does not.
    fn export_midi_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("MIDI File", &["mid"])
            .set_file_name(project_files::midi_export_name(
                self.project_path.as_deref(),
            ))
            .save_file()
        else {
            return;
        };
        let mut path = path;
        path.set_extension("mid");
        match project_files::write_midi(&path, self.project()) {
            Ok(bytes) => {
                self.set_message(format!("Exported {} ({bytes} bytes)", display_name(&path)));
                log::info!("MIDI exported: {}", path.display());
            }
            Err(err) => {
                self.set_message(format!("MIDI export failed: {err}"));
                log::error!("MIDI export failed: {err}");
            }
        }
    }

    /// Captures live plugin state blobs into the document before saving
    /// (save-time only, not an undoable edit — same class as the patch
    /// layout, spec/plugin-state.md D3). Individual failures are logged and
    /// skipped: a broken plugin must not block the whole save (D7).
    fn capture_plugin_states(&mut self) {
        let doc_by_engine: std::collections::HashMap<u64, u64> = self
            .node_map
            .iter()
            .map(|(doc_id, engine_id)| (*engine_id, *doc_id))
            .collect();
        // Two passes: the guard loop borrows `voice_guards` mutably, the doc
        // write needs `project_mut` — collect blobs first, apply afterwards.
        let mut blobs: Vec<(u64, Vec<u8>)> = Vec::new();
        for entry in &mut self.voice_guards {
            let Some(doc_id) = doc_by_engine.get(&entry.node_id).copied() else {
                continue;
            };
            let blob = match &mut entry._guard {
                PluginGuard::Clap(guard) => match guard.save_state() {
                    Ok(blob) => Some(blob),
                    Err(glitcherz_clap_host::HostError::NoState) => None,
                    Err(err) => {
                        log::warn!("State capture skipped for {}: {err}", entry.plugin_id);
                        None
                    }
                },
                PluginGuard::Vst3(guard) => match guard.save_state() {
                    Ok(blob) => Some(blob),
                    Err(err) => {
                        log::warn!("State capture skipped for {}: {err}", entry.plugin_id);
                        None
                    }
                },
            };
            if let Some(blob) = blob {
                blobs.push((doc_id, blob));
            }
        }
        for (doc_id, blob) in blobs {
            let Some(node) = self.project_mut().graph.node_by_id_mut(doc_id) else {
                continue;
            };
            match node {
                NodeDoc::Instrument {
                    voice: VoiceSpec::Clap { state, .. },
                    ..
                }
                | NodeDoc::Effect {
                    voice: VoiceSpec::Clap { state, .. },
                    ..
                }
                | NodeDoc::Instrument {
                    voice: VoiceSpec::Vst3 { state, .. },
                    ..
                }
                | NodeDoc::Effect {
                    voice: VoiceSpec::Vst3 { state, .. },
                    ..
                } => *state = blob,
                _ => {}
            }
        }
    }

    /// Write to disk and update recent files / status message. Returns
    /// whether the write succeeded (U2: the title's dirty marker and the
    /// unsaved-changes modal depend on it).
    fn do_save(&mut self, path: PathBuf) -> bool {
        // Persist the patch canvas layout (view data) with the project.
        let layout = self.patch_state.export_layout(&self.project().graph);
        self.project_mut().graph.layout = layout;
        self.capture_plugin_states();
        match glitcherz_model::save_project(self.project(), &path) {
            Ok(()) => {
                self.session.mark_clean();
                self.project_path = Some(path.clone());
                self.push_recent(path.clone());
                self.set_message(format!("Saved {}", path.display()));
                log::info!("Project saved: {}", path.display());
                true
            }
            Err(err) => {
                self.set_message(format!("Save failed: {err}"));
                log::error!("Failed to save project: {err}");
                false
            }
        }
    }

    /// Load from disk, replace the document, rebuild the engine graph per the document, and
    /// re-push all parameters.
    fn do_open(&mut self, path: PathBuf) {
        match glitcherz_model::load_project(&path) {
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
                // U1: the opened document's song loop reaches the engine.
                self.push_loop_range();
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

    /// Build a SoundFont voice processor from the shared session library
    /// (v1: one library per session, spec §6): reuse the cached font when
    /// the path matches, otherwise load (and replace the cache). Control
    /// thread only (D3).
    fn obtain_sf2_voice(
        &mut self,
        path: &str,
        sample_rate: f32,
    ) -> Result<Sf2VoiceProcessor, Sf2Error> {
        if let Some(lib) = self.sf2_library.as_ref() {
            if lib.path() == std::path::Path::new(path) {
                return Sf2VoiceProcessor::new(lib.font(), sample_rate);
            }
        }
        let library = Sf2Library::load(std::path::Path::new(path))?;
        self.sf2_library = Some(Arc::new(library));
        Sf2VoiceProcessor::new(
            self.sf2_library.as_ref().expect("just set").font(),
            sample_rate,
        )
    }

    /// Switch a SoundFont instrument node's preset: document bank/program as
    /// one undoable edit + live node-param delivery (block-granularity, D7).
    fn set_soundfont_preset(&mut self, doc_id: u64, bank: u8, program: u8) {
        let before = self.begin_edit();
        if let Some(NodeDoc::Instrument {
            voice:
                VoiceSpec::SoundFont {
                    bank: b,
                    program: p,
                    ..
                },
            ..
        }) = self.project_mut().graph.node_by_id_mut(doc_id)
        {
            *b = bank;
            *p = program;
        }
        if let Some(engine_id) = self.engine_id(doc_id) {
            self.engine
                .set_node_param(engine_id, ParamId::Sf2BankSelect, f32::from(bank));
            self.engine
                .set_node_param(engine_id, ParamId::Sf2ProgramChange, f32::from(program));
            self.commit_edit("Select SF2 Preset", before);
            return;
        }
        self.commit_edit("Select SF2 Preset", before);
        self.set_message("Preset switch failed: voice not installed".to_string());
    }

    // ---- Instrument presets (host-side library over the state blob) ----

    /// An instrument node's preset-library key (`None` for the built-in
    /// synth, which has no presets yet).
    fn voice_plugin_key(&self, doc_id: u64) -> Option<String> {
        match self.project().graph.node_by_id(doc_id)? {
            NodeDoc::Instrument { voice, .. } => match voice {
                VoiceSpec::Clap {
                    path, plugin_id, ..
                } => Some(preset_plugin_key("clap", path, plugin_id)),
                VoiceSpec::Vst3 { path, class_id, .. } => {
                    Some(preset_plugin_key("vst3", path, class_id))
                }
                VoiceSpec::Builtin | VoiceSpec::SoundFont { .. } => None,
            },
            _ => None,
        }
    }

    /// The live guard owning a document node (its own engine id).
    fn node_guard(&mut self, doc_id: u64) -> Option<&mut PluginGuard> {
        let engine_id = *self.node_map.get(&doc_id)?;
        self.voice_guards
            .iter_mut()
            .find(|e| e.node_id == engine_id)
            .map(|e| &mut e._guard)
    }

    /// Whether a document node's live guard is an external plugin that can
    /// embed an editor (CLAP or VST3; read-only — usable where
    /// `node_guard`'s `&mut` borrow would conflict). Whether the specific
    /// plugin actually exposes a view is only known once `open_editor`
    /// runs; this is the "show the button / reopen on target change"
    /// predicate.
    fn node_guard_has_editor(&self, doc_id: u64) -> bool {
        let Some(&engine_id) = self.node_map.get(&doc_id) else {
            return false;
        };
        self.voice_guards.iter().any(|e| {
            e.node_id == engine_id
                && matches!(e._guard, PluginGuard::Clap(_) | PluginGuard::Vst3(_))
        })
    }

    /// Re-seeds a node's slider cache from the plugin's live values
    /// (after a state/preset load).
    fn refresh_voice_slider_cache(&mut self, doc_id: u64) {
        match self.node_guard(doc_id) {
            Some(PluginGuard::Clap(g)) => {
                let values = g.read_param_values();
                self.clap_voice_values.insert(doc_id, values);
            }
            Some(PluginGuard::Vst3(g)) => {
                let values = g.read_param_values();
                self.vst3_voice_values.insert(doc_id, values);
            }
            _ => {}
        }
    }

    /// Loads a preset into an instrument node: live `load_state` first (a
    /// refusing plugin must not touch the document), slider cache re-seed,
    /// then the blob into `VoiceSpec.state` as one undoable edit.
    fn load_voice_preset(&mut self, doc_id: u64, name: &str) {
        let Some(key) = self.voice_plugin_key(doc_id) else {
            return;
        };
        let Some(blob) = read_voice_preset_in(&preset_library_root(), &key, name) else {
            self.set_message(format!("Preset unreadable: {name}"));
            return;
        };
        let loaded = match self.node_guard(doc_id) {
            Some(PluginGuard::Clap(g)) => g.load_state(&blob).is_ok(),
            Some(PluginGuard::Vst3(g)) => g.load_state(&blob).is_ok(),
            _ => false,
        };
        if !loaded {
            self.set_message("Preset load failed: plugin rejected the state".to_string());
            return;
        }
        self.refresh_voice_slider_cache(doc_id);
        let before = self.begin_edit();
        if let Some(NodeDoc::Instrument { voice, .. }) =
            self.project_mut().graph.node_by_id_mut(doc_id)
        {
            match voice {
                VoiceSpec::Clap { state, .. } | VoiceSpec::Vst3 { state, .. } => *state = blob,
                VoiceSpec::Builtin | VoiceSpec::SoundFont { .. } => {}
            }
        }
        self.commit_edit("Load Preset", before);
        self.voice_preset_selected.insert(doc_id, name.to_string());
        self.set_message(format!("Loaded preset {name}"));
    }

    /// Saves an instrument node's current state blob as a named preset file
    /// (file operation — not an undoable edit, same class as project save).
    fn save_voice_preset_as(&mut self, doc_id: u64) {
        let Some(key) = self.voice_plugin_key(doc_id) else {
            return;
        };
        let Some(name) = sanitize_preset_name(&self.preset_name_input) else {
            self.set_message("Preset name is empty".to_string());
            return;
        };
        let blob = match self.node_guard(doc_id) {
            Some(PluginGuard::Clap(g)) => g.save_state().ok(),
            Some(PluginGuard::Vst3(g)) => g.save_state().ok(),
            _ => None,
        };
        let Some(blob) = blob else {
            self.set_message("Save preset failed: plugin state unavailable".to_string());
            return;
        };
        match save_voice_preset_in(&preset_library_root(), &key, &name, &blob) {
            Ok(_) => {
                self.voice_preset_selected.insert(doc_id, name.clone());
                self.set_message(format!("Preset saved: {name}"));
                log::info!("Preset saved: {key}/{name}");
            }
            Err(err) => {
                self.set_message(format!("Preset save failed: {err}"));
                log::error!("Preset save failed: {err}");
            }
        }
    }

    /// Resets an instrument node to the factory-default state captured at
    /// spawn time: live reload + slider cache re-seed + an undoable edit
    /// that clears the persisted blob (empty state = factory default on
    /// reload).
    fn reset_voice_preset(&mut self, doc_id: u64) {
        let Some(default_blob) = self.voice_default_state.get(&doc_id).cloned() else {
            self.set_message("Factory default unavailable this session".to_string());
            return;
        };
        let loaded = match self.node_guard(doc_id) {
            Some(PluginGuard::Clap(g)) => g.load_state(&default_blob).is_ok(),
            Some(PluginGuard::Vst3(g)) => g.load_state(&default_blob).is_ok(),
            _ => false,
        };
        if !loaded {
            self.set_message("Reset failed: plugin rejected the state".to_string());
            return;
        }
        self.refresh_voice_slider_cache(doc_id);
        let before = self.begin_edit();
        if let Some(NodeDoc::Instrument { voice, .. }) =
            self.project_mut().graph.node_by_id_mut(doc_id)
        {
            match voice {
                VoiceSpec::Clap { state, .. } | VoiceSpec::Vst3 { state, .. } => {
                    *state = Vec::new()
                }
                VoiceSpec::Builtin | VoiceSpec::SoundFont { .. } => {}
            }
        }
        self.commit_edit("Reset Preset", before);
        self.voice_preset_selected.remove(&doc_id);
        self.set_message("Reset to factory default".to_string());
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
                .push(glitcherz_model::CableDoc::audio(in_from, out_to));
            self.node_map.remove(&effect_doc);
            self.retire_guard(effect_engine);
            self.clap_effect_params.remove(&effect_doc);
            self.clap_effect_values.remove(&effect_doc);
            self.vst3_effect_params.remove(&effect_doc);
            self.vst3_effect_values.remove(&effect_doc);
        }
        self.commit_edit("Clear External Effects", before);
    }

    /// The track's external effect nodes as `(doc_id, display label, is_clap)`
    /// — the shared data source for the effect param panels and the
    /// automation target selector.
    fn track_external_fx(&self, track: usize) -> Vec<(u64, String, bool)> {
        self.project()
            .graph
            .nodes
            .iter()
            .filter(|n| n.track() == track)
            .filter_map(|n| match n {
                NodeDoc::Effect { node_id, voice, .. } => match voice {
                    VoiceSpec::Clap { plugin_id, .. } => Some((*node_id, plugin_id.clone(), true)),
                    VoiceSpec::Vst3 { path, .. } => Some((
                        *node_id,
                        std::path::Path::new(path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "VST3".to_string()),
                        false,
                    )),
                    VoiceSpec::Builtin | VoiceSpec::SoundFont { .. } => None,
                },
                _ => None,
            })
            .collect()
    }

    fn send_sequencer(&mut self, cmd: SequencerCommand) {
        self.engine.send(EngineCommand::Sequencer(cmd));
    }

    fn send_note_on(&mut self, note: u8, velocity: f32) {
        self.send_to_vk_targets(NoteEvent::On { note, velocity });
        self.held_notes.insert(note);
        self.current_note = Some(note);
    }

    fn send_note_off(&mut self, note: u8) {
        self.send_to_vk_targets(NoteEvent::Off { note });
        self.held_notes.remove(&note);
    }

    /// Delivers one live event to every destination of the keyboard input
    /// node (G6 — node-addressed, skips the track table). **No fallback**: an
    /// unbound or unconnected input is silent, and the status bar says so on
    /// the first key press rather than quietly playing the selected track.
    /// Index loop rather than an iterator so the per-target helper may take
    /// `&mut self`; the list is at most `MAX_MIDI_FANOUT` long.
    fn send_to_vk_targets(&mut self, event: NoteEvent) {
        if self.vk_targets.is_empty() {
            if matches!(event, NoteEvent::On { .. }) {
                self.set_message(
                    "Keyboard has no destination: cable the input node to an instrument (Ctrl+L)"
                        .to_string(),
                );
            }
            return;
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
    fn send_note_event_at(&mut self, doc: u64, event: NoteEvent) {
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
    fn refresh_vk_targets(&mut self) {
        let targets = {
            let doc = &self.project().graph;
            match doc.computer_keyboard() {
                Some(vk) => doc.keyboard_routes_from(vk),
                None => Vec::new(),
            }
        };
        if targets == self.vk_targets {
            return;
        }
        let old = std::mem::take(&mut self.vk_targets);
        let held: Vec<u8> = self.held_notes.iter().copied().collect();
        for doc in old {
            if targets.contains(&doc) {
                continue;
            }
            for note in held.iter().copied() {
                self.send_note_event_at(doc, NoteEvent::Off { note });
            }
        }
        self.vk_targets = targets;
    }

    /// Handle computer keyboard playing (FL Studio-style layout; Z/X shift octaves).
    ///
    /// While the piano roll owns keyboard focus, the computer keyboard
    /// drives the roll's command family (G/T/S/D/K...) instead — note-ons
    /// are suppressed; note-offs still pass so held notes never stick.
    /// Text input widgets (none yet besides DragValue edits) still need
    /// the same treatment.
    fn handle_keyboard_input(&mut self, ctx: &egui::Context) {
        let roll_owns_focus = self.roll_state.owns_keyboard_focus(ctx)
            || self.timeline_state.owns_keyboard_focus(ctx);
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
            // Global play/pause (U1): works regardless of which editor owns
            // focus, but not while a text field is being edited (it would
            // steal the space that ends the edit).
            if key == egui::Key::Space && pressed && !ctx.egui_wants_keyboard_input() {
                if self.engine.is_playing() {
                    self.engine.pause();
                } else {
                    self.send_sequencer(SequencerCommand::Play);
                }
                continue;
            }
            // Home / End: transport jump (U1) — same text-edit guard.
            if pressed && !ctx.egui_wants_keyboard_input() {
                match key {
                    egui::Key::Home => {
                        self.engine.seek_ticks(0.0);
                        continue;
                    }
                    egui::Key::End => {
                        let end = f64::from(self.arrangement_end_ticks());
                        self.engine.seek_ticks(end);
                        continue;
                    }
                    _ => {}
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

    fn transport_panel(&mut self, ui: &mut egui::Ui) {
        let playing = self.engine.is_playing();
        let response = egui::Panel::top("glitcherz_transport").show(ui, |ui| {
            ui.horizontal(|ui| {
                // U2: the file menu is the single entry point for project
                // work (the old inline Open / Save / Recent buttons live
                // here now).
                ui.menu_button("File", |ui| {
                    if ui
                        .add(egui::Button::new("New").shortcut_text("Ctrl+N"))
                        .clicked()
                    {
                        ui.close();
                        self.request_file_action(FileAction::New);
                    }
                    if ui
                        .add(egui::Button::new("Open…").shortcut_text("Ctrl+O"))
                        .clicked()
                    {
                        ui.close();
                        self.open_project_dialog();
                    }
                    ui.menu_button("Open Recent", |ui| {
                        if self.recent.is_empty() {
                            ui.weak("(empty)");
                        }
                        let recent = self.recent.clone();
                        for path in recent {
                            if ui.button(display_name(&path)).clicked() {
                                ui.close();
                                self.request_file_action(FileAction::Open(path));
                            }
                        }
                    });
                    ui.separator();
                    if ui
                        .add(egui::Button::new("Save").shortcut_text("Ctrl+S"))
                        .clicked()
                    {
                        ui.close();
                        self.save_project();
                    }
                    if ui
                        .add(egui::Button::new("Save As…").shortcut_text("Ctrl+Shift+S"))
                        .clicked()
                    {
                        ui.close();
                        self.save_project_as();
                    }
                    ui.separator();
                    if ui.button("Import MIDI…").clicked() {
                        ui.close();
                        self.import_midi_dialog();
                    }
                    if ui.button("Export MIDI…").clicked() {
                        ui.close();
                        self.export_midi_dialog();
                    }
                });
                ui.separator();
                // Transport group (U1): rewind / play-pause / stop /
                // skip-to-end, then the song-loop toggle and marker jumps.
                if ui
                    .button("⏮")
                    .on_hover_text("Return to start (Home)")
                    .clicked()
                {
                    self.engine.seek_ticks(0.0);
                }
                let play_label = if playing { "⏸ Pause" } else { "▶ Play" };
                let mut play_btn = egui::Button::new(play_label);
                if playing {
                    play_btn = play_btn.fill(egui::Color32::DARK_GREEN);
                }
                if ui.add(play_btn).on_hover_text("Space").clicked() {
                    if playing {
                        self.engine.pause();
                    } else {
                        self.send_sequencer(SequencerCommand::Play);
                    }
                }
                if ui
                    .button("■ Stop")
                    .on_hover_text("Stop and rewind")
                    .clicked()
                {
                    self.send_sequencer(SequencerCommand::Stop);
                }
                if ui.button("⏭").on_hover_text("Skip to end (End)").clicked() {
                    self.engine
                        .seek_ticks(f64::from(self.arrangement_end_ticks()));
                }
                let loop_on = self.project().metadata.loop_range.is_active();
                let loop_btn = egui::Button::new("Loop").selected(loop_on);
                if ui
                    .add(loop_btn)
                    .on_hover_text("Song loop (drag it out on the ruler with Ctrl)")
                    .clicked()
                {
                    let enable = !loop_on;
                    self.timeline_toggle_loop(enable);
                }
                if ui.button("‹").on_hover_text("Previous marker").clicked() {
                    self.seek_to_previous_marker();
                }
                if ui.button("›").on_hover_text("Next marker").clicked() {
                    self.seek_to_next_marker();
                }
                ui.separator();
                ui.label("Tempo");
                // U2: the value is read into a local first — writing the
                // document every frame would keep `Session::is_dirty` (and
                // the title's `*`) permanently true.
                let before = self.begin_param_edit();
                let mut tempo = self.project().metadata.tempo_bpm;
                let bpm = ui.add(
                    egui::DragValue::new(&mut tempo)
                        .range(*BPM_RANGE.start()..=*BPM_RANGE.end())
                        .speed(0.5)
                        .suffix(" BPM"),
                );
                if bpm.changed() {
                    self.project_mut().metadata.tempo_bpm = tempo;
                    // One drag = one undo step; keyboard nudges are separate steps.
                    if bpm.dragged() {
                        self.commit_param_edit("Tempo", before, "tempo");
                    } else {
                        self.commit_edit("Tempo", before);
                    }
                    self.send_sequencer(SequencerCommand::SetBpm(convert::to_f32(tempo)));
                }
                if bpm.drag_stopped() {
                    self.end_param_edit();
                }
                // U3b: a tempo curve overrides the transport, so show the
                // effective value it is actually running at (read-only — the
                // field above stays the user's base tempo).
                let live_bpm = self.engine.live_bpm();
                if self.engine.is_playing() && (live_bpm - convert::to_f32(tempo)).abs() > 0.05 {
                    ui.label(
                        egui::RichText::new(format!("→ {live_bpm:.1} auto"))
                            .small()
                            .color(egui::Color32::from_rgb(230, 190, 70)),
                    )
                    .on_hover_text("Tempo curve (Timeline In) is driving the clock");
                }
                ui.separator();
                // U3a: meter. Same read-local-then-write rule as tempo — the
                // dropdown must not touch the document every frame.
                ui.label("Meter");
                let meter = self.project().metadata.time_signature;
                let mut chosen = meter;
                egui::ComboBox::from_id_salt("glitcherz_meter")
                    .selected_text(meter.to_string())
                    .width(52.0)
                    .show_ui(ui, |ui| {
                        for (numerator, denominator) in METER_PRESETS {
                            let candidate =
                                glitcherz_model::TimeSignature::new(numerator, denominator);
                            ui.selectable_value(&mut chosen, candidate, candidate.to_string());
                        }
                    });
                if chosen != meter {
                    self.set_meter(chosen);
                }
                ui.separator();
                ui.weak("GlitcherZ — Rust DAW");
            });
        });
        // Chrome: hovering the transport releases any editor's focus.
        self.focus_router.register_neutral(response.response.rect);
    }

    /// Arrangement end in ticks (U1: the skip-to-end target) — the latest
    /// clip end across all tracks, rounded up to a bar (mirrors the
    /// engine's D16 rule); `0` when the arrangement is empty.
    fn arrangement_end_ticks(&self) -> u32 {
        let bar = u64::from(self.project().ticks_per_bar());
        let mut latest = 0u64;
        for track in &self.project().midi_tracks {
            for clip in &track.clips {
                latest = latest.max(u64::from(clip.start_ticks) + u64::from(clip.span_ticks));
            }
        }
        for track in &self.project().control_tracks {
            for clip in &track.clips {
                latest = latest.max(u64::from(clip.start_ticks) + u64::from(clip.span_ticks));
            }
        }
        let end = if latest == 0 {
            0
        } else {
            latest.div_ceil(bar) * bar
        };
        end.min(u64::from(u32::MAX)) as u32
    }

    /// Seeks to the nearest marker strictly before the playhead (U1);
    /// without one, back to the arrangement start.
    fn seek_to_previous_marker(&mut self) {
        let pos = f64::from(self.engine.musical_ticks());
        let target = self
            .project()
            .metadata
            .markers
            .iter()
            .rev()
            .find(|m| f64::from(m.tick) < pos - 1e-6)
            .map_or(0.0, |m| f64::from(m.tick));
        self.engine.seek_ticks(target);
    }

    /// Seeks to the nearest marker strictly after the playhead (U1);
    /// without one, to the arrangement end.
    fn seek_to_next_marker(&mut self) {
        let pos = f64::from(self.engine.musical_ticks());
        let target = self
            .project()
            .metadata
            .markers
            .iter()
            .find(|m| f64::from(m.tick) > pos + 1e-6)
            .map_or_else(
                || f64::from(self.arrangement_end_ticks()),
                |m| f64::from(m.tick),
            );
        self.engine.seek_ticks(target);
    }

    /// Arrangement timeline panel (timeline spec T3): rows = tracks, clip
    /// blocks with move/resize/insert/delete, double-click opens the editor.
    /// Main view tab bar (stacks under the transport panel).
    fn view_tabs(&mut self, ui: &mut egui::Ui) {
        let response = egui::Panel::top("glitcherz_views").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view, MainView::Timeline, "Timeline");
                ui.selectable_value(&mut self.view, MainView::Edit, "Edit");
                ui.selectable_value(&mut self.view, MainView::Patch, "Patch");
                ui.selectable_value(&mut self.view, MainView::Synth, "Synth / Mixer");
            });
        });
        // Chrome: switching tabs releases the previous view's focus.
        self.focus_router.register_neutral(response.response.rect);
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
                focus_router: router,
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
                    loop_range: &project.metadata.loop_range,
                    markers: &project.metadata.markers,
                    // U3a: the document's bar length, not the 4/4 constant.
                    bar_ticks: project.ticks_per_bar(),
                },
                router,
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
                // Double-click jumps straight to the editor.
                self.set_edit_target(EditTarget::Midi { track, clip });
                self.view = MainView::Edit;
            }
            timeline::TimelineAction::OpenControlEditor { track, clip } => {
                self.selected_auto = track;
                self.set_edit_target(EditTarget::Control { track, clip });
                self.view = MainView::Edit;
            }
            timeline::TimelineAction::AddMidiTrack => self.add_midi_track(),
            timeline::TimelineAction::RemoveLastMidiTrack => self.remove_last_midi_track(),
            timeline::TimelineAction::ReorderMidiTrack { from, to } => {
                self.reorder_midi_track(from, to)
            }
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
            timeline::TimelineAction::RemoveClipsBatch { clips } => {
                self.timeline_remove_clips_batch(clips)
            }
            timeline::TimelineAction::SplitClip {
                is_control,
                track,
                clip,
                at_ticks,
            } => self.timeline_split_clip(is_control, track, clip, at_ticks),
            timeline::TimelineAction::MoveClipsBatch { moves } => {
                self.timeline_move_clips_batch(moves)
            }
            timeline::TimelineAction::StretchClipsBatch { spans } => {
                self.timeline_stretch_clips_batch(spans)
            }
            timeline::TimelineAction::DuplicateClipsBatch { dups } => {
                self.timeline_duplicate_clips_batch(dups)
            }
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
        }
    }

    /// Sets the song loop range (U1): one undoable document edit, then the
    /// engine push (a disabled/degenerate range disables looping).
    fn timeline_set_loop(&mut self, start_ticks: u32, end_ticks: u32) {
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
    fn timeline_toggle_loop(&mut self, enabled: bool) {
        let before = self.begin_edit();
        self.project_mut().metadata.loop_range.enabled = enabled;
        self.commit_edit("Toggle Song Loop", before);
        self.push_loop_range();
    }

    /// Adds a timeline marker (U1): auto-named "M<n>", inserted in tick
    /// order. One undo step; the marker cap refuses with a status message.
    fn timeline_add_marker(&mut self, tick: u32) {
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
    fn timeline_remove_marker(&mut self, index: usize) {
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
    fn push_loop_range(&mut self) {
        let range = self.project().metadata.loop_range;
        let (start, end) = if range.is_active() {
            (range.start_ticks, range.end_ticks)
        } else {
            (0, 0)
        };
        self.engine.set_loop_range(start, end);
    }

    /// Pushes the document meter to the engine (U3a): the engine derives its
    /// bar length from it (arrangement-end rounding).
    fn push_time_signature(&mut self) {
        let meter = self.project().metadata.time_signature;
        self.send_sequencer(SequencerCommand::SetTimeSignature {
            numerator: meter.numerator,
            denominator: meter.denominator,
        });
    }

    /// Meter change from the transport (U3a): one undo step, and **no event
    /// moves** — only the bar-derived measures (grid, loop snapping,
    /// arrangement end, the ruler's bar numbers) change. Existing clips,
    /// markers and the loop range keep their ticks, so a loop may stop sitting
    /// on a bar line; the next manual drag snaps to the new grid.
    fn set_meter(&mut self, meter: glitcherz_model::TimeSignature) {
        if meter == self.project().metadata.time_signature {
            return;
        }
        let before = self.begin_edit();
        self.project_mut().metadata.time_signature = meter;
        self.commit_edit("Meter", before);
        self.push_time_signature();
        self.set_message(format!("Meter {meter}"));
    }

    /// Timeline clip edits: the view sends data, these apply it undoably.
    /// A drag frame goes through `commit_param_edit` with the gesture key, so
    /// one drag is one undo step.
    fn timeline_add_clip(&mut self, is_control: bool, track: usize, start_ticks: u32) {
        let before = self.begin_edit();
        // A new clip is one bar long — of the document's meter (U3a), not the
        // 4/4 constant the model default carries.
        let bar = self.project().ticks_per_bar();
        let added = if is_control {
            self.project_mut()
                .control_tracks
                .get_mut(track)
                .map(|t| {
                    if t.clips.len() < MAX_CLIPS_PER_TRACK {
                        let mut clip = ControlClip::new(start_ticks);
                        clip.span_ticks = bar;
                        clip.loop_ticks = bar;
                        t.clips.push(clip);
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
                        let mut clip = MidiClip::new(start_ticks);
                        clip.span_ticks = bar;
                        clip.loop_ticks = bar;
                        t.clips.push(clip);
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
                    let changed = c.start_ticks != start_ticks
                        || c.span_ticks != span_ticks
                        || c.loop_ticks != span_ticks;
                    c.start_ticks = start_ticks;
                    // Single-period clips (D14): the loop follows the span, so
                    // resizing exposes fresh empty bars instead of tiling.
                    c.span_ticks = span_ticks;
                    c.loop_ticks = span_ticks;
                    changed
                })
                .unwrap_or(false)
        } else {
            self.project_mut()
                .midi_tracks
                .get_mut(track)
                .and_then(|t| t.clips.get_mut(clip))
                .map(|c| {
                    let changed = c.start_ticks != start_ticks
                        || c.span_ticks != span_ticks
                        || c.loop_ticks != span_ticks;
                    c.start_ticks = start_ticks;
                    c.span_ticks = span_ticks;
                    c.loop_ticks = span_ticks;
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

    /// Deletes every clip in the list as ONE undo step. Per track the
    /// indices are applied descending so earlier removals cannot shift
    /// later ones (the timeline reports pre-removal indices).
    fn timeline_remove_clips_batch(&mut self, clips: Vec<timeline::ClipSel>) {
        if clips.is_empty() {
            return;
        }
        let before = self.begin_edit();
        let mut by_key: Vec<(bool, usize, Vec<usize>)> = Vec::new();
        for c in clips {
            if let Some((_, _, idxs)) = by_key
                .iter_mut()
                .find(|(k, t, _)| *k == c.is_control && *t == c.track)
            {
                idxs.push(c.clip);
            } else {
                by_key.push((c.is_control, c.track, vec![c.clip]));
            }
        }
        let mut touched = Vec::new();
        for (is_control, track, mut idxs) in by_key {
            idxs.sort_unstable();
            idxs.dedup();
            let removed = if is_control {
                self.project_mut()
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
                    .unwrap_or(0)
            } else {
                self.project_mut()
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
                    .unwrap_or(0)
            };
            if removed > 0 {
                touched.push((is_control, track));
            }
        }
        if !touched.is_empty() {
            self.commit_edit("Delete Clips", before);
            for (is_control, track) in touched {
                self.sync_clip_row(is_control, track);
            }
        }
    }

    /// Splits one clip at `at_ticks` as ONE undo step: the left half keeps
    /// start + the shortened span, the right half is inserted right after
    /// with cloned content (loop-based clips both play the same loop). The
    /// selection is patched for the index shift (split entry dropped, later
    /// indices on the row bumped).
    fn timeline_split_clip(&mut self, is_control: bool, track: usize, clip: usize, at_ticks: u32) {
        let before = self.begin_edit();
        let split = if is_control {
            self.project_mut()
                .control_tracks
                .get_mut(track)
                .map(|t| timeline::split_clip_in(&mut t.clips, at_ticks, clip))
                .unwrap_or(false)
        } else {
            self.project_mut()
                .midi_tracks
                .get_mut(track)
                .map(|t| timeline::split_clip_in(&mut t.clips, at_ticks, clip))
                .unwrap_or(false)
        };
        if split {
            self.commit_edit("Split Clip", before);
            self.sync_clip_row(is_control, track);
            let sel = &mut self.timeline_state.selection;
            sel.retain(|s| s.is_control != is_control || s.track != track || s.clip != clip);
            for s in sel.iter_mut() {
                if s.is_control == is_control && s.track == track && s.clip > clip {
                    s.clip += 1;
                }
            }
        }
    }

    /// G-modal finalize: moves every clip in ONE undo step (may cross
    /// tracks of the same kind). Sources are removed descending per track,
    /// then re-appended at their destinations with the new start —
    /// appending cannot shift any remaining index.
    fn timeline_move_clips_batch(&mut self, moves: Vec<timeline::ClipMove>) {
        if moves.is_empty() {
            return;
        }
        let before = self.begin_edit();
        let mut touched = Vec::new();
        {
            let project = self.project_mut();
            for (is_control, kind_moves) in [
                (
                    false,
                    moves
                        .iter()
                        .filter(|m| !m.from.is_control)
                        .collect::<Vec<_>>(),
                ),
                (
                    true,
                    moves
                        .iter()
                        .filter(|m| m.from.is_control)
                        .collect::<Vec<_>>(),
                ),
            ] {
                if kind_moves.is_empty() {
                    continue;
                }
                let spec: Vec<(usize, usize, usize, u32)> = kind_moves
                    .iter()
                    .map(|m| (m.from.track, m.from.clip, m.to_track, m.start_ticks))
                    .collect();
                let rows = if is_control {
                    let mut tables: Vec<&mut Vec<ControlClip>> = project
                        .control_tracks
                        .iter_mut()
                        .map(|t| &mut t.clips)
                        .collect();
                    relocate_in(tables.as_mut_slice(), &spec)
                } else {
                    let mut tables: Vec<&mut Vec<MidiClip>> = project
                        .midi_tracks
                        .iter_mut()
                        .map(|t| &mut t.clips)
                        .collect();
                    relocate_in(tables.as_mut_slice(), &spec)
                };
                for r in rows {
                    touched.push((is_control, r));
                }
            }
        }
        if !touched.is_empty() {
            self.commit_edit("Move Clips", before);
            for (is_control, track) in touched {
                self.sync_clip_row(is_control, track);
            }
        }
    }

    /// T-modal finalize: stretches every clip in ONE undo step (same
    /// track, bar-aligned spans validated by the view).
    fn timeline_stretch_clips_batch(&mut self, spans: Vec<timeline::ClipSpanEdit>) {
        if spans.is_empty() {
            return;
        }
        let before = self.begin_edit();
        let mut touched = Vec::new();
        {
            let project = self.project_mut();
            for e in &spans {
                let changed = if e.sel.is_control {
                    project
                        .control_tracks
                        .get_mut(e.sel.track)
                        .and_then(|t| t.clips.get_mut(e.sel.clip))
                        .map(|c| {
                            c.start_ticks = e.start_ticks;
                            // Single-period clips (D14): the loop follows the span.
                            c.span_ticks = e.span_ticks;
                            c.loop_ticks = e.span_ticks;
                        })
                        .is_some()
                } else {
                    project
                        .midi_tracks
                        .get_mut(e.sel.track)
                        .and_then(|t| t.clips.get_mut(e.sel.clip))
                        .map(|c| {
                            c.start_ticks = e.start_ticks;
                            c.span_ticks = e.span_ticks;
                            c.loop_ticks = e.span_ticks;
                        })
                        .is_some()
                };
                if changed && !touched.contains(&(e.sel.is_control, e.sel.track)) {
                    touched.push((e.sel.is_control, e.sel.track));
                }
            }
        }
        if !touched.is_empty() {
            self.commit_edit("Stretch Clips", before);
            for (is_control, track) in touched {
                self.sync_clip_row(is_control, track);
            }
        }
    }

    /// Shift+D finalize: clones every source clip's content onto its
    /// target row/start in ONE undo step; the originals are untouched.
    /// The selection then moves to the new copies (they are appended, so
    /// their indices are the destination table's former lengths).
    fn timeline_duplicate_clips_batch(&mut self, dups: Vec<timeline::ClipDup>) {
        if dups.is_empty() {
            return;
        }
        let before = self.begin_edit();
        let mut new_sel = Vec::new();
        let mut touched = Vec::new();
        {
            let project = self.project_mut();
            for d in &dups {
                if d.from.is_control {
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
                        is_control: true,
                        track: d.to_track,
                        clip: index,
                    });
                    if !touched.contains(&(true, d.to_track)) {
                        touched.push((true, d.to_track));
                    }
                } else {
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
                        is_control: false,
                        track: d.to_track,
                        clip: index,
                    });
                    if !touched.contains(&(false, d.to_track)) {
                        touched.push((false, d.to_track));
                    }
                }
            }
        }
        if !new_sel.is_empty() {
            self.commit_edit("Duplicate Clips", before);
            for (is_control, track) in touched {
                self.sync_clip_row(is_control, track);
            }
            self.timeline_state.selection = new_sel;
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

    /// Edit tab: the clip editor as a main view (double-clicking a clip in
    /// the timeline jumps here). With no targeted clip it offers the
    /// selected track's first clip.
    fn edit_panel(&mut self, ui: &mut egui::Ui) {
        let Some(target) = self.edit_clip else {
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.heading("Edit");
                ui.add_space(8.0);
                ui.label("Double-click a clip in the Timeline to edit it here.");
                ui.add_space(12.0);
                if !self.project().midi_tracks.is_empty() {
                    let track = self
                        .selected_track
                        .min(self.project().midi_tracks.len() - 1);
                    let name = if self.project().midi_tracks[track].name.is_empty() {
                        format!("Track {}", track + 1)
                    } else {
                        self.project().midi_tracks[track].name.clone()
                    };
                    if ui.button(format!("Edit {name} · clip 1")).clicked() {
                        self.set_edit_target(EditTarget::Midi { track, clip: 0 });
                    }
                }
            });
            return;
        };
        egui::ScrollArea::vertical().show(ui, |ui| match target {
            EditTarget::Midi { track, clip } => self.midi_editor_body(ui, track, clip),
            EditTarget::Control { track, clip } => self.control_editor_body(ui, track, clip),
        });
    }

    /// U9 E2: thin banner shown while the startup plugin scan still has
    /// modules queued.
    ///
    /// It names the module the frame will block on at its end (spec D14), so
    /// a plugin that hangs inside its metadata read leaves its own name on
    /// screen — the only diagnostic in-process scanning can offer.
    fn plugin_scan_banner(&self, ui: &mut egui::Ui) {
        let Some(upcoming) = self.plugin_catalog.upcoming() else {
            return;
        };
        let (done, total) = self.plugin_catalog.progress();
        let name = upcoming
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| upcoming.path.display().to_string());
        egui::Panel::top("glitcherz_plugin_scan").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                        .desired_width(140.0)
                        .text(format!("{done}/{total}")),
                );
                ui.weak(format!("Scanning plug-ins — {name}"));
            });
        });
    }

    /// U9 E2: scans exactly one module at the **end** of the frame (spec D14).
    ///
    /// A module that fails to load yields nothing; progress advances either
    /// way so the queue always drains.
    fn plugin_scan_step(&mut self) {
        let Some(candidate) = self.plugin_catalog.take_next() else {
            return;
        };
        let entries = plugin_catalog::scan_module(&candidate);
        self.plugin_catalog.accept(entries);
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
        // G6: where the computer keyboard goes (the on-screen keyboard panel
        // is gone — the patch decides, so the status bar says where).
        let keys_text = if self.vk_targets.is_empty() {
            "none".to_string()
        } else {
            let mut tracks: Vec<usize> = self
                .vk_targets
                .iter()
                .filter_map(|doc| self.project().graph.node_by_id(*doc).map(NodeDoc::track))
                .map(|track| track + 1)
                .collect();
            tracks.sort_unstable();
            tracks.dedup();
            let list = tracks
                .iter()
                .map(|t| format!("T{t}"))
                .collect::<Vec<_>>()
                .join(" ");
            format!("{} synth(s) {list}", self.vk_targets.len())
        };
        // Status message (disappears automatically after a timeout).
        let message = self
            .status_message
            .take_if(|(_, at)| at.elapsed() < STATUS_MESSAGE_TTL);

        let response = egui::Panel::bottom("glitcherz_status").show(ui, |ui| {
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
                ui.label(format!("Keys → {keys_text}"));
                ui.separator();
                // Musical transport position (bar.beat, from the sequencer's tick counter —
                // frozen while stopped). The raw seconds counter is engine uptime (audio
                // callback keeps rendering silent blocks), kept as a secondary diagnostic.
                let ticks = self.engine.musical_ticks();
                // U3a: bar length and beat unit both come from the document's
                // meter, so 6/8 counts six beats while 3/4 counts three.
                let meter = self.project().metadata.time_signature;
                let tpb = meter.ticks_per_bar() as f32;
                let bar = (ticks / tpb).floor().max(0.0);
                let beat = (ticks - bar * tpb) / meter.ticks_per_beat() as f32 + 1.0;
                ui.label(format!("Pos {}.{}", bar as u32 + 1, beat));
                ui.separator();
                ui.label(format!("Uptime {:.1} s", position));
                if std::env::var_os("GLITCHERZ_FOCUS_DEBUG").is_some() {
                    ui.separator();
                    let ctx = ui.ctx().clone();
                    let (pos, down) =
                        ctx.input(|i| (i.pointer.latest_pos(), i.pointer.any_down()));
                    let focused = ctx.memory(|m| m.focused());
                    let (kb, nz) = self.focus_router.registry_size();
                    let retained = self.focus_router.last_known_pos();
                    ui.monospace(format!(
                        "focus: live={pos:?} retained={retained:?} down={down} owner={focused:?} kb={kb} nz={nz}"
                    ));
                }
                if let Some((text, _)) = message {
                    ui.separator();
                    ui.colored_label(egui::Color32::LIGHT_BLUE, text);
                }
            });
        });
        // Chrome: hovering the status bar releases any editor's focus.
        self.focus_router.register_neutral(response.response.rect);
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
            let sel = self.selected_track.min(self.project().midi_tracks.len() - 1);
            self.selected_track = sel;
            // Panel editing target (spec `plugin-gui.md` §6): the patch
            // selection's first editable document node, falling back to the
            // selected track's first-match instrument (pre-G4 behavior).
            let target = self.panel_target_node();
            let target_doc = target.and_then(|id| self.project().graph.node_by_id(id).cloned());
            let title = match (&target_doc, target) {
                (Some(NodeDoc::Effect { .. }), Some(id)) => {
                    format!("Synth — Effect #{id} · Track {}", sel + 1)
                }
                (Some(NodeDoc::Mixer { .. }), Some(id)) => format!("Synth — Mixer #{id}"),
                (Some(NodeDoc::Instrument { .. }), Some(id)) => {
                    format!("Synth — Instrument #{id} · Track {}", sel + 1)
                }
                _ => {
                    let track_title = if self.project().midi_tracks[sel].name.is_empty() {
                        format!("Track {}", sel + 1)
                    } else {
                        self.project().midi_tracks[sel].name.clone()
                    };
                    format!("Synth — {track_title}")
                }
            };
            ui.heading(title);
            // Embedded plugin editor (spec `plugin-gui.md` §4): CLAP
            // instruments only (v1). The toggle requests/destroys the
            // session; the framed area below reserves layout space whose
            // rect drives the native container window position (the real
            // plugin GUI draws on top of it as a native child window).
            // Deliberately **outside** the ScrollArea: a native child
            // window cannot be clipped by egui, so a scrolling editor
            // would smear over neighbouring UI (ghosting) — the editor
            // stays pinned at the top and only the parameter stack below
            // scrolls.
            {
                let editor_here = self
                    .plugin_editor
                    .as_ref()
                    .map(|e| Some(e.doc_node) == target)
                    .unwrap_or(false);
                let has_editor = target.is_some_and(|t| self.node_guard_has_editor(t));
                if has_editor {
                    let label = if editor_here {
                        "Close Editor"
                    } else {
                        "Show Editor"
                    };
                    if ui.button(label).clicked() {
                        if editor_here {
                            self.editor_user_open = false;
                            self.close_plugin_editor();
                        } else {
                            self.editor_user_open = true;
                            self.plugin_editor_wants_open = true;
                        }
                    }
                }
                if let Some(editor) = self.plugin_editor.as_mut() {
                    if Some(editor.doc_node) == target {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(editor.size.0.max(1.0), editor.size.1.max(1.0)),
                                egui::Sense::hover(),
                            );
                            editor.rect = Some(rect);
                        });
                    }
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
                let sf2_state = self
                    .project()
                    .graph
                    .node_by_id(instr_id)
                    .and_then(|n| match n {
                        NodeDoc::Instrument {
                            voice:
                                VoiceSpec::SoundFont {
                                    path,
                                    bank,
                                    program,
                                },
                            ..
                        } => Some((path.clone(), *bank, *program)),
                        _ => None,
                    });
                if let Some((sf2_path, sf2_bank, sf2_program)) = sf2_state {
                    // Snapshot the preset list (releases the library borrow
                    // before the picker's undoable edit runs).
                    let presets: Option<Vec<(u8, u8, String)>> = self
                        .sf2_library
                        .as_ref()
                        .filter(|lib| lib.path() == std::path::Path::new(&sf2_path))
                        .map(|lib| {
                            lib.presets()
                                .iter()
                                .map(|p| (p.bank, p.program, p.name.clone()))
                                .collect()
                        });
                    match presets {
                        Some(presets) => {
                            // Bank-grouped picker (LMMS-style): the bank
                            // selector narrows the patch list; patches are
                            // ordered by program (GM order) inside a bank.
                            let mut banks: std::collections::BTreeMap<u8, Vec<(u8, String)>> =
                                std::collections::BTreeMap::new();
                            for (bank, program, name) in &presets {
                                banks.entry(*bank).or_default().push((*program, name.clone()));
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
                                egui::ComboBox::from_id_salt(format!("sf2_bank_{instr_id}"))
                                    .selected_text(format!("Bank {sf2_bank}"))
                                    .show_ui(ui, |ui| {
                                        for (bank, _) in &bank_list {
                                            if ui
                                                .selectable_label(
                                                    *bank == sf2_bank,
                                                    format!("Bank {bank}"),
                                                )
                                                .clicked()
                                            {
                                                bank_picked = Some(*bank);
                                            }
                                        }
                                    });
                                let patch_label = match current_name(sf2_bank, sf2_program) {
                                    Some(name) => format!("{sf2_program}. {name}"),
                                    None => format!("{sf2_bank}:{sf2_program}"),
                                };
                                egui::ComboBox::from_id_salt(format!("sf2_preset_{instr_id}"))
                                    .selected_text(patch_label)
                                    .show_ui(ui, |ui| {
                                        if let Some((_, patches)) =
                                            bank_list.iter().find(|(b, _)| *b == sf2_bank)
                                        {
                                            for (program, name) in patches {
                                                if ui
                                                    .selectable_label(
                                                        *program == sf2_program,
                                                        format!("{program}. {name}"),
                                                    )
                                                    .clicked()
                                                {
                                                    preset_picked = Some((sf2_bank, *program));
                                                }
                                            }
                                        }
                                    });
                                if let Some(bank) = bank_picked {
                                    if bank != sf2_bank {
                                        // Keep the program number when the new
                                        // bank has it; otherwise fall back to
                                        // the bank's first patch.
                                        let target = bank_list
                                            .iter()
                                            .find(|(b, _)| *b == bank)
                                            .and_then(|(_, patches)| {
                                                patches
                                                    .iter()
                                                    .find(|(p, _)| *p == sf2_program)
                                                    .map(|_| sf2_program)
                                                    .or_else(|| {
                                                        patches.first().map(|(p, _)| *p)
                                                    })
                                            });
                                        if let Some(program) = target {
                                            preset_picked = Some((bank, program));
                                        }
                                    }
                                }
                                if let Some((bank, program)) = preset_picked {
                                    self.set_soundfont_preset(instr_id, bank, program);
                                }
                            });
                        }
                        None => {
                            ui.weak("(soundfont file not loaded this session — reopen it)");
                        }
                    }
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
                _ => self.track_external_fx(sel),
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

            // CLAP plugin parameter panel: enumerated via the params extension at instantiation.
            // Sliders send raw plugin-range values through the `PluginParam` bridge (applied at
            // the next `process`, block granularity). Rows are virtualized (`show_rows`) and
            // grouped by the CLAP `module` path (Dexed's OP1/OP2/… sections) — the label shows
            // `module / name` and an empty-filter view sorts by module. Current values live in
            // the UI session only; a fresh plugin load resets them to the plugin defaults.
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
            ui.weak("Node graph: instrument -> delay -> mixer channels -> master (doc-driven, changes go live)");
            ui.add_space(8.0);

            // Filter / envelope: instrument-targeted (G4 — the panel
            // target's node fields; the no-selection fallback resolves to
            // the selected track's instrument node anyway).
            let instr_target = match (&target_doc, target) {
                (Some(NodeDoc::Instrument { .. }), Some(id)) => Some(id),
                _ => None,
            };
            // Delay sliders: an Effect panel target edits that node; the
            // instrument target (and the track fallback) edits the track's
            // built-in delay (pre-G4 behavior); a Mixer target shows none
            // (the console below owns mixer parameters).
            let delay_target: Option<u64> = match (&target_doc, target) {
                (Some(NodeDoc::Effect { .. }), Some(id)) => Some(id),
                (Some(NodeDoc::Mixer { .. }), _) => None,
                _ => self
                    .project()
                    .graph
                    .builtin_delay_of_track(sel)
                    .map(NodeDoc::node_id),
            };
            egui::Grid::new("synth_params")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    if let Some(instr_id) = instr_target {
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
                    ui.horizontal(|ui| {
                        let mode = self.synth_of_node(instr_id).unwrap_or_default().filter_mode;
                        for (name, value) in [("Low-pass", 0.0), ("High-pass", 1.0), ("Band-pass", 2.0)] {
                            if ui
                                .selectable_label((mode - value).abs() < 0.5, name)
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

                    if let Some(delay_id) = delay_target {
                        ui.label("Delay");
                        self.delay_sliders(ui, delay_id);
                        ui.end_row();
                    }

                    ui.label("Master Gain");
                    // U2: same rule as the tempo field — no document write
                    // unless the slider actually moved, or the title would
                    // claim unsaved changes forever.
                    let before = self.begin_param_edit();
                    let mut master_gain = self.project().master_gain;
                    let r = ui.add(egui::Slider::new(&mut master_gain, 0.0..=1.5).text("Linear"));
                    if r.changed() {
                        self.project_mut().master_gain = master_gain;
                        self.commit_param_edit("Master Gain", before, "master-gain");
                        self.engine.send(EngineCommand::SetParam {
                            id: ParamId::MasterGain,
                            value: master_gain,
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

    /// ADSR four sliders (local copies + write-back, avoiding borrow
    /// conflicts), bound to the target instrument node (G4).
    fn adsr_sliders(&mut self, ui: &mut egui::Ui, doc_id: u64) {
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
    fn delay_sliders(&mut self, ui: &mut egui::Ui, doc_id: u64) {
        let delay = self.delay_of_node(doc_id).unwrap_or_default();
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
                    self.send_node_param(doc_id, id, *value);
                }
            }
        });
        let before = self.begin_param_edit();
        if let Some(delay) = self.delay_mut_node(doc_id) {
            delay.mix = mix;
            delay.time_s = time;
            delay.feedback = feedback;
        }
        self.commit_param_edit("Delay Params", before, "delay-params");
    }

    /// Send one document node's parameter to the audio thread (G4 node
    /// addressing; no-op when the node has no engine counterpart).
    fn send_node_param(&mut self, doc_id: u64, id: ParamId, value: f32) {
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
    fn sync_vst3_host_param(&mut self, doc_id: u64, id: u32, value: f64) {
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
    fn apply_inline_param(&mut self, node: u64, param: patch::InlineParamId, value: f32) {
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
                patch::InlineParam::DelayTime
                | patch::InlineParam::DelayFeedback
                | patch::InlineParam::DelayMix => {
                    let before = self.begin_param_edit();
                    if let Some(d) = self.delay_mut_node(node) {
                        match p {
                            patch::InlineParam::DelayTime => d.time_s = value,
                            patch::InlineParam::DelayFeedback => d.feedback = value,
                            patch::InlineParam::DelayMix => d.mix = value,
                            _ => {}
                        }
                    }
                    self.commit_param_edit("Delay Params", before, "delay-params");
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
            .push(glitcherz_model::MidiTrack::default());
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
            .push(glitcherz_model::CableDoc::audio(voice_id, delay_id));
        self.project_mut()
            .graph
            .cables
            .push(glitcherz_model::CableDoc::midi(track, voice_id));
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
                    .push(glitcherz_model::CableDoc::audio_to_channel(
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
        self.push_node_params();
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
        // The editor may have targeted the removed track's clip.
        if let Some(EditTarget::Midi { track, .. }) = self.edit_clip
            && track == last
        {
            self.edit_clip = None;
        }
        self.commit_edit("Remove MIDI Track", before);
    }

    /// Reorders the MIDI tracks (the track at `from` is re-inserted so it
    /// ends up just before original index `to`): the track table, the
    /// per-track node `track` fields and the `MidiTrack` cable indices all
    /// move together as one undo step, then the engine is rebuilt from the
    /// document — the same path undo/redo already takes. Mixer / Keyboard /
    /// Feedback `track` fields are patch-layout grouping only and stay put
    /// (the same exclusion `remove_last_midi_track` applies).
    fn reorder_midi_track(&mut self, from: usize, to: usize) {
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
        if let Some(EditTarget::Midi { track, .. }) = &mut self.edit_clip
            && *track < len
        {
            *track = perm[*track];
        }
        for sel in &mut self.timeline_state.selection {
            if !sel.is_control && sel.track < len {
                sel.track = perm[sel.track];
            }
        }
        self.apply_document();
        self.commit_edit("Reorder Tracks", before);
    }

    /// Piano roll hint (the editor itself lives in the Edit tab; double-click
    /// a MIDI clip on the arrangement timeline to jump there).
    fn clip_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.add_space(4.0);
        ui.weak(
            "Piano roll: double-click a MIDI clip in the Arrangement timeline \
             (bottom panel) — it opens in the Edit tab.",
        );
    }

    /// Points the Edit tab at `target`, dropping any in-flight roll gesture:
    /// selection indices refer to the clip that was being edited.
    fn set_edit_target(&mut self, target: EditTarget) {
        self.roll_state.clear_gesture();
        self.edit_clip = Some(target);
    }

    /// Body of the piano-roll editor (Edit tab): edits the targeted clip,
    /// pushes edits to the audio thread immediately.
    fn midi_editor_body(&mut self, ui: &mut egui::Ui, track: usize, clip_index: usize) {
        if self.project().midi_tracks.is_empty() {
            // The target track was removed underneath the editor.
            self.edit_clip = None;
            return;
        }
        let sel = track.min(self.project().midi_tracks.len() - 1);
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
            if ui
                .add_enabled(clip_index > 0, egui::Button::new("←"))
                .on_hover_text("Previous clip")
                .clicked()
            {
                self.set_edit_target(EditTarget::Midi {
                    track: sel,
                    clip: clip_index - 1,
                });
            }
            if ui
                .add_enabled(clip_index + 1 < clip_count, egui::Button::new("→"))
                .on_hover_text("Next clip")
                .clicked()
            {
                self.set_edit_target(EditTarget::Midi {
                    track: sel,
                    clip: clip_index + 1,
                });
            }
            if ui.button("× Close").clicked() {
                self.edit_clip = None;
            }
        });
        let playhead = self.engine.musical_ticks();
        let playing = self.engine.is_playing();
        // U3a: the roll's bar lines follow the document meter; read it before
        // the destructuring below (which borrows `self` field by field).
        let bar_ticks = self.project().ticks_per_bar();
        let intent = {
            // Destructured: `project()` borrows all of `self`, which would
            // collide with the `&mut self.roll_state` below.
            let Self {
                session,
                roll_state,
                focus_router: router,
                ..
            } = self;
            let clip = &session.project().midi_tracks[sel].clips[clip_index];
            piano_roll::show(ui, clip, roll_state, playhead, playing, bar_ticks, router)
        };
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

        // Event List (M3.6): spreadsheet-style precision editing under the roll.
        egui::CollapsingHeader::new("Event List")
            .default_open(false)
            .show(ui, |ui| {
                let list_intent = {
                    let clip = &self.project().midi_tracks[sel].clips[clip_index];
                    event_list::show(ui, clip)
                };
                if let Some(list_intent) = list_intent {
                    self.apply_event_list_intent(sel, clip_index, list_intent);
                }
            });
    }

    /// Runs an Event List edit as a command (M1.4 contract).
    fn apply_event_list_intent(
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
    fn apply_roll_intent(&mut self, track: usize, clip: usize, intent: piano_roll::RollIntent) {
        use piano_roll::RollIntent;
        use serde_json::json;

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
    fn reselect_from(&mut self, outcome: &glitcherz_command::Result<serde_json::Value>) {
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
        // parameters) plus the global song tempo. Mixer-fader automation is
        // deferred (P5f decision). Options carry the **document** node_id (the
        // cable's persisted identity), mapped when dispatching.
        let mut options: Vec<(String, AutoTarget, AutoParam)> = Vec::new();
        // U3b: song tempo is always offered — the conductor terminal is a
        // fixed entity, so it needs no ownership check.
        options.push((
            "Song Tempo (BPM)".to_string(),
            AutoTarget::TimelineIn,
            AutoParam::Tempo,
        ));
        if let Some(voice_doc) = self
            .project()
            .graph
            .instrument_of_track(sel)
            .map(NodeDoc::node_id)
        {
            options.push((
                "Cutoff".to_string(),
                AutoTarget::Node(voice_doc),
                AutoParam::FilterCutoff,
            ));
            if let Some(params) = self.clap_voice_params.get(&voice_doc) {
                for param in params {
                    let label = if param.name.is_empty() {
                        format!("Param {}", param.id)
                    } else {
                        param.name.clone()
                    };
                    options.push((
                        format!("CLAP {label}"),
                        AutoTarget::Node(voice_doc),
                        AutoParam::PluginParam {
                            param_id: param.id,
                            name: label,
                        },
                    ));
                }
            }
            if let Some(params) = self.voice_params.get(&voice_doc) {
                for param in params {
                    let label = if param.title.is_empty() {
                        format!("Param {}", param.id)
                    } else {
                        param.title.clone()
                    };
                    options.push((
                        format!("VST3 {label}"),
                        AutoTarget::Node(voice_doc),
                        AutoParam::PluginParam {
                            param_id: param.id,
                            name: label,
                        },
                    ));
                }
            }
        }

        // External effect parameters (node-addressed endpoints land on the
        // effect node itself; the same CLAP raw / VST3 normalized domain
        // split as the instruments).
        for (fx_doc, fx_label, is_clap) in self.track_external_fx(sel) {
            if is_clap {
                if let Some(params) = self.clap_effect_params.get(&fx_doc) {
                    for param in params {
                        let label = if param.name.is_empty() {
                            format!("Param {}", param.id)
                        } else {
                            param.name.clone()
                        };
                        options.push((
                            format!("FX CLAP {fx_label} · {label}"),
                            AutoTarget::Node(fx_doc),
                            AutoParam::PluginParam {
                                param_id: param.id,
                                name: label,
                            },
                        ));
                    }
                }
            } else if let Some(params) = self.vst3_effect_params.get(&fx_doc) {
                for param in params {
                    let label = if param.title.is_empty() {
                        format!("Param {}", param.id)
                    } else {
                        param.title.clone()
                    };
                    options.push((
                        format!("FX VST3 {fx_label} · {label}"),
                        AutoTarget::Node(fx_doc),
                        AutoParam::PluginParam {
                            param_id: param.id,
                            name: label,
                        },
                    ));
                }
            }
        }

        let mut picked: Option<(AutoTarget, AutoParam)> = None;
        ui.horizontal(|ui| {
            ui.label("Target");
            ui.menu_button("Pick Parameter…", |ui| {
                // 2000+ parameter plugins make an unfiltered menu unusable:
                // a filter box + virtualized rows keep it responsive.
                ui.add(
                    egui::TextEdit::singleline(&mut self.auto_param_filter)
                        .hint_text("Filter parameters…"),
                );
                let needle = self.auto_param_filter.to_lowercase();
                let visible: Vec<usize> = options
                    .iter()
                    .enumerate()
                    .filter(|(_, (label, _, _))| {
                        needle.is_empty() || label.to_lowercase().contains(&needle)
                    })
                    .map(|(i, _)| i)
                    .collect();
                let row_h = ui.text_style_height(&egui::TextStyle::Body);
                if visible.is_empty() {
                    ui.weak("(no match)");
                }
                egui::ScrollArea::vertical().max_height(320.0).show_rows(
                    ui,
                    row_h,
                    visible.len(),
                    |ui, range| {
                        for row in range {
                            let (label, target, param) = &options[visible[row]];
                            if ui.button(label).clicked() {
                                picked = Some((*target, param.clone()));
                                ui.close();
                            }
                        }
                    },
                );
            });
            if let Some((target, param)) = picked {
                self.set_auto_endpoint(index, target, param);
            }
            // Current endpoint display.
            let current = self
                .project()
                .graph
                .auto_target(index)
                .map(|(port, param)| match (port, param) {
                    (PortRef::TimelineIn, _) => "Song Tempo".to_string(),
                    (_, AutoParam::TrackGain) => "Volume".to_string(),
                    (_, AutoParam::TrackPan) => "Pan".to_string(),
                    (_, AutoParam::FilterCutoff) => "Cutoff".to_string(),
                    (_, AutoParam::PluginParam { name, .. }) => name.clone(),
                    (_, AutoParam::Tempo) => "Song Tempo".to_string(),
                })
                .unwrap_or_else(|| "Not connected".to_string());
            ui.weak(format!("-> {current}"));
        });

        // The curve editor lives in the Edit tab (double-click a control clip
        // on the arrangement timeline to jump there); the clip's own
        // `loop_ticks` sets the editing width.
        ui.weak(
            "Curve editor: double-click a control clip in the Arrangement \
             timeline (bottom panel) — it opens in the Edit tab.",
        );
    }

    /// Body of the curve editor (Edit tab): edits the targeted control clip,
    /// pushes edits to the audio thread immediately.
    fn control_editor_body(&mut self, ui: &mut egui::Ui, index: usize, clip_index: usize) {
        if self.project().control_tracks.is_empty() {
            // The target track was removed underneath the editor.
            self.edit_clip = None;
            return;
        }
        let playhead = self.engine.musical_ticks();
        let playing = self.engine.is_playing();
        // Header: track name + clip navigation + close.
        let track_count = self.project().control_tracks.len();
        let index = index.min(track_count.saturating_sub(1));
        let track_name = self
            .project()
            .control_tracks
            .get(index)
            .map(|t| {
                if t.name.is_empty() {
                    format!("Control {}", index + 1)
                } else {
                    t.name.clone()
                }
            })
            .unwrap_or_else(|| format!("Control {}", index + 1));
        let clip_count = self
            .project()
            .control_tracks
            .get(index)
            .map(|t| t.clips.len())
            .unwrap_or(0);
        let clip_index = if clip_count == 0 {
            0
        } else {
            clip_index.min(clip_count - 1)
        };
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
                self.set_edit_target(EditTarget::Control {
                    track: index,
                    clip: clip_index - 1,
                });
            }
            if ui
                .add_enabled(clip_index + 1 < clip_count, egui::Button::new("→"))
                .on_hover_text("Next clip")
                .clicked()
            {
                self.set_edit_target(EditTarget::Control {
                    track: index,
                    clip: clip_index + 1,
                });
            }
            if ui.button("× Close").clicked() {
                self.edit_clip = None;
            }
        });
        let axis_param = self
            .project()
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
                .push(glitcherz_model::ControlClip::new(0));
            self.commit_edit("New Clip", before);
            self.send_control_clips(index);
            return;
        }
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
                clip.upsert_point(glitcherz_model::AutoPoint { tick, value });
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
}

impl eframe::App for GlitcherZApp {
    // eframe persistence: called on window close and periodically, saving the recent files list.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(
            storage,
            UI_STATE_KEY,
            &UiState {
                recent_files: self.recent.clone(),
                recent_plugins: self.recent_plugins.clone(),
            },
        );
        // U9 E2: the plugin catalog (spec `plugin-browser.md` D11). A partial
        // catalog is still a valid cache — the next launch keeps the scanned
        // modules and replans the rest.
        if let Some(text) = plugin_catalog::encode(&self.plugin_catalog.to_cache()) {
            storage.set_string(PLUGIN_CACHE_KEY, text);
        }
    }

    // eframe 0.36: the App trait entry point changed from update(ctx) to ui(&mut Ui);
    // top/bottom panels act directly on the root Ui, and the remaining space is the central
    // region.
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            log::info!("UI first frame rendered");
        }

        // Hover-follow focus resolves first, using the rects registered
        // last frame, so every `has_focus()` check below (including the
        // keyboard handling) sees the hover decision this very frame.
        self.focus_router.begin_frame(ui.ctx());

        // G6: the vk route decides where live notes go — recomputed before
        // the keyboard handling below reads it.
        self.refresh_vk_targets();

        // U2: an open unsaved-changes modal owns the keyboard (its Escape
        // cancels); the transport / note shortcuts stay out of the way. The
        // same holds for the plugin popup (U9 E3): it owns ↑↓/Enter/Esc.
        if self.pending_file_action.is_none() && !self.picker.is_open() {
            self.handle_keyboard_input(ui.ctx());
        }

        // U2: unsaved-changes guard on window close. The request is refused
        // once and replayed after the modal is answered.
        if ui.ctx().input(|i| i.viewport().close_requested())
            && !self.allow_close
            && self.session.is_dirty()
            && self.pending_file_action.is_none()
        {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending_file_action = Some(FileAction::Exit);
        }

        // For smoke tests: request exit once the auto-close deadline is reached
        // (the guard above must not block it).
        if let Some(after) = self.auto_close_after {
            if !self.close_sent && self.opened_at.elapsed() >= after {
                self.close_sent = true;
                self.allow_close = true;
                log::info!("Auto-close deadline reached; requesting exit");
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // Window title: file name + unsaved marker (U2). Recomputed every
        // frame so edits and undos retitle immediately; the viewport command
        // only fires when the text actually changed.
        self.window_title =
            project_files::window_title(self.project_path.as_deref(), self.session.is_dirty());
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
        // per node.
        let playing = self.engine.is_playing();
        if self.was_playing && !playing {
            self.push_node_params();
        }
        self.was_playing = playing;

        // U9 E2: the plugin-scan banner sits above the transport while the
        // startup scan has modules left (it names the module the frame's tail
        // will block on, spec D14).
        self.plugin_scan_banner(ui);
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
            MainView::Edit => {
                egui::CentralPanel::default().show(ui, |ui| self.edit_panel(ui));
            }
            MainView::Patch => {
                egui::CentralPanel::default().show(ui, |ui| self.graph_patch(ui));
            }
            MainView::Synth => self.synth_panel(ui),
        }

        // Embedded plugin editor lifecycle + window sync (spec
        // `plugin-gui.md` §4/§6). Open requests come from the synth panel
        // (which has no eframe frame to parent a container with); plugin
        // gui events are polled each frame; the native container follows
        // the rect the panel laid out. Tab-away hides the container;
        // switching the panel target destroys the editor (D9) — and with
        // the user toggle still on it reopens on the new node (G4).
        let target = self.panel_target_node();
        if self.plugin_editor_wants_open {
            self.plugin_editor_wants_open = false;
            if let Some(t) = target {
                self.open_plugin_editor(frame, t);
            }
        }
        let mut destroy_editor = false;
        // Parameter read-back (spec `plugin-gui.md` §5/§9): parameter
        // changes the plugin made inside its own GUI. CLAP pulls them via
        // `request_flush` (the plugin sets a flag), VST3 pushes them
        // through `IComponentHandler::performEdit` into a lock-free queue;
        // both land here. Applied after the guard borrow ends (engine
        // delivery + slider cache). `None` while the editor is closed (no
        // polling, no engine traffic).
        let mut flush_changes: Option<GuiParamBatch> = None;
        if let Some(editor) = self.plugin_editor.as_mut() {
            if Some(editor.doc_node) != target {
                destroy_editor = true;
            } else if let Some(guard) = self
                .voice_guards
                .iter_mut()
                .find(|e| e.node_id == editor.node_id)
                .map(|e| &mut e._guard)
            {
                let ppp = ui.ctx().pixels_per_point();
                match guard {
                    PluginGuard::Clap(g) => {
                        let events = g.editor_events();
                        if let Some(size) = events.resize {
                            editor.size = (size.width as f32 / ppp, size.height as f32 / ppp);
                        }
                        if g.gui_closed() {
                            // The plugin destroyed its GUI: the host
                            // acknowledges by calling destroy exactly once,
                            // then tears the session down.
                            g.close_editor();
                            self.editor_user_open = false;
                            destroy_editor = true;
                        } else {
                            let changes = g.poll_param_flush();
                            if !changes.is_empty() {
                                flush_changes = Some(GuiParamBatch {
                                    doc_node: editor.doc_node,
                                    is_clap: true,
                                    changes,
                                });
                            }
                        }
                    }
                    PluginGuard::Vst3(g) => {
                        // The view asked for a new size: accept it, then
                        // confirm once the container is actually resized
                        // (ack below).
                        if let Some(size) = g.editor_resize_request() {
                            editor.size = (size.width as f32 / ppp, size.height as f32 / ppp);
                            editor.pending_ack = Some(size);
                        }
                        let changes = g.take_param_edits();
                        if !changes.is_empty() {
                            flush_changes = Some(GuiParamBatch {
                                doc_node: editor.doc_node,
                                is_clap: false,
                                changes,
                            });
                        }
                    }
                }
            }
        }
        if destroy_editor {
            self.close_plugin_editor();
        }
        // G4: the target changed under an open editor — reopen on the new
        // node while the user toggle is still on (spec `plugin-gui.md` §6).
        if self.editor_user_open
            && self.plugin_editor.is_none()
            && let Some(t) = target.filter(|t| self.node_guard_has_editor(*t))
        {
            self.open_plugin_editor(frame, t);
        }
        if let Some(GuiParamBatch {
            doc_node,
            is_clap,
            changes,
        }) = flush_changes
        {
            // GUI edits do not enter undo history (spec D8); persistence is
            // covered by the preset system's save-time state capture.
            for (id, value) in changes {
                if is_clap {
                    self.clap_voice_values
                        .entry(doc_node)
                        .or_default()
                        .insert(id, value);
                } else {
                    // VST3 parameter values are normalized 0..1, which is
                    // also the engine bridge domain.
                    self.vst3_voice_values
                        .entry(doc_node)
                        .or_default()
                        .insert(id, value);
                }
                self.send_node_param(doc_node, ParamId::PluginParam(id), value as f32);
            }
        }
        // The view's size request is only acknowledged after the container
        // has been moved to it (the host half of the VST3 resize
        // handshake).
        let mut ack_resize: Option<(u64, glitcherz_vst3_host::ViewSize)> = None;
        if let Some(editor) = self.plugin_editor.as_mut() {
            if self.view != MainView::Synth {
                // D9: tab-away only hides; returning to the synth tab
                // shows the container again via reposition's
                // SWP_SHOWWINDOW.
                editor.container.hide();
            } else if let Some(rect) = editor.rect {
                editor
                    .container
                    .reposition_screen_rect(frame, rect, ui.ctx().pixels_per_point());
                if let Some(size) = editor.pending_ack.take() {
                    ack_resize = Some((editor.node_id, size));
                }
            }
        }
        if let Some((node_id, size)) = ack_resize
            && let Some(PluginGuard::Vst3(g)) = self.guard_by_engine_id(node_id)
        {
            g.ack_editor_size(size);
        }

        // U9 E3: the plugin popup draws above every panel and reports a pick
        // (it creates the node with its final source, spec D1).
        self.plugin_picker_frame(ui.ctx());

        // U2: the unsaved-changes modal draws on top of every panel, and an
        // approved close is replayed here (the guard above let it through).
        self.unsaved_changes_dialog(ui.ctx());
        if self.allow_close && !self.close_sent {
            self.close_sent = true;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // U9 E2: the startup plugin scan runs at the very end of the frame —
        // one module per frame, after every panel has been drawn (spec D14).
        // While modules remain, ask for the next frame immediately so the
        // queue drains faster than the 50 ms display tick below.
        self.plugin_scan_step();
        if self.plugin_catalog.is_scanning() {
            ui.ctx().request_repaint();
        }

        // Audio-driven displays (level meter, playback position, envelope) need periodic
        // repaints; without this, egui sleeps when idle and auto-close (smoke test) never fires.
        ui.ctx().request_repaint_after(Duration::from_millis(50));
    }
}

impl Drop for GlitcherZApp {
    fn drop(&mut self) {
        // Struct fields drop in declaration order and `voice_guards` is
        // declared before `plugin_editor` — without this explicit close
        // the plugin library would unload while its editor window is
        // still attached (spec `plugin-gui.md` §4.3).
        self.close_plugin_editor();
    }
}

/// Start the main window (blocks until the window closes). **Must be called on the main thread.**
pub fn run(engine: AudioEngine, config: UiConfig) -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("GlitcherZ")
            .with_inner_size([920.0, 760.0])
            .with_min_inner_size([760.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "GlitcherZ",
        native_options,
        Box::new(move |cc| Ok(Box::new(GlitcherZApp::new(cc, engine, config)))),
    )
}

#[cfg(test)]
mod preset_store_tests {
    use super::*;

    /// A unique throwaway preset-library root for one test run.
    fn temp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("glitcherz_preset_test_{tag}_{nanos}"))
    }

    #[test]
    fn preset_names_are_filesystem_safe() {
        assert_eq!(
            sanitize_preset_name("  My Lead  "),
            Some("My Lead".to_string())
        );
        assert_eq!(sanitize_preset_name(""), None);
        assert_eq!(sanitize_preset_name("   "), None);
        assert_eq!(sanitize_preset_name("."), None);
        assert_eq!(sanitize_preset_name(".."), None);
        // Path separators and reserved characters are mapped away.
        assert_eq!(
            sanitize_preset_name("a/b\\c:d"),
            Some("a_b_c_d".to_string())
        );
    }

    #[test]
    fn preset_plugin_key_separates_paths() {
        let a = preset_plugin_key("clap", "C:/plugins/dexed.clap", "Dexed");
        let b = preset_plugin_key("clap", "C:/elsewhere/dexed.clap", "Dexed");
        assert_ne!(a, b, "same-named plugins from different paths differ");
        let c = preset_plugin_key("vst3", "C:/plugins/dexed.vst3", "same cid");
        assert_ne!(a, c, "formats are namespaced");
        assert!(a.starts_with("clap-"));
    }

    #[test]
    fn preset_store_roundtrip_and_list() {
        let root = temp_root("roundtrip");
        let key = "clap-00000001-Dexed";
        let blob = vec![1u8, 2, 3, 250];
        save_voice_preset_in(&root, key, "Init", &blob).expect("save");
        save_voice_preset_in(&root, key, "Bass", &blob).expect("save");

        assert_eq!(
            list_voice_presets_in(&root, key),
            vec!["Bass".to_string(), "Init".to_string()]
        );
        assert_eq!(read_voice_preset_in(&root, key, "Init"), Some(blob));
        assert_eq!(read_voice_preset_in(&root, key, "missing"), None);

        delete_voice_preset_in(&root, key, "Init").expect("delete");
        assert_eq!(list_voice_presets_in(&root, key), vec!["Bass".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
#[cfg(test)]
mod panel_target_tests {
    use super::*;

    #[test]
    fn no_selection_falls_back_to_track_instrument() {
        let project = Project::default();
        let doc = &project.graph;
        let t0 = doc.instrument_of_track(0).unwrap().node_id();
        // Pre-G4 behavior: nothing selected → the selected track's instrument.
        assert_eq!(resolve_panel_target(doc, &[], 0), Some(t0));
    }

    #[test]
    fn selection_wins_over_track_fallback() {
        let project = Project::default();
        let doc = &project.graph;
        let t1 = doc.instrument_of_track(1).unwrap().node_id();
        // Cross-track patch target: the selection addresses the panel.
        assert_eq!(resolve_panel_target(doc, &[t1], 0), Some(t1));
    }

    #[test]
    fn effect_and_mixer_nodes_are_editable_targets() {
        let project = Project::default();
        let doc = &project.graph;
        let delay = doc.builtin_delay_of_track(0).unwrap().node_id();
        let mixer = doc.first_mixer().unwrap().node_id();
        assert_eq!(resolve_panel_target(doc, &[delay], 0), Some(delay));
        assert_eq!(resolve_panel_target(doc, &[mixer], 0), Some(mixer));
    }

    #[test]
    fn stale_ids_are_filtered_and_fall_back() {
        let project = Project::default();
        let doc = &project.graph;
        let t0 = doc.instrument_of_track(0).unwrap().node_id();
        // Deleted node ids never resolve; the walk falls back to the track.
        assert_eq!(resolve_panel_target(doc, &[9999], 0), Some(t0));
        // No editable node on a nonexistent track → no target.
        assert_eq!(resolve_panel_target(doc, &[], 99), None);
    }

    #[test]
    fn document_order_breaks_multi_selection_ties() {
        let project = Project::default();
        let doc = &project.graph;
        let t0 = doc.instrument_of_track(0).unwrap().node_id();
        let delay1 = doc.builtin_delay_of_track(1).unwrap().node_id();
        // Selection order is irrelevant: document order decides.
        assert_eq!(resolve_panel_target(doc, &[delay1, t0], 0), Some(t0));
        assert_eq!(resolve_panel_target(doc, &[t0, delay1], 0), Some(t0));
    }
}
#[cfg(test)]
mod inline_param_tests {
    use super::*;

    /// G5: every registry entry maps to the engine parameter the matching
    /// panel slider uses (inline and panel edits address the same
    /// `set_node_param` entry, so both paths stay interchangeable).
    #[test]
    fn inline_params_map_to_panel_param_ids() {
        assert_eq!(
            inline_param_id(patch::InlineParam::Cutoff),
            ParamId::FilterCutoff
        );
        assert_eq!(
            inline_param_id(patch::InlineParam::Resonance),
            ParamId::FilterResonance
        );
        assert_eq!(
            inline_param_id(patch::InlineParam::DelayTime),
            ParamId::DelayTime
        );
        assert_eq!(
            inline_param_id(patch::InlineParam::DelayFeedback),
            ParamId::DelayFeedback
        );
        assert_eq!(
            inline_param_id(patch::InlineParam::DelayMix),
            ParamId::DelayMix
        );
        assert_eq!(
            inline_param_id(patch::InlineParam::MixerOutPan),
            ParamId::MixerOutPan
        );
        // Plugin rows bridge through the same PluginParam entry as the panel
        // and automation paths.
        assert_eq!(
            param_id_of(&AutoParam::PluginParam {
                param_id: 7,
                name: String::new()
            }),
            Some(ParamId::PluginParam(7))
        );
        // U3b: tempo has no ParamId — it never reaches a node's set_param.
        assert_eq!(param_id_of(&AutoParam::Tempo), None);
    }
}

#[cfg(test)]
mod splice_tests {
    use super::*;
    use glitcherz_model::CableDoc;

    /// E4 acceptance: a splice reproduces the downstream slot exactly, so a
    /// mixer input keeps the channel its fader is bound to (D17).
    #[test]
    fn splice_keeps_the_downstream_channel_binding() {
        let cable = CableDoc::audio_to_channel(PortRef::Node(4), PortRef::Node(9), 2);
        let (from, to, in_port) = splice_endpoints(&cable).expect("audio splice");
        assert_eq!(from, 4);
        assert_eq!(to, PortRef::Node(9));
        assert_eq!(in_port, 2);
    }

    /// A splice into the master output keeps the target as-is.
    #[test]
    fn splice_accepts_a_master_target() {
        let cable = CableDoc::audio(PortRef::Node(3), PortRef::Master);
        let (from, to, in_port) = splice_endpoints(&cable).expect("audio splice");
        assert_eq!(from, 3);
        assert_eq!(to, PortRef::Master);
        assert_eq!(in_port, 0);
    }

    #[test]
    fn midi_and_automation_cables_take_no_processor() {
        let midi = CableDoc::midi(0, 7);
        assert_eq!(
            splice_endpoints(&midi),
            Err("only audio cables accept inserted nodes")
        );
    }

    #[test]
    fn a_cable_without_a_node_source_is_rejected() {
        // Timeline / master outs have no upstream node to re-feed.
        let cable = CableDoc::audio(PortRef::Master, PortRef::Node(5));
        assert_eq!(splice_endpoints(&cable), Err("cable has no node source"));
    }

    /// Only processors fit an audio splice: an instrument would join the chain
    /// with no MIDI feed and stay silent.
    #[test]
    fn only_effects_and_mixers_fit_an_audio_splice() {
        use patch::NodeSource;
        let insertable = [
            NodeSource::BuiltinDelay,
            NodeSource::ClapEffect {
                path: "C:/p/fx.clap".to_string(),
                plugin_id: "fx".to_string(),
            },
            NodeSource::Vst3Effect {
                path: "C:/p/fx.vst3".to_string(),
                class_id: "fx".to_string(),
            },
            NodeSource::Mixer,
        ];
        for source in &insertable {
            assert!(source.fits_audio_insert(), "{source:?} should insert");
        }
        let rejected = [
            NodeSource::BuiltinSynth,
            NodeSource::SoundFont {
                path: "C:/p/x.sf2".to_string(),
            },
            NodeSource::ClapInstrument {
                path: "C:/p/i.clap".to_string(),
                plugin_id: "i".to_string(),
            },
            NodeSource::Vst3Instrument {
                path: "C:/p/i.vst3".to_string(),
                class_id: "i".to_string(),
            },
            NodeSource::Keyboard,
            NodeSource::Feedback,
        ];
        for source in &rejected {
            assert!(!source.fits_audio_insert(), "{source:?} must not insert");
        }
    }
}
