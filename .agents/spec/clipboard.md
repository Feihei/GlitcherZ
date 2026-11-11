# 剪贴板与复制粘贴（Ctrl+C / Ctrl+V）

> ID: F-040 · 域: EDIT / PATCH / UI
> 状态：设计（2026-11-11）
> 关联：`timeline.md`、`editing-model.md`、`node-graph.md`、`snap.md`

## 1. 问题

当前各视图都没有应用级剪贴板：

- Timeline / Edit 的复制只有 `Shift+D`，落点由鼠标相对 grab 点决定，不能先复制、再移动播放头后粘贴；
- Patch 完全没有复制能力，只有 Delete；
- `Ctrl+C` / `Ctrl+V` 在全局键盘白名单里落入 `_ => continue`（`keyboard.rs`），既不处理也不消费。

用户期望：

| 视图 | 复制内容 | 粘贴锚点 |
| :--- | :--- | :--- |
| Timeline | 选中 clip（跨轨 / 跨 lane） | 播放头 |
| Edit（钢琴卷帘） | 选中音符 | 播放头（换算到当前 clip） |
| Patch | 选中节点 + 选中集内部连线 | 鼠标光标（graph 坐标） |

多对象粘贴一律**保持簇内相对偏移**：Timeline / Edit 用最早起点对齐锚点，Patch 用节点组包围盒左上角对齐锚点。

## 2. 决策

| ID | 问题 | 结论 |
| :--- | :--- | :--- |
| D1 | 剪贴板作用域 | **全局单剪贴板**：`GlitcherZApp.clipboard: Option<Clipboard>`，内容带类型标记。任何视图都可复制，但粘贴只在类型匹配的视图生效 |
| D2 | 存索引还是存值 | **存值快照**。undo / 删除会让 `ClipSel` 的 clip 索引、`RollState.selection` 的音符索引失效，剪贴板不能持有索引 |
| D3 | 键位与归属 | 各视图在自己的键盘块内 `consume_key(COMMAND, C/V)`：Timeline / Edit 沿用 `response.has_focus()`，Patch 沿用 `response.hovered()`（与 Delete 一致）。全局 `keyboard.rs` 白名单保持现状（只 `continue`、不消费），视图在后续绘制中仍能拿到事件 |
| D4 | macOS 兼容 | 用 `egui::Modifiers::COMMAND` 匹配：Windows / Linux 等价 Ctrl，macOS 为 Cmd。既有全局 Ctrl 白名单不动（其 Cmd 缺口是历史问题，不在本特性范围） |
| D5 | 空选 / 空剪贴板 | 不覆盖既有剪贴板；状态栏提示 `Copy: nothing selected` / `Paste: clipboard is empty`。类型不匹配提示 `Paste: clipboard holds <notes/clips/nodes>`，不改文档 |
| D6 | 文本输入冲突 | 焦点在文本输入框时视图不消费（`ctx.text_edit_focused()`），交给系统剪贴板——与 eframe 的 `clipboard` feature 分工不变 |
| D7 | Undo | 每次粘贴记**一步** undo：
- `Paste Clips` / `Paste Notes` / `Paste Nodes`；Shift+D 记 `Duplicate Nodes`。
- 不新增命令注册表条目，走既有编辑守卫（`begin_edit` / `commit_edit`，复合操作用 `session.suspend_history()`），因此不影响 MCP / CLI 的命令计数断言 |
| D8 | 跨工程 | 新建 / 打开 / 关闭工程时**清空剪贴板**（内容引用旧工程轨道与节点 id，跨工程粘贴无意义） |
| D9 | 明确不做 | Ctrl+X（cut 另立）、系统剪贴板互操作（文本 / 图片 / 文件）、跨视图类型转换（把 clip 粘贴成音符等） |

## 3. 剪贴板结构

```rust
enum Clipboard {
    Clips(Vec<ClipCopy>),
    Notes(NotesCopy),
    Nodes(NodesCopy),
}

struct ClipCopy {
    lane: TimelineLane,
    track: usize,
    start: u32,      // 绝对 tick，用于恢复簇内相对偏移
    clip: MidiClip | ControlClip | AudioClip, // 克隆，见 `timeline_duplicate_clips_batch`
}

struct NotesCopy {
    // clip-local tick，已减去最小 start；粘贴时再加锚点
    notes: Vec<Note>,
    min_start: u32,
}

struct NodesCopy {
    nodes: Vec<(NodeDoc, Pos2)>, // 值 + 复制时的 graph 位置
    cables: Vec<CableDoc>,       // 仅两端都在复制集合内的线
    bbox_min: Pos2,              // 包围盒左上角，粘贴时对齐
}
```

Timeline 的 clip 类型三分支（MIDI / Control / Audio）与 `timeline_duplicate_clips_batch` 一致；Audio 只克隆秒窗口，不复制 wav 文件。

## 4. 粘贴语义

### 4.1 Timeline

