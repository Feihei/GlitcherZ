# Spec — 插件 GUI 宿主 + Patch 交互升级（plugin-gui）

> 状态：定稿（2026-10-20，三轮 Q&A）。关联：`plugin-state.md`（StateGate 协议）、`node-graph.md`（图架构、P5e 遗留）。
> 验收锚点：Dexed（CLAP）在 Synth 面板内嵌编辑器可操作、参数实时生效、保存/加载不丢。

## 1 目标与非目标

**目标**：

1. CLAP 插件 GUI（embedded）嵌入主窗口——插件自绘编辑器窗口挂在 Synth/Mixer 面板。
2. GUI 内参数编辑实时回接引擎 + 宿主滑块缓存同步。
3. Synth/Mixer 面板从"轨 → first-match 节点"重构为**选中节点驱动**（P5e 遗留收口）。
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
| D2 | 挂载形态 | 嵌入式（child HWND），Synth/Mixer 面板内 | VST3 只有嵌入；floating 覆盖不了 VST3 |
| D3 | 平台 | Windows-first，`plugin_window` 平台抽象层 | 开发/实测环境 Windows；NSView/X11 后续补 |
| D4 | 实施顺序 | GUI 先行（G1–G3）→ 面板节点化（G4）→ 内联参数（G5）→ vk 节点（G6） | 痛点优先；面板重构后编辑器挂到新面板自然过渡 |
| D5 | 内联参数形态 | 节点常显核心参数（每节点类型注册表定义），高度自适应 | plugdata 式；选中展开抽屉增加往返 |
| D6 | vk 节点 | 纯 UI 路由：文档记录节点+连线，选中时键盘事件沿连线解析目标 instrument，引擎零改动 | 引擎节点是为未来硬件 MIDI-in 铺路，v1 用不上 |
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
- Synth 面板（当前轨声源为 CLAP 插件时）画编辑器区：`ui.allocate_exact_size(desired_size, Sense::hover())` 占位 → `rect` × `pixels_per_point` → 物理像素 → `SetWindowPos`（SWP_NOACTIVATE，z-order 恰在 GL surface 上方由 child 窗口语义天然成立）。
- 尺寸状态机：`desired_size` 初值 = `open_editor` 返回值；插件 `request_resize` → 更新 desired_size + `ctx.request_repaint()`（搭现有 50ms repaint 班车即可，不强求即时）。
- rect 滚出视口 / tab 切走（`MainView` 离开 Synth）→ `hide()`（D9）；回到 Synth → `show()` + 立即 reposition。
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

## 6 G4 — Synth/Mixer 面板按选中节点重构（P5e 收口）

- 语义：**patch 中选中的 instrument / effect / mixer 文档节点** = Synth/Mixer tab 的编辑目标（类比 Edit tab 之于 clip）。双击 patch 节点跳转 Synth tab（同双击 timeline clip 进 Edit）。
- 面板按 `NodeDoc` 变体分派：
  - Instrument：现有声源三选一（内置 synth 参数 / SF2 面板 / 插件滑条 + preset 工具条）+ **编辑器嵌入区**（G2）。
  - Effect：效果器参数面板（现有 `set_node_param(ParamId::PluginParam)` 节点寻址路径）。
  - Mixer：通道条面板（现有 mixer 面板按 node_id 直达）。
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

## 8 G6 — Virtual keyboard 节点（UI 路由）

- 文档：`NodeDoc::Keyboard`（新变体，serde default 加法——无字段的 unit 变体，旧文件无感）；patch 右键菜单 `AddNodeKind::VirtualKeyboard`。
- 引擎：**跳过**——`install_node` 遇 `Keyboard` 不建 processor、不占引擎节点槽（文档 node_id 与引擎 fresh_node_id 映射表记"无 RT 对应"）；`CableDoc` 允许 `Keyboard → Instrument` 的 Midi 连线（连线类型校验表加一行）；引擎 `ExecPlan` 解析时忽略 Keyboard 出边（它不参与时间轴扇出）。
- 路由：patch 选中集含 Keyboard 节点且其 MIDI 出边可达 instrument → `handle_keyboard_input` / `keyboard_section`（电脑键盘 + 屏幕键盘）目标从 selected_track 切换为该 instrument 节点（node_id 直达 live note 通道；现有 per-track live 路径不动，vk 是**附加**路由，未选 vk 时行为完全兼容）。
- 可达性解析为纯函数 `keyboard_route(doc, selection) -> Option<node_id>`（BFS 沿 Midi 连线，遇第一个 instrument 终点停），单测覆盖：无 vk 选中 / vk 无连线 / vk 连效果器（穿 MIDI 直通？——不穿，只认 instrument 终点）/ 多 instrument 取文档序首个。
- `octave_base` 全局保留；vk 节点绘制 = 小钢琴图标 + 名称节点。

## 9 G7 — VST3 IPlugView 镜像（后补，只记差异）

- `vst3 0.3.0` bindings 已含 `IPlugView` / `IPlugFrame` / `ViewRect` / `kPlatformTypeHWND`（bindings.rs:1775/1672/4184/4237）——无需自写 COM 绑定。
- 宿主侧新增 `HostPlugFrame`（`Class` + `IPlugFrameTrait::onSize`，照 `NoopComponentHandler` 模式，host_impl.rs）。
- guard：`create_view()`（`IEditController::createView("editor")`）→ `attached(hwnd, kPlatformTypeHWND)` → `getSize`；关闭 = `removed` + `release`。
- 参数回接：`NoopComponentHandler` 替换为实现 `ComponentHandler2` 的 `performEdit(paramID, valueNormalized)` → `set_node_param` + `vst3_voice_values` 同步（归一化域，沿用 lib.rs:22-25 注释约定）。
- 宿主滑块 → 插件 GUI：滑块变更补 `beginEdit/performEdit/endEdit` 三段（控制器才会刷自己的 GUI）——CLAP 无此协议，VST3 专有。
- UI 层（plugin_window / 面板 / reposition / 销毁时序）全复用，格式分派在 guard 层。

## 10 分阶段实施与验收

| 阶段 | 内容 | 验收 |
| :--- | :--- | :--- |
| G1 | clack gui feature + HostGuiImpl + guard open/resize/close + test-plugin 假 gui | 协议测试 CI 绿（无头） |
| G2 | plugin_window 平台层 + Synth 面板嵌入 + reposition + 销毁时序 | **Dexed 实测**：打开/操作/切轨/退出无崩溃 |
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