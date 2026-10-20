---
title: "模块节点图架构"
summary: "GlitcherZ 引擎演进：可自由布线的处理图 + 时间轴信号源（Cardinal/PureData 式布线，乐队现场隐喻）"
status: 已评审定稿（2026-10-07 拷问式 Q&A 两轮）；v2 修订（2026-10-09 D14–D21 单输入 + Mixer 语义）评审通过
related: [../plan.md, ../architecture.md]
---

# Spec：模块节点图架构

## 1. 目标与非目标

**目标**：把合成器、效果器、混音通道从 `Mixer` 的固定拓扑（`[TrackStrip; 4]`）转为可自由布线的图节点；时间轴的 MIDI 与自动化信号作为**时间轴节点**的输出端口，经连线接入图——模拟乐队现场的 analog 布线（乐谱/指挥 = 时间轴，乐手 + pedals + 调音台 = 节点图）。

**非目标**（本期不做）：

- 音频反馈环路（P6 可选，1 块延迟节点）；
- 逐样本事件偏移（维持块粒度，现状 ≤2.7 ms）；
- 项目文件 v1 兼容（**决策 D8：不考虑兼容**，model 直接 v2 重构）；
- 增量子图更新（用"执行计划重建 + 差异化节点回收"替代，见 D5）。

