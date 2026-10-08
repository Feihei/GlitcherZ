//! Lock-free command queue from the UI to the audio thread.

use rtrb::{Consumer, Producer, RingBuffer};

use crate::realtime::automation::RtAutoTrack;
use crate::realtime::sequencer::{PATTERN_STEPS, RtClip, SequencerStep, TrackSource};

/// Default capacity of the control command queue. When full, new commands are
/// dropped by the sender (see
/// [`crate::engine::AudioEngine::send`]); no thread is ever blocked.
pub const COMMAND_QUEUE_CAPACITY: usize = 1024;

/// Parameter identifier. Each `Processor` declares the parameters it responds to
/// in [`Processor::set_param`](crate::realtime::Processor::set_param).
///
/// TODO(from roadmap step 5 onward): introduce graph node IDs and switch to
/// `NodeTarget`-directed routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamId {
    /// Sine source frequency (Hz).
    SineFrequency,
    /// Sine source output gain (linear).
    SineGain,
    /// Master output gain (linear).
    MasterGain,
    /// Filter cutoff frequency (Hz).
    FilterCutoff,
    /// Filter resonance (Simper SVF's res, 0..0.98).
    FilterResonance,
    /// Filter mode: 0 = low-pass, 1 = high-pass, 2 = band-pass.
    FilterMode,
    /// Envelope attack time (seconds).
    Attack,
    /// Envelope decay time (seconds).
    Decay,
    /// Envelope sustain level (0..1).
    Sustain,
    /// Envelope release time (seconds).
    Release,
    /// Track gain (linear, 0..1.5).
    TrackGain,
    /// Track pan (-1..1, linear pan law).
    TrackPan,
    /// Track mute (≥ 0.5 means muted).
    TrackMute,
    /// Delay time (seconds, 0.005..1).
    DelayTime,
    /// Delay feedback (0..0.9).
    DelayFeedback,
    /// Delay wet mix ratio (0..1).
    DelayMix,
    /// External plugin parameter (payload is the plugin-side parameter ID).
    /// Values uniformly carry the **normalized 0..1** semantic: CLAP and VST3
    /// nodes both forward them as-is via their respective host protocols (both
    /// happen to be normalized models), and each node maps them to the
    /// plugin's actual parameter range itself.
    PluginParam(u32),
}

/// Note event (MIDI semantics). `note` is the MIDI note number (69 = A4),
/// `velocity` is the velocity in 0..1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoteEvent {
    /// Note pressed: triggers the sound source + envelope enters Attack.
    On { note: u8, velocity: f32 },
    /// Note released: envelope enters Release.
    Off { note: u8 },
}

/// Sequencer transport and configuration commands.
///
/// `SetClip` carries an entire `Copy` clip (about 2 KB), making this enum
/// significantly larger than the other variants — a deliberate trade-off for
/// "zero heap allocation on the real-time side": the command queue allocates
/// slots for the largest variant (1024 × ≈2 KB ≈ 2 MB, pre-allocated once).
/// If clip size grows in the future, this could become a separate bulk data
/// channel (non-real-time pre-written shared buffer + small command
/// notification).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy)]
pub enum SequencerCommand {
    /// Play from step 0 (the stream position at receive time is the clock origin).
    Play,
    /// Stop and release all tracks' current notes.
    Stop,
    /// Set BPM (20..300).
    SetBpm(f32),
    /// Set the gate ratio of the step length (0.05..1).
    SetGate(f32),
    /// Replace a track's 16-step pattern in one block.
    SetPattern {
        track: usize,
        steps: [SequencerStep; PATTERN_STEPS],
    },
    /// Replace a track's piano-roll clip in one block.
    SetClip { track: usize, clip: RtClip },
    /// Switch a track's sound source (step mode / piano-roll clip).
    SetSource { track: usize, source: TrackSource },
    /// Replace an automation track entity in one block (empty curve = clear).
    ///
    /// The curve carries no target parameter (D7): the delivery endpoint
    /// `(node, parameter)` is determined by the node graph's automation routing
    /// table, set via `AudioEngine::set_auto_route`.
    SetAutoTrack {
        /// Automation track index (0..[`crate::realtime::MAX_AUTO_TRACKS`]).
        index: usize,
        /// Track entity (its own loop length + pure anchor curve).
        auto: RtAutoTrack,
    },
    /// Set the active MIDI track count (D12 dynamic tracks; values beyond
    /// capacity are clamped on the real-time side).
    SetMidiTrackCount(usize),
}

