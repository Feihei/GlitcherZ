#![forbid(unsafe_code)]
// The model is pure data and must never panic: a panic inside a command is
// caught by glitcherz-command and rolled back, but preventing it here is cheaper.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

//! # glitcherz-model
//!
//! GlitcherZ data model: MIDI tracks, automation tracks, the node graph, and the
//! project file (document layer).
//!
//! Conventions (see AGENTS.md "Data Model"):
//! - All types must be `Serialize + Deserialize` (RON format);
//! - New fields always get `#[serde(default)]` to keep old project files
//!   forward compatible;
//! - The model is the "document", the engine is the "player": this crate does
//!   not depend on `glitcherz-engine`, and the engine does not depend on this
//!   crate either -- `glitcherz-app` / `glitcherz-ui` do the conversion glue.
//!
//! v4 (spec `.agents/spec/timeline.md` §4, D14-D15):
//! - `MidiTrack { name, clips: Vec<MidiClip> }` — a track holds up to
//!   [`MAX_CLIPS_PER_TRACK`] clips placed on the arrangement timeline
//!   (absolute `start_ticks`, `span_ticks`, content loops every
//!   `loop_ticks` within the span);
//! - `AutomationTrack` becomes `ControlTrack { name, clips:
//!   Vec<ControlClip> }`, same clip placement semantics;
//! - the step sequencer is retired (`Step` / `Pattern` / `TrackSource` /
//!   per-track single `Clip` / project `gate` are gone);
//! - validation rejects over-capacity documents (report, never truncate).
//!
//! v3 (spec `.agents/spec/node-graph.md` §7, D11-D13, unchanged in v4):
//! - `GraphDoc{nodes, cables}` fully explicit: the default topology is also
//!   written into the document, and the loader rebuilds the engine graph
//!   purely from the document (D13);
//! - `node_id` is persisted with the file (D11); id 0 is invalid,
//!   id 1 = master output reserved ([`MASTER_NODE_ID`]), document nodes
//!   start from 2.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub mod file;
pub mod ops;
pub mod smf;

pub use file::{
    ModelError, PROJECT_VERSION, ProjectFile, load_project, parse_project, save_project,
};
pub use ops::{
    DEFAULT_SEED, DurationOp, QuantizeOptions, VelocityOp, apply_duration, apply_to_selected,
    apply_velocity, quantize, scale_notes, transpose,
};

/// MIDI track capacity limit (D12; the engine side uses the same value).
pub const MAX_MIDI_TRACKS: usize = 16;

/// Automation track capacity limit (D12; the engine-side `MAX_AUTO_TRACKS`
/// uses the same value).
pub const MAX_AUTO_TRACKS: usize = 16;

/// Audio track capacity (U6). Capped well below the mixer channel budget
/// so MIDI + audio tracks can coexist on the default single mixer.
pub const MAX_AUDIO_TRACKS: usize = 8;

/// Node graph capacity limit (D1; same value on the engine side).
pub const MAX_NODES: usize = 64;

/// Cable capacity limit (D1; same value on the engine side; P5f: 256 — a
/// full 32-channel mixer consumes 32 cables on its own).
pub const MAX_CABLES: usize = 256;

/// Mixer node capacity limits (P5f D15/D21): channel count 2..=32.
pub const MIN_MIXER_CHANNELS: usize = 2;
pub const MAX_MIXER_CHANNELS: usize = 32;

/// Clips per track capacity limit (D15; the engine side uses the same value).
pub const MAX_CLIPS_PER_TRACK: usize = 8;

/// Notes per MIDI clip capacity limit (D15; the engine side
/// `MAX_CLIP_NOTES` uses the same value).
pub const MAX_CLIP_NOTES: usize = 64;

/// Anchor points per control clip capacity limit (the engine-side
/// `MAX_AUTO_POINTS` uses the same value).
pub const MAX_CURVE_POINTS: usize = 64;

/// Arrangement length safety bound in bars (D16; guards pathological
/// documents).
pub const MAX_ARRANGEMENT_BARS: u32 = 256;

/// Number of MIDI tracks in the default (new) project.
pub const DEFAULT_MIDI_TRACKS: usize = 4;

/// Reserved node_id of the master output (D6: always present, cannot be
/// deleted, not listed in the document node table).
pub const MASTER_NODE_ID: u64 = 1;

/// Default note velocity (used by serde field defaults).
///
/// Velocities are integers 1..=127, matching the MIDI wire format; the audio
/// thread divides by 127 when synthesizing.
pub const DEFAULT_VELOCITY: u8 = 100;

/// Lowest MIDI velocity (a note-off; silence).
pub const MIN_VELOCITY: u8 = 1;

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
///
/// E4 added the stereo/tempo fields; they are additive (`serde(default)`), so
/// older documents keep loading without a version bump.
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
    /// Ping-pong (cross-fed stereo) switch (E4).
    #[serde(default)]
    pub ping_pong: bool,
    /// Whether the delay time follows the project tempo (E4).
    #[serde(default)]
    pub sync: bool,
    /// Synced note-division index (see the engine's `delay::DIVISION_TICKS`).
    #[serde(default = "default_delay_division")]
    pub sync_division: u8,
}

fn default_delay_time() -> f32 {
    0.3
}

fn default_delay_feedback() -> f32 {
    0.35
}

/// Default synced division: index 2 = 1/4 note (mirrors the engine descriptor).
fn default_delay_division() -> u8 {
    2
}

impl Default for DelayParams {
    fn default() -> Self {
        Self {
            time_s: default_delay_time(),
            feedback: default_delay_feedback(),
            mix: 0.0,
            ping_pong: false,
            sync: false,
            sync_division: default_delay_division(),
        }
    }
}

/// Three-band EQ parameters (spec `effects.md` E2a; slot order matches the
/// engine descriptor).
///
/// All gains are dB (`-18..18`), the corner/center frequencies Hz, `mid_q` the
/// bell's quality factor and `low_cut` the high-pass corner. `low_cut` at
/// [`EQ_LOW_CUT_OFF_HZ`] means "no low cut".
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EqParams {
    /// Low-shelf gain (dB).
    #[serde(default)]
    pub low_gain: f32,
    /// Low-shelf corner (Hz).
    #[serde(default = "default_eq_low_freq")]
    pub low_freq: f32,
    /// Bell gain (dB).
    #[serde(default)]
    pub mid_gain: f32,
    /// Bell center (Hz).
    #[serde(default = "default_eq_mid_freq")]
    pub mid_freq: f32,
    /// Bell Q.
    #[serde(default = "default_eq_mid_q")]
    pub mid_q: f32,
    /// High-shelf gain (dB).
    #[serde(default)]
    pub high_gain: f32,
    /// High-shelf corner (Hz).
    #[serde(default = "default_eq_high_freq")]
    pub high_freq: f32,
    /// Low-cut corner (Hz); `20` means off.
    #[serde(default = "default_eq_low_cut")]
    pub low_cut: f32,
}

/// `low_cut` value that means "no low cut" (mirrors the engine constant).
pub const EQ_LOW_CUT_OFF_HZ: f32 = 20.0;

fn default_eq_low_freq() -> f32 {
    100.0
}

fn default_eq_mid_freq() -> f32 {
    1_000.0
}

fn default_eq_mid_q() -> f32 {
    1.0
}

fn default_eq_high_freq() -> f32 {
    8_000.0
}

fn default_eq_low_cut() -> f32 {
    EQ_LOW_CUT_OFF_HZ
}

impl Default for EqParams {
    fn default() -> Self {
        Self {
            low_gain: 0.0,
            low_freq: default_eq_low_freq(),
            mid_gain: 0.0,
            mid_freq: default_eq_mid_freq(),
            mid_q: default_eq_mid_q(),
            high_gain: 0.0,
            high_freq: default_eq_high_freq(),
            low_cut: default_eq_low_cut(),
        }
    }
}

/// Feed-forward peak compressor parameters (spec `effects.md` E2b; slot order
/// matches the engine descriptor).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompressorParams {
    /// Threshold (dB).
    #[serde(default = "default_comp_threshold")]
    pub threshold: f32,
    /// Compression ratio (`>= 1`).
    #[serde(default = "default_comp_ratio")]
    pub ratio: f32,
    /// Attack time (ms).
    #[serde(default = "default_comp_attack")]
    pub attack: f32,
    /// Release time (ms).
    #[serde(default = "default_comp_release")]
    pub release: f32,
    /// Soft-knee width (dB, `0` = hard knee).
    #[serde(default = "default_comp_knee")]
    pub knee: f32,
    /// Makeup gain (dB).
    #[serde(default)]
    pub makeup: f32,
    /// Dry/wet mix (`1` = fully compressed).
    #[serde(default = "default_mix_one")]
    pub mix: f32,
    /// Link both channels to one `max(|L|, |R|)` detector (D14).
    #[serde(default = "default_true")]
    pub stereo_link: bool,
}

fn default_comp_threshold() -> f32 {
    -18.0
}

fn default_comp_ratio() -> f32 {
    4.0
}

fn default_comp_attack() -> f32 {
    10.0
}

fn default_comp_release() -> f32 {
    100.0
}

fn default_comp_knee() -> f32 {
    6.0
}

fn default_mix_one() -> f32 {
    1.0
}

fn default_true() -> bool {
    true
}

impl Default for CompressorParams {
    fn default() -> Self {
        Self {
            threshold: default_comp_threshold(),
            ratio: default_comp_ratio(),
            attack: default_comp_attack(),
            release: default_comp_release(),
            knee: default_comp_knee(),
            makeup: 0.0,
            mix: default_mix_one(),
            stereo_link: default_true(),
        }
    }
}

/// Stereo FDN-8 reverb parameters (spec `effects.md` E3; slot order matches the
/// engine descriptor).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReverbParams {
    /// Stereo pre-delay (ms).
    #[serde(default)]
    pub pre_delay: f32,
    /// RT60 decay time (seconds).
    #[serde(default = "default_reverb_decay")]
    pub decay: f32,
    /// Damping (high-frequency loss in the feedback loop, 0..1).
    #[serde(default = "default_reverb_damping")]
    pub damping: f32,
    /// Low-cut corner (Hz).
    #[serde(default = "default_reverb_low_cut")]
    pub low_cut: f32,
    /// Stereo width (0 = mono, 1 = full spread).
    #[serde(default = "default_mix_one")]
    pub width: f32,
    /// Dry/wet mix (`0` = fully dry).
    #[serde(default = "default_reverb_mix")]
    pub mix: f32,
}

fn default_reverb_decay() -> f32 {
    2.0
}

fn default_reverb_damping() -> f32 {
    0.5
}

fn default_reverb_low_cut() -> f32 {
    20.0
}

fn default_reverb_mix() -> f32 {
    0.3
}

impl Default for ReverbParams {
    fn default() -> Self {
        Self {
            pre_delay: 0.0,
            decay: default_reverb_decay(),
            damping: default_reverb_damping(),
            low_cut: default_reverb_low_cut(),
            width: default_mix_one(),
            mix: default_reverb_mix(),
        }
    }
}

/// Stereo chorus parameters (spec `effects.md` E4; slot order matches the
/// engine descriptor).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ChorusParams {
    /// LFO rate (Hz).
    #[serde(default = "default_chorus_rate")]
    pub rate: f32,
    /// Modulation depth (0..1).
    #[serde(default = "default_chorus_depth")]
    pub depth: f32,
    /// Stereo width (0 = in phase, 1 = fully spread).
    #[serde(default = "default_half")]
    pub width: f32,
    /// Dry/wet mix (`0` = fully dry).
    #[serde(default = "default_half")]
    pub mix: f32,
}

fn default_chorus_rate() -> f32 {
    1.0
}

fn default_chorus_depth() -> f32 {
    0.5
}

fn default_half() -> f32 {
    0.5
}

impl Default for ChorusParams {
    fn default() -> Self {
        Self {
            rate: default_chorus_rate(),
            depth: default_chorus_depth(),
            width: default_half(),
            mix: default_half(),
        }
    }
}

/// Stereo flanger parameters (spec `effects.md` E4; slot order matches the
/// engine descriptor).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FlangerParams {
    /// LFO rate (Hz).
    #[serde(default = "default_flanger_rate")]
    pub rate: f32,
    /// Modulation depth (0..1).
    #[serde(default = "default_flanger_depth")]
    pub depth: f32,
    /// Feedback amount (`-0.95..0.95`).
    #[serde(default = "default_half")]
    pub feedback: f32,
    /// Stereo width (0 = in phase, 1 = fully spread).
    #[serde(default = "default_half")]
    pub width: f32,
    /// Dry/wet mix (`0` = fully dry).
    #[serde(default = "default_half")]
    pub mix: f32,
}

fn default_flanger_rate() -> f32 {
    0.5
}

fn default_flanger_depth() -> f32 {
    0.7
}

impl Default for FlangerParams {
    fn default() -> Self {
        Self {
            rate: default_flanger_rate(),
            depth: default_flanger_depth(),
            feedback: default_half(),
            width: default_half(),
            mix: default_half(),
        }
    }
}

/// Built-in effect kind (spec `.agents/spec/effects.md` D1).
///
/// The wire format is the `kind` tag of [`EffectParams`]; this enum lets UI and
/// engine code branch on the kind without matching every parameter field. It is
/// deliberately **not** shared with the engine's own `BuiltinEffectKind`: the
/// engine crate does not depend on this one, and the UI maps between them when
/// it installs a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinEffectKind {
    /// Feedback delay.
    Delay,
    /// Three-band EQ with a low cut.
    Eq,
    /// Feed-forward peak compressor.
    Compressor,
    /// Stereo FDN-8 reverb.
    Reverb,
    /// Stereo chorus.
    Chorus,
    /// Stereo flanger.
    Flanger,
}

impl BuiltinEffectKind {
    /// English label used by menus and node captions.
    pub fn label(self) -> &'static str {
        match self {
            Self::Delay => "Delay",
            Self::Eq => "EQ",
            Self::Compressor => "Compressor",
            Self::Reverb => "Reverb",
            Self::Chorus => "Chorus",
            Self::Flanger => "Flanger",
        }
    }

    /// Every built-in kind, in menu order.
    pub const ALL: &'static [BuiltinEffectKind] = &[
        BuiltinEffectKind::Delay,
        BuiltinEffectKind::Eq,
        BuiltinEffectKind::Compressor,
        BuiltinEffectKind::Reverb,
        BuiltinEffectKind::Chorus,
        BuiltinEffectKind::Flanger,
    ];
}

/// Built-in effect parameter payload: **the tag is the kind**, so a node can
/// never claim one kind while carrying another's parameters (D1).
///
/// Serialized as `params: (kind: "delay", time_s: 0.3, …)`. A v7 project
/// stores the delay payload in a sibling `delay` field instead; loading runs
/// `migrate_v7_to_v8` to move it here (D3/D4).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EffectParams {
    /// Feedback delay.
    Delay(DelayParams),
    /// Three-band EQ with a low cut (`Box` keeps `NodeDoc` small).
    Eq(Box<EqParams>),
    /// Feed-forward peak compressor.
    Compressor(Box<CompressorParams>),
    /// Stereo FDN-8 reverb.
    Reverb(Box<ReverbParams>),
    /// Stereo chorus.
    Chorus(Box<ChorusParams>),
    /// Stereo flanger.
    Flanger(Box<FlangerParams>),
}

impl Default for EffectParams {
    fn default() -> Self {
        Self::Delay(DelayParams::default())
    }
}

impl EffectParams {
    /// Kind of this payload.
    pub fn kind(&self) -> BuiltinEffectKind {
        match self {
            Self::Delay(_) => BuiltinEffectKind::Delay,
            Self::Eq(_) => BuiltinEffectKind::Eq,
            Self::Compressor(_) => BuiltinEffectKind::Compressor,
            Self::Reverb(_) => BuiltinEffectKind::Reverb,
            Self::Chorus(_) => BuiltinEffectKind::Chorus,
            Self::Flanger(_) => BuiltinEffectKind::Flanger,
        }
    }

    /// Delay parameters, when this is a delay node.
    pub fn as_delay(&self) -> Option<&DelayParams> {
        match self {
            Self::Delay(params) => Some(params),
            _ => None,
        }
    }

    /// Mutable delay parameters, when this is a delay node.
    pub fn as_delay_mut(&mut self) -> Option<&mut DelayParams> {
        match self {
            Self::Delay(params) => Some(params),
            _ => None,
        }
    }

    /// EQ parameters, when this is an EQ node.
    pub fn as_eq(&self) -> Option<&EqParams> {
        match self {
            Self::Eq(params) => Some(params),
            _ => None,
        }
    }

    /// Mutable EQ parameters, when this is an EQ node.
    pub fn as_eq_mut(&mut self) -> Option<&mut EqParams> {
        match self {
            Self::Eq(params) => Some(params),
            _ => None,
        }
    }

    /// Compressor parameters, when this is a compressor node.
    pub fn as_compressor(&self) -> Option<&CompressorParams> {
        match self {
            Self::Compressor(params) => Some(params),
            _ => None,
        }
    }

    /// Mutable compressor parameters, when this is a compressor node.
    pub fn as_compressor_mut(&mut self) -> Option<&mut CompressorParams> {
        match self {
            Self::Compressor(params) => Some(params),
            _ => None,
        }
    }

    /// Reverb parameters, when this is a reverb node.
    pub fn as_reverb(&self) -> Option<&ReverbParams> {
        match self {
            Self::Reverb(params) => Some(params),
            _ => None,
        }
    }

    /// Mutable reverb parameters, when this is a reverb node.
    pub fn as_reverb_mut(&mut self) -> Option<&mut ReverbParams> {
        match self {
            Self::Reverb(params) => Some(params),
            _ => None,
        }
    }

