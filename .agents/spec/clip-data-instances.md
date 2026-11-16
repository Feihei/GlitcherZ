# clip 与数据分离（数据池 + 实例引用）

> ID: F-041 · 域: EDIT / PROJ / ENGINE
> 状态：M1–M6 已交付（2026-11-11）；2026-11-12 修订 T / 复制语义；2026-11-12 review 续作 **M9 / M10 / M11 已交付**（窗口内嵌 clip、`loop` 周期、`source_offset: i64`、T 纯窗口 + 写时增长、128 上限、v14 迁移、`tile N` 一次性复制），**M10.1 编辑范围 / 窗口跟随已交付**（卷帘编辑 `max(content, window)`、`window_follow`、`clip.fit_window`），**M10.2 卷帘对齐已交付**（数据坐标 offset 映射、指针行 floor、头空白写入），**M10.3 左扩展退役已交付**（T 只右扩、手动裁剪后编辑限于窗口），**M10.4 边界统一已交付**（分左右 fit），设计见 §13
> 关联：`timeline.md`（D14 / D15 / D31 / D32 / D33）、`editing-model.md`、`audio-trim.md`（D1 是同一形状的先例）、`clipboard.md`（F-040 快照需随之扩展）、`roll-multiclip.md`

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

> D1–D17 是 M1–M8 已交付的决策，保留作历史；D18–D27 是 2026-11-12 review 追加的目标决策。两者冲突处（窗口是否越界、周期如何表达、tile 的归属、复制粒度）以 D18–D27 与 §13 为准。

