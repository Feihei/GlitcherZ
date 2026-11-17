# 吸附档位与磁性边界（精确裁剪）

> ID: F-045 · 域: EDIT / UI
> 状态：设计已定，待实施（2026-11-17）
> 关联：`snap.md`（Met 系列 S1–S7，本特性修订 S3/S7）、`clip-data-instances.md`（F-041 D32/D35/D38/D39/D43）、`timeline.md`、`editing-model.md`、`meter-map.md`

## 1. 问题

### 1.1 档位断层

时间轴只有两档落点：默认 `Snap::on(meter.ticks_per_bar_at(0))`（**整小节**，4/4 = 3840 ticks），按住 Ctrl 时 `Snap::OFF`（**最近 tick**）。中间没有拍、没有 step。用户诉求是「大部分情况对齐拍，偶尔更细（例如裁掉末尾音符）」，当前没有落点——只能按 Ctrl 掉到 tick 级，然后手抖落在拍外。

6/8 下一小节 = 2880 ticks = 3 拍；以小节为单位裁剪等于最小可动 3 拍，明显过粗。

### 1.2 off 与 tick 是同一个东西

`Snap::OFF` = `{ grid: 0 }`，`Snap::on(1)` = `{ grid: 1 }`。`Snap::grid()` 对 off 返回 1，`Snap::ticks()` 对 off 走 `raw.round()`，对 grid=1 走 `((raw/1).round()*1)`——**两者数值完全等价**（`snap.rs:27-39`）。差异只在 `is_off()`，而生产代码里 `is_off()` 只有一个读点：`timeline.rs:4475` 的 `snap_bar()`。

结论：**档位菜单不放 Off，也不放 Tick**。Ctrl 临时覆盖就是 tick，菜单里再放一个同义项是冗余；内部 `Snap::OFF` 保留作为 Ctrl 的语义标记（状态栏据此显示 `Snap off`）。

### 1.3 更细的网格 ≠ 能裁到音符末尾

若末尾 note-off 本身不在网格上（SMF 导入、或当初按 Ctrl 画的），1/16 网格同样落不到 3712 tick 这种位置。要精确裁掉末尾音符，需要的是**吸附到内容边界**，而不是把网格切更细。

`clip.fit_window`（`clip.rs:201-212`）已在算同一组锚点：窗口内第一个 note-on、最后一个 note-off。磁性边界吸附可以直接复用这套锚点——本质是「拖拽时实时 fit 正在拖的那条边」，语义天然一致。

### 1.4 关键陷阱：`snap_bar` 忽略 grid

```rust
// timeline.rs:4474-4480（现状）
fn snap_bar(meter: &MeterMap, ticks: f32, snap: Snap) -> u32 {
    if snap.is_off() { ticks.round().max(0.0) as u32 }
    else { meter.snap_bar(f64::from(ticks.max(0.0))) }  // ← 完全无视 snap.grid()
}
```

只要 `is_off()` 为 false，一律走 `meter.snap_bar()`（小节）。**因此把入口的 `Snap::on(...)` 换成 beat 的 tick 数，时间轴落点仍然吸附到小节**。F-045 必须重构这条路径，否则档位菜单在时间轴上完全无效。

## 2. 决策

