# clip 与数据分离（数据池 + 实例引用）

> ID: F-041 · 域: EDIT / PROJ / ENGINE
> 状态：设计（2026-11-11），M1 待实施
> 关联：`timeline.md`（D14 / D15 / D31 / D32）、`editing-model.md`、`audio-trim.md`（D1 是同一形状的先例）、`clipboard.md`（F-040 快照需随之扩展）、`roll-multiclip.md`

## 1. 问题

### 1.1 现状

三种 clip 的数据全部**内嵌、值语义**，没有任何跨 clip 共享：

| clip | 内容字段 | 位置 |
| :--- | :--- | :--- |
| `MidiClip` | `sequence: Sequence { notes, ctrls }` | `model/lib.rs:1846` |
| `ControlClip` | `curve: Vec<AutoPoint>` + `interp: CurveInterp` | `model/lib.rs:2107` |
| `AudioClip` | `path` + `source_offset` / `source_len`（秒） | `model/lib.rs:2007` |

- clip 没有稳定 ID，寻址一律靠 `(lane, track, index)`。
- 复制（`Shift+D` / `Ctrl+V`）是**深拷贝**：`timeline_ops.rs` 的 `.cloned()` 会把 `Vec<Note>` 整份复制。
- 没有 `ClipId`，也没有「引用另一个 clip」的字段。

### 1.2 用户需求

Blender 式 object / mesh 分离：**多个 clip 可以引用同一份 MIDI / curve 数据**，改一处全部变化，各 clip 的位置与窗口独立。

### 1.3 tile 与共享是正交的两条轴

现状的 tile **不是**「多个 clip 共用一份数据」：文档里只有一个 clip，靠 `span_ticks > loop_ticks` 让引擎按 `loop_ticks` 取模重复调度同一份内嵌内容。

| 能力 | 机制 | 能做到 | 做不到 |
| :--- | :--- | :--- | :--- |
| 平铺 | 单个 clip 内部循环 | 连续重复 N 遍 | 非连续复用、跨轨复用、A/B 段复用同一 pattern |
| 共享实例 | 多个 clip → 一份 data | 任意位置、跨轨、跨 lane 复用同一内容 | 单 clip 内部自动重复（仍需 span > length） |

因此本特性**不替代 tile**；`span > length` 时仍然循环（见 D3），tile 的行为保持不变。

## 2. 决策

