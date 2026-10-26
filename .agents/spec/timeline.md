# 编曲时间轴（Timeline）Spec

> 状态：定稿（2026-10-08，经实测反馈 + 四项决策对齐）。
> 上游：`node-graph.md`（节点图架构，P1-P5d 已完成）。本 spec 定义真正的编曲时间轴，替换当前"每轨单循环 clip"的鼓机模型。

## 1. 背景与问题

实测反馈（2026-10-08）：

1. **Piano roll 画音符不发声**——根因：UI 从未提供 `TrackSource` 切换控件，所有轨永远停在 `Steps` 源，piano roll 编辑的 clip 被下发但调度器不看（`lib.rs` 中 `midi_tracks[].source` 无任何赋值点）。
2. **固定 4 轨**——模型/引擎已动态化（上限 16），但 UI 无加轨/删轨按钮。
3. **没有真正的时间轴**——当前是"全局一个循环位置 + 每轨一个自循环 clip"的鼓机模型：无绝对时间、无 clip 摆放、无编曲长度。

## 2. 目标形态

- 多轨 **arrangement timeline**：行 = 轨道（MIDI 轨 + control 轨，动态增删），轨上插 clip（有绝对位置与长度），双击 clip 跳转 Edit 标签页（MIDI clip → piano roll；control clip → 曲线编辑）。
- 节点图中 **timeline 作为一个无输入口、多输出口（n = MIDI 轨数 + control 轨数）的不可删除节点**，把 MIDI / control 数据流投递到 instrument / effect 节点。

## 3. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| D14 | Steps 退役 | `TrackSource::Steps` / pattern / 16-step 面板移除，统一 clip 模型。**未来特性（记录不实施）**：clip 在 timeline 上平铺——一个 clip 的内容在 span 时间段内循环，编辑 clip 即改变整段循环（类似 step 在时段内循环的体验）。为此一期就把 `span_ticks` / `loop_ticks` 语义建进 Clip 模型（调度按周期平铺），UI 一期只做单周期呈现。 |
| D15 | 引擎定容 Copy | 每轨 ≤ 8 个 clip、单 clip ≤ 64 音符（沿用 `MAX_CLIP_NOTES`）；结构命令**按轨批量**下发（`SetTrackClips` / `SetControlClips`），延续全 Copy 无堆共享的 RT 约束。超容报错不截断（与图容量同哲学）。 |
| D16 | 编曲长度内容自适应 | arrangement 末尾 = 所有 clip 结束位置向上取整到小节；播放到末尾自动停止；一期不做 loop region。安全上限 256 小节（防病态文档）。 |
| D17 | control clip 终点由连线决定 | 延续 D7：control 轨在 timeline 上只是时间容器，投递终点由节点图连线（`auto_routes`）决定；clip span 之外不投递（目标保持最后值）。删轨/改图不产生悬空绑定。 |

## 4. 数据模型（v4）

```ron
MidiTrack: ( name, clips: [MidiClip] )
MidiClip:  ( start_ticks, span_ticks, loop_ticks, notes: [Note] )   // notes 相对 clip 起点
AutomationTrack → Control 轨同构:
ControlTrack: ( name, clips: [ControlClip] )
ControlClip: ( start_ticks, span_ticks, loop_ticks, curve: [AutoPoint] )
```

- 移除：`pattern` / `source` / 单 `clip` 字段、`TrackSource`。
- 校验：每轨 clip ≤ 8、音符 ≤ 64、`span_ticks ≥ loop_ticks`、`start_ticks + span_ticks ≤ 256 小节`；越界报错不截断。
- `PROJECT_VERSION = 4`，无兼容读路径（Q5 延续）。

## 5. 引擎调度（Sequencer 重写）

- `RtMidiClip { start, span, loop_ticks, notes[64], note_count }` / `RtControlClip` 同构，定容 `[[RtMidiClip; 8]; 16]`。
- 调度：块对应的绝对 tick 窗口 `[pos, pos+block_ticks)` 内，扫描每轨 clip 列表；clip 内容按 `loop_ticks` 周期在 `[start, start+span)` 内平铺（D14 未来特性的调度语义一期即落地，UI 一期单周期呈现）；命中的音符转 `(track, NoteEvent)`，pending_offs 机制不变。
- 自动化求值：查 control clips——span 内按周期映射曲线，span 外不投递。
- 走带：位置到达 arrangement 末尾自动 stop；块边界调度精度注记延续（≤1 block jitter）。

## 6. 分阶段计划

- [x] **T0 快修**（不动数据模型）：默认音源切 `Clip`（piano roll 立即可听）；移除步进音序器 UI 区块；加/删 MIDI 轨按钮（引擎 `register_track_chain` / `remove_node` 已有，文档与引擎同步）。✅ `5915e99`
- [x] **T1 模型 v4**：`MidiClip`/`ControlClip` + 校验 + 往返测试；`PROJECT_VERSION = 4`。Steps/Pattern/TrackSource/gate 退役，`AutomationTrack` → `ControlTrack`（clips 顶层化）；UI 过渡期 piano roll / lane 暂编**首 clip**，engine 仍按首 clip 循环调度（SetSource 强制 Clip），窗口调度重写归 T2。
- [x] **T2 引擎 timeline 调度**：`RtMidiClip`/`RtControlClip` 定容 + `SetTrackClips`/`SetControlClips` 命令 + 窗口调度 + 内容自适应停止 + automation clip 求值。✅ `Sequencer` 重写为绝对时间窗口扫描（clip 平铺 `start + k·loop`、span 边界截断、arrangement end 自动停但 pending off 继续放完）；`SetPattern/SetSource/SetGate/SetClip/SetAutoTrack` 全部退役（engine 默认 clip span=0=空表）；控制轨求值改为首个覆盖当前位置的 clip。
- [x] **T3 编曲视图**：`timeline.rs`——行 = 轨、clip 块（拖动移位/拖边调长/删除）、双击跳转 Edit 标签页（piano roll / lane 复用）、轨头（增删轨/静音/选择）、标尺与小节网格、播放头。
- [x] **T4 画布 timeline 节点**：显式 Timeline 节点（无输入、n 输出口、不可删除），per-track 源小节点从画布移除；`PortRef::MidiTrack/AutoTrack` 语义不变，纯呈现层合并。✅ 画布单 Timeline 实体（端口 = MIDI 轨 + control 轨，带 T/A 标签），`timeline_port_ref` 为端口→轨映射唯一真源；`CanvasId::MidiTrack/AutoTrack` 退役。**2026-10-26 扩展**：端口序 = **MIDI → Curve → Audio**，映射唯一真源改 `timeline_port_kind`（`TimelinePort::{Midi,Auto,Audio}`）；U6 的 `NodeDoc::AudioTrack` 节点也折进 timeline node（不再单独画框，端口标签 T/C/A），见 `audio-track.md` §4 补充决策。
