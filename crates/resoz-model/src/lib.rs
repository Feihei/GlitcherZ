#![forbid(unsafe_code)]

//! # resoz-model
//!
//! ResoZ data model: MIDI tracks, automation tracks, the node graph, and the
//! project file (document layer).
//!
//! Conventions (see AGENTS.md "Data Model"):
//! - All types must be `Serialize + Deserialize` (RON format);
//! - New fields always get `#[serde(default)]` to keep old project files
//!   forward compatible;
//! - The model is the "document", the engine is the "player": this crate does
//!   not depend on `resoz-engine`, and the engine does not depend on this
//!   crate either -- `resoz-app` / `resoz-ui` do the conversion glue.
//!
//! v3 (spec `docs/spec/node-graph.md` §7 final state, D11-D13):
//! - `MidiTrack` (note plane) / `AutomationTrack` (pure curve) become
//!   top-level `Vec`s (D12);
//! - `GraphDoc{nodes, cables}` fully explicit: the default topology is also
//!   written into the document, and the loader rebuilds the engine graph
//!   purely from the document (D13);
//! - `node_id` is persisted with the file (D11); id 0 is invalid,
//!   id 1 = master output reserved ([`MASTER_NODE_ID`]), document nodes
//!   start from 2.

pub mod file;

pub use file::{
    ModelError, PROJECT_VERSION, ProjectFile, load_project, parse_project, save_project,
};

/// Number of steps in the step sequencer.
pub const PATTERN_STEPS: usize = 16;

/// MIDI track capacity limit (D12; the engine side uses the same value).
pub const MAX_MIDI_TRACKS: usize = 16;

/// Automation track capacity limit (D12; the engine-side `MAX_AUTO_TRACKS`
/// uses the same value).
pub const MAX_AUTO_TRACKS: usize = 16;

/// Node graph capacity limit (D1; same value on the engine side).
pub const MAX_NODES: usize = 64;

/// Cable capacity limit (D1; same value on the engine side).
pub const MAX_CABLES: usize = 128;

/// Number of MIDI tracks in the default (new) project.
pub const DEFAULT_MIDI_TRACKS: usize = 4;

/// Reserved node_id of the master output (D6: always present, cannot be
/// deleted, not listed in the document node table).
pub const MASTER_NODE_ID: u64 = 1;

/// A single step: switch + note + velocity.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Step {
    /// Whether this step triggers.
    #[serde(default)]
    pub enabled: bool,
    /// MIDI note number (69 = A4).
    #[serde(default = "default_note")]
    pub note: u8,
    /// Velocity (0..1, audio-path parameters use f32).
    #[serde(default = "default_velocity")]
    pub velocity: f32,
}

fn default_note() -> u8 {
    60
}

fn default_velocity() -> f32 {
    0.9
}

impl Default for Step {
    fn default() -> Self {
        Self {
            enabled: false,
            note: default_note(),
            velocity: default_velocity(),
        }
    }
}

/// Step sequencer mode: 16-step monophonic track.
///
/// The fixed-length array (instead of a `Vec`) is intentional: `Pattern`
/// stays `Copy`, so it can be sent to the audio thread via the lock-free
/// command queue as a whole block, with no heap allocation at all.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pattern {
    #[serde(default = "default_steps")]
    pub steps: [Step; PATTERN_STEPS],
}

fn default_steps() -> [Step; PATTERN_STEPS] {
    [Step::default(); PATTERN_STEPS]
}

impl Default for Pattern {
    fn default() -> Self {
        Self {
            steps: default_steps(),
        }
    }
}

impl Pattern {
    /// Sets step 0 to the given note (convenience constructor, for
    /// tests/demos).
    pub fn single_note(note: u8) -> Self {
        let mut steps = default_steps();
        steps[0] = Step {
            enabled: true,
            note,
            velocity: 0.9,
        };
        Self { steps }
    }
}

/// Per-track synthesizer timbre parameters.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SynthParams {
    /// Filter cutoff frequency (Hz).
    #[serde(default = "default_cutoff")]
    pub cutoff_hz: f32,
    /// Filter resonance (Simper SVF res, 0..0.98).
    #[serde(default = "default_resonance")]
    pub resonance: f32,
    /// Filter mode: 0 = low-pass, 1 = high-pass, 2 = band-pass.
    #[serde(default)]
    pub filter_mode: f32,
    /// Envelope attack (seconds).
    #[serde(default = "default_attack")]
    pub attack_s: f32,
    /// Envelope decay (seconds).
    #[serde(default = "default_decay")]
    pub decay_s: f32,
    /// Envelope sustain (0..1).
    #[serde(default)]
    pub sustain: f32,
    /// Envelope release (seconds).
    #[serde(default = "default_release")]
    pub release_s: f32,
}

fn default_cutoff() -> f32 {
    12_000.0
}

fn default_resonance() -> f32 {
    0.2
}

fn default_attack() -> f32 {
    0.01
}

fn default_decay() -> f32 {
    0.10
}

fn default_sustain() -> f32 {
    0.8
}

fn default_release() -> f32 {
    0.20
}

