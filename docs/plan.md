# 计划与状态 — ResoZ

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

## 当前状态（2026-10-07）

路线图 1–10 全部落地。乐器插件（CLAP / VST3）为一等声源，效果器节点已接入，自动化桥接到插件参数。剩余为插件生态收尾项与新架构方向。

## 遗留任务

- [x] CLAP 插件参数**枚举**（2026-10-08 完成：`ClapInstance::list_params` 经 params 扩展 vtable 直调（`raw::enumerate_params`，clack 0.2 包装无读取方法）；测试插件实现 Amplitude 参数；UI 参数面板 + 自动化目标选择器接入 `clap_voice_params`）
- [ ] CLAP / VST3 效果器节点参数桥接（需处理图节点 ID 引入，区分声源/效果器参数空间）
- [ ] 插件状态保存/加载（CLAP state extension / VST3 FState → ProjectFile）
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
- [x] P5d.1 画布交互打磨（2026-10-09：左键点选/框选（Shift 追加多选，多选节点整体拖动，Delete 批量删除——先按降序删连线再删节点防索引漂移）、中键平移、滚轮/Ctrl+滚轮缩放（指针锚点）；旧"拖背景平移"移除。修复两项：① `screen_rect` 统一乘 zoom，绘制/端口/命中单一几何源（此前端口与框选在 zoom≠1 时漂移）；② egui 0.36 对 click+drag sense 做**点击/拖拽判别**，原地按下不触发 `drag_started`，点选补走 `clicked()` 路径。新增 `resoz-ui/tests/egui_event_semantics.rs` 锁定该事件语义防 egui 升级回归）
- [x] P5d.2 缩放初值修复 + 改名 Patch（2026-10-09：① `PatchState` 曾 `#[derive(Default)]`，`zoom: f32` 初值 0.0 → 整图坍缩到左上角、点 Reset 才恢复，改为手写 `Default`（zoom=1.0）+ 回归测试 `default_zoom_is_one_not_zero`；② **canvas → patch 全量改名**：Pure Data 语境下连线图叫 patch，`canvas.rs` → `patch.rs`，`CanvasId/CanvasAction/CanvasDeps/CanvasState` → `PatchId/PatchAction/PatchDeps/PatchState`，`MainView::Canvas` → `MainView::Patch`，Tab 标签 "Canvas" → "Patch"）
- [x] P5e Patch 自由加删节点（2026-10-09：① 右键 context menu 添加 Instrument / Effect (Delay) / Channel Strip（`AddNodeKind` + `PatchAction::AddNode{kind, at}`；新节点悬空出生 D6、归属当前选中轨、位置落在点击处——文档 push → 引擎 `install_node` → `PatchState::place_node`，引擎安装失败回滚文档，节点表满 64 拒绝）；② `DeleteNode` 放宽到全部文档节点（master/timeline 固定不可删）：引擎 bypass 语义不变，MIDI/自动化触及连线直接移除，删除后刷新所属轨寻址表；③ 抽取 `register_track_chain(track)` 助手（apply_document 与加/删节点共用）。回归测试：place_node 位置不被 ensure_positions 覆盖。遗留：面板仍是"轨→first-match 节点"驱动，多乐器/多 strip 轨的面板寻址与 `track` 字段弱化待后续）
- [x] P5e.1 节点位置持久化（2026-10-09：`GraphDoc.layout: LayoutDoc{nodes: BTreeMap<u64, [f32; 2]>, timeline, master}`——`#[serde(default)]` 旧文件无感兼容，BTreeMap 保证序列化稳定，model 不依赖 egui；`PatchState::export_layout`（保存前导出，剔除文档中已不存在的 id）/ `import_layout`（加载后导入，缺项回落自动布点）；`do_save` / `do_open` 接线。demo_project 携带 layout 进入 roundtrip 覆盖；patch.rs 增 export 过滤 stale id + 往返回归测试。168 测试绿）
- [x] P5e.2 连线交互打磨（2026-10-09：① **端口抓取热区** `PORT_HIT_RADIUS` 12→18，拉线起点更好抓；② **Ctrl+右键拖拽切线**（Blender 式 knife）：记录指针轨迹，采样点距轨迹 < 10px 的连线红色高亮，松开批量 `DeleteCable`（索引降序防漂移）、Escape 取消；Ctrl+右键单击不再弹加节点菜单；③ **连线中间控制点**：每条连线贝塞尔中点画控制圆点（hover 放大），点击弹菜单——音频线可 Insert Effect / Insert Channel Strip（`PatchAction::InsertNode{cable, kind, at}`：A→B 替换为 A→新→B，新节点落位点击处，复用 `create_patch_node`/`connect_audio`），Delete Cable 全类型可用；几何命中统一收敛到 `cable_controls` 助手 + `dist_point_seg`。170 测试绿）
- [ ] P5f 单输入语义 + Mixer 节点（D14–D21）：spec v2 评审通过（docs/spec/node-graph.md）。决议：master 也单输入；strip 退役并入 mixer——per-channel gain/pan/mute/solo（舞台调音台方式）+ 输出级 pan/mute；通道数按钮手动增减 2..32（拔线不丢设置，连线可落任意空通道）；v3→v4 加载期自动迁移；默认图改全局 mixer 拓扑；推子自动化后续版本。实现拆分见 spec §9 P5f（引擎 → model v4 → UI → 轨寻址/metering）
- [ ] P6 音频反馈环路（可选，1 块延迟节点）

