//! # glitcherz-vst3-host
//!
//! VST3 plugin host adapter layer (non-realtime infrastructure): a bridge
//! between the GlitcherZ engine and the VST3 plugin ecosystem. The architecture
//! mirrors `glitcherz-clap-host`.
//!
//! Responsibilities:
//! - Scan VST3 modules (single files / bundle directories; standard
//!   directories + the `GLITCHERZ_VST3_PATH` environment variable);
//! - Enumerate plugin descriptions (CID / name / instrument or not);
//! - Instantiate plugins on a **non-realtime thread**: instantiate the
//!   `IComponent` and `IEditController`, wire up `IConnectionPoint` and bus
//!   activation, producing a [`Vst3Node`] that can run inside the audio
//!   callback (the real-time iron rules apply to its `process` path).
//!
//! Bridging model:
//! - **Notes**: [`Processor::note_event`] stashes `NoteOn/Off` into
//!   fixed-capacity event slots; the next `process` emits them as host
//!   `IEventList` events (block granularity);
//! - **Parameters**: `ParamId::PluginParam(u32)` is emitted through the host
//!   `IParameterChanges` queue, with values passed using **VST3 normalized
//!   semantics (0..1)** — plugin parameters written by the automation
//!   evaluator thus bridge naturally (unlike CLAP's raw-value semantics, the
//!   UI layer must convert through the controller to display plugin
//!   parameter values, see the roadmap note);
//! - **Audio**: instruments (no audio input buses) render into a
//!   node-internal preallocated stereo scratch buffer; effects (with audio
//!   input buses) automatically wire the input path (engine buffer →
//!   separate input buffer → plugin → separate output buffer → copy back
//!   into the engine buffer). Both use kSample32 single precision.
//!
//! Lifecycle:
//! - [`Vst3InstanceGuard`] holds the dynamic library and all plugin COM
//!   objects, and **must live as long as the [`Vst3Node`] and be dropped
//!   after it** (the same recycling-queue pairing convention as
//!   `ClapInstanceGuard`);
//! - Node drop calls `setProcessing(false)`; guard drop calls `setActive(false)`
//!   + `terminate()` + disconnect, and only then unloads the dynamic library.

#![allow(unsafe_code)]
// unsafe policy (as strict as glitcherz-clap-host/src/raw.rs): all three
// modules of this crate are VST3/COM boundary layers — raw (module
// loading), host_impl (host COM object implementations), lib (interface
// calls and lifecycle management). Every unsafe block carries a SAFETY
// comment; behavioral verification lives in tests (invalid files must
// return errors rather than UB).

pub mod host_impl;
pub mod raw;

use std::ffi::{CStr, c_char};
use std::path::{Path, PathBuf};

use vst3::Steinberg::Vst::{
    IAudioProcessorTrait, IComponentTrait, IConnectionPointTrait, IEditControllerTrait,
};
use vst3::Steinberg::{self, IPluginBaseTrait, IPluginFactory2Trait, IPluginFactoryTrait, Vst};
use vst3::{ComPtr, ComWrapper, Interface};

use std::sync::Arc;

use glitcherz_engine::host_gate::StateGate;
use glitcherz_engine::realtime::FIXED_BLOCK_SIZE;
use glitcherz_engine::realtime::buffer::AudioBuffer;
use glitcherz_engine::realtime::commands::ParamId;
use glitcherz_engine::realtime::context::ProcessContext;
use glitcherz_engine::realtime::processor::Processor;

use crate::host_impl::{
    HostEventList, HostMemoryStream, HostParamChanges, HostParamValueQueue, NoopComponentHandler,
    QueueSlot,
};

/// Maximum number of bridged events queued per `process` call (notes +
/// parameters combined).
pub const MAX_PENDING_EVENTS: usize = 32;

/// Number of preallocated parameter queues held by the node (cap on
/// distinct parameter IDs).
const PARAM_QUEUE_COUNT: usize = host_impl::MAX_PARAM_QUEUES;

/// Name of the environment variable holding plugin scan directories
/// (`PATH`-style separated list).
pub const VST3_PATH_ENV: &str = "GLITCHERZ_VST3_PATH";

/// Host-side errors.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("Failed to load plugin module: {0}")]
    Load(#[from] raw::RawError),
    #[error("Failed to create component instance (tresult = {0})")]
    CreateComponent(Steinberg::tresult),
    #[error("Component does not provide a controller CID")]
    NoControllerCid,
    #[error("Failed to create controller instance (tresult = {0})")]
    CreateController(Steinberg::tresult),
    #[error("Component does not implement IAudioProcessor")]
    NoAudioProcessor,
    #[error("Plugin does not support 32-bit sample processing")]
    SampleSizeUnsupported,
    #[error("Invalid plugin CID: {0}")]
    BadCid(String),
    #[error("Plugin failed to save state (tresult = {0:#x})")]
    StateSave(Steinberg::tresult),
    #[error("Plugin failed to load state (tresult = {0:#x})")]
    StateLoad(Steinberg::tresult),
}

