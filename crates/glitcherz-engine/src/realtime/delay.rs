//! Feedback delay effect (spec `.agents/spec/effects.md` E1/E4).
//!
//! Classic feedback delay: `wet = ring[read]`, write `input + wet * feedback`,
//! output `dry * (1 - mix) + wet * mix`. Feedback is clamped to <= 0.9 so the
//! decay is bounded; denormals are flushed on write. The delay time is an
//! integer number of samples (no interpolation — that is what the modulation
//! effects in [`crate::realtime::mod_delay`] are for).
//!
//! E4 adds two stereo behaviours:
//! - **ping-pong**: the two delay lines are cross-fed (`write_l = in_r +
//!   fb·wet_r`, `write_r = in_l + fb·wet_l`), so a left impulse echoes out of
//!   the right channel first and then alternates;
//! - **tempo-sync**: with `sync` on, the delay time is a note division of the
//!   block's tempo (`ticks · ctx.sec_per_tick`) instead of free seconds.
//!
//! Parameters live in an [`EffectParamBank`] (D5/D7): `set_param` accepts the
//! legacy [`ParamId::Delay*`](ParamId) variants (the fixed track trio) and the
//! slot-addressed [`ParamId::BuiltinParam`], both landing in the same smoothed
//! slots. All paths are RT-safe.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::EffectParamBank;
use crate::realtime::effect::descriptor::{EffectDescriptor, ParamSpec, StereoClass, Unit};
use crate::realtime::processor::Processor;
use crate::realtime::sequencer::TICKS_PER_QUARTER;

/// Maximum free-mode delay time (seconds) — the `time` slider's upper bound.
pub const MAX_DELAY_S: f32 = 1.0;

/// Ring-buffer capacity (seconds): large enough for the slowest synced
/// division at a slow tempo, so sync never clips its tail.
const MAX_BUFFER_S: f32 = 4.0;

/// Slot of the delay time parameter (seconds, free mode).
pub const SLOT_TIME: u8 = 0;
/// Slot of the feedback amount.
pub const SLOT_FEEDBACK: u8 = 1;
/// Slot of the wet mix ratio.
pub const SLOT_MIX: u8 = 2;
/// Slot of the ping-pong switch.
pub const SLOT_PING_PONG: u8 = 3;
/// Slot of the tempo-sync switch.
pub const SLOT_SYNC: u8 = 4;
/// Slot of the synced note division (index into [`DIVISION_TICKS`]).
pub const SLOT_DIVISION: u8 = 5;

/// Synced note divisions in ticks. **Order is the UI combo order and must stay
/// parallel to [`DIVISION_LABELS`] and the model's `sync_division` index.**
pub const DIVISION_TICKS: [u32; 8] = [
    TICKS_PER_QUARTER * 4,     // 1/1
    TICKS_PER_QUARTER * 2,     // 1/2
    TICKS_PER_QUARTER,         // 1/4
    TICKS_PER_QUARTER / 2,     // 1/8
    TICKS_PER_QUARTER / 4,     // 1/16
    TICKS_PER_QUARTER / 3,     // 1/8 triplet
    TICKS_PER_QUARTER / 6,     // 1/16 triplet
    TICKS_PER_QUARTER * 3 / 4, // 1/8 dotted
];

/// English labels for the sync combo (parallel to [`DIVISION_TICKS`]).
pub const DIVISION_LABELS: &[&str] = &["1/1", "1/2", "1/4", "1/8", "1/16", "1/8T", "1/16T", "1/8D"];

static DELAY_PARAMS: [ParamSpec; 6] = [
    // `time_s` is stored in seconds (0.005..1.0), so the spec must say seconds
    // — `ParamSpec::time` would tag it as milliseconds and the slider would
    // read "0.2 ms" for a 250 ms tap.
    ParamSpec::seconds("time", 0.005, MAX_DELAY_S, 0.3).inline_row(),
    ParamSpec::gain("feedback", Unit::Percent, 0.0, 0.9, 0.35)
        .short("fdbk")
        .inline_row(),
    ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 0.0).inline_row(),
    ParamSpec::switch("ping_pong", 0.0),
    ParamSpec::switch("sync", 0.0),
    ParamSpec::enumeration("division", DIVISION_LABELS, 2),
];

