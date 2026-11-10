# 拍号变化标记与拍号范围（Met）Spec

> 状态：**设计已定，待实施**（2026-11-10 立项）。取代 `tempo-meter.md` 的 D5「拍号静态」。
> 上游：`tempo-meter.md`（U3a 拍号、D8 改拍号不改写事件）、`timeline.md`（D16 arrangement 末尾、D29 吸附）、`node-graph.md`（`Timeline In` 端口）。
> 涉及：`glitcherz-model`（`Property` 新增 `meter.rs`）、`glitcherz-ui`（timeline / ruler / status_bar / transport / recording / export_audio / file_ops）、`glitcherz-engine`（`sequencer.rs` / `metronome.rs` / `commands.rs`）、`glitcherz-model/src/smf.rs`。

## 1. 背景与问题

1. **拍号是工程级单值**：`ProjectMetadata.time_signature: TimeSignature`（`model/lib.rs:6375-6379`），全曲只有一个拍号。需要中途变拍（如 4/4 → 6/8 → 3/4）时没有数据结构。
2. **换算全是「乘法」**：`ticks_per_bar()`（`lib.rs:1538-1541`）、`Project::ticks_per_bar()`（`lib.rs:6096-6101`）都是「拍号 → 小节长度」的一步乘法。有变化点后必须变成**分段累积**，这是本 spec 的核心改动。
3. **拍号范围 UI 太窄**：模型已支持 `numerator 1..=16`、`denominator {1,2,4,8,16,32}`（`lib.rs:1520-1532`），但走带只有 6 个预设（`METER_PRESETS`，`ui/lib.rs:227`、`transport.rs:429-445`），13/8 这类值进不来。

## 2. 目标形态

- **变化标记**：`ProjectMetadata.meter_markers: Vec<MeterMarker>`，每个 marker = 「从该 tick 起拍号变为 X」。第 0 段的拍号仍是 `metadata.time_signature`。
- **求值层**：`MeterMap` 一次性预计算段表（段起点 + 该段起始小节号），所有「小节 ↔ tick」换算都走它，替换掉全部 `bar_ticks` 单值用法。
- **拍号输入**：走带的拍号控件扩成「分子 1–12 + 分母 {1/2, 1/4, 1/8, 1/16}」；模型范围**不收窄**，导入的超范围值只读显示。

## 3. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| D1 | **拍号变化用 marker 表，不复用 control 轨曲线** | 拍号是离散阶梯（`4/4` → `6/8`），不是 `[-1,1]` 连续值；control clip 的插值、归一化、曲线编辑全不适用。tempo 曲线（U3b）与本表并列，互不影响。 |
| D2 | **marker 必须落在「当前段的小节线」上** | 不允许从半小节处变拍号：否则小节会被切成两半，bar 编号出现小数段，`bar ↔ tick` 不再可逆。UI 拖动/新建一律吸附到段内小节线；文件里非法位置在 `MeterMap::new` 中**向下吸附**到所在小节起点（宽容读取，不拒绝）。 |
| D3 | **`tick > 0`，0 处由 `metadata.time_signature` 承担** | 避免「全局拍号」与「tick 0 的 marker」两个真源。在 0 处改拍号 = 直接改全局字段，UI 自动这样路由。 |
| D4 | **`MeterMap` 预计算段表，查询 O(log n)** | 段表存 `(段起点 tick, 段起始小节号, 拍号)`。构造时按段累加小节数；ruler 每帧要画上千条网格线，不能每次 O(markers) 线性扫。 |
| D5 | **像素保持全局线性：`px_per_tick` 由 base 拍号锚定** | `px_per_tick = px_per_bar / base_bar_ticks`，`px_per_bar` 的语义改为「第 0 段小节的像素宽度」。这样 `x ↔ tick` 全 timeline 仍是一条直线，**只有网格线/标号/吸附粒度按段变化**；若反过来按段改 `px_per_tick`，整条 timeline 的坐标映射都要分段，风险与收益不成比例。副作用：3/4 段的小节比 4/4 段窄 25%，符合直觉。 |
| D6 | **引擎用定容段表，RT 只做整数运算** | 新命令 `SetMeterMarkers { count, segs: [MeterSeg; MAX] }`（`Copy`）。`Sequencer` 持段表，`advance` / `refresh_arrangement_end` / metronome 都按 `position` 查段；无分配、无锁、无浮点（拍号域全是整数）。 |
| D7 | **SMF 每段写 `FF 58`，导入全收** | 导出在 tick 0 写 base，再在每个 marker tick 写一条 `FF 58`；导入读**所有** `FF 58`，第一个（tick 0 或最早）转 base，其余转 marker。沿用现有「空白工程才继承、已有内容保留工程拍号并提示」的策略。 |
| D8 | **改拍号 / 增删 marker 不改写任何事件 tick** | 沿用 `tempo-meter.md` D8：note / clip / marker / loop 的 tick 一律不动。变的只有「引用小节」的换算（ruler 标号、bar 网格、吸附、arrangement 末尾、状态栏、SMF）。 |
| D9 | **`PROJECT_VERSION` 10 → 11，纯加法，无迁移函数** | 新字段带 `#[serde(default)]` + `skip_serializing_if = "Vec::is_empty"`：旧文件读出空表，`MeterMap` 退化成单段，行为与今天逐字节一致。 |
| D10 | **不做复合拍分组** | 12/8 仍按 12 个八分拍计数、重音只在第 1 拍（用户 2026-11-10 拍板）。3+3+3+3 分组留作后续独立项，届时给 `TimeSignature` 加可选 `group`。 |

