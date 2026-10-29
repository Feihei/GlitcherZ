# MIDI 事件处理节点 Spec

> 状态：草案（2026-10-29）。**M1 + M2 已实施（2026-10-29）**，M3–M5 待做。
> 上游：`node-graph.md`（D3 连线三类型、D5 结构更新、D9 实施策略）、`control-tracks.md`（D3 `midi_cc` / D4 去重、`AutoDest::PitchBend`）、`roll-multiclip.md` D9–D11（`PitchOffset` 逐样本平滑）。

## 1. 背景与问题

现有图只有一条数据通路：**音频缓冲**。MIDI 事件由时间轴 / 键盘在块边界直接投递到处理器方法（`graph.rs::track_note` → `slot_note`），节点之间没有事件线；自动化也只有「时间轴控制轨 → 节点」单向（`auto_routes`）。因此：

1. 无法插入「MIDI 进、MIDI 出」的处理节点（琶音器）；
2. 无法让节点把**自己产生**的曲线/控制值送给下游（包络器、滑音器）；
3. `CableKind::Midi` 的源目前只有 `MidiTrack` / `Keyboard`，目标只允许 `Instrument`。

本 spec 补的是一条**块内事件 / 控制总线**，以及三个建立在其上的节点。算法本身很小，成本集中在端口模型与转发路径。

## 2. 目标与非目标

**目标**

- 新增节点族：**MIDI 效果器**（1 MIDI in / 1 MIDI out）与**控制源**（1 MIDI in / 1 Control out）。
- 首批节点：`Arpeggiator`（琶音器）、`Portamento`（滑音器）、`MidiEnvelope`（包络器）。
- 节点间事件与控制线参与拓扑排序，**同块内传播**（不额外延迟一个音频块）。
- CLAP/VST3 处理器零改动：新增的 `Processor` 方法全部带默认实现。

**非目标**

- 采样级事件偏移：仍维持块粒度（`node-graph.md` §1 非目标）。48 kHz / 128 帧 ≈ 2.7 ms。
- CLAP / VST3 的 **note-effect 插件**宿主（本 spec 只做内置节点；1 MIDI in/out 剖面为其留位）。
- MPE / MIDI 2.0。
- 音频反馈之外的「事件反馈」：事件/控制环一律拒绝。

## 3. 节点端口剖面

| 节点类型 | MIDI 入 | MIDI 出 | Control 出 | Audio 入/出 | 音频输出 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| `Arpeggiator` | 1 | 1 | — | 0 / 0 | 恒静音 |
| `Portamento` | 1 | 1（透传 note） | 1（音高偏移，半音） | 0 / 0 | 恒静音 |
| `MidiEnvelope` | 1 | — | 1（周期值） | 0 / 0 | 恒静音 |
| 乐器（现状） | 1 | — | — | 0 / 1 | 声源 |
| 效果器 / Mixer / Master | — | — | — | 现状 | 现状 |

**端口身份（D1）**：不引入端口号。`CableKind` 已经区分类型，规定**每种 `CableKind` 在每个节点上同名端口各 ≤1**（MIDI 入 1、MIDI 出 1、Control 出 1），扇出（多条出线）不受限。`CableDoc.in_port` 继续只表示音频目标的 mixer 通道，MIDI / Control 线恒 0，非 0 在校验期拒绝。

MIDI 效果器与控制源没有音频端口：图仍会调用它们的 `process`（缓冲已清零），实现必须只返回、不写音频。

