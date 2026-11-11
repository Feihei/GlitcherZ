# 实现细节 — GlitcherZ

> 记录已落地模块的实现要点、协议细节与版本 gotchas。新增模块落地后在此追加条目。

## 目录

正文按主题分 9 节，节内大致按时间排列；条目用 `###`，更深子节用 `####`。特性编号规则见 `AGENTS.md`「特性编号（F-ID）」，全量映射见下方「特性 ID 注册表」。

- **引擎与实时**：realtime 图与 sequencer、自动化与控制线、结构更新与回收、增量引擎同步、双口与调制效果
- **模型与文件格式**：Project / 迁移、自动保存、媒体路径与重定位
- **编辑与视图**：卷帘 / timeline / 曲线、音高通路、UI 模块与走带、clip 操作
- **节点图与面板**：patch 交互与内联参数、Inspector 面板
- **插件宿主**：CLAP / VST3、GUI 嵌入、状态持久化、插件浏览器
- **效果器与声源**：内建效果器、采样与鼓机、模态 / 拨弦 / 风琴 / 波表
- **录音与 headless I/O**：录音、导出、Preferences、CLI、MCP
- **框架升级与 gotchas**：依赖版本契约、egui 升级、跨平台构建
- **系列实施补记与交互打磨**：2026-11-11 自 plan.md 归位的系列记录、I 系列
- **特性 ID 注册表**：`F-001`–`F-039`（新特性先登记再实现）

## 特性 ID 注册表

编号是身份，域是标签（可多值）；一个 F-ID 对应一份 spec。**新增特性先取当前最大号 +1 并在此登记，再写 spec 与实现。** 历史字母系列保留为别名，不重命名、不回溯。首批 37 条按 spec 文件名排序一次性登记，**不代表时间先后**；F-038 起按落地时间递增。

| ID | 特性 | 域 | spec | 原系列 |
| :--- | :--- | :--- | :--- | :--- |
| F-001 | 音频解码（symphonia） | IO / VOICE | `audio-decoding.md` | X |
| F-002 | audio track | EDIT / VOICE | `audio-track.md` | U6 |
| F-003 | audio clip 裁剪 | EDIT / PROJ | `audio-trim.md` | U11 |
| F-004 | 自动保存与崩溃恢复 | PROJ | `autosave.md` | S3a |
| F-005 | CLI 批量命令 | IO | `cli-commands.md` | S4 |
| F-006 | 控制轨 CC 输出 | EDIT / PATCH | `control-tracks.md` | U4 |
| F-007 | 曲线归一化 + Curve Map | PATCH / EDIT | `curve-map.md` | C1–C3 |
| F-008 | 鼓机 | VOICE | `drum-kit.md` | W1 |
| F-009 | 双音频输入口 | ENGINE / PATCH | `dual-audio-input.md` | Z0 |
| F-010 | Edit 试听与独立时间轴 | EDIT / ENGINE | `edit-audition.md` | Aud |
| F-011 | 编辑交互模型与命令层 | EDIT | `editing-model.md` | M1–M4 |
| F-012 | 内建效果器框架 | MIX / ENGINE | `effects.md` | E1–E5 |
| F-013 | 导出音频与规格化 | IO | `export-audio.md` | U7 / S3c |
| F-014 | 内置合成器 | VOICE | `instrument.md` | — |
| F-015 | LFO 控制源 | PATCH / ENGINE | `lfo.md` | — |
| F-016 | MCP server | IO | `mcp-server.md` | S5 |
| F-017 | 媒体路径 / 收集 / 重定位 | PROJ | `media.md` | S3b |
| F-018 | 拍号映射（meter markers） | EDIT / ENGINE | `meter-map.md` | Met |
| F-019 | MIDI 随机化与 strum | EDIT | `midi-generators.md` | W4 / W5 |
| F-020 | MIDI 事件处理节点 | PATCH / EDIT | `midi-nodes.md` | M |
| F-021 | 模态打击声源 | VOICE | `modal-instrument.md` | B1 |
| F-022 | 节点图 | PATCH / ENGINE | `node-graph.md` | P1–P6 |
| F-023 | 拉杆风琴 | VOICE | `organ-instrument.md` | S2 |
| F-024 | 拨弦声源 | VOICE | `pluck-instrument.md` | S1 |
| F-025 | 插件浏览器与添加菜单 | PLUG / UI | `plugin-browser.md` | U9 |
| F-026 | 插件 GUI 宿主 | PLUG / UI | `plugin-gui.md` | G1–G7 |
| F-027 | 插件状态持久化 | PLUG / PROJ | `plugin-state.md` | 遗留任务 |
| F-028 | 节拍器与录音 | IO / ENGINE | `recording.md` | R1–R8 |
| F-029 | 多 clip 卷帘与音高曲线 | EDIT | `roll-multiclip.md` | P0–P5 |
| F-030 | 采样播放器 | VOICE | `sample-player.md` | U5 |
| F-031 | SF2 音色库与节点自足化 | VOICE / PATCH | `sf2-loader.md` | S1–S6 / U13 |
| F-032 | 侧链压缩 | MIX / ENGINE | `sidechain-compressor.md` | Z1 |
| F-033 | 吸附统一（Snap） | EDIT / UI | `snap.md` | Met |
| F-034 | tempo 与拍号 | EDIT / ENGINE | `tempo-meter.md` | U3 |
| F-035 | 编曲时间轴 | EDIT | `timeline.md` | T / M4b / W2 |
| F-036 | Vocoder | MIX / ENGINE | `vocoder.md` | Z2 |
| F-037 | 波表合成 | VOICE | `wavetable-instrument.md` | S3 |
| F-038 | MIDI 命令族（量化 / 连奏 / 粘合 / 去重叠 / 移调） | EDIT | `midi-commands.md` | W6 |
| F-039 | CLI / MCP 命令词汇表（节点参数稳定名 + clip.add） | PATCH / EDIT / IO | `command-vocabulary.md` | 遗留硬阻塞 |
| F-040 | 剪贴板与复制粘贴（Ctrl+C / Ctrl+V） | EDIT / PATCH / UI | `clipboard.md` | C1–C2 |

> 无独立 spec 的历史条目（transport / 文件管理 U2 / U10 / U14 / I 系列 / B2 / W3a–c 等）保留字母别名，不做回溯编号；新特性一律先写 spec 再领号。

## 引擎与实时

### 时间轴入图（P3，2026-10-08）

- **MIDI 连线表**：`ExecPlan::midi_targets`（每 MIDI 轨扇出 ≤ `MAX_MIDI_FANOUT`=4）——音序器与键盘演奏的音符事件经 `RtGraph::track_note` 扇出到全部连线目标（`note_off_all` 同样遍历连线表）；默认图每轨连本轨声源（复刻旧语义）。编译器侧 `connect_midi/disconnect_midi`（重复连线幂等、空槽/越界拒绝、扇出超限报 `MidiFanOutOverflow`）。
- **自动化连线表（D7）**：`RtAutomation` 退化为**纯锚点曲线**（`enabled + points`，不再内嵌目标参数）；新增 `RtAutoTrack { loop_ticks = 384（一小节，可独立设置）, curve }` 独立实体（`MAX_AUTO_TRACKS`=16），与 MIDI 轨道音源循环解耦。`ExecPlan::auto_routes[16]` 终点 = `(槽位, 参数)`，控制侧以轨道坐标 `(track, param)` 经 `route_slot`（混音三项→通道条、延迟三项→延迟、其余→声源）解析；`RtGraph::auto_set_param` 查表投递，未连线静默。
- **命令面**：`SequencerCommand::SetAutomation` → `SetAutoTrack { index, auto }`；`AudioEngine` 新增 `set_auto_track`（命令队列）/ `set_auto_route` / `clear_auto_route`（编译器→结构队列，队列满快照回滚）。
- **UI 适配**：`send_automation` 同步下发曲线（`SetAutoTrack`）与连线终点（`set_auto_route`）；自动化轨道索引 = `track × MAX_AUTOMATIONS(4) + 文档 Vec 槽位`（Vec 只追加/原位替换，槽位稳定）；循环长度随泳道显示口径（片段长或一小节），P4 起由文档 v2 显式携带。
- **结构队列扩容 8 → 64**：项目加载一次性下发最多 16 条自动化连线 + 每轨声源恢复，原容量会静默丢连线。
- Sequencer 内核零改动（`advance` 回调签名不变）；现有音序/自动化端到端测试全部经连线路径通过。

### 节点图（`realtime::graph`，P1+P2，spec 见 `.agents/spec/node-graph.md`）

- **项目文件 v2（P4）**：`PROJECT_VERSION = 2`，无 v1 读路径（serde 字段级默认值容忍缺失）。新增 `Project.graph: GraphDoc`（过渡形态，spec §7 完整 nodes+cables 随 P5 落地）——`midi_links`（MIDI 跨轨扇出，上限 16；加载期校验越界/自连/超容报 `ModelError::InvalidGraph`）；`Automation::loop_ticks` 显式携带（默认 384），泳道显示口径同步回写。引擎侧配套 `AudioEngine::connect_midi_track/disconnect_midi_track`（快照回滚），UI `push_all_params` 加载时重放连线。
- **`RtGraph`**：定容 64 槽位，每节点一个专属立体声 128 帧输出缓冲；处理按预编译拓扑序遍历——清零 → 多入边自动求和（≤8）→ 原地 `Processor::process`；主输出槽缓冲即 cpal 写出源。
- **`ExecPlan`**（全 `Copy`）：Kahn 拓扑排序 + 入边表 + 每轨 voice/delay/channel 路由表（P3 由时间轴连线表替代）；编译期拒绝环（D10）与入边超限。
- **结构更新协议（D5）**：控制侧 `GraphCompiler`（全 `Copy`，队列满可快照回滚）编译计划并产出 `GraphUpdate::SetTopology { plan, deployments, removed }`；音频线程块边界整体换计划 + 指针级装卸，零分配；被移除节点连同输出缓冲经 `RecycledNode`（已扩展携带 `output: AudioBuffer`）回执队列送回控制线程释放。
- **`ChannelStripProcessor`**：增益/声像（线性律）/静音升级为真节点（原 `TrackStrip` 求和段逻辑）；默认拓扑复刻旧 Mixer（`声源 → 延迟 → 通道条 → 主输出`），旧 mixer 全部数学测试以默认图形式通过。
- **行为变化**：`set_builtin_voice` 现在回收外部插件节点（旧实现驻留），切回插件需重新实例化（状态不保留）；UI 守卫经回收 `node_id` 正常配对释放。
- P1+P2 时 `mixer.rs` 曾随固定 Mixer 一起删除（`MixerLevels` → `GraphLevels`；`VoiceUpdate` → `GraphUpdate`）；P5f 重建 `mixer.rs` 承载 `MixerProcessor`（见下节）。

### Mixer 节点与单输入语义（P5f，D14–D21，2026-10-09）

- **单输入纪律（D14/D16）**：compile 期按 `NodeKind`（`Plain` / `Mixer` / `Master`）校验入边——Plain 与 Master ≤1、Mixer ≤32 且 `in_port` 唯一，超限整个计划拒绝（`FanInOverflow`）。隐式加合废止：新连线落到已占用 `(to, in_port)` 槽位时**顶掉旧线**（`add_cable` 严格替换语义，精确重复幂等）。
- **`MixerProcessor`（D15/D20）**：语义分两段——**per-channel 权重**（gain × pan 声像系数 × mute × solo）在图求和循环逐边乘（求和后通道分离不复存在，权重必须在求和时生效），经 `Processor::input_weights()` 暴露——带默认实现返回 `None` = 单位权重，全部内置与 CLAP/VST3 节点零改动，仅 Mixer 覆写；权重在 `set_param` 时重算缓存。**输出段**（pan/mute）原地处理，与旧通道条同数学。solo 作用域 = 单台 mixer（任一通道 solo → 未 solo 通道权重清零），级联 mixer 各自独立。
- **参数路径（D5/D20）**：`ParamId::{MixerGain(u8), MixerPan(u8), MixerMute(u8), MixerSolo(u8), MixerOutPan, MixerOutMute}`——推子/pan/mute/solo 全走节点参数路径（块边界生效），**永不触发拓扑重建**。
- **默认图（D18，2026-10-29 精简）**：默认 = 一条可弹 MIDI 轨（内置乐器 → `mixer.ch0`）+ 单个全局 Mixer（默认 1 通道）→ master + Computer 键盘 → 该乐器 + 每条 audio 轨一个未接线的生成器节点；默认**不挂 delay**。新建 MIDI 轨只增加空轨道位（`track_present` 表标记占用，不建节点/不接线/不扩 mixer），用户在 patch 自行搭链。`allocate_track_chain` 仅保留给引擎参考拓扑/测试，其电费点（`track_channel`）= 轨道链末端 delay。
- **旁路修订（D19）**：`remove_node` 旁路重连仅适用于 ≤1 入边节点（效果器照旧旁通，直连保留目标 `in_port`）；mixer/master 多入边无处重连——删除即弃线。
- **model（Q-B 决议）**：`NodeDoc::Mixer{node_id, track, inputs: u8(1..=32), channels: Vec<MixerChannel>, out_pan, out_muted}` 替代 ChannelStrip（旧变体保留仅作加载读路径，新文档不再产出）；加载期自动迁移——strip → 1 通道 mixer（原 gain/pan/mute 进通道 0）、master 入边 >1 时插入合成 mixer 承接。文档校验（load/save 双端）：非 mixer ≤1 音频入边、mixer `in_port` 唯一且 < `inputs`、通道数 1..=32。
- **UI**：mixer 节点画 N 个输入口（节点盒随通道数伸缩）+ 按口命中落线（精确口命中优先、否则自动落第一个空通道——任意空口可落，D21）+ 调音台风格面板（每通道 solo/mute/竖直推子/pan + 输出段 pan/mute + 通道增删按钮 1..=32，占用口不可删）；ChannelStrip 面板退役。
- **轨寻址（P5f ④）**：UI `track_channel_target(track)` 沿轨链连线走到首个 mixer 节点得 `(mixer_doc_id, in_port)`；轨推子/mute 读写对应 mixer 通道（文档与引擎参数路径同步），未接入 mixer 的轨回落默认值。`register_track_chain` 的 channel 槽位退化为电平点（传 delay）。
- **容量**：`MAX_IN_EDGES` 8→32（仅 mixer 可达）、`MAX_CABLES` 128→256（32 口 mixer 单节点即占 32 线）；32 通道求和 ≈8K FMA/块 ≈ 8µs，48kHz 下 <0.5% 单核。

### 引擎核心（`glitcherz-engine::realtime`）

- `AudioBuffer`、`Processor`（`set_param` + `note_event`）、`ProcessorChain`、`BlockRunner`（固定 128 块拆分 + 块边界命令消费 + `Feedback` 原子回传：主电平/位置/步进/走带/**每轨电平**）；
- **`Mixer`**：4 条 `TrackStrip`（`SynthProcessor → DelayProcessor`）并行处理 + 线性声像/增益/静音求和 → 主链（`MasterGain`）；`MixerLevels` 每块回传各轨 post-fader 峰值；
- 内置合成器链（`SynthProcessor`）：**16 声部**，每声部带限振荡器（Sine / Triangle / Saw / Square，锯齿与方波走 PolyBLEP）→ 独立 TPT SVF（低通/高通/带通，cutoff 在 log 域、resonance 在线性域各 10 ms 一阶平滑）→ 独立线性 ADSR；振荡器可单独使用（`ParamId::OscFrequency / OscGain / OscWaveform`，spec `spec/instrument.md`）。声部分配：空闲槽 → 同音先 release → 最老 steal；`Off{n}` 只释放对应音、`Off{0}` 为全释放哨兵；`OscFrequency` 直控只作用于最新 voice；Idle voice 整段跳过渲染，各 voice 在预分配 mono scratch 内渲染后累加（相同时刻 N 音完全可加，已用"和弦 = 独奏之和"测试锁定）。`MAX_VOICES` 编译期断言与 `MAX_PENDING_OFFS_PER_TRACK` 对齐。旧描述"正弦振荡器 + ADSR → TPT SVF → 反馈延迟"是 TrackStrip 时代的状态，滤波器当时并未接线；
- `Sequencer`（音频线程块边界采样时钟调度）：**纯窗口扫描**模型——只维护音乐位置（tick，1 四分音符 = 96），每块把 `[t0, t1)` 内到期事件一次性调度出来；BPM 变化只改未来速率（音乐位置不跳），循环回绕由窗口映射推导；**每轨双音源**（16 步步进模式 / 钢琴卷帘片段，`SetSource` 切换），卷帘音符按自身 start/length 触发；事件按 `(track, NoteEvent)` 送入对应轨道（≤1 块 ≈ 2.7 ms 抖动，逐样本偏移待后续）。

### 结构更新与节点回收（`realtime::voice`）

- `VoiceUpdate` 结构更新队列（rtrb 携 `Box<dyn Processor>`，**非 Copy**，控制侧分配 / 实时侧块边界只消费）；
- `TrackStrip` 声源**双槽预分配**（内置 `SynthProcessor` 常驻 + `external_voice: Option<Box<dyn Processor>>`，换装 = 指针写入零分配）；
- CLAP 效果器槽：`TrackStrip` 信号顺序固化为 `声源 → DelayProcessor → 外部效果器槽[0..4]`（`effect_slots: [Option<Box<dyn Processor>>; 4]` 预分配）；
- **节点回收回执队列**（`recycle_channel`，音频 → 控制方向）——被替换的旧节点由音频线程塞回执队列、控制线程 `AudioEngine::reclaim_nodes` 排空释放，UI 按 `node_id` 丢弃配对守卫后安全卸载动态库；实时线程**永不 drop 堆对象**（回收队列满时放弃卸装而非就地 drop）；
- UI 侧 `voice_guards` 注册表持有 `ClapInstanceGuard` / `Vst3InstanceGuard`（**不可提前丢弃**——卸载动态库会使实时节点悬空，进程退出统一回收）。

### 自动化（路线图第 8 步）

- `glitcherz-model` 的 `Automation`（`AutoParam` = 音量/声像/截止 + `AutoPoint{tick,value}` 有序列表，线性插值、两端保持端值）挂 `Track.automations`；
- 引擎侧 `RtAutomation`（定容 64 锚点 Copy）经 `SetAutomation` 命令 upsert（同参替换 / 空槽插入 / 空曲线=删除，每轨 ≤4 条）；
- 求值器在 `BlockRunner` 每块**仅播放中**按"循环后的音乐位置"（步进源 = 一小节、卷帘源 = 片段 loop_ticks 取模）采样并经混音器内部路径直写参数（不走命令队列）；UI 在走带停止沿重推手动参数实现"停止后回到手动值"；
- 插件参数桥接端到端（引擎集成测试锁定）：自动化曲线 → `AutoParam::PluginParam{param_id}` → `ParamId::PluginParam` → 求值器 → `TrackStrip`（`PluginParam` 只路由外部声源）→ `ClapNode` → `ParamValueEvent` → 插件生效。

### 曲线归一化与 Curve Map 节点（C1–C3，spec `curve-map.md`，2026-10-30）

- **存储域整体搬迁**（D1）：`AutoPoint::value` 从「目标域」（cutoff Hz / gain / 秒 / CC / 半音 / BPM）改为**归一化 `[-1,1]`**，`0` = 轴中心。轴跟着**目标**走：`MapSpec { min, max, mode }` 由 `AutoParam::default_map` 解出，**不写进文档**（D12：`CableDoc::map` 只在显式覆盖时才 `Some`，平时是 `None` = 按目标推导）。
- **`PROJECT_VERSION` 8 → 9**，`migrate_v8_to_v9` 是**唯一带逆映射的迁移**：先按每条 `CableKind::Automation` 连线的目标轴求 `MapSpec`（含效果器槽位 → `effect_slot_map` → 目标节点 payload），写进 `cable.map`，再对每条控制轨的每个锚点 `map.unmap(value)`；**悬空曲线**（无连线 / 目标不存在）保留数值并 `clamp(-1,1)`（用户选择：静默，不加提示）。
- **引擎只做一次算术**（D4/D10）：`AutoRoute.map` / `EventCable.map` 在编译期带上，`deliver_dest` 先 `if !value.is_finite() { return }` 再 `map.map(value)` 然后分派。`MapSpec` 是引擎自己的 `Copy` 类型（model 不依赖 engine），UI 的 `engine_map` / `engine_mode` 是唯一跨界点。
- **控制源声明自己的轴**（D6）：`Processor::map_spec() -> Option<MapSpec>` 默认 `None`；`MidiEnvelope`（min..max，倒置范围自动交换）与 `Portamento`（半音范围）返回 `Some`，投递循环里 `slot_map_spec(source).unwrap_or(edge.map)` —— 不需要第二遍解析，也不需要节点种类表。两者的输出值同时改为归一化（`level*2-1` / 对 `min..max` 归一）。
- **进入 Curve Map 的那一跳强制恒等**：`is_curve_map_in(dest)`（即 `AutoDest::Param(ParamId::CurveMapIn)`）在投递循环与 `auto_deliver` 里都跳过映射。否则「包络的 ±24 刻度」会先被算进 map 的输入、再从 map 输出算一次，等于缩放两次。
- **Curve Map 节点**（D5）：`NodeDoc::CurveMap` + `CurveMapParams{min,max,mode}`（默认 `-1..1 Linear` = **恒等**，插入即无副作用）；engine `CurveMapProcessor` 只有「存住输入 + 原值吐出 + 声明轴」三件事，`ParamId::{CurveMapIn, CurveMapMin/Max/Mode}`。它和包络/滑音在同一块内生效：排在小节排序下游，上游投递后其 `process_events` 当块即吐出。
- **校验上的两处不对称**（`GraphDoc::validate`）：map 输入 ≤1 **不分来源**（时间轴曲线与节点控制源共用一个直通寄存器），普通节点的控制入 ≤1 仍只约束节点源；`AutoParam::CurveMap` 只允许出现在 map 节点上，map 节点也只接受它。
- **轴表两份、测试对齐**：model 的 `effect_slot_axis`（迁移与无 map 连线要读，但 model 不能依赖 engine 的 `EffectDescriptor`）镜像引擎描述符的 `min/max`；UI 的 `axis_table_matches_the_engine_descriptors` 遍历全部效果器全部槽位，任何一边改范围就红。
- **UI**（C3）：`curve.rs` 只剩归一化 ↔ 屏幕映射（`y_of` / `norm_of_y`）、`axis_caption` 与 `map_in`；曲线编辑器/时间轴预览/piano-roll overlay 按归一化取高度，所以**换目标形状不变**。目标菜单每行追加轴摘要（`Cutoff · 20–18k Log`、`Pitch · ±24`）；map 节点体显示 `Map 20..18k · Log`，单控制端口拖线**直接连**（`single_control_port`），Inspector 有 Low/High/Mode 与 `−1 → x  0 → y  +1 → z` 读数。
- **顺带修掉的 M5 遗留 bug**：`resolve_panel_target` 的可编辑集合只有 instrument/effect/mixer，选中 MIDI FX / 控制节点时会一路回退到本轨乐器，于是 `midi_fx_params()` **实际不可达**（M5 的三个参数面板一直是死代码）。C3 把 Arp/Portamento/MidiEnvelope/CurveMap 补进集合——否则 Curve Map 的范围没有任何入口可改。测试 `control_nodes_are_editable_targets` 钉住。
- **D12 修订（轴推导而非快照）**：连接时不再把解析结果写进 `cable.map`（`connect_event_cable` / `set_auto_endpoint` 都留 `None`），`cable_map` 的优先级是「Curve Map 源从节点实时读（`params.map()`）→ `cable.map` 显式覆盖 → 目标默认轴」。`migrate_v8_to_v9` 也只在内存里解析一次用于逆映射锚点，不落文档。这样目标范围后改（效果器换类型、map 节点改 min/max、SF2 bend range）都不会让文档与引擎分叉。
- **bend range 多套了一层**：`AutoParam::PitchBend { range_semitones }` 本身是节点 `bend_range` 的副本，只推导 `default_map` 仍会读到旧值。`AutoParam::bend_range(graph, target)` 优先读 `VoiceSpec::SoundFont::bend_range`，没有再回落到参数值；`default_map` 与四处引擎分派（`apply_document`、`event_dest_of`、`set_auto_endpoint`、离线导出）共用它，`refresh_maps_for(doc_id)` 在 Inspector 改范围后重发该节点上的路由。分母对了，曲线才不会被压成几分之一。

### 反馈环路（P6，spec `node-graph.md` D10 修订，2026-10-21）

显式 **Feedback 节点**：环路必须经过它才合法，不含它的环照旧 `TopologyError::Cycle`。

- **1 块延迟 = 排序反转，不是缓冲**（本轮最省事的一刀）：编译期把「入 Feedback 的连线」视为**抽头**（tap）——该边不计入 Feedback 节点自身的入度（它构造上就是根），反而给**源**方的入度 +1，于是 Feedback 先跑、抽头源后跑，运行时抽头源缓冲里还留着上一块的输出。无额外延迟缓冲、无跨块状态、`ExecPlan` 依旧全 `Copy`。延迟 = `BLOCK` 样本（128 @ 48k ≈ 2.67 ms），写进 spec 供用户预期。
- **为何反转安全**：`F → … → X` 叠加 `X → F` 本来就是环，反转只是把这条既有的环约束换成"先 F 后 X"，不会凭空造出新环。释放抽头源的时机在**处理到 Feedback 节点时**（`checked_sub` 一次）；`checked_sub` 失败即自环（纯延迟没有源），报 `Cycle`。
- **`FeedbackProcessor` 只做 gain**：默认 0.5（裸环 unity 会持续或发散），`set_param(FeedbackGain)` clamp 0..1，unity 走 early-return fast path。文档侧 `NodeDoc::Feedback{node_id, track, gain}`（`#[serde(default)]` 默认 0.5）。
- **无环的 Feedback 同样延迟一块**：不特判"这节点其实没成环"，语义统一（否则图上"有没有环"会静默改变一处延迟，用户没法推理）。
- **删除不走旁路**：既有 D19 旁路要求 `in_count <= 1`，`bypassable` 额外排除 `NodeKind::Feedback`——旁路会把环重连成直连（多半直接变非法环），所以它的连线直接丢弃（mixer 同款）。
- **UI 接入**：patch 右键 "Feedback Tap"（`AddNodeKind::Feedback`，粉紫配色、默认落 x=600）、`inline_param_id`/`apply_inline_param` 支持 `FeedbackGain`（undo 键 `feedback-gain`）、`install_node` 装 `FeedbackProcessor` + `set_param`、`kind` 推导 `match node { Mixer→Mixer, Feedback→Feedback, _→Plain }`。
- **测试 5 项**（`graph.rs`）：`feedback_tap_is_exempt_from_cycle_rejection`（编译通过 + 断言 Feedback 排在 mixer 之前）、`feedback_tap_delays_one_block_and_applies_gain`（`y[n]=1+0.5y[n-1]` → 1 / 1.5 / 1.75 / 1.875，逐块钉住递推）、`feedback_tap_without_a_cycle_still_delays`、`feedback_self_loop_is_rejected`、`removing_a_feedback_node_drops_its_cables`。
- **踩坑**：`FeedbackProcessor` 忘了补进 `realtime/mod.rs` 的 `pub use graph::{…}`，UI 侧 `unresolved import`——新 `Processor` 类型照例要过一遍 re-export。

### 增量引擎同步（S2-5，2026-11-03）

**问题**：undo / redo 无条件走 `apply_document()`，即使只改了一个音符也会删掉全部节点再重装 —— 插件要重新 spawn + `load_state`，一次撤销的代价可能上百毫秒。

**两层判据**：
- `Project::topology_eq`：`graph.nodes`（含参数）、`graph.cables`、三类轨道数量全等 —— 严格版本，视图字段（名称 / 颜色 / clip 内容）不进比较。
- `Project::engine_topology_eq`（S2-5b）：只比**节点身份**。`node_id` / `track` 必比；`VoiceSpec` 只比 `path`（Clap/Vst3 再加 plugin id / class id 与 state）；内置效果器只比 `EffectParams::kind()`；mixer 只比 `inputs`；drum kit 只比每个 slot 的 `note` + `path`。synth / effect 数值、mixer 推子、SF2 bank/program/gain/bend、Sample root/mode/gain、Arp/Portamento/Env/LFO/CurveMap/Feedback/Input 参数全部视为可重放。

**分派**：`GlitcherZApp::sync_engine(before)` 在 `undo` / `redo` / `jump_history_to` / `execute` 前后调用。身份等价走 `refresh_clips_and_transport()`：master gain、BPM、拍号、**`push_node_params` + `push_mixer_params`**、MIDI / control / audio clip 表、track count、loop range；身份变化才 `apply_document()`。`push_all_clips` 因此退役。

**`push_node_params` 的重放面**（S2-5b 补齐）：Instrument 按 voice 分流（Builtin → `synth_param_values`；SoundFont → bank/program/gain/bend；Sample → root/play mode/gain；DrumKit → `DrumGain` + 每槽 gain/pan/tune/choke；Clap/Vst3 跳过，参数由宿主面板即时推送）；Effect Builtin → `BuiltinParam` 槽；Arp / Portamento / Env / LFO / CurveMap / Feedback / Input 逐 `ParamId` 推。这些 `set_param` 路径与构造期 `apply_params` 等价（engine crate 内两个入口共用同一套 clamp）。

**验证**：model crate 5 项（`topology_eq` 3 + `engine_topology_eq` 参数跳过 / voice·mixer 身份）；全仓 fmt/clippy/test 绿 + `--seconds 5` 冒烟。App 级"undo 不重建"无法在无窗口环境断言，以判据单测 + 结构（单一 `sync_engine` 入口）兜底。

### Z0：双音频输入口 + Ring Mod（2026-11-08）

spec `.agents/spec/dual-audio-input.md`。目标不是「加一个效果器」，而是让 `Processor` 第一次能同时读到两路**语义独立**的逐样本波形，为 Z1 侧链压缩 / Z2 Vocoder 铺路。

**引擎侧最小切面**：`ProcessContext` 全仓约 90 处字面量构造，绝不扩字段。改为 `Processor` 加带默认实现的 `process_with_aux(ctx, main, aux)`（默认转发 `process`），既有 processor 零改动；`RtGraph` 只对 `NodeKind::Dual` 调它。aux 是**整图一个** `AudioBuffer`（`RtGraph::new` 构造期分配 2ch × block_size），不进 `NodeDeployment` / `RecycledNode` / `PendingNode`，部署与回收协议完全不动。`process_block` 开头 `let mut aux = self.aux.take()`、结尾放回，是为了让求和同时借用 `self.slots[src]` 与 scratch，不用把 `self` 拆成一堆字段。

**最容易写错的两处**：

- **分流必须按 kind，不能按 `in_port == 1`**。mixer 的通道 1 也是 `in_port = 1`，按端口判断会把 mixer 的一个通道混进 aux。只有 `plan.kinds[s] == NodeKind::Dual` 时 `ports[i] == 1` 才表示 Mod。测试 `mixer_channel_one_stays_in_the_main_path` 用 0.25 + 0.5 → 0.75 锁住这条。
- **PDC 两条路径都要分流**。`compute_pdc` 本来就是逐入边算 `pdc_delay[slot][edge]`，与端口无关，不用改；但 `delay == 0` 的 `sum_into` 与 `sum_into_delayed` 两个分支都必须选对目标缓冲。测试用「Main 源声明 10 帧 latency + Mod 源一个脉冲」让 PDC 给 Mod 边补 10 帧，乘积脉冲正好落在第 10 帧；若 Mod 走了快路径，输出会全程为 0。

**端口防御分工**：`add_cable` 的 `allowed_ports(kind)` 负责**端口范围**（Plain/Master/Feedback = 1、Dual = 2、Mixer = `MAX_IN_EDGES`），`compile()` 的 `max_in()` 只管**入边总数**，两者不重叠。`remove_cables_to_port` 本来就保证 `(to, in_port)` 唯一，所以双口不需要新机制。`GraphDoc::validate` 同步放宽为按 `NodeDoc::audio_inputs()` 取上限 / 合法端口，master 仍单独硬校验 0。

**删除语义**：`remove_node` 的 `bypassable` 显式排除 `Dual`——双口节点即使只有一条入边，那条线也可能是 Mod，旁路会把调制源直接接进主信号。文档侧 `patch_ops::delete_graph_node` 同样用 `is_dual` 跳过旁路对生成。

**端口剖面的真源**：定稿 spec 写的是给 `EffectDescriptor` 加 `audio_inputs` 字段，实施时改成 `BuiltinEffectKind::audio_inputs() -> u8`（engine / model 各一份 `const fn`）。原因是 `EffectDescriptor` 是 14 处静态结构体字面量（+2 处测试构造），加字段要同步改 16 个文件且全是样板；端口数是**效果器种类**属性，与参数表 / 单位 / 平滑无关。已作为 §6.1 勘误写回 spec。`NodeDoc::audio_inputs()` 再由 `EffectParams::kind().audio_inputs()` 推导，UI 实时装配（`engine_sync::install_node`）与离线装配（`headless::assemble_offline`）两处都只查它。

**Ring Mod**：`out = lerp(main, main × mod, mix)`，逐样本、零状态、零延迟，`mix` 走 `EffectParamBank` 的 `BuiltinParam(0)`；`mix = 0` 严格不写 buffer（沿用 E 系列直通不变量）；`process` 转发到 `process_with_aux(..., None)`，所以单口图里缺 Mod 时调制器视为静音。它是 Z0 的数学验收样例，不单独立项。

**用户可见的坑（已写进 `docs/manual.md`）**：调制源**不要**再接 `Master`。D6 规定悬空节点照常执行但输出无人消费即静音，所以调制源只需连到 Dual 节点的 `Mod` 口；多连一根到 master 会把原始载波漏进主输出。

**验收**：新增 8 项 `graph.rs` 图级测试、1 项 model 校验 / RON 往返、1 项 `headless.rs` 离线渲染端到端（无 Mod 静音 / 有 Mod 出声）、`effect_rt_audit` 的双口零分配断言、Ring Mod 5 项单测；`fmt` / `clippy --workspace --all-targets -D warnings` / `test --workspace` / `build --workspace` 全绿。**零回归**：`9edaebf` worktree 编出的 pre-Z0 二进制与 post-Z0 二进制对同一默认工程渲染，两份 WAV 各 44288 帧 / 177196 字节，`cmp` 逐字节一致。

### clip 级 controller 事件（P0，spec `roll-multiclip.md`，2026-10-27）

**`Sequence.ctrls` 此前是四段全空**：模型自 M1.3 就有 `CtrlEvent`/`CtrlKind`（含 `PitchBend`），但 SMF 导入的兜底分支是 `_ => cursor.skip(2)?`（CC/bend/aftertouch/program **全部丢弃**）、导出只写 `0x90`/`0x80`、`RtMidiClip` 只有 `notes`、UI 无编辑入口——所以 P0 的实际范围是"导入解析 + 引擎调度 + 导出写入"三段，不是 spec 最初写的"导出补齐"。

**`advance` 的回调换成 trait**：原先 `emit: &mut dyn FnMut(usize, NoteEvent)`；加 ctrl 若再加一个闭包参数，block runner 需要两个闭包同时持有 `&mut RtGraph`（两次可变借用，编译不过）。改为 `sink: &mut dyn SequencerSink`（`note` / `ctrl` 两方法，`ctrl` 默认空实现让纯音符 sink 保持单方法），block runner 侧 `GraphSink(&mut RtGraph)` 一个适配器把两类事件送进同一张图。

**同 block 内的顺序是 ctrl → note**：pitch bend / program change 设定的是"音符起始状态"，晚一块到就是错音高/错音色。调度里 ctrl 循环放在 note 循环**之前**；SMF 导出同样把同 tick 顺序定为 off(0) → ctrl(1) → on(2)（`SmfEvent::order_rank`）。两者对齐，导入导出往返后行为一致。

**`write_track` 必须按 status 决定 data byte 数**：原格式 `(tick, status, d1, d2)` 固定写 3 字节；`0xC0`（program）与 `0xD0`（channel pressure）只有 **1 个** data byte，多写一个会让后续每个事件错位（文件仍能被"解析"，只是全乱）。改 `SmfEvent{tick,status,data1,data2,data_len}` + `two()`/`one()`。回归测试 `single_data_byte_ctrls_do_not_shift_later_events` 用 program→pressure→note 序列锁定。

**导入的 `origin` 改为"最早事件"**：原来只看 `notes.first()`；controller-only 的轨（只有弯音没有音符）会落 origin=0 且 span=1。现在 `min(note.start, ctrl.tick)`，span 也把 `ctrl.tick + 1` 算进去。

**`RtCtrlKind` 不能用 `#[derive(Default)]` + `#[default]`**：`#[default]` 只允许 unit 变体，首变体是 `Cc(u8)` → 手写 `Default`（`Cc(0)`），供 `[RtCtrl; MAX_CLIP_CTRLS]` 的数组初始化。

**定容与内存（2026-10-28 已作废，见下条）**：~~`MAX_CLIP_CTRLS = 32`…队列按元素大小预分配故可接受~~——该取舍正是 64 音符上限的成因，D15 修订后定容数组整体退场。

### clip 表改走结构队列（D15 修订，2026-10-28）

**触发**：导入一段 294 音符的 MIDI → 保存报 `MIDI track #0 clip #0 holds 294 notes, exceeding the limit 64`。旧 D15 让整张 clip 表（`[RtMidiClip; 8]`，音符/controller 定容数组）搭 `SequencerCommand::SetTrackClips` 这个 `Copy` 变体过命令队列；rtrb 按**最大变体**预分配 1024 个槽，于是"单 clip 音符数"和"队列内存"直接相乘：64 音符 ≈ 9.5 KB/槽 ≈ 9.7 MB 队列，512 音符就要 ~54 MB。真实 MIDI 一轨上千音符，上限根本抬不动。

**改法**：`RtMidiClip` 的 `notes`/`ctrls` 变成 `Vec`（控制线程拥有），`MidiClipTable = [RtMidiClip; MAX_TRACK_CLIPS]`。表经**结构队列**（`GraphUpdate::SetMidiClips`，与 `SetAudioClips` 同队列）下发；block runner 在结构更新循环里 `sequencer.set_track_clips(track, table)`，返回值是被顶下来的旧表，立刻走 `trash_midi_clips`（满则 park 到 `midi_trash_pending`，下一块重试；极端情况下 `mem::forget` 有界泄漏，绝不在音频线程 drop）。控制线程每帧 `AudioEngine::reclaim_midi_clips()` 排空释放。与 U6 D4 的 audio clip 表完全同构。

**control clip 表也迁出命令队列（同一修订的第二步）**：`ControlClipTable = [RtControlClip; MAX_TRACK_CLIPS]` 是纯 `Copy`（每条曲线定容 `[RtAutoPoint; 64]`，无堆所有权），但它有 32 × ~544 B ≈ 17 KB。留在命令队列就是 1024 槽 × 17 KB ≈ 17 MB 的启动预分配，且把 `SequencerCommand` 的每个变体都撑大。改成 `GraphUpdate::SetControlClips` 后，代价挪到结构队列的一次性 256 × 17 KB ≈ 4.5 MB。**它不需要垃圾回执**（没有 `Vec`/`Arc` 要释放），block runner 直接 `sequencer.set_control_clips(index, table)` 原地覆盖。这是一条通用判据（D25 的补充）：**载荷大就要走结构队列，载荷带堆所有权才需要回执队列**。

**`Sequencer.control_clips` 必须「直接填堆」，不只是装箱**：16 轨 × 32 clip × 544 B ≈ 278 KB 内联会让每次 `Sequencer` move 变成 278 KB 栈拷贝。第一步只写成 `Box::new(std::array::from_fn(...))` —— **这不够**：`Box::new(expr)` 要先把整个数组在栈上造出来再拷进堆，峰值仍是 ~550 KB。在 Windows 上主线程默认只有 1 MB 栈，实测 `cargo run` 一启动就 `STATUS_STACK_OVERFLOW (0xc00000fd)`；而 `cargo test` 的测试线程有 2 MB，所以单测全绿、app 挂——**这就是为什么这个 bug 没被测试抓住**。正解是 `Box<[ControlClipTable]>` + `(0..N).map(...).collect()`：`collect` 走 `Vec → into_boxed_slice`，逐元素写进堆缓冲区，栈上只有单个元素（17 KB）的临时量。通用教训：**给实时结构加定容大数组前先算 `size_of`，超过 ~16 KB 就该装箱；装箱还要确认临时量没有落回栈上**。

**为什么 `Vec` 而不是更大的定容数组**：队列里带 `Vec` 的变体只占表头（~24 B ×2 ×32 clip ≈ 1.5 KB），音符本体在堆上且长度随内容；定容数组则是 `32 × N × 12 B` 的无条件内存，且每次编辑要整表 memcpy（`push_all_clips` 每轨一份 × 16 轨）。`Vec` 版 `push`/`push_ctrl` 天然只能控制线程调用（可能扩容），文档注释已锁死这一点。

**`schedule_track_clips` 的借用调整**：原实现 `let clips = self.clips[track];` 靠 `Copy` 复制整表来避开 `&self` / `&mut self.pending_offs` 冲突。`Vec` 不可 `Copy`，改为两个不相交字段借用（`&self.clips[track]` + `&mut self.pending_offs[track]`），并把 `set_pending_off` 从方法降为自由函数（接收 `&mut` 槽位数组）——否则调用它要再借整个 `self`。

**导入分块**：`smf::split_into_clips` 按 `IMPORT_CLIP_NOTES = 4096` 切块，切点落在 start tick 变化处（和弦不跨 clip），块起点 = 块内最早事件，`loop == span` 单周期；块数超过 `MAX_CLIPS_PER_TRACK` 时用 `div_ceil` 放大块长，不丢材料。UI 在导入状态栏补一句「N dense track(s) split into M clips」，避免一排气 clip 看着像 bug。

**行为变化**：单 clip 音符上限从 64 抬到文档级 `MAX_CLIP_NOTES = 65536`（仅防病态文件）；`MAX_CLIP_CTRLS` 常量删除（controller 也换成 `Vec`，密集弯音不再被 32 截断）；`SequencerCommand::SetTrackClips` / `SetControlClips` 两个变体删除，命令枚举回到小结构；`MAX_CLIPS_PER_TRACK` 8 → 32（model 与 engine 的 `MAX_TRACK_CLIPS` 同步），一轨可以放 32 个段落 clip。


**只投递 CC 与 pitch bend**：`graph::slot_ctrl` 对 `ChannelPressure`/`PolyPressure`/`Program` 明确返回 `false`——内置声源没有对应的消费面，SF2 的 program 走 `ParamId::Sf2ProgramChange` 参数路径。VST3 宿主无 `Processor::midi_cc` 实现（`vst3-0.3.0` 的 `DataEvent` 只有 SysEx），因此拿到默认 `false`，与 U4 D6 一致。

**内置声源对 bend 返回 `false` 是有意的**：P1 会让它们的音高走 `ParamId::PitchOffset`（逐样本 `Smoothed`），既能表达细腻 vibrato，也不需要用户定义 bend range。

## 模型与文件格式

### 项目文件与模型（`glitcherz-model`）

- `Project`（metadata + gate + master_gain + 4×`Track`）；`Track` = 名称 + `Pattern` + `Clip`（`Note{start,length,note,velocity}` 列表 + loop_ticks，tick 基准：`TICKS_PER_QUARTER=96`/步=24/小节=384）+ 音源枚举（`VoiceSpec`: `Builtin | Clap | Vst3`）+ 混音（增益/声像/静音）+ `SynthParams` + `DelayParams`，全部 serde/RON + 字段级 `#[serde(default)]`；引擎侧镜像 `RtClip`（定容 128 音符 `Copy`，超容截断）经 `SetClip` 命令整块下发；
- `ProjectFile{version: u32, project}`（当前 `PROJECT_VERSION = 1`），`save_project`（RON pretty）/ `load_project` / `parse_project`；解析前用 `ron::Value` 探测顶层结构（含 `project`/`version` 键 → 包装格式，否则回退裸 `Project` 开发期格式——serde 会把裸格式的字段当未知字段"成功"忽略，必须先探测）；
- 程序化 `Default` 全部手写（派生 Default 会绕过 serde 字段默认值，曾导致 gate/tempo 为 0 的真 bug）。

### 自动保存与崩溃恢复（S3a，2026-11-03）

**信号用"文件存在"，不用时间戳比较**：快照在手存成功、明确丢弃（`FileAction::Exit`）、以及窗口关闭时文档为 clean（`Drop`）三条路径上都会被删除，所以启动时发现文件就等于"上一轮没干净退出"。`autosave::candidates` 因此只做 `metadata().is_file()`，不需要 mtime 判断谁更新 —— 规则可穷举，测试也不需要伪造修改时间。

**落点两套**：已存盘工程 → `<name>.glz.autosave`（同目录，跟着工程走）；从未存盘 → `<eframe::storage_dir("GlitcherZ")>/autosave/Untitled.glz`。`storage_dir` 返回 `None` 的平台只快照已存盘工程，不猜路径。

**写入**：`glitcherz_model::save_autosave` 复用抽出来的 `write_atomic`（`.tmp` → `sync_all` → rename），但**不写 `.bak`、不 `validate`** —— 快照不是用户要的版本，而"文档瞬时非法"不能成为丢工作的理由（加载路径仍 validate，坏快照在恢复时报错）。

**节奏**：`autosave::Clock` 只做时间算术（不 IO、不读时钟，`Instant` 由调用方给），`observe(revision, dirty, now)` 在 revision 变化时记 `last_edit`，`due(now)` = 脏 && revision ≠ 已写 revision && (静默 ≥30 s || 距上次写 ≥60 s)。`mark_written` 在成功**和失败**后都调用：满盘时不会每帧刷错误，而是下一分钟再试。

**为什么快照是克隆**：`capture_plugin_states` 走 `project_mut()`，会 bump revision 并置 dirty —— 在 tick 里直接调用等于每 30 s 自触发一次写盘（死循环）。所以 tick 里 `project().clone()` + `plugin_state_blobs()` + 自由函数 `apply_plugin_state(&mut snapshot, blobs)`，只写克隆；手存路径反过来把 blobs 应用到活文档（`capture_plugin_states` = 两者组合）。

**恢复**：`adopt_document(project, path, dirty)` 是 open / recover 共用的一段（`session.replace` → layout 导入 → `project_path` / recent → 清选区与卷帘状态 → `autosave_clock.reset` → `push_all_params` + `push_loop_range`）。恢复时先按快照所在目录清掉 `base_dir`（`load_project` 会把相对媒体根指向快照目录），再设为原工程目录，并 `Session::mark_dirty()` —— 快照从未以该工程保存过，标题必须带 `*`。走 `queue_blocking_job` 的 banner 三相位（安装插件与 SoundFont 与普通打开同样慢）。

**提示**：启动 `recovery_dialog` 是 modal，每项 `Recover` + 年龄 + 大小，底部 `Discard all` / `Later`。`Later` 只清列表不删文件（下次启动仍会问）；恢复一个不动其它（其它属于别的工程）。打开提示期间 `handle_keyboard_input` 让路。

**测试**（+6）：`an_autosave_reloads_without_leaving_a_backup`（两次写无 `.bak` / 无残留 `.tmp` / 可重载为同一文档）、`a_quiet_pause_arms_the_snapshot`、`continuous_editing_still_snapshots_on_the_spacing`、`a_clean_document_is_never_snapshotted`、`snapshots_are_found_beside_recent_projects_and_in_app_data`、`age_labels_stay_readable`。

### 媒体路径、收集与重定位（S3b，2026-11-03）

**一条规则，两个函数**：`Project::media_path_string` 决定存什么（在工程目录下 → 相对；否则绝对；分隔符一律 `/`），`Project::resolve_media_path` 决定怎么读（`\` 折回 `/`，相对则 join `base_dir`）。所有加载点（`obtain_sample_voice` / `obtain_sf2_voice` / `obtain_drum_kit_voice` / `build_audio_clip_table`）都只调 resolve，缓存键是**解析后的绝对路径** —— 否则两个工程的同名媒体会共用一条缓存。

**为什么存 `/`**：`strip_prefix` + `to_string_lossy` 在 Windows 上产出 `demo.media\kick.wav`，这个字符串在 macOS 上是一个**文件名**而不是路径。反过来老 Windows 工程里的 `\` 由 resolve 折回 `/`，所以两个方向都能开。

**收集（`Collect Media Files…`）**：遍历 `media_paths()`，跳过已在 `<stem>.media/` 里的，`media::collect_target` 给出落点——同名不同大小加 ` (2)`，同名同大小视为已收集（跳过复制，所以重复执行是幂等的，也不会把 300 MB 的 SF2 复制两份）。一步 undo（`Collect Media`）。**不 `apply_document`**：引擎里解码好的数据就是这些字节的副本。

**重定位（`Relocate Missing Media…`）**：`missing_media()` 给出**存储形式**的路径（这样才能按 key 重写），选一个目录后 `media::find_by_name` 在其下 BFS（深度 ≤4、预算 20000、大小写不敏感）按文件名匹配，重写后**必须** `apply_document()` —— 节点里装的是旧路径的源。仍缺的文件留在列表里，提示框不关。

**缺失提示**：`adopt_document` 末尾 `refresh_missing_media()`，所以打开 / 恢复工程都会算一次；非空弹 modal（路径列表 + `Locate Folder…` / `Continue without them`）。不做播放中转圈轮询——用户正在编辑时弹窗比不弹更烦。

**顺手修的一处**：Inspector 的 SF2 preset 表按**存储路径**查 `sf2_preset_names`，改成相对路径存盘后就查不到了（下拉空白）。现在按 `base_dir + stored` 解析后查（`graph_patch` 里 destructure 出 `session`，用 `session.project().base_dir` 而不是 `self.project()`，否则和 `patch_state` 的可变借用打架）。

**测试**（+4）：`media_references_are_enumerated_rewritten_and_checked`（去重 / 空路径跳过 / 重写 / 缺失判定）、`relative_media_paths_resolve_against_the_project_directory`（新增 `/` 存储与 `\` 兼容读两条断言）、`a_free_name_is_used_as_is` / `name_clashes_get_a_suffix_but_the_same_file_does_not`、`relocation_finds_a_file_by_name_inside_a_picked_folder`。

## 编辑与视图

### UI（`glitcherz-ui`，eframe 0.36 glow）

- **File 菜单**（U2 收口）：New / Open… / Open Recent ▸ / Save / Save As… / Import MIDI… / Export MIDI…，快捷键 `Ctrl+N` / `Ctrl+O` / `Ctrl+S` / `Ctrl+Shift+S`（优先于演奏键盘映射）；最近文件经 eframe `persistence` 存 RON，容量 8；窗口标题每帧按「文件名 + 未保存 `*`」重算；状态栏消息（8 秒自动消失）；`--open <文件>` 启动加载；
- 走带 + BPM、**自动化泳道**（`lane` 自绘组件：参数三选 + 启用开关 + 锚点点按/拖拽/右键删除，播放头走线，截止频率对数轴）、**混音器面板**（轨道选择/静音/post-fader 电平条/竖直推子/声像）、16 步网格（播放位置黄色描边）、**钢琴卷帘编辑器**（`piano_roll` 自绘组件：点空白绘制、拖拽移动、右键删除、滚轮滚动音高、播放头走线；fit-to-width 时间轴、1/64 吸附）、画笔音符/音长、滤波器/ADSR/延迟面板（作用于选中轨道）、虚拟键盘（屏幕 + 电脑键盘）、主输出电平表；
- 音序器音乐位置经 `Feedback.position_ticks`（AtomicF32）回传驱动播放头。

#### 文件管理（U2，2026-10-22）

- **`project_files` 模块**：只放纯函数与文件 IO（标题格式化、导出默认名、空轨判定、导入落位计划、SMF 读写包装），app 壳负责对话框 / undo / 引擎推送，因此不必起 egui 上下文即可测试。
- **导入落位**（`plan_import`）：先复用**空轨**（无音符且无 CC 任意 clip）再追加新轨，容量按 `MAX_MIDI_TRACKS`，超出部分丢弃并报数。`Project::default()` 有 1 条空 MIDI 轨，所以「新建 → 小导入」落在它里面，其余按需追加。
- **整次导入 = 一步 undo**：外层 `begin_edit` + `suspend_history`，内层 `add_midi_track()` 自身的 `commit_edit` 在挂起期间被丢弃；收尾 `resume_history` 后一次性 commit。轨道内容写入后逐轨 `send_track_clips`（不走 `apply_document` 全量重建，节点已由 `add_midi_track` 装好）。
- **`Session::is_dirty` 的可信度是前提**（此前形同虚设）：任何每帧执行的 `project_mut()` 都会置脏，于是保存后标题立刻变 `*`。三个元凶全部改为「读局部值 → `changed()` 才写」或「读取走 `project()`」——走带条的 tempo `DragValue`、Synth 面板的 master gain `Slider`、`control_editor_body` 的 `auto_endpoint` 读取。**新增每帧代码时不要直接 `DragValue::new(&mut self.project_mut()...)`**。
- **未保存确认**：`FileAction{New, Open(PathBuf), Exit}` 统一走 `request_file_action`；dirty 时挂进 `pending_file_action` 并弹 `egui::Modal`（Save / Discard changes / Cancel）。**Save 失败或用户取消保存时对话框保持打开、动作不执行**（`do_save` 因此返回 `bool`）。关窗拦截 = `viewport().close_requested()` → `CancelClose` → 答复后 `allow_close` 回放；auto-close 冒烟路径直接置 `allow_close` 绕过。modal 打开期间跳过 `handle_keyboard_input`（否则 Space 会边弹窗边播放）。
- **已知取舍**：undo 退回到已保存状态仍显示 `*`（要清必须比内容 hash，成本不值）；SMF 导入不搬文件里的 tempo（`import_smf` 只回轨道与拍号），沿用当前工程 BPM。

#### 拍号（U3a，2026-10-22）

- **模型**：`TimeSignature{numerator, denominator}`（`serde(default)` → 旧文档读出 4/4，**PROJECT_VERSION 仍 6**）；`new()` 钳制（分子 1..=16、分母向上取到 2 的幂 ≤32），`is_valid()` 才是 `Project::validate` 的严格检查。派生 `ticks_per_bar()` = `TPQ × 4 / den × num`、`ticks_per_beat()`（= 分母的音符时值，用于 `Pos bar.beat` 读数）。`TICKS_PER_BAR` 常量保留为 4/4 默认（新建 clip 的兜底），不再是唯一真源。
- **引擎镜像**：`glitcherz-engine` 不依赖 model，`ticks_per_bar_of()` 是第二份公式，由 `tests/tick_base_consistency.rs` 对锁（4/4 3/4 2/4 5/4 6/8 7/8 9/8 全比对）。`Sequencer::bar_ticks` 只在 `SetTimeSignature` 时变，改拍号**不碰 transport 位置**，只触发 `refresh_arrangement_end`（排列末尾按新小节取整）。
- **UI 侧**：`TimelineParams.bar_ticks` / `piano_roll::show(..., bar_ticks, ...)` 把文档拍号传进两个自绘视图；`snap_bar` / `knife_target` / `grab_targets` / `stretch_targets` 全部显式接收 bar 长度（原来都闭包捕获 `TICKS_PER_BAR`）。走带拍号下拉一步 undo，且**只改网格不改事件**。
- **排列上限是 tick 而不是小节**：`MAX_ARRANGEMENT_TICKS` 保持原值（4/4 下 256 小节），视图宽度按 `ceil(limit / bar_ticks)` 铺满——7/8 下能放 292 小节、3/4 下 341 小节，内容上限始终是同一段时间。
- **改拍号后的既有对象停在原 tick**：loop 端点、marker、clip 不吸附也不换算（可能不再落在大节线上）；下一次手动拖动才按新网格吸附。这是"拍号是记谱不是变换"的直接结论，别在 UI 里加"自动重排"。
- **SMF**：导出写真实 `FF 58`（`dd = log2(denominator)`，`cc=24` / `bb=8` 不变）；`import_smf` 返回 `SmfImport{tracks, time_signature}`，首个 `FF 58` 胜出。**导入策略**：`project_files::is_unwritten`（所有 MIDI 轨无音符无 CC 且无控制 clip）时继承文件拍号（此时"导入"等价于"打开"），否则保留工程拍号并在状态栏说明差异——一次导入不应偷偷重划已有内容的网格。

#### tempo 曲线（U3b，2026-10-22）

- **`Timeline In` 不是节点，是身份**：`PortRef::TimelineIn` 与 `PortRef::Master` 同级——无 node_id、不进 `graph.nodes`、无 engine processor、没有 `AddNodeKind`。单例由"不可实例化"保证，所以 `validate` 里**没有**任何"只能有一个"的规则，只有连线规则三条：指向它的线必须是 `Automation`；`AutoParam::Tempo` 只能指向它；它自己的端口表 v1 只有 `Tempo`（`AutoParam` 落成节点参数一律拒绝）。patch 里它就是一个固定 box（`PatchId::TimelineIn`，1 个入端口、0 个出端口），`LayoutDoc.timeline_in` 记住它的位置。
- **引擎用哨兵路由，不走 `slot`**：`AutoRoute.slot == TIMELINE_IN`（`u16::MAX - 1`，与 `NO_EDGE = u16::MAX` 区分）表示"这条曲线驱动时钟"。`GraphCompiler::set_timeline_in_route()` 直接写这个哨兵，不做 `node_slot` 查找；`RtGraph::auto_set_param` 见到哨兵立即返回（它永远不能落进 `slots`）；block runner 反过来需要一个**只读**的 `RtGraph::auto_route(index)` 来分辨"节点目标 / 时钟目标"。
- **顺序是功能的一半（D4）**：`evaluate_automation` 从 `advance()` **之后**移到**之前**，用块起始位置采样。放在之后会让每一块都用上一块的 tempo 跑 10 ms——曲线永远慢一拍，长 ramp 上能听出来。改这一处时同时改 `run_next_block` 的步骤注释，否则下一个人会"顺手"挪回去。
- **每块先清 override**：`evaluate_automation` 开头 `clear_bpm_override()`，再按"首个覆盖当前位置的 clip"设置。空隙里自动回落 `base_bpm`，这与普通参数"clip 外保持最后值"是**有意相反**的：时钟必须有一个确定的基值，否则一段 tempo 曲线结束后速度会永远停在最后一点（D3）。
- **borrow 拆分的写法**：在 `evaluate_automation` 里同时要 `&sequencer.control_clips()` 与 `&mut graph`，还要随后 `&mut sequencer`。用 `let Self { sequencer, graph, .. } = self;` 拿到两个字段，再把命中结果收进**固定容量栈数组**（`[(usize, f32); MAX_AUTO_TRACKS]` + 计数），最后统一投递——不要为了图省事 `Vec`，那是 RT 违规。
- **`Sequencer` 两层 tempo**：`base_bpm`（用户拧的）+ `bpm_override: Option<f32>`（每块重算），`effective_bpm()` 统一钳制 20..300（`MIN_BPM`/`MAX_BPM`，非有限值回落 120）。`play` / `stop` / `pause` 清 override；`seek` **不清**——seek 后下一块立刻按目标 tick 重采样，这正是 D4 想要的（覆盖值只活一块）。
- **UI**：patch 里从控制轨出口拖到 `Timeline In` 的 box 上直接成线（`PatchAction::ConnectTempoIn`，跳过参数弹窗——单端口没有可选项）；Synth 面板的 `Pick Parameter…` 顶部固定一条 `Song Tempo (BPM)`（`AutoTarget::TimelineIn`）。走带 BPM 字段永远显示**用户基值**，播放中若曲线接管，旁边追加 `→ 143.2 auto`（读 `Feedback.bpm` 原子）。曲线纵轴 20..300 对数轴（`lane::axis`）。
- **`param_id_of` 改返回 `Option<ParamId>`**：`Tempo` 没有也不该有 `ParamId`（它不进 `set_param`）。返回 `Some(default)` 之类的占位值会在将来某次重构里悄悄把 tempo 送进节点通路，宁可让类型系统顶住。调用点只有两处（`apply_document` 的自动化分支、`set_auto_endpoint`）。
- **测试**：model `timeline_in_accepts_only_tempo_automation`（3 条非法连线逐一拒绝）+ RON 往返（tag `timeline_in` / `Tempo`）；engine `tempo_override_overrides_base_until_cleared` / `transport_edges_drop_the_tempo_override` / `tempo_curve_overrides_the_transport_clock`（位置按 `bpm/60×TPQ/sr` 公式算，**别按 48 kHz 硬编码**——block 测试的 harness 是 44.1 kHz）/ `tempo_curve_gap_falls_back_to_base` / `no_tempo_route_keeps_the_base_bpm`（含"接线 → 生效 → 剪线 → 回落"）；patch `timeline_in_ports_and_cable_rules`。
- **默认模板（2026-10-29）**：`Project::default()` = 1 条 MIDI 轨（带内置乐器、无 delay）+ 1 条控制轨 + 1 条 audio 轨（生成器节点默认未接 mixer）+ 单通道全局 mixer。新建 MIDI 轨只加空轨道位，自动化/音频轨可由工具栏显式添加。

#### 钢琴卷帘拖动协议（`piano_roll::DragSelection`）

拖动手势**不持有音符索引**，改为「冻结锚点 + 增量发射」，两条不变量：

- **冻结锚点**：抓取瞬间把音符的 `start` / `end` 存进 `anchor_start` / `anchor_end`。每条 MIDI 命令结束都会`sort_by_key` 重排 clip，索引在第一帧之后就失效了；锚点让手势始终跟着用户真正抓住的那个音符。
- **增量发射**：每帧计算**绝对目标**，只发出 `目标 - 已累计` 的差额（`applied_start` / `applied_end` / `applied_note`）。命令侧是相对增量（`start + ticks`），若每帧重发抓取点起的总偏移会按 1+2+3+… 复利累加，音符会「飞走」。

`clamped_move` 可能吞掉部分 delta，因此累加的是**实际生效值**（从返回的 `MoveNotes` 里读回），不是请求值。`resolve_created` 改为接收新音符的 `start` / `length`（`note_add` 刚建时的几何），不再靠索引回读。

#### 命中判定必须与绘制共用同一套几何

音符的**绘制矩形**由 `note_rect()` 独占定义，`note_at()`（命中）、`note_edges()`（边缘 trim 判定）、marquee 相交测试全部复用它。绘制与命中一旦各写一份就会静默分叉：

- tick 区间判定（`start <= tick < end`）与绘制的 `max(3.0)` 最小宽度不一致——短音符看起来比它的 tick 区间宽；
- 左闭右开区间**排除了右边缘本身**，于是拖尾落在空白处，触发"新建音符"而不是拉伸末端。

命中矩形在绘制矩形之外再扩 `HIT_SLOP`（x 4 px / y 1 px）——音符末端是 1–2 px 的瞄准目标，按下点经常落在绘制矩形外侧几像素，没有宽容度就会被判成空白。`note_edges()`（trim 的视觉边缘）与绘制保持精确、不带 slop。

`edge_zone` 的两个 8 px（`EDGE_PX`）区在窄音符上会重叠，此时按**离哪条边更近**决定，避免拖头却拉伸尾。

#### 手势分类必须用按下点，不能用 `drag_started` 帧的指针位置

**这是"拖头变移动、拖尾变新建"的真正根因。** egui 要等指针移动超过 `max_click_dist`（默认 6 pt）才报告 `drag_started`，因此该帧的 `interact_pointer_pos()` 已经比按下点滑出 6+ pt：抓在音符头部的判定读成"音符中部"（→ 整体移动），抓在尾部外侧的判定读成"空白"（→ 新建音符）。移动之所以碰巧正常，只是因为音符够宽、滑出 6 pt 后仍在中部。

修法：`drag_started` 分支改读 `input(|i| i.pointer.press_origin())`（"Where did the current click/drag originate?"），命中判定、边缘判定、移动锚点全部以**按下点**为准。副作用是移动锚点也变精确了——音符第一帧就贴合指针，不再先跳 6 pt。该契约由 `tests/egui_event_semantics.rs::drag_started_keeps_the_press_point_separate_from_the_current_pos` 钉住（若未来 egui 改掉 6 pt 阈值语义，测试先红）。

`clicked()` 帧不能这么改——`press_origin` 在松开后变 `None`，click 判定继续用 `interact_pointer_pos()`（egui 的 click 要求移动 < 6 pt，位置足够接近按下点）。

#### M4a 编辑交互模型（状态 × 目标 × 限定键）

钢琴卷帘的语义决策源从"指针落点 × 修饰键"移到**离散状态**（spec 见 `spec/editing-model.md`）：常驻态只有 Select，`d` 切到粘性 Draw；G/S/T/K/Shift+D 是一次性命令族（`RollState::arm` 唤起、拖拽执行、Esc 取消、`drag_stopped` 消费）。映射矩阵由 `tests/roll_state_machine.rs` 的 22 项集成测试逐格钉住（合成事件灌进真实 egui 帧，断言发射的 intent）。

落地时撞上三个 egui 0.36 契约，全部写进了测试脚手架的注释里：

1. **合成帧的时间步长同时受两个阈值夹击**：`max_click_duration`（0.8 s）内不松开就不是 click（4 帧 click 的 press→release 跨 2 步，步长必须 < 0.4 s）；`max_double_click_delay`（0.3 s）内两次 click 会配对成双击（整次 click 跨 4 步，步长必须 > 0.075 s）。取 0.1 s/帧。用 1 s/帧会让**每次 click 都被判成 drag**（按住超时 → `is_decidedly_dragging`），症状是选择逻辑全走 drag_started 分支、`response.clicked()` 永不触发。
2. **Escape 会被 egui 的焦点机制在半路拦截**：`Memory::begin_pass` 在任何 widget 布局之前就把 `focused_widget` 清空（除非该 widget 的 `EventFilter` 认领 Escape）。卷帘要靠 `set_focus_lock_filter(id, EventFilter { escape: true, .. })` 才能收到那个"取消命令"的按键。
3. **`drag_started` 帧上 `dragged` 同时为真**：手势的装配 intent（Ctrl-drag 的 Duplicate、draw 的 AddNote）会被同帧的增量发射覆盖——一个 intent/帧的架构下，复制拖动自 M3.8 起一直是坏的（只移动、不复制，且无集成测试覆盖）。修法：`response.dragged() && !(response.drag_started() && intent.is_some())`，装配帧只装配，增量从下一帧开始。

另修两个连带问题：工具栏提示文字过长会**撑大 ui 的可用宽度**（egui 每次分配后都把 `max_rect` 扩到包含子 rect，卷帘于是比面板还宽、画到屏幕外）——加 `.truncate()`；`apply_axis` 原先先锁 `pending` 命令，但 armed Grab 的 pending 要等 `drag_stopped` 才消费，mid-drag 按 X/Y 锁的是过期条目——改为运行中的 drag 优先。

**M4a.4 实测反馈修订**（同日，Blender 式模态改定，spec §3.3 同步）：

- **G/S/T/Shift+D 改为模态**：不再"arm 后等拖拽"——按键即进入模态，选区以 ghost 半透明预览逐帧跟随指针（`Modal::frame` 纯几何计算，不写文档），LMB 落定（`Modal::finalize` 一步算出总量 intent），RMB/Esc 取消。因模态期间文档不变，选区 index 全程有效，无需快照/恢复命令；取消零成本是"预览不落盘"的直接红利。拖动回归纯 tweak 手势。
- **K 改为轨迹切刀**：stroke 沿划动路径收集切口（`knife_crossing`：斜穿行取与行中线交点，同行扫过取覆盖区间中点；触边界不切，防零长半音符），**无需预选**；切口在 `drag_stopped` 一批落下（`SplitNotes { cuts: Vec<(usize, i64)> }`）——中途落盘会让 split 的重排序毁掉后续帧的 index 测试，批量是唯一安全形态。`midi.notes_split` 签名从 `{indices, tick}` 改为 `{cuts: [{index, tick}]}`。
- **Scale pivot 改为选区 bbox 起点**（原 bbox 中心）：从起点缩放保证音符间相对关系不动（起点左侧没有音符，命令层钳制形同虚设——正确性白拿）。
- **按键卫生**：所有已处理按键 `consume_key`（modifiers 逻辑匹配，Shift+D 先于 d 消费）；`EventFilter` 扩到 `horizontal_arrows/vertical_arrows: true`——egui 的箭头焦点导航正是"上下箭头 nudge 一次后焦点逃逸"的根源；K 键 beep 疑似未消费按键在 Windows 的系统 beep，消费后待复测。新增 Delete 键删除选区。

**M4a.5–M4a.7 实测连环修订**（同日）：

- **M4a.5 ghost 可见性**：模态期间选中原音符全不透明会把半透明 ghost 盖住（S/T 起点附近最明显）。按 Blender 惯例改为原位调暗（alpha 60）、预览实色（ghost alpha 110→170）；identity 帧（刚按键未动）不触发调暗。
- **M4a.6 焦点门可视化**："D/G/T/S 全部无响应、指针正常"——排查结论是 `has_focus()` 门不可见（夹具因校准点击天然有焦点，焦点门从未被测试覆盖；代码层无回归路径）。两处落地：工具栏无焦点时显示黄色提示 chip（`RollState.had_focus` 记上帧）；焦点获取从"分类为 click 才请求"加固为**任意按下即获焦**（慢按住既非 click 也非 drag 的边角）。诊断分叉：按 D 有声=焦点门关；无声=事件没到 egui（OS/输入法层；winit 在 Windows 建窗即禁 IME，egui-winit 只在 TextEdit 获焦时开 IME）。
- **M4a.7 虚拟钢琴让键**：roll 获焦后 D/G/T/S/K 同时是 FL 布局琴键会发声。roll 把 widget id 注册进 `RollState`，app 的电脑键盘处理经 `owns_keyboard_focus(ctx)`（egui `Memory::has_focus` 实时查询，无帧延迟、无 stale）跳过 note-on；note-off 仍放行防卡音。规则一句话：**点了 roll 键盘归编辑器，点别处归虚拟钢琴**。

#### M4b 时间轴命令族（选区 → 切刀 → 模态 → 复制）

四段交付，全部复用 M4a 的模态机制与 M4b.1 的焦点管线；决策项（spec §4）用户拍板：**G=自由移动+X 锁时间；重叠=部分允许、完全拒绝；K 吸附整 bar；Shift+D 复制纳入时间轴**。

- **M4b.1 选区模型**：`ClipSel{is_control, track, clip}` 跨轨选区；跨行框选由各行上报自己段内命中、`show()` 帧末统一合并——**带必须在看到 `drag_stopped` 的行之后延迟一帧清除**（`marquee_ending`），否则同帧行序靠后的行会丢末帧命中。焦点管线对齐 roll（任意按下即获焦 + Escape filter + 焦点 chip + 虚拟钢琴门控扩展）。Delete 批删 `RemoveClipsBatch`（每轨索引降序应用防位移，一步 undo）。
- **M4b.2 切刀**：K 唤起（单次消费），点击在 `snap_bar` 处切开；两侧各 ≥1 bar 否则一帧状态提示拒绝。`split_clip_in` 纯函数：左半保留 start 缩短 span，右半携带切口之后的内容（平移到新原点，跨界音符在两半延续）插在 index+1；两半各自单周期（`loop = span`，见文末"单周期不变量"）。armed 期间指针归切刀（无拖动无框选）。
- **M4b.3 模态 G/T**：G = 时间 bar delta + 同类轨 row delta 的**刚性移动**（行 delta 全局钳制不逐 clip，组内相对关系不动）；X 切时间锁。T 默认拉终点、Shift+T 拉起点（bar 对齐、≥1 bar、不出编排边界）。ghost 经**行几何注册表**跨行绘制（目标可落同类任意行，行先注册 `lane` rect，ghost 帧行末统一画）；原位变暗 alpha 60（M4a.5 惯例）。落定校验 `filter_covered`：目标**完全覆盖**既有（非移动）clip 时该 clip 留原位+提示，批次其余照常落下（部分重叠允许）。`relocate_in`：每源轨降序 remove + 目的轨 append——**append 不位移任何剩余索引**，跨轨移动因此不需要索引重映射。
- **M4b.4 Shift+D**：克隆选区进入 grab 式模态（`ModalKind::Duplicate` 与 Grab 共享 `grab_targets` 数学与 X 锁），LMB 落定 `DuplicateClipsBatch`（内容克隆 append 到目标轨、选区转移到副本），RMB/Esc 副本不落盘。

测试抓到的真 bug：stretch 首版把 **bar 数当 tick 直接加**（`bar_delta` 未乘 `TICKS_PER_BAR`），位移量小 3840 倍——纯函数单测（指针 3 bar → 期望整 bar 对齐）当场抓住。教训照旧：几何数学全部收进纯函数并单测，UI 只做取值与绘制。

### 内嵌 CJK 字体兜底（2026-11-03）

- **症状与根因**：轨名/片段名开放用户命名后，中文输入落到 epaint 的替换字形（U+FFFD）显示成豆腐块——`default_fonts` 只含 Latin/Cyrillic，全仓此前没有任何 `FontDefinitions`。
- **方案**：`glitcherz-ui/src/fonts.rs` 用 `include_bytes!` 内嵌 **Noto Sans SC Regular 子集**（23,187 字形 / 4.97 MB），`GlitcherZApp::new` 开头 `fonts::install(&cc.egui_ctx)`，内部 `Context::add_font(FontInsert::new(..), FontPriority::Lowest)` 同时挂到 `Proportional` 与 `Monospace` 两族末尾。
- **为什么 `Lowest` 而不是替换默认族**：Latin 仍走 egui 自带 Ubuntu-Light / Hack，行高与所有按现有度量调过的布局一分不变；CJK 只在前面几族都没有该字形时才被取用。测试 `latin_row_height_stays_with_the_bundled_face` 卡这条——Noto 面高 1.448 em vs Ubuntu-Light 1.149 em，插到最前会让拉丁行高 +26%。
- **为什么 Noto Sans SC 而不是思源黑体 CN**：同一设计、同一度量（同一 Adobe/Google 项目，CN/SC 是同一批字形），差别只在许可——Source Han 家族名带 OFL 保留字体名 "Source"，子集属于 Modified Version，再分发必须先改名；"Noto Sans SC" 不含保留名，可原样发布。OFL 全文随包 `assets/fonts/OFL.txt`。
- **子集口径**（可复现命令见 `assets/fonts/README.md`）：ASCII / Latin-1 / 常用标点 / 箭头 / 数学符号 / CJK 标点 / 假名 / CJK 基本区 U+4E00–9FFF **全部 20,976 个码位** / 兼容表意 / 全角半角 / 罗马数字 / 圈括号数字 ①⑴㈠ / 制表符与块元素 / 注音 ㄅ–ㄩ / 兼容单位 ㎎㎜㊣。`--no-hinting` 白省 1.3 MB（epaint 经 `skrifa` 光栅化，CFF hint 本来就不用）。原始 8.3 MB → 带 hint 子集 6.3 MB → 4.97 MB。
- **覆盖结论（实测）**：GBK 的 21,791 个字符，子集自身命中 21,654，其余由 egui 自带的 Ubuntu-Light / NotoEmoji 兜住（希腊、西里尔、拼音声调、★☆），只剩 `≒`(U+2252) 与 `⌒`(U+2312) 无字形——这两个上游字体本身就没有。汉字侧：GBK 的 21,003 个汉字 100% 覆盖，且额外多出基本区里 GBK 缺的约 5,000 个（玥、喆、堃、龘）；CJK 扩展 A/B 与韩文不收。
- **不做**：运行时读系统字体（三平台字形不一致，Linux 可能压根没有）、CJK 扩展 A/B（生僻字，不值 1 MB+ 的代价）。
- **验证**：`fonts::tests` 两项——`embedded_font_renders_cjk_without_replacement_glyphs`（"鼓组 Bass 01" 逐字无 U+FFFD、advance 非零、CJK 字形拿到 atlas 槽位）与上行行高测试。

### 一批 UI 修正（2026-10-23：meter 横放 / timeline mute 移除 / 编辑器坐标 / 参数面板门控 / picker 点击）

- **Master meter 横放**：`master_meter` 改两条水平条（上 L 下 R，各 7px 高），`allocate_exact_size(ui.available_width(), 18.0)` 吃掉走带行剩余宽度；调用顺序 = gain 推子 → `Panic` → meter（meter 在 Panic 右侧）。
- **timeline 的 M 按钮删除**（mute 归 mixer 节点）：`TimelineAction::ToggleMute` / `TimelineParams::muted` / `HeaderKind::Midi::muted` / `track_mixer_channel_state` 全链删除；MIDI header 名字按钮宽度 `HEADER_WIDTH - 56` → `- 16`（只剩 grip 占位）。
- **插件 GUI 坐标（重要根因修正）**：`reposition_rect_platform` 删除 `ClientToScreen` origin 减法。egui 坐标 = client 相对（egui_winit 以 `Pos2::ZERO` 构造 `screen_rect`），child 窗口坐标 = 父 client 相对，`rect * ppp` 直接对上。旧代码多减 origin = 容器上移一个标题栏高度——这就是从 G2 起一直存在的"GUI 盖住按钮 / Editor 标题"的根因。
- **Instrument 参数面板按 voice 门控**：第二个 Inspector Instrument 块里先解析 `voice_is_vst3` / `voice_is_clap`，`Plugin Params` / `Plugin Params CLAP` 两个 CollapsingHeader 各自只在对应 voice 时渲染；built-in / SoundFont 乐器不再出现空面板（与 effect 面板"有参数数据才出现"的规则对齐）。
- **picker 鼠标点击失效（egui hit-testing 坑，必记）**：egui 0.31+ 的点击命中停在指针下**最上层** widget，不穿透——picker 行内 `Label`（hover sense）注册在行 response **之后**，把点击吸掉了，所以只有 Enter（键盘不走 hit-test）能选。修法：`allocate_exact_size(Sense::hover())` 占位 → 画内容 → **最后** `ui.interact(rect, 显式id, Sense::click())` 注册（行成为自己标签的上层）。行 id 显式化还顺带解决了"Recent 分组条件渲染导致自动 id 漂移"的隐患；`mouse_click_picks_a_row` headless 测试锁住行为（`ctx.read_response` 取行 rect → 跨帧 press/release）。注意 read_response 要等 popup **settle 帧之后**（开帧是 sizing pass，行可能没画）。

### 控制轨 CC 输出与 inline 曲线带（U4a/U4b，spec `control-tracks.md`，2026-10-23）

- **CC 是电缆的"目标模式"，不是新的电缆类型**（D1）：端点仍走 `CableKind::Automation`（`AutoTrack → Node`），拖线、删除、单源约束、`validate` 全部零改动——节点目标已拒绝 timeline-only 参数、`TimelineIn` 只收 `Tempo`，所以 CC 自动落在正确位置，**结构本身保证合法性**。
- **`AutoDest` 分派**（D2）：引擎侧 `AutoRoute{slot, param: ParamId}` 换成 `AutoRoute{slot, dest: AutoDest}`，`Param(ParamId) | Cc{controller, channel}`；`RtGraph::auto_set_param` 改名 `auto_deliver`，按 dest 分派到 `slot_set_param` 或 `slot_midi_cc`。`Processor::midi_cc(channel, controller, value) -> bool` 是**新开的一条显式通道**（D3），不复用 `NoteEvent` 变体——音符路径的消费方（SF2 voice 池、插件事件槽）与 CC 无关，混进来只会让两边的分支都变脏。
- **值域与去重**（D4/D7，值域部分**已被 C1 归一化取代**，见下节；去重逻辑仍然有效）：曲线存**真实 7bit CC 值**（不是归一化的 0..1），纵轴 0..127；投递时 `clamp(0,127).round()`（round 而非 truncate，否则曲线停在 0.4 附近永远发 0），`last_cc: [i16; MAX_AUTO_TRACKS]` 记上次发送值（`-1` = 从未发）→ **变化才发**。`apply_update` 末尾把 `last_cc` 清成全 `-1`：重定向到新目标后必须立刻收到一次当前值，否则要等曲线动起来才生效。
- **时间戳取块边界**（D5）：CLAP 走 `MidiEvent::new(0, 0, data)`（`time = 0`），SF2 走块首 `process_midi_message`；采样级 CC 需要每采样事件队列，收益不可辨，明确不做。
- **格式差异**（D6）：CLAP 组装 `0xB0 | channel` 注入输入事件流；SF2 **忽略外来 channel**（该节点所有音符都发在固定 `CHANNEL`，尊重外来 channel 反而静默失效）；**VST3 明确不支持**——`vst3-0.3.0` 的 `DataEvent` 只有 `kMidiSysEx`，`kLegacyMIDICCOutEvent` 是插件**输出**语义，宿主用它属未定义行为，`Vst3Node::midi_cc` 返回 `false`。
- **UI 接线**：终点行多一条 `MIDI CC…`（就地 `CC`/`ch` 两个 DragValue，release 才提交 = 一次 undo）、端点显示 `cc_label()`、拖线菜单对 instrument 追加 8 条预设（1/2/7/10/11/64/71/74）；`set_auto_endpoint` 按 `param.cc_of()` 在 `set_auto_cc_route` / `set_auto_route` 间分派，文档重推（undo/open）那条路径同样分流。
- **~~inline 曲线带~~（U4b D9/D10，已由 U12 取代）**：选中控制轨时，其行**下方**展开 84 px 高的带（`curve_band()`）……（历史记录，见下方 U12）
- **~~单一编辑入口~~（D10，已由 U12 取代）**：`EditTarget::Control` / `control_editor_body` / `OpenControlEditor` 全部删除……（历史记录，见下方 U12）
- **手势与 undo**：点击加锚（`dragging: false` → 单步 `Add Anchor`）、拖拽移动（`curve:{track}:{clip}` 合并键，drag end 收合并）、右键删最近锚（半格容差）。单击已有锚点时**不移动它**——低缩放下 6 px 命中容差可能跨过整格吸附，顺手改掉才是 bug。（U12 后这些手势搬进 Edit tab 的曲线编辑器，方法改名 `curve_upsert` / `curve_remove_point`。）
- **U12（2026-10-26，spec `control-tracks.md` §8，取代 D9/D10）**：曲线编辑回到 **Edit tab**，timeline 只留只读预览。
  - **曲线类型**：`ControlClip.interp: CurveInterp{Linear, Smooth}`（serde 加性字段，`PROJECT_VERSION` 不变）。`Linear` = 现状分段线性（freehand / polyline 共用）；`Smooth` = **单调三次 Hermite（PCHIP 风格）**——内部切线取相邻割线的加权调和平均、异号归零，端点取单侧割线，**不过冲**。model `sample_curve`（`Vec`）与 engine `RtAutomation::value_at`（定容数组）同构实现 + 镜像测试。
  - **timeline 预览**：内联可编辑带退役（删 `curve_band` / `CurvePoint` / `CurveRemove` / `CurveDragEnd` / `timeline_curve_point` / `timeline_curve_remove` / `TimelineState::curve_drag`）；`lane_row` 加 `clip_body` 回调，控制行在 **clip 块内**用 `draw_control_preview()` 采样 `glitcherz_model::sample_curve` 画只读曲线（与引擎同数学），不再多一行。
  - **Edit tab 曲线编辑器**：`edit_clip: Option<EditTarget::{Midi{..}, Control{..}}>`；双击控制 clip → `TimelineAction::OpenControlEditor{track, clip}`（取代 `ShowControlCurve`）→ `control_editor_body()`。画布显示**一个 loop 周期**；三工具 `CurveTool::{Draw, Line, Curve}`（Draw/Line 写 `Linear`，Curve 写 `Smooth`，`set_clip_interp` 一步 undo）；交互沿用 `curve_upsert`（drag 合并 undo）/ `curve_remove_point`；`curve_tool`/`curve_drag` 为 app 瞬态字段（`set_control_edit_target` 按 clip 的 `interp` 同步工具）。

### 命令集（轨道定向）

- `SetParam`（主链）/ `SetTrackParam{track}` / `Note{track}` / `Sequencer(Play|Stop|SetBpm|SetGate|SetPattern{track}|SetClip|SetSource|SetAutomation)`；模式整块 `Copy` 数组经队列传递。

### 多 clip 卷帘（P3）：subject / ghost / 焦点

**`RollSubject` 是"这一帧画什么"的唯一来源**：`{ focus: usize, context: Vec<ClipRef>, dropped: usize }`，`ClipRef { lane, track, clip }`。`focus` 是 `context` 的下标（不是独立的 clip 引用），这样"焦点必须在可见集合里"由类型结构保证，不需要额外断言。`single(track, clip)` 是退化构造，`assemble(focus, selection, clips)` 是 app 侧装配入口——`clips(track)` 回调用来判断引用是否已失效（选区活得比 clip 久）。

**ghost = 换一个 origin 的同一套几何**：`geom.at_origin(clip.start_ticks)` 之后直接复用 `note_rect`，颜色换成 `GHOST_NOTE`（`from_rgba_premultiplied(14,35,21,56)`，即 focus 色 22% alpha 的预乘值）。**它必须是 `const`**，而 `Color32::from_rgba_unmultiplied` 不是 const fn——所以写预乘值，注释里记原色。ghost 循环里没有"命中测试"代码可删，因为它压根没被喂进 `note_at`；测试 `a_ghost_note_is_painted_but_never_hit` 用"同坐标点 focus 音符仍能选中"做对照，防止坐标算错造成的假通过。

**`over_focus` 是 ghost 带来的新边界**：窗口现在是 context 跨度，指针可以落在 focus 之外。创建音符的 `snap(...).min(loop_ticks-1)` 会把窗外点击变成"在 tick 0 创建"，所以入口先判 `(0.0..loop_ticks).contains(&geom.local_at(x))`，窗外单击只清空选择 + 状态提示，窗外空拖不起框选（ghost 不可选，框选必然空手）。

**焦点切换是纯 view-state**：`set_roll_focus` 只改 `subject.focus` 并 `clear_gesture()`（顺带清 selection——索引属于旧 focus）。chip 条与 `Ctrl+Tab` 都在 `midi_editor_body` 里、`show()` 之前绘制，所以 `show` 不需要知道"焦点可能换人"。**`Ctrl+Tab` 用 `input_mut().consume_key` 消费**，否则 egui 的焦点遍历会先吃掉它；裸 `Tab` 自 2026-11-05 起归全应用视图切换（`MainView::cycle`），`handle_keyboard_input` 在卷帘持焦时跳过 `Ctrl+Tab` 让这里先拿。

**`EditTarget` 失去 `Copy`**：`Midi(RollSubject)` 带 `Vec`，`edit_panel` 改为 `self.edit_clip.clone()`。80 字节级 clone 每帧一次，比"拆成只读引用 + 内部再取一次 &mut self"容易读得多。

**track 重排要改全体**：`reorder_midi_track` 原来只重映射 focus 的 `track`，现在必须遍历 `context_mut()`——漏改会让 ghost 指向另一条轨的音符。

### Edit tab 跟随 timeline 选区（2026-10-28）

**Edit tab 没有自己的开关**：目标由 `edit_clip` 变成"从 `timeline_state.selection` 派生"（`sync_edit_target` 每帧在视图分发前跑一次）。双击不再是唯一入口，只是"选中 + 切到 Edit"的快捷方式；反过来，切走选区（点空白、Esc、删除、换选）就自动关掉编辑器。`× Close` 按钮因此退役——面板不再拥有自己的生命周期，空选区时它像 Inspector 一样给提示（"回 Timeline 选一个 clip"），只选中 audio clip 时明说音频 clip 没有编辑器。

**`edit_selection` 快照是必需的，不是缓存**：装配 subject 只在**选区变化**时发生（spec `roll-multiclip.md` E15 的约束）。没有快照就只能每帧重装，那会把 chip 条 / Tab 换来的 focus 抹回"最后选中的那个"。快照同时被 `set_edit_target` / `set_control_edit_target` 写，所以 ← → 翻 clip、双击打开这些不走选区的路径也不会触发下一次重装。

**谁当焦点 = 选区里最后一个"活着且可编辑"的 clip**（`lane != Audio` 且 `clips.get(clip)` 命中）：反向扫描等价于"最后点的那个"，与双击落点一致；存活性检查是因为选区活得比 clip/轨道久（删轨、undo 都会留下死索引），死索引不能进 subject。

**目标消失时不再回退到 clip 0**：`midi_editor_body` / `control_editor_body` 原来会把越界索引钳到 0，选区驱动的语义下那等于"悄悄换了 clip"。现在置空 `edit_clip` 交给下一帧的 `sync_edit_target` 重算——选区里还有活着的 clip 就换过去，全死了就回到提示态。

**行选中不再关编辑器**：`SelectTrack` / `SelectControlTrack` / `SelectAudioTrack` 原来顺手清 `edit_clip`，现在只改行高亮——轨道选中与 clip 选区是两件事，后者才是 Edit tab 的输入。`do_open_now` 额外清 `timeline_state.selection`（它索引的是**上一个**文档），`reorder_midi_track` 在重映射选区后同步快照。

### 音高通路（P1）：一个目标，两种投递

**曲线存半音，不存归一化**：`AutoParam::PitchBend { channel, range_semitones }` 的曲线值域是 `[-range, +range]` 半音（默认 ±2）。range 是**文档字段**而不是常量，因为同一个曲线在不同音源上可以是 ±2 的揉弦也可能是 ±12 的大滑音；存归一化再乘 range 会让"改了 range 曲线跟着变形"，存半音则改 range 只影响投递换算。`pitch_bend(channel, range)` 构造器钳制 `0..=15` / `0.5..=24`（NaN 回落 2.0），这与 MIDI 14 bit 的可表示精度对齐（±24 半音时 1 LSB ≈ 0.003 半音）。

**只有 `AutoDest::PitchBend` 一个目标，没有"内置源 PitchBend / 插件 PitchBend"两个**：spec 早期草案按声源类型分支，实测那样等于把"哪类节点吃什么"这张表搬到 RT 线程维护，节点自己最清楚。现在的投递顺序是：

```rust
if self.slot_midi_pitch_bend(route.slot, channel, bend) { return; }   // CLAP / SF2
let _ = self.slot_set_param(route.slot, ParamId::PitchOffset, value); // synth / sample
```

`slot_midi_pitch_bend` 只有实现了 `Processor::midi_pitch_bend` 的节点返回 true；内置合成器与采样播放器返回 false，于是自然落到逐样本参数。**VST3 两边都没有 = 静音失败**，这是有意的（VST3 侧要给逐样本音高需要 `ProcessData` 里的 `PitchBend` 事件，宿主层还没接，spec 记为"不支持"而非"假装支持"）。UI 悬停文案把这个差异直接写给用户看。

**14 bit 换算与去重**：`ratio = (value / range).clamp(-1, 1)`，正向 `×8191`、负向 `×8192`（MIDI 的 0..16383 中心在 8192，两侧不对称，用同一个 8191 会让 +1 全量偏差 1 LSB）。`last_bend: [i32; MAX_AUTO_TRACKS]` 与 `last_cc` 同构——同一 block 内重复值不重复下发，否则 16 分音符的平坦段每块发一次 bend，把插件的事件队列喂爆。

**逐样本平滑是必须的，块级会响**：曲线在拖拽时逐帧跳值，块级跳变在低频上就是 click。`SineProcessor` 用 self 携带的 `pitch_ratio/target/coeff`（`coeff_for(5.0ms, sr)`）逐样本 `ratio += (target - ratio) * coeff`，然后 `phase += (base_inc * ratio).clamp(0.0, 0.5)`——`clamp` 上界是 0.5（每样本半周期），防止极端弯音把相位增量推过 Nyquist。`SampleVoiceProcessor` 的难点是 16 个 voice 共享同一个 block 级平滑：**每块先填一条 `pitch_ramp: Box<[f32]>`，再让每个 voice 按 `voice.pos += voice.rate * ramp[f]` 读**，这样 16 个 voice 的滑音曲线完全一致，也避免每个 voice 各存一份状态（原先 voice 结构体是纯 POD，加了状态就得在 note-on 时重置，容易漏）。`render_voice` 因此涨到 8 个参数，`#[allow(clippy::too_many_arguments)]` + 注释说明比拆结构体更可读。

**测试用"半周期"验证而不是听感**：`pitch_offset_glides_within_one_block` 断言块内前半段与后半段的过零间隔比例趋近目标 ratio；`pitch_offset_moves_a_voice_that_is_already_playing`（`tests/sample_voice.rs`，配 `cursor_advance` helper）专门覆盖"note 已经开声之后再弯音"——这是最容易漏的路径，因为 ramp 只在 `process` 开头填，voice 若在块中途 pop 进来就会用到上一块的旧值。

### 曲线泳道（P4）与折叠（P5）

**卷帘不认识电缆**：`OverlayLane<'a> { clip: ClipRef, data: &'a ControlClip, axis: (f32, f32, bool), label: String }` 由 app 侧 `midi_editor_body` 组装——纵轴 `(min, max, symmetric)` 来自 `curve::axis(&param)`，`param` 来自 `control_track_param(project, track)`（按 `PortRef::AutoTrack` + `CableKind::Automation` 反查电缆）。卷帘只负责"给一个矩形，按这个值域画折线"。这样自动化目标换了、值域变了，卷帘一行不用改。

**时间轴复用而不是再推一遍**：每条 lane `allocate_exact_size` 拿到自己的 rect 后，`geom.at_origin(clip.start_ticks).over(rect)`——`over` 换掉 `area` 但保留 `origin/t0_x/ticks_to_x`。于是泳道与音符**逐像素对齐**，`a_lane_shares_the_time_axis_with_the_notes` 就是拿同一 tick 在两个 rect 里的 x 相减断言为 0。

**`ViewWindow` 要并上 lane 的跨度**：context 里可能有"没有音符但有一条长曲线"的 clip，只看音符会让窗口截掉曲线尾巴。`show` 里用 `fold(..., |w, lane| w.union(ViewWindow::of(...)))`，`union` 取 min/max。

**逐像素采样画折线**：不是"连锚点"，而是对矩形内每个 x 求 tick → `value_at(phase)` → y。原因是曲线是分段插值 + 可能循环，连锚点会在 loop 边界画出一条横跨全屏的假线。空曲线（`points` 空）时 `value_at` 返回 None，直接断开（不画 0 线）。`rem_euclid` 处理循环：`phase = (tick - clip.start) mod loop`。

**拖拽分类靠 `press_origin()`**：单击（加锚点）与拖动（移动锚点）在 `response.dragged()` 上无法区分——第一帧拖动就已经有 delta。判据是"按下时的位置是否落在某个锚点的 `ANCHOR_RADIUS` 内"：是则 `CurveUpsert { from: Some(tick), dragging: true }`，否则 `from: None`（新建）。松手发 `CurveDragEnd` → app 层 `end_param_edit()` 收口 undo 事务。这个 bug 在测试里表现得很典型：`dragging_a_lane_anchor_reports_the_anchor_it_moved` 最初收到 `from: Some(960)`（拖到的新位置）而不是原点。

**`lane_rects` 是布局的公开产物，测试用它而不是自己推**：测试 fixture 一开始自己按 `ROLL_HEIGHT + LANE_HEIGHT * i` 推算泳道 y，漏了 velocity 条，导致"点 (x, 0.0) 得到 0.25"。现在 `RollState.lane_rects: Vec<egui::Rect>` 每帧由 `show` 填充，`lane_pos()` 直接查。同理折叠：**点击处理发生在布局之后**，所以切换折叠状态当帧的 rect 还是旧的，`the_lane_toggle_collapses_the_lane` 必须多跑一帧才看到 18 px。

**折叠是 view-state，按 track 索引**：`collapsed_lanes: Vec<usize>` 存的是 control lane 的序号（= `lanes` 数组下标），不落文档——它是"我暂时不想看这条"的临时偏好，跟工程语义无关。折叠后高度 `CURVE_LANE_COLLAPSED_HEIGHT = 18.0`（够放一个小方块 + label），方块点击在 `curve_lanes` 里 `continue` 掉，不进入曲线交互。

### 卷帘坐标系绝对化（P2，spec `roll-multiclip.md`，2026-10-27）

**`RollGeom` 是唯一的坐标真相**：`{ origin_tick, t0_x, ticks_to_x, top_note, row_h, area }`，`x_of(local) = t0_x + (origin + local) * ticks_to_x`、`local_at(x)` 是其逆。绘制（音符/网格/播放头/modal ghost）、命中（`note_at`/`note_edges`）、刀切（`knife_crossing`）、框选、velocity 泳道全部走它。原来这些是 5–7 个参数的自由函数，每加一处绘制就复制一次公式；现在 `note_rect(&note)` 一个参数，P3 的 ghost 只是换 `origin_tick`。

**tick 仍全程 clip-local**：只有几何知道 origin。`pointer_tick` 变成 `geom.local_at(pos.x).max(0.0)`——钳制必须在**局部**坐标做，先钳绝对 tick 会让 clip 起点左侧的点击变成负的局部 tick。命令、模态、`clamped_move`、`selection_bbox`、`scale_max_k` 一行未改，这是"纯重构"能成立的原因。

**`scroll_x` 从局部 tick 变绝对 tick**：`ViewWindow { start, end }` 收口滚动钳制（原来三处在 zoom / wheel / middle-drag 里各写一遍 `clamp(0.0, loop - width/t_x)`）。P2 的窗口就是 `[clip.start, clip.start + loop]`，P3 换成 context 跨度即可。

**`RollState.view_origin` 是必需的，不是可选优化**：state 跨 target 存活，绝对 scroll 会让"打开一个从第 9 小节开始的 clip"继承上一个 clip 的 scroll（整个落在窗外）。origin 变化时重锚到自身起点，首帧 `None` 也走这条路径——这正是旧的"打开 clip 看到它开头"行为。

**等价性怎么保证**：`t0_x = rect.left() - scroll * t_x`，scroll = origin 时，音符 x = `t0_x + (origin + local) * t_x = rect.left() + local * t_x`，与改造前逐像素相同。37 项原测试全部通过（3 处直接调自由函数的改为调方法，断言未改），另加 `clips_with_different_origins_share_one_time_axis`（同一 `t0_x` 下两个 origin 的音符相隔一个 origin 的距离、逆映射各自正确）与 `the_scroll_window_is_the_clip_span`。

**fit 基准与 margin 留给 P3**：spec 原写"fit 改 context 跨度、钳制加 margin"，实测 `span_ticks ≥ loop_ticks`（活跃时长可长于内容），用 span 拟合会让单 clip 视图缩小，违反等价验收；P3 改用"context 内各 clip `start + loop` 的最大跨度"，单 clip 退化为现在的 `loop_ticks`。

### 全选与「拖出 clip」撤销修复（W3c，2026-11-05）

**Ctrl+A / Ctrl+Shift+A 分两层放**：

- **时间轴 = 视图级命令**（`timeline::show` 开头，和 `Shift+T` 平铺同一层级）。行级键盘 handler 在 `response.has_focus()` 里、每次只拿到本行那一张 clip 表，而"全选"定义上跨轨跨 lane，所以放在 `all_clips(midi, control, audio)`（纯函数，表序 MIDI → control → audio，空轨不产出）。`Ctrl+Shift+A` 先消费：`consume_key` 按逻辑修饰键匹配，窄的 `Ctrl+A` 模式会把 `Ctrl+Shift+A` 一起吃掉。内联改名（`clip_rename` / `rename` 的 `TextEdit`）持焦时整段跳过（`text_edit_focused()`），否则输入框里的全选会被抢。
- **卷帘 = 焦点 clip 的全部音符**（`RollState.selection` 是 focus clip 的下标表；ghost 不可编辑，不进选区），在 `Keys` 里多两个 `consume_key`，处理点放在"没有模态 / 没有拖拽"的 idle 分支，和 G/T/S/D 同族。

**「音符拖出 clip 外无法 Ctrl+Z」的两处根因**：

1. `clamped_move_deltas` 的右边界只钳 **start**（`loop_ticks - 1 - start`）。240 tick 的音符能被拖到 3839，尾巴挂在 4079 —— clip 只有 3840。这种状态引擎排不下、卷帘只画出半截，撤销/重做往返之后看到的位置和"拖出去"时不一样，读起来就是"撤销没反应"。改成钳**尾巴**：`room = (loop_ticks - note.length - note.start).max(0)`。`max(0)` 是防呆：已经超长的音符（被裁短的 clip 里遗留）不会被右拖反推到左边。拖拽与模态 ghost 共用同一个函数，所以预览和确认永远一致。
2. **残留的 drag 状态**：释放事件没送到卷帘时（在窗口外松手、远程桌面的 `PointerGone`、切窗口丢焦点），`state.drag` 还在，下一帧继续按指针重放 delta —— 正好把 Ctrl+Z 刚恢复的音符又推回去。`show()` 在拖拽处理之后加一道「指针没按下就丢弃 drag / marquee」的兜底。

**测试**：`ctrl_a_selects_every_note_and_ctrl_shift_a_clears`（卷帘，走真实 egui 帧）、`all_clips_covers_every_lane_in_table_order`（表序 + 空表）、`a_drag_out_of_the_clip_stops_at_the_tail_and_undoes_in_one_step`（拖到窗口外 → 尾巴仍在 clip 内 → 同一 gesture key → 一次 undo 回到 0）、`clamped_move_keeps_notes_inside_the_clip_and_pitch_range` 与模态 clamp 用例的期望值从 `loop-1` 改为 `loop-length`。

**标签顺序**回退成 `Timeline → Edit → Patch → Inspector`（`MainView::ORDER` 仍是唯一来源，标签栏 / `Tab` 循环 / `Alt+1..4` 自动跟随）；默认视图不变。`docs/manual.md`、`docs/shortcuts.md` 同步。

### UI 模块拆分（S3，2026-11-04）

`ui/src/lib.rs` 从 **16,421 行降到 3,383 行**：`impl GlitcherZApp` 的 229 个方法按面板搬到 14 个同级模块，`lib.rs` 只留结构体/常量、`new`、编辑守卫与 undo、`ui()`、自由函数和测试。

| 模块 | 行数 | 内容 |
| :--- | ---: | :--- |
| `engine_sync.rs` | 1282 | `apply_document` / `install_node` / 参数下发 / `sync_engine` / clip 表 |
| `patch_ops.rs` | 1384 | patch 画布交互、建节点、连线接线、删除 |
| `file_ops.rs` | 781 | 存/开/新建、MIDI 导入导出、自动保存、媒体收集与重定位、崩溃恢复 |
| `export_ops.rs` | 835 | 离线引擎装配、多 pass 导出、导出对话框 |
| `voices.rs` | 1032 | SoundFont / 采样 / 鼓机加载、音色预设、外部 FX、插件编辑器窗口 |
| `recording.rs` | 1035 | 录音取音与 take 收尾 |
| `timeline_ops.rs` | 1240 | 时间轴动作、clip 增删改、曲线点 |
| `inspector_panel.rs` | 1978 | Inspector 及各 voice / effect 子面板 |
| `mixer_panel.rs` | 897 | 混音台通道条与轨道增删 |
| `editing.rs` | 960 | 编辑目标、卷帘与曲线编辑器 |
| `transport.rs` | 580 | 走带条、master 段、跳转、视图 tab |
| `keyboard.rs` | 585 | 电脑键盘 / MIDI 输入、键盘节点面板 |
| `settings.rs` | 379 | Preferences、设备菜单、undo 历史窗口 |
| `status_bar.rs` | 211 | 状态栏、编辑工具栏、插件扫描条 |

**两条硬规则**：

1. 子模块里的方法一律 `pub(crate)`。私有方法在子模块里**对父模块不可见**（`E0624`：`method is private`），搬迁时第一版就是被这个拦下来的。
2. 模块顶部 `use super::*;` 拿结构体字段与 `lib.rs` 的自由函数（私有项对后代模块可见，反之不行）。新方法直接写进对应模块，不要再往 `lib.rs` 追加。

**做法**：一次性脚本按 `impl` 花括号配平（剥掉字符串与注释再计数）切条目、按名字抽取、保持原顺序，每批搬完 `cargo check`。纯位移：前后 980 项测试一致，`fmt` / `clippy -D warnings` 零 diff。

### 走带栏图标化 + Timeline In 输入口跟随轨道名（2026-11-06）

#### 1. Timeline In 的录音输入口显示轨道名

`Timeline In` 终端每个录音输入口原来画的是固定标号 `T1` / `C1` / `A1`（从端口序号算出来的），所以编曲视图改了轨道名，这个口纹丝不动——而同一张画布上的 timeline 输出口一直用的是**实时**轨道名，两种口对同一件事给出两种答案。

抽出 `timeline_in_port_label(kind, track, deps)`：走 `deps.track_name` 每帧读一次（与 outlet caption 同一个读取器），有名字就显示名字并套 `truncate_fullwidth` 的 6 全角字符上限，没名字才退回标号。终端宽度 150，端口圆心在左边缘，10 pt 间隔 + 60 pt 文字仍留有富余。

测试 `timeline_in_inputs_read_live_track_names_and_fall_back_to_tags`：改名后标签跟着变、超长截断、空名回落 `T1` / `C2` / `A3`。

#### 2. 走带栏改成图标优先

顺序按"播放 → 停止 → 回到开头 → 跳到结尾 ‖ 循环 → 上一个/下一个标记 ‖ 节拍器 ‖ 录音 + 录音选项"重排，`▶/⏸`、`⏹`、`⏺`、`🔁`、`♩` 全部只留图标，`Tempo` / `Meter` 两个标签删掉（`128 BPM` 与 `4/4` 自己会说话），`Latency` 标签也删掉（`0 smp` 的后缀已经带单位）。名称和快捷键全部搬进 `on_hover_text`：`Play (Space)`、`Pause (Space)`、`Return to start (Home)`、`Skip to end (End)`、`Previous marker (,)`、`Next marker (.)`、`Record … (R)`；没有快捷键的（stop / loop / metronome）就只写作用。

几个连带决定：

- **录音状态改用底色**。文字没了之后，"预备拍中 / 正在录"必须靠颜色：琥珀 = counting in，暗红 = capturing，红点本身也随状态变亮。tooltip 仍然逐字说明（预备拍那版还带上 `record_pre_roll_bars`）。
- **tempo 与 meter 合成一组**，中间不再插分隔符：标签一去掉，`128 BPM [4/4]` 本来就是同一件事的两个值，两个 separator 只会切碎视觉。
- **`⏹` 而不是原来的 `■`**。`■`(U+25A0) 来自 emoji-icon-font，和 `⏸ ⏮ ⏭` 的媒体控制块不是一个语族；换成 `⏹`(U+23F9) 之后八个走带图标全部落在 U+23xx / U+1F5xx，字重也一致。`♩`(U+2669) 同理（`♪` 留给时间轴上的音符合成标记，语义分开）。
- 拒绝卡片里的 `press ⏺ Rec again` 改成 `press ⏺ again`——按钮上已经没有 `Rec` 这个词了。

`fonts::UI_SYMBOLS` 补 `⏹ U+23F9`、`♩ U+2669`、`🔁 U+1F501`，回归测试保证这三枚在字体栈里真的有字形（`🔁` 需要 `monochrome_emoji_fonts`，见上一节）。

`tests/timeline_focus.rs` 的手搓走带条同步改成图标版，保持它跟真走带栏的形状一致。

### 图标间距收紧 + 手绘节拍器（2026-11-06）

图标化之后走带栏仍然比预期宽：一枚 14 pt 图标在 `Button` 里左右各 8 pt padding，再叠上 egui 0.37 默认的 8 pt `item_spacing.x`，两个字形的中心距是 30 pt——一行八个图标像八个不相干的按钮。新模块 `crates/glitcherz-ui/src/transport_icons.rs` 收口这两件事：

- `icon_button(ui, Button)`：只把 `button_padding.x` 压到 4（并立刻还原，别让后面的 DragValue / 复选盒继承），再给 `min_size.x = 22` 兜底，让不同字宽（`▶` 与 `‹`）步距一致。宽度 = 14 + 8 = 22，中心距 = 22 + 4 = 26。
- 走带栏整行 `item_spacing.x = 4.0`（默认 8）。分隔符因此从 17 pt 收到 9 pt，横向总量省下来的空间足够 Master 电平表伸展。

**节拍器不再用 `♩`**。八分音符是"音符"，不是节拍器——`♩` 放在录音按钮旁边会被读成"音符合成标记"（`♪` 已经占了这个语义），而 Unicode 里根本没有机械节拍器的码位。所以自己画：`metronome_button` 用 `allocate_exact_size(22 × interact_size.y)` 拿到和邻居完全一样的按钮矩形，用 `style().interact_selectable()` 取当前状态的 fill/stroke（悬停、按下、"开着"的选中底色因此全部跟着主题走），再用 `Painter` 画三笔——梯形箱体（一笔闭合 `painter.line`，拐角才会接上而不是四段线头）、斜的摆杆、摆杆上的配重横条。

`metronome_shape(rect)` 是纯几何函数，测试直接断言比例：顶宽 < 0.7 倍底宽、底宽 < 高（否则像口袋）、左右对称、摆杆在**自身高度处**的梯形半宽之内（不是笼统地塞进包围盒）、斜率 > 0.15（近乎垂直会被读成铃铛）。摆杆底端离箱底 10%、顶端离顶板 16%，让它不穿盖。

`fonts::UI_SYMBOLS` 因此删掉 `♩ U+2669`——白名单只留真正还在牌面上的符号。`docs/manual.md` 的走带栏那一行同步改成"节拍器按钮"。

### Master 组上走带条：立体声表 + Panic（U8 收尾，2026-10-23）

**为什么挪**：Inspector 的契约是"只显示选中节点的参数"，可它底部压着 `Master Gain` 与 `Master Out Level` —— 两个全局量。视觉上它们紧贴 `Cutoff Freq` / `Volume Envelope` 那组节点参数（同在一个 `Grid` 里），很容易被读成"master 的滤波/包络"。现在 master 组进 `transport_panel`（`master_section`），`Panel::top` 不管切到哪个 tab 都在，符合"走带条上的东西属于整首歌"。Inspector 侧只剩节点参数，并在 `Grid` 前加了一行 `Node Parameters — filter / envelope / delay of the node above, not the master bus`。

**立体声峰值链路**：`GraphLevels.master_lr: [f32; 2]`（新增 `slot_peak_lr`，逐声道扫；`master` 仍保留为两者的 max，历史单值调用点与测试不动）→ `Feedback.{level_l, level_r}` → `AudioEngine::master_peak_lr()` → 两个 `PeakMeter`（各 600 ms hold）。UI 侧自绘两条竖条：`-60..0 dB` 映射，`-9 dB` 以下绿、以上琥珀、`-1 dB` 以上红（红=过载是音频表惯例，与股票红绿无关）。用 `ProgressBar` 做不到——它只有一条且是横向的。

**Panic 为什么不发 `Off { note: 0 }`**：`note_off_all` 的毯式 off 到不了按 pitch 匹配的插件（Dexed 的挂音问题已记在 SF2/CLAP 两节）。真实释放要逐 pitch 走 0..127，但 **CLAP / VST3 两个宿主的 pending 队列都只有 32 条**（`MAX_PENDING_EVENTS`），一个 block 内塞 128 条会静默丢事件。所以 `RtGraph::panic()` 只设 `panic_sweep = Some(0)` 游标，`process_block` 开头每块发 `PANIC_SWEEP_BATCH = 24` 个 pitch 再推进（24 而非 32，给同 block 的常规事件留余量）：128/24 = 6 块，48 kHz / 128 frames 下约 16 ms。`EngineCommand::Panic` 额外先 `release_pending_notes()`（按 pitch 释放音序器已取走的待关音符）。**语义是静音不是停止**：transport 继续跑、参数一个不动，`--seconds` 冒烟与 `panic_sweeps_every_pitch_across_bounded_batches`（断言 128 个不同 pitch 全部到达）锁定。

**验收坑**：master gain 的 `Slider` 用 `custom_formatter` 显示 dB（线性 0..1.5 → `-∞..+3.5 dB`）；拖动仍走 `begin_param_edit` / `commit_param_edit("master-gain")` / `drag_stopped` 三步，否则一次拖动会塞满 undo 栈，或者每帧写文档让标题永远带 `*`。

### B2：片段临时静音（clip mute，2026-11-10）

spec `timeline.md` D31。需求原话是「ctrl 中键切换 mute、mute 之后淡显，外加 h 隐藏 / alt h 显示的 blender 键位」。

**静音是调度层的，不是增益层的。** `muted` 存在 clip 上（三类 clip 各一个 `bool`，serde `default` + 假值不序列化，`PROJECT_VERSION` 不动），一路带到引擎的三个 RT 结构（`RtMidiClip` / `RtControlClip` / `RtAudioClip`）。这样 clip 的表索引与文档完全对齐（`MAX_TRACK_CLIPS` 的槽位语义依赖这一点），而「不发声」只有三处判定：`schedule_track_clips` 的 `continue`、`RtControlClip::value_at_absolute` 的 `None`、`AudioTrackProcessor::process` 的 `filter`。**没有新命令、没有新的队列载荷**——静音跟着每次既有 push 走，改一次 = 重推被触及的轨道表。

**span 不动是有意的。** 最省事的实现是在 `headless` 的搬运层把 muted clip 换成 `RtMidiClip::default()`（span 0 即不调度），但那样 `refresh_arrangement_end` 也会跳过它：整首曲子静音后 arrangement end 变 0（= 无界），transport 从此不再自动停。静音在 DAW 里不该改变曲长，所以 RT 侧保留 span、只加标志位。控制轨同理：静音 = 不投递，目标保持最后值，与「clip 之外」完全一致（D17 的既有语义，不需要新规则）。

**中键的冲突要先解决。** middle drag 是平移（在 `show()` 里读上一帧的行几何、直接推 ScrollArea offset），所以 `Ctrl` + 中键必须在**两处**同时让路：平移那段加 `!ctrl`，否则「点一下切 mute」会顺带把视图推走；`lane_row` 里用 `response.middle_clicked()` 判定，只在 `Ctrl` 按下时动作。撤销/重做只记一步（`timeline_set_clip_muted` 先只读地判断有没有真的要改，没有就不 `project_mut`、不记 undo、不推引擎——`push_undo` 只比 `Arc::ptr_eq`，空 `project_mut` 也会留下一个假的历史步）。

**`H` 是虚拟钢琴键位**（FL 布局的 KEYMAP 里 `H` = 半音 9），所以它必须和 `G` / `T` / `K` 一样焦点门控：`show()` 顶部用 `state.owns_keyboard_focus(ctx)`。`Alt+H` 是「恢复全编曲」，需要跨三张表，于是放在 `show()` 的视图级命令区（与 `Ctrl+A` 同一处），而不是 `lane_row` 里——行级 handler 看不见别的轨道。`take_mute_shortcut` 照 `take_tile_shortcut` 的写法同时读事件的 modifier 快照与当前持有态（winit 的 modifier 会晚一帧）。回执也走视图自己的 status（「N clips muted」/「Alt+H: no muted clips」），跟 Ctrl+A 的计数提示一致。

**淡显是一层遮罩，不是逐个调色。** clip 块上有缝线、波形、曲线预览、组色条四层绘制，逐个降 alpha 会漏。做法是这些画完之后压一层 lane 底色的半透明矩形（alpha 165），再画选中/编辑描边（保持全强度：静音不能把"选中"也吃掉），最后是降透明度的标签和右上角 `M` 角标（窄块没有标签时仍可认出静音）。选择态与 modal 变暗是两个独立效果，可以叠加。

**测试**：`timeline_mute.rs` 两条集成用例驱动真实 egui 帧（Ctrl+中键切换、裸中键不切换、`H` 静音选区、`Alt+H` 全恢复且未静音时为零动作）；引擎三条（muted clip 不排任何事件但 arrangement end 不变、un-mute 后同一份表恢复发声；control clip 不投递、span 仍在；audio clip 静音/恢复）；model 一条（未静音不写键、静音往返，三张表都覆盖）。

## 节点图与面板

### mixer 通道条上画布（U14，spec `node-graph.md` D22，2026-10-28）

- **一行一个通道，压在端口上**：绘制段在 inline 参数循环之后（后注册 → 指针优先于节点拖拽），位置**由 `in_ports_screen` 的端口坐标推导**（`channel_strip_rect(port_pos, box, zoom)`），而不是另起一套均分——端口位置一改，条自动跟随，不会出现"推子飘在两根线中间"。条高 22 < `PORT_SPACING` 30，所以 2 通道（`port_area` 只有 48 图空间、step 24）时上下仍各留 1 px。
- **左边界必须是 22，不是端口半径 6**：拉线的判定半径是 `PORT_HIT_RADIUS`（18），条从端口圆点右侧 22 起才开始，否则按 fader 会变成拉线。右边界 = 盒右 − `INLINE_INSET`(8)；150 宽的盒子里 fader 76 + M/S 各 18 刚好，**所以没有 pan**。
- **`egui::Button` 又一次不可用**：M/S chip 只有 `22 × zoom` 高，原生按钮 18 pt 下限会顶出条外——与 D15 的 `action_button` 同一病因，故 `channel_toggle` 自绘（fill + stroke + 居中文字，字号 `min(9z, 高×0.8)`），命中靠 `allocate_rect(Sense::click())`。激活色：mute 红 (190,85,70)、solo 黄 (205,175,65)，文字用深灰（20）保证在彩底上可读。
- **fader 命中区取整条高度**：rail 只有 3 pt 高，直接 `allocate_rect(rail)` 抓不住；命中矩形用 rail 的 x 范围 × strip 的 y 范围，与 inline 滑条的 `hit` 同一思路。
- **写入复用既有三条通道**：`InlineParam::{MixerGain,MixerMute,MixerSolo}(ch)` → `inline_doc_value`（读 `channels[ch]`，索引越界返回 0 而非 panic——通道删除与绘制可能差一帧）→ `PatchAction::SetInlineParam` → `apply_inline_param`。fader 用 `commit_param_edit`，**merge key 必须带通道号**（`mixer-gain-{ch}`），否则连续拖两个通道的推子会合并成一步 undo；M/S 是离散点击，`begin_edit`/`commit_edit` 各一步。
- **同步是结构性的，不是同步代码**：两侧都读写 `NodeDoc::Mixer.channels`，引擎侧都发 `ParamId::MixerGain/Mute/Solo(ch)`（`set_node_param`），所以"patch 改完 Inspector 显示旧值"这类漂移不可能发生——不需要任何额外的一致性逻辑。

### patch 节点内联操作行 + 双击路由（U10，2026-10-23）

- **几何单一来源**：`size_of` 为 Timeline（2 行）与 Mixer（1 行）预留 `NODE_TITLE_BAND + action_rows * ACTION_ROW_HEIGHT` 的头部带；端口位置改由 `port_area()` 计算——上界在头部带之下、下界再减去内联滑块行（`inline_row_count * INLINE_ROW_HEIGHT * zoom`）。`out_ports_screen` / `in_ports_screen` 同时供**绘制与命中测试**使用，所以只改一处就保证"看到的位置"与"能拖到的位置"一致；漏掉下界会让 mixer 最后一条端口漂到 `Out Pan` 滑块上。
- **按钮要自绘，不能 `egui::Button`**（2026-10-28 修正，spec `sf2-loader.md` D15）：`egui::Button` 的最小高度是 `spacing.interact_size.y`（18 pt），而操作行在屏幕上只有 `18 × zoom` 高——0.6–1.0 缩放区间按钮比行还高，被居中后顶出去盖住节点标题（缩到 0.6 以下整行消失，所以只在中间区间露出）。改为 `action_button`：`painter` 自绘底+描边+截断文字（`LayoutJob` + `text::TextWrapping::truncate_at_width`）、字号按行高封顶、painter 裁到按钮矩形，再用 `ui.allocate_rect(rect, Sense::click())` 注册命中。禁用态仍保留 hover 文案（"为什么点不了"）。几何集中到 `action_row_band` / `action_button_rects`，回归测试 `action_buttons_stay_inside_their_band_at_any_zoom` 在 0.6 / 0.75 / 1.0 / 1.75 四个缩放点断言按钮不出带、不出盒。旧实现（`scope_builder` + `add_enabled` + 按 zoom 缩 `button_padding`）解决的是"禁用态不可表达"，新实现两者兼顾。
- **zoom 门槛**：`ACTION_MIN_ZOOM = 0.6` 以下不画按钮（缩略视图里按钮只有十几个像素，点了只会误触）。
- **动作全转发既有实现**：`add_midi_track` / `remove_last_midi_track` / `add_control_track` / `remove_control_track` / `mixer_add_channel` / `mixer_remove_channel`，undo 粒度与引擎同步沿用原路径。`- Slot` 的禁用条件抄 `mixer_remove_channel` 的拒绝条件（最后一槽仍有音频线），把"点了没反应"提前说清。
- **双击路由要门控**：`opens_inspector(node)` 只放行 Instrument / Effect / Mixer。Inspector 在没有编辑目标时会 fallback 到"当前轨的乐器"，所以让 VK / Feedback / ChannelStrip 也能跳过去 = 显示别人的参数，比不反应更糟。Timeline 跳 `MainView::Timeline`，Master（参数在走带条）与 Timeline In（纯汇）不响应。

### 节点参数自动化词汇表（U4c / A，2026-10-28）

**缺的从来不是通路，是词汇表。** 引擎的自动化链路 `set_auto_route(index, node_id, ParamId)` → `auto_deliver` → `slot_set_param` 一直能写任意节点的任意 `ParamId`（`graph.rs` 里 `auto_route_delivers_curve_values_to_target_param` 就是 mixer 通道 gain 的端到端断言）。用户看到"gain 收不到控制"，是因为 `AutoParam` 只有 7 个变体 + `accepts()` 只认乐器 + `auto_options` 只出 Cutoff/插件参数 —— 菜单里压根没有 gain 这一项。

实现要点：

- **地址进 model，翻译在 UI**：`AutoParam::Node(NodeParam)`，`NodeParam` 覆盖 mixer 通道/输出级、效果器 slot、Feedback/SF2/Sample gain、resonance 与 ADSR。engine 的 `ParamId` 仍只由 `lib.rs::node_param_id` 单点翻译（model 不依赖 engine，沿用原分层）。加性变体 → `PROJECT_VERSION` 不变，旧工程零迁移。
- **两个菜单合一**：`node_auto_options(doc, node_id, clap, vst3)` 是 patch 弹窗与 Inspector Target 行的唯一枚举来源。以前两处各写一遍，正是"mixer 通道从没进过列表"的原因。`plugin_params_of` 负责乐器/效果器的参数表分家（`clap_voice_params` vs `clap_effect_params`），顺带把内置效果器的 descriptor slot 补进 Inspector 菜单（原先只有 CLAP/VST3 效果器）。
- **曲线轴需要节点上下文**：`curve::axis(&AutoParam)` 保持上下文无关（各参数对齐面板 range），但效果器 slot 的真实范围在节点 descriptor 里 → 新增 `axis_in(param, doc, target)`，三个绘制点（Edit tab 曲线编辑器、卷帘 overlay、timeline 只读预览）全部改走它。timeline 的 `TimelineParams::curve_param: Option<&AutoParam>` 因此换成 `curve_axis: (f32, f32, bool)` 元组（预览只需要轴，不需要参数身份）。
- **只校验 mixer 家族**：`check_node_param` 强校验 `channel < inputs` 与 `MixerOut*` 必须在 mixer 上 —— 通道索引是文档能主动作废的唯一参数身份（删通道）。resonance/SF2 gain 这类与 voice 绑定的参数不校验：换 voice 会让老工程保存失败，而引擎对不认识的参数本来就返回 `false`。
- **删通道要在同一步收尾**：`mixer_remove_channel` 除了 `inputs -= 1` + `channels.pop()`，还要 `retain` 掉指向该通道的 automation 电缆并 `clear_auto_route(index)`，否则下次保存被校验拒绝、引擎也会继续往闲置通道写值。

**已知边界**：参数仍是块边界写入（≤2.7 ms）。内置效果器有自己的 `Smoothed`，但 **mixer 通道权重是块内常量**（`input_weights()` 每块读一次，`sum_into_weighted` 逐样本乘同一权重），所以 LFO 形态的 gain 曲线会有 375 Hz 台阶；推子慢推听不出来。块内线性斜坡是 A2 的后续项：`Processor` 增加"上一块权重"接口 + 求和循环在 `from`/`to` 之间插值，代价是 mixer 求和数学测试的期望值要跟着改成"块内均值"。

### Inspector 面板分家：合成器参数只属于内置合成器（2026-11-05）

**症状**：选中鼓组 / 采样 / SoundFont / 插件乐器节点时，Inspector 下半部照样画出 `Waveform` / `Cutoff Freq` / `Resonance` / `Filter Mode` / `Volume Envelope` 一整块 `Node Parameters`，拖动滑块文档字段会变、undo 也记，但**一个采样都不变**（用户报障）。

**根因**：那块网格用的是 `match (&target_doc, target) { (Some(NodeDoc::Instrument { .. }), ..) }`——只判断"这是个乐器节点"，没判断"这个乐器是不是内置合成器"。而 `engine_sync` 建图时是按 voice 分派的（`VoiceSpec::Builtin => SynthProcessor`，其余走采样播放器 / 鼓机 / SF2 / 插件），`synth_param_values(synth)` 也只在 `Builtin` 分支被推送。所以非内置 voice 的 `SynthParams` 是**只写不读**的死字段。画布上的内联行（`patch.rs::inline_params_of`）早就按 voice 分了家，缺的只有 Inspector。

**修法三层**：

1. `lib.rs` 新增 `voice_has_synth_params(&VoiceSpec) -> bool`（`matches!(voice, VoiceSpec::Builtin)`），与 `panel_shows_track_extras` 并列，成为"There be synths"的唯一判据。
2. Inspector 用 `synth_target` 取代原来的 `instr_target`：voice 不是 `Builtin` 就是 `None`。`Node Parameters` 标题与整个网格都跟着它——鼓组的 Inspector 现在只剩鼓槽表 + 输出 gain，采样只有路径/根音/播放模式/gain，SF2 只有文件/预设/gain。
3. `engine_sync::synth_of_node` / `synth_mut_node` 也加同一道守卫，非内置 voice 返回 `None`。原来它们对任何 `NodeDoc::Instrument` 都返回 `Some`，等于给调用方一个"写了也不生效"的引用；`adsr_sliders` 与 `apply_inline_param` 都走这条路，守卫让语义从"能写但没用"变成"没有这个概念"。

**测试**：`panel_target_tests::synth_params_belong_to_the_builtin_voice_only`（`Builtin` 为真；Sample / DrumKit / SoundFont / Clap / Vst3 逐一为假）。

**未改动**：引擎侧 `SynthParams` 字段保留（工程文件向前兼容，切换 voice 时旧字段仍在文档里），只是 UI 不再假装它们起作用。

### Inspector 收口：改名、mixer 分派、编辑器折叠头（U8，2026-10-23）

**改名**：`MainView::Synth` → `Inspector`，`synth_panel` → `inspector_panel`，tab 文案 `Synth / Mixer` → `Inspector`。面板标题不再带通道前缀（`Synth — Instrument #3` → `Instrument #3`），因为面板身份已由 tab 表明。

**面板只剩"选中节点的参数"**：三处结构管理搬走——① mixer 控制台改 `mixer_section(ui, mixer_doc)`，只有 `target_doc` 是 `NodeDoc::Mixer` 时才画（原先无论选什么都会画一遍"first mixer"）；② 混音台头部的 `+ Track` / `- Track` 删除（timeline 工具栏本来就有同名按钮，且那是轨道管理不是混音参数）；③ `automation_section` 拆解：`automation_strip`（新增，timeline 底部 `egui::Panel::bottom("glitcherz_automation")`）+ `add_control_track` / `remove_control_track` + `auto_target_row`（端点选择器，原样搬过来，`index` 改成入参）+ 删除 `clip_section` 死文案。曲线管理是轨道级状态，跟轨道放一起。

**2026-11-03 更新**：`automation_strip` / `auto_target_row` 已删除——`+/- Curve` 由 timeline 工具栏与 patch Timeline 节点承担，端点由节点图连线选择。随之移除的还有 CC 通道号与 Pitch Bend range 的内联微调；后续若要恢复，应加到节点图的连线弹窗里，而不是再加一条底部条。`Panel::bottom` 的声明顺序说明（见下段）随 strip 删除失效。

**`Panel::bottom` 的声明顺序**：必须写在 `timeline::show` **之前**（面板先占位，再让内容吃剩余高度）；`timeline::show` 内部的 ScrollArea 是 `auto_shrink(false)`，放后面会把 strip 挤出屏幕。

**插件编辑器改折叠头**（原来 `Show Editor` / `Close Editor` 按钮，展开后原生子窗口会盖住按钮）：

- 用 `CollapsingHeader::new("Editor").id_salt("plugin_editor_header")`，三角独占一行在占位矩形上方，原生窗口只能覆盖 body 的 rect，永远压不到开关。
- **`.open(Some(self.editor_user_open))`**：让 app 的 `editor_user_open` 成为唯一真值。若交给 egui memory 自己记，插件自毁 GUI（`gui_closed()` → 清 flag）那条路径会把 header 留在展开态，下一帧"header 开着但 flag 是 false"就把它又拉起来——死循环。强制 open 后点击不再自动 toggle，改由 `header_response.clicked()` 手动翻转 flag（`close_plugin_editor()` / `plugin_editor_wants_open = true`）+ `request_repaint()`，下一帧 header 跟上。
- body 只在展开时布局：会话可能比点击晚一帧（`plugin_editor_wants_open` 在本帧尾部才被消费），因此 body 里用 `if let Some(editor) = ... && Some(editor.doc_node) == target` 兜住 `None`，不画占位文字（避免一帧闪烁）。
- 折叠后立刻 `close_plugin_editor()`，不存在"隐藏但仍然加载"的中间态；`editor.rect` 留旧值无害（会话已销毁）。

## 插件宿主

### CLAP 宿主（`glitcherz-clap-host`）

- **参数枚举**：`ClapInstance::list_params()` → `raw::enumerate_params`——直接经 `clap_plugin.get_extension` 取 `clap_plugin_params` vtable 指针直调（clack 0.2 的 `PluginParams` 包装无读取方法，raw 模块为唯一 unsafe 边界）；返回 `ClapParamInfo{id, name, module, min/max/default}`。测试插件实现一个可自动化 Amplitude 参数（ID 1，0..1，默认 0.5，f64 位模式原子共享），参数事件驱动渲染振幅。
- **UI 接入**：`clap_voice_params`（轨道 → 参数快照）在装填 CLAP 声源时填入；参数面板 CollapsingHeader 内滑块直接调参（当前值存 `clap_voice_values`（UI 会话内，track → param id → f64），变更经 `send_track_param(ParamId::PluginParam)` 下发，块粒度下一 process 生效；重载插件/切回内置/文档重应用时重置为插件默认值；state 持久化落地后，重载路径经 `load_state` + `read_param_values` 重播种滑块缓存）；自动化目标选择器合并 CLAP（曲线值 = 插件值域原始值）与 VST3（曲线值 = 归一化 0..1）两份列表，并含外挂效果器参数（端点 = 效果器节点）。
- **真实插件兼容性复现**：`dexed_instrument_note_on_produces_audio`（clap-host 测试）——标准扫描目录发现 `dexed*.clap` 时实例化 + NoteOn + 16 块内断言非零输出，找不到则跳过；用于把"宿主桥接问题"与"插件状态/默认补丁问题"（宿主无 gui/state 扩展，Dexed 跑在内部默认补丁上）分开定位。

### 插件状态持久化（CLAP state extension，2026-10-15，spec 见 `.agents/spec/plugin-state.md`）

- **宿主侧**：workspace `clack-extensions` features 加 `"state"`（clack 0.2 的 `PluginState` 包装**有**完整 save/load 方法，与 params 不同、无需裸调 vtable）。`GlitcherZHostShared.state_ext: OnceLock<Option<PluginState>>` 在 `initializing()` 经 `InitializingPluginHandle::get_extension` 捕获，spawn 后 `access_shared_handler` 读出存进 `ClapInstanceGuard`；`GlitcherZHostMainThread` 实现 `HostStateImpl`（mark_dirty → no-op，采集时机见 D3）。
- **API**：`ClapInstanceGuard::{save_state() -> Vec<u8>, load_state(&[u8])}`（main thread，错误变体 `NoState/StateSave/StateLoad`）；`ClapInstance::read_param_values()` → `HashMap<u32, f64>`——raw vtable `get_value`（复用 enumerate 的扩展抓取模式），供状态加载后刷新滑块缓存。
- **数据落点（D1/D2）**：`VoiceSpec::Clap/Vst3` 各加 `state: Vec<u8>`（`#[serde(default, skip_serializing_if = "Vec::is_empty")]`）——旧文件无感，`PROJECT_VERSION` 不动。**注意**：VoiceSpec 是内部 tag 枚举（`type: "clap"` snake_case），RON legacy 形态测试别写成 `(Clap: ...)`。
- **数据流（D3/D4）**：保存 = `do_save` → `capture_plugin_states()`（engine_id 经 `node_map` 反查 doc_id，两段式：guard 循环采集 blobs → `project_mut()` 写文档；单实例失败 log + 跳过，D7）→ 序列化；恢复 = `install_node` spawn 成功后 `state` 非空即 `load_state` + `read_param_values` 回填 `clap_voice_values`。面板换声源（`load_clap_voice`/构造点）产 `state: Vec::new()`；切内置声源状态随 `VoiceSpec::Builtin` 替换自然丢弃（换声源动作 E5 后不存在，构造点统一走 `create_patch_node_on`）。undo/redo 经 `apply_document → install_node` 走同一恢复路径（但状态 blob 本身不进 undo 图，D5——保存前 undo 回退到旧快照时 state 为旧值）。
- **测试**：clap-host `state_roundtrip_restores_amplitude`（设参 0.25 → save（8 字节）→ 新实例 load → get_value 读回 0.25，新实例先验证默认 0.5）；model `voice_spec_state_serde_compatibility`（空 state 不序列化 + legacy 无 state 字段可加载）。
- **VST3**（S5，2026-10-15）：`HostMemoryStream`（`IBStream` 内存流 COM 对象，控制线程专用；读/写/seek/tell 全实现）+ `Vst3InstanceGuard::{save_state, load_state, read_param_values}`——save = 组件 `getState` 写入内存流后整块取出；load = 组件 `setState` + 控制器 `setComponentState` 同步（**两个消费者各一条独立流**：ComRef 不走 SmartPtr 毯子实现，无 vtable 透传 seek，直接免掉 rewind）；save 时插件保持活跃（与 CLAP 镜像一致的简化宿主姿态，Dexed 实测兼容；规范最稳妥顺序是先于 setActive，留待生命周期重构收紧）；测试插件工厂改**逐实例创建**（原 OnceLock 单例与 kManyInstances 相悖且测不了"新实例回落默认值"），SineComponent 以 8 字节 LE f64 经 `getState/setState` 持久化 Gain（`IParameterChanges` 逐队列消费、末点胜出、作用于振幅），控制器镜像读回 = D6 滑块刷新载体（归一化滑块 UI 待接入，`read_param_values` 已预留）。

### CLAP 宿主（`glitcherz-clap-host`，路线图第 9 步）

- `glitcherz-test-plugin`（cdylib）：clack-plugin 单音正弦乐器 fixture（`org.glitcherz.sine`，instrument+stereo 特性），供宿主端到端测试；
- 宿主 crate（唯一允许 unsafe 的边界 = `raw::load_entry`，模块级 SAFETY 文档 + 无效文件测试）：`clap_search_paths` / `scan_clap_files`（标准目录 + `GLITCHERZ_CLAP_PATH`）/ `list_clap_plugins`（描述枚举 + instrument 特性识别）；`ClapInstance::spawn_rt(path, id, config)` 控制线程加载 → 实例化 → 激活，产出守卫 `ClapInstanceGuard`（持有动态库 + 实例，必须与节点同寿命）+ `ClapNode`；
- `ClapNode` 实现 `glitcherz-engine::realtime::Processor`：音符/参数经定容事件槽（`MAX_PENDING_EVENTS=32`）暂存，每块以 `NoteOn/OffEvent` / `ParamValueEvent` 发出（块粒度）；`ParamId::PluginParam(u32)` 已加入参数空间（自动化桥接的落点）；音频经构造期预分配的 `AudioPorts` + 立体声临时缓冲渲染回 `AudioBuffer`；首块自动 `start_processing`，任何失败输出静音（无 panic）；处理用统一 `PluginAudioProcessor`（`as_started_mut`/`start_processing` 运行时切换）；
- 分层决定：**engine 保持零 CLAP 依赖**——clap-host 依赖 engine（实现其 Processor trait）。

### VST3 宿主（`glitcherz-vst3-host`）

- 架构镜像 `glitcherz-clap-host`：扫描（单文件 / bundle 目录，标准目录 + `GLITCHERZ_VST3_PATH`）→ 非实时线程实例化（IComponent + IEditController + IConnectionPoint 互连 + 总线激活）→ `Vst3Node`；
- **参数语义差异**：VST3 参数以**归一化 0..1** 经 `IParameterChanges` 传递（CLAP 是原始值）——自动化求值器天然桥接；UI 层滑块直接工作在归一化域（plain↔normalized 换算需逐参调控制器，v1 不做，见「插件参数桥接收尾」节）；
- 乐器渲染到节点内预分配立体声缓冲；效果器自动接通输入通路（引擎缓冲 → 独立输入缓冲 → 插件 → 独立输出缓冲 → 拷回引擎缓冲），kSample32 单精度；
- 生命周期：`Vst3InstanceGuard` 持有动态库与全部 COM 对象，**必须与 `Vst3Node` 同寿命且晚于其释放**；Node 释放时 `setProcessing(false)`；Guard 释放时 `setActive(false)` + `terminate()` + 断开连接，最后才卸载动态库；
- 已集成：`VoiceSpec::Vst3{path, class_id}` 进 model 与项目序列化；UI 守卫注册表 + 乐器/效果器懒扫描缓存 + 每轨参数列表。

### 插件参数桥接收尾（效果器面板 + VST3 归一化滑块，2026-10-15）

- **效果器参数寻址**：遗留任务标注的「区分声源/效果器参数空间」由**图节点寻址**自然消解——效果器参数经 `AudioEngine::set_node_param(engine_id, ParamId::PluginParam(id), value)` 直达节点自身的 `set_param`（ClapNode/Vst3Node 早已实现），无需新的参数空间。
- **缓存键位**：效果器参数缓存（`clap_effect_params` / `clap_effect_values` / `vst3_effect_params` / `vst3_effect_values`）以**文档 node_id** 为键（跨引擎重建稳定；下发时经 `node_map` 反查 engine id），乐器缓存沿用 track 键。填充路径两条：live 插入（`load_clap_effect` / `load_vst3_effect`，`insert_effect_common` 改回 `Option<doc_id>`）、文档重建（`install_node` 的 Effect 两分支）；**E5 删掉了 live 插入这条，只剩文档重建**；清理路径三处：删除节点、`clear_external_effects`、`apply_document` 整批 clear。
- **域约定**：CLAP 滑块与自动化曲线 = 插件原始值域（min/max 来自参数枚举）；VST3 滑块与自动化曲线 = 归一化 0..1（`ParameterInfo` 不携带 plain 值域，plain↔normalized 换算需逐参调控制器，v1 明确不做——归一化滑块诚实且跨插件一致）。VST3 当前值经 S5 的 `read_param_values`（控制器 `getParamNormalized` 读回）播种会话缓存 `vst3_voice_values`（乐器）/ `vst3_effect_values`（效果器）。
- **自动化目标**：`track_external_fx(track)` 辅助统一产出效果器清单（doc_id + 显示标签 + 格式），轨道面板与自动化选择器共用；效果器参数作为自动化端点落在效果器节点上（`AutoParam::PluginParam` 域随格式）。

### 实测反馈修订（大参数量插件：Dexed CLAP 156 / VST3 2238 参数，2026-10-15）

- **扫描列表重复**：每个插件出现两次（Dexed / xstudio bridge / plugdata）——根因是扫描器枚举了工厂里全部类而**未按 category 过滤**：组件的 category 是 `Audio Module Class`，编辑控制器是 `Component Controller Class` 且常与组件同名。修复：`fallback_class_info` 只放行 `Audio Module Class`（控制器本就该经 `getControllerClassId` 定位，不靠枚举发现）；测试插件工厂补枚举控制器类（category 用规范值）钉住回归（`scanner_filters_out_controller_classes`）。
- **面板卡顿**：每帧渲染全部滑块（2238 个）+ 每帧克隆全部参数结构体。修复：乐器参数面板改**虚拟化行**（`ScrollArea::show_rows`，只布局可见行）+ 过滤框（`clap_param_filter` / `vst3_param_filter`）；取值用**下标索引 + 延迟提交**模式——闭包内只读参数表、把变更收集进局部 `Vec`，出闭包后再写缓存/发参数（既避免每帧 clone 参数 Vec，又绕开「闭包内读参数表 + 写缓存」的借用冲突）。CLAP 面板按 `module` 路径稳定排序并显示 `module / name`（Dexed 的 OP1/OP2 分组可读）；FX 面板包 ScrollArea；自动化目标菜单同样过滤 + 虚拟化（`auto_param_filter`）。
- **跨格式残留**：VST3 → CLAP 切换后 VST3 面板残留——`load_clap_voice` / `load_vst3_voice` 互删对方格式的面板缓存（`voice_params` + `vst3_voice_values` ↔ `clap_voice_params` + `clap_voice_values`）——E5 后没有切换，这段清理随函数一起删除。
- **GUI 缺口确认**：宿主无 CLAP gui 扩展 / VST3 `IPlugView`（无头运行），已入 plan.md 遗留项（需独立 spec）。

### 乐器预设系统 v1（2026-10-15）

- **存储**：`presets/<插件键>/<名字>.glzpreset`，文件内容 = **原始状态字节**（不包 host 信封——blob 格式本就插件私有，文件名即预设名）。插件键 `preset_plugin_key(format, path, id)` = `<format>-<路径 hash8>-<id 片段>`：路径 hash 让不同安装的同名插件互不串库，id 片段经 `sanitize_filename_fragment`（64/24 字符上限，非法字符 → `_`）。库根 = 工作目录下 `presets/`（dev 运行即仓库根）。
- **加载/重置 = 可撤销文档编辑**：顺序是先活体 `guard.load_state(&blob)`（插件拒收 → 报错返回，文档不动）→ `refresh_voice_slider_cache`（CLAP 守卫新增 `read_param_values`——原方法在 `ClapInstance` 上，守卫注册后 UI 够不着）→ `begin_edit` 写 `VoiceSpec.state` → `commit_edit("Load Preset")`。undo/redo 走 apply_document 重装即自然回滚。
- **出厂默认快照**：`voice_default_state`（track → blob）在 spawn 时抓取——**必须先于文档 state 恢复**（四条路径：install_node 乐器两分支 + `load_clap_voice`/`load_vst3_voice`；E5 后只剩前两条）。Reset = 活体加载快照 + 滑块回填 + 文档 state 置空（空 state = 重载即出厂默认，与 D4 语义一致）。
- **Save As = 文件操作**（同 project save 类，不进 undo）；Delete 仅对当前选中预设可用。内置声源隐藏预设行。
- **守卫定位**：`instrument_engine_guard(track)` 经乐器 doc 节点反查 engine id 再扫 `voice_guards`——不能直接按 track 匹配（效果器守卫同轨注册）。
- **后续**：效果器预设（机制可平移，缓存键换 doc id）；预设重命名/导入导出按需再加。

### Dexed .syx 预设导入（dev 工具，2026-10-15）

- **state 格式逆向结论**：Dexed 的 CLAP state = `VC2!` 魔数 + u32 LE XML 长度 + XML + `\0`。XML 根 `dexedState`（cutoff/gain/opSwitch/mpe… 属性 + `currentProgram`），子元素 `dexedBlob` 带**两个** JUCE 专有 base64 属性：`base64:sysex`（整盘 4104 字节 DX7 bank）与 `base64:program`（161 字节 `data[]`）。**加载时 `memcpy(data, program, 161)` 覆盖引擎数据——换音色必须替换 program，只换 sysex 无效**（cart 只是库存储）。
- **编码**：`MemoryBlock::toBase64Encoding` 是 JUCE 专有变体（非 RFC 4648）：字母表 = `.` + `A-Z` + `a-z` + `0-9` + `+`（`.`=0，无 `/` 无 `=` 填充，尾组零填充），格式 = `"SIZE." + 编码流`，位序 MSB-first 6-bit 组。
- **`data[161]` 布局**：`[0..155]` = 解包音色（6 op × 21：EG rate/level×8、BP、scale depth×2（+11/12 曲线位包、+13 rate scaling、+14 AMS、+15 KVS、+16 output、+17 mode、+18 coarse、+19 fine、**+20 detune**）→ pitch EG 8（126..134）→ algorithm@134 → feedback@135 → osc sync@136 → LFO speed/delay/pmd/lamd（137..141）→ lfo keysync/wave/pmsens 位包（141..144）→ transpose@144 → 名字 10 字节（145..155））；`[155]` = opSwitch 位包（运行时由 XML opSwitch 属性同步）；`[156..161]` = 控制器杂项。`Cartridge::unpackProgram` 已 1:1 移植为 `unpack_voice`（含 `normparm` 越界归一化）。
- **导入器**：`dexed_import_ym2612_banks`（`#[ignore]`，`DEXED_IMPORT_BANK_DIR` 指定 bank 目录）：校验 bank 头/校验和 → 模板 XML 双属性替换（sysex = 整盘 bank、program = 逐音色解包 + 模板尾部继承）→ `load_state` → `save_state` 回读解码逐字节校验 program → bank 首音色 NoteOn 出声断言 → 写 `presets/<键>/<bank>-NN-<音色名>.glzpreset`。`dexed_dump_param_table` 可导出插件参数表（id/名字/域）供排查。**2026-10-19 修**：sysex 属性原漏写 `4104.` size 前缀（`toBase64Encoding` 约定 `"SIZE."+编码流`），JUCE `fromBase64Encoding` 会把 `"7DL"` 当长度 7 解析 → `cart.load()` 堆越界读、cart 数据损坏（随后 `memcpy(data, program, 161)` 覆盖故不影响即时音色，但影响 program 切换与再保存）；已补前缀并重跑导入迁移 128 个 preset（现 `sysex="4104.…"`）。
- 已入库：YM2612-Dexed-Presets 四个 bank × 32 = **128 个音色**（BASS_BRASS / KEYS_BELLS / MORPH / SYNTH），预设文件为本地用户数据（.gitignore 已排除）。
- **Dexed 声音问题定位与修复（2026-10-16，诊断 harness `dexed_preset_render_profile` / `dexed_engine_playback_repro` / `dexed_live_load_race`，均 `#[ignore]`）**：导入数据无问题（`unpack_voice` 与上游 `unpackProgram` 逐字段一致、program 字节独立解码复核；引擎级单音符 velocity 127 / 长音符 4s 全程持续、note-off 正常）。修复三处——
  ① **Stop/Play 按 pitch 释放**：`Sequencer::take_pending_offs`（`std::mem::replace` 清槽带出 `(time, note)`）+ `BlockRunner::release_pending_notes`（Play/Stop 先逐轨按音符号 Off，再 `note_off_all` 兜底群发——内置 synth 的 env 忽略音符号，pitch 匹配插件（Dexed `keyup` 按 `midi_note` 查找）靠逐轨 Off；原 `Off{note:0}` 永远不匹配 → 挂死音 = "Stop 停不了"）。
  ② **循环长音符 retrigger guard**：`schedule_track_clips` 同音符重触发前先清槽发 Off（否则单槽 pending off 被覆盖丢失，Dexed 挂死复音堆积成循环杂音）；回归测试 `stop_drains_pending_offs_by_pitch` / `long_note_in_looping_clip_retriggers_with_releases`。
  ③ **`StateGate`（`glitcherz_engine::host_gate`，CLAP + VST3 镜像）**：guard 的 `save_state`/`load_state`/`read_param_values` 包裹 `begin_host_call`/`end_host_call`；`ClapNode::process`/`Vst3Node::process` 开头 `audio_should_park()`——宿主调用在飞时停靠应答、输出置零（仅原子操作）；spawn 时节点未处理不等待。**注意**：实测证明竞态并未污染 Dexed state（竞态 A（绕门）/B（走门）/C（干净对照）三组渲染逐字节一致，此前的"竞态复现"是 morph 音色自身快衰减 pluck 的假阳性——race harness 的 race 循环必须以与对照相同的 blob 结尾，否则最后加载的音色包络会被误读为"损坏"），gate 是 clack 主线程契约的加固而非实测缺陷修复。
  ④ **每轨多音符 pending-off 槽位（2026-10-19，本次 click 根因）**：`Sequencer.pending_offs` 原为 `[Option<(u64,u8)>; MAX_MIDI_TRACKS]`——**每轨仅 1 槽**。同轨第二个同时发声音符（用户实测场景：一个 clip 里的两个长音 B flat + F）的 On 会直接**覆盖**第一个音符的 pending-off，第一个音符永不 note-off；Dexed 按音高匹配释放、`Off{note:0}` 也清不掉 → 挂死 voice 堆积，听感即"琶音般的很多个正弦波 / 换 preset 后短促 click 或无声"（内置 synth 的 env 忽略音符号，故听不出该症状）。改为每轨定长多槽 `[[Option<(u64,u8)>; MAX_PENDING_OFFS_PER_TRACK=16]; MAX_MIDI_TRACKS]`：`set_pending_off` 先复用同音高槽、否则取首个空槽；retrigger guard 扫描全部槽；`take_pending_offs` 返回嵌套数组、`release_pending_notes` 逐槽释放（全程 RT-safe 固定数组）。回归测试 `simultaneous_notes_on_one_track_both_release`；端到端 `dexed_engine_playback_repro` 增补"和弦 Bb3+F3 持续发声 + Stop 后 tail peak=0"断言。
  **结论：根因在宿主的音序器 note-off 追踪，与 Dexed 插件及预设数据无关**（`unpack_voice` 与官方 `unpackProgram` 对 4 bank×32 音色 0 差异、program 字节正确、单音符引擎路径与 live-load 竞态路径均正常）。诊断命令：`cargo test -p glitcherz-clap-host <harness名> -- --ignored --nocapture`。

### 插件 GUI 宿主（plugin-gui spec，G1–G3，2026-10-20）

G1（gui 扩展宿主）/ G2（Win32 嵌入容器）/ G3（参数回接）已落地，Dexed 实测通过；决策 D1–D10 与阶段明细见 `spec/plugin-gui.md`。跨版本 gotchas：

- **CLAP gui 契约**：`destroy` 即 `closed(was_destroyed=true)` 的**宿主确认**（宿主主动关闭后插件才回调 closed；spec 初稿方向写反已修）。gui/flush 调用**不过 StateGate**（[main-thread] 调用与 process 的并发安全由插件契约保证，D7 同款裁决）；GUI 编辑**不进 undo**（preset save 捕获 state 覆盖持久化，D8）；tab 切走 = hide、切轨/retire = destroy（D9）。
- **test-plugin 观察通道**：cdylib 的 static 宿主进程读不到——插件内观察状态（gui 调用序列位图 GuiCallMask、destroy 计数）经**只读参数**暴露，宿主侧 `read_param_values` 读回断言。改 test-plugin 后须显式 `cargo build -p glitcherz-test-plugin`——**dll 不随 clap-host 测试自动重建**（测试仍链旧 dll，症状是"改了没生效"）。
- **Win32 容器（`glitcherz-ui::plugin_window`，G2）**：宿主自建 STATIC 子容器、插件真窗口为孙窗口（宿主永不直接操作插件 HWND）。三个实测坑：① 容器**不能带 `WS_DISABLED`**——禁用父窗口阻断整个子树的鼠标命中与键盘焦点；② reposition **去抖**——同 rect 不再 `SetWindowPos`，否则周期性 `WM_WINDOWPOSCHANGED` 干扰插件内部拖拽（hide 后强制重放一次）；③ 嵌入区**必须在 ScrollArea 外**——原生子窗口不受 egui 裁剪，放滚动区内会留残影。
- **G3 flush 必须 raw vtable 直调**：clack 的 `PluginParams::flush` 包装要 `InactivePluginMainThreadHandle`（实例已激活拿不到）、`flush_active` 要 processor 独占 handle（processor 已移交音频线程的 ClapNode）——两条包装路都走不通；`raw::flush_params` 直调 `clap_plugin_params::flush`（与 `enumerate_params` 同模式）：空 in EventBuffer + 栈上 out EventBuffer（容量 64），解析 `ParamValueEvent` 回 `(id, value)`。
- **clack wrapper 的 flush 分派规则**：对**活跃**实例，宿主调 flush 时 wrapper 分派给 `PluginAudioProcessorParams::flush`（audio 侧实现），inactive 才走 main-thread 版本——**插件侧两个 flush 实现都要写 out queue**（只实现 main-thread 版本时活跃实例回读永远为空，且测试链的是旧 dll 时更难定位）。
- **UI 轮询时序（G3）**：`ui()` 尾部、编辑器 open 且非 `gui_closed` 时轮询 `poll_param_flush`（原子标志 take 语义），回读值同步 `clap_voice_values` 滑块缓存 + `send_track_param(ParamId::PluginParam)` 下发引擎；缓存写入与下发在 guard 借用结束之后做（借用冲突）。轮询放 `gui_closed` 的 else 分支，避开插件自毁边角。

### 键盘输入节点（G6，spec `plugin-gui.md` §8，2026-10-20；2026-10-22 改版为输入端口）

`NodeDoc::Keyboard{node_id, track, source}` 是文档里**唯一没有引擎对应物的节点变体**：不进引擎图、不建 processor、不占 slot（`install_node` 首行早退，`node_map` 里查不到它）。它的语义全靠文档的**绑定 + 连线**（"A 设计"，2026-10-22 定）：

- **绑定（A 设计）**：`source: MidiSource = Computer | Device(name)`，`#[serde(default)] = Computer`。**一源一节点**由 `validate` 强制（`validate_rejects_two_nodes_bound_to_the_same_source`）；`create_patch_node` 在已有 Computer 节点时拒绝再加一个（否则写出自己 validate 会拒的文档）。`track` 只作 patch 摆放分组，**不再是路由依据**。
- **无 fallback**：`send_to_vk_targets` 在 `vk_targets` 为空时只发一次状态提示（"cable the input node to an instrument (Ctrl+L)"）然后静音——旧的 `EngineCommand::Note { track }` 回落分支已删除。`send_note_event_at(doc, event)` 只做 `node_map` 查表，查不到就丢（节点刚删）。`--auto-play-note` 例外：它直发 `EngineCommand::Note { track }`，因为那是音频链诊断，不是路由。
- **路由纯函数**：`GraphDoc::computer_keyboard() -> Option<node_id>` + `keyboard_routes_from(vk) -> Vec<u64>`。沿 `kind == Midi && from == Node(id)` 做 BFS，最后**按文档序遍历 nodes 收集全部 Instrument**（不是 BFS 出队顺序——多目标时结果确定）。非 instrument 终点会被遍历但永不返回（MIDI 不穿透效果器）。
- **默认工程**：`default_graph_doc` 末尾追加一个 `Computer` 节点（id = 现有最大 id + 1）并 `keyboard_midi(vk, track0 voice)`——新工程开箱能弹，但那条线是可见可删的真实连线。默认模板（2026-10-29 精简）节点 = 乐器 + mixer + keyboard + 每条 audio 轨的生成器节点；连线 = MIDI 自连 + 乐器→mixer.ch0 + mixer→master + keyboard→乐器。
- **多出线（2026-10-22）**：一个输出口可拉多条 Midi 连线（多出 / 单入，与 track outlet 同规则），一次按键送到全部目标 → 无混音器即可叠音。`connect_keyboard_midi` 是「追加 + 去重 + 上限 `MAX_MIDI_FANOUT`(4)」——旧的 `retain` 同源连线把它锁成单目标，正是"只能拉一条线"的根因。上限只在 dispatch 层拦（文档 `validate` 放行，加载别人多连线的工程不会整份失败）。
- **音符号直达**：引擎 `EngineCommand::NoteAtNode { node_id, event }` → `RtGraph::node_note`（slot 线性扫描找 `node_id`，无分配；未知 id 返回 false 静默丢弃）。这条路径**绕开轨道扇出表**，因此能弹一个没接时间轴 MIDI 线的悬空 instrument。
- **UI 状态机**：`GlitcherZApp::vk_targets: Vec<u64>`（文档 instrument id，文档序），每帧 `ui()` 开头由 `computer_keyboard()` 重算；`send_to_vk_targets` 按索引循环逐个投递。**只在"掉出路由"的目标上补 Off**：新增目标不切断正在响的音，删节点/连线时旧目标全部掉出 → 全部释放（否则挂音永远收不到 note-off——与 Dexed "Stop 停不了" 同一类坑）。
- **`Ctrl+L` 快速试听**：`toggle_audition_route` 把选中的 instrument 接到 / 断开电脑键盘节点；全连 → 只删这几条，未全连 → 补缺的那几条并检查 `MAX_MIDI_FANOUT`。不重写手搭的其他连线。快捷键在全局 handler 的 Ctrl 分支里，不与演奏映射冲突。
- **屏幕键盘退役（2026-10-22）**：`keyboard_section`（on-screen 键盘 + `Keyboard — Track N` 头部 + `mouse_down_notes`/`BLACK_SEMIS`）整段删除；状态栏新增 `Keys → N synth(s) T1 T3`（空则 `none`）。副作用：键位提示不再常显（如需再补，放输入节点 tooltip）。
- **无引擎节点路径的守卫**：`create_patch_node` 对键盘节点跳过"引擎安装是否成功"的判定（否则会被判失败回滚），`delete_graph_node` / `remove_last_midi_track` 对 `node_map` 缺失的节点不做 `remove_node`/`retire_guard`。

### VST3 编辑器镜像（G7，spec `plugin-gui.md` §9，2026-10-21）

CLAP（G1–G3）的 VST3 镜像。UI 层（`plugin_window` 容器 / 面板占位区 / reposition / 销毁时序）**原样复用**，格式分派全在 guard 层——`PluginGuard::{Clap,Vst3}` 的两个 `open_editor`/`close_editor` 同签名。

- **新模块 `vst3-host/gui.rs`**（与 `host_impl.rs` 的音频线程契约分开）：`HostPlugFrame`（`IPlugFrame::resizeView` → 原子打包 w/h；**回调里不调 `onSize`**，宿主这时还没改窗口尺寸）、`HostComponentHandler`（`IComponentHandler` + `IComponentHandler2`；`performEdit` 入队，`beginEdit/endEdit/restartComponent/setDirty/startGroupEdit/finishGroupEdit` 只应答）、`EditQueue`（128 槽原子环形队列：`performEdit` 可能来自插件 GUI 线程**或**音频线程，所以只能原子操作；CAS 抢写指针，抢输时宁可丢一条也不能写坏；UI 每帧 `drain`）。
- **`NoopComponentHandler` 删除**：它的 `performEdit` 空实现正是 G7 要补的缺口；handler 现在持有 `Arc<EditQueue>`，guard 持有同一 Arc 供轮询。
- **guard 新增字段**：`view`（`Option<ComPtr<IPlugView>>`，声明在最前 → 先于 plug frame/controller 释放）、`editing`（`Option<ComPtr<IEditControllerHostEditing>>`，spawn 时 `cast` 一次）、`gui`/`edits`/`plug_frame_ptr`/`_plug_frame`。`tear_down()` 第一步改成 `close_editor()`（视图会回调 controller 与 plug frame，必须死在 terminate 之前）。
- **`open_editor` 序列**：`createView("editor")` → `setFrame(plug frame)` → 可选 `IPlugViewContentScaleSupport::setContentScaleFactor(ppp)` → `isPlatformTypeSupported(kPlatformTypeHWND)` → `attached(hwnd, kPlatformTypeHWND)` → `getSize`。**每一处失败都撤销 attach 并 `setFrame(null)`**，不留半开视图（E2 失败清理的 CLAP 同款）。返回 `ViewSize{width,height}`（物理像素，UI 除 ppp 转 points）。
- **尺寸握手（与 spec 原文相反）**：插件 `resizeView` 只落进 `GuiHostState`；UI 帧尾 `editor_resize_request()` 取走后更新 `editor.size` 并记 `PluginEditor.pending_ack`，**等容器真的 `reposition_screen_rect` 之后**才 `ack_editor_size()` → `IPlugView::onSize(rect)`。先 ack 后改窗口会让等 `onSize` 的插件按旧几何重排。
- **参数回接（插件 → 宿主）**：VST3 没有 CLAP `request_flush` 那种"宿主拉"的通道，只有 `performEdit` 的"推"。UI 帧尾 `take_param_edits()` 把 `(id, normalized)` 交给 `GuiParamBatch{is_clap:false}`，写 `vst3_voice_values` 缓存 + `send_node_param(PluginParam)` 下发引擎（归一化域与引擎桥同域，无需换算）。
- **宿主滑块 → 插件 GUI（spec 方向修正）**：正确接口是 `IEditControllerHostEditing::beginEditFromHost(id)` → `setParamNormalized(id,v)` → `endEditFromHost(id)`；`beginEdit/performEdit/endEdit` 是插件→宿主方向（`IComponentHandler`），spec 原文写反。控制器不实现该扩展时退化为单独 `setParamNormalized`。UI 出口 `sync_vst3_host_param(doc_id,id,value)`，接在三处：VST3 乐器参数面板、VST3 效果参数面板、节点内联插件参数（面板/内联互换时 GUI 与缓存一起动）。
- **测试插件**：`SineController` 加只读探针参数 **id 2 `GuiCalls`**（值 = 掩码/1024，10 位以内是精确二进制小数），掩码由 `SinePlugView` 写：attached/removed/getSize/onSize/setFrame/canResize/resizeView 达成/performEdit 达成/beginEditFromHost/endEditFromHost。`SinePlugView` 是**无窗口假视图**：`attached` 时调 `IPlugFrame::resizeView(320×240)` 并调 `IComponentHandler::performEdit(1, 0.75)`（CLAP 假 GUI `hide` 钩子的镜像）；`createView` 只认 `"editor"`，返回 `ComWrapper::to_com_ptr().into_raw()`（+1 引用交给宿主，factory 同款），并把自身 COM 指针存进 `AtomicPtr` 供回调使用（`&` 下改原子不需要 unsafe）。
- **测试**：`gui` 3 项（队列往返 / 溢出丢新 / 尺寸请求 take 一次）；`vst3-host` 3 项（`editor_view_round_trip` = attached→getSize→resize 请求→onSize 确认→removed，含重复 close 与重开；`perform_edit_reaches_the_host_queue`；`host_param_change_reaches_the_controller` 含 begin/end 位）。`parameter_enumeration_reports_gain` 期望值改 2 个参数。
- **踩坑**：`cargo test --workspace` **不会**重建 cdylib fixture（G7 首次跑测试时链到 10-16 的旧 dll，症状 = `NoEditorView` + 只有 1 个参数）。改过 `glitcherz-test-vst3` 后先 `cargo build -p glitcherz-test-vst3` 再跑测试；`test_plugin_path()` 只检查文件存在，旧 dll 不会报警。

### 插件目录与启动扫描（U9 E1/E2，spec `plugin-browser.md`，2026-10-22）

**模块切分**：`glitcherz-ui/src/plugin_catalog.rs` 完全不依赖 egui —— 数据模型（`PluginEntry` / `PluginCache`）、缓存 diff（`plan_refresh`）、合并排序（`merge` / `sort_entries` / `view`）、编码（RON）、扫描状态机（`PluginCatalog`）与 `scan_module` 都在这里，帧循环只做三件事：画进度条、取一个模块、喂回结果。八个纯函数单测 + 两个状态机单测不碰磁盘、不碰插件二进制。

**缓存有效性**（易踩点）：
- 有效性按**模块路径**判定，不是按条目 —— 一个 VST3 bundle 可能注册多个 class，它们的 `mtime/size` 相同，必须同生共死（`plan_keeps_unchanged_module_and_all_of_its_classes` 锁定）。
- 目录集比较用**打标签的扁平字符串** `"clap:<path>"` / `"vst3:<path>"`，任一格式的搜索目录变化都触发全量重扫，但缓存字段只有一个 `Vec<String>`。
- 版本不符或目录集变化时，老的条目路径全部进 `removed`（仅用于日志），`keep` 清空。

**接线**：四个惰性 `Option<Vec<...>>`（`clap_plugins` / `clap_effects` / `vst3_plugins` / `vst3_effects`）全部删除，换成单个 `plugin_catalog`。原先 `scanned_*()` 里"首次展开菜单时全盘扫描"的整段逻辑消失——这正是原来 UI 卡数秒的根因，且 CLAP 乐器/效果各扫一遍、VST3 同理（最多 4 次全量）。现在菜单只剩查询期过滤：`plugin_entries(format, want_instrument)`（薄包一层 `view`）；空列表提示由 `plugin_empty_hint` 区分"扫描中"与"真的没有"。

**帧序契约（D14）**：扫描发生在 `ui()` 的**最后**——`self.plugin_scan_step()` 取 `pending` 队首扫一个模块，紧跟 `request_repaint()` 让队列以帧率推进（否则只有 50 ms 的显示刷新节奏，冷启动 200 个模块要十几秒）。进度条在 `ui()` 开头以 `TopBottomPanel` 画出，文案取 `upcoming()` = **队首**（下一帧才扫的那个），因此插件挂死时屏幕上已经写着它的文件名 —— 这是 in-process 扫描唯一能给的可观测性。

**持久化**：`save()` 里 `storage.set_string("plugin_cache", ron)`；部分扫完的目录也是合法缓存（下次启动 keep 已扫的、只补剩余）。eframe 退出/周期写盘，崩溃丢缓存只损失一次冷扫。

**Rescan**：patch 右键菜单底部的 `Rescan Plug-ins` → `PatchAction::RescanPlugins` → `PluginCatalog::rescan()`（塞一个空缓存，`dirs` 不匹配 ⇒ 全量）。E3 重写添加菜单时该项随之搬走。

**实测**：冷启动扫描期间日志里能看到 Cardinal 的 CLAP 描述符枚举会拉起整套 Rack 上下文（`Creating module widget ...`，约 140 ms）——正是"启动时一次性付清"优于"点开菜单才付"的实例。热启动（缓存命中）**零 dylib 加载**：第二次运行 `RUST_LOG=info` 输出中 Cardinal/Rack 日志条数为 0。

### 插件选择器与添加菜单（U9 E3，spec `plugin-browser.md`，2026-10-23）

**模块切分**：`glitcherz-ui/src/plugin_picker.rs` 只有浮层与它的纯函数（`PickFilter` / `PickChoice` / `RecentPlugin` / `matches_query` / `visible` / `visible_recents` / `remember`），数据源仍是 `plugin_catalog` 的条目表 —— picker 不碰磁盘、不实例化插件。**菜单**在 `patch.rs`（`PatchAction::OpenPluginPicker` / `PickSoundFont`），**建节点**在 `lib.rs`（`create_patch_node(NodeSource, at)`）—— 三者只通过 action 通信，patch 依旧零副作用。

**为什么是 `Area` 而不是 `menu_button`**：叶子层要放 `TextEdit` + 滚动区 + 键盘导航，egui 原生 submenu 对这三样都不友好（spec D4）。popup 用 `Order::Foreground`，`constrain_to(ctx.content_rect())` 保证不出屏。**注意 `ctx.screen_rect()` 在 egui 0.36 已改名 `content_rect()`**。

**键盘归属**（易踩点）：
- `TextEdit` 的 `event_filter` 默认包含 `vertical_arrows`，单行框也会吃掉 ↑↓；所以 picker **先** `input_mut(|i| i.consume_key(...))` 读走 ↑↓/Enter/Esc，**再**画输入框。`consume_key` 只标记 consumed（`filtered_events` 不过滤 consumed），因此真正让 Enter 不被输入框吞掉的是 `.return_key(None)` —— 单行框默认把 Enter 当"结束输入"（`transition: surrender_focus`）。
- 输入框用固定 `Id`（`query_id()`）：egui 自动生成的 id 会随 popup 位置变化，导致开帧 `request_focus` 落空、下一帧丢焦点。
- 打开帧的点击/`Esc` 必须忽略一次（`just_opened` 标志）：开菜单那一下的 pointer click 与新 popup 的"点击外部关闭"是同一帧同一事件，不设这道闸，popup 会自己关掉。
- picker 打开时 `handle_keyboard_input` 整段跳过，否则空格（播放）/Delete 会穿透到走带与 patch。

**行渲染**：22 px 的 `allocate_exact_size` + `Sense::click()` 自绘背景，`ui.new_child(max_rect)` 里左 `Label::truncate()`（宽度 = 总宽 − 格式标签宽）、右 `right_to_left` 布局放 `CLAP`/`VST3` 小字标签；`response.on_hover_text(path)` 给全路径。Recent 段与 All 段共用同一份行列表，键盘高亮索引因此天然连续（`rows` 每帧重建，选中索引 clamp）。

**单次 install（D1）**：`NodeSource` 扁平枚举（`BuiltinSynth` / `BuiltinDelay` / `SoundFont` / `ClapInstrument` / `Vst3Instrument` / `ClapEffect` / `Vst3Effect` / `Mixer` / `Feedback` / `Keyboard`）直接给出 `VoiceSpec`，`create_patch_node` push 文档后**只调一次** `install_node`。旧路径"先建内置、再从面板换装"的两次 install + 一次回收消失。

**失败即回滚**：`install_node` 对插件加载失败是**静默降级**（回落到 SynthProcessor / DelayProcessor），因此新节点若"声称是插件"却在 `voice_guards` 里找不到自己的 engine_id，就当失败处理：`session.restore(before)` + 状态栏报错，文档不留空壳。SoundFont 不在此列（缺文件回落到内置声源是既定语义，`sf2-loader.md` D8）。

**Recent（D10）**：`UiState.recent_plugins`（`#[serde(default)]`，跨会话）。`remember()` 按 `(format, path, id)` 去重后插到队首，再按 `is_instrument` 各自截断 5 条。点 Recent 里的条目即使插件已被卸载也会尝试实例化 —— 失败走上面的回滚路径并报错，比藏起来更可诊断。

**测试**：8 项单测。4 项用 `ctx.run_ui(RawInput)` 跑 headless 帧（键盘选取、Esc 取消、Recent 置顶、开帧点击不自关），不需要窗口/显示后端 —— 与 `tests/egui_event_semantics.rs` 同一套路，可回归"popup 在真实帧循环里能画出并返回结果"。

**E4 已补齐**：连线中点菜单改为 `Insert ▸ Effect ▸ …` + `Insert ▸ Mixer`，picker 增 `PickerPlace::Insert`（见下一节）。

### 连线中点插入复用 picker（U9 E4，spec `plugin-browser.md`，2026-10-23）

**同一 popup 两个落点**：`PickerPlace`（`Add { at }` / `Insert { cable, at }`）挂在 `GlitcherZApp.picker_place` 上，`PickFilter`（类别 + 格式）仍是列表筛选。`PatchAction::OpenPluginPicker` 里 `at: Pos2` 换成 `place: PickerPlace`，菜单与 picker 因此不需要第二份浮层代码；`PickerPlace::at()` 给需要坐标的调用点留一个统一出口。

**菜单**：`Cable ▸ Insert ▸ Effect ▸ {Built-in Delay, ──, CLAP…, VST3…}` + `Insert ▸ Mixer`，音频线才有 `Insert` 分支；MIDI / 自动化线只剩 `Delete Cable`。和 `Add` 菜单同一套"先类别后来源"的形状，两处出生节点的地方读起来一致。

**三个必须守的约束**：
1. **归属轨道**（修复既有缺陷）：`create_patch_node` 原先把 `selected_track` 写进节点的 `track` 字段，插进别的轨的链路时归属就错了。拆出 `create_patch_node_on(source, at, track)`，插入路径用 `graph.node_by_id(上游节点).track()` —— 处理器永远跟着被插的链路走，`register_track_chain` 也刷对那张表。
2. **一步 undo**：建节点、断旧线、接两根线各自 `commit_edit`，不合起来就是 4 步。用 `session.suspend_history()` 包住这三段、末尾 `commit_edit("Insert Node", before)` 提交一个快照（与 `Import MIDI` 同一套路）。`create_patch_node_on` 失败时先 `resume_history()` 再返回，靠它自己的 `session.restore` 把文档退回原位，线一根没动。
3. **槽位不变**：纯函数 `splice_endpoints(&CableDoc) -> Result<(上游 node, 下游 PortRef, in_port), &str>`，`A → B` 变 `A → new → B` 时下游的 `in_port` 必须原样搬过去（D17，mixer 推子与线缆的绑定）。非音频线、`from` 不是节点的线直接报错，比原先散在函数体里的三处 `if` 好测。

**可插入源白名单**：`NodeSource::fits_audio_insert()` —— `BuiltinDelay` / `ClapEffect` / `Vst3Effect` / `Mixer`。乐器进音频线会没有 MIDI 输入、永久静音，所以不是"能跑就先允许"。picker 侧已经用 `NodeCategory::Effect` 筛选，这层是动作层的兜底。

**测试**：`splice_tests` 5 项（槽位保留、master 目标、MIDI 拒收、无上游节点拒收、可插入源白名单）。App 级断言（`node_map` 只加 1 条、`retiring_guards` 不增长）无法在无窗口环境下做 —— `GlitcherZApp::new` 需要 `eframe::CreationContext`，仓库也没有 headless app harness，故以 `cargo run --seconds 5` 冒烟 + 代码结构（单次 `install_node` + 无回收分支）代替。

### Inspector 收口（U9 E5，spec `plugin-browser.md` §8，2026-10-23）

**删掉的两块**：① 乐器源选择行（`Built-in Synth` / `CLAP Instruments…` / `VST3 Instruments…` / `SoundFont…`）；② 同段的 `CLAP Effects…` / `VST3 Effects…` 快捷加载。删完之后 Inspector 只剩 preset / 参数栈 / `Show Editor` / 内联参数 —— D2 的"结构归 patch、参数归 Inspector"这才真正成立。

**保留的两块（容易误删）**：SoundFont **preset** 下拉（`Bank n` + `<program>. <name>` 两个 ComboBox）是术语表里的 preset，不是源选择；`Clear All Effects` 不是选择器而是批量删除，挪到效果节点列表头部（`fx_track_view`，即面板没有窄化成单个效果节点时；窄化时标题会被读成"清掉这一个"）。

**连带删掉的死代码**（"换源 = 删了重建"的直接结果，D3）：`set_builtin_instrument`、`load_soundfont_voice`、`load_clap_voice`、`load_vst3_voice`、`insert_effect_common`、`load_clap_effect`、`load_vst3_effect`、`plugin_entries`、`plugin_empty_hint`、`node_track`。这些都是"已存在节点换处理器"或"隐式插到轨链尾"的路径，E3/E4 之后它们既无人调用也与新语义冲突：`insert_effect_common` 会绕过 D6 悬空出生、把效果器悄悄接到 mixer 前面，正是 D12 要消掉的隐式捷径。净减 **549 行**，`node_map` 只有 `install_node` 一个入口，`cargo clippy --workspace --all-targets -D warnings` 无 `#[expect(dead_code)]` 兜底。

**读取顺序提醒**：本文档前面几节（S5 面板、插件参数桥接、乐器预设、插件状态持久化）里出现的 `load_clap_voice` / `load_vst3_voice` / `load_soundfont_voice` / `load_clap_effect` / `load_vst3_effect` / `insert_effect_common` 都是 E5 之前的**历史路径**；现在的活体装配只有 `install_node`（`apply_document` / undo / redo / 加节点 / 插线中）一条。

**验证**：全仓 497 测试绿（删除全程没动测试），`cargo run -p glitcherz-app -- --seconds 5` 首帧正常、按时退出。Dexed（CLAP）与 200+ VST3 目录的手工验收仍按 spec §10 门槛留给人工过一遍。

## 效果器与声源

### SF2 音色库加载器 S1–S6（2026-10-19 收官，spec 见 `.agents/spec/sf2-loader.md`）

- **S1 RT 审计**（结论细节在 spec §7）：`rustysynth-ext = "=1.4.0"` 热路径零堆分配实证（计数 `#[global_allocator]`，测试 `tests/sf2_rt_audit.rs`），`VoiceCollection` 即预分配池，voice 池 fork 预案（D4）解除；唯一 panic 点 = `render` 等长契约（调用侧保证）。**审计 harness 的坑**：断言 label 的 `format!` 在计数窗口内求值会被计入分配（首跑真实库的假阳性），label 构建须在 scope 之外；计数器全局，测试须 `Mutex` 串行。
- **共享测试 fixture**：`tests/common/mod.rs` 手建最小合法 SF2 字节流（单 preset 正弦 + loop + exclusive class，bank/program 可参数化 `minimal_sf2_bytes_at(bank, program)`）——纯 `cargo test` 无外部文件可跑全部路径；真实库测试统一走 `#[ignore]` + `GLITCHERZ_SF2_PATH` 环境变量（本地数据不入库，D9）。**手写 RIFF 的坑**：ibag terminator 的 generator index 必须指向 igen 全部记录之后（差一条 → zone 丢 `sampleID` → 被当 global zone → 0 region → 静音且不报错）。
- **S2 非实时模块 `engine::sf2`**：`Sf2Library::load(path)`（控制线程：BufReader + `SoundFont::new`，产物 `Arc<SoundFont>` 交 RT 侧不再解析）+ 加载期一次性 `presets(): &[Sf2Preset{bank: u8, program: u8, name}]` 枚举（D7 轨级 u8:u8 寻址；**bank/program 超出 u8 的 preset 跳过 + log::warn**，全部不可寻址 → `Sf2Error::NoPresets`）+ `find_preset(bank, program)`（UI 下拉与 S4 VoiceSpec 寻址共用）；`Sf2Error{Io, Parse, NoPresets}` thiserror。rustysynth-ext 自 S2 起为 engine 正式依赖。测试 `tests/sf2_load.rs` 8 场景（含截断/垃圾字节/越界 bank），真实库（MuseScore_General/FatBoy）load + 枚举 + **preset 地址唯一性**验证通过。
- **S3 实时渲染 `realtime/sf2.rs` `Sf2VoiceProcessor`**：实现 `Processor`，spec §7 接入要点全部落实——① `SynthesizerSettings.block_size = 128`（对齐 `FIXED_BLOCK_SIZE`）；② stereo render 缓冲构造期预分配，`process` 内经 `render(&mut l, &mut r)` 等长切片填入（排除库唯一 panic 点）后 `copy_channel` 进图缓冲（目标帧数小于 render 帧数时截断，mono 缓冲只写 ch0）；③ 轨级 bank:program 经 `process_midi_message(0xB0/0xC0)` 下发（零分配，S1 已实证），`set_master_volume` 纯写入挂 `ParamId`。**参数面**：`ParamId::{Sf2BankSelect, Sf2ProgramChange, Sf2MasterVolume}`（bank/program 为字面整数参数：`f32.round() as i32`，NaN 饱和转换为 0；负值不钳制——库侧 `set_bank/set_patch` 纯字段写、仅作 preset_lookup HashMap 键，未命中走 GM/默认 preset 回落，已核实安全；master volume 非有限值忽略）；`Sf2Error::Synth` 变体承接 `SynthesizerSettings` 越界（采样率 16000..=192000 / block 8..=1024 / 复音 8..=256）。**复音** `SF2_POLYPHONY = 64`。**毯式 note-off 语义**：引擎 transport stop 发 `Off { note: 0 }`，pitch-matched 接收端会忽略 → 挂音；以 `held: u128` 位图跟踪按下键，`Off { note: 0 }` 且 key 0 未按住 = `note_off_all()` 全释放，key 0 按住 = 正常 pitch-matched 释放（`held_note_zero_release_stays_pitch_matched` 测试锁定）。**边界**：velocity f32→MIDI 七位（NaN/Inf 钳制）；越界 note（≥128）照常转发 synth（内部静默忽略）但 `1u128 << note` 前有 `<128` 守卫（否则 panic）。测试 `tests/sf2_voice.rs` 13 场景 + 真实库 smoke（`#[ignore]` + `GLITCHERZ_SF2_PATH`）。**测试坑**：GM 默认 CC#91 reverb send=40，被拒绝的音符仍带 ~0.003 尾音残余——绝对阈值断言（`< 0.001`）会假失败，须先渲染 baseline 再相对比较；fixture `two_preset_sf2_bytes` 的 preset keyRange 以 pgen generator `lo | hi << 8` 编码（`20..=40`）。
- **S4 model `VoiceSpec::SoundFont`**（spec D5/D6/D8）：`SoundFont { path: String, bank: u8, program: u8 }` 内部 tag 枚举新变体（tag = `"sound_font"`，serde default 加法字段，PROJECT_VERSION 不动；bank/program 缺失加载为 0:0）。测试：往返覆盖进 `voice_spec_ron_roundtrip` + 新 `voice_spec_soundfont_serde_compatibility`（tag 形态 + partial 字段默认）。**两个 serde gotcha**：RON 紧凑输出无空格（断言 `contains("type:\"sound_font\"")`，带空格版本假失败）；`#[serde(default)]` 的默认值是类型默认（`u8::default()` = 0）而非实例值。**UI 侧连带补齐 8 处非穷尽 match**（编译必需）：`install_node` Instrument+SoundFont = 完整文档恢复路径——非实时线程 `Sf2Library::load` → `Sf2VoiceProcessor::new` → 构造期 `set_param(Sf2BankSelect/Sf2ProgramChange)` 下发（D7 地址在装入前重放），文件丢失/解析失败回落 `SynthProcessor` + `log::error`（D8，文档字段保留）；Effect+SoundFont 回落 `DelayProcessor`（防御性，UI 不产生该组合）；`voice_plugin_key`/`track_external_fx` → `None`（S5 前无 preset 库键/外挂效果器面板）；preset load/reset 的 `&mut` match → `{}`（SF2 无 state blob）。patch 图标签 `SF2 {bank}:{program} · Track N`。
- **S5 UI 音色库面板**：Synth 面板 Instrument 区加 `SoundFont…` 按钮（`rfd::FileDialog` filter `sf2`/`sf3`，与项目保存同一同步弹窗先例）→ 调用侧 begin_edit + 文档改 `VoiceSpec::SoundFont{path, bank: 0, program: 0}` + `load_soundfont_voice` + commit_edit（与 CLAP/VST3 换声源同一 undo 分工：改文档在调用侧，函数只做 engine 替换）。**会话单库**（spec §6 v1 不做多库）：App 字段 `sf2_library: Option<Arc<Sf2Library>>`；`obtain_sf2_voice(path, sr)` = path 匹配复用缓存 / 未命中 `Sf2Library::load` 并替换缓存，`install_node` 与面板共用（同一工程重开秒装，避免每次 install 重读几十 MB 文件）。`load_soundfont_voice`：retire_guard（外部插件切 SF2）→ obtain → `set_param(Sf2BankSelect/Sf2ProgramChange)` 构造期下发 → `replace_node` → 清其他格式面板缓存。**preset 下拉**：当前轨声源为 SoundFont 时显示 ComboBox（`sf2_preset_{sel}`），preset 列表从共享库枚举后**快照 clone 出来**（`Vec<(u8,u8,String)>`，释放 `self.sf2_library` 借用——否则 ComboBox 回调里 undoable 编辑的 `&mut self` 借用冲突）；`set_soundfont_preset` = begin_edit + 文档改 bank/program + `engine.set_node_param(engine_id, Sf2BankSelect/Sf2ProgramChange)`（节点寻址即时下发，FIFO 顺序保证 bank 先于 program）+ commit_edit("Select SF2 Preset")；undo/redo 经 apply_document → install_node 重放。库未加载（跨会话或路径不符）显示 `(soundfont file not loaded this session — reopen it)`；`Built-in Synth` 高亮条件改为 `!is_external && !is_sf2`（源选择行本身已于 U9 E5 删除）。真实库端到端验证：MuseScore_General smoke（256 preset 枚举 + preset 切换 + 有声输出）通过。
- **S6 SF3 feature 常开 + 端到端验收**：workspace `rustysynth-ext = { version = "=1.4.0", features = ["sf3"] }`——lewton 0.10（Vorbis 解码），新增传递依赖 lewton/ogg/byteorder/tinyvec 全纯 Rust、无 C 构建链（跨平台 CI 前提保持）。SF3 解码只发生在 `SoundFont::new`（加载期，控制线程 D3），RT 热路径代码零改动。**S1 审计复跑（sf3 开启后）**：fixture 9 场景 + 真实库 MuseScore_General（256 preset 逐个 bank/program 切换 + 和弦 + render 逐计数）全部 0 分配；真实库端到端验收 `-- --ignored` 全绿（sf2_load / sf2_rt_audit / sf2_voice 三套）。UI 侧 S5 的文件对话框 filter 已含 `sf3`，无需改动。本机无 .sf3 样本文件，SF3 解码路径的实证验证留待拿到真实 SF3 库后补跑（依赖面与解码线程已静态核实）。
- **S8 SF3 实测 + 奇数块补齐（2026-10-28）**：真实 SF3（Airfont380 Stanway Grand，3 MB）首次加载报 `invalid sound font: the INFO list contains an unknown ID '?LIS'`。**根因不是解码器而是 RIFF 遍历**：该文件 `smpl` 长度 3 099 967（奇数），RIFF 要求其后补 1 字节对齐、且该补齐字节**计入外层 LIST 长度**，而上游 `soundfont_sampledata::read` 的 `while bytes_read() < end` 循环读完 `size` 就下一个块 → 错位一字节，下一块 id 读成 `?LIS`（`?` 是非 ASCII 替换符）。**SF3 的 `smpl` 是拼接的 Ogg 流，长度奇数是常态**，所以这不是"某个坏文件"而是"所有 SF3 都挂"。修法：`BinaryReader::discard_padding(reader, size)`（奇数 size 吃掉 1 字节）在 `sdta` / `INFO` / `pdta` 三处循环尾部各调一次；回归测试 `reads_an_odd_sized_smpl_chunk_followed_by_another_chunk`（合成字节流，无外部 fixture）。修复后该文件 `Sf2Library::load` → 1 preset `0:0 Steinway Grand`，debug 构建解码 5.2 s（release 会明显更快）。**注意**：`.vendor/rustysynth-ext` 是独立 package（workspace `exclude`），其单测只能在该目录内 `cargo test` 跑，仓库 CI 不覆盖；3 个 `test_load_*` 失败源于缺上游 samples fixture，与本地改动无关。
- **S7 preset 合并语义调查（2026-10-20，结论：保持相加语义，"vel 100 偏轻"非引擎 bug）**：起因 = 钢琴类 patch 虚拟键盘（vel≈0.9）有声、piano roll（默认 vel 100/0.7874）偏轻，疑 preset zone 合并语义错误。**实证过程**：① vendored `rustysynth-ext` 到 `.vendor/`（根 `Cargo.toml` path 依赖 + `[workspace] exclude`，保留 sf3 feature），新增 preset 层 `explicit: u64` 位掩码 + SF2 2.04 §8.2.3 的 14 个非法 preset generator 过滤（`PRESET_INVALID_GENERATORS`，当前测试字体无此类项零行为差异）+ 公开诊断 API `PresetRegion::get_generator()`；② 实现 SF2 §8.5.2 覆盖语义（preset 显式 generator 覆盖 inst 值，仅 initialAttenuation 相加）后 vel 1.0 峰值从 0.0912 **暴跌 150 倍至 0.0006**——假设证伪，回退相加语义。**根因**：MuseScore_General（机器生成字体）的 preset 值是**覆盖意图与相加意图的混合体**——覆盖意图如 `attackVolEnv=-7973`（10ms 整值）/`5573`（25s 哑音层技巧），相加意图如 FF 层 `initialFilterFc=-343`（微调偏移，覆盖语义下 cutoff 变 6.7Hz 近静音）。纯覆盖或纯相加都无法两全，但相加优雅降级、覆盖造成灾难，故 MeltySynth/RustySynth 家族的相加行为是正确选择。**vel 100 偏轻的真实构成**（note 60 匹配多 zone 叠加）：`key 0..127 vel 75..107` zone（attn 14dB + `modEnvToFilterFc=-2350` 暗化 → 用 MF 采样）+ `key 60..127 vel 64..107` zone（attn 17dB + attack 25s 哑音层）+ rustysynth velocity→gain 曲线（`2·20·log10(vel/127)` Polyphone 风格，vel 100 → -4.2dB）= 字体设计使然，非 bug。**外部参照全部不可用**：webfetch 损坏；scoop 装 FluidSynth / cargo fetch oxisynth 的 GitHub 下载均被 sandbox 拦截。测试 `real_soundfont_velocity_sweep` 的 dump 已扩展为打印每 preset zone 的 key/vel range + 显式 generator 清单（`generator_name()` 辅助）+ inst 层滤波/包络参数，供后续人耳对照。vendored crate 自身 3 个失败单测 = 缺上游 testdata fixture（registry 包不含），与本地修改无关。

### 采样播放器 instrument（U5，spec `sample-player.md`，2026-10-24）

**模块镜像 SF2 布局**：`engine::sample`（非实时，hound 解码）+ `realtime/sample.rs`（RT processor），装配走 `install_node` 唯一入口。`SampleData` 把任何位深（int 8/16/24/32 经 hound 统一 `samples::<i32>` 有符号化 + 归一化，float 32）解码为 f32 立体声帧盒；**交错流组帧必须整块 `chunks_exact(2)`**（立体声）或 `into_iter().map(|v| [v, v])`（单声道），逐样本 push 会把右声道丢掉。`SampleLibrary` 是 path → `Arc<SampleData>` 的多样本会话缓存（与 sf2 单库不同：采样器一轨一样本），`obtain_sample_voice` 未命中才解码。

**RT 内核三个易错点**：

1. **步进系数合一**：`rate = 2^((note−root)/12) · wav_sr / engine_sr`——音高比率与采样率补偿是同一个 per-engine-frame 步进，不单独做重采样预处理；线性插值读取。voice 触发时 rate 定格（root note 参数变更只影响后续 note-on，sampler 惯例）。
2. **OneShot 与毯式 off 的区分**：`Off { note: 0 }`（`held` 位图 key 0 未按住）= blanket sweep → **所有** voice 进 fade（含 OneShot，Stop/Panic 必须能停）；pitch-matched off 只进 Gate voice。voice 存 `one_shot` 快照（触发时的 play mode）。
3. **池回收**：`voices[0..active_count]` 紧凑区，块尾 swap-compact（`!active` 的与尾部交换，交换后不推进索引重查）；池满偷 age 最大者（age 每块 +1）。渲染写预分配 scratch 再 `copy_channel` 覆盖 buffer（生成器语义，同 SF2）。

**测试方法论（两条坑）**：① RT 审计的 `AuditScope` 内不许建 `AudioBuffer`——构造分配会被计入（3 次假阳性）；② ramp fixture 逐块上升、块间峰值不可比，**增益/衰减/复音堆叠断言必须用恒幅 fixture**（`flat_fixture`），"输出 = 源 × rate" 数学断言才用 ramp（整数游标处 frac=0 插值精确）。复音上限验证技巧：同 note 触发 16 次（同样本同 rate 同相位）→ 峰值 = 16 × 单 voice；第 17 次后仍 16×即证明偷音。

**UI 接线**：`Add ▸ Instrument ▸ Sample…`（`PatchAction::PickSample` → rfd wav 对话框 → `NodeSource::Sample` → `create_patch_node` 一步 install）；Inspector `sample_section` 挂在 sf2 preset picker 之后（文件名 / root DragValue / play mode ComboBox / gain Slider，root+gain 走 `begin_param_edit("sample-params")` 手势合并，mode 是离散一步 undo）；patch 内联 `SAMPLE_INLINE`（root+gain）经 `apply_inline_param` 与面板同源；节点标签 `SAMPLE {stem}·{note名} · Track N`（`note_name`：`60 → C4`）。install_node 文件丢失回落 `SynthProcessor`（文档字段保留），`Effect + Sample` 防御性回落 Delay。`param_id_of`/`AutoParam` 未加 Sample 变体——automation 菜单的 Cutoff 选项对 Sample voice 不响应（与 SoundFont 同现状，spec §6 明确不做）。

### Audio track（U6，spec `audio-track.md`，2026-10-24）

**播放数学完全闭合在 tick 域**：帧位 = `(song_tick − start)/span × wav_frames`——比例映射是**纯函数**，loop 回卷 / seek 跳中段零游标零特判（位置即状态）。拖长 clip = 变速、tempo 变化 = 变调（磁带效应，用户确认取舍）。每输出帧 j 的 clip tick = `song_ticks + j·dt/frames`，线性插值读 wav（U5 `render_voice` 内核平移）。stop/pause 静音由 `ctx.transport_active` 门控——**没有它，冻结的 song_ticks 会每块重放同一帧区间**（循环卡带）。

**非 Copy 数据的 RT 通道设计**（`Arc<SampleData>` 不能进 `EngineCommand`——其 Copy 是铁律）：

- **clip 表走 structure 队列**：`GraphUpdate::SetAudioClips{node_id, table}`（结构队列本来就传 `Box<dyn Processor>`）。`RtGraph::set_audio_clips` 按 node_id 线性扫 slot → 调 **`Processor::swap_audio_clips` trait 默认方法**（None = 未消费，调用方把传入表进 trash）。
- **displaced 表走 trash 队列**：`mem::replace` 是 move，音频线程零 dealloc；旧表 push `rtrb::RingBuffer<AudioClipTable>`（容量 64），控制线程 UI 帧尾 `engine.reclaim_audio_clips()` drain+drop。满则 park 单槽（`audio_trash_pending`）下块重试；park 被占的理论满 `mem::forget`（有界泄漏）优于 RT dealloc——唯一生产者（UI）同时是唯一 drainer，逻辑不可达。
- **`rtrb::push` 失败把值还回来**：`Err(PushError::Full(v))` 解构取回，不是消耗。

**ctx 构造顺序**（block.rs）：automation evaluate **之后**、advance 之前——`song_ticks` = evaluate 前的 position（本块起点）、`transport_dt_ticks` = evaluate 后 bpm 的步进（与 advance 内部同公式，`Sequencer::transport_dt_ticks` 公开）、`transport_active` = is_playing。**顺序错了 tempo 曲线就滞后一块**。

**`swap_audio_clips` 放错 impl 块的静默失效**：曾写在 `impl AudioTrackProcessor`（固有块）而非 `impl Processor`——trait 走默认 `None`，`set_audio_clips` 永不消费表，编译零警告（固有方法被测试用到不算 dead）。修复后加 **trait 路径测试**（`Box<dyn Processor>` 上调 `swap_audio_clips` 断言 Some）锁定——这是 trait 默认方法 + 固有同名的经典陷阱。

**timeline 三轨化**：`is_control: bool` → `TimelineLane{Midi, Control, Audio}` 枚举（`ClipSel`/action 字段全线 lane 化，编译器强制补全）。行交互全靠 `Placement` trait 对 `AudioClip` 的实现：`set_loop` no-op（loop==span 恒成立）、**`retain_from(offset)` = start += offset; span -= offset——split 右半段自动播 wav 后半**（比例映射的语义红利）。M4b modal（G/T/Shift+D）对 audio 关闭：`arm_modal` 过滤 Audio 选区（两表推理逻辑不动，v1 边界）；K 切刀/点选/框选/拖动/resize/右键删/批删全继承。**lane_row 内变量遮蔽坑**：`let (is_control, track) = row.key()` 脚本替换成 `let lane = ...` 后与行内 Rect 变量 `lane` 冲突——枚举值改名 `lane_kind`（块级遮蔽无碍，函数级遮蔽会炸掉后续所有 Rect 用法）。

**波形"所见即所听"**：`SampleData` 加载期算 peaks（256 帧/列 min/max，双声道折叠）；`draw_waveform` 列 j ↔ peaks 子区间 [j/n,(j+1)/n) 的合并极值——与 RT 读取同一比例映射。UI 帧内波形源 = `SampleLibrary::get`（**只读查找，帧内不解码**——`&dyn Fn(&str) -> Option<Arc>` 不能是 FnMut，load 的 `&mut self` 进不了闭包）。

**建 clip 的 span 换算**：wav 秒 × bpm/60 × TICKS_PER_QUARTER → ticks（clamp ≥1 bar、≤ 排列边界 − start）。

### 采样切分：头 / 腹 / 尾 + ping-pong（SP6，2026-11-05）

spec `sample-player.md` D12–D15 落地。**文档层**（`SampleRegion`：`start` / `loop_start` / `loop_end` / `end` 四个**源帧** + `loop_mode`）→ **参数通道**（五个 `ParamId`）→ **RT 渲染**（回卷 / 反射 / 松键进尾）三段。

几个当初容易踩的点：

- **`0` 是"到文件末尾"的哨兵**。文档不携带文件长度（解码数据在会话 `SampleLibrary` 里），所以 `end == 0` / `loop_end == 0` 必须解释成"文件的最后一帧"，否则旧工程一打开就被裁成 0 帧。解析规则写了两份：model 的 `SampleRegion::resolve()`（喂 UI 波形把手）和 engine 的 `resolve_region()`（喂渲染器）——engine crate 不依赖 model，两份必须对同一件事说同一个答案，改一处记得改另一处。
- **`end` 是"最后一个可插值帧"（`len - 1`），不是 `len`**。渲染器读 `src[i]` 和 `src[i+1]` 做线性插值，游标必须停在 `len-2` 以内；沿用旧实现的 `last = len - 1` 语义，旧工程的输出才能逐样本不变。
- **回卷用 `rem_euclid`，不用 while 减**。音高比率可以很大（同一个采样降 4 个八度或升 4 个八度），`while pos >= loop_end { pos -= span }` 在高比率下会自旋；`loop_start + (pos - loop_start).rem_euclid(span)` 一次算完。ping-pong 用一个周期 `2 * span` 折返：`t = rem_euclid(period)`，`t <= span` 正向、否则反向并翻转 `dir`，接缝处没有跳变（`a_pingpong_loop_has_no_seam` 用 ramp 样本的最大帧间步长钉住这点，Forward 在回卷处会跳整段高度）。
- **松键进尾分岔**：只有"声部已经进到循环段"（`pos >= loop_start`）时才跳到 `loop_end` 播尾；在**头段**里松键仍然走原来的即时 10ms fade——否则一次短促按键会把起音切掉直接跳到尾巴。尾部 fade 用 `fade_wait` 预存"还要等多少引擎帧"：`(tail / (rate · pitch)) - release_frames`，负数归零（尾巴比 fade 还短就立刻淡出）。`release_all`（Stop / Panic / 切轨）**不做**尾部处理，停止必须快。
- **OneShot 忽略循环**（D14）：`wrapping = region.loops() && !one_shot`。否则 OneShot + loop 会无限循环，只能靠 Stop 停。OneShot 于是等价于"头 → 循环段 → 尾走一遍"。
- **窗口在触发时定格**（沿用 D4 的 rate 冻结语义）：改窗口只影响后续 note-on，不会把正在响的游标拽走。因此 `SampleStart/...` 的 `set_param` 只写 `raw_region` 再重解析，不 touch 任何 voice。

**参数路线**：`push_node_params`（实时）与 `export_ops`（离线渲染）各推一遍同样的五个参数——离线导出漏推就会导出用户听不到的素材（导出的是整条文件而不是裁好的窗口）。

**测试**：`sample_voice.rs` +4（`a_forward_loop_repeats_the_window`、`a_pingpong_loop_has_no_seam`、`a_gate_release_plays_the_tail_before_fading`、`a_one_shot_plays_the_window_once`）；model +4（`sample_region_tests`：0 哨兵、越界钳制、一帧循环拒绝、旧文件 default）。旧的采样用例一个没改，全部照过——"旧工程逐样本不变"这条由它们守。

### 采样窗口 UI：波形条 + 四条把手（2026-11-05）

`Sample` 的 Inspector 多了一块 `Play window`：峰值条上用四条竖线（`S` / `A` / `B` / `E`）标出 **起始 / 循环起 / 循环止 / 结束**，窗外区域压暗、循环段浅蓝着色，下面一行是循环模式下拉 + 四个可输入的帧号。

- **数据源**是会话 `SampleLibrary` 的缓存解码（`sample_data_of` 先 `get` 再 `load`），所以面板不额外存任何 UI 状态：换文件、undo、重开工程，波形跟着文档走。
- **`resolved.to_region()` 是编辑基准**。旧工程四个字段都是 `0`（等于"整条样本"），第一下拖动必须先把解析结果**落地**成显式帧号再改，否则 `0` 会被当成"0 号帧"写回去，样本瞬间被裁成 0。这条是 model 里 `ResolvedSampleRegion::to_region` 存在的唯一理由。
- **把手互不越界**：`move_region_handle` 每次按邻居夹一遍（头 ≤ 循环起；循环起在 `[头, 循环止-1]`；循环止在 `[循环起+1, 尾]`；尾在 `[循环止, 末帧]`）。交叉的窗口在渲染器里没有对应行为，会画出"图和声音不一致"的假象，所以干脆不允许。
- **一次拖动一步 undo**：`apply_sample_region` 用合并键 `sample-region`，`drag_stopped` 时 `end_param_edit()` 收尾；输入框和下来这两个**非拖动**路径在写完立刻 `end_param_edit()`，避免它们和后面某次拖动并进同一步。
- **不发重建**：五个窗口参数走 `send_node_param` 即时下发，处理器按 D4 的冻结语义只影响后续 note-on——所以拖动把手时正在响的声部不会被拽着跳（听力上的直觉是"改的是下一次触发"）。
- **`Full` 按钮**只在窗口不是整条样本时出现，点一下把 `region` 复位成 `SampleRegion::default()`（四个 0 + Off），也就是"D12 之前的形态"。

**测试**：`region_handle_tests::moving_a_handle_keeps_the_window_ordered`（四个方向各自的夹取）、`pixel_and_frame_agree`（像素↔帧往返、越界钳制）。

### 波形切换跨淡（declick，2026-11-05）

spec `instrument.md` D13 落地：`OscProcessor` 在运行中换波形时不再直接换算法，而是把旧波形和新波形在**同一相位**上各求一次值再按 5 ms 线性混合。

- **不需要第二个相位累加器**。crossfade 的两个波形共用 `phase` 和 `inc`（含 PolyBLEP 的 `dt`），所以淡入期间音高完全一致、不会拍频；代价只是淡入这 220 个采样里多算一次 `wave_sample`——RT 安全，无分配。
- **`started` 是必要的状态**。构造后第一次 `process` 之前换波形不该淡入（还没发声，没有 click 可遮），否则"新建轨 → 选 Saw → 播放"的前 5 ms 会听到 Sine→Saw 的滑变，而不是一开始就是锯齿。这条也让既有测试（`waveform_shapes_are_distinct` 等在构造期设波形后立刻比对形状）保持原意。
- **5 ms 内再次切换**：从"听感主导"的那个波形重新起淡（淡入进度 < 0.5 就继续淡出旧波形，否则把新波形当成新的起点），单步不超过半个波形落差。比 5 ms 更密的连续切换不是音乐手势，只保证有界。
- **测试锁的是一阶差分上界**：`a_waveform_switch_is_crossfaded` 把切换点放在相位 ≈ 0.25（Sine ≈ +1、Triangle ≈ 0，硬切是满幅阶跃），断言整段最大帧间步长 < 0.10（Sine 自身斜率 0.063 + 淡入附加 ≈ 0.005）。去掉 crossfade 这条会以 ~1.0 失败。
- 顺带发现：锯齿在相位 0 的值经 PolyBLEP 修正后是 0 而不是 -1（`-1 - poly_blep(0) = -1 - (-1)`），所以"起手第一采样"的断言用三角波而不是锯齿。

**测试**：`osc.rs` +2（`a_waveform_switch_is_crossfaded`、`a_waveform_change_before_playback_does_not_fade`），既有 13 个振荡器用例全部照过。

### 鼓组增益改读 dB，双击回 unity（2026-11-05）

**改动只在 UI 层**：`DrumSlot.gain` 仍是线性 0..=2（引擎与工程文件不动），面板把它当 dB 用。

- 每个槽位的 Gain 从 `DragValue`（裸乘数 0.63）换成 `Slider` + `custom_formatter(|v| gain_db_text(v))`，与主输出推子同一个格式化器——因此两处的"0 dB""−∞ dB"字面一致，不会各说各话。
- **双击推子轨道复位到 0 dB**（`gain_response.double_clicked()` → `1.0`）。为什么不是双击数值框：egui 的 `DragValue` 在 `clicked()` 里就 `request_focus`，下一帧变成 `TextEdit`，双击的第二下落在文本框上，`double_clicked()` 永远等不到；`Slider` 的轨道本身不抢点击，双击可靠。写入仍走 `apply_drum_slot_edit`（每槽位+字段一个合并 undo）。
- 套件级输出 Gain 也套上同一格式化器（同样是线性值，别处一致）。

**测试**：`gain_label_tests::linear_gain_reads_in_db`（1.0 → `+0.0 dB`、2.0 → `+6.0 dB`、0.5 → `-6.0 dB`、0 与 1e-9 → `-∞ dB`）。

### Moog 四级 ladder（FilterMode::Moog，2026-11-05）

spec `instrument.md` D14 落地。新文件 `realtime/moog.rs` 放 DSP，`SvfProcessor` 只是多一个 `ladder` 字段和 process 开头的一个分支——2 极的三条通路（LP/HP/BP）一个字节都没动。

- **反馈必须零延迟**。四级 TPT 单极点级联的输出可以写成输入的闭式：`y4 = G⁴·u + σ`，把 `u = x − k·y4` 代进去就解出来了。用一采样延迟的反馈（很多教学实现）在 fc 接近 Nyquist 时相位多转一点就失谐，k 大时直接发散；ZDF 版本在 20 kHz 也稳。
- **饱和放反馈路径而不是每一级**。级联保持线性 → 闭式解继续成立，不用迭代；`|tanh| ≤ |x|` 保证环路增益只减不增，所以"自激"是有界的极限环而不是指数增长。这一条正好就是验收里的"有界自激"。
- **共振映射从线性改成二次**（`k = 4.3·(res/0.98)²`）。第一版是线性的，测试直接抓到通带 RMS 只有 0.51（SVF 是 0.68）：ladder 的直流增益是 `1/(1+k)`，线性映射在 res=0.1 时就给出 k=0.43，掉了 3 dB；res=0.2（默认值）会掉 5 dB，用户一换模式就以为音量被吃了。二次映射 + 输出补 `1 + k/2` 之后，res=0.1 时通带 0.64、res=0.2 时 0.62，切模式基本听不出音量差。
- **只补一半**：全补 `1+k` 会把共振峰也一起抬上去（顶端 k=4.3 就是 5.3 倍增益，直接削顶），而且"高共振时低音变薄"本来就是 Moog 的音色特征。留一半是有意的折中，写进了 §6 已知边界。
- **状态隔离靠 assert_eq 锁**：`a_detour_through_the_ladder_leaves_the_two_pole_modes_bit_exact` 让一个滤波器在 Moog 模式跑 8 个 block 再切回 LP，然后把输出与从没切过的那个逐样本比较——相等才说明 ladder 分支没碰 `ic1/ic2`。这比"看起来没坏"可靠。
- `FilterMode` 多了个变体，`svf.rs` 里那个 `match mode` 必须补一支：写成 `Moog => v2`（退化成 2 极低通）并注明不可达，而不是 `=> input`——万一将来有人把 ladder 分支挪走，宁可音色不对也不要静默直通。
- UI 的 Filter Mode 行从 3 个按钮变 4 个，改成 `horizontal_wrapped` 并给每个按钮加了 hover 说明（12 dB/oct vs 24 dB/oct）——窄一点的窗口会把 Moog 折到第二行，而不是被裁掉。

**测试**：`moog.rs` +5（直流增益、4 极衰减对比、自激有界、全量扫频、双声道状态独立）；`svf.rs` +3（与 2 极的通带/阻带成对比较、Moog 往返逐比特不变、滑块顶端的共振有界）。

### Z1：侧链压缩 Sidechain Comp（2026-11-08）

spec `.agents/spec/sidechain-compressor.md`。Z0 的第一个真实消费者：检测源从节点自己的输入换到 `Mod` 口。

**为什么不改 `Compressor`**：端口剖面是**种类**属性（Z0-D2 的 `params.kind().audio_inputs()`）。把现有压缩器改成双口，旧工程里每个压缩器都会变成 `NodeKind::Dual`，而 D19 规定 Dual 即使只有一条入边也不旁路——旧工程删除压缩器会断链。所以新增 `BuiltinEffectKind::SidechainCompressor`（label `"Sidechain Comp"`），`Compressor` 的参数表、逐样本行为、删除语义保持原样。

**一个 struct 服务两张参数表**：`EffectParamBank` 对越界槽返回 `0.0` / `false`（`bank.rs:58-87`），所以普通压缩器没有 slot 8 时 `is_on(SLOT_SIDECHAIN)` 恒为 `false`，`run` 里一句 `key.filter(|_| self.params.is_on(SLOT_SIDECHAIN))` 就同时覆盖两种种类。参数表用 `const fn sidechain_params()` 在 `BASE_PARAMS` 之上追加第 9 项，避免复制 8 行 `ParamSpec`。

**`process` / `process_with_aux` 收敛到 `run(buffer, key)`**：原来的 `process` 主体整体挪进 `run`，`linked` 分支的 `max(|L|,|R|)` 抽成 `stereo_peak()`，两声道增益应用抽成 `apply_gain()`。侧链分支与 `linked` 分支形状相同，只是峰值来自 `key`。`aux` 可能比 `frames` 短（离线渲染最后一块），`stereo_peak` 用 `get()` 越界读 0，不 panic 不分配。

**静音键控不回落内部检测（修订 Z0 的 Z-D12）**：`aux` 在未接线时是静音缓冲而非 `None`，处理器无法区分「没接线」和「接了静音源」。两者都应当**不压缩**——键控源安静时放行正是侧链要的效果，回落会在 kick 的空隙里抽吸 bass。切换检测源是显式操作：`sidechain` 开关（slot 8），**默认 off**（新建节点按普通压缩器工作，接好键控源后再打开）。

**侧链语义**：检测源 `max(|L|, |R|)` 单探测器，一个 GR 同时作用于两个输出声道（键控是控制信号，不是音频路径）；侧链模式下 `stereo_link` 不参与。增益计算、GR 平滑、makeup、mix、GR 表全部沿用 E2b，`latency_samples()` 仍为 0。

**model 层**：`CompressorParams` 加 `sidechain: bool`（`#[serde(default)]`，即 `false`），`EffectParams::SidechainCompressor(Box<CompressorParams>)` 与 `Compressor` 共享 struct——重复 8 个字段的独立 struct 维护成本更高；普通压缩器上该字段不可达（descriptor 无该槽，Inspector 不显示，`set` 返回 `false`），只是 serde 写出一份无害默认值。参数轴表补 slot 8 → `M::linear(0.0, 1.0)`。

**画布零改动**：`NodeDoc::audio_inputs()` 的既有条件（`voice == Builtin && params.kind().audio_inputs() == 2`）自动把新种类认成双口，`patch.rs` / `patch_ops.rs` 不需要任何分支。

**验收**：8 项压缩器单测（键控跟随 Mod、静音键控逐样本直通、`sidechain = 0` 与普通压缩器逐样本一致、GR 与 program 电平无关、键控不串音、普通表无 slot 8）、1 项图级（`sidechain_compressor_is_keyed_by_the_mod_port`）、1 项 model 校验与 RON 往返（含旧 payload 默认**关闭**键控）、1 项离线端到端（dry / wet / internal 三方 RMS）、`effect_rt_audit` 侧链路径零分配。**零回归**：`aaeb958` worktree + 独立 `CARGO_TARGET_DIR` 编出 pre-Z1 二进制，与 post-Z1 对同一默认工程 `--render --rate 22050 --bits 16 --to 1920`，两份 WAV `cmp` 逐字节一致。

#### Z1 合并修订（2026-11-09）：`Sidechain Comp` 并回 `Compressor`

用户确认两个节点种类重复（sidechain off ≡ 普通压缩器，Z1-D2 有逐样本一致测试锁定），合并实施。spec 修订见 `sidechain-compressor.md` §9，Z-D9 的旁路细化同步登记在 `dual-audio-input.md` §6。

- **model**：`BuiltinEffectKind` / `EffectParams` 删除 `SidechainCompressor`，`Compressor` 加 `#[serde(alias = "sidechain_compressor")]`（旧文件零迁移加载，`sidechain` 键控状态保留，序列化收敛写 `"compressor"`）；`audio_inputs() → 2`；参数轴表 slot 8 并入 Compressor；`as_sidechain_compressor*` 删除。
- **engine**：`COMPRESSOR_PARAMS` 统一 9 项（`SIDECHAIN_PARAMS` / `SIDECHAIN_DESCRIPTOR` / `CompressorProcessor::sidechain()` 删除，UI 用 `with_meters`）；`effect/mod.rs` 名册合并。**删除旁路细化**：`remove_node` 的 `bypassable` 从「排除 Dual」改为「Dual 且无非 Main 口入边（`mod_edges == 0`）→ 可旁路」，配图级测试 `removing_a_dual_node_with_only_main_wired_bypasses_again`；「Mod 有线不旁路」由原测试继续锁定。
- **UI**：`effect.rs` 五处分派合并（Compressor 分支加 `SLOT_SIDECHAIN` 读写）；`patch_ops::delete_graph_node` 同步旁路细化（`mod_wired` 判断）；端口绘制 / Main-Mod 标签由 `audio_inputs()` 推导自动跟随，画布零分支改动。
- **验收**：model `compressor_has_two_ports_and_a_persistent_switch`（含 alias 往返与键控状态保留）、engine `keying_switch_is_the_ninth_slot` / `switch_off_ignores_the_key_input` 与双删除语义测试、UI slot 8 往返断言；`fmt` / `clippy --workspace --all-targets -D warnings` / `test --workspace` 全绿。

### B1：模态打击声源 Mallet（2026-11-08）

spec `.agents/spec/modal-instrument.md`。一个引擎 + 四张预设表（Mallet / Vibraphone / Bell / Singing Bowl），一组二阶谐振器被一次噪声脉冲激励。不是「Synth 换个预设」：频谱非谐波、每个模态各自衰减、note-off 不管尾音。

**为什么用复旋转子而不是两极点递归**：`(re, im)` 每采样 `re' = c·re − s·im`、`im' = s·re + c·im`，衰减**每块乘一次** `exp(−frames/τ)`。教科书形式 `y[n] = 2r·cosθ·y[n−1] − r²·y[n−2]` 里 `r = exp(−1/τ)`，诵钵 26 s T60 时 `r = 0.9999979`，而 f32 在 1.0 附近的分辨率只有 `1.19e−7` —— 衰减率误差能到几个百分点，尾音长度直接失控。块率 exp 同时把 `exp` 移出内层循环（16 模态 × 16 声部 × 375 块/s 可忽略），f32 乘法级联稳定，且单位冲激响应 `|a| = 1`，不需要按 `1/sin θ` 补偿幅度。

**实施期发现的真实缺陷：激励相位必须随机**。第一版把激励只加进 `re`，两次同音敲击相位相同 → 相干抵消，`retrigger_adds_to_the_ringing_state` 实测「第二次敲击反而更弱」（0.6009 → 0.4329）。物理上敲击相对模态相位本来就是任意的，所以每次 `trigger` 抽一个相位 `φ = π·U(−1,1)`，激励按 `(cos φ, sin φ)` 同时注入 `re` / `im`。这条是测试逼出来的，不是设计时想到的。

**T60 与声部释放**：预设表的 `decay_s` 是 −60 dB 时间，`τ = T60 / ln(1000)`；释放阈值 `IDLE_EPS = 1e−3`（也是 −60 dB）与 T60 天然重合，所以「表里写 26 s 就是响 26 s」。模态瞬时频率越过 `0.45·fs` 时该谐振器本块 `on = false`（增益 0、状态冻结），高音区自然只留低次模态，不混叠。

**note 语义是鼓机 K6 的照搬**：`Off{n}`（n ≠ 0）忽略，敲击乐的尾音不该被松键掐断；`Off{0}` 是 blanket 哨兵（Stop / Panic），全池 10 ms 线性 fade。池子 16 声部，同音重击复用同一槽**叠加**激励，池满偷最老声部时**保留谐振器状态、只重算 `c` / `s` / `gain` / `tau`** —— 尾音滑到新音高而不是清零。清零是明确否决的方案，必然 click。

**响度归一**：四个预设的 `Σgain` 差 2.6 倍（1.56 … 4.50），激励幅度按 `1/Σgain · gain · velocity^1.5` 归一，所以切预设不会跳音量。激励长度 `0.5 ms + (1−hardness)² · 8 ms`，低通截止 `300 · 2^(brightness·6)` Hz（上限 `0.45·fs`），包络 `(1 − t)²` 保证尾部无 click；两个参数在 note-on 时冻结，只影响下一次敲击。

**model / 装配 / UI 的接线**：`VoiceSpec::Modal { params: ModalParams }` 加法变体（serde tag `modal`），`ModalPreset` 是 `snake_case` 枚举，`PROJECT_VERSION` 保持 10。装配仍然只改 `headless::spawn_instrument` 一处：离线在构造期 seed `modal_param_values`，实时靠 `engine_sync::push_node_params` 重放（与内置合成器同形）。`NodeParam` 新增 5 个自动化词汇，`preset` 不进自动化（离散值，与 `SynthWaveform` 同例）。画布节点内联 `decay` / `bright` 两行，其余四个参数在 Inspector 的 modal 分区；节点标题 `MALLET · <preset> · Track N`。

**两个测试口径的坑**（都踩过）：

- `decay_scale` 的对比必须用 Bowl。用 Mallet 比「5 s 后的能量」时两个值都是 0（T60 只有 1.2 s），测试会以 `0 vs 0` 通过 —— 那是假绿。
- 拍频断言不能用 512 帧的短窗 RMS：窗里是数百 Hz 的模态内振荡，包络起伏被完全淹没（实测 0.39 vs 0.30，看着像拍频其实不是）。改用 250 ms 窗的慢包络，把模态振荡平均掉，只留 1–2 Hz 的失谐拍频。

**验收**：15 项 `modal.rs` 单测（静音、预设表形状、原子起音、10 s 后仍有能量、尾音释放槽位、note-off 不掐尾 + blanket 收尾、重击叠加、非谐波峰 Goertzel、拍频、decay 缩放、brightness 频谱、偷声部有界、极端参数有限、参数 clamp、零力度不敲）、1 项 model 往返与旧 payload 默认、`effect_rt_audit` 的 `process` / `note_event` / `set_param` 零分配、1 项离线端到端（Bowl 预设 6 s 渲染，末四分之一仍有尾音能量）。

### Z2：16 带 Vocoder（2026-11-08）

spec `.agents/spec/vocoder.md`。`Main` = carrier、`Mod` = modulator，16 个固定对数带，两路各一套 Simper SVF 带通；分析侧整流 + attack/release 一极点得到逐带包络，合成侧用同一带组滤 carrier 再乘包络求和，外加一条带组之上的噪声旁路。

**带通取 `k · v1`，不是 `v1`。** SVF 的原始带通抽头峰值是 `1/k = Q`，而 vocoder 天生是两级串联（分析 + 合成），两边都用原始抽头的话输出电平随 `Q²` 走——用户拧 `q` 会听到响度而不是选择性。`k · v1` 归一成单位峰值后，`q` 只改选择性。代价是带组不再「重构原信号」（带宽变窄时总能量下降），这正是 vocoder 该有的行为。实测两个满幅正弦（同频、落在带中心）RMS ≈ 0.62（−4 dB），差的那部分是包络的整流均值损失；`matched_sines_land_near_unity` 把这条钉住，否则以后有人「顺手去掉那个多余的 `k`」，只会发现声音变大而不明白为什么。

**包络一极点必须有硬地板。** f32 的一极点会停在目标上方几个 1e−5 不动（`Smoothed` 里的「no-progress snap」是同一现象的另一面）。在 compressor 里那点残量只是让 GR 差 0.0001 dB，在 vocoder 里它会把 carrier 永远乘出去——「Mod 口静音」就永远到不了零。`ENV_FLOOR = 1e−6` 直接归零，`envelope_follower_snaps_to_zero` 锁住。

**清浊不做分类器。** 原本的说法是「逐带包络 + 清浊处理」，落地时改成**频段划界**：modulator 过 `max(high, 4 kHz)` 的二阶高通，用固定 2 ms / 40 ms 的包络去乘确定性噪声。理由有两条：① 带组之上的能量本来就是「带组盖不到」的那部分，按频率切不需要判断；② 音高检测式的浊音 / 清音分类一旦误判，噪声会忽开忽关，比不做更刺耳。旁路用自己的固定包络而不是共享 `release`——共享的话慢 release 会把齿音拖成一片「嘶」。

**参数变化不重置状态。** 每块重算 16 组 `g = tan(πf/sr)`（一次 `powf` 算带间距，其余靠连乘），但 SVF 的 `ic1`/`ic2` 与包络继续跑。拖 `q` / `low` / `high` 时可能有一个小瞬态，但不会像「重建滤波器组」那样清空尾音。`low` / `high` 范围重叠，用户可以把 `high` 拖到等于 `low`，DSP 侧强制 `high = max(high, 2·low)`（16 个带叠在同一个中心频率上会共振爆炸）。

**Mix 与 air 的两处细节**：`mix = 0` 时仍然跑完整 DSP，只让输出退化成 `carrier · 1 + wet · 0`——短路它会让重新打开 mix 的第一块是冷启动。air 探测器**始终运行**（即使 `sibilance = 0`），否则包络会停留在很久以前的值，一拧上去先来一记噪声爆音。

**三个踩过的坑**：

- NaN 防御写成 `if !(freq > 0.0) || …` 会被 clippy 的 `neg_cmp_op_on_partial_ord` 拒（clippy 认为这种写法掩盖了不可比性）；改 `!freq.is_finite() || freq <= 0.0`。
- `low_and_high_move_the_bank` 第一版只断言「覆盖时 RMS 是未覆盖时的 10 倍」——两个数都可能是 1e−9 的假绿。补了 `included > 0.1` 的绝对下限，顺手把阈值收紧到 0.15 倍。
- 离线端到端测试沿用 `"dry"` / `"wet"` 作临时文件名会与 Z0 / Z1 的同名测试撞车（`save_temp` 只按 tag + process id 区分），单独跑必过、全量跑偶发 `拒绝访问`。改用 `z2_dry` / `z2_wet`。

**验收**：18 项 `vocoder.rs` 单测（描述符、单位峰值 × 三个 Q、静音、缺 aux、直通、门控包络、起音、频率选择、带边界、Q、sibilance、声道独立、极端参数、NaN 参数、可复现、release、地板、电平标定）、1 项 model RON 往返与旧 payload 默认、`effect_rt_audit` 的 `process_with_aux`/`set_param` 零分配、1 项离线端到端（无 Mod 线 → 静音；接线 → 出声）。全量 1136 项通过，默认工程渲染与 pre-Z2 逐字节一致。

### S1：拨弦声源 Pluck（2026-11-08）

spec `.agents/spec/pluck-instrument.md`。Karplus-Strong：一根分数延迟的环，环里串一个一阶低通当阻尼，一圈噪声当激励。与 B1 同属「激励—共鸣」，但没有一行 DSP 可以共用——B1 是并联谐振器组的非谐波谱，S1 是严格谐波谱、所有谐波共享一个环路增益。所以是独立模块，`realtime/pluck.rs`。

**环路增益不是 T60 增益。** `g_target = exp(−ln1000 / (f0·decay_s))` 只描述「一个谐波衰减到 −60 dB」；但环里还串着阻尼低通，它在 f0 处的幅度 `|H(f0)| = a / sqrt(1 − 2(1−a)cosω + (1−a)²)` 小于 1，会额外吃掉能量。直接写 `g = g_target` 的话，`decay` 在重阻尼下会被压短一半以上。所以写入的是 `g = min(0.99995, g_target / |H(f0)|)`：`decay` 描述的是**基频的 T60**，高次谐波由阻尼自然先死。`0.99995` 的硬上限意味着「阻尼很重时尾音短于 `decay`」——这时阻尼赢，是物理正确的，不是缺陷。

**note-off 阻尼必须乘在环里，不能挂在输出上。** 第一版是输出端 per-sample 指数淡出（`fade *= fade_coeff`，T60 = `release_s`），实测 `release = 0.15` 时松键 0.1 s 内 RMS 归零。原因很直白：淡出是逐样本复合的，而一个环路周期有 `D ≈ 436` 个样本（A4 @48 kHz），也就是这个「每样本一次」的衰减在一圈内被乘了 436 次。改成环路增益 `damp_gain = exp(−ln1000·D / (release_s·fs))`，一圈正好 −60 dB。同一个 bug 还让 `decay` 的 T60 实测值偏了 7 倍（42 / 37 而不是 5.6），因为阻尼和激励电平两个误差叠在一起——把 `EXCITATION_LEVEL` 从 0.35 提到 0.8（A4 峰值约 0.49）后尾音才不再提前落进 `IDLE_EPS`。

**第一个环路周期是静音。** 激励要走完一圈才到输出：D = 2 时 0.04 ms，A4 时 2.3 ms，MIDI 0 @48 kHz 时 122 ms。这是 Karplus-Strong 的固有性质，不是延迟缺陷，所以不「修复」。读写顺序保证 `read` 永远是 `D` 个样本之前写入的值（分数延迟线性插值读在写之前），环路周期严格等于 `D`，没有隐含的额外样本延迟。

**`retune` 必须吃 `pitch_ratio`。** 第一版 `retune` 只 clamp 已有的 `delay`，于是 `PitchOffset` 改不动已经在响的声部——拨弦变成「只有下一个音才跟」。改成从 `voice.note` + `note_to_frequency` 重建 `delay`，`PitchOffset` 立即作用于全池。

**没有预设枚举，这是刻意的。** B1 的预设表是结构性的（模态比 / 每模态衰减，参数化不了），Pluck 的「木吉他 / 贝斯 / 竖琴」只是五个标量的组合——预设会立刻被滑杆改脏，还多出「我现在在哪个预设里」的状态问题。五个宏参数直接给用户（`decay` / `damping` / `brightness` / `release` / `gain`）。

**几个常量与它们的理由**：`MAX_DELAY = 16384`（96 kHz 覆盖 MIDI 0；48 kHz 到 2.9 Hz）、`MAX_VOICES = 16`（每节点固定 1 MiB，构造期分配）、`MAX_EXCITATION_FRAMES = 4096`、`SCRATCH_FRAMES = 1024`、`IDLE_EPS = 1e−6`（1e−4 会让尾音提前判死）、`GAIN_CEILING = 0.99995`。激励噪声是 xorshift32——确定性，所以离线渲染可复现，两个新实例对同一事件序列逐样本相同。

**model / 装配 / UI 的接线**：`VoiceSpec::Pluck { params: PluckParams }` 加法变体（serde tag `pluck`，`#[serde(default)]`），`PROJECT_VERSION` 保持 10，旧工程零迁移。装配仍然只改 `headless::spawn_instrument` 一处：离线在构造期 seed `pluck_param_values`，实时靠 `engine_sync::push_node_params` 重放。`NodeParam` 新增 5 个自动化词汇（全部连续量，全可自动化，与 B1 的 `preset` 不同），`MapSpec` 的 `decay` 0.1..20 / `damping`·`brightness` 0..1 / `release` 0.005..2 / `gain` 0..2 与 `node_param_map` 对齐。画布节点内联 `decay` / `damp` 两行，其余三个在 Inspector 的 pluck 分区；节点标题 `PLUCK · Track N`。`voices.rs` 四处分派补 `Pluck => None`（无外部文件）。

**验收**：16 项 `pluck.rs` 单测（静音、发声、音高与 `PitchOffset` 的 Goertzel、T60 偏差、decay 尾音长度、damping 明暗、brightness 频谱、note-off 阻尼、blanket、重击叠加、偷声部有界、非有限参数、极端参数、零力度、可复现、非法采样率）、1 项 model RON 往返与旧 payload 默认、`effect_rt_audit` 的 `process` / `note_event` / `set_param` 零分配、1 项离线端到端（拨弦窗口 RMS 0.061，松键后窗口 0.000）。默认工程渲染与 pre-S1 逐字节一致（44288 帧 / 177196 字节），全量 1154 项通过。

### S2：拉杆风琴 Organ（2026-11-09）

spec `.agents/spec/organ-instrument.md`。九个严格谐波分量的加权和，听上去是最没有算法风险的一类声源——难点确实不在 DSP，在**相位管理**与**参数面**。因此 `realtime/organ.rs` 的实现比 `pluck.rs` 短，测试却更长（19 项）。

**9 个 drawbar 必须共享一个相位累加器。** 直觉做法是每根 bar 一个累加器（9 个 `OscProcessor` 或 9 个 `phase` 字段），每根各自 `exp2` 出频率。这在数学上等价，在数值上不等价：它们是 9 段互相独立的浮点累加，误差各自长，分钟级尺度上会互相滑动。16'（0.5·f0）与 8'（f0）本来就是八度关系、拍频为零，漂移一出现就变成可闻的慢速拍——这是听感上立刻能发现的错，不是「理论上不严谨」。所以基准取 **16'**（`dp = 0.5·f0/fs`），9 根 bar 相对 8' 的比值 0.5 / 1.5 / 1 / 2 / 3 / 4 / 5 / 6 / 8 全是 `0.5·f0` 的整数倍，第 k 根就是 `sin(TAU · fract(k·p))`，`k ∈ {1, 3, 2, 4, 6, 8, 10, 12, 16}`。一个 `p` 保证九个分量恒有固定的相位关系，还省掉 8 份状态。

**`L = 0` 必须特判为精确 0。** 电平阶梯按 `10^(−0.15·(8−L))` 算，`L = 0` 得到的是 `10^(−1.2) ≈ 0.063`（−24 dB）——不是「关掉」。九根全归零时这 9 个 −24 dB 的残留仍会叠出可闻的糊音，而「把所有拉杆推回去」是风琴最基本的动作。所以 `LEVEL_GAIN[0] = 0.0` 硬编码，`L = 0` 的 bar 连 `sin` 都不算（掩码在 `configure` 里，`on[index] = gain > 0`）。

**`OUTPUT_SCALE = 0.25` 是响度折中，不做归一化。** 默认注册 `888000000` 的 `Σ = 3.0`，乘 0.25 后单声部峰值约 0.75，与内置合成器的单位振荡器同一量级；九根全开时峰值 2.25，会顶到 limiter——这在真机上也一样（拉满更响）。不做「按开着的 bar 数自动归一」是刻意的：归一化会让「加一根 bar」听不出变化，那比削波更糟。

**A/R 直接复用 `AdsrProcessor`，`decay = 0`、`sustain = 1.0`。** 风琴没有 ADSR，但键开关必须有斜坡，否则每个音的头尾都是 click。选 `AdsrProcessor` 而不是写一个线性斜坡，关键收益是 `trigger` **从当前电平重新起音**——偷声部和同音重按天然无 click，不需要额外的淡出淡入逻辑。默认 `attack = 5 ms` / `release = 40 ms`。

**`held` 与 `note` 必须分成两个字段。** 第一版只有 `note: Option<u8>`：note-off 时清成 `None`，声部靠包络自然衰减。结果松键后的尾音**全部掉到 f0 上**——因为音高是从 `note` 算的，`note` 一没了，`base_inc` 就成了「无音高」的兜底值。改成 `note: Option<u8>` 保留到声部真正空闲 + `held: bool` 单独表示「键还按着」，尾音才在正确的音高上跑完。`held` 同时给 `Off{0}` 的 blanket 语义用。

**力度不参与音量，但 `velocity == 0` 仍然不发声。** 真实音轮风琴的键触点就是开关，`trigger` 恒传 `1.0`；响度来源是注册 + `gain`，这是乐器的性格。但 `velocity = 0` 必须静音，一是与另外两个引擎一致，二是兼容「velocity 0 当 note-off」的 MIDI 习惯。

**打击泛音加在总包络之前。** 它是一根额外的正弦（2nd → `k = 4`（4'），3rd → `k = 6`（2⅔'）），带自己的指数衰减：`perc -= perc·c`，`c = 1 − exp(−ln1000/(T60·fs))`，逐样本一极点（比块率乘法少一级台阶，长 T60 下不会有 zipper）。电平冻结在 note-on（中途改 `percussion` 不影响已经在响的打击音），`percussion = 0` 时整段 `if` 跳过、零成本。**为什么在包络之前**：键松开时打击音要跟着一起被关掉，这是真机行为；如果加在包络之后，松键的瞬间会漏出一小段干净的打击音。

**Nyquist 掩码用 `max(当前, 目标)` 的 pitch ratio。** 某根 bar 令 `k · 0.5 · f0 ≥ 0.45·fs` 时本块不参与。判定若只看当前音高，滑音往上冲的过程中会有一块「刚越界但还没更新」的样本段；取两者较大值就不会瞬时越界。高音区上方 bar 自然消失（音色变薄），v1 不做 foldback——真机在那个位置是折回低八度，但那是每根 bar 一条额外的频率映射，且与「注册决定音色」的直觉相反。

**`OrganBar(u8)` 是一个 payload 变体，不是 9 个枚举变体。** 与既有的 `DrumSlotGain(u8)` 同例：`set_param` 里只有一条分支，`NodeParam` / `node_param_map` / 自动化标签也各只有一处，代价是编译期无法穷尽检查索引——由 `set_param` 里 `index >= 9 → false` 守住。自动化词汇里 `OrganAttack` / `OrganRelease` **不复用** `NodeParam::Attack` / `Release`：后者的 `MapSpec` 是 0..2 / 0..3，而风琴的合法区是 0.001..0.2 / 0.005..1.0，共用会让曲线 90% 的行程落在 clamp 上。

**测试口径的坑**：用默认 `888000000` 测 `PitchOffset` 是测不出来的——+12 半音只是把 16' 挪到了原来的 8' 位置，注册本身掩盖了偏移。测偏移前必须先 `silence_bars` 再只开 8'。另一个坑是 Nyquist 断言不能写「很小」：掩码生效时是**精确 0**，`1'` 在 C8 上实测峰值 0.000000，写成 `> 0.001` 之类的宽松阈值会让「漏掉掩码」和「掩码偏严」都通过。

**model / 装配 / UI 的接线**：`VoiceSpec::Organ { params: OrganParams }` 加法变体（serde tag `organ`，`#[serde(default)]`），`PROJECT_VERSION` 保持 10。装配仍然只改 `headless::spawn_instrument` 一处：离线在构造期 seed `organ_param_values`（9 根 bar + 6 个宏共 15 条），实时靠 `engine_sync::push_node_params` 重放。`NodeParam` 新增 14 个词汇（9 根 bar + 打击三件套 + A/R + gain），打击谐波不进自动化（离散值，与 `ModalPreset` / `SynthWaveform` 同例）。UI 的 footage 字符串只有一处来源（`ORGAN_FOOTAGES`），选择器 / Inspector 网格 / 自动化菜单 / 内联行全引用它，避免 `2⅔'` 这种带 unicode 分数字符的标签被抄成两份后不一致。节点标题 `ORGAN · Track N`，画布内联 `8'`（drawbar 2）与 `perc`（最常拧的两个），Inspector 是 3×3 的 drawbar 网格 + 打击三件套 + A/R + gain。

**验收**：19 项 `organ.rs` 单测（静音、发声、音高与 `PitchOffset` 的 Goertzel、频谱由 drawbar 决定、16' 次八度、3 dB 阶梯、全归零静音、A·R、blanket、打击泛音与谐波切换、同音复用、偷声部有界、Nyquist、非有限参数、极端参数、零力度、可复现、非法采样率）、1 项 model RON 往返与旧 payload 默认、`effect_rt_audit` 的 `process` / `note_event` / `set_param` 零分配、1 项离线端到端（held RMS 0.42235，松键后 0.00000）。实测：默认注册 RMS 0.3062 / 峰值 0.6249；只开 8' 时 f0 / 2f0 = 1 689 272；`8 → 4` 电平比 0.251（−12.0 dB）、`8 → 1` 为 0.089；打击早/晚能量比 1 479 044；`1'` 在 C8 上峰值 0.000000。默认工程渲染与 pre-S2 逐字节一致（44288 帧 / 177196 字节），全量 1175 项通过。

### S3：波表合成 Wavetable（2026-11-09）

spec `.agents/spec/wavetable-instrument.md`。前面四个声源的谐波要么实时算（Organ 的 9 根 bar）、要么由物理模型自然长出来（Modal / Pluck），这一项第一次把**一整个数据层**塞进引擎：表从哪来、装多少、怎么抗混叠、怎么在不分配的前提下换。DSP 本身只有「一次查表 + 三次 lerp」。

**表由谐波谱生成，不是手工画点。** 4 帧锚点音色各写一条 `(k, amp)` 定义：Sine 只有 `k=1`；Triangle 奇次 `(−1)^((k−1)/2)/k²`；Saw 全次 `1/k`；Square 奇次 `1/k`。`amp` 带符号，等于把相位 π 烘进表里——不需要 cos 表，也不需要 `phase_offset` 字段。这个选择的真正价值在带限：想做「只保留到 H 次谐波」的版本，只需把求和上限从 `BASE_HARMONICS` 换成 `H`，一行代码，不用 FFT、不用滤波、不用过采样。手工画点做不到这件事，因为画出来的点没有「第几阶谐波」这个维度。

**每八度一层，共 9 层（256 / 128 / … / 1 次谐波）。** 固定波的谐波数恒定，f0 升高时高次谐波越 Nyquist 折回成非谐波杂音——这是波表唯一真正的技术风险。选层按 `max_k = 0.45·fs/(f0·ratio)` 取**最小**的 L 使 `256 >> L ≤ max_k`，连 `H(8) = 1` 都不满足（基频自己越过 0.45·fs）则该声部静音。每八度一层是最小够用粒度：再多只省内存不改善音质，再少会让高音区过早丢谐波。`ratio` 取 `max(当前, 目标)`，与 Organ 的 Nyquist 掩码同一规则，滑音途中不瞬时越界。选层在**块率**完成、硬切、不做层间交叉淡化——被丢掉的谐波都在 `(0.225·fs, 0.45·fs]`，且幅度按 `1/k` 已经很小；交叉淡化要把查表次数翻倍（每样本 8 次），听感上换不回来。`crossing_a_layer_boundary_stays_continuous` 用「接缝处的最大样本步长 vs 两侧稳态的最大样本步长」锁住这件事，实测 A4 跨 L3→L4 是 **1.046×**——接缝的步长恰好等于信号自己在该音高上的斜率，没有第三种成分。

**同一帧的所有层共用一个归一化系数，这是整个模块最重要的一行。** 系数取该帧在 L=0 下的峰值倒数，所有层乘同一个数。若按层各自归一，弹到高音区会**突然变响**：窄层里只剩几个谐波，归一化把每个都抬到接近峰值。这是这类引擎最常见也最难查的 bug（听感上是「高音区整体偏响」，而单听任何一个音都正常），`level_switch_keeps_the_level` 用 A1 / A6 的 RMS 比（实测 1.790，窗口 0.5..2.0）把它钉住。

**归一化用的是截断后波形的峰值，所以窄层合法超过 1。** 方波带限到 256 次谐波时 Gibbs 过冲到 ≈1.18，系数因此是 ≈0.85；只留基频的那一层乘完到 **1.0799**。这不是余量，是物理后果——`the_bank_is_complete` 的上限写 1.4 而不是 1.05，注释里也照实写 Gibbs，不然下一个人会把它当成为保险留的空白。

**表用正弦查表生成，零三角函数。** `sin(2π·k·i/N)` 在 `i` 整数、`N` 为 2 的幂时正好落在同一张 `N` 点正弦表的网格上：`SIN_LUT[(k·i) & (N−1)]`，数学上精确而非近似。这把构造期的 800 万次 `sin` 换成 800 万次乘加，实测建表 **6.4 ms（release）/ 106 ms（debug）**。表本身是进程级 `OnceLock<Bank>`（`4 × 9 × 4096 × 4 B = 576 KiB`），全进程一份，节点只持 `&'static` 切片——理由不只是省内存：24 个单测每个都 `new` 一遍，不共享会让测试慢到不可接受。`process` 里因此既没有分配也没有锁，而且**每样本成本与表里的谐波数无关**（4 次查表 + 3 次 lerp），这正是波表相对加法合成的性价比所在。

**力度白拿。** `AdsrProcessor` 的峰值本来就是 velocity（`trigger` 把 level clamp 进 `[0, velocity]` 并从当前电平起音），所以把真实 velocity 传进去即可：`velocity = 0.5` 的稳态 RMS 实测是 `velocity = 1.0` 的 **0.500**，不需要额外乘法链，重按 / 偷声部仍然无 click。这一条与 Organ 刻意相反——音轮风琴的键触点是开关，波表是键盘合成器，`velocity → 电平` 是它的自然语义。

**`position` 是唯一新增的平滑状态。** 它是块率下发的参数，硬切会在自动化扫动时产生 zipper；每样本一极点（10 ms）把阶梯变成斜坡。`position → 帧` 的映射踩过一次坑：第一版把 `FRAMES - 2` 当 `span` 用，等于把 4 帧压成 3 帧，`position = 1` 只到锯齿而到不了方波，三个频谱测试同时红。正确写法是两个常量——`span = FRAMES - 1`、`last_index = FRAMES - 2`，`f = position·span`、`j = min(floor(f), last_index)`——并且 `render_voice` 与 `preview_waveform` 两处必须同时改。

**Inspector 的波形预览不读播放用的表。** 播放表的分层按音高选，而预览没有音高（它画的是一周期形状），读哪一层都是错的。所以 `preview_waveform` 用同一套 `frame_amplitude` 重新求和，带限到 `out.len()/2`，并复用 `bank.scales` 保持与播放一致的归一化——共用同一份谱定义与同一组系数，预览就不会随实现漂。这也解释了为什么它是 `pub` 的控制线程函数而不是 RT 路径的一部分。

**model / 装配 / UI 的接线**：`VoiceSpec::Wavetable { params: WavetableParams }` 加法变体（serde tag `wavetable`，`#[serde(default)]`），`PROJECT_VERSION` 保持 10，旧工程零迁移。因为表是内置的，不存在「空路径」态，节点出生即有声——这是它比 Sample / SoundFont 简单的地方。装配仍然只改 `headless::spawn_instrument` 一处：离线在构造期 seed `wavetable_param_values`（4 条），实时靠 `engine_sync::push_node_params` 重放。`NodeParam` 新增 4 个词汇，全部连续、全部可自动化（扫 `position` 就是这个乐器的核心演奏动作，必须能挂曲线）；`WavetablePosition` / `Attack` / `Release` **不复用** `NodeParam::Attack` / `Release`，原因同 S2（合法区 0.001..2 / 0.005..5，共用会让曲线大半行程落在 clamp 上）。UI 加选择器第 8 个内置乐器（Add 菜单在 `Organ` 之后）、节点标题 `WTABLE · Track N`、画布内联 `pos`、Inspector wavetable 分区（一个周期的波形预览 + Position / Attack / Release / Gain 四行滑杆）。

**v1 边界**：不做用户表导入（`.wav` / `.wt`、命名、缓存淘汰、文件对话 UI），不做滤波器、不做 unison / detune / FM。用户导入是「文件 + UI」的活，风险不在 DSP，真要做得按 `media` / `sf2_library` 的形状单独写 spec；滤波器接既有效果器即可，塞进引擎会让「表决定音色」这件事变模糊，也会给 `--render` 的频谱验收多一个变量。

**验收**：24 项 `wavetable.rs` 单测（静音、发声、音高与 `PitchOffset` 的 Goertzel、position 0 纯正弦、position 1 方波奇次、扫动频谱单调、混叠防御、低音保留更多谐波、选层随音高单调、跨层响度不变、层边界连续、A·R、blanket、力度线性、零力度、同音复用、偷声部有界、position 平滑、非有限参数、极端参数、可复现、非法采样率、预览跟随 position、表完整）、1 项 model RON 往返与旧 payload 默认、`effect_rt_audit` 的 wavetable 零分配段、1 项离线端到端。实测：默认（正弦帧）RMS 0.2475 / 峰值 0.3500；f0/2f0 = 269 723、f0/3f0 = 2 511 366；方波 3f0/2f0 = 29 551；扫动 4f0 = 0 / 0 / 0.02370（⅓ 处为 0，三角帧只有奇次）；A6 锯齿 8f0/9f0 = 138 264；A1 / A6 RMS 比 1.790；velocity 0.5 / 1.0 = 0.500；层接缝步长 0.478 对稳态 0.457（1.046×）。默认工程渲染与 pre-S3 逐字节一致（44288 帧 / 177196 字节），全量 1208 项通过。

## 录音与 headless I/O

### 录音输入 UI 归位（R4/R5/R9，2026-11-03）

**设备与电平不再挂在走带条。** 走带条只保留"这次怎么录"的会话设置（pre-roll、`Mute others`、手动 `Latency`）；录音源与电平归到它们描述的实体上：

- `Audio In` 节点（`NodeDoc::Input`）底部新增两行信息区：设备 chip（点击弹出录音源菜单，含系统默认 / Inputs / System playback 两组 + Refresh）和该节点所选 channel 的实时峰值条。节点高度由 `size_of` 的 `INPUT_INFO_ROWS` 预留；菜单打开时用 `PatchState.input_sources` 缓存一次枚举结果（枚举会打开驱动，不能每帧调用）。
- Inspector 选中 `Audio In` 时显示 `input_section`：设备菜单、流状态（设备名 / 通道数 / 采样率 / loopback / 错误）、该 channel 的 peak、Ch 1/Ch 2 选择、输出 gain。`resolve_panel_target` / `opens_inspector` 已把 `NodeDoc::Input` 加入可编辑白名单。
- `NodeDoc::Input.channel/gain` 之前只有模型字段、没有 UI 入口；现在节点内联（`InlineParam::InputChannel/InputGain`）和 Inspector 都会写文档并经 `ParamId::InputChannel/InputGain` 下发引擎。channel 行是 combo（Ch 1 / Ch 2），与 SF2 bank/patch 共用 `pending_choice` 弹窗。
- **电平按通道**：`InputStats` 的 `peaks: [AtomicU32; 2]` 由 cpal 回调按 `i % channels` 分通道写入（栈数组 + relaxed 原子，仍满足实时铁律），`peak_of(ch)` 供节点/Inspector 读；`peak()` 返回两通道最大值。

**为什么 latency 留在走带条**：它是 take 的对齐偏移（正数把音频 clip 起点提前），依赖当前设备与缓冲大小，会话级、不随工程保存；放在走带条与 pre-roll / `Mute others` 同属"本次录音怎么采"，挂到工程节点上会被误读成节点参数。控件已加 `Latency` 标签与完整 hover 说明。

### 音频输出 Preferences（S1-5，2026-11-03）

**引擎侧只加一个入口**：`AudioEngine::start_with(OutputConfig { device, sample_rate, buffer_size })`，`start()` 就是 `OutputConfig::default()`。`device = None` 用系统默认；指定名字找不到时是 `UnknownOutputDevice`（启动路径回落默认并提示）；指定采样率只在设备的 f32 配置里找，找不优先挑与默认配置相同声道数的，其次 2 声道，最后最窄的（引擎只渲染立体声，8 声道端点只会多出 6 条静音轨）；`buffer_size` 直接映射 `cpal::BufferSize::Fixed`。

**运行时切换 = 换引擎，不是换流**：`AudioEngine` 的 `BlockRunner` 已经 move 进 cpal 回调闭包，拿不回来。`apply_output_config` 先构造新引擎，成功后才 `mem::replace` 并 `drop` 旧引擎（旧 stream 停止、旧图随之销毁），随后清掉 `voice_guards` / `retiring_guards`（旧引擎不会回回收执，留着会泄漏插件库），再 `apply_document()` 按新采样率重建整张图并重开输入流。失败时旧引擎继续跑，错误留在 Preferences 窗口里，不弹全局对话框。

**配置存在 `UiState`**（`output_device` / `output_sample_rate` / `output_buffer_size`，全部 `#[serde(default)]`）：`save()` 写回，启动时 `GlitcherZApp::new` 读出来——只有显式设置过且与当前引擎不符才重建，普通启动不会重复打开默认设备；保存的设备不可用时用默认引擎启动并在状态栏说明原因。

**掉线提示**：cpal error 回调（不在数据路径）把错误文本写进 `Arc<Mutex<Option<String>>>`，UI 每帧 `take_stream_error()` 并显示红色状态栏（"Audio output stopped: … Open Edit ▸ Preferences…"），同一个字符串覆盖写，不会堆积。Preferences 窗口也显示最近一次流错误。

**测试**：`output_config_matches` 四态（默认任意匹配 / 设备 / 采样率 / 缓冲的不匹配）、`UiState` RON 往返；`cargo run -p glitcherz-app -- --seconds 5` 冒烟覆盖默认启动路径。真实设备/采样率切换需要人工点 Apply（headless 无法验证）。

### 导出规格化（S3c，2026-11-03）

**格式由 `WavFormat { sample_rate, bits }` 决定，唯一的换算点在 `Bits::wav_spec`**：16/24 走 `SampleFormat::Int`（24-bit 用 i32 载值、hound 写 3 字节），32f 走 `SampleFormat::Float`。16/24 先 `clamp(-1,1)` 并拦 NaN（`finite_or_silence`）；32f 只拦 NaN/Inf，保留引擎自己的 overshoot —— 这正是选 float 交付的意义。

**采样率必须跟着离线引擎走**：`spawn_offline_node` 以前读 `self.engine.info().sample_rate`，于是"导出采样率"这件事根本不存在。现在它显式收 `export_rate`，`start_export` 用它开 `start_offline(rate)` —— 96 kHz 导出不再需要 96 kHz 设备。音频 clip 是时间制播放（`elapsed × wav_sr`），换 rate 不会变调。

**stems = 同一张图、多个 pass**：`ExportPass { path, label, target }`，`target: Option<TrackSel>`（`None` = 整混音）。`pump` 里 pass 的生命周期是显式的：

```
pass 写完尾音 → finishing → close_pass（finalize + 记 outputs）
             → 还有 pass？index+1，flush_left = 2 s 的块数
flush 阶段：渲染但丢弃（上一 pass 的 reverb/delay 尾巴）
flush 归零 → writer 为 None → begin_pass：开新 writer → setup(engine, node_map, target)
           → seek(window.0) → Play → tail_left 复位
```

`setup` 由 app 提供（`set_export_pass_tables`）：目标轨 `rt_clips` / `build_audio_clip_table`，其它轨推 `empty_midi_clip_table()` / `empty_clip_table()`。**不重装图**——重装等于每轨一次 CLAP/VST3 实例化 + state 回灌；改址只是几次结构队列推送。control 轨（automation / tempo）全程保留：它们属于"这个 stem 听起来的样子"。

**`pass_finished` 必须要求 writer 存在**：初版只判 `!is_playing() && tail_left == 0`，flush 结束后这个条件立刻成立，于是 pass 被"关闭"而从未开始 —— 三个 pass 只写一个文件。是 3-pass 渲染测试抓到的（`setup_calls` 为空）。

**取消**：删掉 `job.path()` 对应的未完成文件（不在 `outputs` 里即是半成品），已完成 pass 保留并在状态栏报数量。半成品 wav 看起来像成品，比没有更糟。

**命名**：`sanitize_file_part` 把 `/ \ : * ? " < > |` 与控制码换成 `_`、折叠连续空格、去掉尾部点与空格（Windows 会静默裁掉，导致写出的路径和记录的不一致），空名回落 `track`；重名追加 ` 2`、` 3`。stem 文件固定落在用户选的那个目录（`mix.with_file_name`）。

**测试**（+4）：`bit_depths_match_the_wav_spec`、`the_device_rate_leads_the_sample_rate_choices`、`stem_names_survive_hostile_track_names`、`a_multi_pass_job_writes_every_pass_in_the_requested_format`（真离线引擎跑完 3 个 pass，读回三个 24-bit @22050 文件，并断言 setup 只对两个 stem 触发）。

### Export Audio 离线渲染（U7，spec `export-audio.md`，2026-10-24）

**离线引擎 = 同一装配减去 cpal**：`AudioEngine::start_offline(sample_rate)` 与 `start()` 唯一的差别是不建流——`_stream: Option<Stream>` + `runner: Option<BlockRunner>`（实时把 runner move 进闭包后字段留 `None`；离线留 `Some`，`render_offline_block` 转发 `render_interleaved`）。无设备依赖 → headless/CI 可跑。装配/命令/结构队列全部复用，离线引擎的高层 API（add_node/connect_nodes/set_auto_route/send_audio_clips…）与 live 完全同路径。

**`SetArrangementFloor`（本 feature 新增的引擎命令）**：D16 自停的 `arrangement_end` 只统计 MIDI/control clip（`refresh_arrangement_end`），audio clip 终点更晚时自停先发生 → `transport_active=false` → AudioTrackProcessor 静音切断。floor 让 `arrangement_end = max(内容, floor)`。live 引擎永不推（零影响）；导出装配推 `export_end_ticks`（全 clip 终点 max）。**任何"sequencer 内容之外还有时间轴内容"的后续特性（录音？）都要重访这个坑**。

**Play 的 `note_off_all` 吞同块 note**：`apply_command(Play)` 会群发 blanket off——FIFO 队列里 Play 之前发的手动 Note 在**同块**被消费：note 触发 envelope → 紧跟 note_off_all → release 从 level≈0 开始 = 瞬间静音。**导出无此坑**（音符全部来自 clip 调度，advance 在 Play 生效后的块里跑）；`offline_engine` 测试的时序注释锁定该认知。手动 note 测试必须：装配块 → Play → **再渲染一块** → Note。

**离线装配 = `spawn_offline_node`（不是 install_node 的参数化）**：install_node 一半代码是 UI 缓存记账（clap_voice_params/voice_default_state/voice_guards——**离线 guard 混入主列表会被 engine_id 撞号误回收**）。独立 spawn 只走声音路径（构造/spawn+load_state/构造期 set_param），guard 收集到 job 本地的 `Vec`，随 job drop 统一释放。VST3 的 `guard.load_state` 是 `&self`（StateGate 内部处理）、CLAP 是 `&mut`——`Ok(instance)` 的 mut 两家不同。

**帧间分片**（U9 扫描先例）：帧尾 `pump_export` 渲染 34 块（~90ms 音频）+ `request_repaint`；进度 = `musical_ticks / (end-start)`；到达终点 `pause()`（停在 end，tail 期间 transport_active=false 只放效果尾——audio 内容按设计止于 end）。空工程 window=(0,0)：Play 后立即 pause，只渲染 1s 尾音（空文件带尾）。Cancel 直接 drop job（半成品文件留给用户）。**writer finalize 才写 RIFF 头尺寸**——取消的半成品 wav 头是错的，可接受（文档写明）。

**测试口径**：ADSR 默认 release **0.2s**（≈96 块@48k/128），不是采样播放器的 10ms——"Stop 后静音"断言要给足块数；探针（临时 debug 测试 + println + assert!(false)）是定位"链路哪一环无声"的最快手段，删净即可。

### S0：headless 装配层 + `--render`（2026-11-07）

新模块 `glitcherz-ui/src/headless.rs`。动机不是"给 CLI 加个入口"，而是**文档 → 引擎的装配原来写了两遍**：`engine_sync.rs::install_node`（18 处 `VoiceSpec::` 分支）与 `export_ops.rs::spawn_offline_node`（12 处）互为镜像，`start_export` 又把 `apply_document` 的连线 / clip 表 / floor 逻辑整段抄了一遍。每新增一个声源都要改两处。

**分层**：`Media` trait 只暴露三个"文档路径 → processor"操作（sample / soundfont / drum）加一个 `audio_data`。`AppMedia(&mut GlitcherZApp)` 复用现有会话缓存（`obtain_*`），`FreshMedia` 是 CLI 的一次性 `SampleLibrary`（不做 LRU，渲染只碰一次文件）。`spawn_node(node, rate, &mut impl Media, live, input)` 是全仓**唯一**的 `NodeDoc` → `Box<dyn Processor>` 构造点，返回 `Spawned { processor, guard, info }`；`info` 是纯 UI 记账（插件参数表 / 出厂 state blob / 内置效果电平表），离线路径直接丢掉。

**两条调用链**：

```
实时  apply_document → install_node → spawn_node(live=true)  → add_node + 会话缓存写回
GUI   start_export   → assemble_offline → spawn_node(live=false) → ExportJob::new → 帧尾 pump
CLI   --render       → render_project_to_wav → assemble_offline   → ExportJob::new → 循环 pump
```

`assemble_offline` 从 `start_export` 原样搬来（track chain / cables / 三张 clip 表 / `SetArrangementFloor` / seek / Play），因此 GUI 导出与 CLI 逐样本一致是**结构上**的保证，测试 `gui_export_pipeline_matches_the_cli_render` 锁住这条契约。

**几个必须记住的取舍**：

- `live` 只影响记账，不影响 DSP——**除了构造期参数**：实时路径靠 `apply_document` 之后的 `push_node_params` 重放内置合成器 / 采样窗口参数（带参数平滑），离线没有这一步所以必须在构造期下发。若两边都提前下发，实时首块的平滑起点会变，属于行为改动，不做。
- 空路径的 Sample / SoundFont 节点：原来实时静音、导出时 `obtain_*` 失败回落内置合成器（同一工程两种声音）。统一成静音。
- `AudioTrack` 节点构造期用 `empty_clip_table()`，两条路径随后都推真表；单节点安装（`mixer_panel` / `patch_ops`）不走 `apply_document` 的 step 5b，所以 `install_node` 尾部补一次 `send_audio_clips_for_track`。
- 虚拟键盘在 `assemble_offline` 里必须 `continue`（`spawn_node` 为保持 total 会返回一个占位 processor，但不能让它进图）；离线 `Input` 恒静音。
- `AppMedia` 借用整个 `&mut GlitcherZApp`，所以 `input` processor 要在构造 media **之前**从 `self.engine` 取好；`start_export` 先 `self.project().clone()` 结束不可变借用，`build_audio_clip_table` 则只快照本轨的 clips + `base_dir`（不整份克隆文档，stem 多 pass 时每轨都会调一次）。这是本项目反复遇到的 borrow 形状。
- `render_project_to_wav` 用 `ExportJob` 的同步 pump（不建窗口、不 `AudioEngine::start()`），带一个宽松的 block 上限防走带卡死；结束前检查 `job.outputs()` 非空，再 `hound::WavReader` 回读头部拿帧数。

**验收**：40 个测试二进制全绿（新增 6 项：ui lib 3、app 单测 2、app 集成 1），`fmt` / `clippy -D warnings` 干净，`--seconds 3` 窗口冒烟 + `--render` 无窗口冒烟通过。用户文档：`docs/README.md` 运行节、`docs/manual.md` 导出节。

### S4：CLI 批量命令（2026-11-09）

spec `.agents/spec/cli-commands.md`。这一项的特殊之处是**它几乎没有新设计**：写 spec 时读代码才发现 `glitcherz-command` 早就是那条「命令面」——`Session::new(Project)` 不碰 egui，`execute(id, &serde_json::Value)` 自带快照 / 回滚 / panic 兜底 / undo / journal（journal 的 doc 注释原文就是「macros and **agent scripting**」），`CommandInfo` 已经 `Serialize`，`serde_json` 早在 lock 文件里，`load_project` / `save_project` 本就在 `glitcherz-model` 且与 UI 无关。plan 立项时预设的三个问题里，前两个的答案都是「不用设计，已经有了」。剩下要做的只有三件：**一个入口、一个枚举口、一套失败语义**。

**一个入口。** `--open` 给文档，`--ops <file|->` 或可重复的 `--do <id> [--params <json>]` 给命令序列，`--save` / `--wav` 是可选终点。三者拼成一条流水线：载入 → 执行 → 存盘 → 渲染。**argv 顺序就是执行顺序**，实现方式是 `--ops` 在解析期只记路径、执行期才读文件，所以 `--do a --ops list.json --do b` 的次序是 `a, list…, b`——这一点有专门的等价性测试（同一个混排批次与一次写死的 `--ops` 必须得到同一份文档），因为「参数按什么顺序生效」是最容易在重构里悄悄坏掉的东西。

**全或无。** 任一命令失败立即停，`--save` 与 `--wav` 都不写。不需要手工回滚：文件只在全部命令成功之后才写一次，所以「跑一半崩了」不会留下半个文件。这是把 `Session::execute` 对单条命令的承诺（一条命令不能留下半个编辑）提到批次粒度，也是为什么失败批次可以为「跳过坏命令继续」——那等于带着未知状态写文件。

**`--wav` 必须渲染编辑后的内存文档。** 否则「改完渲染」变成「渲染改之前的文件」，整个闭环是假的。为此把 `render_project_to_wav` 拆成「`load_project` + `render_doc_to_wav(&Project, …)`」，前者行为逐字节不变（S0 的零回归与对拍测试都还在）。验收方式很有意思：同一份工程 `transpose +12` 后 `--save` + `--wav`，产出的 wav 必须与「`--render` 那份存盘文件」**逐字节一致**，同时必须与「不跑命令的 `--wav`」不同——两条断言一起才排除了「其实只是碰巧渲染了磁盘上的旧文件」。

**失败语义**是 S4 唯一真正的新东西，因为原来所有失败都是 `ExitCode::FAILURE`（1），机器分不清「参数写错」和「这条命令现在不适用」。现在固定 0（成功）/ 1（引擎或窗口）/ 2（参数或输入文档）/ 3（工程或渲染）/ 4（命令失败），`--json` 信封里的 `failed.stage` 再区分 `ops` / `load` / `command` / `save` / `render`，只有 `command` 带 `id` 与 `kind`（`CommandError` 变体名小写：`unknown` / `unavailable` / `bad_params` / `panicked`）。`--render` 失败的退出码因此从 1 变成 3——这是本阶段唯一一处对外行为变更。

**流的选择**也踩过一次：批次失败是「结果」不是「用法错误」，所以人读模式下错误行进 stdout 的转录、`--json` 模式下信封进 stdout 并把人类可读的那一行另写 stderr；只有参数层错误（退出 2）直接进 stderr。三条测试一开始都按「错误进 stderr」写，全红。

**`--render` 与批次互斥**（同时出现退出 2）而不是按参数个数猜语义：`--render` 一个参数时表示输出、两个参数时表示输入+输出，这种解析迟早咬人。冲突信息直接指路 `--wav`。

**模块切分**：`main.rs` 只剩 `main` + 六个模式的分派（279 行）；解析、批次执行、渲染分别进 `args.rs` / `batch.rs` / `render_cmd.rs`。互斥规则只在 `Args::mode()` 一处判断，不在各调用点重复。

**已知限制（写进 spec §9 并用测试钉住）**：新工程的轨道上**没有 clip**，而 16 条 `midi.*` 全部作用于既有 clip，所以 `--do midi.note_add` 在默认工程上直接退出 4——「从零造一首曲子」仍需要一份模板工程。这是词汇表的问题（缺 `clip.add`），不是运输层的问题，另开一期。

**验收**：21 条（19 条用例 + 零回归 + 门禁）全部回填。39 项新测试（21 单测 + 18 项真子进程用例），全量 **1 245 项通过 / 0 失败**。`fmt` / `clippy --workspace --all-targets -D warnings` / `build --workspace` 全绿。零回归：`8c3ec61` 的 detached worktree 与 post-S4 对同一份工程各出 44 288 帧 / 177 196 字节，`cmp` 逐字节一致。

### S5：MCP server（2026-11-09）

spec `.agents/spec/mcp-server.md`。立项时 plan 写的前置是「把 headless 层提升为独立 `glitcherz-session` crate」，S0-D1 的原话却是「真需要独立 server 进程时再原样提升」。到了要判断的时刻，读了代码的结论是**不提升**：客户端 spawn 的本来就是独立进程，`--mcp` 与窗口不共享任何内存，「进程隔离」这个真需求已经满足；而 `headless` 的 `spawn_node` / `Media` / `SpawnInfo` 与 UI 的 `install_node` 缓存深度耦合（`AppMedia` 就是 `&mut GlitcherZApp`），外迁是一次大重构，收益只有一个消费者。于是 MCP 成了 `glitcherz` 的**第四种 headless 模式**，与 `--render` / `--ops` / `--commands` 共用 `Session`、`render_doc_to_wav`、`save_project`——同一份代码，零重复。

**协议是手写的，不是引库。** MCP 的 stdio 传输就是「一行一个 JSON 对象」，`serde_json` 早在依赖里；而引入 `rmcp` 之类的框架，省下的 200 行换来的是把版本协商、错误码、握手顺序全部交给第三方——这三样恰恰是本阶段要定死的东西。实现只有三处：`protocol.rs`（帧解析 + 错误码 + 版本协商）、`mod.rs`（服务循环 + 方法分派）、`tools.rs`（6 个工具 + JSON Schema）。服务端支持的版本是 `2024-11-05` / `2025-03-26` / `2025-06-18`，客户端请求哪个就回显哪个，不认识就回 `2025-06-18`——对「只有 tools」的服务端三版没有行为差异，回显只是省客户端一次降级。

**失败走两条通道。** 工具执行失败（文件读不到、命令不接受这个编辑、渲染失败）是 `isError: true` + `content[0].text` 里一个 JSON 错误对象——错误是给**模型**读的，模型要能看到原因并自己纠正，不能让它变成协议栈里一句 `-32602`。JSON-RPC error 只留给「请求本身不合规」：坏 JSON `-32700`、不是请求对象 `-32600`、未知方法 `-32601`、参数不符 schema 或未知工具 `-32602`。`isError` 与报告里的 `ok` 同真同假，客户端读哪条都对，`stage` 复用 S4 的词表（`load` / `command` / `save` / `render`）再加一个 `path`。

**会话是全或无的反面。** S4 的批次能全或无，是因为它不写文件——「最后才写一次」天然原子。MCP 没有这个杠杆：会话是活的，第 3 条命令失败时前 2 条已经改了文档。所以这里只承诺**单条命令原子**（`Session::execute` 的既有保证），批次默认 `stop_on_error: true` 失败即停，报告里剩下的列进 `skipped`。补偿手段是 `commands_run` 的 `dry_run`：调用前捕获 `Arc<Project>` 快照，跑完全部命令再 `restore` 回去，`applied: false`。这让 agent 能「先验证这套编辑，再落地」，比「跑坏了重开」便宜得多。不做全或无也不假装做，是这一段的取舍核心。

**路径沙箱**。`--root`（缺省 cwd）是唯一可写区：相对路径 join、绝对路径原样，然后**词法规整**（消 `.` / `..`，纯字符串操作——写入目标还不存在，`canonicalize` 用不上），再校验 `starts_with(root)`，绕过前缀的 `..` 在弹到根之后自然被这一步挡住。绝对路径同样受检，否则「给个绝对路径」就是后门。词法规整看不见「root 内指向外部的软链接」，所以父目录存在时再 `canonicalize` 复检一次；残余的 TOCTOU 写进 spec §9，不假装解决。

**插件默认禁用**是 MCP 场景独有的风险：S4 的批次是人手敲参数，MCP 是模型拼参数，而 CLAP / VST3 就是 `dlopen` 任意二进制。为此把 `headless::spawn_node` 的 `live: bool` 换成 `SpawnPolicy::{Live, Offline, OfflineNoPlugins}`——**布尔参数长出第三个值时，枚举才是诚实的**，而且顺手让「是否收集 GUI 簿记」与「是否允许加载插件」两个概念分开（`policy.live()` / `policy.plugins()`）。插件乐器被拒时渲染静音；插件效果器被拒时回落到内置效果，这正好是「实例化失败」已有的行为，所以拒绝和失败走同一条路。`warnings` 不从 `spawn_node` 回传（那要给 `SpawnInfo` 加字段、要跨 `Media` 泛型），而是在渲染前扫一遍文档拿节点列表——同一份信息，零侵入。

**零回归的边界**：`render_doc_to_wav` 的签名和 `RenderOptions` 一个字段都没动，新增的是 `render_doc_to_wav_with(…, policy)` 这个带策略的版本，前者变成后者的 `SpawnPolicy::Offline` 薄包装。11 处 `RenderOptions { … }` 字面量因此一处都不用改——**给老函数加参数会让每个调用点都要动，加一个变体函数只多三行**。

**验收**：25 条（23 条用例 + 零回归 + 门禁）全部回填。58 项新测试（35 单测：沙箱 6 / 协议 10 / 工具 9 / 分派 10；23 项真子进程端到端），全量 **1 303 项通过 / 0 失败**。`fmt` / `clippy --workspace --all-targets -D warnings` / `build --workspace` 全绿。零回归：`f4fafe9`（S5 spec commit，最后一个 pre-S5 代码状态）的 detached worktree 与 post-S5 对同一份工程各出 44 288 帧 / 177 196 字节，`cmp` 逐字节一致（sha256 `04ab77f9…`）。另有一条独立证据：`render_wav` 与 `--render` 同参数产出的 wav 在测试里 `assert!(cli == via_mcp)` 逐字节相等——这是 `render_doc_to_wav_with` 没有偷偷改变渲染行为的钉子。

**没做的（写进 spec §2，用户 2026-11-09 拍板）**：命令词汇扩容（`NodeParam` 稳定字符串名、`clip.add`）、undo / redo / history 工具、resources / prompts / sampling / progress、HTTP / SSE 传输。前两项是独立一期——词汇表要定命名规范、要迁移 40+ 参数、要决定 `node_id` 跨工程不稳定怎么办，塞进 MCP 会把「协议 + 安全」撑成三份设计。

## 框架升级与 gotchas

- cpal 0.18；
- egui 0.36：`App::ui(&mut Ui)`、`egui::Panel`、`Response` 无 `pressed/released`（用 `is_pointer_button_down_on`）、`DragValue::range(RangeInclusive)`、`Painter::rect_stroke(rect, radius, stroke, StrokeKind)`；
- clack 0.2：`clack-extensions` 宿主侧 trait 需 `clack-host` feature；`Pckn::new` 需显式 u16/u32 字面量；`start_processing` 返回 `&mut Started`；`PluginInstance::new` 收 `&CStr`；`Pckn` 类型导出路径是 `clack_common::events::Pckn`（`events::pckn` 模块私有）；`ParamValueEvent::new(time, param_id, pckn, value)`；`EventBuffer` 经 `as_input()/as_output()` 包装后 `as_raw()/as_raw_mut()` 取 raw 指针（G3 flush 直调用到）；
- RON/serde：定长数组（`[Track; 4]`/`[Step; 16]`）在 RON 里是**圆括号**（serde tuple 路径），方括号只用于 `Vec`；
- VST3 参数为归一化 0..1（CLAP 为原始值），跨格式展示需换算。

### 跨平台构建：VST3 枚举类型 + eframe 的 Linux 后端（2026-10-23）

症状：Windows 通过，`ubuntu-latest` / `macos-latest` 各自报一类错（CI 从没绿过）。

**① `vst3` 0.3 的枚举类型按平台分叉**：生成的绑定把每个枚举写两遍 —— 常量容器（`Vst::MediaTypes`、`Vst::BusDirections`、`Vst::ProcessModes`、`Vst::SymbolicSampleSizes`、`Vst::Event_::EventTypes`、`Steinberg::IBStream_::IStreamSeekMode`、`PClassInfo_::ClassCardinality` …）的类型是 `vst3::support::DefaultEnumType` = `c_int`（Windows）/ `c_uint`（其他），而**接收这些值的字段与接口参数一律写成 `int32`**（`ProcessSetup::processMode`、`ProcessData::symbolicSampleSize`、`IComponent::getBusCount(type, dir)` 等）；也有少数常量的类型本身就是 `int32`（如 `ParameterFlags`），天然对齐、不用转换。Windows 上两者都是 `i32` 所以看不出问题，macOS/Linux 上是两种类型，`cargo build` 直接 `E0308`。

接缝收在两个 `compat` 里（宿主 `src/compat.rs`、fixture 内同名小函数）：`to_int32`（常量 → `int32` 槽）/ `to_uint32`（`BusInfo::flags` 这类 `uint32` 槽）/ `to_enum`（`int32` 参数 → 枚举类型，好在 `match` 里直接对常量分支）。泛型走 `TryInto`/`TryFrom`，两个方向都能编译，不必写 `#[allow(unnecessary_cast)]`——`as` 在 Windows 上是多余的、在 Linux 上是必需的，正是 `clippy -D warnings` 会卡住的那种差别。判 `match` 的入参一律先 `to_enum(...)` 再对常量，避免 `m if m == ...` 这种守卫写法。

**② eframe 关掉默认 features 会连 winit 的 Linux 后端一起关掉**：workspace 里 `eframe = { default-features = false, features = ["default_fonts"] }`（为了不拉 wgpu），`glitcherz-ui` 只补了 `glow`/`persistence`，于是 `x11`/`wayland` 两个 feature 也没了 —— winit 在 Linux 上 `compile_error!("The platform you're compiling for is not supported by winit")`。`glitcherz-ui` 现在显式带 `wayland` + `x11`（在 Windows/macOS 上是空操作），`Cargo.lock` 因此多出 X11/Wayland 栈（另有一批只进 lock、不进构建图的 `egui-wgpu`/`wgpu` 条目，无影响）。`winit/default` 附带的两项没跟着开：`wayland-dlopen`（免运行期 `libwayland` 硬依赖）与 `wayland-csd-adwaita`（GNOME Wayland 下自绘窗口边框）——CI 装了 `libwayland-dev`，链接期没问题；要在 GNOME 上跑得舒服再单独补 feature。

**验证方式**：Windows 上无法直接编译 Linux/macOS，`cargo check` 交叉 target 又会卡在 `alsa-sys` 的 pkg-config。用 `[patch.crates-io]` 指一份把 `DefaultEnumType` 改成恒为 `c_uint` 的 `vst3` 副本，就能在 Windows 上跑出 macOS/Linux 的 `E0308`，改完即绿；feature 侧用 `cargo tree --target x86_64-unknown-linux-gnu -e features -i winit` 确认 `x11`/`wayland` 真的上来了。

### egui 0.36 焦点契约（M4b 时间轴“永远 no focus”根因）

症状：时间轴工具栏黄色 “no keyboard focus” 常亮，点击也救不回来；所有 G/T/S/K/Delete 全死；钢琴窗同套机制却正常。

根因是 **widget id 的位置相关性**：

- `Ui::next_auto_id()` 是**纯计数器**（`Id::new(next_auto_id_salt)`），不掺 ui 自己的 id；
- `ui.horizontal` / `Ui::new_child` 默认走 `IdSource::Child`：子 Ui 的 `unique_id = parent.id.with(salt).with(父计数器)`，且内部自增种子取自该值——所以**父 Ui 里任意 widget 增减，都会整体平移下面所有子 Ui 及其后代的 id**；
- `ui.push_id` **救不了**：它只给盐值，`unique_id` 仍含计数器；
- 解法是 `UiBuilder::new().id(explicit_id)`（`IdSource::Explicit`）：子 Ui 的 id 与上方一切无关。

故障链条：焦点 chip / knife·modal 提示 / 状态消息本来直接画在行的父 Ui 里，它们的**出现与消失**把每行 `horizontal` 的 id 整体挪了一位；egui 的焦点是**绑 id**的，`Focus::end_pass` 的死人开关还给刚获焦的 id 一帧宽限（`recently_gained_focus`），于是焦点永久停在一个没有任何 widget 认领的**幽灵 id**上——每行 `has_focus()` 恒假，键全死，chip 重新点亮；而 chip 点亮本身又会把 id 挪回去，形成闪烁。**点击永远救不回来**，因为救活的那一帧正是 chip 消失、id 再次漂移的那一帧。

修复（`timeline.rs`）：

- 每行改用 `ui.new_child(UiBuilder::new().layout(Layout::left_to_right(Align::Center)).id(Id::new("glitcherz_timeline_row").with(row.key())))`，行内 widget 拿到稳定的 id；
- 所有可变提示搬进 toolbar 的 `ui.horizontal` **子 Ui**（与 roll 一致）：子 Ui 只占父层一个槽，内容多少不影响下游顺带——顺带修掉“状态消息一来整排行往下跳一行”的视觉 bug；
- `owns_keyboard_focus` 增加“本帧有行真的报告了焦点”：只看 egui 的 focus id 时，幽灵 id 会让它谎报 true（从而把虚拟钢琴静音、而按键其实到不了任何东西）；
- 焦点锁 filter 认领 `horizontal_arrows`/`vertical_arrows`（roll 自 M4a.4 起如此）：未认领的方向键会把焦点“导航”给别的 widget。

守护测试 `crates/glitcherz-ui/tests/timeline_focus.rs`：用真实 egui 帧复刻 app 的面板结构（transport 含 DragValue、status bar、view tabs、CentralPanel），断言焦点能扛住①点击后的若干帧 ②自身提示的来去 ③行上方任意 widget 的增删（该用例在修复前失败）。

#### 实测追加的两课（2026-10-14，用户复测暴露）

上一节修复后真实跑 app 暴露两个独立问题，根源都是"单元测试覆盖不到的 egui 行为"：

**① `new_child` 不推进父光标 → 整个时间轴只剩最后一行**。`Ui::new_child` 的文档明说它"不在父 Ui 里分配空间"，需要调用方自己 `advance_cursor_after_rect(child.min_rect())`——`ui.horizontal` 内部（`scope_dyn`）就是这么做的。但只补 advance 还不够：`Layout::left_to_right(Align::Center)` 的**frame 会撑满可用高度**（`Layout::next_frame_ignore_wrap`：水平布局 + `vertical_align()==Center` → `frame_size.y = available_rect.height()`），`expand_to_include_rect(frame_rect)` 又把这个 frame 计入 min_rect——`ui.horizontal` 之所以没这个问题，是它先用 `allocate_ui_with_layout` 把子 Ui 高度钳到 `interact_size.y`。所以行的正确写法是：`new_child(UiBuilder::new().layout(...).id(...).max_rect(可用矩形钳到 ROW_HEIGHT))` + 行画完后 `advance_cursor_after_rect(row_ui.min_rect())`。症状：所有行叠在同一位置，只有最后画的行可见，"时间轴只剩 track4，上面全是灰的"；焦点 fixture 测试全绿——它只断言 id/焦点，从不断言行位置。守护测试 `rows_stack_vertically_instead_of_overlapping`：从 egui widget stack 读回行容器矩形，断言纵向堆叠不重叠。

**② `drag_started` 只在指针移动后才触发 → 纯点击选不中 clip**。egui 0.36 对 `Sense::click_and_drag` 做点击/拖动消歧：*"If the widget also senses clicks, this will only become true if the pointer has moved a bit"*（超过 `max_click_dist`）。M4b.1 把 clip 选中放在 `drag_started` 分支里，于是"点一下 clip"永远不进选区，G/T/S/Shift+D 全部闪一句 "nothing selected"。钢琴窗早在自己的一轮里踩过同一个坑（`piano_roll.rs` clicked 分支注释）。修复：镜像 roll，加 `response.clicked()` 处理——命中 clip 则选中（Shift/Ctrl 追加/移除），空白处取消选择（带修饰键的空白点击不动选区）；拖动路径原样保留（真拖动时 `drag_started` 照常触发，二者互斥）。规范本就写明"点击=选中、空白单击=取消"（editing-model.md 交互表），是实现没对齐规范。守护测试 `clicking_a_clip_then_g_arms_the_move_modal` 直接钉住用户报障的操作流（点击 clip → G → 模态亮起）；`TimelineState` 补 `modal_active()` 观测访问器（与 roll 的测试访问器对齐）。

**③ 状态消息一帧即逝 = 拒绝原因完全不可读（2026-10-14）**。用户复测"按 G 只见黄字一闪"——那条黄字是 `state.status` 拒绝消息，toolbar 用 `take()` 当帧抽走，人眼根本看不清内容。G 路径下只可能是 "nothing selected" 或 "mixed selection"，闪现本身证明按键已到达 timeline（焦点门是通的）、只是按 G 时选区为空。修复：状态消息改为**粘性 5 秒**（`status: Option<(String, Instant)>` + `set_status` helper，新消息顶替旧消息，过期自动清除）；同时在 toolbar 加持久的 **"N selected"** 选区计数 chip——"我的点击到底选没选中"从此一眼可判。拒绝消息若在模态 finalize 期间产生（`&state.modal` 还被借用），先存入局部 `refused_msg` 再在借用结束后 flush。

#### 命令族对齐 roll（2026-10-14，用户复测反馈）

用户复测通过 G/Shift+D 后提出三处与 roll 不一致，全部统一：

- **X/Y 轴锁**：`ModalKind::Grab/Duplicate` 的 `time_locked: bool` 改为 `locked: Option<Axis>`（`Axis::{Time, Row}`），镜像 roll 的 `Move { axis: Option<Axis> }`。Blender 约定（roll `Axis` doc 注释定音）：**X = 仅时间轴移动**（行冻结，即原 `time_locked` 的行为——原字段名"锁时间"实为"锁定到时间轴"，改名消除歧义）、**Y = 仅行轴移动**（时间冻结，新增）。同键再按解锁、另一轴接管（`apply_axis` 同款三态）。
- **T+S 拉伸**：Shift+T 退役；bare `T` 武装尾缘拉伸，模态存活时 `S` 翻转为首缘（模态存活时键盘只答限定键——镜像 roll 的 "A modal transform owns the keyboard" 规则）。
- **单次刀**：K 从开关切换改为只武装；一次 LMB 点击消费武装（成功切割 / 拒绝提示 / 空白均可——roll 的 stroke 完成即收刀同理）；Esc / 右键取消不消费。

单测 `grab_moves_by_bars_and_rows_within_kind` 更新字段并补 Row 锁断言（时间冻结、行仍动）。
#### clip 单周期不变量：resize 拖长自动平铺 bug（2026-10-20）

症状：clip 第一小节输入音符后拖长尾部，播放时音符在新增小节**自动重复**（pattern 平铺效果），预期应是空白小节。

根因是 **UI 打破了 D14 的单周期前提**：引擎调度器按 `loop_ticks` 周期在 `[start, start+span)` 内平铺音符（spec 预埋的未来特性，`sequencer.rs` 的 `m·loop` 循环），而 UI resize/stretch 只写 `span_ticks` 不动 `loop_ticks`（恒为默认 1 bar）——`span > loop` 后平铺自动生效。同一裂口还让 split 产生 `span < loop` 的**非法文档**（模型校验 `span ≥ loop`，保存即报错）。

修复 = UI 层所有改 `span_ticks` 的路径维持 `loop_ticks == span_ticks` 不变量（引擎平铺语义原样保留，`span == loop` 时天然无重复）：

- `timeline_move_resize_clip` / `timeline_stretch_clips_batch`（`lib.rs`）：写 span 同时写 loop；
- `split_clip_in`（`timeline.rs`）：两半各自 `loop = span`；`Placement` trait 新增 `set_loop` + `retain_from(offset)`——右半内容取切口之后部分并平移到新原点（**跨界音符两半延续**：右半以剩余尾巴从 0 开始；`sequence.ctrls` 与 `curve` 锚点按 `tick ≥ offset` 平移过滤），左半内容**原样保留**（超出新 span 的音符/锚点只是隐藏，拖回长度即恢复——数据永不丢）；
- piano roll / lane 绘制过滤 `start ≥ loop_ticks` 的遗留内容（防溢出画布右侧；交互早已按位置匹配 + `min(loop_ticks - 1)` clamp，超界项点不到）。

新增小节从此**空白可输入**（piano roll 宽度 = `loop_ticks` = span，拖长即扩宽编辑区）。测试：`split_halves_keep_start_end_and_content` 重写（含跨界音符断言）、新增 `split_shifts_control_curve_anchors`。引擎平铺测试 `clip_tiles_within_span_and_stops_at_span_end` 不变（未来特性仍被锁定）。
#### roll / timeline 视图导航与轨道重排（2026-10-21）

**piano roll**：`RollState` 新增 `zoom_x`（0.2–64×，对 fit-to-width 的乘子）/ `zoom_y`（0.4–6×，对 `ROW_HEIGHT` 的乘子）/ `scroll_x`（视口左缘的 tick）。几何全面参数化：`ticks_to_x = width/fit_ticks * zoom_x`、`row_h = ROW_HEIGHT * zoom_y`、`visible_rows = roll_h/row_h`、**`t0_x = rect.left() - scroll_x * ticks_to_x` 是横向唯一几何源**（网格/音符/ghost/播放头/velocity lane/knife/命中全部由它派生，绘制与命中不漂移）；`note_rect/note_edges/note_hit_rect/note_at/knife_crossing` 签名改为显式携带 `t0_x/row_h`。**2026-11-05 起高度动态**：`roll_h = (available_height - velocity 48 - Σ overlay lane - item spacing).max(ROLL_MIN_HEIGHT=16 行)`，`available_height` 非有限（滚动容器内）时回落固定 `ROLL_HEIGHT`（32 行）；`fit_ticks = max(context span, 2 bars)`——默认打开即两小节，单小节 clip 不再独占宽度。交互：Ctrl+滚轮 = 时间轴缩放（`zoom_scroll_anchor` 纯函数锚定指针下 tick，clamp 0..loop-width）；Ctrl+Alt+滚轮 = 行高缩放（egui `zoom_delta_2d()` 的 vertical-only 分支 `(zx-zy).abs()>1e-4` 判定，纯 Ctrl 是 uniform 只缩时间轴）；Shift+滚轮 = 水平滚动（egui `horizontal_scroll_modifier` 把 delta 折进 `wheel.x`）；纯滚轮 = 纵向滚动（`top_note` clamp 下界随行高变 `rows-1`）；中键拖动 = 双轴平移（`dragged_by(Middle)` + `pointer.delta()`）。

**timeline**：`TimelineState` 新增 `row_zoom`（0.5–3×）+ `rows_rect`/`rows_viewport_min`（**上一帧**行区域屏幕矩形与视口原点）。关键时序：导航输入在 `ScrollArea::show` **之前**处理——zoom 与 offset 补偿同帧生效；若放在闭包内改 state 会被 ScrollArea end 的 store 覆盖。水平缩放锚定公式：`tick = (offset.x + ptr.x - vmin.x) / old_ppt`，新 `offset.x = tick * new_ppt - (ptr.x - vmin.x)`（`vmin` 取内容 ui 的 `clip_rect().min`，滚动方向上等于视口左上）。Ctrl+Alt+滚轮缩行高（`ROW_HEIGHT * row_zoom` 贯穿 lane_row/grab_targets 的 row_delta）；中键平移直接 load/store `egui::containers::scroll_area::State`（`egui::ScrollArea::State` 不是合法路径——State 未从顶层导出）。纯滚轮/Shift 滚轮保持 ScrollArea::both 原生行为。**网格铺满编排上限**（2026-10-21 追加）：lane/ruler 宽度 = `MAX_ARRANGEMENT_BARS`(256) × px_per_bar，不再随 clip 末端截断——G 模态 ghost / 插入 / drop 任何位置都有 bar 线可对齐，滚动范围与所有交互的 clamp 上限一致；网格与 ruler 绘制**视口裁剪**（`ui.clip_rect()` 换算 first/last bar index，ruler 端 `div_ceil(step)` 对齐刻度步长），256 bar 全铺不涨每帧绘制量。`MIN_CONTENT_BARS` 删除。

**轨道重排**：MIDI 轨头左侧 12px 六点抓手（`Sense::drag()`，hover 显示 Grab 光标，name 按钮缩 12px 让位）。拖拽期间 `state.reorder = Some(from)`，每帧 `reorder_target(ptr.y, midi_lane_rects)` 返回（插入位 `to`，指示线 y）——`to` 是**原数组坐标**的插入位置（行上半 = i、下半 = i+1、末行下 = len）；释放（全局 `pointer.primary_released()`，非 grip 的 `drag_stopped`——指针已跨行）且 `to ∉ {from, from+1}` 时发 `TimelineAction::ReorderMidiTrack`。app 层 `reorder_midi_track`：`track_permutation(len, from, to)`（old→new 双射，测试全组合验证）驱动轨表重排 + `NodeDoc::Instrument/Effect/ChannelStrip` 的 track 字段重映射（**Mixer/Keyboard/Feedback 不动**——track 仅 patch 分组，与 `remove_last_midi_track` 的排除一致）+ `PortRef::MidiTrack` 连线重映射 + `selected_track/edit_clip/timeline selection` 跟随，`apply_document()` 全量重建（undo/redo 同路径零额外代码）。

**坑（中键 pan 与左键手势串台）**：egui 的 `drag_started()/dragged()/drag_stopped()` **不区分按钮**——任何键的拖动都触发，中键 pan 落地后中键拖动会同时进入左键手势（marquee/绘制/clip 移动）。修复 = roll/timeline 所有手势入口改为 `drag_*_by(PointerButton::Primary)`（`clicked()/double_clicked()` 默认就只算 Primary，无需改；patch.rs 自 P5d.1 起一直按按钮过滤，正是先例）。守护测试 `middle_drag_never_reports_as_a_primary_drag`（egui_event_semantics.rs）锁定该契约。

**坑（模态变换失焦 + 落定无效）**：G/T/Shift+D 模态不是真实拖动——router 的 `any_down` 冻结只保护按住按钮的手势，hover-follow 照常运转（指针越过内容边缘到达 neutral chrome 即 surrender，Esc/轴限定键全灭）；且确认点击是 `response.clicked()`（组件矩形内），指针已在组件外时点击落不进任何 lane。修复两层：① `FocusRouter::set_focus_hold(bool)`——模态/armed knife 期间由组件每帧置位，`begin_frame` 跳过整个 hover-follow（与 any_down 同级的冻结；每帧设置所以模态结束自动解）；② 确认/取消改读**全局** `i.pointer.button_clicked(Primary/Secondary)`（timeline 在 finalize 段、roll 在 show() 尾部兜底——lane 内点击先被原有路径消费掉 modal，两路天然互斥；确认 frame 用 `latest_pos()` 与 ghost 预览同源）。守护测试：`focus_hold_pins_the_owner_through_neutral_hover`（focus.rs）+ `a_click_outside_any_widget_is_globally_visible`（egui_event_semantics.rs，锁定"死区点击全局可见、Response 沉默"契约）。**注意**：focus 单测里被聚焦的 id 必须有真实 widget（`ui.interact`）——egui 的 focus dead-man switch 会清掉无人认领的 id。

**坑**：PowerShell 5.1 `Get-Content -Raw` 默认按 ANSI(GBK) 解码 UTF-8 文件 + `Set-Content -Encoding UTF8` 会加 BOM——非 ASCII（→/—/·）全部乱码且不可逆（部分字符吞换行合并行）。**改源文件一律用 edit 工具或 Python（显式 `encoding='utf-8'`）**；已损坏时 `git checkout -- <file>` 恢复后重放编辑。

### egui 0.36 → 0.37（2026-11-05）

`eframe` 从 0.36.2 升到 0.37.0（egui / epaint / ecolor / emath 跟着走）。`default-features = false` 保持不变，只把 0.37 新变成"可选"的 `clipboard` 显式打开。

**0.36 → 0.37 实际撞到的五个契约**（升级前评估里点了四个，最后一个只有编译器会告诉你）：

| 变化 | 影响面 | 处理 |
| :--- | :--- | :--- |
| `clipboard` 变成 eframe 的**可选 feature**（默认 feature 里有，但我们关掉了默认） | 所有文本框的 Ctrl+C/V | `glitcherz-ui` 的 eframe features 加 `clipboard`。不加上去**不会编译失败**——只是复制粘贴静默失效，属于最难自查的一类 |
| `Id::new` 弃用，拆成 `Id::unique`（全局唯一）与 `IdSalt::new`（局部盐） | 30 处、11 个文件 | 全部换 `Id::unique`：签名相同、语义相同（`Id::new` 在 0.37 里就是 `Id::unique` 的薄封装） |
| `Ui::id()` → `Ui::scope_id()`，`UiBuilder::id()` → `UiBuilder::scope_id()` | 2 处 | 直接改名。注意这**不是** `unique_id()`：`scope_id` 是布局作用域的稳定 id，`ui.interact` / `UiBuilder` 要的正是它 |
| `ScrollArea::State.offset` 变成**私有字段**，只留 `clamped_offset()` / `unclamped_offset()` 读 | 时间轴 Ctrl+滚轮锚点缩放、时间轴中键平移、混音器中键平移，外加 `mixer_strip_scroll` 测试脚手架 | 0.37 没有 setter，**改用 builder 的 `scroll_offset` / `horizontal_scroll_offset`**：手势算出目标偏移 → 传给 ScrollArea；不再"读 state → 改 → store"。只在手势发生的那一帧传，否则每帧都传会把视图钉死、滚轮失效 |
| `Event::MouseWheel` 多了一个 `source: MouseWheelSource` 字段（Wheel / Trackpad / Momentum / Unknown） | `tests/egui_event_semantics.rs` 的合成滚轮事件 | 补 `source: Wheel` |

**刻意没做的**：`system_fonts` 仍然关闭（工程自带 Noto Sans SC，见 `fonts.rs`）；`wgpu` 不启用，继续用 glow；`links` 不启用。这也说明为什么 `epaint_default_fonts` 的字体集（Ubuntu-Light / NotoEmoji / emoji-icon-font / Hack）与 0.36 完全一致 —— `.agents/glossary.md` 里那张"哪些字形能显示"的表不用改。

**ScrollArea 那一条是唯一有行为风险的**。旧写法是"改存储里的 offset"，相当于在 ScrollArea 布局**之前**把状态改掉，同一帧生效；新写法 `scroll_offset()` 在 `ScrollArea::show` 里 `state.offset.x = offset_x.unwrap_or(state.offset.x)`，恰好也是布局前赋值，所以缩放锚点的数学（`tick → new_ppt → anchored`）一个字没改。差别只在"谁拥有这次赋值"：手势帧由 builder 覆盖，其余帧照旧由滚轮/拖动驱动。

**门槛**：`cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace`（39 个测试二进制）全绿，**第一次跑就过**，没有需要改断言的用例——包括最依赖像素几何的 `timeline_focus`(9)、`mixer_strip_scroll`(2)、`egui_event_semantics`(1)、`roll_state_machine`。`cargo build -p glitcherz-app` 也能链接出可执行文件。

**视觉**：0.37 把默认间距、按钮内边距、圆角调大了（"roomier buttons / larger corner radius"），工程没有覆盖全局 `Style`，所以整套界面会比 0.36 略微松一点、圆一点；滚动条命中区和橡胶带滚动也换成了新实现。这是升级的预期代价，需要实际开窗口看一眼再决定要不要用 `Style` 补偿。

### egui 0.37 复盘：丢失的图标字形 + 端口名贴回插口（2026-11-06）

开窗口看到两处升级后的毛病，一处是回归，一处是老账。

#### 1. 走带按钮变豆腐块：`monochrome_emoji_fonts` 没打开

egui 0.37 把 NotoEmoji 与 emoji-icon-font 挪进了 `monochrome_emoji_fonts` feature（默认关），而 **eframe 不转发这个 feature**，我们又用 `default-features = false` 构建。结果：字体栈里一个能画媒体控制块的字体都没有，`⏮ ⏸ ⏭ ⏺` 全变成主字体的 `.notdef` 方框。**编译器不会报任何错**——这类"静默降级"比编译失败难查得多。

排查过程本身也踩了一个坑：0.36 时代用 `galley` 里有没有 `U+FFFD` 判断缺字形；0.37 换 harfrust 排版后，找不到的字形是**主字体的 notdef 豆腐**、`chr` 原样保留，所以老判据对每一个缺失字形都"通过"。正确入口是 `FontsView::has_glyph`。

全仓字符串字面量扫出 25 个非 ASCII 符号，逐个查：缺 7 个 —— `⏮ ⏸ ⏭ ⏺`（媒体控制块 U+23xx，GBK 里没有，内置 Noto Sans SC 子集也就兜不住）、`↩`（patch 里的"回车/返回"提示）、`♪`（音符标记）、`▸`（菜单路径箭头）。

- 打开 `monochrome_emoji_fonts`：workspace 加一条 `egui = { version = "0.37", default-features = false, features = ["monochrome_emoji_fonts"] }`，`glitcherz-ui` 里 `egui = { workspace = true }`。**代码里不用它**——这行依赖的唯一作用是让 cargo 做 feature 合并时把 eframe 编译的那份 egui 也带上这两个字体。代价：debug 可执行文件 57.47 MB → 58.21 MB（+722 KB，正好是 NotoEmoji 419 KB + emoji-icon-font 324 KB）。
- `▸` 是唯一补不回来的（连 emoji 字体都没有 U+25B8）：`Edit ▸ Preferences` 改成 `Edit › Preferences`（U+203A 每个字体都有）。

顺带核对了 egui 自己的控件字符：`⏴ ⏵ ⏶ ⏷ ⇧ ⌘ ⚙ ⚠ ↕ ☀` 与常用 emoji 都齐；只有 `⌃`(U+2303) / `⌥`(U+2325) 没有——那是 macOS 的 control/option 修饰键字形，只在打印 `Modifiers` 名字时才用，本项目不打印，先记在 glossary 里。

新增回归测试 `fonts::tests::every_symbol_the_ui_labels_with_has_a_glyph`：把 UI 实际用到的符号列成 `UI_SYMBOLS`，逐个问 `has_glyph`，缺一个就失败并点名。glossary 的"符号白名单"整节按 0.37 重写（旧版本还写着"egui 0.36 默认字体"、并且把 `→ ← ↑ ↓` 列为禁用——从内置 CJK 兜底之后它们一直能用）。

#### 2. 端口名离插口太远：30 pt 是给"最差缩放"付的钱

`Timeline` 输出口 / `Mixer` 输入口的名字本来挂在离圆心 **30 pt** 的列上，而 `Timeline In` 终端的名字是 **10 pt** ——所以同一个画布里两种间距，后者看着才正常。

30 的来历写在注释里：0.6（最小可交互缩放）时 `30 × 0.6 = 18 = PORT_HIT_RADIUS`，即"按下名字不会误起一条连线"。也就是说，间距是为**最差缩放**选的，100% 下就空出 24 pt。

把规则换了个位置实现，间距就能还给视觉：

- `PORT_NAME_GAP` 30 → **10**（和 `Timeline In` 终端同一个值，`Timeline In` 反过来改成读这个常量 + `10.0 * zoom`，缩放行为也与其它标题一致了）。
- 新增 `port_grab_at()`：**名字矩形内的按下归节点，不再问端口**。于是名字贴到 10 pt（落在 18 pt 抓取圈里）也不会"拖名字拖出一根线"；圆点本身以及名字以外的整圈仍然照旧抓连线。`name_labels` 的每帧快照因此从绘制前挪到按下的路由之前——同一份 rect 现在同时驱动抓取否决、右键改名和绘制。
- 连带效果：`CHANNEL_STRIP_LEFT` 由 `10 + 72 + 6` 推出 = 88（原 108），混音器推子因此长了 20 pt。这是同一个公式的自然结果，没有额外调参。

测试改写为 `port_name_captions_hug_the_dot_and_own_their_press`：断言名字边缘正好在 `PORT_NAME_GAP * zoom` 处、名字里的按下 `port_grab_at` 返回 `None`、圆点上的按下仍然返回 `Some`，并要求名字不越过推子条。manual 的端口名一段补了一句"左键落在名字上是拖动节点、不会误拽连线"。

## 系列实施补记与交互打磨

### I 系列：交互打磨（I1–I6，2026-11-10）

用户一次提了 5 条（vk 节点可弹、卷帘键列试听、工具栏按钮、draw 默认长度、strum 失效），评审后加第 6 条（timeline 头裁）。决策见本章末的 I-D1…I-D7，按 I5 → I6 → I4 → I2 → I1 → I3 落地。

**I5 是默认值的锅，不是命令层的。** `StrumOptions::default()` 全 0，而 `model::ops::strum()` 在 spread = 0 时是恒等变换（`strum_with_zero_spread_is_the_identity` 锁着这条契约）。UI 用 default 打开面板、Apply 静默关闭——所以"strum 没反应"。修法放在 UI 侧 `default_strum_options()`（spread = 1/32 = 120 t、window = 30 t），并在 spread = 0 时给状态栏提示、保持面板打开。模型契约不动。

**I6 的每帧必须从拖拽起点快照重算。** `Placement::retain_from()` 已经实现真头裁（裁头 + 跨界音符保留尾巴 + 内容前移），但它是按 delta 改文档的——每帧拿"当前" clip 再算 delta 会累积吞音符。所以 `Drag::Resize` 带 `origin_start / origin_span / origin_loop` 和一份 `HeadTrimOrigin` 快照（MIDI/Control 是 `Box<Clip>` 克隆，音频只要 offset）。左边缘按下曾是 Move，现在左右各 `EDGE_GRAB`，窄于 `3×EDGE_GRAB` 的 clip 仍优先当作移动。tiled clip 的头裁只缩短容器并提示——真裁会毁掉重复相位。

**I2 的试听不能走 intent 槽。** `show()` 一帧只返回一个 `RollIntent`，键列试听会和拖动音符抢这个槽。改成 `preview_events: Vec<(note, velocity, on)>`，`editing.rs` 在 `apply_roll_intent` 之前排空成 `EngineCommand::Note`。松手兜底：`!pointer.any_down()` 时强制 Off，丢事件不会留下卡住的音。

**顺带修的边界 bug**：键列和网格的 x 边界是同一条像素线，`Rect::contains` 两端闭区间，而键列的 `interact` 注册在网格之后（更上层）——点 tick 0（未滚动视图的最左一列）会被判给琴键列，选框起不来。命中矩形收进 1 px 解决。这是 `select_mode_empty_drag_marquee_selects_what_it_covers` 变红的真正原因，不是 test 本身的问题。

**I1 的 transpose 是文档字段。** `NodeDoc::Keyboard.transpose: i8`（serde `default`，`PROJECT_VERSION` 不动），只影响该节点发出的事件；`send_keyboard_node_note` 与电脑键盘路径都过一次 `shifted_note()`（越界返回 `None`，不 clamp）。八度是纯视图态（`PatchState.keyboard_octaves`），不写文档。键条命中在 `allocate_rect` 之后 `ui.interact`，压过画布拖拽；删节点先释放 held 键再删。

**I3 的两组按钮必须长得不一样。** `toolbar.rs` 只提供两个原语：`mode_group`（互斥、填充、点当前模式不关掉自己）和 `action_button`（一次性、armed 时描边不填充）。`Select`/`Draw` 从"一个循环切换的文字 chip"变成两个互斥按钮；`Grab`/`Scale`/`Stretch`/`Knife` 是动作，`Knife` 不再额外显示 `K` chip。**从按钮 arm 的抓取点要推迟**：按钮在网格/轨道区外面，用它当零点会让选区在指针移动途中先跳一下。roll 用 `pending_arm`，等指针进入网格那一帧再 `arm_tool`；timeline 直接把 `grab` 传 `None`，复用既有的 `INFINITY` 哨兵 + 首帧 latch。键盘路径和按钮路径共用同一个 `arm_tool` / `arm_modal`，不会各写一份。

**门禁**：`cargo fmt --all`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` = **1322 通过 / 0 失败 / 11 ignored**。新增测试 14 条：I5 默认值 + 分数标签往返、I4 长度解析 + 自动回退、I2 `key_at` 几何 + 试听序列、I1 键条两八度 + 八度/移调边界、I6 `trim_head_start` 吸附/夹取 + 头裁保留跨界尾巴、I3 `mode_group` 互斥点击 + `arm_tool` 抓取点/空选区/`pending_arm`。

**零回归**：`5419ed1`（最后一个 pre-I 代码状态）的 detached worktree 与本轮之后的二进制，对同一份 `working_files/regression-project.glz` 各渲染 132 352 帧 / 794 180 字节，`cmp` 逐字节一致（sha256 `268d5306e2979f8b082b1f007af5fce1d161e2f2bccded9e1f8d674fb47aacac`）。I 系列只动视图与输入，B2 的 `muted` 默认全 false 且不写序列化键，所以音频路径确实没被触碰。

#### 后续修：工具栏点击会夺走快捷键（2026-11-10）

I3 上线后用户反馈"改完快捷键都无效了"。根因不在按键映射，而在**焦点**：egui 的默认 `SurrenderFocusOn::Clicks` 规定，任何落在控件矩形之外的点击都会让当前聚焦控件**主动交出焦点**，而卷帘/时间轴的悬停跟焦只在各自的音符网格 / 轨道矩形上注册过命中面。工具栏紧贴在网格**上方**，点一次 `[Draw]` / `[Grab]` / 缩放按钮就等于"在网格外点了一下"——焦点当场被交出去，而指针还停在按钮上，路由器找不到可还回焦点的矩形，于是 `d` / `g` / `s` / `t` / `k` / `Delete` / `Ctrl+A` / `Esc` 全部失效，直到把指针移回网格。**表现是"快捷键全死了"，实际是"键盘归属没了"**。

修法是把每个视图的键盘面扩大成它实际拥有的整个面板：

- 卷帘：工具栏行 + 琴键列 + 网格（外加速度条，在它分配处单独注册一条同 id 的矩形）。多个矩形可以用同一个 id 注册多条，路由器按"最后匹配者优先"取 id，重叠无副作用。
- 时间轴：轨道行的 id 是按位置生成的，没法提前知道，所以在**所有行画完之后**用 `geom.first()` 注册一条"工具栏带"（顶 = 工具栏行顶，底 = 第一行轨道顶，宽 = 表头 + 轨道），挂在当前持焦行上。底边收在第一行轨道上沿，所以它不可能抢走相邻行的焦点。
- 两边都要按 `style.interaction.interact_radius`（默认 5 px）**外扩**：egui 的命中测试本来就允许点在控件外侧 5 px 内，按钮的上边缘恰好落在面板 margin 里，不外扩的话点边缘仍算"在外面"。

教训：**任何"工具栏 + 画布"的上下结构，工具栏都必须算进画布的键盘面**；判断标准不是"看起来在同一个面板里"，而是"悬停跟焦注册的矩形覆盖不覆盖它"。回归测试用真实 egui 帧驱动：点工具栏后 `d` 必须仍能切模式（卷帘），指针停在按钮上时 `owns_keyboard_focus` 必须仍为真（时间轴）。顺带修正了 `keyboard_ownership_tracks_egui_focus_exactly` 里"把指针停到工具栏（非注册面）"的旧前提——工具栏现在是注册面了，改停到面板下方的死区。

**门禁**：1324 通过 / 0 失败 / 11 ignored。

#### 后续修：时间轴的键盘面是整个面板（2026-11-10）

卷帘侧修好后用户反馈「edit tab 正常了，但 timeline 快捷键还是不生效，按键会触发 vk 键盘事件」。第一层修复只把工具栏带注册进了键盘面，但那条带挂在 `focus_id` 上——首帧它还是 `None`（新窗口从工具栏进永远拿不到焦点），而且带只盖工具栏那一行：轨道下方大片空白、标尺都不在注册面里，指针停在那里 `owns_keyboard_focus()` 依旧 false，按键漏给 vk 发声。

修法换成**面板级注册**：`TimelineState` 删掉 `had_focus` / `focus_id` 两个字段（状态回写路径整条移除），每帧记住第一条行响应的 id（`frame_lane`），所有行画完后用这个 id 把「工具栏顶 → 最后一行底」整块矩形外扩 `interact_radius` 注册成键盘面。焦点路由的语义同时收紧成「指针落在面板内任意位置都跟随」，缝、子面板、空白区靠 sticky 保留上一个持有者。两条回归测试锁住：悬停在最后一行下方、悬停在工具栏/标尺上都必须持有键盘。真实窗口用 `GLITCHERZ_FOCUS_TRACE` 临时探针验证过同一空白位置从 `focused=None` 变 `focused=Some(lane)`，验证完已拆。

教训：上一条「工具栏要算进键盘面」的结论只对了一半——**正确单位是面板，不是「网格 + 顺带补的条带」**；注册面也不能依赖「先有过焦点」的状态（首帧 `None` 是隐性死区）。

#### 后续修：vk 的旧键衬底与 25 键 G-G 琴条（2026-11-10）

**旧键衬底**：patch 节点盒子绘制循环里还留着 G6 时代的装饰性 `paint_key_strip`（单八度 7 白键、固定像素偏移 `top+30` 到 `bottom-8`），I1 的交互琴条叠在它上面——高倍缩放时两者几何彻底脱钩（装饰条不随 zoom 缩放，飘在盒子中部），这就是「新交互琴键后面还压着旧的黑白键」。删除装饰条与其调用：交互琴条是键盘节点唯一的视觉签名，与其它 in-node 内容同一缩放门槛（`ACTION_MIN_ZOOM`），低缩放退化为纯盒子，与其他节点一致。

**琴条 24 键 → 25 键（C..B → G..G）**：原区间是半开的——「两个八度」却摸不到顶上的 C，且中音 C 以下一个键都没有。改为基音 = 视图八度的 C 下方那个 G（`octave*12+7`，八度 4 → G3..G5）：中音 C 下方多出 5 个低音键、顶部补回全八度的 return G，窗口对称，25 键顶格（八度 8 恰好 = MIDI 127）。实现上捡了个便宜：**从 G 起键时白键半音表不变**（G 本身就是全白键音阶的起点，interval 表与 C 八度一致），`WHITE_STEPS` / `BLACK_KEYS` 原样保留，只加 15 根白键列与 `keyboard_base_note()`；标签从「只标 C」扩成「C + 两端 G」。八度视图态语义从「左缘 C」改成「窗口括住的那个 C 八度」，`clamp(0, 8)` 不动。测试重写为 G-G 断言并补 MIDI 顶界；一个坑：15 等分让白键宽变成非精确浮点，测试取样点必须取列中间而非键缝（旧测试 196/14=14.0 的精确值把这层掩盖了）。

**门禁**：`cargo fmt` / `cargo clippy --workspace --all-targets` / `cargo test --workspace` = **1329 通过 / 0 失败 / 11 ignored**。提交 `83a0a4d`（timeline 面板键盘面）+ `ac85457`（vk 衬底与 G-G）。

#### 后续修：vk 琴条按 C 调形状排（2026-11-10）

用户指出「vk node 最低音是 G，但不是 transpose 到 G 大调，黑白键应该按照 C 调排列」，并给出模式串 `wbwbw wbwbwwbwbwbw wbwbwwbwbwbw w`。

**根因是基音与音程表不匹配。** `WHITE_STEPS` / `BLACK_KEYS` 两张表都是 **C 相对**的（C D E F G A B = `[0,2,4,5,7,9,11]`），但琴条的基音换成了 G。直接套用的结果：白键行算出来是 G 大调音阶 **G A B C D E F#**——F# 被当成白键画出来，而第 5 个黑键落在 F(65) 这个白键音高上。物理钢琴不可能把 F# 画成白键，这就是「transpose 到 G 大调」的具体形态。

**修法**：从 G 起算的白键表是 `[0,2,4,5,7,9,10]`（八度顶部那个 F 比 C 基准低半音）；黑键依附的白键索引也跟着挪——升号只坐在 G / A / C / D / F 之后，绝不坐在 B / E 之后（B-C、E-F 本来就是半音关系）。修完后 25 键的实际排列与用户给的模式串**逐键吻合**（用户串多出的 5 个字符正好是这条规则的自然延续 G#5 A5 A#5 B5 C6）。

**顺带修的标签锚点**：原来标签条件用**移调后**的音高选列（`sounding % 12 == 0`），移调一变标签就滑到别的键上——看着像标签跟着键盘走了。改成用**物理键**选列（C 与 G 两个地标），文字仍显示实际发声的音名。

**测试**：新增 `keyboard_strip_keeps_the_c_major_shape_from_g`，逐列断言 15 根白键（G3…G5）与 10 个黑键（G#3 A#3 C#4 D#4 F#4 G#4 A#4 C#5 D#5 F#5）的音高与位置。

**门禁**：`cargo fmt` / `cargo clippy --workspace --all-targets` / `cargo test --workspace` = **1336 通过 / 0 失败 / 11 ignored**。提交 `ef14e28`。

#### 后续修：vk 音区接管电脑键盘，双向联动（2026-11-10）

用户反馈两条：「vk node 上的 transpose 在电脑键盘上可以起作用，但 oct 调整对电脑键盘无效，应该是联动的才对」+「电脑键盘弹奏的时候，vk 上相应的琴键应该也要有按下的显示，是互相关联的」。

**根因一是两套互不相通的「八度」。** `Tr -/Tr +` 改的是文档字段 `NodeDoc::Keyboard.transpose`，电脑键盘路径 `vk_transpose()` 读它 → 移调对电脑键盘生效。`Oct -/Oct +` 改的却是视图态 `PatchState.keyboard_octaves`，唯一消费者是琴条的绘制与命中 → 电脑键盘根本不读它；电脑键盘的实际音高由**另一个全局 app 字段** `octave_base`（默认 60 = C4，`Z`/`X` 在 24–84 之间调）决定。两套八度各走各的，所以「移调管用、八度不管用」。

**根因二是高亮分居两处。** 电脑键盘按下的音高记在 app 级 `held_notes`，琴条高亮只读 patch 级 `keyboard_held`——后者只在鼠标点琴条时被填。两条路径之间没有任何连接。

**决策翻转（I-D1 B → C）**：八度一旦影响发声，就不再是 viewport 属性。改成文档字段 `NodeDoc::Keyboard.octave: i8`（serde `default = 4`，`PROJECT_VERSION` 不动），并让**节点拥有音区**：

- 琴条仍括 `G(octave)..G(octave+2)`（`keyboard_base_note`，公式不动，默认八度 4 → G3..G5）；
- 电脑键盘从**窗口的 C** 开始（`C(octave) = octave*12+12`）：17 个电脑键（半音 0..16）永远落在窗口内 → 音高映射天然成立，按下的键必然能对上琴条上的某个键；
- `Z`/`X` 与节点的 `Oct -`/`Oct +` 是同一个值（`Z`/`X` 在有节点时改节点的 `octave`，没有节点才动全局 `octave_base`）——一个音区，两处入口；
- 默认值零变化：八度 4 → 基准 C4 = 60，与改动前 `octave_base` 的默认值一致，老工程打开就是原样。

**高亮桥接**：电脑键盘的按下 / 松开写进 patch 级 `keyboard_held_keys`，**与鼠标的 `keyboard_held` 分开**——指针抬起的兜底清理只释放鼠标按下的键，否则会把用户还按着的电脑键盘键误发 Off，声音戛然而止。琴条绘制吃两个集合的并集。映射是纯音高映射（`strip_key_for`：音高落在窗口内 → 键就是它自己；落在窗口外 → 不亮，音区在按住时移动也不会留下假高亮）。

**顺带收掉的冲突**：卷帘 / 时间轴的模态里 `X`/`Y` 是轴选择器，音区不能同时动——`Z`/`X` 在模态激活时不改音区（`modal_transform_active()`）。

**已知遗留（未在本轮修，已于 2026-11-11 修复 → 见下一节）**：`refresh_vk_targets` 在路由变化时用**物理音高**发 Note Off，而按下时发的是**移调后**的音高——移调不为 0 时会留下卡住的音。这是既有缺陷（transpose 早就存在），与本轮的音区无关；彻底修法是让 `held_notes` 记「物理 → 实际发声」的映射。

**测试**：新增 6 条——model 的 `the_keyboard_register_survives_a_reload`（存盘往返 + 旧 payload 落到默认八度）、UI 的 `the_computer_keyboard_plays_the_node_register` / `every_computer_key_lands_inside_the_strip` / `a_note_outside_the_window_lights_nothing` / `the_keyboard_register_stays_in_range`。

**门禁**：`cargo fmt` / `cargo clippy --workspace --all-targets -D warnings` / `cargo test --workspace` = **1344 通过 / 0 失败 / 11 ignored**。

#### 后续修：移调后卡住的音（2026-11-11）

上节记的遗留缺陷：**Note On 发的是移调后音高，两处 Note Off 都不认它**。

1. `send_note_off` 重新用**当前** `vk_transpose()` 算一遍 → 按住某个键期间点了 `Tr -/Tr +`，松开时算出的音高与按下时不同，Off 打空；
2. `refresh_vk_targets` 遍历 `held_notes`（当时是 `HashSet<u8>`，**物理音高**）给掉线的目标发 Off → 移调 ≠ 0 时删掉输入线/输入节点会留下永远在响的 voice。

**修法**：`held_notes` 从 `HashSet<u8>` 换成 `HeldNotes(HashMap<u8, u8>)`，键是物理键、值是**按下那一刻真正发出去的音高**。封装类型刻意只暴露 `press(physical, sounding)` / `release(physical) -> Option<sounding>` / `is_held(physical)` / `sounding()` / `clear()`——**不提供物理键的迭代器**，以后想拿「正在响的音」只能走 `sounding()`，API 层面堵死复发。

- `send_note_off` 用 `release()` 返回的旧值发 Off，不再重算 transpose；返回 `None`（没有配对的 On）就干脆不发 Off，但**无条件**清 strip 高亮，避免残留；
- `refresh_vk_targets` 改用 `held_notes.sounding().collect()`；
- 去重判断 `contains(&note)` → `is_held(note)`；`file_ops` 的 `clear()` 语义不变。

**范围刻意收窄**：鼠标点琴条的路径（`PatchState.keyboard_held` + `PatchAction::KeyboardNote`）不动。同一个指针不可能既按住琴条上的键又去点 `Tr` 按钮，所以它的 On/Off 一定落在同一个 transpose 上；它也不经过 `refresh_vk_targets`。为它引入同样的映射只会多一份状态、没有收益。

**顺带修好**：按住键时改移调不再卡音（这是第 1 条的同一根因，之前没写进遗留清单）。

**测试**：`keyboard.rs` 新增 4 条纯逻辑回归——`a_transposed_note_releases_the_pitch_that_was_sent`、`a_transpose_change_while_held_keeps_the_original_release`、`a_route_change_releases_the_sounding_pitches`（核心：Off 列表是 `[62, 66]` 而不是物理 `[60, 64]`）、`clearing_drops_every_held_key`。

**门禁**：`cargo fmt --all` / `cargo clippy --workspace --all-targets -D warnings` / `cargo test --workspace` = **1348 通过 / 0 失败 / 11 ignored**。

### 系列实施补记（2026-11-11 自 plan.md 归位）

plan.md 此后只保留未完成项与活跃系列；下列系列的实施记录此前只存在于 plan.md，现按系列归位。与上方既有章节重复的以既有章节为准。

#### U1：transport 完整化 + seek 基建（2026-10-22）

- 引擎 `SequencerCommand::{Seek, Pause, SetLoopRange}`；`play()` 改为**从当前位置续播**（stop 回零 / pause 保位 / seek 清 pending offs）。
- `advance()` 重写为**分段调度**：块跨回卷点时同时调度尾段 `[t0, loop_end)` 与回卷头段 `[loop_start, …)`；loop 激活时抑制 D16 自停。
- pending off 按样本钟释放。**踩过的错**：想当然地按环绕映射 off 会把长音的 off 提早到错误的块；正确做法是让 off 按连续样本钟自然释放，不需要 wrap 映射。
- model：`ProjectMetadata.loop_range: SongLoop{enabled,start,end}`（serde default，版本不动）+ `markers: Vec<Marker>`（上限 32）+ validate。
- UI：transport 按钮组；Space 全局 play/pause（`egui_wants_keyboard_input` 守卫防劫持文本编辑）、Home/End；ruler 交互化——裸点击/拖 = seek scrub、Ctrl+拖 = 拉选 song loop（bar 吸附，一次拖 = 一步 undo）、拖 loop 边 = 单端重塑、右键 = 加 marker。undo/redo 与 `do_open` 经 `apply_document` 重推 loop range。
- 测试：引擎 7 新增 + model 3；sequencer 两条旧测试补显式 `seek(0)`（play 语义变了）。

#### U11：audio clip 裁剪 + 模态编辑（2026-10-26，spec `audio-trim.md`）

- 语义从 U6 的「变速跟随」改为「裁剪」：拖边缘调裁剪窗口、不变调。**核心代价**：clip 时长必须从 tick 域挪到秒域，属时间基修订而非字段加法。
- `AudioClip{path, start_ticks, source_offset(f64 秒), source_len(f64 秒), gain, legacy_span_ticks(read-only)}`；`Project::{sec_per_tick, audio_clip_span_ticks}` 派生 tick 长度；`migrate_v6_to_v7` + `PROJECT_VERSION` 6→7。
- 引擎：`RtAudioClip` 秒域 + `render_clip` 原生速率读（窗口外 / 文件尾静音）；`ProcessContext.sec_per_tick`（`Sequencer::base_sec_per_tick()`，**有意不跟随 tempo 曲线**）。
- `Placement` trait 方法加 `sec_per_tick: f64`（MIDI/control 忽略）；modal 几何 / targets / ghost / batch 两表 → 三表，`arm_modal` 放行 audio。
- 交互：T = 裁尾、`t+s` = 裁头、G = 移动（含跨 audio 轨）、Shift+D = 复制 + G；跨轨移动经 `relocate_in` + `sync_clip_row` 重推源/目标轨表，一步 undo。
- **已知取舍（用户确认接受）**：audio 不跟随 tempo 曲线，有曲线的段落会与 MIDI 错位；精确方案（tick↔秒积分 + `song_sec` 累积 + clip 表存 `start_sec`）见 spec §12，未实施。

#### U13：SoundFont 节点自足化（2026-10-28，spec `sf2-loader.md` D10–D17）

- 空节点**静音**（不回落内置合成器）：`SilentProcessor` 清零并显式吃掉 note/CC/bend 事件。
- `Sf2MasterVolume` 改名 `Sf2Gain`（线性 0..2，非有限值丢弃），构造期 `set_master_volume(1.0)` 抵消 crate 默认 0.5 的隐藏 -6 dB；`VoiceSpec::SoundFont { gain }`（`serde(default = 1.0)`，版本不动）。
- 字体缓存改为 `sf2_libraries` LRU（cap 2，用户拍板）+ 独立 `sf2_preset_names` 名表（淘汰后仍能列 preset）；`load_sound_font(doc_id)` = 解析 → 写文档 → `replace_node` 原位换装（保 node id，线缆 / 端口 / MIDI 路由零改动）→ 一步 undo。
- `BlockingJob` 三相位状态机（排队帧 → banner 帧 → 运行帧），banner 与插件扫描条同槽位；**不做异步**（用户拍板）。
- 节点内 `Load File…` / `Replace…` + `bank`/`patch` 自绘 chip（原生 `ComboBox` 在缩放绘制中会溢出节点）+ gain 滑条；Inspector 与节点调同一对函数（`pick_sound_font` / `set_soundfont_preset`）。
- **已知边界**：解析先 alloc `Vec<u8>` 再转 `Vec<i16>`，峰值 ≈ 2× 文件大小；2 GB 文件会冻结 UI 数秒（banner 只解决「看得见」）。

#### MIDI 事件处理节点 M1–M5（2026-10-29，spec `midi-nodes.md`）

- 一次架构投入：**块内事件 / 控制总线**（`Processor::process_events` 默认空实现、`NodeEvents` 定容 32、`EventCable` 参与 Kahn 排序）+ 三种节点（`Arpeggiator` / `Portamento` / `MidiEnvelope`）。
- `GraphCompiler` 维护独立 `event_cables` 表（不旁路、不混入音频电缆）；`connect_event` / `disconnect_event` + `EventFanInOverflow` / `EventFanOutOverflow` / `TooManyEventCables`。
- Arpeggiator：held 有序表 16、step 按 `song_ticks` 锚定（首次入块立即触发）、gate 先 off 后 on、Up/Down/UpDown/Random（seed = node_id 的 XorShift32）、octave 1..4、latch、blanket `Off{0}` 清空。
- Portamento：偏移初始 = `previous - new`，按 `exp(-dt/tau)` 收敛，支持 legato-only；MidiEnvelope：块级线性 ADSR、零时长段立即跳转、blanket → Release。
- 2026-10-30 增强：连线自动顶替（`clear_midi_sources` / `clear_control_sources`）、MIDI 线中点 `Insert MIDI FX…`、`MIDI FX…` picker；琶音加 harmony / skip / DownUp / 8 档 rate；滑音 `legato_only` 默认 false（每个音起始都从上一音高滑入）。
- **已知边界**：块粒度（≈2.7 ms）无采样内偏移；复音滑音不逐 voice 独立；transport stop 的 blanket off 只覆盖时间轴目标链首。

#### W 系列：鼓机 / clip 编排 / MIDI 生成（2026-10-31）

- **W1 鼓机**：`VoiceSpec::DrumKit { slots, gain }` + `DrumSlot{note,path,gain,pan,tune_semitones,choke}`；引擎 128 项 note 直索引表 + 24 槽 voice 池，全部构造期预分配；1× 原生速率、全 OneShot、choke group 10 ms fade。W1b 把 `note_name` 收进 `model::ops`（C4 = 60 为唯一实现，删掉 ui 三份拷贝），Inspector 键列可拖动改键（`move_drum_slot` 复用 `rebuild_drum_kit`，落点被占用时吸附最近空闲键，松手一次重建 = 一步 undo）。
- **W2 clip 平铺**：拆掉 UI 四处 `loop = span` 强制；`resolve_loop(current, span, requested)` 统一规则（拉长保留周期 → 平铺；缩短裁尾；显式请求钳进容器）。2026-11-04 修订：T / 拖右缘改为真正扩展时间，平铺独立成 **Shift+T** 模态（Alt+T 在 Windows 触发 `SC_KEYMENU` 吞后续鼠标事件，仅保留备用）。
- **W3a clip 命名**：三类 clip 加 `name`（serde default + 空值不序列化，版本不动），`F2` / `Alt+双击` 就地 `TextEdit`，一步 undo、不推引擎。
- **W3b clip 编组**：`Project.clip_groups` + 三类 clip 的 `group: Option<u32>`；`Ctrl+G` 建组 / `Ctrl+Shift+G` 解组；选区扩组在 `show()` 末尾幂等收敛，`Alt+单击` 临时拆组；组内混合类型（MIDI + Curve + Audio）在 G / Shift+D 下作为刚性组一起移动。组名编辑 / 成员手动增删仍为后续项。
- **W4 随机化 / W5 strum**：`RandomizeOptions` + `randomize()`、`StrumOptions` + `strum()`，均 seeded、原地、不改 slice 长度；命令 `midi.randomize` / `midi.strum` + roll 工具栏面板。strum 的 UI 默认值问题见 I5。
- **W3c**（全选与拖出 clip 撤销修复）见上文独立章节。

#### LFO 控制源节点（2026-10-31，spec `lfo.md`）

- 1 control out，输出归一化 `[-1,1]`，可直连参数 / CC / pitch bend，或先过 Curve Map。
- 参数：Waveform（Sine / Triangle / Saw / Square / SampleHold）、Rate（0.01–40 Hz）、Sync + 分频（1/1…1/32）、Depth（0–1）。
- 时间基准：Hz 模式累计播放秒数；sync 模式由 `song_ticks` 派生（跟随 tempo 曲线 / seek / loop）。停止时冻结相位**且不投递**；Stop→Play 从 0 重启，Pause→Play 继续。
- **已知边界**：输出是**绝对覆盖**而非相对调制；无 MIDI key-sync、Phase / Offset、Reverse Saw。

#### X 系列：symphonia 解码（2026-11-04，spec `audio-decoding.md`）

- 选型：`fundsp` 不引依赖（只读公式写标量版）；`symphonia 0.6.1` 整体引入（MPL-2.0，`default-features = false` + 显式 `flac / mp3 / mpa / ogg / vorbis / aac / isomp4 / aiff / pcm / wav`）；`midly` / `dasp` 不引入。
- `engine/src/sample.rs` 拆 `decode_wav`（hound 原逻辑零改动）与 `decode_symphonia`；`.wav` 走前者，其余按扩展名 hint + 内容探测走后者；`AUDIO_FILE_EXTENSIONS` 是格式清单唯一真源。
- **0.6 API gotcha**：`Probe::probe()`（不是 `format()`）、`CodecRegistry::make_audio_decoder()`（不是 `make()`）、`next_packet() -> Result<Option<Packet>>`、`Track.codec_params` 是 `Option<CodecParameters>` 要 `.audio()` 取出、`Hint` 在 `core::formats::probe`、`decode()` 返回 `GenericAudioBufferRef`；`copy_to_vec_interleaved` 会 `resize` 目标 Vec（覆盖而非 append），每包复用 scratch。
- 验收含「同一 fixture 两条分路逐帧相等」（两边 i16 都除以 32768）；未入库真实 flac/mp3 fixture。RT 侧 / `SampleData` / 文档版本全不动。

#### 录音引擎链路 R1–R8（2026-11-01，spec `recording.md`）

- **R1 节拍器**：构造期预生成重拍 / 弱拍 click 表；`Sequencer` 增 `numerator` / `denominator` + `beat_ticks()`；`BlockRunner` 在 `process_block` **之后**混入（不进节点图、不进主电平表），Play/Stop/Pause/Seek 时 `resync`；离线导出恒关闭。
- **R2 capture**：`realtime/capture.rs`（`CaptureTap` + `CapturedNote` + 8192 ring + 溢出计数）；`EngineCommand::SetCapture`；`BlockRunner` 在 Note 入口先取 `position_ticks()` 再入库——**sequencer 排的音符不经命令队列，天然不入 ring**。
- **R3 midir**：`CommandSender(Arc<Mutex<Producer>>)` 让驱动回调直推命令队列；`MidiInputHub` 差量开合 + 回调内零分配的**路由快照**；`EngineCommand::CtrlAtNode` 接进既有 `dispatch_ctrl`。设备绑定是文档编辑（一次 undo），不是应用配置。
- **R6 轨输入口**：`PortRef::TrackIn{kind,track}`（v10，无迁移）+ `GraphDoc::{track_input_source, track_input_tracks, node_track_inputs}`；录音归属由 `Timeline In` 的轨输入口连线决定（默认工程不预接）。`SequencerCommand::SetRecordMute` 只丢 On。
- **R4 音频录音**：`NodeDoc::Input` + `crossbeam-queue` 双无锁 ring（monitor + take，Arc 句柄跨图重建存活）；cpal 输入流按引擎采样率打开、f32、无重采样；`realtime::input::InputProcessor`（通道选择 / gain / 欠采样计数）；`.media/` 写 32-bit float mono wav（相对路径）。
- **R7 / R8 烘培**：`RtGraph` 烘培表 + 块末 `baked_notes` / `baked_controls`；MIDI 在事件派发处抓取（Arp / Portamento 输出），控制源在块边界抓值（LFO / Envelope / Portamento / CurveMap）。限制：Timeline 曲线出口 → 曲线输入仍未采样，见 spec R8 备注。
- UI 侧输入控件归位见上文「录音输入 UI 归位」章。

#### S1-1 / S1-2 / S1-3a（2026-11-01，review 修复）

- **S1-1 启动默认不发声**：`Args.no_autoplay` → `Args.autoplay`（默认 `false`），`--autoplay` 显式开启；`--note` / `--freq` 视作显式请求（隐含 autoplay，保住旧冒烟脚本）；`--no-autoplay` 保留为兼容别名。usage 与 `main.rs` 头注释同步（原写 "roadmap step 3"）。
- **S1-2 原子保存 + 备份 + 拒绝未来版本**：`写 <name>.glz.tmp → sync_all → copy 旧文件到 .bak → rename` 覆盖；备份是 **best effort**（`.bak` 被占用不该让一次成功的保存变成失败）。顺带用 `ProjectFileRef<'a>`（只 derive `Serialize`、借用文档）去掉保存路径上一次 `Project` 全量深拷贝；`parse_project` 新增 `version > PROJECT_VERSION` 拒绝（`ModelError::NewerVersion`，文案带两个版本号）。
- **S1-3a undo 栈按字节预算裁剪**：`Project::approx_bytes()`（`Vec` 按 `len × size_of::<T>()` 计价，忽略不会长大的标量载荷）+ `NodeDoc::payload_bytes()` / `VoiceSpec::payload_bytes()`（插件 state blob、采样路径）；`DEFAULT_HISTORY_BYTES = 256 MiB`，`HistoryEntry.bytes` 在入栈时缓存。`trim_history()` **先按条数裁、再按字节裁，且先丢 redo 尾部、再丢 undo 最旧**，永远保留 ≥1 步（单步就超预算时也不清空，否则重工程等于没有 undo）。`undo` / `redo` 在栈间搬运时同步维护字节数。
- **S1-3a 踩坑（真 bug）**：初版 `take_oldest_undo/redo` 用 `drain(..1)`，在**空栈**上越界 panic——预算裁剪第一次就会走到；改为 `drain(..len.min(1))`，由 `one_oversized_step_still_leaves_undo_usable` 捕获。
- **S2-4 的立项依据（P1-4）**：`begin_param_edit` 持有文档 `Arc` 克隆 → `project_mut()` 的 `Arc::make_mut` 发现 refcount ≥ 2 → **每帧整体深拷贝**；当时全仓该模式 62 处，密集工程会明显掉帧。这是「快照只在首帧取、值变化才写、release 提交」手势 API 的直接原因。

#### G4 / G5：面板按选中节点 + 节点内联参数（2026-10-20）

- **G4**：patch 双击节点 → `OpenNode` 跳 Inspector + `selected_doc_nodes()`；`resolve_panel_target` 纯函数（文档序首个可编辑节点，无选中回落 selected_track 乐器）；六个乐器会话缓存键从 track 改为 **doc node_id**（与效果器缓存对齐）；synth 面板按 `NodeDoc` 变体分派（Instrument / Effect / Mixer），滑条下发 `send_node_param`。
- **G5**：静态注册表 `SYNTH_INLINE`(cut/res) / `DELAY_INLINE`(time/fdbk/mix) / `MIXER_INLINE`(pan)，插件节点走 `PatchDeps.plugin_inline` 取前 2 个枚举参数；`InlineParam` + `PatchAction::SetInlineParam`；节点高度公式加 `inline_row_count × INLINE_ROW_HEIGHT(16)`；滑条在 `show()` 尾部用 `ui.put` 后置注册，**后层赢得指针 → 拖滑条不改节点位置**。

#### 视图标签 / 全选补充 / Edit 页纵向空间（2026-11-05）

- 标签顺序由 `MainView::ORDER` 单一来源驱动（标签栏 / Tab 循环 / Alt+1..4 都读它）；裸 Tab 从此全局归视图切换，卷帘 clip 焦点切换改 `Ctrl+Tab` / `Ctrl+Shift+Tab`。最终顺序 `Timeline → Edit → Patch → Inspector`。
- `Ctrl+A` / `Ctrl+Shift+A` 全选是**视图级**命令（`all_clips()`：MIDI → control → audio 表序），因为行级 handler 只看得见自己那一轨。同时修「音符拖出 clip 后 Ctrl+Z 没反应」：`clamped_move_deltas` 从只钳 start 改为连尾巴一起钳（`loop_ticks - length - start`），指针已抬起时丢弃残留 `drag` / `marquee`。
- Edit 页纵向空间：timeline `ROW_HEIGHT` 28→40；piano roll 改为吃满 `available_height - (velocity + overlay lanes + spacing)`（下限 `ROLL_MIN_HEIGHT`，`ROLL_HEIGHT` 降级为无边界兜底）；Edit 页 MIDI 目标去掉外层 ScrollArea，Event List 进 `Panel::bottom`（默认 1/3 高、可拖分隔条）；默认时间缩放 fit 基准加两小节下限。

#### patch timeline 三类轨统一增删（2026-10-26）

- timeline 工具栏与 patch timeline 节点都补齐 `+/- MIDI`、`+/- Curve`、`+/- Audio` 三对按钮，走同一批 `add_*` / `remove_*`；`TIMELINE_ACTION_ROWS` 2→3。
- **audio 节点折入是呈现层合并**：`ensure_positions` 跳过 `NodeDoc::AudioTrack`（不进 positions → 不绘制 / 不命中 / 不框选），`patch_port_from` 把该节点映射到 timeline 的端口，`timeline_port_kind` 成为端口→轨唯一真源；文档 / 引擎拓扑不动，音频信号仍由生成器节点产生。端口序 MIDI → Curve → Audio。

#### I 系列决策（I-D1…I-D7，2026-11-10）

- **I-D1 八度与移调存哪**：原定「八度视图态 + 移调文档字段」，同日修订为 **C：两者都是文档字段**——八度一旦影响发声就不是 viewport 属性；节点拥有音区，电脑键盘从窗口的 C 开始，`Z`/`X` 与 `Oct -/+` 是同一个值。实施见上文「vk 音区接管电脑键盘」。
- **I-D2 工具栏分组**：模式组（互斥、持久、pressed 态）+ 动作组（一次性、armed 描边）。卷帘模式组 = `[Select][Draw]`；Grab / Scale / Stretch / Knife 是动作，点按钮 == 按一次对应快捷键；timeline 无持久模式，只有动作组。
- **I-D3 试听是否受录音影响**：v1 照常走 `EngineCommand::Note{track}`，录音 armed 时会被 capture（按下听到的就是会录下的）。
- **I-D4 timeline 不补独立 Scale**：`T`（Stretch）就是时间缩放，按钮写 `Stretch (T)`。
- **I-D5 timeline 不补 Draw**：创建语义已由双击承担（MIDI/control 建空 clip、audio 导文件），再加 Draw 会与双击和 G 的拖拽选区混。
- **I-D6 strum 单位改音符分数**（1/32、1/16…）+ 悬停显示 `@BPM` 毫秒数。
- **I-D7 头裁内容语义**：单周期 clip（`loop == span`）走真头裁（`retain_from`：内容前移、裁掉开头、跨界音符保留尾巴）；tiled clip 只能容器截断并给状态栏提示；MIDI/control 头裁只能向右，音频靠 `source_offset` 可向左回补到 0。

#### Met M3：拍号变化求值层（2026-11-11，spec `meter-map.md`）

- `MeterMarker { tick, signature }`（`serde(default)`，空表 `skip_serializing_if`）+ `MAX_METER_MARKERS = 64`；`PROJECT_VERSION` 10 → 11，**纯加法、无迁移函数**——v10 文件读出空表，单段行为逐字节不变。
- `validate`：每项签名合法、`tick > 0`、严格升序、总数 ≤ 上限；**不校验「落在小节线」**，宽容读取交给求值层向下吸附（D2/D3）。
- `model/src/meter.rs`：`MeterMap::new(base, &markers)` 一次展开成 `MeterSegment { tick, bar, signature }` 段表（乱序输入内部排序，半小节 marker 向下对齐到所在段网格，tick 0 / 重复起点丢弃）。查询全部二分 + 整数运算：`at` / `ticks_per_bar_at` / `ticks_per_beat_at` / `bar_beat_at` / `bar_start` / `next_bar` / `prev_bar` / `snap_bar`（吸附基准是**当前段**网格，D5 的像素线性由此不受影响）。
- 空表 = 单段，`Project::{meter_map, bar_beat_at, ticks_per_bar_at}` 是 M4 的唯一读取入口；`ticks_per_bar()` 保留为第 0 段快捷方式。
- 测试 +13：`meter.rs` 7 项（单段 / 跨段小节编号连续 / `bar_start` 往返 / 半小节吸附 / 乱序输入 / 段内吸附 / 跨段 `next`·`prev`）、`lib.rs` 5 项 validate（合法 / tick 0 / 乱序重复 / 超上限 / 非法签名）、`file.rs` 1 项 RON 往返 + 空表不落盘。
- **门禁**：`cargo test -p glitcherz-model` = 194 通过；`fmt` / `clippy -D warnings` 干净。本阶段无调用点改动，零回归。

#### Met M4：timeline 读取层分段（2026-11-11，spec `meter-map.md` §6）

- `TimelineParams.bar_ticks: u32` → `meter: &MeterMap`；`px_per_tick = px_per_bar / 第 0 段小节长`（D5 像素仍全局线性，只有网格 / 标号 / 吸附按段变化）；内容宽度 = `meter.next_bar(ARRANGEMENT_LIMIT_TICKS)`（无 marker 时与旧 `div_ceil × px_per_bar` 逐像素一致）。
- 新增纯函数 `for_each_bar_line(meter, from_tick, to_tick, f(bar, tick, seg_bar_ticks))`：ruler 与每条 lane 共用，视口裁剪；段边界只由「下一段」发出，标号是**全局**小节号。ruler 在段起点画加粗线 + 拍号胶囊（只读，M5a 接编辑），标号每 4 小节按**段内**小节像素宽度稀疏化。
- 吸附：`snap_bar(meter, raw, snap)` 按 raw 所在段取网格（跨段拖动在边界切换基准）；`Snap::OFF`（Ctrl）落最近 tick。`knife_target` / `trim_head_start` 的「至少/保留一小节」用落点 / clip 起点所在段。模态 G / T / Tile 的步进取**选区最早 clip** 所在段（S7：模态仍是整小节）。
- 其它读取点：状态栏 `Project::bar_beat_at`；`seek_by_bars` 用 `next_bar` / `prev_bar` 分段步进；`arrangement_end_ticks` 与 `export_end_ticks` 用 `next_bar(latest - 1)`（结束点已在线上就保持该线）；录音起点 `next_bar(now)`、录音 take 名字用 `bar_beat_at`；新建 clip 的默认长度取起点段。卷帘网格按段循环（段起点相对 clip origin），fit / randomize / strum 范围取 origin 段。
- **与 spec §6.4 的偏离（记录在案）**：`MeterMap` 目前由 `timeline_ops` / `editing` 每帧各构造一次，尚未升级为 `GlitcherZApp` 字段缓存——缓存需要「文档变更即重建」的钩子，等 M5a 加入 marker 编辑时一并做，避免现在引入缓存过期风险。表上限 65 项，每帧一次小 `Vec` 分配的代价可忽略。
- 测试 +2（`bar_lines_follow_meter_segments` 锁段内网格与连续编号、`snapping_switches_basis_at_a_meter_change` 锁跨段吸附 + Ctrl 关闭）+ 5 个既有调用点改传 `MeterMap`。
- **门禁**：`cargo fmt` / `cargo clippy --workspace --all-targets -D warnings` / `cargo test --workspace` = 43 个 test binary 全 `ok. 0 failed`。

#### Met M5a：meter 标记编辑（2026-11-11，spec `meter-map.md` §6.3）

- ruler 胶囊交互：**左键拖动**（按段小节吸附，按住 Ctrl 不吸附；`clamp_meter_tick` 保证 `tick > 0` 且不越过相邻 marker）；**双击**打开工具栏里的内联拍号编辑器（复用 `meter_ui::meter_editor`，即时改值）；**右键**删除；**Shift+右键空白**新建（tick 已按段吸附）。
- `TimelineAction` 新增 `AddMeterMarker` / `MoveMeterMarker` / `SetMeterMarker` / `EndMeterMarkerEdit` / `RemoveMeterMarker`；`TimelineState` 新增 `meter_drag` / `meter_edit`（`meter_edit` 为 `pub(crate)`，app 在新建后自动打开编辑器）。
- app 侧：排序插入（初始签名 = 所在段拍号，等于没改，直到用户编辑）、同 bar 已存在时只提示、`tick == 0` 拒绝（D3 的 base meter 归 `metadata.time_signature`）、上限 `MAX_METER_MARKERS`。拖动与数值手势走 `commit_param_edit`（key `meter-marker-move` / `meter-marker-value`），`EndMeterMarkerEdit` 收手势——一次拖动 / 一次数字拖动 = 一步 undo。
- **与 spec §6.3 的偏离（记录在案）**：没有引入右键菜单，新建走 `Shift+右键`，改值走工具栏内联编辑器而非 Inspector——保留既有的「右键即加命名 marker」习惯，且 Inspector 是节点参数面板，不适合常驻一条与选中节点无关的拍号区。
- 测试 +1：`meter_drag_stays_sorted_and_off_tick_zero`（tick 0 与邻位钳制）。
- **门禁**：`fmt` / `clippy -p glitcherz-ui --all-targets -D warnings` 干净；`cargo test -p glitcherz-ui` = 11 个 test binary 全 `ok. 0 failed`。

#### Met M5b：引擎段表 + SMF 多段（2026-11-11，spec `meter-map.md` §7 / D7）

- 引擎：`commands::MeterSeg { tick, bar, numerator, denominator }`（`Copy`）+ `MAX_METER_SEGS = 65`（64 个 marker + base 段）+ `SequencerCommand::SetMeterMarkers { count, segs }`。`Sequencer` 新增定容 `meter_segs` / `meter_len` / `meter_index`；`bar_ticks` / `numerator` / `denominator` 降级为**当前段缓存**，由 `sync_meter_segment()` 每块（block runner 在 `metronome.tick` 之前）重锚。`set_meter_markers` 做段表规整（seg0 固定 tick 0、tick 严格升序，乱序后续项丢弃），`set_time_signature` 同步写 seg0。
- `refresh_arrangement_end` 改为 `next_bar(latest - 1)`（段内小节线取整，结束点已在线上保持不变）；`next_bar` 是引擎侧纯整数实现。
- 节拍器：`Metronome` 记录 `last_beat_ticks` / `last_seg_start`；跨段时 `resync(&sequencer)` 并让该块静音，拍索引改为**相对当前段起点**（6/8 段从自己的第 1 拍数，重音落在段首）。`resync` 签名从 `(pos, beat_ticks)` 改为 `(&Sequencer)`，四处 transport 调用点同步。
- UI：`push_time_signature` 现在同时下发 base（`SetTimeSignature`）与整张段表（`SetMeterMarkers`），幂等；marker 增删/移动/改值都会调用它，加载与 undo 走同一入口。
- SMF：导出 conductor 在 tick 0 写 base，再在每个 marker tick 写一条 `FF 58`（delta 递增）；导入收集**所有** `FF 58`（绝对 tick），排序后最早一条作 base、其余作 `meter_markers`（同 tick 去重）。`SmfImport` 增加 `meter_markers`；空白工程导入时 base + markers 一起继承（`file_ops`），非空工程保留工程拍号并提示。
- 测试：model 新增 `meter_changes_round_trip_as_multiple_ff_58_events`（3 条 `FF 58` + 往返相等）；metronome 四处 `resync` 调用点改签名。
- **门禁（2026-11-11 实跑）**：`cargo fmt --all` / `cargo clippy --workspace --all-targets -- -D warnings` 干净；`cargo test --workspace` 日志中 **43 个 test binary 全 `ok. 0 failed`**，无 `FAILED` / `panicked` / `error`。补跑前修过两处：`sequencer.rs` 的 `MeterSeg` import 误落在测试模块（主模块找不到类型），SMF 测试 `windows(3)` 的结果是 `&&[u8]` 需解引用；clippy 另要求 `max(1).min(N)` → `clamp`。

#### Aud3：Edit 独立标尺 + 播放头（2026-11-11，spec `edit-audition.md` §5.4–5.6）

- `piano_roll::show` 新增 `clip_span`（= `span_ticks.max(loop_ticks)`）与上层 `RULER_HEIGHT = 20` 的标尺：位置在工具栏与音符区之间，左侧 `KEYS_WIDTH` 列写 `bars`，右侧按 **meter 段**画小节号（与 timeline ruler 同一分段逻辑，段内每 4 小节稀疏化），并画 clip 起止边界、静态播放头（播放时加粗）。
- 标尺交互**不注册 widget**：`primary_down && press_origin 在标尺内` 时发 `RollIntent::Seek`，由 `editing.rs` 直接 `engine.seek_ticks`（不改文档、不进 undo）。若在标尺上 `ui.interact`，会抢走音符网格的 drag 归属。
- `ViewWindow::of` 与 overlay lane 的窗口右边界从 `loop` 扩到 `span`，平铺区可横向滚到；主卷帘仍只展开一个内容周期（D7）。
- 播放头：新增纯函数 `playhead_local(playhead, origin, loop, span)`，`Some(rel)` 仅当位置在 clip 内且落在首个内容周期；主区与 velocity 泳道共用它，`origin` 相对相位修正了旧的 `rem_euclid`（clip 起点非整周期会偏）。停止时按 1px 暗黄静态绘制。
- **knife 回归的真正根因**：标尺让音符区下移后，测试 fixture 的 y 校准点（2px 步进得到的第一命中点）与真实行矩形可差 1–2px，而 `knife_crossing` 的沿行判定用的是**精确** hit 矩形 —— 点击能中（`note_at` 有 HIT_SLOP），拖动却一个 cut 都收不到。修法：沿行判定改用 hit 矩形并上下各留 2px 容差（`(hit.top()-2)..=(hit.bottom()+2)`），与「选择已经宽容」的口径一致。`roll_state_machine` 的 2 个 knife 拖动用例恢复。
- 测试：`playhead_local` 的 7 条相位断言；`the_view_window_is_the_whole_context` 改为按 `span` 断言（期望 3840）。Aud 系列此前未提交的 `stash@{0}` 由此手工合并（未 pop，避免与 Met M4 的 `piano_roll.rs` 冲突）。
- **门禁**：`cargo fmt --all` / `cargo clippy -p glitcherz-ui --all-targets -D warnings` 干净；`cargo test -p glitcherz-ui` = 11 个 test binary 全 `ok. 0 failed`；`cargo test --workspace` = 43 个 test binary 全 `ok. 0 failed`。

#### 命令族扩展：Quantize / Legato / Glue / Remove Overlap / Transpose（2026-11-11 交付）

- `midi.quantize` / `midi.transpose` / `midi.duration_op {op:"legato"|"remove_overlap"}` 此前已有；本次补 `midi.legato`、`midi.glue`、`midi.remove_overlap` 三个薄命令，并给 quantize / transpose / legato / duration_op / remove_overlap 的返回值补 `indices`（`locate` 重建），供 UI 在命令重排音符后保持选区。
- `glue`：`model::ops::glue_selected(&mut Vec<Note>, GlueOptions { gap_ticks }, selected)` —— 唯一 **缩短**音符表 op（与 `notes_split` 同类例外）：同 `note` + `channel` 且 `next.start <= cur.start + cur.length + gap` 的相邻音符合并到较早者，end 取最晚；选区外不动；返回幸存值供 `locate`。命令返回 `{merged, indices}`。
- UI：卷帘工具栏新增 `Notes` 菜单（Quantize / Legato / Glue / Remove Overlap / Transpose ±1 ±12）——菜单顶部是 `strength`（量化强度 0–1）与 `gap`（legato/glue 空隙 ticks）两个 `DragValue`，存在 `RollState::midi_strength` / `midi_gap`（view state）。快捷键 `Q` / `L` / `R` / `Shift+G`。`RollIntent::MidiOp { op, indices }` 只表达意图，参数由 `MidiOp` 变体自带（`Quantize { strength, grid_ticks }` / `Legato { gap }` / `Glue { gap }`），`editing.rs::apply_midi_op` 翻译成命令并写回 `indices`；选区为空 = 整 clip（命令层 `indices?` 语义）。
- 测试：model 2 项 glue（合并 / gap + 选区），command 3 项（legato 延长 + undo、glue 合并 + indices、remove_overlap 截断），MCP 命令数 16 → 19。
- **门禁**：`cargo fmt --all` / `cargo clippy --workspace --all-targets -D warnings` 干净；`cargo test --workspace` = 43 个 test binary 全 `ok. 0 failed`。

#### 命令词汇表 V1a：NodeParam 稳定名 + graph.set_param/query（2026-11-11 交付）

- model：`NodeParam::key()`（34 个 snake_case key，公开 API）+ `from_key()`（无参变体）/ `from_key_indexed()`（`effect_slot` / `organ_bar`）；`ChannelParam::key/from_key`；`node_param_range`（与 `node_param_map` 的常量域一致）、`node_param_value` / `set_node_param`（按节点类型 match 并 clamp 到域）、`node_param_keys`（query 用）、`NodeDoc::kind_key()`。
- 命令：新 `glitcherz-command/src/graph.rs` —— `graph.set_param {node, param, value, index?, channel?, channel_param?}`（`node_id` 寻址、域 clamp、类型不匹配报错）与 `graph.query`（节点 + 支持的参数 key / value / min / max）；注册后命令数 19 → 21。
- UI 无需改动：`execute` 后 `sync_engine` 在拓扑不变时走 `refresh_clips_and_transport`，其中已 `push_node_params` / `push_mixer_params`，参数即时生效。
- 测试：model `node_param_keys_round_trip`（key 往返 + 下标边界）、`node_params_read_and_write`（synth attack clamp、mixer pan/mute、跨类型拒绝）；command `graph_set_param_and_query_round_trip`（写入 / 读回 / 超范围 clamp / 未知节点报错）；registry 的「非 query 命令都 journal」断言放行 `graph.query`。
- **门禁**：`cargo fmt --all` / `cargo clippy --workspace --all-targets -D warnings` 干净；`cargo test --workspace` = 43 个 test binary 全 `ok. 0 failed`。
- **V1a 覆盖边界**：`effect_slot` 与 Modal / Pluck / Organ / Wavetable / Sf2 / Sample 参数尚未读写（`set_param` 返回 unsupported），`graph.set_voice` 未做 —— 归 V1b。

#### 命令词汇表 V2：clip.add（2026-11-11 交付）

- 新 `glitcherz-command/src/clip.rs`：`clip.add {track, lane? midi, start_ticks? 0, span_ticks? bar, name?} → {clip, start_ticks, span_ticks, loop_ticks}`。
- 默认 `span_ticks` = 起点所在拍号段的一小节（`Project::ticks_per_bar_at`，Met M4 语义），`loop_ticks = span_ticks`（一个内容周期 = 整段）；`start + span` 收敛到 `MAX_ARRANGEMENT_TICKS`；上限 `MAX_CLIPS_PER_TRACK`；`lane` 只接受 `midi`（control / audio 需目标 / 媒体路径，归后续）。
- 命令数 21 → 22；新 clip 可直接被 `midi.note_add` 写入（命令按 `track` + `clip` 索引寻址）。
- 测试：`clip_add_creates_an_empty_midi_clip`（追加、命名、`span == loop`、空内容、可写音符、`lane` 非 midi 报错）。
- **门禁**：fmt / clippy workspace 干净；`cargo test --workspace` = 43 个 test binary 全 `ok. 0 failed`。

#### 命令词汇表 V3：端到端（2026-11-11 交付）

- 新集成测试 `mcp_cli::scratch_build_adds_a_clip_notes_and_renders`：真实启动 `--mcp` 进程，`graph.query` 找 instrument 节点 → `graph.set_param attack` → `clip.add`（从报告取 `clip` 索引）→ `midi.note_add` + `midi.transpose` → 两次 `render_wav` 对比，断言有音符后渲染字节不同（证明命令真的进了音频路径）。
- `docs/manual.md` 的「命令行：批量命令」补三行命令表（`clip.add` / `graph.query` / `graph.set_param`）与「`commands_run` 的 `results` 带回 clip 索引」的用法说明。
- **门禁**：fmt / clippy workspace 干净；`cargo test --workspace` = 43 个 test binary 全 `ok. 0 failed`（新增集成用例计入 `mcp_cli`）。
- **V1b 待做**：`effect_slot` 读写、声源参数（Modal / Pluck / Organ / Wavetable / Sf2 / Sample）、`graph.set_voice`；这三项是「换音色」诉求的最后一段。

#### 剪贴板 C1–C2：Ctrl+C/V + Patch Shift+D（2026-11-11 交付）

- 新模块 `glitcherz-ui/src/clipboard.rs`：`Clipboard::{Clips, Notes, Nodes}` 存**值快照**（不存选区索引，undo / 删除后仍可粘贴）；纯函数 `pasted_starts` / `pasted_notes` / `locate_notes` 与 `NodesCopy::{collect, placed}`。`GlitcherZApp.clipboard` 在 `do_new` 与 `adopt_document` 清空（快照引用旧工程的轨道 / 节点 id）。
- 各视图只消费按键并返回意图，不碰剪贴板：Timeline `CopySelection` / `PasteClips`（`has_focus`）、钢琴卷帘 `CopyNotes` / `PasteNotes`（`has_focus`）、Patch `CopySelection` / `PasteNodes { at }` / `DuplicateNodes { at }`（`hovered`，后者即 `Shift+D`）。键位统一用 `Modifiers::COMMAND`（Win/Linux = Ctrl，macOS = Cmd）。
- 锚点：Timeline / Edit 用 `engine.musical_ticks()`；Edit 再减 `clip.start_ticks` 并 clamp 0（clip-local）。Patch 用新增的 `PatchState::to_graph(rect.min, pointer)`（`to_screen` 的逆变换）。簇内相对偏移保持：Timeline 以最早 `start` 对播放头，Edit 以最早音符对播放头，Patch 以包围盒左上角对光标。
- 粘贴走编辑守卫、**不新增命令**（避免 CLI/MCP 命令计数变化）：Timeline 克隆快照 push 后 `sync_clip_row`；卷帘 `notes_mut().extend + sort + locate_notes` 并切选区；Patch `NodeDoc::set_node_id`（model 新增）分配新 id → `install_node` → `connect_audio` / `connect_event_cable`，`suspend_history` 包住内部 commit，外层一次 `commit_edit`。
- Patch 只复制**两端都在选中集内**的线（外部线不复制，避免把未选节点拖进副本）；引擎拒绝的节点（槽满 / 插件加载失败）单独回滚文档 + `engine.remove_node` 后跳过，其内部线一并丢弃，状态栏报跳过数量。
- 测试：`clipboard` 5 条（相对偏移、负锚点 clamp、first-match 定位、包围盒对齐、内部线筛选）+ `patch` 的 `ctrl_c_on_the_patch_requests_a_copy`（真实 egui 帧 + hover + `COMMAND`）。
- **门禁**：`cargo fmt` / `cargo clippy --workspace --all-targets -D warnings` 干净；`cargo test -p glitcherz-ui` 与 `cargo test --workspace` 全 `ok. 0 failed`（FSM 的 FAIL 为已知误报，以原始 `test result` 与退出码为准）。
- **事件链勘误（2026-11-11，用户实测）**：首版用 `consume_key(COMMAND, C/V)`，真实窗口里 `Shift+D` 生效而 Ctrl+C/V 完全无响应。根因在 `egui-winit 0.36.2 on_keyboard_input`：`is_copy_command` / `is_paste_command` 命中后只推 `Event::Copy` / `Event::Paste(内容)` 并 `return`，**不再推 `Event::Key`**；且系统剪贴板为空时连 `Paste` 都不推。修复：视图改消费 Copy/Paste 事件（`clipboard::take_copy_event` / `take_paste_event`，从 `InputState.events` 中移除），Key 路径保留给 headless / 非 winit 后端；复制时 `ctx.copy_text(sentinel)` 写一条哨兵，保证下一次 Ctrl+V 一定有 `Paste` 事件。测试改为注入 `Event::Copy` / `Event::Paste`，并新增 Patch 粘贴分发用例。

#### 命令词汇表 V1b-1：声源参数 + graph.set_voice（2026-11-11 交付）

- model：`node_param_value` / `set_node_param` 增加 `VoiceSpec` 分支（`voice_param_value` / `set_voice_param`）——覆盖 Modal（decay / beating / hardness / brightness / gain）、Pluck（decay / damping / brightness / release / gain）、Organ（9 个 drawbar + percussion / decay / attack / release / gain）、Wavetable（position / attack / release / gain）、SoundFont gain、Sample gain；全部按 `node_param_range` 的域 clamp（Organ drawbar 取整、越界 index 返回不支持）。
- `node_param_keys` 按 `VoiceSpec` 返回可写 key：Builtin → synth 五参数；Modal / Pluck / Wavetable → 各自；Organ → 9 个 `organ_bar` + 5 个宏观参数；SoundFont / Sample → 各自的 gain；插件 voice 为空。
- 新命令 `graph.set_voice {node, voice: builtin|modal|pluck|organ|wavetable}`：切换 `VoiceSpec` 到对应内置音色的默认参数；只对 instrument 节点有效，文件音色（SF2 / sample）与插件拒绝。命令数 22 → 23。
- 测试：model `voice_params_read_and_write`（Modal decay、Organ bar clamp / 越界、Sf2 gain、keys 按 voice）；command `graph_set_voice_switches_and_exposes_voice_params`（切换 → 调 `modal_decay` → 文档读回 + query 只列该 voice 参数 + `sf2` 拒绝）。
- **门禁**：`cargo check -p glitcherz-model -p glitcherz-command --all-targets` / `cargo clippy -p glitcherz-model -p glitcherz-command --all-targets -- -D warnings` 干净；`cargo test -p glitcherz-model voice_param` 与 `cargo test -p glitcherz-command --test transactional graph_set_voice` 均 `ok. 1 passed; 0 failed`。**全量 `cargo test --workspace` 本轮无法跑完**：工作区 UI 有未提交的 F-040 剪贴板在途改动（`timeline.rs` 引用 `clipboard::…` 而 `lib.rs` 缺 `mod clipboard;`），与本轮无关、未触碰。
- **V1b-2 待做**：`effect_slot` 读写（需把 `glitcherz-ui::effect` 的槽映射下沉到 model，`EffectParams::slot_value` / `set_slot_value` / `slot_count`）。

#### 阶段边界（自 plan.md 归位）

- **Y 系列 S0**：不碰 MCP、不改 `PROJECT_VERSION`、不动文档格式、不引入新 crate / 新依赖、不改 GUI 导出交互（stems / 24-bit / 采样率选择）。
- **Z / B 系列**：不改 `PROJECT_VERSION`、不改单口节点的逐样本行为、**不给 CLAP / VST3 插件效果器开双口**（宿主侧 port 协商是大工程，另立项）、不引入新依赖、不做任意多输入口（只 `0` / `1` 两个）、不做端口间自动降混。
- **X 系列**：不做编码器（导出 / 录音仍 hound 写 WAV）、Opus / ALAC、元数据展示、流式解码、采样率重采样（`rubato` 未引入）、`PROJECT_VERSION` bump。
- **I 系列**：不做 vk 节点的和弦 / 琶音器、键条力度分区（y 轴力度）、note repeat、timeline draw 到「音频片段内容编辑」；不改 `StrumOptions` 的模型默认值、不改 `PROJECT_VERSION`、不新增 crate、不动实时线程。
