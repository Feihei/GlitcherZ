# AGENTS.md — GlitcherZ

> 本文件是**索引与硬规范**。架构约定、实现细节、计划分别见 `.agents/` 下文档；动代码前先读对应文档。

## 项目简介

**GlitcherZ** 是一个使用 Rust 开发的跨平台（Windows / macOS / Linux）桌面数字音频工作站（DAW），参考 LMMS 的工作流，包含：

- 内置音源合成器
- 内置效果器（EQ、压缩、延迟、混响等）
- 钢琴卷帘与编曲时间轴
- 多轨混音器
- （远期）VST3 / CLAP 插件宿主支持

## 技术栈

| 层级 | 技术 |
| :--- | :--- |
| 语言 | Rust（edition 2024，stable toolchain） |
| 音频设备 | `cpal` |
| 信号处理/合成 | 自研 DSP（`glitcherz-engine/src/realtime/`）；算法参考 `fundsp`（**非依赖**，取其思路不引 crate） |
| GUI | `egui`（`eframe`） |
| 线程通信 | `crossbeam-channel` / `rtrb`（实时无锁队列） |
| 序列化 | `serde` + `ron` |
| 插件宿主 | `clack`（CLAP）、`vst3`（VST3，COM 绑定） |

## 目录结构

```
glitcherz/
├── Cargo.toml              # workspace
├── crates/
│   ├── glitcherz-engine/       # 音频引擎：处理图、调度、合成器、效果器
│   ├── glitcherz-model/        # 数据模型：轨道、片段、自动化、项目文件
│   ├── glitcherz-command/      # 命令注册表：事务性编辑 + undo 历史（持有 Arc<Project>）
│   ├── glitcherz-ui/           # egui 界面：走带、音序器、钢琴卷帘、混音器
│   ├── glitcherz-clap-host/    # CLAP 插件宿主：扫描 / 实例化 / 守卫管理
│   ├── glitcherz-vst3-host/    # VST3 插件宿主：架构镜像 clap-host
│   ├── glitcherz-test-plugin/  # CLAP 测试插件 fixture（cdylib）
│   ├── glitcherz-test-vst3/    # VST3 测试插件 fixture
│   └── glitcherz-app/          # 可执行入口，粘合 engine/model/ui
├── .agents/               # agent 开发文档（见下方「文档索引」）
└── docs/                  # 用户文档与 assets 素材
```

## 实时音频铁律（最高优先级）

音频回调线程中**严禁**出现以下行为，任何 PR 违反即拒绝：

1. **禁止内存分配/释放**：不使用 `Vec::new()`、`Box::new()`、`String` 等，所有缓冲区预分配。
2. **禁止锁**：不使用 `std::sync::Mutex`、`RwLock`。UI → 音频线程通信只用无锁队列（`rtrb`）或原子变量。
3. **禁止系统调用**：不做文件 IO、日志、内存映射、加载插件。
4. **禁止 panic**：回调内使用 `expect` 之外的错误处理方式，任何 panic 都会崩溃整个音频流。
5. 音频回调代码集中在 `glitcherz-engine` 的 `realtime` 模块，评审时逐行审查。

## 编码规范

- **语言约定**：代码注释、doc comment、日志、UI 文案、错误消息一律**英文**（UI 语言约定；CJK 字形由内嵌 Noto Sans SC 兜底，仅服务用户数据如轨名/片段名）；术语以 `.agents/glossary.md` 为准，新概念先登记再使用。`.agents/` 与本文件等项目文档可用中文。
- `cargo clippy -- -D warnings` 必须零警告。
- `cargo fmt` 格式化所有代码。
- 禁止 `unsafe`，除非：模块顶部有 `// SAFETY:` 注释 + 理由 + 测试。当前允许的边界：`glitcherz-clap-host` / `glitcherz-vst3-host` 的插件绑定层，以及 `glitcherz-ui` 的 `plugin_window` 模块（Win32 容器窗口，spec `plugin-gui.md` §4）。
- 浮点数统一使用 `f32`（音频路径）/ `f64`（UI 与时间计算），转换点集中在 `glitcherz-engine::convert`。
- 错误处理：应用层用 `thiserror` 定义错误枚举，不使用 `anyhow` 跨 crate 边界。
- **UI 层不得直接修改 model**：两种等价路径，都记录 undo——
  ① 命令：`GlitcherZApp::execute(id, params)`（报错/panic 自动回滚）；连续手势用 `execute_merged(id, params, key)`；
  ② 编辑守卫：`let before = self.begin_edit(); ... project_mut() 修改 ...; self.commit_edit(label, before)`。参数拖动用 `begin_param_edit`/`commit_param_edit(label, before, key)` + 拖动结束 `end_param_edit()`。
  复合操作（内部会触发其他可撤销编辑）先 `session.suspend_history()`，收尾 `resume_history()` 再 commit——只记外层一步。
  组件层（钢琴卷帘/timeline）**只读文档 + 返回意图**（`RollIntent`/`TimelineAction`），由 app 层翻译成命令或守卫编辑；组件不得持有 `&mut` 文档。U4b 之后曲线编辑并入 timeline 的 inline 带（`TimelineAction::CurvePoint` / `CurveRemove`），独立的 automation lane 组件（`lane::show` + `LaneIntent`）已退休，模块改名 `curve.rs`，只剩轴映射（`axis`/`to_norm`/`from_norm`/`SNAP_TICKS`）。
