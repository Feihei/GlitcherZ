//! 步进音序器与钢琴卷帘调度（音频线程侧，块边界采样时钟，多轨）。
//!
//! 调度模型是**纯窗口扫描**：音序器只维护音乐位置（tick），
//! 每个块把 `[t0, t1)` 窗口内到期的步进/音符一次性调度出来。
//! BPM 变化只影响未来速率（位置以 tick 计，音乐位置天然保持），
//! 循环回绕由窗口映射推导，无需行进式时钟簿记。

use crate::realtime::DEFAULT_MIDI_TRACKS;
use crate::realtime::MAX_MIDI_TRACKS;
use crate::realtime::automation::{MAX_AUTO_TRACKS, RtAutoTrack};
use crate::realtime::commands::NoteEvent;
use crate::realtime::context::ProcessContext;

/// 音序器步数。
pub const PATTERN_STEPS: usize = 16;

/// 音符时间基准：每四分音符的 tick 数（与 `resoz_model` 的约定一致）。
pub const TICKS_PER_QUARTER: u32 = 96;

/// 每个步进（16 分音符）的 tick 数。
pub const TICKS_PER_STEP: u32 = 24;

/// 卷帘音符数上限（音频线程侧 `Copy` 表示的容量约束）。
pub const MAX_CLIP_NOTES: usize = 128;

/// 轨道音源（引擎侧表示，与 `resoz_model::TrackSource` 镜像）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackSource {
    /// 16 步步进模式（默认）。
    #[default]
    Steps,
    /// 钢琴卷帘片段。
    Clip,
}

/// 单个步进的音频线程侧表示。
///
/// 保持 `Copy`：整条模式可作为一个命令经无锁队列整块传递，实时侧
/// 零分配。与 `resoz_model::Step` 互为镜像（见 AGENTS.md「数据模型」）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SequencerStep {
    /// 该步是否触发。
    pub enabled: bool,
    /// MIDI 音符编号。
    pub note: u8,
    /// 力度（0..1）。
    pub velocity: f32,
}

/// 卷帘音符的音频线程侧表示（镜像 `resoz_model::Note`）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RtNote {
    /// 起始位置（tick，相对片段起点）。
    pub start_ticks: u32,
    /// 时长（tick）。
    pub length_ticks: u32,
    /// MIDI 音符编号。
    pub note: u8,
    /// 力度（0..1）。
    pub velocity: f32,
}

/// 卷帘片段的音频线程侧表示（镜像 `resoz_model::Clip`，定容 `Copy`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtClip {
    /// 循环长度（tick）。
    pub loop_ticks: u32,
    /// 音符集合（前 `note_count` 个有效）。
    pub notes: [RtNote; MAX_CLIP_NOTES],
    /// 有效音符数。
    pub note_count: usize,
}

impl Default for RtClip {
    fn default() -> Self {
        Self {
            loop_ticks: TICKS_PER_STEP * PATTERN_STEPS as u32,
            notes: [RtNote::default(); MAX_CLIP_NOTES],
            note_count: 0,
        }
    }
}

impl RtClip {
    /// 追加一个音符；超过 [`MAX_CLIP_NOTES`] 时静默丢弃。
    pub fn push(&mut self, note: RtNote) {
        if self.note_count < MAX_CLIP_NOTES {
            self.notes[self.note_count] = note;
            self.note_count += 1;
        }
    }
}

