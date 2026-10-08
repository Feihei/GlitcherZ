#![forbid(unsafe_code)]

//! # resoz-model
//!
//! ResoZ 数据模型：MIDI 轨道、自动化轨道、节点图与项目文件（文档层）。
//!
//! 约定（见 AGENTS.md「数据模型」）：
//! - 所有类型必须 `Serialize + Deserialize`（RON 格式）；
//! - 新增字段一律加 `#[serde(default)]`，保证旧项目文件向前兼容；
//! - 模型是"文档"，引擎是"播放器"：本 crate 不依赖 `resoz-engine`，
//!   引擎也不依赖本 crate —— 由 `resoz-app` / `resoz-ui` 做转换粘合。
//!
//! v3（spec `docs/spec/node-graph.md` §7 终态，D11-D13）：
//! - `MidiTrack`（音符面）/ `AutomationTrack`（纯曲线）顶层 `Vec`（D12）；
//! - `GraphDoc{nodes, cables}` 全显式：默认拓扑也写入文档，加载器纯按
//!   文档重建引擎图（D13）；
//! - `node_id` 随文件持久化（D11）；id 0 无效，id 1 = 主输出保留
//!   （[`MASTER_NODE_ID`]），文档节点从 2 起。

pub mod file;

pub use file::{
    ModelError, PROJECT_VERSION, ProjectFile, load_project, parse_project, save_project,
};

/// 步进音序器的步数。
pub const PATTERN_STEPS: usize = 16;

/// MIDI 轨道容量上限（D12；引擎侧定容同值）。
pub const MAX_MIDI_TRACKS: usize = 16;

/// 自动化轨道容量上限（D12；引擎侧 `MAX_AUTO_TRACKS` 同值）。
pub const MAX_AUTO_TRACKS: usize = 16;

/// 节点图容量上限（D1；引擎侧同值）。
pub const MAX_NODES: usize = 64;

/// 连线容量上限（D1；引擎侧同值）。
pub const MAX_CABLES: usize = 128;

/// 默认（新建项目）的 MIDI 轨道数。
pub const DEFAULT_MIDI_TRACKS: usize = 4;

/// 主输出的保留 node_id（D6：固定存在、不可删除、不入文档节点表）。
pub const MASTER_NODE_ID: u64 = 1;

/// 单个步进：开关 + 音符 + 力度。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Step {
    /// 该步是否触发。
    #[serde(default)]
    pub enabled: bool,
    /// MIDI 音符编号（69 = A4）。
    #[serde(default = "default_note")]
    pub note: u8,
    /// 力度（0..1，音频路径参数用 f32）。
    #[serde(default = "default_velocity")]
    pub velocity: f32,
}

fn default_note() -> u8 {
    60
}

fn default_velocity() -> f32 {
    0.9
}

impl Default for Step {
    fn default() -> Self {
        Self {
            enabled: false,
            note: default_note(),
            velocity: default_velocity(),
        }
    }
}

/// 步进音序模式：16 步单音轨。
///
/// 固定长度数组（而非 `Vec`）是有意的：`Pattern` 保持 `Copy`，
/// 可整块经无锁命令队列送往音频线程，无需任何堆分配。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pattern {
    #[serde(default = "default_steps")]
    pub steps: [Step; PATTERN_STEPS],
}

fn default_steps() -> [Step; PATTERN_STEPS] {
    [Step::default(); PATTERN_STEPS]
}

impl Default for Pattern {
    fn default() -> Self {
        Self {
            steps: default_steps(),
        }
    }
}

impl Pattern {
    /// 第 0 步置为指定音符（便捷构造，测试/演示用）。
    pub fn single_note(note: u8) -> Self {
        let mut steps = default_steps();
        steps[0] = Step {
            enabled: true,
            note,
            velocity: 0.9,
        };
        Self { steps }
    }
}

/// 每轨合成器音色参数。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SynthParams {
    /// 滤波器截止频率（Hz）。
    #[serde(default = "default_cutoff")]
    pub cutoff_hz: f32,
    /// 滤波器共振（Simper SVF res，0..0.98）。
    #[serde(default = "default_resonance")]
    pub resonance: f32,
    /// 滤波器模式：0 = 低通，1 = 高通，2 = 带通。
    #[serde(default)]
    pub filter_mode: f32,
    /// 包络 Attack（秒）。
    #[serde(default = "default_attack")]
    pub attack_s: f32,
    /// 包络 Decay（秒）。
    #[serde(default = "default_decay")]
    pub decay_s: f32,
    /// 包络 Sustain（0..1）。
    #[serde(default)]
    pub sustain: f32,
    /// 包络 Release（秒）。
    #[serde(default = "default_release")]
    pub release_s: f32,
}

fn default_cutoff() -> f32 {
    12_000.0
}

fn default_resonance() -> f32 {
    0.2
}

fn default_attack() -> f32 {
    0.01
}

fn default_decay() -> f32 {
    0.10
}

fn default_sustain() -> f32 {
    0.8
}

fn default_release() -> f32 {
    0.20
}

