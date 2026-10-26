//! ADSR volume envelope.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;

/// Envelope stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Silent (no note), outputs 0.
    Idle,
    /// Rises from the current level to the peak.
    Attack,
    /// Falls from the peak to the sustain level.
    Decay,
    /// Holds the sustain level.
    Sustain,
    /// Decays from the current level back to 0.
    Release,
}

/// ADSR volume envelope — an in-place multiplicative effect.
///
/// Parameters (all updated at block boundaries via [`Processor::set_param`]):
/// Attack / Decay / Release are in seconds (0..10), Sustain is a relative
/// level in 0..1.
/// Peak = velocity (0..1, delivered with `NoteOn`).
///
/// Implementation details: each stage uses a **linear slope** — Attack reaches
/// the peak exactly at the end of A seconds, Decay reaches sustain exactly at
/// the end of D seconds, and Release starts from the level at entry and reaches
/// zero exactly at the end of R seconds; stages with a zero time parameter are
/// skipped within the same sample. All paths
/// RT-safe: no allocation, no locks, no panics.
#[derive(Debug)]
pub struct AdsrProcessor {
    sample_rate: f32,
    attack_s: f32,
    decay_s: f32,
    sustain: f32,
    release_s: f32,
    velocity: f32,
    stage: Stage,
    /// Current envelope level, always within `[0, velocity]`.
    level: f32,
    /// Per-sample decay amount on entering Release (guarantees reaching exactly zero within R seconds).
    release_rate: f32,
}

/// Allowed upper bound for stage time parameters (in seconds).
const MAX_TIME_S: f32 = 10.0;

impl AdsrProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction-time tolerance).
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate: if sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            attack_s: 0.01,
            decay_s: 0.10,
            sustain: 0.8,
            release_s: 0.20,
            velocity: 1.0,
            stage: Stage::Idle,
            level: 0.0,
            release_rate: 0.0,
        }
    }

    /// Current envelope level. RT-safe.
    pub fn level(&self) -> f32 {
        self.level
    }

    /// Triggers a note (enters Attack, resuming from the current level —
    /// retriggering during release re-attacks).
    /// RT-safe.
    fn trigger(&mut self, velocity: f32) {
        self.velocity = if velocity.is_finite() {
            velocity.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.level = self.level.clamp(0.0, self.velocity);
        self.stage = Stage::Attack;
    }

    /// Releases the note (enters Release; no-op when Idle). RT-safe.
    fn release(&mut self) {
        if self.stage != Stage::Idle {
            self.release_rate = if self.release_s > 1e-6 && self.level > 0.0 {
                self.level / (self.release_s * self.sample_rate)
            } else {
                0.0
            };
            self.stage = Stage::Release;
        }
    }

    /// Advances one sample and returns the envelope level. Zero-duration
    /// stages are skipped within the same sample.
    /// RT-safe.
    fn next_level(&mut self) -> f32 {
        let peak = self.velocity;
        loop {
            match self.stage {
                Stage::Idle => return 0.0,
                Stage::Attack => {
                    if self.attack_s <= 1e-6 {
                        self.level = peak;
                        self.stage = Stage::Decay;
                        continue;
                    }
                    let step = peak / (self.attack_s * self.sample_rate);
                    self.level = (self.level + step).min(peak);
                    if self.level >= peak {
                        self.stage = Stage::Decay;
                    }
                    return self.level;
                }
                Stage::Decay => {
                    let target = peak * self.sustain;
                    if self.decay_s <= 1e-6 {
                        self.level = target;
                        self.stage = Stage::Sustain;
                        continue;
                    }
                    let step = (peak - target) / (self.decay_s * self.sample_rate);
                    self.level = (self.level - step).max(target);
                    if self.level <= target {
                        self.stage = Stage::Sustain;
                    }
                    return self.level;
                }
                Stage::Sustain => {
                    // Recomputed per sample: tracks mid-note changes to sustain / velocity.
                    self.level = peak * self.sustain;
                    return self.level;
                }
                Stage::Release => {
                    if self.release_rate <= 0.0
                        || !self.release_rate.is_finite()
                        || self.level <= 0.0
                    {
                        self.level = 0.0;
                        self.stage = Stage::Idle;
                        continue;
                    }
                    self.level = (self.level - self.release_rate).max(0.0);
                    if self.level <= 0.0 {
                        self.stage = Stage::Idle;
                    }
                    return self.level;
                }
            }
        }
    }
}

impl Processor for AdsrProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        for i in 0..frames {
            let gain = self.next_level();
            for ch in 0..buffer.channels() {
                if let Some(slot) = buffer.channel_mut(ch).and_then(|c| c.get_mut(i)) {
                    *slot *= gain;
                }
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        if !value.is_finite() {
            return matches!(
                id,
                ParamId::Attack | ParamId::Decay | ParamId::Sustain | ParamId::Release
            );
        }
        match id {
            ParamId::Attack => {
                self.attack_s = value.clamp(0.0, MAX_TIME_S);
                true
            }
            ParamId::Decay => {
                self.decay_s = value.clamp(0.0, MAX_TIME_S);
                true
            }
            ParamId::Sustain => {
                self.sustain = value.clamp(0.0, 1.0);
                true
            }
            ParamId::Release => {
                self.release_s = value.clamp(0.0, MAX_TIME_S);
                true
            }
            _ => false,
        }
    }

    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { velocity, .. } => {
                self.trigger(velocity);
                true
            }
            NoteEvent::Off { .. } => {
                self.release();
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AdsrProcessor;
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::{NoteEvent, ParamId};
    use crate::realtime::context::ProcessContext;
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

    /// Renders `blocks` blocks with full-scale input and returns channel 0 after the envelope.
    fn render_with_full_scale(env: &mut AdsrProcessor, blocks: usize) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::new();
        for _ in 0..blocks {
            buf.set_len(BLOCK);
            buf.channel_mut(0).unwrap().fill(1.0);
            env.process(&ctx(), &mut buf);
            out.extend_from_slice(buf.channel(0).unwrap());
        }
        out
    }

