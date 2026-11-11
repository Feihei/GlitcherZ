# 计划与状态 — GlitcherZ

> 管计划与进度。已完成系列的实施记录在 `implementation.md`（末尾「系列实施补记」章收纳 2026-11-11 自本文件归位的内容），设计在 `.agents/spec/`；本文件只留路线图、未完成项与活跃系列。

## 路线图（按顺序实施，勿跳步）

1. ✅ `cpal` 输出正弦波（验证音频管线）
2. ✅ `egui` 集成 + 参数滑块实时控制正弦波频率（验证跨线程通信）
3. ✅ 音量包络 + 简单滤波器（第一个完整合成器）
4. ✅ 步进音序器（16 步，触发合成器）
5. ✅ 多轨 + 混音器 + 简单效果器链
6. ✅ 钢琴卷帘 MIDI 编辑
7. ✅ 项目保存/加载（RON）
8. ✅ 自动化曲线
9. ✅ CLAP 插件宿主（先于 VST3，API 更简洁）
10. ✅ VST3 插件宿主（乐器/效果器已集成进 model/UI）

## 当前状态（2026-11-11）

路线图 1–10 全部落地；基础功能、节点图、MIDI 编辑、插件宿主、可用性（U 系列）、录音（R 系列）、内建效果器（E 系列）、声源扩展（Y / Z / B 系列）与交互打磨（I 系列）均已交付。已完成系列的设计与实现位置见下方「历史系列索引」——本文件不再逐项记录它们的实施细节。

- **进行中**：Met 系列（吸附统一 + 拍号变化，M1 / M2a / M2b 已交付，M3–M5b 待做）、Aud 系列（Edit 试听，Aud1 / Aud2 已交付，Aud3 待做）。
- **下一步候选**：命令族扩展（quantize / legato / glue / transpose）、C5 一曲线多目标、CLI / MCP 的命令词汇表。
- **门禁**：`cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace`，在本机沙箱外跑（沙箱内 cargo 会毒化 `target/debug/incremental`）。

## 未完成项

- [ ] **未来命令族扩展**：quantize / legato / glue / transpose = 同族零侵入（M3 已有 ops 与命令骨架，补齐菜单与快捷键即可）。
- [ ] **C5 一曲线多目标（候选）**：结论是**不需要**强制 map 节点——轴本来就是每条边自己的，放开多路后每路按目标各自推导即可。真正的阻塞在别处：`auto_routes` 每轨只 1 槽、`DedupKey::Auto` 是 per-track 的 CC/bend last-value 记忆（两条分支会互相污染）、model 的 `auto_endpoint` / `auto_target` / `remove_auto_cable` 都是单值语义。
- [ ] **空闲重绘**（review 2026-11-01 遗留）：`request_repaint_after(50 ms)` 目前无条件调用，建议只在播放中 / 电平可见时开——纯 UI 性能项，不碰引擎。
- [ ] **录音 PDC 自动对齐**（review 2026-11-01 遗留）：PDC 已进图（S2-3），但 R5 仍靠手动 `Latency` offset；自动值需要汇总「输入设备 → 轨道链」的往返延迟。
- **S5 之后的硬阻塞**（Y 系列收官时记下的三条）：
  - **命令词汇表**是唯一的硬阻塞——16 条 `midi.*` 全都作用于既有 clip，改音色参数也还没有稳定字符串名（`NodeParam` 只有 UI 显示名）。MCP 能「改音符」但改不了「用什么音色」、也开不了新 clip。这一期 = `NodeParam` 稳定名 + `clip.add` + `graph.set_param`，按 `spec/` 的规矩先写独立 spec。
  - **用户表导入**（Wavetable S3 留的口子）：`.wav` / `.wt` 文件、命名、缓存淘汰、文件对话 UI；风险全在文件与 UI，不在 DSP。
  - **安全边界**：现在只管客户端给的路径，工程内部引用的媒体 / SF2 路径照单全收（等同于在窗口里打开一个不信任的工程）。要做就得先有「工程信任模型」。

## 吸附统一与拍号变化（Met 系列，2026-11-10 立项）

用户一句话需求：**编辑时按住 Ctrl 临时禁用吸附**；**timeline 可加 meter 标记中途换拍号**；**拍号范围扩到每小节 1–12 拍、每拍 1/2–1/16**。

