# MIDI 命令族（Quantize / Legato / Glue / Remove Overlap / Transpose）

> ID: F-038 · 域: EDIT
> 状态：已交付（2026-11-11）
> 关联：`editing-model.md`（命令层）、`midi-generators.md`（randomize / strum）

## 1. 背景

音符级编辑（量化、连奏、粘合、去重叠、移调）此前只有 `glitcherz-model::ops` 里的纯函数或零散命令，没有统一 UI 入口，也无法被 CLI / MCP 稳定调用。本特性把它们收成可撤销、可枚举的命令族，并在钢琴卷帘提供 `Notes` 菜单与快捷键；纯函数仍是唯一实现，命令层只做寻址、选区与排序重建。

## 2. 命令

| id | 参数 | 语义 |
| :--- | :--- | :--- |
| `midi.quantize`（已有） | `grid_ticks?`、`strength? 0..1`、`swing?`、`include_within?`、`exclude_within?`、`quantize_ends?`、`preserve_duration?`、`random?` | 起点（可选尾端）吸附网格 |
| `midi.transpose`（已有） | `semitones` | 半音移调，clamp 0..127 |
| `midi.legato`（新增） | `gap? 0` | 按 `start` 排序，前音延长到 `下一个.start - gap`，末音不变 |
| `midi.glue`（新增） | `gap? 0` | 同 `note` + `channel`、间距 ≤ `gap` 的相邻音符合并到较早者 |
| `midi.remove_overlap`（新增） | — | 在后一个同音高音符起点截断前一个 |
| `midi.duration_op {op:"legato"\|"remove_overlap"}`（已有） | `value?`、`gap?` | 薄命令委托的底层入口，保留 |

共同约定：

- `indices?` 缺省 = 整个 clip；给定则只作用于这些音符。
- 全部返回 `{changed|merged|quantized|transposed, indices: [n]}`；`indices` 是操作后选区的新位置（命令会重排），UI 用它重建选区。
- 一次调用 = 一步 undo（拖动 / 按键手势由 UI 合并）。

## 3. 语义细节

### 3.1 legato

排序键 `(start, index)`；**只延长不缩短**；`gap = 0` 时前音尾端恰好落在后音起点。末音保持原长。

### 3.2 glue（唯一缩短音符表的 op）

`glue_selected(&mut Vec<Note>, GlueOptions { gap_ticks }, selected)`：

1. 选中项按 `(note, channel, start)` 排序；
2. 同 voice 且 `next.start <= cur.start + cur.length + gap` 时合并，`end` 取最晚；
3. 代表保留 velocity / channel，被吸收项删除；
4. 返回幸存值，命令层用 `locate` 换成新索引。

与 `notes_split` 同类例外：**不满足** `&mut [Note]` 不变长度契约，因此签名用 `&mut Vec<Note>` 并返回幸存值。

### 3.3 remove_overlap

`DurationOp::RemoveOverlap`：同音高且后音起点早于前音尾端时，前音长度截到后音起点（≥ 1）。与 glue 方向相反。

## 4. UI

`Notes` 菜单（卷帘工具栏）：

- 顶部 `strength`（0–1）与 `gap`（ticks）两个 `DragValue`，存在 `RollState::midi_strength` / `midi_gap`（view state，不进文档）；
- 菜单项：Quantize (`Q`) / Legato (`L`) / Glue (`Shift+G`) / Remove Overlap (`R`) / Transpose ±1 ±12；
- 选区为空时作用于整个 clip；
- `RollIntent::MidiOp { op, indices }` 只表达意图，参数由 `MidiOp` 变体携带（`Quantize { strength, grid_ticks }` / `Legato { gap }` / `Glue { gap }`），`editing.rs::apply_midi_op` 翻译成命令并回写 `indices`。

## 5. 明确不做

- 音阶量化（scale quantize）、按和弦 / 调性移调、glue 跨 clip。
- 把 `legato` 与引擎 `portamento`（滑音，`midi-nodes.md` §5.2）混为一谈：后者是音高连续滑变，不属于编辑命令族。
- 为 `remove_overlap` 增加跨选区边界的处理（只在选区内运算）。

## 6. 验收

- 命令单测：legato 延长 + undo；glue 合并 + indices；remove_overlap 截断；quantize 选区。
- MCP `commands_list` 包含全部 id（命令数 19）。
- `cargo fmt` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace` 全绿。