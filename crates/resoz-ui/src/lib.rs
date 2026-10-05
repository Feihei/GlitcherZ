#![forbid(unsafe_code)]

//! # resoz-ui
//!
//! ResoZ 的 egui 界面：走带控制、步进音序器、钢琴卷帘、混音器。
//!
//! 当前（路线图第 3 步）：主窗口 + 完整合成器面板 —— 滤波器（截止/
//! 共振/模式）、ADSR 包络参数、虚拟键盘（屏幕按键 + 电脑键盘）。所有
//! 参数改动经 [`resoz_engine::realtime::EngineCommand`] 无锁队列实时
//! 送往音频线程（块边界生效），峰值电平经
//! [`resoz_engine::AudioEngine::peak_level`]（内部为 `AtomicF32`）回传。
//!
//! 分层说明：本 crate 直接依赖 engine 的非实时控制端——`AudioEngine`
//! 只在 UI 线程被触碰，永不进入音频回调。引入图节点编辑后，这一层
//! 将改为持有通用的控制句柄抽象。

use std::collections::HashSet;
use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

use eframe::egui;

use resoz_engine::AudioEngine;
use resoz_engine::convert;
use resoz_engine::realtime::{EngineCommand, NoteEvent, ParamId};

/// 滤波器截止频率滑块范围（对数刻度）。
pub const CUTOFF_RANGE: RangeInclusive<f64> = 20.0..=18_000.0;

/// 主增益滑块范围（线性）。
pub const GAIN_RANGE: RangeInclusive<f64> = 0.0..=1.0;

/// 共振滑块范围（Simper SVF res）。
pub const RES_RANGE: RangeInclusive<f64> = 0.0..=0.98;

/// Attack / Decay 滑块范围（秒）。
pub const AD_RANGE: RangeInclusive<f64> = 0.0..=2.0;

/// Sustain 滑块范围（相对电平）。
pub const S_RANGE: RangeInclusive<f64> = 0.0..=1.0;

/// Release 滑块范围（秒）。
pub const R_RANGE: RangeInclusive<f64> = 0.0..=3.0;

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

/// UI 启动参数。
#[derive(Debug, Clone)]
pub struct UiConfig {
    /// 初始主增益，会被钳制进 [`GAIN_RANGE`]。
    pub gain: f64,
    /// 启动时自动触发的音符（演示/冒烟测试用；`None` = 静默启动）。
    pub auto_play_note: Option<u8>,
    /// 启动后自动关闭窗口（冒烟测试用；`None` = 手动关闭）。
    pub auto_close_after: Option<Duration>,
}

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
    gain: f64,
    cutoff_hz: f64,
    resonance: f64,
    /// 滤波模式：0 = 低通，1 = 高通，2 = 带通（与 `ParamId::FilterMode`
    /// 的取值约定一致）。
    mode: f64,
    attack_s: f64,
    decay_s: f64,
    sustain: f64,
    release_s: f64,
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
    /// 诊断标志：`ui()` 是否已进入过（仅日志用）。
    first_frame_logged: bool,
    /// 诊断标志：是否已发出关闭命令（仅日志用）。
    close_sent: bool,
}

impl ResoZApp {
    /// 构造窗口应用，并把初始参数经命令队列下发（下一个音频块边界生效）。
    pub fn new(engine: AudioEngine, config: UiConfig) -> Self {
        let mut app = Self {
            engine,
            gain: config.gain.clamp(*GAIN_RANGE.start(), *GAIN_RANGE.end()),
            cutoff_hz: 12_000.0,
            resonance: 0.2,
            mode: 0.0,
            attack_s: 0.01,
            decay_s: 0.10,
            sustain: 0.8,
            release_s: 0.20,
            octave_base: 60,
            held_notes: HashSet::new(),
            mouse_down_notes: HashSet::new(),
            current_note: None,
            meter: PeakMeter::new(),
            opened_at: Instant::now(),
            auto_close_after: config.auto_close_after,
            first_frame_logged: false,
            close_sent: false,
        };
        app.push_params();
        if let Some(note) = config.auto_play_note {
            app.send_note_on(note, 1.0);
        }
        app
    }

    /// 把当前 UI 参数完整下发一遍（队列满载时命令被丢弃，可安全忽略）。
    fn push_params(&mut self) {
        for (id, value) in [
            (ParamId::MasterGain, self.gain),
            (ParamId::FilterCutoff, self.cutoff_hz),
            (ParamId::FilterResonance, self.resonance),
            (ParamId::FilterMode, self.mode),
            (ParamId::Attack, self.attack_s),
            (ParamId::Decay, self.decay_s),
            (ParamId::Sustain, self.sustain),
            (ParamId::Release, self.release_s),
        ] {
            self.engine.send(EngineCommand::SetParam {
                id,
                value: convert::to_f32(value),
            });
        }
    }

    fn send_note_on(&mut self, note: u8, velocity: f32) {
        self.engine
            .send(EngineCommand::Note(NoteEvent::On { note, velocity }));
        self.held_notes.insert(note);
        self.current_note = Some(note);
    }