## 4. 数据模型

```rust
// glitcherz-model/src/lib.rs
pub struct MeterMarker {          // 新
    pub tick: u32,                // 必须 > 0，且落在当前段的小节线
    pub signature: TimeSignature, // 复用现有类型
}
pub struct ProjectMetadata {
    /* ... 现有字段 ... */
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub meter_markers: Vec<MeterMarker>,
}

pub const MAX_METER_MARKERS: usize = 64;
```

约束（`Project::validate`）：

- 每个 `signature.is_valid()`；
- `tick > 0` 且 `tick <= MAX_ARRANGEMENT_TICKS`；
- 表按 `tick` 严格升序、无重复；
- 长度 ≤ `MAX_METER_MARKERS`。
- **不校验**「marker 在小节线上」：宽容读取，`MeterMap::new` 向下吸附。

## 5. 求值层（`model/src/meter.rs`，新模块）

```rust
pub struct MeterSegment {
    pub tick: u32,      // 段起点
    pub bar: u32,       // 该段起点所在的小节号（0 基）
    pub signature: TimeSignature,
}

pub struct MeterMap {
    base_bar: u32,          // = 0，留作将来「从第 N 小节起」扩展
    segments: Vec<MeterSegment>,  // 段 0 恒为 (tick 0, base signature)
}

impl MeterMap {
    pub fn new(base: TimeSignature, markers: &[MeterMarker]) -> Self;
    pub fn at(&self, tick: u32) -> TimeSignature;       // 取所在段拍号
    pub fn ticks_per_bar_at(&self, tick: u32) -> u32;
    pub fn ticks_per_beat_at(&self, tick: u32) -> u32;
    pub fn bar_beat_at(&self, tick: u32) -> (u32, u32); // bar 0 基、beat 0 基
    pub fn bar_start(&self, bar: u32) -> Option<u32>;
    pub fn next_bar(&self, tick: u32) -> u32;           // 严格大于 tick 的下一条小节线
    pub fn prev_bar(&self, tick: u32) -> u32;           // 严格小于 tick 的上一条小节线
    pub fn snap_bar(&self, tick: f64) -> u32;           // 段内相对小节网格吸附
    pub fn segment_of(&self, tick: u32) -> &MeterSegment;
}
```

累积公式（`new` 里一次算完）：

