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
| 信号处理/合成 | `fundsp`、`dasp` |
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

- **语言约定**：代码注释、doc comment、日志、UI 文案、错误消息一律**英文**（用户要求：egui 默认字体无 CJK 字形）；术语以 `.agents/glossary.md` 为准，新概念先登记再使用。`.agents/` 与本文件等项目文档可用中文。
- `cargo clippy -- -D warnings` 必须零警告。
- `cargo fmt` 格式化所有代码。
- 禁止 `unsafe`，除非：模块顶部有 `// SAFETY:` 注释 + 理由 + 测试。当前允许的边界：`glitcherz-clap-host` / `glitcherz-vst3-host` 的插件绑定层，以及 `glitcherz-ui` 的 `plugin_window` 模块（Win32 容器窗口，spec `plugin-gui.md` §4）。
- 浮点数统一使用 `f32`（音频路径）/ `f64`（UI 与时间计算），转换点集中在 `glitcherz-engine::convert`。
- 错误处理：应用层用 `thiserror` 定义错误枚举，不使用 `anyhow` 跨 crate 边界。
- **UI 层不得直接修改 model**：两种等价路径，都记录 undo——
  ① 命令：`GlitcherZApp::execute(id, params)`（报错/panic 自动回滚）；连续手势用 `execute_merged(id, params, key)`；
  ② 编辑守卫：`let before = self.begin_edit(); ... project_mut() 修改 ...; self.commit_edit(label, before)`。参数拖动用 `begin_param_edit`/`commit_param_edit(label, before, key)` + 拖动结束 `end_param_edit()`。
  复合操作（内部会触发其他可撤销编辑）先 `session.suspend_history()`，收尾 `resume_history()` 再 commit——只记外层一步。
  组件层（钢琴卷帘/timeline/automation lane）**只读文档 + 返回意图**（`RollIntent`/`TimelineAction`/`LaneIntent`），由 app 层翻译成命令或守卫编辑；组件不得持有 `&mut` 文档。
- **MIDI ops 纯函数化**：签名一律 `fn(&mut [Note], Options)`，原地修改、**不改变 slice 长度**（故可作用于选区），不碰 clip/track/context；随机部分走 seeded XorShift 保可测可复现。

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
- **Linux**：需要 ALSA 开发包（`libasound2-dev`）；GUI 依赖系统 GTK/wayland 库。
- **Windows**：建议安装 ASIO SDK 后启用 `cpal` 的 asio feature（可选）。
- **macOS**：无需额外依赖。

## 文档索引

| 文档 | 内容 | 何时读 |
| :--- | :--- | :--- |
| `.agents/architecture.md` | 架构约定：线程模型、处理图、数据模型 | 动引擎/模型代码前 |
| `.agents/glossary.md` | 术语表：中英对照 + 文案风格约定 | 写注释/日志/UI 文案、引入新概念前 |
| `.agents/implementation.md` | 已落地模块的实现要点、协议细节、版本 gotchas | 改现有功能、排查问题时 |
| `.agents/plan.md` | 路线图、当前状态、遗留任务、进行中的方向 | 规划下一步、同步进度时 |
| `.agents/spec/` | 特性 spec：`node-graph.md`（节点图）、`timeline.md`（编曲时间轴）、`editing-model.md`（MIDI 编辑）、`plugin-*.md`（插件宿主）、`sf2-loader.md`、`tempo-meter.md`（U3 拍号 + tempo 曲线） | 实施对应特性前 |

目录约定：`.agents/` 只放 agent / 开发向文档；`docs/` 只放面向用户的文档与 assets 素材（图片、示例工程等），二者不要混放。

## 给 AI 助手的特别说明

修改音频回调路径代码时，先完整阅读 `glitcherz-engine/src/realtime/` 下的现有代码并遵守上述实时铁律；对架构有疑问时优先阅读 `glitcherz-engine` 的模块文档注释与 `.agents/architecture.md`，而非自行假设。计划类变更先更新 `.agents/plan.md`，重大设计决策先写 `.agents/spec/` 再实现。
