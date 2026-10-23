# 计划与状态 — GlitcherZ

> 管计划与进度。已完成模块的实现细节沉淀到 `implementation.md`，本文件只留任务与方向。

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
10. ✅ VST3 插件宿主（乐器/效果器已集成进 model/UI；遗留项见下）

## 当前状态（2026-10-22）

路线图 1–10 全部落地。乐器插件（CLAP / VST3）为一等声源，效果器节点已接入，自动化桥接到插件参数。节点图 P1–P5f 与 MIDI 编辑 M1–M4 全部交付；**SF2/SF3 音色库方向 S1–S6 全部完成**（RT 审计零分配 → 非实时加载 `engine::sf2` → 实时渲染 `Sf2VoiceProcessor` → `VoiceSpec::SoundFont` 文档变体 → UI 音色库面板（加载/preset 下拉/声源绑定，undoable）→ sf3 feature 常开 + 审计复跑 + 真实库端到端验收）。SF2 声源现为三等声源之一（内置合成器 / 插件 / SoundFont）。**插件 GUI 宿主 G1–G7 全部交付**（CLAP 编辑嵌入 + 参数回接 + 面板节点化 + 内联参数 + vk 节点 + VST3 `IPlugView` 镜像）；**P6 反馈环路交付**（显式 Feedback 节点，1 块延迟由拓扑排序反转实现）。**可用性提升 U 系列进行中**：U1（走带完整化 + seek 基建）、U2（文件管理：File 菜单 / New / SMF 导入导出 / dirty 确认）与 U3（拍号文档化 + tempo 曲线）已交付，下一阶段 U4（控制轨 CC 输出 + timeline 一等化）。

## 遗留任务

- [x] CLAP 插件参数**枚举**（2026-10-08 完成：`ClapInstance::list_params` 经 params 扩展 vtable 直调（`raw::enumerate_params`，clack 0.2 包装无读取方法）；测试插件实现 Amplitude 参数；UI 参数面板 + 自动化目标选择器接入 `clap_voice_params`）
- [x] CLAP 乐器参数**滑块面板**（2026-10-15：`clap_voice_values` 会话内当前值 map（track → param id → f64 原始值域），滑块变更经 `send_track_param(ParamId::PluginParam)` 直达 `ClapNode`；重载插件/切回内置声源/文档重应用时重置为插件默认值；state 持久化落地后（见下），重载路径经 `load_state` + `read_param_values` 重播种滑块缓存。VST3 滑块已接（归一化 0..1，见下）；Dexed 兼容性复现测试见 `glitcherz-clap-host` 的 `dexed_instrument_note_on_produces_audio`）
- [x] 插件状态保存/加载 **CLAP 部分**（2026-10-15，spec `spec/plugin-state.md` D1–D7 + S1–S4：`VoiceSpec` 加 `state: Vec<u8>`（serde default，PROJECT_VERSION 不动）；clack state 扩展宿主捕获 + `ClapInstanceGuard::{save_state, load_state}` + raw `get_value` 滑块回填；do_save 采集 / install_node 恢复；测试插件 amplitude 往返测试。VST3 FState 镜像 = S5，见下条）
- [x] CLAP / VST3 效果器节点参数桥接（2026-10-15：效果器参数缓存以**文档 node_id** 为键（跨引擎重建稳定），UI 面板经 `set_node_param(engine_id, ParamId::PluginParam)` 节点寻址直达效果器节点——"参数空间区分"由图节点寻址自然消解；CLAP 显示原始值域、VST3 显示归一化 0..1（与乐器同split）；live 插入与文档重建（install_node）双路径填充缓存，删除节点/清空效果器/整批重建三处清理；自动化目标选择器接入效果器参数（端点 = 效果器 doc_id，曲线值域随格式））
- [x] VST3 参数归一化换算的 UI 层处理（2026-10-15：v1 决策——滑块直接工作在归一化 0..1 域，不做 plain↔normalized 换算（VST3 ParameterInfo 不携带 plain 值域，换算需逐参调控制器；归一化滑块诚实且够用）；当前值经 `read_param_values`（S5 落地的 `getParamNormalized` 读回）播种会话缓存，CLAP `clap_voice_values` 的镜像 `vst3_voice_values`；自动化曲线值沿用归一化域）
- [x] 插件状态保存/加载 **VST3 部分**（2026-10-15，S5 镜像 clap-host：`HostMemoryStream`（`IBStream` 内存流 COM 对象，host_impl）+ `Vst3InstanceGuard::{save_state, load_state, read_param_values}`——save 走组件 `getState`，load = 组件 `setState` + 控制器 `setComponentState` 同步（各给一条独立流）；测试插件工厂改**逐实例创建**（原 OnceLock 单例与 kManyInstances 相悖且无法测"新实例回落默认值"），SineComponent 以 8 字节 LE f64 持久化 Gain（经 `IParameterChanges` 消费、作用于振幅）、控制器镜像读回；UI install_node 两处恢复 + capture 采集 + 文档写回；端到端 `state_roundtrip_restores_gain`。`read_param_values` 为归一化滑块（下一条）预留）
- [x] clip 拖长自动平铺 bug 修复（2026-10-20，用户实测"拖长 clip 尾部后第一小节音符自动重复"：UI resize/stretch/split 打破 D14 单周期前提 `loop == span`，引擎预埋的周期平铺调度意外生效；split 还会产生 `span < loop` 的非法文档（保存报错）。修复 = UI 层所有改 `span_ticks` 的路径同步 `loop_ticks`，split 右半段内容平移（跨界音符两半延续），拖短遗留内容保留数据仅隐藏。新增小节从此空白可输入。细节见 implementation.md"clip 单周期不变量"节）
- [x] 插件 GUI 宿主 → 升级为独立方向并全部交付（G1–G7，见下方"插件 GUI 宿主 + Patch 交互升级"，spec `spec/plugin-gui.md`）
- [x] 乐器预设系统 v1（2026-10-15，实测"只能拖滑块太痛苦"的直接回应：宿主侧预设库 `presets/<插件键>/<名字>.glzpreset`（原始状态字节，插件键 = 格式 + 路径 hash8 + id，同名插件不同安装互不串库）；UI 一行 Preset 工具条（下拉加载 / Reset / 名称框 + Save As / Delete），乐器 CLAP 与 VST3 通用。加载/重置 = 先活体 `load_state`（插件拒收则文档不动）→ 滑块缓存回填（CLAP 守卫补 `read_param_values`）→ `VoiceSpec.state` 写入为**一步可撤销编辑**；Reset 用 spawn 时抓的出厂默认快照（`voice_default_state`，文档 state 清空 = 重载即出厂默认）；Save As 是文件操作不进 undo。效果器预设为后续项——机制（守卫 + doc 键缓存）可平移）
- [x] Dexed .syx 预设导入工具（2026-10-15，clap-host dev harness `dexed_import_ym2612_banks`，`#[ignore]` + `DEXED_IMPORT_BANK_DIR` 环境变量指定 bank 目录）：DX7 bank（4104 字节，32 音色）→ Dexed state 构造。逆向出的格式要点：state = `VC2!` + u32 LE xml_len + XML + `\0`；XML 的 dexedBlob 元素带 `base64:sysex`（整盘 4104 bank）与 **`base64:program`（161 字节 data[]，加载时 `memcpy` 覆盖引擎数据——真正决定声音的是它**）双属性；编码是 **JUCE 专有 base64**（`MemoryBlock::toBase64Encoding`：字母表 `.` + A-Z + a-z + 0-9 + `+`，无 `/` 无填充，"SIZE." 前缀）。导入 = `Cartridge::unpackProgram` 移植（128 位包 → 155 引擎字节）+ 模板尾部 5 字节继承 + 双属性替换 + `currentProgram`。YM2612-Dexed-Presets 四个 bank 128 个音色已入库（逐个 program 往返校验 + 每行首音色出声验证），预设文件为本地数据不入库
- [x] Dexed 播放缺陷修复（2026-10-16，实测"preset 只有 click / Stop 停不了 / 循环杂音"三个症状，诊断 harness 三个见 implementation.md）：导入数据无问题。根因与修复——
  - [x] **Stop/Play 按 pitch 释放**：`note_off_all` 原来只发 `Off{note:0}`，pitch 匹配的插件（Dexed `keyup` 按 `midi_note` 查找）永远收不到 → 挂死音。修复：`Sequencer::take_pending_offs`（清槽并带出真实音符号）+ `BlockRunner::release_pending_notes`（Play/Stop 先按音符号逐轨 Off，再兜底群发）；回归测试 `stop_drains_pending_offs_by_pitch`
  - [x] **循环长音符合并 retrigger guard**：`pending_offs` 每轨单槽被新 occurrence 覆盖 → 前一 occurrence 的 off 丢失 → 挂死堆积（"永远重复的杂音"）。修复：`schedule_track_clips` 同音符重触发时先发 Off 再发 On（槽清空 + 干净重触发）；回归测试 `long_note_in_looping_clip_retriggers_with_releases`
  - [x] **`StateGate` 主线程调用与音频线程串行化（正确性加固，CLAP + VST3 镜像）**：guard 的 save/load/read_param_values 与音频线程 `process` 并发违反 clack 主线程契约；`glitcherz_engine::host_gate::StateGate`（Arc 共享原子门：音频侧 `audio_should_park` 停靠应答 + 控制侧有界等待握手），guard 三方法包裹，spawn 时（节点未处理）不等待。验证：竞态 A（绕门）/B（走门）/C（干净对照）三组渲染逐字节一致——**实测竞态未污染 Dexed state（此前"竞态复现"是 morph 音色自身 pluck 包络的假阳性），gate 为契约加固而非实测缺陷修复**
  - 遗留：preset 音色在 App 实测仍 click 的根因尚未完全钉死（引擎级单音符/长音符路径已验证正常；优先怀疑挂死音修复后消失，待用户复测确认）
