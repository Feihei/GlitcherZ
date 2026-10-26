//! Linear gain effect.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::Smoothed;
use crate::realtime::effect::descriptor::{ParamSpec, Unit};
use crate::realtime::processor::Processor;

/// Master gain parameter: linear, per-sample, 10 ms smoothing.
const GAIN_SPEC: ParamSpec = ParamSpec::gain("gain", Unit::Percent, 0.0, 2.0, 1.0);

/// Linear gain — the first effect and also the simplest `Processor`
/// reference implementation.
///
/// The gain is smoothed (spec `.agents/spec/effects.md` D8) so fader moves do
/// not stair-step: constructor and pre-render `set_param` values snap, runtime
/// changes ramp over 10 ms. Unity is an exact passthrough (`x * 1.0 == x`).
///
/// Comes with the standard effect tests (AGENTS.md "Testing"): silent input →
/// silent output; full-scale input → no NaN/Inf.
pub struct GainProcessor {
    gain: Smoothed,
}

impl GainProcessor {
    /// Non-finite values fall back to 0 (silence). Assumes a 48 kHz stream;
    /// use [`GainProcessor::new_at`] when the rate is known.
    pub fn new(gain: f32) -> Self {
        Self::new_at(gain, 48_000.0)
    }

    /// Same as [`GainProcessor::new`] with an explicit sample rate (the
    /// smoothing coefficient depends on it). Construction time only.
    pub fn new_at(gain: f32, sample_rate: f32) -> Self {
        let mut smoothed = Smoothed::new(&GAIN_SPEC, sample_rate);
        smoothed.snap(sanitize(gain));
        Self { gain: smoothed }
    }

    /// Current gain. RT-safe.
    pub fn gain(&self) -> f32 {
        self.gain.value()
    }

    /// Sets the gain. Non-finite values are ignored. RT-safe.
    fn set_gain(&mut self, gain: f32) {
        self.gain.set(gain);
    }
}

fn sanitize(gain: f32) -> f32 {
    if gain.is_finite() { gain } else { 0.0 }
}

impl Processor for GainProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        let channels = buffer.channels();
        let unity = self.gain.value() == 1.0 && self.gain.target() == 1.0;
        if unity {
            // Identity fast path: still advance so a later change ramps.
            self.gain.advance(frames);
            return;
        }
        for index in 0..frames {
            // One tick per frame, not per channel, so stereo does not double
            // the ramp speed.
            let gain = self.gain.tick();
            for ch in 0..channels {
                if let Some(channel) = buffer.channel_mut(ch) {
                    channel[index] *= gain;
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
            sec_per_tick: 60.0 / (120.0 * 960.0),
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

    #[test]
    fn runtime_change_ramps_and_unity_is_exact() {
        let mut gain = GainProcessor::new(1.0);
        let mut buf = AudioBuffer::new(1, 32);
        buf.set_len(32);
        buf.channel_mut(0).unwrap().fill(0.5);
        gain.process(&ctx(32), &mut buf);
        assert_eq!(buf.channel(0).unwrap()[0], 0.5, "unity is exact");

        gain.set_param(ParamId::MasterGain, 0.0);
        buf.channel_mut(0).unwrap().fill(1.0);
        gain.process(&ctx(32), &mut buf);
        let first = buf.channel(0).unwrap()[0];
        assert!(
            first > 0.9 && first < 1.0,
            "fader move must ramp, got {first}"
        );
        assert!(
            gain.gain() < 1.0,
            "the smoother must be heading towards the new target"
        );
    }
}
