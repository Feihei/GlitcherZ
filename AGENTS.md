# AGENTS.md — ResoZ

## 项目简介

**ResoZ** 是一个使用 Rust 开发的跨平台（Windows / macOS / Linux）桌面数字音频工作站（DAW），参考 LMMS 的工作流，包含：

- 内置音源合成器
- 内置效果器（EQ、压缩、延迟、混响等）
- 步进音序器与钢琴卷帘
- 多轨混音器
- （远期）VST3 / CLAP 插件宿主支持

## 技术栈

| 层级 | 技术 |
| :--- | :--- |
| 语言 | Rust（edition 2024，stable toolchain） |
| 音频设备 | `cpal` |
| 信号处理/合成 | `fundsp`、`dasp` |
| GUI | `egui`（`eframe`） |
| 线程通信 | `crossbeam-channel` / `rtrb`（实时无锁队列） |
| 序列化 | `serde` + `ron` |
| 插件宿主（远期） | `clap-sys`、`vst3` |

## 目录结构

```
resoz/
├── Cargo.toml            # workspace
├── crates/
│   ├── resoz-engine/     # 音频引擎：处理图、调度、合成器、效果器
│   ├── resoz-model/      # 数据模型：轨道、片段、自动化、项目文件
│   ├── resoz-ui/         # egui 界面：走带、音序器、钢琴卷帘、混音器
│   └── resoz-app/        # 可执行入口，粘合 engine/model/ui
└── assets/               # 图标、默认音色、示例项目
```

## 实时音频铁律（最高优先级）

音频回调线程中**严禁**出现以下行为，任何 PR 违反即拒绝：

1. **禁止内存分配/释放**：不使用 `Vec::new()`、`Box::new()`、`String` 等，所有缓冲区预分配。
2. **禁止锁**：不使用 `std::sync::Mutex`、`RwLock`。UI → 音频线程通信只用无锁队列（`rtrb`）或原子变量。
3. **禁止系统调用**：不做文件 IO、日志、内存映射、加载插件。
4. **禁止 panic**：回调内使用 `expect` 之外的错误处理方式，任何 panic 都会崩溃整个音频流。
5. 音频回调代码集中在 `resoz-engine` 的 `realtime` 模块，评审时逐行审查。

## 架构约定

### 线程模型

```
UI 线程 (egui)                音频线程 (cpal callback)
     │  control message (rtrb)     ▲
     ├──────────────────────────►──┤
     │ ◄──◄─── state snapshot (原子/队列) ───┤
```

- **UI 线程**：每秒 60 帧，负责绘制、用户交互、项目编辑。持有 `resoz-model` 的可变状态。
- **音频线程**：`cpal` 回调，只从预分配的处理图读取，消费控制消息队列。
- **参数更新**：所有影响音频的参数（增益、音符、效果器开关）通过**无锁命令队列**传递，音频线程在块边界（block boundary）消费。
- **状态回传**：音频电平表、播放位置等用 `AtomicF32` / `AtomicU64` 回传给 UI。

### 处理图

- 节点实现 `Processor` trait，`process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer)`。
- 图结构变更（增删节点）由 UI 线程构建新图，通过**双缓冲交换**（`ArcSwap` 或消息队列重建指令）应用到音频线程，绝不原地修改运行中的图。
- 固定块大小处理（建议 64 或 128 samples），`cpal` 回调内的剩余样本用预分配的中间缓冲拆分。

### 数据模型

- `resoz-model` 中的所有类型必须 `Serialize + Deserialize`（RON 格式），保证项目文件向前兼容（新增字段用 `#[serde(default)]`）。
- 模型与实时引擎解耦：模型是"文档"，引擎是"播放器"，编译期互不依赖对方的内部类型。

## 编码规范

- `cargo clippy -- -D warnings` 必须零警告。
- `cargo fmt` 格式化所有代码。
- 禁止 `unsafe`，除非：模块顶部有 `// SAFETY:` 注释 + 理由 + 测试。当前预期仅 `cpal`/`egui` 绑定层可能需要。
- 浮点数统一使用 `f32`（音频路径）/ `f64`（UI 与时间计算），转换点集中在 `resoz-engine::convert`。
- 错误处理：应用层用 `thiserror` 定义错误枚举，不使用 `anyhow` 跨 crate 边界。

## 测试

