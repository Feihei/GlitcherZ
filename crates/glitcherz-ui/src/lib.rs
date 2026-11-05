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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use glitcherz_clap_host::{ClapInstance, ClapInstanceConfig};
use glitcherz_engine::AUDIO_FILE_EXTENSIONS;
use glitcherz_engine::AudioEngine;
use glitcherz_engine::OutputConfig;
use glitcherz_engine::RecordSource;
use glitcherz_engine::SampleError;
use glitcherz_engine::SampleLibrary;
use glitcherz_engine::Sf2Error;
use glitcherz_engine::convert;
use glitcherz_engine::realtime::{
    ArpeggiatorProcessor, AudioTrackProcessor, AutoDest, ChannelStripProcessor, Curve,
    CurveMapProcessor, DrumKitProcessor, DrumSlotSpec, EffectMeters, EngineCommand, EventKind,
    FeedbackProcessor, InputProcessor, LfoProcessor, MAX_AUTO_POINTS, MAX_MIDI_FANOUT,
    MAX_TRACK_CLIPS, MapMode as EngineMapMode, MapSpec as EngineMapSpec, MidiClipTable,
    MidiEnvelopeProcessor, MixerChannel as EngineMixerChannel, MixerProcessor, NodeKind, NoteEvent,
    ParamId, PortamentoProcessor, Processor, RtAudioClip, RtAutoPoint, RtAutomation, RtControlClip,
    RtCtrl, RtCtrlKind, RtCurveInterp, RtMidiClip, RtNote, SAMPLE_DEFAULT_ROOT,
    SampleVoiceProcessor, SequencerCommand, Sf2VoiceProcessor, SilentProcessor, SynthProcessor,
    Unit, empty_clip_table, empty_midi_clip_table, mode_value as curve_mode_value,
};
use glitcherz_engine::{MidiTarget, Sf2Library, Sf2Preset};
use glitcherz_model::{
    ArpeggiatorParams, AutoParam, AutoPoint, CableKind, ChannelParam, ControlClip, ControlTrack,
    CtrlKind, CurveInterp, CurveMapParams, EffectParams, GraphDoc, LFO_DIVISION_LABELS, LfoParams,
    LfoWaveform, MAX_ARRANGEMENT_TICKS, MAX_AUTO_TRACKS, MAX_CLIPS_PER_TRACK, MAX_MARKERS,
    MAX_MIDI_TRACKS, MAX_MIXER_CHANNELS, MAX_NODES, MIN_MIXER_CHANNELS, Marker, MidiClip,
    MidiEnvelopeParams, MidiSource, MidiTrack, NodeDoc, NodeParam, Note, PortRef, PortamentoParams,
    Project, SamplePlayMode, SongLoop, SynthParams, SynthWaveform, TrackKind, VoiceSpec,
};

pub mod autosave;
pub mod curve;
pub mod editing;
pub mod effect;
pub mod engine_sync;
pub mod event_list;
pub mod export_audio;
pub mod export_ops;
pub mod file_ops;
pub mod focus;
pub mod fonts;
pub mod grid;
pub mod inspector_panel;
pub mod keyboard;
pub mod media;
pub mod mixer_panel;
pub mod patch;
pub mod patch_ops;
pub mod piano_roll;
pub mod plugin_catalog;
pub mod plugin_picker;
pub mod plugin_window;
pub mod project_files;
pub mod recorder;
pub mod recording;
pub mod settings;
pub mod status_bar;
pub mod timeline;
pub mod timeline_ops;
pub mod transport;
pub mod voices;

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

/// Master output gain range (linear, 1.0 = unity). Same span as a track
/// fader so the two feel identical.
pub const MASTER_GAIN_RANGE: RangeInclusive<f32> = 0.0..=1.5;

/// Meter scale floor in dB (the transport meter's bottom of travel).
const METER_FLOOR_DB: f32 = -60.0;

/// Width reserved for the mixer console's pinned right column (channel
/// add/remove + output stage) next to the horizontally scrolling strip row.
const MIXER_STRIP_CONTROLS_W: f32 = 120.0;

/// Id salt of the mixer console's strip row scroll area. The middle-drag pan
/// resolves the id from the ScrollArea output, so this only keeps the state
/// stable across frames (and across mixer nodes — one console at a time).
const MIXER_STRIP_SCROLL: &str = "glitcherz_mixer_strip_scroll";

/// Id salt of the strip row's child Ui (the horizontal band that holds the
/// scrolling strips plus the pinned right column). Explicit so the scroll
/// area's own id — derived from this Ui — survives layout churn above.
const MIXER_STRIP_ROW: &str = "glitcherz_mixer_strip_row";

/// Width of one mixer strip column (its controls are all built to this
/// width, so the title cell matches them).
const MIXER_STRIP_W: f32 = 96.0;

/// Minimum height of a strip's title cell (the real height is one line of body
/// text, measured per frame — this only floors it so a very small font still
/// leaves room for the inline rename field).
const MIXER_STRIP_TITLE_MIN_H: f32 = 18.0;

/// First-frame height of the strip row (bootstrap only — the real height is
/// measured from the laid-out strips one frame later, so a font or DPI change
/// corrects itself). Roughly label + S/M + meter + fader + pan + spacing.
const MIXER_STRIP_MIN_H: f32 = 240.0;

/// Share of the Edit tab's height the Event List strip takes on its first
/// frame — roughly two thirds of the tab stay with the piano roll.
const EVENT_LIST_HEIGHT_FRACTION: f32 = 1.0 / 3.0;

/// Floor for the Event List strip: its caption row plus a few note rows.
/// Dragging the divider below this collapses the strip instead.
const EVENT_LIST_MIN_H: f32 = 96.0;

/// Ceiling for the Event List strip, so the roll keeps a usable band even on a
/// tall window (the panel also caps itself at the available height).
const EVENT_LIST_MAX_H: f32 = 640.0;

/// One in-progress channel-title rename in the mixer console. The console
/// strips are drawn inside a closure, so the state is `take()`n out of the app
/// before the row lays out and put back afterwards.
struct ChannelRename {
    /// Mixer node owning the channel (only one console is visible at a time,
    /// but the node id keeps a stale editor from landing on another mixer).
    node: u64,
    /// Channel index inside that node.
    channel: usize,
    /// Draft text (empty on commit clears the custom name).
    draft: String,
    /// The right-click that opened the editor lands one frame before the text
    /// field exists — request focus on the first frame that draws it.
    needs_focus: bool,
}

/// One in-flight audio take (R4): the take ring's samples plus the geometry
/// needed to cut them into clips when the transport stops.
struct AudioTake {
    /// Audio tracks armed by a `Timeline In` cable from an `Input` node.
    tracks: Vec<usize>,
    /// Absolute tick where capture starts (the clip start, before latency
    /// compensation).
    start_ticks: f64,
    /// Interleaved samples to discard before the take begins (pre-roll).
    skip_interleaved: usize,
    /// Interleaved channels in the input stream.
    channels: usize,
    /// Engine sample rate (the recording rate — no resampling).
    sample_rate: u32,
    /// Interleaved samples captured so far.
    samples: Vec<f32>,
}

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

/// Two-cycle LFO shape preview for the Inspector (spec `lfo.md` §6).
///
/// UI-thread only: the point list is allocated here, outside the audio
/// callback. Sample & Hold alternates two fixed demo values (its real sequence
/// depends on the node seed and is only audible while playing).
fn lfo_wave_preview(ui: &mut egui::Ui, params: LfoParams) {
    let width = ui.available_width().clamp(80.0, 260.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 56.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, egui::Color32::from_gray(22));
    painter.rect_stroke(
        rect,
        3.0,
        egui::Stroke::new(1.0, egui::Color32::from_gray(70)),
        egui::StrokeKind::Inside,
    );
    let mid = rect.center().y;
    painter.line_segment(
        [egui::pos2(rect.left(), mid), egui::pos2(rect.right(), mid)],
        egui::Stroke::new(1.0, egui::Color32::from_gray(55)),
    );
    const STEPS: usize = 128;
    let mut points = Vec::with_capacity(STEPS + 1);
    for i in 0..=STEPS {
        let t = i as f32 / STEPS as f32;
        let cycles = t * 2.0;
        let hold = if (cycles.floor() as i32) % 2 == 0 {
            0.55
        } else {
            -0.6
        };
        let value = params.preview_value(cycles.fract(), hold) * params.depth;
        points.push(egui::pos2(
            rect.left() + rect.width() * t,
            mid - value * rect.height() * 0.42,
        ));
    }
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(1.5, egui::Color32::from_rgb(120, 195, 100)),
    ));
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

/// Every engine parameter of a built-in synthesizer payload, in one place
/// (spec `instrument.md`): the live document push and the offline export
/// engine both walk this table, so they cannot drift apart.
fn synth_param_values(synth: &SynthParams) -> [(ParamId, f32); 8] {
    [
        (ParamId::OscWaveform, f32::from(synth.waveform.index())),
        (ParamId::FilterCutoff, synth.cutoff_hz),
        (ParamId::FilterResonance, synth.resonance),
        (ParamId::FilterMode, synth.filter_mode),
        (ParamId::Attack, synth.attack_s),
        (ParamId::Decay, synth.decay_s),
        (ParamId::Sustain, synth.sustain),
        (ParamId::Release, synth.release_s),
    ]
}

/// Document automation parameter → engine parameter ID.
///
/// `None` for [`AutoParam::Tempo`] (the engine route uses the Timeline In
/// sentinel instead, U3b D1/D7), for [`AutoParam::Cc`] and
/// [`AutoParam::PitchBend`]: those are events in the node's MIDI input, not
/// parameter slots (U4a D2/D3, P1 D9), and must never reach `set_node_param`.
fn param_id_of(param: &AutoParam) -> Option<ParamId> {
    match param {
        AutoParam::TrackGain => Some(ParamId::TrackGain),
        AutoParam::TrackPan => Some(ParamId::TrackPan),
        AutoParam::FilterCutoff => Some(ParamId::FilterCutoff),
        AutoParam::PluginParam { param_id, .. } => Some(ParamId::PluginParam(*param_id)),
        AutoParam::Node(param) => Some(node_param_id(*param)),
        // The curve map's input port is a pass-through, not a parameter
        // (spec `curve-map.md` D5): the value is normalized on the wire.
        AutoParam::CurveMap => Some(ParamId::CurveMapIn),
        AutoParam::Tempo | AutoParam::Cc { .. } | AutoParam::PitchBend { .. } => None,
    }
}

/// Node parameter address (model) → engine control destination for a
/// node-to-node control cable (spec `midi-nodes.md` D6).
///
/// CC / bend keep their event-stream semantics (they are not parameters);
/// everything else lands on the same `ParamId` the automation tables use.
fn event_dest_of(param: &AutoParam, graph: &GraphDoc, target: &PortRef) -> AutoDest {
    if let Some((controller, channel)) = param.cc_of() {
        return AutoDest::Cc {
            controller,
            channel,
        };
    }
    if let Some((channel, _)) = param.pitch_bend_of() {
        // Same derivation as the axis (spec `curve-map.md` D12): the range is
        // the instrument's, so an SF2 range edit reaches this route too.
        return AutoDest::PitchBend {
            channel,
            range_semitones: param.bend_range(graph, target),
        };
    }
    AutoDest::Param(param_id_of(param).unwrap_or(ParamId::MasterGain))
}