| ID | 问题 | 结论 |
| :--- | :--- | :--- |
| D1 | 共享用什么载体 | **数据池 + 索引**：`Project.midi_data: Vec<MidiData>` / `Project.curve_data: Vec<CurveData>`，clip 只存 `data: u32`。否决 `Arc<Sequence>`（`Arc::make_mut` 在仍有引用者时会复制，反而断开共享；写入口漏改会让历史快照被就地改写）与「clip 引用 master clip」（主从级联语义绕，不支持跨轨自由复用） |
| D2 | `loop_ticks` 怎么办 | **退休**，拆成三处：数据长度 `data.length_ticks`（编辑范围）、`clip.source_offset`（窗口起点）、`clip.span_ticks`（窗口长）。播放位置 = `(source_offset + t) mod length_ticks` |
| D3 | 窗口能否超出数据长度 | **能**，超出部分按 `length_ticks` 取模循环填充。因此不需要独立的 `repeats` 字段；`span = N × length` 就是平铺，与现状等价 |
| D4 | 编辑共享数据的传播 | **影响所有引用者**（Blender 语义）。只有显式 Unlink 才能让当前 clip 独立 |
| D5 | trim / split | **非破坏窗口**。头裁 = `source_offset += delta`、`span -= delta`；尾裁只改 `span`；split 两半共享同一 data。数据永不因裁剪被删改 |
| D6 | 钢琴卷帘编辑什么 | 编辑的是 **data**，编辑范围 = `length_ticks`。clip 窗口在卷帘里画边界标线，窗口外内容降透明度但可编辑 |
| D7 | `ControlClip` | 同构改造：`CurveData { points, interp, length_ticks }`，clip 存 `source_offset` / `span` / `data`。`interp` 随数据走（共享时一起共享） |
| D8 | 音频 | **不动**。`AudioClip` 已经是「数据（wav 文件，由 path 共享）+ 窗口（秒级 offset / len）」，语义上已符合本模型；不引入媒体池 |
| D9 | 复制语义 | **`Shift+D` 共享 data**（instance duplicate，只复制 clip 标量）；**`Ctrl+C/V` 复制 data**（剪贴板是值快照，且必须能跨工程粘贴）；另给菜单项 `Duplicate with New Data` |
| D10 | 撤销 | **不改 undo 算法**。池是 `Project` 的普通字段，`Arc<Project>` 快照天然隔离；所有写入口直接改池元素，不需要 `make_mut` |
| D11 | 引擎 | `RtMidiClip` / `RtControlClip` 改成 `source_offset` + `length_ticks`；相位调度见 §4。引擎侧仍**每个 clip 各自一份 `Vec` 副本**（池共享的优化留到以后） |
| D12 | 影响面 | 改 data 时要重推**所有引用它的 track** 的 clip 表，不能只推当前 track。新增 `sync_clip_data(data_id)` 按引用扫描 |
| D13 | 文件版本 | `PROJECT_VERSION` 11 → 12。迁移：旧 `loop_ticks` → `length_ticks`，`source_offset = 0`，每个旧 clip **各自独占一份 data**，行为逐 tick 不变 |
| D14 | 池回收 | 允许孤儿数据。保存时用临时副本 compact（只影响落盘内容）；运行时池达到 `MAX_CLIP_DATA_ENTRIES`（4096）时做一次 compact + 索引重映射 |
| D15 | UI 入口 | clip header 徽章行追加共享徽章 + 引用数；Inspector 新增 clip 区块，给 `Link Data` / `Unlink`；Edit 头显示 `shared by N clips`。**不做独立数据面板**（留 M7） |
| D16 | 命令层 | M6 新增 `clip.link_data` / `clip.unlink_data`；同步更新命令计数断言（F-039 V3 的 23 → 25） |
| D17 | 明确不做 | 跨类型共享（MIDI data 给 curve clip）、独立数据面板（M7）、数据命名、播放中切换 data、音频媒体池、数据级自动化 |

## 3. 数据结构

```rust
pub const MAX_CLIP_DATA_ENTRIES: usize = 4096;

pub struct MidiData {
    pub sequence: Sequence,     // 复用现有 Sequence { notes, ctrls }
    pub length_ticks: u32,      // >= 1
}

pub struct CurveData {
    pub points: Vec<AutoPoint>,
    pub interp: CurveInterp,
    pub length_ticks: u32,      // >= 1
}

pub struct MidiClip {
    pub name: String,
    pub group: Option<u32>,
    pub muted: bool,
    pub start_ticks: u32,
    // F-041: window into `Project::midi_data[data]`
    pub source_offset: u32,     // normalized to [0, length_ticks)
    pub span_ticks: u32,        // >= 1; may exceed length_ticks (loops)
    pub data: u32,              // index into Project::midi_data

    // v11 legacy, read-only (same pattern as `legacy_notes` / `legacy_span_ticks`)
    #[serde(default, rename = "loop_ticks", skip_serializing)]
    pub legacy_loop_ticks: u32,
    #[serde(default, rename = "sequence", skip_serializing)]
    pub legacy_sequence: Sequence,
}

pub struct ControlClip {
    pub name: String,
    pub group: Option<u32>,
    pub muted: bool,
    pub start_ticks: u32,
    pub source_offset: u32,
    pub span_ticks: u32,
    pub data: u32,

    #[serde(default, rename = "loop_ticks", skip_serializing)]
    pub legacy_loop_ticks: u32,
    #[serde(default, rename = "curve", skip_serializing)]
    pub legacy_curve: Vec<AutoPoint>,
    #[serde(default, rename = "interp", skip_serializing)]
    pub legacy_interp: CurveInterp,
}

pub struct Project {
    // ...existing fields...
    #[serde(default)]
    pub midi_data: Vec<MidiData>,
    #[serde(default)]
    pub curve_data: Vec<CurveData>,
}
```

