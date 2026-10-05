#![forbid(unsafe_code)]

//! # resoz-ui
//!
//! ResoZ 的 egui 界面：走带控制、步进音序器、钢琴卷帘、混音器。
//!
//! 当前（路线图第 2 步）：主窗口骨架 + 正弦音源参数面板。参数改动经
//! [`resoz_engine::realtime::EngineCommand`] 无锁队列实时送往音频线程
//! （块边界生效），峰值电平经 [`resoz_engine::AudioEngine::peak_level`]
//! （内部为 `AtomicF32`）回传绘制。
//!
//! 分层说明：本 crate 直接依赖 engine 的非实时控制端——`AudioEngine`
//! 只在 UI 线程被触碰，永不进入音频回调。引入图节点编辑后，这一层
//! 将改为持有通用的控制句柄抽象。

use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

use eframe::egui;

use resoz_engine::AudioEngine;
use resoz_engine::convert;
use resoz_engine::realtime::{EngineCommand, ParamId};

/// 频率滑块范围（对数刻度）。
pub const FREQ_RANGE: RangeInclusive<f64> = 20.0..=8000.0;

/// 主增益滑块范围（线性）。
pub const GAIN_RANGE: RangeInclusive<f64> = 0.0..=1.0;

/// UI 启动参数。
#[derive(Debug, Clone)]
pub struct UiConfig {
    /// 初始频率（Hz），会被钳制进 [`FREQ_RANGE`]。
    pub freq_hz: f64,
    /// 初始主增益，会被钳制进 [`GAIN_RANGE`]。
    pub gain: f64,
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
    freq_hz: f64,
    gain: f64,
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
        let freq_hz = config.freq_hz.clamp(*FREQ_RANGE.start(), *FREQ_RANGE.end());
        let gain = config.gain.clamp(*GAIN_RANGE.start(), *GAIN_RANGE.end());
        let mut app = Self {
            engine,
            freq_hz,
            gain,
            meter: PeakMeter::new(),
            opened_at: Instant::now(),
            auto_close_after: config.auto_close_after,
            first_frame_logged: false,
            close_sent: false,
        };
        app.push_params();
        app
    }

    /// 把当前 UI 参数完整下发一遍（队列满载时命令被丢弃，可安全忽略）。
    fn push_params(&mut self) {
        self.engine.send(EngineCommand::SetParam {
            id: ParamId::SineFrequency,
            value: convert::to_f32(self.freq_hz),
        });
        self.engine.send(EngineCommand::SetParam {
            id: ParamId::MasterGain,
            value: convert::to_f32(self.gain),
        });
    }

    fn transport_panel(&self, ui: &mut egui::Ui) {
        egui::Panel::top("resoz_transport").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_enabled(false, egui::Button::new("▶ 播放"))
                    .on_disabled_hover_text("走带控制在路线图第 3 步接入");
                ui.add_enabled(false, egui::Button::new("■ 停止"))
                    .on_disabled_hover_text("走带控制在路线图第 3 步接入");
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
        egui::Panel::bottom("resoz_status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("设备：{}", info.device_name));
                ui.separator();
                ui.label(format!(
                    "{} Hz / {} 声道",
                    info.sample_rate, info.out_channels
                ));
                ui.separator();
                ui.label(format!("位置 {:.1} s", position));
            });
        });
    }

    fn synth_panel(&mut self, ui: &mut egui::Ui) {
        self.meter.update(self.engine.peak_level());
        let peak = self.meter.held;

        // eframe 0.36 的根 Ui 无边距；包一层 CentralPanel 保持默认观感。
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("正弦音源");
            ui.weak("路线图第 1 步演示音源 —— 拖动滑块实时控制音频线程参数");
            ui.add_space(8.0);

            egui::Grid::new("synth_params")
                .num_columns(2)
                .spacing([16.0, 10.0])
                .show(ui, |ui| {
                    ui.label("频率");
                    let freq = ui.add(
                        egui::Slider::new(&mut self.freq_hz, FREQ_RANGE)
                            .logarithmic(true)
                            .text("Hz"),
                    );
                    if freq.changed() {
                        self.engine.send(EngineCommand::SetParam {
                            id: ParamId::SineFrequency,
                            value: convert::to_f32(self.freq_hz),
                        });
                    }
                    ui.end_row();

                    ui.label("主增益");
                    let gain = ui.add(egui::Slider::new(&mut self.gain, GAIN_RANGE).text("线性"));
                    if gain.changed() {
                        self.engine.send(EngineCommand::SetParam {
                            id: ParamId::MasterGain,
                            value: convert::to_f32(self.gain),
                        });
                    }
                    ui.end_row();
                });

            ui.add_space(12.0);
            ui.label("峰值电平");
            ui.add(
                egui::ProgressBar::new(peak.clamp(0.0, 1.0))
                    .desired_height(16.0)
                    .show_percentage(),
            );
            ui.weak(format!("峰值 {peak:.3} —— AtomicF32 回传，峰值保持 600 ms"));
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

        // 音频驱动的显示（电平表、播放位置）需要周期性重绘；
        // 没有它，egui 在无交互时休眠，自动关窗（冒烟测试）也不会触发。
        ui.ctx().request_repaint_after(Duration::from_millis(50));
    }
}

/// 启动主窗口（阻塞至窗口关闭）。**必须在主线程调用。**
pub fn run(engine: AudioEngine, config: UiConfig) -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ResoZ")
            .with_inner_size([880.0, 560.0])
            .with_min_inner_size([640.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ResoZ",
        native_options,
        Box::new(move |_cc| Ok(Box::new(ResoZApp::new(engine, config)))),
    )
}