- **MIDI ops 纯函数化**：签名一律 `fn(&mut [Note], Options)`，原地修改、**不改变 slice 长度**（故可作用于选区），不碰 clip/track/context；随机部分走 seeded XorShift 保可测可复现。
- **`glitcherz-ui` 面板方法分模块**：`impl GlitcherZApp` 的方法分布在 14 个同级模块（`transport` / `inspector_panel` / `mixer_panel` / `timeline_ops` / `patch_ops` / `file_ops` / `voices` / `recording` / `editing` / `engine_sync` / `settings` / `status_bar` / `keyboard` / `export_ops`），`lib.rs` 只留结构体、`new`、编辑守卫与 `ui()`。新方法写进对应模块，且必须 `pub(crate)` 才能被父模块与兄弟模块调用（私有方法对父模块不可见）。

## 特性编号（F-ID）

**一个特性 = 一份独立 spec = 一个 `F-###`。** 编号是身份，域是标签，二者分离——跨领域的特性不必拍板归属。

- **格式**：`F-001`，三位补零，全局递增，**永不复用、不回收**。下一个号 = 注册表当前最大号 +1。
- **域标签**（固定 9 个，可多值）：`ENGINE`（实时 / DSP / sequencer / PDC）、`PATCH`（节点图 / 路由 / 自动化 / 控制信号）、`EDIT`（MIDI / 曲线 / 时间轴 / 卷帘）、`MIX`（混音器 / 内建效果器）、`VOICE`（声源：synth / sample / SF2 / drums / modal / pluck / organ / wavetable）、`PLUG`（CLAP / VST3）、`PROJ`（工程文件 / 迁移 / 媒体 / 自动保存）、`IO`（录音 / 导出 / CLI / MCP）、`UI`（跨面板交互规范：快捷键 / 焦点 / 图标）。
- **写法**：标题 `### F-142 · clip 头裁（EDIT / PROJ，2026-10-26）`；里程碑 `F-142/M1`；spec 头部 `> ID: F-142 · 域: EDIT / PROJ`。
- **登记**：分配号后先写入 `implementation.md` 的「特性 ID 注册表」，再写 spec / 实现；`implementation.md` 的记录标题必须带 ID。
- **历史条目**：字母系列（U / W / X / Y / Z / B / I / E / S / M / P / G / R / C / A / SP）保留为别名，不重命名、不回溯编号；新内容一律用 F-ID。
- **无 spec 的特性**：先补一份最小 spec 再领号——编号与 spec 一一对应。

## 测试

- `glitcherz-engine`：单元测试覆盖每个 `Processor`（输入输出数学验证）；使用 `cargo test` 即可，不需要真实音频设备。
- 新增效果器/合成器模块必须附带：
  1. 静音输入 → 输出静音测试
  2. 满幅输入 → 无 NaN/Inf 测试
- `glitcherz-ui`：不强制 UI 测试，但模型层逻辑（音序、自动化求值）必须在 `glitcherz-model` 测试。

## 构建与运行

```bash
# 开发运行
cargo run -p glitcherz-app

# 全量检查（提交前必跑）
cargo fmt && cargo clippy --workspace -- -D warnings && cargo test --workspace

# 发布构建
cargo build --release -p glitcherz-app
```

平台依赖：
- **Linux**：ALSA 开发包（`libasound2-dev`）+ X11 / Wayland / xkbcommon / OpenGL 开发包（winit、rfd、egui glow 后端；不需要 GTK）。完整清单见 `.github/actions/install-linux-deps/action.yml`。
- **Windows**：建议安装 ASIO SDK 后启用 `cpal` 的 asio feature（可选）。
- **macOS**：无需额外依赖。

## 持续集成

| Workflow | 触发 | 做什么 |
| :--- | :--- | :--- |
| `.github/workflows/ci.yml` | push 任意分支（纯文档提交跳过）、PR 到 `main`、手动 | `fmt` → `clippy -D warnings` → 三平台（Linux/macOS/Windows）`cargo build --workspace` + `cargo test --workspace` |
| `.github/workflows/release.yml` | push `v*` tag | preflight（tag 与 `[workspace.package] version` 一致性 + fmt/clippy/test）→ 构建 Windows x86_64 / macOS universal / Linux x86_64 → 单点发布 Release（归档 + `sha256sums.txt`） |

