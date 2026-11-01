//! Graph node for live audio input (R4, spec `recording.md` D9).
//!
//! The node owns a clone of the monitor ring; the cpal input callback owns
//! another. Per block it drains exactly `frames × device_channels` interleaved
//! samples and writes the selected channel to both output channels, scaled by
//! the node gain. A short ring means silence, not a stall: keeping the
//! callback lock-free is worth more than a seamless monitor.
//!
//! RT-safe: no allocation, no locks, no panics.

use std::sync::Arc;

use crate::audio_input::{InputRing, InputStats};
use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;

/// Live audio input node processor.
pub struct InputProcessor {
    ring: InputRing,
    /// Channels in the interleaved input stream.
    device_channels: usize,
    /// Selected channel (0-based).
    channel: usize,
    gain: f32,
    underruns: u64,
    stats: Arc<InputStats>,
}

impl InputProcessor {
    /// Builds the processor from the shared monitor ring. **Not RT-safe**.
    pub fn new(ring: InputRing, device_channels: usize, channel: usize, gain: f32) -> Self {
        let device_channels = device_channels.max(1);
        Self {
            ring,
            device_channels,
            channel: channel.min(device_channels - 1),
            gain: if gain.is_finite() {
                gain.clamp(0.0, 2.0)
            } else {
                1.0
            },
            underruns: 0,
            stats: Arc::new(InputStats::default()),
        }
    }

    /// Shares the live stream's stats handle (tests / UI peak meters).
    pub fn with_stats(mut self, stats: Arc<InputStats>) -> Self {
        self.stats = stats;
        self
    }
}

impl Processor for InputProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let Some((left, right)) = buffer.stereo_channels_mut() else {
            return;
        };
        let frames = left.len().min(right.len());
        let mut underruns = 0u64;
        for i in 0..frames {
            let mut selected = 0.0;
            for channel in 0..self.device_channels {
                match self.ring.pop() {
                    Some(sample) => {
                        if channel == self.channel {
                            selected = sample;
                        }
                    }
                    None => {
                        underruns += 1;
                    }
                }
            }
            let value = selected * self.gain;
            left[i] = value;
            right[i] = value;
        }
        if underruns > 0 {
            self.underruns = self.underruns.saturating_add(underruns);
            self.stats.set_underruns(self.underruns);
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::InputGain => {
                if value.is_finite() {
                    self.gain = value.clamp(0.0, 2.0);
                }
                true
            }
            ParamId::InputChannel => {
                self.channel =
                    (value.max(0.0) as usize).min(self.device_channels.saturating_sub(1));
                true
            }
            _ => false,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::InputProcessor;
    use crate::audio_input::InputRing;
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;
    use std::sync::Arc;

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

    #[test]
    fn a_block_reads_the_selected_channel_with_gain() {
        let ring = InputRing::new(64);
        let mut processor = InputProcessor::new(ring.clone(), 2, 1, 0.5);
        for _ in 0..8 {
            ring.push(0.25); // left, ignored
            ring.push(0.8); // right, selected
        }
        let mut buffer = AudioBuffer::new(2, 8);
        buffer.set_len(8);
        processor.process(&ctx(8), &mut buffer);
        for frame in 0..8 {
            assert!((buffer.channel(0).unwrap()[frame] - 0.4).abs() < 1e-6);
            assert!((buffer.channel(1).unwrap()[frame] - 0.4).abs() < 1e-6);
        }
    }

    #[test]
    fn an_empty_input_ring_produces_silence_not_a_panic() {
        let ring = InputRing::new(8);
        let processor = InputProcessor::new(ring, 1, 0, 1.0);
        let stats = Arc::new(crate::audio_input::InputStats::default());
        let mut processor = processor.with_stats(stats.clone());
        let mut buffer = AudioBuffer::new(2, 16);
        buffer.set_len(16);
        processor.process(&ctx(16), &mut buffer);
        assert!(buffer.channel(0).unwrap().iter().all(|s| *s == 0.0));
        assert_eq!(stats.underruns(), 16);
    }

    #[test]
    fn gain_and_channel_follow_the_parameter_path() {
        let ring = InputRing::new(16);
        let mut processor = InputProcessor::new(ring.clone(), 2, 0, 1.0);
        assert!(processor.set_param(ParamId::InputGain, 0.5));
        assert!(processor.set_param(ParamId::InputChannel, 1.0));
        ring.push(1.0);
        ring.push(0.2);
        let mut buffer = AudioBuffer::new(2, 1);
        buffer.set_len(1);
        processor.process(&ctx(1), &mut buffer);
        assert!((buffer.channel(0).unwrap()[0] - 0.1).abs() < 1e-6);

        // Out-of-range parameters are clamped, not rejected / panicked.
        processor.set_param(ParamId::InputGain, f32::NAN);
        processor.set_param(ParamId::InputChannel, 99.0);
        assert!(!processor.set_param(ParamId::MasterGain, 1.0));
    }

    #[test]
    fn the_ring_is_fifo_and_reports_full() {
        let ring = InputRing::new(1);
        assert!(ring.push(0.5));
        assert!(!ring.push(1.0), "one usable slot");
        assert_eq!(ring.pop(), Some(0.5));
        assert_eq!(ring.pop(), None);
    }
}
