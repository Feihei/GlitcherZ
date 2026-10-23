# Spec — 插件 GUI 宿主 + Patch 交互升级（plugin-gui）

> 状态：定稿（2026-10-20，三轮 Q&A）。关联：`plugin-state.md`（StateGate 协议）、`node-graph.md`（图架构、P5e 遗留）。
> 验收锚点：Dexed（CLAP）在 Inspector 面板内嵌编辑器可操作、参数实时生效、保存/加载不丢。

## 1 目标与非目标

**目标**：

1. CLAP 插件 GUI（embedded）嵌入主窗口——插件自绘编辑器窗口挂在 Inspector 面板。
2. GUI 内参数编辑实时回接引擎 + 宿主滑块缓存同步。
3. Inspector 面板从"轨 → first-match 节点"重构为**选中节点驱动**（P5e 遗留收口）。
4. Patch 节点常显核心参数控件（plugdata 式）。
5. Virtual keyboard 节点入 patch（UI 路由，引擎零改动）。
6. VST3 `IPlugView` 镜像（后补，本 spec 只定协议差异）。

**非目标**：

- 不做 VST3 GUI 的先行实现（G7 后补）。
- 不做 macOS / Linux 嵌入路径（v1 Windows HWND，留平台抽象层）。
- 不做 CLAP floating 窗口模式（`is_floating=true` 的插件 v1 拒开 + log 提示）。
- 不做插件 GUI 内编辑的 undo 记录（插件内部状态宿主无从切分；持久化由 preset 系统 save 时捕获 state 覆盖）。
- 不做插件窗口尺寸的用户自由拖拽（v1：插件发起的 resize 跟随，宿主不提供 resize 手柄）。

## 2 决策记录

| # | 决策 | 结论 | 理由 |
| :--- | :--- | :--- | :--- |
| D1 | 格式范围 | CLAP 先行（G1–G3），VST3 镜像 G7 后补 | Dexed 是实测痛点；架构镜像成本低 |
| D2 | 挂载形态 | 嵌入式（child HWND），Inspector 面板内 | VST3 只有嵌入；floating 覆盖不了 VST3 |
| D3 | 平台 | Windows-first，`plugin_window` 平台抽象层 | 开发/实测环境 Windows；NSView/X11 后续补 |
| D4 | 实施顺序 | GUI 先行（G1–G3）→ 面板节点化（G4）→ 内联参数（G5）→ vk 节点（G6） | 痛点优先；面板重构后编辑器挂到新面板自然过渡 |
| D5 | 内联参数形态 | 节点常显核心参数（每节点类型注册表定义），高度自适应 | plugdata 式；选中展开抽屉增加往返 |
| D6 | 输入节点 | 纯 UI 端口：一源一节点（源唯一），事件只沿连线走、无轨道 fallback，可多条出线叠音源；默认工程带 Computer 节点，引擎零改动 | 引擎节点是为未来硬件 MIDI-in 铺路，v1 用不上 |
| D7 | GUI 调用与 StateGate | **gui 扩展调用不过闸** | CLAP 线程契约：[main-thread] gui 调用与 audio `process()` 并发安全由插件保证；state 扩展才要求停靠。停靠反而卡 GUI |
| D8 | GUI 编辑参数不进 undo | 同非目标 4 | preset save 捕获 state 已覆盖持久化 |
| D9 | tab 切走 = hide，切轨/换声源 = destroy | 编辑器生命周期最小化销毁，保留 GUI 状态跨 tab | hide 后 rect 消失只需 ShowWindow；跨轨插件实例都可能换 |
| D10 | 宿主滑块 → 插件 GUI 同步 | 依赖插件对 host 直调 `params::set_value` 的自身同步（CLAP 契约内建）；VST3 侧 G7 需 beginEdit/performEdit/endEdit 三段序列 | CLAP 无需额外协议；VST3 控制器只有经 performEdit 才会刷 GUI |

## 3 G1 — CLAP gui 扩展宿主侧

### 3.1 依赖与注册

