# 多 clip 钢琴卷帘 + 音高曲线 Spec

> 状态：草案（2026-10-27，经一轮设计讨论）。P0 可独立先行，P2 先于 P3/P4，P1 可并行。
> 上游：`timeline.md`（clip 模型 / 平铺语义）、`editing-model.md`（M4 交互模型：常驻态只有 Select、语义决策源不得回到指针）、`control-tracks.md`（曲线一等实体、`AutoDest`、U4a CC 通路、§7 开放问题）、`sf2-loader.md`（bend range）、`effects.md`（`Smoothed`）。

## 1. 背景与问题

1. **画音高曲线必须离开音符视图**。U12 把曲线编辑做进 Edit tab，但 `EditTarget::{Midi, Control}` 是互斥的二选一：看得到音符就看不到曲线，反之亦然。而 pitch 曲线的编辑动机恰恰是"对着音符画"。
2. **LMMS 的对照**：它的 piano roll pitch 曲线只对部分 instrument 生效，SF2 等采样器必须在独立 curve clip 里画。**根因不是 MIDI 格式限制，而是通路不同**——LMMS 那条曲线是直接改音源的音高**参数**（浮点、逐样本），而采样器/外部插件的音高由它自己的 MIDI 解释决定（bend range、采样层、键区映射），宿主改参数语义不等价，于是被排除在外。
3. **GlitcherZ 现状的三处空缺**：
   - `Sequence.ctrls`（M1.3，含 `CtrlKind::PitchBend`，-8192..8191）在模型里，但**四条链路断了三条**：`RtMidiClip` 只有 `notes`（不发声）、piano roll / event list 无 ctrl 编辑（不可写）、`export_smf` 只写 `0x90`/`0x80`（**导入的 CC/bend 一导出就丢**）。
   - `AutoParam` 只有 `TrackGain`/`TrackPan`/`FilterCutoff`/`PluginParam`/`Tempo`/`Cc`——**没有任何 pitch 目标**，曲线根本弯不了音。
   - 卷帘只认一个 clip，时间上相邻/重叠的其它 clip 完全不可见。

## 2. 目标形态

- **多 clip 同屏**：Edit tab 的卷帘同时呈现一组被选中的 clip，其中一个可写（**focus**），其余只读（**ghost**）。时间重叠、跨轨均可。
- **曲线进同一时间轴**：可见 control clip 的曲线作为**可写泳道**叠在音符区下方，与音符共享横轴——pitch 曲线就在音符正下方画，不需要切换 target。
- **音高通路按声源分流**：内置声源走参数（逐样本平滑），外部声源走 MIDI bend（块级），各自用能表达的方式。

## 3. 关键判断

### 3.1 内嵌曲线与 MIDI 的三层限制

| 层 | 事实 | 后果 |
| :-- | :-- | :-- |
| 格式 | SMF 没有"曲线"，只有离散 `0xE0`（14bit、每通道一条） | 导出必须把曲线采样成密集事件：保真度与文件体积互为代价。这是**有损转换**，不是不兼容 |
| 语义 | bend 是每通道一份，复音下弯不了单个音；bend range 在接收端（RPN 0 / 采样器内部），宿主不知道 | "半音"纵轴无处定义，除非宿主显式持有 range（见 D9）；per-note pitch 需要 MPE 或独立 voice 参数 |
| 工程 | 需要同时补齐模型 / 引擎 / UI / 导出四条链路 | 不是加字段，而是补三条链路 + 一套编辑器 + 一组定容容量决策 |

### 3.2 坐标系已参数化

卷帘现有几何是 `t0_x = rect.left() - scroll_x * ticks_to_x`、`pointer_tick = (pos.x - t0_x) / ticks_to_x`，而 `scroll_x` 的定义就是"卷帘左边缘的绝对 tick"。**横轴已经是绝对 tick 线性映射**，clip 局部 tick 只是恰好从 0 起算。

因此多 clip 同屏 = **给几何加一个 clip 原点 + 放开可见范围钳制**，不是重写几何。所有绘制与命中共用 `t0_x`/`ticks_to_x` 两个量，改动集中且可用单 clip 等价测试锁定。

### 3.3 曲线属于"节点图的一等实体"

D7/D17 已定：曲线是独立实体，投递终点由 patch 连线决定。把曲线内嵌进 MIDI clip 等于在节点图之外开第二条旁路——sequencer 里两套来源排队，命令族/undo/导出都要双写。

## 4. 决策

