//! # resoz-test-vst3
//!
//! 测试用 VST3 正弦乐器插件（cdylib fixture，仅供 `resoz-vst3-host`
//! 的端到端测试加载）。
//!
//! 结构（VST3 双对象模型的最小实现）：
//! - `SineFactory`：IPluginFactory(2) —— 枚举一个乐器类；
//! - `SineComponent`：IPluginBase + IComponent + IAudioProcessor +
//!   IConnectionPoint —— A4 正弦乐器，NoteOn 触发、NoteOff 停止；
//! - `SineController`：IPluginBase + IEditController + IConnectionPoint
//!   —— 零参数的哑控制器。
//!
//! 导出符号：`GetPluginFactory` / `InitDll` / `ExitDll`（Windows 单
//! 文件模块约定）。
//!
//! CID 用字面量字节数组定义（ASCII "RESOZSINE" + 序号），宿主测试
//! 按同样的十六进制字符串引用，避免端序歧义：
//! - 组件：`5245534f5a53494e4500000000000001`
//! - 控制器：`5245534f5a53494e4500000000000002`

#![allow(non_snake_case, non_upper_case_globals)]
// COM 边界：vtable 签名由 SDK 绑定固定为 unsafe，插件实现同理。
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vst3::Steinberg::Vst::{
    self, IAudioProcessorTrait, IComponentTrait, IConnectionPointTrait, IEditControllerTrait,
    IEventListTrait,
};
use vst3::Steinberg::{self, IPluginBaseTrait, TUID};
use vst3::{Class, ComRef, ComWrapper};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 组件 CID（"RESOZSINE" + 0x01）。
const COMPONENT_CID: TUID = [
    0x52, 0x45, 0x53, 0x4F, 0x5A, 0x53, 0x49, 0x4E, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];
/// 控制器 CID（"RESOZSINE" + 0x02）。
const CONTROLLER_CID: TUID = [
    0x52, 0x45, 0x53, 0x4F, 0x5A, 0x53, 0x49, 0x4E, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02,
];

const PLUGIN_NAME: &str = "ResoZ Sine VST3";
const VENDOR: &str = "ResoZ";

/// A4 频率（与内置引擎测试约定一致）。
const SINE_FREQ_HZ: f32 = 440.0;

/// 复用 sample_rate（宿主 setupProcessing 传入；简化实现忽略，
/// 测试固定 48 kHz）。
const SAMPLE_RATE: f32 = 48_000.0;

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 把字符串填进 NUL 结尾的定长 char8 字段。
fn c_field<const N: usize>(text: &str) -> [Steinberg::char8; N] {
    let mut out = [0 as Steinberg::char8; N];
    for (index, byte) in text.bytes().take(N - 1).enumerate() {
        out[index] = byte as Steinberg::char8;
    }
    out
}

/// 把字符串填进 NUL 结尾的定长 TChar（UTF-16）字段。
fn utf16_field<const N: usize>(text: &str) -> [Steinberg::char16; N] {
    let mut out = [0u16; N];
    for (index, unit) in text.encode_utf16().take(N - 1).enumerate() {
        out[index] = unit;
    }
    out
}

/// 原子存取 f32（velocity/phase 的简化载体）。
fn f32_bits_cell(value: &AtomicU32) -> f32 {
    f32::from_bits(value.load(Ordering::Relaxed))
}