访问约定：

- clip 自身**不再有** `notes()` / `notes_mut()` / `curve` 字段。
- 统一走 `Project` 上的 helper：
  - `Project::midi_data_of(&self, clip: &MidiClip) -> Option<&MidiData>`
  - `Project::midi_data_mut(&mut self, clip_data: u32) -> Option<&mut MidiData>`
  - `Project::curve_data_of` / `curve_data_mut` 同理
- 先读 `clip.data` 再借池，避免 `midi_tracks[..].clips[..]` 与 `midi_data[..]` 的可变借用冲突。

## 4. 播放语义

### 4.1 MIDI

数据内音符满足现有边界规则：`0 <= n.start`、`n.start + n.length <= length_ticks`。clip 局部时间 `t ∈ [0, span)`。

对每个数据音符 `i` 与每个整数 `k`，其 clip-local 起点：

```
on(t) = n_i.start - source_offset + k * length_ticks
```

取所有与 `[0, span)` 有交的实例，并按窗口边界截断：

```
head = max(on, 0)
tail = min(on + n_i.length, span)
emit note-on at head with duration (tail - head) if tail > head
```

这覆盖三种情况：

- `source_offset = 0`：与现状逐 tick 一致；
- 头裁落在音符中间：从窗口起点发 note-on，长度截断（与音频裁剪一致）；
- `span > length`：同一音符按周期重复，等价于现有 tile。

note-off 沿用现有 pending 机制，按截断后的长度排程。

### 4.2 Control

```
phase = (source_offset + t) mod length_ticks
value = curve.value_at(phase)
```

`value_at_absolute` 的 `start + local % loop_ticks` 改为上式。曲线在数据周期边界的连续性维持现状（锚点只在 `[0, length)` 内）。

### 4.3 音频

不变。

## 5. 编辑语义

| 操作 | 新行为 |
| :--- | :--- |
| 头裁 | `source_offset = (source_offset + delta) rem_euclid length`；`span -= delta`；**不删音符** |
| 尾裁 | 只改 `span`；数据不变 |
| T 拉伸 | 只改 `span`（保持 `source_offset` 与 `length`），重复次数随之变化；删除 `resolve_stretch_loop` |
| Alt+T 平铺 | `period = data.length_ticks`；`span = period × repeats`；已平铺时只调 repeats |
| 切分 | 左：`span = cut`；右：`source_offset = (source_offset + cut) mod length`、`span = 原 span - cut`；两半共享 data；**取消「平铺 clip 不可切」限制** |
| `Shift+D` | 复制 clip 标量，**共享 data** |
| `Ctrl+C/V` | 快照携带数据内容，粘贴时**新建 data** 并指向它（跨工程安全） |
| `Duplicate with New Data` | 复制 clip + 复制 data |
| `Link Data` | 选中多个同类 clip → 全部指向选区首个的 data；各自 `source_offset` / `span` 不变 |
| `Unlink` | 复制当前 data 到池末尾，当前 clip 指向副本；一次 undo |
| 新建 clip | 新建一份空 data（`length_ticks = span`）并指向它 |
| 删除 clip | 只删 clip，不动 data（孤儿留给 D14） |

## 6. 接入面

已核对代码的改动点：