```text
seg[0]    = { tick: 0, bar: 0, signature: base }
for k in 1..n:
    a = seg[k-1]
    bars = (marker[k].tick - a.tick) / a.signature.ticks_per_bar()   // D2 保证整除
    seg[k] = { tick: marker[k].tick, bar: a.bar + bars, signature: marker[k].signature }
```

查询（`segment_of` 二分）：

```text
bar  = seg.bar + (tick - seg.tick) / seg.signature.ticks_per_bar()
beat = ((tick - seg.tick) % seg.signature.ticks_per_bar()) / seg.signature.ticks_per_beat()
```

`Project` 上的便捷方法（内部 `MeterMap::new`，marker 数量少时每次构造可接受；UI/引擎各自持有一份缓存）：

```rust
impl Project {
    pub fn meter_map(&self) -> MeterMap;
    pub fn bar_beat_at(&self, tick: u32) -> (u32, u32);
    pub fn ticks_per_bar_at(&self, tick: u32) -> u32;
}
```

`ticks_per_bar()` 保留为「第 0 段」的快捷方式，避免一次性改爆调用点；新代码一律用 `*_at`。

## 6. UI 改动

### 6.1 像素与布局

| 位置 | 现状 | 改为 |
| :--- | :--- | :--- |
| `timeline.rs:1498-1502` | `bar_ticks = p.bar_ticks.max(1)`；`px_per_tick = px_per_bar / bar_ticks` | `px_per_tick = px_per_bar / p.meter.segments[0].signature.ticks_per_bar()`（D5） |
| `timeline.rs:1510-1511` | `content_bars = LIMIT.div_ceil(bar_ticks)`；`content_px = content_bars * px_per_bar` | `content_px = ARRANGEMENT_LIMIT_TICKS as f32 * px_per_tick` |
| `TimelineParams.bar_ticks: u32` | 单值 | `meter: MeterMap`（`Copy` 不需要；用 `&MeterMap` 或 `Arc`，见 §6.4） |

### 6.2 绘制

- **ruler**（`timeline.rs:1810-1846`）：按段循环画小节线，bar 号 = `seg.bar + (tick - seg.tick) / bar_ticks + 1`；段起点画加粗竖线 + 拍号胶囊（`4/4`，11 px）。`step`（每 4 小节标号）按**段内**小节像素宽度判断，不跨段混算。
- **lane 网格**（`timeline.rs:2961-2966`）：同样按段循环；视口裁剪到 `[seg.tick, next_seg.tick)` 后再算起止。
- **钢琴卷帘**（`piano_roll.rs:2065-2092`）：目前在一个 `loop_ticks` 窗口内按单拍号重锚；改为在窗口内按 `MeterMap` 分段，段边界也是小节线。
- **meter 旗标**：与命名 `Marker`（`lib.rs:6401-6411`，小三角 + 名字）视觉区分——meter marker 画成**竖直胶囊 + 拍号文本**，不显示名字，可拖动。

### 6.3 交互

| 操作 | 行为 |
| :--- | :--- |
| 新建 | ruler 右键菜单新增 `Add meter change`（与现有 `Add marker` 并列）；在指针所在 tick 向下吸附到段内小节线后插入；若与已有 marker 同 tick，则改为「编辑该 marker」。 |
| 拖动 | 左键拖 meter 胶囊：按段内小节网格吸附；按住 `Ctrl` 时**不吸附**（`snap.md` S2），但仍钳制在 `tick > 0` 且不与相邻 marker 交叉。 |
| 编辑值 | 双击胶囊 → 弹出拍号控件（与 M1 的走带控件同一组件）；或选中后在 Inspector 显示。 |
| 删除 | 右键胶囊 → `Remove meter change`；删段后退回前一拍号（不是删数据事件）。 |
| undo | 全部走 `begin_edit` / `commit_edit`，一步一个 undo（拖动全程合并为一个手势）。 |
| 新动作 | `TimelineAction::{AddMeterMarker, MoveMeterMarker, SetMeterMarker, RemoveMeterMarker}`，由 app 层翻译。 |

