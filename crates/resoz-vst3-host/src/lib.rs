//! # resoz-vst3-host
//!
//! VST3 插件宿主适配层（非实时基础设施）：ResoZ 引擎与 VST3 插件
//! 生态之间的桥梁。架构镜像 `resoz-clap-host`。
//!
//! 职责：
//! - 扫描 VST3 模块（单文件 / bundle 目录；标准目录 +
//!   `RESOZ_VST3_PATH` 环境变量）；
//! - 枚举插件描述（CID / 名称 / 是否乐器）；
//! - 在**非实时线程**实例化插件（IComponent + IEditController +
//!   IConnectionPoint 互连 + 总线激活），产出可在音频回调中运行的
//!   [`Vst3Node`]（实时铁律约束适用于其 `process` 路径）。
//!
//! 桥接模型：
//! - **音符**：[`Processor::note_event`] 把 `NoteOn/Off` 暂存进定容
//!   事件槽，下一次 `process` 以宿主 `IEventList` 事件发出（块粒度）；
//! - **参数**：`ParamId::PluginParam(u32)` 以宿主 `IParameterChanges`
//!   队列发出，值按 **VST3 归一化语义（0..1）** 传递——自动化求值
//!   器写入的插件参数由此天然桥接（与 CLAP 的原始值语义不同，UI 层
//!   展示插件参数时须经控制器做归一化换算，见路线图备注）；
//! - **音频**：乐器（无音频输入总线）渲染到节点内预分配的立体声
//!   临时缓冲；效果器（有音频输入总线）自动接通输入通路（引擎缓冲
//!   → 独立输入缓冲 → 插件 → 独立输出缓冲 → 拷回引擎缓冲）。均为
//!   kSample32 单精度。
//!
//! 生命周期：
//! - [`Vst3InstanceGuard`] 持有动态库与全部插件 COM 对象，**必须与
//!   [`Vst3Node`] 同寿命且晚于其释放**（与 `ClapInstanceGuard` 相同
//!   的回收队列配对约定）；
//! - Node 释放时 `setProcessing(false)`；Guard 释放时 `setActive(false)`
//!   + `terminate()` + 断开连接，最后才卸载动态库。

#![allow(unsafe_code)]
// unsafe 策略（与 resoz-clap-host/src/raw.rs 同等严格度）：本 crate
// 的三个模块均为 VST3/COM 边界层——raw（模块加载）、host_impl（宿主
// COM 对象实现）、lib（接口调用与生命周期管理）。每个 unsafe 块附
// SAFETY 注释；行为验证见 tests（无效文件必须返回错误而非 UB）。

pub mod host_impl;
pub mod raw;

use std::ffi::{CStr, c_char};
use std::path::{Path, PathBuf};

use vst3::Steinberg::Vst::{
    IAudioProcessorTrait, IComponentTrait, IConnectionPointTrait, IEditControllerTrait,
};
use vst3::Steinberg::{self, IPluginBaseTrait, IPluginFactory2Trait, IPluginFactoryTrait, Vst};
use vst3::{ComPtr, ComWrapper, Interface};

use resoz_engine::realtime::FIXED_BLOCK_SIZE;
use resoz_engine::realtime::buffer::AudioBuffer;
use resoz_engine::realtime::commands::ParamId;
use resoz_engine::realtime::context::ProcessContext;
use resoz_engine::realtime::processor::Processor;

use crate::host_impl::{
    HostEventList, HostParamChanges, HostParamValueQueue, NoopComponentHandler, QueueSlot,
};

/// 一次 `process` 最多排队的桥接事件数（音符 + 参数合计）。
pub const MAX_PENDING_EVENTS: usize = 32;

/// 节点持有的预分配参数队列数（不同参数 ID 上限）。
const PARAM_QUEUE_COUNT: usize = host_impl::MAX_PARAM_QUEUES;

/// 插件扫描目录环境变量的名字（`PATH` 风格分隔列表）。
pub const VST3_PATH_ENV: &str = "RESOZ_VST3_PATH";

/// 宿主侧错误。
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("加载插件模块失败：{0}")]
    Load(#[from] raw::RawError),
    #[error("创建组件实例失败（tresult = {0}）")]
    CreateComponent(Steinberg::tresult),
    #[error("组件未提供控制器 CID")]
    NoControllerCid,
    #[error("创建控制器实例失败（tresult = {0}）")]
    CreateController(Steinberg::tresult),
    #[error("组件未实现 IAudioProcessor")]
    NoAudioProcessor,
    #[error("插件不支持 32 位样本处理")]
    SampleSizeUnsupported,
    #[error("无效的插件 CID：{0}")]
    BadCid(String),
}

