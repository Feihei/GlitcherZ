//! Automation curve value mapping (U4b, revised by spec `curve-map.md` C1).
//!
//! Curves store **normalized** values in `[-1, 1]` (D1) — `-1` is the bottom of
//! the destination axis, `0` its centre, `+1` its top. The axis itself lives on
//! the cable ([`glitcherz_model::MapSpec`]), so re-routing a curve to another
//! parameter keeps its shape, and the editor draws the same curve at the same
//! height whatever the target is.
//!
//! What is left here: the normalized ↔ screen mapping, the axis caption, and
//! the time snap step the band and the engine's interpolation agree on.

use glitcherz_model::{AutoParam, GraphDoc, MapMode, MapSpec, PortRef};

/// Time snap granularity (tick) — a 1/32 note (240 ticks at 960 PPQ).
///
/// (Was 12 at 96 PPQ; missed in the M1.1 rescale because nothing here derived
/// from `TICKS_PER_STEP`.)
pub const SNAP_TICKS: u32 = glitcherz_model::TICKS_PER_STEP / 2;

/// The mapping of an automation endpoint, resolved against the document (an
/// effect slot's range lives in the target node's payload).
///
/// The single access point for the UI: the curve band, the roll overlay and
/// the target menus all read the axis from here, so what the user sees, what
/// the engine applies and what the migration assumed are the same table.
pub(crate) fn map_in(param: &AutoParam, doc: &GraphDoc, target: &PortRef) -> MapSpec {
    param.default_map(doc, target)
}

/// Normalized value → vertical position in `0..1` (0 = bottom).
pub(crate) fn y_of(norm: f32) -> f32 {
    (norm.clamp(-1.0, 1.0) + 1.0) * 0.5
}

/// Vertical position in `0..1` → normalized value.
pub(crate) fn norm_of_y(y: f32) -> f32 {
    y.clamp(0.0, 1.0) * 2.0 - 1.0
}

/// Caption for the axis: `"20…18000 Log"` (real units, plus the mode when it
/// is not linear).
pub(crate) fn axis_caption(map: MapSpec) -> String {
    let map = map.sanitized();
    match map.mode {
        MapMode::Linear => format!("{:.4}…{:.4}", map.min, map.max),
        mode => format!("{:.4}…{:.4} {}", map.min, map.max, mode.label()),
    }
}

#[cfg(test)]
mod tests {
    use super::{axis_caption, map_in, norm_of_y, y_of};
    use glitcherz_model::{AutoParam, GraphDoc, MapMode, MapSpec, NodeParam, PortRef};

    #[test]
    fn normalized_values_sit_on_a_centred_axis() {
        assert!((y_of(-1.0) - 0.0).abs() < 1e-6);
        assert!((y_of(0.0) - 0.5).abs() < 1e-6);
        assert!((y_of(1.0) - 1.0).abs() < 1e-6);
        // Out-of-range anchors still draw inside the band.
        assert!((y_of(-9.0) - 0.0).abs() < 1e-6);
        assert!((y_of(9.0) - 1.0).abs() < 1e-6);
        // Round trip through the pixel mapping.
        for norm in [-1.0, -0.25, 0.0, 0.75, 1.0] {
            assert!((norm_of_y(y_of(norm)) - norm).abs() < 1e-6);
        }
    }

    #[test]
    fn axis_caption_shows_real_units_and_the_mode() {
        assert_eq!(
            axis_caption(MapSpec::linear(-24.0, 24.0)),
            "-24.0000…24.0000"
        );
        assert_eq!(
            axis_caption(MapSpec::log(20.0, 18_000.0)),
            "20.0000…18000.0000 Log"
        );
        assert_eq!(
            axis_caption(MapSpec {
                min: 0.0,
                max: 100.0,
                mode: MapMode::Log,
            }),
            "0.0000…100.0000",
            "a log axis without a positive floor degrades to linear"
        );
    }

    /// The editor's axis is the cable's map, resolved with the target node.
    #[test]
    fn map_in_resolves_against_the_target() {
        let mut doc = GraphDoc::default();
        doc.nodes.push(glitcherz_model::NodeDoc::Effect {
            node_id: 7,
            track: 0,
            voice: glitcherz_model::VoiceSpec::Builtin,
            params: glitcherz_model::EffectParams::Delay(glitcherz_model::DelayParams::default()),
            delay: None,
        });
        let slot = AutoParam::Node(NodeParam::EffectSlot(0));
        assert_eq!(
            map_in(&slot, &doc, &PortRef::Node(7)),
            MapSpec::log(0.005, 1.0)
        );
        assert_eq!(
            AutoParam::pitch_bend(0, 7.0).default_map(&doc, &PortRef::Node(7)),
            MapSpec::linear(-7.0, 7.0)
        );
    }
}
