//! 正弦波生成器。

use std::f32::consts::TAU;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;

/// 正弦波生成器（路线图第 1 步的演示音源）。
///
/// 相位以 0..1 归一化累加并在每样本后包裹，避免大数值的浮点精度损失；
/// 跨块相位连续（见单元测试）。
pub struct SineProcessor {
    sample_rate: f32,
    phase: f32,
    frequency_hz: f32,
    gain: f32,
}

impl SineProcessor {
    /// `sample_rate <= 0` 时回退 48 kHz（构造期容错，不在实时路径上）。
    pub fn new(sample_rate: f32) -> Self {
        debug_assert!(sample_rate > 0.0, "sample_rate 必须为正");
        Self {
            sample_rate: if sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            phase: 0.0,
            frequency_hz: 440.0,
            gain: 0.25,
        }
    }

    /// 当前频率（Hz）。RT-safe。
    pub fn frequency(&self) -> f32 {
        self.frequency_hz
    }

    /// 当前增益（线性）。RT-safe。
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// 设置频率：钳制到 `[0, sample_rate/2]`（奈奎斯特），非有限值忽略。
    /// RT-safe。
    fn set_frequency(&mut self, hz: f32) {
        if hz.is_finite() {
            self.frequency_hz = hz.clamp(0.0, self.sample_rate * 0.5);
        }
    }

    /// 设置增益：钳制到 `[0, 4]`（防炸耳/削波），非有限值忽略。RT-safe。
    fn set_gain(&mut self, gain: f32) {
        if gain.is_finite() {
            self.gain = gain.clamp(0.0, 4.0);
        }
    }
}

impl Processor for SineProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        if frames == 0 || buffer.channels() == 0 {
            return;
        }
        let inc = (self.frequency_hz / self.sample_rate).clamp(0.0, 0.5);
        let gain = self.gain;
        let mut phase = self.phase;

        // 先生成到第 0 声道，再复制到其余声道，避免逐声道重复计算 sin。
        if let Some(ch0) = buffer.channel_mut(0) {
            for slot in ch0.iter_mut() {
                *slot = (TAU * phase).sin() * gain;
                phase += inc;
                if phase >= 1.0 {
                    phase -= 1.0;
                }
            }
        }
        for ch in 1..buffer.channels() {
            // 逐声道复制：先取只读样本，再取可变槽位（避免同时持有两个借用）。
            for i in 0..frames {
                let sample = buffer.channel(0).and_then(|c| c.get(i)).copied();
                if let (Some(s), Some(dst)) =
                    (sample, buffer.channel_mut(ch).and_then(|c| c.get_mut(i)))
                {
                    *dst = s;
                }
            }
        }
        self.phase = phase;
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::SineFrequency => {
                self.set_frequency(value);
                true
            }
            ParamId::SineGain => {
                self.set_gain(value);
                true
            }
            ParamId::MasterGain => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SineProcessor;
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;
    use crate::testing::count_positive_crossings;

    const SR: f32 = 44_100.0;

    fn ctx(frames: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames,
            playback_sample_pos: 0,
        }
    }

    /// 按固定块渲染 `blocks` 个块，返回第 0 声道的输出。
    fn render(sine: &mut SineProcessor, blocks: usize) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, 128);
        let mut collected = Vec::new();
        for _ in 0..blocks {
            buf.set_len(128);
            buf.clear();
            sine.process(&ctx(128), &mut buf);
            collected.extend_from_slice(buf.channel(0).unwrap());
        }
        collected
    }

    #[test]
    fn frequency_matches_zero_crossings() {
        let mut sine = SineProcessor::new(SR);
        // 344 块 ≈ 0.9985 s @ 440 Hz → 约 439 次正向过零。
        let crossings = count_positive_crossings(&render(&mut sine, 344));
        assert!(
            (crossings as i64 - 440).abs() <= 2,
            "crossings = {crossings}"
        );
    }

    #[test]
    fn amplitude_matches_gain() {
        let mut sine = SineProcessor::new(SR);
        let mut buf = AudioBuffer::new(1, 256);
        buf.set_len(256);
        buf.clear();
        sine.process(&ctx(256), &mut buf);
        let peak = buf
            .channel(0)
            .unwrap()
            .iter()
            .fold(0.0f32, |acc, s| acc.max(*s));
        assert!((peak - 0.25).abs() < 1e-3, "peak = {peak}");
    }

    #[test]
    fn full_scale_output_is_finite() {
        let mut sine = SineProcessor::new(SR);
        sine.set_param(ParamId::SineGain, 4.0);
        sine.set_param(ParamId::SineFrequency, SR / 2.0 - 1_000.0);
        let mut buf = AudioBuffer::new(2, 128);
        for _ in 0..1_000 {
            buf.set_len(128);
            buf.clear();
            sine.process(&ctx(128), &mut buf);
            for ch in 0..2 {
                assert!(buf.channel(ch).unwrap().iter().all(|s| s.is_finite()));
            }
        }
    }

    #[test]
    fn params_are_clamped_and_nan_ignored() {
        let mut sine = SineProcessor::new(SR);
        sine.set_param(ParamId::SineFrequency, -50.0);
        assert!(sine.frequency().abs() < 1e-9);

        sine.set_param(ParamId::SineFrequency, f32::NAN);
        assert!(sine.frequency().abs() < 1e-9, "非有限值应被忽略");

        sine.set_param(ParamId::SineFrequency, SR);
        assert!((sine.frequency() - SR / 2.0).abs() < 1e-3, "钳制到奈奎斯特");

        sine.set_param(ParamId::SineGain, f32::INFINITY);
        assert!((sine.gain() - 0.25).abs() < 1e-9, "非有限增益应被忽略");

        assert!(
            !sine.set_param(ParamId::MasterGain, 1.0),
            "不响应的参数应返回 false"
        );
    }

    #[test]
    fn phase_continuous_across_blocks() {
        // 分块渲染与整段渲染的过零数应一致（±1）。
        let mut sine = SineProcessor::new(SR);
        let chunked = count_positive_crossings(&render(&mut sine, 50));

        let mut sine = SineProcessor::new(SR);
        let mut buf = AudioBuffer::new(1, 128 * 50);
        buf.set_len(128 * 50);
        buf.clear();
        sine.process(&ctx(128 * 50), &mut buf);
        let whole = count_positive_crossings(buf.channel(0).unwrap());

        assert!(
            (chunked as i64 - whole as i64).abs() <= 1,
            "chunked = {chunked}, whole = {whole}"
        );
    }
}