Spec：`spec/snap.md`（S1–S8）、`spec/meter-map.md`（D1–D10，扩展 `tempo-meter.md` D5）。

### 现状评估（已核对代码，不是猜测）

| 项 | 现状（位置） | 差距 | 体量 | 风险 |
| :--- | :--- | :--- | :--- | :--- |
| **Met-1** 临时禁用吸附 | 吸附是**五套并行实现**：卷帘 `snap_ticks`（`piano_roll.rs:3258`，用 `RollState.grid`）、时间轴 `snap_bar`（`timeline.rs:4036`，整小节）、标尺同名闭包（`:1854`）、模态内联 round（`:1072/1131/1213`）、曲线固定 `SNAP_TICKS`（`curve.rs:18`，1/32）。全部不接收修饰键 | 无关闭入口；粒度也不统一 | 中（新 `snap.rs` + ~30 个调用点参数化，~350 行） | `Ctrl` 已被占用：卷帘 `Ctrl+拖 = 复制`（`piano_roll.rs:2653`）直接冲突；时间轴 ruler `Ctrl+拖 = loop`（区域隔离）、lane `Ctrl+拖 = 追加选择`（可叠加） |
| **Met-2** meter 标记 | 拍号是 `ProjectMetadata.time_signature` **单值**（`model/lib.rs:6375`）；`ticks_per_bar()` 一步乘法（`lib.rs:1538`）；引擎 `Sequencer.bar_ticks` 单值 + `SetTimeSignature`（`sequencer.rs:631`）；SMF 只写一条 `FF 58` | 无变化点数据结构；小节换算不可分段 | **大**（model 求值层 + timeline/ruler/状态栏/seek/录音/导出分段 + 引擎段表 + SMF 多段，~900 行） | 像素映射：现在 `px_per_tick = px_per_bar / bar_ticks`（`timeline.rs:1498-1502`），按段改映射会波及整条 timeline |
| **Met-3** 拍号范围 | 模型已支持 `numerator 1..=16`、`denominator {1,2,4,8,16,32}`（`lib.rs:1520-1532`）；UI 只有 6 个预设（`METER_PRESETS`，`ui/lib.rs:227`、`transport.rs:429-445`） | UI 输入太窄，13/8 进不来 | 小（控件替换，~80 行） | 不能收窄模型范围，否则 SMF 导入的 16/32 分母会被 `validate` 拒 |

### 决策（2026-11-10 已拍板）

| ID | 问题 | 结论 |
| :--- | :--- | :--- |
| Met-D1 | 禁用吸附用什么键 | **`Ctrl` / `Cmd`**。`Alt` 排除：Windows 松开 Alt 触发 `SC_KEYMENU` 吞鼠标事件、按住时 egui `PointerGone`（D29 已实证）；且 `Alt+拖` 在 timeline 已绑定拆组（`timeline.rs:3364`） |
| Met-D2 | 卷帘 `Ctrl+拖动复制` 怎么办 | **删除**。复制只保留 `Shift+D` 模态与工具栏 `Duplicate ⇧D` 按钮（roll + timeline 各一个） |
| Met-D3 | meter marker 能否落在半小节 | **不能**，必须吸附到当前段小节线；文件里的非法位置在 `MeterMap::new` 向下吸附（宽容读取，不拒绝） |
| Met-D4 | 拍号变段后的像素映射 | `px_per_tick` **全局线性**，锚定第 0 段小节宽度（`px_per_bar / base_bar_ticks`）；只有网格/标号/吸附按段变化 |
| Met-D5 | 复合拍分组（12/8 = 3+3+3+3） | **不做**，12/8 按 12 个八分拍计数、重音只在第 1 拍 |
| Met-D6 | 拍号范围 | 模型不动（1..=16 / {1,2,4,8,16,32}）；UI 给分子 1–12、分母 {1/2, 1/4, 1/8, 1/16}；超范围值只读显示并提示 |
| Met-D7 | 文件版本 | `PROJECT_VERSION` 10 → 11；`meter_markers` 是 `serde(default)` 加法字段，**无迁移函数**，空表时行为逐字节不变 |

### 里程碑与依赖