impl Default for SynthParams {
    fn default() -> Self {
        Self {
            cutoff_hz: default_cutoff(),
            resonance: default_resonance(),
            filter_mode: 0.0,
            attack_s: default_attack(),
            decay_s: default_decay(),
            sustain: default_sustain(),
            release_s: default_release(),
        }
    }
}

/// 每轨延迟效果参数（`mix = 0` 表示旁通）。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DelayParams {
    /// 延迟时间（秒，0.005..1）。
    #[serde(default = "default_delay_time")]
    pub time_s: f32,
    /// 反馈量（0..0.9）。
    #[serde(default = "default_delay_feedback")]
    pub feedback: f32,
    /// 湿声混合比（0..1）。
    #[serde(default)]
    pub mix: f32,
}

fn default_delay_time() -> f32 {
    0.3
}

fn default_delay_feedback() -> f32 {
    0.35
}

impl Default for DelayParams {
    fn default() -> Self {
        Self {
            time_s: default_delay_time(),
            feedback: default_delay_feedback(),
            mix: 0.0,
        }
    }
}

/// 音符时间基准：每四分音符的 tick 数（1/96 四分音符 ≈ 10.4 ms @120 BPM）。
pub const TICKS_PER_QUARTER: u32 = 96;

/// 每个步进（16 分音符）的 tick 数。
pub const TICKS_PER_STEP: u32 = 24;

/// 一小节（4/4）的 tick 数 = 16 步 × 24。
pub const TICKS_PER_BAR: u32 = TICKS_PER_STEP * 16;

/// 钢琴卷帘中的一个音符块。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Note {
    /// 起始位置（tick，相对片段起点）。
    #[serde(default)]
    pub start: u32,
    /// 时长（tick）。
    #[serde(default = "default_note_length")]
    pub length: u32,
    /// MIDI 音符编号。
    #[serde(default)]
    pub note: u8,
    /// 力度（0..1）。
    #[serde(default = "default_velocity")]
    pub velocity: f32,
}

fn default_note_length() -> u32 {
    TICKS_PER_STEP
}

impl Default for Note {
    fn default() -> Self {
        Self {
            start: 0,
            length: default_note_length(),
            note: 60,
            velocity: default_velocity(),
        }
    }
}

/// 钢琴卷帘片段：一组音符 + 循环长度。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Clip {
    /// 循环长度（tick），默认一小节（4/4）。
    #[serde(default = "default_clip_loop")]
    pub loop_ticks: u32,
    /// 音符集合。
    #[serde(default)]
    pub notes: Vec<Note>,
}

fn default_clip_loop() -> u32 {
    TICKS_PER_BAR
}

impl Default for Clip {
    fn default() -> Self {
        Self {
            loop_ticks: default_clip_loop(),
            notes: Vec::new(),
        }
    }
}

/// 节点声源：内置（合成器/延迟）或 CLAP / VST3 插件。
///
/// 语义随节点类型而定：`Instrument` 节点的 `Builtin` = 内置合成器，
/// `Effect` 节点的 `Builtin` = 内置延迟。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VoiceSpec {
    /// 内置（默认，旧项目文件向前兼容）。
    #[default]
    Builtin,
    /// CLAP 插件：动态库路径 + 插件 ID（实例化在非实时线程）。
    Clap {
        #[serde(default)]
        path: String,
        #[serde(default)]
        plugin_id: String,
    },
    /// VST3 插件：.vst3 文件/Bundle 路径 + 类 CID（16 字节十六进制）。
    Vst3 {
        #[serde(default)]
        path: String,
        #[serde(default)]
        class_id: String,
    },
}

/// 轨道音源：16 步步进模式或钢琴卷帘片段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum TrackSource {
    /// 步进音序模式（默认，向前兼容旧项目文件）。
    #[default]
    Steps,
    /// 钢琴卷帘片段。
    Clip,
}

/// 一条 MIDI 轨的音符面（D12 v3：从旧 `Track` 拆出）。
///
/// 音色/混音/效果不再是轨道字段——它们是节点图里的
/// [`NodeDoc::Instrument`] / [`NodeDoc::Effect`] / [`NodeDoc::ChannelStrip`]。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct MidiTrack {
    /// 轨道名（空则 UI 显示"轨道 N"）。
    #[serde(default)]
    pub name: String,
    /// 该轨的步进模式（`source = Steps` 时播放）。
    #[serde(default)]
    pub pattern: Pattern,
    /// 该轨的卷帘片段（`source = Clip` 时播放）。
    #[serde(default)]
    pub clip: Clip,
    /// 音源选择。
    #[serde(default)]
    pub source: TrackSource,
}

/// 可自动化 / 可连线的节点参数（自动化连线终点负载）。
///
/// D7：自动化曲线本身不含目标——目标 `(node_id, param)` 由
/// [`CableDoc`] 携带；本枚举即其中 `param` 的文档表示。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub enum AutoParam {
    /// 轨道增益（线性，0..1.5）。
    #[default]
    TrackGain,
    /// 轨道声像（-1..1）。
    TrackPan,
    /// 滤波器截止频率（Hz，20..18000，UI 对数轴）。
    FilterCutoff,
    /// CLAP 插件参数（自动化桥接到 `ParamValueEvent`；name 供 UI 显示）。
    PluginParam {
        /// 插件侧参数 ID。
        #[serde(default)]
        param_id: u32,
        /// 参数显示名（编辑期快照，序列化保存）。
        #[serde(default)]
        name: String,
    },
}

