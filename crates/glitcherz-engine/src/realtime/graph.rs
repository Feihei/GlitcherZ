//! Node graph (P1–P3): wireable processing graph core, default track
//! topology, timeline routing tables, and the structural update protocol.
//!
//! Replaces the old fixed-topology `Mixer` (spec: `.agents/spec/node-graph.md`):
//!
//! - **One preallocated stereo output buffer per node**; multiple incoming
//!   edges are summed automatically at node inputs (D4); the in-place
//!   processing semantics of `Processor` are unchanged, so all built-in and
//!   CLAP/VST3 nodes plug in without modification.
//! - **Structural update protocol (D5)**: the control-side [`GraphCompiler`]
//!   maintains the topology and compiles the [`ExecPlan`] (topological sort +
//!   in-edge table, Kahn's algorithm, cycles rejected — D10); the audio
//!   thread swaps in whole plans via the [`GraphUpdate`] queue and performs
//!   pointer-level additions/removals; removed nodes (together with their
//!   output buffers) are sent back to the control thread for deallocation
//!   through the recycling receipt queue — the real-time thread **never
//!   drops heap objects**.
//! - **Timeline routing tables (P3, D7)**: timeline nodes exist permanently
//!   but occupy no audio nodes — MIDI tracks fan out to one or more
//!   instrument nodes via [`ExecPlan::midi_targets`] (multi-way fan-out is
//!   allowed); automation tracks deliver through [`ExecPlan::auto_routes`]
//!   with `(node, param)` as the destination; curves themselves carry no
//!   target.
//! - The default topology mirrors the old mixer: `instrument → delay →
//!   channel strip → master output`; the channel strip's gain/pan/mute are
//!   upgraded into a real node [`ChannelStripProcessor`]; each MIDI track is
//!   connected to its own instrument by default.

use rtrb::Producer;

use crate::realtime::DEFAULT_MIDI_TRACKS;
use crate::realtime::MAX_MIDI_TRACKS;
use crate::realtime::automation::MAX_AUTO_TRACKS;
use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::delay::DelayProcessor;
use crate::realtime::effect::Smoothed;
use crate::realtime::effect::descriptor::{ParamSpec, Unit};
use crate::realtime::gain::GainProcessor;
use crate::realtime::mixer::MixerProcessor;
use crate::realtime::processor::{ChannelWeights, Processor};
use crate::realtime::synth::SynthProcessor;
use crate::realtime::voice::RecycledNode;

/// Graph capacity limit (D1): node count.
pub const MAX_NODES: usize = 64;
/// Graph capacity limit (D1): cable count (P5f: 256 — a full 32-channel
/// mixer consumes 32 cables on its own).
pub const MAX_CABLES: usize = 256;
/// Graph capacity limit (P5f D14/D15): max in-edges per node — only mixer
/// nodes can reach this; every other node takes ≤ 1 in-edge (D14) and the
/// master output exactly 1 (D16), enforced at compile time via [`NodeKind`].
pub const MAX_IN_EDGES: usize = 32;
/// Sentinel value for "no edge" (empty slot marker in `out_edge` / cable tables).
pub const NO_EDGE: u16 = u16::MAX;
/// Sentinel slot of the Timeline In endpoint (U3b, spec
/// `.agents/spec/tempo-meter.md` §6): an [`AutoRoute`] whose `slot` is this
/// value delivers its curve to the **transport clock** instead of a node
/// parameter. Distinct from [`NO_EDGE`] so "cabled to the conductor" and
/// "not cabled" stay distinguishable. It never indexes `slots`.
pub const TIMELINE_IN: u16 = u16::MAX - 1;
/// Number of external effect slots per track (following the old `TrackStrip` semantics).
pub const EFFECT_SLOTS: usize = 4;
/// Maximum nodes installed in a single structural update (≥ the per-track trio of 3).
pub const MAX_DEPLOYMENTS: usize = 4;
/// Maximum nodes removed in a single structural update (≥ track trio + effects, 7).
pub const MAX_REMOVALS: usize = 8;
/// Maximum instrument nodes a single MIDI track can fan out to simultaneously (D7).
pub const MAX_MIDI_FANOUT: usize = 4;

/// Reserved node_id of the master output (D6: always present, cannot be removed or retyped).
/// Document nodes (glitcherz-model) are allocated from 2; 0 is always the invalid sentinel.
pub const MASTER_NODE_ID: u64 = 1;

/// A single audio cable (compiler side): source slot → destination slot.
///
/// P5b: replaces the old single-out-edge `out_edge: [u16; MAX_NODES]` — one
/// output port can drive multiple cables (§3 "out-edges are unbounded"); the
/// runtime side sums by the in-edge table only, so multi-out fan-out works
/// naturally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cable {
    /// Source node slot.
    pub from: u16,
    /// Destination node slot.
    pub to: u16,
    /// Destination channel (D17): mixer input port index for mixer targets
    /// (drives the per-channel summing weights); 0 for every other target.
    pub in_port: u8,
}

/// Input discipline of a slot (P5f D14/D16): governs the compile-time
/// in-edge limit. Tracked per slot by the compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// Ordinary node: ≤ 1 audio in-edge (instruments take 0 anyway).
    Plain,
    /// Mixer node: ≤ [`MAX_IN_EDGES`] audio in-edges, unique `in_port` per edge.
    Mixer,
    /// Master output: exactly ≤ 1 audio in-edge (D16).
    Master,
    /// Feedback tap (P6): sums up to [`MAX_IN_EDGES`] in-edges **read from
    /// the previous block**, which is what exempts it from cycle rejection.
    /// Ordering edges into this node are reversed (it runs *before* its
    /// sources), so the tap sources still hold last block's output when the
    /// sum happens — the explicit one-block delay, no extra buffer needed.
    Feedback,
}

impl NodeKind {
    /// Compile-time in-edge limit for this node kind.
    pub fn max_in(self) -> usize {
        match self {
            NodeKind::Mixer | NodeKind::Feedback => MAX_IN_EDGES,
            NodeKind::Plain | NodeKind::Master => 1,
        }
    }
}

/// What one automation cable delivers (U4a, spec `control-tracks.md` D2).
///
/// A curve can drive a node **parameter** or emit a **MIDI CC** into the
/// node's MIDI input. The two are different channels, not two spellings of the
/// same thing (a plugin's `Cutoff` parameter and CC74 are unrelated), so the
/// route carries the distinction instead of overloading `ParamId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoDest {
    /// Node parameter value (`Processor::set_param`).
    Param(ParamId),
    /// MIDI control change (`Processor::midi_cc`), 7-bit value.
    Cc {
        /// Controller number 0..=127.
        controller: u8,
        /// MIDI channel 0..=15.
        channel: u8,
    },
}

impl Default for AutoDest {
    fn default() -> Self {
        AutoDest::Param(ParamId::MasterGain)
    }
}

/// Destination of one automation cable (D7): target node slot + destination.
///
/// The curve carries no destination; `slot == NO_EDGE` means this automation
/// track is not connected (silent). `slot == TIMELINE_IN` (U3b) means the
/// curve drives the transport clock; `dest` is then unused — v1 has exactly
/// one conductor port (`Tempo`), so the sentinel alone identifies it (a
/// second port such as `Swing` would add a `port` field, see spec §4.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoRoute {
    /// Target node slot.
    pub slot: u16,
    /// What the sampled value is delivered as.
    pub dest: AutoDest,
}

impl Default for AutoRoute {
    fn default() -> Self {
        Self {
            slot: NO_EDGE,
            dest: AutoDest::default(),
        }
    }
}

/// Precompiled execution plan: fully `Copy`; the audio thread traverses only
/// this per block (D5).
///
/// Produced by the control-side [`GraphCompiler::compile`] and swapped whole
/// into [`RtGraph`] via [`GraphUpdate`]; the audio thread reads it only.
#[derive(Debug, Clone, Copy)]
pub struct ExecPlan {
    /// Slot indices in topological order (only the first `order_len` entries are valid).
    pub order: [u16; MAX_NODES],
    /// Length of the valid topological order.
    pub order_len: usize,
    /// Audio in-edges of each slot (source slot indices; only the first
    /// `in_counts[slot]` entries are valid).
    pub in_edges: [[u16; MAX_IN_EDGES]; MAX_NODES],
    /// Per-in-edge destination channel (D17/D20): `in_ports[slot][i]` is the
    /// mixer channel the i-th in-edge lands on (drives the per-channel
    /// summing weights; 0 for non-mixer targets).
    pub in_ports: [[u8; MAX_IN_EDGES]; MAX_NODES],
    /// In-edge count of each slot.
    pub in_counts: [u8; MAX_NODES],
    /// Node ID of each slot (P5b: public addressing key; slots are an
    /// internal implementation detail).
    pub node_ids: [u64; MAX_NODES],
    /// Master output slot (the data source for cpal interleaved writes).
    pub master_slot: u16,
    /// Per-track instrument slot (track addressing for manual
    /// parameters/levels; for note delivery see `midi_targets`).
    pub track_voice: [u16; MAX_MIDI_TRACKS],
    /// Per-track delay effect slot.
    pub track_delay: [u16; MAX_MIDI_TRACKS],
    /// Per-track channel strip slot (the metering point for level reporting).
    pub track_channel: [u16; MAX_MIDI_TRACKS],
    /// Timeline MIDI routing table: destination slots fanned out by each
    /// MIDI track (only the first `midi_target_counts[track]` entries are
    /// valid).
    pub midi_targets: [[u16; MAX_MIDI_FANOUT]; MAX_MIDI_TRACKS],
    /// Valid fan-out count of each MIDI track.
    pub midi_target_counts: [u8; MAX_MIDI_TRACKS],
    /// Timeline automation routing table: delivery destinations indexed by
    /// automation track (D7).
    pub auto_routes: [AutoRoute; MAX_AUTO_TRACKS],
}

impl ExecPlan {
    /// Empty plan (no nodes, silent output, timeline unconnected).
    pub const EMPTY: Self = Self {
        order: [0; MAX_NODES],
        order_len: 0,
        in_edges: [[NO_EDGE; MAX_IN_EDGES]; MAX_NODES],
        in_ports: [[0; MAX_IN_EDGES]; MAX_NODES],
        in_counts: [0; MAX_NODES],
        node_ids: [0; MAX_NODES],
        master_slot: 0,
        track_voice: [NO_EDGE; MAX_MIDI_TRACKS],
        track_delay: [NO_EDGE; MAX_MIDI_TRACKS],
        track_channel: [NO_EDGE; MAX_MIDI_TRACKS],
        midi_targets: [[NO_EDGE; MAX_MIDI_FANOUT]; MAX_MIDI_TRACKS],
        midi_target_counts: [0; MAX_MIDI_TRACKS],
        auto_routes: [AutoRoute {
            slot: NO_EDGE,
            dest: AutoDest::Param(ParamId::MasterGain),
        }; MAX_AUTO_TRACKS],
    };
}

/// Level snapshot after one block of processing (shape follows the old
/// `MixerLevels`, fixed-capacity track side extended to [`MAX_MIDI_TRACKS`],
/// unconfigured tracks are always 0).
#[derive(Debug, Clone, Copy, Default)]
pub struct GraphLevels {
    /// Peak of each track's channel strip output (post-fader, 0 when muted).
    pub tracks: [f32; MAX_MIDI_TRACKS],
    /// Master output peak, both channels collapsed to their maximum (kept
    /// for single-bar call sites; the transport meter reads `master_lr`).
    pub master: f32,
    /// Master output peak per channel, `[L, R]` — the transport's stereo
    /// meter. A mono output leaves `R` at zero rather than duplicating `L`.
    pub master_lr: [f32; 2],
}

/// Runtime node slot: processor + dedicated stereo output buffer.
struct NodeSlot {
    node_id: u64,
    processor: Box<dyn Processor>,
    output: AudioBuffer,
}

/// Runtime graph: fixed-capacity slots + the current execution plan.
/// Structural changes only do pointer moves and `Copy` overwrites
/// ([`RtGraph::apply_update`]); the processing path is allocation-free.
pub struct RtGraph {
    slots: [Option<NodeSlot>; MAX_NODES],
    plan: ExecPlan,
    block_size: usize,
    /// Panic sweep cursor (see [`RtGraph::panic`]): `Some(start)` = the next
    /// block must release pitches `start..` and advance; `None` = idle.
    panic_sweep: Option<u8>,
    /// Last 7-bit value emitted per automation track (U4a D4): `-1` = none
    /// yet. A control change is an **absolute** value, so resending an
    /// unchanged one is pure overhead (375 blocks/s otherwise).
    last_cc: [i16; MAX_AUTO_TRACKS],
}

/// Pitches released per block by a panic sweep. Kept below the plugin hosts'
/// 32-event pending queue (`MAX_PENDING_EVENTS`) so a sweep never crowds out
/// regular events queued in the same block.
const PANIC_SWEEP_BATCH: u8 = 24;

/// Structural update payload: one topology change = new execution plan +
/// incremental installs + incremental removals.
///
/// Delivered over the structural queue (rtrb, non-`Copy`); the enum values in
/// the queue live in the ring buffer's preallocated storage; after the audio
/// thread `take`s all heap resources, dropping the leftover values performs
/// no heap operations.
#[allow(clippy::large_enum_variant)] // SetTopology dwarfs SetAudioClips;
// the queue preallocates by the largest
// variant either way (same trade-off as
// SequencerCommand::SetTrackClips).
pub enum GraphUpdate {
    /// Swaps in the whole execution plan and applies incremental
    /// installs/removals (removals first, then installs).
    SetTopology {
        /// New execution plan (`Copy` overwrite).
        plan: ExecPlan,
        /// Nodes to install (output buffers already allocated on the control side).
        deployments: [NodeDeployment; MAX_DEPLOYMENTS],
        /// Number of valid installs.
        deployment_count: usize,
        /// Slots to remove (node + buffer sent to the recycle queue).
        removed: [u16; MAX_REMOVALS],
        /// Number of valid removals.
        removed_count: usize,
    },
    /// Replaces one audio-track node's clip table (U6, spec
    /// `.agents/spec/audio-track.md` D4). The table carries
    /// `Arc<SampleData>` (not `Copy`), which is exactly why it rides this
    /// queue instead of the command queue. The block runner routes the
    /// displaced old table into the trash queue — never dropped in place.
    SetAudioClips {
        /// Target node ID (the `NodeDoc::AudioTrack`'s engine id).
        node_id: u64,
        /// The new clip table (built on the control thread).
        clips: crate::realtime::audio_track::AudioClipTable,
    },
}

/// One node install: control-side-constructed processor + dedicated output
/// buffer.
///
/// The buffer must be preallocated on the control side (the real-time thread
/// must not allocate) and moves into the graph together with the node; when a
/// node is removed, its buffer is returned to the control thread for
/// deallocation via [`RecycledNode`].
pub struct NodeDeployment {
    /// Destination slot.
    pub slot: u16,
    /// Node ID monotonically allocated on the control side (used for recycle pairing).
    pub node_id: u64,
    /// `Some` = pending install (processor + output buffer); set to `None` after consumption.
    pub node: Option<(Box<dyn Processor>, AudioBuffer)>,
}

