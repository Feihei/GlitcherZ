//! Processing graph node trait and serial chain.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;

/// Max channel count a per-input-weighted node (the mixer) can address
/// (D15: mixer inputs 2..=32; equals the graph's `MAX_IN_EDGES`).
pub const MAX_WEIGHT_CHANNELS: usize = 32;

/// Per-input-channel effective L/R summing weights (D20/D21): index =
/// in-edge channel (`CableDoc.in_port`), value = `[left, right]` multiplier
/// applied while the graph sums that edge into the node's buffer.
pub type ChannelWeights = [[f32; 2]; MAX_WEIGHT_CHANNELS];

/// Processing graph node.
///
/// `process` runs on the audio callback thread; implementations must follow
/// the real-time iron rules (see the [`crate::realtime`] module docs): no
/// allocation, no locks, no system calls, no panics.
///
/// Semantics: the processor reads and writes the active region of `buffer`
/// **in place** — generators write it, effects modify it in place.
pub trait Processor: Send {
    /// Processes one fixed block.
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer);

    /// Per-input-channel summing weights (D20): `Some` = the graph applies
    /// these L/R weights while summing each in-edge into this node's buffer
    /// (mixer nodes: per-channel gain/pan/mute/solo); `None` (the default) =
    /// unit weights, plain summing. Fader moves ride the parameter path below
    /// — they never trigger a topology rebuild (D5).
    fn input_weights(&self) -> Option<&ChannelWeights> {
        None
    }

    /// Applies one parameter update at a block boundary (from the control
    /// command queue).
    ///
    /// Returns `true` if this node recognized and consumed the parameter.
    /// RT-safe: implementations must not allocate or panic.
    fn set_param(&mut self, _id: ParamId, _value: f32) -> bool {
        let _ = (_id, _value);
        false
    }

    /// Handles one note event at a block boundary (from the control command
    /// queue).
    ///
    /// Returns `true` if this node recognized and consumed the event.
    /// RT-safe: implementations must not allocate or panic.
    fn note_event(&mut self, _event: NoteEvent) -> bool {
        let _ = _event;
        false
    }
}

/// Pre-allocated serial processing chain (the seed of an effect chain).
///
/// The chain structure is only modified during graph rebuilds (non-real-time
/// thread); the audio thread only does read-only traversal, so there is no
/// allocation at runtime. Hot-swapping the graph structure (double-buffered
/// swap) is deferred to a later roadmap item.
#[derive(Default)]
pub struct ProcessorChain {
    processors: Vec<Box<dyn Processor>>,
}

impl ProcessorChain {
    /// Empty chain. **Not RT-safe** (no allocation, but only relevant to
    /// graph rebuilds).
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a processor. **Not RT-safe**: may only be called before the
    /// audio stream starts or in a future graph rebuild path.
    pub fn push(&mut self, processor: Box<dyn Processor>) {
        self.processors.push(processor);
    }

    /// Number of nodes in the chain. RT-safe.
    pub fn len(&self) -> usize {
        self.processors.len()
    }

    /// Whether the chain is empty. RT-safe.
    pub fn is_empty(&self) -> bool {
        self.processors.is_empty()
    }
}

impl Processor for ProcessorChain {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        for processor in &mut self.processors {
            processor.process(ctx, buffer);
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        // Hand it to the first node that recognizes the parameter. Switch to
        // directed routing once graph node IDs are introduced.
        self.processors.iter_mut().any(|p| p.set_param(id, value))
    }

    fn note_event(&mut self, event: NoteEvent) -> bool {
        self.processors.iter_mut().any(|p| p.note_event(event))
    }
}

#[cfg(test)]
mod tests {
    use super::{Processor, ProcessorChain};
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::gain::GainProcessor;

    fn ctx(frames: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: 48_000.0,
            frames,
            playback_sample_pos: 0,
        }
    }

    #[test]
    fn chain_applies_processors_in_order() {
        // 0.5 → 0.5: two chained 0.5 gains yield 0.25.
        let mut chain = ProcessorChain::new();
        chain.push(Box::new(GainProcessor::new(0.5)));
        chain.push(Box::new(GainProcessor::new(0.5)));
        assert_eq!(chain.len(), 2);

        let mut buf = AudioBuffer::new(1, 8);
        buf.set_len(8);
        buf.channel_mut(0).unwrap().fill(1.0);
        chain.process(&ctx(8), &mut buf);
        assert!(
            buf.channel(0)
                .unwrap()
                .iter()
                .all(|s| (*s - 0.25).abs() < 1e-6)
        );
    }

    #[test]
    fn chain_routes_param_to_first_handler() {
        let mut chain = ProcessorChain::new();
        chain.push(Box::new(GainProcessor::new(1.0)));
        assert!(chain.set_param(ParamId::MasterGain, 0.5));
        assert!(!chain.set_param(ParamId::SineFrequency, 100.0));

        let mut buf = AudioBuffer::new(1, 4);
        buf.set_len(4);
        buf.channel_mut(0).unwrap().fill(1.0);
        chain.process(&ctx(4), &mut buf);
        assert!(
            buf.channel(0)
                .unwrap()
                .iter()
                .all(|s| (*s - 0.5).abs() < 1e-6)
        );
    }
}
