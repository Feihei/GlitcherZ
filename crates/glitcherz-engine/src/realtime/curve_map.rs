//! Curve Map node (spec `.agents/spec/curve-map.md` D5/D6).
//!
//! One control in, one control out, nothing else. The node holds the last
//! normalized value it was handed and re-emits it unchanged on its control
//! output, while **declaring the axis** the next hop should deliver through
//! ([`Processor::map_spec`]). That is the whole point of the node: the value
//! travels normalized everywhere (D1), the axis travels with the *destination*
//! (D2), and this node lets the user insert a destination of their own.
//!
//! The default axis is `-1..1` linear, i.e. the identity — dropping a Curve
//! Map into a chain changes nothing until its range is edited.
//!
//! Real-time rules: no allocation, no locks, no panics; every parameter write
//! is a plain scalar store.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::events::NodeEvents;
use crate::realtime::map::{MapMode, MapSpec};
use crate::realtime::processor::Processor;

/// Control pass-through with a declared output axis.
pub struct CurveMapProcessor {
    min: f32,
    max: f32,
    mode: MapMode,
    /// Last normalized value arriving on the control input. Held rather than
    /// streamed, because the graph delivers control values at block
    /// boundaries and only the newest one matters (it is an absolute level,
    /// not an event stream).
    input: f32,
}

impl CurveMapProcessor {
    /// Identity map (`-1..1` linear), input parked at 0 (axis centre).
    pub fn new() -> Self {
        Self {
            min: -1.0,
            max: 1.0,
            mode: MapMode::Linear,
            input: 0.0,
        }
    }

    /// Loads the document parameters.
    pub fn apply_params(&mut self, min: f32, max: f32, mode: MapMode) {
        self.set_param(ParamId::CurveMapMin, min);
        self.set_param(ParamId::CurveMapMax, max);
        self.set_param(ParamId::CurveMapMode, mode_value(mode));
    }

    /// The axis this node publishes (already repaired, so the audio thread
    /// never sees a backwards or non-finite range).
    pub fn spec(&self) -> MapSpec {
        MapSpec {
            min: self.min,
            max: self.max,
            mode: self.mode,
        }
        .sanitized()
    }
}

impl Default for CurveMapProcessor {
    fn default() -> Self {
        Self::new()
    }
}

/// [`MapMode`] → the float the parameter path speaks (`ParamId::CurveMapMode`).
pub fn mode_value(mode: MapMode) -> f32 {
    match mode {
        MapMode::Linear => 0.0,
        MapMode::Log => 1.0,
        MapMode::Exp => 2.0,
    }
}

/// The parameter path's float → [`MapMode`] (out of range rounds and clamps).
pub fn mode_from_value(value: f32) -> MapMode {
    if !value.is_finite() {
        return MapMode::Linear;
    }
    match value.round() as i32 {
        1 => MapMode::Log,
        2 => MapMode::Exp,
        _ => MapMode::Linear,
    }
}

impl Processor for CurveMapProcessor {
    fn process(&mut self, _ctx: &ProcessContext, _buffer: &mut AudioBuffer) {
        // A control node has no audio: the graph still gives it a (silent)
        // buffer, and leaving it untouched is exactly right.
    }

    fn process_events(&mut self, _ctx: &ProcessContext, out: &mut NodeEvents) {
        // Pass-through (D5): the input is already normalized, so re-emitting
        // it is the entire DSP. The axis is published through `map_spec`.
        out.control = Some(self.input);
    }

