//! Band-limited oscillator (sine / triangle / saw / square).
//!
//! The oscillator is the demo instrument's source from roadmap step 1,
//! generalized in spec `instrument.md`: the phase accumulator stays the same,
//! the waveform is selected by index and saw / square are corrected with
//! PolyBLEP so high notes do not alias.

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

/// Number of waveforms the oscillator understands (`ParamId::OscWaveform`).
pub const OSC_WAVEFORM_COUNT: u8 = 4;

/// Crossfade length when the waveform is switched while sounding, in
/// seconds (spec `instrument.md` §6: the phase is continuous but the
/// instantaneous value jumps, and the jump is a click).
const WAVEFORM_FADE_SECONDS: f32 = 0.005;

/// Sine / triangle / saw / square oscillator.
///
/// Phase is accumulated normalized in 0..1 and wrapped after each sample,
/// avoiding floating-point precision loss with large values; phase is
/// continuous across blocks and across waveform changes (see unit tests).
/// The waveform index follows the document's `SynthWaveform` order:
/// 0 = sine, 1 = triangle, 2 = saw, 3 = square.
pub struct OscProcessor {
    sample_rate: f32,
    phase: f32,
    frequency_hz: f32,
    gain: f32,
    /// 0 = sine, 1 = triangle, 2 = saw, 3 = square.
    waveform: u8,
    /// The waveform being faded out after a switch, and how far the fade has
    /// got (`fade_pos >= fade_len` = idle). Both waveforms are evaluated at
    /// the **same** phase, so the crossfade costs no second phase accumulator
    /// and cannot drift.
    prev_waveform: u8,
    fade_pos: f32,
    fade_len: f32,
    /// Set by the first rendered block. Before that nothing has sounded, so a
    /// waveform choice carries no click and must not be faded (a construction
    /// -time selection should produce the requested shape from sample 0).
    started: bool,
    /// Current pitch multiplier (`2^(offset/12)`), advanced **per sample**.
    pitch_ratio: f32,
    /// Where the multiplier is heading (set once per block by the curve).
    pitch_target: f32,
    /// Per-sample one-pole coefficient.
    pitch_coeff: f32,
}

/// PolyBLEP residual for one discontinuity at phase `0` (or `1`).
///
/// `t` is the normalized phase, `dt` the phase increment of the sample. The
/// correction spans one sample on either side of the jump; `dt` is clamped and
/// a zero/negative increment returns 0 so the division can never produce NaN.
fn poly_blep(t: f32, dt: f32) -> f32 {
    if !dt.is_finite() || dt <= 0.0 {
        return 0.0;
    }
    let dt = dt.min(0.5);
    if t < dt {
        let x = t / dt;
        x + x - x * x - 1.0
    } else if t > 1.0 - dt {
        let x = (t - 1.0) / dt;
        x * x + x + x + 1.0
    } else {
        0.0
    }
}

/// Waveform sample at `phase` in 0..1 (without gain).
///
/// Triangle / saw / square follow the LFO's phase convention
/// (spec `lfo.md` §4): triangle starts at `-1`, saw is the rising ramp
/// `2φ - 1`, square is `+1` for the first half cycle.
fn wave_sample(waveform: u8, phase: f32, dt: f32) -> f32 {
    match waveform {
        1 => 1.0 - 4.0 * (phase - 0.5).abs(),
        2 => {
            let ramp = 2.0 * phase - 1.0;
            ramp - poly_blep(phase, dt)
        }
        3 => {
            let mut square = if phase < 0.5 { 1.0 } else { -1.0 };
            let mut half = phase + 0.5;
            if half >= 1.0 {
                half -= 1.0;
            }
            square += poly_blep(phase, dt);
            square -= poly_blep(half, dt);
            square
        }
        _ => (TAU * phase).sin(),
    }
}

