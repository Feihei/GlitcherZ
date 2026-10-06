#![forbid(unsafe_code)]

//! # resoz-ui
//!
//! ResoZ 的 egui 界面：走带控制、步进音序器、钢琴卷帘、混音器。
//!
//! 当前（路线图第 6 步）：多轨主窗口 —— 4 条轨道，每轨独立合成器
//! （滤波器/ADSR/延迟）、16 步音序模式或钢琴卷帘片段（可切换）与
//! 混音条（静音/推子/声像/电平表）。UI 持有 `resoz_model::Project`
//! 文档并把它当作参数面板状态；所有改动经
//! [`resoz_engine::realtime::EngineCommand`] 轨道定向命令实时送往
//! 音频线程。
//!
//! 分层说明：本 crate 直接依赖 engine 的非实时控制端——`AudioEngine`
//! 只在 UI 线程被触碰，永不进入音频回调。模型与引擎互不依赖，发送前
//! 在此完成文档 → 引擎表示的转换。

use std::collections::HashSet;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

use resoz_engine::AudioEngine;
use resoz_engine::convert;
use resoz_engine::realtime::{
    EngineCommand, MAX_CLIP_NOTES, NoteEvent, ParamId, RtClip, RtNote, SequencerCommand,
    SequencerStep, TrackSource,
};
use resoz_model::{
    AutoParam, Automation, PATTERN_STEPS, Pattern, Project, TICKS_PER_BAR,
    TrackSource as ModelTrackSource,
};

pub mod lane;
pub mod piano_roll;

/// 滤波器截止频率滑块范围（对数刻度）。
pub const CUTOFF_RANGE: RangeInclusive<f32> = 20.0..=18_000.0;

/// 共振滑块范围（Simper SVF res）。
pub const RES_RANGE: RangeInclusive<f32> = 0.0..=0.98;

/// Attack / Decay 滑块范围（秒）。
pub const AD_RANGE: RangeInclusive<f32> = 0.0..=2.0;

/// Sustain 滑块范围（相对电平）。
pub const S_RANGE: RangeInclusive<f32> = 0.0..=1.0;

/// Release 滑块范围（秒）。
pub const R_RANGE: RangeInclusive<f32> = 0.0..=3.0;

/// 延迟时间滑块范围（秒）。
pub const DELAY_TIME_RANGE: RangeInclusive<f32> = 0.005..=1.0;

/// 延迟反馈滑块范围。
pub const DELAY_FB_RANGE: RangeInclusive<f32> = 0.0..=0.9;

/// 延迟湿声滑块范围。
pub const DELAY_MIX_RANGE: RangeInclusive<f32> = 0.0..=1.0;

/// 轨道推子范围（线性）。
pub const TRACK_GAIN_RANGE: RangeInclusive<f32> = 0.0..=1.5;

/// 轨道声像滑块范围。
pub const PAN_RANGE: RangeInclusive<f32> = -1.0..=1.0;

/// BPM 可调范围（文档中为 f64：时间计算约定）。
pub const BPM_RANGE: RangeInclusive<f64> = 20.0..=300.0;

/// 音长（gate 占步长比例）滑块范围。
pub const GATE_RANGE: RangeInclusive<f32> = 0.05..=1.0;

/// 电脑键盘 → 半音偏移（相对八度基准，FL Studio 风格布局）。
const KEYMAP: &[(egui::Key, u8)] = &[
    (egui::Key::A, 0),
    (egui::Key::W, 1),
    (egui::Key::S, 2),
    (egui::Key::E, 3),
    (egui::Key::D, 4),
    (egui::Key::F, 5),
    (egui::Key::T, 6),
    (egui::Key::G, 7),
    (egui::Key::Y, 8),
    (egui::Key::H, 9),
    (egui::Key::U, 10),
    (egui::Key::J, 11),
    (egui::Key::K, 12),
    (egui::Key::O, 13),
    (egui::Key::L, 14),
    (egui::Key::P, 15),
    (egui::Key::Semicolon, 16),
];

/// 一个八度 13 个键（C 到下一组 C）的黑键半音位置。
const BLACK_SEMIS: [u8; 5] = [1, 3, 6, 8, 10];

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// MIDI 音符编号 → 音名（如 69 → "A4"）。
fn note_name(note: u8) -> String {
    let octave = note as i32 / 12 - 1;
    format!("{}{octave}", NOTE_NAMES[(note % 12) as usize])
}

/// 路径 → 显示名（文件名，失败则整条路径）。
fn display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// UI 启动参数。
#[derive(Debug, Clone, Default)]
pub struct UiConfig {
    /// 初始主增益，会被钳制进 0..1.5。
    pub gain: f64,
    /// 启动时自动触发的音符（演示/冒烟测试用；`None` = 静默启动）。
    pub auto_play_note: Option<u8>,
    /// 启动后自动关闭窗口（冒烟测试用；`None` = 手动关闭）。
    pub auto_close_after: Option<Duration>,
    /// 启动时加载的项目文件（`--open`）。
    pub open_path: Option<PathBuf>,
}

/// 经 eframe 持久化存储（`persistence` feature）保存的 UI 状态。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct UiState {
    /// 最近打开的项目文件（新在前，最多 [`RECENT_FILES_MAX`] 条）。
    recent_files: Vec<PathBuf>,
}

