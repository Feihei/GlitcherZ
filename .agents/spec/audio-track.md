# Spec: Audio Track（时间轴音频轨）

> 状态：已收官（2026-10-24，A1–A5 全部落地）。对应 `plan.md`「U6 audio track」。
> 前置：U5 采样播放器已交付（`engine::sample` 解码 + `SampleLibrary`
> 缓存 + 插值内核可平移）；timeline 视图 / M4b clip 手势 /
> `Placement` trait 已就绪。

## 1. 问题

时间轴只能摆 MIDI clip。配唱、采样拼接、录音素材（"一段 wav 铺在
arrangement 上混音"）没有载体——U5 解决的是"wav 当乐器弹"（note
触发、pitch 映射），U6 解决的是"wav 当素材铺"（时间轴位置驱动、无
音符语义）。二者共用解码与缓存基建。

## 2. 核心决策

| # | 决策 | 结论 | 理由 |
| :-- | :-- | :-- | :-- |
| D1 | tempo 语义 | **变速跟随**：clip 的 `span_ticks` 固定，wav 内容按比例重采样铺满 span。帧位 = `(t − start)/span × wav_frames`，**数学完全闭合在 tick 域** | 用户确认（2026-10-24）：与 MIDI clip 完全同构，loop 回卷 / seek 零特判（比例映射自动重锚，无游标同步问题）；代价 = tempo 变化时音频变调（磁带加速效应）+ 拖长 clip 即变速 |
| D2 | 建 clip 入口 | **双击 audio 轨空白处弹 wav 文件对话框**（rfd），clip 出生在点击 tick（bar 吸附），`span_ticks` = wav 自然时长按当前 bpm 换算 | 用户确认：与 MIDI 轨双击建 clip 对称，可发现性好 |
| D3 | 图拓扑 | 每轨一个 `NodeDoc::AudioTrack{node_id, track}` 节点：`NodeKind::Plain` 生成器（无入边、覆盖写私有 `node.output`，同 `Sf2VoiceProcessor` 语义）。加轨自动连 mixer 空闲通道（照 `add_midi_track` 的 delay→mixer 段）；用户可改拉到任意 mixer 通道 / master / 效果器链 | 节点图一等公民架构；timeline 不是引擎节点（P3 既有裁决），音频轨也走"每轨普通节点"通道，`ExecPlan`/`NodeKind` 零改动 |
| D4 | 调度 | **`ProcessContext` 加 `song_ticks`（本块起始 transport tick）+ `transport_dt_ticks`（本块 tick 步进）**，block runner 在 `sequencer.advance` 之前从 `position_ticks` + effective_bpm 读取。`AudioTrackProcessor` 自持 clip 表，经 **`EngineCommand::SetAudioClips{node_id, clips}`** 更新（同 `SetTrackClips` 的逐帧重发模式） | processor 的唯一入口就是 `process(ctx)`；比例映射是绝对函数（每帧独立计算），ctx 两字段即够，无游标累积漂移 |
| D5 | 文档 | `Project.audio_tracks: Vec<AudioTrack>`（serde default，版本不动）；`AudioTrack{name, clips}`；`AudioClip{start_ticks, span_ticks, path, gain(默认1.0)}`；`MAX_AUDIO_TRACKS = 8` | 照 `control_tracks` 加法先例；**不做 offset 头部裁剪**（比例映射下 start/span 已自洽，trim = 改 start+span）；gain 字段留位（serde 加法，v1 无 UI——轨级音量用 mixer 通道推子） |
| D6 | 缓存与波形 | 复用 `SampleLibrary`（path→`Arc<SampleData>` 会话缓存）；`SampleData` 加载期计算 **peaks**（每 256 帧 min/max 对，`Box<[(f32,f32)]>`），音频轨与采样播放器共享缓存 | 波形渲染不重复解码；peaks 是纯加载期成本，RT 不 touch |
| D7 | RT 契约 | `AudioTrackProcessor::process` 纯循环：逐 clip（活跃于本块 tick 区间者）逐帧线性插值读 wav、乘 gain、累加进 buffer。无 scratch、无分配、无锁；clip 表更新走命令队列块边界消费 | 实时铁律；插值内核平移自 U5 `render_voice` |
| D8 | 测试 | 铁律两组（静音/NaN）+ 数学验证（帧位比例、span 变速、loop 重锚、seek 跳入 clip 中段、多 clip 同轨叠加）+ RT 审计（计数 allocator 零分配）+ model RON 往返（含 partial default） | AGENTS.md 强制 + U5 审计先例 |

## 3. 数据流

```
装载：双击 audio 轨空白 → TimelineAction::AddAudioClip{track,start_ticks}
      → app 层 rfd 选 .wav → SampleLibrary 解码（控制线程）
      → 文档 AudioClip{start_ticks, span_ticks=时长×bpm换算, path}
      → SetAudioClips{node_id, [RtAudioClip{start, span, Arc<SampleData>, gain}]}

播放：BlockRunner 每块（sequencer.advance 之前）读 position_ticks
      → ctx{song_ticks, transport_dt_ticks}
      → AudioTrackProcessor.process：逐活跃 clip，帧位 j =
        (song_ticks + j·dt/frames − start)/span × wav_frames，
        线性插值读 + gain 累加 → 节点私有 buffer → 图求和 → master

视图：timeline audio 行 → lane_row 泛型手势（Placement impl AudioClip
      免费：移动/resize/多选/split/duplicate）+ clip 体绘制回调画
      peaks 波形列（视口裁剪同 curve_band 模式）
```