impl Default for SynthParams {
    fn default() -> Self {
        Self {
            cutoff_hz: default_cutoff(),
            resonance: default_resonance(),
            filter_mode: 0.0,
            attack_s: default_attack(),
            decay_s: default_decay(),
            sustain: default_sustain(),
            release_s: default_release(),
        }
    }
}

/// Per-track delay effect parameters (`mix = 0` means bypass).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DelayParams {
    /// Delay time (seconds, 0.005..1).
    #[serde(default = "default_delay_time")]
    pub time_s: f32,
    /// Feedback amount (0..0.9).
    #[serde(default = "default_delay_feedback")]
    pub feedback: f32,
    /// Wet mix ratio (0..1).
    #[serde(default)]
    pub mix: f32,
}

fn default_delay_time() -> f32 {
    0.3
}

fn default_delay_feedback() -> f32 {
    0.35
}

impl Default for DelayParams {
    fn default() -> Self {
        Self {
            time_s: default_delay_time(),
            feedback: default_delay_feedback(),
            mix: 0.0,
        }
    }
}

/// Note time base: ticks per quarter note (1/96 quarter ≈ 10.4 ms @120 BPM).
pub const TICKS_PER_QUARTER: u32 = 96;

/// Ticks per step (16th note).
pub const TICKS_PER_STEP: u32 = 24;

/// Ticks per bar (4/4) = 16 steps × 24.
pub const TICKS_PER_BAR: u32 = TICKS_PER_STEP * 16;

/// A note block in the piano roll.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Note {
    /// Start position (tick, relative to clip start).
    #[serde(default)]
    pub start: u32,
    /// Length (tick).
    #[serde(default = "default_note_length")]
    pub length: u32,
    /// MIDI note number.
    #[serde(default)]
    pub note: u8,
    /// Velocity (0..1).
    #[serde(default = "default_velocity")]
    pub velocity: f32,
}

fn default_note_length() -> u32 {
    TICKS_PER_STEP
}

impl Default for Note {
    fn default() -> Self {
        Self {
            start: 0,
            length: default_note_length(),
            note: 60,
            velocity: default_velocity(),
        }
    }
}

/// Piano roll clip: a set of notes + loop length.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Clip {
    /// Loop length (tick), defaults to one bar (4/4).
    #[serde(default = "default_clip_loop")]
    pub loop_ticks: u32,
    /// Note collection.
    #[serde(default)]
    pub notes: Vec<Note>,
}

fn default_clip_loop() -> u32 {
    TICKS_PER_BAR
}

impl Default for Clip {
    fn default() -> Self {
        Self {
            loop_ticks: default_clip_loop(),
            notes: Vec::new(),
        }
    }
}

/// Node voice: built-in (synthesizer/delay) or CLAP / VST3 plugin.
///
/// Semantics depend on the node type: `Builtin` on an `Instrument` node =
/// built-in synthesizer, `Builtin` on an `Effect` node = built-in delay.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VoiceSpec {
    /// Built-in (default, forward compatible with old project files).
    #[default]
    Builtin,
    /// CLAP plugin: dynamic library path + plugin ID (instantiated on the
    /// non-real-time thread).
    Clap {
        #[serde(default)]
        path: String,
        #[serde(default)]
        plugin_id: String,
    },
    /// VST3 plugin: .vst3 file/bundle path + class CID (16-byte hex).
    Vst3 {
        #[serde(default)]
        path: String,
        #[serde(default)]
        class_id: String,
    },
}

/// Track sound source: 16-step pattern or piano roll clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum TrackSource {
    /// Step sequencer mode (default, forward compatible with old project
    /// files).
    #[default]
    Steps,
    /// Piano roll clip.
    Clip,
}

/// The note plane of one MIDI track (D12 v3: split out of the old `Track`).
///
/// Timbre/mix/effects are no longer track fields -- they are the
/// [`NodeDoc::Instrument`] / [`NodeDoc::Effect`] / [`NodeDoc::ChannelStrip`]
/// nodes in the node graph.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct MidiTrack {
    /// Track name (if empty the UI shows "Track N").
    #[serde(default)]
    pub name: String,
    /// The track's step pattern (played when `source = Steps`).
    #[serde(default)]
    pub pattern: Pattern,
    /// The track's clip (played when `source = Clip`).
    #[serde(default)]
    pub clip: Clip,
    /// Sound source selection.
    #[serde(default)]
    pub source: TrackSource,
}

/// Automatable / cable-connected node parameter (automation cable endpoint
/// payload).
///
/// D7: the automation curve itself carries no target -- the target
/// `(node_id, param)` is carried by [`CableDoc`]; this enum is the document
/// representation of `param` in it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub enum AutoParam {
    /// Track gain (linear, 0..1.5).
    #[default]
    TrackGain,
    /// Track pan (-1..1).
    TrackPan,
    /// Filter cutoff frequency (Hz, 20..18000, log axis in the UI).
    FilterCutoff,
    /// CLAP plugin parameter (bridged to automation `ParamValueEvent`; name
    /// for UI display).
    PluginParam {
        /// Plugin-side parameter ID.
        #[serde(default)]
        param_id: u32,
        /// Parameter display name (edit-time snapshot, serialized and saved).
        #[serde(default)]
        name: String,
    },
}