/// Whether a tresult indicates success (kResultOk / kResultTrue).
fn tresult_ok(result: Steinberg::tresult) -> bool {
    result == Steinberg::kResultOk || result == Steinberg::kResultTrue
}

// ---------------------------------------------------------------------------
// CID representation
// ---------------------------------------------------------------------------

/// Formats a 16-byte CID as a 32-character lowercase hex string.
pub fn format_cid(cid: &Steinberg::TUID) -> String {
    cid.iter()
        .map(|byte| format!("{:02x}", *byte as u8))
        .collect()
}

/// Parses a 32-character hex string back into a 16-byte CID.
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
// Plugin scanning
// ---------------------------------------------------------------------------

/// Platform-specific default VST3 plugin search directories.
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

/// All search directories: `GLITCHERZ_VST3_PATH` first, then the platform
/// default directories.
pub fn vst3_search_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(extra) = std::env::var_os(VST3_PATH_ENV) {
        dirs.extend(std::env::split_paths(&extra));
    }
    dirs.extend(standard_vst3_dirs());
    dirs
}

/// Scans `.vst3` modules under all search directories (single file or
/// bundle directory, non-recursive, per VST3-spec convention).
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

/// Description of an instantiable plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct Vst3PluginInfo {
    /// Module path (single file or bundle directory).
    pub path: PathBuf,
    /// Component class CID (32-character lowercase hex, parseable via
    /// [`parse_cid`]).
    pub cid: String,
    /// Display name.
    pub name: String,
    /// Whether it is an instrument (subCategories contains `Instrument`).
    pub is_instrument: bool,
}

/// Reads a C-style NUL-terminated string field (`[char8; N]`).
fn c_field_text(field: &[Steinberg::char8]) -> String {
    // SAFETY: The field is guaranteed NUL-terminated (SDK convention); read
    // truncated to within the field length.
    unsafe { CStr::from_ptr(field.as_ptr() as *const c_char) }
        .to_string_lossy()
        .into_owned()
}

/// Reads one factory class entry (PClassInfo2 → PClassInfo fallback).
///
/// Returns `None` for classes that are **not audio components**: per the
/// VST3 spec the component's category is `Audio Module Class`, while the
/// edit controller registers as `Component Controller Class` (frequently
/// under the same display name as the component — without this filter every
/// plugin appears twice in the scan list). Controllers are located via
/// `getControllerClassId`, not by enumeration.
fn fallback_class_info(
    factory: &ComPtr<Steinberg::IPluginFactory>,
    factory2: Option<&ComPtr<Steinberg::IPluginFactory2>>,
    index: i32,
) -> Option<(String, String, bool)> {
    if let Some(f2) = factory2 {
        let mut info2: Steinberg::PClassInfo2 = unsafe { std::mem::zeroed() };
        if tresult_ok(unsafe { f2.getClassInfo2(index, &mut info2) }) {
            if c_field_text(&info2.category) != "Audio Module Class" {
                return None;
            }
            return Some((
                format_cid(&info2.cid),
                c_field_text(&info2.name),
                c_field_text(&info2.subCategories).contains("Instrument"),
            ));
        }
    }
    // SAFETY: factory reference is valid; info is filled in by the SDK.
    unsafe {
        let mut info: Steinberg::PClassInfo = std::mem::zeroed();
        if tresult_ok(factory.getClassInfo(index, &mut info)) {
            if c_field_text(&info.category) != "Audio Module Class" {
                return None;
            }
            // v1 class info carries no sub-category information; guess from
            // the category name.
            return Some((
                format_cid(&info.cid),
                c_field_text(&info.name),
                c_field_text(&info.category).contains("Instrument"),
            ));
        }
    }
    None
}

