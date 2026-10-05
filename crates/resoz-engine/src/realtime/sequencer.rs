//! 步进音序器（音频线程侧，块边界采样时钟调度，多轨）。

use crate::realtime::NUM_TRACKS;
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

/// 多轨步进音序器：一个全局走带时钟（BPM / gate / 步进索引），
/// 每条轨道独立的 16 步模式与 gate 尾音，按采样时钟在块边界把到期
/// 步进转成 `(轨道, NoteEvent)`。
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
    patterns: [[SequencerStep; PATTERN_STEPS]; NUM_TRACKS],
    step_index: usize,
    /// 最近一次触发步进的时刻（BPM 变化时用于重推下一拍）。
    last_step_sample: u64,
    next_step_sample: u64,
    /// 各轨待发送的 NoteOff（gate 结束时刻 + 音符）。
    pending_offs: [Option<(u64, u8)>; NUM_TRACKS],
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
            patterns: [[SequencerStep::default(); PATTERN_STEPS]; NUM_TRACKS],
            step_index: 0,
            last_step_sample: 0,
            next_step_sample: 0,
            pending_offs: [None; NUM_TRACKS],
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
        self.pending_offs = [None; NUM_TRACKS];
    }

    /// 停止调度并清空全部 gate 尾音。
    /// 调用方应向所有轨道发 `NoteOff` 释放包络。RT-safe。
    pub fn stop(&mut self) {
        self.playing = false;
        self.pending_offs = [None; NUM_TRACKS];
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

    /// 整块替换某条轨道的模式（越界轨道忽略）。RT-safe。
    pub fn set_pattern(&mut self, track: usize, steps: [SequencerStep; PATTERN_STEPS]) {
        if let Some(slot) = self.patterns.get_mut(track) {
            *slot = steps;
        }
    }

    /// 推进一个块：把到期的 gate off 与步进 on 经 `emit(轨道, 事件)` 送出
    /// （同一块内先 off 后 on）。RT-safe。
    pub fn advance(&mut self, ctx: &ProcessContext, emit: &mut dyn FnMut(usize, NoteEvent)) {
        if !self.playing {
            return;
        }
        let block_end = ctx.playback_sample_pos + ctx.frames as u64;

        // 1. 到期的 gate off（先于本块的 note on，保证各轨衔接干净）。
        for (track, pending) in self.pending_offs.iter_mut().enumerate() {
            if let Some((time, note)) = *pending {
                if time < block_end {
                    emit(track, NoteEvent::Off { note });
                    *pending = None;
                }
            }
        }

        // 2. 到期的步进。常规参数下步长 ≫ 块长，每块至多一步；
        //    循环写法保证极端参数下也不丢步。
        while self.next_step_sample < block_end {
            for (track, pattern) in self.patterns.iter().enumerate() {
                let step = pattern[self.step_index];
                if !step.enabled {
                    continue;
                }
                let velocity = if step.velocity.is_finite() {
                    step.velocity.clamp(0.0, 1.0)
                } else {
                    0.9
                };
                emit(
                    track,
                    NoteEvent::On {
                        note: step.note,
                        velocity,
                    },
                );
                let len = self.step_len_samples();
                let upper = len.saturating_sub(1).max(1);
                let gate_samples = ((len as f32 * self.gate) as u64).clamp(1, upper);
                self.pending_offs[track] = Some((self.next_step_sample + gate_samples, step.note));
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
    use crate::realtime::NUM_TRACKS;
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

    type Event = (usize, NoteEvent);

    /// 推进 `blocks` 个块，收集全部 (轨道, 事件)。
    fn run(seq: &mut Sequencer, start_pos: u64, blocks: usize) -> Vec<Event> {
        let mut events = Vec::new();
        for i in 0..blocks {
            let pos = start_pos + (i * BLOCK) as u64;
            seq.advance(&ctx(pos), &mut |track, event| {
                events.push((track, event));
            });
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

    fn ons(events: &[Event]) -> Vec<(usize, u8)> {
        events
            .iter()
            .filter_map(|(t, e)| match e {
                NoteEvent::On { note, .. } => Some((*t, *note)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plays_per_track_patterns_with_gate_offs() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0); // 16 分音符 = 6000 样本
        let mut track0 = [SequencerStep::default(); PATTERN_STEPS];
        track0[0] = step(60);
        track0[1] = step(64);
        let mut track1 = [SequencerStep::default(); PATTERN_STEPS];
        track1[0] = step(72);
        seq.set_pattern(0, track0);
        seq.set_pattern(1, track1);
        seq.play(0);

        let events = run(&mut seq, 0, 375); // 1 s
        assert_eq!(
            ons(&events),
            vec![(0, 60), (1, 72), (0, 64)],
            "两轨应同时钟触发"
        );
        let offs = events
            .iter()
            .filter(|(_, e)| matches!(e, NoteEvent::Off { .. }))
            .count();
        assert_eq!(offs, 3, "每个音符都应有 gate off");
        assert!(matches!(events[0], (0, NoteEvent::On { .. })));
        assert!(matches!(events[1], (1, NoteEvent::On { .. })));
    }

    #[test]
    fn disabled_steps_emit_nothing() {
        let mut seq = Sequencer::new(SR);
        seq.play(0);
        let events = run(&mut seq, 0, 375);
        assert!(events.is_empty(), "全禁用模式不应发声");
        assert_eq!(seq.step_index(), 375 * 128 / 6000 % PATTERN_STEPS);
    }

    #[test]
    fn pattern_change_targets_only_one_track() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        seq.set_pattern(0, pattern);
        seq.set_pattern(1, pattern);
        // 越界轨道：静默忽略，不得 panic。
        seq.set_pattern(NUM_TRACKS, pattern);
        seq.set_pattern(usize::MAX, pattern);
        seq.play(0);
        let events = run(&mut seq, 0, 20);
        assert_eq!(
            ons(&events),
            vec![(0, 60), (1, 60)],
            "越轨设置不应产生额外事件"
        );
    }

    #[test]
    fn bpm_change_alters_step_rate() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        for step_slot in &mut pattern {
            *step_slot = step(60);
        }
        seq.set_pattern(0, pattern);

        seq.set_bpm(120.0);
        seq.play(0);
        let slow = run(&mut seq, 0, 375).len(); // 6000 样本/步 → 8 On + 8 Off

        seq.set_bpm(240.0);
        seq.play(0);
        let fast = run(&mut seq, 0, 375).len(); // 3000 样本/步 → 16 On + 16 Off
        assert_eq!(fast, slow * 2, "240 BPM 的事件率应恰为 120 BPM 的两倍");
    }

    #[test]
    fn stop_quiets_every_track() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        for track in 0..NUM_TRACKS {
            seq.set_pattern(track, pattern);
        }
        seq.play(0);
        let events = run(&mut seq, 0, 1);
        assert_eq!(ons(&events).len(), NUM_TRACKS, "四轨应同时触发第 0 步");

        seq.stop();
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
        seq.set_pattern(0, pattern);
        seq.play(0);

        // 20 步 ≈ 120_000 样本 + 余量：step 0 触发两次。
        let fired: Vec<u8> = run(&mut seq, 0, 950)
            .iter()
            .filter_map(|(t, e)| {
                if *t == 0 {
                    match e {
                        NoteEvent::On { note, .. } => Some(*note),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(fired, vec![60, 72, 60], "步进时钟应在 16 步后回绕");
    }

    #[test]
    fn gate_off_comes_before_next_note_on_at_full_gate() {
        let mut seq = Sequencer::new(SR);
        seq.set_gate(1.0);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        pattern[1] = step(64);
        seq.set_pattern(0, pattern);
        seq.play(0);

        let events = run(&mut seq, 0, 60);
        // 60 块 = 7680 样本覆盖步 0（on@0, off@5999）与步 1 的 on@6000；
        // 顺序 on → off → on 即证明 off 先于下一个 note on。
        let kinds: Vec<&str> = events
            .iter()
            .map(|(_, e)| match e {
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
