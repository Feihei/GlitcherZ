# Spec: Editor 单一画布 —— Curve 焦点（F-049）

> ID: F-049 · 域: EDIT / UI
> 状态：已交付（2026-11-18）。来源：用户实测两条反馈 ——「曲线编辑被限定在一个小节之内，小节之外点击没反应」「curve 与 MIDI 同时选中时希望在同一钢琴窗里叠加显示，非焦点淡显」。取代 F-048 的 D5（曲线独立画布）与 D6（按类型切换编辑器）。

## 1. 背景与问题

F-048 给 curve clip 补了独立画布（ruler + 小节线 + 播放头），并加了 `editing` chips 让混选能切换目标。实测仍有两条：

1. **曲线长度被锁在一小节**：小节之外点击、拖拽都没有反应，一条 1 小节的曲线永远长不大。
2. **两种 clip 不能同框**：即使混选，Editor 里也只有一个（钢琴卷帘 **或** 曲线画布）。想把曲线对准音符，只能来回切。

根因两条，都不在画布本身：

| # | 根因 | 位置 |
| :-- | :-- | :-- |
| R1 | `curve_upsert` 把 tick **钳进内容循环**（`snap(data) % period`），而新建 curve clip 只有一小节内容。小节之外落点全被折回第 1 小节 | `timeline_ops::curve_upsert` |
| R2 | `enum EditTarget { Midi(RollSubject), Control {…} }` 让两种焦点是**互斥的两种编辑器**：`control_editor_body` 自己造几何、自己画时间轴，和钢琴卷帘没关系 | `lib.rs` / `editing.rs` / `status_bar.rs` |

## 2. 核心决策

| # | 决策 | 结论 | 理由 |
| :-- | :-- | :-- | :-- |
| D1 | 一种焦点 | `ClipRef` 带上 `RollLane`（`Midi` / `Control`），curve clip 就是 `RollSubject` 的**焦点**；删除 `EditTarget`、`CurveTool`、`control_editor_body`（约 380 行） | 两种 clip 应该走同一套时间轴、视窗、播放头、ruler、chips、`Ctrl+Tab`。两套画布是问题的源头，不是解法 |
| D2 | 空 MIDI 占位 | `show()` 发现焦点是 control clip 时，合成一个同窗口的空 `MidiClip` + 空 `MidiData`，让 note 相关几何 / ghost / 视窗代码**原样复用** | 曲线焦点没有音符可写，但需要「同一时间轴上的空卷帘」；命中测试自然什么都找不到，不必给每条 note 路径加 `if` |
| D3 | 焦点 lane 加高加亮 | 焦点 lane `CURVE_FOCUS_LANE_HEIGHT = 112`（scenery 64、折叠 18），亮色画笔 + 4 px 锚点；scenery 用 ~30% alpha 的暗色 + 2.5 px 锚点，**画完即 `continue`**，不做命中 | 与「焦点 clip 可写 / ghost 只读」完全同构；「我在编辑哪条」不用猜（与 F-048 D6 的意图一致，只是把切换器交给已有的 chips） |
| D4 | 写时增长 | 落点超出内容时不再钳制：内容长到锚点（`tick + 1`），窗口长到**下一根小节线**（`curve_growth`，与 `midi.note_add` 的写时增长同规则）；有 `loop_ticks` 时仍按循环取模 | 这才是「曲线能长」的唯一正确位置 —— 在命令层，且和 MIDI 音符的既有规则对齐，卷帘只是把真实 tick 交上去 |
| D5 | 空力度条不占位 | curve 焦点不分配力度条（不是分配 0 高度） | 0 高度的一行仍吃掉约 20 px 布局，把 lane 栈顶出面板：底部若干像素落在 clip rect 之外，点击不响应 |
| D6 | 工具行搬到卷帘工具栏 | `curve_toolbar`（`Line` / `Draw` / `Smooth`）替换 note 工具栏的位置；`Draw` 仍写 `Linear`，差别在手势（逐帧落点 vs 拖一个锚点） | U12 的三种输入方式保留，但不再是「另一个编辑器里的另一条工具栏」 |

## 3. 行为细则

### 3.1 布局

```
Editor
├ chips（context ≥ 2 时）: MIDI 与 curve 混排，焦点项填充；Ctrl+Tab / Ctrl+Shift+Tab 轮换
├ 工具栏：MIDI 焦点 → note 工具栏 + Notes 菜单；curve 焦点 → Line / Draw / Smooth
├ clip ruler（两者共用，点击 seek）
├ 卷帘（焦点 = MIDI 时是 note 区；焦点 = curve 时是空区 + 画 ghost 音符）
├ 力度条（仅 MIDI 焦点分配）
└ curve lanes：context 里每条 control clip 一条；焦点那条可写，其余淡显只读
```