| ID | 问题 | 结论 |
| :--- | :--- | :--- |
| D1 | 共享用什么载体 | **数据池 + 索引**：`Project.midi_data: Vec<MidiData>` / `Project.curve_data: Vec<CurveData>`，clip 只存 `data: u32`。否决 `Arc<Sequence>`（`Arc::make_mut` 在仍有引用者时会复制，反而断开共享；写入口漏改会让历史快照被就地改写）与「clip 引用 master clip」（主从级联语义绕，不支持跨轨自由复用） |
| D2 | `loop_ticks` 怎么办 | **退休**，拆成三处：数据长度 `data.length_ticks`（编辑范围）、`clip.source_offset`（窗口起点）、`clip.span_ticks`（窗口长）；播放位置 = `source_offset + t`，仅在 `clip.repeat` 打开时才按 `mod length_ticks` 循环 |
| D3 | 窗口能否超出数据长度 | **能**，但默认**不循环**：超出部分是空白（2026-11-12 修订）。T 拉伸会把 `length_ticks` 同步扩展到窗口末端（头部扩展则前补空白），所以正常 T 之后窗口与内容对齐、卷帘可直接写新区域；只有手工窗口 / 旧迁移才会留下 `span > length` 的静默尾部。需要平铺时由 `Shift+T` 显式打开 `clip.repeat`，之后 `span = N × length` 才重复内容 |
| D4 | 编辑共享数据的传播 | **影响所有引用者**（Blender 语义）。只有显式 Unlink 才能让当前 clip 独立 |
| D5 | trim / split | **非破坏窗口**。头裁 = `source_offset += delta`、`span -= delta`；尾裁只改 `span`；split 两半共享同一 data。数据永不因裁剪被删改 |
| D6 | 钢琴卷帘编辑什么 | 编辑的是 **data**，编辑范围 = `length_ticks`。clip 窗口在卷帘里画边界标线，窗口外内容降透明度但可编辑 |
| D7 | `ControlClip` | 同构改造：`CurveData { points, interp, length_ticks }`，clip 存 `source_offset` / `span` / `data`。`interp` 随数据走（共享时一起共享） |
| D8 | 音频 | **不动**。`AudioClip` 已经是「数据（wav 文件，由 path 共享）+ 窗口（秒级 offset / len）」，语义上已符合本模型；不引入媒体池 |
| D9 | 复制语义 | **`Shift+D` 复制 data**（副本与原片段脱离，编辑互不影响）；**`Alt+D` 共享 data**（关联复制，Blender 的 linked duplicate）；`Ctrl+C/V` 复制 data（剪贴板是值快照，且必须能跨工程粘贴）；工具栏 `Duplicate` 与 `Shift+D` 同义。**没有 `ClipId`**：共享靠池索引 |
| D10 | 撤销 | **不改 undo 算法**。池是 `Project` 的普通字段，`Arc<Project>` 快照天然隔离；所有写入口直接改池元素，不需要 `make_mut` |
| D11 | 引擎 | `RtMidiClip` / `RtControlClip` 改成 `source_offset` + `length_ticks`；相位调度见 §4。引擎侧仍**每个 clip 各自一份 `Vec` 副本**（池共享的优化留到以后） |
| D12 | 影响面 | 改 data 时要重推**所有引用它的 track** 的 clip 表，不能只推当前 track。新增 `sync_clip_data(data_id)` 按引用扫描 |
| D13 | 文件版本 | `PROJECT_VERSION` 11 → 12；再 12 → 13（2026-11-12 修订加入 `repeat`）。v11→v12：旧 `loop_ticks` → `length_ticks`，`source_offset = 0`，每个旧 clip **各自独占一份 data**。v12→v13：`span > length` 的旧窗口全部标 `repeat = true`，保持旧平铺行为；新 clip 默认 `repeat = false` |
| D14 | 池回收 | 允许孤儿数据。保存时用临时副本 compact（只影响落盘内容）；运行时池达到 `MAX_CLIP_DATA_ENTRIES`（4096）时做一次 compact + 索引重映射 |
| D15 | UI 入口 | clip header 徽章行追加共享徽章 + 引用数；Inspector 新增 clip 区块，给 `Link Data` / `Unlink`；Edit 头显示 `shared by N clips`。**不做独立数据面板**（留 M7） |
| D16 | 命令层 | M6 新增 `clip.link_data` / `clip.unlink_data`；同步更新命令计数断言（F-039 V3 的 23 → 25） |
| D17 | 明确不做 | 跨类型共享（MIDI data 给 curve clip）、独立数据面板（M7）、数据命名、播放中切换 data、音频媒体池、数据级自动化 |
| D18 | 窗口两端越界（M10） | `source_offset` 改 `i64`；窗口 `[offset, offset+span)` 允许负起点（前导空白）与超出 `length`（尾部空白）；不再要求 offset ∈ [0, length) |
| D19 | 显式周期（M10） | `repeat: bool` → `loop_ticks: Option<u32>`；`None` 不循环，`Some(p)` 以 `p` 为周期；周期与 `data.length_ticks` 解耦，允许 `p != length` 与 `p > span` |
| D20 | T 纯窗口（M10） | T / 拖边只写窗口字段，**绝不改 `length_ticks`、不移动事件**；卷帘编辑范围 = 窗口，越界写入时把 `length` 增长到事件末端（写时增长），共享 data 在此时才 COW |
| D21 | 组织层按 data 分组（M9） | 批量复制 / 拉伸 / Unlink 以 data 为单元去重：同一 data 一次操作只克隆或 COW 一次，组内保持原共享结构；`Link Data` 是用户**显式合并**，选区统一指向首个成员的 data（不做分组）；`Unlink` 组内共享新副本 |
| D22 | 同步与保存补口（M9） | 新增 `sync_clip_data(kind, data)`（扫描全部引用 track 重推）；卷帘 / `midi.*` / Link / Unlink 后统一调用；`save_project` / `save_autosave` 在临时副本上 compact 后序列化 |
| D23 | 窗口内嵌 clip（M10） | **不拆 window 池**：只保留 `data` 池；`source_offset` / `span_ticks` / `loop_ticks` 与 `start_ticks` / `group` / `muted` / `name` 一起留在 clip 上。窗口永远私有，clip 间只共享 data |
| D24 | tile 是操作，不是类型（M11） | **放弃 `TileSpec` / `tile` 字段 / explode**：`tile N` = 把当前 clip 紧密复制 N 份（窗口字段各自一份、共享 data），生成 N 个普通 clip；不合成整体，用户自行 group |
| D25 | 复制语义三档（M11） | `Alt+D` 复制 clip、共享 data；`tile N` 与 `Alt+D` 数据语义相同，只是紧密排列 N 份；`Shift+D` 复制 clip + 复制 data；tile 是**一次性操作**，不记录份数、不识别已有序列，再执行就是继续追加 |
| D26 | 文件版本（M10） | `13 → 14`：旧 clip 字段升级（`source_offset` → i64、`repeat` → `loop_ticks`）；`repeat=true` 且 `offset = 0` 按周期展开成 N 个 clip（窗口字段各自一份、共享 data），非整数倍尾部另起一个截断 clip；`loop_ticks` 仅作旧态兼容保留 |
| D28 | 单轨 clip 上限（M10） | `MAX_CLIPS_PER_TRACK` 32 → 128，model 与 engine `MAX_TRACK_CLIPS` 同步；`SetMidiClips` / `SetControlClips` 载荷改 `Box<[...]>`（控制线程分配、实时侧 `mem::replace`、旧表进 trash），避免 `GraphUpdate` 变体增大超过 4 KiB 触发栈溢出 |
| D29 | 同轨区间选择（M9） | 单击 clip 设锚点；`Shift` + 单击同轨另一 clip 选中两者之间（按 `start_ticks` 排序）的全部 clip；`Ctrl` 保持追加；跨轨 `Shift` 退化为单选；区间选后仍走现有组扩选 |
| D30 | 编辑范围独立于窗口（M10.1） | 卷帘编辑范围 = `max(data.length, window end)`（数据坐标）：T 扩出的尾部可写，手动裁短后窗口外内容仍可编辑；窗口只是播放裁剪，不限制内容编辑 |
| D31 | 窗口自动跟随（M10.1） | `window_follow` 默认对新 clip 开启；T / 拖边 / trim 关闭。跟随期间窗口末端 = 最后一个 note-off 向上取整到小节（至少 1 小节）；`clip.fit_window` 重新开启并立即按内容末端调整 |
| D32 | 禁止空窗口（M10.1） | 手动缩小 / 移动窗口（T、拖边、左缘 trim）后必须至少有一个音符与窗口相交；数据本身为空时豁免，便于新建 clip 先准备内容 |
| D33 | 卷帘以数据坐标对齐窗口（M10.2） | 卷帘绘制 / 命中按数据 tick（`note.start`），`x_of` 减 `source_offset`、`local_at` 加回；播放头用窗口局部 tick 再加 offset 绘制。窗口移动后音符相对窗口移位、绝对试听位置不变 |
| D34 | 指针行与头空白写入（M10.2） | 指针 y 用 `floor` 落到所在行（不再四舍五入到下一音）；前导空白可用负数据 tick 写入：`midi.note_add` 接受负 `start_ticks`，`grow_midi_data_to_notes` 把整份内容右移 `shift`、`data.length += shift`、`source_offset += shift`，绝对位置不变（共享先 COW） |
| D35 | T 左扩退役（M10.3） | `T` / `stretch_targets` 只改右缘；`S` 左缘资格键移除；需要左扩时改为「音符整体右移 + clip 左移（或先右扩）」；左缘 trim（向内裁）保留 |
| D36 | 跟随与编辑范围（M10.3） | `window_follow = true` 时编辑范围 `max(content, window)`，写音符自动撑窗口；手动裁剪后 `window_follow = false`，编辑范围限于窗口，右侧必须先 T 右扩才能写；窗口外音符不绘制 |
| D37 | 边界操作统一（M10.4） | 窗口只保留边界拖动（自由裁剪 / 放宽）与自适应；`clip.fit_window {edge: left/right/both}` 以**窗口内**第一个 note-on / 最后一个 note-off 为基准，左向下、右向上取整到小节；fit 不改变 `window_follow`（持续跟随仍默认开启） |
| D38 | 边界视觉区分（M10.5，2026-11-16 修订） | clip 内画内容范围细线（首 note-on..末 note-off；control 为首末 anchor），窗口边缘裁进内容时沿被裁边缘画**全高锯齿带**（原两像素小齿肉眼难辨，与普通边缘无异）；fit 不裁内容 → 无锯齿，手动裁剪裁到内容 → 有锯齿；不新增状态 |
| D39 | 窗口上限与编辑范围（M10.6） | T / 右缘扩展上限 = 最后一个事件所在小节的末端；写入不自动扩窗口；卷帘编辑范围 = `max(content, window)`，窗口外内容可编辑；fit 仍是一次性手动操作 |
| D40 | 左裁剪双向可恢复（M10.7） | 左缘拖动与 `T` 的 start 边（`S` 翻转）支持双向：向右裁深、向左恢复；恢复上限 = 数据起点（`source_offset` 最低到 0，被裁掉的前导空白已不存在）；`trim_head_start` 下限从 origin_start 改为 0；`retain_from` 接受负 delta，model 层钳制恢复边界；M10.3 的「左扩」仍不回归——恢复的只是此前裁掉的部分 |
| D41 | 写入自动跟随右缘（M10.8，2026-11-16 修订） | 所有 `midi.*` 写入后（`grow_midi_data_to_notes`）：`data.length` 收敛到事件末端、`span_ticks` 直接设为 `最后 note-off 向上取整到小节 − source_offset`——**双向跟随**（M10.10 修订：音符缩短 / 删除后窗口同步收缩，不再保留超长幽灵窗口；上限公式与 timeline 的 stretch/resize 统一为数据坐标取整）；修订 M10.6 的「写入不自动扩窗口」 |
| D42 | 卷帘编辑范围 = 可见空白（M10.9，2026-11-16 修订） | 创建 / 拖长音符的右界 = 可见右缘（不再 clamp 到 `loop_ticks`）：在 clip 右缘之外的空白处点击 / 拖动直接写入，窗口随后按 D41 自动跟随；但可写区止于**右侧最近的 ghost clip 左缘**——ghost 身体永远是 no-op（写入会让本 clip 扩窗盖住邻居，P3 D2 保留）；网格绘制覆盖可见区域；`window_follow` 关闭时同样成立（写入即扩窗，覆盖 D36 的「先 T 右扩才能写」） |
| D43 | 窗口边界钳制（M10.10） | 左缘拖动钳制在**数据起点**：`retain_from` 返回 applied（实际移动量，`source_offset=0` 后恢复量为 0），clip start/span 按 applied 推导——拖过数据起点不再整体左滑 + 右缘膨胀（audio 同规则，钳在素材头）；右缘拖动 / `T` 扩展钳制在**最后内容小节末端**（`content_span_cap`，遗留超限窗口扩展为 no-op、缩短自由）；head trim 应用层不再用指针绝对值覆盖 start/span |
| D27 | 本次不做 | `TileSpec` / explode / 嵌套 tile / 记录 tile 份数、per-repeat override、自动 group、音频 tile、独立 window 面板（归 M7 扩展）、`ClipId` |