impl RtGraph {
    /// Empty graph (all slots empty, silent output). **Not RT-safe** (no heap
    /// allocation; construction only).
    pub fn new(block_size: usize) -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            plan: ExecPlan::EMPTY,
            block_size: block_size.max(1),
            panic_sweep: None,
            last_cc: [-1; MAX_AUTO_TRACKS],
        }
    }

    /// Installs a node directly (used only by default-graph construction
    /// before stream start). **Not RT-safe**.
    fn install(
        &mut self,
        slot: usize,
        node_id: u64,
        processor: Box<dyn Processor>,
        output: AudioBuffer,
    ) {
        self.slots[slot] = Some(NodeSlot {
            node_id,
            processor,
            output,
        });
    }

    /// Applies one structural update: remove (into the recycle queue) first,
    /// install second, then swap in the whole new plan.
    /// RT-safe: only pointer moves, `Copy` overwrites, and queue pushes; no
    /// allocation / heap drops.
    pub fn apply_update(&mut self, update: GraphUpdate, recycle: &mut Producer<RecycledNode>) {
        // `SetAudioClips` is intercepted by the block runner before this
        // point; reaching it here would be a wiring bug — refuse it rather
        // than mis-handle the table.
        let GraphUpdate::SetTopology {
            plan,
            mut deployments,
            deployment_count,
            removed,
            removed_count,
        } = update
        else {
            return;
        };
        for &slot in &removed[..removed_count] {
            if let Some(node) = self.slots[slot as usize].take() {
                // If the recycle queue is full, skip the uninstall (keep
                // current state) to avoid dropping on the real-time thread.
                let _ = recycle.push(RecycledNode {
                    node_id: node.node_id,
                    node: node.processor,
                    output: node.output,
                });
            }
        }
        for dep in deployments.iter_mut().take(deployment_count) {
            let Some((processor, output)) = dep.node.take() else {
                continue;
            };
            let s = dep.slot as usize;
            if s >= MAX_NODES {
                // The control-side protocol guarantees this never happens;
                // the defensive path also recycles instead of dropping in place.
                let _ = recycle.push(RecycledNode {
                    node_id: dep.node_id,
                    node: processor,
                    output,
                });
                continue;
            }
            self.slots[s] = Some(NodeSlot {
                node_id: dep.node_id,
                processor,
                output,
            });
        }
        self.plan = plan;
        // A new plan may point a curve at a different node: forget the last
        // emitted value so the next sampled block always delivers one (the
        // new target would otherwise wait for the curve to move).
        self.last_cc = [-1; MAX_AUTO_TRACKS];
    }

    /// Replaces one node's audio clip table (U6 D4). Returns the table that
    /// must go to the trash queue: the displaced old one on success, or the
    /// unconsumed input when the node is unknown / not an audio-track node.
    /// RT-safe: a linear slot scan (≤ [`MAX_NODES`]) plus a move; nothing
    /// drops here.
    pub fn set_audio_clips(
        &mut self,
        node_id: u64,
        clips: crate::realtime::audio_track::AudioClipTable,
    ) -> Option<crate::realtime::audio_track::AudioClipTable> {
        for slot in self.slots.iter_mut().flatten() {
            if slot.node_id == node_id {
                return slot.processor.swap_audio_clips(clips);
            }
        }
        None
    }

    /// Processes one block: in topological order, sum in-edges → consume
    /// events → process in place; returns levels.
    /// RT-safe.
    pub fn process_block(&mut self, ctx: &ProcessContext) -> GraphLevels {
        // A panic started in a previous command boundary continues here
        // (bounded batch per block, see `panic`).
        self.drain_panic_sweep();
        let frames = self.block_size.min(ctx.frames);
        for i in 0..self.plan.order_len {
            let s = self.plan.order[i] as usize;
            // Temporarily take this node out: borrowing the source buffer
            // read-only and this buffer writable is perfectly safe.
            let Some(mut node) = self.slots[s].take() else {
                continue;
            };
            node.output.set_len(frames);
            node.output.clear();
            let count = self.plan.in_counts[s] as usize;
            // Copy the edge tables out (arrays are `Copy`) so summing does not
            // borrow `self` while `self.slots` is being mutated.
            let edges = self.plan.in_edges[s];
            let ports = self.plan.in_ports[s];
            // D20: per-channel weights come from the destination processor
            // (mixer nodes); `None` = unit weights. Split borrow: `processor`
            // is read while `output` is written — disjoint fields of the
            // taken-out node.
            let weights: Option<&ChannelWeights> = node.processor.input_weights();
            for (i, &src) in edges[..count].iter().enumerate() {
                if let Some(src_node) = self.slots[src as usize].as_ref() {
                    match weights {
                        Some(w) => sum_into_weighted(
                            &mut node.output,
                            &src_node.output,
                            w[ports[i] as usize],
                        ),
                        None => sum_into(&mut node.output, &src_node.output),
                    }
                }
            }
            node.processor.process(ctx, &mut node.output);
            self.slots[s] = Some(node);
        }
        let mut levels = GraphLevels::default();
        for (t, &slot) in self.plan.track_channel.iter().enumerate() {
            levels.tracks[t] = self.slot_peak(slot);
        }
        levels.master_lr = self.slot_peak_lr(self.plan.master_slot);
        levels.master = levels.master_lr[0].max(levels.master_lr[1]);
        levels
    }

    /// Peak of a slot's output (0 for empty slots / out of range). RT-safe.
    fn slot_peak(&self, slot: u16) -> f32 {
        let Some(node) = self.slots.get(slot as usize).and_then(|s| s.as_ref()) else {
            return 0.0;
        };
        let mut peak = 0.0f32;
        for ch in 0..node.output.channels() {
            if let Some(channel) = node.output.channel(ch) {
                for &sample in channel {
                    let abs = sample.abs();
                    if abs > peak {
                        peak = abs;
                    }
                }
            }
        }
        peak
    }

    /// Peak of a slot's output per channel, `[L, R]` (0 for an empty slot;
    /// a mono output reports `[peak, 0]`). RT-safe.
    fn slot_peak_lr(&self, slot: u16) -> [f32; 2] {
        let Some(node) = self.slots.get(slot as usize).and_then(|s| s.as_ref()) else {
            return [0.0; 2];
        };
        let mut peaks = [0.0f32; 2];
        for (ch, peak) in peaks.iter_mut().enumerate() {
            let Some(channel) = node.output.channel(ch) else {
                break;
            };
            for &sample in channel {
                let abs = sample.abs();
                if abs > *peak {
                    *peak = abs;
                }
            }
        }
        peaks
    }

    /// Delivers a note event to a MIDI track: fans out via the routing table
    /// to all connected instrument nodes (shared by timeline sequencing and
    /// keyboard playing; unconnected = silent). RT-safe.
    pub fn track_note(&mut self, track: usize, event: NoteEvent) -> bool {
        if track >= MAX_MIDI_TRACKS {
            return false;
        }
        let count = self.plan.midi_target_counts[track] as usize;
        // Routing table rows are `Copy`: copy out first so the borrow does
        // not conflict with delivery.
        let targets = self.plan.midi_targets[track];
        let mut delivered = false;
        for &slot in &targets[..count] {
            delivered |= self.slot_note(slot, event);
        }
        delivered
    }

    /// Releases all connected targets' current notes (call before transport
    /// play/stop). RT-safe.
    pub fn note_off_all(&mut self) {
        for track in 0..MAX_MIDI_TRACKS {
            let count = self.plan.midi_target_counts[track] as usize;
            let targets = self.plan.midi_targets[track];
            for &slot in &targets[..count] {
                let _ = self.slot_note(slot, NoteEvent::Off { note: 0 });
            }
        }
    }

    /// Panic: release **every** sounding voice, transport untouched.
    ///
    /// A blanket `Off { note: 0 }` (what [`Self::note_off_all`] sends) never
    /// reaches a pitch-matching plugin — Dexed keeps its voices forever. So a
    /// panic walks all 128 pitches across every slot in batches of
    /// [`PANIC_SWEEP_BATCH`] per block (≈15 ms at 44.1 kHz / 128 frames): the
    /// plugin hosts' pending event queues hold 32 entries, and a single
    /// 128-event burst would be dropped past the cap.
    ///
    /// RT-safe: fixed loops, no allocation.
    pub fn panic(&mut self) {
        self.panic_sweep = Some(0);
    }

    /// Sends one panic batch to every slot and advances the cursor
    /// (no-op when no panic is in flight). RT-safe.
    fn drain_panic_sweep(&mut self) {
        let Some(start) = self.panic_sweep else {
            return;
        };
        let end = (u16::from(start) + u16::from(PANIC_SWEEP_BATCH)).min(128) as u8;
        for slot in 0..MAX_NODES {
            if self.slots[slot].is_none() {
                continue;
            }
            for note in start..end {
                let _ = self.slot_note(slot as u16, NoteEvent::Off { note });
            }
        }
        self.panic_sweep = (end < 128).then_some(end);
    }

    fn slot_note(&mut self, slot: u16, event: NoteEvent) -> bool {
        self.slots
            .get_mut(slot as usize)
            .and_then(|s| s.as_mut())
            .is_some_and(|n| n.processor.note_event(event))
    }

    /// Targeted note delivery by node_id (G6: the virtual-keyboard route —
    /// one instrument, no track fan-out). Unknown / empty slots are silently
    /// ignored. RT-safe (linear slot scan, no allocation).
    pub fn node_note(&mut self, node_id: u64, event: NoteEvent) -> bool {
        let Some(index) = self
            .slots
            .iter()
            .position(|s| s.as_ref().is_some_and(|n| n.node_id == node_id))
        else {
            return false;
        };
        // `index` comes from the slice itself, so the cast cannot truncate
        // (slot capacity 64 < u16::MAX).
        self.slot_note(index as u16, event)
    }

    /// Automation track delivery: looks up the routing table and hands the
    /// curve value to `(target node, destination)` (fixed at compile time, see
    /// [`AutoRoute`]). Silent when unconnected / out of range. RT-safe.
    ///
    /// Parameter destinations write the value through; CC destinations
    /// quantize to 7 bits and **only emit on change** (U4a D4).
    pub fn auto_deliver(&mut self, auto_index: usize, value: f32) {
        let Some(&route) = self.plan.auto_routes.get(auto_index) else {
            return;
        };
        if route.slot == NO_EDGE {
            return;
        }
        // U3b: the conductor endpoint is not a node — the block runner reads
        // the routing table itself and steers the transport clock instead.
        if route.slot == TIMELINE_IN {
            return;
        }
        match route.dest {
            AutoDest::Param(id) => {
                let _ = self.slot_set_param(route.slot, id, value);
            }
            AutoDest::Cc {
                controller,
                channel,
            } => {
                if !value.is_finite() {
                    return;
                }
                // MIDI CC is a 7-bit integer: round rather than truncate so a
                // 0.5 anchor lands on 64, not 0.
                let value = value.clamp(0.0, 127.0).round() as i16;
                if self.last_cc.get(auto_index) == Some(&value) {
                    return;
                }
                if let Some(slot) = self.last_cc.get_mut(auto_index) {
                    *slot = value;
                }
                let _ = self.slot_midi_cc(route.slot, channel, controller, value as u8);
            }
        }
    }

    /// Routing entry of one automation track (U3b): the block runner needs to
    /// tell a node target from the Timeline In sentinel **before** delivering
    /// the sampled value. RT-safe.
    pub fn auto_route(&self, auto_index: usize) -> Option<AutoRoute> {
        self.plan.auto_routes.get(auto_index).copied()
    }

    /// Targeted parameter delivery by node_id (P5b: the public node-addressed
    /// path; both the patch and patch cables follow this). RT-safe.
    pub fn node_set_param(&mut self, node_id: u64, id: ParamId, value: f32) -> bool {
        for slot in self.slots.iter_mut().flatten() {
            if slot.node_id == node_id {
                return slot.processor.set_param(id, value);
            }
        }
        false
    }

    /// Track parameter routing (out of range is ignored): the mixer trio →
    /// channel strip; the delay trio → delay; timbre/envelope/filter/plugin
    /// parameters → instrument. **Transitional path** — after P5b only the
    /// mixer panel's manual parameters still use it (retired when the P5d
    /// patch lands); timeline delivery always goes through the node_id
    /// routing table. RT-safe.
    pub fn track_set_param(&mut self, track: usize, id: ParamId, value: f32) {
        if track >= MAX_MIDI_TRACKS {
            return;
        }
        let slot = route_slot(
            self.plan.track_voice[track],
            self.plan.track_delay[track],
            self.plan.track_channel[track],
            id,
        );
        if slot != NO_EDGE {
            let _ = self.slot_set_param(slot, id, value);
        }
    }

    /// Master output slot parameter (master gain). RT-safe.
    pub fn master_set_param(&mut self, id: ParamId, value: f32) -> bool {
        self.slot_set_param(self.plan.master_slot, id, value)
    }

    fn slot_set_param(&mut self, slot: u16, id: ParamId, value: f32) -> bool {
        self.slots
            .get_mut(slot as usize)
            .and_then(|s| s.as_mut())
            .is_some_and(|n| n.processor.set_param(id, value))
    }

    fn slot_midi_cc(&mut self, slot: u16, channel: u8, controller: u8, value: u8) -> bool {
        self.slots
            .get_mut(slot as usize)
            .and_then(|s| s.as_mut())
            .is_some_and(|n| n.processor.midi_cc(channel, controller, value))
    }

    /// Read-only access to the master output buffer (for `BlockRunner`
    /// interleaved writes). RT-safe.
    pub fn master_channel(&self, ch: usize) -> Option<&[f32]> {
        self.slots
            .get(self.plan.master_slot as usize)
            .and_then(|s| s.as_ref())
            .and_then(|n| n.output.channel(ch))
    }
}

/// Adds the active region of `src` into `dst` sample by sample (multi-in-edge
/// summing, D4). RT-safe.
fn sum_into(dst: &mut AudioBuffer, src: &AudioBuffer) {
    for ch in 0..dst.channels().min(src.channels()) {
        if let (Some(d), Some(s)) = (dst.channel_mut(ch), src.channel(ch)) {
            for (d, s) in d.iter_mut().zip(s.iter()) {
                *d += s;
            }
        }
    }
}

/// Weighted variant of [`sum_into`] (D20): channel 0 is scaled by `weight[0]`,
/// channel 1 by `weight[1]` before accumulating. RT-safe (pure FMA loop).
fn sum_into_weighted(dst: &mut AudioBuffer, src: &AudioBuffer, weight: [f32; 2]) {
    for (ch, w) in weight
        .iter()
        .enumerate()
        .take(dst.channels().min(src.channels()))
    {
        if let (Some(d), Some(s)) = (dst.channel_mut(ch), src.channel(ch)) {
            for (d, s) in d.iter_mut().zip(s.iter()) {
                *d += s * w;
            }
        }
    }
}

/// Resolves the destination slot within a track by parameter kind: the mixer
/// trio → channel strip; the delay trio → delay; master gain has no track
/// home; the rest (timbre/envelope/filter/plugin) → instrument.
/// Routing semantics shared by the graph runtime and the compiler.
fn route_slot(voice: u16, delay: u16, channel: u16, param: ParamId) -> u16 {
    match param {
        ParamId::TrackGain | ParamId::TrackPan | ParamId::TrackMute => channel,
        ParamId::DelayTime | ParamId::DelayFeedback | ParamId::DelayMix => delay,
        ParamId::MasterGain => NO_EDGE,
        // Built-in effect slots are node-addressed (`SetNodeParam` + the
        // effect's own `node_id`), never part of a track's fixed trio.
        ParamId::BuiltinParam(_) => NO_EDGE,
        _ => voice,
    }
}

/// Channel strip node: gain / pan (linear law) / mute. After graphification
/// the mixer trio becomes real nodes.
///
/// Semantics match the old `TrackStrip` summing stage: muted output is all
/// zeros; pan p=0 → (1,1), p=±1 → one side full scale.
/// Channel strip gain/pan specs (D8: both smoothed — fader moves are where
/// zipper noise is most audible).
const STRIP_GAIN_SPEC: ParamSpec = ParamSpec::gain("gain", Unit::Percent, 0.0, 1.5, 0.8);
const STRIP_PAN_SPEC: ParamSpec = ParamSpec::pan("pan", -1.0, 1.0, 0.0);

pub struct ChannelStripProcessor {
    gain: Smoothed,
    pan: Smoothed,
    muted: bool,
}

impl ChannelStripProcessor {
    /// Default gain 0.8, pan centered, not muted (same as the old
    /// `TrackStrip::new`). Assumes a 48 kHz stream; use
    /// [`ChannelStripProcessor::new_at`] when the rate is known.
    pub fn new() -> Self {
        Self::new_at(48_000.0)
    }

    /// Same as [`ChannelStripProcessor::new`] with an explicit sample rate.
    pub fn new_at(sample_rate: f32) -> Self {
        Self {
            gain: Smoothed::new(&STRIP_GAIN_SPEC, sample_rate),
            pan: Smoothed::new(&STRIP_PAN_SPEC, sample_rate),
            muted: false,
        }
    }
}

