//! Node patch (P5d MVP, spec §9 P5d): egui self-drawn node graph view and cable editing.
//!
//! Responsibility boundary: the patch only does **view + interaction intent** — pan (middle
//! drag)/zoom (wheel), node box drawing (color-coded by type), cable drawing, drag-to-connect
//! interaction, point/marquee selection and deletion, right-click node adding, Ctrl+right-drag
//! cable cutting (Blender-style knife) and cable-midpoint insert/delete menus. All side
//! effects are returned via [`PatchAction`] and applied to the document + engine in one place
//! by `GlitcherZApp::apply_patch_actions` (the patch never touches the engine/guards directly).
//!
//! Coordinate convention: node positions are stored in **graph space** and persisted with
//! the project (`GraphDoc::layout`, exported on save / imported on load; P5c dual identity
//! — the patch deals with document node_ids + fixed entities and is unaware of engine ids).

use std::collections::HashMap;

use eframe::egui;
use egui::{Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};

use glitcherz_model::{
    AutoParam, BuiltinEffectKind, CableDoc, CableKind, ChannelParam, GraphDoc, LayoutDoc,
    MAX_AUDIO_TRACKS, MAX_AUTO_TRACKS, MAX_MIDI_TRACKS, MAX_MIXER_CHANNELS, MIN_MIXER_CHANNELS,
    MidiSource, NodeDoc, NodeParam, PortRef, VoiceSpec,
};

use crate::plugin_catalog::PluginFormat;

/// Inline parameter identity for built-in node kinds (G5, spec
/// `plugin-gui.md` §7): one small slider row under the node body. The
/// `ParamId` translation lives in the app glue (the patch stays
/// engine-agnostic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineParam {
    /// Built-in synth filter cutoff (Hz).
    Cutoff,
    /// Built-in synth filter resonance.
    Resonance,
    /// Built-in effect parameter, addressed by its **descriptor slot**
    /// (spec `effects.md` D5) — one variant for every built-in effect.
    EffectSlot(u8),
    /// Mixer output-stage pan.
    MixerOutPan,
    /// Feedback tap gain (P6).
    FeedbackGain,
    /// Sample player root note (U5, spec `sample-player.md` D10).
    SampleRoot,
    /// Sample player output gain (U5).
    SampleGain,
    /// SoundFont output gain (U13): a slider row.
    Sf2Gain,
    /// SoundFont bank selector (U13): a combo row, not a slider.
    Sf2Bank,
    /// SoundFont patch (program) selector (U13): a combo row.
    Sf2Program,
    /// Mixer channel fader (U14), addressed by channel index. Draws as the
    /// strip beside the channel's own input port, not as a bottom row — a
    /// console works because each fader sits on the line it governs.
    MixerGain(u8),
    /// Mixer channel mute toggle (U14): a discrete chip, value 0 / 1.
    MixerMute(u8),
    /// Mixer channel solo toggle (U14): a discrete chip, value 0 / 1.
    MixerSolo(u8),
}

/// How an inline row is presented (U13). Discrete selectors such as a
/// SoundFont bank cannot be dragged: their row draws a chip and opens a
/// popup list instead of a slider rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Slider,
    Combo,
}

impl InlineParam {
    /// Presentation of this parameter's row.
    fn row_kind(self) -> RowKind {
        match self {
            InlineParam::Sf2Bank | InlineParam::Sf2Program => RowKind::Combo,
            _ => RowKind::Slider,
        }
    }
}

/// Inline parameter addressing: built-in registry entry or an external
/// plugin parameter id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InlineParamId {
    BuiltIn(InlineParam),
    Plugin(u32),
}

/// Static registry entry: label + document read/write accessor key.
#[derive(Debug, Clone, Copy)]
struct InlineParamSpec {
    param: InlineParam,
    label: &'static str,
}

/// Built-in synth inline rows (cutoff / resonance).
const SYNTH_INLINE: [InlineParamSpec; 2] = [
    InlineParamSpec {
        param: InlineParam::Cutoff,
        label: "cut",
    },
    InlineParamSpec {
        param: InlineParam::Resonance,
        label: "res",
    },
];

/// Mixer inline row (output-stage pan; channel parameters stay on the
/// mixer console — spec §7).
const MIXER_INLINE: [InlineParamSpec; 1] = [InlineParamSpec {
    param: InlineParam::MixerOutPan,
    label: "pan",
}];

/// Feedback tap inline row (P6): the loop amount.
const FEEDBACK_INLINE: [InlineParamSpec; 1] = [InlineParamSpec {
    param: InlineParam::FeedbackGain,
    label: "fb",
}];

/// Sample player inline rows (U5): root note + gain (spec
/// `sample-player.md` D10 — the two parameters that shape the sound most).
const SAMPLE_INLINE: [InlineParamSpec; 2] = [
    InlineParamSpec {
        param: InlineParam::SampleRoot,
        label: "root",
    },
    InlineParamSpec {
        param: InlineParam::SampleGain,
        label: "gain",
    },
];

/// SoundFont inline rows (U13): the preset address and the output trim —
/// the three things a SoundFont node can be adjusted on without opening the
/// Inspector. Bank / patch are combos (see [`RowKind`]), gain is a slider.
const SF2_INLINE: [InlineParamSpec; 3] = [
    InlineParamSpec {
        param: InlineParam::Sf2Bank,
        label: "bank",
    },
    InlineParamSpec {
        param: InlineParam::Sf2Program,
        label: "patch",
    },
    InlineParamSpec {
        param: InlineParam::Sf2Gain,
        label: "gain",
    },
];

/// Static inline-parameter registry for one node (G5 spec §7). SoundFont
/// instruments expose bank / patch / gain (U13); plugin nodes take their
/// rows from the session enumeration instead (first 2 parameters — see
/// [`PatchDeps::plugin_inline`]).
fn inline_params_of(node: &NodeDoc) -> Vec<InlineParamSpec> {
    match node {
        NodeDoc::Instrument {
            voice: VoiceSpec::Builtin,
            ..
        } => SYNTH_INLINE.to_vec(),
        NodeDoc::Instrument {
            voice: VoiceSpec::Sample { .. },
            ..
        } => SAMPLE_INLINE.to_vec(),
        NodeDoc::Instrument {
            voice: VoiceSpec::SoundFont { .. },
            ..
        } => SF2_INLINE.to_vec(),
        // Table-driven from the engine descriptor (spec `effects.md` D6): the
        // row order, labels and count all come from the effect's static table.
        // Only slots flagged `INLINE` get a row — an eight-parameter EQ would
        // otherwise turn the canvas node into a column of sliders.
        NodeDoc::Effect {
            voice: VoiceSpec::Builtin | VoiceSpec::SoundFont { .. },
            params,
            ..
        } => {
            let descriptor = crate::effect::descriptor(params);
            descriptor
                .inline_slots()
                .map(|slot| InlineParamSpec {
                    param: InlineParam::EffectSlot(slot),
                    label: descriptor.spec(slot).map_or("", |spec| spec.short),
                })
                .collect()
        }
        NodeDoc::Mixer { .. } => MIXER_INLINE.to_vec(),
        NodeDoc::Feedback { .. } => FEEDBACK_INLINE.to_vec(),
        _ => Vec::new(),
    }
}

/// One external plugin inline control (G5): the first enumerated
/// parameters of a plugin node, with the session's current value and
/// range. CLAP carries raw plugin ranges; VST3 normalized 0..1 (the same
/// domain split as the panel sliders).
#[derive(Debug, Clone)]
pub struct InlinePluginControl {
    /// Plugin-side parameter id.
    pub param_id: u32,
    /// Short row label.
    pub label: String,
    /// Current value (session cache or plugin default).
    pub value: f64,
    /// Slider range minimum.
    pub min: f64,
    /// Slider range maximum.
    pub max: f64,
}

/// Value-text precision of one inline row follows the parameter span: two
/// decimals for -1..=1-style ranges, one for tens, none for hertz-wide spans
/// (the egui-native slider picked this automatically; the drawn one needs it
/// spelled out).
fn fmt_inline_value(value: f64, min: f64, max: f64) -> String {
    let span = (max - min).abs();
    if span <= 2.0 {
        format!("{value:.2}")
    } else if span <= 20.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    }
}

/// Normalized position of `value` inside `min..=max` (0.5 for a degenerate
/// range instead of a NaN that would drift the handle off the rail).
fn inline_ratio(value: f64, min: f64, max: f64) -> f64 {
    if (max - min).abs() < f64::EPSILON {
        0.5
    } else {
        ((value - min) / (max - min)).clamp(0.0, 1.0)
    }
}

/// Layout of one inline row (already screen space): label anchor, rail rect,
/// value anchor and the interactive hit rect. Shared by drawing and tests so
/// the row cannot spill past its own box or overlap its neighbours — the
/// failure mode that motivated replacing the egui-native slider (whose font
/// and rail ignore the row rect).
fn inline_row_parts(row: Rect, z: f32) -> (Pos2, Rect, Pos2, Rect) {
    let mid = row.center().y;
    let label = Pos2::new(row.left() + INLINE_INSET * z, mid);
    let rail_left = row.left() + (INLINE_INSET + INLINE_LABEL_WIDTH) * z;
    let hit_right = row.right() - INLINE_INSET * z;
    let rail = Rect::from_min_max(
        Pos2::new(rail_left, mid - 1.5 * z),
        Pos2::new(hit_right - INLINE_VALUE_WIDTH * z, mid + 1.5 * z),
    );
    let value = Pos2::new(hit_right, mid);
    let hit = Rect::from_min_max(
        Pos2::new(rail_left, row.top()),
        Pos2::new(hit_right, row.bottom()),
    );
    (label, rail, value, hit)
}

/// Current document value + slider range of one built-in inline
/// parameter (G5): reads the node's own fields (the ranges mirror the
/// panel sliders — one source of slider geometry per parameter).
fn inline_doc_value(node: &NodeDoc, param: InlineParam) -> (f64, f64, f64) {
    use crate::{CUTOFF_RANGE, RES_RANGE};
    match (node, param) {
        (NodeDoc::Instrument { synth, .. }, InlineParam::Cutoff) => (
            synth.cutoff_hz as f64,
            *CUTOFF_RANGE.start() as f64,
            *CUTOFF_RANGE.end() as f64,
        ),
        (NodeDoc::Instrument { synth, .. }, InlineParam::Resonance) => (
            synth.resonance as f64,
            *RES_RANGE.start() as f64,
            *RES_RANGE.end() as f64,
        ),
        (NodeDoc::Effect { params, .. }, InlineParam::EffectSlot(slot)) => {
            let (min, max) = crate::effect::range(params, slot);
            (
                f64::from(crate::effect::value(params, slot)),
                f64::from(min),
                f64::from(max),
            )
        }
        (NodeDoc::Mixer { out_pan, .. }, InlineParam::MixerOutPan) => (*out_pan as f64, -1.0, 1.0),
        // U14: the channel strips read the same document fields the Inspector
        // console does, so the two surfaces cannot drift apart.
        (NodeDoc::Mixer { channels, .. }, InlineParam::MixerGain(ch)) => {
            let gain = channels.get(ch as usize).map_or(0.0, |c| c.gain);
            (
                f64::from(gain),
                f64::from(*crate::TRACK_GAIN_RANGE.start()),
                f64::from(*crate::TRACK_GAIN_RANGE.end()),
            )
        }
        (NodeDoc::Mixer { channels, .. }, InlineParam::MixerMute(ch)) => (
            if channels.get(ch as usize).is_some_and(|c| c.muted) {
                1.0
            } else {
                0.0
            },
            0.0,
            1.0,
        ),
        (NodeDoc::Mixer { channels, .. }, InlineParam::MixerSolo(ch)) => (
            if channels.get(ch as usize).is_some_and(|c| c.solo) {
                1.0
            } else {
                0.0
            },
            0.0,
            1.0,
        ),
        (NodeDoc::Feedback { gain, .. }, InlineParam::FeedbackGain) => (*gain as f64, 0.0, 1.0),
        (
            NodeDoc::Instrument {
                voice: VoiceSpec::Sample { root_note, .. },
                ..
            },
            InlineParam::SampleRoot,
        ) => (f64::from(*root_note), 0.0, 127.0),
        (
            NodeDoc::Instrument {
                voice: VoiceSpec::Sample { gain, .. },
                ..
            },
            InlineParam::SampleGain,
        ) => (f64::from(*gain), 0.0, 2.0),
        (
            NodeDoc::Instrument {
                voice: VoiceSpec::SoundFont { bank, .. },
                ..
            },
            InlineParam::Sf2Bank,
        ) => (f64::from(*bank), 0.0, 255.0),
        (
            NodeDoc::Instrument {
                voice: VoiceSpec::SoundFont { program, .. },
                ..
            },
            InlineParam::Sf2Program,
        ) => (f64::from(*program), 0.0, 255.0),
        (
            NodeDoc::Instrument {
                voice: VoiceSpec::SoundFont { gain, .. },
                ..
            },
            InlineParam::Sf2Gain,
        ) => (f64::from(*gain), 0.0, 2.0),
        _ => (0.0, 0.0, 1.0),
    }
}

/// One drawn inline row (G5 / U13): everything the drawing loop needs,
/// already resolved from the document and the session snapshots.
struct InlineRow {
    /// Row caption (`cut`, `bank`, `gain`, a plugin parameter name).
    label: String,
    /// Parameter the row edits.
    param: InlineParamId,
    /// Current value.
    value: f64,
    /// Slider range (ignored by combo rows).
    min: f64,
    max: f64,
    /// Combo rows: `(value, caption)`; empty for sliders, and empty for a
    /// combo whose list is unavailable (no font loaded).
    choices: Vec<(u8, String)>,
}

/// Choice list of a SoundFont combo row (U13).
///
/// Bank lists the font's distinct banks; patch lists the patches of the
/// **current bank**, sorted by program number so the list reads in GM order
/// instead of file order. Captions are truncated to the node's width — the
/// popup is 220 px wide, a MuseScore-General patch name is not.
fn sf2_choices(presets: &[(u8, u8, String)], param: InlineParam, bank: u8) -> Vec<(u8, String)> {
    match param {
        InlineParam::Sf2Bank => {
            let mut banks: Vec<u8> = presets.iter().map(|(bank, _, _)| *bank).collect();
            banks.sort_unstable();
            banks.dedup();
            banks
                .into_iter()
                .map(|bank| (bank, format!("Bank {bank}")))
                .collect()
        }
        InlineParam::Sf2Program => {
            let mut patches: Vec<(u8, String)> = presets
                .iter()
                .filter(|(preset_bank, _, _)| *preset_bank == bank)
                .map(|(_, program, name)| (*program, format!("{program}. {}", truncate(name, 22))))
                .collect();
            patches.sort_unstable_by_key(|(program, _)| *program);
            patches
        }
        _ => Vec::new(),
    }
}

/// Patch entity identity: document nodes + fixed entities (master output /
/// the arrangement timeline).
///
/// T4 (spec `.agents/spec/timeline.md`): the per-track source boxes merged into
/// **one explicit Timeline node** — no inputs, `n` outputs (n = MIDI tracks +
/// control tracks + audio tracks, laid out MIDI → Curve → Audio), undeletable.
/// MIDI / control ports keep their `PortRef::MidiTrack/AutoTrack` semantics;
/// audio ports fold in the per-track `NodeDoc::AudioTrack` generator (U6),
/// whose box would otherwise stand alone. This is a presentation-layer merge —
/// the document topology is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatchId {
    /// Document node (persisted node_id, ≥ 2).
    Node(u64),
    /// Master output (reserved identity, D6).
    Master,
    /// The arrangement timeline (one box; one output port per track).
    Timeline,
    /// The timeline **input terminal** (U3b, spec `tempo-meter.md` D1): a
    /// fixed pure sink mirroring [`PatchId::Timeline`]. No outputs, one input
    /// port (`Tempo`), always present, never deletable or duplicable — the
    /// identity is the singleton guarantee, so no validation or guard code is
    /// needed (D2).
    TimelineIn,
}

/// Category of a node being added (spec `plugin-browser.md` D1): the add menu
/// asks for the category first, then for the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeCategory {
    /// MIDI entry (built-in synth, SoundFont, or an instrument plugin).
    Instrument,
    /// Audio processor (built-in delay or an effect plugin).
    Effect,
}

impl NodeCategory {
    /// Plugin-catalog classification flag for this category (spec D9).
    pub fn is_instrument(self) -> bool {
        matches!(self, NodeCategory::Instrument)
    }
}

/// Where a picker pick lands (spec `plugin-browser.md` §6.1 `PickerMode`).
///
/// The picker itself is identical in both cases; only the receiver differs, so
/// the same popup serves the add menu and the cable-midpoint menu.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PickerPlace {
    /// A new dangling node at `at` (D6: no cables until the user draws them).
    Add {
        /// Graph-space creation position.
        at: Pos2,
    },
    /// Spliced into the audio cable at document index `cable`: `A → B` becomes
    /// `A → new → B`. `at` is where the new node is placed.
    Insert {
        /// Index into [`GraphDoc::cables`].
        cable: usize,
        /// Graph-space creation position.
        at: Pos2,
    },
}

impl PickerPlace {
    /// Graph-space position the pick will be created at.
    pub fn at(self) -> Pos2 {
        match self {
            PickerPlace::Add { at } | PickerPlace::Insert { at, .. } => at,
        }
    }
}

/// The source a node is born with (spec `plugin-browser.md` §7).
///
/// Unlike the old `AddNodeKind` (which mixed "category" and "concrete node"),
/// this already carries the plugin identity, so [`crate::GlitcherZApp`]
/// installs the node exactly once with its real voice — no "create a built-in,
/// then swap it in the panel" double install (spec D1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeSource {
    /// Built-in synth voice.
    BuiltinSynth,
    /// Built-in effect of the given kind (spec `effects.md` D1): the roster is
    /// walked from `BuiltinEffectKind::ALL`, so a new effect needs no new
    /// `NodeSource` variant.
    Builtin(BuiltinEffectKind),
    /// SoundFont instrument (U13). The path may be **empty** — that is the
    /// normal "born empty" node, silent until its `Load File…` button swaps a
    /// real voice in.
    SoundFont {
        /// `.sf2` / `.sf3` path, or empty for a node waiting for one.
        path: String,
    },
    /// Wav sample player (U5, spec `sample-player.md`): born with default
    /// root 60 / Gate / unity gain.
    Sample {
        /// `.wav` path picked in the file dialog.
        path: String,
    },
    /// CLAP instrument plugin.
    ClapInstrument {
        /// Module path.
        path: String,
        /// CLAP plugin id.
        plugin_id: String,
    },
    /// VST3 instrument plugin.
    Vst3Instrument {
        /// Module path.
        path: String,
        /// VST3 component class id.
        class_id: String,
    },
    /// CLAP effect plugin.
    ClapEffect {
        /// Module path.
        path: String,
        /// CLAP plugin id.
        plugin_id: String,
    },
    /// VST3 effect plugin.
    Vst3Effect {
        /// Module path.
        path: String,
        /// VST3 component class id.
        class_id: String,
    },
    /// Mixer node (P5f D15/D21): the explicit multi-channel summing point.
    Mixer,
    /// Feedback tap node (P6, spec `node-graph.md` D10): sums its inputs one
    /// block late, which is what makes a loop legal.
    Feedback,
    /// Virtual keyboard node (G6, spec `plugin-gui.md` §8): document-only
    /// routing node.
    Keyboard,
}

impl NodeSource {
    /// Whether this source can be spliced into an audio cable (`A → x → B`).
    ///
    /// Instruments are excluded: an instrument spliced into an audio cable has
    /// no MIDI feed, so it would be born silent and the cable would carry
    /// nothing. Mixers are single-input-agnostic and perfectly legal.
    pub fn fits_audio_insert(&self) -> bool {
        matches!(
            self,
            NodeSource::Builtin(_)
                | NodeSource::ClapEffect { .. }
                | NodeSource::Vst3Effect { .. }
                | NodeSource::Mixer
        )
    }
}

