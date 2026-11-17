# 数据池面板与 clip 右键菜单

> ID: F-046 · 域: EDIT / UI / PROJ
> 状态：**M1 已实施**（2026-11-17）；M2 / M3 待做
> 关联：`clip-data-instances.md`（F-041 D15/D17/D27 的 M7 展开，本特性取代之）、`timeline.md`（W3b 编组、B2 静音）、`clipboard.md`、`snap.md`、`command-vocabulary.md`

## 1. 问题

### 1.1 数据没有名字，管理只能靠编号

`midi_data` / `curve_data` 是 `Vec`，clip 用 `data: u32` 存**池索引**（`lib.rs:6446/6449`）。UI 上唯二的标识是：

- clip 头上的 `shared:N` 徽章（`timeline.rs:3553`）；
- Inspector 里那行 `Clip data · MIDI #3 · shared by 2 clip(s)`（`timeline_ops.rs:2160`）。

`#3` 是槽位号，不是身份：`compact_clip_data`（`lib.rs:6699`）在保存时压缩孤儿，索引会整体重排。所以现在既无法在工程里指认"那段鼓点是哪段"，也无法在两次保存之间记住一个编号。F-041 D17 把「数据命名」划出了范围，这一条现在挡在所有数据管理需求前面。

### 1.2 Inspector 串了两个选择域

Inspector 的职责是"选中对象的参数"（`inspector_panel.rs:8-13` 自己这么写的）。但它在标题下面直接挂了 clip 区块（`inspector_panel.rs:66` → `timeline_ops.rs:2137`），而 clip 和 patch 节点是两个**互斥**的选择域：选中一个节点时 clip 区块也会出现（只要时间轴还留着旧选区）。用户不会去节点参数面板里找 clip 数据，也找不到。

同时，那个区块是 `Link Data` / `Unlink` 的**唯一入口**，摘掉它必须同时给出替代路径。

### 1.3 右键现在是"删除"

`timeline.rs:4037`：

```rust
// Right-click: cancel a live band, disarm the knife, otherwise
// delete the clip under the pointer.
if response.secondary_clicked() && state.modal.is_none() {
    if state.knife_armed { ... } else if let Some(pos) = ... {
        if state.marquee.take().is_none() {
            if let Some(index) = hit_clip(clips, tick_at(pos.x), sec_per_tick) {
                actions.push(TimelineAction::RemoveClip { ... });
```

右键即删是早期为了"快速清空"留下的，和任何 DAW 的右键菜单都冲突，而且误触代价高。加菜单必须先顶掉它。

### 1.4 删除键不一致

| 表面 | `Delete` | `Backspace` |
| :--- | :--- | :--- |
| 时间轴 clip | ✅ `timeline.rs:4080` | ❌ |
| 卷帘音符 | ✅ `piano_roll.rs:2789` | ❌ |
| Patch 节点 | ✅ `patch.rs:5437`（两者都收） | ✅ |

同一个"删除"在三处有三种行为，Backspace 在时间轴上完全无效。