/// The map a document cable delivers through (spec `curve-map.md` D2/D4).
///
/// New documents carry it explicitly on the cable (written whenever the UI
/// routes it); a cable from an older file derives it from the target, which is
/// exactly what the v8 → v9 migration would have written.
/// Live destinations of one keyboard node (R6 D31/D33): `(tracks, nodes)`.
///
/// A node wired into one or more MIDI **recording inputs** dispatches into
/// those tracks — the track outlet then reaches the instruments, so listening
/// and recording share one path — and its direct instrument cables are
/// dormant. Without a recording-input cable the direct cable list is used.
fn keyboard_live_routes(doc: &GraphDoc, node: u64) -> (Vec<usize>, Vec<u64>) {
    let tracks: Vec<usize> = doc
        .node_track_inputs(node)
        .into_iter()
        .filter_map(|(kind, track)| (kind == TrackKind::Midi).then_some(track))
        .collect();
    let nodes = if tracks.is_empty() {
        doc.keyboard_routes_from(node)
    } else {
        Vec::new()
    };
    (tracks, nodes)
}

/// Frames of audio the take ring holds before capture starts: a span in
/// **ticks** converted through `sec_per_tick` (960 ticks per quarter, not
/// seconds) at the device rate.
///
/// Multiplying ticks by the sample rate directly turns a one-bar pre-roll
/// into hundreds of millions of samples, which skips the entire take and
/// reads as "audio records nothing".
fn pre_roll_skip_frames(span_ticks: f64, sec_per_tick: f64, sample_rate: u32) -> usize {
    (span_ticks.max(0.0) * sec_per_tick.max(0.0) * f64::from(sample_rate))
        .round()
        .max(0.0) as usize
}

/// Short form of a cpal device name for the transport chip: "麦克风 (Realtek
/// High Definition Audio)" would otherwise push the whole transport row wide.
fn short_device_name(name: &str) -> String {
    const LIMIT: usize = 24;
    if name.chars().count() <= LIMIT {
        return name.to_string();
    }
    let mut out: String = name.chars().take(LIMIT).collect();
    out.push('…');
    out
}

/// Writes the selected channel of an interleaved take as a mono f32 wav
/// (R4 D11). Float PCM keeps the take lossless; the sample cache decodes it
/// like any other wav.
fn write_mono_wav(
    path: &std::path::Path,
    data: &[f32],
    channels: usize,
    sample_rate: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for frame in data.chunks_exact(channels.max(1)) {
        writer.write_sample(frame[0])?;
    }
    writer.finalize()?;
    Ok(())
}

/// Whether a timeline MIDI outlet reaches an instrument through the MIDI
/// node graph (R6 D30 warning: recording works but nothing is audible).
fn midi_outlet_has_instrument(doc: &GraphDoc, track: usize) -> bool {
    let mut reached: std::collections::BTreeSet<u64> = Default::default();
    let mut queue: std::collections::VecDeque<u64> = Default::default();
    for cable in &doc.cables {
        if cable.kind == CableKind::Midi
            && cable.from == PortRef::MidiTrack(track)
            && let PortRef::Node(id) = cable.to
        {
            queue.push_back(id);
        }
    }
    while let Some(id) = queue.pop_front() {
        if !reached.insert(id) {
            continue;
        }
        if doc
            .node_by_id(id)
            .is_some_and(|node| matches!(node, NodeDoc::Instrument { .. }))
        {
            return true;
        }
        for cable in &doc.cables {
            if cable.kind == CableKind::Midi
                && cable.from == PortRef::Node(id)
                && let PortRef::Node(next) = cable.to
            {
                queue.push_back(next);
            }
        }
    }
    false
}

/// Whether an audio track's generator reaches the master through audio
/// cables. The default graph leaves every `AudioTrack` node unwired, so a
/// recorded take can be perfect and still produce no sound — worth saying
/// out loud at take start (same spirit as the MIDI outlet warning).
fn audio_outlet_reaches_master(doc: &GraphDoc, track: usize) -> bool {
    let Some(start) = doc.nodes.iter().find_map(|node| match node {
        NodeDoc::AudioTrack { node_id, track: t } if *t == track => Some(*node_id),
        _ => None,
    }) else {
        return false;
    };
    let mut reached: std::collections::BTreeSet<u64> = Default::default();
    let mut queue: std::collections::VecDeque<u64> = std::collections::VecDeque::from([start]);
    while let Some(id) = queue.pop_front() {
        if !reached.insert(id) {
            continue;
        }
        for cable in &doc.cables {
            if cable.kind != CableKind::Audio || cable.from != PortRef::Node(id) {
                continue;
            }
            match cable.to {
                PortRef::Master => return true,
                PortRef::Node(next) => queue.push_back(next),
                _ => {}
            }
        }
    }
    false
}

fn cable_map(cable: &glitcherz_model::CableDoc, graph: &GraphDoc) -> glitcherz_model::MapSpec {
    // A Curve Map source owns the axis of the hop after it (spec `curve-map.md`
    // D6), and it is read from the node rather than from a copy: editing Low /
    // High in the inspector moves the hop with it, no re-wiring needed.
    if let PortRef::Node(id) = &cable.from
        && let Some(NodeDoc::CurveMap { params, .. }) = graph.node_by_id(*id)
    {
        return params.map();
    }
    // `None` means "derive from the target", so a curve follows the range of
    // whatever it points at *now* — including a range that changes later
    // (SF2 bend range, an effect node swapped for another, a map node edit).
    cable
        .map
        .unwrap_or_else(|| cable.param.default_map(graph, &cable.to))
}

/// Document map → engine map. The two crates keep their own `Copy` types on
/// purpose (model stays engine-free), so this is the single crossing point.
fn engine_map(map: glitcherz_model::MapSpec) -> EngineMapSpec {
    EngineMapSpec {
        min: map.min,
        max: map.max,
        mode: engine_mode(map.mode),
    }
}

/// Document mapping mode → engine mapping mode (same crossing point as
/// [`engine_map`]).
fn engine_mode(mode: glitcherz_model::MapMode) -> EngineMapMode {
    match mode {
        glitcherz_model::MapMode::Linear => EngineMapMode::Linear,
        glitcherz_model::MapMode::Log => EngineMapMode::Log,
        glitcherz_model::MapMode::Exp => EngineMapMode::Exp,
    }
}

/// Node parameter address (model) → engine parameter ID (U4c, spec
/// `control-tracks.md` §9 D16).
///
/// The single translation point between the document's [`NodeParam`] and the
/// engine's `ParamId`: the model stays engine-free, and every consumer (the
/// automation route and the menus) goes through here.
fn node_param_id(param: NodeParam) -> ParamId {
    match param {
        NodeParam::EffectSlot(slot) => ParamId::BuiltinParam(slot),
        NodeParam::MixerChannel { channel, param } => match param {
            ChannelParam::Gain => ParamId::MixerGain(channel),
            ChannelParam::Pan => ParamId::MixerPan(channel),
            ChannelParam::Mute => ParamId::MixerMute(channel),
            ChannelParam::Solo => ParamId::MixerSolo(channel),
        },
        NodeParam::MixerOutPan => ParamId::MixerOutPan,
        NodeParam::MixerOutMute => ParamId::MixerOutMute,
        NodeParam::FeedbackGain => ParamId::FeedbackGain,
        NodeParam::Sf2Gain => ParamId::Sf2Gain,
        NodeParam::SampleGain => ParamId::SampleGain,
        NodeParam::PitchOffset => ParamId::PitchOffset,
        NodeParam::Resonance => ParamId::FilterResonance,
        NodeParam::Attack => ParamId::Attack,
        NodeParam::Decay => ParamId::Decay,
        NodeParam::Sustain => ParamId::Sustain,
        NodeParam::Release => ParamId::Release,
    }
}

/// Plugin parameter tables of one node (U4c): instruments and effects keep
/// **separate** session tables (the same split the panels use), so the caller
/// has to know which pair to hand over.
fn plugin_params_of<'a>(
    doc: &GraphDoc,
    node_id: u64,
    clap_voice: &'a HashMap<u64, Vec<glitcherz_clap_host::ClapParamInfo>>,
    clap_fx: &'a HashMap<u64, Vec<glitcherz_clap_host::ClapParamInfo>>,
    vst3_voice: &'a HashMap<u64, Vec<glitcherz_vst3_host::Vst3ParamInfo>>,
    vst3_fx: &'a HashMap<u64, Vec<glitcherz_vst3_host::Vst3ParamInfo>>,
) -> (
    Option<&'a [glitcherz_clap_host::ClapParamInfo]>,
    Option<&'a [glitcherz_vst3_host::Vst3ParamInfo]>,
) {
    let (clap, vst3) = match doc.node_by_id(node_id) {
        Some(NodeDoc::Instrument { .. }) => (clap_voice, vst3_voice),
        Some(NodeDoc::Effect { .. }) => (clap_fx, vst3_fx),
        _ => return (None, None),
    };
    (
        clap.get(&node_id).map(Vec::as_slice),
        vst3.get(&node_id).map(Vec::as_slice),
    )
}

