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
- [ ] P6 音频反馈环路（可选，1 块延迟节点）

## 附：VST3 调研结论（2026-10-07，已实施）

crates.io 上 `vst3` crate（0.3.0）为可用起点。VST3 宿主比 CLAP 复杂（预估 2-3 倍工作量），差异点：

1. **模块扫描**：Windows 下 `.vst3` 是目录 bundle（`Contents/x86_64-win/*.vst3` 内嵌 dll），需按规范遍历目录结构；
2. **COM**：插件对象基于 COM 生命周期管理（引用计数 / QueryInterface），宿主需处理 COM 初始化；
3. **双组件**：`IComponent`（音频处理）与 `IEditController`（参数/编辑器）分离，宿主负责连接两者；
4. **参数/事件桥接**：`ParameterChanges`/`EventList` 语义与 CLAP 类似但结构不同，复用 `ParamId::PluginParam(u32)` 空间与结构更新通道。

（实际落地：`resoz-vst3-host` 镜像 clap-host 架构，见 `implementation.md`。）