## 4. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| D1 | **两种节点族，不改造 `PortRef`** | `PortRef::Node(id)` 对 MIDI / Control 线不需要端口号，靠 `CableKind` + 「每节点每种端口 ≤1」定位。零迁移、零端口结构改造。 |
| D2 | **`Processor::process_events` 是新通道，`process` 签名不动** | `fn process_events(&mut self, ctx: &ProcessContext, out: &mut NodeEvents)`，默认空实现。事件节点覆写它产出本块事件；乐器/效果器/插件零改动。不复用 `NoteEvent` 通道（输出与输入语义相反，且需要承载 CC/bend）。 |
| D3 | **`NodeEvents` 是定容总线** | `events: [Option<RtEvent>; MAX_NODE_EVENTS]`（32，与插件 pending 队列同量级）+ `len`，加 `control: Option<f32>`（v1 每节点一个控制出口）。溢出时**丢弃新增事件**（保留已入队），不 panic。`RtEvent::{Note(NoteEvent), Ctrl(RtCtrl)}` 直接复用现有类型。 |
| D4 | **事件/控制边进入 Kahn 排序** | 新增独立 `EventCable { from, to, kind: Midi/Control, dest: AutoDest }` 表（定容 128）。`GraphCompiler::compile` 先按现状算音频 in-edge，再把事件边叠加进 `degree` 与出边遍历，保证上游 MIDI 效果器先于下游乐器执行 → 同块传播。事件环（无 `Feedback` 豁免）报 `Cycle`。 |
| D5 | **转发发生在遍历中，而不是块边界队列** | `RtGraph::process_block` 处理完一个槽位后，立即把它的 `NodeEvents` 转发给连带目标：Note → `note_event`，CC / bend → `midi_cc` / `midi_pitch_bend`。目标槽位此时还没被 `take`，可直接可变借用；不引入第二遍扫描或额外缓冲。 |
| D6 | **Control 边的 dest 复用 `AutoDest`** | Control 线在 `CableDoc.param` 中携带 `AutoParam`，编译期解析为 `AutoDest::{Param, Cc, PitchBend}`；投递逻辑与 `auto_deliver` 共用（抽 `deliver_dest(slot, dest, value)`）。去重（CC / bend 变化才发）由目标槽位的独立记忆表负责，结构更新时清空。 |
| D7 | **控制源自己给刻度，引擎不猜参数轴** | `MidiEnvelope` 与 `Portamento` 各有 `min` / `max` 参数（默认见 §5），输出值 = `min + (max-min) × t`。CC 目标请设 `0..127`，音高目标设半音范围。引擎不读取 UI 的参数轴（`node_param_axis`），避免把展示层依赖拉进 RT 侧。 |
| D8 | **不做 `CableKind` 新枚举值** | MIDI 线仍是 `CableKind::Midi`、控制线仍是 `CableKind::Automation`，只是把 `from` 放宽到节点。新枚举会复制拖线、删除、单源、校验、着色五处逻辑，换不来表达力。 |
| D9 | **删除节点时事件线直接丢弃，不旁路** | `Portamento` / `Arpeggiator` / `MidiEnvelope` 的入射与出射事件/控制线随节点删除一起移除。理由与 D19 的 mixer/master 相同：事件流被改写后「直连」在语义上不成立（arp 的 note 与原始 note 不是同一序列）。 |
| D10 | **时间轴/键盘的投递路径不变** | 「`MidiTrack` / `Keyboard` → 节点」仍走现有 `midi_targets` / `node_note`，只是目标校验从「必须是乐器」放宽到「乐器或 MIDI 效果器」。因此 arp 链首不需要新的接线协议。 |
| D11 | **`PROJECT_VERSION` 不变** | 新增 `NodeDoc` 变体与参数结构是纯加性 tag（同 E 系列 `EffectParams` 变体的先例）。旧文件读新代码无变化；不提供反向读取。 |
| D12 | **块级时钟，无采样内偏移** | arp 的 step 边界用 `ctx.song_ticks`（绝对 tick）+ `ctx.sec_per_tick`；每块最多产出一个 step 事件对。1/32 @120 BPM ≈ 62.5 ms，远大于 2.7 ms 块长，不存在一块跨多 step 的常规场景（极端 BPM 下取最后一个 step 并只发一对）。 |
| D13 | **滑音用「偏移」而不是「改写音符」** | `Portamento` 透传原始 note，另输出一条从 `delta` 收敛到 0 的半音偏移；下游按 `PitchOffset` 或 `PitchBend` 叠加。改写 note 需要知道下游音源的音域与换音策略，无法对内置合成器 / SF2 / 插件通用。 |

## 5. 节点语义与参数

### 5.1 `Arpeggiator`