/// Static parameter table (spec `.agents/spec/effects.md`).
pub static DESCRIPTOR: EffectDescriptor = EffectDescriptor {
    name: "Delay",
    params: &DELAY_PARAMS,
    stereo: StereoClass::Mono,
};

/// Feedback delay (one pre-allocated ring buffer per channel, L/R).
pub struct DelayProcessor {
    sample_rate: f32,
    rings: [Box<[f32]>; 2],
    /// Per-channel write pointer (channels are processed in order; sharing a
    /// pointer would desynchronize the two rings).
    write_pos: [usize; 2],
    params: EffectParamBank,
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
        let capacity = (MAX_BUFFER_S * sr) as usize + 1;
        Self {
            sample_rate: sr,
            rings: [
                vec![0.0; capacity].into_boxed_slice(),
                vec![0.0; capacity].into_boxed_slice(),
            ],
            write_pos: [0; 2],
            params: EffectParamBank::new(&DESCRIPTOR, sr),
        }
    }

    /// Current (settled) wet mix ratio. RT-safe.
    pub fn mix(&self) -> f32 {
        self.params.value(SLOT_MIX)
    }

    /// Current (settled) free-mode delay length in seconds. RT-safe.
    pub fn time_s(&self) -> f32 {
        self.params.value(SLOT_TIME)
    }

    /// Current (settled) feedback amount. RT-safe.
    pub fn feedback(&self) -> f32 {
        self.params.value(SLOT_FEEDBACK)
    }

    /// Reads one channel's ring at the given integer delay. RT-safe.
    fn read(&self, channel: usize, delay: usize) -> f32 {
        let Some(ring) = self.rings.get(channel) else {
            return 0.0;
        };
        let capacity = ring.len();
        if capacity == 0 {
            return 0.0;
        }
        let write = self.write_pos.get(channel).copied().unwrap_or(0);
        let read_pos = (write + capacity - delay.min(capacity - 1)) % capacity;
        ring.get(read_pos).copied().unwrap_or(0.0)
    }

    /// Writes one channel's ring and advances its cursor. RT-safe.
    fn push(&mut self, channel: usize, value: f32) {
        let Some(ring) = self.rings.get_mut(channel) else {
            return;
        };
        let capacity = ring.len();
        if capacity == 0 {
            return;
        }
        let write = self.write_pos.get(channel).copied().unwrap_or(0);
        if let Some(slot) = ring.get_mut(write) {
            // Flush denormals so a silent tail does not burn CPU.
            *slot = if value.abs() < 1e-18 { 0.0 } else { value };
        }
        if let Some(cursor) = self.write_pos.get_mut(channel) {
            *cursor = (write + 1) % capacity;
        }
    }
}

/// Delay length in integer samples, clamped to the supported window.
///
/// Rounded (not truncated): the log-domain smoother round-trips the time
/// through `ln`/`exp`, so a truncated product can land one sample short
/// (0.05 s → 2399.99… samples).
fn delay_samples(time_s: f32, sample_rate: f32) -> usize {
    let t = if time_s.is_finite() {
        time_s.clamp(0.005, MAX_BUFFER_S)
    } else {
        0.3
    };
    (t * sample_rate).round() as usize
}

/// Ticks of a synced division by index (out-of-range falls back to 1/4).
fn division_ticks(index: f32) -> u32 {
    if !index.is_finite() {
        return TICKS_PER_QUARTER;
    }
    let index = index.round().max(0.0) as usize;
    DIVISION_TICKS
        .get(index)
        .copied()
        .unwrap_or(TICKS_PER_QUARTER)
}

impl Processor for DelayProcessor {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        let channels = buffer.channels().min(2);
        if frames == 0 || channels == 0 {
            return;
        }
        let capacity = self.rings[0].len();

