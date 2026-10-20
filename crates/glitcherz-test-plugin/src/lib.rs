//! GlitcherZ test CLAP plugin: monophonic sine instrument (with one
//! automatable parameter, a state extension, and a fake GUI extension).
//!
//! Purpose: end-to-end verification of GlitcherZ's CLAP host — plugin discovery,
//! instantiation, note event bridging, parameter enumeration, state
//! save/load round-trip, and the GUI open/close protocol. The
//! implementation is deliberately minimal: a single voice (the last NoteOn
//! wins), exposing one Amplitude parameter (ID 1, 0..1, default 0.5); the
//! state extension persists the amplitude as 8 LE bytes.
//!
//! The fake GUI never opens a real window: every gui extension call sets a
//! bit in a call bitmask and (for destroy) a call counter, both readable by
//! the host through read-only parameters (IDs 2/3) so protocol tests can
//! assert exact call sequences from the host process. Three host-callback
//! paths are exercised: `set_parent` requests a resize, `show` requests a
//! show, and `hide` simulates an in-GUI parameter edit (the plugin changes
//! its amplitude and calls `request_flush`, so the host's flush read-back
//! can be verified). A negative `set_scale` simulates the plugin losing its
//! GUI (`closed(destroyed=true)` + failing call).

use std::ffi::CStr;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use clack_common::events::Match;
use clack_common::events::Pckn;
use clack_common::events::event_types::{NoteOffEvent, NoteOnEvent, ParamValueEvent};
use clack_common::stream::{InputStream, OutputStream};
use clack_common::utils::Cookie;
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, HostGui, PluginGui, PluginGuiImpl, Window,
};
use clack_extensions::params::{
    HostParams, ParamDisplayWriter, ParamInfo, ParamInfoFlags, ParamInfoWriter,
    PluginAudioProcessorParams, PluginMainThreadParams, PluginParams,
};
use clack_extensions::state::{PluginState, PluginStateImpl};
use clack_plugin::clack_export_entry;
use clack_plugin::extensions::PluginExtensions;
use clack_plugin::plugin::features::{INSTRUMENT, STEREO};
use clack_plugin::prelude::*;

/// Fixed ID of the Amplitude parameter.
const AMPLITUDE_PARAM_ID: u32 = 1;
/// Default value of the Amplitude parameter.
const AMPLITUDE_DEFAULT: f64 = 0.5;
/// Fixed ID of the read-only GuiCallMask parameter (bitmask of the gui
/// extension calls observed so far; see the `GUI_CALL_*` bits).
const GUI_MASK_PARAM_ID: u32 = 2;
/// Fixed ID of the read-only GuiDestroyCount parameter (how many times the
/// gui destroy call ran — lets host tests verify close idempotency).
const GUI_DESTROY_COUNT_PARAM_ID: u32 = 3;

/// GUI call observation bits (mirrored in `glitcherz-clap-host` tests as
/// `GUI_CALL_*`).
pub const GUI_CALL_CREATE: u32 = 1 << 0;
pub const GUI_CALL_DESTROY: u32 = 1 << 1;
pub const GUI_CALL_SET_SCALE: u32 = 1 << 2;
pub const GUI_CALL_GET_SIZE: u32 = 1 << 3;
pub const GUI_CALL_SET_PARENT: u32 = 1 << 4;
pub const GUI_CALL_SHOW: u32 = 1 << 5;
pub const GUI_CALL_HIDE: u32 = 1 << 6;
pub const GUI_CALL_SET_SIZE: u32 = 1 << 7;
pub const GUI_CALL_ADJUST_SIZE: u32 = 1 << 8;

/// Size reported by the fake GUI.
pub const GUI_FAKE_SIZE: GuiSize = GuiSize {
    width: 320,
    height: 200,
};
/// Size the fake GUI requests from the host during `set_parent` (exercises
/// the host's `request_resize` callback).
pub const GUI_FAKE_REQUESTED_SIZE: GuiSize = GuiSize {
    width: 240,
    height: 160,
};
/// Amplitude the fake GUI "edit hook" sets (see `hide`): exercises the
/// host's `request_flush` → `flush` read-back path.
pub const GUI_EDIT_AMPLITUDE: f64 = 0.75;