/// 自动化锚点：tick 位置 + 参数真实值。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct AutoPoint {
    /// 位置（tick，相对循环起点）。
    #[serde(default)]
    pub tick: u32,
    /// 参数值（真实单位，非归一化）。
    #[serde(default)]
    pub value: f32,
}

/// 一条自动化轨道（D7/D12 v3：顶层实体，纯锚点曲线，不含目标）。
///
/// 锚点按 tick 升序存放；取值在相邻锚点间线性插值，两端外保持端值。
/// 空曲线或未启用时不产生任何自动化。投递终点 `(node_id, param)` 由
/// 节点图里的 [`CableKind::Automation`] 连线决定。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct AutomationTrack {
    /// 是否启用。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 循环长度（tick）：求值位置按此取模。默认一小节（384）。
    #[serde(default = "default_loop_ticks")]
    pub loop_ticks: u32,
    /// 锚点（按 tick 升序）。
    #[serde(default)]
    pub points: Vec<AutoPoint>,
}

fn default_loop_ticks() -> u32 {
    TICKS_PER_BAR
}

fn default_true() -> bool {
    true
}

impl AutomationTrack {
    /// 在 `tick` 处取值：空/未启用返回 `None`；两点间线性插值；
    /// 首尾之外钳到端点值。要求锚点有序（编辑接口负责维护）。
    pub fn value_at(&self, tick: f64) -> Option<f32> {
        if !self.enabled || self.points.is_empty() {
            return None;
        }
        let first = self.points[0];
        let last = *self.points.last()?;
        if tick <= f64::from(first.tick) {
            return Some(first.value);
        }
        if tick >= f64::from(last.tick) {
            return Some(last.value);
        }
        // 线性扫描即可：锚点数有限，调用频率为每块每曲线一次。
        for window in self.points.windows(2) {
            let (a, b) = (window[0], window[1]);
            if tick < f64::from(b.tick) {
                let span = f64::from(b.tick) - f64::from(a.tick);
                let ratio = if span > 0.0 {
                    (tick - f64::from(a.tick)) / span
                } else {
                    0.0
                };
                return Some(a.value + (b.value - a.value) * ratio as f32);
            }
        }
        Some(last.value)
    }

    /// 插入或更新锚点（同 tick 覆盖），并保持按 tick 升序。
    pub fn upsert_point(&mut self, point: AutoPoint) {
        self.points.retain(|p| p.tick != point.tick);
        let pos = self.points.partition_point(|p| p.tick < point.tick);
        self.points.insert(pos, point);
    }

    /// 移除 `tick` 容差范围内最近的锚点；命中返回 `true`。
    pub fn remove_near(&mut self, tick: u32, tolerance: u32) -> bool {
        let Some(best) = self
            .points
            .iter()
            .enumerate()
            .filter(|(_, p)| p.tick.abs_diff(tick) <= tolerance)
            .min_by_key(|(_, p)| p.tick.abs_diff(tick))
            .map(|(index, _)| index)
        else {
            return false;
        };
        self.points.remove(best);
        true
    }
}

/// 文档节点（spec §7 `NodeDoc`）：引擎图的节点身份 + 参数持久化。
///
/// `track` 是节点的**主属轨道**（哪个轨道的面板控制它）——引擎侧的
/// 轨道路由表（手动参数/电平/键盘寻址）按此装配；真正的信号流向由
/// [`GraphDoc::cables`] 决定，二者解耦。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeDoc {
    /// 乐器节点（音频源头，MIDI 入口）。`Builtin` = 内置合成器。
    Instrument {
        /// 持久化节点 ID（≥ 2；1 = 主输出保留）。
        #[serde(default)]
        node_id: u64,
        /// 主属轨道。
        #[serde(default)]
        track: usize,
        /// 声源规格。
        #[serde(default)]
        voice: VoiceSpec,
        /// 内置合成器音色（外部插件忽略）。
        #[serde(default)]
        synth: SynthParams,
    },
    /// 效果器节点（1 入 1 出）。`Builtin` = 内置延迟。
    Effect {
        /// 持久化节点 ID。
        #[serde(default)]
        node_id: u64,
        /// 主属轨道。
        #[serde(default)]
        track: usize,
        /// 效果器规格。
        #[serde(default)]
        voice: VoiceSpec,
        /// 内置延迟参数（外部插件忽略）。
        #[serde(default)]
        delay: DelayParams,
    },
    /// 通道条节点（增益/声像/静音，≤8 入边求和）。
    ChannelStrip {
        /// 持久化节点 ID。
        #[serde(default)]
        node_id: u64,
        /// 主属轨道。
        #[serde(default)]
        track: usize,
        /// 轨道增益（线性，0..1.5）。
        #[serde(default = "default_track_gain")]
        gain: f32,
        /// 声像（-1..1，线性声像律）。
        #[serde(default)]
        pan: f32,
        /// 静音。
        #[serde(default)]
        muted: bool,
    },
}

fn default_track_gain() -> f32 {
    0.8
}