## 3. 数据结构

> 以下是 M1–M8 已交付的结构（`repeat: bool`、`source_offset: u32`）。M10 的目标结构是 §13.2 的 `MidiData` + `MidiClip`（窗口字段内嵌，不拆池）；tile 是操作而非结构（§13.5），迁移规则见 §13.6。

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
    pub span_ticks: u32,        // >= 1; may exceed length_ticks
    pub repeat: bool,           // v13: loop the content past length_ticks
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
    pub repeat: bool,           // v13: same flag, curve window
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

播放位置：`repeat = true` 时 `(source_offset + t) mod length_ticks`；否则 `source_offset + t`，且只播放到 `length_ticks` 为止（超出部分是空白）。对每个数据音符 `i` 与每个整数 `k`，其 clip-local 起点：

```
on(t) = n_i.start - source_offset + k * length_ticks
```

`repeat = false` 时只取 `k = 0`，窗口有效终点为 `min(span, length_ticks - source_offset)`；长音符尾部在内容末端截断。`repeat = true` 时取所有与 `[0, span)` 有交的实例：

```
head = max(on, 0)
tail = min(on + n_i.length, span)
emit note-on at head with duration (tail - head) if tail > head
```

这覆盖三种情况：

- `source_offset = 0`：与现状逐 tick 一致；
- 头裁落在音符中间：从窗口起点发 note-on，长度截断（与音频裁剪一致）；
- `span > length` 且 `repeat = false`：内容只播一次，尾部静默；
- `span > length` 且 `repeat = true`：同一音符按周期重复，等价于现有 tile。

