# SoundCraft 对标调研：MIDI Editor

> 参考仓库：`.ref/soundcraft`（storytold/soundcraft，v0.4.0，Rust + egui，对标 Pro Tools）
> 调研日期：2026-10-10

## 一、结论先行

SoundCraft 的 MIDI Editor 本身（`crates/ui-egui/src/midi_editor.rs`，仅 354 行）**并不复杂**，UI 层是 egui 直接 `painter` 自绘，没有用任何 widget 抽象。

真正值得借鉴的是它**围绕 MIDI Editor 的一套基础设施**，共 5 个点，按借鉴价值排序：

| 优先级 | 借鉴点 | SoundCraft 位置 | 我们的现状 | 价值 |
| :--- | :--- | :--- | :--- | :--- |
| P0 | 命令注册表 + 事务性 execute | `engine/src/cmd/mod.rs`、`engine/src/lib.rs` | 无（UI 直接改 model） | 极高 |
| P0 | PPQ=960 + Note/Sequence 数据模型 | `midi/src/sequence.rs` | PPQ=96，Note 无 channel | 高 |
| P1 | Event Operations（量化/力度/时值） | `midi/src/ops.rs` | 无 | 高 |
| P1 | GridValue 网格体系（附点/三连音/绝对网格） | `time/src/grid.rs` | 硬编码 `SNAP_TICKS=6` | 高 |
| P2 | SMF 导入导出 + Event List / Score Editor | `midi/src/smf.rs`、`ui-egui/src/{windows,score_editor}.rs` | 无 | 中 |

---

## 二、P0-1：命令注册表 + 事务性 execute

### 是什么

所有编辑行为都是**注册过的命令**，有唯一 id、菜单路径、快捷键、参数文档、启用条件：

```rust
cmd!("midi.note_edit", "Edit Note", [], None,
     "{clip, index, pitch?, start_ticks?, length_ticks?, velocity?}",
     always, note_edit)
```

三宏变体：`cmd!`（可撤销 + 记 journal）、`cmd!(noundo ...)`、`cmd!(query ...)`。

### 为什么值得抄

`Engine::execute()` 做了三件我们目前完全没有的事：

**1. panic 不炸进程，且文档自动回滚**

```rust
let before = Arc::clone(&self.doc);
let result = std::panic::catch_unwind(AssertUnwindSafe(|| (spec.run)(self, params)));
// panic → self.doc = before; 返回 EngineError::Internal
```

**2. 命令失败不留半成品**

```rust
Err(e) => {
    if !Arc::ptr_eq(&before, &self.doc) { self.doc = before; }  // 强制回滚
    Err(e)
}
```

我们的 `piano_roll.rs` 现在是 `clip.notes.push(...)` 直接改，改到一半出问题就是脏状态。

**3. CoW 快照做 undo，O(1) 入栈**

`Arc<Session>` + `Arc::make_mut`。我们如果之后也要 undo，这个方案比 diff-patch 简单得多。

### 关键：execute_merged —— 手势合并

这是**手感的关键**。拖 fader / 拖音符时每帧都调 `execute`，但不能产生几百条 undo：

```rust
pub fn execute_merged(&mut self, id: &str, params: &Value, key: &str) -> Result<Value> {
    let continuing = self.merge_key.as_deref() == Some(key);
    let before_len = self.undo.len();
    let r = self.execute(id, params)?;
    if continuing && self.undo.len() == before_len + 1 && before_len > 0 {
        self.undo.pop();   // 丢掉刚压入的，前一步已持有手势前的快照
    }
    self.merge_key = Some(key.to_string());
    Ok(r)
}
pub fn end_merge(&mut self) { self.merge_key = None; }
```

soundcraft 的 velocity lane 拖动就是这么用的：

```rust
app.engine.execute_merged("midi.note_edit", &json!({...velocity: v}), &format!("vel:{}", cid.0));
```

**我们照抄的话**：`piano_roll.rs` 的拖动应该走 `execute_merged(id, params, "roll:drag")`，松手调 `end_merge()`，一次拖动 = 一步 undo。

### 一个亮点：命令自检测试

