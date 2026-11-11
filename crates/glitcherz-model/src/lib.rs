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

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

pub mod file;
pub mod meter;
pub mod ops;
pub mod smf;

pub use meter::{MeterMap, MeterSegment};

pub use file::{
    AUTOSAVE_SUFFIX, ModelError, PROJECT_VERSION, ProjectFile, autosave_path, load_project,
    parse_project, save_autosave, save_project,
};
pub use ops::{
    DEFAULT_SEED, DurationOp, GlueOptions, QuantizeOptions, RandomizeOptions, StrumDirection,
    StrumOptions, VelocityOp, apply_duration, apply_to_selected, apply_velocity, assign_drum_notes,
    glue_selected, nearest_free_note, note_name, quantize, randomize, reassign_drum_slot,
    scale_notes, strum, transpose,
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

/// Mixer node capacity limits (P5f D15/D21): channel count 1..=32. A mixer
/// starts with a single channel and users grow it on demand.
pub const MIN_MIXER_CHANNELS: usize = 1;
pub const MAX_MIXER_CHANNELS: usize = 32;

/// Clips per track capacity limit (D15 as revised 2026-10-28; the engine side
/// `MAX_TRACK_CLIPS` uses the same value).
///
/// 32 since both clip tables moved to the structure queue: the limit no longer
/// multiplies into command-queue memory, so it can cover a real song (verse /
/// chorus / bridge edits on one track) instead of forcing a single clip.
pub const MAX_CLIPS_PER_TRACK: usize = 32;

/// Notes per MIDI clip capacity limit (D15 as revised 2026-10-28).
///
/// This is now a **document sanity bound only**: the engine keeps notes in a
/// control-side-owned `Vec` per clip (no fixed-capacity array), so a clip is
/// not truncated at playback time any more. The bound exists so a hostile or
/// corrupted file cannot make the editor allocate without limit; the importer
/// chunks dense tracks well below it (see `smf::IMPORT_CLIP_NOTES`).
pub const MAX_CLIP_NOTES: usize = 65_536;

/// Anchor points per control clip capacity limit (the engine-side
/// `MAX_AUTO_POINTS` uses the same value).
pub const MAX_CURVE_POINTS: usize = 64;

/// Arrangement length safety bound in bars (D16; guards pathological
/// documents).
pub const MAX_ARRANGEMENT_BARS: u32 = 256;

/// Number of MIDI tracks in the default (new) project. A fresh project ships
/// one playable MIDI track; further tracks start empty and are built by the
/// user.
pub const DEFAULT_MIDI_TRACKS: usize = 1;

/// Number of control (curve) tracks in the default (new) project.
pub const DEFAULT_CONTROL_TRACKS: usize = 1;

/// Number of audio tracks in the default (new) project.
pub const DEFAULT_AUDIO_TRACKS: usize = 1;

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

/// I1: the default register of a keyboard node.
///
/// The strip brackets `G3..G5` — middle C sits inside the window — and the
/// computer keyboard starts on the window's C.
pub const DEFAULT_KEYBOARD_OCTAVE: i8 = 4;

/// I1: the highest register a keyboard node can be moved to (the strip then
/// tops out exactly at MIDI 127).
pub const MAX_KEYBOARD_OCTAVE: i8 = 8;

/// serde default for [`NodeDoc::Keyboard::octave`] — old documents (written
/// before the register became a document field) land on the default window.
fn default_keyboard_octave() -> i8 {
    DEFAULT_KEYBOARD_OCTAVE
}

/// Built-in oscillator waveform (spec `.agents/spec/instrument.md` D1).
///
/// The engine keeps its own index form (`ParamId::OscWaveform` takes a rounded
/// `0..3`), exactly like the LFO's `LfoWaveform` and the effects'
/// `Unit::Enum` parameters; this enum is the document-side vocabulary and the
/// UI's label source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum SynthWaveform {
    /// `sin(2πφ)`.
    #[default]
    Sine,
    /// Piecewise triangle, starts at `-1`.
    Triangle,
    /// Rising ramp, `2φ - 1`.
    Saw,
    /// Half-period `+1` / `-1`.
    Square,
}

impl SynthWaveform {
    /// All waveforms, in engine index order (`ParamId::OscWaveform`).
    pub const ALL: [Self; 4] = [Self::Sine, Self::Triangle, Self::Saw, Self::Square];

    /// Short English label (UI combo).
    pub fn label(self) -> &'static str {
        match self {
            Self::Sine => "Sine",
            Self::Triangle => "Triangle",
            Self::Saw => "Saw",
            Self::Square => "Square",
        }
    }

    /// Engine parameter index (0..=3).
    pub fn index(self) -> u8 {
        match self {
            Self::Sine => 0,
            Self::Triangle => 1,
            Self::Saw => 2,
            Self::Square => 3,
        }
    }

    /// Index → waveform (unknown indices fall back to `Sine`).
    pub fn from_index(index: u8) -> Self {
        Self::ALL.get(index as usize).copied().unwrap_or(Self::Sine)
    }
}

/// Per-track synthesizer timbre parameters.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SynthParams {
    /// Oscillator waveform (spec `instrument.md`; default Sine keeps older
    /// documents sounding the same).
    #[serde(default)]
    pub waveform: SynthWaveform,
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
            waveform: SynthWaveform::Sine,
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

/// Modal strike presets (spec `.agents/spec/modal-instrument.md` §4).
///
/// The mode tables live in the engine (`realtime/modal.rs`); the document only
/// carries the choice, exactly like [`SynthWaveform`]. Index order must match
/// the engine's `PRESETS` array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModalPreset {
    /// Wooden / metal bar: short decay, close to harmonic.
    #[default]
    Mallet,
    /// Metal bar with motor-like beating on the low modes.
    Vibraphone,
    /// Inharmonic bell with a long tail.
    Bell,
    /// Singing bowl: very long tail, strong beating.
    SingingBowl,
}

impl ModalPreset {
    /// All presets, in engine index order.
    pub const ALL: [Self; 4] = [
        Self::Mallet,
        Self::Vibraphone,
        Self::Bell,
        Self::SingingBowl,
    ];

    /// Short English label (UI combo / node caption).
    pub fn label(self) -> &'static str {
        match self {
            Self::Mallet => "Mallet",
            Self::Vibraphone => "Vibraphone",
            Self::Bell => "Bell",
            Self::SingingBowl => "Singing Bowl",
        }
    }

    /// Engine parameter index (0..=3).
    pub fn index(self) -> u8 {
        match self {
            Self::Mallet => 0,
            Self::Vibraphone => 1,
            Self::Bell => 2,
            Self::SingingBowl => 3,
        }
    }

    /// Index → preset (unknown indices fall back to `Mallet`).
    pub fn from_index(index: u8) -> Self {
        Self::ALL
            .get(index as usize)
            .copied()
            .unwrap_or(Self::Mallet)
    }
}

/// Modal strike instrument parameters (spec `modal-instrument.md` §6).
///
/// Only macro controls: the per-mode tables are engine-side preset data
/// (B1-D7). Every field is `serde(default)`, so a payload written by an older
/// build still loads.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModalParams {
    /// Preset family.
    #[serde(default)]
    pub preset: ModalPreset,
    /// Multiplier over every mode's T60 (0.25..4).
    #[serde(default = "default_modal_decay")]
    pub decay_scale: f32,
    /// Multiplier over the preset's beating offsets (0..2; `0` = no beat).
    #[serde(default = "default_modal_beating")]
    pub beating: f32,
    /// Strike length / softness (0..1); affects the next strike only.
    #[serde(default = "default_modal_hardness")]
    pub hardness: f32,
    /// Strike brightness, i.e. the excitation low-pass (0..1); next strike only.
    #[serde(default = "default_modal_brightness")]
    pub brightness: f32,
    /// Node output gain (0..2).
    #[serde(default = "default_modal_gain")]
    pub gain: f32,
}

fn default_modal_decay() -> f32 {
    1.0
}

fn default_modal_beating() -> f32 {
    1.0
}

fn default_modal_hardness() -> f32 {
    0.7
}

fn default_modal_brightness() -> f32 {
    0.7
}

fn default_modal_gain() -> f32 {
    1.0
}

impl Default for ModalParams {
    fn default() -> Self {
        Self {
            preset: ModalPreset::default(),
            decay_scale: default_modal_decay(),
            beating: default_modal_beating(),
            hardness: default_modal_hardness(),
            brightness: default_modal_brightness(),
            gain: default_modal_gain(),
        }
    }
}

/// Plucked-string parameters (spec `pluck-instrument.md` §5).
///
/// Five macros, all continuous — no preset enum: a pluck "preset" would only
/// be a combination of these scalars and would go stale the moment one of
/// them moves (S1-D7). Every field is `serde(default)`, so a payload written
/// by an older build still loads.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PluckParams {
    /// Fundamental T60 in seconds (0.1..20).
    #[serde(default = "default_pluck_decay")]
    pub decay_s: f32,
    /// Loop low-pass darkness (0..1): `0` keeps `32·f0`, `1` leaves `4·f0`.
    #[serde(default = "default_pluck_damping")]
    pub damping: f32,
    /// Excitation low-pass (0..1); affects the next pluck only.
    #[serde(default = "default_pluck_brightness")]
    pub brightness: f32,
    /// Note-off damper T60 in seconds (0.005..2).
    #[serde(default = "default_pluck_release")]
    pub release_s: f32,
    /// Node output gain (0..2).
    #[serde(default = "default_pluck_gain")]
    pub gain: f32,
}

fn default_pluck_decay() -> f32 {
    4.0
}

fn default_pluck_damping() -> f32 {
    0.35
}

fn default_pluck_brightness() -> f32 {
    0.6
}

fn default_pluck_release() -> f32 {
    0.15
}

fn default_pluck_gain() -> f32 {
    1.0
}

impl Default for PluckParams {
    fn default() -> Self {
        Self {
            decay_s: default_pluck_decay(),
            damping: default_pluck_damping(),
            brightness: default_pluck_brightness(),
            release_s: default_pluck_release(),
            gain: default_pluck_gain(),
        }
    }
}

/// Drawbar organ parameters (spec `organ-instrument.md` §6).
///
/// The nine drawbar levels are the registration — plain integers 0..8, no
/// preset enum: a "registration" is exactly these nine numbers and would go
/// stale the moment one of them moves (S2-D14). Every field is
/// `serde(default)`, so a payload written by an older build still loads.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OrganParams {
    /// Drawbar levels 0..8 in footage order: 16', 5⅓', 8', 4', 2⅔', 2',
    /// 1⅗', 1⅓', 1'. The default is `888000000`.
    #[serde(default = "default_organ_bars")]
    pub bars: [u8; 9],
    /// Percussion partial level (0..1; `0` = off).
    #[serde(default)]
    pub percussion: f32,
    /// Percussion decay as a T60 in seconds (0.1..2).
    #[serde(default = "default_organ_percussion_decay")]
    pub percussion_decay_s: f32,
    /// `true` = the percussion partial is the 3rd harmonic (2⅔'), `false` the
    /// 2nd (4'). Discrete on purpose.
    #[serde(default)]
    pub percussion_third: bool,
    /// Key-switch attack ramp in seconds (0.001..0.2).
    #[serde(default = "default_organ_attack")]
    pub attack_s: f32,
    /// Key-switch release ramp in seconds (0.005..1).
    #[serde(default = "default_organ_release")]
    pub release_s: f32,
    /// Node output gain (0..2).
    #[serde(default = "default_organ_gain")]
    pub gain: f32,
}

fn default_organ_bars() -> [u8; 9] {
    [8, 8, 8, 0, 0, 0, 0, 0, 0]
}

fn default_organ_percussion_decay() -> f32 {
    0.6
}

fn default_organ_attack() -> f32 {
    0.005
}

fn default_organ_release() -> f32 {
    0.04
}

fn default_organ_gain() -> f32 {
    1.0
}

impl Default for OrganParams {
    fn default() -> Self {
        Self {
            bars: default_organ_bars(),
            percussion: 0.0,
            percussion_decay_s: default_organ_percussion_decay(),
            percussion_third: false,
            attack_s: default_organ_attack(),
            release_s: default_organ_release(),
            gain: default_organ_gain(),
        }
    }
}

/// Wavetable parameters (spec `wavetable-instrument.md` §6).
///
/// One timbre axis: `position` slides through the built-in frame bank (sine →
/// triangle → saw → square). There is no preset enum and no filter — the
/// position *is* the sound (S3-D15). Every field is `serde(default)`, so a
/// payload written by an older build still loads.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WavetableParams {
    /// Frame blend (0..1): `0` = pure sine, `1` = square.
    #[serde(default)]
    pub position: f32,
    /// Key-switch attack ramp in seconds (0.001..2).
    #[serde(default = "default_wavetable_attack")]
    pub attack_s: f32,
    /// Key-switch release ramp in seconds (0.005..5).
    #[serde(default = "default_wavetable_release")]
    pub release_s: f32,
    /// Node output gain (0..2).
    #[serde(default = "default_organ_gain")]
    pub gain: f32,
}

fn default_wavetable_attack() -> f32 {
    0.005
}

fn default_wavetable_release() -> f32 {
    0.12
}

impl Default for WavetableParams {
    fn default() -> Self {
        Self {
            position: 0.0,
            attack_s: default_wavetable_attack(),
            release_s: default_wavetable_release(),
            gain: default_organ_gain(),
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
    /// Detect from the Mod port instead of the node input (spec Z1).
    ///
    /// Off by default: the compressor works from its own input until the key
    /// is wired and the switch is enabled, so it never sits inert waiting for
    /// a cable (Z1 merge: every `Compressor` owns both the port and this
    /// switch).
    #[serde(default)]
    pub sidechain: bool,
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
            sidechain: false,
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

fn default_bitcrusher_drive() -> f32 {
    1.0
}

fn default_bitcrusher_bits() -> f32 {
    8.0
}

fn default_bitcrusher_downsample() -> f32 {
    2.0
}

/// Bitcrusher parameters (spec `effects.md` E5; slot order matches the engine
/// descriptor).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BitcrusherParams {
    /// Input gain before quantization.
    #[serde(default = "default_bitcrusher_drive")]
    pub drive: f32,
    /// Quantization depth in bits (2..16; fractional values give a continuous
    /// step size).
    #[serde(default = "default_bitcrusher_bits")]
    pub bits: f32,
    /// Sample & hold ratio (1 = off).
    #[serde(default = "default_bitcrusher_downsample")]
    pub downsample: f32,
    /// Dry/wet mix (`0` = fully dry).
    #[serde(default = "default_mix_one")]
    pub mix: f32,
}

impl Default for BitcrusherParams {
    fn default() -> Self {
        Self {
            drive: default_bitcrusher_drive(),
            bits: default_bitcrusher_bits(),
            downsample: default_bitcrusher_downsample(),
            mix: default_mix_one(),
        }
    }
}

fn default_phaser_rate() -> f32 {
    0.5
}

fn default_phaser_depth() -> f32 {
    0.7
}

fn default_phaser_feedback() -> f32 {
    0.3
}

fn default_phaser_spread() -> f32 {
    1.4
}

fn default_phaser_stages() -> u8 {
    1
}

/// Phaser parameters (spec `effects.md` E5; slot order matches the engine
/// descriptor).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PhaserParams {
    /// LFO rate (Hz).
    #[serde(default = "default_phaser_rate")]
    pub rate: f32,
    /// Sweep depth (0..1, ±3.5 octaves around 120 Hz at 1).
    #[serde(default = "default_phaser_depth")]
    pub depth: f32,
    /// Chain feedback (−0.9..0.9).
    #[serde(default = "default_phaser_feedback")]
    pub feedback: f32,
    /// Geometric frequency ratio between stages.
    #[serde(default = "default_phaser_spread")]
    pub spread: f32,
    /// All-pass stage count as the enum index (0 = 2, 1 = 4, 2 = 6, 3 = 8).
    #[serde(default = "default_phaser_stages")]
    pub stages: u8,
    /// Dry/wet mix (`0` = fully dry).
    #[serde(default = "default_half")]
    pub mix: f32,
}

impl Default for PhaserParams {
    fn default() -> Self {
        Self {
            rate: default_phaser_rate(),
            depth: default_phaser_depth(),
            feedback: default_phaser_feedback(),
            spread: default_phaser_spread(),
            stages: default_phaser_stages(),
            mix: default_half(),
        }
    }
}

fn default_saturation_drive() -> f32 {
    2.0
}

fn default_saturation_tone() -> f32 {
    20_000.0
}

/// Saturation parameters (spec `effects.md` E5; slot order matches the engine
/// descriptor).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SaturationParams {
    /// Shaper mode as the enum index (0 = Soft, 1 = Hard, 2 = Fold).
    #[serde(default)]
    pub mode: u8,
    /// Input drive (1..16).
    #[serde(default = "default_saturation_drive")]
    pub drive: f32,
    /// Trailing tone low pass (Hz; the 20 kHz default is near-off).
    #[serde(default = "default_saturation_tone")]
    pub tone: f32,
    /// Output trim in dB.
    #[serde(default)]
    pub trim: f32,
    /// Dry/wet mix (`0` = fully dry).
    #[serde(default = "default_mix_one")]
    pub mix: f32,
}

impl Default for SaturationParams {
    fn default() -> Self {
        Self {
            mode: 0,
            drive: default_saturation_drive(),
            tone: default_saturation_tone(),
            trim: 0.0,
            mix: default_mix_one(),
        }
    }
}

fn default_limiter_release() -> f32 {
    120.0
}

/// Lookahead limiter parameters (spec `effects.md` E5; slot order matches the
/// engine descriptor).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LimiterParams {
    /// Output ceiling in dB.
    #[serde(default)]
    pub threshold: f32,
    /// Gain release time in ms.
    #[serde(default = "default_limiter_release")]
    pub release: f32,
    /// Whether both channels share one detector (`max(|L|, |R|)`).
    #[serde(default = "default_true")]
    pub stereo_link: bool,
    /// Output trim in dB.
    #[serde(default)]
    pub output: f32,
}

impl Default for LimiterParams {
    fn default() -> Self {
        Self {
            threshold: 0.0,
            release: default_limiter_release(),
            stereo_link: true,
            output: 0.0,
        }
    }
}

/// Ring modulator parameters (Z0, spec `dual-audio-input.md` Z-D11).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RingModParams {
    /// Dry/wet mix (`0` = sample-exact passthrough, `1` = full ring mod).
    #[serde(default = "default_mix_one")]
    pub mix: f32,
}

impl Default for RingModParams {
    fn default() -> Self {
        Self {
            mix: default_mix_one(),
        }
    }
}

/// 16-band vocoder parameters (Z2; slot order matches the engine descriptor).
///
/// `Main` is the carrier, `Mod` is the modulator (spec `vocoder.md` Z2-D2).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VocoderParams {
    /// Envelope attack time (ms).
    #[serde(default = "default_vocoder_attack")]
    pub attack: f32,
    /// Envelope release time (ms).
    #[serde(default = "default_vocoder_release")]
    pub release: f32,
    /// Band quality factor.
    #[serde(default = "default_vocoder_q")]
    pub q: f32,
    /// Lowest band centre (Hz).
    #[serde(default = "default_vocoder_low")]
    pub low: f32,
    /// Highest band centre (Hz).
    #[serde(default = "default_vocoder_high")]
    pub high: f32,
    /// Noise-excited bypass above the band bank (`0` = off).
    #[serde(default = "default_vocoder_sibilance")]
    pub sibilance: f32,
    /// Dry/wet mix (`0` = sample-exact passthrough).
    #[serde(default = "default_mix_one")]
    pub mix: f32,
    /// Output trim (dB), wet path only.
    #[serde(default)]
    pub output: f32,
}

fn default_vocoder_attack() -> f32 {
    8.0
}

fn default_vocoder_release() -> f32 {
    60.0
}

fn default_vocoder_q() -> f32 {
    4.0
}

fn default_vocoder_low() -> f32 {
    120.0
}

fn default_vocoder_high() -> f32 {
    7_000.0
}

fn default_vocoder_sibilance() -> f32 {
    0.25
}

impl Default for VocoderParams {
    fn default() -> Self {
        Self {
            attack: default_vocoder_attack(),
            release: default_vocoder_release(),
            q: default_vocoder_q(),
            low: default_vocoder_low(),
            high: default_vocoder_high(),
            sibilance: default_vocoder_sibilance(),
            mix: default_mix_one(),
            output: 0.0,
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
    /// Feed-forward peak compressor. Owns a Mod port (Z1 merge): the keying
    /// switch decides whether it is read. The alias keeps pre-merge projects
    /// (`kind: "sidechain_compressor"`) loading unchanged.
    #[serde(alias = "sidechain_compressor")]
    Compressor,
    /// Stereo FDN-8 reverb.
    Reverb,
    /// Stereo chorus.
    Chorus,
    /// Stereo flanger.
    Flanger,
    /// Quantization + sample & hold lo-fi.
    Bitcrusher,
    /// All-pass cascade with an LFO-swept corner frequency.
    Phaser,
    /// 2× oversampled waveshaper.
    Saturation,
    /// Lookahead ceiling keeper.
    Limiter,
    /// Two-input ring modulator (Z0 validation effect).
    RingMod,
    /// 16-band vocoder: carrier on Main, modulator on Mod (Z2).
    Vocoder,
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
            Self::Bitcrusher => "Bitcrusher",
            Self::Phaser => "Phaser",
            Self::Saturation => "Saturation",
            Self::Limiter => "Limiter",
            Self::RingMod => "Ring Mod",
            Self::Vocoder => "Vocoder",
        }
    }