/// Every automatable parameter of one node, as `(menu label, endpoint)`
/// (U4c, spec `control-tracks.md` §9 D19).
///
/// One source of truth for **both** entry points (the patch's cable popup and
/// the Inspector's Target row): the two menus used to be maintained separately,
/// which is exactly how the mixer faders ended up with no automation target at
/// all.
fn node_auto_options(
    doc: &GraphDoc,
    node_id: u64,
    clap: Option<&[glitcherz_clap_host::ClapParamInfo]>,
    vst3: Option<&[glitcherz_vst3_host::Vst3ParamInfo]>,
) -> Vec<(String, AutoParam)> {
    let mut options = Vec::new();
    let Some(node) = doc.node_by_id(node_id) else {
        return options;
    };
    // Built-in parameters first (the node's own sound), plugin parameters
    // after — a plugin with 2000 parameters must not bury `Gain`.
    match node {
        NodeDoc::Instrument {
            voice: VoiceSpec::Builtin,
            ..
        } => {
            options.push(("Cutoff".to_string(), AutoParam::FilterCutoff));
            options.push((
                "Resonance".to_string(),
                AutoParam::Node(NodeParam::Resonance),
            ));
            for (label, param) in [
                ("Attack", NodeParam::Attack),
                ("Decay", NodeParam::Decay),
                ("Sustain", NodeParam::Sustain),
                ("Release", NodeParam::Release),
            ] {
                options.push((format!("Env {label}"), AutoParam::Node(param)));
            }
        }
        NodeDoc::Instrument {
            voice: VoiceSpec::SoundFont { .. },
            ..
        } => options.push(("Gain".to_string(), AutoParam::Node(NodeParam::Sf2Gain))),
        NodeDoc::Instrument {
            voice: VoiceSpec::Sample { .. },
            ..
        } => options.push(("Gain".to_string(), AutoParam::Node(NodeParam::SampleGain))),
        NodeDoc::Instrument { .. } => {}
        // Table-driven (spec `effects.md` D6): every slot, not just the inline
        // ones — an EQ band that is not on the node body is still automatable.
        NodeDoc::Effect {
            voice: VoiceSpec::Builtin | VoiceSpec::SoundFont { .. },
            params,
            ..
        } => {
            let descriptor = crate::effect::descriptor(params);
            for (slot, spec) in descriptor.params.iter().enumerate() {
                options.push((
                    spec.name.to_string(),
                    AutoParam::Node(NodeParam::EffectSlot(slot as u8)),
                ));
            }
        }
        NodeDoc::Effect { .. } => {}
        NodeDoc::Mixer { inputs, .. } => {
            for channel in 0..*inputs {
                for (label, param) in [
                    ("Gain", ChannelParam::Gain),
                    ("Pan", ChannelParam::Pan),
                    ("Mute", ChannelParam::Mute),
                    ("Solo", ChannelParam::Solo),
                ] {
                    options.push((
                        format!("Ch {} {label}", channel + 1),
                        AutoParam::Node(NodeParam::MixerChannel { channel, param }),
                    ));
                }
            }
            options.push((
                "Out Pan".to_string(),
                AutoParam::Node(NodeParam::MixerOutPan),
            ));
            options.push((
                "Out Mute".to_string(),
                AutoParam::Node(NodeParam::MixerOutMute),
            ));
        }
        NodeDoc::Feedback { .. } => options.push((
            "Feedback".to_string(),
            AutoParam::Node(NodeParam::FeedbackGain),
        )),
        // D5: the map node's single port — the signal itself, not a value.
        NodeDoc::CurveMap { .. } => options.push(("Curve In".to_string(), AutoParam::CurveMap)),
        _ => {}
    }
    for param in clap.unwrap_or_default() {
        let label = if param.name.is_empty() {
            format!("Param {}", param.id)
        } else {
            param.name.clone()
        };
        options.push((
            format!("CLAP {label}"),
            AutoParam::PluginParam {
                param_id: param.id,
                name: label,
            },
        ));
    }
    for param in vst3.unwrap_or_default() {
        let label = if param.title.is_empty() {
            format!("Param {}", param.id)
        } else {
            param.title.clone()
        };
        options.push((
            format!("VST3 {label}"),
            AutoParam::PluginParam {
                param_id: param.id,
                name: label,
            },
        ));
    }
    // Spec `curve-map.md` §5: every row shows the axis it would drive. A
    // curve's range is then visible *before* the cable exists, and a user
    // picking between `Cutoff 20–18k Log` and `Resonance 0–0.98` can tell
    // which one their shape was drawn for.
    let target = PortRef::Node(node_id);
    for (label, param) in &mut options {
        label.push_str(" · ");
        label.push_str(&axis_hint(&param.default_map(doc, &target)));
    }
    options
}

/// `20–18k Log`, `±24`, `0–1` — the axis summary a target menu row shows.
fn axis_hint(map: &glitcherz_model::MapSpec) -> String {
    let map = map.sanitized();
    // Symmetric axes read better as ±N (pitch, pan, bend).
    let span = if map.min == -map.max && map.min < 0.0 {
        format!("±{}", crate::patch::axis_number(map.max))
    } else {
        format!(
            "{}–{}",
            crate::patch::axis_number(map.min),
            crate::patch::axis_number(map.max)
        )
    };
    match map.mode {
        glitcherz_model::MapMode::Linear => span,
        mode => format!("{span} {}", mode.label()),
    }
}

/// Control-change destinations offered by the node-graph curve menus (U4a).
///
/// A short preset list instead of all 128 controllers: these are the ones that
/// carry meaning in practice. They are offered as cable destinations when a
/// curve is wired into an instrument's MIDI input.
const CC_PRESETS: [(u8, &str); 8] = [
    (1, "Mod Wheel"),
    (2, "Breath"),
    (7, "Volume"),
    (10, "Pan"),
    (11, "Expression"),
    (64, "Sustain"),
    (71, "Resonance"),
    (74, "Cutoff"),
];

/// Document curve interpolation → engine mirror (U12). The two enums are
/// structurally identical; this is the one conversion point.
fn rt_curve_interp(interp: CurveInterp) -> RtCurveInterp {
    match interp {
        CurveInterp::Linear => RtCurveInterp::Linear,
        CurveInterp::Smooth => RtCurveInterp::Smooth,
    }
}

/// Document controller kind → engine form (P0, spec `roll-multiclip.md` D14).
fn rt_ctrl_kind(kind: CtrlKind) -> RtCtrlKind {
    match kind {
        CtrlKind::Cc(controller) => RtCtrlKind::Cc(controller),
        CtrlKind::PitchBend => RtCtrlKind::PitchBend,
        CtrlKind::ChannelPressure => RtCtrlKind::ChannelPressure,
        CtrlKind::PolyPressure(pitch) => RtCtrlKind::PolyPressure(pitch),
        CtrlKind::Program => RtCtrlKind::Program,
    }
}

/// Built-in inline registry entry → engine parameter ID (G5 spec §7: the
/// inline sliders address the same `set_node_param` entries as the panel
/// sliders, so both edit paths stay interchangeable).
fn inline_param_id(p: patch::InlineParam) -> ParamId {
    match p {
        patch::InlineParam::Cutoff => ParamId::FilterCutoff,
        patch::InlineParam::Resonance => ParamId::FilterResonance,
        patch::InlineParam::EffectSlot(slot) => ParamId::BuiltinParam(slot),
        patch::InlineParam::MixerOutPan => ParamId::MixerOutPan,
        // U14: the mixer channel strips on the canvas edit the same channel
        // params the Inspector console does.
        patch::InlineParam::MixerGain(ch) => ParamId::MixerGain(ch),
        patch::InlineParam::MixerMute(ch) => ParamId::MixerMute(ch),
        patch::InlineParam::MixerSolo(ch) => ParamId::MixerSolo(ch),
        patch::InlineParam::FeedbackGain => ParamId::FeedbackGain,
        patch::InlineParam::SampleRoot => ParamId::SampleRootNote,
        patch::InlineParam::SampleGain => ParamId::SampleGain,
        patch::InlineParam::Sf2Gain => ParamId::Sf2Gain,
        // Bank / patch rows go through `set_soundfont_preset` (they need the
        // preset list to keep a valid address), never straight to a param.
        patch::InlineParam::Sf2Bank => ParamId::Sf2BankSelect,
        patch::InlineParam::Sf2Program => ParamId::Sf2ProgramChange,
        patch::InlineParam::InputGain => ParamId::InputGain,
        patch::InlineParam::InputChannel => ParamId::InputChannel,
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
    /// Recording source endpoint name (`None` = system default). App-level,
    /// not document-level: one input stream serves every project.
    #[serde(default)]
    input_device: Option<String>,
    /// Whether `input_device` names an **output** endpoint captured through
    /// WASAPI loopback (R9) rather than a capture endpoint. Separate field so
    /// the stored name stays a plain device name.
    #[serde(default)]
    input_loopback: bool,
    /// Persisted output endpoint (`None` = system default). S1-5: a deliberate
    /// device choice must survive restarts, or every launch silently reverts.
    #[serde(default)]
    output_device: Option<String>,
    /// Persisted output sample rate (`None` = the device default).
    #[serde(default)]
    output_sample_rate: Option<u32>,
    /// Persisted fixed buffer size in frames (`None` = the device default).
    #[serde(default)]
    output_buffer_size: Option<u32>,
}

/// Capacity limit of the recent files list.
const RECENT_FILES_MAX: usize = 8;

impl UiState {
    /// The persisted output setup as an engine request (S1-5).
    fn output_config(&self) -> OutputConfig {
        OutputConfig {
            device: self.output_device.clone(),
            sample_rate: self.output_sample_rate,
            buffer_size: self.output_buffer_size,
        }
    }
}

/// Whether the live engine already satisfies `config` (S1-5). Used at startup
/// to avoid reopening the default device just because a preference exists.
fn output_config_matches(info: &glitcherz_engine::AudioEngineInfo, config: &OutputConfig) -> bool {
    config
        .device
        .as_deref()
        .is_none_or(|device| device == info.device_name)
        && config
            .sample_rate
            .is_none_or(|rate| rate == info.sample_rate)
        && config
            .buffer_size
            .is_none_or(|frames| info.buffer_frames == Some(frames))
}

/// Key for [`UiState`] in eframe storage.
const UI_STATE_KEY: &str = "glitcherz_ui_state";

/// Key for the encoded plugin catalog in eframe storage (spec
/// `plugin-browser.md` D11).
const PLUGIN_CACHE_KEY: &str = "plugin_cache";

/// Status bar message display duration (errors linger twice as long — a
/// failed SoundFont load is worth reading, not glimpsing).
const STATUS_MESSAGE_TTL: Duration = Duration::from_secs(8);
const STATUS_ERROR_TTL: Duration = Duration::from_secs(16);

/// How long the `⏺ Rec` refusal card stays up (it has its own close button,
/// so this only clears a card the user walked away from).
const RECORD_REFUSAL_TTL: Duration = Duration::from_secs(20);

/// How severe a status-bar message is. Failures are painted red and stay up
/// longer; everything else is the neutral info blue.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MessageKind {
    Info,
    Error,
}

/// How many parsed SoundFonts stay resident (U13 D12). Two covers both real
/// cases — one file shared by several nodes, and A/B between two fonts.
/// Eviction costs a re-read on the next reinstall, never a dropped note.
const SF2_LIBRARY_CACHE_CAP: usize = 2;

/// Byte count for a human (`1.9 GB`, `240 MB`).
fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else {
        format!("{:.0} KB", (b / KB).max(1.0))
    }
}

/// Address the document's preset inside a freshly loaded font (U13 D13).
///
/// Swapping files almost always loses `bank:program` (a MuseScore General
/// patch number means nothing in a drum font). Fall back in four steps, each
/// one the closest still-playable answer: same address → first patch of the
/// same bank → GM `0:0` → whatever the file has first. `Sf2Library::load`
/// rejects preset-less files, so the last step always succeeds.
fn resolve_sf2_preset(presets: &[Sf2Preset], bank: u8, program: u8) -> (u8, u8) {
    let has = |bank: u8, program: u8| {
        presets
            .iter()
            .any(|p| p.bank == bank && p.program == program)
    };
    if has(bank, program) {
        return (bank, program);
    }
    if let Some(first) = presets.iter().find(|p| p.bank == bank) {
        return (first.bank, first.program);
    }
    if has(0, 0) {
        return (0, 0);
    }
    // `Sf2Library::load` rejects preset-less files, so there is always one.
    presets
        .first()
        .map_or((0, 0), |first| (first.bank, first.program))
}

