//! Lookahead limiter (spec `.agents/spec/effects.md` E5).
//!
//! The detector watches a fixed 5 ms sliding window of incoming peaks
//! (monotone deque, O(1) per sample) and computes the gain that keeps the
//! signal under the ceiling; the gain drops instantly and recovers with the
//! release constant. Audio is delayed by the same window and multiplied by
//! the gain, so the limiting starts before the transient it must catch —
//! the delay is reported through `latency_samples` and compensated by the
//! graph's PDC (spec `node-graph.md` §11).
//!
//! Stereo handling follows the linkable convention (D13/D14): both channel
//! detectors always run; linked mode feeds `max(|L|, |R|)` into both so
//! their state stays identical, and switching back to independent mode
//! needs no resync.
//!
//! All state pre-allocated; `process` is allocation-, lock- and panic-free.

use std::sync::Arc;

use crate::realtime::atomic::AtomicF32;
use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::descriptor::{EffectDescriptor, ParamFlags, ParamSpec, StereoClass};
use crate::realtime::effect::{EffectMeters, EffectParamBank};
use crate::realtime::processor::Processor;

/// Slot of the output ceiling (dB).
pub const SLOT_THRESHOLD: u8 = 0;
/// Slot of the gain release time (ms).
pub const SLOT_RELEASE: u8 = 1;
/// Slot of the stereo link switch.
pub const SLOT_LINK: u8 = 2;
/// Slot of the output trim (dB).
pub const SLOT_OUTPUT: u8 = 3;

/// Fixed detection window (ms).
const LOOKAHEAD_MS: f32 = 5.0;
/// Ring capacity: 5 ms at 192 kHz plus margin.
const MAX_LOOKAHEAD: usize = 1024;
/// Monotone-queue capacity: the window never holds more than `MAX_LOOKAHEAD`
/// entries; the two extra slots keep the modular arithmetic away from the
/// bounds even if a caller pushes ahead of the window.
const MAX_QUEUE: usize = MAX_LOOKAHEAD + 2;

static LIMITER_PARAMS: [ParamSpec; 4] = [
    ParamSpec::level("threshold", -24.0, 0.0, 0.0).inline_row(),
    ParamSpec::time("release", 5.0, 1000.0, 120.0).inline_row(),
    ParamSpec::switch_with("link", 1.0, ParamFlags::STEREO_LINK),
    ParamSpec::level("output", -12.0, 12.0, 0.0).inline_row(),
];

/// Static parameter table (spec `.agents/spec/effects.md` E5).
pub static DESCRIPTOR: EffectDescriptor = EffectDescriptor {
    name: "Limiter",
    params: &LIMITER_PARAMS,
    stereo: StereoClass::Linkable { default_link: true },
};

/// Fixed-capacity delay line; `push` returns the sample leaving the line.
struct DelayLine {
    buf: [f32; MAX_LOOKAHEAD],
    pos: usize,
    len: usize,
}

impl DelayLine {
    fn new(len: usize) -> Self {
        Self {
            buf: [0.0; MAX_LOOKAHEAD],
            pos: 0,
            len: len.max(1),
        }
    }

    fn push(&mut self, value: f32) -> f32 {
        let old = self.buf[self.pos];
        self.buf[self.pos] = value;
        self.pos = (self.pos + 1) % self.len;
        old
    }
}

/// Sliding-window maximum over the last `window` pushed peaks, amortized
/// O(1): a monotone deque of decreasing peaks whose indices fall out of the
/// window from the front.
struct SlidingMax {
    peaks: [f32; MAX_QUEUE],
    indices: [u32; MAX_QUEUE],
    head: usize,
    tail: usize,
    window: u32,
}

impl SlidingMax {
    fn new(window: u32) -> Self {
        Self {
            peaks: [0.0; MAX_QUEUE],
            indices: [0; MAX_QUEUE],
            head: 0,
            tail: 0,
            window: window.max(1),
        }
    }

    fn push(&mut self, peak: f32, index: u32) {
        while self.head < self.tail && self.peaks[(self.tail - 1) % MAX_QUEUE] <= peak {
            self.tail -= 1;
        }
        let slot = self.tail % MAX_QUEUE;
        self.peaks[slot] = peak;
        self.indices[slot] = index;
        self.tail += 1;
    }