## 2. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| D1 | **数据加名称** | `MidiData` / `CurveData` 各加 `name: String`，`#[serde(default, skip_serializing_if = "String::is_empty")]`。additive 字段 → **`PROJECT_VERSION` 保持 14**，旧工程加载后名称为空。空名回退显示 `MIDI #3` / `Curve #5`（沿用现有徽章文案） |
| D2 | 名称随内容走 | 复制 / COW / `Alt+D` 派生新数据时名字一起复制；**不生成 `.001` 后缀**——重名由 `#id` 与内容摘要区分，改名是一次双击。`Link Data` 保留首个成员的名字，被丢弃的数据随孤儿在保存时消失 |
| D3 | **面板放时间轴内的上下分栏** | `Data` 面板是 **Timeline 视图内部的底部条带**，与 Edit 视图的 Event List 完全同构（`editing.rs:565` 的 `egui::Panel::bottom` + `show_switched`）：上面是轨道排列，下面是数据表，中间可拖拽的分隔线。只在 `Timeline` 视图出现——数据管理是编排工作的伴生面板，不是跨视图的全局 dock。**工具栏（吸附下拉、轨道增删、`Grab`/`Trim`/`Tile`/`Knife`）仍在轨道上方**：分栏只切分工具栏之下的空间，`Panel::bottom` 挂在 `timeline_panel` 的可用区域上，工具栏与标尺的位置、宽度都不变（用户 2026-11-17 确认） |
| D4 | 默认收起 | 首帧只显示标题行 `Data · 12 MIDI · 3 curve · 2 unused`，与 Event List 的折叠行为一致：点标题展开/收起，向下拖分隔线可拖到最小高度收起。展开默认占标签页高度的 1/3（`EVENT_LIST_HEIGHT_FRACTION` 同款），上限 60%。开合状态是 view state（D14） |
| D5 | **Inspector 删除 clip 区** | `clip_data_section` 整体删除（含两个按钮与其调用点），Inspector 回到"只有节点参数"。`Link Data` / `Unlink` 迁入右键菜单（D8），否则失去唯一入口 |
| D6 | **右键菜单取代右键删除** | 命中 clip 时弹菜单，不再删除。模态进行中 / 刀片待命 / 框选进行中的右键仍然是**取消**，不弹菜单（保持 `timeline.rs:4037-4046` 的现有分支顺序）；空车道右键不弹（现状即无操作） |
| D7 | 菜单项与顺序 | `Mute`/`Unmute` → ─ → `Group Clips` / `Ungroup` → ─ → `Link Data` / `Unlink Data` → `Show in Data` → ─ → `Delete`（附 `Del` / `⌫` 提示）。Lock/Unlock 未来插在 `Delete` 之上（D12） |
| D8 | 菜单项可用性 | Mute/Unmute 恒可用（选区**全部**已静音 → 显示 `Unmute`，否则 `Mute`）；Group ≥2 选中；Ungroup 任一成员有 group；`Link Data` ≥2 且**同 lane**（沿用 Inspector 旧规则）；`Unlink Data` 任一成员 `refs > 1`；`Show in Data` 取选区首个；Delete 恒可用 |
| D9 | 右键与选区的交互 | 右键点在**选区内** → 保留整个选区（多选操作生效）；点在选区**外**的 clip 上 → 先单选它（清空其余）再弹菜单。这是所有 DAW 与 Blender 的共同约定，避免"菜单作用于你没看着的东西" |
| D10 | 面板内容 | 单表：`名称 / #id / 类型 / 内容量 / 长度 / 引用`。`内容量` = 音符数或锚点数；`长度` = `length_ticks` 的小节数；`引用` = 引用该数据的 clip 数。顶部过滤 `All / MIDI / Curve`；M2 加名称搜索框 |
| D11 | 引用列可点 | 点某行的引用数 → 选中所有引用该数据的 clip（跨轨、跨 lane 同类型），视图已在 `Timeline`。这是 `Show in Data` 的反向动作：一个从 clip 找数据，一个从数据找 clip，都只要一次点击 |
| D12 | Lock/Unlock 只留位 | 本次**不加字段、不做实现**，只在菜单顺序上预留位置。未来加 `locked: bool` 仍是 additive 字段，同样不动 `PROJECT_VERSION` |
| D13 | **删除键统一** | 时间轴 clip 与卷帘音符都接受 `Delete` **和** `Backspace`（Patch 已是两者）。文本框焦点时 Backspace 仍是退格，靠现有 `text_edit_focused()` 守卫 |
| D14 | 持久化边界 | 数据名称进工程文件（D1）；分栏开合 / 高度 / 当前过滤 / 高亮行都是 view state，不进工程、不改版本 |
| D15 | 孤儿只清不删 | 面板**不做逐行删除**。`refs == 0` 的行置灰，顶部 `Purge Unused` 调用现有 `compact_clip_data()`（索引重映射已有 `save_and_autosave_drop_orphan_pool_entries` 测过）。逐行删除会重排池索引，等于写第二份重映射逻辑 |
| D16 | 重命名交互 | 双击名称单元格行内编辑（复用 track rename 的 `TimelineState::rename` 模式）；Enter / 失焦提交，Esc 取消；提交空串 = 清除名称（回退 `#id`）。一步 undo，不重推引擎（名称是纯 UI 字段） |

