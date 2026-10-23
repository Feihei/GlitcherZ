# 实现细节 — GlitcherZ

> 记录已落地模块的实现要点、协议细节与版本 gotchas。新增模块落地后在此追加条目。

## 已落地

### SF2 音色库加载器 S1–S6（2026-10-19 收官，spec 见 `.agents/spec/sf2-loader.md`）

- **S1 RT 审计**（结论细节在 spec §7）：`rustysynth-ext = "=1.4.0"` 热路径零堆分配实证（计数 `#[global_allocator]`，测试 `tests/sf2_rt_audit.rs`），`VoiceCollection` 即预分配池，voice 池 fork 预案（D4）解除；唯一 panic 点 = `render` 等长契约（调用侧保证）。**审计 harness 的坑**：断言 label 的 `format!` 在计数窗口内求值会被计入分配（首跑真实库的假阳性），label 构建须在 scope 之外；计数器全局，测试须 `Mutex` 串行。
- **共享测试 fixture**：`tests/common/mod.rs` 手建最小合法 SF2 字节流（单 preset 正弦 + loop + exclusive class，bank/program 可参数化 `minimal_sf2_bytes_at(bank, program)`）——纯 `cargo test` 无外部文件可跑全部路径；真实库测试统一走 `#[ignore]` + `GLITCHERZ_SF2_PATH` 环境变量（本地数据不入库，D9）。**手写 RIFF 的坑**：ibag terminator 的 generator index 必须指向 igen 全部记录之后（差一条 → zone 丢 `sampleID` → 被当 global zone → 0 region → 静音且不报错）。
- **S2 非实时模块 `engine::sf2`**：`Sf2Library::load(path)`（控制线程：BufReader + `SoundFont::new`，产物 `Arc<SoundFont>` 交 RT 侧不再解析）+ 加载期一次性 `presets(): &[Sf2Preset{bank: u8, program: u8, name}]` 枚举（D7 轨级 u8:u8 寻址；**bank/program 超出 u8 的 preset 跳过 + log::warn**，全部不可寻址 → `Sf2Error::NoPresets`）+ `find_preset(bank, program)`（UI 下拉与 S4 VoiceSpec 寻址共用）；`Sf2Error{Io, Parse, NoPresets}` thiserror。rustysynth-ext 自 S2 起为 engine 正式依赖。测试 `tests/sf2_load.rs` 8 场景（含截断/垃圾字节/越界 bank），真实库（MuseScore_General/FatBoy）load + 枚举 + **preset 地址唯一性**验证通过。
- **S3 实时渲染 `realtime/sf2.rs` `Sf2VoiceProcessor`**：实现 `Processor`，spec §7 接入要点全部落实——① `SynthesizerSettings.block_size = 128`（对齐 `FIXED_BLOCK_SIZE`）；② stereo render 缓冲构造期预分配，`process` 内经 `render(&mut l, &mut r)` 等长切片填入（排除库唯一 panic 点）后 `copy_channel` 进图缓冲（目标帧数小于 render 帧数时截断，mono 缓冲只写 ch0）；③ 轨级 bank:program 经 `process_midi_message(0xB0/0xC0)` 下发（零分配，S1 已实证），`set_master_volume` 纯写入挂 `ParamId`。**参数面**：`ParamId::{Sf2BankSelect, Sf2ProgramChange, Sf2MasterVolume}`（bank/program 为字面整数参数：`f32.round() as i32`，NaN 饱和转换为 0；负值不钳制——库侧 `set_bank/set_patch` 纯字段写、仅作 preset_lookup HashMap 键，未命中走 GM/默认 preset 回落，已核实安全；master volume 非有限值忽略）；`Sf2Error::Synth` 变体承接 `SynthesizerSettings` 越界（采样率 16000..=192000 / block 8..=1024 / 复音 8..=256）。**复音** `SF2_POLYPHONY = 64`。**毯式 note-off 语义**：引擎 transport stop 发 `Off { note: 0 }`，pitch-matched 接收端会忽略 → 挂音；以 `held: u128` 位图跟踪按下键，`Off { note: 0 }` 且 key 0 未按住 = `note_off_all()` 全释放，key 0 按住 = 正常 pitch-matched 释放（`held_note_zero_release_stays_pitch_matched` 测试锁定）。**边界**：velocity f32→MIDI 七位（NaN/Inf 钳制）；越界 note（≥128）照常转发 synth（内部静默忽略）但 `1u128 << note` 前有 `<128` 守卫（否则 panic）。测试 `tests/sf2_voice.rs` 13 场景 + 真实库 smoke（`#[ignore]` + `GLITCHERZ_SF2_PATH`）。**测试坑**：GM 默认 CC#91 reverb send=40，被拒绝的音符仍带 ~0.003 尾音残余——绝对阈值断言（`< 0.001`）会假失败，须先渲染 baseline 再相对比较；fixture `two_preset_sf2_bytes` 的 preset keyRange 以 pgen generator `lo | hi << 8` 编码（`20..=40`）。
- **S4 model `VoiceSpec::SoundFont`**（spec D5/D6/D8）：`SoundFont { path: String, bank: u8, program: u8 }` 内部 tag 枚举新变体（tag = `"sound_font"`，serde default 加法字段，PROJECT_VERSION 不动；bank/program 缺失加载为 0:0）。测试：往返覆盖进 `voice_spec_ron_roundtrip` + 新 `voice_spec_soundfont_serde_compatibility`（tag 形态 + partial 字段默认）。**两个 serde gotcha**：RON 紧凑输出无空格（断言 `contains("type:\"sound_font\"")`，带空格版本假失败）；`#[serde(default)]` 的默认值是类型默认（`u8::default()` = 0）而非实例值。**UI 侧连带补齐 8 处非穷尽 match**（编译必需）：`install_node` Instrument+SoundFont = 完整文档恢复路径——非实时线程 `Sf2Library::load` → `Sf2VoiceProcessor::new` → 构造期 `set_param(Sf2BankSelect/Sf2ProgramChange)` 下发（D7 地址在装入前重放），文件丢失/解析失败回落 `SynthProcessor` + `log::error`（D8，文档字段保留）；Effect+SoundFont 回落 `DelayProcessor`（防御性，UI 不产生该组合）；`voice_plugin_key`/`track_external_fx` → `None`（S5 前无 preset 库键/外挂效果器面板）；preset load/reset 的 `&mut` match → `{}`（SF2 无 state blob）。patch 图标签 `SF2 {bank}:{program} · Track N`。
- **S5 UI 音色库面板**：Synth 面板 Instrument 区加 `SoundFont…` 按钮（`rfd::FileDialog` filter `sf2`/`sf3`，与项目保存同一同步弹窗先例）→ 调用侧 begin_edit + 文档改 `VoiceSpec::SoundFont{path, bank: 0, program: 0}` + `load_soundfont_voice` + commit_edit（与 CLAP/VST3 换声源同一 undo 分工：改文档在调用侧，函数只做 engine 替换）。**会话单库**（spec §6 v1 不做多库）：App 字段 `sf2_library: Option<Arc<Sf2Library>>`；`obtain_sf2_voice(path, sr)` = path 匹配复用缓存 / 未命中 `Sf2Library::load` 并替换缓存，`install_node` 与面板共用（同一工程重开秒装，避免每次 install 重读几十 MB 文件）。`load_soundfont_voice`：retire_guard（外部插件切 SF2）→ obtain → `set_param(Sf2BankSelect/Sf2ProgramChange)` 构造期下发 → `replace_node` → 清其他格式面板缓存。**preset 下拉**：当前轨声源为 SoundFont 时显示 ComboBox（`sf2_preset_{sel}`），preset 列表从共享库枚举后**快照 clone 出来**（`Vec<(u8,u8,String)>`，释放 `self.sf2_library` 借用——否则 ComboBox 回调里 undoable 编辑的 `&mut self` 借用冲突）；`set_soundfont_preset` = begin_edit + 文档改 bank/program + `engine.set_node_param(engine_id, Sf2BankSelect/Sf2ProgramChange)`（节点寻址即时下发，FIFO 顺序保证 bank 先于 program）+ commit_edit("Select SF2 Preset")；undo/redo 经 apply_document → install_node 重放。库未加载（跨会话或路径不符）显示 `(soundfont file not loaded this session — reopen it)`；`Built-in Synth` 高亮条件改为 `!is_external && !is_sf2`（源选择行本身已于 U9 E5 删除）。真实库端到端验证：MuseScore_General smoke（256 preset 枚举 + preset 切换 + 有声输出）通过。
- **S6 SF3 feature 常开 + 端到端验收**：workspace `rustysynth-ext = { version = "=1.4.0", features = ["sf3"] }`——lewton 0.10（Vorbis 解码），新增传递依赖 lewton/ogg/byteorder/tinyvec 全纯 Rust、无 C 构建链（跨平台 CI 前提保持）。SF3 解码只发生在 `SoundFont::new`（加载期，控制线程 D3），RT 热路径代码零改动。**S1 审计复跑（sf3 开启后）**：fixture 9 场景 + 真实库 MuseScore_General（256 preset 逐个 bank/program 切换 + 和弦 + render 逐计数）全部 0 分配；真实库端到端验收 `-- --ignored` 全绿（sf2_load / sf2_rt_audit / sf2_voice 三套）。UI 侧 S5 的文件对话框 filter 已含 `sf3`，无需改动。本机无 .sf3 样本文件，SF3 解码路径的实证验证留待拿到真实 SF3 库后补跑（依赖面与解码线程已静态核实）。
- **S7 preset 合并语义调查（2026-10-20，结论：保持相加语义，"vel 100 偏轻"非引擎 bug）**：起因 = 钢琴类 patch 虚拟键盘（vel≈0.9）有声、piano roll（默认 vel 100/0.7874）偏轻，疑 preset zone 合并语义错误。**实证过程**：① vendored `rustysynth-ext` 到 `.vendor/`（根 `Cargo.toml` path 依赖 + `[workspace] exclude`，保留 sf3 feature），新增 preset 层 `explicit: u64` 位掩码 + SF2 2.04 §8.2.3 的 14 个非法 preset generator 过滤（`PRESET_INVALID_GENERATORS`，当前测试字体无此类项零行为差异）+ 公开诊断 API `PresetRegion::get_generator()`；② 实现 SF2 §8.5.2 覆盖语义（preset 显式 generator 覆盖 inst 值，仅 initialAttenuation 相加）后 vel 1.0 峰值从 0.0912 **暴跌 150 倍至 0.0006**——假设证伪，回退相加语义。**根因**：MuseScore_General（机器生成字体）的 preset 值是**覆盖意图与相加意图的混合体**——覆盖意图如 `attackVolEnv=-7973`（10ms 整值）/`5573`（25s 哑音层技巧），相加意图如 FF 层 `initialFilterFc=-343`（微调偏移，覆盖语义下 cutoff 变 6.7Hz 近静音）。纯覆盖或纯相加都无法两全，但相加优雅降级、覆盖造成灾难，故 MeltySynth/RustySynth 家族的相加行为是正确选择。**vel 100 偏轻的真实构成**（note 60 匹配多 zone 叠加）：`key 0..127 vel 75..107` zone（attn 14dB + `modEnvToFilterFc=-2350` 暗化 → 用 MF 采样）+ `key 60..127 vel 64..107` zone（attn 17dB + attack 25s 哑音层）+ rustysynth velocity→gain 曲线（`2·20·log10(vel/127)` Polyphone 风格，vel 100 → -4.2dB）= 字体设计使然，非 bug。**外部参照全部不可用**：webfetch 损坏；scoop 装 FluidSynth / cargo fetch oxisynth 的 GitHub 下载均被 sandbox 拦截。测试 `real_soundfont_velocity_sweep` 的 dump 已扩展为打印每 preset zone 的 key/vel range + 显式 generator 清单（`generator_name()` 辅助）+ inst 层滤波/包络参数，供后续人耳对照。vendored crate 自身 3 个失败单测 = 缺上游 testdata fixture（registry 包不含），与本地修改无关。

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
- **数据流（D3/D4）**：保存 = `do_save` → `capture_plugin_states()`（engine_id 经 `node_map` 反查 doc_id，两段式：guard 循环采集 blobs → `project_mut()` 写文档；单实例失败 log + 跳过，D7）→ 序列化；恢复 = `install_node` spawn 成功后 `state` 非空即 `load_state` + `read_param_values` 回填 `clap_voice_values`。面板换声源（`load_clap_voice`/构造点）产 `state: Vec::new()`；切内置声源状态随 `VoiceSpec::Builtin` 替换自然丢弃（换声源动作 E5 后不存在，构造点统一走 `create_patch_node_on`）。undo/redo 经 `apply_document → install_node` 走同一恢复路径（但状态 blob 本身不进 undo 图，D5——保存前 undo 回退到旧快照时 state 为旧值）。
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