    /// Number of audio input ports (Z0). Dual-input effects answer 2.
    pub const fn audio_inputs(self) -> u8 {
        match self {
            Self::RingMod | Self::Compressor | Self::Vocoder => 2,
            _ => 1,
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
        BuiltinEffectKind::Bitcrusher,
        BuiltinEffectKind::Phaser,
        BuiltinEffectKind::Saturation,
        BuiltinEffectKind::Limiter,
        BuiltinEffectKind::RingMod,
        BuiltinEffectKind::Vocoder,
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
    /// Feed-forward peak compressor. Owns a Mod port (Z1 merge): keying stays
    /// dormant until the switch is on. The alias keeps pre-merge projects
    /// (`kind: "sidechain_compressor"`) loading unchanged.
    #[serde(alias = "sidechain_compressor")]
    Compressor(Box<CompressorParams>),
    /// Stereo FDN-8 reverb.
    Reverb(Box<ReverbParams>),
    /// Stereo chorus.
    Chorus(Box<ChorusParams>),
    /// Stereo flanger.
    Flanger(Box<FlangerParams>),
    /// Quantization + sample & hold lo-fi.
    Bitcrusher(Box<BitcrusherParams>),
    /// All-pass cascade with an LFO-swept corner frequency.
    Phaser(Box<PhaserParams>),
    /// 2× oversampled waveshaper.
    Saturation(Box<SaturationParams>),
    /// Lookahead ceiling keeper.
    Limiter(Box<LimiterParams>),
    /// Two-input ring modulator (Z0).
    RingMod(Box<RingModParams>),
    /// 16-band vocoder: carrier on Main, modulator on Mod (Z2).
    Vocoder(Box<VocoderParams>),
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
            Self::Bitcrusher(_) => BuiltinEffectKind::Bitcrusher,
            Self::Phaser(_) => BuiltinEffectKind::Phaser,
            Self::Saturation(_) => BuiltinEffectKind::Saturation,
            Self::Limiter(_) => BuiltinEffectKind::Limiter,
            Self::RingMod(_) => BuiltinEffectKind::RingMod,

            Self::Vocoder(_) => BuiltinEffectKind::Vocoder,
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

    /// Vocoder parameters, when this is a vocoder node.
    pub fn as_vocoder(&self) -> Option<&VocoderParams> {
        match self {
            Self::Vocoder(params) => Some(params),
            _ => None,
        }
    }

    /// Mutable vocoder parameters, when this is a vocoder node.
    pub fn as_vocoder_mut(&mut self) -> Option<&mut VocoderParams> {
        match self {
            Self::Vocoder(params) => Some(params),
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

    /// Bitcrusher parameters, when this is a bitcrusher node.
    pub fn as_bitcrusher(&self) -> Option<&BitcrusherParams> {
        match self {
            Self::Bitcrusher(params) => Some(params),
            _ => None,
        }
    }

    /// Mutable bitcrusher parameters, when this is a bitcrusher node.
    pub fn as_bitcrusher_mut(&mut self) -> Option<&mut BitcrusherParams> {
        match self {
            Self::Bitcrusher(params) => Some(params),
            _ => None,
        }
    }

    /// Phaser parameters, when this is a phaser node.
    pub fn as_phaser(&self) -> Option<&PhaserParams> {
        match self {
            Self::Phaser(params) => Some(params),
            _ => None,
        }
    }

    /// Mutable phaser parameters, when this is a phaser node.
    pub fn as_phaser_mut(&mut self) -> Option<&mut PhaserParams> {
        match self {
            Self::Phaser(params) => Some(params),
            _ => None,
        }
    }

    /// Saturation parameters, when this is a saturation node.
    pub fn as_saturation(&self) -> Option<&SaturationParams> {
        match self {
            Self::Saturation(params) => Some(params),
            _ => None,
        }
    }

    /// Mutable saturation parameters, when this is a saturation node.
    pub fn as_saturation_mut(&mut self) -> Option<&mut SaturationParams> {
        match self {
            Self::Saturation(params) => Some(params),
            _ => None,
        }
    }

    /// Limiter parameters, when this is a limiter node.
    pub fn as_limiter(&self) -> Option<&LimiterParams> {
        match self {
            Self::Limiter(params) => Some(params),
            _ => None,
        }
    }

    /// Mutable limiter parameters, when this is a limiter node.
    pub fn as_limiter_mut(&mut self) -> Option<&mut LimiterParams> {
        match self {
            Self::Limiter(params) => Some(params),
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

/// Upper bound of named clip groups (W3b).
pub const MAX_CLIP_GROUPS: usize = 64;

/// Label palette for clip groups (W3b): eight repeating RGB colours. The
/// model stores plain bytes so it never depends on a UI colour type.
pub const CLIP_GROUP_COLORS: [[u8; 3]; 8] = [
    [90, 150, 220],
    [220, 160, 90],
    [110, 190, 140],
    [200, 110, 180],
    [230, 200, 90],
    [120, 190, 210],
    [200, 120, 110],
    [160, 140, 220],
];

/// A named, coloured clip group (W3b, spec `timeline.md` D28).
///
/// Membership lives on the clips themselves (`group: Option<u32>`), so one
/// group can span MIDI / control / audio lanes and different tracks: moving
/// or copying one member moves/copies them all.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClipGroup {
    /// Group id (1..; unique per project).
    #[serde(default = "clip_group_default_id")]
    pub id: u32,
    #[serde(default)]
    pub name: String,
    /// Label colour (RGB) for the timeline stripe.
    #[serde(default = "clip_group_default_color")]
    pub color: [u8; 3],
}

impl Default for ClipGroup {
    fn default() -> Self {
        Self {
            id: clip_group_default_id(),
            name: String::new(),
            color: clip_group_default_color(),
        }
    }
}

fn clip_group_default_id() -> u32 {
    1
}

fn clip_group_default_color() -> [u8; 3] {
    CLIP_GROUP_COLORS[0]
}

/// A MIDI clip placed on the arrangement timeline (D14/D15).
///
/// `start_ticks` is the absolute position on the arrangement; the clip is
/// audible for `span_ticks` from there, and its note content repeats every
/// `loop_ticks` within the span (D14). A single-period clip has
/// `loop_ticks == span_ticks`; the timeline's Alt+T tool lowers `loop_ticks`
/// to tile the content. Sequence positions are relative to the clip start.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MidiClip {
    /// Optional user-facing clip name (W3a). Empty means unnamed: the
    /// timeline falls back to a content summary ("N notes").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Optional clip-group id (W3b): members move / copy / delete together,
    /// across tracks and lane kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u32>,
    /// Temporary clip mute (B2). A muted clip keeps its placement, content
    /// and length — the sequencer simply schedules nothing for it, so the
    /// toggle is non-destructive and costs no re-layout. Track mute stays on
    /// the mixer channel; this is the per-clip layer.
    #[serde(default, skip_serializing_if = "is_false")]
    pub muted: bool,
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

/// `serde(skip_serializing_if)` helper for flags that default to `false`:
/// a document only carries the key once it is actually set.
fn is_false(value: &bool) -> bool {
    !*value
}

impl Default for MidiClip {
    fn default() -> Self {
        Self {
            name: String::new(),
            group: None,
            muted: false,
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
    /// Optional user-facing clip name (W3a). Empty means unnamed: the
    /// timeline falls back to the wav file stem.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Optional clip-group id (W3b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u32>,
    /// Temporary clip mute (B2): the wav keeps its trim window and position,
    /// playback just skips it. Non-destructive (see [`MidiClip::muted`]).
    #[serde(default, skip_serializing_if = "is_false")]
    pub muted: bool,
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
            name: String::new(),
            group: None,
            muted: false,
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
    /// Optional user-facing clip name (W3a). Empty means unnamed: the
    /// timeline falls back to a content summary ("N anchors").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Optional clip-group id (W3b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u32>,
    /// Temporary clip mute (B2): the curve keeps its anchors and span, the
    /// automation is simply not delivered — the target holds its last value,
    /// exactly as outside the clip (see [`MidiClip::muted`]).
    #[serde(default, skip_serializing_if = "is_false")]
    pub muted: bool,
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
            name: String::new(),
            group: None,
            muted: false,
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
    ///
    /// U13: a node may be born with an **empty path** (no file dialog at
    /// add time) and stay silent until `Load File…` picks one; the path is
    /// re-pickable at any time.
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
        /// Node output trim (linear, 0..=2, default 1.0 = the font's own
        /// level). SoundFonts are routinely mastered quiet, so the trim
        /// belongs to the source instead of costing a graph node (U13).
        #[serde(default = "soundfont_default_gain")]
        gain: f32,
        /// Pitch-bend range in semitones (1..=24, default 2 = GM). Applied to
        /// the instrument with the standard RPN 0 sequence (spec
        /// `curve-map.md` D8/D9).
        #[serde(default = "soundfont_default_bend_range")]
        bend_range: f32,
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
        /// Play window and loop points inside the file (spec
        /// `sample-player.md` D12: head / loop / tail split). `default`
        /// keeps every pre-D12 project on the whole sample.
        #[serde(default)]
        region: SampleRegion,
    },
    /// Multi-sample drum kit (W1, spec `drum-kit.md`): each slot maps one
    /// MIDI key to a short wav. Sample data stays out of the document (the
    /// session `SampleLibrary` cache holds the decoded `Arc`s); an empty
    /// slot list is silent (U13's empty-node rule).
    DrumKit {
        #[serde(default)]
        slots: Vec<DrumSlot>,
        /// Node output trim (linear, 0..=2, default 1.0).
        #[serde(default = "sample_default_gain")]
        gain: f32,
    },
    /// Modal strike instrument (B1, spec `modal-instrument.md`): mallet,
    /// vibraphone, bell and singing bowl from one resonator engine.
    ///
    /// No external file and no "born empty" state: the mode tables are
    /// engine-side preset data, so a node is audible the moment it exists.
    Modal {
        #[serde(default)]
        params: ModalParams,
    },
    /// Plucked string (S1, spec `pluck-instrument.md`): Karplus-Strong delay
    /// line. No external file, no "born empty" state — the string is there
    /// the moment the node is.
    Pluck {
        #[serde(default)]
        params: PluckParams,
    },
    /// Drawbar organ (S2, spec `organ-instrument.md`): nine sine drawbars plus
    /// a decaying percussion partial. No external file, no "born empty" state.
    Organ {
        #[serde(default)]
        params: OrganParams,
    },
    /// Wavetable synthesizer (S3, spec `wavetable-instrument.md`): a built-in
    /// bank of band-limited single-cycle tables, blended by `position`. No
    /// external file, no "born empty" state.
    Wavetable {
        #[serde(default)]
        params: WavetableParams,
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

/// How a looped sample repeats (spec `sample-player.md` D12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum SampleLoopMode {
    /// No loop: the sample plays straight through (pre-D12 behavior).
    #[default]
    Off,
    /// Jump back to `loop_start` at `loop_end` (the usual sustain loop).
    Forward,
    /// Bounce: play forward to `loop_end`, then backwards to `loop_start`
    /// without a seam (LMMS / hardware sampler ping-pong).
    PingPong,
}

impl SampleLoopMode {
    /// Every mode, in the engine's parameter order (0 = Off).
    pub const ALL: [Self; 3] = [Self::Off, Self::Forward, Self::PingPong];

    /// Numeric index used by `ParamId::SampleLoopMode` and the UI radios.
    pub fn index(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Forward => 1,
            Self::PingPong => 2,
        }
    }

    /// UI label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Forward => "Forward",
            Self::PingPong => "Ping-pong",
        }
    }
}

/// Play window and loop points of a sample voice, in **source frames**
/// (spec `sample-player.md` D12/D13).
///
/// Frames, not seconds: the document must not care about the file's sample
/// rate, and trimming is a waveform gesture. `0` on `loop_end` / `end` means
/// "the end of the file": the document does not carry the length, and a
/// sample that is later replaced by a longer one should not keep an
/// accidental cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct SampleRegion {
    /// First frame of the head.
    #[serde(default)]
    pub start: u32,
    /// Loop window start (used while a Gate note is held).
    #[serde(default)]
    pub loop_start: u32,
    /// Loop window end / start of the tail. `0` = `end`.
    #[serde(default)]
    pub loop_end: u32,
    /// Exclusive end of the window. `0` = the last frame of the file.
    #[serde(default)]
    pub end: u32,
    /// Whether and how the loop window repeats.
    #[serde(default)]
    pub loop_mode: SampleLoopMode,
}

impl SampleRegion {
    /// Whether this region is the whole, unlooped sample (the pre-D12
    /// shape) — callers skip the trim UI's reset affordance on it.
    pub fn is_full_sample(&self) -> bool {
        *self == Self::default()
    }

    /// Resolves the window against a concrete sample length (`frames` =
    /// number of frame pairs in the file). Returns exclusive bounds plus
    /// the loop window, all clamped into the file.
    ///
    /// Shared by the engine-side trigger path and the UI's waveform view so
    /// a handle can never promise a frame the renderer will not reach.
    pub fn resolve(&self, frames: u32) -> ResolvedSampleRegion {
        // The last interpolatable frame pair (`i + 1` must exist).
        let last = frames.saturating_sub(1);
        let end = if self.end == 0 {
            last
        } else {
            self.end.min(last)
        };
        let start = self.start.min(end);
        let loop_end = if self.loop_end == 0 {
            end
        } else {
            self.loop_end.min(end)
        };
        let loop_start = self.loop_start.min(loop_end);
        ResolvedSampleRegion {
            start,
            loop_start,
            loop_end,
            end,
            loop_mode: self.loop_mode,
            // A zero-width window is not a loop: refusing here keeps every
            // renderer from having to re-check.
            loops: self.loop_mode != SampleLoopMode::Off && loop_end.saturating_sub(loop_start) > 1,
        }
    }
}

/// [`SampleRegion`] with every position resolved against the file length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSampleRegion {
    pub start: u32,
    pub loop_start: u32,
    pub loop_end: u32,
    pub end: u32,
    pub loop_mode: SampleLoopMode,
    /// `loop_mode != Off` **and** the loop window is wider than one frame.
    pub loops: bool,
}

impl Default for ResolvedSampleRegion {
    fn default() -> Self {
        Self {
            start: 0,
            loop_start: 0,
            loop_end: 0,
            end: 0,
            loop_mode: SampleLoopMode::Off,
            loops: false,
        }
    }
}

impl ResolvedSampleRegion {
    /// Back to explicit document values.
    ///
    /// This is the editing base for the waveform handles: a legacy file has
    /// every field at `0`, and the first drag must materialize the resolved
    /// whole-sample window instead of writing `0` back over it.
    pub fn to_region(self) -> SampleRegion {
        SampleRegion {
            start: self.start,
            loop_start: self.loop_start,
            loop_end: self.loop_end,
            end: self.end,
            loop_mode: self.loop_mode,
        }
    }
}

/// Capacity of one drum kit (W1, spec `drum-kit.md` K3). 32 overlapping
/// one-shot voices already exceed what a 24-voice engine pool can sustain,
/// so the document limit stays modest.
pub const MAX_DRUM_SLOTS: usize = 32;

/// Default trigger key of a new drum slot (GM kick, 36).
pub const DRUM_DEFAULT_NOTE: u8 = 36;

/// One key of a drum kit (W1, spec `drum-kit.md` K2): a short wav plus the
/// per-slot trim, placement and choke group. Sample data stays out of the
/// document — `path` is the only identity.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DrumSlot {
    /// MIDI key that triggers this slot (0..=127, unique per kit).
    #[serde(default = "drum_default_note")]
    pub note: u8,
    /// Path to the .wav file (decoded through the session sample cache).
    #[serde(default)]
    pub path: String,
    /// Slot trim (linear, 0..=2, default 1.0).
    #[serde(default = "sample_default_gain")]
    pub gain: f32,
    /// Stereo placement (-1..=1, default 0 = centre).
    #[serde(default)]
    pub pan: f32,
    /// Pitch trim in semitones (-24..=24, default 0 = native rate).
    #[serde(default)]
    pub tune_semitones: f32,
    /// Choke group: a new hit in the same group fades the old one out
    /// (closed vs open hi-hat, K7). `None` = no choke.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choke: Option<u8>,
}

impl Default for DrumSlot {
    fn default() -> Self {
        Self {
            note: DRUM_DEFAULT_NOTE,
            path: String::new(),
            gain: 1.0,
            pan: 0.0,
            tune_semitones: 0.0,
            choke: None,
        }
    }
}

fn drum_default_note() -> u8 {
    DRUM_DEFAULT_NOTE
}

/// Serde default for [`VoiceSpec::Sample`]'s `gain`.
fn sample_default_gain() -> f32 {
    1.0
}

/// Serde default for [`VoiceSpec::SoundFont`]'s `gain` (U13): unity = the
/// font's own level, which is also what the engine's processor is built
/// with.
fn soundfont_default_gain() -> f32 {
    1.0
}

/// Serde default for [`VoiceSpec::SoundFont`]'s `bend_range`: the GM value.
fn soundfont_default_bend_range() -> f32 {
    2.0
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

/// Node parameter address for automation (U4c, spec
/// `.agents/spec/control-tracks.md` §9): the *parameter* half of a
/// `(node, parameter)` cable endpoint.
///
/// The node half comes from the cable's destination port
/// ([`PortRef::Node`]), so this enum only has to say **which** parameter of
/// that node a curve drives. It mirrors the continuous / discrete node
/// parameters the engine understands (`ParamId`), but deliberately lives in
/// the model crate: the engine keeps its own `ParamId`, and the UI translates
/// one into the other in a single place (spec D16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NodeParam {
    /// Built-in effect parameter addressed by its **descriptor slot**
    /// (spec `effects.md` D5). The range comes from the node's own effect
    /// descriptor, so the axis needs the document (spec D18).
    EffectSlot(u8),
    /// One mixer channel's console parameter (D15/D21).
    MixerChannel {
        /// Channel index (0-based; must stay inside the mixer's `inputs`).
        channel: u8,
        /// Which console control the curve drives.
        param: ChannelParam,
    },
    /// Mixer output-stage pan (-1..1).
    MixerOutPan,
    /// Mixer output-stage mute (≥ 0.5 = muted).
    MixerOutMute,
    /// Feedback node tap gain (P6, 0..1).
    FeedbackGain,
    /// SoundFont node output gain (U13, 0..2).
    Sf2Gain,
    /// Sample-player node output gain (U5, 0..2).
    SampleGain,
    /// Instrument pitch offset (semitones, -24..24). The built-in and sample
    /// voices accept it directly; it is the pitch input for a control cable
    /// (spec `curve-map.md` D8).
    PitchOffset,
    /// Built-in synth filter resonance (0..0.98).
    Resonance,
    /// Built-in synth envelope attack (seconds, 0..2).
    Attack,
    /// Built-in synth envelope decay (seconds, 0..2).
    Decay,
    /// Built-in synth envelope sustain level (0..1).
    Sustain,
    /// Built-in synth envelope release (seconds, 0..3).
    Release,
    /// Modal instrument decay multiplier over every mode's T60
    /// (0.25..4; spec `modal-instrument.md` B1-D11).
    ModalDecay,
    /// Modal instrument beating multiplier (0..2; `0` = harmonic pairs).
    ModalBeating,
    /// Modal instrument strike hardness (0..1; next strike only).
    ModalHardness,
    /// Modal instrument strike brightness (0..1; next strike only).
    ModalBrightness,
    /// Modal instrument output gain (0..2).
    ModalGain,
    /// Plucked string: fundamental T60 in seconds (0.1..20; S1-D10).
    PluckDecay,
    /// Plucked string: loop low-pass darkness (0..1).
    PluckDamping,
    /// Plucked string: excitation low-pass (0..1; next pluck only).
    PluckBrightness,
    /// Plucked string: note-off damper T60 in seconds (0.005..2).
    PluckRelease,
    /// Plucked string: node output gain (0..2).
    PluckGain,
    /// Drawbar organ: level of one drawbar (payload = index 0..8; 0..8;
    /// spec `organ-instrument.md` S2-D11).
    OrganBar(u8),
    /// Drawbar organ: percussion partial level (0..1; next note-on only).
    OrganPercussion,
    /// Drawbar organ: percussion decay as a T60 in seconds (0.1..2).
    OrganPercussionDecay,
    /// Drawbar organ: key-switch attack ramp in seconds (0.001..0.2).
    ///
    /// Deliberately **not** [`NodeParam::Attack`]: that axis is 0..2, and an
    /// organ curve would spend most of its travel in the clamp region.
    OrganAttack,
    /// Drawbar organ: key-switch release ramp in seconds (0.005..1).
    OrganRelease,
    /// Drawbar organ: node output gain (0..2).
    OrganGain,
    /// Wavetable: frame blend (0..1; spec `wavetable-instrument.md` S3-D13).
    WavetablePosition,
    /// Wavetable: key-switch attack ramp in seconds (0.001..2).
    WavetableAttack,
    /// Wavetable: key-switch release ramp in seconds (0.005..5).
    WavetableRelease,
    /// Wavetable: node output gain (0..2).
    WavetableGain,
}

/// Which control of a mixer channel a curve drives ([`NodeParam::MixerChannel`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChannelParam {
    /// Fader (linear, 0..1.5).
    Gain,
    /// Pan (-1..1).
    Pan,
    /// Mute (≥ 0.5 = muted — a step curve is the point).
    Mute,
    /// Solo (≥ 0.5 = soloed).
    Solo,
}

impl ChannelParam {
    /// Stable snake_case key (F-039, spec `command-vocabulary.md` D2).
    pub fn key(&self) -> &'static str {
        match self {
            Self::Gain => "gain",
            Self::Pan => "pan",
            Self::Mute => "mute",
            Self::Solo => "solo",
        }
    }

    /// Inverse of [`Self::key`].
    pub fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "gain" => Self::Gain,
            "pan" => Self::Pan,
            "mute" => Self::Mute,
            "solo" => Self::Solo,
            _ => return None,
        })
    }
}

impl NodeParam {
    /// Stable snake_case key (F-039, spec `command-vocabulary.md` D1). This is
    /// the **public** CLI / MCP name: renaming a key is a breaking change, and
    /// every new variant must add a key plus a round-trip test.
    pub fn key(&self) -> &'static str {
        match self {
            Self::EffectSlot(_) => "effect_slot",
            Self::MixerChannel { .. } => "mixer_channel",
            Self::MixerOutPan => "mixer_out_pan",
            Self::MixerOutMute => "mixer_out_mute",
            Self::FeedbackGain => "feedback_gain",
            Self::Sf2Gain => "sf2_gain",
            Self::SampleGain => "sample_gain",
            Self::PitchOffset => "pitch_offset",
            Self::Resonance => "resonance",
            Self::Attack => "attack",
            Self::Decay => "decay",
            Self::Sustain => "sustain",
            Self::Release => "release",
            Self::ModalDecay => "modal_decay",
            Self::ModalBeating => "modal_beating",
            Self::ModalHardness => "modal_hardness",
            Self::ModalBrightness => "modal_brightness",
            Self::ModalGain => "modal_gain",
            Self::PluckDecay => "pluck_decay",
            Self::PluckDamping => "pluck_damping",
            Self::PluckBrightness => "pluck_brightness",
            Self::PluckRelease => "pluck_release",
            Self::PluckGain => "pluck_gain",
            Self::OrganBar(_) => "organ_bar",
            Self::OrganPercussion => "organ_percussion",
            Self::OrganPercussionDecay => "organ_percussion_decay",
            Self::OrganAttack => "organ_attack",
            Self::OrganRelease => "organ_release",
            Self::OrganGain => "organ_gain",
            Self::WavetablePosition => "wavetable_position",
            Self::WavetableAttack => "wavetable_attack",
            Self::WavetableRelease => "wavetable_release",
            Self::WavetableGain => "wavetable_gain",
        }
    }

    /// Inverse of [`Self::key`] for the parameterless variants.
    pub fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "mixer_out_pan" => Self::MixerOutPan,
            "mixer_out_mute" => Self::MixerOutMute,
            "feedback_gain" => Self::FeedbackGain,
            "sf2_gain" => Self::Sf2Gain,
            "sample_gain" => Self::SampleGain,
            "pitch_offset" => Self::PitchOffset,
            "resonance" => Self::Resonance,
            "attack" => Self::Attack,
            "decay" => Self::Decay,
            "sustain" => Self::Sustain,
            "release" => Self::Release,
            "modal_decay" => Self::ModalDecay,
            "modal_beating" => Self::ModalBeating,
            "modal_hardness" => Self::ModalHardness,
            "modal_brightness" => Self::ModalBrightness,
            "modal_gain" => Self::ModalGain,
            "pluck_decay" => Self::PluckDecay,
            "pluck_damping" => Self::PluckDamping,
            "pluck_brightness" => Self::PluckBrightness,
            "pluck_release" => Self::PluckRelease,
            "pluck_gain" => Self::PluckGain,
            "organ_percussion" => Self::OrganPercussion,
            "organ_percussion_decay" => Self::OrganPercussionDecay,
            "organ_attack" => Self::OrganAttack,
            "organ_release" => Self::OrganRelease,
            "organ_gain" => Self::OrganGain,
            "wavetable_position" => Self::WavetablePosition,
            "wavetable_attack" => Self::WavetableAttack,
            "wavetable_release" => Self::WavetableRelease,
            "wavetable_gain" => Self::WavetableGain,
            _ => return None,
        })
    }

    /// Indexed variants (`effect_slot`, `organ_bar`).
    pub fn from_key_indexed(key: &str, index: u8) -> Option<Self> {
        match key {
            "effect_slot" => Some(Self::EffectSlot(index)),
            "organ_bar" if index < 9 => Some(Self::OrganBar(index)),
            _ => None,
        }
    }
}

/// The `(min, max)` domain of a node parameter (F-039), mirroring
/// [`node_param_map`] without needing the graph: `graph.set_param` clamps to
/// it and `graph.query` reports it. `None` for the two address-based variants
/// (`effect_slot`, `mixer_channel`), whose domain comes from the node's own
/// descriptor / mixer channel.
pub fn node_param_range(param: NodeParam) -> Option<(f32, f32)> {
    Some(match param {
        NodeParam::EffectSlot(_) | NodeParam::MixerChannel { .. } => return None,
        NodeParam::MixerOutPan => (-1.0, 1.0),
        NodeParam::MixerOutMute => (0.0, 1.0),
        NodeParam::FeedbackGain => (0.0, 1.0),
        NodeParam::Sf2Gain | NodeParam::SampleGain => (0.0, 2.0),
        NodeParam::PitchOffset => (-24.0, 24.0),
        NodeParam::Resonance => (0.0, 0.98),
        NodeParam::Attack | NodeParam::Decay => (0.0, 2.0),
        NodeParam::Sustain => (0.0, 1.0),
        NodeParam::Release => (0.0, 3.0),
        NodeParam::ModalDecay => (0.25, 4.0),
        NodeParam::ModalBeating => (0.0, 2.0),
        NodeParam::ModalHardness | NodeParam::ModalBrightness => (0.0, 1.0),
        NodeParam::ModalGain => (0.0, 2.0),
        NodeParam::PluckDecay => (0.1, 20.0),
        NodeParam::PluckDamping | NodeParam::PluckBrightness => (0.0, 1.0),
        NodeParam::PluckRelease => (0.005, 2.0),
        NodeParam::PluckGain => (0.0, 2.0),
        NodeParam::OrganBar(_) => (0.0, 8.0),
        NodeParam::OrganPercussion => (0.0, 1.0),
        NodeParam::OrganPercussionDecay => (0.1, 2.0),
        NodeParam::OrganAttack => (0.001, 0.2),
        NodeParam::OrganRelease => (0.005, 1.0),
        NodeParam::OrganGain => (0.0, 2.0),
        NodeParam::WavetablePosition => (0.0, 1.0),
        NodeParam::WavetableAttack => (0.001, 2.0),
        NodeParam::WavetableRelease => (0.005, 5.0),
        NodeParam::WavetableGain => (0.0, 2.0),
    })
}

/// Reads a node parameter (F-039). `None` when the node kind does not carry
/// that parameter — the same condition that makes `graph.set_param` fail.
pub fn node_param_value(node: &NodeDoc, param: NodeParam) -> Option<f32> {
    match (node, param) {
        (NodeDoc::Instrument { synth, .. }, NodeParam::Resonance) => Some(synth.resonance),
        (NodeDoc::Instrument { synth, .. }, NodeParam::Attack) => Some(synth.attack_s),
        (NodeDoc::Instrument { synth, .. }, NodeParam::Decay) => Some(synth.decay_s),
        (NodeDoc::Instrument { synth, .. }, NodeParam::Sustain) => Some(synth.sustain),
        (NodeDoc::Instrument { synth, .. }, NodeParam::Release) => Some(synth.release_s),
        (NodeDoc::Mixer { channels, .. }, NodeParam::MixerChannel { channel, param }) => {
            let channel = channels.get(usize::from(channel))?;
            Some(match param {
                ChannelParam::Gain => channel.gain,
                ChannelParam::Pan => channel.pan,
                ChannelParam::Mute => bool_value(channel.muted),
                ChannelParam::Solo => bool_value(channel.solo),
            })
        }
        (NodeDoc::Mixer { out_pan, .. }, NodeParam::MixerOutPan) => Some(*out_pan),
        (NodeDoc::Mixer { out_muted, .. }, NodeParam::MixerOutMute) => Some(bool_value(*out_muted)),
        (NodeDoc::Feedback { gain, .. }, NodeParam::FeedbackGain) => Some(*gain),
        _ => None,
    }
}

/// Writes a node parameter in place (F-039), clamping to
/// [`node_param_range`]. Returns `false` when the node kind does not carry that
/// parameter (or `value` is not finite).
pub fn set_node_param(node: &mut NodeDoc, param: NodeParam, value: f32) -> bool {
    if !value.is_finite() {
        return false;
    }
    match (node, param) {
        (NodeDoc::Instrument { synth, .. }, NodeParam::Resonance) => {
            synth.resonance = value.clamp(0.0, 0.98);
            true
        }
        (NodeDoc::Instrument { synth, .. }, NodeParam::Attack) => {
            synth.attack_s = value.clamp(0.0, 2.0);
            true
        }
        (NodeDoc::Instrument { synth, .. }, NodeParam::Decay) => {
            synth.decay_s = value.clamp(0.0, 2.0);
            true
        }
        (NodeDoc::Instrument { synth, .. }, NodeParam::Sustain) => {
            synth.sustain = value.clamp(0.0, 1.0);
            true
        }
        (NodeDoc::Instrument { synth, .. }, NodeParam::Release) => {
            synth.release_s = value.clamp(0.0, 3.0);
            true
        }
        (NodeDoc::Mixer { channels, .. }, NodeParam::MixerChannel { channel, param }) => {
            let Some(channel) = channels.get_mut(usize::from(channel)) else {
                return false;
            };
            match param {
                ChannelParam::Gain => channel.gain = value.clamp(0.0, 1.5),
                ChannelParam::Pan => channel.pan = value.clamp(-1.0, 1.0),
                ChannelParam::Mute => channel.muted = value >= 0.5,
                ChannelParam::Solo => channel.solo = value >= 0.5,
            }
            true
        }
        (NodeDoc::Mixer { out_pan, .. }, NodeParam::MixerOutPan) => {
            *out_pan = value.clamp(-1.0, 1.0);
            true
        }
        (NodeDoc::Mixer { out_muted, .. }, NodeParam::MixerOutMute) => {
            *out_muted = value >= 0.5;
            true
        }
        (NodeDoc::Feedback { gain, .. }, NodeParam::FeedbackGain) => {
            *gain = value.clamp(0.0, 1.0);
            true
        }
        _ => false,
    }
}

/// The parameters a node supports today (F-039 `graph.query`), in a stable
/// order. Address-based `effect_slot` / voice-specific axes are added as their
/// nodes gain first-class read/write support.
pub fn node_param_keys(node: &NodeDoc) -> Vec<NodeParam> {
    match node {
        NodeDoc::Instrument { .. } => vec![
            NodeParam::Resonance,
            NodeParam::Attack,
            NodeParam::Decay,
            NodeParam::Sustain,
            NodeParam::Release,
        ],
        NodeDoc::Mixer { channels, .. } => {
            let mut keys = Vec::new();
            for channel in 0..channels.len() as u8 {
                for param in [
                    ChannelParam::Gain,
                    ChannelParam::Pan,
                    ChannelParam::Mute,
                    ChannelParam::Solo,
                ] {
                    keys.push(NodeParam::MixerChannel { channel, param });
                }
            }
            keys.push(NodeParam::MixerOutPan);
            keys.push(NodeParam::MixerOutMute);
            keys
        }
        NodeDoc::Feedback { .. } => vec![NodeParam::FeedbackGain],
        _ => Vec::new(),
    }
}

/// Boolean parameters use the 0/1 convention of the automation axes (F-039 D4).
fn bool_value(value: bool) -> f32 {
    if value { 1.0 } else { 0.0 }
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
    /// A node parameter addressed by the cable's destination port (U4c, spec
    /// `.agents/spec/control-tracks.md` §9).
    ///
    /// Unlike the legacy variants above (which are really "the track's
    /// fader" / "the filter cutoff of whatever node this lands on"), this one
    /// is explicit about *which* parameter of the target node moves — the
    /// difference between a mixer channel fader and its pan, or between two
    /// slots of the same effect, becomes expressible without growing this
    /// enum per node kind.
    Node(NodeParam),
    /// The control input of a [`NodeDoc::CurveMap`] node (spec `curve-map.md`
    /// D5).
    ///
    /// Deliberately *not* a parameter: the node has nothing to set on that
    /// port, it only forwards the normalized signal. The cable therefore
    /// carries the identity map ([`MapSpec::UNIT`]), so the value arrives
    /// untouched — the node's own axis applies to its **output**, not here.
    CurveMap,
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

    /// Effective bend range of this destination, in semitones.
    ///
    /// SF2's range is a property of the **instrument** (`VoiceSpec::SoundFont`
    /// `bend_range`, spec `curve-map.md` D8), not of the cable: reading it from
    /// the node here is what lets an inspector edit move an existing curve
    /// instead of leaving a stale copy behind. Everything else falls back to
    /// the value recorded on the cable, then to the GM default.
    pub fn bend_range(&self, graph: &GraphDoc, target: &PortRef) -> f32 {
        if let PortRef::Node(id) = target
            && let Some(NodeDoc::Instrument {
                voice: VoiceSpec::SoundFont { bend_range, .. },
                ..
            }) = graph.node_by_id(*id)
            && bend_range.is_finite()
            && *bend_range > 0.0
        {
            return *bend_range;
        }
        match self {
            AutoParam::PitchBend {
                range_semitones, ..
            } if range_semitones.is_finite() && *range_semitones > 0.0 => *range_semitones,
            _ => default_bend_range(),
        }
    }

    /// Default mapping of this destination's axis (spec `curve-map.md` D2).
    ///
    /// The one place that knows "how big is this parameter": the curve editor
    /// (axis labels), the cable compiler (engine `MapSpec`) and the v8 → v9
    /// migration all read it, so a curve that moves to another target keeps
    /// its shape everywhere at once.
    ///
    /// `graph` / `target` supply the node context an effect slot needs (its
    /// range lives in the node's [`EffectParams`], not in the enum).
    pub fn default_map(&self, graph: &GraphDoc, target: &PortRef) -> MapSpec {
        match self {
            AutoParam::TrackGain => MapSpec::linear(0.0, 1.5),
            AutoParam::TrackPan => MapSpec::linear(-1.0, 1.0),
            AutoParam::FilterCutoff => MapSpec::log(20.0, 18_000.0),
            AutoParam::Tempo => MapSpec::log(MIN_TEMPO_BPM, MAX_TEMPO_BPM),
            // Plugin range is not readable through the wiring this app has;
            // the clip rectangle is the 0..1 the plugin itself speaks.
            AutoParam::PluginParam { .. } => MapSpec::linear(0.0, 1.0),
            // 7-bit wire range (U4a D7).
            AutoParam::Cc { .. } => MapSpec::linear(0.0, 127.0),
            // Symmetric about zero: `range_semitones` is full-scale (P1 D9).
            AutoParam::PitchBend { .. } => {
                let range = self.bend_range(graph, target);
                MapSpec::linear(-range, range)
            }
            AutoParam::Node(param) => node_param_map(*param, graph, target),
            // The Curve Map input takes the signal as-is (D5/D6): its own
            // axis belongs to the hop *after* the node.
            AutoParam::CurveMap => MapSpec::UNIT,
        }
    }
}