- workspace `Cargo.toml`：`clack-extensions` features 追加 `"gui"`。`raw_window_handle_06` feature **不需要**（`Window::from_win32_hwnd` 直接收原生指针，不走 rwh 互转）。
- `GlitcherZHostShared` 实现 `HostGuiImpl`（clack `gui/host.rs:285`）：`resize_hints_changed` / `request_resize(GuiSize) -> Result` / `request_show` / `request_hide` / `closed(was_destroyed)`。
- `GlitcherZHostHandlers::declare_extensions` 注册 `HostGui`（同 `HostState` 模式）。
- 回调是 `&self`（Shared handler），状态经 `Arc<GuiHostState>`（原子/锁只在 UI 线程读写，`request_resize` 存期望尺寸 + `request_show`/`request_hide` 信号；`closed(was_destroyed=true)` 置粘性标志——CLAP 契约：宿主**必须**随后调 `destroy` 确认，`close_editor` 即确认点）。

### 3.2 guard 扩展

`ClapInstanceGuard` 仿 `state_ext` 模式：

- `gui_ext: OnceLock<Option<PluginGui>>`——`initializing` 回调捕获（`get_extension::<PluginGui>()`）。
- `open_editor(&mut self, hwnd: *mut c_void, scale: f64) -> Result<GuiSize, HostError>`：
  0. `is_api_supported(WIN32, embedded)`（false → `Err(GuiUnsupportedApi)`）
  1. `create()` → 2. `set_scale(scale)`（Windows per-monitor DPI，传 egui `pixels_per_point`）→ 3. `get_size()` → 4. `set_parent(raw::gui_set_parent(hwnd))` → 5. `show()`，返回初始 `GuiSize`。create 成功后续任一步失败 → destroy 清理（无半开状态）。
- `resize_editor(&self, size: GuiSize)`：`can_resize` 时 `adjust_size` 协商 → `set_size`。另提供 `hide_editor`/`show_editor`（D9 tab 切换）与 `editor_events()`/`gui_closed()` 轮询接口。
- `close_editor(&self)`：幂等 → `destroy()`（**destroy 即 `closed(was_destroyed=true)` 的宿主确认**——CLAP 契约要求宿主以 destroy 应答插件自毁报告；宿主侧 `gui_open` 标志保证 destroy 恰好一次）。open 失败路径（create 成功后续失败）同样 destroy 清理。
- `resize_editor(&self, size: GuiSize)`：`adjust_size` → `set_size`。
- 全部方法在 UI 主线程调用（clack `PluginMainThreadHandle` 契约）；**不过 StateGate**（D7）。
- unsafe 边界：`set_parent` 的窗口句柄存活契约——调用方（UI 层）保证主窗口存活期间才 open，close 先于 guard drop（§4.3 时序）。

### 3.3 测试插件假 GUI

`glitcherz-test-plugin` 实现 clack-plugin 的 gui 扩展（**不开真窗口**）：

- 观察状态放 `SineShared`（宿主进程读不到 cdylib 的全局 static，经只读参数暴露回去）：调用位图 `gui_calls: AtomicU32`（`GUI_CALL_*` 位）→ 参数 ID 2 "GuiCallMask"；destroy 计数 `gui_destroy_count: AtomicU32` → 参数 ID 3 "GuiDestroyCount"（幂等验证需要次数，位图只能证"调用过"）。宿主测试侧镜像常量。
- 回调触发：`set_parent` 调 `host.request_resize(240,160)`、`show` 调 `host.request_show()`（main thread handle `.shared().get_extension::<HostGui>()`）；**负 scale** = 测试钩子：`host.closed(true)` + set_scale 返回 Err（模拟插件自毁）。
- `is_api_supported` 只认 WIN32 嵌入；`get_size` 返回固定 `(320, 200)`；`can_resize` true + `adjust_size` 直通。
- 协议测试（CI 无头可跑，`glitcherz-clap-host`）：open_editor 调用序列（create → set_scale → get_size → set_parent → show 位图逐位断言）、close 幂等（destroy 计数恰好一次，重复 close 不加）、`closed` 路径（负 scale → open Err(SetScaleError) + 失败清理 destroy 即确认 + 可重开）、`request_resize`/`request_show` 到达 `editor_events()`（take 语义二次 poll 为空）、未开编辑器时 resize/hide/show 报 `GuiNotOpen`。