G1（gui 扩展宿主）/ G2（Win32 嵌入容器）/ G3（参数回接）已落地，Dexed 实测通过；决策 D1–D10 与阶段明细见 `spec/plugin-gui.md` 与 plan.md。跨版本 gotchas：

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
- **默认工程**：`default_graph_doc` 末尾追加一个 `Computer` 节点（id = 现有最大 id + 1）并 `keyboard_midi(vk, track0 voice)`——新工程开箱能弹，但那条线是可见可删的真实连线。默认图节点数/连线数断言已相应 +1/+1。
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

### 项目文件与模型（`glitcherz-model`）

- `Project`（metadata + gate + master_gain + 4×`Track`）；`Track` = 名称 + `Pattern` + `Clip`（`Note{start,length,note,velocity}` 列表 + loop_ticks，tick 基准：`TICKS_PER_QUARTER=96`/步=24/小节=384）+ 音源枚举（`VoiceSpec`: `Builtin | Clap | Vst3`）+ 混音（增益/声像/静音）+ `SynthParams` + `DelayParams`，全部 serde/RON + 字段级 `#[serde(default)]`；引擎侧镜像 `RtClip`（定容 128 音符 `Copy`，超容截断）经 `SetClip` 命令整块下发；
- `ProjectFile{version: u32, project}`（当前 `PROJECT_VERSION = 1`），`save_project`（RON pretty）/ `load_project` / `parse_project`；解析前用 `ron::Value` 探测顶层结构（含 `project`/`version` 键 → 包装格式，否则回退裸 `Project` 开发期格式——serde 会把裸格式的字段当未知字段"成功"忽略，必须先探测）；
- 程序化 `Default` 全部手写（派生 Default 会绕过 serde 字段默认值，曾导致 gate/tempo 为 0 的真 bug）。