| 里程碑 | 内容 | 依赖 | 规模 |
| :--- | :--- | :--- | :--- |
| **M0** 文档 | `spec/snap.md` + `spec/meter-map.md` + `tempo-meter.md` D5 改口 + `timeline.md` D32 + 本小节 | — | 小（已完成） |
| **M1** 拍号输入（2026-11-10 交付） | 走带控件改为「分子 1–12 + 分母四选一」；超范围只读；`METER_PRESETS` 退休；`meter_ui::meter_editor` 复用给 M5a；拖动经 `set_meter(before)` 合并为一步 undo | M0 | 小 |
| **M2a** 吸附收口 | 新 `snap.rs`；五套实现改吃 `Snap`；**恒 on，零行为变化**；on/off 纯函数单测 | M0 | 中 |
| **M2b** Ctrl 接线（2026-11-10 交付） | 视图入口派生 `Snap::OFF`；状态栏 `Snap off`；删除卷帘 `Ctrl+拖动复制`；两个工具栏加 `Duplicate ⇧D`；快捷键文档同步；G / T / Tile 模态保持 bar 步进（spec S7） | M2a | 中 |
| **M3** MeterMap | `MeterMarker` + 上限 + `validate` + `model/src/meter.rs` 全部查询 + 分段数学单测；版本 11 | M0 | 中 |
| **M4** timeline 分段 | `TimelineParams` 借 `&MeterMap`；ruler / lane 网格 / 吸附 / knife / trim / 状态栏 / seek / 录音 / 导出；ruler 画 meter 胶囊（只读） | M3、M2b | 大 |
| **M5a** marker 编辑 | ruler 右键新建、拖动、双击改值、右键删除；Inspector 拍号区；`TimelineAction` 四个变体；复用 M1 控件 | M4、M1 | 中 |
| **M5b** 引擎 + SMF | `MeterSeg` / `SetMeterMarkers`；`Sequencer` 段表与 metronome 跨段重锚；SMF 多段 `FF 58` 导入导出 | M3 | 大 |

实施顺序：**M1 → M2a → M2b → M3 → M4 → M5a → M5b**。M1 是 M5a 的前置控件；M2b 先落地，M4 的吸附逻辑只需改一次。

### 验收门禁

- 每个里程碑单独 commit（`feat(ui):` / `feat(model):` / `feat(engine):`），文档（M0）先合。
- `cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace` 全绿；**在本机沙箱外跑**（沙箱内 cargo 会毒化 `target/debug/incremental`）。
- M2 零回归证据：不按 Ctrl 时所有落点逐 tick 一致；M3 空表时 `MeterMap` 单段，旧工程导出/渲染逐字节一致。
- M4/M5 回归：`working_files/regression-project.glz` 在无 marker 下 `--render` 与 pre-Met 二进制逐字节一致；有 marker 的用例另建 fixture。

### 明确不做（本系列）

- 复合拍分组、拍号自动化（`Timeline In` 不加 `Meter` 端口）、半小节换拍号。
- 持久化「吸附开关」/ `GridValue::Off`、时间轴可配置吸附粒度。
- 把复制迁到 `Alt+拖动`（Met-D1 已排除 Alt）。
- 按段改变 `px_per_tick`、按新拍号重排/量化既有内容。

## Edit 视图试听与独立时间轴（Aud 系列，2026-11-10 立项）

用户一句话需求：**Edit 界面加独立于 timeline 的 clip 时间线与播放头，用于试听当前 clip 范围；Edit 播放中切 tab 自动停止。**

Spec：`spec/edit-audition.md`（D1–D10）。

### 现状评估（已核对代码）

