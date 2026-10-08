//! 节点图（P1–P3）：可布线处理图核心、默认轨道拓扑、时间轴连线表
//! 与结构更新协议。
//!
//! 取代旧 `Mixer` 固定拓扑（spec 见 `docs/spec/node-graph.md`）：
//!
//! - **每节点一个预分配立体声输出缓冲**；多条入边在节点输入处自动求和
//!   （D4）；`Processor` 的原地处理语义不变，所有内置与 CLAP/VST3 节点
//!   免改接入。
//! - **结构更新协议（D5）**：控制侧 [`GraphCompiler`] 维护拓扑、编译
//!   [`ExecPlan`]（拓扑排序 + 入边表，Kahn 算法，有环拒绝 —— D10）；音频
//!   线程经 [`GraphUpdate`] 队列整体换入计划并做指针级增删，被移除的
//!   节点（连同输出缓冲）经回收回执队列送回控制线程释放 —— 实时线程
//!   **永不 drop 堆对象**。
//! - **时间轴连线表（P3，D7）**：时间轴节点固定存在但不占音频节点——
//!   MIDI 轨道经 [`ExecPlan::midi_targets`] 扇出到若干乐器节点（可多路
//!   扇出）；自动化轨道经 [`ExecPlan::auto_routes`] 以 `(节点, 参数)`
//!   为终点投递，曲线本身不含目标。
//! - 默认拓扑复刻旧 Mixer：`声源 → 延迟 → 通道条 → 主输出`，通道条的
//!   增益/声像/静音升级为真节点 [`ChannelStripProcessor`]；每条 MIDI 轨
//!   默认连到本轨声源。

use rtrb::Producer;

use crate::realtime::DEFAULT_MIDI_TRACKS;
use crate::realtime::MAX_MIDI_TRACKS;
use crate::realtime::automation::MAX_AUTO_TRACKS;
use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::delay::DelayProcessor;
use crate::realtime::gain::GainProcessor;
use crate::realtime::processor::Processor;
use crate::realtime::synth::SynthProcessor;
use crate::realtime::voice::RecycledNode;

/// 图容量上限（D1）：节点数。
pub const MAX_NODES: usize = 64;
/// 图容量上限（D1）：连线数（P5b：编译器侧 cable 列表定容）。
pub const MAX_CABLES: usize = 128;
/// 图容量上限（D1）：单节点最大入边数（超限拒绝连线）。
pub const MAX_IN_EDGES: usize = 8;
/// 无边哨兵值（`out_edge` / 连线表的空槽标记）。
pub const NO_EDGE: u16 = u16::MAX;
/// 每轨外部效果器槽数（沿旧 `TrackStrip` 语义）。
pub const EFFECT_SLOTS: usize = 4;
/// 单次结构更新最多安装的节点数（≥ 每轨三件套 3）。
pub const MAX_DEPLOYMENTS: usize = 4;
/// 单次结构更新最多移除的节点数（≥ 轨道三件套 + 效果器 7）。
pub const MAX_REMOVALS: usize = 8;
/// 单条 MIDI 轨最多同时扇出的乐器节点数（D7）。
pub const MAX_MIDI_FANOUT: usize = 4;

/// 单条音频连线（编译器侧）：源槽位 → 目标槽位。
///
/// P5b：取代旧的单出边 `out_edge: [u16; MAX_NODES]`——一个输出口可以
/// 拉多根线（§3"出边不设限"），运行侧求和只看入边表，多出扇出天然工作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cable {
    /// 源节点槽位。
    pub from: u16,
    /// 目标节点槽位。
    pub to: u16,
}

/// 一条自动化连线的终点（D7）：目标节点槽位 + 目标参数。
///
/// 曲线不含目标；`slot == NO_EDGE` 表示该自动化轨道未连线（静默）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoRoute {
    /// 目标节点槽位。
    pub slot: u16,
    /// 目标参数。
    pub param: ParamId,
}

impl Default for AutoRoute {
    fn default() -> Self {
        Self {
            slot: NO_EDGE,
            param: ParamId::MasterGain,
        }
    }
}

/// 预编译执行计划：全 `Copy`，音频线程每块只按此遍历（D5）。
///
/// 由控制侧 [`GraphCompiler::compile`] 生成，经 [`GraphUpdate`] 整体
/// 换入 [`RtGraph`]；音频线程对它只读。
#[derive(Debug, Clone, Copy)]
pub struct ExecPlan {
    /// 拓扑序的槽位下标（仅前 `order_len` 个有效）。
    pub order: [u16; MAX_NODES],
    /// 有效拓扑序长度。
    pub order_len: usize,
    /// 各槽位的音频入边（源槽位下标，仅前 `in_counts[slot]` 个有效）。
    pub in_edges: [[u16; MAX_IN_EDGES]; MAX_NODES],
    /// 各槽位入边数。
    pub in_counts: [u8; MAX_NODES],
    /// 各槽位的节点 ID（P5b：公开寻址键，槽位是内部实现细节）。
    pub node_ids: [u64; MAX_NODES],
    /// 主输出槽位（cpal 交错写出的数据源）。
    pub master_slot: u16,
    /// 每轨声源槽位（手动参数/电平的轨道寻址；音符投递见 `midi_targets`）。
    pub track_voice: [u16; MAX_MIDI_TRACKS],
    /// 每轨延迟效果槽位。
    pub track_delay: [u16; MAX_MIDI_TRACKS],
    /// 每轨通道条槽位（电平回传的计量点）。
    pub track_channel: [u16; MAX_MIDI_TRACKS],
    /// 时间轴 MIDI 轨连线表：每条 MIDI 轨扇出的目标槽位（仅前
    /// `midi_target_counts[track]` 个有效）。
    pub midi_targets: [[u16; MAX_MIDI_FANOUT]; MAX_MIDI_TRACKS],
    /// 各 MIDI 轨的有效扇出数。
    pub midi_target_counts: [u8; MAX_MIDI_TRACKS],
    /// 时间轴自动化连线表：按自动化轨道索引的投递终点（D7）。
    pub auto_routes: [AutoRoute; MAX_AUTO_TRACKS],
}

impl ExecPlan {
    /// 空计划（无节点，输出静音，时间轴未连线）。
    pub const EMPTY: Self = Self {
        order: [0; MAX_NODES],
        order_len: 0,
        in_edges: [[NO_EDGE; MAX_IN_EDGES]; MAX_NODES],
        in_counts: [0; MAX_NODES],
        node_ids: [0; MAX_NODES],
        master_slot: 0,
        track_voice: [NO_EDGE; MAX_MIDI_TRACKS],
        track_delay: [NO_EDGE; MAX_MIDI_TRACKS],
        track_channel: [NO_EDGE; MAX_MIDI_TRACKS],
        midi_targets: [[NO_EDGE; MAX_MIDI_FANOUT]; MAX_MIDI_TRACKS],
        midi_target_counts: [0; MAX_MIDI_TRACKS],
        auto_routes: [AutoRoute {
            slot: NO_EDGE,
            param: ParamId::MasterGain,
        }; MAX_AUTO_TRACKS],
    };
}

/// 一个块处理后的电平快照（形状沿旧 `MixerLevels`，轨道侧定容扩至
/// [`MAX_MIDI_TRACKS`]，未配置轨恒为 0）。
#[derive(Debug, Clone, Copy, Default)]
pub struct GraphLevels {
    /// 各轨通道条输出峰值（post-fader，静音为 0）。
    pub tracks: [f32; MAX_MIDI_TRACKS],
    /// 主输出峰值。
    pub master: f32,
}

/// 运行时节点槽位：处理器 + 专属立体声输出缓冲。
struct NodeSlot {
    node_id: u64,
    processor: Box<dyn Processor>,
    output: AudioBuffer,
}

/// 运行时图：定容槽位 + 当前执行计划。结构变更只做指针移动与
/// `Copy` 覆写（[`RtGraph::apply_update`]），处理路径零分配。
pub struct RtGraph {
    slots: [Option<NodeSlot>; MAX_NODES],
    plan: ExecPlan,
    block_size: usize,
}

/// 结构更新载荷：一次拓扑变更 = 新执行计划 + 增量安装 + 增量移除。
///
/// 经结构队列（rtrb，非 `Copy`）下发；队列里的枚举本身住在环形缓冲
/// 预分配存储中，音频线程 `take` 出全部堆资源后，残留值 drop 无堆操作。
pub enum GraphUpdate {
    /// 整体换入执行计划，并应用增量安装/移除（先移除后安装）。
    SetTopology {
        /// 新执行计划（`Copy` 覆写）。
        plan: ExecPlan,
        /// 待安装节点（控制侧已分配好输出缓冲）。
        deployments: [NodeDeployment; MAX_DEPLOYMENTS],
        /// 有效安装数。
        deployment_count: usize,
        /// 待移除槽位（节点 + 缓冲送回收队列）。
        removed: [u16; MAX_REMOVALS],
        /// 有效移除数。
        removed_count: usize,
    },
}