## 2. 决策记录（拷问式 Q&A 定稿）

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| D1 | 定容规模 | **64 节点 / 128 连线 / 每节点 ≤8 入边**，超限在 UI 拒绝；全部实时结构定容预分配（与现有 `MAX_*` 风格一致） |
| D2 | 端口剖面 | 由节点类型决定（见 §3 表），乐器 0 音频入、效果器 1入1出、通道条/求和多入边。**入边部分已被 D14/D15 修订（2026-10-09）** |
| D3 | 连线三类型 | `Audio` / `Midi` / `Automation`，与端口类型绑定；类型不匹配 UI 拉线时拒绝，引擎侧零防御 |
| D4 | 多入边求和 | 一个音频入口可接多条入边，自动求和（模拟信号直觉）。**已被 D14 修订（2026-10-09）：隐式加合废止** |
| D5 | 结构更新协议 | 拓扑变更 = **重建执行计划**（控制侧拓扑排序 + 缓冲下标分配，微秒级）+ **差异化节点回收**（新旧图都存在的节点复用，只回收被删节点）。参数/事件走命令队列与块内直写，**永不触发重建** |
| D6 | 悬空语义 | 无输入通路的节点**照常执行**（副作用——延迟线残响等——保持活跃，重新接线状态连续），但输出无人消费即听感静音；**主输出节点固定存在、不可删除、不可改类型**，cpal 回调只读它的输出缓冲 |
| D7 | 时间轴节点 | 固定存在（与主输出同级）：**MIDI 轨道**与**自动化轨道**是两类独立实体，各出一种端口；自动化曲线不再内嵌目标参数（现 `AutoParam` 职责），**目标 = 连线终点 (node_id, param)** |
| D8 | 兼容性 | 不做 v1 迁移；`Project` v2 按节点图重新设计 |
| D9 | 实施策略 | **P1+P2 连做**，不留两套引擎并存中间态 |
| D10 | 环路 | 拓扑排序失败（有环）→ 拒绝该连线；**P6 修订（2026-10-21）**：环必须经过一个显式 **Feedback 节点**才合法——该节点的入边是"抽头"，编译器把它排在抽头源**之前**（读上一块缓冲），因此环不再参与排序约束；不含 Feedback 节点的环仍然拒绝 |
| D11 | node_id 持久化 | `node_id` 随项目文件持久化（完整形态里 cables 跨会话引用节点的前提，补全 P4 过渡期"会话内单调不持久化"的临时口径）；加载后引擎编译器从文档 `max(node_id) + 1` 继续单调分配 |
| D12 | 轨道动态化 | MIDI 轨 `Vec<MidiTrack>`（引擎侧定容 `MAX_MIDI_TRACKS = 16` + 活跃轨数控制侧下发；`midi_targets`/路由表/音序器全部扩容）；自动化轨道顶层 `Vec<AutomationTrack>`（引擎已定容 16） |
| D13 | 默认图全显式 | 默认拓扑（每轨 `声源→延迟→通道条→主输出` + MIDI 自连）全部以 `nodes + cables` 写入项目文档；加载器**纯按文档重建**引擎图，`GraphCompiler::default_graph` 退役为"新建项目生成初始文档"的参考实现；文档与引擎一张皮 |
| D14 | 单输入语义（修订 D4，2026-10-09） | 音频节点**不允许多入边隐式加合**：每个音频入口 ≤1 条入边（乐器 0 条）。需要混合必须显式经 Mixer 节点（模块合成器式显式混音，消除不可见的隐式求和）。compile 期按节点类型校验，违反 → 拒绝连线 |
| D15 | Mixer 节点 | 新节点类型吸收 ChannelStrip（strip 退役）：**输入通道数可变 2..32**（D21），每通道独立 **gain / pan / mute / solo**（虚拟通道条，2026-10-09 评审升级：弱化传统 DAW 轨道管理，按舞台调音台方式管理 solo/mute/pan）；输出级 pan/mute（主控段）。**solo 语义**：同一 mixer 内任一通道 solo 时，未 solo 通道静音（solo 作用域 = 单台 mixer，级联 mixer 各自独立） |
| D16 | 主输出单输入 | master 同样遵守单输入（≤1 条音频入边）；多路汇入必须经 mixer（可级联）进 master。MasterGain 参数保留 |
| D17 | 连线↔输入口绑定 | `CableDoc` 增 `in_port: u8`：mixer 目标的输入口下标（唯一、< 当前口数），其余节点恒 0。per-input 推子与线的绑定关系随文件持久化，删线不错位。UI 连线到 mixer 时自动落第一个空口 |
| D18 | 默认图拓扑（修订 D13） | 默认图改为：每轨 `声源→延迟→mixer.in[i]` + **单个全局 Mixer（通道数 = max(轨数, 2)）→ master**。轨寻址与 metering 点随之调整（见 P5f） |
| D19 | 旁路语义修订 | `remove_node` 旁路重连仅适用于 ≤1 入边的节点（效果器 1入1出照旧旁通）；mixer/master 不可旁路（多入边无处重连）——删除 mixer 时其触及连线直接移除 |
| D20 | 入边权重与调参解耦 | per-channel 有效权重（gain × pan 声像系数 × mute × solo）以**立体声 L/R 权重表**参与求和：`Processor` trait 增带默认实现的 `input_weights() -> Option<&[[f32; 2]; MAX_IN_EDGES]>`（默认 `None` = 单位权重，插件零改动），`MixerProcessor` 覆写返回按通道状态实时计算的权重表；求和循环逐边乘权。**推子/pan/mute/solo 拖动走参数命令路径，永不触发拓扑重建**（守 D5） |
| D21 | 通道数管理（2026-10-09 评审拍板） | **手动增减**：mixer 面板提供添加/删除通道按钮，下限 2 无法删、上限 32 无法加。通道设置（fader/pan/mute/solo）**与插线状态解耦**——拔线不丢设置、常驻通道可预先留空。连线可落到**任意空通道**（不要求顺序占口）。否决"入口数 = 入边数 + 1 自动增减"方案：拆线丢状态 + 口位重排/留洞语义复杂 + 自动化需要稳定端口身份 |

## 3. 节点类型与端口剖面

| 节点类型 | 音频入 | 音频出 | 事件/参数口 | 可删除 | 说明 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 乐器（内置合成器 / CLAP / VST3 乐器） | 0 | 1 | MIDI 入 | ✓ | 只能做音频源头 |
| 效果器（内置 / CLAP / VST3 效果） | 1 | 1 | 参数入 | ✓ | 链式串联 |
| Mixer | 2..32（通道数按钮增减，D21；每通道 gain/pan/mute/solo） | 1 | 参数入（通道自动化后续版本） | ✓ | 显式混合点（D15）；吸收原 ChannelStrip；输出级 pan/mute |
| 主输出 | 1（单输入，D16） | 1（→硬件） | MasterGain 参数 | ✗ 固定 | 拓扑排序的汇点 |
| 时间轴 | 0 | 0 | MIDI 出 ×n + 自动化出 ×n | ✗ 固定 | 见 §6 |

