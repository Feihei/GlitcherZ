# Edit 视图试听与独立时间轴（Aud）Spec

> 状态：**设计已定，待实施**（2026-11-10 立项）。
> 上游：`timeline.md` D16（arrangement end 自动停）、D29（吸附）、`tempo-meter.md`（拍号）、I2（单音试听）、I3（工具栏）。
> 涉及：`glitcherz-engine`（`commands.rs` / `sequencer.rs` / `block.rs`）、`glitcherz-ui`（`editing.rs` / `piano_roll.rs` / `transport.rs` / `keyboard.rs` / `lib.rs`）、`docs/manual.md` / `docs/shortcuts.md`。

## 1. 背景与问题

1. **Edit 没有时间轴**：卷帘的小节线只画 `loop_ticks`（`piano_roll.rs:2083-2110`），没有 ruler；视图窗口也按 `clip.start_ticks + loop_ticks` 定（`piano_roll.rs:1342-1375`），平铺 clip 的 `span` 完全不展开。
2. **没有区间试听**：只有单音 audition（I2，`piano_roll.rs:2026-2064` / `editing.rs:658-670`）。Edit 里按全局 Play 会从当前 transport 位置播整曲，无法"只听这个 clip"。
3. **播放头只在播放时画**，且按 `loop_ticks` 取模（`piano_roll.rs:2221-2231`），取模前**没有减 `origin_tick`**——clip 起点不是 `loop` 整数倍时相位偏移。
4. **切视图没有任何播放副作用**：tab 点击（`transport.rs:565-575`）与快捷键（`keyboard.rs:529-563`）各自直接写 `self.view`，没有统一入口。

## 2. 目标形态

- Edit 视图顶部一条**独立于 timeline** 的 clip ruler，覆盖 `[clip.start, clip.start + clip.span)`。
- Edit 视图下按全局 Play / Space = **试听整个 clip**：从 clip 起点播到 span 末尾，自动停止并回到起点。
- Edit 试听中切换 tab = **自动停止并清除试听范围**；文档、song loop、undo 均不受影响。
- 播放头在 ruler 上始终可见（钳在 clip 范围内）；主卷帘只在当前位置落在一个 `loop` 周期内时绘制。

## 3. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| D1 | **独立 audition range，不复用 song loop** | song loop 存于 `Project.metadata.loop_range`（序列化、走带按钮显示），借用它会让试听期间走带 loop 灯与实际状态不一致，且试听会写文档 / 弄脏工程。新增引擎级 `SetAuditionRange`，只活在引擎与 UI 会话态。 |
| D2 | **范围 = `[start, start + span)`** | 用户 2026-11-10 拍板：整块 clip（含平铺重复）。MIDI / control 读 `span_ticks`；audio 在 Edit 没有编辑器，不参与。 |
| D3 | **单次播放，引擎精确停回起点** | 到 `audition_end` 时 `stop_at_end = true` 且 `position_ticks` 置回 `audition_start`，不靠 UI 帧轮询（避免掉帧超播）。 |
| D4 | **audition 优先于 song loop** | 两者同时激活时 audition 分支先判；song loop 字段原样保留，试听结束后照旧生效。 |
| D5 | **复用全局 Play / Space，不新增按钮** | 符合「功能入口不重复」；Edit 下 Play 即试听，Timeline 下 Play 仍是整曲。 |
| D6 | **切 tab = Pause + 清范围** | 用 `pause_transport()`（保留 transport 位置），不清文档、不改 song loop。若正在录音，沿用 `pause_transport` 已有的 punch-out 路径。 |
| D7 | **ruler 与主区共用时间轴，视口扩到 span** | `ViewWindow` 右边界从 `loop` 改为 `span`，允许横向滚到平铺区；主卷帘内容仍只画一个 `loop` 周期，平铺区只在 ruler 上可见。 |
| D8 | **播放头相位修正 + 停止后静态显示** | 主区位置 = `(playhead - origin).rem_euclid(loop)`，且仅当 `0 <= playhead - origin < loop` 时画；ruler 上按绝对 tick 画并钳在 span 内。 |
| D9 | **视图切换收口 `set_view(ui, next)`** | tab 点击与快捷键共用，单一地方执行 D6 的停止判定，避免两处逻辑分叉。 |
| D10 | **不动文件格式、不动 `PROJECT_VERSION`** | audition 是会话态，不序列化、不进 undo。 |