note-off 沿用现有 pending 机制，按截断后的长度排程。

### 4.2 Control

```
phase = (source_offset + t) mod length_ticks    // repeat = true
phase = source_offset + t                       // repeat = false, t < length - source_offset
value = curve.value_at(phase)
```

`repeat = false` 且窗口超出内容长度时，超出部分返回 `None`（目标保持最后值）。`value_at_absolute` 的 `start + local % loop_ticks` 改为上式。曲线在数据周期边界的连续性维持现状（锚点只在 `[0, length)` 内）。

### 4.3 音频

不变。

## 5. 编辑语义

> 以下是 M1–M8 已交付的行为（T / 拖边会改 `length_ticks`，头扩会移动事件）。M10 起以 §13.3 为准：T / 拖边只写窗口，事件与 `length` 不动，越界写入才触发写时增长；M11 起 tile 上的 T 只改 `count`。

| 操作 | 新行为 |
| :--- | :--- |
| 头裁 | `source_offset = (source_offset + delta) rem_euclid length`；`span -= delta`；**不删音符** |
| 尾裁 | 只改 `span`；数据不变 |
| T 拉伸 | 改 `span` 并清 `repeat`；尾部扩展同时把 `length_ticks` 扩到窗口末端，头部扩展（`T+S` 向左）给数据前补空白（所有音符 / 锚点右移、`length` 增长），所以内容保持原时间位置且新区域可写；共享 data 先自动复制一份（2026-11-12 修订） |
| Alt+T 平铺 | `period = data.length_ticks`；`span = period × repeats`；`repeat = true`；已平铺时只调 repeats |
| 切分 | 左：`span = cut`；右：`source_offset = (source_offset + cut) mod length`、`span = 原 span - cut`；两半共享 data 且继承 `repeat`；**取消「平铺 clip 不可切」限制** |
| `Shift+D` | 复制 clip 标量，**复制 data**（脱离共享） |
| `Alt+D` | 复制 clip 标量，**共享 data** |
| `Ctrl+C/V` | 快照携带数据内容，粘贴时**新建 data** 并指向它（跨工程安全） |
| `Link Data` | 选中多个同类 clip → 全部指向选区首个的 data；各自 `source_offset` / `span` / `repeat` 不变 |
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
| Timeline 右键菜单 | 追加 `Duplicate with New Data`（当前未做：`Shift+D` 已是独立复制，右键菜单并入后续） |
| 快捷键 | `Shift+D` = 复制 + 独立数据；`Alt+D` = 复制 + 共享数据（Blender linked duplicate，视图级扫描，Alt 丢指针也能进入模态）；工具栏 `Duplicate` 与 `Shift+D` 同义 |

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
| **M8**（2026-11-12 修订） | `repeat` 标记 + v12→v13 迁移 + 引擎/预览非循环尾部静默 + `Shift+D`/`Alt+D` 复制语义 + T 清 `repeat` / Tile 设 `repeat` | M6 | 中 |
| **M9** 收敛补口（2026-11-12 review，无迁移） | `sync_clip_data` + 卷帘 / 命令 / Link 调用点；保存与自动保存 temp compact；批量复制 / 拉伸按 data 分组 COW；`Link Data` 按原 data 分组合并、`Unlink` 逐个展开；同轨 `Shift` 区间选择；`ClipTileEdit.loop_ticks` 改名 `period_ticks`（纯改名） | M8 | 中 |
| **M10** 窗口内嵌 + T 纯窗口（2026-11-12 review） | clip 内嵌窗口字段：`source_offset: i64`、`repeat` → `loop_ticks: Option<u32>`（**不拆 window 池**）；单轨上限 32 → 128 + 结构队列 `Box` 载荷；T / 拖边纯窗口 + 卷帘写时增长；引擎负相位与显式周期；**一次** v13→v14 迁移（含平铺 → N 个 clip）；零回归门禁 | M9 | 中 |
| **M11** tile N 操作（2026-11-12 review） | `Shift+T` 一次性把当前 clip 紧密复制 N 份（窗口字段各自一份、共享 data），**不记录份数、不识别已有序列**；无 `TileSpec`、无 explode；切刀即普通 split；命令 `clip.tile` | M10 | 中 |

