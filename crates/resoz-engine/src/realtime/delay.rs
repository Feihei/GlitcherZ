//! Feedback delay effect.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;

/// Maximum delay time (seconds) — the ring buffers are pre-allocated once at
/// this capacity during construction.
pub const MAX_DELAY_S: f32 = 1.0;

/// Feedback delay (one pre-allocated ring buffer per channel, L/R).
///
/// Classic feedback delay: `wet = ring[read]`, write `input + wet * feedback`,
/// output `dry * (1 - mix) + wet * mix`. Feedback is clamped to <= 0.9 to keep
/// the decay bounded; denormals are flushed on write. The delay time is an
/// integer number of samples (no interpolation), so changing it at runtime
/// jumps the read position instantly (audible; smoothing left for later).
/// All paths are RT-safe.
pub struct DelayProcessor {
    sample_rate: f32,
    rings: [Box<[f32]>; 2],
    /// Per-channel write pointer (channels are processed in order; sharing a
    /// pointer would desynchronize the two rings).
    write_pos: [usize; 2],
    time_s: f32,
    feedback: f32,
    mix: f32,
}

impl DelayProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction-time
    /// tolerance; allocation only happens here).
    pub fn new(sample_rate: f32) -> Self {
        let sr = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        let capacity = (MAX_DELAY_S * sr) as usize + 1;
        Self {
            sample_rate: sr,
            rings: [
                vec![0.0; capacity].into_boxed_slice(),
                vec![0.0; capacity].into_boxed_slice(),
            ],
            write_pos: [0; 2],
            time_s: 0.3,
            feedback: 0.35,
            mix: 0.0,
        }
    }

    /// Current wet mix ratio. RT-safe.
    pub fn mix(&self) -> f32 {
        self.mix
    }

    /// Delay length (integer samples).
    fn delay_samples(&self) -> usize {
        let t = if self.time_s.is_finite() {
            self.time_s.clamp(0.005, MAX_DELAY_S)
        } else {
            0.3
        };
        (t * self.sample_rate) as usize
    }
}