/// External plugin instance guard (CLAP / VST3).
#[allow(dead_code)] // guard held only to extend lifetime; the field itself is never read
pub(crate) enum PluginGuard {
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

/// Resolves the Inspector's editing target (spec `plugin-gui.md` §6):
/// the first **editable** document node in the patch selection, in document
/// order. Only a genuinely empty selection falls back to the selected track's
/// first-match instrument (the pre-G4 track-driven behavior).
///
/// "Editable" includes the built-in MIDI FX / control nodes (M5) and the Curve
/// Map (spec `curve-map.md` §5): their Inspector panel *is* their only UI, so
/// leaving them out made `midi_fx_params` unreachable — selecting one fell
/// back to the track's instrument instead.
///
/// Selecting a node that has no editable parameters (the timeline, the master
/// output, a keyboard / sample / audio-track node) must **not** resurrect the
/// instrument panel: the panel belongs to the selection. Stale ids (a node
/// deleted while still selected) are the one exception and still fall back,
/// so deleting a node does not blank the panel.
fn resolve_panel_target(
    doc: &GraphDoc,
    selected: &[u64],
    has_any_selection: bool,
    track: usize,
) -> Option<u64> {
    if has_any_selection {
        let mut matched_uneditable = false;
        for node in &doc.nodes {
            if selected.contains(&node.node_id()) {
                if matches!(
                    node,
                    NodeDoc::Instrument { .. }
                        | NodeDoc::Effect { .. }
                        | NodeDoc::Mixer { .. }
                        | NodeDoc::Arpeggiator { .. }
                        | NodeDoc::Portamento { .. }
                        | NodeDoc::MidiEnvelope { .. }
                        | NodeDoc::Lfo { .. }
                        | NodeDoc::CurveMap { .. }
                        // R4: the live input node's device picker, meter and
                        // channel/gain are its Inspector panel.
                        | NodeDoc::Input { .. }
                ) {
                    return Some(node.node_id());
                }
                matched_uneditable = true;
            }
        }
        if matched_uneditable || selected.is_empty() {
            // A non-editable node, or only the timeline / master entry, is
            // selected: the Inspector shows that node's (empty) panel.
            return None;
        }
        // Only stale ids are left: deleted nodes keep the track fallback.
    }
    doc.instrument_of_track(track).map(NodeDoc::node_id)
}

/// Whether the Inspector follows the **track** (built-in delay + external
/// effect list) instead of a single node.
///
/// True when nothing is selected, or when the panel target is the track's
/// instrument (the pre-G4 "instrument + its delay" view). Any other selection
/// — timeline, mixer, a MIDI FX / control node, an effect — owns the panel and
/// must not show another node's parameters.
fn panel_shows_track_extras(target_doc: &Option<NodeDoc>, has_any_selection: bool) -> bool {
    match target_doc {
        Some(NodeDoc::Instrument { .. }) => true,
        Some(_) => false,
        None => !has_any_selection,
    }
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

/// Master fader label: linear gain rendered in dB (`-∞` at silence).
fn gain_db_text(gain: f32) -> String {
    if gain <= 1e-4 {
        "-∞ dB".to_string()
    } else {
        format!("{:+.1} dB", 20.0 * gain.log10())
    }
}

/// One channel's level in dB (clamped by the callers' floor, not here).
fn level_db(level: f32) -> f32 {
    20.0 * level.max(1e-6).log10()
}

/// dB label for a meter read-out (`-∞` below the displayed floor).
fn meter_db_label(db: f32) -> String {
    if db <= METER_FLOOR_DB {
        "-∞".to_string()
    } else {
        format!("{db:.1}")
    }
}

/// Meter bar colour by level: green under -9 dB, amber approaching full
/// scale, red at the top (overload territory).
fn meter_color(db: f32) -> egui::Color32 {
    if db >= -1.0 {
        egui::Color32::from_rgb(220, 96, 84)
    } else if db >= -9.0 {
        egui::Color32::from_rgb(224, 184, 76)
    } else {
        egui::Color32::from_rgb(96, 186, 112)
    }
}

/// Stereo peak meter for the transport bar: two vertical bars (L / R) on a
/// [`METER_FLOOR_DB`]..0 dB scale, fed by the 600 ms peak hold. Drawn by
/// hand because a `ProgressBar` is a single horizontal bar.
/// Master output meter: two thin **horizontal** bars, left channel on top,
/// right below. Takes the rest of the transport row, so the meter gets a long
/// run-out; total height matches one button row.
fn master_meter(ui: &mut egui::Ui, levels: [f32; 2]) {
    let width = ui.available_width().max(60.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 18.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let bar_h = 7.0;
    let gap = 4.0;
    for (index, level) in levels.into_iter().enumerate() {
        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.top() + index as f32 * (bar_h + gap)),
            egui::vec2(rect.width(), bar_h),
        );
        painter.rect_filled(
            bar,
            egui::CornerRadius::same(1),
            ui.visuals().extreme_bg_color,
        );
        let db = level_db(level);
        let fill = ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0);
        if fill > 0.0 {
            painter.rect_filled(
                egui::Rect::from_min_max(
                    bar.min,
                    egui::pos2(bar.left() + bar.width() * fill, bar.bottom()),
                ),
                egui::CornerRadius::same(1),
                meter_color(db),
            );
        }
    }
    let [left, right] = levels.map(|level| meter_db_label(level_db(level)));
    response.on_hover_text(format!("L {left} dB · R {right} dB"));
}

/// Main view switch (tab bar under the transport panel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum MainView {
    /// Arrangement timeline (default view: the DAW's home screen).
    #[default]
    Timeline,
    /// Piano roll (MIDI clip) or curve editor (control clip) for the
    /// timeline's selected clip (U12: control clips have their own curve
    /// editor here again). The tab has no target of its own: it follows the
    /// clip selection, so a double-click and a plain "select, then open Edit"
    /// land on the same view.
    Edit,
    /// Node patch graph (full-height, dedicated view).
    Patch,
    /// The selected node's parameters (plus its mixer console when the
    /// selection is a mixer node).
    Inspector,
}

impl MainView {
    /// Left-to-right tab order, which is also the `Tab` / `Ctrl`-free cycle
    /// order and the `Alt+1..4` index. The graph comes first because that is
    /// where the project is actually built, but the arrangement stays the
    /// view the app *opens* on (see `default` above): a DAW's home screen is
    /// still the timeline, it is just no longer the leftmost tab.
    const ORDER: [Self; 4] = [Self::Patch, Self::Timeline, Self::Edit, Self::Inspector];

    /// Tab caption.
    fn label(self) -> &'static str {
        match self {
            Self::Patch => "Patch",
            Self::Timeline => "Timeline",
            Self::Edit => "Edit",
            Self::Inspector => "Inspector",
        }
    }

    /// Next / previous view in [`Self::ORDER`], wrapping at both ends.
    fn cycle(self, forward: bool) -> Self {
        let i = Self::ORDER
            .iter()
            .position(|v| *v == self)
            .unwrap_or_default();
        let n = Self::ORDER.len();
        Self::ORDER[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }

    /// `Alt+1..4` target: the n-th tab (1-based), if it exists.
    fn from_index(index: usize) -> Option<Self> {
        Self::ORDER.get(index).copied()
    }
}

#[cfg(test)]
mod main_view_tests {
    use super::MainView;

    /// The tab bar, the `Tab` cycle and `Alt+1..4` all read `ORDER`, so this
    /// pins the one order the user asked for: patch first, arrangement second.
    #[test]
    fn the_tab_order_puts_patch_first_and_timeline_second() {
        assert_eq!(
            MainView::ORDER,
            [
                MainView::Patch,
                MainView::Timeline,
                MainView::Edit,
                MainView::Inspector
            ]
        );
        assert_eq!(
            MainView::default(),
            MainView::Timeline,
            "a new session still opens on the arrangement, not on the graph"
        );
    }

    #[test]
    fn cycling_wraps_in_both_directions() {
        assert_eq!(MainView::Inspector.cycle(true), MainView::Patch);
        assert_eq!(MainView::Patch.cycle(false), MainView::Inspector);
        assert_eq!(MainView::Patch.cycle(true), MainView::Timeline);
        assert_eq!(MainView::Timeline.cycle(false), MainView::Patch);
    }

    #[test]
    fn alt_digits_index_the_same_order() {
        assert_eq!(MainView::from_index(0), Some(MainView::Patch));
        assert_eq!(MainView::from_index(1), Some(MainView::Timeline));
        assert_eq!(MainView::from_index(2), Some(MainView::Edit));
        assert_eq!(MainView::from_index(3), Some(MainView::Inspector));
        assert_eq!(MainView::from_index(4), None, "only four tabs");
    }
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

/// Which clip the Edit tab is targeting (U12: it hosts both the MIDI piano
/// roll and the control curve editor).
#[derive(Debug, Clone, PartialEq, Eq)]
enum EditTarget {
    /// MIDI clips: the writable focus plus the read-only ghost context (P3,
    /// spec `roll-multiclip.md` D4).
    Midi(piano_roll::RollSubject),
    /// Control clip (curve editor).
    Control { track: usize, clip: usize },
}

/// Curve editor input tool (U12, spec `control-tracks.md` §8 D15). Transient
/// (never persisted): the persisted data is `ControlClip::interp`, which the
/// tool selects — `Draw`/`Line` write `Linear`, `Curve` writes `Smooth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum CurveTool {
    /// Control-point polyline (discrete points, straight segments).
    #[default]
    Line,
    /// Freehand multi-point draw.
    Draw,
    /// Control-point smooth curve (monotone cubic).
    Curve,
}

impl CurveTool {
    /// The interpolation this tool writes into the document.
    fn interp(self) -> CurveInterp {
        match self {
            CurveTool::Draw | CurveTool::Line => CurveInterp::Linear,
            CurveTool::Curve => CurveInterp::Smooth,
        }
    }

    /// The tool matching a clip's persisted interpolation.
    fn of(interp: CurveInterp) -> Self {
        match interp {
            CurveInterp::Linear => CurveTool::Line,
            CurveInterp::Smooth => CurveTool::Curve,
        }
    }
}

