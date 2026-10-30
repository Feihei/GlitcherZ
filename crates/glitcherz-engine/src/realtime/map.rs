//! Normalized control-value mapping (spec `.agents/spec/curve-map.md`).
//!
//! Curves and node control outputs carry a normalized `[-1, 1]` value; the
//! **destination** carries the range and mode that turn it into real units
//! (cutoff Hz, fader gain, semitones, MIDI CC, ...). That split is what lets a
//! curve keep its shape when it is re-routed, and what lets one control source
//! drive three different targets without knowing what they are.
//!
//! Everything here is `Copy` arithmetic: no tables, no allocation, no locks
//! (spec D10).

/// How a [`MapSpec`] turns `t = (norm + 1) / 2 ∈ [0, 1]` into a real value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapMode {
    /// `min + t·(max-min)`.
    Linear,
    /// `min·(max/min)^t` — requires `min > 0`; falls back to linear otherwise.
    Log,
    /// `min + t²·(max-min)` — the top of the range gets the room.
    Exp,
}

/// Range + mapping of a normalized control value onto a target's real axis.
///
/// `-1` maps to `min`, `0` to the axis centre, `+1` to `max`, for every mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapSpec {
    /// Value at `norm = -1`.
    pub min: f32,
    /// Value at `norm = +1`.
    pub max: f32,
    /// Mapping mode.
    pub mode: MapMode,
}

impl MapSpec {
    /// Identity map: the normalized value is the real value.
    pub const UNIT: Self = Self {
        min: -1.0,
        max: 1.0,
        mode: MapMode::Linear,
    };

    /// Linear map of `min..max` (const so `AutoRoute::default` stays const).
    pub const fn linear(min: f32, max: f32) -> Self {
        Self {
            min,
            max,
            mode: MapMode::Linear,
        }
    }

    /// Log map of `min..max`.
    pub const fn log(min: f32, max: f32) -> Self {
        Self {
            min,
            max,
            mode: MapMode::Log,
        }
    }

    /// Repairs a malformed spec so the audio thread never has to (D3): inverted
    /// or non-finite bounds collapse to [`Self::UNIT`] and a `Log` range with a
    /// non-positive lower bound degrades to `Linear`.
    pub fn sanitized(self) -> Self {
        if !self.min.is_finite() || !self.max.is_finite() || self.max < self.min {
            return Self::UNIT;
        }
        let mode = match self.mode {
            MapMode::Log if self.min <= 0.0 => MapMode::Linear,
            mode => mode,
        };
        Self {
            min: self.min,
            max: self.max,
            mode,
        }
    }

    /// Normalized `[-1, 1]` → real axis value. RT-safe.
    pub fn map(self, norm: f32) -> f32 {
        if !norm.is_finite() {
            return self.min;
        }
        let spec = self.sanitized();
        let t = (norm.clamp(-1.0, 1.0) + 1.0) * 0.5;
        match spec.mode {
            MapMode::Linear => spec.min + t * (spec.max - spec.min),
            MapMode::Log => spec.min * (spec.max / spec.min).powf(t),
            MapMode::Exp => spec.min + t * t * (spec.max - spec.min),
        }
    }

    /// Real axis value → normalized `[-1, 1]` (inverse of [`Self::map`]).
    ///
    /// Not on the audio path (delivery only ever goes the other way); kept for
    /// tools, tests and the export path, which start from real-unit data.
    pub fn unmap(self, value: f32) -> f32 {
        let spec = self.sanitized();
        if !value.is_finite() || spec.max <= spec.min {
            return 0.0;
        }
        let value = value.clamp(spec.min, spec.max);
        let t = match spec.mode {
            MapMode::Linear => (value - spec.min) / (spec.max - spec.min),
            MapMode::Log => (value / spec.min).ln() / (spec.max / spec.min).ln(),
            MapMode::Exp => ((value - spec.min) / (spec.max - spec.min)).max(0.0).sqrt(),
        };
        (t * 2.0 - 1.0).clamp(-1.0, 1.0)
    }
}

impl Default for MapSpec {
    fn default() -> Self {
        Self::UNIT
    }
}

#[cfg(test)]
mod tests {
    use super::{MapMode, MapSpec};

    #[test]
    fn ends_and_centre_are_exact() {
        let pan = MapSpec::linear(-1.0, 1.0);
        assert_eq!(pan.map(-1.0), -1.0);
        assert_eq!(pan.map(0.0), 0.0);
        assert_eq!(pan.map(1.0), 1.0);
    }

    #[test]
    fn log_is_the_geometric_mean_at_the_centre() {
        let cutoff = MapSpec::log(20.0, 18_000.0);
        assert!((cutoff.map(-1.0) - 20.0).abs() < 1e-3);
        assert!((cutoff.map(0.0) - 600.0).abs() < 1.0);
        assert!((cutoff.map(1.0) - 18_000.0).abs() < 0.1);
    }

    #[test]
    fn exp_pushes_resolution_to_the_top() {
        let spec = MapSpec {
            min: 0.0,
            max: 100.0,
            mode: MapMode::Exp,
        };
        assert!((spec.map(0.0) - 25.0).abs() < 1e-4);
        assert!((spec.map(1.0) - 100.0).abs() < 1e-4);
    }

    #[test]
    fn malformed_specs_degrade_instead_of_garbling() {
        // Log without a positive floor cannot take a logarithm.
        let spec = MapSpec {
            min: 0.0,
            max: 127.0,
            mode: MapMode::Log,
        };
        assert_eq!(spec.sanitized().mode, MapMode::Linear);
        assert!((spec.map(0.0) - 63.5).abs() < 1e-4);
        // Inverted / NaN bounds become the identity map.
        assert_eq!(MapSpec::linear(5.0, -5.0).sanitized(), MapSpec::UNIT);
        assert_eq!(
            MapSpec {
                min: f32::NAN,
                max: 1.0,
                mode: MapMode::Linear,
            }
            .sanitized(),
            MapSpec::UNIT
        );
        // Non-finite input lands on the lower end, never on NaN.
        assert_eq!(MapSpec::linear(0.0, 10.0).map(f32::NAN), 0.0);
        // Out-of-range input clamps instead of extrapolating.
        assert_eq!(MapSpec::linear(0.0, 10.0).map(5.0), 10.0);
    }
}
