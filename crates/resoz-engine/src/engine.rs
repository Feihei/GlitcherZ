//! 音频引擎的非实时控制端。
//!
//! [`AudioEngine`] 拥有 cpal 输出流与命令队列的 producer；所有实时路径
//! 代码都在 [`crate::realtime::BlockRunner`] 中，随流回调启动时 move 进
//! 音频线程。UI / 控制代码永远不直接触碰实时数据。

use std::sync::atomic::Ordering;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::convert;
use crate::realtime::{
    BlockRunner, COMMAND_QUEUE_CAPACITY, EngineCommand, FIXED_BLOCK_SIZE, Feedback, GraphCompiler,
    GraphUpdate, Sequencer, command_channel, recycle_channel, structure_channel,
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
    /// 结构更新队列：图拓扑变更（换计划 / 装卸节点）的发送端。
    structure: rtrb::Producer<GraphUpdate>,
    /// 节点回收消费端：UI 定期 [`AudioEngine::reclaim_nodes`] 排空，
    /// 释放旧插件节点与对应守卫。
    recycle: rtrb::Consumer<crate::realtime::RecycledNode>,
    /// 控制侧图状态（非实时）：拓扑权威副本 + 执行计划编译器。
    /// 全 `Copy`，队列 push 失败时可整快照回滚。
    compiler: GraphCompiler,
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

        // 初始处理图：默认拓扑（复刻旧 Mixer）：每轨 声源 → 延迟 → 通道条
        // → 主输出。拓扑变更经 GraphUpdate 队列整体换入（spec D5）。
        let (compiler, graph) = GraphCompiler::default_graph(sample_rate_f32, FIXED_BLOCK_SIZE);

        let feedback = Feedback::new();
        let sequencer = Sequencer::new(sample_rate_f32);
        let (structure_tx, structure_rx) = structure_channel();
        let (recycle_tx, recycle_rx) = recycle_channel();
        let mut runner = BlockRunner::new(
            FIXED_BLOCK_SIZE,
            stream_config.channels as usize,
            sample_rate_f32,
            graph,
            sequencer,
            command_rx,
            structure_rx,
            recycle_tx,
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
            structure: structure_tx,
            recycle: recycle_rx,
            compiler,
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

    /// 音序器音乐位置（tick，自播放起点累计；用于卷帘播放头）。
    pub fn musical_ticks(&self) -> f32 {
        self.feedback.position_ticks.load()
    }

    /// 音序器是否播放中。
    pub fn is_playing(&self) -> bool {
        self.feedback.playing.load(Ordering::Relaxed)
    }

    /// 将指定轨道的声源换装为外部节点（如 CLAP 乐器）。
    /// 节点由调用方在**非实时线程**构造；音频线程在下一个块边界换入，
    /// 被替换的旧节点经回执队列送回，由 [`AudioEngine::reclaim_nodes`]
    /// 交还控制线程释放。
    /// 返回分配的节点 ID；`None` 表示越界或队列满（命令被丢弃）。
    pub fn set_external_voice(
        &mut self,
        track: usize,
        node: Box<dyn crate::realtime::Processor>,
    ) -> Option<u64> {
        let snapshot = self.compiler;
        match self.compiler.set_voice(track, node) {
            Ok((update, node_id)) => {
                if self.structure.push(update).is_ok() {
                    Some(node_id)
                } else {
                    // 队列满：回滚编译器快照（update 随节点一同被丢弃）。
                    self.compiler = snapshot;
                    None
                }
            }
            Err(node) => {
                drop(node);
                None
            }
        }
    }

    /// 向指定轨道效果链末端追加外部节点（如 CLAP 效果器）。
    /// 返回分配的节点 ID；`None` 表示越界/槽满/队列满（节点被释放）。
    pub fn insert_external_effect(
        &mut self,
        track: usize,
        node: Box<dyn crate::realtime::Processor>,
    ) -> Option<u64> {
        let snapshot = self.compiler;
        match self.compiler.insert_effect(track, node) {
            Ok((update, node_id)) => {
                if self.structure.push(update).is_ok() {
                    Some(node_id)
                } else {
                    self.compiler = snapshot;
                    None
                }
            }
            Err(node) => {
                drop(node);
                None
            }
        }
    }

    /// 清空指定轨道的全部外部效果器节点（节点经回执队列回收）。
    pub fn clear_external_effects(&mut self, track: usize) -> bool {
        let snapshot = self.compiler;
        match self.compiler.clear_effects(track) {
            Ok(update) => {
                if self.structure.push(update).is_ok() {
                    true
                } else {
                    self.compiler = snapshot;
                    false
                }
            }
            Err(_) => false,
        }
    }

    /// 排空节点回收回执队列：在**控制线程**释放旧节点（含输出缓冲）
    /// 并返回其 ID（调用方据此丢弃配对的插件守卫，安全卸载动态库）。
    pub fn reclaim_nodes(&mut self) -> Vec<u64> {
        let mut ids = Vec::new();
        while let Ok(recycled) = self.recycle.pop() {
            let crate::realtime::RecycledNode {
                node_id,
                node,
                output,
            } = recycled;
            drop(node); // 控制线程释放：实时铁律仅约束音频回调
            drop(output);
            ids.push(node_id);
        }
        ids
    }

    /// 将指定轨道的声源恢复为内置合成器（旧外部节点经回执队列回收，
    /// 切回插件时需重新实例化）。
    pub fn set_builtin_voice(&mut self, track: usize) -> bool {
        let snapshot = self.compiler;
        match self.compiler.activate_builtin_voice(track) {
            Ok(update) => {
                if self.structure.push(update).is_ok() {
                    true
                } else {
                    self.compiler = snapshot;
                    false
                }
            }
            Err(_) => false,
        }
    }

    /// 指定轨道的 post-fader 峰值电平（越界返回 0）。
    pub fn track_level(&self, track: usize) -> f32 {
        self.feedback
            .track_levels
            .get(track)
            .map_or(0.0, |level| level.load())
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