- [x] piano roll / timeline 视图缩放与平移 + 轨道重排（2026-10-21：两视图统一 Blender/DAW 式导航——**roll**：`RollState` 加 `zoom_x/zoom_y/scroll_x`（几何全面参数化，`t0_x` 单一来源），Ctrl+滚轮沿时间轴缩放（指针锚定 `zoom_scroll_anchor`）、Ctrl+Alt+滚轮纵向行高缩放（`zoom_top_note_anchor`）、Shift+滚轮水平滚动、滚轮纵向滚动、中键拖动双轴平移，velocity lane 与主 roll 共享横向几何；**timeline**：`TimelineState` 加 `row_zoom/rows_rect/rows_viewport_min`，Ctrl+滚轮水平缩放（指针 tick 锚定 + ScrollArea offset 当帧补偿——导航输入在 `ScrollArea::show` 之前用上一帧行几何处理）、Ctrl+Alt+滚轮行高缩放（0.5–3×）、中键平移（直接写 `containers::scroll_area::State`，注意闭包内改会被 end 覆盖、必须在 show 之前）、纯滚轮/Shift 滚轮仍归 ScrollArea。**轨道重排**：MIDI 轨头 6 点抓手拖拽，`reorder_target` 按行中心求插入位（原数组坐标）、`track_permutation` 生成 old→new 置换，drop 产生 `TimelineAction::ReorderMidiTrack`，app 层 `reorder_midi_track` 一步 undo 编辑（轨表 + Instrument/Effect 节点 track 字段 + `PortRef::MidiTrack` 连线索引 + selected_track/edit_clip/timeline selection 同步重映射）后 `apply_document` 全量重建（undo/redo 同路径）。测试：roll 锚定不变性 2 + 默认值、timeline 置换双射全组合 + reorder_target 分档 + 默认值；全仓 fmt/clippy/test 绿）

## 新方向：模块节点图架构（spec 已定稿）

2026-10-07 评估可行并经两轮拷问式 Q&A 定稿，spec 见 `spec/node-graph.md`。核心决策：定容 64 节点/128 连线/每节点 ≤8 入边；端口剖面按节点类型；三连线类型（Audio/Midi/Automation）UI 层拒绝类型不匹配；拓扑变更 = 重建执行计划 + 差异化节点回收（复用现有结构队列/回收协议，调参永不重建）；悬空节点照常执行、主输出与时间轴节点固定存在；时间轴节点区分 MIDI 轨道与自动化轨道两类端口组，自动化曲线不再内嵌目标（目标 = 连线终点）；**不做 v1 兼容**，model v2 重构；P1+P2 连做不留并存中间态。

- [x] P1+P2 图核心 + 替换 Mixer（2026-10-07 完成：`realtime::graph` 模块落地，旧 mixer 数学测试以默认图形式全数通过 + 图专项测试；行为变化：`set_builtin_voice` 改为回收外部节点而非驻留，切回插件需重新实例化）
- [x] P3 时间轴入图（2026-10-08 完成：`ExecPlan::midi_targets` 每轨扇出 ≤4 + `ExecPlan::auto_routes` 终点 `(槽位, 参数)`；`RtAutomation` 去目标化为纯锚点，新增 `RtAutoTrack{loop_ticks=384, curve}` 独立实体，`SequencerCommand::SetAutomation` → `SetAutoTrack`；`AudioEngine::set_auto_route/clear_auto_route/set_auto_track`；UI `send_automation` 同步下发连线终点；结构队列 8→64。Sequencer 内核零改动，音序/自动化端到端测试经连线路径通过）
- [x] P4 model v2（`Project` v2 + GraphDoc 过渡形态：midi_links + loop_ticks 显式化，无 v1 读路径）✅ 2026-10-08
- [x] P5a CLAP 参数枚举（clack 0.2 raw vtable 直调；测试插件 Amplitude 参数端到端）✅ 2026-10-08
- [x] P5b 引擎寻址层 node_id 化 + 轨道定容 16（ExecPlan slot→node_id 表；cable 列表取代单出边；`SetMidiTrackCount` + add/remove_midi_track）✅ 2026-10-08
- [x] P5c 模型完整形态（GraphDoc{nodes, cables} 终态，PROJECT_VERSION=3；Track 拆 MidiTrack + 图节点文档；automation_tracks 顶层化；D13 全显式：加载器经 add_node/connect_nodes 粒度按文档重建；双身份约定：文档 node_id 持久化、引擎侧 fresh_node_id + UI node_map 映射，插件守卫经退役区按回收回执释放）✅ 2026-10-08
- [x] P5d UI 节点画布（`patch.rs`，原 canvas.rs：画布只做视图 + 交互意图，`PatchAction` 统一落文档 + 引擎；平移/缩放/类型着色/贝塞尔连线分色/拉线端口类型校验/自动化拉线参数菜单/Delete 删除联动，效果器删除走旁路语义；节点位置会话内存不持久化）✅ 2026-10-08
- [x] P5d.1 画布交互打磨（2026-10-09：左键点选/框选（Shift 追加多选，多选节点整体拖动，Delete 批量删除——先按降序删连线再删节点防索引漂移）、中键平移、滚轮/Ctrl+滚轮缩放（指针锚点）；旧"拖背景平移"移除。修复两项：① `screen_rect` 统一乘 zoom，绘制/端口/命中单一几何源（此前端口与框选在 zoom≠1 时漂移）；② egui 0.36 对 click+drag sense 做**点击/拖拽判别**，原地按下不触发 `drag_started`，点选补走 `clicked()` 路径。新增 `glitcherz-ui/tests/egui_event_semantics.rs` 锁定该事件语义防 egui 升级回归）
- [x] P5d.2 缩放初值修复 + 改名 Patch（2026-10-09：① `PatchState` 曾 `#[derive(Default)]`，`zoom: f32` 初值 0.0 → 整图坍缩到左上角、点 Reset 才恢复，改为手写 `Default`（zoom=1.0）+ 回归测试 `default_zoom_is_one_not_zero`；② **canvas → patch 全量改名**：Pure Data 语境下连线图叫 patch，`canvas.rs` → `patch.rs`，`CanvasId/CanvasAction/CanvasDeps/CanvasState` → `PatchId/PatchAction/PatchDeps/PatchState`，`MainView::Canvas` → `MainView::Patch`，Tab 标签 "Canvas" → "Patch"）
- [x] P5e Patch 自由加删节点（2026-10-09：① 右键 context menu 添加 Instrument / Effect (Delay) / Channel Strip（`AddNodeKind` + `PatchAction::AddNode{kind, at}`；新节点悬空出生 D6、归属当前选中轨、位置落在点击处——文档 push → 引擎 `install_node` → `PatchState::place_node`，引擎安装失败回滚文档，节点表满 64 拒绝）；② `DeleteNode` 放宽到全部文档节点（master/timeline 固定不可删）：引擎 bypass 语义不变，MIDI/自动化触及连线直接移除，删除后刷新所属轨寻址表；③ 抽取 `register_track_chain(track)` 助手（apply_document 与加/删节点共用）。回归测试：place_node 位置不被 ensure_positions 覆盖。遗留：面板仍是"轨→first-match 节点"驱动，多乐器/多 strip 轨的面板寻址与 `track` 字段弱化待后续）
- [x] P5e.1 节点位置持久化（2026-10-09：`GraphDoc.layout: LayoutDoc{nodes: BTreeMap<u64, [f32; 2]>, timeline, master}`——`#[serde(default)]` 旧文件无感兼容，BTreeMap 保证序列化稳定，model 不依赖 egui；`PatchState::export_layout`（保存前导出，剔除文档中已不存在的 id）/ `import_layout`（加载后导入，缺项回落自动布点）；`do_save` / `do_open` 接线。demo_project 携带 layout 进入 roundtrip 覆盖；patch.rs 增 export 过滤 stale id + 往返回归测试。168 测试绿）
- [x] P5e.2 连线交互打磨（2026-10-09：① **端口抓取热区** `PORT_HIT_RADIUS` 12→18，拉线起点更好抓；② **Ctrl+右键拖拽切线**（Blender 式 knife）：记录指针轨迹，采样点距轨迹 < 10px 的连线红色高亮，松开批量 `DeleteCable`（索引降序防漂移）、Escape 取消；Ctrl+右键单击不再弹加节点菜单；③ **连线中间控制点**：每条连线贝塞尔中点画控制圆点（hover 放大），点击弹菜单——音频线可 Insert Effect / Insert Channel Strip（`PatchAction::InsertNode{cable, kind, at}`：A→B 替换为 A→新→B，新节点落位点击处，复用 `create_patch_node`/`connect_audio`），Delete Cable 全类型可用；几何命中统一收敛到 `cable_controls` 助手 + `dist_point_seg`。170 测试绿）
- [x] P5f 单输入语义 + Mixer 节点（2026-10-09 完成，commit `b34d0cc`，spec §9 ①–④ 一次交付）：① 引擎 `MixerProcessor`（2..32 通道 gain/pan/mute/solo + 输出级 pan/mute；`input_weights()` 权重表参与图求和循环，默认实现 `None` = 单位权重、插件零改动）+ `NodeKind`（Plain/Mixer/Master）compile 期入边纪律 + 容量扩容（`MAX_IN_EDGES` 32 / `MAX_CABLES` 256）+ D19 旁路修订（≤1 入边才旁通）；② model `NodeDoc::Mixer{inputs, channels, out_pan, out_muted}` 替代 ChannelStrip（旧变体仅作加载路径）+ `CableDoc.in_port`（D17 连线↔输入口绑定持久化）+ 旧版文档加载期自动迁移（strip → 2 通道 mixer、master 多入边 → 合成 mixer）；③ UI 调音台面板（每通道 solo/mute/推子/pan + 输出段 + 通道增删 2..=32、占用口保护）+ mixer 节点按口落线（精确口命中优先、否则第一个空通道）；④ 轨寻址：`track_channel_target` 沿连线推导轨 → (mixer, channel)，轨推子/mute 走 `ParamId::Mixer*` 参数路径（永不触发重建），电平点 = 轨道链末端 delay。验收（spec §9）：默认图端到端 / 32 通道加权求和数学 / 参数路径不重建 / 迁移往返 / P5e.x 画布回归——全绿
- [x] P6 音频反馈环路 ✅ 2026-10-21（commit `f47567e`）：显式 **Feedback 节点**（`NodeKind::Feedback` / `FeedbackProcessor` / `NodeDoc::Feedback{gain}`，patch 右键 "Feedback Tap"）。1 块延迟不是缓冲而是**排序**：编译期把「入 Feedback 的连线」约束反转（Feedback 先跑，抽头源后跑），运行时抽头源缓冲里还是上一块的输出；无环的 Feedback 同样延迟一块（语义统一）。`gain` 0..1 默认 0.5 防裸环发散；自环与不含 Feedback 的环仍报 `Cycle`；删除 Feedback 不走旁路（会重连成非法直连）。测试 5 项（环豁免/一块延迟递推 1→1.5→1.75→1.875/无环也延迟/自环拒绝/删除丢线）