## 3. 结构与 API

```rust
// glitcherz-model/src/lib.rs
pub struct MidiData {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,        // 新增，其余不变
    pub sequence: Sequence,
    pub length_ticks: u32,
}
pub struct CurveData {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub points: Vec<AutoPoint>,
    pub interp: CurveInterp,
    pub length_ticks: u32,
}

impl Project {
    /// 「引用该数据的所有 clip」，跨轨、按池类型。
    pub fn midi_data_users(&self, data: u32) -> Vec<(usize, usize)>;   // (track, clip)
    pub fn curve_data_users(&self, data: u32) -> Vec<(usize, usize)>;
    /// 池内孤儿数量（面板标题行与 Purge 按钮的禁用条件）。
    pub fn unused_data_count(&self) -> (usize, usize);                 // (midi, curve)
}
```

**UI 侧新增的 view state**（`GlitcherZApp`，不进工程）：

```rust
data_panel_open: bool,                      // D4 默认 false
data_filter: DataFilter,                    // All / Midi / Curve
data_search: String,                        // M2
data_rename: Option<(DataKind, u32, String)>,  // D16 行内编辑草稿
data_focus: Option<(DataKind, u32)>,        // Show in Data 的目标行高亮
```

**动作复用**：菜单不需要新命令，现有 `TimelineAction` 已覆盖全部条目——`SetClipMuted { clips, muted }`（`timeline.rs:677`）、`GroupClips` / `UngroupClips`（`:665/:670`）、`RemoveClipsBatch`（`:542`）。`Link Data` / `Unlink Data` 目前只在 Inspector 里直接调 `self.link_selected_clip_data()`，迁入菜单时改成与其它条目同形的 `TimelineAction::{LinkClipData, UnlinkClipData} { clips }`，保持"动作从 UI 收集、由 app 统一提交 undo"的单一路径。

**面板与工作区的唯一耦合**是 `data_focus`：`Show in Data` 写它 + 置 `data_panel_open = true`；面板读到后滚动到该行并高亮。

## 4. 接入面

| 层 | 文件 | 落点 |
| :--- | :--- | :--- |
| 模型字段 | `glitcherz-model/src/lib.rs` | `MidiData::name` / `CurveData::name`（D1）；`*_data_users` / `unused_data_count`（§3） |
| 面板 | 新 `glitcherz-ui/src/data_panel.rs` | 面板本体：标题行、表格、过滤、重命名、Purge、引用跳转 |
| 布局 | `timeline_ops.rs:80`（`timeline_panel`） | 在 `timeline::show(ui, ...)` **之前**声明 `Panel::bottom`（先占位，轨道区拿剩余高度——与 `editing.rs:565` 的顺序一致），面板通过 `show_switched` 折叠/展开 |
| Inspector | `inspector_panel.rs:66` | 删除 `clip_data_section(ui)` 调用 |
| 旧区块 | `timeline_ops.rs:2137-2200` | 删除 `clip_data_section`；`link_/unlink_selected_clip_data` 改签名或改为动作处理器 |
| 菜单 | `timeline.rs`（`lane_row`） | 右键分支改弹菜单（D6/D7/D9）；新增 `TimelineAction::{LinkClipData, UnlinkClipData, ShowInData}` |
| 删除键 | `timeline.rs:4080` / `piano_roll.rs:2789` | 加 `Key::Backspace`（D13） |
| 文档 | `docs/manual.md` / `docs/shortcuts.md` | 时间轴表里的右键说明、Data 面板一节、删除键行 |

## 5. 里程碑

