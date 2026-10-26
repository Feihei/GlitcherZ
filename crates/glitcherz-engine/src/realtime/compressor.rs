//! Feed-forward peak compressor (spec `.agents/spec/effects.md` E2b).
//!
//! The gain computer works in dB and is a pure function ([`compute_gr_db`]) so
//! the curve can be tested without a device:
//!
//! ```text
//! over = level_db − threshold
//! knee = 0:  gr = over · slope                       (over > 0 only)
//! knee > 0:  |over| <= knee/2 → gr = slope · t² / (2·knee),  t = over + knee/2
//!            over >  knee/2   → gr = over · slope
//!            over < −knee/2   → gr = 0
//! slope = 1/ratio − 1  (<= 0, because the output is always gain *reduction*)
//! ```
//!
//! Both knee branches meet at `t = knee` with matching slope, so the curve is
//! continuous and smooth in the knee — that is what "soft knee" means here.
//!
//! Attack and release are one-poles on **the GR value in dB**, not on the level
//! detector, which is the usual DAW behaviour: the reduction moves toward its
//! target at the attack rate when it grows and at the release rate when it
//! shrinks.
//!
//! Stereo: the detector is per channel; when `stereo_link` is on both channels
//! share one detector using `max(|L|, |R|)` (D14), so a loud left channel
//! reduces the right one by exactly the same amount.

use std::sync::Arc;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::EffectParamBank;
use crate::realtime::effect::descriptor::{
    EffectDescriptor, ParamFlags, ParamSpec, StereoClass, Unit,
};
use crate::realtime::effect::meters::EffectMeters;
use crate::realtime::effect::smoothed::coeff_for;
use crate::realtime::processor::Processor;

/// Slot of the threshold (dB).
pub const SLOT_THRESHOLD: u8 = 0;
/// Slot of the compression ratio.
pub const SLOT_RATIO: u8 = 1;
/// Slot of the attack time (ms).
pub const SLOT_ATTACK: u8 = 2;
/// Slot of the release time (ms).
pub const SLOT_RELEASE: u8 = 3;
/// Slot of the soft-knee width (dB).
pub const SLOT_KNEE: u8 = 4;
/// Slot of the makeup gain (dB).
pub const SLOT_MAKEUP: u8 = 5;
/// Slot of the dry/wet mix.
pub const SLOT_MIX: u8 = 6;
/// Slot of the stereo-link switch.
pub const SLOT_LINK: u8 = 7;

static COMPRESSOR_PARAMS: [ParamSpec; 8] = [
    ParamSpec::level("threshold", -60.0, 0.0, -18.0)
        .short("thr")
        .inline_row(),
    ParamSpec::ratio("ratio", 1.0, 20.0, 4.0).inline_row(),
    ParamSpec::time("attack", 0.1, 200.0, 10.0),
    ParamSpec::time("release", 5.0, 2_000.0, 100.0),
    ParamSpec::level("knee", 0.0, 24.0, 6.0),
    ParamSpec::gain("makeup", Unit::Db, 0.0, 24.0, 0.0)
        .short("mkup")
        .inline_row(),
    ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 1.0),
    ParamSpec::switch_with("stereo_link", 1.0, ParamFlags::STEREO_LINK),
];

/// Static parameter table (spec `.agents/spec/effects.md` E2b).
pub static DESCRIPTOR: EffectDescriptor = EffectDescriptor {
    name: "Compressor",
    params: &COMPRESSOR_PARAMS,
    stereo: StereoClass::Linkable { default_link: true },
};

/// Level floor: anything quieter is treated as −120 dB.
const MIN_LEVEL_DB: f32 = -120.0;
/// Linear amplitude below which the level is floored (≈ −120 dB).
const MIN_PEAK: f32 = 1e-6;

/// Gain reduction in dB (`<= 0`) for one detector level.
///
/// Pure, branch-light and RT-safe: this is the whole "curve" of the
/// compressor, testable without any audio buffer.
pub fn compute_gr_db(level_db: f32, threshold: f32, ratio: f32, knee: f32) -> f32 {
    let over = level_db - threshold;
    // `ratio >= 1`, so the slope is never positive.
    let slope = 1.0 / ratio.max(1.0) - 1.0;
    if knee <= 0.0 {
        return if over > 0.0 { over * slope } else { 0.0 };
    }
    let half = knee * 0.5;
    if over <= -half {
        0.0
    } else if over >= half {
        over * slope
    } else {
        // Quadratic interpolation across the knee; meets both hard-knee
        // branches with a matching value *and* a matching slope.
        let t = over + half;
        slope * t * t / (2.0 * knee)
    }
}