/// Plugin shared state (main thread + audio thread): amplitude, stored
/// atomically as an f64 bit pattern, plus the gui call observation state
/// and the pending-GUI-edit flag the flush read-back consumes.
pub struct SineShared {
    amplitude_bits: AtomicU64,
    /// Bitmask of gui extension calls observed so far.
    gui_calls: AtomicU32,
    /// Number of gui destroy calls (idempotency checks).
    gui_destroy_count: AtomicU32,
    /// Whether the GUI changed the amplitude since the last flush reported
    /// it (the flush callback writes the pending edit into its out queue
    /// and takes this flag).
    gui_param_dirty: AtomicBool,
}

impl PluginShared<'_> for SineShared {}

impl SineShared {
    fn amplitude(&self) -> f64 {
        f64::from_bits(self.amplitude_bits.load(Ordering::Relaxed))
    }

    fn set_amplitude(&self, value: f64) {
        let clamped = value.clamp(0.0, 1.0);
        self.amplitude_bits
            .store(clamped.to_bits(), Ordering::Relaxed);
    }

    /// A GUI-side edit: moves the value and marks it pending for the next
    /// flush (the host's `request_flush` → `flush` read-back path).
    fn set_amplitude_from_gui(&self, value: f64) {
        self.set_amplitude(value);
        self.gui_param_dirty.store(true, Ordering::Relaxed);
    }

    fn take_gui_param_dirty(&self) -> bool {
        self.gui_param_dirty.swap(false, Ordering::Relaxed)
    }

    /// Reports a pending GUI edit through a flush out queue (shared by the
    /// main-thread and audio-processor flush implementations — the clack
    /// wrapper dispatches to the audio one while the plugin is active).
    fn report_pending_gui_edit(&self, out: &mut OutputEvents) {
        if self.take_gui_param_dirty() {
            out.try_push(ParamValueEvent::new(
                0,
                ClapId::new(AMPLITUDE_PARAM_ID),
                Pckn::new(0u16, 0u16, 0u16, 0u32),
                self.amplitude(),
            ))
            .ok();
        }
    }

    fn mark_gui(&self, bit: u32) {
        self.gui_calls.fetch_or(bit, Ordering::Relaxed);
    }

    fn gui_mask(&self) -> u32 {
        self.gui_calls.load(Ordering::Relaxed)
    }

    fn gui_destroy_count(&self) -> u32 {
        self.gui_destroy_count.load(Ordering::Relaxed)
    }

    fn request_host_resize(&self, host: &HostMainThreadHandle<'_>, size: GuiSize) {
        if let Some(gui) = host.shared().get_extension::<HostGui>() {
            gui.request_resize(&host.shared(), size.width, size.height)
                .ok();
        }
    }

    fn request_host_show(&self, host: &HostMainThreadHandle<'_>) {
        if let Some(gui) = host.shared().get_extension::<HostGui>() {
            gui.request_show(&host.shared()).ok();
        }
    }

    fn report_host_closed(&self, host: &HostMainThreadHandle<'_>) {
        if let Some(gui) = host.shared().get_extension::<HostGui>() {
            gui.closed(&host.shared(), true);
        }
    }

    fn request_host_flush(&self, host: &HostMainThreadHandle<'_>) {
        if let Some(params) = host.shared().get_extension::<HostParams>() {
            params.request_flush(&host.shared());
        }
    }
}

/// Audio-thread side: monophonic sine synthesis, amplitude read from shared
/// state.
pub struct SineAudio<'a> {
    sample_rate: f32,
    phase: f32,
    note_frequency: f32,
    gate: bool,
    shared: &'a SineShared,
}