/// Automation anchor: tick position + real parameter value.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct AutoPoint {
    /// Position (tick, relative to loop start).
    #[serde(default)]
    pub tick: u32,
    /// Parameter value (real units, not normalized).
    #[serde(default)]
    pub value: f32,
}

/// One automation track (D7/D12 v3: top-level entity, pure anchor curve, no
/// target).
///
/// Anchors are stored in ascending tick order; values are linearly
/// interpolated between adjacent anchors, and clamped to the end values
/// outside both ends. Empty curves or disabled tracks produce no automation.
/// The delivery endpoint `(node_id, param)` is decided by the
/// [`CableKind::Automation`] cable in the node graph.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct AutomationTrack {
    /// Whether enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Loop length (tick): evaluation positions are taken modulo this.
    /// Defaults to one bar (384).
    #[serde(default = "default_loop_ticks")]
    pub loop_ticks: u32,
    /// Anchors (ascending by tick).
    #[serde(default)]
    pub points: Vec<AutoPoint>,
}

fn default_loop_ticks() -> u32 {
    TICKS_PER_BAR
}

fn default_true() -> bool {
    true
}

impl AutomationTrack {
    /// Samples the value at `tick`: returns `None` when empty/disabled;
    /// linear interpolation between two points; clamps to the endpoint values
    /// outside both ends. Requires anchors to be ordered (editing interfaces
    /// maintain this).
    pub fn value_at(&self, tick: f64) -> Option<f32> {
        if !self.enabled || self.points.is_empty() {
            return None;
        }
        let first = self.points[0];
        let last = *self.points.last()?;
        if tick <= f64::from(first.tick) {
            return Some(first.value);
        }
        if tick >= f64::from(last.tick) {
            return Some(last.value);
        }
        // Linear scan is enough: the number of anchors is finite, and the
        // call frequency is once per block per curve.
        for window in self.points.windows(2) {
            let (a, b) = (window[0], window[1]);
            if tick < f64::from(b.tick) {
                let span = f64::from(b.tick) - f64::from(a.tick);
                let ratio = if span > 0.0 {
                    (tick - f64::from(a.tick)) / span
                } else {
                    0.0
                };
                return Some(a.value + (b.value - a.value) * ratio as f32);
            }
        }
        Some(last.value)
    }

    /// Inserts or updates an anchor (overwrites the same tick), keeping
    /// ascending tick order.
    pub fn upsert_point(&mut self, point: AutoPoint) {
        self.points.retain(|p| p.tick != point.tick);
        let pos = self.points.partition_point(|p| p.tick < point.tick);
        self.points.insert(pos, point);
    }

    /// Removes the nearest anchor within the `tick` tolerance; returns
    /// `true` on a hit.
    pub fn remove_near(&mut self, tick: u32, tolerance: u32) -> bool {
        let Some(best) = self
            .points
            .iter()
            .enumerate()
            .filter(|(_, p)| p.tick.abs_diff(tick) <= tolerance)
            .min_by_key(|(_, p)| p.tick.abs_diff(tick))
            .map(|(index, _)| index)
        else {
            return false;
        };
        self.points.remove(best);
        true
    }
}

/// Document node (spec §7 `NodeDoc`): node identity in the engine graph +
/// parameter persistence.
///
/// `track` is the node's **owning track** (which track's panel controls it) --
/// the engine-side track routing table (manual params/level/keyboard
/// addressing) is assembled by it; the actual signal flow is decided by
/// [`GraphDoc::cables`], and the two are decoupled.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeDoc {
    /// Instrument node (audio source, MIDI entry). `Builtin` = built-in
    /// synthesizer.
    Instrument {
        /// Persisted node ID (≥ 2; 1 = master output reserved).
        #[serde(default)]
        node_id: u64,
        /// Owning track.
        #[serde(default)]
        track: usize,
        /// Voice specification.
        #[serde(default)]
        voice: VoiceSpec,
        /// Built-in synthesizer timbre (ignored for external plugins).
        #[serde(default)]
        synth: SynthParams,
    },
    /// Effect node (1 in, 1 out). `Builtin` = built-in delay.
    Effect {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track.
        #[serde(default)]
        track: usize,
        /// Effect specification.
        #[serde(default)]
        voice: VoiceSpec,
        /// Built-in delay parameters (ignored for external plugins).
        #[serde(default)]
        delay: DelayParams,
    },
    /// Channel strip node (gain/pan/mute, sums ≤8 input edges).
    ChannelStrip {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track.
        #[serde(default)]
        track: usize,
        /// Track gain (linear, 0..1.5).
        #[serde(default = "default_track_gain")]
        gain: f32,
        /// Pan (-1..1, linear pan law).
        #[serde(default)]
        pan: f32,
        /// Mute.
        #[serde(default)]
        muted: bool,
    },
}

fn default_track_gain() -> f32 {
    0.8
}

impl NodeDoc {
    /// Node ID.
    pub fn node_id(&self) -> u64 {
        match self {
            NodeDoc::Instrument { node_id, .. }
            | NodeDoc::Effect { node_id, .. }
            | NodeDoc::ChannelStrip { node_id, .. } => *node_id,
        }
    }