### 6.4 传递方式

`TimelineParams` 目前是每帧构造的借用结构（`timeline_ops.rs:144-159`）。`MeterMap` 含 `Vec`，若直接塞进去会每帧分配。做法：`GlitcherZApp` 持 `meter_map: MeterMap`，在文档变更（`commit_edit` / `apply_document` / open / undo）后重建；`TimelineParams` 只借 `&MeterMap`。钢琴卷帘同理。

### 6.5 其他读取点

| 位置 | 改动 |
| :--- | :--- |
| `status_bar.rs:182-189` | `bar_beat_at(ticks)` 取代本地除法。 |
| `transport.rs:526-530` `seek_by_bars` | `next_bar` / `prev_bar` 分段步进；`Ctrl+←/→` 语义不变。 |
| `recording.rs:384` `(now / bar).ceil() * bar` | `meter.next_bar(now)`。 |
| `export_audio.rs:245-252` 末尾取整 | 最后一段的 `next_bar`。 |
| `file_ops.rs:317-330` 导入拍号 | 见 D7：空白工程继承 base + markers，已有内容保留并提示。 |
| `timeline.rs:42-45` `ARRANGEMENT_LIMIT_TICKS` | 不变（绝对 tick 上限）。 |
| `timeline.rs:4036` `snap_bar(ticks, bar_ticks)` | `meter.snap_bar(ticks)`（+ `Snap` 开关，见 `snap.md`）。 |
| `timeline.rs:1854` 标尺局部 `snap_bar` 闭包 | 删除，统一用 `MeterMap::snap_bar`。 |
| `timeline.rs:4020-4032` `knife_target` | 左右各至少一「当前段」小节。 |
| `timeline.rs:4046` `trim_head_start` | 分段小节长度。 |

## 7. 引擎

```rust
// commands.rs
#[derive(Clone, Copy)]
pub struct MeterSeg {
    pub tick: u32,
    pub bar: u32,
    pub numerator: u8,
    pub denominator: u8,
}
pub const MAX_METER_SEGS: usize = 64;

SequencerCommand::SetMeterMarkers { count: u8, segs: [MeterSeg; MAX_METER_SEGS] }
```

- `Sequencer` 新增 `meter_segs: [MeterSeg; 64]` + `meter_len: u8`；`bar_ticks` / `numerator` / `denominator` 保留为**当前段**缓存，在 `advance` 跨段时更新（tick 只增，段指针可线性推进，不用每块二分）。
- `refresh_arrangement_end()`：用最后一段的 `bar_ticks` 对最新 clip 末尾 `div_ceil(bar)*bar`，再按段表回卷到真实小节线（`next_bar` 的整数版）。
- metronome（`metronome.rs:168-195`）：`beat_ticks` 与 `numerator` 取**当前段**；跨段时重锚 `resync(pos, beat_ticks)`，避免把 4/4 的拍计数带进 6/8。
- 兼容：`SetTimeSignature` **保留**并继续下发 base；`SetMeterMarkers` 单独下发模板。UI 每次改拍号或 marker 都推两者（幂等、可重复）。
- 队列容量评估：`64 × 12 B = 768 B` 的单元素；若 sequencer 队列内存不可接受，把 `MAX_METER_SEGS` 降到 32（模型上限同步降到 32），本 spec 不锁死。

## 8. 拍号范围（功能 3）

- **模型不动**：`numerator 1..=16`、`denominator {1,2,4,8,16,32}`（`lib.rs:1520-1532`）。收窄会拒绝 SMF 导入的合法分母。
- **UI 控件**（`transport.rs` 与 meter marker 弹窗复用同一组件）：
  - 分子：`DragValue` 1..=12（用户 2026-11-10 指定范围）；
  - 分母：下拉 `1/2`、`1/4`、`1/8`、`1/16`；
  - 当前值超出 UI 范围（如导入 13/8、5/32）时：控件**只读显示原值**，旁边给一条 `imported value` 提示，不静默钳制。
