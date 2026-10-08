# 实现细节 — ResoZ

> 记录已落地模块的实现要点、协议细节与版本 gotchas。新增模块落地后在此追加条目。

## 已落地

### 时间轴入图（P3，2026-10-08）

- **MIDI 连线表**：`ExecPlan::midi_targets`（每 MIDI 轨扇出 ≤ `MAX_MIDI_FANOUT`=4）——音序器与键盘演奏的音符事件经 `RtGraph::track_note` 扇出到全部连线目标（`note_off_all` 同样遍历连线表）；默认图每轨连本轨声源（复刻旧语义）。编译器侧 `connect_midi/disconnect_midi`（重复连线幂等、空槽/越界拒绝、扇出超限报 `MidiFanOutOverflow`）。
- **自动化连线表（D7）**：`RtAutomation` 退化为**纯锚点曲线**（`enabled + points`，不再内嵌目标参数）；新增 `RtAutoTrack { loop_ticks = 384（一小节，可独立设置）, curve }` 独立实体（`MAX_AUTO_TRACKS`=16），与 MIDI 轨道音源循环解耦。`ExecPlan::auto_routes[16]` 终点 = `(槽位, 参数)`，控制侧以轨道坐标 `(track, param)` 经 `route_slot`（混音三项→通道条、延迟三项→延迟、其余→声源）解析；`RtGraph::auto_set_param` 查表投递，未连线静默。
- **命令面**：`SequencerCommand::SetAutomation` → `SetAutoTrack { index, auto }`；`AudioEngine` 新增 `set_auto_track`（命令队列）/ `set_auto_route` / `clear_auto_route`（编译器→结构队列，队列满快照回滚）。
- **UI 适配**：`send_automation` 同步下发曲线（`SetAutoTrack`）与连线终点（`set_auto_route`）；自动化轨道索引 = `track × MAX_AUTOMATIONS(4) + 文档 Vec 槽位`（Vec 只追加/原位替换，槽位稳定）；循环长度随泳道显示口径（片段长或一小节），P4 起由文档 v2 显式携带。
- **结构队列扩容 8 → 64**：项目加载一次性下发最多 16 条自动化连线 + 每轨声源恢复，原容量会静默丢连线。
- Sequencer 内核零改动（`advance` 回调签名不变）；现有音序/自动化端到端测试全部经连线路径通过。

### CLAP 宿主（`resoz-clap-host`）

- **参数枚举**：`ClapInstance::list_params()` → `raw::enumerate_params`——直接经 `clap_plugin.get_extension` 取 `clap_plugin_params` vtable 指针直调（clack 0.2 的 `PluginParams` 包装无读取方法，raw 模块为唯一 unsafe 边界）；返回 `ClapParamInfo{id, name, module, min/max/default}`。测试插件实现一个可自动化 Amplitude 参数（ID 1，0..1，默认 0.5，f64 位模式原子共享），参数事件驱动渲染振幅。
- **UI 接入**：`clap_voice_params`（轨道 → 参数快照）在装填 CLAP 声源时填入；参数面板 CollapsingHeader 展示；自动化目标选择器合并 CLAP（曲线值 = 插件值域原始值）与 VST3（曲线值 = 归一化 0..1）两份列表。

### 节点图（`realtime::graph`，P1+P2，spec 见 `docs/spec/node-graph.md`）