| # | 内容 | 依赖 | 规模 |
| :--- | :--- | :--- | :--- |
| **M1** | 数据命名 + 时间轴上下分栏 + 只读列表（名称 / #id / 类型 / 内容量 / 长度 / 引用）+ 过滤 | — | 中 |
| **M2** | 重命名（行内）+ 引用列跳转 + `Purge Unused` + 名称搜索 | M1 | 中 |
| **M3** | Inspector 摘区 + clip 右键菜单（全部条目）+ 删除键统一 + 文档 | M1（`Show in Data` 依赖分栏） | 中 |

顺序：M1 → M2 / M3 可并行。M3 的 `Show in Data` 是唯一硬依赖 M1 的条目，其余菜单项只依赖已有的动作，可以先落。

## 6. 验收

1. 新建两个 clip 各拖一次，`Data` 面板出现两行、`引用` 各为 1、名称为 `MIDI #N`。
2. 双击名称改名 → 标题行、面板行、clip 头顶徽章的悬浮提示同步；`Ctrl+Z` 一步撤销；`Ctrl+Y` 恢复。
3. 改名后保存并重开工程，名称仍在；用 F-045 之前的旧工程打开，名称为空、显示 `#id`，无警告。
4. `Alt+D` 出一个共享副本 → 该行 `引用` 变 2；改名后两个 clip 都跟着变（共享同一 data）。
5. `Shift+D` 出新数据副本 → 面板多一行、名称与原行相同、`引用` 各为 1。
6. `Link Data` 合并两行 → 目标行 `引用` = 2，另一行变灰 `引用 0`；撤销后恢复两行各自引用。
7. `Purge Unused` → 灰行消失，所有 `clip.data` 仍指向正确内容（播放与未 purge 前一致），一步撤销。
8. 点某行引用数 → 所有引用者被选中；轨道区滚动到选区（视图已在 `Timeline`，不切换标签）。
9. 右键 clip（单选）→ 菜单七项，`Group` 灰、`Link Data` 灰；右键多选 → `Group` 可用；右键选区内的某一员 → 选区不被破坏。
10. 右键落在选区外的 clip → 选区先变成它，再弹菜单。
11. 菜单 `Mute` → 全部选中静音，再右键显示 `Unmute`；混合静音状态显示 `Mute`。
12. 菜单 `Delete` 与 `Delete` / `Backspace` 都删掉选区，各记一步 undo。
13. 框选中 / `G` 模态中 / 刀片待命时右键 → 仅取消，不弹菜单。
14. Inspector 在选中 clip 时不再出现任何 clip 数据区块。
15. 工程文件不含分栏状态（开合 / 高度 / 过滤）；`PROJECT_VERSION` 仍为 14。
16. 分栏只出现在 `Timeline` 视图：切到 `Edit` / `Patch` / `Inspector` 时不显示；切回时保持上次的开合与高度。拖分隔线到最小高度即折叠成标题行；点标题行可展开/收起。展开后轨道行与标尺仍可正常滚动、缩放，不被挤压变形。

## 7. 不做

- 数据面板的拖放（把数据拖到 clip 上）、data 目录 / 标签 / 收藏。
- 音频媒体池（F-041 D8 不动；音频 clip 引用文件路径，没有池）。
- 数据级自动化 / 数据级效果器。
- 跨类型共享（MIDI data 给 curve clip）——`Link Data` 仍限同 lane。
- `.001` 自动重名后缀（D2）。
- 池索引的稳定性承诺：`#id` 是显示槽位，压缩后会变，文档与 UI 都不把它当身份。
- Lock/Unlock 的实际实现（D12），只预留菜单位置。
- **跨视图常驻的全局 dock**：面板只属于 `Timeline` 视图（D3）；`Edit` 已有 Event List，`Patch` / `Inspector` 不挂数据表。

## 8. 风险

