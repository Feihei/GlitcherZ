# 用户波表导入（Wavetable Import）

> ID: F-043 · 域: VOICE / PROJ
> 状态：设计（2026-11-11）
> 关联：`wavetable-instrument.md`（S3-D2/D3/D10）、`media.md`

## 1. 问题

Wavetable 只有 4 个内置帧（Sine / Triangle / Saw / Square），用户无法导入自己的单周期波形。

## 2. 决策

| ID | 决策 |
| :--- | :--- |
| D1 | model：`WavetableParams.custom: Option<WavetableTable { path, name, frames }>`；路径走媒体相对路径（`media.rs`），空 / 缺失 = 内置表。 |
| D2 | 只支持**单周期 mono WAV**：长度 2 的幂 64..8192，16/24-bit int 与 32-bit float；多周期文件按首个 `size` 截断并提示。 |
| D3 | 自定义帧作为内置 bank 的**第 5 帧**：position 0..1 映射 5 帧（帧 4 = 自定义）；无自定义时仍是 4 帧，旧行为逐样本不变。 |
| D4 | 非实时解析 + 生成 9 层带限表（复用 S3-D2/D3 的谱生成与 mipmap 逻辑）；经 `GraphUpdate` 把 `Arc<WavetableBank>` 交给节点，实时线程只读，零分配零锁。 |
| D5 | UI：Inspector 的 wavetable 分区加 `Load table…` / `Clear`；缺失文件回退内置并提示；`voices.rs` 的 Wavetable 分支补文件选择。 |
| D6 | 不做 `.wt` / Serum 多帧格式、命名库、缓存淘汰、多表 bank。 |

## 3. 里程碑

| 里程碑 | 内容 | 依赖 | 规模 |
| :--- | :--- | :--- | :--- |
| **M1** 表数据 | model 字段 + 单周期 WAV 解析 + 带限 bank 构造 + 单元测试 | — | 中 |
| **M2** 引擎加载 | 动态 `Arc` bank + 加载命令 / `GraphUpdate` + 实时只读 | M1 | 中 |
| **M3** UI / 回退 | 文件对话框 + 缺失回退 + 文档 | M1、M2 | 小 |

## 4. 验收

- 导入单周期 WAV 后音色可听、`position` 可扫到自定义帧；缺失文件回退内置。
- 旧工程（无 `custom`）逐样本不变；实时零分配、零锁；`PROJECT_VERSION` 加 1 且无迁移（纯加法）。