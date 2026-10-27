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

use glitcherz_model::AutoParam;

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
    }
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
    use super::{axis, from_norm, to_norm};
    use glitcherz_model::AutoParam;

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
}