/// 最近文件列表容量上限。
const RECENT_FILES_MAX: usize = 8;

/// eframe 存储中 [`UiState`] 的键名。
const UI_STATE_KEY: &str = "resoz_ui_state";

/// 状态栏消息显示时长。
const STATUS_MESSAGE_TTL: Duration = Duration::from_secs(8);

/// 峰值电平表：峰值保持 600 ms 后回落，视觉上更容易读。
#[derive(Debug)]
struct PeakMeter {
    held: f32,
    hold_until: Instant,
}

impl PeakMeter {
    fn new() -> Self {
        Self {
            held: 0.0,
            hold_until: Instant::now(),
        }
    }

    /// 用引擎最新峰值刷新保持值（每帧调用）。UI 线程专用。
    fn update(&mut self, current: f32) {
        let now = Instant::now();
        if current >= self.held || now >= self.hold_until {
            self.held = current;
            self.hold_until = now + Duration::from_millis(600);
        }
    }
}

/// 主窗口应用。拥有 [`AudioEngine`]：窗口关闭即引擎丢弃、音频流停止。
pub struct ResoZApp {
    engine: AudioEngine,
    /// 项目文档（UI 持有模型状态，见 AGENTS.md「线程模型」）。
    project: Project,
    /// 正在编辑的轨道（合成器/音序器/键盘的目标）。
    selected_track: usize,
    /// 钢琴卷帘的跨帧状态（滚动/拖拽）。
    roll_state: piano_roll::RollState,
    /// 自动化泳道当前编辑的参数。
    lane_param: AutoParam,
    /// 自动化泳道的跨帧状态（拖拽）。
    lane_state: lane::LaneState,
    /// 画笔音符：点选空白步时写入的音符。
    brush_note: u8,
    /// 八度基准（C 的 MIDI 编号，默认 60 = C4）。
    octave_base: u8,
    /// 当前按住的音符（屏幕键 + 电脑键共用，用于高亮与去重）。
    held_notes: HashSet<u8>,
    /// 本帧鼠标按住的屏幕键音符（用于按住/松开的边沿检测）。
    mouse_down_notes: HashSet<u8>,
    /// 最近触发的音符（状态栏显示用）。
    current_note: Option<u8>,
    meter: PeakMeter,
    opened_at: Instant,
    auto_close_after: Option<Duration>,
    /// 当前项目文件路径（`None` = 未保存过，保存时弹对话框）。
    project_path: Option<PathBuf>,
    /// 最近打开的项目文件（持久化）。
    recent: Vec<PathBuf>,
    /// 状态栏消息（文本 + 出现时刻，超时自动消失）。
    status_message: Option<(String, Instant)>,
    /// 期望的窗口标题。
    window_title: String,
    /// 已发送给窗口系统的标题（避免每帧重发命令）。
    title_sent: String,
    /// 上一帧的走带状态（用于检测停止沿 → 恢复手动参数）。
    was_playing: bool,
    /// 诊断标志：`ui()` 是否已进入过（仅日志用）。
    first_frame_logged: bool,
    /// 诊断标志：是否已发出关闭命令（仅日志用）。
    close_sent: bool,
}

