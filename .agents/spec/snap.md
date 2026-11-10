# 吸附统一入口与临时禁用（Snap）Spec

> 状态：**设计已定，待实施**（2026-11-10 立项）。
> 上游：`timeline.md`（D29 吸附与模态）、`editing-model.md`（卷帘手势）、`control-tracks.md`（曲线编辑）。
> 涉及：`glitcherz-ui` 的 `timeline.rs` / `piano_roll.rs` / `editing.rs` / `curve.rs` / `timeline_ops.rs` / `toolbar.rs` / `status_bar.rs`。

## 1. 背景与问题

吸附目前是**五套并行实现**，粒度各自为政，且没有任何关闭入口：

| # | 位置 | 粒度 | 用途 |
| :--- | :--- | :--- | :--- |
| 1 | `piano_roll.rs:3258` `snap_ticks(ticks, grid_ticks)` | `RollState.grid`（默认 1/16） | 卷帘音符移动 / resize / draw / 模态 |
| 2 | `timeline.rs:4036` `snap_bar(ticks, bar_ticks)` | 整小节 | clip 拖动 / resize / 切刀 / 头裁 |
| 3 | `timeline.rs:1854` 标尺内同名闭包（f64） | 整小节 | loop / marker |
| 4 | `timeline.rs:1072 / 1131 / 1213` 模态内联 round | 整小节 / 重复份数 | G / Shift+D / T / Shift+T |
| 5 | `editing.rs:328-333`、`piano_roll.rs:3176-3183` 内联 `SNAP_TICKS` | 固定 1/32（`curve.rs:18`） | 曲线锚点 |

问题：用户无法在不吸附的情况下微调；想做「自由放置」只能先放大视图。修饰键层面 `Ctrl` 在时间轴（ruler loop）和卷帘（拖动复制）已有用途，需要先理清。

## 2. 目标形态

- 一个 `Snap` 值类型贯穿所有编辑路径：`grid <= 0` 表示关闭。
- 按住 `Ctrl`（macOS `Cmd`）时**全局临时关闭吸附**，松开恢复；网格线仍绘制，状态栏提示 `Snap off`。
- 复制手势收敛到 `Shift+D` 与工具栏按钮，`Ctrl+拖动复制` 删除，为禁用吸附让路。

## 3. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| S1 | **统一 `Snap` 值类型，`0` = 关闭** | `Snap { grid: i64 }`；`Snap::OFF`、`Snap::on(grid)`、`ticks(raw: f32) -> i64`、`ticks_f64(raw: f64) -> f64`。关闭时 = 四舍五入到最近 tick（不是截断），仍保持整数 tick 域。 |
| S2 | **`Ctrl` / `Cmd` 按住临时关闭** | 每帧在视图入口派生一次：`let snap = if ctrl_or_cmd { Snap::OFF } else { Snap::on(grid) };`，再向下传。**不用 Alt**：Windows 松开 Alt 触发 `SC_KEYMENU` 吞掉鼠标事件、按住 Alt 时 egui 指针会 `PointerGone`（`timeline.md` D29 已实证）；Alt 在 timeline 还绑定拆组（`timeline.rs:3364`）。 |
| S3 | **网格线照画，状态栏提示** | 关闭吸附时网格线不隐藏（否则失去位置参照）；按住期间状态栏右侧显示 `Snap off`，松开消失。不做全局持久开关（用户 2026-11-10 只要临时禁用）。 |
| S4 | **复制只保留 `Shift+D` + 工具栏按钮** | 删除卷帘 `Ctrl+拖动复制`（`piano_roll.rs:2649-2668`）与对应 `RollIntent::Duplicate` 的拖动分支。卷帘工具栏（`toolbar.rs`，I3 已交付）加 `Duplicate ⇧D` 动作按钮；timeline 工具栏同样加。`Shift+D` 模态语义不变（复制 + Grab）。 |
| S5 | **删除命中的容差不属于吸附** | `timeline_ops.rs:1134` `remove_near(tick, SNAP_TICKS/2)` 是「点得够近就删」，与落点吸附无关，不改。 |
| S6 | **Ctrl 的既有拖动语义与新语义叠加，不迁移** | 时间轴：ruler 区 `Ctrl+拖 = 新建 loop`（区域隔离，不冲突）；clip 区 `Ctrl+拖 = 追加选择 + 不吸附`。卷帘：`Ctrl+单击 = 多选`（单击无落点，不冲突）；`Ctrl+拖` 改为纯「不吸附」（复制已按 S4 移除）。`Ctrl+中键 = clip mute` 不涉及位置。 |

