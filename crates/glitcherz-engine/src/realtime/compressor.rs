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
/// Slot of the sidechain switch (sidechain variant only, spec Z1).
pub const SLOT_SIDECHAIN: u8 = 8;

/// The eight parameters every compressor shares (E2b order).
const BASE_PARAMS: [ParamSpec; 8] = [
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

/// The shared parameters (E2b order) plus the keying switch (Z1 merge).
const fn compressor_params() -> [ParamSpec; 9] {
    let mut params = [BASE_PARAMS[0]; 9];
    let mut index = 0;
    while index < BASE_PARAMS.len() {
        params[index] = BASE_PARAMS[index];
        index += 1;
    }
    params[SLOT_SIDECHAIN as usize] = ParamSpec::switch("sidechain", 0.0);
    params
}

static COMPRESSOR_PARAMS: [ParamSpec; 9] = compressor_params();

/// Static parameter table (spec `.agents/spec/effects.md` E2b; Z1 merge adds
/// the keying switch at slot 8, default off = internal detection).
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
        Self::build(sample_rate, &DESCRIPTOR, None)
    }

    /// Same, with a meter handle the control thread keeps a clone of.
    /// Construction time only.
    pub fn with_meters(sample_rate: f32, meters: Option<Arc<EffectMeters>>) -> Self {
        Self::build(sample_rate, &DESCRIPTOR, meters)
    }

    /// Shared constructor; `descriptor` decides the parameter table.
    fn build(
        sample_rate: f32,
        descriptor: &'static EffectDescriptor,
        meters: Option<Arc<EffectMeters>>,
    ) -> Self {
        let sr = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            sample_rate: sr,
            params: EffectParamBank::new(descriptor, sr),
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

    /// Block loop shared by both variants. `key` is the external detector
    /// source (the Mod port); `None` keeps the internal detector.
    ///
    /// A silent key means "no compression", never a fallback to the internal
    /// detector (spec Z1-D3): the whole point of keying is that the key signal
    /// decides, so a gap in the key must release instead of squeezing the
    /// programme material.
    fn run(&mut self, buffer: &mut AudioBuffer, key: Option<&AudioBuffer>) {
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
        // Z1 merge: the switch lives at slot 8, default off = internal
        // detection. A wired key is ignored until it is armed.
        let key = key.filter(|_| self.params.is_on(SLOT_SIDECHAIN));

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

            if let Some(key) = key {
                // External keying: one detector drives both channels (Z1-D4).
                // `stereo_peak` reads before `apply_gain` writes.
                let gr = self.step_gr(0, stereo_peak(key, index), &settings);
                block_gr = block_gr.min(gr);
                let factor = mix_factor(gr, makeup, mix);
                apply_gain(buffer, channels, index, factor, &mut out_peak);
            } else if linked {
                let gr = self.step_gr(0, stereo_peak(buffer, index), &settings);
                block_gr = block_gr.min(gr);
                let factor = mix_factor(gr, makeup, mix);
                apply_gain(buffer, channels, index, factor, &mut out_peak);
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
}

/// `max(|L|, |R|)` at `index`; missing channels or frames read as `0`.
fn stereo_peak(buffer: &AudioBuffer, index: usize) -> f32 {
    let peak = |ch| {
        buffer
            .channel(ch)
            .and_then(|channel| channel.get(index).copied())
            .unwrap_or(0.0)
            .abs()
    };
    peak(0).max(peak(1))
}

/// Applies one gain factor to every output channel at `index`.
fn apply_gain(
    buffer: &mut AudioBuffer,
    channels: usize,
    index: usize,
    factor: f32,
    out_peak: &mut [f32; 2],
) {
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
}

impl Processor for CompressorProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        self.run(buffer, None);
    }

    fn process_with_aux(
        &mut self,
        _ctx: &ProcessContext,
        buffer: &mut AudioBuffer,
        aux: Option<&AudioBuffer>,
    ) {
        self.run(buffer, aux);
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
        SLOT_RATIO, SLOT_RELEASE, SLOT_SIDECHAIN, SLOT_THRESHOLD, compute_gr_db,
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

    // --- Sidechain variant (spec Z1) ----------------------------------------

    /// Feeds `main` through with `key` on the Mod port.
    fn run_keyed(comp: &mut CompressorProcessor, main: &[f32], key: &[f32]) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, BLOCK);
        let mut aux = AudioBuffer::new(2, BLOCK);
        let mut out = Vec::with_capacity(main.len());
        for (index, chunk) in main.chunks(BLOCK).enumerate() {
            let start = index * BLOCK;
            let len = chunk.len();
            buf.set_len(len);
            buf.channel_mut(0).unwrap()[..len].copy_from_slice(chunk);
            aux.set_len(len);
            let end = start + len;
            for ch in 0..2 {
                aux.channel_mut(ch).unwrap()[..len].copy_from_slice(&key[start..end]);
            }
            comp.process_with_aux(&ctx(), &mut buf, Some(&aux));
            out.extend_from_slice(&buf.channel(0).unwrap()[..len]);
        }
        out
    }

    /// E2b settings, hard knee so the expected GR is arithmetic.
    fn keyed_comp() -> CompressorProcessor {
        let mut comp = CompressorProcessor::new(SR);
        // The switch is off by default (no inert node waiting for a cable),
        // so every keying test has to arm it explicitly.
        set(&mut comp, SLOT_SIDECHAIN, 1.0);
        set(&mut comp, SLOT_THRESHOLD, -18.0);
        set(&mut comp, SLOT_RATIO, 4.0);
        set(&mut comp, SLOT_KNEE, 0.0);
        set(&mut comp, SLOT_ATTACK, 1.0);
        set(&mut comp, SLOT_RELEASE, 50.0);
        comp
    }

    #[test]
    fn keying_switch_is_the_ninth_slot() {
        assert_eq!(DESCRIPTOR.params.len(), 9);
        assert!(DESCRIPTOR.is_linkable());
        // The link switch stays at slot 7; the key switch is appended.
        assert_eq!(DESCRIPTOR.link_slot(), Some(SLOT_LINK));
        assert_eq!(
            DESCRIPTOR.spec(SLOT_SIDECHAIN).map(|spec| spec.name),
            Some("sidechain")
        );
        assert!(DESCRIPTOR.spec(9).is_none());

        // Fresh nodes key from their own input until the switch is armed.
        let mut fresh = CompressorProcessor::new(SR);
        assert_eq!(fresh.value(SLOT_SIDECHAIN), 0.0, "keying starts off");
        let main = vec![0.5f32; BLOCK * 16];
        let key = vec![0.9f32; BLOCK * 16];
        let keyed_off = run_keyed(&mut fresh, &main, &key);
        let mut reference = CompressorProcessor::new(SR);
        let plain = run_mono(&mut reference, &main);
        assert_eq!(keyed_off, plain, "an unarmed key must be ignored");
    }

    /// Z1 merge: slot 8 exists on every compressor, and a switch that is off
    /// makes a wired key inert — the aux port is ignored entirely.
    #[test]
    fn switch_off_ignores_the_key_input() {
        let mut comp = CompressorProcessor::new(SR);
        assert!(comp.set_param(ParamId::BuiltinParam(SLOT_SIDECHAIN), 1.0));
        assert!(comp.set_param(ParamId::BuiltinParam(SLOT_SIDECHAIN), 0.0));
        assert_eq!(comp.value(SLOT_SIDECHAIN), 0.0);

        // Handing it a loud aux buffer must change nothing.
        let main = vec![0.5f32; BLOCK * 8];
        let key = vec![0.5f32; BLOCK * 8];
        let mut with_key = CompressorProcessor::new(SR);
        set(&mut with_key, SLOT_THRESHOLD, -18.0);
        set(&mut with_key, SLOT_RATIO, 4.0);
        set(&mut with_key, SLOT_KNEE, 0.0);
        set(&mut with_key, SLOT_ATTACK, 1.0);
        set(&mut with_key, SLOT_RELEASE, 50.0);
        let with_aux = run_keyed(&mut with_key, &main, &key);

        let mut reference = CompressorProcessor::new(SR);
        set(&mut reference, SLOT_THRESHOLD, -18.0);
        set(&mut reference, SLOT_RATIO, 4.0);
        set(&mut reference, SLOT_KNEE, 0.0);
        set(&mut reference, SLOT_ATTACK, 1.0);
        set(&mut reference, SLOT_RELEASE, 50.0);
        let internal = run_mono(&mut reference, &main);

        assert_eq!(
            with_aux, internal,
            "an unarmed key must be ignored entirely"
        );
    }

    /// The defining assertion of external keying: the reduction is decided by
    /// the key level, and a silent key releases instead of compressing.
    #[test]
    fn sidechain_reduces_only_while_the_key_is_loud() {
        let main = vec![0.5f32; BLOCK * 80];
        // Quiet half, then a loud half.
        let mut key = vec![0.0f32; BLOCK * 80];
        key[BLOCK * 40..].fill(0.5);

        let mut comp = keyed_comp();
        let out = run_keyed(&mut comp, &main, &key);

        // Key silent → no reduction, programme passes untouched.
        let quiet_tail = out[BLOCK * 39];
        assert!(
            (quiet_tail - 0.5).abs() < 1e-6,
            "a silent key must not compress, got {quiet_tail}"
        );

        // Key at 0.5 → 11.98 dB over a 4:1 threshold → −8.98 dB.
        let expected_gr = (20.0 * 0.5f32.log10() + 18.0) * (0.25 - 1.0);
        let expected = 0.5 * 10f32.powf(expected_gr / 20.0);
        let loud_tail = out[out.len() - 1];
        assert!(
            (loud_tail / expected - 1.0).abs() < 0.01,
            "keyed output {loud_tail}, expected {expected} (gr {expected_gr:.2} dB)"
        );
        assert!(
            (comp.gain_reduction_db(0) - expected_gr).abs() < 0.05,
            "reported GR {}",
            comp.gain_reduction_db(0)
        );
    }

    /// Z1-D3: no fallback to the internal detector. The programme is well over
    /// the threshold, so a fallback would show up as a big reduction.
    #[test]
    fn silent_key_never_compresses_the_programme() {
        let main = vec![0.5f32; BLOCK * 32];
        let key = vec![0.0f32; BLOCK * 32];
        let mut comp = keyed_comp();
        let out = run_keyed(&mut comp, &main, &key);
        for (index, (x, y)) in main.iter().zip(out.iter()).enumerate() {
            assert!((x - y).abs() < 1e-7, "sample {index}: {x} → {y}");
        }
        assert!(comp.gain_reduction_db(0).abs() < 1e-6);
    }

    /// Same key, different programme levels → same reduction.
    #[test]
    fn sidechain_reduction_ignores_the_programme_level() {
        let key = vec![0.5f32; BLOCK * 64];
        let mut loud_comp = keyed_comp();
        let mut quiet_comp = keyed_comp();
        let loud = vec![0.5f32; BLOCK * 64];
        let quiet = vec![0.2f32; BLOCK * 64];

        run_keyed(&mut loud_comp, &loud, &key);
        run_keyed(&mut quiet_comp, &quiet, &key);

        let a = loud_comp.gain_reduction_db(0);
        let b = quiet_comp.gain_reduction_db(0);
        assert!((a - b).abs() < 1e-6, "GR must follow the key: {a} vs {b}");
        assert!(a < -8.0, "expected a strong reduction, got {a}");
    }

    /// `sidechain = 0` is a switch back to the internal detector, sample for
    /// sample identical to the plain compressor.
    #[test]
    fn sidechain_switch_off_matches_internal_detection() {
        let main = sine(220.0, 0.3);
        let key = vec![0.9f32; main.len()];

        let mut keyed = keyed_comp();
        set(&mut keyed, SLOT_SIDECHAIN, 0.0);
        let keyed_out = run_keyed(&mut keyed, &main, &key);

        let mut plain = CompressorProcessor::new(SR);
        set(&mut plain, SLOT_THRESHOLD, -18.0);
        set(&mut plain, SLOT_RATIO, 4.0);
        set(&mut plain, SLOT_KNEE, 0.0);
        set(&mut plain, SLOT_ATTACK, 1.0);
        set(&mut plain, SLOT_RELEASE, 50.0);
        let plain_out = run_mono(&mut plain, &main);

        assert_eq!(
            keyed_out, plain_out,
            "sidechain off must be the internal detector bit for bit"
        );
    }

    /// The key is a control signal: it must not leak into the output.
    #[test]
    fn key_signal_does_not_leak_into_the_output() {
        // Programme is silence, key is loud: the output must stay silent.
        let main = vec![0.0f32; BLOCK * 16];
        let key = vec![0.9f32; BLOCK * 16];
        let mut comp = keyed_comp();
        set(&mut comp, SLOT_MAKEUP, 0.0);
        let out = run_keyed(&mut comp, &main, &key);
        assert!(
            out.iter().all(|s| s.abs() < 1e-9),
            "key leaked into the programme path"
        );
        // ... while the detector did react to it.
        assert!(comp.gain_reduction_db(0) < -8.0);
    }
}
