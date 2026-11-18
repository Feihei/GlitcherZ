# Spec: Editor 视图修缮（F-048）

> ID: F-048 · 域: EDIT / UI / ENGINE
> 状态：已交付（2026-11-18）。来源：用户实测五条反馈（力度误伤 / 音频尾巴不播 / 波形裁剪预览 / 曲线时间轴 / tab 命名）。
> **D5 / D6 已被 F-049 取代**：曲线不再有独立画布与「按类型切换编辑器」，它成为钢琴卷帘里的一个焦点 lane
> （见 `editor-curve-focus.md`）。D1–D4、D7 仍然有效。

## 1. 背景与问题

用户在一次实测中报出五条，全部集中在「编辑视图 + 播放范围」：

1. **力度误伤**：在钢琴窗力度条（velocity lane）上拖某个音符，该音符**前面所有音符**的力度一起变，无法单独调整。
2. **播放范围截断**：Timeline 同时有 audio 与 MIDI clip 时，播放只到 MIDI clip 的结尾，audio 长出来的部分不播（`End` / `⏭` 也停在 MIDI 结尾）。
3. **波形预览不裁剪**：audio clip 拖边缘裁剪后，边缘没有锯齿标记，波形看起来像整段被挤压到新宽度（播放内容其实是对的）。
4. **曲线编辑器缺时间轴**：curve clip 的编辑视图没有小节线 / 拍线 / 播放头；与 MIDI clip 同时选中时，Editor 只显示其中一个，不能切换。
5. **命名不统一**：`Edit` 是动词，其余 tab（`Timeline` / `Patch` / `Inspector`）都是名词。

## 2. 核心决策

| # | 决策 | 结论 | 理由 |
| :-- | :-- | :-- | :-- |
| D1 | 力度命中域 | **按柱命中，不按 tick 跨度**：每音符一根 3–8 px 的柱画在它的起点，命中同一矩形，取最近的一根（和弦重叠时取音符序第一个） | 旧实现 `start <= tick < end` 让长音符"拥有"整条 lane：点后面音符的柱会连带改掉所有覆盖该 tick 的长音符 → 用户看到的"前面全变了"。绘制与命中共用一个矩形（`velocity_bar_rect`），不再漂移 |
| D2 | 编排结束 | UI 的 `arrangement_end_ticks` 加入 audio 轨（`audio_clip_span_ticks`）；live engine 通过 `set_arrangement_floor` 拿到同一个值 | sequencer 的 auto-stop 只认识 MIDI/control，audio 不是 sequencer 内容；此前只有离线导出传 floor，live 播放在最后一个 MIDI 小节停住 |
| D3 | 波形映射 | `draw_waveform` 接收 `(source_offset, source_len)`，把峰值索引按**裁剪窗口**映射，而不是把整个文件铺满 rect | 矩形是**窗口**、峰值是**整个文件**；少一层映射就变成"整段挤压"。播放读窗口，所以只有预览错 |
| D4 | 裁剪标记 | 抽出 `draw_crop_teeth`，MIDI / control / audio 共用；audio 左缘 `source_offset > 0`、右缘 `source_offset + source_len < 文件时长`（拿不到样本时只判左缘） | 三类 clip 的裁剪语义应该长得一样，用户一眼区分"被裁"和"文件本来就短" |
| D5 ⤳ | 曲线时间轴 | 画布顶部加 16 px 小节 ruler（小节号 + 点击 seek），曲线区按 meter 表画小节线（强）/ 拍线（弱，像素不足时省略），并画播放头（播放黄色 / 停止暗黄） | 与 Timeline、钢琴卷帘读同一张 `MeterMap`；canvas 仍显示一个内容周期，`base = start_ticks − source_offset` 把 data tick 映射到绝对位置 |
| D6 ⤳ | 编辑目标切换 | Editor 顶部在**选区含 ≥ 2 个可编辑 clip** 时显示 `editing` chips（MIDI + curve 混排）；单个目标时不占空间 | 目标此前只跟"最后选中"的 clip；混合选择时够不到 curve。chips 只改视图目标（走 `set_edit_target` / `set_control_edit_target`），不动文档、不进 undo |
| D7 | 命名 | `MainView::Edit` 的标签与占位标题改为 `Editor` | 与其余名词型 tab 一致；`Edit` 保留给 `File / Edit` 菜单 |