impl<'a> PluginAudioProcessor<'a, SineShared, SineMainThread<'a>> for SineAudio<'a> {
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &SineMainThread<'a>,
        shared: &'a SineShared,
        audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        Ok(Self {
            sample_rate: audio_config.sample_rate as f32,
            phase: 0.0,
            note_frequency: 440.0,
            gate: false,
            shared,
        })
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        // 1. Handle input events (block-granularity simplification: applied
        //    at the same block start).
        for event in events.input.iter() {
            if let Some(note_on) = event.as_event::<NoteOnEvent>() {
                let key = match note_on.key() {
                    Match::Specific(key) => i32::from(key),
                    _ => 69,
                };
                let semitones = f64::from(key) - 69.0;
                self.note_frequency = (440.0 * 2.0f64.powf(semitones / 12.0)) as f32;
                self.gate = true;
            } else if event.as_event::<NoteOffEvent>().is_some() {
                self.gate = false;
            } else if let Some(param) = event.as_event::<ParamValueEvent>() {
                if param.param_id().map(|id| id.get()) == Some(AMPLITUDE_PARAM_ID) {
                    self.shared.set_amplitude(param.value());
                }
            }
        }

        // 2. Render all output ports (silence without a note). Phase is
        //    computed statelessly per frame, advancing only once per frame
        //    (independent of channel traversal order).
        let frames = audio.frames_count() as usize;
        let amplitude = if self.gate {
            self.shared.amplitude() as f32
        } else {
            0.0
        };
        let increment = self.note_frequency / self.sample_rate;
        let start_phase = self.phase;

        for mut port_pair in &mut audio {
            let Ok(sample_type) = port_pair.channels() else {
                continue;
            };
            let Some(channels) = sample_type.into_f32() else {
                continue;
            };
            for channel_pair in channels {
                // OutputOnly/InputOutput/InPlace all destructure the output
                // slice directly.
                let buffer = match channel_pair {
                    ChannelPair::OutputOnly(buffer) => buffer,
                    ChannelPair::InputOutput(_, buffer) => buffer,
                    ChannelPair::InPlace(buffer) => buffer,
                    ChannelPair::InputOnly(_) => continue,
                };
                for (sample_index, slot) in buffer.iter_mut().enumerate() {
                    if sample_index >= frames {
                        break;
                    }
                    let phase = start_phase + increment * sample_index as f32;
                    *slot = (core::f32::consts::TAU * phase).sin() * amplitude;
                }
            }
        }

        self.phase = (start_phase + increment * frames as f32).fract();
        Ok(ProcessStatus::Continue)
    }
}

impl PluginAudioProcessorParams for SineAudio<'_> {
    /// Parameter sync while inactive: writes input events into shared
    /// state, and reports pending GUI edits through the out queue.
    fn flush(
        &mut self,
        input_parameter_changes: &InputEvents,
        output_parameter_changes: &mut OutputEvents,
    ) {
        for event in input_parameter_changes.iter() {
            if let Some(param) = event.as_event::<ParamValueEvent>() {
                if param.param_id().map(|id| id.get()) == Some(AMPLITUDE_PARAM_ID) {
                    self.shared.set_amplitude(param.value());
                }
            }
        }
        self.shared
            .report_pending_gui_edit(output_parameter_changes);
    }
}

/// Main-thread side: parameter metadata, value queries, state persistence
/// and the fake GUI (CLAP params/state/gui extension implementations).
pub struct SineMainThread<'a> {
    shared: &'a SineShared,
    host: HostMainThreadHandle<'a>,
}

impl<'a> PluginMainThread<'a, SineShared> for SineMainThread<'a> {}

