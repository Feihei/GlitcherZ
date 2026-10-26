# Spec: Audio Clip 裁剪与模态编辑（U11）

> 状态：已交付（2026-10-26，A–E 全部落地，全仓 fmt/clippy/test 绿）。对应 `plan.md`「U11 audio clip 裁剪 + 模态编辑」。
> 前置：U6 audio track 已交付（`AudioTrackProcessor` / clip 表 structure 队列 / timeline audio 行）；
> M4b modal（G / T / Shift+D，两表推理）已交付；U5 `sample` 解码 + 插值内核可平移。
> 本 spec **修订 U6 D1「变速跟随」**，并为 audio clip 开放 G / T / Shift+D。

## 1. 背景与问题

用户实测反馈两条：

1. **播放无声（已单独修复，非本 spec 范围）**：`add_audio_track` 在文档 cable push 之后又重算 `first_free_channel`，通道已被自己占用 → `None` → `return` 跳过 `connect_nodes`，audio 节点成孤立节点。导出走文档 `in_port` 故正常。此 bug 已修，本 spec 不重复。
2. **audio clip 的边界语义**：现状 U6 D1「**变速跟随**」——clip 的 `span_ticks` 固定，wav 内容按比例重采样铺满 span，帧位 = `(t − start)/span × wav_frames`。后果是**拖 clip 边缘 = 变速变调**（tape-style）。用户实测认为这不符合采样素材的直觉：

   > "采样 clip 的边界是把原 audio 文件**裁剪**出来一节的行为，而不是时间上的**拉伸**；T 正是调整左右两端裁剪边界的功能。"

   G / Shift+D 同理：现状 `arm_modal`（`timeline.rs:2503`）直接拒绝 audio 选区，提示"audio clips edit by direct drag — G / T / Shift+D are MIDI/curve only"。用户诉求是让 audio 也有移动 / 复制 / 裁剪。

**核心洞察（本 spec 的立足点）**：要「拖边缘不变调」，clip 的时长语义**必须从 tick 域挪到秒（绝对时间）域**——tick 长度固定而内容不重采样，在数学上不成立。这直接推翻 U6 D1 选址 tick 域的理由（"纯函数、loop/seek 零特判"），因此这是一次**时间基修订**，不是一个字段加法。

## 2. 核心决策

| # | 决策 | 结论 | 理由 |
| :-- | :-- | :-- | :-- |
| D1 | 播放速率 | **1× 不重采样**：wav 以原生速率播放（只做 wav_sr → engine_sr 的采样率转换，非变调） | 用户实测：采样边界 = 从原文件裁一段；U6 D1 的变速变调不符直觉 |
| D2 | 时间基 | **绝对时间（不跟随 tempo 曲线）**：tick↔秒 用 `base_bpm` 换算，audio 不随 tempo 曲线改变速率或音高 | "不变调"的必然推论；`base_bpm` 纯函数 → 保留 tick 域无需游标的优点。代价见 D3 / §5 |
| D3 | 视图长度 | clip 的 tick 长度 = `source_len / sec_per_tick(base_bpm)`，**派生不存**；改 tempo 曲线不动 audio，改 `base_bpm` 才动 clip 视觉长度 | 与 D2 自洽；一个字段单一来源（秒），避免 tick/秒 双写漂移 |
| D4 | 裁剪窗口 | `AudioClip` 加 `source_offset`（秒，wav 内起点）+ `source_len`（秒，窗口长度）；位置仍 `start_ticks`；**删 `span_ticks`** | 裁剪语义必须记录"从 wav 哪取"——U6 D5 显式省略的字段 |
| D5 | T 语义 | **右端 T（裸）= 改 `source_len`（裁尾）**；**左端 T（`s` 翻转）= 同时改 `start_ticks` + `source_offset`（裁头，剩余内容时间轴位置不变）**；边界吸附到 grid step，最小 1 step | 用户："T 正是调整左右两端裁剪边界"。裁头/裁尾的位移/窗口联动见 §4 |
| D6 | G 语义 | 移动：改 `start_ticks` + 跨 audio 轨；复用 midi 的 `grab_targets` 逻辑（三表化） | 用户："G 沿时间轴移动 clip 或跨轨道移动 clip" |
| D7 | Shift+D | **复制（clone，引用同一 wav path，不复制文件）+ G**；副本落在 offset 位，选区转副本；含跨轨 | 用户："Shift+D 就是先创建副本再执行 G" |
| D8 | 三表化 | modal 几何 / targets / ghost 绘制 / app batch 从「midi + control 两表」改「midi + control + audio 三表」 | 现状 `grab_targets` / `stretch_targets` / `clip_geometry` / ghost 行键 `(u8::from(is_control), track)` 全部硬编码两路 |
| D9 | 跨轨重推 | audio 移动 / 复制**跨轨**时，一次编辑同时重推源轨 + 目标轨两张 clip 表（structure 队列 `SetAudioClips`），合并为**一步 undo** | audio 表含 `Arc<SampleData>` 非 Copy，走 structure 队列；`relocate_in` 现为同类搬迁 |
| D10 | 迁移 | `PROJECT_VERSION` 6 → 7；`migrate_v6_to_v7`：`span_ticks` → `source_len`（按当时 `base_bpm` 换算为秒），`source_offset = 0` | 字段替换无法靠 serde default 无损兼容（span 与 len 语义不同） |