/// dB amplitude → linear (`-inf`-safe).
fn level_db(peak: f32) -> f32 {
    if peak > MIN_PEAK {
        20.0 * peak.log10()
    } else {
        MIN_LEVEL_DB
    }
}

/// Output factor for one gain-reduction value, including makeup and mix.
///
/// `mix = 0` returns exactly `1.0`, which keeps the dry path sample-exact.
fn mix_factor(gr_db: f32, makeup_db: f32, mix: f32) -> f32 {
    let gain = 10f32.powf((gr_db + makeup_db) * 0.05);
    1.0 + mix * (gain - 1.0)
}

/// Per-block compressor settings (everything the sample loop needs).
#[derive(Clone, Copy)]
struct Settings {
    threshold: f32,
    ratio: f32,
    knee: f32,
    /// One-pole coefficient while the reduction grows.
    attack: f32,
    /// One-pole coefficient while it shrinks.
    release: f32,
}

/// Feed-forward peak compressor (1 in, 1 out, zero latency).
pub struct CompressorProcessor {
    sample_rate: f32,
    params: EffectParamBank,
    /// Per-channel gain reduction in dB (both entries used when unlinked).
    gr: [f32; 2],
    /// Shared with the UI when the node is created through the app; `None` in
    /// tests and in offline rendering.
    meters: Option<Arc<EffectMeters>>,
}

impl CompressorProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction tolerance).
    pub fn new(sample_rate: f32) -> Self {
        Self::with_meters(sample_rate, None)
    }

    /// Same, with a meter handle the control thread keeps a clone of.
    /// Construction time only.
    pub fn with_meters(sample_rate: f32, meters: Option<Arc<EffectMeters>>) -> Self {
        let sr = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            sample_rate: sr,
            params: EffectParamBank::new(&DESCRIPTOR, sr),
            gr: [0.0; 2],
            meters,
        }
    }

    /// Current (settled) value of a slot, for tests and read-back.
    pub fn value(&self, slot: u8) -> f32 {
        self.params.value(slot)
    }

    /// Current gain reduction of one channel (dB, `<= 0`).
    pub fn gain_reduction_db(&self, channel: usize) -> f32 {
        self.gr.get(channel).copied().unwrap_or(0.0)
    }

    /// One-pole step of the GR value toward `target`.
    ///
    /// Uses the attack coefficient while the reduction grows (target below the
    /// current value) and the release coefficient otherwise. Includes the same
    /// no-progress snap as [`Smoothed`](crate::realtime::effect::Smoothed):
    /// in f32 a one-pole can otherwise stall a few 1e-5 short of its target.
    fn step_gr(&mut self, channel: usize, peak: f32, settings: &Settings) -> f32 {
        let target = compute_gr_db(
            level_db(peak),
            settings.threshold,
            settings.ratio,
            settings.knee,
        );
        let Some(current) = self.gr.get_mut(channel) else {
            return target;
        };
        let coeff = if target < *current {
            settings.attack
        } else {
            settings.release
        };
        let next = *current + (target - *current) * coeff;
        *current = if next == *current { target } else { next };
        *current
    }
}