## 新方向：MIDI 编辑能力对标 SoundCraft（M1–M3）

2026-10-10 调研 `.ref/soundcraft`（对标 Pro Tools），完整报告见 `ref-soundcraft-midi.md`。SoundCraft 的 MIDI Editor 本身仅 354 行，真正可借鉴的是周边基础设施。按依赖顺序分三步，**M1 不做完不要开 M2**——M2 的多选/框选依赖 M1 的稳定索引，M3 的 ops 要在 M1 的数据模型上做。

### M1 地基：数据模型 + 命令层（breaking change，单独 commit）

- [x] **M1.1 PPQ 96 → 960** ✅ 2026-10-10。`TICKS_PER_QUARTER`=960 / `TICKS_PER_STEP`=240 / `TICKS_PER_BAR`=3840（后者改为派生，不再硬编码）。**顺带修隐患**：`realtime/sequencer.rs` 原先在 27/30/83 行**重复定义**三份常量，engine 侧改为单一来源；新增 `glitcherz-ui/tests/tick_base_consistency.rs` 锁住两侧一致（engine 刻意不依赖 model，实时线程与文档层解耦，故用测试而非 `pub use` 约束）。
      960 的收益已用测试锁定：1/64 三连音 = 160 tick（整除）、附点 1/16 = 360、1/8 三连音 = 320，96 PPQ 下这些无法精确表达。
- [x] **M1.2 `Note` 字段对齐** ✅ 2026-10-10。`velocity: f32`(0..1) → `u8`(1..127)，`DEFAULT_VELOCITY=100` / `MIN_VELOCITY=1`；`start`/`length`: `u32` → `i64`；新增 `channel: u8`。
      `Note` 新增 `end()` / `start_clamped()` / `length_clamped()` / `velocity_f32()`；`RtNote` 新增 `new(i64,…)` 在引擎边界一次性 clamp，**音频线程不再需要 NaN 防护**（原 `sanitize_velocity` 已删——整数 velocity 不可能是 NaN）。
      piano_roll 音符亮度改为编码力度（`0.45 + 0.55*vel/127`）。
- [x] **M1.3 `Sequence { notes, ctrls }`** ✅ 2026-10-10。取代裸 `Vec<Note>`，为 CC/PitchBend/Aftertouch 留位（D2）。新增 `CtrlEvent`/`CtrlKind`（`group_key()` 供分组排序）。
      `sort()` 不变量：notes 按 `(start, note, channel, length, velocity)`（注意我们的字段叫 `note` 不是 soundcraft 的 `pitch`），ctrls 按 tick **稳定**排序。
      `remove_duplicates()`：同 `(channel,note,start)` 保留最长；同 `(tick,channel,kind)` 保留最后一个（=合成器实际发声值）。
      另加 `end_tick()` / `notes_in()`（按 start 命中）/`notes_overlapping()`（按发声区间相交）。
      **坑（已在 doc comment 标注）**：sort 后 `Vec<usize>` 选择集索引失效，持有索引的调用方必须重新解析。
      ⚠️ 试过 `#[serde(flatten)]` 让旧 `notes` 字段自动兼容——**不行**，flatten 破坏 RON 的 tuple struct 语法，`midi_clip_partial_fields_use_defaults` 直接解析失败。改为嵌套 `sequence` 字段 + 迁移。
- [x] **M1.5 项目文件迁移** ✅ 2026-10-10。`PROJECT_VERSION` 5→6。`Project::migrate_v5_to_v6()`：所有 tick ×10（clip start/span/loop、音符 start/length、自动化锚点），v5 的 `notes` 折进 `sequence`，velocity 0..1 → 1..=127（0 → `MIN_VELOCITY=1`，NaN → `DEFAULT_VELOCITY`）。v6 文档不动（幂等）。
      `parse_project` 按 `version < PROJECT_VERSION` 分支触发。
      **坑**：`Option<Vec<LegacyNote>>` 在 RON 里必须写 `Some([...])`，缺失字段直接 `ExpectedOption` 报错 → 改用裸 `Vec` + `#[serde(default, rename="notes", skip_serializing)]`。
- [x] **M1.4 新 crate `glitcherz-command`** ✅ 2026-10-10（commit `0e87044`）。采用方案 A：文档移入 `Arc<Project>`，`Session` 持有文档 + undo/redo 栈 + 手势状态，快照 = 一次原子引用计数自增。
      `Session::execute` 三条保证：① 命令**报错后回滚**（不留半成品）；② **panic 被 `catch_unwind` 捕获并回滚**（不是崩溃）；③ 每条命令记录一步 undo（`Arc` 克隆，O(1)）。
      `execute_merged(id, params, key)` 合并同 key 的连续调用（保留首个快照、丢弃后续）→ **一次拖动 = 一步 undo**，这是手感命门。
      命令注册表：`CommandSpec{id,label,menu,shortcut,params,enabled,run,undoable}` + `cmd!`/`cmd!(noundo ...)` 宏。首批 6 条 MIDI 命令（add/edit/delete/move/change_velocity/query）。
      **piano_roll 改造**：不再直接改文档，改为返回 `RollIntent`（AddNote/EditNote/DeleteNotes），由 app 走命令执行。`note_add` **返回新 index** —— 排序会置换 vector，否则拖拽目标会指错音符。
      `Ctrl+Z` / `Ctrl+Y` 绑定 undo / redo。undo/redo 后 `push_all_clips()` 重推 clip 表（音频线程持副本）。
      自检测试：`every_command_survives_empty_params` + `no_command_panics_on_a_populated_document`（空参数/有数据两种情况都不许 panic）。

> **M1 全部完成**。全部文档修改已迁移到可撤销路径（commit `9d380bb`）：每个编辑点 `begin_edit()` 快照 → `project_mut()` 修改 → `commit_edit(label)` 记一步 undo（无实际变化不记录，`Arc::ptr_eq` 判定）。
      两个新原语：`push_undo_merged(key)`（带外编辑的手势合并——tempo/master gain 拖动一次拖 = 一步）、`suspend_history()`（嵌套编辑只记外层一步——加 MIDI 轨可能扩 mixer）+ `restore(snapshot)`（引擎侧失败回滚，不进历史）。
      **timeline 与 automation lane 也改造为纯视图**：`TimelineAction` 新增 `AddClip/MoveResizeClip/RemoveClip`，lane 返回 `LaneIntent`，由 app 走守卫执行；clip 拖动经手势键合并为一步。
      undo/redo 后 `apply_document()` 全量重建引擎图（undo 可能恢复节点/连线变化，不只是 clip 表）。
      **顺带修 M1.1 漏网**：lane 的 `SNAP_TICKS=12` 还是 96 PPQ 值（不派生自常量所以没被重算），改为 `TICKS_PER_STEP / 2` 派生。
      唯一例外：`do_save` 写 layout（保存不是编辑）。`GlitcherZApp::project_mut()` 仍存在，但新代码一律走守卫。

**M1 验收**：`cargo test --workspace` 全绿；旧 `.glz` 文件加载后 tick 语义不变；命令层空参数自检通过。

> **M1.1/M1.2 过程中的发现（2026-10-10）**：`auto_stop_keeps_pending_offs_draining` 测试原本硬断言"自动停止发生在第 750 个 block"。PPQ ×10 后实测停在**第 751** 个——位置计数器是 f64，每块累加 `dt_ticks` 会积累亚块误差。原断言在 96 PPQ 下是**侥幸通过**的。已改为"循环直到 `is_playing()` 为假"的不依赖具体 block 的写法。**教训：涉及时间推进的测试不要硬编码 block 数**，PPQ 或 tempo 变更时这类断言会静默失效。

### M2 编辑交互（依赖 M1）

- [x] **M2.1 `GridValue`**（2026-10-11，`glitcherz-ui/src/grid.rs`）：`Note(NoteValue{division, dotted, triplet})` + `Seconds/Frames/Samples`（后三个 UI 暂不暴露，换算公式 `ticks = t·bpm·ppq/60` 与采样率无关；测试锁定 1/64T=40 tick 等精度）。roll 工具栏 ComboBox 选网格。
      **网格线从每个 bar line 重新计数**（bar 内循环 `tick += grid_ticks`），拍号变化不会错位。
      顺带：960 PPQ 下旧 `SNAP_TICKS` 常量被枚举取代；三连音网格不再有舍入误差（96 PPQ 时代 1/8T=8 tick 是近似值）。