/// 一次节点安装：控制侧构造好的处理器 + 专属输出缓冲。
///
/// 缓冲必须由控制侧预分配（实时线程禁止分配），随节点一起 move 进图；
/// 节点被移除时缓冲随 [`RecycledNode`] 返回控制线程释放。
pub struct NodeDeployment {
    /// 目标槽位。
    pub slot: u16,
    /// 控制侧单调分配的节点 ID（回收配对用）。
    pub node_id: u64,
    /// `Some` = 待安装（处理器 + 输出缓冲）；消费后置 `None`。
    pub node: Option<(Box<dyn Processor>, AudioBuffer)>,
}

impl RtGraph {
    /// 空图（所有槽位空，输出静音）。**非 RT-safe**（无堆分配，仅构造期使用）。
    pub fn new(block_size: usize) -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            plan: ExecPlan::EMPTY,
            block_size: block_size.max(1),
        }
    }

    /// 直接安装一个节点（仅流启动前的默认图构建使用）。**非 RT-safe**。
    fn install(
        &mut self,
        slot: usize,
        node_id: u64,
        processor: Box<dyn Processor>,
        output: AudioBuffer,
    ) {
        self.slots[slot] = Some(NodeSlot {
            node_id,
            processor,
            output,
        });
    }

    /// 应用一次结构更新：先移除（入回收队列）后安装，再整体换入新计划。
    /// RT-safe：只做指针移动、`Copy` 覆写与队列 push，无分配/drop 堆对象。
    pub fn apply_update(&mut self, update: GraphUpdate, recycle: &mut Producer<RecycledNode>) {
        let GraphUpdate::SetTopology {
            plan,
            mut deployments,
            deployment_count,
            removed,
            removed_count,
        } = update;
        for &slot in &removed[..removed_count] {
            if let Some(node) = self.slots[slot as usize].take() {
                // 回收队列满时放弃卸装（保持现状），避免实时线程 drop。
                let _ = recycle.push(RecycledNode {
                    node_id: node.node_id,
                    node: node.processor,
                    output: node.output,
                });
            }
        }
        for dep in deployments.iter_mut().take(deployment_count) {
            let Some((processor, output)) = dep.node.take() else {
                continue;
            };
            let s = dep.slot as usize;
            if s >= MAX_NODES {
                // 控制侧协议保证不发生；防御路径同样走回收而非就地 drop。
                let _ = recycle.push(RecycledNode {
                    node_id: dep.node_id,
                    node: processor,
                    output,
                });
                continue;
            }
            self.slots[s] = Some(NodeSlot {
                node_id: dep.node_id,
                processor,
                output,
            });
        }
        self.plan = plan;
    }

    /// 处理一个块：按拓扑序 求和入边 → 消费事件 → 原地处理，返回电平。
    /// RT-safe。
    pub fn process_block(&mut self, ctx: &ProcessContext) -> GraphLevels {
        let frames = self.block_size.min(ctx.frames);
        for i in 0..self.plan.order_len {
            let s = self.plan.order[i] as usize;
            // 临时取出本节点：源缓冲只读与本缓冲可写借用人畜无害。
            let Some(mut node) = self.slots[s].take() else {
                continue;
            };
            node.output.set_len(frames);
            node.output.clear();
            let count = self.plan.in_counts[s] as usize;
            for &src in &self.plan.in_edges[s][..count] {
                if let Some(src_node) = self.slots[src as usize].as_ref() {
                    sum_into(&mut node.output, &src_node.output);
                }
            }
            node.processor.process(ctx, &mut node.output);
            self.slots[s] = Some(node);
        }
        let mut levels = GraphLevels::default();
        for (t, &slot) in self.plan.track_channel.iter().enumerate() {
            levels.tracks[t] = self.slot_peak(slot);
        }
        levels.master = self.slot_peak(self.plan.master_slot);
        levels
    }

    /// 槽位输出的峰值（空槽/越界为 0）。RT-safe。
    fn slot_peak(&self, slot: u16) -> f32 {
        let Some(node) = self.slots.get(slot as usize).and_then(|s| s.as_ref()) else {
            return 0.0;
        };
        let mut peak = 0.0f32;
        for ch in 0..node.output.channels() {
            if let Some(channel) = node.output.channel(ch) {
                for &sample in channel {
                    let abs = sample.abs();
                    if abs > peak {
                        peak = abs;
                    }
                }
            }
        }
        peak
    }

    /// 向某条 MIDI 轨投递音符事件：按连线表扇出到全部目标乐器节点
    /// （时间轴音序与键盘演奏共用此路径；未连线 = 静默）。RT-safe。
    pub fn track_note(&mut self, track: usize, event: NoteEvent) -> bool {
        if track >= MAX_MIDI_TRACKS {
            return false;
        }
        let count = self.plan.midi_target_counts[track] as usize;
        // 连线表行是 `Copy`：先拷出避免借用与投递互斥。
        let targets = self.plan.midi_targets[track];
        let mut delivered = false;
        for &slot in &targets[..count] {
            delivered |= self.slot_note(slot, event);
        }
        delivered
    }

    /// 释放所有连线目标当前音符（走带 play/stop 前调用）。RT-safe。
    pub fn note_off_all(&mut self) {
        for track in 0..MAX_MIDI_TRACKS {
            let count = self.plan.midi_target_counts[track] as usize;
            let targets = self.plan.midi_targets[track];
            for &slot in &targets[..count] {
                let _ = self.slot_note(slot, NoteEvent::Off { note: 0 });
            }
        }
    }

    fn slot_note(&mut self, slot: u16, event: NoteEvent) -> bool {
        self.slots
            .get_mut(slot as usize)
            .and_then(|s| s.as_mut())
            .is_some_and(|n| n.processor.note_event(event))
    }

    /// 自动化轨道投递：查连线表把曲线值写给 `(目标节点, 目标参数)`
    /// （终点在编译期已定，见 [`AutoRoute`]）。未连线/越界静默。RT-safe。
    pub fn auto_set_param(&mut self, auto_index: usize, value: f32) {
        let Some(&route) = self.plan.auto_routes.get(auto_index) else {
            return;
        };
        if route.slot == NO_EDGE {
            return;
        }
        let _ = self.slot_set_param(route.slot, route.param, value);
    }

    /// 按 node_id 定向参数投递（P5b：节点寻址的公开路径；画布与
    /// 自动化连线均以此为准）。RT-safe。
    pub fn node_set_param(&mut self, node_id: u64, id: ParamId, value: f32) -> bool {
        for slot in self.slots.iter_mut().flatten() {
            if slot.node_id == node_id {
                return slot.processor.set_param(id, value);
            }
        }
        false
    }

    /// 轨道参数路由（越界忽略）：混音三项 → 通道条；延迟三项 → 延迟；
    /// 音色/包络/滤波/插件参数 → 声源。**过渡路径**——P5b 后仅剩混音
    /// 器面板的手动参数仍在用（P5d 画布落地时退役）；时间轴投递一律
    /// 走 node_id 连线表。RT-safe。
    pub fn track_set_param(&mut self, track: usize, id: ParamId, value: f32) {
        if track >= MAX_MIDI_TRACKS {
            return;
        }
        let slot = route_slot(
            self.plan.track_voice[track],
            self.plan.track_delay[track],
            self.plan.track_channel[track],
            id,
        );
        if slot != NO_EDGE {
            let _ = self.slot_set_param(slot, id, value);
        }
    }

    /// 主输出槽位参数（主增益）。RT-safe。
    pub fn master_set_param(&mut self, id: ParamId, value: f32) -> bool {
        self.slot_set_param(self.plan.master_slot, id, value)
    }

    fn slot_set_param(&mut self, slot: u16, id: ParamId, value: f32) -> bool {
        self.slots
            .get_mut(slot as usize)
            .and_then(|s| s.as_mut())
            .is_some_and(|n| n.processor.set_param(id, value))
    }

    /// 主输出缓冲只读访问（供 `BlockRunner` 交错写出）。RT-safe。
    pub fn master_channel(&self, ch: usize) -> Option<&[f32]> {
        self.slots
            .get(self.plan.master_slot as usize)
            .and_then(|s| s.as_ref())
            .and_then(|n| n.output.channel(ch))
    }
}

/// 把 `src` 活动区域逐样本累加进 `dst`（多入边求和，D4）。RT-safe。
fn sum_into(dst: &mut AudioBuffer, src: &AudioBuffer) {
    for ch in 0..dst.channels().min(src.channels()) {
        if let (Some(d), Some(s)) = (dst.channel_mut(ch), src.channel(ch)) {
            for (d, s) in d.iter_mut().zip(s.iter()) {
                *d += s;
            }
        }
    }
}

/// 按参数种类解析轨道内的目标槽位：混音三项 → 通道条；延迟三项 →
/// 延迟；主增益无轨道归宿；其余（音色/包络/滤波/插件）→ 声源。
/// 图运行侧与编译器共用的路由语义。
fn route_slot(voice: u16, delay: u16, channel: u16, param: ParamId) -> u16 {
    match param {
        ParamId::TrackGain | ParamId::TrackPan | ParamId::TrackMute => channel,
        ParamId::DelayTime | ParamId::DelayFeedback | ParamId::DelayMix => delay,
        ParamId::MasterGain => NO_EDGE,
        _ => voice,
    }
}