## 4. 统一类型

```rust
// glitcherz-ui/src/snap.rs（新）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snap {
    grid: i64,
}

impl Snap {
    pub const OFF: Snap = Snap { grid: 0 };

    pub fn on(grid: i64) -> Self {
        Snap { grid: grid.max(1) }
    }

    pub fn is_off(self) -> bool {
        self.grid <= 0
    }

    /// Nearest grid line in ticks; off = nearest tick. Never negative.
    pub fn ticks(self, raw: f32) -> i64 {
        if self.is_off() {
            raw.round().max(0.0) as i64
        } else {
            ((raw / self.grid as f32).round() as i64 * self.grid).max(0)
        }
    }

    /// Same for the f64 paths (ruler / loop).
    pub fn ticks_f64(self, raw: f64) -> f64 {
        if self.is_off() {
            raw.round().max(0.0)
        } else {
            ((raw / self.grid as f64).round() as f64 * self.grid as f64).max(0.0)
        }
    }
}
```

`timeline.rs` 的分段小节吸附（配合 `meter-map.md`）单独实现，但同样吃 `Snap`：

```rust
pub fn snap_bar(meter: &MeterMap, raw: f64, snap: Snap) -> u32 {
    if snap.is_off() {
        return raw.round().max(0.0) as u32;
    }
    meter.snap_bar(raw)   // 段内相对小节网格（meter-map.md D2）
}
```

## 5. 改造清单

### 5.1 卷帘（`piano_roll.rs`）

| 位置 | 改动 |
| :--- | :--- |
| `:1940` 主区 `snap` 闭包 | 改为 `Snap::on(grid_ticks)` / 视图入口派生 |
| `:3258` `snap_ticks` | 签名改 `(ticks: f32, snap: Snap) -> i64` |
| `:3297` `draw_end` / `:3307` `trim_start` | 末参 `grid_ticks: i64` → `snap: Snap` |
| `:694 / 732-733` `Modal::frame` | 传 `Snap` |
| `:2524` 双击加音符、`:2730` draw 起点 | 用 `snap.ticks(...)` |
| `:2803` 拖动、`:2848 / 2864` resize | 同上 |
| `:2561-2563` 方向键微移 | **保持 ±`grid_ticks`**，不受 Ctrl 影响（键盘步进不是拖动） |
| `:3176-3183` 曲线 overlay `mapped` | `Snap::on(SNAP_TICKS)`，Ctrl 时 OFF |
| `:2649-2668` Ctrl+拖动复制 | **删除**（S4） |

### 5.2 时间轴（`timeline.rs`）

| 位置 | 改动 |
| :--- | :--- |
| `:1854` 标尺局部 `snap_bar` | 删除，统一 `snap_bar(meter, raw, snap)` |
| `:1908 / 1930 / 1976` loop 新建/提交/ghost | 同一 `Snap`（ruler 区仍走 Ctrl=loop 分支，故此处 `Snap` 恒为 on） |
| `:1967` 右键加命名 marker | 受 `Snap` 影响 |
| `:3317` 双击空 lane 建 clip | 受 `Snap` 影响 |
| `:3442` clip 拖动 | `snap_bar(meter, ..., snap)` |
| `:3469-3474 / :4057` 头裁 | 同上 |
| `:3490` 右缘 resize | 同上 |
| `:4028` `knife_target` / `:3247` 切刀预览 | 同上 |
| `:1072` `grab_targets` | bar_delta 由 `Snap` 决定（off → 按 tick 连续移动） |
| `:1131` `stretch_targets` | 同上 |
| `:1213-1245` `tile_targets` | 关闭吸附时**不分份**，只按 tick 连续拉伸；份数取整逻辑仅在 snap on 时生效 |

