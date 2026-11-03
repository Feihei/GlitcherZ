//! The non-real-time control side of the audio engine.
//!
//! [`AudioEngine`] owns the cpal output stream and the producer end of the
//! command queue; all real-time path code lives in
//! [`crate::realtime::BlockRunner`], which is moved into the audio thread when
//! the stream callback starts. UI / control code never touches real-time data
//! directly.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::convert;
use crate::realtime::{
    AutoDest, BlockRunner, COMMAND_QUEUE_CAPACITY, EngineCommand, EventKind, FIXED_BLOCK_SIZE,
    Feedback, GraphCompiler, GraphUpdate, MapSpec, ParamId, Sequencer, SequencerCommand,
    command_channel, recycle_channel, structure_channel,
};

/// Shareable producer end of the command queue.
///
/// The UI thread alone used to own the producer, which made live MIDI input
/// impossible: a hardware input callback runs on the driver's thread and has no
/// way back to the UI except a one-frame hop (16-50 ms of latency before the
/// note even reaches the engine). Wrapping the producer in a mutex lets any
/// control-side thread push straight into the queue, so a played note reaches
/// the audio callback at the next block boundary however it arrived.
///
/// This is **not** on the real-time path — the audio thread only holds the
/// consumer half — so a blocking lock is acceptable here. Uncontended it costs
/// tens of nanoseconds, and the only other contender is the UI thread pushing
/// parameters.
#[derive(Clone)]
pub struct CommandSender(Arc<Mutex<rtrb::Producer<EngineCommand>>>);

impl CommandSender {
    /// Wraps a raw producer. Crate-internal and test-only: production code
    /// always gets its handle from [`AudioEngine::command_sender`].
    #[cfg(test)]
    pub(crate) fn from_producer(producer: rtrb::Producer<EngineCommand>) -> Self {
        Self(Arc::new(Mutex::new(producer)))
    }

    /// Pushes a command; `false` when the queue is full and it was dropped.
    pub fn send(&self, command: EngineCommand) -> bool {
        // A poisoned lock only means some other control-side thread panicked
        // while holding it; the producer itself is still consistent (it is a
        // plain index pair), so recover rather than losing the whole queue.
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(command)
            .is_ok()
    }
}

/// Basic engine info (for UI display).
#[derive(Debug, Clone)]
pub struct AudioEngineInfo {
    /// Output device name ("unknown device" when unavailable).
    pub device_name: String,
    /// Sample rate (Hz).
    pub sample_rate: u32,
    /// Number of output channels.
    pub out_channels: u16,
}

/// Engine error. The application layer can just `Display` it; no fine-grained
/// handling is needed.
/// (Since cpal 0.18, all device/stream errors are unified as `cpal::Error`,
/// which contains an `ErrorKind` internally.)
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("no audio output device available")]
    NoOutputDevice,
    /// R4: the input stream could not be opened; the engine still runs (the
    /// Input node is silent and the UI reports the reason).
    #[error("no audio input device available")]
    NoInputDevice,
    /// R4: the device cannot deliver the engine's sample rate in f32.
    #[error("unsupported audio input config: {0}")]
    InputUnsupported(String),
    #[error("audio device error: {0}")]
    Device(#[from] cpal::Error),
}

/// The audio engine. Dropping [`AudioEngine`] (and thus the internal
/// `cpal::Stream`) stops the output.
pub struct AudioEngine {
    /// Output stream (live engines only; offline engines have `None`).
    _stream: Option<cpal::Stream>,
    commands: CommandSender,
    /// Structure update queue: producer end for graph topology changes
    /// (plan swaps / installing or removing nodes).
    structure: rtrb::Producer<GraphUpdate>,
    /// Node recycling consumer end: the UI periodically drains it via
    /// [`AudioEngine::reclaim_nodes`], releasing old plugin nodes and their
    /// corresponding guards.
    recycle: rtrb::Consumer<crate::realtime::RecycledNode>,
    /// Displaced audio clip tables (U6 D4): drained on the control thread
    /// by [`AudioEngine::reclaim_audio_clips`] — the audio thread only
    /// moves them into this queue, never drops them.
    audio_trash: rtrb::Consumer<crate::realtime::audio_track::AudioClipTable>,
    /// Displaced MIDI clip tables (D15 revision): drained by
    /// [`AudioEngine::reclaim_midi_clips`]. The tables own `Vec`s, so the
    /// audio thread must never drop them.
    midi_trash: rtrb::Consumer<crate::realtime::MidiClipTable>,
    /// Capture ring consumer end (spec `recording.md` D6): live input events
    /// stamped with the transport position, drained by
    /// [`AudioEngine::drain_captured`] on the control thread.
    capture: rtrb::Consumer<crate::realtime::CapturedNote>,
    /// Control capture ring (R8): block-sampled control values baked into a
    /// curve track, drained by [`AudioEngine::drain_captured_controls`].
    capture_control: rtrb::Consumer<crate::realtime::CapturedControl>,
    /// Cumulative count of captured events the ring could not hold. Read by
    /// [`AudioEngine::capture_overflow`] so the UI can warn instead of
    /// silently losing notes.
    capture_overflow: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Control-side graph state (non-real-time): authoritative topology copy +
    /// execution plan compiler. Entirely `Copy`, so the whole snapshot can be
    /// rolled back when a queue push fails.
    compiler: GraphCompiler,
    /// Offline-mode block runner (U7, spec `export-audio.md` D3): the live
    /// engine moves its runner into the cpal callback and leaves `None`
    /// here; an offline engine keeps it on the field so the control thread
    /// can drive blocks via [`AudioEngine::render_offline_block`].
    ///
    /// **Boxed** for the same reason the live runner is: `BlockRunner` is a
    /// large fixed-capacity struct (~50 KB once the graph's per-node event
    /// buffers are included), and an unboxed copy here would also inflate
    /// every by-value move of [`AudioEngine`] itself.
    offline_runner: Option<Box<BlockRunner>>,
    feedback: Feedback,
    info: AudioEngineInfo,
    /// Live audio input (R4): `None` when no input device / unsupported rate.
    /// Dropping the engine closes the stream.
    input: Option<crate::audio_input::AudioInput>,
    /// Why the last input open failed, for the device picker (the picker must
    /// be able to say "this device cannot do 48 kHz f32", not just "no input").
    input_error: Option<String>,
}