/// A control-thread job that blocks the UI while it runs (U13).
///
/// The motivating case is loading a SoundFont: `SoundFont::new` parses the
/// whole RIFF tree and decodes the sample data in one call, seconds for a
/// 100 MB font and tens of seconds for a multi-GB one. There is no way to
/// chunk it, and a worker thread would add a whole class of races (a second
/// load, a project close, an undo arriving mid-parse) for one blocking call.
///
/// Because egui only paints *after* `ui()` returns, the notice has to be on
/// screen **before** the work starts. Hence the three phases:
///
/// | phase | frame does |
/// | :--- | :--- |
/// | 0 | queues the job; banner is not on screen yet → ask for another frame |
/// | 1 | draws the banner; nothing else → paint lands after `ui()` returns |
/// | 2 | draws the banner again, then runs the job (the screen keeps showing frame 1's paint) |
pub struct BlockingJob {
    /// Banner caption; says what the window is busy with.
    label: String,
    /// Phase 0 → 1 → 2 (run). See the table above.
    phase: u8,
    /// The work itself, handed a `&mut GlitcherZApp` so it can do anything a
    /// menu handler could.
    run: Box<dyn FnOnce(&mut GlitcherZApp)>,
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
    /// Audio track being edited (U6: row highlight + `- Audio` target).
    selected_audio: usize,
    /// Arrangement timeline cross-frame state (zoom/drag).
    timeline_state: timeline::TimelineState,
    /// Which clip the Edit tab is targeting (`None` = nothing selected, the
    /// tab shows its hint). Derived from the timeline's clip selection by
    /// [`Self::sync_edit_target`]: the target kind picks the editor (MIDI
    /// piano roll vs control curve, U12).
    edit_clip: Option<EditTarget>,
    /// Timeline selection the current [`Self::edit_clip`] was assembled from.
    /// Kept so [`Self::sync_edit_target`] only rebuilds the roll when the
    /// selection actually moved — the roll's own focus (chip strip / Tab)
    /// would otherwise be reset every frame (spec `roll-multiclip.md` E15).
    edit_selection: Vec<timeline::ClipSel>,
    /// Curve editor tool (U12; transient input mode, not persisted).
    curve_tool: CurveTool,
    /// Anchor tick currently being dragged in the curve editor (U12; `None`
    /// = no drag). The anchor's tick is its identity while it moves.
    curve_drag: Option<u32>,
    /// Whether the Edit tab's Event List strip is expanded. It is a resizable
    /// bottom panel: the roll takes everything above it, and dragging the
    /// divider past its minimum collapses the strip to its caption row.
    event_list_open: bool,
    /// Piano roll cross-frame state (scroll/drag).
    roll_state: piano_roll::RollState,
    /// Hover-follow keyboard focus arbiter (Blender-style context; see
    /// `focus.rs`). Keyboards surfaces register each frame; the decision is
    /// applied at the start of the next frame.
    focus_router: focus::FocusRouter,
    /// Node patch cross-frame state (pan/zoom/selection/drag).
    patch_state: patch::PatchState,
    /// Mixer strip row viewport as of the last frame: scroll area id + visible
    /// rect. The middle-drag pan reads it **before** the ScrollArea lays out,
    /// so the offset and the pointer move in the same frame (the same trick
    /// the timeline plays with `rows_rect`).
    mixer_strip_view: Option<(egui::Id, egui::Rect)>,
    /// Height of the mixer strip band, measured from the last laid-out row.
    /// The strip row is laid out in a child Ui with an **explicit** height:
    /// `ui.horizontal` would give it one row (~18 px), which collapses the
    /// scroll area's viewport and with it the middle-drag hit box and the
    /// scroll bar position.
    mixer_strip_h: f32,
    /// In-progress channel-title rename in the mixer console (right-click a
    /// strip title, type, Enter commits). `None` = no editor open.
    channel_rename: Option<ChannelRename>,
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
    /// Timeline recording tracks the computer-keyboard node feeds (R6 D33):
    /// when the node has a cable into a track's recording input, those tracks
    /// are its destinations and the direct instrument cables go dormant.
    vk_track_targets: Vec<usize>,
    /// Hardware MIDI input (R3, spec `recording.md` §3.2): open ports and the
    /// routing snapshots their callbacks read. Control-side only — the
    /// callbacks push into the command queue directly, never through the UI.
    midi_hub: glitcherz_engine::MidiInputHub,
    /// Port names from the last enumeration, kept next to the hub so the
    /// Inspector can render its menu without a second backend query.
    midi_ports: Vec<String>,
    /// Bindings last pushed to the hub, so an unchanged document does not
    /// rebuild the connections every frame.
    midi_bindings: Vec<(String, Vec<glitcherz_engine::MidiTarget>)>,
    /// Ports the last binding pass could not find (bound but unplugged). Shown
    /// once in the status bar, not every frame.
    midi_missing: Vec<String>,
    /// Currently held notes (shared by on-screen keys and computer keys, for highlighting and
    /// de-duplication).
    held_notes: HashSet<u8>,
    /// Most recently triggered note (shown in the status bar).
    current_note: Option<u8>,
    /// Master meter peak-hold, one per channel (the transport meter).
    meter_l: PeakMeter,
    meter_r: PeakMeter,
    opened_at: Instant,
    auto_close_after: Option<Duration>,
    /// Current project file path (`None` = never saved; a dialog pops up on save).
    project_path: Option<PathBuf>,
    /// Recently opened project files (persisted).
    recent: Vec<PathBuf>,
    /// Crash-recovery snapshots found at startup (S3a). Emptied once the user
    /// has answered the prompt; a non-empty list draws the recovery modal.
    recovery: Vec<autosave::Candidate>,
    /// The app's storage directory (the untitled document's snapshot lives
    /// there). `None` on a platform without one — then only saved projects are
    /// snapshotted.
    autosave_app_dir: Option<PathBuf>,
    /// When the next snapshot is due (S3a).
    autosave_clock: autosave::Clock,
    /// Media the document references but cannot find (S3b). Non-empty draws
    /// the missing-media prompt; every path is a **stored** string so it can be
    /// rewritten by key. Recomputed on open, collect and relocate.
    missing_media: Vec<String>,
    /// Status bar message (text + time shown + severity; disappears
    /// automatically after a timeout).
    status_message: Option<(String, Instant, MessageKind)>,
    /// Why the last `⏺ Rec` press was refused (text + time shown). Drawn as a
    /// floating card above the transport: a status-line-only refusal is too
    /// easy to miss, and "Rec did nothing" reads as a dead button.
    record_refusal: Option<(String, Instant)>,
    /// When the last take started. A take whose transport never starts rolling
    /// is a dead button, not a recording — checked a second later.
    record_started_at: Option<Instant>,
    /// Recording source endpoint name (`None` = the system default). Persisted:
    /// a picker that forgets the pick every launch is worse than no picker.
    audio_device: Option<String>,
    /// Whether `audio_device` is an output endpoint captured through WASAPI
    /// loopback (`audio_device` + this = `RecordSource::Loopback`).
    audio_loopback: bool,
    /// Sources shown by the transport's recording menu (refreshed on demand —
    /// enumeration opens the driver).
    audio_sources: Vec<RecordSource>,
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
    /// Meter handles of built-in effects that publish one (the compressor),
    /// keyed by **document node_id**. The handle travels with the node: the
    /// audio thread owns a clone, this map owns the other, and a recycled node
    /// id can never read a stale meter (spec `effects.md` E2b).
    effect_meters: HashMap<u64, std::sync::Arc<EffectMeters>>,
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