顺序：**M1 → M2 → M3 → M4 / M5（可并行）→ M6 → M7**；review 续作按 **M9 → M10 → M11**，M9 可先单独交付，M10 与 M11 之间不插其它文件格式变更。M1 必须单独提交，它的门禁是「v11 工程打开后播放 / 导出与旧版逐字节一致」；M10 的门禁同形——「v13 工程打开后播放 / `--render` 与 M9 二进制逐字节一致」。

## 10. 验收

1. v11 工程打开：播放、`--render` 导出与旧二进制逐字节一致（M1 门禁）。
2. 同轨两 clip `Link Data`：在 A 卷帘改音符，B 同步；播放同步。
3. 跨轨 `Link Data`：编辑同步；两轨播放位置互不影响。
4. 头裁共享 clip：另一 clip 的音符表不变；`source_offset` 回拖可还原。
5. 切分共享 clip：两半共享同一 data，改任一半两半都变。
6. `span > length` 且 `repeat` 打开：循环填充；Alt+T 只改重复次数；`repeat` 关闭且窗口超出内容时超出部分静默。T 尾部扩展后可直接在卷帘新区域写音符；`T+S` 头部扩展后原音符仍在原位、新头部可写，且共享 data 的 T 拉伸不影响其它引用者。
7. `Unlink` 后改一个不影响另一个。
8. `Shift+D` 独立；`Alt+D` 共享；`Ctrl+C/V` 独立。
9. undo / redo 正确恢复池内容与引用关系。`repeat` 也随快照回滚。
10. 存盘重开，共享关系与 `repeat` 保留（级联保存在同一 data 索引）。
11. Inspector 徽章与 `shared:N` 计数在 link / unlink / 删除 / undo 后正确刷新。
12. **M9**：跨轨共享同一 data，在 A 轨卷帘改音符后 B 轨下一 block 即按新内容发声（不需要切轨 / 存盘）。
13. **M9**：保存含孤儿 data 的工程，落盘后池长度等于实际引用数；重开工程引用关系不变。
14. **M9**：`Shift+D` 复制一个组，组内原本共享同一 data 的成员在副本中仍共享；`T` 批量拉伸同类组同样不拆关联。
15. **M9**：同轨单击 A 再 `Shift` 单击 B，选中 A、B 之间按 `start` 排序的全部 clip；跨轨 `Shift` 退化为单选；选区触及组时仍扩到整组。
16. **M10**：`T` 尾部扩展后 `data.length_ticks` 不变；新尾部在卷帘写入音符时才增长；`T+S` 头部扩展后所有事件绝对位置不变（`source_offset` 变负）。
17. **M10**：窗口起点早于数据（负 offset）时前导静默；窗口尾部超出数据时尾部静默；`loop_ticks = Some(p)` 且 `p != length` 时按 p 重复。
18. **M10**：单轨可容纳 128 个 clip；`SetMidiClips` / `SetControlClips` 经 `Box` 载荷下发，`GraphUpdate` 变体不随上限线性增大；v13 工程播放 / 导出零回归。
19. **M11**：`tile N` 生成 N 个紧密相邻 clip，窗口字段各自一份、共享 data；改任一份的 events 全部同步，改窗口只影响自己；再执行 `tile N` 是继续追加（不识别、不记录份数）；`K` 切开后是普通 split（无特判），单周期段自然就是普通 clip；一步 undo。

## 11. 测试

- **model**：v11 fixture 迁移（`loop_ticks` → `length_ticks`、`source_offset = 0`、data 独占）；`validate`（越界 data、`length = 0`、孤儿允许）；引用计数；compact 后索引重映射；`approx_bytes` 只计一份。
- **engine**：相位公式四组（见 M2）；`span` 跨 3 个周期；窗口起点落在音符中间；note-off 不重复发。
- **command**：24 条 `midi.*` 经池读写后 `query` 返回一致；`clip.add` 建 data。
- **ui**：`split_clip_in` 左右窗口计算、trim 非破坏、`tile_targets` 用 `length` 而非旧 `loop`、复制三模式、`ClipSnapshot` 跨工程粘贴。
- **端到端**：MCP `clip.add` → `clip.link_data` → `midi.note_add` → render 双轨同内容。
- **M9**：`sync_clip_data` 覆盖「同轨多引用 / 跨轨多引用 / 孤儿 data / compact 后索引重映射」四种情形；组复制与组拉伸的 data 去重各一条；保存临时副本 compact 不改变内存索引；同轨 `Shift` 区间选择（含跨轨退化、组扩选叠加、锚点失效）。
- **M10**：v13 fixture 迁移（`repeat=false` → 1 个 placement；`repeat=true` 且 `offset=0` → q 个整周期 + 尾部截断；`offset!=0` → `loop_ticks=Some` 兼容）；窗口两端越界（负 offset / 尾超长）引擎静默；卷帘写时增长只 COW 被写的那一份；`loop_ticks != length` 的相位；128 上限与 `Box` 载荷的模型 / 引擎一致性断言。
- **M11**：`tile N` 的 N=1 等价 no-op、槽位不足时按可用份数生成并提示、重复执行只追加；`K` 切序列的普通 split；undo 一次回滚整批。