### UI（`glitcherz-ui`，eframe 0.36 glow）

- **File 菜单**（U2 收口）：New / Open… / Open Recent ▸ / Save / Save As… / Import MIDI… / Export MIDI…，快捷键 `Ctrl+N` / `Ctrl+O` / `Ctrl+S` / `Ctrl+Shift+S`（优先于演奏键盘映射）；最近文件经 eframe `persistence` 存 RON，容量 8；窗口标题每帧按「文件名 + 未保存 `*`」重算；状态栏消息（8 秒自动消失）；`--open <文件>` 启动加载；
- 走带 + BPM、**自动化泳道**（`lane` 自绘组件：参数三选 + 启用开关 + 锚点点按/拖拽/右键删除，播放头走线，截止频率对数轴）、**混音器面板**（轨道选择/静音/post-fader 电平条/竖直推子/声像）、16 步网格（播放位置黄色描边）、**钢琴卷帘编辑器**（`piano_roll` 自绘组件：点空白绘制、拖拽移动、右键删除、滚轮滚动音高、播放头走线；fit-to-width 时间轴、1/64 吸附）、画笔音符/音长、滤波器/ADSR/延迟面板（作用于选中轨道）、虚拟键盘（屏幕 + 电脑键盘）、主输出电平表；
- 音序器音乐位置经 `Feedback.position_ticks`（AtomicF32）回传驱动播放头。

