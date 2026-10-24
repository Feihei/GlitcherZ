//! Linear gain effect.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;

/// Linear gain — the first effect and also the simplest `Processor`
/// reference implementation.
///
/// Comes with the standard effect tests (AGENTS.md "Testing"): silent input →
/// silent output; full-scale input → no NaN/Inf.
pub struct GainProcessor {
    gain: f32,
}

impl GainProcessor {
    /// Non-finite values fall back to 0 (silence), avoiding NaN pollution of
    /// the signal.
    pub fn new(gain: f32) -> Self {
        Self {
            gain: sanitize(gain),
        }
    }

    /// Sets the gain. Non-finite values are ignored (silence). RT-safe.
    fn set_gain(&mut self, gain: f32) {
        self.gain = sanitize(gain);
    }
}

fn sanitize(gain: f32) -> f32 {
    if gain.is_finite() { gain } else { 0.0 }
}

impl Processor for GainProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        if self.gain == 1.0 {
            return; // identity fast path
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
            ParamId::MasterGain => {
                self.set_gain(value);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GainProcessor;
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
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
        }
    }

    // AGENTS.md standard test 1: silent input → silent output.
    #[test]
    fn silence_in_silence_out() {
        let mut gain = GainProcessor::new(2.0);
        let mut buf = AudioBuffer::new(2, 64);
        buf.set_len(64);
        buf.clear();
        gain.process(&ctx(64), &mut buf);
        for ch in 0..2 {
            assert!(buf.channel(ch).unwrap().iter().all(|s| s.abs() < 1e-12));
        }
    }

    // AGENTS.md standard test 2: full-scale input → no NaN/Inf.
    #[test]
    fn full_scale_scales_without_nan() {
        let mut gain = GainProcessor::new(0.5);
        let mut buf = AudioBuffer::new(1, 64);
        buf.set_len(64);
        buf.channel_mut(0).unwrap().fill(1.0);
        gain.process(&ctx(64), &mut buf);
        for s in buf.channel(0).unwrap() {
            assert!(s.is_finite());
            assert!((*s - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn master_gain_param_routes() {
        let mut gain = GainProcessor::new(1.0);
        assert!(gain.set_param(ParamId::MasterGain, 0.5));
        assert!(!gain.set_param(ParamId::SineFrequency, 100.0));

        let mut buf = AudioBuffer::new(1, 8);
        buf.set_len(8);
        buf.channel_mut(0).unwrap().fill(1.0);
        gain.process(&ctx(8), &mut buf);
        assert!(
            buf.channel(0)
                .unwrap()
                .iter()
                .all(|s| (*s - 0.5).abs() < 1e-6)
        );
    }

    #[test]
    fn nan_gain_is_silenced() {
        let mut gain = GainProcessor::new(f32::NAN);
        let mut buf = AudioBuffer::new(1, 8);
        buf.set_len(8);
        buf.channel_mut(0).unwrap().fill(1.0);
        gain.process(&ctx(8), &mut buf);
        assert!(buf.channel(0).unwrap().iter().all(|s| s.is_finite()));
        assert!(buf.channel(0).unwrap().iter().all(|s| s.abs() < 1e-12));
    }
}
