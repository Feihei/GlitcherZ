//! 状态变量滤波器（TPT / Simper SVF）。

use std::f32::consts::PI;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;

/// 滤波器输出模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterMode {
    /// 低通（默认）。
    #[default]
    LowPass,
    /// 高通。
    HighPass,
    /// 带通。
    BandPass,
}

/// 状态变量滤波器（零延迟反馈 / TPT 结构，Simper SVF）。
///
/// 二阶（12 dB/oct），同时提供低通/高通/带通输出；共振以 Simper 的
/// `res`（0..1，k = 2 - 2·res）参数化，`res → 1` 时趋于自激。
///
/// 截止频率在处理时钳制到 `[20, min(20000, 0.45·sr)]`；状态在静音时
/// 冲洗反规范数（denormal），避免 CPU 尖峰。所有路径 RT-safe。
///
/// 状态按声道保存，最多 2 声道（与 [`AudioBuffer`] 在本管线中的实际
/// 声道数一致）；超出 2 的声道不做处理。
#[derive(Debug)]
pub struct SvfProcessor {
    sample_rate: f32,
    mode: FilterMode,
    cutoff_hz: f32,
    resonance: f32,
    /// 每声道状态（最多 2）。
    ic1: [f32; 2],
    ic2: [f32; 2],
    /// 每块重算的系数。
    a1: f32,
    a2: f32,
    a3: f32,
    k: f32,
}

/// 防止共振打到自激的上限。
const MAX_RESONANCE: f32 = 0.98;
/// 冲洗反规范数的阈值。
const DENORMAL_EPS: f32 = 1e-18;

impl SvfProcessor {
    /// `sample_rate <= 0` 时回退 48 kHz（构造期容错）。
    pub fn new(sample_rate: f32) -> Self {
        let mut svf = Self {
            sample_rate: if sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            mode: FilterMode::LowPass,
            cutoff_hz: 12_000.0,
            resonance: 0.2,
            ic1: [0.0; 2],
            ic2: [0.0; 2],
            a1: 0.0,
            a2: 0.0,
            a3: 0.0,
            k: 2.0,
        };
        svf.update_coeffs();
        svf
    }

    /// 依据当前参数重算每块系数。RT-safe（纯浮点，无分支爆炸）。
    fn update_coeffs(&mut self) {
        let nyquist = self.sample_rate * 0.5;
        let fc = self
            .cutoff_hz
            .clamp(20.0, 20_000.0)
            .min(nyquist * 0.9)
            .max(1.0);
        let g = (PI * fc / self.sample_rate).tan();
        self.k = (2.0 - 2.0 * self.resonance).max(0.0);
        self.a1 = 1.0 / (1.0 + g * (g + self.k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }

    /// 冲洗反规范数状态。
    fn flush(state: &mut f32) {
        if state.abs() < DENORMAL_EPS {
            *state = 0.0;
        }
    }
}

impl Processor for SvfProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        self.update_coeffs();
        let (a1, a2, a3, k) = (self.a1, self.a2, self.a3, self.k);
        let mode = self.mode;
        for ch in 0..buffer.channels().min(2) {
            let (Some(mut ic1), Some(mut ic2)) = (Some(self.ic1[ch]), Some(self.ic2[ch])) else {
                continue;
            };
            if let Some(channel) = buffer.channel_mut(ch) {
                for sample in channel.iter_mut() {
                    let input = *sample;
                    let v3 = input - ic2;
                    let v1 = a1 * ic1 + a2 * v3;
                    let v2 = ic2 + a2 * ic1 + a3 * v3;
                    ic1 = 2.0 * v1 - ic1;
                    ic2 = 2.0 * v2 - ic2;
                    *sample = match mode {
                        FilterMode::LowPass => v2,
                        // 带通乘 k 归一：中心频率增益恒为 1（模拟原型
                        // s/(s²+ks+1) 的原始带通增益是 1/k，随 Q 变化）。
                        FilterMode::BandPass => v1 * k,
                        FilterMode::HighPass => input - k * v1 - v2,
                    };
                    Self::flush(&mut ic1);
                    Self::flush(&mut ic2);
                }
            }
            self.ic1[ch] = ic1;
            self.ic2[ch] = ic2;
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        if !value.is_finite() {
            return matches!(
                id,
                ParamId::FilterCutoff | ParamId::FilterResonance | ParamId::FilterMode
            );
        }
        match id {
            ParamId::FilterCutoff => {
                self.cutoff_hz = value.clamp(20.0, 20_000.0);
                true
            }
            ParamId::FilterResonance => {
                self.resonance = value.clamp(0.0, MAX_RESONANCE);
                true
            }
            ParamId::FilterMode => {
                self.mode = if value < 0.5 {
                    FilterMode::LowPass
                } else if value < 1.5 {
                    FilterMode::HighPass
                } else {
                    FilterMode::BandPass
                };
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::{FilterMode, SvfProcessor};
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn ctx() -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: 0,
        }
    }

    /// 把 `signal` 按固定块送入滤波器，返回滤波后的信号。
    fn run_through(svf: &mut SvfProcessor, signal: &[f32]) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(BLOCK) {
            buf.set_len(chunk.len());
            buf.channel_mut(0).unwrap().copy_from_slice(chunk);
            svf.process(&ctx(), &mut buf);
            out.extend_from_slice(buf.channel(0).unwrap());
        }
        out
    }

    fn sine(freq: f32, seconds: f32) -> Vec<f32> {
        let n = (SR * seconds) as usize;
        (0..n).map(|i| (TAU * freq * i as f32 / SR).sin()).collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| *s * *s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    // 标准测试 1：静音输入 → 输出静音。
    #[test]
    fn silence_in_silence_out() {
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterResonance, 0.95); // 高共振下也不得自激出声
        let out = run_through(&mut svf, &vec![0.0; BLOCK * 8]);
        assert!(out.iter().all(|s| s.abs() < 1e-9));
    }

    // 标准测试 2：满幅输入 → 无 NaN/Inf。
    #[test]
    fn full_scale_stays_finite_even_at_high_resonance() {
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterCutoff, 500.0);
        svf.set_param(ParamId::FilterResonance, 0.98);
        let signal = sine(1000.0, 1.0);
        let out = run_through(&mut svf, &signal);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(
            out.iter().fold(0.0f32, |a, s| a.max(s.abs())) < 100.0,
            "共振输出应有界"
        );
    }