#### 文件管理（U2，2026-10-22）

- **`project_files` 模块**：只放纯函数与文件 IO（标题格式化、导出默认名、空轨判定、导入落位计划、SMF 读写包装），app 壳负责对话框 / undo / 引擎推送，因此不必起 egui 上下文即可测试。
- **导入落位**（`plan_import`）：先复用**空轨**（无音符且无 CC 任意 clip）再追加新轨，容量按 `MAX_MIDI_TRACKS`，超出部分丢弃并报数。`Project::default()` 有 4 条空轨，所以「新建 → 导入」不会留下占位空轨。
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
- **已知取舍**：`Project::default()` 仍是 0 条控制轨，所以新工程要按 Synth 面板的 `+ Add` 才有一条可拖的 A1 出口。默认轨的取舍留给 U4（控制轨一等化进 timeline 时一并决定），U3 不动默认工程。

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

### 命令集（轨道定向）

- `SetParam`（主链）/ `SetTrackParam{track}` / `Note{track}` / `Sequencer(Play|Stop|SetBpm|SetGate|SetPattern{track}|SetClip|SetSource|SetAutomation)`；模式整块 `Copy` 数组经队列传递。

## 版本注意（gotchas）

- cpal 0.18；
- egui 0.36：`App::ui(&mut Ui)`、`egui::Panel`、`Response` 无 `pressed/released`（用 `is_pointer_button_down_on`）、`DragValue::range(RangeInclusive)`、`Painter::rect_stroke(rect, radius, stroke, StrokeKind)`；
- clack 0.2：`clack-extensions` 宿主侧 trait 需 `clack-host` feature；`Pckn::new` 需显式 u16/u32 字面量；`start_processing` 返回 `&mut Started`；`PluginInstance::new` 收 `&CStr`；`Pckn` 类型导出路径是 `clack_common::events::Pckn`（`events::pckn` 模块私有）；`ParamValueEvent::new(time, param_id, pckn, value)`；`EventBuffer` 经 `as_input()/as_output()` 包装后 `as_raw()/as_raw_mut()` 取 raw 指针（G3 flush 直调用到）；
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
#### clip 单周期不变量：resize 拖长自动平铺 bug（2026-10-20）