/// 通道条节点：增益 / 声像（线性律）/ 静音。图化后混音三项成为真节点。
///
/// 语义与旧 `TrackStrip` 求和段一致：静音输出全零；声像 p=0 → (1,1)，
/// p=±1 → 单边全量。
pub struct ChannelStripProcessor {
    gain: f32,
    pan: f32,
    muted: bool,
}

impl ChannelStripProcessor {
    /// 默认增益 0.8、声像居中、不静音（与旧 `TrackStrip::new` 一致）。
    pub fn new() -> Self {
        Self {
            gain: 0.8,
            pan: 0.0,
            muted: false,
        }
    }
}

impl Default for ChannelStripProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl Processor for ChannelStripProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        if self.muted {
            for ch in 0..buffer.channels() {
                if let Some(channel) = buffer.channel_mut(ch) {
                    channel.fill(0.0);
                }
            }
            return;
        }
        let left = (1.0 - self.pan).clamp(0.0, 1.0);
        let right = (1.0 + self.pan).clamp(0.0, 1.0);
        let gain = self.gain;
        for (ch, pan_gain) in [(0usize, left), (1usize, right)] {
            let g = gain * pan_gain;
            if g >= 1.0 {
                continue; // 恒等快速路径
            }
            if g <= 0.0 {
                if let Some(channel) = buffer.channel_mut(ch) {
                    channel.fill(0.0);
                }
                continue;
            }
            if let Some(channel) = buffer.channel_mut(ch) {
                for sample in channel.iter_mut() {
                    *sample *= g;
                }
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::TrackGain => {
                if value.is_finite() {
                    self.gain = value.clamp(0.0, 1.5);
                }
                true
            }
            ParamId::TrackPan => {
                if value.is_finite() {
                    self.pan = value.clamp(-1.0, 1.0);
                }
                true
            }
            ParamId::TrackMute => {
                self.muted = value >= 0.5;
                true
            }
            _ => false,
        }
    }
}

/// 拓扑编译/校验错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopologyError {
    /// 目标轨道越界（≥ [`MAX_MIDI_TRACKS`]）或未配置。
    InvalidTrack,
    /// 目标节点无效（node_id 未分配 / 槽位越界或空槽）。
    InvalidSlot,
    /// 目标索引越界（自动化轨道索引）。
    InvalidIndex,
    /// 无空闲槽位（容量 64 满）。
    NoFreeSlot,
    /// 无空闲 MIDI 轨（容量 [`MAX_MIDI_TRACKS`] 满）。
    NoFreeTrack,
    /// 连线数超容（> 128）。
    TooManyCables,
    /// 拓扑含环（D10：拒绝）。
    Cycle,
    /// 某节点入边超限（> 8）。
    FanInOverflow,
    /// MIDI 轨扇出超限（> [`MAX_MIDI_FANOUT`]）。
    MidiFanOutOverflow,
}

/// 控制侧图状态（**非实时**）：槽位占用 + node_id 表 + cable 列表 +
/// 轨道路由 + 时间轴连线表，负责编译 [`ExecPlan`] 与生成
/// [`GraphUpdate`]。全 `Copy` —— 引擎在队列 push 失败时可整快照回滚。
#[derive(Debug, Clone, Copy)]
pub struct GraphCompiler {
    sample_rate: f32,
    block_size: usize,
    occupied: [bool; MAX_NODES],
    /// 槽位 → node_id（P5b 公开寻址键；空槽为 0，正式 ID 从 1 起）。
    slot_ids: [u64; MAX_NODES],
    /// 音频连线列表（`from → to`，去重；P5b 取代单出边 `out_edge`）。
    cables: [Cable; MAX_CABLES],
    cable_count: usize,
    master_slot: u16,
    track_voice: [u16; MAX_MIDI_TRACKS],
    track_delay: [u16; MAX_MIDI_TRACKS],
    track_channel: [u16; MAX_MIDI_TRACKS],
    track_effects: [[u16; EFFECT_SLOTS]; MAX_MIDI_TRACKS],
    /// 时间轴 MIDI 轨连线表（扇出目标槽位）。
    midi_targets: [[u16; MAX_MIDI_FANOUT]; MAX_MIDI_TRACKS],
    midi_target_counts: [u8; MAX_MIDI_TRACKS],
    /// 时间轴自动化连线表（终点 = (槽位, 参数)）。
    auto_routes: [AutoRoute; MAX_AUTO_TRACKS],
    next_node_id: u64,
}

impl GraphCompiler {
    /// 构建默认拓扑（复刻旧 Mixer）：主输出 + 每轨 `声源 → 延迟 → 通道条`。
    /// **非 RT-safe**（分配处理器与缓冲），只在音频流启动前调用。
    ///
    /// P5c 起（D13 全显式）退役为"新建项目生成初始文档"的参考实现：
    /// 项目加载改由文档逐步建图（`add_midi_track` / `insert_effect` /
    /// `connect_*` 粒度）。
    pub fn default_graph(sample_rate: f32, block_size: usize) -> (Self, RtGraph) {
        let mut compiler = Self {
            sample_rate,
            block_size: block_size.max(1),
            occupied: [false; MAX_NODES],
            slot_ids: [0; MAX_NODES],
            cables: [Cable { from: 0, to: 0 }; MAX_CABLES],
            cable_count: 0,
            master_slot: 0,
            track_voice: [NO_EDGE; MAX_MIDI_TRACKS],
            track_delay: [NO_EDGE; MAX_MIDI_TRACKS],
            track_channel: [NO_EDGE; MAX_MIDI_TRACKS],
            track_effects: [[NO_EDGE; EFFECT_SLOTS]; MAX_MIDI_TRACKS],
            midi_targets: [[NO_EDGE; MAX_MIDI_FANOUT]; MAX_MIDI_TRACKS],
            midi_target_counts: [0; MAX_MIDI_TRACKS],
            auto_routes: [AutoRoute::default(); MAX_AUTO_TRACKS],
            next_node_id: 1,
        };
        let mut graph = RtGraph::new(block_size);
        let bs = compiler.block_size;

        let master_id = compiler.fresh_id();
        let master = compiler.alloc_slot();
        compiler.master_slot = master;
        compiler.slot_ids[master as usize] = master_id;
        graph.install(
            master as usize,
            master_id,
            Box::new(GainProcessor::new(1.0)),
            AudioBuffer::new(2, bs),
        );

        for t in 0..DEFAULT_MIDI_TRACKS {
            let slots = compiler.allocate_track_chain(t);
            for (kind, &(slot, id)) in slots.iter().enumerate() {
                graph.install(
                    slot as usize,
                    id,
                    chain_processor(kind, sample_rate),
                    AudioBuffer::new(2, bs),
                );
            }
        }

        // 默认拓扑由构造保证无环；失败即启动期 bug，直接 panic 可接受。
        graph.plan = compiler
            .compile()
            .expect("default graph topology must be acyclic");
        (compiler, graph)
    }

    /// 为一条（未配置的）MIDI 轨分配默认三件套：声源 → 延迟 → 通道条
    /// → 主输出的槽位、node_id、路由表项、cable 与 MIDI 自连。
    /// 返回 `[(slot, node_id); 3]`（顺序：声源 / 延迟 / 通道条）。
    /// 仅构造/加轨路径调用（调用方需先确保容量充足）。
    fn allocate_track_chain(&mut self, track: usize) -> [(u16, u64); 3] {
        let voice = self.alloc_slot();
        let delay = self.alloc_slot();
        let channel = self.alloc_slot();
        self.track_voice[track] = voice;
        self.track_delay[track] = delay;
        self.track_channel[track] = channel;
        // 连线：声源 → 延迟 → 通道条 → 主输出（构造期保证无环，panic 可接受）。
        self.add_cable(voice, delay).expect("fresh chain cables");
        self.add_cable(delay, channel).expect("fresh chain cables");
        self.add_cable(channel, self.master_slot)
            .expect("fresh chain cables");
        // 时间轴默认布线：MIDI 轨 t → 本轨声源（复刻旧 Mixer 语义）。
        self.midi_targets[track][0] = voice;
        self.midi_target_counts[track] = 1;

        let mut out = [(0u16, 0u64); 3];
        for (slot, entry) in [voice, delay, channel].into_iter().zip(out.iter_mut()) {
            let id = self.fresh_id();
            self.slot_ids[slot as usize] = id;
            *entry = (slot, id);
        }
        out
    }

    /// 撤销 [`Self::allocate_track_chain`]（编译失败时的回滚路径）。
    fn deallocate_track_chain(&mut self, track: usize) {
        let slots = [
            self.track_voice[track],
            self.track_delay[track],
            self.track_channel[track],
        ];
        self.remove_cables_touching(&slots);
        for slot in slots {
            if slot != NO_EDGE {
                self.occupied[slot as usize] = false;
                self.slot_ids[slot as usize] = 0;
            }
        }
        self.track_voice[track] = NO_EDGE;
        self.track_delay[track] = NO_EDGE;
        self.track_channel[track] = NO_EDGE;
        self.midi_targets[track] = [NO_EDGE; MAX_MIDI_FANOUT];
        self.midi_target_counts[track] = 0;
    }

