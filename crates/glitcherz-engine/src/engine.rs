//! The non-real-time control side of the audio engine.
//!
//! [`AudioEngine`] owns the cpal output stream and the producer end of the
//! command queue; all real-time path code lives in
//! [`crate::realtime::BlockRunner`], which is moved into the audio thread when
//! the stream callback starts. UI / control code never touches real-time data
//! directly.

use std::sync::atomic::Ordering;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::convert;
use crate::realtime::{
    BlockRunner, COMMAND_QUEUE_CAPACITY, EngineCommand, FIXED_BLOCK_SIZE, Feedback, GraphCompiler,
    GraphUpdate, ParamId, Sequencer, SequencerCommand, command_channel, recycle_channel,
    structure_channel,
};

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
    #[error("audio device error: {0}")]
    Device(#[from] cpal::Error),
}

/// The audio engine. Dropping [`AudioEngine`] (and thus the internal
/// `cpal::Stream`) stops the output.
pub struct AudioEngine {
    _stream: cpal::Stream,
    commands: rtrb::Producer<EngineCommand>,
    /// Structure update queue: producer end for graph topology changes
    /// (plan swaps / installing or removing nodes).
    structure: rtrb::Producer<GraphUpdate>,
    /// Node recycling consumer end: the UI periodically drains it via
    /// [`AudioEngine::reclaim_nodes`], releasing old plugin nodes and their
    /// corresponding guards.
    recycle: rtrb::Consumer<crate::realtime::RecycledNode>,
    /// Control-side graph state (non-real-time): authoritative topology copy +
    /// execution plan compiler. Entirely `Copy`, so the whole snapshot can be
    /// rolled back when a queue push fails.
    compiler: GraphCompiler,
    feedback: Feedback,
    info: AudioEngineInfo,
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

        let (commands, command_rx) = command_channel(COMMAND_QUEUE_CAPACITY);

        // Initial processing graph: main output only (P5c, D13 fully
        // explicit). The project document builds the graph step by step via
        // add_node / connect_nodes granularity (UI layer glue).
        let (compiler, graph) = GraphCompiler::empty_graph(sample_rate_f32, FIXED_BLOCK_SIZE);

        let feedback = Feedback::new();
        let sequencer = Sequencer::new(sample_rate_f32);
        let (structure_tx, structure_rx) = structure_channel();
        let (recycle_tx, recycle_rx) = recycle_channel();
        let mut runner = BlockRunner::new(
            FIXED_BLOCK_SIZE,
            stream_config.channels as usize,
            sample_rate_f32,
            graph,
            sequencer,
            command_rx,
            structure_rx,
            recycle_tx,
            feedback.clone(),
        );

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

        Ok(Self {
            _stream: stream,
            commands,
            structure: structure_tx,
            recycle: recycle_rx,
            compiler,
            feedback,
            info: AudioEngineInfo {
                device_name,
                sample_rate: stream_config.sample_rate,
                out_channels: stream_config.channels,
            },
        })
    }

    /// Sends a control command to the audio thread (takes effect at the next
    /// block boundary).
    /// Returns `false` when the queue is full and the command was dropped
    /// (non-critical path, safe to ignore).
    pub fn send(&mut self, command: EngineCommand) -> bool {
        self.commands.push(command).is_ok()
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
            } = recycled;
            drop(node); // Released on the control thread: the real-time iron rule only constrains the audio callback
            drop(output);
            ids.push(node_id);
        }
        ids
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
    pub fn set_control_clips(
        &mut self,
        index: usize,
        clips: [crate::realtime::RtControlClip; crate::realtime::MAX_TRACK_CLIPS],
    ) -> bool {
        self.send(EngineCommand::Sequencer(
            SequencerCommand::SetControlClips { index, clips },
        ))
    }

    /// Timeline wiring: connects automation track `auto_index` to a target
    /// node parameter (**node_id addressing**, P5b). Calling again
    /// re-routes.
    pub fn set_auto_route(&mut self, auto_index: usize, node_id: u64, param: ParamId) -> bool {
        self.apply_plan_update(|compiler| compiler.set_auto_route(auto_index, node_id, param))
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
    ) -> bool {
        self.apply_plan_update(|compiler| {
            compiler.set_auto_cc_route(auto_index, node_id, channel, controller)
        })
    }

    /// Timeline wiring: connects automation track `auto_index` to the
    /// **Timeline In** endpoint (U3b): the curve overrides the transport BPM
    /// per block instead of driving a node parameter.
    pub fn set_timeline_in_route(&mut self, auto_index: usize) -> bool {
        self.apply_plan_update(|compiler| compiler.set_timeline_in_route(auto_index))
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
        self.send(EngineCommand::Sequencer(
            SequencerCommand::SetMidiTrackCount(count),
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
