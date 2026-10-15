# Spec: 插件状态保存/加载（Plugin State Persistence）

> 状态：已定稿（2026-10-15）。对应 `plan.md` 遗留任务「插件状态保存/加载」。
> 范围：CLAP state extension（本 spec 主体）；VST3 FState 镜像跟进（第二阶段，同落点）。

## 1. 问题

CLAP/VST3 声源与效果器为图节点后，插件的内部状态（音色补丁、用户在插件参数上
做的调整）只活在会话里：重载插件、切回内置声源再切回、保存后重开工程，全部回落
插件默认值。plan.md 中已记录「滑块当前值不持久化——待 state 扩展接入」。

## 2. 核心决策

| # | 决策 | 结论 | 理由 |
| :-- | :--- | :--- | :--- |
| D1 | 状态落点 | `VoiceSpec::Clap` / `Vst3` 各加 `state: Vec<u8>` 字段 | 状态与声源规格一一绑定：换声源 = 换状态，声源删除 = 状态消失，语义自然 |
| D2 | 兼容性 | `#[serde(default, skip_serializing_if = "Vec::is_empty")]` | 旧文件无感加载，**PROJECT_VERSION 不动**（加法字段带默认值） |
| D3 | 采集时机 | `do_save` 时从活跃实例拉取写入文档 | 与 layout 同类：**保存不是编辑**，不进 undo 历史；实时跟写（参数变化即写 doc）会污染 undo 图且无收益 |
| D4 | 恢复时机 | `install_node` / `load_clap_voice` spawn 成功后，`state` 非空即 `load` | 覆盖 open / undo / redo / 换声源回退四条重建路径 |
| D5 | undo 语义 | 插件状态**不进 undo 图**（v1） | 状态 blob 对文档其余部分不可见；参数微调的 undo/redo 不回滚插件内部状态，与主流 DAW v1 行为一致 |
| D6 | 滑块回填 | 状态加载后经 params 扩展 `get_value`（raw vtable）读回当前值刷新 `clap_voice_values` | 否则 UI 滑块仍显示会话缓存/默认值，与实际发声状态脱节 |
| D7 | 采集失败处理 | 单实例 save/load 失败仅 log + 跳过，不阻断整体保存/加载 | 插件损坏不应让工程打不开；宁缺勿炸 |
| D8 | VST3 | 第二阶段镜像同落点（FState → 同一个 `state` 字段） | 宿主架构本就镜像 clap-host |

## 3. 数据流

```
保存：do_save → 遍历 voice_guards（engine_id 反查 node_map 得 doc_id）
      → ClapInstanceGuard::save_state() → project_mut() 写入 VoiceSpec.state
      → save_project 序列化（空 state 不写盘）

加载：load_project → do_open → apply_document → install_node
      → spawn_rt 成功 → voice.state 非空？load_state() : 跳过
      → read_param_values()（params get_value）→ clap_voice_values 回填滑块
```

## 4. 实现拆分

- [x] **S1** 文档（本 spec + plan.md）✅ 2026-10-15
- [x] **S2** clap-host：workspace features 加 `"state"`；`GlitcherZHostShared` 在
  `initializing()` 经 `InitializingPluginHandle::get_extension` 捕获 `PluginState`
  （OnceLock）；`GlitcherZHostMainThread` 实现 `HostStateImpl`（mark_dirty → log）；
  `ClapInstanceGuard::{save_state, load_state}`；`raw.rs` 增 params `get_value`
  裸调（复用 enumerate_params 的 vtable 模式）；测试插件实现 `PluginStateImpl`
  （amplitude 8 字节 LE 往返）+ 端到端测试（设参 → save → 新实例 load → 读回）
  ✅ 2026-10-15（commit `f71de79`）
- [x] **S3** model：`VoiceSpec` 两变体加 `state` 字段 + RON 往返测试 ✅ 2026-10-15（commit `a94af04`）
- [x] **S4** UI：`do_save` 采集（engine_id → doc_id 反查）；`install_node` /
  `load_clap_voice` 恢复 + 滑块回填；换内置声源路径确认状态随 `VoiceSpec::Builtin`
  替换自然丢弃 ✅ 2026-10-15（commit `a94af04`）
- [x] **S5** VST3 FState 镜像 ✅ 2026-10-15：`HostMemoryStream`（`IBStream` 内存流
  COM 对象，host_impl；控制线程专用）+ `Vst3InstanceGuard::{save_state, load_state,
  read_param_values}`——save 走组件 `getState`；load = 组件 `setState` + 控制器
  `setComponentState` 同步（两个消费者各一条独立流，免 seek）；测试插件工厂改逐实例
  创建（原 OnceLock 单例无法测"新实例回落默认值"），SineComponent 以 8 字节 LE f64
  持久化 Gain（`IParameterChanges` 消费 + 作用于振幅）、控制器 `setComponentState`/
  `setParamNormalized` 镜像读回（D6 载体）；UI install_node 乐器/效果器两处恢复 +
  capture 采集 + 文档写回；端到端 `state_roundtrip_restores_gain`

## 5. 边界与明确不做

- **不做**插件状态进 undo 历史（D5）。
- **不做** mark_dirty 驱动的脏标记/增量保存——每次保存全量采集，工程规模下成本可忽略。
- **不做**状态版本迁移：状态 blob 是插件私有格式，插件自己负责（CLAP 规范约定）。
- 插件缺失（重开工程时 .clap 文件不在扫描目录）：现有 fallback 路径（回落内置声源）不变，
  state 字段保留在文档中，插件找回后重新加载即恢复。
