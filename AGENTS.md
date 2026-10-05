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
6. ⬜ 钢琴卷帘 MIDI 编辑
7. ⬜ 项目保存/加载（RON）
8. ⬜ 自动化曲线
9. ⬜ CLAP 插件宿主（先于 VST3，API 更简洁）
10. ⬜ VST3 插件宿主

## 当前状态与下一步

**当前进度**：路线图第 5 步已完成（2026-10-05）。

已落地：
- workspace 四 crate 骨架（`resoz-model` / `resoz-engine` / `resoz-ui` / `resoz-app`）；
- `resoz-engine::realtime`：`AudioBuffer`、`Processor`（`set_param` + `note_event`）、`ProcessorChain`、`BlockRunner`（固定 128 块拆分 + 块边界命令消费 + `Feedback` 原子回传：主电平/位置/步进/走带/**每轨电平**）；
- **`Mixer`**：4 条 `TrackStrip`（`SynthProcessor → DelayProcessor`）并行处理 + 线性声像/增益/静音求和 → 主链（`MasterGain`）；`MixerLevels` 每块回传各轨 post-fader 峰值；
- 完整合成器链（每轨）：正弦振荡器 + 线性 ADSR → TPT SVF（低通/高通/带通）→ 反馈延迟（`DelayProcessor`：预分配环形缓冲 1 s 上限、feedback ≤ 0.9、反规范数冲洗、每声道独立写指针）；
- `Sequencer`（音频线程块边界采样时钟调度）：**每轨独立 16 步模式**、BPM（20..300）、gate 占比、play/stop 走带；事件按 `(track, NoteEvent)` 送入对应轨道（≤1 块 ≈ 2.7 ms 抖动，逐样本偏移待后续）；
- `resoz-model`：`Project`（metadata + gate + master_gain + 4×`Track`）；`Track` = 名称 + `Pattern` + 混音（增益/声像/静音）+ `SynthParams` + `DelayParams`，全部 serde/RON + 字段级 `#[serde(default)]`；
- 命令集（轨道定向）：`SetParam`（主链）/ `SetTrackParam{track}` / `Note{track}` / `Sequencer(Play|Stop|SetBpm|SetGate|SetPattern{track})`；模式整块 `Copy` 数组经队列传递；
- `resoz-ui`：eframe 0.36（glow）——走带 + BPM、**混音器面板**（轨道选择/静音/post-fader 电平条/竖直推子/声像）、16 步网格（编辑选中轨道，播放位置黄色描边）、画笔音符/音长、滤波器/ADSR/延迟面板（作用于选中轨道）、虚拟键盘（屏幕 + 电脑键盘，演奏选中轨道）、主输出电平表。

版本注意：cpal 0.18；egui 0.36（`App::ui(&mut Ui)`、`egui::Panel`、`Response` 无 `pressed/released` 用 `is_pointer_button_down_on`、`DragValue::range(RangeInclusive)`、`Painter::rect_stroke(rect, radius, stroke, StrokeKind)`）。

**下一步任务**（路线图第 6 步：钢琴卷帘 MIDI 编辑）：
- [ ] `resoz-model`：片段/音符事件（`Note { start, length, note, velocity }`）与 Clip 结构，取代/兼容步进模式
- [ ] `resoz-engine`：钢琴卷帘调度（音符按 start/length 精确触发，块边界内子块偏移或逐样本包络起点）
- [ ] `resoz-ui`：钢琴卷帘编辑器（网格绘制、音符块拖拽/绘制/删除、缩放与滚动）
- [ ] 回放联动：轨道可切换 步进模式 / 卷帘片段 两种音源

---

**给 AI 助手的特别说明**：修改音频回调路径代码时，先完整阅读 `resoz-engine/src/realtime/` 下的现有代码并遵守上述实时铁律；对架构有疑问时优先阅读 `resoz-engine` 的模块文档注释，而非自行假设。