## 3. 播放数学

```
sec_per_tick = 60 / (base_bpm × PPQ)          // PPQ = 960
elapsed_sec  = (song_tick − start_ticks) × sec_per_tick
if 0 ≤ elapsed_sec < source_len:
    frame = (source_offset + elapsed_sec) × wav_sample_rate
    → 线性插值（内核平移自 U5 `render_voice` / 现 `render_clip`）
else:
    静音（clip 窗口外）
```

- 与 U6 D1 同样**纯 tick 域函数**：位置即状态，loop 回卷 / seek 零特判——D2 选 `base_bpm` 正是为了保住这一点。
- `source_offset + elapsed_sec` 超出 wav 尾（`source_len` 超过文件剩余）时**读 0（静音）**，不 wrap、不 clamp 到最后一帧（避免常噪音）。
- 多 clip 同轨仍**叠加**（U6 D7 不变）。

## 4. T（裁剪）的位移/窗口联动

记 `spt = sec_per_tick`、clip 原 `(T0, O0, L0)`，`Δ = 裁剪边界的 tick 位移`。

- **裁尾（右端，裸 T）**：`start` 不变，`source_len = L0 + Δ·spt`。播放窗口向后延伸/收缩，起点内容不动。
- **裁头（左端，`t+s`）**：`start = T0 + Δ`，`source_offset = O0 + Δ·spt`，`source_len = L0 − Δ·spt`。
  - 不变式验证：裁头后播放 tick `t ≥ T0+Δ` 的帧 = `(O0 + Δ·spt + (t − T0 − Δ)·spt) × sr = (O0 + (t − T0)·spt) × sr`，**与裁前同 tick 的内容完全一致** → 剩余内容在时间轴上的位置/内容不变，仅暴露窗口左移。

**边界钳制**：
- `source_offset ≥ 0`（不能裁到文件头之前；左端最多拖到 `Δ = −O0/spt`）。
- `source_len ≥` 1 个 grid step（防止零长 clip）；超出文件尾部分听觉为静音（保留，不自动截断——用户可能先拖后调）。
- 吸附：落点吸附到当前 grid step（同 timeline 既有 `SNAP_TICKS`）；bar 吸附不强制（采样裁剪常需精细）。

## 5. 已知取舍（D2 的代价，实现前必须知情）

- **audio 不跟随 tempo 曲线**：tempo 曲线改变 midi 事件在同一 tick 的绝对时间，而 audio 按 `base_bpm` 走，二者在**有 tempo 曲线的段落会错位**。这是"不变调"的固有含义（等同主流 DAW 的"不 warp / 绝对时间"音频）。
- **clip 视觉长度随 `base_bpm` 变化**：改工程基础 BPM 时 audio clip 在 tick 轴上的长度随之伸缩（内容不变）。midi clip 不伸缩，二者表现不同——UI 需在文档/提示中让这可见（现状 midi clip 改 BPM 长度不变，audio 会变）。
- **若未来要 audio 跟随 tempo 曲线**（精确同步），需 block runner 维护 `song_sec` 累积 + `seek`/loop 重锚（放弃纯函数）——**本缺口已接受，未来方案见 §12**。

## 6. 数据模型

```rust
pub struct AudioClip {
    pub path: String,
    pub start_ticks: u32,        // 位置（tick，bar 吸附）——不变
    pub source_offset: f64,      // NEW：裁剪起点（秒，wav 内，≥ 0）
    pub source_len: f64,         // NEW：裁剪窗口长度（秒，≥ 1 step）
    pub gain: f32,               // 不变
    // span_ticks 删除
}
```

