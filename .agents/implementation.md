# 实现细节 — GlitcherZ

> 记录已落地模块的实现要点、协议细节与版本 gotchas。新增模块落地后在此追加条目。

## 已落地

### 时间轴入图（P3，2026-10-08）

- **MIDI 连线表**：`ExecPlan::midi_targets`（每 MIDI 轨扇出 ≤ `MAX_MIDI_FANOUT`=4）——音序器与键盘演奏的音符事件经 `RtGraph::track_note` 扇出到全部连线目标（`note_off_all` 同样遍历连线表）；默认图每轨连本轨声源（复刻旧语义）。编译器侧 `connect_midi/disconnect_midi`（重复连线幂等、空槽/越界拒绝、扇出超限报 `MidiFanOutOverflow`）。
- **自动化连线表（D7）**：`RtAutomation` 退化为**纯锚点曲线**（`enabled + points`，不再内嵌目标参数）；新增 `RtAutoTrack { loop_ticks = 384（一小节，可独立设置）, curve }` 独立实体（`MAX_AUTO_TRACKS`=16），与 MIDI 轨道音源循环解耦。`ExecPlan::auto_routes[16]` 终点 = `(槽位, 参数)`，控制侧以轨道坐标 `(track, param)` 经 `route_slot`（混音三项→通道条、延迟三项→延迟、其余→声源）解析；`RtGraph::auto_set_param` 查表投递，未连线静默。
- **命令面**：`SequencerCommand::SetAutomation` → `SetAutoTrack { index, auto }`；`AudioEngine` 新增 `set_auto_track`（命令队列）/ `set_auto_route` / `clear_auto_route`（编译器→结构队列，队列满快照回滚）。
- **UI 适配**：`send_automation` 同步下发曲线（`SetAutoTrack`）与连线终点（`set_auto_route`）；自动化轨道索引 = `track × MAX_AUTOMATIONS(4) + 文档 Vec 槽位`（Vec 只追加/原位替换，槽位稳定）；循环长度随泳道显示口径（片段长或一小节），P4 起由文档 v2 显式携带。
- **结构队列扩容 8 → 64**：项目加载一次性下发最多 16 条自动化连线 + 每轨声源恢复，原容量会静默丢连线。
- Sequencer 内核零改动（`advance` 回调签名不变）；现有音序/自动化端到端测试全部经连线路径通过。

### CLAP 宿主（`glitcherz-clap-host`）

- **参数枚举**：`ClapInstance::list_params()` → `raw::enumerate_params`——直接经 `clap_plugin.get_extension` 取 `clap_plugin_params` vtable 指针直调（clack 0.2 的 `PluginParams` 包装无读取方法，raw 模块为唯一 unsafe 边界）；返回 `ClapParamInfo{id, name, module, min/max/default}`。测试插件实现一个可自动化 Amplitude 参数（ID 1，0..1，默认 0.5，f64 位模式原子共享），参数事件驱动渲染振幅。
- **UI 接入**：`clap_voice_params`（轨道 → 参数快照）在装填 CLAP 声源时填入；参数面板 CollapsingHeader 内滑块直接调参（当前值存 `clap_voice_values`（UI 会话内，track → param id → f64），变更经 `send_track_param(ParamId::PluginParam)` 下发，块粒度下一 process 生效；重载插件/切回内置/文档重应用时重置为插件默认值；state 持久化落地后，重载路径经 `load_state` + `read_param_values` 重播种滑块缓存）；自动化目标选择器合并 CLAP（曲线值 = 插件值域原始值）与 VST3（曲线值 = 归一化 0..1）两份列表，并含外挂效果器参数（端点 = 效果器节点）。
- **真实插件兼容性复现**：`dexed_instrument_note_on_produces_audio`（clap-host 测试）——标准扫描目录发现 `dexed*.clap` 时实例化 + NoteOn + 16 块内断言非零输出，找不到则跳过；用于把"宿主桥接问题"与"插件状态/默认补丁问题"（宿主无 gui/state 扩展，Dexed 跑在内部默认补丁上）分开定位。

### 插件状态持久化（CLAP state extension，2026-10-15，spec 见 `.agents/spec/plugin-state.md`）