/// 多轨音序器：一个全局走带时钟（BPM / gate / 音乐位置），
/// 每条轨道独立的音源（步进模式或卷帘片段）与 gate 尾音，
/// 按采样时钟在块边界把到期事件转成 `(MIDI 轨, NoteEvent)`。
///
/// 自动化轨道是与 MIDI 轨道并列的独立实体（D7）：[`Sequencer`] 只
/// 持有它们的曲线与自带循环长度；投递目标由节点图的自动化连线表
/// 决定，本结构不感知。
///
/// 时钟以 tick 计（音乐位置），与采样时钟的换算由 BPM 推导；
/// gate off 以绝对采样位置记录，落在包含它的块边界发出。
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
    /// gate 占步长的比例（0.05..1，仅步进音源使用；卷帘用音符时长）。
    gate: f32,
    position_ticks: f64,
    /// 活跃 MIDI 轨数（D12 动态轨数：控制侧经 `SetMidiTrackCount` 下发，
    /// 调度只扫描 `0..track_count`；未配置轨的 patterns/clips 为默认空）。
    track_count: usize,
    sources: [TrackSource; MAX_MIDI_TRACKS],
    patterns: [[SequencerStep; PATTERN_STEPS]; MAX_MIDI_TRACKS],
    clips: [RtClip; MAX_MIDI_TRACKS],
    /// 自动化轨道实体（独立于 MIDI 轨道，见 D7）。
    autos: [RtAutoTrack; MAX_AUTO_TRACKS],
    /// 各轨待发送的 NoteOff（gate/时长结束的绝对采样位置 + 音符）。
    pending_offs: [Option<(u64, u8)>; MAX_MIDI_TRACKS],
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
            position_ticks: 0.0,
            track_count: DEFAULT_MIDI_TRACKS,
            sources: [TrackSource::default(); MAX_MIDI_TRACKS],
            patterns: [[SequencerStep::default(); PATTERN_STEPS]; MAX_MIDI_TRACKS],
            clips: [RtClip::default(); MAX_MIDI_TRACKS],
            autos: [RtAutoTrack::default(); MAX_AUTO_TRACKS],
            pending_offs: [None; MAX_MIDI_TRACKS],
        }
    }

    /// 是否播放中。RT-safe。
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// 当前步索引（`⌊音乐位置 / 步长⌋ mod 16`，用于 UI 高亮"正在响的步"）。
    /// RT-safe。
    pub fn step_index(&self) -> usize {
        (self.position_ticks / f64::from(TICKS_PER_STEP)).floor() as usize % PATTERN_STEPS
    }

    /// 音乐位置（tick，自播放起点累计）。RT-safe。
    pub fn position_ticks(&self) -> f64 {
        self.position_ticks
    }

    /// 每采样点对应的 tick 数（f64 中间计算）。
    fn ticks_per_sample(&self) -> f64 {
        let bpm = if self.bpm.is_finite() {
            self.bpm.clamp(20.0, 300.0)
        } else {
            120.0
        };
        // tick/秒 = BPM/60 × TPQN；除以采样率得 tick/样本。
        f64::from(bpm) / 60.0 * f64::from(TICKS_PER_QUARTER) / f64::from(self.sample_rate)
    }

    /// 从第 0 步开始播放（音乐位置归零）。RT-safe。
    pub fn play(&mut self) {
        self.playing = true;
        self.position_ticks = 0.0;
        self.pending_offs = [None; MAX_MIDI_TRACKS];
    }

    /// 停止调度并清空全部 gate 尾音。
    /// 调用方应向所有轨道发 `NoteOff` 释放包络。RT-safe。
    pub fn stop(&mut self) {
        self.playing = false;
        self.pending_offs = [None; MAX_MIDI_TRACKS];
    }

    /// 设置活跃 MIDI 轨数（钳到 [`MAX_MIDI_TRACKS`]；缩轨时更高编号轨
    /// 的 gate 尾音一并清除）。RT-safe。
    pub fn set_track_count(&mut self, count: usize) {
        self.track_count = count.min(MAX_MIDI_TRACKS);
        if self.pending_offs.len() > self.track_count {
            for pending in self.pending_offs[self.track_count..].iter_mut() {
                *pending = None;
            }
        }
    }

    /// 当前活跃 MIDI 轨数。RT-safe。
    pub fn track_count(&self) -> usize {
        self.track_count
    }

    /// 设置 BPM（20..300，非有限值忽略）。音乐位置以 tick 计，
    /// 变速不影响已播放的内容，只改变后续速率。RT-safe。
    pub fn set_bpm(&mut self, bpm: f32) {
        if bpm.is_finite() {
            self.bpm = bpm.clamp(20.0, 300.0);
        }
    }

    /// 设置 gate 占比（0.05..1，非有限值忽略）。RT-safe。
    pub fn set_gate(&mut self, gate: f32) {
        if gate.is_finite() {
            self.gate = gate.clamp(0.05, 1.0);
        }
    }

    /// 整块替换某条轨道的步进模式（越界轨道忽略）。RT-safe。
    pub fn set_pattern(&mut self, track: usize, steps: [SequencerStep; PATTERN_STEPS]) {
        if let Some(slot) = self.patterns.get_mut(track) {
            *slot = steps;
        }
    }

    /// 整块替换某条轨道的卷帘片段（越界轨道忽略）。RT-safe。
    pub fn set_clip(&mut self, track: usize, clip: RtClip) {
        if let Some(slot) = self.clips.get_mut(track) {
            *slot = clip;
        }
    }

    /// 切换某条轨道的音源（越界轨道忽略）。RT-safe。
    pub fn set_source(&mut self, track: usize, source: TrackSource) {
        if let Some(slot) = self.sources.get_mut(track) {
            *slot = source;
        }
    }

    /// 整块替换一条自动化轨道（越界索引忽略；空曲线 = 清空该轨道）。
    /// 曲线不含目标 —— 投递终点由节点图的自动化连线表决定。RT-safe。
    pub fn set_auto_track(&mut self, index: usize, auto: RtAutoTrack) {
        if let Some(slot) = self.autos.get_mut(index) {
            *slot = auto;
        }
    }

    /// 全部自动化轨道实体的只读访问（供求值器按连线表采样）。RT-safe。
    pub fn auto_tracks(&self) -> &[RtAutoTrack; MAX_AUTO_TRACKS] {
        &self.autos
    }

    /// 推进一个块：把窗口 `[t0, t1)`（tick）内到期的 gate off 与
    /// 音符 on 经 `emit(轨道, 事件)` 送出（同一块内先 off 后 on）。
    /// RT-safe。
    pub fn advance(&mut self, ctx: &ProcessContext, emit: &mut dyn FnMut(usize, NoteEvent)) {
        if !self.playing {
            return;
        }
        let tps = self.ticks_per_sample();
        let samples_per_tick = 1.0 / tps;
        let dt_ticks = ctx.frames as f64 * tps;
        let t0 = self.position_ticks;
        let t1 = t0 + dt_ticks;
        let block_start = ctx.playback_sample_pos;

        // 1. 到期的 gate off（先于本块的 note on，保证各轨衔接干净）。
        for (track, pending) in self.pending_offs.iter_mut().enumerate() {
            if let Some((time, note)) = *pending {
                if time < block_start + ctx.frames as u64 {
                    emit(track, NoteEvent::Off { note });
                    *pending = None;
                }
            }
        }

        // 2. 各轨按音源调度窗口内的音符 on（只扫活跃轨，D12）。
        for track in 0..self.track_count {
            match self.sources[track] {
                TrackSource::Steps => {
                    self.schedule_steps(track, t0, t1, block_start, samples_per_tick, emit);
                }
                TrackSource::Clip => {
                    self.schedule_clip(track, t0, t1, block_start, samples_per_tick, emit);
                }
            }
        }

        self.position_ticks = t1;
    }

    /// 步进音源：窗口内起始的步（24 tick 一步）。
    #[allow(clippy::too_many_arguments)]
    fn schedule_steps(
        &mut self,
        track: usize,
        t0: f64,
        t1: f64,
        block_start: u64,
        samples_per_tick: f64,
        emit: &mut dyn FnMut(usize, NoteEvent),
    ) {
        let step_ticks = f64::from(TICKS_PER_STEP);
        // 起点恰好落在窗口边界上的步也要触发：ceil(t0) .. ceil(t1)。
        let first = (t0 / step_ticks).ceil() as i64;
        let last = (t1 / step_ticks).ceil() as i64;
        for k in first..last {
            let start_tick = k as f64 * step_ticks;
            if start_tick < t0 {
                continue; // 浮点边界保护
            }
            let pattern_index = (k as usize) % PATTERN_STEPS;
            let step = self.patterns[track][pattern_index];
            if !step.enabled {
                continue;
            }
            let velocity = sanitize_velocity(step.velocity);
            emit(
                track,
                NoteEvent::On {
                    note: step.note,
                    velocity,
                },
            );
            // gate off 时刻（tick）→ 绝对采样位置。
            let off_tick = start_tick + step_ticks * f64::from(self.gate);
            self.set_pending_off(
                track,
                off_tick,
                t0,
                block_start,
                samples_per_tick,
                step.note,
            );
        }
    }

    /// 卷帘音源：窗口内起始的音符出现（含跨循环回绕的每一次出现）。
    #[allow(clippy::too_many_arguments)]
    fn schedule_clip(
        &mut self,
        track: usize,
        t0: f64,
        t1: f64,
        block_start: u64,
        samples_per_tick: f64,
        emit: &mut dyn FnMut(usize, NoteEvent),
    ) {
        let clip = self.clips[track];
        if clip.loop_ticks == 0 || clip.note_count == 0 {
            return;
        }
        let loop_ticks = f64::from(clip.loop_ticks);
        for note in &clip.notes[..clip.note_count] {
            let start_offset = f64::from(note.start_ticks);
            let length = f64::from(note.length_ticks.max(1));
            // 第 m 次出现位于 m·L + s；求首个 ≥ t0 的 m（可为负，钳到 0）。
            let mut m = ((t0 - start_offset) / loop_ticks).ceil();
            if m < 0.0 {
                m = 0.0;
            }
            loop {
                let start_tick = m * loop_ticks + start_offset;
                if start_tick >= t1 {
                    break;
                }
                if start_tick >= t0 {
                    let velocity = sanitize_velocity(note.velocity);
                    emit(
                        track,
                        NoteEvent::On {
                            note: note.note,
                            velocity,
                        },
                    );
                    let off_tick = start_tick + length;
                    self.set_pending_off(
                        track,
                        off_tick,
                        t0,
                        block_start,
                        samples_per_tick,
                        note.note,
                    );
                }
                m += 1.0;
            }
        }
    }

    /// 记录 gate off：换算为绝对采样位置，至少落在下一块（保证 off
    /// 晚于同一块内已发出的 on）。RT-safe。
    #[allow(clippy::too_many_arguments)]
    fn set_pending_off(
        &mut self,
        track: usize,
        off_tick: f64,
        t0: f64,
        block_start: u64,
        samples_per_tick: f64,
        note: u8,
    ) {
        let offset_samples = ((off_tick - t0) * samples_per_tick).max(1.0);
        self.pending_offs[track] = Some((block_start + offset_samples as u64, note));
    }
}