约束（D14/D15/D17）：非 mixer 节点 ≤1 条音频入边、mixer ≤32 且 `in_port` 唯一，compile 期校验拒绝；节点总出边不设限（一条输出可拉多根线，读同一缓冲）。定容调整：`MAX_IN_EDGES` 8→32（仅 mixer 用得到）、`MAX_CABLES` 128→256（一个 32 通道 mixer 即占 32 条线）。最大并发音频缓冲不变（64 × 2ch × 128 × 4B = 64 KB）。混合开销：32 通道 mixer 每块 32 × 128 × 2 ≈ 8K 次 FMA ≈ 8µs，48kHz 下 <0.5% 单核，可忽略。

## 4. 引擎侧数据结构（`glitcherz-engine::realtime::graph`，新模块）

```rust
pub const MAX_NODES: usize = 64;
pub const MAX_CABLES: usize = 256;     // D17：32 口 mixer 单节点即占 32 线
pub const MAX_IN_EDGES: usize = 32;    // D14/D15：仅 mixer 可达；其余节点 ≤1（compile 校验）
pub const MAX_PENDING_EVENTS: usize = 32;   // 沿用 ClapNode 模式

#[derive(Clone, Copy, PartialEq)]
pub enum CableKind { Audio, Midi, Automation }

/// 节点运行时槽位。processor 语义不变：就地读写 output 缓冲。
pub struct NodeSlot {
    node_id: u64,                      // 控制侧单调分配，跨计划稳定
    processor: Box<dyn Processor>,     // 现有 trait 零改动；CLAP/VST3 节点直接入列
    output: AudioBuffer,               // 立体声 128 帧，预分配
    in_edges: [u16; MAX_IN_EDGES],     // 入边源节点槽位下标（Audio 线）
    in_edge_count: usize,
    notes: NoteEventSlot,              // 块边界投递的 MIDI 事件（定容）
    params: ParamEventSlot,            // 块边界投递的参数事件（自动化线）
}

/// 预编译执行计划：音频线程每块只按此遍历，无任何搜索/分配。
pub struct ExecPlan {
    order: [u16; MAX_NODES],           // 拓扑序的槽位下标（D10：无环才成立）
    order_len: usize,
}

/// 整图运行时。结构变更经队列整体换入（指针移动，零重分配）。
pub struct RtGraph {
    slots: [Option<NodeSlot>; MAX_NODES],
    plan: ExecPlan,
    master_slot: u16,                  // 主输出固定槽位（D6）
}
```

**每块执行序**（`BlockRunner` 内，替换现 `Mixer::process_block`）：

1. 块边界：消费命令队列与结构更新队列（现有协议扩展，见 §5）；
2. 时间轴调度：Sequencer 窗口扫描出事件，按 **MIDI 连线表**投递到目标节点的 `notes` 槽（现 `(track, event)` 定向的推广）；
3. 自动化求值：仅播放中，每条自动化轨道按其 `loop_ticks` 取模采样，按 **Automation 连线表**投递 `(node_id, param, value)` 到目标 `params` 槽；
4. 按 `plan.order` 遍历：`output.clear()` → 各入边源 `output` **乘权累加**（D14：非 mixer 至多 1 条；mixer 逐边乘 `input_weights()` 的 L/R 权重，D20）→ 处理 `notes`/`params` 槽事件 → `processor.process(ctx, &mut output)`；
5. 主输出槽的 `output` 即交错写出源（现 `master_channel` 的等价物）；
6. Feedback 回传：电平表按 `node_id` 索引（预分配 64 原子数组），替换现 `track_levels[4]`。

**`Processor` trait 仅增一个带默认实现的方法（D20）**：`input_weights()` 默认返回 `None`（= 单位权重），所有既有内置节点与 CLAP/VST3 节点零改动；仅 `MixerProcessor` 覆写。source 型节点（乐器）清零后写入；effect 型节点先按权求和入边再原地处理——与现语义一致。

## 5. 结构更新协议（D5 展开）

控制侧持有 `GraphDoc`（编辑态），拓扑变更时：

