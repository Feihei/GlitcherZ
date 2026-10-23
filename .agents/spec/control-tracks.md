# 控制轨：CC 输出与 inline 曲线（U4）Spec

> 状态：草案（2026-10-23）。U4a（CC 输出）与 U4b（inline 曲线）互相独立，可分两批落地。
> 上游：`timeline.md` D17（control clip 投递窗口）、`node-graph.md` D7（`auto_routes`）、`tempo-meter.md` §3（tempo / 普通参数 / CC 三类目标的差异表）、U8（automation strip 已归 timeline）。

## 1. 背景与问题

1. **曲线只能落在"参数"上**：`AutoParam` 只有 `TrackGain` / `TrackPan` / `FilterCutoff` / `PluginParam` / `Tempo`，全部走 `Processor::set_param` 或时钟哨兵。插件里"只认 CC 的控制"（mod wheel / expression / sustain / 老式 DX7 风格音源的 CC 音量）没有任何入口——`PluginParam` 是插件自己的参数 id，与 CC 是两套东西，不能互相替代。
2. **曲线编辑在 Edit tab 的浮窗里**：U8 把"加/删控制轨 + 目标选择"搬到了 timeline 底部，但**画曲线**仍要双击 control clip → 切到 Edit tab（`control_editor_body`）。看曲线要离开编曲视图，且一次只能看一个 clip。

## 2. 目标形态

- **U4a**：control 轨的自动化电缆多一种目标模式——MIDI CC。曲线纵轴 0..127，值按块采样量化成 7bit 事件，注入目标节点的 MIDI 输入流。
- **U4b**：选中控制轨时，它的曲线**直接展开在该行下方**（LMMS song editor 语义），横轴是绝对 tick，所有 clip 的曲线同时可见可编辑；Edit tab 的浮窗退休。

## 3. 关键判断：CC 是"事件流"，不是参数

`tempo-meter.md` §3 已经预判了这条分界，这里把它落实：

| 维度 | 参数自动化（现状） | CC（U4a） |
| :--- | :--- | :--- |
| 消费点 | `Processor::set_param(id, f32)` | `Processor::midi_cc(ch, cc, u8)` |
| 值域 | 各参数自定义（Hz / dB / 归一化） | **整数 0..127** |
| 目标 | 节点参数槽 | 节点 MIDI 输入（与音符同一条流） |
| 语义 | "这个参数现在等于 v" | "此刻发生了一次 CC 事件" |
| 丢失一帧 | 无影响（参数是状态） | 事件丢了就是丢了（但 CC 是绝对量，下一帧补发即可） |

不能把 CC 塞进 `ParamId`：`ParamId` 是"参数槽"，插件的 CC 与插件的参数是两条独立通道，同一个插件可以既有 `Cutoff` 参数又响应 CC74，语义不同、值域不同、无法互换。