| ID | 问题 | 结论 |
| :--- | :--- | :--- |
| D1 | 档位集合 | `Bar / Beat / Step`，**默认 Beat**。菜单无 Off、无 Tick（§1.2）。 |
| D2 | Step 的粒度 | 固定 `TICKS_PER_STEP`（240 ticks = 1/16 音符），与拍号无关；Bar / Beat 按 containing segment 的 meter 解析（跨段不漂移，见 D4）。 |
| D3 | `Snap` 承载什么 | 由单个 `grid: i64` 扩为带**音乐语义**的网格：`SnapGrid::{ Bar, Beat, Step, Fixed(i64) }` + `Snap::OFF` 保留为 Ctrl 的 tick 覆盖。`Fixed` 供卷帘（1/16 等）与曲线（120 ticks）继续用任意数值，避免改写 F-0xx 既有调用点。 |
| D4 | 网格解析 | `MeterMap` 新增 `snap_grid(raw, grid)`：与 `snap_bar`（`meter.rs:208`）同构——按**指针所在段**解析网格宽度、以段起点为锚、跨段边界向上取整落到下一段起点。`snap_bar` 变为 Bar 特例或保留为兼容 wrapper。 |
| D5 | 磁性边界目标 | 只取**内容边界**：MIDI 的 note-on / note-off，curve 的 anchor.tick。不取 clip 边缘（已有把手）、不取小节线（网格已覆盖）。 |
| D6 | 磁性范围 | 仅时间轴的**边缘操作**：右缘 resize、头裁、`T` 拉伸的两端、切刀落点。普通 clip 移动不吸内容边界（避免拖动时跳粘）。卷帘内的音符裁剪留待后续特性。 |
| D7 | 网格 vs 磁性冲突 | 两者各出一个候选：网格线（按 D1 档位）+ 指针 `MAGNET_PX`（建议 8px）内最近的边界。取**屏幕上更近**者；等距取网格（可预测）。Ctrl 时两者全关 → 纯 tick。 |
| D8 | 坐标映射 | 内容边界在数据 tick，边缘操作在排列 tick：右缘用 `note.end - source_offset`，左缘用 `note.start - source_offset`；负/越界经 `clamp(0, data.length)`。映射错会得到「吸到了差一个 offset 的位置」。 |
| D9 | 无内容时 | data 为空或无 note/anchor → 磁性无候选，退化为纯网格；不得 panic、不得改变既有空 data 可编辑性。 |
| D10 | 最短窗口 | 从 **1 小节**降为 **1 拍**（clip 起点处的 meter），作为与吸附档位无关的**结构下限**。改 `timeline.rs:3821`（右缘 min_span）、`:4496-4497`（头裁上限）、`clip.rs:278`（fit 的 `.max(bar)`）。Step / Ctrl 可以 fine position，但 span 不得小于 1 拍。 |
| D11 | 非空规则（F-041 D32） | **删除硬拒绝**：`midi_window_has_content` / `curve_window_has_content` 的 6 处调用（`timeline_ops.rs:822/847/2498/2569/2628/2677`）连同两个 helper 移除。裁到空隙是合法操作，D38 的全高锯齿带已经是充分视觉反馈。空 data 豁免条款随删除消失（不再需要）。同步改 `stretch_rejects_an_empty_window` 测试。 |
| D12 | G / T / Shift+T 模态 | 步进粒度从写死的 bar 改为**当前档位**（Bar/Beat/Step），但**不接 Ctrl**（修订 `snap.md` S7：模态永不落 tick）。这三者的 `bar_delta` 计算在 `stretch_targets` / `tile_targets` / `grab_targets`（`timeline.rs:1069-1220`）。 |
| D13 | 状态栏 | `status_bar.rs:190-204` 增加常驻当前档位标签（`Snap: Bar/Beat/Step`）；Ctrl 按住时仍显示黄色 `Snap off`。两者同处一行，Ctrl 优先。 |
| D14 | 档位状态 | 新增 `TimelineState::snap: SnapGrid`，**会话级 view state**，不进 `Project`、不改 `PROJECT_VERSION`（同 `RollState.grid` 的处理）。 |
| D15 | 快捷键 | 不新增档位循环快捷键；通过时间轴工具栏下拉选择（同卷帘网格下拉）。修饰键空间已饱和，且下拉更可发现。 |
| D16 | 例外保留 | 标尺 loop 绘制 / 新建 / marker（`timeline.rs:1938` 及其消费点）与 meter marker 拖动保持小节吸附——loop 范围是音乐段落，小节是正确的单位。录音 `take_clip` 的整小节向上取整不动。 |

## 3. 建议 API 形状

```rust
// glitcherz-ui/src/snap.rs
pub enum SnapGrid {
    Bar,                 // meter-relative, segment aware
    Beat,                // meter-relative, segment aware
    Step,                // fixed TICKS_PER_STEP (240)
    Fixed(i64),          // piano-roll / curve legacy grids
}

pub struct Snap {
    grid: SnapGrid,
    off: bool,           // true only for the Ctrl override
}
impl Snap {
    pub const OFF: Self = Self { grid: SnapGrid::Fixed(1), off: true };
    pub fn on(grid: SnapGrid) -> Self;
    pub fn ticks(&self, meter: &MeterMap, raw: f32) -> i64;
    pub fn grid_ticks(&self, meter: &MeterMap, at: u32) -> i64;
    pub fn is_off(&self) -> bool;
}

// glitcherz-model/src/meter.rs
impl MeterMap {
    /// Nearest grid line inside the containing segment; the grid width is
    /// resolved from that segment's meter, anchored at its start.
    pub fn snap_grid(&self, raw: f64, grid: i64) -> u32;
}
```

