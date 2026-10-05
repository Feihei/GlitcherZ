//! # 实时音频路径（Realtime）
//!
//! 本模块内所有会在**音频回调线程**上执行的代码必须遵守实时铁律
//! （AGENTS.md「实时音频铁律」，最高优先级）：
//!
//! 1. **禁止内存分配/释放**（`Vec::new`、`Box::new`、`String`、`format!` 等）；
//!    所有缓冲区在构造期（非实时线程）预分配。
//! 2. **禁止锁**（`Mutex`、`RwLock`）；跨线程通信只允许无锁队列
//!    （`rtrb`）与原子变量。
//! 3. **禁止系统调用**：无文件 IO、无日志（`log!` / `println!`）、无插件加载。
//! 4. **禁止 panic**：不使用 `expect` / `unwrap` / 可 panic 的索引等；
//!    边界情况一律降级（静音输出、丢弃命令）。
//!
//! 公开方法在文档中以 **RT-safe** 标注是否可在音频回调上调用。
//! 构造函数允许分配，但只能在非实时线程调用；完成后即 move 进音频线程。
//!
//! 模块结构：
//! - [`buffer`]：预分配平面缓冲 [`AudioBuffer`]；
//! - [`context`]：每块上下文 [`ProcessContext`]；
//! - [`processor`]：[`Processor`] trait 与 [`ProcessorChain`]；
//! - [`sine`] / [`gain`]：正弦音源与增益效果器（`Processor` 参考实现）；
//! - [`envelope`] / [`svf`] / [`synth`]：ADSR 包络、SVF 滤波器与完整合成器；
//! - [`commands`]：`rtrb` 命令队列与 [`EngineCommand`] / [`NoteEvent`] / [`ParamId`]；
//! - [`atomic`]：[`AtomicF32`]（音频线程 → UI 的标量回传）；
//! - [`block`]：[`BlockRunner`] —— 把宿主（cpal）任意长度的交错缓冲拆成
//!   固定块、在块边界消费命令、回传状态。

pub mod atomic;
pub mod block;
pub mod buffer;
pub mod commands;
pub mod context;
pub mod delay;
pub mod envelope;
pub mod gain;
pub mod mixer;
pub mod processor;
pub mod sequencer;
pub mod sine;
pub mod svf;
pub mod synth;

/// 固定处理块大小（样本数）。见 AGENTS.md「处理图」（建议 64 或 128）。
pub const FIXED_BLOCK_SIZE: usize = 128;

/// 项目轨道数（第 5 步固定 4 轨；与 `resoz_model::NUM_TRACKS` 保持一致）。
pub const NUM_TRACKS: usize = 4;

pub use atomic::AtomicF32;
pub use block::{BlockRunner, Feedback};
pub use buffer::AudioBuffer;
pub use commands::{
    COMMAND_QUEUE_CAPACITY, EngineCommand, NoteEvent, ParamId, SequencerCommand, command_channel,
};
pub use context::ProcessContext;
pub use delay::DelayProcessor;
pub use envelope::AdsrProcessor;
pub use gain::GainProcessor;
pub use mixer::{Mixer, MixerLevels};
pub use processor::{Processor, ProcessorChain};
pub use sequencer::{
    MAX_CLIP_NOTES, PATTERN_STEPS, RtClip, RtNote, Sequencer, SequencerStep, TICKS_PER_QUARTER,
    TICKS_PER_STEP, TrackSource,
};
pub use sine::SineProcessor;
pub use svf::{FilterMode, SvfProcessor};
pub use synth::{SynthProcessor, note_to_frequency};