/// Interaction intents produced within one patch frame (applied by the caller; the patch has
/// no side effects).
#[derive(Debug, Clone)]
pub enum PatchAction {
    /// Add a new node with `source` at graph-space position `at` (right-click
    /// menu). The new node is born dangling (no cables; D6 dangling semantics)
    /// — the user wires it up.
    AddNode {
        source: NodeSource,
        at: Pos2,
    },
    /// Open the plugin picker for `category` / `format` (spec `plugin-browser.md`
    /// D4/D5): `None` category = the global `Search Plug-ins…` entry, whose
    /// pick decides the node category from the entry's own classification.
    /// `place` says whether the pick becomes a dangling node or is spliced into
    /// a cable.
    OpenPluginPicker {
        /// Category the leaf menu asked for (`None` = both).
        category: Option<NodeCategory>,
        /// Binary format the leaf menu asked for (`None` = both).
        format: Option<PluginFormat>,
        /// Where the pick lands.
        place: PickerPlace,
    },
    /// Open the SoundFont file dialog for an **existing** node and swap its
    /// voice (U13; the patch cannot run a modal dialog inside the menu
    /// closure). The app parses, resolves a preset that exists in the new
    /// file and swaps the processor in place — same function the Inspector's
    /// `Load File…` calls.
    LoadSoundFont {
        node: u64,
    },
    /// Open the wav sample file dialog and create a sample instrument from
    /// the pick (U5; same modal-dialog constraint as `PickSoundFont`).
    PickSample {
        at: Pos2,
    },
    /// Re-scan every plugin module, ignoring the cached stats (U9 E2, spec
    /// `plugin-browser.md` §5.4). The catalog queues the work; the caller only
    /// restarts the scan.
    RescanPlugins,
    /// Audio cable drag: source document node → target (node or master output).
    /// `in_port` binds the cable to the target's input port — the mixer channel
    /// (D17; 0 for single-input nodes / master).
    ConnectAudio {
        from_doc: u64,
        to: PortRef,
        in_port: u8,
    },
    /// MIDI cable drag: timeline track → instrument document node.
    ConnectMidi {
        track: usize,
        to_doc: u64,
    },
    /// MIDI cable drag out of a virtual keyboard node (G6): keyboard node →
    /// instrument document node (the only node→node MIDI edge).
    ConnectKeyboardMidi {
        from_doc: u64,
        to_doc: u64,
    },
    /// Automation cable drag (parameter already picked by the user from the popup menu).
    ConnectAuto {
        index: usize,
        to_doc: u64,
        param: AutoParam,
    },
    /// Automation cable drag onto the fixed Timeline In terminal (U3b): the
    /// single `Tempo` port needs no parameter popup (D7), so the cable is
    /// committed directly.
    ConnectTempoIn {
        index: usize,
    },
    /// Delete a cable (index into the document cable table).
    DeleteCable(usize),
    /// Delete a node (document node_id; any instrument / effect / channel strip is
    /// deletable — master / timeline are fixed and never selected as document nodes).
    DeleteNode(u64),
    /// Insert a new node with `source` into the audio cable at document index
    /// `cable`, replacing `A → B` with `A → new → B` (cable-midpoint menu).
    /// `at` = graph-space position where the user clicked (the new node is
    /// placed there).
    InsertNode {
        cable: usize,
        source: NodeSource,
        at: Pos2,
    },
    /// Set a node's owning track as the current editing track.
    SelectTrack(usize),
    /// Double-clicked a document node: jump to the Synth panel with that
    /// node as the editing target (spec `plugin-gui.md` §6 — the patch
    /// analogue of double-clicking a timeline clip to open the Edit view).
    OpenNode(u64),
    /// Double-clicked the fixed Timeline node: open the arrangement view
    /// (the patch analogue of double-clicking a clip into the Edit tab).
    OpenTimeline,
    /// Timeline node body: append a MIDI track / drop the last one.
    AddMidiTrack,
    RemoveLastMidiTrack,
    /// Timeline node body: append a control track / drop the selected one.
    AddControlTrack,
    RemoveControlTrack,
    /// Timeline node body: append an audio track / drop the last one (U6).
    AddAudioTrack,
    RemoveLastAudioTrack,
    /// Mixer node body: append a channel / drop the last one (D21). The
    /// document node id comes from the box the button was drawn on.
    AddMixerSlot(u64),
    RemoveMixerSlot(u64),
    /// Inline parameter slider changed on a node body (G5 spec §7): the
    /// value is delivered through the non-rebuilding `set_node_param`
    /// path by the glue.
    SetInlineParam {
        node: u64,
        param: InlineParamId,
        value: f32,
    },
}

/// Double-click gate: whether the Inspector has anything to show for a node.
/// Instruments and effects carry parameters; the mixer carries the channel
/// console. The virtual keyboard, the legacy strip and feedback taps do not,
/// so double-clicking them stays inert instead of switching to a panel that
/// would fall back to some unrelated track's instrument.
fn opens_inspector(node: Option<&NodeDoc>) -> bool {
    matches!(
        node,
        Some(NodeDoc::Instrument { .. } | NodeDoc::Effect { .. } | NodeDoc::Mixer { .. })
    )
}

/// One in-node action button (the small track / slot controls drawn inside
/// a node box).
struct NodeAction {
    /// Button caption.
    label: &'static str,
    /// Whether the action currently applies (limit reached / nothing to
    /// remove → disabled, with `hover` saying why).
    enabled: bool,
    /// Tooltip; doubles as the reason when disabled.
    hover: &'static str,
    /// Intent emitted on click.
    action: PatchAction,
}

/// Buttons of one in-node action row. The timeline owns track management
/// (row 0: MIDI tracks, row 1: control tracks, row 2: audio tracks); the
/// mixer owns its slot count. Any other entity returns no buttons.
fn node_action_buttons(id: PatchId, row: usize, deps: &PatchDeps) -> Vec<NodeAction> {
    match id {
        PatchId::Timeline if row == 0 => vec![
            NodeAction {
                label: "+ MIDI",
                enabled: deps.track_count < MAX_MIDI_TRACKS,
                hover: "Add a MIDI track",
                action: PatchAction::AddMidiTrack,
            },
            NodeAction {
                label: "- MIDI",
                enabled: deps.track_count > 0,
                hover: "Remove the last MIDI track",
                action: PatchAction::RemoveLastMidiTrack,
            },
        ],
        PatchId::Timeline if row == 1 => vec![
            NodeAction {
                label: "+ Curve",
                enabled: deps.auto_count < MAX_AUTO_TRACKS,
                hover: "Add a control (curve) track",
                action: PatchAction::AddControlTrack,
            },
            NodeAction {
                label: "- Curve",
                enabled: deps.auto_count > 0,
                hover: "Remove the selected curve track",
                action: PatchAction::RemoveControlTrack,
            },
        ],
        PatchId::Timeline if row == 2 => vec![
            NodeAction {
                label: "+ Audio",
                enabled: deps.audio_count < MAX_AUDIO_TRACKS,
                hover: "Add an audio track",
                action: PatchAction::AddAudioTrack,
            },
            NodeAction {
                label: "- Audio",
                enabled: deps.audio_count > 0,
                hover: "Remove the last audio track",
                action: PatchAction::RemoveLastAudioTrack,
            },
        ],
        PatchId::Timeline => Vec::new(),
        PatchId::Node(doc_id) => {
            // U13: a SoundFont node owns its file, so the swap lives on the
            // box. Empty path = "Load File…", loaded = "Replace…" — the same
            // button in both states, because there is nothing to "reset to".
            if let Some(NodeDoc::Instrument {
                voice: VoiceSpec::SoundFont { path, .. },
                ..
            }) = deps.doc.node_by_id(doc_id)
            {
                return if row == 0 {
                    vec![NodeAction {
                        label: if path.is_empty() {
                            "Load File…"
                        } else {
                            "Replace…"
                        },
                        enabled: true,
                        hover: "Pick a .sf2 / .sf3 file for this node",
                        action: PatchAction::LoadSoundFont { node: doc_id },
                    }]
                } else {
                    Vec::new()
                };
            }
            let inputs = match deps.doc.node_by_id(doc_id) {
                Some(NodeDoc::Mixer { inputs, .. }) => *inputs as usize,
                _ => return Vec::new(),
            };
            // The last slot cannot be dropped while a cable still lands on it
            // (mirrors `mixer_remove_channel`, so the button says so up front
            // instead of failing silently).
            let last = inputs.saturating_sub(1) as u8;
            let occupied = deps.doc.cables.iter().any(|c| {
                c.kind == CableKind::Audio && c.to == PortRef::Node(doc_id) && c.in_port == last
            });
            vec![
                NodeAction {
                    label: "+ Slot",
                    enabled: inputs < MAX_MIXER_CHANNELS,
                    hover: "Add a mixer channel (slot)",
                    action: PatchAction::AddMixerSlot(doc_id),
                },
                NodeAction {
                    label: "- Slot",
                    enabled: inputs > MIN_MIXER_CHANNELS && !occupied,
                    hover: if occupied {
                        "Remove the last slot (disconnect its cable first)"
                    } else {
                        "Remove the last mixer channel (slot)"
                    },
                    action: PatchAction::RemoveMixerSlot(doc_id),
                },
            ]
        }
        _ => Vec::new(),
    }
}