    fn note_on() -> NoteEvent {
        NoteEvent::On {
            note: 69,
            velocity: 1.0,
        }
    }

    fn note_off() -> NoteEvent {
        NoteEvent::Off { note: 69 }
    }

    // Standard test 1: silent input → silent output (even while the envelope is in Attack).
    #[test]
    fn silence_in_silence_out() {
        let mut env = AdsrProcessor::new(SR);
        env.note_event(note_on());
        let mut buf = AudioBuffer::new(2, BLOCK);
        buf.set_len(BLOCK);
        buf.clear();
        env.process(&ctx(), &mut buf);
        for ch in 0..2 {
            assert!(buf.channel(ch).unwrap().iter().all(|s| s.abs() < 1e-12));
        }
    }

    // Standard test 2: full-scale input → no NaN/Inf, and level stays in range.
    #[test]
    fn full_scale_stays_bounded_and_finite() {
        let mut env = AdsrProcessor::new(SR);
        env.note_event(note_on());
        let out = render_with_full_scale(&mut env, 500);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(
            out.iter().all(|s| (0.0..=1.0).contains(s)),
            "envelope level must stay within [0,1]"
        );
    }

    #[test]
    fn attack_reaches_peak_in_configured_time() {
        let mut env = AdsrProcessor::new(SR);
        env.set_param(ParamId::Attack, 0.1); // 4800 samples
        env.set_param(ParamId::Decay, 10.0); // lower the decay rate for easier observation
        env.set_param(ParamId::Sustain, 1.0);
        env.note_event(note_on());

        let out = render_with_full_scale(&mut env, 50); // 6000 samples
        let at_4800 = out[4_799];
        assert!(
            (at_4800 - 1.0).abs() < 0.05,
            "should reach the peak at the end of attack, got {at_4800}"
        );
        let early = out[479]; // should still be small at 1%
        assert!(early < 0.2, "attack start should be near 0, got {early}");
    }

    #[test]
    fn release_decays_to_silence() {
        let mut env = AdsrProcessor::new(SR);
        env.set_param(ParamId::Attack, 0.0);
        env.set_param(ParamId::Decay, 0.0);
        env.set_param(ParamId::Sustain, 0.8);
        env.set_param(ParamId::Release, 0.2);
        env.note_event(note_on());
        let _ = render_with_full_scale(&mut env, 20); // 2560 samples, long in sustain

        env.note_event(note_off());
        let out = render_with_full_scale(&mut env, 80); // 10240 samples > release 0.2 s
        let tail = &out[out.len() - BLOCK..];
        assert!(
            tail.iter().all(|s| s.abs() < 0.01),
            "should be silent after release ends, tail {tail:?}"
        );
    }

    #[test]
    fn retrigger_after_partial_release_jumps_back_up() {
        let mut env = AdsrProcessor::new(SR);
        env.set_param(ParamId::Attack, 0.0);
        env.set_param(ParamId::Decay, 0.0);
        env.set_param(ParamId::Sustain, 0.8);
        env.set_param(ParamId::Release, 1.0);
        env.note_event(note_on());
        let _ = render_with_full_scale(&mut env, 20);

        env.note_event(note_off());
        let during_release = render_with_full_scale(&mut env, 4); // during release
        assert!(during_release[during_release.len() - 1] < 0.8);

        env.note_event(note_on()); // retrigger during release
        let retriggered = render_with_full_scale(&mut env, 2);
        assert!(
            retriggered[retriggered.len() - 1] > during_release[during_release.len() - 1],
            "level should rise again after retrigger"
        );
    }

    #[test]
    fn params_are_clamped_and_nan_velocity_defaults_full() {
        let mut env = AdsrProcessor::new(SR);
        assert!(env.set_param(ParamId::Attack, -5.0));
        assert!(env.set_param(ParamId::Sustain, 5.0));
        assert!(
            !env.set_param(ParamId::MasterGain, 1.0),
            "unhandled parameters should return false"
        );

        env.set_param(ParamId::Attack, 0.0);
        env.set_param(ParamId::Decay, 0.0);
        env.note_event(NoteEvent::On {
            note: 60,
            velocity: f32::NAN, // non-finite velocity should fall back to 1.0
        });
        let out = render_with_full_scale(&mut env, 1);
        assert!(
            (out[out.len() - 1] - 1.0).abs() < 1e-6,
            "peak should be 1.0"
        );
    }

    #[test]
    fn zero_attack_and_decay_jump_directly_to_sustain() {
        let mut env = AdsrProcessor::new(SR);
        env.set_param(ParamId::Attack, 0.0);
        env.set_param(ParamId::Decay, 0.0);
        env.set_param(ParamId::Sustain, 0.5);
        env.note_event(note_on());
        let out = render_with_full_scale(&mut env, 1);
        assert!(
            (out[0] - 0.5).abs() < 1e-6,
            "with A/D at 0 the first sample should already be the sustain level"
        );
    }
}
