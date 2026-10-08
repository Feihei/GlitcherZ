//! # resoz-clap-host
//!
//! CLAP 插件宿主适配层（非实时基础设施）：ResoZ 引擎与 CLAP 插件
//! 生态之间的桥梁。
//!
//! 职责：
//! - 扫描 CLAP 插件目录（标准目录 + `RESOZ_CLAP_PATH` 环境变量）；
//! - 枚举插件描述（ID / 名称 / 是否乐器）；
//! - 在**非实时线程**实例化插件，产出可在音频回调中运行的
//!   [`ClapNode`]（实时铁律约束适用于其 `process` 路径）；
//! - 枚举插件实例的参数列表（[`ClapInstance::list_params`]，params
//!   扩展直调，见 `raw` 模块）。
//!
//! 桥接模型：
//! - **音符**：[`Processor::note_event`] 把 `NoteOn/Off` 暂存进定容
//!   事件槽，下一次 `process` 以 CLAP 输入事件发出（块粒度）；
//! - **参数**：`ParamId::PluginParam(u32)` 同理以 `ParamValueEvent`
//!   发出——自动化求值器写入的插件参数由此天然桥接；
//! - **音频**：乐器插件（无输入端口）渲染到节点内预分配的立体声
//!   临时缓冲，再拷贝进 [`AudioBuffer`]。
//!
//! 插件动态库生命周期由 [`ClapInstanceGuard`] 承担，必须与
//! [`ClapNode`] 同寿命（见 [`ClapInstance::spawn_rt`]）。

#![deny(unsafe_code)]
// unsafe 仅存在于 raw 模块（动态库加载边界），理由与验证见其文档。

pub mod raw;

use std::path::{Path, PathBuf};

use clack_common::events::event_types::{NoteOffEvent, NoteOnEvent, ParamValueEvent};
use clack_extensions::log::{HostLog, HostLogImpl};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamRescanFlags,
};
use clack_host::entry::PluginEntryError;
use clack_host::prelude::*;
use clack_host::process::audio_buffers::InputChannel;

use resoz_engine::realtime::FIXED_BLOCK_SIZE;
use resoz_engine::realtime::buffer::AudioBuffer;
use resoz_engine::realtime::commands::ParamId;
use resoz_engine::realtime::context::ProcessContext;
use resoz_engine::realtime::processor::Processor;

/// 一次 `process` 最多排队的桥接事件数（音符 + 参数合计）。
pub const MAX_PENDING_EVENTS: usize = 32;

/// 插件扫描目录环境变量的名字（`PATH` 风格分隔列表）。
pub const CLAP_PATH_ENV: &str = "RESOZ_CLAP_PATH";

/// 乐器插件的空输入端口缓冲类型（无音频输入）。
type NoInputPorts = [AudioPortBuffer<
    std::iter::Empty<InputChannel<'static, f32>>,
    std::iter::Empty<InputChannel<'static, f64>>,
>; 0];

/// 宿主侧错误。
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("加载插件动态库失败：{0}")]
    Load(#[source] PluginEntryError),
    #[error("插件实例化失败：{0}")]
    Instance(#[source] PluginInstanceError),
    #[error("插件激活失败：{0}")]
    Activate(#[source] PluginInstanceError),
    #[error("插件处理启动失败：{0}")]
    Start(#[source] PluginInstanceError),
    #[error("宿主信息构造失败")]
    HostInfo,
    #[error("插件缺少工厂或有效 ID")]
    NoPlugin,
    #[error("插件未实现 params 扩展")]
    NoParams,
}

// ---------------------------------------------------------------------------
// 插件扫描
// ---------------------------------------------------------------------------

/// 平台相关的 CLAP 插件标准搜索目录。
fn standard_clap_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        dirs.push(
            PathBuf::from(program_files)
                .join("Common Files")
                .join("CLAP"),
        );
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let local_app_data = PathBuf::from(local_app_data);
        dirs.push(local_app_data.join("Programs").join("Common").join("CLAP"));
        dirs.push(local_app_data.join("CLAP"));
    }
    if let Some(app_data) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(app_data).join("CLAP"));
    }
    dirs
}