    fn fresh_id(&mut self) -> u64 {
        let id = self.next_node_id;
        self.next_node_id += 1;
        id
    }

    /// node_id → 槽位解析（未分配 / 空槽返回 `None`）。node_id 0 恒无效。
    pub fn node_slot(&self, node_id: u64) -> Option<u16> {
        if node_id == 0 {
            return None;
        }
        for (slot, &id) in self.slot_ids.iter().enumerate() {
            if id == node_id && self.occupied[slot] {
                return Some(slot as u16);
            }
        }
        None
    }

    /// 某轨声源的 node_id（UI / 加载器把轨道坐标解析成节点寻址用）。
    pub fn track_voice_node_id(&self, track: usize) -> Option<u64> {
        self.track_table_id(self.track_voice, track)
    }

    /// 某轨延迟节点的 node_id。
    pub fn track_delay_node_id(&self, track: usize) -> Option<u64> {
        self.track_table_id(self.track_delay, track)
    }

    /// 某轨通道条的 node_id。
    pub fn track_channel_node_id(&self, track: usize) -> Option<u64> {
        self.track_table_id(self.track_channel, track)
    }

    fn track_table_id(&self, table: [u16; MAX_MIDI_TRACKS], track: usize) -> Option<u64> {
        let slot = *table.get(track)?;
        if slot == NO_EDGE {
            return None;
        }
        self.slot_ids
            .get(slot as usize)
            .copied()
            .filter(|&id| id != 0)
    }

    /// 主输出节点的 node_id（固定存在，D6）。
    pub fn master_node_id(&self) -> u64 {
        self.slot_ids[self.master_slot as usize]
    }

    /// 当前已配置的 MIDI 轨数（`track_voice` 非 `NO_EDGE` 的连续段）。
    pub fn midi_track_count(&self) -> usize {
        self.track_voice
            .iter()
            .position(|&s| s == NO_EDGE)
            .unwrap_or(MAX_MIDI_TRACKS)
    }

    // ---- cable 列表基元 ----

    fn has_cable(&self, from: u16, to: u16) -> bool {
        self.cables[..self.cable_count]
            .iter()
            .any(|c| c.from == from && c.to == to)
    }

    /// 追加连线（重复连线幂等成功）。容量满返回 [`TopologyError::TooManyCables`]。
    fn add_cable(&mut self, from: u16, to: u16) -> Result<(), TopologyError> {
        if self.has_cable(from, to) {
            return Ok(());
        }
        if self.cable_count >= MAX_CABLES {
            return Err(TopologyError::TooManyCables);
        }
        self.cables[self.cable_count] = Cable { from, to };
        self.cable_count += 1;
        Ok(())
    }

    /// 移除一条连线（未连线幂等返回 `false`）。
    fn remove_cable(&mut self, from: u16, to: u16) -> bool {
        let Some(pos) = self.cables[..self.cable_count]
            .iter()
            .position(|c| c.from == from && c.to == to)
        else {
            return false;
        };
        self.cables.copy_within(pos + 1..self.cable_count, pos);
        self.cable_count -= 1;
        true
    }

    /// 移除与任一槽位相关的全部连线（出边与入边，节点/轨道移除时调用）。
    fn remove_cables_touching(&mut self, slots: &[u16]) {
        let mut i = 0;
        while i < self.cable_count {
            if slots.contains(&self.cables[i].from) || slots.contains(&self.cables[i].to) {
                self.cables.copy_within(i + 1..self.cable_count, i);
                self.cable_count -= 1;
            } else {
                i += 1;
            }
        }
    }

    fn alloc_slot(&mut self) -> u16 {
        let slot = self
            .occupied
            .iter()
            .position(|&o| !o)
            .expect("graph slot capacity") as u16;
        self.occupied[slot as usize] = true;
        slot
    }

    /// 打包一次结构更新（控制侧分配输出缓冲；可批量安装）。
    /// 非实时路径：`deployments` 用 `Vec` 携带堆资源（≤ [`MAX_DEPLOYMENTS`]）。
    #[allow(clippy::type_complexity)]
    fn build_update(
        &self,
        plan: ExecPlan,
        deployments: Vec<(u16, u64, Box<dyn Processor>)>,
        removed: &[u16],
    ) -> GraphUpdate {
        let mut batch = std::array::from_fn(|_| NodeDeployment {
            slot: 0,
            node_id: 0,
            node: None,
        });
        let deployment_count = deployments.len().min(MAX_DEPLOYMENTS);
        for (batch_slot, (slot, node_id, node)) in deployments.into_iter().enumerate() {
            if batch_slot >= MAX_DEPLOYMENTS {
                // 防御路径：超出单次更新容量的安装直接释放（控制线程允许 drop）。
                drop(node);
                continue;
            }
            batch[batch_slot] = NodeDeployment {
                slot,
                node_id,
                node: Some((node, AudioBuffer::new(2, self.block_size))),
            };
        }
        let mut removed_arr = [0u16; MAX_REMOVALS];
        let removed_count = removed.len().min(MAX_REMOVALS);
        removed_arr[..removed_count].copy_from_slice(&removed[..removed_count]);
        GraphUpdate::SetTopology {
            plan,
            deployments: batch,
            deployment_count,
            removed: removed_arr,
            removed_count,
        }
    }

    /// 编译执行计划：入边表 + Kahn 拓扑排序（有环/入边超限拒绝）。
    pub fn compile(&self) -> Result<ExecPlan, TopologyError> {
        let mut plan = ExecPlan::EMPTY;
        let mut in_counts = [0u8; MAX_NODES];
        for cable in &self.cables[..self.cable_count] {
            // 防御：只统计两端都在占用中的连线（悬空 cable 不参与拓扑）。
            if !self.occupied[cable.from as usize] || !self.occupied[cable.to as usize] {
                continue;
            }
            let d = cable.to as usize;
            if in_counts[d] as usize >= MAX_IN_EDGES {
                return Err(TopologyError::FanInOverflow);
            }
            plan.in_edges[d][in_counts[d] as usize] = cable.from;
            in_counts[d] += 1;
        }

        let mut degree = in_counts;
        let mut work = [0u16; MAX_NODES];
        let mut work_len = 0usize;
        let mut head = 0usize;
        for (slot, &occupied) in self.occupied.iter().enumerate() {
            if occupied && degree[slot] == 0 {
                work[work_len] = slot as u16;
                work_len += 1;
            }
        }
        while head < work_len {
            let s = work[head] as usize;
            head += 1;
            plan.order[plan.order_len] = s as u16;
            plan.order_len += 1;
            // 遍历该槽位的全部出边（多出扇出：每个下游入度各减一）。
            for cable in &self.cables[..self.cable_count] {
                if cable.from as usize == s
                    && self.occupied[cable.from as usize]
                    && self.occupied[cable.to as usize]
                {
                    let d = cable.to as usize;
                    degree[d] -= 1;
                    if degree[d] == 0 {
                        work[work_len] = cable.to;
                        work_len += 1;
                    }
                }
            }
        }

        let occupied_count = self.occupied.iter().filter(|&&o| o).count();
        if plan.order_len != occupied_count {
            return Err(TopologyError::Cycle);
        }
        plan.in_counts = in_counts;
        plan.node_ids = self.slot_ids;
        plan.master_slot = self.master_slot;
        plan.track_voice = self.track_voice;
        plan.track_delay = self.track_delay;
        plan.track_channel = self.track_channel;
        plan.midi_targets = self.midi_targets;
        plan.midi_target_counts = self.midi_target_counts;
        plan.auto_routes = self.auto_routes;
        Ok(plan)
    }

    /// 将某轨声源替换为给定处理器（旧节点经回收队列释放）。
    /// 失败时原样交还 `node` 供调用方释放。
    pub fn set_voice(
        &mut self,
        track: usize,
        node: Box<dyn Processor>,
    ) -> Result<(GraphUpdate, u64), Box<dyn Processor>> {
        if track >= MAX_MIDI_TRACKS || self.track_voice[track] == NO_EDGE {
            return Err(node);
        }
        let slot = self.track_voice[track];
        let node_id = self.fresh_id();
        self.slot_ids[slot as usize] = node_id;
        let Ok(plan) = self.compile() else {
            return Err(node);
        };
        let update = self.build_update(plan, vec![(slot, node_id, node)], &[slot]);
        Ok((update, node_id))
    }

    /// 将某轨声源恢复为内置合成器（旧外部节点经回收队列释放）。
    ///
    /// 行为变化（相对旧 `TrackStrip` 驻留语义）：外部节点被移除而非
    /// 驻留，切回插件时需重新实例化（状态不保留）。UI 守卫经回收
    /// `node_id` 正常配对释放。
    pub fn activate_builtin_voice(&mut self, track: usize) -> Result<GraphUpdate, TopologyError> {
        if track >= MAX_MIDI_TRACKS || self.track_voice[track] == NO_EDGE {
            return Err(TopologyError::InvalidTrack);
        }
        let slot = self.track_voice[track];
        let node_id = self.fresh_id();
        let node = Box::new(SynthProcessor::new(self.sample_rate));
        self.slot_ids[slot as usize] = node_id;
        let plan = self.compile()?;
        Ok(self.build_update(plan, vec![(slot, node_id, node)], &[slot]))
    }