| 层 | 位置 | 改法 |
| :--- | :--- | :--- |
| model | `model/lib.rs` `MidiClip` 1846 / `ControlClip` 2107 / `Project` 6337 | 结构替换 + `midi_data` / `curve_data` + helper + `validate` + `approx_bytes`（按池统计） |
| model | `file.rs` `PROJECT_VERSION` 42 | 11 → 12 + `migrate_v11_to_v12` |
| model | `smf.rs` | 导出读池；按 `source_offset` / `span` 展开窗口 |
| engine | `realtime/sequencer.rs:1026` `schedule_track_clips` | 相位实例公式 |
| engine | `realtime/automation.rs:233` `value_at_absolute` | 相位取模 |
| engine | `RtMidiClip` / `RtControlClip` | `loop_ticks` → `length_ticks` + `source_offset` |
| command | `midi.rs:446` `clip_mut` / `:462` `notes_mut` | 改为经 `Project` helper 返回 `&mut MidiData`；24 条 `midi.*` 命令不动语义 |
| command | `clip.rs` `clip.add` | 建 clip 前先建 data |
| ui | `headless.rs:684` `midi_clip_table` | 读池；`RtMidiClip` 带 `source_offset` / `length_ticks` |
| ui | `timeline.rs:754` `Placement` / `:816` `retain_from` / `:985` `split_clip_in` / `:1261` `tile_targets` | 窗口语义（§5） |
| ui | `timeline_ops.rs` 复制 / 粘贴 / trim / split / tile | 见 §5；新增 `sync_clip_data` |
| ui | `piano_roll.rs` | 编辑范围改 `length_ticks`；窗口标线；窗口外降透明度 |
| ui | `clipboard.rs` | `ClipCopy` 携带 data 内容（见 §7） |
| ui | `recording.rs` | 录音落 clip 时新建 data |
| ui | `export_audio.rs` / `export_ops.rs` | 渲染路径经引擎，不直接读 notes |

`sync_clip_data`：给定 `data: u32` 与类型，扫描所有 track 的 clip，凡是 `clip.data == data` 的 track，重推整条 clip 表（`send_track_clips` / `send_control_clips`）。钢琴卷帘、Inspector、命令层编辑后统一走它。

## 7. 剪贴板耦合（F-040）

`clipboard.rs` 的 `ClipCopy` 目前是 `Midi(MidiClip)` 等值快照。改造后 clip 只含 `data` 索引，跨工程粘贴会指向不存在的池。因此：

```rust
enum ClipSnapshot {
    Midi(MidiClip, MidiData),
    Control(ControlClip, CurveData),
    Audio(AudioClip),
}
```

- 复制时连同 data 内容一起快照；
- 粘贴时 data 内容 push 进新工程的池，clip 指向新索引；
- 因此 `Ctrl+V` 天然是「复制数据」，与 D9 一致。

## 8. UI

| 位置 | 内容 |
| :--- | :--- |
| clip header 徽章行 | 追加 `shared:N`（仅 `refs > 1` 时显示）。**只用 ASCII 文本**，不用 `⛓` / `↔` 等码位（egui 默认 proportional 字体缺字形会静默变豆腐块，见 `●` U+25CF 的既有教训）。现有 `×N`、组名、`M` 的绘制顺序保持，共享徽章排在 `M` 之后 |
| Inspector | 新增 clip 区块（当前 Inspector 只显示节点参数）：名称、lane/track、`start` / `span` / `source_offset`、`length`、`shared by N clips`、`Link Data` / `Unlink` 按钮 |
| Edit 页头 | 现有 `"{track} · clip {n}"` 后追加 `shared by N clips`（仅 N > 1） |
| Timeline 右键菜单 | 追加 `Duplicate with New Data` |

`Link Data` 只在多选且同类型时可用；`Unlink` 只在单选且 `refs > 1` 时可用。两个按钮都走编辑守卫，各记一步 undo。

## 9. 里程碑

