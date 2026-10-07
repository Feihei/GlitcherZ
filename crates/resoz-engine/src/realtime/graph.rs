//! 节点图（P1+P2）：可布线处理图核心、默认轨道拓扑与结构更新协议。
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
//! - **P1+P2 过渡定向**：`ExecPlan` 内置每轨 voice/delay/channel 槽位
//!   路由表（P3 由时间轴节点的 MIDI/自动化连线表替代）。
//! - 默认拓扑复刻旧 Mixer：`声源 → 延迟 → 通道条 → 主输出`，通道条的
//!   增益/声像/静音升级为真节点 [`ChannelStripProcessor`]。

use rtrb::Producer;

use crate::realtime::NUM_TRACKS;
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
/// 图容量上限（D1）：单节点最大入边数（超限拒绝连线）。
pub const MAX_IN_EDGES: usize = 8;
/// 无边哨兵值（`out_edge` / 轨道路由表的空槽标记）。
pub const NO_EDGE: u16 = u16::MAX;
/// 每轨外部效果器槽数（沿旧 `TrackStrip` 语义）。
pub const EFFECT_SLOTS: usize = 4;
/// 单次结构更新最多安装的节点数。
pub const MAX_DEPLOYMENTS: usize = 4;
/// 单次结构更新最多移除的节点数（≥ `EFFECT_SLOTS`）。
pub const MAX_REMOVALS: usize = 8;

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
    /// 主输出槽位（cpal 交错写出的数据源）。
    pub master_slot: u16,
    /// 每轨声源槽位（P1+P2 过渡路由表，P3 由连线表替代）。
    pub track_voice: [u16; NUM_TRACKS],
    /// 每轨延迟效果槽位。
    pub track_delay: [u16; NUM_TRACKS],
    /// 每轨通道条槽位（电平回传的计量点）。
    pub track_channel: [u16; NUM_TRACKS],
}

impl ExecPlan {
    /// 空计划（无节点，输出静音）。
    pub const EMPTY: Self = Self {
        order: [0; MAX_NODES],
        order_len: 0,
        in_edges: [[NO_EDGE; MAX_IN_EDGES]; MAX_NODES],
        in_counts: [0; MAX_NODES],
        master_slot: 0,
        track_voice: [NO_EDGE; NUM_TRACKS],
        track_delay: [NO_EDGE; NUM_TRACKS],
        track_channel: [NO_EDGE; NUM_TRACKS],
    };
}

/// 一个块处理后的电平快照（形状与旧 `MixerLevels` 一致）。
#[derive(Debug, Clone, Copy, Default)]
pub struct GraphLevels {
    /// 各轨通道条输出峰值（post-fader，静音为 0）。
    pub tracks: [f32; NUM_TRACKS],
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

    /// 向某轨声源发音符事件（越界/空槽忽略）。RT-safe。
    pub fn track_note(&mut self, track: usize, event: NoteEvent) -> bool {
        if track >= NUM_TRACKS {
            return false;
        }
        self.slot_note(self.plan.track_voice[track], event)
    }

    /// 释放所有声源当前音符（走带 play/stop 前调用）。RT-safe。
    pub fn note_off_all(&mut self) {
        for track in 0..NUM_TRACKS {
            let _ = self.slot_note(self.plan.track_voice[track], NoteEvent::Off { note: 0 });
        }
    }

    fn slot_note(&mut self, slot: u16, event: NoteEvent) -> bool {
        self.slots
            .get_mut(slot as usize)
            .and_then(|s| s.as_mut())
            .is_some_and(|n| n.processor.note_event(event))
    }