    /// Captured factory-default state blobs (document node_id → blob),
    /// taken at spawn time before any document state restore — the preset
    /// "Reset" target.
    voice_default_state: HashMap<u64, Vec<u8>>,
    /// Selected preset name per instrument node (the ComboBox display +
    /// Delete target).
    voice_preset_selected: HashMap<u64, String>,
    /// "Save preset" name input.
    preset_name_input: String,
    /// Loaded SoundFonts, **least recently used first** (U13 D12). Two
    /// entries cover the shapes that actually happen — one file shared by
    /// several nodes, and A/B between two fonts — without hoarding hundreds
    /// of MB per unused file. Evicting is safe: every live synthesizer owns
    /// its own `Arc<SoundFont>`, so an evicted entry only costs a re-read the
    /// next time a node is reinstalled (undo / project open / file swap).
    sf2_libraries: Vec<(PathBuf, Arc<Sf2Library>)>,
    /// Preset names by path (U13), kept even after the parsed font above is
    /// evicted. A few KB of text per font, and it is what lets the Inspector
    /// and the node's combos still list the presets of a font that only
    /// survives inside a live synthesizer.
    sf2_preset_names: HashMap<PathBuf, Vec<(u8, u8, String)>>,
    /// A control-thread job that blocks the UI while it runs (U13).
    /// SoundFont parsing cannot be chunked, so the banner is painted on one
    /// frame and the job runs at the end of the next one (see
    /// [`BlockingJob`]).
    blocking_job: Option<BlockingJob>,
    /// Session multi-sample cache (U5, spec `sample-player.md` D7): path →
    /// decoded `Arc<SampleData>`. Every sample instrument shares it, so
    /// reinstall / undo / document reload never re-reads a wav.
    sample_library: SampleLibrary,
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
    /// Running offline export (U7, spec `export-audio.md`): pumped a few
    /// blocks per frame at the end of `ui()`. Dropping it releases the
    /// offline engine and its plugin guards.
    export_job: Option<export_audio::ExportJob>,
    /// Transport metronome state (spec `recording.md` R1). UI-side only: the
    /// engine holds the authoritative click, this drives the button and is
    /// re-sent on toggle. Never persisted in the document.
    metronome_on: bool,
    /// Linear click level, `0..=1` (default `0.6` — audible under a mix
    /// without dominating it).
    metronome_gain: f32,
    /// Set when the click was turned on but the command queue was full, so
    /// the next frame retries instead of silently dropping the state change.
    metronome_pending: bool,
    /// MIDI recording state machine (spec `recording.md` R2): pre-roll,
    /// capture filtering, note pairing and the take → clip conversion.
    recorder: recorder::MidiRecorder,
    /// One frame's worth of captured events, drained into the recorder and
    /// reused (the drain appends, the recorder consumes).
    capture_scratch: Vec<glitcherz_engine::realtime::CapturedNote>,
    /// Pre-roll length in bars (`0..=2`, spec D14; default one bar so a take
    /// starts without the user having to count the click in).
    record_pre_roll_bars: u8,
    /// R6 D29: while recording, silence the sequenced notes of every other
    /// MIDI track, so only the take is heard. UI-session state, not persisted.
    record_mute_other: bool,
    /// Desired capture-tap state that a full command queue refused; retried
    /// every frame until it lands.
    capture_pending: Option<bool>,
    /// Capture-overflow count at the last warning, so a loss is reported once
    /// per burst instead of every frame.
    capture_overflow_seen: u64,
    /// What the current take actually pulled out of the engine: MIDI events,
    /// interleaved audio samples and control points. Reported when a take ends
    /// empty, so "nothing recorded" says *where* the signal stopped.
    take_events: u64,
    take_samples: u64,
    take_points: u64,
    /// Curve-track baking state (R8).
    control_recorder: recorder::ControlRecorder,
    /// One frame's control samples, reused.
    control_scratch: Vec<glitcherz_engine::realtime::CapturedControl>,
    /// In-flight audio take (R4). `None` = not recording audio.
    audio_take: Option<AudioTake>,
    /// Scratch buffer for one frame's recorded samples (reused).
    audio_scratch: Vec<f32>,
    /// Manual input-latency offset in samples (D13): positive values move the
    /// clip earlier on the timeline. Session-only until PDC exists.
    record_latency_samples: i64,
    /// Input drop count at the last warning (R5: report once per burst).
    input_dropped_seen: u64,
    /// The range-selection modal for a pending export (U7 D2).
    export_dialog_open: bool,
    /// Selected range inside the export dialog.
    export_range_choice: export_audio::ExportRange,
    /// Selected bit depth (S3c). Sticky across exports: a delivery format is
    /// a session preference, not a per-file question.
    export_bits: export_audio::Bits,
    /// Selected export sample rate (S3c); starts at the device rate.
    export_sample_rate: u32,
    /// Whether the export also writes one file per track (S3c).
    export_stems: bool,
    /// Whether the Undo History panel is open (S1-4). View state only.
    undo_history_open: bool,
    /// Live output setup (S1-5): persisted to `UiState` and re-applied by
    /// rebuilding the engine when the user changes it.
    output_config: OutputConfig,
    /// Whether the Preferences window is open.
    preferences_open: bool,
    /// Pending edits inside Preferences; only Apply commits them.
    preferences_draft: OutputConfig,
    /// Output-device names shown by Preferences (enumerated when the window
    /// opens — enumeration opens the driver, so never per frame).
    preferences_devices: Vec<String>,
    /// Why the last Apply failed (shown in the Preferences window so a bad
    /// pick does not silently keep the old device).
    preferences_error: Option<String>,
    /// Last output-stream error already reported, to avoid re-showing the same
    /// dead-stream message every frame.
    last_stream_error: Option<String>,
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

/// Writes captured plugin state blobs into `project` (S3a: the manual save
/// applies them to the live document, the auto-save snapshot to a throwaway
/// clone — a snapshot must not dirty the document it copies).
fn apply_plugin_state(project: &mut Project, blobs: Vec<(u64, Vec<u8>)>) {
    for (doc_id, blob) in blobs {
        let Some(node) = project.graph.node_by_id_mut(doc_id) else {
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

impl GlitcherZApp {
    /// Build the window app, restore persisted recent files, and push the whole project document
    /// down via incremental graph building + the command queue (takes effect at the next audio
    /// block boundary).
    pub fn new(cc: &eframe::CreationContext<'_>, engine: AudioEngine, config: UiConfig) -> Self {
        // CJK glyphs come from the embedded fallback face; without it every
        // non-Latin track/clip name renders as tofu (module `fonts`).
        fonts::install(&cc.egui_ctx);
        let ui_state: UiState = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, UI_STATE_KEY))
            .unwrap_or_default();
        // S3a: a snapshot left on disk means the previous run died with
        // unsaved work (a clean save or exit removes it). Collect the
        // candidates before `ui_state` is taken apart below.
        let autosave_app_dir = eframe::storage_dir("GlitcherZ");
        let recovery = autosave::candidates(&ui_state.recent_files, autosave_app_dir.as_deref());
        // U9 E2: the plugin catalog restores from storage and plans the
        // startup scan (a warm cache usually leaves nothing to do).
        let cached_plugins = cc
            .storage
            .and_then(|storage| storage.get_string(PLUGIN_CACHE_KEY));
        // S1-5: a persisted output choice is applied before the window shows.
        // Only an explicit preference triggers a reopen; the plain default
        // launch keeps the engine handed in by `main`.
        let output_config = ui_state.output_config();
        let wants_explicit_output = output_config.device.is_some()
            || output_config.sample_rate.is_some()
            || output_config.buffer_size.is_some();
        let (engine, startup_audio_error) = if wants_explicit_output
            && !output_config_matches(engine.info(), &output_config)
        {
            match AudioEngine::start_with(output_config.clone()) {
                Ok(started) => {
                    drop(engine);
                    (started, None)
                }
                Err(err) => (
                    engine,
                    Some(format!(
                        "Saved audio settings unavailable ({err}); using the default output device."
                    )),
                ),
            }
        } else {
            (engine, None)
        };
        // S3c: the export dialog starts at the device rate; the engine moves
        // into the app below, so read it here.
        let device_sample_rate = engine.info().sample_rate;
        let mut app = Self {
            engine,
            session: glitcherz_command::Session::new(Project {
                master_gain: config.gain.clamp(0.0, 1.5) as f32,
                ..Project::default()
            }),
            node_map: HashMap::new(),
            selected_track: 0,
            selected_auto: 0,
            selected_audio: 0,
            timeline_state: timeline::TimelineState::default(),
            edit_clip: None,
            edit_selection: Vec::new(),
            curve_tool: CurveTool::default(),
            curve_drag: None,
            event_list_open: true,
            roll_state: piano_roll::RollState::new(),
            focus_router: focus::FocusRouter::new(),
            patch_state: patch::PatchState::default(),
            mixer_strip_view: None,
            mixer_strip_h: MIXER_STRIP_MIN_H,
            channel_rename: None,
            view: MainView::default(),
            octave_base: 60,
            vk_targets: Vec::new(),
            vk_track_targets: Vec::new(),
            midi_hub: glitcherz_engine::MidiInputHub::new(),
            midi_ports: Vec::new(),
            midi_bindings: Vec::new(),
            midi_missing: Vec::new(),
            held_notes: HashSet::new(),
            current_note: None,
            meter_l: PeakMeter::new(),
            meter_r: PeakMeter::new(),
            opened_at: Instant::now(),
            auto_close_after: config.auto_close_after,
            project_path: None,
            recent: ui_state.recent_files,
            recovery,
            autosave_app_dir,
            autosave_clock: autosave::Clock::new(Instant::now()),
            missing_media: Vec::new(),
            status_message: None,
            record_refusal: None,
            record_started_at: None,
            audio_device: ui_state.input_device.clone(),
            audio_loopback: ui_state.input_loopback,
            audio_sources: Vec::new(),
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
            effect_meters: HashMap::new(),
            clap_voice_params: HashMap::new(),
            clap_voice_values: HashMap::new(),
            vst3_voice_values: HashMap::new(),
            clap_effect_params: HashMap::new(),
            clap_effect_values: HashMap::new(),
            vst3_effect_params: HashMap::new(),
            vst3_effect_values: HashMap::new(),
            clap_param_filter: String::new(),
            vst3_param_filter: String::new(),

            voice_default_state: HashMap::new(),
            voice_preset_selected: HashMap::new(),
            preset_name_input: String::new(),
            sf2_libraries: Vec::new(),
            sf2_preset_names: HashMap::new(),
            blocking_job: None,
            sample_library: SampleLibrary::new(),
            plugin_editor: None,
            plugin_editor_wants_open: false,
            editor_user_open: false,
            export_job: None,
            metronome_on: false,
            metronome_gain: 0.6,
            metronome_pending: false,
            recorder: recorder::MidiRecorder::new(),
            capture_scratch: Vec::new(),
            record_pre_roll_bars: 1,
            record_mute_other: false,
            capture_pending: None,
            capture_overflow_seen: 0,
            take_events: 0,
            take_samples: 0,
            take_points: 0,
            control_recorder: recorder::ControlRecorder::new(),
            control_scratch: Vec::new(),
            audio_take: None,
            audio_scratch: Vec::new(),
            record_latency_samples: 0,
            input_dropped_seen: 0,
            export_dialog_open: false,
            export_range_choice: export_audio::ExportRange::FullSong,
            export_bits: export_audio::Bits::Int24,
            export_sample_rate: device_sample_rate,
            export_stems: false,
            undo_history_open: false,
            output_config,
            preferences_open: false,
            preferences_draft: OutputConfig::default(),
            preferences_devices: Vec::new(),
            preferences_error: None,
            last_stream_error: None,
            first_frame_logged: false,
            close_sent: false,
        };
        // R4/R9: honour the persisted recording source. A picker that forgets
        // the pick every launch is worse than none; a missing device (or an
        // output endpoint that got unplugged) falls back to the system default
        // instead of leaving the user without an input.
        if app.audio_device.is_some() {
            let source = app.record_source();
            if let Err(err) = app.engine.reopen_input(&source) {
                log::warn!(
                    "Saved recording source unavailable, falling back to the system default: {err}"
                );
                app.audio_device = None;
                app.audio_loopback = false;
                if let Err(err) = app.engine.reopen_input(&RecordSource::Default) {
                    log::warn!("audio input unavailable: {err}");
                }
            }
        }
        app.audio_sources = AudioEngine::record_sources();
        if let Some(err) = startup_audio_error {
            app.set_error(err);
        }
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
            // Startup (`--open`): run it inline. There is no window to update
            // yet, so the blocking banner would buy nothing — and opening
            // here keeps the first painted frame the real project.
            app.do_open_now(path);
        }
        // R3: one backend enumeration at startup so a project that already
        // binds a device reconnects before the first frame. A failure here is
        // not fatal — a machine without MIDI support just has no ports.
        if let Err(err) = app.refresh_midi_ports() {
            log::warn!("MIDI input unavailable: {err}");
        }
        app
    }

    /// Re-enumerates hardware MIDI input ports (startup, and the Inspector's
    /// refresh button). Errors are reported in the status bar rather than
    /// propagated: no MIDI backend is a degraded, not a broken, app.
    fn refresh_midi_ports(&mut self) -> Result<(), glitcherz_engine::MidiInputError> {
        let result = self.midi_hub.refresh_ports();
        self.midi_ports = self.midi_hub.ports().to_vec();
        log::info!("MIDI input ports: {:?}", self.midi_ports);
        // Force the next sync to re-evaluate: a port that just appeared may
        // satisfy a binding that was reported missing.
        self.midi_bindings.clear();
        result
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
        let before = self.session.project_arc();
        let outcome = self.session.execute(id, params)?;
        self.sync_engine(&before);
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

    /// Reverts the last document edit (Ctrl+Z).
    ///
    /// Only command-recorded steps are undoable; edits made through
    /// [`GlitcherZApp::project_mut`] are not in the history yet.
    pub fn undo(&mut self) {
        // Undo/redo invalidates every held index: drag, selection, marquee.
        self.roll_state.clear_gesture();

        let before = self.session.project_arc();
        if self.session.undo() {
            self.sync_engine(&before);
        } else {
            self.set_message("Nothing to undo".to_string());
        }
    }

    /// Reapplies the last undone edit (Ctrl+Y).
    pub fn redo(&mut self) {
        // Undo/redo invalidates every held index: drag, selection, marquee.
        self.roll_state.clear_gesture();

        let before = self.session.project_arc();
        if self.session.redo() {
            self.sync_engine(&before);
        } else {
            self.set_message("Nothing to redo".to_string());
        }
    }

    /// Jumps to a state picked in the Undo History panel (S1-4).
    ///
    /// Same contract as [`Self::undo`]: held indices are invalidated and the
    /// engine follows the restored document (incremental when the graph did
    /// not change).
    fn jump_history_to(&mut self, depth: usize) {
        self.roll_state.clear_gesture();
        let before = self.session.project_arc();
        if self.session.jump_to(depth) {
            self.sync_engine(&before);
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
        self.status_message = Some((text, Instant::now(), MessageKind::Info));
    }

    /// Set a **failure** status message: red, and up twice as long as an
    /// info line. Silent failures were the norm before this — a load that
    /// returned early only left a one-frame info label behind.
    fn set_error(&mut self, text: String) {
        self.status_message = Some((text, Instant::now(), MessageKind::Error));
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
                input_device: self.audio_device.clone(),
                input_loopback: self.audio_loopback,
                output_device: self.output_config.device.clone(),
                output_sample_rate: self.output_config.sample_rate,
                output_buffer_size: self.output_config.buffer_size,
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
        // R3: hardware input ports follow the same rule — their callbacks read
        // a routing snapshot rebuilt from the document.
        self.sync_midi_bindings();

        // U2: an open unsaved-changes modal owns the keyboard (its Escape
        // cancels); the transport / note shortcuts stay out of the way. The
        // same holds for the plugin popup (U9 E3): it owns ↑↓/Enter/Esc, and
        // for the startup recovery prompt (S3a), which must be answered first.
        if self.pending_file_action.is_none() && !self.picker.is_open() && self.recovery.is_empty()
        {
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

        // U6: release displaced audio clip tables (control-thread drops of
        // the old `Arc<SampleData>` refcounts; same frame cadence as the
        // node recycling above).
        self.engine.reclaim_audio_clips();
        // D15 revision: same cadence for displaced MIDI clip tables (their
        // `Vec`s must be released on the control thread).
        self.engine.reclaim_midi_clips();

        // R2: recording housekeeping (spec `recording.md`) — drain the capture
        // tap and advance pre-roll → recording. Runs before the transport
        // edges below, which may close the take.
        self.poll_recording();

        // Transport stop edge: automation no longer overrides; restore manual parameter values
        // per node.
        let playing = self.engine.is_playing();
        if self.was_playing && !playing {
            // The transport can stop on its own (arrangement end); a take must
            // still be written, using the last position it observed.
            if self.is_recording() {
                self.finish_recording();
            }
            self.push_node_params();
        }
        self.was_playing = playing;

        // A take whose transport never rolled must not sit silently in
        // pre-roll (the user reads that as "Rec did nothing"). Give the engine
        // a second to pick the Play up, then say so.
        if self
            .record_started_at
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(1))
        {
            let stalled = self.recorder.is_active() && !playing;
            self.record_started_at = None;
            if stalled {
                self.recording_problem(
                    "Recording is armed, but the transport did not start playing — the audio \
                     engine is not running (check the output device)."
                        .to_string(),
                );
            }
        }

        // U9 E2: the plugin-scan banner sits above the transport while the
        // startup scan has modules left (it names the module the frame's tail
        // will block on, spec D14). The U7 export banner shares the slot.
        self.plugin_scan_banner(ui);
        self.export_banner(ui);
        // U13: same slot for a UI-blocking job (a SoundFont parse the user
        // asked for). It must draw before the panels so the paint that lands
        // after `ui()` returns already shows what the window is busy with.
        self.blocking_banner(ui);
        self.transport_panel(ui);
        self.recording_refusal_card(ui);
        self.undo_history_panel(ui.ctx());
        // S1-5: a dead output stream would otherwise look exactly like silence.
        if let Some(err) = self.engine.take_stream_error() {
            self.set_error(format!(
                "Audio output stopped: {err}. Open Edit ▸ Preferences to pick another device."
            ));
            self.last_stream_error = Some(err);
        }
        self.preferences_panel(ui.ctx());
        self.status_panel(ui);
        self.view_tabs(ui);
        // The Edit tab follows the timeline's clip selection (no target of its
        // own), so the target is re-derived before the view that consumes it.
        self.sync_edit_target();

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
            MainView::Inspector => self.inspector_panel(ui),
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
            if self.view != MainView::Inspector {
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

        // S3a: the crash-recovery prompt. Drawn first so a snapshot left by the
        // previous run is the first thing the user answers.
        self.recovery_dialog(ui.ctx());
        // S3b: media the opened project cannot find.
        self.missing_media_dialog(ui.ctx());

        // U7: the export range modal rides above every panel (the
        // unsaved-changes precedent).
        self.export_dialog(ui.ctx());

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

        // U7: the offline export pumps here too (same end-of-frame slot),
        // and wants repaints while it runs so the progress bar moves.
        self.pump_export();
        if self.export_job.is_some() {
            ui.ctx().request_repaint();
        }

        // U13: the blocking job advances last — its `run` phase is the one
        // that stops the window (nothing paints until `ui()` returns, which
        // is exactly why the banner was drawn two frames ago).
        if self.run_blocking_job() || self.blocking_job.is_some() {
            ui.ctx().request_repaint();
        }

        // S3a: crash snapshot last — after every panel, so a long write lands
        // in the quiet part of the frame. Disk IO on the UI thread only.
        self.tick_autosave(Instant::now());

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
        // S3a: a clean document leaves nothing to recover, so a plain window
        // close (which never goes through the unsaved-changes modal) must not
        // leave a stale snapshot behind. A **dirty** document keeps its
        // snapshot on purpose: reaching Drop with unsaved edits means the run
        // is ending without the user's answer, which is exactly the case the
        // snapshot exists for.
        if !self.session.is_dirty() {
            self.clear_autosave();
        }
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
mod audio_preferences_tests {
    use super::*;

    fn info(device: &str, rate: u32, buffer: Option<u32>) -> glitcherz_engine::AudioEngineInfo {
        glitcherz_engine::AudioEngineInfo {
            device_name: device.to_string(),
            sample_rate: rate,
            out_channels: 2,
            buffer_frames: buffer,
        }
    }

    #[test]
    fn an_empty_config_matches_the_default_engine() {
        assert!(output_config_matches(
            &info("Speakers", 48_000, None),
            &OutputConfig::default()
        ));
    }

    #[test]
    fn explicit_fields_must_match_the_live_engine() {
        let live = info("Speakers", 48_000, Some(256));
        assert!(output_config_matches(
            &live,
            &OutputConfig {
                device: Some("Speakers".into()),
                sample_rate: Some(48_000),
                buffer_size: Some(256),
            }
        ));
        assert!(!output_config_matches(
            &live,
            &OutputConfig {
                device: Some("Headphones".into()),
                ..Default::default()
            }
        ));
        assert!(!output_config_matches(
            &live,
            &OutputConfig {
                sample_rate: Some(44_100),
                ..Default::default()
            }
        ));
        assert!(!output_config_matches(
            &live,
            &OutputConfig {
                buffer_size: Some(512),
                ..Default::default()
            }
        ));
    }

    #[test]
    fn ui_state_roundtrips_the_output_settings() {
        let state = UiState {
            output_device: Some("Speakers".into()),
            output_sample_rate: Some(96_000),
            output_buffer_size: Some(256),
            ..Default::default()
        };
        let text = ron::ser::to_string(&state).expect("serialize UiState");
        let back: UiState = ron::from_str(&text).expect("deserialize UiState");
        assert_eq!(back.output_config(), state.output_config());
    }
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
        assert_eq!(resolve_panel_target(doc, &[], false, 0), Some(t0));
    }

    #[test]
    fn selection_wins_over_track_fallback() {
        let mut project = Project::default();
        let t1 = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Instrument {
            node_id: t1,
            track: 1,
            voice: VoiceSpec::Builtin,
            synth: SynthParams::default(),
        });
        let doc = &project.graph;
        // Cross-track patch target: the selection addresses the panel.
        assert_eq!(resolve_panel_target(doc, &[t1], true, 0), Some(t1));
    }

    /// R4: the live input node owns a panel (device picker + channel meter),
    /// so selecting it resolves to the node, and a double-click opens it.
    #[test]
    fn input_node_is_an_editable_target() {
        let mut project = Project::default();
        let input = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Input {
            node_id: input,
            channel: 1,
            gain: 0.5,
        });
        let doc = &project.graph;
        assert_eq!(resolve_panel_target(doc, &[input], true, 0), Some(input));
        assert!(patch::opens_inspector(doc.node_by_id(input)));
    }

    #[test]
    fn effect_and_mixer_nodes_are_editable_targets() {
        let mut project = Project::default();
        let delay = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Effect {
            node_id: delay,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: EffectParams::default(),
            delay: None,
        });
        let doc = &project.graph;
        let mixer = doc.first_mixer().unwrap().node_id();
        assert_eq!(resolve_panel_target(doc, &[delay], true, 0), Some(delay));
        assert_eq!(resolve_panel_target(doc, &[mixer], true, 0), Some(mixer));
    }

    #[test]
    fn stale_ids_are_filtered_and_fall_back() {
        let project = Project::default();
        let doc = &project.graph;
        let t0 = doc.instrument_of_track(0).unwrap().node_id();
        // Deleted node ids never resolve; the walk falls back to the track.
        assert_eq!(resolve_panel_target(doc, &[9999], true, 0), Some(t0));
        // No editable node on a nonexistent track → no target.
        assert_eq!(resolve_panel_target(doc, &[], false, 99), None);
    }

    /// C3 regression: the built-in MIDI FX / control panels (and the Curve
    /// Map, whose only UI is its Inspector) must be reachable by selecting
    /// the node — the panel used to resolve straight past them to the track
    /// instrument.
    #[test]
    fn control_nodes_are_editable_targets() {
        let mut project = Project::default();
        let map = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::CurveMap {
            node_id: map,
            track: 0,
            params: CurveMapParams::default(),
        });
        let env = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::MidiEnvelope {
            node_id: env,
            track: 0,
            params: MidiEnvelopeParams::default(),
        });
        let doc = &project.graph;
        assert_eq!(resolve_panel_target(doc, &[map], true, 0), Some(map));
        assert_eq!(resolve_panel_target(doc, &[env], true, 0), Some(env));
    }