## 新方向：MIDI 编辑能力对标 SoundCraft（M1–M3）

2026-10-10 调研 `.ref/soundcraft`（对标 Pro Tools），完整报告见 `ref-soundcraft-midi.md`。SoundCraft 的 MIDI Editor 本身仅 354 行，真正可借鉴的是周边基础设施。按依赖顺序分三步，**M1 不做完不要开 M2**——M2 的多选/框选依赖 M1 的稳定索引，M3 的 ops 要在 M1 的数据模型上做。

### M1 地基：数据模型 + 命令层（breaking change，单独 commit）

- [x] **M1.1 PPQ 96 → 960** ✅ 2026-10-10。`TICKS_PER_QUARTER`=960 / `TICKS_PER_STEP`=240 / `TICKS_PER_BAR`=3840（后者改为派生，不再硬编码）。**顺带修隐患**：`realtime/sequencer.rs` 原先在 27/30/83 行**重复定义**三份常量，engine 侧改为单一来源；新增 `resoz-ui/tests/tick_base_consistency.rs` 锁住两侧一致（engine 刻意不依赖 model，实时线程与文档层解耦，故用测试而非 `pub use` 约束）。
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
- [ ] **M1.4 新 crate `resoz-command`**：命令注册表（id/label/menu/shortcut/params/enabled/run/undoable）+ `execute()`（`catch_unwind` 捕panic 并回滚、失败强制回滚不留半成品）+ `execute_merged(id, params, key)` 手势合并 + `Arc` CoW 快照 undo。
      内置自检测试：所有命令用空参数跑一遍不许 panic（照抄 soundcraft 的 `every_command_survives_empty_params`）。
      **注意**：undo 需要快照整个文档，但我们的 `Project` 目前是普通 struct（soundcraft 用 `Arc<Session>`）。落地时要么给 `Project` 套 `Arc`，要么先只做 execute + 回滚，undo 留到有实际需求时。

**M1 验收**：`cargo test --workspace` 全绿；旧 `.rez` 文件加载后 tick 语义不变；命令层空参数自检通过。

> **M1.1/M1.2 过程中的发现（2026-10-10）**：`auto_stop_keeps_pending_offs_draining` 测试原本硬断言"自动停止发生在第 750 个 block"。PPQ ×10 后实测停在**第 751** 个——位置计数器是 f64，每块累加 `dt_ticks` 会积累亚块误差。原断言在 96 PPQ 下是**侥幸通过**的。已改为"循环直到 `is_playing()` 为假"的不依赖具体 block 的写法。**教训：涉及时间推进的测试不要硬编码 block 数**，PPQ 或 tempo 变更时这类断言会静默失效。

