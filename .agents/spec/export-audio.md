# Spec: Export Audio（离线渲染导出）

> 状态：已收官（2026-10-24，X1–X4 全部落地）。对应 `plan.md`「U7 Export
> Audio」——U 系列最后一项。
> 前置：全部声源就绪（合成器 / SF2 / 采样播放器 / 插件 / audio
> track）；`BlockRunner` 的块驱动与 U6 的 `render_interleaved` 接口。

## 1. 问题

作品只能"播放时听到"，没有交付产物。需要把整曲（或 loop 区间）
离线渲染成 wav 文件——不经过 cpal 实时流，控制线程按块驱动引擎图
（同一套 sequencer 调度 / automation 求值 / 节点处理），hound 写
文件。

## 2. 核心决策

| # | 决策 | 结论 | 理由 |
| :-- | :-- | :-- | :-- |
| D1 | 执行模式 | **帧间分片**：每帧末尾渲染若干块（~34 块 ≈ 90 ms 音频@48k/128），走带条上方进度 banner + Cancel（复用 U9 插件扫描的分片先例与 UI 位） | 用户确认；UI 不冻结，长曲可取消 |
| D2 | 渲染范围 | **整曲 / song loop 两选项**（导出对话框 radio）：整曲 = `[0, end_ticks)`；loop = `[loop_start, loop_end)`。两者都追加 1 s 尾音 | 用户确认；导出 loop 段做素材是常见需求 |
| D3 | 离线引擎 | `AudioEngine::start_offline(sample_rate)`：与 `start()` 同一套装配（命令/结构/回收/trash 队列 + compiler + BlockRunner + feedback），**无 cpal 流**（`runner: Option<BlockRunner>` 字段：实时模式 move 进闭包后为 `None`，离线模式留在字段上）。离线**不推 `SetLoopRange`**（永无回卷——song loop 只是渲染区间剪裁；clip 自身的 loop 平铺照常，那是内容） | 单一驱动路径复用全部调度语义；回卷与导出互斥 |
| D4 | 离线装配 | **独立 `spawn_offline_node`**（声音路径专用）：Builtin/SF2/Sample（复用 `obtain_*` 与 `build_audio_clip_table`）/ CLAP/VST3（spawn_rt + `load_state` + 解构）/ Mixer / Feedback。**不碰 UI 会话缓存与 `voice_guards`**——guard 收集到 job 本地的 `Vec`，渲染结束统一 drop；影子 `node_map` 也活在 job 里 | 与 `install_node` 语义分叉（那边一半代码是 UI 缓存记账）；离线 guard 若混入主列表会被 engine_id 撞号误回收 |
| D5 | 终点与进度 | 渲染终点 = **max(所有 clip 终点)**（MIDI + control + audio——sequencer 的 `arrangement_end` 不含 audio clips，必须在文档侧自己算）+ 1 s 尾音（delay/release 收尾）。进度按 ticks：`musical_ticks / end_ticks`；保险块数上限防异常死循环 | audio track 终点必须计入；尾音后参数冻结在最后值（曲线区间外保持语义） |
| D6 | 快照语义 | 渲染的是**装配时刻的副本**：clip 表 / 参数 / 插件 state 在 start 时拷入离线引擎；导出期间的文档编辑不影响本次渲染（也不锁编辑——想纳入改动就重新导出） | 离线副本架构的天然属性；锁编辑代价高且无必要 |
| D7 | 输出格式 | 16-bit PCM wav（hound `WavWriter`），立体声，采样率 = 引擎实际 rate；rfd SaveFile 默认名 `{project_stem}.wav`；导出是文件操作**不进 undo** | 16-bit 播放器兼容性最好；与 SMF 导出同先例 |
| D8 | 落点 | engine：`engine.rs` 的 offline 构造 + `render_offline_block`；UI：新模块 `export_audio.rs`（`ExportJob` 状态机 + banner + 纯函数），File 菜单 `Export Audio…` | lib.rs 已 8k 行不再膨胀；状态机独立可测 |