## 4 G2 — UI 嵌入与窗口同步

### 4.1 平台抽象层

`glitcherz-ui/src/plugin_window.rs`：

```rust
pub struct PluginWindowHandle(platform_impl::NativeChild); // Windows: HWND 子窗口操作集
// reposition(rect_px: IntRect) / show() / hide()
```

- `cfg(windows)`：`windows-sys`（feature `Win32_UI_WindowsAndMessaging` + `Win32_Foundation`）`SetWindowPos` / `ShowWindow`。
- 非 Windows：编译期空实现，`open_editor` 返回 `Err(UnsupportedPlatform)`。
- **unsafe 边界登记**：AGENTS.md 禁 unsafe 例外新增此项（平台窗口操作层，模块顶部 `// SAFETY:` 说明——句柄来源 `eframe` 主窗口、仅 UI 线程调用、生命周期由 §4.3 时序保证）。

### 4.2 reposition 循环

- 句柄来源：`GlitcherZApp::ui` 的 `frame`（当前 `_frame` 未用）→ `frame.winit_window()` → `HasWindowHandle`（eframe 0.36 已实现，epi.rs:702）→ `Win32WindowHandle.hwnd`。`raw-window-handle 0.6` 已在树（eframe 传递），加直依赖。
- Inspector 面板（目标节点带编辑器时）画编辑器区：折叠头 `CollapsingHeader("Editor")` 的**三角在占位矩形上方一行**（2026-10-23 起；此前是 `Show Editor` / `Close Editor` 按钮，展开后原生窗口会盖住按钮），展开后 `ui.allocate_exact_size(desired_size, Sense::hover())` 占位 → `rect` × `pixels_per_point` → 物理像素 → `SetWindowPos`（SWP_NOACTIVATE，z-order 恰在 GL surface 上方由 child 窗口语义天然成立）。
- 尺寸状态机：`desired_size` 初值 = `open_editor` 返回值；插件 `request_resize` → 更新 desired_size + `ctx.request_repaint()`（搭现有 50ms repaint 班车即可，不强求即时）。
- rect 滚出视口 / tab 切走（`MainView` 离开 Inspector）→ `hide()`（D9）；回到 Inspector → `show()` + 立即 reposition。
- `pixels_per_point` 变化（zoom）→ reposition 用新换算（插件内容不缩放，仅位置/容器尺寸跟随）。

### 4.3 销毁时序（正确性关键）

不变量：**编辑器 destroy 必须先于 guard drop（dll 卸载）**，且先于主窗口销毁。

1. 切轨 / 换声源 / 换 preset（重载插件）/ 删除节点：**所有**将 guard 移入 `retiring_guards` 或替换 `voice_guards` 的路径，前置 `close_editor`（幂等）。
2. app 退出（`ViewportCommand::Close` 处理点）：先遍历关闭全部编辑器，再走现有退出流程。
3. 插件自毁（`closed(was_destroyed=true)`）：宿主标记后不再调 destroy，UI 侧清面板。
- 上述路径在 lib.rs 的 guard 生命周期管理点集中收口为一个 `close_track_editor(track)` / `close_all_editors()` 助手，禁止各处直调 `close_editor` 绕过状态清理。

## 5 G3 — 参数回接（CLAP）