/// Context-free map of one [`NodeParam`] (ranges mirror the panel sliders —
/// one source of truth per parameter, spec `control-tracks.md` §9).
///
/// Effect slots have no context-free range: they fall back to 0..1 and are
/// resolved against the node's payload by [`effect_slot_map`].
pub fn node_param_map(param: NodeParam, graph: &GraphDoc, target: &PortRef) -> MapSpec {
    match param {
        NodeParam::EffectSlot(slot) => effect_slot_map(graph, target, slot),
        NodeParam::MixerChannel { param, .. } => match param {
            ChannelParam::Gain => MapSpec::linear(0.0, 1.5),
            ChannelParam::Pan => MapSpec::linear(-1.0, 1.0),
            ChannelParam::Mute | ChannelParam::Solo => MapSpec::linear(0.0, 1.0),
        },
        NodeParam::MixerOutPan => MapSpec::linear(-1.0, 1.0),
        NodeParam::MixerOutMute => MapSpec::linear(0.0, 1.0),
        NodeParam::FeedbackGain => MapSpec::linear(0.0, 1.0),
        NodeParam::Sf2Gain | NodeParam::SampleGain => MapSpec::linear(0.0, 2.0),
        NodeParam::PitchOffset => MapSpec::linear(-24.0, 24.0),
        NodeParam::Resonance => MapSpec::linear(0.0, 0.98),
        NodeParam::Attack | NodeParam::Decay => MapSpec::linear(0.0, 2.0),
        NodeParam::Sustain => MapSpec::linear(0.0, 1.0),
        NodeParam::Release => MapSpec::linear(0.0, 3.0),
        NodeParam::ModalDecay => MapSpec::linear(0.25, 4.0),
        NodeParam::ModalBeating => MapSpec::linear(0.0, 2.0),
        NodeParam::ModalHardness | NodeParam::ModalBrightness => MapSpec::linear(0.0, 1.0),
        NodeParam::ModalGain => MapSpec::linear(0.0, 2.0),
        NodeParam::PluckDecay => MapSpec::linear(0.1, 20.0),
        NodeParam::PluckDamping | NodeParam::PluckBrightness => MapSpec::linear(0.0, 1.0),
        NodeParam::PluckRelease => MapSpec::linear(0.005, 2.0),
        NodeParam::PluckGain => MapSpec::linear(0.0, 2.0),
        NodeParam::OrganBar(_) => MapSpec::linear(0.0, 8.0),
        NodeParam::OrganPercussion => MapSpec::linear(0.0, 1.0),
        NodeParam::OrganPercussionDecay => MapSpec::linear(0.1, 2.0),
        NodeParam::OrganAttack => MapSpec::linear(0.001, 0.2),
        NodeParam::OrganRelease => MapSpec::linear(0.005, 1.0),
        NodeParam::OrganGain => MapSpec::linear(0.0, 2.0),
        NodeParam::WavetablePosition => MapSpec::linear(0.0, 1.0),
        NodeParam::WavetableAttack => MapSpec::linear(0.001, 2.0),
        NodeParam::WavetableRelease => MapSpec::linear(0.005, 5.0),
        NodeParam::WavetableGain => MapSpec::linear(0.0, 2.0),
    }
}

/// Map of one built-in effect parameter slot (spec `effects.md` D6).
///
/// The engine's `EffectDescriptor` is the slider's source of truth, but the
/// engine crate is not a dependency of the model, so this table mirrors it
/// slot by slot. `glitcherz-ui`'s `effect_axis_matches_descriptor` test walks
/// every descriptor and fails on drift.
///
/// A plugin effect (or a node that is not an effect at all) has no descriptor
/// here: 0..1 is what a plugin parameter speaks anyway.
pub fn effect_slot_map(graph: &GraphDoc, target: &PortRef, slot: u8) -> MapSpec {
    let Some(NodeDoc::Effect { params, .. }) = (match target {
        PortRef::Node(id) => graph.node_by_id(*id),
        _ => None,
    }) else {
        return MapSpec::linear(0.0, 1.0);
    };
    effect_slot_axis(params, slot)
}

/// Range + mode of one slot of a built-in effect payload.
pub fn effect_slot_axis(params: &EffectParams, slot: u8) -> MapSpec {
    use MapSpec as M;
    match (params, slot) {
        // time (s), feedback, mix, ping-pong, sync, division (0..7).
        (EffectParams::Delay(_), 0) => M::log(0.005, 1.0),
        (EffectParams::Delay(_), 1) => M::linear(0.0, 0.9),
        (EffectParams::Delay(_), 2) => M::linear(0.0, 1.0),
        (EffectParams::Delay(_), 3) => M::linear(0.0, 1.0),
        (EffectParams::Delay(_), 4) => M::linear(0.0, 1.0),
        (EffectParams::Delay(_), 5) => M::linear(0.0, 7.0),
        // low gain (dB), low freq, mid gain, mid freq, mid Q, high gain,
        // high freq, low cut.
        (EffectParams::Eq(_), 0) => M::linear(-18.0, 18.0),
        (EffectParams::Eq(_), 1) => M::log(30.0, 500.0),
        (EffectParams::Eq(_), 2) => M::linear(-18.0, 18.0),
        (EffectParams::Eq(_), 3) => M::log(200.0, 5_000.0),
        (EffectParams::Eq(_), 4) => M::log(0.3, 8.0),
        (EffectParams::Eq(_), 5) => M::linear(-18.0, 18.0),
        (EffectParams::Eq(_), 6) => M::log(1_500.0, 16_000.0),
        (EffectParams::Eq(_), 7) => M::log(20.0, 500.0),
        // threshold (dB), ratio, attack (ms), release (ms), knee (dB),
        // makeup (dB), mix, stereo link, keying switch (Z1 merge).
        (EffectParams::Compressor(_), 0) => M::linear(-60.0, 0.0),
        (EffectParams::Compressor(_), 1) => M::log(1.0, 20.0),
        (EffectParams::Compressor(_), 2) => M::log(0.1, 200.0),
        (EffectParams::Compressor(_), 3) => M::log(5.0, 2_000.0),
        (EffectParams::Compressor(_), 4) => M::linear(0.0, 24.0),
        (EffectParams::Compressor(_), 5) => M::linear(0.0, 24.0),
        (EffectParams::Compressor(_), 6) => M::linear(0.0, 1.0),
        (EffectParams::Compressor(_), 7) => M::linear(0.0, 1.0),
        (EffectParams::Compressor(_), 8) => M::linear(0.0, 1.0),
        // pre-delay (ms), decay (s), damping, low cut, width, mix.
        (EffectParams::Reverb(_), 0) => M::linear(0.0, 200.0),
        (EffectParams::Reverb(_), 1) => M::log(0.2, 12.0),
        (EffectParams::Reverb(_), 2) => M::linear(0.0, 1.0),
        (EffectParams::Reverb(_), 3) => M::log(20.0, 500.0),
        (EffectParams::Reverb(_), 4) => M::linear(0.0, 1.0),
        (EffectParams::Reverb(_), 5) => M::linear(0.0, 1.0),
        // rate, depth, width, mix.
        (EffectParams::Chorus(_), 0) => M::log(0.05, 8.0),
        (EffectParams::Chorus(_), 1) => M::linear(0.0, 1.0),
        (EffectParams::Chorus(_), 2) => M::linear(0.0, 1.0),
        (EffectParams::Chorus(_), 3) => M::linear(0.0, 1.0),
        // rate, depth, feedback, width, mix.
        (EffectParams::Flanger(_), 0) => M::log(0.02, 5.0),
        (EffectParams::Flanger(_), 1) => M::linear(0.0, 1.0),
        (EffectParams::Flanger(_), 2) => M::linear(-0.95, 0.95),
        (EffectParams::Flanger(_), 3) => M::linear(0.0, 1.0),
        (EffectParams::Flanger(_), 4) => M::linear(0.0, 1.0),
        // drive, bits, downsample, mix.
        (EffectParams::Bitcrusher(_), 0) => M::log(1.0, 16.0),
        (EffectParams::Bitcrusher(_), 1) => M::linear(2.0, 16.0),
        (EffectParams::Bitcrusher(_), 2) => M::linear(1.0, 32.0),
        (EffectParams::Bitcrusher(_), 3) => M::linear(0.0, 1.0),
        // rate, depth, feedback, spread, stages (enum), mix.
        (EffectParams::Phaser(_), 0) => M::log(0.02, 8.0),
        (EffectParams::Phaser(_), 1) => M::linear(0.0, 1.0),
        (EffectParams::Phaser(_), 2) => M::linear(-0.9, 0.9),
        (EffectParams::Phaser(_), 3) => M::log(1.1, 2.6),
        (EffectParams::Phaser(_), 4) => M::linear(0.0, 3.0),
        (EffectParams::Phaser(_), 5) => M::linear(0.0, 1.0),
        // mode (enum), drive, tone, trim, mix.
        (EffectParams::Saturation(_), 0) => M::linear(0.0, 2.0),
        (EffectParams::Saturation(_), 1) => M::log(1.0, 16.0),
        (EffectParams::Saturation(_), 2) => M::log(1_000.0, 20_000.0),
        (EffectParams::Saturation(_), 3) => M::linear(-24.0, 24.0),
        (EffectParams::Saturation(_), 4) => M::linear(0.0, 1.0),
        // threshold (dB), release (ms), stereo link, output (dB).
        (EffectParams::Limiter(_), 0) => M::linear(-24.0, 0.0),
        (EffectParams::Limiter(_), 1) => M::log(5.0, 1_000.0),
        (EffectParams::Limiter(_), 2) => M::linear(0.0, 1.0),
        (EffectParams::Limiter(_), 3) => M::linear(-12.0, 12.0),
        // Z0: dry/wet mix of the two-input ring modulator (0..1).
        (EffectParams::RingMod(_), 0) => M::linear(0.0, 1.0),
        // Z2: attack (ms), release (ms), Q, low band, high band, sibilance,
        // mix, output (dB).
        (EffectParams::Vocoder(_), 0) => M::log(0.5, 100.0),
        (EffectParams::Vocoder(_), 1) => M::log(5.0, 500.0),
        (EffectParams::Vocoder(_), 2) => M::log(0.7, 16.0),
        (EffectParams::Vocoder(_), 3) => M::log(40.0, 1_000.0),
        (EffectParams::Vocoder(_), 4) => M::log(1_000.0, 16_000.0),
        (EffectParams::Vocoder(_), 5) => M::linear(0.0, 1.0),
        (EffectParams::Vocoder(_), 6) => M::linear(0.0, 1.0),
        (EffectParams::Vocoder(_), 7) => M::linear(-24.0, 12.0),
        // Out-of-range slot (a plugin payload, or a document that kept a
        // stale slot after the effect changed kind): normalized pass-through.
        _ => M::linear(0.0, 1.0),
    }
}

/// Mapping mode of a [`MapSpec`] (spec `.agents/spec/curve-map.md` D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MapMode {
    /// `min + t·(max-min)`.
    #[default]
    Linear,
    /// `min·(max/min)^t` — equal ratios take equal axis distance (cutoff,
    /// tempo). Requires `min > 0`.
    Log,
    /// `min + t²·(max-min)` — the top of the range gets the room.
    Exp,
}

impl MapMode {
    /// UI label (English, like every other node caption).
    pub fn label(self) -> &'static str {
        match self {
            MapMode::Linear => "Linear",
            MapMode::Log => "Log",
            MapMode::Exp => "Exp",
        }
    }

    /// Every mode in menu order.
    pub const ALL: [MapMode; 3] = [MapMode::Linear, MapMode::Log, MapMode::Exp];
}

/// Range + mapping of a normalized `[-1, 1]` control signal onto a target's
/// real axis (spec `.agents/spec/curve-map.md` D2).
///
/// Curves and control sources store normalized values, so the mapping lives
/// with the **destination**, not with the data: re-routing a curve to another
/// parameter keeps its shape (D1). `-1` maps to `min`, `0` to the axis centre
/// and `+1` to `max` for every mode.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MapSpec {
    /// Value at `norm = -1`.
    pub min: f32,
    /// Value at `norm = +1`.
    pub max: f32,
    /// Mapping mode.
    #[serde(default)]
    pub mode: MapMode,
}

impl Default for MapSpec {
    /// Identity: the normalized value pass-through (`-1..1` linear).
    fn default() -> Self {
        Self::UNIT
    }
}

impl MapSpec {
    /// Identity map — the normalized value is used as the real value.
    pub const UNIT: Self = Self {
        min: -1.0,
        max: 1.0,
        mode: MapMode::Linear,
    };

    /// Linear map of `min..max`.
    pub fn linear(min: f32, max: f32) -> Self {
        Self {
            min,
            max,
            mode: MapMode::Linear,
        }
    }

    /// Logarithmic map of `min..max` (requires `min > 0`; falls back to linear).
    pub fn log(min: f32, max: f32) -> Self {
        Self {
            min,
            max,
            mode: MapMode::Log,
        }
    }

    /// Repairs a hand-written document: non-finite bounds collapse to the
    /// identity map, `max < min` and a non-positive `Log` range fall back to
    /// `Linear` (D3).
    pub fn sanitized(self) -> Self {
        if !self.min.is_finite() || !self.max.is_finite() || self.max < self.min {
            return Self::UNIT;
        }
        let mode = match self.mode {
            MapMode::Log if self.min <= 0.0 => MapMode::Linear,
            mode => mode,
        };
        Self {
            min: self.min,
            max: self.max,
            mode,
        }
    }

    /// Normalized `[-1, 1]` → real axis value. RT-safe: clamp + arithmetic.
    pub fn map(self, norm: f32) -> f32 {
        if !norm.is_finite() {
            return self.min;
        }
        let spec = self.sanitized();
        let t = (norm.clamp(-1.0, 1.0) + 1.0) * 0.5;
        match spec.mode {
            MapMode::Linear => spec.min + t * (spec.max - spec.min),
            MapMode::Log => spec.min * (spec.max / spec.min).powf(t),
            MapMode::Exp => spec.min + t * t * (spec.max - spec.min),
        }
    }

    /// Real axis value → normalized `[-1, 1]` (inverse of [`Self::map`]).
    ///
    /// Used by the v8 → v9 migration and by the editor when it reads an
    /// externally supplied value; out-of-range input clamps to the ends.
    pub fn unmap(self, value: f32) -> f32 {
        let spec = self.sanitized();
        if !value.is_finite() || spec.max <= spec.min {
            return 0.0;
        }
        let value = value.clamp(spec.min, spec.max);
        let t = match spec.mode {
            MapMode::Linear => (value - spec.min) / (spec.max - spec.min),
            MapMode::Log => (value / spec.min).ln() / (spec.max / spec.min).ln(),
            MapMode::Exp => ((value - spec.min) / (spec.max - spec.min)).max(0.0).sqrt(),
        };
        (t * 2.0 - 1.0).clamp(-1.0, 1.0)
    }
}

/// Lowest song tempo a tempo curve can produce (the transport's own floor,
/// `sequencer::MIN_BPM`). The engine clamps to the same range, so the axis is
/// the range the user can actually hear (spec `curve-map.md` D2).
pub const MIN_TEMPO_BPM: f32 = 20.0;
/// Highest song tempo a tempo curve can produce (`sequencer::MAX_BPM`).
pub const MAX_TEMPO_BPM: f32 = 300.0;

/// Automation anchor: tick position + **normalized** control value.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct AutoPoint {
    /// Position (tick, relative to loop start).
    #[serde(default)]
    pub tick: u32,
    /// Control value in `[-1, 1]` (spec `curve-map.md` D1): the cable's
    /// [`MapSpec`] turns it into the target's real units, so one curve means
    /// the same thing on any destination.
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
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MixerChannel {
    /// Channel name as typed in the console (empty = show the default
    /// "Ch N" / "Ch N · <track>" label). Renaming a channel is presentation
    /// only — it never touches the routing or the fader state.
    #[serde(default)]
    pub name: String,
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
            name: String::new(),
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

/// Arpeggiator step order (spec `midi-nodes.md` §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArpMode {
    /// Ascending.
    #[default]
    Up,
    /// Descending.
    Down,
    /// Up then down, endpoints not repeated.
    UpDown,
    /// Down then up, endpoints not repeated.
    DownUp,
    /// Seeded random walk (reproducible per node).
    Random,
}

impl ArpMode {
    /// Index transported to the engine (`ParamId::ArpMode`).
    pub fn index(self) -> u8 {
        match self {
            ArpMode::Up => 0,
            ArpMode::Down => 1,
            ArpMode::UpDown => 2,
            ArpMode::DownUp => 3,
            ArpMode::Random => 4,
        }
    }

    /// UI label (English, like every other node caption).
    pub fn label(self) -> &'static str {
        match self {
            ArpMode::Up => "Up",
            ArpMode::Down => "Down",
            ArpMode::UpDown => "UpDown",
            ArpMode::DownUp => "DownUp",
            ArpMode::Random => "Random",
        }
    }

    /// Every mode in menu order.
    pub const ALL: [ArpMode; 5] = [
        ArpMode::Up,
        ArpMode::Down,
        ArpMode::UpDown,
        ArpMode::DownUp,
        ArpMode::Random,
    ];
}

/// Arpeggiator harmony rule (spec `midi-nodes.md` §5.1): extra notes stacked
/// on every held key before the octave layers are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArpHarmony {
    /// Play the held keys only.
    #[default]
    Off,
    /// Root + octave.
    Octave,
    /// Root + perfect fifth.
    Fifth,
    /// Root + major third.
    MajorThird,
    /// Root + minor third.
    MinorThird,
    /// Whole-tone ladder up to the octave.
    WholeTone,
    /// Chromatic ladder up to the octave.
    Chromatic,
}

impl ArpHarmony {
    /// Index transported to the engine (`ParamId::ArpHarmony`).
    pub fn index(self) -> u8 {
        match self {
            ArpHarmony::Off => 0,
            ArpHarmony::Octave => 1,
            ArpHarmony::Fifth => 2,
            ArpHarmony::MajorThird => 3,
            ArpHarmony::MinorThird => 4,
            ArpHarmony::WholeTone => 5,
            ArpHarmony::Chromatic => 6,
        }
    }

    /// UI label.
    pub fn label(self) -> &'static str {
        match self {
            ArpHarmony::Off => "Off",
            ArpHarmony::Octave => "Octave",
            ArpHarmony::Fifth => "Fifth",
            ArpHarmony::MajorThird => "Major 3rd",
            ArpHarmony::MinorThird => "Minor 3rd",
            ArpHarmony::WholeTone => "Whole Tone",
            ArpHarmony::Chromatic => "Chromatic",
        }
    }

    /// Every harmony in menu order.
    pub const ALL: [ArpHarmony; 7] = [
        ArpHarmony::Off,
        ArpHarmony::Octave,
        ArpHarmony::Fifth,
        ArpHarmony::MajorThird,
        ArpHarmony::MinorThird,
        ArpHarmony::WholeTone,
        ArpHarmony::Chromatic,
    ];
}

/// Step division choices, in the order the engine's `ARP_DIVISION_TICKS` table
/// uses (spec `midi-nodes.md` §5.1: 1/2, 1/4, 1/8, 1/8T, 1/16, 1/16T,
/// 1/32, 1/64).
pub const ARP_DIVISION_COUNT: usize = 8;
/// Default division index: 1/16.
pub const ARP_DEFAULT_DIVISION: u8 = 4;

/// Arpeggiator parameters (spec `midi-nodes.md` §5.1).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ArpeggiatorParams {
    /// Division index into [`ARP_DIVISION_COUNT`].
    #[serde(default = "default_arp_division")]
    pub division: u8,
    /// Note-on length as a fraction of one step (0.05..=1).
    #[serde(default = "default_arp_gate")]
    pub gate: f32,
    /// Step order.
    #[serde(default)]
    pub mode: ArpMode,
    /// Octave layers (1..=4).
    #[serde(default = "default_arp_octaves")]
    pub octaves: u8,
    /// Keep running after all keys are released.
    #[serde(default)]
    pub latch: bool,
    /// Harmony rule.
    #[serde(default)]
    pub harmony: ArpHarmony,
    /// Probability (0..=0.9) of skipping a step.
    #[serde(default)]
    pub skip: f32,
}

fn default_arp_division() -> u8 {
    ARP_DEFAULT_DIVISION
}

fn default_arp_gate() -> f32 {
    0.5
}

fn default_arp_octaves() -> u8 {
    1
}

impl Default for ArpeggiatorParams {
    fn default() -> Self {
        Self {
            division: ARP_DEFAULT_DIVISION,
            gate: default_arp_gate(),
            mode: ArpMode::Up,
            octaves: 1,
            latch: false,
            harmony: ArpHarmony::Off,
            skip: 0.0,
        }
    }
}

impl ArpeggiatorParams {
    /// Returns a load-time-safe copy (out-of-range values degrade to defaults
    /// or the nearest bound — spec `midi-nodes.md` §7).
    pub fn sanitized(self) -> Self {
        Self {
            division: if (self.division as usize) < ARP_DIVISION_COUNT {
                self.division
            } else {
                ARP_DEFAULT_DIVISION
            },
            gate: if self.gate.is_finite() {
                self.gate.clamp(0.05, 1.0)
            } else {
                0.5
            },
            mode: self.mode,
            octaves: self.octaves.clamp(1, 4),
            latch: self.latch,
            harmony: self.harmony,
            skip: if self.skip.is_finite() {
                self.skip.clamp(0.0, 0.9)
            } else {
                0.0
            },
        }
    }
}

/// Portamento parameters (spec `midi-nodes.md` §5.2).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PortamentoParams {
    /// Glide time constant in milliseconds (0..=2000).
    #[serde(default = "default_portamento_time")]
    pub time_ms: f32,
    /// Output scale, low end (semitones).
    #[serde(default = "default_portamento_min")]
    pub min: f32,
    /// Output scale, high end (semitones).
    #[serde(default = "default_portamento_max")]
    pub max: f32,
    /// Glide only while the previous note is still held (default off: every
    /// note starts a short slide from the previous pitch, spec §5.2).
    #[serde(default)]
    pub legato_only: bool,
}

fn default_portamento_time() -> f32 {
    120.0
}

fn default_portamento_min() -> f32 {
    -24.0
}

fn default_portamento_max() -> f32 {
    24.0
}

impl Default for PortamentoParams {
    fn default() -> Self {
        Self {
            time_ms: default_portamento_time(),
            min: default_portamento_min(),
            max: default_portamento_max(),
            legato_only: false,
        }
    }
}

impl PortamentoParams {
    /// Load-time clamp (spec `midi-nodes.md` §7).
    pub fn sanitized(self) -> Self {
        let time_ms = if self.time_ms.is_finite() {
            self.time_ms.clamp(0.0, 2000.0)
        } else {
            default_portamento_time()
        };
        let min = if self.min.is_finite() {
            self.min.clamp(-24.0, 24.0)
        } else {
            default_portamento_min()
        };
        let max = if self.max.is_finite() {
            self.max.clamp(-24.0, 24.0)
        } else {
            default_portamento_max()
        };
        if min > max {
            Default::default()
        } else {
            Self {
                time_ms,
                min,
                max,
                legato_only: self.legato_only,
            }
        }
    }
}

/// MIDI-triggered envelope parameters (spec `midi-nodes.md` §5.3).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MidiEnvelopeParams {
    /// Attack time in seconds (0..=5).
    #[serde(default = "default_env_attack")]
    pub attack: f32,
    /// Decay time in seconds (0..=5).
    #[serde(default = "default_env_decay")]
    pub decay: f32,
    /// Sustain level (0..=1).
    #[serde(default = "default_env_sustain")]
    pub sustain: f32,
    /// Release time in seconds (0..=10).
    #[serde(default = "default_env_release")]
    pub release: f32,
    /// Output scale, low end.
    #[serde(default)]
    pub min: f32,
    /// Output scale, high end (driving a CC uses 0..=127).
    #[serde(default = "default_env_max")]
    pub max: f32,
}

fn default_env_attack() -> f32 {
    0.01
}

fn default_env_decay() -> f32 {
    0.2
}

fn default_env_sustain() -> f32 {
    0.7
}

fn default_env_release() -> f32 {
    0.4
}

fn default_env_max() -> f32 {
    1.0
}

impl Default for MidiEnvelopeParams {
    fn default() -> Self {
        Self {
            attack: default_env_attack(),
            decay: default_env_decay(),
            sustain: default_env_sustain(),
            release: default_env_release(),
            min: 0.0,
            max: default_env_max(),
        }
    }
}

impl MidiEnvelopeParams {
    /// Load-time clamp (spec `midi-nodes.md` §7).
    pub fn sanitized(self) -> Self {
        let segment = |value: f32, max: f32, fallback: f32| {
            if value.is_finite() {
                value.clamp(0.0, max)
            } else {
                fallback
            }
        };
        let min = if self.min.is_finite() {
            self.min.clamp(-127.0, 127.0)
        } else {
            0.0
        };
        let max = if self.max.is_finite() {
            self.max.clamp(-127.0, 127.0)
        } else {
            default_env_max()
        };
        Self {
            attack: segment(self.attack, 5.0, default_env_attack()),
            decay: segment(self.decay, 5.0, default_env_decay()),
            sustain: segment(self.sustain, 1.0, default_env_sustain()),
            release: segment(self.release, 10.0, default_env_release()),
            min,
            max: if min > max { min } else { max },
        }
    }
}

/// LFO waveform (spec `.agents/spec/lfo.md` §4).
///
/// The engine keeps its own index form (`ParamId::LfoWaveform` takes a
/// rounded `0..4`), exactly like the effects' `Unit::Enum` parameters; this
/// enum is the document-side vocabulary and the UI's label source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum LfoWaveform {
    /// `sin(2πφ)`.
    #[default]
    Sine,
    /// Piecewise triangle, starts at `-1`.
    Triangle,
    /// Rising ramp, `2φ - 1`.
    Saw,
    /// Half-period `+1` / `-1`.
    Square,
    /// One random value per cycle, held until the next cycle boundary.
    SampleHold,
}

impl LfoWaveform {
    /// All waveforms, in engine index order (`ParamId::LfoWaveform`).
    pub const ALL: [Self; 5] = [
        Self::Sine,
        Self::Triangle,
        Self::Saw,
        Self::Square,
        Self::SampleHold,
    ];

    /// Short English label (UI combo + patch node caption).
    pub fn label(self) -> &'static str {
        match self {
            Self::Sine => "Sine",
            Self::Triangle => "Triangle",
            Self::Saw => "Saw",
            Self::Square => "Square",
            Self::SampleHold => "S&H",
        }
    }

    /// Engine parameter index (0..=4).
    pub fn index(self) -> u8 {
        match self {
            Self::Sine => 0,
            Self::Triangle => 1,
            Self::Saw => 2,
            Self::Square => 3,
            Self::SampleHold => 4,
        }
    }
}

/// LFO sync division choices, in the engine's `LFO_DIVISION_TICKS` table
/// order (spec `lfo.md` §3).
pub const LFO_DIVISION_COUNT: usize = 6;
/// Default sync division index: 1/4 note.
pub const LFO_DEFAULT_DIVISION: u8 = 2;
/// Division labels, parallel to the engine's tick table.
pub const LFO_DIVISION_LABELS: [&str; LFO_DIVISION_COUNT] =
    ["1/1", "1/2", "1/4", "1/8", "1/16", "1/32"];

