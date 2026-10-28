//! Silent placeholder source (U13).
//!
//! A node whose voice has no file yet (a SoundFont node born empty, waiting
//! for its `Load File…` click) still needs an engine slot: the document
//! node's cables and MIDI routing are wired at creation time, and the file
//! is swapped in later with `AudioEngine::replace_node` — no cable surgery,
//! no graph rebuild.
//!
//! [`SilentProcessor`] is that slot's occupant: it renders nothing and
//! ignores every event. It deliberately does **not** fall back to the
//! built-in synth: a node labelled "SoundFont" that silently plays a synth
//! patch is the confusing state this type exists to avoid (only a *missing
//! file* keeps the D8 built-in fallback, where the substitution is a hint
//! that something needs re-loading).

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::NoteEvent;
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;

/// Renders silence, consumes every event. RT-safe: a memcpy of zeros.
#[derive(Debug, Default)]
pub struct SilentProcessor;

impl SilentProcessor {
    /// No allocation, no state.
    pub fn new() -> Self {
        Self
    }
}

impl Processor for SilentProcessor {
    /// The graph pre-clears every node buffer, so this is belt-and-braces:
    /// the node cannot leak a stale partial block if that ever changes.
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        buffer.clear();
    }

    /// Consumes notes on purpose: the node owns its MIDI feed, it just has
    /// nothing to play it with yet. Answering `false` would let the graph
    /// fall back to other parameter routes for a voice that has no file.
    fn note_event(&mut self, _event: NoteEvent) -> bool {
        true
    }

    /// Same reasoning as [`Self::note_event`] — a control change addressed to
    /// this node is consumed (and dropped) rather than retried elsewhere.
    fn midi_cc(&mut self, _channel: u8, _controller: u8, _value: u8) -> bool {
        true
    }

    /// Same reasoning as [`Self::note_event`].
    fn midi_pitch_bend(&mut self, _channel: u8, _value: i16) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::SilentProcessor;
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::NoteEvent;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;

    fn ctx(frames: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: 48_000.0,
            frames,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        }
    }

    /// The documented behavior: whatever the buffer held before the call, the
    /// output block is silence — and notes/CCs are absorbed without touching
    /// it (a SoundFont node waiting for its file stays silent while the
    /// sequencer keeps sending it events).
    #[test]
    fn renders_silence_and_swallows_events() {
        let mut processor = SilentProcessor::new();
        let mut buffer = AudioBuffer::new(2, 128);
        buffer.set_len(128);
        for ch in 0..2 {
            buffer.channel_mut(ch).expect("stereo").fill(0.75);
        }

        assert!(processor.note_event(NoteEvent::On {
            note: 60,
            velocity: 1.0
        }));
        assert!(processor.midi_cc(0, 7, 100));
        processor.process(&ctx(128), &mut buffer);

        for ch in 0..2 {
            assert!(
                buffer
                    .channel(ch)
                    .expect("stereo")
                    .iter()
                    .all(|s| *s == 0.0),
                "channel {ch} must be silent"
            );
        }
        assert!(
            buffer
                .channel(0)
                .expect("stereo")
                .iter()
                .all(|s| s.is_finite())
        );
    }
}