| 项 | 现状（位置） | 差距 | 体量 | 风险 |
| :--- | :--- | :--- | :--- | :--- |
| **Aud-1** Edit 时间轴 | 卷帘无 ruler；小节线只画 `loop_ticks`（`piano_roll.rs:2083-2110`）；`ViewWindow` 用 `start + loop`（`:1342-1375`） | 无 ruler、看不到平铺 span | 中（ruler 绘制 + `clip_span` 参数 + seek intent，~200 行） | 与主区共用时间轴，滚动范围扩到 span 后平铺区在主区是空白，需靠 ruler 表达 |
| **Aud-2** 播放头 | 只在 `playing` 时画；按 `loop` 取模且**未减 `origin_tick`**（`piano_roll.rs:2221-2231 / 2263-2273`） | 停止时不显示；起点非整周期会偏 | 小（相位 + 静态显示，~40 行） | velocity / overlay 两条泳道各有一份，必须同时修 |
| **Aud-3** clip 范围试听 | 引擎无 audition 概念；只有 song loop（`loop_range` 序列化）；Edit 按 Play 播整曲 | 无区间试听 | 中（`SetAuditionRange` + `advance` 三分支 + 4 测试，~150 行） | `advance` 的窗口切分已有 loop/arrangement 两支，加第三支必须保证 pending offs 与循环回卷零回归 |
| **Aud-4** 切 tab 停止 | tab 点击（`transport.rs:565-575`）与快捷键（`keyboard.rs:529-563`）各自直接写 `self.view`，无停止副作用 | 无统一入口 | 小（`set_view` 收口 + 策略纯函数，~60 行） | 只有 Edit 发起的试听才该停；Timeline 播放中切 Edit 不能误停 |

### 决策（2026-11-10 已拍板）

| ID | 问题 | 结论 |
| :--- | :--- | :--- |
| Aud-D1 | 试听范围 | **整个 clip = `[start, start+span)`**；ruler 覆盖 span，主区仍只显示一个 loop 周期 |
| Aud-D2 | 播完怎么办 | **单次播放**：到 span 末尾自动停并回到 clip 起点；引擎精确处理，不靠 UI 轮询 |
| Aud-D3 | 怎么触发 | **复用全局 Play / Space**；不新增 Edit 专属按钮 |
| Aud-D4 | 用 song loop 实现？ | **不用**。新增独立 `SetAuditionRange`，不写文档、不弄脏工程、不与走带 loop 灯混淆 |
| Aud-D5 | 切 tab 用 Stop 还是 Pause | **Pause**（保留位置）+ 清范围；不清文档、不动 song loop |
| Aud-D6 | 视图切换入口 | 收口为 `set_view()`，tab 点击与快捷键共用 |
| Aud-D7 | 文件格式 | **不动**；audition 是会话态，不进 undo、不进版本 |

### 里程碑与依赖

| 里程碑 | 内容 | 依赖 | 规模 |
| :--- | :--- | :--- | :--- |
| **Aud1** 引擎（2026-11-10 交付） | `SetAuditionRange` + `Sequencer` 字段 + `advance` 三分支 + 4 条单测 | 无 | 中 |
| **Aud2** 试听路由（2026-11-10 交付） | `audition` 会话态、`toggle_transport`（Play 按钮 + Space 共用）、自然结束清理、`set_view` + 切 tab 停止、策略纯函数单测；`docs/manual.md` / `docs/shortcuts.md` 同步 | Aud1 | 中 |
| **Aud3** ruler + 播放头 | `clip_span` 参数、`ViewWindow` 扩到 span、ruler 绘制/seek、播放头相位与静态显示、roll fixture 测试、文档同步 | Aud2 | 中 |

顺序：**Aud1 → Aud2 → Aud3**。Aud3 会改 `piano_roll.rs`，需与 Met 系列的 M2b（同一文件）错开，避免两个系列在同一区域反复改。

### 验收门禁

- 每里程碑单独 commit；`cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace` 全绿（`sf2_rt_audit` 的并行 flaky 单跑确认）。
- 零回归：audition 未激活时 `advance` 行为与今天逐字节一致；Timeline 下 Play 不走新分支。
- 切 tab 停止必须是**纯策略**可测（旧视图、新视图、audition 是否激活 → 是否 Pause），不依赖 egui 帧。

### 明确不做（本系列）

- 试听循环、Edit 独立播放按钮、audio clip 的 Edit 波形/试听、平铺区在主卷帘展开、audition 序列化、试听淡入淡出。

## 历史系列索引

已完成系列的细节不再占用本文件；下表保证 spec 里「对应 `plan.md` 某系列」的旧引用仍可解析。