```rust
#[test]
fn every_command_survives_empty_params() {
    for demo in [false, true] {
        for c in command_specs() {
            if matches!(c.id, "app.quit") { continue; }
            let r = e.execute(c.id, &json!({}));
            if let Err(EngineError::Internal(id, msg)) = r { panic!("{id} panicked: {msg}"); }
        }
    }
}
```

所有命令用空参数跑一遍不许 panic。我们的命令层建起来时直接上这个测试。

---

## 三、P0-2：数据模型

### SoundCraft 的 Note

```rust
pub struct Note {
    pub pitch: u8,              // 0..=127
    pub velocity: u8,           // 1..=127  ← 注意是 u8 整数
    pub release_velocity: u8,   // note-off 力度
    pub channel: u8,            // 0..=15
    pub start: i64,             // tick（i64，允许负值中间态）
    pub length: i64,            // tick，恒 > 0
}
impl Note { pub fn end(&self) -> i64 { self.start.saturating_add(self.length) } }
```

`Sequence { notes: Vec<Note>, ctrls: Vec<CtrlEvent> }` —— **CC / PitchBend / Aftertouch 和音符平级存放**。

### 我们的 Note

```rust
pub struct Note {
    pub start: u32,     // tick，u32
    pub length: u32,
    pub note: u8,
    pub velocity: f32,  // ← 0.0~1.0 浮点
}
```

### 差距与建议

| 项 | 我们 | SoundCraft | 建议 |
| :--- | :--- | :--- | :--- |
| PPQ | 96 | **960** | 必改。96 太粗，1/64 三连音 = 4 tick，量化误差明显；960 是业界标准（SMF 也用 960），导入导出不用换算 |
| velocity | `f32` 0..1 | `u8` 1..127 | 建议改 u8。f32 在 MIDI 语义下无意义，且 SMF 转换要来回 clamp |
| start 类型 | `u32` | `i64` | 建议 `i64`。拖动时需要负值中间态，现在得先 clamp 再 clamp 回来 |
| channel | ✗ | ✓ | 加。SMF 是多通道的，现在导入多通道 MIDI 会全糊成 channel 0 |
| release_velocity | ✗ | ✓ | 低优先，但有了能正确导出 SMF |
| CC / 弯音 | ✗ | ✓ | 中优先。Pro Tools 的钢琴卷帘支持 CC lane |

### 排序不变量

```rust
pub fn sort(&mut self) {
    self.notes.sort_by_key(|a| (a.start, a.pitch, a.channel, a.length, a.velocity));
    self.ctrls.sort_by_key(|c| c.tick);
}
```

每条命令改完都必须 `sort()`。注意副作用：**`Vec<usize>` 索引选择集在 sort 后会失效**。SoundCraft 是靠"命令返回新 index"来解决的（见下），这是个坑，我们如果照搬要注意。

### remove_duplicates —— 导入脏数据的兜底

```rust
/// 同 (channel, pitch, start) 保留最长的那个
pub fn remove_duplicates(&mut self) -> usize;
```

真实 MIDI 文件里重复音符很常见（叠轨、量化碰撞）。导入时自动跑一遍，省得后面编辑器里出现"删不掉的音符"。

---

## 四、P1-1：Event Operations

Pro Tools 的 Event 菜单（MIDI Operations）—— 这是**编辑效率的核心**，我们目前一个都没有。

### 量化（`QuantizeOptions`，8 个参数）

```rust
pub struct QuantizeOptions {
    pub grid_ticks: i64,
    pub strength: f32,          // 0..=1，往网格线靠拢的比例（Pro Tools 的 Q 值）
    pub swing: f32,             // 0..=1，每隔一条线延迟 swing*grid/3 → 1.0 = 三连音感
    pub include_within: f32,    // 只量化距离目标 < include*半格的音符
    pub exclude_within: f32,    // 已经很准的音符不动（保护人played的音符）
    pub quantize_ends: bool,    // 是否也量化音符结束
    pub preserve_duration: bool,
    pub random: f32,            // 量化后随机偏移，避免机械感
}
```

**`exclude_within` 是最实用的一个**：录进来的 MIDI 通常大部分已经挺准，只把跑偏的拉回去，已准的不动。这个"容差窗口"概念我们一定要有。

**`swing` 的实现值得看**：