    /// Drops expired entries and returns the strongest peak in the window.
    fn max(&mut self, cursor: u32) -> f32 {
        while self.head < self.tail && self.indices[self.head % MAX_QUEUE] + self.window <= cursor {
            self.head += 1;
        }
        if self.head < self.tail {
            self.peaks[self.head % MAX_QUEUE]
        } else {
            0.0
        }
    }
}

/// Lookahead limiter (1 in/1 out per channel, fixed 5 ms latency).
pub struct LimiterProcessor {
    sample_rate: f32,
    params: EffectParamBank,
    latency: usize,
    delay: [DelayLine; 2],
    detector: [SlidingMax; 2],
    cursor: u32,
    gain: [f32; 2],
    meters: Option<Arc<EffectMeters>>,
}

impl LimiterProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction tolerance).
    pub fn new(sample_rate: f32) -> Self {
        Self::with_meters(sample_rate, None)
    }

    /// Construction variant that publishes a gain-reduction meter handle.
    pub fn with_meters(sample_rate: f32, meters: Option<Arc<EffectMeters>>) -> Self {
        let sample_rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        let latency =
            ((LOOKAHEAD_MS * 0.001 * sample_rate).round() as usize).clamp(1, MAX_LOOKAHEAD);
        Self {
            sample_rate,
            params: EffectParamBank::new(&DESCRIPTOR, sample_rate),
            latency,
            delay: [DelayLine::new(latency), DelayLine::new(latency)],
            detector: [
                SlidingMax::new(latency as u32),
                SlidingMax::new(latency as u32),
            ],
            cursor: 0,
            gain: [1.0; 2],
            meters,
        }
    }

    /// Current (settled) value of a slot, for tests and read-back.
    pub fn value(&self, slot: u8) -> f32 {
        self.params.value(slot)
    }
}