| # | 决策 | 内容 |
| :-- | :-- | :-- |
| D1 | **横轴统一为绝对 tick，引入 clip 原点** | 几何抽成 `RollGeom { origin_tick: i64, t0_x: f32, ticks_to_x: f32, top_note: f32, row_h: f32 }` 随绘制/命中函数传递（现在这对参数已经到处传，成结构体不改变数学）。音符/clip 内元素的 x = `t0_x + (origin_tick + local_tick) * ticks_to_x`；指针 → 局部 tick 时减 `origin_tick`。focus 与每个 ghost 各带自己的 `origin_tick`。 |
| D2 | **可见范围自由，编辑范围受限** | `scroll_x` 钳制从 `[0, loop_ticks - width/t_x]` 改为 `[min_start - margin, max_end + margin - width/t_x]`（`min/max` 来自 context 的 clip span），允许滚出 clip 看到邻近 ghost 与空白。**但创建音符仍限制在 focus 的 `[start, start+span)`**，窗外点击 = no-op + 状态提示。 |
| D3 | **zoom 的 fit 基准从 focus 的 `loop_ticks` 改为 context 的 tick 跨度** | `fit_ticks = (max_end - min_start).max(bar_ticks)`。单 clip 时等价于现状（`loop == span` 不变量），多 clip 时保证初次打开 ghost 也在视窗内。 |
| D4 | **`RollSubject { focus: ClipRef, context: Vec<ClipRef> }`** | `ClipRef { lane: Midi \| Control, track: usize, clip: usize }`。`focus` 必在 `context` 内。`EditTarget::Midi { track, clip }` 升级为 `EditTarget::Midi(RollSubject)`，`EditTarget::Control` 保持不变（曲线编辑器仍可独立打开）。`MAX_ROLL_CONTEXT = 8`，超出按选区顺序截断 + 状态栏提示。 |
| D5 | **context 来源 = timeline 选区** | 双击 clip 时装配：timeline 选中集合中的 clip ∩ 可显示类型，`focus` = 双击项；若双击项不在选区则 `context = [它]`。选区为空时等价于现状。P3 只收 MIDI clip；P4 起把 control clip 一并收进（走泳道）。 |
| D6 | **ghost = 非 focus 的可见元素，低 alpha、只读、不参与命中** | 渲染：音符矩形 alpha ≈ 0.22、无描边、无 velocity 条、无 hover 高亮。命中：不进 `note_at`、不进框选、刀切轨迹不切它、拖动不可能抓到它。**焦点切换不走 ghost 点击**（见 §6）。理由：M4 花整个 spec 把语义决策从指针坐标移开，ghost 吃点击会把"空拖=框选 / Shift 追加"的歧义重新引回来。 |
| D7 | **焦点切换 = 纯视图状态** | Edit tab 顶部一条 chip 条（`Track 1 · bar 5`），点击或 `Tab`/`Shift+Tab` 循环切换；timeline 双击亦可。不入 undo、不动文档。切换时**清空** `RollState.selection`（索引属于旧 focus 的 `sequences`）并重做 `retarget`。 |
| D8 | **曲线叠加用统一 `Overlay` 抽象，不单独开 control 路径** | 泳道栈 = velocity lane（focus 专属）+ `context` 中每个 control clip 一条 `OverlayLane { clip: ClipRef }`。纵向可滚动、可折叠。将来 audio clip 同屏沿用同一抽象。 |
| D9 | **PitchBend 作为曲线目标，曲线值以"半音"计量** | `AutoParam::PitchBend { channel: u8, range_semitones: f32 }` + `AutoDest::PitchBend { channel, range_semitones }` + `Processor::midi_pitch_bend(channel: u8, value: i16)`（-8192..8191，默认实现返回 `false`）。`range_semitones` 默认 2.0、是**用户可编辑的文档字段**（不是运行时探测）——这样曲线纵轴永远有确定含义，14bit 换算 `v = round(clamp(semi / range, -1, 1) * 8191)`。`channel` 0..=15、`range_semitones` 钳到 0.5..=24。 |
| D10 | **声源分流（与 U4a D6 同构）** | <br>• 内置 synth / sample player → **参数 `PitchOffset`**（f32 半音，-24..24）<br>• CLAP → raw MIDI `0xE0`（`MidiEvent::new(0, 0, [0xE0\|ch, lsb, msb])`）<br>• SF2 → `process_midi_message(ch, 0xE0, lsb, msb)`，range 取 preset 的 pitch bend range 作默认值<br>• VST3 → **当前 `vst3-0.3.0` 绑定发不了 bend**（`DataEvent` 只有 SysEx，与 control-tracks D6 同一结论）→ 返回 `false`，UI 标注 `(CLAP/SF2)`<br>• 内置合成器/采样器**不**响应 `midi_pitch_bend`（走参数） |
| D11 | **`PitchOffset` 作用于在唱 voice，逐样本平滑** | synth：voice 存 `base_freq`，每样本乘 `2^(offset/12)`；sample：`rate = base_rate * 2^(offset/12)`，同处逐样本。用 `Smoothed`（effects.md D 系列同一机制）每样本推进。**与 `SampleRootNote` 的语义有意相反**（root 只影响后续 voice，pitch offset 影响当前——否则滑音不可能）。 |
| D12 | **块级 vs 逐样本是体验分水岭** | MIDI bend 与 CC 同为块级（48k/128 ≈ 2.7 ms，与 control-tracks D5 一致），vibrato 会有可闻台阶；参数通路逐样本平滑。文档与 UI 都要诚实：内置声源的 pitch 曲线比插件更细腻。 |
| D13 | **一写多读** | `Sequence.ctrls` 只读（导入、导出、event list 展示）；`ControlClip` 是**唯一书写路径**。理由：U12 已经用"单一编辑入口"否掉过一次双入口，这里不重新开。 |
| D14 | **补 `ctrls` 的引擎与导出通路（P0，独立交付）** | `RtMidiClip` 加 `ctrls: [RtCtrl; MAX_CLIP_CTRLS]` + `ctrl_count`（`RtCtrl { tick: u32, channel: u8, kind: u8, value: i32 }`，`MAX_CLIP_CTRLS = 32`）；sequencer 窗口扫描 ctrl（零长度点事件，无 pending off）；投递复用 `Processor::midi_cc` + 新增 `midi_pitch_bend`；`export_smf` 补 ctrls（含 `0xE0` 编码，同 tick 排序沿用 note-off 优先规则）。**不动 UI 编辑**——这是修补 latent bug（导入的 MIDI 不响、导出丢失），不是新特性。 |
| D15 | **不做 curve↔midi 数据绑定** | `MidiClip` 没有稳定 id（数组索引会漂移），绑定会在 copy / split / move 时级联维护。同屏之后"对齐"由视觉 + bar 吸附保证；要"一起移动"用 M4b 已有的 timeline 多选 + G 批量——那天然就是用户想要的"绑定"。 |
| D16 | **泳道可写，写入目标是它自己的 control clip** | 卷帘主区写音符（focus），每条 overlay lane 写**该 lane 对应 clip** 的曲线，两者界限清晰、互不越权。命令走既有 `curve_upsert` / `curve_remove_point`（U12），一步 undo、按 `curve:{track}:{clip}` 合并拖动。**这是本 spec 对"必须同屏"这一诉求的正面回答**——曲线不进 MIDI clip，但编辑它们不再需要切换 target。 |
| D17 | **loop 边界跳变如实绘制** | `ControlClip::value_at` 区间外保持端点值（U12 已定），所以 clip 按 `loop_ticks` 平铺时每个接缝都是**跳变**而非斜坡。泳道不要自动补平，否则视觉与听感不符。（v1 因 `loop == span` 不变量实际只有一段，Lane 绘制按 `tiled_notes` 式迭代，将来放开时不返工。） |

