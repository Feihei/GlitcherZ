# Spec: 内置 SF2 音色库加载器（SoundFont Loader）

> 状态：已定稿（2026-10-19）。对应 `plan.md`「新方向：内置 SF2 音色库加载器」。
> 范围：纯 Rust 方案；SF2 为主体，SF3（Vorbis 压缩采样）为第二阶段可选 feature。
> 前置调研：crates.io 检索（2026-10-19），结论见 §2。

## 1. 问题

当前声源只有两类：内置振荡器合成器（`VoiceSpec::Builtin`）与外部插件
（CLAP / VST3）。免费 SF2 音色库生态庞大（MuseScore General、TimGM6mb、
GeneralUser GS 等），是 LMMS 工作流的标配声源来源；不加载 SF2 意味着用户
要么忍受基础合成音色，要么必须安装三方插件。需要一个内置、纯 Rust、无
C 构建链依赖的 SF2 加载与合成路径。

## 2. 调研结论（crates.io，2026-10-19）

| 候选 | 纯 Rust | 版本/维护 | SF3 | 结论 |
| :--- | :--- | :--- | :--- | :--- |
| rustysynth / rustysynth-ext | 是（零依赖除 std） | 上游 1.3.6；fork ext 1.4.0（2026-09，Rust 2024） | ext 支持（feature 门控，lewton 解码） | **选定** |
| oxisynth | 是 | 0.0.5，API 不稳定 | 可选 | 行为对齐 FluidSynth 但成熟度低，不选 |
| xsynth-core | 是 | 0.4.0，活跃 | SF2/SF3/SFZ | 面向 MIDI 渲染生态，嵌入引擎要自包一层，不选 |
| fluidlite / tsf-sys | 否（C 绑定） | — | — | 违反纯 Rust 前提，排除 |

**选型决策（方案 A）：`rustysynth-ext` 锁版本 `=1.4.0`**（不用 `1.5.0-alpha`）。
理由：纯 Rust、零 unsafe、零 C 构建链要求（跨平台 CI 无系统依赖）、SF2+SF3
全覆盖、MIT、API 即 `Arc<SoundFont>` + `Synthesizer`（与本项目守卫/文档模式
贴合）。fork 单人维护的风险以锁版本缓解：MIT 允许 vendor，最坏情况本地
接管补丁（见 D4 的 voice 池）。

## 3. 核心决策

| # | 决策 | 结论 | 理由 |
| :-- | :--- | :--- | :--- |
| D1 | crate 选型 | `rustysynth-ext = "=1.4.0"`，`sf3` feature 第二阶段再开 | 见 §2；SF3 引入 lewton 依赖，默认关闭保持依赖面最小 |
| D2 | 代码落点 | `glitcherz-engine` 内两模块：`sf2`（非实时：加载/解析/preset 枚举）+ `realtime/sf2.rs`（RT 渲染 Processor） | SF2 声源是合成器，归 engine；文件 IO 与解析严禁进音频回调，必须分模块隔离 |
| D3 | 加载线程 | `SoundFont::new` 只在控制线程（UI 触发）执行，产物 `Arc<SoundFont>` 交引擎 | 解析 + 采样解码是重 IO/分配操作；与插件 instantiate 同一先例 |
| D4 | RT 安全策略 | **先审计后接入**：S1 用分配计数 allocator + 源码审计确认 `note_on`/`render` 的分配行为；若违反铁律，fork ext 预分配 voice 池（MIT 允许），不允许带分配裸进回调 | 实时铁律最高优先级；rustysynth 上游 render 内部有 voice 分配，未审计前不得接入 |
| D5 | 文档落点 | `VoiceSpec` 新增 `SoundFont { path, bank, program }` 变体 | 与 CLAP/VST3 的 `VoiceSpec` 变体同构：声源规格与状态一一绑定；采样数据不进文档（同 `.glzpreset` 先例） |
| D6 | 兼容性 | `#[serde(default, skip_serializing_if = ...)]` 加法字段，PROJECT_VERSION 不动 | 同 plugin-state spec D2 先例 |
| D7 | preset 选择 | 轨级 bank:program（u8 + u8），UI 下拉枚举自加载的 SoundFont | LMMS 工作流：轨选音色；v1 不做 drum bank 特判（bank 128 留待实测） |
| D8 | 文件丢失 | 重开工程时 SF2 不在原路径：回落内置声源，文档字段保留 | 同 plugin-state spec「插件缺失」先例，宁缺勿炸 |
| D9 | 测试 | 静音输入 → 输出静音；满幅 → 无 NaN/Inf；fixture 用 TimGM6mb.sf2（本地数据不入库，`#[ignore]` + 环境变量指定） | AGENTS.md 强制两组测试；大体积音频素材不进 git |

## 4. 数据流

```
加载：UI 文件选择 → sf2::load(path)（控制线程）
      → Arc<SoundFont> + preset 枚举 → 经命令下发引擎持有

播放：轨声源 = VoiceSpec::SoundFont{path, bank, program}
      → realtime::sf2::Sf2VoiceProcessor（图节点，接 MIDI 入边）
      → note_on/off 经无锁结构命令下发（同 Builtin 声源路径）
      → render 进图求和循环 → master 输出
```

## 5. 实现拆分

- [ ] **S1** RT 安全审计 spike：分配计数 allocator 包 `Synthesizer::note_on`/`render`
  单音符与和弦场景，产出审计结论写入本节；决定是否需要 voice 池 fork
- [ ] **S2** engine `sf2` 模块（非实时）：load / preset 枚举 / 错误类型（thiserror）
- [ ] **S3** engine `realtime/sf2.rs`：`Sf2VoiceProcessor` 实现 `Processor`，预分配缓冲；
  逐行审查进回调路径（铁律评审）
- [ ] **S4** model：`VoiceSpec::SoundFont` 变体 + RON 往返测试
- [ ] **S5** UI：音色库面板（加载 SF2/SF3、preset 下拉、轨声源绑定）+ undo/redo 路径
  走现有守卫/命令约定
- [ ] **S6** SF3 feature 开启 + 端到端验收（含 S1 审计复跑）

## 6. 边界与明确不做

- **不做** DLS / SFZ 格式（xsynth-soundfonts 支持但超出内置音源库 v1 范围）。
- **不做** SF2 编辑器、采样提取/导出、用户采样导入。
- **不做** drum bank（bank 128）特判与 GM 重映射（v1 直通 bank:program，实测后定）。
- **不做** 多 SoundFont 同时加载（v1 单库全局共享，多库留待实测需求）。