    fn send_note_off(&mut self, note: u8) {
        self.engine
            .send(EngineCommand::Note(NoteEvent::Off { note }));
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
                ..
            } = event
            else {
                continue;
            };
            if repeat {
                continue;
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

    fn transport_panel(&self, ui: &mut egui::Ui) {
        egui::Panel::top("resoz_transport").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_enabled(false, egui::Button::new("▶ 播放"))
                    .on_disabled_hover_text("走带控制在后续路线图接入");
                ui.add_enabled(false, egui::Button::new("■ 停止"))
                    .on_disabled_hover_text("走带控制在后续路线图接入");
                ui.separator();
                ui.label("120.0 BPM");
                ui.separator();
                ui.weak("ResoZ — Rust DAW");
            });
        });
    }

    fn status_panel(&self, ui: &mut egui::Ui) {
        let info = self.engine.info().clone();
        let position = self.engine.position_seconds();
        let note_text = self
            .current_note
            .map(note_name)
            .unwrap_or_else(|| "—".to_string());
        egui::Panel::bottom("resoz_status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("设备：{}", info.device_name));
                ui.separator();
                ui.label(format!(
                    "{} Hz / {} 声道",
                    info.sample_rate, info.out_channels
                ));
                ui.separator();
                ui.label(format!("音符：{note_text}"));
                ui.separator();
                ui.label(format!("位置 {:.1} s", position));
            });
        });
    }

    /// 发一个参数到音频线程。
    fn send_param(&mut self, id: ParamId, value: f64) {
        self.engine.send(EngineCommand::SetParam {
            id,
            value: convert::to_f32(value),
        });
    }

    fn synth_panel(&mut self, ui: &mut egui::Ui) {
        self.meter.update(self.engine.peak_level());
        let peak = self.meter.held;

        // eframe 0.36 的根 Ui 无边距；包一层 CentralPanel 保持默认观感。
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("合成器");
            ui.weak("正弦波 → ADSR 包络 → SVF 滤波器（单声部）—— 所有改动实时送达音频线程");
            ui.add_space(8.0);

            egui::Grid::new("synth_params")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    ui.label("截止频率");
                    let r = ui.add(
                        egui::Slider::new(&mut self.cutoff_hz, CUTOFF_RANGE)
                            .logarithmic(true)
                            .text("Hz"),
                    );
                    if r.changed() {
                        self.send_param(ParamId::FilterCutoff, self.cutoff_hz);
                    }
                    ui.end_row();

                    ui.label("共振");
                    let r = ui.add(egui::Slider::new(&mut self.resonance, RES_RANGE));
                    if r.changed() {
                        self.send_param(ParamId::FilterResonance, self.resonance);
                    }
                    ui.end_row();

                    ui.label("滤波模式");
                    ui.horizontal(|ui| {
                        for (name, value) in [("低通", 0.0), ("高通", 1.0), ("带通", 2.0)] {
                            if ui.selectable_label(self.mode == value, name).clicked() {
                                self.mode = value;
                                self.send_param(ParamId::FilterMode, value);
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("音量包络");
                    ui.horizontal(|ui| {
                        // 先取本地副本再写回，避免借用冲突。
                        let mut a = self.attack_s;
                        let mut d = self.decay_s;
                        let mut s = self.sustain;
                        let mut r = self.release_s;
                        let sliders = [
                            ("A", &mut a, AD_RANGE, ParamId::Attack),
                            ("D", &mut d, AD_RANGE, ParamId::Decay),
                            ("S", &mut s, S_RANGE, ParamId::Sustain),
                            ("R", &mut r, R_RANGE, ParamId::Release),
                        ];
                        for (label, value, range, id) in sliders {
                            let resp = ui.add_sized(
                                [104.0, 20.0],
                                egui::Slider::new(value, range).text(label),
                            );
                            if resp.changed() {
                                self.send_param(id, *value);
                            }
                        }
                        self.attack_s = a;
                        self.decay_s = d;
                        self.sustain = s;
                        self.release_s = r;
                    });
                    ui.end_row();

                    ui.label("主增益");
                    let r = ui.add(egui::Slider::new(&mut self.gain, GAIN_RANGE).text("线性"));
                    if r.changed() {
                        self.send_param(ParamId::MasterGain, self.gain);
                    }
                    ui.end_row();
                });

            self.keyboard_section(ui);

            ui.add_space(10.0);
            ui.label("峰值电平");
            ui.add(
                egui::ProgressBar::new(peak.clamp(0.0, 1.0))
                    .desired_height(16.0)
                    .show_percentage(),
            );
            ui.weak(format!("峰值 {peak:.3} —— AtomicF32 回传，峰值保持 600 ms"));
        });
    }

    /// 虚拟键盘：屏幕按键（按下发声、松开停音）+ 八度切换。
    fn keyboard_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("键盘");
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
            .with_inner_size([880.0, 620.0])
            .with_min_inner_size([720.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ResoZ",
        native_options,
        Box::new(move |_cc| Ok(Box::new(ResoZApp::new(engine, config)))),
    )
}