impl NodeDoc {
    /// 节点 ID。
    pub fn node_id(&self) -> u64 {
        match self {
            NodeDoc::Instrument { node_id, .. }
            | NodeDoc::Effect { node_id, .. }
            | NodeDoc::ChannelStrip { node_id, .. } => *node_id,
        }
    }

    /// 主属轨道。
    pub fn track(&self) -> usize {
        match self {
            NodeDoc::Instrument { track, .. }
            | NodeDoc::Effect { track, .. }
            | NodeDoc::ChannelStrip { track, .. } => *track,
        }
    }
}

/// 连线端口引用（spec §7 `PortRef`）。
///
/// `Master` / `MidiTrack` / `AutoTrack` 是固定实体的保留身份（不入
/// 节点表，加载器/引擎隐式补齐）；`Node` 引用文档节点。
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortRef {
    /// 文档节点（node_id）。
    Node(u64),
    /// 主输出（固定节点，D6）。
    #[default]
    Master,
    /// 时间轴 MIDI 轨出口（轨道下标）。
    MidiTrack(usize),
    /// 时间轴自动化轨出口（轨道下标）。
    AutoTrack(usize),
}

/// 连线类型（D3）：与端口类型绑定，类型不匹配在加载期拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CableKind {
    /// 音频线：节点输出 → 节点输入 / 主输出。
    #[default]
    Audio,
    /// MIDI 线：时间轴 MIDI 轨 → 乐器节点。
    Midi,
    /// 自动化线：时间轴自动化轨 → 节点参数口。
    Automation,
}

/// 文档连线（spec §7 `CableDoc`）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CableDoc {
    /// 连线类型。
    #[serde(default)]
    pub kind: CableKind,
    /// 源端口。
    #[serde(default)]
    pub from: PortRef,
    /// 目标端口。
    #[serde(default)]
    pub to: PortRef,
    /// 终点参数（仅 `kind = Automation` 时使用；D7）。
    #[serde(default)]
    pub param: AutoParam,
}

impl CableDoc {
    /// 音频连线便捷构造。
    pub fn audio(from: impl Into<PortRef>, to: impl Into<PortRef>) -> Self {
        Self {
            kind: CableKind::Audio,
            from: from.into(),
            to: to.into(),
            param: AutoParam::default(),
        }
    }

    /// MIDI 连线便捷构造。
    pub fn midi(from_track: usize, to_node: u64) -> Self {
        Self {
            kind: CableKind::Midi,
            from: PortRef::MidiTrack(from_track),
            to: PortRef::Node(to_node),
            param: AutoParam::default(),
        }
    }

    /// 自动化连线便捷构造。
    pub fn automation(from_track: usize, to_node: u64, param: AutoParam) -> Self {
        Self {
            kind: CableKind::Automation,
            from: PortRef::AutoTrack(from_track),
            to: PortRef::Node(to_node),
            param,
        }
    }
}

impl From<u64> for PortRef {
    fn from(node_id: u64) -> Self {
        PortRef::Node(node_id)
    }
}

/// 节点图的文档表示（v3 终态，spec §7）：全显式 nodes + cables。
///
/// 默认拓扑（每轨 `声源 → 延迟 → 通道条 → 主输出` + MIDI 自连）也
/// 完整写入文档（D13）：加载器**纯按文档重建**引擎图，文档与引擎
/// 一张皮。主输出与时间轴是固定实体，不出现在 `nodes` 里，连线以
/// [`PortRef::Master`] / [`PortRef::MidiTrack`] / [`PortRef::AutoTrack`]
/// 引用。
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct GraphDoc {
    /// 节点表（≤ [`MAX_NODES`]；node_id 唯一且 ≥ 2）。
    #[serde(default)]
    pub nodes: Vec<NodeDoc>,
    /// 连线表（≤ [`MAX_CABLES`]）。
    #[serde(default)]
    pub cables: Vec<CableDoc>,
}

impl GraphDoc {
    /// 文档内的下一个可用 node_id（max + 1，且不小于 2）。
    pub fn next_node_id(&self) -> u64 {
        self.nodes
            .iter()
            .map(NodeDoc::node_id)
            .max()
            .unwrap_or(MASTER_NODE_ID)
            .max(MASTER_NODE_ID)
            + 1
    }

    /// 按 node_id 查节点。
    pub fn node_by_id(&self, node_id: u64) -> Option<&NodeDoc> {
        self.nodes.iter().find(|n| n.node_id() == node_id)
    }