```rust
fn nearest_line(t: i64, grid: i64, swing: f64) -> i64 {
    let swing_off = round_i64(swing * grid as f64 / 3.0);
    let k = t.saturating_add(grid / 2).div_euclid(grid);
    // 在 k-1, k, k+1 三条候选线里取最近的
    for kk in [k-1, k, k+1] {
        let line = kk * grid + if kk.rem_euclid(2) == 1 { swing_off } else { 0 };
        ...
    }
}
```

swing 不是简单的"偶数 tick 偏移"，而是在奇数网格线上加偏移，然后**在三条候选线里搜最近的**（因为 swing 偏移后最近线可能不是理论最近）。这个细节我们做 swing 时别写错。

### 力度（`VelocityOp`）

```rust
pub enum VelocityOp {
    Set(u8), Add(i32), Scale(f32),
    Limit { min: u8, max: u8 },
    Ramp { from: u8, to: u8 },   // 按时间线性渐变 —— 做 crescendo 很好用
    Humanize(u8),                // 随机 ±amount
}
```

`Ramp` 和 `Humanize` 我们完全没有，但都是 5 行代码的事，效果立竿见影。

### 时值（`DurationOp`）

```rust
pub enum DurationOp {
    Set(i64), Add(i64), Scale(f32),
    Legato { gap: i64 },        // 每个音延长到下一个音开始前 gap
    RemoveOverlap,              // 同 pitch 的重叠音截断（旋律线单音化）
}
```

`Legato` + `RemoveOverlap` 这两个是"整理乱录 MIDI"的必备。

### 一个关键设计：ops 全部原地改、不改长度

```rust
//! All operations work in place on a slice of notes and never change the slice length,
//! so they can be applied to a selection.
pub fn transpose(notes: &mut [Note], semitones: i32)  // pitch clamp 0..127，不 drop
```

**只 clamp 不丢弃**。转调超出范围就堆在边界，而不是把音删掉。这个语义要抄——很多 DAW 这里会丢音符，用户会觉得"我的音没了"。

### 随机化用确定性 XorShift

```rust
pub const DEFAULT_SEED: u64 = 0x5EED_50DC_4AF7_0001;
```

`quantize` / `humanize` 的随机部分全部走 seeded XorShift，`*_with_seed` 变体可显式传种子。**可测试 + 可复现**，我们做 random 类编辑必须坚持这一点。

---

## 五、P1-2：GridValue 网格体系

我们的 `piano_roll.rs` 里是硬编码：

```rust
pub const SNAP_TICKS: u32 = 6;   // 1/64 note，固定死
```

SoundCraft 的 `GridValue`：

```rust
pub enum GridValue {
    Note { value: NoteValue, dotted: bool, triplet: bool },
    Seconds(f64),      // Min:Secs 网格
    Frames(i64),       // 视频帧
    Samples(i64),
}
pub enum NoteValue { Bar, Half, Quarter, Eighth, Sixteenth, ThirtySecond, SixtyFourth }
```

**关键实现细节**：

```rust
pub fn line_at_or_before(&self, pos, ...) -> Samples {
    let t = map.samples_to_ticks_f(pos, sr).floor() as i64;
    // 网格线从每个小节线重新开始 → 奇数拍号也能对齐
    let bb = map.bar_beat_at_tick(t);
    let bar_tick = map.bar_start_tick(bb.bar);
    let step = Self::note_ticks(value, dotted, triplet, map, bar_tick);
    let k = (t - bar_tick).div_euclid(step);
    map.tick_to_samples(bar_tick + k * step, sr)
}
```

**网格从每个小节线重新计数**，不是从 0 tick 全局计数。3/4、5/4、7/8 这些拍号下，全局计数会让网格和小节线错位。这是个我们一定会踩的坑。

另外附点/三连音是 tick 级运算：`dotted → t * 3 / 2`，`triplet → t * 2 / 3`。

---

## 六、P2：周边视图

### Event List（`ui-egui/src/windows.rs:225`）

Pro Tools 的事件列表 —— 表格形式列出所有音符（Start / Note / Vel / Length），可直接改数值和删除。**120 行代码**，用 `egui::Grid` + `DragValue` 就成了。