/// Control commands from the UI / control thread to the audio thread.
///
/// Must remain `Copy`: commands pass by value through the lock-free queue,
/// **heap allocations are forbidden** (`String` / `Vec` / `Box`, etc.).
#[allow(clippy::large_enum_variant)] // see the SequencerCommand notes
#[derive(Debug, Clone, Copy)]
pub enum EngineCommand {
    /// Set a master-chain parameter (global parameters such as master gain).
    SetParam { id: ParamId, value: f32 },
    /// Set a parameter of the given track (tone / envelope / delay / gain / pan / mute).
    SetTrackParam {
        track: usize,
        id: ParamId,
        value: f32,
    },
    /// Send a note event to the given track's sound source (keyboard playing, etc.).
    Note { track: usize, event: NoteEvent },
    /// Sequencer transport / configuration.
    Sequencer(SequencerCommand),
    /// Set a node parameter addressed by node_id (P5b: node-addressed path;
    /// both automation routing and the canvas treat this as authoritative).
    SetNodeParam {
        /// Target node ID (assigned by the compiler, exposed to the UI via
        /// structure updates).
        node_id: u64,
        /// Parameter.
        id: ParamId,
        /// Parameter value.
        value: f32,
    },
}

/// Creates a pair of command queue endpoints: the producer goes to the UI /
/// control thread, the consumer goes to the audio thread (consumed at block
/// boundaries). **Not RT-safe**; call only before the stream starts.
pub fn command_channel(capacity: usize) -> (Producer<EngineCommand>, Consumer<EngineCommand>) {
    RingBuffer::new(capacity)
}

#[cfg(test)]
mod tests {
    use super::{EngineCommand, NoteEvent, ParamId, command_channel};

    fn set_param(value: f32) -> EngineCommand {
        EngineCommand::SetParam {
            id: ParamId::MasterGain,
            value,
        }
    }

    fn match_value(cmd: EngineCommand) -> f32 {
        match cmd {
            EngineCommand::SetParam { value, .. } => value,
            EngineCommand::SetTrackParam { value, .. } => value,
            EngineCommand::SetNodeParam { value, .. } => value,
            EngineCommand::Note { .. } => f32::NAN,
            EngineCommand::Sequencer(_) => f32::NAN,
        }
    }

    #[test]
    fn fifo_order() {
        let (mut tx, mut rx) = command_channel(4);
        tx.push(set_param(0.25)).expect("push failed");
        tx.push(EngineCommand::Note {
            track: 0,
            event: NoteEvent::On {
                note: 69,
                velocity: 1.0,
            },
        })
        .expect("push failed");
        tx.push(set_param(0.5)).expect("push failed");
        assert_eq!(rx.pop().ok().map(match_value), Some(0.25));
        assert!(matches!(
            rx.pop(),
            Ok(EngineCommand::Note {
                track: 0,
                event: NoteEvent::On { note: 69, .. }
            })
        ));
        assert_eq!(rx.pop().ok().map(match_value), Some(0.5));
        assert!(rx.pop().is_err(), "empty queue should return Err");
    }

    #[test]
    fn full_queue_rejects_push() {
        let (mut tx, mut rx) = command_channel(2);
        assert!(tx.push(set_param(1.0)).is_ok());
        assert!(tx.push(set_param(2.0)).is_ok());
        assert!(
            tx.push(set_param(3.0)).is_err(),
            "capacity 2 full should return Err"
        );
        assert_eq!(rx.pop().ok().map(match_value), Some(1.0));
        assert!(
            tx.push(set_param(3.0)).is_ok(),
            "should accept pushes again after popping"
        );
    }
}