impl Default for ChannelStripProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl Processor for ChannelStripProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        if self.muted {
            for ch in 0..buffer.channels() {
                if let Some(channel) = buffer.channel_mut(ch) {
                    channel.fill(0.0);
                }
            }
            return;
        }
        let frames = buffer.len();
        for index in 0..frames {
            // One tick per frame: stereo must not double the ramp speed.
            let gain = self.gain.tick();
            let pan = self.pan.tick().clamp(-1.0, 1.0);
            let weights = [(1.0 - pan).clamp(0.0, 1.0), (1.0 + pan).clamp(0.0, 1.0)];
            for (ch, weight) in weights.iter().enumerate() {
                if let Some(channel) = buffer.channel_mut(ch) {
                    channel[index] *= gain * weight;
                }
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::TrackGain => {
                self.gain.set(value);
                true
            }
            ParamId::TrackPan => {
                self.pan.set(value);
                true
            }
            ParamId::TrackMute => {
                self.muted = value >= 0.5;
                true
            }
            _ => false,
        }
    }
}

/// Feedback tap node (P6): the explicit loop breaker.
///
/// Its in-edges are summed from the **previous block's** buffers (the
/// compiler orders it before its tap sources, see [`NodeKind::Feedback`]),
/// so the processor itself only has to scale the sum — the one-block delay
/// lives in the scheduling, not in a buffer. `gain` keeps a bare loop from
/// diverging (default 0.5, clamped to 0..1).
pub struct FeedbackProcessor {
    gain: f32,
}

impl FeedbackProcessor {
    /// Default gain 0.5 (a loop at unity would sustain or blow up).
    pub fn new() -> Self {
        Self { gain: 0.5 }
    }
}

impl Default for FeedbackProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl Processor for FeedbackProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        if (self.gain - 1.0).abs() < f32::EPSILON {
            return; // Unity gain: the summed tap is the output
        }
        for ch in 0..buffer.channels() {
            if let Some(channel) = buffer.channel_mut(ch) {
                for sample in channel.iter_mut() {
                    *sample *= self.gain;
                }
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::FeedbackGain => {
                if value.is_finite() {
                    self.gain = value.clamp(0.0, 1.0);
                }
                true
            }
            _ => false,
        }
    }
}

/// Topology compilation/validation errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopologyError {
    /// Destination track out of range (≥ [`MAX_MIDI_TRACKS`]) or unconfigured.
    InvalidTrack,
    /// Destination node invalid (unassigned node_id / slot out of range or empty).
    InvalidSlot,
    /// Destination index out of range (automation track index).
    InvalidIndex,
    /// No free slot (capacity 64 exhausted).
    NoFreeSlot,
    /// No free MIDI track (capacity [`MAX_MIDI_TRACKS`] exhausted).
    NoFreeTrack,
    /// Cable count over capacity (> 128).
    TooManyCables,
    /// Topology contains a cycle (D10: rejected).
    Cycle,
    /// A node exceeds its in-edge limit (> 8).
    FanInOverflow,
    /// MIDI track fan-out exceeds the limit (> [`MAX_MIDI_FANOUT`]).
    MidiFanOutOverflow,
}

/// Control-side graph state (**non-real-time**): slot occupancy + node_id
/// table + cable list + track routing + timeline routing tables; compiles
/// [`ExecPlan`] and produces [`GraphUpdate`]. Fully `Copy` — the engine can
/// roll back to a whole snapshot when a queue push fails.
#[derive(Debug, Clone, Copy)]
pub struct GraphCompiler {
    sample_rate: f32,
    block_size: usize,
    occupied: [bool; MAX_NODES],
    /// Slot → node_id (P5b public addressing key; empty slots are 0, real
    /// IDs start at 1).
    slot_ids: [u64; MAX_NODES],
    /// Slot → node kind (P5f D14/D16: compile-time in-edge discipline).
    kinds: [NodeKind; MAX_NODES],
    /// Audio cable list (`from → to`, deduplicated; P5b replaces the
    /// single-out-edge `out_edge`).
    cables: [Cable; MAX_CABLES],
    cable_count: usize,
    master_slot: u16,
    track_voice: [u16; MAX_MIDI_TRACKS],
    track_delay: [u16; MAX_MIDI_TRACKS],
    track_channel: [u16; MAX_MIDI_TRACKS],
    track_effects: [[u16; EFFECT_SLOTS]; MAX_MIDI_TRACKS],
    /// Timeline MIDI routing table (fan-out destination slots).
    midi_targets: [[u16; MAX_MIDI_FANOUT]; MAX_MIDI_TRACKS],
    midi_target_counts: [u8; MAX_MIDI_TRACKS],
    /// Timeline automation routing table (destination = (slot, param)).
    auto_routes: [AutoRoute; MAX_AUTO_TRACKS],
    next_node_id: u64,
}

impl GraphCompiler {
    /// Builds an empty graph: master output only (node_id = [`MASTER_NODE_ID`]).
    ///
    /// From P5c on (D13 fully explicit) this is the engine's startup shape:
    /// the project document builds the graph step by step through
    /// `add_node` / `register_track_chain` / `connect_nodes`. **Not RT-safe**
    /// (allocates processors and buffers); call only before the audio stream
    /// starts.
    pub fn empty_graph(sample_rate: f32, block_size: usize) -> (Self, RtGraph) {
        let mut compiler = Self {
            sample_rate,
            block_size: block_size.max(1),
            occupied: [false; MAX_NODES],
            slot_ids: [0; MAX_NODES],
            kinds: [NodeKind::Plain; MAX_NODES],
            cables: [Cable {
                from: 0,
                to: 0,
                in_port: 0,
            }; MAX_CABLES],
            cable_count: 0,
            master_slot: 0,
            track_voice: [NO_EDGE; MAX_MIDI_TRACKS],
            track_delay: [NO_EDGE; MAX_MIDI_TRACKS],
            track_channel: [NO_EDGE; MAX_MIDI_TRACKS],
            track_effects: [[NO_EDGE; EFFECT_SLOTS]; MAX_MIDI_TRACKS],
            midi_targets: [[NO_EDGE; MAX_MIDI_FANOUT]; MAX_MIDI_TRACKS],
            midi_target_counts: [0; MAX_MIDI_TRACKS],
            auto_routes: [AutoRoute::default(); MAX_AUTO_TRACKS],
            next_node_id: MASTER_NODE_ID,
        };
        let mut graph = RtGraph::new(block_size);

        let master_id = compiler.fresh_id();
        let master = compiler.alloc_slot();
        compiler.master_slot = master;
        compiler.slot_ids[master as usize] = master_id;
        compiler.kinds[master as usize] = NodeKind::Master;
        graph.install(
            master as usize,
            master_id,
            Box::new(GainProcessor::new_at(1.0, compiler.sample_rate)),
            AudioBuffer::new(2, compiler.block_size),
        );

        graph.plan = compiler.compile().expect("master-only graph must compile");
        (compiler, graph)
    }

    /// Builds the reference topology (P5f D18): master output + per-track
    /// `instrument → delay → mixer.ch[i]` + one global mixer → master.
    /// **Not RT-safe** (allocates processors and buffers); call only before
    /// the audio stream starts.
    ///
    /// From P5c on (D13 fully explicit) this is retired as the engine's
    /// test/reference implementation: the production path builds the graph
    /// step by step from the project document (see [`Self::empty_graph`]).
    /// The model layer's `default_graph_doc` is the authoritative generator
    /// for the "new project initial document".
    pub fn default_graph(sample_rate: f32, block_size: usize) -> (Self, RtGraph) {
        let (mut compiler, mut graph) = Self::empty_graph(sample_rate, block_size);
        let bs = compiler.block_size;

        let track_count = DEFAULT_MIDI_TRACKS;
        for t in 0..track_count {
            let slots = compiler.allocate_track_chain(t);
            for (kind, &(slot, id)) in slots.iter().enumerate() {
                graph.install(
                    slot as usize,
                    id,
                    chain_processor(kind, sample_rate),
                    AudioBuffer::new(2, bs),
                );
            }
        }

        // The global mixer (D18): one channel per track.
        let mixer_slot = compiler.alloc_slot();
        compiler.kinds[mixer_slot as usize] = NodeKind::Mixer;
        let mixer_id = compiler.fresh_id();
        compiler.slot_ids[mixer_slot as usize] = mixer_id;
        graph.install(
            mixer_slot as usize,
            mixer_id,
            Box::new(MixerProcessor::new(track_count.max(2))),
            AudioBuffer::new(2, bs),
        );
        for t in 0..track_count {
            compiler
                .add_cable(compiler.track_delay[t], mixer_slot, t as u8)
                .expect("fresh chain cables");
        }
        compiler
            .add_cable(mixer_slot, compiler.master_slot, 0)
            .expect("fresh chain cables");

        // The default topology is acyclic by construction; failure means a
        // startup-time bug, so panicking directly is acceptable.
        graph.plan = compiler
            .compile()
            .expect("default graph topology must be acyclic");
        (compiler, graph)
    }

    /// Allocates the default pair for an (unconfigured) MIDI track: the
    /// slots, node_ids, routing table entries and cables of
    /// `instrument → delay` plus the track's MIDI self-connection
    /// (P5f D18: mixing happens on explicit mixer nodes, wired by callers).
    /// Returns `[(slot, node_id); 2]` (order: instrument / delay).
    /// Called only from construction/add-track paths (callers must ensure
    /// capacity first). The metering table (`track_channel`) points at the
    /// delay node — the track's chain end under D18.
    fn allocate_track_chain(&mut self, track: usize) -> [(u16, u64); 2] {
        let voice = self.alloc_slot();
        let delay = self.alloc_slot();
        self.track_voice[track] = voice;
        self.track_delay[track] = delay;
        self.track_channel[track] = delay; // metering point = chain end (P5f)
        // Cables: instrument → delay (acyclic by construction; panic ok).
        self.add_cable(voice, delay, 0).expect("fresh chain cables");
        // Timeline default wiring: MIDI track t → its own instrument (mirrors
        // the old mixer semantics).
        self.midi_targets[track][0] = voice;
        self.midi_target_counts[track] = 1;

        let mut out = [(0u16, 0u64); 2];
        for (slot, entry) in [voice, delay].into_iter().zip(out.iter_mut()) {
            let id = self.fresh_id();
            self.slot_ids[slot as usize] = id;
            self.kinds[slot as usize] = NodeKind::Plain;
            *entry = (slot, id);
        }
        out
    }

    /// Undoes [`Self::allocate_track_chain`] (the rollback path when compilation fails).
    fn deallocate_track_chain(&mut self, track: usize) {
        let slots = [self.track_voice[track], self.track_delay[track]];
        self.remove_cables_touching(&slots);
        for slot in slots {
            if slot != NO_EDGE {
                self.occupied[slot as usize] = false;
                self.slot_ids[slot as usize] = 0;
                self.kinds[slot as usize] = NodeKind::Plain;
            }
        }
        self.track_voice[track] = NO_EDGE;
        self.track_delay[track] = NO_EDGE;
        self.track_channel[track] = NO_EDGE;
        self.midi_targets[track] = [NO_EDGE; MAX_MIDI_FANOUT];
        self.midi_target_counts[track] = 0;
    }

    fn fresh_id(&mut self) -> u64 {
        let id = self.next_node_id;
        self.next_node_id += 1;
        id
    }

    /// Allocates the next session-monotonic node_id (used by the document
    /// rebuild path: the UI treats it as the engine-side identity, and the
    /// document id → engine id mapping is maintained by the caller; recycled
    /// ids are never reused within a session so plugin guard pairing is
    /// unambiguous).
    pub fn fresh_node_id(&mut self) -> u64 {
        self.fresh_id()
    }

    /// node_id → slot resolution (returns `None` when unassigned / empty
    /// slot). node_id 0 is always invalid.
    pub fn node_slot(&self, node_id: u64) -> Option<u16> {
        if node_id == 0 {
            return None;
        }
        for (slot, &id) in self.slot_ids.iter().enumerate() {
            if id == node_id && self.occupied[slot] {
                return Some(slot as u16);
            }
        }
        None
    }

    /// node_id of a track's instrument (used by UI / loader to resolve track
    /// coordinates into node addressing).
    pub fn track_voice_node_id(&self, track: usize) -> Option<u64> {
        self.track_table_id(self.track_voice, track)
    }

    /// node_id of a track's delay node.
    pub fn track_delay_node_id(&self, track: usize) -> Option<u64> {
        self.track_table_id(self.track_delay, track)
    }

    /// node_id of a track's channel strip.
    pub fn track_channel_node_id(&self, track: usize) -> Option<u64> {
        self.track_table_id(self.track_channel, track)
    }

    fn track_table_id(&self, table: [u16; MAX_MIDI_TRACKS], track: usize) -> Option<u64> {
        let slot = *table.get(track)?;
        if slot == NO_EDGE {
            return None;
        }
        self.slot_ids
            .get(slot as usize)
            .copied()
            .filter(|&id| id != 0)
    }

    /// node_id of the master output node (always present, D6).
    pub fn master_node_id(&self) -> u64 {
        self.slot_ids[self.master_slot as usize]
    }

    /// node_id of the first occupied slot of the given kind (P5f: the UI
    /// finds the global mixer node this way). Empty when no such node exists.
    pub fn node_id_of_kind(&self, kind: NodeKind) -> Option<u64> {
        self.kinds
            .iter()
            .zip(self.occupied.iter())
            .position(|(&k, &occupied)| k == kind && occupied)
            .and_then(|slot| self.slot_ids.get(slot).copied().filter(|&id| id != 0))
    }

    /// Number of currently configured MIDI tracks (the contiguous run of
    /// `track_voice` that is not `NO_EDGE`).
    pub fn midi_track_count(&self) -> usize {
        self.track_voice
            .iter()
            .position(|&s| s == NO_EDGE)
            .unwrap_or(MAX_MIDI_TRACKS)
    }

    // ---- cable list primitives ----

    fn has_cable(&self, from: u16, to: u16) -> bool {
        self.cables[..self.cable_count]
            .iter()
            .any(|c| c.from == from && c.to == to)
    }

    /// Appends a cable. Every audio input slot holds exactly one cable
    /// (strict single-input, D14): a new cable onto an occupied `(to,
    /// in_port)` slot disconnects the previous one. Exact duplicates are
    /// idempotent. Returns [`TopologyError::TooManyCables`] when full.
    fn add_cable(&mut self, from: u16, to: u16, in_port: u8) -> Result<(), TopologyError> {
        if self.cables[..self.cable_count]
            .iter()
            .any(|c| c.from == from && c.to == to && c.in_port == in_port)
        {
            return Ok(());
        }
        self.remove_cables_to_port(to, in_port);
        if self.cable_count >= MAX_CABLES {
            return Err(TopologyError::TooManyCables);
        }
        self.cables[self.cable_count] = Cable { from, to, in_port };
        self.cable_count += 1;
        Ok(())
    }

    /// Removes every cable entering the input slot `(to, in_port)` (strict
    /// single-input replacement; returns how many were removed).
    fn remove_cables_to_port(&mut self, to: u16, in_port: u8) -> usize {
        let mut removed = 0;
        let mut i = 0;
        while i < self.cable_count {
            if self.cables[i].to == to && self.cables[i].in_port == in_port {
                self.cables.copy_within(i + 1..self.cable_count, i);
                self.cable_count -= 1;
                removed += 1;
            } else {
                i += 1;
            }
        }
        removed
    }

    /// Removes one cable (idempotent, returns `false` when not connected).
    fn remove_cable(&mut self, from: u16, to: u16) -> bool {
        let Some(pos) = self.cables[..self.cable_count]
            .iter()
            .position(|c| c.from == from && c.to == to)
        else {
            return false;
        };
        self.cables.copy_within(pos + 1..self.cable_count, pos);
        self.cable_count -= 1;
        true
    }

    /// Removes all cables touching any of the given slots (out-edges and
    /// in-edges; called on node/track removal).
    fn remove_cables_touching(&mut self, slots: &[u16]) {
        let mut i = 0;
        while i < self.cable_count {
            if slots.contains(&self.cables[i].from) || slots.contains(&self.cables[i].to) {
                self.cables.copy_within(i + 1..self.cable_count, i);
                self.cable_count -= 1;
            } else {
                i += 1;
            }
        }
    }