    /// Chorus parameters, when this is a chorus node.
    pub fn as_chorus(&self) -> Option<&ChorusParams> {
        match self {
            Self::Chorus(params) => Some(params),
            _ => None,
        }
    }

    /// Mutable chorus parameters, when this is a chorus node.
    pub fn as_chorus_mut(&mut self) -> Option<&mut ChorusParams> {
        match self {
            Self::Chorus(params) => Some(params),
            _ => None,
        }
    }

    /// Flanger parameters, when this is a flanger node.
    pub fn as_flanger(&self) -> Option<&FlangerParams> {
        match self {
            Self::Flanger(params) => Some(params),
            _ => None,
        }
    }

    /// Mutable flanger parameters, when this is a flanger node.
    pub fn as_flanger_mut(&mut self) -> Option<&mut FlangerParams> {
        match self {
            Self::Flanger(params) => Some(params),
            _ => None,
        }
    }
}

/// Note time base: ticks per quarter note.
///
/// 960 PPQ is the Standard MIDI File convention and the de-facto industry
/// standard (`PPQ` in most DAWs). It divides evenly for 1/64 triplets (80
/// ticks), which a coarser base cannot express without rounding error.
pub const TICKS_PER_QUARTER: u32 = 960;

/// Ticks per step (16th note).
pub const TICKS_PER_STEP: u32 = TICKS_PER_QUARTER / 4;

/// Ticks per bar (4/4) = 16 steps × 240.
///
/// The 4/4 default, kept as the span of a freshly created clip. Reading the
/// **document's** meter goes through [`TimeSignature::ticks_per_bar`] (U3a) —
/// this constant is no longer the only source of a bar length.
pub const TICKS_PER_BAR: u32 = TICKS_PER_STEP * 16;

/// Largest number of beats per bar a [`TimeSignature`] can express.
pub const MAX_TIME_SIGNATURE_NUMERATOR: u8 = 16;

/// Musical meter (U3a): how the timeline is grouped into bars.
///
/// A meter is **notation plus a bar length**, never a data transform: changing
/// it moves no note, clip, marker or loop endpoint. Only the derived
/// [`TimeSignature::ticks_per_bar`] changes, and with it everything that
/// measures in bars — grid lines, loop snapping, arrangement rounding, the
/// ruler's bar numbers and the SMF `FF 58` meta event.
///
/// Two meters can share a bar length yet group differently (3/4 and 6/8 are
/// both 2880 ticks here): the grouping is notation, the tick count is what
/// other code reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TimeSignature {
    /// Beats per bar (`1..=`[`MAX_TIME_SIGNATURE_NUMERATOR`]).
    #[serde(default = "default_meter_numerator")]
    pub numerator: u8,
    /// The note value that counts as one beat: a power of two (1, 2, 4, 8, 16
    /// or 32 — the range the SMF `FF 58` denominator exponent can express).
    #[serde(default = "default_meter_denominator")]
    pub denominator: u8,
}

fn default_meter_numerator() -> u8 {
    4
}

fn default_meter_denominator() -> u8 {
    4
}

impl TimeSignature {
    /// 4/4 — the project default and what old (meter-less) documents load with.
    pub const COMMON: Self = Self {
        numerator: 4,
        denominator: 4,
    };

    /// Builds a meter, clamping both fields into range. The file layer and the
    /// UI both funnel through here, so a malformed value can never produce a
    /// zero-length bar; [`TimeSignature::is_valid`] is the strict check
    /// `Project::validate` runs on load.
    pub fn new(numerator: u8, denominator: u8) -> Self {
        Self {
            numerator: numerator.clamp(1, MAX_TIME_SIGNATURE_NUMERATOR),
            denominator: denominator.clamp(1, 32).next_power_of_two().min(32),
        }
    }

    /// Whether both fields are in range (no clamping performed).
    pub fn is_valid(&self) -> bool {
        (1..=MAX_TIME_SIGNATURE_NUMERATOR).contains(&self.numerator)
            && self.denominator.is_power_of_two()
            && (1..=32).contains(&self.denominator)
    }

    /// Ticks per bar: `TICKS_PER_QUARTER × 4 / denominator × numerator`.
    ///
    /// 4/4 = 3840, 3/4 = 2880, 6/8 = 2880 (the same span, grouped in three),
    /// 7/8 = 3360. Always `>= 1` (the fields are clamped on construction).
    pub fn ticks_per_bar(&self) -> u32 {
        TICKS_PER_QUARTER * 4 / u32::from(self.denominator.max(1))
            * u32::from(self.numerator.max(1))
    }

    /// Ticks in one beat (the denominator's note value) — the bar.beat readout
    /// steps by this, so 6/8 counts six beats while 3/4 counts three.
    pub fn ticks_per_beat(&self) -> u32 {
        TICKS_PER_QUARTER * 4 / u32::from(self.denominator.max(1))
    }
}

impl Default for TimeSignature {
    fn default() -> Self {
        Self::COMMON
    }
}

impl std::fmt::Display for TimeSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.numerator, self.denominator)
    }
}

/// A note block in the piano roll.
///
/// Tick positions are signed so that drag gestures can carry a negative
/// intermediate value without a clamp round-trip; they are clamped to `>= 0`
/// before reaching the audio thread (see `Note::start_clamped`).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Note {
    /// Start position (tick, relative to clip start).
    #[serde(default)]
    pub start: i64,
    /// Length (tick), always `>= 1`.
    #[serde(default = "default_note_length")]
    pub length: i64,
    /// MIDI note number.
    #[serde(default = "default_note")]
    pub note: u8,
    /// Note-on velocity, 1..=127 (MIDI wire range).
    #[serde(default = "default_velocity")]
    pub velocity: u8,
    /// MIDI channel, 0..=15.
    #[serde(default)]
    pub channel: u8,
}

fn default_note_length() -> i64 {
    i64::from(TICKS_PER_STEP)
}

fn default_note() -> u8 {
    60
}

fn default_velocity() -> u8 {
    DEFAULT_VELOCITY
}

impl Note {
    /// Tick at which the note ends (`start + length`, saturating).
    pub fn end(&self) -> i64 {
        self.start.saturating_add(self.length)
    }

    /// Start clamped into the valid range for the audio thread.
    pub fn start_clamped(&self) -> u32 {
        self.start.clamp(0, i64::from(u32::MAX)) as u32
    }

    /// Length clamped to at least one tick and into `u32` range.
    pub fn length_clamped(&self) -> u32 {
        self.length.clamp(1, i64::from(u32::MAX)) as u32
    }

    /// Velocity normalized to 0.0..=1.0 for the synthesizer.
    pub fn velocity_f32(&self) -> f32 {
        f32::from(self.velocity.clamp(MIN_VELOCITY, 127)) / 127.0
    }
}

impl Default for Note {
    fn default() -> Self {
        Self {
            start: 0,
            length: default_note_length(),
            note: default_note(),
            velocity: DEFAULT_VELOCITY,
            channel: 0,
        }
    }
}

/// Kind of a continuous / channel controller event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CtrlKind {
    /// Control change with the controller number 0..=127. Value 0..=127.
    Cc(u8),
    /// Pitch bend. Value is centred: -8192..=8191 (0 = no bend).
    PitchBend,
    /// Channel (mono) aftertouch. Value 0..=127.
    ChannelPressure,
    /// Polyphonic aftertouch for the given pitch. Value 0..=127.
    PolyPressure(u8),
    /// Program change. Value is the program number 0..=127.
    Program,
}

impl CtrlKind {
    /// Sort/group key so events of the same kind sit together.
    pub fn group_key(self) -> (u8, u8) {
        match self {
            CtrlKind::Cc(n) => (0, n),
            CtrlKind::PitchBend => (1, 0),
            CtrlKind::ChannelPressure => (2, 0),
            CtrlKind::PolyPressure(p) => (3, p),
            CtrlKind::Program => (4, 0),
        }
    }
}

/// A controller / channel event at a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CtrlEvent {
    /// Absolute position within the sequence (tick).
    pub tick: i64,
    /// MIDI channel 0..=15.
    pub channel: u8,
    /// Which controller this drives.
    pub kind: CtrlKind,
    /// Value in the controller's own range (see [`CtrlKind`]).
    pub value: i32,
}

/// The MIDI content of a clip: notes plus controller events (M1.3).
///
/// Controller events are stored alongside notes rather than inside them
/// because they are independent of pitch and length — the same model serves
/// a note-only roll today and CC lanes later.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Sequence {
    /// Notes, kept sorted by [`Sequence::sort`].
    #[serde(default)]
    pub notes: Vec<Note>,
    /// Controller events, kept sorted by tick.
    #[serde(default)]
    pub ctrls: Vec<CtrlEvent>,
}

impl Sequence {
    /// Restores the sort invariant: notes by
    /// `(start, pitch, channel, length, velocity)`, controller events by tick.
    ///
    /// **Callers holding note indices across a mutation must re-resolve them
    /// afterwards** — sorting permutes the vector, so an index-based selection
    /// is only valid for the pre-sort order.
    pub fn sort(&mut self) {
        self.notes
            .sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
        // Stable, so same-tick controller events keep their relative order.
        self.ctrls.sort_by_key(|c| c.tick);
    }

    /// The last tick covered by any note end or controller event (0 when
    /// empty).
    pub fn end_tick(&self) -> i64 {
        let notes = self.notes.iter().map(Note::end).max();
        let ctrls = self.ctrls.iter().map(|c| c.tick).max();
        match (notes, ctrls) {
            (Some(a), Some(b)) => a.max(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => 0,
        }
    }

    /// Notes whose start lies in `[start, end)`.
    pub fn notes_in(&self, start: i64, end: i64) -> impl Iterator<Item = &Note> {
        self.notes
            .iter()
            .filter(move |n| n.start >= start && n.start < end)
    }

    /// Notes that sound anywhere inside `[start, end)`.
    pub fn notes_overlapping(&self, start: i64, end: i64) -> impl Iterator<Item = &Note> {
        self.notes
            .iter()
            .filter(move |n| n.start < end && n.end() > start)
    }

    /// Drops events a synthesizer could never reproduce: duplicate notes (same
    /// channel, pitch and start — the longest wins) and duplicate controller
    /// events (same tick, channel and kind — the last wins, matching what a
    /// synth would end up playing). Returns how many events were removed.
    ///
    /// Imported files routinely contain these from layered takes or quantized
    /// collisions; without this pass the editor shows notes that cannot be
    /// deleted by any selection-based operation.
    pub fn remove_duplicates(&mut self) -> usize {
        let before = self.notes.len() + self.ctrls.len();

        // Longest first inside each (start, pitch, channel) group, then keep
        // the head of each run.
        self.notes.sort_by(|a, b| {
            (a.start, a.note, a.channel, b.length).cmp(&(b.start, b.note, b.channel, a.length))
        });
        self.notes.dedup_by(|later, kept| {
            later.start == kept.start && later.note == kept.note && later.channel == kept.channel
        });

        let mut kept: Vec<CtrlEvent> = Vec::with_capacity(self.ctrls.len());
        for c in self.ctrls.drain(..) {
            // Only events at the same tick (the tail of `kept`) can match.
            let slot = kept
                .iter_mut()
                .rev()
                .take_while(|k| k.tick == c.tick)
                .find(|k| k.channel == c.channel && k.kind.group_key() == c.kind.group_key());
            match slot {
                Some(slot) => *slot = c,
                None => kept.push(c),
            }
        }
        self.ctrls = kept;

        self.sort();
        before - (self.notes.len() + self.ctrls.len())
    }
}

/// A MIDI clip placed on the arrangement timeline (D14/D15).
///
/// `start_ticks` is the absolute position on the arrangement; the clip is
/// audible for `span_ticks` from there, and its note content repeats every
/// `loop_ticks` within the span (D14 future tiling semantics: the scheduler
/// already tiles, the UI presents one period for now). Sequence positions are
/// relative to the clip start.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MidiClip {
    /// Absolute start position on the arrangement (tick).
    #[serde(default)]
    pub start_ticks: u32,
    /// How long the clip stays active (tick), ≥ `loop_ticks`.
    #[serde(default = "default_clip_span")]
    pub span_ticks: u32,
    /// Content loop length (tick), defaults to one bar (4/4).
    #[serde(default = "default_clip_loop")]
    pub loop_ticks: u32,
    /// Note and controller content (relative to the clip start).
    ///
    /// Nested rather than flattened: `serde(flatten)` breaks RON's
    /// tuple-struct form, which the file tests and hand-written fixtures rely
    /// on.
    #[serde(default)]
    pub sequence: Sequence,
    /// v5 documents stored notes directly on the clip. Read-only: the loader
    /// moves them into [`MidiClip::sequence`] and rescales the ticks
    /// (96 PPQ → 960 PPQ). Never written back.
    #[serde(default, rename = "notes", skip_serializing)]
    pub legacy_notes: Vec<LegacyNote>,
}

/// A v5 note, kept only so old files can be read.
///
/// v5 stored `start`/`length` as `u32` ticks at 96 PPQ and `velocity` as
/// `f32` in 0..1; v6 uses `i64` ticks at 960 PPQ and `u8` in 1..=127.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LegacyNote {
    /// Start position (tick, relative to clip start).
    #[serde(default)]
    pub start: u32,
    /// Length (tick).
    #[serde(default = "default_legacy_note_length")]
    pub length: u32,
    /// MIDI note number.
    #[serde(default = "default_note")]
    pub note: u8,
    /// Velocity (0..1).
    #[serde(default = "default_legacy_velocity")]
    pub velocity: f32,
}

/// v5's `TICKS_PER_STEP`: a 16th note at 96 PPQ.
pub const LEGACY_TICKS_PER_STEP: u32 = 24;

fn default_legacy_note_length() -> u32 {
    LEGACY_TICKS_PER_STEP
}

fn default_legacy_velocity() -> f32 {
    0.9
}

impl LegacyNote {
    /// Rescales to the current model: ticks ×10, velocity 0..1 → 1..=127.
    pub fn upgrade(&self) -> Note {
        Note {
            start: i64::from(self.start) * 10,
            length: (i64::from(self.length) * 10).max(1),
            note: self.note,
            velocity: if self.velocity.is_finite() {
                let scaled = (self.velocity.clamp(0.0, 1.0) * 127.0).round() as i64;
                scaled.clamp(i64::from(MIN_VELOCITY), 127) as u8
            } else {
                DEFAULT_VELOCITY
            },
            channel: 0,
        }
    }
}

fn default_clip_span() -> u32 {
    TICKS_PER_BAR
}

fn default_clip_loop() -> u32 {
    TICKS_PER_BAR
}

impl Default for MidiClip {
    fn default() -> Self {
        Self {
            start_ticks: 0,
            span_ticks: default_clip_span(),
            loop_ticks: default_clip_loop(),
            sequence: Sequence::default(),
            legacy_notes: Vec::new(),
        }
    }
}

impl MidiClip {
    /// Convenience constructor: one empty bar clip starting at `start_ticks`.
    pub fn new(start_ticks: u32) -> Self {
        Self {
            start_ticks,
            ..MidiClip::default()
        }
    }

    /// Read-only access to the clip's notes.
    pub fn notes(&self) -> &[Note] {
        &self.sequence.notes
    }

    /// Mutable access to the clip's notes.
    ///
    /// The caller is responsible for restoring [`Sequence::sort`]'s invariant
    /// afterwards if the mutation can break it.
    pub fn notes_mut(&mut self) -> &mut Vec<Note> {
        &mut self.sequence.notes
    }

    /// Number of notes in the clip.
    pub fn note_count(&self) -> usize {
        self.sequence.notes.len()
    }

    /// Restores the sequence sort invariant.
    pub fn sort(&mut self) {
        self.sequence.sort();
    }
}

/// An audio clip placed on the arrangement timeline (U6 / U11, spec
/// `.agents/spec/audio-trim.md` D1/D4).
///
/// The wav plays at its **native rate** (1×, only a sample-rate conversion —
/// never a pitch-changing stretch): the clip is a **trim window** into the
/// source file. `source_offset` is where the window starts inside the wav
/// (seconds) and `source_len` how long it is. The clip's tick footprint on
/// the arrangement is *derived* from `source_len` at the base tempo
/// ([`Project::audio_clip_span_ticks`], D3) — never stored, so the window and
/// its tick length cannot drift apart. Audio does not follow the tempo curve
/// (D2): the base tempo is the single conversion source.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AudioClip {
    /// Path to the .wav file (decoded via the session sample cache; a
    /// missing file makes the clip silent, not invalid).
    #[serde(default)]
    pub path: String,
    /// Absolute start position on the arrangement (tick).
    #[serde(default)]
    pub start_ticks: u32,
    /// Trim window start inside the wav (seconds, `>= 0`).
    #[serde(default)]
    pub source_offset: f64,
    /// Trim window length (seconds, `> 0`) — the playback window.
    #[serde(default = "default_audio_source_len")]
    pub source_len: f64,
    /// Clip gain (linear, 0..=2, default 1.0). Field reserved for a later
    /// UI; track-level volume is the mixer channel fader.
    #[serde(default = "default_audio_gain")]
    pub gain: f32,
    /// v6 documents stored the clip length as a tick `span_ticks` under the
    /// old proportional-stretch semantics (U6 D1). Read-only: the v6→v7
    /// migration turns it into `source_len` seconds at the base tempo and
    /// clears it. Never written back (`skip_serializing`).
    #[serde(default, rename = "span_ticks", skip_serializing)]
    pub legacy_span_ticks: u32,
}

impl Default for AudioClip {
    fn default() -> Self {
        Self {
            path: String::new(),
            start_ticks: 0,
            source_offset: 0.0,
            source_len: default_audio_source_len(),
            gain: default_audio_gain(),
            legacy_span_ticks: 0,
        }
    }
}