impl Processor for CompressorProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        let channels = buffer.channels().min(2);
        if frames == 0 || channels == 0 {
            return;
        }

        // Block-rate parameters (D11).
        let threshold = self.params.advance(SLOT_THRESHOLD, frames);
        let ratio = self.params.advance(SLOT_RATIO, frames);
        let attack_ms = self.params.advance(SLOT_ATTACK, frames);
        let release_ms = self.params.advance(SLOT_RELEASE, frames);
        let knee = self.params.advance(SLOT_KNEE, frames);
        let linked = self.params.is_on(SLOT_LINK);

        let settings = Settings {
            threshold,
            ratio,
            knee,
            attack: coeff_for(attack_ms, self.sample_rate),
            release: coeff_for(release_ms, self.sample_rate),
        };

        let mut block_gr = 0.0f32;
        let mut out_peak = [0.0f32; 2];

        for index in 0..frames {
            // Per-sample parameters, shared by both channels: tick once per
            // frame so stereo does not double the ramp.
            let mix = self.params.tick(SLOT_MIX);
            let makeup = self.params.tick(SLOT_MAKEUP);

            if linked {
                let left = buffer
                    .channel(0)
                    .and_then(|c| c.get(index).copied())
                    .unwrap_or(0.0)
                    .abs();
                let right = buffer
                    .channel(1)
                    .and_then(|c| c.get(index).copied())
                    .unwrap_or(0.0)
                    .abs();
                let gr = self.step_gr(0, left.max(right), &settings);
                let factor = mix_factor(gr, makeup, mix);
                block_gr = block_gr.min(gr);
                for ch in 0..channels {
                    let Some(channel) = buffer.channel_mut(ch) else {
                        continue;
                    };
                    let Some(sample) = channel.get_mut(index) else {
                        continue;
                    };
                    let y = *sample * factor;
                    *sample = y;
                    if let Some(peak) = out_peak.get_mut(ch) {
                        *peak = peak.max(y.abs());
                    }
                }
            } else {
                for ch in 0..channels {
                    let Some(channel) = buffer.channel_mut(ch) else {
                        continue;
                    };
                    let Some(sample) = channel.get_mut(index) else {
                        continue;
                    };
                    let x = *sample;
                    let gr = self.step_gr(ch, x.abs(), &settings);
                    let y = x * mix_factor(gr, makeup, mix);
                    *sample = y;
                    block_gr = block_gr.min(gr);
                    if let Some(peak) = out_peak.get_mut(ch) {
                        *peak = peak.max(y.abs());
                    }
                }
            }
        }

        if let Some(meters) = &self.meters {
            meters.store(block_gr, out_peak[0], out_peak[1]);
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::BuiltinParam(slot) => self.params.set(slot, value),
            _ => false,
        }
    }

    fn latency_samples(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;
    use std::sync::Arc;

    use super::{
        CompressorProcessor, DESCRIPTOR, SLOT_ATTACK, SLOT_KNEE, SLOT_LINK, SLOT_MAKEUP, SLOT_MIX,
        SLOT_RATIO, SLOT_RELEASE, SLOT_THRESHOLD, compute_gr_db,
    };
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::effect::meters::EffectMeters;
    use crate::realtime::effect::validate_descriptor;
    use crate::realtime::processor::Processor;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn ctx() -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        }
    }

    /// Feeds a mono signal through in fixed blocks.
    fn run_mono(comp: &mut CompressorProcessor, signal: &[f32]) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(BLOCK) {
            buf.set_len(chunk.len());
            buf.channel_mut(0).unwrap().copy_from_slice(chunk);
            comp.process(&ctx(), &mut buf);
            out.extend_from_slice(buf.channel(0).unwrap());
        }
        out
    }

    /// Feeds a stereo signal (left, right) through in fixed blocks.
    fn run_stereo(
        comp: &mut CompressorProcessor,
        left: &[f32],
        right: &[f32],
    ) -> (Vec<f32>, Vec<f32>) {
        let mut buf = AudioBuffer::new(2, BLOCK);
        let mut out_l = Vec::with_capacity(left.len());
        let mut out_r = Vec::with_capacity(right.len());
        for index in 0..left.len().div_ceil(BLOCK) {
            let start = index * BLOCK;
            let end = (start + BLOCK).min(left.len());
            let len = end - start;
            buf.set_len(len);
            buf.channel_mut(0).unwrap()[..len].copy_from_slice(&left[start..end]);
            buf.channel_mut(1).unwrap()[..len].copy_from_slice(&right[start..end]);
            comp.process(&ctx(), &mut buf);
            out_l.extend_from_slice(&buf.channel(0).unwrap()[..len]);
            out_r.extend_from_slice(&buf.channel(1).unwrap()[..len]);
        }
        (out_l, out_r)
    }

    fn sine(freq: f32, seconds: f32) -> Vec<f32> {
        let n = (SR * seconds) as usize;
        (0..n).map(|i| (TAU * freq * i as f32 / SR).sin()).collect()
    }

    fn set(comp: &mut CompressorProcessor, slot: u8, value: f32) {
        assert!(
            comp.set_param(ParamId::BuiltinParam(slot), value),
            "slot {slot} must be accepted"
        );
    }

    #[test]
    fn descriptor_is_well_formed() {
        assert_eq!(validate_descriptor(&DESCRIPTOR), Ok(()));
        assert!(DESCRIPTOR.is_linkable());
        assert_eq!(DESCRIPTOR.link_slot(), Some(SLOT_LINK));
        assert_eq!(DESCRIPTOR.inline_slots().count(), 3);
    }

    // --- The pure gain computer -------------------------------------------

    #[test]
    fn curve_is_flat_below_the_threshold() {
        assert_eq!(compute_gr_db(-30.0, -18.0, 4.0, 6.0), 0.0);
        assert_eq!(compute_gr_db(-60.0, -18.0, 20.0, 0.0), 0.0);
    }

    #[test]
    fn hard_knee_reduces_by_the_ratio_slope() {
        // 12 dB over a 4:1 threshold → 12 · (1/4 − 1) = −9 dB.
        let gr = compute_gr_db(0.0, -12.0, 4.0, 0.0);
        assert!((gr + 9.0).abs() < 1e-6, "got {gr}");
    }

    #[test]
    fn soft_knee_is_continuous_with_the_hard_knee() {
        let (threshold, ratio, knee) = (-18.0, 4.0, 6.0);
        let slope = 1.0 / ratio - 1.0;
        let at_top = compute_gr_db(threshold + knee / 2.0, threshold, ratio, knee);
        assert!((at_top - slope * knee / 2.0).abs() < 1e-6, "top {at_top}");
        let at_bottom = compute_gr_db(threshold - knee / 2.0, threshold, ratio, knee);
        assert!(at_bottom.abs() < 1e-6, "bottom {at_bottom}");
        // Strictly between the two knee edges the curve is already compressing.
        let middle = compute_gr_db(threshold, threshold, ratio, knee);
        assert!(middle < 0.0 && middle > at_top, "middle {middle}");
    }

    #[test]
    fn ratio_of_one_never_compresses() {
        assert_eq!(compute_gr_db(0.0, -40.0, 1.0, 6.0), 0.0);
    }

    // --- Processor behaviour -----------------------------------------------

    #[test]
    fn silence_in_silence_out() {
        let mut comp = CompressorProcessor::new(SR);
        set(&mut comp, SLOT_THRESHOLD, -60.0);
        set(&mut comp, SLOT_MAKEUP, 24.0);
        let out = run_mono(&mut comp, &vec![0.0; BLOCK * 16]);
        assert!(out.iter().all(|s| s.abs() < 1e-9));
    }

    #[test]
    fn full_scale_stays_finite() {
        let mut comp = CompressorProcessor::new(SR);
        set(&mut comp, SLOT_THRESHOLD, -60.0);
        set(&mut comp, SLOT_RATIO, 20.0);
        set(&mut comp, SLOT_ATTACK, 0.1);
        set(&mut comp, SLOT_MAKEUP, 24.0);
        let out = run_mono(&mut comp, &sine(220.0, 1.0));
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(out.iter().fold(0.0f32, |a, s| a.max(s.abs())) < 32.0);
    }

    /// Below the threshold with no makeup the signal is untouched — the
    /// compressor must not colour what it does not compress.
    #[test]
    fn below_threshold_is_unchanged() {
        let mut comp = CompressorProcessor::new(SR);
        set(&mut comp, SLOT_THRESHOLD, -18.0);
        let quiet = vec![0.03f32; BLOCK * 8];
        let out = run_mono(&mut comp, &quiet);
        for (i, (x, y)) in quiet.iter().zip(out.iter()).enumerate() {
            assert!((x - y).abs() < 1e-9, "sample {i}: {x} → {y}");
        }
        assert!(comp.gain_reduction_db(0).abs() < 1e-6);
    }

    #[test]
    fn steady_input_settles_at_the_expected_reduction() {
        let mut comp = CompressorProcessor::new(SR);
        set(&mut comp, SLOT_THRESHOLD, -18.0);
        set(&mut comp, SLOT_RATIO, 4.0);
        set(&mut comp, SLOT_KNEE, 0.0);
        set(&mut comp, SLOT_ATTACK, 1.0);
        set(&mut comp, SLOT_RELEASE, 50.0);

        // 0.5 → −6.02 dB; 11.98 dB over the threshold → −8.99 dB GR.
        let level = 0.5f32;
        let out = run_mono(&mut comp, &vec![level; BLOCK * 64]);
        let expected_gr = (20.0 * level.log10() + 18.0) * (0.25 - 1.0);
        let tail = out[out.len() - BLOCK];
        let expected = level * 10f32.powf(expected_gr / 20.0);
        assert!(
            (tail / expected - 1.0).abs() < 0.01,
            "settled at {tail}, expected {expected} (gr {expected_gr:.2} dB)"
        );
        assert!(
            (comp.gain_reduction_db(0) - expected_gr).abs() < 0.05,
            "reported GR {} vs {expected_gr}",
            comp.gain_reduction_db(0)
        );
    }

    #[test]
    fn makeup_gain_is_applied_on_top_of_the_reduction() {
        let mut comp = CompressorProcessor::new(SR);
        set(&mut comp, SLOT_THRESHOLD, 0.0); // never compress a 0.03 signal
        set(&mut comp, SLOT_MAKEUP, 6.0);
        let out = run_mono(&mut comp, &vec![0.25f32; BLOCK * 8]);
        let tail = out[out.len() - 1];
        assert!(
            (tail / (0.25 * 1.995_262) - 1.0).abs() < 1e-3,
            "+6 dB makeup should double the level, got {tail}"
        );
    }

    /// `mix = 0` is a sample-exact dry path even while the detector is working.
    #[test]
    fn mix_zero_is_a_sample_exact_passthrough() {
        let mut comp = CompressorProcessor::new(SR);
        set(&mut comp, SLOT_THRESHOLD, -60.0);
        set(&mut comp, SLOT_RATIO, 20.0);
        set(&mut comp, SLOT_MIX, 0.0);
        let input = sine(220.0, 0.2);
        let out = run_mono(&mut comp, &input);
        assert_eq!(out, input, "mix = 0 must return the dry signal bit for bit");
    }

    #[test]
    fn linked_stereo_shares_one_detector() {
        // A loud left channel must pull the quiet right channel down too.
        let left = vec![0.5f32; BLOCK * 64];
        let right = vec![0.01f32; BLOCK * 64];

        let mut linked = CompressorProcessor::new(SR);
        set(&mut linked, SLOT_THRESHOLD, -18.0);
        set(&mut linked, SLOT_RATIO, 4.0);
        set(&mut linked, SLOT_KNEE, 0.0);
        set(&mut linked, SLOT_ATTACK, 1.0);
        set(&mut linked, SLOT_RELEASE, 50.0);
        let (_, right_linked) = run_stereo(&mut linked, &left, &right);

        let mut split = CompressorProcessor::new(SR);
        set(&mut split, SLOT_THRESHOLD, -18.0);
        set(&mut split, SLOT_RATIO, 4.0);
        set(&mut split, SLOT_KNEE, 0.0);
        set(&mut split, SLOT_ATTACK, 1.0);
        set(&mut split, SLOT_RELEASE, 50.0);
        set(&mut split, SLOT_LINK, 0.0);
        let (_, right_split) = run_stereo(&mut split, &left, &right);

        let linked_tail = right_linked[right_linked.len() - 1];
        let split_tail = right_split[right_split.len() - 1];
        assert!(
            (linked_tail / 0.01) < 0.6,
            "linked right channel should be reduced, got {linked_tail}"
        );
        assert!(
            (split_tail / 0.01 - 1.0).abs() < 1e-3,
            "unlinked right channel is below the threshold, got {split_tail}"
        );
        assert!(linked.gain_reduction_db(0) < -8.0);
        assert!(split.gain_reduction_db(1).abs() < 1e-6);
    }

    #[test]
    fn meters_publish_gr_and_peaks() {
        let meters = Arc::new(EffectMeters::new());
        let mut comp = CompressorProcessor::with_meters(SR, Some(meters.clone()));
        set(&mut comp, SLOT_THRESHOLD, -18.0);
        set(&mut comp, SLOT_RATIO, 4.0);
        set(&mut comp, SLOT_KNEE, 0.0);
        run_mono(&mut comp, &vec![0.5f32; BLOCK * 32]);

        let reading = meters.read();
        assert!(reading.gr_db < -5.0, "GR meter {reading:?}");
        assert!(reading.peak_l > 0.0 && reading.peak_l < 0.5, "{reading:?}");
    }

    #[test]
    fn params_are_clamped_and_unknown_ids_rejected() {
        let mut comp = CompressorProcessor::new(SR);
        set(&mut comp, SLOT_THRESHOLD, 12.0);
        assert!(comp.value(SLOT_THRESHOLD).abs() < 1e-6);
        set(&mut comp, SLOT_THRESHOLD, -100.0);
        assert!((comp.value(SLOT_THRESHOLD) + 60.0).abs() < 1e-6);
        set(&mut comp, SLOT_RATIO, 0.0);
        assert!((comp.value(SLOT_RATIO) - 1.0).abs() < 1e-6);
        set(&mut comp, SLOT_ATTACK, f32::NAN);
        assert!((comp.value(SLOT_ATTACK) - 10.0).abs() < 1e-6);
        assert!(!comp.set_param(ParamId::BuiltinParam(42), 1.0));
        assert!(!comp.set_param(ParamId::MasterGain, 1.0));
    }
}