/// LFO parameters (spec `.agents/spec/lfo.md` §3).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LfoParams {
    /// Waveform shape.
    #[serde(default)]
    pub waveform: LfoWaveform,
    /// Free-running rate in Hz when `sync` is off (0.01..=40).
    #[serde(default = "default_lfo_rate")]
    pub rate_hz: f32,
    /// Use the musical division instead of Hz.
    #[serde(default)]
    pub sync: bool,
    /// Division index into [`LFO_DIVISION_COUNT`] when `sync` is on.
    #[serde(default = "default_lfo_division")]
    pub division: u8,
    /// Output depth (0..=1): the waveform is scaled into `[-1, 1] × depth`.
    #[serde(default = "default_lfo_depth")]
    pub depth: f32,
}

fn default_lfo_rate() -> f32 {
    1.0
}

fn default_lfo_division() -> u8 {
    LFO_DEFAULT_DIVISION
}

fn default_lfo_depth() -> f32 {
    0.5
}

impl Default for LfoParams {
    fn default() -> Self {
        Self {
            waveform: LfoWaveform::Sine,
            rate_hz: default_lfo_rate(),
            sync: false,
            division: default_lfo_division(),
            depth: default_lfo_depth(),
        }
    }
}

impl LfoParams {
    /// Load-time clamp (same rule as the other control sources): a
    /// hand-written file must never reach the audio thread with a non-finite
    /// rate, a negative depth or a division index outside the table.
    pub fn sanitized(self) -> Self {
        Self {
            waveform: self.waveform,
            rate_hz: if self.rate_hz.is_finite() {
                self.rate_hz.clamp(0.01, 40.0)
            } else {
                default_lfo_rate()
            },
            sync: self.sync,
            division: if (self.division as usize) < LFO_DIVISION_COUNT {
                self.division
            } else {
                LFO_DEFAULT_DIVISION
            },
            depth: if self.depth.is_finite() {
                self.depth.clamp(0.0, 1.0)
            } else {
                default_lfo_depth()
            },
        }
    }

    /// One-cycle preview value in `[-1, 1]` for the Inspector's waveform
    /// drawing (`hold` is the current S&H value; ignored by the others).
    ///
    /// Kept next to the parameters so the UI preview cannot drift from the
    /// engine's formula (spec `lfo.md` §4).
    pub fn preview_value(self, phase: f32, hold: f32) -> f32 {
        let p = phase.clamp(0.0, 1.0);
        match self.waveform {
            LfoWaveform::Sine => (std::f32::consts::TAU * p).sin(),
            LfoWaveform::Triangle => {
                if p < 0.5 {
                    4.0 * p - 1.0
                } else {
                    3.0 - 4.0 * p
                }
            }
            LfoWaveform::Saw => 2.0 * p - 1.0,
            LfoWaveform::Square => {
                if p < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            LfoWaveform::SampleHold => hold,
        }
    }
}

/// Curve Map node parameters (spec `.agents/spec/curve-map.md` D5).
///
/// The node is a **pure pass-through with an opinion**: it takes the
/// normalized control value it receives and re-emits it unchanged, while
/// declaring the axis ([`MapSpec`]) the *next* hop should deliver through
/// (D6). So `min`/`max`/`mode` describe the destination, not the value —
/// which is why the defaults (`-1..1` linear) change nothing until the user
/// edits them.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CurveMapParams {
    /// Value the next hop sees at `norm = -1`.
    #[serde(default = "default_curve_map_min")]
    pub min: f32,
    /// Value the next hop sees at `norm = +1`.
    #[serde(default = "default_curve_map_max")]
    pub max: f32,
    /// Mapping mode of the declared axis.
    #[serde(default)]
    pub mode: MapMode,
}

fn default_curve_map_min() -> f32 {
    -1.0
}

fn default_curve_map_max() -> f32 {
    1.0
}

impl Default for CurveMapParams {
    fn default() -> Self {
        Self {
            min: default_curve_map_min(),
            max: default_curve_map_max(),
            mode: MapMode::Linear,
        }
    }
}

impl CurveMapParams {
    /// The axis this node declares for its output.
    pub fn map(self) -> MapSpec {
        MapSpec {
            min: self.min,
            max: self.max,
            mode: self.mode,
        }
    }

    /// Load-time repair (same rule as [`MapSpec::sanitized`]): a hand-written
    /// file cannot leave the node with a backwards or non-finite axis.
    pub fn sanitized(self) -> Self {
        let map = self.map().sanitized();
        Self {
            min: map.min,
            max: map.max,
            mode: map.mode,
        }
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
        /// Channel (input port) count, 1..=32 (D21: add/remove buttons).
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
        /// Semitone offset applied to this node's outgoing notes (I1).
        /// Range `-24..=24`; this is the musical transposition that survives
        /// a reload.
        #[serde(default)]
        transpose: i8,
        /// I1: the register this keyboard node plays in.
        ///
        /// The 25-key strip brackets `G(octave)..G(octave+2)` and the computer
        /// keyboard starts on that window's C, so the strip on the box and the
        /// keys under the user's fingers are the same instrument. Range
        /// `0..=8`; the default ([`DEFAULT_KEYBOARD_OCTAVE`]) is the
        /// `G3..G5` window around middle C.
        #[serde(default = "default_keyboard_octave")]
        octave: i8,
    },
    /// MIDI effect: arpeggiator (1 MIDI in, 1 MIDI out, no audio ports;
    /// spec `.agents/spec/midi-nodes.md` §5.1).
    Arpeggiator {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track (grouping only).
        #[serde(default)]
        track: usize,
        /// Step / order parameters.
        #[serde(default)]
        params: ArpeggiatorParams,
    },
    /// MIDI effect + control source: portamento (1 MIDI in, 1 MIDI out,
    /// 1 control out, no audio ports; spec `midi-nodes.md` §5.2).
    Portamento {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track (grouping only).
        #[serde(default)]
        track: usize,
        /// Glide parameters.
        #[serde(default)]
        params: PortamentoParams,
    },
    /// Control source: MIDI-triggered envelope (1 MIDI in, 1 control out, no
    /// audio ports; spec `midi-nodes.md` §5.3).
    MidiEnvelope {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track (grouping only).
        #[serde(default)]
        track: usize,
        /// ADSR + output scale.
        #[serde(default)]
        params: MidiEnvelopeParams,
    },
    /// Control source: free-running LFO (1 control out, no audio / MIDI
    /// ports; spec `lfo.md`).
    ///
    /// Runs off the transport clock: Hz mode accumulates playing seconds,
    /// sync mode derives its phase from `song_ticks`. Output is normalized
    /// `[-1, 1]`, so it drives a parameter, a CC or a bend through the same
    /// target-derived axis as a curve (D5 of `curve-map.md`).
    Lfo {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track (grouping only).
        #[serde(default)]
        track: usize,
        /// Waveform / rate / depth.
        #[serde(default)]
        params: LfoParams,
    },
    /// Control map: 1 control in, 1 control out, no audio / MIDI ports
    /// (spec `curve-map.md` D5).
    ///
    /// Sits between a curve / control source and its destination and declares
    /// the axis the destination is driven through (D6), which is how a curve
    /// can be reshaped (or moved to a differently-scaled target) without
    /// editing a single anchor. The value itself is untouched — mapping stays
    /// one arithmetic step on the receiving side (D4).
    CurveMap {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Owning track (grouping only).
        #[serde(default)]
        track: usize,
        /// Declared axis of the output.
        #[serde(default)]
        params: CurveMapParams,
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
    /// Live audio input node (R4, spec `recording.md` D9): 1 audio out, no
    /// input, no engine-side MIDI. It is the graph's door for a microphone /
    /// instrument, and the source that arms an audio track's recording input.
    ///
    /// Deliberately has **no `track` field**: recording ownership is the
    /// `Timeline In` wiring (D22), so one input can feed several audio tracks.
    Input {
        /// Persisted node ID.
        #[serde(default)]
        node_id: u64,
        /// Which interleaved channel of the device the node exposes (0-based;
        /// v1 supports the first two channels).
        #[serde(default)]
        channel: u8,
        /// Output gain (linear, 0..2; 1.0 = unity).
        #[serde(default = "default_input_gain")]
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

/// Unity gain for a live input node (D10: born silent downstream because it is
/// unconnected, not because its gain is turned down).
fn default_input_gain() -> f32 {
    1.0
}

impl VoiceSpec {
    /// Heap bytes held by the voice payload (paths, plugin state, drum slots).
    /// Part of [`Project::approx_bytes`].
    fn payload_bytes(&self) -> usize {
        match self {
            VoiceSpec::Builtin => 0,
            VoiceSpec::SoundFont { path, .. } => path.len(),
            VoiceSpec::Sample { path, .. } => path.len(),
            VoiceSpec::DrumKit { slots, .. } => {
                slots.len() * std::mem::size_of::<DrumSlot>()
                    + slots.iter().map(|slot| slot.path.len()).sum::<usize>()
            }
            VoiceSpec::Modal { .. } => std::mem::size_of::<ModalParams>(),
            VoiceSpec::Pluck { .. } => std::mem::size_of::<PluckParams>(),
            VoiceSpec::Organ { .. } => std::mem::size_of::<OrganParams>(),
            VoiceSpec::Wavetable { .. } => std::mem::size_of::<WavetableParams>(),
            VoiceSpec::Clap {
                path,
                plugin_id,
                state,
            } => path.len() + plugin_id.len() + state.len(),
            VoiceSpec::Vst3 {
                path,
                class_id,
                state,
            } => path.len() + class_id.len() + state.len(),
        }
    }
}

impl NodeDoc {
    /// Bytes the node owns *behind* the enum slot: voice paths, plugin state
    /// blobs, drum slot tables. Part of [`Project::approx_bytes`]; nodes that
    /// only carry scalars report 0.
    fn payload_bytes(&self) -> usize {
        match self {
            NodeDoc::Instrument { voice, .. } | NodeDoc::Effect { voice, .. } => {
                voice.payload_bytes()
            }
            _ => 0,
        }
    }

    /// Stable node-kind label for CLI / MCP introspection (F-039).
    pub fn kind_key(&self) -> &'static str {
        match self {
            NodeDoc::Instrument { .. } => "instrument",
            NodeDoc::Effect { .. } => "effect",
            NodeDoc::Mixer { .. } => "mixer",
            NodeDoc::Keyboard { .. } => "keyboard",
            NodeDoc::Arpeggiator { .. } => "arpeggiator",
            NodeDoc::Portamento { .. } => "portamento",
            NodeDoc::MidiEnvelope { .. } => "midi_envelope",
            NodeDoc::Lfo { .. } => "lfo",
            NodeDoc::CurveMap { .. } => "curve_map",
            NodeDoc::Feedback { .. } => "feedback",
            NodeDoc::Input { .. } => "input",
            NodeDoc::AudioTrack { .. } => "audio_track",
            NodeDoc::ChannelStrip { .. } => "channel_strip",
        }
    }

    /// Node ID.
    pub fn node_id(&self) -> u64 {
        match self {
            NodeDoc::Instrument { node_id, .. }
            | NodeDoc::Effect { node_id, .. }
            | NodeDoc::Mixer { node_id, .. }
            | NodeDoc::Feedback { node_id, .. }
            | NodeDoc::Keyboard { node_id, .. }
            | NodeDoc::AudioTrack { node_id, .. }
            | NodeDoc::Arpeggiator { node_id, .. }
            | NodeDoc::Portamento { node_id, .. }
            | NodeDoc::MidiEnvelope { node_id, .. }
            | NodeDoc::Lfo { node_id, .. }
            | NodeDoc::CurveMap { node_id, .. }
            | NodeDoc::ChannelStrip { node_id, .. }
            | NodeDoc::Input { node_id, .. } => *node_id,
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
            | NodeDoc::Arpeggiator { track, .. }
            | NodeDoc::Portamento { track, .. }
            | NodeDoc::MidiEnvelope { track, .. }
            | NodeDoc::Lfo { track, .. }
            | NodeDoc::CurveMap { track, .. }
            | NodeDoc::ChannelStrip { track, .. } => *track,
            // R4: the live input node has no owning track (recording ownership
            // is the `Timeline In` wiring, D22).
            NodeDoc::Input { .. } => 0,
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

    /// Whether this node has a MIDI output port (virtual keyboard or a MIDI
    /// effect — spec `midi-nodes.md` §3).
    pub fn has_midi_out(&self) -> bool {
        matches!(
            self,
            NodeDoc::Keyboard { .. } | NodeDoc::Arpeggiator { .. } | NodeDoc::Portamento { .. }
        )
    }

    /// Whether this node has a MIDI input port (instrument or MIDI effect).
    pub fn has_midi_in(&self) -> bool {
        matches!(
            self,
            NodeDoc::Instrument { .. }
                | NodeDoc::Arpeggiator { .. }
                | NodeDoc::Portamento { .. }
                | NodeDoc::MidiEnvelope { .. }
        )
    }

    /// Whether this node has a control output port (spec §3).
    pub fn has_control_out(&self) -> bool {
        matches!(
            self,
            NodeDoc::Portamento { .. }
                | NodeDoc::MidiEnvelope { .. }
                | NodeDoc::Lfo { .. }
                | NodeDoc::CurveMap { .. }
        )
    }

    /// Whether this node accepts control cables as a destination (spec §7).
    pub fn accepts_control(&self) -> bool {
        matches!(
            self,
            NodeDoc::Instrument { .. }
                | NodeDoc::Effect { .. }
                | NodeDoc::Mixer { .. }
                | NodeDoc::Feedback { .. }
                | NodeDoc::CurveMap { .. }
        )
    }

    /// Whether this node has audio ports at all (keyboard, MIDI effects,
    /// control sources and the Curve Map have none — spec §3).
    pub fn has_audio_ports(&self) -> bool {
        !matches!(
            self,
            NodeDoc::Keyboard { .. }
                | NodeDoc::Arpeggiator { .. }
                | NodeDoc::Portamento { .. }
                | NodeDoc::MidiEnvelope { .. }
                | NodeDoc::Lfo { .. }
                | NodeDoc::CurveMap { .. }
        )
    }

    /// Number of audio input ports (Z0). A mixer exposes one per channel,
    /// dual-input built-in effects expose Main (0) and Mod (1), everything
    /// else the single port 0. MIDI/control-only nodes answer 0.
    pub fn audio_inputs(&self) -> u8 {
        match self {
            NodeDoc::Mixer { inputs, .. } => *inputs,
            NodeDoc::Effect { voice, params, .. }
                if matches!(voice, VoiceSpec::Builtin) && params.kind().audio_inputs() == 2 =>
            {
                2
            }
            _ => u8::from(self.has_audio_ports()),
        }
    }
}

/// Cable port reference (spec §7 `PortRef`).
///
/// `Master` / `TimelineIn` / `MidiTrack` / `AutoTrack` / `TrackIn` are reserved
/// identities of fixed entities (not listed in the node table; the loader /
/// engine fills them in implicitly); `Node` references a document node.
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
    /// entity. Port 0 is the tempo port ([`AutoParam::Tempo`]); R6 adds one
    /// recording input per track (see [`PortRef::TrackIn`]).
    TimelineIn,
    /// One timeline track's **recording input** (R6, spec `recording.md` §5):
    /// a port on the fixed `Timeline In` terminal. A cable landing here says
    /// "this source records into that track"; the destination is a document
    /// fact, not UI state (D22). Type-matched only (D24), at most one source
    /// per port (D23).
    TrackIn {
        /// Which track family the input belongs to.
        kind: TrackKind,
        /// Track index inside that family.
        track: usize,
    },
}

/// Timeline track family addressed by a [`PortRef::TrackIn`] port (R6,
/// spec `recording.md` §5 D21).
///
/// The order is the one the `Timeline In` port table uses (MIDI → curve →
/// audio), matching the `Timeline` outlet order.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    /// MIDI track (note plane).
    #[default]
    Midi,
    /// Control (curve) track.
    Curve,
    /// Audio track.
    Audio,
}

/// The `Timeline In` port index of `kind`/`track` (R6 D21), or `None` when the
/// track is out of range.
///
/// Port 0 is the pre-existing tempo port; the recording inputs follow in
/// **MIDI → curve → audio** order, one per track, so the terminal and the
/// `Timeline` outlet expose the same sequence.
pub fn timeline_in_port(
    kind: TrackKind,
    track: usize,
    midi_count: usize,
    auto_count: usize,
    audio_count: usize,
) -> Option<u8> {
    let offset = match kind {
        TrackKind::Midi if track < midi_count => track,
        TrackKind::Curve if track < auto_count => midi_count + track,
        TrackKind::Audio if track < audio_count => midi_count + auto_count + track,
        _ => return None,
    };
    u8::try_from(offset + 1).ok()
}

/// Inverse of [`timeline_in_port`]: a `Timeline In` port index → its track
/// family and index. Port 0 (tempo) returns `None`.
pub fn timeline_in_port_kind(
    port: u8,
    midi_count: usize,
    auto_count: usize,
    audio_count: usize,
) -> Option<(TrackKind, usize)> {
    let index = usize::from(port).checked_sub(1)?;
    if index < midi_count {
        Some((TrackKind::Midi, index))
    } else if index < midi_count + auto_count {
        Some((TrackKind::Curve, index - midi_count))
    } else if index < midi_count + auto_count + audio_count {
        Some((TrackKind::Audio, index - midi_count - auto_count))
    } else {
        None
    }
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
    /// Mapping of the normalized curve onto the target's real axis (spec
    /// `curve-map.md` D2/D4). Written whenever the UI (re)routes an automation
    /// cable; `None` means "derive from the target" ([`AutoParam::default_map`])
    /// and only survives in documents that never went through the editor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map: Option<MapSpec>,
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
            map: None,
        }
    }

    /// Audio cable landing on a specific mixer channel (P5f D17).
    pub fn audio_to_channel(from: impl Into<PortRef>, to: impl Into<PortRef>, in_port: u8) -> Self {
        Self {
            in_port,
            ..CableDoc::audio(from, to)
        }
    }