/// 全部搜索目录：`RESOZ_CLAP_PATH` 优先，其后是平台标准目录。
pub fn clap_search_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(extra) = std::env::var_os(CLAP_PATH_ENV) {
        dirs.extend(std::env::split_paths(&extra));
    }
    dirs.extend(standard_clap_dirs());
    dirs
}

/// 扫描全部搜索目录下的 `.clap` 动态库文件（非递归，符合 CLAP 规范惯例）。
pub fn scan_clap_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in clap_search_paths() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|ext| ext == "clap").unwrap_or(false) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// 一个可实例化插件的描述信息。
#[derive(Debug, Clone, PartialEq)]
pub struct ClapPluginInfo {
    /// 动态库路径。
    pub path: PathBuf,
    /// 插件唯一 ID（CLAP 规范要求稳定）。
    pub id: String,
    /// 显示名称。
    pub name: String,
    /// 是否为乐器（合成器；`instrument` 特性标记）。
    pub is_instrument: bool,
}

/// 一个插件参数的元数据快照（实例化时经 params 扩展读出）。
#[derive(Debug, Clone, PartialEq)]
pub struct ClapParamInfo {
    /// 插件侧参数 ID（`ParamId::PluginParam` 桥接用）。
    pub id: u32,
    /// 显示名称。
    pub name: String,
    /// 模块路径（`/` 分隔，可空）。
    pub module: String,
    /// 最小值（插件值域）。
    pub min_value: f64,
    /// 最大值（插件值域）。
    pub max_value: f64,
    /// 默认值（插件值域）。
    pub default_value: f64,
}

/// 枚举给定 `.clap` 动态库内的全部插件。加载失败的库被跳过并记日志。
pub fn list_clap_plugins_in(path: &Path) -> Vec<ClapPluginInfo> {
    let entry = match raw::load_entry(path) {
        Ok(entry) => entry,
        Err(err) => {
            log::warn!("跳过无法加载的 CLAP 插件 {}：{err}", path.display());
            return Vec::new();
        }
    };
    let Some(factory) = entry.get_plugin_factory() else {
        return Vec::new();
    };
    let mut infos = Vec::new();
    for index in 0..factory.plugin_count() {
        let Some(descriptor) = factory.plugin_descriptor(index) else {
            continue;
        };
        let Some(id) = descriptor.id() else {
            continue;
        };
        let id = id.to_string_lossy().into_owned();
        if id.is_empty() {
            continue;
        }
        infos.push(ClapPluginInfo {
            path: path.to_path_buf(),
            name: descriptor
                .name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| id.clone()),
            is_instrument: descriptor
                .features()
                .any(|feature| feature.to_string_lossy() == "instrument"),
            id,
        });
    }
    infos
}

/// 扫描并枚举全部目录里的全部插件。
pub fn list_clap_plugins() -> Vec<ClapPluginInfo> {
    let mut infos = Vec::new();
    for path in scan_clap_files() {
        infos.extend(list_clap_plugins_in(&path));
    }
    infos
}

// ---------------------------------------------------------------------------
// 宿主回调实现
// ---------------------------------------------------------------------------

struct ResoZHostShared;

impl SharedHandler<'_> for ResoZHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

impl HostLogImpl for ResoZHostShared {
    fn log(&self, severity: clack_extensions::log::LogSeverity, message: &str) {
        log::info!(target: "clap_plugin", "[{severity}] {message}");
    }
}

impl HostParamsImplShared for ResoZHostShared {
    fn request_flush(&self) {
        // 简化宿主：插件请求 flush 时，下一次 process 会带上全部暂存事件。
    }
}

struct ResoZHostMainThread;

impl MainThreadHandler<'_> for ResoZHostMainThread {}