    #[test]
    fn impulse_at_high_resonance_decays() {
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterCutoff, 800.0);
        svf.set_param(ParamId::FilterResonance, 0.98);
        let mut signal = vec![0.0; SR as usize];
        signal[0] = 1.0;
        let out = run_through(&mut svf, &signal);
        let peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(
            peak.is_finite() && peak < 10.0,
            "冲激响应应有界，峰值 {peak}"
        );
        let tail = &out[out.len() - BLOCK..];
        assert!(
            tail.iter().all(|s| s.abs() < 1e-3),
            "高 Q 冲激响应应衰减干净"
        );
    }

    #[test]
    fn low_pass_attenuates_above_cutoff_and_passes_below() {
        // 通带：440 Hz @ fc 8000 → 基本不衰减。
        // （正弦 RMS = 0.707；尾部窗口约 1.17 个周期，再留余量。）
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterCutoff, 8_000.0);
        svf.set_param(ParamId::FilterResonance, 0.1);
        let passed = run_through(&mut svf, &sine(440.0, 0.5));
        let tail_rms = rms(&passed[passed.len() - BLOCK..]);
        assert!(tail_rms > 0.6, "通带 RMS 应接近 0.707，实际 {tail_rms}");

        // 阻带：1000 Hz @ fc 200 → 二阶强衰减。
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterCutoff, 200.0);
        svf.set_param(ParamId::FilterResonance, 0.1);
        let blocked = run_through(&mut svf, &sine(1_000.0, 0.5));
        let tail_rms = rms(&blocked[blocked.len() - BLOCK..]);
        assert!(tail_rms < 0.25, "阻带 RMS 应远小于 0.707，实际 {tail_rms}");
    }

    #[test]
    fn high_pass_blocks_low_frequencies() {
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterMode, 1.0); // HighPass
        svf.set_param(ParamId::FilterCutoff, 2_000.0);
        svf.set_param(ParamId::FilterResonance, 0.1);
        let blocked = run_through(&mut svf, &sine(200.0, 0.5));
        let tail_rms = rms(&blocked[blocked.len() - BLOCK..]);
        assert!(tail_rms < 0.2, "高通对低频应强衰减，实际 {tail_rms}");
    }

    #[test]
    fn band_pass_peaks_at_center_frequency() {
        let mut svf = SvfProcessor::new(SR);
        svf.set_param(ParamId::FilterMode, 2.0); // BandPass
        svf.set_param(ParamId::FilterCutoff, 440.0);
        svf.set_param(ParamId::FilterResonance, 0.3);
        let centered = run_through(&mut svf, &sine(440.0, 0.5));
        let center_rms = rms(&centered[centered.len() - BLOCK..]);
        assert!(
            center_rms > 0.6,
            "带通中心增益归一后应接近 0.707，实际 {center_rms}"
        );

        // 远端（4000 Hz）按一阶 -6 dB/oct 滚降。
        let far = run_through(&mut svf, &sine(4_000.0, 0.5));
        let far_rms = rms(&far[far.len() - BLOCK..]);
        assert!(far_rms < 0.2, "带通对远端频率应明显衰减，实际 {far_rms}");
    }

    #[test]
    fn dc_gain_is_unity_for_low_pass_and_zero_for_high_pass() {
        let n = SR as usize; // 1 秒，充分收敛

        let mut lp = SvfProcessor::new(SR);
        lp.set_param(ParamId::FilterCutoff, 1_000.0);
        let out = run_through(&mut lp, &vec![1.0; n]);
        assert!(
            (out[out.len() - 1] - 1.0).abs() < 1e-3,
            "低通直流增益应为 1，实际 {}",
            out[out.len() - 1]
        );

        let mut hp = SvfProcessor::new(SR);
        hp.set_param(ParamId::FilterMode, 1.0);
        hp.set_param(ParamId::FilterCutoff, 1_000.0);
        let out = run_through(&mut hp, &vec![1.0; n]);
        assert!(
            out[out.len() - 1].abs() < 1e-3,
            "高通直流增益应为 0，实际 {}",
            out[out.len() - 1]
        );
    }

    #[test]
    fn params_are_clamped() {
        let mut svf = SvfProcessor::new(SR);
        assert!(svf.set_param(ParamId::FilterCutoff, -100.0));
        assert!((svf.cutoff_hz - 20.0).abs() < 1e-6);
        svf.set_param(ParamId::FilterCutoff, 1_000_000.0);
        assert!((svf.cutoff_hz - 20_000.0).abs() < 1e-3);

        svf.set_param(ParamId::FilterResonance, 5.0);
        assert!((svf.resonance - 0.98).abs() < 1e-6);

        svf.set_param(ParamId::FilterMode, 1.0);
        assert_eq!(svf.mode, FilterMode::HighPass);
        assert!(
            !svf.set_param(ParamId::MasterGain, 1.0),
            "不响应的参数应返回 false"
        );
    }
}
