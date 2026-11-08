# Z0：双音频输入口 Spec

> 状态：定稿（2026-11-08）。上游：`plan.md` Z / B 系列、`spec/node-graph.md`（D2/D14/D17/D19/D23–D26）、`spec/effects.md`（§12 边界）。
> 范围：只做**基建 + 一个最小双口效果器（Ring Mod）**。侧链压缩（Z1）、Vocoder（Z2）不在本 spec。

## 1. 背景与现状

用户要「两个 audio 调制口」：侧链压缩需要外部键控，vocoder 需要 carrier / modulator 两路独立波形。现状做不到，且不是加一个效果器能解决的：

| 约束 | 代码事实 |
| :--- | :--- |
| 引擎只有一个输入缓冲 | `RtGraph::process_block` 把**所有**入边 `sum_into` / `sum_into_delayed` 到 `node.output`，再 `processor.process(ctx, &mut output)`。`Processor` 只见一个 buffer，多接一条线 = 隐式加合。 |
| 编译上限 | `GraphCompiler::compile()` 用 `self.kinds[d].max_in()` 限制入边数；`NodeKind::{Plain,Master}` = 1、`Mixer/Feedback` = 32。 |
| per-port 唯一已有 | `GraphCompiler::add_cable` 会先 `remove_cables_to_port(to, in_port)`，已保证 `(to, in_port)` 至多一条线 —— **双口不需要新机制，只需要放宽数量上限 + 校验端口范围**。 |
| 端口范围没校验 | `add_cable` / `connect_nodes` 不检查 `in_port` 的合法性，现在靠 `GraphDoc::validate` 把非 mixer 的 `in_port != 0` 拒掉。双口落地后引擎侧必须自己挡住 `in_port >= 2`。 |
| 文档与画布 | `GraphDoc::validate` 对非 mixer 要求 `in_port == 0`；`patch.rs::in_ports_screen` 除 mixer / TimelineIn 外一律返回 1 个口；`port_name_labels` 只标 mixer 通道名。 |
| 删除语义 | `GraphCompiler::remove_node` 的 `bypassable = in_count <= 1 && kind != Feedback`：双口节点只接一条线时会被旁路，但那条线可能是 **Mod 口**，旁路会把调制源错误地当成主信号直连下游。 |
| PDC | `compute_pdc` 按**所有**入边累积 `out_latency`、逐边算 `pdc_delay[slot][edge]`，与端口无关；但运行时求和要按端口分流，快路径与 `sum_into_delayed` 两条分支都要改。 |
| 实时安全 | `NodeDeployment` / `RecycledNode` 把 processor、output buffer、PDC history 在控制侧分配、经结构队列搬进音频线程、卸载时回收；音频线程零分配零释放。 |

## 2. 目标 / 非目标

**目标**：让一个内置效果器节点拥有两个语义独立的音频入口（`Main` / `Mod`），processor 能同时读到两路逐样本波形；图、文档、画布、删除、PDC、自动化全链路一致；不改单口节点的任何逐样本行为；`PROJECT_VERSION` 不动。

**非目标**：侧链压缩、Vocoder、插件效果器（CLAP/VST3）双口、任意多输入口、端口间自动降混、音频反馈环语义变更。

## 3. 决策

