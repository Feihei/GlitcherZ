//! Central point for conversions between f32 (audio path) and f64 (UI / time
//! calculations).
//!
//! See AGENTS.md "Coding Standards" for the convention: all cross-precision
//! conversions should go through this module for easier review and uniform
//! rounding behavior.

/// `f64 → f32`.
pub fn to_f32(value: f64) -> f32 {
    value as f32
}

/// `f32 → f64`.
pub fn to_f64(value: f32) -> f64 {
    value as f64
}

/// Sample rate u32 → f32 (common sample rates such as 44100 / 48000 convert
/// losslessly and exactly).
pub fn sample_rate_to_f32(sample_rate: u32) -> f32 {
    sample_rate as f32
}

/// Sample count → seconds (time calculations use f64). Returns 0 when
/// `sample_rate <= 0`.
pub fn samples_to_seconds(samples: u64, sample_rate: f32) -> f64 {
    if sample_rate > 0.0 {
        samples as f64 / f64::from(sample_rate)
    } else {
        0.0
    }
}

/// Seconds → sample count (rounded down). Returns 0 for invalid input
/// (non-positive sample rate or negative time).
pub fn seconds_to_samples(seconds: f64, sample_rate: f32) -> u64 {
    if sample_rate > 0.0 && seconds > 0.0 {
        (seconds * f64::from(sample_rate)) as u64
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precision_conversions() {
        assert_eq!(to_f32(1.0f64), 1.0f32);
        assert_eq!(to_f64(0.5f32), 0.5f64);
    }

    #[test]
    fn samples_and_seconds() {
        assert!((samples_to_seconds(44_100, 44_100.0) - 1.0).abs() < 1e-12);
        assert_eq!(seconds_to_samples(1.0, 48_000.0), 48_000);
    }

    #[test]
    fn degenerate_inputs_are_safe() {
        assert_eq!(seconds_to_samples(-1.0, 48_000.0), 0);
        assert_eq!(samples_to_seconds(960, 0.0), 0.0);
    }

    #[test]
    fn sample_rate_conversion() {
        assert_eq!(sample_rate_to_f32(44_100), 44_100.0);
        assert_eq!(sample_rate_to_f32(48_000), 48_000.0);
    }
}
