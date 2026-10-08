//! # resoz-clap-host
//!
//! CLAP plugin host adaptation layer (non-real-time infrastructure): the
//! bridge between the ResoZ engine and the CLAP plugin ecosystem.
//!
//! Responsibilities:
//! - Scans CLAP plugin directories (standard directories + the
//!   `RESOZ_CLAP_PATH` environment variable);
//! - Enumerates plugin descriptions (ID / name / whether it is an
//!   instrument);
//! - Instantiates plugins on a **non-real-time thread**, producing a
//!   [`ClapNode`] that can run in the audio callback (the real-time iron
//!   rules apply to its `process` path);
//! - Enumerates the parameter list of a plugin instance
//!   ([`ClapInstance::list_params`], direct calls via the params extension,
//!   see the `raw` module).
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

/// Maximum number of bridging events queued per `process` (notes +
/// parameters combined).
pub const MAX_PENDING_EVENTS: usize = 32;

/// Name of the environment variable holding the plugin scan directories
/// (`PATH`-style separated list).
pub const CLAP_PATH_ENV: &str = "RESOZ_CLAP_PATH";

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

/// All search directories: `RESOZ_CLAP_PATH` first, then the platform
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
        // Simplified host: when the plugin requests a flush, the next process
        // carries all stashed events.
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

/// Guard holding the plugin dynamic library and instance. **Must live as
/// long as the [`ClapNode`]**: dropping the guard unloads the plugin dynamic
/// library, leaving the node's processing pointers dangling.
pub struct ClapInstanceGuard {
    _entry: PluginEntry,
    _instance: PluginInstance<ResoZHostHandlers>,
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
        // Simplified host: activated on the control thread (no thread-check
        // enforcement registered, consistent with most simple hosts);
        // processing must be started via as_started_mut before process.
        let processor: PluginAudioProcessor<ResoZHostHandlers> = stopped.into();

        Ok(Self {
            guard: ClapInstanceGuard {
                _entry: entry,
                _instance: instance,
            },
            node: ClapNode::new(processor, config),
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
            return false; // dropped when full (non-critical path)
        }
        self.pending[self.pending_count] = event;
        self.pending_count += 1;
        true
    }
}

impl Processor for ClapNode {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
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

    /// Dynamic library path of the test plugin (`cargo test --workspace`
    /// builds the cdylib first).
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
            eprintln!("skipped: test plugin cdylib not built (use cargo test --workspace)");
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
        .expect("failed to instantiate the test plugin");
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
            .find(|info| info.id == "org.resoz.sine")
            .expect("the test plugin should be enumerated");
        assert!(
            sine.is_instrument,
            "the sine plugin should carry the instrument feature"
        );
        assert_eq!(sine.name, "ResoZ Sine");
    }

    #[test]
    fn lists_plugin_params() {
        let Some((instance, _ctx)) = spawn_test_plugin() else {
            return;
        };
        let params = instance.list_params();
        assert_eq!(
            params.len(),
            1,
            "the test plugin should expose one parameter"
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
    fn invalid_plugin_fails_gracefully() {
        let bogus = std::env::temp_dir().join("resoz_bogus.clap");
        std::fs::write(&bogus, b"not a dll").expect("write failed");
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
