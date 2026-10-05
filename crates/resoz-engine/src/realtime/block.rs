//! 固定块处理调度：cpal 回调 ↔ 处理链的桥梁。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rtrb::Consumer;

use crate::realtime::atomic::AtomicF32;
use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::EngineCommand;
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::{Processor, ProcessorChain};

/// 把宿主（cpal）交来的任意长度交错缓冲区拆成固定块交给处理链执行，
/// 再交错写回。实例在音频流启动前构造，随后 move 进音频回调线程，
/// 其生命周期内独占音频线程。
///
/// 每个固定块的执行顺序（`run_next_block`）：
/// 1. **块边界**消费全部待处理命令（队列空则立即返回）；
/// 2. 准备静音块（预分配 scratch）；
/// 3. 运行处理链；
/// 4. 回传峰值电平（`AtomicF32`）与播放位置（`AtomicU64`）。
pub struct BlockRunner {
    fixed_block: usize,
    out_channels: usize,
    sample_rate: f32,
    chain: ProcessorChain,
    scratch: AudioBuffer,
    scratch_pos: usize,
    commands: Consumer<EngineCommand>,
    level: Arc<AtomicF32>,
    position: Arc<AtomicU64>,
    played_samples: u64,
}

impl BlockRunner {
    /// 构造。**非 RT-safe**（分配 scratch），必须在启动音频流前调用。
    pub fn new(
        fixed_block: usize,
        out_channels: usize,
        sample_rate: f32,
        chain: ProcessorChain,
        commands: Consumer<EngineCommand>,
        level: Arc<AtomicF32>,
        position: Arc<AtomicU64>,
    ) -> Self {
        Self {
            fixed_block: fixed_block.max(1),
            out_channels: out_channels.max(1),
            sample_rate: if sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            chain,
            scratch: AudioBuffer::new(2, fixed_block.max(1)),
            scratch_pos: 0,
            commands,
            level,
            position,
            played_samples: 0,
        }
    }

    /// 渲染宿主给出的交错输出缓冲区。**RT-safe**（cpal 数据回调直接调用）。
    ///
    /// 声道映射：源为立体声（L/R）；输出第 0/1 声道取 L/R，其余声道静音；
    /// 单声道输出取 L。
    pub fn render_interleaved(&mut self, out: &mut [f32]) {
        for frame in out.chunks_mut(self.out_channels) {
            if self.scratch_pos >= self.scratch.len() {
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

    /// 执行一个固定块。只应在 [`BlockRunner::render_interleaved`] 内调用。
    fn run_next_block(&mut self) {
        // 1. 块边界：消费所有待处理命令（无锁队列，空则立即返回）。
        while let Ok(cmd) = self.commands.pop() {
            self.apply_command(cmd);
        }

        // 2. 准备静音块。
        let frames = self.fixed_block.min(self.scratch.capacity());
        self.scratch.set_len(frames);
        self.scratch.clear();

        // 3. 运行处理链。
        let ctx = ProcessContext {
            sample_rate: self.sample_rate,
            frames,
            playback_sample_pos: self.played_samples,
        };
        self.chain.process(&ctx, &mut self.scratch);

        // 4. 状态回传：峰值电平 + 播放位置（本块起始）。
        let mut peak = 0.0f32;
        for ch in 0..self.scratch.channels() {
            if let Some(channel) = self.scratch.channel(ch) {
                for sample in channel {
                    peak = peak.max(sample.abs());
                }
            }
        }
        self.level.store(peak);
        self.position.store(self.played_samples, Ordering::Relaxed);

        self.played_samples += frames as u64;
        self.scratch_pos = 0;
    }

    fn apply_command(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::SetParam { id, value } => {
                // 未被任何节点识别的参数静默丢弃（发送方负责只发送有效参数）。
                let _ = self.chain.set_param(id, value);
            }
        }
    }

    /// 读取 scratch 中 `(声道, 位置)` 的样本；越界一律静音（不 panic）。
    fn sample_at(&self, ch: usize, pos: usize) -> f32 {
        self.scratch
            .channel(ch)
            .and_then(|c| c.get(pos))
            .copied()
            .unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use rtrb::Producer;

    use super::BlockRunner;
    use crate::realtime::atomic::AtomicF32;
    use crate::realtime::commands::{EngineCommand, ParamId, command_channel};
    use crate::realtime::processor::ProcessorChain;
    use crate::realtime::sine::SineProcessor;
    use crate::testing::count_positive_crossings;

    const SR: f32 = 44_100.0;

    fn make_runner(
        fixed_block: usize,
        out_channels: usize,
    ) -> (
        BlockRunner,
        Producer<EngineCommand>,
        Arc<AtomicF32>,
        Arc<AtomicU64>,
    ) {
        let (tx, rx) = command_channel(64);
        let mut chain = ProcessorChain::new();
        chain.push(Box::new(SineProcessor::new(SR)));
        let level = Arc::new(AtomicF32::new(0.0));
        let position = Arc::new(AtomicU64::new(0));
        let runner = BlockRunner::new(
            fixed_block,
            out_channels,
            SR,
            chain,
            rx,
            level.clone(),
            position.clone(),
        );
        (runner, tx, level, position)
    }

    #[test]
    fn phase_continuity_across_arbitrary_host_blocks() {
        let (mut runner, _tx, _level, _position) = make_runner(128, 2);
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
        let (mut runner, _tx, _level, _position) = make_runner(128, 4);
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
        let (mut runner, _tx, _level, _position) = make_runner(128, 1);
        let mut out = vec![0.0f32; 256];
        runner.render_interleaved(&mut out);
        assert!(out.iter().any(|s| s.abs() > 0.01), "应有非静音输出");
    }

    #[test]
    fn command_takes_effect_at_block_boundary() {
        let (mut runner, mut tx, _level, _position) = make_runner(128, 2);
        let half_frames = 11_025usize; // 0.25 s
        let mut out = vec![0.0f32; 44_100 * 2];

        runner.render_interleaved(&mut out[..half_frames * 2]);
        tx.push(EngineCommand::SetParam {
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
    fn reports_level_and_position() {
        let (mut runner, _tx, level, position) = make_runner(128, 2);
        let mut out = vec![0.0f32; 300 * 2];
        runner.render_interleaved(&mut out);

        // 300 帧 → 消费 3 个块；position 为当前正在缓冲的块起点（第 3 块 = 256）。
        assert_eq!(position.load(Ordering::Relaxed), 256);
        assert!(level.load() > 0.0, "有信号输出时峰值电平应为正");
    }
}