fn sanitize_velocity(velocity: f32) -> f32 {
    if velocity.is_finite() {
        velocity.clamp(0.0, 1.0)
    } else {
        0.9
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CLIP_NOTES, PATTERN_STEPS, RtClip, RtNote, Sequencer, SequencerStep, TICKS_PER_STEP,
        TrackSource,
    };
    use crate::realtime::DEFAULT_MIDI_TRACKS;
    use crate::realtime::MAX_MIDI_TRACKS;
    use crate::realtime::automation::{MAX_AUTO_TRACKS, RtAutoPoint, RtAutoTrack};
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
    fn run(seq: &mut Sequencer, blocks: usize) -> Vec<Event> {
        let mut events = Vec::new();
        let mut pos = 0u64;
        for _ in 0..blocks {
            seq.advance(&ctx(pos), &mut |track, event| {
                events.push((track, event));
            });
            pos += BLOCK as u64;
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

    fn clip_note(start: u32, length: u32, note: u8) -> RtNote {
        RtNote {
            start_ticks: start,
            length_ticks: length,
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
        seq.set_bpm(120.0); // 16 分音符 = 24 tick = 6000 样本
        let mut track0 = [SequencerStep::default(); PATTERN_STEPS];
        track0[0] = step(60);
        track0[1] = step(64);
        let mut track1 = [SequencerStep::default(); PATTERN_STEPS];
        track1[0] = step(72);
        seq.set_pattern(0, track0);
        seq.set_pattern(1, track1);
        seq.play();

        let events = run(&mut seq, 375); // 1 s
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
        seq.play();
        let events = run(&mut seq, 375);
        assert!(events.is_empty(), "全禁用模式不应发声");
        // 48000 样本 = 192 tick = 8 步。
        assert_eq!(seq.step_index(), 8);
    }

    #[test]
    fn clip_notes_fire_at_their_positions_with_lengths() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        let mut clip = RtClip::default();
        clip.push(clip_note(0, 24, 60));
        clip.push(clip_note(24, 48, 64));
        seq.set_clip(0, clip);
        seq.set_source(0, TrackSource::Clip);
        seq.play();

        let events = run(&mut seq, 375);
        assert_eq!(
            ons(&events),
            vec![(0, 60), (0, 64)],
            "音符应按 start 依次触发"
        );
        // 每个音符有 off（按自身长度，而非 gate）。
        let offs = events
            .iter()
            .filter(|(_, e)| matches!(e, NoteEvent::Off { .. }))
            .count();
        assert_eq!(offs, 2);
    }

    #[test]
    fn clip_loops_around() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        let mut clip = RtClip::default();
        clip.push(clip_note(0, 24, 60));
        seq.set_clip(0, clip);
        seq.set_source(0, TrackSource::Clip);
        seq.play();

        // 循环长度 384 tick = 96000 样本 = 750 块；跑 2.5 圈 → 触发 3 次。
        let ons_count = run(&mut seq, 1_900)
            .iter()
            .filter(|(_, e)| matches!(e, NoteEvent::On { .. }))
            .count();
        assert_eq!(ons_count, 3, "片段应按 loop_ticks 回绕循环");
    }

    #[test]
    fn source_switching_uses_only_active_source() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(48);
        seq.set_pattern(0, pattern);
        let mut clip = RtClip::default();
        clip.push(clip_note(0, 24, 72));
        seq.set_clip(0, clip);
        seq.play();

        seq.set_source(0, TrackSource::Steps);
        assert_eq!(ons(&run(&mut seq, 30)), vec![(0, 48)]);

        seq.play();
        seq.set_source(0, TrackSource::Clip);
        assert_eq!(ons(&run(&mut seq, 30)), vec![(0, 72)]);
    }

    #[test]
    fn clip_over_capacity_is_truncated() {
        let mut clip = RtClip::default();
        for i in 0..(MAX_CLIP_NOTES + 10) {
            clip.push(clip_note(i as u32 * TICKS_PER_STEP, 24, 60));
        }
        assert_eq!(clip.note_count, MAX_CLIP_NOTES, "超容量的音符应被丢弃");
    }

    #[test]
    fn bpm_change_preserves_musical_position() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        seq.play();
        run(&mut seq, 100); // 12800 样本 = 51.2 tick @120BPM
        let before = seq.position_ticks();
        seq.set_bpm(240.0);
        assert!((seq.position_ticks() - before).abs() < 1e-9, "变速不跳位置");
    }

    #[test]
    fn bpm_change_alters_step_rate() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        for step_slot in &mut pattern {
            *step_slot = step(60);
        }
        seq.set_pattern(0, pattern);

        // 370 块 = 47360 样本：120 BPM 下 189.44 tick（8 步），
        // 240 BPM 下 378.88 tick（16 步）——窗口都避开步进边界。
        let count_ons = |events: &[Event]| {
            events
                .iter()
                .filter(|(_, e)| matches!(e, NoteEvent::On { .. }))
                .count()
        };
        seq.set_bpm(120.0);
        seq.play();
        let slow = count_ons(&run(&mut seq, 370));

        seq.set_bpm(240.0);
        seq.play();
        let fast = count_ons(&run(&mut seq, 370));
        assert_eq!(fast, slow * 2, "240 BPM 的触发率应恰为 120 BPM 的两倍");
    }

    #[test]
    fn stop_quiets_every_track() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        for track in 0..DEFAULT_MIDI_TRACKS {
            seq.set_pattern(track, pattern);
        }
        seq.play();
        let events = run(&mut seq, 1);
        assert_eq!(
            ons(&events).len(),
            DEFAULT_MIDI_TRACKS,
            "默认四轨应同时触发第 0 步"
        );

        seq.stop();
        let events = run(&mut seq, 375);
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
        seq.play();

        // 20 步（480 tick ≈ 120_000 样本）+ 余量：step 0 触发两次。
        let fired: Vec<u8> = run(&mut seq, 950)
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
        seq.play();

        let events = run(&mut seq, 60);
        // 60 块 = 7680 样本覆盖步 0（on@0, off@5999）与步 1 的 on@6000。
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
        seq.play();
        assert!(seq.is_playing());
        // 5760 样本 = 23.04 tick < 24：仍在第 0 步内。
        run(&mut seq, 45);
        assert_eq!(seq.step_index(), 0);
        run(&mut seq, 5); // 6400 样本 = 25.6 tick：进入第 1 步
        assert_eq!(seq.step_index(), 1);
    }

    #[test]
    fn auto_tracks_are_independent_entities() {
        let mut seq = Sequencer::new(SR);
        let mut auto = RtAutoTrack {
            loop_ticks: 96, // 与 MIDI 轨道循环解耦（D7）
            ..RtAutoTrack::default()
        };
        auto.curve.push(RtAutoPoint {
            tick: 0,
            value: 0.5,
        });
        seq.set_auto_track(5, auto);
        seq.set_auto_track(MAX_AUTO_TRACKS, auto); // 越界索引忽略
        let tracks = seq.auto_tracks();
        assert_eq!(tracks[5].loop_ticks, 96);
        assert_eq!(tracks[5].curve.point_count, 1);
        assert!(
            tracks
                .iter()
                .enumerate()
                .all(|(i, t)| i == 5 || !t.curve.is_active()),
            "其余自动化轨道应保持默认空曲线"
        );
        // MIDI 轨道不受自动化轨道写入影响：模式照常调度。
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        seq.set_pattern(0, pattern);
        seq.play();
        assert_eq!(ons(&run(&mut seq, 30)), vec![(0, 60)]);
    }

    #[test]
    fn track_count_gates_scheduling() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        seq.set_pattern(5, pattern);
        seq.set_track_count(4);
        seq.play();
        assert!(
            ons(&run(&mut seq, 30)).is_empty(),
            "活跃轨数之外的轨不应调度"
        );
        seq.set_track_count(8);
        seq.play(); // 重新归零走带，让步 0 重新落进调度窗口
        assert_eq!(ons(&run(&mut seq, 30)), vec![(5, 60)]);
        // 钳制到容量上限。
        seq.set_track_count(MAX_MIDI_TRACKS + 10);
        assert_eq!(seq.track_count(), MAX_MIDI_TRACKS);
    }
}