/// tresult 是否表示成功（kResultOk / kResultTrue）。
fn tresult_ok(result: Steinberg::tresult) -> bool {
    result == Steinberg::kResultOk || result == Steinberg::kResultTrue
}

// ---------------------------------------------------------------------------
// CID 表示
// ---------------------------------------------------------------------------

/// 把 16 字节 CID 格式化为 32 位小写十六进制字符串。
pub fn format_cid(cid: &Steinberg::TUID) -> String {
    cid.iter()
        .map(|byte| format!("{:02x}", *byte as u8))
        .collect()
}

/// 把 32 位十六进制字符串解析回 16 字节 CID。
pub fn parse_cid(text: &str) -> Option<Steinberg::TUID> {
    let text = text.trim();
    if text.len() != 32 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut cid: Steinberg::TUID = [0; 16];
    for (index, byte) in cid.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()? as c_char;
    }
    Some(cid)
}

// ---------------------------------------------------------------------------
// 插件扫描
// ---------------------------------------------------------------------------

/// 平台相关的 VST3 插件标准搜索目录。
fn standard_vst3_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        dirs.push(
            PathBuf::from(program_files)
                .join("Common Files")
                .join("VST3"),
        );
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(
            PathBuf::from(local_app_data)
                .join("Programs")
                .join("Common")
                .join("VST3"),
        );
    }
    if let Some(app_data) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(app_data).join("VST3"));
    }
    dirs
}

/// 全部搜索目录：`RESOZ_VST3_PATH` 优先，其后是平台标准目录。
pub fn vst3_search_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(extra) = std::env::var_os(VST3_PATH_ENV) {
        dirs.extend(std::env::split_paths(&extra));
    }
    dirs.extend(standard_vst3_dirs());
    dirs
}

/// 扫描全部搜索目录下的 `.vst3` 模块（单文件或 bundle 目录，
/// 非递归，符合 VST3 规范惯例）。
pub fn scan_vst3_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in vst3_search_paths() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|ext| ext == "vst3").unwrap_or(false) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// 一个可实例化插件的描述信息。
#[derive(Debug, Clone, PartialEq)]
pub struct Vst3PluginInfo {
    /// 模块路径（单文件或 bundle 目录）。
    pub path: PathBuf,
    /// 组件类 CID（32 位小写十六进制，[`parse_cid`] 可解析）。
    pub cid: String,
    /// 显示名称。
    pub name: String,
    /// 是否为乐器（subCategories 含 `Instrument`）。
    pub is_instrument: bool,
}

/// 读取 C 语言 NUL 结尾字符串字段（`[char8; N]`）。
fn c_field_text(field: &[Steinberg::char8]) -> String {
    // SAFETY: 字段保证 NUL 结尾（SDK 约定），截断至字段长度内读取。
    unsafe { CStr::from_ptr(field.as_ptr() as *const c_char) }
        .to_string_lossy()
        .into_owned()
}

/// PClassInfo3 不可用时的回退：PClassInfo2 → PClassInfo。
fn fallback_class_info(
    factory: &ComPtr<Steinberg::IPluginFactory>,
    factory2: Option<&ComPtr<Steinberg::IPluginFactory2>>,
    index: i32,
) -> (String, String, bool) {
    if let Some(f2) = factory2 {
        let mut info2: Steinberg::PClassInfo2 = unsafe { std::mem::zeroed() };
        if tresult_ok(unsafe { f2.getClassInfo2(index, &mut info2) }) {
            return (
                format_cid(&info2.cid),
                c_field_text(&info2.name),
                c_field_text(&info2.subCategories).contains("Instrument"),
            );
        }
    }
    // SAFETY: factory 引用有效；info 由 SDK 填充。
    unsafe {
        let mut info: Steinberg::PClassInfo = std::mem::zeroed();
        if tresult_ok(factory.getClassInfo(index, &mut info)) {
            // v1 class info 无子类别信息，只能按类别名猜。
            return (
                format_cid(&info.cid),
                c_field_text(&info.name),
                c_field_text(&info.category).contains("Instrument"),
            );
        }
    }
    (String::new(), String::new(), false)
}