    /// 向某轨效果链末端（通道条之前）追加一个效果器节点。
    /// 无空闲槽位时原样交还 `node`。
    pub fn insert_effect(
        &mut self,
        track: usize,
        node: Box<dyn Processor>,
    ) -> Result<(GraphUpdate, u64), Box<dyn Processor>> {
        if track >= MAX_MIDI_TRACKS || self.track_channel[track] == NO_EDGE {
            return Err(node);
        }
        let Some(free_idx) = self.occupied.iter().position(|&o| !o) else {
            return Err(node);
        };
        let channel = self.track_channel[track];
        // 沿链找到直连通道条的最后一个节点，把新效果器插在它与通道条之间。
        let mut last = self.track_delay[track];
        let mut hops = 0usize;
        while !self.has_cable(last, channel) {
            match self.cables[..self.cable_count]
                .iter()
                .find(|c| c.from == last)
            {
                Some(cable) => last = cable.to,
                None => {
                    // 拓扑损坏（不应发生）；放弃插入。
                    return Err(node);
                }
            }
            hops += 1;
            if last == NO_EDGE || hops > MAX_NODES {
                return Err(node);
            }
        }

        let free = free_idx as u16;
        self.occupied[free as usize] = true;
        self.remove_cable(last, channel);
        // 失败即回滚（保持编译器与运行图一致）。
        if self
            .add_cable(last, free)
            .and_then(|()| self.add_cable(free, channel))
            .is_err()
        {
            self.occupied[free as usize] = false;
            let _ = self.remove_cable(last, free);
            let _ = self.remove_cable(free, channel);
            let _ = self.add_cable(last, channel);
            return Err(node);
        }
        if let Some(e) = self.track_effects[track]
            .iter_mut()
            .find(|e| **e == NO_EDGE)
        {
            *e = free;
        }
        let plan = match self.compile() {
            Ok(plan) => plan,
            Err(_) => {
                // 回滚：新槽位的入边与出边一并清理。
                self.remove_cables_touching(&[free]);
                self.occupied[free as usize] = false;
                self.slot_ids[free as usize] = 0;
                let _ = self.add_cable(last, channel);
                if let Some(e) = self.track_effects[track].iter_mut().find(|e| **e == free) {
                    *e = NO_EDGE;
                }
                return Err(node);
            }
        };
        let node_id = self.fresh_id();
        self.slot_ids[free as usize] = node_id;
        let update = self.build_update(plan, vec![(free, node_id, node)], &[]);
        Ok((update, node_id))
    }

    /// 清空某轨全部外部效果器（节点经回收队列释放）。
    pub fn clear_effects(&mut self, track: usize) -> Result<GraphUpdate, TopologyError> {
        if track >= MAX_MIDI_TRACKS {
            return Err(TopologyError::InvalidTrack);
        }
        let channel = self.track_channel[track];
        let delay = self.track_delay[track];
        let mut removed = [0u16; MAX_REMOVALS];
        let mut removed_count = 0usize;
        for effect in self.track_effects[track] {
            if effect != NO_EDGE && removed_count < MAX_REMOVALS {
                // 入边与出边一并清理（悬空 cable 会让拓扑编译误判）。
                self.remove_cables_touching(&[effect]);
                self.occupied[effect as usize] = false;
                self.slot_ids[effect as usize] = 0;
                removed[removed_count] = effect;
                removed_count += 1;
            }
        }
        self.track_effects[track] = [NO_EDGE; EFFECT_SLOTS];
        if delay != NO_EDGE && channel != NO_EDGE && !self.has_cable(delay, channel) {
            self.add_cable(delay, channel)?;
        }
        let plan = self.compile()?;
        Ok(self.build_update(plan, Vec::new(), &removed[..removed_count]))
    }

    /// 新增一条 MIDI 轨：安装默认三件套（声源 → 延迟 → 通道条 → 主输出）
    /// 并写好本轨 MIDI 自连。返回 `(结构更新, [voice, delay, channel] node_id)`。
    ///
    /// 轨道容量满 → [`TopologyError::NoFreeTrack`]；槽位不足 →
    /// [`TopologyError::NoFreeSlot`]；主输出入边超限（> 8，见 D4/D1）→
    /// [`TopologyError::FanInOverflow`]（UI 层应引导用户改用求和节点分层）。
    pub fn add_midi_track(&mut self) -> Result<(GraphUpdate, [u64; 3]), TopologyError> {
        let Some(track) = self.track_voice.iter().position(|&s| s == NO_EDGE) else {
            return Err(TopologyError::NoFreeTrack);
        };
        // 预检容量（alloc_slot 满载 panic，必须提前挡住）。
        if self.occupied.iter().filter(|&&o| !o).count() < 3 {
            return Err(TopologyError::NoFreeSlot);
        }
        if self.cable_count + 3 > MAX_CABLES {
            return Err(TopologyError::TooManyCables);
        }
        let slots = self.allocate_track_chain(track);
        let Ok(plan) = self.compile() else {
            // 回滚分配（主输出入边超限等）。
            self.deallocate_track_chain(track);
            return Err(TopologyError::FanInOverflow);
        };
        let ids = [slots[0].1, slots[1].1, slots[2].1];
        let deployments: Vec<(u16, u64, Box<dyn Processor>)> = slots
            .into_iter()
            .enumerate()
            .map(|(kind, (slot, id))| (slot, id, chain_processor(kind, self.sample_rate)))
            .collect();
        Ok((self.build_update(plan, deployments, &[]), ids))
    }

    /// 拆除一条 MIDI 轨：三件套连同其全部效果器一并移除（cable 清理 +
    /// 槽位释放 + 路由表清空），节点经回收队列释放。未配置轨幂等成功。
    pub fn remove_midi_track(&mut self, track: usize) -> Result<GraphUpdate, TopologyError> {
        if track >= MAX_MIDI_TRACKS || self.track_voice[track] == NO_EDGE {
            return Err(TopologyError::InvalidTrack);
        }
        let mut slots = [NO_EDGE; 3 + EFFECT_SLOTS];
        let mut count = 0usize;
        for slot in [
            self.track_voice[track],
            self.track_delay[track],
            self.track_channel[track],
        ] {
            if slot != NO_EDGE {
                slots[count] = slot;
                count += 1;
            }
        }
        for effect in self.track_effects[track] {
            if effect != NO_EDGE && count < slots.len() {
                slots[count] = effect;
                count += 1;
            }
        }
        self.remove_cables_touching(&slots[..count]);
        for &slot in &slots[..count] {
            self.occupied[slot as usize] = false;
            self.slot_ids[slot as usize] = 0;
        }
        self.track_voice[track] = NO_EDGE;
        self.track_delay[track] = NO_EDGE;
        self.track_channel[track] = NO_EDGE;
        self.track_effects[track] = [NO_EDGE; EFFECT_SLOTS];
        self.midi_targets[track] = [NO_EDGE; MAX_MIDI_FANOUT];
        self.midi_target_counts[track] = 0;
        let plan = self.compile()?;
        Ok(self.build_update(plan, Vec::new(), &slots[..count]))
    }

    /// 时间轴布线：把 MIDI 轨 `track` 连到目标节点（**node_id 寻址**，
    /// P5b）——重复连线幂等返回；目标节点必须已存在。连线表不参与音频
    /// 拓扑排序，故只会因扇出超限失败。
    pub fn connect_midi(
        &mut self,
        track: usize,
        node_id: u64,
    ) -> Result<GraphUpdate, TopologyError> {
        if track >= MAX_MIDI_TRACKS {
            return Err(TopologyError::InvalidTrack);
        }
        let slot = self.node_slot(node_id).ok_or(TopologyError::InvalidSlot)?;
        if self.midi_targets[track][..self.midi_target_counts[track] as usize].contains(&slot) {
            // 幂等：重复连线视为成功（返回当前计划）。
            return Ok(self.build_update(self.compile()?, Vec::new(), &[]));
        }
        let count = self.midi_target_counts[track] as usize;
        if count >= MAX_MIDI_FANOUT {
            return Err(TopologyError::MidiFanOutOverflow);
        }
        self.midi_targets[track][count] = slot;
        self.midi_target_counts[track] += 1;
        Ok(self.build_update(self.compile()?, Vec::new(), &[]))
    }

    /// 时间轴布线：断开 MIDI 轨 `track` 到目标节点的连线（未连线幂等）。
    pub fn disconnect_midi(
        &mut self,
        track: usize,
        node_id: u64,
    ) -> Result<GraphUpdate, TopologyError> {
        if track >= MAX_MIDI_TRACKS {
            return Err(TopologyError::InvalidTrack);
        }
        let slot = self.node_slot(node_id).ok_or(TopologyError::InvalidSlot)?;
        let count = self.midi_target_counts[track] as usize;
        let Some(pos) = self.midi_targets[track][..count]
            .iter()
            .position(|&s| s == slot)
        else {
            return Ok(self.build_update(self.compile()?, Vec::new(), &[]));
        };
        self.midi_targets[track].copy_within(pos + 1..count, pos);
        self.midi_targets[track][count - 1] = NO_EDGE;
        self.midi_target_counts[track] -= 1;
        Ok(self.build_update(self.compile()?, Vec::new(), &[]))
    }