## 4. 实现拆分

- [x] **A1** model：`AudioTrack`/`AudioClip` + `Project.audio_tracks` +
  `MAX_AUDIO_TRACKS`（=8）+ validate 段（loop 传 span——音频无循环字段）
  + RON 往返测试（含旧文件缺失字段 → 空 Vec）✅ 2026-10-24
- [x] **A2** engine：`SampleData` 加 peaks（256 帧/列 min/max 对，双
  声道折叠）；`RtAudioClip`；`AudioTrackProcessor`（比例插值内核）；
  `ProcessContext` 加 `song_ticks`/`transport_dt_ticks`/`transport_active`
  三字段（block runner 在 automation evaluate 之后构造，tempo 曲线已
 生效）；clip 表（含 `Arc<SampleData>`，非 Copy）走 **structure 队列**
  `GraphUpdate::SetAudioClips`（`EngineCommand` 保持 Copy 纯净）+
  displaced 表经 **trash 队列** 回控制线程 drop（move 非 drop，音频
  线程零释放；满则 park 单槽重试，理论满 `mem::forget` 有界泄漏优于
  RT dealloc）；`Processor::swap_audio_clips` trait 默认方法（None =
  未消费，调用方把传入表进 trash）；铁律测试 + 数学测试 + RT 审计
  ✅ 2026-10-24（`tests/audio_track.rs` 13 项 +
  `tests/audio_track_rt_audit.rs` 2 项零分配）
- [x] **A3** UI 装配：`NodeDoc::AudioTrack{node_id, track}` + install_node
  分支 + `apply_document` 装配 + `add_audio_track`/`remove_last_audio_track`
  （suspend_history 一步 undo，mixer 通道自动绑定/扩容）+ clip 表下发
  （`send_audio_clips_for_track`）+ **`TimelineLane` 枚举**取代
  `is_control: bool`（`ClipSel`/action 字段全线 lane 化）；
  undo/redo 全量重建覆盖；UI 帧尾 `reclaim_audio_clips()` 轮询
  ✅ 2026-10-24
- [x] **A4** timeline 视图：`RowKind::Audio(i)` + 行循环 + `Placement`
  impl（`set_loop` no-op——loop==span 恒成立；`retain_from` = 平移
  start/收缩 span，比例映射让 split 语义自动正确）+ `draw_waveform`
  peaks 列（列 = peaks 子区间极值，所见即所听）+ 双击空白 →
  `AddAudioClip`（app 弹 rfd，span = wav 秒 × 当前 bpm 换算 ticks）+
  工具栏 `+ Audio`/`- Audio`；双击 audio clip = 选中该轨；
  点选/框选/拖动/resize/K 切刀/右键删/批删 全部继承 lane_row 泛型
  ✅ 2026-10-24
- [x] **A5** 端到端验收：fixture wav → clip 数学（比例/变速/loop 重锚/
  seek 跳中段/边界切割/多 clip 叠加/gain）+ 全仓 fmt/clippy/test 绿 +
  冒烟 ✅ 2026-10-24（全仓 558 项绿）

### 实施勘误与补充决策（写代码时发现）

- **`swap_audio_clips` 曾误放固有 impl 块**：trait 走默认 `None` →
  `set_audio_clips` 永不消费表（静默失效）。修复 + trait 路径测试锁定
  （`swap_clips_via_the_processor_trait_consumes_tables`）。
- **M4b modal（G/T/Shift+D）对 audio 不开放**（v1 边界，spec §5 已
  记）：两表推理的 grab/stretch 逻辑不动，`arm_modal` 过滤 Audio 选区
  并提示"audio clips edit by direct drag"。K 切刀/拖动/resize 继承。
- **闭包波形源**：`TimelineParams.waveform_of` 是 `&dyn Fn(&str) ->
  Option<Arc<SampleData>>`（`SampleLibrary::get` 只读查找——帧内不解
  码，解码只在编辑路径）。

## 5. 边界与明确不做

- **不做** 变速不变调（timestretch / sinc 多相）——D1 变速即变调。
- **不做** `offset` 头部裁剪字段（比例映射自洽；trim 需求由改
  start/span 表达）。
- **不做** clip gain 的 UI（字段留位默认 1.0；轨级音量 = mixer 通道
  推子，现成）。
- **不做** 拖放文件进 timeline（drag & drop）、音频录音。
- **不做** audio clip 的钢琴卷帘式内容编辑（无内容语义；split /
  duplicate / move / resize 全部继承）。
- **不做** warp / transient 检测 / beat 切片。- **不做** audio clip 的 G / T / Shift+D modal（两表推理逻辑不动，
  `arm_modal` 过滤 Audio 选区；直接拖动/resize/K 切刀已覆盖——实测
  有需求再三表化）。