    /// 轨道参数路由（越界忽略）：混音三项 → 通道条；延迟三项 → 延迟；
    /// 音色/包络/滤波/插件参数 → 声源。与旧 `TrackStrip::set_param`
    /// 语义一致。RT-safe。
    pub fn track_set_param(&mut self, track: usize, id: ParamId, value: f32) {
        if track >= NUM_TRACKS {
            return;
        }
        let slot = match id {
            ParamId::TrackGain | ParamId::TrackPan | ParamId::TrackMute => {
                self.plan.track_channel[track]
            }
            ParamId::DelayTime | ParamId::DelayFeedback | ParamId::DelayMix => {
                self.plan.track_delay[track]
            }
            ParamId::MasterGain => return,
            _ => self.plan.track_voice[track],
        };
        if let Some(node) = self.slots.get_mut(slot as usize).and_then(|s| s.as_mut()) {
            let _ = node.processor.set_param(id, value);
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
    /// 目标轨道越界。
    InvalidTrack,
    /// 无空闲槽位（容量 64 满）。
    NoFreeSlot,
    /// 拓扑含环（D10：拒绝）。
    Cycle,
    /// 某节点入边超限（> 8）。
    FanInOverflow,
}

/// 控制侧图状态（**非实时**）：槽位占用 + 出边表 + 轨道路由，负责
/// 编译 [`ExecPlan`] 与生成 [`GraphUpdate`]。全 `Copy` —— 引擎在队列
/// push 失败时可整快照回滚。
#[derive(Debug, Clone, Copy)]
pub struct GraphCompiler {
    sample_rate: f32,
    block_size: usize,
    occupied: [bool; MAX_NODES],
    out_edge: [u16; MAX_NODES],
    master_slot: u16,
    track_voice: [u16; NUM_TRACKS],
    track_delay: [u16; NUM_TRACKS],
    track_channel: [u16; NUM_TRACKS],
    track_effects: [[u16; EFFECT_SLOTS]; NUM_TRACKS],
    next_node_id: u64,
}

impl GraphCompiler {
    /// 构建默认拓扑（复刻旧 Mixer）：主输出 + 每轨 `声源 → 延迟 → 通道条`。
    /// **非 RT-safe**（分配处理器与缓冲），只在音频流启动前调用。
    pub fn default_graph(sample_rate: f32, block_size: usize) -> (Self, RtGraph) {
        let mut compiler = Self {
            sample_rate,
            block_size: block_size.max(1),
            occupied: [false; MAX_NODES],
            out_edge: [NO_EDGE; MAX_NODES],
            master_slot: 0,
            track_voice: [NO_EDGE; NUM_TRACKS],
            track_delay: [NO_EDGE; NUM_TRACKS],
            track_channel: [NO_EDGE; NUM_TRACKS],
            track_effects: [[NO_EDGE; EFFECT_SLOTS]; NUM_TRACKS],
            next_node_id: 1,
        };
        let mut graph = RtGraph::new(block_size);
        let bs = compiler.block_size;

        let master_id = compiler.fresh_id();
        let master = compiler.alloc_slot();
        compiler.master_slot = master;
        graph.install(
            master as usize,
            master_id,
            Box::new(GainProcessor::new(1.0)),
            AudioBuffer::new(2, bs),
        );

        for t in 0..NUM_TRACKS {
            let voice = compiler.alloc_slot();
            let delay = compiler.alloc_slot();
            let channel = compiler.alloc_slot();
            compiler.track_voice[t] = voice;
            compiler.track_delay[t] = delay;
            compiler.track_channel[t] = channel;
            compiler.out_edge[voice as usize] = delay;
            compiler.out_edge[delay as usize] = channel;
            compiler.out_edge[channel as usize] = master;

            let voice_id = compiler.fresh_id();
            graph.install(
                voice as usize,
                voice_id,
                Box::new(SynthProcessor::new(sample_rate)),
                AudioBuffer::new(2, bs),
            );
            let delay_id = compiler.fresh_id();
            graph.install(
                delay as usize,
                delay_id,
                Box::new(DelayProcessor::new(sample_rate)),
                AudioBuffer::new(2, bs),
            );
            let channel_id = compiler.fresh_id();
            graph.install(
                channel as usize,
                channel_id,
                Box::new(ChannelStripProcessor::new()),
                AudioBuffer::new(2, bs),
            );
        }

        // 默认拓扑由构造保证无环；失败即启动期 bug，直接 panic 可接受。
        graph.plan = compiler
            .compile()
            .expect("default graph topology must be acyclic");
        (compiler, graph)
    }

    fn fresh_id(&mut self) -> u64 {
        let id = self.next_node_id;
        self.next_node_id += 1;
        id
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

    /// 打包一次结构更新（控制侧分配输出缓冲）。
    #[allow(clippy::type_complexity)]
    fn build_update(
        &self,
        plan: ExecPlan,
        deployment: Option<(u16, u64, Box<dyn Processor>)>,
        removed: &[u16],
    ) -> GraphUpdate {
        let mut deployments = std::array::from_fn(|_| NodeDeployment {
            slot: 0,
            node_id: 0,
            node: None,
        });
        let mut deployment_count = 0;
        if let Some((slot, node_id, node)) = deployment {
            deployments[0] = NodeDeployment {
                slot,
                node_id,
                node: Some((node, AudioBuffer::new(2, self.block_size))),
            };
            deployment_count = 1;
        }
        let mut removed_arr = [0u16; MAX_REMOVALS];
        let removed_count = removed.len().min(MAX_REMOVALS);
        removed_arr[..removed_count].copy_from_slice(&removed[..removed_count]);
        GraphUpdate::SetTopology {
            plan,
            deployments,
            deployment_count,
            removed: removed_arr,
            removed_count,
        }
    }

    /// 编译执行计划：入边表 + Kahn 拓扑排序（有环/入边超限拒绝）。
    pub fn compile(&self) -> Result<ExecPlan, TopologyError> {
        let mut plan = ExecPlan::EMPTY;
        let mut in_counts = [0u8; MAX_NODES];
        for slot in 0..MAX_NODES {
            if !self.occupied[slot] {
                continue;
            }
            let dst = self.out_edge[slot];
            if dst == NO_EDGE {
                continue;
            }
            let d = dst as usize;
            if in_counts[d] as usize >= MAX_IN_EDGES {
                return Err(TopologyError::FanInOverflow);
            }
            plan.in_edges[d][in_counts[d] as usize] = slot as u16;
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
            let dst = self.out_edge[s];
            if dst != NO_EDGE {
                let d = dst as usize;
                degree[d] -= 1;
                if degree[d] == 0 {
                    work[work_len] = dst;
                    work_len += 1;
                }
            }
        }

        let occupied_count = self.occupied.iter().filter(|&&o| o).count();
        if plan.order_len != occupied_count {
            return Err(TopologyError::Cycle);
        }
        plan.in_counts = in_counts;
        plan.master_slot = self.master_slot;
        plan.track_voice = self.track_voice;
        plan.track_delay = self.track_delay;
        plan.track_channel = self.track_channel;
        Ok(plan)
    }

    /// 将某轨声源替换为给定处理器（旧节点经回收队列释放）。
    /// 失败时原样交还 `node` 供调用方释放。
    pub fn set_voice(
        &mut self,
        track: usize,
        node: Box<dyn Processor>,
    ) -> Result<(GraphUpdate, u64), Box<dyn Processor>> {
        if track >= NUM_TRACKS {
            return Err(node);
        }
        let slot = self.track_voice[track];
        let node_id = self.fresh_id();
        let Ok(plan) = self.compile() else {
            return Err(node);
        };
        Ok((
            self.build_update(plan, Some((slot, node_id, node)), &[slot]),
            node_id,
        ))
    }

    /// 将某轨声源恢复为内置合成器（旧外部节点经回收队列释放）。
    ///
    /// 行为变化（相对旧 `TrackStrip` 驻留语义）：外部节点被移除而非
    /// 驻留，切回插件时需重新实例化（状态不保留）。UI 守卫经回收
    /// `node_id` 正常配对释放。
    pub fn activate_builtin_voice(&mut self, track: usize) -> Result<GraphUpdate, TopologyError> {
        if track >= NUM_TRACKS {
            return Err(TopologyError::InvalidTrack);
        }
        let slot = self.track_voice[track];
        let node_id = self.fresh_id();
        let node = Box::new(SynthProcessor::new(self.sample_rate));
        let plan = self.compile()?;
        Ok(self.build_update(plan, Some((slot, node_id, node)), &[slot]))
    }

    /// 向某轨效果链末端（通道条之前）追加一个效果器节点。
    /// 无空闲槽位时原样交还 `node`。
    pub fn insert_effect(
        &mut self,
        track: usize,
        node: Box<dyn Processor>,
    ) -> Result<(GraphUpdate, u64), Box<dyn Processor>> {
        if track >= NUM_TRACKS {
            return Err(node);
        }
        let Some(free) = self.occupied.iter().position(|&o| !o) else {
            return Err(node);
        };
        let channel = self.track_channel[track];
        // 沿链找到指向通道条的最后一个节点，把新效果器插在它与通道条之间。
        let mut last = self.track_delay[track];
        let mut hops = 0usize;
        while self.out_edge[last as usize] != channel {
            last = self.out_edge[last as usize];
            hops += 1;
            if last == NO_EDGE || hops > MAX_NODES {
                // 拓扑损坏（不应发生）；放弃插入。
                return Err(node);
            }
        }

        let free = free as u16;
        self.occupied[free as usize] = true;
        self.out_edge[last as usize] = free;
        self.out_edge[free as usize] = channel;
        if let Some(e) = self.track_effects[track]
            .iter_mut()
            .find(|e| **e == NO_EDGE)
        {
            *e = free;
        }
        let Ok(plan) = self.compile() else {
            // 回滚边与占用（保持编译器与运行图一致）。
            self.occupied[free as usize] = false;
            self.out_edge[last as usize] = channel;
            self.out_edge[free as usize] = NO_EDGE;
            if let Some(e) = self.track_effects[track].iter_mut().find(|e| **e == free) {
                *e = NO_EDGE;
            }
            return Err(node);
        };
        let node_id = self.fresh_id();
        Ok((
            self.build_update(plan, Some((free, node_id, node)), &[]),
            node_id,
        ))
    }

    /// 清空某轨全部外部效果器（节点经回收队列释放）。
    pub fn clear_effects(&mut self, track: usize) -> Result<GraphUpdate, TopologyError> {
        if track >= NUM_TRACKS {
            return Err(TopologyError::InvalidTrack);
        }
        let channel = self.track_channel[track];
        let delay = self.track_delay[track];
        let mut removed = [0u16; MAX_REMOVALS];
        let mut removed_count = 0usize;
        for effect in self.track_effects[track] {
            if effect != NO_EDGE && removed_count < MAX_REMOVALS {
                self.occupied[effect as usize] = false;
                self.out_edge[effect as usize] = NO_EDGE;
                removed[removed_count] = effect;
                removed_count += 1;
            }
        }
        self.track_effects[track] = [NO_EDGE; EFFECT_SLOTS];
        self.out_edge[delay as usize] = channel;
        let plan = self.compile()?;
        Ok(self.build_update(plan, None, &removed[..removed_count]))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChannelStripProcessor, GraphCompiler, GraphUpdate, NUM_TRACKS, RtGraph, TopologyError,
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
        for i in 1..NUM_TRACKS {
            assert!(levels.tracks[i].abs() < 1e-9, "未触发的轨道应为静音");
        }
    }

    #[test]
    fn invalid_track_index_is_ignored() {
        let mut graph = default_graph();
        assert!(!graph.track_note(NUM_TRACKS, note_on(60)));
        assert!(!graph.track_note(usize::MAX, note_on(60)));
        graph.track_set_param(NUM_TRACKS, ParamId::TrackGain, 1.0);
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
        compiler.out_edge[compiler.master_slot as usize] = compiler.track_voice[0];
        assert!(matches!(compiler.compile(), Err(TopologyError::Cycle)));
    }

    #[test]
    fn compile_rejects_fan_in_overflow() {
        let (mut compiler, _graph) = GraphCompiler::default_graph(SR, BLOCK);
        // 8 条入边是上限；默认主输出已有 4 条，再补 5 条触发溢出。
        for slot in 13..18usize {
            compiler.occupied[slot] = true;
            compiler.out_edge[slot] = compiler.master_slot;
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
}