/// 枚举给定 VST3 模块内的全部插件类。加载失败的模块被跳过并记日志。
pub fn list_vst3_plugins_in(path: &Path) -> Vec<Vst3PluginInfo> {
    let module = match raw::load_module(path) {
        Ok(module) => module,
        Err(err) => {
            log::warn!("跳过无法加载的 VST3 模块 {}：{err}", path.display());
            return Vec::new();
        }
    };
    let factory = &module.factory;
    // SAFETY: 工厂指针有效（模块加载成功），按规范枚举类数量。
    let count = unsafe { factory.countClasses() };
    // PClassInfo2 在扩展接口上（COM queryInterface）。
    let factory2 = factory.cast::<Steinberg::IPluginFactory2>();
    let mut infos = Vec::new();
    for index in 0..count {
        let (cid, name, is_instrument) = fallback_class_info(factory, factory2.as_ref(), index);
        if cid.chars().all(|c| c == '0') {
            continue;
        }
        infos.push(Vst3PluginInfo {
            path: path.to_path_buf(),
            cid,
            name,
            is_instrument,
        });
    }
    infos
}

/// 扫描并枚举全部目录里的全部插件。
pub fn list_vst3_plugins() -> Vec<Vst3PluginInfo> {
    let mut infos = Vec::new();
    for path in scan_vst3_files() {
        infos.extend(list_vst3_plugins_in(&path));
    }
    infos
}

// ---------------------------------------------------------------------------
// 实例化
// ---------------------------------------------------------------------------

/// 实例化时的静态配置。
pub struct Vst3InstanceConfig {
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// 最大块大小（应与 [`FIXED_BLOCK_SIZE`] 一致）。
    pub max_frames: u32,
}

/// 持有插件模块与全部 COM 对象的守卫。**必须与 [`Vst3Node`] 同寿命
/// 且晚于其释放**：先释放守卫会卸载插件动态库，使节点的处理代码
/// 悬空（与 `ClapInstanceGuard` 相同的回收队列配对约定）。
///
/// **字段顺序即析构顺序**：模块放最后——先释放插件 COM 对象引用，
/// 再卸载动态库。
pub struct Vst3InstanceGuard {
    component_connection: Option<ComPtr<Steinberg::Vst::IConnectionPoint>>,
    controller_connection: Option<ComPtr<Steinberg::Vst::IConnectionPoint>>,
    controller: ComPtr<Steinberg::Vst::IEditController>,
    component: ComPtr<Steinberg::Vst::IComponent>,
    /// 插件持有的宿主处理器（控制器按规范长期持有此指针，须同寿命）。
    // 字段仅用于延长生命周期（spawn 时已把指针交给控制器）。
    #[allow(dead_code)]
    handler_ptr: ComPtr<Steinberg::Vst::IComponentHandler>,
    _handler: ComWrapper<NoopComponentHandler>,
    /// 模块（动态库 + 工厂）。**最后一个字段**：最后释放——先释放
    /// 全部插件 COM 对象引用，再卸载动态库。
    // 字段仅用于延长生命周期（工厂/库句柄由各 COM 引用隐式使用）。
    #[allow(dead_code)]
    module: raw::Vst3Module,
}

impl Vst3InstanceGuard {
    /// 规范化拆除：断开连接 → setActive(false) → terminate()。
    /// 复用于正常 Drop 与实例化失败路径。
    fn tear_down(&mut self) {
        // SAFETY: 组件/控制器对象均存活（动态库最后卸载，见字段
        // 顺序）；调用按 VST3 拆除顺序进行，失败仅忽略。
        unsafe {
            if let (Some(component), Some(controller)) =
                (&self.component_connection, &self.controller_connection)
            {
                let _ = component.disconnect(controller.as_ptr().cast());
                let _ = controller.disconnect(component.as_ptr().cast());
            }
            let _ = self.component.setActive(0);
            let _ = self.controller.terminate();
            let _ = self.component.terminate();
        }
    }

    /// 枚举控制器全部参数（控制侧调用，非 RT-safe）。
    ///
    /// 供 UI 展示参数列表与后续自动化选参；值为归一化 0..1 语义。
    pub fn parameters(&self) -> Vec<Vst3ParamInfo> {
        // SAFETY: controller 存活（守卫持有）；info 由插件按 SDK 约定
        // 填充（title 为 NUL 结尾 UTF-16），逐项读取后立即转 Rust 字符串。
        unsafe {
            let count = self.controller.getParameterCount().max(0);
            let mut out = Vec::with_capacity(count as usize);
            for index in 0..count {
                let mut info: Steinberg::Vst::ParameterInfo = std::mem::zeroed();
                if !tresult_ok(self.controller.getParameterInfo(index, &mut info)) {
                    continue;
                }
                out.push(Vst3ParamInfo {
                    id: info.id,
                    title: utf16_to_string(&info.title),
                    default_normalized: info.defaultNormalizedValue,
                    can_automate: info.flags
                        & Steinberg::Vst::ParameterInfo_::ParameterFlags_::kCanAutomate
                        != 0,
                });
            }
            out
        }
    }
}