impl AudioEngine {
    /// Starts the engine on the host's default output device. **Must be
    /// called on a non-real-time thread** (allocation is allowed during
    /// construction).
    pub fn start() -> Result<Self, EngineError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(EngineError::NoOutputDevice)?;
        let supported = device.default_output_config()?;

        // The data callback always works with f32; if the device's native
        // format differs, cpal converts at the callback boundary.
        log::info!(
            "device default format {:?}, building output stream as f32",
            supported.sample_format()
        );
        let stream_config = supported.config();
        let sample_rate_f32 = convert::sample_rate_to_f32(stream_config.sample_rate);
        let device_name = device.to_string();

        let (command_tx, command_rx) = command_channel(COMMAND_QUEUE_CAPACITY);
        let commands = CommandSender(Arc::new(Mutex::new(command_tx)));

        // Initial processing graph: main output only (P5c, D13 fully
        // explicit). The project document builds the graph step by step via
        // add_node / connect_nodes granularity (UI layer glue).
        let (compiler, graph) = GraphCompiler::empty_graph(sample_rate_f32, FIXED_BLOCK_SIZE);

        let feedback = Feedback::new();
        let sequencer = Sequencer::new(sample_rate_f32);
        let (structure_tx, structure_rx) = structure_channel();
        let (recycle_tx, recycle_rx) = recycle_channel();
        let (audio_trash_tx, audio_trash_rx) =
            crate::realtime::audio_track::audio_clip_trash_channel();
        let (midi_trash_tx, midi_trash_rx) = crate::realtime::midi_clip_trash_channel();
        // Capture tap (spec `recording.md` D6): the audio thread holds the
        // producer, the control side keeps the consumer for `drain_captured`.
        let (capture_tx, capture_rx) =
            crate::realtime::capture_channel(crate::realtime::CAPTURE_QUEUE_CAPACITY);
        let (control_tx, control_rx) =
            crate::realtime::control_capture_channel(crate::realtime::CAPTURE_QUEUE_CAPACITY);
        let capture_overflow = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let capture = crate::realtime::CaptureTap::with_control(
            capture_tx,
            control_tx,
            capture_overflow.clone(),
        );
        // Boxed on purpose: `BlockRunner` is ~50 KB (the graph carries a fixed
        // per-node event buffer table), and cpal's `build_output_stream` passes
        // the callback closure by value through a stack of generic
        // sample-format layers. Each of those layers keeps its own copy, which
        // is fine for a pointer-sized capture but overflows the main thread's
        // 1 MiB stack when the capture is a whole runner (STATUS_STACK_OVERFLOW
        // at startup). The RT path is unaffected: the box is allocated here,
        // before the stream starts.
        let mut runner = Box::new(BlockRunner::new(
            FIXED_BLOCK_SIZE,
            stream_config.channels as usize,
            sample_rate_f32,
            graph,
            sequencer,
            command_rx,
            structure_rx,
            recycle_tx,
            audio_trash_tx,
            midi_trash_tx,
            capture,
            feedback.clone(),
        ));