    /// Owning track.
    pub fn track(&self) -> usize {
        match self {
            NodeDoc::Instrument { track, .. }
            | NodeDoc::Effect { track, .. }
            | NodeDoc::ChannelStrip { track, .. } => *track,
        }
    }
}

/// Cable port reference (spec §7 `PortRef`).
///
/// `Master` / `MidiTrack` / `AutoTrack` are reserved identities of fixed
/// entities (not listed in the node table; the loader/engine fills them in
/// implicitly); `Node` references a document node.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortRef {
    /// Document node (node_id).
    Node(u64),
    /// Master output (fixed node, D6).
    #[default]
    Master,
    /// Timeline MIDI track outlet (track index).
    MidiTrack(usize),
    /// Timeline automation track outlet (track index).
    AutoTrack(usize),
}

/// Cable type (D3): bound to the port type; type mismatches are rejected at
/// load time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CableKind {
    /// Audio cable: node output → node input / master output.
    #[default]
    Audio,
    /// MIDI cable: timeline MIDI track → instrument node.
    Midi,
    /// Automation cable: timeline automation track → node parameter port.
    Automation,
}

/// Document cable (spec §7 `CableDoc`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CableDoc {
    /// Cable type.
    #[serde(default)]
    pub kind: CableKind,
    /// Source port.
    #[serde(default)]
    pub from: PortRef,
    /// Target port.
    #[serde(default)]
    pub to: PortRef,
    /// Endpoint parameter (only used when `kind = Automation`; D7).
    #[serde(default)]
    pub param: AutoParam,
}

impl CableDoc {
    /// Convenience constructor for audio cables.
    pub fn audio(from: impl Into<PortRef>, to: impl Into<PortRef>) -> Self {
        Self {
            kind: CableKind::Audio,
            from: from.into(),
            to: to.into(),
            param: AutoParam::default(),
        }
    }

    /// Convenience constructor for MIDI cables.
    pub fn midi(from_track: usize, to_node: u64) -> Self {
        Self {
            kind: CableKind::Midi,
            from: PortRef::MidiTrack(from_track),
            to: PortRef::Node(to_node),
            param: AutoParam::default(),
        }
    }

    /// Convenience constructor for automation cables.
    pub fn automation(from_track: usize, to_node: u64, param: AutoParam) -> Self {
        Self {
            kind: CableKind::Automation,
            from: PortRef::AutoTrack(from_track),
            to: PortRef::Node(to_node),
            param,
        }
    }
}

impl From<u64> for PortRef {
    fn from(node_id: u64) -> Self {
        PortRef::Node(node_id)
    }
}

/// Document representation of the node graph (v3 final state, spec §7):
/// fully explicit nodes + cables.
///
/// The default topology (per track `voice → delay → channel strip → master
/// output` + MIDI self-connection) is also fully written into the document
/// (D13): the loader **rebuilds the engine graph purely from the document**;
/// document and engine are one and the same. The master output and the
/// timeline are fixed entities and do not appear in `nodes`; cables reference
/// them via [`PortRef::Master`] / [`PortRef::MidiTrack`] /
/// [`PortRef::AutoTrack`].
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct GraphDoc {
    /// Node table (≤ [`MAX_NODES`]; node_id unique and ≥ 2).
    #[serde(default)]
    pub nodes: Vec<NodeDoc>,
    /// Cable table (≤ [`MAX_CABLES`]).
    #[serde(default)]
    pub cables: Vec<CableDoc>,
}

impl GraphDoc {
    /// Next available node_id in the document (max + 1, at least 2).
    pub fn next_node_id(&self) -> u64 {
        self.nodes
            .iter()
            .map(NodeDoc::node_id)
            .max()
            .unwrap_or(MASTER_NODE_ID)
            .max(MASTER_NODE_ID)
            + 1
    }

    /// Finds a node by node_id.
    pub fn node_by_id(&self, node_id: u64) -> Option<&NodeDoc> {
        self.nodes.iter().find(|n| n.node_id() == node_id)
    }

