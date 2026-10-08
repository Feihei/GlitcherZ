#![forbid(unsafe_code)]

//! # resoz-ui
//!
//! ResoZ 的 egui 界面：走带控制、步进音序器、钢琴卷帘、混音器、
//! 自动化泳道。
//!
//! P5c（spec `docs/spec/node-graph.md` D13）：引擎图**纯按项目文档
//! 重建**——`ResoZApp` 持有 `Project` 文档，加载/新建时经
//! `apply_document` 以 `add_node` / `connect_nodes` 粒度逐步建图；
//! 文档 node_id 是持久化身份（D11），引擎侧身份为会话内单调分配的
//! `engine_id`（映射见 `node_map`，会话内不复用已回收 id）。
//!
//! 分层说明：本 crate 直接依赖 engine 的非实时控制端——`AudioEngine`
//! 只在 UI 线程被触碰，永不进入音频回调。模型与引擎互不依赖，发送前
//! 在此完成文档 → 引擎表示的转换。

use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

use resoz_clap_host::{ClapInstance, ClapInstanceConfig};
use resoz_engine::AudioEngine;
use resoz_engine::convert;
use resoz_engine::realtime::{
    ChannelStripProcessor, DelayProcessor, EngineCommand, MAX_AUTO_POINTS, MAX_CLIP_NOTES,
    NoteEvent, ParamId, Processor, RtAutoPoint, RtAutoTrack, RtAutomation, RtClip, RtNote,
    SequencerCommand, SequencerStep, SynthProcessor, TrackSource,
};
use resoz_model::{
    AutoParam, AutomationTrack, CableKind, DelayParams, MAX_AUTO_TRACKS, NodeDoc, PATTERN_STEPS,
    Pattern, PortRef, Project, SynthParams, TICKS_PER_BAR, VoiceSpec,
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

/// 文档自动化参数 → 引擎参数 ID。
fn param_id_of(param: &AutoParam) -> ParamId {
    match param {
        AutoParam::TrackGain => ParamId::TrackGain,
        AutoParam::TrackPan => ParamId::TrackPan,
        AutoParam::FilterCutoff => ParamId::FilterCutoff,
        AutoParam::PluginParam { param_id, .. } => ParamId::PluginParam(*param_id),
    }
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

/// 外部插件实例守卫（CLAP / VST3）。
#[allow(dead_code)] // 守卫仅为延长寿命持有，字段本身不读取
enum PluginGuard {
    Clap(resoz_clap_host::ClapInstanceGuard),
    Vst3(resoz_vst3_host::Vst3InstanceGuard),
}

/// 一个已装填的外部插件声源/效果器守卫（node_id 为**引擎侧**身份）。
///
/// 守卫**不可提前丢弃**——卸载动态库会使实时侧节点悬空。被换下/
/// 随文档移除的守卫转入 `retiring_guards`，经回收回执按序释放。
#[allow(dead_code)]
struct PluginGuardEntry {
    /// 与引擎侧节点 ID 配对（回收回执按此匹配）。
    node_id: u64,
    #[allow(dead_code)]
    track: usize,
    #[allow(dead_code)]
    plugin_id: String,
    _guard: PluginGuard,
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
    /// 项目文档（UI 持有模型状态，见 AGENTS.md「线程模型」）。
    project: Project,
    /// 文档 node_id → 引擎 node_id（会话内单调，重载文档即全部换新）。
    node_map: HashMap<u64, u64>,
    /// 正在编辑的轨道（合成器/音序器/键盘的目标）。
    selected_track: usize,
    /// 正在编辑的自动化轨道。
    selected_auto: usize,
    /// 钢琴卷帘的跨帧状态（滚动/拖拽）。
    roll_state: piano_roll::RollState,
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
    /// 在役外部插件守卫（CLAP/VST3）：guard 丢弃会卸载插件动态库，
    /// 使实时侧节点悬空，故持有至节点被换下为止。
    voice_guards: Vec<PluginGuardEntry>,
    /// 已换下/随旧文档移除的守卫：等实时侧回收回执到达后再丢弃
    /// （动态库必须活到旧节点真正离开处理图）。
    retiring_guards: Vec<PluginGuardEntry>,
    /// 已扫描的 CLAP 乐器列表（懒扫描缓存）。
    clap_plugins: Option<Vec<resoz_clap_host::ClapPluginInfo>>,
    /// 已扫描的 CLAP 效果器列表（懒扫描缓存）。
    clap_effects: Option<Vec<resoz_clap_host::ClapPluginInfo>>,
    /// 已扫描的 VST3 乐器列表（懒扫描缓存）。
    vst3_plugins: Option<Vec<resoz_vst3_host::Vst3PluginInfo>>,
    /// 已扫描的 VST3 效果器列表（懒扫描缓存）。
    vst3_effects: Option<Vec<resoz_vst3_host::Vst3PluginInfo>>,
    /// 各轨道 VST3 声源的参数列表（实例化时读出，键 = 轨道号）。
    /// 仅诊断/展示用；实际参数控制仍走 `PluginParam` 桥接。
    voice_params: HashMap<usize, Vec<resoz_vst3_host::Vst3ParamInfo>>,
    /// 各轨道 CLAP 声源的参数列表（实例化时读出，键 = 轨道号）。
    /// 自动化连线目标数据源；曲线值语义 = 插件值域原始值。
    clap_voice_params: HashMap<usize, Vec<resoz_clap_host::ClapParamInfo>>,
    /// 诊断标志：`ui()` 是否已进入过（仅日志用）。
    first_frame_logged: bool,
    /// 诊断标志：是否已发出关闭命令（仅日志用）。
    close_sent: bool,
}

impl ResoZApp {
    /// 构造窗口应用，恢复持久化的最近文件，并把整个项目文档经
    /// 逐步建图 + 命令队列下发（下一个音频块边界生效）。
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
            node_map: HashMap::new(),
            selected_track: 0,
            selected_auto: 0,
            roll_state: piano_roll::RollState::new(),
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
            voice_guards: Vec::new(),
            retiring_guards: Vec::new(),
            clap_plugins: None,
            clap_effects: None,
            vst3_plugins: None,
            vst3_effects: None,
            voice_params: HashMap::new(),
            clap_voice_params: HashMap::new(),
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

    // ---- 文档 → 引擎（D13 全显式：加载器纯按文档重建） ----

    /// 把当前项目文档重建进引擎：移除旧节点 → 安装文档节点 →
    /// 装配轨道寻址表 → 按文档连线 → 下发活跃轨数。
    fn apply_document(&mut self) {
        // 1) 移除上一份文档装进引擎的节点；其守卫全部转入退役区，
        //    等回收回执按序释放（引擎 id 不复用，配对无歧义）。
        let stale: Vec<u64> = self.node_map.values().copied().collect();
        for engine_id in stale {
            if self.engine.remove_node(engine_id) {
                self.retire_guard(engine_id);
            }
        }
        self.node_map.clear();
        self.voice_params.clear();
        self.clap_voice_params.clear();

        // 2) 安装文档节点（外部插件失败 → 回退内置，保证链路完整）。
        let sample_rate = self.engine.info().sample_rate;
        let nodes = self.project.graph.nodes.clone();
        for node in &nodes {
            self.install_node(node, sample_rate);
        }
        //（install_node 以 u32 采样率实例化插件；内置处理器经 convert 转 f32。）

        // 3) 装配轨道寻址表（手动参数/电平/键盘的轨道坐标寻址）。
        for track in 0..self.project.midi_tracks.len() {
            let resolve = |doc_id: Option<u64>| -> Option<u64> {
                doc_id.and_then(|id| self.node_map.get(&id).copied())
            };
            let voice = resolve(
                self.project
                    .graph
                    .instrument_of_track(track)
                    .map(NodeDoc::node_id),
            );
            let delay = resolve(
                self.project
                    .graph
                    .builtin_delay_of_track(track)
                    .map(NodeDoc::node_id),
            );
            let channel = resolve(
                self.project
                    .graph
                    .channel_strip_of_track(track)
                    .map(NodeDoc::node_id),
            );
            if !self
                .engine
                .register_track_chain(track, voice, delay, channel)
            {
                log::warn!("轨道 {} 寻址表装配失败（节点缺失或队列满）", track + 1);
            }
        }

        // 4) 按文档连线。
        let master = self.engine.master_node_id();
        let cables = self.project.graph.cables.clone();
        for cable in &cables {
            let from = match &cable.from {
                PortRef::Node(id) => self.node_map.get(id).copied(),
                _ => None,
            };
            let to_node = match &cable.to {
                PortRef::Node(id) => self.node_map.get(id).copied(),
                PortRef::Master => Some(master),
                _ => None,
            };
            match cable.kind {
                CableKind::Audio => {
                    let (Some(from), Some(to)) = (from, to_node) else {
                        log::warn!("音频线端点节点缺失，跳过：{cable:?}");
                        continue;
                    };
                    if !self.engine.connect_nodes(from, to) {
                        log::warn!("音频线 ({from} → {to}) 下发失败（成环/超限/队列满）");
                    }
                }
                CableKind::Midi => {
                    let PortRef::MidiTrack(track) = &cable.from else {
                        continue;
                    };
                    let Some(to) = to_node else {
                        log::warn!("MIDI 线目标节点缺失，跳过：{cable:?}");
                        continue;
                    };
                    if !self.engine.connect_midi_node(*track, to) {
                        log::warn!("MIDI 线（轨 {track} → {to}）下发失败（扇出超限/队列满）");
                    }
                }
                CableKind::Automation => {
                    let PortRef::AutoTrack(index) = &cable.from else {
                        continue;
                    };
                    let Some(to) = to_node else {
                        log::warn!("自动化线目标节点缺失，跳过：{cable:?}");
                        continue;
                    };
                    if !self
                        .engine
                        .set_auto_route(*index, to, param_id_of(&cable.param))
                    {
                        log::warn!("自动化线（轨 {index} → {to}）下发失败");
                    }
                }
            }
        }

        // 5) 活跃 MIDI 轨数（D12）。
        self.engine
            .set_midi_track_count(self.project.midi_tracks.len());
    }

    /// 安装一个文档节点：构造处理器（外部插件经非实时实例化）→
    /// 引擎 `add_node`（引擎侧新 id）→ 登记映射与守卫。
    fn install_node(&mut self, node: &NodeDoc, sample_rate: u32) {
        let sr = convert::sample_rate_to_f32(sample_rate);
        let doc_id = node.node_id();
        let track = node.track();
        let (processor, guard, plugin_label): (Box<dyn Processor>, Option<PluginGuard>, String) =
            match node {
                NodeDoc::Instrument { voice, .. } => match voice {
                    VoiceSpec::Builtin => (Box::new(SynthProcessor::new(sr)), None, String::new()),
                    VoiceSpec::Clap { path, plugin_id } => {
                        match ClapInstance::spawn_rt(
                            std::path::Path::new(path),
                            plugin_id,
                            ClapInstanceConfig {
                                sample_rate,
                                max_frames: 128,
                            },
                        ) {
                            Ok(instance) => {
                                // 参数枚举需在实例被拆解前进行（守卫持有插件实例）。
                                let params = instance.list_params();
                                let ClapInstance { guard, node } = instance;
                                self.clap_voice_params.insert(track, params);
                                (
                                    Box::new(node),
                                    Some(PluginGuard::Clap(guard)),
                                    plugin_id.clone(),
                                )
                            }
                            Err(err) => {
                                log::error!("实例化 CLAP 插件 {plugin_id} 失败：{err}");
                                self.clap_voice_params.remove(&track);
                                (Box::new(SynthProcessor::new(sr)), None, String::new())
                            }
                        }
                    }
                    VoiceSpec::Vst3 { path, class_id } => {
                        match resoz_vst3_host::Vst3Instance::spawn_rt(
                            std::path::Path::new(path),
                            class_id,
                            resoz_vst3_host::Vst3InstanceConfig {
                                sample_rate,
                                max_frames: 128,
                            },
                        ) {
                            Ok(instance) => {
                                let resoz_vst3_host::Vst3Instance { guard, node } = instance;
                                let params = guard.parameters();
                                self.voice_params.insert(track, params);
                                (
                                    Box::new(node),
                                    Some(PluginGuard::Vst3(guard)),
                                    class_id.clone(),
                                )
                            }
                            Err(err) => {
                                log::error!("实例化 VST3 插件 {class_id} 失败：{err}");
                                self.voice_params.remove(&track);
                                (Box::new(SynthProcessor::new(sr)), None, String::new())
                            }
                        }
                    }
                },
                NodeDoc::Effect { voice, .. } => match voice {
                    VoiceSpec::Builtin => (Box::new(DelayProcessor::new(sr)), None, String::new()),
                    VoiceSpec::Clap { path, plugin_id } => {
                        match ClapInstance::spawn_rt(
                            std::path::Path::new(path),
                            plugin_id,
                            ClapInstanceConfig {
                                sample_rate,
                                max_frames: 128,
                            },
                        ) {
                            Ok(instance) => {
                                let ClapInstance { guard, node } = instance;
                                (
                                    Box::new(node),
                                    Some(PluginGuard::Clap(guard)),
                                    plugin_id.clone(),
                                )
                            }
                            Err(err) => {
                                log::error!("实例化 CLAP 效果器 {plugin_id} 失败：{err}");
                                (Box::new(DelayProcessor::new(sr)), None, String::new())
                            }
                        }
                    }
                    VoiceSpec::Vst3 { path, class_id } => {
                        match resoz_vst3_host::Vst3Instance::spawn_rt(
                            std::path::Path::new(path),
                            class_id,
                            resoz_vst3_host::Vst3InstanceConfig {
                                sample_rate,
                                max_frames: 128,
                            },
                        ) {
                            Ok(instance) => {
                                let resoz_vst3_host::Vst3Instance { guard, node } = instance;
                                (
                                    Box::new(node),
                                    Some(PluginGuard::Vst3(guard)),
                                    class_id.clone(),
                                )
                            }
                            Err(err) => {
                                log::error!("实例化 VST3 效果器 {class_id} 失败：{err}");
                                (Box::new(DelayProcessor::new(sr)), None, String::new())
                            }
                        }
                    }
                },
                NodeDoc::ChannelStrip { .. } => {
                    (Box::new(ChannelStripProcessor::new()), None, String::new())
                }
            };
        let engine_id = self.engine.fresh_node_id();
        if self.engine.add_node(engine_id, processor) {
            self.node_map.insert(doc_id, engine_id);
            if let Some(guard) = guard {
                self.voice_guards.push(PluginGuardEntry {
                    node_id: engine_id,
                    track,
                    plugin_id: plugin_label,
                    _guard: guard,
                });
            }
        } else {
            drop(guard); // 节点从未入图：立即丢弃守卫（动态库安全卸载）。
            log::error!("文档节点 {doc_id} 安装失败（槽位满/队列满）");
        }
    }

    /// 把某节点的在役守卫转入退役区（节点被换下/移除时调用；
    /// 退役守卫等回收回执到达后释放）。
    fn retire_guard(&mut self, engine_id: u64) {
        let mut i = 0;
        while i < self.voice_guards.len() {
            if self.voice_guards[i].node_id == engine_id {
                let entry = self.voice_guards.remove(i);
                self.retiring_guards.push(entry);
            } else {
                i += 1;
            }
        }
    }

    /// 文档 node_id → 引擎 node_id。
    fn engine_id(&self, doc_id: u64) -> Option<u64> {
        self.node_map.get(&doc_id).copied()
    }

    /// 把项目文档的参数完整下发一遍（队列满载时命令被丢弃，可安全忽略）。
    fn push_all_params(&mut self) {
        self.apply_document();
        self.engine.send(EngineCommand::SetParam {
            id: ParamId::MasterGain,
            value: self.project.master_gain,
        });
        self.send_sequencer(SequencerCommand::SetBpm(convert::to_f32(
            self.project.metadata.tempo_bpm,
        )));
        self.send_sequencer(SequencerCommand::SetGate(self.project.gate));
        for track in 0..self.project.midi_tracks.len() {
            self.push_track_params(track);
            let steps = self.rt_pattern(track);
            self.send_sequencer(SequencerCommand::SetPattern { track, steps });
            let clip = self.rt_clip(track);
            self.send_sequencer(SequencerCommand::SetClip { track, clip });
            let source = match self.project.midi_tracks[track].source {
                resoz_model::TrackSource::Steps => TrackSource::Steps,
                resoz_model::TrackSource::Clip => TrackSource::Clip,
            };
            self.send_sequencer(SequencerCommand::SetSource { track, source });
        }
        for index in 0..self.project.automation_tracks.len() {
            self.send_auto_curve(index);
        }
    }

    /// 下发一条轨道的全部参数（音色 / 延迟 / 混音；轨道寻址表已装配）。
    fn push_track_params(&mut self, track: usize) {
        let synth = self.synth_of_track(track).unwrap_or_default();
        let delay = self.delay_of_track(track).unwrap_or_default();
        let (muted, gain, pan) = self.track_strip_state(track);
        let params = [
            (ParamId::FilterCutoff, synth.cutoff_hz),
            (ParamId::FilterResonance, synth.resonance),
            (ParamId::FilterMode, synth.filter_mode),
            (ParamId::Attack, synth.attack_s),
            (ParamId::Decay, synth.decay_s),
            (ParamId::Sustain, synth.sustain),
            (ParamId::Release, synth.release_s),
            (ParamId::DelayTime, delay.time_s),
            (ParamId::DelayFeedback, delay.feedback),
            (ParamId::DelayMix, delay.mix),
            (ParamId::TrackGain, gain),
            (ParamId::TrackPan, pan),
            (ParamId::TrackMute, if muted { 1.0 } else { 0.0 }),
        ];
        for (id, value) in params {
            self.engine
                .send(EngineCommand::SetTrackParam { track, id, value });
        }
    }

    // ---- 文档字段便捷访问（节点图形态下的轨道面板数据源） ----

    /// 某轨的内置合成器音色参数。
    fn synth_of_track(&self, track: usize) -> Option<SynthParams> {
        match self.project.graph.instrument_of_track(track)? {
            NodeDoc::Instrument { synth, .. } => Some(*synth),
            _ => None,
        }
    }

    /// 某轨的内置合成器音色参数（可变）。
    fn synth_mut(&mut self, track: usize) -> Option<&mut SynthParams> {
        match self.project.graph.instrument_of_track_mut(track)? {
            NodeDoc::Instrument { synth, .. } => Some(synth),
            _ => None,
        }
    }

    /// 某轨的内置延迟参数。
    fn delay_of_track(&self, track: usize) -> Option<DelayParams> {
        match self.project.graph.builtin_delay_of_track(track)? {
            NodeDoc::Effect { delay, .. } => Some(*delay),
            _ => None,
        }
    }

    /// 某轨的内置延迟参数（可变）。
    fn delay_mut(&mut self, track: usize) -> Option<&mut DelayParams> {
        match self.project.graph.builtin_delay_of_track_mut(track)? {
            NodeDoc::Effect { delay, .. } => Some(delay),
            _ => None,
        }
    }

    /// 某轨通道条状态：(静音, 增益, 声像)；无通道条节点时取默认。
    fn track_strip_state(&self, track: usize) -> (bool, f32, f32) {
        match self.project.graph.channel_strip_of_track(track) {
            Some(NodeDoc::ChannelStrip {
                gain, pan, muted, ..
            }) => (*muted, *gain, *pan),
            _ => (false, 0.8, 0.0),
        }
    }

    // ---- 自动化轨道下发（D7/D12：曲线与连线终点分离） ----

    /// 下发一条自动化轨道的曲线实体（终点由图的自动化连线表决定，
    /// 经 `apply_document` / `set_auto_endpoint` 设置）。
    fn send_auto_curve(&mut self, index: usize) {
        let Some(track) = self.project.automation_tracks.get(index) else {
            return;
        };
        let mut rt = RtAutomation {
            enabled: track.enabled,
            ..RtAutomation::default()
        };
        for point in track.points.iter().take(MAX_AUTO_POINTS) {
            rt.push(RtAutoPoint {
                tick: point.tick,
                value: point.value,
            });
        }
        self.send_sequencer(SequencerCommand::SetAutoTrack {
            index,
            auto: RtAutoTrack {
                loop_ticks: track.loop_ticks,
                curve: rt,
            },
        });
    }

    /// 重定向一条自动化轨道的连线终点（文档 cable 存**文档** node_id，
    /// 引擎路由按 node_map 映射到引擎侧 id）。
    fn set_auto_endpoint(&mut self, index: usize, doc_id: u64, param: AutoParam) {
        self.project.graph.remove_auto_cable(index);
        self.project
            .graph
            .cables
            .push(resoz_model::CableDoc::automation(
                index,
                doc_id,
                param.clone(),
            ));
        if let Some(engine_id) = self.engine_id(doc_id) {
            self.engine
                .set_auto_route(index, engine_id, param_id_of(&param));
        }
    }

    /// 文档模式 → 引擎表示（`Copy` 数组，零堆分配）。
    fn rt_pattern(&self, track: usize) -> [SequencerStep; PATTERN_STEPS] {
        let mut steps = [SequencerStep::default(); PATTERN_STEPS];
        for (i, step) in self.project.midi_tracks[track]
            .pattern
            .steps
            .iter()
            .enumerate()
        {
            steps[i] = SequencerStep {
                enabled: step.enabled,
                note: step.note,
                velocity: step.velocity,
            };
        }
        steps
    }

    /// 文档片段 → 引擎表示（超容量截断，`Copy` 结构，零堆分配）。
    fn rt_clip(&self, track: usize) -> RtClip {
        let clip = &self.project.midi_tracks[track].clip;
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

    /// 读盘、替换文档、按文档重建引擎图并重发全部参数。
    fn do_open(&mut self, path: PathBuf) {
        match resoz_model::load_project(&path) {
            Ok(project) => {
                self.project = project;
                self.project_path = Some(path.clone());
                self.push_recent(path.clone());
                self.selected_track = 0;
                self.selected_auto = 0;
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

    /// 把选中轨道的声源恢复为内置合成器（原位替换，节点身份不变）。
    fn set_builtin_instrument(&mut self, track: usize) {
        if let Some(NodeDoc::Instrument { voice, .. }) =
            self.project.graph.instrument_of_track_mut(track)
        {
            *voice = VoiceSpec::Builtin;
        }
        self.voice_params.remove(&track);
        self.clap_voice_params.remove(&track);
        let Some(doc_id) = self
            .project
            .graph
            .instrument_of_track(track)
            .map(NodeDoc::node_id)
        else {
            return;
        };
        let Some(engine_id) = self.engine_id(doc_id) else {
            return;
        };
        let sample_rate = convert::sample_rate_to_f32(self.engine.info().sample_rate);
        if self
            .engine
            .replace_node(engine_id, Box::new(SynthProcessor::new(sample_rate)))
            .is_none()
        {
            log::error!("轨道 {} 声源恢复失败（节点缺失或队列满）", track + 1);
        }
    }

    /// 给轨道装填 CLAP 乐器声源：非实时实例化 → 引擎原位替换 → 守卫登记。
    /// 成功后顺带枚举插件参数（自动化连线目标的数据源）。
    fn load_clap_voice(&mut self, track: usize, path: &str, plugin_id: &str) {
        let Some(doc_id) = self
            .project
            .graph
            .instrument_of_track(track)
            .map(NodeDoc::node_id)
        else {
            return;
        };
        let Some(engine_id) = self.engine_id(doc_id) else {
            return;
        };
        self.retire_guard(engine_id);
        match ClapInstance::spawn_rt(
            std::path::Path::new(path),
            plugin_id,
            ClapInstanceConfig {
                sample_rate: self.engine.info().sample_rate,
                max_frames: 128,
            },
        ) {
            Ok(instance) => {
                // 参数枚举需在实例被拆解前进行（守卫持有插件实例）。
                let params = instance.list_params();
                let ClapInstance { guard, node } = instance;
                if self
                    .engine
                    .replace_node(engine_id, Box::new(node))
                    .is_some()
                {
                    self.voice_guards.push(PluginGuardEntry {
                        node_id: engine_id,
                        track,
                        plugin_id: plugin_id.to_string(),
                        _guard: PluginGuard::Clap(guard),
                    });
                    self.clap_voice_params.insert(track, params);
                    log::info!("轨道 {} 已装填 CLAP 声源：{plugin_id}", track + 1);
                } else {
                    log::error!("声源队列满：轨道 {} 装填失败", track + 1);
                }
            }
            Err(err) => {
                log::error!("实例化 CLAP 插件 {plugin_id} 失败：{err}");
                self.set_message(format!("插件加载失败：{err}"));
            }
        }
    }

    /// 给轨道装填 VST3 乐器声源：非实时实例化 → 引擎原位替换 → 守卫登记。
    fn load_vst3_voice(&mut self, track: usize, path: &str, class_id: &str) {
        let Some(doc_id) = self
            .project
            .graph
            .instrument_of_track(track)
            .map(NodeDoc::node_id)
        else {
            return;
        };
        let Some(engine_id) = self.engine_id(doc_id) else {
            return;
        };
        self.retire_guard(engine_id);
        match resoz_vst3_host::Vst3Instance::spawn_rt(
            std::path::Path::new(path),
            class_id,
            resoz_vst3_host::Vst3InstanceConfig {
                sample_rate: self.engine.info().sample_rate,
                max_frames: 128,
            },
        ) {
            Ok(instance) => {
                let resoz_vst3_host::Vst3Instance { guard, node } = instance;
                // 控制侧枚举参数列表（守卫移入注册表前读出）。
                let params = guard.parameters();
                if self
                    .engine
                    .replace_node(engine_id, Box::new(node))
                    .is_some()
                {
                    self.voice_params.insert(track, params);
                    self.voice_guards.push(PluginGuardEntry {
                        node_id: engine_id,
                        track,
                        plugin_id: class_id.to_string(),
                        _guard: PluginGuard::Vst3(guard),
                    });
                    log::info!("轨道 {} 已装填 VST3 声源：{class_id}", track + 1);
                } else {
                    log::error!("声源队列满：轨道 {} 装填失败", track + 1);
                }
            }
            Err(err) => {
                log::error!("实例化 VST3 插件 {class_id} 失败：{err}");
                self.set_message(format!("VST3 插件加载失败：{err}"));
            }
        }
    }

    /// 在轨道链末（通道条之前）插入一个外部效果器节点：引擎
    /// `insert_node_between` + 文档同步（节点 + 旁路线改接）。
    fn insert_effect_common(
        &mut self,
        track: usize,
        voice: VoiceSpec,
        plugin_label: String,
        node: Box<dyn Processor>,
        guard: PluginGuard,
    ) {
        let Some(strip_doc) = self
            .project
            .graph
            .channel_strip_of_track(track)
            .map(NodeDoc::node_id)
        else {
            log::error!("轨道 {} 无通道条节点，效果器插入失败", track + 1);
            return;
        };
        let Some(strip_engine) = self.engine_id(strip_doc) else {
            return;
        };
        // 找到当前直连通道条的音频线源（文档拓扑）。
        let Some(in_from_doc) = self.project.graph.cables.iter().find_map(|c| {
            if c.kind == CableKind::Audio && c.to == PortRef::Node(strip_doc) {
                match c.from {
                    PortRef::Node(id) => Some(id),
                    _ => None,
                }
            } else {
                None
            }
        }) else {
            log::error!("轨道 {} 通道条无入线，效果器插入失败", track + 1);
            return;
        };
        let Some(in_from_engine) = self.engine_id(in_from_doc) else {
            return;
        };
        let new_doc_id = self.project.graph.next_node_id();
        let new_engine_id = self.engine.fresh_node_id();
        if !self
            .engine
            .insert_node_between(new_engine_id, in_from_engine, strip_engine, node)
        {
            log::error!("结构队列满：轨道 {} 插入效果器失败", track + 1);
            return;
        }
        // 文档同步：节点 + 旁路线改接。
        self.project.graph.nodes.push(NodeDoc::Effect {
            node_id: new_doc_id,
            track,
            voice,
            delay: DelayParams::default(),
        });
        self.project.graph.cables.retain(|c| {
            !(c.kind == CableKind::Audio
                && c.from == PortRef::Node(in_from_doc)
                && c.to == PortRef::Node(strip_doc))
        });
        self.project
            .graph
            .cables
            .push(resoz_model::CableDoc::audio(in_from_doc, new_doc_id));
        self.project
            .graph
            .cables
            .push(resoz_model::CableDoc::audio(new_doc_id, strip_doc));
        self.node_map.insert(new_doc_id, new_engine_id);
        self.voice_guards.push(PluginGuardEntry {
            node_id: new_engine_id,
            track,
            plugin_id: plugin_label,
            _guard: guard,
        });
        log::info!("轨道 {} 已插入外部效果器", track + 1);
    }

    /// 给轨道插入 CLAP 效果器节点（非实时实例化 → 引擎插线 → 守卫登记）。
    fn load_clap_effect(&mut self, track: usize, path: &str, plugin_id: &str) {
        match ClapInstance::spawn_rt(
            std::path::Path::new(path),
            plugin_id,
            ClapInstanceConfig {
                sample_rate: self.engine.info().sample_rate,
                max_frames: 128,
            },
        ) {
            Ok(instance) => {
                let ClapInstance { guard, node } = instance;
                self.insert_effect_common(
                    track,
                    VoiceSpec::Clap {
                        path: path.to_string(),
                        plugin_id: plugin_id.to_string(),
                    },
                    plugin_id.to_string(),
                    Box::new(node),
                    PluginGuard::Clap(guard),
                );
            }
            Err(err) => {
                log::error!("实例化 CLAP 效果器 {plugin_id} 失败：{err}");
                self.set_message(format!("插件加载失败：{err}"));
            }
        }
    }

    /// 给轨道插入 VST3 效果器节点（非实时实例化 → 引擎插线 → 守卫登记）。
    fn load_vst3_effect(&mut self, track: usize, path: &str, class_id: &str) {
        match resoz_vst3_host::Vst3Instance::spawn_rt(
            std::path::Path::new(path),
            class_id,
            resoz_vst3_host::Vst3InstanceConfig {
                sample_rate: self.engine.info().sample_rate,
                max_frames: 128,
            },
        ) {
            Ok(instance) => {
                let resoz_vst3_host::Vst3Instance { guard, node } = instance;
                self.insert_effect_common(
                    track,
                    VoiceSpec::Vst3 {
                        path: path.to_string(),
                        class_id: class_id.to_string(),
                    },
                    class_id.to_string(),
                    Box::new(node),
                    PluginGuard::Vst3(guard),
                );
            }
            Err(err) => {
                log::error!("实例化 VST3 效果器 {class_id} 失败：{err}");
                self.set_message(format!("VST3 插件加载失败：{err}"));
            }
        }
    }

    /// 清空某轨全部外部效果器（内置延迟保留）：引擎旁路移除 + 文档同步。
    fn clear_external_effects(&mut self, track: usize) {
        while let Some(effect) = self.project.graph.nodes.iter().find(|n| {
            n.track() == track
                && match n {
                    NodeDoc::Effect { voice, .. } => !matches!(voice, VoiceSpec::Builtin),
                    _ => false,
                }
        }) {
            let effect_doc = effect.node_id();
            let Some(&effect_engine) = self.node_map.get(&effect_doc) else {
                break;
            };
            // 文档线：入线源与出线目标（旁路直连对）。
            let in_from = self.project.graph.cables.iter().find_map(|c| {
                if c.kind == CableKind::Audio && c.to == PortRef::Node(effect_doc) {
                    match c.from {
                        PortRef::Node(id) => Some(id),
                        _ => None,
                    }
                } else {
                    None
                }
            });
            let out_to = self.project.graph.cables.iter().find_map(|c| {
                if c.kind == CableKind::Audio && c.from == PortRef::Node(effect_doc) {
                    Some(c.to.clone())
                } else {
                    None
                }
            });
            let (Some(in_from), Some(out_to)) = (in_from, out_to) else {
                log::warn!("轨道 {track} 效果器 {effect_doc} 连线不完整，停止清理");
                break;
            };
            if !self.engine.remove_node(effect_engine) {
                break;
            }
            self.project
                .graph
                .nodes
                .retain(|n| n.node_id() != effect_doc);
            self.project.graph.cables.retain(|c| {
                !(c.kind == CableKind::Audio
                    && (c.from == PortRef::Node(effect_doc) || c.to == PortRef::Node(effect_doc)))
            });
            self.project
                .graph
                .cables
                .push(resoz_model::CableDoc::audio(in_from, out_to));
            self.node_map.remove(&effect_doc);
            self.retire_guard(effect_engine);
        }
    }

    /// 已扫描的 CLAP 乐器（懒加载）。
    fn scanned_instruments(&mut self) -> &[resoz_clap_host::ClapPluginInfo] {
        if self.clap_plugins.is_none() {
            self.clap_plugins = Some(
                resoz_clap_host::list_clap_plugins()
                    .into_iter()
                    .filter(|info| info.is_instrument)
                    .collect(),
            );
        }
        self.clap_plugins.as_deref().unwrap_or(&[])
    }

    /// 已扫描的 CLAP 效果器（懒加载）。
    fn scanned_effects(&mut self) -> &[resoz_clap_host::ClapPluginInfo] {
        if self.clap_effects.is_none() {
            self.clap_effects = Some(
                resoz_clap_host::list_clap_plugins()
                    .into_iter()
                    .filter(|info| !info.is_instrument)
                    .collect(),
            );
        }
        self.clap_effects.as_deref().unwrap_or(&[])
    }

    /// 已扫描的 VST3 乐器（懒加载）。
    fn scanned_vst3_instruments(&mut self) -> &[resoz_vst3_host::Vst3PluginInfo] {
        if self.vst3_plugins.is_none() {
            self.vst3_plugins = Some(
                resoz_vst3_host::list_vst3_plugins()
                    .into_iter()
                    .filter(|info| info.is_instrument)
                    .collect(),
            );
        }
        self.vst3_plugins.as_deref().unwrap_or(&[])
    }

    /// 已扫描的 VST3 效果器（懒加载）。
    fn scanned_vst3_effects(&mut self) -> &[resoz_vst3_host::Vst3PluginInfo] {
        if self.vst3_effects.is_none() {
            self.vst3_effects = Some(
                resoz_vst3_host::list_vst3_plugins()
                    .into_iter()
                    .filter(|info| !info.is_instrument)
                    .collect(),
            );
        }
        self.vst3_effects.as_deref().unwrap_or(&[])
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
            .midi_tracks
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
            if self.project.midi_tracks.is_empty() {
                ui.heading("合成器");
                ui.weak("（项目没有 MIDI 轨道）");
                return;
            }
            let sel = self.selected_track.min(self.project.midi_tracks.len() - 1);
            self.selected_track = sel;
            let track_title = if self.project.midi_tracks[sel].name.is_empty() {
                format!("轨道 {}", sel + 1)
            } else {
                self.project.midi_tracks[sel].name.clone()
            };
            ui.heading(format!("合成器 — {track_title}"));
            ui.horizontal(|ui| {
                ui.label("声源");
                let is_external = self
                    .project
                    .graph
                    .instrument_of_track(sel)
                    .and_then(|n| match n {
                        NodeDoc::Instrument { voice, .. } => Some(matches!(
                            voice,
                            VoiceSpec::Clap { .. } | VoiceSpec::Vst3 { .. }
                        )),
                        _ => None,
                    })
                    .unwrap_or(false);
                if ui.selectable_label(!is_external, "内置合成器").clicked() {
                    self.set_builtin_instrument(sel);
                }
                // 先收集选择，菜单闭包结束后再装填（避免借用冲突）。
                let mut picked: Option<(String, String)> = None;
                ui.menu_button("CLAP 乐器…", |ui| {
                    let empty: Vec<resoz_clap_host::ClapPluginInfo> = Vec::new();
                    let plugins = self.scanned_instruments();
                    let list = if plugins.is_empty() { &empty } else { plugins };
                    if list.is_empty() {
                        ui.weak("（未发现乐器插件；可用 RESOZ_CLAP_PATH 指定目录）");
                    }
                    for info in list {
                        if ui.button(&info.name).clicked() {
                            picked = Some((info.path.display().to_string(), info.id.clone()));
                            ui.close();
                        }
                    }
                });
                if let Some((path, plugin_id)) = picked {
                    if let Some(NodeDoc::Instrument { voice, .. }) =
                        self.project.graph.instrument_of_track_mut(sel)
                    {
                        *voice = VoiceSpec::Clap {
                            path: path.clone(),
                            plugin_id: plugin_id.clone(),
                        };
                    }
                    self.load_clap_voice(sel, &path, &plugin_id);
                }
                let mut vst3_picked: Option<(String, String)> = None;
                ui.menu_button("VST3 乐器…", |ui| {
                    let empty: Vec<resoz_vst3_host::Vst3PluginInfo> = Vec::new();
                    let plugins = self.scanned_vst3_instruments();
                    let list = if plugins.is_empty() { &empty } else { plugins };
                    if list.is_empty() {
                        ui.weak("（未发现 VST3 插件；可用 RESOZ_VST3_PATH 指定目录）");
                    }
                    for info in list {
                        if ui.button(&info.name).clicked() {
                            vst3_picked = Some((info.path.display().to_string(), info.cid.clone()));
                            ui.close();
                        }
                    }
                });
                if let Some((path, class_id)) = vst3_picked {
                    if let Some(NodeDoc::Instrument { voice, .. }) =
                        self.project.graph.instrument_of_track_mut(sel)
                    {
                        *voice = VoiceSpec::Vst3 {
                            path: path.clone(),
                            class_id: class_id.clone(),
                        };
                    }
                    self.load_vst3_voice(sel, &path, &class_id);
                }
                if is_external {
                    ui.weak("（外部乐器插件）");
                }
                ui.separator();
                ui.label("效果器");
                // 先收集选择，菜单闭包结束后再装填。
                let mut effect_picked: Option<(String, String)> = None;
                let mut clear_effects = false;
                ui.menu_button("CLAP 效果器…", |ui| {
                    if ui.button("清空全部效果器").clicked() {
                        clear_effects = true;
                        ui.close();
                    }
                    let empty: Vec<resoz_clap_host::ClapPluginInfo> = Vec::new();
                    let plugins = self.scanned_effects();
                    let list = if plugins.is_empty() { &empty } else { plugins };
                    if list.is_empty() {
                        ui.weak("（未发现效果器插件）");
                    }
                    for info in list {
                        if ui.button(&info.name).clicked() {
                            effect_picked =
                                Some((info.path.display().to_string(), info.id.clone()));
                            ui.close();
                        }
                    }
                });
                if clear_effects {
                    self.clear_external_effects(sel);
                }
                if let Some((path, plugin_id)) = effect_picked {
                    self.load_clap_effect(sel, &path, &plugin_id);
                }
                let mut vst3_effect_picked: Option<(String, String)> = None;
                ui.menu_button("VST3 效果器…", |ui| {
                    let empty: Vec<resoz_vst3_host::Vst3PluginInfo> = Vec::new();
                    let plugins = self.scanned_vst3_effects();
                    let list = if plugins.is_empty() { &empty } else { plugins };
                    if list.is_empty() {
                        ui.weak("（未发现 VST3 效果器插件）");
                    }
                    for info in list {
                        if ui.button(&info.name).clicked() {
                            vst3_effect_picked =
                                Some((info.path.display().to_string(), info.cid.clone()));
                            ui.close();
                        }
                    }
                });
                if let Some((path, class_id)) = vst3_effect_picked {
                    self.load_vst3_effect(sel, &path, &class_id);
                }
            });

            // VST3 插件参数列表（实例化时控制侧读出；展示默认归一化值）。
            if let Some(params) = self.voice_params.get(&sel) {
                if !params.is_empty() {
                    egui::CollapsingHeader::new(format!("插件参数（{}）", params.len()))
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::Grid::new("vst3_params")
                                .num_columns(3)
                                .spacing([16.0, 4.0])
                                .show(ui, |ui| {
                                    for param in params {
                                        ui.label(&param.title);
                                        ui.weak(format!("id {}", param.id));
                                        ui.weak(format!(
                                            "默认 {:.3}{}",
                                            param.default_normalized,
                                            if param.can_automate {
                                                ""
                                            } else {
                                                "（不可自动化）"
                                            }
                                        ));
                                        ui.end_row();
                                    }
                                });
                        });
                }
            }

            // CLAP 插件参数列表（实例化时经 params 扩展枚举）。
            if let Some(params) = self.clap_voice_params.get(&sel) {
                if !params.is_empty() {
                    egui::CollapsingHeader::new(format!("插件参数 CLAP（{}）", params.len()))
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::Grid::new("clap_params")
                                .num_columns(3)
                                .spacing([16.0, 4.0])
                                .show(ui, |ui| {
                                    for param in params {
                                        ui.label(&param.name);
                                        ui.weak(format!("id {}", param.id));
                                        ui.weak(format!(
                                            "默认 {:.3} [{:.3}..{:.3}]",
                                            param.default_value, param.min_value, param.max_value
                                        ));
                                        ui.end_row();
                                    }
                                });
                        });
                }
            }
            ui.weak("节点图：声源 → 延迟 → 通道条 → 主输出（文档全显式，改动实时送达音频线程）");
            ui.add_space(8.0);

            // 滤波器 / 包络（绑定到选中轨道的文档节点字段）。
            egui::Grid::new("synth_params")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    ui.label("截止频率");
                    let mut cutoff = self.synth_of_track(sel).unwrap_or_default().cutoff_hz;
                    let r = ui.add(
                        egui::Slider::new(&mut cutoff, CUTOFF_RANGE)
                            .logarithmic(true)
                            .text("Hz"),
                    );
                    if r.changed() {
                        if let Some(s) = self.synth_mut(sel) {
                            s.cutoff_hz = cutoff;
                        }
                        self.send_track_param(sel, ParamId::FilterCutoff, cutoff);
                    }
                    ui.end_row();

                    ui.label("共振");
                    let mut resonance = self.synth_of_track(sel).unwrap_or_default().resonance;
                    let r = ui.add(egui::Slider::new(&mut resonance, RES_RANGE));
                    if r.changed() {
                        if let Some(s) = self.synth_mut(sel) {
                            s.resonance = resonance;
                        }
                        self.send_track_param(sel, ParamId::FilterResonance, resonance);
                    }
                    ui.end_row();

                    ui.label("滤波模式");
                    ui.horizontal(|ui| {
                        let mode = self.synth_of_track(sel).unwrap_or_default().filter_mode;
                        for (name, value) in [("低通", 0.0), ("高通", 1.0), ("带通", 2.0)] {
                            if ui
                                .selectable_label((mode - value).abs() < 0.5, name)
                                .clicked()
                            {
                                if let Some(s) = self.synth_mut(sel) {
                                    s.filter_mode = value;
                                }
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
        let synth = self.synth_of_track(sel).unwrap_or_default();
        let mut a = synth.attack_s;
        let mut d = synth.decay_s;
        let mut s = synth.sustain;
        let mut r = synth.release_s;
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
        if let Some(synth) = self.synth_mut(sel) {
            synth.attack_s = a;
            synth.decay_s = d;
            synth.sustain = s;
            synth.release_s = r;
        }
    }

    /// 延迟三滑块（湿声/时间/反馈）。
    fn delay_sliders(&mut self, ui: &mut egui::Ui) {
        let sel = self.selected_track;
        let delay = self.delay_of_track(sel).unwrap_or_default();
        let mut mix = delay.mix;
        let mut time = delay.time_s;
        let mut feedback = delay.feedback;
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
        if let Some(delay) = self.delay_mut(sel) {
            delay.mix = mix;
            delay.time_s = time;
            delay.feedback = feedback;
        }
    }

    /// 发送一条轨道参数到音频线程。
    fn send_track_param(&mut self, track: usize, id: ParamId, value: f32) {
        self.engine
            .send(EngineCommand::SetTrackParam { track, id, value });
    }

    /// 混音器：每轨 选择 / 静音 / 电平表 / 推子 / 声像（Vec 遍历，
    /// 数据源 = 文档通道条节点）。
    fn mixer_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.add_space(4.0);
        ui.label("混音器");

        // 先收集编辑，循环后统一写回并发送（避免借用冲突）。
        let num_tracks = self.project.midi_tracks.len();
        let mut mute_edits: Vec<Option<bool>> = vec![None; num_tracks];
        let mut gain_edits: Vec<Option<f32>> = vec![None; num_tracks];
        let mut pan_edits: Vec<Option<f32>> = vec![None; num_tracks];
        let mut select_edits: Option<usize> = None;

        ui.horizontal(|ui| {
            for i in 0..num_tracks {
                ui.vertical(|ui| {
                    let name = if self.project.midi_tracks[i].name.is_empty() {
                        format!("轨道 {}", i + 1)
                    } else {
                        self.project.midi_tracks[i].name.clone()
                    };
                    if ui
                        .selectable_label(self.selected_track == i, name)
                        .clicked()
                    {
                        select_edits = Some(i);
                    }

                    let (muted, _gain, _pan) = self.track_strip_state(i);
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

                    let mut gain = self.track_strip_state(i).1;
                    let fader = ui.add_sized(
                        [96.0, 110.0],
                        egui::Slider::new(&mut gain, TRACK_GAIN_RANGE).vertical(),
                    );
                    if fader.changed() {
                        gain_edits[i] = Some(gain);
                    }

                    let mut pan = self.track_strip_state(i).2;
                    let pan_resp = ui.add_sized(
                        [96.0, 20.0],
                        egui::Slider::new(&mut pan, PAN_RANGE).text("声像"),
                    );
                    if pan_resp.changed() {
                        pan_edits[i] = Some(pan);
                    }
                });
                if i + 1 < num_tracks {
                    ui.separator();
                }
            }
        });

        if let Some(i) = select_edits {
            self.selected_track = i;
        }
        for i in 0..num_tracks {
            if let Some(muted) = mute_edits[i] {
                if let Some(NodeDoc::ChannelStrip { muted: m, .. }) =
                    self.project.graph.channel_strip_of_track_mut(i)
                {
                    *m = muted;
                }
                self.send_track_param(i, ParamId::TrackMute, if muted { 1.0 } else { 0.0 });
            }
            if let Some(gain) = gain_edits[i] {
                if let Some(NodeDoc::ChannelStrip { gain: g, .. }) =
                    self.project.graph.channel_strip_of_track_mut(i)
                {
                    *g = gain;
                }
                self.send_track_param(i, ParamId::TrackGain, gain);
            }
            if let Some(pan) = pan_edits[i] {
                if let Some(NodeDoc::ChannelStrip { pan: p, .. }) =
                    self.project.graph.channel_strip_of_track_mut(i)
                {
                    *p = pan;
                }
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
                self.project.midi_tracks[sel].pattern = Pattern::default();
                self.send_pattern(sel);
            }
            if ui.button("全部 C4").clicked() {
                self.project.midi_tracks[sel]
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
            for (i, step) in self.project.midi_tracks[sel]
                .pattern
                .steps
                .iter()
                .enumerate()
            {
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
                    self.project.midi_tracks[sel].pattern.steps[i] = resoz_model::Step {
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
            &mut self.project.midi_tracks[sel].clip,
            &mut self.roll_state,
            playhead,
            playing,
        );
        if edited {
            self.send_clip(sel);
        }
    }

    /// 自动化轨道区（D12 顶层实体；每条曲线的投递终点由节点图连线决定）。
    fn automation_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.add_space(4.0);
        let auto_count = self.project.automation_tracks.len();
        ui.horizontal(|ui| {
            ui.label("自动化轨道");
            for i in 0..auto_count {
                if ui
                    .selectable_label(self.selected_auto == i, format!("{}", i + 1))
                    .clicked()
                {
                    self.selected_auto = i;
                }
            }
            if ui.button("＋ 新增").clicked() && auto_count < MAX_AUTO_TRACKS {
                self.project
                    .automation_tracks
                    .push(AutomationTrack::default());
                self.selected_auto = auto_count;
                self.send_auto_curve(self.selected_auto);
            }
            if ui.button("－ 删除").clicked() && auto_count > 0 {
                let index = self.selected_auto.min(auto_count - 1);
                self.project.automation_tracks.remove(index);
                let _ = self.engine.clear_auto_route(index);
                // 引擎侧该索引清空为空曲线。
                self.send_sequencer(SequencerCommand::SetAutoTrack {
                    index,
                    auto: RtAutoTrack {
                        loop_ticks: TICKS_PER_BAR,
                        curve: RtAutomation::default(),
                    },
                });
                self.selected_auto =
                    index.min(self.project.automation_tracks.len().saturating_sub(1));
            }
            ui.weak("点按泳道加锚点 · 拖拽移动 · 右键删除");
        });

        if auto_count == 0 {
            ui.weak("（无自动化轨道；点「＋ 新增」创建一条曲线）");
            return;
        }
        let index = self.selected_auto.min(auto_count - 1);
        let sel = self.selected_track;

        // 终点选择：本轨通道条（音量/声像）、声源（截止/插件参数）。
        // 选项携带**文档** node_id（cable 持久化身份），下发时映射。
        let mut options: Vec<(String, u64, AutoParam)> = Vec::new();
        if let Some(strip_doc) = self
            .project
            .graph
            .channel_strip_of_track(sel)
            .map(NodeDoc::node_id)
        {
            options.push(("音量".to_string(), strip_doc, AutoParam::TrackGain));
            options.push(("声像".to_string(), strip_doc, AutoParam::TrackPan));
        }
        if let Some(voice_doc) = self
            .project
            .graph
            .instrument_of_track(sel)
            .map(NodeDoc::node_id)
        {
            options.push(("截止".to_string(), voice_doc, AutoParam::FilterCutoff));
            if let Some(params) = self.clap_voice_params.get(&sel) {
                for param in params {
                    let label = if param.name.is_empty() {
                        format!("参数 {}", param.id)
                    } else {
                        param.name.clone()
                    };
                    options.push((
                        format!("CLAP {label}"),
                        voice_doc,
                        AutoParam::PluginParam {
                            param_id: param.id,
                            name: label,
                        },
                    ));
                }
            }
            if let Some(params) = self.voice_params.get(&sel) {
                for param in params {
                    let label = if param.title.is_empty() {
                        format!("参数 {}", param.id)
                    } else {
                        param.title.clone()
                    };
                    options.push((
                        format!("VST3 {label}"),
                        voice_doc,
                        AutoParam::PluginParam {
                            param_id: param.id,
                            name: label,
                        },
                    ));
                }
            }
        }

        let mut picked: Option<(u64, AutoParam)> = None;
        ui.horizontal(|ui| {
            ui.label("目标");
            ui.menu_button("选择参数…", |ui| {
                for (label, doc_id, param) in &options {
                    if ui.button(label).clicked() {
                        picked = Some((*doc_id, param.clone()));
                        ui.close();
                    }
                }
            });
            if let Some((doc_id, param)) = picked {
                self.set_auto_endpoint(index, doc_id, param);
            }
            // 当前终点显示。
            let current = self
                .project
                .graph
                .auto_endpoint(index)
                .map(|param| match &param {
                    AutoParam::TrackGain => "音量".to_string(),
                    AutoParam::TrackPan => "声像".to_string(),
                    AutoParam::FilterCutoff => "截止".to_string(),
                    AutoParam::PluginParam { name, .. } => name.clone(),
                })
                .unwrap_or_else(|| "未连线".to_string());
            ui.weak(format!("→ {current}"));
            let mut enabled = self.project.automation_tracks[index].enabled;
            if ui.checkbox(&mut enabled, "启用").changed() {
                self.project.automation_tracks[index].enabled = enabled;
                self.send_auto_curve(index);
            }
        });

        // 泳道循环长度随轨道音源口径，并同步回写文档。
        let loop_ticks = match self.project.midi_tracks[sel].source {
            resoz_model::TrackSource::Clip => self.project.midi_tracks[sel].clip.loop_ticks,
            _ => TICKS_PER_BAR,
        };
        let playhead = self.engine.musical_ticks();
        let playing = self.engine.is_playing();
        let axis_param = self.project.graph.auto_endpoint(index).unwrap_or_default();
        if let Some(curve) = self.project.automation_tracks.get_mut(index) {
            curve.loop_ticks = loop_ticks;
            let edited = lane::show(
                ui,
                curve,
                &axis_param,
                loop_ticks,
                playhead,
                playing,
                &mut self.lane_state,
            );
            if edited {
                self.send_auto_curve(index);
            }
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

        // 回收已卸装的插件节点：在控制线程释放，并按 node_id 丢弃
        // 退役区里配对的守卫（安全卸载动态库；在役守卫不受影响）。
        for node_id in self.engine.reclaim_nodes() {
            if let Some(pos) = self
                .retiring_guards
                .iter()
                .position(|entry| entry.node_id == node_id)
            {
                self.retiring_guards.remove(pos);
            }
        }

        // 走带停止沿：自动化不再覆盖，恢复各轨手动参数值。
        let playing = self.engine.is_playing();
        if self.was_playing && !playing {
            for track in 0..self.project.midi_tracks.len() {
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