## 3. 数据流

```
启动：File ▸ Export Audio… → 范围弹窗（Full song / Song loop + Start）
      → rfd SaveFile 选路径
      → AudioEngine::start_offline(engine_rate)
      → 逐文档节点 spawn_offline_node（影子 node_map + guards）
      → 连线（Audio/Midi/Automation 含 tempo 哨兵）/ register_track_chain
      → clip 表三套下发（SetTrackClips / SetControlClips / SetAudioClips）
      → seek(start) + Play

推进：每帧尾 pump_export：渲染 ~34 块，render_interleaved →
      WavWriter 写交错样本；进度 = musical_ticks / end_ticks

结束：musical_ticks ≥ end → 尾音 1 s（375 块@48k）→ writer.finalize
      → drop job（guard 统一释放，状态栏报路径）；
      Cancel → 直接 drop（半成品文件保留，用户自理）
```

## 4. 实现拆分

- [x] **X1** engine：`AudioEngine::start_offline(sample_rate)`（`_stream:
  Option<Stream>`，`runner: Option<BlockRunner>`——实时 move 进闭包后
  `None`，离线留字段）+ `render_offline_block(&mut [f32])` + 离线驱动
  测试（出声 / 进度 ticks / 自停与 release 静音 / live 引擎拒绝）
  ✅ 2026-10-24（`tests/offline_engine.rs` 3 项）
- [x] **X2** UI `export_audio.rs`：`export_end_ticks`（全 clip 终点
  max 含 audio，纯函数 4 测试）+ `ExportJob`（engine/guards/writer/
  pump/progress/finalize）✅ 2026-10-24
- [x] **X3** UI 入口：`spawn_offline_node`（声音路径装配，无缓存
  记账）+ `start_export`（apply_document 镜像：节点/连线/三套 clip
  表/`SetArrangementFloor`/seek+Play）+ `pump_export`（帧尾 34 块）+
  `export_banner`（进度 + Cancel，扫描 banner 同槽）+ File 菜单
  `Export Audio…` + 范围选择 Modal（Full song / Song loop，无有效
  loop 时禁用）✅ 2026-10-24
- [x] **X4** 验收：`offline_engine` 3 项 + `export_end_ticks` 4 项 +
  全仓 fmt/clippy/test 绿（566 项）+ 冒烟 ✅ 2026-10-24

### 实施勘误与补充决策（写代码时发现）

- **`SequencerCommand::SetArrangementFloor`（新）**：D16 自停只看
  MIDI/control 内容（`refresh_arrangement_end`），audio clip 终点更晚
  会被 `transport_active` 静音切断。floor 让自停不早于导出终点
  （live 引擎永不推，零影响）。
- **Play 命令会群发 `note_off_all`**：同块内先发的手动 note 被立即
  释放（新 envelope 从 level 0 release = 瞬间静音）。导出音符全部
  来自 clip 调度（Play 之后 advance 路径），天然无此坑——
  `offline_engine` 测试的时序注释锁定该认知。
- **VST3 `guard.load_state` 走 `&self`**（StateGate 内部处理），CLAP
  的走 `&mut`——离线 spawn 的 `Ok(instance)` 是否 `mut` 两家不同。
- **ADSR 默认 release 0.2 s**：测试的"Stop 后静音"断言要给足 ~96
  块（不是采样播放器的 10 ms）。

## 5. 边界与明确不做

- **不做** 导出格式选项（24-bit / float / mp3 / stems 分轨）——
  v1 固定 16-bit 立体声 wav；stems 是另一个方向。
- **不做** normalization / dither / 前导静音裁剪。
- **不做** 后台渲染线程（D1 分片已保 UI 活性；线程化的 guard 生命
  周期管理收益不成比例）。
- **不做** 渲染期间锁编辑（D6 快照语义）。
- **不做** 实时监听导出（边渲染边播）。
- **已知取舍**：导出期间的主引擎照常出声（两引擎互不干扰——各自
  持队列与图；共享的只有只读 `Arc<SampleData>` 与文档快照）。