impl Processor for LimiterProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        let channels = buffer.channels().min(2);
        if frames == 0 || channels == 0 {
            return;
        }

        let threshold = self.params.advance(SLOT_THRESHOLD, frames);
        let ceiling = 10f32.powf(threshold / 20.0);
        let release = self.params.advance(SLOT_RELEASE, frames);
        let release_coeff = 1.0 - (-1.0 / (release.max(0.1) * 0.001 * self.sample_rate)).exp();
        let linked = self.params.advance(SLOT_LINK, frames) >= 0.5;
        let out_gain = 10f32.powf(self.params.advance(SLOT_OUTPUT, frames) / 20.0);

        let mut block_peak = 0.0f32;
        let mut block_gain = 1.0f32;

        for index in 0..frames {
            // Both detectors always run. Linked mode feeds the joined peak
            // into both so their state stays identical (D14); independent
            // mode keeps them per-channel. A mono buffer mirrors its input
            // into the second chain so a later stereo buffer starts clean.
            let left = buffer
                .channel(0)
                .and_then(|channel| channel.get(index).copied())
                .unwrap_or(0.0);
            let right = buffer
                .channel(1)
                .and_then(|channel| channel.get(index).copied())
                .unwrap_or(left);
            let joined = left.abs().max(right.abs());
            let det = [
                if linked { joined } else { left.abs() },
                if linked { joined } else { right.abs() },
            ];

            for ch in 0..2 {
                // Each delay line takes its own channel's input; with a mono
                // buffer `right` already mirrors `left`, so both chains stay
                // fed and no state ever goes stale. Only the live channels
                // are written back.
                let input = if ch == 0 { left } else { right };
                let delayed = self.delay[ch].push(input);
                self.detector[ch].push(det[ch], self.cursor);
                let window_peak = self.detector[ch].max(self.cursor + 1);
                let target = (ceiling / window_peak.max(1e-9)).min(1.0);
                let gain = &mut self.gain[ch];
                if target < *gain {
                    *gain = target;
                } else {
                    *gain += (1.0 - *gain) * release_coeff;
                }
                block_gain = block_gain.min(*gain);
                block_peak = block_peak.max(delayed.abs() * *gain);

                if ch < channels
                    && let Some(sample) = buffer
                        .channel_mut(ch)
                        .and_then(|channel| channel.get_mut(index))
                {
                    *sample = delayed * *gain * out_gain;
                }
            }
            self.cursor = self.cursor.wrapping_add(1);
        }

        if let Some(meters) = &self.meters {
            meters.store(20.0 * block_gain.max(1e-6).log10(), block_peak, block_peak);
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::BuiltinParam(slot) => self.params.set(slot, value),
            _ => false,
        }
    }

    fn latency_samples(&self) -> usize {
        self.latency
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        DESCRIPTOR, LimiterProcessor, SLOT_LINK, SLOT_OUTPUT, SLOT_RELEASE, SLOT_THRESHOLD,
    };
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::effect::validate_descriptor;
    use crate::realtime::processor::Processor;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;
    /// 5 ms at 48 kHz.
    const LATENCY: usize = 240;

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

    fn run_stereo(
        limiter: &mut LimiterProcessor,
        left: &[f32],
        right: &[f32],
    ) -> (Vec<f32>, Vec<f32>) {
        let mut buffer = AudioBuffer::new(2, BLOCK);
        let mut out_l = Vec::with_capacity(left.len());
        let mut out_r = Vec::with_capacity(right.len());
        for i in 0..left.len().div_ceil(BLOCK) {
            let start = i * BLOCK;
            let end = (start + BLOCK).min(left.len());
            let frames = end - start;
            buffer.set_len(frames);
            if let Some(channel) = buffer.channel_mut(0) {
                channel[..frames].copy_from_slice(&left[start..end]);
            }
            if let Some(channel) = buffer.channel_mut(1) {
                channel[..frames].copy_from_slice(&right[start..end]);
            }
            limiter.process(&ctx(), &mut buffer);
            if let Some(channel) = buffer.channel(0) {
                out_l.extend_from_slice(&channel[..frames]);
            }
            if let Some(channel) = buffer.channel(1) {
                out_r.extend_from_slice(&channel[..frames]);
            }
        }
        (out_l, out_r)
    }

    fn set(limiter: &mut LimiterProcessor, slot: u8, value: f32) {
        assert!(
            limiter.set_param(ParamId::BuiltinParam(slot), value),
            "slot {slot} must be accepted"
        );
    }

    #[test]
    fn descriptor_is_well_formed() {
        assert_eq!(validate_descriptor(&DESCRIPTOR), Ok(()));
        assert_eq!(DESCRIPTOR.name, "Limiter");
        assert!(DESCRIPTOR.is_linkable());
        assert_eq!(DESCRIPTOR.link_slot(), Some(SLOT_LINK));
        assert_eq!(DESCRIPTOR.inline_slots().count(), 3);
    }

    #[test]
    fn latency_is_the_lookahead_window() {
        let limiter = LimiterProcessor::new(SR);
        assert_eq!(limiter.latency_samples(), LATENCY);
    }

    #[test]
    fn silence_in_silence_out() {
        let mut limiter = LimiterProcessor::new(SR);
        set(&mut limiter, SLOT_THRESHOLD, -24.0);
        set(&mut limiter, SLOT_OUTPUT, 6.0);
        let silence = vec![0.0; BLOCK * 16];
        let (out_l, out_r) = run_stereo(&mut limiter, &silence, &silence);
        assert!(out_l.iter().all(|s| s.abs() < 1e-9));
        assert!(out_r.iter().all(|s| s.abs() < 1e-9));
    }

    #[test]
    fn full_scale_stays_finite() {
        let mut limiter = LimiterProcessor::new(SR);
        set(&mut limiter, SLOT_THRESHOLD, -24.0);
        set(&mut limiter, SLOT_OUTPUT, 12.0);
        let loud = vec![0.9f32; BLOCK * 32];
        let (out_l, out_r) = run_stereo(&mut limiter, &loud, &loud);
        assert!(out_l.iter().all(|s| s.is_finite()));
        assert!(out_r.iter().all(|s| s.is_finite()));
        let peak = out_l.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        // Ceiling −24 dB + 12 dB trim = −12 dB ≈ 0.251; the gain envelope
        // settles there from the first window on.
        assert!(peak < 0.9, "the limiter must engage, peak {peak}");
    }

    /// With the ceiling at 0 dB the signal never exceeds full scale, so the
    /// gain must stay exactly 1 and the output must be the input delayed by
    /// the reported latency — bit for bit.
    #[test]
    fn below_ceiling_is_a_bit_exact_delay() {
        let mut limiter = LimiterProcessor::new(SR);
        set(&mut limiter, SLOT_THRESHOLD, 0.0);
        set(&mut limiter, SLOT_OUTPUT, 0.0);
        let mut left = vec![0.0f32; BLOCK * 4];
        for (i, s) in left.iter_mut().enumerate() {
            *s = 0.5 * ((i as f32) * 0.05).sin();
        }
        let right = vec![0.1f32; BLOCK * 4];
        let (out_l, out_r) = run_stereo(&mut limiter, &left, &right);
        for (i, s) in out_l.iter().enumerate() {
            let expected = if i >= LATENCY { left[i - LATENCY] } else { 0.0 };
            assert_eq!(*s, expected, "sample {i} must be the delayed input");
        }
        for (i, s) in out_r.iter().enumerate() {
            let expected = if i >= LATENCY {
                right[i - LATENCY]
            } else {
                0.0
            };
            assert_eq!(*s, expected, "sample {i} must be the delayed input");
        }
    }

    /// A steady signal above the ceiling must settle with its peak pinned to
    /// the ceiling (minus the output trim).
    #[test]
    fn steady_overs_settle_at_the_ceiling() {
        let mut limiter = LimiterProcessor::new(SR);
        set(&mut limiter, SLOT_THRESHOLD, -12.0);
        set(&mut limiter, SLOT_OUTPUT, 0.0);
        let loud = vec![0.5f32; BLOCK * 64];
        let (out_l, _) = run_stereo(&mut limiter, &loud, &loud);
        let tail = &out_l[out_l.len() - BLOCK..];
        let peak = tail.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        let ceiling = 10f32.powf(-12.0 / 20.0);
        assert!(
            (peak / ceiling - 1.0).abs() < 0.02,
            "steady over must settle at the ceiling: peak {peak} vs {ceiling}"
        );
    }

    /// Linked mode lets a loud left channel pull the gain down for a quiet
    /// right channel; independent mode leaves the right channel untouched.
    #[test]
    fn stereo_link_pulls_the_quiet_channel_down() {
        let loud = vec![0.8f32; BLOCK * 32];
        let quiet = vec![0.1f32; BLOCK * 32];

        let mut linked = LimiterProcessor::new(SR);
        set(&mut linked, SLOT_THRESHOLD, -12.0);
        set(&mut linked, SLOT_LINK, 1.0);
        let (l, r) = run_stereo(&mut linked, &loud, &quiet);
        let linked_l = l[l.len() - 1];
        let linked_r = r[r.len() - 1];

        let mut independent = LimiterProcessor::new(SR);
        set(&mut independent, SLOT_THRESHOLD, -12.0);
        set(&mut independent, SLOT_LINK, 0.0);
        let (l, r) = run_stereo(&mut independent, &loud, &quiet);
        let solo_l = l[l.len() - 1];
        let solo_r = r[r.len() - 1];

        // Left is compressed in both modes; the quiet right channel is only
        // pulled down when linked.
        assert!(
            solo_r / quiet[0] > 0.99,
            "independent mode must not touch R"
        );
        assert!(
            linked_r / linked_l - (solo_r / solo_l) < -0.05,
            "linked mode must pull R down with L (linked {linked_r} vs solo {solo_r})"
        );
    }

    #[test]
    fn params_are_clamped_and_unknown_ids_rejected() {
        let mut limiter = LimiterProcessor::new(SR);
        set(&mut limiter, SLOT_THRESHOLD, -99.0);
        assert!((limiter.value(SLOT_THRESHOLD) + 24.0).abs() < 1e-6);
        set(&mut limiter, SLOT_RELEASE, f32::NAN);
        assert!((limiter.value(SLOT_RELEASE) - 120.0).abs() < 0.05);
        set(&mut limiter, SLOT_OUTPUT, 99.0);
        assert!((limiter.value(SLOT_OUTPUT) - 12.0).abs() < 1e-6);
        assert!(!limiter.set_param(ParamId::BuiltinParam(42), 1.0));
        assert!(!limiter.set_param(ParamId::MasterGain, 1.0));
    }

    /// The meter handle tracks the block gain reduction.
    #[test]
    fn meters_track_the_gain_reduction() {
        let meters = Arc::new(crate::realtime::EffectMeters::new());
        let mut limiter = LimiterProcessor::with_meters(SR, Some(meters.clone()));
        set(&mut limiter, SLOT_THRESHOLD, -12.0);
        let loud = vec![0.5f32; BLOCK * 8];
        let _ = run_stereo(&mut limiter, &loud, &loud);
        let reading = meters.read();
        assert!(
            reading.gr_db < -5.0,
            "the limiter must report meaningful gain reduction, got {}",
            reading.gr_db
        );
    }
}