- `resoz-engine`：单元测试覆盖每个 `Processor`（输入输出数学验证）；使用 `cargo test` 即可，不需要真实音频设备。
- 新增效果器/合成器模块必须附带：
  1. 静音输入 → 输出静音测试
  2. 满幅输入 → 无 NaN/Inf 测试
- `resoz-ui`：不强制 UI 测试，但模型层逻辑（音序、自动化求值）必须在 `resoz-model` 测试。

## 构建与运行

```bash
# 开发运行
cargo run -p resoz-app

# 全量检查（提交前必跑）
cargo fmt && cargo clippy --workspace -- -D warnings && cargo test --workspace

# 发布构建
cargo build --release -p resoz-app
```

平台依赖：
- **Linux**：需要 ALSA 开发包（`libasound2-dev`）；GUI 依赖系统 GTK/wayland 库。
- **Windows**：建议安装 ASIO SDK 后启用 `cpal` 的 asio feature（可选）。
- **macOS**：无需额外依赖。

## 路线图（按顺序实施，勿跳步）

1. ✅ `cpal` 输出正弦波（验证音频管线）
2. ✅ `egui` 集成 + 参数滑块实时控制正弦波频率（验证跨线程通信）
3. ✅ 音量包络 + 简单滤波器（第一个完整合成器）
4. ✅ 步进音序器（16 步，触发合成器）
5. ✅ 多轨 + 混音器 + 简单效果器链
6. ✅ 钢琴卷帘 MIDI 编辑
7. ✅ 项目保存/加载（RON）
8. ✅ 自动化曲线
9. ⬜ CLAP 插件宿主（先于 VST3，API 更简洁）
10. ⬜ VST3 插件宿主

## 当前状态与下一步

**当前进度**：路线图第 9 步完成 + 遗留项收尾（2026-10-07）。用户明确期望已落实：**乐器（合成器）插件为一等声源，效果器节点已接入，自动化桥接到插件参数**。VST3（第 10 步）调研结论见下一步任务。