/// Enumerates all plugin classes within the given VST3 module. Modules that
/// fail to load are skipped and logged.
pub fn list_vst3_plugins_in(path: &Path) -> Vec<Vst3PluginInfo> {
    let module = match raw::load_module(path) {
        Ok(module) => module,
        Err(err) => {
            log::warn!("Skipping unloadable VST3 module {}: {err}", path.display());
            return Vec::new();
        }
    };
    let factory = &module.factory;
    // SAFETY: The factory pointer is valid (module loaded successfully);
    // enumerate the class count per spec.
    let count = unsafe { factory.countClasses() };
    // PClassInfo2 lives on the extended interface (COM queryInterface).
    let factory2 = factory.cast::<Steinberg::IPluginFactory2>();
    let mut infos = Vec::new();
    for index in 0..count {
        let Some((cid, name, is_instrument)) =
            fallback_class_info(factory, factory2.as_ref(), index)
        else {
            continue;
        };
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

/// Scans and enumerates all plugins in all directories.
pub fn list_vst3_plugins() -> Vec<Vst3PluginInfo> {
    let mut infos = Vec::new();
    for path in scan_vst3_files() {
        infos.extend(list_vst3_plugins_in(&path));
    }
    infos
}

// ---------------------------------------------------------------------------
// Instantiation
// ---------------------------------------------------------------------------

/// Static configuration used at instantiation time.
pub struct Vst3InstanceConfig {
    /// Sample rate (Hz).
    pub sample_rate: u32,
    /// Maximum block size (should match [`FIXED_BLOCK_SIZE`]).
    pub max_frames: u32,
}

/// Guard holding the plugin module and all COM objects. **Must live as
/// long as the [`Vst3Node`] and be dropped after it**: dropping the guard
/// first would unload the plugin dynamic library and leave the node's
/// processing code dangling (the same recycling-queue pairing convention
/// as `ClapInstanceGuard`).
///
/// **Field order is drop order**: the module comes last — plugin COM
/// object references are released first, then the dynamic library is
/// unloaded.
pub struct Vst3InstanceGuard {
    component_connection: Option<ComPtr<Steinberg::Vst::IConnectionPoint>>,
    controller_connection: Option<ComPtr<Steinberg::Vst::IConnectionPoint>>,
    controller: ComPtr<Steinberg::Vst::IEditController>,
    component: ComPtr<Steinberg::Vst::IComponent>,
    /// Handoff gate shared with the realtime node (the CLAP mirror; see
    /// `glitcherz_engine::host_gate::StateGate`).
    state_gate: Arc<StateGate>,
    /// Host-side handler held by the plugin (the controller keeps this
    /// pointer long-term per spec; it must share the lifetime).
    // Field exists only to extend the lifetime (the pointer was handed to
    // the controller at spawn time).
    #[allow(dead_code)]
    handler_ptr: ComPtr<Steinberg::Vst::IComponentHandler>,
    _handler: ComWrapper<NoopComponentHandler>,
    /// Module (dynamic library + factory). **Last field**: dropped last —
    /// all plugin COM object references are released first, then the
    /// dynamic library is unloaded.
    // Field exists only to extend the lifetime (the factory/library handle
    // is implicitly used by the COM references).
    #[allow(dead_code)]
    module: raw::Vst3Module,
}

impl Vst3InstanceGuard {
    /// Canonical teardown: disconnect → setActive(false) → terminate().
    /// Reused by both the normal Drop path and instantiation failure paths.
    fn tear_down(&mut self) {
        // SAFETY: The component/controller objects are all alive (the
        // dynamic library is unloaded last; see field order); the calls
        // follow the VST3 teardown order, and failures are simply ignored.
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

    /// Enumerates all controller parameters (control-side call, not
    /// RT-safe).
    ///
    /// Used by the UI to show the parameter list and for later automation
    /// parameter selection; values use normalized 0..1 semantics.
    pub fn parameters(&self) -> Vec<Vst3ParamInfo> {
        // SAFETY: controller is alive (held by the guard); info is filled
        // in by the plugin per SDK convention (title is a NUL-terminated
        // UTF-16 string); each entry is converted to a Rust string
        // immediately after reading.
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

    /// Saves the plugin's full state blob (the component's `getState`
    /// stream, control-side call; the plugin may remain active — the
    /// simple-host posture, same class as the CLAP mirror). The blob format
    /// is plugin-private per the VST3 spec.
    ///
    /// Serialized against the audio thread via the state gate (the CLAP
    /// mirror: a racing save would tear the plugin's engine state the same
    /// way a load does). Returns [`HostError::StateSave`] when the plugin
    /// refuses (`kNotImplemented` = the plugin implements no state
    /// persistence).
    pub fn save_state(&self) -> Result<Vec<u8>, HostError> {
        self.state_gate.begin_host_call();
        let result = self.save_state_impl();
        self.state_gate.end_host_call();
        result
    }

    fn save_state_impl(&self) -> Result<Vec<u8>, HostError> {
        let stream = ComWrapper::new(HostMemoryStream::new());
        // SAFETY: the component is alive (held by the guard); the stream
        // object outlives the call (same scope).
        let result = unsafe {
            let ibstream = stream
                .as_com_ref::<Steinberg::IBStream>()
                .expect("HostMemoryStream always implements IBStream");
            self.component.getState(ibstream.as_ptr())
        };
        if !tresult_ok(result) {
            return Err(HostError::StateSave(result));
        }
        Ok(stream.take_bytes())
    }

    /// Restores the plugin state from a previously saved blob (control-side
    /// call, after instantiation): component `setState` first (the
    /// authoritative state), then the spec-mandated controller
    /// `setComponentState` sync so the controller's parameter read-back
    /// matches. Each consumer gets a fresh stream over the same blob (the
    /// stream is consumed sequentially; no rewind needed). A refusing
    /// controller is logged and tolerated (the VST3 spec allows state-less
    /// controllers); a refusing component fails the call with
    /// [`HostError::StateLoad`].
    pub fn load_state(&self, data: &[u8]) -> Result<(), HostError> {
        self.state_gate.begin_host_call();
        let result = self.load_state_impl(data);
        self.state_gate.end_host_call();
        result
    }

    fn load_state_impl(&self, data: &[u8]) -> Result<(), HostError> {
        // Component pass: the authoritative state.
        let component_stream = ComWrapper::new(HostMemoryStream::from_bytes(data.to_vec()));
        // SAFETY: the component is alive (held by the guard); the stream
        // outlives the call (same scope).
        let result = unsafe {
            let ibstream = component_stream
                .as_com_ref::<Steinberg::IBStream>()
                .expect("HostMemoryStream always implements IBStream");
            self.component.setState(ibstream.as_ptr())
        };
        if !tresult_ok(result) {
            return Err(HostError::StateLoad(result));
        }
        // Controller pass: a fresh stream over the same blob.
        let controller_stream = ComWrapper::new(HostMemoryStream::from_bytes(data.to_vec()));
        // SAFETY: the controller is alive (held by the guard); the stream
        // outlives the call (same scope).
        let controller_result = unsafe {
            let ibstream = controller_stream
                .as_com_ref::<Steinberg::IBStream>()
                .expect("HostMemoryStream always implements IBStream");
            self.controller.setComponentState(ibstream.as_ptr())
        };
        if !tresult_ok(controller_result) {
            log::warn!(
                "VST3 controller rejected setComponentState (0x{controller_result:08x}); parameter read-back may be stale"
            );
        }
        Ok(())
    }

    /// Reads the current normalized value (0..1) of every parameter the
    /// controller exposes (control side, `getParamNormalized`). Used after
    /// a state load to refresh UI-side caches (the CLAP D6 mirror; the
    /// VST3 parameter panel lands with the normalization work). Serialized
    /// against the audio thread via the state gate (same tear risk).
    pub fn read_param_values(&self) -> std::collections::HashMap<u32, f64> {
        self.state_gate.begin_host_call();
        // SAFETY: the controller is alive (held by the guard); control-side
        // call, not RT-safe.
        let values = unsafe {
            self.parameters()
                .into_iter()
                .map(|info| (info.id, self.controller.getParamNormalized(info.id)))
                .collect()
        };
        self.state_gate.end_host_call();
        values
    }
}

/// Plugin parameter description (control-side enumeration; see
/// [`Vst3InstanceGuard::parameters`]).
#[derive(Debug, Clone)]
pub struct Vst3ParamInfo {
    /// Plugin-side parameter ID (payload of `ParamId::PluginParam`).
    pub id: u32,
    /// Parameter title (converted from UTF-16, may be empty).
    pub title: String,
    /// Normalized default value (0..1).
    pub default_normalized: f64,
    /// Whether it can be automated (VST3 `kCanAutomate` flag).
    pub can_automate: bool,
}

/// NUL-terminated UTF-16 field → Rust string (truncated to within the
/// field length when too long).
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

/// Stashed bridged events.
#[derive(Debug, Clone, Copy)]
pub enum PendingVst3Event {
    /// Note pressed.
    NoteOn { note: u8, velocity: f32 },
    /// Note released.
    NoteOff { note: u8 },
    /// Plugin parameter value (`ParamId::PluginParam` bridge, VST3
    /// normalized 0..1).
    Param { id: u32, value: f32 },
}

/// VST3 instrument node that can run inside the audio callback.
///
/// Constructed on the non-realtime thread ([`Vst3Instance::spawn_rt`]) and
/// then moved into the audio thread. `process` / `note_event` / `set_param`
/// all follow the real-time iron rules: no allocation (events and buffers
/// are all preallocated at construction), no locks, no panics.
pub struct Vst3Node {
    /// Host event list (owned + stable heap address; the raw pointer stays
    /// valid for the object's lifetime).
    event_list: ComWrapper<HostEventList>,
    /// Host parameter changes container.
    param_changes: ComWrapper<HostParamChanges>,
    /// Preallocated parameter queues (order matches the slots inside
    /// `HostParamChanges`).
    _param_queues: Vec<ComWrapper<HostParamValueQueue>>,
    /// Raw interface pointers handed to the plugin (non-owning; lifetime =
    /// the objects above).
    input_events: *mut Steinberg::Vst::IEventList,
    input_param_changes: *mut Vst::IParameterChanges,
    /// Plugin audio processor (interface view of the same COM object as
    /// component).
    audio_processor: ComPtr<Steinberg::Vst::IAudioProcessor>,
    /// Whether the plugin declares audio input buses (effects true /
    /// instruments false).
    has_audio_input: bool,
    /// Input path scratch buffers (used by effects only; preallocated at
    /// construction).
    in_l: Box<[f32]>,
    in_r: Box<[f32]>,
    out_l: Box<[f32]>,
    out_r: Box<[f32]>,
    pending: [PendingVst3Event; MAX_PENDING_EVENTS],
    pending_count: usize,
    /// Handoff gate shared with the control-side guard (the CLAP mirror; see
    /// `glitcherz_engine::host_gate::StateGate`).
    state_gate: Arc<StateGate>,
}

// SAFETY: The node is moved wholesale between threads (constructed on the
// control thread → used on the audio thread); the internal raw pointers all
// point to stable Arc heap allocations owned by the node itself and move
// with it; COM calls happen only inside the audio-thread process. The
// official VST3 interface objects are marked Send.
unsafe impl Send for Vst3Node {}

impl Drop for Vst3Node {
    fn drop(&mut self) {
        // SAFETY: The node is dropped before the guard (recycling-queue
        // convention), so the plugin code is still loaded.
        unsafe {
            let _ = self.audio_processor.setProcessing(0);
        }
    }
}

/// One-shot instantiation result: control-side guard + realtime node.
///
/// **Field order is drop order**: `node` is dropped first
/// (`setProcessing(false)` must happen before the dynamic library is
/// unloaded), `guard` afterwards. When storing `guard`/`node` separately
/// (the engine hot-swap path), this order must be preserved — the guard
/// must not be dropped before the node.
pub struct Vst3Instance {
    /// Realtime-side node (moved into the audio thread).
    pub node: Vst3Node,
    /// Control-side liveness guard (held by the UI/engine thread).
    pub guard: Vst3InstanceGuard,
}

impl Vst3Instance {
    /// Loads and activates the plugin, producing the realtime node. **Must
    /// be called on the non-realtime thread**
    /// (involves dynamic library loading, plugin-internal allocation, and
    /// COM lifetime management).
    pub fn spawn_rt(
        path: &Path,
        class_id: &str,
        config: Vst3InstanceConfig,
    ) -> Result<Self, HostError> {
        let module = raw::load_module(path)?;
        let cid = parse_cid(class_id).ok_or_else(|| HostError::BadCid(class_id.to_owned()))?;
        let factory = &module.factory;

        // 1. Create the component and controller (VST3 dual-object model).
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

        // 2. Host handler (empty implementation) + connection point
        //    interconnect.
        let handler = ComWrapper::new(NoopComponentHandler);
        let handler_ptr = handler
            .to_com_ptr::<Steinberg::Vst::IComponentHandler>()
            .expect("NoopComponentHandler always implements IComponentHandler");
        let component_connection = component.cast::<Steinberg::Vst::IConnectionPoint>();
        let controller_connection = controller.cast::<Steinberg::Vst::IConnectionPoint>();
        if let (Some(component_cp), Some(controller_cp)) =
            (&component_connection, &controller_connection)
        {
            // SAFETY: Both connection point objects are alive;
            // interconnection is a symmetric, spec-mandated call.
            unsafe {
                let _ = component_cp.connect(controller_cp.as_ptr().cast());
                let _ = controller_cp.connect(component_cp.as_ptr().cast());
            }
        }

        // 3. Initialization (context = IHostApplication*; the simple host
        //    passes null).
        let init_result = unsafe {
            component.initialize(std::ptr::null_mut())
                | controller.initialize(std::ptr::null_mut())
                | controller.setComponentHandler(handler_ptr.as_ptr().cast())
        };
        if !tresult_ok(init_result) {
            log::warn!(
                "VST3 initialization returned 0x{init_result:08x}; continuing as a simple instrument"
            );
        }

        // 4. Bus enumeration and activation (activates all audio
        //    output/event input buses).
        activate_buses(&component);
        let audio_processor = component
            .cast::<Steinberg::Vst::IAudioProcessor>()
            .ok_or(HostError::NoAudioProcessor)?;
        // SAFETY: setup/setActive are called in spec order before activation.
        unsafe {
            let mut setup = Vst::ProcessSetup {
                processMode: Vst::ProcessModes_::kRealtime,
                symbolicSampleSize: Vst::SymbolicSampleSizes_::kSample32,
                maxSamplesPerBlock: config.max_frames.max(FIXED_BLOCK_SIZE as u32) as i32,
                sampleRate: f64::from(config.sample_rate),
            };
            if !tresult_ok(audio_processor.setupProcessing(&mut setup)) {
                log::warn!(
                    "VST3 plugin rejected ProcessSetup; continuing with the default configuration"
                );
            }
            let _ = component.setActive(1);
            if !tresult_ok(audio_processor.setProcessing(1)) {
                log::warn!("VST3 plugin rejected setProcessing(true); output will be silent");
            }
        }

        // 5. Construct the realtime node (all preallocation happens here).
        let state_gate = Arc::new(StateGate::default());
        // Whether an audio input bus exists (true for effects; false for
        // instruments) — the query must complete before component is moved
        // into the guard.
        let has_audio_input = unsafe {
            component.getBusCount(Vst::MediaTypes_::kAudio, Vst::BusDirections_::kInput) > 0
        };
        let frames = config.max_frames.max(FIXED_BLOCK_SIZE as u32) as usize;
        let event_list = ComWrapper::new(HostEventList::new());
        let input_events = event_list
            .as_com_ref::<Steinberg::Vst::IEventList>()
            .expect("HostEventList always implements IEventList")
            .as_ptr();
        let mut queues = Vec::with_capacity(PARAM_QUEUE_COUNT);
        let mut slots = Vec::with_capacity(PARAM_QUEUE_COUNT);
        for _ in 0..PARAM_QUEUE_COUNT {
            let queue = ComWrapper::new(HostParamValueQueue::new());
            slots.push(QueueSlot {
                // Shared reference to raw pointer: the COM interface object
                // remains valid for the wrapper's lifetime (the node holds
                // ownership); no unsafe needed — a raw pointer by itself is
                // just data.
                data: &*queue as *const HostParamValueQueue as *mut HostParamValueQueue,
                com: queue
                    .as_com_ref::<Vst::IParamValueQueue>()
                    .expect("HostParamValueQueue always implements IParamValueQueue")
                    .as_ptr(),
            });
            queues.push(queue);
        }
        let param_changes = ComWrapper::new(HostParamChanges::new(slots));
        let input_param_changes = param_changes
            .as_com_ref::<Vst::IParameterChanges>()
            .expect("HostParamChanges always implements IParameterChanges")
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
            state_gate: Arc::clone(&state_gate),
        };

        // Instantiation success path: objects are handed to the guard; the
        // failure path is covered by tear_down.
        // (All steps here can no longer fail; assemble directly.)
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
                state_gate,
            },
        })
    }
}