- 视窗（`ViewWindow`）对 lane 取并集，再并上焦点 lane 的一小节空白：小节之外一开始就看得见、点得到。
- `Ctrl+Tab` 只在 `context.len() > 1` 时由 Editor 消费；焦点在 lane 之间无差别轮换。

### 3.2 坐标

lane 的横轴是**数据 tick**（含 `source_offset`），与卷帘同一把尺：

```
x_data(tick) = lane_geom.x_of(tick − source_offset)
data_at(x)   = lane_geom.local_at(x) + source_offset
```

- 绘制范围：无 `loop` 时 `[source_offset, min(source_offset + span, length))` —— 窗口里超出内容的那截是空白，不是重复曲线。
- 有 `loop_ticks` 时按周期画（同一锚点重复播放），落点按 `% period` 折回。
- 拖动按**按下点**分类（与 note 手势同规则）：egui 报出 `drag_started` 时指针已经移动，用移动后的位置找锚点会松掉用户真正按住的那一个。
- clip 起点左侧不算这条 clip（`in_clip`）：那里是空白画布，不是 tick 0 的锚点。

### 3.3 增长

```
end     = tick + 1
content = max(end, length)
needed  = end − source_offset
span    = needed > span ? ceil(needed / bar) × bar : span
```

- 有 `loop` 直接返回 `None`（循环会平铺，没有「更长」这件事）。
- 一次命令一步 undo；拖拽帧走合并手势（`curve:{track}:{clip}`）。
- 增长后 `send_control_clips(track)` 重推引擎，播放窗口随 `span` 一起变长。

### 3.4 试听与推子

- `Play` / `Space` 的试听范围按**焦点 lane** 取：MIDI → `start + span`，curve → `start + span`，两者同一份代码。
- curve 焦点不发送 note 预听事件（`take_preview_events()` 直接丢弃），避免在曲线轨上误响焦点音符。

## 4. 影响面

- `crates/glitcherz-ui/src/piano_roll.rs`：`RollLane` / `ClipRef` 焦点、`CurveInterp` intent、`curve_lanes`（数据 tick 坐标 + 焦点 / scenery 分叉）、`curve_toolbar`、空 MIDI 占位、力度条跳过的布局修正。
- `crates/glitcherz-ui/src/editing.rs`：`set_edit_focus` / `set_roll_focus`、`editor_body`（lane 感知的表头、chips、lanes 构建、intent 分发）、删除 `control_editor_body`。
- `crates/glitcherz-ui/src/timeline_ops.rs`：`curve_upsert` + `curve_growth`。
- `crates/glitcherz-ui/src/lib.rs`：`edit_clip: Option<RollSubject>`（删除 `EditTarget` / `CurveTool` / `curve_tool` / `curve_drag`）。
- `crates/glitcherz-ui/src/status_bar.rs` / `transport.rs` / `mixer_panel.rs`：`EditTarget` 的三处匹配改为 lane 感知。
- `docs/manual.md` / `docs/shortcuts.md`：Editor 章节重写。

## 5. 验收

- [x] 小节之外点击 curve lane 落的是真实 tick（`a_lane_click_past_the_content_is_not_clamped_into_the_loop`：tick 3000 原样上报）。
- [x] `curve_growth`：内容长到锚点、窗口取整到小节；`loop` 不增长；窗口有 `source_offset` 时按局部长度算（4 例单测）。
- [x] 非焦点 lane 点不动（`a_lane_outside_the_focus_takes_no_edits`）。
- [x] 拖拽仍按按下点抓锚点（`dragging_a_lane_anchor_reports_the_anchor_it_moved`）。
- [x] 焦点 lane 112 px、scenery 64 px 时 lane 栈不出面板（`dragging_a_lane_anchor…` 的几何断言依赖它）。
- [x] `cargo fmt` / `clippy -D warnings` / `test --workspace` 绿（39 + 344 + 全工作区）。

## 6. 明确不做

- 不在 lane 里做多选锚点 / 框选（曲线仍是「逐点」编辑）。
- 不做 lane 之间拖拽换轨（换轨仍走时间轴）。
- 不让 scenery lane 承接任何写入（焦点只能从 chips / `Ctrl+Tab` 换）。
- 曲线不再有独立的「折叠成一行」以外的视图状态；关闭 Editor 后不记忆画布位置。