症状：clip 第一小节输入音符后拖长尾部，播放时音符在新增小节**自动重复**（pattern 平铺效果），预期应是空白小节。

根因是 **UI 打破了 D14 的单周期前提**：引擎调度器按 `loop_ticks` 周期在 `[start, start+span)` 内平铺音符（spec 预埋的未来特性，`sequencer.rs` 的 `m·loop` 循环），而 UI resize/stretch 只写 `span_ticks` 不动 `loop_ticks`（恒为默认 1 bar）——`span > loop` 后平铺自动生效。同一裂口还让 split 产生 `span < loop` 的**非法文档**（模型校验 `span ≥ loop`，保存即报错）。

修复 = UI 层所有改 `span_ticks` 的路径维持 `loop_ticks == span_ticks` 不变量（引擎平铺语义原样保留，`span == loop` 时天然无重复）：

- `timeline_move_resize_clip` / `timeline_stretch_clips_batch`（`lib.rs`）：写 span 同时写 loop；
- `split_clip_in`（`timeline.rs`）：两半各自 `loop = span`；`Placement` trait 新增 `set_loop` + `retain_from(offset)`——右半内容取切口之后部分并平移到新原点（**跨界音符两半延续**：右半以剩余尾巴从 0 开始；`sequence.ctrls` 与 `curve` 锚点按 `tick ≥ offset` 平移过滤），左半内容**原样保留**（超出新 span 的音符/锚点只是隐藏，拖回长度即恢复——数据永不丢）；
- piano roll / lane 绘制过滤 `start ≥ loop_ticks` 的遗留内容（防溢出画布右侧；交互早已按位置匹配 + `min(loop_ticks - 1)` clamp，超界项点不到）。

新增小节从此**空白可输入**（piano roll 宽度 = `loop_ticks` = span，拖长即扩宽编辑区）。测试：`split_halves_keep_start_end_and_content` 重写（含跨界音符断言）、新增 `split_shifts_control_curve_anchors`。引擎平铺测试 `clip_tiles_within_span_and_stops_at_span_end` 不变（未来特性仍被锁定）。
#### roll / timeline 视图导航与轨道重排（2026-10-21）

