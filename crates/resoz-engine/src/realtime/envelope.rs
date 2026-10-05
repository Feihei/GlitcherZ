//! ADSR 音量包络。

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;

/// 包络阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// 静默（无音符），输出 0。
    Idle,
    /// 从当前电平升至峰值。
    Attack,
    /// 从峰值降至 sustain 电平。
    Decay,
    /// 维持 sustain 电平。
    Sustain,
    /// 从当前电平衰减回 0。
    Release,
}

/// ADSR 音量包络 —— 原地乘性效果器。
///
/// 参数（均经 [`Processor::set_param`] 在块边界更新）：
/// Attack / Decay / Release 为秒（0..10），Sustain 为 0..1 的相对电平。
/// 峰值 = velocity（0..1，随 `NoteOn` 下发）。
///
/// 实现细节：各阶段为**线性斜率**——Attack 恰在 A 秒末到达峰值，
/// Decay 恰在 D 秒末到达 sustain，Release 从进入时的电平出发恰在
/// R 秒末归零；时间参数为 0 的阶段在同一样本内直接跳过。所有路径
/// RT-safe：无分配、无锁、无 panic。
#[derive(Debug)]
pub struct AdsrProcessor {
    sample_rate: f32,
    attack_s: f32,
    decay_s: f32,
    sustain: f32,
    release_s: f32,
    velocity: f32,
    stage: Stage,
    /// 当前包络电平，始终在 `[0, velocity]` 内。
    level: f32,
    /// 进入 Release 时的每样本衰减量（保证 R 秒内恰好归零）。
    release_rate: f32,
}

/// 各阶段时间参数的允许上限（秒）。
const MAX_TIME_S: f32 = 10.0;

impl AdsrProcessor {
    /// `sample_rate <= 0` 时回退 48 kHz（构造期容错）。
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate: if sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            attack_s: 0.01,
            decay_s: 0.10,
            sustain: 0.8,
            release_s: 0.20,
            velocity: 1.0,
            stage: Stage::Idle,
            level: 0.0,
            release_rate: 0.0,
        }
    }

    /// 当前包络电平。RT-safe。
    pub fn level(&self) -> f32 {
        self.level
    }

    /// 触发音符（进入 Attack，从当前电平续起 —— 释放中重触发即再起音）。
    /// RT-safe。
    fn trigger(&mut self, velocity: f32) {
        self.velocity = if velocity.is_finite() {
            velocity.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.level = self.level.clamp(0.0, self.velocity);
        self.stage = Stage::Attack;
    }

    /// 松开音符（进入 Release；Idle 下无效）。RT-safe。
    fn release(&mut self) {
        if self.stage != Stage::Idle {
            self.release_rate = if self.release_s > 1e-6 && self.level > 0.0 {
                self.level / (self.release_s * self.sample_rate)
            } else {
                0.0
            };
            self.stage = Stage::Release;
        }
    }

    /// 推进一个样本并返回包络电平。零时长的阶段在同一样本内跳过。
    /// RT-safe。
    fn next_level(&mut self) -> f32 {
        let peak = self.velocity;
        loop {
            match self.stage {
                Stage::Idle => return 0.0,
                Stage::Attack => {
                    if self.attack_s <= 1e-6 {
                        self.level = peak;
                        self.stage = Stage::Decay;
                        continue;
                    }
                    let step = peak / (self.attack_s * self.sample_rate);
                    self.level = (self.level + step).min(peak);
                    if self.level >= peak {
                        self.stage = Stage::Decay;
                    }
                    return self.level;
                }
                Stage::Decay => {
                    let target = peak * self.sustain;
                    if self.decay_s <= 1e-6 {
                        self.level = target;
                        self.stage = Stage::Sustain;
                        continue;
                    }
                    let step = (peak - target) / (self.decay_s * self.sample_rate);
                    self.level = (self.level - step).max(target);
                    if self.level <= target {
                        self.stage = Stage::Sustain;
                    }
                    return self.level;
                }
                Stage::Sustain => {
                    // 每样本重算：sustain / velocity 中途变化也能跟随。
                    self.level = peak * self.sustain;
                    return self.level;
                }
                Stage::Release => {
                    if self.release_rate <= 0.0
                        || !self.release_rate.is_finite()
                        || self.level <= 0.0
                    {
                        self.level = 0.0;
                        self.stage = Stage::Idle;
                        continue;
                    }
                    self.level = (self.level - self.release_rate).max(0.0);
                    if self.level <= 0.0 {
                        self.stage = Stage::Idle;
                    }
                    return self.level;
                }
            }
        }
    }
}

impl Processor for AdsrProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        for i in 0..frames {
            let gain = self.next_level();
            for ch in 0..buffer.channels() {
                if let Some(slot) = buffer.channel_mut(ch).and_then(|c| c.get_mut(i)) {
                    *slot *= gain;
                }
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        if !value.is_finite() {
            return matches!(
                id,
                ParamId::Attack | ParamId::Decay | ParamId::Sustain | ParamId::Release
            );
        }
        match id {
            ParamId::Attack => {
                self.attack_s = value.clamp(0.0, MAX_TIME_S);
                true
            }
            ParamId::Decay => {
                self.decay_s = value.clamp(0.0, MAX_TIME_S);
                true
            }
            ParamId::Sustain => {
                self.sustain = value.clamp(0.0, 1.0);
                true
            }
            ParamId::Release => {
                self.release_s = value.clamp(0.0, MAX_TIME_S);
                true
            }
            _ => false,
        }
    }

    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { velocity, .. } => {
                self.trigger(velocity);
                true
            }
            NoteEvent::Off { .. } => {
                self.release();
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AdsrProcessor;
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::{NoteEvent, ParamId};
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

    /// 满幅输入下渲染 `blocks` 个块，返回包络作用后的第 0 声道。
    fn render_with_full_scale(env: &mut AdsrProcessor, blocks: usize) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::new();
        for _ in 0..blocks {
            buf.set_len(BLOCK);
            buf.channel_mut(0).unwrap().fill(1.0);
            env.process(&ctx(), &mut buf);
            out.extend_from_slice(buf.channel(0).unwrap());
        }
        out
    }

