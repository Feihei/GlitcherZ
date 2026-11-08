//! Ring modulator — Z0's minimal two-input effect
//! (spec `.agents/spec/dual-audio-input.md` Z-D11).
//!
//! `out = lerp(main, main × mod, mix)` per sample, with no state and no
//! latency. It exists to prove the graph delivers two independent,
//! block-aligned audio buffers; sidechain compression and the vocoder reuse
//! the same plumbing. A missing Mod port behaves as an all-zero modulator.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::EffectParamBank;
use crate::realtime::effect::descriptor::{EffectDescriptor, ParamSpec, StereoClass, Unit};
use crate::realtime::processor::Processor;

/// Dry/wet mix slot.
pub const SLOT_MIX: u8 = 0;

static RING_MOD_PARAMS: [ParamSpec; 1] =
    [ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 1.0).inline_row()];

/// Static parameter table.
pub static DESCRIPTOR: EffectDescriptor = EffectDescriptor {
    name: "Ring Mod",
    params: &RING_MOD_PARAMS,
    stereo: StereoClass::Mono,
};

/// Ring modulator (Main + Mod in, 1 out, zero latency).
pub struct RingModProcessor {
    params: EffectParamBank,
}

impl RingModProcessor {
    /// Construction time only; the smoothing coefficient depends on the rate.
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            params: EffectParamBank::new(&DESCRIPTOR, sample_rate),
        }
    }

    /// Current (settled) value of a slot, for tests and read-back.
    pub fn value(&self, slot: u8) -> f32 {
        self.params.value(slot)
    }
}

impl Processor for RingModProcessor {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        // A single-input graph never supplies aux: treat the modulator as
        // silent rather than as a copy of Main.
        self.process_with_aux(ctx, buffer, None);
    }

    fn process_with_aux(
        &mut self,
        _ctx: &ProcessContext,
        buffer: &mut AudioBuffer,
        aux: Option<&AudioBuffer>,
    ) {
        let frames = buffer.len();
        let channels = buffer.channels().min(2);
        if frames == 0 || channels == 0 {
            return;
        }
        for index in 0..frames {
            let mix = self.params.tick(SLOT_MIX).clamp(0.0, 1.0);
            if mix == 0.0 {
                // Strict sample-exact passthrough (E series invariant): do
                // not write the buffer at all, but keep the smoother moving.
                continue;
            }
            for ch in 0..channels {
                let input = buffer
                    .channel(ch)
                    .and_then(|channel| channel.get(index).copied())
                    .unwrap_or(0.0);
                let modulator = aux
                    .and_then(|a| a.channel(ch).or_else(|| a.channel(0)))
                    .and_then(|channel| channel.get(index).copied())
                    .unwrap_or(0.0);
                if let Some(sample) = buffer
                    .channel_mut(ch)
                    .and_then(|channel| channel.get_mut(index))
                {
                    *sample = input * (1.0 - mix) + input * modulator * mix;
                }
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::BuiltinParam(slot) => self.params.set(slot, value),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RingModProcessor, SLOT_MIX};
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

    fn stereo(left: f32, right: f32, frames: usize) -> AudioBuffer {
        let mut buf = AudioBuffer::new(2, frames);
        buf.set_len(frames);
        buf.channel_mut(0).unwrap().fill(left);
        buf.channel_mut(1).unwrap().fill(right);
        buf
    }

    #[test]
    fn two_ports_do_not_leak_into_each_other() {
        let mut proc = RingModProcessor::new(48_000.0);
        let mut main = stereo(0.5, -0.5, 8);
        let aux = stereo(-0.25, 0.5, 8);
        proc.process_with_aux(&ctx(8), &mut main, Some(&aux));
        // 0.5 * -0.25 = -0.125 (L); -0.5 * 0.5 = -0.25 (R).
        assert!(
            main.channel(0)
                .unwrap()
                .iter()
                .all(|s| (*s + 0.125).abs() < 1e-6)
        );
        assert!(
            main.channel(1)
                .unwrap()
                .iter()
                .all(|s| (*s + 0.25).abs() < 1e-6)
        );
    }

    #[test]
    fn missing_mod_port_behaves_as_silence() {
        let mut proc = RingModProcessor::new(48_000.0);
        let mut main = stereo(0.75, 0.75, 8);
        proc.process_with_aux(&ctx(8), &mut main, None);
        assert!(main.channel(0).unwrap().iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn dry_mix_is_sample_exact_passthrough() {
        let mut proc = RingModProcessor::new(48_000.0);
        assert!(proc.set_param(ParamId::BuiltinParam(SLOT_MIX), 0.0));
        let before = stereo(0.3, -0.7, 8);
        let mut main = stereo(0.3, -0.7, 8);
        let aux = stereo(1.0, 1.0, 8);
        proc.process_with_aux(&ctx(8), &mut main, Some(&aux));
        assert_eq!(main.channel(0).unwrap(), before.channel(0).unwrap());
        assert_eq!(main.channel(1).unwrap(), before.channel(1).unwrap());
    }

    #[test]
    fn silent_input_stays_silent() {
        let mut proc = RingModProcessor::new(48_000.0);
        let mut main = stereo(0.0, 0.0, 8);
        let aux = stereo(1.0, -1.0, 8);
        proc.process_with_aux(&ctx(8), &mut main, Some(&aux));
        assert!(main.channel(0).unwrap().iter().all(|s| *s == 0.0));
        assert!(main.channel(1).unwrap().iter().all(|s| *s == 0.0));
    }

    #[test]
    fn full_scale_input_has_no_non_finite_output() {
        let mut proc = RingModProcessor::new(48_000.0);
        let mut main = stereo(1.0, -1.0, 8);
        let aux = stereo(-1.0, 1.0, 8);
        proc.process_with_aux(&ctx(8), &mut main, Some(&aux));
        assert!(
            main.channel(0)
                .unwrap()
                .iter()
                .chain(main.channel(1).unwrap().iter())
                .all(|s| s.is_finite())
        );
    }
}