fn set_f32_bits_cell(cell: &AtomicU32, value: f32) {
    cell.store(value.to_bits(), Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// 工厂
// ---------------------------------------------------------------------------

/// 单插件工厂。
struct SineFactory;

impl Class for SineFactory {
    type Interfaces = (
        Steinberg::IPluginFactory,
        Steinberg::IPluginFactory2,
        Steinberg::IPluginFactory3,
    );
}

impl Steinberg::IPluginFactoryTrait for SineFactory {
    unsafe fn getFactoryInfo(&self, info: *mut Steinberg::PFactoryInfo) -> Steinberg::tresult {
        // SAFETY: 宿主按规范提供可写 PFactoryInfo。
        unsafe {
            *info = Steinberg::PFactoryInfo {
                vendor: c_field::<64>(VENDOR),
                url: c_field::<256>("https://github.com/resoz"),
                email: c_field::<128>(""),
                flags: 0,
            };
        }
        Steinberg::kResultOk
    }

    unsafe fn countClasses(&self) -> Steinberg::int32 {
        1
    }

    unsafe fn getClassInfo(
        &self,
        index: Steinberg::int32,
        info: *mut Steinberg::PClassInfo,
    ) -> Steinberg::tresult {
        if index != 0 {
            return Steinberg::kInvalidArgument;
        }
        // SAFETY: 宿主按规范提供可写 PClassInfo。
        unsafe {
            *info = Steinberg::PClassInfo {
                cid: COMPONENT_CID,
                cardinality: Steinberg::PClassInfo_::ClassCardinality_::kManyInstances,
                category: c_field::<32>("Audio Module Class"),
                name: c_field::<64>(PLUGIN_NAME),
            };
        }
        Steinberg::kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: Steinberg::FIDString,
        _iid: Steinberg::FIDString,
        obj: *mut *mut c_void,
    ) -> Steinberg::tresult {
        // SAFETY: cid 指向宿主提供的 16 字节 CID；返回 +1 引用计数的
        // 接口指针（to_com_ptr），归宿主所有。
        unsafe {
            let requested: [u8; 16] = std::slice::from_raw_parts(cid as *const u8, 16)
                .try_into()
                .unwrap_or([0; 16]);
            let component = requested
                .iter()
                .zip(COMPONENT_CID.iter())
                .all(|(a, b)| (*a as i8) == *b);
            let controller = requested
                .iter()
                .zip(CONTROLLER_CID.iter())
                .all(|(a, b)| (*a as i8) == *b);
            if component {
                static COMPONENT: OnceLock<ComWrapper<SineComponent>> = OnceLock::new();
                let wrapper = COMPONENT.get_or_init(|| ComWrapper::new(SineComponent::new()));
                if let Some(ptr) = wrapper.to_com_ptr::<Vst::IComponent>() {
                    *obj = ptr.into_raw() as *mut c_void;
                    return Steinberg::kResultOk;
                }
                return Steinberg::kResultFalse;
            }
            if controller {
                static CONTROLLER: OnceLock<ComWrapper<SineController>> = OnceLock::new();
                let wrapper = CONTROLLER.get_or_init(|| ComWrapper::new(SineController));
                if let Some(ptr) = wrapper.to_com_ptr::<Vst::IEditController>() {
                    *obj = ptr.into_raw() as *mut c_void;
                    return Steinberg::kResultOk;
                }
                return Steinberg::kResultFalse;
            }
            Steinberg::kInvalidArgument
        }
    }
}

impl Steinberg::IPluginFactory2Trait for SineFactory {
    unsafe fn getClassInfo2(
        &self,
        index: Steinberg::int32,
        info: *mut Steinberg::PClassInfo2,
    ) -> Steinberg::tresult {
        if index != 0 {
            return Steinberg::kInvalidArgument;
        }
        // SAFETY: 宿主按规范提供可写 PClassInfo2。
        unsafe {
            *info = Steinberg::PClassInfo2 {
                cid: COMPONENT_CID,
                cardinality: Steinberg::PClassInfo_::ClassCardinality_::kManyInstances,
                category: c_field::<32>("Audio Module Class"),
                name: c_field::<64>(PLUGIN_NAME),
                classFlags: 0,
                subCategories: c_field::<128>("Instrument|Synth|Stereo"),
                vendor: c_field::<64>(VENDOR),
                version: c_field::<64>("1.0.0"),
                sdkVersion: c_field::<64>("VST 3.8.0"),
            };
        }
        Steinberg::kResultOk
    }
}

impl Steinberg::IPluginFactory3Trait for SineFactory {
    unsafe fn getClassInfoUnicode(
        &self,
        _index: Steinberg::int32,
        _info: *mut Steinberg::PClassInfoW,
    ) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn setHostContext(&self, _context: *mut Steinberg::FUnknown) -> Steinberg::tresult {
        Steinberg::kResultOk
    }
}

// ---------------------------------------------------------------------------
// 组件（IComponent + IAudioProcessor）
// ---------------------------------------------------------------------------

/// A4 正弦乐器组件：NoteOn 触发、NoteOff 静音，单声部、立体声输出。
struct SineComponent {
    note_on: AtomicBool,
    velocity_bits: AtomicU32,
    phase_bits: AtomicU32,
}

impl SineComponent {
    fn new() -> Self {
        Self {
            note_on: AtomicBool::new(false),
            velocity_bits: AtomicU32::new(0.0_f32.to_bits()),
            phase_bits: AtomicU32::new(0.0_f32.to_bits()),
        }
    }
}

impl Class for SineComponent {
    type Interfaces = (
        Steinberg::IPluginBase,
        Vst::IComponent,
        Vst::IAudioProcessor,
        Vst::IConnectionPoint,
    );
}

impl IPluginBaseTrait for SineComponent {
    unsafe fn initialize(&self, _context: *mut Steinberg::FUnknown) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn terminate(&self) -> Steinberg::tresult {
        Steinberg::kResultOk
    }
}

impl IComponentTrait for SineComponent {
    unsafe fn getControllerClassId(&self, classId: *mut TUID) -> Steinberg::tresult {
        // SAFETY: 宿主提供可写 TUID。
        unsafe {
            *classId = CONTROLLER_CID;
        }
        Steinberg::kResultOk
    }

    unsafe fn setIoMode(&self, _mode: Vst::IoMode) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn getBusCount(
        &self,
        r#type: Vst::MediaType,
        dir: Vst::BusDirection,
    ) -> Steinberg::int32 {
        match (r#type, dir) {
            (Vst::MediaTypes_::kAudio, Vst::BusDirections_::kOutput) => 1,
            (Vst::MediaTypes_::kEvent, Vst::BusDirections_::kInput) => 1,
            _ => 0,
        }
    }

    unsafe fn getBusInfo(
        &self,
        r#type: Vst::MediaType,
        dir: Vst::BusDirection,
        index: Steinberg::int32,
        bus: *mut Vst::BusInfo,
    ) -> Steinberg::tresult {
        if index != 0 {
            return Steinberg::kInvalidArgument;
        }
        // SAFETY: 宿主提供可写 BusInfo。
        unsafe {
            *bus = Vst::BusInfo {
                mediaType: r#type,
                direction: dir,
                channelCount: match r#type {
                    Vst::MediaTypes_::kAudio => 2,
                    _ => 1,
                },
                name: match r#type {
                    Vst::MediaTypes_::kAudio => utf16_field::<128>("Stereo Out"),
                    _ => utf16_field::<128>("Note In"),
                },
                busType: Vst::BusTypes_::kMain,
                flags: Vst::BusInfo_::BusFlags_::kDefaultActive as u32,
            };
        }
        Steinberg::kResultOk
    }

    unsafe fn getRoutingInfo(
        &self,
        _inInfo: *mut Vst::RoutingInfo,
        _outInfo: *mut Vst::RoutingInfo,
    ) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn activateBus(
        &self,
        _type: Vst::MediaType,
        _dir: Vst::BusDirection,
        _index: Steinberg::int32,
        _state: Steinberg::TBool,
    ) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn setActive(&self, _state: Steinberg::TBool) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn setState(&self, _state: *mut Steinberg::IBStream) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn getState(&self, _state: *mut Steinberg::IBStream) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }
}