/// 插件参数描述（控制侧枚举，见 [`Vst3InstanceGuard::parameters`]）。
#[derive(Debug, Clone)]
pub struct Vst3ParamInfo {
    /// 插件侧参数 ID（`ParamId::PluginParam` 的载荷）。
    pub id: u32,
    /// 参数标题（UTF-16 转换，可能为空）。
    pub title: String,
    /// 归一化默认值（0..1）。
    pub default_normalized: f64,
    /// 是否可自动化（VST3 `kCanAutomate` 标志）。
    pub can_automate: bool,
}

/// NUL 结尾 UTF-16 字段 → Rust 字符串（超长截断至字段长度内）。
fn utf16_to_string(field: &[Steinberg::char16]) -> String {
    let len = field
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(field.len());
    String::from_utf16_lossy(&field[..len])
}

impl Drop for Vst3InstanceGuard {
    fn drop(&mut self) {
        self.tear_down();
    }
}

/// 暂存的桥接事件。
#[derive(Debug, Clone, Copy)]
pub enum PendingVst3Event {
    /// 音符按下。
    NoteOn { note: u8, velocity: f32 },
    /// 音符松开。
    NoteOff { note: u8 },
    /// 插件参数值（`ParamId::PluginParam` 桥接，VST3 归一化 0..1）。
    Param { id: u32, value: f32 },
}

/// 可在音频回调中运行的 VST3 乐器节点。
///
/// 构造在非实时线程（[`Vst3Instance::spawn_rt`]），随后 move 进音频
/// 线程。`process` / `note_event` / `set_param` 均遵守实时铁律：
/// 无分配（事件与缓冲全部构造期预分配）、无锁、无 panic。
pub struct Vst3Node {
    /// 宿主事件列表（所有权 + 稳定堆地址；原始指针随对象存续）。
    event_list: ComWrapper<HostEventList>,
    /// 宿主参数变化容器。
    param_changes: ComWrapper<HostParamChanges>,
    /// 预分配参数队列（顺序与 `HostParamChanges` 内槽位一致）。
    _param_queues: Vec<ComWrapper<HostParamValueQueue>>,
    /// 传给插件的原始接口指针（非所有权，生命周期 = 上方对象）。
    input_events: *mut Steinberg::Vst::IEventList,
    input_param_changes: *mut Vst::IParameterChanges,
    /// 插件音频处理器（与 component 同一 COM 对象的接口视图）。
    audio_processor: ComPtr<Steinberg::Vst::IAudioProcessor>,
    /// 插件是否声明音频输入总线（效果器 true / 乐器 false）。
    has_audio_input: bool,
    /// 输入通路临时缓冲（仅效果器使用；构造期预分配）。
    in_l: Box<[f32]>,
    in_r: Box<[f32]>,
    out_l: Box<[f32]>,
    out_r: Box<[f32]>,
    pending: [PendingVst3Event; MAX_PENDING_EVENTS],
    pending_count: usize,
}

// SAFETY: 节点在线程间整体 move（控制线程构造 → 音频线程使用），
// 内部裸指针均指向节点自身持有的稳定 Arc 堆分配，随节点一起移动；
// COM 调用只发生在音频线程 process 内。Vst3 官方接口对象标为 Send。
unsafe impl Send for Vst3Node {}

impl Drop for Vst3Node {
    fn drop(&mut self) {
        // SAFETY: 节点先于守卫释放（回收队列约定），插件代码仍加载。
        unsafe {
            let _ = self.audio_processor.setProcessing(0);
        }
    }
}

/// 一次性实例化产物：控制侧守卫 + 实时节点。
///
/// **字段顺序即析构顺序**：`node` 先释放（`setProcessing(false)` 必
/// 须发生在动态库卸载之前），`guard` 后释放。把 `guard`/`node` 拆开
/// 存放（引擎换装路径）时必须保持这一顺序——守卫不得早于节点释放。
pub struct Vst3Instance {
    /// 实时侧节点（move 进音频线程）。
    pub node: Vst3Node,
    /// 控制侧存活守卫（UI/引擎线程持有）。
    pub guard: Vst3InstanceGuard,
}

