//! Mixer node (P5f, D15/D21): the explicit summing point.
//!
//! Semantics split in two stages:
//!
//! - **Per-channel weights** (gain/pan/mute/solo) are applied *while the
//!   graph sums each in-edge* into the node's buffer — exposed via
//!   [`Processor::input_weights`]. Once summed, per-channel separation no
//!   longer exists, so the weights must live in the summing loop (D20).
//! - **Output stage** (pan/mute) is applied in place after summing, same
//!   math as the old channel strip.
//!
//! Fader moves ride [`Processor::set_param`] (command path, block boundary)
//! and recompute the cached effective weights — they never trigger a
//! topology rebuild (D5).

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::{ChannelWeights, MAX_WEIGHT_CHANNELS, Processor};

/// One mixer channel's console state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MixerChannel {
    /// Channel gain (linear, 0..1.5).
    pub gain: f32,
    /// Channel pan (-1..1, linear law).
    pub pan: f32,
    /// Channel mute.
    pub muted: bool,
    /// Channel solo (any solo mutes all non-soloed channels of the mixer).
    pub solo: bool,
}

impl Default for MixerChannel {
    fn default() -> Self {
        Self {
            gain: 0.8,
            pan: 0.0,
            muted: false,
            solo: false,
        }
    }
}

/// Mixer processor: up to [`MAX_WEIGHT_CHANNELS`] input channels summed with
/// per-channel weights + an output pan/mute stage.
pub struct MixerProcessor {
    channels: [MixerChannel; MAX_WEIGHT_CHANNELS],
    out_pan: f32,
    out_muted: bool,
    /// Cached effective L/R weights, recomputed on every parameter change.
    weights: ChannelWeights,
}

impl MixerProcessor {
    /// Mixer with `count` channels (clamped to 1..=[`MAX_WEIGHT_CHANNELS`]);
    /// all channels at console defaults.
    pub fn new(count: usize) -> Self {
        Self::with_channels(
            &vec![MixerChannel::default(); count.clamp(1, MAX_WEIGHT_CHANNELS)],
            0.0,
            false,
        )
    }

    /// Mixer from explicit channel state (the document-load path). Channels
    /// beyond [`MAX_WEIGHT_CHANNELS`] are dropped; missing entries fall back
    /// to console defaults.
    pub fn with_channels(channels: &[MixerChannel], out_pan: f32, out_muted: bool) -> Self {
        let mut this = Self {
            channels: [MixerChannel::default(); MAX_WEIGHT_CHANNELS],
            out_pan: sanitize_pan(out_pan),
            out_muted,
            weights: [[0.0; 2]; MAX_WEIGHT_CHANNELS],
        };
        for (slot, ch) in this.channels.iter_mut().zip(channels.iter()) {
            *slot = MixerChannel {
                gain: sanitize_gain(ch.gain),
                pan: sanitize_pan(ch.pan),
                muted: ch.muted,
                solo: ch.solo,
            };
        }
        this.recompute();
        this
    }

    /// Recomputes the effective L/R summing weights from the console state.
    fn recompute(&mut self) {
        let any_solo = self.channels.iter().any(|c| c.solo);
        for (weight, ch) in self.weights.iter_mut().zip(self.channels.iter()) {
            *weight = if ch.muted || (any_solo && !ch.solo) {
                [0.0, 0.0]
            } else {
                let left = (1.0 - ch.pan).clamp(0.0, 1.0);
                let right = (1.0 + ch.pan).clamp(0.0, 1.0);
                [ch.gain * left, ch.gain * right]
            };
        }
    }
}

fn sanitize_gain(g: f32) -> f32 {
    if g.is_finite() {
        g.clamp(0.0, 1.5)
    } else {
        0.0
    }
}