| 参数 | 类型 / 范围 | 默认 | 语义 |
| :--- | :--- | :--- | :--- |
| `division` | enum（1/4、1/8、1/8T、1/8.、1/16、1/16T、1/32） | 1/16 | step 长度（tick），表独立于 `delay::DIVISION_TICKS` |
| `gate` | 0.05..1.0 | 0.5 | note on 持续占 step 的比例 |
| `mode` | enum（Up、Down、UpDown、Random） | Up | 遍历顺序；`Random` 用节点内 seeded XorShift（种子 = `node_id`），可复现 |
| `octaves` | 1..4 | 1 | 音域扩展层数 |
| `latch` | bool | false | 松开全部键后仍按最后一次和弦继续跑；新 note on 替换和弦 |

语义：

- 输入 note on 累积到**定容 held 表**（16 键 + 插入序；满则忽略新键，保持确定行为）。输入 note off 从表移除；受 `latch` 影响。
- 每块检查是否跨过 step 边界：跨过 → 先发上一 step 的 note off（若 gate 未到点），再发新 step 的 note on；否则若 `gate` 到点 → 发 off。
- 输出事件**只包含 arp 自己产生的音**；输入 note 不直接透传（否则原音与琶音同时响）。
- 收到 blanket `Off { note: 0 }`（transport stop / panic 的既有语义）→ 清空 held、终止当前 step、向出边**透传同一个 blanket off**，保证下游乐器一定静音。这是本节唯一的 off 透传路径。
- `transport_active == false` 时不产生新事件；若 step 在唱则补一个 off。

### 5.2 `Portamento`（滑音器）

| 参数 | 类型 / 范围 | 默认 | 语义 |
| :--- | :--- | :--- | :--- |
| `time` | 0..2000 ms | 120 | 偏移从初始值收敛到 0 的时间常数（指数逼近） |
| `min` / `max` | -24..24 半音 | -24 / 24 | 输出刻度（D7） |
| `legato_only` | bool | true | 仅在「上一个音仍按住」时滑音（v1 按最近 input 判定） |

语义（D13）：

- 透传所有输入 note（on/off 原样转发）到 MIDI 出。
- 收到 note on 时记录 `delta = previous_note - new_note`（半音）。输出偏移从 `delta` 出发、按 `time` 指数收敛到 0；初始值钳到 `min..max`。
- 首个 note（无 `previous_note`）偏移为 0。
- 控制出口每块输出当前偏移（半音）。用户把 Control 线接到 `PitchOffset`（内置合成器，逐样本平滑，听感最好）或 `PitchBend{range}`（SF2 / CLAP / VST3 按节点自答分流，见 `roll-multiclip.md` D10）。
- 复音输入取「最近一次 note on」作为 reference；不承诺逐 voice 独立滑音（见 §9 开放问题）。

### 5.3 `MidiEnvelope`（包络器）

| 参数 | 类型 / 范围 | 默认 | 语义 |
| :--- | :--- | :--- | :--- |
| `attack` | 0..5 s | 0.01 | note on 后 0 → 1 |
| `decay` | 0..5 s | 0.2 | 1 → sustain |
| `sustain` | 0..1 | 0.7 | 按住时的电平 |
| `release` | 0..10 s | 0.4 | note off 后 → 0 |
| `min` / `max` | -127..127 | 0 / 1 | 输出刻度（D7；驱动 CC 时设 0..127） |

- 触发：note on 进入 A→D→S，note off 进入 R；blanket off 与普通 off 同义。
- 每块输出一次采样值（与 `ControlTrack` 的块级曲线同精度），可与多条 Control 线并联（一个值扇出到多个 dest）。
- 与合成器内部 ADSR 的区别：后者只缩放自身音量；这里是**可路由的控制源**，可驱动 cutoff、mixer 通道 gain、插件参数、CC 等。

## 6. 引擎侧数据结构（`glitcherz-engine`）

