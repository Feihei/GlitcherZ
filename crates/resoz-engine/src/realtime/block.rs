//! 固定块处理调度：cpal 回调 ↔ 混音器的桥梁。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use rtrb::Consumer;

use crate::realtime::atomic::AtomicF32;
use crate::realtime::commands::{EngineCommand, SequencerCommand};
use crate::realtime::context::ProcessContext;
use crate::realtime::mixer::Mixer;
use crate::realtime::sequencer::Sequencer;

/// 引擎 → UI 的状态回传句柄：与 [`BlockRunner`] 共享的一组原子量。
///
/// `BlockRunner` 在每个块边界写入，[`crate::engine::AudioEngine`] 持有
/// 克隆副本供 UI 读取。
#[derive(Clone)]
pub struct Feedback {
    /// 主输出最近块峰值电平。
    pub level: Arc<AtomicF32>,
    /// 播放位置（流启动以来的样本数）。
    pub position: Arc<AtomicU64>,
    /// 音序器当前步进索引。
    pub step: Arc<AtomicU32>,
    /// 音序器走带状态。
    pub playing: Arc<AtomicBool>,
    /// 音序器音乐位置（tick，自播放起点累计）。
    pub position_ticks: Arc<AtomicF32>,
    /// 各轨道 post-fader 峰值电平。
    pub track_levels: [Arc<AtomicF32>; crate::realtime::NUM_TRACKS],
}

impl Feedback {
    /// 全零初始值。**非 RT-safe**，在启动音频流前调用。
    pub fn new() -> Self {
        Self {
            level: Arc::new(AtomicF32::new(0.0)),
            position: Arc::new(AtomicU64::new(0)),
            step: Arc::new(AtomicU32::new(0)),
            playing: Arc::new(AtomicBool::new(false)),
            position_ticks: Arc::new(AtomicF32::new(0.0)),
            track_levels: std::array::from_fn(|_| Arc::new(AtomicF32::new(0.0))),
        }
    }
}

impl Default for Feedback {
    fn default() -> Self {
        Self::new()
    }
}

/// 把宿主（cpal）交来的任意长度交错缓冲区拆成固定块交给混音器执行，
/// 再交错写回。实例在音频流启动前构造，随后 move 进音频回调线程，
/// 其生命周期内独占音频线程。
///
/// 每个固定块的执行顺序（`run_next_block`）：
/// 1. **块边界**消费全部待处理命令（队列空则立即返回）；
/// 2. 音序器按采样时钟调度，到期音符事件直接送入对应轨道；
/// 3. 混音器处理一个块（各轨并行 → 求和 → 主链）；
/// 4. 回传峰值电平、播放位置、音序器步进与走带状态（原子量）。
pub struct BlockRunner {
    fixed_block: usize,
    out_channels: usize,
    sample_rate: f32,
    mixer: Mixer,
    sequencer: Sequencer,
    scratch_pos: usize,
    commands: Consumer<EngineCommand>,
    feedback: Feedback,
    played_samples: u64,
}

impl BlockRunner {
    /// 构造。**非 RT-safe**（分配缓冲），必须在启动音频流前调用。
    pub fn new(
        fixed_block: usize,
        out_channels: usize,
        sample_rate: f32,
        mixer: Mixer,
        sequencer: Sequencer,
        commands: Consumer<EngineCommand>,
        feedback: Feedback,
    ) -> Self {
        Self {
            fixed_block: fixed_block.max(1),
            out_channels: out_channels.max(1),
            sample_rate: if sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            mixer,
            sequencer,
            scratch_pos: 0,
            commands,
            feedback,
            played_samples: 0,
        }
    }

    /// 渲染宿主给出的交错输出缓冲区。**RT-safe**（cpal 数据回调直接调用）。
    ///
    /// 声道映射：源为立体声（L/R）；输出第 0/1 声道取 L/R，其余声道静音；
    /// 单声道输出取 L。
    pub fn render_interleaved(&mut self, out: &mut [f32]) {
        for frame in out.chunks_mut(self.out_channels) {
            if self.scratch_pos >= self.mixer_block_len() {
                self.run_next_block();
            }
            let left = self.sample_at(0, self.scratch_pos);
            let right = self.sample_at(1, self.scratch_pos);
            for (ch, slot) in frame.iter_mut().enumerate() {
                *slot = match ch {
                    0 => left,
                    1 => right,
                    _ => 0.0,
                };
            }
            self.scratch_pos += 1;
        }
    }

