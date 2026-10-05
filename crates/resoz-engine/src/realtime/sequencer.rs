//! 步进音序器（音频线程侧，块边界采样时钟调度）。

use crate::realtime::commands::NoteEvent;
use crate::realtime::context::ProcessContext;

/// 音序器步数。
pub const PATTERN_STEPS: usize = 16;

/// 单个步进的音频线程侧表示。
///
/// 保持 `Copy`：整条模式（16 步 ≈ 128 字节）可作为一个命令经无锁队列
/// 整块传递，实时侧零分配。与 `resoz_model::Step` 互为镜像 —— 两个
/// crate 互不依赖，由 UI/应用层做转换（见 AGENTS.md「数据模型」）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SequencerStep {
    /// 该步是否触发。
    pub enabled: bool,
    /// MIDI 音符编号。
    pub note: u8,
    /// 力度（0..1）。
    pub velocity: f32,
}

/// 步进音序器：按采样时钟在块边界把到期步进转成 [`NoteEvent`]。
///
/// 时钟以"流启动以来的绝对采样位置"计（与
/// [`ProcessContext::playback_sample_pos`] 同一坐标系）。
///
/// 精度说明：事件在包含其时刻的**块边界**统一发出（≤ 1 块 ≈ 2.7 ms
/// 抖动 @48 kHz / 128 块），逐样本偏移留待后续版本支持。
///
/// 所有方法 RT-safe：无分配、无锁、无 panic（`advance` 的事件经回调
/// 送出，不收集、不堆分配）。
pub struct Sequencer {
    sample_rate: f32,
    playing: bool,
    bpm: f32,
    /// gate 占步长的比例（0.05..1）。
    gate: f32,
    pattern: [SequencerStep; PATTERN_STEPS],
    step_index: usize,
    /// 最近一次触发步进的时刻（BPM 变化时用于重推下一拍）。
    last_step_sample: u64,
    next_step_sample: u64,
    /// 待发送的 NoteOff（gate 结束时刻 + 音符）。单声部：同一时刻最多一个。
    pending_off: Option<(u64, u8)>,
    /// 最后触发的音符（`stop` 时交给调用方释放包络）。
    last_note: Option<u8>,
}