    /// 某轨道的乐器节点（首个 `track` 匹配的 `Instrument`）。
    pub fn instrument_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::Instrument { .. }) && n.track() == track)
    }

    /// 某轨道的内置延迟节点（延迟参数的文档宿主）。
    pub fn builtin_delay_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes.iter().find(|n| {
            matches!(
                n,
                NodeDoc::Effect {
                    voice: VoiceSpec::Builtin,
                    ..
                }
            ) && n.track() == track
        })
    }

    /// 某轨道的首个效果器节点。
    pub fn effect_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::Effect { .. }) && n.track() == track)
    }

    /// 某轨道的通道条节点。
    pub fn channel_strip_of_track(&self, track: usize) -> Option<&NodeDoc> {
        self.nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::ChannelStrip { .. }) && n.track() == track)
    }

    /// 某轨道的通道条节点（可变）。
    pub fn channel_strip_of_track_mut(&mut self, track: usize) -> Option<&mut NodeDoc> {
        self.nodes
            .iter_mut()
            .find(|n| matches!(n, NodeDoc::ChannelStrip { .. }) && n.track() == track)
    }

    /// 某轨道的乐器节点（可变）。
    pub fn instrument_of_track_mut(&mut self, track: usize) -> Option<&mut NodeDoc> {
        self.nodes
            .iter_mut()
            .find(|n| matches!(n, NodeDoc::Instrument { .. }) && n.track() == track)
    }

    /// 某轨道的内置延迟节点（可变）。
    pub fn builtin_delay_of_track_mut(&mut self, track: usize) -> Option<&mut NodeDoc> {
        self.nodes.iter_mut().find(|n| {
            matches!(
                n,
                NodeDoc::Effect {
                    voice: VoiceSpec::Builtin,
                    ..
                }
            ) && n.track() == track
        })
    }

    /// 自动化轨道 `index` 的连线终点参数（未连线返回 `None`）。
    pub fn auto_endpoint(&self, index: usize) -> Option<AutoParam> {
        self.cables.iter().find_map(|c| {
            if c.kind == CableKind::Automation && c.from == PortRef::AutoTrack(index) {
                Some(c.param.clone())
            } else {
                None
            }
        })
    }

    /// 移除自动化轨道 `index` 的连线（改终点前调用）。
    pub fn remove_auto_cable(&mut self, index: usize) {
        self.cables
            .retain(|c| !(c.kind == CableKind::Automation && c.from == PortRef::AutoTrack(index)));
    }

    /// 加载期校验（文件层是最后防线，报错而非截断）。
    ///
    /// `midi_count` / `auto_count` 为项目当前的轨道数（端口引用越界
    /// 即非法）。
    pub fn validate(&self, midi_count: usize, auto_count: usize) -> Result<(), String> {
        if self.nodes.len() > MAX_NODES {
            return Err(format!("节点数 {} 超过上限 {MAX_NODES}", self.nodes.len()));
        }
        if self.cables.len() > MAX_CABLES {
            return Err(format!(
                "连线数 {} 超过上限 {MAX_CABLES}",
                self.cables.len()
            ));
        }
        for (i, node_id) in self.nodes.iter().map(NodeDoc::node_id).enumerate() {
            if node_id <= MASTER_NODE_ID {
                return Err(format!(
                    "节点 #{i} 的 node_id = {node_id} 非法（0 无效，1 = 主输出保留）"
                ));
            }
            if self.nodes[i + 1..].iter().any(|n| n.node_id() == node_id) {
                return Err(format!("node_id {node_id} 重复"));
            }
        }
        let node_ids: Vec<u64> = self.nodes.iter().map(NodeDoc::node_id).collect();
        for cable in &self.cables {
            self.check_port(&cable.from, &node_ids, midi_count, auto_count)?;
            self.check_port(&cable.to, &node_ids, midi_count, auto_count)?;
            match cable.kind {
                CableKind::Audio => {
                    if !matches!(cable.from, PortRef::Node(_)) {
                        return Err("音频线源端口必须是节点".to_string());
                    }
                    if !matches!(cable.to, PortRef::Node(_) | PortRef::Master) {
                        return Err("音频线目标端口必须是节点或主输出".to_string());
                    }
                }
                CableKind::Midi => {
                    if !matches!(cable.from, PortRef::MidiTrack(_)) {
                        return Err("MIDI 线源端口必须是时间轴 MIDI 轨".to_string());
                    }
                    match cable.to {
                        PortRef::Node(id) => {
                            if !self.nodes.iter().any(|n| {
                                n.node_id() == id && matches!(n, NodeDoc::Instrument { .. })
                            }) {
                                return Err(format!("MIDI 线目标 node_id {id} 不是乐器节点"));
                            }
                        }
                        _ => return Err("MIDI 线目标端口必须是乐器节点".to_string()),
                    }
                }
                CableKind::Automation => {
                    if !matches!(cable.from, PortRef::AutoTrack(_)) {
                        return Err("自动化线源端口必须是时间轴自动化轨".to_string());
                    }
                    if !matches!(cable.to, PortRef::Node(_)) {
                        return Err("自动化线目标端口必须是节点参数口".to_string());
                    }
                }
            }
        }
        Ok(())
    }

    fn check_port(
        &self,
        port: &PortRef,
        node_ids: &[u64],
        midi_count: usize,
        auto_count: usize,
    ) -> Result<(), String> {
        match port {
            PortRef::Node(id) => {
                if !node_ids.contains(id) {
                    return Err(format!("连线引用了不存在的节点 node_id {id}"));
                }
            }
            PortRef::MidiTrack(index) => {
                if *index >= midi_count {
                    return Err(format!("MIDI 轨端口 {index} 越界（轨道数 = {midi_count}）"));
                }
            }
            PortRef::AutoTrack(index) => {
                if *index >= auto_count {
                    return Err(format!(
                        "自动化轨端口 {index} 越界（轨道数 = {auto_count}）"
                    ));
                }
            }
            PortRef::Master => {}
        }
        Ok(())
    }
}