/// Default trim window: one bar at the default tempo (the audio analogue of
/// the old one-bar `span_ticks` default; U11 D3 keeps it in seconds).
fn default_audio_source_len() -> f64 {
    f64::from(TICKS_PER_BAR) * 60.0 / (default_tempo() * f64::from(TICKS_PER_QUARTER))
}

fn default_audio_gain() -> f32 {
    1.0
}

impl AudioClip {
    /// Convenience constructor: one-bar clip of `path` starting at
    /// `start_ticks`.
    pub fn new(path: &str, start_ticks: u32) -> Self {
        Self {
            path: path.to_string(),
            start_ticks,
            ..AudioClip::default()
        }
    }
}

/// How a control curve is interpolated between its anchors (U12, spec
/// `.agents/spec/control-tracks.md` §8).
///
/// The editor offers three *input* tools (freehand draw / control-point
/// polyline / control-point curve); only the last is a distinct data shape, so
/// the mode is a single per-clip enum: `Linear` (freehand **and** polyline)
/// vs `Smooth` (monotone cubic Hermite).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CurveInterp {
    /// Piecewise-linear segments between anchors (freehand / polyline).
    #[default]
    Linear,
    /// Monotone cubic Hermite (Fritsch–Carlson / PCHIP style): smooth and
    /// **overshoot-free** (a value never leaves the range of its two
    /// bounding anchors), so it is safe for automation / CC / tempo.
    Smooth,
}

/// A control (automation) clip placed on the arrangement timeline (D17).
///
/// Same placement semantics as [`MidiClip`]; outside the span nothing is
/// delivered (the target keeps its last value). The delivery endpoint is
/// decided by the node graph's automation cable, not by the clip.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ControlClip {
    /// Absolute start position on the arrangement (tick).
    #[serde(default)]
    pub start_ticks: u32,
    /// How long the clip stays active (tick), ≥ `loop_ticks`.
    #[serde(default = "default_clip_span")]
    pub span_ticks: u32,
    /// Content loop length (tick), defaults to one bar (4/4).
    #[serde(default = "default_clip_loop")]
    pub loop_ticks: u32,
    /// Anchor curve (ticks relative to the clip start, ascending).
    #[serde(default)]
    pub curve: Vec<AutoPoint>,
    /// Interpolation between anchors (U12; additive serde field, default
    /// `Linear` — old documents read back with the original behavior).
    #[serde(default)]
    pub interp: CurveInterp,
}

impl Default for ControlClip {
    fn default() -> Self {
        Self {
            start_ticks: 0,
            span_ticks: default_clip_span(),
            loop_ticks: default_clip_loop(),
            curve: Vec::new(),
            interp: CurveInterp::default(),
        }
    }
}

impl ControlClip {
    /// Convenience constructor: one empty bar clip starting at `start_ticks`.
    pub fn new(start_ticks: u32) -> Self {
        Self {
            start_ticks,
            ..ControlClip::default()
        }
    }

    /// Samples the value at a clip-relative tick: returns `None` when the
    /// curve is empty; otherwise the value is interpolated according to
    /// [`ControlClip::interp`] and clamped to the endpoint values outside both
    /// ends. Requires anchors to be ordered (editing interfaces maintain
    /// this).
    pub fn value_at(&self, tick: f64) -> Option<f32> {
        sample_curve(&self.curve, self.interp, tick)
    }

    /// Inserts or updates an anchor (overwrites the same tick), keeping
    /// ascending tick order.
    pub fn upsert_point(&mut self, point: AutoPoint) {
        self.curve.retain(|p| p.tick != point.tick);
        let pos = self.curve.partition_point(|p| p.tick < point.tick);
        self.curve.insert(pos, point);
    }

    /// Removes the nearest anchor within the `tick` tolerance; returns
    /// `true` on a hit.
    pub fn remove_near(&mut self, tick: u32, tolerance: u32) -> bool {
        let Some(best) = self
            .curve
            .iter()
            .enumerate()
            .filter(|(_, p)| p.tick.abs_diff(tick) <= tolerance)
            .min_by_key(|(_, p)| p.tick.abs_diff(tick))
            .map(|(index, _)| index)
        else {
            return false;
        };
        self.curve.remove(best);
        true
    }
}

/// Samples an ordered anchor list at `tick` (U12). `None` for an empty curve;
/// endpoint values are held outside the range. The engine mirrors this
/// exactly (`RtAutomation::value_at`) — the two must stay behaviorally
/// identical (mirror tests assert the same sample points).
pub fn sample_curve(points: &[AutoPoint], interp: CurveInterp, tick: f64) -> Option<f32> {
    let first = *points.first()?;
    let last = *points.last()?;
    if tick <= f64::from(first.tick) {
        return Some(first.value);
    }
    if tick >= f64::from(last.tick) {
        return Some(last.value);
    }
    match interp {
        CurveInterp::Linear => sample_linear(points, tick),
        CurveInterp::Smooth => sample_smooth(points, tick),
    }
}

/// Piecewise-linear segment lookup (the pre-U12 behavior).
fn sample_linear(points: &[AutoPoint], tick: f64) -> Option<f32> {
    for window in points.windows(2) {
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
    points.last().map(|p| p.value)
}

/// One secant slope between two anchors (0 for a degenerate/duplicate tick).
fn secant(a: AutoPoint, b: AutoPoint) -> f32 {
    let h = f64::from(b.tick) - f64::from(a.tick);
    if h <= 0.0 {
        0.0
    } else {
        ((b.value - a.value) as f64 / h) as f32
    }
}

/// Monotone-cubic (PCHIP / Fritsch–Carlson style) tangent at anchor `k`.
///
/// Interior tangents are the harmonically weighted mean of the adjacent
/// secants (zero when their signs differ — that is what preserves
/// monotonicity); endpoints use their single neighbouring secant. Only
/// anchors `k-1..=k+1` are read, so Smooth stays a local, allocation-free
/// function.
fn smooth_tangent(points: &[AutoPoint], k: usize) -> f32 {
    let n = points.len();
    if n < 2 {
        return 0.0;
    }
    if k == 0 {
        return secant(points[0], points[1]);
    }
    if k == n - 1 {
        return secant(points[n - 2], points[n - 1]);
    }
    let d_prev = secant(points[k - 1], points[k]);
    let d_next = secant(points[k], points[k + 1]);
    if d_prev * d_next <= 0.0 {
        return 0.0;
    }
    let h_prev = (f64::from(points[k].tick) - f64::from(points[k - 1].tick)) as f32;
    let h_next = (f64::from(points[k + 1].tick) - f64::from(points[k].tick)) as f32;
    let w1 = 2.0 * h_next + h_prev;
    let w2 = h_next + 2.0 * h_prev;
    let denom = w1 / d_prev + w2 / d_next;
    if denom.abs() < f32::EPSILON {
        0.0
    } else {
        (w1 + w2) / denom
    }
}

/// Monotone cubic Hermite segment (U12 `Smooth`): finds the segment holding
/// `tick`, evaluates the standard cubic Hermite basis with the two bounding
/// tangents. Falls back to linear on a degenerate (zero-width) segment.
fn sample_smooth(points: &[AutoPoint], tick: f64) -> Option<f32> {
    for i in 0..points.len().saturating_sub(1) {
        let (a, b) = (points[i], points[i + 1]);
        if tick >= f64::from(b.tick) {
            continue;
        }
        let h = f64::from(b.tick) - f64::from(a.tick);
        if h <= 0.0 {
            return Some(a.value);
        }
        let t = ((tick - f64::from(a.tick)) / h) as f32;
        let m0 = smooth_tangent(points, i);
        let m1 = smooth_tangent(points, i + 1);
        let t2 = t * t;
        let t3 = t2 * t;
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        let hf = h as f32;
        return Some(h00 * a.value + h10 * hf * m0 + h01 * b.value + h11 * hf * m1);
    }
    points.last().map(|p| p.value)
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
    /// SoundFont file (.sf2/.sf3): file path + `bank:program` preset
    /// address (spec `sf2-loader.md` D7). Sample data stays out of the
    /// document; a missing file falls back to the built-in voice with the
    /// fields kept (D8).
    SoundFont {
        /// Path to the .sf2/.sf3 file.
        #[serde(default)]
        path: String,
        /// Preset bank number (0..=255).
        #[serde(default)]
        bank: u8,
        /// Preset program number (0..=255).
        #[serde(default)]
        program: u8,
    },
    /// Wav sample player (spec `sample-player.md` D8): file path + root
    /// note + play mode + gain. Decoded sample data stays out of the
    /// document (session `SampleLibrary` cache on the engine side); a
    /// missing file falls back to the built-in voice with the fields kept
    /// (D9).
    Sample {
        /// Path to the .wav file.
        #[serde(default)]
        path: String,
        /// Root MIDI note of the sample (0..=127, default 60 = C4):
        /// playback rate = 2^((note − root)/12) with sample-rate
        /// compensation folded in.
        #[serde(default = "sample_default_root")]
        root_note: u8,
        /// How note-offs interact with the sample (default Gate).
        #[serde(default)]
        play_mode: SamplePlayMode,
        /// Output gain (linear, 0..=2, default 1.0).
        #[serde(default = "sample_default_gain")]
        gain: f32,
    },
    /// CLAP plugin: dynamic library path + plugin ID (instantiated on the
    /// non-real-time thread).
    Clap {
        #[serde(default)]
        path: String,
        #[serde(default)]
        plugin_id: String,
        /// Full plugin state blob (state extension), captured at save time;
        /// empty = plugin defaults. Plugin-private format (CLAP spec), the
        /// plugin itself owns any versioning inside it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        state: Vec<u8>,
    },
    /// VST3 plugin: .vst3 file/bundle path + class CID (16-byte hex).
    Vst3 {
        #[serde(default)]
        path: String,
        #[serde(default)]
        class_id: String,
        /// Full plugin state blob (FState stream), captured at save time;
        /// empty = plugin defaults. Plugin-private format.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        state: Vec<u8>,
    },
}

/// How a sample voice reacts to note-offs (spec `sample-player.md` D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum SamplePlayMode {
    /// Note-off enters the fixed release fade (~10 ms, D2).
    #[default]
    Gate,
    /// Pitch-matched note-offs are ignored; the sample plays to its end.
    /// The blanket transport sweep (Stop / Panic) still stops the voice.
    OneShot,
}

/// Serde default for [`VoiceSpec::Sample`]'s `root_note` (60 = C4).
fn sample_default_root() -> u8 {
    60
}

/// Serde default for [`VoiceSpec::Sample`]'s `gain`.
fn sample_default_gain() -> f32 {
    1.0
}

/// The note plane of one MIDI track (D12 v3 / v4 timeline).
///
/// Timbre/mix/effects are no longer track fields -- they are the
/// [`NodeDoc::Instrument`] / [`NodeDoc::Effect`] / [`NodeDoc::ChannelStrip`]
/// nodes in the node graph. Since v4 the track holds clips placed on the
/// arrangement timeline (the per-track single `Clip` / step pattern /
/// `TrackSource` are retired, D14).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct MidiTrack {
    /// Track name (if empty the UI shows "Track N").
    #[serde(default)]
    pub name: String,
    /// Clips on the arrangement timeline (≤ [`MAX_CLIPS_PER_TRACK`], D15).
    #[serde(default)]
    pub clips: Vec<MidiClip>,
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
    /// Song tempo in BPM (U3b, spec `.agents/spec/tempo-meter.md`).
    ///
    /// Only valid on a cable targeting [`PortRef::TimelineIn`] — tempo is a
    /// clock, not a node parameter, so it must never land on a node's
    /// `set_param` path (D1/D7). The curve value **overrides** the transport
    /// BPM rather than adding to it (D3).
    Tempo,
    /// MIDI control change injected into the target node's MIDI input (U4a,
    /// spec `.agents/spec/control-tracks.md` D1).
    ///
    /// A CC is an **event stream**, not a parameter: the target plugin's
    /// `Cutoff` parameter and CC74 are different channels with different
    /// domains, so this must never take the `set_param` path (D2/D3). The
    /// curve stores the real 0..127 value (D7); the engine quantizes and
    /// de-duplicates per block (D4).
    Cc {
        /// Controller number 0..=127.
        #[serde(default)]
        controller: u8,
        /// MIDI channel 0..=15 (the UI shows 1..=16).
        #[serde(default)]
        channel: u8,
    },
    /// Pitch bend injected into the target node's MIDI input (P1, spec
    /// `.agents/spec/roll-multiclip.md` D9).
    ///
    /// Like [`AutoParam::Cc`] this is an **event stream**, not a parameter:
    /// it never takes the `set_param` path in the engine, and the built-in
    /// sources answer the resulting `midi_pitch_bend` with `false` because
    /// their pitch automation runs through `ParamId::PitchOffset` instead
    /// (D10/D11 — per-sample, no 14-bit quantization, no bend range).
    ///
    /// The curve stores **semitones**, so the vertical axis has a meaning the
    /// user picked: `range_semitones` defines the full-scale bend at
    /// ±8192/8191. It is a document field rather than something probed from
    /// the plugin host, because a plugin's bend range is not readable through
    /// the wiring this app has (and SF2 presets disagree with each other).
    PitchBend {
        /// MIDI channel 0..=15 (the UI shows 1..=16).
        #[serde(default)]
        channel: u8,
        /// Bend range in semitones at full scale (clamped 0.5..=24).
        #[serde(default = "default_bend_range")]
        range_semitones: f32,
    },
}

/// Default pitch-bend range: the General MIDI default of ±2 semitones.
fn default_bend_range() -> f32 {
    2.0
}

/// Allowed pitch-bend range span (semitones). Narrower than this makes the
/// axis unusable, wider than an octave is not a range any receiver ships with.
pub const MIN_BEND_RANGE: f32 = 0.5;
/// Upper bound of [`AutoParam::PitchBend::range_semitones`].
pub const MAX_BEND_RANGE: f32 = 24.0;

impl AutoParam {
    /// Whether this parameter is only legal on the Timeline In endpoint.
    pub fn is_timeline_in_only(&self) -> bool {
        matches!(self, AutoParam::Tempo)
    }

    /// A control-change destination with both fields clamped into range
    /// (`controller` 0..=127, `channel` 0..=15). Documents written by hand or
    /// by an older version cannot produce an out-of-range MIDI byte.
    pub fn cc(controller: u8, channel: u8) -> Self {
        AutoParam::Cc {
            controller: controller.min(127),
            channel: channel.min(15),
        }
    }

    /// `(controller, channel)` when this is a CC destination.
    pub fn cc_of(&self) -> Option<(u8, u8)> {
        match self {
            AutoParam::Cc {
                controller,
                channel,
            } => Some((*controller, *channel)),
            _ => None,
        }
    }

    /// A pitch-bend destination with both fields clamped into range
    /// (`channel` 0..=15, `range_semitones` 0.5..=24). Same hygiene as
    /// [`Self::cc`]: no path can put an out-of-range value on the wire or
    /// produce an axis with no meaning.
    pub fn pitch_bend(channel: u8, range_semitones: f32) -> Self {
        let range = if range_semitones.is_finite() {
            range_semitones.clamp(MIN_BEND_RANGE, MAX_BEND_RANGE)
        } else {
            default_bend_range()
        };
        AutoParam::PitchBend {
            channel: channel.min(15),
            range_semitones: range,
        }
    }

    /// `(channel, range_semitones)` when this is a pitch-bend destination.
    pub fn pitch_bend_of(&self) -> Option<(u8, f32)> {
        match self {
            AutoParam::PitchBend {
                channel,
                range_semitones,
            } => Some((*channel, *range_semitones)),
            _ => None,
        }
    }
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

/// One control (automation) track (D7/D12/D17: top-level time container).
///
/// The track itself carries no target parameter and no global loop anymore --
/// delivery endpoints are decided by [`CableKind::Automation`] cables in the
/// node graph, and time is expressed by the [`ControlClip`]s placed on it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct ControlTrack {
    /// Track name (if empty the UI shows "Control N").
    #[serde(default)]
    pub name: String,
    /// Clips on the arrangement timeline (≤ [`MAX_CLIPS_PER_TRACK`], D15).
    #[serde(default)]
    pub clips: Vec<ControlClip>,
}

/// One audio track (U6, spec `.agents/spec/audio-track.md` D5): a lane on
/// the arrangement timeline holding [`AudioClip`]s. Timbre/mix/effects are
/// graph nodes as with MIDI tracks — the track itself only owns placement.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct AudioTrack {
    /// Track name (if empty the UI shows "Audio N").
    #[serde(default)]
    pub name: String,
    /// Clips on the arrangement timeline (≤ [`MAX_CLIPS_PER_TRACK`]).
    #[serde(default)]
    pub clips: Vec<AudioClip>,
}

/// One mixer channel's console state (P5f D15/D21: a virtual channel strip —
/// the stage-console metaphor; channel state is decoupled from cabling).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MixerChannel {
    /// Channel gain (linear, 0..1.5).
    #[serde(default = "default_track_gain")]
    pub gain: f32,
    /// Channel pan (-1..1, linear pan law).
    #[serde(default)]
    pub pan: f32,
    /// Channel mute.
    #[serde(default)]
    pub muted: bool,
    /// Channel solo (any solo mutes all non-soloed channels of the mixer).
    #[serde(default)]
    pub solo: bool,
}

impl Default for MixerChannel {
    fn default() -> Self {
        Self {
            gain: default_track_gain(),
            pan: 0.0,
            muted: false,
            solo: false,
        }
    }
}