impl IAudioProcessorTrait for SineComponent {
    unsafe fn setBusArrangements(
        &self,
        _inputs: *mut Vst::SpeakerArrangement,
        numIns: Steinberg::int32,
        outputs: *mut Vst::SpeakerArrangement,
        numOuts: Steinberg::int32,
    ) -> Steinberg::tresult {
        // 只接受 0 入 / 1 出（立体声）。
        const K_STEREO: Vst::SpeakerArrangement = 3; // L|R
        if numIns == 0
            && numOuts == 1
            // SAFETY: 宿主按规范提供 numOuts 个可读元素。
            && unsafe { *outputs } == K_STEREO
        {
            return Steinberg::kResultTrue;
        }
        Steinberg::kResultFalse
    }

    unsafe fn getBusArrangement(
        &self,
        _dir: Vst::BusDirection,
        _index: Steinberg::int32,
        _arr: *mut Vst::SpeakerArrangement,
    ) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn canProcessSampleSize(
        &self,
        symbolicSampleSize: Steinberg::int32,
    ) -> Steinberg::tresult {
        if symbolicSampleSize == Vst::SymbolicSampleSizes_::kSample32 {
            Steinberg::kResultTrue
        } else {
            Steinberg::kNotImplemented
        }
    }

    unsafe fn getLatencySamples(&self) -> Steinberg::uint32 {
        0
    }

