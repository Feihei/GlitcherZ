#![forbid(unsafe_code)]

//! # resoz-engine
//!
//! ResoZ 音频引擎：处理图、调度、合成器与效果器。
//!
//! 模块划分：
//! - [`realtime`]：实时音频路径 —— [`realtime::AudioBuffer`]、
//!   [`realtime::Processor`] trait、[`realtime::BlockRunner`]（把宿主缓冲
//!   拆成固定块）与无锁命令队列。**音频回调上执行的代码全部集中于此**，
//!   并遵守模块文档中的实时铁律。
//! - [`engine`]：非实时控制端 —— [`AudioEngine`] 负责 cpal 设备管理、
//!   命令队列 producer 与状态回传（峰值电平、播放位置）。
//! - [`convert`]：f32（音频路径）/ f64（UI、时间）转换的集中点。
//!
//! 线程模型概览（详见 AGENTS.md）：
//!
//! ```text
//! UI/控制线程                         音频回调线程 (cpal)
//! ───────────                        ──────────────────
//! AudioEngine::send ── rtrb ──► BlockRunner（块边界消费命令）
//! AudioEngine::peak_level ◄─ AtomicF32 ◄─ 每块峰值
//! AudioEngine::position_seconds ◄─ AtomicU64 ◄─ 播放位置
//! ```

pub mod convert;
pub mod engine;
pub mod realtime;

#[cfg(test)]
pub(crate) mod testing;

pub use engine::{AudioEngine, AudioEngineInfo, EngineError, available_outputs};
