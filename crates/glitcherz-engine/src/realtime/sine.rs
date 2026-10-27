//! Sine wave generator.

use std::f32::consts::TAU;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::smoothed::coeff_for;
use crate::realtime::processor::Processor;

/// One-pole time constant for pitch changes (ms). Short enough that a
/// deliberate slide keeps its shape, long enough that a curve sampled per
/// block arrives as a ramp rather than as steps (D11: the whole point of the
/// parameter path over MIDI bend).
const PITCH_SMOOTHING_MS: f32 = 5.0;

/// Pitch-offset span accepted by [`ParamId::PitchOffset`] (semitones).
const PITCH_OFFSET_LIMIT: f32 = 24.0;

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
    /// Current pitch multiplier (`2^(offset/12)`), advanced **per sample**.
    pitch_ratio: f32,
    /// Where the multiplier is heading (set once per block by the curve).
    pitch_target: f32,
    /// Per-sample one-pole coefficient.
    pitch_coeff: f32,
}

impl SineProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction-time
    /// tolerance, not on the real-time path).
    pub fn new(sample_rate: f32) -> Self {
        debug_assert!(sample_rate > 0.0, "sample_rate must be positive");
        let sample_rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            sample_rate,
            phase: 0.0,
            frequency_hz: 440.0,
            gain: 0.25,
            pitch_ratio: 1.0,
            pitch_target: 1.0,
            pitch_coeff: coeff_for(PITCH_SMOOTHING_MS, sample_rate),
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

    /// Sets the pitch offset in semitones (clamped ±24, non-finite ignored).
    ///
    /// Stored as a **ratio target**: the render loop interpolates toward it
    /// per sample, so a curve sampled once per block is heard as a glide.
    /// RT-safe.
    pub fn set_pitch_offset(&mut self, semitones: f32) {
        if semitones.is_finite() {
            let semitones = semitones.clamp(-PITCH_OFFSET_LIMIT, PITCH_OFFSET_LIMIT);
            self.pitch_target = (semitones / 12.0).exp2();
        }
    }

    /// Current pitch multiplier (`1.0` = no offset). RT-safe.
    pub fn pitch_ratio(&self) -> f32 {
        self.pitch_ratio
    }
}

impl Processor for SineProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        if frames == 0 || buffer.channels() == 0 {
            return;
        }
        let base_inc = self.frequency_hz / self.sample_rate;
        let gain = self.gain;
        let target = self.pitch_target;
        let coeff = self.pitch_coeff;
        let mut ratio = self.pitch_ratio;
        let mut phase = self.phase;

        // Generate into channel 0 first, then copy to the remaining channels,
        // avoiding recomputing sin per channel.
        if let Some(ch0) = buffer.channel_mut(0) {
            for slot in ch0.iter_mut() {
                *slot = (TAU * phase).sin() * gain;
                // P1/D11: the pitch multiplier moves **inside** the sample
                // loop. Moving it per block is what makes a MIDI bend sound
                // like a staircase; here a block-boundary step becomes a
                // 5 ms glide and the spectrum stays clean.
                phase += (base_inc * ratio).clamp(0.0, 0.5);
                if phase >= 1.0 {
                    phase -= 1.0;
                }
                ratio += (target - ratio) * coeff;
            }
        }
        self.pitch_ratio = ratio;
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
            ParamId::PitchOffset => {
                self.set_pitch_offset(value);
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
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
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

    /// Indices of rising zero crossings (the sine's positive half-cycles).
    fn rising_crossings(samples: &[f32]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut prev = 0.0f32;
        for (i, sample) in samples.iter().enumerate() {
            if prev <= 0.0 && *sample > 0.0 {
                out.push(i);
            }
            prev = *sample;
        }
        out
    }

    /// P1/D11: a pitch-offset change must be a **ramp inside the block**, not
    /// a step at the block boundary. Block-rate application would leave every
    /// half-cycle in the block the same length; per-sample smoothing cannot.
    #[test]
    fn pitch_offset_glides_within_one_block() {
        let mut sine = SineProcessor::new(SR);
        // Asked before the first sample: the smoother starts at 1.0 and has to
        // travel to 2.0 during the very block we measure.
        sine.set_pitch_offset(12.0);
        let out = render(&mut sine, 8); // 1024 frames
        let crossings = rising_crossings(&out);
        assert!(
            crossings.len() >= 12,
            "need enough cycles to compare, got {}",
            crossings.len()
        );
        let first = crossings[1] - crossings[0];
        let last = crossings[crossings.len() - 1] - crossings[crossings.len() - 2];
        assert!(
            last + 10 < first,
            "half-cycles must shorten as the pitch rises (first = {first}, last = {last})"
        );
        // …and the octave is reached, not merely approached.
        assert!(
            (sine.pitch_ratio() - 2.0).abs() < 0.02,
            "ratio = {}",
            sine.pitch_ratio()
        );
    }

    #[test]
    fn pitch_offset_clamps_and_ignores_non_finite() {
        let mut sine = SineProcessor::new(SR);
        sine.set_pitch_offset(1_000.0);
        assert!(
            (sine.pitch_ratio() - 1.0).abs() < 1e-6,
            "the ratio only moves when rendered"
        );
        // 24 semitones = two octaves.
        let _ = render(&mut sine, 400);
        assert!(
            (sine.pitch_ratio() - 4.0).abs() < 0.05,
            "clamped to ±24 st, ratio = {}",
            sine.pitch_ratio()
        );
        let before = sine.pitch_ratio();
        sine.set_pitch_offset(f32::NAN);
        let _ = render(&mut sine, 10);
        assert!(
            (sine.pitch_ratio() - before).abs() < 1e-6,
            "NaN must not move the pitch"
        );
    }
}