## 5. 数据流

```text
timeline 选区 (M4b clip 多选)
        │ 双击 clip
        ▼
RollSubject { focus, context }        ← MAX_ROLL_CONTEXT = 8
        │
        ├─ focus ────────→ 主区（全交互：选中/拖动/G/S/T/K/框选/画音符）
        │                   写入 → RollIntent → midi.* 命令（零改动）
        │
        ├─ MIDI ghost ───→ 主区只读渲染（alpha 0.22，不命中）
        │
        └─ Control clip ─→ OverlayLane（横轴同 RollGeom，纵轴按 AutoDest）
                            写入 → curve_upsert / curve_remove_point
                                    │
                                    ▼
                    CableDoc(Automation → 节点或 TimelineIn)
                                    │
                ┌───────────────────┴───────────────────┐
          AutoDest::Param                     AutoDest::Cc / PitchBend
        （内置 synth/sample 的 PitchOffset）    （CLAP raw MIDI / SF2）
        逐样本 Smoothed                          块级量化 + 变化才发
```

## 6. 被否决的备选

| 方案 | 否决理由 |
| :-- | :-- |
| MIDI clip 内嵌 pitch 曲线（LMMS 式） | 要补四条链路 + 第二条编辑路径 + 导出采样渲染；与"曲线是节点图一等实体"（D7/D17）冲突；复音与 bend range 的语义问题一个都不会消失。LMMS 能做只是因为它的曲线压根不是 MIDI。 |
| 每个 clip 各自一个坐标变换 | 每加一种 lane 类型都要重算几何；重叠/平铺无法共享横轴。现状几何已经参数化在 `t0_x`/`ticks_to_x` 上，加原点更便宜。 |
| ghost 可点击切换焦点 | 与"空拖=框选""Shift 追加"抢语义，等于退回 M3.8 的指针中心模型；误触成本高（想框选却切了 focus，selection 被清空）。 |
| curve clip ↔ midi clip 数据绑定 | 无稳定 clip id；copy/split/move/resize 全部要级联传播；且同屏后"对齐"已由视觉解决。 |
| 把 pitch 存进 `Note`（per-note pitch） | 复音下 per-note pitch 仍需 MPE 或独立 voice 参数；`Note` 的字段是排序键的一部分（`sort` 按 `(start, note, channel, length, velocity)`），加字段会污染不变量与所有 ops 的签名。 |
| overlay lane 只读 | 用户诉求就是"对着音符画"。只读等于还得切 target，问题原样保留。 |
| 用 MIDI bend 统一所有声源 | 内置合成器没有 MIDI 输入语义（`NoteEvent::On` 直接定频），且块级量化做不出细腻 vibrato；VST3 当前发不了 bend。 |

