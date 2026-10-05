//! 音频引擎的非实时控制端。
//!
//! [`AudioEngine`] 拥有 cpal 输出流与命令队列的 producer；所有实时路径
//! 代码都在 [`crate::realtime::BlockRunner`] 中，随流回调启动时 move 进
//! 音频线程。UI / 控制代码永远不直接触碰实时数据。

use std::sync::atomic::Ordering;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::convert;
use crate::realtime::{
    BlockRunner, COMMAND_QUEUE_CAPACITY, EngineCommand, FIXED_BLOCK_SIZE, Feedback, GainProcessor,
    ProcessorChain, Sequencer, SvfProcessor, SynthProcessor, command_channel,
};

/// 引擎的基础信息（UI 显示用）。
#[derive(Debug, Clone)]
pub struct AudioEngineInfo {
    /// 输出设备名（拿不到时为"未知设备"）。
    pub device_name: String,
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// 输出声道数。
    pub out_channels: u16,
}

/// 引擎错误。应用层直接 `Display` 输出，不需要细分处理。
/// （cpal 0.18 起所有设备/流错误统一为 `cpal::Error`，内部含 `ErrorKind`。）
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("没有可用的音频输出设备")]
    NoOutputDevice,
    #[error("音频设备错误: {0}")]
    Device(#[from] cpal::Error),
}

/// 音频引擎。丢弃 [`AudioEngine`]（即丢弃内部的 `cpal::Stream`）即停止输出。
pub struct AudioEngine {
    _stream: cpal::Stream,
    commands: rtrb::Producer<EngineCommand>,
    feedback: Feedback,
    info: AudioEngineInfo,
}

impl AudioEngine {
    /// 使用宿主默认输出设备启动引擎。**必须在非实时线程调用**（构造期允许分配）。
    pub fn start() -> Result<Self, EngineError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(EngineError::NoOutputDevice)?;
        let supported = device.default_output_config()?;

        // 数据回调统一按 f32 处理；设备原生格式若不同，由 cpal 在回调边界转换。
        log::info!(
            "设备默认格式 {:?}，以 f32 构建输出流",
            supported.sample_format()
        );
        let stream_config = supported.config();
        let sample_rate_f32 = convert::sample_rate_to_f32(stream_config.sample_rate);
        let device_name = device.to_string();

        let (commands, command_rx) = command_channel(COMMAND_QUEUE_CAPACITY);

        // 初始处理图：合成器（振荡器 + ADSR）→ SVF 滤波器 → 主增益。
        // （图结构重建后续走双缓冲交换方案，见 AGENTS.md「处理图」。）
        let mut chain = ProcessorChain::new();
        chain.push(Box::new(SynthProcessor::new(sample_rate_f32)));
        chain.push(Box::new(SvfProcessor::new(sample_rate_f32)));
        chain.push(Box::new(GainProcessor::new(1.0)));

        let feedback = Feedback::new();
        let sequencer = Sequencer::new(sample_rate_f32);
        let mut runner = BlockRunner::new(
            FIXED_BLOCK_SIZE,
            stream_config.channels as usize,
            sample_rate_f32,
            chain,
            sequencer,
            command_rx,
            feedback.clone(),
        );

        // 数据回调：实时铁律约束的代码全部集中在 BlockRunner 内。
        let stream = device.build_output_stream(
            stream_config,
            move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                runner.render_interleaved(data);
            },
            move |err| {
                // 错误回调不在数据路径上（设备热插拔等），允许日志。
                log::error!("音频流错误: {err}");
            },
            None,
        )?;
        stream.play()?;

        Ok(Self {
            _stream: stream,
            commands,
            feedback,
            info: AudioEngineInfo {
                device_name,
                sample_rate: stream_config.sample_rate,
                out_channels: stream_config.channels,
            },
        })
    }

    /// 发送一条控制命令到音频线程（在下一个块边界生效）。
    /// 返回 `false` 表示队列已满、命令被丢弃（非关键路径，可安全忽略）。
    pub fn send(&mut self, command: EngineCommand) -> bool {
        self.commands.push(command).is_ok()
    }

    /// 最近一个音频块的峰值电平（两个声道取最大，约 0..1）。
    pub fn peak_level(&self) -> f32 {
        self.feedback.level.load()
    }

    /// 音序器当前步进索引（0..16，未播放时停留在最后一次位置）。
    pub fn current_step(&self) -> u32 {
        self.feedback.step.load(Ordering::Relaxed)
    }

    /// 音序器是否播放中。
    pub fn is_playing(&self) -> bool {
        self.feedback.playing.load(Ordering::Relaxed)
    }

    /// 播放位置（秒，f64：时间计算用 f64）。
    pub fn position_seconds(&self) -> f64 {
        convert::samples_to_seconds(
            self.feedback.position.load(Ordering::Relaxed),
            convert::sample_rate_to_f32(self.info.sample_rate),
        )
    }

    /// 引擎信息。
    pub fn info(&self) -> &AudioEngineInfo {
        &self.info
    }
}

/// 枚举宿主的输出设备名（诊断用，非实时）。
pub fn available_outputs() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.devices() else {
        return Vec::new();
    };
    devices.map(|d| d.to_string()).collect()
}