- [x] **M2.2 多选 + 橡皮筋框选** + Shift/Ctrl 追加（2026-10-11）：`RollState.selection: Vec<usize>`。约定：点击音符单选（修饰键=toggle）、空处**按下即拖=框选**、**原地点击=建音符**（阈值 4px 区分）。拖动移动**整个选区**。
      **核心不变量**：选区索引在"统一 delta 移动 + 稳定排序"下保持有效——roll 侧 `clamped_move` 把 delta 钳到选区内边界（越界钳制若不对称会置换排序、索引漂移），专门测试锁定。删除/撤销后 `clear_gesture()` 全清。
- [x] **M2.3 方向键微调**（2026-10-11）：roll `request_focus()` 后响应 Arrow——上下半音（Shift=八度）、左右一格网格；走 `notes_move` 同一合并手势。
- [x] **M2.4 velocity lane**（2026-10-11）：roll 下方 48px lane，力度条与音符同 x 映射、同高亮色。拖动发 `SetVelocities{updates}`（绝对值 map）→ `change_velocity` 新增 `set_each` 参数 → `execute_merged` 合并为一步 undo。有选区时整个选区跟随指针；无选区时扫过的音符被"刷"上力度。
- [x] **M2.5 音符亮度编码力度**：M1.2 已随 velocity u8 化落地（`0.45 + 0.55·vel/127`）。

### M3 编辑能力（依赖 M1 数据模型）

- [x] **M3.1 `ops.rs` 纯函数**（2026-10-11，`glitcherz-model/src/ops.rs`，commit `8616ce0`）：`transpose` / `apply_velocity` / `apply_duration` / `quantize`，全部 `fn(&mut [Note], Options)` 原地改、不改长度；clamp 不 drop；随机走 XorShift64*（`DEFAULT_SEED=0x5EED_50DC_4AF7_0001`，`*_with_seed` 可传种子）。
      **选区形式**：`apply_to_selected(notes, indices, op)` 复制-变换-写回，ramp/legato 的顺序只在选区内计算（"作用于选区"语义）。调用方负责 re-sort。
- [x] **M3.2 量化**：8 个 `QuantizeOptions` 齐全。`exclude_within` 容差窗口（半格的分数）；`strength` 插值；**swing 按报告实现**——奇数线延迟 `swing·grid/3`，最近线在 k-1/k/k+1 三候选里搜（报告特别警告过别写成简单偏移，测试 `swing_searches_three_candidate_lines` 锁定）。
      命令 `midi.quantize {track, clip, indices?, grid_ticks?, strength?, swing?, include_within?, exclude_within?, quantize_ends?, preserve_duration?, random?}`。
- [x] **M3.3 力度**：`VelocityOp::{Set,Add,Scale,Limit,Ramp,Humanize}`；Ramp 按 start 时间线性渐变（crescendo），Humanize 走种子随机。命令 `midi.velocity_op {op: set|add|scale|limit|ramp|humanize}`。
- [x] **M3.4 时值**：`DurationOp::{Set,Add,Scale,Legato{gap},RemoveOverlap}`；Legato 延到下一音前 gap（时间序，选区内）；RemoveOverlap 同 pitch 重叠截断。命令 `midi.duration_op {op: set|add|scale|legato|remove_overlap}`。
      **注意**：pitch/length 都是排序键，四条命令执行后都 re-sort。
- [x] **M3.5 SMF 导入导出**（2026-10-11，`glitcherz-model/src/smf.rs`，commit `7832286`）：format 1，division=960 与内部 PPQ 一致（**双向零换算**）。导出：clip 按 absolute 位置平铺 loop、conductor 轨写 tempo/拍号；导入：报告边界清单全覆盖（running status / vel-0=off / 同 pitch FIFO 配对 / 悬挂音轨末闭合 / 零长强制 1 / SMPTE 按 120BPM 约定 / 畸形报错带字节 offset / MAX_TRACKS+MAX_EVENTS 上限），导入后 `remove_duplicates` 语义的排序去重。
      **语义要点**：format 1 的 conductor 轨（纯 meta）折叠进全局 tempo 不生成轨道；clip 挂在最早音符的绝对 tick、notes 相对化（保持 arrangement 位置）。
- [x] **M3.6 Event List 窗口**（2026-10-11，`glitcherz-ui/src/event_list.rs`）：`egui::Grid` 表格 Start/Note/Vel/Length/Ch + 每行删除；Note 列 `custom_formatter` 显示 "C4" 风格音名（编辑值仍是半音数）。DragValue `from_get_set(Option<f64>)` 收集每帧变更到一个 partial `NoteEdit` → `midi.note_edit` 合并手势（一拖=一步 undo）。放在 roll 下方 CollapsingHeader。
- [x] **M3.7 绘制手势 + Edit 标签页**（2026-10-12，`glitcherz-ui/src/piano_roll.rs` + `lib.rs`，commit `425302d`）：用户实测发现"拖动创建音符后沿时间轴漂移"——旧手势 press-drag 是"创建+整体移动"，音符跟着指针跑。改为主流 DAW 约定：**按下处 note on、释放处 note off**（起点/音高固定在按下点，只有末尾跟随指针、吸附网格、最短一格、不超 loop）。新增 `RollIntent::ResizeEnd{index, length_ticks, gesture}` → `midi.note_edit` 合并手势；`gesture` id 让 AddNote + 各帧 ResizeEnd 共享一个 undo key（一次绘制=一步 undo，两次绘制互不合并）；`note_edit` 返回的新 index 经 `RollState::retarget` 回填选区（排序可能置换 index）。顺带补上两处死交互：空处单击=加默认长音符、音符上单击=选中（Shift/Ctrl toggle），此前只有 drag_started 分支、纯点击无响应。
      **Edit 标签页**：`MainView::Edit` 取代浮动编辑器窗口——双击 timeline clip 经 `set_edit_target`（清 roll 手势，旧选区 index 指向旧 clip）直接跳转 Edit tab；两个编辑器头部加 ←/→ clip 导航 + × Close；无目标时显示提示 + "编辑选中轨 clip 1" 按钮；删轨时清理指向被删轨的 edit target + `midi_tracks` 为空的防护。
      **顺带修复**：`apply_roll_intent` 的 AddNote 分支早退跳过 `send_track_clips`，新画的音符不进引擎、不发声，直到下一次无关编辑推 clip 表。
- [x] **M3.8 编辑手势完善：单击防误触 / 端点修剪 / 拖动复制**（2026-10-12，`glitcherz-ui/src/piano_roll.rs` + `lib.rs` + `glitcherz-command/src/midi.rs`）：用户实测 M3.7 后反馈"空白单击创建音符太容易误触（本想选择）"，并追问移动/端点编辑/拖动复制的顺手实现。参照主流 DAW 语义（Ableton/Bitwig/Cubase 综合，两项语义分叉经用户确认）：
      - **空白单击 = 取消选中**（不再创建）；**双击 = 插入默认长度音符**（保留快速插入路径）。egui 0.36 `drag_started()` 只在指针移动后才触发，click/drag/双击天然互斥、无双重触发。绘制仍靠 press-drag。
      - **端点修剪**：音符左右边缘 ±5px（`EDGE_PX`）内按下拖动 = 修剪该端（hover 显示 ↔ 光标）；抓取未选中音符的边缘 = 只修剪它，抓取已选中音符的边缘 = 整个选区一起拉伸。`RollIntent::ResizeStart/ResizeEnd{delta_ticks, gesture}` → 新命令 `midi.notes_edit_delta{indices, delta_start, delta_length}`（左缘 trim 传镜像 length delta，端点跟随；per-note 钳制 start≥0/length≥1）。
      - **拖动复制**：Ctrl+音符拖动 = 复制选区并拖动副本（Ctrl+单击仍是 toggle 加选——click 与 drag 由 egui 区分）。新命令 `midi.notes_duplicate` 返回副本新索引；复制与后续 move 帧共享 `roll-move:{t}:{c}:{g}` undo key（一次复制拖动=一步 undo）。
      - **选区重映射**：`notes_move`/`notes_edit_delta`/`notes_duplicate` 统一返回编辑过的音符排序后新索引（`locate()`：按值+已消费标记匹配，值相等孪音符可互换）；lib.rs `reselect_from` 用返回索引重设选区——修复了移动跨过未选中音符时排序置换、旧选区 index 指错音符的潜在 bug。MoveNotes 的 undo key 带 gesture id（每次拖动独立 undo 步），方向键 nudge 用共享 `roll-nudge:{t}:{c}` key（连续按键合并）。
      - **边界规则（用户确认）**：手势由按下位置决定，选区不改变空白语义——**移动必须抓在音符上**（选区内/未选中皆可，后者先选中它），空白拖动永远=画（无论有无选区）。有选区时空白拖动画出新音符并选中它，不算矛盾：抓取即意图声明，抓空=画一笔，Ctrl+Z 一步退掉。

### 明确不做（已评估）

- **Score Editor 五线谱**：soundcraft 自述"readable sketch rather than an engraving engine"，价值不如把piano roll 做扎实。
- **command journal / macro recording**：暂无 macro 需求。

### M1 顺带补AGENTS.md 实时铁律

1. **UI 层不得直接修改 model**，所有编辑经 `execute(cmd_id, params)`，以便统一撤销/回滚/审计。
2. **MIDI ops 纯函数化 + 确定性随机**：签名一律 `fn(&mut [Note], Options)`，不碰 clip/track/context，可脱离 DAW 单独测试。