## 7. 开放问题

- **ghost 是否显示 velocity 条**：当前拟不显示（velocity 是 focus 专属语义，ghost 的值没有编辑入口）。
- **context 上限 8 是否够**：16 轨 × 8 clip 理论上可全选（128）。P3 先 8 并提示，实测后再调。
- **`range_semitones` 的编辑落点**：曲线 lane 头部（就地 DragValue）还是 Inspector 的自动化目标行？倾向前者（与 lane 的纵轴刻度同处）。
- **P0 是否真的先做**：它是 latent bug 修复（导入不响 + 导出丢失），与多 clip 无依赖。建议先做，但它会改动 `RtMidiClip` 与命令体大小。
- **`SequencerCommand` 队列内存**：rtrb 按元素大小 × 容量预分配，`RtMidiClip` 加 ctrls 会让 `SetTrackClips` 命令体涨约 30%（单条约 10 KB → 13 KB）。可接受，但需实测确认不触及容量边界。

## 8. 分阶段

- [ ] **P0 `ctrls` 链路补齐**（D14）：model→engine→export 三段；`midi_pitch_bend` trait 方法；RT 零分配审计。UI 不动。
  - 验收：导入含 CC/bend 的 MIDI → 发声正确；导出 → 事件不丢；`smf` 往返测试。
- [ ] **P1 音高通路**（D9–D12）：`AutoParam::PitchBend` + `AutoDest::PitchBend` + `ParamId::PitchOffset` + synth/sample 逐样本实现 + 分流表落地。
  - 验收：内置 synth 的 pitch 曲线无台阶（逐样本）；CLAP/SF2 收到 `0xE0`；VST3 返回 false 且 UI 标注。
- [ ] **P2 坐标系绝对化**（D1–D3）：`RollGeom` 结构体化、clip 原点、可见范围放开、fit 基准改 context 跨度。
  - 验收：单 clip 下所有既有 roll 测试与实测行为**逐项等价**（这是纯重构，行为不变）。
- [ ] **P3 多 clip 主体**（D4–D7）：`RollSubject` + ghost 渲染 + chip 条 + 焦点切换 + context 装配。
  - 验收：重叠加跨轨的 3 个 clip 同时可见；ghost 点击/框选/刀切均无反应；切换焦点后 selection 清空且编辑落到新 focus。
- [ ] **P4 曲线泳道**（D8、D16、D17）：多泳道栈 + 纵轴按 `AutoDest` + 可写 curve 编辑。
  - 验收：pitch 曲线在音符正下方对齐绘制、可拖动锚点、undo 一步；CC lane 纵轴 0..127；bend lane 纵轴按 `range_semitones`。
- [ ] **P5（可选）泳道打磨**：ghost velocity、泳道折叠记忆、lane 高度拖动、context 上限调整。

依赖：P2 必须先于 P3/P4；P1 与 P2 可并行；P0 独立。