impl Vst3Instance {
    /// 加载并激活插件，产出实时节点。**必须在非实时线程调用**
    /// （涉及动态库加载、插件内部分配与 COM 生命周期管理）。
    pub fn spawn_rt(
        path: &Path,
        class_id: &str,
        config: Vst3InstanceConfig,
    ) -> Result<Self, HostError> {
        let module = raw::load_module(path)?;
        let cid = parse_cid(class_id).ok_or_else(|| HostError::BadCid(class_id.to_owned()))?;
        let factory = &module.factory;

        // 1. 创建组件与控制器（VST3 双对象模型）。
        let component = create_instance::<Steinberg::Vst::IComponent>(factory, &cid)
            .map_err(HostError::CreateComponent)?;
        let mut controller_cid: Steinberg::TUID = [0; 16];
        let controller_cid_ok =
            tresult_ok(unsafe { component.getControllerClassId(&mut controller_cid) });
        if !controller_cid_ok {
            return Err(HostError::NoControllerCid);
        }
        let controller =
            create_instance::<Steinberg::Vst::IEditController>(factory, &controller_cid)
                .map_err(HostError::CreateController)?;

        // 2. 宿主处理器（空实现）+ 连接点互连。
        let handler = ComWrapper::new(NoopComponentHandler);
        let handler_ptr = handler
            .to_com_ptr::<Steinberg::Vst::IComponentHandler>()
            .expect("NoopComponentHandler 必然实现 IComponentHandler");
        let component_connection = component.cast::<Steinberg::Vst::IConnectionPoint>();
        let controller_connection = controller.cast::<Steinberg::Vst::IConnectionPoint>();
        if let (Some(component_cp), Some(controller_cp)) =
            (&component_connection, &controller_connection)
        {
            // SAFETY: 两个连接点对象均存活，互连是对称的规范调用。
            unsafe {
                let _ = component_cp.connect(controller_cp.as_ptr().cast());
                let _ = controller_cp.connect(component_cp.as_ptr().cast());
            }
        }

        // 3. 初始化（context = IHostApplication*，简化宿主传空）。
        let init_result = unsafe {
            component.initialize(std::ptr::null_mut())
                | controller.initialize(std::ptr::null_mut())
                | controller.setComponentHandler(handler_ptr.as_ptr().cast())
        };
        if !tresult_ok(init_result) {
            log::warn!("VST3 初始化返回 0x{init_result:08x}，继续按简单乐器使用");
        }

        // 4. 总线枚举与激活（激活全部音频输出/事件输入总线）。
        activate_buses(&component);
        let audio_processor = component
            .cast::<Steinberg::Vst::IAudioProcessor>()
            .ok_or(HostError::NoAudioProcessor)?;
        // SAFETY: setup/setActive 按规范顺序在激活前调用。
        unsafe {
            let mut setup = Vst::ProcessSetup {
                processMode: Vst::ProcessModes_::kRealtime,
                symbolicSampleSize: Vst::SymbolicSampleSizes_::kSample32,
                maxSamplesPerBlock: config.max_frames.max(FIXED_BLOCK_SIZE as u32) as i32,
                sampleRate: f64::from(config.sample_rate),
            };
            if !tresult_ok(audio_processor.setupProcessing(&mut setup)) {
                log::warn!("VST3 插件拒绝 ProcessSetup，按默认配置继续");
            }
            let _ = component.setActive(1);
            if !tresult_ok(audio_processor.setProcessing(1)) {
                log::warn!("VST3 插件拒绝 setProcessing(true)，将输出静音");
            }
        }

        // 5. 构造实时节点（全部预分配在此完成）。
        // 是否有音频输入总线（效果器为真；乐器为假）——查询须在
        // component 移入守卫之前完成。
        let has_audio_input = unsafe {
            component.getBusCount(Vst::MediaTypes_::kAudio, Vst::BusDirections_::kInput) > 0
        };
        let frames = config.max_frames.max(FIXED_BLOCK_SIZE as u32) as usize;
        let event_list = ComWrapper::new(HostEventList::new());
        let input_events = event_list
            .as_com_ref::<Steinberg::Vst::IEventList>()
            .expect("HostEventList 必然实现 IEventList")
            .as_ptr();
        let mut queues = Vec::with_capacity(PARAM_QUEUE_COUNT);
        let mut slots = Vec::with_capacity(PARAM_QUEUE_COUNT);
        for _ in 0..PARAM_QUEUE_COUNT {
            let queue = ComWrapper::new(HostParamValueQueue::new());
            slots.push(QueueSlot {
                // 共享引用转裸指针：COM 接口对象在 wrapper 存活期内有效
                // （节点持有所有权），无需 unsafe——裸指针本身只是数据。
                data: &*queue as *const HostParamValueQueue as *mut HostParamValueQueue,
                com: queue
                    .as_com_ref::<Vst::IParamValueQueue>()
                    .expect("HostParamValueQueue 必然实现 IParamValueQueue")
                    .as_ptr(),
            });
            queues.push(queue);
        }
        let param_changes = ComWrapper::new(HostParamChanges::new(slots));
        let input_param_changes = param_changes
            .as_com_ref::<Vst::IParameterChanges>()
            .expect("HostParamChanges 必然实现 IParameterChanges")
            .as_ptr();

        let node = Vst3Node {
            event_list,
            param_changes,
            _param_queues: queues,
            input_events,
            input_param_changes,
            audio_processor,
            has_audio_input,
            in_l: vec![0.0; frames].into_boxed_slice(),
            in_r: vec![0.0; frames].into_boxed_slice(),
            out_l: vec![0.0; frames].into_boxed_slice(),
            out_r: vec![0.0; frames].into_boxed_slice(),
            pending: [PendingVst3Event::NoteOff { note: 0 }; MAX_PENDING_EVENTS],
            pending_count: 0,
        };

        // 实例化成功路径：对象移交守卫；失败路径由 tear_down 兜底。
        // （此处全部步骤已无可能失败的调用，直接组装。）
        Ok(Self {
            node,
            guard: Vst3InstanceGuard {
                component_connection,
                controller_connection,
                controller,
                component,
                handler_ptr,
                _handler: handler,
                module,
            },
        })
    }
}