    fn alloc_slot(&mut self) -> u16 {
        let slot = self
            .occupied
            .iter()
            .position(|&o| !o)
            .expect("graph slot capacity") as u16;
        self.occupied[slot as usize] = true;
        slot
    }

    /// Packs one structural update (control side allocates output buffers;
    /// batch installs allowed).
    /// Non-real-time path: `deployments` carries heap resources in a `Vec`
    /// (≤ [`MAX_DEPLOYMENTS`]).
    #[allow(clippy::type_complexity)]
    fn build_update(
        &self,
        plan: ExecPlan,
        deployments: Vec<(u16, u64, Box<dyn Processor>)>,
        removed: &[u16],
    ) -> GraphUpdate {
        let mut batch = std::array::from_fn(|_| NodeDeployment {
            slot: 0,
            node_id: 0,
            node: None,
        });
        let deployment_count = deployments.len().min(MAX_DEPLOYMENTS);
        for (batch_slot, (slot, node_id, node)) in deployments.into_iter().enumerate() {
            if batch_slot >= MAX_DEPLOYMENTS {
                // Defensive path: installs beyond the single-update capacity
                // are dropped directly (the control thread may drop).
                drop(node);
                continue;
            }
            batch[batch_slot] = NodeDeployment {
                slot,
                node_id,
                node: Some((node, AudioBuffer::new(2, self.block_size))),
            };
        }
        let mut removed_arr = [0u16; MAX_REMOVALS];
        let removed_count = removed.len().min(MAX_REMOVALS);
        removed_arr[..removed_count].copy_from_slice(&removed[..removed_count]);
        GraphUpdate::SetTopology {
            plan,
            deployments: batch,
            deployment_count,
            removed: removed_arr,
            removed_count,
        }
    }

    /// Compiles the execution plan: in-edge table (with per-edge destination
    /// channels, D17) + Kahn topological sort. Compile-time input discipline
    /// (P5f D14/D16): plain/master slots take ≤ 1 in-edge, mixer slots ≤
    /// [`MAX_IN_EDGES`] — overflow rejects the whole plan.
    pub fn compile(&self) -> Result<ExecPlan, TopologyError> {
        let mut plan = ExecPlan::EMPTY;
        let mut in_counts = [0u8; MAX_NODES];
        for cable in &self.cables[..self.cable_count] {
            // Defensive: only count cables whose both ends are occupied
            // (dangling cables do not join the topology).
            if !self.occupied[cable.from as usize] || !self.occupied[cable.to as usize] {
                continue;
            }
            let d = cable.to as usize;
            let limit = self.kinds[d].max_in();
            if in_counts[d] as usize >= limit {
                return Err(TopologyError::FanInOverflow);
            }
            plan.in_edges[d][in_counts[d] as usize] = cable.from;
            plan.in_ports[d][in_counts[d] as usize] = cable.in_port;
            in_counts[d] += 1;
        }

        // Ordering degrees (P6): a normal cable constrains `to` after
        // `from`; a cable *into* a feedback node is a tap, so the
        // constraint is reversed (the feedback node runs first and reads
        // the tap source's previous-block buffer). Reversal never creates a
        // new cycle: `F → … → X` on top of `X → F` was already a cycle.
        let mut degree = [0u16; MAX_NODES];
        for (slot, count) in in_counts.iter().enumerate() {
            // A feedback node's in-edges are taps: they never gate the node
            // itself (it is a root by construction).
            if self.kinds[slot] == NodeKind::Feedback {
                continue;
            }
            degree[slot] = u16::from(*count);
        }
        for cable in &self.cables[..self.cable_count] {
            if !self.occupied[cable.from as usize] || !self.occupied[cable.to as usize] {
                continue;
            }
            if self.kinds[cable.to as usize] == NodeKind::Feedback {
                degree[cable.from as usize] += 1;
            }
        }
        let mut work = [0u16; MAX_NODES];
        let mut work_len = 0usize;
        let mut head = 0usize;
        for (slot, &occupied) in self.occupied.iter().enumerate() {
            if occupied && degree[slot] == 0 {
                work[work_len] = slot as u16;
                work_len += 1;
            }
        }
        while head < work_len {
            let s = work[head] as usize;
            head += 1;
            plan.order[plan.order_len] = s as u16;
            plan.order_len += 1;
            // Traverse all out-edges of this slot (multi-out fan-out: each
            // downstream in-degree decrements by one). Edges into a
            // feedback node do not constrain their destination (see above).
            for cable in &self.cables[..self.cable_count] {
                if cable.from as usize != s
                    || !self.occupied[cable.from as usize]
                    || !self.occupied[cable.to as usize]
                {
                    continue;
                }
                if self.kinds[cable.to as usize] == NodeKind::Feedback {
                    continue;
                }
                let d = cable.to as usize;
                if degree[d] > 0 {
                    degree[d] -= 1;
                }
                if degree[d] == 0 {
                    work[work_len] = cable.to;
                    work_len += 1;
                }
            }
            // This node's tap edges (only meaningful on a feedback node):
            // releasing their sources is what lets the rest of the graph
            // run after it.
            if self.kinds[s] == NodeKind::Feedback {
                for cable in &self.cables[..self.cable_count] {
                    if cable.to as usize != s
                        || !self.occupied[cable.from as usize]
                        || !self.occupied[cable.to as usize]
                    {
                        continue;
                    }
                    let src = cable.from as usize;
                    let Some(next) = degree[src].checked_sub(1) else {
                        // Self-loop on a feedback node: its own tap can
                        // never be satisfied (a pure delay has no source).
                        return Err(TopologyError::Cycle);
                    };
                    degree[src] = next;
                    if next == 0 {
                        work[work_len] = cable.from;
                        work_len += 1;
                    }
                }
            }
        }

        let occupied_count = self.occupied.iter().filter(|&&o| o).count();
        if plan.order_len != occupied_count {
            return Err(TopologyError::Cycle);
        }
        plan.in_counts = in_counts;
        plan.node_ids = self.slot_ids;
        plan.master_slot = self.master_slot;
        plan.track_voice = self.track_voice;
        plan.track_delay = self.track_delay;
        plan.track_channel = self.track_channel;
        plan.midi_targets = self.midi_targets;
        plan.midi_target_counts = self.midi_target_counts;
        plan.auto_routes = self.auto_routes;
        Ok(plan)
    }

    /// Replaces a track's instrument with the given processor (the old node
    /// is released via the recycle queue).
    /// On failure, hands `node` back unchanged for the caller to release.
    pub fn set_voice(
        &mut self,
        track: usize,
        node: Box<dyn Processor>,
    ) -> Result<(GraphUpdate, u64), Box<dyn Processor>> {
        if track >= MAX_MIDI_TRACKS || self.track_voice[track] == NO_EDGE {
            return Err(node);
        }
        let slot = self.track_voice[track];
        let node_id = self.fresh_id();
        self.slot_ids[slot as usize] = node_id;
        let Ok(plan) = self.compile() else {
            return Err(node);
        };
        let update = self.build_update(plan, vec![(slot, node_id, node)], &[slot]);
        Ok((update, node_id))
    }

    /// Restores a track's instrument to the built-in synthesizer (the old
    /// external node is released via the recycle queue).
    ///
    /// Behavior change (relative to the old `TrackStrip` residency
    /// semantics): the external node is removed rather than resident;
    /// switching back to a plugin requires re-instantiation (state is not
    /// preserved). UI guards are released normally, pairing with the
    /// recycled `node_id`.
    pub fn activate_builtin_voice(&mut self, track: usize) -> Result<GraphUpdate, TopologyError> {
        if track >= MAX_MIDI_TRACKS || self.track_voice[track] == NO_EDGE {
            return Err(TopologyError::InvalidTrack);
        }
        let slot = self.track_voice[track];
        let node_id = self.fresh_id();
        let node = Box::new(SynthProcessor::new(self.sample_rate));
        self.slot_ids[slot as usize] = node_id;
        let plan = self.compile()?;
        Ok(self.build_update(plan, vec![(slot, node_id, node)], &[slot]))
    }

    /// Appends an effect node at the end of a track's chain: spliced between
    /// the track's delay node and its current downstream target (P5f: the
    /// per-track strip is gone; the chain end feeds a mixer channel or
    /// dangles). When no free slot exists, hands `node` back unchanged.
    pub fn insert_effect(
        &mut self,
        track: usize,
        node: Box<dyn Processor>,
    ) -> Result<(GraphUpdate, u64), Box<dyn Processor>> {
        if track >= MAX_MIDI_TRACKS || self.track_delay[track] == NO_EDGE {
            return Err(node);
        }
        let Some(free_idx) = self.occupied.iter().position(|&o| !o) else {
            return Err(node);
        };
        let delay = self.track_delay[track];
        // The delay's current downstream edge (if any) — the new effect is
        // spliced into it.
        let downstream = self.cables[..self.cable_count]
            .iter()
            .find(|c| c.from == delay)
            .map(|c| (c.to, c.in_port));

        let free = free_idx as u16;
        self.occupied[free as usize] = true;
        self.kinds[free as usize] = NodeKind::Plain;
        if let Some((to, port)) = downstream {
            self.remove_cable(delay, to);
            if self
                .add_cable(delay, free, 0)
                .and_then(|()| self.add_cable(free, to, port))
                .is_err()
            {
                self.occupied[free as usize] = false;
                self.kinds[free as usize] = NodeKind::Plain;
                let _ = self.remove_cable(delay, free);
                let _ = self.remove_cable(free, to);
                let _ = self.add_cable(delay, to, port);
                return Err(node);
            }
        } else if self.add_cable(delay, free, 0).is_err() {
            self.occupied[free as usize] = false;
            self.kinds[free as usize] = NodeKind::Plain;
            return Err(node);
        }
        if let Some(e) = self.track_effects[track]
            .iter_mut()
            .find(|e| **e == NO_EDGE)
        {
            *e = free;
        }
        let plan = match self.compile() {
            Ok(plan) => plan,
            Err(_) => {
                // Rollback: clear the new slot's in-edges and out-edges together.
                self.remove_cables_touching(&[free]);
                self.occupied[free as usize] = false;
                self.slot_ids[free as usize] = 0;
                self.kinds[free as usize] = NodeKind::Plain;
                if let Some((to, port)) = downstream {
                    let _ = self.add_cable(delay, to, port);
                } else {
                    let _ = self.remove_cable(delay, free);
                }
                if let Some(e) = self.track_effects[track].iter_mut().find(|e| **e == free) {
                    *e = NO_EDGE;
                }
                return Err(node);
            }
        };
        let node_id = self.fresh_id();
        self.slot_ids[free as usize] = node_id;
        let update = self.build_update(plan, vec![(free, node_id, node)], &[]);
        Ok((update, node_id))
    }

    /// Clears all external effects of a track (nodes are released via the
    /// recycle queue).
    pub fn clear_effects(&mut self, track: usize) -> Result<GraphUpdate, TopologyError> {
        if track >= MAX_MIDI_TRACKS {
            return Err(TopologyError::InvalidTrack);
        }
        let delay = self.track_delay[track];
        // Capture the first downstream node outside the effect table (where
        // the chain must re-attach once the effects are gone).
        let mut target = NO_EDGE;
        let mut target_port = 0u8;
        if delay != NO_EDGE {
            let mut cur = delay;
            for _ in 0..MAX_NODES {
                let Some(next) = self.cables[..self.cable_count]
                    .iter()
                    .find(|c| c.from == cur)
                else {
                    break;
                };
                if self.track_effects[track].contains(&next.to) {
                    cur = next.to;
                } else {
                    target = next.to;
                    target_port = next.in_port;
                    break;
                }
            }
        }
        let mut removed = [0u16; MAX_REMOVALS];
        let mut removed_count = 0usize;
        for effect in self.track_effects[track] {
            if effect != NO_EDGE && removed_count < MAX_REMOVALS {
                // Clear in-edges and out-edges together (dangling cables
                // would confuse topology compilation).
                self.remove_cables_touching(&[effect]);
                self.occupied[effect as usize] = false;
                self.slot_ids[effect as usize] = 0;
                self.kinds[effect as usize] = NodeKind::Plain;
                removed[removed_count] = effect;
                removed_count += 1;
            }
        }
        self.track_effects[track] = [NO_EDGE; EFFECT_SLOTS];
        if delay != NO_EDGE
            && target != NO_EDGE
            && !self.cables[..self.cable_count]
                .iter()
                .any(|c| c.from == delay)
        {
            self.add_cable(delay, target, target_port)?;
        }
        let plan = self.compile()?;
        Ok(self.build_update(plan, Vec::new(), &removed[..removed_count]))
    }

    /// Adds a MIDI track: installs the default pair (`instrument → delay`)
    /// and writes the track's MIDI self-connection (P5f D18: mixing happens
    /// on explicit mixer nodes — callers wire `delay → mixer.ch[i]`).
    /// Returns `(structural update, [voice, delay] node_ids)`.
    ///
    /// Track capacity full → [`TopologyError::NoFreeTrack`]; not enough slots
    /// → [`TopologyError::NoFreeSlot`]; over-capacity cables →
    /// [`TopologyError::TooManyCables`].
    pub fn add_midi_track(&mut self) -> Result<(GraphUpdate, [u64; 2]), TopologyError> {
        let Some(track) = self.track_voice.iter().position(|&s| s == NO_EDGE) else {
            return Err(TopologyError::NoFreeTrack);
        };
        // Precheck capacity (alloc_slot panics when full, so it must be
        // blocked early).
        if self.occupied.iter().filter(|&&o| !o).count() < 2 {
            return Err(TopologyError::NoFreeSlot);
        }
        if self.cable_count + 1 > MAX_CABLES {
            return Err(TopologyError::TooManyCables);
        }
        let slots = self.allocate_track_chain(track);
        let Ok(plan) = self.compile() else {
            // Rollback the allocation.
            self.deallocate_track_chain(track);
            return Err(TopologyError::FanInOverflow);
        };
        let ids = [slots[0].1, slots[1].1];
        let deployments: Vec<(u16, u64, Box<dyn Processor>)> = slots
            .into_iter()
            .enumerate()
            .map(|(kind, (slot, id))| (slot, id, chain_processor(kind, self.sample_rate)))
            .collect();
        Ok((self.build_update(plan, deployments, &[]), ids))
    }

    /// Tears down a MIDI track: the trio plus all its effects are removed
    /// (cable cleanup + slot release + routing table cleared), nodes released
    /// via the recycle queue. Idempotent success for unconfigured tracks.
    pub fn remove_midi_track(&mut self, track: usize) -> Result<GraphUpdate, TopologyError> {
        if track >= MAX_MIDI_TRACKS || self.track_voice[track] == NO_EDGE {
            return Err(TopologyError::InvalidTrack);
        }
        let mut slots = [NO_EDGE; 3 + EFFECT_SLOTS];
        let mut count = 0usize;
        for slot in [
            self.track_voice[track],
            self.track_delay[track],
            self.track_channel[track],
        ] {
            if slot != NO_EDGE {
                slots[count] = slot;
                count += 1;
            }
        }
        for effect in self.track_effects[track] {
            if effect != NO_EDGE && count < slots.len() {
                slots[count] = effect;
                count += 1;
            }
        }
        self.remove_cables_touching(&slots[..count]);
        for &slot in &slots[..count] {
            self.occupied[slot as usize] = false;
            self.slot_ids[slot as usize] = 0;
        }
        self.track_voice[track] = NO_EDGE;
        self.track_delay[track] = NO_EDGE;
        self.track_channel[track] = NO_EDGE;
        self.track_effects[track] = [NO_EDGE; EFFECT_SLOTS];
        self.midi_targets[track] = [NO_EDGE; MAX_MIDI_FANOUT];
        self.midi_target_counts[track] = 0;
        let plan = self.compile()?;
        Ok(self.build_update(plan, Vec::new(), &slots[..count]))
    }