**piano roll**：`RollState` 新增 `zoom_x`（0.2–64×，对 fit-to-width 的乘子）/ `zoom_y`（0.4–6×，对 `ROW_HEIGHT` 的乘子）/ `scroll_x`（视口左缘的 tick）。几何全面参数化：`ticks_to_x = width/loop_ticks * zoom_x`、`row_h = ROW_HEIGHT * zoom_y`、`visible_rows = ROLL_HEIGHT/row_h`、**`t0_x = rect.left() - scroll_x * ticks_to_x` 是横向唯一几何源**（网格/音符/ghost/播放头/velocity lane/knife/命中全部由它派生，绘制与命中不漂移）；`note_rect/note_edges/note_hit_rect/note_at/knife_crossing` 签名改为显式携带 `t0_x/row_h`。交互：Ctrl+滚轮 = 时间轴缩放（`zoom_scroll_anchor` 纯函数锚定指针下 tick，clamp 0..loop-width）；Ctrl+Alt+滚轮 = 行高缩放（egui `zoom_delta_2d()` 的 vertical-only 分支 `(zx-zy).abs()>1e-4` 判定，纯 Ctrl 是 uniform 只缩时间轴）；Shift+滚轮 = 水平滚动（egui `horizontal_scroll_modifier` 把 delta 折进 `wheel.x`）；纯滚轮 = 纵向滚动（`top_note` clamp 下界随行高变 `rows-1`）；中键拖动 = 双轴平移（`dragged_by(Middle)` + `pointer.delta()`）。

**timeline**：`TimelineState` 新增 `row_zoom`（0.5–3×）+ `rows_rect`/`rows_viewport_min`（**上一帧**行区域屏幕矩形与视口原点）。关键时序：导航输入在 `ScrollArea::show` **之前**处理——zoom 与 offset 补偿同帧生效；若放在闭包内改 state 会被 ScrollArea end 的 store 覆盖。水平缩放锚定公式：`tick = (offset.x + ptr.x - vmin.x) / old_ppt`，新 `offset.x = tick * new_ppt - (ptr.x - vmin.x)`（`vmin` 取内容 ui 的 `clip_rect().min`，滚动方向上等于视口左上）。Ctrl+Alt+滚轮缩行高（`ROW_HEIGHT * row_zoom` 贯穿 lane_row/grab_targets 的 row_delta）；中键平移直接 load/store `egui::containers::scroll_area::State`（`egui::ScrollArea::State` 不是合法路径——State 未从顶层导出）。纯滚轮/Shift 滚轮保持 ScrollArea::both 原生行为。**网格铺满编排上限**（2026-10-21 追加）：lane/ruler 宽度 = `MAX_ARRANGEMENT_BARS`(256) × px_per_bar，不再随 clip 末端截断——G 模态 ghost / 插入 / drop 任何位置都有 bar 线可对齐，滚动范围与所有交互的 clamp 上限一致；网格与 ruler 绘制**视口裁剪**（`ui.clip_rect()` 换算 first/last bar index，ruler 端 `div_ceil(step)` 对齐刻度步长），256 bar 全铺不涨每帧绘制量。`MIN_CONTENT_BARS` 删除。

**轨道重排**：MIDI 轨头左侧 12px 六点抓手（`Sense::drag()`，hover 显示 Grab 光标，name 按钮缩 12px 让位）。拖拽期间 `state.reorder = Some(from)`，每帧 `reorder_target(ptr.y, midi_lane_rects)` 返回（插入位 `to`，指示线 y）——`to` 是**原数组坐标**的插入位置（行上半 = i、下半 = i+1、末行下 = len）；释放（全局 `pointer.primary_released()`，非 grip 的 `drag_stopped`——指针已跨行）且 `to ∉ {from, from+1}` 时发 `TimelineAction::ReorderMidiTrack`。app 层 `reorder_midi_track`：`track_permutation(len, from, to)`（old→new 双射，测试全组合验证）驱动轨表重排 + `NodeDoc::Instrument/Effect/ChannelStrip` 的 track 字段重映射（**Mixer/Keyboard/Feedback 不动**——track 仅 patch 分组，与 `remove_last_midi_track` 的排除一致）+ `PortRef::MidiTrack` 连线重映射 + `selected_track/edit_clip/timeline selection` 跟随，`apply_document()` 全量重建（undo/redo 同路径零额外代码）。

