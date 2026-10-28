//! Lock-free command queue from the UI to the audio thread.

use rtrb::{Consumer, Producer, RingBuffer};

use crate::realtime::automation::RtControlClip;
use crate::realtime::sequencer::MAX_TRACK_CLIPS;

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
    /// Pitch offset in **semitones** (−24..24, `0` = no offset), P1 /
    /// spec `roll-multiclip.md` D10/D11.
    ///
    /// The built-in sources' answer to "bend this note": a real-valued offset
    /// that they apply **per sample** with a one-pole smoother, instead of a
    /// 14-bit MIDI event applied at block boundaries. No bend range, no
    /// quantization, and it works on sounding voices (which `SampleRootNote`
    /// deliberately does not).
    PitchOffset,
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
    /// Built-in effect parameter addressed by **slot index** into the node's
    /// [`EffectDescriptor`](crate::realtime::effect::EffectDescriptor) (spec
    /// `.agents/spec/effects.md` D5).
    ///
    /// The slot is the parameter's position in the effect's static table, and
    /// the node itself is addressed by `EngineCommand::SetNodeParam`'s
    /// `node_id` — so two reverbs on different nodes never collide, and adding
    /// an effect does not grow this enum. The three `Delay*` variants above are
    /// the legacy fixed track trio (`track_delay`) and forward to the same
    /// slots inside `DelayProcessor`.
    BuiltinParam(u8),
    /// Feedback tap gain (P6; 0..1). Applied to the one-block-delayed sum
    /// of the feedback node's in-edges.
    FeedbackGain,
    /// Mixer channel gain (payload = channel index 0..32, linear 0..1.5; D15).
    MixerGain(u8),
    /// Mixer channel pan (payload = channel index; -1..1, linear law).
    MixerPan(u8),
    /// Mixer channel mute (payload = channel index; ≥ 0.5 = muted).
    MixerMute(u8),
    /// Mixer channel solo (payload = channel index; ≥ 0.5 = soloed; any solo
    /// mutes all non-soloed channels of the same mixer, D21).
    MixerSolo(u8),
    /// Mixer output-stage pan (-1..1, linear law).
    MixerOutPan,
    /// Mixer output-stage mute (≥ 0.5 = muted).
    MixerOutMute,
    /// External plugin parameter (payload is the plugin-side parameter ID).
    /// Values uniformly carry the **normalized 0..1** semantic: CLAP and VST3
    /// nodes both forward them as-is via their respective host protocols (both
    /// happen to be normalized models), and each node maps them to the
    /// plugin's actual parameter range itself.
    PluginParam(u32),
    /// SF2 voice: bank select (MIDI CC #0). Value carries the **literal bank
    /// number** (rounded; D7 track-level `bank:program` addressing). Takes
    /// effect together with the next `Sf2ProgramChange`, mirroring MIDI
    /// semantics.
    Sf2BankSelect,
    /// SF2 voice: program change. Value carries the **literal program
    /// number** (rounded); applies the currently latched bank.
    Sf2ProgramChange,
    /// SF2 voice: node output gain (linear, clamped 0..2; non-finite values
    /// are ignored). `1.0` = the font's own level: the processor overrides
    /// the synthesizer's built-in 0.5 default at construction (U13), since
    /// the crate default is a hidden -6 dB that made every SoundFont node
    /// sound quiet.
    Sf2Gain,
    /// Sample voice: root MIDI note of the loaded sample (0..127, default
    /// 60 = C4). Affects the playback rate of **subsequent** note-ons;
    /// sounding voices keep their trigger-time rate (the sampler-standard
    /// freeze semantics).
    SampleRootNote,
    /// Sample voice: output gain (linear, clamped 0..2; non-finite values
    /// are ignored). Applies immediately to all sounding voices.
    SampleGain,
    /// Sample voice: play mode — 0 = Gate (note-off enters the release
    /// fade), ≥ 0.5 = OneShot (pitch-matched note-offs are ignored; the
    /// blanket transport sweep still stops the voice).
    SamplePlayMode,
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
/// Clip tables are **not** here (D15 revision, 2026-10-28): MIDI and control
/// clip tables carry control-side-owned `Vec`s and ride the structure queue
/// (`GraphUpdate::SetMidiClips` / `SetControlClips`), so they never inflate the
/// command-queue slot size. This enum stays small and `Copy`, and the queue
/// keeps its 1024 slots at a negligible footprint.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy)]
pub enum SequencerCommand {
    /// Play (from the current transport position — resume semantics; the
    /// position only returns to zero via `Stop` or `Seek(0.0)`).
    Play,
    /// Stop, rewind to zero and release all tracks' current notes.
    Stop,
    /// Pause: stop scheduling but keep the transport position (resume with
    /// `Play`). Sounding notes are released by the block runner.
    Pause,
    /// Move the transport to `ticks` (musical position; negative values are
    /// clamped to zero). Pending note offs are cleared — nothing from the
    /// old position may fire at the new one.
    Seek(f64),
    /// Song loop range (U1): while playing, the transport wraps from
    /// `end_ticks` back to `start_ticks`. `end_ticks == 0` (or `end <=
    /// start`) disables looping.
    SetLoopRange { start_ticks: u32, end_ticks: u32 },
    /// Set BPM (20..300).
    SetBpm(f32),
    /// Set the document's meter (U3a). Only the bar length derived from it
    /// changes (arrangement-end rounding); no event moves and the transport
    /// position is kept — a meter is notation plus a bar length, not a data
    /// transform. Invalid fields are clamped on the real-time side.
    SetTimeSignature { numerator: u8, denominator: u8 },
    /// Replace one control track's clip table in one block (D15 batch Copy;
    /// D17: the delivery endpoint stays decided by the automation routing
    /// table, set via `AudioEngine::set_auto_route`).
    SetControlClips {
        index: usize,
        clips: [RtControlClip; MAX_TRACK_CLIPS],
    },
    /// Set the active MIDI track count (D12 dynamic tracks; values beyond
    /// capacity are clamped on the real-time side).
    SetMidiTrackCount(usize),
    /// Set the arrangement-end floor (U7 export): the auto-stop never fires
    /// before this tick. The offline export sets it to the latest clip end
    /// across ALL track kinds (audio clips are not sequencer content, so
    /// the D16 end alone would cut audio ending past the MIDI arrangement).
    /// `0` clears it — the live engine never sets anything else.
    SetArrangementFloor(u32),
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
    /// Send a note event to **one instrument node** addressed by `node_id`
    /// (G6, spec `.agents/spec/plugin-gui.md` §8: the virtual-keyboard route).
    ///
    /// Unlike [`EngineCommand::Note`] — which fans out through the timeline
    /// MIDI routing table — this reaches exactly the addressed node and
    /// ignores the track table entirely (the vk node itself never enters the
    /// graph; only its document cable decides the target). Unknown ids are
    /// silently dropped (RT-safe, no panic).
    NoteAtNode { node_id: u64, event: NoteEvent },
    /// Sequencer transport / configuration.
    Sequencer(SequencerCommand),
    /// Set a node parameter addressed by node_id (P5b: node-addressed path;
    /// both automation routing and the patch treat this as authoritative).
    SetNodeParam {
        /// Target node ID (assigned by the compiler, exposed to the UI via
        /// structure updates).
        node_id: u64,
        /// Parameter.
        id: ParamId,
        /// Parameter value.
        value: f32,
    },
    /// Panic: release every sounding voice now (all 128 pitches, every
    /// node), leaving the transport and all parameters untouched. The
    /// sweep spans a few blocks — see `RtGraph::panic` — so the response
    /// lands within roughly 15 ms.
    Panic,
}

/// Creates a pair of command queue endpoints: the producer goes to the UI /
/// control thread, the consumer goes to the audio thread (consumed at block
/// boundaries). **Not RT-safe**; call only before the stream starts.
pub fn command_channel(capacity: usize) -> (Producer<EngineCommand>, Consumer<EngineCommand>) {
    RingBuffer::new(capacity)
}

#[cfg(test)]
mod tests {
    use super::{EngineCommand, NoteEvent, ParamId, SequencerCommand, command_channel};

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
            EngineCommand::NoteAtNode { .. } => f32::NAN,
            EngineCommand::Sequencer(SequencerCommand::SetBpm(value)) => value,
            EngineCommand::Sequencer(_) => f32::NAN,
            EngineCommand::Panic => f32::NAN,
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