/// Input source bound to a [`NodeDoc::Keyboard`] (spec `plugin-gui.md` §8).
///
/// The binding is **one-to-one**: a node owns exactly one source, and a source
/// is owned by at most one node ([`GraphDoc::validate`]). That is what makes a
/// key press have exactly one entry point into the graph — no "which node
/// wins" and no silent second route to another instrument.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MidiSource {
    /// The computer / on-screen keyboard (the only source wired in v1).
    #[default]
    Computer,
    /// A hardware MIDI input port, identified by the backend's port name.
    /// Hardware input is not wired yet; the document shape is fixed now so
    /// landing it does not need another node-layout change.
    Device(String),
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
    /// Effect node (1 in, 1 out). `Builtin` = built-in effect described by
    /// `params`.
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
        /// Built-in effect parameters (ignored for external plugins); the tag
        /// inside is the effect kind (spec `effects.md` D1).
        #[serde(default)]
        params: EffectParams,
        /// v7 compatibility bridge (D4): the delay payload used to live in
        /// this sibling field. `migrate_v7_to_v8` moves it into `params`; new
        /// files never write it (`skip_serializing_if`), so it is always
        /// `None` at runtime.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delay: Option<DelayParams>,
    },
    /// Mixer node (P5f D15/D21): the explicit summing point. Absorbs the old
    /// ChannelStrip — per-channel gain/pan/mute/solo faders + an output
    /// pan/mute stage; channels are managed by add/remove buttons
    /// ([`MIN_MIXER_CHANNELS`]..=[`MAX_MIXER_CHANNELS`]) and decoupled from
    /// cabling (unplugging never loses fader settings).
    Mixer {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track (grouping only; a mixer usually serves many tracks).
        #[serde(default)]
        track: usize,
        /// Channel (input port) count, 2..=32 (D21: add/remove buttons).
        #[serde(default = "default_mixer_inputs")]
        inputs: u8,
        /// Per-channel console state; `len() == inputs as usize`.
        #[serde(default)]
        channels: Vec<MixerChannel>,
        /// Output-stage pan (-1..1, linear law).
        #[serde(default)]
        out_pan: f32,
        /// Output-stage mute.
        #[serde(default)]
        out_muted: bool,
    },
    /// Keyboard input node (G6, spec `plugin-gui.md` §8): a **UI-only
    /// routing node** that owns exactly one input source. It has no real-time
    /// processor and never enters the engine graph (the document node_id has
    /// no engine counterpart); its MIDI out cables point at instruments, so
    /// whatever the bound device plays enters the graph through **this** node
    /// and nowhere else.
    ///
    /// The node carries `node_id` (cable endpoints address it), `track`
    /// (patch auto-placement grouping only — it is *not* a routing input) and
    /// `source` (the bound device; [`MidiSource::Computer`] in v1).
    Keyboard {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track (auto-placement grouping only).
        #[serde(default)]
        track: usize,
        /// The input source this node listens to. [`GraphDoc::validate`]
        /// rejects two nodes bound to the same source.
        #[serde(default)]
        source: MidiSource,
    },
    /// Feedback tap node (P6, spec `node-graph.md` D10): the explicit,
    /// self-documenting loop breaker. Its in-edges are summed **one block
    /// late** (the engine orders it before its tap sources), which is what
    /// exempts it from the cycle rejection — so it has N inputs (taps) and
    /// one output. `gain` (0..1) scales the delayed sum, which keeps a bare
    /// loop from diverging.
    Feedback {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track (grouping only).
        #[serde(default)]
        track: usize,
        /// Feedback amount applied to the delayed sum (0..1).
        #[serde(default = "default_feedback_gain")]
        gain: f32,
    },
    /// Audio track node (U6, spec `.agents/spec/audio-track.md` D3): the
    /// generator that plays the owning [`AudioTrack`]'s clips off the
    /// transport clock. The clip table itself lives in
    /// [`Project::audio_tracks`] (indexed by `track`), not in the node —
    /// this is the same "placement in the track, identity in the graph"
    /// split as MIDI tracks.
    AudioTrack {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning audio track (index into `Project::audio_tracks`).
        #[serde(default)]
        track: usize,
    },
    /// Legacy v4 channel strip — load-only (the v4→v5 migration converts it
    /// into a 2-channel [`NodeDoc::Mixer`]); new documents never emit it.
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

fn default_mixer_inputs() -> u8 {
    MIN_MIXER_CHANNELS as u8
}

fn default_track_gain() -> f32 {
    0.8
}

fn default_feedback_gain() -> f32 {
    0.5
}

impl NodeDoc {
    /// Node ID.
    pub fn node_id(&self) -> u64 {
        match self {
            NodeDoc::Instrument { node_id, .. }
            | NodeDoc::Effect { node_id, .. }
            | NodeDoc::Mixer { node_id, .. }
            | NodeDoc::Feedback { node_id, .. }
            | NodeDoc::Keyboard { node_id, .. }
            | NodeDoc::AudioTrack { node_id, .. }
            | NodeDoc::ChannelStrip { node_id, .. } => *node_id,
        }
    }

    /// Owning track.
    pub fn track(&self) -> usize {
        match self {
            NodeDoc::Instrument { track, .. }
            | NodeDoc::Effect { track, .. }
            | NodeDoc::Mixer { track, .. }
            | NodeDoc::Feedback { track, .. }
            | NodeDoc::Keyboard { track, .. }
            | NodeDoc::AudioTrack { track, .. }
            | NodeDoc::ChannelStrip { track, .. } => *track,
        }
    }

    /// Whether this node is a mixer (the only multi-input node, D14/D15).
    pub fn is_mixer(&self) -> bool {
        matches!(self, NodeDoc::Mixer { .. })
    }

    /// Whether this node is a virtual keyboard (G6: document-only routing
    /// node — no engine processor, no audio ports).
    pub fn is_keyboard(&self) -> bool {
        matches!(self, NodeDoc::Keyboard { .. })
    }
}

/// Cable port reference (spec §7 `PortRef`).
///
/// `Master` / `TimelineIn` / `MidiTrack` / `AutoTrack` are reserved identities
/// of fixed entities (not listed in the node table; the loader/engine fills
/// them in implicitly); `Node` references a document node.
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
    /// Timeline input terminal (U3b, spec `tempo-meter.md` D1): the fixed
    /// **pure sink** mirroring the `Timeline` source. It has no node_id, no
    /// engine processor and no user-creatable instance — identity alone makes
    /// it a singleton (D2), so `validate` only constrains the cables, not the
    /// entity. v1 has a single port ([`AutoParam::Tempo`]).
    TimelineIn,
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
    /// Destination channel (P5f D17): mixer input port index when `to` is a
    /// mixer node (persisted so fader ↔ cable binding survives edits); 0 for
    /// every other target.
    #[serde(default)]
    pub in_port: u8,
}

impl CableDoc {
    /// Convenience constructor for audio cables.
    pub fn audio(from: impl Into<PortRef>, to: impl Into<PortRef>) -> Self {
        Self {
            kind: CableKind::Audio,
            from: from.into(),
            to: to.into(),
            param: AutoParam::default(),
            in_port: 0,
        }
    }

    /// Audio cable landing on a specific mixer channel (P5f D17).
    pub fn audio_to_channel(from: impl Into<PortRef>, to: impl Into<PortRef>, in_port: u8) -> Self {
        Self {
            in_port,
            ..CableDoc::audio(from, to)
        }
    }

    /// Convenience constructor for MIDI cables.
    pub fn midi(from_track: usize, to_node: u64) -> Self {
        Self {
            kind: CableKind::Midi,
            from: PortRef::MidiTrack(from_track),
            to: PortRef::Node(to_node),
            param: AutoParam::default(),
            in_port: 0,
        }
    }

    /// Convenience constructor for a virtual-keyboard MIDI cable (G6: vk
    /// node → instrument node; the only node→node MIDI edge).
    pub fn keyboard_midi(from_node: u64, to_node: u64) -> Self {
        Self {
            kind: CableKind::Midi,
            from: PortRef::Node(from_node),
            to: PortRef::Node(to_node),
            param: AutoParam::default(),
            in_port: 0,
        }
    }

    /// Convenience constructor for automation cables.
    pub fn automation(from_track: usize, to_node: u64, param: AutoParam) -> Self {
        Self {
            kind: CableKind::Automation,
            from: PortRef::AutoTrack(from_track),
            to: PortRef::Node(to_node),
            param,
            in_port: 0,
        }
    }