对我们的价值：钢琴卷帘不适合精确改单个音的 velocity（要拖 velocity lane），Event List 是精确编辑的补充。这个功能便宜又实用。

### Score Editor（`ui-egui/src/score_editor.rs`，159 行）

五线谱视图。**不推荐现在抄** —— 它自己注释里说"A readable sketch rather than an engraving engine"（可读的草图，不是制谱引擎）。等 MIDI 编辑基础打牢了再说。

### SMF 导入导出（`midi/src/smf.rs`，541 行）

**这个必须抄，而且要一开始就抄**，否则后面格式定了再改数据结构代价很大。

值得注意的边界处理（都是我们一定会遇到的）：

| 情况 | 处理 |
| :--- | :--- |
| division 为 SMPTE | 按 120bpm 近似换算；29.97 drop-frame 单独处理 |
| running status | 支持 |
| note-on velocity 0 | 当作 note-off，力度取 64 |
| 同 pitch 重叠音 | FIFO 匹配（先开先关） |
| 悬挂音（note-off 缺失） | 在轨道末尾闭合 |
| 零长度音 | 强制 length = 1 tick |
| format 1 无音符的conductor 轨 | 折叠进全局 tempo/meter/marker |
| 畸形输入 | 全部返回 `MidiError`，带字节 offset |

安全上限：`MAX_EVENTS = 5_000_000`、`MAX_TRACKS = 4096`。原话："The reader treats its input as hostile"（把输入当敌对数据）。

另外 crate 根部挂了 lint：
```rust
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, ...)]
```

### 一个易被忽略的细节：note 颜色编码 velocity

```rust
let k = 0.45 + 0.55 * f32::from(n.velocity) / 127.0;   // 亮度 = 力度
```

拖 velocity lane 时能直接看出力度分布。零成本，值得抄。

---

## 七、我们该抄什么（按优先级）

### 第一步：地基（不做这个，后面全白干）

1. **PPQ 96 → 960**，同步调整所有 `TICKS_PER_STEP` 常量
2. `Note.velocity`: `f32` → `u8`；`start/length`: `u32` → `i64`；加 `channel`
3. `Sequence { notes, ctrls }` 替代裸 `Vec<Note>`
4. `sort()` 不变量 + `remove_duplicates()`
5. `crates/resoz-command` 新 crate：命令注册表 + `execute`（panic 捕获 + 回滚）+ `execute_merged`

> ⚠️ 这一步动的是 `resoz-model` 和 `resoz-engine`，`sequencer.rs`（898 行）和 `piano_roll.rs` 都要跟着改。建议单独一个 commit，标注 breaking change。

### 第二步：编辑能力

6. `GridValue` 替换硬编码 `SNAP_TICKS`，网格线从 bar line 起算
7. `ops.rs`：quantize（含 exclude_within 容差 + swing）/ velocity（Ramp/Humanize）/ duration（Legato/RemoveOverlap），全部走 seeded XorShift
8. piano roll 拖动接 `execute_merged`
9. velocity lane（soundcraft 的实现只有 ~20 行，直接抄）
10. 橡皮筋框选 + 多选 + 方向键微调（半音/八度、移动一格）

### 第三步：外围

11. SMF 导入导出（趁早，`Sequence` 结构一定还会变）
12. Event List 窗口

---

## 八、明确**不**建议抄的

- **Score Editor** —— 159 行的五线谱草图，做的价值不如把 piano roll 做扎实
- **command journal / macro recording** —— 有意思但我们现在没有 macro 需求
- **CI/release 三套 workflow** —— 跟 MIDI 无关

---

## 九、给我们的两条铁律补充建议

参考 SoundCraft 的做法，在 `AGENTS.md` 的实时铁律之外补：

1. **UI 层不得直接修改 model**。所有编辑经 `execute(cmd_id, params)`，以便统一撤销 / 回滚 / 审计。
   （我们现在 `piano_roll.rs` 签名是 `clip: &mut MidiClip`，直接改。）

2. **MIDI ops 纯函数化 + 确定性随机**。`ops.rs` 里的函数签名一律 `fn(&mut [Note], Options)`，不碰 clip/track/context，随机部分走 seeded XorShift。这让它们可以脱离 DAW 单独测试。