impl Sequencer {
    /// `sample_rate <= 0` 时回退 48 kHz（构造期容错）。
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate: if sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            playing: false,
            bpm: 120.0,
            gate: 0.8,
            pattern: [SequencerStep::default(); PATTERN_STEPS],
            step_index: 0,
            last_step_sample: 0,
            next_step_sample: 0,
            pending_off: None,
            last_note: None,
        }
    }

    /// 是否播放中。RT-safe。
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// 当前步索引（0..16）。RT-safe。
    pub fn step_index(&self) -> usize {
        self.step_index
    }

    /// 16 分音符的长度（样本数）。
    fn step_len_samples(&self) -> u64 {
        let bpm = if self.bpm.is_finite() {
            self.bpm.clamp(20.0, 300.0)
        } else {
            120.0
        };
        // 中间计算用 f64（时间计算约定），最终折算为样本数。
        let seconds_per_step = 60.0_f64 / f64::from(bpm) / 4.0;
        (seconds_per_step * f64::from(self.sample_rate)).max(1.0) as u64
    }

    /// 从 `at_sample` 起从第 0 步开始播放。RT-safe。
    pub fn play(&mut self, at_sample: u64) {
        self.playing = true;
        self.step_index = 0;
        self.last_step_sample = at_sample;
        self.next_step_sample = at_sample;
        self.pending_off = None;
    }

    /// 停止调度。返回最后触发中的音符，调用方应据此发 `NoteOff`
    /// 释放包络（`None` 表示没有音符在响）。RT-safe。
    pub fn stop(&mut self) -> Option<u8> {
        self.playing = false;
        self.pending_off = None;
        self.last_note.take()
    }

    /// 设置 BPM（20..300，非有限值忽略）。播放中会保持音乐位置重推下一拍。
    /// RT-safe。
    pub fn set_bpm(&mut self, bpm: f32) {
        if !bpm.is_finite() {
            return;
        }
        self.bpm = bpm.clamp(20.0, 300.0);
        if self.playing {
            self.next_step_sample = self.last_step_sample + self.step_len_samples();
        }
    }

    /// 设置 gate 占比（0.05..1，非有限值忽略）。RT-safe。
    pub fn set_gate(&mut self, gate: f32) {
        if gate.is_finite() {
            self.gate = gate.clamp(0.05, 1.0);
        }
    }

    /// 整块替换模式。RT-safe。
    pub fn set_pattern(&mut self, pattern: [SequencerStep; PATTERN_STEPS]) {
        self.pattern = pattern;
    }

    /// 推进一个块：把到期的 gate off 与步进 on 经 `emit` 送出
    /// （同一块内先 off 后 on）。RT-safe。
    pub fn advance(&mut self, ctx: &ProcessContext, emit: &mut dyn FnMut(NoteEvent)) {
        if !self.playing {
            return;
        }
        let block_end = ctx.playback_sample_pos + ctx.frames as u64;

        // 1. 到期的 gate off（先于本块的 note on，保证单声部衔接干净）。
        if let Some((t, note)) = self.pending_off {
            if t < block_end {
                emit(NoteEvent::Off { note });
                self.pending_off = None;
            }
        }

        // 2. 到期的步进。常规参数下步长 ≫ 块长，每块至多一步；
        //    循环写法保证极端参数下也不丢步。
        while self.next_step_sample < block_end {
            let step = self.pattern[self.step_index];
            if step.enabled {
                let velocity = if step.velocity.is_finite() {
                    step.velocity.clamp(0.0, 1.0)
                } else {
                    0.9
                };
                emit(NoteEvent::On {
                    note: step.note,
                    velocity,
                });
                self.last_note = Some(step.note);
                let len = self.step_len_samples();
                let upper = len.saturating_sub(1).max(1);
                let gate_samples = ((len as f32 * self.gate) as u64).clamp(1, upper);
                self.pending_off = Some((self.next_step_sample + gate_samples, step.note));
            }
            self.last_step_sample = self.next_step_sample;
            self.step_index = (self.step_index + 1) % PATTERN_STEPS;
            self.next_step_sample += self.step_len_samples();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PATTERN_STEPS, Sequencer, SequencerStep};
    use crate::realtime::commands::NoteEvent;
    use crate::realtime::context::ProcessContext;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn ctx(pos: u64) -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: pos,
        }
    }

    /// 推进 `blocks` 个块，收集全部事件。
    fn run(seq: &mut Sequencer, start_pos: u64, blocks: usize) -> Vec<NoteEvent> {
        let mut events = Vec::new();
        for i in 0..blocks {
            let pos = start_pos + (i * BLOCK) as u64;
            seq.advance(&ctx(pos), &mut |event| events.push(event));
        }
        events
    }

    fn step(note: u8) -> SequencerStep {
        SequencerStep {
            enabled: true,
            note,
            velocity: 0.9,
        }
    }

    #[test]
    fn plays_enabled_steps_with_gate_offs() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0); // 16 分音符 = 6000 样本
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        pattern[1] = step(64);
        pattern[2] = step(67);
        seq.set_pattern(pattern);
        seq.play(0);

        let events = run(&mut seq, 0, 375); // 48_000 样本 = 1 s
        let on_notes: Vec<u8> = events
            .iter()
            .filter_map(|e| match e {
                NoteEvent::On { note, .. } => Some(*note),
                _ => None,
            })
            .collect();
        let offs = events
            .iter()
            .filter(|e| matches!(e, NoteEvent::Off { .. }))
            .count();
        assert_eq!(on_notes, vec![60, 64, 67], "三步应依次触发");
        assert_eq!(offs, 3, "每个音符都应有 gate off");
        // On / Off 交替：On 必须先于配对的 Off。
        assert!(matches!(events[0], NoteEvent::On { .. }));
        assert!(matches!(events[1], NoteEvent::Off { .. }));
    }

    #[test]
    fn disabled_steps_emit_nothing() {
        let mut seq = Sequencer::new(SR);
        seq.set_pattern([SequencerStep::default(); PATTERN_STEPS]);
        seq.play(0);
        let events = run(&mut seq, 0, 375);
        assert!(events.is_empty(), "全禁用模式不应发声");
        // 步进时钟仍然推进。
        assert_eq!(seq.step_index(), 375 * 128 / 6000 % PATTERN_STEPS);
    }

    #[test]
    fn bpm_change_alters_step_rate() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        for s in &mut pattern {
            *s = step(60);
        }
        seq.set_pattern(pattern);

        seq.set_bpm(120.0);
        seq.play(0);
        let slow = run(&mut seq, 0, 375).len(); // 6000 样本/步 → 8 On + 8 Off

        seq.set_bpm(240.0);
        seq.play(0);
        let fast = run(&mut seq, 0, 375).len(); // 3000 样本/步 → 16 On + 16 Off
        assert_eq!(fast, slow * 2, "240 BPM 的事件率应恰为 120 BPM 的两倍");
    }

    #[test]
    fn stop_quiets_and_returns_last_note() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        seq.set_pattern(pattern);
        seq.play(0);
        let events = run(&mut seq, 0, 1); // 触发第 0 步
        assert!(matches!(events[0], NoteEvent::On { note: 60, .. }));

        let last = seq.stop();
        assert_eq!(last, Some(60));
        let events = run(&mut seq, 256, 375);
        assert!(events.is_empty(), "停止后不应再有事件");
        assert!(!seq.is_playing());
    }

    #[test]
    fn step_clock_wraps_every_16_steps() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        pattern[15] = step(72);
        seq.set_pattern(pattern);
        seq.play(0);

        // 20 步 ≈ 120_000 样本 + 余量：step 0 触发两次。
        let ons: Vec<u8> = run(&mut seq, 0, 950)
            .iter()
            .filter_map(|e| match e {
                NoteEvent::On { note, .. } => Some(*note),
                _ => None,
            })
            .collect();
        assert_eq!(ons, vec![60, 72, 60], "步进时钟应在 16 步后回绕");
    }

    #[test]
    fn gate_off_comes_before_next_note_on_at_full_gate() {
        let mut seq = Sequencer::new(SR);
        seq.set_gate(1.0);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        pattern[1] = step(64);
        seq.set_pattern(pattern);
        seq.play(0);

        let events = run(&mut seq, 0, 60);
        // gate=1.0 时 off 恰在下一步前一瞬：60 块 = 7680 样本覆盖
        // 步 0（on@0, off@5999）与步 1 的 on@6000；其 off 在窗口外。
        // 顺序 on → off → on 即证明 off 先于下一个 note on。
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                NoteEvent::On { .. } => "on",
                NoteEvent::Off { .. } => "off",
            })
            .collect();
        assert_eq!(kinds, vec!["on", "off", "on"]);
    }

    #[test]
    fn playing_and_step_feedback() {
        let mut seq = Sequencer::new(SR);
        assert!(!seq.is_playing());
        seq.play(0);
        assert!(seq.is_playing());
        assert_eq!(seq.step_index(), 0);
        run(&mut seq, 0, 45); // 5760 样本 < 6000：尚未跨过第 1 步
        assert_eq!(seq.step_index(), 1);
        run(&mut seq, 5760, 5); // 再 640 样本，跨过 6000
        assert_eq!(seq.step_index(), 2);
    }
}