## 新方向：M4 编辑交互模型（spec 已定稿）

2026-10-13 经三轮拷问式 Q&A 定稿，spec 见 `spec/editing-model.md`。核心决策：语义决策源从"指针落点 × 修饰键"（全手势）移到**离散状态**——常驻态只有 Select；D（Draw）为粘性命令（d 进入 / Esc 退出，输入爆发场景）；G/S/T/K/Shift+D 为一次性命令族（按键唤起、一次操作、Esc 取消、不改变常驻态）。关键设计：命令在 draw 态下同样生效（画完自动选中 → 直接 g/t 调整，1 次按键而非模式往返），消解 Ableton 式双态切换税。G 带 X/Y 轴锁定（X=只改时间 / Y=只改音高，mid-drag 可切）；T 默认拉终点、`t+s` 拉起点（统一规则：bare=最常见变体，限定键=次常见变体；`t+e` 与裸 T 重合已删）；S pivot=选区中心且不吸附；Shift+D=复制并移动（arm Grab）。时间轴砍掉 S（clip 是容器无音乐语义）与 D（clip 摆放低频，双击创建+拖缘调长已覆盖）。M3.8"空白拖动永远=画"边界规则废除，空拖回归框选。undo 合并全沿用 execute_merged + gesture id 基建，新增仅 Scale/Split 两族命令 + 状态机 + 视觉反馈。

- [x] **M4a 钢琴窗**：状态机（Select/Draw）+ G/S/T/Shift+D/K + 视觉反馈 + 映射矩阵测试（2026-10-13：命令层 `a02612e`；状态机+接线 `feat(roll)`。测试 75 项 glitcherz-ui 全绿，含 22 项映射矩阵集成测试）
- [x] **M4b 时间轴**（2026-10-13 交付，2026-10-14 对齐 roll：X/Y 轴锁、t+s 翻转、单次刀）：clip 跨轨多选（`ClipSel` + 框选合并 + Delete 批删一步 undo）+ G/T 模态（ghost 跨行预览、X 轴锁时间、落定 `filter_covered` 重叠策略）+ K 切刀（bar 吸附）+ Shift+D 复制模态（副本落定选区转移）。四段交付细节见 `implementation.md` M4b 段与 `spec/editing-model.md`
- [ ] **未来命令族扩展**：quantize / legato / glue / transpose = 同族零侵入

## 新方向：内置 SF2 音色库加载器（spec 已定稿）

2026-10-19 调研 crates.io 并定稿方案 A，spec 见 `spec/sf2-loader.md`。选型
`rustysynth-ext` 锁版本 `=1.4.0`（纯 Rust、零 unsafe、SF2+SF3、MIT；oxisynth
0.0.x 与 xsynth 生态不选，C 绑定类排除）。核心决策：加载/解析只在控制线程，
`Arc<SoundFont>` 交引擎；`VoiceSpec::SoundFont{path, bank, program}` 加法变体
（PROJECT_VERSION 不动）；**RT 安全先审计后接入**——rustysynth render 内部有
voice 分配，S1 spike 用分配计数确认行为，违反铁律则 fork 预分配 voice 池，
不允许带分配裸进音频回调。

- [x] S1 RT 安全审计 spike（分配计数 allocator + 源码审计，结论回写 spec）✅ 2026-10-19：`rustysynth-ext` 1.4.0 热路径（note_on/off、render、process_midi_message、reset、voice stealing、exclusive class）**零堆分配实证通过**——`VoiceCollection` 即预分配池（ext fork 相对上游的关键改进），**无需 fork**（D4 预案解除）。审计 harness `glitcherz-engine/tests/sf2_rt_audit.rs`：手建最小 SF2 字节流 fixture（无外部文件、常规 CI 可跑）9 场景 + 真实库 `#[ignore]` 审计（MuseScore_General 256 preset / FatBoy 全量逐 preset 计数，全 0 分配）。唯一 panic 点 = render 等长契约（调用侧保证）。结论与 S3 接入要点见 spec §7
- [x] S2 engine `sf2` 非实时模块（load / preset 枚举 / thiserror 错误）✅ 2026-10-19：`engine::sf2`（`Sf2Library::load` → `Arc<SoundFont>` + 加载期 preset 枚举 `Sf2Preset{bank: u8, program: u8, name}` + `find_preset`；`Sf2Error{Io, Parse, NoPresets}`；bank/program 超出 u8 的 preset 跳过 + warn）。rustysynth-ext 转正式依赖；S1 fixture 提取 `tests/common`（bank/program 参数化），`tests/sf2_load.rs` 8 场景 + 真实库（MuseScore_General/FatBoy，含 preset 地址唯一性）通过
- [x] S3 engine `realtime/sf2.rs` `Sf2VoiceProcessor`（`Processor` 实现，逐行铁律评审）✅ 2026-10-19：stereo render 缓冲构造期预分配（`SynthesizerSettings.block_size=128` 对齐）；`ParamId::{Sf2BankSelect, Sf2ProgramChange, Sf2MasterVolume}`（bank/program 经 `process_midi_message(0xB0/0xC0)` 零分配下发，master volume 纯写入）；`held: u128` 位图解决毯式 `Off { note: 0 }` 歧义（key 0 未按 = 全释放，按住 = pitch-matched）；velocity→MIDI 七位映射 + 越界 note 守卫（`1u128 << note` 需 <128）；`Sf2Error::Synth` 变体承接设置越界。`tests/sf2_voice.rs` 13 场景（构造、静音/NaN 铁律对、奇形缓冲、生命周期、velocity-0、毯式释放×2、参数路由、音量、bank/program 寻址、真实库 smoke）——**坑**：GM 默认 CC#91 reverb send=40 使被拒音符仍带 ~0.003 尾音，绝对阈值断言须改 baseline 比较
- [x] S4 model `VoiceSpec::SoundFont` 变体 + RON 往返测试 ✅ 2026-10-19：`SoundFont { path, bank: u8, program: u8 }`（serde default 加法字段，PROJECT_VERSION 不动；partial 加载 = 0:0）；`voice_spec_ron_roundtrip` 覆盖 + `voice_spec_soundfont_serde_compatibility`（tag `"sound_font"` + 无 bank/program 可加载）。**坑**：RON 紧凑输出无空格（`type:"sound_font"`，`contains("type: \"sound_font\"")` 会假失败）；serde default 是 `u8::default()` 而非实例值。UI 侧 8 处非穷尽 match 补分支：install_node Instrument+SoundFont 走完整加载（`Sf2Library::load` → `Sf2VoiceProcessor::new` → `set_param` 下发 bank/program，文件丢失/解析失败回落 SynthProcessor + log，D8）；Effect+SoundFont 回落 Delay（防御性，UI 不产生该组合）；`voice_plugin_key`/`track_external_fx` → None；preset load/reset 的 `&mut` match → `{}`（无 state blob）
- [x] S5 UI 音色库面板（加载 / preset 下拉 / 轨声源绑定，走守卫与命令约定）✅ 2026-10-19：Synth 面板 `SoundFont…` 按钮（rfd 对话框 filter sf2/sf3）→ 文档改 `VoiceSpec::SoundFont`（begin_edit/commit_edit，undoable）→ `load_soundfont_voice`（engine 替换 + preset 下发）；App 持有 `sf2_library: Option<Arc<Sf2Library>>` 会话单库（spec §6），`obtain_sf2_voice` path 匹配复用 / 未命中加载并缓存（install_node 重装同样复用，避免反复读大文件）；preset 下拉（ComboBox）从共享库枚举，列表快照释放借用后走 `set_soundfont_preset`（undoable 文档编辑 + `set_node_param` 即时下发）；当前声源为 SoundFont 时 `Built-in Synth` 不再高亮
- [x] S6 SF3 feature 开启 + 端到端验收 ✅ 2026-10-19：workspace `rustysynth-ext = { version = "=1.4.0", features = ["sf3"] }`（lewton 0.10 Vorbis 解码；新增依赖面 lewton/ogg/byteorder/tinyvec 全纯 Rust，无 C 构建链）；Vorbis 解码只发生在加载期（`SoundFont::new`，控制线程），RT 热路径代码不变。**S1 审计复跑通过**：fixture 9 场景 + 真实库（MuseScore_General 256 preset 逐计数）sf3 开启后仍 0 分配；真实库端到端验收（sf2_load 枚举 / sf2_rt_audit 零分配 / sf2_voice smoke）全绿。**SF2 方向 S1–S6 全部完成**

## 新方向：插件 GUI 宿主 + Patch 交互升级（spec 已定稿）

2026-10-20 经三轮 Q&A 定稿，spec 见 `spec/plugin-gui.md`。核心决策：CLAP 先行（Dexed 验收）+ VST3 `IPlugView` G7 镜像后补；嵌入式 child HWND 挂 Synth/Mixer 面板；Windows-first 留平台抽象层；gui 调用不过 StateGate（CLAP 契约 [main-thread] 与 process 并发安全由插件保证）；GUI 编辑不进 undo（preset save 捕获 state 覆盖持久化）；tab 切走 = hide、切轨 = destroy。**愿景扩展**（用户提出）：synth/mixer tab 从轨驱动重构为**选中节点驱动**（P5e 遗留收口，双击 patch 节点跳转）；patch 节点常显核心参数（plugdata 式注册表）；virtual keyboard 作为 patch 可添加的 UI 路由节点（选中时电脑键盘沿 Midi 连线弹目标 instrument，引擎零改动）。

