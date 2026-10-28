//! Automation curve value mapping (U4b).
//!
//! Since the curve editor moved into the timeline's inline band (spec
//! `control-tracks.md` D10), this module holds only what the band and the
//! engine's curve interpolation must agree on: how a parameter value maps to
//! the vertical axis, and the time snap step for anchors.
//!
//! The vertical axis uses the parameter's real range (log axis for filter
//! cutoff and tempo, the 7-bit MIDI range for CC), the same mapping the
//! retired lane editor used — a document authored before U4b looks identical
//! in the band.

use glitcherz_model::{AutoParam, ChannelParam, GraphDoc, NodeDoc, NodeParam, PortRef};

/// Time snap granularity (tick) — a 1/32 note (240 ticks at 960 PPQ).
///
/// (Was 12 at 96 PPQ; missed in the M1.1 rescale because nothing here derived
/// from `TICKS_PER_STEP`.)
pub const SNAP_TICKS: u32 = glitcherz_model::TICKS_PER_STEP / 2;

/// Parameter axis metadata: (min, max, logarithmic?).
pub(crate) fn axis(param: &AutoParam) -> (f32, f32, bool) {
    match param {
        AutoParam::TrackGain => (0.0, 1.5, false),
        AutoParam::TrackPan => (-1.0, 1.0, false),
        AutoParam::FilterCutoff => (20.0, 18_000.0, true),
        // U3b: song tempo shares the transport's own range; log axis so slow
        // ramps get as much room as fast ones.
        AutoParam::Tempo => (20.0, 300.0, true),
        // Plugin parameter range unknown: display normalized 0..1 (interpreted by the plugin).
        AutoParam::PluginParam { .. } => (0.0, 1.0, false),
        // U4a: the curve stores the real 7-bit CC value (spec D7), so the
        // axis is the MIDI range rather than a normalized 0..1.
        AutoParam::Cc { .. } => (0.0, 127.0, false),
        // P1/D9: the bend curve stores **semitones** and `range_semitones`
        // is the full-scale deflection, so the axis is symmetric about zero.
        // This is the reason the range is a document field: an axis without
        // a defined span would make "how far does this dip go" unanswerable.
        AutoParam::PitchBend {
            range_semitones, ..
        } => {
            let range = if range_semitones.is_finite() && *range_semitones > 0.0 {
                *range_semitones
            } else {
                2.0
            };
            (-range, range, false)
        }
        // U4c: node parameters carry their own span except effect slots,
        // whose range lives in the target node's descriptor (see
        // [`axis_in`], which is what the editors call).
        AutoParam::Node(param) => node_param_axis(*param),
    }
}

/// Context-free axis of one [`NodeParam`] (U4c).
///
/// Ranges mirror the UI sliders of each panel (one source of truth per
/// parameter: `RES_RANGE` / `AD_RANGE` / … above). Effect slots have no
/// context-free range — they fall back to 0..1 and are resolved against the
/// node's descriptor by [`axis_in`].
fn node_param_axis(param: NodeParam) -> (f32, f32, bool) {
    match param {
        NodeParam::EffectSlot(_) => (0.0, 1.0, false),
        NodeParam::MixerChannel { param, .. } => match param {
            ChannelParam::Gain => (0.0, 1.5, false),
            ChannelParam::Pan => (-1.0, 1.0, false),
            ChannelParam::Mute | ChannelParam::Solo => (0.0, 1.0, false),
        },
        NodeParam::MixerOutPan => (-1.0, 1.0, false),
        NodeParam::MixerOutMute => (0.0, 1.0, false),
        NodeParam::FeedbackGain => (0.0, 1.0, false),
        NodeParam::Sf2Gain | NodeParam::SampleGain => (0.0, 2.0, false),
        NodeParam::Resonance => (0.0, 0.98, false),
        NodeParam::Attack | NodeParam::Decay => (0.0, 2.0, false),
        NodeParam::Sustain => (0.0, 1.0, false),
        NodeParam::Release => (0.0, 3.0, false),
    }
}

/// Axis of an automation endpoint **with** its node context (U4c): effect
/// slot ranges come from the target effect's descriptor (spec
/// `effects.md` D6), everything else is context-free.
pub(crate) fn axis_in(param: &AutoParam, doc: &GraphDoc, target: &PortRef) -> (f32, f32, bool) {
    if let (AutoParam::Node(NodeParam::EffectSlot(slot)), PortRef::Node(node_id)) = (param, target)
        && let Some(NodeDoc::Effect { params, .. }) = doc.node_by_id(*node_id)
    {
        let (min, max) = crate::effect::range(params, *slot);
        let log = crate::effect::descriptor(params)
            .spec(*slot)
            .is_some_and(|spec| spec.curve == glitcherz_engine::realtime::Curve::Log);
        return (min, max, log);
    }
    axis(param)
}

/// Real value → 0..1 normalization.
pub(crate) fn to_norm(value: f32, min: f32, max: f32, log: bool) -> f32 {
    let norm = if log {
        let (lo, hi) = (f64::from(min).log10(), f64::from(max).log10());
        ((f64::from(value).log10() - lo) / (hi - lo)) as f32
    } else {
        (value - min) / (max - min)
    };
    norm.clamp(0.0, 1.0)
}