**坑（中键 pan 与左键手势串台）**：egui 的 `drag_started()/dragged()/drag_stopped()` **不区分按钮**——任何键的拖动都触发，中键 pan 落地后中键拖动会同时进入左键手势（marquee/绘制/clip 移动）。修复 = roll/timeline 所有手势入口改为 `drag_*_by(PointerButton::Primary)`（`clicked()/double_clicked()` 默认就只算 Primary，无需改；patch.rs 自 P5d.1 起一直按按钮过滤，正是先例）。守护测试 `middle_drag_never_reports_as_a_primary_drag`（egui_event_semantics.rs）锁定该契约。

**坑（模态变换失焦 + 落定无效）**：G/T/Shift+D 模态不是真实拖动——router 的 `any_down` 冻结只保护按住按钮的手势，hover-follow 照常运转（指针越过内容边缘到达 neutral chrome 即 surrender，Esc/轴限定键全灭）；且确认点击是 `response.clicked()`（组件矩形内），指针已在组件外时点击落不进任何 lane。修复两层：① `FocusRouter::set_focus_hold(bool)`——模态/armed knife 期间由组件每帧置位，`begin_frame` 跳过整个 hover-follow（与 any_down 同级的冻结；每帧设置所以模态结束自动解）；② 确认/取消改读**全局** `i.pointer.button_clicked(Primary/Secondary)`（timeline 在 finalize 段、roll 在 show() 尾部兜底——lane 内点击先被原有路径消费掉 modal，两路天然互斥；确认 frame 用 `latest_pos()` 与 ghost 预览同源）。守护测试：`focus_hold_pins_the_owner_through_neutral_hover`（focus.rs）+ `a_click_outside_any_widget_is_globally_visible`（egui_event_semantics.rs，锁定"死区点击全局可见、Response 沉默"契约）。**注意**：focus 单测里被聚焦的 id 必须有真实 widget（`ui.interact`）——egui 的 focus dead-man switch 会清掉无人认领的 id。

**坑**：PowerShell 5.1 `Get-Content -Raw` 默认按 ANSI(GBK) 解码 UTF-8 文件 + `Set-Content -Encoding UTF8` 会加 BOM——非 ASCII（→/—/·）全部乱码且不可逆（部分字符吞换行合并行）。**改源文件一律用 edit 工具或 Python（显式 `encoding='utf-8'`）**；已损坏时 `git checkout -- <file>` 恢复后重放编辑。

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

### Inspector 收口：改名、mixer 分派、编辑器折叠头（U8，2026-10-23）

**改名**：`MainView::Synth` → `Inspector`，`synth_panel` → `inspector_panel`，tab 文案 `Synth / Mixer` → `Inspector`。面板标题不再带通道前缀（`Synth — Instrument #3` → `Instrument #3`），因为面板身份已由 tab 表明。

**面板只剩"选中节点的参数"**：三处结构管理搬走——① mixer 控制台改 `mixer_section(ui, mixer_doc)`，只有 `target_doc` 是 `NodeDoc::Mixer` 时才画（原先无论选什么都会画一遍"first mixer"）；② 混音台头部的 `+ Track` / `- Track` 删除（timeline 工具栏本来就有同名按钮，且那是轨道管理不是混音参数）；③ `automation_section` 拆解：`automation_strip`（新增，timeline 底部 `egui::Panel::bottom("glitcherz_automation")`）+ `add_control_track` / `remove_control_track` + `auto_target_row`（端点选择器，原样搬过来，`index` 改成入参）+ 删除 `clip_section` 死文案。曲线管理是轨道级状态，跟轨道放一起。

**`Panel::bottom` 的声明顺序**：必须写在 `timeline::show` **之前**（面板先占位，再让内容吃剩余高度）；`timeline::show` 内部的 ScrollArea 是 `auto_shrink(false)`，放后面会把 strip 挤出屏幕。

**插件编辑器改折叠头**（原来 `Show Editor` / `Close Editor` 按钮，展开后原生子窗口会盖住按钮）：