## 12. 风险

| 风险 | 处理 |
| :--- | :--- |
| 借用冲突（`clips[..]` 与 `midi_data[..]` 同时可变） | 先读 `clip.data` 拿到索引，再借池；不跨语句持有两个 `&mut Project` 子借用 |
| 漏推引用 track | `sync_clip_data` 是唯一入口，禁止直接用 `send_track_clips` 处理内容编辑 |
| 剪贴板跨工程指向失效索引 | `ClipSnapshot` 携带 data 内容（§7） |
| 池索引因 compact 失效 | compact 只在保存副本与上限回收时做，且同时重映射全部引用 |
| 老工程头裁内容不可恢复 | 迁移按 `source_offset = 0`；旧数据已被物理删除，属于既定损失 |
| 徽章字形缺失 | 只用 ASCII `shared:N` |
| M10 后借用仍只有 clip 与 data 两层 | 不为窗口另建池：窗口字段直接从 clip 读；禁止引入 `Project::*_window_of` 一类间接寻址 |
| `source_offset: i64` 溢出 / 负值落盘 | serde 支持 i64；`validate` 限制 `abs(offset) ≤ MAX_ARRANGEMENT_TICKS`，引擎按 i64 运算后再 clamp |
| 上限 32 → 128 让 `GraphUpdate` 变体增大 | `SetMidiClips` / `SetControlClips` 改 `Box<MidiClipTable>` / `Box<ControlClipTable>`：控制线程分配，实时侧 `mem::replace` 后旧表进 trash 队列，`GraphUpdate` 只带指针 |
| tile N 受 128 槽位限制 | 剩余槽位不足时按可用份数生成并提示；不自动扩容；超大批量分段执行 |
| tile N 生成大量 placement 后的绘制 / 选择开销 | 128 槽位封顶；沿用逐块绘制，不引入折叠容器 |
| 同轨区间选择与组扩选叠加成超集 | 先算区间，再走现有 `expand_group_selection`；`Alt+单击` 仍是单成员出口；文档写清「区间选之后组仍扩」 |
| M10 迁移把平铺展开成多个 placement 后显示突变 | 迁移表逐项断言；整数倍展开成 N 块、非整数倍尾部另起截断块，播放与旧版逐字节一致 |
| `write-time growth` 让卷帘编辑悄悄改 data 长度 | 只对焦点 clip 的 data 增长；共享时先 COW；一步 undo 覆盖「COW + 增长」 |

## 13. 2026-11-12 review：窗口内嵌 clip + tile N 操作

### 13.1 触发问题：四种职责仍压在 clip 上

M1–M8 只重构了「内容」。`MidiClip` / `ControlClip` 现在同时承担四种职责，这是 review 中所有交互冲突的共同根因：

| 职责 | 现状字段 | 残留问题 |
| :--- | :--- | :--- |
| 内容引用 | `data` | 已是新模型 |
| 窗口 | `source_offset` / `span_ticks` / `repeat` | 文档不变量是 `offset ∈ [0, length)`，但 `retain_from` 用 `saturating_add` 不取模（`timeline.rs:795`）；头越界只能靠改数据 |
| 放置 | `start_ticks` | 仍是旧模型：位置与窗口绑在一起，tile 无法引用它 |
| 组织 | `group` / `muted` / `name` | 挂在 clip 上，与数据共享无关，但会被复制 / 拆分一起搬走 |

其他新旧混杂的证据：

| 位置 | 现象 |
| :--- | :--- |
| `timeline_ops.rs:2293` | `T+S` 头扩右移全部音符并增长 `length_ticks`，用「改内容」模拟「改窗口」；共享 data 先 COW → 一次拉伸拆掉关联 |
| `timeline_ops.rs:2407` | 拖右缘同样增长 `length_ticks`，窗口操作产生内容副作用 |
| `timeline.rs:144` | `ClipTileEdit.loop_ticks` 仍用已退休字段名，落定时只写 `repeat = true`，类型与语义脱节 |
| `editing.rs:742` / `:1176` | 卷帘编辑后只 `send_track_clips(track)`；spec §6 要求的 `sync_clip_data` 从未实现 |
| `file.rs:144` | `save_project` 直接序列化，D14 的「保存时 compact」未实现，孤儿 data 写盘 |
| `timeline_ops.rs:1685` | `Shift+D` 复制组时逐成员克隆 data，组内原本共享的成员在副本里被拆散 |
| `clip.rs:111` | `Link Data` 把多选全部并成一份 data，抹掉原有分组；`Unlink` 却只作用于首个选区 |

结论：**内容层已重构，窗口层半成品，放置层与组织层还是旧模型。**

### 13.2 目标分层

```
L0 MidiData / CurveData / wav file     内容（可被多个 clip 共享）
L1 MidiClip / ControlClip              窗口 + 放置 + 组织（data 引用 + offset/span/loop + start + group/muted/name）
```

