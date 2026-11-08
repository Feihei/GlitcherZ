//! Lock-free command queue from the UI to the audio thread.

use rtrb::{Consumer, Producer, RingBuffer};

use crate::realtime::sequencer::RtCtrl;

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
    /// Oscillator source frequency (Hz).
    OscFrequency,
    /// Oscillator output gain (linear).
    OscGain,
    /// Oscillator waveform index (0 = Sine, 1 = Triangle, 2 = Saw,
    /// 3 = Square; spec `instrument.md` D1).
    ///
    /// Discrete: switches take effect at the block boundary. The historical
    /// `Sine*` names are gone with the single-waveform oscillator.
    OscWaveform,
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
    /// Audio-input node gain (R4; linear 0..2).
    InputGain,
    /// Audio-input node source channel (R4; 0-based index into the device's
    /// interleaved stream).
    InputChannel,
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
    /// SF2 voice: pitch-bend range in semitones (1..=24, rounded). Applied
    /// with the standard RPN 0 sequence, so the instrument's 14-bit bend
    /// events span the chosen range (spec `curve-map.md` D9).
    Sf2BendRange,
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
    /// Sample voice: first frame of the head (source frames, rounded;
    /// spec `sample-player.md` D12). Frozen into voices at trigger time,
    /// like [`ParamId::SampleRootNote`].
    ///
    /// Frames ride the `f32` parameter channel, which is exact up to
    /// 2^24 frames (~5.8 min at 48 kHz) — far beyond any playable sample.
    SampleStart,
    /// Sample voice: loop window start (source frames). `0` = file start.
    SampleLoopStart,
    /// Sample voice: loop window end / start of the tail (source frames).
    /// `0` = [`ParamId::SampleEnd`].
    SampleLoopEnd,
    /// Sample voice: exclusive end of the window (source frames). `0` = the
    /// file's last frame, so an unset region is the whole sample.
    SampleEnd,
    /// Sample voice: loop mode — 0 = Off, 1 = Forward, 2 = Ping-pong.
    /// OneShot voices ignore the loop (they play head → loop → tail once;
    /// spec D14).
    SampleLoopMode,
    /// Drum kit: node output trim (linear, clamped 0..2; spec
    /// `drum-kit.md` K8). Applies immediately to every sounding voice.
    DrumGain,
    /// Drum kit: per-slot trim (payload = trigger MIDI key, 0..127).
    DrumSlotGain(u8),
    /// Drum kit: per-slot pan (-1..1, payload = trigger key).
    DrumSlotPan(u8),
    /// Drum kit: per-slot pitch trim in semitones (-24..24, payload =
    /// trigger key). Affects the rate of **subsequent** hits; sounding
    /// voices keep their trigger-time rate (K8).
    DrumSlotTune(u8),
    /// Drum kit: per-slot choke group (payload = trigger key; value is the
    /// group number, negative = no choke). New hits in the same group fade
    /// older ones out (K7).
    DrumSlotChoke(u8),
    /// Arpeggiator step division index (0..7; spec `midi-nodes.md` §5.1).
    ArpDivision,
    /// Arpeggiator gate length as a fraction of one step (0.05..1).
    ArpGate,
    /// Arpeggiator step order: 0 = Up, 1 = Down, 2 = UpDown, 3 = DownUp,
    /// 4 = Random.
    ArpMode,
    /// Arpeggiator octave layers (1..4).
    ArpOctaves,
    /// Arpeggiator latch (≥ 0.5 = keep running after all keys are released).
    ArpLatch,
    /// Arpeggiator harmony rule index (0 = off, 1 = octave, 2 = fifth,
    /// 3 = major third, 4 = minor third, 5 = whole tone, 6 = chromatic).
    ArpHarmony,
    /// Probability (0..0.9) of skipping a step.
    ArpSkip,
    /// Portamento glide time constant (milliseconds, 0..2000; spec
    /// `midi-nodes.md` §5.2).
    PortTime,
    /// Portamento output scale, low end (semitones, -24..24).
    PortMin,
    /// Portamento output scale, high end (semitones, -24..24).
    PortMax,
    /// Portamento legato-only glide (≥ 0.5 = glide only while the previous
    /// note is still held).
    PortLegato,
    /// MIDI envelope attack time (seconds, 0..5; spec `midi-nodes.md` §5.3).
    EnvAttack,
    /// MIDI envelope decay time (seconds, 0..5).
    EnvDecay,
    /// MIDI envelope sustain level (0..1).
    EnvSustain,
    /// MIDI envelope release time (seconds, 0..10).
    EnvRelease,
    /// MIDI envelope output scale, low end.
    EnvMin,
    /// MIDI envelope output scale, high end (driving a CC uses 0..127).
    EnvMax,
    /// LFO free-running rate in Hz (0.01..40; spec `.agents/spec/lfo.md` §3).
    LfoRate,
    /// LFO waveform index (0 = Sine, 1 = Triangle, 2 = Saw, 3 = Square,
    /// 4 = Sample & Hold).
    LfoWaveform,
    /// LFO output depth (0..1).
    LfoDepth,
    /// LFO musical sync (≥ 0.5 = use the division instead of Hz).
    LfoSync,
    /// LFO sync division index (0..5, parallel to `LFO_DIVISION_TICKS`).
    LfoDivision,
    /// Curve Map: the normalized control value arriving on its input port
    /// (**already normalized**, `-1..1`; spec `curve-map.md` D5/D6).
    ///
    /// Deliberately a parameter and not an event: the node has no MIDI input,
    /// it holds the last value it was handed and re-emits it on its control
    /// output. That is also why this write is *not* axis-mapped — the map
    /// belongs to the hop after the node.
    CurveMapIn,
    /// Curve Map: output axis low end.
    CurveMapMin,
    /// Curve Map: output axis high end.
    CurveMapMax,
    /// Curve Map: mapping mode (0 = Linear, 1 = Log, 2 = Exp).
    CurveMapMode,
    /// Modal instrument: preset index (0 = Mallet, 1 = Vibraphone,
    /// 2 = Bell, 3 = Singing Bowl; spec `.agents/spec/modal-instrument.md`).
    ///
    /// Discrete: the mode table is rebuilt at the block boundary, but the
    /// ringing resonator states are kept (a retuned tail glides instead of
    /// clicking).
    ModalPreset,
    /// Modal instrument: decay multiplier over every mode's T60
    /// (0.25..4; non-finite values are ignored).
    ModalDecay,
    /// Modal instrument: multiplier over the preset's beating offsets
    /// (0..2; `0` = perfectly harmonic pairs, `2` = exaggerated).
    ModalBeating,
    /// Modal instrument: strike length / softness (0..1). Affects the
    /// **next** strike only; sounding tails keep their excitation.
    ModalHardness,
    /// Modal instrument: strike brightness, i.e. the excitation low-pass
    /// cutoff (0..1). Affects the next strike only.
    ModalBrightness,
    /// Modal instrument: node output gain (linear, clamped 0..2; applies to
    /// every sounding voice).
    ModalGain,
    /// Plucked string: fundamental T60 in seconds (0.1..20; S1-D9).
    PluckDecay,
    /// Plucked string: loop low-pass darkness (0..1; 1 = `4·f0`, 0 = `32·f0`).
    PluckDamping,
    /// Plucked string: excitation low-pass (0..1); affects the next pluck.
    PluckBrightness,
    /// Plucked string: note-off damper T60 in seconds (0.005..2).
    PluckRelease,
    /// Plucked string: node output gain (0..2).
    PluckGain,
    /// Drawbar organ: level of one drawbar (payload = index 0..8, value 0..8;
    /// spec `.agents/spec/organ-instrument.md` S2-D10).
    ///
    /// A payload variant instead of nine enum arms, same shape as
    /// [`ParamId::DrumSlotGain`]: the engine has one branch and the UI loops.
    OrganBar(u8),
    /// Drawbar organ: percussion partial level (0..1; `0` = off). Frozen at
    /// the next note-on.
    OrganPercussion,
    /// Drawbar organ: percussion decay as a T60 in seconds (0.1..2).
    OrganPercussionDecay,
    /// Drawbar organ: percussion harmonic (`< 0.5` = 2nd / 4', otherwise
    /// 3rd / 2⅔'). Discrete: the partial is frozen at the next note-on.
    OrganPercussionHarmonic,
    /// Drawbar organ: key-switch attack ramp in seconds (0.001..0.2).
    OrganAttack,
    /// Drawbar organ: key-switch release ramp in seconds (0.005..1.0).
    OrganRelease,
    /// Drawbar organ: node output gain (0..2).
    OrganGain,
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
/// clip tables ride the structure queue (`GraphUpdate::SetMidiClips` /
/// `SetControlClips`), so they never inflate the command-queue slot size. The
/// MIDI table owns `Vec`s; the control table is a plain `Copy` array that only
/// travels this way because of its size (`32 × ~544 B ≈ 17 KB`). This enum
/// stays small and `Copy`, and the queue keeps its 1024 slots at a negligible
/// footprint.
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
    /// Enable / configure the transport metronome (`spec/recording.md` D1–D5).
    ///
    /// The click is synthesized inside [`crate::realtime::block::BlockRunner`]
    /// and mixed **after** the node graph: it is not a node, not automation,
    /// not part of the master peak meter, and the offline export engine never
    /// receives this command, so a rendered file can never contain a click
    /// (D2). `gain` is linear `0..=1`; out-of-range values are clamped
    /// real-time-side.
    SetMetronome {
        /// Whether the click sounds while the transport plays.
        enabled: bool,
        /// Linear click level, `0..=1`.
        gain: f32,
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
    /// Mute other tracks during a recording (R6 D29): bit `n` set = MIDI track
    /// `n` drops its sequenced **note-ons** (offs still fire, so nothing hangs
    /// on the mute edge). `0` = every track plays normally.
    SetRecordMute(u32),
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
    /// Send a controller event to **one instrument node** addressed by
    /// `node_id` (R3: hardware MIDI CC and pitch bend, spec
    /// `.agents/spec/recording.md` §3.2).
    ///
    /// The node-addressed twin of [`EngineCommand::NoteAtNode`]: hardware input
    /// is a live performance source, so it reaches the instrument the input
    /// node is cabled to rather than fanning out through the timeline's track
    /// table. Unknown ids are dropped without panicking.
    CtrlAtNode { node_id: u64, ctrl: RtCtrl },
    /// Send a controller event to a **timeline MIDI track** (R6 D31): the
    /// track-addressed twin of [`EngineCommand::CtrlAtNode`], used when a
    /// hardware input is wired into a track's recording input. Fans out through
    /// the timeline's routing table exactly like [`EngineCommand::Note`], so a
    /// controller follows the same path as the notes it shapes.
    Ctrl { track: usize, ctrl: RtCtrl },
    /// Arm / update a MIDI bake source (R7): the node's event output is copied
    /// into the capture ring for the tracks in the bitmask (bit `n` = track
    /// `n`). `tracks == 0` clears that source.
    SetMidiBake { node_id: u64, tracks: u16 },
    /// Arm / update a control bake source (R8): the node's block control value
    /// is copied into the control capture ring for the curve tracks in the
    /// bitmask. `tracks == 0` clears that source.
    SetCtrlBake { node_id: u64, tracks: u16 },
    /// Clear every bake source (take ended). RT-safe.
    ClearBakes,
    /// Sequencer transport / configuration.
    Sequencer(SequencerCommand),
    /// Enables / disables the capture tap (spec `.agents/spec/recording.md`
    /// D6/D7).
    ///
    /// While enabled, every live event arriving through [`EngineCommand::Note`]
    /// or [`EngineCommand::NoteAtNode`] is copied into the capture ring with
    /// the block-boundary transport position. Sequencer-scheduled events are
    /// never captured — a recording must not re-record the arrangement.
    SetCapture(bool),
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
            EngineCommand::CtrlAtNode { .. } => f32::NAN,
            EngineCommand::Ctrl { .. } => f32::NAN,
            EngineCommand::SetMidiBake { .. } => f32::NAN,
            EngineCommand::SetCtrlBake { .. } => f32::NAN,
            EngineCommand::ClearBakes => f32::NAN,
            EngineCommand::Sequencer(SequencerCommand::SetBpm(value)) => value,
            EngineCommand::Sequencer(_) => f32::NAN,
            EngineCommand::SetCapture(_) => f32::NAN,
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