| 风险 | 说明 | 缓解 |
| :--- | :--- | :--- |
| 右键语义突变 | 从"点一下就删"变成"点一下弹菜单"，肌肉记忆会失效 | 删除键补 Backspace（D13）+ 菜单最后一项；文档与 `manual.md` 同步 |
| 分栏压缩编排高度 | 与 Event List 同样的取舍：展开后轨道区变矮 | 默认收起；默认只占 1/3、上限 60%；拖分隔线到最小即折叠；`Timeline` 是唯一受影响的视图 |
| 名称重名 | `Alt+D` 复制会得到同名行 | `#id` + 内容量区分；一行双击即可改（D2/D16） |
| 池索引漂移 | Purge 后 `#id` 全变，`data_focus` 可能指向已移动的行 | Purge 后清空 `data_focus`；行标识用 `(kind, index)` 只在同一帧内有效 |
| 表格性能 | 池上限 4096 条（`MAX_CLIP_DATA_ENTRIES`） | 只画可见行（`ScrollArea::show_rows`）；引用数按需算并缓存在帧内 |

## 9. 实施记录

### M1 · 2026-11-17 交付

**落了什么**

| 项 | 落点 |
| :--- | :--- |
| 数据名称 | `MidiData::name` / `CurveData::name`，`#[serde(default, skip_serializing_if = "String::is_empty")]`；`PROJECT_VERSION` 未动（仍 14）。构造点用 `..Default::default()` 或显式 `String::new()` 补齐 |
| 池查询 | `Project::midi_data_users` / `curve_data_users` / `unused_data_count`（`model/lib.rs`，紧邻已有的 `*_data_refs`） |
| 面板 | 新 `crates/glitcherz-ui/src/data_panel.rs`：`DataFilter`、`DataRow`、`rows()`、`summary()`、`show()` |
| 布局 | `timeline_ops.rs::timeline_panel`，`timeline::show` **之前**声明 `Panel::bottom`（D3） |
| 视图状态 | `GlitcherZApp::{data_panel_open, data_filter}`，默认 `false` / `All`（D4） |
| 文档 | `docs/manual.md` 新增「数据面板」一节 |

**与设计不同的地方**

1. **调用点在波形闭包之前**，不是笼统的「`timeline::show` 之前」：`timeline_panel` 里那个 `waveform_of` 闭包借了 `&self.sample_library` 活到 `timeline::show` 结束，之后没法再拿 `&mut self.data_panel_open`。面板因此放在闭包构造之前，语义仍是「先占高度、轨道拿剩余」。
2. **过滤用三个 `selectable_label`**（`right_to_left` 布局，阅读顺序 `All | MIDI | Curve`），不是 `ComboBox`：三个固定值不值得一次弹层；也避免像吸附下拉那样在测试里从外部点不到。
3. **引用数按帧建表**：`ref_counts()` 扫一遍所有 clip 得到 `Vec<usize>`，行查询是 O(1)。逐行调 `midi_data_refs` 是 O(行 × clip)，池到 4096 时会卡。与 §3 的「按需算并缓存在帧内」同义。
4. **`Length` 列按 4/4 折算**（`TICKS_PER_BAR`）：池里的数据没有自己的拍号，长度只是给行之间做比较用的量级，不参与任何放置。
5. **表格手绘列**（`add_sized` + `Label::truncate`）而非 `egui::Grid`：配 `ScrollArea::show_rows` 只画可见行（§8 的性能缓解），Grid 做不到按行切片。
6. **`data_search` / `data_rename` / `data_focus` 未加**：属于 M2，字段等实现时再进 `GlitcherZApp`，现在加空壳只会漂移。

**验收进度**（§6 编号）

- ✅ 1（面板出现两行、`引用` 各 1、名称为 `MIDI #N`）、15（工程不含分栏状态、版本仍 14）、16（只在 Timeline 出现、开合与高度保持、轨道与标尺照常滚动缩放）
- ⏳ 2–14 依赖 M2 / M3

**测试**：`data_panel` 5 个单测（空名回退、命名显示、孤儿计数、过滤、引用数与 `*_data_users`）；`cargo test -p glitcherz-model -p glitcherz-ui` 全绿；`cargo clippy -p glitcherz-ui --all-targets -- -D warnings` 零告警；`cargo check --workspace --all-targets` 干净（沙箱外）。