    /// Convenience constructor for a tempo cable (U3b): control track →
    /// the fixed Timeline In endpoint's `Tempo` port.
    pub fn tempo(from_track: usize) -> Self {
        Self {
            kind: CableKind::Automation,
            from: PortRef::AutoTrack(from_track),
            to: PortRef::TimelineIn,
            param: AutoParam::Tempo,
            in_port: 0,
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
    /// Patch canvas layout (pure view data, not part of the signal topology;
    /// missing entries fall back to UI auto-placement).
    #[serde(default)]
    pub layout: LayoutDoc,
}

/// Patch canvas layout: entity positions in graph space, egui-free
/// (`[f32; 2]` = (x, y) graph units).
///
/// Entries referencing nonexistent node ids are tolerated (the UI prunes
/// them); [`BTreeMap`] keeps serialized output deterministic.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct LayoutDoc {
    /// Document node positions (keyed by node_id).
    #[serde(default)]
    pub nodes: BTreeMap<u64, [f32; 2]>,
    /// Timeline box position.
    #[serde(default)]
    pub timeline: Option<[f32; 2]>,
    /// Master output box position.
    #[serde(default)]
    pub master: Option<[f32; 2]>,
    /// Timeline In box position (U3b).
    #[serde(default)]
    pub timeline_in: Option<[f32; 2]>,
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

    /// Finds a node by node_id (mutable).
    pub fn node_by_id_mut(&mut self, node_id: u64) -> Option<&mut NodeDoc> {
        self.nodes.iter_mut().find(|n| n.node_id() == node_id)
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

    /// The node bound to the computer keyboard, if the document has one
    /// (spec `plugin-gui.md` §8). Identity, not selection: the keyboard is
    /// live whenever its node exists, exactly like a hardware port.
    pub fn computer_keyboard(&self) -> Option<u64> {
        self.nodes
            .iter()
            .find(|n| {
                matches!(
                    n,
                    NodeDoc::Keyboard {
                        source: MidiSource::Computer,
                        ..
                    }
                )
            })
            .map(NodeDoc::node_id)
    }

    /// All instruments cabled from one keyboard node, in document order
    /// (spec `plugin-gui.md` §8).
    ///
    /// The node's MIDI out cables (`Keyboard → node`, G6 cable rule) are
    /// walked breadth-first and **every** instrument reached is returned, so
    /// one source drives all of its cabled instruments at once: a MIDI source
    /// is one out jack that any number of cables may leave (multi-out /
    /// single-in, the same rule the timeline's track outlets follow), which
    /// makes synth layering need no mixer gymnastics. The fan-out cap lives at
    /// the dispatch edge (`MAX_MIDI_FANOUT`), not in the document — a loaded
    /// file with extra cables still resolves.
    ///
    /// Non-instrument endpoints (an effect fed by a MIDI cable, which the UI
    /// never creates) are traversed but never returned — no MIDI pass-through.
    /// Empty = nothing is cabled (the source is silent: there is **no**
    /// track fallback).
    pub fn keyboard_routes_from(&self, keyboard: u64) -> Vec<u64> {
        if !self.node_by_id(keyboard).is_some_and(NodeDoc::is_keyboard) {
            return Vec::new();
        }
        let mut reached: BTreeSet<u64> = BTreeSet::from([keyboard]);
        let mut queue: VecDeque<u64> = VecDeque::from([keyboard]);
        while let Some(id) = queue.pop_front() {
            for cable in &self.cables {
                if cable.kind != CableKind::Midi || cable.from != PortRef::Node(id) {
                    continue;
                }
                if let PortRef::Node(to) = cable.to
                    && reached.insert(to)
                {
                    queue.push_back(to);
                }
            }
        }
        self.nodes
            .iter()
            .filter(|n| matches!(n, NodeDoc::Instrument { .. }) && reached.contains(&n.node_id()))
            .map(NodeDoc::node_id)
            .collect()
    }

    /// The first effect node of a track.
    pub fn effect_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::Effect { .. }) && n.track() == track)
    }

    /// The channel strip node of a track (legacy v4 documents only).
    pub fn channel_strip_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::ChannelStrip { .. }) && n.track() == track)
    }

    /// All mixer nodes (P5f: the explicit summing points).
    pub fn mixers(&self) -> impl Iterator<Item = &NodeDoc> {
        self.nodes.iter().filter(|n| n.is_mixer())
    }

    /// The first mixer node (the "global mixer" of the default topology).
    pub fn first_mixer(&self) -> Option<&NodeDoc> {
        self.mixers().next()
    }

    /// The first free input channel of mixer `mixer_id` (D17: cables may land
    /// on any free channel; `None` when the node is not a mixer or all
    /// channels are taken).
    pub fn first_free_channel(&self, mixer_id: u64) -> Option<u8> {
        let inputs = match self.node_by_id(mixer_id)? {
            NodeDoc::Mixer { inputs, .. } => *inputs as usize,
            _ => return None,
        };
        let used: Vec<usize> = self
            .cables
            .iter()
            .filter(|c| c.kind == CableKind::Audio && c.to == PortRef::Node(mixer_id))
            .map(|c| c.in_port as usize)
            .collect();
        (0..inputs).find(|ch| !used.contains(ch)).map(|ch| ch as u8)
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

    /// Full endpoint of the cable for automation track `index`: the
    /// destination port (a document node, or the fixed [`PortRef::TimelineIn`])
    /// plus the parameter (U3b). `None` when not cabled.
    pub fn auto_target(&self, index: usize) -> Option<(PortRef, AutoParam)> {
        self.cables.iter().find_map(|c| {
            if c.kind == CableKind::Automation && c.from == PortRef::AutoTrack(index) {
                Some((c.to.clone(), c.param.clone()))
            } else {
                None
            }
        })
    }

    /// Whether automation track `index` is cabled to the Timeline In endpoint
    /// (U3b: song tempo).
    pub fn auto_targets_timeline_in(&self, index: usize) -> bool {
        matches!(self.auto_target(index), Some((PortRef::TimelineIn, _)))
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
        // Mixer channel integrity (P5f D15/D21).
        for node in &self.nodes {
            if let NodeDoc::Mixer {
                node_id,
                inputs,
                channels,
                ..
            } = node
            {
                let inputs = *inputs as usize;
                if !(MIN_MIXER_CHANNELS..=MAX_MIXER_CHANNELS).contains(&inputs) {
                    return Err(format!(
                        "mixer node {node_id} has {inputs} inputs (allowed {MIN_MIXER_CHANNELS}..={MAX_MIXER_CHANNELS})"
                    ));
                }
                if channels.len() != inputs {
                    return Err(format!(
                        "mixer node {node_id} has {} channels but {} inputs",
                        channels.len(),
                        inputs
                    ));
                }
            }
        }
        // Input-source ownership (spec `plugin-gui.md` §8): one source, one
        // keyboard node. Two nodes listening to the same device would route
        // the same key press into two unrelated fan-outs — unpredictable and
        // unauditable, so the binding is rejected instead.
        let mut sources: std::collections::HashSet<&MidiSource> = Default::default();
        for node in &self.nodes {
            if let NodeDoc::Keyboard {
                node_id, source, ..
            } = node
                && !sources.insert(source)
            {
                return Err(format!(
                    "keyboard node {node_id} binds an input source that another node already owns ({source:?})"
                ));
            }
        }
        // Single-input discipline (P5f D14/D16): audio cables into one
        // destination — only mixers accept multiple (≤ 32 with unique
        // in_port); everything else ≤ 1, master included.
        let mut audio_in_count: std::collections::BTreeMap<u64, usize> = Default::default();
        let mut mixer_ports: std::collections::BTreeMap<u64, Vec<u8>> = Default::default();
        for cable in &self.cables {
            self.check_port(&cable.from, &node_ids, midi_count, auto_count)?;
            self.check_port(&cable.to, &node_ids, midi_count, auto_count)?;
            match cable.kind {
                CableKind::Audio => {
                    if !matches!(cable.from, PortRef::Node(_)) {
                        return Err("audio cable source port must be a node".to_string());
                    }
                    // G6: a virtual keyboard has no audio ports at all.
                    if let PortRef::Node(id) = cable.from
                        && self.node_by_id(id).is_some_and(NodeDoc::is_keyboard)
                    {
                        return Err(format!(
                            "virtual keyboard node {id} has no audio output (MIDI only)"
                        ));
                    }
                    let dest_key = match &cable.to {
                        PortRef::Node(id) => *id,
                        PortRef::Master => MASTER_NODE_ID,
                        _ => {
                            return Err(
                                "audio cable target port must be a node or the master output"
                                    .to_string(),
                            );
                        }
                    };
                    if self.node_by_id(dest_key).is_some_and(NodeDoc::is_keyboard) {
                        return Err(format!(
                            "virtual keyboard node {dest_key} has no audio input (MIDI only)"
                        ));
                    }
                    if dest_key == MASTER_NODE_ID {
                        // Master (D16): single input; in_port must be 0.
                        if cable.in_port != 0 {
                            return Err(
                                "master input has no channel ports (in_port must be 0)".to_string()
                            );
                        }
                    } else if let Some(NodeDoc::Mixer { inputs, .. }) = self.node_by_id(dest_key) {
                        if cable.in_port as usize >= *inputs as usize {
                            return Err(format!(
                                "audio cable targets mixer {dest_key} channel {} out of range (inputs = {inputs})",
                                cable.in_port
                            ));
                        }
                        let ports = mixer_ports.entry(dest_key).or_default();
                        if ports.contains(&cable.in_port) {
                            return Err(format!(
                                "mixer {dest_key} channel {} receives multiple cables",
                                cable.in_port
                            ));
                        }
                        ports.push(cable.in_port);
                    } else {
                        if cable.in_port != 0 {
                            return Err(format!(
                                "non-mixer node {dest_key} has no channel ports (in_port must be 0)"
                            ));
                        }
                    }
                    let count = audio_in_count.entry(dest_key).or_insert(0);
                    *count += 1;
                    let limit = if dest_key == MASTER_NODE_ID {
                        1
                    } else if matches!(self.node_by_id(dest_key), Some(NodeDoc::Mixer { .. })) {
                        MAX_MIXER_CHANNELS
                    } else {
                        1
                    };
                    if *count > limit {
                        return Err(format!(
                            "node {dest_key} receives {count} audio cables (limit {limit}, D14/D16: only mixers sum multiple inputs)"
                        ));
                    }
                }
                CableKind::Midi => {
                    // D3 + G6: MIDI sources are the timeline MIDI track
                    // outlets and virtual keyboard nodes (the only node kind
                    // with a MIDI out port).
                    match cable.from {
                        PortRef::MidiTrack(_) => {}
                        PortRef::Node(id) => {
                            if !self.node_by_id(id).is_some_and(NodeDoc::is_keyboard) {
                                return Err(format!(
                                    "MIDI cable source node {id} is not a virtual keyboard (D3: track outlets and vk nodes only)"
                                ));
                            }
                        }
                        _ => {
                            return Err(
                                "MIDI cable source port must be a timeline MIDI track or a virtual keyboard node"
                                    .to_string(),
                            );
                        }
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
                    // U3b D1/D7: the only valid automation destination is a
                    // node parameter port, or the fixed Timeline In endpoint
                    // whose sole v1 port is `Tempo`. Tempo is a clock, so it
                    // must not reach a node's `set_param` path, and every
                    // other parameter must not reach the clock.
                    match cable.to {
                        PortRef::Node(_) => {
                            if cable.param.is_timeline_in_only() {
                                return Err(format!(
                                    "{:?} is only valid on the timeline input (it is a clock, not a node parameter)",
                                    cable.param
                                ));
                            }
                        }
                        PortRef::TimelineIn => {
                            if !matches!(cable.param, AutoParam::Tempo) {
                                return Err("the timeline input only accepts the tempo port (v1)"
                                    .to_string());
                            }
                        }
                        _ => {
                            return Err(
                                "automation cable target port must be a node parameter port or the timeline input"
                                    .to_string(),
                            );
                        }
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
            // Fixed sink with no identity to resolve (U3b D2).
            PortRef::TimelineIn => {}
        }
        Ok(())
    }
}

/// Generates the default node graph document (P5f D18): per track
/// `voice → delay → mixer.ch[i]` + one global mixer (`inputs =
/// max(track_count, 2)`) → master output + this track's MIDI self-connection.
///
/// node_ids are allocated consecutively starting from [`MASTER_NODE_ID`] + 1.
pub fn default_graph_doc(track_count: usize) -> GraphDoc {
    let mut nodes = Vec::new();
    let mut cables = Vec::new();
    let mut next_id = MASTER_NODE_ID + 1;
    let mut voice_ids = Vec::new();
    let mut delay_ids = Vec::new();
    for track in 0..track_count {
        let voice_id = next_id;
        let delay_id = next_id + 1;
        next_id += 2;
        voice_ids.push(voice_id);
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
            params: EffectParams::default(),
            delay: None,
        });
        cables.push(CableDoc::audio(voice_id, delay_id));
        cables.push(CableDoc::midi(track, voice_id));
        delay_ids.push(delay_id);
    }
    // One global mixer: one channel per track (D21 minimum 2).
    let inputs = track_count.max(MIN_MIXER_CHANNELS);
    let mixer_id = next_id;
    nodes.push(NodeDoc::Mixer {
        node_id: mixer_id,
        track: 0,
        inputs: inputs as u8,
        channels: vec![MixerChannel::default(); inputs],
        out_pan: 0.0,
        out_muted: false,
    });
    for (ch, delay_id) in delay_ids.iter().enumerate() {
        cables.push(CableDoc::audio_to_channel(*delay_id, mixer_id, ch as u8));
    }
    cables.push(CableDoc::audio(mixer_id, PortRef::Master));
    // One computer-keyboard input node, cabled to the first track's
    // instrument (spec `plugin-gui.md` §8): a fresh project plays from the
    // computer keyboard, but through an explicit, visible, deletable route
    // instead of the old implicit track fallback.
    next_id += 1;
    let keyboard_id = next_id;
    nodes.push(NodeDoc::Keyboard {
        node_id: keyboard_id,
        track: 0,
        source: MidiSource::Computer,
    });
    if let Some(voice_id) = voice_ids.first() {
        cables.push(CableDoc::keyboard_midi(keyboard_id, *voice_id));
    }
    GraphDoc {
        nodes,
        cables,
        layout: LayoutDoc::default(),
    }
}

/// Multi-track project document (v4).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Project {
    /// Project metadata (name, tempo, etc.).
    #[serde(default)]
    pub metadata: ProjectMetadata,
    /// Master output gain (linear).
    #[serde(default = "default_master_gain")]
    pub master_gain: f32,
    /// MIDI tracks (note plane, D12 dynamic, capped at [`MAX_MIDI_TRACKS`]).
    #[serde(default)]
    pub midi_tracks: Vec<MidiTrack>,
    /// Control (automation) tracks (D17, capped at [`MAX_AUTO_TRACKS`]).
    #[serde(default)]
    pub control_tracks: Vec<ControlTrack>,
    /// Audio tracks (U6, capped at [`MAX_AUDIO_TRACKS`]).
    #[serde(default)]
    pub audio_tracks: Vec<AudioTrack>,
    /// Node graph document (D13 fully explicit).
    #[serde(default)]
    pub graph: GraphDoc,
}

fn default_master_gain() -> f32 {
    0.8
}

impl Default for Project {
    fn default() -> Self {
        Self {
            metadata: ProjectMetadata::default(),
            master_gain: default_master_gain(),
            midi_tracks: vec![MidiTrack::default(); DEFAULT_MIDI_TRACKS],
            control_tracks: Vec::new(),
            audio_tracks: Vec::new(),
            graph: default_graph_doc(DEFAULT_MIDI_TRACKS),
        }
    }
}

/// Maximum arrangement length in ticks (D16 safety bound).
pub const MAX_ARRANGEMENT_TICKS: u64 = (MAX_ARRANGEMENT_BARS as u64) * (TICKS_PER_BAR as u64);

impl Project {
    /// Migrates older document shapes to the current one (v4 → v5, P5f):
    ///
    /// 1. Every legacy [`NodeDoc::ChannelStrip`] becomes a 2-channel
    ///    [`NodeDoc::Mixer`] (channel 0 carries the strip's gain/pan/mute).
    ///    Cables keep targeting the same node_id; automation cables that
    ///    targeted strip parameters (TrackGain/TrackPan) now target a mixer
    ///    node the engine ignores — acceptable for a one-time migration.
    /// 2. If the master output receives multiple audio cables (legal under
    ///    v4's ≤8 implicit summing, illegal under D16), a synthetic mixer is
    ///    inserted to absorb them: each old master cable is rewired onto its
    ///    own mixer channel.
    ///
    /// Idempotent: migrated documents pass through unchanged. Call before
    /// [`Project::validate`].
    pub fn migrate(&mut self) {
        // 1) ChannelStrip → Mixer.
        for node in &mut self.graph.nodes {
            if let NodeDoc::ChannelStrip {
                node_id,
                track,
                gain,
                pan,
                muted,
            } = node
            {
                let converted = NodeDoc::Mixer {
                    node_id: *node_id,
                    track: *track,
                    inputs: MIN_MIXER_CHANNELS as u8,
                    channels: vec![
                        MixerChannel {
                            gain: *gain,
                            pan: *pan,
                            muted: *muted,
                            solo: false,
                        },
                        MixerChannel::default(),
                    ],
                    out_pan: 0.0,
                    out_muted: false,
                };
                *node = converted;
            }
        }

        // 2) Master multi-input → synthetic mixer.
        let master_ins: Vec<usize> = self
            .graph
            .cables
            .iter()
            .enumerate()
            .filter(|(_, c)| c.kind == CableKind::Audio && c.to == PortRef::Master)
            .map(|(i, _)| i)
            .collect();
        if master_ins.len() > 1 {
            let inputs = master_ins.len().min(MAX_MIXER_CHANNELS);
            let mixer_id = self.graph.next_node_id();
            self.graph.nodes.push(NodeDoc::Mixer {
                node_id: mixer_id,
                track: 0,
                inputs: inputs as u8,
                channels: vec![MixerChannel::default(); inputs],
                out_pan: 0.0,
                out_muted: false,
            });
            for (ch, cable_idx) in master_ins.iter().enumerate().take(inputs) {
                self.graph.cables[*cable_idx].to = PortRef::Node(mixer_id);
                self.graph.cables[*cable_idx].in_port = ch as u8;
            }
            self.graph
                .cables
                .push(CableDoc::audio(mixer_id, PortRef::Master));
        }
    }

    /// Upgrades a v5 document to v6: the tick base moves from 96 PPQ to 960
    /// PPQ and notes move from the clip into [`MidiClip::sequence`].
    ///
    /// Every tick in the document is multiplied by 10 — clip placement and
    /// span, note starts and lengths, automation anchors. Values are clamped
    /// rather than rejected: a v5 document was already bounded by
    /// `MAX_ARRANGEMENT_BARS`, and the bound is expressed in bars, so the
    /// rescaled document still validates.
    ///
    /// Idempotent: a v6 document has `legacy_notes == None` and already-960
    /// ticks, so this is a no-op. Only call it for documents whose
    /// `version < 6`.
    pub fn migrate_v5_to_v6(&mut self) {
        fn scale(t: &mut u32) {
            *t = t.saturating_mul(10);
        }

        for track in &mut self.midi_tracks {
            for clip in &mut track.clips {
                scale(&mut clip.start_ticks);
                scale(&mut clip.span_ticks);
                scale(&mut clip.loop_ticks);
                if !clip.legacy_notes.is_empty() {
                    let mut seq = std::mem::take(&mut clip.sequence);
                    seq.notes = clip.legacy_notes.iter().map(LegacyNote::upgrade).collect();
                    seq.sort();
                    clip.sequence = seq;
                }
                clip.legacy_notes.clear();
            }
        }
        for track in &mut self.control_tracks {
            for clip in &mut track.clips {
                scale(&mut clip.start_ticks);
                scale(&mut clip.span_ticks);
                scale(&mut clip.loop_ticks);
                for point in &mut clip.curve {
                    scale(&mut point.tick);
                }
            }
        }
    }

    /// Upgrades a v6 document to v7 (U11): audio clips leave the tick-domain
    /// proportional-stretch model (`span_ticks`) for a seconds-domain trim
    /// window (`source_len`). The old span is converted at the base tempo;
    /// the window starts at the file head (`source_offset = 0`) — v6 stored
    /// no offset. Idempotent for documents that already carry `source_len`.
    ///
    /// Only call it for documents whose `version < 7`.
    pub fn migrate_v6_to_v7(&mut self) {
        let spt = self.sec_per_tick();
        for track in &mut self.audio_tracks {
            for clip in &mut track.clips {
                if clip.legacy_span_ticks > 0 {
                    clip.source_len = f64::from(clip.legacy_span_ticks) * spt;
                }
                clip.source_offset = 0.0;
                clip.legacy_span_ticks = 0;
            }
        }
    }

    /// v7 → v8 (spec `.agents/spec/effects.md` D3/D4): the built-in effect
    /// payload moved from a sibling `delay` field into the tagged `params`
    /// enum, so the kind and its parameters cannot disagree.
    ///
    /// Nodes that never had a legacy payload keep the default (`Delay` with
    /// default values), which is exactly what v7 meant by an effect node with
    /// the built-in voice.
    pub fn migrate_v7_to_v8(&mut self) {
        for node in &mut self.graph.nodes {
            if let NodeDoc::Effect { params, delay, .. } = node {
                if let Some(delay) = delay.take() {
                    *params = EffectParams::Delay(delay);
                }
            }
        }
    }

    /// The document's bar length in ticks (U3a): every bar-based measure
    /// (grid, loop snapping, arrangement rounding, ruler) reads this instead
    /// of the 4/4 [`TICKS_PER_BAR`] constant.
    pub fn ticks_per_bar(&self) -> u32 {
        self.metadata.time_signature.ticks_per_bar()
    }

    /// Seconds per tick at the project's **base** tempo (U11 D2). Audio clips
    /// convert between the tick domain and seconds with this constant — they
    /// deliberately do not follow the tempo curve.
    pub fn sec_per_tick(&self) -> f64 {
        60.0 / (self.metadata.tempo_bpm.max(1.0) * f64::from(TICKS_PER_QUARTER))
    }

    /// Tick footprint of an audio clip (U11 D3): its trim window (`source_len`
    /// seconds) mapped back to ticks at the base tempo. Derived — never stored
    /// on the clip. Always `>= 1` so the clip keeps a positive footprint.
    pub fn audio_clip_span_ticks(&self, clip: &AudioClip) -> u32 {
        let ticks = (clip.source_len / self.sec_per_tick()).round();
        // `max` swallows NaN (returns 1.0); `as` saturates huge values.
        ticks.max(1.0) as u32
    }

    /// Project-level validation: track counts, clip capacities/placement and
    /// graph structure (the file layer is the last line of defense; report
    /// errors instead of truncating).
    pub fn validate(&self) -> Result<(), String> {
        if !self.metadata.time_signature.is_valid() {
            return Err(format!(
                "invalid time signature {} (numerator 1..={MAX_TIME_SIGNATURE_NUMERATOR}, denominator a power of two up to 32)",
                self.metadata.time_signature
            ));
        }
        if self.midi_tracks.len() > MAX_MIDI_TRACKS {
            return Err(format!(
                "MIDI track count {} exceeds the limit {MAX_MIDI_TRACKS}",
                self.midi_tracks.len()
            ));
        }
        if self.control_tracks.len() > MAX_AUTO_TRACKS {
            return Err(format!(
                "control track count {} exceeds the limit {MAX_AUTO_TRACKS}",
                self.control_tracks.len()
            ));
        }
        for (ti, track) in self.midi_tracks.iter().enumerate() {
            if track.clips.len() > MAX_CLIPS_PER_TRACK {
                return Err(format!(
                    "MIDI track #{ti} holds {} clips, exceeding the limit {MAX_CLIPS_PER_TRACK}",
                    track.clips.len()
                ));
            }
            for (ci, clip) in track.clips.iter().enumerate() {
                if clip.notes().len() > MAX_CLIP_NOTES {
                    return Err(format!(
                        "MIDI track #{ti} clip #{ci} holds {} notes, exceeding the limit {MAX_CLIP_NOTES}",
                        clip.notes().len()
                    ));
                }
                check_clip_placement(ti, ci, clip.start_ticks, clip.span_ticks, clip.loop_ticks)?;
            }
        }
        for (ti, track) in self.control_tracks.iter().enumerate() {
            if track.clips.len() > MAX_CLIPS_PER_TRACK {
                return Err(format!(
                    "control track #{ti} holds {} clips, exceeding the limit {MAX_CLIPS_PER_TRACK}",
                    track.clips.len()
                ));
            }
            for (ci, clip) in track.clips.iter().enumerate() {
                if clip.curve.len() > MAX_CURVE_POINTS {
                    return Err(format!(
                        "control track #{ti} clip #{ci} holds {} anchors, exceeding the limit {MAX_CURVE_POINTS}",
                        clip.curve.len()
                    ));
                }
                check_clip_placement(ti, ci, clip.start_ticks, clip.span_ticks, clip.loop_ticks)?;
            }
        }
        if self.audio_tracks.len() > MAX_AUDIO_TRACKS {
            return Err(format!(
                "audio track count {} exceeds the limit {MAX_AUDIO_TRACKS}",
                self.audio_tracks.len()
            ));
        }
        for (ti, track) in self.audio_tracks.iter().enumerate() {
            if track.clips.len() > MAX_CLIPS_PER_TRACK {
                return Err(format!(
                    "audio track #{ti} holds {} clips, exceeding the limit {MAX_CLIPS_PER_TRACK}",
                    track.clips.len()
                ));
            }
            for (ci, clip) in track.clips.iter().enumerate() {
                // U11: the trim window replaces the old span/loop pair; the
                // arrangement bound is checked on the derived tick footprint.
                if clip.source_offset.is_nan() || clip.source_offset < 0.0 {
                    return Err(format!(
                        "audio track #{ti} clip #{ci} has an invalid source_offset {}",
                        clip.source_offset
                    ));
                }
                if clip.source_len.is_nan() || clip.source_len <= 0.0 {
                    return Err(format!(
                        "audio track #{ti} clip #{ci} has a non-positive source_len {}",
                        clip.source_len
                    ));
                }
                let span = self.audio_clip_span_ticks(clip);
                if u64::from(clip.start_ticks) + u64::from(span) > MAX_ARRANGEMENT_TICKS {
                    return Err(format!(
                        "audio track #{ti} clip #{ci} ends beyond the {MAX_ARRANGEMENT_BARS}-bar arrangement bound"
                    ));
                }
            }
        }
        if u64::from(self.metadata.loop_range.end_ticks) > MAX_ARRANGEMENT_TICKS {
            return Err(format!(
                "song loop end {} exceeds the {MAX_ARRANGEMENT_BARS}-bar arrangement bound",
                self.metadata.loop_range.end_ticks
            ));
        }
        if self.metadata.markers.len() > MAX_MARKERS {
            return Err(format!(
                "marker count {} exceeds the limit {MAX_MARKERS}",
                self.metadata.markers.len()
            ));
        }
        for (i, marker) in self.metadata.markers.iter().enumerate() {
            if marker.name.is_empty() {
                return Err(format!("marker #{i} has an empty name"));
            }
            if u64::from(marker.tick) > MAX_ARRANGEMENT_TICKS {
                return Err(format!(
                    "marker #{i} ({}) lies beyond the {MAX_ARRANGEMENT_BARS}-bar arrangement bound",
                    marker.name
                ));
            }
        }
        self.graph
            .validate(self.midi_tracks.len(), self.control_tracks.len())
    }
}

/// Shared clip placement validation (D14/D15/D16).
fn check_clip_placement(
    track: usize,
    clip: usize,
    start_ticks: u32,
    span_ticks: u32,
    loop_ticks: u32,
) -> Result<(), String> {
    if loop_ticks == 0 {
        return Err(format!(
            "track #{track} clip #{clip} has loop_ticks = 0 (must be ≥ 1)"
        ));
    }
    if span_ticks < loop_ticks {
        return Err(format!(
            "track #{track} clip #{clip} has span_ticks {span_ticks} < loop_ticks {loop_ticks}"
        ));
    }
    if u64::from(start_ticks) + u64::from(span_ticks) > MAX_ARRANGEMENT_TICKS {
        return Err(format!(
            "track #{track} clip #{clip} ends beyond the {MAX_ARRANGEMENT_BARS}-bar arrangement bound"
        ));
    }
    Ok(())
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
    /// Song loop range (U1): the transport wraps from `end_ticks` back to
    /// `start_ticks` while playing. Disabled by default and whenever
    /// `end_ticks <= start_ticks`.
    #[serde(default)]
    pub loop_range: SongLoop,
    /// Timeline markers (U1): named jump targets on the arrangement ruler,
    /// kept sorted by tick. Additive field — old files load with none.
    #[serde(default)]
    pub markers: Vec<Marker>,
    /// Musical meter (U3a). Additive field: documents written before it
    /// existed load as 4/4, byte-identical to the old behavior — which is why
    /// `PROJECT_VERSION` stays at 6.
    #[serde(default)]
    pub time_signature: TimeSignature,
}

/// Song loop range (U1). See [`ProjectMetadata::loop_range`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SongLoop {
    /// Whether the range is active (the range can be kept while toggled off).
    pub enabled: bool,
    /// Loop start (ticks; inclusive).
    pub start_ticks: u32,
    /// Loop end (ticks; exclusive).
    pub end_ticks: u32,
}