    #[test]
    fn document_order_breaks_multi_selection_ties() {
        let mut project = Project::default();
        let doc = &mut project.graph;
        let t0 = doc.instrument_of_track(0).unwrap().node_id();
        let t1 = doc.next_node_id();
        doc.nodes.push(NodeDoc::Instrument {
            node_id: t1,
            track: 1,
            voice: VoiceSpec::Builtin,
            synth: SynthParams::default(),
        });
        let doc = &project.graph;
        // Selection order is irrelevant: document order decides.
        assert_eq!(resolve_panel_target(doc, &[t1, t0], true, 0), Some(t0));
        assert_eq!(resolve_panel_target(doc, &[t0, t1], true, 0), Some(t0));
    }

    /// The timeline / master entries are not document nodes at all
    /// (`selected_doc_nodes` filters them out). Selecting one must show that
    /// node's empty panel, not the track instrument.
    #[test]
    fn timeline_selection_does_not_fall_back() {
        let project = Project::default();
        let doc = &project.graph;
        assert_eq!(resolve_panel_target(doc, &[], true, 0), None);
    }

    /// A real but non-editable node owns the panel too: the instrument
    /// parameters belong to the instrument node, not to the track.
    #[test]
    fn non_editable_node_does_not_fall_back() {
        let mut project = Project::default();
        let keyboard = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Keyboard {
            node_id: keyboard,
            track: 0,
            source: glitcherz_model::MidiSource::default(),
        });
        let doc = &project.graph;
        assert_eq!(resolve_panel_target(doc, &[keyboard], true, 0), None);
    }

    /// Track-level extras follow the same rule: only an empty selection or
    /// the track instrument shows the built-in delay / external effects.
    #[test]
    fn extra_panels_follow_the_selection() {
        let instrument = Some(NodeDoc::Instrument {
            node_id: 2,
            track: 0,
            voice: VoiceSpec::Builtin,
            synth: SynthParams::default(),
        });
        let lfo = Some(NodeDoc::Lfo {
            node_id: 5,
            track: 0,
            params: LfoParams::default(),
        });
        assert!(panel_shows_track_extras(&instrument, false));
        assert!(!panel_shows_track_extras(&lfo, true));
        assert!(!panel_shows_track_extras(&None, true), "timeline selected");
        assert!(panel_shows_track_extras(&None, false), "nothing selected");
    }
}
#[cfg(test)]
mod inline_param_tests {
    use super::*;