    /// The instrument node of a track (first `Instrument` whose `track`
    /// matches).
    pub fn instrument_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::Instrument { .. }) && n.track() == track)
    }

    /// The built-in delay node of a track (document host of the delay
    /// parameters).
    pub fn builtin_delay_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes.iter().find(|n| {
            matches!(
                n,
                NodeDoc::Effect {
                    voice: VoiceSpec::Builtin,
                    ..
                }
            ) && n.track() == track
        })
    }

    /// The first effect node of a track.
    pub fn effect_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::Effect { .. }) && n.track() == track)
    }

    /// The channel strip node of a track.
    pub fn channel_strip_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::ChannelStrip { .. }) && n.track() == track)
    }

    /// The channel strip node of a track (mutable).
    pub fn channel_strip_of_track_mut(&mut self, track: usize) -> Option<&mut NodeDoc> {
        self.nodes
            .iter_mut()
            .find(|n| matches!(n, NodeDoc::ChannelStrip { .. }) && n.track() == track)
    }

    /// The instrument node of a track (mutable).
    pub fn instrument_of_track_mut(&mut self, track: usize) -> Option<&mut NodeDoc> {
        self.nodes
            .iter_mut()
            .find(|n| matches!(n, NodeDoc::Instrument { .. }) && n.track() == track)
    }

    /// The built-in delay node of a track (mutable).
    pub fn builtin_delay_of_track_mut(&mut self, track: usize) -> Option<&mut NodeDoc> {
        self.nodes.iter_mut().find(|n| {
            matches!(
                n,
                NodeDoc::Effect {
                    voice: VoiceSpec::Builtin,
                    ..
                }
            ) && n.track() == track
        })
    }

    /// Endpoint parameter of the cable for automation track `index`
    /// (returns `None` when not cabled).
    pub fn auto_endpoint(&self, index: usize) -> Option<AutoParam> {
        self.cables.iter().find_map(|c| {
            if c.kind == CableKind::Automation && c.from == PortRef::AutoTrack(index) {
                Some(c.param.clone())
            } else {
                None
            }
        })
    }

    /// Removes the cable for automation track `index` (call before changing
    /// the endpoint).
    pub fn remove_auto_cable(&mut self, index: usize) {
        self.cables
            .retain(|c| !(c.kind == CableKind::Automation && c.from == PortRef::AutoTrack(index)));
    }

    /// Load-time validation (the file layer is the last line of defense;
    /// report errors instead of truncating).
    ///
    /// `midi_count` / `auto_count` are the current track counts of the
    /// project (out-of-range port references are invalid).
    pub fn validate(&self, midi_count: usize, auto_count: usize) -> Result<(), String> {
        if self.nodes.len() > MAX_NODES {
            return Err(format!(
                "node count {} exceeds the limit {MAX_NODES}",
                self.nodes.len()
            ));
        }
        if self.cables.len() > MAX_CABLES {
            return Err(format!(
                "cable count {} exceeds the limit {MAX_CABLES}",
                self.cables.len()
            ));
        }
        for (i, node_id) in self.nodes.iter().map(NodeDoc::node_id).enumerate() {
            if node_id <= MASTER_NODE_ID {
                return Err(format!(
                    "invalid node_id = {node_id} for node #{i} (0 is invalid, 1 = master output reserved)"
                ));
            }
            if self.nodes[i + 1..].iter().any(|n| n.node_id() == node_id) {
                return Err(format!("duplicate node_id {node_id}"));
            }
        }
        let node_ids: Vec<u64> = self.nodes.iter().map(NodeDoc::node_id).collect();
        for cable in &self.cables {
            self.check_port(&cable.from, &node_ids, midi_count, auto_count)?;
            self.check_port(&cable.to, &node_ids, midi_count, auto_count)?;
            match cable.kind {
                CableKind::Audio => {
                    if !matches!(cable.from, PortRef::Node(_)) {
                        return Err("audio cable source port must be a node".to_string());
                    }
                    if !matches!(cable.to, PortRef::Node(_) | PortRef::Master) {
                        return Err(
                            "audio cable target port must be a node or the master output"
                                .to_string(),
                        );
                    }
                }
                CableKind::Midi => {
                    if !matches!(cable.from, PortRef::MidiTrack(_)) {
                        return Err(
                            "MIDI cable source port must be a timeline MIDI track".to_string()
                        );
                    }
                    match cable.to {
                        PortRef::Node(id) => {
                            if !self.nodes.iter().any(|n| {
                                n.node_id() == id && matches!(n, NodeDoc::Instrument { .. })
                            }) {
                                return Err(format!(
                                    "MIDI cable target node_id {id} is not an instrument node"
                                ));
                            }
                        }
                        _ => {
                            return Err(
                                "MIDI cable target port must be an instrument node".to_string()
                            );
                        }
                    }
                }
                CableKind::Automation => {
                    if !matches!(cable.from, PortRef::AutoTrack(_)) {
                        return Err(
                            "automation cable source port must be a timeline automation track"
                                .to_string(),
                        );
                    }
                    if !matches!(cable.to, PortRef::Node(_)) {
                        return Err("automation cable target port must be a node parameter port"
                            .to_string());
                    }
                }
            }
        }
        Ok(())
    }

    fn check_port(
        &self,
        port: &PortRef,
        node_ids: &[u64],
        midi_count: usize,
        auto_count: usize,
    ) -> Result<(), String> {
        match port {
            PortRef::Node(id) => {
                if !node_ids.contains(id) {
                    return Err(format!("cable references a nonexistent node_id {id}"));
                }
            }
            PortRef::MidiTrack(index) => {
                if *index >= midi_count {
                    return Err(format!(
                        "MIDI track port {index} out of range (track count = {midi_count})"
                    ));
                }
            }
            PortRef::AutoTrack(index) => {
                if *index >= auto_count {
                    return Err(format!(
                        "automation track port {index} out of range (track count = {auto_count})"
                    ));
                }
            }
            PortRef::Master => {}
        }
        Ok(())
    }
}