| ID | 决策 |
| :--- | :--- |
| Z-D1 | **`NodeKind::Dual`**：`max_in() = 2`；合法端口 `in_port ∈ {0 = Main, 1 = Mod}`，每口 ≤1 条入边（`add_cable` 已保证）。mixer / Feedback / master / Plain 语义全部不动。 |
| Z-D2 | **双口身份由效果器种类推导，文档不加字段**：`BuiltinEffectKind::descriptor().audio_inputs`（新增字段，默认 1）为唯一真源；只有 `voice == VoiceSpec::Builtin` 且 `params.kind().descriptor().audio_inputs == 2` 的 `NodeDoc::Effect` 安装为 `Dual`。`engine_sync::install_node` 与 `headless::assemble_offline` 两处 `add_node` 的 kind 决策同步。CLAP / VST3 效果器恒单口。 |
| Z-D3 | **不改 `ProcessContext`（全仓 90 处字面量构造）**。给 `Processor` 加带默认实现的方法：<br>`fn process_with_aux(&mut self, ctx: &ProcessContext, main: &mut AudioBuffer, aux: Option<&AudioBuffer>) { self.process(ctx, main) }`<br>既有 processor 零改动；runner 只对 `kinds[s] == Dual` 调它，其余仍调 `process`。 |
| Z-D4 | **整图共用一个 aux scratch**（见 §6 勘误）：`RtGraph` 新增 `aux: AudioBuffer`，`RtGraph::new(block_size)` 在控制侧分配一次（2ch × block_size）。每块处理 Dual 节点时 `aux.set_len(frames)` + `clear()`，把 `in_port == 1` 的入边求和进去，再交给 `process_with_aux`。**不进 `NodeDeployment` / `RecycledNode` / `PendingNode`**，部署与回收协议零改动；内存增量 = 1 个块缓冲。 |
| Z-D5 | **分流靠 kind，不靠 `in_port == 1`**：mixer 的 `in_port` 可能是 1（通道 1），误判会把 mixer 通道混进 aux。runner 只在 `self.plan.kinds[s] == NodeKind::Dual` 时按 `ports[i] == 1` 分流；其它 kind 全部照旧求和进 `output`。 |
| Z-D6 | **引擎侧端口范围校验**：`add_cable` 增加 `allowed_ports(kind)` —— Plain / Master / Feedback = 1（只 0），Dual = 2，Mixer = `MAX_IN_EDGES`；`in_port >= allowed` 直接 `Err(TopologyError::TooManyCables)`（沿用现有错误类型，不新增枚举）。`compile()` 的 `max_in()` 只管入边总数，两者分工不变。 |
| Z-D7 | `GraphDoc::validate` 放宽：非 mixer 节点按 `audio_inputs` 取入边上限，`in_port` 唯一且 `< audio_inputs`；mixer 规则不变；旧文件 `in_port == 0` 自然合法 → **零迁移、`PROJECT_VERSION = 10` 不动**。新增 `CableDoc::audio_to_port(from, to, port)` 构造器。 |
| Z-D8 | 画布：`in_ports_screen` 对 Dual 节点返回 2 个口，`port_name_labels` 标 `Main` / `Mod`；拉线落口复用 mixer 的「命中口优先，否则第一个空口」逻辑（`first_free_port`）；`accepts` 校验不变。 |
| Z-D9 | **删除语义修订**：`GraphCompiler::remove_node` 的 `bypassable` 增加 `kind != Dual`；`GraphDoc` 侧删除双口节点时其触及的音频线一并移除，不做旁路重连。理由同 §1 —— Mod 线没有可解释的旁路目标。 |
| Z-D10 | **PDC 两条路径同步分流**：`compute_pdc` 不改（已按所有入边累积）；`process_block` 的 `delay == 0` 与 `sum_into_delayed` 两个分支都按端口选择目标缓冲（`output` 或 `aux`）。两条入边各自按 `pdc_delay[slot][edge]` 补偿，保证 carrier / modulator 相位对齐。 |
| Z-D11 | **Ring Mod 是 Z0 的验收样例**，不是额外立项：`BuiltinEffectKind::RingMod` + `EffectParams::RingMod(RingModParams { mix })` + `realtime/ring_mod.rs`。DSP `out = lerp(main, main × mod, mix)`，逐样本、零状态、零延迟；`mix = 0` 逐样本直通（与 E 系列直通不变量一致）。`mix` 走 `EffectParamBank` 的 `BuiltinParam(0)`，沿用 `Unit::Percent` / 平滑。进 Add 菜单（`BuiltinEffectKind::ALL` 追加）。 |
| Z-D12 | Mod 口未接线时 `aux` 全零：Ring Mod 在 `mix > 0` 时输出静音，符合「调制源缺失」的物理直觉。Z1 侧链压缩则回落内部检测（其行为在 Z1 spec 定），两者不冲突。 |
| Z-D13 | **modulator 源不应再接 master**：D6 规定悬空节点照常执行、输出无人消费即静音，因此调制源只需连到 Dual 节点的 Mod 口。用户文档必须写明，否则原声会漏进主输出。 |

## 4. 每块执行序（对 `node-graph.md` §4 第 4 步的修订）

```
for s in plan.order:
    node = slots[s].take()
    node.output.clear(); node.output.set_len(frames)
    dual = plan.kinds[s] == Dual
    if dual:
        self.aux.clear(); self.aux.set_len(frames)
    for each in-edge i:
        dst = if dual && in_ports[s][i] == 1 { &mut aux } else { &mut node.output }
        delay = pdc_active ? pdc_delay[s][i] : 0
        weight = input_weights().map(|w| w[in_ports[s][i]])   // Dual 节点不用权重
        delay == 0 ? sum_into(dst, src.output) : sum_into_delayed(dst, src.history, ...)
    if dual:
        node.processor.process_with_aux(ctx, &mut node.output, Some(&aux))
    else:
        node.processor.process(ctx, &mut node.output)
    ... (history / events 不变)
```

`self.aux` 与 `self.slots` 是 `RtGraph` 的不同字段：循环前用字段拆分借用（`let Self { slots, plan, aux, pdc_delay, .. } = self;`），避免 `&mut self.aux` 与 `&self.slots[src]` 冲突。`node` 仍是 `take()` 出来的，`processor` / `output` 的 split borrow 形状不变。

## 5. 端口剖面（对 `node-graph.md` §3 表的增量）

| 节点类型 | 音频入 | 说明 |
| :--- | :--- | :--- |
| 双口效果器（内置） | 2（Main=0 / Mod=1，每口 ≤1 边） | 只见于 `audio_inputs == 2` 的内置效果器；v1 只有 Ring Mod，Z1/Z2 复用 |
| 其余 | 不变 | 乐器 0、单口效果器/master 1、mixer 1..32、Feedback ≤32 |

