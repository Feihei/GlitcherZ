//! Document ↔ engine bridge for built-in effects (spec
//! `.agents/spec/effects.md` §5/D1).
//!
//! The model stores a tagged [`EffectParams`] payload; the engine exposes a
//! [`BuiltinEffectKind`] plus a static descriptor table. This module is the
//! single place that maps one onto the other, and the single place that knows
//! which document field backs which parameter slot — every UI surface (patch
//! inline rows, Inspector sliders, param push, menu labels) goes through it, so
//! a new effect only adds arms here.

use glitcherz_engine::realtime::{BuiltinEffectKind, EffectDescriptor, ParamSpec, Unit, delay};
use glitcherz_model::{BuiltinEffectKind as DocEffectKind, EffectParams};

/// Engine kind of a document payload.
pub fn engine_kind(params: &EffectParams) -> BuiltinEffectKind {
    match params.kind() {
        DocEffectKind::Delay => BuiltinEffectKind::Delay,
    }
}

/// Static descriptor for a document payload.
pub fn descriptor(params: &EffectParams) -> &'static EffectDescriptor {
    engine_kind(params).descriptor()
}

/// Default payload for a newly created node of `kind`.
pub fn default_params(kind: DocEffectKind) -> EffectParams {
    match kind {
        DocEffectKind::Delay => EffectParams::Delay(glitcherz_model::DelayParams::default()),
    }
}

/// Value of `slot` in the document payload (`0.0` for an unknown slot).
pub fn value(params: &EffectParams, slot: u8) -> f32 {
    match params {
        EffectParams::Delay(delay) => match slot {
            delay::SLOT_TIME => delay.time_s,
            delay::SLOT_FEEDBACK => delay.feedback,
            delay::SLOT_MIX => delay.mix,
            _ => 0.0,
        },
    }
}

/// Writes `slot` in the document payload (unknown slots are ignored).
pub fn set_value(params: &mut EffectParams, slot: u8, value: f32) {
    match params {
        EffectParams::Delay(delay) => match slot {
            delay::SLOT_TIME => delay.time_s = value,
            delay::SLOT_FEEDBACK => delay.feedback = value,
            delay::SLOT_MIX => delay.mix = value,
            _ => {}
        },
    }
}

/// Range of a slot straight from the descriptor (`0..1` when unknown).
pub fn range(params: &EffectParams, slot: u8) -> (f32, f32) {
    match descriptor(params).spec(slot) {
        Some(spec) => (spec.min, spec.max),
        None => (0.0, 1.0),
    }
}

/// Slider text for one value ("12.5 ms", "-3.0 dB", "45 %").
pub fn format_value(spec: &ParamSpec, value: f32) -> String {
    match spec.unit {
        Unit::Db => format!("{value:+.1} dB"),
        Unit::Hz if value >= 1000.0 => format!("{:.2} kHz", value / 1000.0),
        Unit::Hz => format!("{value:.0} Hz"),
        // Seconds are the storage unit; below one second the millisecond form
        // is what a user actually reads off a delay readout.
        Unit::Seconds if value < 1.0 => format!("{:.1} ms", value * 1000.0),
        Unit::Seconds => format!("{value:.2} s"),
        Unit::Ms if value >= 1000.0 => format!("{:.2} s", value / 1000.0),
        Unit::Ms => format!("{value:.1} ms"),
        Unit::Percent => format!("{:.0} %", value * 100.0),
        Unit::Ratio => format!("{value:.2}"),
        Unit::Pan if value.abs() < 0.005 => "C".to_string(),
        Unit::Pan if value < 0.0 => format!("L{:.0}", -value * 100.0),
        Unit::Pan => format!("R{:.0}", value * 100.0),
        Unit::Bool if value >= 0.5 => "On".to_string(),
        Unit::Bool => "Off".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{descriptor, engine_kind, format_value, range, set_value, value};
    use glitcherz_engine::realtime::BuiltinEffectKind;
    use glitcherz_model::{DelayParams, EffectParams};

    fn delay_params() -> EffectParams {
        EffectParams::Delay(DelayParams {
            time_s: 0.25,
            feedback: 0.5,
            mix: 0.75,
        })
    }

    #[test]
    fn kind_maps_to_the_engine_roster() {
        assert_eq!(engine_kind(&delay_params()), BuiltinEffectKind::Delay);
        assert_eq!(descriptor(&delay_params()).name, "Delay");
    }

    #[test]
    fn slots_read_and_write_the_document_fields() {
        let mut params = delay_params();
        assert!((value(&params, super::delay::SLOT_TIME) - 0.25).abs() < 1e-6);
        set_value(&mut params, super::delay::SLOT_MIX, 0.1);
        assert!((value(&params, super::delay::SLOT_MIX) - 0.1).abs() < 1e-6);
        // Out-of-range slots are inert, never a panic.
        assert_eq!(value(&params, 9), 0.0);
        set_value(&mut params, 9, 1.0);
    }

    #[test]
    fn ranges_come_from_the_descriptor() {
        let params = delay_params();
        let (min, max) = range(&params, super::delay::SLOT_TIME);
        assert!((min - 0.005).abs() < 1e-9);
        assert!((max - 1.0).abs() < 1e-9);
        assert_eq!(range(&params, 42), (0.0, 1.0));
    }

    #[test]
    fn formatting_follows_the_unit() {
        let params = delay_params();
        let spec = descriptor(&params).spec(super::delay::SLOT_TIME).unwrap();
        assert_eq!(format_value(spec, 0.25), "250.0 ms");
        assert_eq!(format_value(spec, 1.5), "1.50 s");
        let mix = descriptor(&params).spec(super::delay::SLOT_MIX).unwrap();
        assert_eq!(format_value(mix, 0.5), "50 %");
    }
}
