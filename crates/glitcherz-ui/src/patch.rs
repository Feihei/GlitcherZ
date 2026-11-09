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
    AutoParam, BuiltinEffectKind, CableDoc, CableKind, ChannelParam, CurveMapParams, GraphDoc,
    LFO_DIVISION_LABELS, LayoutDoc, MAX_AUDIO_TRACKS, MAX_AUTO_TRACKS, MAX_MIDI_TRACKS,
    MAX_MIXER_CHANNELS, MIN_MIXER_CHANNELS, MidiSource, NodeDoc, NodeParam, PortRef, TrackKind,
    VoiceSpec, note_name, timeline_in_port, timeline_in_port_kind,
};

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
    /// Live input node output gain (R4): document field, 0..2.
    InputGain,
    /// Live input node channel selector (R4): a combo row, 0 / 1.
    InputChannel,
    /// Modal instrument decay multiplier (B1): the "how long does it ring"
    /// control, the one most worth having on the node body.
    ModalDecay,
    /// Modal instrument strike brightness (B1).
    ModalBrightness,
    /// Plucked-string fundamental T60 (S1): the "how long does it ring"
    /// control, the one most worth having on the node body.
    PluckDecay,
    /// Plucked-string loop damping (S1).
    PluckDamping,
    /// Drawbar organ 8' drawbar level (S2): the bar every registration moves.
    OrganBar8,
    /// Drawbar organ percussion level (S2).
    OrganPercussion,
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
            InlineParam::Sf2Bank | InlineParam::Sf2Program | InlineParam::InputChannel => {
                RowKind::Combo
            }
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

/// Modal inline rows (B1): decay and strike brightness. The preset, beating,
/// hardness and gain controls live in the Inspector — a modal node must not
/// turn into a column of sliders.
const MODAL_INLINE: [InlineParamSpec; 2] = [
    InlineParamSpec {
        param: InlineParam::ModalDecay,
        label: "decay",
    },
    InlineParamSpec {
        param: InlineParam::ModalBrightness,
        label: "bright",
    },
];

/// Pluck inline rows (S1): decay and damping. Brightness / release / gain
/// live in the Inspector — same rule as the modal node.
const PLUCK_INLINE: [InlineParamSpec; 2] = [
    InlineParamSpec {
        param: InlineParam::PluckDecay,
        label: "decay",
    },
    InlineParamSpec {
        param: InlineParam::PluckDamping,
        label: "damp",
    },
];