```rust
pub struct MidiClip {
    // window: private to this clip, never shared
    pub data: u32,               // L0 pool index
    pub source_offset: i64,      // may be negative: leading blank
    pub span_ticks: u32,         // >= 1
    pub loop_ticks: Option<u32>, // None = play once; Some(p) = period p (legacy only)

    // placement + organization
    pub start_ticks: u32,
    pub group: Option<u32>,
    pub muted: bool,
    pub name: String,
}
```

`ControlClip` 同构（`curve_data` 池 + 同样字段）。音频不动（`path` + 秒级窗口 + `start`）。

**不设 window 池。** 窗口字段跟着 clip 走，clip 间只共享 `data`。此前把窗口独立成池，是为了支持「tile 引用 clip」——tile 已改为一次性复制操作，这个理由不再存在；保留窗口池反而会让「clip 的起止」和「数据取用区间」混淆。

不变量：

- `span >= 1`；`source_offset` 允许任意符号，但 `|source_offset| ≤ MAX_ARRANGEMENT_TICKS`；
- `loop = Some(p)` 时 `p >= 1`；允许 `p > span` 与 `p != data.length`；`loop` 只服务旧态兼容与窗口内循环，新交互不再主动写入；
- `tile N` 产物满足：N 个 clip 的 `start` 依次相差 `span`、`data` 相同、窗口字段值相同（但各自独立）；
- 术语：UI 一律说 clip；`window` 只指 clip 上 `source_offset` / `span_ticks` / `loop_ticks` 这组字段，不是独立实体。

### 13.3 播放与编辑公式

设 clip 局部时间 `t ∈ [0, span)`；每个 clip 独立调度，`tile N` 产出的 N 份就是 N 个 clip：

```
window_pos = source_offset + t
loop = None        → 只播放 window_pos ∈ [0, length) 的部分；两端越界静默
loop = Some(p)     → phase = window_pos.rem_euclid(p)，再按 p 的实例展开
```

- 卷帘编辑范围 = 窗口覆盖的数据区间 `[source_offset, source_offset+span)`；写入落在 `[0, length)` 之外时：先把 `length` 增长到事件末端，若 `refs > 1` 先 COW —— **这是唯一会因编辑内容而改 `length` 的路径**。
- 头扩（`T+S`）只减小 `source_offset`，事件 tick 不动；负 offset 的前导区间在卷帘里显示为空白，可写。
- `T` / 拖边清 `loop_ticks = None`；窗口编辑只影响当前 clip（`tile N` 的各份窗口字段互相独立）。
- `loop_ticks` 与 `data.length` 完全解耦：改 data 长度不再隐式改变任何 clip 的节奏。

### 13.4 M9：收敛补口（无文件迁移）

| 改动 | 位置 | 规则 |
| :--- | :--- | :--- |
| `sync_clip_data` | `engine_sync.rs` / `timeline_ops.rs` | 按 `(kind, data)` 扫描全部 track，凡有 `clip.data == data` 的行重推整表；内容编辑只许走它 |
| 调用点 | `editing.rs` / `command/midi.rs` / `clip.rs` | 卷帘提交、`midi.*`、`clip.link_data` / `unlink_data` 之后 |
| 保存 compact | `file.rs` | `save_project` / `save_autosave` 先在 `Project` 临时副本上 `compact_clip_data()`，内存索引不变 |
| data 分组 COW | `timeline_ops.rs` | 批量复制 / 拉伸前把 data 索引分组；同组只克隆一次，副本共享同一新索引 |
| Link / Unlink | `clip.rs` / UI | Link 是显式合并：选区统一指向首个成员的 data；Unlink 按原 data 分组，一次克隆、组内共享新副本；组标签不参与 |
| 同轨区间选择 | `timeline.rs` | 单击设 `selection_anchor`；`Shift` + 单击同轨另一 clip 选中两者之间（按 `start` 排序）的全部 clip；`Ctrl` 保持追加；跨轨退化单选 |
| 命名清理 | `timeline.rs` | `ClipTileEdit.loop_ticks` → `period_ticks`；注释里「tiled clip == repeat」的旧描述改成分层表述 |

M9 **不改** `source_offset: u32`、不改 `repeat: bool`、不做 v14 —— 因此 T 的窗口化留到 M10 一次性处理，避免两次迁移。

### 13.5 M10 / M11：窗口内嵌与 tile N 操作

**M10（结构 + 迁移）**

1. model：clip 内嵌窗口字段——`source_offset: i64`、`repeat: bool` → `loop_ticks: Option<u32>`；**不新增 window 池**，`validate` / `approx_bytes` / 引用计数 / compact 仍只覆盖 data 池。
2. 保留 `MidiClip` / `ControlClip` 类型名（概念上它就是 placement，不拆类型）。
3. `timeline::Placement` trait 保持现有字段集合；窗口读写继续走 clip 自身，不引入 `Project::*_window_of`。
4. `T` / 拖边 / trim / split 全部改写为窗口语义（§13.3）；删除 `stretch_*_clip` 的 prepend 与 `length` 扩展。
5. 卷帘：编辑范围 = 窗口；越界写入触发写时增长（§13.3）。
6. engine：`RtMidiClip` / `RtControlClip` 直接取 clip 字段；`source_offset` 带符号；`loop_ticks: Option<u32>`；负前导与尾部越界静默。
7. `headless` / `export` / `clipboard` / `command` / MCP：寻址与快照结构不变，`ClipSnapshot` 仍是 `(clip, data)` 二元组。
8. 文件版本 `13 → 14`（§13.6）。
9. 单轨上限 `32 → 128`：model `MAX_CLIPS_PER_TRACK` 与 engine `MAX_TRACK_CLIPS` 同步；`SetMidiClips` / `SetControlClips` 改 `Box` 载荷（§12 风险表）。