impl HostParamsImplMainThread for ResoZHostMainThread {
    fn rescan(&self, _flags: ParamRescanFlags) {}
    fn clear(&self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

struct ResoZHostHandlers;

impl HostHandlers for ResoZHostHandlers {
    type Shared<'a> = ResoZHostShared;
    type MainThread<'a> = ResoZHostMainThread;
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder.register::<HostLog>();
        builder.register::<HostParams>();
    }
}

// ---------------------------------------------------------------------------
// 实时节点
// ---------------------------------------------------------------------------

/// 暂存的桥接事件。
#[derive(Debug, Clone, Copy)]
pub enum PendingClapEvent {
    /// 音符按下。
    NoteOn { note: u8, velocity: f32 },
    /// 音符松开。
    NoteOff { note: u8 },
    /// 插件参数值（`ParamId::PluginParam` 桥接）。
    Param { id: u32, value: f32 },
}

/// 可在音频回调中运行的 CLAP 乐器节点。
///
/// 构造在非实时线程（[`ClapInstance::spawn_rt`]），随后 move 进音频
/// 线程。`process` / `note_event` / `set_param` 均遵守实时铁律：
/// 无分配（事件与缓冲全部构造期预分配）、无锁、无 panic。
pub struct ClapNode {
    /// 统一状态处理器：内部在 Started/Stopped 间切换，`process` 可失败
    /// （运行时检查），失败即输出静音。
    processor: PluginAudioProcessor<ResoZHostHandlers>,
    input_events: EventBuffer,
    output_events: EventBuffer,
    input_ports: AudioPorts,
    output_ports: AudioPorts,
    out_l: Box<[f32]>,
    out_r: Box<[f32]>,
    pending: [PendingClapEvent; MAX_PENDING_EVENTS],
    pending_count: usize,
}

/// 持有插件动态库与实例的守卫。**必须与 [`ClapNode`] 同寿命**：
/// 丢守护卫会卸载插件动态库，使节点的处理指针悬空。
pub struct ClapInstanceGuard {
    _entry: PluginEntry,
    _instance: PluginInstance<ResoZHostHandlers>,
}

/// 一次性实例化产物：控制侧守卫 + 实时节点。
pub struct ClapInstance {
    /// 控制侧存活守卫（UI/引擎线程持有）。
    pub guard: ClapInstanceGuard,
    /// 实时侧节点（move 进音频线程）。
    pub node: ClapNode,
}

/// 实例化时的静态配置。
pub struct ClapInstanceConfig {
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// 最大块大小（应与 [`FIXED_BLOCK_SIZE`] 一致）。
    pub max_frames: u32,
}

impl ClapInstance {
    /// 加载并激活插件，产出实时节点。**必须在非实时线程调用**
    /// （涉及动态库加载与插件内部分配）。
    pub fn spawn_rt(
        path: &Path,
        plugin_id: &str,
        config: ClapInstanceConfig,
    ) -> Result<Self, HostError> {
        let entry = raw::load_entry(path).map_err(HostError::Load)?;
        let Ok(plugin_id) = std::ffi::CString::new(plugin_id) else {
            return Err(HostError::NoPlugin);
        };
        let host_info = HostInfo::new(
            "ResoZ",
            "ResoZ Project",
            "https://github.com/resoz",
            env!("CARGO_PKG_VERSION"),
        )
        .map_err(|_| HostError::HostInfo)?;
        let mut instance = PluginInstance::<ResoZHostHandlers>::new(
            |_| ResoZHostShared,
            |_| ResoZHostMainThread,
            &entry,
            &plugin_id,
            &host_info,
        )
        .map_err(HostError::Instance)?;

        let stopped = instance
            .activate(
                |_, _| (),
                PluginAudioConfiguration {
                    sample_rate: f64::from(config.sample_rate),
                    min_frames_count: config.max_frames,
                    max_frames_count: config.max_frames,
                },
            )
            .map_err(HostError::Activate)?;
        // 简化宿主：在控制线程激活（未注册 thread-check 强制，与多数
        // 简易宿主一致）；process 前需经 as_started_mut 启动。
        let processor: PluginAudioProcessor<ResoZHostHandlers> = stopped.into();

        Ok(Self {
            guard: ClapInstanceGuard {
                _entry: entry,
                _instance: instance,
            },
            node: ClapNode::new(processor, config),
        })
    }