- **宿主侧**：workspace `clack-extensions` features 加 `"state"`（clack 0.2 的 `PluginState` 包装**有**完整 save/load 方法，与 params 不同、无需裸调 vtable）。`GlitcherZHostShared.state_ext: OnceLock<Option<PluginState>>` 在 `initializing()` 经 `InitializingPluginHandle::get_extension` 捕获，spawn 后 `access_shared_handler` 读出存进 `ClapInstanceGuard`；`GlitcherZHostMainThread` 实现 `HostStateImpl`（mark_dirty → no-op，采集时机见 D3）。
- **API**：`ClapInstanceGuard::{save_state() -> Vec<u8>, load_state(&[u8])}`（main thread，错误变体 `NoState/StateSave/StateLoad`）；`ClapInstance::read_param_values()` → `HashMap<u32, f64>`——raw vtable `get_value`（复用 enumerate 的扩展抓取模式），供状态加载后刷新滑块缓存。
- **数据落点（D1/D2）**：`VoiceSpec::Clap/Vst3` 各加 `state: Vec<u8>`（`#[serde(default, skip_serializing_if = "Vec::is_empty")]`）——旧文件无感，`PROJECT_VERSION` 不动。**注意**：VoiceSpec 是内部 tag 枚举（`type: "clap"` snake_case），RON legacy 形态测试别写成 `(Clap: ...)`。
- **数据流（D3/D4）**：保存 = `do_save` → `capture_plugin_states()`（engine_id 经 `node_map` 反查 doc_id，两段式：guard 循环采集 blobs → `project_mut()` 写文档；单实例失败 log + 跳过，D7）→ 序列化；恢复 = `install_node` spawn 成功后 `state` 非空即 `load_state` + `read_param_values` 回填 `clap_voice_values`。面板换声源（`load_clap_voice`/构造点）产 `state: Vec::new()`；切内置声源状态随 `VoiceSpec::Builtin` 替换自然丢弃。undo/redo 经 `apply_document → install_node` 走同一恢复路径（但状态 blob 本身不进 undo 图，D5——保存前 undo 回退到旧快照时 state 为旧值）。
- **测试**：clap-host `state_roundtrip_restores_amplitude`（设参 0.25 → save（8 字节）→ 新实例 load → get_value 读回 0.25，新实例先验证默认 0.5）；model `voice_spec_state_serde_compatibility`（空 state 不序列化 + legacy 无 state 字段可加载）。
- **VST3**（S5，2026-10-15）：`HostMemoryStream`（`IBStream` 内存流 COM 对象，控制线程专用；读/写/seek/tell 全实现）+ `Vst3InstanceGuard::{save_state, load_state, read_param_values}`——save = 组件 `getState` 写入内存流后整块取出；load = 组件 `setState` + 控制器 `setComponentState` 同步（**两个消费者各一条独立流**：ComRef 不走 SmartPtr 毯子实现，无 vtable 透传 seek，直接免掉 rewind）；save 时插件保持活跃（与 CLAP 镜像一致的简化宿主姿态，Dexed 实测兼容；规范最稳妥顺序是先于 setActive，留待生命周期重构收紧）；测试插件工厂改**逐实例创建**（原 OnceLock 单例与 kManyInstances 相悖且测不了"新实例回落默认值"），SineComponent 以 8 字节 LE f64 经 `getState/setState` 持久化 Gain（`IParameterChanges` 逐队列消费、末点胜出、作用于振幅），控制器镜像读回 = D6 滑块刷新载体（归一化滑块 UI 待接入，`read_param_values` 已预留）。

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
- **默认图（D18）**：每轨 `声源→延迟→mixer.ch[i]` + 单个全局 Mixer（通道数 = max(轨数, 2)）→ master；`allocate_track_chain` 缩为 voice+delay 两节点，轨电平点（`track_channel`）= 轨道链末端 delay（pre-mixer，mute 不影响其读数）。
- **旁路修订（D19）**：`remove_node` 旁路重连仅适用于 ≤1 入边节点（效果器照旧旁通，直连保留目标 `in_port`）；mixer/master 多入边无处重连——删除即弃线。
- **model（Q-B 决议）**：`NodeDoc::Mixer{node_id, track, inputs: u8(2..=32), channels: Vec<MixerChannel>, out_pan, out_muted}` 替代 ChannelStrip（旧变体保留仅作加载读路径，新文档不再产出）；加载期自动迁移——strip → 2 通道 mixer（原 gain/pan/mute 进通道 0）、master 入边 >1 时插入合成 mixer 承接。文档校验（load/save 双端）：非 mixer ≤1 音频入边、mixer `in_port` 唯一且 < `inputs`、通道数 2..=32。
- **UI**：mixer 节点画 N 个输入口（节点盒随通道数伸缩）+ 按口命中落线（精确口命中优先、否则自动落第一个空通道——任意空口可落，D21）+ 调音台风格面板（每通道 solo/mute/竖直推子/pan + 输出段 pan/mute + 通道增删按钮 2..=32，占用口不可删）；ChannelStrip 面板退役。
- **轨寻址（P5f ④）**：UI `track_channel_target(track)` 沿轨链连线走到首个 mixer 节点得 `(mixer_doc_id, in_port)`；轨推子/mute 读写对应 mixer 通道（文档与引擎参数路径同步），未接入 mixer 的轨回落默认值。`register_track_chain` 的 channel 槽位退化为电平点（传 delay）。
- **容量**：`MAX_IN_EDGES` 8→32（仅 mixer 可达）、`MAX_CABLES` 128→256（32 口 mixer 单节点即占 32 线）；32 通道求和 ≈8K FMA/块 ≈ 8µs，48kHz 下 <0.5% 单核。