/// Creates an instance of the given class and interface via the factory
/// (reference count +1, owned by the caller).
fn create_instance<I: Interface>(
    factory: &ComPtr<Steinberg::IPluginFactory>,
    cid: &Steinberg::TUID,
) -> Result<ComPtr<I>, Steinberg::tresult> {
    // SAFETY: createInstance returns an interface pointer with a +1
    // reference count per the VST3 spec, taken over by ComPtr RAII; the
    // iid is a 16-byte GUID (in the createInstance context, FIDString is
    // just a byte pointer to the binary CID, consistent with the SDK).
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

/// Activates all audio input/output and event input buses of the component.
fn activate_buses(component: &ComPtr<Steinberg::Vst::IComponent>) {
    // SAFETY: The component is already initialized; bus information is
    // enumerated per spec.
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
            return; // Drop when full (non-critical path)
        }
        self.pending[self.pending_count] = event;
        self.pending_count += 1;
    }
}

/// Constructs a NoteOn event (bus 0, sample offset 0).
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

/// Constructs a NoteOff event.
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
        // 0. State-gate handoff (the CLAP mirror): while the control thread
        //    runs a main-thread plugin call, park — acknowledge and keep the
        //    output zeroed (atomics only, no locks).
        if self.state_gate.audio_should_park() {
            let frames = ctx.frames.min(self.out_l.len()).min(buffer.capacity());
            let out_l = &mut self.out_l[..frames];
            let out_r = &mut self.out_r[..frames];
            out_l.fill(0.0);
            out_r.fill(0.0);
            for (channel_index, source) in [&self.out_l[..frames], &self.out_r[..frames]]
                .into_iter()
                .enumerate()
            {
                if let Some(destination) = buffer.channel_mut(channel_index) {
                    for (index, sample) in destination.iter_mut().enumerate() {
                        *sample = source.get(index).copied().unwrap_or(0.0);
                    }
                }
            }
            return;
        }
        let frames = ctx.frames.min(self.out_l.len()).min(buffer.capacity());

        // 1. Stashed events → host COM objects (block granularity).
        //    ComWrapper exposes &HostEventList / &HostParamChanges via
        //    Deref; mutable internal state is carried by Cell/UnsafeCell
        //    (serialized within the audio thread; see the host_impl
        //    documentation).
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
                    // VST3 normalized semantics: the value is committed
                    // directly as 0..1.
                    self.param_changes.push(id, f64::from(value));
                }
            }
        }
        self.pending_count = 0;

        // 2. Audio buffers + processing context + ProcessData (all on the
        //    stack).
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

        // Effects: copy the engine buffer contents into separate input
        // buffers (output is written to separate buffers and copied back
        // wholesale at the end, avoiding in-place read/write aliasing
        // inside the plugin).
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
            // Engine buffer → input path buffers (missing channels are
            // zero-filled).
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
            // SAFETY/NOTE: writes to input_channel_ptrs reach the plugin
            // for reading via the channelBuffers32 pointers in
            // input_buses; the compiler cannot see that read across FFI,
            // hence unused_assignments is allowed (false positive).
            #[allow(unused_assignments)]
            {
                input_channel_ptrs[0] = self.in_l[..frames].as_mut_ptr();
                input_channel_ptrs[1] = self.in_r[..frames].as_mut_ptr();
            }
        }

        let mut vst_context: Vst::ProcessContext = unsafe { std::mem::zeroed() };
        vst_context.sampleRate = f64::from(ctx.sample_rate);
        vst_context.projectTimeSamples = i64::try_from(ctx.playback_sample_pos).unwrap_or(i64::MAX);
        // state = 0: declare no valid bits (the simple host provides no
        // transport info).
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

        // SAFETY: The plugin interface is valid for the guard's lifetime
        // (recycling-queue convention); the output buffers are zeroed, so
        // a plugin failure path naturally produces silence.
        let result = unsafe { self.audio_processor.process(&mut data) };
        if !tresult_ok(result) {
            // Output stays zeroed (no panic); report only.
            log::trace!("VST3 process returned 0x{result:08x}");
        }

        // 3. Copy into the engine's planar buffers (defensive out-of-bounds
        //    clamping, no panic).
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

    fn note_event(&mut self, event: glitcherz_engine::realtime::NoteEvent) -> bool {
        match event {
            glitcherz_engine::realtime::NoteEvent::On { note, velocity } => {
                self.stash(PendingVst3Event::NoteOn { note, velocity });
            }
            glitcherz_engine::realtime::NoteEvent::Off { note } => {
                self.stash(PendingVst3Event::NoteOff { note });
            }
        }
        true
    }
}