**M11（tile N 操作）**

| 操作 | 规则 |
| :--- | :--- |
| `Shift+T`（tile N） | 模态内按当前 clip 宽度决定 N；确认后一次性生成 N 个紧密相邻 clip：`start_k = start + k·span`，窗口字段各自一份（值相同）、共享 data；一步 undo |
| 再次 tile N | **一次性操作，不识别、不记录已有序列**：再执行就是把当前 clip 再复制 N 份追加到后面；想减少只能删除 clip |
| 编辑 | 每份都是普通 clip：可单独 `G` / `T` / `K` / 静音 / 改窗口；改 events 因共享 data 同步全部 |
| 整体操作 | 不自动 group；需要整体移动 / 复制时用户自行 `Ctrl+G` |
| `K` 切刀 | 无特判：普通 split 在切点分成两个 clip；序列被切成两段，单周期段自然就是普通 clip |
| 复制 | `Alt+D` 复制 clip、共享 data；`tile N` 与 `Alt+D` 同数据语义，只是批量紧密排列；`Shift+D` 复制 clip + 复制 data |
| 槽位 | `MAX_CLIPS_PER_TRACK = 128` 封顶；剩余槽位不足时按可用份数生成并提示 |
| 命令 / MCP | 新增 `clip.tile {track, clip, count}`，返回新 clip 索引列表；不新增 MCP 工具 |

**为什么不做 tile 类型**：类型的目的是表达「一个整体、内部不可直接编辑」。但用户真正需要的是「重复同一内容、每份可独立调整」，这正好是 N 个共享 data 的普通 clip。放弃类型后，`K` 切刀、单周期退化、单独移动 / 裁剪、undo、选择、编组全部复用现有路径；代价只有 128 槽位与一次批量 push。

### 13.6 v13 → v14 迁移

对每个旧 clip：

```
clip_v14 = MidiClip {
    data,                                  // unchanged; still shared
    source_offset: old.source_offset as i64,
    span_ticks,
    loop_ticks,                            // see below
    start_ticks, group, muted, name,
}
```

- `repeat = false` → 1 个 clip，`loop_ticks = None`；
- `repeat = true` 且 `offset == 0` → 展开成普通 clip 序列：
  - `p = length_ticks`、`q = span / p`、`r = span % p`；
  - 生成 `q` 个整周期 clip，第 k 个 `start = 原 start + k·p`，窗口 = `{offset: 0, span: p, loop: None}`；
  - 若 `r > 0`，再生成 1 个尾部 clip，`start = 原 start + q·p`，窗口 = `{offset: 0, span: r, loop: None}`；
  - 每份窗口字段独立（值相同），共享同一 `data`；
- `repeat = true` 且 `offset != 0` → 1 个 clip，`loop_ticks = Some(p)`：跨数据边界的旧窗口无法用单个非循环窗口等价表达；
- 若某轨展开后 clip 数超过 128 → 该 clip **不展开**，保留 `loop_ticks = Some(p)`，保证 `validate` 通过且播放零回归；
- 每份旧 clip 至少产生 1 个 clip，`q = 0` 时只产生尾部截断 clip；
- clip 顺序 = 旧顺序 × 展开顺序，便于断言；
- legacy 字段（`legacy_loop_ticks` / `legacy_sequence` / `legacy_curve` / `legacy_interp`）继续只读，写路径不输出；
- v14 **不可回退**到 v13；`<name>.glz.bak` 是唯一退路。

### 13.7 门禁

- M9：`cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace`；新增 §10 第 12–15 条验收。
- M10：**零回归**——同一 v13 工程在 M9 二进制与 M10 二进制下播放、`--render` 逐字节一致；新增 §10 第 16–18 条。
- M11：新增 §10 第 19 条 + §11 tile N 测试；批量生成的 undo 为一步。
- 每个里程碑单独 commit；M10 的迁移与结构改动必须同一个提交，不能拆成「先改结构后迁移」。

### 13.8 明确不做

- `TileSpec` / `explode` / 嵌套 tile / 记录 tile 份数 / 再 tile 调整已有序列 —— tile N 是一次性操作，重复执行就是继续追加尾部 clip；
- window 池 / 多 clip 共享同一裁剪窗口（InDesign 式联动裁切）—— 窗口字段永远私有；将来要做再单独立项；
- per-repeat override（某一遍单独改内容）；想改某一份就单独编辑它的窗口 / data；
- 自动 group（不把 tile N 序列合成整体，需要整体操作时用户自行 `Ctrl+G`）；
- 音频 tile（`path` + 秒窗口已足够，音频继续不动）；
- 独立窗口面板（归 M7 数据面板扩展时一并考虑）；
- `ClipId` / 稳定 UUID（仍用池索引 + compact 重映射）。