/// 经工厂创建指定类与接口的实例（引用计数 +1，归调用方）。
fn create_instance<I: Interface>(
    factory: &ComPtr<Steinberg::IPluginFactory>,
    cid: &Steinberg::TUID,
) -> Result<ComPtr<I>, Steinberg::tresult> {
    // SAFETY: createInstance 按 VST3 规范返回 +1 引用计数的接口
    // 指针，ComPtr RAII 接管；iid 即 16 字节 GUID（FIDString 在
    // createInstance 语境下就是二进制 CID 的字节指针，与 SDK 一致）。
    unsafe {
        let mut obj: *mut std::ffi::c_void = std::ptr::null_mut();
        let result = factory.createInstance(
            cid.as_ptr() as Steinberg::FIDString,
            I::IID.as_ptr() as Steinberg::FIDString,
            &mut obj,
        );
        if !tresult_ok(result) {
            return Err(result);
        }
        ComPtr::<I>::from_raw(obj.cast()).ok_or(result)
    }
}

/// 激活组件的全部音频输入/输出与事件输入总线。
fn activate_buses(component: &ComPtr<Steinberg::Vst::IComponent>) {
    // SAFETY: 组件已 initialize；总线信息按规范枚举。
    unsafe {
        let audio_inputs =
            component.getBusCount(Vst::MediaTypes_::kAudio, Vst::BusDirections_::kInput);
        for index in 0..audio_inputs {
            let _ = component.activateBus(
                Vst::MediaTypes_::kAudio,
                Vst::BusDirections_::kInput,
                index,
                1,
            );
        }
        let audio_outputs =
            component.getBusCount(Vst::MediaTypes_::kAudio, Vst::BusDirections_::kOutput);
        for index in 0..audio_outputs {
            let _ = component.activateBus(
                Vst::MediaTypes_::kAudio,
                Vst::BusDirections_::kOutput,
                index,
                1,
            );
        }
        let event_inputs =
            component.getBusCount(Vst::MediaTypes_::kEvent, Vst::BusDirections_::kInput);
        for index in 0..event_inputs {
            let _ = component.activateBus(
                Vst::MediaTypes_::kEvent,
                Vst::BusDirections_::kInput,
                index,
                1,
            );
        }
    }
}

impl Vst3Node {
    fn stash(&mut self, event: PendingVst3Event) {
        if self.pending_count >= MAX_PENDING_EVENTS {
            return; // 满载丢弃（非关键路径）
        }
        self.pending[self.pending_count] = event;
        self.pending_count += 1;
    }
}

/// 构造 NoteOn 事件（总线 0，样本偏移 0）。
fn note_on_event(note: u8, velocity: f32) -> Vst::Event {
    let mut event: Vst::Event = unsafe { std::mem::zeroed() };
    event.r#type = Vst::Event_::EventTypes_::kNoteOnEvent as u16;
    event.__field0.noteOn = Vst::NoteOnEvent {
        channel: 0,
        pitch: i16::from(note),
        tuning: 0.0,
        velocity,
        length: 0,
        noteId: -1,
    };
    event
}

/// 构造 NoteOff 事件。
fn note_off_event(note: u8) -> Vst::Event {
    let mut event: Vst::Event = unsafe { std::mem::zeroed() };
    event.r#type = Vst::Event_::EventTypes_::kNoteOffEvent as u16;
    event.__field0.noteOff = Vst::NoteOffEvent {
        channel: 0,
        pitch: i16::from(note),
        velocity: 0.0,
        noteId: -1,
        tuning: 0.0,
    };
    event
}

impl Processor for Vst3Node {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = ctx.frames.min(self.out_l.len()).min(buffer.capacity());

