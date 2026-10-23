//! Fixed-block processing dispatch: bridge between the cpal callback and the mixer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use rtrb::Consumer;

use crate::realtime::atomic::AtomicF32;
use crate::realtime::automation::MAX_AUTO_TRACKS;
use crate::realtime::commands::{EngineCommand, NoteEvent, SequencerCommand};
use crate::realtime::context::ProcessContext;
use crate::realtime::graph::{GraphLevels, GraphUpdate, NO_EDGE, RtGraph, TIMELINE_IN};
use crate::realtime::sequencer::Sequencer;
use crate::realtime::voice::RecycledNode;

/// Engine → UI state feedback handle: a set of atomics shared with [`BlockRunner`].
///
/// `BlockRunner` writes at every block boundary; [`crate::engine::AudioEngine`]
/// holds a clone for the UI to read.
#[derive(Clone)]
pub struct Feedback {
    /// Peak level of the master output in the most recent block, both
    /// channels collapsed (kept for single-value readers; the transport
    /// meter reads `level_l` / `level_r`).
    pub level: Arc<AtomicF32>,
    /// Master output peak of the left channel in the most recent block.
    pub level_l: Arc<AtomicF32>,
    /// Master output peak of the right channel in the most recent block.
    pub level_r: Arc<AtomicF32>,
    /// Playback position (samples since the stream started).
    pub position: Arc<AtomicU64>,
    /// Sequencer's current step index.
    pub step: Arc<AtomicU32>,
    /// Sequencer transport state.
    pub playing: Arc<AtomicBool>,
    /// Sequencer musical position (ticks, accumulated from the play start).
    pub position_ticks: Arc<AtomicF32>,
    /// Effective transport BPM of the most recent block (U3b: reflects a
    /// tempo-curve override, so the UI can show that automation is driving
    /// the clock). Equals the base BPM when no curve covers the playhead.
    pub bpm: Arc<AtomicF32>,
    /// Per-track post-fader peak levels (fixed capacity
    /// [`crate::realtime::MAX_MIDI_TRACKS`], unconfigured tracks always 0).
    pub track_levels: [Arc<AtomicF32>; crate::realtime::MAX_MIDI_TRACKS],
}

impl Feedback {
    /// All-zero initial value. **Not RT-safe**; call before starting the audio stream.
    pub fn new() -> Self {
        Self {
            level: Arc::new(AtomicF32::new(0.0)),
            level_l: Arc::new(AtomicF32::new(0.0)),
            level_r: Arc::new(AtomicF32::new(0.0)),
            position: Arc::new(AtomicU64::new(0)),
            step: Arc::new(AtomicU32::new(0)),
            playing: Arc::new(AtomicBool::new(false)),
            position_ticks: Arc::new(AtomicF32::new(0.0)),
            bpm: Arc::new(AtomicF32::new(120.0)),
            track_levels: std::array::from_fn(|_| Arc::new(AtomicF32::new(0.0))),
        }
    }
}

impl Default for Feedback {
    fn default() -> Self {
        Self::new()
    }
}

/// Splits the arbitrarily long interleaved buffer handed over by the host
/// (cpal) into fixed blocks for the node graph to execute, then interleaves
/// and writes back. Instances are constructed before the audio stream starts,
/// then moved into the audio callback thread,
/// exclusively owning the audio thread for their lifetime.
///
/// Execution order for each fixed block (`run_next_block`):
/// 1. At the **block boundary**, consume all pending commands (return
///    immediately if the queue is empty);
/// 2. Apply structure updates (swap plans / attach-detach nodes, recycling via
///    the receipt queue);
/// 3. Automation tracks are evaluated (only while playing) at the block's
///    start position — before scheduling, so a tempo curve governs the block
///    it was sampled for and node parameters are current when the block runs;
/// 4. The sequencer schedules on the sample clock; due note events fan out
///    along the graph's MIDI routing table;
/// 5. The node graph processes one block in precompiled topological order
///    (sum inputs → process in place);
/// 6. Report peak level, playback position, sequencer step, transport state
///    and the effective BPM (atomics).
pub struct BlockRunner {
    fixed_block: usize,
    out_channels: usize,
    sample_rate: f32,
    graph: RtGraph,
    /// Heap-allocated: with per-track clip tables (timeline T2) the struct is
    /// ~200 KB, far too large for the main thread's stack as a plain field
    /// (`AudioEngine::start` would overflow the 1 MB default). Constructed
    /// once on a non-real-time thread; the audio callback only dereferences.
    sequencer: Box<Sequencer>,
    scratch_pos: usize,
    commands: Consumer<EngineCommand>,
    structure: Consumer<GraphUpdate>,
    recycle: rtrb::Producer<RecycledNode>,
    feedback: Feedback,
    played_samples: u64,
}