        // Block-rate parameter (D11): one smoothed step per block.
        let sync = self.params.is_on(SLOT_SYNC);
        let delay_s = if sync {
            let division = self.params.advance(SLOT_DIVISION, frames);
            (division_ticks(division) as f64 * ctx.sec_per_tick) as f32
        } else {
            self.params.advance(SLOT_TIME, frames)
        };
        let delay = delay_samples(delay_s, self.sample_rate).min(capacity - 1);
        let ping_pong = self.params.is_on(SLOT_PING_PONG);

        for index in 0..frames {
            let mix = self.params.tick(SLOT_MIX).clamp(0.0, 1.0);
            let feedback = self.params.tick(SLOT_FEEDBACK).clamp(0.0, 0.9);

            if channels > 1 {
                let in_l = buffer
                    .channel(0)
                    .and_then(|channel| channel.get(index).copied())
                    .unwrap_or(0.0);
                let in_r = buffer
                    .channel(1)
                    .and_then(|channel| channel.get(index).copied())
                    .unwrap_or(0.0);
                let wet_l = self.read(0, delay);
                let wet_r = self.read(1, delay);
                if ping_pong {
                    // Crossed input and feedback: a left impulse leaves the
                    // right channel first, then alternates L→R→L… .
                    self.push(0, in_r + feedback * wet_r);
                    self.push(1, in_l + feedback * wet_l);
                } else {
                    self.push(0, in_l + feedback * wet_l);
                    self.push(1, in_r + feedback * wet_r);
                }
                if let Some(sample) = buffer
                    .channel_mut(0)
                    .and_then(|channel| channel.get_mut(index))
                {
                    *sample = in_l * (1.0 - mix) + wet_l * mix;
                }
                if let Some(sample) = buffer
                    .channel_mut(1)
                    .and_then(|channel| channel.get_mut(index))
                {
                    *sample = in_r * (1.0 - mix) + wet_r * mix;
                }
            } else {
                let input = buffer
                    .channel(0)
                    .and_then(|channel| channel.get(index).copied())
                    .unwrap_or(0.0);
                let wet = self.read(0, delay);
                self.push(0, input + feedback * wet);
                if let Some(sample) = buffer
                    .channel_mut(0)
                    .and_then(|channel| channel.get_mut(index))
                {
                    *sample = input * (1.0 - mix) + wet * mix;
                }
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        let slot = match id {
            ParamId::DelayTime => SLOT_TIME,
            ParamId::DelayFeedback => SLOT_FEEDBACK,
            ParamId::DelayMix => SLOT_MIX,
            ParamId::BuiltinParam(slot) => slot,
            _ => return false,
        };
        self.params.set(slot, value)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DESCRIPTOR, DelayProcessor, MAX_DELAY_S, SLOT_FEEDBACK, SLOT_MIX, SLOT_PING_PONG,
        SLOT_SYNC, SLOT_TIME,
    };
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;
    /// Seconds per tick at 120 BPM (`TICKS_PER_QUARTER = 960`).
    const SEC_PER_TICK_120: f64 = 60.0 / (120.0 * 960.0);

    fn ctx(frames: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: SEC_PER_TICK_120,
        }
    }

    fn ctx_at(frames: usize, sec_per_tick: f64) -> ProcessContext {
        ProcessContext {
            sec_per_tick,
            ..ctx(frames)
        }
    }