| 系列 | 范围 | 状态 | 设计 | 实现 |
| :--- | :--- | :--- | :--- | :--- |
| 路线图 | 1–10（正弦 → 多轨 → 卷帘 → 自动化 → CLAP/VST3） | 已交付 | — | `implementation.md` 分节 |
| P 系列 | P1–P6（节点图、Mixer、反馈环路） | 已交付 | `spec/node-graph.md` | `implementation.md` 节点图 / Mixer / 反馈环路 |
| M 系列 | M1–M4（数据模型、命令层、编辑交互） | 已交付 | `spec/editing-model.md` | `implementation.md` M1 / M4a / M4b |
| U 系列 | U1–U14（走带、文件、拍号、控制轨、采样器、audio track、导出、Inspector、插件浏览器、内联管理、裁剪、曲线、SoundFont、mixer 条） | 已交付 | `spec/{audio-track,audio-trim,sample-player,export-audio,plugin-browser,control-tracks,sf2-loader}.md` | `implementation.md` 各同名节 + 「系列实施补记」 |
| E 系列 | E1–E5（效果器框架 + EQ/压缩/混响/调制/失真类） | 已交付 | `spec/effects.md` §14–§18 | `spec/effects.md` 实施勘误 + `implementation.md` |
| SF2 / 音色库 | S1–S6 + U13（节点自足化） | 已交付 | `spec/sf2-loader.md` | `implementation.md` SF2 节 + 「系列实施补记」 |
| 插件宿主 | G1–G7（GUI 嵌入、参数回接、VST3 IPlugView） | 已交付 | `spec/plugin-gui.md` / `plugin-state.md` / `plugin-browser.md` | `implementation.md` 各同名节 + 「系列实施补记」G4/G5 |
| R 系列 | P0–P5（多 clip 卷帘 + 音高曲线） | 已交付 | `spec/roll-multiclip.md` | `implementation.md` 多 clip 卷帘等节 |
| W 系列 | W1–W5（鼓机、平铺、命名、编组、随机化、strum） | 已交付 | `spec/drum-kit.md` / `timeline.md` / `midi-generators.md` | `implementation.md`「系列实施补记」 |
| LFO / MIDI nodes | 控制源与块内事件总线 | 已交付 | `spec/lfo.md` / `midi-nodes.md` | `implementation.md`「系列实施补记」 |
| S 系列 | S1–S3（稳定性、最短的板、交付打磨） | 已交付 | `spec/{autosave,media,export-audio}.md` | `implementation.md` 各同名节 + 「系列实施补记」 |
| 录音 | R1–R8（节拍器、capture、midir、音频输入、烘培） | 已交付 | `spec/recording.md` | `implementation.md`「录音输入 UI 归位」+「系列实施补记」 |
| X 系列 | X1–X3（symphonia 解码） | 已交付 | `spec/audio-decoding.md` | `implementation.md`「系列实施补记」 |
| Y 系列 | S0–S5（headless 渲染、三个乐器、CLI、MCP） | 已交付 | `spec/{cli-commands,mcp-server,pluck-instrument,organ-instrument,wavetable-instrument}.md` | `implementation.md` S0–S5 各节 |
| Z / B 系列 | Z0–Z2、B1–B2（双口、侧链、vocoder、模态声源、clip mute） | 已交付 | `spec/{dual-audio-input,sidechain-compressor,vocoder,modal-instrument}.md` / `timeline.md` D31 | `implementation.md` 各同名节 |
| I 系列 | I1–I6（交互打磨） | 已交付 | 决策表见 `implementation.md` | `implementation.md` I 系列 |

## 明确不做（全局）

- **Score Editor 五线谱**：`soundcraft` 自述是 readable sketch，价值不如把 piano roll 做扎实。
- **command journal / macro recording**：暂无 macro 需求。
- **轨道文件夹**：W3b 已选择 clip 级跨轨编组，不做轨道层级。
- **全局 Mixer 独立视图**：mixer 节点控制台 + 走带 master 组已够用。
- **audio clip 循环平铺**：audio 走裁剪语义（U11），不做重复平铺。
- **演奏期随机**：W4 只做一次性写文档，实时随机不做。
- **Echo / Gain 节点**：Delay 即回声，通道条已是增益级。
- **每效果 sidechain 输入**：已由 Z0 的双音频口 + Z1 侧链压缩替代，不再单列。
- **P5 剩余三项**：ghost velocity、泳道高度拖动、context 上限调整（`roll-multiclip.md` §14 裁决）。