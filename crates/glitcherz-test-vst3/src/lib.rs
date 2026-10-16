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
//! - `SineController`: IPluginBase + IEditController + IConnectionPoint
//!   — a single Gain parameter (id 1, default 0.5);
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
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vst3::Steinberg::Vst::{
    self, IAudioProcessorTrait, IComponentTrait, IConnectionPointTrait, IEditControllerTrait,
    IEventListTrait, IParamValueQueueTrait, IParameterChangesTrait,
};
use vst3::Steinberg::{self, IBStreamTrait, IPluginBaseTrait, TUID};
use vst3::{Class, ComRef, ComWrapper};

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
                    cardinality: Steinberg::PClassInfo_::ClassCardinality_::kManyInstances,
                    category: c_field::<32>("Audio Module Class"),
                    name: c_field::<64>(PLUGIN_NAME),
                },
                1 => Steinberg::PClassInfo {
                    cid: EFFECT_COMPONENT_CID,
                    cardinality: Steinberg::PClassInfo_::ClassCardinality_::kManyInstances,
                    category: c_field::<32>("Audio Module Class"),
                    name: c_field::<64>(EFFECT_NAME),
                },
                2 => Steinberg::PClassInfo {
                    cid: CONTROLLER_CID,
                    cardinality: Steinberg::PClassInfo_::ClassCardinality_::kManyInstances,
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
                cardinality: Steinberg::PClassInfo_::ClassCardinality_::kManyInstances,
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
        // SAFETY: The host provides a writable BusInfo.
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
                || data.symbolicSampleSize != Vst::SymbolicSampleSizes_::kSample32
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
                            match event.r#type as i32 {
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
        match (r#type, dir) {
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
        if index != 0 || r#type != Vst::MediaTypes_::kAudio {
            return Steinberg::kInvalidArgument;
        }
        // SAFETY: The host provides a writable BusInfo.
        unsafe {
            *bus = Vst::BusInfo {
                mediaType: r#type,
                direction: dir,
                channelCount: 2,
                name: match dir {
                    Vst::BusDirections_::kInput => utf16_field::<128>("Stereo In"),
                    _ => utf16_field::<128>("Stereo Out"),
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
        if symbolicSampleSize == Vst::SymbolicSampleSizes_::kSample32 {
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
                || data.symbolicSampleSize != Vst::SymbolicSampleSizes_::kSample32
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
struct SineController {
    gain_bits: AtomicU64,
}

impl SineController {
    fn new() -> Self {
        Self {
            gain_bits: AtomicU64::new(DEFAULT_GAIN_BITS),
        }
    }
}

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
        // SAFETY: The host guarantees info points to a writable
        // ParameterInfo.
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

    unsafe fn getParamNormalized(&self, id: Vst::ParamID) -> Vst::ParamValue {
        if id == 1 {
            f64::from_bits(self.gain_bits.load(Ordering::Relaxed))
        } else {
            0.0
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