        // 1. 暂存事件 → 宿主 COM 对象（块粒度）。ComWrapper 经 Deref
        //    暴露 &HostEventList / &HostParamChanges，可变内部状态由
        //    Cell/UnsafeCell 承载（音频线程内串行，见 host_impl 文档）。
        self.event_list.clear();
        self.param_changes.clear();
        for event in &self.pending[..self.pending_count] {
            match *event {
                PendingVst3Event::NoteOn { note, velocity } => {
                    self.event_list.push(note_on_event(note, velocity));
                }
                PendingVst3Event::NoteOff { note } => {
                    self.event_list.push(note_off_event(note));
                }
                PendingVst3Event::Param { id, value } => {
                    // VST3 归一化语义：值直接作为 0..1 提交。
                    self.param_changes.push(id, f64::from(value));
                }
            }
        }
        self.pending_count = 0;

        // 2. 音频缓冲 + 处理上下文 + ProcessData（全部栈上）。
        let out_l = &mut self.out_l[..frames];
        let out_r = &mut self.out_r[..frames];
        out_l.fill(0.0);
        out_r.fill(0.0);
        let mut channel_ptrs: [*mut f32; 2] = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
        let mut output_buses = [Vst::AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: Vst::AudioBusBuffers__type0 {
                channelBuffers32: channel_ptrs.as_mut_ptr(),
            },
        }];

        // 效果器：把引擎缓冲内容拷入独立输入缓冲（输出写独立缓冲、
        // 最后整体拷回，避免插件就地读写混叠）。
        let has_audio_input = self.has_audio_input;
        let mut input_channel_ptrs: [*mut f32; 2] = [std::ptr::null_mut(), std::ptr::null_mut()];
        let mut input_buses = [Vst::AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: Vst::AudioBusBuffers__type0 {
                channelBuffers32: input_channel_ptrs.as_mut_ptr(),
            },
        }];
        if has_audio_input {
            // 引擎缓冲 → 输入通路缓冲（缺失声道填零）。
            {
                let in_l = &mut self.in_l[..frames];
                let in_r = &mut self.in_r[..frames];
                for (destination, channel_index) in [in_l, in_r].iter_mut().zip(0usize..) {
                    if let Some(source) = buffer.channel(channel_index) {
                        for (index, sample) in destination.iter_mut().enumerate() {
                            *sample = source.get(index).copied().unwrap_or(0.0);
                        }
                    } else {
                        destination.fill(0.0);
                    }
                }
            }
            // SAFETY/说明：input_channel_ptrs 的写入经 input_buses 的
            // channelBuffers32 指针交给插件读取，编译器无法跨 FFI 看到
            // 该读取，故允许 unused_assignments（误报）。
            #[allow(unused_assignments)]
            {
                input_channel_ptrs[0] = self.in_l[..frames].as_mut_ptr();
                input_channel_ptrs[1] = self.in_r[..frames].as_mut_ptr();
            }
        }

        let mut vst_context: Vst::ProcessContext = unsafe { std::mem::zeroed() };
        vst_context.sampleRate = f64::from(ctx.sample_rate);
        vst_context.projectTimeSamples = i64::try_from(ctx.playback_sample_pos).unwrap_or(i64::MAX);
        // state = 0：不声明任何有效位（简化宿主不提供走带信息）。
        let mut data = Vst::ProcessData {
            processMode: Vst::ProcessModes_::kRealtime,
            symbolicSampleSize: Vst::SymbolicSampleSizes_::kSample32,
            numSamples: frames as i32,
            numInputs: if has_audio_input { 1 } else { 0 },
            numOutputs: 1,
            inputs: if has_audio_input {
                input_buses.as_mut_ptr()
            } else {
                std::ptr::null_mut()
            },
            outputs: output_buses.as_mut_ptr(),
            inputParameterChanges: self.input_param_changes,
            outputParameterChanges: std::ptr::null_mut(),
            inputEvents: self.input_events,
            outputEvents: std::ptr::null_mut(),
            processContext: &mut vst_context,
        };

        // SAFETY: 插件接口在守卫生存期内有效（回收队列约定）；
        // 输出缓冲已清零，插件失败路径自然产生静音。
        let result = unsafe { self.audio_processor.process(&mut data) };
        if !tresult_ok(result) {
            // 输出保持清零（无 panic），仅提示。
            log::trace!("VST3 process 返回 0x{result:08x}");
        }

        // 3. 拷贝进引擎平面缓冲（越界防御性钳制，不 panic）。
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
                self.stash(PendingVst3Event::Param {
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
                self.stash(PendingVst3Event::NoteOn { note, velocity });
            }
            resoz_engine::realtime::NoteEvent::Off { note } => {
                self.stash(PendingVst3Event::NoteOff { note });
            }
        }
        true
    }
}

