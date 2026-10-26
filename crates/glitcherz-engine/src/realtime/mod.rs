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
//! - [`graph`]: node graph core ([`RtGraph`](graph::RtGraph), [`ExecPlan`](graph::ExecPlan),
//!   structure update protocol, and control-side compiler);
//! - [`sine`] / [`gain`]: sine oscillator source and gain effect (`Processor` reference implementations);
//! - [`envelope`] / [`svf`] / [`synth`]: ADSR envelope, SVF filter, and full synthesizer;
//! - [`sf2`]: SoundFont voice ([`Sf2VoiceProcessor`](sf2::Sf2VoiceProcessor)) — RT rendering
//!   around the audited `rustysynth-ext` synthesizer (loading stays in [`crate::sf2`]);
//! - [`sample`]: sample player voice ([`SampleVoiceProcessor`](sample::SampleVoiceProcessor)) —
//!   RT rendering of a pre-decoded wav with a fixed voice pool (decoding stays in
//!   [`crate::sample`]);
//! - [`commands`]: `rtrb` command queue and [`EngineCommand`] / [`NoteEvent`] / [`ParamId`];
//! - [`atomic`]: [`AtomicF32`] (audio thread → UI scalar feedback);
//! - [`block`]: [`BlockRunner`] — splits the host's (cpal) arbitrarily long interleaved
//!   buffer into fixed blocks, consumes commands at block boundaries, and reports state back.

pub mod atomic;
pub mod audio_track;
pub mod automation;
pub mod block;
pub mod buffer;
pub mod commands;
pub mod context;
pub mod delay;
pub mod envelope;
pub mod gain;
pub mod graph;
pub mod mixer;
pub mod processor;
pub mod sample;
pub mod sequencer;
pub mod sf2;
pub mod sine;
pub mod svf;
pub mod synth;
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

pub use audio_track::{
    AUDIO_CLIP_TRASH_CAPACITY, AudioClipTable, AudioTrackProcessor, RtAudioClip,
    audio_clip_trash_channel, empty_clip_table,
};
pub use automation::{
    MAX_AUTO_POINTS, MAX_AUTO_TRACKS, RtAutoPoint, RtAutomation, RtControlClip, RtCurveInterp,
};
pub use block::{BlockRunner, Feedback};
pub use buffer::AudioBuffer;
pub use commands::{
    COMMAND_QUEUE_CAPACITY, EngineCommand, NoteEvent, ParamId, SequencerCommand, command_channel,
};
pub use context::ProcessContext;
pub use delay::DelayProcessor;
pub use envelope::AdsrProcessor;
pub use gain::GainProcessor;
pub use graph::{
    AutoDest, AutoRoute, Cable, ChannelStripProcessor, ExecPlan, FeedbackProcessor, GraphCompiler,
    GraphLevels, GraphUpdate, MASTER_NODE_ID, MAX_CABLES, MAX_MIDI_FANOUT, NodeKind, RtGraph,
    TIMELINE_IN, TopologyError,
};
pub use mixer::{MixerChannel, MixerProcessor};
pub use processor::{ChannelWeights, MAX_WEIGHT_CHANNELS, Processor, ProcessorChain};
pub use sample::{SAMPLE_DEFAULT_ROOT, SAMPLE_POLYPHONY, SampleVoiceProcessor};
pub use sequencer::{
    MAX_CLIP_NOTES, MAX_TIME_SIGNATURE_NUMERATOR, MAX_TRACK_CLIPS, MIN_VELOCITY, RtMidiClip,
    RtNote, Sequencer, TICKS_PER_BAR, TICKS_PER_QUARTER, TICKS_PER_STEP, ticks_per_bar_of,
};
pub use sf2::{SF2_POLYPHONY, Sf2VoiceProcessor};
pub use sine::SineProcessor;
pub use svf::{FilterMode, SvfProcessor};
pub use synth::{SynthProcessor, note_to_frequency};
pub use voice::{RecycledNode, STRUCTURE_QUEUE_CAPACITY, recycle_channel, structure_channel};