| 里程碑 | 内容 | 依赖 | 规模 |
| :--- | :--- | :--- | :--- |
| **M0** 文档 | 本 spec + `plan.md` 小节 + `implementation.md` 注册表 | — | 小（本次） |
| **M1** model | 数据池 + 两类 clip 结构 + helper + `validate` + `approx_bytes` + `PROJECT_VERSION 11→12` 迁移。**行为零变化**，单独提交并跑全量测试 | M0 | 大 |
| **M2** engine | `RtMidiClip` / `RtControlClip` 字段 + 相位调度 + 4 条数学单测（`source_offset` 非零 / 跨多周期 / 窗口截断音符 / curve 相位） | M1 | 中 |
| **M3** 接入 | `command` helper + `headless::midi_clip_table` + `sync_clip_data` + 剪贴板快照扩展 | M2 | 中 |
| **M4** 卷帘 | 编辑范围 `length_ticks`、窗口标线、窗口外降透明度、`RollSubject` 传 data 切片 | M3 | 中偏大 |
| **M5** timeline | `Placement` / trim / split / T / tile / 复制模式 / `×N` / seam | M3 | 中 |
| **M6** UI 入口 | header 徽章 + Inspector clip 区块 + Edit 头 + 右键菜单 + `clip.link_data` / `clip.unlink_data` + 文档 | M4、M5 | 中 |
| **M7** 数据面板（可选） | 数据池浏览 / 命名 / 批量替换引用 | M6 | 大 |

顺序：**M1 → M2 → M3 → M4 / M5（可并行）→ M6 → M7**。M1 必须单独提交，它的门禁是「v11 工程打开后播放 / 导出与旧版逐字节一致」。

## 10. 验收

1. v11 工程打开：播放、`--render` 导出与旧二进制逐字节一致（M1 门禁）。
2. 同轨两 clip `Link Data`：在 A 卷帘改音符，B 同步；播放同步。
3. 跨轨 `Link Data`：编辑同步；两轨播放位置互不影响。
4. 头裁共享 clip：另一 clip 的音符表不变；`source_offset` 回拖可还原。
5. 切分共享 clip：两半共享同一 data，改任一半两半都变。
6. `span > length`：循环填充；Alt+T 只改重复次数。
7. `Unlink` 后改一个不影响另一个。
8. `Shift+D` 共享；`Ctrl+C/V` 独立；`Duplicate with New Data` 独立。
9. undo / redo 正确恢复池内容与引用关系。
10. 存盘重开，共享关系保留（级联保存在同一 data 索引）。
11. Inspector 徽章与 `shared:N` 计数在 link / unlink / 删除 / undo 后正确刷新。

## 11. 测试

- **model**：v11 fixture 迁移（`loop_ticks` → `length_ticks`、`source_offset = 0`、data 独占）；`validate`（越界 data、`length = 0`、孤儿允许）；引用计数；compact 后索引重映射；`approx_bytes` 只计一份。
- **engine**：相位公式四组（见 M2）；`span` 跨 3 个周期；窗口起点落在音符中间；note-off 不重复发。
- **command**：24 条 `midi.*` 经池读写后 `query` 返回一致；`clip.add` 建 data。
- **ui**：`split_clip_in` 左右窗口计算、trim 非破坏、`tile_targets` 用 `length` 而非旧 `loop`、复制三模式、`ClipSnapshot` 跨工程粘贴。
- **端到端**：MCP `clip.add` → `clip.link_data` → `midi.note_add` → render 双轨同内容。

## 12. 风险

| 风险 | 处理 |
| :--- | :--- |
| 借用冲突（`clips[..]` 与 `midi_data[..]` 同时可变） | 先读 `clip.data` 拿到索引，再借池；不跨语句持有两个 `&mut Project` 子借用 |
| 漏推引用 track | `sync_clip_data` 是唯一入口，禁止直接用 `send_track_clips` 处理内容编辑 |
| 剪贴板跨工程指向失效索引 | `ClipSnapshot` 携带 data 内容（§7） |
| 池索引因 compact 失效 | compact 只在保存副本与上限回收时做，且同时重映射全部引用 |
| 老工程头裁内容不可恢复 | 迁移按 `source_offset = 0`；旧数据已被物理删除，属于既定损失 |
| 徽章字形缺失 | 只用 ASCII `shared:N` |