### 5.3 曲线（`editing.rs`）

| 位置 | 改动 |
| :--- | :--- |
| `:328-333` `mapped` | `Snap::on(SNAP_TICKS)`，Ctrl 时 OFF |
| `:392-397` 右键删除 | 容差不变（S5） |

### 5.4 UI 提示与工具栏

- `status_bar.rs`：`ctrl_held && !text_edit_focused` 时追加 `Snap off`（与现有提示同区、同字号，不新增颜色）。
- `toolbar.rs`：roll 动作组追加 `Duplicate ⇧D`；timeline 动作组追加 `Duplicate ⇧D`（均调 `action_button_ui`，一次性，不保持高亮）。卷帘 `Ctrl+拖动复制` 删除后，`Duplicate` 按钮与 `Shift+D` 是唯一复制入口。

## 6. 边界与验收

| 场景 | 期望 |
| :--- | :--- |
| 卷帘按住 Ctrl 拖音符 | 落点 = 最近 tick；松开 Ctrl 后下一次拖动恢复网格吸附 |
| 卷帘按住 Ctrl 拖曲线锚点 | 1/32 吸附关闭，落到任意 tick |
| 时间轴按住 Ctrl 拖 clip | 位置不吸附小节；松开后恢复 |
| 时间轴按住 Ctrl 拖 clip 头部 | 头裁起点不吸附，但仍钳在 `[origin_start, ...]` 与 ≥1 tick |
| 时间轴 Ctrl+拖 ruler | 仍是新建 loop（S6），且 loop 端点按小节吸附 |
| 时间轴 Ctrl+单击 clip | 仍是追加选择；若同时拖动，则「追加选择 + 不吸附」 |
| 卷帘 Ctrl+拖动 | **不再复制**；只做「不吸附」拖动（S4） |
| Shift+D | 仍是复制 + Grab 模态，行为不变 |
| 工具栏 Duplicate | 等同按一次 `Shift+D`，触发后不保持高亮 |
| 网格线 | 按住 Ctrl 时仍显示，不闪烁 |
| 状态栏 | 按住 Ctrl 时显示 `Snap off`；文本输入框聚焦时不显示 |
| 方向键微移 | 不受 Ctrl 影响，仍按 grid 步进 |
| 无回归 | 不按 Ctrl 时所有落点与今天逐 tick 一致 |

## 7. 分阶段

| 阶段 | 内容 |
| :--- | :--- |
| **M2a** 收口 | 新增 `snap.rs`；五套实现改吃 `Snap`；**不改键位、不改行为**（恒 `Snap::on`）；纯函数单测覆盖 on/off 两条分支。 |
| **M2b** 接线 | 各视图入口从 `modifiers.ctrl \| command` 派生 `Snap`；状态栏提示；删除卷帘 Ctrl+拖动复制；两个工具栏加 `Duplicate ⇧D`；`docs/shortcuts.md` 同步。 |

## 8. 明确不做

- 持久化的「吸附开关」或「网格关闭」设置（`GridValue::Off` 不引入）。
- 时间轴的可配置吸附粒度（本次仍是整小节；将来若引入，作为 `Snap` 的 grid 值即可）。
- 把 `Ctrl+拖动复制` 迁到 `Alt+拖动`（S2 已排除 Alt）。
- 改变删除命中容差、方向键步进、框选（marquee）行为。