    fn map_spec(&self) -> Option<MapSpec> {
        Some(self.spec())
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            // Normalized pass-through port (D5/D6): no axis applies here, so
            // the value is stored as-is — clamping to `[-1, 1]` keeps a
            // hand-written or mis-scaled source from parking the output
            // outside the bus's domain.
            ParamId::CurveMapIn => {
                if value.is_finite() {
                    self.input = value.clamp(-1.0, 1.0);
                }
                true
            }
            ParamId::CurveMapMin => {
                if value.is_finite() {
                    self.min = value;
                }
                true
            }
            ParamId::CurveMapMax => {
                if value.is_finite() {
                    self.max = value;
                }
                true
            }
            ParamId::CurveMapMode => {
                self.mode = mode_from_value(value);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CurveMapProcessor, mode_from_value, mode_value};
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::events::NodeEvents;
    use crate::realtime::map::MapMode;
    use crate::realtime::processor::Processor;

    const BLOCK: usize = 128;

    fn ctx() -> ProcessContext {
        ProcessContext {
            sample_rate: 48_000.0,
            frames: BLOCK,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        }
    }

    fn emit(processor: &mut CurveMapProcessor) -> f32 {
        let mut out = NodeEvents::default();
        processor.process_events(&ctx(), &mut out);
        out.control.expect("a curve map always emits")
    }

    #[test]
    fn the_default_axis_is_the_identity() {
        let processor = CurveMapProcessor::new();
        let map = processor.map_spec().expect("declares its axis");
        assert_eq!(map.map(-1.0), -1.0);
        assert_eq!(map.map(0.0), 0.0);
        assert_eq!(map.map(1.0), 1.0);
    }

    #[test]
    fn the_value_passes_through_untouched() {
        let mut processor = CurveMapProcessor::new();
        processor.set_param(ParamId::CurveMapMin, 20.0);
        processor.set_param(ParamId::CurveMapMax, 18_000.0);
        processor.set_param(ParamId::CurveMapMode, mode_value(MapMode::Log));
        processor.set_param(ParamId::CurveMapIn, 0.5);
        assert_eq!(emit(&mut processor), 0.5, "the axis applies downstream");
        processor.set_param(ParamId::CurveMapIn, -1.0);
        assert_eq!(emit(&mut processor), -1.0);
    }

    #[test]
    fn the_declared_axis_drives_the_next_hop() {
        let mut processor = CurveMapProcessor::new();
        processor.set_param(ParamId::CurveMapMin, 20.0);
        processor.set_param(ParamId::CurveMapMax, 18_000.0);
        processor.set_param(ParamId::CurveMapMode, mode_value(MapMode::Log));
        let map = processor.map_spec().unwrap();
        assert!((map.map(1.0) - 18_000.0).abs() < 0.1);
        assert!((map.map(0.0) - 600.0).abs() < 1.0, "geometric midpoint");
    }

    #[test]
    fn a_backwards_range_degrades_to_the_identity() {
        let mut processor = CurveMapProcessor::new();
        processor.set_param(ParamId::CurveMapMin, 10.0);
        processor.set_param(ParamId::CurveMapMax, 2.0);
        assert_eq!(processor.spec().map(0.0), 0.0, "repaired to -1..1");
        // Non-finite writes are ignored rather than poisoning the axis.
        processor.set_param(ParamId::CurveMapMin, f32::NAN);
        assert!(processor.spec().min.is_finite());
    }

    #[test]
    fn the_input_stays_inside_the_normalized_domain() {
        let mut processor = CurveMapProcessor::new();
        processor.set_param(ParamId::CurveMapIn, 9.0);
        assert_eq!(emit(&mut processor), 1.0);
        processor.set_param(ParamId::CurveMapIn, -9.0);
        assert_eq!(emit(&mut processor), -1.0);
        processor.set_param(ParamId::CurveMapIn, f32::NAN);
        assert_eq!(emit(&mut processor), -1.0, "NaN is ignored, not stored");
    }

    #[test]
    fn the_mode_value_round_trips() {
        for mode in [MapMode::Linear, MapMode::Log, MapMode::Exp] {
            assert_eq!(mode_from_value(mode_value(mode)), mode);
        }
        // Out-of-range / non-finite values land on Linear instead of panicking.
        assert_eq!(mode_from_value(-3.0), MapMode::Linear);
        assert_eq!(mode_from_value(99.0), MapMode::Linear);
        assert_eq!(mode_from_value(f32::NAN), MapMode::Linear);
    }

    #[test]
    fn unknown_parameters_are_ignored() {
        let mut processor = CurveMapProcessor::new();
        assert!(!processor.set_param(ParamId::MasterGain, 0.5));
    }
}