- 用 `CollapsingHeader::new("Editor").id_salt("plugin_editor_header")`，三角独占一行在占位矩形上方，原生窗口只能覆盖 body 的 rect，永远压不到开关。
- **`.open(Some(self.editor_user_open))`**：让 app 的 `editor_user_open` 成为唯一真值。若交给 egui memory 自己记，插件自毁 GUI（`gui_closed()` → 清 flag）那条路径会把 header 留在展开态，下一帧"header 开着但 flag 是 false"就把它又拉起来——死循环。强制 open 后点击不再自动 toggle，改由 `header_response.clicked()` 手动翻转 flag（`close_plugin_editor()` / `plugin_editor_wants_open = true`）+ `request_repaint()`，下一帧 header 跟上。
- body 只在展开时布局：会话可能比点击晚一帧（`plugin_editor_wants_open` 在本帧尾部才被消费），因此 body 里用 `if let Some(editor) = ... && Some(editor.doc_node) == target` 兜住 `None`，不画占位文字（避免一帧闪烁）。
- 折叠后立刻 `close_plugin_editor()`，不存在"隐藏但仍然加载"的中间态；`editor.rect` 留旧值无害（会话已销毁）。

### 跨平台构建：VST3 枚举类型 + eframe 的 Linux 后端（2026-10-23）

症状：Windows 通过，`ubuntu-latest` / `macos-latest` 各自报一类错（CI 从没绿过）。

**① `vst3` 0.3 的枚举类型按平台分叉**：生成的绑定把每个枚举写两遍 —— 常量容器（`Vst::MediaTypes`、`Vst::BusDirections`、`Vst::ProcessModes`、`Vst::SymbolicSampleSizes`、`Vst::Event_::EventTypes`、`Steinberg::IBStream_::IStreamSeekMode`、`PClassInfo_::ClassCardinality` …）的类型是 `vst3::support::DefaultEnumType` = `c_int`（Windows）/ `c_uint`（其他），而**接收这些值的字段与接口参数一律写成 `int32`**（`ProcessSetup::processMode`、`ProcessData::symbolicSampleSize`、`IComponent::getBusCount(type, dir)` 等）；也有少数常量的类型本身就是 `int32`（如 `ParameterFlags`），天然对齐、不用转换。Windows 上两者都是 `i32` 所以看不出问题，macOS/Linux 上是两种类型，`cargo build` 直接 `E0308`。

接缝收在两个 `compat` 里（宿主 `src/compat.rs`、fixture 内同名小函数）：`to_int32`（常量 → `int32` 槽）/ `to_uint32`（`BusInfo::flags` 这类 `uint32` 槽）/ `to_enum`（`int32` 参数 → 枚举类型，好在 `match` 里直接对常量分支）。泛型走 `TryInto`/`TryFrom`，两个方向都能编译，不必写 `#[allow(unnecessary_cast)]`——`as` 在 Windows 上是多余的、在 Linux 上是必需的，正是 `clippy -D warnings` 会卡住的那种差别。判 `match` 的入参一律先 `to_enum(...)` 再对常量，避免 `m if m == ...` 这种守卫写法。

**② eframe 关掉默认 features 会连 winit 的 Linux 后端一起关掉**：workspace 里 `eframe = { default-features = false, features = ["default_fonts"] }`（为了不拉 wgpu），`glitcherz-ui` 只补了 `glow`/`persistence`，于是 `x11`/`wayland` 两个 feature 也没了 —— winit 在 Linux 上 `compile_error!("The platform you're compiling for is not supported by winit")`。`glitcherz-ui` 现在显式带 `wayland` + `x11`（在 Windows/macOS 上是空操作），`Cargo.lock` 因此多出 X11/Wayland 栈（另有一批只进 lock、不进构建图的 `egui-wgpu`/`wgpu` 条目，无影响）。`winit/default` 附带的两项没跟着开：`wayland-dlopen`（免运行期 `libwayland` 硬依赖）与 `wayland-csd-adwaita`（GNOME Wayland 下自绘窗口边框）——CI 装了 `libwayland-dev`，链接期没问题；要在 GNOME 上跑得舒服再单独补 feature。

**验证方式**：Windows 上无法直接编译 Linux/macOS，`cargo check` 交叉 target 又会卡在 `alsa-sys` 的 pkg-config。用 `[patch.crates-io]` 指一份把 `DefaultEnumType` 改成恒为 `c_uint` 的 `vst3` 副本，就能在 Windows 上跑出 macOS/Linux 的 `E0308`，改完即绿；feature 侧用 `cargo tree --target x86_64-unknown-linux-gnu -e features -i winit` 确认 `x11`/`wayland` 真的上来了。