### 引擎核心（`glitcherz-engine::realtime`）

- `AudioBuffer`、`Processor`（`set_param` + `note_event`）、`ProcessorChain`、`BlockRunner`（固定 128 块拆分 + 块边界命令消费 + `Feedback` 原子回传：主电平/位置/步进/走带/**每轨电平**）；
- **`Mixer`**：4 条 `TrackStrip`（`SynthProcessor → DelayProcessor`）并行处理 + 线性声像/增益/静音求和 → 主链（`MasterGain`）；`MixerLevels` 每块回传各轨 post-fader 峰值；
- 完整合成器链（每轨）：正弦振荡器 + 线性 ADSR → TPT SVF（低通/高通/带通）→ 反馈延迟（`DelayProcessor`：预分配环形缓冲 1 s 上限、feedback ≤ 0.9、反规范数冲洗、每声道独立写指针）；
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
- **缓存键位**：效果器参数缓存（`clap_effect_params` / `clap_effect_values` / `vst3_effect_params` / `vst3_effect_values`）以**文档 node_id** 为键（跨引擎重建稳定；下发时经 `node_map` 反查 engine id），乐器缓存沿用 track 键。填充路径两条：live 插入（`load_clap_effect` / `load_vst3_effect`，`insert_effect_common` 改回 `Option<doc_id>`）、文档重建（`install_node` 的 Effect 两分支）；清理路径三处：删除节点、`clear_external_effects`、`apply_document` 整批 clear。
- **域约定**：CLAP 滑块与自动化曲线 = 插件原始值域（min/max 来自参数枚举）；VST3 滑块与自动化曲线 = 归一化 0..1（`ParameterInfo` 不携带 plain 值域，plain↔normalized 换算需逐参调控制器，v1 明确不做——归一化滑块诚实且跨插件一致）。VST3 当前值经 S5 的 `read_param_values`（控制器 `getParamNormalized` 读回）播种会话缓存 `vst3_voice_values`（乐器）/ `vst3_effect_values`（效果器）。
- **自动化目标**：`track_external_fx(track)` 辅助统一产出效果器清单（doc_id + 显示标签 + 格式），轨道面板与自动化选择器共用；效果器参数作为自动化端点落在效果器节点上（`AutoParam::PluginParam` 域随格式）。

### 项目文件与模型（`glitcherz-model`）

- `Project`（metadata + gate + master_gain + 4×`Track`）；`Track` = 名称 + `Pattern` + `Clip`（`Note{start,length,note,velocity}` 列表 + loop_ticks，tick 基准：`TICKS_PER_QUARTER=96`/步=24/小节=384）+ 音源枚举（`VoiceSpec`: `Builtin | Clap | Vst3`）+ 混音（增益/声像/静音）+ `SynthParams` + `DelayParams`，全部 serde/RON + 字段级 `#[serde(default)]`；引擎侧镜像 `RtClip`（定容 128 音符 `Copy`，超容截断）经 `SetClip` 命令整块下发；
- `ProjectFile{version: u32, project}`（当前 `PROJECT_VERSION = 1`），`save_project`（RON pretty）/ `load_project` / `parse_project`；解析前用 `ron::Value` 探测顶层结构（含 `project`/`version` 键 → 包装格式，否则回退裸 `Project` 开发期格式——serde 会把裸格式的字段当未知字段"成功"忽略，必须先探测）；
- 程序化 `Default` 全部手写（派生 Default 会绕过 serde 字段默认值，曾导致 gate/tempo 为 0 的真 bug）。

### UI（`glitcherz-ui`，eframe 0.36 glow）

- 📂 打开 / 💾 保存（无路径时转另存为）/ 最近文件菜单（eframe `persistence` feature 存 RON，容量 8）；窗口标题随当前文件更新；状态栏消息（8 秒自动消失）；`Ctrl+S` / `Ctrl+O` 快捷键（优先于演奏键盘映射）；`--open <文件>` 启动加载；
- 走带 + BPM、**自动化泳道**（`lane` 自绘组件：参数三选 + 启用开关 + 锚点点按/拖拽/右键删除，播放头走线，截止频率对数轴）、**混音器面板**（轨道选择/静音/post-fader 电平条/竖直推子/声像）、16 步网格（播放位置黄色描边）、**钢琴卷帘编辑器**（`piano_roll` 自绘组件：点空白绘制、拖拽移动、右键删除、滚轮滚动音高、播放头走线；fit-to-width 时间轴、1/64 吸附）、画笔音符/音长、滤波器/ADSR/延迟面板（作用于选中轨道）、虚拟键盘（屏幕 + 电脑键盘）、主输出电平表；
- 音序器音乐位置经 `Feedback.position_ticks`（AtomicF32）回传驱动播放头。

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
- **M4b.2 切刀**：K 唤起（切换式，可连续切），点击在 `snap_bar` 处切开；两侧各 ≥1 bar 否则一帧状态提示拒绝。`split_clip_in` 纯函数：左半保留 start 缩短 span，右半克隆内容（loop 语义两半同 loop）插在 index+1。armed 期间指针归切刀（无拖动无框选）。
- **M4b.3 模态 G/T**：G = 时间 bar delta + 同类轨 row delta 的**刚性移动**（行 delta 全局钳制不逐 clip，组内相对关系不动）；X 切时间锁。T 默认拉终点、Shift+T 拉起点（bar 对齐、≥1 bar、不出编排边界）。ghost 经**行几何注册表**跨行绘制（目标可落同类任意行，行先注册 `lane` rect，ghost 帧行末统一画）；原位变暗 alpha 60（M4a.5 惯例）。落定校验 `filter_covered`：目标**完全覆盖**既有（非移动）clip 时该 clip 留原位+提示，批次其余照常落下（部分重叠允许）。`relocate_in`：每源轨降序 remove + 目的轨 append——**append 不位移任何剩余索引**，跨轨移动因此不需要索引重映射。
- **M4b.4 Shift+D**：克隆选区进入 grab 式模态（`ModalKind::Duplicate` 与 Grab 共享 `grab_targets` 数学与 X 锁），LMB 落定 `DuplicateClipsBatch`（内容克隆 append 到目标轨、选区转移到副本），RMB/Esc 副本不落盘。

测试抓到的真 bug：stretch 首版把 **bar 数当 tick 直接加**（`bar_delta` 未乘 `TICKS_PER_BAR`），位移量小 3840 倍——纯函数单测（指针 3 bar → 期望整 bar 对齐）当场抓住。教训照旧：几何数学全部收进纯函数并单测，UI 只做取值与绘制。

### 命令集（轨道定向）

- `SetParam`（主链）/ `SetTrackParam{track}` / `Note{track}` / `Sequencer(Play|Stop|SetBpm|SetGate|SetPattern{track}|SetClip|SetSource|SetAutomation)`；模式整块 `Copy` 数组经队列传递。

## 版本注意（gotchas）

- cpal 0.18；
- egui 0.36：`App::ui(&mut Ui)`、`egui::Panel`、`Response` 无 `pressed/released`（用 `is_pointer_button_down_on`）、`DragValue::range(RangeInclusive)`、`Painter::rect_stroke(rect, radius, stroke, StrokeKind)`；
- clack 0.2：`clack-extensions` 宿主侧 trait 需 `clack-host` feature；`Pckn::new` 需显式 u16/u32 字面量；`start_processing` 返回 `&mut Started`；`PluginInstance::new` 收 `&CStr`；
- RON/serde：定长数组（`[Track; 4]`/`[Step; 16]`）在 RON 里是**圆括号**（serde tuple 路径），方括号只用于 `Vec`；
- VST3 参数为归一化 0..1（CLAP 为原始值），跨格式展示需换算。

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