D14 修订为「非 mixer、非 Dual 节点 ≤1 条音频入边」；D2 的端口剖面表按上表补一行；D17 的 `in_port` 语义从「mixer 通道下标」推广为「目标输入口下标」。

## 6. 与 plan Z0 决策的勘误

| plan 决策 | spec 定稿 | 理由 |
| :--- | :--- | :--- |
| Z0-D2：`NodeSlot` 只对 Dual 节点预分配 `aux: Option<AudioBuffer>` | **Z-D4：整图一个 scratch** | 每块任一时刻只处理一个节点，aux 用完即弃，per-node 没有并行需求；改成共享后 `PendingNode` / `NodeDeployment` / `RecycledNode` / `apply_update` 四条部署回收路径**全部不用改**，内存从 ~64 个块缓冲降到 1 个，风险与改动面都更小。 |
| Z0-D4：D19 修订为「双口节点不走旁路」 | **Z-D9：`bypassable` 显式排除 `Dual`** | 原表述可误读为「只看入边数」；双口节点只接一条 Mod 线时仍不可旁路。 |
| Z0-D1：未提端口范围校验 | **Z-D6：`add_cable` 加 `allowed_ports`** | `add_cable` 只保证 per-port 唯一、不保证端口合法；`in_port = 5` 会被静默当成主输入，必须引擎侧挡住。 |

## 7. 实施切分（按文件，单 commit 内自洽）

| 层 | 文件 | 改动 |
| :--- | :--- | :--- |
| engine | `realtime/processor.rs` | `Processor::process_with_aux` 默认方法 |
| engine | `realtime/graph.rs` | `NodeKind::Dual` + `max_in` / `allowed_ports`、`RtGraph::aux` + 构造、`process_block` 分流、`add_cable` 端口校验、`remove_node` bypass 排除 Dual |
| engine | `realtime/effect/descriptor.rs` / `mod.rs` | `EffectDescriptor.audio_inputs`（默认 1，`const fn` 兼容）、`BuiltinEffectKind::RingMod` + `ALL` + `descriptor()` 分发 |
| engine | `realtime/ring_mod.rs` + `realtime/mod.rs` | `RingModProcessor`（`EffectParamBank` + `process_with_aux`） |
| model | `model/lib.rs` | `RingModParams` / `EffectParams::RingMod` / `kind()`；节点 `audio_inputs(&self)` 推导；`validate` 放宽；`CableDoc::audio_to_port`；删除节点语义 |
| ui | `headless.rs` | `spawn_effect` 的 RingMod 分支；`assemble_offline` 的 kind 决策 |
| ui | `engine_sync.rs` | `install_node` 的 kind 决策 |
| ui | `patch.rs` / `effect.rs` | 两口绘制与标签、落口、表驱动接线 |
| docs | `node-graph.md` / `effects.md` / `plan.md` / `implementation.md` / `docs/manual.md` / `AGENTS.md` | 索引与边界同步 |

`model` 与 `engine` 互不依赖：`audio_inputs` 的推导在 ui / model 两侧各自查 `BuiltinEffectKind::descriptor()`（model 不依赖 engine，所以 model 侧用 `EffectParams::kind()` 的本地匹配表，与 E 系列 `kind()` 同款做法）。

## 8. 验收

- [ ] 双口数学：`main = 常数 0.5`、`mod = 常数 -0.25` 时 Ring Mod 输出符合 `lerp` 公式；两路互换不串音；只接 Main / 只接 Mod 两种接线各自符合 Z-D12。
- [ ] PDC 非零图：给 Main 源挂一个已知 latency 的测试 processor，Mod 源无延迟 → 两路经 `sum_into_delayed` 补偿后逐样本对齐（`compute_pdc` 的 `pdc_delay` 对两条边分别生效）。
- [ ] 单口零回归：默认工程 `render` 输出与改动前**逐字节一致**；`graph_without_latency_keeps_the_fast_path` 等既有图测试全绿。
- [ ] 连线防御：`in_port = 2` 落 Dual 节点、`in_port = 1` 落 Plain 节点都被拒；`(Dual, 0)` 与 `(Dual, 1)` 各自唯一。
- [ ] 删除：双口节点删除后其 Main / Mod 线都被移除，不产生旁路直连。
- [ ] `rt_audit` 对 Ring Mod 的 `process_with_aux` 零分配；`fmt` / `clippy --workspace --all-targets -D warnings` / `test --workspace` 全绿；`--render` 无窗口冒烟。
- [ ] 用户文档：modulator 源不要接 master 的说明落 `docs/manual.md`。

## 9. 边界与明确不做

- `PROJECT_VERSION` 不动、文档零新增字段、旧工程零迁移。
- 不给 CLAP / VST3 效果器开双口（宿主侧 port 协商另立项）；不引入新 crate。
- 端口只 0 / 1 两个；不做自动降混、不做 Mod 口的 gain / pan、不做 Mod 口电平表。
- 不改 Feedback 抽头语义（D10）、不改 mixer 的 `in_port` = 通道下标语义、不改 master 单输入（D16）。
- Z1 侧链压缩、Z2 Vocoder 各自另写 spec；本 spec 不为它们预留参数槽位或 API。