## 4. 引擎

```rust
// commands.rs
SequencerCommand::SetAuditionRange { start_ticks: u32, end_ticks: u32 }  // 0/0 = 清
```

```rust
// sequencer.rs
audition_start_ticks: u32,
audition_end_ticks: u32,     // 0 = off

pub fn set_audition_range(&mut self, start: u32, end: u32) {
    if end == 0 || end <= start { self.audition_start_ticks = 0; self.audition_end_ticks = 0; }
    else { self.audition_start_ticks = start; self.audition_end_ticks = end; }
}
fn audition_active(&self) -> bool { self.audition_end_ticks > 0 }
```

`advance()` 的窗口分支（`sequencer.rs:761-827`）从「loop / arrangement 二选一」变成三选一，audition 在最前：

```text
if audition_active:
    as = start, ae = end
    if t >= ae: t = as                    // Play 时位置在范围外 → 回到起点
    seg_end = min(t + budget, ae)
    schedule [t, seg_end)
    if seg_end >= ae: stop_at_end = true; rewind_to = as; t = ae
    else: t = seg_end
    if stop_at_end: break
else if loop_active: ...                  // 既有回卷逻辑不变
else: arrangement end 钳制 ...
```

末尾：

```text
if let Some(start) = rewind_to { t = start; }
self.position_ticks = t;
if stop_at_end { self.playing = false; }  // pending offs 照旧在后续块排空
```

- 命令处理：`block.rs` 的 `SequencerCommand` 分派加一条 `SetAuditionRange → sequencer.set_audition_range`。
- `Play` / `Seek` 不清 audition；`Stop` 由 UI 先清范围再停（引擎不主动清，保持命令无隐式副作用）。
- RT 安全：全整数/浮点算术，无分配、无锁；`EngineCommand` 仍 `Copy`。

## 5. UI

### 5.1 会话态

```rust
struct AuditionRange { start: u32, end: u32 }   // end = start + span
// GlitcherZApp: audition: Option<AuditionRange>
```

### 5.2 Play / Space 路由

```
if !playing:
    if view == Edit && let Some(target) = edit_clip:
        (start, span) = target 的 start_ticks / span_ticks
        if span == 0: status("clip is empty"); return
        engine.seek_ticks(start)
        send SetAuditionRange { start, end: start + span }
        send Play
        audition = Some(...)
    else:
        send Play
else:
    pause_transport(); clear audition
```

- `transport.rs:185-203`（Play 按钮）与 `keyboard.rs:466-481`（Space）改为调用同一个 `toggle_transport()`，避免两份分支。
- 停止按钮：先 `clear_audition()`（发 `SetAuditionRange{0,0}`、清会话态）再 `stop_transport()`。
- 自然结束：每帧在 `was_playing && !playing` 时，若 `audition.is_some()` 则清会话态与引擎范围（位置已由引擎置回 `start`）。
- Pause 后再次 Play：从 clip 起点重新开始（D3 的单次语义），不续播半截。

### 5.3 视图切换

```rust
fn set_view(&mut self, next: MainView) {
    if self.view == MainView::Edit && next != MainView::Edit && self.audition.is_some() {
        self.clear_audition();
        self.pause_transport();
    }
    self.view = next;
}
```

- 两个入口（`transport.rs:569`、`keyboard.rs:560`）都改调 `set_view`。
- 判定抽成纯函数 `stops_audition_on_switch(old, new, audition_active) -> bool`，单测覆盖。

### 5.4 ruler

