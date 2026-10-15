//! Structure update queue and node recycling receipts.
//!
//! Graph topology changes are dispatched via [`structure_channel`] (rtrb,
//! non-`Copy`): the control side compiles a
//! [`GraphUpdate`](crate::realtime::graph::GraphUpdate) (containing the new
//! execution plan and the nodes to install), and the audio thread swaps it in
//! wholesale at a block boundary. CLAP/VST3 plugins can only be instantiated
//! on a non-real-time thread, so the install payload is fully constructed on
//! the control side.
//!
//! **Removed nodes are sent back to the control thread for release via the
//! [`RecycledNode`] receipt queue** — the real-time thread never drops heap
//! objects; the receipt carries the node ID (paired with installation) and
//! the dedicated output buffer (the buffer likewise must not be released on
//! the real-time thread).

use rtrb::{Consumer, Producer, RingBuffer};

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::processor::Processor;

/// A node that has been uninstalled and sent back from the audio thread to
/// the control thread. The control thread drops it (including the
/// corresponding plugin guard and output buffer); the real-time thread never
/// releases heap resources.
pub struct RecycledNode {
    /// Paired with `NodeDeployment::node_id` at installation time.
    pub node_id: u64,
    pub node: Box<dyn Processor>,
    /// The node's dedicated output buffer, returned to the control thread
    /// for release along with the node.
    pub output: AudioBuffer,
}

/// Default capacity of the structure update queue.
///
/// A low-frequency operation, but project loading (P5c, D13 fully explicit)
/// dispatches the whole graph's incremental building updates at once:
/// worst case 64 nodes + 128 connections ≈ 192 entries, so 256 leaves ample
/// headroom.
pub const STRUCTURE_QUEUE_CAPACITY: usize = 256;

/// Default capacity of the node recycling receipt queue.
pub const RECYCLE_QUEUE_CAPACITY: usize = 16;

/// Endpoints of the structure update queue. **Not RT-safe**; constructed
/// only before the stream starts.
pub fn structure_channel() -> (
    Producer<crate::realtime::graph::GraphUpdate>,
    Consumer<crate::realtime::graph::GraphUpdate>,
) {
    RingBuffer::new(STRUCTURE_QUEUE_CAPACITY)
}

/// Endpoints of the node recycling receipt queue: audio thread → control
/// thread. **Not RT-safe**.
pub fn recycle_channel() -> (Producer<RecycledNode>, Consumer<RecycledNode>) {
    RingBuffer::new(RECYCLE_QUEUE_CAPACITY)
}