- **项目文件 v2（P4）**：`PROJECT_VERSION = 2`，无 v1 读路径（serde 字段级默认值容忍缺失）。新增 `Project.graph: GraphDoc`（过渡形态，spec §7 完整 nodes+cables 随 P5 落地）——`midi_links`（MIDI 跨轨扇出，上限 16；加载期校验越界/自连/超容报 `ModelError::InvalidGraph`）；`Automation::loop_ticks` 显式携带（默认 384），泳道显示口径同步回写。引擎侧配套 `AudioEngine::connect_midi_track/disconnect_midi_track`（快照回滚），UI `push_all_params` 加载时重放连线。
- **`RtGraph`**：定容 64 槽位，每节点一个专属立体声 128 帧输出缓冲；处理按预编译拓扑序遍历——清零 → 多入边自动求和（≤8）→ 原地 `Processor::process`；主输出槽缓冲即 cpal 写出源。
- **`ExecPlan`**（全 `Copy`）：Kahn 拓扑排序 + 入边表 + 每轨 voice/delay/channel 路由表（P3 由时间轴连线表替代）；编译期拒绝环（D10）与入边超限。
- **结构更新协议（D5）**：控制侧 `GraphCompiler`（全 `Copy`，队列满可快照回滚）编译计划并产出 `GraphUpdate::SetTopology { plan, deployments, removed }`；音频线程块边界整体换计划 + 指针级装卸，零分配；被移除节点连同输出缓冲经 `RecycledNode`（已扩展携带 `output: AudioBuffer`）回执队列送回控制线程释放。
- **`ChannelStripProcessor`**：增益/声像（线性律）/静音升级为真节点（原 `TrackStrip` 求和段逻辑）；默认拓扑复刻旧 Mixer（`声源 → 延迟 → 通道条 → 主输出`），旧 mixer 全部数学测试以默认图形式通过。
- **行为变化**：`set_builtin_voice` 现在回收外部插件节点（旧实现驻留），切回插件需重新实例化（状态不保留）；UI 守卫经回收 `node_id` 正常配对释放。
- `mixer.rs` 已删除；`MixerLevels` → `GraphLevels`；`VoiceUpdate` → `GraphUpdate`。

### 引擎核心（`resoz-engine::realtime`）

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

- `resoz-model` 的 `Automation`（`AutoParam` = 音量/声像/截止 + `AutoPoint{tick,value}` 有序列表，线性插值、两端保持端值）挂 `Track.automations`；
- 引擎侧 `RtAutomation`（定容 64 锚点 Copy）经 `SetAutomation` 命令 upsert（同参替换 / 空槽插入 / 空曲线=删除，每轨 ≤4 条）；
- 求值器在 `BlockRunner` 每块**仅播放中**按"循环后的音乐位置"（步进源 = 一小节、卷帘源 = 片段 loop_ticks 取模）采样并经混音器内部路径直写参数（不走命令队列）；UI 在走带停止沿重推手动参数实现"停止后回到手动值"；
- 插件参数桥接端到端（引擎集成测试锁定）：自动化曲线 → `AutoParam::PluginParam{param_id}` → `ParamId::PluginParam` → 求值器 → `TrackStrip`（`PluginParam` 只路由外部声源）→ `ClapNode` → `ParamValueEvent` → 插件生效。

### CLAP 宿主（`resoz-clap-host`，路线图第 9 步）

- `resoz-test-plugin`（cdylib）：clack-plugin 单音正弦乐器 fixture（`org.resoz.sine`，instrument+stereo 特性），供宿主端到端测试；
- 宿主 crate（唯一允许 unsafe 的边界 = `raw::load_entry`，模块级 SAFETY 文档 + 无效文件测试）：`clap_search_paths` / `scan_clap_files`（标准目录 + `RESOZ_CLAP_PATH`）/ `list_clap_plugins`（描述枚举 + instrument 特性识别）；`ClapInstance::spawn_rt(path, id, config)` 控制线程加载 → 实例化 → 激活，产出守卫 `ClapInstanceGuard`（持有动态库 + 实例，必须与节点同寿命）+ `ClapNode`；
- `ClapNode` 实现 `resoz-engine::realtime::Processor`：音符/参数经定容事件槽（`MAX_PENDING_EVENTS=32`）暂存，每块以 `NoteOn/OffEvent` / `ParamValueEvent` 发出（块粒度）；`ParamId::PluginParam(u32)` 已加入参数空间（自动化桥接的落点）；音频经构造期预分配的 `AudioPorts` + 立体声临时缓冲渲染回 `AudioBuffer`；首块自动 `start_processing`，任何失败输出静音（无 panic）；处理用统一 `PluginAudioProcessor`（`as_started_mut`/`start_processing` 运行时切换）；
- 分层决定：**engine 保持零 CLAP 依赖**——clap-host 依赖 engine（实现其 Processor trait）。

### VST3 宿主（`resoz-vst3-host`）