    /// 时间轴布线：设定自动化轨道 `auto_index` 的投递终点。
    ///
    /// 终点以 **node_id** 表达（P5b：取代按参数种类猜终点的过渡语义；
    /// UI 依据画布/面板选择的节点解析寻址）。重复设定即重定向。
    pub fn set_auto_route(
        &mut self,
        auto_index: usize,
        node_id: u64,
        param: ParamId,
    ) -> Result<GraphUpdate, TopologyError> {
        if auto_index >= MAX_AUTO_TRACKS {
            return Err(TopologyError::InvalidIndex);
        }
        let slot = self.node_slot(node_id).ok_or(TopologyError::InvalidSlot)?;
        self.auto_routes[auto_index] = AutoRoute { slot, param };
        Ok(self.build_update(self.compile()?, Vec::new(), &[]))
    }

    /// 时间轴布线：断开自动化轨道 `auto_index` 的连线（未连线幂等）。
    pub fn clear_auto_route(&mut self, auto_index: usize) -> Result<GraphUpdate, TopologyError> {
        if auto_index >= MAX_AUTO_TRACKS {
            return Err(TopologyError::InvalidIndex);
        }
        self.auto_routes[auto_index] = AutoRoute::default();
        Ok(self.build_update(self.compile()?, Vec::new(), &[]))
    }
}