    /// 枚举插件暴露的全部参数（元数据快照）。
    ///
    /// 在**非实时线程**调用（直调插件主线程回调）。插件未实现 params
    /// 扩展时返回空列表；个别参数读取失败被跳过而非整体失败。
    /// RT-safe 无关——本方法不触碰实时节点。
    pub fn list_params(&self) -> Vec<ClapParamInfo> {
        raw::enumerate_params(self.guard._instance.raw_instance())
    }
}

impl ClapNode {
    fn new(
        processor: impl Into<PluginAudioProcessor<ResoZHostHandlers>>,
        config: ClapInstanceConfig,
    ) -> Self {
        let frames = config.max_frames.max(FIXED_BLOCK_SIZE as u32) as usize;
        Self {
            processor: processor.into(),
            input_events: EventBuffer::with_capacity(MAX_PENDING_EVENTS),
            output_events: EventBuffer::with_capacity(MAX_PENDING_EVENTS),
            input_ports: AudioPorts::with_capacity(0, 0),
            output_ports: AudioPorts::with_capacity(2, 1),
            out_l: vec![0.0; frames].into_boxed_slice(),
            out_r: vec![0.0; frames].into_boxed_slice(),
            pending: [PendingClapEvent::NoteOff { note: 0 }; MAX_PENDING_EVENTS],
            pending_count: 0,
        }
    }

    fn stash(&mut self, event: PendingClapEvent) -> bool {
        if self.pending_count >= MAX_PENDING_EVENTS {
            return false; // 满载丢弃（非关键路径）
        }
        self.pending[self.pending_count] = event;
        self.pending_count += 1;
        true
    }
}

impl Processor for ClapNode {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = ctx.frames.min(self.out_l.len()).min(buffer.capacity());

        // 1. 暂存事件 → CLAP 输入事件（块粒度）。
        self.input_events.clear();
        for event in &self.pending[..self.pending_count] {
            match *event {
                PendingClapEvent::NoteOn { note, velocity } => {
                    self.input_events.push(&NoteOnEvent::new(
                        0,
                        Pckn::new(0u16, 0u16, u16::from(note), u32::from(note)),
                        f64::from(velocity),
                    ))
                }
                PendingClapEvent::NoteOff { note } => self.input_events.push(&NoteOffEvent::new(
                    0,
                    Pckn::new(0u16, 0u16, u16::from(note), u32::from(note)),
                    0.0,
                )),
                PendingClapEvent::Param { id, value } => {
                    self.input_events.push(&ParamValueEvent::new(
                        0,
                        ClapId::new(id),
                        Pckn::new(0u16, 0u16, 0u16, 0u32),
                        f64::from(value),
                    ))
                }
            }
        }
        self.pending_count = 0;
        self.output_events.clear();

