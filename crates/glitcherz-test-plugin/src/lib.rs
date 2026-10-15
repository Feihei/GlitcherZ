//! GlitcherZ test CLAP plugin: monophonic sine instrument (with one
//! automatable parameter and a state extension).
//!
//! Purpose: end-to-end verification of GlitcherZ's CLAP host — plugin discovery,
//! instantiation, note event bridging, parameter enumeration and state
//! save/load round-trip. The implementation is deliberately minimal: a single
//! voice (the last NoteOn wins), exposing one Amplitude parameter (ID 1, 0..1,
//! default 0.5); the state extension persists the amplitude as 8 LE bytes.

use std::ffi::CStr;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use clack_common::events::Match;
use clack_common::events::event_types::{NoteOffEvent, NoteOnEvent, ParamValueEvent};
use clack_common::stream::{InputStream, OutputStream};
use clack_common::utils::Cookie;
use clack_extensions::params::{
    ParamDisplayWriter, ParamInfo, ParamInfoFlags, ParamInfoWriter, PluginAudioProcessorParams,
    PluginMainThreadParams, PluginParams,
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

/// Plugin shared state (main thread + audio thread): amplitude, stored
/// atomically as an f64 bit pattern.
pub struct SineShared {
    amplitude_bits: AtomicU64,
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
    /// Parameter sync while inactive: writes input events into shared state.
    fn flush(
        &mut self,
        input_parameter_changes: &InputEvents,
        _output_parameter_changes: &mut OutputEvents,
    ) {
        for event in input_parameter_changes.iter() {
            if let Some(param) = event.as_event::<ParamValueEvent>() {
                if param.param_id().map(|id| id.get()) == Some(AMPLITUDE_PARAM_ID) {
                    self.shared.set_amplitude(param.value());
                }
            }
        }
    }
}

/// Main-thread side: parameter metadata and value queries (CLAP params
/// extension implementation).
pub struct SineMainThread<'a> {
    shared: &'a SineShared,
}

impl<'a> PluginMainThread<'a, SineShared> for SineMainThread<'a> {}

impl PluginMainThreadParams for SineMainThread<'_> {
    fn count(&self) -> u32 {
        1
    }

    fn get_info(&self, param_index: u32, info: &mut ParamInfoWriter) {
        if param_index != 0 {
            return;
        }
        info.set(&ParamInfo {
            id: ClapId::new(AMPLITUDE_PARAM_ID),
            flags: ParamInfoFlags::IS_AUTOMATABLE,
            cookie: Cookie::empty(),
            name: b"Amplitude",
            module: b"",
            min_value: 0.0,
            max_value: 1.0,
            default_value: AMPLITUDE_DEFAULT,
        });
    }

    fn get_value(&self, param_id: ClapId) -> Option<f64> {
        (param_id.get() == AMPLITUDE_PARAM_ID).then(|| self.shared.amplitude())
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
        _output_parameter_changes: &mut OutputEvents,
    ) {
        for event in input_parameter_changes.iter() {
            if let Some(param) = event.as_event::<ParamValueEvent>() {
                if param.param_id().map(|id| id.get()) == Some(AMPLITUDE_PARAM_ID) {
                    self.shared.set_amplitude(param.value());
                }
            }
        }
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
        })
    }

    fn new_main_thread<'a>(
        _host: HostMainThreadHandle<'a>,
        shared: &'a Self::Shared<'a>,
    ) -> Result<Self::MainThread<'a>, PluginError> {
        Ok(SineMainThread { shared })
    }
}

clack_export_entry!(SinglePluginEntry<SinePlugin>);
