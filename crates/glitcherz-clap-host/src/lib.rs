//! # glitcherz-clap-host
//!
//! CLAP plugin host adaptation layer (non-real-time infrastructure): the
//! bridge between the GlitcherZ engine and the CLAP plugin ecosystem.
//!
//! Responsibilities:
//! - Scans CLAP plugin directories (standard directories + the
//!   `GLITCHERZ_CLAP_PATH` environment variable);
//! - Enumerates plugin descriptions (ID / name / whether it is an
//!   instrument);
//! - Instantiates plugins on a **non-real-time thread**, producing a
//!   [`ClapNode`] that can run in the audio callback (the real-time iron
//!   rules apply to its `process` path);
//! - Enumerates the parameter list of a plugin instance
//!   ([`ClapInstance::list_params`], direct calls via the params extension,
//!   see the `raw` module);
//! - Saves/restores the full plugin state blob via the state extension
//!   ([`ClapInstanceGuard::save_state`] / [`ClapInstanceGuard::load_state`])
//!   for project persistence.
//!
//! Bridging model:
//! - **Notes**: [`Processor::note_event`] stashes `NoteOn/Off` into a
//!   fixed-capacity event slot; the next `process` emits them as CLAP input
//!   events (block granularity);
//! - **Parameters**: `ParamId::PluginParam(u32)` likewise emits a
//!   `ParamValueEvent` — plugin parameters written by the automation
//!   evaluator are bridged naturally this way;
//! - **Audio**: instrument plugins (no input ports) render into the node's
//!   pre-allocated stereo scratch buffer, which is then copied into the
//!   [`AudioBuffer`].
//!
//! The plugin dynamic library's lifetime is managed by
//! [`ClapInstanceGuard`], which must live as long as the [`ClapNode`] (see
//! [`ClapInstance::spawn_rt`]).

#![deny(unsafe_code)]
// unsafe exists only in the raw module (dynamic library loading boundary);
// rationale and verification are in its docs.

pub mod raw;

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use clack_common::events::event_types::{MidiEvent, NoteOffEvent, NoteOnEvent, ParamValueEvent};
pub use clack_extensions::gui::GuiSize;
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiError, HostGui, HostGuiImpl, PluginGui,
};
use clack_extensions::log::{HostLog, HostLogImpl};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamRescanFlags,
};
use clack_extensions::state::{HostState, HostStateImpl, PluginState, StateError};
use clack_host::entry::PluginEntryError;
use clack_host::prelude::*;
use clack_host::process::audio_buffers::InputChannel;

use glitcherz_engine::realtime::FIXED_BLOCK_SIZE;
use glitcherz_engine::realtime::buffer::AudioBuffer;
use glitcherz_engine::realtime::commands::ParamId;
use glitcherz_engine::realtime::context::ProcessContext;
use glitcherz_engine::realtime::processor::Processor;

/// Maximum number of bridging events queued per `process` (notes +
/// parameters combined).
pub const MAX_PENDING_EVENTS: usize = 32;

/// Name of the environment variable holding the plugin scan directories
/// (`PATH`-style separated list).
pub const CLAP_PATH_ENV: &str = "GLITCHERZ_CLAP_PATH";

/// Empty input port buffer type for instrument plugins (no audio input).
type NoInputPorts = [AudioPortBuffer<
    std::iter::Empty<InputChannel<'static, f32>>,
    std::iter::Empty<InputChannel<'static, f64>>,
>; 0];

