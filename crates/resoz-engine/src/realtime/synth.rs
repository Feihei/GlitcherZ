//! 完整合成器：振荡器 → ADSR 包络（单声部）。

use crate::convert;
use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::envelope::AdsrProcessor;
use crate::realtime::processor::Processor;
use crate::realtime::sine::SineProcessor;

/// MIDI 音符编号 → 频率（Hz）。A4（69）= 440 Hz。
/// 音源/UI 层传值，非逐样本热路径（f64 计算后经 convert 转 f32）。
pub fn note_to_frequency(note: u8) -> f32 {
    convert::to_f32(440.0 * 2.0f64.powf((note as f64 - 69.0) / 12.0))
}

/// 单声部合成器：正弦振荡器 + ADSR 包络。
///
/// 音符语义（MIDI）：
/// - `On { note, velocity }`：振荡器立即改频，包络从当前电平进入 Attack
///   （释放中重触发即再起音）；
/// - `Off { .. }`：包络进入 Release（单声部实现忽略具体音符）。
///
/// 滤波器不内置 —— 由处理链在其后串联 [`SvfProcessor`](crate::realtime::SvfProcessor)。
pub struct SynthProcessor {
    osc: SineProcessor,
    env: AdsrProcessor,
}

impl SynthProcessor {
    /// `sample_rate <= 0` 时内部模块各自回退 48 kHz（构造期容错）。
    ///
    /// 振荡器增益归一为 1：声部振幅完全由包络（velocity → sustain）决定，
    /// 混响着的主增益/滤波器在链路下游。
    pub fn new(sample_rate: f32) -> Self {
        let mut osc = SineProcessor::new(sample_rate);
        osc.set_param(ParamId::SineGain, 1.0);
        Self {
            osc,
            env: AdsrProcessor::new(sample_rate),
        }
    }

    /// 当前振荡器频率（Hz）。RT-safe。
    pub fn frequency(&self) -> f32 {
        self.osc.frequency()
    }

    /// 当前包络电平（电平表/可视化用）。RT-safe。
    pub fn envelope_level(&self) -> f32 {
        self.env.level()
    }
}

impl Processor for SynthProcessor {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        self.osc.process(ctx, buffer);
        self.env.process(ctx, buffer);
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::SineFrequency | ParamId::SineGain => self.osc.set_param(id, value),
            ParamId::Attack | ParamId::Decay | ParamId::Sustain | ParamId::Release => {
                self.env.set_param(id, value)
            }
            _ => false,
        }
    }

    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { note, velocity } => {
                self.osc.set_frequency(note_to_frequency(note));
                self.env.note_event(NoteEvent::On { note, velocity })
            }
            NoteEvent::Off { .. } => self.env.note_event(event),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SynthProcessor, note_to_frequency};
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::{NoteEvent, ParamId};
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;
    use crate::testing::count_positive_crossings;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn ctx() -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: 0,
        }
    }

    fn render(synth: &mut SynthProcessor, blocks: usize) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::new();
        for _ in 0..blocks {
            buf.set_len(BLOCK);
            buf.clear();
            synth.process(&ctx(), &mut buf);
            out.extend_from_slice(buf.channel(0).unwrap());
        }
        out
    }

    fn note_on(note: u8) -> NoteEvent {
        NoteEvent::On {
            note,
            velocity: 1.0,
        }
    }

    #[test]
    fn note_to_frequency_table() {
        assert!((note_to_frequency(69) - 440.0).abs() < 0.01, "A4 = 440 Hz");
        assert!(
            (note_to_frequency(60) - 261.63).abs() < 0.05,
            "C4 ≈ 261.63 Hz"
        );
        assert!((note_to_frequency(81) - 880.0).abs() < 0.02, "A5 = 880 Hz");
    }

    // 标准测试 1：无音符时输出静音。
    #[test]
    fn silent_without_note() {
        let mut synth = SynthProcessor::new(SR);
        let out = render(&mut synth, 20);
        assert!(out.iter().all(|s| s.abs() < 1e-12));
    }

    #[test]
    fn note_on_plays_note_frequency() {
        let mut synth = SynthProcessor::new(SR);
        synth.set_param(ParamId::Attack, 0.0);
        synth.set_param(ParamId::Decay, 0.0);
        synth.set_param(ParamId::Sustain, 0.8);
        assert!(synth.note_event(note_on(69)), "合成器应消费音符事件");

        // 跳过 attack 无关——A=0 直接进入 sustain；1 秒 @ A4 → 440 次过零。
        let out = render(&mut synth, SR as usize / BLOCK);
        let crossings = count_positive_crossings(&out);
        assert!(
            (crossings as i64 - 440).abs() <= 2,
            "crossings = {crossings}"
        );
        let peak = out.iter().fold(0.0f32, |a, s| a.max(*s));
        assert!(
            (peak - 0.8).abs() < 0.05,
            "sustain 电平应约 0.8，实际 {peak}"
        );
    }

    #[test]
    fn note_off_fades_to_silence() {
        let mut synth = SynthProcessor::new(SR);
        synth.set_param(ParamId::Attack, 0.0);
        synth.set_param(ParamId::Decay, 0.0);
        synth.set_param(ParamId::Sustain, 0.8);
        synth.set_param(ParamId::Release, 0.2);
        synth.note_event(note_on(60));
        let _ = render(&mut synth, 20);

        synth.note_event(NoteEvent::Off { note: 60 });
        let out = render(&mut synth, 80); // 10240 样本 > release 0.2 s
        let tail = &out[out.len() - BLOCK..];
        assert!(tail.iter().all(|s| s.abs() < 0.01), "松键后应衰减至静音");
    }

    // 标准测试 2：极端力度/频率下输出无 NaN/Inf。
    #[test]
    fn extreme_params_stay_finite() {
        let mut synth = SynthProcessor::new(SR);
        synth.set_param(ParamId::SineFrequency, SR / 2.0);
        synth.note_event(NoteEvent::On {
            note: 127,
            velocity: f32::NAN,
        });
        let out = render(&mut synth, 40);
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn params_route_to_osc_and_envelope() {
        let mut synth = SynthProcessor::new(SR);
        assert!(synth.set_param(ParamId::SineFrequency, 220.0));
        assert!((synth.frequency() - 220.0).abs() < 1e-3);
        assert!(synth.set_param(ParamId::Attack, 0.5));
        assert!(synth.set_param(ParamId::Release, 0.5));
        assert!(
            !synth.set_param(ParamId::MasterGain, 1.0),
            "主增益不归合成器管"
        );
        assert!(
            !synth.set_param(ParamId::FilterCutoff, 1000.0),
            "滤波器不归合成器管"
        );
    }
}