    /// Timeline wiring: connects MIDI track `track` to a target node
    /// (**node_id addressing**, P5b) — duplicate connections return
    /// idempotently; the target node must already exist. The routing table
    /// does not participate in audio topological sorting, so this can only
    /// fail on fan-out overflow.
    pub fn connect_midi(
        &mut self,
        track: usize,
        node_id: u64,
    ) -> Result<GraphUpdate, TopologyError> {
        if track >= MAX_MIDI_TRACKS {
            return Err(TopologyError::InvalidTrack);
        }
        let slot = self.node_slot(node_id).ok_or(TopologyError::InvalidSlot)?;
        if self.midi_targets[track][..self.midi_target_counts[track] as usize].contains(&slot) {
            // Idempotent: duplicate connections count as success (returns the
            // current plan).
            return Ok(self.build_update(self.compile()?, Vec::new(), &[]));
        }
        let count = self.midi_target_counts[track] as usize;
        if count >= MAX_MIDI_FANOUT {
            return Err(TopologyError::MidiFanOutOverflow);
        }
        self.midi_targets[track][count] = slot;
        self.midi_target_counts[track] += 1;
        Ok(self.build_update(self.compile()?, Vec::new(), &[]))
    }

    /// Timeline wiring: disconnects MIDI track `track` from a target node
    /// (idempotent when unconnected).
    pub fn disconnect_midi(
        &mut self,
        track: usize,
        node_id: u64,
    ) -> Result<GraphUpdate, TopologyError> {
        if track >= MAX_MIDI_TRACKS {
            return Err(TopologyError::InvalidTrack);
        }
        let slot = self.node_slot(node_id).ok_or(TopologyError::InvalidSlot)?;
        let count = self.midi_target_counts[track] as usize;
        let Some(pos) = self.midi_targets[track][..count]
            .iter()
            .position(|&s| s == slot)
        else {
            return Ok(self.build_update(self.compile()?, Vec::new(), &[]));
        };
        self.midi_targets[track].copy_within(pos + 1..count, pos);
        self.midi_targets[track][count - 1] = NO_EDGE;
        self.midi_target_counts[track] -= 1;
        Ok(self.build_update(self.compile()?, Vec::new(), &[]))
    }

    /// Timeline wiring: sets the delivery destination of automation track
    /// `auto_index`.
    ///
    /// The destination is expressed as a **node_id** (P5b: replaces the
    /// transitional semantics of guessing the destination by parameter kind;
    /// the UI resolves addressing based on the node chosen in the
    /// patch/panel). Setting again simply redirects.
    pub fn set_auto_route(
        &mut self,
        auto_index: usize,
        node_id: u64,
        param: ParamId,
    ) -> Result<GraphUpdate, TopologyError> {
        if auto_index >= MAX_AUTO_TRACKS {
            return Err(TopologyError::InvalidIndex);
        }
        let slot = self.node_slot(node_id).ok_or(TopologyError::InvalidSlot)?;
        self.auto_routes[auto_index] = AutoRoute {
            slot,
            dest: AutoDest::Param(param),
        };
        Ok(self.build_update(self.compile()?, Vec::new(), &[]))
    }

    /// Timeline wiring: routes automation track `auto_index` to a **MIDI CC**
    /// on `node_id` (U4a). Same node-addressed path as [`Self::set_auto_route`];
    /// the destination kind tells `RtGraph` to emit an event instead of
    /// setting a parameter.
    pub fn set_auto_cc_route(
        &mut self,
        auto_index: usize,
        node_id: u64,
        channel: u8,
        controller: u8,
    ) -> Result<GraphUpdate, TopologyError> {
        if auto_index >= MAX_AUTO_TRACKS {
            return Err(TopologyError::InvalidIndex);
        }
        let slot = self.node_slot(node_id).ok_or(TopologyError::InvalidSlot)?;
        self.auto_routes[auto_index] = AutoRoute {
            slot,
            dest: AutoDest::Cc {
                controller: controller.min(127),
                channel: channel.min(15),
            },
        };
        Ok(self.build_update(self.compile()?, Vec::new(), &[]))
    }

    /// Timeline wiring: connects automation track `auto_index` to the
    /// **Timeline In** endpoint (U3b): the sampled curve overrides the
    /// transport BPM for the next block instead of setting a node parameter.
    /// No node lookup happens here — the sentinel never enters `slots`.
    pub fn set_timeline_in_route(
        &mut self,
        auto_index: usize,
    ) -> Result<GraphUpdate, TopologyError> {
        if auto_index >= MAX_AUTO_TRACKS {
            return Err(TopologyError::InvalidIndex);
        }
        // `param` is unused for the conductor route (v1 has one port); keep
        // the default so the table stays `Copy` and comparable.
        self.auto_routes[auto_index] = AutoRoute {
            slot: TIMELINE_IN,
            dest: AutoDest::Param(ParamId::MasterGain),
        };
        Ok(self.build_update(self.compile()?, Vec::new(), &[]))
    }

    /// Timeline wiring: disconnects automation track `auto_index` (idempotent
    /// when unconnected).
    pub fn clear_auto_route(&mut self, auto_index: usize) -> Result<GraphUpdate, TopologyError> {
        if auto_index >= MAX_AUTO_TRACKS {
            return Err(TopologyError::InvalidIndex);
        }
        self.auto_routes[auto_index] = AutoRoute::default();
        Ok(self.build_update(self.compile()?, Vec::new(), &[]))
    }

    // ---- incremental document-based graph building API (P5c, D13 fully
    // explicit: the loader rebuilds the graph from the document) ----

    /// Installs a document node (**explicit node_id**, D11: persisted with
    /// the file).
    ///
    /// node_id 0 (invalid sentinel) or 1 (master output reserved) →
    /// [`TopologyError::InvalidSlot`]; duplicate ids or full slots error out
    /// as well. `next_node_id` advances regardless of success or failure
    /// (guaranteeing later fresh allocations never collide with document ids).
    /// `kind` sets the node's compile-time input discipline (P5f D14/D16).
    pub fn add_node(
        &mut self,
        node_id: u64,
        kind: NodeKind,
        node: Box<dyn Processor>,
    ) -> Result<GraphUpdate, TopologyError> {
        if node_id <= MASTER_NODE_ID {
            return Err(TopologyError::InvalidSlot);
        }
        self.next_node_id = self.next_node_id.max(node_id + 1);
        if self.node_slot(node_id).is_some() {
            return Err(TopologyError::InvalidSlot);
        }
        let Some(free_idx) = self.occupied.iter().position(|&o| !o) else {
            return Err(TopologyError::NoFreeSlot);
        };
        let free = free_idx as u16;
        self.occupied[free as usize] = true;
        self.slot_ids[free as usize] = node_id;
        self.kinds[free as usize] = kind;
        let plan = match self.compile() {
            Ok(plan) => plan,
            Err(_) => {
                self.occupied[free as usize] = false;
                self.slot_ids[free as usize] = 0;
                self.kinds[free as usize] = NodeKind::Plain;
                return Err(TopologyError::Cycle);
            }
        };
        Ok(self.build_update(plan, vec![(free, node_id, node)], &[]))
    }

    /// Assembles the track addressing tables (P5c): maps a track's trio of
    /// node_ids from the document into `track_voice` / `track_delay` /
    /// `track_channel` so that the legacy track-coordinate APIs (manual
    /// parameters, level reporting, keyboard playing) keep working. Pass
    /// `None` for any role absent from the document; the corresponding entry
    /// is cleared. The track effects table is cleared.
    pub fn register_track_chain(
        &mut self,
        track: usize,
        voice: Option<u64>,
        delay: Option<u64>,
        channel: Option<u64>,
    ) -> Result<GraphUpdate, TopologyError> {
        if track >= MAX_MIDI_TRACKS {
            return Err(TopologyError::InvalidTrack);
        }
        let resolve = |compiler: &Self, id: Option<u64>| -> Result<u16, TopologyError> {
            match id {
                None => Ok(NO_EDGE),
                Some(id) => compiler.node_slot(id).ok_or(TopologyError::InvalidSlot),
            }
        };
        self.track_voice[track] = resolve(self, voice)?;
        self.track_delay[track] = resolve(self, delay)?;
        self.track_channel[track] = resolve(self, channel)?;
        self.track_effects[track] = [NO_EDGE; EFFECT_SLOTS];
        Ok(self.build_update(self.compile()?, Vec::new(), &[]))
    }

    /// Audio cable between nodes (**node_id addressing**, the engine landing
    /// point of document cables). `in_port` is the destination mixer channel
    /// (D17; 0 for non-mixer targets).
    /// Duplicate connections are idempotent; destination in-edge overflow /
    /// cycles / over-capacity error out (rollback on failure).
    pub fn connect_nodes(
        &mut self,
        from_id: u64,
        to_id: u64,
        in_port: u8,
    ) -> Result<GraphUpdate, TopologyError> {
        let from = self.node_slot(from_id).ok_or(TopologyError::InvalidSlot)?;
        let to = self.node_slot(to_id).ok_or(TopologyError::InvalidSlot)?;
        if self.add_cable(from, to, in_port).is_err() {
            return Err(TopologyError::TooManyCables);
        }
        match self.compile() {
            Ok(plan) => Ok(self.build_update(plan, Vec::new(), &[])),
            Err(err) => {
                let _ = self.remove_cable(from, to);
                Err(err)
            }
        }
    }

    /// Disconnects an audio cable between nodes (idempotent success when
    /// unconnected).
    pub fn disconnect_nodes(
        &mut self,
        from_id: u64,
        to_id: u64,
    ) -> Result<GraphUpdate, TopologyError> {
        let from = self.node_slot(from_id).ok_or(TopologyError::InvalidSlot)?;
        let to = self.node_slot(to_id).ok_or(TopologyError::InvalidSlot)?;
        self.remove_cable(from, to);
        Ok(self.build_update(self.compile()?, Vec::new(), &[]))
    }

    /// Replaces a node's processor in place (**keeps the node_id**, D11:
    /// node identity unchanged; the old processor is released via the
    /// recycle queue). Shared by built-in/plugin instrument and effect swaps.
    pub fn replace_node(
        &mut self,
        node_id: u64,
        node: Box<dyn Processor>,
    ) -> Result<(GraphUpdate, u64), Box<dyn Processor>> {
        let Some(slot) = self.node_slot(node_id) else {
            return Err(node);
        };
        let Ok(plan) = self.compile() else {
            return Err(node);
        };
        // apply_update removes first, installs second: the old node in the
        // same slot dequeues, the new node enqueues.
        Ok((
            self.build_update(plan, vec![(slot, node_id, node)], &[slot]),
            node_id,
        ))
    }

    /// Inserts a node between an existing cable `from_id → to_id` (the engine
    /// landing point of document external effects). On failure, hands `node`
    /// back unchanged and restores the original cable.
    pub fn insert_node_between(
        &mut self,
        new_id: u64,
        from_id: u64,
        to_id: u64,
        node: Box<dyn Processor>,
    ) -> Result<GraphUpdate, Box<dyn Processor>> {
        if new_id <= MASTER_NODE_ID {
            return Err(node);
        }
        self.next_node_id = self.next_node_id.max(new_id + 1);
        let Some(from) = self.node_slot(from_id) else {
            return Err(node);
        };
        let Some(to) = self.node_slot(to_id) else {
            return Err(node);
        };
        if self.node_slot(new_id).is_some() {
            return Err(node);
        }
        let Some(free_idx) = self.occupied.iter().position(|&o| !o) else {
            return Err(node);
        };
        let free = free_idx as u16;
        self.occupied[free as usize] = true;
        self.slot_ids[free as usize] = new_id;
        self.kinds[free as usize] = NodeKind::Plain;
        // Preserve the original cable's destination channel when splicing:
        // `from → (mixer, ch)` becomes `from → new → (mixer, ch)`.
        let old_port = self.cables[..self.cable_count]
            .iter()
            .find(|c| c.from == from && c.to == to)
            .map(|c| c.in_port)
            .unwrap_or(0);
        self.remove_cable(from, to);
        if self
            .add_cable(from, free, 0)
            .and_then(|()| self.add_cable(free, to, old_port))
            .is_err()
        {
            self.rollback_splice(free, from, to, old_port);
            return Err(node);
        }
        match self.compile() {
            Ok(plan) => Ok(self.build_update(plan, vec![(free, new_id, node)], &[])),
            Err(_) => {
                self.rollback_splice(free, from, to, old_port);
                Err(node)
            }
        }
    }

    /// Rollback for [`Self::insert_node_between`]: removes the new slot's
    /// cables and restores the original one (with its destination channel).
    fn rollback_splice(&mut self, free: u16, from: u16, to: u16, old_port: u8) {
        self.remove_cables_touching(&[free]);
        self.occupied[free as usize] = false;
        self.slot_ids[free as usize] = 0;
        self.kinds[free as usize] = NodeKind::Plain;
        let _ = self.add_cable(from, to, old_port);
    }

    /// Removes a node (**bypass semantics**: all its in-edge sources and
    /// out-edge destinations are reconnected directly, so the signal path
    /// restores naturally — removing an external effect bypasses it). Entries
    /// in the automation routing table and MIDI fan-out tables pointing to it
    /// are cleaned up as well. The master output cannot be removed (D6).
    pub fn remove_node(&mut self, node_id: u64) -> Result<GraphUpdate, TopologyError> {
        let Some(slot) = self.node_slot(node_id) else {
            return Err(TopologyError::InvalidSlot);
        };
        if slot == self.master_slot {
            return Err(TopologyError::InvalidSlot);
        }
        // D19 bypass semantics: only nodes with ≤ 1 in-edge are bypassed
        // (in-edge sources × out-edge destinations become direct pairs).
        // Multi-input nodes (mixers) have no meaningful bypass — their cables
        // are simply dropped.
        let in_count = self.cables[..self.cable_count]
            .iter()
            .filter(|c| c.to == slot)
            .count();
        let mut bypass = [Cable {
            from: 0,
            to: 0,
            in_port: 0,
        }; MAX_NODES];
        let mut bypass_count = 0usize;
        // P6: a feedback node's cables are taps — bypassing it would turn
        // the loop into a direct edge and, usually, into a rejected cycle;
        // its cables are dropped instead (mixer-like).
        let bypassable = in_count <= 1 && self.kinds[slot as usize] != NodeKind::Feedback;
        if bypassable {
            for cable in &self.cables[..self.cable_count] {
                if cable.to == slot {
                    for out in &self.cables[..self.cable_count] {
                        if out.from == slot
                            && !self.has_cable(cable.from, out.to)
                            && !bypass[..bypass_count]
                                .iter()
                                .any(|b| b.from == cable.from && b.to == out.to)
                        {
                            bypass[bypass_count] = Cable {
                                from: cable.from,
                                to: out.to,
                                in_port: out.in_port,
                            };
                            bypass_count += 1;
                        }
                    }
                }
            }
        }
        self.remove_cables_touching(&[slot]);
        for bypass_cable in &bypass[..bypass_count] {
            let _ = self.add_cable(bypass_cable.from, bypass_cable.to, bypass_cable.in_port);
        }
        for track in 0..MAX_MIDI_TRACKS {
            // Remove within the MIDI fan-out table row.
            let mut count = self.midi_target_counts[track] as usize;
            let mut i = 0;
            while i < count {
                if self.midi_targets[track][i] == slot {
                    self.midi_targets[track].copy_within(i + 1..count, i);
                    self.midi_targets[track][count - 1] = NO_EDGE;
                    self.midi_target_counts[track] -= 1;
                    count -= 1;
                } else {
                    i += 1;
                }
            }
            // Track addressing tables and the effects table.
            if self.track_voice[track] == slot {
                self.track_voice[track] = NO_EDGE;
            }
            if self.track_delay[track] == slot {
                self.track_delay[track] = NO_EDGE;
            }
            if self.track_channel[track] == slot {
                self.track_channel[track] = NO_EDGE;
            }
            for effect in &mut self.track_effects[track] {
                if *effect == slot {
                    *effect = NO_EDGE;
                }
            }
        }
        for route in &mut self.auto_routes {
            if route.slot == slot {
                *route = AutoRoute::default();
            }
        }
        self.occupied[slot as usize] = false;
        self.slot_ids[slot as usize] = 0;
        self.kinds[slot as usize] = NodeKind::Plain;
        let plan = self.compile()?;
        Ok(self.build_update(plan, Vec::new(), &[slot]))
    }
}