1. 校验：连线类型匹配、容量上限、**拓扑排序（有环拒绝，D10）**；任一失败 → UI 层拒绝编辑，引擎无感；
2. 差异计算：新图节点集合 vs 当前图节点集合 → 复用集合（`Box` 指针直接搬入新图）+ 移除集合（连同 `Box` 走回收路径）；
3. 编译 `ExecPlan`（拓扑序 + 入边表）；
4. 新增 `StructureUpdate::SetGraph { graph: RtGraph, removed: [RecycledNode; k] }` 经结构队列下发（吸收现有 `VoiceUpdate` 四变体——换装/插效果器都退化为"改图"）；队列满 → 放弃本次更新（UI 重试），实时线程永不 drop 堆对象（沿用现铁律）；
5. 音频线程块边界整体换入 `RtGraph`，把 `removed` 逐个塞回收回执队列；控制线程 `reclaim_nodes()` 排空释放，UI 按 `node_id` 丢弃配对守卫（现有 `voice_guards` 机制不变）。

**参数路径不变**：`SetParam`/`SetTrackParam` 改为按 `node_id` 定向，走命令队列块边界消费；自动化逐块直写。两者与结构更新彻底解耦——**调参永不重建**（回应 Q2 性能疑虑）。

## 6. 时间轴节点（D7 展开）

- 固定节点，不参与音频缓冲；拥有两类端口组：
  - **MIDI 轨道**（现步进模式 + 钢琴卷帘片段的能力合并为轨道实体）：每轨一个 MIDI 出口，可扇出连多个乐器节点；
  - **自动化轨道**：每轨一个自动化出口，连线终点必须指定 `(node_id, param)`；曲线 = 纯锚点列表（`AutoPoint{tick, value}` 有序列表，线性插值语义不变），**不再内嵌目标**；
- 自动化轨道自带 `loop_ticks`（默认一小节，384 tick），与 MIDI 轨道的循环解耦（现 `Track.automations` 随源循环的语义废止）；
- Sequencer 内核（纯窗口扫描、采样时钟、BPM 语义）**零改动**，只把投递目标从 `track: usize` 换成查 MIDI 连线表。

## 7. Model v2（`glitcherz-model`，D8）

```rust
pub struct Project {                       // v2，PROJECT_VERSION = 2
    pub metadata: Metadata,
    pub bpm: f64,
    pub gate: f32,
    pub midi_tracks: Vec<MidiTrack>,       // 步进模式 / 钢琴卷帘（现 Track 的音符面）
    pub automation_tracks: Vec<AutomationTrack>,  // 纯锚点 + loop_ticks
    pub graph: GraphDoc,
}

pub struct GraphDoc {
    pub nodes: Vec<NodeDoc>,               // Vec + 容量校验 64（保存/加载时校验）
    pub cables: Vec<CableDoc>,             // 容量校验 256
}

pub enum NodeDoc {
    Instrument { node_id: u64, voice: VoiceSpec, params: ... },
    Effect { node_id: u64, voice: VoiceSpec, params: ... },
    Mixer {                               // D15/D21：吸收 ChannelStrip
        node_id: u64,
        channels: Vec<MixerChannel>,      // 通道数 2..32，按钮增减；与插线状态解耦
        pan: f32,                         // 输出级声像（主控段）
        muted: bool,                      // 输出级静音（主控段）
    },
    // Master / Timeline 固定存在，不入 Vec（由加载器隐式补齐）
}

pub struct MixerChannel {                 // 虚拟通道条（舞台调音台语义）
    pub gain: f32,                        // 通道推子
    pub pan: f32,                         // 通道声像
    pub muted: bool,
    pub solo: bool,                       // 同 mixer 内任一 solo → 未 solo 通道静音
}

pub struct CableDoc {
    pub kind: CableKind,
    pub from: PortRef,                     // (node_id, port)
    pub to: PortRef,                       // Automation 线的 port 含 (node_id, param)
    pub in_port: u8,                       // D17：mixer 目标的通道下标（唯一、< channels.len()）；其余恒 0
}
```

`VoiceSpec`（`Builtin | Clap | Vst3`）原样复用。serde 全程 `#[serde(default)]`；定容上限在 `load` 时校验，超限报错而非截断（编辑态已有 UI 防御，文件层是最后防线）。

**文档校验（D14/D17，load 与保存双端）**：非 mixer 节点至多 1 条音频入边；mixer 的音频入边 `in_port` 唯一且 < `channels.len()`；`2 ≤ channels.len() ≤ 32`。`PROJECT_VERSION = 4`（v3 自动迁移，见 §10 Q-B 决议）。

## 8. 实时安全性检查单