```rust
// realtime/events.rs（新）
pub const MAX_NODE_EVENTS: usize = 32;
pub const MAX_EVENT_FANOUT: usize = 8;   // 每节点出边；Control 需扇出到多个参数
pub const MAX_EVENT_CABLES: usize = 128;

#[derive(Clone, Copy)]
pub enum RtEvent {
    Note(NoteEvent),
    Ctrl(RtCtrl),
}

#[derive(Clone, Copy, Default)]
pub struct NodeEvents {
    events: [Option<RtEvent>; MAX_NODE_EVENTS],
    len: u8,
    pub control: Option<f32>,
}

impl NodeEvents {
    pub fn push_note(&mut self, note: NoteEvent);   // 满则丢弃
    pub fn push_ctrl(&mut self, ctrl: RtCtrl);      // 满则丢弃
    pub fn iter(&self) -> impl Iterator<Item = RtEvent> + '_;
}

// realtime/graph.rs
pub enum EventKind { Midi, Control }

#[derive(Clone, Copy)]
pub struct EventCable {
    pub from: u16,
    pub to: u16,
    pub kind: EventKind,
    /// Ignored for `Midi`; the compiled destination for `Control`.
    pub dest: AutoDest,
}

pub struct ExecPlan {
    // ...现状字段...
    /// 每槽位的出射事件边（转发用；入度只在编译期临时计算，不入计划）。
    pub event_out: [[EventCable; MAX_EVENT_FANOUT]; MAX_NODES],
    pub event_out_counts: [u8; MAX_NODES],
}
```

`NodeSlot` 增 `events: NodeEvents`（每块遍历前 `len = 0, control = None`）。

`GraphCompiler` 增 `connect_event` / `disconnect_event`；`compile` 把事件边同时计入排序（D4）。

**每块顺序**（`RtGraph::process_block` 内，替换现状 5 步）：

1. `drain_panic_sweep()`（现状）；
2. 按 `plan.order` 遍历；对每个槽位：
   - 清空音频缓冲并按现有规则求和；
   - 清空 `events`；
   - `processor.process(ctx, &mut output)`（事件节点只返回）；
   - `processor.process_events(ctx, &mut events)`；
   - 按 `event_out` 转发事件（Note / CC / bend）与控制值（`deliver_dest`），随后放回槽位；
3. 电平采样（现状）。

## 7. 模型（`glitcherz-model`）

```rust
pub enum NodeDoc {
    // ...现状...
    Arpeggiator { node_id: u64, track: usize, params: ArpeggiatorParams },
    Portamento  { node_id: u64, track: usize, params: PortamentoParams },
    MidiEnvelope{ node_id: u64, track: usize, params: MidiEnvelopeParams },
}
```

`ArpeggiatorParams { division: u8, gate: f32, mode: ArpMode, octaves: u8, latch: bool }`
`PortamentoParams { time_ms: f32, min: f32, max: f32, legato_only: bool }`
`MidiEnvelopeParams { attack: f32, decay: f32, sustain: f32, release: f32, min: f32, max: f32 }`

全部 `#[serde(default)]`。数值在加载期钳制（`division` 越界回 1/16，`gate` 0.05..1，`octaves` 1..4，时间非有限值回默认）。

**校验（`GraphDoc::validate`）**

| 规则 | 说明 |
| :--- | :--- |
| MIDI 线源 | `MidiTrack` / `Keyboard` / MIDI 效果器（`Arpeggiator`、`Portamento`） |
| MIDI 线目标 | 乐器 / MIDI 效果器 |
| MIDI 入边 ≤1 | 对同一目标节点：时间轴/键盘连接数 + 节点事件入边 ≤1（含 `Arpeggiator` 等），否则拒绝（多源事件流无合并语义） |
| Control 线源 | `AutoTrack` / `Portamento` / `MidiEnvelope` |
| Control 线目标 | 乐器 / 效果器 / Mixer / Feedback（控制源自身、MIDI 效果器、键盘、音频轨除外） |
| Control 入边 ≤1 | 每目标节点至多一条（v1 不定义多源合并） |
| Control + Tempo | `param == Tempo` 只能落在 `TimelineIn`，源为节点时禁止 |
| `in_port` | MIDI / Control 线必须为 0 |
| 节点能力 | 三种新节点（`Arpeggiator` / `Portamento` / `MidiEnvelope`）没有音频端口；音频线不得以它们为源或目标 |

## 8. UI（`glitcherz-ui`）