/// 0..1 normalization → real value.
pub(crate) fn from_norm(norm: f32, min: f32, max: f32, log: bool) -> f32 {
    let norm = norm.clamp(0.0, 1.0);
    if log {
        let (lo, hi) = (f64::from(min).log10(), f64::from(max).log10());
        10f64.powf(lo + (hi - lo) * f64::from(norm)) as f32
    } else {
        min + (max - min) * norm
    }
}

#[cfg(test)]
mod tests {
    use super::{axis, axis_in, from_norm, to_norm};
    use glitcherz_model::{AutoParam, ChannelParam, GraphDoc, NodeDoc, NodeParam, PortRef};

    #[test]
    fn axis_roundtrip_linear() {
        let (min, max, log) = axis(&AutoParam::TrackGain);
        assert!(!log);
        let norm = to_norm(0.75, min, max, log);
        assert!((from_norm(norm, min, max, log) - 0.75).abs() < 1e-6);
    }

    #[test]
    fn axis_roundtrip_log() {
        let (min, max, log) = axis(&AutoParam::FilterCutoff);
        assert!(log);
        let norm = to_norm(1_200.0, min, max, log);
        assert!((from_norm(norm, min, max, log) - 1_200.0).abs() < 1.0);
    }

    #[test]
    fn values_clamped_to_axis() {
        let (min, max, log) = axis(&AutoParam::TrackPan);
        assert!(to_norm(-99.0, min, max, log) <= 0.0);
        assert!(to_norm(99.0, min, max, log) >= 1.0);
    }

    /// U4a D7: CC curves store the real 7-bit value, so the band's axis is
    /// the MIDI range — an anchor dropped at "half height" must read 64, not
    /// 0.5.
    #[test]
    fn cc_axis_is_the_midi_range() {
        let (min, max, log) = axis(&AutoParam::Cc {
            controller: 74,
            channel: 0,
        });
        assert_eq!((min, max, log), (0.0, 127.0, false));
        let norm = to_norm(64.0, min, max, log);
        assert!((from_norm(norm, min, max, log) - 64.0).abs() < 1e-3);
    }

    /// U4c: node parameters carry the same spans as their panel sliders — a
    /// curve drawn at half height must land on half the fader, not on some
    /// normalized guess.
    #[test]
    fn node_param_axes_match_the_panels() {
        let cases = [
            (
                AutoParam::Node(NodeParam::MixerChannel {
                    channel: 0,
                    param: ChannelParam::Gain,
                }),
                (0.0, 1.5, false),
            ),
            (
                AutoParam::Node(NodeParam::MixerChannel {
                    channel: 3,
                    param: ChannelParam::Pan,
                }),
                (-1.0, 1.0, false),
            ),
            (
                AutoParam::Node(NodeParam::MixerChannel {
                    channel: 1,
                    param: ChannelParam::Mute,
                }),
                (0.0, 1.0, false),
            ),
            (AutoParam::Node(NodeParam::FeedbackGain), (0.0, 1.0, false)),
            (AutoParam::Node(NodeParam::Sf2Gain), (0.0, 2.0, false)),
            (AutoParam::Node(NodeParam::SampleGain), (0.0, 2.0, false)),
            (AutoParam::Node(NodeParam::Resonance), (0.0, 0.98, false)),
            (AutoParam::Node(NodeParam::Release), (0.0, 3.0, false)),
        ];
        for (param, expected) in cases {
            assert_eq!(axis(&param), expected, "{param:?}");
        }
        // Half height on a fader reads 0.75, not 0.5.
        let (min, max, log) = axis(&AutoParam::Node(NodeParam::MixerChannel {
            channel: 0,
            param: ChannelParam::Gain,
        }));
        assert!((from_norm(0.5, min, max, log) - 0.75).abs() < 1e-6);
    }

    /// U4c: an effect slot's span lives in the node's descriptor, so the
    /// context-free axis is only a placeholder — `axis_in` is what the editors
    /// call, and it must return the real range (and the log flag).
    #[test]
    fn effect_slot_axis_comes_from_the_node_descriptor() {
        let params = crate::effect::default_params(glitcherz_model::BuiltinEffectKind::Delay);
        let mut doc = GraphDoc::default();
        doc.nodes.push(NodeDoc::Effect {
            node_id: 7,
            track: 0,
            voice: glitcherz_model::VoiceSpec::Builtin,
            params,
            delay: None,
        });
        // Delay slot 0 is the time in seconds (descriptor range 0.005..=1).
        let param = AutoParam::Node(NodeParam::EffectSlot(0));
        let (min, max, log) = axis_in(&param, &doc, &PortRef::Node(7));
        let (spec_min, spec_max) = crate::effect::range(
            match &doc.nodes[0] {
                NodeDoc::Effect { params, .. } => params,
                _ => unreachable!(),
            },
            0,
        );
        assert_eq!((min, max), (spec_min, spec_max));
        assert!(log, "delay time is a log parameter");

        // No document context (or a nonexistent node): the placeholder axis.
        assert_eq!(axis(&param), (0.0, 1.0, false));
        assert_eq!(axis_in(&param, &doc, &PortRef::Node(99)), (0.0, 1.0, false));
    }
}