    /// Audio cable landing on a specific input port (Z0: Main = 0, Mod = 1).
    pub fn audio_to_port(from: impl Into<PortRef>, to: impl Into<PortRef>, in_port: u8) -> Self {
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
            map: None,
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
            map: None,
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
            map: None,
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
            map: None,
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

    /// The cable feeding one timeline track's recording input (R6 D22), if the
    /// track is armed by wiring.
    pub fn track_input_source(&self, kind: TrackKind, track: usize) -> Option<&CableDoc> {
        self.cables.iter().find(|cable| {
            matches!(
                cable.to,
                PortRef::TrackIn { kind: k, track: t } if k == kind && t == track
            )
        })
    }

    /// Every track of `kind` whose recording input has a cable (the armed set,
    /// R6 D22), in track order.
    pub fn track_input_tracks(&self, kind: TrackKind) -> Vec<usize> {
        let mut tracks: Vec<usize> = self
            .cables
            .iter()
            .filter_map(|cable| match cable.to {
                PortRef::TrackIn { kind: k, track } if k == kind => Some(track),
                _ => None,
            })
            .collect();
        tracks.sort_unstable();
        tracks.dedup();
        tracks
    }

    /// Every timeline recording input fed by one source node (R6 D23/D33):
    /// the track family and index pairs, in document cable order.
    ///
    /// A node with at least one entry here is **taken over**: its direct MIDI
    /// cables are dormant and its events are dispatched through these tracks
    /// instead (D33 — pure derivation, no stored mute flag).
    pub fn node_track_inputs(&self, node_id: u64) -> Vec<(TrackKind, usize)> {
        self.cables
            .iter()
            .filter_map(|cable| match (&cable.from, &cable.to) {
                (PortRef::Node(id), PortRef::TrackIn { kind, track }) if *id == node_id => {
                    Some((*kind, *track))
                }
                _ => None,
            })
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

    /// First audio input port with no cable (Z0). Unlike
    /// [`GraphDoc::first_free_channel`] this covers any node; `None` means
    /// every port is occupied.
    pub fn first_free_port(&self, node_id: u64) -> Option<u8> {
        let ports = self.node_by_id(node_id)?.audio_inputs();
        (0..ports).find(|port| {
            !self.cables.iter().any(|c| {
                c.kind == CableKind::Audio && c.to == PortRef::Node(node_id) && c.in_port == *port
            })
        })
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
        // Live input node range (R4): v1 is mono/stereo, unity gain 0..2.
        for node in &self.nodes {
            if let NodeDoc::Input {
                node_id,
                channel,
                gain,
            } = node
            {
                if *channel > 1 {
                    return Err(format!(
                        "input node {node_id} selects channel {channel} (v1 supports 0..1)"
                    ));
                }
                if !gain.is_finite() || !(0.0..=2.0).contains(gain) {
                    return Err(format!(
                        "input node {node_id} has an out-of-range gain ({gain})"
                    ));
                }
            }
        }
        // Drum kit integrity (W1, spec `drum-kit.md` K3): bounded slots and
        // unique trigger keys — a duplicate key would make "which sample
        // plays" depend on table order.
        for node in &self.nodes {
            if let NodeDoc::Instrument { node_id, voice, .. } = node
                && let VoiceSpec::DrumKit { slots, .. } = voice
            {
                if slots.len() > MAX_DRUM_SLOTS {
                    return Err(format!(
                        "drum kit node {node_id} has {} slots (limit {MAX_DRUM_SLOTS})",
                        slots.len()
                    ));
                }
                let mut keys = std::collections::HashSet::new();
                for slot in slots {
                    if !keys.insert(slot.note) {
                        return Err(format!(
                            "drum kit node {node_id} maps key {} twice",
                            slot.note
                        ));
                    }
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
        // Input discipline (P5f D14/D16, Z0 Z-D7): audio cables into one
        // destination — mixers accept up to 32 (unique in_port), dual-input
        // effects accept 2 (Main/Mod), everything else ≤ 1.
        let mut audio_in_count: std::collections::BTreeMap<u64, usize> = Default::default();
        let mut mixer_ports: std::collections::BTreeMap<u64, Vec<u8>> = Default::default();
        // MIDI event-cable bookkeeping (spec `midi-nodes.md` §7): node event
        // in-edges are at most one per destination, and they cannot coexist
        // with a timeline / keyboard feed (two independent event sources into
        // one node have no merge semantics). Timeline + keyboard together stay
        // legal (the default document wires both to the same instrument).
        let mut midi_event_in: std::collections::BTreeMap<u64, usize> = Default::default();
        let mut midi_external_in: std::collections::BTreeMap<u64, usize> = Default::default();
        // Node-to-node control in-edges are at most one per destination; the
        // timeline automation tables keep their existing (looser) rules.
        let mut control_event_in: std::collections::BTreeMap<u64, usize> = Default::default();
        // Curve Map inputs are single-source for **every** source kind (spec
        // `curve-map.md` D5), so they get their own counter.
        let mut curve_map_in: std::collections::BTreeMap<u64, usize> = Default::default();
        // R6 D23: one recording source per timeline track input, whatever the
        // cable kind (a second cable is rejected, not silently replaced).
        let mut track_in_count: std::collections::BTreeMap<(TrackKind, usize), usize> =
            Default::default();
        for cable in &self.cables {
            self.check_port(&cable.from, &node_ids, midi_count, auto_count)?;
            self.check_port(&cable.to, &node_ids, midi_count, auto_count)?;
            if let PortRef::TrackIn { kind, track } = &cable.to {
                let expected = match kind {
                    TrackKind::Midi => CableKind::Midi,
                    TrackKind::Curve => CableKind::Automation,
                    TrackKind::Audio => CableKind::Audio,
                };
                if cable.kind != expected {
                    return Err(format!(
                        "cable kind {:?} does not match the {:?} recording input (track {track}, D24)",
                        cable.kind, kind
                    ));
                }
                let count = track_in_count.entry((*kind, *track)).or_insert(0);
                *count += 1;
                if *count > 1 {
                    return Err(format!(
                        "the {:?} recording input of track {track} receives multiple cables (one source per input, D23)",
                        kind
                    ));
                }
                // D24: the source has to be the kind of data the input records.
                // Timeline track outlets are sources too, but a timeline outlet
                // feeding its own input family would re-record the arrangement
                // and is not part of the R6 matrix (D26).
                match (kind, &cable.from) {
                    (TrackKind::Midi, PortRef::Node(id)) => {
                        let source = self.node_by_id(*id).ok_or_else(|| {
                            format!("MIDI recording input source node {id} does not exist")
                        })?;
                        if !source.has_midi_out() {
                            return Err(format!(
                                "MIDI recording input source node {id} has no MIDI output (keyboard / MIDI-effect nodes only)"
                            ));
                        }
                    }
                    (TrackKind::Audio, PortRef::Node(id)) => {
                        let source = self.node_by_id(*id).ok_or_else(|| {
                            format!("audio recording input source node {id} does not exist")
                        })?;
                        if matches!(source, NodeDoc::AudioTrack { .. }) || !source.has_audio_ports()
                        {
                            return Err(format!(
                                "audio recording input source node {id} has no audio output (instrument / mixer / input nodes only)"
                            ));
                        }
                    }
                    (TrackKind::Curve, PortRef::AutoTrack(_)) => {}
                    (TrackKind::Curve, PortRef::Node(id)) => {
                        let source = self.node_by_id(*id).ok_or_else(|| {
                            format!("control recording input source node {id} does not exist")
                        })?;
                        if !source.has_control_out() {
                            return Err(format!(
                                "control recording input source node {id} has no control output (Portamento / MIDI Envelope / LFO / Curve Map only)"
                            ));
                        }
                    }
                    _ => {
                        return Err(format!(
                            "the {:?} recording input of track {track} does not accept this source port (D24)",
                            kind
                        ));
                    }
                }
                // The input port itself is a sink: nothing else to check.
                continue;
            }
            match cable.kind {
                CableKind::Audio => {
                    if !matches!(cable.from, PortRef::Node(_)) {
                        return Err("audio cable source port must be a node".to_string());
                    }
                    // G6 + spec `midi-nodes.md` §3: keyboards, MIDI effects and control
                    // sources have no audio ports at all.
                    if let PortRef::Node(id) = cable.from
                        && self
                            .node_by_id(id)
                            .is_some_and(|node| !node.has_audio_ports())
                    {
                        return Err(format!("node {id} has no audio output (MIDI/control only)"));
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
                    if self
                        .node_by_id(dest_key)
                        .is_some_and(|node| !node.has_audio_ports())
                    {
                        return Err(format!(
                            "node {dest_key} has no audio input (MIDI/control only)"
                        ));
                    }
                    let dest_node = self.node_by_id(dest_key);
                    let port_limit = dest_node.map_or(1, |node| usize::from(node.audio_inputs()));
                    if dest_key == MASTER_NODE_ID {
                        // Master (D16): single input; in_port must be 0.
                        if cable.in_port != 0 {
                            return Err(
                                "master input has no channel ports (in_port must be 0)".to_string()
                            );
                        }
                    } else if let Some(NodeDoc::Mixer { inputs, .. }) = dest_node {
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
                        // Z0: a dual-input effect accepts ports 0 (Main) and
                        // 1 (Mod), each at most one cable; every other node
                        // keeps port 0 only (pre-Z0 behaviour unchanged).
                        if usize::from(cable.in_port) >= port_limit {
                            return Err(format!(
                                "node {dest_key} has no input port {} (has {port_limit})",
                                cable.in_port
                            ));
                        }
                        let ports = mixer_ports.entry(dest_key).or_default();
                        if ports.contains(&cable.in_port) {
                            return Err(format!(
                                "node {dest_key} input port {} receives multiple cables",
                                cable.in_port
                            ));
                        }
                        ports.push(cable.in_port);
                    }
                    let count = audio_in_count.entry(dest_key).or_insert(0);
                    *count += 1;
                    let limit = if dest_key == MASTER_NODE_ID {
                        1
                    } else if matches!(dest_node, Some(NodeDoc::Mixer { .. })) {
                        MAX_MIXER_CHANNELS
                    } else {
                        port_limit
                    };
                    if *count > limit {
                        return Err(format!(
                            "node {dest_key} receives {count} audio cables (limit {limit}, D14/D16/Z0)"
                        ));
                    }
                }
                CableKind::Midi => {
                    // D3 + G6 + spec `midi-nodes.md` §7: MIDI sources are the
                    // timeline track outlets, virtual keyboards and MIDI
                    // effects (Arpeggiator / Portamento); destinations are
                    // instruments and MIDI effects.
                    match cable.from {
                        PortRef::MidiTrack(_) => {}
                        PortRef::Node(id) => {
                            let source = self.node_by_id(id).ok_or_else(|| {
                                format!("MIDI cable source node {id} does not exist")
                            })?;
                            if !source.has_midi_out() {
                                return Err(format!(
                                    "MIDI cable source node {id} has no MIDI output (keyboard / MIDI-effect nodes only)"
                                ));
                            }
                        }
                        _ => {
                            return Err(
                                "MIDI cable source port must be a timeline MIDI track, a virtual keyboard or a MIDI-effect node"
                                    .to_string(),
                            );
                        }
                    }
                    let PortRef::Node(target_id) = cable.to else {
                        return Err(
                            "MIDI cable target port must be an instrument or MIDI-effect node"
                                .to_string(),
                        );
                    };
                    let target = self.node_by_id(target_id).ok_or_else(|| {
                        format!("MIDI cable target node {target_id} does not exist")
                    })?;
                    if !target.has_midi_in() {
                        return Err(format!(
                            "MIDI cable target node {target_id} has no MIDI input (instrument / MIDI-effect nodes only)"
                        ));
                    }
                    match cable.from {
                        PortRef::MidiTrack(_) => {
                            *midi_external_in.entry(target_id).or_insert(0) += 1;
                        }
                        PortRef::Node(source_id) => {
                            if self.node_by_id(source_id).is_some_and(NodeDoc::is_keyboard) {
                                *midi_external_in.entry(target_id).or_insert(0) += 1;
                            } else {
                                let count = midi_event_in.entry(target_id).or_insert(0);
                                *count += 1;
                                if *count > 1 {
                                    return Err(format!(
                                        "node {target_id} receives multiple MIDI-effect cables (one event source per node)"
                                    ));
                                }
                            }
                        }
                        _ => {}
                    }
                    if midi_event_in.get(&target_id).copied().unwrap_or(0) > 0
                        && midi_external_in.get(&target_id).copied().unwrap_or(0) > 0
                    {
                        return Err(format!(
                            "node {target_id} receives both a timeline/keyboard feed and a MIDI-effect cable (one event source per node)"
                        ));
                    }
                }
                CableKind::Automation => {
                    // spec `midi-nodes.md` §7: automation sources are timeline
                    // automation tracks or control-output nodes
                    // (Portamento / MidiEnvelope).
                    let control_source = match cable.from {
                        PortRef::AutoTrack(_) => None,
                        PortRef::Node(id) => {
                            let source = self.node_by_id(id).ok_or_else(|| {
                                format!("automation cable source node {id} does not exist")
                            })?;
                            if !source.has_control_out() {
                                return Err(format!(
                                    "automation cable source node {id} has no control output (Portamento / MIDI Envelope / LFO / Curve Map only)"
                                ));
                            }
                            Some(id)
                        }
                        _ => {
                            return Err(
                                "automation cable source port must be a timeline automation track or a control-output node"
                                    .to_string(),
                            );
                        }
                    };
                    // U3b D1/D7: the only valid automation destination is a
                    // node parameter port, or the fixed Timeline In endpoint
                    // whose sole v1 port is `Tempo`. Tempo is a clock, so it
                    // must not reach a node's `set_param` path, and every
                    // other parameter must not reach the clock.
                    match cable.to {
                        PortRef::Node(id) => {
                            if cable.param.is_timeline_in_only() {
                                return Err(format!(
                                    "{:?} is only valid on the timeline input (it is a clock, not a node parameter)",
                                    cable.param
                                ));
                            }
                            let target = self.node_by_id(id).ok_or_else(|| {
                                format!("automation cable target node {id} does not exist")
                            })?;
                            if !target.accepts_control() {
                                return Err(format!(
                                    "automation cable target node {id} does not accept control input (instrument / effect / mixer / feedback / curve map only)"
                                ));
                            }
                            // Curve Map (spec `curve-map.md` D5/D6): one
                            // control in, whatever the source kind — a
                            // timeline curve and a control node would
                            // otherwise fight over the same pass-through.
                            // Its port has no axis of its own, so the cable
                            // must say so (`AutoParam::CurveMap`).
                            if matches!(target, NodeDoc::CurveMap { .. }) {
                                if cable.param != AutoParam::CurveMap {
                                    return Err(format!(
                                        "curve map node {id} only accepts the curve-map port (a mapped signal, not {:?})",
                                        cable.param
                                    ));
                                }
                                let count = curve_map_in.entry(id).or_insert(0);
                                *count += 1;
                                if *count > 1 {
                                    return Err(format!(
                                        "curve map node {id} receives multiple control cables (one control input)"
                                    ));
                                }
                            } else if cable.param == AutoParam::CurveMap {
                                return Err(format!(
                                    "the curve-map port is only valid on a curve map node ({id} is not one)"
                                ));
                            }
                            if control_source == Some(id) {
                                return Err(format!(
                                    "node {id} cannot receive a control cable from itself"
                                ));
                            }
                            if control_source.is_some() {
                                let count = control_event_in.entry(id).or_insert(0);
                                *count += 1;
                                if *count > 1 {
                                    return Err(format!(
                                        "node {id} receives multiple control cables (one control source per node)"
                                    ));
                                }
                            }
                            // U4c (spec §9 D20): a mixer channel address must
                            // resolve to an existing channel. This is the one
                            // parameter family a document edit can invalidate
                            // (shrink the mixer) and the engine cannot ignore
                            // meaningfully, so it is the one we enforce.
                            if let AutoParam::Node(param) = &cable.param {
                                self.check_node_param(param, id)?;
                            }
                        }
                        PortRef::TimelineIn => {
                            if control_source.is_some() {
                                return Err(
                                    "the timeline input only accepts timeline automation tracks"
                                        .to_string(),
                                );
                            }
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
            PortRef::TrackIn { kind, track } => {
                let count = match kind {
                    TrackKind::Midi => midi_count,
                    TrackKind::Curve => auto_count,
                    // The graph document does not know the project's audio
                    // track count; `Project::validate` re-checks the real one.
                    TrackKind::Audio => MAX_AUDIO_TRACKS,
                };
                if *track >= count {
                    return Err(format!(
                        "{kind:?} recording input {track} out of range (track count = {count})"
                    ));
                }
            }
            PortRef::Master => {}
            // Fixed sink with no identity to resolve (U3b D2).
            PortRef::TimelineIn => {}
        }
        Ok(())
    }

    /// Checks one [`NodeParam`] against the node it lands on (U4c, spec §9
    /// D20).
    ///
    /// Only the **mixer** family is enforced: a channel address is a
    /// positional identity that a document edit (shrinking the mixer) can
    /// invalidate, and the engine cannot do anything sensible with an
    /// out-of-range channel. Voice-dependent parameters (resonance / ADSR /
    /// SoundFont gain / effect slots) are *not* checked against the node
    /// kind: swapping a node's voice would then make an existing project
    /// unsavable, while the engine already ignores a parameter the processor
    /// does not recognize.
    fn check_node_param(&self, param: &NodeParam, node_id: u64) -> Result<(), String> {
        let mixer_inputs = match self.node_by_id(node_id) {
            Some(NodeDoc::Mixer { inputs, .. }) => Some(*inputs),
            _ => None,
        };
        match param {
            NodeParam::MixerChannel { channel, .. } => match mixer_inputs {
                Some(inputs) if *channel < inputs => Ok(()),
                Some(inputs) => Err(format!(
                    "automation targets mixer {node_id} channel {channel} out of range (inputs = {inputs})"
                )),
                None => Err(format!(
                    "automation targets mixer channel {channel}, but node {node_id} is not a mixer"
                )),
            },
            NodeParam::MixerOutPan | NodeParam::MixerOutMute => match mixer_inputs {
                Some(_) => Ok(()),
                None => Err(format!(
                    "automation targets the mixer output stage, but node {node_id} is not a mixer"
                )),
            },
            _ => Ok(()),
        }
    }
}

/// Generates the default node graph document (P5f D18, revised 2026-10-29):
/// one playable MIDI track (built-in instrument → mixer.ch0), one global
/// mixer with a single channel → master output, the track's MIDI
/// self-connection, this track's computer-keyboard input, and one generator
/// node per audio track (left unwired so the user patches it).
///
/// Further MIDI tracks start empty: adding a track does not create an
/// instrument, an effect or a mixer channel. node_ids are allocated
/// consecutively starting from [`MASTER_NODE_ID`] + 1.
pub fn default_graph_doc(midi_track_count: usize, audio_track_count: usize) -> GraphDoc {
    let mut nodes = Vec::new();
    let mut cables = Vec::new();
    let mut next_id = MASTER_NODE_ID + 1;

    // Only the first MIDI track ships an instrument (so a fresh project plays
    // from the computer keyboard); the rest are bare tracks the user builds.
    let first_voice = if midi_track_count > 0 {
        let voice_id = next_id;
        next_id += 1;
        nodes.push(NodeDoc::Instrument {
            node_id: voice_id,
            track: 0,
            voice: VoiceSpec::Builtin,
            synth: SynthParams::default(),
        });
        cables.push(CableDoc::midi(0, voice_id));
        Some(voice_id)
    } else {
        None
    };

    // One global mixer with the minimum channel count (1) → master. The user
    // grows it when a second source needs a home.
    let mixer_id = next_id;
    next_id += 1;
    let inputs = MIN_MIXER_CHANNELS;
    nodes.push(NodeDoc::Mixer {
        node_id: mixer_id,
        track: 0,
        inputs: inputs as u8,
        channels: vec![MixerChannel::default(); inputs],
        out_pan: 0.0,
        out_muted: false,
    });
    cables.push(CableDoc::audio(mixer_id, PortRef::Master));
    if let Some(voice_id) = first_voice {
        cables.push(CableDoc::audio_to_channel(voice_id, mixer_id, 0));
    }

    // One computer-keyboard input node, cabled to the first track's
    // instrument (spec `plugin-gui.md` §8): a fresh project plays from the
    // computer keyboard, but through an explicit, visible, deletable route
    // instead of the old implicit track fallback.
    if let Some(voice_id) = first_voice {
        let keyboard_id = next_id;
        next_id += 1;
        nodes.push(NodeDoc::Keyboard {
            node_id: keyboard_id,
            track: 0,
            source: MidiSource::Computer,
            transpose: 0,
            octave: DEFAULT_KEYBOARD_OCTAVE,
        });
        cables.push(CableDoc::keyboard_midi(keyboard_id, voice_id));
    }

    // Audio tracks get their generator node so the lane is visible in the
    // patch; it starts unwired (the single mixer channel is already taken),
    // and the user decides where it lands.
    for track in 0..audio_track_count {
        let node_id = next_id;
        next_id += 1;
        nodes.push(NodeDoc::AudioTrack { node_id, track });
    }
    let _ = next_id;

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
    /// Named clip groups (W3b, spec `timeline.md` D28). Membership lives on
    /// the clips (`group: Option<u32>`); an empty table means no groups.
    #[serde(default)]
    pub clip_groups: Vec<ClipGroup>,
    /// Node graph document (D13 fully explicit).
    #[serde(default)]
    pub graph: GraphDoc,
    /// Directory the project file lives in (R4 D11): session-only, **never
    /// serialized**. Relative clip paths resolve against it, so a project
    /// copied together with its `<stem>.media/` folder plays anywhere.
    #[serde(skip)]
    pub base_dir: Option<PathBuf>,
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
            control_tracks: vec![ControlTrack::default(); DEFAULT_CONTROL_TRACKS],
            audio_tracks: vec![AudioTrack::default(); DEFAULT_AUDIO_TRACKS],
            clip_groups: Vec::new(),
            graph: default_graph_doc(DEFAULT_MIDI_TRACKS, DEFAULT_AUDIO_TRACKS),
            base_dir: None,
        }
    }
}

/// Maximum arrangement length in ticks (D16 safety bound).
pub const MAX_ARRANGEMENT_TICKS: u64 = (MAX_ARRANGEMENT_BARS as u64) * (TICKS_PER_BAR as u64);

/// S2-5b: whether two nodes would construct the same processor. Values that
/// a live processor accepts through `ParamId` are excluded.
fn node_engine_identity_eq(a: &NodeDoc, b: &NodeDoc) -> bool {
    if a.node_id() != b.node_id() || a.track() != b.track() {
        return false;
    }
    match (a, b) {
        (NodeDoc::Instrument { voice: va, .. }, NodeDoc::Instrument { voice: vb, .. }) => {
            voice_engine_identity_eq(va, vb)
        }
        (
            NodeDoc::Effect {
                voice: va,
                params: pa,
                ..
            },
            NodeDoc::Effect {
                voice: vb,
                params: pb,
                ..
            },
        ) => voice_engine_identity_eq(va, vb) && pa.kind() == pb.kind(),
        // A mixer's channel count decides its ports; faders are replayed.
        (NodeDoc::Mixer { inputs: ia, .. }, NodeDoc::Mixer { inputs: ib, .. }) => ia == ib,
        // UI-only node: no engine counterpart, but a changed node id / track is
        // still an identity change for the graph document.
        (NodeDoc::Keyboard { .. }, NodeDoc::Keyboard { .. }) => true,
        // Parameter-only engine nodes (all values replay through ParamId).
        (NodeDoc::Arpeggiator { .. }, NodeDoc::Arpeggiator { .. })
        | (NodeDoc::Portamento { .. }, NodeDoc::Portamento { .. })
        | (NodeDoc::MidiEnvelope { .. }, NodeDoc::MidiEnvelope { .. })
        | (NodeDoc::Lfo { .. }, NodeDoc::Lfo { .. })
        | (NodeDoc::CurveMap { .. }, NodeDoc::CurveMap { .. })
        | (NodeDoc::Feedback { .. }, NodeDoc::Feedback { .. })
        | (NodeDoc::Input { .. }, NodeDoc::Input { .. })
        | (NodeDoc::AudioTrack { .. }, NodeDoc::AudioTrack { .. }) => true,
        // Legacy load-only node: never worth a fast path.
        (NodeDoc::ChannelStrip { .. }, NodeDoc::ChannelStrip { .. }) => a == b,
        _ => false,
    }
}

/// S2-5b: whether two voices load the same source. Preset / root / gain /
/// bend / drum trim values are parameters; paths and plugin state are not.
fn voice_engine_identity_eq(a: &VoiceSpec, b: &VoiceSpec) -> bool {
    match (a, b) {
        (VoiceSpec::Builtin, VoiceSpec::Builtin) => true,
        // No file to reload: every difference is a replayed parameter, so the
        // running engine (including its ringing voices) must survive an edit.
        (VoiceSpec::Modal { .. }, VoiceSpec::Modal { .. })
        | (VoiceSpec::Pluck { .. }, VoiceSpec::Pluck { .. })
        | (VoiceSpec::Organ { .. }, VoiceSpec::Organ { .. }) => true,
        (VoiceSpec::Wavetable { .. }, VoiceSpec::Wavetable { .. }) => true,
        (VoiceSpec::SoundFont { path: pa, .. }, VoiceSpec::SoundFont { path: pb, .. }) => pa == pb,
        (VoiceSpec::Sample { path: pa, .. }, VoiceSpec::Sample { path: pb, .. }) => pa == pb,
        (VoiceSpec::DrumKit { slots: sa, .. }, VoiceSpec::DrumKit { slots: sb, .. }) => {
            sa.len() == sb.len()
                && sa
                    .iter()
                    .zip(sb)
                    .all(|(x, y)| x.note == y.note && x.path == y.path)
        }
        (
            VoiceSpec::Clap {
                path: pa,
                plugin_id: ia,
                state: sa,
            },
            VoiceSpec::Clap {
                path: pb,
                plugin_id: ib,
                state: sb,
            },
        ) => pa == pb && ia == ib && sa == sb,
        (
            VoiceSpec::Vst3 {
                path: pa,
                class_id: ca,
                state: sa,
            },
            VoiceSpec::Vst3 {
                path: pb,
                class_id: cb,
                state: sb,
            },
        ) => pa == pb && ca == cb && sa == sb,
        _ => false,
    }
}

impl Project {
    /// Whether the real-time graph would be identical for both documents:
    /// node payloads, cables, and the three track-list lengths.
    ///
    /// Clip contents, clip names/colours, marker text and other view-only
    /// fields are deliberately excluded — they are pushed to the engine
    /// separately (S2-5). The UI uses this to skip a full graph rebuild when
    /// undo/redo only touched clips.
    pub fn topology_eq(&self, other: &Project) -> bool {
        self.graph.nodes == other.graph.nodes
            && self.graph.cables == other.graph.cables
            && self.midi_tracks.len() == other.midi_tracks.len()
            && self.control_tracks.len() == other.control_tracks.len()
            && self.audio_tracks.len() == other.audio_tracks.len()
    }

    /// Whether the engine would keep the same **processors and wiring**,
    /// ignoring values that can be replayed through `ParamId` (synth/effect
    /// faders, mixer faders, LFO rate, …).
    ///
    /// This is the S2-5b predicate: parameter-only edits can push values to
    /// the live processors instead of re-instantiating them. Anything that
    /// changes which processor would be constructed — a voice path / plugin
    /// ID / plugin state, the built-in effect kind, a mixer's channel count, a
    /// drum slot's key or sample — still counts as a new topology.
    pub fn engine_topology_eq(&self, other: &Project) -> bool {
        self.graph.nodes.len() == other.graph.nodes.len()
            && self
                .graph
                .nodes
                .iter()
                .zip(&other.graph.nodes)
                .all(|(a, b)| node_engine_identity_eq(a, b))
            && self.graph.cables == other.graph.cables
            && self.midi_tracks.len() == other.midi_tracks.len()
            && self.control_tracks.len() == other.control_tracks.len()
            && self.audio_tracks.len() == other.audio_tracks.len()
    }

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
        // 0) Load-time parameter clamp for the MIDI nodes (spec
        // `midi-nodes.md` §7). Additive: no version change, idempotent.
        for node in &mut self.graph.nodes {
            match node {
                NodeDoc::Arpeggiator { params, .. } => *params = params.sanitized(),
                NodeDoc::Portamento { params, .. } => *params = params.sanitized(),
                NodeDoc::MidiEnvelope { params, .. } => *params = params.sanitized(),
                // LFO rate / depth / division (spec `lfo.md` D8).
                NodeDoc::Lfo { params, .. } => *params = params.sanitized(),
                // Curve Map axis (spec `curve-map.md` D5): a hand-written
                // file must not reach the audio thread with a backwards or
                // non-finite range.
                NodeDoc::CurveMap { params, .. } => *params = params.sanitized(),
                // SF2 bend range (spec `curve-map.md` D8): 1..=24 st, GM 2.
                NodeDoc::Instrument {
                    voice: VoiceSpec::SoundFont { bend_range, .. },
                    ..
                } => {
                    *bend_range = if bend_range.is_finite() {
                        bend_range.clamp(1.0, 24.0)
                    } else {
                        2.0
                    };
                }
                _ => {}
            }
        }
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
                    channels: vec![MixerChannel {
                        name: String::new(),
                        gain: *gain,
                        pan: *pan,
                        muted: *muted,
                        solo: false,
                    }],
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

    /// v8 → v9 (spec `.agents/spec/curve-map.md` D1/D7): curve anchors move
    /// from the target's real domain to the normalized `[-1, 1]` domain.
    ///
    /// The axis is resolved **before** the anchors are moved (target axis,
    /// including effect slot ranges which live in the target node's payload),
    /// so an old project's curves keep their shape and their sound. Nothing is
    /// written onto the cables: `map: None` means "derive from the target",
    /// which keeps a curve glued to the range of whatever it points at later
    /// instead of to the range it happened to have at migration time.
    ///
    /// D7 last clause: a curve on a track with no cable has no axis to invert.
    /// Its values are clamped into `[-1, 1]` and kept — silently, because such
    /// a curve is not driving anything (it only becomes audible once the user
    /// routes it, at which point they pick the axis).
    pub fn migrate_v8_to_v9(&mut self) {
        // One resolved axis per control track.
        let mut track_maps: Vec<Option<MapSpec>> = vec![None; self.control_tracks.len()];
        for cable in &self.graph.cables {
            if cable.kind != CableKind::Automation {
                continue;
            }
            let PortRef::AutoTrack(track) = cable.from else {
                continue;
            };
            let Some(slot) = track_maps.get_mut(track) else {
                continue;
            };
            *slot = Some(
                cable
                    .map
                    .unwrap_or_else(|| cable.param.default_map(&self.graph, &cable.to)),
            );
        }
        for (track, track_map) in self.control_tracks.iter_mut().zip(track_maps) {
            for clip in &mut track.clips {
                for point in &mut clip.curve {
                    point.value = match track_map {
                        Some(map) => map.unmap(point.value),
                        None if point.value.is_finite() => point.value.clamp(-1.0, 1.0),
                        None => 0.0,
                    };
                }
            }
        }
    }

    /// Resolves a document media path (R4 D11): an absolute path is used as-is;
    /// a relative one is joined to the project directory. With no `base_dir`
    /// (unsaved project / bare document) the relative path is returned
    /// unchanged, which keeps tests and headless tools working against the cwd.
    ///
    /// Stored paths use `/` (S3b): a project written on Windows has to open on
    /// macOS and the other way round, so a `\` from an older file is folded back
    /// to `/` here instead of being read as a file name character on unix.
    pub fn resolve_media_path(&self, path: &str) -> PathBuf {
        let stored = path.replace('\\', "/");
        let candidate = Path::new(&stored);
        if candidate.is_absolute() {
            return candidate.to_path_buf();
        }
        match &self.base_dir {
            Some(dir) => dir.join(candidate),
            None => candidate.to_path_buf(),
        }
    }

    /// The string to store in a clip for `path`: relative to the project
    /// directory when the file lives under it, absolute otherwise. Separators
    /// are written as `/` so the project file stays portable (S3b).
    pub fn media_path_string(&self, path: &Path) -> String {
        let text = match &self.base_dir {
            Some(dir) => path
                .strip_prefix(dir)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string(),
            None => path.to_string_lossy().to_string(),
        };
        text.replace('\\', "/")
    }

    /// Every media file the document references (S3b): sample instruments,
    /// SoundFonts, drum-kit slots and audio clips, as the strings stored in the
    /// document. Unloaded nodes / clips (empty path) are skipped, duplicates
    /// appear once.
    ///
    /// Plugin binaries (`VoiceSpec::Clap` / `Vst3`) are deliberately **not**
    /// media: they are installed software, not project assets.
    pub fn media_paths(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for node in &self.graph.nodes {
            let voice = match node {
                NodeDoc::Instrument { voice, .. } | NodeDoc::Effect { voice, .. } => voice,
                _ => continue,
            };
            match voice {
                VoiceSpec::Sample { path, .. } | VoiceSpec::SoundFont { path, .. } => {
                    push_media_path(&mut out, path);
                }
                VoiceSpec::DrumKit { slots, .. } => {
                    for slot in slots {
                        push_media_path(&mut out, &slot.path);
                    }
                }
                _ => {}
            }
        }
        for track in &self.audio_tracks {
            for clip in &track.clips {
                push_media_path(&mut out, &clip.path);
            }
        }
        out
    }

    /// Rewrites stored media paths through `map` (old string → new string);
    /// anything not in the map is left alone. The building block of "collect
    /// media files" and "relocate missing media" (S3b) — both are one edit.
    pub fn rewrite_media_paths(&mut self, map: &HashMap<String, String>) {
        if map.is_empty() {
            return;
        }
        for node in &mut self.graph.nodes {
            let voice = match node {
                NodeDoc::Instrument { voice, .. } | NodeDoc::Effect { voice, .. } => voice,
                _ => continue,
            };
            match voice {
                VoiceSpec::Sample { path, .. } | VoiceSpec::SoundFont { path, .. } => {
                    if let Some(new) = map.get(path.as_str()) {
                        *path = new.clone();
                    }
                }
                VoiceSpec::DrumKit { slots, .. } => {
                    for slot in slots.iter_mut() {
                        if let Some(new) = map.get(slot.path.as_str()) {
                            slot.path = new.clone();
                        }
                    }
                }
                _ => {}
            }
        }
        for track in &mut self.audio_tracks {
            for clip in &mut track.clips {
                if let Some(new) = map.get(clip.path.as_str()) {
                    clip.path = new.clone();
                }
            }
        }
    }

    /// Referenced media paths that do not resolve to an existing file, as the
    /// **stored** strings (so the caller can rewrite them by key).
    pub fn missing_media(&self) -> Vec<String> {
        self.media_paths()
            .into_iter()
            .filter(|stored| !self.resolve_media_path(stored).is_file())
            .collect()
    }

    /// The document's bar length in ticks (U3a): every bar-based measure
    /// (grid, loop snapping, arrangement rounding, ruler) reads this instead
    /// of the 4/4 [`TICKS_PER_BAR`] constant.
    pub fn ticks_per_bar(&self) -> u32 {
        self.metadata.time_signature.ticks_per_bar()
    }

    /// The piecewise meter table (Met M3): the base meter plus every
    /// [`MeterMarker`]. Building it is cheap (one small table); the UI and the
    /// engine each keep their own cache and rebuild it on document changes.
    pub fn meter_map(&self) -> MeterMap {
        MeterMap::new(self.metadata.time_signature, &self.metadata.meter_markers)
    }

    /// 0-based `(bar, beat)` at an absolute tick, following meter changes.
    pub fn bar_beat_at(&self, tick: u32) -> (u32, u32) {
        self.meter_map().bar_beat_at(tick)
    }

    /// Bar length in ticks at an absolute tick, following meter changes.
    pub fn ticks_per_bar_at(&self, tick: u32) -> u32 {
        self.meter_map().ticks_per_bar_at(tick)
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

    /// Approximate heap footprint of the document, in bytes.
    ///
    /// The undo history keeps whole-document snapshots, so it needs a size
    /// signal to bound its own memory (see
    /// `glitcherz_command::Session::history_byte_limit`). Exactness is not
    /// required — the job is to notice a project built from millions of notes
    /// or holding a tens-of-megabytes plugin state — so vectors are priced as
    /// `len * size_of::<T>()` rather than by walking every element, and node
    /// payloads that never grow large (mixer channel tables, arpeggiator
    /// steps) are ignored.
    pub fn approx_bytes(&self) -> usize {
        let mut total = std::mem::size_of::<Self>();

        total += self.metadata.name.len();
        total += self.metadata.markers.len() * std::mem::size_of::<Marker>();
        total += self.metadata.meter_markers.len() * std::mem::size_of::<MeterMarker>();
        total += self.clip_groups.len() * std::mem::size_of::<ClipGroup>();
        for group in &self.clip_groups {
            total += group.name.len();
        }

        for track in &self.midi_tracks {
            total += std::mem::size_of::<MidiTrack>() + track.name.len();
            total += track.clips.len() * std::mem::size_of::<MidiClip>();
            for clip in &track.clips {
                total += clip.name.len();
                total += clip.sequence.notes.len() * std::mem::size_of::<Note>();
                total += clip.sequence.ctrls.len() * std::mem::size_of::<CtrlEvent>();
            }
        }

        for track in &self.control_tracks {
            total += std::mem::size_of::<ControlTrack>() + track.name.len();
            total += track.clips.len() * std::mem::size_of::<ControlClip>();
            for clip in &track.clips {
                total += clip.name.len();
                total += clip.curve.len() * std::mem::size_of::<AutoPoint>();
            }
        }

        for track in &self.audio_tracks {
            total += std::mem::size_of::<AudioTrack>() + track.name.len();
            total += track.clips.len() * std::mem::size_of::<AudioClip>();
            for clip in &track.clips {
                total += clip.name.len() + clip.path.len();
            }
        }

        total += self.graph.cables.len() * std::mem::size_of::<CableDoc>();
        for node in &self.graph.nodes {
            total += std::mem::size_of::<NodeDoc>() + node.payload_bytes();
        }

        total
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
        // W3b: clip groups are a flat, bounded table with unique ids. A clip's
        // dangling `group` reference is harmless (it renders as ungrouped) and
        // is cleaned when a group is dissolved, so it is not a file error.
        if self.clip_groups.len() > MAX_CLIP_GROUPS {
            return Err(format!(
                "clip group count {} exceeds the limit {MAX_CLIP_GROUPS}",
                self.clip_groups.len()
            ));
        }
        for (i, group) in self.clip_groups.iter().enumerate() {
            if group.id == 0 {
                return Err("clip group id 0 is reserved".to_string());
            }
            if self.clip_groups[i + 1..]
                .iter()
                .any(|other| other.id == group.id)
            {
                return Err(format!("duplicate clip group id {}", group.id));
            }
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
        // Met M3: meter changes are a bounded, strictly ascending table. The
        // bar-line position is deliberately *not* validated: `MeterMap::new`
        // snaps a stray marker down instead of refusing the document (D2).
        if self.metadata.meter_markers.len() > MAX_METER_MARKERS {
            return Err(format!(
                "meter change count {} exceeds the limit {MAX_METER_MARKERS}",
                self.metadata.meter_markers.len()
            ));
        }
        for (i, marker) in self.metadata.meter_markers.iter().enumerate() {
            if !marker.signature.is_valid() {
                return Err(format!(
                    "meter change #{i} has an invalid time signature {}",
                    marker.signature
                ));
            }
            if marker.tick == 0 {
                return Err("meter change at tick 0 is reserved for the base meter".to_string());
            }
            if u64::from(marker.tick) > MAX_ARRANGEMENT_TICKS {
                return Err(format!(
                    "meter change #{i} lies beyond the {MAX_ARRANGEMENT_BARS}-bar arrangement bound"
                ));
            }
            if i > 0 && marker.tick <= self.metadata.meter_markers[i - 1].tick {
                return Err(format!(
                    "meter change #{i} is not strictly after the previous one"
                ));
            }
        }
        self.graph
            .validate(self.midi_tracks.len(), self.control_tracks.len())?;
        // R6 D21: an audio recording input must reference a real audio track.
        // The graph document only knows the universal cap; the project owns the
        // actual count.
        for cable in &self.graph.cables {
            if let PortRef::TrackIn {
                kind: TrackKind::Audio,
                track,
            } = cable.to
                && track >= self.audio_tracks.len()
            {
                return Err(format!(
                    "audio recording input {track} out of range (track count = {})",
                    self.audio_tracks.len()
                ));
            }
        }
        Ok(())
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
    /// Musical meter (U3a): the base meter, in effect from tick 0 until the
    /// first [`MeterMarker`]. Documents written before it existed load as 4/4.
    #[serde(default)]
    pub time_signature: TimeSignature,
    /// Mid-song meter changes (Met M3; spec `meter-map.md` D1–D3). Each entry
    /// says "from this tick on, the meter is X". Kept sorted by tick and never
    /// placed at tick 0 (that is what `time_signature` expresses). Additive
    /// field: older files load with an empty table, which evaluates exactly
    /// like the single-meter behavior.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub meter_markers: Vec<MeterMarker>,
}

/// A mid-song meter change (Met M3). See [`ProjectMetadata::meter_markers`].
///
/// `tick` must be `> 0` and, for the file layer, strictly ascending. It should
/// also sit on a bar line; [`MeterMap::new`] snaps a stray value down to the
/// containing segment's grid instead of rejecting the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MeterMarker {
    #[serde(default)]
    pub tick: u32,
    #[serde(default)]
    pub signature: TimeSignature,
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

/// Maximum number of meter changes (Met M3). Matches the engine's fixed
/// segment table (`MAX_METER_SEGS`), so the UI can always push a valid map.
pub const MAX_METER_MARKERS: usize = 64;

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
            meter_markers: Vec::new(),
        }
    }
}

/// Appends `path` to `out` unless it is empty or already listed (S3b: one
/// media file can be shared by several clips / nodes).
fn push_media_path(out: &mut Vec<String>, path: &str) {
    if !path.is_empty() && !out.iter().any(|existing| existing == path) {
        out.push(path.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ArpMode, ArpeggiatorParams, AudioClip, AudioTrack, AutoParam, AutoPoint, CableDoc,
        CableKind, ChannelParam, ControlClip, ControlTrack, CurveMapParams, DEFAULT_AUDIO_TRACKS,
        DEFAULT_CONTROL_TRACKS, DEFAULT_KEYBOARD_OCTAVE, DEFAULT_MIDI_TRACKS, DelayParams,
        EffectParams, GraphDoc, LFO_DEFAULT_DIVISION, LFO_DIVISION_COUNT, LFO_DIVISION_LABELS,
        LfoParams, LfoWaveform, MASTER_NODE_ID, MAX_ARRANGEMENT_BARS, MAX_ARRANGEMENT_TICKS,
        MAX_AUDIO_TRACKS, MAX_BEND_RANGE, MAX_CLIP_NOTES, MAX_CLIPS_PER_TRACK, MAX_NODES,
        MAX_TEMPO_BPM, MIN_BEND_RANGE, MIN_TEMPO_BPM, MapMode, MapSpec, MidiClip,
        MidiEnvelopeParams, MidiSource, MidiTrack, MixerChannel, NodeDoc, NodeParam, PortRef,
        PortamentoParams, Project, SamplePlayMode, SampleRegion, SynthParams, SynthWaveform,
        TICKS_PER_BAR, TICKS_PER_QUARTER, TICKS_PER_STEP, TrackKind, VoiceSpec,
        default_audio_source_len, default_graph_doc,
    };

    #[test]
    fn node_param_keys_round_trip() {
        // Every parameterless key resolves back to the same variant.
        for key in [
            "mixer_out_pan",
            "mixer_out_mute",
            "feedback_gain",
            "sf2_gain",
            "sample_gain",
            "pitch_offset",
            "resonance",
            "attack",
            "decay",
            "sustain",
            "release",
            "modal_decay",
            "modal_beating",
            "modal_hardness",
            "modal_brightness",
            "modal_gain",
            "pluck_decay",
            "pluck_damping",
            "pluck_brightness",
            "pluck_release",
            "pluck_gain",
            "organ_percussion",
            "organ_percussion_decay",
            "organ_attack",
            "organ_release",
            "organ_gain",
            "wavetable_position",
            "wavetable_attack",
            "wavetable_release",
            "wavetable_gain",
        ] {
            let param = NodeParam::from_key(key).expect("known key");
            assert_eq!(param.key(), key);
        }
        assert_eq!(
            NodeParam::from_key_indexed("effect_slot", 2),
            Some(NodeParam::EffectSlot(2))
        );
        assert_eq!(
            NodeParam::from_key_indexed("organ_bar", 8),
            Some(NodeParam::OrganBar(8))
        );
        assert_eq!(NodeParam::from_key_indexed("organ_bar", 9), None);
        assert_eq!(NodeParam::from_key("effect_slot"), None);
        for key in ["gain", "pan", "mute", "solo"] {
            let param = ChannelParam::from_key(key).expect("known channel key");
            assert_eq!(param.key(), key);
        }
    }

    #[test]
    fn node_params_read_and_write() {
        let mut instrument = NodeDoc::Instrument {
            node_id: 2,
            track: 0,
            voice: VoiceSpec::Builtin,
            synth: SynthParams::default(),
        };
        assert!(super::set_node_param(
            &mut instrument,
            NodeParam::Attack,
            0.25
        ));
        assert_eq!(
            super::node_param_value(&instrument, NodeParam::Attack),
            Some(0.25)
        );
        // Clamped to the documented 0..2 domain.
        assert!(super::set_node_param(
            &mut instrument,
            NodeParam::Attack,
            9.0
        ));
        assert_eq!(
            super::node_param_value(&instrument, NodeParam::Attack),
            Some(2.0)
        );
        // A parameter of another node kind is refused, not silently ignored.
        assert!(!super::set_node_param(
            &mut instrument,
            NodeParam::FeedbackGain,
            0.5
        ));

        let mut mixer = NodeDoc::Mixer {
            node_id: 5,
            track: 0,
            inputs: 1,
            channels: vec![MixerChannel {
                name: String::new(),
                gain: 0.8,
                pan: 0.0,
                muted: false,
                solo: false,
            }],
            out_pan: 0.0,
            out_muted: false,
        };
        assert!(super::set_node_param(
            &mut mixer,
            NodeParam::MixerChannel {
                channel: 0,
                param: ChannelParam::Pan,
            },
            -0.5
        ));
        assert_eq!(
            super::node_param_value(
                &mixer,
                NodeParam::MixerChannel {
                    channel: 0,
                    param: ChannelParam::Pan,
                }
            ),
            Some(-0.5)
        );
        assert!(super::set_node_param(
            &mut mixer,
            NodeParam::MixerOutMute,
            1.0
        ));
        assert_eq!(
            super::node_param_value(&mixer, NodeParam::MixerOutMute),
            Some(1.0)
        );
    }

    fn effect_node(params: EffectParams) -> NodeDoc {
        NodeDoc::Effect {
            node_id: 3,
            track: 0,
            voice: VoiceSpec::Builtin,
            params,
            delay: None,
        }
    }

    /// B1: the modal voice is an additive variant — a payload with no
    /// parameters loads with the documented defaults, and the graph accepts
    /// it as an ordinary single-output instrument.
    #[test]
    fn modal_voice_round_trips_and_defaults() {
        let params = super::ModalParams {
            preset: super::ModalPreset::SingingBowl,
            decay_scale: 2.5,
            beating: 0.5,
            ..Default::default()
        };
        let node = NodeDoc::Instrument {
            node_id: 2,
            track: 0,
            voice: VoiceSpec::Modal { params },
            synth: SynthParams::default(),
        };
        let text = ron::to_string(&node).expect("serialize");
        assert!(text.contains("modal"), "voice tag missing: {text}");
        assert!(text.contains("singing_bowl"), "preset tag missing: {text}");
        let back: NodeDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(node, back);

        // A payload written before the macros existed still loads.
        let legacy: NodeDoc =
            ron::from_str(r#"(type:"instrument", node_id:3, track:0, voice:(type:"modal"))"#)
                .expect("a bare modal payload should parse");
        let NodeDoc::Instrument {
            voice: VoiceSpec::Modal { params },
            ..
        } = legacy
        else {
            panic!("expected a modal instrument");
        };
        assert_eq!(params, super::ModalParams::default());
        assert_eq!(params.preset, super::ModalPreset::Mallet);
        assert!((params.decay_scale - 1.0).abs() < 1e-6);

        let graph = GraphDoc {
            nodes: vec![node],
            cables: vec![CableDoc::audio(2u64, PortRef::Master)],
            ..Default::default()
        };
        graph
            .validate(2, 0)
            .expect("a modal instrument must validate");
    }

    /// S1: the pluck voice is additive too — a graph written before the
    /// variant existed loads with the documented defaults, and the payload
    /// round-trips through RON without losing a parameter.
    #[test]
    fn pluck_voice_round_trips_and_defaults() {
        let params = super::PluckParams {
            decay_s: 6.5,
            damping: 0.7,
            ..Default::default()
        };
        let node = NodeDoc::Instrument {
            node_id: 2,
            track: 0,
            voice: VoiceSpec::Pluck { params },
            synth: SynthParams::default(),
        };
        let text = ron::to_string(&node).expect("serialize");
        assert!(text.contains("pluck"), "voice tag missing: {text}");
        let back: NodeDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(node, back);

        // A bare payload loads with the documented defaults.
        let legacy: NodeDoc =
            ron::from_str(r#"(type:"instrument", node_id:3, track:0, voice:(type:"pluck"))"#)
                .expect("a bare pluck payload should parse");
        let NodeDoc::Instrument {
            voice: VoiceSpec::Pluck { params },
            ..
        } = legacy
        else {
            panic!("expected a pluck instrument");
        };
        assert_eq!(params, super::PluckParams::default());
        assert!((params.decay_s - 4.0).abs() < 1e-6);
        assert!((params.release_s - 0.15).abs() < 1e-6);

        let graph = GraphDoc {
            nodes: vec![node],
            cables: vec![CableDoc::audio(2u64, PortRef::Master)],
            ..Default::default()
        };
        graph
            .validate(2, 0)
            .expect("a pluck instrument must validate");
    }

    /// S2: the organ payload is additive as well — a bare `organ` tag loads
    /// with the documented defaults (the `888000000` registration), and the
    /// nine drawbar levels survive a RON round-trip.
    #[test]
    fn organ_voice_round_trips_and_defaults() {
        let params = super::OrganParams {
            bars: [8, 0, 8, 0, 0, 3, 0, 0, 0],
            percussion: 0.7,
            percussion_third: true,
            ..Default::default()
        };
        let node = NodeDoc::Instrument {
            node_id: 2,
            track: 0,
            voice: VoiceSpec::Organ { params },
            synth: SynthParams::default(),
        };
        let text = ron::to_string(&node).expect("serialize");
        assert!(text.contains("organ"), "voice tag missing: {text}");
        let back: NodeDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(node, back);

        let legacy: NodeDoc =
            ron::from_str(r#"(type:"instrument", node_id:3, track:0, voice:(type:"organ"))"#)
                .expect("a bare organ payload should parse");
        let NodeDoc::Instrument {
            voice: VoiceSpec::Organ { params },
            ..
        } = legacy
        else {
            panic!("expected an organ instrument");
        };
        assert_eq!(params, super::OrganParams::default());
        assert_eq!(params.bars, [8, 8, 8, 0, 0, 0, 0, 0, 0]);
        assert!((params.attack_s - 0.005).abs() < 1e-6);
        assert!((params.gain - 1.0).abs() < 1e-6);

        let graph = GraphDoc {
            nodes: vec![node],
            cables: vec![CableDoc::audio(2u64, PortRef::Master)],
            ..Default::default()
        };
        graph
            .validate(2, 0)
            .expect("an organ instrument must validate");
    }

    /// S3: the wavetable payload is additive too — a bare `wavetable` tag loads
    /// with the documented defaults (sine frame, 5 ms attack), and the
    /// position survives a RON round-trip.
    #[test]
    fn wavetable_voice_round_trips_and_defaults() {
        let params = super::WavetableParams {
            position: 0.625,
            attack_s: 0.3,
            release_s: 1.5,
            gain: 0.8,
        };
        let node = NodeDoc::Instrument {
            node_id: 2,
            track: 0,
            voice: VoiceSpec::Wavetable { params },
            synth: SynthParams::default(),
        };
        let text = ron::to_string(&node).expect("serialize");
        assert!(text.contains("wavetable"), "voice tag missing: {text}");
        let back: NodeDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(node, back);

        let legacy: NodeDoc =
            ron::from_str(r#"(type:"instrument", node_id:3, track:0, voice:(type:"wavetable"))"#)
                .expect("a bare wavetable payload should parse");
        let NodeDoc::Instrument {
            voice: VoiceSpec::Wavetable { params },
            ..
        } = legacy
        else {
            panic!("expected a wavetable instrument");
        };
        assert_eq!(params, super::WavetableParams::default());
        assert!((params.position - 0.0).abs() < 1e-6);
        assert!((params.release_s - 0.12).abs() < 1e-6);
        assert!((params.gain - 1.0).abs() < 1e-6);

        let graph = GraphDoc {
            nodes: vec![node],
            cables: vec![CableDoc::audio(2u64, PortRef::Master)],
            ..Default::default()
        };
        graph
            .validate(2, 0)
            .expect("a wavetable instrument must validate");
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

    // Z0: the port profile is derived from the effect kind, the document
    // validator enforces the same port range / per-port uniqueness, and the
    // new payload round-trips without a PROJECT_VERSION bump.
    #[test]
    fn dual_input_effect_validates_both_audio_ports() {
        let instrument = |node_id: u64, track: usize| NodeDoc::Instrument {
            node_id,
            track,
            voice: VoiceSpec::Builtin,
            synth: SynthParams::default(),
        };
        let dual = NodeDoc::Effect {
            node_id: 4,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: EffectParams::RingMod(Box::default()),
            delay: None,
        };
        assert_eq!(dual.audio_inputs(), 2, "Ring Mod has Main + Mod");
        assert_eq!(
            effect_node(EffectParams::Delay(DelayParams::default())).audio_inputs(),
            1,
            "every existing effect stays single-input"
        );

        let build = |cables: Vec<CableDoc>| GraphDoc {
            nodes: vec![instrument(2, 0), instrument(3, 1), dual.clone()],
            cables,
            ..Default::default()
        };
        let legal = build(vec![
            CableDoc::audio_to_port(2u64, 4u64, 0),
            CableDoc::audio_to_port(3u64, 4u64, 1),
            CableDoc::audio(4u64, PortRef::Master),
        ]);
        legal.validate(2, 0).expect("Main + Mod must be legal");
        assert_eq!(legal.first_free_port(4), None, "both ports are occupied");
        assert_eq!(legal.first_free_port(2), Some(0));

        // A port that does not exist must be rejected by the document layer.
        let mut bad = legal.clone();
        bad.cables[1] = CableDoc::audio_to_port(3u64, 4u64, 2);
        assert!(bad.validate(2, 0).is_err());

        // Two cables may not share the Mod port.
        let mut bad = legal.clone();
        bad.cables[0] = CableDoc::audio_to_port(2u64, 4u64, 1);
        assert!(bad.validate(2, 0).is_err());

        // Plain nodes keep the pre-Z0 rule: port 0 only.
        let mut bad = legal.clone();
        bad.cables[0] = CableDoc::audio_to_port(2u64, 3u64, 1);
        assert!(bad.validate(2, 0).is_err());

        let text = ron::to_string(&dual).expect("serialize");
        assert!(text.contains("ring_mod"), "tag missing: {text}");
        let back: NodeDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(dual, back);
    }

    /// Z1 merge: the compressor is a dual-input kind. Both ports validate, the
    /// keying switch survives a round trip, and pre-merge
    /// `sidechain_compressor` payloads load through the serde alias.
    #[test]
    fn compressor_has_two_ports_and_a_persistent_switch() {
        let node = NodeDoc::Effect {
            node_id: 4,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: EffectParams::Compressor(Box::default()),
            delay: None,
        };
        assert_eq!(node.audio_inputs(), 2, "Main + Mod");
        let kind = EffectParams::Compressor(Box::default()).kind();
        assert_eq!(kind, super::BuiltinEffectKind::Compressor);
        assert_eq!(kind.label(), "Compressor");

        let instrument = |node_id: u64, track: usize| NodeDoc::Instrument {
            node_id,
            track,
            voice: VoiceSpec::Builtin,
            synth: SynthParams::default(),
        };
        let graph = GraphDoc {
            nodes: vec![instrument(2, 0), instrument(3, 1), node.clone()],
            cables: vec![
                CableDoc::audio_to_port(2u64, 4u64, 0),
                CableDoc::audio_to_port(3u64, 4u64, 1),
                CableDoc::audio(4u64, PortRef::Master),
            ],
            ..Default::default()
        };
        graph.validate(2, 0).expect("Main + Mod must be legal");

        let text = ron::to_string(&node).expect("serialize");
        assert!(text.contains("compressor"), "tag missing: {text}");
        assert!(text.contains("sidechain:"), "switch missing: {text}");
        let back: NodeDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(node, back);

        // A payload that predates the switch loads with keying off.
        let legacy: NodeDoc = ron::from_str(
            r#"(type:"effect", node_id:3, track:0, voice:(type:"builtin"),
                params:(kind:"compressor"))"#,
        )
        .expect("a bare compressor payload should parse");
        let NodeDoc::Effect { params, .. } = legacy else {
            panic!("expected an effect node");
        };
        let comp = params.as_compressor().expect("compressor payload");
        assert!(!comp.sidechain, "keying starts disabled");

        // A pre-merge sidechain payload loads as a Compressor via the alias,
        // keeping its keying state and its two ports.
        let merged: NodeDoc = ron::from_str(
            r#"(type:"effect", node_id:5, track:0, voice:(type:"builtin"),
                params:(kind:"sidechain_compressor", sidechain:true))"#,
        )
        .expect("a pre-merge sidechain payload should parse");
        assert_eq!(merged.audio_inputs(), 2, "the alias keeps both ports");
        let NodeDoc::Effect { params, .. } = merged else {
            panic!("expected an effect node");
        };
        assert_eq!(params.kind(), super::BuiltinEffectKind::Compressor);
        let comp = params.as_compressor().expect("compressor payload");
        assert!(comp.sidechain, "keying survives the alias");
    }

    // Z2: the vocoder is the second real dual-input effect, and its payload
    // must survive a round trip plus the "written by a newer build" path.
    #[test]
    fn vocoder_has_two_ports_and_round_trips_its_macros() {
        let node = NodeDoc::Effect {
            node_id: 4,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: EffectParams::Vocoder(Box::new(super::VocoderParams {
                q: 8.0,
                low: 90.0,
                sibilance: 0.6,
                ..Default::default()
            })),
            delay: None,
        };
        assert_eq!(node.audio_inputs(), 2, "Main = carrier, Mod = modulator");
        let kind = EffectParams::Vocoder(Box::default()).kind();
        assert_eq!(kind, super::BuiltinEffectKind::Vocoder);
        assert_eq!(kind.label(), "Vocoder");

        let instrument = |node_id: u64, track: usize| NodeDoc::Instrument {
            node_id,
            track,
            voice: VoiceSpec::Builtin,
            synth: SynthParams::default(),
        };
        let graph = GraphDoc {
            nodes: vec![instrument(2, 0), instrument(3, 1), node.clone()],
            cables: vec![
                CableDoc::audio_to_port(2u64, 4u64, 0),
                CableDoc::audio_to_port(3u64, 4u64, 1),
                CableDoc::audio(4u64, PortRef::Master),
            ],
            ..Default::default()
        };
        graph
            .validate(2, 0)
            .expect("carrier + modulator must be legal");

        let text = ron::to_string(&node).expect("serialize");
        assert!(text.contains("vocoder"), "tag missing: {text}");
        assert!(text.contains("sibilance:"), "macro missing: {text}");
        let back: NodeDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(node, back);

        // A payload that predates the parameters loads at the shipped defaults.
        let legacy: NodeDoc = ron::from_str(
            r#"(type:"effect", node_id:3, track:0, voice:(type:"builtin"),
                params:(kind:"vocoder"))"#,
        )
        .expect("a bare vocoder payload should parse");
        let NodeDoc::Effect { params, .. } = legacy else {
            panic!("expected an effect node");
        };
        let voc = params.as_vocoder().expect("vocoder payload");
        assert_eq!(*voc, super::VocoderParams::default());
        assert!((voc.high - 7_000.0).abs() < 1e-3);
        assert!((voc.mix - 1.0).abs() < 1e-6, "fully wet by default");
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

    /// B2: `muted` is an additive, optional field. An unmuted clip writes no
    /// key at all (older documents and older readers are untouched), a muted
    /// one round-trips — for all three clip tables.
    #[test]
    fn clip_mute_is_optional_and_round_trips() {
        let plain = MidiClip::default();
        let text = ron::ser::to_string(&plain).expect("serialize");
        assert!(
            !text.contains("muted"),
            "an unmuted clip must not write the key: {text}"
        );
        let back: MidiClip = ron::from_str(&text).expect("deserialize");
        assert!(!back.muted, "a missing key means unmuted");

        let muted = MidiClip {
            muted: true,
            ..MidiClip::default()
        };
        let text = ron::ser::to_string(&muted).expect("serialize");
        assert!(text.contains("muted:true"), "mute must persist: {text}");
        let back: MidiClip = ron::from_str(&text).expect("deserialize");
        assert!(back.muted);

        for text in [
            ron::ser::to_string(&ControlClip {
                muted: true,
                ..ControlClip::default()
            })
            .expect("serialize"),
            ron::ser::to_string(&AudioClip {
                muted: true,
                ..AudioClip::default()
            })
            .expect("serialize"),
        ] {
            assert!(text.contains("muted:true"), "mute must persist: {text}");
        }
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
            name: "Intro".to_string(),
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
        // W3a: clips written before the name field read back as unnamed.
        assert!(back.name.is_empty());
    }

    /// W3b: clip groups are a flat table + a per-clip tag; both round-trip,
    /// and the validator rejects id 0, duplicates and an oversized table.
    #[test]
    fn clip_groups_roundtrip_and_validate() {
        let mut project = Project::default();
        project.clip_groups.push(super::ClipGroup {
            id: 1,
            name: "Verse".to_string(),
            color: [10, 20, 30],
        });
        project.midi_tracks[0].clips.push(MidiClip {
            group: Some(1),
            ..MidiClip::default()
        });
        let text = ron::to_string(&project).expect("serialize");
        let back: Project = ron::from_str(&text).expect("deserialize");
        assert_eq!(back.clip_groups, project.clip_groups);
        assert_eq!(back.midi_tracks[0].clips[0].group, Some(1));
        assert!(back.validate().is_ok());

        let duplicate = Project {
            clip_groups: vec![super::ClipGroup::default(), super::ClipGroup::default()],
            ..Project::default()
        };
        assert!(
            duplicate.validate().is_err(),
            "duplicate ids must be rejected"
        );
        let zero = Project {
            clip_groups: vec![super::ClipGroup {
                id: 0,
                ..super::ClipGroup::default()
            }],
            ..Project::default()
        };
        assert!(zero.validate().is_err(), "id 0 is reserved");
        let oversized = Project {
            clip_groups: vec![super::ClipGroup::default(); super::MAX_CLIP_GROUPS + 1],
            ..Project::default()
        };
        assert!(oversized.validate().is_err(), "the table is capped");
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

        // Pre-U6 project files have no `audio_tracks` field: stripping it from a
        // serialized file mimics one, and it loads as empty.
        let mut legacy_project = Project::default();
        legacy_project.audio_tracks.clear();
        let legacy = ron::to_string(&legacy_project)
            .expect("serialize")
            .replace("audio_tracks:[],", "");
        assert!(!legacy.contains("audio_tracks"));
        let parsed: Project = ron::from_str(&legacy).expect("legacy file must parse");
        assert!(parsed.audio_tracks.is_empty());
        parsed.validate().expect("legacy shape is valid");
    }

    /// R6: the graph only knows the universal audio cap; the project rejects a
    /// recording input that no actual audio track backs.
    #[test]
    fn an_audio_recording_input_must_name_a_real_track() {
        let mut project = Project::default();
        let from = project.graph.nodes.first().map(NodeDoc::node_id).unwrap();
        project.graph.cables.push(CableDoc {
            kind: CableKind::Audio,
            from: PortRef::Node(from),
            to: PortRef::TrackIn {
                kind: TrackKind::Audio,
                track: 0,
            },
            param: AutoParam::default(),
            in_port: 0,
            map: None,
        });
        project
            .validate()
            .expect("one audio track exists by default");
        project.audio_tracks.clear();
        assert!(
            project.validate().is_err(),
            "no audio track may be armed by a stale recording input"
        );
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
                    name: String::new(),
                    group: None,
                    muted: false,
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

    /// D13/D18 (revised 2026-10-29): the default document fully describes a
    /// minimal template — one playable MIDI track (instrument → mixer.ch0),
    /// one global mixer with a single channel → master, the keyboard input,
    /// and one unwired generator node for the audio lane.
    #[test]
    fn topology_eq_ignores_view_only_changes() {
        let base = Project::default();
        let mut renamed = base.clone();
        renamed.midi_tracks[0].name = "Renamed".to_string();
        renamed.control_tracks[0].name = "Curve".to_string();
        assert!(
            base.topology_eq(&renamed),
            "names do not change the real-time graph"
        );
    }

    #[test]
    fn topology_eq_rejects_node_and_cable_changes() {
        let base = Project::default();

        let mut node_changed = base.clone();
        for node in &mut node_changed.graph.nodes {
            if let NodeDoc::Instrument { track, .. } = node {
                *track += 1;
                break;
            }
        }
        assert!(!base.topology_eq(&node_changed), "node payloads matter");

        let mut cable_changed = base.clone();
        cable_changed.graph.cables[0].in_port =
            cable_changed.graph.cables[0].in_port.wrapping_add(1);
        assert!(!base.topology_eq(&cable_changed), "cable ports matter");
    }

    #[test]
    fn topology_eq_rejects_track_count_changes() {
        let base = Project::default();
        let mut extra = base.clone();
        let track = extra.control_tracks[0].clone();
        extra.control_tracks.push(track);
        assert!(!base.topology_eq(&extra), "track lists bound the engine");
    }

    #[test]
    fn engine_topology_eq_ignores_replayable_parameters() {
        let base = Project::default();
        let mut changed = base.clone();
        for node in &mut changed.graph.nodes {
            if let NodeDoc::Instrument { synth, .. } = node {
                synth.cutoff_hz += 100.0;
                break;
            }
        }
        assert!(
            base.engine_topology_eq(&changed),
            "synth values replay through ParamId"
        );
        assert!(
            !base.topology_eq(&changed),
            "the strict predicate still sees the payload"
        );
    }

    #[test]
    fn engine_topology_eq_rejects_voice_and_mixer_shape_changes() {
        let base = Project::default();
        let mut voice = base.clone();
        for node in &mut voice.graph.nodes {
            if let NodeDoc::Instrument { voice, .. } = node {
                *voice = VoiceSpec::SoundFont {
                    path: "a.sf2".to_string(),
                    bank: 0,
                    program: 0,
                    gain: 1.0,
                    bend_range: 2.0,
                };
                break;
            }
        }
        assert!(voice.engine_topology_eq(&voice));

        let mut other_voice = voice.clone();
        for node in &mut other_voice.graph.nodes {
            if let NodeDoc::Instrument { voice, .. } = node {
                if let VoiceSpec::SoundFont { path, .. } = voice {
                    *path = "b.sf2".to_string();
                }
                break;
            }
        }
        assert!(
            !voice.engine_topology_eq(&other_voice),
            "another font needs a new processor"
        );

        let mut wider_mixer = base.clone();
        for node in &mut wider_mixer.graph.nodes {
            if let NodeDoc::Mixer { inputs, .. } = node {
                *inputs += 1;
                break;
            }
        }
        assert!(
            !base.engine_topology_eq(&wider_mixer),
            "channel count decides the ports"
        );
    }

    #[test]
    fn default_project_graph_is_fully_explicit() {
        let project = Project::default();
        assert_eq!(project.midi_tracks.len(), DEFAULT_MIDI_TRACKS);
        assert_eq!(project.control_tracks.len(), DEFAULT_CONTROL_TRACKS);
        assert_eq!(project.audio_tracks.len(), DEFAULT_AUDIO_TRACKS);
        let graph = &project.graph;
        assert_eq!(
            graph.nodes.len(),
            4,
            "instrument + mixer + keyboard + audio-track node"
        );
        // Cables: midi self-connection + instrument→mixer + mixer→master +
        // keyboard→instrument.
        assert_eq!(graph.cables.len(), 4);
        let voice = graph
            .instrument_of_track(0)
            .expect("track 0 keeps a playable instrument");
        assert_eq!(voice.node_id(), MASTER_NODE_ID + 1);
        assert!(
            graph.builtin_delay_of_track(0).is_none(),
            "the default template no longer ships a delay node"
        );
        let landing = graph
            .cables
            .iter()
            .find(|c| c.from == PortRef::Node(voice.node_id()))
            .expect("instrument should feed the mixer");
        assert_eq!(landing.in_port, 0);
        let mixer = graph.first_mixer().expect("global mixer should exist");
        assert!(matches!(mixer, NodeDoc::Mixer { inputs: 1, .. }));
        assert!(
            graph
                .nodes
                .iter()
                .any(|n| matches!(n, NodeDoc::AudioTrack { track: 0, .. })),
            "the audio lane has its generator node (unwired by default)"
        );
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
        let graph = default_graph_doc(2, 0);
        // ids: track0 voice=2, mixer=3, computer keyboard=4. Track 1 starts
        // bare (the user builds it), so it owns no node.
        assert_eq!(graph.next_node_id(), 5);
        assert_eq!(
            graph.instrument_of_track(0).map(NodeDoc::node_id),
            Some(MASTER_NODE_ID + 1)
        );
        assert!(graph.instrument_of_track(1).is_none());
        assert_eq!(graph.auto_endpoint(0), None);
    }

    /// Spec `plugin-gui.md` §8: a fresh project owns one computer-keyboard
    /// node already cabled to the first track's instrument — the computer
    /// keyboard plays out of the box, but through a visible route instead of
    /// an implicit track fallback.
    #[test]
    fn default_project_ships_one_routed_computer_keyboard() {
        let graph = default_graph_doc(DEFAULT_MIDI_TRACKS, 0);
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
                gain: 1.0,
                bend_range: 2.0,
            },
            VoiceSpec::Sample {
                path: "kick.wav".to_string(),
                root_note: 60,
                play_mode: SamplePlayMode::Gate,
                gain: 1.0,
                region: SampleRegion::default(),
            },
            VoiceSpec::Sample {
                path: "hat.wav".to_string(),
                root_note: 48,
                play_mode: SamplePlayMode::OneShot,
                gain: 0.5,
                region: SampleRegion::default(),
            },
            VoiceSpec::DrumKit {
                slots: Vec::new(),
                gain: 1.0,
            },
            VoiceSpec::DrumKit {
                slots: vec![
                    super::DrumSlot {
                        note: 36,
                        path: "kick.wav".to_string(),
                        gain: 1.0,
                        pan: 0.0,
                        tune_semitones: 0.0,
                        choke: None,
                    },
                    super::DrumSlot {
                        note: 42,
                        path: "hat.wav".to_string(),
                        gain: 0.8,
                        pan: -0.3,
                        tune_semitones: 2.0,
                        choke: Some(1),
                    },
                ],
                gain: 0.9,
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
            gain: 1.0,
            bend_range: 2.0,
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
                program: 0,
                gain: 1.0,
                bend_range: 2.0,
            }
        );
    }

    /// Spec `instrument.md` D5: `waveform` is an additive `SynthParams`
    /// field — older files load as Sine and `PROJECT_VERSION` does not move.
    #[test]
    fn synth_params_waveform_serde_compatibility() {
        let synth = SynthParams {
            waveform: SynthWaveform::Saw,
            ..Default::default()
        };
        let text = ron::to_string(&synth).expect("serialization failed");
        assert!(
            text.contains("Saw"),
            "the waveform must serialize by name: {text}"
        );
        let round: SynthParams = ron::from_str(&text).expect("roundtrip failed");
        assert_eq!(round, synth);

        // A payload written before the field existed: everything else keeps
        // its documented default, the waveform falls back to Sine.
        let legacy: SynthParams = ron::from_str(
            "(cutoff_hz: 12000.0, resonance: 0.2, filter_mode: 0.0, \
             attack_s: 0.01, decay_s: 0.1, sustain: 0.8, release_s: 0.2)",
        )
        .expect("legacy payload must load");
        assert_eq!(legacy, SynthParams::default());
        assert_eq!(legacy.waveform, SynthWaveform::Sine);
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
            region: SampleRegion::default(),
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
                gain: 1.0,
                region: SampleRegion::default()
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
    fn validate_accepts_a_sorted_meter_change_table() {
        let mut project = Project::default();
        project.metadata.meter_markers = vec![
            super::MeterMarker {
                tick: TICKS_PER_BAR * 4,
                signature: super::TimeSignature::new(6, 8),
            },
            super::MeterMarker {
                tick: TICKS_PER_BAR * 8,
                signature: super::TimeSignature::new(3, 4),
            },
        ];
        project
            .validate()
            .expect("a sorted meter table must validate");
        assert_eq!(project.ticks_per_bar_at(TICKS_PER_BAR * 2), TICKS_PER_BAR);
        assert_eq!(project.ticks_per_bar_at(TICKS_PER_BAR * 4), 2880);
        assert_eq!(project.bar_beat_at(TICKS_PER_BAR * 4), (4, 0));
    }

    #[test]
    fn validate_rejects_a_meter_change_at_tick_zero() {
        let mut project = Project::default();
        project.metadata.meter_markers = vec![super::MeterMarker {
            tick: 0,
            signature: super::TimeSignature::new(3, 4),
        }];
        let err = project.validate().expect_err("tick 0 is the base meter's");
        assert!(err.contains("tick 0"), "unexpected error: {err}");
    }

    #[test]
    fn validate_rejects_unsorted_or_duplicate_meter_changes() {
        let mut project = Project::default();
        project.metadata.meter_markers = vec![
            super::MeterMarker {
                tick: TICKS_PER_BAR * 4,
                signature: super::TimeSignature::new(3, 4),
            },
            super::MeterMarker {
                tick: TICKS_PER_BAR * 4,
                signature: super::TimeSignature::new(6, 8),
            },
        ];
        let err = project.validate().expect_err("duplicates must fail");
        assert!(err.contains("strictly after"), "unexpected error: {err}");
    }

    #[test]
    fn validate_rejects_too_many_meter_changes() {
        let mut project = Project::default();
        project.metadata.meter_markers = (1..=super::MAX_METER_MARKERS + 1)
            .map(|i| super::MeterMarker {
                tick: u32::try_from(i).expect("small index") * TICKS_PER_BAR,
                signature: super::TimeSignature::new(3, 4),
            })
            .collect();
        let err = project.validate().expect_err("should reject");
        assert!(
            err.contains("meter change count"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_rejects_an_invalid_meter_change_signature() {
        let mut project = Project::default();
        project.metadata.meter_markers = vec![super::MeterMarker {
            tick: TICKS_PER_BAR,
            signature: super::TimeSignature {
                numerator: 0,
                denominator: 4,
            },
        }];
        let err = project.validate().expect_err("0 beats per bar is invalid");
        assert!(
            err.contains("invalid time signature"),
            "unexpected error: {err}"
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
    fn validate_accepts_a_dense_clip() {
        // D15 revision regression: 300 notes in one clip (a real imported
        // piano part) used to fail validation at save time.
        let mut project = Project::default();
        let mut clip = MidiClip::default();
        for i in 0..300 {
            clip.notes_mut().push(super::Note {
                start: (i as i64) * i64::from(TICKS_PER_STEP),
                length: i64::from(TICKS_PER_STEP),
                ..super::Note::default()
            });
        }
        project.midi_tracks[0].clips.push(clip);
        project.validate().expect("a dense clip must validate");
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

                    transpose: 0,
                    octave: DEFAULT_KEYBOARD_OCTAVE,
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

            transpose: 0,
            octave: DEFAULT_KEYBOARD_OCTAVE,
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

                transpose: 0,
                octave: DEFAULT_KEYBOARD_OCTAVE,
            }
        );
    }

    /// I1: the register is a document field — it survives a reload, and a
    /// document written before the field existed lands on the default window.
    #[test]
    fn the_keyboard_register_survives_a_reload() {
        let text = ron::to_string(&NodeDoc::Keyboard {
            node_id: 6,
            track: 0,
            source: MidiSource::Computer,
            transpose: 0,
            octave: 6,
        })
        .expect("serialization failed");
        let back: NodeDoc = ron::from_str(&text).expect("deserialization failed");
        assert_eq!(
            back,
            NodeDoc::Keyboard {
                node_id: 6,
                track: 0,
                source: MidiSource::Computer,
                transpose: 0,
                octave: 6,
            }
        );

        // An old payload (no `octave` field) opens on the default window.
        let old = r#"(type:"keyboard", node_id:6, track:0, source:computer, transpose:0)"#;
        let back: NodeDoc = ron::from_str(old).expect("deserialization failed");
        assert_eq!(
            back,
            NodeDoc::Keyboard {
                node_id: 6,
                track: 0,
                source: MidiSource::Computer,
                transpose: 0,
                octave: DEFAULT_KEYBOARD_OCTAVE,
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

            transpose: 0,
            octave: DEFAULT_KEYBOARD_OCTAVE,
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
    /// W1: drum-kit integrity — duplicate trigger keys and the slot cap.
    #[test]
    fn validate_rejects_duplicate_or_oversized_drum_kits() {
        fn set_drum_voice(doc: &mut GraphDoc, node_id: u64, slots: Vec<super::DrumSlot>) {
            if let Some(NodeDoc::Instrument { voice, .. }) =
                doc.nodes.iter_mut().find(|node| node.node_id() == node_id)
            {
                *voice = VoiceSpec::DrumKit { slots, gain: 1.0 };
            }
        }
        fn drum_slot(note: u8) -> super::DrumSlot {
            super::DrumSlot {
                note,
                ..super::DrumSlot::default()
            }
        }

        let mut doc = vk_doc();
        set_drum_voice(&mut doc, 2, vec![drum_slot(36), drum_slot(36)]);
        let err = doc
            .validate(2, 0)
            .expect_err("duplicate trigger keys must be rejected");
        assert!(err.contains("twice"), "unexpected error: {err}");

        let mut doc = vk_doc();
        let slots: Vec<super::DrumSlot> = (0..super::MAX_DRUM_SLOTS as u8 + 1)
            .map(drum_slot)
            .collect();
        set_drum_voice(&mut doc, 2, slots);
        let err = doc
            .validate(2, 0)
            .expect_err("an oversized kit must be rejected");
        assert!(err.contains("slots"), "unexpected error: {err}");

        let mut doc = vk_doc();
        set_drum_voice(&mut doc, 2, vec![drum_slot(36), drum_slot(38)]);
        assert!(doc.validate(2, 0).is_ok(), "two distinct keys are legal");
    }

    #[test]
    fn validate_rejects_two_nodes_bound_to_the_same_source() {
        let mut doc = vk_doc();
        doc.nodes.push(NodeDoc::Keyboard {
            node_id: 8,
            track: 1,
            source: MidiSource::Computer,

            transpose: 0,
            octave: DEFAULT_KEYBOARD_OCTAVE,
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

            transpose: 0,
            octave: DEFAULT_KEYBOARD_OCTAVE,
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

    /// R6 (spec `recording.md` §5 D21-D24): one recording source per track
    /// input, type matched, track in range.
    #[test]
    fn track_input_cables_follow_the_recording_rules() {
        fn track_in(
            kind: CableKind,
            from: PortRef,
            track_kind: TrackKind,
            track: usize,
        ) -> CableDoc {
            CableDoc {
                kind,
                from,
                to: PortRef::TrackIn {
                    kind: track_kind,
                    track,
                },
                param: AutoParam::default(),
                in_port: 0,
                map: None,
            }
        }

        // Keyboard → MIDI track input is the R6 path.
        let mut doc = vk_doc();
        doc.cables.push(track_in(
            CableKind::Midi,
            PortRef::Node(6),
            TrackKind::Midi,
            1,
        ));
        assert!(
            doc.validate(2, 0).is_ok(),
            "keyboard → MIDI recording input"
        );

        // One input, one source (D23): the same cable twice is still two
        // cables into the port.
        let mut doc = vk_doc();
        doc.cables.push(track_in(
            CableKind::Midi,
            PortRef::Node(6),
            TrackKind::Midi,
            1,
        ));
        doc.cables.push(track_in(
            CableKind::Midi,
            PortRef::Node(6),
            TrackKind::Midi,
            1,
        ));
        assert!(doc.validate(2, 0).is_err(), "one source per input");

        // Port type must match the track family (D24).
        let mut doc = vk_doc();
        doc.cables.push(track_in(
            CableKind::Audio,
            PortRef::Node(2),
            TrackKind::Midi,
            1,
        ));
        assert!(doc.validate(2, 0).is_err(), "audio data on a MIDI input");

        // Track range follows the family count.
        let mut doc = vk_doc();
        doc.cables.push(track_in(
            CableKind::Midi,
            PortRef::Node(6),
            TrackKind::Midi,
            5,
        ));
        assert!(doc.validate(2, 0).is_err(), "MIDI track out of range");

        // An instrument's audio output can arm an audio input.
        let mut doc = vk_doc();
        doc.cables.push(track_in(
            CableKind::Audio,
            PortRef::Node(2),
            TrackKind::Audio,
            0,
        ));
        assert!(
            doc.validate(2, 0).is_ok(),
            "instrument → audio recording input"
        );

        // A curve input takes the timeline curve outlet.
        let mut doc = vk_doc();
        doc.cables.push(track_in(
            CableKind::Automation,
            PortRef::AutoTrack(0),
            TrackKind::Curve,
            0,
        ));
        assert!(
            doc.validate(2, 1).is_ok(),
            "curve outlet → curve recording input"
        );

        // A keyboard is not a control source.
        let mut doc = vk_doc();
        doc.cables.push(track_in(
            CableKind::Automation,
            PortRef::Node(6),
            TrackKind::Curve,
            0,
        ));
        assert!(doc.validate(2, 1).is_err(), "keyboard has no control out");
    }

    /// R4: the live input node is mono/stereo and its gain stays 0..2.
    #[test]
    fn input_node_validation_bounds_channel_and_gain() {
        let mut doc = vk_doc();
        doc.nodes.push(NodeDoc::Input {
            node_id: 8,
            channel: 0,
            gain: 1.0,
        });
        assert!(doc.validate(2, 0).is_ok());
        doc.nodes.pop();
        doc.nodes.push(NodeDoc::Input {
            node_id: 8,
            channel: 2,
            gain: 1.0,
        });
        assert!(doc.validate(2, 0).is_err(), "channel 2 is out of range");
        doc.nodes.pop();
        doc.nodes.push(NodeDoc::Input {
            node_id: 8,
            channel: 0,
            gain: 3.0,
        });
        assert!(doc.validate(2, 0).is_err(), "gain above 2 is rejected");
    }

    /// R4 D11: relative media paths resolve against the project directory and
    /// are stored relative to it.
    #[test]
    fn relative_media_paths_resolve_against_the_project_directory() {
        let mut project = Project {
            base_dir: Some(std::path::PathBuf::from("songs")),
            ..Project::default()
        };
        assert_eq!(
            project.resolve_media_path("demo.media/rec-1.wav"),
            std::path::PathBuf::from("songs").join("demo.media/rec-1.wav")
        );
        // The absolute sample has to be absolute *for the host platform*: a
        // `C:/...` drive path is an ordinary relative path on unix, so a
        // hardcoded one only proves the rule on Windows. The temp directory is
        // absolute everywhere.
        let outside = std::env::temp_dir().join("glitcherz-take.wav");
        assert_eq!(
            project.resolve_media_path(&outside.to_string_lossy()),
            outside,
            "absolute paths are untouched"
        );
        assert_eq!(
            project.media_path_string(std::path::Path::new("songs/demo.media/rec-1.wav")),
            "demo.media/rec-1.wav"
        );
        // S3b: separators are stored as `/` whatever the host uses, and a `\`
        // written by an older Windows build still resolves.
        assert_eq!(
            project.media_path_string(
                &std::path::Path::new("songs")
                    .join("demo.media")
                    .join("x.wav")
            ),
            "demo.media/x.wav"
        );
        assert_eq!(
            project.resolve_media_path("demo.media\\x.wav"),
            std::path::PathBuf::from("songs")
                .join("demo.media")
                .join("x.wav")
        );
        project.base_dir = None;
        assert_eq!(
            project.resolve_media_path("demo.media/x.wav"),
            std::path::PathBuf::from("demo.media/x.wav")
        );
    }

    /// S3b: every media reference is enumerated once, rewritten by map, and a
    /// path that does not resolve to a file is reported as missing.
    #[test]
    fn media_references_are_enumerated_rewritten_and_checked() {
        let dir = std::env::temp_dir().join(format!("glitcherz_media_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("demo.media")).expect("temp dir");
        let present = dir.join("demo.media").join("kick.wav");
        std::fs::write(&present, b"not really a wav").expect("write");

        let mut project = Project {
            base_dir: Some(dir.clone()),
            ..Project::default()
        };
        // One audio clip with media, one without (an empty clip is not a
        // reference), plus a missing sample and a drum slot sharing the
        // present file.
        project.audio_tracks = vec![crate::AudioTrack {
            clips: vec![
                crate::AudioClip {
                    path: "demo.media/kick.wav".to_string(),
                    source_len: 1.0,
                    ..crate::AudioClip::default()
                },
                crate::AudioClip::default(),
            ],
            ..crate::AudioTrack::default()
        }];
        let sample_id = project.graph.first_mixer().unwrap().node_id() + 1;
        for (node_id, path) in [
            (sample_id, "demo.media/kick.wav"),
            (sample_id + 1, "gone/snare.wav"),
        ] {
            project.graph.nodes.push(NodeDoc::Instrument {
                node_id,
                track: 0,
                voice: VoiceSpec::Sample {
                    path: path.to_string(),
                    root_note: 60,
                    play_mode: SamplePlayMode::Gate,
                    gain: 1.0,
                    region: SampleRegion::default(),
                },
                synth: SynthParams::default(),
            });
        }

        let mut paths = project.media_paths();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "demo.media/kick.wav".to_string(),
                "gone/snare.wav".to_string()
            ],
            "duplicates collapse, empty paths are skipped"
        );
        assert_eq!(project.missing_media(), vec!["gone/snare.wav".to_string()]);

        let mut map = std::collections::HashMap::new();
        map.insert(
            "gone/snare.wav".to_string(),
            "demo.media/snare.wav".to_string(),
        );
        project.rewrite_media_paths(&map);
        assert_eq!(project.media_paths().len(), 2);
        assert_eq!(project.media_paths()[1], "demo.media/snare.wav");
        assert_eq!(
            project.missing_media(),
            vec!["demo.media/snare.wav".to_string()],
            "the rewritten path is missing until the file is copied"
        );

        project.base_dir = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// U3b (spec `tempo-meter.md` D1/D2/D7): the Timeline In terminal accepts
    /// exactly one thing — an automation cable carrying `Tempo`. Everything
    /// else is rejected by `validate`, and no extra singleton rule is needed
    /// (the identity is not instantiable).
    #[test]
    fn timeline_in_accepts_only_tempo_automation() {
        let mut doc = default_graph_doc(2, 0);
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
        let mut doc = default_graph_doc(2, 0);
        doc.cables.push(CableDoc::audio(2, PortRef::TimelineIn));
        assert!(
            doc.validate(2, 0).is_err(),
            "audio cannot enter the terminal"
        );

        // Neither do MIDI cables.
        let mut doc = default_graph_doc(2, 0);
        doc.cables.push(CableDoc {
            kind: CableKind::Midi,
            from: PortRef::MidiTrack(0),
            to: PortRef::TimelineIn,
            param: AutoParam::default(),
            in_port: 0,
            map: None,
        });
        assert!(
            doc.validate(2, 0).is_err(),
            "MIDI cannot enter the terminal"
        );

        // Tempo is a clock, not a node parameter: it must not reach a node.
        let mut doc = default_graph_doc(2, 0);
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
        let mut doc = default_graph_doc(2, 0);
        doc.cables.push(CableDoc {
            kind: CableKind::Automation,
            from: PortRef::AutoTrack(0),
            to: PortRef::TimelineIn,
            param: AutoParam::TrackGain,
            in_port: 0,
            map: None,
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
        let mut doc = default_graph_doc(2, 0);
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

        let mut doc = default_graph_doc(2, 0);
        doc.cables.push(CableDoc {
            kind: CableKind::Automation,
            from: PortRef::AutoTrack(0),
            to: PortRef::TimelineIn,
            param: AutoParam::cc(74, 0),
            in_port: 0,
            map: None,
        });
        assert!(
            doc.validate(2, 1).is_err(),
            "the terminal's only v1 port is Tempo"
        );
    }

    /// U4c (spec `control-tracks.md` §9 D20): a mixer channel address is the
    /// one node parameter the document can invalidate (shrinking the mixer),
    /// so validation is strict about it — and only about it.
    #[test]
    fn mixer_channel_automation_is_range_checked() {
        let mut doc = default_graph_doc(3, 0);
        let mixer = doc.first_mixer().expect("global mixer").node_id();
        // Grow the mixer explicitly: the default template ships one channel.
        if let Some(NodeDoc::Mixer {
            inputs, channels, ..
        }) = doc.node_by_id_mut(mixer)
        {
            *inputs = 3;
            *channels = vec![MixerChannel::default(); 3];
        }
        let channel = |channel: u8| {
            AutoParam::Node(NodeParam::MixerChannel {
                channel,
                param: ChannelParam::Gain,
            })
        };
        doc.cables.push(CableDoc::automation(0, mixer, channel(2)));
        assert!(doc.validate(3, 1).is_ok(), "channel 2 of a 3-in mixer");

        // Past the last channel: the engine would silently write an unused
        // slot, so the document must refuse it.
        doc.cables.last_mut().unwrap().param = channel(3);
        assert!(doc.validate(3, 1).is_err());

        // Same address on a node that is not a mixer.
        let voice = doc.instrument_of_track(0).unwrap().node_id();
        doc.cables.last_mut().unwrap().to = PortRef::Node(voice);
        assert!(doc.validate(3, 1).is_err());

        // Output-stage params are mixer-only too.
        doc.cables.last_mut().unwrap().param = AutoParam::Node(NodeParam::MixerOutPan);
        assert!(doc.validate(3, 1).is_err());
        doc.cables.last_mut().unwrap().to = PortRef::Node(mixer);
        assert!(doc.validate(3, 1).is_ok());

        // Voice-dependent parameters are deliberately **not** checked against
        // the node kind: swapping a node's voice must not make a project
        // unsavable (the engine ignores a parameter it does not recognize).
        doc.cables.last_mut().unwrap().to = PortRef::Node(voice);
        doc.cables.last_mut().unwrap().param = AutoParam::Node(NodeParam::Sf2Gain);
        assert!(doc.validate(3, 1).is_ok());
    }

    /// U4c: the nested `NodeParam` payload has to survive the RON round trip —
    /// the whole point of putting the address in the document.
    #[test]
    fn node_param_cable_roundtrips_through_ron() {
        for param in [
            AutoParam::Node(NodeParam::EffectSlot(3)),
            AutoParam::Node(NodeParam::MixerChannel {
                channel: 1,
                param: ChannelParam::Solo,
            }),
            AutoParam::Node(NodeParam::SampleGain),
        ] {
            let cable = CableDoc::automation(2, 9, param);
            let text = ron::to_string(&cable).expect("serialize");
            let back: CableDoc = ron::from_str(&text).expect("deserialize");
            assert_eq!(back, cable, "round trip must be lossless: {text}");
        }
    }

    // ---- M2: MIDI event nodes (spec `midi-nodes.md`) ----

    fn arp_node(id: u64) -> NodeDoc {
        NodeDoc::Arpeggiator {
            node_id: id,
            track: 0,
            params: ArpeggiatorParams::default(),
        }
    }

    fn glide_node(id: u64) -> NodeDoc {
        NodeDoc::Portamento {
            node_id: id,
            track: 0,
            params: PortamentoParams::default(),
        }
    }

    fn env_node(id: u64) -> NodeDoc {
        NodeDoc::MidiEnvelope {
            node_id: id,
            track: 0,
            params: MidiEnvelopeParams::default(),
        }
    }

    fn curve_map_node(id: u64) -> NodeDoc {
        NodeDoc::CurveMap {
            node_id: id,
            track: 0,
            params: CurveMapParams::default(),
        }
    }

    fn mixer_node(id: u64) -> NodeDoc {
        NodeDoc::Mixer {
            node_id: id,
            track: 0,
            inputs: 1,
            channels: vec![MixerChannel::default()],
            out_pan: 0.0,
            out_muted: false,
        }
    }

    /// Node-to-node MIDI cable (any source with a MIDI out).
    fn node_midi(from: u64, to: u64) -> CableDoc {
        CableDoc {
            kind: CableKind::Midi,
            from: PortRef::Node(from),
            to: PortRef::Node(to),
            param: AutoParam::default(),
            in_port: 0,
            map: None,
        }
    }

    /// Node-to-node control cable.
    fn control_cable(from: u64, to: u64, param: AutoParam) -> CableDoc {
        CableDoc {
            kind: CableKind::Automation,
            from: PortRef::Node(from),
            to: PortRef::Node(to),
            param,
            in_port: 0,
            map: None,
        }
    }

    /// instrument(2) + instrument(3) + arp(4) + portamento(5) + envelope(6)
    /// + mixer(7), no cables.
    fn midi_node_doc() -> GraphDoc {
        GraphDoc {
            nodes: vec![
                NodeDoc::Instrument {
                    node_id: 2,
                    track: 0,
                    voice: VoiceSpec::Builtin,
                    synth: SynthParams::default(),
                },
                NodeDoc::Instrument {
                    node_id: 3,
                    track: 1,
                    voice: VoiceSpec::Builtin,
                    synth: SynthParams::default(),
                },
                arp_node(4),
                glide_node(5),
                env_node(6),
                mixer_node(7),
            ],
            cables: Vec::new(),
            layout: Default::default(),
        }
    }

    #[test]
    fn midi_event_nodes_round_trip_through_ron() {
        for (node, tag) in [
            (arp_node(4), "arpeggiator"),
            (glide_node(5), "portamento"),
            (env_node(6), "midi_envelope"),
        ] {
            let text = ron::to_string(&node).expect("serialize");
            assert!(
                text.contains(&format!("type:\"{tag}\"")),
                "tag missing: {text}"
            );
            let back: NodeDoc = ron::from_str(&text).expect("deserialize");
            assert_eq!(node, back);
        }
        // Non-default parameter payload survives too.
        let node = NodeDoc::Arpeggiator {
            node_id: 4,
            track: 1,
            params: ArpeggiatorParams {
                division: 2,
                gate: 0.75,
                mode: ArpMode::UpDown,
                octaves: 3,
                latch: true,
                ..ArpeggiatorParams::default()
            },
        };
        let back: NodeDoc = ron::from_str(&ron::to_string(&node).unwrap()).unwrap();
        assert_eq!(node, back);
    }

    #[test]
    fn midi_node_cable_matrix_accepts_legal_chains() {
        let mut doc = midi_node_doc();
        // Timeline → arp → portamento → instrument (full MIDI effect chain).
        doc.cables.push(CableDoc::midi(0, 4));
        doc.cables.push(node_midi(4, 5));
        doc.cables.push(node_midi(5, 2));
        // Envelope driven by the same chain's keyboard-side input is a
        // separate legal edge: arp → envelope (it has a MIDI in).
        doc.cables.push(node_midi(4, 6));
        // Control cables: envelope → instrument param, portamento → bend on
        // a second instrument (one control source per destination).
        doc.cables
            .push(control_cable(6, 2, AutoParam::Node(NodeParam::Resonance)));
        doc.cables.push(control_cable(
            5,
            3,
            AutoParam::PitchBend {
                channel: 0,
                range_semitones: 2.0,
            },
        ));
        assert!(doc.validate(1, 0).is_ok(), "legal chain must validate");
    }

    #[test]
    fn midi_node_cable_matrix_rejects_bad_endpoints() {
        // MIDI cable into a node without a MIDI input (mixer).
        let mut doc = midi_node_doc();
        doc.cables.push(node_midi(4, 7));
        assert!(doc.validate(1, 0).is_err(), "mixer has no MIDI in");

        // MIDI cable out of a node without a MIDI output (envelope).
        let mut doc = midi_node_doc();
        doc.cables.push(node_midi(6, 2));
        assert!(doc.validate(1, 0).is_err(), "envelope has no MIDI out");

        // Audio cables cannot touch MIDI-only nodes (either direction).
        let mut doc = midi_node_doc();
        doc.cables
            .push(CableDoc::audio(PortRef::Node(4), PortRef::Node(2)));
        assert!(doc.validate(1, 0).is_err(), "arp has no audio out");
        let mut doc = midi_node_doc();
        doc.cables
            .push(CableDoc::audio(PortRef::Node(2), PortRef::Node(6)));
        assert!(doc.validate(1, 0).is_err(), "envelope has no audio in");

        // Automation source without a control output (instrument).
        let mut doc = midi_node_doc();
        doc.cables
            .push(control_cable(2, 7, AutoParam::Node(NodeParam::MixerOutPan)));
        assert!(doc.validate(1, 0).is_err(), "instrument has no control out");

        // Automation into a node that does not accept control (arp).
        let mut doc = midi_node_doc();
        doc.cables
            .push(control_cable(6, 4, AutoParam::Node(NodeParam::Resonance)));
        assert!(doc.validate(1, 0).is_err(), "arp does not take control in");

        // Tempo from a node source (clocks only come from the timeline).
        let mut doc = midi_node_doc();
        doc.cables.push(control_cable(6, 2, AutoParam::Tempo));
        assert!(doc.validate(1, 0).is_err(), "tempo is timeline-only");
        let mut doc = midi_node_doc();
        doc.cables.push(CableDoc {
            kind: CableKind::Automation,
            from: PortRef::Node(6),
            to: PortRef::TimelineIn,
            param: AutoParam::Tempo,
            in_port: 0,
            map: None,
        });
        assert!(
            doc.validate(1, 0).is_err(),
            "the timeline input only accepts timeline automation tracks"
        );
    }

    #[test]
    fn midi_node_single_event_and_control_source_rules() {
        // A node fed by a MIDI-effect cable refuses a timeline feed.
        let mut doc = midi_node_doc();
        doc.cables.push(node_midi(4, 2));
        doc.cables.push(CableDoc::midi(0, 2));
        assert!(
            doc.validate(1, 0).is_err(),
            "timeline feed + event cable into one node"
        );

        // Two event cables into one destination.
        let mut doc = midi_node_doc();
        doc.cables.push(node_midi(4, 2));
        doc.cables.push(node_midi(5, 2));
        assert!(doc.validate(1, 0).is_err(), "two MIDI-effect cables");

        // Two control cables into one destination.
        let mut doc = midi_node_doc();
        doc.cables
            .push(control_cable(6, 2, AutoParam::Node(NodeParam::Resonance)));
        doc.cables.push(control_cable(
            5,
            2,
            AutoParam::PitchBend {
                channel: 0,
                range_semitones: 2.0,
            },
        ));
        assert!(doc.validate(1, 0).is_err(), "two control cables");
    }

    // ---- C2: Curve Map node (spec `curve-map.md` D5/D6) ----

    #[test]
    fn curve_map_round_trips_and_defaults_to_the_identity() {
        let node = curve_map_node(9);
        let text = ron::to_string(&node).expect("serialize");
        assert!(text.contains("type:\"curve_map\""), "tag missing: {text}");
        let back: NodeDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(node, back);
        // The defaults are the identity map: inserting the node is a no-op
        // until the user edits the range.
        assert_eq!(CurveMapParams::default().map(), MapSpec::UNIT);
        assert!(
            !NodeDoc::CurveMap {
                node_id: 9,
                track: 0,
                params: CurveMapParams::default(),
            }
            .has_audio_ports()
        );
    }

    #[test]
    fn curve_map_accepts_one_control_cable_from_any_source() {
        // Envelope → map → instrument resonance: the D6 chain.
        let mut doc = midi_node_doc();
        doc.nodes.push(curve_map_node(9));
        doc.cables.push(CableDoc::midi(0, 6));
        doc.cables.push(control_cable(6, 9, AutoParam::CurveMap));
        doc.cables
            .push(control_cable(9, 2, AutoParam::Node(NodeParam::Resonance)));
        assert!(doc.validate(1, 0).is_ok(), "one in, one out");

        // A timeline curve may drive the map's input too (it is a curve, not
        // a control source — D5 says either).
        let mut doc = midi_node_doc();
        doc.nodes.push(curve_map_node(9));
        doc.cables.push(CableDoc {
            kind: CableKind::Automation,
            from: PortRef::AutoTrack(0),
            to: PortRef::Node(9),
            param: AutoParam::CurveMap,
            in_port: 0,
            map: None,
        });
        doc.cables
            .push(control_cable(9, 2, AutoParam::Node(NodeParam::Resonance)));
        assert!(doc.validate(1, 1).is_ok(), "timeline curve into a map");
    }

    #[test]
    fn curve_map_rejects_a_second_input_and_foreign_ports() {
        // Two sources into one map: there is one pass-through register, so a
        // second writer would silently win.
        let mut doc = midi_node_doc();
        doc.nodes.push(curve_map_node(9));
        doc.cables.push(control_cable(6, 9, AutoParam::CurveMap));
        doc.cables.push(CableDoc {
            kind: CableKind::Automation,
            from: PortRef::AutoTrack(0),
            to: PortRef::Node(9),
            param: AutoParam::CurveMap,
            in_port: 0,
            map: None,
        });
        assert!(doc.validate(1, 1).is_err(), "one control input");

        // A real parameter on the map's input is a category error: the port
        // carries the signal, not a value the user picked.
        let mut doc = midi_node_doc();
        doc.nodes.push(curve_map_node(9));
        doc.cables
            .push(control_cable(6, 9, AutoParam::Node(NodeParam::Resonance)));
        assert!(doc.validate(1, 0).is_err(), "map input is not a parameter");

        // ... and the map port is meaningless on any other node.
        let mut doc = midi_node_doc();
        doc.cables.push(control_cable(6, 2, AutoParam::CurveMap));
        assert!(doc.validate(1, 0).is_err(), "curve port only on a map node");

        // No audio, no MIDI.
        let mut doc = midi_node_doc();
        doc.nodes.push(curve_map_node(9));
        doc.cables
            .push(CableDoc::audio(PortRef::Node(2), PortRef::Node(9)));
        assert!(doc.validate(1, 0).is_err(), "map has no audio in");
        let mut doc = midi_node_doc();
        doc.nodes.push(curve_map_node(9));
        doc.cables.push(node_midi(4, 9));
        assert!(doc.validate(1, 0).is_err(), "map has no MIDI in");
    }

    #[test]
    fn curve_map_params_repair_on_migrate() {
        let graph = GraphDoc {
            nodes: vec![
                NodeDoc::CurveMap {
                    node_id: 9,
                    track: 0,
                    params: CurveMapParams {
                        min: 500.0,
                        max: 20.0,
                        mode: MapMode::Log,
                    },
                },
                NodeDoc::CurveMap {
                    node_id: 10,
                    track: 0,
                    params: CurveMapParams {
                        min: f32::NAN,
                        max: 1.0,
                        mode: MapMode::Exp,
                    },
                },
            ],
            cables: Vec::new(),
            layout: Default::default(),
        };
        let mut project = Project {
            graph,
            ..Project::default()
        };
        project.migrate();
        let NodeDoc::CurveMap { params, .. } = &project.graph.nodes[0] else {
            panic!("curve map must survive");
        };
        // Inverted bounds collapse to the identity rather than a backwards axis.
        assert_eq!(*params, CurveMapParams::default());
        let NodeDoc::CurveMap { params, .. } = &project.graph.nodes[1] else {
            panic!("curve map must survive");
        };
        assert!(params.min.is_finite());
    }

    #[test]
    fn midi_node_params_clamp_on_migrate() {
        let graph = GraphDoc {
            nodes: vec![
                NodeDoc::Arpeggiator {
                    node_id: 4,
                    track: 0,
                    params: ArpeggiatorParams {
                        division: 99,
                        gate: f32::NAN,
                        mode: ArpMode::Random,
                        octaves: 9,
                        latch: true,
                        ..ArpeggiatorParams::default()
                    },
                },
                NodeDoc::Portamento {
                    node_id: 5,
                    track: 0,
                    params: PortamentoParams {
                        time_ms: f32::INFINITY,
                        min: 10.0,
                        max: -10.0,
                        legato_only: false,
                    },
                },
                NodeDoc::MidiEnvelope {
                    node_id: 6,
                    track: 0,
                    params: MidiEnvelopeParams {
                        attack: -1.0,
                        decay: f32::NAN,
                        sustain: 5.0,
                        release: 999.0,
                        min: 200.0,
                        max: -5.0,
                    },
                },
            ],
            cables: Vec::new(),
            layout: Default::default(),
        };
        let mut project = Project {
            graph,
            ..Project::default()
        };
        project.migrate();
        let NodeDoc::Arpeggiator { params, .. } = &project.graph.nodes[0] else {
            panic!("arp node must survive");
        };
        assert_eq!(params.division, super::ARP_DEFAULT_DIVISION);
        assert_eq!(params.gate, 0.5);
        assert_eq!(params.mode, ArpMode::Random);
        assert_eq!(params.octaves, 4);
        assert!(params.latch);

        let NodeDoc::Portamento { params, .. } = &project.graph.nodes[1] else {
            panic!("portamento node must survive");
        };
        // min > max degrades to the whole default rather than an empty scale.
        assert_eq!(*params, PortamentoParams::default());

        let NodeDoc::MidiEnvelope { params, .. } = &project.graph.nodes[2] else {
            panic!("envelope node must survive");
        };
        assert_eq!(params.attack, 0.0);
        assert_eq!(params.decay, 0.2);
        assert_eq!(params.sustain, 1.0);
        assert_eq!(params.release, 10.0);
        // min > max collapses onto min after the ±127 clamp.
        assert_eq!((params.min, params.max), (127.0, 127.0));
    }

    /// D11: the new node variants are purely additive — the version must not
    /// move (bumping it here means old files suddenly "need" a migration).
    /// v9 is the one exception, and it *does* carry a migration: the curve
    /// value domain changed (spec `curve-map.md` D7). v11 (Met M3) moves the
    /// number for the additive `meter_markers` field, without a migration.
    #[test]
    fn project_version_matches_the_curve_migration() {
        assert_eq!(super::PROJECT_VERSION, 11);
    }

    // ---- SF2 bend range (spec `curve-map.md` D8/D9) ----

    #[test]
    fn sf2_bend_range_round_trips_and_defaults_to_gm() {
        let voice = VoiceSpec::SoundFont {
            path: "piano.sf2".to_string(),
            bank: 1,
            program: 2,
            gain: 0.8,
            bend_range: 7.0,
        };
        let text = ron::to_string(&voice).expect("serialize");
        let back: VoiceSpec = ron::from_str(&text).expect("deserialize");
        assert_eq!(voice, back);

        // Files written before the field existed keep the GM ±2 default.
        let legacy: VoiceSpec =
            ron::from_str("(type:\"sound_font\",path:\"piano.sf2\",bank:0,program:0,gain:1.0)")
                .expect("legacy deserialize");
        assert_eq!(
            legacy,
            VoiceSpec::SoundFont {
                path: "piano.sf2".to_string(),
                bank: 0,
                program: 0,
                gain: 1.0,
                bend_range: 2.0,
            }
        );
    }

    #[test]
    fn sf2_bend_range_is_clamped_on_load() {
        for (stored, expected) in [(0.1_f32, 1.0), (99.0, 24.0), (f32::NAN, 2.0)] {
            let mut project = Project::default();
            project.graph.nodes.push(NodeDoc::Instrument {
                node_id: 99,
                track: 0,
                voice: VoiceSpec::SoundFont {
                    path: String::new(),
                    bank: 0,
                    program: 0,
                    gain: 1.0,
                    bend_range: stored,
                },
                synth: SynthParams::default(),
            });
            project.migrate();
            let Some(NodeDoc::Instrument {
                voice: VoiceSpec::SoundFont { bend_range, .. },
                ..
            }) = project.graph.nodes.last()
            else {
                panic!("sf2 node must survive");
            };
            assert_eq!(*bend_range, expected, "stored {stored}");
        }
    }

    // ---- Curve normalization protocol (spec `curve-map.md` C1) ----

    #[test]
    fn map_spec_hits_both_ends_and_the_centre() {
        let linear = MapSpec::linear(-24.0, 24.0);
        assert_eq!(linear.map(-1.0), -24.0);
        assert_eq!(linear.map(0.0), 0.0);
        assert_eq!(linear.map(1.0), 24.0);
        // Out-of-range input clamps to the ends instead of extrapolating.
        assert_eq!(linear.map(-9.0), -24.0);
        assert_eq!(linear.map(9.0), 24.0);

        let log = MapSpec::log(20.0, 18_000.0);
        assert!((log.map(-1.0) - 20.0).abs() < 1e-3);
        assert!((log.map(1.0) - 18_000.0).abs() < 1e-1);
        // Log means equal ratios per equal axis step: the centre is the
        // geometric mean, not the arithmetic one.
        assert!((log.map(0.0) - 600.0).abs() < 1.0);

        let exp = MapSpec {
            min: 0.0,
            max: 1.0,
            mode: MapMode::Exp,
        };
        assert!((exp.map(0.0) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn unmap_inverts_map() {
        for spec in [
            MapSpec::linear(0.0, 1.5),
            MapSpec::linear(-1.0, 1.0),
            MapSpec::log(20.0, 18_000.0),
            MapSpec::log(0.2, 12.0),
            MapSpec {
                min: -18.0,
                max: 18.0,
                mode: MapMode::Exp,
            },
        ] {
            for norm in [-1.0, -0.5, 0.0, 0.25, 1.0] {
                let back = spec.unmap(spec.map(norm));
                assert!((back - norm).abs() < 1e-4, "{spec:?} at {norm}: {back}");
            }
        }
    }

    #[test]
    fn lfo_params_default_round_trip_and_clamp() {
        let p = LfoParams::default();
        assert_eq!(p.waveform, LfoWaveform::Sine);
        assert!((p.rate_hz - 1.0).abs() < 1e-6);
        assert_eq!(p.division, LFO_DEFAULT_DIVISION);
        assert!((p.depth - 0.5).abs() < 1e-6);
        assert_eq!(LFO_DIVISION_LABELS.len(), LFO_DIVISION_COUNT);

        let bad = LfoParams {
            rate_hz: f32::NAN,
            depth: 9.0,
            division: 99,
            ..p
        }
        .sanitized();
        assert!((bad.rate_hz - 1.0).abs() < 1e-6);
        assert_eq!(bad.depth, 1.0);
        assert_eq!(bad.division, LFO_DEFAULT_DIVISION);

        let slow = LfoParams { rate_hz: 0.0, ..p }.sanitized();
        assert_eq!(slow.rate_hz, 0.01);
        let fast = LfoParams {
            rate_hz: 500.0,
            ..p
        }
        .sanitized();
        assert_eq!(fast.rate_hz, 40.0);
    }

    #[test]
    fn lfo_preview_values_match_the_waveform_table() {
        let p = LfoParams::default();
        let hold = 0.42;
        for (wave, expected) in [
            (LfoWaveform::Sine, [0.0, 1.0, 0.0, -1.0]),
            (LfoWaveform::Triangle, [-1.0, 0.0, 1.0, 0.0]),
            (LfoWaveform::Saw, [-1.0, -0.5, 0.0, 0.5]),
            (LfoWaveform::Square, [1.0, 1.0, -1.0, -1.0]),
            (LfoWaveform::SampleHold, [hold, hold, hold, hold]),
        ] {
            let params = LfoParams {
                waveform: wave,
                ..p
            };
            for (phase, want) in [0.0, 0.25, 0.5, 0.75].into_iter().zip(expected) {
                let got = params.preview_value(phase, hold);
                assert!(
                    (got - want).abs() < 1e-5,
                    "{} at {phase}: {got} != {want}",
                    wave.label()
                );
            }
        }
    }

    #[test]
    fn lfo_node_is_a_control_source_without_audio_ports() {
        let node = NodeDoc::Lfo {
            node_id: 9,
            track: 0,
            params: LfoParams::default(),
        };
        assert!(node.has_control_out());
        assert!(!node.has_audio_ports());
        assert!(!node.has_midi_in());
        assert!(!node.has_midi_out());
        assert!(!node.accepts_control());
        assert_eq!(node.node_id(), 9);
    }

    #[test]
    fn lfo_node_round_trips_through_ron() {
        let node = NodeDoc::Lfo {
            node_id: 9,
            track: 1,
            params: LfoParams {
                waveform: LfoWaveform::SampleHold,
                rate_hz: 2.5,
                sync: true,
                division: 3,
                depth: 0.75,
            },
        };
        let text = ron::to_string(&node).expect("serialize");
        assert!(text.contains("type:\"lfo\""), "tag missing: {text}");
        let back: NodeDoc = ron::from_str(&text).expect("deserialize");
        assert_eq!(node, back);
    }

    #[test]
    fn malformed_maps_fall_back_to_linear() {
        // Log with a non-positive lower bound has no logarithm: linear.
        let spec = MapSpec {
            min: 0.0,
            max: 100.0,
            mode: MapMode::Log,
        };
        assert_eq!(spec.sanitized().mode, MapMode::Linear);
        assert!((spec.map(0.0) - 50.0).abs() < 1e-6);
        // Inverted / non-finite bounds collapse to the identity map.
        assert_eq!(MapSpec::linear(10.0, 0.0).sanitized(), MapSpec::UNIT);
        assert_eq!(
            MapSpec {
                min: f32::NAN,
                max: 1.0,
                mode: MapMode::Linear
            }
            .sanitized(),
            MapSpec::UNIT
        );
    }

    #[test]
    fn default_map_follows_the_target_axis() {
        let mut graph = GraphDoc::default();
        let params = EffectParams::Delay(DelayParams::default());
        graph.nodes.push(NodeDoc::Effect {
            node_id: 7,
            track: 0,
            voice: VoiceSpec::Builtin,
            params,
            delay: None,
        });

        // Context-free parameters.
        assert_eq!(
            AutoParam::Node(NodeParam::PitchOffset).default_map(&graph, &PortRef::Node(7)),
            MapSpec::linear(-24.0, 24.0)
        );
        assert_eq!(
            AutoParam::Tempo.default_map(&graph, &PortRef::TimelineIn),
            MapSpec::log(MIN_TEMPO_BPM, MAX_TEMPO_BPM)
        );
        assert_eq!(
            AutoParam::pitch_bend(0, 12.0).default_map(&graph, &PortRef::Node(7)),
            MapSpec::linear(-12.0, 12.0)
        );
        // Effect slots resolve against the node payload (delay time is log).
        assert_eq!(
            AutoParam::Node(NodeParam::EffectSlot(0)).default_map(&graph, &PortRef::Node(7)),
            MapSpec::log(0.005, 1.0)
        );
        // Unknown node: normalized pass-through.
        assert_eq!(
            AutoParam::Node(NodeParam::EffectSlot(0)).default_map(&graph, &PortRef::Node(99)),
            MapSpec::linear(0.0, 1.0)
        );
    }

    /// D12: SF2's bend range belongs to the **instrument**, so the axis has to
    /// follow the node. Otherwise raising the range in the inspector would
    /// leave every stored cable squeezed into the old, narrower wire.
    #[test]
    fn a_bend_axis_follows_the_instruments_range() {
        let mut graph = GraphDoc::default();
        graph.nodes.push(NodeDoc::Instrument {
            node_id: 11,
            track: 0,
            voice: VoiceSpec::SoundFont {
                path: "font.sf2".to_string(),
                bank: 0,
                program: 0,
                gain: 1.0,
                bend_range: 12.0,
            },
            synth: SynthParams::default(),
        });
        // The cable still says ±2 (it was routed before the range changed).
        let param = AutoParam::pitch_bend(0, 2.0);
        let target = PortRef::Node(11);
        assert_eq!(param.bend_range(&graph, &target), 12.0);
        assert_eq!(
            param.default_map(&graph, &target),
            MapSpec::linear(-12.0, 12.0)
        );
        // No instrument to ask (a plugin, or a dangling cable): the copy wins.
        assert_eq!(param.bend_range(&graph, &PortRef::Master), 2.0);
    }

    /// D7: an old project's curves keep their shape — every anchor is
    /// inverse-mapped through the same axis the old value lived on, and the
    /// cable records the map it now needs.
    #[test]
    fn migration_normalizes_curves_through_their_target_axis() {
        // Start from an empty graph: the default document already ships a
        // full patch, whose cables would sit in front of ours.
        let mut project = Project {
            graph: GraphDoc::default(),
            ..Project::default()
        };
        project.graph.nodes.push(NodeDoc::Instrument {
            node_id: 2,
            track: 0,
            voice: VoiceSpec::Builtin,
            synth: SynthParams::default(),
        });
        project.graph.cables.push(CableDoc::automation(
            0,
            2,
            AutoParam::Node(NodeParam::PitchOffset),
        ));
        project.graph.cables.push(CableDoc::tempo(1));
        project.control_tracks.resize_with(3, ControlTrack::default);
        // ±24 st axis: 0 st is the centre, +12 st is three quarters up.
        project.control_tracks[0].clips = vec![ControlClip {
            curve: vec![
                AutoPoint {
                    tick: 0,
                    value: 0.0,
                },
                AutoPoint {
                    tick: 100,
                    value: 12.0,
                },
                AutoPoint {
                    tick: 200,
                    value: -24.0,
                },
            ],
            ..ControlClip::default()
        }];
        // Tempo axis is log 20..300: 120 BPM is no longer the middle.
        project.control_tracks[1].clips = vec![ControlClip {
            curve: vec![AutoPoint {
                tick: 0,
                value: 120.0,
            }],
            ..ControlClip::default()
        }];
        // No cable: values are clamped into the normalized domain.
        project.control_tracks[2].clips = vec![ControlClip {
            curve: vec![
                AutoPoint {
                    tick: 0,
                    value: 4.0,
                },
                AutoPoint {
                    tick: 10,
                    value: -3.0,
                },
            ],
            ..ControlClip::default()
        }];

        project.migrate_v8_to_v9();

        let pitch = &project.control_tracks[0].clips[0].curve;
        assert_eq!(pitch[0].value, 0.0);
        assert!((pitch[1].value - 0.5).abs() < 1e-5);
        assert_eq!(pitch[2].value, -1.0);
        // The cable stores nothing: the axis stays derivable from the target,
        // so it moves if that target's range ever changes.
        assert_eq!(project.graph.cables[0].map, None);
        assert_eq!(
            project.graph.cables[0]
                .param
                .default_map(&project.graph, &project.graph.cables[0].to),
            MapSpec::linear(-24.0, 24.0)
        );

        let tempo = project.control_tracks[1].clips[0].curve[0].value;
        let spec = MapSpec::log(MIN_TEMPO_BPM, MAX_TEMPO_BPM);
        assert!((spec.map(tempo) - 120.0).abs() < 0.01, "tempo {tempo}");
        assert_eq!(project.graph.cables[1].map, None);

        let loose = &project.control_tracks[2].clips[0].curve;
        assert_eq!((loose[0].value, loose[1].value), (1.0, -1.0));
        // A dangling curve stays dangling: migration never invents a cable.
        assert!(project.graph.auto_target(2).is_none());
    }

    /// A v9 document round-trips: the map rides the cable, the anchors stay
    /// normalized, and a legacy document without the field loads as `None`
    /// (derived at compile time) instead of shifting values.
    #[test]
    fn v9_cables_round_trip_and_legacy_cables_stay_unmapped() {
        let mut project = Project::default();
        project.graph.cables.push(CableDoc::tempo(0));
        project.graph.cables[0].map = Some(MapSpec::log(MIN_TEMPO_BPM, MAX_TEMPO_BPM));
        project.control_tracks = vec![ControlTrack {
            clips: vec![ControlClip {
                curve: vec![AutoPoint {
                    tick: 0,
                    value: 0.25,
                }],
                ..ControlClip::default()
            }],
            ..ControlTrack::default()
        }];
        let text = ron::to_string(&project).expect("serialize");
        let back: Project = ron::from_str(&text).expect("deserialize");
        assert_eq!(back.graph.cables[0].map, project.graph.cables[0].map);
        assert_eq!(back.control_tracks[0].clips[0].curve[0].value, 0.25);

        // A cable written before v9 has no `map` key: it still parses, and the
        // compiler derives the map from the target instead.
        let legacy = CableDoc::tempo(0);
        assert_eq!(legacy.map, None);
        let legacy_text = ron::to_string(&legacy).expect("serialize legacy");
        assert!(!legacy_text.contains("map"));
        let parsed: CableDoc = ron::from_str(&legacy_text).expect("legacy cable");
        assert_eq!(parsed, legacy);
    }
}

#[cfg(test)]
mod sample_region_tests {
    use super::{SampleLoopMode, SampleRegion};

    /// A sample of `frames` frame pairs has `frames - 1` as the last
    /// interpolatable position; `end == 0` means "the whole file", and every
    /// handle is clamped inside it so the UI can never point past the data.
    #[test]
    fn zero_bounds_mean_the_whole_sample() {
        let whole = SampleRegion::default().resolve(48_000);
        assert_eq!(whole.start, 0);
        assert_eq!(whole.loop_start, 0);
        assert_eq!(whole.loop_end, 47_999);
        assert_eq!(whole.end, 47_999);
        assert!(!whole.loops, "Off never loops");
        assert!(SampleRegion::default().is_full_sample());
    }

    #[test]
    fn explicit_bounds_are_kept_and_clamped() {
        let region = SampleRegion {
            start: 100,
            loop_start: 500,
            loop_end: 1_500,
            end: 2_000,
            loop_mode: SampleLoopMode::Forward,
        }
        .resolve(48_000);
        assert_eq!(
            (region.start, region.loop_start, region.loop_end, region.end),
            (100, 500, 1_500, 2_000)
        );
        assert!(region.loops);

        // Bounds past the end clamp to the last frame, and a start past the
        // window collapses onto `end` instead of inverting it.
        let clipped = SampleRegion {
            start: 90_000,
            loop_start: 80_000,
            loop_end: 90_000,
            end: 90_000,
            loop_mode: SampleLoopMode::PingPong,
        }
        .resolve(1_000);
        assert_eq!(clipped.end, 999);
        assert_eq!(clipped.start, 999);
        assert_eq!(clipped.loop_end, 999);
        assert_eq!(clipped.loop_start, 999);
        assert!(!clipped.loops, "a zero-width window is not a loop");
    }

    /// One frame of loop is a click generator, not a loop: refuse it here so
    /// every renderer inherits the same guard.
    #[test]
    fn a_one_frame_loop_does_not_loop() {
        for mode in [SampleLoopMode::Forward, SampleLoopMode::PingPong] {
            let region = SampleRegion {
                loop_start: 100,
                loop_end: 101,
                end: 0,
                loop_mode: mode,
                ..SampleRegion::default()
            }
            .resolve(1_000);
            assert!(!region.loops, "{mode:?} with one frame must not loop");
        }
        let two = SampleRegion {
            loop_start: 100,
            loop_end: 102,
            loop_mode: SampleLoopMode::Forward,
            ..SampleRegion::default()
        }
        .resolve(1_000);
        assert!(two.loops);
    }

    /// Old projects carry no `region` key at all: the whole-sample default
    /// must load, and the mode index must match the engine's parameter order.
    #[test]
    fn region_defaults_and_mode_indices() {
        let partial: super::VoiceSpec =
            ron::from_str("(type: \"sample\", path: \"kick.wav\")").expect("legacy file loads");
        let super::VoiceSpec::Sample { region, .. } = partial else {
            panic!("expected a sample voice");
        };
        assert!(region.is_full_sample(), "legacy file = whole sample");
        assert_eq!(SampleLoopMode::ALL.map(SampleLoopMode::index), [0, 1, 2]);
        assert_eq!(SampleLoopMode::default(), SampleLoopMode::Off);
    }
}