        // Data callback: all code constrained by the real-time iron rules is
        // concentrated inside BlockRunner.
        let stream = device.build_output_stream(
            stream_config,
            move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                runner.render_interleaved(data);
            },
            move |err| {
                // The error callback is not on the data path (device hotplug
                // etc.), so logging is allowed.
                log::error!("audio stream error: {err}");
            },
            None,
        )?;
        stream.play()?;

        // R4: open the input side best-effort. A machine without a microphone
        // must still run the DAW; the Input node then reports the failure.
        let mut input_error = None;
        let input = match crate::audio_input::AudioInput::open(
            stream_config.sample_rate,
            &crate::audio_input::RecordSource::Default,
        ) {
            Ok(input) => {
                log::info!(
                    "audio input: {} ({} ch @ {} Hz){}",
                    input.info().device_name,
                    input.info().channels,
                    input.info().sample_rate,
                    if input.info().loopback {
                        " [WASAPI loopback]"
                    } else {
                        ""
                    }
                );
                Some(input)
            }
            Err(err) => {
                log::warn!("audio input unavailable: {err}");
                input_error = Some(err.to_string());
                None
            }
        };

        Ok(Self {
            _stream: Some(stream),
            commands,
            structure: structure_tx,
            recycle: recycle_rx,
            audio_trash: audio_trash_rx,
            midi_trash: midi_trash_rx,
            capture: capture_rx,
            capture_control: control_rx,
            capture_overflow,
            compiler,
            offline_runner: None,
            feedback,
            info: AudioEngineInfo {
                device_name,
                sample_rate: stream_config.sample_rate,
                out_channels: stream_config.channels,
            },
            input,
            input_error,
        })
    }

    /// Builds an **offline** engine (U7, spec `export-audio.md` D3): the
    /// same queue/compiler/runner/feedback assembly as [`AudioEngine::start`]
    /// minus the cpal device and stream — no audio hardware is touched, so
    /// this works on headless machines. The runner stays on the
    /// `offline_runner` field; the caller drives blocks with
    /// [`AudioEngine::render_offline_block`] and reads progress through the
    /// usual feedback methods (`musical_ticks` / `is_playing`).
    ///
    /// Output is fixed stereo at `sample_rate`. **Must be called on a
    /// non-real-time thread** (allocation during construction).
    pub fn start_offline(sample_rate: u32) -> Result<Self, EngineError> {
        let sample_rate_f32 = convert::sample_rate_to_f32(sample_rate);
        let (command_tx, command_rx) = command_channel(COMMAND_QUEUE_CAPACITY);
        let commands = CommandSender(Arc::new(Mutex::new(command_tx)));
        let (compiler, graph) = GraphCompiler::empty_graph(sample_rate_f32, FIXED_BLOCK_SIZE);
        let feedback = Feedback::new();
        let sequencer = Sequencer::new(sample_rate_f32);
        let (structure_tx, structure_rx) = structure_channel();
        let (recycle_tx, recycle_rx) = recycle_channel();
        let (audio_trash_tx, audio_trash_rx) =
            crate::realtime::audio_track::audio_clip_trash_channel();
        let (midi_trash_tx, midi_trash_rx) = crate::realtime::midi_clip_trash_channel();
        // Same capture plumbing as the live engine, permanently disabled: the
        // offline engine exists to render a mix, and nothing in the export
        // path ever sends `SetCapture`.
        let (capture_tx, capture_rx) =
            crate::realtime::capture_channel(crate::realtime::CAPTURE_QUEUE_CAPACITY);
        let (control_tx, control_rx) =
            crate::realtime::control_capture_channel(crate::realtime::CAPTURE_QUEUE_CAPACITY);
        let capture_overflow = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let runner = Box::new(BlockRunner::new(
            FIXED_BLOCK_SIZE,
            2,
            sample_rate_f32,
            graph,
            sequencer,
            command_rx,
            structure_rx,
            recycle_tx,
            audio_trash_tx,
            midi_trash_tx,
            crate::realtime::CaptureTap::with_control(
                capture_tx,
                control_tx,
                capture_overflow.clone(),
            ),
            feedback.clone(),
        ));
        Ok(Self {
            _stream: None,
            commands,
            structure: structure_tx,
            recycle: recycle_rx,
            audio_trash: audio_trash_rx,
            midi_trash: midi_trash_rx,
            capture: capture_rx,
            capture_control: control_rx,
            capture_overflow,
            compiler,
            offline_runner: Some(runner),
            feedback,
            info: AudioEngineInfo {
                device_name: "offline".to_string(),
                sample_rate,
                out_channels: 2,
            },
            input: None,
            input_error: None,
        })
    }

    /// Renders one block (or any chunk of frames — the runner splits into
    /// fixed blocks internally) of the offline engine into `out`
    /// (interleaved stereo, `len = frames * 2`). Returns `false` when this
    /// is a live engine (no offline runner) and nothing was rendered.
    ///
    /// The same path the cpal callback takes: commands and structure
    /// updates are consumed at block boundaries, the sequencer advances,
    /// automation is evaluated, and the graph processes. **Control thread
    /// only** (the offline runner is not shared with any callback).
    pub fn render_offline_block(&mut self, out: &mut [f32]) -> bool {
        match self.offline_runner.as_mut() {
            Some(runner) => {
                runner.render_interleaved(out);
                true
            }
            None => false,
        }
    }

    /// Sends a control command to the audio thread (takes effect at the next
    /// block boundary).
    /// Returns `false` when the queue is full and the command was dropped
    /// (non-critical path, safe to ignore).
    pub fn send(&mut self, command: EngineCommand) -> bool {
        self.commands.send(command)
    }

    /// Clone of the shareable command producer, for control-side threads other
    /// than the UI (hardware MIDI input, R3).
    ///
    /// Commands sent this way are indistinguishable from the UI's: they take
    /// effect at the next block boundary and are captured by the tap when it is
    /// enabled.
    pub fn command_sender(&self) -> CommandSender {
        self.commands.clone()
    }

    /// Peak level of the most recent audio block (max of both channels,
    /// roughly 0..1).
    pub fn peak_level(&self) -> f32 {
        self.feedback.level.load()
    }

    /// Peak level of the most recent audio block per channel, `[L, R]`
    /// (roughly 0..1 each) — the transport's stereo meter.
    pub fn master_peak_lr(&self) -> [f32; 2] {
        [self.feedback.level_l.load(), self.feedback.level_r.load()]
    }

    /// Current step index of the sequencer (0..16, stays at the last position
    /// when not playing).
    pub fn current_step(&self) -> u32 {
        self.feedback.step.load(Ordering::Relaxed)
    }

    /// Musical position of the sequencer (ticks, accumulated from the play
    /// start; used for the piano-roll playhead).
    pub fn musical_ticks(&self) -> f32 {
        self.feedback.position_ticks.load()
    }

    /// Whether the sequencer is playing.
    pub fn is_playing(&self) -> bool {
        self.feedback.playing.load(Ordering::Relaxed)
    }

    /// Effective BPM of the most recent block (U3b): a tempo curve overrides
    /// the transport field, so this can differ from the document's
    /// `metadata.tempo_bpm` while automation is driving the clock.
    pub fn live_bpm(&self) -> f32 {
        self.feedback.bpm.load()
    }

    /// Swaps the instrument of the given track to an external node (e.g. a
    /// CLAP instrument).
    /// The node is constructed by the caller on a **non-real-time thread**;
    /// the audio thread swaps it in at the next block boundary, and the
    /// replaced old node is sent back through the receipt queue, to be
    /// returned to the control thread for release by
    /// [`AudioEngine::reclaim_nodes`].
    /// Returns the allocated node ID; `None` means out of bounds or queue
    /// full (command dropped).
    pub fn set_external_voice(
        &mut self,
        track: usize,
        node: Box<dyn crate::realtime::Processor>,
    ) -> Option<u64> {
        let snapshot = self.compiler;
        match self.compiler.set_voice(track, node) {
            Ok((update, node_id)) => {
                if self.structure.push(update).is_ok() {
                    Some(node_id)
                } else {
                    // Queue full: roll back the compiler snapshot (the update
                    // is dropped along with the node).
                    self.compiler = snapshot;
                    None
                }
            }
            Err(node) => {
                drop(node);
                None
            }
        }
    }

    /// Appends an external node to the end of the given track's effect chain
    /// (e.g. a CLAP effect).
    /// Returns the allocated node ID; `None` means out of bounds / slot full /
    /// queue full (node released).
    pub fn insert_external_effect(
        &mut self,
        track: usize,
        node: Box<dyn crate::realtime::Processor>,
    ) -> Option<u64> {
        let snapshot = self.compiler;
        match self.compiler.insert_effect(track, node) {
            Ok((update, node_id)) => {
                if self.structure.push(update).is_ok() {
                    Some(node_id)
                } else {
                    self.compiler = snapshot;
                    None
                }
            }
            Err(node) => {
                drop(node);
                None
            }
        }
    }

    /// Clears all external effect nodes of the given track (nodes are
    /// recycled through the receipt queue).
    pub fn clear_external_effects(&mut self, track: usize) -> bool {
        let snapshot = self.compiler;
        match self.compiler.clear_effects(track) {
            Ok(update) => {
                if self.structure.push(update).is_ok() {
                    true
                } else {
                    self.compiler = snapshot;
                    false
                }
            }
            Err(_) => false,
        }
    }

    /// Drains the node recycling receipt queue: releases old nodes (including
    /// output buffers) on the **control thread** and returns their IDs (the
    /// caller uses them to drop the paired plugin guards, safely unloading
    /// dynamic libraries).
    pub fn reclaim_nodes(&mut self) -> Vec<u64> {
        let mut ids = Vec::new();
        while let Ok(recycled) = self.recycle.pop() {
            let crate::realtime::RecycledNode {
                node_id,
                node,
                output,
                history,
            } = recycled;
            drop(node); // Released on the control thread: the real-time iron rule only constrains the audio callback
            drop(output);
            drop(history);
            ids.push(node_id);
        }
        ids
    }

    /// Replaces one audio-track node's clip table (U6, spec
    /// `.agents/spec/audio-track.md` D4). The table (built by the caller
    /// from the document's `AudioTrack::clips`) rides the structure queue;
    /// the displaced old table comes back through the trash queue and is
    /// released by [`AudioEngine::reclaim_audio_clips`]. Returns `false`
    /// if the queue is full (the update is dropped — retry on the next
    /// edit; playback keeps the old table).
    pub fn send_audio_clips(
        &mut self,
        node_id: u64,
        clips: crate::realtime::audio_track::AudioClipTable,
    ) -> bool {
        self.structure
            .push(GraphUpdate::SetAudioClips { node_id, clips })
            .is_ok()
    }

    /// Drains the displaced-audio-clip trash queue (U6 D4): drops old tables
    /// on the **control thread** (this is the only place their `Arc`
    /// refcounts may reach zero). The UI calls this every frame alongside
    /// `reclaim_nodes`.
    pub fn reclaim_audio_clips(&mut self) {
        while let Ok(_table) = self.audio_trash.pop() {
            // Dropped here: control-thread deallocation only.
        }
    }

    /// Replaces one MIDI track's clip table (D15 revision, 2026-10-28).
    ///
    /// The table (built by the caller from the document's `MidiTrack::clips`)
    /// carries control-side-owned note/controller `Vec`s, so it rides the
    /// **structure queue** instead of the `Copy` command queue — that is what
    /// removed the old 64-notes-per-clip cap. The displaced old table comes
    /// back through the trash queue and is released by
    /// [`AudioEngine::reclaim_midi_clips`]. Returns `false` if the queue is
    /// full (the update is dropped — retry on the next edit; playback keeps
    /// the old table).
    pub fn send_midi_clips(&mut self, track: usize, clips: crate::realtime::MidiClipTable) -> bool {
        self.structure
            .push(GraphUpdate::SetMidiClips { track, clips })
            .is_ok()
    }

    /// Drains the displaced-MIDI-clip trash queue (D15 revision): drops old
    /// tables (and their `Vec` allocations) on the **control thread**. The UI
    /// calls this every frame alongside `reclaim_audio_clips`.
    pub fn reclaim_midi_clips(&mut self) {
        while let Ok(_table) = self.midi_trash.pop() {
            // Dropped here: control-thread deallocation only.
        }
    }

    /// Restores the given track's instrument to the built-in synthesizer (the
    /// old external node is recycled through the receipt queue; it must be
    /// re-instantiated when switching back to the plugin).
    pub fn set_builtin_voice(&mut self, track: usize) -> bool {
        let snapshot = self.compiler;
        match self.compiler.activate_builtin_voice(track) {
            Ok(update) => {
                if self.structure.push(update).is_ok() {
                    true
                } else {
                    self.compiler = snapshot;
                    false
                }
            }
            Err(_) => false,
        }
    }

    /// Replaces one control track's clip table wholesale (empty table = clear;
    /// out-of-bounds indices are ignored on the real-time side). The curves
    /// have no target — the routing endpoint is set separately via
    /// [`AudioEngine::set_auto_route`] (D7/D17).
    ///
    /// D15 revision (2026-10-28): the table rides the **structure queue**, like
    /// the MIDI clip tables. It owns no heap, but at 32 clips it is ~17 KB —
    /// far too big for the `Copy` command queue. Returns `false` when the
    /// queue is full (the update is dropped; the next edit re-sends it).
    pub fn set_control_clips(
        &mut self,
        index: usize,
        clips: crate::realtime::ControlClipTable,
    ) -> bool {
        self.structure
            .push(GraphUpdate::SetControlClips { index, clips })
            .is_ok()
    }

    /// Timeline wiring: connects automation track `auto_index` to a target
    /// node parameter (**node_id addressing**, P5b). Calling again
    /// re-routes.
    pub fn set_auto_route(
        &mut self,
        auto_index: usize,
        node_id: u64,
        param: ParamId,
        map: MapSpec,
    ) -> bool {
        self.apply_plan_update(|compiler| compiler.set_auto_route(auto_index, node_id, param, map))
    }

    /// Timeline wiring: routes automation track `auto_index` to a **MIDI CC**
    /// on `node_id` (U4a): the curve value is quantized to 7 bits and emitted
    /// into the node's MIDI input instead of being written as a parameter.
    pub fn set_auto_cc_route(
        &mut self,
        auto_index: usize,
        node_id: u64,
        channel: u8,
        controller: u8,
        map: MapSpec,
    ) -> bool {
        self.apply_plan_update(|compiler| {
            compiler.set_auto_cc_route(auto_index, node_id, channel, controller, map)
        })
    }

    /// Timeline wiring: routes automation track `auto_index` to a **pitch
    /// bend** on `node_id` (P1): the curve carries semitones and the graph
    /// quantizes to 14 bits using `range_semitones`.
    pub fn set_auto_pitch_bend_route(
        &mut self,
        auto_index: usize,
        node_id: u64,
        channel: u8,
        range_semitones: f32,
        map: MapSpec,
    ) -> bool {
        self.apply_plan_update(|compiler| {
            compiler.set_auto_pitch_bend_route(auto_index, node_id, channel, range_semitones, map)
        })
    }

    /// Timeline wiring: connects automation track `auto_index` to the
    /// **Timeline In** endpoint (U3b): the curve overrides the transport BPM
    /// per block instead of driving a node parameter.
    pub fn set_timeline_in_route(&mut self, auto_index: usize, map: MapSpec) -> bool {
        self.apply_plan_update(|compiler| compiler.set_timeline_in_route(auto_index, map))
    }

    /// Timeline wiring: disconnects automation track `auto_index`.
    pub fn clear_auto_route(&mut self, auto_index: usize) -> bool {
        self.apply_plan_update(|compiler| compiler.clear_auto_route(auto_index))
    }

    /// Timeline wiring: additionally fans out note events of MIDI track
    /// `from_track` to the target node (**node_id addressing**, P5b; the
    /// default graph already includes the track's self-connection).
    /// Repeated connections are idempotent; returns `false` when the fan-out
    /// limit is exceeded.
    pub fn connect_midi_node(&mut self, from_track: usize, node_id: u64) -> bool {
        self.apply_plan_update(|compiler| compiler.connect_midi(from_track, node_id))
    }

    /// Timeline wiring: disconnects the fan-out from MIDI track `from_track`
    /// to the target node (the track's self-connection is part of the default
    /// graph and cannot be disconnected).
    pub fn disconnect_midi_node(&mut self, from_track: usize, node_id: u64) -> bool {
        self.apply_plan_update(|compiler| compiler.disconnect_midi(from_track, node_id))
    }

    /// Sets a node parameter directly by node_id (P5b: the public
    /// node-addressed path).
    /// Returns `false` when the queue is full (command dropped, non-critical
    /// path).
    pub fn set_node_param(&mut self, node_id: u64, id: ParamId, value: f32) -> bool {
        self.send(EngineCommand::SetNodeParam { node_id, id, value })
    }

    /// The node_id of a track's instrument (used by the UI to resolve track
    /// coordinates into node addressing).
    pub fn voice_node_id(&self, track: usize) -> Option<u64> {
        self.compiler.track_voice_node_id(track)
    }

    /// The node_id of a track's delay node.
    pub fn delay_node_id(&self, track: usize) -> Option<u64> {
        self.compiler.track_delay_node_id(track)
    }

    /// The node_id of a track's channel strip.
    pub fn channel_node_id(&self, track: usize) -> Option<u64> {
        self.compiler.track_channel_node_id(track)
    }

    /// The node_id of the master output node.
    pub fn master_node_id(&self) -> u64 {
        self.compiler.master_node_id()
    }

    /// The node_id of the first node of the given kind (P5f: the UI finds the
    /// global mixer node this way).
    pub fn node_id_of_kind(&self, kind: crate::realtime::NodeKind) -> Option<u64> {
        self.compiler.node_id_of_kind(kind)
    }

    /// Adds a MIDI track (default pair + the track's MIDI self-connection;
    /// P5f D18: mixing happens on explicit mixer nodes — the caller wires
    /// `delay → mixer.ch[i]`). Returns the two new node_ids `[voice, delay]`;
    /// returns `None` when track/slot capacity is full (the compiler has been
    /// rolled back).
    pub fn add_midi_track(&mut self) -> Option<[u64; 2]> {
        let snapshot = self.compiler;
        match self.compiler.add_midi_track() {
            Ok((update, ids)) => {
                if self.structure.push(update).is_ok() {
                    Some(ids)
                } else {
                    self.compiler = snapshot;
                    None
                }
            }
            Err(_) => None,
        }
    }

    /// Tears down a MIDI track (the trio is recycled together with its
    /// effects).
    pub fn remove_midi_track(&mut self, track: usize) -> bool {
        self.apply_plan_update(|compiler| compiler.remove_midi_track(track))
    }

    /// Sets the number of active MIDI tracks (D12; clamped to the capacity
    /// limit on the real-time side).
    pub fn set_midi_track_count(&mut self, count: usize) -> bool {
        // Occupancy is decoupled from the node slots: a bare track still
        // exists even when the user has not built its instrument yet.
        self.compiler.set_present_track_count(count);
        self.send(EngineCommand::Sequencer(
            SequencerCommand::SetMidiTrackCount(count),
        ))
    }

    /// Sets the arrangement-end floor (U7 export): the auto-stop never
    /// fires before this tick. The offline export uses it to keep
    /// audio-track material ending past the MIDI arrangement running; the
    /// live engine never sets it.
    pub fn set_arrangement_floor(&mut self, floor_ticks: u32) -> bool {
        self.send(EngineCommand::Sequencer(
            SequencerCommand::SetArrangementFloor(floor_ticks),
        ))
    }

    /// Pauses the transport: scheduling stops, the musical position stays
    /// (resume with `SequencerCommand::Play`). U1.
    pub fn pause(&mut self) -> bool {
        self.send(EngineCommand::Sequencer(SequencerCommand::Pause))
    }

    /// Moves the transport to `ticks` (musical position, ≥ 0). U1.
    pub fn seek_ticks(&mut self, ticks: f64) -> bool {
        self.send(EngineCommand::Sequencer(SequencerCommand::Seek(ticks)))
    }

    /// Sets the song loop range (U1); `end == 0` or `end <= start` disables
    /// looping.
    pub fn set_loop_range(&mut self, start_ticks: u32, end_ticks: u32) -> bool {
        self.send(EngineCommand::Sequencer(SequencerCommand::SetLoopRange {
            start_ticks,
            end_ticks,
        }))
    }

    /// Sets the document's meter (U3a): the derived bar length feeds the
    /// arrangement-end rounding. No event moves and the transport keeps its
    /// position.
    pub fn set_time_signature(&mut self, numerator: u8, denominator: u8) -> bool {
        self.send(EngineCommand::Sequencer(
            SequencerCommand::SetTimeSignature {
                numerator,
                denominator,
            },
        ))
    }

    /// Enables / configures the transport metronome (`spec/recording.md`).
    ///
    /// The click is mixed after the graph, so it survives muting the master
    /// chain and never enters automation or the exported audio. Returns
    /// `false` if the command queue is full.
    pub fn set_metronome(&mut self, enabled: bool, gain: f32) -> bool {
        self.send(EngineCommand::Sequencer(SequencerCommand::SetMetronome {
            enabled,
            gain,
        }))
    }

    /// Records while monitoring: mute the sequenced notes of every MIDI track
    /// whose bit is set (R6 D29). Bit `n` = track `n`; `0` restores normal
    /// playback. Returns `false` if the command queue is full (retry next
    /// frame).
    pub fn set_record_mute(&mut self, bits: u32) -> bool {
        self.send(EngineCommand::Sequencer(SequencerCommand::SetRecordMute(
            bits,
        )))
    }

    /// Enables / disables the capture tap (spec `.agents/spec/recording.md`
    /// D6/D7).
    ///
    /// While enabled, every live note event that reaches the audio thread is
    /// stamped with the block-boundary transport position and queued for
    /// [`AudioEngine::drain_captured`]. Sequencer-scheduled events are never
    /// captured, so a recording does not re-record the arrangement. Returns
    /// `false` if the command queue is full (retry next frame).
    pub fn set_capture(&mut self, enabled: bool) -> bool {
        self.send(EngineCommand::SetCapture(enabled))
    }

    /// R4: description of the live input device (device name, channels, rate),
    /// or `None` when no input stream could be opened.
    pub fn input_info(&self) -> Option<&crate::audio_input::AudioInputInfo> {
        self.input.as_ref().map(|input| input.info())
    }

    /// Every recording source the picker offers (R9): capture endpoints, then
    /// render endpoints as WASAPI loopback. **Control thread only.**
    pub fn record_sources() -> Vec<crate::audio_input::RecordSource> {
        crate::audio_input::record_sources()
    }

    /// Why the current input stream is not open (`None` while it is).
    pub fn input_error(&self) -> Option<&str> {
        self.input_error.as_deref()
    }

    /// Swaps the live input stream for `source`.
    ///
    /// The old stream is dropped first: two open streams on one device is
    /// what makes a microphone unusable on Windows, and the graph's
    /// `InputProcessor` keeps its ring clone either way — the caller rebuilds
    /// the graph so the node latches onto the new ring.
    pub fn reopen_input(
        &mut self,
        source: &crate::audio_input::RecordSource,
    ) -> Result<(), String> {
        self.input = None;
        let rate = self.info.sample_rate;
        match crate::audio_input::AudioInput::open(rate, source) {
            Ok(input) => {
                log::info!(
                    "audio input: {} ({} ch @ {} Hz){}",
                    input.info().device_name,
                    input.info().channels,
                    input.info().sample_rate,
                    if input.info().loopback {
                        " [WASAPI loopback]"
                    } else {
                        ""
                    }
                );
                self.input = Some(input);
                self.input_error = None;
                Ok(())
            }
            Err(err) => {
                let message = err.to_string();
                log::warn!("audio input unavailable: {message}");
                self.input_error = Some(message.clone());
                Err(message)
            }
        }
    }

    /// R4: monitor ring handle for the graph's `InputProcessor` (a new clone
    /// each call; all clones address the same queue). `None` = no input.
    pub fn input_ring(&self) -> Option<crate::audio_input::InputRing> {
        self.input.as_ref().map(|input| input.monitor_ring())
    }

    /// R4: shared input counters (peak / drops / underruns), if live.
    pub fn input_stats(&self) -> Option<std::sync::Arc<crate::audio_input::InputStats>> {
        self.input.as_ref().map(|input| input.stats())
    }

    /// R4: feed / stop feeding the monitor ring. Disabling clears what was
    /// queued so a later node never starts on stale samples.
    pub fn set_input_monitor(&mut self, enabled: bool) {
        if let Some(input) = &self.input {
            input.set_monitor_enabled(enabled);
        }
    }

    /// R4: arms the take ring. Enabling clears stale samples first.
    pub fn set_audio_capture(&mut self, enabled: bool) {
        if let Some(input) = &self.input {
            input.set_record_enabled(enabled);
        }
    }

    /// R4: drains the take ring into `out` (never clears `out`); returns the
    /// number of samples moved. **Control thread only.**
    pub fn drain_recorded_audio(&mut self, out: &mut Vec<f32>) -> usize {
        self.input
            .as_ref()
            .map_or(0, |input| input.drain_recorded(out))
    }

    /// Appends everything the audio thread captured since the last call to
    /// `out` (**never clears it** — callers reuse one buffer per frame and
    /// clear it themselves). **Control thread only**: it is the single
    /// consumer of the ring.
    pub fn drain_captured(&mut self, out: &mut Vec<crate::realtime::CapturedNote>) {
        while let Ok(captured) = self.capture.pop() {
            out.push(captured);
        }
    }

    /// R8: appends every block-sampled control value captured since the last
    /// call. **Control thread only.**
    pub fn drain_captured_controls(&mut self, out: &mut Vec<crate::realtime::CapturedControl>) {
        while let Ok(captured) = self.capture_control.pop() {
            out.push(captured);
        }
    }

    /// R7: arms / updates a MIDI bake source for a track bitmask (`0` clears
    /// that source). Returns `false` when the command queue is full.
    pub fn set_midi_bake(&mut self, node_id: u64, tracks: u16) -> bool {
        self.send(EngineCommand::SetMidiBake { node_id, tracks })
    }

    /// R8: arms / updates a control bake source for a track bitmask.
    pub fn set_ctrl_bake(&mut self, node_id: u64, tracks: u16) -> bool {
        self.send(EngineCommand::SetCtrlBake { node_id, tracks })
    }

    /// Clears every bake source. Returns `false` when the queue is full.
    pub fn clear_bakes(&mut self) -> bool {
        self.send(EngineCommand::ClearBakes)
    }

    /// Cumulative number of captured events the ring dropped because it was
    /// full (0 = nothing lost). Cumulative on purpose: the UI compares it
    /// against the previous read to report "N events dropped".
    pub fn capture_overflow(&self) -> u64 {
        self.capture_overflow.load(Ordering::Relaxed)
    }

    // ---- Document-driven graph building API (P5c, D13: the loader rebuilds
    // the engine graph from the document) ----

    /// Allocates the next session-monotonic node_id (taken before a document
    /// node is installed; the mapping to document node_ids is maintained by
    /// the caller).
    pub fn fresh_node_id(&mut self) -> u64 {
        self.compiler.fresh_node_id()
    }

    /// Installs a document node (explicit node_id, D11). `kind` sets the
    /// node's compile-time input discipline (P5f D14/D16: only mixer nodes
    /// accept multiple in-edges). Returns `false` for
    /// an invalid id / duplicate id / full slots / full queue (the node is
    /// released, the compiler has been rolled back).
    pub fn add_node(
        &mut self,
        node_id: u64,
        kind: crate::realtime::NodeKind,
        node: Box<dyn crate::realtime::Processor>,
    ) -> bool {
        let snapshot = self.compiler;
        match self.compiler.add_node(node_id, kind, node) {
            Ok(update) => {
                if self.structure.push(update).is_ok() {
                    true
                } else {
                    self.compiler = snapshot;
                    false
                }
            }
            Err(_) => false,
        }
    }

    /// Assembles the track addressing table (mapping from document trio
    /// node_ids to the legacy track-coordinate API).
    pub fn register_track_chain(
        &mut self,
        track: usize,
        voice: Option<u64>,
        delay: Option<u64>,
        channel: Option<u64>,
    ) -> bool {
        self.apply_plan_update(|compiler| {
            compiler.register_track_chain(track, voice, delay, channel)
        })
    }

    /// Audio connection between nodes (node_id addressing). `in_port` is the
    /// destination mixer channel (D17; 0 for non-mixer targets). Repeated
    /// connections are idempotent.
    pub fn connect_nodes(&mut self, from_id: u64, to_id: u64, in_port: u8) -> bool {
        self.apply_plan_update(|compiler| compiler.connect_nodes(from_id, to_id, in_port))
    }

    /// Disconnects an audio connection between nodes (idempotent when not
    /// connected).
    pub fn disconnect_nodes(&mut self, from_id: u64, to_id: u64) -> bool {
        self.apply_plan_update(|compiler| compiler.disconnect_nodes(from_id, to_id))
    }

    /// Connects a node-to-node event/control cable (spec `midi-nodes.md` D4;
    /// node_id addressing). `kind` selects MIDI events or one control value
    /// per block delivered as `dest`. Repeated connections are idempotent;
    /// returns `false` on cycle / fan-in / fan-out / capacity errors or a
    /// full structure queue.
    pub fn connect_event(
        &mut self,
        from_id: u64,
        to_id: u64,
        kind: EventKind,
        dest: AutoDest,
        map: MapSpec,
    ) -> bool {
        self.apply_plan_update(|compiler| compiler.connect_event(from_id, to_id, kind, dest, map))
    }

    /// Removes a node-to-node event/control cable (idempotent when not
    /// connected).
    pub fn disconnect_event(&mut self, from_id: u64, to_id: u64, kind: EventKind) -> bool {
        self.apply_plan_update(|compiler| compiler.disconnect_event(from_id, to_id, kind))
    }

    /// Replaces a node's processor in place (**node_id preserved**, D11). The
    /// old processor is released via the recycling receipt; returns the
    /// preserved node_id, `None` = node does not exist / queue full (the node
    /// is released, the compiler has been rolled back).
    pub fn replace_node(
        &mut self,
        node_id: u64,
        node: Box<dyn crate::realtime::Processor>,
    ) -> Option<u64> {
        let snapshot = self.compiler;
        match self.compiler.replace_node(node_id, node) {
            Ok((update, kept)) => {
                if self.structure.push(update).is_ok() {
                    Some(kept)
                } else {
                    self.compiler = snapshot;
                    None
                }
            }
            Err(node) => {
                drop(node);
                None
            }
        }
    }

    /// Inserts a node (explicit node_id) between an existing connection
    /// `from_id → to_id`.
    pub fn insert_node_between(
        &mut self,
        new_id: u64,
        from_id: u64,
        to_id: u64,
        node: Box<dyn crate::realtime::Processor>,
    ) -> bool {
        let snapshot = self.compiler;
        match self
            .compiler
            .insert_node_between(new_id, from_id, to_id, node)
        {
            Ok(update) => {
                if self.structure.push(update).is_ok() {
                    true
                } else {
                    self.compiler = snapshot;
                    false
                }
            }
            Err(node) => {
                drop(node);
                false
            }
        }
    }

    /// Removes a node (along with its incoming/outgoing connections and
    /// routing table entries). The master output cannot be removed (D6).
    pub fn remove_node(&mut self, node_id: u64) -> bool {
        self.apply_plan_update(|compiler| compiler.remove_node(node_id))
    }

    /// Applies a plan-only topology change (connection table edits etc.),
    /// rolling back when the queue is full.
    fn apply_plan_update(
        &mut self,
        edit: impl FnOnce(&mut GraphCompiler) -> Result<GraphUpdate, crate::realtime::TopologyError>,
    ) -> bool {
        let snapshot = self.compiler;
        match edit(&mut self.compiler) {
            Ok(update) => {
                if self.structure.push(update).is_ok() {
                    true
                } else {
                    self.compiler = snapshot;
                    false
                }
            }
            Err(_) => {
                self.compiler = snapshot;
                false
            }
        }
    }

    /// Post-fader peak level of the given track (returns 0 when out of
    /// bounds).
    pub fn track_level(&self, track: usize) -> f32 {
        self.feedback
            .track_levels
            .get(track)
            .map_or(0.0, |level| level.load())
    }

    /// Playback position (seconds, f64: time calculations use f64).
    pub fn position_seconds(&self) -> f64 {
        convert::samples_to_seconds(
            self.feedback.position.load(Ordering::Relaxed),
            convert::sample_rate_to_f32(self.info.sample_rate),
        )
    }

    /// Engine info.
    pub fn info(&self) -> &AudioEngineInfo {
        &self.info
    }
}

/// Enumerates the host's output device names (for diagnostics,
/// non-real-time).
pub fn available_outputs() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.devices() else {
        return Vec::new();
    };
    devices.map(|d| d.to_string()).collect()
}
