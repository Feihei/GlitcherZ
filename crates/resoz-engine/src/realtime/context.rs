//! 每块处理上下文。

/// 一个固定处理块的上下文。`Copy`，由音频线程在每个块边界构造。
#[derive(Debug, Clone, Copy)]
pub struct ProcessContext {
    /// 采样率（Hz）。
    pub sample_rate: f32,
    /// 本块样本数（≤ `AudioBuffer` 容量）。
    pub frames: usize,
    /// 本块第一个样本的全局位置（自音频流启动起的累计样本数）。
    pub playback_sample_pos: u64,
}

impl ProcessContext {
    /// 本块起始时间（秒）。时间计算统一用 f64（见 AGENTS.md「编码规范」）。
    pub fn start_seconds(&self) -> f64 {
        crate::convert::samples_to_seconds(self.playback_sample_pos, self.sample_rate)
    }
}

#[cfg(test)]
mod tests {
    use super::ProcessContext;

    #[test]
    fn start_seconds_uses_f64_math() {
        let ctx = ProcessContext {
            sample_rate: 48_000.0,
            frames: 128,
            playback_sample_pos: 48_000,
        };
        assert!((ctx.start_seconds() - 1.0).abs() < 1e-12);
    }
}