/// Generates the default node graph document (D13 fully explicit; used by
/// `Project::default` and new projects): per track `voice → delay → channel
/// strip → master output` + this track's MIDI self-connection.
///
/// node_ids are allocated consecutively starting from [`MASTER_NODE_ID`] + 1.
pub fn default_graph_doc(track_count: usize) -> GraphDoc {
    let mut nodes = Vec::new();
    let mut cables = Vec::new();
    let mut next_id = MASTER_NODE_ID + 1;
    for track in 0..track_count {
        let voice_id = next_id;
        let delay_id = next_id + 1;
        let strip_id = next_id + 2;
        next_id += 3;
        nodes.push(NodeDoc::Instrument {
            node_id: voice_id,
            track,
            voice: VoiceSpec::Builtin,
            synth: SynthParams::default(),
        });
        nodes.push(NodeDoc::Effect {
            node_id: delay_id,
            track,
            voice: VoiceSpec::Builtin,
            delay: DelayParams::default(),
        });
        nodes.push(NodeDoc::ChannelStrip {
            node_id: strip_id,
            track,
            gain: default_track_gain(),
            pan: 0.0,
            muted: false,
        });
        cables.push(CableDoc::audio(voice_id, delay_id));
        cables.push(CableDoc::audio(delay_id, strip_id));
        cables.push(CableDoc::audio(strip_id, PortRef::Master));
        cables.push(CableDoc::midi(track, voice_id));
    }
    GraphDoc { nodes, cables }
}

/// Multi-track project document (v3).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Project {
    /// Project metadata (name, tempo, etc.).
    #[serde(default)]
    pub metadata: ProjectMetadata,
    /// Sequencer gate ratio of a step (0.05..1).
    #[serde(default = "default_gate")]
    pub gate: f32,
    /// Master output gain (linear).
    #[serde(default = "default_master_gain")]
    pub master_gain: f32,
    /// MIDI tracks (note plane, D12 dynamic, capped at [`MAX_MIDI_TRACKS`]).
    #[serde(default)]
    pub midi_tracks: Vec<MidiTrack>,
    /// Automation tracks (top-level entities, D7/D12, capped at
    /// [`MAX_AUTO_TRACKS`]).
    #[serde(default)]
    pub automation_tracks: Vec<AutomationTrack>,
    /// Node graph document (D13 fully explicit).
    #[serde(default)]
    pub graph: GraphDoc,
}

fn default_gate() -> f32 {
    0.8
}

fn default_master_gain() -> f32 {
    0.8
}

impl Default for Project {
    fn default() -> Self {
        Self {
            metadata: ProjectMetadata::default(),
            gate: default_gate(),
            master_gain: default_master_gain(),
            midi_tracks: vec![MidiTrack::default(); DEFAULT_MIDI_TRACKS],
            automation_tracks: Vec::new(),
            graph: default_graph_doc(DEFAULT_MIDI_TRACKS),
        }
    }
}

impl Project {
    /// Project-level validation: track count limits + graph structure (the
    /// file layer is the last line of defense).
    pub fn validate(&self) -> Result<(), String> {
        if self.midi_tracks.len() > MAX_MIDI_TRACKS {
            return Err(format!(
                "MIDI track count {} exceeds the limit {MAX_MIDI_TRACKS}",
                self.midi_tracks.len()
            ));
        }
        if self.automation_tracks.len() > MAX_AUTO_TRACKS {
            return Err(format!(
                "automation track count {} exceeds the limit {MAX_AUTO_TRACKS}",
                self.automation_tracks.len()
            ));
        }
        self.graph
            .validate(self.midi_tracks.len(), self.automation_tracks.len())
    }
}

/// Project-level metadata.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProjectMetadata {
    /// Project name.
    #[serde(default)]
    pub name: String,
    /// Tempo (BPM). The UI and time calculations uniformly use f64.
    #[serde(default = "default_tempo")]
    pub tempo_bpm: f64,
}

fn default_tempo() -> f64 {
    120.0
}