// This crate does not export GetPluginFactory (that is a plugin-side
// export symbol).

#[cfg(test)]
mod tests {
    use super::{
        Vst3Instance, Vst3InstanceConfig, Vst3PluginInfo, list_vst3_plugins_in, parse_cid,
    };
    use glitcherz_engine::realtime::NoteEvent;
    use glitcherz_engine::realtime::buffer::AudioBuffer;
    use glitcherz_engine::realtime::commands::ParamId;
    use glitcherz_engine::realtime::context::ProcessContext;
    use glitcherz_engine::realtime::processor::Processor;

    use std::path::PathBuf;

    /// Dynamic library path of the test plugin (`cargo test --workspace`
    /// builds the cdylib first).
    fn test_plugin_path() -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug")
            .join(format!(
                "{}glitcherz_test_vst3{}",
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
            eprintln!("skipped: VST3 test plugin cdylib not built (run cargo test --workspace)");
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
        .expect("failed to instantiate the test plugin");
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
        let cid = parse_cid(text).expect("valid CID");
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

        // No note triggered → silence.
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
            "should be silent with no note"
        );

        // NoteOn(A4) → sine output, both channels identical.
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
        assert!(
            peak_l > 0.1,
            "should have audible output after NoteOn, peak {peak_l}"
        );
        // Both channels come from the same mono voice; content should
        // match.
        let same = buffer
            .channel(1)
            .unwrap()
            .iter()
            .zip(buffer.channel(0).unwrap())
            .all(|(r, l)| (r - l).abs() < 1e-9);
        assert!(same, "stereo channels should be identical (mono source)");

        // NoteOff → silence (instrument has no release tail).
        assert!(node.note_event(NoteEvent::Off { note: 69 }));
        node.process(&ctx, &mut buffer);
        assert!(
            buffer
                .channel(0)
                .unwrap()
                .iter()
                .all(|sample| sample.abs() < 1e-9),
            "should be silent after NoteOff"
        );
    }

