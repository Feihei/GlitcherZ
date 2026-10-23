//! # glitcherz-test-vst3
//!
//! Test VST3 plugin set (cdylib fixture, loaded only by the end-to-end
//! tests of `glitcherz-vst3-host`).
//!
//! Structure (a minimal implementation of the VST3 dual-object model):
//! - `SineFactory`: IPluginFactory(2) — enumerates an instrument and an
//!   effect class;
//! - `SineComponent`: IPluginBase + IComponent + IAudioProcessor +
//!   IConnectionPoint — an A4 sine instrument, triggered by NoteOn and
//!   stopped by NoteOff. Persists its Gain parameter (id 1, normalized
//!   f64) through `IComponent::getState`/`setState` as an 8-byte
//!   little-endian blob, consumed from the host's `IParameterChanges`
//!   queue during processing (state roundtrip fixture, spec S5);
//! - `SineController`: IPluginBase + IEditController + IConnectionPoint +
//!   IEditControllerHostEditing — a Gain parameter (id 1, default 0.5)
//!   plus a read-only `GuiCalls` probe (id 2) and an editor-view factory;
//! - `SinePlugView`: headless `IPlugView` fixture for the G7 protocol tests
//!   (attach/remove/size bookkeeping, a `resizeView` request at attach time
//!   and a simulated GUI parameter edit through the host handler);
//! - `GainEffectComponent`: stereo passthrough gain effect
//!   (output = input × 0.5, no event input bus); the controller reuses
//!   `SineController`.
//!
//! Export symbols: `GetPluginFactory` / `InitDll` / `ExitDll` (Windows
//! single-file module convention).
//!
//! CIDs are defined as literal byte arrays (ASCII "GLITCHERZSINE" + a
//! sequence number); host tests reference the same hex strings to avoid
//! endianness ambiguity:
//! - Instrument component: `5245534f5a53494e4500000000000001`
//! - Controller: `5245534f5a53494e4500000000000002`
//! - Effect component: `5245534f5a53494e4500000000000003`

#![allow(non_snake_case, non_upper_case_globals)]
// COM boundary: vtable signatures are fixed as unsafe by the SDK bindings;
// plugin implementations likewise.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use vst3::Steinberg::Vst::{
    self, IAudioProcessorTrait, IComponentHandlerTrait, IComponentTrait, IConnectionPointTrait,
    IEditControllerHostEditingTrait, IEditControllerTrait, IEventListTrait, IParamValueQueueTrait,
    IParameterChangesTrait,
};
use vst3::Steinberg::{
    self, IBStreamTrait, IPlugFrameTrait, IPlugViewTrait, IPluginBaseTrait, TUID,
};
use vst3::{Class, ComRef, ComWrapper};

// ---------------------------------------------------------------------------
// Enum type seam
//
// The generated bindings spell each enum twice: the constant containers
// (`Vst::MediaTypes`, `Vst::BusDirections`, `Vst::SymbolicSampleSizes`,
// `Vst::Event_::EventTypes`, `Steinberg::PClassInfo_::ClassCardinality`, ...)
// are `vst3::support::DefaultEnumType` — `c_int` on Windows, `c_uint`
// everywhere else — while the struct fields and interface parameters that
// receive them are `int32`. On Windows both are `i32`; on macOS/Linux they
// are distinct types, so the fixture converts at every hand-off.

/// Generated enum constant → `int32` slot (fields, arguments, comparisons).
/// Enum discriminants are small non-negative values, so this never fails.
#[inline]
fn to_int32<T>(value: T) -> Steinberg::int32
where
    T: TryInto<Steinberg::int32>,
{
    value.try_into().unwrap_or(0)
}

/// Generated enum constant → `uint32` slot (e.g. `BusInfo::flags`), for the
/// unsigned fields (`as u32` would be a redundant cast off Windows).
#[inline]
fn to_uint32<T>(value: T) -> Steinberg::uint32
where
    T: TryInto<Steinberg::uint32>,
{
    value.try_into().unwrap_or(0)
}

/// `int32` interface value → generated enum type, so incoming parameters can
/// be `match`ed against the enum constants (`T` is inferred from the arms).
#[inline]
fn to_enum<T>(value: Steinberg::int32) -> T
where
    T: TryFrom<Steinberg::int32> + Default,
{
    T::try_from(value).unwrap_or_default()
}

