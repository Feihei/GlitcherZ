# Spec: Export Audio（离线渲染导出）

> 状态：已收官（2026-10-24，X1–X4 全部落地）；**2026-11-03 S3c 扩展**
> （X5）：位深 16/24/32f、采样率可选、每轨 stems。对应 `plan.md`「U7 Export
> Audio」与「S3 交付与打磨」。
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
| D7 | 输出格式 | 位深 **16 / 24 / 32-bit float 三选**（默认 24-bit PCM），立体声；`Bits::wav_spec` 一个地方决定 hound spec；导出仍然是文件操作**不进 undo** | S3c 用户裁定：16-bit 交付不出去，24-bit 是常规交付，32f 给后续处理留空间。三档都由 hound 原生支持，没有新依赖 |
| D8 | 落点 | engine：`engine.rs` 的 offline 构造 + `render_offline_block`；UI：新模块 `export_audio.rs`（`ExportJob` 状态机 + banner + 纯函数），File 菜单 `Export Audio…` | lib.rs 已 8k 行不再膨胀；状态机独立可测 |
| D9 | 采样率 | **可选**（设备 rate + 44.1/48/88.2/96 去重列表，默认设备 rate）；离线引擎按所选 rate 构建，`spawn_offline_node` 显式收到该 rate | S3c：此前 `spawn_offline_node` 读**活引擎**的 rate，等于把导出钉死在设备上。"96 kHz 导出"不需要 96 kHz 设备 |
| D10 | stems | **同一离线引擎、多 pass**：pass 0 = 整混音，之后每轨一 pass；pass 切换只重推 clip 表（目标轨保留内容，其它轨推空表），图 / 插件 / mixer / control 轨不动 | 重装图 = 每轨一次 CLAP/VST3 实例化 + state 回灌（Dexed 量级可感）；改址只是几次结构队列推送。automation / tempo 属于"这个 stem 听起来的样子"，所以 control 轨保留 |
| D11 | pass 之间 | 渲染 **2 s 静音并丢弃**（`FLUSH_SECONDS`） | 上一 pass 的 reverb/delay 尾巴不能漏进下一个文件开头；插件内部状态无法 reset，只能靠时间衰减 |
| D12 | 取消 | 删掉**未完成**的那个文件（已完成 pass 保留），状态栏报保留数量 | 半成品 wav 看起来像成品，比没有更糟 |
| D13 | 命名 | 混音 = 用户选的名字；stem = 同目录 `<stem>_<轨名>.wav`，轨名过 `sanitize_file_part`（Windows 保留字符 / 控制码 → `_`，尾部点与空格去掉），重名追加 ` 2` | stem 必须落在用户选的目录里；轨名是用户输入，不能直接当文件名 |

## 3. 数据流

```
启动：File ▸ Export Audio… → 范围弹窗（Full song / Song loop + Start）
      → rfd SaveFile 选路径
      → AudioEngine::start_offline(engine_rate)
      → 逐文档节点 spawn_offline_node（影子 node_map + guards）
      → 连线（Audio/Midi/Automation 含 tempo 哨兵）/ register_track_chain
      → clip 表三套下发（SetMidiClips / SetControlClips，经结构队列；SetAudioClips）
      → seek(start) + Play

推进：每帧尾 pump_export：渲染 ~34 块，render_interleaved →
      WavWriter 写交错样本；进度 = (pass + musical_ticks/end_ticks) / pass 数

结束：musical_ticks ≥ end → 尾音 1 s（375 块@48k）→ writer.finalize
      → drop job（guard 统一释放，状态栏报路径）；
      Cancel → 删除当前未完成文件后 drop（已完成 pass 保留）

stems（S3c）：pass 列表 = [mix, 每轨一个]；每个新 pass 之前
      丢弃 2 s 静音 → 开新 writer → setup 回调重推 clip 表
      （目标轨内容 / 其它轨空表）→ seek(start) + Play
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
- [x] **X5 S3c 规格化** ✅ 2026-11-03：`Bits`（16/24/32f）+ `WavFormat`
  + `sample_rate_choices`；`ExportJob` 改多 pass（`ExportPass` /
  `pump(blocks, setup)` / `begin_pass` / `close_pass` / 2 s flush /
  `outputs`）；`spawn_offline_node` 收 `export_rate`；`start_export`
  收 `format + passes`；`export_passes` / `set_export_pass_tables`
  （stem 改址）；对话框加位深 radio / 采样率下拉 / stems 勾选；banner
  显示 `n/total — 轨名 — 文件`；取消删半成品。测试 +4（WavSpec 三档 /
  采样率列表去重 / 轨名消毒与 stem 路径 / 3-pass 真实渲染写出 3 个
  24-bit@22050 文件且 setup 回调只对 stem 触发）

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

- **不做** normalization / dither / 前导静音裁剪。
- **不做** mp3 / flac 等有损或压缩格式（hound 只写 PCM 系）。
- **不做** 分组 stems（bus / 编组导出）：stem 的粒度就是文档里的轨道。
- **不做** 后台渲染线程（D1 分片已保 UI 活性；线程化的 guard 生命
  周期管理收益不成比例）。
- **不做** 渲染期间锁编辑（D6 快照语义）。
- **不做** 实时监听导出（边渲染边播）。
- **已知取舍**：导出期间的主引擎照常出声（两引擎互不干扰——各自
  持队列与图；共享的只有只读 `Arc<SampleData>` 与文档快照）。
- **已知取舍**：stems 每个 pass 都要从窗口起点重跑，总耗时 ≈
  （轨数 + 1）× 整曲时长；进度条按 pass 加权，用户能看出在推进。
- **已知取舍**：flush 2 s 是在"插件状态不可 reset"前提下的经验值；
  一个 10 s 尾的巨型 reverb 仍可能漏一点进下一个文件。