impl SongLoop {
    /// Whether the range describes an actual loop (`end > start` while
    /// enabled). Mirrors the engine's degenerate-range rule.
    pub fn is_active(&self) -> bool {
        self.enabled && self.end_ticks > self.start_ticks
    }
}

/// Maximum number of timeline markers (U1).
pub const MAX_MARKERS: usize = 32;

/// One timeline marker (U1): a named jump target on the arrangement ruler.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Marker {
    /// Absolute position (ticks).
    pub tick: u32,
    /// Display name (non-empty; `Marker 1` style when created via the UI).
    pub name: String,
}

fn default_tempo() -> f64 {
    120.0
}

impl Default for ProjectMetadata {
    fn default() -> Self {
        Self {
            name: String::new(),
            tempo_bpm: default_tempo(),
            loop_range: SongLoop::default(),
            markers: Vec::new(),
            time_signature: TimeSignature::COMMON,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AudioClip, AudioTrack, AutoParam, AutoPoint, CableDoc, CableKind, DEFAULT_MIDI_TRACKS,
        EffectParams, GraphDoc, MASTER_NODE_ID, MAX_ARRANGEMENT_BARS, MAX_ARRANGEMENT_TICKS,
        MAX_AUDIO_TRACKS, MAX_BEND_RANGE, MAX_CLIP_NOTES, MAX_CLIPS_PER_TRACK, MAX_NODES,
        MIN_BEND_RANGE, MidiClip, MidiSource, MidiTrack, NodeDoc, PortRef, Project, SamplePlayMode,
        SynthParams, TICKS_PER_BAR, TICKS_PER_QUARTER, TICKS_PER_STEP, VoiceSpec,
        default_audio_source_len, default_graph_doc,
    };

    fn effect_node(params: EffectParams) -> NodeDoc {
        NodeDoc::Effect {
            node_id: 3,
            track: 0,
            voice: VoiceSpec::Builtin,
            params,
            delay: None,
        }
    }

    // E1: the effect payload's tag *is* the kind (D1), and the legacy sibling
    // field is never written again (D4).
    #[test]
    fn effect_params_serde_carries_the_kind_tag() {
        let node = effect_node(EffectParams::Delay(super::DelayParams {
            time_s: 0.42,
            feedback: 0.6,
            mix: 0.5,
            ..Default::default()
        }));
        let text = ron::to_string(&node).expect("serialize");
        assert!(text.contains("kind:\"delay\""), "tag missing: {text}");
        assert!(!text.contains("delay:Some"), "legacy field written: {text}");
        let back: NodeDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(node, back);
    }

    // E2: every effect kind round-trips through the same tagged payload, and
    // the tag always matches `kind()`.
    #[test]
    fn every_effect_kind_round_trips_with_its_tag() {
        let cases = [
            (EffectParams::Delay(super::DelayParams::default()), "delay"),
            (EffectParams::Eq(Box::default()), "eq"),
            (EffectParams::Compressor(Box::default()), "compressor"),
            (EffectParams::Reverb(Box::default()), "reverb"),
            (EffectParams::Chorus(Box::default()), "chorus"),
            (EffectParams::Flanger(Box::default()), "flanger"),
        ];
        for (params, tag) in cases {
            let text = ron::to_string(&effect_node(params.clone())).expect("serialize");
            assert!(
                text.contains(&format!("kind:\"{tag}\"")),
                "{tag} tag missing: {text}"
            );
            let back: NodeDoc = ron::from_str(&text).expect("deserialize");
            match back {
                NodeDoc::Effect { params: back, .. } => assert_eq!(back, params),
                other => panic!("expected an effect node, got {other:?}"),
            }
        }
    }

    // E2: a payload written by a newer build without some fields still loads
    // with the documented defaults (forward compatibility).
    #[test]
    fn partial_effect_payloads_take_their_defaults() {
        let node: NodeDoc = ron::from_str(
            r#"(type:"effect", node_id:3, track:0, voice:(type:"builtin"),
                params:(kind:"compressor"))"#,
        )
        .expect("a bare compressor payload should parse");
        let NodeDoc::Effect { params, .. } = node else {
            panic!("expected an effect node");
        };
        let comp = params.as_compressor().expect("compressor payload");
        assert_eq!(*comp, super::CompressorParams::default());
        assert!(comp.stereo_link, "compression starts linked (D13)");

        let node: NodeDoc = ron::from_str(
            r#"(type:"effect", node_id:3, track:0, voice:(type:"builtin"),
                params:(kind:"eq", low_gain:6.0))"#,
        )
        .expect("a partial EQ payload should parse");
        let NodeDoc::Effect { params, .. } = node else {
            panic!("expected an effect node");
        };
        let eq = params.as_eq().expect("eq payload");
        assert_eq!(eq.low_gain, 6.0);
        assert_eq!(eq.mid_q, 1.0);
        assert_eq!(eq.low_cut, super::EQ_LOW_CUT_OFF_HZ);
    }

    // E1: a v7 node (sibling `delay` payload) still parses, and the migration
    // moves those values into `params` (D3/D4).
    #[test]
    fn legacy_v7_delay_field_deserializes_and_migrates() {
        let text = r#"(type:"effect", node_id:3, track:0, voice:(type:"builtin"),
            delay:(time_s:0.42, feedback:0.6, mix:0.5))"#;
        let node: NodeDoc = ron::from_str(text).expect("v7 effect node should parse");
        let mut project = Project::default();
        project.graph.nodes = vec![node];
        project.migrate_v7_to_v8();

        match &project.graph.nodes[0] {
            NodeDoc::Effect { params, delay, .. } => {
                assert!(delay.is_none(), "the bridge must be consumed");
                assert_eq!(
                    params,
                    &EffectParams::Delay(super::DelayParams {
                        time_s: 0.42,
                        feedback: 0.6,
                        mix: 0.5,
                        ..Default::default()
                    })
                );
            }
            other => panic!("expected an effect node, got {other:?}"),
        }
    }

    // E1: an effect node without any payload keeps the delay defaults, which is
    // exactly what a v7 built-in effect meant.
    #[test]
    fn effect_params_default_to_delay() {
        let node: NodeDoc =
            ron::from_str(r#"(type:"effect", node_id:3, track:0, voice:(type:"builtin"))"#)
                .expect("payload-less effect node should parse");
        match node {
            NodeDoc::Effect { params, delay, .. } => {
                assert!(delay.is_none());
                assert_eq!(params.kind(), super::BuiltinEffectKind::Delay);
                assert_eq!(params, EffectParams::default());
            }
            other => panic!("expected an effect node, got {other:?}"),
        }
    }

    #[test]
    fn midi_clip_ron_roundtrip() {
        let mut clip = MidiClip {
            start_ticks: 2 * TICKS_PER_BAR,
            span_ticks: 4 * TICKS_PER_BAR,
            ..MidiClip::default()
        };
        clip.notes_mut().push(super::Note {
            start: 0,
            length: 480,
            note: 60,
            velocity: super::DEFAULT_VELOCITY,
            channel: 0,
        });
        clip.notes_mut().push(super::Note {
            start: 240,
            length: 240,
            note: 64,
            velocity: 127,
            channel: 0,
        });
        let text = ron::to_string(&clip).expect("serialization failed");
        let back: MidiClip = ron::from_str(&text).expect("deserialization failed");
        assert_eq!(clip, back);
    }

    #[test]
    fn midi_clip_partial_fields_use_defaults() {
        // Only `start_ticks` written: span/loop default to one bar.
        let back: MidiClip = ron::from_str("(start_ticks: 384)").expect("parse failed");
        assert_eq!(back.start_ticks, 384);
        assert_eq!(back.span_ticks, TICKS_PER_BAR);
        assert_eq!(back.loop_ticks, TICKS_PER_BAR);
        assert!(back.notes().is_empty());
    }

    /// U6/U11: audio track round-trip + partial defaults + old-file loading.
    #[test]
    fn audio_track_roundtrip_and_defaults() {
        let track = AudioTrack {
            name: "Vocal".to_string(),
            clips: vec![
                AudioClip {
                    path: "vox_1.wav".to_string(),
                    start_ticks: TICKS_PER_BAR,
                    source_offset: 0.25,
                    source_len: 2.5,
                    gain: 0.5,
                    ..AudioClip::default()
                },
                AudioClip::new("vox_2.wav", 8 * TICKS_PER_BAR),
            ],
        };
        let text = ron::to_string(&track).expect("serialization failed");
        let back: AudioTrack = ron::from_str(&text).expect("deserialization failed");
        assert_eq!(track, back);

        // Partial clip: only path + start written — the trim window defaults
        // to one bar at the default tempo (2.0 seconds).
        let clip: AudioClip =
            ron::from_str("(path: \"a.wav\", start_ticks: 960)").expect("parse failed");
        assert!((clip.source_len - default_audio_source_len()).abs() < 1e-9);
        assert_eq!(clip.source_offset, 0.0);
        assert_eq!(clip.gain, 1.0);
        assert_eq!(clip.legacy_span_ticks, 0);

        // Pre-U6 project files have no `audio_tracks` field: stripping it
        // from a serialized file mimics one, and it loads as empty.
        let legacy = ron::to_string(&Project::default())
            .expect("serialize")
            .replace("audio_tracks:[],", "");
        assert!(!legacy.contains("audio_tracks"));
        let parsed: Project = ron::from_str(&legacy).expect("legacy file must parse");
        assert!(parsed.audio_tracks.is_empty());
        parsed.validate().expect("legacy shape is valid");
    }

    /// U6: the audio-track validate section enforces the caps.
    #[test]
    fn audio_track_validation_rejects_overflow() {
        let mut project = Project {
            audio_tracks: vec![AudioTrack::default(); MAX_AUDIO_TRACKS],
            ..Project::default()
        };
        project.validate().expect("at the cap is fine");
        project.audio_tracks.push(AudioTrack::default());
        assert!(project.validate().is_err(), "beyond the cap must fail");

        project.audio_tracks.truncate(MAX_AUDIO_TRACKS);
        project.audio_tracks[0] = AudioTrack {
            clips: vec![AudioClip::default(); MAX_CLIPS_PER_TRACK + 1],
            ..AudioTrack::default()
        };
        assert!(
            project.validate().is_err(),
            "clip count beyond the per-track cap must fail"
        );

        project.audio_tracks[0] = AudioTrack {
            clips: vec![AudioClip {
                path: "x.wav".to_string(),
                start_ticks: 0,
                source_offset: 0.0,
                source_len: 0.0,
                gain: 1.0,
                ..AudioClip::default()
            }],
            ..AudioTrack::default()
        };
        assert!(
            project.validate().is_err(),
            "zero-length trim window must fail"
        );
    }

    /// U11: a v6 audio clip's tick span becomes seconds at the base tempo.
    #[test]
    fn audio_clip_v6_to_v7_migration() {
        let mut project = Project {
            audio_tracks: vec![AudioTrack {
                name: String::new(),
                clips: vec![AudioClip {
                    path: "a.wav".to_string(),
                    start_ticks: 0,
                    source_offset: 1.0,
                    source_len: 0.0,
                    gain: 1.0,
                    legacy_span_ticks: TICKS_PER_BAR,
                }],
            }],
            ..Project::default()
        };
        project.migrate_v6_to_v7();
        let clip = &project.audio_tracks[0].clips[0];
        // One bar at the default 120 bpm is 2 seconds.
        assert!(
            (clip.source_len - 2.0).abs() < 1e-9,
            "expected 2.0 s, got {}",
            clip.source_len
        );
        assert_eq!(clip.source_offset, 0.0);
        assert_eq!(clip.legacy_span_ticks, 0);
    }

    #[test]
    fn note_partial_fields_use_defaults() {
        let back: super::Note = ron::from_str("(start: 48, note: 72)").expect("parse failed");
        assert_eq!(
            back.length,
            i64::from(TICKS_PER_STEP),
            "note length defaults to one 16th note"
        );
        assert_eq!(back.velocity, super::DEFAULT_VELOCITY);
        assert_eq!(back.channel, 0);
        assert_eq!(back.note, 72);
    }

    #[test]
    fn sequence_sort_orders_notes_deterministically() {
        use super::{CtrlEvent, CtrlKind, Sequence};
        let mut seq = Sequence {
            notes: vec![
                super::Note {
                    start: 240,
                    note: 60,
                    ..Default::default()
                },
                super::Note {
                    start: 0,
                    note: 64,
                    ..Default::default()
                },
                super::Note {
                    start: 0,
                    note: 60,
                    ..Default::default()
                },
            ],
            ctrls: vec![
                CtrlEvent {
                    tick: 480,
                    channel: 0,
                    kind: CtrlKind::Cc(7),
                    value: 100,
                },
                CtrlEvent {
                    tick: 0,
                    channel: 0,
                    kind: CtrlKind::Cc(11),
                    value: 64,
                },
            ],
        };
        seq.sort();
        let notes: Vec<(i64, u8)> = seq.notes.iter().map(|n| (n.start, n.note)).collect();
        assert_eq!(notes, vec![(0, 60), (0, 64), (240, 60)]);
        let ticks: Vec<i64> = seq.ctrls.iter().map(|c| c.tick).collect();
        assert_eq!(ticks, vec![0, 480]);
    }

    #[test]
    fn remove_duplicates_keeps_the_longest_note() {
        use super::Sequence;
        let mut seq = Sequence {
            notes: vec![
                super::Note {
                    start: 0,
                    length: 240,
                    note: 60,
                    ..Default::default()
                },
                super::Note {
                    start: 0,
                    length: 960,
                    note: 60,
                    ..Default::default()
                },
                super::Note {
                    start: 0,
                    length: 480,
                    note: 60,
                    ..Default::default()
                },
                // Same start/pitch but another channel: not a duplicate.
                super::Note {
                    start: 0,
                    length: 240,
                    note: 60,
                    channel: 1,
                    ..Default::default()
                },
            ],
            ctrls: vec![],
        };
        assert_eq!(seq.remove_duplicates(), 2, "two stacked duplicates dropped");
        assert_eq!(seq.notes.len(), 2);
        assert_eq!(seq.notes[0].length, 960, "the longest survives");
        assert_eq!(seq.notes[0].channel, 0, "sorted back to channel 0 first");
        assert_eq!(seq.notes[1].channel, 1);
    }

    #[test]
    fn remove_duplicates_keeps_the_last_controller_event() {
        use super::{CtrlEvent, CtrlKind, Sequence};
        let mut seq = Sequence {
            notes: vec![],
            ctrls: vec![
                CtrlEvent {
                    tick: 0,
                    channel: 0,
                    kind: CtrlKind::Cc(7),
                    value: 10,
                },
                CtrlEvent {
                    tick: 0,
                    channel: 0,
                    kind: CtrlKind::Cc(7),
                    value: 90,
                },
                // Different tick: kept.
                CtrlEvent {
                    tick: 0,
                    channel: 0,
                    kind: CtrlKind::Cc(8),
                    value: 50,
                },
                // Different controller: kept.
                CtrlEvent {
                    tick: 0,
                    channel: 0,
                    kind: CtrlKind::PitchBend,
                    value: 0,
                },
            ],
        };
        assert_eq!(seq.remove_duplicates(), 1);
        assert_eq!(seq.ctrls.len(), 3);
        // The later value wins — that is what a synth would end up playing.
        let cc7 = seq
            .ctrls
            .iter()
            .find(|c| c.kind == CtrlKind::Cc(7))
            .unwrap();
        assert_eq!(cc7.value, 90);
    }

    #[test]
    fn sequence_queries_respect_the_half_open_range() {
        use super::Sequence;
        let seq = Sequence {
            notes: vec![
                super::Note {
                    start: 0,
                    length: 100,
                    note: 60,
                    ..Default::default()
                },
                super::Note {
                    start: 100,
                    length: 100,
                    note: 62,
                    ..Default::default()
                },
                super::Note {
                    start: 200,
                    length: 100,
                    note: 64,
                    ..Default::default()
                },
            ],
            ctrls: vec![],
        };
        // `notes_in` keys on the start tick: the note starting exactly at the
        // upper bound is excluded.
        assert_eq!(seq.notes_in(0, 200).count(), 2);
        // `notes_overlapping` keys on sounding anywhere inside: the note
        // starting at 100 is still sounding at the boundary.
        assert_eq!(seq.notes_overlapping(0, 200).count(), 2);
        assert_eq!(seq.notes_overlapping(150, 250).count(), 2);
        assert_eq!(seq.end_tick(), 300);
    }

    #[test]
    fn empty_sequence_end_tick_is_zero() {
        assert_eq!(super::Sequence::default().end_tick(), 0);
    }

    #[test]
    fn tick_base_divides_note_values_exactly() {
        // M1: the whole reason for 960 PPQ — every common subdivision must land
        // on a whole tick, including triplets (which a 96 PPQ base cannot do).
        assert_eq!(TICKS_PER_QUARTER, 960);
        assert_eq!(TICKS_PER_STEP, 240);
        assert_eq!(TICKS_PER_BAR, 3840);
        // 1/64 triplet = quarter / 6.
        assert_eq!(TICKS_PER_QUARTER / 6, 160);
        // Dotted 1/16 = 16th * 3/2.
        assert_eq!(TICKS_PER_STEP * 3 / 2, 360);
        // Triplet 1/8 = quarter / 3.
        assert_eq!(TICKS_PER_QUARTER / 3, 320);
    }

    #[test]
    fn meter_derives_the_bar_length_from_its_own_fields() {
        use super::TimeSignature;
        // 4/4 keeps the historical constant.
        assert_eq!(TimeSignature::COMMON.ticks_per_bar(), TICKS_PER_BAR);
        assert_eq!(TimeSignature::new(3, 4).ticks_per_bar(), 2880);
        // 6/8 and 3/4 span the SAME ticks but group differently: the only
        // difference a meter makes to arithmetic is the bar length.
        assert_eq!(TimeSignature::new(6, 8).ticks_per_bar(), 2880);
        assert_eq!(TimeSignature::new(7, 8).ticks_per_bar(), 3360);
        assert_eq!(TimeSignature::new(5, 4).ticks_per_bar(), 4800);
        // Beat length is the denominator's note value.
        assert_eq!(TimeSignature::new(3, 4).ticks_per_beat(), TICKS_PER_QUARTER);
        assert_eq!(TimeSignature::new(6, 8).ticks_per_beat(), 480);
    }

    #[test]
    fn meter_clamps_and_validates() {
        use super::TimeSignature;
        // Out-of-range input snaps instead of producing a zero-length bar.
        let clamped = TimeSignature::new(0, 0);
        assert_eq!(clamped.numerator, 1);
        assert_eq!(clamped.denominator, 1);
        assert!(clamped.ticks_per_bar() >= 1);
        // Non-power-of-two denominators round up to the next power of two.
        assert_eq!(TimeSignature::new(4, 3).denominator, 4);
        assert_eq!(TimeSignature::new(9, 64).denominator, 32);
        assert_eq!(TimeSignature::new(20, 4).numerator, 16);
        // Validation is strict: the parsed document must already be in range.
        assert!(TimeSignature::COMMON.is_valid());
        assert!(
            !TimeSignature {
                numerator: 0,
                denominator: 4
            }
            .is_valid()
        );
        assert!(
            !TimeSignature {
                numerator: 4,
                denominator: 3
            }
            .is_valid()
        );
        assert!(
            !TimeSignature {
                numerator: 4,
                denominator: 64
            }
            .is_valid()
        );
    }

    #[test]
    fn meter_is_additive_for_old_documents() {
        // A document written before U3a has no `time_signature` field: it must
        // load as 4/4 (no version bump, no migration branch).
        let metadata: super::ProjectMetadata =
            ron::from_str(r#"(name: "old", tempo_bpm: 120.0)"#).expect("parse");
        assert_eq!(metadata.time_signature, super::TimeSignature::COMMON);
        assert_eq!(metadata.time_signature.ticks_per_bar(), TICKS_PER_BAR);
        // A partial object (numerator only) still fills the denominator.
        let partial: super::TimeSignature = ron::from_str("(numerator: 3)").expect("parse");
        assert_eq!(partial, super::TimeSignature::new(3, 4));
    }

    #[test]
    fn note_end_uses_saturating_add() {
        let n = super::Note {
            start: i64::MAX - 2,
            length: i64::MAX,
            ..super::Note::default()
        };
        assert_eq!(n.end(), i64::MAX, "end() saturates instead of overflowing");
    }

    #[test]
    fn note_clamps_signed_drag_values_for_the_audio_thread() {
        // Drag gestures produce negative intermediates; the audio thread only
        // ever sees non-negative, in-range values.
        let n = super::Note {
            start: -500,
            length: 0,
            note: 200,
            channel: 99,
            velocity: 0,
        };
        assert_eq!(n.start_clamped(), 0, "negative start clamps to 0");
        assert_eq!(n.length_clamped(), 1, "zero length clamps to one tick");
        assert_eq!(n.velocity_f32(), f32::from(super::MIN_VELOCITY) / 127.0);
        assert_eq!(super::Note { velocity: 127, ..n }.velocity_f32(), 1.0);
    }

    #[test]
    fn default_project_graph_is_fully_explicit() {
        // D13/D18: the default document fully describes the default topology
        // (per-track voice → delay + one global mixer → master + the
        // computer-keyboard input node).
        let project = Project::default();
        assert_eq!(project.midi_tracks.len(), DEFAULT_MIDI_TRACKS);
        let graph = &project.graph;
        assert_eq!(
            graph.nodes.len(),
            DEFAULT_MIDI_TRACKS * 2 + 2,
            "two nodes per track + the global mixer + the keyboard input node"
        );
        // Cables: voice→delay + midi self-connection per track, then
        // delay→mixer.ch per track + mixer→master + keyboard→track 0 voice.
        assert_eq!(graph.cables.len(), DEFAULT_MIDI_TRACKS * 3 + 2);
        for t in 0..DEFAULT_MIDI_TRACKS {
            let voice = graph.instrument_of_track(t).expect("voice should exist");
            let delay = graph.builtin_delay_of_track(t).expect("delay should exist");
            assert_eq!(voice.node_id(), (MASTER_NODE_ID + 1) + (t as u64) * 2);
            let landing = graph
                .cables
                .iter()
                .find(|c| c.from == PortRef::Node(delay.node_id()))
                .expect("delay should feed the mixer");
            assert_eq!(landing.in_port, t as u8, "track t lands on channel t");
        }
        let mixer = graph.first_mixer().expect("global mixer should exist");
        assert!(matches!(mixer, NodeDoc::Mixer { inputs: 4, .. }));
        assert!(project.validate().is_ok());
    }

    #[test]
    fn project_ron_roundtrip_v5() {
        let mut project = Project::default();
        project.metadata.name = "demo".to_string();
        project.midi_tracks[0].name = "Bass".to_string();
        let mut clip = MidiClip::new(TICKS_PER_BAR);
        clip.span_ticks = 2 * TICKS_PER_BAR;
        clip.notes_mut().push(super::Note {
            start: 0,
            length: 480,
            note: 48,
            velocity: super::DEFAULT_VELOCITY,
            channel: 0,
        });
        project.midi_tracks[0].clips.push(clip);
        project.control_tracks.push(super::ControlTrack {
            name: "Gain Ride".to_string(),
            clips: vec![super::ControlClip {
                start_ticks: 0,
                span_ticks: TICKS_PER_BAR,
                loop_ticks: TICKS_PER_BAR,
                curve: vec![AutoPoint {
                    tick: 0,
                    value: 1.0,
                }],
                ..super::ControlClip::default()
            }],
        });
        // Control track 0 → global mixer (channel 0 gain).
        let mixer = project.graph.first_mixer().unwrap().node_id();
        project
            .graph
            .cables
            .push(CableDoc::automation(0, mixer, AutoParam::TrackGain));
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
        assert!(back.control_tracks.is_empty());
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
            ..Default::default()
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
            ..Default::default()
        };
        assert!(graph.validate(2, 0).is_err());

        // Cable references a nonexistent node.
        let graph = GraphDoc {
            nodes: Vec::new(),
            cables: vec![CableDoc::audio(9, PortRef::Master)],
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        };
        assert!(graph.validate(1, 0).is_err());
    }

    #[test]
    fn next_node_id_and_track_lookups() {
        let graph = default_graph_doc(2);
        // ids: track0 voice=2 delay=3, track1 voice=4 delay=5, mixer=6,
        // computer keyboard=7.
        assert_eq!(graph.next_node_id(), 8);
        assert_eq!(
            graph.instrument_of_track(1).map(NodeDoc::node_id),
            Some(MASTER_NODE_ID + 3)
        );
        assert!(graph.instrument_of_track(2).is_none());
        assert_eq!(graph.auto_endpoint(0), None);
    }

    /// Spec `plugin-gui.md` §8: a fresh project owns one computer-keyboard
    /// node already cabled to the first track's instrument — the computer
    /// keyboard plays out of the box, but through a visible route instead of
    /// an implicit track fallback.
    #[test]
    fn default_project_ships_one_routed_computer_keyboard() {
        let graph = default_graph_doc(DEFAULT_MIDI_TRACKS);
        let vk = graph.computer_keyboard().expect("default keyboard node");
        let first_voice = graph.instrument_of_track(0).expect("track 0 voice");
        assert_eq!(graph.keyboard_routes_from(vk), vec![first_voice.node_id()]);
        assert!(graph.validate(DEFAULT_MIDI_TRACKS, 0).is_ok());
        assert_eq!(
            graph.nodes.iter().filter(|n| n.is_keyboard()).count(),
            1,
            "exactly one input node by default"
        );
    }

    #[test]
    fn voice_spec_ron_roundtrip() {
        for voice in [
            VoiceSpec::Builtin,
            VoiceSpec::SoundFont {
                path: "General.sf2".to_string(),
                bank: 0,
                program: 40,
            },
            VoiceSpec::Sample {
                path: "kick.wav".to_string(),
                root_note: 60,
                play_mode: SamplePlayMode::Gate,
                gain: 1.0,
            },
            VoiceSpec::Sample {
                path: "hat.wav".to_string(),
                root_note: 48,
                play_mode: SamplePlayMode::OneShot,
                gain: 0.5,
            },
            VoiceSpec::Clap {
                path: "a.clap".to_string(),
                plugin_id: "x".to_string(),
                state: Vec::new(),
            },
            VoiceSpec::Clap {
                path: "a.clap".to_string(),
                plugin_id: "x".to_string(),
                state: vec![1, 2, 3],
            },
            VoiceSpec::Vst3 {
                path: "b.vst3".to_string(),
                class_id: "cid".to_string(),
                state: Vec::new(),
            },
            VoiceSpec::Vst3 {
                path: "b.vst3".to_string(),
                class_id: "cid".to_string(),
                state: vec![9, 8],
            },
        ] {
            let text = ron::to_string(&voice).expect("serialization failed");
            let back: VoiceSpec = ron::from_str(&text).expect("deserialization failed");
            assert_eq!(voice, back);
        }
    }

    /// Empty state must not appear in the file; old files without the field
    /// load as empty (serde default, D2).
    #[test]
    fn voice_spec_state_serde_compatibility() {
        let clap = VoiceSpec::Clap {
            path: "a.clap".to_string(),
            plugin_id: "x".to_string(),
            state: Vec::new(),
        };
        let text = ron::to_string(&clap).expect("serialization failed");
        assert!(
            !text.contains("state"),
            "empty state must be skipped: {text}"
        );
        // Legacy form (pre-state files; internally tagged, snake_case tag).
        let legacy =
            ron::from_str::<VoiceSpec>("(type: \"clap\", path: \"a.clap\", plugin_id: \"x\")")
                .expect("legacy form must deserialize");
        assert_eq!(legacy, clap);
    }

    /// SoundFont voice (spec `sf2-loader.md` D5/D6): internally tagged as
    /// `sound_font`; files without bank/program load as 0:0 (serde default,
    /// additive field -- PROJECT_VERSION untouched).
    #[test]
    fn voice_spec_soundfont_serde_compatibility() {
        let soundfont = VoiceSpec::SoundFont {
            path: "General.sf2".to_string(),
            bank: 0,
            program: 40,
        };
        let text = ron::to_string(&soundfont).expect("serialization failed");
        assert!(
            text.contains("type:\"sound_font\""),
            "tag must be snake_case sound_font: {text}"
        );
        // Older files (or hand-written projects) without bank/program: both
        // default to 0 = preset 0:0.
        let partial: VoiceSpec = ron::from_str("(type: \"sound_font\", path: \"General.sf2\")")
            .expect("fields must default");
        assert_eq!(
            partial,
            VoiceSpec::SoundFont {
                path: "General.sf2".to_string(),
                bank: 0,
                program: 0
            }
        );
    }

    /// Sample voice (spec `sample-player.md` D8): internally tagged as
    /// `sample`; files without root_note/play_mode/gain load as
    /// 60/Gate/1.0 (serde default, additive field -- PROJECT_VERSION
    /// untouched).
    #[test]
    fn voice_spec_sample_serde_compatibility() {
        let sample = VoiceSpec::Sample {
            path: "kick.wav".to_string(),
            root_note: 48,
            play_mode: SamplePlayMode::OneShot,
            gain: 0.5,
        };
        let text = ron::to_string(&sample).expect("serialization failed");
        assert!(
            text.contains("type:\"sample\""),
            "tag must be snake_case sample: {text}"
        );
        // Older files (or hand-written projects) without the optional
        // fields: root 60 (C4), Gate, unity gain.
        let partial: VoiceSpec =
            ron::from_str("(type: \"sample\", path: \"kick.wav\")").expect("fields must default");
        assert_eq!(
            partial,
            VoiceSpec::Sample {
                path: "kick.wav".to_string(),
                root_note: 60,
                play_mode: SamplePlayMode::Gate,
                gain: 1.0
            }
        );
        // Serialized names of the play mode variants are snake_case.
        assert!(
            text.contains("OneShot"),
            "play mode variant name must round-trip: {text}"
        );
        let back: VoiceSpec = ron::from_str(&text).expect("deserialization failed");
        assert_eq!(back, sample);
    }

    #[test]
    fn control_clip_value_at_segments() {
        let mut clip = super::ControlClip {
            curve: vec![
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
            ],
            ..super::ControlClip::default()
        };
        assert!((clip.value_at(0.0).unwrap() - 1.0).abs() < 1e-6);
        assert!(
            (clip.value_at(48.0).unwrap() - 0.75).abs() < 1e-6,
            "linear midpoint"
        );
        assert!((clip.value_at(192.0).unwrap()).abs() < 1e-6);
        assert!(
            (clip.value_at(1_000.0).unwrap()).abs() < 1e-6,
            "holds the end value past the tail"
        );
        assert!(
            (clip.value_at(-5.0).unwrap() - 1.0).abs() < 1e-6,
            "holds the end value before the head"
        );

        clip.curve.clear();
        assert!(
            clip.value_at(48.0).is_none(),
            "empty curve produces no automation"
        );
    }

    /// U12: `Smooth` is a monotone cubic — the value never leaves the range of
    /// its two bounding anchors (no overshoot), the peak anchor is hit
    /// exactly, and endpoints are held outside the curve.
    #[test]
    fn smooth_interp_is_overshoot_free_and_hits_anchors() {
        let clip = super::ControlClip {
            interp: super::CurveInterp::Smooth,
            curve: vec![
                AutoPoint {
                    tick: 0,
                    value: 0.0,
                },
                AutoPoint {
                    tick: 50,
                    value: 10.0,
                },
                AutoPoint {
                    tick: 100,
                    value: 0.0,
                },
            ],
            ..super::ControlClip::default()
        };
        assert!((clip.value_at(-1.0).unwrap()).abs() < 1e-6, "head held");
        assert!((clip.value_at(200.0).unwrap()).abs() < 1e-6, "tail held");
        assert!(
            (clip.value_at(50.0).unwrap() - 10.0).abs() < 1e-6,
            "peak anchor hit exactly"
        );
        for k in 0..=400 {
            let v = clip.value_at(f64::from(k) * 0.25).unwrap();
            assert!(
                (-1e-4..=10.0 + 1e-4).contains(&v),
                "overshoot at tick {}: {v}",
                k as f64 * 0.25
            );
        }
    }

    /// U12: a monotone rising anchor set stays monotone under `Smooth`
    /// (samples increase between consecutive anchors).
    #[test]
    fn smooth_interp_preserves_monotonicity() {
        let clip = super::ControlClip {
            interp: super::CurveInterp::Smooth,
            curve: vec![
                AutoPoint {
                    tick: 0,
                    value: 0.0,
                },
                AutoPoint {
                    tick: 96,
                    value: 3.0,
                },
                AutoPoint {
                    tick: 192,
                    value: 4.0,
                },
            ],
            ..super::ControlClip::default()
        };
        let mut prev = clip.value_at(0.0).unwrap();
        for k in 1..=192 {
            let v = clip.value_at(f64::from(k)).unwrap();
            assert!(v >= prev - 1e-5, "dip at tick {k}: {prev} -> {v}");
            prev = v;
        }
        assert!((v_at(&clip, 192.0) - 4.0).abs() < 1e-6);
    }

    fn v_at(clip: &super::ControlClip, tick: f64) -> f32 {
        clip.value_at(tick).unwrap()
    }

    /// U12: the new field is additive — an old document without `interp`
    /// reads back as the original (`Linear`) behavior, and the default clip
    /// carries `Linear`.
    #[test]
    fn curve_interp_defaults_to_linear_for_old_documents() {
        assert_eq!(
            super::ControlClip::default().interp,
            super::CurveInterp::Linear
        );
        let ron = "(start_ticks:0,span_ticks:384,loop_ticks:384,curve:[(tick:0,value:1.0)])";
        let clip: super::ControlClip = ron::from_str(ron).expect("legacy clip parses");
        assert_eq!(clip.interp, super::CurveInterp::Linear);
    }

    #[test]
    fn control_clip_upsert_keeps_order() {
        let mut clip = super::ControlClip::default();
        clip.upsert_point(AutoPoint {
            tick: 96,
            value: 0.5,
        });
        clip.upsert_point(AutoPoint {
            tick: 0,
            value: 1.0,
        });
        clip.upsert_point(AutoPoint {
            tick: 48,
            value: 0.75,
        });
        clip.upsert_point(AutoPoint {
            tick: 48,
            value: 0.8,
        }); // Overwrites the same tick
        let ticks: Vec<u32> = clip.curve.iter().map(|p| p.tick).collect();
        assert_eq!(
            ticks,
            vec![0, 48, 96],
            "anchors stay ascending and the same tick overwrites"
        );
        assert!((clip.curve[1].value - 0.8).abs() < 1e-6);
    }

    #[test]
    fn control_clip_remove_near() {
        let mut clip = super::ControlClip::default();
        clip.upsert_point(AutoPoint {
            tick: 48,
            value: 0.5,
        });
        assert!(clip.remove_near(50, 6), "should hit within tolerance");
        assert!(clip.curve.is_empty());
        assert!(
            !clip.remove_near(0, 6),
            "removal on an empty curve returns false"
        );
    }

    #[test]
    fn validate_rejects_too_many_clips() {
        let mut project = Project::default();
        for i in 0..=MAX_CLIPS_PER_TRACK {
            project.midi_tracks[0]
                .clips
                .push(MidiClip::new((i as u32) * TICKS_PER_BAR));
        }
        let err = project.validate().expect_err("should reject");
        assert!(err.contains("clips"), "unexpected error: {err}");
    }

    #[test]
    fn validate_rejects_too_many_notes() {
        let mut project = Project::default();
        let mut clip = MidiClip::default();
        for i in 0..=MAX_CLIP_NOTES {
            clip.notes_mut().push(super::Note {
                start: (i as i64) * i64::from(TICKS_PER_STEP),
                ..super::Note::default()
            });
        }
        project.midi_tracks[0].clips.push(clip);
        let err = project.validate().expect_err("should reject");
        assert!(err.contains("notes"), "unexpected error: {err}");
    }

    #[test]
    fn validate_rejects_span_shorter_than_loop() {
        let mut project = Project::default();
        project.midi_tracks[0].clips.push(MidiClip {
            start_ticks: 0,
            span_ticks: TICKS_PER_BAR,
            loop_ticks: 2 * TICKS_PER_BAR,
            ..MidiClip::default()
        });
        let err = project.validate().expect_err("should reject");
        assert!(err.contains("span_ticks"), "unexpected error: {err}");
    }

    #[test]
    fn validate_rejects_zero_loop() {
        let mut project = Project::default();
        project.midi_tracks[0].clips.push(MidiClip {
            start_ticks: 0,
            span_ticks: TICKS_PER_BAR,
            loop_ticks: 0,
            ..MidiClip::default()
        });
        assert!(project.validate().is_err());
    }

    #[test]
    fn validate_rejects_beyond_arrangement_bound() {
        let mut project = Project::default();
        project.midi_tracks[0].clips.push(MidiClip {
            start_ticks: u32::try_from(MAX_ARRANGEMENT_TICKS).unwrap(),
            span_ticks: TICKS_PER_BAR,
            loop_ticks: TICKS_PER_BAR,
            ..MidiClip::default()
        });
        let err = project.validate().expect_err("should reject");
        assert!(err.contains("bound"), "unexpected error: {err}");
    }

    #[test]
    fn validate_accepts_arrangement_ending_exactly_at_bound() {
        let mut project = Project::default();
        let last_start = u64::from(MAX_ARRANGEMENT_BARS - 1) * u64::from(TICKS_PER_BAR);
        project.midi_tracks[0].clips.push(MidiClip {
            start_ticks: u32::try_from(last_start).unwrap(),
            span_ticks: TICKS_PER_BAR,
            loop_ticks: TICKS_PER_BAR,
            ..MidiClip::default()
        });
        assert!(project.validate().is_ok());
    }

    #[test]
    fn validate_rejects_too_many_curve_points() {
        let mut project = Project::default();
        let mut clip = super::ControlClip::default();
        for i in 0..=super::MAX_CURVE_POINTS {
            clip.curve.push(AutoPoint {
                tick: (i as u32) * TICKS_PER_STEP,
                value: 0.5,
            });
        }
        project.control_tracks.push(super::ControlTrack {
            name: String::new(),
            clips: vec![clip],
        });
        let err = project.validate().expect_err("should reject");
        assert!(err.contains("anchors"), "unexpected error: {err}");
    }

    #[test]
    fn midi_track_defaults() {
        let track = MidiTrack::default();
        assert!(track.clips.is_empty());
        let _ = SynthParams::default();
    }

    // ---- G6: virtual keyboard node (spec `plugin-gui.md` §8) ----

    /// A document with two instrument nodes (ids 2 / 4) and no cables.
    fn vk_doc() -> GraphDoc {
        GraphDoc {
            nodes: vec![
                NodeDoc::Instrument {
                    node_id: 2,
                    track: 0,
                    voice: VoiceSpec::Builtin,
                    synth: SynthParams::default(),
                },
                NodeDoc::Instrument {
                    node_id: 4,
                    track: 1,
                    voice: VoiceSpec::Builtin,
                    synth: SynthParams::default(),
                },
                NodeDoc::Keyboard {
                    node_id: 6,
                    track: 0,
                    source: MidiSource::Computer,
                },
            ],
            cables: Vec::new(),
            layout: Default::default(),
        }
    }

    #[test]
    fn keyboard_node_is_document_only_and_keeps_identity() {
        let doc = vk_doc();
        let kb = doc.node_by_id(6).expect("keyboard node present");
        assert!(kb.is_keyboard());
        assert_eq!(kb.track(), 0);
        assert_eq!(doc.computer_keyboard(), Some(6));
        assert!(!doc.node_by_id(2).expect("instrument").is_keyboard());

        // serde: the bound source is part of the saved shape.
        let text = ron::to_string(&NodeDoc::Keyboard {
            node_id: 6,
            track: 2,
            source: MidiSource::Computer,
        })
        .expect("serialization failed");
        assert!(text.contains("keyboard"), "tag missing: {text}");
        assert!(text.contains("computer"), "source tag missing: {text}");
        let back: NodeDoc = ron::from_str(&text).expect("deserialization failed");
        assert_eq!(
            back,
            NodeDoc::Keyboard {
                node_id: 6,
                track: 2,
                source: MidiSource::Computer,
            }
        );
    }

    #[test]
    fn only_the_computer_source_answers_the_live_keyboard() {
        let mut doc = vk_doc();
        assert_eq!(doc.computer_keyboard(), Some(6));
        // A hardware-bound node is not the computer keyboard's owner.
        doc.nodes.push(NodeDoc::Keyboard {
            node_id: 8,
            track: 1,
            source: MidiSource::Device("KeyStep".to_string()),
        });
        assert_eq!(doc.computer_keyboard(), Some(6));

        let mut doc = vk_doc();
        doc.nodes.retain(|n| !n.is_keyboard());
        assert_eq!(doc.computer_keyboard(), None, "no node, no input");
    }

    #[test]
    fn keyboard_routes_without_cables_is_empty() {
        let doc = vk_doc();
        assert!(
            doc.keyboard_routes_from(6).is_empty(),
            "an unconnected source is silent (no track fallback)"
        );
        assert!(doc.keyboard_routes_from(2).is_empty(), "not a keyboard");
    }

    #[test]
    fn keyboard_routes_follow_the_midi_cable_to_the_instrument() {
        let mut doc = vk_doc();
        doc.cables.push(CableDoc::keyboard_midi(6, 4));
        assert_eq!(doc.keyboard_routes_from(6), vec![4]);
    }

    #[test]
    fn one_keyboard_feeds_every_cabled_instrument_in_document_order() {
        let mut doc = vk_doc();
        // Cables added in "reverse" document order: the result still follows
        // document order (id 2 before id 4), so layering is deterministic.
        doc.cables.push(CableDoc::keyboard_midi(6, 4));
        doc.cables.push(CableDoc::keyboard_midi(6, 2));
        assert_eq!(doc.keyboard_routes_from(6), vec![2, 4]);
    }

    /// Spec `plugin-gui.md` §8: one source, one node. Two nodes listening to
    /// the same device would double-trigger every key into two unrelated
    /// fan-outs, so the binding is a document-level error.
    #[test]
    fn validate_rejects_two_nodes_bound_to_the_same_source() {
        let mut doc = vk_doc();
        doc.nodes.push(NodeDoc::Keyboard {
            node_id: 8,
            track: 1,
            source: MidiSource::Computer,
        });
        let err = doc
            .validate(2, 0)
            .expect_err("two computer keyboard nodes must be rejected");
        assert!(err.contains("already owns"), "unexpected error: {err}");

        // Distinct sources are fine (hardware ports exist side by side).
        let mut doc = vk_doc();
        doc.nodes.push(NodeDoc::Keyboard {
            node_id: 8,
            track: 1,
            source: MidiSource::Device("KeyStep".to_string()),
        });
        assert!(doc.validate(2, 0).is_ok());
    }

    #[test]
    fn keyboard_routes_ignore_non_instrument_endpoints() {
        let mut doc = vk_doc();
        doc.nodes.push(NodeDoc::Effect {
            node_id: 8,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: EffectParams::default(),
            delay: None,
        });
        doc.cables.push(CableDoc::keyboard_midi(6, 8));
        assert!(
            doc.keyboard_routes_from(6).is_empty(),
            "MIDI never passes through an effect to an instrument"
        );
    }

    #[test]
    fn validate_allows_keyboard_midi_cable_only() {
        let mut doc = vk_doc();
        doc.cables.push(CableDoc::keyboard_midi(6, 2));
        assert!(doc.validate(2, 0).is_ok(), "vk → instrument MIDI is legal");

        // One vk out jack feeds several instruments: fan-out is a document
        // fact (the cap is enforced at dispatch, like the timeline's).
        let mut doc = vk_doc();
        doc.cables.push(CableDoc::keyboard_midi(6, 2));
        doc.cables.push(CableDoc::keyboard_midi(6, 4));
        assert!(
            doc.validate(2, 0).is_ok(),
            "one keyboard may cable several instruments (layering)"
        );

        // Audio cables touching a virtual keyboard are rejected both ways.
        let mut doc = vk_doc();
        doc.cables.push(CableDoc::audio(6, 2));
        assert!(doc.validate(2, 0).is_err(), "vk has no audio output");

        let mut doc = vk_doc();
        doc.cables.push(CableDoc::audio(2, 6));
        assert!(doc.validate(2, 0).is_err(), "vk has no audio input");

        // A MIDI cable sourced from a non-keyboard node stays illegal.
        let mut doc = vk_doc();
        doc.cables.push(CableDoc::keyboard_midi(2, 4));
        assert!(
            doc.validate(2, 0).is_err(),
            "only vk nodes have a MIDI out port"
        );
    }

    /// U3b (spec `tempo-meter.md` D1/D2/D7): the Timeline In terminal accepts
    /// exactly one thing — an automation cable carrying `Tempo`. Everything
    /// else is rejected by `validate`, and no extra singleton rule is needed
    /// (the identity is not instantiable).
    #[test]
    fn timeline_in_accepts_only_tempo_automation() {
        let mut doc = default_graph_doc(2);
        doc.cables.push(CableDoc::tempo(0));
        assert!(
            doc.validate(2, 1).is_ok(),
            "tempo → Timeline In is a legal automation cable"
        );
        assert_eq!(
            doc.auto_target(0),
            Some((PortRef::TimelineIn, AutoParam::Tempo))
        );
        assert!(doc.auto_targets_timeline_in(0));
        assert!(!doc.auto_targets_timeline_in(1), "track 1 is not cabled");

        // Audio cables have no port there (the terminal is a pure sink).
        let mut doc = default_graph_doc(2);
        doc.cables.push(CableDoc::audio(2, PortRef::TimelineIn));
        assert!(
            doc.validate(2, 0).is_err(),
            "audio cannot enter the terminal"
        );

        // Neither do MIDI cables.
        let mut doc = default_graph_doc(2);
        doc.cables.push(CableDoc {
            kind: CableKind::Midi,
            from: PortRef::MidiTrack(0),
            to: PortRef::TimelineIn,
            param: AutoParam::default(),
            in_port: 0,
        });
        assert!(
            doc.validate(2, 0).is_err(),
            "MIDI cannot enter the terminal"
        );

        // Tempo is a clock, not a node parameter: it must not reach a node.
        let mut doc = default_graph_doc(2);
        let voice = doc
            .instrument_of_track(0)
            .expect("track 0 has an instrument")
            .node_id();
        doc.cables
            .push(CableDoc::automation(0, voice, AutoParam::Tempo));
        assert!(
            doc.validate(2, 1).is_err(),
            "Tempo only exists on the Timeline In endpoint"
        );

        // v1 has a single port: other parameters cannot enter through it.
        let mut doc = default_graph_doc(2);
        doc.cables.push(CableDoc {
            kind: CableKind::Automation,
            from: PortRef::AutoTrack(0),
            to: PortRef::TimelineIn,
            param: AutoParam::TrackGain,
            in_port: 0,
        });
        assert!(
            doc.validate(2, 1).is_err(),
            "the terminal's only v1 port is Tempo"
        );
    }

    /// U3b: the new port/parameter tags are part of the saved format, so a
    /// document written now must read back identically (no version bump —
    /// both additions are behind a serde tag).
    #[test]
    fn tempo_cable_roundtrips_through_ron() {
        let cable = CableDoc::tempo(3);
        let text = ron::to_string(&cable).expect("serialize");
        assert!(text.contains("timeline_in"), "port tag: {text}");
        assert!(text.contains("Tempo"), "parameter tag: {text}");
        let back: CableDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(back, cable);
    }

    /// U4a: a CC destination is clamped into MIDI byte range on construction,
    /// so a hand-written document cannot produce a status byte out of range.
    #[test]
    fn cc_parameter_clamps_controller_and_channel() {
        let param = AutoParam::cc(200, 99);
        assert_eq!(param.cc_of(), Some((127, 15)));
        assert!(!param.is_timeline_in_only());
        assert_eq!(AutoParam::cc(74, 0).cc_of(), Some((74, 0)));
        assert_eq!(AutoParam::TrackGain.cc_of(), None);

        // The destination survives the saved format unchanged.
        let cable = CableDoc::automation(0, 2, AutoParam::cc(74, 3));
        let text = ron::to_string(&cable).expect("serialize");
        assert!(text.contains("Cc"), "parameter tag: {text}");
        let back: CableDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(back, cable);
    }

    /// P1/D9: the bend range is clamped into its documented span on
    /// construction, because the range is what makes the curve's semitone
    /// axis meaningful — a zero or negative one would divide it away.
    #[test]
    fn pitch_bend_parameter_clamps_range_and_channel() {
        let param = AutoParam::pitch_bend(99, 1_000.0);
        assert_eq!(param.pitch_bend_of(), Some((15, MAX_BEND_RANGE)));
        assert_eq!(
            AutoParam::pitch_bend(3, 0.0).pitch_bend_of(),
            Some((3, MIN_BEND_RANGE))
        );
        assert_eq!(
            AutoParam::pitch_bend(3, f32::NAN).pitch_bend_of(),
            Some((3, 2.0)),
            "a malformed range falls back to the GM default"
        );
        assert_eq!(
            AutoParam::pitch_bend(0, 2.0).pitch_bend_of(),
            Some((0, 2.0))
        );
        assert_eq!(AutoParam::TrackGain.pitch_bend_of(), None);
        assert!(!param.is_timeline_in_only());

        // The destination survives the saved format unchanged, and a document
        // written before P1 (no such variant) still loads.
        let cable = CableDoc::automation(0, 2, AutoParam::pitch_bend(1, 7.5));
        let text = ron::to_string(&cable).expect("serialize");
        assert!(text.contains("PitchBend"), "parameter tag: {text}");
        let back: CableDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(back, cable);
    }

    /// U4a: CC is an event stream for a node's MIDI input — it cannot enter
    /// the conductor terminal, and a node target accepts it.
    #[test]
    fn cc_cable_must_target_a_node() {
        let mut doc = default_graph_doc(2);
        let voice = doc
            .instrument_of_track(0)
            .expect("track 0 has an instrument")
            .node_id();
        doc.cables
            .push(CableDoc::automation(0, voice, AutoParam::cc(74, 0)));
        assert!(
            doc.validate(2, 1).is_ok(),
            "a CC cable into an instrument is legal"
        );

        let mut doc = default_graph_doc(2);
        doc.cables.push(CableDoc {
            kind: CableKind::Automation,
            from: PortRef::AutoTrack(0),
            to: PortRef::TimelineIn,
            param: AutoParam::cc(74, 0),
            in_port: 0,
        });
        assert!(
            doc.validate(2, 1).is_err(),
            "the terminal's only v1 port is Tempo"
        );
    }
}