- [x] G1 CLAP gui 宿主侧 ✅ 2026-10-20：workspace + test-plugin 开 `clack-extensions` "gui" feature；`GuiHostState`（resize_request 打包 u64 / show / hide / hints / closed 粘性）+ `HostGuiImpl` 挂 Shared handler + `declare_extensions` 注册 `HostGui`；guard `open_editor`（is_api_supported → create → set_scale → get_size → set_parent → show，失败 destroy 清理）/ `close_editor`（幂等，**destroy 即 `closed(true)` 的宿主确认**——CLAP 契约，spec 原文写反已修）/ `resize_editor`（adjust_size 协商）/ `hide_editor`/`show_editor` / `editor_events()`（take 语义）/ `gui_closed()`；`set_parent` 的 unsafe 收口 `raw::gui_set_parent`（模块 SAFETY 文档扩展）。test-plugin 假 GUI：观察状态放 `SineShared`（cdylib static 宿主读不到）经只读参数 ID 2 GuiCallMask（位图）/ ID 3 GuiDestroyCount（计数）暴露；set_parent→request_resize(240,160)、show→request_show、负 scale→closed(true)+Err（自毁模拟）。协议测试 6 项全绿（序列位图逐位、close 幂等计数、失败清理重开、回调到达、GuiNotOpen），`lists_plugin_params` 更新 3 参数，全 workspace 395 测试绿
- [x] G2 UI 嵌入 ✅ 2026-10-20（Dexed 实测通过）：`plugin_window` 模块（STATIC 子容器：宿主自建、不碰插件窗口，插件真窗口为孙窗口；Win32 unsafe 边界登记 AGENTS.md，crate `forbid`→`deny` + 模块 allow）；`main_window_handle`（eframe `winit_window` → rwh 0.6 Win32）；App 生命周期接线——`open_plugin_editor`（ui() 尾部处理面板请求）/`close_plugin_editor`（先 gui.destroy 后容器 drop，§4.3 序）/`retire_guard` 前置关闭 / `impl Drop for GlitcherZApp` 保证退出序；synth 面板 Show/Close Editor 按钮 + 占位区（rect 驱动容器）；每帧 `editor_events` 轮询（resize→size 更新）+ `gui_closed` 确认；tab 切走 hide（D9）。**实测修 bug**：① 容器去 `WS_DISABLED`（禁用父窗口阻断子树鼠标命中与键盘焦点）② reposition 去抖（同 rect 不再 SetWindowPos，消除 50ms WM_WINDOWPOSCHANGED 干扰插件拖拽；hide 后强制重放）③ 编辑器区移出 ScrollArea（原生子窗口不受 egui 裁剪，滚动残影）。Win32 容器往返测试 1 项，全绿
- [x] G3 参数回接 ✅ 2026-10-20：`ParamsHostState`（`request_flush` 原子标志）+ `ClapInstanceGuard::poll_param_flush`（take 语义；标志置位时 raw vtable 直调 `clap_plugin_params::flush`，空 in / 栈上 out queue，解析 `ParamValueEvent` 回读 `(id, value)`）→ UI 帧尾部编辑器开着时轮询（`gui_closed` 的 else 分支，避免自毁边角）→ `clap_voice_values` 滑块缓存同步 + `send_track_param(ParamId::PluginParam)` 引擎下发；GUI 编辑不进 undo（D8）。**实施发现（spec §5 修正）**：clack 的 `PluginParams::flush` 包装要 `InactivePluginMainThreadHandle`、`flush_active` 要 processor 独占 handle——processor 已移交音频线程，两条路都走不通，须 raw 直调（`raw::flush_params`，与 `enumerate_params` 同模式，unsafe 边界已入模块 SAFETY 文档）；flush 不过 StateGate（D7 同款裁决：[main-thread] 调用，request_flush 的标准宿主响应）。test-plugin 钩子：`hide` 模拟 GUI 内改参数（`set_amplitude_from_gui` 置 dirty + `request_host_flush`），**audio 与 main-thread 两个 flush 实现都要写 out queue**（clack wrapper 对活跃实例分派给 audio 侧实现——第一次只改 main-thread 版本测试失败即此因）。测试 2 项：协议往返 + take 语义（`param_flush_reads_back_gui_changes`）、回读值走 `PluginParam` 桥影响引擎输出（`param_flush_value_feeds_engine_bridge`）。全仓 398 测试绿
- [x] G4 面板按选中节点重构 ✅ 2026-10-20（commit bb9d3de）：patch 双击节点 → `OpenNode` action 跳 Synth tab + `selected_doc_nodes()`；`resolve_panel_target` 纯函数（文档序首个可编辑节点，无选中回落 selected_track 乐器，单测 5 例）；六个乐器会话缓存键 track→doc node_id（与效果器缓存对齐），`delete_graph_node`/`remove_last_midi_track` 清理路径同步；synth 面板按 `NodeDoc` 变体分派（Instrument=完整声源栈+编辑器区 / Effect=该节点插件参数+delay 滑条 / Mixer=控制台），滑条下发 `send_node_param`（SetNodeParam）；`adsr_sliders`/`delay_sliders` 收 doc_id；效果器加载挂 `node_track(instr_id)`（跨轨目标正确）；`PluginEditor.doc_node` 化 + `editor_user_open` 会话开关（目标切换销毁重开）+ flush 回接 node 键；automation 选项列表（auto_options/automation_section）node 键化。回归：单轨无选中行为不变
- [x] G5 节点内联核心参数 ✅ 2026-10-20：静态注册表 `SYNTH_INLINE`(cut/res)/`DELAY_INLINE`(time/fdbk/mix)/`MIXER_INLINE`(pan)（SoundFont/legacy 零行，插件节点走 `PatchDeps.plugin_inline` 闭包取前 2 枚举参数，CLAP 原始域 / VST3 归一化域与面板滑条同源）；`InlineParam`/`InlineParamId`/`InlinePluginControl` 公开类型 + `PatchAction::SetInlineParam`；`size_of` 高度公式 = 现有 base（端口自适应）+ `inline_row_count × INLINE_ROW_HEIGHT(16)`；patch 几何方法签名统一 `(rect_min, id, deps: &PatchDeps)`；show() 尾部节点 body 上叠真实 `egui::Slider`（`ui.put`，后层赢得指针→拖滑条不改节点位置）；值路径 `apply_inline_param`：内建项文档字段写 + undo 合并键与面板共享（synth-cutoff/synth-res/delay-params/mixer-out-pan）+ `inline_param_id` 映射 `send_node_param` 非重建下发，插件项同步 `clap/vst3_voice_values` 会话缓存（面板/内联互换）；`inline_plugin_label` 6 字符短标签。测试 5 项：注册表变体覆盖 / 高度公式（4 节点型 + 固定实体）/ `inline_doc_value` 文档读取与面板范围镜像 / `inline_param_id`↔ParamId 全表映射 / plugin 直通。全仓 fmt+clippy+test 绿
- [x] G6 vk 节点（`NodeDoc::Keyboard` + 右键菜单 + `keyboard_route` 纯函数路由，未选 vk 零行为差异）✅ 2026-10-20（commit 470954c）：model `NodeDoc::Keyboard{node_id, track}`（文档唯一**无 RT 对应**的节点变体，serde 加法 / PROJECT_VERSION 不动）+ `GraphDoc::keyboard_route(selection)`（沿 Midi 连线 BFS，取**文档序首个** instrument，非 instrument 终点不穿透）+ validate 新规则（MIDI 源 = `MidiTrack(_)` | Keyboard 节点；audio 连线两端禁止 vk，vk 无音频口）。引擎 `EngineCommand::NoteAtNode` + `RtGraph::node_note(node_id)`（slot 线性扫描、零分配、未知 id 静默丢弃），vk 不建 processor、不占 slot（`install_node` 早退，`node_map` 无条目）。UI：patch 右键 `Virtual Keyboard`、vk 单 MIDI 出端口（绿色）/ 零入端口 / 迷你键盘条 / 高度 56；从 vk 出端口拖到 instrument 发 `ConnectKeyboardMidi`（**文档-only** 连线，单目标替换，无引擎调用）；app `vk_target` 每帧由选中集刷新（路由切换先按**旧**目标释放挂音，防止挂死音），键盘标题显示 "vk → Track N"；删节点/删轨路径对无引擎对应节点补守卫。测试 10 项（model 6 / engine 3 / patch 1）。**2026-10-22 已被下方「G6 收口」取代**：路由不再看选中集（改按 `source` 绑定）、可多出线、无轨道 fallback、屏幕键盘退役
- [x] G7 VST3 `IPlugView` 镜像（`HostPlugFrame` + `createView`/`attached` + `performEdit` 回接 + 宿主滑块 beginEdit 三段）✅ 2026-10-21（commit 63b0920）：新模块 `vst3-host/gui.rs`（`HostPlugFrame::resizeView` 原子打包请求、`HostComponentHandler` = `IComponentHandler`+`IComponentHandler2` 的 `performEdit` → 128 槽无锁 `EditQueue`（GUI/音频线程都可能写）、`GuiHostState` take 语义）；`NoopComponentHandler` 删除（空 `performEdit` 正是 G7 的缺口）。guard 加 `view`/`editing`/`gui`/`edits`/`plug_frame` 字段，`open_editor` = createView→setFrame→可选 `setContentScaleFactor`→platform 检查→attached→getSize（每步失败都撤销 attach），`close_editor` = removed+setFrame(null)+release，`tear_down` 先关视图。**两处 spec 方向修正**：① `resizeView` 只记录请求，UI reposition 容器之后才 `onSize` 确认（`PluginEditor.pending_ack`）；② 宿主滑块→GUI 是 `IEditControllerHostEditing::{beginEditFromHost,setParamNormalized,endEditFromHost}`，`beginEdit/performEdit/endEdit` 属插件→宿主方向。UI 复用地板不动：`PluginGuard` 分派 open/close，帧尾按格式分支轮询（CLAP flush / VST3 drain），`GuiParamBatch{is_clap}` 选缓存，`sync_vst3_host_param` 接乐器面板/效果面板/内联参数三处，`node_guard_is_clap` → `node_guard_has_editor`。测试插件加只读探针参数 id 2 `GuiCalls`（掩码/1024）+ 无窗口假 `SinePlugView`（attached 时发 resizeView 320×240 与 performEdit(1,0.75)）+ `IEditControllerHostEditing` 探针位。测试 6 项（gui 3 / host 3），全仓 fmt+clippy+test 绿