- `Project` 加辅助 `clip_span_ticks(&self, clip) -> u32` = `round(source_len / sec_per_tick(base_bpm))`（`Placement` / timeline 布局 / `export_end_ticks` 统一读它，杜绝各处自算）。
- `validate`：`source_offset ≥ 0`、`source_len > 0`、`path` 非空（沿用 U6）。
- `Placement` impl（`timeline.rs`）的 `start()` / `span()` 改读派生值；`set_loop` 仍 no-op（audio 无 loop 语义）。

## 7. 引擎改动

- `RtAudioClip`：`{ start_ticks: f64, source_offset: f64, source_len: f64, data: Arc<SampleData>, gain: f32 }`（去 `span_ticks`）。
- `ProcessContext`：`song_ticks` / `transport_dt_ticks` 保留；`AudioTrackProcessor::process` 用块起始 `song_ticks` + `sec_per_tick`（block runner 依 effective_bpm 的 `base_bpm` 算，随块传入或从 ctx 读）算 `elapsed_sec`。**注意**：D2 下 `sec_per_tick` 恒用 `base_bpm`（不走 tempo 曲线的 effective_bpm），与 sequencer 的 `advance` 所用的 bpm **有意不同源**。
- `render_clip` 数学替换为 §3；超出窗口/文件尾判定同 §3。
- RT 契约不变：纯循环、线性插值、无分配、无锁（U6 D7 铁律）。

## 8. UI / 交互改动

- **`arm_modal`（`timeline.rs:2496`）**：删除 audio 拒绝分支（2503-2510）；G / Shift+D 的"单一种类"校验（`selection[0].lane` 全等）保留——audio 与 midi/control 混选仍拒绝（跨种类移动无意义）。
- **三表化**（D8）：
  - `grab_targets` / `stretch_targets` / `clip_geometry` / `filter_covered` / `track_spans` / `clamp_row_delta` 签名加 `audio_tracks: &[AudioTrack]` 或抽象为统一行访问。
  - **`Placement` trait 方法加 `sec_per_tick: f64` 参数**（`span` / `set_span` / `set_loop` / `retain_from`；`start` / `set_start` 不需要）：MIDI/control 忽略它，audio 用 base-tempo 换算派生 tick span、把写入的 tick 长度折回秒窗口。trait 无 `Project` 参数，这是注入 tempo 的实际落点（由 `TimelineParams.sec_per_tick` 一路传到 `lane_row` / 几何函数）。
  - ghost 绘制：颜色/行键从 `bool is_control` 改三值（`(0|1|2, track)`）。
  - app 层 `timeline_move_clips_batch` / `timeline_duplicate_clips_batch` / `timeline_stretch_clips_batch` 补 `TimelineLane::Audio` 分支。
- **T 落地**：`timeline_stretch_clips_batch` 的 audio 分支按 §4 写 `start_ticks` / `source_offset` / `source_len`（若改动触及文件头/尾则钳制）。
- **G / Shift+D 落地**：移动 / 复制后调 `send_audio_clips_for_track`；跨轨时对源轨 + 目标轨各推一次，整段包在 `suspend_history` + 单次 `commit_edit` 里（D9）。
- **direct drag（拖身/resize）**：现走 `timeline_move_resize_clip`（已有 Audio 分支）。语义对齐 T——resize **右缘 = 裁尾**、**左缘 = 裁头**（与裸 T / `t+s` 一致），不再改 `span` 重采样。

## 9. 实现拆分（U11）

- [x] **U11-A model**：`AudioClip` 字段替换（D4）+ `clip_span_ticks` 派生 + `validate` + `PROJECT_VERSION` 6→7 迁移（D10）+ RON 往返 / 迁移测试。✅ 2026-10-26
- [x] **U11-B engine**：`RtAudioClip` 字段替换（§7）+ `render_clip` 秒域数学 + 窗口/文件尾静音 + 铁律两组 + 数学测试（裁剪窗口内/外、offset 起点、裁头内容不变式、多 clip 叠加）+ RT 审计。✅ 2026-10-26
- [x] **U11-C UI 装配**：`send_audio_clips_for_track` 携带 `source_offset` / `source_len`；`timeline_move_resize_clip` 语义对齐（§8）；`export_end_ticks` 改读 `clip_span_ticks`。✅ 2026-10-26
- [x] **U11-D modal 三表化**：`arm_modal` 放行 audio + 几何/targets/ghost/batch 三表化（D8）+ 跨轨重推 & 一步 undo（D9）。✅ 2026-10-26
- [x] **U11-E 测试与验收**：timeline 测试（audio G 移动同轨/跨轨、Shift+D 复制选区转移、T 裁头/裁尾、min-step 钳制、混选拒绝）+ 端到端（clip 裁剪后播放内容与导出一致）+ 全仓 fmt/clippy/test 绿 + 冒烟。✅ 2026-10-26（model/engine/timeline 测试 + 全仓绿；GUI 冒烟待用户实测）