impl Default for ProjectMetadata {
    fn default() -> Self {
        Self {
            name: String::new(),
            tempo_bpm: default_tempo(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutoParam, AutoPoint, AutomationTrack, CableDoc, DEFAULT_MIDI_TRACKS, DelayParams,
        GraphDoc, MASTER_NODE_ID, MAX_NODES, MidiTrack, NodeDoc, PATTERN_STEPS, Pattern, PortRef,
        Project, Step, SynthParams, TICKS_PER_BAR, TICKS_PER_STEP, TrackSource, VoiceSpec,
        default_graph_doc,
    };

    #[test]
    fn pattern_ron_roundtrip() {
        let mut pattern = Pattern::default();
        pattern.steps[0] = Step {
            enabled: true,
            note: 69,
            velocity: 0.9,
        };
        pattern.steps[7].enabled = true;
        let text = ron::to_string(&pattern).expect("serialization failed");
        let back: Pattern = ron::from_str(&text).expect("deserialization failed");
        assert_eq!(pattern, back);
    }

    #[test]
    fn step_partial_fields_use_defaults() {
        // Only `note` is written: enabled/velocity take serde defaults when
        // missing (old file compatibility).
        let back: Step = ron::from_str("(note: 64)").expect("parse failed");
        assert!(!back.enabled);
        assert_eq!(back.note, 64);
        assert!((back.velocity - 0.9).abs() < 1e-6);
    }

    #[test]
    fn default_project_graph_is_fully_explicit() {
        // D13: the default document fully describes the default topology.
        let project = Project::default();
        assert_eq!(project.midi_tracks.len(), DEFAULT_MIDI_TRACKS);
        let graph = &project.graph;
        assert_eq!(graph.nodes.len(), DEFAULT_MIDI_TRACKS * 3);
        // 4 cables per track: voice→delay→strip→master + midi self-connection.
        assert_eq!(graph.cables.len(), DEFAULT_MIDI_TRACKS * 4);
        for t in 0..DEFAULT_MIDI_TRACKS {
            let voice = graph.instrument_of_track(t).expect("voice should exist");
            let delay = graph.builtin_delay_of_track(t).expect("delay should exist");
            let strip = graph
                .channel_strip_of_track(t)
                .expect("channel strip should exist");
            assert_eq!(voice.node_id(), (MASTER_NODE_ID + 1) + (t as u64) * 3);
            assert_eq!(strip.node_id(), (MASTER_NODE_ID + 3) + (t as u64) * 3);
            let _ = delay;
        }
        assert!(project.validate().is_ok());
    }

    #[test]
    fn project_ron_roundtrip() {
        let mut project = Project::default();
        project.metadata.name = "demo".to_string();
        project.midi_tracks[0].name = "Bass".to_string();
        project.midi_tracks[0].pattern = Pattern::single_note(48);
        project.midi_tracks[1].source = TrackSource::Clip;
        project.automation_tracks.push(AutomationTrack {
            enabled: true,
            loop_ticks: 192,
            points: vec![AutoPoint {
                tick: 0,
                value: 1.0,
            }],
        });
        // Automation track 0 → track 0 channel strip gain.
        let strip = project.graph.channel_strip_of_track(0).unwrap().node_id();
        project
            .graph
            .cables
            .push(CableDoc::automation(0, strip, AutoParam::TrackGain));
        let text = ron::to_string(&project).expect("serialization failed");
        let back: Project = ron::from_str(&text).expect("deserialization failed");
        assert_eq!(project, back);
        assert!(back.validate().is_ok());
    }

    #[test]
    fn partial_project_falls_back_to_defaults() {
        // Hand-edited/future files: only metadata is written, the rest take
        // field-level defaults (empty tracks + empty graph -- whatever the
        // document says, D13).
        let back: Project = ron::from_str("(metadata: (tempo_bpm: 140))").expect("parse failed");
        assert!((back.metadata.tempo_bpm - 140.0).abs() < 1e-9);
        assert!(back.midi_tracks.is_empty());
        assert!(back.graph.nodes.is_empty());
        assert!(
            back.validate().is_ok(),
            "empty tracks + empty graph is a valid document"
        );
    }

    #[test]
    fn graph_doc_validate_rejects_bad_structure() {
        // Reserved id (1 = master output).
        let graph = GraphDoc {
            nodes: vec![NodeDoc::ChannelStrip {
                node_id: MASTER_NODE_ID,
                track: 0,
                gain: 0.8,
                pan: 0.0,
                muted: false,
            }],
            cables: Vec::new(),
        };
        assert!(graph.validate(1, 0).is_err());

        // Duplicate node_id.
        let graph = GraphDoc {
            nodes: vec![
                NodeDoc::ChannelStrip {
                    node_id: 2,
                    track: 0,
                    gain: 0.8,
                    pan: 0.0,
                    muted: false,
                },
                NodeDoc::ChannelStrip {
                    node_id: 2,
                    track: 1,
                    gain: 0.8,
                    pan: 0.0,
                    muted: false,
                },
            ],
            cables: Vec::new(),
        };
        assert!(graph.validate(2, 0).is_err());

        // Cable references a nonexistent node.
        let graph = GraphDoc {
            nodes: Vec::new(),
            cables: vec![CableDoc::audio(9, PortRef::Master)],
        };
        assert!(graph.validate(0, 0).is_err());

        // MIDI cable target is not an instrument.
        let graph = GraphDoc {
            nodes: vec![NodeDoc::ChannelStrip {
                node_id: 2,
                track: 0,
                gain: 0.8,
                pan: 0.0,
                muted: false,
            }],
            cables: vec![CableDoc::midi(0, 2)],
        };
        assert!(graph.validate(1, 0).is_err());

        // Automation cable source port out of range.
        let graph = GraphDoc {
            nodes: vec![NodeDoc::ChannelStrip {
                node_id: 2,
                track: 0,
                gain: 0.8,
                pan: 0.0,
                muted: false,
            }],
            cables: vec![CableDoc::automation(3, 2, AutoParam::TrackGain)],
        };
        assert!(graph.validate(1, 1).is_err());

        // Node capacity exceeded.
        let nodes: Vec<NodeDoc> = (0..=MAX_NODES)
            .map(|i| NodeDoc::ChannelStrip {
                node_id: 2 + i as u64,
                track: 0,
                gain: 0.8,
                pan: 0.0,
                muted: false,
            })
            .collect();
        let graph = GraphDoc {
            nodes,
            cables: Vec::new(),
        };
        assert!(graph.validate(1, 0).is_err());
    }

    #[test]
    fn next_node_id_and_track_lookups() {
        let graph = default_graph_doc(2);
        assert_eq!(graph.next_node_id(), (MASTER_NODE_ID + 1) + 2 * 3);
        assert_eq!(
            graph.instrument_of_track(1).map(NodeDoc::node_id),
            Some(MASTER_NODE_ID + 4)
        );
        assert!(graph.instrument_of_track(2).is_none());
        assert_eq!(graph.auto_endpoint(0), None);
    }

    #[test]
    fn voice_spec_ron_roundtrip() {
        for voice in [
            VoiceSpec::Builtin,
            VoiceSpec::Clap {
                path: "a.clap".to_string(),
                plugin_id: "x".to_string(),
            },
            VoiceSpec::Vst3 {
                path: "b.vst3".to_string(),
                class_id: "cid".to_string(),
            },
        ] {
            let text = ron::to_string(&voice).expect("serialization failed");
            let back: VoiceSpec = ron::from_str(&text).expect("deserialization failed");
            assert_eq!(voice, back);
        }
    }

    #[test]
    fn automation_value_at_segments() {
        let mut auto = AutomationTrack {
            enabled: true,
            ..AutomationTrack::default()
        };
        auto.points = vec![
            AutoPoint {
                tick: 0,
                value: 1.0,
            },
            AutoPoint {
                tick: 96,
                value: 0.5,
            },
            AutoPoint {
                tick: 192,
                value: 0.0,
            },
        ];
        assert!((auto.value_at(0.0).unwrap() - 1.0).abs() < 1e-6);
        assert!(
            (auto.value_at(48.0).unwrap() - 0.75).abs() < 1e-6,
            "linear midpoint"
        );
        assert!((auto.value_at(192.0).unwrap()).abs() < 1e-6);
        assert!(
            (auto.value_at(1_000.0).unwrap()).abs() < 1e-6,
            "holds the end value past the tail"
        );
        assert!(
            (auto.value_at(-5.0).unwrap() - 1.0).abs() < 1e-6,
            "holds the end value before the head"
        );

        auto.enabled = false;
        assert!(
            auto.value_at(48.0).is_none(),
            "disabled produces no automation"
        );
        auto.points.clear();
        auto.enabled = true;
        assert!(
            auto.value_at(48.0).is_none(),
            "empty curve produces no automation"
        );
    }

    #[test]
    fn automation_upsert_keeps_order() {
        let mut auto = AutomationTrack::default();
        auto.upsert_point(AutoPoint {
            tick: 96,
            value: 0.5,
        });
        auto.upsert_point(AutoPoint {
            tick: 0,
            value: 1.0,
        });
        auto.upsert_point(AutoPoint {
            tick: 48,
            value: 0.75,
        });
        auto.upsert_point(AutoPoint {
            tick: 48,
            value: 0.8,
        }); // Overwrites the same tick
        let ticks: Vec<u32> = auto.points.iter().map(|p| p.tick).collect();
        assert_eq!(
            ticks,
            vec![0, 48, 96],
            "anchors stay ascending and the same tick overwrites"
        );
        assert!((auto.points[1].value - 0.8).abs() < 1e-6);
    }

    #[test]
    fn automation_remove_near() {
        let mut auto = AutomationTrack::default();
        auto.upsert_point(AutoPoint {
            tick: 48,
            value: 0.5,
        });
        assert!(auto.remove_near(50, 6), "should hit within tolerance");
        assert!(auto.points.is_empty());
        assert!(
            !auto.remove_near(0, 6),
            "removal on an empty curve returns false"
        );
    }

    #[test]
    fn clip_ron_roundtrip() {
        use super::{Clip, Note};
        let mut clip = Clip::default();
        clip.notes.push(Note {
            start: 0,
            length: 48,
            note: 60,
            velocity: 0.9,
        });
        clip.notes.push(Note {
            start: 24,
            length: 24,
            note: 64,
            velocity: 1.0,
        });
        let text = ron::to_string(&clip).expect("serialization failed");
        let back: Clip = ron::from_str(&text).expect("deserialization failed");
        assert_eq!(clip, back);
    }

    #[test]
    fn note_partial_fields_use_defaults() {
        use super::Note;
        let back: Note = ron::from_str("(start: 48, note: 72)").expect("parse failed");
        assert_eq!(
            back.length, TICKS_PER_STEP,
            "note length defaults to one 16th note"
        );
        assert!((back.velocity - 0.9).abs() < 1e-6);
    }

    #[test]
    fn midi_track_defaults_and_delay_params() {
        let track = MidiTrack::default();
        assert_eq!(track.source, TrackSource::Steps);
        assert_eq!(track.pattern.steps.len(), PATTERN_STEPS);
        assert_eq!(track.clip.loop_ticks, TICKS_PER_BAR);
        let delay = DelayParams::default();
        assert!(delay.mix.abs() < 1e-6, "delay defaults to bypass");
        let _synth: SynthParams = ron::from_str("()").unwrap_or_default();
    }
}