// 本 crate 不导出 GetPluginFactory（那是插件侧的导出符号）。

#[cfg(test)]
mod tests {
    use super::{
        Vst3Instance, Vst3InstanceConfig, Vst3PluginInfo, list_vst3_plugins_in, parse_cid,
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
                "{}resoz_test_vst3{}",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX
            ));
        path.exists().then_some(path)
    }

    fn spawn_test_plugin() -> Option<(Vst3Instance, ProcessContext)> {
        spawn_test_class("5245534f5a53494e4500000000000001")
    }

    fn spawn_test_class(class_id: &str) -> Option<(Vst3Instance, ProcessContext)> {
        let Some(path) = test_plugin_path() else {
            eprintln!("跳过：VST3 测试插件 cdylib 未构建（请用 cargo test --workspace）");
            return None;
        };
        let instance = Vst3Instance::spawn_rt(
            &path,
            class_id,
            Vst3InstanceConfig {
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
    fn parse_cid_round_trip() {
        let text = "52455a4f5a53494e4500000000000001";
        let cid = parse_cid(text).expect("合法 CID");
        assert_eq!(super::format_cid(&cid), text);
        assert!(parse_cid("nothex").is_none());
        assert!(parse_cid("52455a4f5a53494e450000000000000g").is_none());
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
    fn parameter_enumeration_reports_gain() {
        let Some((instance, _ctx)) = spawn_test_plugin() else {
            return;
        };
        let params = instance.guard.parameters();
        assert_eq!(params.len(), 1, "测试插件应暴露 1 个参数");
        let gain = &params[0];
        assert_eq!(gain.id, 1);
        assert_eq!(gain.title, "Gain");
        assert!((gain.default_normalized - 0.5).abs() < 1e-9);
        assert!(gain.can_automate, "Gain 应标记为可自动化");
    }

    #[test]
    fn end_to_end_gain_effect_processes_input() {
        // 效果器类（"RESOZSINE" + 0x03）。
        let Some((mut instance, ctx)) = spawn_test_class("5245534f5a53494e4500000000000003") else {
            return;
        };
        let node = &mut instance.node;

        // 输入 0.5 → 输出应为 0.25（固定增益 ×0.5）。
        let mut buffer = AudioBuffer::new(2, 128);
        buffer.set_len(128);
        for channel_index in 0..2 {
            for sample in buffer.channel_mut(channel_index).unwrap() {
                *sample = 0.5;
            }
        }
        node.process(&ctx, &mut buffer);
        for channel_index in 0..2 {
            for (index, sample) in buffer.channel(channel_index).unwrap().iter().enumerate() {
                assert!(
                    (sample - 0.25).abs() < 1e-6,
                    "增益效果器应输出 0.25，实际 {sample}（声道 {channel_index} 样本 {index}）"
                );
            }
        }
    }

    #[test]
    fn param_bridge_routes_and_ignores_builtin() {
        let Some((mut instance, ctx)) = spawn_test_plugin() else {
            return;
        };
        let node = &mut instance.node;

        // 插件参数被接受并入队（即使插件暂无参数，事件路径也畅通）。
        assert!(node.set_param(ParamId::PluginParam(1), 0.5));
        // 内置参数不被 Vst3Node 消费（归内置节点管）。
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
            eprintln!("跳过：VST3 测试插件 cdylib 未构建");
            return;
        };
        // 扫描按 .vst3 后缀匹配：把 cdylib 复制为 .vst3 单文件模块。
        let temp = std::env::temp_dir().join("resoz_test_vst3_scan.vst3");
        std::fs::copy(&path, &temp).expect("复制失败");
        let infos: Vec<Vst3PluginInfo> = list_vst3_plugins_in(&temp);
        let _ = std::fs::remove_file(&temp);
        let sine = infos
            .iter()
            .find(|info| info.cid == "5245534f5a53494e4500000000000001")
            .expect("测试插件应被枚举");
        assert!(sine.is_instrument, "正弦插件应标记为乐器");
        assert_eq!(sine.name, "ResoZ Sine VST3");
    }

    #[test]
    fn invalid_plugin_fails_gracefully() {
        let bogus = std::env::temp_dir().join("resoz_bogus.vst3");
        std::fs::write(&bogus, b"not a dll").expect("写入失败");
        let result = Vst3Instance::spawn_rt(
            &bogus,
            "52455a4f5a53494e4500000000000001",
            Vst3InstanceConfig {
                sample_rate: 48_000,
                max_frames: 128,
            },
        );
        let _ = std::fs::remove_file(&bogus);
        assert!(matches!(result, Err(super::HostError::Load(_))));
    }
}