impl BlockRunner {
    /// Constructs. **Not RT-safe** (allocates buffers); must be called before
    /// starting the audio stream.
    // All parameters are queue endpoints/handles wired once at startup; little
    // benefit from bundling them.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        fixed_block: usize,
        out_channels: usize,
        sample_rate: f32,
        graph: RtGraph,
        sequencer: Sequencer,
        commands: Consumer<EngineCommand>,
        structure: Consumer<GraphUpdate>,
        recycle: rtrb::Producer<RecycledNode>,
        feedback: Feedback,
    ) -> Self {
        Self {
            fixed_block: fixed_block.max(1),
            out_channels: out_channels.max(1),
            sample_rate: if sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            graph,
            sequencer: Box::new(sequencer),
            scratch_pos: 0,
            commands,
            structure,
            recycle,
            feedback,
            played_samples: 0,
        }
    }

    /// Renders the interleaved output buffer provided by the host. **RT-safe**
    /// (called directly from the cpal data callback).
    ///
    /// Channel mapping: the source is stereo (L/R); output channels 0/1 take
    /// L/R, remaining channels are silent;
    /// mono output takes L.
    pub fn render_interleaved(&mut self, out: &mut [f32]) {
        for frame in out.chunks_mut(self.out_channels) {
            if self.scratch_pos >= self.graph_block_len() {
                self.run_next_block();
            }
            let left = self.sample_at(0, self.scratch_pos);
            let right = self.sample_at(1, self.scratch_pos);
            for (ch, slot) in frame.iter_mut().enumerate() {
                *slot = match ch {
                    0 => left,
                    1 => right,
                    _ => 0.0,
                };
            }
            self.scratch_pos += 1;
        }
    }

    /// Active length of the current master buffer.
    fn graph_block_len(&self) -> usize {
        self.graph
            .master_channel(0)
            .map_or(0, |channel| channel.len())
    }

    /// Executes one fixed block. Should only be called within [`BlockRunner::render_interleaved`].
    fn run_next_block(&mut self) {
        // 1. Block boundary: consume all pending commands and structure updates
        //    (lock-free queues, return immediately if empty; swapping graphs is
        //    zero-allocation).
        while let Ok(cmd) = self.commands.pop() {
            self.apply_command(cmd);
        }
        while let Ok(update) = self.structure.pop() {
            self.graph.apply_update(update, &mut self.recycle);
        }

        // 2. This block's context (shared by the sequencer and the mixer).
        let frames = self.fixed_block;
        let ctx = ProcessContext {
            sample_rate: self.sample_rate,
            frames,
            playback_sample_pos: self.played_samples,
        };

        // 3. Automation evaluation (only while playing) **before** the
        //    sequencer advances (U3b D4): a tempo curve must be sampled at the
        //    block's *start* position so the block it governs actually runs at
        //    the sampled rate — evaluating afterwards would always spend one
        //    block at the stale tempo. Note events are scheduled with the
        //    effective BPM in the same step.
        let position_before = self.sequencer.position_ticks();
        if self.sequencer.is_playing() {
            self.evaluate_automation(position_before);
        }

        // 4. Sequencer scheduling: due note events fan out along the graph's
        //    MIDI routing table to the target instrument nodes (block-boundary
        //    precision). Split field borrows so sequencer and graph can be
        //    mutable at the same time.
        let Self {
            sequencer, graph, ..
        } = self;
        sequencer.advance(&ctx, &mut |track, event| {
            graph.track_note(track, event);
        });

        // 5. The node graph processes one block (topological order: sum inputs → process in place → master output).
        let levels: GraphLevels = self.graph.process_block(&ctx);

        // 6. State feedback.
        self.feedback.level.store(levels.master);
        self.feedback.level_l.store(levels.master_lr[0]);
        self.feedback.level_r.store(levels.master_lr[1]);
        for (slot, value) in self.feedback.track_levels.iter().zip(levels.tracks) {
            slot.store(value);
        }
        self.feedback
            .position
            .store(self.played_samples, Ordering::Relaxed);
        self.feedback
            .step
            .store(self.sequencer.step_index() as u32, Ordering::Relaxed);
        self.feedback
            .playing
            .store(self.sequencer.is_playing(), Ordering::Relaxed);
        self.feedback
            .position_ticks
            .store(self.sequencer.position_ticks() as f32);
        self.feedback.bpm.store(self.sequencer.effective_bpm());

        self.played_samples += frames as u64;
        self.scratch_pos = 0;
    }

    /// Samples and applies all control tracks' clips at `position_ticks`: the
    /// first clip covering the transport position delivers its curve value
    /// (D17: nothing is delivered outside any clip span); the delivery
    /// endpoint is determined by the graph's routing table.
    ///
    /// U3b: a route into the Timeline In sentinel steers the **transport
    /// clock** (tempo override) instead of a node parameter; the override is
    /// cleared first, so a gap between tempo clips drops back to the base BPM
    /// rather than freezing at the last value (D3). Everything is collected
    /// into fixed-size buffers before delivery to keep the sequencer and
    /// graph borrows apart — no allocation (RT rule 1).
    /// RT-safe.
    fn evaluate_automation(&mut self, position_ticks: f64) {
        let Self {
            sequencer, graph, ..
        } = self;
        sequencer.clear_bpm_override();
        let mut tempo: Option<f32> = None;
        let mut node_values = [(0usize, 0.0f32); MAX_AUTO_TRACKS];
        let mut node_count = 0usize;
        for (index, clips) in sequencer.control_clips().iter().enumerate() {
            for clip in clips {
                if let Some(value) = clip.value_at_absolute(position_ticks) {
                    match graph.auto_route(index) {
                        Some(route) if route.slot == TIMELINE_IN => tempo = Some(value),
                        Some(route) if route.slot != NO_EDGE => {
                            node_values[node_count] = (index, value);
                            node_count += 1;
                        }
                        _ => {}
                    }
                    break;
                }
            }
        }
        for &(index, value) in &node_values[..node_count] {
            graph.auto_set_param(index, value);
        }
        if let Some(bpm) = tempo {
            sequencer.set_bpm_override(bpm);
        }
    }

    fn apply_command(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::SetParam { id, value } => {
                // Unrecognized parameters are silently dropped (the sender is
                // responsible for sending only valid parameters).
                let _ = self.graph.master_set_param(id, value);
            }
            EngineCommand::SetTrackParam { track, id, value } => {
                self.graph.track_set_param(track, id, value);
            }
            EngineCommand::SetNodeParam { node_id, id, value } => {
                self.graph.node_set_param(node_id, id, value);
            }
            EngineCommand::Note { track, event } => {
                self.graph.track_note(track, event);
            }
            EngineCommand::NoteAtNode { node_id, event } => {
                // G6: virtual-keyboard route — the addressed instrument only.
                let _ = self.graph.node_note(node_id, event);
            }
            EngineCommand::Sequencer(cmd) => self.apply_sequencer_command(cmd),
            EngineCommand::Panic => {
                // Panic = silence, not stop: the transport keeps running and
                // every parameter is left alone. Pending sequencer offs are
                // released first (by pitch), then the graph sweeps the rest.
                self.release_pending_notes();
                self.graph.panic();
            }
        }
    }

    /// Releases the sequencer's taken pending notes **by note number**
    /// (pitch-matched release; see [`Sequencer::take_pending_offs`]). RT-safe.
    fn release_pending_notes(&mut self) {
        for (track, slots) in self.sequencer.take_pending_offs().into_iter().enumerate() {
            for (_, note) in slots.into_iter().flatten() {
                self.graph.track_note(track, NoteEvent::Off { note });
            }
        }
    }

    fn apply_sequencer_command(&mut self, cmd: SequencerCommand) {
        match cmd {
            SequencerCommand::Play => {
                self.sequencer.play();
                // Release the previous run's still-pending notes **by pitch**
                // (see `take_pending_offs`), then a blanket off for the
                // built-in chain — clearing manual notes that may still be
                // held, avoiding overlap with sequencer voices.
                self.release_pending_notes();
                self.graph.note_off_all();
            }
            SequencerCommand::Stop => {
                self.sequencer.stop();
                // Same pitch-matched release: a blanket `Off { note: 0 }`
                // alone never reaches a pitch-matching plugin (Dexed), whose
                // sounding notes then hang forever.
                self.release_pending_notes();
                self.graph.note_off_all();
            }
            SequencerCommand::Pause => {
                // U1: the position stays (resume via Play); sounding notes
                // still fall silent — same release semantics as Stop.
                self.sequencer.pause();
                self.release_pending_notes();
                self.graph.note_off_all();
            }
            SequencerCommand::Seek(ticks) => {
                // U1: nothing scheduled relative to the old position may
                // fire at the new one (the sequencer clears its pending
                // offs); sounding notes release like a transport jump.
                self.sequencer.seek(ticks);
                self.release_pending_notes();
                self.graph.note_off_all();
            }
            SequencerCommand::SetLoopRange {
                start_ticks,
                end_ticks,
            } => {
                self.sequencer.set_loop_range(start_ticks, end_ticks);
            }
            SequencerCommand::SetBpm(bpm) => self.sequencer.set_bpm(bpm),
            SequencerCommand::SetTimeSignature {
                numerator,
                denominator,
            } => self.sequencer.set_time_signature(numerator, denominator),
            SequencerCommand::SetTrackClips { track, clips } => {
                self.sequencer.set_track_clips(track, clips);
            }
            SequencerCommand::SetControlClips { index, clips } => {
                self.sequencer.set_control_clips(index, clips);
            }
            SequencerCommand::SetMidiTrackCount(count) => {
                self.sequencer.set_track_count(count);
            }
        }
    }

    /// Reads the sample at `(channel, position)` from the master buffer;
    /// out-of-range is always silent (no panic).
    fn sample_at(&self, ch: usize, pos: usize) -> f32 {
        self.graph
            .master_channel(ch)
            .and_then(|c| c.get(pos))
            .copied()
            .unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use rtrb::Producer;

    use super::{BlockRunner, Feedback};
    use crate::realtime::automation::{RtAutoPoint, RtAutomation, RtControlClip};
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::{
        EngineCommand, NoteEvent, ParamId, SequencerCommand, command_channel,
    };
    use crate::realtime::context::ProcessContext;
    use crate::realtime::graph::{GraphCompiler, GraphUpdate};
    use crate::realtime::processor::Processor;
    use crate::realtime::sequencer::{
        MAX_TRACK_CLIPS, RtMidiClip, RtNote, Sequencer, TICKS_PER_BAR, TICKS_PER_QUARTER,
        TICKS_PER_STEP,
    };
    use crate::testing::count_positive_crossings;

    const SR: f32 = 44_100.0;

    /// Test harness: returns (runner, command sender, structure update sender,
    /// feedback handle, graph compiler).
    fn make_runner(
        fixed_block: usize,
        out_channels: usize,
    ) -> (
        BlockRunner,
        Producer<EngineCommand>,
        Producer<GraphUpdate>,
        Feedback,
        GraphCompiler,
    ) {
        let (tx, rx) = command_channel(64);
        let (graph_tx, graph_rx) = crate::realtime::voice::structure_channel();
        let (_recycle_tx, _recycle_rx) = crate::realtime::voice::recycle_channel();
        let (compiler, graph) = GraphCompiler::default_graph(SR, fixed_block);
        let feedback = Feedback::new();
        let sequencer = Sequencer::new(SR);
        let runner = BlockRunner::new(
            fixed_block,
            out_channels,
            SR,
            graph,
            sequencer,
            rx,
            graph_rx,
            _recycle_tx,
            feedback.clone(),
        );
        (runner, tx, graph_tx, feedback, compiler)
    }

    /// Fires an A4 note on track 0 (queue consumed at the next block boundary).
    fn note_on_69(tx: &mut Producer<EngineCommand>) {
        tx.push(EngineCommand::Note {
            track: 0,
            event: NoteEvent::On {
                note: 69,
                velocity: 1.0,
            },
        })
        .expect("queue not full");
    }

    #[test]
    fn phase_continuity_across_arbitrary_host_blocks() {
        let (mut runner, mut tx, _voice_tx, _feedback, _compiler) = make_runner(128, 2);
        note_on_69(&mut tx);
        // Render about 1 second with host buffers not divisible by the fixed
        // block (377), verifying block splitting does not break phase.
        let total_frames = 44_100usize;
        let mut interleaved = vec![0.0f32; total_frames * 2];
        let mut rendered = 0;
        while rendered < total_frames {
            let n = 377.min(total_frames - rendered);
            runner.render_interleaved(&mut interleaved[rendered * 2..(rendered + n) * 2]);
            rendered += n;
        }
        let left: Vec<f32> = interleaved.iter().copied().step_by(2).collect();
        let crossings = count_positive_crossings(&left);
        assert!(
            (crossings as i64 - 440).abs() <= 2,
            "crossings = {crossings}"
        );
    }

    #[test]
    fn channels_beyond_two_are_silent() {
        let (mut runner, mut tx, _voice_tx, _feedback, _compiler) = make_runner(128, 4);
        note_on_69(&mut tx);
        let mut out = vec![7.0f32; 256 * 4]; // pre-filled non-zero, verifying it is overwritten/zeroed
        runner.render_interleaved(&mut out);

        let frame = |i: usize| (out[i * 4], out[i * 4 + 1], out[i * 4 + 2], out[i * 4 + 3]);
        let (l, r, c2, c3) = frame(10);
        assert!(
            c2.abs() < 1e-12 && c3.abs() < 1e-12,
            "extra channels must be silent"
        );
        assert!(
            (l - r).abs() < 1e-9,
            "L/R content should match for a mono source"
        );
        assert!(
            l.abs() > 0.01 || out.iter().any(|s| s.abs() > 0.01),
            "there should be non-silent output"
        );
    }

    #[test]
    fn mono_output_takes_left_channel() {
        let (mut runner, mut tx, _voice_tx, _feedback, _compiler) = make_runner(128, 1);
        note_on_69(&mut tx);
        let mut out = vec![0.0f32; 256];
        runner.render_interleaved(&mut out);
        assert!(
            out.iter().any(|s| s.abs() > 0.01),
            "there should be non-silent output"
        );
    }

    #[test]
    fn command_takes_effect_at_block_boundary() {
        let (mut runner, mut tx, _voice_tx, _feedback, _compiler) = make_runner(128, 2);
        note_on_69(&mut tx);
        let half_frames = 11_025usize; // 0.25 s
        let mut out = vec![0.0f32; 44_100 * 2];

        runner.render_interleaved(&mut out[..half_frames * 2]);
        tx.push(EngineCommand::SetTrackParam {
            track: 0,
            id: ParamId::SineFrequency,
            value: 880.0,
        })
        .expect("queue not full");
        runner.render_interleaved(&mut out[half_frames * 2..]);

        // Second half (0.75 s) should be 880 Hz → 660 positive zero crossings
        // (±4 for transition-block error).
        let second_half: Vec<f32> = out[half_frames * 2..].iter().copied().step_by(2).collect();
        let crossings = count_positive_crossings(&second_half);
        assert!(
            (crossings as f64 - 660.0).abs() <= 4.0,
            "crossings = {crossings}"
        );
    }

    #[test]
    fn reports_level_position_and_transport() {
        let (mut runner, mut tx, _voice_tx, feedback, _compiler) = make_runner(128, 2);
        note_on_69(&mut tx);
        let mut out = vec![0.0f32; 300 * 2];
        runner.render_interleaved(&mut out);

        // 300 frames → consumes 3 blocks; position is the start of the block
        // currently being buffered (block 3 = 256).
        assert_eq!(feedback.position.load(Ordering::Relaxed), 256);
        assert!(
            feedback.level.load() > 0.0,
            "peak level must be positive when signal is output"
        );
        assert!(
            feedback.track_levels[0].load() > 0.0,
            "track 0 level must be positive"
        );
    }

    #[test]
    fn note_off_fades_stream_to_silence() {
        let (mut runner, mut tx, _voice_tx, feedback, _compiler) = make_runner(128, 2);
        note_on_69(&mut tx);
        let mut out = vec![0.0f32; 22_050 * 2];
        runner.render_interleaved(&mut out); // 0.5 s sustain

        tx.push(EngineCommand::Note {
            track: 0,
            event: NoteEvent::Off { note: 69 },
        })
        .expect("queue not full");
        runner.render_interleaved(&mut out); // release 0.2 s + margin

        assert!(
            feedback.level.load() < 0.02,
            "output should decay to silence after NoteOff, got {}",
            feedback.level.load()
        );
    }

    #[test]
    fn silent_until_first_note() {
        let (mut runner, _tx, _voice_tx, feedback, _compiler) = make_runner(128, 2);
        let mut out = vec![0.0f32; 1024 * 2];
        runner.render_interleaved(&mut out);
        assert!(
            feedback.level.load() < 1e-9,
            "should be silent when no note is triggered"
        );
    }

    /// G6 (spec `plugin-gui.md` §8): `NoteAtNode` reaches exactly the
    /// addressed instrument — not the track's MIDI fan-out targets.
    #[test]
    fn note_at_node_reaches_only_the_addressed_instrument() {
        let (mut runner, mut tx, _voice_tx, feedback, compiler) = make_runner(128, 2);
        let node = compiler
            .track_voice_node_id(1)
            .expect("track 1 should carry an instrument node");
        tx.push(EngineCommand::NoteAtNode {
            node_id: node,
            event: NoteEvent::On {
                note: 69,
                velocity: 1.0,
            },
        })
        .expect("queue not full");
        let mut out = vec![0.0f32; 512 * 2];
        runner.render_interleaved(&mut out);

        assert!(
            feedback.track_levels[1].load() > 0.0,
            "the addressed instrument (track 1) should sound"
        );
        assert!(
            feedback.track_levels[0].load() < 1e-9,
            "the track table must not be involved"
        );
    }

    /// G6: an unknown node id is silently dropped (no panic, no audio).
    #[test]
    fn note_at_unknown_node_is_a_dropped_no_op() {
        let (mut runner, mut tx, _voice_tx, feedback, _compiler) = make_runner(128, 2);
        tx.push(EngineCommand::NoteAtNode {
            node_id: 9999,
            event: NoteEvent::On {
                note: 69,
                velocity: 1.0,
            },
        })
        .expect("queue not full");
        let mut out = vec![0.0f32; 512 * 2];
        runner.render_interleaved(&mut out);
        assert!(feedback.level.load() < 1e-9, "no node should sound");
    }

    /// Sequencer end-to-end: command queue sends pattern and transport →
    /// synthesizer sounds → state feedback.
    /// Enqueues a one-clip table for a track: notes at `positions` (16th-note
    /// grid), one bar loop, one bar span.
    fn send_track_clips(tx: &mut Producer<EngineCommand>, track: usize, notes: &[(u32, u8)]) {
        let mut clip = RtMidiClip {
            span_ticks: TICKS_PER_BAR,
            loop_ticks: TICKS_PER_BAR,
            ..RtMidiClip::default()
        };
        for (start, note) in notes {
            clip.push(RtNote {
                start_ticks: *start,
                length_ticks: TICKS_PER_STEP,
                note: *note,
                channel: 0,
                velocity: 100,
            });
        }
        let mut clips = [RtMidiClip::default(); MAX_TRACK_CLIPS];
        clips[0] = clip;
        tx.push(EngineCommand::Sequencer(SequencerCommand::SetTrackClips {
            track,
            clips,
        }))
        .expect("queue not full");
    }

    #[test]
    fn sequencer_drives_synth_through_command_queue() {
        let (mut runner, mut tx, _voice_tx, feedback, _compiler) = make_runner(128, 2);

        // Clip: track 0 fires A4 at ticks 0/48/96; BPM 240.
        send_track_clips(&mut tx, 0, &[(0, 69), (48, 69), (96, 69)]);
        tx.push(EngineCommand::Sequencer(SequencerCommand::SetBpm(240.0)))
            .expect("queue not full");
        tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
            .expect("queue not full");

        let mut out = vec![0.0f32; 44_100]; // 1 s, mono is fine
        runner.render_interleaved(&mut out);

        assert!(
            feedback.playing.load(Ordering::Relaxed),
            "should be playing"
        );
        // Verify the audio content directly (the level atomic only reflects the
        // last block; the end may coincide with a new note attacking from zero
        // phase, making it a poor "is it sounding" criterion).
        let audio_peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(
            audio_peak > 0.05,
            "there should be audible output after the sequencer fires, peak {audio_peak}"
        );

        // Stop → transport state feedback flips to false.
        tx.push(EngineCommand::Sequencer(SequencerCommand::Stop))
            .expect("queue not full");
        runner.render_interleaved(&mut out);
        assert!(
            !feedback.playing.load(Ordering::Relaxed),
            "should no longer be playing after stopping"
        );
        assert!(feedback.step.load(Ordering::Relaxed) < 16);
    }

    #[test]
    fn silent_when_sequencer_not_playing() {
        let (mut runner, mut tx, _voice_tx, feedback, _compiler) = make_runner(128, 2);
        send_track_clips(&mut tx, 0, &[(0, 60)]);
        let mut out = vec![0.0f32; 22_050];
        runner.render_interleaved(&mut out);
        assert!(
            feedback.level.load() < 1e-9,
            "the sequencer should not sound when play is not pressed"
        );
    }

    #[test]
    fn automation_drives_mixer_gain_during_playback() {
        let (mut runner, mut tx, mut structure_tx, feedback, mut compiler) = make_runner(128, 2);
        // Gain curve: tick 0 = 1.0 → tick = one quarter note = 0.0
        // (120 BPM / 44.1 kHz ≈ 0.5 s).
        // Control track 0 (one clip covering bar 1, loop one bar) connects to
        // the global mixer's channel 0 gain (addressed by node_id; P5f D18:
        // track 0's chain feeds mixer channel 0 in the default graph).
        let mixer_id = compiler
            .node_id_of_kind(crate::realtime::NodeKind::Mixer)
            .expect("global mixer node_id should exist");
        let mut curve = RtAutomation {
            enabled: true,
            ..RtAutomation::default()
        };
        curve.push(RtAutoPoint {
            tick: 0,
            value: 1.0,
        });
        curve.push(RtAutoPoint {
            tick: TICKS_PER_QUARTER,
            value: 0.0,
        });
        let mut clips = [RtControlClip::default(); MAX_TRACK_CLIPS];
        clips[0] = RtControlClip {
            start_ticks: 0,
            span_ticks: TICKS_PER_BAR,
            loop_ticks: TICKS_PER_BAR,
            curve,
        };
        tx.push(EngineCommand::Sequencer(
            SequencerCommand::SetControlClips { index: 0, clips },
        ))
        .expect("queue not full");
        let update = compiler
            .set_auto_route(0, mixer_id, ParamId::MixerGain(0))
            .expect("automation routing should succeed");
        structure_tx.push(update).expect("structure queue not full");
        tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
            .expect("queue not full");
        // Note: Play clears manual notes, so note events must be enqueued after Play.
        note_on_69(&mut tx);

        let mut chunk = vec![0.0f32; 4410 * 2]; // 0.1 s
        let mut early_max = 0.0f32;
        for _ in 0..2 {
            runner.render_interleaved(&mut chunk);
            early_max = early_max.max(feedback.level.load());
        }
        assert!(
            early_max > 0.3,
            "gain ≈ 1 near the automation start, level should be large: {early_max}"
        );

        // Skip the middle 0.3 s (curve decay segment); afterwards the gain has reached zero.
        for _ in 0..3 {
            runner.render_interleaved(&mut chunk);
        }
        let mut late_max = 0.0f32;
        for _ in 0..5 {
            runner.render_interleaved(&mut chunk);
            late_max = late_max.max(feedback.level.load());
        }
        assert!(
            late_max < 0.02,
            "after 0.5 s the automation gain reaches zero, level should be near-silent: {late_max}"
        );
    }

    /// Test external sound source: writes the last received plugin parameter
    /// value into the buffer as a DC level, making it easy to observe whether
    /// parameter bridging works.
    struct ParamEcho {
        value: f32,
    }

    impl Processor for ParamEcho {
        fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
            for ch in 0..buffer.channels() {
                if let Some(channel) = buffer.channel_mut(ch) {
                    channel.fill(self.value);
                }
            }
        }

        fn set_param(&mut self, id: ParamId, value: f32) -> bool {
            if let ParamId::PluginParam(_) = id {
                self.value = value;
                true
            } else {
                false
            }
        }

        fn note_event(&mut self, _event: NoteEvent) -> bool {
            true
        }
    }

    #[test]
    fn voice_swap_and_automation_bridge_to_plugin_params() {
        let (mut runner, mut tx, mut structure_tx, feedback, mut compiler) = make_runner(128, 2);

        // 1. Swap in the external sound source (simulating a CLAP instrument
        // node): initial parameter 0 → silence.
        let (update, _id) = match compiler.set_voice(0, Box::new(ParamEcho { value: 0.0 })) {
            Ok(pair) => pair,
            Err(_) => panic!("swap in external sound source"),
        };
        structure_tx.push(update).expect("structure queue not full");
        let mut chunk = vec![0.0f32; 1024 * 2];
        runner.render_interleaved(&mut chunk);
        assert!(
            feedback.track_levels[0].load() < 1e-6,
            "external source with parameter 0 should output silence"
        );

        // 2. Write the plugin parameter via the command queue → external voice level follows.
        tx.push(EngineCommand::SetTrackParam {
            track: 0,
            id: ParamId::PluginParam(1),
            value: 0.5,
        })
        .expect("queue not full");
        runner.render_interleaved(&mut chunk);
        // metering point = track chain end (delay, dry pass-through; P5f
        // removed the per-track strip) → level = parameter × 1.0.
        assert!(
            (feedback.track_levels[0].load() - 0.5).abs() < 0.01,
            "plugin parameter 0.5 should read 0.5 at the chain end, got {}",
            feedback.track_levels[0].load()
        );

        // 3. Automation bridging: the curve drives PluginParam (0 → 1 over one
        //    bar), automation track 0 connects to track 0's sound source
        //    (ParamEcho, node_id is the ID newly assigned after the swap)'s
        //    plugin parameter 1.
        let voice_id = compiler
            .track_voice_node_id(0)
            .expect("there should be a node_id after the swap");
        let mut curve = RtAutomation {
            enabled: true,
            ..RtAutomation::default()
        };
        curve.push(RtAutoPoint {
            tick: 0,
            value: 0.0,
        });
        curve.push(RtAutoPoint {
            tick: TICKS_PER_BAR,
            value: 1.0,
        });
        let mut clips = [RtControlClip::default(); MAX_TRACK_CLIPS];
        clips[0] = RtControlClip {
            start_ticks: 0,
            span_ticks: TICKS_PER_BAR,
            loop_ticks: TICKS_PER_BAR,
            curve,
        };
        tx.push(EngineCommand::Sequencer(
            SequencerCommand::SetControlClips { index: 0, clips },
        ))
        .expect("queue not full");
        let update = compiler
            .set_auto_route(0, voice_id, ParamId::PluginParam(1))
            .expect("automation routing should succeed");
        structure_tx.push(update).expect("structure queue not full");
        tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
            .expect("queue not full");

        // Loop one bar @120BPM/44.1kHz = 88.2k samples ≈ 2 s.
        // First 5 blocks probe the ramp start, the middle is
        // discarded, and the tail probes the ramp end.
        let mut early = 0.0f32;
        for _ in 0..5 {
            runner.render_interleaved(&mut chunk);
            early = early.max(feedback.track_levels[0].load());
        }
        for _ in 0..77 {
            runner.render_interleaved(&mut chunk);
        }
        let mut late = 0.0f32;
        for _ in 0..5 {
            runner.render_interleaved(&mut chunk);
            late = late.max(feedback.track_levels[0].load());
        }
        assert!(
            late > 0.5 && early < 0.2,
            "automation should drive the plugin parameter ramp (including 0.8 track gain): early={early}, late={late}"
        );

        // 4. Switch back to the built-in sound source: the built-in
        // synthesizer has no notes → silence.
        let update = compiler
            .activate_builtin_voice(0)
            .expect("switch back to built-in sound source");
        structure_tx.push(update).expect("structure queue not full");
        tx.push(EngineCommand::Sequencer(SequencerCommand::Stop))
            .expect("queue not full");
        runner.render_interleaved(&mut chunk);
        assert!(
            feedback.track_levels[0].load() < 1e-6,
            "should be silent when switched back to the built-in source with no notes"
        );
    }

    #[test]
    fn tracks_play_in_parallel() {
        let (mut runner, mut tx, _voice_tx, feedback, _compiler) = make_runner(128, 2);
        // Two tracks fire different notes simultaneously: both track levels should be positive.
        for (track, note) in [(0usize, 60u8), (1usize, 67u8)] {
            tx.push(EngineCommand::Note {
                track,
                event: NoteEvent::On {
                    note,
                    velocity: 1.0,
                },
            })
            .expect("queue not full");
        }
        let mut out = vec![0.0f32; 4096 * 2];
        runner.render_interleaved(&mut out);
        assert!(
            feedback.track_levels[0].load() > 0.0,
            "track 0 should have a level"
        );
        assert!(
            feedback.track_levels[1].load() > 0.0,
            "track 1 should have a level"
        );
        assert!(
            feedback.track_levels[2].load() < 1e-9 && feedback.track_levels[3].load() < 1e-9,
            "unused tracks must be silent"
        );
    }

    /// Builds a flat tempo curve control clip for U3b tests.
    fn tempo_clip(start: u32, span: u32, bpm: f32) -> RtControlClip {
        let mut curve = RtAutomation {
            enabled: true,
            ..RtAutomation::default()
        };
        curve.push(RtAutoPoint {
            tick: 0,
            value: bpm,
        });
        curve.push(RtAutoPoint {
            tick: span,
            value: bpm,
        });
        RtControlClip {
            start_ticks: start,
            span_ticks: span,
            loop_ticks: span,
            curve,
        }
    }

    /// U3b D4: a tempo curve sampled at the block start governs that same
    /// block — at 240 BPM the transport covers twice the ticks per block, and
    /// the UI-side `feedback.bpm` reports the effective value.
    #[test]
    fn tempo_curve_overrides_the_transport_clock() {
        let (mut runner, mut tx, mut structure_tx, feedback, mut compiler) = make_runner(128, 2);
        let mut clips = [RtControlClip::default(); MAX_TRACK_CLIPS];
        clips[0] = tempo_clip(0, TICKS_PER_BAR, 240.0);
        tx.push(EngineCommand::Sequencer(
            SequencerCommand::SetControlClips { index: 0, clips },
        ))
        .expect("queue not full");
        let update = compiler
            .set_timeline_in_route(0)
            .expect("tempo routing should succeed");
        structure_tx.push(update).expect("structure queue not full");
        tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
            .expect("queue not full");

        // 100 blocks × 128 frames. At the base 120 BPM that is half the ticks
        // the 240 BPM curve produces; derive both from the tick maths so the
        // assertion does not depend on the harness sample rate.
        let mut out = vec![0.0f32; 128 * 100 * 2];
        runner.render_interleaved(&mut out);

        let bpm = feedback.bpm.load();
        assert!(
            (bpm - 240.0).abs() < 0.5,
            "effective BPM should be the curve value, got {bpm}"
        );
        let ticks_per_frame = |bpm: f32| bpm / 60.0 * TICKS_PER_QUARTER as f32 / SR;
        let expected = 100.0 * 128.0 * ticks_per_frame(240.0);
        let pos = feedback.position_ticks.load();
        assert!(
            (pos - expected).abs() <= 12.0,
            "transport should advance at the curve rate: expected {expected}, position = {pos}"
        );
        assert!(
            pos > 1.8 * 100.0 * 128.0 * ticks_per_frame(120.0),
            "the curve must roughly double the base advance, position = {pos}"
        );
    }

    /// U3b D3: the override is rebuilt every block, so a gap between tempo
    /// clips drops the transport back to the base BPM instead of freezing at
    /// the last curve value.
    #[test]
    fn tempo_curve_gap_falls_back_to_base() {
        let (mut runner, mut tx, mut structure_tx, feedback, mut compiler) = make_runner(128, 2);
        // Bar 1 at 240 BPM, silence (no clip) during bar 2, bar 3 at 240 again.
        let mut clips = [RtControlClip::default(); MAX_TRACK_CLIPS];
        clips[0] = tempo_clip(0, TICKS_PER_BAR, 240.0);
        clips[1] = tempo_clip(2 * TICKS_PER_BAR, TICKS_PER_BAR, 240.0);
        tx.push(EngineCommand::Sequencer(
            SequencerCommand::SetControlClips { index: 0, clips },
        ))
        .expect("queue not full");
        let update = compiler
            .set_timeline_in_route(0)
            .expect("tempo routing should succeed");
        structure_tx.push(update).expect("structure queue not full");
        tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
            .expect("queue not full");

        // 450 blocks: bar 1 at 240 BPM takes ~375 blocks, the remaining ~75
        // blocks land in the gap at 120 BPM.
        let mut out = vec![0.0f32; 128 * 450 * 2];
        runner.render_interleaved(&mut out);

        let pos = feedback.position_ticks.load();
        assert!(
            pos > TICKS_PER_BAR as f32 && pos < (2 * TICKS_PER_BAR) as f32,
            "the playhead should sit in the gap, position = {pos}"
        );
        let bpm = feedback.bpm.load();
        assert!(
            (bpm - 120.0).abs() < 0.5,
            "a tempo gap must fall back to the base BPM, got {bpm}"
        );
    }

    /// U3b: with no tempo cable the transport behaves exactly as before (pure
    /// addition, zero regression) — and dropping the route restores the base.
    #[test]
    fn no_tempo_route_keeps_the_base_bpm() {
        let (mut runner, mut tx, mut structure_tx, feedback, mut compiler) = make_runner(128, 2);
        let mut clips = [RtControlClip::default(); MAX_TRACK_CLIPS];
        clips[0] = tempo_clip(0, TICKS_PER_BAR, 240.0);
        tx.push(EngineCommand::Sequencer(
            SequencerCommand::SetControlClips { index: 0, clips },
        ))
        .expect("queue not full");
        tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
            .expect("queue not full");

        let mut out = vec![0.0f32; 128 * 50 * 2];
        runner.render_interleaved(&mut out);
        let bpm = feedback.bpm.load();
        assert!(
            (bpm - 120.0).abs() < 0.5,
            "an unrouted curve must not touch the clock, got {bpm}"
        );

        // Cable the track to the conductor, then cut it: the override applies
        // while cabled and disappears on disconnect.
        let update = compiler
            .set_timeline_in_route(0)
            .expect("tempo routing should succeed");
        structure_tx.push(update).expect("structure queue not full");
        runner.render_interleaved(&mut out);
        let cabled = feedback.bpm.load();
        assert!(
            (cabled - 240.0).abs() < 0.5,
            "cabled curve should drive the clock, got {cabled}"
        );

        let update = compiler
            .clear_auto_route(0)
            .expect("clearing the route should succeed");
        structure_tx.push(update).expect("structure queue not full");
        runner.render_interleaved(&mut out);
        let cut = feedback.bpm.load();
        assert!(
            (cut - 120.0).abs() < 0.5,
            "cutting the cable restores the base BPM, got {cut}"
        );
    }
}