- `HostParamsImplShared::request_flush`（原空实现）→ 置原子标志 `flush_requested`（`ParamsHostState`，与 `GuiHostState` 同模式）。
- UI 帧驱动（`GlitcherZApp::ui` 尾部，50ms repaint 已保证节拍）：标志置位时调 params 扩展的 `flush`（空 in queue / 栈上 out queue）→ 解析 out 参数 → `set_node_param`/`send_track_param` 引擎下发 + `clap_voice_values` 滑块缓存同步（编辑器打开时才轮询，避免空转）。
- **实施修正（2026-10-20）**：clack 的 `PluginParams::flush` 包装需 `InactivePluginMainThreadHandle`（实例已激活，拿不到）、`flush_active` 需 processor 的独占 handle（processor 已移交音频线程的 `ClapNode`）——两条包装路径在本宿主架构下都不可用，实际落地为 `raw::flush_params` vtable 直调（与 `enumerate_params` 同模式）。另有插件侧分发细节：clack wrapper 对**活跃**实例把 flush 回调分派给 audio-processor 实现（`PluginAudioProcessorParams::flush`），inactive 时才走 main-thread 版本——插件两侧实现都要写 out queue。
- 通道归属：flush 回读发生在 guard（有 `PluginInstance`），下发需要 node_id——`PluginGuardEntry` 已有，编辑器 open 时缓存 entry 的 `(node_id, track)` 到 GUI 状态（G2 的 `PluginEditor` 已携带）。
- 并发：flush 不过 StateGate（D7 同款裁决：[main-thread] 插件调用，`request_flush` 的标准宿主响应，插件的 GUI 与 process 并发安全保证同样覆盖 flush；过闸反而每帧打断音频）。
- 测试：test-plugin 假 gui 的 `hide` 提供"内部改参数"测试钩子（改 amplitude + 置 dirty + 调 host `request_flush`）→ 宿主轮询后断言回读 `(1, 0.75)` + take 语义；回读值经 `ParamId::PluginParam` 桥下发后影响引擎输出。
- 宿主滑块 → 插件 GUI：无额外协议（D10，CLAP 契约内建）。

## 6 G4 — Inspector 面板按选中节点重构（P5e 收口）

- 语义：**patch 中选中的 instrument / effect / mixer 文档节点** = Inspector tab 的编辑目标（类比 Edit tab 之于 clip）。双击 patch 节点跳转 Inspector tab（同双击 timeline clip 进 Edit）；**但当节点没有可显示参数时不跳**——VK / Feedback / legacy strip 双击无反应，否则面板会 fallback 到"当前轨的乐器"显示别人的参数（2026-10-23）。双击固定的 Timeline 节点跳 Timeline tab，Master / Timeline In 不响应。
- 面板按 `NodeDoc` 变体分派：
  - Instrument：preset 工具条 + 参数栈 + 插件滑条 + **编辑器折叠区**（G2）。源选择已于 U9 E5 迁出（加节点即带最终声源，spec `plugin-browser.md` D1/D3）。
  - Effect：效果器参数面板（现有 `set_node_param(ParamId::PluginParam)` 节点寻址路径）。
  - Mixer：通道条控制台（`mixer_section(ui, mixer_doc)`）——**只有选中 mixer 节点时才出现**（2026-10-23）；轨道增删归 timeline 工具栏，自动化曲线归 timeline 底部 strip。
- 寻址统一为 **node_id**：`clap_voice_values` / `vst3_voice_values` 从 `HashMap<track, …>` 迁移为 `HashMap<node_id, …>`（效果器参数缓存已是 node_id 键，乐器侧对齐）；`track_channel_target` 的轨推导保留为"无选中节点"回落（当前 selected_track 行为不变）。
- 编辑器生命周期挂接：选中节点变化 → 旧编辑器 `close_editor` → 新节点是插件乐器且用户开了编辑器 → 重开。编辑器开关状态会话内存，不持久化。

## 7 G5 — Patch 节点内联核心参数

- 节点类型注册表 `inline_params: &[InlineParamSpec]`（ui 层静态表，按 `NodeDoc` 变体）：
  - 内置 Synth：cutoff / resonance / gain
  - Delay：time / feedback / mix
  - ChannelStrip(Mixer)：out gain（通道参数仍走面板）
  - SoundFont：无（preset 已是主操作）
  - 插件节点：前 2 个枚举参数（v1 上限，防小控件刷屏）