`Snap::OFF` 与 `SnapGrid::Fixed(1)` 数值等价（§1.2）；`off` 只为状态栏与「磁性关闭」判定服务，不作为档位暴露。

## 4. 接入面

| 层 | 位置 | 改法 |
| :--- | :--- | :--- |
| UI / snap | `snap.rs:11-49` | 扩 `SnapGrid` / `Snap`，`ticks` 接 meter |
| model | `meter.rs:205-226` | 新增 `snap_grid`，`snap_bar` 复用或保留 |
| UI / 状态 | `lib.rs:1759-1819` `GlitcherZApp` | 视图状态持有 `TimelineState::snap`（D14） |
| UI / 入口 | `timeline.rs:3134-3141` | Ctrl → `Snap::OFF`；否则按 `state.timeline_snap` 构造 |
| UI / 落点 | `timeline.rs:3586, 3561, 3655, 3773, 3800-3806, 3820` | 统一走 `snap_grid`，边缘操作附加磁性（D6） |
| UI / helper | `timeline.rs:4474-4480` `snap_bar` | 按 grid 解析，不再忽略 `snap.grid()` |
| UI / helper | `timeline.rs:4397-4410` `knife_target` | 接磁性边界（D6） |
| UI / helper | `timeline.rs:4489-4499` `trim_head_start` | bar → 档位 + 1 拍下限（D10） |
| UI / 模态 | `timeline.rs:1069-1220` | `bar_delta` 按档位步进，不接 Ctrl（D12） |
| UI / 卷帘 | `piano_roll.rs:2367-2372, 3731-3735` | `SnapGrid::Fixed(grid_ticks / SNAP_TICKS)`，行为不变 |
| UI / 曲线 | `editing.rs:331-335` | 同上，`Fixed(SNAP_TICKS)` |
| 命令 | `clip.rs:258-279` `fit_window` | bar → 1 拍下限（D10） |
| 命令 / UI | `timeline_ops.rs:822/847/2498/2569/2628/2677` | 删除非空规则（D11） |
| 状态栏 | `status_bar.rs:190-204` | 常驻档位标签（D13） |
| 文档 | `docs/shortcuts.md:5,103,125,130`、`docs/manual.md:79,103,139,167` | 描述档位与磁性；Ctrl 文案不变 |
| 文档 | `.agents/spec/snap.md:33,37,161-162` | 修订 S3（新增会话级档位）、S7（模态跟档位不跟 Ctrl）、§9（取消"将来引入"的待办标记） |

## 5. 里程碑

| 里程碑 | 内容 | 依赖 | 门禁 |
| :--- | :--- | :--- | :--- |
| **M0** | 本 spec + 注册表 + plan 立项 | — | 文档评审 |
| **M1** | `SnapGrid` / `MeterMap::snap_grid` / 档位状态 / 状态栏；**Bar 档行为逐 tick 零回归** | M0 | 现有 snap 测试全绿，Bar 档落点与旧 `meter.snap_bar` 一致 |
| **M2** | 磁性边界吸附（右缘 / 头裁 / T 两端 / 切刀）+ 1 拍下限 + 非空规则删除 | M1 | §6 验收 3–9 |
| **M3** | G / T / Shift+T 模态跟档位；docs 同步 | M2 | §6 验收 10–12 + 全量门禁 |

顺序 M1 → M2 → M3；M1 必须单独提交（门禁是 Bar 档零回归），M2 的磁性吸附与非空规则删除可拆两个 commit，但都在 M3 之前。

## 6. 验收

