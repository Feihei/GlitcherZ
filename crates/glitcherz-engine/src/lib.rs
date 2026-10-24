#![forbid(unsafe_code)]

//! # glitcherz-engine
//!
//! The GlitcherZ audio engine: processing graph, scheduling, synthesizer and
//! effects.
//!
//! Module layout:
//! - [`realtime`]: the real-time audio path — [`realtime::AudioBuffer`],
//!   the [`realtime::Processor`] trait, [`realtime::BlockRunner`] (which
//!   splits host buffers into fixed blocks) and lock-free command queues.
//!   **All code executed on the audio callback lives here**, and follows the
//!   real-time iron rules in the module docs.
//! - [`engine`]: the non-real-time control side — [`AudioEngine`] handles cpal
//!   device management, the command queue producer and state feedback (peak
//!   level, playback position).
//! - [`sf2`]: non-real-time SoundFont loading (S2) — file IO, parsing and
//!   preset enumeration; the parsed [`Arc<rustysynth_ext::SoundFont>`] is
//!   handed to the audio-side processor without any further parsing.
//! - [`sample`]: non-real-time wav decoding (U5) — hound decode to f32
//!   stereo frames plus the session-level [`sample::SampleLibrary`] cache;
//!   the audio-side processor receives the already-decoded
//!   [`Arc<sample::SampleData>`].
//! - [`convert`]: the central point for f32 (audio path) / f64 (UI, time)
//!   conversions.
//! - [`host_gate`]: plugin-host infrastructure — the atomic handoff gate
//!   shared by the CLAP/VST3 hosts to serialize main-thread plugin calls
//!   (state load/save/param read) against the audio thread's `process`.
//!
//! Thread model overview (see AGENTS.md for details):
//!
//! ```text
//! UI/control thread                    Audio callback thread (cpal)
//! ───────────                        ──────────────────
//! AudioEngine::send ── rtrb ──► BlockRunner (consumes commands at block boundaries)
//! AudioEngine::peak_level ◄─ AtomicF32 ◄─ per-block peak
//! AudioEngine::position_seconds ◄─ AtomicU64 ◄─ playback position
//! ```

pub mod convert;
pub mod engine;
pub mod host_gate;
pub mod realtime;
pub mod sample;
pub mod sf2;

#[cfg(test)]
pub(crate) mod testing;

pub use engine::{AudioEngine, AudioEngineInfo, EngineError, available_outputs};
pub use sample::{SampleData, SampleError, SampleLibrary, StereoFrame};
pub use sf2::{Sf2Error, Sf2Library, Sf2Preset};
