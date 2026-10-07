---
title: "模块节点图架构"
summary: "ResoZ 引擎演进：可自由布线的处理图 + 时间轴信号源（Cardinal/PureData 式布线，乐队现场隐喻）"
status: 已评审定稿（2026-10-07 拷问式 Q&A 两轮）
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
| D2 | 端口剖面 | 由节点类型决定（见 §3 表），乐器 0 音频入、效果器 1入1出、通道条/求和多入边 |
| D3 | 连线三类型 | `Audio` / `Midi` / `Automation`，与端口类型绑定；类型不匹配 UI 拉线时拒绝，引擎侧零防御 |
| D4 | 多入边求和 | 一个音频入口可接多条入边，自动求和（模拟信号直觉） |
| D5 | 结构更新协议 | 拓扑变更 = **重建执行计划**（控制侧拓扑排序 + 缓冲下标分配，微秒级）+ **差异化节点回收**（新旧图都存在的节点复用，只回收被删节点）。参数/事件走命令队列与块内直写，**永不触发重建** |
| D6 | 悬空语义 | 无输入通路的节点**照常执行**（副作用——延迟线残响等——保持活跃，重新接线状态连续），但输出无人消费即听感静音；**主输出节点固定存在、不可删除、不可改类型**，cpal 回调只读它的输出缓冲 |
| D7 | 时间轴节点 | 固定存在（与主输出同级）：**MIDI 轨道**与**自动化轨道**是两类独立实体，各出一种端口；自动化曲线不再内嵌目标参数（现 `AutoParam` 职责），**目标 = 连线终点 (node_id, param)** |
| D8 | 兼容性 | 不做 v1 迁移；`Project` v2 按节点图重新设计 |
| D9 | 实施策略 | **P1+P2 连做**，不留两套引擎并存中间态 |
| D10 | 环路 | 初期禁止：拓扑排序失败（有环）→ UI 拒绝该连线；P6 再议显式反馈节点 |

## 3. 节点类型与端口剖面

| 节点类型 | 音频入 | 音频出 | 事件/参数口 | 可删除 | 说明 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 乐器（内置合成器 / CLAP / VST3 乐器） | 0 | 1 | MIDI 入 | ✓ | 只能做音频源头 |
| 效果器（内置 / CLAP / VST3 效果） | 1 | 1 | 参数入 | ✓ | 链式串联 |
| 通道条（增益/声像/静音） | 1（≤8 入边求和） | 1 | MIDI 入 + 参数入 | ✓ | 现 `TrackStrip` 的混音三项升级为真节点 |
| 求和/主输出 | 1（≤8 入边求和） | 1（→硬件） | — | ✗ 固定 | 拓扑排序的汇点 |
| 时间轴 | 0 | 0 | MIDI 出 ×n + 自动化出 ×n | ✗ 固定 | 见 §6 |

约束：入边总数 ≤8、节点总出边不设限（一条输出可拉多根线，读同一缓冲）。最大并发音频缓冲 = 每节点一个立体声 128 帧输出缓冲：64 × 2ch × 128 × 4B = **64 KB**，启动期一次预分配。

## 4. 引擎侧数据结构（`resoz-engine::realtime::graph`，新模块）

```rust
pub const MAX_NODES: usize = 64;
pub const MAX_CABLES: usize = 128;
pub const MAX_IN_EDGES: usize = 8;
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
4. 按 `plan.order` 遍历：`output.clear()` → 各入边源 `output` **累加求和**（D4）→ 处理 `notes`/`params` 槽事件 → `processor.process(ctx, &mut output)`；
5. 主输出槽的 `output` 即交错写出源（现 `master_channel` 的等价物）；
6. Feedback 回传：电平表按 `node_id` 索引（预分配 64 原子数组），替换现 `track_levels[4]`。

**`Processor` trait 不动**：source 型节点（乐器）清零后写入；effect 型节点先求和入边再原地处理——与现语义完全一致，所有内置节点与 CLAP/VST3 节点免改接入。

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

## 7. Model v2（`resoz-model`，D8）

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
    pub cables: Vec<CableDoc>,             // 容量校验 128
}

pub enum NodeDoc {
    Instrument { node_id: u64, voice: VoiceSpec, params: ... },
    Effect { node_id: u64, voice: VoiceSpec, params: ... },
    ChannelStrip { node_id: u64, gain: f32, pan: f32, muted: bool },
    // Master / Timeline 固定存在，不入 Vec（由加载器隐式补齐）
}

pub struct CableDoc {
    pub kind: CableKind,
    pub from: PortRef,                     // (node_id, port)
    pub to: PortRef,                       // Automation 线的 port 含 (node_id, param)
}
```

`VoiceSpec`（`Builtin | Clap | Vst3`）原样复用。serde 全程 `#[serde(default)]`；定容上限在 `load` 时校验，超限报错而非截断（编辑态已有 UI 防御，文件层是最后防线）。

## 8. 实时安全性检查单

- [ ] `RtGraph`/`ExecPlan`/所有缓冲：流启动前预分配；运行期结构变更只做指针移动与数组拷贝；
- [ ] 求和遍历、事件槽消费：无分配、无锁、无 syscall、无 panic（回收队列满放弃而非 drop）；
- [ ] 拓扑排序、容量/环路校验：仅控制线程；
- [ ] 电平回传：`[AtomicF32; MAX_NODES]` 定容数组，按槽位写、按 `node_id` 查；
- [ ] CLAP/VST3 节点：现有事件槽与守卫配对协议原样生效。

## 9. 分阶段实施（D9：P1+P2 连做）

- [ ] **P1+P2 图核心 + 替换 Mixer**：`graph` 模块（NodeSlot/ExecPlan/RtGraph/求和遍历/事件槽）+ `SetGraph` 结构协议 + 差异回收 + ChannelStrip/主输出真节点；**一步替换 `[TrackStrip; 4]`**。验收：现 Mixer 全部数学测试以"默认图（复刻旧拓扑）"形式通过；新增图专项测试（多入边求和、悬空执行、播放中换图、回收计数、有环拒绝）。
- [ ] **P3 时间轴入图**：时间轴节点 + MIDI 连线表投递 + 自动化轨道（无目标曲线）+ Automation 连线投递。验收：现有音序/自动化端到端测试经连线路径通过。
- [ ] **P4 model v2**：`Project` v2 + 保存/加载 + `node_id` 稳定性（编辑会话内单调，不跨文件持久复用）。验收：往返保存加载图结构无损。
- [ ] **P5 UI 节点画布**：egui 自绘（平移/缩放/拉线/类型着色/删除联动），现有混音器面板逐步让位。前置依赖：CLAP 参数枚举（遗留任务升级为前置项，自动化线需要参数列表）。
- [ ] **P6 反馈环路（可选）**：显式 1 块延迟反馈节点（128 样本 ≈ 2.7 ms），拓扑排序对其豁免。

## 10. 开放问题（实施中再定，不阻塞 P1+P2）

- 单声道/多声道连线的降混规则（现状全立体声，暂不处理）；
- ChannelStrip 之外是否需要"总线"节点（P5 后按需求议）；
- `MixerLevels` → 节点电平的 UI 呈现（画布上节点发光 vs 保留电平条）。