- `METER_PRESETS` 保留作为「常用快速选择」附加在分母下拉里？**不保留**：改成上述两控件后，预设会与自由输入重复，违反「功能入口不重复」。仅在悬停文案里给出 `4/4 · 3/4 · 6/8` 示例。
- 12/8 等复合拍按 D10 的简单语义显示（12 拍）。

## 9. 边界与验收

| 场景 | 期望 |
| :--- | :--- |
| 无 marker（旧工程） | `MeterMap` 单段；ruler / 吸附 / 状态栏 / 导出与今天逐字节一致 |
| 4/4 @ bar 5 起变 6/8 | bar 5 起点是 marker；前一栏画 `4/4` 胶囊、之后画 `6/8`；bar 编号连续无跳号 |
| 6/8 段内小节长度 | `6 × 480 = 2880` tick；小节像素宽度 = 2880 × `px_per_tick` |
| 段内吸附 | 在 6/8 段拖 clip，落点吸附到 2880 的整数倍（相对段起点）；不会吸回 4/4 网格 |
| 跨段拖动 | 拖过段边界时吸附基准在边界处切换（边界本身是小节线，落点连续） |
| Ctrl 拖动 | 落点不吸附（任意 tick），但 marker 不能被拖到 `tick 0` 或越过相邻 marker |
| 改 marker 拍号 | 事件 tick 全部不动（D8）；只有后续小节长度/编号变化 |
| 删除 marker | 该段回退到前一段拍号；段长重新合并，bar 编号重算 |
| seek / Ctrl+←→ | 按当前段小节步进；跨段时下一步落到下一段第 1 小节 |
| 状态栏 | `Pos 5.1` 在某段边界处不跳号；6/8 段 `beat` 在 1..=6 |
| 录音落盘 | 起点 `next_bar` 到当前段小节线 |
| 导出末尾 | 最后一段小节线取整 |
| SMF 导出 | tick 0 一条 `FF 58`，每个 marker 一条；分母写真实值（8 写 3） |
| SMF 导入（空白工程） | base + markers 全继承 |
| SMF 导入（已有内容） | 保留工程拍号/标记，状态栏提示文件名拍号 |
| 旧 v10 文件 | 读出空表，`version` 升到 11 后保存；往返后行为不变 |
| 超范围拍号（13/8） | 加载成功，UI 只读显示 `13/8`，不被钳成 4/4 |

## 10. 分阶段

| 阶段 | 内容 | 依赖 |
| :--- | :--- | :--- |
| **M3** 求值层 | `MeterMarker` + `MAX_METER_MARKERS` + `validate` + `meter.rs`（`MeterMap` 全部查询）+ 分段数学单测；`PROJECT_VERSION` 11。**此阶段无调用点改动**，空表时零回归。 | 无 |
| **M4** timeline 读取层 | `TimelineParams` 改 `&MeterMap`；ruler / lane 网格 / snap / knife / trim / 状态栏 / seek / 录音 / 导出 全部改分段；ruler 画 meter 胶囊（只读）；app 缓存 `MeterMap`。 | M3、`snap.md` S2（吸附入口先统一，避免两次改） |
| **M5a** 编辑 | ruler 右键 `Add meter change`、拖动 / 双击改值 / 右键删除；`TimelineAction` 四个变体；Inspector 拍号区；复用 M1 控件。 | M4 |
| **M5b** 引擎 + SMF | `MeterSeg` / `SetMeterMarkers`、`Sequencer` 段表与 metronome 重锚、SMF 多段导入导出。 | M3、M5a |

## 11. 明确不做

- 复合拍分组（3+3+3+3）、拍号自动化（不接 `Timeline In` 端口）。
- 半小节处的拍号变化（D2）。
- 按段改变 `px_per_tick`（D5）。
- 多拍号的同时「重排内容」或「按新网格量化」（D8）。
- `Timeline In` 增加 `Meter` 端口：拍号不是连续值。