/// Paints one in-node action button and reports whether it was clicked.
///
/// Hand-painted rather than `egui::Button`: a native button is never
/// shorter than `Style::spacing.interact_size.y` (18 pt), so while the patch
/// was zoomed out it overflowed its `18 * zoom` row, got centred on it and
/// painted over the node title band. Every measure here scales with `z`, the
/// caption is truncated to the button width and the painter is clipped to the
/// button rect, so a row stays inside its band at any zoom.
fn action_button(ui: &mut egui::Ui, rect: Rect, z: f32, action: &NodeAction) -> bool {
    let resp = ui.allocate_rect(rect, Sense::click());
    let hot = action.enabled && resp.hovered();
    let (fill, stroke, text) = if !action.enabled {
        (
            Color32::from_gray(34),
            Color32::from_gray(58),
            Color32::from_gray(115),
        )
    } else if hot {
        (
            Color32::from_gray(72),
            Color32::from_gray(130),
            Color32::from_gray(240),
        )
    } else {
        (
            Color32::from_gray(52),
            Color32::from_gray(92),
            Color32::from_gray(205),
        )
    };
    let r = rect.shrink(0.5 * z);
    let painter = ui.painter().with_clip_rect(r);
    painter.rect_filled(r, 2.0 * z, fill);
    painter.rect_stroke(
        r,
        2.0 * z,
        Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    // Font capped by the row height: a galley is ~1.3x its font size, so a
    // fixed 11 pt caption is what used to poke out of a zoomed-out row.
    let px = (11.0 * z).min(r.height() * 0.72).max(1.0);
    let mut job = egui::text::LayoutJob::simple_singleline(
        action.label.to_owned(),
        FontId::proportional(px),
        text,
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width((r.width() - 4.0 * z).max(1.0));
    let galley = painter.layout_job(job);
    let pos = Pos2::new(
        r.center().x - galley.size().x * 0.5,
        r.center().y - galley.size().y * 0.5,
    );
    painter.galley(pos, galley, text);
    let resp = resp
        .on_hover_cursor(if action.enabled {
            egui::CursorIcon::PointingHand
        } else {
            egui::CursorIcon::Default
        })
        .on_hover_text(action.hover);
    action.enabled && resp.clicked()
}

/// Snapshot of external data the patch depends on (immutable borrows to avoid borrow conflicts).
pub struct PatchDeps<'a> {
    /// Project graph document (nodes + cables).
    pub doc: &'a GraphDoc,
    /// Active MIDI track count.
    pub track_count: usize,
    /// Automation track count.
    pub auto_count: usize,
    /// Audio track count (U6): drives the timeline node's third action row
    /// and its audio output ports.
    pub audio_count: usize,
    /// Given a document node id, return automatable parameter options `(label, parameter)`.
    pub auto_options: &'a dyn Fn(u64) -> Vec<(String, AutoParam)>,
    /// Given a document node id, return its external-plugin inline
    /// controls (first enumerated parameters, G5 spec §7). Empty for
    /// built-in nodes — their rows come from the static registry.
    pub plugin_inline: &'a dyn Fn(u64) -> Vec<InlinePluginControl>,
    /// Given a SoundFont instrument node, return its font's preset table
    /// `(bank, program, name)` (U13). Empty when no file is loaded or the
    /// library is not resident this session — the node's bank / patch combos
    /// then stay inert instead of offering a list the engine cannot play.
    pub sf2_presets: &'a dyn Fn(u64) -> Vec<(u8, u8, String)>,
}

/// Cable being dragged (source = the entity + output port owning the drag).
#[derive(Debug, Clone, Copy)]
struct CableDrag {
    from: PatchId,
    /// Output port index (only [`PatchId::Timeline`] has more than one).
    port: usize,
}

/// Patch cross-frame state.
#[derive(Debug)]
pub struct PatchState {
    /// Viewport pan (graph-space coordinates; (0,0) when the graph origin is at the patch
    /// top-left).
    pan: Vec2,
    /// Zoom factor (0.35..3.0; starts at 1.0 — `f32::default()` would be 0.0 and collapse
    /// the whole graph onto the pan point, the top-left corner).
    zoom: f32,
    /// Entity → graph-space position (session memory; missing entries are auto-placed along
    /// track chains).
    positions: HashMap<PatchId, Pos2>,
    /// Current selection (nodes and/or cables).
    selection: Selection,
    /// Drag in progress.
    drag: Option<DragKind>,
    /// Parameters awaiting selection after dropping an automation cable drag: `(track index,
    /// target document node, popup position)`.
    pending_auto: Option<(usize, u64, Pos2)>,
    /// Graph-space position where the add-node context menu was opened (consumed when a
    /// node kind is picked).
    add_menu_at: Option<Pos2>,
    /// Ctrl + right-drag cable cut in progress: screen-space pointer path.
    cut_path: Option<Vec<Pos2>>,
    /// Cable-midpoint menu awaiting a pick: `(cable index, popup screen position,
    /// graph-space insert position, frame it was opened)`. `u64::MAX` marks "opened this
    /// frame" so the same-frame click does not immediately close it.
    pending_cable_menu: Option<(usize, Pos2, Pos2, u64)>,
    /// SoundFont combo row awaiting a pick (U13): `(node, parameter, popup
    /// screen position, frame it was opened)` — same frame-stamp trick as
    /// the cable menu, because a click opens it.
    pending_sf2_choice: Option<(u64, InlineParam, Pos2, u64)>,
}

impl Default for PatchState {
    fn default() -> Self {
        Self {
            pan: Vec2::ZERO,
            zoom: 1.0,
            positions: HashMap::default(),
            selection: Selection::default(),
            drag: None,
            pending_auto: None,
            add_menu_at: None,
            cut_path: None,
            pending_cable_menu: None,
            pending_sf2_choice: None,
        }
    }
}

/// Current selection (multi-select: marquee box selection can select several nodes).
#[derive(Debug, Default, Clone)]
struct Selection {
    nodes: Vec<PatchId>,
    cables: Vec<usize>,
}

impl Selection {
    fn contains_node(&self, id: PatchId) -> bool {
        self.nodes.contains(&id)
    }

    fn toggle_node(&mut self, id: PatchId) {
        if let Some(i) = self.nodes.iter().position(|&n| n == id) {
            self.nodes.remove(i);
        } else {
            self.nodes.push(id);
        }
    }

    fn toggle_cable(&mut self, i: usize) {
        if let Some(k) = self.cables.iter().position(|&c| c == i) {
            self.cables.remove(k);
        } else {
            self.cables.push(i);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum DragKind {
    /// Drag a node (all selected nodes move together when the pressed node is
    /// part of the current selection).
    Node(PatchId),
    /// Cable dragging in progress (source output port).
    Cable(CableDrag),
    /// Marquee box selection in progress (`start` = screen-space anchor,
    /// `add` = Shift append mode instead of replacing the selection).
    BoxSelect { start: Pos2, add: bool },
}

/// Audio node box size.
const NODE_SIZE: Vec2 = Vec2::new(150.0, 64.0);
/// Virtual keyboard box (G6): a compact box with a mini key strip.
const KEYBOARD_SIZE: Vec2 = Vec2::new(150.0, 56.0);
/// Mixer port-area minimum height (scales with the channel/input count).
const NODE_HEIGHT_MIN: f32 = 64.0;
/// Vertical spacing between mixer input ports (P5f: one port per channel).
const PORT_SPACING: f32 = 30.0;
/// Title band at the top of a node box. Ports and inline rows are measured
/// from below it, so a port dot never lands on the label.
const NODE_TITLE_BAND: f32 = 24.0;
/// Height of one in-node action row (the track / slot buttons).
const ACTION_ROW_HEIGHT: f32 = 18.0;
/// Below this zoom the in-node buttons are too small to hit; they are left
/// out (box and ports still draw).
const ACTION_MIN_ZOOM: f32 = 0.6;
/// Inline parameter row height (graph space; G5 — the node body grows
/// downward by `rows * INLINE_ROW_HEIGHT`).
const INLINE_ROW_HEIGHT: f32 = 16.0;
/// Left/right inset of the inline slider rows inside the node box.
const INLINE_INSET: f32 = 8.0;
/// Width reserved for the inline row label (graph space).
const INLINE_LABEL_WIDTH: f32 = 34.0;
/// Width reserved for the inline row value text (graph space).
const INLINE_VALUE_WIDTH: f32 = 30.0;
/// Below this zoom the inline slider rows are not drawn: the egui-native
/// slider cannot shrink with the row rect (its font and rail come from the
/// fixed style), so half-size rows only overflow the box. Geometry still
/// reserves the rows (stable box / port layout while zooming), mirroring the
/// action-button threshold.
const INLINE_MIN_ZOOM: f32 = ACTION_MIN_ZOOM;
/// Mixer channel strip height (U14, graph space). Kept under `PORT_SPACING`
/// so two neighbouring strips never touch, and tall enough for the `M` / `S`
/// chips to stay clickable at [`ACTION_MIN_ZOOM`].
const CHANNEL_STRIP_HEIGHT: f32 = 22.0;
/// Left edge of a channel strip, clear of the port dot and of
/// `PORT_HIT_RADIUS` — a press inside the dot must still start a cable drag.
const CHANNEL_STRIP_LEFT: f32 = 22.0;
/// Width of one `M` / `S` toggle chip.
const CHANNEL_TOGGLE_W: f32 = 18.0;
/// Gap between the two toggles and between the fader and the mute chip.
const CHANNEL_TOGGLE_GAP: f32 = 3.0;
/// Timeline box size per output port row (width fixed, height scales).
const TIMELINE_WIDTH: f32 = 150.0;
const TIMELINE_ROW_HEIGHT: f32 = 26.0;
/// Action rows inside the timeline box: `+ MIDI` / `- MIDI`, `+ Curve` /
/// `- Curve` and `+ Audio` / `- Audio`.
const TIMELINE_ACTION_ROWS: usize = 3;
/// Timeline header height: title band + the three action rows.
const TIMELINE_HEADER: f32 = NODE_TITLE_BAND + TIMELINE_ACTION_ROWS as f32 * ACTION_ROW_HEIGHT;
/// Timeline In terminal size (U3b): one port row, same width as the timeline
/// box it mirrors.
const TIMELINE_IN_SIZE: Vec2 = Vec2::new(TIMELINE_WIDTH, 52.0);
/// Fixed patch height.
const CANVAS_HEIGHT: f32 = 300.0;
/// Output port circle radius / hit radius (generous so port drags are easy to start).
const PORT_RADIUS: f32 = 6.0;
const PORT_HIT_RADIUS: f32 = 18.0;
/// Cable hit distance.
const CABLE_HIT_DIST: f32 = 8.0;
/// Cable-cut hit distance (pointer path within this distance of a cable severs it).
const CUT_HIT_DIST: f32 = 10.0;
/// Cable midpoint control dot radius / hit radius.
const MIDPOINT_RADIUS: f32 = 4.0;
const MIDPOINT_HIT_RADIUS: f32 = 10.0;
/// Zoom range.
const ZOOM_MIN: f32 = 0.35;
const ZOOM_MAX: f32 = 3.0;

/// Timeline output port count (MIDI tracks first, then control tracks, then
/// audio tracks — matches the top-to-bottom action-row order).
fn timeline_port_count(midi_count: usize, auto_count: usize, audio_count: usize) -> usize {
    midi_count + auto_count + audio_count
}

/// Which track kind one timeline output port addresses. The three kinds are
/// laid out MIDI → Curve → Audio (the same order as the action rows), so a
/// port index decodes to exactly one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimelinePort {
    /// MIDI track outlet (`PortRef::MidiTrack`).
    Midi(usize),
    /// Control (curve) track outlet (`PortRef::AutoTrack`).
    Auto(usize),
    /// Audio track outlet (U6): its engine node feeds the cable, but the
    /// patch draws the port on the timeline box like the other kinds.
    Audio(usize),
}

/// A timeline output port index → its track kind (`port >= midi + auto` is
/// an audio track; `0..midi` MIDI, `midi..midi + auto` control).
fn timeline_port_kind(port: usize, midi_count: usize, auto_count: usize) -> TimelinePort {
    if port < midi_count {
        TimelinePort::Midi(port)
    } else if port < midi_count + auto_count {
        TimelinePort::Auto(port - midi_count)
    } else {
        TimelinePort::Audio(port - midi_count - auto_count)
    }
}

/// Timeline box size (height scales with the port count).
fn timeline_size(midi_count: usize, auto_count: usize, audio_count: usize) -> Vec2 {
    Vec2::new(
        TIMELINE_WIDTH,
        TIMELINE_HEADER
            + (TIMELINE_ROW_HEIGHT
                * timeline_port_count(midi_count, auto_count, audio_count) as f32)
                .max(TIMELINE_ROW_HEIGHT),
    )
}

/// In-node action rows: the timeline grows / shrinks tracks (two rows), the
/// mixer grows / shrinks slots (one row). Everything else has none — the
/// buttons only exist where the entity owns the thing they add.
fn action_rows(id: PatchId, deps: &PatchDeps) -> usize {
    match id {
        PatchId::Timeline => TIMELINE_ACTION_ROWS,
        PatchId::Node(node_id) => match deps.doc.node_by_id(node_id) {
            Some(node) if node.is_mixer() => 1,
            // U13: the SoundFont file button row.
            Some(NodeDoc::Instrument {
                voice: VoiceSpec::SoundFont { .. },
                ..
            }) => 1,
            _ => 0,
        },
        _ => 0,
    }
}

/// Height of the band above the port area (title + action rows), graph space.
fn header_band(id: PatchId, deps: &PatchDeps) -> f32 {
    NODE_TITLE_BAND + action_rows(id, deps) as f32 * ACTION_ROW_HEIGHT
}

/// Screen rect of one in-node action row: the `row`-th `ACTION_ROW_HEIGHT`
/// strip below the title band, inset a little vertically so the button does
/// not touch the band edges. Always inside the node box — the drawing code
/// and the tests share this one measure.
fn action_row_band(r: Rect, row: usize, zoom: f32) -> Rect {
    let pad = 1.5 * zoom;
    let top = r.top() + (NODE_TITLE_BAND + row as f32 * ACTION_ROW_HEIGHT) * zoom + pad;
    Rect::from_min_max(
        Pos2::new(r.left(), top),
        Pos2::new(
            r.right(),
            top + (ACTION_ROW_HEIGHT * zoom - 2.0 * pad).max(1.0),
        ),
    )
}

/// Buttons of one action row: they share the band width evenly, inset by
/// [`INLINE_INSET`] on both sides (same gutter as the inline rows).
fn action_button_rects(band: Rect, count: usize, zoom: f32) -> Vec<Rect> {
    let count = count.max(1);
    let gap = 4.0 * zoom;
    let width = ((band.width() - 2.0 * INLINE_INSET * zoom - gap * (count - 1) as f32)
        / count as f32)
        .max(1.0);
    (0..count)
        .map(|k| {
            Rect::from_min_size(
                Pos2::new(
                    band.left() + INLINE_INSET * zoom + k as f32 * (width + gap),
                    band.top(),
                ),
                Vec2::new(width, band.height()),
            )
        })
        .collect()
}

/// Paints the slider rail of one inline row or channel strip: a dim full-run
/// rail, a brighter run up to the handle and the handle dot itself. Shared so
/// the mixer strips and the inline rows look like one family of controls.
fn draw_fader_rail(painter: &egui::Painter, rail: Rect, t: f32, z: f32) {
    let mid_y = rail.center().y;
    let handle_x = rail.left() + t * rail.width();
    painter.line_segment(
        [
            Pos2::new(rail.left(), mid_y),
            Pos2::new(rail.right(), mid_y),
        ],
        Stroke::new(2.0 * z, Color32::from_gray(80)),
    );
    if t > 0.0 {
        painter.line_segment(
            [Pos2::new(rail.left(), mid_y), Pos2::new(handle_x, mid_y)],
            Stroke::new(2.0 * z, Color32::from_gray(160)),
        );
    }
    painter.circle_filled(
        Pos2::new(handle_x, mid_y),
        (4.0 * z).max(1.5),
        Color32::from_gray(215),
    );
}

/// Screen rect of one mixer channel strip (U14): centred on the channel's
/// input port, running from just right of the port dot to the node's right
/// inset. One strip per channel is what makes the fader sit on the line it
/// governs instead of in a detached list at the bottom of the box.
fn channel_strip_rect(port: Pos2, node: Rect, z: f32) -> Rect {
    let h = CHANNEL_STRIP_HEIGHT * z;
    Rect::from_min_max(
        Pos2::new(port.x + CHANNEL_STRIP_LEFT * z, port.y - h * 0.5),
        Pos2::new(node.right() - INLINE_INSET * z, port.y + h * 0.5),
    )
}

/// Cells inside one channel strip: the fader rail and the two toggles, laid
/// out right-to-left so the fader takes whatever width is left.
struct ChannelStripCells {
    rail: Rect,
    mute: Rect,
    solo: Rect,
}

fn channel_strip_cells(strip: Rect, z: f32) -> ChannelStripCells {
    let chip = CHANNEL_TOGGLE_W * z;
    let gap = CHANNEL_TOGGLE_GAP * z;
    let pad = 2.0 * z;
    // `S` is the outermost chip: solo is the rarer, more destructive press,
    // so it does not sit in the middle where a slip would hit it.
    let solo = Rect::from_min_max(
        Pos2::new(strip.right() - chip, strip.top() + pad),
        Pos2::new(strip.right(), strip.bottom() - pad),
    );
    let mute = solo.translate(Vec2::new(-(chip + gap), 0.0));
    let mid = strip.center().y;
    let rail = Rect::from_min_max(
        Pos2::new(strip.left(), mid - 1.5 * z),
        Pos2::new(mute.left() - 5.0 * z, mid + 1.5 * z),
    );
    ChannelStripCells { rail, mute, solo }
}

/// Paints one `M` / `S` toggle chip and reports a click. Same hand-painted
/// treatment as the action buttons: a native button cannot shrink below
/// `interact_size.y`, and the strip is 22 * zoom tall.
fn channel_toggle(
    ui: &mut egui::Ui,
    rect: Rect,
    z: f32,
    label: &str,
    active: bool,
    tint: Color32,
    hover: &str,
) -> bool {
    let resp = ui.allocate_rect(rect, Sense::click());
    let hot = resp.hovered();
    let (fill, text) = if active {
        (tint, Color32::from_gray(20))
    } else if hot {
        (Color32::from_gray(72), Color32::from_gray(235))
    } else {
        (Color32::from_gray(48), Color32::from_gray(160))
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0 * z, fill);
    painter.rect_stroke(
        rect,
        2.0 * z,
        Stroke::new(
            1.0,
            if active {
                Color32::from_gray(200)
            } else {
                Color32::from_gray(84)
            },
        ),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        FontId::proportional((9.0 * z).min(rect.height() * 0.8).max(1.0)),
        text,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(hover)
        .clicked()
}

/// Type coloring (spec P5d): stroke varies by type, fill is a dark base.
fn node_color(id: PatchId, doc: &GraphDoc) -> (Color32, Color32) {
    let stroke = match id {
        PatchId::Master => Color32::from_rgb(230, 110, 90),
        PatchId::Timeline => Color32::from_rgb(170, 120, 225),
        // The sink shares the timeline's identity colour (same subsystem).
        PatchId::TimelineIn => Color32::from_rgb(150, 105, 205),
        PatchId::Node(id) => match doc.node_by_id(id) {
            Some(NodeDoc::Instrument { .. }) => Color32::from_rgb(90, 160, 220),
            Some(NodeDoc::Effect { .. }) => Color32::from_rgb(220, 150, 70),
            Some(NodeDoc::Mixer { .. }) => Color32::from_rgb(110, 190, 120),
            Some(NodeDoc::Keyboard { .. }) => Color32::from_rgb(95, 205, 190),
            Some(NodeDoc::Feedback { .. }) => Color32::from_rgb(215, 105, 160),
            // U6: audio tracks share the sample-player family's warm tone.
            Some(NodeDoc::AudioTrack { .. }) => Color32::from_rgb(205, 165, 90),
            Some(NodeDoc::ChannelStrip { .. }) => Color32::from_gray(140),
            None => Color32::from_gray(120),
        },
    };
    let fill = match id {
        PatchId::Master => Color32::from_rgb(52, 32, 30),
        PatchId::Timeline => Color32::from_rgb(38, 30, 50),
        PatchId::TimelineIn => Color32::from_rgb(33, 26, 44),
        PatchId::Node(_) => Color32::from_rgb(34, 38, 46),
    };
    (stroke, fill)
}

/// Cable color (by type).
fn cable_color(kind: CableKind) -> Color32 {
    match kind {
        CableKind::Audio => Color32::from_rgb(150, 152, 160),
        CableKind::Midi => Color32::from_rgb(90, 200, 110),
        CableKind::Automation => Color32::from_rgb(230, 190, 70),
    }
}

/// Inline row count of one entity (G5): static registry length for
/// built-in nodes, the session plugin enumeration length for plugin
/// nodes, 0 for fixed entities.
fn inline_row_count(id: PatchId, deps: &PatchDeps) -> usize {
    let PatchId::Node(node_id) = id else {
        return 0;
    };
    let Some(node) = deps.doc.node_by_id(node_id) else {
        return 0;
    };
    match node {
        NodeDoc::Instrument {
            voice: VoiceSpec::Clap { .. } | VoiceSpec::Vst3 { .. },
            ..
        }
        | NodeDoc::Effect {
            voice: VoiceSpec::Clap { .. } | VoiceSpec::Vst3 { .. },
            ..
        } => (deps.plugin_inline)(node_id).len(),
        _ => inline_params_of(node).len(),
    }
}

/// Entity box size. Mixer boxes scale with the channel (input port) count so
/// every channel gets its own input port dot on the left edge; every node
/// grows downward by its inline parameter rows (G5).
fn size_of(id: PatchId, deps: &PatchDeps) -> Vec2 {
    let base = match id {
        PatchId::Timeline => timeline_size(deps.track_count, deps.auto_count, deps.audio_count),
        PatchId::TimelineIn => TIMELINE_IN_SIZE,
        PatchId::Node(node_id) => match deps.doc.node_by_id(node_id) {
            // Title + slot action row, then one port per channel.
            Some(NodeDoc::Mixer { inputs, .. }) => Vec2::new(
                NODE_SIZE.x,
                header_band(id, deps) + (*inputs as f32 * PORT_SPACING).max(NODE_HEIGHT_MIN),
            ),
            Some(NodeDoc::Keyboard { .. }) => KEYBOARD_SIZE,
            // U13: a node that owns an action row (the SoundFont file
            // button) reserves it on top of the title band.
            _ => Vec2::new(
                NODE_SIZE.x,
                NODE_SIZE.y + action_rows(id, deps) as f32 * ACTION_ROW_HEIGHT,
            ),
        },
        _ => NODE_SIZE,
    };
    Vec2::new(
        base.x,
        base.y + inline_row_count(id, deps) as f32 * INLINE_ROW_HEIGHT,
    )
}

/// Short file stem for node labels (U5): `kick_01.wav` → `kick_01` (empty
/// for unparseable paths).
fn file_stem_of(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .map(|s| truncate(&s, 10))
        .unwrap_or_default()
}

/// MIDI note number → scientific pitch name (`60` → `C4`).
fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let name = NAMES[(note % 12) as usize];
    let octave = (note / 12) as i32 - 1;
    format!("{name}{octave}")
}

/// Entity label.
fn label_of(id: PatchId, doc: &GraphDoc) -> String {
    match id {
        PatchId::Master => "Master Out".to_string(),
        PatchId::Timeline => "Timeline".to_string(),
        PatchId::TimelineIn => "Timeline In".to_string(),
        PatchId::Node(node_id) => match doc.node_by_id(node_id) {
            Some(NodeDoc::Instrument { voice, track, .. }) => match voice {
                VoiceSpec::Builtin => format!("Synth · Track {}", track + 1),
                // U13: the file is the node's identity now (the preset is
                // on the combos below); the old `bank:program` label went
                // stale the moment a file could be swapped.
                VoiceSpec::SoundFont { path, .. } => {
                    if path.is_empty() {
                        "SF2 · (no file)".to_string()
                    } else {
                        format!("SF2 · {}", file_stem_of(path))
                    }
                }
                VoiceSpec::Sample {
                    path, root_note, ..
                } => format!(
                    "SAMPLE {}·{} · Track {}",
                    file_stem_of(path),
                    note_name(*root_note),
                    track + 1
                ),
                VoiceSpec::Clap { plugin_id, .. } => {
                    format!("♪ {}", truncate(plugin_id, 16))
                }
                VoiceSpec::Vst3 { class_id, .. } => format!("♪ {}", truncate(class_id, 16)),
            },
            Some(NodeDoc::Effect { voice, track, .. }) => match voice {
                VoiceSpec::Builtin | VoiceSpec::SoundFont { .. } | VoiceSpec::Sample { .. } => {
                    format!("Delay · Track {}", track + 1)
                }
                VoiceSpec::Clap { plugin_id, .. } => {
                    format!("Fx {}", truncate(plugin_id, 14))
                }
                VoiceSpec::Vst3 { class_id, .. } => format!("Fx {}", truncate(class_id, 14)),
            },
            Some(NodeDoc::Mixer { inputs, .. }) => format!("Mixer · {} ch", inputs),
            // G6 (A design): the node *is* its input source — the track is
            // only a placement hint now, so it stays out of the label.
            Some(NodeDoc::Keyboard { source, .. }) => match source {
                MidiSource::Computer => "Keyboard · Computer".to_string(),
                MidiSource::Device(name) => format!("Keyboard · {name}"),
            },
            Some(NodeDoc::Feedback { gain, .. }) => format!("Feedback {:.0}%", gain * 100.0),
            Some(NodeDoc::AudioTrack { track, .. }) => {
                format!("Audio · Track {}", track + 1)
            }
            Some(NodeDoc::ChannelStrip { track, .. }) => {
                format!("Strip (legacy) · Track {}", track + 1)
            }
            None => format!("Node {node_id}"),
        },
    }
}

/// Short label for an automation cable endpoint parameter.
///
/// Node parameters get a context-free label (`param 2` for an effect slot);
/// [`auto_param_label_at`] upgrades it with the target node's own names.
pub(crate) fn auto_param_label(param: &AutoParam) -> String {
    match param {
        AutoParam::TrackGain => "Volume".to_string(),
        AutoParam::TrackPan => "Pan".to_string(),
        AutoParam::FilterCutoff => "Cutoff".to_string(),
        AutoParam::PluginParam { name, .. } => name.clone(),
        AutoParam::Tempo => "Tempo".to_string(),
        // U4a: cable labels sit in the patch, where the channel matters less
        // than knowing the destination is a controller.
        AutoParam::Cc { controller, .. } => format!("CC {controller}"),
        // P1: a bend is a controller too; the range lives in its editor.
        AutoParam::PitchBend { .. } => "Pitch Bend".to_string(),
        AutoParam::Node(param) => match param {
            NodeParam::EffectSlot(slot) => format!("Param {slot}"),
            NodeParam::MixerChannel { channel, param } => match param {
                ChannelParam::Gain => format!("Ch {} Gain", channel + 1),
                ChannelParam::Pan => format!("Ch {} Pan", channel + 1),
                ChannelParam::Mute => format!("Ch {} Mute", channel + 1),
                ChannelParam::Solo => format!("Ch {} Solo", channel + 1),
            },
            NodeParam::MixerOutPan => "Out Pan".to_string(),
            NodeParam::MixerOutMute => "Out Mute".to_string(),
            NodeParam::FeedbackGain => "Feedback".to_string(),
            NodeParam::Sf2Gain | NodeParam::SampleGain => "Gain".to_string(),
            NodeParam::Resonance => "Resonance".to_string(),
            NodeParam::Attack => "Attack".to_string(),
            NodeParam::Decay => "Decay".to_string(),
            NodeParam::Sustain => "Sustain".to_string(),
            NodeParam::Release => "Release".to_string(),
        },
    }
}

/// [`auto_param_label`] with the destination node's context (U4c): a built-in
/// effect slot is labelled by its descriptor entry (`time`, `mix`, …) instead
/// of a slot number, because "Param 2" on a cable tells the user nothing.
pub(crate) fn auto_param_label_at(param: &AutoParam, doc: &GraphDoc, target: &PortRef) -> String {
    if let (AutoParam::Node(NodeParam::EffectSlot(slot)), PortRef::Node(node_id)) = (param, target)
        && let Some(NodeDoc::Effect { params, .. }) = doc.node_by_id(*node_id)
        && let Some(spec) = crate::effect::descriptor(params).spec(*slot)
    {
        return spec.short.to_string();
    }
    auto_param_label(param)
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

impl PatchState {
    /// Document node ids currently selected (fixed entities — timeline /
    /// master — excluded). Read by the app to resolve the Synth panel's
    /// editing target (spec `plugin-gui.md` §6).
    pub fn selected_doc_nodes(&self) -> Vec<u64> {
        self.selection
            .nodes
            .iter()
            .filter_map(|id| match id {
                PatchId::Node(doc) => Some(*doc),
                PatchId::Timeline | PatchId::Master | PatchId::TimelineIn => None,
            })
            .collect()
    }

    /// Graph space → screen space.
    fn to_screen(&self, rect_min: Pos2, p: Pos2) -> Pos2 {
        Pos2::new(
            rect_min.x + (p.x - self.pan.x) * self.zoom,
            rect_min.y + (p.y - self.pan.y) * self.zoom,
        )
    }

    /// Entity screen rect (returns `None` when no position exists).
    ///
    /// Single source of truth for entity geometry: box size is scaled by
    /// the zoom factor here so drawing, ports and ALL hit tests stay in sync.
    fn screen_rect(&self, rect_min: Pos2, id: PatchId, deps: &PatchDeps) -> Option<Rect> {
        let p = *self.positions.get(&id)?;
        Some(Rect::from_min_size(
            self.to_screen(rect_min, p),
            size_of(id, deps) * self.zoom,
        ))
    }

    /// Screen-space vertical extent of one box's port area: below the
    /// title / action band, above the inline slider rows.
    fn port_area(&self, r: Rect, id: PatchId, deps: &PatchDeps) -> (f32, f32) {
        let top = r.top() + header_band(id, deps) * self.zoom;
        let bottom = r.bottom() - inline_row_count(id, deps) as f32 * INLINE_ROW_HEIGHT * self.zoom;
        (top, bottom.max(top + 1.0))
    }

    /// Output ports of one entity: `(port index, screen position)` pairs. The
    /// timeline exposes one port per track spread along its right edge; audio
    /// nodes have a single port; the master output has none.
    fn out_ports_screen(
        &self,
        rect_min: Pos2,
        id: PatchId,
        deps: &PatchDeps,
    ) -> Vec<(usize, Pos2)> {
        let Some(r) = self.screen_rect(rect_min, id, deps) else {
            return Vec::new();
        };
        match id {
            PatchId::Master => Vec::new(),
            // The timeline input is a pure sink: no outputs (U3b).
            PatchId::TimelineIn => Vec::new(),
            PatchId::Timeline => {
                let n = timeline_port_count(deps.track_count, deps.auto_count, deps.audio_count);
                let (top, bottom) = self.port_area(r, id, deps);
                let step = (bottom - top) / n.max(1) as f32;
                (0..n)
                    .map(|i| (i, Pos2::new(r.right(), top + (i as f32 + 0.5) * step)))
                    .collect()
            }
            PatchId::Node(_) => vec![(0, Pos2::new(r.right(), r.center().y))],
        }
    }

    /// Input ports of one entity: `(channel index, screen position)` pairs
    /// (P5f D17: a mixer exposes one port per channel spread along its left
    /// edge; other nodes/master have the single port 0; the timeline has none
    /// — it is a pure source, D14/T4).
    fn in_ports_screen(&self, rect_min: Pos2, id: PatchId, deps: &PatchDeps) -> Vec<(u8, Pos2)> {
        let Some(r) = self.screen_rect(rect_min, id, deps) else {
            return Vec::new();
        };
        let n = match id {
            PatchId::Timeline => return Vec::new(),
            // v1 port table: `Tempo` only (spec §4.2 lists future slots).
            PatchId::TimelineIn => 1,
            PatchId::Master => 1,
            PatchId::Node(node_id) => match deps.doc.node_by_id(node_id) {
                Some(node) if node.is_mixer() => {
                    if let NodeDoc::Mixer { inputs, .. } = node {
                        *inputs as usize
                    } else {
                        1
                    }
                }
                // G6: the virtual keyboard has no audio input.
                Some(node) if node.is_keyboard() => 0,
                _ => 1,
            },
        };
        // A mixer spreads its channels over the port area (below the slot
        // buttons, above the inline row); single-port boxes keep the centre.
        if n > 1 {
            let (top, bottom) = self.port_area(r, id, deps);
            let step = (bottom - top) / n as f32;
            return (0..n)
                .map(|i| (i as u8, Pos2::new(r.left(), top + (i as f32 + 0.5) * step)))
                .collect();
        }
        (0..n)
            .map(|i| (i as u8, Pos2::new(r.left(), r.center().y)))
            .collect()
    }

    /// Fill in default placements for missing entities (never overwrites existing positions;
    /// prunes stale entries).
    ///
    /// Audio-track nodes (U6) are **not placed**: the patch renders their
    /// output ports on the timeline box instead of a separate node box, so
    /// they never enter `positions` (and thus are never drawn / hit-tested).
    fn ensure_positions(
        &mut self,
        doc: &GraphDoc,
        track_count: usize,
        auto_count: usize,
        audio_count: usize,
    ) {
        let mut wanted: Vec<PatchId> = Vec::new();

        // One column per track: instrument → effect stack → mixer.
        let mut effect_slot = vec![0usize; track_count];
        // Input nodes share the timeline's left column, stacked underneath its
        // box (the gap between the timeline and the instrument column is too
        // narrow for a 150 px box).
        let mut keyboard_slot = 0usize;
        let keyboard_top = 32.0 + timeline_size(track_count, auto_count, audio_count).y + 16.0;
        for node in &doc.nodes {
            // U6: the audio-track node is folded into the timeline box (its
            // port is drawn there); skip placing it as a standalone box.
            if matches!(node, NodeDoc::AudioTrack { .. }) {
                continue;
            }
            let track = node.track().min(track_count.saturating_sub(1));
            let y = 32.0 + track as f32 * 132.0;
            let (id, pos) = match node {
                NodeDoc::Instrument { .. } => (PatchId::Node(node.node_id()), Pos2::new(260.0, y)),
                NodeDoc::Effect { voice, .. } => {
                    let k = if matches!(voice, VoiceSpec::Builtin) {
                        0
                    } else {
                        1 + effect_slot[track]
                    };
                    effect_slot[track] += 1;
                    (
                        PatchId::Node(node.node_id()),
                        Pos2::new(480.0, y + k as f32 * 64.0),
                    )
                }
                NodeDoc::Mixer { .. } => (PatchId::Node(node.node_id()), Pos2::new(700.0, y)),
                // P6: feedback taps sit just left of the mixer column.
                NodeDoc::Feedback { .. } => (PatchId::Node(node.node_id()), Pos2::new(600.0, y)),
                // U6: audio-track nodes are folded into the timeline box (see
                // the guard at the top of this loop) — unreachable here.
                NodeDoc::AudioTrack { .. } => (PatchId::Node(node.node_id()), Pos2::new(260.0, y)),
                // G6: input nodes stack in the left column below the
                // timeline box (only a fallback — the menu places them at the
                // click position).
                NodeDoc::Keyboard { .. } => {
                    let k = keyboard_slot;
                    keyboard_slot += 1;
                    (
                        PatchId::Node(node.node_id()),
                        Pos2::new(16.0, keyboard_top + k as f32 * 68.0),
                    )
                }
                NodeDoc::ChannelStrip { .. } => {
                    (PatchId::Node(node.node_id()), Pos2::new(700.0, y))
                }
            };
            wanted.push(id);
            self.positions.entry(id).or_insert(pos);
        }

        // The single explicit Timeline node (T4), top-left of the graph.
        wanted.push(PatchId::Timeline);
        self.positions
            .entry(PatchId::Timeline)
            .or_insert(Pos2::new(16.0, 32.0));

        wanted.push(PatchId::Master);
        let y_center = (32.0 + track_count.max(1) as f32 * 132.0) * 0.5;
        self.positions
            .entry(PatchId::Master)
            .or_insert(Pos2::new(930.0, y_center));

        // The fixed Timeline In sink (U3b) sits below the master output —
        // same right-hand column, i.e. where signals terminate.
        wanted.push(PatchId::TimelineIn);
        self.positions
            .entry(PatchId::TimelineIn)
            .or_insert(Pos2::new(930.0, y_center + 96.0));

        // Prune positions of deleted entities to prevent unbounded hashmap growth.
        self.positions.retain(|id, _| wanted.contains(id));
    }

    /// Set the graph-space position of a freshly added node (called by the glue after
    /// committing an [`PatchAction::AddNode`]; `ensure_positions` never overwrites it).
    pub fn place_node(&mut self, node_id: u64, at: Pos2) {
        self.positions.insert(PatchId::Node(node_id), at);
    }

    /// Snapshot the current positions into a document layout (called by the glue right
    /// before saving). Only entities that still exist are exported; stale node entries
    /// (already deleted from the document) are dropped.
    pub fn export_layout(&self, doc: &GraphDoc) -> LayoutDoc {
        let mut layout = LayoutDoc::default();
        for (id, p) in &self.positions {
            match *id {
                PatchId::Node(node_id) => {
                    if doc.node_by_id(node_id).is_some() {
                        layout.nodes.insert(node_id, [p.x, p.y]);
                    }
                }
                PatchId::Timeline => layout.timeline = Some([p.x, p.y]),
                PatchId::Master => layout.master = Some([p.x, p.y]),
                PatchId::TimelineIn => layout.timeline_in = Some([p.x, p.y]),
            }
        }
        layout
    }

    /// Seed positions from a document layout (called after load / new project;
    /// clears the session view state — missing entries fall back to auto-placement
    /// on the next `ensure_positions`).
    pub fn import_layout(&mut self, layout: &LayoutDoc) {
        self.positions.clear();
        for (node_id, xy) in &layout.nodes {
            self.positions
                .insert(PatchId::Node(*node_id), Pos2::new(xy[0], xy[1]));
        }
        if let Some([x, y]) = layout.timeline {
            self.positions.insert(PatchId::Timeline, Pos2::new(x, y));
        }
        if let Some([x, y]) = layout.master {
            self.positions.insert(PatchId::Master, Pos2::new(x, y));
        }
        if let Some([x, y]) = layout.timeline_in {
            self.positions.insert(PatchId::TimelineIn, Pos2::new(x, y));
        }
    }

    /// Hit-test an output port (screen distance) → `(entity, port index)`.
    fn hit_out_port(&self, rect_min: Pos2, sp: Pos2, deps: &PatchDeps) -> Option<(PatchId, usize)> {
        for &id in self.positions.keys() {
            for (port, p) in self.out_ports_screen(rect_min, id, deps) {
                if p.distance(sp) <= PORT_HIT_RADIUS {
                    return Some((id, port));
                }
            }
        }
        None
    }

    /// Hit-test an input port (screen distance) → `(entity, channel index)`.
    /// Used for mixer-channel-targeted cable drops (D17).
    fn hit_in_port(&self, rect_min: Pos2, sp: Pos2, deps: &PatchDeps) -> Option<(PatchId, u8)> {
        for &id in self.positions.keys() {
            for (port, p) in self.in_ports_screen(rect_min, id, deps) {
                if p.distance(sp) <= PORT_HIT_RADIUS {
                    return Some((id, port));
                }
            }
        }
        None
    }

    /// Hit-test an entity box.
    fn hit_node(&self, rect_min: Pos2, sp: Pos2, deps: &PatchDeps) -> Option<PatchId> {
        for &id in self.positions.keys() {
            if let Some(r) = self.screen_rect(rect_min, id, deps) {
                if r.contains(sp) {
                    return Some(id);
                }
            }
        }
        None
    }

    /// All entities whose screen rect intersects the marquee (box selection).
    fn nodes_in_rect(&self, rect_min: Pos2, marquee: Rect, deps: &PatchDeps) -> Vec<PatchId> {
        self.positions
            .keys()
            .copied()
            .filter(|&id| {
                self.screen_rect(rect_min, id, deps)
                    .is_some_and(|r| r.intersects(marquee))
            })
            .collect()
    }

    /// Endpoint screen positions of a document cable → bezier control points
    /// (`None` when either endpoint has no position / the ports don't resolve).
    /// The endpoint lands on the input port matching `cable.in_port` (D17:
    /// the mixer channel the cable is bound to).
    fn cable_controls(
        &self,
        rect_min: Pos2,
        cable: &CableDoc,
        deps: &PatchDeps,
    ) -> Option<[Pos2; 4]> {
        let (from, from_port) = patch_port_from(&cable.from, deps)?;
        let to = patch_port_to(&cable.to)?;
        let p0 = self
            .out_ports_screen(rect_min, from, deps)
            .into_iter()
            .find(|(k, _)| *k == from_port)?
            .1;
        let p3 = self
            .in_ports_screen(rect_min, to, deps)
            .into_iter()
            .find(|(k, _)| *k == cable.in_port)?
            .1;
        Some(bezier_controls(p0, p3))
    }

    /// Hit-test a cable (returns the index into the document cable table).
    fn hit_cable(&self, rect_min: Pos2, sp: Pos2, deps: &PatchDeps) -> Option<usize> {
        for (i, cable) in deps.doc.cables.iter().enumerate() {
            let Some(c) = self.cable_controls(rect_min, cable, deps) else {
                continue;
            };
            for pt in bezier_points(c, 24) {
                if pt.distance(sp) <= CABLE_HIT_DIST {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Hit-test a cable midpoint control dot (returns the cable index).
    fn hit_cable_midpoint(&self, rect_min: Pos2, sp: Pos2, deps: &PatchDeps) -> Option<usize> {
        for (i, cable) in deps.doc.cables.iter().enumerate() {
            let Some(c) = self.cable_controls(rect_min, cable, deps) else {
                continue;
            };
            if bezier_points(c, 24)[12].distance(sp) <= MIDPOINT_HIT_RADIUS {
                return Some(i);
            }
        }
        None
    }

    /// All cables whose sampled curve passes within [`CUT_HIT_DIST`] of the
    /// screen-space pointer path (Blender-style knife hit test).
    fn cables_crossing_path(&self, rect_min: Pos2, path: &[Pos2], deps: &PatchDeps) -> Vec<usize> {
        let mut hits = Vec::new();
        if path.len() < 2 {
            return hits;
        }
        for (i, cable) in deps.doc.cables.iter().enumerate() {
            let Some(c) = self.cable_controls(rect_min, cable, deps) else {
                continue;
            };
            'cable: for pt in bezier_points(c, 32) {
                for seg in path.windows(2) {
                    if dist_point_seg(pt, seg[0], seg[1]) <= CUT_HIT_DIST {
                        hits.push(i);
                        break 'cable;
                    }
                }
            }
        }
        hits
    }

    /// Point-select at screen position `sp`, shared by the press-drag path and
    /// the plain-click path (egui 0.36 disambiguates clicks from drags, so an
    /// in-place press never produces `drag_started` — only `clicked`).
    ///
    /// Returns the point-selected node (if any) so the caller can do the
    /// patch-panel track linkage.
    fn point_select(
        &mut self,
        rect_min: Pos2,
        sp: Pos2,
        deps: &PatchDeps,
        shift: bool,
    ) -> Option<PatchId> {
        if let Some(id) = self.hit_node(rect_min, sp, deps) {
            if shift {
                self.selection.toggle_node(id);
            } else if !self.selection.contains_node(id) {
                self.selection = Selection {
                    nodes: vec![id],
                    cables: Vec::new(),
                };
            }
            Some(id)
        } else if let Some(i) = self.hit_cable_midpoint(rect_min, sp, deps) {
            // Click on a cable's midpoint control dot: open the insert/delete menu.
            let graph_at = Pos2::new(
                self.pan.x + (sp.x - rect_min.x) / self.zoom,
                self.pan.y + (sp.y - rect_min.y) / self.zoom,
            );
            self.pending_cable_menu = Some((i, sp + Vec2::new(4.0, 4.0), graph_at, u64::MAX));
            None
        } else if let Some(i) = self.hit_cable(rect_min, sp, deps) {
            if shift {
                self.selection.toggle_cable(i);
            } else {
                self.selection = Selection {
                    nodes: Vec::new(),
                    cables: vec![i],
                };
            }
            None
        } else {
            // Click on empty background clears the selection.
            if !shift {
                self.selection = Selection::default();
            }
            None
        }
    }
}

/// Patch-panel linkage: point-selecting a document node switches the main
/// editing track (the timeline / master have no track binding).
fn track_of_node(picked: Option<PatchId>, doc: &GraphDoc) -> Option<usize> {
    match picked {
        Some(PatchId::Node(node_id)) => doc.node_by_id(node_id).map(|node| node.track()),
        _ => None,
    }
}

/// Engine/document node id of the audio-track generator for `track` (U6), or
/// `None` when the track has no node. The patch folds this node into the
/// timeline box's audio ports, so its id only crosses the boundary inside
/// cable endpoints (never as a drawable box).
fn audio_node_id(doc: &GraphDoc, track: usize) -> Option<u64> {
    doc.nodes.iter().find_map(|n| match n {
        NodeDoc::AudioTrack { node_id, track: t } if *t == track => Some(*node_id),
        _ => None,
    })
}

/// Inverse of [`audio_node_id`]: an audio-track node id → its track index.
fn audio_track_of_node(doc: &GraphDoc, node_id: u64) -> Option<usize> {
    doc.nodes.iter().find_map(|n| match n {
        NodeDoc::AudioTrack { node_id: id, track } if *id == node_id => Some(*track),
        _ => None,
    })
}

/// `PortRef` → cable source entity + output port index. Timeline ports are
/// laid out MIDI → Curve → Audio (`0..midi` = MIDI, `midi..midi+auto` =
/// control, `midi+auto..` = audio). An audio-track document node resolves to
/// the timeline box's matching audio port (U6), so its cable is drawn from
/// the timeline like every other track kind.
fn patch_port_from(port: &PortRef, deps: &PatchDeps) -> Option<(PatchId, usize)> {
    match port {
        PortRef::Node(id) => {
            if let Some(track) = audio_track_of_node(deps.doc, *id) {
                Some((
                    PatchId::Timeline,
                    deps.track_count + deps.auto_count + track,
                ))
            } else {
                Some((PatchId::Node(*id), 0))
            }
        }
        PortRef::MidiTrack(t) => Some((PatchId::Timeline, *t)),
        PortRef::AutoTrack(i) => Some((PatchId::Timeline, deps.track_count + i)),
        PortRef::Master => None, // The master output is not a valid audio source.
        PortRef::TimelineIn => None, // Pure sink: no output ports.
    }
}

/// `PortRef` → cable target entity.
fn patch_port_to(port: &PortRef) -> Option<PatchId> {
    match port {
        PortRef::Node(id) => Some(PatchId::Node(*id)),
        PortRef::Master => Some(PatchId::Master),
        PortRef::TimelineIn => Some(PatchId::TimelineIn),
        _ => None,
    }
}

/// Cable kind → whether the target entity accepts it (port type rules, spec §7/D3).
fn accepts(kind: CableKind, target: PatchId, doc: &GraphDoc) -> bool {
    if let PatchId::Node(id) = target {
        let Some(node) = doc.node_by_id(id) else {
            return false;
        };
        match kind {
            CableKind::Audio => {
                matches!(
                    node,
                    NodeDoc::Effect { .. } | NodeDoc::Mixer { .. } | NodeDoc::Feedback { .. }
                )
            }
            CableKind::Midi => matches!(node, NodeDoc::Instrument { .. }),
            // U4c (spec `control-tracks.md` §9 D19): every node kind that owns
            // an automatable parameter accepts an automation cable. The
            // keyboard (MIDI source) and the audio-track node (no parameters)
            // stay out — a cable there could never do anything.
            CableKind::Automation => matches!(
                node,
                NodeDoc::Instrument { .. }
                    | NodeDoc::Effect { .. }
                    | NodeDoc::Mixer { .. }
                    | NodeDoc::Feedback { .. }
            ),
        }
    } else {
        match target {
            PatchId::Master => kind == CableKind::Audio,
            // U3b D7: the conductor terminal only takes automation (its sole
            // port is `Tempo`); audio / MIDI cables are rejected like any
            // other port-type mismatch.
            PatchId::TimelineIn => kind == CableKind::Automation,
            PatchId::Timeline => false,
            // Handled by the `if let` above (unreachable here).
            PatchId::Node(_) => false,
        }
    }
}

/// Virtual-keyboard glyph (G6): a mini white/black key strip inside the node
/// box, drawn beneath the label.
fn paint_key_strip(painter: &egui::Painter, node: Rect) {
    let strip = Rect::from_min_max(
        Pos2::new(node.left() + 10.0, node.top() + 30.0),
        Pos2::new(node.right() - 10.0, node.bottom() - 8.0),
    );
    if strip.width() <= 8.0 || strip.height() <= 4.0 {
        return;
    }
    let white_keys = 7;
    let key_w = strip.width() / white_keys as f32;
    for i in 0..white_keys {
        let key = Rect::from_min_size(
            Pos2::new(strip.left() + i as f32 * key_w, strip.top()),
            Vec2::new((key_w - 1.0).max(1.0), strip.height()),
        );
        painter.rect_filled(key, 1.0, Color32::from_rgb(232, 234, 236));
    }
    // Black keys on the C-D-E / F-G-A boundaries of the one-octave strip.
    for i in [0, 1, 3, 4, 5] {
        let x = strip.left() + (i as f32 + 1.0) * key_w;
        let key = Rect::from_min_size(
            Pos2::new(x - key_w * 0.28, strip.top()),
            Vec2::new(key_w * 0.56, strip.height() * 0.6),
        );
        if key.right() <= strip.right() {
            painter.rect_filled(key, 1.0, Color32::from_rgb(30, 32, 36));
        }
    }
}

/// Cubic bezier control points: horizontal extension scales with distance.
fn bezier_controls(p0: Pos2, p3: Pos2) -> [Pos2; 4] {
    let dx = ((p3.x - p0.x).abs() * 0.5).clamp(40.0, 140.0);
    [
        p0,
        Pos2::new(p0.x + dx, p0.y),
        Pos2::new(p3.x - dx, p3.y),
        p3,
    ]
}

/// Linear interpolation between two points.
fn lerp2(a: Pos2, b: Pos2, t: f32) -> Pos2 {
    Pos2::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
}

/// Shortest distance from point `p` to the segment `a → b`.
fn dist_point_seg(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.dot(ab);
    if len2 <= f32::EPSILON {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.distance(a + ab * t)
}

/// Uniform bezier sampling (for hit testing).
fn bezier_points(c: [Pos2; 4], samples: usize) -> Vec<Pos2> {
    (0..=samples)
        .map(|i| {
            let t = i as f32 / samples as f32;
            let (a, b, d, e) = (c[0], c[1], c[2], c[3]);
            let ab = lerp2(a, b, t);
            let bd = lerp2(b, d, t);
            let de = lerp2(d, e, t);
            let abbd = lerp2(ab, bd, t);
            let bdde = lerp2(bd, de, t);
            lerp2(abbd, bdde, t)
        })
        .collect()
}

/// Draw one frame of the patch and collect interaction intents.
pub fn show(ui: &mut egui::Ui, state: &mut PatchState, deps: PatchDeps<'_>) -> Vec<PatchAction> {
    let mut actions = Vec::new();
    let doc = deps.doc;
    state.ensure_positions(doc, deps.track_count, deps.auto_count, deps.audio_count);

    // ---- Toolbar ----
    ui.horizontal(|ui| {
        ui.strong("Node Graph");
        if ui.button("-").clicked() {
            state.zoom = (state.zoom / 1.2).clamp(ZOOM_MIN, ZOOM_MAX);
        }
        if ui.button("+").clicked() {
            state.zoom = (state.zoom * 1.2).clamp(ZOOM_MIN, ZOOM_MAX);
        }
        if ui.button("Reset").clicked() {
            state.pan = Vec2::ZERO;
            state.zoom = 1.0;
        }
        ui.weak("Right-click to add node · Ctrl+right-drag to cut cables · click a cable dot to insert/delete · click to select · drag bg to box-select · middle-drag to pan · scroll to zoom · drag ports to connect · Delete to remove");
    });

    // ---- Patch ----
    // Fill the remaining central-panel height (dedicated Patch view); fall back
    // to the fixed minimum when the available space is smaller.
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(
            ui.available_width(),
            ui.available_height().max(CANVAS_HEIGHT),
        ),
        Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_rgb(22, 24, 28));

    // Wheel zoom (plain wheel and Ctrl+wheel / pinch, anchored at the pointer).
    // NOTE: `zoom_delta()` is a *multiplicative* factor whose neutral value is
    // 1.0 (NOT 0.0) — comparing against 0.0 would zoom ×2 every frame and pin
    // the view at ZOOM_MAX.
    let (zoom_factor, scroll_y) = ui
        .ctx()
        .input(|i| (i.zoom_delta(), i.smooth_scroll_delta.y));
    if response.hovered() {
        let mult = if zoom_factor != 1.0 {
            // Ctrl+wheel / pinch: egui already gives the multiplicative factor.
            zoom_factor
        } else if scroll_y != 0.0 {
            // Plain wheel: convert the scroll delta into a zoom multiplier.
            1.0 + scroll_y * 0.005
        } else {
            1.0
        };
        if mult != 1.0 {
            let old = state.zoom;
            state.zoom = (state.zoom * mult).clamp(ZOOM_MIN, ZOOM_MAX);
            if let Some(sp) = response.hover_pos() {
                let graph = state.pan + (sp - rect.min) / old;
                state.pan = graph - (sp - rect.min) / state.zoom;
            }
        }
    }

    let pointer = response.interact_pointer_pos();
    let drag_started = response.drag_started_by(egui::PointerButton::Primary);
    let drag_stopped = response.drag_stopped_by(egui::PointerButton::Primary);
    let shift = ui.ctx().input(|i| i.modifiers.shift);
    let ctrl = ui.ctx().input(|i| i.modifiers.ctrl);

    // Shorthands for the port geometry (the timeline box grows with tracks).
    let midi_count = deps.track_count;
    let auto_count = deps.auto_count;

    // ---- Press: hit-test and enter the matching drag ----
    // egui 0.36 (click+drag sense) only starts a drag after the pointer moves
    // decisively; in-place presses are handled by the `clicked` path below.
    if drag_started {
        let sp = pointer.unwrap_or(rect.min);
        if let Some((source, port)) = state.hit_out_port(rect.min, sp, &deps) {
            state.selection = Selection::default();
            state.drag = Some(DragKind::Cable(CableDrag { from: source, port }));
        } else if let Some(id) = state.hit_node(rect.min, sp, &deps) {
            let picked = state.point_select(rect.min, sp, &deps, shift);
            if !shift && let Some(track) = track_of_node(picked, doc) {
                actions.push(PatchAction::SelectTrack(track));
            }
            state.drag = Some(DragKind::Node(id));
        } else {
            // Background drag: marquee box selection (Shift appends instead of
            // replacing). A plain background click never reaches here — it is
            // a `clicked` and clears the selection there.
            if !shift {
                state.selection = Selection::default();
            }
            state.drag = Some(DragKind::BoxSelect {
                start: sp,
                add: shift,
            });
        }
    }

    // ---- Plain click (press + release without decisive drag): point-select ----
    if response.clicked()
        && let Some(sp) = pointer
    {
        let picked = state.point_select(rect.min, sp, &deps, shift);
        if !shift && let Some(track) = track_of_node(picked, doc) {
            actions.push(PatchAction::SelectTrack(track));
        }
    }

    // ---- Double-click: open the view that edits the entity ----
    // The timeline box is the arrangement's own entity → its tab. A document
    // node opens the Inspector, but only when the panel would actually show
    // something: instrument / effect parameters, or the mixer console. The
    // virtual keyboard owns no parameters and the master group already lives
    // on the transport bar, so those stay inert.
    if response.double_clicked()
        && let Some(sp) = pointer
        && let Some(target) = state.hit_node(rect.min, sp, &deps)
    {
        match target {
            PatchId::Timeline => actions.push(PatchAction::OpenTimeline),
            PatchId::Node(doc_id) => {
                if opens_inspector(doc.node_by_id(doc_id)) {
                    actions.push(PatchAction::OpenNode(doc_id));
                }
            }
            PatchId::Master | PatchId::TimelineIn => {}
        }
    }

    // ---- Right-click: add-node context menu (any position; the new node appears where
    // the user clicked). With Ctrl held the right button belongs to the cable-cut
    // interaction instead (Blender-style knife) and the menu is suppressed. ----
    if response.secondary_clicked()
        && !ctrl
        && let Some(sp) = pointer
    {
        state.add_menu_at = Some(Pos2::new(
            state.pan.x + (sp.x - rect.min.x) / state.zoom,
            state.pan.y + (sp.y - rect.min.y) / state.zoom,
        ));
    }
    // egui manages the open/close lifecycle (Escape / click elsewhere close it); the
    // closure runs only while the menu is open. egui 0.36's `context_menu` closure
    // returns `()`, so the pick is collected through captured outer variables.
    //
    // Structure (spec `plugin-browser.md` §6.1): category first, then source. The
    // plugin leaves open the filterable picker instead of listing hundreds of
    // entries in a submenu (D4/D5); the built-in leaves create directly.
    let mut picked_source: Option<NodeSource> = None;
    let mut picked_picker: Option<(Option<NodeCategory>, Option<PluginFormat>)> = None;
    let mut picked_sample = false;
    let mut rescan_plugins = false;
    if !(response.secondary_clicked() && ctrl) {
        response.context_menu(|ui| {
            ui.set_min_width(160.0);
            ui.strong("Add Node");
            ui.separator();
            // D5: cross-category jump — the entry's own classification decides
            // whether an instrument or an effect node is created.
            if ui.button("Search Plug-ins…").clicked() {
                picked_picker = Some((None, None));
            }
            ui.menu_button("Instrument", |ui| {
                if ui.button("Built-in Synth").clicked() {
                    picked_source = Some(NodeSource::BuiltinSynth);
                }
                // U13: born empty on purpose — the node carries its own
                // `Load File…` button, so adding one never blocks on a
                // dialog and a file can be swapped later without rebuilding
                // the node (and its cables).
                if ui.button("SoundFont").clicked() {
                    picked_source = Some(NodeSource::SoundFont {
                        path: String::new(),
                    });
                }
                if ui.button("Sample…").clicked() {
                    picked_sample = true;
                }
                ui.separator();
                if ui.button("CLAP…").clicked() {
                    picked_picker =
                        Some((Some(NodeCategory::Instrument), Some(PluginFormat::Clap)));
                }
                if ui.button("VST3…").clicked() {
                    picked_picker =
                        Some((Some(NodeCategory::Instrument), Some(PluginFormat::Vst3)));
                }
            });
            ui.menu_button("Effect", |ui| {
                for kind in BuiltinEffectKind::ALL {
                    if ui.button(kind.label()).clicked() {
                        picked_source = Some(NodeSource::Builtin(*kind));
                    }
                }
                ui.separator();
                if ui.button("CLAP…").clicked() {
                    picked_picker = Some((Some(NodeCategory::Effect), Some(PluginFormat::Clap)));
                }
                if ui.button("VST3…").clicked() {
                    picked_picker = Some((Some(NodeCategory::Effect), Some(PluginFormat::Vst3)));
                }
            });
            ui.separator();
            // No source dimension: one level is enough (spec §6.1).
            if ui.button("Mixer").clicked() {
                picked_source = Some(NodeSource::Mixer);
            }
            if ui.button("Keyboard In (computer)").clicked() {
                picked_source = Some(NodeSource::Keyboard);
            }
            if ui.button("Feedback Tap").clicked() {
                picked_source = Some(NodeSource::Feedback);
            }
            ui.separator();
            // U9 E2: full plugin re-scan on demand (the startup scan is
            // cached, so this is the escape hatch after installing or
            // removing plugins while the app is closed).
            if ui.button("Rescan Plug-ins").clicked() {
                rescan_plugins = true;
            }
        });
    }
    if rescan_plugins {
        actions.push(PatchAction::RescanPlugins);
    }
    // The menu position is consumed by whichever action the user picked.
    if picked_source.is_some() || picked_picker.is_some() || picked_sample {
        if let Some(at) = state.add_menu_at.take() {
            if let Some(source) = picked_source {
                actions.push(PatchAction::AddNode { source, at });
            } else if let Some((category, format)) = picked_picker {
                actions.push(PatchAction::OpenPluginPicker {
                    category,
                    format,
                    place: PickerPlace::Add { at },
                });
            } else {
                actions.push(PatchAction::PickSample { at });
            }
        }
    }

    // ---- Ctrl + right-drag: cut cables the pointer path crosses ----
    if response.drag_started_by(egui::PointerButton::Secondary) && ctrl {
        state.cut_path = Some(pointer.map(|p| vec![p]).unwrap_or_default());
    }
    if state.cut_path.is_some()
        && response.dragged_by(egui::PointerButton::Secondary)
        && let Some(sp) = pointer
    {
        let path = state.cut_path.as_mut().expect("checked above");
        if path.last() != Some(&sp) {
            path.push(sp);
        }
        if ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
            state.cut_path = None;
        }
    }
    if response.drag_stopped_by(egui::PointerButton::Secondary) {
        if let Some(path) = state.cut_path.take()
            && path.len() >= 2
        {
            let mut cuts = state.cables_crossing_path(rect.min, &path, &deps);
            cuts.sort_unstable();
            cuts.dedup();
            // Descending index order: each removal shifts later document indices.
            for i in cuts.into_iter().rev() {
                actions.push(PatchAction::DeleteCable(i));
            }
        }
        // Cable indices may have shifted; drop any stale cable selection.
        state.selection.cables.clear();
    }

    // ---- Drag in progress ----
    // Middle button: pan the viewport.
    if response.dragged_by(egui::PointerButton::Middle) {
        let d = ui.ctx().input(|i| i.pointer.delta());
        state.pan -= d / state.zoom;
    }
    if response.dragged_by(egui::PointerButton::Primary)
        && let Some(DragKind::Node(id)) = state.drag
    {
        let d = response.drag_delta() / state.zoom;
        if state.selection.contains_node(id) && state.selection.nodes.len() > 1 {
            // Moving a selected node moves the whole selection.
            for nid in state.selection.nodes.clone() {
                if let Some(p) = state.positions.get_mut(&nid) {
                    *p += d;
                }
            }
        } else if let Some(p) = state.positions.get_mut(&id) {
            *p += d;
        }
    }

    // ---- Drag released: commit cable / marquee ----
    if drag_stopped {
        match state.drag.take() {
            Some(DragKind::Cable(drag)) => {
                if let Some(sp) = pointer
                    && let Some(target) = state.hit_node(rect.min, sp, &deps)
                {
                    match drag.from {
                        // Timeline port: MIDI tracks, then control tracks, then
                        // audio tracks (mapping lives in `timeline_port_kind`).
                        PatchId::Timeline => {
                            match timeline_port_kind(drag.port, midi_count, auto_count) {
                                TimelinePort::Midi(track) => {
                                    if accepts(CableKind::Midi, target, doc)
                                        && let PatchId::Node(to_doc) = target
                                    {
                                        actions.push(PatchAction::ConnectMidi { track, to_doc });
                                    }
                                }
                                TimelinePort::Auto(index)
                                    if accepts(CableKind::Automation, target, doc) =>
                                {
                                    match target {
                                        // The conductor terminal has a single
                                        // port, so the cable commits straight
                                        // away — no parameter popup (U3b D7).
                                        PatchId::TimelineIn => {
                                            actions.push(PatchAction::ConnectTempoIn { index });
                                        }
                                        PatchId::Node(to_doc) => {
                                            // The cable is committed after the
                                            // parameter is picked in the popup menu.
                                            state.pending_auto =
                                                Some((index, to_doc, sp + Vec2::new(4.0, 4.0)));
                                        }
                                        _ => {}
                                    }
                                }
                                TimelinePort::Auto(_) => {}
                                // U6: the audio-track node is folded into the
                                // timeline box, so its audio cable is dragged
                                // from this port (target / channel math mirrors
                                // the ordinary node-out-port branch below).
                                TimelinePort::Audio(track) => {
                                    if accepts(CableKind::Audio, target, doc)
                                        && let Some(from_doc) = audio_node_id(doc, track)
                                    {
                                        let to = match target {
                                            PatchId::Node(id) => PortRef::Node(id),
                                            PatchId::Master => PortRef::Master,
                                            _ => PortRef::Master,
                                        };
                                        let in_port = match target {
                                            PatchId::Node(to_doc)
                                                if doc
                                                    .node_by_id(to_doc)
                                                    .is_some_and(|n| n.is_mixer()) =>
                                            {
                                                match state.hit_in_port(rect.min, sp, &deps) {
                                                    Some((PatchId::Node(_), port)) => Some(port),
                                                    _ => doc.first_free_channel(to_doc),
                                                }
                                            }
                                            _ => Some(0),
                                        };
                                        if let Some(in_port) = in_port {
                                            actions.push(PatchAction::ConnectAudio {
                                                from_doc,
                                                to,
                                                in_port,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                        PatchId::Node(from_doc) => {
                            // G6: a virtual keyboard's out port is a MIDI
                            // port; every other node out port is audio.
                            if doc.node_by_id(from_doc).is_some_and(NodeDoc::is_keyboard) {
                                if accepts(CableKind::Midi, target, doc)
                                    && drag.from != target
                                    && let PatchId::Node(to_doc) = target
                                {
                                    actions.push(PatchAction::ConnectKeyboardMidi {
                                        from_doc,
                                        to_doc,
                                    });
                                }
                            } else if accepts(CableKind::Audio, target, doc) && drag.from != target
                            {
                                let to = match target {
                                    PatchId::Node(id) => PortRef::Node(id),
                                    PatchId::Master => PortRef::Master,
                                    _ => PortRef::Master,
                                };
                                // D17: the cable binds to the target's input
                                // port. Dropping on a mixer picks the exact
                                // channel port when hit precisely, else the
                                // first free channel; dropping anywhere else
                                // is port 0. Slots are strictly single-input
                                // (D14): the new cable replaces the old one
                                // on the same slot; no free channel → reject.
                                let in_port = match target {
                                    PatchId::Node(to_doc)
                                        if doc.node_by_id(to_doc).is_some_and(|n| n.is_mixer()) =>
                                    {
                                        match state.hit_in_port(rect.min, sp, &deps) {
                                            Some((PatchId::Node(_), port)) => Some(port),
                                            _ => doc.first_free_channel(to_doc),
                                        }
                                    }
                                    _ => Some(0),
                                };
                                if let Some(in_port) = in_port {
                                    actions.push(PatchAction::ConnectAudio {
                                        from_doc,
                                        to,
                                        in_port,
                                    });
                                }
                            }
                        }
                        PatchId::Master => {}
                        // A pure sink has no output port, so no drag can start
                        // here (U3b).
                        PatchId::TimelineIn => {}
                    }
                }
            }
            Some(DragKind::BoxSelect { start, add }) => {
                if let Some(sp) = pointer {
                    let marquee = Rect::from_two_pos(start, sp);
                    let hits = state.nodes_in_rect(rect.min, marquee, &deps);
                    if add {
                        for id in hits {
                            if !state.selection.contains_node(id) {
                                state.selection.nodes.push(id);
                            }
                        }
                    } else {
                        state.selection = Selection {
                            nodes: hits,
                            cables: Vec::new(),
                        };
                    }
                }
            }
            _ => {}
        }
    }

    // ---- Draw cables (beneath nodes) ----
    for (i, cable) in doc.cables.iter().enumerate() {
        let Some(bez) = state.cable_controls(rect.min, cable, &deps) else {
            continue;
        };
        let selected = state.selection.cables.contains(&i);
        let stroke = egui::Stroke::new(
            if selected { 3.5 } else { 2.0 },
            if selected {
                Color32::WHITE
            } else {
                cable_color(cable.kind)
            },
        );
        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
            bez,
            false,
            Color32::TRANSPARENT,
            stroke,
        ));
        let mid = bezier_points(bez, 24)[12];
        // Label automation cables with the endpoint parameter above the midpoint.
        if cable.kind == CableKind::Automation {
            painter.text(
                mid + Vec2::new(0.0, -12.0),
                egui::Align2::CENTER_CENTER,
                auto_param_label_at(&cable.param, doc, &cable.to),
                FontId::proportional(11.0),
                cable_color(CableKind::Automation),
            );
        }
        // Midpoint control dot (click → insert node / delete cable menu).
        let hovered = pointer.is_some_and(|sp| sp.distance(mid) <= MIDPOINT_HIT_RADIUS);
        let r = if hovered {
            MIDPOINT_RADIUS + 2.0
        } else {
            MIDPOINT_RADIUS
        };
        painter.circle_filled(mid, r, cable_color(cable.kind));
        painter.circle_stroke(mid, r, egui::Stroke::new(1.0, Color32::BLACK));
    }

    // ---- Cable-cut visuals: pointer path + cables about to be severed ----
    if let Some(path) = &state.cut_path
        && path.len() >= 2
    {
        for i in state.cables_crossing_path(rect.min, path, &deps) {
            if let Some(bez) = state.cable_controls(rect.min, &doc.cables[i], &deps) {
                painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                    bez,
                    false,
                    Color32::TRANSPARENT,
                    egui::Stroke::new(4.0, Color32::from_rgb(230, 70, 70)),
                ));
            }
        }
        painter.add(egui::Shape::line(
            path.clone(),
            egui::Stroke::new(1.5, Color32::from_rgb(230, 70, 70)),
        ));
    }

    // ---- Cable drag preview ----
    if let Some(DragKind::Cable(drag)) = state.drag {
        let kind = match drag.from {
            PatchId::Timeline => match timeline_port_kind(drag.port, midi_count, auto_count) {
                TimelinePort::Midi(_) => CableKind::Midi,
                TimelinePort::Auto(_) => CableKind::Automation,
                TimelinePort::Audio(_) => CableKind::Audio,
            },
            PatchId::Node(node_id) => {
                // G6: the vk node's out port carries MIDI, not audio.
                if doc.node_by_id(node_id).is_some_and(NodeDoc::is_keyboard) {
                    CableKind::Midi
                } else {
                    CableKind::Audio
                }
            }
            PatchId::Master => CableKind::Audio,
            // Unreachable (no output ports); automation keeps the type
            // consistent with the terminal's port table.
            PatchId::TimelineIn => CableKind::Automation,
        };
        if let Some(sp) = pointer {
            let p0 = state
                .out_ports_screen(rect.min, drag.from, &deps)
                .into_iter()
                .find(|(k, _)| *k == drag.port)
                .map(|(_, p)| p);
            if let Some(p0) = p0 {
                painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                    bezier_controls(p0, sp),
                    false,
                    Color32::TRANSPARENT,
                    egui::Stroke::new(2.0, cable_color(kind).gamma_multiply(0.7)),
                ));
                // Highlight the hovered target.
                if let Some(target) = state.hit_node(rect.min, sp, &deps) {
                    if accepts(kind, target, doc) && drag.from != target {
                        if let Some(r) = state.screen_rect(rect.min, target, &deps) {
                            painter.rect_stroke(
                                r,
                                4.0,
                                egui::Stroke::new(2.0, Color32::WHITE),
                                egui::StrokeKind::Outside,
                            );
                        }
                    }
                }
            }
        }
    }

    // ---- Draw nodes ----
    for &id in state.positions.keys() {
        let Some(r) = state.screen_rect(rect.min, id, &deps) else {
            continue;
        };
        if !r.intersects(rect) {
            continue;
        }
        let (stroke, fill) = node_color(id, doc);
        let selected = state.selection.contains_node(id);
        painter.rect_filled(r, 4.0, fill);
        painter.rect_stroke(
            r,
            4.0,
            egui::Stroke::new(if selected { 2.5 } else { 1.2 }, stroke),
            egui::StrokeKind::Middle,
        );

        let text = label_of(id, doc);
        let text_pos = match id {
            PatchId::Timeline => Pos2::new(r.center().x, r.top() + 12.0),
            PatchId::TimelineIn => Pos2::new(r.center().x, r.top() + 12.0),
            _ => Pos2::new(r.center().x, r.top() + 18.0),
        };
        painter.text(
            text_pos,
            egui::Align2::CENTER_CENTER,
            text,
            FontId::proportional(13.0),
            Color32::from_rgb(225, 225, 228),
        );

        // G6: mini key strip — the virtual keyboard's visual signature.
        if let PatchId::Node(node_id) = id
            && doc.node_by_id(node_id).is_some_and(NodeDoc::is_keyboard)
        {
            paint_key_strip(&painter, r);
        }

        // Output ports (the timeline draws one per track with a small label).
        for (port, p) in state.out_ports_screen(rect.min, id, &deps) {
            let port_color = match id {
                PatchId::Timeline => match timeline_port_kind(port, midi_count, auto_count) {
                    TimelinePort::Midi(_) => cable_color(CableKind::Midi),
                    TimelinePort::Auto(_) => cable_color(CableKind::Automation),
                    TimelinePort::Audio(_) => cable_color(CableKind::Audio),
                },
                PatchId::Node(node_id)
                    if doc.node_by_id(node_id).is_some_and(NodeDoc::is_keyboard) =>
                {
                    cable_color(CableKind::Midi)
                }
                _ => cable_color(CableKind::Audio),
            };
            painter.circle_filled(p, PORT_RADIUS, port_color);
            painter.circle_stroke(p, PORT_RADIUS, egui::Stroke::new(1.0, Color32::BLACK));
            if let PatchId::Timeline = id {
                let label = match timeline_port_kind(port, midi_count, auto_count) {
                    TimelinePort::Midi(t) => format!("T{}", t + 1),
                    TimelinePort::Auto(i) => format!("C{}", i + 1),
                    TimelinePort::Audio(i) => format!("A{}", i + 1),
                };
                painter.text(
                    Pos2::new(p.x - 10.0, p.y),
                    egui::Align2::RIGHT_CENTER,
                    label,
                    FontId::proportional(10.0),
                    Color32::from_gray(170),
                );
            }
        }
        // Input ports: mixers draw one per channel (P5f D17), other nodes /
        // master the single port 0. The Timeline In terminal names its port
        // (U3b: v1 has the `Tempo` port only).
        for (port, p) in state.in_ports_screen(rect.min, id, &deps) {
            let inbound = if matches!(id, PatchId::TimelineIn) {
                cable_color(CableKind::Automation)
            } else {
                Color32::from_gray(90)
            };
            painter.circle_filled(p, PORT_RADIUS, inbound);
            painter.circle_stroke(p, PORT_RADIUS, egui::Stroke::new(1.0, Color32::BLACK));
            if matches!(id, PatchId::TimelineIn) && port == 0 {
                painter.text(
                    Pos2::new(p.x + 10.0, p.y),
                    egui::Align2::LEFT_CENTER,
                    "Tempo",
                    FontId::proportional(10.0),
                    Color32::from_gray(170),
                );
            }
        }
    }

    // ---- Inline parameter sliders (G5 spec §7) ----
    // Real widgets placed over the bottom row strip of each node box (the
    // height formula in `size_of` reserves it). Being later in the layer,
    // they sit above the painted nodes and win the pointer from the patch
    // backdrop — dragging a slider adjusts the parameter instead of moving
    // the node. Values are read-only snapshots; edits leave as actions.
    for &id in state.positions.keys() {
        let PatchId::Node(doc_id) = id else {
            continue;
        };
        let Some(node) = doc.node_by_id(doc_id) else {
            continue;
        };
        // One row per registered parameter; SoundFont bank / patch rows are
        // combos, so they also carry their choice list (U13).
        let mut rows: Vec<InlineRow> = match node {
            NodeDoc::Instrument {
                voice: VoiceSpec::Clap { .. } | VoiceSpec::Vst3 { .. },
                ..
            }
            | NodeDoc::Effect {
                voice: VoiceSpec::Clap { .. } | VoiceSpec::Vst3 { .. },
                ..
            } => (deps.plugin_inline)(doc_id)
                .into_iter()
                .map(|c| InlineRow {
                    label: c.label,
                    param: InlineParamId::Plugin(c.param_id),
                    value: c.value,
                    min: c.min,
                    max: c.max,
                    choices: Vec::new(),
                })
                .collect(),
            _ => inline_params_of(node)
                .iter()
                .map(|spec| {
                    let (value, min, max) = inline_doc_value(node, spec.param);
                    InlineRow {
                        label: spec.label.to_string(),
                        param: InlineParamId::BuiltIn(spec.param),
                        value,
                        min,
                        max,
                        choices: Vec::new(),
                    }
                })
                .collect(),
        };
        // SoundFont combo rows: the preset table is a session snapshot (the
        // patch cannot read a parsed font), so it arrives through `deps`.
        // Without it (no file yet / library not resident) the row stays
        // visible but inert — the same "say why" rule as a disabled button.
        if let NodeDoc::Instrument {
            voice: VoiceSpec::SoundFont { bank, .. },
            ..
        } = node
        {
            let presets = (deps.sf2_presets)(doc_id);
            for row in &mut rows {
                if let InlineParamId::BuiltIn(param) = row.param
                    && param.row_kind() == RowKind::Combo
                {
                    row.choices = sf2_choices(&presets, param, *bank);
                }
            }
        }
        if rows.is_empty() {
            continue;
        }
        let Some(r) = state.screen_rect(rect.min, id, &deps) else {
            continue;
        };
        if !r.intersects(rect) {
            continue;
        }
        if state.zoom < INLINE_MIN_ZOOM {
            // The drawn slider shrinks with the row; the egui-native one does
            // not (fixed style font / rail → it overflowed the box at half
            // zoom). Below the threshold the rows drop out entirely, like the
            // action buttons.
            continue;
        }
        let row_h = INLINE_ROW_HEIGHT * state.zoom;
        let n = rows.len();
        for (i, row) in rows.into_iter().enumerate() {
            let top = r.bottom() - (n - i) as f32 * row_h;
            let strip =
                Rect::from_min_max(Pos2::new(r.left(), top), Pos2::new(r.right(), top + row_h));
            let (label_pos, rail, value_pos, hit) = inline_row_parts(strip, state.zoom);
            let InlineRow {
                label,
                param,
                mut value,
                min,
                max,
                choices,
            } = row;
            painter.text(
                label_pos,
                egui::Align2::LEFT_CENTER,
                &label,
                FontId::proportional(9.0 * state.zoom),
                Color32::from_gray(190),
            );
            // ---- Combo row (U13): a chip that opens a list ----
            if matches!(param, InlineParamId::BuiltIn(p) if p.row_kind() == RowKind::Combo) {
                let caption = choices
                    .iter()
                    .find(|(choice, _)| f64::from(*choice) == value)
                    .map(|(_, caption)| caption.clone())
                    .unwrap_or_else(|| format!("{}", value.round() as i64));
                // The chip spans the slider's footprint: label stays on the
                // left, the whole rest is the click target.
                let chip = Rect::from_min_max(
                    Pos2::new(rail.left(), top + 1.5 * state.zoom),
                    Pos2::new(hit.right(), top + row_h - 1.5 * state.zoom),
                );
                let enabled = !choices.is_empty();
                painter.rect_filled(
                    chip,
                    2.0 * state.zoom,
                    if enabled {
                        Color32::from_gray(52)
                    } else {
                        Color32::from_gray(38)
                    },
                );
                painter.rect_stroke(
                    chip,
                    2.0 * state.zoom,
                    egui::Stroke::new(1.0, Color32::from_gray(if enabled { 90 } else { 60 })),
                    egui::StrokeKind::Inside,
                );
                painter.text(
                    Pos2::new(chip.left() + 4.0 * state.zoom, chip.center().y),
                    egui::Align2::LEFT_CENTER,
                    &caption,
                    FontId::proportional(9.0 * state.zoom),
                    Color32::from_gray(if enabled { 215 } else { 120 }),
                );
                // Chevron drawn as two strokes, not a "▾" glyph: the
                // default font has no such character and painted a tofu box
                // in its place (verified in the running UI).
                let arrow = Stroke::new(
                    1.2 * state.zoom,
                    Color32::from_gray(if enabled { 190 } else { 110 }),
                );
                let tip_x = chip.right() - 5.0 * state.zoom;
                let tip_y = chip.center().y;
                let wing = 2.6 * state.zoom;
                painter.line_segment(
                    [
                        Pos2::new(tip_x - wing, tip_y - wing * 0.7),
                        Pos2::new(tip_x, tip_y + wing * 0.7),
                    ],
                    arrow,
                );
                painter.line_segment(
                    [
                        Pos2::new(tip_x, tip_y + wing * 0.7),
                        Pos2::new(tip_x + wing, tip_y - wing * 0.7),
                    ],
                    arrow,
                );
                // Real widget over the chip: a press opens the list instead
                // of dragging the node (same priority as the sliders).
                let resp = ui
                    .allocate_rect(chip, Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                let resp = if enabled {
                    resp
                } else {
                    resp.on_hover_text("Load a sound font first")
                };
                if enabled && resp.clicked() {
                    if let Some(sp) = resp.interact_pointer_pos() {
                        state.pending_sf2_choice = Some((
                            doc_id,
                            match param {
                                InlineParamId::BuiltIn(p) => p,
                                InlineParamId::Plugin(_) => continue,
                            },
                            sp + Vec2::new(4.0, 4.0),
                            u64::MAX,
                        ));
                    }
                }
                if enabled {
                    resp.on_hover_text(format!("{label}: {caption}"));
                }
                continue;
            }
            // ---- Slider row (G5) ----
            // Rail with a filled run up to the handle, then the handle dot —
            // every measure multiplied by the zoom so the row stays inside
            // its rect at any scale.
            let t = inline_ratio(value, min, max) as f32;
            draw_fader_rail(&painter, rail, t, state.zoom);
            painter.text(
                value_pos,
                egui::Align2::RIGHT_CENTER,
                fmt_inline_value(value, min, max),
                FontId::proportional(9.0 * state.zoom),
                Color32::from_gray(200),
            );
            // Real widget over the painted row (same pointer priority as the
            // action buttons): press here drags the handle, never the node.
            let mut resp = ui.allocate_rect(hit, Sense::click_and_drag());
            if resp.dragged() || resp.clicked() {
                if let Some(sp) = resp.interact_pointer_pos() {
                    let width = rail.width();
                    if width > 0.0 {
                        let k = ((sp.x - rail.left()) / width).clamp(0.0, 1.0) as f64;
                        value = min + k * (max - min);
                        actions.push(PatchAction::SetInlineParam {
                            node: doc_id,
                            param,
                            value: value as f32,
                        });
                    }
                }
            }
            resp = resp.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
            let _ =
                resp.on_hover_text(format!("{} = {}", label, fmt_inline_value(value, min, max)));
        }
    }

    // ---- Mixer channel strips (U14) ----
    // One strip per input port: fader + mute + solo across the row the
    // channel's cable lands on, so a fader is always next to the line it
    // governs. Values are read from the same document fields the Inspector
    // console edits — the two surfaces cannot disagree, and an edit here
    // raises the same undo step through the same param path.
    for &id in state.positions.keys() {
        let PatchId::Node(doc_id) = id else {
            continue;
        };
        let Some(NodeDoc::Mixer { channels, .. }) = doc.node_by_id(doc_id) else {
            continue;
        };
        if state.zoom < INLINE_MIN_ZOOM {
            continue;
        }
        let Some(r) = state.screen_rect(rect.min, id, &deps) else {
            continue;
        };
        if !r.intersects(rect) {
            continue;
        }
        let z = state.zoom;
        for (port, port_pos) in state.in_ports_screen(rect.min, id, &deps) {
            let Some(ch) = channels.get(port as usize) else {
                continue;
            };
            let strip = channel_strip_rect(port_pos, r, z);
            let cells = channel_strip_cells(strip, z);
            let gain_min = *crate::TRACK_GAIN_RANGE.start();
            let gain_max = *crate::TRACK_GAIN_RANGE.end();
            draw_fader_rail(
                &painter,
                cells.rail,
                inline_ratio(f64::from(ch.gain), f64::from(gain_min), f64::from(gain_max)) as f32,
                z,
            );
            // The fader grabs the whole strip height: a 3 px rail would be
            // impossible to hit, and this row has no inline label to drag.
            let hit = Rect::from_min_max(
                Pos2::new(cells.rail.left(), strip.top()),
                Pos2::new(cells.rail.right(), strip.bottom()),
            );
            let resp = ui.allocate_rect(hit, Sense::click_and_drag());
            if (resp.dragged() || resp.clicked())
                && let Some(sp) = resp.interact_pointer_pos()
                && cells.rail.width() > 0.0
            {
                let k = ((sp.x - cells.rail.left()) / cells.rail.width()).clamp(0.0, 1.0);
                actions.push(PatchAction::SetInlineParam {
                    node: doc_id,
                    param: InlineParamId::BuiltIn(InlineParam::MixerGain(port)),
                    value: gain_min + k * (gain_max - gain_min),
                });
            }
            let _ = resp
                .on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
                .on_hover_text(format!("Ch {} volume {:.0}%", port + 1, ch.gain * 100.0));
            let mute = channel_toggle(
                ui,
                cells.mute,
                z,
                "M",
                ch.muted,
                Color32::from_rgb(190, 85, 70),
                "Mute this channel",
            );
            if mute {
                actions.push(PatchAction::SetInlineParam {
                    node: doc_id,
                    param: InlineParamId::BuiltIn(InlineParam::MixerMute(port)),
                    value: if ch.muted { 0.0 } else { 1.0 },
                });
            }
            let solo = channel_toggle(
                ui,
                cells.solo,
                z,
                "S",
                ch.solo,
                Color32::from_rgb(205, 175, 65),
                "Solo this channel (the others fall silent)",
            );
            if solo {
                actions.push(PatchAction::SetInlineParam {
                    node: doc_id,
                    param: InlineParamId::BuiltIn(InlineParam::MixerSolo(port)),
                    value: if ch.solo { 0.0 } else { 1.0 },
                });
            }
        }
    }

    // ---- SoundFont bank / patch list (U13) ----
    // Opened by a combo row. An `Area` (not a `ComboBox`) because the row is
    // a hand-painted strip scaled by the zoom: a native combo would keep its
    // own font size and overflow the box at low zoom, and the list has to
    // stay readable while the patch itself is zoomed out.
    // Stamp the opening frame first — the click that opened this menu is an
    // `any_click` in the same frame.
    if let Some((_, _, _, frame)) = &mut state.pending_sf2_choice
        && *frame == u64::MAX
    {
        *frame = ui.ctx().cumulative_frame_nr();
    }
    // The node can disappear while the list is open (delete / undo).
    if state
        .pending_sf2_choice
        .is_some_and(|(node, ..)| doc.node_by_id(node).is_none())
    {
        state.pending_sf2_choice = None;
    }
    if let Some((doc_id, param, menu_pos, opened)) = state.pending_sf2_choice {
        let bank = match doc.node_by_id(doc_id) {
            Some(NodeDoc::Instrument {
                voice: VoiceSpec::SoundFont { bank, .. },
                ..
            }) => *bank,
            _ => 0,
        };
        let presets = (deps.sf2_presets)(doc_id);
        let choices = sf2_choices(&presets, param, bank);
        let current = doc
            .node_by_id(doc_id)
            .map_or(0.0, |node| inline_doc_value(node, param).0);
        let title = match param {
            InlineParam::Sf2Bank => "SoundFont Bank",
            _ => "SoundFont Patch",
        };
        let mut picked: Option<u8> = None;
        egui::Area::new(egui::Id::new("glitcherz_patch_sf2_choice"))
            .fixed_pos(menu_pos)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(220.0);
                    ui.strong(title);
                    egui::ScrollArea::vertical()
                        .max_height(260.0)
                        .show(ui, |ui| {
                            for (value, caption) in &choices {
                                if ui
                                    .selectable_label(
                                        f64::from(*value) == current,
                                        caption.as_str(),
                                    )
                                    .clicked()
                                {
                                    picked = Some(*value);
                                }
                            }
                        });
                });
            });
        match picked {
            Some(value) => {
                actions.push(PatchAction::SetInlineParam {
                    node: doc_id,
                    param: InlineParamId::BuiltIn(param),
                    value: f32::from(value),
                });
                state.pending_sf2_choice = None;
            }
            None => {
                // Ignore the click that opened the menu (same frame).
                if opened != ui.ctx().cumulative_frame_nr()
                    && ui
                        .ctx()
                        .input(|i| i.pointer.any_click() || i.key_pressed(egui::Key::Escape))
                {
                    state.pending_sf2_choice = None;
                }
            }
        }
    }

    // ---- In-node action rows ----
    // Track management belongs to the timeline and the slot count to the
    // mixer, so those buttons sit on the boxes that own them instead of a
    // side panel. Drawn and hit-tested here (not with `egui::Button`), so the
    // whole row scales with the zoom like the inline rows do.
    if state.zoom >= ACTION_MIN_ZOOM {
        for &id in state.positions.keys() {
            let rows = action_rows(id, &deps);
            if rows == 0 {
                continue;
            }
            let Some(r) = state.screen_rect(rect.min, id, &deps) else {
                continue;
            };
            if !r.intersects(rect) {
                continue;
            }
            let z = state.zoom;
            for row in 0..rows {
                let buttons = node_action_buttons(id, row, &deps);
                let band = action_row_band(r, row, z);
                // Two buttons for the timeline / mixer, one for a SoundFont
                // node's file button (U13) — the row is split evenly.
                let rects = action_button_rects(band, buttons.len(), z);
                for (b, btn_rect) in buttons.into_iter().zip(rects) {
                    if action_button(ui, btn_rect, z, &b) {
                        actions.push(b.action);
                    }
                }
            }
        }
    }

    // ---- Marquee (box selection in progress, drawn on top) ----
    if let Some(DragKind::BoxSelect { start, .. }) = state.drag
        && let Some(sp) = pointer
    {
        let m = Rect::from_two_pos(start, sp);
        painter.rect_filled(m, 2.0, Color32::from_rgba_unmultiplied(120, 170, 255, 26));
        painter.rect_stroke(
            m,
            2.0,
            egui::Stroke::new(1.0, Color32::from_rgb(120, 170, 255)),
            egui::StrokeKind::Middle,
        );
    }

    // ---- Delete removes the whole selection (while hovering the patch) ----
    if response.hovered() && ui.ctx().input(|i| i.key_pressed(egui::Key::Delete)) {
        // Cables first, in descending index order (each removal shifts later
        // indices); nodes after (node deletion may cascade attached cables in
        // the caller).
        let mut cables = state.selection.cables.clone();
        cables.sort_unstable_by(|a, b| b.cmp(a));
        cables.dedup();
        for i in cables {
            actions.push(PatchAction::DeleteCable(i));
        }
        for id in state.selection.nodes.clone() {
            if let PatchId::Node(node_id) = id {
                actions.push(PatchAction::DeleteNode(node_id));
            }
        }
        if !state.selection.nodes.is_empty() || !state.selection.cables.is_empty() {
            state.selection = Selection::default();
        }
    }

    // ---- Automation parameter popup menu ----
    if let Some((index, to_doc, menu_pos)) = state.pending_auto {
        let options = (deps.auto_options)(to_doc);
        let mut picked: Option<AutoParam> = None;
        egui::Area::new(egui::Id::new("glitcherz_patch_auto_menu"))
            .fixed_pos(menu_pos)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(140.0);
                    ui.strong("Automation Target Param");
                    for (label, param) in &options {
                        if ui.button(label).clicked() {
                            picked = Some(param.clone());
                        }
                    }
                });
            });
        if let Some(param) = picked {
            actions.push(PatchAction::ConnectAuto {
                index,
                to_doc,
                param,
            });
            state.pending_auto = None;
        } else if ui
            .ctx()
            .input(|i| i.pointer.any_click() || i.key_pressed(egui::Key::Escape))
        {
            state.pending_auto = None;
        }
    }

    // ---- Cable midpoint menu (click the control dot): insert node / delete ----
    // Stamp the opening frame first so the click that opened the menu (any_click in
    // the same frame) does not close it immediately.
    if let Some((_, _, _, frame)) = &mut state.pending_cable_menu
        && *frame == u64::MAX
    {
        *frame = ui.ctx().cumulative_frame_nr();
    }
    if let Some((cable_i, menu_pos, at, opened)) = state.pending_cable_menu {
        #[derive(Clone)]
        enum CablePick {
            Insert(NodeSource),
            OpenPicker(PluginFormat),
            Delete,
        }
        let mut picked: Option<CablePick> = None;
        egui::Area::new(egui::Id::new("glitcherz_patch_cable_menu"))
            .fixed_pos(menu_pos)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(140.0);
                    ui.strong("Cable");
                    // Same type-first shape as the add menu (spec §6.1), so the
                    // two places a node can be born read alike. MIDI / automation
                    // cables accept no processors at all.
                    let is_audio = doc
                        .cables
                        .get(cable_i)
                        .is_some_and(|c| c.kind == CableKind::Audio);
                    if is_audio {
                        ui.menu_button("Insert", |ui| {
                            ui.menu_button("Effect", |ui| {
                                for kind in BuiltinEffectKind::ALL {
                                    if ui.button(kind.label()).clicked() {
                                        picked =
                                            Some(CablePick::Insert(NodeSource::Builtin(*kind)));
                                    }
                                }
                                ui.separator();
                                if ui.button("CLAP…").clicked() {
                                    picked = Some(CablePick::OpenPicker(PluginFormat::Clap));
                                }
                                if ui.button("VST3…").clicked() {
                                    picked = Some(CablePick::OpenPicker(PluginFormat::Vst3));
                                }
                            });
                            // A mixer is not an "effect"; it gets its own leaf.
                            if ui.button("Mixer").clicked() {
                                picked = Some(CablePick::Insert(NodeSource::Mixer));
                            }
                        });
                    }
                    if ui.button("Delete Cable").clicked() {
                        picked = Some(CablePick::Delete);
                    }
                });
            });
        match picked {
            Some(CablePick::Insert(source)) => {
                actions.push(PatchAction::InsertNode {
                    cable: cable_i,
                    source,
                    at,
                });
                state.pending_cable_menu = None;
            }
            Some(CablePick::OpenPicker(format)) => {
                actions.push(PatchAction::OpenPluginPicker {
                    // Only effects can be spliced into an audio cable: the
                    // instrument leaves would produce a silent node.
                    category: Some(NodeCategory::Effect),
                    format: Some(format),
                    place: PickerPlace::Insert { cable: cable_i, at },
                });
                state.pending_cable_menu = None;
            }
            Some(CablePick::Delete) => {
                actions.push(PatchAction::DeleteCable(cable_i));
                state.pending_cable_menu = None;
            }
            None => {
                let close = ui
                    .ctx()
                    .input(|i| i.pointer.any_click() || i.key_pressed(egui::Key::Escape));
                if close && ui.ctx().cumulative_frame_nr() != opened {
                    state.pending_cable_menu = None;
                }
            }
        }
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use glitcherz_model::Project;

    /// Regression: `#[derive(Default)]` gave `zoom: 0.0`, collapsing the whole
    /// graph onto the pan point (top-left corner) until Reset was clicked.
    #[test]
    fn default_zoom_is_one_not_zero() {
        let state = PatchState::default();
        assert_eq!(state.zoom, 1.0);
    }

    #[test]
    fn ensure_positions_covers_all_entities() {
        let project = Project::default();
        let doc = &project.graph;
        let track_count = project.midi_tracks.len();
        let auto_count = project.control_tracks.len();
        let mut state = PatchState::default();
        state.ensure_positions(doc, track_count, auto_count, project.audio_tracks.len());

        for node in &doc.nodes {
            if matches!(node, NodeDoc::AudioTrack { .. }) {
                continue; // folded into the timeline box (no standalone position)
            }
            assert!(
                state.positions.contains_key(&PatchId::Node(node.node_id())),
                "document node {} should have a position",
                node.node_id()
            );
        }
        assert!(state.positions.contains_key(&PatchId::Master));
        // T4: one explicit Timeline entity instead of per-track source boxes.
        assert!(state.positions.contains_key(&PatchId::Timeline));
        // U3b: the Timeline In sink is a fixed entity too — always present.
        assert!(
            state.positions.contains_key(&PatchId::TimelineIn),
            "the conductor terminal must always exist in the patch"
        );
        assert_eq!(
            state
                .positions
                .keys()
                .filter(|id| **id == PatchId::Timeline)
                .count(),
            1
        );
    }

    /// U3b D1/D7: the conductor terminal is a pure sink with a single
    /// automation input port — no outputs, and it rejects audio / MIDI cables
    /// exactly like any other port-type mismatch.
    #[test]
    fn timeline_in_ports_and_cable_rules() {
        let project = Project::default();
        let doc = &project.graph;
        let deps = PatchDeps {
            doc,
            track_count: project.midi_tracks.len(),
            auto_count: project.control_tracks.len(),
            audio_count: project.audio_tracks.len(),
            auto_options: &|_| Vec::new(),
            plugin_inline: &|_| Vec::new(),
            sf2_presets: &|_| Vec::new(),
        };
        let mut state = PatchState::default();
        state.ensure_positions(doc, deps.track_count, deps.auto_count, deps.audio_count);

        let min = Pos2::ZERO;
        assert!(
            state
                .out_ports_screen(min, PatchId::TimelineIn, &deps)
                .is_empty(),
            "a pure sink has no output ports"
        );
        assert_eq!(
            state.in_ports_screen(min, PatchId::TimelineIn, &deps).len(),
            1,
            "v1 exposes exactly the Tempo port"
        );

        assert!(accepts(CableKind::Automation, PatchId::TimelineIn, doc));
        assert!(!accepts(CableKind::Audio, PatchId::TimelineIn, doc));
        assert!(!accepts(CableKind::Midi, PatchId::TimelineIn, doc));

        // Cable endpoint resolution: the sink is a valid target, never a
        // source; the timeline itself accepts nothing.
        assert_eq!(
            patch_port_to(&PortRef::TimelineIn),
            Some(PatchId::TimelineIn)
        );
        assert_eq!(patch_port_from(&PortRef::TimelineIn, &deps), None);
        assert!(!accepts(CableKind::Automation, PatchId::Timeline, doc));

        // The layout round-trips the sink's position like master / timeline.
        assert!(state.export_layout(doc).timeline_in.is_some());
    }

    /// A freshly added node's placed position must survive `ensure_positions`
    /// (the auto-placement only fills *missing* entries). Mirrors the real glue
    /// order: document node pushed first, then `place_node`.
    #[test]
    fn place_node_position_survives_ensure_positions() {
        let mut project = Project::default();
        let new_id = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Mixer {
            node_id: new_id,
            track: 0,
            inputs: 2,
            channels: vec![Default::default(); 2],
            out_pan: 0.0,
            out_muted: false,
        });
        let doc = &project.graph;
        let mut state = PatchState::default();
        state.ensure_positions(
            doc,
            project.midi_tracks.len(),
            project.control_tracks.len(),
            project.audio_tracks.len(),
        );

        let at = Pos2::new(555.0, 77.0);
        state.place_node(new_id, at);
        state.ensure_positions(
            doc,
            project.midi_tracks.len(),
            project.control_tracks.len(),
            project.audio_tracks.len(),
        );

        assert_eq!(state.positions.get(&PatchId::Node(new_id)), Some(&at));
    }

    /// Layout export/import: stale ids are dropped, existing positions roundtrip.
    #[test]
    fn export_layout_filters_stale_and_roundtrips() {
        let mut project = Project::default();
        let id = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Mixer {
            node_id: id,
            track: 0,
            inputs: 2,
            channels: vec![Default::default(); 2],
            out_pan: 0.0,
            out_muted: false,
        });
        let doc = &project.graph;
        let mut state = PatchState::default();
        state.ensure_positions(
            doc,
            project.midi_tracks.len(),
            project.control_tracks.len(),
            project.audio_tracks.len(),
        );
        // Stale entry: a node id that is no longer in the document.
        state
            .positions
            .insert(PatchId::Node(9999), Pos2::new(1.0, 2.0));

        let layout = state.export_layout(doc);
        assert!(layout.nodes.contains_key(&id), "live node exported");
        assert!(!layout.nodes.contains_key(&9999), "stale id dropped");
        assert!(layout.timeline.is_some());
        assert!(layout.master.is_some());

        let mut fresh = PatchState::default();
        fresh.import_layout(&layout);
        for (want_id, want_p) in &state.positions {
            if *want_id == PatchId::Node(9999) {
                assert!(!fresh.positions.contains_key(want_id));
            } else {
                assert_eq!(fresh.positions.get(want_id), Some(want_p));
            }
        }
    }

    /// Cable-cut hit test: a path hugging a cable's curve hits it; a far-away path
    /// hits nothing.
    #[test]
    fn cut_path_hits_crossing_cable_only() {
        let mut project = Project::default();
        let mixer = project.graph.first_mixer().unwrap().node_id();
        project
            .graph
            .cables
            .push(CableDoc::audio(mixer, PortRef::Master));
        let idx = project.graph.cables.len() - 1;
        let doc = &project.graph;
        let deps = PatchDeps {
            doc,
            track_count: project.midi_tracks.len(),
            auto_count: project.control_tracks.len(),
            audio_count: project.audio_tracks.len(),
            auto_options: &|_| Vec::new(),
            plugin_inline: &|_| Vec::new(),
            sf2_presets: &|_| Vec::new(),
        };
        let mut state = PatchState::default();
        state.ensure_positions(doc, deps.track_count, deps.auto_count, deps.audio_count);

        let rect_min = Pos2::ZERO; // pan = 0, zoom = 1 → screen == graph space
        let bez = state
            .cable_controls(rect_min, &doc.cables[idx], &deps)
            .expect("cable endpoints resolve");
        let path = bezier_points(bez, 8);
        let hits = state.cables_crossing_path(rect_min, &path, &deps);
        assert!(hits.contains(&idx), "path along the cable must hit it");

        let far = [Pos2::new(-3000.0, -3000.0), Pos2::new(-2500.0, -2500.0)];
        assert!(
            state.cables_crossing_path(rect_min, &far, &deps).is_empty(),
            "a path far from all cables must hit nothing"
        );
    }

    /// G5: the static inline registry covers every built-in node variant
    /// (and stays silent for SoundFont / plugin / legacy nodes).
    #[test]
    fn inline_registry_covers_builtin_variants() {
        let synth = NodeDoc::Instrument {
            node_id: 2,
            track: 0,
            voice: VoiceSpec::Builtin,
            synth: glitcherz_model::SynthParams::default(),
        };
        let params: Vec<InlineParam> = inline_params_of(&synth).iter().map(|s| s.param).collect();
        assert_eq!(params, vec![InlineParam::Cutoff, InlineParam::Resonance]);

        let sf2 = NodeDoc::Instrument {
            node_id: 3,
            track: 0,
            voice: VoiceSpec::SoundFont {
                path: String::new(),
                bank: 0,
                program: 0,
                gain: 1.0,
            },
            synth: glitcherz_model::SynthParams::default(),
        };
        // U13: bank / patch (combos) + gain (slider).
        let sf2_params: Vec<InlineParam> = inline_params_of(&sf2).iter().map(|s| s.param).collect();
        assert_eq!(
            sf2_params,
            vec![
                InlineParam::Sf2Bank,
                InlineParam::Sf2Program,
                InlineParam::Sf2Gain
            ]
        );
        assert_eq!(InlineParam::Sf2Bank.row_kind(), RowKind::Combo);
        assert_eq!(InlineParam::Sf2Program.row_kind(), RowKind::Combo);
        assert_eq!(InlineParam::Sf2Gain.row_kind(), RowKind::Slider);

        let clap = NodeDoc::Instrument {
            node_id: 4,
            track: 0,
            voice: VoiceSpec::Clap {
                path: String::new(),
                plugin_id: "p".to_string(),
                state: Vec::new(),
            },
            synth: glitcherz_model::SynthParams::default(),
        };
        assert!(
            inline_params_of(&clap).is_empty(),
            "plugin nodes take their rows from the session enumeration"
        );

        // U5: sample instruments expose root + gain inline rows.
        let sample = NodeDoc::Instrument {
            node_id: 7,
            track: 0,
            voice: VoiceSpec::Sample {
                path: "kick.wav".to_string(),
                root_note: 60,
                play_mode: glitcherz_model::SamplePlayMode::Gate,
                gain: 1.0,
            },
            synth: glitcherz_model::SynthParams::default(),
        };
        let params: Vec<InlineParam> = inline_params_of(&sample).iter().map(|s| s.param).collect();
        assert_eq!(
            params,
            vec![InlineParam::SampleRoot, InlineParam::SampleGain]
        );

        let delay = NodeDoc::Effect {
            node_id: 5,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: glitcherz_model::EffectParams::default(),
            delay: None,
        };
        let params: Vec<InlineParam> = inline_params_of(&delay).iter().map(|s| s.param).collect();
        assert_eq!(
            params,
            vec![
                InlineParam::EffectSlot(0),
                InlineParam::EffectSlot(1),
                InlineParam::EffectSlot(2),
            ],
            "one inline row per descriptor slot, in table order"
        );
        let labels: Vec<&str> = inline_params_of(&delay).iter().map(|s| s.label).collect();
        assert_eq!(labels, vec!["time", "fdbk", "mix"]);

        // E2: the big effects only expose their `INLINE` slots, so the canvas
        // node stays three or four rows tall instead of eight.
        let eq = NodeDoc::Effect {
            node_id: 7,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: glitcherz_model::EffectParams::Eq(Box::default()),
            delay: None,
        };
        let labels: Vec<&str> = inline_params_of(&eq).iter().map(|s| s.label).collect();
        assert_eq!(labels, vec!["low", "mid", "hi", "cut"]);

        let comp = NodeDoc::Effect {
            node_id: 8,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: glitcherz_model::EffectParams::Compressor(Box::default()),
            delay: None,
        };
        let labels: Vec<&str> = inline_params_of(&comp).iter().map(|s| s.label).collect();
        assert_eq!(labels, vec!["thr", "ratio", "mkup"]);

        let mixer = NodeDoc::Mixer {
            node_id: 6,
            track: 0,
            inputs: 2,
            channels: vec![Default::default(); 2],
            out_pan: 0.0,
            out_muted: false,
        };
        let params: Vec<InlineParam> = inline_params_of(&mixer).iter().map(|s| s.param).collect();
        assert_eq!(params, vec![InlineParam::MixerOutPan]);
    }

    /// G5: the node box height grows downward by `rows * INLINE_ROW_HEIGHT`
    /// on top of the existing (port-adaptive) base size; fixed entities are
    /// unaffected.
    #[test]
    fn node_size_grows_with_inline_rows() {
        let mut project = Project::default();
        let plugin_node = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Instrument {
            node_id: plugin_node,
            track: 0,
            voice: VoiceSpec::Clap {
                path: String::new(),
                plugin_id: "p".to_string(),
                state: Vec::new(),
            },
            synth: glitcherz_model::SynthParams::default(),
        });
        let delay_id = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Effect {
            node_id: delay_id,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: glitcherz_model::EffectParams::default(),
            delay: None,
        });
        let doc = &project.graph;
        let deps = PatchDeps {
            doc,
            track_count: project.midi_tracks.len(),
            auto_count: project.control_tracks.len(),
            audio_count: project.audio_tracks.len(),
            auto_options: &|_| Vec::new(),
            sf2_presets: &|_| Vec::new(),
            plugin_inline: &|node| {
                if node == plugin_node {
                    vec![
                        InlinePluginControl {
                            param_id: 1,
                            label: "a".to_string(),
                            value: 0.0,
                            min: 0.0,
                            max: 1.0,
                        },
                        InlinePluginControl {
                            param_id: 2,
                            label: "b".to_string(),
                            value: 0.0,
                            min: 0.0,
                            max: 1.0,
                        },
                    ]
                } else {
                    Vec::new()
                }
            },
        };

        let instr = doc.instrument_of_track(0).unwrap().node_id();
        let delay = delay_id;
        let mixer = doc.first_mixer().unwrap().node_id();
        let NodeDoc::Mixer { inputs, .. } = doc.node_by_id(mixer).unwrap() else {
            unreachable!()
        };
        // Mixer: title + slot action row, channel-scaled port area, 1 registry row.
        let mixer_base = header_band(PatchId::Node(mixer), &deps)
            + (*inputs as f32 * PORT_SPACING).max(NODE_HEIGHT_MIN);

        // Built-in synth: 2 registry rows.
        assert_eq!(
            size_of(PatchId::Node(instr), &deps),
            Vec2::new(NODE_SIZE.x, NODE_SIZE.y + 2.0 * INLINE_ROW_HEIGHT)
        );
        // Built-in delay: 3 registry rows.
        assert_eq!(
            size_of(PatchId::Node(delay), &deps),
            Vec2::new(NODE_SIZE.x, NODE_SIZE.y + 3.0 * INLINE_ROW_HEIGHT)
        );
        // Mixer: channel-scaled base + 1 registry row.
        assert_eq!(
            size_of(PatchId::Node(mixer), &deps),
            Vec2::new(NODE_SIZE.x, mixer_base + INLINE_ROW_HEIGHT)
        );
        // Plugin node: 2 session-enumerated rows.
        assert_eq!(
            size_of(PatchId::Node(plugin_node), &deps),
            Vec2::new(NODE_SIZE.x, NODE_SIZE.y + 2.0 * INLINE_ROW_HEIGHT)
        );
        // Fixed entities: no inline rows.
        assert_eq!(
            size_of(PatchId::Timeline, &deps),
            timeline_size(deps.track_count, deps.auto_count, deps.audio_count)
        );
        assert_eq!(size_of(PatchId::Master, &deps), NODE_SIZE);
    }

    /// G5: `inline_doc_value` reads the node's own document fields and
    /// mirrors the panel slider ranges (one slider-geometry source per
    /// parameter — inline and panel sliders are interchangeable).
    #[test]
    fn inline_doc_value_reads_document_fields() {
        let synth = glitcherz_model::SynthParams {
            cutoff_hz: 440.0,
            resonance: 0.5,
            ..Default::default()
        };
        let instr = NodeDoc::Instrument {
            node_id: 2,
            track: 0,
            voice: VoiceSpec::Builtin,
            synth,
        };
        let (v, min, max) = inline_doc_value(&instr, InlineParam::Cutoff);
        assert_eq!(v, 440.0);
        assert_eq!(
            (min, max),
            (
                *crate::CUTOFF_RANGE.start() as f64,
                *crate::CUTOFF_RANGE.end() as f64
            )
        );
        let (v, min, max) = inline_doc_value(&instr, InlineParam::Resonance);
        assert_eq!(v, 0.5);
        assert_eq!(
            (min, max),
            (
                *crate::RES_RANGE.start() as f64,
                *crate::RES_RANGE.end() as f64
            )
        );

        let delay = glitcherz_model::DelayParams {
            time_s: 0.25,
            feedback: 0.4,
            mix: 0.3,
            ..Default::default()
        };
        let fx = NodeDoc::Effect {
            node_id: 3,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: glitcherz_model::EffectParams::Delay(delay),
            delay: None,
        };
        assert_eq!(inline_doc_value(&fx, InlineParam::EffectSlot(0)).0, 0.25);
        assert!((inline_doc_value(&fx, InlineParam::EffectSlot(1)).0 - 0.4).abs() < 1e-6);
        assert!((inline_doc_value(&fx, InlineParam::EffectSlot(2)).0 - 0.3).abs() < 1e-6);
        let (_, min, max) = inline_doc_value(&fx, InlineParam::EffectSlot(0));
        assert_eq!(
            (min, max),
            (
                *crate::DELAY_TIME_RANGE.start() as f64,
                *crate::DELAY_TIME_RANGE.end() as f64
            ),
            "the inline range comes from the engine descriptor"
        );

        let mixer = NodeDoc::Mixer {
            node_id: 4,
            track: 0,
            inputs: 2,
            channels: vec![Default::default(); 2],
            out_pan: -0.5,
            out_muted: false,
        };
        let (v, min, max) = inline_doc_value(&mixer, InlineParam::MixerOutPan);
        assert_eq!(v, -0.5);
        assert_eq!((min, max), (-1.0, 1.0));

        // U5: sample root / gain read from the voice spec fields; ranges
        // mirror the Inspector DragValue (0..127) and slider (0..2).
        let sample = NodeDoc::Instrument {
            node_id: 5,
            track: 0,
            voice: VoiceSpec::Sample {
                path: "kick.wav".to_string(),
                root_note: 48,
                play_mode: glitcherz_model::SamplePlayMode::OneShot,
                gain: 0.5,
            },
            synth: glitcherz_model::SynthParams::default(),
        };
        let (v, min, max) = inline_doc_value(&sample, InlineParam::SampleRoot);
        assert_eq!((v, min, max), (48.0, 0.0, 127.0));
        let (v, min, max) = inline_doc_value(&sample, InlineParam::SampleGain);
        assert_eq!((v, min, max), (0.5, 0.0, 2.0));
    }

    #[test]
    fn inline_value_format_follows_span() {
        // -1..=1-style spans get two decimals (pan / gain / mix rows).
        assert_eq!(fmt_inline_value(0.25, -1.0, 1.0), "0.25");
        // Tens-wide spans get one decimal.
        assert_eq!(fmt_inline_value(1.234, 0.0, 10.0), "1.2");
        // Hertz-wide spans stay integral (cutoff 20..=20000).
        assert_eq!(fmt_inline_value(440.4, 20.0, 20000.0), "440");
        // Degenerate range still yields a finite handle position.
        assert_eq!(inline_ratio(7.0, 3.0, 3.0), 0.5);
        assert_eq!(inline_ratio(-5.0, 0.0, 1.0), 0.0);
        assert_eq!(inline_ratio(5.0, 0.0, 1.0), 1.0);
    }

    #[test]
    fn inline_row_parts_stay_inside_the_row() {
        // A zoom-1 row of one synth node box: label, rail and value must all
        // fit, and the hit rect must not reach the label column or spill
        // past the row. This is the drawn-slider contract the egui-native
        // one broke (fixed style font / rail → overflow at low zoom).
        for z in [1.0f32, ACTION_MIN_ZOOM, 1.75] {
            let row = Rect::from_min_size(
                Pos2::new(100.0, 200.0),
                Vec2::new(150.0 * z, INLINE_ROW_HEIGHT * z),
            );
            let (label, rail, value, hit) = inline_row_parts(row, z);
            assert!(label.x > row.left() && label.x < rail.left() + 1.0);
            assert!(rail.left() >= row.left() && rail.right() <= row.right());
            assert!(rail.top() >= row.top() && rail.bottom() <= row.bottom());
            assert!(value.x <= row.right() + 0.5 && value.x > rail.right());
            assert_eq!(hit.left(), rail.left(), "hit starts at the rail");
            assert!(hit.right() <= row.right());
            assert!(hit.top() >= row.top() && hit.bottom() <= row.bottom());
            // The label column is not part of the hit rect: pressing it
            // drags the node, not the parameter.
            assert!(hit.left() >= row.left() + (INLINE_INSET + INLINE_LABEL_WIDTH) * z - 0.5);
            // The rail keeps a positive width even at the smallest zoom.
            assert!(rail.width() > 10.0, "rail too narrow at zoom {z}");
        }
    }

    #[test]
    fn dist_point_seg_basics() {
        let a = Pos2::new(0.0, 0.0);
        let b = Pos2::new(10.0, 0.0);
        assert_eq!(dist_point_seg(Pos2::new(5.0, 3.0), a, b), 3.0);
        assert_eq!(dist_point_seg(Pos2::new(-4.0, 0.0), a, b), 4.0);
        // Beyond the end: clamps to the endpoint b.
        assert_eq!(dist_point_seg(Pos2::new(15.0, 0.0), a, b), 5.0);
        assert!((dist_point_seg(Pos2::new(15.0, 4.0), a, b) - 41.0f32.sqrt()).abs() < 1e-4);
        assert_eq!(dist_point_seg(Pos2::new(1.0, 0.0), a, a), 1.0);
    }

    #[test]
    fn timeline_ports_map_to_track_refs() {
        // 2 MIDI + 3 control + 1 audio → 6 ports: MIDI, then curve, then audio.
        assert_eq!(
            timeline_port_kind(0, 2, 3),
            TimelinePort::Midi(0),
            "port 0 = MIDI track 0"
        );
        assert_eq!(
            timeline_port_kind(1, 2, 3),
            TimelinePort::Midi(1),
            "port 1 = MIDI track 1"
        );
        assert_eq!(
            timeline_port_kind(2, 2, 3),
            TimelinePort::Auto(0),
            "port 2 = control track 0"
        );
        assert_eq!(
            timeline_port_kind(4, 2, 3),
            TimelinePort::Auto(2),
            "port 4 = control track 2"
        );
        assert_eq!(
            timeline_port_kind(5, 2, 3),
            TimelinePort::Audio(0),
            "port 5 = audio track 0"
        );
        assert_eq!(timeline_port_count(2, 3, 1), 6);
    }

    /// U6 follow-up: an audio-track node is folded into the timeline box. It is
    /// never placed as a standalone box (so never drawn / hit-tested), and its
    /// cable is resolved from the timeline's matching audio port (the port
    /// order is MIDI → Curve → Audio).
    #[test]
    fn audio_track_node_folds_into_timeline_ports() {
        let project = Project::default();
        let doc = &project.graph;
        let node_id = audio_node_id(doc, 0).expect("default audio-track node");
        let deps = PatchDeps {
            doc,
            track_count: project.midi_tracks.len(),
            auto_count: project.control_tracks.len(),
            audio_count: project.audio_tracks.len(),
            auto_options: &|_| Vec::new(),
            plugin_inline: &|_| Vec::new(),
            sf2_presets: &|_| Vec::new(),
        };

        let mut state = PatchState::default();
        state.ensure_positions(doc, deps.track_count, deps.auto_count, deps.audio_count);
        assert!(
            !state.positions.contains_key(&PatchId::Node(node_id)),
            "the audio-track node is folded into the timeline box, not placed"
        );

        // Node id ↔ track index round-trips, and the cable endpoint resolves to
        // the timeline's audio port (after every MIDI and curve port).
        assert_eq!(audio_track_of_node(doc, node_id), Some(0));
        assert_eq!(audio_node_id(doc, 0), Some(node_id));
        let expected_port = deps.track_count + deps.auto_count;
        assert_eq!(
            patch_port_from(&PortRef::Node(node_id), &deps),
            Some((PatchId::Timeline, expected_port))
        );

        let ports = state.out_ports_screen(Pos2::ZERO, PatchId::Timeline, &deps);
        assert_eq!(
            ports.len(),
            deps.track_count + deps.auto_count + deps.audio_count,
            "the timeline exposes one port per track of every kind"
        );
    }

    #[test]
    fn port_type_rules_reject_mismatches() {
        let mut project = Project::default();
        let delay_id = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Effect {
            node_id: delay_id,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: glitcherz_model::EffectParams::default(),
            delay: None,
        });
        let doc = &project.graph;
        let instrument = doc.instrument_of_track(0).unwrap().node_id();
        let mixer = doc.first_mixer().unwrap().node_id();
        let delay = delay_id;

        // Audio: effects/mixers/master output accept; instruments reject.
        assert!(accepts(CableKind::Audio, PatchId::Node(delay), doc));
        assert!(accepts(CableKind::Audio, PatchId::Node(mixer), doc));
        assert!(accepts(CableKind::Audio, PatchId::Master, doc));
        assert!(!accepts(CableKind::Audio, PatchId::Node(instrument), doc));
        // MIDI: instruments accept; everything else rejects.
        assert!(accepts(CableKind::Midi, PatchId::Node(instrument), doc));
        assert!(!accepts(CableKind::Midi, PatchId::Node(mixer), doc));
        assert!(!accepts(CableKind::Midi, PatchId::Master, doc));
        // Automation (U4c, spec `control-tracks.md` §9 D19): every node with
        // automatable parameters accepts it — instruments, built-in effects,
        // mixers and feedback; master (not a node) and the timeline do not.
        assert!(accepts(
            CableKind::Automation,
            PatchId::Node(instrument),
            doc
        ));
        assert!(accepts(CableKind::Automation, PatchId::Node(delay), doc));
        assert!(
            accepts(CableKind::Automation, PatchId::Node(mixer), doc),
            "mixer channel faders are automation targets now"
        );
        assert!(!accepts(CableKind::Automation, PatchId::Master, doc));
    }

    /// G6 (spec `plugin-gui.md` §8): an input node is a MIDI-only source with
    /// no audio ports, its own compact box, and a label naming its **source**
    /// (the track is only a placement hint).
    #[test]
    fn virtual_keyboard_node_geometry_and_port_rules() {
        let mut doc = Project::default().graph;
        let instrument = doc.instrument_of_track(0).unwrap().node_id();
        doc.nodes.push(NodeDoc::Keyboard {
            node_id: 99,
            track: 0,
            source: MidiSource::Device("KeyStep".to_string()),
        });
        let auto_options = |_: u64| -> Vec<(String, AutoParam)> { Vec::new() };
        let plugin_inline = |_: u64| -> Vec<InlinePluginControl> { Vec::new() };
        let sf2_presets = |_: u64| -> Vec<(u8, u8, String)> { Vec::new() };
        let deps = PatchDeps {
            doc: &doc,
            track_count: 1,
            auto_count: 0,
            audio_count: 0,
            auto_options: &auto_options,
            plugin_inline: &plugin_inline,
            sf2_presets: &sf2_presets,
        };
        let vk = PatchId::Node(99);

        // MIDI reaches instruments only; the vk node itself is no target.
        assert!(accepts(CableKind::Midi, PatchId::Node(instrument), &doc));
        assert!(!accepts(CableKind::Midi, vk, &doc));
        assert!(!accepts(CableKind::Audio, vk, &doc));

        // Geometry: compact box, exactly one (MIDI) out port, no in ports.
        assert_eq!(size_of(vk, &deps), KEYBOARD_SIZE);
        assert_eq!(inline_row_count(vk, &deps), 0);
        let mut state = PatchState::default();
        state.place_node(99, Pos2::ZERO);
        let min = Pos2::ZERO;
        assert_eq!(state.out_ports_screen(min, vk, &deps).len(), 1);
        assert!(state.in_ports_screen(min, vk, &deps).is_empty());
        assert_eq!(label_of(vk, &doc), "Keyboard · KeyStep");
        // The default project ships a computer-keyboard node instead.
        let default_vk = doc.computer_keyboard().expect("default keyboard node");
        assert_eq!(
            label_of(PatchId::Node(default_vk), &doc),
            "Keyboard · Computer"
        );
    }

    /// U10: the timeline and mixer boxes reserve action rows for their own
    /// add / remove buttons, and their ports stay in the port area below;
    /// only those two entities carry buttons, and the virtual keyboard gates
    /// the Inspector double-click off.
    #[test]
    fn action_rows_reserve_space_and_only_owner_nodes_have_them() {
        let project = Project::default();
        let doc = &project.graph;
        let auto_options = |_: u64| -> Vec<(String, AutoParam)> { Vec::new() };
        let plugin_inline = |_: u64| -> Vec<InlinePluginControl> { Vec::new() };
        let sf2_presets = |_: u64| -> Vec<(u8, u8, String)> { Vec::new() };
        let track_count = project.midi_tracks.len();
        let auto_count = 1;
        let audio_count = 1;
        let deps = PatchDeps {
            doc,
            track_count,
            auto_count,
            audio_count,
            auto_options: &auto_options,
            plugin_inline: &plugin_inline,
            sf2_presets: &sf2_presets,
        };
        let mut state = PatchState::default();
        state.positions.insert(PatchId::Timeline, Pos2::ZERO);
        let mixer = doc.first_mixer().unwrap().node_id();
        state.place_node(mixer, Pos2::new(0.0, 400.0));

        // Timeline: title + three action rows, then one row per port.
        assert_eq!(action_rows(PatchId::Timeline, &deps), TIMELINE_ACTION_ROWS);
        let r = state
            .screen_rect(Pos2::ZERO, PatchId::Timeline, &deps)
            .unwrap();
        assert_eq!(
            r.height(),
            TIMELINE_HEADER + TIMELINE_ROW_HEIGHT * (track_count + auto_count + audio_count) as f32
        );
        let ports = state.out_ports_screen(Pos2::ZERO, PatchId::Timeline, &deps);
        assert_eq!(ports.len(), track_count + auto_count + audio_count);
        for (_, p) in &ports {
            assert!(p.y >= r.top() + TIMELINE_HEADER, "port under the buttons");
            assert!(p.y <= r.bottom());
        }

        // Mixer: title + slot row above the port area, inline row below it.
        let mid = PatchId::Node(mixer);
        let m = state.screen_rect(Pos2::ZERO, mid, &deps).unwrap();
        let band = header_band(mid, &deps);
        let inline_top = m.bottom() - INLINE_ROW_HEIGHT; // out-pan row
        let in_ports = state.in_ports_screen(Pos2::ZERO, mid, &deps);
        assert!(!in_ports.is_empty());
        for (_, p) in &in_ports {
            assert!(p.y >= m.top() + band, "port under the slot buttons");
            assert!(p.y <= inline_top, "port above the inline slider row");
        }
        assert_eq!(action_rows(mid, &deps), 1);

        // Only the two owner nodes carry buttons.
        assert_eq!(action_rows(PatchId::Master, &deps), 0);
        assert_eq!(action_rows(PatchId::TimelineIn, &deps), 0);
        assert!(node_action_buttons(PatchId::Master, 0, &deps).is_empty());
        let midi_row = node_action_buttons(PatchId::Timeline, 0, &deps);
        assert_eq!((midi_row[0].label, midi_row[1].label), ("+ MIDI", "- MIDI"));
        assert!(midi_row[0].enabled && midi_row[1].enabled);
        let curve_row = node_action_buttons(PatchId::Timeline, 1, &deps);
        assert_eq!(
            (curve_row[0].label, curve_row[1].label),
            ("+ Curve", "- Curve")
        );
        let audio_row = node_action_buttons(PatchId::Timeline, 2, &deps);
        assert_eq!(
            (audio_row[0].label, audio_row[1].label),
            ("+ Audio", "- Audio")
        );
        assert!(audio_row[0].enabled && audio_row[1].enabled);
        // Default project: every slot carries a track cable → remove refuses.
        let slot_row = node_action_buttons(mid, 0, &deps);
        assert!(slot_row[0].enabled);
        assert!(!slot_row[1].enabled, "last slot carries a track cable");

        // A free last slot can be dropped; a cable landing on it blocks it
        // again (the button mirrors `mixer_remove_channel`).
        let mut free = Project::default();
        let fm = free.graph.first_mixer().map(NodeDoc::node_id).unwrap();
        let NodeDoc::Mixer { inputs, .. } = free.graph.node_by_id(fm).unwrap().clone() else {
            unreachable!()
        };
        if let Some(NodeDoc::Mixer {
            inputs, channels, ..
        }) = free.graph.node_by_id_mut(fm)
        {
            *inputs += 1;
            channels.push(Default::default());
        }
        let fdeps = PatchDeps {
            doc: &free.graph,
            track_count,
            auto_count,
            audio_count,
            auto_options: &auto_options,
            plugin_inline: &plugin_inline,
            sf2_presets: &sf2_presets,
        };
        assert!(node_action_buttons(PatchId::Node(fm), 0, &fdeps)[1].enabled);
        let from = free
            .graph
            .instrument_of_track(0)
            .map(NodeDoc::node_id)
            .unwrap();
        free.graph
            .cables
            .push(CableDoc::audio_to_channel(from, fm, inputs));
        let fdeps = PatchDeps {
            doc: &free.graph,
            track_count,
            auto_count,
            audio_count,
            auto_options: &auto_options,
            plugin_inline: &plugin_inline,
            sf2_presets: &sf2_presets,
        };
        assert!(!node_action_buttons(PatchId::Node(fm), 0, &fdeps)[1].enabled);

        // Double-click gate: keyboard / feedback have no panel to show.
        let keyboard = doc
            .nodes
            .iter()
            .find(|n| n.is_keyboard())
            .cloned()
            .expect("default project has a keyboard node");
        assert!(!opens_inspector(Some(&keyboard)));
        assert!(opens_inspector(doc.node_by_id(mixer)));
    }

    /// Regression: the in-node action rows are hand-painted because an
    /// `egui::Button` never gets shorter than `interact_size.y` (18 pt). At
    /// zoom < 1 that floor made the button taller than its `18 * zoom` row,
    /// so it was centred on the row and painted over the node title. The
    /// geometry must keep every button inside its band at any zoom.
    #[test]
    fn action_buttons_stay_inside_their_band_at_any_zoom() {
        for z in [ACTION_MIN_ZOOM, 0.75, 1.0, 1.75] {
            // A SoundFont node: title band + one action row + body.
            let rect = Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(NODE_SIZE.x, NODE_SIZE.y + ACTION_ROW_HEIGHT) * z,
            );
            for row in 0..2 {
                let band = action_row_band(rect, row, z);
                let band_top = rect.top() + (NODE_TITLE_BAND + row as f32 * ACTION_ROW_HEIGHT) * z;
                assert!(
                    band.top() >= band_top,
                    "row {row} at zoom {z} starts above its band"
                );
                assert!(
                    band.bottom() <= rect.bottom(),
                    "row {row} at zoom {z} leaves the node box"
                );
                for b in action_button_rects(band, 2, z) {
                    assert!(b.top() >= band.top() - 0.01 && b.bottom() <= band.bottom() + 0.01);
                    assert!(b.left() >= rect.left() && b.right() <= rect.right() + 0.01);
                }
            }
        }
    }

    /// U14: a mixer channel strip has to clear the port dot (whose drag hit
    /// radius is 18 pt — closer and pressing the fader would start a cable),
    /// stay inside the box, never overlap the neighbouring channel, and keep
    /// all three cells apart. If the rail collapsed to zero width the fader
    /// would be unclickable rather than merely small.
    #[test]
    fn mixer_channel_strips_clear_the_ports_at_any_zoom() {
        for z in [INLINE_MIN_ZOOM, 0.8, 1.0, 2.0] {
            // A 4-channel mixer box: title band + the slot action row, then
            // one `PORT_SPACING` row per channel.
            let mixer_header = NODE_TITLE_BAND + ACTION_ROW_HEIGHT;
            let node = Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(NODE_SIZE.x, mixer_header + 4.0 * PORT_SPACING) * z,
            );
            for port in 0..4 {
                let y = node.top() + (mixer_header + (port as f32 + 0.5) * PORT_SPACING) * z;
                let dot = Pos2::new(node.left(), y);
                let strip = channel_strip_rect(dot, node, z);
                assert!(
                    strip.left() >= dot.x + PORT_HIT_RADIUS * z,
                    "port {port} at zoom {z}: the strip overlaps the port drag radius"
                );
                assert!(
                    strip.right() <= node.right() - INLINE_INSET * z + 0.01,
                    "port {port} at zoom {z}: the strip leaves the box"
                );
                assert!(strip.height() <= PORT_SPACING * z + 0.01);
                let cells = channel_strip_cells(strip, z);
                assert!(
                    cells.rail.width() > 1.0,
                    "port {port} at zoom {z}: no fader"
                );
                assert!(cells.rail.right() < cells.mute.left());
                assert!(cells.mute.right() < cells.solo.left() + 0.01);
                assert!(cells.solo.right() <= strip.right() + 0.01);
                for cell in [cells.rail, cells.mute, cells.solo] {
                    assert!(
                        cell.top() >= strip.top() - 0.01 && cell.bottom() <= strip.bottom() + 0.01
                    );
                }
            }
        }
    }

    /// U14: the strips read the console's document fields, so a fader moved on
    /// the canvas and the same channel in the Inspector always agree. This is
    /// the half of "同步" that can be tested without a window; the other half
    /// is the shared `PatchAction` → `apply_inline_param` path.
    #[test]
    fn mixer_strips_read_the_console_fields() {
        let node = NodeDoc::Mixer {
            node_id: 7,
            track: 0,
            inputs: 2,
            channels: vec![
                glitcherz_model::MixerChannel {
                    gain: 0.5,
                    pan: 0.0,
                    muted: true,
                    solo: false,
                },
                glitcherz_model::MixerChannel {
                    gain: 1.25,
                    pan: 0.0,
                    muted: false,
                    solo: true,
                },
            ],
            out_pan: 0.0,
            out_muted: false,
        };
        let (gain0, min, max) = inline_doc_value(&node, InlineParam::MixerGain(0));
        assert!((gain0 - 0.5).abs() < 1e-6);
        assert!(min < 0.5 && max > 1.25);
        assert_eq!(inline_doc_value(&node, InlineParam::MixerGain(1)).0, 1.25);
        assert_eq!(inline_doc_value(&node, InlineParam::MixerMute(0)).0, 1.0);
        assert_eq!(inline_doc_value(&node, InlineParam::MixerMute(1)).0, 0.0);
        assert_eq!(inline_doc_value(&node, InlineParam::MixerSolo(0)).0, 0.0);
        assert_eq!(inline_doc_value(&node, InlineParam::MixerSolo(1)).0, 1.0);
        // Out-of-range channels degrade to silence-neutral values instead of
        // panicking (the strip loop can outlive a channel removal by a frame).
        assert_eq!(inline_doc_value(&node, InlineParam::MixerGain(9)).0, 0.0);
        assert_eq!(inline_doc_value(&node, InlineParam::MixerMute(9)).0, 0.0);
    }

    /// U13: a SoundFont node is self-sufficient. Its box carries the file
    /// button, the preset address (two combos) and the output gain, its
    /// label names the **file** (a `bank:program` label goes stale the moment
    /// the file can be swapped), and its preset lists come from the session
    /// table rather than from the document.
    #[test]
    fn soundfont_node_owns_file_preset_and_gain() {
        let presets: Vec<(u8, u8, String)> = vec![
            (0, 40, "Violin".to_string()),
            (0, 0, "Grand Piano".to_string()),
            (8, 0, "Standard Kit".to_string()),
        ];
        let mut project = Project::default();
        let node = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Instrument {
            node_id: node,
            track: 0,
            voice: VoiceSpec::SoundFont {
                path: String::new(),
                bank: 0,
                program: 0,
                gain: 1.0,
            },
            synth: glitcherz_model::SynthParams::default(),
        });
        let id = PatchId::Node(node);

        // Born empty: silent, one file button, no preset to offer yet.
        {
            let table = |target: u64| -> Vec<(u8, u8, String)> {
                if target == node {
                    presets.clone()
                } else {
                    Vec::new()
                }
            };
            let deps = PatchDeps {
                doc: &project.graph,
                track_count: 1,
                auto_count: 0,
                audio_count: 0,
                auto_options: &|_| Vec::new(),
                plugin_inline: &|_| Vec::new(),
                sf2_presets: &table,
            };
            assert_eq!(label_of(id, &project.graph), "SF2 · (no file)");
            assert_eq!(action_rows(id, &deps), 1);
            assert_eq!(header_band(id, &deps), NODE_TITLE_BAND + ACTION_ROW_HEIGHT);
            let row = node_action_buttons(id, 0, &deps);
            assert_eq!(row.len(), 1);
            assert_eq!(row[0].label, "Load File…");
            assert!(matches!(
                row[0].action,
                PatchAction::LoadSoundFont { node: target } if target == node
            ));
            // Box: title band + file row + bank / patch / gain.
            assert_eq!(inline_row_count(id, &deps), 3);
            assert_eq!(
                size_of(id, &deps),
                Vec2::new(
                    NODE_SIZE.x,
                    NODE_SIZE.y + ACTION_ROW_HEIGHT + 3.0 * INLINE_ROW_HEIGHT
                )
            );
        }

        // Loaded: the label switches to the file stem, the button to Replace.
        if let Some(NodeDoc::Instrument { voice, .. }) = project.graph.node_by_id_mut(node) {
            *voice = VoiceSpec::SoundFont {
                path: "C:/fonts/GPiano.sf2".to_string(),
                bank: 0,
                program: 0,
                gain: 1.0,
            };
        }
        {
            let table = |target: u64| -> Vec<(u8, u8, String)> {
                if target == node {
                    presets.clone()
                } else {
                    Vec::new()
                }
            };
            let deps = PatchDeps {
                doc: &project.graph,
                track_count: 1,
                auto_count: 0,
                audio_count: 0,
                auto_options: &|_| Vec::new(),
                plugin_inline: &|_| Vec::new(),
                sf2_presets: &table,
            };
            // Stem, not the full name: `file_stem_of` truncates for the box.
            assert_eq!(label_of(id, &project.graph), "SF2 · GPiano");
            assert_eq!(node_action_buttons(id, 0, &deps)[0].label, "Replace…");
            // Row values come from the document, not from the session table.
            let doc_node = project.graph.node_by_id(node).unwrap();
            assert_eq!(inline_doc_value(doc_node, InlineParam::Sf2Bank).0, 0.0);
            assert_eq!(inline_doc_value(doc_node, InlineParam::Sf2Program).0, 0.0);
            assert_eq!(inline_doc_value(doc_node, InlineParam::Sf2Gain).0, 1.0);
        }

        // Combo lists: distinct banks ascending; patches of the current bank
        // ordered by program (GM order, not file order).
        assert_eq!(
            sf2_choices(&presets, InlineParam::Sf2Bank, 0),
            vec![(0, "Bank 0".to_string()), (8, "Bank 8".to_string())]
        );
        let patches = sf2_choices(&presets, InlineParam::Sf2Program, 0);
        assert_eq!(patches[0], (0, "0. Grand Piano".to_string()));
        assert_eq!(patches[1], (40, "40. Violin".to_string()));
        assert_eq!(
            sf2_choices(&presets, InlineParam::Sf2Program, 8),
            vec![(0, "0. Standard Kit".to_string())]
        );
        // A slider parameter has no choices.
        assert!(sf2_choices(&presets, InlineParam::Sf2Gain, 0).is_empty());
    }
}
