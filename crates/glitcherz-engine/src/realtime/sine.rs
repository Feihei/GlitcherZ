//! Sine wave generator.

use std::f32::consts::TAU;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;

/// Sine wave generator (the demo instrument from roadmap step 1).
///
/// Phase is accumulated normalized in 0..1 and wrapped after each sample,
/// avoiding floating-point precision loss with large values; phase is
/// continuous across blocks (see unit tests).
pub struct SineProcessor {
    sample_rate: f32,
    phase: f32,
    frequency_hz: f32,
    gain: f32,
}

impl SineProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction-time
    /// tolerance, not on the real-time path).
    pub fn new(sample_rate: f32) -> Self {
        debug_assert!(sample_rate > 0.0, "sample_rate must be positive");
        Self {
            sample_rate: if sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            phase: 0.0,
            frequency_hz: 440.0,
            gain: 0.25,
        }
    }

    /// Current frequency (Hz). RT-safe.
    pub fn frequency(&self) -> f32 {
        self.frequency_hz
    }

    /// Current gain (linear). RT-safe.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Sets frequency: clamped to `[0, sample_rate/2]` (Nyquist),
    /// non-finite values ignored. RT-safe. Exposed so composite nodes such
    /// as the synthesizer can change pitch.
    pub fn set_frequency(&mut self, hz: f32) {
        if hz.is_finite() {
            self.frequency_hz = hz.clamp(0.0, self.sample_rate * 0.5);
        }
    }

    /// Sets gain: clamped to `[0, 4]` (ear/clipping protection), non-finite
    /// values ignored. RT-safe.
    fn set_gain(&mut self, gain: f32) {
        if gain.is_finite() {
            self.gain = gain.clamp(0.0, 4.0);
        }
    }
}

impl Processor for SineProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        if frames == 0 || buffer.channels() == 0 {
            return;
        }
        let inc = (self.frequency_hz / self.sample_rate).clamp(0.0, 0.5);
        let gain = self.gain;
        let mut phase = self.phase;

        // Generate into channel 0 first, then copy to the remaining channels,
        // avoiding recomputing sin per channel.
        if let Some(ch0) = buffer.channel_mut(0) {
            for slot in ch0.iter_mut() {
                *slot = (TAU * phase).sin() * gain;
                phase += inc;
                if phase >= 1.0 {
                    phase -= 1.0;
                }
            }
        }
        for ch in 1..buffer.channels() {
            // Copy per channel: take the read-only sample first, then the
            // mutable slot (avoids holding two borrows at once).
            for i in 0..frames {
                let sample = buffer.channel(0).and_then(|c| c.get(i)).copied();
                if let (Some(s), Some(dst)) =
                    (sample, buffer.channel_mut(ch).and_then(|c| c.get_mut(i)))
                {
                    *dst = s;
                }
            }
        }
        self.phase = phase;
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::SineFrequency => {
                self.set_frequency(value);
                true
            }
            ParamId::SineGain => {
                self.set_gain(value);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SineProcessor;
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;
    use crate::testing::count_positive_crossings;

    const SR: f32 = 44_100.0;

    fn ctx(frames: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames,
            playback_sample_pos: 0,
        }
    }

    /// Renders `blocks` fixed-size blocks, returning the channel 0 output.
    fn render(sine: &mut SineProcessor, blocks: usize) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, 128);
        let mut collected = Vec::new();
        for _ in 0..blocks {
            buf.set_len(128);
            buf.clear();
            sine.process(&ctx(128), &mut buf);
            collected.extend_from_slice(buf.channel(0).unwrap());
        }
        collected
    }

    #[test]
    fn frequency_matches_zero_crossings() {
        let mut sine = SineProcessor::new(SR);
        // 344 blocks ≈ 0.9985 s @ 440 Hz → about 439 positive zero crossings.
        let crossings = count_positive_crossings(&render(&mut sine, 344));
        assert!(
            (crossings as i64 - 440).abs() <= 2,
            "crossings = {crossings}"
        );
    }

    #[test]
    fn amplitude_matches_gain() {
        let mut sine = SineProcessor::new(SR);
        let mut buf = AudioBuffer::new(1, 256);
        buf.set_len(256);
        buf.clear();
        sine.process(&ctx(256), &mut buf);
        let peak = buf
            .channel(0)
            .unwrap()
            .iter()
            .fold(0.0f32, |acc, s| acc.max(*s));
        assert!((peak - 0.25).abs() < 1e-3, "peak = {peak}");
    }

    #[test]
    fn full_scale_output_is_finite() {
        let mut sine = SineProcessor::new(SR);
        sine.set_param(ParamId::SineGain, 4.0);
        sine.set_param(ParamId::SineFrequency, SR / 2.0 - 1_000.0);
        let mut buf = AudioBuffer::new(2, 128);
        for _ in 0..1_000 {
            buf.set_len(128);
            buf.clear();
            sine.process(&ctx(128), &mut buf);
            for ch in 0..2 {
                assert!(buf.channel(ch).unwrap().iter().all(|s| s.is_finite()));
            }
        }
    }

    #[test]
    fn params_are_clamped_and_nan_ignored() {
        let mut sine = SineProcessor::new(SR);
        sine.set_param(ParamId::SineFrequency, -50.0);
        assert!(sine.frequency().abs() < 1e-9);

        sine.set_param(ParamId::SineFrequency, f32::NAN);
        assert!(
            sine.frequency().abs() < 1e-9,
            "non-finite values should be ignored"
        );

        sine.set_param(ParamId::SineFrequency, SR);
        assert!(
            (sine.frequency() - SR / 2.0).abs() < 1e-3,
            "clamped to Nyquist"
        );

        sine.set_param(ParamId::SineGain, f32::INFINITY);
        assert!(
            (sine.gain() - 0.25).abs() < 1e-9,
            "non-finite gain should be ignored"
        );

        assert!(
            !sine.set_param(ParamId::MasterGain, 1.0),
            "unhandled parameters should return false"
        );
    }

    #[test]
    fn phase_continuous_across_blocks() {
        // Zero-crossing counts of chunked vs whole-buffer rendering should
        // match (±1).
        let mut sine = SineProcessor::new(SR);
        let chunked = count_positive_crossings(&render(&mut sine, 50));

        let mut sine = SineProcessor::new(SR);
        let mut buf = AudioBuffer::new(1, 128 * 50);
        buf.set_len(128 * 50);
        buf.clear();
        sine.process(&ctx(128 * 50), &mut buf);
        let whole = count_positive_crossings(buf.channel(0).unwrap());

        assert!(
            (chunked as i64 - whole as i64).abs() <= 1,
            "chunked = {chunked}, whole = {whole}"
        );
    }
}