impl OscProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction-time
    /// tolerance, not on the real-time path).
    pub fn new(sample_rate: f32) -> Self {
        debug_assert!(sample_rate > 0.0, "sample_rate must be positive");
        let sample_rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        let fade_len = (WAVEFORM_FADE_SECONDS * sample_rate).max(1.0);
        Self {
            sample_rate,
            phase: 0.0,
            frequency_hz: 440.0,
            gain: 0.25,
            waveform: 0,
            prev_waveform: 0,
            // `fade_pos >= fade_len` = idle, so a fresh oscillator never fades.
            fade_pos: fade_len,
            fade_len,
            started: false,
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

    /// Current waveform index (0..3). RT-safe.
    pub fn waveform(&self) -> u8 {
        self.waveform
    }

    /// Sets the waveform index (clamped to `0..3`; non-finite values are
    /// ignored). A change made while the oscillator is sounding arms a
    /// [`WAVEFORM_FADE_SECONDS`] crossfade instead of a step. RT-safe.
    fn set_waveform(&mut self, waveform: f32) {
        if !waveform.is_finite() {
            return;
        }
        let next = (waveform.round().max(0.0) as u8).min(OSC_WAVEFORM_COUNT - 1);
        if next == self.waveform {
            return;
        }
        if self.started {
            // A second switch while a fade is still running: restart from
            // whichever waveform the listener is actually hearing, so the
            // restart adds at most half a waveform step. Retriggering faster
            // than 5 ms is not a musical gesture, it only has to stay bounded.
            let progress = if self.fade_len > 0.0 {
                (self.fade_pos / self.fade_len).clamp(0.0, 1.0)
            } else {
                1.0
            };
            self.prev_waveform = if progress < 0.5 {
                self.prev_waveform
            } else {
                self.waveform
            };
            self.fade_pos = 0.0;
        } else {
            // Nothing has sounded yet: no click to hide.
            self.prev_waveform = next;
        }
        self.waveform = next;
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

impl Processor for OscProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        if frames == 0 || buffer.channels() == 0 {
            return;
        }
        let base_inc = self.frequency_hz / self.sample_rate;
        let gain = self.gain;
        let waveform = self.waveform;
        let prev_waveform = self.prev_waveform;
        let fade_len = self.fade_len;
        let mut fade_pos = self.fade_pos;
        let target = self.pitch_target;
        let coeff = self.pitch_coeff;
        let mut ratio = self.pitch_ratio;
        let mut phase = self.phase;

        // Generate into channel 0 first, then copy to the remaining channels,
        // avoiding recomputing the waveform per channel.
        if let Some(ch0) = buffer.channel_mut(0) {
            for slot in ch0.iter_mut() {
                let inc = (base_inc * ratio).clamp(0.0, 0.5);
                let mut sample = wave_sample(waveform, phase, inc);
                if fade_pos < fade_len {
                    // Linear crossfade at the **shared** phase: the two
                    // waveforms are read at the same `phase`, so the sum stays
                    // amplitude-consistent and no second accumulator exists to
                    // drift out of tune.
                    let w = (fade_pos / fade_len).clamp(0.0, 1.0);
                    let old = wave_sample(prev_waveform, phase, inc);
                    sample = old + (sample - old) * w;
                    fade_pos += 1.0;
                }
                *slot = sample * gain;
                // P1/D11: the pitch multiplier moves **inside** the sample
                // loop. Moving it per block is what makes a MIDI bend sound
                // like a staircase; here a block-boundary step becomes a
                // 5 ms glide and the spectrum stays clean.
                phase += inc;
                if phase >= 1.0 {
                    phase -= 1.0;
                }
                ratio += (target - ratio) * coeff;
            }
        }
        self.pitch_ratio = ratio;
        self.fade_pos = if fade_pos >= fade_len {
            fade_len
        } else {
            fade_pos
        };
        self.started = true;
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
            ParamId::OscFrequency => {
                self.set_frequency(value);
                true
            }
            ParamId::OscGain => {
                self.set_gain(value);
                true
            }
            ParamId::OscWaveform => {
                self.set_waveform(value);
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
    use super::{OSC_WAVEFORM_COUNT, OscProcessor};
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
    fn render(osc: &mut OscProcessor, blocks: usize) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, 128);
        let mut collected = Vec::new();
        for _ in 0..blocks {
            buf.set_len(128);
            buf.clear();
            osc.process(&ctx(128), &mut buf);
            collected.extend_from_slice(buf.channel(0).unwrap());
        }
        collected
    }

    fn oscillator(waveform: u8) -> OscProcessor {
        let mut osc = OscProcessor::new(SR);
        osc.set_param(ParamId::OscWaveform, waveform as f32);
        osc
    }

    /// Renders exactly `frames` frames in one block, returning channel 0.
    fn render_frames(osc: &mut OscProcessor, frames: usize) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, frames);
        buf.set_len(frames);
        buf.clear();
        osc.process(&ctx(frames), &mut buf);
        buf.channel(0).unwrap().to_vec()
    }

    /// Largest absolute sample-to-sample step in a stream.
    fn max_step(samples: &[f32]) -> f32 {
        samples
            .windows(2)
            .fold(0.0f32, |acc, pair| acc.max((pair[1] - pair[0]).abs()))
    }

    #[test]
    fn frequency_matches_zero_crossings() {
        let mut sine = OscProcessor::new(SR);
        // 344 blocks ≈ 0.9985 s @ 440 Hz → about 439 positive zero crossings.
        let crossings = count_positive_crossings(&render(&mut sine, 344));
        assert!(
            (crossings as i64 - 440).abs() <= 2,
            "crossings = {crossings}"
        );
    }

    /// Every waveform must run at the requested pitch: one rising edge per
    /// period, whatever the shape.
    #[test]
    fn all_waveforms_match_the_note_frequency() {
        for index in 0..OSC_WAVEFORM_COUNT {
            let mut osc = oscillator(index);
            let crossings = count_positive_crossings(&render(&mut osc, 344));
            assert!(
                (crossings as i64 - 440).abs() <= 4,
                "waveform {index}: crossings = {crossings}"
            );
        }
    }

    #[test]
    fn waveform_index_is_clamped() {
        let mut osc = OscProcessor::new(SR);
        osc.set_param(ParamId::OscWaveform, 99.0);
        assert_eq!(osc.waveform(), OSC_WAVEFORM_COUNT - 1);
        osc.set_param(ParamId::OscWaveform, -5.0);
        assert_eq!(osc.waveform(), 0);
        osc.set_param(ParamId::OscWaveform, 2.6);
        assert_eq!(osc.waveform(), 3, "rounds to the nearest waveform");
        osc.set_param(ParamId::OscWaveform, f32::NAN);
        assert_eq!(osc.waveform(), 3, "non-finite values are ignored");
    }

    #[test]
    fn waveform_shapes_are_distinct() {
        // Sine, triangle, saw and square all reach the same fundamental, so
        // the check is on their shape: compare the first 64 samples pairwise.
        // (RMS would not work — a triangle and a saw both measure 1/√3.)
        let values: Vec<Vec<f32>> = (0..OSC_WAVEFORM_COUNT)
            .map(|index| render(&mut oscillator(index), 1))
            .collect();
        for (i, a) in values.iter().enumerate() {
            for (j, b) in values.iter().enumerate().skip(i + 1) {
                let difference: f32 = a
                    .iter()
                    .zip(b.iter())
                    .map(|(x, y)| (x - y).abs())
                    .sum::<f32>()
                    / a.len() as f32;
                assert!(
                    difference > 0.05,
                    "waveforms {i} and {j} look alike (mean |diff| = {difference})"
                );
            }
        }
    }

    /// The reason PolyBLEP exists: a naive square at a high note folds its
    /// harmonics back down as inharmonic energy. Corrected, the waveform stays
    /// close to ±1 (a naive one does too at the sample points, but its
    /// interpolation shows ringing); the robust check here is that the
    /// corrected square has no sample outside its range.
    #[test]
    fn corrected_square_stays_inside_its_range() {
        let mut osc = oscillator(3);
        osc.set_param(ParamId::OscGain, 1.0);
        osc.set_param(ParamId::OscFrequency, 5_000.0);
        let out = render(&mut osc, 20);
        assert!(out.iter().all(|s| s.is_finite()));
        let peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(peak <= 1.05, "peak = {peak}");
    }

    #[test]
    fn amplitude_matches_gain() {
        let mut sine = OscProcessor::new(SR);
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
        let mut sine = OscProcessor::new(SR);
        sine.set_param(ParamId::OscGain, 4.0);
        sine.set_param(ParamId::OscFrequency, SR / 2.0 - 1_000.0);
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

    /// E5 `declick()` for the synth: switching waveform mid-note must not
    /// step. The switch is deliberately made where sine and triangle are
    /// maximally apart (sine ≈ +1, triangle ≈ 0), i.e. a full-scale jump if
    /// the waveform were swapped outright. The crossfade has to turn it into a
    /// ramp whose per-sample rise stays below the sine's own slope plus the
    /// fade slope.
    #[test]
    fn a_waveform_switch_is_crossfaded() {
        let mut osc = OscProcessor::new(SR);
        osc.set_param(ParamId::OscGain, 1.0);
        osc.set_param(ParamId::OscFrequency, 440.0);

        // 440 Hz at 44.1 kHz: 25 frames land on phase 25 * 22/2205 ≈ 0.2494.
        let mut out = render_frames(&mut osc, 25);
        let phase_sum: f32 = 25.0 * 440.0 / SR;
        assert!((phase_sum - 0.25).abs() < 0.01, "phase = {phase_sum}");

        osc.set_param(ParamId::OscWaveform, 1.0);
        out.extend_from_slice(&render_frames(&mut osc, 512));

        assert!(out.iter().all(|s| s.is_finite()), "output must stay finite");
        let step = max_step(&out);
        // Sine slope at 440 Hz is 2π·440/44100 ≈ 0.063; the fade adds at most
        // |tri - sine| / fade_len ≈ 0.005 per sample. An un-faded switch would
        // show a step near 1.0 here.
        assert!(step < 0.10, "switching stepped by {step}");
        let peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(
            peak <= 1.05,
            "the crossfade must not overshoot, peak = {peak}"
        );
        assert!(
            osc.fade_pos >= osc.fade_len,
            "the crossfade should be finished after 512 frames"
        );
        assert_eq!(osc.waveform(), 1);
    }

    /// Nothing has sounded before the first block, so a waveform selected at
    /// construction is heard from sample 0 — no fade, no ramp in.
    #[test]
    fn a_waveform_change_before_playback_does_not_fade() {
        let mut triangle = oscillator(1);
        assert!(triangle.fade_pos >= triangle.fade_len, "must start idle");
        let out = render_frames(&mut triangle, 32);
        // A triangle starts at -1 (× the default 0.25 gain); a fade from the
        // default sine would start at 0 instead. (Saw is no good here: its
        // PolyBLEP correction puts the corrected value at 0 for phase 0.)
        assert!(
            (out[0] + 0.25).abs() < 1e-6,
            "first sample should be the triangle's -1, got {}",
            out[0]
        );
    }

    #[test]
    fn params_are_clamped_and_nan_ignored() {
        let mut sine = OscProcessor::new(SR);
        sine.set_param(ParamId::OscFrequency, -50.0);
        assert!(sine.frequency().abs() < 1e-9);

        sine.set_param(ParamId::OscFrequency, f32::NAN);
        assert!(
            sine.frequency().abs() < 1e-9,
            "non-finite values should be ignored"
        );

        sine.set_param(ParamId::OscFrequency, SR);
        assert!(
            (sine.frequency() - SR / 2.0).abs() < 1e-3,
            "clamped to Nyquist"
        );

        sine.set_param(ParamId::OscGain, f32::INFINITY);
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
        let mut sine = OscProcessor::new(SR);
        let chunked = count_positive_crossings(&render(&mut sine, 50));

        let mut sine = OscProcessor::new(SR);
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
        let mut sine = OscProcessor::new(SR);
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
        let mut sine = OscProcessor::new(SR);
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