    /// 当前主缓冲活动长度。
    fn mixer_block_len(&self) -> usize {
        self.mixer
            .master_channel(0)
            .map_or(0, |channel| channel.len())
    }

    /// 执行一个固定块。只应在 [`BlockRunner::render_interleaved`] 内调用。
    fn run_next_block(&mut self) {
        // 1. 块边界：消费所有待处理命令（无锁队列，空则立即返回）。
        while let Ok(cmd) = self.commands.pop() {
            self.apply_command(cmd);
        }

        // 2. 本块上下文（供音序器与混音器共用）。
        let frames = self.fixed_block;
        let ctx = ProcessContext {
            sample_rate: self.sample_rate,
            frames,
            playback_sample_pos: self.played_samples,
        };

        // 3. 音序器调度：到期音符事件直接送入对应轨道（块边界精度）。
        //    拆字段借用，让 sequencer 与 mixer 同时可变。
        //    自动化取本块起始位置（块内音频以起点近似）。
        let position_before = self.sequencer.position_ticks();
        let Self {
            sequencer, mixer, ..
        } = self;
        sequencer.advance(&ctx, &mut |track, event| {
            mixer.track_note(track, event);
        });

        // 4. 自动化求值（仅播放中）：按循环后的音乐位置采样各轨曲线，
        //    经混音器内部路径直写参数（不走命令队列）；停止时最后一次
        //    手动值保持生效（由 UI 在走带停止沿重推手动参数）。
        if self.sequencer.is_playing() {
            self.evaluate_automation(position_before);
        }

        // 5. 混音器处理一个块（各轨并行 → 求和 → 主链）。
        let levels = self.mixer.process_block(&ctx);

        // 6. 状态回传。
        self.feedback.level.store(levels.master);
        for (slot, value) in self.feedback.track_levels.iter().zip(levels.tracks) {
            slot.store(value);
        }
        self.feedback
            .position
            .store(self.played_samples, Ordering::Relaxed);
        self.feedback
            .step
            .store(self.sequencer.step_index() as u32, Ordering::Relaxed);
        self.feedback
            .playing
            .store(self.sequencer.is_playing(), Ordering::Relaxed);
        self.feedback
            .position_ticks
            .store(self.sequencer.position_ticks() as f32);

        self.played_samples += frames as u64;
        self.scratch_pos = 0;
    }

    /// 采样并应用全部轨道的自动化曲线。RT-safe（无分配）。
    fn evaluate_automation(&mut self, position_ticks: f64) {
        for track in 0..crate::realtime::NUM_TRACKS {
            let loop_ticks = self.sequencer.track_loop_ticks(track);
            let tick = if loop_ticks > 0 {
                position_ticks % f64::from(loop_ticks)
            } else {
                position_ticks
            };
            for curve in self.sequencer.track_automations(track) {
                if let Some(value) = curve.value_at(tick) {
                    self.mixer.track_set_param(track, curve.param, value);
                }
            }
        }
    }