## 新方向：可用性提升（U 系列，2026-10-21 立项）

骨架功能已齐（路线图 1–10、节点图 P、MIDI M/S、插件 GUI G 全交付），转向可用性。对照 LMMS / Ableton / FL 盘点走带、文件管理、轨道类型、交付输出的缺口，经用户取舍确认。**评估后未纳入**：全局 Mixer 独立视图、track header solo/volume/pan 增强——现有 mixer 节点控制台 + 轨头 mute 暂够用，待痛点实证再议。

核心依赖判断：

- **seek 基建先行**（U1）：引擎 `SequencerCommand` 无 Seek/Pause、播放头只读，是暂停 / 跳标记 / loop 区 / space 键的共同前置
- **拍号文档化宜早**（U3）：`TICKS_PER_BAR` 常量 → 文档派生，波及 timeline 网格 / grid / SMF / sequencer，越晚做撞得越多；`time_signature` 走 serde default 故**不动文件版本**（2026-10-22 修正：原判"breaking 迁移 v6→v7"是错的）
- **wav 基建一份两用**（U5→U6）：采样播放器与 audio track 共用解码 + `Arc` 缓存层
- **控制轨现状澄清**：`ControlTrack`（自动化轨）已覆盖"画曲线 + patch 连线 + timeline 控制"（P3/P5c 交付）；U4 补的是 **CC 输出模式**与曲线编辑**一等化进 timeline 视图**（现藏于 Synth/Mixer tab 底部 automation_section）

- [x] **U1 transport 完整化 + seek 基建** ✅ 2026-10-22：引擎 `SequencerCommand::{Seek, Pause, SetLoopRange}` + `play()` 语义改**从当前位置续播**（stop 回零 / pause 保位 / seek 清 pending offs），`advance()` 重写为**分段调度**——块跨回卷点时同时调度尾段 `[t0, loop_end)` 与回卷头段 `[loop_start, …)`，loop 激活时抑制 D16 自停；pending off 按样本钟释放（连续跨回卷，长音自然全长释放，无需环绕映射——想当然的 wrap 映射会把 off 提早到错误的块，实测推翻后删除）；AudioEngine 薄包装 `pause/seek_ticks/set_loop_range`。model `ProjectMetadata` 加 `loop_range: SongLoop{enabled,start,end}`（serde default，版本不动）+ `markers: Vec<Marker>`（上限 32）+ validate。UI：transport 按钮组 ⏮/▶⏸/■/⏭/Loop/‹›（marker 跳转，无 marker 时 ‹=回零 ⏭=曲尾）；**space 全局 play/pause** + Home/End（`egui_wants_keyboard_input` 守卫防劫持文本编辑）；timeline ruler 交互化——裸点击/拖=seek scrub、**Ctrl+拖=拉选 song loop**（bar 吸附，ghost 预览，一次拖=一步 undo）、拖 loop 条边缘=单端重塑、右键=加 marker（bar 吸附，自动名 M<n>；右键 marker 旗标=删除）；ruler 画 loop brace（enabled 实心琥珀 / disabled 轮廓）、marker 旗标+名、playhead 竖线；undo/redo 与 do_open 经 `apply_document`/显式调用重推 loop range。测试：引擎 7 新增（续播/pause/seek 定位与清尾音/loop 回卷重复+抑制自停/跨回卷长音全长/退化区间禁用）+ model 3（loop+marker 往返、旧文档默认）；sequencer 两个旧测试补显式 `seek(0)`（play 语义变化）。全仓 fmt/clippy/test 绿（449 项）
- [x] **U2 文件管理完整化** ✅ 2026-10-22：新模块 `project_files.rs`（纯函数 + 文件 IO，可脱离 egui 测试）。**File 菜单**取代走带条上的散按钮（New / Open… / Open Recent ▸ / Save / Save As… / Import MIDI… / Export MIDI…，快捷键 Ctrl+N / Ctrl+O / Ctrl+S / Ctrl+Shift+S）；**New** = `Project::default()`（4 条空轨 + 默认图）+ 会话状态复位（roll/timeline/lane/patch、edit target、插件编辑器、held notes）→ `push_all_params()` + `push_loop_range()` 重建引擎。**SMF 接线**（`smf.rs` 库层此前零调用）：Export 走 `export_smf` + 默认文件名取工程 stem（`Untitled.mid`）；Import **先填空轨再追加**（`plan_import`：空轨（无音符且无 CC）按序复用，超出者按 `MAX_MIDI_TRACKS=16` 追加，容量外丢弃并在状态栏报数），整次导入 = **一步 undo**（外层 `suspend_history` 吃掉 `add_midi_track` 的嵌套提交）。**dirty flag 接线**：`Session::is_dirty` 此前形同虚设——tempo / master gain 的 `DragValue`/`Slider` 每帧 `project_mut()` 写入、`control_editor_body` 每帧 `project_mut()` 读 `auto_endpoint`，**保存后立刻标脏**；三处改为「读局部变量、`changed` 才写」+ 读取走 `project()`，dirty 才可信。标题每帧由 `window_title()` 计算（`GlitcherZ — name[*]`，未保存为 `Untitled`），仅文本变化时下发 viewport command。**未保存确认**：`FileAction{New,Open,Exit}` 经 `request_file_action` 拦截，dirty 时弹 `egui::Modal`（Save / Discard changes / Cancel；保存失败或取消保存 → 对话框保留、动作不执行）；关窗走 `close_requested` → `CancelClose` → 答复后回放（`allow_close`），auto-close 冒烟路径绕过确认。测试 8 项（标题 4 态 / 导出名 / 空轨判定 / 导入落位 3 例 / SMF 文件往返）。**遗留**：Export Audio 归 U7；undo 退回已保存状态仍显示 `*`（需内容比对才能清，按主流 DAW 行为接受）
- [x] **U3a 拍号文档化** ✅ 2026-10-22：model `TimeSignature{numerator, denominator}`（`COMMON=4/4`，`new()` 钳制 / `is_valid()` 严格，serde `default` → 旧文档读出 4/4，**PROJECT_VERSION 仍 6**）+ `ProjectMetadata.time_signature` + 派生 `ticks_per_bar()` / `ticks_per_beat()` + `Project::ticks_per_bar()` + validate 范围检查。engine 镜像 `ticks_per_bar_of()`（crate 间不互相依赖，`tick_base_consistency` 锁两处公式）+ `Sequencer::bar_ticks` 字段 + `SequencerCommand::SetTimeSignature` + `AudioEngine::set_time_signature`；`refresh_arrangement_end` 改用当前拍号取整，**改拍号不动 transport 位置**。SMF 导出写真实 `FF 58`（`dd = log2(denominator)`）；`import_smf` 返回 `SmfImport{tracks, time_signature}`（首个 `FF 58` 胜出）——**导入策略**：空白工程（`project_files::is_unwritten`，无音符无控制 clip）视为"打开"，继承文件拍号；已有内容则保留自身拍号并在状态栏报差异（不让一次导入偷偷重划网格）。UI：走带拍号下拉（4/4 3/4 2/4 5/4 6/8 7/8，一步 undo、不移动任何事件）；timeline 网格 / ruler 标号 / loop 吸附 / 刀切最小长度 / 缩放锚点 / 新建 clip 默认 1 小节 / piano roll 小节线与 beat 次分组 / 走带 `Pos bar.beat`（beat 单位 = 分母音符）/ skip-to-end 取整，全部改读派生值；排列上限仍是**绝对 tick** 边界（`MAX_ARRANGEMENT_TICKS`，4/4 下 256 小节），但视图宽度按 `ceil(limit/bar_ticks)` 铺满。测试：model 3 + file 2（往返 / 旧文档默认）+ engine 2（取整随拍号 / 钳制）+ smf 2（`FF 58` 往返 / 无拍号报 `None`）+ ui 2（6/8 吸附与刀切 / `is_unwritten`），`tick_base_consistency` 增加拍号公式对锁
- [x] **U3b tempo 曲线** ✅ 2026-10-22（spec `.agents/spec/tempo-meter.md` D1–D9）：固定端点 `PortRef::TimelineIn`（与 `Master`/`Timeline` 同级的纯汇，无 node_id、不进 `graph.nodes`、无 processor；单例由"不可实例化"保证，`validate` 只约束连线——必须是 `Automation`、`param` 必须是 `Tempo`、反向 `Tempo → 节点` 拒绝）+ `AutoParam::Tempo` + `CableDoc::tempo()` + `LayoutDoc.timeline_in`。引擎编译器写**哨兵路由** `AutoRoute.slot = TIMELINE_IN`（`u16::MAX-1`，与 `NO_EDGE` 区分，永不落进 `slots`）；`RtGraph::auto_route()` 只读出口让 block runner 分辨节点/时钟目标；`Sequencer` 拆 `base_bpm` + `bpm_override`（`effective_bpm()` 统一钳制，`play`/`stop`/`pause` 清覆盖、`seek` 不清）。**`evaluate_automation` 前移到 `advance()` 之前**（D4：否则每块都用上一块的 tempo），每块先清覆盖 → 曲线空隙自动回落 base（D3，与普通参数"保持最后值"有意相反）。UI：patch 固定 box（1 入 0 出，`Tempo` 端口标注）+ 拖线直连 `ConnectTempoIn`（跳过参数弹窗）+ `accepts` 只放行 Automation；Synth 面板 `Pick Parameter… → Song Tempo (BPM)`；走带 BPM 旁播放中显示 `→ x.x auto`（`Feedback.bpm` 原子）；曲线纵轴 20..300 对数轴。`param_id_of` 改为 `Option<ParamId>`（Tempo 无 ParamId，顶住"顺手接到 set_param"）。**tempo 不是环**，是自指时钟（tick 域采样的前向欧拉积分），不需 Feedback 节点。测试 +7（model 2 / engine 4 / patch 1），全仓 471 项绿。已知取舍：默认工程仍 0 控制轨，新工程需先 `+ Add` 才有可拖的 A 出口（留给 U4）
- [x] **CI / 发布流水线** ✅ 2026-10-22：`.github/workflows/ci.yml`（push 任意分支 + PR 到 main：`fmt --check` / `clippy --workspace --all-targets -D warnings` / 三平台 `build + test`，纯文档路径跳过 push 触发）与 `.github/workflows/release.yml`（push `v*` tag：preflight 校验 tag 与 workspace 版本一致并跑 fmt/clippy/test → Windows x86_64 / macOS universal（lipo 合并 arm64+intel）/ Linux x86_64（ubuntu-22.04 构建换 glibc 2.35 兼容）→ 单点 `publish` 上传归档 + 合并 `sha256sums.txt`，release body 由上一 tag 以来的 commit 生成）。Linux 系统依赖抽成 composite action `.github/actions/install-linux-deps`（ALSA + X11/Wayland/xkbcommon + GL）。测试 job 先 `cargo build --workspace`：CLAP/VST3 host 单测从 `target/debug` dlopen fixture cdylib
- [x] **G6 收口：键盘输入端口（A 设计）** ✅ 2026-10-22（spec `plugin-gui.md` §8）：`NodeDoc::Keyboard{node_id, track, source}` 绑一个 `MidiSource`（`Computer` / `Device(name)` 占位），`validate` 强制**一源一节点**；键盘事件只沿该节点出线走，**删掉轨道 fallback**（无节点/无连线 = 静音 + 状态栏 `Keys → none`）；默认工程带一个 `Computer` 节点连到 Track 1 instrument（开箱可弹，但那条线可见可删）；路由改为 `computer_keyboard()` + `keyboard_routes_from(vk)`（不再看选中集）；`Ctrl+L` 把选中 instrument 接到/断开电脑键盘节点（快速试听）；删除 synth/mixer tab 的屏幕键盘区（`keyboard_section` 整段）。无迁移负担（用户确认不考虑旧文件）。测试 +2（默认工程节点/连线、源唯一性）
- [ ] **U4 控制轨 CC 输出 + timeline 一等化**：自动化电缆新增 MIDI CC 目标模式（曲线值 → 7bit CC 事件，注入插件 MIDI 输入流，覆盖只认 CC 的插件）；曲线编辑 lane 从 Synth/Mixer tab 搬进 timeline 视图（选中控制轨时显示，LMMS song editor 语义）
- [ ] **U5 采样播放器 instrument**（spec 先行，RT 审计同 S1 流程）：wav 解码（hound，控制线程）+ 会话 SampleLibrary 缓存（`Arc` 共享 f32 立体声缓冲）；`VoiceSpec::Sample { path, root_note, … }` + `SampleVoiceProcessor`（RT 预分配 voice 池、零分配审计）；note-on 触发样本播放 + root note 音高映射
- [ ] **U6 audio track**（依赖 U5，spec 先行）：`AudioTrack` 文档轨 + `AudioClip { path, span, offset, gain }`；`RtAudioClip` 从预加载缓冲按时间轴播放入图混音；timeline 波形渲染；无 pitch/音符语义（区别于 U5 乐器化采样）
- [ ] **U7 Export Audio + 电平表升级**：离线渲染 wav（控制线程 block 循环驱动引擎图，绕开 cpal 实时流，hound 写文件）；master 电平表立体声双条 + dB 刻度（-60..0，替换单条峰值）
- [ ] **U8 Synth/Mixer tab → Inspector 重构**：改名 Inspector（G4 按选中节点分派已就位，此步收口堆叠杂项）：automation lane 挪 timeline（U4 交付）、master 电平表挪位、clip_section 死文案清除；Instrument 面板收敛为 preset + 参数 + 插件编辑器区（**源选择已迁出**，见下方 U9 / `spec/plugin-browser.md`）