/// GUI protocol probe bits (read-only parameter id 2, normalized as
/// `mask / 1024` — ≤ 10 bits, an exact binary fraction).
const GUI_ATTACHED: u64 = 1 << 0;
const GUI_REMOVED: u64 = 1 << 1;
const GUI_GET_SIZE: u64 = 1 << 2;
const GUI_ON_SIZE: u64 = 1 << 3;
const GUI_SET_FRAME: u64 = 1 << 4;
const GUI_CAN_RESIZE: u64 = 1 << 5;
const GUI_RESIZE_REQUESTED: u64 = 1 << 6;
const GUI_PERFORM_EDIT: u64 = 1 << 7;
const GUI_BEGIN_HOST_EDIT: u64 = 1 << 8;
const GUI_END_HOST_EDIT: u64 = 1 << 9;

/// Normalized scaling of the probe mask.
const GUI_MASK_SCALE: f64 = 1024.0;

/// Fake editor view size (pixels).
const VIEW_WIDTH: i32 = 320;
const VIEW_HEIGHT: i32 = 240;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Component CID ("GLITCHERZSINE" + 0x01).
const COMPONENT_CID: TUID = [
    0x52, 0x45, 0x53, 0x4F, 0x5A, 0x53, 0x49, 0x4E, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];
/// Controller CID ("GLITCHERZSINE" + 0x02).
const CONTROLLER_CID: TUID = [
    0x52, 0x45, 0x53, 0x4F, 0x5A, 0x53, 0x49, 0x4E, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02,
];
/// Effect component CID ("GLITCHERZSINE" + 0x03).
const EFFECT_COMPONENT_CID: TUID = [
    0x52, 0x45, 0x53, 0x4F, 0x5A, 0x53, 0x49, 0x4E, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03,
];

const PLUGIN_NAME: &str = "GlitcherZ Sine VST3";
const EFFECT_NAME: &str = "GlitcherZ Gain VST3";
const VENDOR: &str = "GlitcherZ";

/// A4 frequency (consistent with the built-in engine test convention).
const SINE_FREQ_HZ: f32 = 440.0;

/// Reuses sample_rate (passed in by the host's setupProcessing; the
/// simplified implementation ignores it, tests fix it at 48 kHz).
const SAMPLE_RATE: f32 = 48_000.0;

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Fills a string into a NUL-terminated fixed-length char8 field.
fn c_field<const N: usize>(text: &str) -> [Steinberg::char8; N] {
    let mut out = [0 as Steinberg::char8; N];
    for (index, byte) in text.bytes().take(N - 1).enumerate() {
        out[index] = byte as Steinberg::char8;
    }
    out
}

/// Fills a string into a NUL-terminated fixed-length TChar (UTF-16) field.
fn utf16_field<const N: usize>(text: &str) -> [Steinberg::char16; N] {
    let mut out = [0u16; N];
    for (index, unit) in text.encode_utf16().take(N - 1).enumerate() {
        out[index] = unit;
    }
    out
}

/// Atomic access to f32 (simplified carrier for velocity/phase).
fn f32_bits_cell(value: &AtomicU32) -> f32 {
    f32::from_bits(value.load(Ordering::Relaxed))
}