- 锚点 `anchor = engine.musical_ticks()`（绝对 tick，播放中即实时播放头）。
- `min_start = min(clip.start)`；每个副本 `new_start = anchor + (clip.start - min_start)`。
- 目标轨道 = 原 `track`；轨道越界（已被删除 / 跨工程）则跳过该项并计数。
- 每轨受 `MAX_CLIPS_PER_TRACK` 限制，超限跳过。
- 副本 append 到目标轨 clip 表末尾，粘贴后选区切换到副本（与 Shift+D 一致）。
- 通过 `sync_clip_row` 重推受影响的轨道。

### 4.2 Edit

- 锚点 `anchor_local = max(0, playhead_abs - clip.start_ticks)`，其中 `playhead_abs` 为 `engine.musical_ticks()`，clip 为当前 focus MIDI clip。
- `min_start = min(note.start)`（clip-local）；每个副本 `new_start = anchor_local + (note.start - min_start)`。
- 音符其余字段（音高、时长、力度、通道）原样保留。
- 复用 `midi.notes_duplicate` 的「原地复制 + 返回副本索引」能力：先按偏移改写副本 start，再一次性写入；粘贴后选区切到副本。
- 越过 clip 末尾的音符沿用现有音符编辑的边界规则，不改变 clip 时长。

### 4.3 Patch

- 锚点 = 指针的 graph 坐标（由 `PatchAction::Paste { at }` 携带）。
- 每个新节点 `node_id = graph.next_node_id()` 依序分配；`NodeDoc` 的 `node_id` 字段重写（model 新增 `NodeDoc::set_node_id`）。
- 位置 `new_pos = at + (old_pos - bbox_min)`，保证簇内相对位置不变。
- `cables` 只复制两端都在集合内的线；端点 id 映射到新 id，`kind` / `param` / `in_port` 原样保留。跨选择边界的外部连线不复制。
- 逐个 `install_node`（插件节点会重新实例化并沿用 `VoiceSpec` 里的 state）。任一节点安装失败（引擎槽满 / 插件加载失败）→ **整次粘贴回滚**文档并提示，保证文档与引擎一致。
- 粘贴后选区 = 新节点集合。
- `Shift+D` = 对当前选中集执行同一套复制 + 在指针处落位 + 选中副本，一次 undo；位置属于视图 layout，之后可继续用鼠标拖动（拖动不入 undo，与现状一致）。

## 5. 视图分工

| 层 | 职责 |
| :--- | :--- |
| 组件（`timeline.rs` / `piano_roll.rs` / `patch.rs`） | 只消费按键并返回意图：`CopySelection` / `Paste { at }` / `DuplicateAt { at }`；不读写剪贴板、不改文档 |
| app 层（`timeline_ops.rs` / `editing.rs` / `patch_ops.rs`） | 构造快照、写入剪贴板、用编辑守卫执行粘贴、切选区、必要时同步引擎 |
| `lib.rs` | 持有 `clipboard` 字段；新建 / 打开工程时清空 |

## 6. 里程碑

| 里程碑 | 内容 | 依赖 | 规模 |
| :--- | :--- | :--- | :--- |
| **M1** | 剪贴板结构 + `clipboard` 字段 + Timeline / Edit 的 Ctrl+C/V + 播放头锚点 + undo + 跨工程清空 | — | 中 |
| **M2** | Patch Ctrl+C/V + 节点 id 重写 + 内部连线复制 + `Shift+D` 复制到指针 + 插件失败回滚 | M1（共用 `Clipboard`） | 中 |

顺序：**M1 → M2**，逐里程碑单独提交。M2 依赖 M1 的剪贴板枚举，但两阶段改的是不同视图，可分开验证。

## 7. 验收

1. Timeline：跨轨选 2 个 clip → Ctrl+C → 移动播放头 → Ctrl+V：相对偏移不变、最早起点 = 播放头、选区切到副本、一步 undo。
2. Edit：选 3 个音符 → Ctrl+C → 移动播放头（含 clip 起点非零）→ Ctrl+V：最早音符贴播放头，三音相对关系不变，一步 undo。
3. Patch：选 2 节点及其间连线 → Ctrl+C → 指针移到空白处 → Ctrl+V：新 id、包围盒左上 = 指针、内部线保留、外部线不复制、一步 undo。
4. Patch `Shift+D`：复制到指针处并选中副本；插件节点重新实例化，失败时整次回滚。
5. 类型不匹配 / 空选 / 空剪贴板：只出状态栏提示，文档与 undo 栈不变。
6. 新建工程后 Ctrl+V：提示剪贴板为空。
7. 文本输入框内 Ctrl+C/V：行为不变（系统剪贴板）。

## 8. 测试

- `glitcherz-ui` 单测：剪贴板快照相对偏移计算（Timeline / Edit 纯函数）、类型不匹配分支、Patch id 重映射与内部连线筛选（纯函数）。
- 状态机测试：`roll_state_machine` 增加 Ctrl+C/V 产生意图、文本焦点时不产生意图。
- Timeline 交互测试：`timeline_focus.rs` 增加 Ctrl+C/V 意图测试。
- Patch：`patch.rs` 测试模块增加复制集合筛选（两端在集合内的线）与包围盒对齐。