## 10. 边界与明确不做

- **不做** timestretch / 变速不变调（sinc 多相）——本 spec 是"不重采样"（D1），不是"变速不变调"。
- **不做** warp / transient 检测 / beat 切片。
- **不做** audio 跟随 tempo 曲线（D2 取舍，精确同步留后续）。
- **不做** 拖放文件进 timeline、音频录音。
- **不做** audio clip 的音高 / 反向 / fade-in-out（无内容语义）。
- **不做** Ctrl+拖复制（timeline 无此入口，Shift+D 为唯一复制入口）。
- **不做** 跨种类（audio ↔ midi/control）混合移动/复制。

## 11. 影响面（改动既有文件）

- `crates/glitcherz-model/src/lib.rs`：`AudioClip`、`clip_span_ticks`、`validate`、`migrate_v6_to_v7`。
- `crates/glitcherz-model/src/file.rs`：`PROJECT_VERSION` 6→7 + 迁移链分派（现为 `version < PROJECT_VERSION → migrate_v5_to_v6`，需改为按源版本逐级迁移）。
- `crates/glitcherz-engine/src/realtime/audio_track.rs`：`RtAudioClip`、`render_clip`。
- `crates/glitcherz-ui/src/lib.rs`：`send_audio_clips_for_track`、`add_audio_clip`（建 clip 初始化 offset/len）、`timeline_move_resize_clip`、三个 batch 函数、`export_end_ticks`。
- `crates/glitcherz-ui/src/timeline.rs`：`arm_modal`、`grab_targets` / `stretch_targets` / `clip_geometry` / ghost 绘制、`Placement impl AudioClip`。
- `.agents/spec/audio-track.md` §5 的「不做 offset 头部裁剪」「不做 G/T/Shift+D」两条**作废**（由本 spec 取代）。
## 12. 未来方案：audio 跟随 tempo 曲线（精确同步）

> **状态：已接受缺口，不在 U11 实施。** 本 spec §5（D2）刻意让 audio 用 `base_bpm`、不跟随 tempo 曲线；有曲线的段落里 audio 与 midi 错位是已知取舍。本方案为后续精确同步预留，触发条件：用户实测该场景错位不可接受。
> **关键约束**：方案仍要守住 D1「不变调」——即"跟随 tempo 曲线"指 audio 的**真实秒位置/时长与 midi 对齐**，不是重采样变调。

思路：把时间基点从常量 `base_bpm` 换成 **tempo 曲线的 tick↔秒 积分**，位置随曲线浮动、内容仍秒域固定。

1. **定义 `tick_to_sec(t)`** = `∫₀ᵗ sec_per_tick(effective_bpm) dt`（tempo 曲线的积分），`sec_per_tick(bpm) = 60/(bpm·PPQ)`；逆映射 `sec_to_tick` 供视图派生。
2. **block runner 维护 `song_sec`**（transport 真实秒位置）：每块 `song_sec += dt_ticks × sec_per_tick(本块 effective_bpm)`；`seek` / loop 回卷时用 `tick_to_sec` 重锚。→ 放弃 §3 的纯函数性质，引入游标同步与 seek/loop 特判（U6 D1 当年的立论基础）。
3. **clip 表改存 `start_sec`**（控制线程构造表时用 `tick_to_sec(start_ticks)` 算）+ `source_offset` / `source_len` 不变；播放用 `elapsed_sec = song_sec − start_sec`（§3 其余数学不变）。
4. **tempo 曲线编辑** → 重推全部 audio 轨 clip 表（`start_sec` 随曲线变）；`base_bpm` 改动同理。
5. **`clip_span_ticks` 派生**改读 `sec_to_tick(source_len)` 逆映射，clip 视觉长度随 tempo 曲线变化。

代价与风险：放弃 tick 域纯函数；`seek` / loop 需重锚（U1 曾实测"想当然的 wrap 映射会把 off 提早"的同类坑，实现时须逐块验证）；tempo 曲线每次编辑触发全 audio 轨表重推（structure 队列压力，需按帧分片或去抖）。