- 发布前**先改 `Cargo.toml` 的 workspace 版本并提交，再打 tag**（`v0.1.0` ↔ `0.1.0`），不一致 preflight 直接失败。
- Linux 二进制在 `ubuntu-22.04` 上构建（glibc 2.35），以兼容更多发行版。
- 测试 job 先 `cargo build --workspace`：CLAP / VST3 host 单测会从 `target/debug` dlopen fixture cdylib，只跑 `cargo test` 不保证该产物存在。

## 文档索引

| 文档 | 内容 | 何时读 |
| :--- | :--- | :--- |
| `.agents/architecture.md` | 架构约定：线程模型、处理图、数据模型 | 动引擎/模型代码前 |
| `.agents/glossary.md` | 术语表：中英对照 + 文案风格约定 | 写注释/日志/UI 文案、引入新概念前 |
| `.agents/implementation.md` | 已落地模块的实现要点、协议细节、版本 gotchas；末尾「系列实施补记」收纳自 plan.md 归位的系列记录 | 改现有功能、排查问题时 |
| `.agents/plan.md` | 路线图、当前状态、未完成项、活跃系列（Met / Aud）、历史系列索引 | 规划下一步、同步进度时 |
| `.agents/spec/` | 特性 spec：`node-graph.md`（节点图）、`timeline.md`（编曲时间轴）、`editing-model.md`（MIDI 编辑）、`plugin-*.md`（插件宿主）、`sf2-loader.md`、`tempo-meter.md`（U3 拍号 + tempo 曲线）、`control-tracks.md`（U4 控制轨 CC 输出 + U12 曲线编辑：Edit tab · Linear/Smooth）、`roll-multiclip.md`（R 系列：多 clip 卷帘 + 音高曲线）、`effects.md`（内建效果器框架 + 统一参数平滑 + 立体声联动）、`midi-nodes.md`（MIDI 事件处理节点：琶音 / 滑音 / 包络 + 块内事件总线）、`curve-map.md`（曲线归一化 + Curve Map 节点 + pitch 输入口）、`midi-generators.md`（W4/W5 随机化 + strum）、`drum-kit.md`（W1 多采样鼓机）、`lfo.md`（LFO 控制源）、`instrument.md`（内置合成器：多波形 + 滤波器接线）、`autosave.md`（S3a 自动保存 + 崩溃恢复）、`media.md`（S3b 媒体相对路径 + 收集 + 重定位）、`audio-decoding.md`（X 系列：symphonia 解码接入 + WAV 零回归分路）、`dual-audio-input.md`（Z0 双音频输入口 + Ring Mod 样例 + 侧链/Vocoder 前置，已交付）、`sidechain-compressor.md`（Z1 双口压缩器：外部键控检测 + 零回归边界，已交付）、`modal-instrument.md`（B1 模态敲击声源：复旋转子 + 拍频 + 长尾，Mallet / 诵钵，已交付）、`vocoder.md`（Z2 双口 vocoder：16 带分析/合成 + 清浊噪声旁路 + 外部 carrier，已交付）、`pluck-instrument.md`（S1 Karplus-Strong 拨弦声源：延迟线 + 阻尼环路 + note-off 阻尼，已交付）、`organ-instrument.md`（S2 拉杆风琴：9 根 drawbar 共享相位 + 打击泛音 + A/R，已交付）、`wavetable-instrument.md`（S3 波表合成：4 帧锚点音色 + 每八度带限层 + position 插值，已交付）、`cli-commands.md`（S4 CLI 批量命令：驱动既有 `glitcherz-command` 注册表 + 固定退出码 + `--json` 信封，已交付）、`mcp-server.md`（S5 MCP server：`--mcp` stdio JSON-RPC + 6 个工具 + root 沙箱 + 插件默认禁用，已交付） | 实施对应特性前 |
| `docs/` | 用户文档：[`README.md`](docs/README.md) 索引、[`manual.md`](docs/manual.md) 主手册、[`shortcuts.md`](docs/shortcuts.md) 快捷键与手势、[`project-files.md`](docs/project-files.md) 工程文件与磁盘布局 | 改 UI 文案 / 交互 / 文件格式后同步；回答用户"怎么用"时 |

目录约定：`.agents/` 只放 agent / 开发向文档；`docs/` 只放面向用户的文档与 assets 素材（图片、示例工程等），二者不要混放。

## 给 AI 助手的特别说明

修改音频回调路径代码时，先完整阅读 `glitcherz-engine/src/realtime/` 下的现有代码并遵守上述实时铁律；对架构有疑问时优先阅读 `glitcherz-engine` 的模块文档注释与 `.agents/architecture.md`，而非自行假设。计划类变更先更新 `.agents/plan.md`，重大设计决策先写 `.agents/spec/` 再实现。