impl Processor for DelayProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let mix = self.mix.clamp(0.0, 1.0);
        let feedback = if self.feedback.is_finite() {
            self.feedback.clamp(0.0, 0.9)
        } else {
            0.0
        };
        let delay = self.delay_samples();
        let capacity = self.rings[0].len();

        for ch in 0..buffer.channels().min(2) {
            let Some(channel) = buffer.channel_mut(ch) else {
                continue;
            };
            let ring = &mut self.rings[ch];
            let mut write_pos = self.write_pos[ch];
            for sample in channel.iter_mut() {
                let input = *sample;
                let read_pos = (write_pos + capacity - delay) % capacity;
                let wet = ring[read_pos];
                let mut injected = input + wet * feedback;
                if injected.abs() < 1e-18 {
                    injected = 0.0; // flush denormals so silent tails don't burn CPU
                }
                ring[write_pos] = injected;
                *sample = input * (1.0 - mix) + wet * mix;
                write_pos = (write_pos + 1) % capacity;
            }
            self.write_pos[ch] = write_pos;
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        if !value.is_finite() {
            return matches!(
                id,
                ParamId::DelayTime | ParamId::DelayFeedback | ParamId::DelayMix
            );
        }
        match id {
            ParamId::DelayTime => {
                self.time_s = value.clamp(0.005, MAX_DELAY_S);
                true
            }
            ParamId::DelayFeedback => {
                self.feedback = value.clamp(0.0, 0.9);
                true
            }
            ParamId::DelayMix => {
                self.mix = value.clamp(0.0, 1.0);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DelayProcessor, MAX_DELAY_S};
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;

    fn ctx(frames: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames,
            playback_sample_pos: 0,
        }
    }

    /// Pushes `signal` through the effect in blocks, returns channel-0 output.
    fn run_through(delay: &mut DelayProcessor, signal: &[f32]) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, 128);
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(128) {
            buf.set_len(chunk.len());
            buf.channel_mut(0).unwrap().copy_from_slice(chunk);
            delay.process(&ctx(chunk.len()), &mut buf);
            out.extend_from_slice(buf.channel(0).unwrap());
        }
        out
    }

    // Standard test 1: silence in -> silence out (including high feedback).
    #[test]
    fn silence_in_silence_out() {
        let mut delay = DelayProcessor::new(SR);
        delay.set_param(ParamId::DelayMix, 1.0);
        delay.set_param(ParamId::DelayFeedback, 0.9);
        let out = run_through(&mut delay, &vec![0.0; 48_000]);
        assert!(out.iter().all(|s| s.abs() < 1e-9));
    }

    // Standard test 2: full-scale input -> no NaN/Inf.
    #[test]
    fn full_scale_stays_finite() {
        let mut delay = DelayProcessor::new(SR);
        delay.set_param(ParamId::DelayMix, 1.0);
        delay.set_param(ParamId::DelayFeedback, 0.9);
        let signal: Vec<f32> = (0..48_000)
            .map(|i| (TAU * 440.0 * i as f32 / SR).sin())
            .collect();
        let out = run_through(&mut delay, &signal);
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn mix_zero_is_exact_passthrough() {
        let mut delay = DelayProcessor::new(SR);
        let signal: Vec<f32> = (0..2_000)
            .map(|i| (TAU * 440.0 * i as f32 / SR).sin())
            .collect();
        let out = run_through(&mut delay, &signal);
        assert_eq!(out, signal, "mix = 0 must be sample-exact passthrough");
    }

    #[test]
    fn single_tap_lands_at_configured_time() {
        let mut delay = DelayProcessor::new(SR);
        delay.set_param(ParamId::DelayMix, 1.0);
        delay.set_param(ParamId::DelayFeedback, 0.0);
        delay.set_param(ParamId::DelayTime, 0.25); // 12_000 samples

        let mut signal = vec![0.0f32; 30_000];
        signal[0] = 1.0;
        let out = run_through(&mut delay, &signal);
        assert!(
            out[0].abs() < 1e-9,
            "dry signal is fully replaced at 100% wet; t=0 must be silent"
        );
        assert!(
            (out[12_000] - 1.0).abs() < 1e-6,
            "echo must land exactly at 0.25 s"
        );
        assert!(
            out.iter()
                .enumerate()
                .all(|(i, s)| i == 12_000 || s.abs() < 1e-9),
            "with zero feedback there must be a single echo tap"
        );
    }

    #[test]
    fn feedback_taps_decay_geometrically() {
        let mut delay = DelayProcessor::new(SR);
        delay.set_param(ParamId::DelayMix, 1.0);
        delay.set_param(ParamId::DelayFeedback, 0.5);
        delay.set_param(ParamId::DelayTime, 0.25); // 12_000 samples

        let mut signal = vec![0.0f32; 48_000];
        signal[0] = 1.0;
        let out = run_through(&mut delay, &signal);
        assert!((out[12_000] - 1.0).abs() < 1e-6);
        assert!((out[24_000] - 0.5).abs() < 1e-6, "second tap must be 0.5");
        assert!((out[36_000] - 0.25).abs() < 1e-6, "third tap must be 0.25");
    }

    #[test]
    fn channels_delay_independently() {
        let mut delay = DelayProcessor::new(SR);
        delay.set_param(ParamId::DelayMix, 1.0);
        delay.set_param(ParamId::DelayFeedback, 0.0);
        delay.set_param(ParamId::DelayTime, 0.05); // 2_400 samples

        // Feed an impulse to the left channel only; keep the right silent.
        let mut buf = AudioBuffer::new(2, 128);
        buf.set_len(128);
        buf.channel_mut(0).unwrap()[0] = 1.0;
        delay.process(&ctx(128), &mut buf);

        // Render blocks past the delay point (2400 samples); the right channel
        // must stay silent throughout.
        let mut silence = AudioBuffer::new(2, 128);
        silence.set_len(128);
        silence.clear();
        let mut left_at_delay = 0.0f32;
        for block in 0..20 {
            silence.clear();
            silence.set_len(128);
            delay.process(&ctx(128), &mut silence);
            if block == 17 {
                // The impulse block consumed samples [0, 128); this block
                // covers [2304, 2432), so the delay point 2400 sits at
                // offset 96 within the block.
                left_at_delay = silence.channel(0).unwrap()[96];
            }
            assert!(
                silence.channel(1).unwrap().iter().all(|s| s.abs() < 1e-9),
                "right channel must remain silent"
            );
        }
        assert!(
            (left_at_delay - 1.0).abs() < 1e-6,
            "left-channel echo must land on the delay point, got {left_at_delay}"
        );
    }

    #[test]
    fn params_are_clamped() {
        let mut delay = DelayProcessor::new(SR);
        assert!(delay.set_param(ParamId::DelayTime, 10.0));
        assert!((delay.time_s - MAX_DELAY_S).abs() < 1e-6);
        delay.set_param(ParamId::DelayFeedback, 5.0);
        assert!((delay.feedback - 0.9).abs() < 1e-6);
        delay.set_param(ParamId::DelayMix, -1.0);
        assert!((delay.mix - 0.0).abs() < 1e-6);
        assert!(
            !delay.set_param(ParamId::MasterGain, 1.0),
            "unhandled params must return false"
        );
    }
}