    unsafe fn setupProcessing(&self, _setup: *mut Vst::ProcessSetup) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn setProcessing(&self, _state: Steinberg::TBool) -> Steinberg::tresult {
        // 进入/退出处理态时复位相位，保证测试确定性。
        self.phase_bits.store(0.0_f32.to_bits(), Ordering::Relaxed);
        Steinberg::kResultOk
    }

    unsafe fn process(&self, data: *mut Vst::ProcessData) -> Steinberg::tresult {
        // SAFETY: 宿主按规范填充 ProcessData；输出指针在 numSamples
        // 范围内可写。
        unsafe {
            let data = &mut *data;
            if data.numOutputs < 1
                || data.outputs.is_null()
                || data.symbolicSampleSize != Vst::SymbolicSampleSizes_::kSample32
            {
                return Steinberg::kNotImplemented;
            }
            let frames = data.numSamples.max(0) as usize;
            let bus = &mut *data.outputs;
            let channels = (bus.numChannels.max(0) as usize).min(2);
            bus.silenceFlags = 0;

            // 消费宿主事件（NoteOn/NoteOff）。
            if !data.inputEvents.is_null() {
                if let Some(events) = ComRef::<Vst::IEventList>::from_raw(data.inputEvents) {
                    let count = events.getEventCount();
                    for index in 0..count {
                        let mut event: Vst::Event = std::mem::zeroed();
                        if events.getEvent(index, &mut event) == Steinberg::kResultOk {
                            match event.r#type as i32 {
                                Vst::Event_::EventTypes_::kNoteOnEvent => {
                                    let note_on = &event.__field0.noteOn;
                                    self.note_on.store(true, Ordering::Relaxed);
                                    set_f32_bits_cell(&self.velocity_bits, note_on.velocity);
                                    // 复位相位保证测试确定性。
                                    self.phase_bits.store(0.0_f32.to_bits(), Ordering::Relaxed);
                                }
                                Vst::Event_::EventTypes_::kNoteOffEvent => {
                                    self.note_on.store(false, Ordering::Relaxed);
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }

            if !self.note_on.load(Ordering::Relaxed) || channels == 0 {
                for channel in 0..channels {
                    let base = *bus.__field0.channelBuffers32.add(channel);
                    for sample in 0..frames {
                        *base.add(sample) = 0.0;
                    }
                }
                return Steinberg::kResultOk;
            }

            let velocity = f32_bits_cell(&self.velocity_bits);
            let step = std::f32::consts::TAU * SINE_FREQ_HZ / SAMPLE_RATE;
            let mut phase = f32_bits_cell(&self.phase_bits);
            for sample in 0..frames {
                let value = phase.sin() * velocity;
                phase += step;
                if phase >= std::f32::consts::TAU {
                    phase -= std::f32::consts::TAU;
                }
                for channel in 0..channels {
                    let base = *bus.__field0.channelBuffers32.add(channel);
                    *base.add(sample) = value;
                }
            }
            self.phase_bits.store(phase.to_bits(), Ordering::Relaxed);
        }
        Steinberg::kResultOk
    }

    unsafe fn getTailSamples(&self) -> Steinberg::uint32 {
        0
    }
}

impl IConnectionPointTrait for SineComponent {
    unsafe fn connect(&self, _other: *mut Vst::IConnectionPoint) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn disconnect(&self, _other: *mut Vst::IConnectionPoint) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn notify(&self, _message: *mut Steinberg::Vst::IMessage) -> Steinberg::tresult {
        Steinberg::kResultOk
    }
}

// ---------------------------------------------------------------------------
// 控制器（IEditController，哑实现）
// ---------------------------------------------------------------------------

/// 零参数哑控制器。
struct SineController;

impl Class for SineController {
    type Interfaces = (
        Steinberg::IPluginBase,
        Vst::IEditController,
        Vst::IConnectionPoint,
    );
}

impl IPluginBaseTrait for SineController {
    unsafe fn initialize(&self, _context: *mut Steinberg::FUnknown) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn terminate(&self) -> Steinberg::tresult {
        Steinberg::kResultOk
    }
}

impl IEditControllerTrait for SineController {
    unsafe fn setComponentState(&self, _state: *mut Steinberg::IBStream) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn setState(&self, _state: *mut Steinberg::IBStream) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn getState(&self, _state: *mut Steinberg::IBStream) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn getParameterCount(&self) -> Steinberg::int32 {
        1
    }

    unsafe fn getParameterInfo(
        &self,
        paramIndex: Steinberg::int32,
        info: *mut Vst::ParameterInfo,
    ) -> Steinberg::tresult {
        if paramIndex != 0 {
            return Steinberg::kInvalidArgument;
        }
        // SAFETY: 宿主保证 info 指向可写的 ParameterInfo。
        unsafe {
            (*info) = Vst::ParameterInfo {
                id: 1,
                title: utf16_field::<128>("Gain"),
                shortTitle: utf16_field::<128>("Gain"),
                units: utf16_field::<128>(""),
                stepCount: 0,
                defaultNormalizedValue: 0.5,
                unitId: 0,
                flags: Vst::ParameterInfo_::ParameterFlags_::kCanAutomate,
            };
        }
        Steinberg::kResultOk
    }

    unsafe fn getParamStringByValue(
        &self,
        _id: Vst::ParamID,
        _valueNormalized: Vst::ParamValue,
        _string: *mut Vst::String128,
    ) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn getParamValueByString(
        &self,
        _id: Vst::ParamID,
        _string: *mut Vst::TChar,
        _valueNormalized: *mut Vst::ParamValue,
    ) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn normalizedParamToPlain(
        &self,
        _id: Vst::ParamID,
        valueNormalized: Vst::ParamValue,
    ) -> Vst::ParamValue {
        valueNormalized
    }

    unsafe fn plainParamToNormalized(
        &self,
        _id: Vst::ParamID,
        plainValue: Vst::ParamValue,
    ) -> Vst::ParamValue {
        plainValue
    }

    unsafe fn getParamNormalized(&self, _id: Vst::ParamID) -> Vst::ParamValue {
        0.0
    }

    unsafe fn setParamNormalized(
        &self,
        _id: Vst::ParamID,
        _value: Vst::ParamValue,
    ) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn setComponentHandler(
        &self,
        _handler: *mut Vst::IComponentHandler,
    ) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn createView(&self, _name: Steinberg::FIDString) -> *mut Steinberg::IPlugView {
        std::ptr::null_mut()
    }
}

impl IConnectionPointTrait for SineController {
    unsafe fn connect(&self, _other: *mut Vst::IConnectionPoint) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn disconnect(&self, _other: *mut Vst::IConnectionPoint) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn notify(&self, _message: *mut Steinberg::Vst::IMessage) -> Steinberg::tresult {
        Steinberg::kResultOk
    }
}

// ---------------------------------------------------------------------------
// 模块入口（Windows 单文件模块约定）
// ---------------------------------------------------------------------------

static FACTORY: OnceLock<ComWrapper<SineFactory>> = OnceLock::new();

/// VST3 模块入口：返回 +1 引用计数的工厂指针。
#[unsafe(no_mangle)]
pub extern "system" fn GetPluginFactory() -> *mut c_void {
    let factory = FACTORY.get_or_init(|| ComWrapper::new(SineFactory));
    match factory.to_com_ptr::<Steinberg::IPluginFactory>() {
        Some(ptr) => ptr.into_raw() as *mut c_void,
        None => std::ptr::null_mut(),
    }
}

/// Windows SDK 约定的模块初始化（简化实现：无全局状态）。
#[unsafe(no_mangle)]
pub extern "system" fn InitDll() -> i32 {
    1
}

/// Windows SDK 约定的模块反初始化（工厂静态存活到进程退出）。
#[unsafe(no_mangle)]
pub extern "system" fn ExitDll() -> i32 {
    0
}