实施顺序 U1 → U2 → U3 → U4 → U5 → U6 → U7 → U8（U4 无重依赖可前插；U5/U6 音频链条工作量大；U7 在音频能力齐后覆盖全轨渲染；U8 收口）。

## 新方向：插件浏览器与添加节点菜单（U9，2026-10-22 立项，spec 已定稿）

痛点实证（用户 2026-10-22 提出"过多插件右键菜单太长很影响体验，Blender/ComfyUI 都是多级菜单 + 搜索过滤框"）。spec 见 `spec/plugin-browser.md`。核心决策：**类型前置**（Add 第一层选 Instrument/Effect，节点带最终声源出生，只 install 一次、一步 undo）；**多级菜单 + 过滤框**（Blender `Add` 菜单 / ComfyUI 搜索语义，叶子层不是裸 submenu）；**启动时一次扫描 + 缓存 + 进度条，前台分片推进**（每帧末尾扫一个模块；主线程扫描免 COM 初始化，键 `(path, mtime, size)`，一趟扫描四视图分流，消灭现在 4 次重复全量）；**职责切分**（结构操作归 patch、参数编辑归 Inspector，**不提供换源动作**——换声源 = 删了重建，参数不迁移）。不动引擎、不动文档版本。

- [x] **E1 目录与缓存内核** ✅ 2026-10-22：`glitcherz-ui/src/plugin_catalog.rs`（`PluginFormat`/`PluginEntry`/`PluginCache`/`CandidateFile::stat`/`collect_from_disk`/`plan_refresh`/`merge`+`sort_entries`/`view`/`encode`+`decode`），纯函数 8 项单测；ui crate 增 `ron` 依赖。全仓 482 测试绿
- [x] **E2 扫描接线** ✅ 2026-10-22：`PluginCatalog` 状态机 + 帧尾单模块推进（`upcoming` 文案 = 队首，挂住时可定位）+ 顶部进度条 + `storage.set_string("plugin_cache")` 持久化 + patch 右键 `Rescan Plug-ins`；四个惰性扫描缓存删除。全仓 484 测试绿，热启动零 dylib 加载实测通过
- [x] **E3 picker 与菜单** ✅ 2026-10-23：`plugin_picker.rs` 浮层（`PickFilter` + 过滤框 + ↑↓/Enter/Esc + Recent 分组 + `Show all plug-ins…`）+ `Add` 三级菜单 + 全局 `Search Plug-ins…`；同批落地 `NodeCategory` / `NodeSource`（取代 `AddNodeKind`）与 `create_patch_node(source)` 单次 install（插件装不起来即回滚，不留空壳）；Recent 存 `UiState` 跨会话。8 项单测（4 项 headless egui 帧驱动），全仓 492 测试绿
- [x] **E4 连线中点 Insert 复用 picker** ✅ 2026-10-23：cable 中点菜单改 `Insert ▸ Effect ▸ Built-in Delay / CLAP… / VST3…` + `Insert ▸ Mixer`；`PickerPlace::Insert` 携带 cable 下标复用同一 popup；`insert_node_in_cable` 吃插件效果器、按上游节点定轨（`create_patch_node_on`，修掉原先误用选中轨）、`suspend_history` 包住三步使整次拼接一步 undo；`splice_endpoints` 守住下游 `in_port` 与线型校验。5 项单测，全仓 497 测试绿
- [x] **E5 Inspector 收口** ✅ 2026-10-23：删乐器源选择行（`Built-in Synth` / `CLAP Instruments…` / `VST3 Instruments…` / `SoundFont…`）与 `CLAP Effects…` / `VST3 Effects…` 快捷加载；`Clear All Effects` 挪到效果节点列表头部（窄化到单个效果节点时隐藏）；SoundFont preset 下拉保留（preset ≠ 源）。连带删除"换源"时代的死代码 `set_builtin_instrument` / `load_*_voice` / `load_*_effect` / `insert_effect_common` / `plugin_entries` / `plugin_empty_hint` / `node_track`（净减 549 行），`install_node` 成为唯一装配入口。全仓 497 测试绿 + 5 秒冒烟通过

与 U 系列的关系：E1–E5 建议**先于 U8** 落地（U8 的"源选择"项由本 spec 接管，E5 与 U8 剩余项可合并收口）。

## 附：VST3 调研结论（2026-10-07，已实施）

crates.io 上 `vst3` crate（0.3.0）为可用起点。VST3 宿主比 CLAP 复杂（预估 2-3 倍工作量），差异点：

1. **模块扫描**：Windows 下 `.vst3` 是目录 bundle（`Contents/x86_64-win/*.vst3` 内嵌 dll），需按规范遍历目录结构；
2. **COM**：插件对象基于 COM 生命周期管理（引用计数 / QueryInterface），宿主需处理 COM 初始化；
3. **双组件**：`IComponent`（音频处理）与 `IEditController`（参数/编辑器）分离，宿主负责连接两者；
4. **参数/事件桥接**：`ParameterChanges`/`EventList` 语义与 CLAP 类似但结构不同，复用 `ParamId::PluginParam(u32)` 空间与结构更新通道。

（实际落地：`glitcherz-vst3-host` 镜像 clap-host 架构，见 `implementation.md`。）