1. 档位菜单只有 `Bar / Beat / Step`，默认 Beat；没有 Off / Tick 项。
2. Bar 档下所有落点与 F-045 之前逐 tick 一致（含跨拍号段的段锚定）。
3. Beat 档下 4/4 的裁剪落点间隔 960 ticks；6/8 间隔 480 ticks；跨 4/4 → 6/8 边界不漂移。
4. Step 档落点间隔固定 240 ticks。
5. 右缘拖到末尾 note-off 附近（≤8px）时精确吸附到 `note.end - source_offset`，即便该点不在当前网格上。
6. 头裁拖到首个 note-on 附近同理；source_offset 为负时映射仍正确。
7. 网格线与边界都在阈值内时取更近者；等距取网格。
8. Ctrl 拖动完全关闭网格与磁性，落点是最近 tick，状态栏显示 `Snap off`。
9. 空 data 或无 note 的 clip：磁性无候选，纯网格裁剪不 panic。
10. 最短窗口 = 1 拍；右缘 / 头裁 / `clip.fit_window` 三处一致。
11. 裁到两个音符之间的空隙不再被拒绝；锯齿带（D38）仍正确画出。
12. `G` / `T` / `Shift+T` 按当前档位步进；按 Ctrl 不改变它们的步进（S7 修订）。
13. 状态栏在 Ctrl 松开后显示当前档位；档位切换只影响会话，存盘重开不写入工程。

## 7. 测试

- **snap**：`SnapGrid::{Bar,Beat,Step,Fixed}` 的 `grid_ticks`；`Snap::OFF` 与 `Fixed(1)` 数值等价；跨 meter 段（4/4→6/8）的 `meter.snap_grid` 段锚定与向上取整；`snap_bar` 兼容 wrapper 的逐 tick 对照。
- **timeline**：`knife_target` / `trim_head_start` / 右缘在 Beat / Step / Off 下的落点；磁性命中 / 未命中 / 等距；负 offset 映射；1 拍下限钳制（MIDI / curve / audio 三 lane）。
- **command**：`clip.fit_window` 的 1 拍下限；非空规则删除后 `stretch_rejects_an_empty_window` 改为「允许空窗口」的断言。
- **状态**：档位切换不影响 `Project` 序列化（存盘字节不变）。
- **门禁**：`cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace`（沙箱外跑）。

## 8. 风险

| 风险 | 处理 |
| :--- | :--- |
| `snap_bar` 忽略 grid 导致档位失效 | M1 显式重构 + Bar 档逐 tick 对照测试；这是本特性最高优先级回归门禁 |
| off == tick 被误当成两个档位 | 菜单只暴露三档；`is_off()` 只服务于 Ctrl 与状态栏（D1/§1.2） |
| 跨拍号段 beat 网格漂移 | `meter.snap_grid` 按 pointer 所在段解析并锚在段起点（D4），不复用 tick 0 的网格宽度 |
| 磁性吸到错误坐标 | 数据 tick → 排列 tick 显式减 `source_offset` 并 clamp（D8）；负 offset 单测 |
| 磁性在拖动中反复跳粘 | 仅边缘操作启用（D6）、阈值 8px、等距取网格（D7） |
| 删除非空规则后测试/文档残留 | 6 处调用 + 2 个 helper + 1 个测试 + spec D32 + docs 一并改；grep 校验零残留 |
| 1 拍下限与 Step / Ctrl 冲突 | span 钳制独立于吸附档位（D10），fine position 只影响边界位置 |
| 会话档位与卷帘网格不一致造成困惑 | 两者明确独立（D14）；卷帘网格下拉保持在卷帘工具栏，时间轴档位放时间轴工具栏 |

## 9. 明确不做

- 档位菜单里的 Off / Tick（§1.2）；持久化的吸附总开关（`snap.md` S3 原意保留）。
- 工程文件记录档位 / 修改 `PROJECT_VERSION`（D14）。
- 新增档位循环快捷键（D15）；使用 Alt 作修饰键（`snap.md` S2 已排除）。
- 普通 clip 移动吸内容边界；卷帘内音符裁剪吸内容边界（D6）。
- 标尺 loop / marker / 录音落点的档位化（D16 保留小节）。
- 统一时间轴档位与卷帘网格为单一设置（两套 view state 并存）。
- 把 F-041 D32 的非空规则改成软提示——直接删除，锯齿带即为反馈（D11）。
