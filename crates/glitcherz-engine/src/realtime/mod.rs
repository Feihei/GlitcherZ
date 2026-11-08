//! # Real-time audio path
//!
//! All code in this module that runs on the **audio callback thread** must obey
//! the real-time iron rules (AGENTS.md "Real-Time Audio Iron Rules", highest priority):
//!
//! 1. **No allocation/deallocation** (`Vec::new`, `Box::new`, `String`, `format!`, etc.);
//!    all buffers are pre-allocated at construction time (non-real-time thread).
//! 2. **No locks** (`Mutex`, `RwLock`); cross-thread communication only through
//!    lock-free queues (`rtrb`) and atomic variables.
//! 3. **No syscalls**: no file IO, no logging (`log!` / `println!`), no plugin loading.
//! 4. **No panics**: no `expect` / `unwrap` / panicking indexing, etc.;
//!    edge cases always degrade gracefully (silent output, dropped commands).
//!
//! Public methods are marked **RT-safe** in their docs to indicate whether they
//! may be called on the audio callback.
//! Constructors may allocate, but must only be called on the non-real-time thread;
//! once complete they are moved into the audio thread.
//!
//! Module structure:
//! - [`buffer`]: pre-allocated flat buffers ([`AudioBuffer`]);
//! - [`context`]: per-block context ([`ProcessContext`]);
//! - [`processor`]: the [`Processor`] trait and [`ProcessorChain`];
//! - [`map`]: normalized `[-1, 1]` control values → target axes ([`MapSpec`]);
//! - [`graph`]: node graph core ([`RtGraph`](graph::RtGraph), [`ExecPlan`](graph::ExecPlan),
//!   structure update protocol, and control-side compiler);
//! - [`osc`] / [`gain`]: band-limited oscillator source and gain effect (`Processor` reference implementations);
//! - [`effect`]: built-in effect framework — descriptors, [`Smoothed`](effect::Smoothed)
//!   parameter smoothing and the [`EffectParamBank`](effect::EffectParamBank);
//! - [`delay`] / [`eq`] / [`compressor`] / [`reverb`]: the built-in effects themselves (each
//!   owns its static descriptor next to its DSP);
//! - [`envelope`] / [`svf`] / [`synth`]: ADSR envelope, SVF filter, and full synthesizer;
//! - [`sf2`]: SoundFont voice ([`Sf2VoiceProcessor`](sf2::Sf2VoiceProcessor)) — RT rendering
//!   around the audited `rustysynth-ext` synthesizer (loading stays in [`crate::sf2`]);
//! - [`sample`]: sample player voice ([`SampleVoiceProcessor`](sample::SampleVoiceProcessor)) —
//!   RT rendering of a pre-decoded wav with a fixed voice pool (decoding stays in
//!   [`crate::sample`]);
//! - [`commands`]: `rtrb` command queue and [`EngineCommand`] / [`NoteEvent`] / [`ParamId`];
//! - [`capture`]: capture tap — stamps live input events with the transport position and
//!   hands them back to the control thread (recording, spec `recording.md` D6);
//! - [`atomic`]: [`AtomicF32`] (audio thread → UI scalar feedback);
//! - [`block`]: [`BlockRunner`] — splits the host's (cpal) arbitrarily long interleaved
//!   buffer into fixed blocks, consumes commands at block boundaries, and reports state back.

pub mod arpeggiator;
pub mod atomic;
pub mod audio_track;
pub mod automation;
pub mod bitcrusher;
pub mod block;
pub mod buffer;
pub mod capture;
pub mod chorus;
pub mod commands;
pub mod compressor;
pub mod context;
pub mod curve_map;
pub mod delay;
pub mod drum_kit;
pub mod effect;
pub mod envelope;
pub mod eq;
pub mod events;
pub mod flanger;
pub mod gain;
pub mod graph;
pub mod input;
pub mod lfo;
pub mod limiter;
pub mod map;
pub mod metronome;
pub mod midi_envelope;
pub mod mixer;
pub mod mod_delay;
pub mod modal;
pub mod moog;
pub mod organ;
pub mod osc;
pub mod phaser;
pub mod pluck;
pub mod portamento;
pub mod processor;
pub mod reverb;
pub mod ring_mod;
pub mod sample;
pub mod saturation;
pub mod sequencer;
pub mod sf2;
pub mod silent;

pub mod svf;
pub mod synth;
pub mod vocoder;
pub mod voice;

/// Fixed processing block size (in samples). See AGENTS.md "Processing Graph" (64 or 128 recommended).
pub const FIXED_BLOCK_SIZE: usize = 128;

/// MIDI track capacity limit (D12 dynamic tracks: the model layer's `Vec<MidiTrack>`
/// shares this limit).
/// The active track count is sent from the control side via
/// `SequencerCommand::SetMidiTrackCount`;
/// graph routing tables for unconfigured tracks are entirely empty slots (inert).
pub const MAX_MIDI_TRACKS: usize = 16;