- 架构镜像 `resoz-clap-host`：扫描（单文件 / bundle 目录，标准目录 + `RESOZ_VST3_PATH`）→ 非实时线程实例化（IComponent + IEditController + IConnectionPoint 互连 + 总线激活）→ `Vst3Node`；
- **参数语义差异**：VST3 参数以**归一化 0..1** 经 `IParameterChanges` 传递（CLAP 是原始值）——自动化求值器天然桥接，但 UI 层展示插件参数时须经控制器做归一化换算；
- 乐器渲染到节点内预分配立体声缓冲；效果器自动接通输入通路（引擎缓冲 → 独立输入缓冲 → 插件 → 独立输出缓冲 → 拷回引擎缓冲），kSample32 单精度；
- 生命周期：`Vst3InstanceGuard` 持有动态库与全部 COM 对象，**必须与 `Vst3Node` 同寿命且晚于其释放**；Node 释放时 `setProcessing(false)`；Guard 释放时 `setActive(false)` + `terminate()` + 断开连接，最后才卸载动态库；
- 已集成：`VoiceSpec::Vst3{path, class_id}` 进 model 与项目序列化；UI 守卫注册表 + 乐器/效果器懒扫描缓存 + 每轨参数列表。

### 项目文件与模型（`resoz-model`）

- `Project`（metadata + gate + master_gain + 4×`Track`）；`Track` = 名称 + `Pattern` + `Clip`（`Note{start,length,note,velocity}` 列表 + loop_ticks，tick 基准：`TICKS_PER_QUARTER=96`/步=24/小节=384）+ 音源枚举（`VoiceSpec`: `Builtin | Clap | Vst3`）+ 混音（增益/声像/静音）+ `SynthParams` + `DelayParams`，全部 serde/RON + 字段级 `#[serde(default)]`；引擎侧镜像 `RtClip`（定容 128 音符 `Copy`，超容截断）经 `SetClip` 命令整块下发；
- `ProjectFile{version: u32, project}`（当前 `PROJECT_VERSION = 1`），`save_project`（RON pretty）/ `load_project` / `parse_project`；解析前用 `ron::Value` 探测顶层结构（含 `project`/`version` 键 → 包装格式，否则回退裸 `Project` 开发期格式——serde 会把裸格式的字段当未知字段"成功"忽略，必须先探测）；
- 程序化 `Default` 全部手写（派生 Default 会绕过 serde 字段默认值，曾导致 gate/tempo 为 0 的真 bug）。

### UI（`resoz-ui`，eframe 0.36 glow）

- 📂 打开 / 💾 保存（无路径时转另存为）/ 最近文件菜单（eframe `persistence` feature 存 RON，容量 8）；窗口标题随当前文件更新；状态栏消息（8 秒自动消失）；`Ctrl+S` / `Ctrl+O` 快捷键（优先于演奏键盘映射）；`--open <文件>` 启动加载；
- 走带 + BPM、**自动化泳道**（`lane` 自绘组件：参数三选 + 启用开关 + 锚点点按/拖拽/右键删除，播放头走线，截止频率对数轴）、**混音器面板**（轨道选择/静音/post-fader 电平条/竖直推子/声像）、16 步网格（播放位置黄色描边）、**钢琴卷帘编辑器**（`piano_roll` 自绘组件：点空白绘制、拖拽移动、右键删除、滚轮滚动音高、播放头走线；fit-to-width 时间轴、1/64 吸附）、画笔音符/音长、滤波器/ADSR/延迟面板（作用于选中轨道）、虚拟键盘（屏幕 + 电脑键盘）、主输出电平表；
- 音序器音乐位置经 `Feedback.position_ticks`（AtomicF32）回传驱动播放头。

### 命令集（轨道定向）

- `SetParam`（主链）/ `SetTrackParam{track}` / `Note{track}` / `Sequencer(Play|Stop|SetBpm|SetGate|SetPattern{track}|SetClip|SetSource|SetAutomation)`；模式整块 `Copy` 数组经队列传递。

## 版本注意（gotchas）

- cpal 0.18；
- egui 0.36：`App::ui(&mut Ui)`、`egui::Panel`、`Response` 无 `pressed/released`（用 `is_pointer_button_down_on`）、`DragValue::range(RangeInclusive)`、`Painter::rect_stroke(rect, radius, stroke, StrokeKind)`；
- clack 0.2：`clack-extensions` 宿主侧 trait 需 `clack-host` feature；`Pckn::new` 需显式 u16/u32 字面量；`start_processing` 返回 `&mut Started`；`PluginInstance::new` 收 `&CStr`；
- RON/serde：定长数组（`[Track; 4]`/`[Step; 16]`）在 RON 里是**圆括号**（serde tuple 路径），方括号只用于 `Vec`；
- VST3 参数为归一化 0..1（CLAP 为原始值），跨格式展示需换算。
