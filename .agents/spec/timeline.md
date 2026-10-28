# 编曲时间轴（Timeline）Spec

> 状态：定稿（2026-10-08，经实测反馈 + 四项决策对齐）。
> 上游：`node-graph.md`（节点图架构，P1-P5d 已完成）。本 spec 定义真正的编曲时间轴，替换当前"每轨单循环 clip"的鼓机模型。

## 1. 背景与问题

实测反馈（2026-10-08）：

1. **Piano roll 画音符不发声**——根因：UI 从未提供 `TrackSource` 切换控件，所有轨永远停在 `Steps` 源，piano roll 编辑的 clip 被下发但调度器不看（`lib.rs` 中 `midi_tracks[].source` 无任何赋值点）。
2. **固定 4 轨**——模型/引擎已动态化（上限 16），但 UI 无加轨/删轨按钮。
3. **没有真正的时间轴**——当前是"全局一个循环位置 + 每轨一个自循环 clip"的鼓机模型：无绝对时间、无 clip 摆放、无编曲长度。

## 2. 目标形态

- 多轨 **arrangement timeline**：行 = 轨道（MIDI 轨 + control 轨，动态增删），轨上插 clip（有绝对位置与长度）。Edit 标签页跟随 clip 选区：选中后切到 Edit 即打开该 clip（MIDI clip → piano roll；control clip → 曲线编辑），双击是同一件事的快捷方式；没有可编辑的选中 clip 时 Edit 只给提示。
- 节点图中 **timeline 作为一个无输入口、多输出口（n = MIDI 轨数 + control 轨数）的不可删除节点**，把 MIDI / control 数据流投递到 instrument / effect 节点。

## 3. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| D14 | Steps 退役 | `TrackSource::Steps` / pattern / 16-step 面板移除，统一 clip 模型。**未来特性（记录不实施）**：clip 在 timeline 上平铺——一个 clip 的内容在 span 时间段内循环，编辑 clip 即改变整段循环（类似 step 在时段内循环的体验）。为此一期就把 `span_ticks` / `loop_ticks` 语义建进 Clip 模型（调度按周期平铺），UI 一期只做单周期呈现。 |
| D15 | clip 表走结构队列（2026-10-28 修订） | 每轨 ≤ **32** 个 clip（2026-10-28 二次修订：原 8 个是命令队列时代的副产物）；**单 clip 音符 / controller 数不再有引擎侧上限**（原 `MAX_CLIP_NOTES = 64` 是「整表塞进 `Copy` 命令槽」的副产物：真实 MIDI 一轨常 1000+ 音符，导入后直接不可保存）。修订后 note/ctrl 集合是**控制线程拥有的 `Vec`**：`MidiClipTable` 经结构队列（`GraphUpdate::SetMidiClips`）下发，音频线程 `mem::replace` 换入、把旧表推到 MIDI 垃圾队列由控制线程释放——与 audio clip 表（U6 D4）同一协议。命令队列回到小结构——control clip 表（`ControlClipTable`，定容 `Copy`、无堆所有权）也一并迁到 `GraphUpdate::SetControlClips`，纯因体积（32 × ~544 B ≈ 17 KB/槽）而非所有权；它不需要垃圾回执：音频线程直接原地覆盖旧表。文档侧 `MAX_CLIP_NOTES = 65536` 仅作病态文件防线；导入按 `smf::IMPORT_CLIP_NOTES = 4096` 分块（同 start tick 的音符不切开，块数超 `MAX_CLIPS_PER_TRACK` 时自适应放大块长）。 |
| D25 | clip 数据 vs 命令分离（2026-10-28） | 结构性大数据（MIDI clip 表、audio clip 表）一律走结构队列 + 垃圾回执；命令队列只承载小而 `Copy` 的标量命令。判据：**载荷里带堆所有权（`Vec` / `Arc`）就不得进命令队列**——rtrb 按最大变体预分配，带 `Vec` 的变体只占一个表头（~24 B），定容数组变体却会把整条队列撑到 MB 级。 |
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
- 校验：每轨 clip ≤ 32、单 clip 音符 ≤ 65536（病态文件防线，见 D15 修订）、`span_ticks ≥ loop_ticks`、`start_ticks + span_ticks ≤ 256 小节`；越界报错不截断。
- `PROJECT_VERSION = 4`，无兼容读路径（Q5 延续）。

## 5. 引擎调度（Sequencer 重写）

- `RtMidiClip { start, span, loop_ticks, notes[64], note_count }` / `RtControlClip` 同构，定容 `[[RtMidiClip; 8]; 16]`。
- 调度：块对应的绝对 tick 窗口 `[pos, pos+block_ticks)` 内，扫描每轨 clip 列表；clip 内容按 `loop_ticks` 周期在 `[start, start+span)` 内平铺（D14 未来特性的调度语义一期即落地，UI 一期单周期呈现）；命中的音符转 `(track, NoteEvent)`，pending_offs 机制不变。
- 自动化求值：查 control clips——span 内按周期映射曲线，span 外不投递。
- 走带：位置到达 arrangement 末尾自动 stop；块边界调度精度注记延续（≤1 block jitter）。

## 6. 分阶段计划

- [x] **T0 快修**（不动数据模型）：默认音源切 `Clip`（piano roll 立即可听）；移除步进音序器 UI 区块；加/删 MIDI 轨按钮（引擎 `register_track_chain` / `remove_node` 已有，文档与引擎同步）。✅ `5915e99`
- [x] **T1 模型 v4**：`MidiClip`/`ControlClip` + 校验 + 往返测试；`PROJECT_VERSION = 4`。Steps/Pattern/TrackSource/gate 退役，`AutomationTrack` → `ControlTrack`（clips 顶层化）；UI 过渡期 piano roll / lane 暂编**首 clip**，engine 仍按首 clip 循环调度（SetSource 强制 Clip），窗口调度重写归 T2。
- [x] **T2 引擎 timeline 调度**：`RtMidiClip`/`RtControlClip` + clip 表下发（T2 时经命令队列；2026-10-28 起 MIDI 表改走结构队列，见 D15 修订 / D25）+ 窗口调度 + 内容自适应停止 + automation clip 求值。✅ `Sequencer` 重写为绝对时间窗口扫描（clip 平铺 `start + k·loop`、span 边界截断、arrangement end 自动停但 pending off 继续放完）；`SetPattern/SetSource/SetGate/SetClip/SetAutoTrack` 全部退役（engine 默认 clip span=0=空表）；控制轨求值改为首个覆盖当前位置的 clip。
- [x] **T3 编曲视图**：`timeline.rs`——行 = 轨、clip 块（拖动移位/拖边调长/删除）、双击跳转 Edit 标签页（piano roll / lane 复用）、轨头（增删轨/静音/选择）、标尺与小节网格、播放头。
- [x] **T4 画布 timeline 节点**：显式 Timeline 节点（无输入、n 输出口、不可删除），per-track 源小节点从画布移除；`PortRef::MidiTrack/AutoTrack` 语义不变，纯呈现层合并。✅ 画布单 Timeline 实体（端口 = MIDI 轨 + control 轨，带 T/A 标签），`timeline_port_ref` 为端口→轨映射唯一真源；`CanvasId::MidiTrack/AutoTrack` 退役。**2026-10-26 扩展**：端口序 = **MIDI → Curve → Audio**，映射唯一真源改 `timeline_port_kind`（`TimelinePort::{Midi,Auto,Audio}`）；U6 的 `NodeDoc::AudioTrack` 节点也折进 timeline node（不再单独画框，端口标签 T/C/A），见 `audio-track.md` §4 补充决策。