已落地：
- workspace 四 crate 骨架（`resoz-model` / `resoz-engine` / `resoz-ui` / `resoz-app`）；
- `resoz-engine::realtime`：`AudioBuffer`、`Processor`（`set_param` + `note_event`）、`ProcessorChain`、`BlockRunner`（固定 128 块拆分 + 块边界命令消费 + `Feedback` 原子回传：主电平/位置/步进/走带/**每轨电平**）；
- **`Mixer`**：4 条 `TrackStrip`（`SynthProcessor → DelayProcessor`）并行处理 + 线性声像/增益/静音求和 → 主链（`MasterGain`）；`MixerLevels` 每块回传各轨 post-fader 峰值；
- 完整合成器链（每轨）：正弦振荡器 + 线性 ADSR → TPT SVF（低通/高通/带通）→ 反馈延迟（`DelayProcessor`：预分配环形缓冲 1 s 上限、feedback ≤ 0.9、反规范数冲洗、每声道独立写指针）；
- `Sequencer`（音频线程块边界采样时钟调度）：**纯窗口扫描**模型——只维护音乐位置（tick，1 四分音符 = 96），每块把 `[t0, t1)` 内到期事件一次性调度出来；BPM 变化只改未来速率（音乐位置不跳），循环回绕由窗口映射推导；**每轨双音源**（16 步步进模式 / 钢琴卷帘片段，`SetSource` 切换），卷帘音符按自身 start/length 触发；事件按 `(track, NoteEvent)` 送入对应轨道（≤1 块 ≈ 2.7 ms 抖动，逐样本偏移待后续）；
- **项目文件（第 7 步）**：`resoz-model::file` —— `ProjectFile{version: u32, project}`（当前 `PROJECT_VERSION = 1`），`save_project`（RON pretty）/ `load_project` / `parse_project`；解析前用 `ron::Value` 探测顶层结构（含 `project`/`version` 键 → 包装格式，否则回退裸 `Project` 开发期格式——serde 会把裸格式的字段当未知字段"成功"忽略，必须先探测）；程序化 `Default` 全部手写（派生 Default 会绕过 serde 字段默认值，曾导致 gate/tempo 为 0 的真 bug）；定长数组（`[Track; 4]`/`[Step; 16]`）在 RON 里是**圆括号**（serde tuple 路径），方括号只用于 `Vec`；
- **CLAP 宿主骨架（第 9 步进行中）**：
  - `resoz-test-plugin`（cdylib）：clack-plugin 单音正弦乐器 fixture（`org.resoz.sine`，instrument+stereo 特性），供宿主端到端测试；
  - **`resoz-clap-host` crate**（唯一允许 unsafe 的边界 = `raw::load_entry`，模块级 SAFETY 文档 + 无效文件测试）：`clap_search_paths`/`scan_clap_files`（标准目录 + `RESOZ_CLAP_PATH`）/`list_clap_plugins`（描述枚举 + instrument 特性识别）；`ClapInstance::spawn_rt(path, id, config)` 控制线程加载→实例化→激活，产出守卫 `ClapInstanceGuard`（持有动态库+实例，必须与节点同寿命）+ `ClapNode`；
  - **`ClapNode` 实现 `resoz-engine::realtime::Processor`**：音符/参数经定容事件槽（`MAX_PENDING_EVENTS=32`）暂存，每块以 `NoteOn/OffEvent`/`ParamValueEvent` 发出（块粒度）；`ParamId::PluginParam(u32)` 已加入参数空间（自动化桥接的落点）；音频经构造期预分配的 `AudioPorts` + 立体声临时缓冲渲染回 `AudioBuffer`；首块自动 `start_processing`，任何失败输出静音（无 panic）；处理用统一 `PluginAudioProcessor`（`as_started_mut`/`start_processing` 运行时切换）；
  - **CLAP 效果器 + 节点安全回收（第 9 步收尾）**：`TrackStrip` 信号顺序固化为 `声源 → DelayProcessor → 外部效果器槽[0..4]`（`effect_slots: [Option<Box<dyn Processor>>; 4]` 预分配）；`VoiceUpdate` 扩展 `InsertEffect/ClearEffects` 变体 + `node_id`（控制侧单调分配）；**节点回收回执队列**（`recycle_channel`，音频→控制方向）——被替换的旧节点由音频线程塞回执队列、控制线程 [`AudioEngine::reclaim_nodes`] 排空释放，UI 按 `node_id` 丢弃配对守卫后安全卸载动态库；实时线程**永不 drop 堆对象**（回收队列满时放弃卸装而非就地 drop）；
  - **轨道声源集成（第 9 步收尾）**：`realtime::voice` —— `VoiceUpdate` 结构更新队列（rtrb 携 `Box<dyn Processor>`，**非 Copy**，控制侧分配/实时侧块边界只消费）；`TrackStrip` 声源**双槽预分配**（内置 `SynthProcessor` 常驻 + `external_voice: Option<Box<dyn Processor>>`，换装=指针写入零分配）；`AudioEngine::set_external_voice/set_builtin_voice`；UI 侧 `voice_guards` 注册表持有 `ClapInstanceGuard`（**不可提前丢弃**——卸载动态库会使实时节点悬空，进程退出统一回收）；
  - **自动化桥接端到端**（引擎集成测试锁定）：自动化曲线 → `AutoParam::PluginParam{param_id}` → `ParamId::PluginParam` → 求值器 → `TrackStrip`（`PluginParam` 只路由外部声源）→ `ClapNode` → `ParamValueEvent` → 插件生效；测试含换装→参数写入→自动化爬坡→切回内置全流程；
  - model：`Track.voice: VoiceSpec`（`Builtin | Clap{path, plugin_id}`，serde tagged 随项目序列化，`apply_track_voice` 加载后自动装填）；
  - 分层决定：**engine 保持零 CLAP 依赖**——clap-host 依赖 engine（实现其 Processor trait）；轨道声源集成将走 `Box<dyn Processor>` 结构更新队列（非 Copy 队列，控制侧分配/实时侧只消费），避免在音频线程实例化插件；
  - 版本注意：clack 0.2（`clack-extensions` 宿主侧 trait 需 `clack-host` feature；`Pckn::new` 需显式 u16/u32 字面量；`start_processing` 返回 `&mut Started`；`PluginInstance::new` 收 `&CStr`）；
- **自动化（第 8 步）**：`resoz-model` 的 `Automation`（`AutoParam` = 音量/声像/截止 + `AutoPoint{tick,value}` 有序列表，线性插值、两端保持端值）挂 `Track.automations`；引擎侧 `RtAutomation`（定容 64 锚点 Copy）经 `SetAutomation` 命令 upsert（同参替换/空槽插入/空曲线=删除，每轨 ≤4 条）；求值器在 `BlockRunner` 每块**仅播放中**按"循环后的音乐位置"（步进源=一小节、卷帘源=片段 loop_ticks 取模）采样并经混音器内部路径直写参数（不走命令队列）；UI 在走带停止沿重推手动参数实现"停止后回到手动值"；
- `resoz-ui`：📂 打开 / 💾 保存（无路径时转另存为）/ 最近文件菜单（eframe `persistence` feature 存 RON，容量 8）；窗口标题随当前文件更新；状态栏消息（8 秒自动消失）报告保存/打开成败；`Ctrl+S` / `Ctrl+O` 快捷键（优先于演奏键盘映射）；`--open <文件>` 启动加载；
- `resoz-model`：`Project`（metadata + gate + master_gain + 4×`Track`）；`Track` = 名称 + `Pattern` + `Clip`（`Note{start,length,note,velocity}` 列表 + loop_ticks，tick 基准：`TICKS_PER_QUARTER=96`/步=24/小节=384）+ 音源枚举 + 混音（增益/声像/静音）+ `SynthParams` + `DelayParams`，全部 serde/RON + 字段级 `#[serde(default)]`；引擎侧镜像 `RtClip`（定容 128 音符 `Copy`，超容截断）经 `SetClip` 命令整块下发；
- 命令集（轨道定向）：`SetParam`（主链）/ `SetTrackParam{track}` / `Note{track}` / `Sequencer(Play|Stop|SetBpm|SetGate|SetPattern{track})`；模式整块 `Copy` 数组经队列传递；
- `resoz-ui`：eframe 0.36（glow）——走带 + BPM、**自动化泳道**（`lane` 自绘组件：参数三选 + 启用开关 + 锚点点按/拖拽/右键删除，播放头走线，截止频率对数轴）、**混音器面板**（轨道选择/静音/post-fader 电平条/竖直推子/声像）、16 步网格（播放位置黄色描边）、**钢琴卷帘编辑器**（`piano_roll` 自绘组件：点空白绘制、拖拽移动、右键删除、滚轮滚动音高、播放头走线；fit-to-width 时间轴、1/64 吸附）、画笔音符/音长、滤波器/ADSR/延迟面板（作用于选中轨道）、虚拟键盘（屏幕 + 电脑键盘）、主输出电平表；音序器音乐位置经 `Feedback.position_ticks`（AtomicF32）回传驱动播放头。

版本注意：cpal 0.18；egui 0.36（`App::ui(&mut Ui)`、`egui::Panel`、`Response` 无 `pressed/released` 用 `is_pointer_button_down_on`、`DragValue::range(RangeInclusive)`、`Painter::rect_stroke(rect, radius, stroke, StrokeKind)`）。

**下一步任务**（路线图第 10 步：VST3 插件宿主）：

VST3 调研结论（2026-10-07）：crates.io 上 `vst3` crate（0.3.0，"Rust bindings for the VST 3 API"）是可用起点；`vst3-sys` 最新发布缺失（curl 无版本返回）。VST3 宿主比 CLAP 复杂得多，预估工作量 ≥ CLAP 宿主的 2-3 倍，差异点：
1. **模块扫描**：Windows 下 `.vst3` 是目录 bundle（`Contents/x86_64-win/*.vst3` 内嵌 dll），需要按 VST3 规范遍历目录结构而非单文件；
2. **COM**：Windows 上插件对象基于 COM 生命周期管理（引用计数/QueryInterface），宿主需处理 COM 初始化；
3. **双组件**：`IComponent`（音频处理）与 `IEditController`（参数/编辑器）分离，宿主负责连接两者；
4. **参数/事件桥接**：`ParameterChanges`/`EventList` 语义与 CLAP 类似但结构不同，可复用 `ParamId::PluginParam(u32)` 空间与 VoiceUpdate 换装通道。

- [ ] VST3 宿主骨架：模块扫描 → IComponent/IEditController 实例化 → `VstNode` 实现 `Processor`（复用 ClapNode 的事件桥接模式）
- [ ] CLAP 插件参数**枚举**（clack 主线程生命周期接线，UI 参数面板数据源；当前参数面板仅显示已知参数）
- [ ] CLAP 效果器节点参数桥接（需处理图节点 ID 引入，区分声源/效果器参数空间）
- [ ] 插件状态保存/加载（CLAP state extension / VST3 FState → ProjectFile）

---

**给 AI 助手的特别说明**：修改音频回调路径代码时，先完整阅读 `resoz-engine/src/realtime/` 下的现有代码并遵守上述实时铁律；对架构有疑问时优先阅读 `resoz-engine` 的模块文档注释，而非自行假设。