/// Host-side error.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("failed to load plugin dynamic library: {0}")]
    Load(#[source] PluginEntryError),
    #[error("failed to instantiate plugin: {0}")]
    Instance(#[source] PluginInstanceError),
    #[error("failed to activate plugin: {0}")]
    Activate(#[source] PluginInstanceError),
    #[error("failed to start plugin processing: {0}")]
    Start(#[source] PluginInstanceError),
    #[error("failed to construct host info")]
    HostInfo,
    #[error("plugin lacks a factory or a valid ID")]
    NoPlugin,
    #[error("plugin does not implement the params extension")]
    NoParams,
    #[error("plugin does not implement the state extension")]
    NoState,
    #[error("failed to save plugin state: {0}")]
    StateSave(#[source] StateError),
    #[error("failed to load plugin state: {0}")]
    StateLoad(#[source] StateError),
    #[error("plugin does not implement the gui extension")]
    NoGui,
    #[error("plugin GUI does not support win32 embedding")]
    GuiUnsupportedApi,
    #[error("plugin GUI reported no size")]
    GuiNoSize,
    #[error("plugin GUI is not open")]
    GuiNotOpen,
    #[error("plugin GUI operation failed: {0}")]
    Gui(#[source] GuiError),
}

// ---------------------------------------------------------------------------
// Plugin scanning
// ---------------------------------------------------------------------------

/// Platform-specific standard CLAP plugin search directories.
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

/// All search directories: `GLITCHERZ_CLAP_PATH` first, then the platform
/// standard directories.
pub fn clap_search_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(extra) = std::env::var_os(CLAP_PATH_ENV) {
        dirs.extend(std::env::split_paths(&extra));
    }
    dirs.extend(standard_clap_dirs());
    dirs
}

/// Scans all search directories for `.clap` dynamic library files
/// (non-recursive, following CLAP specification convention).
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

/// Description of an instantiable plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct ClapPluginInfo {
    /// Dynamic library path.
    pub path: PathBuf,
    /// Unique plugin ID (required by the CLAP spec to be stable).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Whether it is an instrument (synthesizer; `instrument` feature tag).
    pub is_instrument: bool,
}

/// Metadata snapshot of one plugin parameter (read via the params extension
/// at instantiation time).
#[derive(Debug, Clone, PartialEq)]
pub struct ClapParamInfo {
    /// Plugin-side parameter ID (for the `ParamId::PluginParam` bridge).
    pub id: u32,
    /// Display name.
    pub name: String,
    /// Module path (`/`-separated, may be empty).
    pub module: String,
    /// Minimum value (plugin value range).
    pub min_value: f64,
    /// Maximum value (plugin value range).
    pub max_value: f64,
    /// Default value (plugin value range).
    pub default_value: f64,
}

/// Enumerates all plugins in the given `.clap` dynamic library. Libraries
/// that fail to load are skipped and logged.
pub fn list_clap_plugins_in(path: &Path) -> Vec<ClapPluginInfo> {
    let entry = match raw::load_entry(path) {
        Ok(entry) => entry,
        Err(err) => {
            log::warn!("skipping unloadable CLAP plugin {}: {err}", path.display());
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

/// Scans and enumerates all plugins in all directories.
pub fn list_clap_plugins() -> Vec<ClapPluginInfo> {
    let mut infos = Vec::new();
    for path in scan_clap_files() {
        infos.extend(list_clap_plugins_in(&path));
    }
    infos
}

// ---------------------------------------------------------------------------
// Host callback implementations
// ---------------------------------------------------------------------------

/// Host-side facts surfaced by gui extension callbacks. Written from plugin
/// main-thread calls, polled by the UI frame loop (single-word atomics
/// only; the audio thread is never involved).
#[derive(Debug, Default)]
pub struct GuiHostState {
    /// Packed [`GuiSize`] of the latest `request_resize` (0 = none pending).
    resize_request: AtomicU64,
    show_request: AtomicBool,
    hide_request: AtomicBool,
    resize_hints_changed: AtomicBool,
    /// Sticky: set by `closed(was_destroyed = true)`; cleared when the
    /// guard acknowledges the destruction (see
    /// [`ClapInstanceGuard::close_editor`]).
    closed: AtomicBool,
}

/// Host-side params-extension callback facts. `request_flush` is called by
/// the plugin from its main thread (typically from inside its GUI after a
/// user edit); the flag is polled and taken by the UI frame loop via
/// [`ClapInstanceGuard::poll_param_flush`] (single-word atomics only; the
/// audio thread is never involved).
#[derive(Debug, Default)]
pub struct ParamsHostState {
    flush_requested: AtomicBool,
}

/// One-shot gui events polled from the UI frame loop (take semantics: a
/// flag is reported once per plugin request).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct EditorEvents {
    /// Latest plugin-requested GUI size.
    pub resize: Option<GuiSize>,
    /// The plugin requested its GUI to be shown.
    pub show: bool,
    /// The plugin requested its GUI to be hidden.
    pub hide: bool,
    /// The plugin's resize hints changed (re-query `get_resize_hints`).
    pub resize_hints_changed: bool,
}

struct GlitcherZHostShared {
    /// Captured during plugin initialization: the plugin's state extension,
    /// when implemented (read back after instantiation for save/load).
    state_ext: OnceLock<Option<PluginState>>,
    /// Captured during plugin initialization: the plugin's gui extension,
    /// when implemented (read back after instantiation for open/close).
    gui_ext: OnceLock<Option<PluginGui>>,
    /// Gui callback facts shared with the guard (polled via
    /// [`ClapInstanceGuard::editor_events`]).
    gui: Arc<GuiHostState>,
    /// Params callback facts shared with the guard (polled via
    /// [`ClapInstanceGuard::poll_param_flush`]).
    params: Arc<ParamsHostState>,
}

impl SharedHandler<'_> for GlitcherZHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}

    fn initializing(&self, instance: InitializingPluginHandle<'_>) {
        let _ = self.state_ext.set(instance.get_extension::<PluginState>());
        let _ = self.gui_ext.set(instance.get_extension::<PluginGui>());
    }
}

impl HostLogImpl for GlitcherZHostShared {
    fn log(&self, severity: clack_extensions::log::LogSeverity, message: &str) {
        log::info!(target: "clap_plugin", "[{severity}] {message}");
    }
}

impl HostParamsImplShared for GlitcherZHostShared {
    fn request_flush(&self) {
        // The plugin changed parameters on its main thread (typically from
        // its GUI); the UI frame loop picks the flag up via
        // ClapInstanceGuard::poll_param_flush and reads the changes back.
        self.params.flush_requested.store(true, Ordering::Relaxed);
    }
}

impl HostGuiImpl for GlitcherZHostShared {
    fn resize_hints_changed(&self) {
        self.gui.resize_hints_changed.store(true, Ordering::Relaxed);
    }

    fn request_resize(&self, new_size: GuiSize) -> Result<(), clack_host::host::HostError> {
        self.gui
            .resize_request
            .store(new_size.pack_to_u64(), Ordering::Relaxed);
        Ok(())
    }

    fn request_show(&self) -> Result<(), clack_host::host::HostError> {
        self.gui.show_request.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn request_hide(&self) -> Result<(), clack_host::host::HostError> {
        self.gui.hide_request.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn closed(&self, was_destroyed: bool) {
        // Per the CLAP contract, `was_destroyed == true` means the plugin
        // already destroyed its GUI; the host acknowledges by calling
        // destroy (see ClapInstanceGuard::close_editor).
        if was_destroyed {
            self.gui.closed.store(true, Ordering::Relaxed);
        }
    }
}

struct GlitcherZHostMainThread;

impl MainThreadHandler<'_> for GlitcherZHostMainThread {}

impl HostParamsImplMainThread for GlitcherZHostMainThread {
    fn rescan(&self, _flags: ParamRescanFlags) {}
    fn clear(&self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

impl HostStateImpl for GlitcherZHostMainThread {
    fn mark_dirty(&self) {
        // No dirty tracking: state is captured wholesale at save time
        // (see .agents/spec/plugin-state.md, D3/D5).
    }
}

struct GlitcherZHostHandlers;

impl HostHandlers for GlitcherZHostHandlers {
    type Shared<'a> = GlitcherZHostShared;
    type MainThread<'a> = GlitcherZHostMainThread;
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder.register::<HostLog>();
        builder.register::<HostParams>();
        builder.register::<HostState>();
        builder.register::<HostGui>();
    }
}

// ---------------------------------------------------------------------------
// Real-time node
// ---------------------------------------------------------------------------

/// A stashed bridging event.
#[derive(Debug, Clone, Copy)]
pub enum PendingClapEvent {
    /// Note pressed.
    NoteOn { note: u8, velocity: f32 },
    /// Note released.
    NoteOff { note: u8 },
    /// Plugin parameter value (`ParamId::PluginParam` bridge).
    Param { id: u32, value: f32 },
    /// Raw MIDI 1.0 bytes (U4a: a control-track CC injected into the plugin's
    /// MIDI input).
    Midi { data: [u8; 3] },
}

/// A CLAP instrument node that can run in the audio callback.
///
/// Constructed on a non-real-time thread ([`ClapInstance::spawn_rt`]), then
/// moved into the audio thread. `process` / `note_event` / `set_param` all
/// follow the real-time iron rules: no allocation (events and buffers are
/// all pre-allocated at construction), no locks, no panics.
pub struct ClapNode {
    /// Unified state processor: internally switches between
    /// Started/Stopped; `process` may fail (runtime check) and outputs
    /// silence on failure.
    processor: PluginAudioProcessor<GlitcherZHostHandlers>,
    input_events: EventBuffer,
    output_events: EventBuffer,
    input_ports: AudioPorts,
    output_ports: AudioPorts,
    out_l: Box<[f32]>,
    out_r: Box<[f32]>,
    pending: [PendingClapEvent; MAX_PENDING_EVENTS],
    pending_count: usize,
    /// Handoff gate shared with the control-side guard (see [`StateGate`]).
    state_gate: Arc<StateGate>,
}

/// Guard holding the plugin dynamic library and instance. **Must live as
/// long as the [`ClapNode`]**: dropping the guard unloads the plugin dynamic
/// library, leaving the node's processing pointers dangling.
pub struct ClapInstanceGuard {
    _entry: PluginEntry,
    instance: PluginInstance<GlitcherZHostHandlers>,
    /// The plugin's state extension, when implemented (captured at
    /// initialization; `None` when the plugin has none).
    state_ext: Option<PluginState>,
    /// Handoff gate shared with the real-time node (see
    /// `glitcherz_engine::host_gate::StateGate`).
    state_gate: Arc<StateGate>,
    /// The plugin's gui extension, when implemented (captured at
    /// initialization; `None` when the plugin has none).
    gui_ext: Option<PluginGui>,
    /// Gui callback facts shared with the host handlers (see
    /// [`Self::editor_events`]).
    gui: Arc<GuiHostState>,
    /// Params callback facts shared with the host handlers (see
    /// [`Self::poll_param_flush`]).
    params: Arc<ParamsHostState>,
    /// Whether the host-side GUI lifecycle is open (created, not yet
    /// destroyed).
    gui_open: bool,
}

use glitcherz_engine::host_gate::StateGate;

impl ClapInstanceGuard {
    /// Saves the plugin's full state blob (main thread only; the plugin may
    /// remain active). The blob format is plugin-private per the CLAP spec.
    ///
    /// The call is serialized against the audio thread via the state gate
    /// (a racing save would tear the plugin's engine state the same way a
    /// load does). Returns [`HostError::NoState`] when the plugin does not
    /// implement the state extension.
    pub fn save_state(&mut self) -> Result<Vec<u8>, HostError> {
        self.state_gate.begin_host_call();
        let result = self.save_state_impl();
        self.state_gate.end_host_call();
        result
    }

    fn save_state_impl(&mut self) -> Result<Vec<u8>, HostError> {
        let state = self.state_ext.ok_or(HostError::NoState)?;
        let mut out = Vec::new();
        state
            .save(&self.instance.plugin_handle(), &mut out)
            .map_err(HostError::StateSave)?;
        Ok(out)
    }

    /// Restores the plugin state from a previously saved blob (main thread
    /// only). Call right after instantiation, before the node starts
    /// processing — or rely on the state gate: a live load (while the audio
    /// thread processes) is serialized against `process` so the plugin's
    /// engine state never tears.
    ///
    /// Returns [`HostError::NoState`] when the plugin does not implement the
    /// state extension.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), HostError> {
        self.state_gate.begin_host_call();
        let result = self.load_state_impl(data);
        self.state_gate.end_host_call();
        result
    }

    fn load_state_impl(&mut self, data: &[u8]) -> Result<(), HostError> {
        let state = self.state_ext.ok_or(HostError::NoState)?;
        let mut reader = std::io::Cursor::new(data);
        state
            .load(&self.instance.plugin_handle(), &mut reader)
            .map_err(HostError::StateLoad)
    }

    /// Reads the current value of every parameter the plugin exposes (main
    /// thread; params extension `get_value`). Missing ids mean the plugin
    /// refused the read.
    ///
    /// Used after a state/preset load to refresh the UI's cached slider
    /// values once the instance is held in the guard registry. Serialized
    /// against the audio thread via the state gate (same tear risk).
    pub fn read_param_values(&self) -> std::collections::HashMap<u32, f64> {
        self.state_gate.begin_host_call();
        let ids: Vec<u32> = raw::enumerate_params(self.instance.raw_instance())
            .iter()
            .map(|p| p.id)
            .collect();
        let values = raw::read_param_values(self.instance.raw_instance(), &ids)
            .into_iter()
            .collect();
        self.state_gate.end_host_call();
        values
    }

    /// Opens the plugin's embedded GUI (main thread only). `parent_hwnd`
    /// is the platform parent window handle (Windows: HWND; borrowed for
    /// the call — the plugin attaches its child window synchronously).
    ///
    /// Follows the CLAP embedded-GUI sequence: create → set_scale →
    /// get_size → set_parent → show. On failure after a successful create
    /// the GUI is destroyed again (no half-open state). Reopening without
    /// an intervening close is allowed and closes first.
    ///
    /// Not serialized against the audio thread: gui calls are main-thread
    /// plugin calls under the CLAP threading contract, safe to run while
    /// `process` executes (unlike state save/load, see
    /// `.agents/spec/plugin-gui.md` D7).
    pub fn open_editor(
        &mut self,
        parent_hwnd: *mut c_void,
        scale: f64,
    ) -> Result<GuiSize, HostError> {
        if self.gui_open {
            self.close_editor();
        }
        let Some(gui) = self.gui_ext else {
            return Err(HostError::NoGui);
        };
        let handle = self.instance.plugin_handle();
        let config = GuiConfiguration {
            api_type: GuiApiType::WIN32,
            is_floating: false,
        };
        if !gui.is_api_supported(&handle, config) {
            return Err(HostError::GuiUnsupportedApi);
        }

        let mut created = false;
        let result = (|| {
            gui.create(&handle, config).map_err(HostError::Gui)?;
            created = true;
            gui.set_scale(&handle, scale).map_err(HostError::Gui)?;
            let size = gui.get_size(&handle).ok_or(HostError::GuiNoSize)?;
            raw::gui_set_parent(&gui, &handle, parent_hwnd).map_err(HostError::Gui)?;
            gui.show(&handle).map_err(HostError::Gui)?;
            Ok(size)
        })();

        match result {
            Ok(size) => {
                self.gui_open = true;
                Ok(size)
            }
            Err(err) => {
                if created {
                    gui.destroy(&handle);
                    self.gui.closed.store(false, Ordering::Relaxed);
                }
                Err(err)
            }
        }
    }

    /// Closes and destroys the plugin GUI (main thread only). Idempotent.
    /// This is also the host acknowledgement required after the plugin
    /// reported `closed(was_destroyed = true)`: destroy is issued exactly
    /// once either way.
    pub fn close_editor(&mut self) {
        if !self.gui_open {
            return;
        }
        self.gui_open = false;
        self.gui.closed.store(false, Ordering::Relaxed);
        if let Some(gui) = self.gui_ext {
            gui.destroy(&self.instance.plugin_handle());
        }
    }

    /// Shows a previously opened (but hidden) editor (main thread only).
    pub fn show_editor(&mut self) -> Result<(), HostError> {
        self.with_open_gui("show", |gui, handle| gui.show(handle))
    }

    /// Hides the editor without destroying it (main thread only; tab-away
    /// semantics, see `.agents/spec/plugin-gui.md` D9).
    pub fn hide_editor(&mut self) -> Result<(), HostError> {
        self.with_open_gui("hide", |gui, handle| gui.hide(handle))
    }

    /// Resizes the open embedded GUI (host-initiated; main thread only).
    /// Negotiates via `adjust_size` when the plugin can resize, then
    /// applies `set_size`.
    pub fn resize_editor(&mut self, size: GuiSize) -> Result<(), HostError> {
        if !self.gui_open {
            return Err(HostError::GuiNotOpen);
        }
        let Some(gui) = self.gui_ext else {
            return Err(HostError::NoGui);
        };
        let handle = self.instance.plugin_handle();
        let working = if gui.can_resize(&handle) {
            gui.adjust_size(&handle, size).unwrap_or(size)
        } else {
            size
        };
        gui.set_size(&handle, working).map_err(HostError::Gui)
    }

    /// Polls one-shot gui extension events (show/hide/resize requests from
    /// the plugin) accumulated since the last call (main thread only).
    pub fn editor_events(&self) -> EditorEvents {
        let packed = self.gui.resize_request.swap(0, Ordering::Relaxed);
        EditorEvents {
            resize: (packed != 0).then(|| GuiSize::unpack_from_u64(packed)),
            show: self.gui.show_request.swap(false, Ordering::Relaxed),
            hide: self.gui.hide_request.swap(false, Ordering::Relaxed),
            resize_hints_changed: self.gui.resize_hints_changed.swap(false, Ordering::Relaxed),
        }
    }

    /// Whether the plugin reported its GUI as destroyed
    /// (`closed(was_destroyed = true)`); sticky until
    /// [`Self::close_editor`] acknowledges it.
    pub fn gui_closed(&self) -> bool {
        self.gui.closed.load(Ordering::Relaxed)
    }

    /// Reads back parameter changes the plugin made on its main thread
    /// (spec `plugin-gui.md` §5, the `request_flush` response): takes the
    /// pending flag and, when set, calls the params extension's `flush`
    /// with an empty input queue, returning the `(param_id, plain_value)`
    /// pairs the plugin reported. Call from the UI frame loop while the
    /// plugin's editor is open (an idle poll is one atomic load).
    ///
    /// Not serialized against the audio thread (spec D7 rationale): the
    /// flush call is a main-thread plugin call and the standard host
    /// response to `request_flush` — the plugin guarantees
    /// main-thread-call safety while `process` executes, exactly like the
    /// gui calls the host already issues per frame.
    pub fn poll_param_flush(&mut self) -> Vec<(u32, f64)> {
        if !self.params.flush_requested.swap(false, Ordering::Relaxed) {
            return Vec::new();
        }
        raw::flush_params(self.instance.raw_instance())
    }

    fn with_open_gui(
        &mut self,
        op: &str,
        f: impl FnOnce(&PluginGui, &PluginMainThreadHandle) -> Result<(), GuiError>,
    ) -> Result<(), HostError> {
        if !self.gui_open {
            return Err(HostError::GuiNotOpen);
        }
        let Some(gui) = self.gui_ext else {
            return Err(HostError::NoGui);
        };
        let handle = self.instance.plugin_handle();
        f(&gui, &handle).map_err(|err| {
            log::warn!("plugin gui {op} failed: {err}");
            HostError::Gui(err)
        })
    }
}

/// One-shot instantiation result: control-side guard + real-time node.
pub struct ClapInstance {
    /// Control-side liveness guard (held by the UI / engine thread).
    pub guard: ClapInstanceGuard,
    /// Real-time-side node (moved into the audio thread).
    pub node: ClapNode,
}

/// Static configuration at instantiation time.
pub struct ClapInstanceConfig {
    /// Sample rate (Hz).
    pub sample_rate: u32,
    /// Maximum block size (should match [`FIXED_BLOCK_SIZE`]).
    pub max_frames: u32,
}

impl ClapInstance {
    /// Loads and activates the plugin, producing a real-time node. **Must be
    /// called on a non-real-time thread** (involves dynamic library loading
    /// and allocations inside the plugin).
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
            "GlitcherZ",
            "GlitcherZ Project",
            "https://github.com/glitcherz",
            env!("CARGO_PKG_VERSION"),
        )
        .map_err(|_| HostError::HostInfo)?;
        let gui_state = Arc::new(GuiHostState::default());
        let shared_gui_state = Arc::clone(&gui_state);
        let params_state = Arc::new(ParamsHostState::default());
        let shared_params_state = Arc::clone(&params_state);
        let mut instance = PluginInstance::<GlitcherZHostHandlers>::new(
            move |_| GlitcherZHostShared {
                state_ext: OnceLock::new(),
                gui_ext: OnceLock::new(),
                gui: Arc::clone(&shared_gui_state),
                params: Arc::clone(&shared_params_state),
            },
            |_| GlitcherZHostMainThread,
            &entry,
            &plugin_id,
            &host_info,
        )
        .map_err(HostError::Instance)?;

        // The extensions are captured in `initializing` (already run by
        // `new`); read them back here so save/load and gui open/close do
        // not depend on handler lifetimes.
        let state_ext = instance
            .access_shared_handler(|shared| shared.state_ext.get().copied())
            .flatten();
        let gui_ext = instance
            .access_shared_handler(|shared| shared.gui_ext.get().copied())
            .flatten();

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
        // Simplified host: activated on the control thread (no thread-check
        // enforcement registered, consistent with most simple hosts);
        // processing must be started via as_started_mut before process.
        let processor: PluginAudioProcessor<GlitcherZHostHandlers> = stopped.into();

        let state_gate = Arc::new(StateGate::default());

        Ok(Self {
            guard: ClapInstanceGuard {
                _entry: entry,
                instance,
                state_ext,
                state_gate: Arc::clone(&state_gate),
                gui_ext,
                gui: gui_state,
                params: params_state,
                gui_open: false,
            },
            node: ClapNode::new(processor, config, state_gate),
        })
    }

    /// Enumerates all parameters exposed by the plugin (metadata snapshot).
    ///
    /// Called on a **non-real-time thread** (directly invokes the plugin's
    /// main-thread callbacks). Returns an empty list when the plugin does not
    /// implement the params extension; individual parameter read failures are
    /// skipped rather than failing the whole call.
    /// RT-safety is irrelevant — this method does not touch the real-time
    /// node.
    pub fn list_params(&self) -> Vec<ClapParamInfo> {
        raw::enumerate_params(self.guard.instance.raw_instance())
    }

    /// Reads the current value of every parameter the plugin exposes (main
    /// thread; params extension `get_value`). Missing ids mean the plugin
    /// refused the read.
    ///
    /// Used after a state load to refresh the UI's cached slider values.
    pub fn read_param_values(&self) -> std::collections::HashMap<u32, f64> {
        let ids: Vec<u32> = self.list_params().iter().map(|p| p.id).collect();
        raw::read_param_values(self.guard.instance.raw_instance(), &ids)
            .into_iter()
            .collect()
    }
}

impl ClapNode {
    fn new(
        processor: impl Into<PluginAudioProcessor<GlitcherZHostHandlers>>,
        config: ClapInstanceConfig,
        state_gate: Arc<StateGate>,
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
            state_gate,
        }
    }

    fn stash(&mut self, event: PendingClapEvent) -> bool {
        if self.pending_count >= MAX_PENDING_EVENTS {
            return false; // dropped when full (non-critical path)
        }
        self.pending[self.pending_count] = event;
        self.pending_count += 1;
        true
    }
}

impl Processor for ClapNode {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        // 0. State-gate handoff: while the control thread runs a main-thread
        //    plugin call (state load/save/param read), park — acknowledge and
        //    keep the output zeroed (atomics only, no locks).
        if self.state_gate.audio_should_park() {
            let frames = ctx.frames.min(self.out_l.len()).min(buffer.capacity());
            for channel in [&mut self.out_l[..frames], &mut self.out_r[..frames]] {
                channel.fill(0.0);
            }
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

        // 1. Stashed events → CLAP input events (block granularity).
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
                // U4a: raw MIDI 1.0 at the block start. CLAP has no typed
                // control-change event, so the three bytes go as-is; plugins
                // that only understand CC (most legacy-style instruments) read
                // this path, not the parameter list.
                PendingClapEvent::Midi { data } => {
                    self.input_events.push(&MidiEvent::new(0, 0, data))
                }
            }
        }
        self.pending_count = 0;
        self.output_events.clear();

        // 2. Audio buffers: instruments have no input; output points at the
        //    node's pre-allocated scratch buffer. Field-splitting borrow: the
        //    two channel buffers and the processor are mutable at the same
        //    time.
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
        // Instruments have no input ports: empty port list.
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

        // 3. Invoke the plugin. Processing starts on the first call; any
        //    failure keeps the output zeroed (no panics).
        let started = match processor.as_started_mut() {
            Ok(started) => started,
            // Stopped state: try to start (first block); keep silence on
            // further failure.
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

        // 4. Copy into the engine's planar buffer (defensive out-of-bounds
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
                self.stash(PendingClapEvent::Param {
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
                self.stash(PendingClapEvent::NoteOn { note, velocity });
            }
            glitcherz_engine::realtime::NoteEvent::Off { note } => {
                self.stash(PendingClapEvent::NoteOff { note });
            }
        }
        true
    }

    /// U4a: a control-track curve drives CC `controller` on `channel`. The
    /// bytes are assembled here (status = 0xB0 | channel) so the audio thread
    /// never formats MIDI.
    fn midi_cc(&mut self, channel: u8, controller: u8, value: u8) -> bool {
        let status = 0xB0 | (channel & 0x0F);
        self.stash(PendingClapEvent::Midi {
            data: [status, controller & 0x7F, value & 0x7F],
        })
    }

    /// P0: a clip's pitch-bend controller event (spec `roll-multiclip.md`
    /// D9/D14). Stored as raw MIDI 1.0 — 14 bits centred at 8192, LSB first —
    /// because CLAP plugins interpret the wire format themselves and each
    /// plugin owns its bend range.
    fn midi_pitch_bend(&mut self, channel: u8, value: i16) -> bool {
        let raw = (i32::from(value.clamp(-8192, 8191)) + 8192) as u16;
        let status = 0xE0 | (channel & 0x0F);
        self.stash(PendingClapEvent::Midi {
            data: [status, (raw & 0x7F) as u8, ((raw >> 7) & 0x7F) as u8],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClapInstance, ClapInstanceConfig, ClapPluginInfo, HostError, list_clap_plugins_in,
        scan_clap_files,
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
                "{}glitcherz_test_plugin{}",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX
            ));
        path.exists().then_some(path)
    }

    fn spawn_test_plugin() -> Option<(ClapInstance, ProcessContext)> {
        let Some(path) = test_plugin_path() else {
            eprintln!("skipped: test plugin cdylib not built (use cargo test --workspace)");
            return None;
        };
        let instance = ClapInstance::spawn_rt(
            &path,
            "org.glitcherz.sine",
            ClapInstanceConfig {
                sample_rate: 48_000,
                max_frames: 128,
            },
        )
        .expect("failed to instantiate the test plugin");
        let ctx = ProcessContext {
            sample_rate: 48_000.0,
            frames: 128,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        };
        Some((instance, ctx))
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
            "should be silent without a note"
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
            "should have notable output after NoteOn, peak {peak_l}"
        );
        // Both channels come from the same single-voice render; contents
        // should be identical.
        let same = buffer
            .channel(1)
            .unwrap()
            .iter()
            .zip(buffer.channel(0).unwrap())
            .all(|(r, l)| (r - l).abs() < 1e-9);
        assert!(
            same,
            "stereo channels should be identical (single-voice source)"
        );

        // NoteOff → silence (the instrument has no release tail).
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
    fn param_bridge_routes_and_ignores_builtin() {
        let Some((mut instance, ctx)) = spawn_test_plugin() else {
            return;
        };
        let node = &mut instance.node;

        // Plugin parameters are accepted and queued (even if the plugin has
        // no parameter implementation yet, the event path works).
        assert!(node.set_param(ParamId::PluginParam(1), 0.5));
        // Built-in parameters are not consumed by ClapNode (they belong to
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
            "parameter events should not break rendering"
        );
    }

    #[test]
    fn list_plugins_reports_our_sine() {
        let Some(path) = test_plugin_path() else {
            eprintln!("skipped: test plugin cdylib not built");
            return;
        };
        let infos: Vec<ClapPluginInfo> = list_clap_plugins_in(&path);
        let sine = infos
            .iter()
            .find(|info| info.id == "org.glitcherz.sine")
            .expect("the test plugin should be enumerated");
        assert!(
            sine.is_instrument,
            "the sine plugin should carry the instrument feature"
        );
        assert_eq!(sine.name, "GlitcherZ Sine");
    }

    #[test]
    fn lists_plugin_params() {
        let Some((instance, _ctx)) = spawn_test_plugin() else {
            return;
        };
        let params = instance.list_params();
        assert_eq!(
            params.len(),
            3,
            "the test plugin should expose three parameters"
        );
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

        // Amplitude parameter 0.25: after NoteOn the peak should be close to
        // 0.25 (sine peak).
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
            "output peak should be close to the parameter value 0.25, got {peak}"
        );
    }

    #[test]
    fn state_roundtrip_restores_amplitude() {
        let Some((mut instance, ctx)) = spawn_test_plugin() else {
            return;
        };
        let mut buffer = AudioBuffer::new(2, 128);
        buffer.set_len(128);
        buffer.clear();

        // Move the amplitude away from its default via the param bridge
        // (needs one process block for the stashed event to flush).
        assert!(instance.node.set_param(ParamId::PluginParam(1), 0.25));
        instance.node.process(&ctx, &mut buffer);

        let blob = instance.guard.save_state().expect("state save should work");
        assert_eq!(blob.len(), 8, "the test plugin saves 8 bytes (f64 bits)");

        // Fresh instance: default amplitude (0.5), then restore the blob.
        let Some((mut restored, _)) = spawn_test_plugin() else {
            return;
        };
        let before = restored.read_param_values();
        assert_eq!(before.get(&1), Some(&0.5), "fresh instance has the default");
        restored
            .guard
            .load_state(&blob)
            .expect("state load should work");
        let after = restored.read_param_values();
        assert_eq!(
            after.get(&1),
            Some(&0.25),
            "loaded state should restore the amplitude"
        );
    }

    // ---- GUI extension protocol tests ----
    //
    // The test plugin's fake GUI records every call into a bitmask
    // (parameter 2) and a destroy counter (parameter 3); the bits below
    // mirror glitcherz-test-plugin's GUI_CALL_* constants. The fake GUI
    // also exercises the host callbacks: set_parent requests a resize to
    // 240x160 and show requests a show.

    use std::ffi::c_void;

    use super::{ClapInstanceGuard, GuiSize};

    const GUI_CALL_CREATE: u32 = 1 << 0;
    const GUI_CALL_DESTROY: u32 = 1 << 1;
    const GUI_CALL_SET_SCALE: u32 = 1 << 2;
    const GUI_CALL_GET_SIZE: u32 = 1 << 3;
    const GUI_CALL_SET_PARENT: u32 = 1 << 4;
    const GUI_CALL_SHOW: u32 = 1 << 5;
    const GUI_CALL_HIDE: u32 = 1 << 6;
    const GUI_CALL_SET_SIZE: u32 = 1 << 7;
    const GUI_CALL_ADJUST_SIZE: u32 = 1 << 8;

    const GUI_MASK_PARAM_ID: u32 = 2;
    const GUI_DESTROY_COUNT_PARAM_ID: u32 = 3;

    /// Fake parent window handle (the fake GUI never dereferences it).
    const FAKE_HWND: *mut c_void = 0x1234_5678usize as *mut c_void;

    fn gui_mask(guard: &ClapInstanceGuard) -> u32 {
        guard
            .read_param_values()
            .get(&GUI_MASK_PARAM_ID)
            .copied()
            .map(|value| value as u32)
            .unwrap_or(0)
    }

    fn gui_destroy_count(guard: &ClapInstanceGuard) -> u32 {
        guard
            .read_param_values()
            .get(&GUI_DESTROY_COUNT_PARAM_ID)
            .copied()
            .map(|value| value as u32)
            .unwrap_or(0)
    }

    #[test]
    fn gui_open_runs_embedded_sequence() {
        let Some((mut instance, _ctx)) = spawn_test_plugin() else {
            return;
        };
        let size = instance
            .guard
            .open_editor(FAKE_HWND, 1.0)
            .expect("open_editor should succeed");
        assert_eq!(
            size,
            GuiSize {
                width: 320,
                height: 200
            },
            "open_editor should report the plugin's size"
        );

        // The full embedded sequence ran, exactly once each.
        let mask = gui_mask(&instance.guard);
        let expected = GUI_CALL_CREATE
            | GUI_CALL_SET_SCALE
            | GUI_CALL_GET_SIZE
            | GUI_CALL_SET_PARENT
            | GUI_CALL_SHOW;
        assert_eq!(
            mask, expected,
            "open should run create→set_scale→get_size→set_parent→show and nothing else"
        );

        // The plugin's callbacks reached the host state (set_parent
        // requests a resize, show requests a show).
        let events = instance.guard.editor_events();
        assert_eq!(
            events.resize,
            Some(GuiSize {
                width: 240,
                height: 160
            }),
            "set_parent should have requested a 240x160 resize"
        );
        assert!(events.show, "show should have requested a show");
        assert!(!events.hide);

        // Take semantics: a second poll reports nothing new.
        let drained = instance.guard.editor_events();
        assert_eq!(
            drained,
            super::EditorEvents::default(),
            "editor events should be one-shot"
        );
    }

    #[test]
    fn gui_close_is_idempotent_and_reopenable() {
        let Some((mut instance, _ctx)) = spawn_test_plugin() else {
            return;
        };
        instance
            .guard
            .open_editor(FAKE_HWND, 1.0)
            .expect("open should succeed");
        instance.guard.close_editor();
        assert_eq!(
            gui_destroy_count(&instance.guard),
            1,
            "first close should destroy exactly once"
        );

        // Idempotent: a second close must not call destroy again.
        instance.guard.close_editor();
        assert_eq!(
            gui_destroy_count(&instance.guard),
            1,
            "second close must not destroy again"
        );

        // Reopenable after close.
        instance
            .guard
            .open_editor(FAKE_HWND, 1.0)
            .expect("reopen should succeed");
        instance.guard.close_editor();
        assert_eq!(
            gui_destroy_count(&instance.guard),
            2,
            "second lifecycle should destroy once more"
        );
        assert!(
            gui_mask(&instance.guard) & GUI_CALL_DESTROY != 0,
            "destroy bit should be set"
        );
    }

    #[test]
    fn gui_open_failure_cleans_up_and_reopens() {
        let Some((mut instance, _ctx)) = spawn_test_plugin() else {
            return;
        };
        // Negative scale: the test plugin reports closed(destroyed=true)
        // and fails set_scale.
        let result = instance.guard.open_editor(FAKE_HWND, -1.0);
        assert!(
            matches!(
                result,
                Err(super::HostError::Gui(
                    clack_extensions::gui::GuiError::SetScaleError
                ))
            ),
            "negative scale should fail at set_scale, got {result:?}"
        );
        assert_eq!(
            gui_destroy_count(&instance.guard),
            1,
            "the failed open should clean up via destroy"
        );
        assert!(
            !instance.guard.gui_closed(),
            "the cleanup destroy acknowledges the closed report"
        );

        // The guard is reusable after the failure.
        instance
            .guard
            .open_editor(FAKE_HWND, 1.0)
            .expect("reopen after failure should succeed");
        instance.guard.close_editor();
        assert_eq!(
            gui_destroy_count(&instance.guard),
            2,
            "the successful lifecycle after the failed one should destroy once"
        );
    }

    #[test]
    fn gui_resize_negotiates_via_adjust_size() {
        let Some((mut instance, _ctx)) = spawn_test_plugin() else {
            return;
        };
        instance
            .guard
            .open_editor(FAKE_HWND, 1.0)
            .expect("open should succeed");
        instance
            .guard
            .resize_editor(GuiSize {
                width: 640,
                height: 480,
            })
            .expect("resize should succeed");
        let mask = gui_mask(&instance.guard);
        assert!(
            mask & GUI_CALL_ADJUST_SIZE != 0 && mask & GUI_CALL_SET_SIZE != 0,
            "resize should go through adjust_size and set_size, mask {mask:#x}"
        );
    }

    #[test]
    fn gui_hide_and_show_without_destroy() {
        let Some((mut instance, _ctx)) = spawn_test_plugin() else {
            return;
        };
        instance
            .guard
            .open_editor(FAKE_HWND, 1.0)
            .expect("open should succeed");
        instance
            .guard
            .hide_editor()
            .expect("hide should succeed on an open editor");
        let mask = gui_mask(&instance.guard);
        assert!(mask & GUI_CALL_HIDE != 0, "hide should reach the plugin");
        assert_eq!(
            gui_destroy_count(&instance.guard),
            0,
            "hide must not destroy"
        );
        instance
            .guard
            .show_editor()
            .expect("show should succeed on a hidden editor");
        assert_eq!(
            gui_destroy_count(&instance.guard),
            0,
            "show must not destroy"
        );
    }

    #[test]
    fn gui_ops_on_closed_editor_error() {
        let Some((mut instance, _ctx)) = spawn_test_plugin() else {
            return;
        };
        assert!(matches!(
            instance.guard.resize_editor(GuiSize {
                width: 1,
                height: 1
            }),
            Err(super::HostError::GuiNotOpen)
        ));
        assert!(matches!(
            instance.guard.hide_editor(),
            Err(super::HostError::GuiNotOpen)
        ));
        assert!(matches!(
            instance.guard.show_editor(),
            Err(super::HostError::GuiNotOpen)
        ));
    }

    // ---- Parameter flush read-back (spec plugin-gui.md §5) ----
    //
    // The test plugin's hide() doubles as the "in-GUI edit" hook: it
    // changes the amplitude on the plugin's main thread and calls
    // host_params::request_flush; the host's poll_param_flush then reads
    // the change back through the params extension's flush callback.

    /// Amplitude the test plugin's GUI edit hook sets (mirrors
    /// glitcherz-test-plugin's GUI_EDIT_AMPLITUDE).
    const GUI_EDIT_AMPLITUDE: f64 = 0.75;

    #[test]
    fn param_flush_reads_back_gui_changes() {
        let Some((mut instance, _ctx)) = spawn_test_plugin() else {
            return;
        };
        instance
            .guard
            .open_editor(FAKE_HWND, 1.0)
            .expect("open should succeed");

        // No request yet: the poll is empty and does not call flush.
        assert!(
            instance.guard.poll_param_flush().is_empty(),
            "no flush requested yet"
        );

        // The in-GUI edit hook: hide changes the amplitude and requests a
        // flush.
        instance
            .guard
            .hide_editor()
            .expect("hide should succeed on an open editor");
        let changes = instance.guard.poll_param_flush();
        assert_eq!(
            changes,
            vec![(1, GUI_EDIT_AMPLITUDE)],
            "the flush read-back should report the GUI edit"
        );

        // Take semantics: a second poll reports nothing new.
        assert!(
            instance.guard.poll_param_flush().is_empty(),
            "flush requests are one-shot"
        );
    }

    #[test]
    fn param_flush_value_feeds_engine_bridge() {
        let Some((mut instance, ctx)) = spawn_test_plugin() else {
            return;
        };
        let node = &mut instance.node;
        let mut buffer = AudioBuffer::new(2, 128);
        buffer.set_len(128);
        buffer.clear();

        // GUI edit read-back (protocol as above), then the read-back value
        // takes the existing PluginParam bridge into the engine.
        instance
            .guard
            .open_editor(FAKE_HWND, 1.0)
            .expect("open should succeed");
        instance
            .guard
            .hide_editor()
            .expect("hide should succeed on an open editor");
        for (id, value) in instance.guard.poll_param_flush() {
            assert!(node.set_param(ParamId::PluginParam(id), value as f32));
        }

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
            (peak - GUI_EDIT_AMPLITUDE as f32).abs() < 0.02,
            "the read-back value should shape the output through the param \
             bridge, expected ~{GUI_EDIT_AMPLITUDE}, got {peak}"
        );
    }

    #[test]
    fn invalid_plugin_fails_gracefully() {
        let bogus = std::env::temp_dir().join("glitcherz_bogus.clap");
        std::fs::write(&bogus, b"not a dll").expect("write failed");
        let result = ClapInstance::spawn_rt(
            &bogus,
            "org.glitcherz.bogus",
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

    /// Real-world repro (diagnostic): if a Dexed CLAP build is installed in
    /// the standard scan directories, instantiate it, trigger a note, and
    /// assert the output is not silent. Isolates "host bridging broken" from
    /// "mixer chain broken" and "plugin state/patch empty" (GlitcherZ hosts
    /// neither the gui nor the state extension, so Dexed runs on its internal
    /// default patch). Skipped when no `dexed*.clap` is found.
    #[test]
    fn dexed_instrument_note_on_produces_audio() {
        let Some(path) = scan_clap_files().into_iter().find(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().to_lowercase().contains("dexed"))
                .unwrap_or(false)
        }) else {
            eprintln!("skipped: no dexed*.clap in the CLAP scan directories");
            return;
        };
        let infos = list_clap_plugins_in(&path);
        let Some(info) = infos.iter().find(|info| info.is_instrument) else {
            panic!(
                "{} exposes no instrument plugin (found: {infos:?})",
                path.display()
            );
        };

        let mut instance = ClapInstance::spawn_rt(
            &path,
            &info.id,
            ClapInstanceConfig {
                sample_rate: 48_000,
                max_frames: 128,
            },
        )
        .expect("Dexed should instantiate and activate");
        let param_count = instance.list_params().len();
        eprintln!("dexed: id={}, params={param_count}", info.id);

        let ctx = ProcessContext {
            sample_rate: 48_000.0,
            frames: 128,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        };
        let mut buffer = AudioBuffer::new(2, 128);
        buffer.set_len(128);
        buffer.clear();

        // Warm-up block (also performs the lazy start_processing), then
        // enough post-NoteOn blocks to cover a slow attack (~27 ms here).
        instance.node.process(&ctx, &mut buffer);
        assert!(instance.node.note_event(NoteEvent::On {
            note: 69,
            velocity: 1.0
        }));
        let mut peak = 0.0f32;
        for _ in 0..16 {
            instance.node.process(&ctx, &mut buffer);
            let block_peak = buffer
                .channel(0)
                .unwrap()
                .iter()
                .fold(0.0f32, |acc, s| acc.max(s.abs()));
            peak = peak.max(block_peak);
        }
        assert!(
            peak > 1e-4,
            "Dexed stayed silent after NoteOn (peak {peak}, params {param_count}) — \
             suspect an empty default patch (no state extension hosted) or a \
             bridging gap; check the clap_plugin log lines"
        );
    }

    /// Dev tool (diagnostic dump): enumerates every Dexed parameter
    /// (id | module | name | min | max | default) into a workspace text file
    /// for building the DX7 sysex → Dexed param mapping. Run with:
    /// `cargo test -p glitcherz-clap-host dexed_dump_param_table -- --ignored --nocapture`
    #[test]
    #[ignore = "dev tool: writes dexed_params.txt into the workspace root"]
    fn dexed_dump_param_table() {
        let Some(path) = scan_clap_files().into_iter().find(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().to_lowercase().contains("dexed"))
                .unwrap_or(false)
        }) else {
            panic!("no dexed*.clap found in the CLAP scan directories");
        };
        let infos = list_clap_plugins_in(&path);
        let info = infos
            .iter()
            .find(|info| info.is_instrument)
            .expect("Dexed exposes no instrument class");
        let instance = ClapInstance::spawn_rt(
            &path,
            &info.id,
            ClapInstanceConfig {
                sample_rate: 48_000,
                max_frames: 128,
            },
        )
        .expect("Dexed should instantiate");
        let params = instance.list_params();
        let mut out = String::new();
        out.push_str(&format!("# Dexed param dump from {}\n", path.display()));
        out.push_str(&format!("# total {}\n", params.len()));
        out.push_str("id | module | name | min | max | default\n");
        for p in &params {
            out.push_str(&format!(
                "{} | {} | {} | {:.3} | {:.3} | {:.3}\n",
                p.id, p.module, p.name, p.min_value, p.max_value, p.default_value
            ));
        }
        let dest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dexed_params.txt");
        std::fs::write(&dest, out).expect("write dump");
        eprintln!("wrote {}", dest.canonicalize().unwrap().display());
    }

    // -----------------------------------------------------------------------
    // DX7 bank (.syx) → Dexed preset import (dev tool)
    // -----------------------------------------------------------------------
    //
    // Dexed's state blob is `VC2!` + u32 LE xml_len + XML + '\0', where the
    // XML carries the **whole 4104-byte DX7 cartridge** as a JUCE base64
    // attribute (`base64:sysex="4104.<b64>"`) plus `currentProgram="N"`.
    // Importing a bank therefore needs no parameter mapping: splice the bank
    // bytes into the template XML, set the program index per voice, and load.

    /// JUCE's **proprietary** base64 (MemoryBlock::toBase64Encoding):
    /// alphabet `.` + A-Z + a-z + 0-9 + `+` (no '/' and no '=' padding),
    /// "SIZE." prefix is written by the caller. Dexed stores its blob
    /// attributes with this encoding, not RFC 4648.
    fn b64_encode(data: &[u8]) -> String {
        const T: &[u8; 64] = b".ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+";
        let num_chars = (data.len() * 8).div_ceil(6);
        let mut out = String::with_capacity(num_chars);
        for i in 0..num_chars {
            let bit_pos = i * 6;
            let byte_i = bit_pos / 8;
            let shift = bit_pos % 8;
            let next = data.get(byte_i + 1).copied().unwrap_or(0) as u32;
            let v = ((((data[byte_i] as u32) << 8) | next) >> (10 - shift)) & 0x3f;
            out.push(T[v as usize] as char);
        }
        out
    }

    /// Inverse of [`b64_encode`] (JUCE `fromBase64Encoding`, minus the size
    /// prefix) — used to verify roundtrips byte-for-byte.
    fn b64_decode(encoded: &str, out_len: usize) -> Vec<u8> {
        fn value(c: u8) -> Option<u32> {
            Some(match c {
                b'.' => 0,
                b'A'..=b'Z' => (c - b'A') as u32 + 1,
                b'a'..=b'z' => (c - b'a') as u32 + 27,
                b'0'..=b'9' => (c - b'0') as u32 + 53,
                b'+' => 63,
                _ => return None,
            })
        }
        let mut out = vec![0u8; out_len];
        for (k, ch) in encoded.chars().enumerate() {
            let Some(v) = value(ch as u8) else { continue };
            let bit_pos = k * 6;
            let byte_i = bit_pos / 8;
            let shift = bit_pos % 8;
            if byte_i >= out_len {
                break;
            }
            // The 6 bits v land at stream positions [shift, shift+6) within
            // the MSB-first byte stream; split across bytes when needed.
            match shift {
                0 => out[byte_i] |= (v << 2) as u8,
                2 => out[byte_i] |= v as u8,
                4 => {
                    out[byte_i] |= (v >> 2) as u8;
                    if byte_i + 1 < out_len {
                        out[byte_i + 1] |= ((v & 3) << 6) as u8;
                    }
                }
                6 => {
                    out[byte_i] |= (v >> 4) as u8;
                    if byte_i + 1 < out_len {
                        out[byte_i + 1] |= ((v & 0xf) << 4) as u8;
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// `Cartridge::unpackProgram` (Dexed old PluginData.cpp, ported 1:1):
    /// one 128-byte packed bank voice → the 155-byte engine layout
    /// (`data[0..155]`).
    fn unpack_voice(packed: &[u8]) -> [u8; 155] {
        let bulk = |i: usize| packed[i] & 0x7f;
        let mut up = [0u8; 155];
        let norm = |value: u8, max: u8| {
            if value <= max {
                value
            } else {
                ((value as f32) / 255.0 * max as f32) as u8
            }
        };
        for op in 0..6usize {
            for i in 0..11 {
                up[op * 21 + i] = norm(bulk(op * 17 + i), 99);
            }
            let curves = bulk(op * 17 + 11) & 0xf;
            up[op * 21 + 11] = curves & 3;
            up[op * 21 + 12] = (curves >> 2) & 3;
            let detune_rs = bulk(op * 17 + 12);
            up[op * 21 + 13] = detune_rs & 7;
            let kvs_ams = bulk(op * 17 + 13) & 0x1f;
            up[op * 21 + 14] = kvs_ams & 3;
            up[op * 21 + 15] = (kvs_ams >> 2) & 7;
            up[op * 21 + 16] = bulk(op * 17 + 14);
            let fcoarse_mode = bulk(op * 17 + 15) & 0x3f;
            up[op * 21 + 17] = fcoarse_mode & 1;
            up[op * 21 + 18] = (fcoarse_mode >> 1) & 0x1f;
            up[op * 21 + 19] = bulk(op * 17 + 16);
            up[op * 21 + 20] = (detune_rs >> 3) & 0x7f;
        }
        for i in 0..8 {
            up[126 + i] = norm(bulk(102 + i), 99);
        }
        up[134] = norm(bulk(110) & 0x1f, 31);
        let oks_fb = bulk(111) & 0xf;
        up[135] = oks_fb & 7;
        up[136] = oks_fb >> 3;
        up[137] = bulk(112);
        up[138] = bulk(113);
        up[139] = bulk(114);
        up[140] = bulk(115);
        let lpms_lfw_lks = bulk(116);
        up[141] = lpms_lfw_lks & 1;
        up[142] = (lpms_lfw_lks >> 1) & 7;
        up[143] = lpms_lfw_lks >> 4;
        up[144] = bulk(117);
        for i in 0..10 {
            up[145 + i] = bulk(118 + i);
        }
        up
    }

    /// Replaces `prefix"old"` with `prefix"new"` (attribute-value surgery on
    /// the flat Dexed XML).
    fn xml_attr_replace(xml: &str, attr_prefix: &str, new_value: &str) -> Option<String> {
        let start = xml.find(attr_prefix)? + attr_prefix.len();
        let end = start + xml[start..].find('"')?;
        Some(format!("{}{}{}", &xml[..start], new_value, &xml[end..]))
    }

    /// Filesystem-safe fragment (mirrors the UI's `sanitize_filename_fragment`).
    fn sanitize_fragment(text: &str, max_chars: usize) -> String {
        let cleaned: String = text
            .chars()
            .take(max_chars)
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | ' ' => c,
                _ => '_',
            })
            .collect();
        let trimmed = cleaned.trim();
        if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
            "plugin".to_string()
        } else {
            trimmed.to_string()
        }
    }

    /// The preset-library key (mirrors the UI's `preset_plugin_key`).
    fn preset_key_for(path_str: &str, id: &str) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path_str.hash(&mut hasher);
        format!(
            "clap-{:08x}-{}",
            hasher.finish() as u32,
            sanitize_fragment(id, 24)
        )
    }

    /// DX7 bank checksum (two's complement of the data sum, 7-bit).
    fn dx7_checksum(data: &[u8]) -> u8 {
        let sum: u32 = data.iter().map(|b| *b as u32).sum();
        ((0x80 - (sum & 0x7f)) & 0x7f) as u8
    }

    /// Imports the YM2612 bank .syx files (each 32 DX7 voices) into the
    /// preset library as Dexed presets — one `.glzpreset` per voice,
    /// verified by a load→save roundtrip and (bank voice #0) a render check.
    #[test]
    #[ignore = "dev tool: imports .syx banks into presets/ (needs Dexed installed)"]
    fn dexed_import_ym2612_banks() {
        let Some(path) = scan_clap_files().into_iter().find(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().to_lowercase().contains("dexed"))
                .unwrap_or(false)
        }) else {
            panic!("no dexed*.clap found in the CLAP scan directories");
        };
        let infos = list_clap_plugins_in(&path);
        let info = infos
            .iter()
            .find(|info| info.is_instrument)
            .expect("Dexed exposes no instrument class");
        let mut instance = ClapInstance::spawn_rt(
            &path,
            &info.id,
            ClapInstanceConfig {
                sample_rate: 48_000,
                max_frames: 128,
            },
        )
        .expect("Dexed should instantiate");

        // Template state: fresh XML with the stock cartridge + program.
        let fresh = instance
            .guard
            .save_state()
            .expect("Dexed implements the state extension");
        assert_eq!(&fresh[0..4], b"VC2!", "unexpected Dexed state magic");
        let xml_len = u32::from_le_bytes([fresh[4], fresh[5], fresh[6], fresh[7]]) as usize;
        let template_xml =
            String::from_utf8(fresh[8..8 + xml_len].to_vec()).expect("Dexed state XML is UTF-8");
        assert!(
            template_xml.contains("base64:sysex=\"4104."),
            "template XML does not carry a 4104-byte cartridge"
        );
        // The `program` blob (Dexed's data[161]) is what actually drives the
        // engine on load (`memcpy(data, program, 161)`) — its tail
        // (data[155..161], op-switch byte + controller extras) is donated by
        // the template.
        let template_program_start = template_xml
            .find("base64:program=\"")
            .expect("program attr")
            + 17;
        let template_program_end =
            template_program_start + template_xml[template_program_start..].find('"').unwrap();
        let template_program_all = &template_xml[template_program_start..template_program_end];
        let (_prefix, template_program_b64) =
            template_program_all.split_once('.').expect("size prefix");
        let template_program = b64_decode(template_program_b64, 161);

        let ctx = ProcessContext {
            sample_rate: 48_000.0,
            frames: 128,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        };
        let mut buffer = AudioBuffer::new(2, 128);
        buffer.set_len(128);
        buffer.clear();

        // Workspace layout: this crate lives at <workspace>/crates/
        // glitcherz-clap-host. Bank directory overridable via
        // `DEXED_IMPORT_BANK_DIR` (a folder of 4104-byte DX7 bank .syx
        // files); defaults to the YM2612 scratch extraction.
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest.join("../..").canonicalize().unwrap();
        let bank_dir = std::env::var("DEXED_IMPORT_BANK_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                workspace.join(".tmp-dexed-presets/YM2612-Dexed-Presets-main/YM2612_CART")
            });
        let preset_root = workspace.join("presets");
        let key = preset_key_for(&path.display().to_string(), &info.id);

        let mut imported = 0usize;
        let mut checked_audio = 0usize;
        let bank_paths: Vec<_> = std::fs::read_dir(&bank_dir)
            .expect("bank dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "syx").unwrap_or(false))
            .collect();
        assert!(
            !bank_paths.is_empty(),
            "no .syx banks found in {bank_dir:?}"
        );

        for bank_path in &bank_paths {
            let cart = std::fs::read(bank_path).expect("read bank");
            assert_eq!(cart.len(), 4104, "bank size must be 4104");
            assert_eq!(
                &cart[0..6],
                &[0xF0, 0x43, 0x00, 0x09, 0x20, 0x00],
                "unexpected DX7 bank header"
            );
            let data = &cart[6..6 + 4096];
            assert_eq!(cart[4102], dx7_checksum(data), "bank checksum mismatch");
            assert_eq!(cart[4103], 0xF7, "missing sysex end byte");
            let bank_label = sanitize_fragment(
                bank_path
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .strip_prefix("YM2612_")
                    .unwrap_or("BANK"),
                24,
            );
            let cart_b64 = b64_encode(&cart);

            for voice in 0..32u32 {
                let packed = &data[voice as usize * 128..(voice as usize + 1) * 128];
                let name: String = packed[118..128]
                    .iter()
                    .map(|b| {
                        if (32..127).contains(b) {
                            *b as char
                        } else {
                            ' '
                        }
                    })
                    .collect();
                let name = name.trim().trim_end_matches('_').replace(' ', "_");
                let name = sanitize_fragment(&name, 24);

                // Engine data: unpacked voice + the template's tail.
                let unpacked = unpack_voice(packed);
                let mut program = [0u8; 161];
                program[..155].copy_from_slice(&unpacked);
                program[155..161].copy_from_slice(&template_program[155..161]);

                // Splice cartridge + program + program index into the XML.
                let xml = xml_attr_replace(
                    &template_xml,
                    "base64:sysex=\"",
                    &format!("4104.{}", cart_b64),
                )
                .expect("sysex attribute");
                let xml = xml_attr_replace(
                    &xml,
                    "base64:program=\"",
                    &format!("161.{}", b64_encode(&program)),
                )
                .expect("program attribute");
                let xml = xml_attr_replace(&xml, "currentProgram=\"", &voice.to_string())
                    .expect("currentProgram attribute");
                let mut blob = Vec::with_capacity(8 + xml.len() + 1);
                blob.extend_from_slice(b"VC2!");
                blob.extend_from_slice(&(xml.len() as u32).to_le_bytes());
                blob.extend_from_slice(xml.as_bytes());
                blob.push(0);

                instance
                    .guard
                    .load_state(&blob)
                    .expect("Dexed rejected the constructed state");

                // Roundtrip check: the saved state must carry the same
                // program bytes (the sound-critical part) and index.
                let saved = instance.guard.save_state().expect("save after load");
                let saved_xml_len =
                    u32::from_le_bytes([saved[4], saved[5], saved[6], saved[7]]) as usize;
                let saved_xml = String::from_utf8(saved[8..8 + saved_xml_len].to_vec()).unwrap();
                assert!(
                    saved_xml.contains(&format!("currentProgram=\"{voice}\"")),
                    "program index did not stick (voice {voice})"
                );
                {
                    let start = saved_xml.find("base64:program=\"").expect("program attr") + 17;
                    let end = start + saved_xml[start..].find('"').unwrap();
                    let (_p, b64) = saved_xml[start..end].split_once('.').expect("size prefix");
                    let saved_program = b64_decode(b64, 161);
                    assert_eq!(
                        &saved_program[..155],
                        &unpacked[..],
                        "engine program bytes did not survive the roundtrip (voice {voice})"
                    );
                }

                // Render check: bank voice #0 must produce sound.
                if voice == 0 {
                    instance.node.process(&ctx, &mut buffer);
                    assert!(instance.node.note_event(NoteEvent::On {
                        note: 69,
                        velocity: 1.0
                    }));
                    let mut peak = 0.0f32;
                    for _ in 0..16 {
                        instance.node.process(&ctx, &mut buffer);
                        peak = peak.max(
                            buffer
                                .channel(0)
                                .unwrap()
                                .iter()
                                .fold(0.0f32, |acc, s| acc.max(s.abs())),
                        );
                    }
                    instance.node.note_event(NoteEvent::Off { note: 69 });
                    assert!(
                        peak > 1e-4,
                        "bank {bank_label} voice 0 stayed silent (peak {peak})"
                    );
                    checked_audio += 1;
                    instance.node.process(&ctx, &mut buffer);
                }

                // Persist: raw state bytes under the bank-prefixed name.
                let preset_name = format!("{bank_label}-{voice:02}-{name}");
                let dir = preset_root.join(&key);
                std::fs::create_dir_all(&dir).expect("preset dir");
                std::fs::write(dir.join(format!("{preset_name}.glzpreset")), &blob)
                    .expect("write preset");
                imported += 1;
            }
            eprintln!("bank {bank_label}: 32 voices imported");
        }

        eprintln!("imported {imported} presets into presets/{key} (audio-checked {checked_audio})");
        assert!(imported > 0);
    }

    /// Dev tool (diagnostic): loads one imported `.glzpreset` (plus the
    /// factory-template state as a control) and renders a long NoteOn,
    /// printing a per-block peak profile so "click at note start, then
    /// silence" reproduces headlessly. Run with:
    /// `cargo test -p glitcherz-clap-host dexed_preset_render_profile -- --ignored --nocapture`
    #[test]
    #[ignore = "dev tool: needs Dexed installed + imported presets/"]
    fn dexed_preset_render_profile() {
        let Some(path) = scan_clap_files().into_iter().find(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().to_lowercase().contains("dexed"))
                .unwrap_or(false)
        }) else {
            panic!("no dexed*.clap found in the CLAP scan directories");
        };
        let infos = list_clap_plugins_in(&path);
        let info = infos
            .iter()
            .find(|info| info.is_instrument)
            .expect("Dexed exposes no instrument class");
        let mut instance = ClapInstance::spawn_rt(
            &path,
            &info.id,
            ClapInstanceConfig {
                sample_rate: 48_000,
                max_frames: 128,
            },
        )
        .expect("Dexed should instantiate");

        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let preset_dir = workspace
            .join("presets")
            .join(preset_key_for(&path.display().to_string(), &info.id));
        let template = instance
            .guard
            .save_state()
            .expect("Dexed implements the state extension");

        // State XML summary: the global attrs that shape the render.
        let xml_of = |blob: &[u8]| -> String {
            let len = u32::from_le_bytes([blob[4], blob[5], blob[6], blob[7]]) as usize;
            String::from_utf8(blob[8..8 + len].to_vec()).unwrap()
        };
        let attr_of = |xml: &str, prefix: &str| -> String {
            let start = match xml.find(prefix) {
                Some(s) => s + prefix.len(),
                None => return "<absent>".into(),
            };
            let end = start + xml[start..].find('"').unwrap();
            xml[start..end].to_string()
        };

        // Unpacked-voice summary from a blob's `program` attr: per-op EG
        // rates/levels + output level, and the global algorithm/feedback.
        let program_of = |xml: &str| -> Vec<u8> {
            let start = xml.find("base64:program=\"").expect("program attr") + 17;
            let end = start + xml[start..].find('"').unwrap();
            let (_p, b64) = xml[start..end].split_once('.').expect("size prefix");
            b64_decode(b64, 161)
        };
        let dump_template_raw = |xml: &str| {
            let start = xml.find("base64:program=\"").expect("program attr") + 17;
            let end = start + xml[start..].find('"').unwrap();
            let raw_attr = &xml[start..end];
            let (prefix, b64) = raw_attr.split_once('.').expect("size prefix");
            eprintln!(
                "    template program attr: size-prefix={prefix} b64_len={} b64_head={}",
                b64.len(),
                &b64[..b64.len().min(24)]
            );
            let at = xml.find("base64:program=\"").expect("program attr");
            eprintln!(
                "    xml around attr: {:?}",
                &xml[at.saturating_sub(60)..at + 80]
            );
            let program = b64_decode(b64, 161);
            eprintln!("    template program bytes[0..40]: {:?}", &program[..40]);
            eprintln!(
                "    template program bytes[155..161]: {:?}",
                &program[155..161]
            );
        };
        let dump_program = |xml: &str| {
            let program = program_of(xml);
            let name: String = program[145..155]
                .iter()
                .map(|b| {
                    if (32..127).contains(b) {
                        *b as char
                    } else {
                        ' '
                    }
                })
                .collect();
            eprintln!("    voice name: {:?}", name.trim());
            eprintln!(
                "    algorithm {} feedback {} osc-sync {}",
                program[134], program[135], program[136]
            );
            for op in 0..6usize {
                let o = op * 21;
                eprintln!(
                    "    op{}: rates={:?} levels={:?} out={} coarse={} fine={} mode={} kvs={} ams={}",
                    6 - op,
                    &program[o..o + 4],
                    &program[o + 4..o + 8],
                    program[o + 16],
                    program[o + 18],
                    program[o + 19],
                    program[o + 17],
                    program[o + 15],
                    program[o + 14],
                );
            }
        };

        let ctx = ProcessContext {
            sample_rate: 48_000.0,
            frames: 128,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        };
        let mut buffer = AudioBuffer::new(2, 128);
        buffer.set_len(128);
        buffer.clear();

        let mut render = |label: &str, blob: &[u8], blocks: usize| {
            eprintln!("== {label}");
            let xml = xml_of(blob);
            eprintln!(
                "    attrs: currentProgram={} opSwitch={} gain={} engineType={}",
                attr_of(&xml, "currentProgram=\""),
                attr_of(&xml, "opSwitch=\""),
                attr_of(&xml, "gain=\""),
                attr_of(&xml, "engineType=\""),
            );
            dump_program(&xml);

            instance.guard.load_state(blob).expect("state load failed");
            instance.node.process(&ctx, &mut buffer); // warm-up / lazy start
            assert!(instance.node.note_event(NoteEvent::On {
                note: 69,
                velocity: 1.0
            }));

            let mut peaks = Vec::with_capacity(blocks);
            for _ in 0..blocks {
                instance.node.process(&ctx, &mut buffer);
                let peak = buffer
                    .channel(0)
                    .unwrap()
                    .iter()
                    .fold(0.0f32, |acc, s| acc.max(s.abs()));
                peaks.push(peak);
            }
            instance.node.note_event(NoteEvent::Off { note: 69 });
            instance.node.process(&ctx, &mut buffer);

            let max_peak = peaks.iter().copied().fold(0.0f32, f32::max);
            let nonzero = peaks.iter().filter(|p| **p > 1e-4).count();
            eprintln!("    max peak {max_peak:.6}, blocks > 1e-4: {nonzero}/{blocks}");
            // Compact profile: every 4th block peak over the first ~2 s.
            let shown: Vec<String> = peaks
                .iter()
                .take(blocks.min(200))
                .step_by(4)
                .map(|p| format!("{p:.4}"))
                .collect();
            eprintln!("    profile: {}", shown.join(" "));
        };

        render("control: fresh template state", &template, 100);
        dump_template_raw(&xml_of(&template));
        for (label, file) in [
            (
                "preset: BASS_BRASS-07 Bass 08",
                "ROM1_BASS_BRASS-07-Bass_08.glzpreset",
            ),
            (
                "preset: MORPH-00 Morph 01",
                "ROM3_MORPH-00-Morph_01.glzpreset",
            ),
        ] {
            let blob = std::fs::read(preset_dir.join(file))
                .unwrap_or_else(|err| panic!("read {file}: {err}"));
            render(label, &blob, 100);
        }

        // App-like scenario: repeated press/release cycles on the Bass 08
        // preset at a lower velocity, across two sample rates, plus live
        // state switches while the plugin is already running.
        let bass_blob = std::fs::read(preset_dir.join("ROM1_BASS_BRASS-07-Bass_08.glzpreset"))
            .expect("read bass preset");
        for sr in [48_000u32, 44_100] {
            let mut inst = ClapInstance::spawn_rt(
                &path,
                &info.id,
                ClapInstanceConfig {
                    sample_rate: sr,
                    max_frames: 128,
                },
            )
            .expect("spawn for cycling test");
            let ctx = ProcessContext {
                sample_rate: sr as f32,
                frames: 128,
                playback_sample_pos: 0,
                song_ticks: 0.0,
                transport_dt_ticks: 0.0,
                transport_active: true,
                sec_per_tick: 60.0 / (120.0 * 960.0),
            };
            let mut buf = AudioBuffer::new(2, 128);
            buf.set_len(128);
            buf.clear();
            inst.node.process(&ctx, &mut buf); // warm-up / lazy start

            let held_peaks = |inst: &mut ClapInstance,
                              ctx: &ProcessContext,
                              buf: &mut AudioBuffer,
                              note: u8,
                              velocity: f32,
                              blocks: usize|
             -> Vec<f32> {
                let _ = inst.node.note_event(NoteEvent::On { note, velocity });
                let mut peaks = Vec::with_capacity(blocks);
                for _ in 0..blocks {
                    inst.node.process(ctx, buf);
                    let peak = buf
                        .channel(0)
                        .unwrap()
                        .iter()
                        .fold(0.0f32, |acc, s| acc.max(s.abs()));
                    peaks.push(peak);
                }
                let _ = inst.node.note_event(NoteEvent::Off { note });
                for _ in 0..8 {
                    inst.node.process(ctx, buf);
                }
                peaks
            };

            inst.guard.load_state(&bass_blob).expect("load bass");
            inst.node.process(&ctx, &mut buf);
            eprintln!("== cycling @ {sr} Hz, velocity 0.8, Bass 08");
            for cycle in 0..5u8 {
                let note = 45 + cycle * 3;
                let peaks = held_peaks(&mut inst, &ctx, &mut buf, note, 0.8, 16);
                eprintln!("    cycle {cycle} (note {note}): held peaks {peaks:?}");
            }

            eprintln!("    -- note x velocity grid (16-block held peak every 2 blocks)");
            for note in [36u8, 48, 60, 69, 81] {
                for velocity in [0.5f32, 1.0] {
                    let peaks = held_peaks(&mut inst, &ctx, &mut buf, note, velocity, 16);
                    let sampled: Vec<f32> = peaks.iter().step_by(2).copied().collect();
                    eprintln!("    note {note} vel {velocity}: {sampled:?}");
                }
            }

            eprintln!("    -- velocity sweep @ note 60 (bass preset then template)");
            for velocity in [1.0f32, 0.95, 0.9, 0.8, 0.7, 0.6, 0.5] {
                let peaks = held_peaks(&mut inst, &ctx, &mut buf, 60, velocity, 16);
                let tail: Vec<f32> = peaks.iter().step_by(4).copied().collect();
                eprintln!("    bass08 vel {velocity}: {tail:?}");
            }
            inst.guard.load_state(&template).expect("reload template");
            inst.node.process(&ctx, &mut buf);
            for velocity in [1.0f32, 0.8, 0.5] {
                let peaks = held_peaks(&mut inst, &ctx, &mut buf, 60, velocity, 16);
                let tail: Vec<f32> = peaks.iter().step_by(4).copied().collect();
                eprintln!("    template vel {velocity}: {tail:?}");
            }

            // KVS attribution: patch the bass preset's program bytes so every
            // operator's key-velocity sensitivity is 0/1/3, re-encode the
            // state, and render at velocity 0.5. If the collapse tracks KVS,
            // Dexed's ScaleVelocity path is live; if identical, velocity
            // enters elsewhere.
            eprintln!("    -- KVS sweep @ note 60 vel 0.5");
            for kvs in [0u8, 1, 3] {
                let mut patched = bass_blob.clone();
                let xml_start = 8usize;
                let xml_len =
                    u32::from_le_bytes([patched[4], patched[5], patched[6], patched[7]]) as usize;
                let mut xml = String::from_utf8(patched[xml_start..xml_start + xml_len].to_vec())
                    .expect("xml");
                let start = xml.find("base64:program=\"").expect("program attr") + 16;
                let end = start + xml[start..].find('"').unwrap();
                let raw_attr = &xml[start..end];
                let (prefix, b64) = raw_attr.split_once('.').expect("size prefix");
                let mut program = b64_decode(b64, prefix.parse::<usize>().unwrap());
                for op in 0..6usize {
                    program[op * 21 + 15] = kvs;
                }
                let new_attr = format!("{}.{}", prefix, b64_encode(&program));
                xml.replace_range(start..end, &new_attr);
                let mut blob = Vec::with_capacity(8 + xml.len() + 1);
                blob.extend_from_slice(b"VC2!");
                blob.extend_from_slice(&(xml.len() as u32).to_le_bytes());
                blob.extend_from_slice(xml.as_bytes());
                blob.push(0);
                patched = blob;

                inst.guard.load_state(&patched).expect("load patched");
                inst.node.process(&ctx, &mut buf);
                let peaks = held_peaks(&mut inst, &ctx, &mut buf, 60, 0.5, 16);
                let tail: Vec<f32> = peaks.iter().step_by(4).copied().collect();
                eprintln!("    kvs={kvs} vel 0.5: {tail:?}");
            }

            // Byte-exact threshold hunt: velocity = byte/127.
            eprintln!("    -- byte threshold hunt (kvs=0 blob, note 60)");
            for byte in [100u32, 103, 105, 106, 107, 108, 109, 110, 111, 113] {
                inst.guard.load_state(&bass_blob).expect("reload bass");
                inst.node.process(&ctx, &mut buf);
                let velocity = byte as f32 / 127.0;
                let peaks = held_peaks(&mut inst, &ctx, &mut buf, 60, velocity, 16);
                let tail: Vec<f32> = peaks.iter().step_by(4).copied().collect();
                eprintln!("    byte {byte}: {tail:?}");
            }
        }
    }

    /// Dev tool (diagnostic): full engine-path repro — Dexed as the track
    /// instrument inside the node graph, notes delivered by the sequencer
    /// (timeline playback), observing sustain, the per-note off, and the
    /// transport Stop behavior. Run with:
    /// `cargo test -p glitcherz-clap-host dexed_engine_playback_repro -- --ignored --nocapture`
    #[test]
    #[ignore = "dev tool: needs Dexed installed + imported presets/"]
    fn dexed_engine_playback_repro() {
        use glitcherz_engine::realtime::Feedback;
        use glitcherz_engine::realtime::block::BlockRunner;
        use glitcherz_engine::realtime::commands::{
            EngineCommand, SequencerCommand, command_channel,
        };
        use glitcherz_engine::realtime::graph::GraphCompiler;
        use glitcherz_engine::realtime::sequencer::{RtMidiClip, RtNote, Sequencer};
        use glitcherz_engine::realtime::voice::{recycle_channel, structure_channel};

        let sr = 48_000.0f32;
        let fixed_block = 128usize;

        let Some(path) = scan_clap_files().into_iter().find(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().to_lowercase().contains("dexed"))
                .unwrap_or(false)
        }) else {
            panic!("no dexed*.clap found");
        };
        let infos = list_clap_plugins_in(&path);
        let info = infos
            .iter()
            .find(|info| info.is_instrument)
            .expect("no instrument class");
        let mut instance = ClapInstance::spawn_rt(
            &path,
            &info.id,
            ClapInstanceConfig {
                sample_rate: 48_000,
                max_frames: 128,
            },
        )
        .expect("spawn");
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let preset = workspace
            .join("presets")
            .join(preset_key_for(&path.display().to_string(), &info.id))
            .join("ROM1_BASS_BRASS-07-Bass_08.glzpreset");
        let blob = std::fs::read(&preset).expect("read preset");
        instance.guard.load_state(&blob).expect("load preset");

        // Engine harness (mirrors the block.rs tests).
        let (mut tx, rx) = command_channel(64);
        let (graph_tx, graph_rx) = structure_channel();
        let (recycle_tx, _recycle_rx) = recycle_channel();
        let (audio_trash_tx, _audio_trash_rx) =
            glitcherz_engine::realtime::audio_track::audio_clip_trash_channel();
        let (mut compiler, graph) = GraphCompiler::default_graph(sr, fixed_block);
        let feedback = Feedback::new();
        let sequencer = Sequencer::new(sr);
        let mut runner = BlockRunner::new(
            fixed_block,
            2,
            sr,
            graph,
            sequencer,
            rx,
            graph_rx,
            recycle_tx,
            audio_trash_tx,
            feedback.clone(),
        );

        // Install the Dexed node as track 0's instrument (mirrors the app's
        // engine set_voice path) and keep the default chain → mixer wiring.
        let mut graph_tx = graph_tx;
        let (update, _node_id) = match compiler.set_voice(0, Box::new(instance.node)) {
            Ok(result) => result,
            Err(_handed_back) => panic!("set voice failed"),
        };
        graph_tx.push(update).expect("structure queue");

        // One bar-long note, sequencer velocity 127 (0..1 = 1.0).
        let mut clips = [RtMidiClip::default(); 8];
        clips[0].start_ticks = 0;
        clips[0].span_ticks = 384;
        clips[0].loop_ticks = 384;
        clips[0].push(RtNote::new(0, 384, 60, 0, 127));
        tx.push(EngineCommand::Sequencer(SequencerCommand::SetTrackClips {
            track: 0,
            clips,
        }))
        .expect("queue");

        tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
            .expect("queue");

        // Render 2 s while playing: per-block peak for the first 30 blocks,
        // then per-100 ms for the rest.
        let block = fixed_block;
        let mut rendered = 0usize;
        let mut profile = Vec::new();
        let mut peak_100ms = 0.0f32;
        while rendered < (sr as usize) * 2 {
            let mut out = vec![0.0f32; block * 2];
            runner.render_interleaved(&mut out);
            for s in out.iter().step_by(2) {
                peak_100ms = peak_100ms.max(s.abs());
            }
            if rendered < block * 30 {
                profile.push((peak_100ms * 1e4).round() / 1e4);
                peak_100ms = 0.0;
            } else if rendered % ((sr as usize) / 10) == 0 {
                profile.push((peak_100ms * 1e3).round() / 1e3);
                peak_100ms = 0.0;
            }
            rendered += block;
        }
        eprintln!(
            "== playing 2 s (note 60, vel 127, one bar): peaks (first 30 per-block, then per-100ms)"
        );
        eprintln!("   {profile:?}");

        // Transport Stop: the sequencer's note_off_all sends Off{note:0} —
        // anything still sounding on another pitch must NOT hang.
        tx.push(EngineCommand::Sequencer(SequencerCommand::Stop))
            .expect("queue");
        let mut rendered = 0usize;
        let mut profile = Vec::new();
        let mut peak_100ms = 0.0f32;
        while rendered < (sr as usize) {
            let mut out = vec![0.0f32; block * 2];
            runner.render_interleaved(&mut out);
            for s in out.iter().step_by(2) {
                peak_100ms = peak_100ms.max(s.abs());
            }
            if rendered % ((sr as usize) / 10) == 0 {
                profile.push((peak_100ms * 1e3).round() / 1e3);
                peak_100ms = 0.0;
            }
            rendered += block;
        }
        eprintln!("== after Stop, 1 s: per-100ms peaks (hanging note check)");
        eprintln!("   {profile:?}");

        // Hanging-note check done properly: play 1 s (note still sounding),
        // then Stop mid-note (the Stop path's note_off_all only sends
        // Off{note:0} — a sounding note 60 must NOT survive it).
        tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
            .expect("queue");
        let mut rendered = 0usize;
        let mut block_peaks = Vec::new();
        while rendered < fixed_block * 20 {
            let mut out = vec![0.0f32; fixed_block * 2];
            runner.render_interleaved(&mut out);
            let peak = out.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
            block_peaks.push((peak * 1e4).round() / 1e4);
            rendered += fixed_block;
        }
        eprintln!("== mid-note Stop section: playing peaks (20 blocks)");
        eprintln!("   {block_peaks:?}");
        for _ in 0..((sr as usize) / fixed_block - 20) {
            let mut out = vec![0.0f32; fixed_block * 2];
            runner.render_interleaved(&mut out);
        }
        tx.push(EngineCommand::Sequencer(SequencerCommand::Stop))
            .expect("queue");
        let mut rendered = 0usize;
        let mut block_peaks = Vec::new();
        while rendered < fixed_block * 40 {
            let mut out = vec![0.0f32; fixed_block * 2];
            runner.render_interleaved(&mut out);
            let peak = out.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
            block_peaks.push((peak * 1e4).round() / 1e4);
            rendered += fixed_block;
        }
        eprintln!("== stop mid-note: per-block peaks after Stop (40 blocks, hanging check)");
        eprintln!("   {block_peaks:?}");

        // Chord regression: two simultaneous long notes on one track must both
        // release. A single per-track pending-off slot used to orphan the first
        // note, leaving Dexed's voice sounding forever — heard as a stuck /
        // arpeggiated drone that a blanket `Off { note: 0 }` cannot clear.
        let mut chords = [RtMidiClip::default(); 8];
        chords[0].start_ticks = 0;
        chords[0].span_ticks = 384;
        chords[0].loop_ticks = 384;
        chords[0].push(RtNote::new(0, 384, 58, 0, 100)); // Bb3
        chords[0].push(RtNote::new(0, 384, 53, 0, 100)); // F3
        tx.push(EngineCommand::Sequencer(SequencerCommand::SetTrackClips {
            track: 0,
            clips: chords,
        }))
        .expect("queue");
        tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
            .expect("queue");
        let mut held_peak = 0.0f32;
        for _ in 0..((sr as usize) / (fixed_block * 2)) {
            let mut out = vec![0.0f32; fixed_block * 2];
            runner.render_interleaved(&mut out);
            for s in out.iter().step_by(2) {
                held_peak = held_peak.max(s.abs());
            }
        }
        tx.push(EngineCommand::Sequencer(SequencerCommand::Stop))
            .expect("queue");
        let mut tail_peak = 0.0f32;
        for _ in 0..((sr as usize) / fixed_block) {
            let mut out = vec![0.0f32; fixed_block * 2];
            runner.render_interleaved(&mut out);
            for s in out.iter().step_by(2) {
                tail_peak = tail_peak.max(s.abs());
            }
        }
        eprintln!(
            "== chord (Bb3+F3) held peak {held_peak:.4}, after-Stop tail peak {tail_peak:.4}"
        );
        assert!(
            held_peak > 1e-3,
            "chord produced no sound (peak {held_peak})"
        );
        assert!(
            tail_peak < 1e-3,
            "a chord voice hung after Stop (tail peak {tail_peak})"
        );
    }

    /// Dev tool (diagnostic): the app's live preset load calls
    /// `ClapInstanceGuard::load_state` on the **UI thread while the audio
    /// thread keeps processing the node**. Three sections: (A) non-gated
    /// loads during processing (race reproduction), (B) gated loads (the
    /// `StateGate` fix), (C) clean control. Every race ends on the bass blob
    /// so any divergence from the control is race damage, not patch
    /// character. Run with:
    /// `cargo test -p glitcherz-clap-host dexed_live_load_race -- --ignored --nocapture`
    #[test]
    #[ignore = "dev tool: needs Dexed installed + imported presets/"]
    fn dexed_live_load_race() {
        let Some(path) = scan_clap_files().into_iter().find(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().to_lowercase().contains("dexed"))
                .unwrap_or(false)
        }) else {
            panic!("no dexed*.clap found");
        };
        let infos = list_clap_plugins_in(&path);
        let info = infos
            .iter()
            .find(|info| info.is_instrument)
            .expect("no instrument class");
        let mut instance = ClapInstance::spawn_rt(
            &path,
            &info.id,
            ClapInstanceConfig {
                sample_rate: 48_000,
                max_frames: 128,
            },
        )
        .expect("spawn");
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let preset_dir = workspace
            .join("presets")
            .join(preset_key_for(&path.display().to_string(), &info.id));
        let bass_blob = std::fs::read(preset_dir.join("ROM1_BASS_BRASS-07-Bass_08.glzpreset"))
            .expect("read preset");
        let morph_blob =
            std::fs::read(preset_dir.join("ROM3_MORPH-00-Morph_01.glzpreset")).expect("read");

        let ctx = ProcessContext {
            sample_rate: 48_000.0,
            frames: 128,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        };
        let mut buf = AudioBuffer::new(2, 128);
        buf.set_len(128);
        buf.clear();

        // Clean state first: load at spawn (before any processing), verify
        // the note sustains.
        instance.guard.load_state(&bass_blob).expect("load bass");
        instance.node.process(&ctx, &mut buf); // warm-up
        let sustain = |instance: &mut ClapInstance, ctx: &ProcessContext, buf: &mut AudioBuffer| {
            let _ = instance.node.note_event(NoteEvent::On {
                note: 60,
                velocity: 1.0,
            });
            let mut peaks = Vec::with_capacity(16);
            for _ in 0..16 {
                instance.node.process(ctx, buf);
                let peak = buf
                    .channel(0)
                    .unwrap()
                    .iter()
                    .fold(0.0f32, |acc, s| acc.max(s.abs()));
                peaks.push((peak * 1e4).round() / 1e4);
            }
            let _ = instance.node.note_event(NoteEvent::Off { note: 60 });
            for _ in 0..8 {
                instance.node.process(ctx, buf);
            }
            peaks
        };
        eprintln!(
            "== clean load (before processing): {:#?}",
            sustain(&mut instance, &ctx, &mut buf)
        );

        // Race sections: audio thread processes while the "UI thread" switches
        // presets live (exactly the app's load_voice_preset path). Every race
        // ends on the BASS blob so the final state is the same as the clean
        // control — any divergence is race damage, not patch character.
        fn race_loads(
            instance: &mut ClapInstance,
            ctx: &ProcessContext,
            buf: &mut AudioBuffer,
            gated: bool,
            morph: &[u8],
            bass: &[u8],
        ) {
            use std::thread::scope;
            let ClapInstance { guard, node } = instance;
            scope(|s| {
                let audio = s.spawn(|| {
                    for _ in 0..(48_000 / 128) {
                        node.process(ctx, buf);
                    }
                });
                for _ in 0..10 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    let _ = if gated {
                        guard.load_state(morph)
                    } else {
                        guard.load_state_impl(morph)
                    };
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    let _ = if gated {
                        guard.load_state(bass)
                    } else {
                        guard.load_state_impl(bass)
                    };
                }
                let _ = audio.join();
            });
        }

        // A. NON-gated loads (guard's raw impl, bypassing the state gate):
        //    if the state corrupts here, the race is real.
        race_loads(
            &mut instance,
            &ctx,
            &mut buf,
            false,
            &morph_blob,
            &bass_blob,
        );
        instance.node.process(&ctx, &mut buf); // warm-up
        eprintln!("== A: 10x NON-gated loads during processing (ends on bass):");
        eprintln!("   {:#?}", sustain(&mut instance, &ctx, &mut buf));

        // B. Gated loads (the fixed path): must match the clean control.
        race_loads(&mut instance, &ctx, &mut buf, true, &morph_blob, &bass_blob);
        instance.node.process(&ctx, &mut buf); // warm-up
        eprintln!("== B: 10x GATED loads during processing (ends on bass):");
        eprintln!("   {:#?}", sustain(&mut instance, &ctx, &mut buf));

        // C. Control: clean reload, no audio thread.
        instance.guard.load_state(&bass_blob).expect("reload");
        instance.node.process(&ctx, &mut buf); // warm-up
        eprintln!("== C: clean reload (no audio thread):");
        eprintln!("   {:#?}", sustain(&mut instance, &ctx, &mut buf));
    }
}