/// 生成默认节点图文档（D13 全显式；`Project::default` 与新建项目用）：
/// 每轨 `声源 → 延迟 → 通道条 → 主输出` + 本轨 MIDI 自连。
///
/// node_id 从 [`MASTER_NODE_ID`] + 1 起连续分配。
pub fn default_graph_doc(track_count: usize) -> GraphDoc {
    let mut nodes = Vec::new();
    let mut cables = Vec::new();
    let mut next_id = MASTER_NODE_ID + 1;
    for track in 0..track_count {
        let voice_id = next_id;
        let delay_id = next_id + 1;
        let strip_id = next_id + 2;
        next_id += 3;
        nodes.push(NodeDoc::Instrument {
            node_id: voice_id,
            track,
            voice: VoiceSpec::Builtin,
            synth: SynthParams::default(),
        });
        nodes.push(NodeDoc::Effect {
            node_id: delay_id,
            track,
            voice: VoiceSpec::Builtin,
            delay: DelayParams::default(),
        });
        nodes.push(NodeDoc::ChannelStrip {
            node_id: strip_id,
            track,
            gain: default_track_gain(),
            pan: 0.0,
            muted: false,
        });
        cables.push(CableDoc::audio(voice_id, delay_id));
        cables.push(CableDoc::audio(delay_id, strip_id));
        cables.push(CableDoc::audio(strip_id, PortRef::Master));
        cables.push(CableDoc::midi(track, voice_id));
    }
    GraphDoc { nodes, cables }
}

/// 多轨项目文档（v3）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Project {
    /// 项目元数据（名称、速度等）。
    #[serde(default)]
    pub metadata: ProjectMetadata,
    /// 音序器 gate 占步长比例（0.05..1）。
    #[serde(default = "default_gate")]
    pub gate: f32,
    /// 主输出增益（线性）。
    #[serde(default = "default_master_gain")]
    pub master_gain: f32,
    /// MIDI 轨道（音符面，D12 动态化，上限 [`MAX_MIDI_TRACKS`]）。
    #[serde(default)]
    pub midi_tracks: Vec<MidiTrack>,
    /// 自动化轨道（顶层实体，D7/D12，上限 [`MAX_AUTO_TRACKS`]）。
    #[serde(default)]
    pub automation_tracks: Vec<AutomationTrack>,
    /// 节点图文档（D13 全显式）。
    #[serde(default)]
    pub graph: GraphDoc,
}

fn default_gate() -> f32 {
    0.8
}

fn default_master_gain() -> f32 {
    0.8
}

impl Default for Project {
    fn default() -> Self {
        Self {
            metadata: ProjectMetadata::default(),
            gate: default_gate(),
            master_gain: default_master_gain(),
            midi_tracks: vec![MidiTrack::default(); DEFAULT_MIDI_TRACKS],
            automation_tracks: Vec::new(),
            graph: default_graph_doc(DEFAULT_MIDI_TRACKS),
        }
    }
}

impl Project {
    /// 项目级校验：轨道数上限 + 图结构（文件层最后防线）。
    pub fn validate(&self) -> Result<(), String> {
        if self.midi_tracks.len() > MAX_MIDI_TRACKS {
            return Err(format!(
                "MIDI 轨道数 {} 超过上限 {MAX_MIDI_TRACKS}",
                self.midi_tracks.len()
            ));
        }
        if self.automation_tracks.len() > MAX_AUTO_TRACKS {
            return Err(format!(
                "自动化轨道数 {} 超过上限 {MAX_AUTO_TRACKS}",
                self.automation_tracks.len()
            ));
        }
        self.graph
            .validate(self.midi_tracks.len(), self.automation_tracks.len())
    }
}

/// 项目级元数据。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProjectMetadata {
    /// 项目名。
    #[serde(default)]
    pub name: String,
    /// 速度（BPM）。UI 与时间计算统一使用 f64。
    #[serde(default = "default_tempo")]
    pub tempo_bpm: f64,
}

fn default_tempo() -> f64 {
    120.0
}