- [ ] `RtGraph`/`ExecPlan`/所有缓冲：流启动前预分配；运行期结构变更只做指针移动与数组拷贝；
- [ ] 求和遍历、事件槽消费：无分配、无锁、无 syscall、无 panic（回收队列满放弃而非 drop）；
- [ ] 拓扑排序、容量/环路校验：仅控制线程；
- [ ] 电平回传：`[AtomicF32; MAX_NODES]` 定容数组，按槽位写、按 `node_id` 查；
- [ ] CLAP/VST3 节点：现有事件槽与守卫配对协议原样生效。

## 9. 分阶段实施（D9：P1+P2 连做）

- [x] **P1+P2 图核心 + 替换 Mixer**：`graph` 模块（NodeSlot/ExecPlan/RtGraph/求和遍历/事件槽）+ `SetGraph` 结构协议 + 差异回收 + ChannelStrip/主输出真节点；**一步替换 `[TrackStrip; 4]`**。验收：现 Mixer 全部数学测试以"默认图（复刻旧拓扑）"形式通过；新增图专项测试（多入边求和、悬空执行、播放中换图、回收计数、有环拒绝）。✅ 2026-10-08
- [x] **P3 时间轴入图**：时间轴节点 + MIDI 连线表投递 + 自动化轨道（无目标曲线）+ Automation 连线投递。验收：现有音序/自动化端到端测试经连线路径通过。✅ 2026-10-08 —— 落地为 `ExecPlan::midi_targets`（每 MIDI 轨扇出 ≤4）+ `ExecPlan::auto_routes`（终点 = (槽位, 参数)，编译期经 `route_slot` 由 `(track, param)` 解析）；`RtAutomation` 退化为纯锚点，新增 `RtAutoTrack{loop_ticks=384, curve}` 独立实体；`GraphCompiler::connect_midi/disconnect_midi/set_auto_route/clear_auto_route`；结构队列扩容 8→64（项目加载一次性下发全部连线）。
- [x] **P4 model v2**：`Project` v2 + 保存/加载 + `node_id` 稳定性（编辑会话内单调，不跨文件持久复用）。验收：往返保存加载图结构无损。✅ 2026-10-08 —— **过渡形态落地**（完整 §7 形态随 P5 `node_id` 端口重构展开）：`PROJECT_VERSION = 2`，不做 v1 读路径（Q5；serde 字段级默认值自然容忍缺失字段）；`Project.graph: GraphDoc` 只序列化当下不可推导的图状态（`midi_links` 跨轨扇出，上限 16，加载期校验越界/自连/超容**报错不截断**）；自动化连线终点 `(track, param)` 可由曲线推导，不重复存储；`Automation::loop_ticks` 显式携带（默认一小节），泳道显示口径同步回写文档。`node_id` 已是会话内单调、不持久化，无需改动。
- [x] **P5a CLAP 参数枚举**（自动化线的参数端前置项）：`ClapInstance::list_params()` 经 clack 0.2 `raw_instance()` vtable 直调实现（`PluginParams` 包装无读取方法，`glitcherz-clap-host/src/raw.rs` 为唯一 unsafe 边界）；UI 自动化选择器合并 CLAP/VST3 参数列表；测试插件补 `Amplitude` 参数端到端验证。✅ 2026-10-08
- [x] **P5b 引擎寻址层（node_id 端口 + 轨道定容 16）**：① `ExecPlan` 增 slot→node_id 表与 id→slot 解析，`RtGraph`/引擎公开 API 从轨道坐标升为 node_id 寻址；② 自动化连线终点与 MIDI 扇出目标显式 node_id 化（取代 `route_slot` 按参数种类猜终点的过渡语义）；③ 编译器出边单值 `out_edge` → 定容 cable 列表（§3"出边不设限"落地：一个输出口拉多根线；快照回滚的全 Copy 约束保持）；④ `NUM_TRACKS = 4` → `MAX_MIDI_TRACKS = 16` 定容 + 活跃轨数控制侧下发（D12：音序器 patterns/clips/pending_offs、路由表、电平表全跟进）。UI 仅最小跟进（现有面板继续工作，内部改走 node_id）。验收：默认拓扑下现有测试全绿；新增 node_id 寻址、多出边扇出、动态轨数专项测试。✅ 2026-10-08 —— `set_node_param` / `set_auto_route(auto_index, node_id, param)` / `connect_midi_node`；cable 列表上限 128（去重、多出边扇出）；`add_midi_track`/`remove_midi_track` 运行期增拆轨；修复 `insert_effect` 漏 occupied 标记与 `clear_effects` 悬空入边导致的 Kahn 误报 Cycle。
- [x] **P5c 模型完整形态（GraphDoc v2 终态）**：`GraphDoc{nodes: Vec<NodeDoc>, cables: Vec<CableDoc>}`（`NodeDoc::{Instrument, Effect, ChannelStrip}` 携带 node_id 与参数，D11 持久化；`CableDoc{kind, from: PortRef, to: PortRef}`，Timeline/Master 以保留身份寻址）；`Track` 拆 `MidiTrack`（音符面）+ 节点文档（voice → Instrument、gain/pan/mute → ChannelStrip、delay → Effect）；`automation_tracks: Vec<AutomationTrack>` 顶层化（D12）；加载器**纯按文档重建**引擎图（D13 全显式，`default_graph` 退役为新建项目初始文档生成器）；UI 面板改 Vec 遍历与节点寻址。验收：往返无损；默认项目文档可完整重建默认图（逐步建图 API：add_node/connect 粒度）。✅ 2026-10-08 —— `PROJECT_VERSION = 3`；`PortRef::{Node, Master, MidiTrack, AutoTrack}`，`MASTER_NODE_ID = 1`（文档节点从 2 起）；引擎新增 `empty_graph`（仅主输出）+ 文档建图 API `add_node`/`register_track_chain`/`connect_nodes`/`replace_node`（保 id 原位换装）/`insert_node_between`/`remove_node`（**旁路语义**：入边源×出边目标重新直连，效果器移除即旁通）；结构队列 64→256（最坏 64 节点 + 128 连线）。**双身份约定**（P5d 画布须知）：文档 node_id 是持久化身份，引擎侧身份为会话内单调 `fresh_node_id`，UI 持 doc→engine 映射（`node_map`）——会话内不复用已回收 id，插件守卫配对无歧义（被换下守卫入退役区，回收回执到达后释放）。UI 重构：`Track` 拆分后全部面板改走 `midi_tracks` Vec + 图节点字段（synth/delay/mixer 三类数据源），自动化区改自动化轨道列表（新增/删除/选终点），`lane::show` 增 `param` 参数（轴映射来自连线终点）。
- [x] **P5d UI 节点画布**：egui 自绘（平移/缩放/拉线/类型着色/删除联动），现有混音器面板逐步让位。前置依赖：P5b 寻址层 + P5a 参数枚举（均就绪后画布只需面对 node_id + cables 数据模型）。✅ 2026-10-08 —— `glitcherz-ui/src/canvas.rs`：画布只做视图 + 交互意图（`CanvasAction` 枚举返回，`GlitcherZApp::apply_canvas_actions` 统一落文档 + 引擎，画布不触碰引擎/守卫）；平移（拖背景）+ 缩放（Ctrl+滚轮/按钮，指针锚定）；类型着色（乐器/效果器/通道条/主输出/MIDI 源/自动化源）；连线贝塞尔分色绘制 + 自动化线标注终点参数；拉线经端口类型规则校验（音频→效果器/通道条/主输出、MIDI→乐器、自动化→乐器/通道条），自动化拉线弹参数菜单（复用 CLAP/VST3 参数枚举）；Delete 删除连线与效果器节点（引擎旁路语义，文档镜像直连对）；点节点联动主编辑轨道。节点位置会话内存（轨道链自动布点 + 拖拽），不持久化。
- [x] **P5f 单输入语义 + Mixer 节点（D14–D21，2026-10-09 spec v2 修订，评审已定）**（✅ 2026-10-09 落地，commit `b34d0cc`，①–④ 一次交付；实现要点沉淀于 `implementation.md` P5f 节）：
  - ① 引擎：`MixerProcessor`（通道状态表 [MixerChannel; 32] + 输出 pan/mute；覆写 `input_weights()` 返回按通道状态算出的 L/R 权重表，求和循环逐边乘权，D20）+ `Processor` trait 增默认实现方法（插件零改动）+ compile 按节点类型校验入边（非 mixer ≤1、mixer ≤32 且 in_port 唯一）+ 常量扩容（`MAX_IN_EDGES` 32 / `MAX_CABLES` 256）+ 旁路语义修订（D19：≤1 入边才旁路，mixer/master 删除即弃线）+ `ParamId::{MixerGain(u8), MixerPan(u8), MixerMute(u8), MixerSolo(u8), MixerOutPan, MixerOutMute}`（手动控制先行，自动化后续）；
  - ② model v4：`NodeDoc::Mixer { channels: Vec<MixerChannel{gain,pan,muted,solo}>, pan, muted }` 替代 ChannelStrip、`CableDoc.in_port`、`default_graph_doc` 改全局 mixer 拓扑（D18：每轨 `声源→延迟→mixer.in[i]`，mixer→master）、加载校验（§7）+ v3 自动迁移（Q-B 决议）；
  - ③ UI：mixer 节点绘制 N 个输入口（节点盒随通道数伸缩）+ 按口命中与连线落口（**自动选第一个空通道，任意空口可落**，D21）+ mixer 面板（每通道推子/pan/mute/solo + 输出 pan/mute 主控段 + 通道添加/删除按钮，2/32 上下限禁用）；
  - ④ 轨寻址与 metering 跟进：`register_track_chain` 的 channel 槽位语义退役，手动 TrackGain/TrackPan/TrackMute 改寻 mixer 通道参数（轨 → (mixer, in_port) 映射由文档连线推导）；每轨电平表点改为轨道末端节点（delay/voice），mixer 自身电平按节点回传；
  - 验收：单输入规则引擎级拒绝测试（含 master）；32 通道 mixer 加权求和数学测试（含 pan 声像系数、mute/solo 逻辑）；推子/pan/mute/solo 拖动不触发重建（参数路径断言）；v3 文件迁移往返；默认图端到端；P5e/P5e.1/P5e.2 画布交互回归。