fn sanitize_pan(p: f32) -> f32 {
    if p.is_finite() {
        p.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

impl Processor for MixerProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        // Output stage only: per-channel weights were applied by the graph's
        // summing loop (D20). Same pan/mute math as the old channel strip.
        if self.out_muted {
            for ch in 0..buffer.channels() {
                if let Some(channel) = buffer.channel_mut(ch) {
                    channel.fill(0.0);
                }
            }
            return;
        }
        let left = (1.0 - self.out_pan).clamp(0.0, 1.0);
        let right = (1.0 + self.out_pan).clamp(0.0, 1.0);
        for (ch, pan_gain) in [(0usize, left), (1usize, right)] {
            if pan_gain >= 1.0 {
                continue; // identity fast path
            }
            if pan_gain <= 0.0 {
                if let Some(channel) = buffer.channel_mut(ch) {
                    channel.fill(0.0);
                }
                continue;
            }
            if let Some(channel) = buffer.channel_mut(ch) {
                for sample in channel.iter_mut() {
                    *sample *= pan_gain;
                }
            }
        }
    }

    fn input_weights(&self) -> Option<&ChannelWeights> {
        Some(&self.weights)
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        let changed = match id {
            ParamId::MixerGain(ch) => self
                .channels
                .get_mut(ch as usize)
                .map(|c| c.gain = sanitize_gain(value))
                .is_some(),
            ParamId::MixerPan(ch) => self
                .channels
                .get_mut(ch as usize)
                .map(|c| c.pan = sanitize_pan(value))
                .is_some(),
            ParamId::MixerMute(ch) => self
                .channels
                .get_mut(ch as usize)
                .map(|c| c.muted = value >= 0.5)
                .is_some(),
            ParamId::MixerSolo(ch) => self
                .channels
                .get_mut(ch as usize)
                .map(|c| c.solo = value >= 0.5)
                .is_some(),
            ParamId::MixerOutPan => {
                let p = sanitize_pan(value);
                let changed = p != self.out_pan;
                self.out_pan = p;
                changed
            }
            ParamId::MixerOutMute => {
                let m = value >= 0.5;
                let changed = m != self.out_muted;
                self.out_muted = m;
                changed
            }
            _ => false,
        };
        if changed {
            self.recompute();
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::{MixerChannel, MixerProcessor};
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;

    fn ctx() -> ProcessContext {
        ProcessContext {
            sample_rate: 48_000.0,
            frames: 8,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        }
    }

    fn filled_buffer(value: f32) -> AudioBuffer {
        let mut buf = AudioBuffer::new(2, 8);
        buf.set_len(8);
        buf.channel_mut(0).unwrap().fill(value);
        buf.channel_mut(1).unwrap().fill(value);
        buf
    }

    #[test]
    fn weights_follow_gain_pan_mute() {
        let mixer = MixerProcessor::with_channels(
            &[
                MixerChannel {
                    gain: 1.0,
                    pan: 0.0,
                    muted: false,
                    solo: false,
                },
                MixerChannel {
                    gain: 0.5,
                    pan: 1.0,
                    muted: false,
                    solo: false,
                },
                MixerChannel {
                    gain: 1.0,
                    pan: 0.0,
                    muted: true,
                    solo: false,
                },
            ],
            0.0,
            false,
        );
        let weights = mixer.input_weights().unwrap();
        assert!((weights[0][0] - 1.0).abs() < 1e-6);
        assert!((weights[0][1] - 1.0).abs() < 1e-6);
        // Pan hard right: silent left, half gain right.
        assert!(weights[1][0].abs() < 1e-6);
        assert!((weights[1][1] - 0.5).abs() < 1e-6);
        // Muted channel: silent both sides.
        assert!(weights[2][0].abs() < 1e-6);
        assert!(weights[2][1].abs() < 1e-6);
    }

    #[test]
    fn solo_mutes_non_soloed_channels() {
        let mixer = MixerProcessor::with_channels(
            &[
                MixerChannel::default(),
                MixerChannel {
                    solo: true,
                    ..MixerChannel::default()
                },
            ],
            0.0,
            false,
        );
        let weights = mixer.input_weights().unwrap();
        assert!(weights[0][0].abs() < 1e-6, "non-soloed channel silenced");
        let expected = 0.8;
        assert!(
            (weights[1][0] - expected).abs() < 1e-6,
            "soloed channel keeps gain"
        );
    }

    #[test]
    fn param_updates_recompute_weights_rt_path() {
        let mut mixer = MixerProcessor::new(2);
        assert!(mixer.set_param(ParamId::MixerMute(0), 1.0));
        assert!(mixer.input_weights().unwrap()[0][0].abs() < 1e-6);
        assert!(mixer.set_param(ParamId::MixerGain(1), 0.25));
        assert!((mixer.input_weights().unwrap()[1][0] - 0.25).abs() < 1e-6);
        assert!(!mixer.set_param(ParamId::SineFrequency, 100.0));
    }

    #[test]
    fn output_stage_pan_mutes_buffer() {
        let mut mixer = MixerProcessor::new(2);
        assert!(mixer.set_param(ParamId::MixerOutMute, 1.0));
        let mut buf = filled_buffer(0.5);
        mixer.process(&ctx(), &mut buf);
        assert!(buf.channel(0).unwrap().iter().all(|s| s.abs() < 1e-12));

        let mut mixer = MixerProcessor::new(2);
        assert!(mixer.set_param(ParamId::MixerOutPan, 1.0));
        let mut buf = filled_buffer(0.5);
        mixer.process(&ctx(), &mut buf);
        assert!(buf.channel(0).unwrap().iter().all(|s| s.abs() < 1e-6));
        assert!(
            buf.channel(1)
                .unwrap()
                .iter()
                .all(|s| (*s - 0.5).abs() < 1e-6)
        );
    }
}