fn set_f32_bits_cell(cell: &AtomicU32, value: f32) {
    cell.store(value.to_bits(), Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Single-plugin factory.
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
        // SAFETY: The host provides a writable PFactoryInfo per spec.
        unsafe {
            *info = Steinberg::PFactoryInfo {
                vendor: c_field::<64>(VENDOR),
                url: c_field::<256>("https://github.com/glitcherz"),
                email: c_field::<128>(""),
                flags: 0,
            };
        }
        Steinberg::kResultOk
    }

    unsafe fn countClasses(&self) -> Steinberg::int32 {
        // The edit controller is enumerated too (spec-conformant factories
        // list it under the "Component Controller Class" category) — the
        // host-side scanner filter regression test relies on it.
        3
    }

    unsafe fn getClassInfo(
        &self,
        index: Steinberg::int32,
        info: *mut Steinberg::PClassInfo,
    ) -> Steinberg::tresult {
        // SAFETY: The host provides a writable PClassInfo per spec.
        unsafe {
            *info = match index {
                0 => Steinberg::PClassInfo {
                    cid: COMPONENT_CID,
                    cardinality: to_int32(
                        Steinberg::PClassInfo_::ClassCardinality_::kManyInstances,
                    ),
                    category: c_field::<32>("Audio Module Class"),
                    name: c_field::<64>(PLUGIN_NAME),
                },
                1 => Steinberg::PClassInfo {
                    cid: EFFECT_COMPONENT_CID,
                    cardinality: to_int32(
                        Steinberg::PClassInfo_::ClassCardinality_::kManyInstances,
                    ),
                    category: c_field::<32>("Audio Module Class"),
                    name: c_field::<64>(EFFECT_NAME),
                },
                2 => Steinberg::PClassInfo {
                    cid: CONTROLLER_CID,
                    cardinality: to_int32(
                        Steinberg::PClassInfo_::ClassCardinality_::kManyInstances,
                    ),
                    category: c_field::<32>("Component Controller Class"),
                    name: c_field::<64>("GlitcherZ Sine VST3 Controller"),
                },
                _ => return Steinberg::kInvalidArgument,
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
        // SAFETY: cid points to the host-provided 16-byte CID; returns an
        // interface pointer with a +1 reference count (to_com_ptr), owned
        // by the host.
        unsafe {
            let requested: [u8; 16] = std::slice::from_raw_parts(cid as *const u8, 16)
                .try_into()
                .unwrap_or([0; 16]);
            let matches = |pattern: &TUID| {
                requested
                    .iter()
                    .zip(pattern.iter())
                    .all(|(a, b)| (*a as i8) == *b)
            };
            if matches(&COMPONENT_CID) {
                // Per kManyInstances, every createInstance call yields a
                // fresh instance (host-owned via the +1 refcounted pointer)
                // — fresh-instance state tests rely on this.
                let wrapper = ComWrapper::new(SineComponent::new());
                if let Some(ptr) = wrapper.to_com_ptr::<Vst::IComponent>() {
                    *obj = ptr.into_raw() as *mut c_void;
                    return Steinberg::kResultOk;
                }
                return Steinberg::kResultFalse;
            }
            if matches(&CONTROLLER_CID) {
                let wrapper = ComWrapper::new(SineController::new());
                if let Some(ptr) = wrapper.to_com_ptr::<Vst::IEditController>() {
                    *obj = ptr.into_raw() as *mut c_void;
                    return Steinberg::kResultOk;
                }
                return Steinberg::kResultFalse;
            }
            if matches(&EFFECT_COMPONENT_CID) {
                let wrapper = ComWrapper::new(GainEffectComponent);
                if let Some(ptr) = wrapper.to_com_ptr::<Vst::IComponent>() {
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
        let (cid, name, sub_categories, category) = match index {
            0 => (
                COMPONENT_CID,
                PLUGIN_NAME,
                "Instrument|Synth|Stereo",
                "Audio Module Class",
            ),
            1 => (
                EFFECT_COMPONENT_CID,
                EFFECT_NAME,
                "Fx|Stereo",
                "Audio Module Class",
            ),
            2 => (
                CONTROLLER_CID,
                "GlitcherZ Sine VST3 Controller",
                "",
                "Component Controller Class",
            ),
            _ => return Steinberg::kInvalidArgument,
        };
        // SAFETY: The host provides a writable PClassInfo2 per spec.
        unsafe {
            *info = Steinberg::PClassInfo2 {
                cid,
                cardinality: to_int32(Steinberg::PClassInfo_::ClassCardinality_::kManyInstances),
                category: c_field::<32>(category),
                name: c_field::<64>(name),
                classFlags: 0,
                subCategories: c_field::<128>(sub_categories),
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
// Component (IComponent + IAudioProcessor)
// ---------------------------------------------------------------------------

/// A4 sine instrument component: triggered by NoteOn, silenced by NoteOff;
/// single voice, stereo output. Carries the Gain parameter (id 1,
/// normalized 0..1, default 0.5) as persisted state: consumed from the
/// host's `IParameterChanges` queue during processing, output amplitude =
/// velocity × gain, saved/restored through `getState`/`setState` as an
/// 8-byte little-endian f64 blob.
struct SineComponent {
    note_on: AtomicBool,
    velocity_bits: AtomicU32,
    phase_bits: AtomicU32,
    gain_bits: AtomicU64,
}

/// Bit pattern of the default normalized gain (0.5).
const DEFAULT_GAIN_BITS: u64 = 0.5_f64.to_bits();

impl SineComponent {
    fn new() -> Self {
        Self {
            note_on: AtomicBool::new(false),
            velocity_bits: AtomicU32::new(0.0_f32.to_bits()),
            phase_bits: AtomicU32::new(0.0_f32.to_bits()),
            gain_bits: AtomicU64::new(DEFAULT_GAIN_BITS),
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
        // SAFETY: The host provides a writable TUID.
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
        match (to_enum(r#type), to_enum(dir)) {
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
        // SAFETY: The host provides a writable BusInfo.
        unsafe {
            *bus = Vst::BusInfo {
                mediaType: r#type,
                direction: dir,
                channelCount: match to_enum(r#type) {
                    Vst::MediaTypes_::kAudio => 2,
                    _ => 1,
                },
                name: match to_enum(r#type) {
                    Vst::MediaTypes_::kAudio => utf16_field::<128>("Stereo Out"),
                    _ => utf16_field::<128>("Note In"),
                },
                busType: to_int32(Vst::BusTypes_::kMain),
                flags: to_uint32(Vst::BusInfo_::BusFlags_::kDefaultActive),
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

    unsafe fn setState(&self, state: *mut Steinberg::IBStream) -> Steinberg::tresult {
        // State blob: 8-byte little-endian f64 (normalized gain bits).
        // SAFETY: the stream pointer is provided by the host per spec and
        // stays valid for the duration of the call.
        let Some(stream) = (unsafe { ComRef::<Steinberg::IBStream>::from_raw(state) }) else {
            return Steinberg::kInvalidArgument;
        };
        let mut bits = [0u8; 8];
        let mut read = 0i32;
        // SAFETY: the stream is provided by the host per spec; the byte
        // buffer is writable for 8 elements.
        let result = unsafe { stream.read(bits.as_mut_ptr().cast(), 8, &mut read) };
        if result == Steinberg::kResultOk && read == 8 {
            self.gain_bits
                .store(u64::from_le_bytes(bits), Ordering::Relaxed);
            Steinberg::kResultOk
        } else {
            Steinberg::kResultFalse
        }
    }

    unsafe fn getState(&self, state: *mut Steinberg::IBStream) -> Steinberg::tresult {
        // SAFETY: the stream pointer is provided by the host per spec and
        // stays valid for the duration of the call.
        let Some(stream) = (unsafe { ComRef::<Steinberg::IBStream>::from_raw(state) }) else {
            return Steinberg::kInvalidArgument;
        };
        let bits = self.gain_bits.load(Ordering::Relaxed).to_le_bytes();
        let mut written = 0i32;
        // SAFETY: the stream is provided by the host per spec; writing 8
        // bytes through the vtable.
        let result = unsafe { stream.write(bits.as_ptr().cast_mut().cast(), 8, &mut written) };
        if result == Steinberg::kResultOk && written == 8 {
            Steinberg::kResultOk
        } else {
            Steinberg::kResultFalse
        }
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
        // Only accept 0 in / 1 out (stereo).
        const K_STEREO: Vst::SpeakerArrangement = 3; // L|R
        if numIns == 0
            && numOuts == 1
            // SAFETY: The host provides numOuts readable elements per spec.
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
        if symbolicSampleSize == to_int32(Vst::SymbolicSampleSizes_::kSample32) {
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
        // Reset the phase when entering/leaving the processing state to
        // keep tests deterministic.
        self.phase_bits.store(0.0_f32.to_bits(), Ordering::Relaxed);
        Steinberg::kResultOk
    }

    unsafe fn process(&self, data: *mut Vst::ProcessData) -> Steinberg::tresult {
        // SAFETY: The host fills ProcessData per spec; output pointers are
        // writable within numSamples.
        unsafe {
            let data = &mut *data;
            if data.numOutputs < 1
                || data.outputs.is_null()
                || data.symbolicSampleSize != to_int32(Vst::SymbolicSampleSizes_::kSample32)
            {
                return Steinberg::kNotImplemented;
            }
            let frames = data.numSamples.max(0) as usize;
            let bus = &mut *data.outputs;
            let channels = (bus.numChannels.max(0) as usize).min(2);
            bus.silenceFlags = 0;

            // Consume host events (NoteOn/NoteOff).
            if !data.inputEvents.is_null() {
                if let Some(events) = ComRef::<Vst::IEventList>::from_raw(data.inputEvents) {
                    let count = events.getEventCount();
                    for index in 0..count {
                        let mut event: Vst::Event = std::mem::zeroed();
                        if events.getEvent(index, &mut event) == Steinberg::kResultOk {
                            match to_enum(event.r#type as i32) {
                                Vst::Event_::EventTypes_::kNoteOnEvent => {
                                    let note_on = &event.__field0.noteOn;
                                    self.note_on.store(true, Ordering::Relaxed);
                                    set_f32_bits_cell(&self.velocity_bits, note_on.velocity);
                                    // Reset the phase to keep tests
                                    // deterministic.
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

            // Consume host parameter changes (id 1 = normalized gain; the
            // last point in the queue wins).
            if !data.inputParameterChanges.is_null() {
                if let Some(changes) =
                    ComRef::<Vst::IParameterChanges>::from_raw(data.inputParameterChanges)
                {
                    let queues = changes.getParameterCount();
                    for queue_index in 0..queues {
                        let Some(queue) = ComRef::<Vst::IParamValueQueue>::from_raw(
                            changes.getParameterData(queue_index),
                        ) else {
                            continue;
                        };
                        if queue.getParameterId() != 1 {
                            continue;
                        }
                        let points = queue.getPointCount();
                        if points <= 0 {
                            continue;
                        }
                        let mut offset = 0i32;
                        let mut value = 0f64;
                        if queue.getPoint(points - 1, &mut offset, &mut value)
                            == Steinberg::kResultOk
                        {
                            self.gain_bits.store(value.to_bits(), Ordering::Relaxed);
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
            let gain = f64::from_bits(self.gain_bits.load(Ordering::Relaxed)) as f32;
            let step = std::f32::consts::TAU * SINE_FREQ_HZ / SAMPLE_RATE;
            let mut phase = f32_bits_cell(&self.phase_bits);
            for sample in 0..frames {
                let value = phase.sin() * velocity * gain;
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
// Gain effect component (stereo passthrough ×0.5, no event input)
// ---------------------------------------------------------------------------

/// Stereo passthrough gain effect: output = input × 0.5 (fixed gain, used
/// by host end-to-end tests to assert the input path). The controller
/// reuses `SineController`.
struct GainEffectComponent;

impl Class for GainEffectComponent {
    type Interfaces = (
        Steinberg::IPluginBase,
        Vst::IComponent,
        Vst::IAudioProcessor,
        Vst::IConnectionPoint,
    );
}

impl IPluginBaseTrait for GainEffectComponent {
    unsafe fn initialize(&self, _context: *mut Steinberg::FUnknown) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn terminate(&self) -> Steinberg::tresult {
        Steinberg::kResultOk
    }
}

impl IComponentTrait for GainEffectComponent {
    unsafe fn getControllerClassId(&self, classId: *mut TUID) -> Steinberg::tresult {
        // SAFETY: The host provides a writable TUID.
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
        match (to_enum(r#type), to_enum(dir)) {
            (Vst::MediaTypes_::kAudio, Vst::BusDirections_::kInput) => 1,
            (Vst::MediaTypes_::kAudio, Vst::BusDirections_::kOutput) => 1,
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
        if index != 0 || r#type != to_int32(Vst::MediaTypes_::kAudio) {
            return Steinberg::kInvalidArgument;
        }
        // SAFETY: The host provides a writable BusInfo.
        unsafe {
            *bus = Vst::BusInfo {
                mediaType: r#type,
                direction: dir,
                channelCount: 2,
                name: match to_enum(dir) {
                    Vst::BusDirections_::kInput => utf16_field::<128>("Stereo In"),
                    _ => utf16_field::<128>("Stereo Out"),
                },
                busType: to_int32(Vst::BusTypes_::kMain),
                flags: to_uint32(Vst::BusInfo_::BusFlags_::kDefaultActive),
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

impl IAudioProcessorTrait for GainEffectComponent {
    unsafe fn setBusArrangements(
        &self,
        inputs: *mut Vst::SpeakerArrangement,
        numIns: Steinberg::int32,
        outputs: *mut Vst::SpeakerArrangement,
        numOuts: Steinberg::int32,
    ) -> Steinberg::tresult {
        // Only accept 1 in / 1 out (stereo).
        const K_STEREO: Vst::SpeakerArrangement = 3; // L|R
        if numIns == 1
            && numOuts == 1
            // SAFETY: The host provides the corresponding number of
            // readable elements per spec.
            && unsafe { *inputs } == K_STEREO
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
        if symbolicSampleSize == to_int32(Vst::SymbolicSampleSizes_::kSample32) {
            Steinberg::kResultTrue
        } else {
            Steinberg::kNotImplemented
        }
    }

    unsafe fn getLatencySamples(&self) -> Steinberg::uint32 {
        0
    }

    unsafe fn getTailSamples(&self) -> Steinberg::uint32 {
        0
    }

    unsafe fn setupProcessing(&self, _setup: *mut Vst::ProcessSetup) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn setProcessing(&self, _state: Steinberg::TBool) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn process(&self, data: *mut Vst::ProcessData) -> Steinberg::tresult {
        // SAFETY: The host fills ProcessData per spec; input readable,
        // output writable.
        unsafe {
            let data = &mut *data;
            if data.numOutputs < 1
                || data.outputs.is_null()
                || data.numInputs < 1
                || data.inputs.is_null()
                || data.symbolicSampleSize != to_int32(Vst::SymbolicSampleSizes_::kSample32)
            {
                return Steinberg::kNotImplemented;
            }
            let frames = data.numSamples.max(0) as usize;
            let input = &*data.inputs;
            let output = &mut *data.outputs;
            output.silenceFlags = 0;
            let in_channels = (input.numChannels.max(0) as usize).min(2);
            let out_channels = (output.numChannels.max(0) as usize).min(2);
            for channel in 0..out_channels {
                let out_base = *output.__field0.channelBuffers32.add(channel);
                // Output channels map to input channels (reuse the last
                // input channel when insufficient).
                let in_channel = if in_channels == 0 {
                    0
                } else if channel < in_channels {
                    channel
                } else {
                    in_channels - 1
                };
                let in_base = *input.__field0.channelBuffers32.add(in_channel);
                for sample in 0..frames {
                    *out_base.add(sample) = *in_base.add(sample) * 0.5;
                }
            }
        }
        Steinberg::kResultOk
    }
}

impl IConnectionPointTrait for GainEffectComponent {
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
// Controller (IEditController, dumb implementation)
// ---------------------------------------------------------------------------

/// Dumb controller mirroring the component's Gain parameter (id 1): keeps a
/// normalized value cell updated through `setComponentState` (the state
/// load sync) and `setParamNormalized`, read back by the host via
/// `getParamNormalized` (the D6 slider-refresh vehicle, spec S5).
///
/// GUI fixture (spec `plugin-gui.md` §9): exposes the host handler pointer
/// to the views it creates, and a read-only parameter (id 2) carrying the
/// GUI call mask, so the host test can assert the exact
/// createView/attached/getSize/removed transaction (cdylib statics are not
/// reachable from the host process — the observation must travel through
/// the COM surface).
struct SineController {
    gain_bits: AtomicU64,
    /// Host component handler (set by `setComponentHandler`), handed to
    /// every view so it can report a simulated GUI edit.
    handler: AtomicPtr<Vst::IComponentHandler>,
    /// GUI protocol probe bits (`GUI_*`).
    gui_calls: Arc<AtomicU64>,
}

impl SineController {
    fn new() -> Self {
        Self {
            gain_bits: AtomicU64::new(DEFAULT_GAIN_BITS),
            handler: AtomicPtr::new(std::ptr::null_mut()),
            gui_calls: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Class for SineController {
    type Interfaces = (
        Steinberg::IPluginBase,
        Vst::IEditController,
        Vst::IEditControllerHostEditing,
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
    unsafe fn setComponentState(&self, state: *mut Steinberg::IBStream) -> Steinberg::tresult {
        // Mirrors the component state blob (8-byte LE f64 normalized gain).
        // SAFETY: the stream pointer is provided by the host per spec and
        // stays valid for the duration of the call.
        let Some(stream) = (unsafe { ComRef::<Steinberg::IBStream>::from_raw(state) }) else {
            return Steinberg::kInvalidArgument;
        };
        let mut bits = [0u8; 8];
        let mut read = 0i32;
        // SAFETY: the stream is provided by the host per spec; the byte
        // buffer is writable for 8 elements.
        let result = unsafe { stream.read(bits.as_mut_ptr().cast(), 8, &mut read) };
        if result == Steinberg::kResultOk && read == 8 {
            self.gain_bits
                .store(u64::from_le_bytes(bits), Ordering::Relaxed);
            Steinberg::kResultOk
        } else {
            Steinberg::kResultFalse
        }
    }

    unsafe fn setState(&self, _state: *mut Steinberg::IBStream) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn getState(&self, _state: *mut Steinberg::IBStream) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn getParameterCount(&self) -> Steinberg::int32 {
        // Gain + the GUI protocol probe (read-only).
        2
    }

    unsafe fn getParameterInfo(
        &self,
        paramIndex: Steinberg::int32,
        info: *mut Vst::ParameterInfo,
    ) -> Steinberg::tresult {
        let (id, title, default) = match paramIndex {
            0 => (1, "Gain", 0.5),
            1 => (
                2,
                "GuiCalls",
                self.gui_calls.load(Ordering::Relaxed) as f64 / GUI_MASK_SCALE,
            ),
            _ => return Steinberg::kInvalidArgument,
        };
        // SAFETY: The host guarantees info points to a writable
        // ParameterInfo.
        unsafe {
            (*info) = Vst::ParameterInfo {
                id,
                title: utf16_field::<128>(title),
                shortTitle: utf16_field::<128>(title),
                units: utf16_field::<128>(""),
                stepCount: 0,
                defaultNormalizedValue: default,
                unitId: 0,
                flags: if id == 1 {
                    Vst::ParameterInfo_::ParameterFlags_::kCanAutomate
                } else {
                    Vst::ParameterInfo_::ParameterFlags_::kIsReadOnly
                },
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

    unsafe fn getParamNormalized(&self, id: Vst::ParamID) -> Vst::ParamValue {
        match id {
            1 => f64::from_bits(self.gain_bits.load(Ordering::Relaxed)),
            // Read-only probe: the GUI call mask as an exact binary
            // fraction (all bits < 8, so mask / 256 is lossless).
            2 => self.gui_calls.load(Ordering::Relaxed) as f64 / GUI_MASK_SCALE,
            _ => 0.0,
        }
    }

    unsafe fn setParamNormalized(
        &self,
        id: Vst::ParamID,
        value: Vst::ParamValue,
    ) -> Steinberg::tresult {
        if id == 1 {
            self.gain_bits.store(value.to_bits(), Ordering::Relaxed);
            return Steinberg::kResultTrue;
        }
        Steinberg::kInvalidArgument
    }

    unsafe fn setComponentHandler(
        &self,
        handler: *mut Vst::IComponentHandler,
    ) -> Steinberg::tresult {
        self.handler.store(handler, Ordering::Relaxed);
        Steinberg::kResultOk
    }

    unsafe fn createView(&self, name: Steinberg::FIDString) -> *mut Steinberg::IPlugView {
        if name.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: `name` comes from the host per spec and is a
        // NUL-terminated FIDString; `ViewType::kEditor` is a static
        // NUL-terminated literal.
        let matches_editor = unsafe {
            let requested = std::ffi::CStr::from_ptr(name);
            let editor = std::ffi::CStr::from_ptr(Vst::ViewType::kEditor);
            requested == editor
        };
        if !matches_editor {
            return std::ptr::null_mut();
        }
        let view = ComWrapper::new(SinePlugView {
            self_ptr: AtomicPtr::new(std::ptr::null_mut()),
            frame: AtomicPtr::new(std::ptr::null_mut()),
            handler: self.handler.load(Ordering::Relaxed),
            calls: Arc::clone(&self.gui_calls),
        });
        // The +1 reference handed to the host (the ComWrapper's Rust data
        // stays owned by the COM object itself — the factory pattern).
        let Some(ptr) = view.to_com_ptr::<Steinberg::IPlugView>() else {
            return std::ptr::null_mut();
        };
        // The view needs its own COM pointer to call back into the host
        // frame (`resizeView(view, rect)`); storing through an atomic needs
        // no unsafe and is visible to every later call.
        view.self_ptr.store(ptr.as_ptr(), Ordering::Release);
        ptr.into_raw()
    }
}

impl IEditControllerHostEditingTrait for SineController {
    unsafe fn beginEditFromHost(&self, _paramID: Vst::ParamID) -> Steinberg::tresult {
        self.gui_calls
            .fetch_or(GUI_BEGIN_HOST_EDIT, Ordering::Relaxed);
        Steinberg::kResultOk
    }

    unsafe fn endEditFromHost(&self, _paramID: Vst::ParamID) -> Steinberg::tresult {
        self.gui_calls
            .fetch_or(GUI_END_HOST_EDIT, Ordering::Relaxed);
        Steinberg::kResultOk
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
// Fake editor view (IPlugView) — headless protocol fixture
// ---------------------------------------------------------------------------

/// Minimal `IPlugView`: no real window, records the call sequence in the
/// GUI probe mask and exercises both host directions:
///
/// - **host → view**: `setFrame`, `attached`, `getSize`, `onSize`,
///   `removed`;
/// - **view → host**: `IPlugFrame::resizeView` (a 320×240 request at
///   attach time) and `IComponentHandler::performEdit` (a simulated GUI
///   edit of Gain to 0.75 — the `request_flush` stand-in).
struct SinePlugView {
    /// The view's own COM interface pointer (needed for `resizeView`).
    self_ptr: AtomicPtr<Steinberg::IPlugView>,
    /// Plug frame set by the host through `setFrame`.
    frame: AtomicPtr<Steinberg::IPlugFrame>,
    /// Host component handler (borrowed; the host owns it).
    handler: *mut Vst::IComponentHandler,
    /// Probe bits shared with the controller.
    calls: Arc<AtomicU64>,
}

impl SinePlugView {
    fn mark(&self, bit: u64) {
        self.calls.fetch_or(bit, Ordering::Relaxed);
    }
}

impl Class for SinePlugView {
    type Interfaces = (Steinberg::IPlugView,);
}

impl IPlugViewTrait for SinePlugView {
    unsafe fn isPlatformTypeSupported(&self, r#type: Steinberg::FIDString) -> Steinberg::tresult {
        if r#type.is_null() {
            return Steinberg::kInvalidArgument;
        }
        // SAFETY: host-provided NUL-terminated FIDString.
        let requested = unsafe { std::ffi::CStr::from_ptr(r#type) };
        if requested == unsafe { std::ffi::CStr::from_ptr(Steinberg::kPlatformTypeHWND) } {
            Steinberg::kResultTrue
        } else {
            Steinberg::kResultFalse
        }
    }

    unsafe fn attached(
        &self,
        parent: *mut c_void,
        r#type: Steinberg::FIDString,
    ) -> Steinberg::tresult {
        if parent.is_null() || r#type.is_null() {
            return Steinberg::kInvalidArgument;
        }
        self.mark(GUI_ATTACHED);
        // Ask the host for a size (view → host direction).
        // SAFETY: the frame pointer was set by the host and is alive for
        // the attached lifetime; the rect is a stack out-parameter.
        unsafe {
            let frame = self.frame.load(Ordering::Acquire);
            if !frame.is_null()
                && let Some(frame) = ComRef::<Steinberg::IPlugFrame>::from_raw(frame)
            {
                let mut rect = Steinberg::ViewRect {
                    left: 0,
                    top: 0,
                    right: VIEW_WIDTH,
                    bottom: VIEW_HEIGHT,
                };
                if frame.resizeView(self.self_ptr.load(Ordering::Acquire), &mut rect)
                    == Steinberg::kResultOk
                {
                    self.mark(GUI_RESIZE_REQUESTED);
                }
            }
            // Simulate a user edit inside the GUI (the CLAP `hide` hook
            // mirror): report Gain = 0.75 through the host handler.
            let handler = self.handler;
            if !handler.is_null()
                && let Some(handler) = ComRef::<Vst::IComponentHandler>::from_raw(handler)
                && handler.performEdit(1, 0.75) == Steinberg::kResultOk
            {
                self.mark(GUI_PERFORM_EDIT);
            }
        }
        Steinberg::kResultOk
    }

    unsafe fn removed(&self) -> Steinberg::tresult {
        self.mark(GUI_REMOVED);
        Steinberg::kResultOk
    }

    unsafe fn onWheel(&self, _distance: f32) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn onKeyDown(
        &self,
        _key: Steinberg::char16,
        _key_code: Steinberg::int16,
        _modifiers: Steinberg::int16,
    ) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn onKeyUp(
        &self,
        _key: Steinberg::char16,
        _key_code: Steinberg::int16,
        _modifiers: Steinberg::int16,
    ) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn getSize(&self, size: *mut Steinberg::ViewRect) -> Steinberg::tresult {
        if size.is_null() {
            return Steinberg::kInvalidArgument;
        }
        self.mark(GUI_GET_SIZE);
        // SAFETY: writable out-parameter provided by the host.
        unsafe {
            *size = Steinberg::ViewRect {
                left: 0,
                top: 0,
                right: VIEW_WIDTH,
                bottom: VIEW_HEIGHT,
            };
        }
        Steinberg::kResultOk
    }

    unsafe fn onSize(&self, new_size: *mut Steinberg::ViewRect) -> Steinberg::tresult {
        if new_size.is_null() {
            return Steinberg::kInvalidArgument;
        }
        self.mark(GUI_ON_SIZE);
        Steinberg::kResultOk
    }

    unsafe fn onFocus(&self, _state: Steinberg::TBool) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }

    unsafe fn setFrame(&self, frame: *mut Steinberg::IPlugFrame) -> Steinberg::tresult {
        self.frame.store(frame, Ordering::Release);
        self.mark(GUI_SET_FRAME);
        Steinberg::kResultOk
    }

    unsafe fn canResize(&self) -> Steinberg::tresult {
        self.mark(GUI_CAN_RESIZE);
        Steinberg::kResultTrue
    }

    unsafe fn checkSizeConstraint(&self, _rect: *mut Steinberg::ViewRect) -> Steinberg::tresult {
        Steinberg::kNotImplemented
    }
}

// ---------------------------------------------------------------------------
// Module entry points (Windows single-file module convention)
// ---------------------------------------------------------------------------

static FACTORY: OnceLock<ComWrapper<SineFactory>> = OnceLock::new();

/// VST3 module entry: returns a factory pointer with a +1 reference count.
#[unsafe(no_mangle)]
pub extern "system" fn GetPluginFactory() -> *mut c_void {
    let factory = FACTORY.get_or_init(|| ComWrapper::new(SineFactory));
    match factory.to_com_ptr::<Steinberg::IPluginFactory>() {
        Some(ptr) => ptr.into_raw() as *mut c_void,
        None => std::ptr::null_mut(),
    }
}

/// Windows SDK convention module initialization (simplified: no global
/// state).
#[unsafe(no_mangle)]
pub extern "system" fn InitDll() -> i32 {
    1
}

/// Windows SDK convention module deinitialization (the factory lives
/// statically until process exit).
#[unsafe(no_mangle)]
pub extern "system" fn ExitDll() -> i32 {
    0
}