- [x] **P6 反馈环路**（2026-10-21 落地）：显式 Feedback 节点（`NodeKind::Feedback` + `FeedbackProcessor` + `NodeDoc::Feedback{gain}`）。实现要点——编译期把「入 Feedback 的连线」的排序约束**反转**（Feedback 先于抽头源运行，运行时读到的就是上一块的输出，1 块 ≈ 128 样本 ≈ 2.67 ms，**不需要额外延迟缓冲**）；节点自身只做 `gain` 缩放（0..1，默认 0.5，防止裸环发散）。无环的 Feedback 节点同样延迟一块（语义统一）。自环 / 不含 Feedback 的环仍报 `Cycle`；删除 Feedback 节点不走旁路（否则会把环重连成非法直连），其连线直接丢弃。

## 10. 开放问题

**P5f 评审项决议（2026-10-09 拍板，已并入 D14–D21 正文）**：

- **Q-A（已决）**：弱化传统 DAW 轨道管理，按舞台调音台方式管理 solo/mute/pan——per-channel `gain/pan/mute/solo`（D15 升级）；用户仍可每轨道对应一个 mixer 通道（默认图 D18 即如此）。
- **Q-B（已决）**：strip 确认退役，多通道 mixer 直连 master。**v3 → v4 采用加载期自动迁移**（ChannelStrip → 2 通道 Mixer，原 gain/pan/mute 进通道 0；master 入边 >1 时插入一个合成 Mixer 承接全部原 master 入边）——迁移逻辑确定可测，项目文件尚少。
- **Q-C（已决）**：通道数管理采用**方案一：面板按钮手动增减**（D21），下限 2 无法删、上限 32 无法加；通道设置与插线状态解耦，拔线不丢；连线可落任意空通道。否决"入口数 = 入边数 + 1 自动增减"（拆线丢状态、口位重排/留洞语义复杂、自动化需稳定端口身份）。

**遗留（不阻塞）**：

- 单声道/多声道连线的降混规则（现状全立体声，暂不处理）；
- mixer 通道的自动化轨道支持（`MixerGain(u8)` 等载荷参数 + 自动化参数菜单动态枚举所选 mixer 的各通道）——后续版本；
- mixer 逐通道电平表（v1 电平点 = 轨道末端节点；通道表后续按需求议）；
- `MixerLevels` → 节点电平的 UI 呈现（画布上节点发光 vs 保留电平条）。
