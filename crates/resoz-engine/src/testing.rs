//! 单元测试共享工具（仅测试构建编译）。

/// 统计正向过零次数（prev ≤ 0 且当前 > 0），用于验证正弦频率：
/// `f Hz` 的正弦波每秒恰好 `f` 次正向过零。
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
