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

## 当前状态（2026-10-15）

路线图 1–10 全部落地。乐器插件（CLAP / VST3）为一等声源，效果器节点已接入，自动化桥接到插件参数。节点图 P1–P5e.2 与 MIDI 编辑 M1–M4（M4a 钢琴窗、M4b 时间轴）全部交付；CLAP 乐器参数滑块已接通（2026-10-15）。下一步：P5f 单输入语义 + Mixer 节点。

## 遗留任务

- [x] CLAP 插件参数**枚举**（2026-10-08 完成：`ClapInstance::list_params` 经 params 扩展 vtable 直调（`raw::enumerate_params`，clack 0.2 包装无读取方法）；测试插件实现 Amplitude 参数；UI 参数面板 + 自动化目标选择器接入 `clap_voice_params`）
- [x] CLAP 乐器参数**滑块面板**（2026-10-15：`clap_voice_values` 会话内当前值 map（track → param id → f64 原始值域），滑块变更经 `send_track_param(ParamId::PluginParam)` 直达 `ClapNode`；重载插件/切回内置声源/文档重应用时重置为插件默认值；state 持久化落地后（见下），重载路径经 `load_state` + `read_param_values` 重播种滑块缓存。VST3 滑块已接（归一化 0..1，见下）；Dexed 兼容性复现测试见 `glitcherz-clap-host` 的 `dexed_instrument_note_on_produces_audio`）
- [x] 插件状态保存/加载 **CLAP 部分**（2026-10-15，spec `spec/plugin-state.md` D1–D7 + S1–S4：`VoiceSpec` 加 `state: Vec<u8>`（serde default，PROJECT_VERSION 不动）；clack state 扩展宿主捕获 + `ClapInstanceGuard::{save_state, load_state}` + raw `get_value` 滑块回填；do_save 采集 / install_node 恢复；测试插件 amplitude 往返测试。VST3 FState 镜像 = S5，见下条）
- [x] CLAP / VST3 效果器节点参数桥接（2026-10-15：效果器参数缓存以**文档 node_id** 为键（跨引擎重建稳定），UI 面板经 `set_node_param(engine_id, ParamId::PluginParam)` 节点寻址直达效果器节点——"参数空间区分"由图节点寻址自然消解；CLAP 显示原始值域、VST3 显示归一化 0..1（与乐器同split）；live 插入与文档重建（install_node）双路径填充缓存，删除节点/清空效果器/整批重建三处清理；自动化目标选择器接入效果器参数（端点 = 效果器 doc_id，曲线值域随格式））
- [x] VST3 参数归一化换算的 UI 层处理（2026-10-15：v1 决策——滑块直接工作在归一化 0..1 域，不做 plain↔normalized 换算（VST3 ParameterInfo 不携带 plain 值域，换算需逐参调控制器；归一化滑块诚实且够用）；当前值经 `read_param_values`（S5 落地的 `getParamNormalized` 读回）播种会话缓存，CLAP `clap_voice_values` 的镜像 `vst3_voice_values`；自动化曲线值沿用归一化域）
- [x] 插件状态保存/加载 **VST3 部分**（2026-10-15，S5 镜像 clap-host：`HostMemoryStream`（`IBStream` 内存流 COM 对象，host_impl）+ `Vst3InstanceGuard::{save_state, load_state, read_param_values}`——save 走组件 `getState`，load = 组件 `setState` + 控制器 `setComponentState` 同步（各给一条独立流）；测试插件工厂改**逐实例创建**（原 OnceLock 单例与 kManyInstances 相悖且无法测"新实例回落默认值"），SineComponent 以 8 字节 LE f64 持久化 Gain（经 `IParameterChanges` 消费、作用于振幅）、控制器镜像读回；UI install_node 两处恢复 + capture 采集 + 文档写回；端到端 `state_roundtrip_restores_gain`。`read_param_values` 为归一化滑块（下一条）预留）
- [ ] VST3 参数归一化换算的 UI 层处理（宿主用归一化 0..1，CLAP 用原始值；见 `implementation.md`）

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
- [ ] P6 音频反馈环路（可选，1 块延迟节点）

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

## 附：VST3 调研结论（2026-10-07，已实施）

crates.io 上 `vst3` crate（0.3.0）为可用起点。VST3 宿主比 CLAP 复杂（预估 2-3 倍工作量），差异点：

1. **模块扫描**：Windows 下 `.vst3` 是目录 bundle（`Contents/x86_64-win/*.vst3` 内嵌 dll），需按规范遍历目录结构；
2. **COM**：插件对象基于 COM 生命周期管理（引用计数 / QueryInterface），宿主需处理 COM 初始化；
3. **双组件**：`IComponent`（音频处理）与 `IEditController`（参数/编辑器）分离，宿主负责连接两者；
4. **参数/事件桥接**：`ParameterChanges`/`EventList` 语义与 CLAP 类似但结构不同，复用 `ParamId::PluginParam(u32)` 空间与结构更新通道。

（实际落地：`glitcherz-vst3-host` 镜像 clap-host 架构，见 `implementation.md`。）