### M2 编辑交互（依赖 M1）

- [ ] **M2.1 `GridValue`** 替代 `piano_roll.rs` 硬编码的 `SNAP_TICKS: u32 = 6`。枚举含 NoteValue(含附点/三连音)/Seconds/Frames/Samples。
      **关键**：网格线必须**从每个 bar line 重新计数**，否则 3/4、5/4、7/8 拍号下与小节线错位。
- [ ] **M2.2 多选 + 橡皮筋框选** + Shift/Ctrl 追加选择。
- [ ] **M2.3 方向键微调**：上下半音（Shift = 八度）、左右移动一格。
- [ ] **M2.4 velocity lane**（soundcraft 约 20 行，直接抄）+ 拖动接 `execute_merged`（一次拖动 = 一步 undo，这是手感命门）。
- [ ] **M2.5 音符亮度编码力度**（`0.45 + 0.55 * vel/127`），零成本可视化。

### M3 编辑能力（依赖 M1 数据模型）

- [ ] **M3.1 `ops.rs` 纯函数**：全部原地改、**不改 slice 长度**（故可作用于选区）、转调只 clamp 0..=127 **不 drop 音符**、随机部分走 seeded XorShift（保可测可复现）。
- [ ] **M3.2 量化**：8 个 `QuantizeOptions`。`exclude_within` 容差窗口最实用（已准的音不动，只拉跑偏的）。
      **swing 坑**：不是简单偏移，而是在 k-1/k/k+1 三条候选线里搜最近。
- [ ] **M3.3 力度**：Set/Add/Scale/Limit + `Ramp`（线性渐变，做crescendo）+ `Humanize`（随机±）。
- [ ] **M3.4 时值**：Set/Add/Scale + `Legato{gap}` + `RemoveOverlap`（同 pitch 重叠截断，旋律线单音化）。
- [ ] **M3.5 SMF 导入导出**（soundcraft 541 行，边界清单见报告）。**趁早做**，`Sequence` 结构还会变。
- [ ] **M3.6 Event List 窗口**（`egui::Grid` 约 120 行：Start/Note/Vel/Length 表格 + DragValue 精确编辑）。

### 明确不做（已评估）

- **Score Editor 五线谱**：soundcraft 自述"readable sketch rather than an engraving engine"，价值不如把piano roll 做扎实。
- **command journal / macro recording**：暂无 macro 需求。

### M1 顺带补AGENTS.md 实时铁律

1. **UI 层不得直接修改 model**，所有编辑经 `execute(cmd_id, params)`，以便统一撤销/回滚/审计。
2. **MIDI ops 纯函数化 + 确定性随机**：签名一律 `fn(&mut [Note], Options)`，不碰 clip/track/context，可脱离 DAW 单独测试。

## 附：VST3 调研结论（2026-10-07，已实施）

crates.io 上 `vst3` crate（0.3.0）为可用起点。VST3 宿主比 CLAP 复杂（预估 2-3 倍工作量），差异点：

1. **模块扫描**：Windows 下 `.vst3` 是目录 bundle（`Contents/x86_64-win/*.vst3` 内嵌 dll），需按规范遍历目录结构；
2. **COM**：插件对象基于 COM 生命周期管理（引用计数 / QueryInterface），宿主需处理 COM 初始化；
3. **双组件**：`IComponent`（音频处理）与 `IEditController`（参数/编辑器）分离，宿主负责连接两者；
4. **参数/事件桥接**：`ParameterChanges`/`EventList` 语义与 CLAP 类似但结构不同，复用 `ParamId::PluginParam(u32)` 空间与结构更新通道。

（实际落地：`resoz-vst3-host` 镜像 clap-host 架构，见 `implementation.md`。）