/// Organ inline rows (S2): the 8' drawbar and the percussion level. A full
/// registration would be nine rows on the node body — the Inspector owns it.
const ORGAN_INLINE: [InlineParamSpec; 2] = [
    InlineParamSpec {
        param: InlineParam::OrganBar8,
        label: "8'",
    },
    InlineParamSpec {
        param: InlineParam::OrganPercussion,
        label: "perc",
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

/// Live input inline rows (R4): which device channel is taken, and its
/// output trim. Row order matches the Inspector panel (channel, then gain).
const INPUT_INLINE: [InlineParamSpec; 2] = [
    InlineParamSpec {
        param: InlineParam::InputChannel,
        label: "ch",
    },
    InlineParamSpec {
        param: InlineParam::InputGain,
        label: "gain",
    },
];

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
        NodeDoc::Instrument {
            voice: VoiceSpec::Modal { .. },
            ..
        } => MODAL_INLINE.to_vec(),
        NodeDoc::Instrument {
            voice: VoiceSpec::Pluck { .. },
            ..
        } => PLUCK_INLINE.to_vec(),
        NodeDoc::Instrument {
            voice: VoiceSpec::Organ { .. },
            ..
        } => ORGAN_INLINE.to_vec(),
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
        NodeDoc::Input { .. } => INPUT_INLINE.to_vec(),
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
        // R4: the live input node owns a device channel and an output trim.
        (NodeDoc::Input { channel, .. }, InlineParam::InputChannel) => {
            (f64::from(*channel), 0.0, 1.0)
        }
        (NodeDoc::Input { gain, .. }, InlineParam::InputGain) => (f64::from(*gain), 0.0, 2.0),
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
        // B1: the two modal rows read the same document fields the Inspector
        // sliders edit.
        (
            NodeDoc::Instrument {
                voice: VoiceSpec::Modal { params },
                ..
            },
            InlineParam::ModalDecay,
        ) => (f64::from(params.decay_scale), 0.25, 4.0),
        (
            NodeDoc::Instrument {
                voice: VoiceSpec::Modal { params },
                ..
            },
            InlineParam::ModalBrightness,
        ) => (f64::from(params.brightness), 0.0, 1.0),
        // S1: the two pluck rows read the same document fields the Inspector
        // sliders edit.
        (
            NodeDoc::Instrument {
                voice: VoiceSpec::Pluck { params },
                ..
            },
            InlineParam::PluckDecay,
        ) => (f64::from(params.decay_s), 0.1, 20.0),
        (
            NodeDoc::Instrument {
                voice: VoiceSpec::Pluck { params },
                ..
            },
            InlineParam::PluckDamping,
        ) => (f64::from(params.damping), 0.0, 1.0),
        // S2: the 8' row edits `bars[2]`, the same field the Inspector grid's
        // 8' cell writes.
        (
            NodeDoc::Instrument {
                voice: VoiceSpec::Organ { params },
                ..
            },
            InlineParam::OrganBar8,
        ) => (f64::from(params.bars[2]), 0.0, 8.0),
        (
            NodeDoc::Instrument {
                voice: VoiceSpec::Organ { params },
                ..
            },
            InlineParam::OrganPercussion,
        ) => (f64::from(params.percussion), 0.0, 1.0),
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
/// Channel list of a live input node (R4): the document exposes the first
/// two device channels, whatever the stream actually carries. The engine
/// clamps an unavailable channel, and the Inspector says so.
fn input_channel_choices() -> Vec<(u8, String)> {
    vec![(0, "Ch 1".to_string()), (1, "Ch 2".to_string())]
}

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
    /// MIDI effect / control source (built-in only, spec `midi-nodes.md`).
    MidiFx,
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
    /// Audio file sample player (U5, spec `sample-player.md`; formats in
    /// `audio-decoding.md`). Like [`Self::SoundFont`] the path may be
    /// **empty**: that is the normal "born empty" node, silent
    /// until its own `Load File…` button swaps a real voice in (v3) — adding
    /// one never blocks on a dialog, and a wrong pick is one click to replace.
    Sample {
        /// Audio file path, or empty for a node waiting for one.
        path: String,
    },
    /// Multi-sample drum kit (W1, spec `drum-kit.md` K2): born with no
    /// slots; the node's own `Load Samples…` button fills it.
    DrumKit,
    /// Modal strike instrument (B1, spec `modal-instrument.md`): mallet /
    /// vibraphone / bell / singing bowl. Born audible — the preset tables are
    /// built in, so there is no file to pick.
    Modal,
    /// Plucked string (S1, spec `pluck-instrument.md`): Karplus-Strong.
    /// Born audible for the same reason — the string is the engine.
    Pluck,
    /// Drawbar organ (S2, spec `organ-instrument.md`): nine sine drawbars
    /// plus a percussion partial. Born audible — there is no file to pick.
    Organ,
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
    /// Live audio input node (R4, spec `recording.md` D9): 1 audio out, no
    /// MIDI. Routes a microphone / instrument into the graph and arms audio
    /// track recording inputs.
    AudioInput,
    /// Arpeggiator MIDI effect (spec `midi-nodes.md` §5.1).
    Arpeggiator,
    /// Portamento MIDI effect + control source (spec `midi-nodes.md` §5.2).
    Portamento,
    /// MIDI-triggered envelope control source (spec `midi-nodes.md` §5.3).
    MidiEnvelope,
    /// Free-running LFO control source (spec `lfo.md`): 1 control out, no
    /// audio / MIDI ports.
    Lfo,
    /// Curve Map control node (spec `curve-map.md` D5): 1 control in, 1
    /// control out, and a declared axis — a mapping card the user can put in
    /// front of any destination.
    CurveMap,
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

    /// Whether this source can be spliced into a MIDI cable
    /// (`spec midi-nodes.md` §8).
    pub fn fits_midi_insert(&self) -> bool {
        matches!(
            self,
            NodeSource::Arpeggiator | NodeSource::Portamento | NodeSource::MidiEnvelope
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
    /// Open the plugin picker for `category` (spec `plugin-browser.md` D4/D5,
    /// v3 D16): `None` category = the global `Search Nodes…` entry, whose pick
    /// decides the node category from the entry's own classification. The
    /// picker lists built-in nodes and plugins alike (tagged `BUILT-IN` /
    /// `CLAP` / `VST3`), so there is no format argument any more.
    /// `place` says whether the pick becomes a dangling node or is spliced into
    /// a cable.
    OpenPluginPicker {
        /// Category the menu leaf asked for (`None` = both).
        category: Option<NodeCategory>,
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
    /// Open the wav file dialog for an **existing** sample node and swap its
    /// voice in place (v3, mirrors `LoadSoundFont`): the node keeps its id,
    /// cables, root note, play mode and gain, so a wrong file is one click to
    /// fix instead of delete + re-add.
    LoadSample {
        node: u64,
    },
    /// Open a multi-select wav dialog for an existing drum-kit node (W1).
    /// New files are appended as slots with free MIDI keys; the node keeps
    /// its id and cables, and the engine processor is rebuilt in place.
    LoadDrumSamples {
        node: u64,
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
    /// Node-to-node MIDI cable (spec `midi-nodes.md` D10): a MIDI effect's
    /// output → instrument / MIDI-effect document node.
    ConnectEventMidi {
        from_doc: u64,
        to_doc: u64,
    },
    /// Node-to-node control cable (spec `midi-nodes.md` D6): a control source
    /// (Portamento / MidiEnvelope) → a node parameter. `param` is picked in
    /// the same popup the timeline automation cable uses.
    ConnectControl {
        from_doc: u64,
        to_doc: u64,
        param: AutoParam,
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
    /// R6: connect a source into one track's **recording input** on the fixed
    /// `Timeline In` terminal. `from` is the drag's source port (a node, a
    /// timeline MIDI outlet or a timeline automation outlet); `kind` + `track`
    /// identify the destination port. Document-only: a recording input is a
    /// terminal, not an engine edge.
    ConnectTrackIn {
        from: PortRef,
        kind: TrackKind,
        track: usize,
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
    /// `Audio In` node: bind the recording stream to another device. One
    /// stream feeds every audio take, so this is global, not per-node.
    PickInputSource(InputSourceItem),
    /// Re-enumerate recording devices after a hot-plug.
    RefreshInputSources,
    /// Inline rename of a track name, opened by right-clicking a timeline
    /// outlet caption. Presentation only: one undo step, no engine push.
    RenameTrack {
        kind: TrackKind,
        track: usize,
        name: String,
    },
    /// Inline rename of a mixer channel's custom name, opened by right-clicking
    /// a mixer inlet caption. An empty name clears it back to the derived
    /// track / `Ch N` label.
    RenameChannel {
        node: u64,
        channel: u8,
        name: String,
    },
}

/// Double-click gate: whether the Inspector has anything to show for a node.
/// Instruments and effects carry parameters; the mixer carries the channel
/// console; the live input node carries its device picker and channel meter.
/// The virtual keyboard, the legacy strip and feedback taps do not, so
/// double-clicking them stays inert instead of switching to a panel that would
/// fall back to some unrelated track's instrument.
pub(crate) fn opens_inspector(node: Option<&NodeDoc>) -> bool {
    matches!(
        node,
        Some(
            NodeDoc::Instrument { .. }
                | NodeDoc::Effect { .. }
                | NodeDoc::Mixer { .. }
                | NodeDoc::Arpeggiator { .. }
                | NodeDoc::Portamento { .. }
                | NodeDoc::MidiEnvelope { .. }
                // The LFO's whole UI is its Inspector too (spec `lfo.md` §6).
                | NodeDoc::Lfo { .. }
                // The map's whole UI is its Inspector (§5: 双击进 Inspector).
                | NodeDoc::CurveMap { .. }
                // R4: the live input node's panel owns the device picker and
                // the channel meter.
                | NodeDoc::Input { .. }
        )
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
            // v3: the sample player works exactly the same way.
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
            if let Some(NodeDoc::Instrument {
                voice: VoiceSpec::Sample { path, .. },
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
                        hover: "Pick an audio file for this node",
                        action: PatchAction::LoadSample { node: doc_id },
                    }]
                } else {
                    Vec::new()
                };
            }
            if let Some(NodeDoc::Instrument {
                voice: VoiceSpec::DrumKit { .. },
                ..
            }) = deps.doc.node_by_id(doc_id)
            {
                return if row == 0 {
                    vec![NodeAction {
                        label: "Load Samples…",
                        enabled: true,
                        hover: "Pick wav files for this kit (one slot per file)",
                        action: PatchAction::LoadDrumSamples { node: doc_id },
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

/// One row of the recording-source list carried into the patch (R4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSourceItem {
    /// Endpoint name; `None` is the system default input.
    pub name: Option<String>,
    /// Whether the endpoint is a WASAPI loopback (system playback).
    pub loopback: bool,
}

/// Live recording-input snapshot for the patch (R4): the same stream data the
/// Inspector reads. `Default` (no stream, no sources) keeps headless tests and
/// panels that never draw an `Audio In` node free of plumbing.
#[derive(Clone, Copy, Default)]
pub struct InputDeps<'a> {
    /// Whether an input stream is currently open.
    pub open: bool,
    /// Live device name, if open.
    pub device: Option<&'a str>,
    /// Whether the live stream is a loopback capture.
    pub loopback: bool,
    /// Channel count of the live stream (0 when closed).
    pub channels: usize,
    /// Stream error, if it failed to open.
    pub error: Option<&'a str>,
    /// Per-channel peak reader (`0..1`); `None` in headless/test contexts.
    pub peak: Option<&'a dyn Fn(u8) -> f32>,
    /// Enumerated recording sources; `None` where devices cannot be probed.
    pub sources: Option<&'a dyn Fn() -> Vec<InputSourceItem>>,
}

impl InputDeps<'_> {
    /// Peak of one channel, or 0 when no stream / reader is wired.
    fn peak_of(&self, channel: u8) -> f32 {
        self.peak.map_or(0.0, |read| read(channel))
    }
}

/// Snapshot of external data the patch depends on (immutable borrows to avoid borrow conflicts).
///
/// `Copy` on purpose: every field is a reference or a `usize`, so a per-frame
/// snapshot is free — a value type here keeps `show` callable from closures
/// that egui captures mutably (headless tests, panels).
#[derive(Clone, Copy)]
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
    /// Live track name by `(kind, index)`, read every frame. The patch view
    /// has no `Project` borrow (only the graph document), so the timeline and
    /// mixer port captions are supplied by this reader — a rename in the
    /// arrangement shows up here on the very next frame.
    pub track_name: &'a dyn Fn(TrackKind, usize) -> String,
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
    /// Live recording-input snapshot (R4): device picker + per-channel meter
    /// on the `Audio In` node.
    pub input: InputDeps<'a>,
}

/// Cable being dragged (source = the entity + output port owning the drag).
#[derive(Debug, Clone, Copy)]
struct CableDrag {
    from: PatchId,
    /// Output port index (only [`PatchId::Timeline`] has more than one).
    port: usize,
}

/// A name the patch can rename in place: a track (timeline outlet) or a mixer
/// channel (mixer inlet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortNameTarget {
    Track { kind: TrackKind, track: usize },
    Channel { node: u64, channel: u8 },
}

/// One open inline rename editor in the patch view.
#[derive(Debug)]
struct PortRename {
    target: PortNameTarget,
    draft: String,
    /// The right-click that opened the editor lands one frame before the text
    /// field exists — request focus on the first frame that draws it.
    needs_focus: bool,
}

/// One painted port caption plus the rect that answers a right-click.
#[derive(Debug, Clone)]
struct PortNameLabel {
    target: PortNameTarget,
    /// Screen-space hit/draw rect.
    rect: Rect,
    /// Truncated caption actually painted.
    display: String,
    /// Untruncated value used for the tooltip.
    full: String,
    /// Initial draft (`""` for an unset custom channel name).
    raw: String,
    /// Placeholder shown while editing.
    hint: String,
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
    pending_auto: Option<(AutoSource, u64, Pos2)>,
    /// Open add-node menu (right-click / `Shift+A`, spec D17).
    add_menu: Option<AddMenu>,
    /// Ctrl + right-drag cable cut in progress: screen-space pointer path.
    cut_path: Option<Vec<Pos2>>,
    /// Cable-midpoint menu awaiting a pick (D18: one flat level).
    pending_cable_menu: Option<CableMenu>,
    /// Combo row awaiting a pick (U13 SoundFont bank/patch, R4 input channel):
    /// `(node, parameter, popup screen position, frame it was opened)` — same
    /// frame-stamp trick as the cable menu, because a click opens it.
    pending_choice: Option<(u64, InlineParam, Pos2, u64)>,
    /// Recording-source popup (R4) opened from an `Audio In` node's device
    /// chip: `(node, popup position, frame opened)`.
    pending_input_menu: Option<(u64, Pos2, u64)>,
    /// Recording sources captured when `pending_input_menu` opened (R4).
    /// Enumerating devices opens the driver, so it happens once per open,
    /// never per frame.
    input_sources: Vec<InputSourceItem>,
    /// Open inline rename on a track / channel name, if any.
    rename: Option<PortRename>,
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
            add_menu: None,
            cut_path: None,
            pending_cable_menu: None,
            pending_choice: None,
            pending_input_menu: None,
            input_sources: Vec::new(),
            rename: None,
        }
    }
}

/// Open add-node menu: where the new node lands and where the popup sits.
///
/// The popup's open state lives in egui (`Popup::open_memory`), so this only
/// carries the *placement* plus the one-shot open command: `fresh` is taken on
/// the frame the menu is opened (right-click / `Shift+A`) and is `false`
/// afterwards — issuing the command every frame would re-open the menu right
/// after a pick closed it.
#[derive(Debug, Clone, Copy)]
struct AddMenu {
    /// Graph-space position the new node is created at.
    at: Pos2,
    /// Screen-space popup anchor.
    pos: Pos2,
    /// Set on the frame the popup must be opened.
    fresh: bool,
}

/// Cable-midpoint menu (spec D18): insert a processor into the cable, or cut it.
#[derive(Debug, Clone, Copy)]
struct CableMenu {
    /// Index into [`GraphDoc::cables`].
    cable: usize,
    /// Graph-space position the inserted node is created at.
    at: Pos2,
    /// Screen-space popup anchor.
    pos: Pos2,
    /// Set on the frame the popup must be opened.
    fresh: bool,
}

/// Popup ids — one per menu, so egui remembers each open state independently
/// and `Popup::is_id_open` can report that a menu closed itself.
fn add_menu_id() -> egui::Id {
    egui::Id::unique("glitcherz_patch_add_menu")
}

fn cable_menu_id() -> egui::Id {
    egui::Id::unique("glitcherz_patch_cable_menu")
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
/// Mixer box width. Wider than a plain node: the input-port name column gets
/// its own space between the port dot and the fader strip.
const MIXER_WIDTH: f32 = 220.0;
/// Graph-space gap from a port dot to its caption. Same value the `Timeline In`
/// terminal has always used, so every named port reads the same way — `● Tempo`
/// / `● Kick` — instead of a caption floating in a column of its own.
///
/// It used to be `30`, i.e. sized so that at the *minimum* zoom the caption
/// cleared `PORT_HIT_RADIUS` (18 screen px). That paid for the port's grab
/// circle with the spacing of every zoom level, and at 100% it left a 24 px
/// hole between the dot and the first glyph. The caption owns its own rect now
/// (see `port_grab_at`), so the gap is free to be a visual gap again.
const PORT_NAME_GAP: f32 = 10.0;
/// Graph-space width of one port name column: six full-width (CJK) glyphs at
/// 10 pt fit, longer names are cut with an ellipsis.
const PORT_NAME_WIDTH: f32 = 72.0;
/// Max caption length in full-width cells (six CJK glyphs / twelve latin
/// letters); longer names are cut with an ellipsis.
const PORT_NAME_FULLWIDTH: usize = 6;
/// Zoom below which the painted port names (and their right-click editor) are
/// dropped — the glyphs would be too small to read or hit, mirroring the
/// in-node action buttons and inline sliders.
const PORT_NAME_MIN_ZOOM: f32 = ACTION_MIN_ZOOM;
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
/// `Audio In` info area (R4): device chip + per-channel meter, drawn above
/// the inline rows. Two rows, same height as a slider row.
const INPUT_INFO_ROWS: f32 = 2.0;
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
/// Left edge of a channel strip, clear of the port dot, of `PORT_HIT_RADIUS`
/// — a press inside the dot must still start a cable drag — and of the whole
/// port-name column, so the fader never paints over the channel name.
const CHANNEL_STRIP_LEFT: f32 = PORT_NAME_GAP + PORT_NAME_WIDTH + 6.0;
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
/// Timeline In terminal size (U3b/R6): one tempo port plus one recording input
/// per track, same width as the timeline box it mirrors. Height scales with the
/// port count; [`timeline_in_size`] is the single measure.
const TIMELINE_IN_WIDTH: f32 = TIMELINE_WIDTH;
/// Fixed patch height.
const CANVAS_HEIGHT: f32 = 300.0;
/// Output port circle radius / hit radius (generous so port drags are easy to start).
const PORT_RADIUS: f32 = 6.0;
const PORT_HIT_RADIUS: f32 = 18.0;
/// Cable hit distance.
const CABLE_HIT_DIST: f32 = 8.0;
/// Automation cable dash pattern (D27): the only line-style channel, kept
/// separate from stroke width, which encodes interaction state.
const CABLE_DASH_LENGTH: f32 = 7.0;
const CABLE_DASH_GAP: f32 = 5.0;
/// Bezier samples used to render a dashed cable.
const CABLE_DASH_SAMPLES: usize = 32;
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

/// `Timeline In` port count (R6 D21): the tempo port plus one recording input
/// per track, in MIDI → curve → audio order.
fn timeline_in_port_count(midi_count: usize, auto_count: usize, audio_count: usize) -> usize {
    1 + midi_count + auto_count + audio_count
}

/// `Timeline In` terminal size: one row per port below the title band.
fn timeline_in_size(midi_count: usize, auto_count: usize, audio_count: usize) -> Vec2 {
    Vec2::new(
        TIMELINE_IN_WIDTH,
        NODE_TITLE_BAND
            + (TIMELINE_ROW_HEIGHT
                * timeline_in_port_count(midi_count, auto_count, audio_count) as f32)
                .max(TIMELINE_ROW_HEIGHT),
    )
}

/// A `Timeline In` port index → its track family and index (`None` = the
/// tempo port). Thin wrapper over the model helper so layout and wiring share
/// one port table.
fn timeline_in_port_track(port: u8, deps: &PatchDeps) -> Option<(TrackKind, usize)> {
    timeline_in_port_kind(port, deps.track_count, deps.auto_count, deps.audio_count)
}

/// Caption for one recording input on the `Timeline In` terminal: the **live**
/// track name, or its `T1` / `C1` / `A1` tag when the track has no name yet.
///
/// Read through `deps.track_name` on every frame, exactly like the timeline
/// outlet captions — the port names the track it feeds, so a rename in the
/// arrangement shows up here immediately instead of leaving a stale tag.
fn timeline_in_port_label(kind: TrackKind, track: usize, deps: &PatchDeps) -> String {
    let name = (deps.track_name)(kind, track);
    if name.is_empty() {
        let tag = match kind {
            TrackKind::Midi => "T",
            TrackKind::Curve => "C",
            TrackKind::Audio => "A",
        };
        format!("{tag}{}", track + 1)
    } else {
        truncate_fullwidth(&name, PORT_NAME_FULLWIDTH)
    }
}

/// The cable kind one recording input accepts (D24).
fn track_kind_cable(kind: TrackKind) -> CableKind {
    match kind {
        TrackKind::Midi => CableKind::Midi,
        TrackKind::Curve => CableKind::Automation,
        TrackKind::Audio => CableKind::Audio,
    }
}

/// The port index of one recording input, or `None` when it is out of range.
fn track_input_port(kind: TrackKind, track: usize, deps: &PatchDeps) -> Option<u8> {
    timeline_in_port(
        kind,
        track,
        deps.track_count,
        deps.auto_count,
        deps.audio_count,
    )
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
            // U13/v3: the SoundFont and sample file buttons.
            Some(NodeDoc::Instrument {
                voice: VoiceSpec::SoundFont { .. } | VoiceSpec::Sample { .. },
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

/// Node identity colour (spec P5d, D27): stroke carries the node family, fill
/// is a dark base. Strokes stay at ~30% saturation so that fully saturated
/// hues belong to cables and ports alone; the fixed entities keep a slightly
/// higher luminance as canvas anchors. Grey fallbacks are already neutral.
fn node_color(id: PatchId, doc: &GraphDoc) -> (Color32, Color32) {
    let stroke = match id {
        PatchId::Master => Color32::from_rgb(168, 112, 104),
        PatchId::Timeline => Color32::from_rgb(136, 120, 168),
        // The sink shares the timeline's identity colour (same subsystem).
        PatchId::TimelineIn => Color32::from_rgb(120, 106, 148),
        PatchId::Node(id) => match doc.node_by_id(id) {
            Some(NodeDoc::Instrument { .. }) => Color32::from_rgb(91, 124, 156),
            Some(NodeDoc::Effect { .. }) => Color32::from_rgb(154, 124, 92),
            Some(NodeDoc::Mixer { .. }) => Color32::from_rgb(110, 140, 120),
            Some(NodeDoc::Keyboard { .. }) => Color32::from_rgb(94, 136, 128),
            // R4: a live input is an audio source, one shade warmer than the
            // keyboard's routing colour.
            Some(NodeDoc::Input { .. }) => Color32::from_rgb(86, 126, 136),
            Some(NodeDoc::Feedback { .. }) => Color32::from_rgb(146, 102, 122),
            // U6: audio tracks share the sample-player family's warm tone.
            Some(NodeDoc::AudioTrack { .. }) => Color32::from_rgb(140, 124, 92),
            // MIDI FX / control sources carry the MIDI cable's green (spec
            // `midi-nodes.md` §8), one saturation step below it.
            Some(NodeDoc::Arpeggiator { .. }) => Color32::from_rgb(94, 140, 100),
            Some(NodeDoc::Portamento { .. }) => Color32::from_rgb(88, 134, 96),
            Some(NodeDoc::MidiEnvelope { .. }) => Color32::from_rgb(82, 128, 94),
            // A free-running LFO is a control *generator*: same family, one
            // shade warmer than the note-triggered envelope.
            Some(NodeDoc::Lfo { .. }) => Color32::from_rgb(110, 136, 94),
            // A curve map is a control-stage node too, one shade cooler than
            // the sources it sits behind (spec `curve-map.md` D5).
            Some(NodeDoc::CurveMap { .. }) => Color32::from_rgb(88, 126, 116),
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

/// Cable colour by signal type (D27). These three hues are the type contract:
/// a port and the cable that may land on it share one colour. Cool cyan for
/// audio keeps it distinguishable from MIDI green and automation amber under
/// red-green colour deficiency; the automation dash adds a non-hue channel.
fn cable_color(kind: CableKind) -> Color32 {
    match kind {
        CableKind::Audio => Color32::from_rgb(84, 178, 196),
        CableKind::Midi => Color32::from_rgb(90, 200, 110),
        CableKind::Automation => Color32::from_rgb(230, 190, 70),
    }
}

/// Paint one cable. Automation cables are dashed — the only line-style
/// channel, chosen to break the green/amber pair for colour-deficient users
/// without giving every type its own pattern (D27). epaint's
/// `CubicBezierShape` has no dash attribute, so the dashed variant samples the
/// same curve and feeds the polyline to `Shape::dashed_line_many`. Audio and
/// MIDI keep the exact bezier path they had before.
fn paint_cable(painter: &egui::Painter, bez: [Pos2; 4], kind: CableKind, stroke: egui::Stroke) {
    if kind == CableKind::Automation {
        let points = bezier_points(bez, CABLE_DASH_SAMPLES);
        let mut shapes = Vec::new();
        egui::Shape::dashed_line_many(
            &points,
            stroke,
            CABLE_DASH_LENGTH,
            CABLE_DASH_GAP,
            &mut shapes,
        );
        painter.extend(shapes);
    } else {
        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
            bez,
            false,
            Color32::TRANSPARENT,
            stroke,
        ));
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
        PatchId::TimelineIn => {
            timeline_in_size(deps.track_count, deps.auto_count, deps.audio_count)
        }
        PatchId::Node(node_id) => match deps.doc.node_by_id(node_id) {
            // Title + slot action row, then one port per channel. The box is
            // wider than a plain node so each port has a name column.
            Some(NodeDoc::Mixer { inputs, .. }) => Vec2::new(
                MIXER_WIDTH,
                header_band(id, deps) + (*inputs as f32 * PORT_SPACING).max(NODE_HEIGHT_MIN),
            ),
            Some(NodeDoc::Keyboard { .. }) => KEYBOARD_SIZE,
            // R4: the live input box carries the device chip + meter above
            // its inline rows.
            Some(NodeDoc::Input { .. }) => Vec2::new(
                NODE_SIZE.x,
                NODE_SIZE.y + INPUT_INFO_ROWS * INLINE_ROW_HEIGHT,
            ),
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

/// Entity label.
fn label_of(id: PatchId, doc: &GraphDoc) -> String {
    match id {
        PatchId::Master => "Master Out".to_string(),
        PatchId::Timeline => "Timeline".to_string(),
        PatchId::TimelineIn => "Timeline In".to_string(),
        PatchId::Node(node_id) => match doc.node_by_id(node_id) {
            Some(NodeDoc::Input { .. }) => "Audio In".to_string(),
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
                } => {
                    if path.is_empty() {
                        format!("SAMPLE (no file) · Track {}", track + 1)
                    } else {
                        format!(
                            "SAMPLE {}·{} · Track {}",
                            file_stem_of(path),
                            note_name(*root_note),
                            track + 1
                        )
                    }
                }
                VoiceSpec::DrumKit { slots, .. } => {
                    if slots.is_empty() {
                        format!("DRUM (empty) · Track {}", track + 1)
                    } else {
                        format!("DRUM {} slots · Track {}", slots.len(), track + 1)
                    }
                }
                VoiceSpec::Modal { params } => {
                    format!("MALLET · {} · Track {}", params.preset.label(), track + 1)
                }
                VoiceSpec::Pluck { .. } => format!("PLUCK · Track {}", track + 1),
                VoiceSpec::Organ { .. } => format!("ORGAN · Track {}", track + 1),
                VoiceSpec::Clap { plugin_id, .. } => {
                    format!("♪ {}", truncate(plugin_id, 16))
                }
                VoiceSpec::Vst3 { class_id, .. } => format!("♪ {}", truncate(class_id, 16)),
            },
            Some(NodeDoc::Effect {
                voice,
                track,
                params,
                ..
            }) => match voice {
                // The built-in roster has six kinds since U10 — the caption
                // takes the payload's own tag, so an EQ node never reads
                // "Delay".
                VoiceSpec::Builtin
                | VoiceSpec::SoundFont { .. }
                | VoiceSpec::Sample { .. }
                | VoiceSpec::DrumKit { .. }
                | VoiceSpec::Modal { .. }
                | VoiceSpec::Pluck { .. }
                | VoiceSpec::Organ { .. } => {
                    format!("{} · Track {}", params.kind().label(), track + 1)
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
            Some(NodeDoc::Arpeggiator { .. }) => "Arpeggiator".to_string(),
            Some(NodeDoc::Portamento { params, .. }) => {
                format!("Portamento · {:.0} ms", params.time_ms)
            }
            Some(NodeDoc::MidiEnvelope { .. }) => "MIDI Env".to_string(),
            // Spec `lfo.md` §6: waveform + time base + depth, readable off the
            // box without opening the Inspector.
            Some(NodeDoc::Lfo { params, .. }) => {
                let rate = if params.sync {
                    LFO_DIVISION_LABELS
                        [usize::from(params.division).min(LFO_DIVISION_LABELS.len() - 1)]
                    .to_string()
                } else {
                    format!("{:.2} Hz", params.rate_hz)
                };
                format!(
                    "LFO · {} {} · {:.0}%",
                    params.waveform.label(),
                    rate,
                    params.depth * 100.0
                )
            }
            // D5: the node body doubles as a "mapping card" — the range and
            // mode are what the user needs to see without opening anything.
            Some(NodeDoc::CurveMap { params, .. }) => {
                format!("Map {}", curve_map_caption(*params))
            }
            Some(NodeDoc::ChannelStrip { track, .. }) => {
                format!("Strip (legacy) · Track {}", track + 1)
            }
            None => format!("Node {node_id}"),
        },
    }
}

/// `-1..1 · Linear` — the axis a [`NodeDoc::CurveMap`] declares.
///
/// The node body doubles as the "mapping card" of spec `curve-map.md` D5: the
/// two numbers and the mode are what the user needs to read off the patch
/// without opening the Inspector, so the caption is kept as short as the
/// numbers allow (18k rather than 18000.0000).
pub(crate) fn curve_map_caption(params: CurveMapParams) -> String {
    format!(
        "{}..{} · {}",
        axis_number(params.min),
        axis_number(params.max),
        params.mode.label()
    )
}

pub(crate) fn axis_number(value: f32) -> String {
    if !value.is_finite() {
        return "?".to_string();
    }
    if value.abs() >= 10_000.0 {
        format!("{:.0}k", value / 1000.0)
    } else if value.abs() >= 100.0 || value.fract().abs() < 0.001 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
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
        // The curve map's pass-through port carries the signal, not a value
        // the user picked (spec `curve-map.md` D5).
        AutoParam::CurveMap => "Curve In".to_string(),
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
            NodeParam::PitchOffset => "Pitch".to_string(),
            NodeParam::Resonance => "Resonance".to_string(),
            NodeParam::Attack => "Attack".to_string(),
            NodeParam::Decay => "Decay".to_string(),
            NodeParam::Sustain => "Sustain".to_string(),
            NodeParam::Release => "Release".to_string(),
            NodeParam::ModalDecay => "Decay".to_string(),
            NodeParam::ModalBeating => "Beating".to_string(),
            NodeParam::ModalHardness => "Hardness".to_string(),
            NodeParam::ModalBrightness => "Brightness".to_string(),
            NodeParam::ModalGain => "Gain".to_string(),
            NodeParam::PluckDecay => "Decay".to_string(),
            NodeParam::PluckDamping => "Damping".to_string(),
            NodeParam::PluckBrightness => "Brightness".to_string(),
            NodeParam::PluckRelease => "Release".to_string(),
            NodeParam::PluckGain => "Gain".to_string(),
            NodeParam::OrganBar(index) => {
                let footage = crate::ORGAN_FOOTAGES
                    .get(*index as usize)
                    .copied()
                    .unwrap_or("?");
                format!("{footage} Bar")
            }
            NodeParam::OrganPercussion => "Percussion".to_string(),
            NodeParam::OrganPercussionDecay => "Perc Decay".to_string(),
            NodeParam::OrganAttack => "Attack".to_string(),
            NodeParam::OrganRelease => "Release".to_string(),
            NodeParam::OrganGain => "Gain".to_string(),
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

/// Truncate a user-facing name to `max_fullwidth` full-width cells: ASCII
/// counts as one cell, non-ASCII (CJK, …) as two. A cut name ends with an
/// ellipsis. Six cells = six Chinese glyphs / twelve latin letters, which is
/// what the port-name column is sized for — a long track name must never
/// widen (or overflow) a node box.
fn truncate_fullwidth(s: &str, max_fullwidth: usize) -> String {
    let budget = max_fullwidth * 2;
    let mut width = 0usize;
    let mut out = String::new();
    let mut cut = false;
    for ch in s.chars() {
        let cells = if ch.is_ascii() { 1 } else { 2 };
        if width + cells > budget {
            cut = true;
            break;
        }
        width += cells;
        out.push(ch);
    }
    if cut {
        out.push('…');
    }
    out
}

/// The track whose audio chain lands on one mixer channel, derived from the
/// cable that terminates there. The last node of that chain owns the track
/// (`NodeDoc::track()`), so this is the inverse of the app's forward
/// `track_channel_target` walk without needing the project.
fn mixer_feeding_track(doc: &GraphDoc, mixer: u64, channel: u8) -> Option<(TrackKind, usize)> {
    let cable = doc.cables.iter().find(|c| {
        c.kind == CableKind::Audio && c.to == PortRef::Node(mixer) && c.in_port == channel
    })?;
    let PortRef::Node(id) = &cable.from else {
        return None;
    };
    let node = doc.node_by_id(*id)?;
    if let NodeDoc::AudioTrack { track, .. } = node {
        Some((TrackKind::Audio, *track))
    } else {
        Some((TrackKind::Midi, node.track()))
    }
}

/// Captions for the two kinds of named ports: every timeline outlet (the
/// track's live name) and every mixer inlet (the channel's custom name, or
/// the feeding track's name, or `Ch N`). Computed once per frame so the
/// right-click hit test and the painting agree on exactly the same rects.
///
/// Nothing here allocates a widget: the canvas keeps its own click/drag
/// sense, so selecting or dragging a node by its title still works.
fn port_name_labels(
    rect_min: Pos2,
    state: &PatchState,
    deps: &PatchDeps<'_>,
) -> Vec<PortNameLabel> {
    let z = state.zoom;
    if z < PORT_NAME_MIN_ZOOM {
        return Vec::new();
    }
    let name_w = PORT_NAME_WIDTH * z;
    let gap = PORT_NAME_GAP * z;
    let h = 18.0 * z;
    let mut out = Vec::new();

    // Timeline outlets: the caption sits to the left of the dot, inside the
    // box — the same place the old `T1` / `C1` / `A1` tags lived.
    for (port, p) in state.out_ports_screen(rect_min, PatchId::Timeline, deps) {
        let (kind, track, tag) = match timeline_port_kind(port, deps.track_count, deps.auto_count) {
            TimelinePort::Midi(t) => (TrackKind::Midi, t, format!("T{}", t + 1)),
            TimelinePort::Auto(i) => (TrackKind::Curve, i, format!("C{}", i + 1)),
            TimelinePort::Audio(i) => (TrackKind::Audio, i, format!("A{}", i + 1)),
        };
        let raw = (deps.track_name)(kind, track);
        let full = if raw.is_empty() {
            tag.clone()
        } else {
            raw.clone()
        };
        out.push(PortNameLabel {
            target: PortNameTarget::Track { kind, track },
            rect: Rect::from_min_size(
                Pos2::new(p.x - gap - name_w, p.y - h * 0.5),
                Vec2::new(name_w, h),
            ),
            display: truncate_fullwidth(&full, PORT_NAME_FULLWIDTH),
            full,
            raw,
            hint: tag,
        });
    }

    // Mixer inlets: the caption sits to the right of the dot, in the column
    // the wider mixer box reserves in front of the fader strip.
    for &id in state.positions.keys() {
        let PatchId::Node(node_id) = id else {
            continue;
        };
        let Some(NodeDoc::Mixer { channels, .. }) = deps.doc.node_by_id(node_id) else {
            continue;
        };
        for (port, p) in state.in_ports_screen(rect_min, id, deps) {
            let Some(ch) = channels.get(port as usize) else {
                continue;
            };
            let fallback = format!("Ch {}", port + 1);
            let track_name = mixer_feeding_track(deps.doc, node_id, port)
                .map(|(kind, track)| (deps.track_name)(kind, track))
                .filter(|name| !name.is_empty());
            let full = if !ch.name.is_empty() {
                ch.name.clone()
            } else {
                track_name.unwrap_or_else(|| fallback.clone())
            };
            out.push(PortNameLabel {
                target: PortNameTarget::Channel {
                    node: node_id,
                    channel: port,
                },
                rect: Rect::from_min_size(
                    Pos2::new(p.x + gap, p.y - h * 0.5),
                    Vec2::new(name_w, h),
                ),
                display: truncate_fullwidth(&full, PORT_NAME_FULLWIDTH),
                full,
                raw: ch.name.clone(),
                hint: fallback,
            });
        }
    }
    out
}

/// Destination input port for an audio cable drop (Z0 Z-D8).
///
/// Mixer and dual-input nodes expose several ports: a precise hit on a dot
/// wins, otherwise the first free port. Every other node lands on port 0.
fn audio_landing_port(
    doc: &GraphDoc,
    state: &PatchState,
    rect_min: Pos2,
    sp: Pos2,
    target: PatchId,
    deps: &PatchDeps<'_>,
) -> Option<u8> {
    let PatchId::Node(to_doc) = target else {
        return Some(0);
    };
    let Some(node) = doc.node_by_id(to_doc) else {
        return Some(0);
    };
    let ports = node.audio_inputs();
    if ports <= 1 {
        return Some(0);
    }
    if let Some((PatchId::Node(_), port)) = state.hit_in_port(rect_min, sp, deps)
        && port < ports
    {
        return Some(port);
    }
    if node.is_mixer() {
        doc.first_free_channel(to_doc)
    } else {
        doc.first_free_port(to_doc)
    }
}

/// Which output port a press at `sp` may grab, if any.
///
/// Captions own their own rect: a press that lands on a name belongs to the
/// node (select / drag), never to the dot the name hugs. The captions sit
/// `PORT_NAME_GAP` (10 pt) from the dot, i.e. **inside** the port's 18 pt grab
/// circle, so without this rule dragging a node by its channel name would rip
/// a cable out of the neighbour port. The dot itself — and the whole ring
/// around it, everywhere a caption is not painted — still grabs as before.
fn port_grab_at(
    labels: &[PortNameLabel],
    state: &PatchState,
    rect_min: Pos2,
    sp: Pos2,
    deps: &PatchDeps<'_>,
) -> Option<(PatchId, usize)> {
    if labels.iter().any(|l| l.rect.contains(sp)) {
        return None;
    }
    state.hit_out_port(rect_min, sp, deps)
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

    /// Whether anything at all is selected — including the special timeline /
    /// master entries that [`Self::selected_doc_nodes`] filters out.
    ///
    /// The Inspector needs this to tell "nothing selected" (fall back to the
    /// track's instrument) from "the timeline is selected" (that node owns the
    /// panel, so the instrument parameters must stay hidden).
    pub fn has_any_selection(&self) -> bool {
        !self.selection.nodes.is_empty() || !self.selection.cables.is_empty()
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
            PatchId::Node(node_id) => {
                // Spec `midi-nodes.md` §3: a MIDI effect exposes a MIDI out;
                // Portamento adds a second (control) port below it.
                if deps
                    .doc
                    .node_by_id(node_id)
                    .is_some_and(|node| matches!(node, NodeDoc::Portamento { .. }))
                {
                    vec![
                        (0, Pos2::new(r.right(), r.center().y - 8.0)),
                        (1, Pos2::new(r.right(), r.center().y + 8.0)),
                    ]
                } else {
                    vec![(0, Pos2::new(r.right(), r.center().y))]
                }
            }
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
            // R6 port table: tempo (0) plus one recording input per track.
            PatchId::TimelineIn => {
                timeline_in_port_count(deps.track_count, deps.auto_count, deps.audio_count)
            }
            PatchId::Master => 1,
            PatchId::Node(node_id) => match deps.doc.node_by_id(node_id) {
                Some(node) if node.is_mixer() => {
                    if let NodeDoc::Mixer { inputs, .. } = node {
                        *inputs as usize
                    } else {
                        1
                    }
                }
                // G6: the virtual keyboard has no audio input; the LFO is a pure
                // control source with no input port at all (spec `lfo.md` D1).
                Some(node)
                    if node.is_keyboard()
                        || matches!(node, NodeDoc::Lfo { .. } | NodeDoc::Input { .. }) =>
                {
                    0
                }
                // Z0: dual-input effects expose Main (0) and Mod (1).
                Some(node) if node.audio_inputs() == 2 => 2,
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
                // Wider box (port-name column): start a little further left so
                // its right edge still clears the master column at x = 930.
                NodeDoc::Mixer { .. } => (PatchId::Node(node.node_id()), Pos2::new(680.0, y)),
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
                // R4: the live input is a source node like the keyboard.
                NodeDoc::Input { .. } => {
                    let k = keyboard_slot;
                    keyboard_slot += 1;
                    (
                        PatchId::Node(node.node_id()),
                        Pos2::new(16.0, keyboard_top + k as f32 * 68.0),
                    )
                }
                // MIDI FX / control sources stack in the same left column
                // (chain heads, spec `midi-nodes.md`; the menu places new ones
                // at the click position — this is the fallback).
                NodeDoc::Arpeggiator { .. }
                | NodeDoc::Portamento { .. }
                | NodeDoc::MidiEnvelope { .. }
                | NodeDoc::Lfo { .. }
                | NodeDoc::CurveMap { .. } => {
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
        let (to, to_port) = match &cable.to {
            // R6: a recording input is a port on the fixed terminal; its
            // index is derived from the track identity, so it stays correct
            // when tracks are added or removed.
            PortRef::TrackIn { kind, track } => {
                (PatchId::TimelineIn, track_input_port(*kind, *track, deps)?)
            }
            PortRef::TimelineIn => (PatchId::TimelineIn, cable.in_port),
            _ => (patch_port_to(&cable.to)?, cable.in_port),
        };
        let p0 = self
            .out_ports_screen(rect_min, from, deps)
            .into_iter()
            .find(|(k, _)| *k == from_port)?
            .1;
        let p3 = self
            .in_ports_screen(rect_min, to, deps)
            .into_iter()
            .find(|(k, _)| *k == to_port)?
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
            self.pending_cable_menu = Some(CableMenu {
                cable: i,
                at: graph_at,
                pos: sp + Vec2::new(2.0, 2.0),
                fresh: true,
            });
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
        // R6: a recording input is a sink, never a source.
        PortRef::TrackIn { .. } => None,
    }
}

/// `PortRef` → cable target entity.
fn patch_port_to(port: &PortRef) -> Option<PatchId> {
    match port {
        PortRef::Node(id) => Some(PatchId::Node(*id)),
        PortRef::Master => Some(PatchId::Master),
        PortRef::TimelineIn => Some(PatchId::TimelineIn),
        // R6: recording inputs live on the fixed terminal.
        PortRef::TrackIn { .. } => Some(PatchId::TimelineIn),
        _ => None,
    }
}

/// Where a pending automation / control cable originates (spec
/// `midi-nodes.md` §7: timeline tracks and control-output nodes share the
/// same destination picker).
#[derive(Clone, Copy, Debug)]
enum AutoSource {
    /// Timeline automation track index.
    Track(usize),
    /// Control-output node (Portamento / MidiEnvelope).
    Node(u64),
}

/// Cable kind carried by one node's output port (spec `midi-nodes.md` §3).
///
/// Port 0 is the MIDI out for MIDI effects / the keyboard; Portamento's
/// port 1 is its control output. Every other node keeps the audio port — the
/// Curve Map has a single port and it is a control port (spec
/// `curve-map.md` D5), so it is `Automation` on port 0.
fn node_out_port_kind(node: &NodeDoc, port: usize) -> CableKind {
    match node {
        NodeDoc::Keyboard { .. } | NodeDoc::Arpeggiator { .. } => CableKind::Midi,
        NodeDoc::Portamento { .. } if port == 0 => CableKind::Midi,
        NodeDoc::Portamento { .. } => CableKind::Automation,
        NodeDoc::MidiEnvelope { .. } | NodeDoc::Lfo { .. } | NodeDoc::CurveMap { .. } => {
            CableKind::Automation
        }
        _ => CableKind::Audio,
    }
}

/// Structural signal kind of a node's input ports (D27).
///
/// Automation has no physical input port: a cable is addressed to
/// `(node_id, param)` and only reuses the structural inlet as a layout
/// landing spot. Input rings therefore show the signal the node consumes,
/// not the cables currently attached to it — a yellow dashed automation line
/// may legitimately end on a cyan or green ring. The cable's own dash and
/// midpoint parameter label carry that distinction.
fn node_in_port_kind(node: &NodeDoc) -> CableKind {
    match node {
        NodeDoc::Instrument { .. }
        | NodeDoc::Arpeggiator { .. }
        | NodeDoc::Portamento { .. }
        | NodeDoc::MidiEnvelope { .. } => CableKind::Midi,
        // D5 (`curve-map.md`): the map node's only inlet is the control signal.
        NodeDoc::CurveMap { .. } => CableKind::Automation,
        _ => CableKind::Audio,
    }
}

/// The one parameter a node's single control port carries, when offering a
/// menu would be noise (spec `curve-map.md` D5: the Curve Map has exactly one
/// input and it is the signal itself).
fn single_control_port(doc: &GraphDoc, node_id: u64) -> Option<AutoParam> {
    match doc.node_by_id(node_id)? {
        NodeDoc::CurveMap { .. } => Some(AutoParam::CurveMap),
        _ => None,
    }
}

/// The track a dormant direct cable is superseded by (R6 D33), if any.
///
/// A keyboard node wired into a MIDI recording input dispatches through that
/// track instead of its direct instrument cables; the direct ones stay in the
/// document and render faded. Pure derivation from the cable table — there is
/// no stored mute state to restore.
fn dormant_cable_track(doc: &GraphDoc, cable: &CableDoc) -> Option<usize> {
    if cable.kind != CableKind::Midi || !matches!(cable.to, PortRef::Node(_)) {
        return None;
    }
    let PortRef::Node(from) = cable.from else {
        return None;
    };
    if !doc.node_by_id(from).is_some_and(NodeDoc::is_keyboard) {
        return None;
    }
    doc.node_track_inputs(from)
        .into_iter()
        .find_map(|(kind, track)| (kind == TrackKind::Midi).then_some(track))
}

/// The document port a cable drag starts from (R6): a node's output, a
/// timeline MIDI/curve outlet, or an audio track's folded node.
fn drag_source_port(drag: &CableDrag, doc: &GraphDoc, deps: &PatchDeps) -> Option<PortRef> {
    match drag.from {
        PatchId::Node(id) => Some(PortRef::Node(id)),
        PatchId::Timeline => match timeline_port_kind(drag.port, deps.track_count, deps.auto_count)
        {
            TimelinePort::Midi(track) => Some(PortRef::MidiTrack(track)),
            TimelinePort::Auto(index) => Some(PortRef::AutoTrack(index)),
            TimelinePort::Audio(track) => audio_node_id(doc, track).map(PortRef::Node),
        },
        PatchId::Master | PatchId::TimelineIn => None,
    }
}

/// Cable kind carried by a drag's source port (the same rule `accepts` and the
/// drop branches use).
fn drag_cable_kind(drag: &CableDrag, doc: &GraphDoc, deps: &PatchDeps) -> CableKind {
    match drag.from {
        PatchId::Timeline => match timeline_port_kind(drag.port, deps.track_count, deps.auto_count)
        {
            TimelinePort::Midi(_) => CableKind::Midi,
            TimelinePort::Auto(_) => CableKind::Automation,
            TimelinePort::Audio(_) => CableKind::Audio,
        },
        PatchId::Node(node_id) => doc
            .node_by_id(node_id)
            .map_or(CableKind::Audio, |node| node_out_port_kind(node, drag.port)),
        PatchId::Master => CableKind::Audio,
        // Unreachable (no output ports); automation keeps the terminal's
        // historical port table.
        PatchId::TimelineIn => CableKind::Automation,
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
            CableKind::Midi => matches!(
                node,
                NodeDoc::Instrument { .. }
                    | NodeDoc::Arpeggiator { .. }
                    | NodeDoc::Portamento { .. }
                    | NodeDoc::MidiEnvelope { .. }
            ),
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
                    // D5: the map node's only input is a control input.
                    | NodeDoc::CurveMap { .. }
            ),
        }
    } else {
        match target {
            PatchId::Master => kind == CableKind::Audio,
            // R6: the terminal hosts a tempo port plus per-track recording inputs;
            // which one a cable may land on is decided by the port it is
            // dropped on (`timeline_in_port_track`), so the box itself accepts
            // every cable type.
            PatchId::TimelineIn => true,
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
        ui.weak("Shift+A / right-click to add node · right-click a port name to rename · Ctrl+right-drag to cut cables · click a cable dot to insert/delete · click to select · drag bg to box-select · middle-drag to pan · scroll to zoom · drag ports to connect · Delete to remove");
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

    // ---- Named-port captions (timeline outlets / mixer inlets) ----
    // One snapshot per frame, taken *before* the press is routed: the same
    // rects drive the cable-grab veto below, the right-click rename and the
    // painting, so a caption can never be clicked where it is not drawn (or
    // vice versa). The captions are *painted*, not widgets, so the canvas
    // keeps its own click / drag / marquee behaviour — pressing a title still
    // selects and drags its node.
    let name_labels = port_name_labels(rect.min, state, &deps);

    // ---- Press: hit-test and enter the matching drag ----
    // egui 0.36 (click+drag sense) only starts a drag after the pointer moves
    // decisively; in-place presses are handled by the `clicked` path below.
    if drag_started {
        let sp = pointer.unwrap_or(rect.min);
        if let Some((source, port)) = port_grab_at(&name_labels, state, rect.min, sp, &deps) {
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

    // The node / channel the editor pointed at may have been removed (or the
    // view zoomed below the caption threshold) — drop a stale editor.
    let stale_rename = state
        .rename
        .as_ref()
        .is_some_and(|r| !name_labels.iter().any(|l| l.target == r.target));
    if stale_rename {
        state.rename = None;
    }
    if let Some(sp) = ui.ctx().pointer_hover_pos()
        && let Some(label) = name_labels.iter().find(|l| l.rect.contains(sp))
    {
        let _ = response
            .clone()
            .on_hover_text(format!("{}\nRight-click to rename", label.full));
    }

    // ---- Add-node menu: right-click, or `Shift+A` (spec D17) ----
    // Right-click anywhere on the canvas; with Ctrl held the right button
    // belongs to the cable-cut interaction instead (Blender-style knife), so
    // the menu is suppressed. `Shift+A` opens the same popup at the cursor,
    // which is why the menu is a `Popup` with an explicit open command rather
    // than `Response::context_menu` (that one only opens on a click).
    // A right-click ON a port caption belongs to the inline rename instead.
    let shift_a = !ctrl
        && !ui.ctx().egui_wants_keyboard_input()
        && ui
            .ctx()
            .input(|i| i.modifiers.shift && !i.modifiers.command && i.key_pressed(egui::Key::A));
    // While an editor is open, the right-click belongs to "click elsewhere
    // to commit" — do not also open a menu / start a second rename.
    let right_click = response.secondary_clicked() && !ctrl && state.rename.is_none();
    let rename_at = if right_click {
        pointer.and_then(|sp| name_labels.iter().find(|l| l.rect.contains(sp)))
    } else {
        None
    };
    if let Some(label) = rename_at {
        state.rename = Some(PortRename {
            target: label.target,
            draft: label.raw.clone(),
            needs_focus: true,
        });
    }
    let open_at = if right_click && rename_at.is_none() {
        pointer
    } else if shift_a {
        Some(
            ui.ctx()
                .pointer_latest_pos()
                .filter(|p| rect.contains(*p))
                .unwrap_or_else(|| rect.center()),
        )
    } else {
        None
    };
    if let Some(sp) = open_at {
        state.add_menu = Some(AddMenu {
            at: Pos2::new(
                state.pan.x + (sp.x - rect.min.x) / state.zoom,
                state.pan.y + (sp.y - rect.min.y) / state.zoom,
            ),
            pos: sp + Vec2::new(2.0, 2.0),
            fresh: true,
        });
    }

    // One flat level (spec D16): every leaf either creates a node directly or
    // opens the picker; egui owns the open/close lifecycle (Escape / click
    // elsewhere / pick), and `add_menu` only carries the placement.
    #[derive(Clone)]
    enum AddPick {
        Add(NodeSource),
        Picker(Option<NodeCategory>),
        Rescan,
    }
    if let Some(menu) = state.add_menu.as_mut() {
        let open_cmd = if std::mem::take(&mut menu.fresh) {
            Some(egui::SetOpenCommand::Bool(true))
        } else {
            None
        };
        let pick = egui::Popup::from_response(&response)
            .id(add_menu_id())
            .kind(egui::PopupKind::Menu)
            .layout(egui::Layout::top_down_justified(egui::Align::Min))
            .at_position(menu.pos)
            .open_memory(open_cmd)
            .show(|ui| {
                ui.set_min_width(180.0);
                ui.strong("Add Node");
                ui.separator();
                // D5: cross-category jump — the entry's own classification
                // decides whether an instrument or an effect node is created.
                if ui.button("Search Nodes…").clicked() {
                    return Some(AddPick::Picker(None));
                }
                // D16: the category leaves ARE the picker (search + list +
                // built-in/CLAP/VST3 tags) — no second level to click through.
                if ui.button("Instrument…").clicked() {
                    return Some(AddPick::Picker(Some(NodeCategory::Instrument)));
                }
                if ui.button("Effect…").clicked() {
                    return Some(AddPick::Picker(Some(NodeCategory::Effect)));
                }
                if ui.button("MIDI FX…").clicked() {
                    return Some(AddPick::Picker(Some(NodeCategory::MidiFx)));
                }
                ui.separator();
                // No source dimension: one level is enough.
                if ui.button("Mixer").clicked() {
                    return Some(AddPick::Add(NodeSource::Mixer));
                }
                if ui.button("Keyboard In (computer)").clicked() {
                    return Some(AddPick::Add(NodeSource::Keyboard));
                }
                if ui
                    .button("Audio Input (microphone)")
                    .on_hover_text("Live input node: 1 audio out, no connections by default")
                    .clicked()
                {
                    return Some(AddPick::Add(NodeSource::AudioInput));
                }
                if ui.button("Feedback Tap").clicked() {
                    return Some(AddPick::Add(NodeSource::Feedback));
                }
                ui.separator();

                ui.separator();
                // U9 E2: full plugin re-scan on demand (the startup scan is
                // cached, so this is the escape hatch after installing or
                // removing plugins while the app is closed).
                if ui.button("Rescan Plug-ins").clicked() {
                    return Some(AddPick::Rescan);
                }
                None
            })
            .and_then(|inner| inner.inner);
        if let Some(pick) = pick {
            match pick {
                AddPick::Add(source) => actions.push(PatchAction::AddNode {
                    source,
                    at: menu.at,
                }),
                AddPick::Picker(category) => actions.push(PatchAction::OpenPluginPicker {
                    category,
                    place: PickerPlace::Add { at: menu.at },
                }),
                AddPick::Rescan => actions.push(PatchAction::RescanPlugins),
            }
        }
        // A closed popup (Escape / click outside / pick handled above) clears
        // the placement so the next right-click starts clean.
        if !egui::Popup::is_id_open(ui.ctx(), add_menu_id()) {
            state.add_menu = None;
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
                    if target == PatchId::TimelineIn {
                        // R6 D24: a recording input accepts one source whose data
                        // kind matches the track family. The drop has to land on a
                        // specific port — dropping anywhere on the box would make
                        // "which track did I mean" ambiguous.
                        if let (Some(from), Some((PatchId::TimelineIn, port))) = (
                            drag_source_port(&drag, doc, &deps),
                            state.hit_in_port(rect.min, sp, &deps),
                        ) && let Some((kind, track)) = timeline_in_port_track(port, &deps)
                            && drag_cable_kind(&drag, doc, &deps) == track_kind_cable(kind)
                        {
                            actions.push(PatchAction::ConnectTrackIn { from, kind, track });
                        }
                    } else {
                        match drag.from {
                            // Timeline port: MIDI tracks, then control tracks, then
                            // audio tracks (mapping lives in `timeline_port_kind`).
                            PatchId::Timeline => {
                                match timeline_port_kind(drag.port, midi_count, auto_count) {
                                    TimelinePort::Midi(track) => {
                                        if accepts(CableKind::Midi, target, doc)
                                            && let PatchId::Node(to_doc) = target
                                        {
                                            actions
                                                .push(PatchAction::ConnectMidi { track, to_doc });
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
                                                // A node with exactly one control
                                                // port (the Curve Map) needs no
                                                // menu: the pick is unambiguous
                                                // (spec `curve-map.md` D5).
                                                if let Some(param) =
                                                    single_control_port(doc, to_doc)
                                                {
                                                    actions.push(PatchAction::ConnectAuto {
                                                        index,
                                                        to_doc,
                                                        param,
                                                    });
                                                } else {
                                                    // The cable is committed after
                                                    // the parameter is picked in the
                                                    // popup menu.
                                                    state.pending_auto = Some((
                                                        AutoSource::Track(index),
                                                        to_doc,
                                                        sp + Vec2::new(4.0, 4.0),
                                                    ));
                                                }
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
                                            let in_port = audio_landing_port(
                                                doc, state, rect.min, sp, target, &deps,
                                            );
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
                                // The output port itself decides the cable kind
                                // (spec `midi-nodes.md` §3): MIDI out → MIDI
                                // cable, control out → automation, else audio.
                                let kind =
                                    doc.node_by_id(from_doc).map_or(CableKind::Audio, |node| {
                                        node_out_port_kind(node, drag.port)
                                    });
                                match kind {
                                    CableKind::Midi => {
                                        if accepts(CableKind::Midi, target, doc)
                                            && drag.from != target
                                            && let PatchId::Node(to_doc) = target
                                        {
                                            if doc
                                                .node_by_id(from_doc)
                                                .is_some_and(NodeDoc::is_keyboard)
                                            {
                                                actions.push(PatchAction::ConnectKeyboardMidi {
                                                    from_doc,
                                                    to_doc,
                                                });
                                            } else {
                                                actions.push(PatchAction::ConnectEventMidi {
                                                    from_doc,
                                                    to_doc,
                                                });
                                            }
                                        }
                                    }
                                    CableKind::Automation => {
                                        if accepts(CableKind::Automation, target, doc)
                                            && drag.from != target
                                            && let PatchId::Node(to_doc) = target
                                        {
                                            if let Some(param) = single_control_port(doc, to_doc) {
                                                // One port, one answer (Curve Map).
                                                actions.push(PatchAction::ConnectControl {
                                                    from_doc,
                                                    to_doc,
                                                    param,
                                                });
                                            } else {
                                                state.pending_auto = Some((
                                                    AutoSource::Node(from_doc),
                                                    to_doc,
                                                    sp + Vec2::new(4.0, 4.0),
                                                ));
                                            }
                                        }
                                    }
                                    CableKind::Audio => {
                                        if accepts(CableKind::Audio, target, doc)
                                            && drag.from != target
                                        {
                                            let to = match target {
                                                PatchId::Node(id) => PortRef::Node(id),
                                                PatchId::Master => PortRef::Master,
                                                _ => PortRef::Master,
                                            };
                                            // D17: the cable binds to the target's
                                            // input port. Dropping on a mixer picks
                                            // the exact channel port when hit
                                            // precisely, else the first free
                                            // channel; dropping anywhere else is
                                            // port 0. Slots are strictly single-
                                            // input (D14): the new cable replaces
                                            // the old one on the same slot; no free
                                            // channel → reject.
                                            let in_port = audio_landing_port(
                                                doc, state, rect.min, sp, target, &deps,
                                            );
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
                            PatchId::Master => {}
                            // A pure sink has no output port, so no drag can start
                            // here (U3b).
                            PatchId::TimelineIn => {}
                        }
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
    // R6 D33: a keyboard node wired into a track input takes over its own
    // output, so its direct MIDI cables no longer dispatch. They stay in the
    // document (nothing is deleted automatically) but render dormant.
    for (i, cable) in doc.cables.iter().enumerate() {
        let Some(bez) = state.cable_controls(rect.min, cable, &deps) else {
            continue;
        };
        let dormant = dormant_cable_track(doc, cable);
        let selected = state.selection.cables.contains(&i);
        let color = if selected {
            Color32::WHITE
        } else {
            cable_color(cable.kind).gamma_multiply(if dormant.is_some() { 0.3 } else { 1.0 })
        };
        let stroke = egui::Stroke::new(
            if selected {
                3.5
            } else if dormant.is_some() {
                1.2
            } else {
                2.0
            },
            color,
        );
        paint_cable(&painter, bez, cable.kind, stroke);
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
        painter.circle_filled(
            mid,
            r,
            color.gamma_multiply(if dormant.is_some() { 0.6 } else { 1.0 }),
        );
        painter.circle_stroke(mid, r, egui::Stroke::new(1.0, Color32::BLACK));
        if let Some(track) = dormant
            && hovered
        {
            painter.text(
                mid + Vec2::new(0.0, 14.0),
                egui::Align2::CENTER_TOP,
                format!(
                    "Device is routed into Track {}; direct cables are dormant",
                    track + 1
                ),
                FontId::proportional(10.0),
                Color32::from_rgb(220, 190, 120),
            );
        }
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
            PatchId::Node(node_id) => doc
                .node_by_id(node_id)
                .map_or(CableKind::Audio, |node| node_out_port_kind(node, drag.port)),
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
                paint_cable(
                    &painter,
                    bezier_controls(p0, sp),
                    kind,
                    egui::Stroke::new(2.0, cable_color(kind).gamma_multiply(0.7)),
                );
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
                PatchId::Node(node_id) => doc
                    .node_by_id(node_id)
                    .map_or(cable_color(CableKind::Audio), |node| {
                        cable_color(node_out_port_kind(node, port))
                    }),
                _ => cable_color(CableKind::Audio),
            };
            painter.circle_filled(p, PORT_RADIUS, port_color);
            painter.circle_stroke(p, PORT_RADIUS, egui::Stroke::new(1.0, Color32::BLACK));
            // Timeline outlet captions (the live track names) are painted in
            // the port-name section below, together with the mixer captions —
            // the old static `T1` / `C1` / `A1` tags went stale on a rename.
        }
        // Input ports: hollow rings hue-coded by the structural signal they
        // accept (D27). Outputs are solid discs; position already encodes
        // direction, so brightness no longer doubles as an in/out cue.
        for (port, p) in state.in_ports_screen(rect.min, id, &deps) {
            let (inbound, label) = match id {
                PatchId::TimelineIn => match timeline_in_port_track(port, &deps) {
                    None => (cable_color(CableKind::Automation), "Tempo".to_string()),
                    Some((kind, track)) => (
                        cable_color(track_kind_cable(kind)),
                        timeline_in_port_label(kind, track, &deps),
                    ),
                },
                // Unreachable (the timeline is a pure source, no input ports).
                PatchId::Timeline | PatchId::Master => {
                    (cable_color(CableKind::Audio), String::new())
                }
                PatchId::Node(node_id) => {
                    let node = deps.doc.node_by_id(node_id);
                    let color = cable_color(node.map_or(CableKind::Audio, node_in_port_kind));
                    // Z0: label the two ports of a dual-input effect so the
                    // Mod inlet is not a second unmarked dot.
                    let label = if node.is_some_and(|n| n.audio_inputs() == 2) {
                        if port == 0 { "Main" } else { "Mod" }.to_string()
                    } else {
                        String::new()
                    };
                    (color, label)
                }
            };
            // The hole is the entity's own fill, so the ring reads as a socket
            // cut into the box rather than a dot sitting on top of it.
            painter.circle_filled(p, PORT_RADIUS, fill);
            painter.circle_stroke(p, PORT_RADIUS, egui::Stroke::new(1.8, inbound));
            painter.circle_stroke(p, PORT_RADIUS + 0.9, egui::Stroke::new(0.8, Color32::BLACK));
            if !label.is_empty() {
                // Same gap and size as the captions in `port_name_labels` —
                // this terminal is the one that made the other two look
                // detached, so it is the reference, not an exception.
                painter.text(
                    Pos2::new(p.x + PORT_NAME_GAP * state.zoom, p.y),
                    egui::Align2::LEFT_CENTER,
                    label,
                    FontId::proportional(10.0 * state.zoom),
                    Color32::from_gray(170),
                );
            }
        }
    }

    // ---- Live input node: device chip + per-channel meter (R4) ----
    // One input stream feeds every audio take, so every `Audio In` node shows
    // the same device; the meter is that node's own channel.
    for &id in state.positions.keys() {
        let PatchId::Node(doc_id) = id else {
            continue;
        };
        let Some(NodeDoc::Input { channel, .. }) = doc.node_by_id(doc_id) else {
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
        let row_h = INLINE_ROW_HEIGHT * z;
        let inline_h = inline_row_count(id, &deps) as f32 * row_h;
        let info_top = r.bottom() - inline_h - INPUT_INFO_ROWS * row_h;
        let inset = INLINE_INSET * z;

        let chip = Rect::from_min_max(
            Pos2::new(r.left() + inset, info_top + 1.0 * z),
            Pos2::new(r.right() - inset, info_top + row_h - 1.0 * z),
        );
        painter.rect_filled(chip, 2.0 * z, Color32::from_gray(46));
        painter.rect_stroke(
            chip,
            2.0 * z,
            Stroke::new(1.0, Color32::from_gray(88)),
            egui::StrokeKind::Inside,
        );
        let caption = match deps.input.device {
            Some(name) if deps.input.loopback => format!("↩ {}", truncate(name, 13)),
            Some(name) => truncate(name, 14),
            None => "no input".to_string(),
        };
        painter.text(
            Pos2::new(chip.left() + 4.0 * z, chip.center().y),
            egui::Align2::LEFT_CENTER,
            &caption,
            FontId::proportional(9.0 * z),
            Color32::from_gray(if deps.input.open { 220 } else { 140 }),
        );
        let resp = ui
            .allocate_rect(chip, Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let tip = match deps.input.error {
            Some(err) => format!("Recording source (one stream for every take)\n{err}"),
            None => "Recording source (one stream for every take)".to_string(),
        };
        let resp = resp.on_hover_text(tip);
        if resp.clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            state.pending_input_menu = Some((doc_id, pos + Vec2::new(4.0, 4.0), u64::MAX));
        }

        let meter = Rect::from_min_max(
            Pos2::new(r.left() + inset, info_top + row_h + 1.0 * z),
            Pos2::new(r.right() - inset, info_top + 2.0 * row_h - 1.0 * z),
        );
        let peak = deps.input.peak_of(*channel).clamp(0.0, 1.0);
        painter.rect_filled(meter, 2.0 * z, Color32::from_gray(28));
        let fill = Rect::from_min_max(
            meter.min,
            Pos2::new(meter.left() + meter.width() * peak, meter.bottom()),
        );
        let bar = if peak > 0.95 {
            Color32::from_rgb(220, 80, 70)
        } else if peak > 0.7 {
            Color32::from_rgb(220, 180, 70)
        } else {
            Color32::from_rgb(80, 185, 205)
        };
        painter.rect_filled(fill, 2.0 * z, bar);
        painter.rect_stroke(
            meter,
            2.0 * z,
            Stroke::new(1.0, Color32::from_gray(70)),
            egui::StrokeKind::Inside,
        );
        painter.text(
            Pos2::new(meter.right() - 3.0 * z, meter.center().y),
            egui::Align2::RIGHT_CENTER,
            format!("{:.0}%", peak * 100.0),
            FontId::proportional(9.0 * z),
            Color32::from_gray(210),
        );
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
        // R4: the input node's channel row is the other combo.
        if matches!(node, NodeDoc::Input { .. }) {
            for row in &mut rows {
                if let InlineParamId::BuiltIn(InlineParam::InputChannel) = row.param {
                    row.choices = input_channel_choices();
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
                        state.pending_choice = Some((
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

    // ---- Port names + inline rename (timeline outlets / mixer inlets) ----
    // Painted last among the node overlays, so an open editor sits above the
    // channel strip. Captions are plain text, not widgets: the right-click
    // that opens the editor is handled on the canvas above using the very
    // same rects, which keeps node select / drag / marquee intact.
    let z = state.zoom;
    let mut rename_commit: Option<(PortNameTarget, String)> = None;
    for label in &name_labels {
        let editing = state
            .rename
            .as_ref()
            .is_some_and(|r| r.target == label.target);
        if editing {
            // A transient editor is wider than the name column (room to type);
            // it overlays the fader strip for the few frames it is open.
            let editor = Rect::from_min_size(
                label.rect.min,
                Vec2::new(label.rect.width().max(140.0 * z), label.rect.height()),
            );
            let mut cancel = false;
            let mut finish = false;
            if let Some(r) = state.rename.as_mut() {
                let resp = ui.put(
                    editor,
                    egui::TextEdit::singleline(&mut r.draft)
                        .font(egui::TextStyle::Small)
                        .hint_text(label.hint.clone()),
                );
                if std::mem::take(&mut r.needs_focus) {
                    resp.request_focus();
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                } else if resp.lost_focus() {
                    // Enter, Tab and a click elsewhere all end the edit.
                    // Empty clears the custom name back to the derived label.
                    rename_commit = Some((r.target, r.draft.trim().to_owned()));
                    finish = true;
                }
            }
            if cancel || finish {
                state.rename = None;
            }
        } else {
            // Timeline outlets read right-to-left into the box; mixer inlets
            // read left-to-right from the port dot.
            let (pos, align) = match label.target {
                PortNameTarget::Track { .. } => (
                    Pos2::new(label.rect.right(), label.rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                ),
                PortNameTarget::Channel { .. } => (
                    Pos2::new(label.rect.left(), label.rect.center().y),
                    egui::Align2::LEFT_CENTER,
                ),
            };
            painter.text(
                pos,
                align,
                &label.display,
                FontId::proportional(10.0 * z),
                Color32::from_rgb(225, 225, 228),
            );
        }
    }
    if let Some((target, name)) = rename_commit {
        match target {
            PortNameTarget::Track { kind, track } => {
                actions.push(PatchAction::RenameTrack { kind, track, name })
            }
            PortNameTarget::Channel { node, channel } => actions.push(PatchAction::RenameChannel {
                node,
                channel,
                name,
            }),
        }
    }

    // ---- SoundFont bank / patch list (U13) ----
    // Opened by a combo row. An `Area` (not a `ComboBox`) because the row is
    // a hand-painted strip scaled by the zoom: a native combo would keep its
    // own font size and overflow the box at low zoom, and the list has to
    // stay readable while the patch itself is zoomed out.
    // Stamp the opening frame first — the click that opened this menu is an
    // `any_click` in the same frame.
    if let Some((_, _, _, frame)) = &mut state.pending_choice
        && *frame == u64::MAX
    {
        *frame = ui.ctx().cumulative_frame_nr();
    }
    // The node can disappear while the list is open (delete / undo).
    if state
        .pending_choice
        .is_some_and(|(node, ..)| doc.node_by_id(node).is_none())
    {
        state.pending_choice = None;
    }
    if let Some((doc_id, param, menu_pos, opened)) = state.pending_choice {
        let bank = match doc.node_by_id(doc_id) {
            Some(NodeDoc::Instrument {
                voice: VoiceSpec::SoundFont { bank, .. },
                ..
            }) => *bank,
            _ => 0,
        };
        let presets = (deps.sf2_presets)(doc_id);
        let choices = match param {
            InlineParam::InputChannel => input_channel_choices(),
            _ => sf2_choices(&presets, param, bank),
        };
        let current = doc
            .node_by_id(doc_id)
            .map_or(0.0, |node| inline_doc_value(node, param).0);
        let title = match param {
            InlineParam::Sf2Bank => "SoundFont Bank",
            InlineParam::Sf2Program => "SoundFont Patch",
            InlineParam::InputChannel => "Input Channel",
            _ => "Parameter",
        };
        let mut picked: Option<u8> = None;
        egui::Area::new(egui::Id::unique("glitcherz_patch_sf2_choice"))
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
                state.pending_choice = None;
            }
            None => {
                // Ignore the click that opened the menu (same frame).
                if opened != ui.ctx().cumulative_frame_nr()
                    && ui
                        .ctx()
                        .input(|i| i.pointer.any_click() || i.key_pressed(egui::Key::Escape))
                {
                    state.pending_choice = None;
                }
            }
        }
    }

    // ---- Recording source list (R4) ----
    // Opened by the device chip on an `Audio In` box. Same list as the
    // Inspector picker; the stream is global, so this edits it for every take.
    let opening = state
        .pending_input_menu
        .is_some_and(|(_, _, frame)| frame == u64::MAX);
    if opening {
        state.pending_input_menu = state
            .pending_input_menu
            .map(|(node, pos, _)| (node, pos, ui.ctx().cumulative_frame_nr()));
        // Enumerating devices opens the driver: do it once per open, never
        // per frame.
        state.input_sources = deps.input.sources.map_or_else(Vec::new, |list| list());
    }
    if state
        .pending_input_menu
        .is_some_and(|(node, ..)| doc.node_by_id(node).is_none())
    {
        state.pending_input_menu = None;
    }
    if let Some((_, menu_pos, opened)) = state.pending_input_menu {
        let sources = state.input_sources.clone();
        let current = InputSourceItem {
            name: deps.input.device.map(str::to_string),
            loopback: deps.input.loopback,
        };
        let mut pick: Option<InputSourceItem> = None;
        let mut refresh = false;
        egui::Area::new(egui::Id::unique("glitcherz_patch_input_source"))
            .fixed_pos(menu_pos)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(320.0);
                    match (deps.input.open, deps.input.device) {
                        (true, Some(name)) => {
                            ui.label(egui::RichText::new(name).strong());
                            ui.weak(format!(
                                "{} ch · {}",
                                deps.input.channels,
                                if deps.input.loopback {
                                    "WASAPI loopback (what this output plays)"
                                } else {
                                    "capture input"
                                }
                            ));
                        }
                        _ => {
                            ui.colored_label(
                                Color32::from_rgb(230, 120, 110),
                                "No input stream is open",
                            );
                            if let Some(err) = deps.input.error {
                                ui.weak(err);
                            }
                        }
                    }
                    ui.separator();
                    if ui
                        .selectable_label(current.name.is_none(), "System default (microphone)")
                        .clicked()
                    {
                        pick = Some(InputSourceItem {
                            name: None,
                            loopback: false,
                        });
                    }
                    let (captures, loopbacks): (Vec<_>, Vec<_>) =
                        sources.into_iter().partition(|item| !item.loopback);
                    ui.add_space(2.0);
                    ui.weak("Inputs");
                    if captures.is_empty() {
                        ui.weak("(none found)");
                    }
                    for item in captures {
                        let selected = current.name == item.name && !current.loopback;
                        let caption = item.name.clone().unwrap_or_default();
                        if ui.selectable_label(selected, caption).clicked() {
                            pick = Some(item);
                        }
                    }
                    ui.add_space(2.0);
                    ui.weak("System playback (loopback)");
                    if loopbacks.is_empty() {
                        ui.weak("(none found)");
                    }
                    for item in loopbacks {
                        let selected = current.name == item.name && current.loopback;
                        let caption = format!("↩ {}", item.name.clone().unwrap_or_default());
                        if ui.selectable_label(selected, caption).clicked() {
                            pick = Some(item);
                        }
                    }
                    ui.separator();
                    if ui.button("Refresh devices").clicked() {
                        refresh = true;
                    }
                });
            });
        if let Some(item) = pick {
            actions.push(PatchAction::PickInputSource(item));
            state.pending_input_menu = None;
        } else if refresh {
            actions.push(PatchAction::RefreshInputSources);
            state.pending_input_menu = None;
        } else if opened != ui.ctx().cumulative_frame_nr()
            && ui
                .ctx()
                .input(|i| i.pointer.any_click() || i.key_pressed(egui::Key::Escape))
        {
            state.pending_input_menu = None;
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
    if let Some((source, to_doc, menu_pos)) = state.pending_auto {
        let options = (deps.auto_options)(to_doc);
        let from_node = matches!(source, AutoSource::Node(_));
        // Pitch targets come from `auto_options` (the app's shared table with
        // the timeline menu), so adding them here would duplicate every row.
        let mut picked: Option<AutoParam> = None;
        egui::Area::new(egui::Id::unique("glitcherz_patch_auto_menu"))
            .fixed_pos(menu_pos)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(140.0);
                    ui.strong(if from_node {
                        "Control Target Param"
                    } else {
                        "Automation Target Param"
                    });
                    for (label, param) in &options {
                        // Tempo is a timeline clock, never a node control
                        // destination (spec `midi-nodes.md` §7).
                        if from_node && matches!(param, AutoParam::Tempo) {
                            continue;
                        }
                        if ui.button(label).clicked() {
                            picked = Some(param.clone());
                        }
                    }
                });
            });
        if let Some(param) = picked {
            match source {
                AutoSource::Track(index) => actions.push(PatchAction::ConnectAuto {
                    index,
                    to_doc,
                    param,
                }),
                AutoSource::Node(from_doc) => actions.push(PatchAction::ConnectControl {
                    from_doc,
                    to_doc,
                    param,
                }),
            }
            state.pending_auto = None;
        } else if ui
            .ctx()
            .input(|i| i.pointer.any_click() || i.key_pressed(egui::Key::Escape))
        {
            state.pending_auto = None;
        }
    }

    // ---- Cable midpoint menu (click the control dot): insert node / delete ----
    // One flat level (spec D18). The old nested `Insert ▸ Effect ▸ …` submenu
    // was unclickable: the menu closed itself on *any* click of that frame,
    // and opening a submenu is a click that picks no leaf — so "Insert"
    // visibly did nothing. A `Popup` keeps the lifecycle (Escape / click
    // outside) in egui's hands, and the picker now answers "which effect".
    if let Some(menu) = state.pending_cable_menu.as_mut() {
        #[derive(Clone)]
        enum CablePick {
            InsertPicker,
            InsertPickerMidi,
            Insert(NodeSource),
            Delete,
        }
        let open_cmd = if std::mem::take(&mut menu.fresh) {
            Some(egui::SetOpenCommand::Bool(true))
        } else {
            None
        };
        // A cable can disappear between opening the menu and picking from it.
        let kind = doc.cables.get(menu.cable).map(|c| c.kind);
        let pick = egui::Popup::from_response(&response)
            .id(cable_menu_id())
            .kind(egui::PopupKind::Menu)
            .layout(egui::Layout::top_down_justified(egui::Align::Min))
            .at_position(menu.pos)
            .open_memory(open_cmd)
            .show(|ui| {
                ui.set_min_width(150.0);
                ui.strong("Cable");
                if kind == Some(CableKind::Audio) {
                    if ui.button("Insert Effect…").clicked() {
                        return Some(CablePick::InsertPicker);
                    }
                    // A mixer is not an "effect"; it gets its own leaf.
                    if ui.button("Insert Mixer").clicked() {
                        return Some(CablePick::Insert(NodeSource::Mixer));
                    }
                    ui.separator();
                }
                // Spec `midi-nodes.md` §8: a MIDI cable takes a MIDI effect
                // (the chain `source → effect → instrument`).
                if kind == Some(CableKind::Midi) {
                    if ui.button("Insert MIDI FX…").clicked() {
                        return Some(CablePick::InsertPickerMidi);
                    }
                    ui.separator();
                }
                if ui.button("Delete Cable").clicked() {
                    return Some(CablePick::Delete);
                }
                None
            })
            .and_then(|inner| inner.inner);
        match pick {
            Some(CablePick::InsertPicker) => actions.push(PatchAction::OpenPluginPicker {
                // Only effects can be spliced into an audio cable: an
                // instrument would be born without a MIDI feed.
                category: Some(NodeCategory::Effect),
                place: PickerPlace::Insert {
                    cable: menu.cable,
                    at: menu.at,
                },
            }),
            Some(CablePick::InsertPickerMidi) => actions.push(PatchAction::OpenPluginPicker {
                category: Some(NodeCategory::MidiFx),
                place: PickerPlace::Insert {
                    cable: menu.cable,
                    at: menu.at,
                },
            }),
            Some(CablePick::Insert(source)) => actions.push(PatchAction::InsertNode {
                cable: menu.cable,
                source,
                at: menu.at,
            }),
            Some(CablePick::Delete) => actions.push(PatchAction::DeleteCable(menu.cable)),
            None => {}
        }
        if !egui::Popup::is_id_open(ui.ctx(), cable_menu_id()) {
            state.pending_cable_menu = None;
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

    /// Spec `curve-map.md` D5: the node body is the mapping card, so its
    /// caption has to stay readable at patch zoom — `18k`, not `18000.0000`.
    #[test]
    fn curve_map_caption_stays_short() {
        let params = CurveMapParams {
            min: 20.0,
            max: 18_000.0,
            mode: glitcherz_model::MapMode::Log,
        };
        assert_eq!(curve_map_caption(params), "20..18k · Log");
        assert_eq!(
            curve_map_caption(CurveMapParams::default()),
            "-1..1 · Linear",
            "the identity default must read as an identity"
        );
    }

    /// D5: the map's single control port commits without a parameter menu.
    #[test]
    fn only_the_curve_map_has_one_control_port() {
        let mut project = Project::default();
        let id = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::CurveMap {
            node_id: id,
            track: 0,
            params: CurveMapParams::default(),
        });
        assert_eq!(
            single_control_port(&project.graph, id),
            Some(AutoParam::CurveMap)
        );
        // An instrument has many parameters, so it keeps the popup.
        let instrument = project.graph.instrument_of_track(0).expect("synth");
        assert_eq!(
            single_control_port(&project.graph, instrument.node_id()),
            None
        );
    }

    /// D27: the three hues are the type contract. Audio must be a real colour
    /// now — the old neutral grey blended into the dark canvas and into the
    /// grey input dots.
    #[test]
    fn d27_cable_hues_are_distinct() {
        let audio = cable_color(CableKind::Audio);
        let midi = cable_color(CableKind::Midi);
        let automation = cable_color(CableKind::Automation);
        assert_ne!(audio, midi);
        assert_ne!(audio, automation);
        assert_ne!(midi, automation);
        let [r, g, b, _] = audio.to_array();
        let (r, g, b) = (u16::from(r), u16::from(g), u16::from(b));
        assert!(
            g > r + 50 && b > r + 50,
            "audio must be a saturated cool hue, got {audio:?}"
        );
    }

    /// D27: saturated hue belongs to cables and ports alone; node strokes are
    /// one saturation step down so the two meanings never compete.
    #[test]
    fn d27_node_strokes_are_desaturated() {
        fn chroma(c: Color32) -> u8 {
            let [r, g, b, _] = c.to_array();
            [r, g, b].into_iter().max().unwrap() - [r, g, b].into_iter().min().unwrap()
        }
        let project = Project::default();
        let doc = &project.graph;
        let instrument = doc.instrument_of_track(0).expect("default synth");
        let stroke = node_color(PatchId::Node(instrument.node_id()), doc).0;
        assert!(
            chroma(stroke) < 80,
            "node stroke chroma {} is too high",
            chroma(stroke)
        );
        for kind in [CableKind::Audio, CableKind::Midi, CableKind::Automation] {
            assert!(
                chroma(cable_color(kind)) > chroma(stroke),
                "cables must be more saturated than node boxes for {kind:?}"
            );
        }
    }

    /// D27: input rings show the *structural* signal a node consumes.
    /// Automation has no physical port, so it must not steal the ring colour
    /// of the nodes it modulates — the yellow dashed line and its midpoint
    /// parameter label carry that distinction.
    #[test]
    fn d27_input_rings_follow_the_structural_signal() {
        let project = Project::default();
        let doc = &project.graph;
        let instrument = doc.instrument_of_track(0).expect("default synth");
        assert_eq!(
            node_in_port_kind(instrument),
            CableKind::Midi,
            "an instrument consumes notes, not audio"
        );
        let mixer = doc
            .nodes
            .iter()
            .find(|node| node.is_mixer())
            .expect("default mixer");
        assert_eq!(node_in_port_kind(mixer), CableKind::Audio);
        assert_eq!(
            node_in_port_kind(&NodeDoc::CurveMap {
                node_id: 99,
                track: 0,
                params: CurveMapParams::default(),
            }),
            CableKind::Automation,
            "the map node's only inlet is the control signal"
        );
        assert_eq!(
            node_in_port_kind(&NodeDoc::Arpeggiator {
                node_id: 99,
                track: 0,
                params: glitcherz_model::ArpeggiatorParams::default(),
            }),
            CableKind::Midi
        );
    }

    /// D27: the dashed automation path must survive a real egui frame.
    /// `CubicBezierShape` has no dash attribute, so this exercises the
    /// sampled-polyline branch of `paint_cable` end to end.
    #[test]
    fn d27_automation_cable_renders_in_a_frame() {
        let mut project = Project::default();
        let instrument = project
            .graph
            .instrument_of_track(0)
            .expect("default synth")
            .node_id();
        project
            .graph
            .cables
            .push(CableDoc::automation(0, instrument, AutoParam::TrackGain));
        let track_name = |_: TrackKind, _: usize| String::new();
        let auto_options = |_: u64| -> Vec<(String, AutoParam)> { Vec::new() };
        let plugin_inline = |_: u64| -> Vec<InlinePluginControl> { Vec::new() };
        let sf2_presets = |_: u64| -> Vec<(u8, u8, String)> { Vec::new() };
        let deps = PatchDeps {
            input: Default::default(),
            doc: &project.graph,
            track_count: project.midi_tracks.len(),
            track_name: &track_name,
            auto_count: project.control_tracks.len(),
            audio_count: project.audio_tracks.len(),
            auto_options: &auto_options,
            plugin_inline: &plugin_inline,
            sf2_presets: &sf2_presets,
        };
        let ctx = egui::Context::default();
        let mut state = PatchState::default();
        state.ensure_positions(
            &project.graph,
            deps.track_count,
            deps.auto_count,
            deps.audio_count,
        );
        let actions = drive(&ctx, &mut state, deps, vec![], 0.0);
        assert!(actions.is_empty(), "a passive frame produces no intents");
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

    /// Run one `patch::show` frame headless and return the intents it produced.
    fn drive(
        ctx: &egui::Context,
        state: &mut PatchState,
        deps: PatchDeps<'_>,
        events: Vec<egui::Event>,
        t: f64,
    ) -> Vec<PatchAction> {
        let mut viewports = egui::ViewportIdMap::default();
        viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                inner_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                ..Default::default()
            },
        );
        let raw = egui::RawInput {
            events,
            time: Some(t),
            viewports,
            ..Default::default()
        };
        let mut actions = Vec::new();
        let mut output = ctx.run_ui(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                actions = show(ui, state, deps);
            });
        });
        output.textures_delta.clear();
        actions
    }

    /// Drive a secondary (right) click at a screen position: hover, press,
    /// release. egui reports `secondary_clicked()` on the release frame.
    fn right_click(
        ctx: &egui::Context,
        state: &mut PatchState,
        deps: PatchDeps<'_>,
        pos: Pos2,
        t: f64,
    ) {
        let hover = egui::Event::PointerMoved(pos);
        let _ = drive(ctx, state, deps, vec![hover.clone()], t);
        for (k, pressed) in [true, false].into_iter().enumerate() {
            let _ = drive(
                ctx,
                state,
                deps,
                vec![
                    hover.clone(),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Secondary,
                        pressed,
                        modifiers: Default::default(),
                    },
                ],
                t + 0.1 * (k as f64 + 1.0),
            );
        }
    }

    /// Where the canvas starts in screen space. A background right-click opens
    /// the add-node menu with `at = pointer - canvas_origin` (pan 0 / zoom 1),
    /// so the menu placement tells us the exact origin the captions use.
    fn canvas_origin(deps: PatchDeps<'_>) -> Pos2 {
        let ctx = egui::Context::default();
        let mut state = PatchState::default();
        state.ensure_positions(
            deps.doc,
            deps.track_count,
            deps.auto_count,
            deps.audio_count,
        );
        for k in 0..3 {
            let _ = drive(&ctx, &mut state, deps, vec![], k as f64 * 0.1);
        }
        let probe = Pos2::new(400.0, 520.0);
        right_click(&ctx, &mut state, deps, probe, 1.0);
        let at = state
            .add_menu
            .expect("a background right-click opens the add-node menu")
            .at;
        Pos2::new(probe.x - at.x, probe.y - at.y)
    }

    /// The six-full-width-cell cap: CJK counts double, ASCII single, and a cut
    /// always ends with an ellipsis — a long name must never widen a node box.
    #[test]
    fn port_names_truncate_at_six_fullwidth_cells() {
        assert_eq!(truncate_fullwidth("Bass", 6), "Bass");
        assert_eq!(truncate_fullwidth("ABCDEFGHIJKL", 6), "ABCDEFGHIJKL");
        assert_eq!(truncate_fullwidth("ABCDEFGHIJKLM", 6), "ABCDEFGHIJKL…");
        assert_eq!(truncate_fullwidth("一二三四五六", 6), "一二三四五六");
        assert_eq!(truncate_fullwidth("一二三四五六七", 6), "一二三四五六…");
        assert_eq!(truncate_fullwidth("一二三ABCDEFG", 6), "一二三ABCDEF…");
        assert_eq!(truncate_fullwidth("", 6), "");
    }

    /// A rename in the arrangement must appear on the timeline outlet and on
    /// the mixer inlet it feeds on the very next frame — the old static `T1` /
    /// `Ch 1` captions went stale.
    #[test]
    fn port_names_read_live_track_and_channel_names() {
        use std::cell::RefCell;

        let project = Project::default();
        let name = RefCell::new("Bass".to_string());
        let track_name = |_: TrackKind, _: usize| name.borrow().clone();
        let auto_options = |_: u64| -> Vec<(String, AutoParam)> { Vec::new() };
        let plugin_inline = |_: u64| -> Vec<InlinePluginControl> { Vec::new() };
        let sf2_presets = |_: u64| -> Vec<(u8, u8, String)> { Vec::new() };
        let deps = PatchDeps {
            input: Default::default(),
            doc: &project.graph,
            track_count: project.midi_tracks.len(),
            track_name: &track_name,
            auto_count: project.control_tracks.len(),
            audio_count: project.audio_tracks.len(),
            auto_options: &auto_options,
            plugin_inline: &plugin_inline,
            sf2_presets: &sf2_presets,
        };
        let mut state = PatchState::default();
        state.ensure_positions(
            &project.graph,
            deps.track_count,
            deps.auto_count,
            deps.audio_count,
        );

        let before = port_name_labels(Pos2::ZERO, &state, &deps);
        *name.borrow_mut() = "Lead Synth".to_string();
        let after = port_name_labels(Pos2::ZERO, &state, &deps);

        let outlet = |labels: &[PortNameLabel]| {
            labels
                .iter()
                .find(|l| {
                    matches!(
                        l.target,
                        PortNameTarget::Track {
                            kind: TrackKind::Midi,
                            track: 0
                        }
                    )
                })
                .expect("a caption for MIDI track 0")
                .full
                .clone()
        };
        let inlet = |labels: &[PortNameLabel]| {
            labels
                .iter()
                .find(|l| matches!(l.target, PortNameTarget::Channel { channel: 0, .. }))
                .expect("a caption for mixer channel 0")
                .full
                .clone()
        };
        assert_eq!(outlet(&before), "Bass");
        assert_eq!(
            outlet(&after),
            "Lead Synth",
            "the timeline outlet must follow the track rename"
        );
        // The default project's first voice feeds mixer channel 0.
        assert_eq!(inlet(&before), "Bass");
        assert_eq!(inlet(&after), "Lead Synth");
    }

    /// The `Timeline In` recording inputs name the track they feed — the same
    /// live reader as the outlet captions, so a rename lands there too. An
    /// unnamed track keeps its positional tag.
    #[test]
    fn timeline_in_inputs_read_live_track_names_and_fall_back_to_tags() {
        use std::cell::RefCell;

        let project = Project::default();
        let name = RefCell::new("Bass".to_string());
        let track_name = |_: TrackKind, _: usize| name.borrow().clone();
        let auto_options = |_: u64| -> Vec<(String, AutoParam)> { Vec::new() };
        let plugin_inline = |_: u64| -> Vec<InlinePluginControl> { Vec::new() };
        let sf2_presets = |_: u64| -> Vec<(u8, u8, String)> { Vec::new() };
        let deps = PatchDeps {
            input: Default::default(),
            doc: &project.graph,
            track_count: project.midi_tracks.len(),
            track_name: &track_name,
            auto_count: project.control_tracks.len(),
            audio_count: project.audio_tracks.len(),
            auto_options: &auto_options,
            plugin_inline: &plugin_inline,
            sf2_presets: &sf2_presets,
        };

        assert_eq!(timeline_in_port_label(TrackKind::Midi, 0, &deps), "Bass");
        *name.borrow_mut() = "Lead Synth".to_string();
        assert_eq!(
            timeline_in_port_label(TrackKind::Midi, 0, &deps),
            "Lead Synth",
            "renaming a track must rename its Timeline In input"
        );
        // Longer than the caption cap: cut, never widened.
        *name.borrow_mut() = "一二三四五六七".to_string();
        assert_eq!(
            timeline_in_port_label(TrackKind::Midi, 0, &deps),
            "一二三四五六…"
        );
        // Unnamed: the port keeps the tag the old build always showed.
        *name.borrow_mut() = String::new();
        assert_eq!(timeline_in_port_label(TrackKind::Midi, 0, &deps), "T1");
        assert_eq!(timeline_in_port_label(TrackKind::Curve, 1, &deps), "C2");
        assert_eq!(timeline_in_port_label(TrackKind::Audio, 2, &deps), "A3");
    }

    /// A custom channel name wins over the derived track name, and the edit
    /// draft starts from that custom name.
    #[test]
    fn a_custom_channel_name_wins_over_the_track_name() {
        let mut project = Project::default();
        let mixer = project
            .graph
            .nodes
            .iter()
            .find(|n| n.is_mixer())
            .map(NodeDoc::node_id)
            .expect("the default project has a mixer");
        if let Some(NodeDoc::Mixer { channels, .. }) = project.graph.node_by_id_mut(mixer) {
            channels[0].name = "Kick Bus".to_string();
        }
        let track_name = |_: TrackKind, _: usize| "Bass".to_string();
        let auto_options = |_: u64| -> Vec<(String, AutoParam)> { Vec::new() };
        let plugin_inline = |_: u64| -> Vec<InlinePluginControl> { Vec::new() };
        let sf2_presets = |_: u64| -> Vec<(u8, u8, String)> { Vec::new() };
        let deps = PatchDeps {
            input: Default::default(),
            doc: &project.graph,
            track_count: project.midi_tracks.len(),
            track_name: &track_name,
            auto_count: project.control_tracks.len(),
            audio_count: project.audio_tracks.len(),
            auto_options: &auto_options,
            plugin_inline: &plugin_inline,
            sf2_presets: &sf2_presets,
        };
        let mut state = PatchState::default();
        state.ensure_positions(
            &project.graph,
            deps.track_count,
            deps.auto_count,
            deps.audio_count,
        );
        let labels = port_name_labels(Pos2::ZERO, &state, &deps);
        let channel = labels
            .iter()
            .find(|l| {
                l.target
                    == PortNameTarget::Channel {
                        node: mixer,
                        channel: 0,
                    }
            })
            .expect("a caption for mixer channel 0");
        assert_eq!(channel.full, "Kick Bus");
        assert_eq!(channel.raw, "Kick Bus");
    }

    /// Captions hug their dot (`PORT_NAME_GAP`), never paint over the mixer's
    /// fader strip, and inside the caption a press belongs to the node — not to
    /// the port the caption overlaps.
    #[test]
    fn port_name_captions_hug_the_dot_and_own_their_press() {
        let project = Project::default();
        let track_name = |_: TrackKind, _: usize| "Bass".to_string();
        let auto_options = |_: u64| -> Vec<(String, AutoParam)> { Vec::new() };
        let plugin_inline = |_: u64| -> Vec<InlinePluginControl> { Vec::new() };
        let sf2_presets = |_: u64| -> Vec<(u8, u8, String)> { Vec::new() };
        let deps = PatchDeps {
            input: Default::default(),
            doc: &project.graph,
            track_count: project.midi_tracks.len(),
            track_name: &track_name,
            auto_count: project.control_tracks.len(),
            audio_count: project.audio_tracks.len(),
            auto_options: &auto_options,
            plugin_inline: &plugin_inline,
            sf2_presets: &sf2_presets,
        };
        let mut state = PatchState::default();
        state.ensure_positions(
            &project.graph,
            deps.track_count,
            deps.auto_count,
            deps.audio_count,
        );
        let labels = port_name_labels(Pos2::ZERO, &state, &deps);
        let n = timeline_port_count(deps.track_count, deps.auto_count, deps.audio_count);
        let gap = PORT_NAME_GAP * state.zoom;
        for ((_, p), label) in state
            .out_ports_screen(Pos2::ZERO, PatchId::Timeline, &deps)
            .iter()
            .zip(labels.iter().take(n))
        {
            // The caption hangs off the dot by exactly one gap — no column
            // sized for the worst zoom (that used to be 30 pt).
            assert!((label.rect.right() - (p.x - gap)).abs() < 0.01);
            assert!(label.rect.left() >= 0.0, "the caption stays inside the box");
            // A press on the name belongs to the node: the box it sits in
            // drags, the port beside it stays untouched.
            assert!(
                port_grab_at(&labels, &state, Pos2::ZERO, label.rect.center(), &deps).is_none(),
                "the caption must not hand the press to the port"
            );
            // The dot itself still grabs, even though it is inside the
            // caption's grab-shadow in the other direction.
            assert!(
                port_grab_at(&labels, &state, Pos2::ZERO, *p, &deps).is_some(),
                "the dot must still start a cable"
            );
        }
        let mixer = project
            .graph
            .nodes
            .iter()
            .find(|n| n.is_mixer())
            .map(NodeDoc::node_id)
            .unwrap();
        for ((_, p), label) in state
            .in_ports_screen(Pos2::ZERO, PatchId::Node(mixer), &deps)
            .iter()
            .zip(labels.iter().skip(n))
        {
            assert!((label.rect.left() - (p.x + gap)).abs() < 0.01);
            assert!(label.rect.right() <= p.x + CHANNEL_STRIP_LEFT * state.zoom + 0.01);
        }
    }

    /// Right-clicking a port caption opens its inline rename editor instead of
    /// the canvas-wide add-node menu.
    #[test]
    fn right_clicking_a_port_name_opens_rename_not_the_add_menu() {
        let project = Project::default();
        let track_name = |_: TrackKind, _: usize| "Bass".to_string();
        let auto_options = |_: u64| -> Vec<(String, AutoParam)> { Vec::new() };
        let plugin_inline = |_: u64| -> Vec<InlinePluginControl> { Vec::new() };
        let sf2_presets = |_: u64| -> Vec<(u8, u8, String)> { Vec::new() };
        let deps = PatchDeps {
            input: Default::default(),
            doc: &project.graph,
            track_count: project.midi_tracks.len(),
            track_name: &track_name,
            auto_count: project.control_tracks.len(),
            audio_count: project.audio_tracks.len(),
            auto_options: &auto_options,
            plugin_inline: &plugin_inline,
            sf2_presets: &sf2_presets,
        };
        let origin = canvas_origin(deps);
        let ctx = egui::Context::default();
        let mut state = PatchState::default();
        state.ensure_positions(
            &project.graph,
            deps.track_count,
            deps.auto_count,
            deps.audio_count,
        );
        for k in 0..3 {
            let _ = drive(&ctx, &mut state, deps, vec![], k as f64 * 0.1);
        }
        let labels = port_name_labels(origin, &state, &deps);
        let caption = labels
            .iter()
            .find(|l| {
                matches!(
                    l.target,
                    PortNameTarget::Track {
                        kind: TrackKind::Midi,
                        track: 0
                    }
                )
            })
            .expect("a caption for MIDI track 0");
        right_click(&ctx, &mut state, deps, caption.rect.center(), 1.0);
        let editor = state
            .rename
            .as_ref()
            .expect("the caption right-click opens the rename editor");
        assert_eq!(
            editor.target,
            PortNameTarget::Track {
                kind: TrackKind::Midi,
                track: 0
            }
        );
        assert_eq!(editor.draft, "Bass");
        assert!(
            state.add_menu.is_none(),
            "the add-node menu must stay closed on a caption"
        );
    }

    /// Regression (v3, spec D18): clicking a cable's midpoint dot must open the
    /// cable menu, and the click that opened it must not dismiss it again.
    ///
    /// The old hand-rolled `Area` closed itself on *any* click of the opening
    /// frame, so opening `Insert ▸ …` visibly did nothing. The menu is now a
    /// real `Popup`, whose open state egui owns.
    #[test]
    fn cable_midpoint_menu_opens_and_survives_its_opening_frame() {
        let project = Project::default();
        let deps = PatchDeps {
            input: Default::default(),
            doc: &project.graph,
            track_count: project.midi_tracks.len(),
            track_name: &|_, _| String::new(),
            auto_count: project.control_tracks.len(),
            audio_count: project.audio_tracks.len(),
            auto_options: &|_| Vec::new(),
            plugin_inline: &|_| Vec::new(),
            sf2_presets: &|_| Vec::new(),
        };
        let mut state = PatchState::default();
        state.ensure_positions(
            &project.graph,
            project.midi_tracks.len(),
            project.control_tracks.len(),
            project.audio_tracks.len(),
        );

        // Any audio cable whose midpoint dot is not covered by a node box.
        let mid = project
            .graph
            .cables
            .iter()
            .filter_map(|cable| {
                let controls = state.cable_controls(Pos2::ZERO, cable, &deps)?;
                let mid = bezier_points(controls, 24)[12];
                state
                    .hit_node(Pos2::ZERO, mid, &deps)
                    .is_none()
                    .then_some(mid)
            })
            .next()
            .expect("the default project has a clickable audio cable");

        // The click that opens the menu only records the placement…
        assert!(state.point_select(Pos2::ZERO, mid, &deps, false).is_none());
        assert!(
            state.pending_cable_menu.is_some(),
            "clicking the midpoint dot must set up the menu"
        );

        // …and the popup then survives its own opening frame.
        let ctx = egui::Context::default();
        drive(&ctx, &mut state, deps, vec![], 0.0);
        drive(&ctx, &mut state, deps, vec![], 0.1);
        assert!(
            egui::Popup::is_id_open(&ctx, cable_menu_id()),
            "the cable menu closed itself on the frame it opened"
        );
        assert!(state.pending_cable_menu.is_some());

        // Escape closes it, and the pending state follows.
        let escape = egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        drive(&ctx, &mut state, deps, vec![escape], 0.2);
        drive(&ctx, &mut state, deps, vec![], 0.3);
        assert!(!egui::Popup::is_id_open(&ctx, cable_menu_id()));
        assert!(state.pending_cable_menu.is_none());
    }

    /// U3b D1/D7: the conductor terminal is a pure sink with a single
    /// automation input port — no outputs, and it rejects audio / MIDI cables
    /// exactly like any other port-type mismatch.
    #[test]
    fn timeline_in_ports_and_cable_rules() {
        let project = Project::default();
        let doc = &project.graph;
        let deps = PatchDeps {
            input: Default::default(),
            doc,
            track_count: project.midi_tracks.len(),
            track_name: &|_, _| String::new(),
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
            timeline_in_port_count(deps.track_count, deps.auto_count, deps.audio_count),
            "the terminal hosts the Tempo port plus one recording input per track"
        );

        // R6: the box accepts every cable type; the drop port decides.
        assert!(accepts(CableKind::Automation, PatchId::TimelineIn, doc));
        assert!(accepts(CableKind::Audio, PatchId::TimelineIn, doc));
        assert!(accepts(CableKind::Midi, PatchId::TimelineIn, doc));

        // Cable endpoint resolution: the sink is a valid target, never a
        // source; the timeline itself accepts nothing.
        assert_eq!(
            patch_port_to(&PortRef::TimelineIn),
            Some(PatchId::TimelineIn)
        );
        assert_eq!(patch_port_from(&PortRef::TimelineIn, &deps), None);
        assert!(!accepts(CableKind::Automation, PatchId::Timeline, doc));
        assert_eq!(
            patch_port_to(&PortRef::TrackIn {
                kind: TrackKind::Midi,
                track: 0
            }),
            Some(PatchId::TimelineIn)
        );
        assert_eq!(
            patch_port_from(
                &PortRef::TrackIn {
                    kind: TrackKind::Midi,
                    track: 0
                },
                &deps
            ),
            None,
            "a recording input is a pure sink"
        );
        assert_eq!(
            timeline_in_port(
                TrackKind::Midi,
                0,
                deps.track_count,
                deps.auto_count,
                deps.audio_count
            ),
            Some(1)
        );
        assert_eq!(
            timeline_in_port(
                TrackKind::Audio,
                0,
                deps.track_count,
                deps.auto_count,
                deps.audio_count
            ),
            Some(3)
        );
        assert_eq!(
            timeline_in_port_kind(0, deps.track_count, deps.auto_count, deps.audio_count),
            None,
            "port 0 is the tempo port"
        );

        // The layout round-trips the sink's position like master / timeline.
        assert!(state.export_layout(doc).timeline_in.is_some());
    }

    /// R6 D33: a keyboard's direct instrument cable renders dormant once the
    /// keyboard feeds a track recording input.
    #[test]
    fn a_taken_over_keyboard_cable_is_dormant() {
        let mut doc = GraphDoc {
            nodes: vec![
                NodeDoc::Instrument {
                    node_id: 2,
                    track: 0,
                    voice: VoiceSpec::Builtin,
                    synth: Default::default(),
                },
                NodeDoc::Keyboard {
                    node_id: 6,
                    track: 0,
                    source: MidiSource::Computer,
                },
            ],
            cables: vec![CableDoc::keyboard_midi(6, 2)],
            layout: Default::default(),
        };
        let direct = doc.cables[0].clone();
        assert_eq!(dormant_cable_track(&doc, &direct), None);
        doc.cables.push(CableDoc {
            kind: CableKind::Midi,
            from: PortRef::Node(6),
            to: PortRef::TrackIn {
                kind: TrackKind::Midi,
                track: 3,
            },
            param: AutoParam::default(),
            in_port: 0,
            map: None,
        });
        assert_eq!(
            dormant_cable_track(&doc, &direct),
            Some(3),
            "the direct cable is superseded by the track route"
        );
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
            input: Default::default(),
            doc,
            track_count: project.midi_tracks.len(),
            track_name: &|_, _| String::new(),
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
                bend_range: 2.0,
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
                region: glitcherz_model::SampleRegion::default(),
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
            input: Default::default(),
            doc,
            track_count: project.midi_tracks.len(),
            track_name: &|_, _| String::new(),
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
        // Mixer: wider box (port-name column), channel-scaled base + 1 row.
        assert_eq!(
            size_of(PatchId::Node(mixer), &deps),
            Vec2::new(MIXER_WIDTH, mixer_base + INLINE_ROW_HEIGHT)
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
                region: glitcherz_model::SampleRegion::default(),
            },
            synth: glitcherz_model::SynthParams::default(),
        };
        let (v, min, max) = inline_doc_value(&sample, InlineParam::SampleRoot);
        assert_eq!((v, min, max), (48.0, 0.0, 127.0));
        let (v, min, max) = inline_doc_value(&sample, InlineParam::SampleGain);
        assert_eq!((v, min, max), (0.5, 0.0, 2.0));

        // R4: the live input rows read the node's channel and trim.
        let input = NodeDoc::Input {
            node_id: 6,
            channel: 1,
            gain: 0.5,
        };
        assert_eq!(
            inline_doc_value(&input, InlineParam::InputChannel),
            (1.0, 0.0, 1.0)
        );
        assert_eq!(
            inline_doc_value(&input, InlineParam::InputGain),
            (0.5, 0.0, 2.0)
        );
        assert!(!input_channel_choices().is_empty());
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
            input: Default::default(),
            doc,
            track_count: project.midi_tracks.len(),
            track_name: &|_, _| String::new(),
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
            input: Default::default(),
            doc: &doc,
            track_count: 1,
            track_name: &|_, _| String::new(),
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

    /// M5 (spec `midi-nodes.md` §3): MIDI effect / control-source output
    /// ports and their cable kinds, plus the accept matrix.
    #[test]
    fn midi_fx_node_ports_and_accept_rules() {
        let mut doc = Project::default().graph;
        let instrument = doc.instrument_of_track(0).unwrap().node_id();
        let mixer = doc.first_mixer().unwrap().node_id();
        let arp = 100;
        doc.nodes.push(NodeDoc::Arpeggiator {
            node_id: arp,
            track: 0,
            params: Default::default(),
        });
        let portamento = 101;
        doc.nodes.push(NodeDoc::Portamento {
            node_id: portamento,
            track: 0,
            params: Default::default(),
        });
        let envelope = 102;
        doc.nodes.push(NodeDoc::MidiEnvelope {
            node_id: envelope,
            track: 0,
            params: Default::default(),
        });

        // Port kinds: arp = MIDI, portamento = MIDI + control, envelope =
        // control, ordinary nodes stay audio.
        assert_eq!(
            node_out_port_kind(doc.node_by_id(arp).unwrap(), 0),
            CableKind::Midi
        );
        assert_eq!(
            node_out_port_kind(doc.node_by_id(portamento).unwrap(), 0),
            CableKind::Midi
        );
        assert_eq!(
            node_out_port_kind(doc.node_by_id(portamento).unwrap(), 1),
            CableKind::Automation
        );
        assert_eq!(
            node_out_port_kind(doc.node_by_id(envelope).unwrap(), 0),
            CableKind::Automation
        );
        assert_eq!(
            node_out_port_kind(doc.node_by_id(instrument).unwrap(), 0),
            CableKind::Audio
        );

        // MIDI goes into instruments / MIDI effects / envelopes; the mixer
        // stays a mismatch.
        for target in [instrument, arp, portamento, envelope] {
            assert!(
                accepts(CableKind::Midi, PatchId::Node(target), &doc),
                "node {target} should take MIDI"
            );
        }
        assert!(!accepts(CableKind::Midi, PatchId::Node(mixer), &doc));

        // Control cables only reach parameter-bearing nodes.
        assert!(accepts(
            CableKind::Automation,
            PatchId::Node(instrument),
            &doc
        ));
        assert!(!accepts(CableKind::Automation, PatchId::Node(arp), &doc));
        assert!(!accepts(
            CableKind::Automation,
            PatchId::Node(envelope),
            &doc
        ));

        // Portamento exposes two output ports; the others one.
        let auto_options = |_: u64| -> Vec<(String, AutoParam)> { Vec::new() };
        let plugin_inline = |_: u64| -> Vec<InlinePluginControl> { Vec::new() };
        let sf2_presets = |_: u64| -> Vec<(u8, u8, String)> { Vec::new() };
        let deps = PatchDeps {
            input: Default::default(),
            doc: &doc,
            track_count: 1,
            track_name: &|_, _| String::new(),
            auto_count: 0,
            audio_count: 0,
            auto_options: &auto_options,
            plugin_inline: &plugin_inline,
            sf2_presets: &sf2_presets,
        };
        let mut state = PatchState::default();
        state.ensure_positions(&doc, 1, 0, 0);
        state.place_node(arp, Pos2::new(0.0, 0.0));
        state.place_node(portamento, Pos2::new(300.0, 0.0));
        state.place_node(envelope, Pos2::new(600.0, 0.0));
        assert_eq!(
            state
                .out_ports_screen(Pos2::ZERO, PatchId::Node(arp), &deps)
                .len(),
            1
        );
        assert_eq!(
            state
                .out_ports_screen(Pos2::ZERO, PatchId::Node(portamento), &deps)
                .len(),
            2
        );
        assert_eq!(
            state
                .out_ports_screen(Pos2::ZERO, PatchId::Node(envelope), &deps)
                .len(),
            1
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
            input: Default::default(),
            doc,
            track_count,
            track_name: &|_, _| String::new(),
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
            input: Default::default(),
            doc: &free.graph,
            track_count,
            track_name: &|_, _| String::new(),
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
            input: Default::default(),
            doc: &free.graph,
            track_count,
            track_name: &|_, _| String::new(),
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
                Vec2::new(MIXER_WIDTH, mixer_header + 4.0 * PORT_SPACING) * z,
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
                    name: String::new(),
                    gain: 0.5,
                    pan: 0.0,
                    muted: true,
                    solo: false,
                },
                glitcherz_model::MixerChannel {
                    name: String::new(),
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
                bend_range: 2.0,
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
                input: Default::default(),
                doc: &project.graph,
                track_count: 1,
                track_name: &|_, _| String::new(),
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
                bend_range: 2.0,
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
                input: Default::default(),
                doc: &project.graph,
                track_count: 1,
                track_name: &|_, _| String::new(),
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

    /// v3: the sample player owns its file exactly like the SoundFont node —
    /// one `Load File…` / `Replace…` button on the box, no file dialog at
    /// creation time (spec `plugin-browser.md` D16).
    #[test]
    fn sample_node_owns_its_file() {
        let mut project = Project::default();
        let node = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Instrument {
            node_id: node,
            track: 0,
            voice: VoiceSpec::Sample {
                path: String::new(),
                root_note: 60,
                play_mode: glitcherz_model::SamplePlayMode::Gate,
                gain: 1.0,
                region: glitcherz_model::SampleRegion::default(),
            },
            synth: glitcherz_model::SynthParams::default(),
        });
        let id = PatchId::Node(node);
        // Born empty: silent, one file button, the caption says so.
        {
            let deps = PatchDeps {
                input: Default::default(),
                doc: &project.graph,
                track_count: project.midi_tracks.len(),
                track_name: &|_, _| String::new(),
                auto_count: 0,
                audio_count: 0,
                auto_options: &|_| Vec::new(),
                plugin_inline: &|_| Vec::new(),
                sf2_presets: &|_| Vec::new(),
            };
            assert_eq!(label_of(id, &project.graph), "SAMPLE (no file) · Track 1");
            assert_eq!(action_rows(id, &deps), 1);
            assert_eq!(header_band(id, &deps), NODE_TITLE_BAND + ACTION_ROW_HEIGHT);
            let row = node_action_buttons(id, 0, &deps);
            assert_eq!(row.len(), 1);
            assert_eq!(row[0].label, "Load File…");
            assert!(matches!(
                row[0].action,
                PatchAction::LoadSample { node: target } if target == node
            ));
            // Box: title band + file row + root / gain.
            assert_eq!(inline_row_count(id, &deps), 2);
            assert_eq!(
                size_of(id, &deps),
                Vec2::new(
                    NODE_SIZE.x,
                    NODE_SIZE.y + ACTION_ROW_HEIGHT + 2.0 * INLINE_ROW_HEIGHT
                )
            );
        }

        // Loaded: the caption turns into the file stem, the button to Replace.
        if let Some(NodeDoc::Instrument { voice, .. }) = project.graph.node_by_id_mut(node) {
            *voice = VoiceSpec::Sample {
                path: "C:/samples/kick.wav".to_string(),
                root_note: 60,
                play_mode: glitcherz_model::SamplePlayMode::Gate,
                gain: 1.0,
                region: glitcherz_model::SampleRegion::default(),
            };
        }
        let deps = PatchDeps {
            input: Default::default(),
            doc: &project.graph,
            track_count: project.midi_tracks.len(),
            track_name: &|_, _| String::new(),
            auto_count: 0,
            audio_count: 0,
            auto_options: &|_| Vec::new(),
            plugin_inline: &|_| Vec::new(),
            sf2_presets: &|_| Vec::new(),
        };
        assert_eq!(label_of(id, &project.graph), "SAMPLE kick·C4 · Track 1");
        assert_eq!(node_action_buttons(id, 0, &deps)[0].label, "Replace…");
    }

    /// The built-in roster has six kinds: a node caption must name the payload's
    /// own kind instead of always saying "Delay".
    #[test]
    fn builtin_effect_caption_names_its_kind() {
        let mut project = Project::default();
        let node = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Effect {
            node_id: node,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: crate::effect::default_params(BuiltinEffectKind::Eq),
            delay: None,
        });
        assert_eq!(
            label_of(PatchId::Node(node), &project.graph),
            "EQ · Track 1"
        );

        if let Some(NodeDoc::Effect { params, .. }) = project.graph.node_by_id_mut(node) {
            *params = crate::effect::default_params(BuiltinEffectKind::Reverb);
        }
        assert_eq!(
            label_of(PatchId::Node(node), &project.graph),
            "Reverb · Track 1"
        );
    }
}