/// 轨道三件套的处理器构造（`kind`: 0 = 声源 / 1 = 延迟 / 2 = 通道条）。
fn chain_processor(kind: usize, sample_rate: f32) -> Box<dyn Processor> {
    match kind {
        0 => Box::new(SynthProcessor::new(sample_rate)),
        1 => Box::new(DelayProcessor::new(sample_rate)),
        _ => Box::new(ChannelStripProcessor::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChannelStripProcessor, DEFAULT_MIDI_TRACKS, GraphCompiler, GraphUpdate, MAX_CABLES,
        MAX_MIDI_FANOUT, MAX_MIDI_TRACKS, RtGraph, TopologyError,
    };
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::{NoteEvent, ParamId};
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;
    use crate::realtime::voice::{RecycledNode, recycle_channel};

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    /// `set_voice` / `insert_effect` 的 Err 载荷是 `Box<dyn Processor>`
    ///（无 `Debug`），测试里用本助手断言成功。
    fn expect_swap(
        result: Result<(GraphUpdate, u64), Box<dyn Processor>>,
        msg: &str,
    ) -> (GraphUpdate, u64) {
        match result {
            Ok(pair) => pair,
            Err(_) => panic!("{msg}"),
        }
    }

    fn ctx() -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: 0,
        }
    }

    fn note_on(note: u8) -> NoteEvent {
        NoteEvent::On {
            note,
            velocity: 1.0,
        }
    }

    fn default_graph() -> RtGraph {
        GraphCompiler::default_graph(SR, BLOCK).1
    }

    fn drain_recycle(recycle: &mut rtrb::Consumer<RecycledNode>) -> Vec<u64> {
        let mut ids = Vec::new();
        while let Ok(n) = recycle.pop() {
            ids.push(n.node_id);
        }
        ids
    }

    // ---- 默认拓扑等价性（旧 Mixer 测试移植） ----

    #[test]
    fn note_plays_through_track_to_master() {
        let mut graph = default_graph();
        assert!(graph.track_note(0, note_on(69)));
        let levels = graph.process_block(&ctx());
        assert!(levels.master > 0.05, "轨道 0 触发后主输出应有信号");
        assert!(levels.tracks[0] > 0.05);
        for i in 1..DEFAULT_MIDI_TRACKS {
            assert!(levels.tracks[i].abs() < 1e-9, "未触发的轨道应为静音");
        }
        // 定容 16 轨中未配置轨（默认 4 轨之外）恒为 0。
        for level in levels.tracks[DEFAULT_MIDI_TRACKS..].iter() {
            assert!(level.abs() < 1e-12, "未配置轨电平应为 0");
        }
    }

    #[test]
    fn invalid_track_index_is_ignored() {
        let mut graph = default_graph();
        assert!(!graph.track_note(MAX_MIDI_TRACKS, note_on(60)));
        assert!(!graph.track_note(usize::MAX, note_on(60)));
        graph.track_set_param(MAX_MIDI_TRACKS, ParamId::TrackGain, 1.0);
        let levels = graph.process_block(&ctx());
        assert!(levels.master.abs() < 1e-9);
    }

    #[test]
    fn track_gain_scales_the_mix() {
        let mut graph = default_graph();
        graph.track_note(0, note_on(69));
        for _ in 0..20 {
            graph.process_block(&ctx());
        }
        let full = graph.process_block(&ctx());
        graph.track_set_param(0, ParamId::TrackGain, 0.4);
        let attenuated = graph.process_block(&ctx());
        let ratio = attenuated.master / full.master;
        assert!(
            (ratio - (0.4 / 0.8)).abs() < 0.1,
            "增益减半后主输出应约减半，ratio = {ratio}"
        );
    }

    #[test]
    fn mute_silences_a_track_but_others_play() {
        let mut graph = default_graph();
        graph.track_note(0, note_on(60));
        graph.track_note(1, note_on(64));
        for _ in 0..20 {
            graph.process_block(&ctx());
        }
        graph.track_set_param(0, ParamId::TrackMute, 1.0);
        let levels = graph.process_block(&ctx());
        assert!(
            levels.tracks[0].abs() < 1e-9,
            "静音轨道 post-fader 电平应为 0"
        );
        assert!(levels.tracks[1] > 0.05, "其他轨道不受影响");
        assert!(levels.master > 0.05, "总输出仍应有未静音轨道的信号");
    }

    #[test]
    fn pan_hard_left_puts_mono_source_only_in_left() {
        let mut graph = default_graph();
        graph.track_note(0, note_on(69));
        graph.track_set_param(0, ParamId::TrackPan, -1.0);
        graph.process_block(&ctx());
        let levels = graph.process_block(&ctx());
        assert!(levels.master > 0.05);

        let left = graph.master_channel(0).unwrap();
        let right = graph.master_channel(1).unwrap();
        assert!(left.iter().any(|s| s.abs() > 0.05), "左声道应有信号");
        assert!(
            right.iter().all(|s| s.abs() < 1e-9),
            "硬左声像下右声道应为静音"
        );
    }

    #[test]
    fn master_gain_applies_after_summing() {
        let mut graph = default_graph();
        graph.track_note(0, note_on(69));
        for _ in 0..20 {
            graph.process_block(&ctx());
        }
        let before = graph.process_block(&ctx());
        graph.master_set_param(ParamId::MasterGain, 0.25);
        let after = graph.process_block(&ctx());
        let ratio = after.master / before.master;
        assert!(
            (ratio - 0.25).abs() < 0.1,
            "主增益 0.25 应使输出约变为 1/4，ratio = {ratio}"
        );
    }

    #[test]
    fn note_off_all_releases_every_track() {
        let mut graph = default_graph();
        graph.track_note(0, note_on(60));
        graph.track_note(1, note_on(64));
        for _ in 0..20 {
            graph.process_block(&ctx());
        }
        graph.note_off_all();
        for _ in 0..100 {
            graph.process_block(&ctx());
        }
        let levels = graph.process_block(&ctx());
        assert!(
            levels.master < 0.01,
            "全部释放后应静音，实际 {}",
            levels.master
        );
    }

    #[test]
    fn track_params_route_to_chain_nodes() {
        let mut graph = default_graph();
        graph.track_set_param(0, ParamId::FilterCutoff, 500.0);
        graph.track_set_param(0, ParamId::DelayMix, 0.5);
        graph.track_set_param(0, ParamId::DelayTime, 0.1);
        graph.track_set_param(0, ParamId::Attack, 0.0);
        graph.track_note(0, note_on(69));
        let out = graph.process_block(&ctx());
        assert!(out.master.is_finite());
    }

    // ---- 图专项：多入边求和 ----

    /// 直流声源（写入常量），用于确定性验证求和数学。
    struct DcSource {
        value: f32,
    }

    impl Processor for DcSource {
        fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
            for ch in 0..buffer.channels() {
                if let Some(channel) = buffer.channel_mut(ch) {
                    channel.fill(self.value);
                }
            }
        }
    }

    #[test]
    fn fan_in_sums_multiple_sources() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        // 逐轨换装直流源 0.25：主输出应随入边数量线性求和（Σ 0.25 × 0.8）。
        let mut expected = 0.0f32;
        for track in 0..3 {
            let (update, _) = expect_swap(
                compiler.set_voice(track, Box::new(DcSource { value: 0.25 })),
                "换装",
            );
            let (mut tx, mut rx) = recycle_channel();
            graph.apply_update(update, &mut tx);
            assert_eq!(drain_recycle(&mut rx).len(), 1, "旧内置声源应被回收");
            expected += 0.25 * 0.8;
            let levels = graph.process_block(&ctx());
            assert!(
                (levels.master - expected).abs() < 1e-6,
                "第 {} 轨接入后主输出应为 {}，实际 {}",
                track + 1,
                expected,
                levels.master
            );
        }
    }

    // ---- 结构更新协议 ----

    /// 测试效果器：增益 0.5（验证插入后信号减半）。
    struct HalfGain;

    impl Processor for HalfGain {
        fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
            for ch in 0..buffer.channels() {
                if let Some(channel) = buffer.channel_mut(ch) {
                    for sample in channel.iter_mut() {
                        *sample *= 0.5;
                    }
                }
            }
        }
    }

    #[test]
    fn insert_effect_attenuates_then_clear_restores_and_recycles() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        graph.track_note(0, note_on(69));
        for _ in 0..20 {
            graph.process_block(&ctx());
        }
        let before = graph.process_block(&ctx());

        // 插入 0.5 增益效果器（位于延迟与通道条之间）。
        let (update, effect_id) =
            expect_swap(compiler.insert_effect(0, Box::new(HalfGain)), "插入效果器");
        graph.apply_update(update, &mut recycle_tx);
        assert!(
            drain_recycle(&mut recycle_rx).is_empty(),
            "插入不应触发回收"
        );
        for _ in 0..3 {
            graph.process_block(&ctx());
        }
        let after = graph.process_block(&ctx());
        let ratio = after.master / before.master;
        assert!(
            (ratio - 0.5).abs() < 0.05,
            "效果器应使输出减半，ratio = {ratio}"
        );

        // 清空效果器 → 节点 + node_id 经回收队列返回。
        let update = compiler.clear_effects(0).expect("清空效果器");
        graph.apply_update(update, &mut recycle_tx);
        assert_eq!(
            drain_recycle(&mut recycle_rx),
            vec![effect_id],
            "被移除的效果器节点应带 node_id 回收"
        );
        for _ in 0..3 {
            graph.process_block(&ctx());
        }
        let restored = graph.process_block(&ctx());
        let ratio = restored.master / before.master;
        assert!(
            (ratio - 1.0).abs() < 0.05,
            "清空后输出应恢复，ratio = {ratio}"
        );
    }

    #[test]
    fn set_voice_recycles_old_processor() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        let (update, _id) = expect_swap(compiler.set_voice(0, Box::new(HalfGain)), "换装声源");
        graph.apply_update(update, &mut recycle_tx);
        let recycled = drain_recycle(&mut recycle_rx);
        assert_eq!(recycled.len(), 1, "旧内置声源应被回收");
        assert_ne!(recycled[0], 0);
        // 换装后轨道仍应工作（HalfGain 无音符 → 静音）。
        let levels = graph.process_block(&ctx());
        assert!(levels.tracks[0].abs() < 1e-9);
    }

    #[test]
    fn removed_voice_slot_stays_silent() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, _recycle_rx) = recycle_channel();
        let (update, _id) = expect_swap(compiler.set_voice(1, Box::new(HalfGain)), "换装声源");
        graph.apply_update(update, &mut recycle_tx);
        graph.track_note(1, note_on(60));
        let levels = graph.process_block(&ctx());
        assert!(levels.tracks[1].abs() < 1e-9, "无音符的外部声源应为静音");
    }

    // ---- 编译器校验 ----

    #[test]
    fn compile_rejects_cycle() {
        let (mut compiler, _graph) = GraphCompiler::default_graph(SR, BLOCK);
        // 人为制造环：主输出 → 轨道 0 声源（私有字段，仅测试可达）。
        compiler
            .add_cable(compiler.master_slot, compiler.track_voice[0])
            .expect("容量内应可加线");
        assert!(matches!(compiler.compile(), Err(TopologyError::Cycle)));
    }

    #[test]
    fn compile_rejects_fan_in_overflow() {
        let (mut compiler, _graph) = GraphCompiler::default_graph(SR, BLOCK);
        // 8 条入边是上限；默认主输出已有 4 条，再补 5 条触发溢出。
        for slot in 13..18usize {
            compiler.occupied[slot] = true;
            compiler.slot_ids[slot] = 900 + slot as u64;
            compiler
                .add_cable(slot as u16, compiler.master_slot)
                .expect("加线");
        }
        assert!(matches!(
            compiler.compile(),
            Err(TopologyError::FanInOverflow)
        ));
    }

    #[test]
    fn default_plan_order_is_topological() {
        let (_compiler, graph) = GraphCompiler::default_graph(SR, BLOCK);
        let plan = graph.plan;
        // 每个节点必须排在其入边源之后。
        for i in 0..plan.order_len {
            let s = plan.order[i] as usize;
            for &src in &plan.in_edges[s][..plan.in_counts[s] as usize] {
                let pos = plan.order[..plan.order_len]
                    .iter()
                    .position(|&o| o == src)
                    .expect("入边源必须在拓扑序中");
                assert!(pos < i, "拓扑序 violated：槽 {s} 先于其源 {src}");
            }
        }
    }

    #[test]
    fn empty_graph_outputs_silence() {
        let mut graph = RtGraph::new(BLOCK);
        let levels = graph.process_block(&ctx());
        assert!(levels.master.abs() < 1e-9);
        assert!(graph.master_channel(0).is_none());
    }

    // ---- ChannelStripProcessor 单元 ----

    #[test]
    fn channel_strip_applies_gain_pan_and_mute() {
        let mut strip = ChannelStripProcessor::new();
        let mut buf = AudioBuffer::new(2, 8);
        buf.set_len(8);
        let fill = |buf: &mut AudioBuffer| {
            buf.channel_mut(0).unwrap().fill(0.5);
            buf.channel_mut(1).unwrap().fill(0.5);
        };
        // 默认：增益 0.8、声像居中（pan 增益 1）→ 双声道 ×0.8。
        fill(&mut buf);
        strip.process(&ctx(), &mut buf);
        assert!(
            buf.channel(0)
                .unwrap()
                .iter()
                .all(|s| (*s - 0.4).abs() < 1e-6)
        );
        assert!(
            buf.channel(1)
                .unwrap()
                .iter()
                .all(|s| (*s - 0.4).abs() < 1e-6)
        );
        // 硬左：右声道归零，左声道仍为 0.5 × 0.8。
        strip.set_param(ParamId::TrackPan, -1.0);
        fill(&mut buf);
        strip.process(&ctx(), &mut buf);
        assert!(
            buf.channel(0)
                .unwrap()
                .iter()
                .all(|s| (*s - 0.4).abs() < 1e-6)
        );
        assert!(buf.channel(1).unwrap().iter().all(|s| s.abs() < 1e-9));
        // 静音：全零。
        strip.set_param(ParamId::TrackMute, 1.0);
        fill(&mut buf);
        strip.process(&ctx(), &mut buf);
        assert!(buf.channel(0).unwrap().iter().all(|s| s.abs() < 1e-12));
        assert!(buf.channel(1).unwrap().iter().all(|s| s.abs() < 1e-12));
    }

    // ---- 压测容量上限的烟雾测试 ----

    #[test]
    fn effect_slots_per_track_capped() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        // 每轨 4 槽 + 容量富余：连续插入 8 个都应成功（默认图 13/64 槽）。
        for _ in 0..8 {
            let (update, _) = expect_swap(
                compiler.insert_effect(0, Box::new(HalfGain)),
                "槽位富余时应插入成功",
            );
            graph.apply_update(update, &mut recycle_tx);
        }
        assert!(drain_recycle(&mut recycle_rx).is_empty());
        // 64 - 13 = 51 空槽：继续插满应最终失败并交还节点。
        while let Ok((update, _)) = compiler.insert_effect(1, Box::new(HalfGain)) {
            graph.apply_update(update, &mut recycle_tx);
        }
        // 图仍能正常处理。
        let levels = graph.process_block(&ctx());
        assert!(levels.master.is_finite());
    }

    // ---- 时间轴连线表（P3，D7） ----

    #[test]
    fn midi_fanout_delivers_to_all_connected_targets() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        // 把 MIDI 轨 0 扇出到轨道 1 的声源（node_id 寻址）：一个音符两轨齐鸣。
        let target_id = compiler.track_voice_node_id(1).expect("轨道 1 声源应存在");
        let update = compiler.connect_midi(0, target_id).expect("扇出连线应成功");
        graph.apply_update(update, &mut recycle_tx);
        assert!(drain_recycle(&mut recycle_rx).is_empty());
        assert_eq!(graph.plan.midi_target_counts[0], 2);

        graph.track_note(0, note_on(69));
        let levels = graph.process_block(&ctx());
        assert!(levels.tracks[0] > 0.05, "本轨声源应发声");
        assert!(levels.tracks[1] > 0.05, "扇出目标声源也应发声");

        // 断开后连线表回到单目标。
        let update = compiler.disconnect_midi(0, target_id).expect("断开应成功");
        graph.apply_update(update, &mut recycle_tx);
        assert_eq!(graph.plan.midi_target_counts[0], 1);
        assert_eq!(graph.plan.midi_targets[0][0], compiler.track_voice[0]);
    }

    #[test]
    fn midi_fanout_is_capped_and_validated() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, _recycle_rx) = recycle_channel();
        // 默认每轨 1 条；再加 3 条到达上限 4。
        for t in 1..4usize {
            let id = compiler.track_voice_node_id(t).expect("声源应存在");
            let update = compiler.connect_midi(0, id).expect("上限内应成功");
            graph.apply_update(update, &mut recycle_tx);
        }
        assert_eq!(graph.plan.midi_target_counts[0], MAX_MIDI_FANOUT as u8);
        // 第 5 条被拒绝：轨道 0 已达上限，任意有效目标都应触发扇出超限。
        let ch0 = compiler.track_channel_node_id(0).expect("通道条应存在");
        assert!(matches!(
            compiler.connect_midi(0, ch0),
            Err(TopologyError::MidiFanOutOverflow)
        ));
        // 未分配的 node_id 拒绝。
        assert!(matches!(
            compiler.connect_midi(0, u64::MAX),
            Err(TopologyError::InvalidSlot)
        ));
        assert!(matches!(
            compiler.connect_midi(0, 0),
            Err(TopologyError::InvalidSlot)
        ));
        // 越界轨道拒绝。
        assert!(matches!(
            compiler.connect_midi(MAX_MIDI_TRACKS, ch0),
            Err(TopologyError::InvalidTrack)
        ));
        // 重复连线幂等。
        let id1 = compiler.track_voice_node_id(1).expect("声源应存在");
        let update = compiler.connect_midi(0, id1).expect("重复连线幂等");
        graph.apply_update(update, &mut recycle_tx);
        assert_eq!(graph.plan.midi_target_counts[0], MAX_MIDI_FANOUT as u8);
    }

    #[test]
    fn auto_route_delivers_curve_values_to_target_param() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        // 直流源 0.25 换装轨道 0 声源；自动化轨道 0 连到本轨通道条增益
        //（node_id 寻址，取代 route_slot 猜终点）。
        let (update, _) = expect_swap(
            compiler.set_voice(0, Box::new(DcSource { value: 0.25 })),
            "换装",
        );
        graph.apply_update(update, &mut recycle_tx);
        assert_eq!(drain_recycle(&mut recycle_rx).len(), 1, "旧声源应被回收");
        let channel_id = compiler.track_channel_node_id(0).expect("通道条应存在");
        let update = compiler
            .set_auto_route(0, channel_id, ParamId::TrackGain)
            .expect("自动化连线应成功");
        graph.apply_update(update, &mut recycle_tx);

        // 曲线值 0.5 → 通道条增益 0.5 → 主输出 = 0.25 × 0.5。
        graph.auto_set_param(0, 0.5);
        let levels = graph.process_block(&ctx());
        assert!(
            (levels.master - 0.125).abs() < 1e-6,
            "自动化应驱动通道条增益，实际 {}",
            levels.master
        );

        // 未连线的自动化轨道静默。
        graph.auto_set_param(1, 1.5);
        let levels = graph.process_block(&ctx());
        assert!((levels.master - 0.125).abs() < 1e-6);

        // 断开后投递失效，增益保持最后值。
        let update = compiler.clear_auto_route(0).expect("断开应成功");
        graph.apply_update(update, &mut recycle_tx);
        graph.auto_set_param(0, 1.5);
        let levels = graph.process_block(&ctx());
        assert!((levels.master - 0.125).abs() < 1e-6, "断开后不应再改参数");

        // 未分配的 node_id 拒绝连线（不破坏原连线）。
        assert!(matches!(
            compiler.set_auto_route(1, u64::MAX, ParamId::TrackGain),
            Err(TopologyError::InvalidSlot)
        ));
    }

    // ---- P5b：node_id 寻址 / 动态轨数 / 多出边扇出 ----

    #[test]
    fn node_id_param_via_master() {
        let (compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let master_id = compiler.master_node_id();
        assert_ne!(master_id, 0, "主输出 node_id 应在构造期分配");
        graph.track_note(0, note_on(69));
        for _ in 0..10 {
            graph.process_block(&ctx());
        }
        let before = graph.process_block(&ctx());
        assert!(graph.node_set_param(master_id, ParamId::MasterGain, 0.25));
        let after = graph.process_block(&ctx());
        let ratio = after.master / before.master;
        assert!(
            (ratio - 0.25).abs() < 0.1,
            "node_id 寻址的主增益应生效，ratio = {ratio}"
        );
    }

    #[test]
    fn add_and_remove_midi_track_roundtrip() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, mut recycle_rx) = recycle_channel();
        assert_eq!(compiler.midi_track_count(), DEFAULT_MIDI_TRACKS);

        // 加轨：返回三个新 node_id，新轨可发声。
        let (update, [voice_id, delay_id, channel_id]) =
            compiler.add_midi_track().expect("加轨应成功");
        graph.apply_update(update, &mut recycle_tx);
        assert!(drain_recycle(&mut recycle_rx).is_empty(), "加轨不回收");
        assert_eq!(compiler.midi_track_count(), DEFAULT_MIDI_TRACKS + 1);
        assert_ne!(voice_id, 0);
        assert_ne!(delay_id, 0);
        assert_ne!(channel_id, 0);
        assert_eq!(
            compiler.track_voice_node_id(DEFAULT_MIDI_TRACKS),
            Some(voice_id),
            "新轨路由表应指向新声源"
        );
        graph.track_note(DEFAULT_MIDI_TRACKS, note_on(69));
        let levels = graph.process_block(&ctx());
        assert!(levels.tracks[DEFAULT_MIDI_TRACKS] > 0.05, "新轨应能发声");

        // 拆轨：三件套回收，路由表清空。
        let update = compiler
            .remove_midi_track(DEFAULT_MIDI_TRACKS)
            .expect("拆轨应成功");
        graph.apply_update(update, &mut recycle_tx);
        assert_eq!(drain_recycle(&mut recycle_rx).len(), 3, "三件套应全部回收");
        assert_eq!(compiler.midi_track_count(), DEFAULT_MIDI_TRACKS);
        assert_eq!(compiler.track_voice_node_id(DEFAULT_MIDI_TRACKS), None);
        graph.track_note(DEFAULT_MIDI_TRACKS, note_on(69));
        let levels = graph.process_block(&ctx());
        assert!(levels.tracks[DEFAULT_MIDI_TRACKS].abs() < 1e-12);
        assert!(levels.master.is_finite(), "拆轨后图仍应正常处理");
    }

    #[test]
    fn add_track_fails_when_master_fan_in_exhausted() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, _recycle_rx) = recycle_channel();
        // 默认主输出已有 4 条入边（D4 上限 8）：加 4 条轨到达上限。
        for _ in 0..4 {
            let (update, _) = compiler.add_midi_track().expect("上限内应成功");
            graph.apply_update(update, &mut recycle_tx);
        }
        assert_eq!(compiler.midi_track_count(), 8);
        // 第 9 条轨 → 主输出入边超限，编译失败且回滚分配。
        assert!(matches!(
            compiler.add_midi_track(),
            Err(TopologyError::FanInOverflow)
        ));
        assert_eq!(compiler.midi_track_count(), 8, "失败后轨数不变");
        // 图仍正常处理。
        graph.track_note(0, note_on(69));
        let levels = graph.process_block(&ctx());
        assert!(levels.master > 0.05);
    }

    #[test]
    fn multi_out_fanout_sums_into_two_sinks() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        let (mut recycle_tx, _recycle_rx) = recycle_channel();
        // 多出边：轨道 0 声源额外连到轨道 1 通道条（一个输出口两根线）。
        let voice0 = compiler.track_voice_node_id(0).expect("声源应存在");
        let slot0 = compiler.node_slot(voice0).expect("槽位应存在");
        let channel1_slot = compiler.track_channel[1];
        compiler
            .add_cable(slot0, channel1_slot)
            .expect("第二根出线应成功");
        compiler.compile().expect("拓扑应仍无环");

        // 直流源 0.25 → 轨 0 与轨 1 通道条（各 ×0.8）→ 主输出 0.25×0.8×2。
        let (update, _) = expect_swap(
            compiler.set_voice(0, Box::new(DcSource { value: 0.25 })),
            "换装",
        );
        graph.apply_update(update, &mut recycle_tx);
        let levels = graph.process_block(&ctx());
        assert!(
            (levels.master - 0.25 * 0.8 * 2.0).abs() < 1e-6,
            "多出扇出应把信号送进两个通道条，实际 {}",
            levels.master
        );
    }

    #[test]
    fn cable_capacity_is_capped() {
        let (mut compiler, mut graph) = GraphCompiler::default_graph(SR, BLOCK);
        // 灌满 cable 列表（编译器不校验端点占用，专注容量语义）。
        let mut i = 0u16;
        while compiler.cable_count < MAX_CABLES {
            compiler.add_cable(60, 61 + i).expect("容量内应成功");
            i += 1;
        }
        assert_eq!(compiler.cable_count, MAX_CABLES);
        // 满载后再加一条唯一连线 → TooManyCables。
        assert!(matches!(
            compiler.add_cable(60, 300),
            Err(TopologyError::TooManyCables)
        ));
        // 重复连线仍幂等（去重优先于容量检查）。
        assert!(compiler.add_cable(60, 61).is_ok());
        let levels = graph.process_block(&ctx());
        assert!(levels.master.is_finite());
    }
}