## 4. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| D1 | **CC 是电缆的目标模式，不是新节点、不是新电缆类型** | `AutoParam::Cc { controller: u8, channel: u8 }`。电缆仍是 `CableKind::Automation`（`AutoTrack → Node`）——"这条曲线喂这个节点"，只是喂的东西从参数值变成 CC 事件。patch 拖线、删除、单源约束全部复用，零新增结构。 |
| D2 | **引擎路由表区分两类投递** | `AutoRoute` 的 `param: ParamId` 换成 `dest: AutoDest`：`AutoDest::Param(ParamId)` 或 `AutoDest::Cc { channel, controller }`。`TIMELINE_IN` 哨兵照旧（`dest` 在该路由下无意义）。`RtGraph::auto_set_param` 按 `dest` 分派：参数 → `set_param`；CC → `midi_cc`。名字保留（调用点只此一处），文档注明它是"投递"。 |
| D3 | **`Processor::midi_cc` 是新增的显式通道** | `fn midi_cc(&mut self, channel: u8, controller: u8, value: u8) -> bool`，默认 `false`。不复用 `note_event`：`NoteEvent` 的名字与语义都与 CC 无关，硬塞变体会让每个实现者去 match 一个与音符无关的分支。 |
| D4 | **7bit 量化 + 变化才发** | 曲线值 clamp 到 `0..=127` 后四舍五入。每个 control 轨记住上次发过的值（`i16`，`-1` = 从未发），量化结果**没变就不发**。48k/128 下满程扫一遍约 375 块/秒，去重后最多 128 条事件；不插值补发（CC 是绝对量，跳变即当前值）。路由切换/播放开始清空记忆，保证新目标先收到一次当前值。 |
| D5 | **块级时间戳，不做采样级** | CLAP 事件带 `time = 0`（块起点），SF2 在块边界应用。采样级 CC 需要每采样事件队列（`rtrb` + 时间戳排序），成本远超收益：CC 的听感分辨率在 1–10 ms 量级，块长 48k/128 ≈ 2.7 ms 已经足够。与现有参数自动化同为块级，不引入新的精度口径。 |
| D6 | **支持 CLAP 与 SoundFont，VST3 明确不支持** | CLAP 走 `MidiEvent::new(0, 0, [0xB0\|ch, cc, val])`（raw MIDI 1.0）；SF2 走 `process_midi_message(ch, 0xB0, cc, val)`。VST3 在 `vst3-0.3.0` 里 `DataEvent` 只有 `kMidiSysEx`，而 SDK 的 `kLegacyMIDICCOutEvent` 是**输出**事件，用作出生事件属于未定义行为——与其发一个插件大概会忽略的事件，不如诚实地返回 `false`（UI 在目标选择器里标注 `(CLAP/SF2)`）。VST3 用户走插件参数自动化。内置合成器没有 CC 映射，返回 `false`。 |
| D7 | **值域就是 0..127，不归一化** | 曲线里存真实 CC 值（`AutoPoint.value` 0..127），与 `FilterCutoff` 存 Hz 同构；纵轴线性、中线 64。不让用户面对"0..1 对应 0..127"的二次换算。 |
| D8 | **模型校验：CC 只能落在节点上，`Tempo` 只能落在 `Timeline In`** | `CableKind::Automation` 的目标是 `TimelineIn` 时 `param` 必须是 `Tempo`；目标是 `Node` 时 `param` 不得是 `Tempo`（`Cc` 在此合法）。`channel` 存 0..=15，UI 显示 1..=16（MIDI 惯例）；`controller` 0..=127，越界在 `AutoParam::new_cc` 处钳制。 |
| D9 | **U4b：曲线展开在所选控制行下方** | 选中控制轨的行时，紧贴该行下插入一条 `LANE_HEIGHT` 高的曲线带（横跨整个 arrangement 宽度，含 header 列的轴标签）。带内按**绝对 tick** 绘制：每个 clip 在自己的 span 内把曲线按其 `loop_ticks` 重复铺开（与音频实际播出的完全一致）；点击/拖拽映射到"该 tick 所属 clip 的 loop 内 tick"（吸附 `TICKS_PER_STEP/2`），右键删除该 clip 内最近的锚点。点在没有任何 clip 的位置不做任何事并给状态提示。 |
| D10 | **Edit tab 的曲线浮窗退休** | `EditTarget::Control` 删除，`control_editor_body` 整段删除。双击 control clip = 选中该轨（展开曲线带），不再切 Edit tab；Edit tab 只剩钢琴卷帘（`EditTarget::Midi`）。曲线**只有一个编辑入口**（inline 带），避免两套拖拽状态。 |

## 5. 投递路径（U4a）

```text
UI ──cable(Cc{ch,cc})→ engine.set_auto_cc_route(index, node_id, ch, cc)
                         └─ node_slot 查找 → AutoRoute{slot, dest: Cc{..}}
                                              │
     每个 block: evaluate_automation(position) 采样曲线 → auto_deliver(index, value)
                                              ├─ dest Param → slot_set_param
                                              └─ dest Cc    → 量化 7bit → 变化? → slot_midi_cc
                                                                        ├─ ClapNode  → PendingClapEvent::Midi → MidiEvent
                                                                        └─ Sf2Voice  → process_midi_message(0xB0)
```

## 6. 被否决的备选

| 方案 | 否决理由 |
| :--- | :--- |
| 新 `CableKind::MidiCc` | 与 Automation 只差一个 `param` 变体，却要复制拖线、命中、删除、单源、校验五处逻辑，换不来任何表达力。 |
| `ParamId::MidiCc(u8)` | 让 CC 混进参数通道：`param_id_of` 会把 CC 送进 `set_param`，插件把 0xB0 当参数 id 查表必然落空；类型上也无法表达"CC 需要 channel"。 |
| CC 走 `NoteEvent::Cc` 变体 | `NoteEvent` 的消费方（音符扇出、`node_note`、每个 Processor 的 `note_event`）都要 match 一个与音符无关的变体；`midi_cc` 单独一条通道更诚实。 |
| 每个 block 强制发一次 CC | 375 事件/秒全指向同一个值，插件侧要么每次都跑一遍平滑，要么日志被刷屏；去重是零成本的。 |
| 采样级 CC 时间戳 | 需要每采样事件队列与排序，收益在听感上不可辨（见 D5）。 |
| VST3 也发 `kLegacyMIDICCOutEvent` | 该事件在 VST3 SDK 中是**插件输出**语义；宿主发它属于越界使用，插件对未定义事件的行为不可预期。 |
| U4b 保留浮窗 + 新增 inline | 两套编辑入口共享同一份曲线，拖拽状态、undo 合并键、锚点索引回填都要各维护一份，必然分叉。 |

## 7. 开放问题（不在 U4）

- **clip 级 CC 事件（`Sequence.ctrls`）仍无引擎通路**：M1.3 的模型与 piano roll 编辑已就位，但 `RtMidiClip` 只有 notes，CC 到不了插件。这与 U4 的曲线 CC 是两条独立来源（一种是"写在 clip 里的事件"，一种是"曲线采样的流"），落地时共用 `Processor::midi_cc` 即可，但需要 sequencer 侧按 tick 排序调度，单独立项。
- **CC 曲线的平滑**：v1 是台阶（块级 + 7bit）。若插件对台阶敏感，可在量化前做一阶低通（需要每轨状态），留给实际听感反馈。