- `patch.rs`：三种节点的绘制盒与类型着色（MIDI FX 用与 MIDI 线一致的绿色；Control 出口沿用 automation 黄色）；Add 菜单新增 **MIDI FX** 分组。
- `accepts()`：按 §7 规则放宽；拉线时只有 `kind` 匹配的端口可落。Control 线拉线仍弹 dest 菜单（复用现有 automation 菜单），但源为节点时隐藏 `Tempo`。
- Inspector / 节点卡：三个节点的参数面板（复用 `effect.rs` 的参数控件风格）。v1 不做节点内 inline 滑条。
- 事件线删除、单源约束、撤销与现有 `CableKind` 逻辑同路。

## 9. 分阶段实施

| 阶段 | 内容 | 验收 |
| :--- | :--- | :--- |
| **M1 事件/控制通道** | `events.rs`、`NodeSlot.events`、`ExecPlan` 事件边、`compile` 排序叠加、转发路径、`deliver_dest` 抽取、删除语义（D9） | 测试处理器（透传/倍增）端到端：上游事件同块到达下游；事件环拒绝；脏删除不误伤音频旁路；RT 审计无分配 |
| **M2 model** | 三个 `NodeDoc` 变体 + 参数 + 校验矩阵 + 加载钳制 | RON 往返；校验矩阵表逐项；`PROJECT_VERSION` 不变 |
| **M3 Arpeggiator** | 引擎 DSP（held 表、step 时钟、模式、octave、latch、blanket off 透传）+ 参数 | 120 BPM 下 1/8 每小节 8 对事件；gate 时点；四种模式序列；stop 清音 |
| **M4 MidiEnvelope + Portamento** | 两个控制源 + 控制边投递（Param / CC / bend）+ 去重 | 包络 ADSR 曲线逐块断言；滑音首音 0、跳音 ±N 收敛到 0；bend 与 `PitchOffset` 分流 |
| **M5 UI** | patch 节点/菜单/拉线/Inspector | `accepts` 矩阵测试；手动跑通 arp → synth 与 envelope → cutoff |

## 10. 被否决的备选

| 方案 | 否决理由 |
| :--- | :--- |
| 在 UI 线程做琶音器 | 时序由帧率（≈60 Hz）决定，卡顿直接变成节奏错误；且与音序器的音频线程时钟不同源。 |
| 把事件编码进 `AudioBuffer` | 复用缓冲省不了任何结构，却让「音频采样」与「事件」两种语义混在一个类型里，所有效果器被迫面对垃圾数据。 |
| 给 `PortRef` 加 `(node_id, port)` 二元组 | v1 每类型只有一个端口，端口号不携带信息；等 note-effect 插件（多端口）落地再改，届时一次性迁移比长期维护空字段便宜。 |
| 每个节点一个 `Vec<RtEvent>` | 音频线程分配/释放，直接违反实时铁律 1。 |
| 事件线延迟一个块（只按音频序转发） | 2.7 ms 延迟会让 arp 与同一轨的其它声源错位，且引入「事件时序取决于图遍历顺序」的隐性规则；排序叠加只多几行代码。 |
| 包络器直接发 CC 事件 | 值域被锁死在 0..127，驱动参数还要二次换算；`min/max` 刻度 + `AutoDest` 三态才是正交的。 |
| 滑音只在内置合成器里做 | 只对内置音源有效，SF2 / 插件拿不到；节点化后由节点自答分流（bend 或 `PitchOffset`）覆盖全部声源。 |
| 采样级事件偏移 | 需要逐采样事件队列与排序，收益不可辨（`node-graph.md` §1 已列为非目标）。 |

## 11. 开放问题（不在本期）

- **复音滑音**：当前取最近 note on 作 reference；逐 voice 独立滑音需要 `PitchOffset` 逐 voice 化（合成器侧改动），另立 spec。
- **arp 的 latch 细节**：新和弦与旧和弦的重叠音、`hold` 与 `latch` 的差别，待实际使用反馈。
- **事件块内偏移**：若未来引入逐采样事件队列，`NodeEvents` 的事件需带 `frame_offset`；本 spec 的定容结构预留了扩展位置。
- **MIDI effect 插件宿主**：`EventKind::Midi` 的 1 in / 1 out 剖面已经与 CLAP note-effect 一致，后续在节点安装层接 `ClapNode` 即可。