impl PluginMainThreadParams for SineMainThread<'_> {
    fn count(&self) -> u32 {
        3
    }

    fn get_info(&self, param_index: u32, info: &mut ParamInfoWriter) {
        match param_index {
            0 => info.set(&ParamInfo {
                id: ClapId::new(AMPLITUDE_PARAM_ID),
                flags: ParamInfoFlags::IS_AUTOMATABLE,
                cookie: Cookie::empty(),
                name: b"Amplitude",
                module: b"",
                min_value: 0.0,
                max_value: 1.0,
                default_value: AMPLITUDE_DEFAULT,
            }),
            1 => info.set(&ParamInfo {
                id: ClapId::new(GUI_MASK_PARAM_ID),
                flags: ParamInfoFlags::IS_READONLY | ParamInfoFlags::IS_STEPPED,
                cookie: Cookie::empty(),
                name: b"GuiCallMask",
                module: b"",
                min_value: 0.0,
                max_value: 1023.0,
                default_value: 0.0,
            }),
            2 => info.set(&ParamInfo {
                id: ClapId::new(GUI_DESTROY_COUNT_PARAM_ID),
                flags: ParamInfoFlags::IS_READONLY | ParamInfoFlags::IS_STEPPED,
                cookie: Cookie::empty(),
                name: b"GuiDestroyCount",
                module: b"",
                min_value: 0.0,
                max_value: 1000.0,
                default_value: 0.0,
            }),
            _ => {}
        }
    }

    fn get_value(&self, param_id: ClapId) -> Option<f64> {
        match param_id.get() {
            AMPLITUDE_PARAM_ID => Some(self.shared.amplitude()),
            GUI_MASK_PARAM_ID => Some(f64::from(self.shared.gui_mask())),
            GUI_DESTROY_COUNT_PARAM_ID => Some(f64::from(self.shared.gui_destroy_count())),
            _ => None,
        }
    }

    fn value_to_text(
        &self,
        _param_id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> core::fmt::Result {
        use core::fmt::Write;
        write!(writer, "{value:.3}")
    }

    fn text_to_value(&self, _param_id: ClapId, text: &CStr) -> Option<f64> {
        text.to_str().ok()?.trim().parse::<f64>().ok()
    }

    fn flush(
        &self,
        input_parameter_changes: &InputEvents,
        output_parameter_changes: &mut OutputEvents,
    ) {
        for event in input_parameter_changes.iter() {
            if let Some(param) = event.as_event::<ParamValueEvent>() {
                if param.param_id().map(|id| id.get()) == Some(AMPLITUDE_PARAM_ID) {
                    self.shared.set_amplitude(param.value());
                }
            }
        }
        // Report pending GUI edits: the host called flush in response to
        // our request_flush, so hand the changed value back through the out
        // queue (the read-back path real GUIs use).
        self.shared
            .report_pending_gui_edit(output_parameter_changes);
    }
}

impl PluginStateImpl for SineMainThread<'_> {
    /// Persists the amplitude as 8 little-endian bytes (f64 bit pattern).
    fn save(&self, output: &mut OutputStream) -> Result<(), PluginError> {
        output
            .write_all(&self.shared.amplitude().to_bits().to_le_bytes())
            .map_err(PluginError::from)
    }

    /// Restores the amplitude from the 8-byte blob.
    fn load(&self, input: &mut InputStream) -> Result<(), PluginError> {
        let mut bits = [0u8; 8];
        input.read_exact(&mut bits).map_err(PluginError::from)?;
        self.shared
            .set_amplitude(f64::from_bits(u64::from_le_bytes(bits)));
        Ok(())
    }
}