- 绘制：节点 body 下方每参数一行小滑条（`egui::Slider` 微型化，`drag_value` 风），节点高度 = 现有（端口自适应）+ 参数行数 × 行高；`NODE_HEIGHT_MIN` 机制不变，公式叠加。
- 值写入：**全部走 `set_node_param` 非重建路径**（ParamId 已有对应变体；插件参数 = `ParamId::PluginParam`）。内联滑条与面板滑条共享合并手势键（一次拖动 = 一步 undo——参数虽非文档态，与现有面板滑条行为对齐）。
- 测试：注册表覆盖每个内建节点变体；高度公式；控件值 ↔ ParamId 换算往返。

## 8 G6 — Keyboard input 节点（MIDI 输入端口）

> **2026-10-22 改版（A 设计）**：vk 从"按选中生效的软路由"变成**输入端口**。旧语义（选中 vk 才接管电脑键盘、未选中回落到选中轨）已废弃。

- 文档：`NodeDoc::Keyboard { node_id, track, source }`，`source: MidiSource = Computer | Device(name)`（`track` 仅作 patch 摆放分组，**不参与路由**；patch 右键菜单 `Keyboard In (computer)`）。
- **一源一节点**：一个节点只绑一个源，一个源只属于一个节点。`validate` 拒绝重复绑定（两个节点听同一个设备会把同一次按键送进两套互不相关的扇出）。结构即约束：连线和绑定就是唯一事实，看 patch 就知道信号去哪。
- **无 fallback**：键盘事件只沿该节点的出线走。没有输入节点、或节点没接线 → 静音（状态栏 `Keys → none` + 首次按键给出提示），不再悄悄播选中轨。
- 默认工程带一个 `Computer` 节点并连到 Track 1 的 instrument：新工程开箱能弹，但那条线是**可见、可删**的一条真实连线，而不是隐式行为。
- 引擎：**跳过**——不建 processor、不占引擎节点槽；`ExecPlan` 解析时忽略 Keyboard 出边；`CableDoc` 允许 `Keyboard → Instrument` 的 Midi 连线。
- 路由纯函数：`GraphDoc::computer_keyboard() -> Option<node_id>`（谁绑了电脑键盘）+ `keyboard_routes_from(vk) -> Vec<node_id>`（BFS 沿 Midi 连线收集全部 instrument，按文档序返回）。单测：无连线 / 连效果器（不穿透）/ 多目标全部返回且文档序 / 非键盘节点 / 源唯一性。
- **多出线**：一个输出口可拉多条 Midi 连线（MIDI 源是"多输出 / 单输入"的一端，与时间轴 track outlet 同规则），一次按键送到全部目标 → 无需混音器即可叠音。每次连线是**新增**（旧实现 `retain` 掉同源连线，把 vk 锁死单目标）；重复连线忽略；上限 `MAX_MIDI_FANOUT`（4）在 dispatch 层拦，文档层不拒（加载别人多连线的工程不会整份失败）。
- 手持音符只在**掉出路由**的目标上补 `NoteOff`：新加目标不切断正在响的音；删除输入节点/连线时全部释放。
- **`Ctrl+L` 快速试听**：把选中的 instrument 接到 / 断开电脑键盘节点（只动这几条线，不重写手搭的其他连线；同样受 `MAX_MIDI_FANOUT` 限制）。
- **屏幕键盘退役**：synth/mixer tab 底部的 on-screen 键盘与 `Keyboard — Track N` 头部一并删除（输入只有一个入口，键盘区不再有"当前轨"概念）；状态栏改为 `Keys → N synth(s) T1 T3`。
- 硬件 MIDI 输入**未接线**：`Device(name)` 只是占位。落地时的约定：掉线保留绑定并显示缺失，**不自动重绑**（自动挑"下一个可用设备"会静默把键盘送到别的音源）。设备枚举需要新依赖（如 `midir`），单独一步。
- `--auto-play-note`（冒烟/诊断）直接 `EngineCommand::Note { track }`：它验证的是音频链，不是路由。

## 9 G7 — VST3 IPlugView 镜像（后补，只记差异）

