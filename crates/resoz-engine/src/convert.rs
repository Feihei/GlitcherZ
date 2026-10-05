//! f32（音频路径）与 f64（UI / 时间计算）之间转换的集中点。
//!
//! 约定见 AGENTS.md「编码规范」：所有跨精度转换都应经过本模块，
//! 便于审查与统一舍入行为。

/// `f64 → f32`。
pub fn to_f32(value: f64) -> f32 {
    value as f32
}

/// `f32 → f64`。
pub fn to_f64(value: f32) -> f64 {
    value as f64
}

/// 采样率 u32 → f32（44100 / 48000 等常用采样率精确无损）。
pub fn sample_rate_to_f32(sample_rate: u32) -> f32 {
    sample_rate as f32
}

/// 采样点数 → 秒（时间计算用 f64）。`sample_rate <= 0` 时返回 0。
pub fn samples_to_seconds(samples: u64, sample_rate: f32) -> f64 {
    if sample_rate > 0.0 {
        samples as f64 / f64::from(sample_rate)
    } else {
        0.0
    }
}

/// 秒 → 采样点数（向下取整）。输入非法（采样率非正或时间为负）时返回 0。
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