## 3. 行为细则

### 3.1 力度条

```
bar = [x_of(n.start), x_of(n.start) + clamp(n.length × scale, 3, 8)] × [bottom − h, bottom]
h   = (LANE_HEIGHT − 4) × velocity / 127
命中 = argmin over bars of distance(x, bar)，distance ≤ HIT_SLOP.x（4 px）
```

- 有选区时仍然整选区跟随指针（既有行为）；无选区时只改命中的那一根。
- 柱之间扫过 = 逐个"paint"，因为每帧只发当前柱，不回写其他音符。
- 长音符不再吞掉它时间范围内的其他音符。

### 3.2 编排结束与 auto-stop

```
end = ceil_to_bar(max(MIDI/control start+span, audio start + audio_clip_span_ticks))
```

- `arrangement_end_ticks` 是 `End` / `⏭` 与 floor 的唯一来源。
- `push_arrangement_floor` 在 audio clip 表推送时调用（`send_audio_clips_for_track` 开头，节点被删也生效）与全量重建尾部调用（撤销 / 打开工程 / 删轨）。
- floor 语义是"auto-stop 不早于该 tick"；空工程为 `0`（无界）。

### 3.3 曲线编辑器画布（⤳ F-049 已取代）

- 小节 / 拍线 / 播放头 / seek 的规则不变，但不再有独立画布：它们由卷帘的 ruler 与 curve lane 承担。

- 布局：`ruler`（16 px）+ `inner`（曲线区），x 轴仍是内容周期 `0..length_ticks`。
- 绝对 tick = `base + data_tick`，`base = clip.start_ticks − clip.source_offset`。
- 小节线按 `MeterSegment.signature.ticks_per_bar()`，拍线按 `ticks_per_beat()`；拍线像素宽 < 6 px 时只画小节。
- ruler 点击 → `engine.seek_ticks(base + tick)`；曲线编辑（加点 / 拖点 / 右键删点）只在 `inner` 内生效。

## 4. 影响面

- `crates/glitcherz-ui/src/piano_roll.rs`：`velocity_bar_rect` / `velocity_target`（绘制与命中同源）。
- `crates/glitcherz-ui/src/transport.rs`：`arrangement_end_ticks`（+ audio）、`push_arrangement_floor`。
- `crates/glitcherz-ui/src/voices.rs` / `engine_sync.rs`：floor 推送时机。
- `crates/glitcherz-ui/src/timeline.rs`：`draw_waveform`（窗口映射）、`draw_crop_teeth`（三态共用）、`lane_row` 的 `waveform_of` 返回窗口、audio 行的裁剪标记。
- `crates/glitcherz-ui/src/editing.rs`：`control_editor_body`（ruler / 网格 / 播放头 / seek；F-049 已删除，规则并入卷帘）、`edit_target_strip`。
- `crates/glitcherz-ui/src/status_bar.rs` / `lib.rs`：`Editor` 标签、`edit_target_strip` 调用。
- `docs/`：tab 名称同步。

## 5. 验收

- [x] 钢琴窗：长音符 + 后置音符，拖后者力度条只改后者（单测 `a_velocity_drag_hits_one_bar_not_every_note_under_the_tick`）。
- [x] 空力度条区域拖动不改任何音符。
- [x] audio 尾段超过最后一个 MIDI clip 时能播到结尾，`⏭` 落在 audio 之后。
- [x] 裁剪 audio clip 后波形只显示窗口内的峰值，被裁边有锯齿。
- [x] curve 编辑视图有小节 / 拍线 / 播放头，ruler 可 seek。
- [x] MIDI + curve 混选时 Editor 顶部可一键切换。
- [x] `cargo fmt` / `clippy -D warnings` / `test --workspace` 绿。

## 6. 明确不做

- 不做 velocity lane 的音高分行（bar 仍叠在同一行）。
- 不做曲线 canvas 的多周期展开（仍一个内容周期）。
- 不改 auto-stop 的其它语义（循环 / 试听优先于 floor）。
- 不给 audio clip 增加 MIDI 式的内容 extent 线（只有裁剪锯齿）。