    /// Pushes `signal` through the effect in blocks, returns channel-0 output.
    fn run_through(delay: &mut DelayProcessor, signal: &[f32]) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(BLOCK) {
            buf.set_len(chunk.len());
            buf.channel_mut(0).unwrap().copy_from_slice(chunk);
            delay.process(&ctx(chunk.len()), &mut buf);
            out.extend_from_slice(buf.channel(0).unwrap());
        }
        out
    }

    /// Renders a mono impulse at a chosen tempo (for the sync tests).
    fn run_impulse_at(delay: &mut DelayProcessor, frames: usize, sec_per_tick: f64) -> Vec<f32> {
        let mut signal = vec![0.0f32; frames];
        signal[0] = 1.0;
        let mut buf = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(frames);
        for chunk in signal.chunks(BLOCK) {
            buf.set_len(chunk.len());
            buf.channel_mut(0).unwrap().copy_from_slice(chunk);
            delay.process(&ctx_at(chunk.len(), sec_per_tick), &mut buf);
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
    fn channels_delay_independently_without_ping_pong() {
        let mut delay = DelayProcessor::new(SR);
        delay.set_param(ParamId::DelayMix, 1.0);
        delay.set_param(ParamId::DelayFeedback, 0.0);
        delay.set_param(ParamId::DelayTime, 0.05); // 2_400 samples

        // Feed an impulse to the left channel only; keep the right silent.
        let mut buf = AudioBuffer::new(2, BLOCK);
        buf.set_len(BLOCK);
        buf.channel_mut(0).unwrap()[0] = 1.0;
        delay.process(&ctx(BLOCK), &mut buf);

        // Render blocks past the delay point (2400 samples); the right channel
        // must stay silent throughout.
        let mut silence = AudioBuffer::new(2, BLOCK);
        let mut left_at_delay = 0.0f32;
        for block in 0..20 {
            silence.clear();
            silence.set_len(BLOCK);
            delay.process(&ctx(BLOCK), &mut silence);
            if block == 17 {
                // This block covers [2304, 2432), so the delay point 2400 sits
                // at offset 96 within the block.
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
    fn ping_pong_starts_on_the_opposite_channel() {
        let mut delay = DelayProcessor::new(SR);
        delay.set_param(ParamId::BuiltinParam(SLOT_MIX), 1.0);
        delay.set_param(ParamId::BuiltinParam(SLOT_FEEDBACK), 0.5);
        delay.set_param(ParamId::BuiltinParam(SLOT_TIME), 0.05); // 2_400 samples
        delay.set_param(ParamId::BuiltinParam(SLOT_PING_PONG), 1.0);

        // Left-only impulse.
        let mut buf = AudioBuffer::new(2, BLOCK);
        buf.set_len(BLOCK);
        buf.channel_mut(0).unwrap()[0] = 1.0;
        delay.process(&ctx(BLOCK), &mut buf);

        let mut taps = [0.0f32; 3];
        let mut buf = AudioBuffer::new(2, BLOCK);
        for block in 0..60 {
            buf.clear();
            buf.set_len(BLOCK);
            delay.process(&ctx(BLOCK), &mut buf);
            // Echoes land at 2400 (block 17, offset 96), 4800 (block 36, offset
            // 64) and 7200 (block 55, offset 32): the impulse block is block 0,
            // so later block k covers samples [128·(k+1), 128·(k+2)).
            let offsets = [(17, 96), (36, 64), (55, 32)];
            for (tap, (blk, off)) in offsets.iter().enumerate() {
                if block == *blk {
                    let left = buf.channel(0).unwrap()[*off];
                    let right = buf.channel(1).unwrap()[*off];
                    taps[tap] = if tap % 2 == 0 { right } else { left };
                }
            }
        }
        assert!(
            taps[0].abs() > 0.9,
            "first echo must be on the right: {taps:?}"
        );
        assert!(
            (taps[1].abs() - 0.5).abs() < 1e-3,
            "second echo (left) must be feedback-weighted: {taps:?}"
        );
        assert!(
            (taps[2].abs() - 0.25).abs() < 1e-3,
            "third echo (right) must be feedback²: {taps:?}"
        );
    }

    #[test]
    fn sync_mode_follows_the_tempo() {
        // 1/4 note (default division) at 120 BPM = 0.5 s = 24_000 samples.
        let mut delay = DelayProcessor::new(SR);
        delay.set_param(ParamId::BuiltinParam(SLOT_MIX), 1.0);
        delay.set_param(ParamId::BuiltinParam(SLOT_FEEDBACK), 0.0);
        delay.set_param(ParamId::BuiltinParam(SLOT_SYNC), 1.0);
        let out = run_impulse_at(&mut delay, 40_000, SEC_PER_TICK_120);
        assert!(
            (out[24_000] - 1.0).abs() < 1e-6,
            "1/4 at 120 BPM must echo at 0.5 s"
        );

        // The same division at 60 BPM = 1.0 s = 48_000 samples.
        let mut slow = DelayProcessor::new(SR);
        slow.set_param(ParamId::BuiltinParam(SLOT_MIX), 1.0);
        slow.set_param(ParamId::BuiltinParam(SLOT_FEEDBACK), 0.0);
        slow.set_param(ParamId::BuiltinParam(SLOT_SYNC), 1.0);
        let sec_per_tick_60 = 60.0 / (60.0 * 960.0);
        let out = run_impulse_at(&mut slow, 60_000, sec_per_tick_60);
        assert!(
            (out[48_000] - 1.0).abs() < 1e-6,
            "1/4 at 60 BPM must echo at 1.0 s"
        );
    }

    #[test]
    fn sync_off_uses_the_free_time() {
        let mut delay = DelayProcessor::new(SR);
        delay.set_param(ParamId::BuiltinParam(SLOT_MIX), 1.0);
        delay.set_param(ParamId::BuiltinParam(SLOT_FEEDBACK), 0.0);
        delay.set_param(ParamId::BuiltinParam(SLOT_TIME), 0.1); // 4_800 samples
        let out = run_impulse_at(&mut delay, 20_000, SEC_PER_TICK_120);
        assert!(
            (out[4_800] - 1.0).abs() < 1e-6,
            "sync off must use the free time, not the tempo"
        );
    }

    #[test]
    fn params_are_clamped() {
        let mut delay = DelayProcessor::new(SR);
        assert!(delay.set_param(ParamId::DelayTime, 10.0));
        assert!((delay.time_s() - MAX_DELAY_S).abs() < 1e-6);
        delay.set_param(ParamId::DelayFeedback, 5.0);
        assert!((delay.feedback() - 0.9).abs() < 1e-6);
        delay.set_param(ParamId::DelayMix, -1.0);
        assert!((delay.mix() - 0.0).abs() < 1e-6);
        assert!(
            !delay.set_param(ParamId::MasterGain, 1.0),
            "unhandled params must return false"
        );
    }

    #[test]
    fn slot_params_match_legacy_variants() {
        let mut legacy = DelayProcessor::new(SR);
        let mut slotted = DelayProcessor::new(SR);
        assert!(legacy.set_param(ParamId::DelayTime, 0.2));
        assert!(slotted.set_param(ParamId::BuiltinParam(SLOT_TIME), 0.2));
        assert!((legacy.time_s() - slotted.time_s()).abs() < 1e-9);
        assert!(slotted.set_param(ParamId::BuiltinParam(SLOT_FEEDBACK), 0.6));
        assert!((slotted.feedback() - 0.6).abs() < 1e-9);
        assert!(slotted.set_param(ParamId::BuiltinParam(SLOT_MIX), 0.4));
        assert!((slotted.mix() - 0.4).abs() < 1e-9);
        assert!(
            !slotted.set_param(ParamId::BuiltinParam(9), 1.0),
            "slot past the descriptor must be rejected"
        );
    }

    #[test]
    fn descriptor_is_well_formed() {
        assert_eq!(
            crate::realtime::effect::validate_descriptor(&DESCRIPTOR),
            Ok(())
        );
        assert_eq!(DESCRIPTOR.name, "Delay");
        assert_eq!(DESCRIPTOR.params.len(), 6);
        assert_eq!(DESCRIPTOR.inline_slots().count(), 3);
        // The division enum advertises the same count as the tick table.
        let division = DESCRIPTOR.spec(super::SLOT_DIVISION).unwrap();
        assert_eq!(division.choices.len(), super::DIVISION_TICKS.len());
    }

    #[test]
    fn runtime_mix_change_ramps_instead_of_jumping() {
        let mut delay = DelayProcessor::new(SR);
        let mut buf = AudioBuffer::new(1, BLOCK);
        buf.set_len(BLOCK);
        buf.channel_mut(0).unwrap().fill(1.0);
        delay.process(&ctx(BLOCK), &mut buf); // mark the smoothers started

        delay.set_param(ParamId::DelayMix, 1.0);
        buf.set_len(BLOCK);
        buf.channel_mut(0).unwrap().fill(1.0);
        delay.process(&ctx(BLOCK), &mut buf);
        let first = buf.channel(0).unwrap()[0];
        assert!(
            first > 0.9 && first < 1.0,
            "a runtime mix change must ramp, first sample was {first}"
        );
    }
}
