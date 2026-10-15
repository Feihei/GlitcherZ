//! Shared unit-test utilities (compiled only for test builds).

/// Counts positive zero crossings (prev ≤ 0 and current > 0), used to verify
/// sine frequency: a sine wave at `f Hz` crosses zero upward exactly `f`
/// times per second.
pub(crate) fn count_positive_crossings(samples: &[f32]) -> usize {
    let mut count = 0;
    let mut prev = 0.0f32;
    for sample in samples {
        if prev <= 0.0 && *sample > 0.0 {
            count += 1;
        }
        prev = *sample;
    }
    count
}