impl ResoZApp {
    /// 构造窗口应用，恢复持久化的最近文件，并把整个项目文档的参数
    /// 经命令队列下发（下一个音频块边界生效）。
    pub fn new(cc: &eframe::CreationContext<'_>, engine: AudioEngine, config: UiConfig) -> Self {
        let ui_state: UiState = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, UI_STATE_KEY))
            .unwrap_or_default();
        let project = Project {
            master_gain: config.gain.clamp(0.0, 1.5) as f32,
            ..Project::default()
        };
        let mut app = Self {
            engine,
            project,
            selected_track: 0,
            roll_state: piano_roll::RollState::new(),
            lane_param: AutoParam::TrackGain,
            lane_state: lane::LaneState::default(),
            brush_note: 60,
            octave_base: 60,
            held_notes: HashSet::new(),
            mouse_down_notes: HashSet::new(),
            current_note: None,
            meter: PeakMeter::new(),
            opened_at: Instant::now(),
            auto_close_after: config.auto_close_after,
            project_path: None,
            recent: ui_state.recent_files,
            status_message: None,
            window_title: "ResoZ".to_string(),
            title_sent: String::new(),
            was_playing: false,
            first_frame_logged: false,
            close_sent: false,
        };
        app.push_all_params();
        if let Some(note) = config.auto_play_note {
            app.send_note_on(note, 1.0);
        }
        if let Some(path) = config.open_path {
            app.do_open(path);
        }
        app
    }

    /// 把项目文档的参数完整下发一遍（队列满载时命令被丢弃，可安全忽略）。
    fn push_all_params(&mut self) {
        self.engine.send(EngineCommand::SetParam {
            id: ParamId::MasterGain,
            value: self.project.master_gain,
        });
        self.send_sequencer(SequencerCommand::SetBpm(convert::to_f32(
            self.project.metadata.tempo_bpm,
        )));
        self.send_sequencer(SequencerCommand::SetGate(self.project.gate));
        for track in 0..self.project.tracks.len() {
            self.push_track_params(track);
            let steps = self.rt_pattern(track);
            self.send_sequencer(SequencerCommand::SetPattern { track, steps });
            let clip = self.rt_clip(track);
            self.send_sequencer(SequencerCommand::SetClip { track, clip });
            let source = match self.project.tracks[track].source {
                ModelTrackSource::Steps => TrackSource::Steps,
                ModelTrackSource::Clip => TrackSource::Clip,
            };
            self.send_sequencer(SequencerCommand::SetSource { track, source });
            let curves = self.project.tracks[track].automations.clone();
            for curve in curves {
                self.send_automation(track, &curve);
            }
        }
    }

    /// 下发一条轨道的全部参数（音色 / 延迟 / 混音）。
    fn push_track_params(&mut self, track: usize) {
        let t = &self.project.tracks[track];
        let params = [
            (ParamId::FilterCutoff, t.synth.cutoff_hz),
            (ParamId::FilterResonance, t.synth.resonance),
            (ParamId::FilterMode, t.synth.filter_mode),
            (ParamId::Attack, t.synth.attack_s),
            (ParamId::Decay, t.synth.decay_s),
            (ParamId::Sustain, t.synth.sustain),
            (ParamId::Release, t.synth.release_s),
            (ParamId::DelayTime, t.delay.time_s),
            (ParamId::DelayFeedback, t.delay.feedback),
            (ParamId::DelayMix, t.delay.mix),
            (ParamId::TrackGain, t.gain),
            (ParamId::TrackPan, t.pan),
            (ParamId::TrackMute, if t.muted { 1.0 } else { 0.0 }),
        ];
        for (id, value) in params {
            self.engine
                .send(EngineCommand::SetTrackParam { track, id, value });
        }
    }

    /// 文档片段 → 引擎表示（超容量截断，`Copy` 结构，零堆分配）。
    fn rt_clip(&self, track: usize) -> RtClip {
        let clip = &self.project.tracks[track].clip;
        let mut rt = RtClip {
            loop_ticks: clip.loop_ticks,
            ..RtClip::default()
        };
        for note in clip.notes.iter().take(MAX_CLIP_NOTES) {
            rt.push(RtNote {
                start_ticks: note.start,
                length_ticks: note.length,
                note: note.note,
                velocity: note.velocity,
            });
        }
        rt
    }

    /// 下发一条轨道的当前卷帘片段。
    fn send_clip(&mut self, track: usize) {
        let clip = self.rt_clip(track);
        self.send_sequencer(SequencerCommand::SetClip { track, clip });
    }

    /// 文档自动化曲线 → 引擎表示（超容量截断）并下发。
    fn send_automation(&mut self, track: usize, curve: &Automation) {
        let mut rt = resoz_engine::realtime::RtAutomation {
            param: match curve.param {
                AutoParam::TrackGain => ParamId::TrackGain,
                AutoParam::TrackPan => ParamId::TrackPan,
                AutoParam::FilterCutoff => ParamId::FilterCutoff,
            },
            enabled: curve.enabled,
            ..resoz_engine::realtime::RtAutomation::default()
        };
        for point in curve
            .points
            .iter()
            .take(resoz_engine::realtime::MAX_AUTO_POINTS)
        {
            rt.push(resoz_engine::realtime::RtAutoPoint {
                tick: point.tick,
                value: point.value,
            });
        }
        self.send_sequencer(SequencerCommand::SetAutomation {
            track,
            automation: rt,
        });
    }

    /// 文档模式 → 引擎表示（`Copy` 数组，零堆分配）。
    fn rt_pattern(&self, track: usize) -> [SequencerStep; PATTERN_STEPS] {
        let mut steps = [SequencerStep::default(); PATTERN_STEPS];
        for (i, step) in self.project.tracks[track].pattern.steps.iter().enumerate() {
            steps[i] = SequencerStep {
                enabled: step.enabled,
                note: step.note,
                velocity: step.velocity,
            };
        }
        steps
    }

    /// 保存到当前路径；从未保存过则弹出"另存为"对话框。
    fn save_project(&mut self) {
        match self.project_path.clone() {
            Some(path) => self.do_save(path),
            None => self.save_project_as(),
        }
    }

    /// "另存为"：弹出保存对话框。
    fn save_project_as(&mut self) {
        let Some(mut path) = rfd::FileDialog::new()
            .add_filter("ResoZ 项目", &["rez"])
            .set_file_name("未命名.rez")
            .save_file()
        else {
            return;
        };
        path.set_extension("rez");
        self.do_save(path);
    }

    /// "打开"：弹出文件选择对话框。
    fn open_project_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("ResoZ 项目", &["rez"])
            .pick_file()
        else {
            return;
        };
        self.do_open(path);
    }

    /// 写盘并更新最近文件 / 标题 / 状态消息。
    fn do_save(&mut self, path: PathBuf) {
        match resoz_model::save_project(&self.project, &path) {
            Ok(()) => {
                self.project_path = Some(path.clone());
                self.push_recent(path.clone());
                self.window_title = format!("ResoZ — {}", display_name(&path));
                self.set_message(format!("已保存 {}", path.display()));
                log::info!("项目已保存：{}", path.display());
            }
            Err(err) => {
                self.set_message(format!("保存失败：{err}"));
                log::error!("保存项目失败：{err}");
            }
        }
    }

    /// 读盘、替换文档并把全部参数重新下发音频线程。
    fn do_open(&mut self, path: PathBuf) {
        match resoz_model::load_project(&path) {
            Ok(project) => {
                self.project = project;
                self.project_path = Some(path.clone());
                self.push_recent(path.clone());
                self.selected_track = 0;
                self.roll_state = piano_roll::RollState::new();
                self.push_all_params();
                self.window_title = format!("ResoZ — {}", display_name(&path));
                self.set_message(format!("已打开 {}", path.display()));
                log::info!("项目已打开：{}", path.display());
            }
            Err(err) => {
                self.set_message(format!("打开失败：{err}"));
                log::error!("打开项目失败：{err}");
            }
        }
    }

    /// 最近文件去重置顶并截断。
    fn push_recent(&mut self, path: PathBuf) {
        self.recent.retain(|p| p != &path);
        self.recent.insert(0, path);
        self.recent.truncate(RECENT_FILES_MAX);
    }

    /// 设置一条状态栏消息（超时自动消失）。
    fn set_message(&mut self, text: String) {
        self.status_message = Some((text, Instant::now()));
    }

    fn send_sequencer(&mut self, cmd: SequencerCommand) {
        self.engine.send(EngineCommand::Sequencer(cmd));
    }

    /// 下发一条轨道的当前模式。
    fn send_pattern(&mut self, track: usize) {
        let steps = self.rt_pattern(track);
        self.send_sequencer(SequencerCommand::SetPattern { track, steps });
    }

    fn send_note_on(&mut self, note: u8, velocity: f32) {
        let track = self.selected_track;
        self.engine.send(EngineCommand::Note {
            track,
            event: NoteEvent::On { note, velocity },
        });
        self.held_notes.insert(note);
        self.current_note = Some(note);
    }

    fn send_note_off(&mut self, note: u8) {
        let track = self.selected_track;
        self.engine.send(EngineCommand::Note {
            track,
            event: NoteEvent::Off { note },
        });
        self.held_notes.remove(&note);
    }

    /// 处理电脑键盘演奏（FL Studio 风格布局，Z/X 切八度）。
    ///
    /// TODO：将来窗口里出现文本输入控件时，必须先判断键盘焦点，
    /// 避免打字触发音符。
    fn handle_keyboard_input(&mut self, ctx: &egui::Context) {
        let events: Vec<egui::Event> = ctx.input(|i| i.events.clone());
        for event in events {
            let egui::Event::Key {
                key,
                pressed,
                repeat,
                modifiers,
                ..
            } = event
            else {
                continue;
            };
            if repeat {
                continue;
            }
            // 快捷键优先于演奏映射（Ctrl+S 保存 / Ctrl+O 打开）。
            if modifiers.ctrl && pressed {
                match key {
                    egui::Key::S => {
                        self.save_project();
                        continue;
                    }
                    egui::Key::O => {
                        self.open_project_dialog();
                        continue;
                    }
                    _ => {
                        continue; // 其他 Ctrl 组合不触发音符
                    }
                }
            }
            match (key, pressed) {
                (egui::Key::Z, true) => {
                    self.octave_base = (self.octave_base.saturating_sub(12)).max(24);
                }
                (egui::Key::X, true) => {
                    self.octave_base = (self.octave_base + 12).min(84);
                }
                _ => {
                    if let Some((_, semi)) = KEYMAP.iter().find(|(k, _)| *k == key) {
                        let note = self.octave_base + semi;
                        if pressed {
                            if !self.held_notes.contains(&note) {
                                self.send_note_on(note, 0.9);
                            }
                        } else if self.held_notes.contains(&note) {
                            self.send_note_off(note);
                        }
                    }
                }
            }
        }
    }

    fn transport_panel(&mut self, ui: &mut egui::Ui) {
        let playing = self.engine.is_playing();
        egui::Panel::top("resoz_transport").show(ui, |ui| {
            ui.horizontal(|ui| {
                let mut play_btn = egui::Button::new("▶ 播放");
                if playing {
                    play_btn = play_btn.fill(egui::Color32::DARK_GREEN);
                }
                if ui.add(play_btn).clicked() {
                    self.send_sequencer(SequencerCommand::Play);
                }
                if ui.button("■ 停止").clicked() {
                    self.send_sequencer(SequencerCommand::Stop);
                }
                ui.separator();
                ui.label("速度");
                let bpm = ui.add(
                    egui::DragValue::new(&mut self.project.metadata.tempo_bpm)
                        .range(*BPM_RANGE.start()..=*BPM_RANGE.end())
                        .speed(0.5)
                        .suffix(" BPM"),
                );
                if bpm.changed() {
                    self.send_sequencer(SequencerCommand::SetBpm(convert::to_f32(
                        self.project.metadata.tempo_bpm,
                    )));
                }
                ui.separator();
                if ui.button("📂 打开").clicked() {
                    self.open_project_dialog();
                }
                if ui.button("💾 保存").clicked() {
                    self.save_project();
                }
                ui.menu_button("最近文件", |ui| {
                    if self.recent.is_empty() {
                        ui.weak("（空）");
                    }
                    let recent = self.recent.clone();
                    for path in recent {
                        if ui.button(display_name(&path)).clicked() {
                            self.do_open(path);
                        }
                    }
                });
                ui.separator();
                ui.weak("ResoZ — Rust DAW");
            });
        });
    }

    fn status_panel(&mut self, ui: &mut egui::Ui) {
        let info = self.engine.info().clone();
        let position = self.engine.position_seconds();
        let note_text = self
            .current_note
            .map(note_name)
            .unwrap_or_else(|| "—".to_string());
        let track_name = self
            .project
            .tracks
            .get(self.selected_track)
            .map(|t| {
                if t.name.is_empty() {
                    format!("轨道 {}", self.selected_track + 1)
                } else {
                    t.name.clone()
                }
            })
            .unwrap_or_else(|| "—".to_string());
        // 状态消息（超时自动消失）。
        let message = self
            .status_message
            .take_if(|(_, at)| at.elapsed() < STATUS_MESSAGE_TTL);

        egui::Panel::bottom("resoz_status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("设备：{}", info.device_name));
                ui.separator();
                ui.label(format!(
                    "{} Hz / {} 声道",
                    info.sample_rate, info.out_channels
                ));
                ui.separator();
                ui.label(format!("编辑：{track_name}"));
                ui.separator();
                ui.label(format!("音符：{note_text}"));
                ui.separator();
                ui.label(format!("位置 {:.1} s", position));
                if let Some((text, _)) = message {
                    ui.separator();
                    ui.colored_label(egui::Color32::LIGHT_BLUE, text);
                }
            });
        });
    }

    fn synth_panel(&mut self, ui: &mut egui::Ui) {
        self.meter.update(self.engine.peak_level());
        let peak = self.meter.held;

        // eframe 0.36 的根 Ui 无边距；包一层 CentralPanel 保持默认观感。
        egui::CentralPanel::default().show(ui, |ui| {
            let sel = self.selected_track;
            let track_title = if self.project.tracks[sel].name.is_empty() {
                format!("轨道 {}", sel + 1)
            } else {
                self.project.tracks[sel].name.clone()
            };
            ui.heading(format!("合成器 — {track_title}"));
            ui.weak("正弦波 → ADSR 包络 → SVF 滤波器 → 延迟（每轨独立）—— 改动实时送达音频线程");
            ui.add_space(8.0);

            // 滤波器 / 包络（绑定到选中轨道的文档字段）。
            egui::Grid::new("synth_params")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    ui.label("截止频率");
                    let r = ui.add(
                        egui::Slider::new(
                            &mut self.project.tracks[sel].synth.cutoff_hz,
                            CUTOFF_RANGE,
                        )
                        .logarithmic(true)
                        .text("Hz"),
                    );
                    if r.changed() {
                        self.send_track_param(
                            sel,
                            ParamId::FilterCutoff,
                            self.project.tracks[sel].synth.cutoff_hz,
                        );
                    }
                    ui.end_row();

                    ui.label("共振");
                    let r = ui.add(egui::Slider::new(
                        &mut self.project.tracks[sel].synth.resonance,
                        RES_RANGE,
                    ));
                    if r.changed() {
                        self.send_track_param(
                            sel,
                            ParamId::FilterResonance,
                            self.project.tracks[sel].synth.resonance,
                        );
                    }
                    ui.end_row();

                    ui.label("滤波模式");
                    ui.horizontal(|ui| {
                        for (name, value) in [("低通", 0.0), ("高通", 1.0), ("带通", 2.0)] {
                            let selected =
                                (self.project.tracks[sel].synth.filter_mode - value).abs() < 0.5;
                            if ui.selectable_label(selected, name).clicked() {
                                self.project.tracks[sel].synth.filter_mode = value;
                                self.send_track_param(sel, ParamId::FilterMode, value);
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("音量包络");
                    self.adsr_sliders(ui);
                    ui.end_row();

                    ui.label("延迟");
                    self.delay_sliders(ui);
                    ui.end_row();

                    ui.label("主增益");
                    let r = ui.add(
                        egui::Slider::new(&mut self.project.master_gain, 0.0..=1.5).text("线性"),
                    );
                    if r.changed() {
                        self.engine.send(EngineCommand::SetParam {
                            id: ParamId::MasterGain,
                            value: self.project.master_gain,
                        });
                    }
                    ui.end_row();
                });

            self.sequencer_section(ui);
            self.clip_section(ui);
            self.automation_section(ui);
            self.keyboard_section(ui);

            ui.add_space(10.0);
            ui.label("主输出电平");
            ui.add(
                egui::ProgressBar::new(peak.clamp(0.0, 1.0))
                    .desired_height(16.0)
                    .show_percentage(),
            );
            ui.weak(format!("峰值 {peak:.3} —— AtomicF32 回传，峰值保持 600 ms"));
        });
    }

    /// ADSR 四滑块（本地副本 + 写回，避免借用冲突）。
    fn adsr_sliders(&mut self, ui: &mut egui::Ui) {
        let sel = self.selected_track;
        let mut a = self.project.tracks[sel].synth.attack_s;
        let mut d = self.project.tracks[sel].synth.decay_s;
        let mut s = self.project.tracks[sel].synth.sustain;
        let mut r = self.project.tracks[sel].synth.release_s;
        ui.horizontal(|ui| {
            let sliders = [
                ("A", &mut a, AD_RANGE, ParamId::Attack),
                ("D", &mut d, AD_RANGE, ParamId::Decay),
                ("S", &mut s, S_RANGE, ParamId::Sustain),
                ("R", &mut r, R_RANGE, ParamId::Release),
            ];
            for (label, value, range, id) in sliders {
                let resp = ui.add_sized([104.0, 20.0], egui::Slider::new(value, range).text(label));
                if resp.changed() {
                    self.send_track_param(sel, id, *value);
                }
            }
        });
        self.project.tracks[sel].synth.attack_s = a;
        self.project.tracks[sel].synth.decay_s = d;
        self.project.tracks[sel].synth.sustain = s;
        self.project.tracks[sel].synth.release_s = r;
    }

    /// 延迟三滑块（湿声/时间/反馈）。
    fn delay_sliders(&mut self, ui: &mut egui::Ui) {
        let sel = self.selected_track;
        let mut mix = self.project.tracks[sel].delay.mix;
        let mut time = self.project.tracks[sel].delay.time_s;
        let mut feedback = self.project.tracks[sel].delay.feedback;
        ui.horizontal(|ui| {
            let sliders = [
                ("湿声", &mut mix, DELAY_MIX_RANGE, ParamId::DelayMix),
                ("时间", &mut time, DELAY_TIME_RANGE, ParamId::DelayTime),
                (
                    "反馈",
                    &mut feedback,
                    DELAY_FB_RANGE,
                    ParamId::DelayFeedback,
                ),
            ];
            for (label, value, range, id) in sliders {
                let resp = ui.add_sized([104.0, 20.0], egui::Slider::new(value, range).text(label));
                if resp.changed() {
                    self.send_track_param(sel, id, *value);
                }
            }
        });
        self.project.tracks[sel].delay.mix = mix;
        self.project.tracks[sel].delay.time_s = time;
        self.project.tracks[sel].delay.feedback = feedback;
    }

    /// 发送一条轨道参数到音频线程。
    fn send_track_param(&mut self, track: usize, id: ParamId, value: f32) {
        self.engine
            .send(EngineCommand::SetTrackParam { track, id, value });
    }

    /// 混音器：每轨 选择 / 静音 / 电平表 / 推子 / 声像。
    fn mixer_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.add_space(4.0);
        ui.label("混音器");

        // 先收集编辑，循环后统一写回并发送（避免借用冲突）。
        let num_tracks = self.project.tracks.len();
        let mut mute_edits = [None; 4];
        let mut gain_edits = [None; 4];
        let mut pan_edits = [None; 4];
        let mut select_edits: Option<usize> = None;

        ui.horizontal(|ui| {
            for i in 0..num_tracks.min(4) {
                ui.vertical(|ui| {
                    let name = if self.project.tracks[i].name.is_empty() {
                        format!("轨道 {}", i + 1)
                    } else {
                        self.project.tracks[i].name.clone()
                    };
                    if ui
                        .selectable_label(self.selected_track == i, name)
                        .clicked()
                    {
                        select_edits = Some(i);
                    }

                    let muted = self.project.tracks[i].muted;
                    let mut btn = egui::Button::new(if muted { "静音" } else { "开" });
                    if muted {
                        btn = btn.fill(egui::Color32::from_rgb(140, 60, 50));
                    }
                    if ui.add_sized([72.0, 22.0], btn).clicked() {
                        mute_edits[i] = Some(!muted);
                    }

                    // post-fader 电平条。
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(96.0, 10.0), egui::Sense::hover());
                    ui.painter()
                        .rect_filled(rect, 2.0, egui::Color32::from_gray(40));
                    let level = self.engine.track_level(i).clamp(0.0, 1.0);
                    let filled = egui::Rect::from_min_size(
                        rect.min,
                        egui::vec2(rect.width() * level, rect.height()),
                    );
                    ui.painter()
                        .rect_filled(filled, 2.0, egui::Color32::from_rgb(70, 160, 90));

                    let mut gain = self.project.tracks[i].gain;
                    let fader = ui.add_sized(
                        [96.0, 110.0],
                        egui::Slider::new(&mut gain, TRACK_GAIN_RANGE).vertical(),
                    );
                    if fader.changed() {
                        gain_edits[i] = Some(gain);
                    }

                    let mut pan = self.project.tracks[i].pan;
                    let pan_resp = ui.add_sized(
                        [96.0, 20.0],
                        egui::Slider::new(&mut pan, PAN_RANGE).text("声像"),
                    );
                    if pan_resp.changed() {
                        pan_edits[i] = Some(pan);
                    }
                });
                if i + 1 < num_tracks.min(4) {
                    ui.separator();
                }
            }
        });

        if let Some(i) = select_edits {
            self.selected_track = i;
        }
        for i in 0..num_tracks.min(4) {
            if let Some(muted) = mute_edits[i] {
                self.project.tracks[i].muted = muted;
                self.send_track_param(i, ParamId::TrackMute, if muted { 1.0 } else { 0.0 });
            }
            if let Some(gain) = gain_edits[i] {
                self.project.tracks[i].gain = gain;
                self.send_track_param(i, ParamId::TrackGain, gain);
            }
            if let Some(pan) = pan_edits[i] {
                self.project.tracks[i].pan = pan;
                self.send_track_param(i, ParamId::TrackPan, pan);
            }
        }
    }

    /// 16 步音序器网格（作用于选中轨道）。
    fn sequencer_section(&mut self, ui: &mut egui::Ui) {
        self.mixer_section(ui);
        ui.separator();
        ui.add_space(4.0);
        let sel = self.selected_track;
        ui.horizontal(|ui| {
            ui.label(format!("步进音序器 — 轨道 {}", sel + 1));
            ui.label("画笔音符");
            ui.add(
                egui::DragValue::new(&mut self.brush_note)
                    .range(0..=127)
                    .speed(0.2),
            );
            ui.monospace(note_name(self.brush_note));
            ui.separator();
            ui.label("音长");
            let gate = ui.add(
                egui::Slider::new(
                    &mut self.project.gate,
                    *GATE_RANGE.start()..=*GATE_RANGE.end(),
                )
                .text("%"),
            );
            if gate.changed() {
                self.send_sequencer(SequencerCommand::SetGate(self.project.gate));
            }
            if ui.button("清空").clicked() {
                self.project.tracks[sel].pattern = Pattern::default();
                self.send_pattern(sel);
            }
            if ui.button("全部 C4").clicked() {
                self.project.tracks[sel]
                    .pattern
                    .steps
                    .fill(resoz_model::Step {
                        enabled: true,
                        note: 60,
                        velocity: 0.9,
                    });
                self.send_pattern(sel);
            }
        });

        let playing = self.engine.is_playing();
        let current = self.engine.current_step();
        let mut cells: [Option<egui::Response>; PATTERN_STEPS] = std::array::from_fn(|_| None);
        // 先收集本帧的编辑动作，循环结束后统一写回并发送
        // （避免迭代器借用与 send_pattern 的可变借用冲突）。
        let mut edits: [(bool, bool, u8); PATTERN_STEPS] = [(false, false, 0); PATTERN_STEPS];
        ui.horizontal(|ui| {
            for (i, step) in self.project.tracks[sel].pattern.steps.iter().enumerate() {
                if i > 0 && i % 4 == 0 {
                    ui.separator();
                }
                let label = if step.enabled {
                    note_name(step.note)
                } else {
                    "·".to_string()
                };
                let mut button = egui::Button::new(egui::RichText::new(label).size(11.0));
                if step.enabled {
                    button = button.fill(egui::Color32::from_rgb(64, 122, 82));
                } else if i % 4 == 0 {
                    button = button.fill(egui::Color32::from_gray(58));
                }
                let response = ui.add_sized([36.0, 28.0], button);
                if response.clicked() {
                    if step.enabled && step.note == self.brush_note {
                        // 已是画笔音符的步再点一次 = 取消。
                        edits[i] = (true, false, step.note);
                    } else {
                        // 空步写入画笔音符；已有其他音符的步改写为画笔音符。
                        edits[i] = (true, true, self.brush_note);
                    }
                }
                if response.secondary_clicked() {
                    edits[i] = (true, false, step.note);
                }
                cells[i] = Some(response);
            }
        });
        if edits.iter().any(|(edit, _, _)| *edit) {
            for (i, (edit, enabled, note)) in edits.iter().enumerate() {
                if *edit {
                    self.project.tracks[sel].pattern.steps[i] = resoz_model::Step {
                        enabled: *enabled,
                        note: *note,
                        velocity: 0.9,
                    };
                }
            }
            self.send_pattern(sel);
        }

        // 播放位置高亮（引擎原子量回传，每块更新一次）。
        if playing {
            let idx = (current as usize) % PATTERN_STEPS;
            if let Some(Some(response)) = cells.get(idx) {
                ui.painter().rect_stroke(
                    response.rect,
                    4.0,
                    egui::Stroke::new(2.0, egui::Color32::YELLOW),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }

    /// 钢琴卷帘（编辑选中轨道的片段；编辑即时下发音频线程）。
    fn clip_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.add_space(4.0);
        let sel = self.selected_track;
        ui.label(format!("钢琴卷帘 — 轨道 {}", sel + 1));
        let playhead = self.engine.musical_ticks();
        let playing = self.engine.is_playing();
        let edited = piano_roll::show(
            ui,
            &mut self.project.tracks[sel].clip,
            &mut self.roll_state,
            playhead,
            playing,
        );
        if edited {
            self.send_clip(sel);
        }
    }

    /// 自动化泳道（编辑选中轨道的曲线；编辑即时下发音频线程）。
    fn automation_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.add_space(4.0);
        let sel = self.selected_track;
        ui.horizontal(|ui| {
            ui.label("自动化");
            for (name, param) in [
                ("音量", AutoParam::TrackGain),
                ("声像", AutoParam::TrackPan),
                ("截止", AutoParam::FilterCutoff),
            ] {
                if ui
                    .selectable_label(self.lane_param == param, name)
                    .clicked()
                {
                    self.lane_param = param;
                }
            }
            let mut enabled = self.project.tracks[sel]
                .automations
                .iter()
                .find(|curve| curve.param == self.lane_param)
                .map(|curve| curve.enabled)
                .unwrap_or(false);
            if ui.checkbox(&mut enabled, "启用").changed() {
                let track = &mut self.project.tracks[sel];
                match track
                    .automations
                    .iter_mut()
                    .find(|curve| curve.param == self.lane_param)
                {
                    Some(curve) => curve.enabled = enabled,
                    None => track.automations.push(Automation {
                        param: self.lane_param,
                        enabled,
                        points: Vec::new(),
                    }),
                }
                let curve = track
                    .automations
                    .iter()
                    .find(|curve| curve.param == self.lane_param)
                    .cloned()
                    .expect("上一步已确保存在");
                self.send_automation(sel, &curve);
            }
            ui.weak("点按加锚点 · 拖拽移动 · 右键删除");
        });

        // 确保泳道对应的曲线存在（空曲线 = 无自动化）。
        if !self.project.tracks[sel]
            .automations
            .iter()
            .any(|curve| curve.param == self.lane_param)
        {
            self.project.tracks[sel].automations.push(Automation {
                param: self.lane_param,
                ..Automation::default()
            });
        }

        let loop_ticks = match self.project.tracks[sel].source {
            ModelTrackSource::Clip => self.project.tracks[sel].clip.loop_ticks,
            _ => TICKS_PER_BAR,
        };
        let playhead = self.engine.musical_ticks();
        let playing = self.engine.is_playing();
        let curve = self.project.tracks[sel]
            .automations
            .iter_mut()
            .find(|curve| curve.param == self.lane_param)
            .expect("上一步已确保存在");
        let edited = lane::show(
            ui,
            curve,
            loop_ticks,
            playhead,
            playing,
            &mut self.lane_state,
        );
        if edited {
            let curve = self.project.tracks[sel]
                .automations
                .iter()
                .find(|curve| curve.param == self.lane_param)
                .cloned()
                .expect("上一步已确保存在");
            self.send_automation(sel, &curve);
        }
    }

    /// 虚拟键盘：屏幕按键（按下发声、松开停音）+ 八度切换。
    fn keyboard_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.add_space(4.0);
        let sel = self.selected_track;
        ui.horizontal(|ui| {
            ui.label(format!("键盘 — 轨道 {}", sel + 1));
            if ui.button("− 八度").clicked() {
                self.octave_base = (self.octave_base.saturating_sub(12)).max(24);
            }
            let low = note_name(self.octave_base);
            let high = note_name(self.octave_base + 12);
            ui.monospace(format!("{low} – {high}"));
            if ui.button("+ 八度").clicked() {
                self.octave_base = (self.octave_base + 12).min(84);
            }
            ui.weak("电脑键 A W S E D F T G Y H U J K O L P ;（Z / X 切八度）");
        });

        ui.horizontal(|ui| {
            for semi in 0..=12u8 {
                let note = self.octave_base + semi;
                let black = BLACK_SEMIS.contains(&semi);
                let label = note_name(note);
                let held = self.held_notes.contains(&note);
                let text = if held {
                    egui::RichText::new(&label).strong()
                } else if black {
                    egui::RichText::new(&label).color(egui::Color32::WHITE)
                } else {
                    egui::RichText::new(&label)
                };
                let mut button = egui::Button::new(text);
                if held {
                    button = button.fill(egui::Color32::DARK_GREEN);
                } else if black {
                    button = button.fill(egui::Color32::from_gray(40));
                }
                let response = ui.add(button);
                // 用"按住状态"的边沿检测（0.36 移除了 pressed/released）：
                // 按下即发声，松开（含拖出按钮）即停音。
                let down = response.is_pointer_button_down_on();
                if down && !self.mouse_down_notes.contains(&note) {
                    self.mouse_down_notes.insert(note);
                    self.send_note_on(note, 0.9);
                } else if !down && self.mouse_down_notes.contains(&note) {
                    self.mouse_down_notes.remove(&note);
                    self.send_note_off(note);
                }
            }
        });
    }
}