    /// Spec `instrument.md`: the live push and the offline export engine walk
    /// one table, and every timbre field of the document payload is on it.
    #[test]
    fn synth_param_table_covers_the_document_payload() {
        let synth = SynthParams {
            waveform: SynthWaveform::Square,
            cutoff_hz: 3_000.0,
            resonance: 0.6,
            filter_mode: 2.0,
            attack_s: 0.02,
            decay_s: 0.3,
            sustain: 0.4,
            release_s: 0.7,
        };
        assert_eq!(
            synth_param_values(&synth),
            [
                (ParamId::OscWaveform, 3.0),
                (ParamId::FilterCutoff, 3_000.0),
                (ParamId::FilterResonance, 0.6),
                (ParamId::FilterMode, 2.0),
                (ParamId::Attack, 0.02),
                (ParamId::Decay, 0.3),
                (ParamId::Sustain, 0.4),
                (ParamId::Release, 0.7),
            ]
        );
    }

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
            inline_param_id(patch::InlineParam::EffectSlot(1)),
            ParamId::BuiltinParam(1)
        );
        assert_eq!(
            inline_param_id(patch::InlineParam::MixerOutPan),
            ParamId::MixerOutPan
        );
        assert_eq!(
            inline_param_id(patch::InlineParam::FeedbackGain),
            ParamId::FeedbackGain
        );
        // U5: sample inline rows map to the sample processor's parameters.
        assert_eq!(
            inline_param_id(patch::InlineParam::SampleRoot),
            ParamId::SampleRootNote
        );
        assert_eq!(
            inline_param_id(patch::InlineParam::SampleGain),
            ParamId::SampleGain
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

    /// U4c: every node parameter address has an engine counterpart — a
    /// missing arm would make the curve a silent no-op, which is the exact
    /// failure mode this feature exists to fix.
    #[test]
    fn node_param_addresses_map_to_engine_params() {
        let cases = [
            (NodeParam::EffectSlot(2), ParamId::BuiltinParam(2)),
            (
                NodeParam::MixerChannel {
                    channel: 5,
                    param: ChannelParam::Gain,
                },
                ParamId::MixerGain(5),
            ),
            (
                NodeParam::MixerChannel {
                    channel: 5,
                    param: ChannelParam::Pan,
                },
                ParamId::MixerPan(5),
            ),
            (
                NodeParam::MixerChannel {
                    channel: 1,
                    param: ChannelParam::Mute,
                },
                ParamId::MixerMute(1),
            ),
            (
                NodeParam::MixerChannel {
                    channel: 1,
                    param: ChannelParam::Solo,
                },
                ParamId::MixerSolo(1),
            ),
            (NodeParam::MixerOutPan, ParamId::MixerOutPan),
            (NodeParam::MixerOutMute, ParamId::MixerOutMute),
            (NodeParam::FeedbackGain, ParamId::FeedbackGain),
            (NodeParam::Sf2Gain, ParamId::Sf2Gain),
            (NodeParam::SampleGain, ParamId::SampleGain),
            (NodeParam::Resonance, ParamId::FilterResonance),
            (NodeParam::Attack, ParamId::Attack),
            (NodeParam::Decay, ParamId::Decay),
            (NodeParam::Sustain, ParamId::Sustain),
            (NodeParam::Release, ParamId::Release),
        ];
        for (param, expected) in cases {
            assert_eq!(param_id_of(&AutoParam::Node(param)), Some(expected));
        }
    }

    /// C3 (spec `curve-map.md` D5/§5): the map node offers exactly one target
    /// — its own pass-through port — and the row says what that port means.
    #[test]
    fn a_curve_map_offers_its_single_pass_through_port() {
        let mut project = Project::default();
        let id = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::CurveMap {
            node_id: id,
            track: 0,
            params: CurveMapParams::default(),
        });
        let options = node_auto_options(&project.graph, id, None, None);
        assert_eq!(options.len(), 1, "{options:?}");
        assert_eq!(options[0].1, AutoParam::CurveMap);
        // `±1` is the signal's own domain, not a musical axis (D1).
        assert_eq!(options[0].0, "Curve In · ±1");
        // And it routes to the engine's pass-through slot, not a parameter.
        assert_eq!(param_id_of(&AutoParam::CurveMap), Some(ParamId::CurveMapIn));
    }

    /// Spec `curve-map.md` D2/D6 (revised 2026-10-30): a cable never stores a
    /// *copy* of its axis. `None` derives it from the target, and a Curve Map
    /// source is read live from its node, so the hop follows a range change
    /// instead of keeping a stale one.
    #[test]
    fn a_cable_derives_its_axis_instead_of_storing_it() {
        let mut project = Project::default();
        let map_id = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::CurveMap {
            node_id: map_id,
            track: 0,
            params: glitcherz_model::CurveMapParams {
                min: 20.0,
                max: 18_000.0,
                mode: glitcherz_model::MapMode::Log,
            },
        });
        let voice = project
            .graph
            .nodes
            .iter()
            .find(|node| matches!(node, NodeDoc::Instrument { .. }))
            .map(NodeDoc::node_id)
            .expect("the default patch ships an instrument");
        project.graph.cables.push(glitcherz_model::CableDoc {
            kind: glitcherz_model::CableKind::Automation,
            from: PortRef::Node(map_id),
            to: PortRef::Node(voice),
            param: AutoParam::FilterCutoff,
            in_port: 0,
            map: None,
        });
        let cable = project.graph.cables.last().unwrap();
        assert_eq!(
            cable_map(cable, &project.graph),
            glitcherz_model::MapSpec::log(20.0, 18_000.0)
        );

        // Edit the node's range: the hop moves with it, no re-wiring.
        if let Some(NodeDoc::CurveMap { params, .. }) = project
            .graph
            .nodes
            .iter_mut()
            .find(|node| matches!(node, NodeDoc::CurveMap { .. }))
        {
            params.max = 4_000.0;
        }
        let cable = project.graph.cables.last().unwrap();
        assert_eq!(
            cable_map(cable, &project.graph),
            glitcherz_model::MapSpec::log(20.0, 4_000.0)
        );

        // A plain target-axis cable derives from the target too, so changing
        // the parameter changes the axis without touching the cable.
        let cable = project.graph.cables.last_mut().unwrap();
        cable.from = PortRef::AutoTrack(0);
        let cable = project.graph.cables.last().unwrap();
        assert_eq!(
            cable_map(cable, &project.graph),
            glitcherz_model::MapSpec::log(20.0, 18_000.0),
            "cutoff keeps its own log axis"
        );
        project.graph.cables.last_mut().unwrap().param = AutoParam::TrackPan;
        let cable = project.graph.cables.last().unwrap();
        assert_eq!(
            cable_map(cable, &project.graph),
            glitcherz_model::MapSpec::linear(-1.0, 1.0)
        );
    }

    /// U4c (spec §9 D19): the target menu enumerates the node's real
    /// parameters — this is where "gain cannot be automated" came from, so a
    /// mixer must offer one Gain row per channel, and a built-in effect every
    /// descriptor slot.
    #[test]
    fn auto_options_cover_mixer_channels_and_effect_slots() {
        let mut project = Project::default();
        let delay_id = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Effect {
            node_id: delay_id,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: EffectParams::default(),
            delay: None,
        });
        let doc = &project.graph;
        let mixer = doc.first_mixer().expect("default mixer");
        let (NodeDoc::Mixer { inputs, .. }, mixer_id) = (mixer, mixer.node_id()) else {
            unreachable!()
        };
        let labels: Vec<String> = node_auto_options(doc, mixer_id, None, None)
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        let channels = *inputs as usize;
        assert_eq!(
            labels.len(),
            channels * 4 + 2,
            "four rows per channel + output pan/mute, got {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.starts_with("Ch 1 Gain · ")),
            "rows carry their axis (spec `curve-map.md` §5): {labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|l| l.starts_with(&format!("Ch {channels} Solo · "))),
            "the last channel must be offered too: {labels:?}"
        );

        // A built-in effect exposes its whole descriptor, not just the inline
        // rows on the node body.
        let delay = doc.node_by_id(delay_id).expect("explicit track delay");
        let options = node_auto_options(doc, delay.node_id(), None, None);
        let NodeDoc::Effect { params, .. } = delay else {
            unreachable!()
        };
        let slots = crate::effect::descriptor(params).params.len();
        assert_eq!(
            options.len(),
            slots,
            "one row per descriptor slot: {options:?}"
        );
        assert!(
            options
                .iter()
                .any(|(_, param)| matches!(param, AutoParam::Node(NodeParam::EffectSlot(0))))
        );

        // Built-in synth: cutoff + resonance + the four envelope stages.
        let synth = doc.instrument_of_track(0).expect("track instrument");
        let labels: Vec<String> = node_auto_options(doc, synth.node_id(), None, None)
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert_eq!(labels.len(), 6, "{labels:?}");
        assert!(
            labels.iter().any(|l| l.starts_with("Cutoff · ")),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.starts_with("Env Release · ")),
            "{labels:?}"
        );
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
            NodeSource::Builtin(glitcherz_model::BuiltinEffectKind::Delay),
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

/// U13: the preset fallback chain and the size readout used by the loading
/// banner. Both are pure, so they are tested without a window or a font.
#[cfg(test)]
mod sf2_tests {
    use super::*;

    fn preset(bank: u8, program: u8, name: &str) -> Sf2Preset {
        Sf2Preset {
            bank,
            program,
            name: name.to_string(),
        }
    }

    /// The old address wins when the new font still has it.
    #[test]
    fn resolve_keeps_a_preset_that_exists() {
        let presets = vec![preset(0, 0, "Piano"), preset(0, 40, "Violin")];
        assert_eq!(resolve_sf2_preset(&presets, 0, 40), (0, 40));
    }

    /// Same bank, different program: take the bank's first patch rather than
    /// falling through to GM 0:0 — the bank carries the user's intent (a drum
    /// kit bank must not resolve to a piano).
    #[test]
    fn resolve_prefers_the_same_bank() {
        let presets = vec![
            preset(0, 0, "Piano"),
            preset(8, 24, "Kit A"),
            preset(8, 0, "Kit B"),
        ];
        assert_eq!(resolve_sf2_preset(&presets, 8, 40), (8, 24));
    }

    /// Bank gone entirely: GM 0:0 if the font has it.
    #[test]
    fn resolve_falls_back_to_general_midi() {
        let presets = vec![preset(4, 2, "Pad"), preset(0, 0, "Piano")];
        assert_eq!(resolve_sf2_preset(&presets, 127, 12), (0, 0));
    }

    /// A font with neither the bank nor 0:0 still yields a playable address.
    #[test]
    fn resolve_never_returns_an_empty_address() {
        let presets = vec![preset(3, 7, "Only")];
        assert_eq!(resolve_sf2_preset(&presets, 9, 9), (3, 7));
        assert_eq!(resolve_sf2_preset(&[], 9, 9), (0, 0));
    }

    #[test]
    fn human_bytes_reads_like_a_file_manager() {
        assert_eq!(human_bytes(0), "1 KB");
        assert_eq!(human_bytes(900 * 1024), "900 KB");
        assert_eq!(human_bytes(240 * 1024 * 1024), "240 MB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0 GB");
    }

    /// The audio take's pre-roll skip is a tick span, not a sample count:
    /// one 4/4 bar at 120 bpm is 3840 ticks = 2 s = 96 000 frames at 48 kHz.
    /// Getting this wrong (ticks × rate) swallowed every take whole.
    #[test]
    fn pre_roll_skip_converts_ticks_to_frames() {
        let sec_per_tick = 60.0 / (120.0 * f64::from(glitcherz_model::TICKS_PER_QUARTER));
        assert_eq!(pre_roll_skip_frames(3840.0, sec_per_tick, 48_000), 96_000);
        assert_eq!(pre_roll_skip_frames(1920.0, sec_per_tick, 48_000), 48_000);
        assert_eq!(pre_roll_skip_frames(0.0, sec_per_tick, 48_000), 0);
        // A half-bar pre-roll is a quarter of a bar's frames, not a million.
        assert!(pre_roll_skip_frames(3840.0, sec_per_tick, 48_000) < 200_000);
    }
}