impl Default for ProjectMetadata {
    fn default() -> Self {
        Self {
            name: String::new(),
            tempo_bpm: default_tempo(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutoParam, AutoPoint, AutomationTrack, CableDoc, DEFAULT_MIDI_TRACKS, DelayParams,
        GraphDoc, MASTER_NODE_ID, MAX_NODES, MidiTrack, NodeDoc, PATTERN_STEPS, Pattern, PortRef,
        Project, Step, SynthParams, TICKS_PER_BAR, TICKS_PER_STEP, TrackSource, VoiceSpec,
        default_graph_doc,
    };

    #[test]
    fn pattern_ron_roundtrip() {
        let mut pattern = Pattern::default();
        pattern.steps[0] = Step {
            enabled: true,
            note: 69,
            velocity: 0.9,
        };
        pattern.steps[7].enabled = true;
        let text = ron::to_string(&pattern).expect("序列化失败");
        let back: Pattern = ron::from_str(&text).expect("反序列化失败");
        assert_eq!(pattern, back);
    }

    #[test]
    fn step_partial_fields_use_defaults() {
        // 只写 note：enabled/velocity 缺失时取 serde default（旧文件兼容）。
        let back: Step = ron::from_str("(note: 64)").expect("解析失败");
        assert!(!back.enabled);
        assert_eq!(back.note, 64);
        assert!((back.velocity - 0.9).abs() < 1e-6);
    }

    #[test]
    fn default_project_graph_is_fully_explicit() {
        // D13：默认文档完整描述默认拓扑。
        let project = Project::default();
        assert_eq!(project.midi_tracks.len(), DEFAULT_MIDI_TRACKS);
        let graph = &project.graph;
        assert_eq!(graph.nodes.len(), DEFAULT_MIDI_TRACKS * 3);
        // 每轨 4 条线：voice→delay→strip→master + midi 自连。
        assert_eq!(graph.cables.len(), DEFAULT_MIDI_TRACKS * 4);
        for t in 0..DEFAULT_MIDI_TRACKS {
            let voice = graph.instrument_of_track(t).expect("声源应存在");
            let delay = graph.builtin_delay_of_track(t).expect("延迟应存在");
            let strip = graph.channel_strip_of_track(t).expect("通道条应存在");
            assert_eq!(voice.node_id(), (MASTER_NODE_ID + 1) + (t as u64) * 3);
            assert_eq!(strip.node_id(), (MASTER_NODE_ID + 3) + (t as u64) * 3);
            let _ = delay;
        }
        assert!(project.validate().is_ok());
    }

    #[test]
    fn project_ron_roundtrip() {
        let mut project = Project::default();
        project.metadata.name = "demo".to_string();
        project.midi_tracks[0].name = "贝斯".to_string();
        project.midi_tracks[0].pattern = Pattern::single_note(48);
        project.midi_tracks[1].source = TrackSource::Clip;
        project.automation_tracks.push(AutomationTrack {
            enabled: true,
            loop_ticks: 192,
            points: vec![AutoPoint {
                tick: 0,
                value: 1.0,
            }],
        });
        // 自动化轨道 0 → 轨道 0 通道条增益。
        let strip = project.graph.channel_strip_of_track(0).unwrap().node_id();
        project
            .graph
            .cables
            .push(CableDoc::automation(0, strip, AutoParam::TrackGain));
        let text = ron::to_string(&project).expect("序列化失败");
        let back: Project = ron::from_str(&text).expect("反序列化失败");
        assert_eq!(project, back);
        assert!(back.validate().is_ok());
    }

    #[test]
    fn partial_project_falls_back_to_defaults() {
        // 手工编辑/未来文件：只写 metadata，其余取字段级默认（空轨道 +
        // 空图——文档说什么就是什么，D13）。
        let back: Project = ron::from_str("(metadata: (tempo_bpm: 140))").expect("解析失败");
        assert!((back.metadata.tempo_bpm - 140.0).abs() < 1e-9);
        assert!(back.midi_tracks.is_empty());
        assert!(back.graph.nodes.is_empty());
        assert!(back.validate().is_ok(), "空轨道 + 空图是合法文档");
    }

    #[test]
    fn graph_doc_validate_rejects_bad_structure() {
        // 保留 id（1 = 主输出）。
        let graph = GraphDoc {
            nodes: vec![NodeDoc::ChannelStrip {
                node_id: MASTER_NODE_ID,
                track: 0,
                gain: 0.8,
                pan: 0.0,
                muted: false,
            }],
            cables: Vec::new(),
        };
        assert!(graph.validate(1, 0).is_err());

        // 重复 node_id。
        let graph = GraphDoc {
            nodes: vec![
                NodeDoc::ChannelStrip {
                    node_id: 2,
                    track: 0,
                    gain: 0.8,
                    pan: 0.0,
                    muted: false,
                },
                NodeDoc::ChannelStrip {
                    node_id: 2,
                    track: 1,
                    gain: 0.8,
                    pan: 0.0,
                    muted: false,
                },
            ],
            cables: Vec::new(),
        };
        assert!(graph.validate(2, 0).is_err());

        // 连线引用不存在的节点。
        let graph = GraphDoc {
            nodes: Vec::new(),
            cables: vec![CableDoc::audio(9, PortRef::Master)],
        };
        assert!(graph.validate(0, 0).is_err());

        // MIDI 线目标不是乐器。
        let graph = GraphDoc {
            nodes: vec![NodeDoc::ChannelStrip {
                node_id: 2,
                track: 0,
                gain: 0.8,
                pan: 0.0,
                muted: false,
            }],
            cables: vec![CableDoc::midi(0, 2)],
        };
        assert!(graph.validate(1, 0).is_err());

        // 自动化线源端口越界。
        let graph = GraphDoc {
            nodes: vec![NodeDoc::ChannelStrip {
                node_id: 2,
                track: 0,
                gain: 0.8,
                pan: 0.0,
                muted: false,
            }],
            cables: vec![CableDoc::automation(3, 2, AutoParam::TrackGain)],
        };
        assert!(graph.validate(1, 1).is_err());

        // 节点容量超限。
        let nodes: Vec<NodeDoc> = (0..=MAX_NODES)
            .map(|i| NodeDoc::ChannelStrip {
                node_id: 2 + i as u64,
                track: 0,
                gain: 0.8,
                pan: 0.0,
                muted: false,
            })
            .collect();
        let graph = GraphDoc {
            nodes,
            cables: Vec::new(),
        };
        assert!(graph.validate(1, 0).is_err());
    }

    #[test]
    fn next_node_id_and_track_lookups() {
        let graph = default_graph_doc(2);
        assert_eq!(graph.next_node_id(), (MASTER_NODE_ID + 1) + 2 * 3);
        assert_eq!(
            graph.instrument_of_track(1).map(NodeDoc::node_id),
            Some(MASTER_NODE_ID + 4)
        );
        assert!(graph.instrument_of_track(2).is_none());
        assert_eq!(graph.auto_endpoint(0), None);
    }

    #[test]
    fn voice_spec_ron_roundtrip() {
        for voice in [
            VoiceSpec::Builtin,
            VoiceSpec::Clap {
                path: "a.clap".to_string(),
                plugin_id: "x".to_string(),
            },
            VoiceSpec::Vst3 {
                path: "b.vst3".to_string(),
                class_id: "cid".to_string(),
            },
        ] {
            let text = ron::to_string(&voice).expect("序列化失败");
            let back: VoiceSpec = ron::from_str(&text).expect("反序列化失败");
            assert_eq!(voice, back);
        }
    }

    #[test]
    fn automation_value_at_segments() {
        let mut auto = AutomationTrack {
            enabled: true,
            ..AutomationTrack::default()
        };
        auto.points = vec![
            AutoPoint {
                tick: 0,
                value: 1.0,
            },
            AutoPoint {
                tick: 96,
                value: 0.5,
            },
            AutoPoint {
                tick: 192,
                value: 0.0,
            },
        ];
        assert!((auto.value_at(0.0).unwrap() - 1.0).abs() < 1e-6);
        assert!(
            (auto.value_at(48.0).unwrap() - 0.75).abs() < 1e-6,
            "线性中点"
        );
        assert!((auto.value_at(192.0).unwrap()).abs() < 1e-6);
        assert!(
            (auto.value_at(1_000.0).unwrap()).abs() < 1e-6,
            "尾外保持端值"
        );
        assert!(
            (auto.value_at(-5.0).unwrap() - 1.0).abs() < 1e-6,
            "首外保持端值"
        );

        auto.enabled = false;
        assert!(auto.value_at(48.0).is_none(), "未启用不产生自动化");
        auto.points.clear();
        auto.enabled = true;
        assert!(auto.value_at(48.0).is_none(), "空曲线不产生自动化");
    }

    #[test]
    fn automation_upsert_keeps_order() {
        let mut auto = AutomationTrack::default();
        auto.upsert_point(AutoPoint {
            tick: 96,
            value: 0.5,
        });
        auto.upsert_point(AutoPoint {
            tick: 0,
            value: 1.0,
        });
        auto.upsert_point(AutoPoint {
            tick: 48,
            value: 0.75,
        });
        auto.upsert_point(AutoPoint {
            tick: 48,
            value: 0.8,
        }); // 同 tick 覆盖
        let ticks: Vec<u32> = auto.points.iter().map(|p| p.tick).collect();
        assert_eq!(ticks, vec![0, 48, 96], "锚点应保持升序且同 tick 覆盖");
        assert!((auto.points[1].value - 0.8).abs() < 1e-6);
    }

    #[test]
    fn automation_remove_near() {
        let mut auto = AutomationTrack::default();
        auto.upsert_point(AutoPoint {
            tick: 48,
            value: 0.5,
        });
        assert!(auto.remove_near(50, 6), "容差内应命中");
        assert!(auto.points.is_empty());
        assert!(!auto.remove_near(0, 6), "空曲线删除返回 false");
    }

    #[test]
    fn clip_ron_roundtrip() {
        use super::{Clip, Note};
        let mut clip = Clip::default();
        clip.notes.push(Note {
            start: 0,
            length: 48,
            note: 60,
            velocity: 0.9,
        });
        clip.notes.push(Note {
            start: 24,
            length: 24,
            note: 64,
            velocity: 1.0,
        });
        let text = ron::to_string(&clip).expect("序列化失败");
        let back: Clip = ron::from_str(&text).expect("反序列化失败");
        assert_eq!(clip, back);
    }

    #[test]
    fn note_partial_fields_use_defaults() {
        use super::Note;
        let back: Note = ron::from_str("(start: 48, note: 72)").expect("解析失败");
        assert_eq!(back.length, TICKS_PER_STEP, "音符时长默认一个 16 分音符");
        assert!((back.velocity - 0.9).abs() < 1e-6);
    }

    #[test]
    fn midi_track_defaults_and_delay_params() {
        let track = MidiTrack::default();
        assert_eq!(track.source, TrackSource::Steps);
        assert_eq!(track.pattern.steps.len(), PATTERN_STEPS);
        assert_eq!(track.clip.loop_ticks, TICKS_PER_BAR);
        let delay = DelayParams::default();
        assert!(delay.mix.abs() < 1e-6, "延迟默认旁通");
        let _synth: SynthParams = ron::from_str("()").unwrap_or_default();
    }
}