        // 2. 音频缓冲：乐器无输入；输出指向节点内预分配临时缓冲。
        //    拆字段借用：两个声道缓冲与处理器同时可变。
        let Self {
            out_l,
            out_r,
            output_ports,
            input_ports,
            input_events,
            output_events,
            processor,
            ..
        } = self;
        // 乐器无输入端口：空端口列表。
        let no_inputs: NoInputPorts = [];
        let input_audio = input_ports.with_input_buffers(no_inputs);
        for channel in [&mut out_l[..frames], &mut out_r[..frames]] {
            channel.fill(0.0);
        }
        let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                [&mut out_l[..frames], &mut out_r[..frames]].into_iter(),
            ),
        }]);

        // 3. 调用插件。首次调用时启动处理；任何失败都让输出保持清零
        //    （无 panic）。
        let started = match processor.as_started_mut() {
            Ok(started) => started,
            // 停止态：尝试启动（首块）；再失败则保持静音。
            Err(_) => match processor.start_processing() {
                Ok(started) => started,
                Err(_) => return,
            },
        };
        {
            let _ = started.process(
                &input_audio,
                &mut output_audio,
                &input_events.as_input(),
                &mut output_events.as_output(),
                None,
                None,
            );
        }

        // 4. 拷贝进引擎平面缓冲（越界防御性钳制，不 panic）。
        for (channel_index, source) in [&out_l[..frames], &out_r[..frames]].into_iter().enumerate()
        {
            if let Some(destination) = buffer.channel_mut(channel_index) {
                for (index, sample) in destination.iter_mut().enumerate() {
                    *sample = source.get(index).copied().unwrap_or(0.0);
                }
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::PluginParam(param_id) => {
                self.stash(PendingClapEvent::Param {
                    id: param_id,
                    value,
                });
                true
            }
            _ => false,
        }
    }

    fn note_event(&mut self, event: resoz_engine::realtime::NoteEvent) -> bool {
        match event {
            resoz_engine::realtime::NoteEvent::On { note, velocity } => {
                self.stash(PendingClapEvent::NoteOn { note, velocity });
            }
            resoz_engine::realtime::NoteEvent::Off { note } => {
                self.stash(PendingClapEvent::NoteOff { note });
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClapInstance, ClapInstanceConfig, ClapPluginInfo, HostError, list_clap_plugins_in,
    };
    use resoz_engine::realtime::NoteEvent;
    use resoz_engine::realtime::buffer::AudioBuffer;
    use resoz_engine::realtime::commands::ParamId;
    use resoz_engine::realtime::context::ProcessContext;
    use resoz_engine::realtime::processor::Processor;

    use std::path::PathBuf;

    /// 测试插件的动态库路径（`cargo test --workspace` 会先构建 cdylib）。
    fn test_plugin_path() -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug")
            .join(format!(
                "{}resoz_test_plugin{}",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX
            ));
        path.exists().then_some(path)
    }

    fn spawn_test_plugin() -> Option<(ClapInstance, ProcessContext)> {
        let Some(path) = test_plugin_path() else {
            eprintln!("跳过：测试插件 cdylib 未构建（请用 cargo test --workspace）");
            return None;
        };
        let instance = ClapInstance::spawn_rt(
            &path,
            "org.resoz.sine",
            ClapInstanceConfig {
                sample_rate: 48_000,
                max_frames: 128,
            },
        )
        .expect("实例化测试插件失败");
        let ctx = ProcessContext {
            sample_rate: 48_000.0,
            frames: 128,
            playback_sample_pos: 0,
        };
        Some((instance, ctx))
    }

    #[test]
    fn end_to_end_sine_instrument() {
        let Some((mut instance, ctx)) = spawn_test_plugin() else {
            return;
        };
        let node = &mut instance.node;

        // 未触发音符 → 静音。
        let mut buffer = AudioBuffer::new(2, 128);
        buffer.set_len(128);
        buffer.clear();
        node.process(&ctx, &mut buffer);
        assert!(
            buffer
                .channel(0)
                .unwrap()
                .iter()
                .all(|sample| sample.abs() < 1e-9),
            "无音符时应静音"
        );

        // NoteOn(A4) → 正弦输出，两声道一致。
        assert!(node.note_event(NoteEvent::On {
            note: 69,
            velocity: 1.0
        }));
        node.process(&ctx, &mut buffer);
        let peak_l = buffer
            .channel(0)
            .unwrap()
            .iter()
            .fold(0.0f32, |acc, s| acc.max(s.abs()));
        assert!(peak_l > 0.1, "NoteOn 后应有明显输出，峰值 {peak_l}");
        // 两声道来自同一单声部渲染，内容应一致。
        let same = buffer
            .channel(1)
            .unwrap()
            .iter()
            .zip(buffer.channel(0).unwrap())
            .all(|(r, l)| (r - l).abs() < 1e-9);
        assert!(same, "立体声两声道应一致（单声部源）");

        // NoteOff → 静音（乐器无释放尾音）。
        assert!(node.note_event(NoteEvent::Off { note: 69 }));
        node.process(&ctx, &mut buffer);
        assert!(
            buffer
                .channel(0)
                .unwrap()
                .iter()
                .all(|sample| sample.abs() < 1e-9),
            "NoteOff 后应静音"
        );
    }

    #[test]
    fn param_bridge_routes_and_ignores_builtin() {
        let Some((mut instance, ctx)) = spawn_test_plugin() else {
            return;
        };
        let node = &mut instance.node;

        // 插件参数被接受并入队（即使插件暂无参数实现，事件路径也畅通）。
        assert!(node.set_param(ParamId::PluginParam(1), 0.5));
        // 内置参数不被 ClapNode 消费（归内置节点管）。
        assert!(!node.set_param(ParamId::TrackGain, 0.5));
        assert!(!node.set_param(ParamId::FilterCutoff, 1_000.0));

        let mut buffer = AudioBuffer::new(2, 128);
        buffer.set_len(128);
        buffer.clear();
        node.process(&ctx, &mut buffer);
        assert!(
            buffer
                .channel(0)
                .unwrap()
                .iter()
                .all(|sample| sample.is_finite()),
            "参数事件不应破坏渲染"
        );
    }

    #[test]
    fn list_plugins_reports_our_sine() {
        let Some(path) = test_plugin_path() else {
            eprintln!("跳过：测试插件 cdylib 未构建");
            return;
        };
        let infos: Vec<ClapPluginInfo> = list_clap_plugins_in(&path);
        let sine = infos
            .iter()
            .find(|info| info.id == "org.resoz.sine")
            .expect("测试插件应被枚举");
        assert!(sine.is_instrument, "正弦插件应带 instrument 特性");
        assert_eq!(sine.name, "ResoZ Sine");
    }

    #[test]
    fn lists_plugin_params() {
        let Some((instance, _ctx)) = spawn_test_plugin() else {
            return;
        };
        let params = instance.list_params();
        assert_eq!(params.len(), 1, "测试插件应暴露一个参数");
        let param = &params[0];
        assert_eq!(param.id, 1);
        assert_eq!(param.name, "Amplitude");
        assert!((param.min_value - 0.0).abs() < 1e-9);
        assert!((param.max_value - 1.0).abs() < 1e-9);
        assert!((param.default_value - 0.5).abs() < 1e-9);
    }

    #[test]
    fn param_value_shapes_output() {
        let Some((mut instance, ctx)) = spawn_test_plugin() else {
            return;
        };
        let node = &mut instance.node;
        let mut buffer = AudioBuffer::new(2, 128);
        buffer.set_len(128);
        buffer.clear();

        // 振幅参数 0.25：NoteOn 后峰值应接近 0.25（正弦峰值）。
        assert!(node.set_param(ParamId::PluginParam(1), 0.25));
        assert!(node.note_event(NoteEvent::On {
            note: 69,
            velocity: 1.0
        }));
        node.process(&ctx, &mut buffer);
        let peak = buffer
            .channel(0)
            .unwrap()
            .iter()
            .fold(0.0f32, |acc, s| acc.max(s.abs()));
        assert!(
            (peak - 0.25).abs() < 0.02,
            "输出峰值应接近参数值 0.25，实际 {peak}"
        );
    }

    #[test]
    fn invalid_plugin_fails_gracefully() {
        let bogus = std::env::temp_dir().join("resoz_bogus.clap");
        std::fs::write(&bogus, b"not a dll").expect("写入失败");
        let result = ClapInstance::spawn_rt(
            &bogus,
            "org.resoz.bogus",
            ClapInstanceConfig {
                sample_rate: 48_000,
                max_frames: 128,
            },
        );
        let _ = std::fs::remove_file(&bogus);
        assert!(matches!(
            result,
            Err(HostError::Load(_) | HostError::NoPlugin)
        ));
    }
}