    fn apply_command(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::SetParam { id, value } => {
                // 未被识别的参数静默丢弃（发送方负责只发送有效参数）。
                let _ = self.mixer.master_set_param(id, value);
            }
            EngineCommand::SetTrackParam { track, id, value } => {
                self.mixer.track_set_param(track, id, value);
            }
            EngineCommand::Note { track, event } => {
                self.mixer.track_note(track, event);
            }
            EngineCommand::Sequencer(cmd) => self.apply_sequencer_command(cmd),
        }
    }

    fn apply_sequencer_command(&mut self, cmd: SequencerCommand) {
        match cmd {
            SequencerCommand::Play => {
                self.sequencer.play();
                // 清掉可能仍按住的手动音符，避免与音序声部叠加。
                self.mixer.note_off_all();
            }
            SequencerCommand::Stop => {
                self.sequencer.stop();
                self.mixer.note_off_all();
            }
            SequencerCommand::SetBpm(bpm) => self.sequencer.set_bpm(bpm),
            SequencerCommand::SetGate(gate) => self.sequencer.set_gate(gate),
            SequencerCommand::SetPattern { track, steps } => {
                self.sequencer.set_pattern(track, steps);
            }
            SequencerCommand::SetClip { track, clip } => {
                self.sequencer.set_clip(track, clip);
            }
            SequencerCommand::SetSource { track, source } => {
                self.sequencer.set_source(track, source);
            }
            SequencerCommand::SetAutomation { track, automation } => {
                self.sequencer.set_automation(track, automation);
            }
        }
    }

    /// 读取主缓冲中 `(声道, 位置)` 的样本；越界一律静音（不 panic）。
    fn sample_at(&self, ch: usize, pos: usize) -> f32 {
        self.mixer
            .master_channel(ch)
            .and_then(|c| c.get(pos))
            .copied()
            .unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use rtrb::Producer;

    use super::{BlockRunner, Feedback};
    use crate::realtime::automation::{RtAutoPoint, RtAutomation};
    use crate::realtime::commands::{
        EngineCommand, NoteEvent, ParamId, SequencerCommand, command_channel,
    };
    use crate::realtime::mixer::Mixer;
    use crate::realtime::sequencer::{PATTERN_STEPS, Sequencer, SequencerStep};
    use crate::testing::count_positive_crossings;

    const SR: f32 = 44_100.0;

    /// 测试台架：返回 (runner, 命令发送端, 状态回传句柄)。
    fn make_runner(
        fixed_block: usize,
        out_channels: usize,
    ) -> (BlockRunner, Producer<EngineCommand>, Feedback) {
        let (tx, rx) = command_channel(64);
        let mixer = Mixer::new(SR, fixed_block);
        let feedback = Feedback::new();
        let sequencer = Sequencer::new(SR);
        let runner = BlockRunner::new(
            fixed_block,
            out_channels,
            SR,
            mixer,
            sequencer,
            rx,
            feedback.clone(),
        );
        (runner, tx, feedback)
    }

    /// 向轨道 0 触发 A4 音符（队列在下一个块边界被消费）。
    fn note_on_69(tx: &mut Producer<EngineCommand>) {
        tx.push(EngineCommand::Note {
            track: 0,
            event: NoteEvent::On {
                note: 69,
                velocity: 1.0,
            },
        })
        .expect("队列未满");
    }

    #[test]
    fn phase_continuity_across_arbitrary_host_blocks() {
        let (mut runner, mut tx, _feedback) = make_runner(128, 2);
        note_on_69(&mut tx);
        // 用不整除固定块（377）的宿主缓冲渲染约 1 秒，验证拆块不破坏相位。
        let total_frames = 44_100usize;
        let mut interleaved = vec![0.0f32; total_frames * 2];
        let mut rendered = 0;
        while rendered < total_frames {
            let n = 377.min(total_frames - rendered);
            runner.render_interleaved(&mut interleaved[rendered * 2..(rendered + n) * 2]);
            rendered += n;
        }
        let left: Vec<f32> = interleaved.iter().copied().step_by(2).collect();
        let crossings = count_positive_crossings(&left);
        assert!(
            (crossings as i64 - 440).abs() <= 2,
            "crossings = {crossings}"
        );
    }

    #[test]
    fn channels_beyond_two_are_silent() {
        let (mut runner, mut tx, _feedback) = make_runner(128, 4);
        note_on_69(&mut tx);
        let mut out = vec![7.0f32; 256 * 4]; // 预填非零，验证被覆盖/清零
        runner.render_interleaved(&mut out);

        let frame = |i: usize| (out[i * 4], out[i * 4 + 1], out[i * 4 + 2], out[i * 4 + 3]);
        let (l, r, c2, c3) = frame(10);
        assert!(c2.abs() < 1e-12 && c3.abs() < 1e-12, "额外声道应为静音");
        assert!((l - r).abs() < 1e-9, "单声道源下 L/R 内容一致");
        assert!(
            l.abs() > 0.01 || out.iter().any(|s| s.abs() > 0.01),
            "应有非静音输出"
        );
    }

    #[test]
    fn mono_output_takes_left_channel() {
        let (mut runner, mut tx, _feedback) = make_runner(128, 1);
        note_on_69(&mut tx);
        let mut out = vec![0.0f32; 256];
        runner.render_interleaved(&mut out);
        assert!(out.iter().any(|s| s.abs() > 0.01), "应有非静音输出");
    }

    #[test]
    fn command_takes_effect_at_block_boundary() {
        let (mut runner, mut tx, _feedback) = make_runner(128, 2);
        note_on_69(&mut tx);
        let half_frames = 11_025usize; // 0.25 s
        let mut out = vec![0.0f32; 44_100 * 2];

        runner.render_interleaved(&mut out[..half_frames * 2]);
        tx.push(EngineCommand::SetTrackParam {
            track: 0,
            id: ParamId::SineFrequency,
            value: 880.0,
        })
        .expect("队列未满");
        runner.render_interleaved(&mut out[half_frames * 2..]);

        // 后半段（0.75 s）应为 880 Hz → 660 次正向过零（±4 含过渡块误差）。
        let second_half: Vec<f32> = out[half_frames * 2..].iter().copied().step_by(2).collect();
        let crossings = count_positive_crossings(&second_half);
        assert!(
            (crossings as f64 - 660.0).abs() <= 4.0,
            "crossings = {crossings}"
        );
    }

    #[test]
    fn reports_level_position_and_transport() {
        let (mut runner, mut tx, feedback) = make_runner(128, 2);
        note_on_69(&mut tx);
        let mut out = vec![0.0f32; 300 * 2];
        runner.render_interleaved(&mut out);

        // 300 帧 → 消费 3 个块；position 为当前正在缓冲的块起点（第 3 块 = 256）。
        assert_eq!(feedback.position.load(Ordering::Relaxed), 256);
        assert!(feedback.level.load() > 0.0, "有信号输出时峰值电平应为正");
        assert!(feedback.track_levels[0].load() > 0.0, "轨道 0 电平应为正");
    }

    #[test]
    fn note_off_fades_stream_to_silence() {
        let (mut runner, mut tx, feedback) = make_runner(128, 2);
        note_on_69(&mut tx);
        let mut out = vec![0.0f32; 22_050 * 2];
        runner.render_interleaved(&mut out); // 0.5 s sustain

        tx.push(EngineCommand::Note {
            track: 0,
            event: NoteEvent::Off { note: 69 },
        })
        .expect("队列未满");
        runner.render_interleaved(&mut out); // release 0.2 s + 余量

        assert!(
            feedback.level.load() < 0.02,
            "NoteOff 后输出应衰减至静音，实际 {}",
            feedback.level.load()
        );
    }

    #[test]
    fn silent_until_first_note() {
        let (mut runner, _tx, feedback) = make_runner(128, 2);
        let mut out = vec![0.0f32; 1024 * 2];
        runner.render_interleaved(&mut out);
        assert!(feedback.level.load() < 1e-9, "未触发音符时应为静音");
    }

    /// 音序器端到端：命令队列下发模式与走带 → 合成器发声 → 状态回传。
    #[test]
    fn sequencer_drives_synth_through_command_queue() {
        let (mut runner, mut tx, feedback) = make_runner(128, 2);

        // 模式：轨道 0 步 0/2/4 触发 A4；BPM 240。
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        for i in [0usize, 2, 4] {
            pattern[i] = SequencerStep {
                enabled: true,
                note: 69,
                velocity: 0.9,
            };
        }
        tx.push(EngineCommand::Sequencer(SequencerCommand::SetPattern {
            track: 0,
            steps: pattern,
        }))
        .expect("队列未满");
        tx.push(EngineCommand::Sequencer(SequencerCommand::SetBpm(240.0)))
            .expect("队列未满");
        tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
            .expect("队列未满");

        let mut out = vec![0.0f32; 44_100]; // 1 s，单声道即可
        runner.render_interleaved(&mut out);

        assert!(feedback.playing.load(Ordering::Relaxed), "应处于播放状态");
        // 直接验证音频内容（level 原子量只反映最后一个块，结尾可能恰逢
        // 新音符从零相位起音，不适合做"是否发声"的判据）。
        let audio_peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(
            audio_peak > 0.05,
            "音序器触发后应有可闻输出，峰值 {audio_peak}"
        );

        // 停止 → 走带状态回传翻转为 false。
        tx.push(EngineCommand::Sequencer(SequencerCommand::Stop))
            .expect("队列未满");
        runner.render_interleaved(&mut out);
        assert!(
            !feedback.playing.load(Ordering::Relaxed),
            "停止后应不再是播放状态"
        );
        assert!(feedback.step.load(Ordering::Relaxed) < PATTERN_STEPS as u32);
    }

    #[test]
    fn silent_when_sequencer_not_playing() {
        let (mut runner, mut tx, feedback) = make_runner(128, 2);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = SequencerStep {
            enabled: true,
            note: 60,
            velocity: 1.0,
        };
        tx.push(EngineCommand::Sequencer(SequencerCommand::SetPattern {
            track: 0,
            steps: pattern,
        }))
        .expect("队列未满");
        let mut out = vec![0.0f32; 22_050];
        runner.render_interleaved(&mut out);
        assert!(feedback.level.load() < 1e-9, "未按播放时音序器不应发声");
    }

    #[test]
    fn automation_drives_track_gain_during_playback() {
        let (mut runner, mut tx, feedback) = make_runner(128, 2);
        // 增益曲线：tick 0 = 1.0 → tick 96 = 0.0（120 BPM / 44.1 kHz ≈ 0.5 s）。
        let mut curve = RtAutomation {
            param: ParamId::TrackGain,
            enabled: true,
            ..RtAutomation::default()
        };
        curve.push(RtAutoPoint {
            tick: 0,
            value: 1.0,
        });
        curve.push(RtAutoPoint {
            tick: 96,
            value: 0.0,
        });
        tx.push(EngineCommand::Sequencer(SequencerCommand::SetAutomation {
            track: 0,
            automation: curve,
        }))
        .expect("队列未满");
        tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
            .expect("队列未满");
        // 注意：Play 会清理手动音符，故音符事件需在 Play 之后入队。
        note_on_69(&mut tx);

        let mut chunk = vec![0.0f32; 4410 * 2]; // 0.1 s
        let mut early_max = 0.0f32;
        for _ in 0..2 {
            runner.render_interleaved(&mut chunk);
            early_max = early_max.max(feedback.track_levels[0].load());
        }
        assert!(
            early_max > 0.3,
            "自动化起点附近增益 ≈ 1，应有较大电平：{early_max}"
        );

        // 中间 0.3 s 忽略（曲线衰减段），之后增益已归零。
        for _ in 0..3 {
            runner.render_interleaved(&mut chunk);
        }
        let mut late_max = 0.0f32;
        for _ in 0..5 {
            runner.render_interleaved(&mut chunk);
            late_max = late_max.max(feedback.track_levels[0].load());
        }
        assert!(
            late_max < 0.02,
            "0.5 s 后自动化增益归零，电平应近静音：{late_max}"
        );
    }

    #[test]
    fn tracks_play_in_parallel() {
        let (mut runner, mut tx, feedback) = make_runner(128, 2);
        // 两条轨道同时触发不同音符：两轨电平均应为正。
        for (track, note) in [(0usize, 60u8), (1usize, 67u8)] {
            tx.push(EngineCommand::Note {
                track,
                event: NoteEvent::On {
                    note,
                    velocity: 1.0,
                },
            })
            .expect("队列未满");
        }
        let mut out = vec![0.0f32; 4096 * 2];
        runner.render_interleaved(&mut out);
        assert!(feedback.track_levels[0].load() > 0.0, "轨道 0 应有电平");
        assert!(feedback.track_levels[1].load() > 0.0, "轨道 1 应有电平");
        assert!(
            feedback.track_levels[2].load() < 1e-9 && feedback.track_levels[3].load() < 1e-9,
            "未使用的轨道应为静音"
        );
    }
}