    #[test]
    fn parameter_enumeration_reports_gain() {
        let Some((instance, _ctx)) = spawn_test_plugin() else {
            return;
        };
        let params = instance.guard.parameters();
        assert_eq!(params.len(), 1, "test plugin should expose 1 parameter");
        let gain = &params[0];
        assert_eq!(gain.id, 1);
        assert_eq!(gain.title, "Gain");
        assert!((gain.default_normalized - 0.5).abs() < 1e-9);
        assert!(gain.can_automate, "Gain should be marked as automatable");
    }

    #[test]
    fn state_roundtrip_restores_gain() {
        let Some((mut instance, ctx)) = spawn_test_plugin() else {
            return;
        };
        let mut buffer = AudioBuffer::new(2, 128);
        buffer.set_len(128);
        buffer.clear();

        // Move the gain away from its default via the param bridge (one
        // block flushes the stashed event into the plugin's parameter
        // queue).
        assert!(instance.node.set_param(ParamId::PluginParam(1), 0.25));
        instance.node.process(&ctx, &mut buffer);

        let blob = instance.guard.save_state().expect("state save should work");
        assert_eq!(blob.len(), 8, "the test plugin saves 8 bytes (f64 LE bits)");

        // Fresh instance (per-instance factory): the component must report
        // the console default.
        let Some((fresh, _)) = spawn_test_plugin() else {
            return;
        };
        let default_blob = fresh.guard.save_state().expect("save on a fresh instance");
        assert_ne!(
            default_blob, blob,
            "fresh instance must carry the default state"
        );

        // Restore, then verify both channels: the component state blob
        // matches the saved one, and the controller's parameter read-back
        // is in sync (the setComponentState mirror).
        fresh
            .guard
            .load_state(&blob)
            .expect("state load should work");
        let restored_blob = fresh.guard.save_state().expect("save after load");
        assert_eq!(
            restored_blob, blob,
            "loaded state must equal the saved blob"
        );
        assert_eq!(
            fresh.guard.read_param_values().get(&1),
            Some(&0.25),
            "controller read-back should reflect the loaded state"
        );
    }