- 位置：卷帘工具栏（`piano_roll.rs:1466-1663`）与 `outer`（`:1806`）之间，高度 `20 px`（对齐 timeline 的 `RULER_HEIGHT`）。
- 横向：与音符区共用 `geom` 的时间映射（`x_abs(tick) = t0_x + tick * ticks_to_x`）；左侧 `KEYS_WIDTH` 区留空，画 `bars` 角标。
- 内容：按 `bar_ticks` 从绝对 tick 0 起算小节线，bar 号 `tick / bar_ticks + 1`；覆盖 `[clip.start, clip.start + span)`，视口裁剪。clip 起点与终点画边界标记。
- 交互：左键点击 / 拖动 → `RollIntent::Seek { tick }`（钳在 clip span 内），app 层 `engine.seek_ticks`，**不进 undo**。
- 样式沿用 timeline ruler 的线条/字号，但**不画 marker / loop brace**，避免被误认为 timeline。
- 新增参数：`piano_roll::show(..., clip_span: i64, ...)`；`ViewWindow::of` 与 `clamp_scroll` 右边界改用 `start + span`。

### 5.5 播放头

| 区域 | 规则 |
| :--- | :--- |
| ruler | 只要 Edit 目标存在就画，绝对 tick 位置，钳在 `[start, start+span]`；颜色沿用 `Color32::YELLOW` |
| 主卷帘 | 仅当 `0 <= playhead - origin < loop` 时画（当前周期内）；位置用 `rem_euclid` 修正后的相位 |
| velocity / overlay lane | 与主卷帘同一条件与相位（修掉 `piano_roll.rs:2225 / 2264` 未减 origin 的问题） |

### 5.6 文档同步

- `docs/manual.md`：Edit tab 一节补「ruler / 试听 / 切 tab 自动停」。
- `docs/shortcuts.md`：Space 在 Edit 下为「试听当前 clip」，其余行为不变。

## 6. 边界与验收

| 场景 | 期望 |
| :--- | :--- |
| Edit 按 Play | `seek` 到 `clip.start`，播到 `start+span` 自动停，位置回到 `clip.start` |
| Edit 按 Space | 与 Play 完全一致 |
| 播到末尾后再按 Play | 从 clip 起点重新播一遍 |
| 播放中点 Play / Space | Pause，位置保留，范围清除；再按 Play 从 clip 起点重播 |
| 播放中点 Stop | 停止并回 0；范围清除 |
| 播放中切到 Timeline / Patch / Inspector | Pause + 清范围；回到 Timeline 按 Play 播整曲 |
| Timeline 播放中切到 Edit | **不停止**（audition 未激活）；Play 按钮此时显示 Pause，按一次先暂停整曲 |
| 平铺 clip（span > loop） | 试听整段 span；ruler 覆盖整段；主区仍只显示一个周期，平铺区只能在 ruler 上看到 |
| clip span = 0 / 无目标 | 不启动试听，状态栏提示 |
| audio clip | Edit 无编辑器，仍显示 "Audio clips have no editor."，不启动 |
| song loop 同时开启 | audition 优先；试听结束后 song loop 照旧 |
| 试听中拖动 clip 边界 / 删除 clip | 立即停止试听并清范围（范围已失效） |
| 停止状态 | ruler 仍画静态播放头；主区仅当位置在周期内时画 |
| clip 起点非 loop 整数倍 | 主区播放头相位正确（不再偏移） |
| 切 tab 时正在录音 | `pause_transport` 的既有 punch-out 生效，与试听清理互不干扰 |

## 7. 分阶段

| 阶段 | 内容 | 依赖 |
| :--- | :--- | :--- |
| **Aud1** 引擎 | `SetAuditionRange` + `Sequencer` 字段/分支 + 4 条单测（末尾停回起点、优先于 song loop、退化范围、范围内 seek） | 无 |
| **Aud2** 试听路由 | `audition` 会话态、`toggle_transport` 收口、自然结束清理、`set_view` 收口与切 tab 停止、策略纯函数单测 | Aud1 |
| **Aud3** ruler + 播放头 | `clip_span` 参数、`ViewWindow` 扩到 span、ruler 绘制与 seek、播放头相位修正/静态显示、roll fixture 测试、文档同步 | Aud2 |

## 8. 明确不做

- 试听循环（D3 单次）。
- Edit 内独立播放按钮（D5）。
- audio clip 的 Edit 波形编辑器 / 音频试听（仍无编辑器）。
- 把平铺区在主卷帘里展开（D7）。
- 修改 song loop 语义、把 audition 序列化进工程文件。
- 试听范围的波形预分析 / 淡入淡出。