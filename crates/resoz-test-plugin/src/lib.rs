//! ResoZ 测试用 CLAP 插件：单音正弦乐器。
//!
//! 用途：端到端验证 ResoZ 的 CLAP 宿主——插件发现、实例化与音符事件
//! 桥接。实现刻意保持最小：单声部（最后一个 NoteOn 获胜），无参数。
//! （参数化插件 fixture 待 UI 选择器一并补齐。）

use clack_common::events::Match;
use clack_common::events::event_types::{NoteOffEvent, NoteOnEvent};
use clack_plugin::clack_export_entry;
use clack_plugin::plugin::features::{INSTRUMENT, STEREO};
use clack_plugin::prelude::*;

/// 音频线程侧：单声部正弦合成。
pub struct SineAudio {
    sample_rate: f32,
    phase: f32,
    note_frequency: f32,
    gate: bool,
}

impl<'a> PluginAudioProcessor<'a, (), ()> for SineAudio {
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &(),
        _shared: &'a (),
        audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        Ok(Self {
            sample_rate: audio_config.sample_rate as f32,
            phase: 0.0,
            note_frequency: 440.0,
            gate: false,
        })
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        // 1. 处理输入事件（块粒度简化：同一块起点应用）。
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
            }
        }

        // 2. 渲染全部输出端口（无音符时静音）。相位按帧无状态推算，
        //    每帧只推进一次（与声道遍历顺序无关）。
        let frames = audio.frames_count() as usize;
        let amplitude = if self.gate { 0.5 } else { 0.0 };
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
                // OutputOnly/InputOutput/InPlace 均直接解构出输出切片。
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

pub struct SinePlugin;

impl Plugin for SinePlugin {
    type AudioProcessor<'a> = SineAudio;
    type Shared<'a> = ();
    type MainThread<'a> = ();
}

impl DefaultPluginFactory for SinePlugin {
    fn get_descriptor() -> PluginDescriptor {
        PluginDescriptor::new("org.resoz.sine", "ResoZ Sine")
            .with_description("ResoZ 测试用正弦乐器")
            .with_features([INSTRUMENT, STEREO])
    }

    fn new_shared(_host: HostSharedHandle<'_>) -> Result<Self::Shared<'_>, PluginError> {
        Ok(())
    }

    fn new_main_thread<'a>(
        _host: HostMainThreadHandle<'a>,
        _shared: &'a Self::Shared<'a>,
    ) -> Result<Self::MainThread<'a>, PluginError> {
        Ok(())
    }
}

clack_export_entry!(SinglePluginEntry<SinePlugin>);