    #[test]
    fn end_to_end_gain_effect_processes_input() {
        // Effect class ("GLITCHERZSINE" + 0x03).
        let Some((mut instance, ctx)) = spawn_test_class("5245534f5a53494e4500000000000003") else {
            return;
        };
        let node = &mut instance.node;

        // Input 0.5 → output should be 0.25 (fixed gain ×0.5).
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
                    "gain effect should output 0.25, got {sample} (channel {channel_index} sample {index})"
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

        // Plugin parameters are accepted and queued (even if the plugin
        // has no parameters yet, the event path works).
        assert!(node.set_param(ParamId::PluginParam(1), 0.5));
        // Built-in parameters are not consumed by Vst3Node (handled by
        // built-in nodes).
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
            "parameter events must not break rendering"
        );
    }

    #[test]
    fn list_plugins_reports_our_sine() {
        let Some(path) = test_plugin_path() else {
            eprintln!("skipped: VST3 test plugin cdylib not built");
            return;
        };
        // Scan matches by .vst3 extension: copy the cdylib as a .vst3
        // single-file module.
        let temp = std::env::temp_dir().join("glitcherz_test_vst3_scan.vst3");
        std::fs::copy(&path, &temp).expect("copy failed");
        let infos: Vec<Vst3PluginInfo> = list_vst3_plugins_in(&temp);
        let _ = std::fs::remove_file(&temp);
        let sine = infos
            .iter()
            .find(|info| info.cid == "5245534f5a53494e4500000000000001")
            .expect("test plugin should be enumerated");
        assert!(
            sine.is_instrument,
            "sine plugin should be marked as instrument"
        );
        assert_eq!(sine.name, "GlitcherZ Sine VST3");
    }

    #[test]
    fn scanner_filters_out_controller_classes() {
        // The fixture factory enumerates the edit controller under the
        // "Component Controller Class" category (as real plugins do) — the
        // scan list must keep audio components only, or every plugin would
        // show up twice (component + same-named controller).
        let Some(path) = test_plugin_path() else {
            eprintln!("skipped: VST3 test plugin cdylib not built");
            return;
        };
        let temp = std::env::temp_dir().join("glitcherz_test_vst3_scan_ctrl.vst3");
        std::fs::copy(&path, &temp).expect("copy failed");
        let infos: Vec<Vst3PluginInfo> = list_vst3_plugins_in(&temp);
        let _ = std::fs::remove_file(&temp);
        assert_eq!(
            infos.len(),
            2,
            "only the two Audio Module Classes should be listed, got {infos:?}"
        );
        assert!(
            infos
                .iter()
                .all(|info| info.cid != "5245534f5a53494e4500000000000002"),
            "the controller CID must not leak into the scan list"
        );
    }

    #[test]
    fn invalid_plugin_fails_gracefully() {
        let bogus = std::env::temp_dir().join("glitcherz_bogus.vst3");
        std::fs::write(&bogus, b"not a dll").expect("write failed");
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