/// Default (new project) MIDI track count.
pub const DEFAULT_MIDI_TRACKS: usize = 4;

pub use atomic::AtomicF32;

pub use arpeggiator::{ARP_DIVISION_COUNT, ArpeggiatorProcessor, MAX_HELD_NOTES};
pub use audio_track::{
    AUDIO_CLIP_TRASH_CAPACITY, AudioClipTable, AudioTrackProcessor, RtAudioClip,
    audio_clip_trash_channel, empty_clip_table,
};
pub use automation::{
    MAX_AUTO_POINTS, MAX_AUTO_TRACKS, RtAutoPoint, RtAutomation, RtControlClip, RtCurveInterp,
};
pub use bitcrusher::BitcrusherProcessor;
pub use block::{BlockRunner, Feedback};
pub use buffer::AudioBuffer;
pub use capture::{
    CAPTURE_QUEUE_CAPACITY, CaptureTap, CapturedControl, CapturedNote, capture_channel,
    control_capture_channel,
};
pub use chorus::ChorusProcessor;
pub use commands::{
    COMMAND_QUEUE_CAPACITY, EngineCommand, NoteEvent, ParamId, SequencerCommand, command_channel,
};
pub use compressor::CompressorProcessor;
pub use context::ProcessContext;
pub use curve_map::{CurveMapProcessor, mode_from_value, mode_value};
pub use delay::DelayProcessor;
pub use drum_kit::{DRUM_MAX_SLOTS, DRUM_POLYPHONY, DrumKitProcessor, DrumSlotSpec};
pub use effect::{
    BuiltinEffectKind, Curve, EffectDescriptor, EffectMeters, EffectParamBank, MAX_EFFECT_PARAMS,
    MeterReading, ParamFlags, ParamSpec, Rate, Smoothed, StereoClass, Unit, validate_descriptor,
};
pub use envelope::AdsrProcessor;
pub use eq::EqProcessor;
pub use events::{MAX_EVENT_CABLES, MAX_EVENT_FANOUT, MAX_NODE_EVENTS, NodeEvents, RtEvent};
pub use flanger::FlangerProcessor;
pub use gain::GainProcessor;
pub use graph::{
    AutoDest, AutoRoute, Cable, ChannelStripProcessor, EventCable, EventKind, ExecPlan,
    FeedbackProcessor, GraphCompiler, GraphLevels, GraphUpdate, MASTER_NODE_ID, MAX_CABLES,
    MAX_MIDI_FANOUT, NodeKind, RtGraph, TIMELINE_IN, TopologyError,
};
pub use input::InputProcessor;
pub use lfo::{
    LFO_DIVISION_TICKS, LFO_MAX_RATE_HZ, LFO_MIN_RATE_HZ, LFO_WAVEFORM_COUNT, LfoProcessor,
};
pub use limiter::LimiterProcessor;
pub use map::{MapMode, MapSpec};
pub use midi_envelope::MidiEnvelopeProcessor;
pub use mixer::{MixerChannel, MixerProcessor};
pub use modal::{MAX_RESONATORS, MAX_VOICES as MODAL_MAX_VOICES, ModalProcessor};
pub use moog::Ladder;
pub use organ::{DRAWBAR_COUNT, MAX_VOICES as ORGAN_MAX_VOICES, OrganProcessor};
pub use osc::{OSC_WAVEFORM_COUNT, OscProcessor};
pub use phaser::PhaserProcessor;
pub use pluck::{MAX_VOICES as PLUCK_MAX_VOICES, PluckProcessor};
pub use portamento::{MAX_PENDING_NOTES, PortamentoProcessor};
pub use processor::{ChannelWeights, MAX_WEIGHT_CHANNELS, Processor, ProcessorChain};
pub use reverb::ReverbProcessor;
pub use ring_mod::RingModProcessor;
pub use sample::{SAMPLE_DEFAULT_ROOT, SAMPLE_POLYPHONY, SampleVoiceProcessor};
pub use saturation::SaturationProcessor;
pub use sequencer::{
    ControlClipTable, MAX_TIME_SIGNATURE_NUMERATOR, MAX_TRACK_CLIPS, MIDI_CLIP_TRASH_CAPACITY,
    MIN_VELOCITY, MidiClipTable, RtCtrl, RtCtrlKind, RtMidiClip, RtNote, Sequencer, SequencerSink,
    TICKS_PER_BAR, TICKS_PER_QUARTER, TICKS_PER_STEP, empty_midi_clip_table,
    midi_clip_trash_channel, ticks_per_bar_of,
};
pub use sf2::{SF2_POLYPHONY, Sf2VoiceProcessor};
pub use silent::SilentProcessor;
pub use svf::{FilterMode, SvfProcessor};
pub use synth::{SynthProcessor, note_to_frequency};
pub use vocoder::VocoderProcessor;
pub use voice::{RecycledNode, STRUCTURE_QUEUE_CAPACITY, recycle_channel, structure_channel};