/// Processor constructor for the track trio (`kind`: 0 = instrument /
/// 1 = delay / 2 = channel strip).
fn chain_processor(kind: usize, sample_rate: f32) -> Box<dyn Processor> {
    match kind {
        0 => Box::new(SynthProcessor::new(sample_rate)),
        1 => Box::new(DelayProcessor::new(sample_rate)),
        _ => Box::new(ChannelStripProcessor::new_at(sample_rate)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChannelStripProcessor, DEFAULT_MIDI_TRACKS, GraphCompiler, GraphUpdate, MAX_AUTO_TRACKS,
        MAX_CABLES, MAX_MIDI_FANOUT, MAX_MIDI_TRACKS, NO_EDGE, NodeKind, RtGraph, TopologyError,
        chain_processor,
    };
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::{NoteEvent, ParamId};
    use crate::realtime::context::ProcessContext;
    use crate::realtime::mixer::MixerProcessor;
    use crate::realtime::processor::Processor;
    use crate::realtime::voice::{RecycledNode, recycle_channel};

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    /// The Err payloads of `set_voice` / `insert_effect` are
    /// `Box<dyn Processor>` (no `Debug`); tests use this helper to assert
    /// success.
    fn expect_swap(
        result: Result<(GraphUpdate, u64), Box<dyn Processor>>,
        msg: &str,
    ) -> (GraphUpdate, u64) {
        match result {
            Ok(pair) => pair,
            Err(_) => panic!("{msg}"),
        }
    }

    /// The Err payload of `insert_node_between` is `Box<dyn Processor>` (no
    /// `Debug`); tests use this helper to assert success.
    fn expect_update(result: Result<GraphUpdate, Box<dyn Processor>>, msg: &str) -> GraphUpdate {
        match result {
            Ok(update) => update,
            Err(_) => panic!("{msg}"),
        }
    }

    fn ctx() -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        }
    }

    fn note_on(note: u8) -> NoteEvent {
        NoteEvent::On {
            note,
            velocity: 1.0,
        }
    }

    fn default_graph() -> RtGraph {
        GraphCompiler::default_graph(SR, BLOCK).1
    }

    fn drain_recycle(recycle: &mut rtrb::Consumer<RecycledNode>) -> Vec<u64> {
        let mut ids = Vec::new();
        while let Ok(n) = recycle.pop() {
            ids.push(n.node_id);
        }
        ids
    }

    // ---- default topology equivalence (ported from the old mixer tests) ----

    /// The default graph's global mixer node_id (P5f D18).
    fn default_mixer_id(compiler: &GraphCompiler) -> u64 {
        compiler
            .node_id_of_kind(NodeKind::Mixer)
            .expect("default graph should carry a global mixer")
    }

    #[test]
    fn note_plays_through_track_to_master() {
        let mut graph = default_graph();
        assert!(graph.track_note(0, note_on(69)));
        let levels = graph.process_block(&ctx());
        assert!(
            levels.master > 0.05,
            "master output should carry a signal after track 0 is triggered"
        );
        assert!(levels.tracks[0] > 0.05);
        for i in 1..DEFAULT_MIDI_TRACKS {
            assert!(
                levels.tracks[i].abs() < 1e-9,
                "untriggered tracks should be silent"
            );
        }
        // In the fixed-capacity 16 tracks, unconfigured tracks (beyond the
        // default 4) are always 0.
        for level in levels.tracks[DEFAULT_MIDI_TRACKS..].iter() {
            assert!(level.abs() < 1e-12, "unconfigured track levels should be 0");
        }
    }

    /// G6 (spec `plugin-gui.md` §8): node-addressed note delivery — exactly
    /// the addressed instrument sounds, the track table stays untouched.
    #[test]
    fn node_note_reaches_only_the_addressed_slot() {
        let (compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let node = compiler
            .track_voice_node_id(1)
            .expect("track 1 should carry an instrument node");
        assert!(graph.node_note(node, note_on(69)));
        let levels = graph.process_block(&ctx());
        assert!(levels.tracks[1] > 0.05, "the addressed instrument sounds");
        assert!(
            levels.tracks[0].abs() < 1e-9,
            "the track fan-out table must not be involved"
        );
        assert!(
            !graph.node_note(u64::MAX, note_on(69)),
            "unknown node ids are dropped"
        );
    }

    #[test]
    fn invalid_track_index_is_ignored() {
        let mut graph = default_graph();
        assert!(!graph.track_note(MAX_MIDI_TRACKS, note_on(60)));
        assert!(!graph.track_note(usize::MAX, note_on(60)));
        graph.track_set_param(MAX_MIDI_TRACKS, ParamId::TrackGain, 1.0);
        let levels = graph.process_block(&ctx());
        assert!(levels.master.abs() < 1e-9);
    }

    #[test]
    fn mixer_gain_scales_the_mix() {
        let (compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let mixer = default_mixer_id(&compiler);
        graph.track_note(0, note_on(69));
        for _ in 0..20 {
            graph.process_block(&ctx());
        }
        let full = graph.process_block(&ctx());
        // Node-addressed fader (P5f: the mixer panel drives mixer channels
        // through the parameter path, never a topology rebuild).
        assert!(graph.node_set_param(mixer, ParamId::MixerGain(0), 0.4));
        let attenuated = graph.process_block(&ctx());
        let ratio = attenuated.master / full.master;
        assert!(
            (ratio - (0.4 / 0.8)).abs() < 0.1,
            "master output should be roughly halved after halving the channel gain, ratio = {ratio}"
        );
    }

    #[test]
    fn mute_silences_a_channel_but_pre_fader_still_runs() {
        let (compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let mixer = default_mixer_id(&compiler);
        graph.track_note(0, note_on(60));
        for _ in 0..20 {
            graph.process_block(&ctx());
        }
        assert!(graph.node_set_param(mixer, ParamId::MixerMute(0), 1.0));
        let levels = graph.process_block(&ctx());
        assert!(
            levels.master.abs() < 1e-9,
            "muted channel should not reach the master output"
        );
        assert!(
            levels.tracks[0] > 0.05,
            "the track's metering point (chain end) sits pre-mixer and still runs"
        );
        assert!(graph.node_set_param(mixer, ParamId::MixerMute(0), 0.0));
        let levels = graph.process_block(&ctx());
        assert!(levels.master > 0.05, "unmuting restores the signal");
    }

    #[test]
    fn channel_pan_hard_left_puts_mono_source_only_in_left() {
        let (compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let mixer = default_mixer_id(&compiler);
        graph.track_note(0, note_on(69));
        assert!(graph.node_set_param(mixer, ParamId::MixerPan(0), -1.0));
        graph.process_block(&ctx());
        let levels = graph.process_block(&ctx());
        assert!(levels.master > 0.05);

        let left = graph.master_channel(0).unwrap();
        let right = graph.master_channel(1).unwrap();
        assert!(
            left.iter().any(|s| s.abs() > 0.05),
            "left channel should carry signal"
        );
        assert!(
            right.iter().all(|s| s.abs() < 1e-9),
            "right channel should be silent at hard-left pan"
        );
    }

    #[test]
    fn master_gain_applies_after_summing() {
        let mut graph = default_graph();
        graph.track_note(0, note_on(69));
        for _ in 0..20 {
            graph.process_block(&ctx());
        }
        let before = graph.process_block(&ctx());
        graph.master_set_param(ParamId::MasterGain, 0.25);
        // Smoothing (D9): the 10 ms ramp has to settle before the ratio means
        // anything. 100 blocks ≈ 27 time constants.
        for _ in 0..100 {
            graph.process_block(&ctx());
        }
        let after = graph.process_block(&ctx());
        let ratio = after.master / before.master;
        assert!(
            (ratio - 0.25).abs() < 0.1,
            "master gain 0.25 should reduce output to about 1/4, ratio = {ratio}"
        );
    }

    /// Records every note-off pitch it receives (no audio) — a stand-in for
    /// a pitch-matching plugin, where a blanket `Off { note: 0 }` is useless.
    struct PitchRecorder {
        offs: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl Processor for PitchRecorder {
        fn process(&mut self, _ctx: &ProcessContext, _buffer: &mut AudioBuffer) {}
        fn note_event(&mut self, event: NoteEvent) -> bool {
            if let NoteEvent::Off { note } = event {
                self.offs.lock().expect("recorder lock").push(note);
            }
            true
        }
    }

    #[test]
    fn panic_sweeps_every_pitch_across_bounded_batches() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, _recycle_rx) = recycle_channel();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (update, _) = expect_swap(
            compiler.set_voice(
                0,
                Box::new(PitchRecorder {
                    offs: std::sync::Arc::clone(&seen),
                }),
            ),
            "swap in the pitch recorder",
        );
        graph.apply_update(update, &mut recycle_tx);

        graph.panic();
        // 128 pitches / 24 per block = 6 blocks; one spare block to confirm
        // the sweep stops instead of running forever.
        for _ in 0..7 {
            graph.process_block(&ctx());
        }

        let mut pitches = seen.lock().expect("recorder lock").clone();
        pitches.sort_unstable();
        pitches.dedup();
        assert_eq!(
            pitches.len(),
            128,
            "panic must release every pitch, got {} distinct",
            pitches.len()
        );
        assert_eq!(pitches.first(), Some(&0));
        assert_eq!(pitches.last(), Some(&127));
    }

    #[test]
    fn master_meter_reports_both_channels() {
        let (compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let mixer = default_mixer_id(&compiler);
        // Track 0 panned hard left: the stereo meter must show L only.
        graph.track_note(0, note_on(69));
        assert!(graph.node_set_param(mixer, ParamId::MixerPan(0), -1.0));
        for _ in 0..20 {
            graph.process_block(&ctx());
        }
        let levels = graph.process_block(&ctx());
        assert!(
            levels.master_lr[0] > 0.01,
            "left channel should carry the signal, got {:?}",
            levels.master_lr
        );
        assert!(
            levels.master_lr[1] < 1e-6,
            "right channel should be silent at hard-left pan, got {:?}",
            levels.master_lr
        );
        assert!(
            (levels.master - levels.master_lr[0]).abs() < 1e-6,
            "the collapsed peak must equal the loudest channel"
        );
    }

    #[test]
    fn note_off_all_releases_every_track() {
        let mut graph = default_graph();
        graph.track_note(0, note_on(60));
        graph.track_note(1, note_on(64));
        for _ in 0..20 {
            graph.process_block(&ctx());
        }
        graph.note_off_all();
        for _ in 0..100 {
            graph.process_block(&ctx());
        }
        let levels = graph.process_block(&ctx());
        assert!(
            levels.master < 0.01,
            "should be silent after all notes released, got {}",
            levels.master
        );
    }

    #[test]
    fn track_params_route_to_chain_nodes() {
        let mut graph = default_graph();
        graph.track_set_param(0, ParamId::FilterCutoff, 500.0);
        graph.track_set_param(0, ParamId::DelayMix, 0.5);
        graph.track_set_param(0, ParamId::DelayTime, 0.1);
        graph.track_set_param(0, ParamId::Attack, 0.0);
        graph.track_note(0, note_on(69));
        let out = graph.process_block(&ctx());
        assert!(out.master.is_finite());
    }

    // ---- graph-specific: multi-in-edge summing ----

    /// DC source (writes a constant), for deterministically verifying the
    /// summing math.
    struct DcSource {
        value: f32,
    }

    impl Processor for DcSource {
        fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
            for ch in 0..buffer.channels() {
                if let Some(channel) = buffer.channel_mut(ch) {
                    channel.fill(self.value);
                }
            }
        }
    }

    #[test]
    fn fan_in_sums_multiple_sources() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        // Swap each track to a DC source of 0.25: master output should sum
        // linearly with in-edge count (Σ 0.25 × 0.8).
        let mut expected = 0.0f32;
        for track in 0..3 {
            let (update, _) = expect_swap(
                compiler.set_voice(track, Box::new(DcSource { value: 0.25 })),
                "swap voice",
            );
            let (mut tx, mut rx) = recycle_channel();
            graph.apply_update(update, &mut tx);
            assert_eq!(
                drain_recycle(&mut rx).len(),
                1,
                "old built-in instrument should be recycled"
            );
            expected += 0.25 * 0.8;
            let levels = graph.process_block(&ctx());
            assert!(
                (levels.master - expected).abs() < 1e-6,
                "after track {} connects, master output should be {}, got {}",
                track + 1,
                expected,
                levels.master
            );
        }
    }

    // ---- structural update protocol ----

    /// Test effect: gain 0.5 (verifies signal halved after insertion).
    struct HalfGain;

    impl Processor for HalfGain {
        fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
            for ch in 0..buffer.channels() {
                if let Some(channel) = buffer.channel_mut(ch) {
                    for sample in channel.iter_mut() {
                        *sample *= 0.5;
                    }
                }
            }
        }
    }

    #[test]
    fn insert_effect_attenuates_then_clear_restores_and_recycles() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        graph.track_note(0, note_on(69));
        for _ in 0..20 {
            graph.process_block(&ctx());
        }
        let before = graph.process_block(&ctx());

        // Insert a 0.5-gain effect (between the delay and the channel strip).
        let (update, effect_id) = expect_swap(
            compiler.insert_effect(0, Box::new(HalfGain)),
            "insert effect",
        );
        graph.apply_update(update, &mut recycle_tx);
        assert!(
            drain_recycle(&mut recycle_rx).is_empty(),
            "insertion should not trigger recycling"
        );
        for _ in 0..3 {
            graph.process_block(&ctx());
        }
        let after = graph.process_block(&ctx());
        let ratio = after.master / before.master;
        assert!(
            (ratio - 0.5).abs() < 0.05,
            "the effect should halve the output, ratio = {ratio}"
        );

        // Clear effects → node + node_id return via the recycle queue.
        let update = compiler.clear_effects(0).expect("clear effects");
        graph.apply_update(update, &mut recycle_tx);
        assert_eq!(
            drain_recycle(&mut recycle_rx),
            vec![effect_id],
            "removed effect node should be recycled with its node_id"
        );
        for _ in 0..3 {
            graph.process_block(&ctx());
        }
        let restored = graph.process_block(&ctx());
        let ratio = restored.master / before.master;
        assert!(
            (ratio - 1.0).abs() < 0.05,
            "output should be restored after clearing, ratio = {ratio}"
        );
    }

    #[test]
    fn set_voice_recycles_old_processor() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        let (update, _id) =
            expect_swap(compiler.set_voice(0, Box::new(HalfGain)), "swap instrument");
        graph.apply_update(update, &mut recycle_tx);
        let recycled = drain_recycle(&mut recycle_rx);
        assert_eq!(
            recycled.len(),
            1,
            "old built-in instrument should be recycled"
        );
        assert_ne!(recycled[0], 0);
        // After the swap the track should still work (HalfGain has no notes
        // → silent).
        let levels = graph.process_block(&ctx());
        assert!(levels.tracks[0].abs() < 1e-9);
    }

    #[test]
    fn removed_voice_slot_stays_silent() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, _recycle_rx) = recycle_channel();
        let (update, _id) =
            expect_swap(compiler.set_voice(1, Box::new(HalfGain)), "swap instrument");
        graph.apply_update(update, &mut recycle_tx);
        graph.track_note(1, note_on(60));
        let levels = graph.process_block(&ctx());
        assert!(
            levels.tracks[1].abs() < 1e-9,
            "external source without notes should be silent"
        );
    }

    // ---- compiler validation ----

    #[test]
    fn compile_rejects_cycle() {
        let (mut compiler, _graph) = GraphCompiler::default_graph(SR, BLOCK);
        // Artificially create a cycle: master output → track 0 instrument
        // (private fields, reachable only in tests).
        compiler
            .add_cable(compiler.master_slot, compiler.track_voice[0], 0)
            .expect("should be able to add a cable within capacity");
        assert!(matches!(compiler.compile(), Err(TopologyError::Cycle)));
    }

    // ---- P6 feedback tap ----

    /// Builds `DC → mixer[0]`, `feedback → mixer[1]`, `mixer → feedback`
    /// (the tap) and `mixer → master`; returns the compiler and a running
    /// graph. The mixer output therefore satisfies `y[n] = 1 + gain·y[n-1]`.
    fn feedback_rig(gain: f32) -> (GraphCompiler, RtGraph) {
        let (mut compiler, mut graph) = GraphCompiler::empty_graph(SR, BLOCK);
        let (mut recycle_tx, _recycle_rx) = recycle_channel();
        let mut feedback = super::FeedbackProcessor::new();
        assert!(feedback.set_param(ParamId::FeedbackGain, gain));
        for update in [
            compiler
                .add_node(10, NodeKind::Plain, Box::new(DcSource { value: 1.0 }))
                .expect("source node"),
            compiler
                .add_node(
                    11,
                    NodeKind::Mixer,
                    Box::new(MixerProcessor::with_channels(
                        &[
                            crate::realtime::mixer::MixerChannel {
                                gain: 1.0,
                                ..Default::default()
                            },
                            crate::realtime::mixer::MixerChannel {
                                gain: 1.0,
                                ..Default::default()
                            },
                        ],
                        0.0,
                        false,
                    )),
                )
                .expect("mixer node"),
            compiler
                .add_node(12, NodeKind::Feedback, Box::new(feedback))
                .expect("feedback node"),
            compiler.connect_nodes(10, 11, 0).expect("source → mixer"),
            compiler.connect_nodes(12, 11, 1).expect("feedback → mixer"),
            // The tap: mixer output feeds the feedback node one block late.
            compiler.connect_nodes(11, 12, 0).expect("mixer → feedback"),
            compiler
                .connect_nodes(11, super::MASTER_NODE_ID, 0)
                .expect("mixer → master"),
        ] {
            graph.apply_update(update, &mut recycle_tx);
        }
        (compiler, graph)
    }

    #[test]
    fn feedback_tap_is_exempt_from_cycle_rejection() {
        let (compiler, _graph) = feedback_rig(0.5);
        let plan = compiler.compile().expect("feedback loop must compile");
        assert_eq!(plan.order_len, 4, "all four nodes must be ordered");
        // The feedback node is ordered before its tap source (the mixer),
        // which is what keeps the read one block stale.
        let feedback_slot = compiler.node_slot(12).expect("feedback slot");
        let mixer_slot = compiler.node_slot(11).expect("mixer slot");
        let pos = |slot: u16| plan.order[..plan.order_len].iter().position(|&s| s == slot);
        assert!(pos(feedback_slot) < pos(mixer_slot));
    }

    #[test]
    fn feedback_tap_delays_one_block_and_applies_gain() {
        let (_compiler, mut graph) = feedback_rig(0.5);
        // y[n] = 1 + 0.5·y[n-1], y[-1] = 0.
        let expected = [1.0f32, 1.5, 1.75, 1.875];
        for (block, want) in expected.iter().enumerate() {
            let levels = graph.process_block(&ctx());
            assert!(
                (levels.master - want).abs() < 1e-6,
                "block {block}: expected {want}, got {}",
                levels.master
            );
        }
    }

    #[test]
    fn feedback_tap_without_a_cycle_still_delays() {
        // X → F with no return path: the node still reads X one block late
        // (the ordering reversal is unconditional), and F drives master.
        let (mut compiler, mut graph) = GraphCompiler::empty_graph(SR, BLOCK);
        let (mut recycle_tx, _recycle_rx) = recycle_channel();
        let mut feedback = super::FeedbackProcessor::new();
        assert!(feedback.set_param(ParamId::FeedbackGain, 1.0));
        for update in [
            compiler
                .add_node(10, NodeKind::Plain, Box::new(DcSource { value: 1.0 }))
                .expect("source node"),
            compiler
                .add_node(11, NodeKind::Feedback, Box::new(feedback))
                .expect("feedback node"),
            compiler
                .connect_nodes(10, 11, 0)
                .expect("source → feedback"),
            compiler
                .connect_nodes(11, super::MASTER_NODE_ID, 0)
                .expect("feedback → master"),
        ] {
            graph.apply_update(update, &mut recycle_tx);
        }
        assert_eq!(
            graph.process_block(&ctx()).master,
            0.0,
            "first block is stale"
        );
        assert!((graph.process_block(&ctx()).master - 1.0).abs() < 1e-6);
    }

    #[test]
    fn feedback_self_loop_is_rejected() {
        let (mut compiler, _graph) = GraphCompiler::empty_graph(SR, BLOCK);
        compiler
            .add_node(
                10,
                NodeKind::Feedback,
                Box::new(super::FeedbackProcessor::new()),
            )
            .expect("feedback node");
        let slot = compiler.node_slot(10).expect("slot");
        compiler.add_cable(slot, slot, 0).expect("self cable");
        assert!(matches!(compiler.compile(), Err(TopologyError::Cycle)));
    }

    #[test]
    fn removing_a_feedback_node_drops_its_cables() {
        let (mut compiler, _graph) = feedback_rig(0.5);
        let (mut recycle_tx, _recycle_rx) = recycle_channel();
        let update = compiler.remove_node(12).expect("feedback node removal");
        // No bypass rewiring: both cables touching the node are gone and the
        // loop is broken (a bypass would have recreated the cycle).
        assert!(
            compiler.cables[..compiler.cable_count]
                .iter()
                .all(|c| c.to != 12 || c.from != 12)
        );
        assert!(compiler.compile().is_ok());
        let _ = update;
        let _ = &mut recycle_tx;
    }

    #[test]
    fn add_cable_replaces_occupied_input_slot() {
        let (mut compiler, _graph) = GraphCompiler::default_graph(SR, BLOCK);
        // Strict single-input (D14): a new cable onto an occupied input slot
        // disconnects the old one instead of failing at compile time.
        let delay = compiler.track_delay[0];
        compiler
            .add_cable(compiler.track_voice[1], delay, 0)
            .expect("add cable");
        assert!(compiler.compile().is_ok());
        // The slot carries exactly one in-edge now (from track 1's voice).
        let in_edges: Vec<_> = compiler.cables[..compiler.cable_count]
            .iter()
            .filter(|c| c.to == delay)
            .collect();
        assert_eq!(in_edges.len(), 1);
        assert_eq!(in_edges[0].from, compiler.track_voice[1]);
        // Exact duplicates are idempotent (no second edge).
        compiler
            .add_cable(compiler.track_voice[1], delay, 0)
            .expect("idempotent");
        assert_eq!(
            compiler.cables[..compiler.cable_count]
                .iter()
                .filter(|c| c.to == delay)
                .count(),
            1
        );
    }

    #[test]
    fn compile_accepts_multi_input_mixer_and_single_input_master() {
        let (compiler, _graph) = GraphCompiler::default_graph(SR, BLOCK);
        // D18 default graph: the mixer carries one in-edge per track (4) and
        // the master exactly one (mixer → master) — both must compile.
        assert!(compiler.compile().is_ok());
        let mixer_in_count = compiler.cables[..compiler.cable_count]
            .iter()
            .filter(|c| compiler.kinds[c.to as usize] == NodeKind::Mixer)
            .count();
        assert_eq!(mixer_in_count, DEFAULT_MIDI_TRACKS);
    }

    #[test]
    fn default_plan_order_is_topological() {
        let (_compiler, graph) = GraphCompiler::default_graph(SR, BLOCK);
        let plan = graph.plan;
        // Every node must come after its in-edge sources.
        for i in 0..plan.order_len {
            let s = plan.order[i] as usize;
            for &src in &plan.in_edges[s][..plan.in_counts[s] as usize] {
                let pos = plan.order[..plan.order_len]
                    .iter()
                    .position(|&o| o == src)
                    .expect("in-edge source must be in the topological order");
                assert!(
                    pos < i,
                    "topological order violated: slot {s} precedes its source {src}"
                );
            }
        }
    }

    #[test]
    fn empty_graph_outputs_silence() {
        let mut graph = RtGraph::new(BLOCK);
        let levels = graph.process_block(&ctx());
        assert!(levels.master.abs() < 1e-9);
        assert!(graph.master_channel(0).is_none());
    }

    // ---- ChannelStripProcessor unit ----

    #[test]
    fn channel_strip_applies_gain_pan_and_mute() {
        let fill = |buf: &mut AudioBuffer| {
            buf.channel_mut(0).unwrap().fill(0.5);
            buf.channel_mut(1).unwrap().fill(0.5);
        };
        let mut buf = AudioBuffer::new(2, 8);
        buf.set_len(8);

        // Default: gain 0.8, pan centered (pan gain 1) → both channels × 0.8.
        let mut strip = ChannelStripProcessor::new();
        fill(&mut buf);
        strip.process(&ctx(), &mut buf);
        assert!(
            buf.channel(0)
                .unwrap()
                .iter()
                .all(|s| (*s - 0.4).abs() < 1e-6)
        );
        assert!(
            buf.channel(1)
                .unwrap()
                .iter()
                .all(|s| (*s - 0.4).abs() < 1e-6)
        );

        // Hard left: right channel zeroed, left channel still 0.5 × 0.8.
        // A fresh strip with the pan set *before* the first render (the
        // smoother snaps pre-start; a live pan move ramps by design).
        let mut strip = ChannelStripProcessor::new();
        strip.set_param(ParamId::TrackPan, -1.0);
        fill(&mut buf);
        strip.process(&ctx(), &mut buf);
        assert!(
            buf.channel(0)
                .unwrap()
                .iter()
                .all(|s| (*s - 0.4).abs() < 1e-6)
        );
        assert!(buf.channel(1).unwrap().iter().all(|s| s.abs() < 1e-9));

        // Muted: all zeros.
        let mut strip = ChannelStripProcessor::new();
        strip.set_param(ParamId::TrackMute, 1.0);
        fill(&mut buf);
        strip.process(&ctx(), &mut buf);
        assert!(buf.channel(0).unwrap().iter().all(|s| s.abs() < 1e-12));
        assert!(buf.channel(1).unwrap().iter().all(|s| s.abs() < 1e-12));

        // A runtime fader move must ramp, not jump (D8).
        let mut strip = ChannelStripProcessor::new();
        fill(&mut buf);
        strip.process(&ctx(), &mut buf);
        strip.set_param(ParamId::TrackGain, 0.0);
        fill(&mut buf);
        strip.process(&ctx(), &mut buf);
        let first = buf.channel(0).unwrap()[0];
        assert!(
            first > 0.3 && first < 0.4,
            "a runtime gain move must ramp, got {first}"
        );
    }

    // ---- smoke tests stressing capacity limits ----

    #[test]
    fn effect_slots_per_track_capped() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        // 4 slots per track + capacity headroom: 8 consecutive insertions
        // should all succeed (default graph uses 13/64 slots).
        for _ in 0..8 {
            let (update, _) = expect_swap(
                compiler.insert_effect(0, Box::new(HalfGain)),
                "insertion should succeed with slots available",
            );
            graph.apply_update(update, &mut recycle_tx);
        }
        assert!(drain_recycle(&mut recycle_rx).is_empty());
        // 64 - 13 = 51 free slots: continuing to fill should eventually fail
        // and hand the node back.
        while let Ok((update, _)) = compiler.insert_effect(1, Box::new(HalfGain)) {
            graph.apply_update(update, &mut recycle_tx);
        }
        // The graph should still process normally.
        let levels = graph.process_block(&ctx());
        assert!(levels.master.is_finite());
    }

    // ---- timeline routing tables (P3, D7) ----

    #[test]
    fn midi_fanout_delivers_to_all_connected_targets() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        // Fan MIDI track 0 out to track 1's instrument (node_id addressing):
        // one note sounds on two tracks.
        let target_id = compiler
            .track_voice_node_id(1)
            .expect("track 1 instrument should exist");
        let update = compiler
            .connect_midi(0, target_id)
            .expect("fan-out cable should succeed");
        graph.apply_update(update, &mut recycle_tx);
        assert!(drain_recycle(&mut recycle_rx).is_empty());
        assert_eq!(graph.plan.midi_target_counts[0], 2);

        graph.track_note(0, note_on(69));
        let levels = graph.process_block(&ctx());
        assert!(
            levels.tracks[0] > 0.05,
            "own track's instrument should sound"
        );
        assert!(
            levels.tracks[1] > 0.05,
            "fan-out target instrument should also sound"
        );

        // After disconnecting, the routing table returns to a single target.
        let update = compiler
            .disconnect_midi(0, target_id)
            .expect("disconnect should succeed");
        graph.apply_update(update, &mut recycle_tx);
        assert_eq!(graph.plan.midi_target_counts[0], 1);
        assert_eq!(graph.plan.midi_targets[0][0], compiler.track_voice[0]);
    }

    #[test]
    fn midi_fanout_is_capped_and_validated() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, _recycle_rx) = recycle_channel();
        // 1 by default per track; adding 3 more reaches the cap of 4.
        for t in 1..4usize {
            let id = compiler
                .track_voice_node_id(t)
                .expect("instrument should exist");
            let update = compiler
                .connect_midi(0, id)
                .expect("should succeed within the cap");
            graph.apply_update(update, &mut recycle_tx);
        }
        assert_eq!(graph.plan.midi_target_counts[0], MAX_MIDI_FANOUT as u8);
        // The 5th is rejected: track 0 is at the cap, any valid target should
        // trigger fan-out overflow.
        let ch0 = compiler
            .track_channel_node_id(0)
            .expect("channel strip should exist");
        assert!(matches!(
            compiler.connect_midi(0, ch0),
            Err(TopologyError::MidiFanOutOverflow)
        ));
        // Unassigned node_id rejected.
        assert!(matches!(
            compiler.connect_midi(0, u64::MAX),
            Err(TopologyError::InvalidSlot)
        ));
        assert!(matches!(
            compiler.connect_midi(0, 0),
            Err(TopologyError::InvalidSlot)
        ));
        // Out-of-range track rejected.
        assert!(matches!(
            compiler.connect_midi(MAX_MIDI_TRACKS, ch0),
            Err(TopologyError::InvalidTrack)
        ));
        // Duplicate connection idempotent.
        let id1 = compiler
            .track_voice_node_id(1)
            .expect("instrument should exist");
        let update = compiler
            .connect_midi(0, id1)
            .expect("duplicate connection idempotent");
        graph.apply_update(update, &mut recycle_tx);
        assert_eq!(graph.plan.midi_target_counts[0], MAX_MIDI_FANOUT as u8);
    }

    #[test]
    fn auto_route_delivers_curve_values_to_target_param() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        // Swap track 0's instrument to a DC source of 0.25; automation track
        // 0 connects to the global mixer's channel 0 gain (node_id addressing;
        // P5f D18: track 0's chain feeds mixer channel 0 by default).
        let (update, _) = expect_swap(
            compiler.set_voice(0, Box::new(DcSource { value: 0.25 })),
            "swap voice",
        );
        graph.apply_update(update, &mut recycle_tx);
        assert_eq!(
            drain_recycle(&mut recycle_rx).len(),
            1,
            "old instrument should be recycled"
        );
        let mixer_id = default_mixer_id(&compiler);
        let update = compiler
            .set_auto_route(0, mixer_id, ParamId::MixerGain(0))
            .expect("automation cable should succeed");
        graph.apply_update(update, &mut recycle_tx);

        // Curve value 0.5 → channel strip gain 0.5 → master output = 0.25 × 0.5.
        graph.auto_deliver(0, 0.5);
        let levels = graph.process_block(&ctx());
        assert!(
            (levels.master - 0.125).abs() < 1e-6,
            "automation should drive the channel strip gain, got {}",
            levels.master
        );

        // Unconnected automation tracks are silent.
        graph.auto_deliver(1, 1.5);
        let levels = graph.process_block(&ctx());
        assert!((levels.master - 0.125).abs() < 1e-6);

        // After disconnecting, delivery stops and the gain keeps its last value.
        let update = compiler
            .clear_auto_route(0)
            .expect("disconnect should succeed");
        graph.apply_update(update, &mut recycle_tx);
        graph.auto_deliver(0, 1.5);
        let levels = graph.process_block(&ctx());
        assert!(
            (levels.master - 0.125).abs() < 1e-6,
            "parameter should not change after disconnect"
        );

        // Unassigned node_id rejected for connection (original cable untouched).
        assert!(matches!(
            compiler.set_auto_route(1, u64::MAX, ParamId::TrackGain),
            Err(TopologyError::InvalidSlot)
        ));
    }

    /// U4a: records the control changes an automation route emits.
    struct CcRecorder {
        seen: std::sync::Arc<std::sync::Mutex<Vec<(u8, u8, u8)>>>,
    }

    impl Processor for CcRecorder {
        fn process(&mut self, _ctx: &ProcessContext, _buffer: &mut AudioBuffer) {}
        fn midi_cc(&mut self, channel: u8, controller: u8, value: u8) -> bool {
            self.seen
                .lock()
                .expect("recorder lock")
                .push((channel, controller, value));
            true
        }
    }

    #[test]
    fn auto_cc_route_quantizes_and_dedupes() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, _recycle_rx) = recycle_channel();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (update, _) = expect_swap(
            compiler.set_voice(
                0,
                Box::new(CcRecorder {
                    seen: std::sync::Arc::clone(&seen),
                }),
            ),
            "swap in the CC recorder",
        );
        graph.apply_update(update, &mut recycle_tx);
        let voice_id = compiler
            .track_voice_node_id(0)
            .expect("instrument should exist");
        let update = compiler
            .set_auto_cc_route(0, voice_id, 3, 74)
            .expect("CC route should succeed");
        graph.apply_update(update, &mut recycle_tx);

        // The curve stores real CC units (spec D7): 50.8 rounds to 51 rather
        // than truncating to 50.
        graph.auto_deliver(0, 50.8);
        // Same quantized value: a CC is absolute, so it must not be resent.
        graph.auto_deliver(0, 50.9);
        // Out-of-range curve values clamp into 7 bits, and the second one is
        // still a duplicate of the clamped result.
        graph.auto_deliver(0, 900.0);
        graph.auto_deliver(0, 127.0);
        // A real move gets through.
        graph.auto_deliver(0, 12.0);

        let events = seen.lock().expect("recorder lock").clone();
        assert_eq!(
            events,
            vec![(3, 74, 51), (3, 74, 127), (3, 74, 12)],
            "quantized, change-only, channel-carrying CC stream"
        );
    }

    #[test]
    fn auto_cc_route_requires_a_live_node() {
        let (mut compiler, _graph) = GraphCompiler::default_graph(SR, BLOCK);
        assert!(matches!(
            compiler.set_auto_cc_route(0, u64::MAX, 0, 74),
            Err(TopologyError::InvalidSlot)
        ));
        assert!(matches!(
            compiler.set_auto_cc_route(MAX_AUTO_TRACKS, 1, 0, 74),
            Err(TopologyError::InvalidIndex)
        ));
    }

    // ---- P5b: node_id addressing / dynamic track count / multi-out fan-out ----

    #[test]
    fn node_id_param_via_master() {
        let (compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let master_id = compiler.master_node_id();
        assert_ne!(
            master_id, 0,
            "master output node_id should be allocated at construction"
        );
        graph.track_note(0, note_on(69));
        for _ in 0..10 {
            graph.process_block(&ctx());
        }
        let before = graph.process_block(&ctx());
        assert!(graph.node_set_param(master_id, ParamId::MasterGain, 0.25));
        // Let the 10 ms smoother settle (D9) before comparing levels.
        for _ in 0..100 {
            graph.process_block(&ctx());
        }
        let after = graph.process_block(&ctx());
        let ratio = after.master / before.master;
        assert!(
            (ratio - 0.25).abs() < 0.1,
            "node_id-addressed master gain should take effect, ratio = {ratio}"
        );
    }

    #[test]
    fn add_and_remove_midi_track_roundtrip() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        assert_eq!(compiler.midi_track_count(), DEFAULT_MIDI_TRACKS);

        // Adding a track: returns two new node_ids; the new track can sound.
        let (update, [voice_id, delay_id]) =
            compiler.add_midi_track().expect("add track should succeed");
        graph.apply_update(update, &mut recycle_tx);
        assert!(
            drain_recycle(&mut recycle_rx).is_empty(),
            "no recycling when adding a track"
        );
        assert_eq!(compiler.midi_track_count(), DEFAULT_MIDI_TRACKS + 1);
        assert_ne!(voice_id, 0);
        assert_ne!(delay_id, 0);
        assert_eq!(
            compiler.track_voice_node_id(DEFAULT_MIDI_TRACKS),
            Some(voice_id),
            "new track's routing table should point to the new instrument"
        );
        graph.track_note(DEFAULT_MIDI_TRACKS, note_on(69));
        let levels = graph.process_block(&ctx());
        assert!(
            levels.tracks[DEFAULT_MIDI_TRACKS] > 0.05,
            "new track should be able to sound"
        );

        // Removing a track: the pair is recycled and the routing table cleared.
        let update = compiler
            .remove_midi_track(DEFAULT_MIDI_TRACKS)
            .expect("remove track should succeed");
        graph.apply_update(update, &mut recycle_tx);
        assert_eq!(
            drain_recycle(&mut recycle_rx).len(),
            2,
            "the whole pair should be recycled"
        );
        assert_eq!(compiler.midi_track_count(), DEFAULT_MIDI_TRACKS);
        assert_eq!(compiler.track_voice_node_id(DEFAULT_MIDI_TRACKS), None);
        graph.track_note(DEFAULT_MIDI_TRACKS, note_on(69));
        let levels = graph.process_block(&ctx());
        assert!(levels.tracks[DEFAULT_MIDI_TRACKS].abs() < 1e-12);
        assert!(
            levels.master.is_finite(),
            "graph should still process normally after track removal"
        );
    }

    #[test]
    fn add_track_fills_capacity_then_fails() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, _recycle_rx) = recycle_channel();
        // P5f D18: adding tracks no longer touches the master (mixing happens
        // on mixer nodes) — tracks can fill the whole 16-track capacity.
        while compiler.midi_track_count() < MAX_MIDI_TRACKS {
            let (update, _) = compiler
                .add_midi_track()
                .expect("should succeed within the track capacity");
            graph.apply_update(update, &mut recycle_tx);
        }
        assert_eq!(compiler.midi_track_count(), MAX_MIDI_TRACKS);
        // The 17th track → NoFreeTrack.
        assert!(matches!(
            compiler.add_midi_track(),
            Err(TopologyError::NoFreeTrack)
        ));
        // The graph still processes normally.
        graph.track_note(0, note_on(69));
        let levels = graph.process_block(&ctx());
        assert!(levels.master > 0.05);
    }

    #[test]
    fn multi_out_fanout_sums_into_two_sinks() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, _recycle_rx) = recycle_channel();
        // Multi-out edges: track 0's instrument additionally connects to the
        // global mixer on channel 1 (two cables from one output port; only
        // the mixer accepts a second in-edge — D14).
        let voice0 = compiler
            .track_voice_node_id(0)
            .expect("instrument should exist");
        let slot0 = compiler.node_slot(voice0).expect("slot should exist");
        let mixer_slot = compiler
            .kinds
            .iter()
            .position(|&k| k == NodeKind::Mixer)
            .expect("mixer slot") as u16;
        compiler
            .add_cable(slot0, mixer_slot, 1)
            .expect("second out-cable should succeed");
        compiler.compile().expect("topology should remain acyclic");

        // DC source 0.25 → mixer ch0 (via delay) and ch1 (direct), each
        // channel × 0.8 → master output 0.25 × 0.8 × 2.
        let (update, _) = expect_swap(
            compiler.set_voice(0, Box::new(DcSource { value: 0.25 })),
            "swap voice",
        );
        graph.apply_update(update, &mut recycle_tx);
        let levels = graph.process_block(&ctx());
        assert!(
            (levels.master - 0.25 * 0.8 * 2.0).abs() < 1e-6,
            "multi-out fan-out should send the signal into both channel strips, got {}",
            levels.master
        );
    }

    #[test]
    fn cable_capacity_is_capped() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        // Fill the cable list (the compiler does not validate endpoint
        // occupancy, focusing on capacity semantics).
        let mut i = 0u16;
        while compiler.cable_count < MAX_CABLES {
            compiler
                .add_cable(60, 100 + i, 0)
                .expect("should succeed within capacity");
            i += 1;
        }
        assert_eq!(compiler.cable_count, MAX_CABLES);
        // Adding one more unique cable when full → TooManyCables (target 999
        // is beyond the filler range 100..346).
        assert!(matches!(
            compiler.add_cable(60, 999, 0),
            Err(TopologyError::TooManyCables)
        ));
        // Duplicate connections remain idempotent (dedup takes precedence
        // over the capacity check).
        assert!(compiler.add_cable(60, 100, 0).is_ok());
        let levels = graph.process_block(&ctx());
        assert!(levels.master.is_finite());
    }

    // ---- P5c: incremental document-based graph building API ----

    #[test]
    fn document_rebuild_end_to_end() {
        // D13: master-only empty graph → rebuilt from the document via
        // add_node / register / connect.
        let (mut compiler, mut graph) = GraphCompiler::empty_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        let apply = |graph: &mut RtGraph, recycle_tx: &mut rtrb::Producer<RecycledNode>, update| {
            graph.apply_update(update, recycle_tx);
        };

        let master = compiler.master_node_id();
        // 2 = instrument, 3 = delay, 4 = global mixer (D18).
        let update = compiler
            .add_node(2, NodeKind::Plain, chain_processor(0, SR))
            .expect("document node should install");
        apply(&mut graph, &mut recycle_tx, update);
        let update = compiler
            .add_node(3, NodeKind::Plain, chain_processor(1, SR))
            .expect("document node should install");
        apply(&mut graph, &mut recycle_tx, update);
        let update = compiler
            .add_node(4, NodeKind::Mixer, Box::new(MixerProcessor::new(2)))
            .expect("document node should install");
        apply(&mut graph, &mut recycle_tx, update);
        assert!(drain_recycle(&mut recycle_rx).is_empty());

        // Track addressing tables + audio cables + MIDI self-connection (all
        // node_id addressed). The metering point is the chain end (delay).
        apply(
            &mut graph,
            &mut recycle_tx,
            compiler
                .register_track_chain(0, Some(2), Some(3), Some(3))
                .expect("assemble track tables"),
        );
        apply(
            &mut graph,
            &mut recycle_tx,
            compiler.connect_nodes(2, 3, 0).expect("document cable"),
        );
        apply(
            &mut graph,
            &mut recycle_tx,
            compiler.connect_nodes(3, 4, 0).expect("document cable"),
        );
        apply(
            &mut graph,
            &mut recycle_tx,
            compiler
                .connect_nodes(4, master, 0)
                .expect("document cable"),
        );
        apply(
            &mut graph,
            &mut recycle_tx,
            compiler.connect_midi(0, 2).expect("MIDI self-connection"),
        );

        // End to end: track 0 note → instrument → delay → mixer ch0 →
        // master output.
        assert!(graph.track_note(0, note_on(69)));
        let levels = graph.process_block(&ctx());
        assert!(
            levels.master > 0.05,
            "the rebuilt-from-document graph should sound"
        );
        assert!(levels.tracks[0] > 0.05);
    }

    #[test]
    fn add_node_rejects_reserved_and_duplicate_ids() {
        let (mut compiler, _graph) = GraphCompiler::empty_graph(SR, BLOCK);
        let (_recycle_tx, mut recycle_rx) = recycle_channel();
        // 0 = invalid sentinel; 1 = master output reserved (D6).
        assert!(
            compiler
                .add_node(0, NodeKind::Plain, chain_processor(2, SR))
                .is_err()
        );
        assert!(
            compiler
                .add_node(1, NodeKind::Plain, chain_processor(2, SR))
                .is_err()
        );
        assert!(drain_recycle(&mut recycle_rx).is_empty());

        let update = compiler
            .add_node(5, NodeKind::Plain, chain_processor(2, SR))
            .expect("first install should succeed");
        let _ = update;
        assert!(
            compiler
                .add_node(5, NodeKind::Plain, chain_processor(2, SR))
                .is_err(),
            "duplicate node_id should be rejected"
        );
        // Even a failed add_node advanced next_node_id: fresh allocations
        // will not collide with 5.
        assert!(compiler.fresh_id() > 5);
    }

    #[test]
    fn replace_node_keeps_identity_and_recycles_old() {
        let (mut compiler, mut graph) = GraphCompiler::empty_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        let master = compiler.master_node_id();
        let update = compiler
            .add_node(2, NodeKind::Plain, chain_processor(0, SR))
            .expect("install");
        graph.apply_update(update, &mut recycle_tx);
        // Channel strip + cables (the DC source goes straight into the master
        // output for easy assertions).
        let update = compiler
            .add_node(3, NodeKind::Plain, chain_processor(2, SR))
            .expect("install");
        graph.apply_update(update, &mut recycle_tx);
        let update = compiler.connect_nodes(2, 3, 0).expect("cable");
        graph.apply_update(update, &mut recycle_tx);
        let update = compiler.connect_nodes(3, master, 0).expect("cable");
        graph.apply_update(update, &mut recycle_tx);

        // In-place replacement with a DC source: node_id 2 stays unchanged.
        let (update, kept) = expect_swap(
            compiler.replace_node(2, Box::new(DcSource { value: 0.25 })),
            "in-place replacement",
        );
        graph.apply_update(update, &mut recycle_tx);
        assert_eq!(kept, 2);
        // The old processor (built-in synthesizer) dequeues for recycling
        // under the same node_id.
        assert_eq!(drain_recycle(&mut recycle_rx), vec![2]);
        let levels = graph.process_block(&ctx());
        assert!(
            (levels.master - 0.25 * 0.8).abs() < 1e-6,
            "after replacement the DC source should pass through (channel strip default gain 0.8)"
        );

        // node_id-addressed parameters still work after replacement.
        assert!(graph.node_set_param(3, ParamId::TrackGain, 0.5));
        // The channel strip gain is smoothed (D8): settle the 10 ms ramp.
        for _ in 0..100 {
            graph.process_block(&ctx());
        }
        let levels = graph.process_block(&ctx());
        assert!(
            (levels.master - 0.125).abs() < 1e-6,
            "expected 0.125 after settling, got {}",
            levels.master
        );
    }

    #[test]
    fn insert_and_remove_node_splices_cables() {
        let (mut compiler, mut graph) = GraphCompiler::empty_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        let master = compiler.master_node_id();
        for (id, kind) in [(2u64, 0usize), (3, 1), (4, 2)] {
            let update = compiler
                .add_node(id, NodeKind::Plain, chain_processor(kind, SR))
                .expect("install");
            graph.apply_update(update, &mut recycle_tx);
        }
        for (from, to) in [(2, 3), (3, 4), (4, master)] {
            let update = compiler.connect_nodes(from, to, 0).expect("cable");
            graph.apply_update(update, &mut recycle_tx);
        }

        // Insert an external effect node (5) between the instrument (2) and
        // the delay (3).
        let update = expect_update(
            compiler.insert_node_between(5, 2, 3, Box::new(DcSource { value: 0.0 })),
            "insert effect",
        );
        graph.apply_update(update, &mut recycle_tx);
        assert!(
            compiler.compile().is_ok(),
            "topology should be valid after insertion"
        );
        // Topology: 2 → 5 → 3 → 4 → master (has_cable takes slots; resolve first).
        let s2 = compiler.node_slot(2).expect("instrument slot");
        let s3 = compiler.node_slot(3).expect("delay slot");
        let s5 = compiler.node_slot(5).expect("effect slot");
        assert!(compiler.has_cable(s2, s5) && compiler.has_cable(s5, s3));
        assert!(!compiler.has_cable(s2, s3));

        // Removing node 5: cables restored to 2 → 3, recycling dequeues.
        let update = compiler.remove_node(5).expect("remove");
        graph.apply_update(update, &mut recycle_tx);
        assert_eq!(drain_recycle(&mut recycle_rx), vec![5]);
        assert!(compiler.has_cable(s2, s3));
        assert!(!compiler.has_cable(s2, s5));

        // Removing the master output is rejected (D6).
        assert!(compiler.remove_node(master).is_err());

        // Automation routing table entries are cleaned up with the removed node.
        let update = compiler
            .set_auto_route(0, 2, ParamId::TrackGain)
            .expect("cable");
        graph.apply_update(update, &mut recycle_tx);
        let update = compiler.remove_node(2).expect("remove instrument");
        graph.apply_update(update, &mut recycle_tx);
        assert_eq!(
            compiler.compile().expect("plan").auto_routes[0].slot,
            NO_EDGE,
            "automation cables pointing to the removed node should be cleaned up"
        );
    }
}