impl PluginGuiImpl for SineMainThread<'_> {
    /// Only Win32 embedding is supported (the fake GUI never opens a
    /// window, but the negotiation answers like a Windows plugin).
    fn is_api_supported(&self, configuration: GuiConfiguration) -> bool {
        configuration.api_type == GuiApiType::WIN32 && !configuration.is_floating
    }

    fn get_preferred_api(&self) -> Option<GuiConfiguration<'_>> {
        Some(GuiConfiguration {
            api_type: GuiApiType::WIN32,
            is_floating: false,
        })
    }

    fn create(&self, _configuration: GuiConfiguration) -> Result<(), PluginError> {
        self.shared.mark_gui(GUI_CALL_CREATE);
        Ok(())
    }

    fn destroy(&self) {
        self.shared.mark_gui(GUI_CALL_DESTROY);
        self.shared
            .gui_destroy_count
            .fetch_add(1, Ordering::Relaxed);
    }

    fn set_scale(&self, scale: f64) -> Result<(), PluginError> {
        self.shared.mark_gui(GUI_CALL_SET_SCALE);
        if scale < 0.0 {
            // Test hook: a negative scale simulates the plugin losing its
            // GUI — it reports closed(destroyed) and fails the call.
            self.shared.report_host_closed(&self.host);
            return Err(PluginError::Message("simulated GUI loss"));
        }
        Ok(())
    }

    fn get_size(&self) -> Option<GuiSize> {
        self.shared.mark_gui(GUI_CALL_GET_SIZE);
        Some(GUI_FAKE_SIZE)
    }

    fn can_resize(&self) -> bool {
        true
    }

    fn adjust_size(&self, size: GuiSize) -> Option<GuiSize> {
        self.shared.mark_gui(GUI_CALL_ADJUST_SIZE);
        Some(size)
    }

    fn set_size(&self, _size: GuiSize) -> Result<(), PluginError> {
        self.shared.mark_gui(GUI_CALL_SET_SIZE);
        Ok(())
    }

    fn set_parent(&self, _window: Window) -> Result<(), PluginError> {
        self.shared.mark_gui(GUI_CALL_SET_PARENT);
        // Exercise the host's request_resize callback path.
        self.shared
            .request_host_resize(&self.host, GUI_FAKE_REQUESTED_SIZE);
        Ok(())
    }

    fn set_transient(&self, _window: Window) -> Result<(), PluginError> {
        Err(PluginError::Message("floating windows are not supported"))
    }

    fn show(&self) -> Result<(), PluginError> {
        self.shared.mark_gui(GUI_CALL_SHOW);
        // Exercise the host's request_show callback path.
        self.shared.request_host_show(&self.host);
        Ok(())
    }

    fn hide(&self) -> Result<(), PluginError> {
        self.shared.mark_gui(GUI_CALL_HIDE);
        // Test hook: simulates the user editing a parameter inside the GUI —
        // the plugin changes the value on its main thread and asks the host
        // to flush (the host then reads the change back via the params
        // extension's flush callback).
        self.shared.set_amplitude_from_gui(GUI_EDIT_AMPLITUDE);
        self.shared.request_host_flush(&self.host);
        Ok(())
    }
}

pub struct SinePlugin;

impl Plugin for SinePlugin {
    type AudioProcessor<'a> = SineAudio<'a>;
    type Shared<'a> = SineShared;
    type MainThread<'a> = SineMainThread<'a>;

    fn declare_extensions(
        builder: &mut PluginExtensions<Self>,
        _shared: Option<&Self::Shared<'_>>,
    ) {
        builder.register::<PluginParams>();
        builder.register::<PluginState>();
        builder.register::<PluginGui>();
    }
}

impl DefaultPluginFactory for SinePlugin {
    fn get_descriptor() -> PluginDescriptor {
        PluginDescriptor::new("org.glitcherz.sine", "GlitcherZ Sine")
            .with_description("GlitcherZ test sine instrument")
            .with_features([INSTRUMENT, STEREO])
    }

    fn new_shared(_host: HostSharedHandle<'_>) -> Result<Self::Shared<'_>, PluginError> {
        Ok(SineShared {
            amplitude_bits: AtomicU64::new(AMPLITUDE_DEFAULT.to_bits()),
            gui_calls: AtomicU32::new(0),
            gui_destroy_count: AtomicU32::new(0),
            gui_param_dirty: AtomicBool::new(false),
        })
    }

    fn new_main_thread<'a>(
        host: HostMainThreadHandle<'a>,
        shared: &'a Self::Shared<'a>,
    ) -> Result<Self::MainThread<'a>, PluginError> {
        Ok(SineMainThread { shared, host })
    }
}

clack_export_entry!(SinglePluginEntry<SinePlugin>);