- `vst3 0.3.0` bindings 已含 `IPlugView` / `IPlugFrame` / `ViewRect` / `kPlatformTypeHWND` / `IPlugViewContentScaleSupport` / `IEditControllerHostEditing`——无需自写 COM 绑定。
- 宿主侧新增 `HostPlugFrame`（`Class` + `IPlugFrameTrait::resizeView`，`gui.rs`）：只记录请求（原子打包 w/h），**不**在回调里调 `onSize`（宿主此时还没改窗口尺寸）。
- guard：`open_editor()`（`createView("editor")` → `setFrame(plug frame)` → 可选 `setContentScaleFactor` → `isPlatformTypeSupported` → `attached(hwnd, kPlatformTypeHWND)` → `getSize`）；关闭 = `removed` + `setFrame(null)` + release。任何中途失败都要撤销 attach（不留半开视图）。
- 尺寸握手（**实施修正 2026-10-21**）：`resizeView` 只落到 UI 的 `pending_ack`，UI 真正 reposition 容器之后才调 `IPlugView::onSize(rect)` 确认——spec 原先把 `onSize` 写在 plug frame 侧，顺序反了（插件等的是"窗口已改尺寸"的确认）。
- 参数回接：`NoopComponentHandler` 替换为实现 `IComponentHandler` + `IComponentHandler2` 的 `HostComponentHandler`：`performEdit(paramID, valueNormalized)` → 无锁 `EditQueue`（原子槽，GUI/音频线程都可能写）→ UI 每帧 drain → `set_node_param` + `vst3_voice_values` 同步（归一化域，沿用 lib.rs 注释约定）。VST3 没有 CLAP `request_flush` 那种"宿主拉"的通道，只有插件"推"的 `performEdit`。
- 宿主滑块 → 插件 GUI（**实施修正 2026-10-21**）：VST3 的正确接口是 `IEditControllerHostEditing::beginEditFromHost(id)` → `setParamNormalized(id, v)` → `endEditFromHost(id)`（`beginEdit/performEdit/endEdit` 是**插件→宿主**方向的 IComponentHandler 回调，方向写反了）。控制器没实现该扩展时退化为单独 `setParamNormalized`。
- UI 层（plugin_window / 面板 / reposition / 销毁时序）全复用，格式分派在 guard 层；`PluginEditor.pending_ack` 只服务 VST3 的 onSize 确认。

## 10 分阶段实施与验收

| 阶段 | 内容 | 验收 |
| :--- | :--- | :--- |
| G1 | clack gui feature + HostGuiImpl + guard open/resize/close + test-plugin 假 gui | 协议测试 CI 绿（无头） |
| G2 | plugin_window 平台层 + Inspector 面板嵌入 + reposition + 销毁时序 | **Dexed 实测**：打开/操作/切轨/退出无崩溃 |
| G3 | request_flush → flush 回接 + 滑块缓存同步 | Dexed GUI 拨参数引擎即时生效；宿主滑块与 GUI 双向一致 |
| G4 | 面板按选中节点重构 + 双击跳转 + 乐器参数缓存 node_id 化 | 多插件轨各自面板寻址正确；回归：单轨行为不变 |
| G5 | 内联参数注册表 + 节点控件 | 拖内联滑条不触发图重建（现有测试锚点）；plugdata 手感 |
| G6 | vk 节点文档/菜单/路由 | 无 vk 时零行为差异（回归）；选中 vk 弹琴路由正确 |
| G7 | VST3 镜像 | 测试 VST3 插件编辑器往返 + performEdit 回接 |

每阶段独立 commit，`cargo fmt && cargo clippy --workspace -- -D warnings && cargo test --workspace` 全绿为门槛。

## 11 开放问题

- 插件 GUI 内部弹出的模态对话框（如 Dexed 的 cart 管理窗）与主窗口的模态关系——v1 不处理 `set_transient`，实测后再定。
- `resize_hints_changed` 的纵横比约束：v1 忽略 hints，只跟 `request_resize`。
- 插件编辑器尺寸超出面板可视区：v1 让面板滚动（`ScrollArea` 已有），不缩放内容。