    fn note_on() -> NoteEvent {
        NoteEvent::On {
            note: 69,
            velocity: 1.0,
        }
    }

    fn note_off() -> NoteEvent {
        NoteEvent::Off { note: 69 }
    }

    // 标准测试 1：静音输入 → 输出静音（即使包络处于 Attack）。
    #[test]
    fn silence_in_silence_out() {
        let mut env = AdsrProcessor::new(SR);
        env.note_event(note_on());
        let mut buf = AudioBuffer::new(2, BLOCK);
        buf.set_len(BLOCK);
        buf.clear();
        env.process(&ctx(), &mut buf);
        for ch in 0..2 {
            assert!(buf.channel(ch).unwrap().iter().all(|s| s.abs() < 1e-12));
        }
    }

    // 标准测试 2：满幅输入 → 无 NaN/Inf，且电平不越界。
    #[test]
    fn full_scale_stays_bounded_and_finite() {
        let mut env = AdsrProcessor::new(SR);
        env.note_event(note_on());
        let out = render_with_full_scale(&mut env, 500);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(
            out.iter().all(|s| (0.0..=1.0).contains(s)),
            "包络电平应位于 [0,1]"
        );
    }

    #[test]
    fn attack_reaches_peak_in_configured_time() {
        let mut env = AdsrProcessor::new(SR);
        env.set_param(ParamId::Attack, 0.1); // 4800 samples
        env.set_param(ParamId::Decay, 10.0); // 压低衰减速率，便于观测
        env.set_param(ParamId::Sustain, 1.0);
        env.note_event(note_on());

        let out = render_with_full_scale(&mut env, 50); // 6000 samples
        let at_4800 = out[4_799];
        assert!(
            (at_4800 - 1.0).abs() < 0.05,
            "attack 结束时应到达峰值，实际 {at_4800}"
        );
        let early = out[479]; // 1% 处应仍很小
        assert!(early < 0.2, "attack 起始应接近 0，实际 {early}");
    }

    #[test]
    fn release_decays_to_silence() {
        let mut env = AdsrProcessor::new(SR);
        env.set_param(ParamId::Attack, 0.0);
        env.set_param(ParamId::Decay, 0.0);
        env.set_param(ParamId::Sustain, 0.8);
        env.set_param(ParamId::Release, 0.2);
        env.note_event(note_on());
        let _ = render_with_full_scale(&mut env, 20); // 2560 样本，早已进入 sustain

        env.note_event(note_off());
        let out = render_with_full_scale(&mut env, 80); // 10240 样本 > release 0.2 s
        let tail = &out[out.len() - BLOCK..];
        assert!(
            tail.iter().all(|s| s.abs() < 0.01),
            "release 结束后应静音，尾部 {tail:?}"
        );
    }

    #[test]
    fn retrigger_after_partial_release_jumps_back_up() {
        let mut env = AdsrProcessor::new(SR);
        env.set_param(ParamId::Attack, 0.0);
        env.set_param(ParamId::Decay, 0.0);
        env.set_param(ParamId::Sustain, 0.8);
        env.set_param(ParamId::Release, 1.0);
        env.note_event(note_on());
        let _ = render_with_full_scale(&mut env, 20);

        env.note_event(note_off());
        let during_release = render_with_full_scale(&mut env, 4); // 释放中
        assert!(during_release[during_release.len() - 1] < 0.8);

        env.note_event(note_on()); // 释放中重触发
        let retriggered = render_with_full_scale(&mut env, 2);
        assert!(
            retriggered[retriggered.len() - 1] > during_release[during_release.len() - 1],
            "重触发后电平应回升"
        );
    }

    #[test]
    fn params_are_clamped_and_nan_velocity_defaults_full() {
        let mut env = AdsrProcessor::new(SR);
        assert!(env.set_param(ParamId::Attack, -5.0));
        assert!(env.set_param(ParamId::Sustain, 5.0));
        assert!(
            !env.set_param(ParamId::MasterGain, 1.0),
            "不响应的参数应返回 false"
        );

        env.set_param(ParamId::Attack, 0.0);
        env.set_param(ParamId::Decay, 0.0);
        env.note_event(NoteEvent::On {
            note: 60,
            velocity: f32::NAN, // 非有限力度应回退为 1.0
        });
        let out = render_with_full_scale(&mut env, 1);
        assert!((out[out.len() - 1] - 1.0).abs() < 1e-6, "峰值应为 1.0");
    }

    #[test]
    fn zero_attack_and_decay_jump_directly_to_sustain() {
        let mut env = AdsrProcessor::new(SR);
        env.set_param(ParamId::Attack, 0.0);
        env.set_param(ParamId::Decay, 0.0);
        env.set_param(ParamId::Sustain, 0.5);
        env.note_event(note_on());
        let out = render_with_full_scale(&mut env, 1);
        assert!(
            (out[0] - 0.5).abs() < 1e-6,
            "A/D 为 0 时第一样本应即 sustain 电平"
        );
    }
}