impl eframe::App for ResoZApp {
    // eframe persistence：关窗与定期回调，保存最近文件列表。
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(
            storage,
            UI_STATE_KEY,
            &UiState {
                recent_files: self.recent.clone(),
            },
        );
    }

    // eframe 0.36：App trait 的入口从 update(ctx) 变为 ui(&mut Ui)；
    // 顶/底面板直接作用在根 Ui 上，剩余空间即中央区域。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            log::info!("UI 首帧渲染");
        }

        self.handle_keyboard_input(ui.ctx());

        // 冒烟测试用：到达自动关闭时限后请求退出。
        if let Some(after) = self.auto_close_after {
            if !self.close_sent && self.opened_at.elapsed() >= after {
                self.close_sent = true;
                log::info!("到达自动关闭时限，请求退出");
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // 窗口标题只在变化时重发（打开/保存时更新）。
        if self.window_title != self.title_sent {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Title(self.window_title.clone()));
            self.title_sent = self.window_title.clone();
        }

        // 走带停止沿：自动化不再覆盖，恢复各轨手动参数值。
        let playing = self.engine.is_playing();
        if self.was_playing && !playing {
            for track in 0..self.project.tracks.len() {
                self.push_track_params(track);
            }
        }
        self.was_playing = playing;

        self.transport_panel(ui);
        self.status_panel(ui);
        self.synth_panel(ui);

        // 音频驱动的显示（电平表、播放位置、包络）需要周期性重绘；
        // 没有它，egui 在无交互时休眠，自动关窗（冒烟测试）也不会触发。
        ui.ctx().request_repaint_after(Duration::from_millis(50));
    }
}

/// 启动主窗口（阻塞至窗口关闭）。**必须在主线程调用。**
pub fn run(engine: AudioEngine, config: UiConfig) -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ResoZ")
            .with_inner_size([920.0, 760.0])
            .with_min_inner_size([760.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ResoZ",
        native_options,
        Box::new(move |cc| Ok(Box::new(ResoZApp::new(cc, engine, config)))),
    )
}
