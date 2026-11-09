# S4：CLI 命令面 Spec（Batch Commands）

> 状态：**已交付（2026-11-09）**。上游：`plan.md`（Y 系列 S4，执行顺序第 6 项）、`spec/node-graph.md`、`spec/editing-model.md`（命令注册表的原始设计）。
> 范围：**把已有的 `glitcherz-command` 注册表从 CLI 打通**——批量跑命令、读结果、存盘、渲染，一条命令都不新增。不做参数 / 节点编辑词汇，不做交互式会话，不做 MCP server。

## 1. 背景

`plan.md` 给 S4 写的立项理由是：「S4 的价值不在「多几个子命令」，而在于 S0 的判别：没有命令面的 render 只是「把工程转成 wav」，有命令面之后 MCP 才能**改**工程而不是只能**读**。」spec 阶段要先回答三个问题：命令面边界、与 `load_project` / `save_project` 的关系、失败语义。

读完代码，前两个问题的答案比预想的乐观：

| 事实 | 位置 | 意义 |
| :--- | :--- | :--- |
| **命令面已经存在** | `crates/glitcherz-command/src/`（1786 行） | 16 条 `midi.*` 命令：`query` / `note_add` / `note_edit` / `note_delete` / `notes_move` / `notes_duplicate` / `notes_edit_delta` / `notes_scale` / `notes_split` / `quantize` / `transpose` / `velocity_op` / `change_velocity` / `duration_op` / `randomize` / `strum` |
| **事务语义已经是免费的** | `Session::execute(id, &Value) -> Result<Value>` | 执行前取 `Arc<Project>` 快照，出错或 panic 就回滚；undo 快照是 `Arc` 克隆（O(1)）；journal 记 `(id, params)`——注释里写的就是「macros and **agent scripting**」 |
| **不需要 UI** | `Session::new(Project)` | `Session` 不持任何 egui 类型，构造只要一个文档 |
| **参数字已经是 JSON** | `CommandSpec::run: fn(&mut Session, &serde_json::Value)` | `serde_json 1.0.151` 早在 lock 文件里（`glitcherz-command` 与 `glitcherz-ui` 都直接依赖），不是新增依赖 |
| **注册表可枚举** | `command_specs()` / `CommandSpec::{id, label, menu, params, undoable, journal, enabled}` | `CommandInfo` 已经是 `Serialize`，`--commands` 几乎白拿 |
| **工程读写与 UI 无关** | `glitcherz-model/src/file.rs`：`load_project` / `save_project` | 纯函数，CLI 直接调模型层即可，不需要抽 `headless`（plan 里的第二个问题到此为止） |

所以 S4 不是「发明一套 ops 语言」，而是**给已有注册表装一个 CLI 外壳**。这也让 S4 的规模从「设计词汇表」缩到「接线 + 失败语义 + 输出格式」。

真正缺的只有三样：

1. **一个入口**：今天 `glitcherz` 只有 `--render`（一次性把工程转 wav），没有「载入 → 跑命令 → 存盘」这条路。
2. **一个枚举口**：agent 想知道有哪些命令，只能读 Rust 源码。
3. **一套失败语义**：今天所有失败都是 `ExitCode::FAILURE`（1），机器分不清「参数写错」和「命令不接受这个编辑」。

## 2. 目标 / 非目标

**目标**：`glitcherz` 在无窗口状态下能 ① 枚举命令面；② 对一份工程按序执行一批命令；③ 把结果存回 `.glz`；④ 把**编辑后的内存文档**直接渲染成 wav；⑤ 用固定退出码 + 可选 JSON 信封把结果交给程序。全程不建窗口、不开音频设备、不引入新 crate。

**非目标**：

- **不新增任何命令词汇**（S4-D1）。参数 / 节点编辑（`graph.set_param`、`graph.add_node`、`graph.set_cable`）需要先给 `NodeParam` 的 ~40 个变体定一套稳定字符串名（今天只有 `patch.rs` 里的 UI 显示名 "Attack" / "8'"），那是一份独立设计，另开 spec。
- 不做交互式会话（stdin 逐条读命令）、不做 `--watch`、不做常驻进程。MCP server 是 S5。
- 不做撤销 / 重做**暴露**：CLI 一次调用就是一个事务，undo 栈在进程退出时丢了，对外没有意义。
- 不做 Save As 的媒体重定位（`media.md` 的 `collect` / `relocate`）；`--save` 只按文档字面写盘，相对媒体路径仍指向原工程目录。
- 不改 `PROJECT_VERSION`、不改工程文件格式、不动 GUI 行为、不动 `--render` 的既有语义与逐字节输出。

## 3. 决策

| ID | 决策 |
| :--- | :--- |
| S4-D1 | **命令面 = 现有注册表，一行词汇都不加。** CLI 只负责「把 `Project` 装进 `Session`、按序 `execute`、把 `Value` 交出去」。新增词汇（尤其 `graph.*`）是独立设计：它要定稳定参数名、要定「节点用什么标识」（`node_id` 是文档内自增数，跨工程不稳定）、要定 `add_node` 的默认放置与端口校验——这些都会把 S4 的 spec 撑大一圈，且与「接线」这件事无关。 |
| S4-D2 | **不新建 crate，不新增外部依赖。** `glitcherz-app` 增加三个依赖：`glitcherz-model`（当前只在 `dev-dependencies`，提升为常规依赖）、`glitcherz-command`、`serde_json = "1"`。三者都已在 lock 文件里（`serde_json 1.0.151`，`glitcherz-command` 与 `glitcherz-ui` 早已直接依赖）。`glitcherz-app` 仍不依赖 `glitcherz-ui` 之外的重型栈。 |
| S4-D3 | **命令行形态**：`--open <in.glz>` 提供文档；`--ops <file\|->` 与可重复的 `--do <id> [--params <json>]` 提供命令序列；`--save <out.glz>` 与 `--wav <out.wav>` 是可选终点。**argv 顺序即执行顺序**：`--ops` 在解析期只记路径、不读文件，执行期按它在 argv 中的位置拼接，因此 `--do a --ops list.json --do b` 的次序是 `a, list…, b`。 |
| S4-D4 | **没有 `--open` 的批次从 `Project::default()` 起**，等价 GUI 的「新建工程」。这条让 `--ops` + `--save` 成为「从零造一份工程」的路径，也让 MCP 不必先准备模板工程。`--save` / `--wav` 都不给 = 纯 dry-run，不落任何文件（跑完只打印结果）。 |
| S4-D5 | **批次是全或无的**：任一命令失败 → 立即停止、**不写 `--save` 也不写 `--wav`**、退出 4。不必手工回滚：文件只在全部命令成功之后才写，天然原子。这与 `Session::execute` 的「一条命令不能留下半个编辑」是同一个承诺，只是粒度从命令提到批次。 |
| S4-D6 | **`--wav` 渲染的是编辑后的内存文档，不是重读磁盘。** 为此把 `headless::render_project_to_wav` 拆成两半：`load_project` + 新的 `render_doc_to_wav(&Project, output, options)`。`render_project_to_wav` 变成「载入 + 调它」，行为逐字节不变（S0 的零回归与对拍测试保持）。如果没有这一步，「改完渲染」就变成「渲染改之前的文件」，整个闭环是假的。 |
| S4-D7 | **`--render <in> <out>` 保持原样且与批次互斥。** 它自带输入路径，与 `--open` 语义重复；两者同时出现按参数错误处理（退出 2），报错信息直接指路 `--wav`。**不做按参数个数猜语义**（`--render` 一个参数时表示输出），那种解析方式迟早咬人。`--rate` / `--bits` / `--to` 两种模式共用。 |
| S4-D8 | **输出：人读默认 + `--json` 信封。** 人读模式每条命令一行 `ok  <id>  <result>`，末尾一行汇总；`--json` 时 stdout **只有一个** JSON 对象（`env_logger` 写 stderr，不污染 stdout）。失败时同样输出信封（`ok: false` + 失败项），人类可读的错误文本另写一份到 stderr。 |
| S4-D9 | **固定退出码**：`0` 成功 / `1` 引擎或窗口启动失败（沿用现状）/ `2` 参数或输入文档错误（含 `--params` 的 JSON 语法、`--ops` 文件读不到或不是合法 JSON 数组）/ `3` 工程文件读写或解析失败、渲染失败 / `4` 命令执行失败（未知 id、当前不可用、参数不合法、panic）。 |
| S4-D10 | **`--commands` 枚举注册表。** JSON 模式输出 `CommandInfo` 数组；人读模式一行一条 `id  [menu]  params`。`enabled` 用 `--open` 的工程求值（不给 `--open` 就用默认工程），所以 agent 能直接看出「为什么这条命令现在不可用」（`disabled_reason`）。 |
| S4-D11 | **写盘复用 `save_project`**：走 `<name>.glz.tmp` + rename 的原子写、保留 `<name>.glz.bak`、写前 `validate()`。CLI 不为了「干净」另写一条裸 `fs::write` 路径——同一份工程被 GUI 和 CLI 交替保存时，两套语义最容易出事。`.bak` 的副作用在文档里写明。 |
| S4-D12 | **模块切分**：`main.rs` 只留 `main` + 分派；新增 `args.rs`（解析 + 单测）、`batch.rs`（ops 载入、命令执行、信封构造——纯函数为主，可单测）、`render_cmd.rs`（`--render` 与 `--wav` 的共用渲染入口）。现 `main.rs` 只有 257 行，直接长到 700 行会毁掉可读性。 |
| S4-D13 | **`--ops` 的文档格式就是 `[{id, params}]`**，`params` 可省（等同 `{}`）。不接受注释、不接受 `--ops` 里再嵌 `--ops`。多一层 DSL 就是多一层要维护的解析器，而它省的字符在 JSON 面前不值一提。 |

## 4. 命令行语法

```
glitcherz [--open <in.glz>] (--ops <file|-> | --do <id> [--params <json>])…
          [--save <out.glz>] [--wav <out.wav>]
          [--rate <hz>] [--bits 16|24|32f] [--to <ticks>] [--json]

glitcherz --commands [--open <in.glz>] [--json]

glitcherz --render <in.glz> <out.wav> [--rate <hz>] [--bits 16|24|32f] [--to <ticks>]
```

| 参数 | 说明 |
| :--- | :--- |
| `--open <in.glz>` | 起始文档。缺省 = `Project::default()` |
| `--ops <file\|->` | 读一个 JSON 数组 `[{"id": "...", "params": {…}}]`；`-` 表示 stdin。可出现在 argv 任意位置，执行顺序按位置 |
| `--do <id>` | 追加一条命令，params 缺省 `{}` |
| `--params <json>` | 给**紧邻的上一条** `--do` 指定参数对象；没有前置 `--do` 是参数错误 |
| `--save <out.glz>` | 全部命令成功后写盘（原子写 + `.bak` + `validate`） |
| `--wav <out.wav>` | 全部命令成功后渲染**内存文档**（可设 `--rate` / `--bits` / `--to`） |
| `--commands` | 枚举注册表；与批次互斥 |
| `--json` | stdout 输出单个 JSON 对象；人读模式默认 |

`--ops -`（stdin）与 `--do` 的区别只是入口，拼进同一条序列里按 argv 位置执行。

## 5. 执行流程

```
parse_args
  ├─ 参数错误 ──────────────────────────────────► exit 2
  ├─ --commands ──► 载入(--open 或默认) ──► 枚举 ──► exit 0
  └─ 批次
       project = --open ? load_project : Project::default()   ← 失败 exit 3
       session = Session::new(project)
       for step in steps:                    ← steps 由 --ops / --do 按 argv 顺序拼出
           params = step.params (JSON)
           match session.execute(id, &params)
             Ok(value)  → 记录 (id, ok, value)
             Err(err)   → 记录失败项，打印，exit 4（不写任何文件）
       if --save: save_project(session.project(), out)        ← 失败 exit 3
       if --wav:  render_doc_to_wav(session.project(), out)   ← 失败 exit 3
       打印信封（stdout）→ exit 0
```

要点：

- **`step` 的解析推迟到执行期**：`--ops` 的文件在轮到它时才读，所以错误信息能带上文件名；`--params` 的 JSON 在 argv 解析期就校验（它本来就是命令行文本，早失败早说）。
- **不做增量写盘**：一条命令都不落盘，`--save` 只在最后写一次。因此「跑一半崩了」不会留下半个文件。
- **`Session` 的 undo / journal 不输出、不修剪**：一次调用的历史最多几十步，`DEFAULT_HISTORY_LIMIT`（200）与 `DEFAULT_JOURNAL_LIMIT`（500）都不会触发；但**不调 `set_history_limit(1)`**——少一次特例就少一处与 GUI 行为分叉的地方。

## 6. 输出与退出码

### 6.1 人读模式

```
loaded working_files/regression-project.glz
ok  midi.transpose  {"transposed":4}
ok  midi.quantize   {"quantized":4}
saved working_files/out.glz
rendered 44288 frames at 22050 Hz to working_files/out.wav
```

失败时（`<id>` 不重复：每个 `CommandError` 的 `Display` 里已经带了命令名）：

```
ok  midi.query      {"notes":[…]}
error  midi.transpose: no clip 0 on track 0
```

人读模式下错误行进 **stdout** 的这段转录（它是一条结果，不是用法错误）；只有参数 / 输入文档层面的错误（退出 2）才直接写 stderr。

### 6.2 `--json` 信封

成功：

```json
{
  "ok": true,
  "project": "regression-project.glz",
  "commands": [
    { "id": "midi.transpose", "ok": true, "result": { "transposed": 4 } }
  ],
  "saved": "working_files/out.glz",
  "render": { "path": "working_files/out.wav", "frames": 44288, "sample_rate": 22050 }
}
```

失败：

```json
{
  "ok": false,
  "project": "regression-project.glz",
  "commands": [ { "id": "midi.query", "ok": true, "result": { "notes": [] } } ],
  "failed": { "stage": "command", "id": "midi.transpose", "kind": "unavailable",
              "error": "midi.transpose is unavailable: there are no notes to edit" }
}
```

`failed.stage` 是失败发生的阶段：`ops`（`--ops` 读不到或格式错）/ `load` / `command` / `save` / `render`。只有 `command` 才有 `id` 与 `kind`——前四个阶段还没轮到某条命令。

- `saved` / `render` 在没给对应终点时**整个键不出现**，而不是 `null`——消费方用「键在不在」判断，比判空更不容易出错。
- `kind` 是 `CommandError` 的变体名小写：`unknown` / `unavailable` / `bad_params` / `panicked`。MCP 靠它区分「你写错了 id」和「这条命令现在不适用」。
- `--json` 时 stdout 不混任何非 JSON 文本；日志与失败摘要走 stderr（`env_logger` 默认行为），所以终端里既拿到纯 JSON 又看得见错在哪。

### 6.3 退出码

| 码 | 含义 | 典型原因 |
| :--- | :--- | :--- |
| `0` | 成功 | — |
| `1` | 引擎 / 窗口启动失败 | 沿用现状，仅 GUI 路径 |
| `2` | 参数或输入文档错误 | 未知 flag、`--params` 不是 JSON 对象、`--ops` 读不到或不是数组、`--render` 与批次同时出现 |
| `3` | 工程文件或渲染失败 | `load_project` / `save_project`（含版本过新、`validate` 失败）、`--wav` 渲染失败 |
| `4` | 命令执行失败 | `unknown` / `unavailable` / `bad_params` / `panicked` |

`--render` 的失败码从 `1` 改为 `3`（它现在和批次共用 `3`）。这是**唯一一处对外行为变更**，在 `docs/manual.md` 与 `README` 里写明。

## 7. 改动面

| 文件 | 改动 |
| :--- | :--- |
| `crates/glitcherz-app/Cargo.toml` | 加 `glitcherz-model`（从 dev 提升）、`glitcherz-command`、`serde_json`；`Cargo.lock` 只多出两条依赖边，**没有新的外部 crate** |
| `crates/glitcherz-app/src/main.rs` | 只留 `main` + 分派（GUI / `--render` / `--commands` / 批次 / `--list-devices`） |
| `crates/glitcherz-app/src/args.rs`（新） | 现有 `parse_args` 迁入 + 新 flag；`BatchStep` / `Mode` 枚举；单测 |
| `crates/glitcherz-app/src/batch.rs`（新） | `load_ops`、`run_batch(&Project, &[BatchStep]) -> Report`、`Report → 人读 / 信封` |
| `crates/glitcherz-app/src/render_cmd.rs`（新） | `--render` 与 `--wav` 共用的选项解析（`--bits` / `--rate`）与调用 |
| `crates/glitcherz-ui/src/headless.rs` | 拆出 `render_doc_to_wav(&Project, …)`；`render_project_to_wav` 改为载入 + 调用 |
| `crates/glitcherz-app/tests/batch_cli.rs`（新） | 真起子进程：改音符 → 存盘 → 重新载入核对；失败批次不写文件；`--json` 信封形状；退出码矩阵 |
| `docs/manual.md` | 新增「命令行：批量命令」小节 + 退出码表；`--render` 失败码说明 |
| `docs/README.md` | 运行节的参数表加 `--commands` / `--ops`，并补一段「命令行改工程」与退出码概述（根 `README.md` 是介绍性文档，不放 CLI 细节） |

**不动**：`glitcherz-command`（一行不改，这是本 spec 的核心约束）、`glitcherz-engine`、`glitcherz-model`、`PROJECT_VERSION`、任何 `realtime/` 代码。

## 8. 验收

> 全部回填于实施完成时；括号里是对应的测试名。共 21 条，19 条对应用例 + 零回归 + 门禁。

- [x] `--commands` 列出全部 16 条命令，JSON 模式下每项有 `id` / `label` / `params` / `enabled`（`commands_lists_the_registry`）。实测 16 条，`midi.query` 的 `params` 为 `{track, clip}`。
- [x] `--commands` 的 `enabled` 随 `--open` 变化：默认工程下 `midi.change_velocity` 为 `false` 且 `disabled_reason` 是 "there are no notes to edit"；换成带音符的工程后变 `true`、`disabled_reason` 为 `null`（`commands_gate_on_the_open_project`）。**注**：注册表里只有 `midi.change_velocity` 用了 `has_notes` 谓词，其余 15 条都是 `always`——所以这条只能用它验。
- [x] `--do midi.query` 对已知 clip 输出音符数组，人读与 JSON 两种模式都可用（`query_returns_the_notes`）。
- [x] **批次的顺序就是 argv 顺序**：`--do note_add --ops list.json --do transpose` 与一次 `--ops` 写死三步，最终 `midi.query` 的返回值完全相同（`argv_order_is_execution_order`）。实测 60 → 72、72 → 84。
- [x] `--ops -` 从 stdin 读同一份数组，效果与文件等价（`ops_reads_stdin`）。
- [x] `--params` 不是合法 JSON → 退出 2；是 JSON 但不是对象 → 退出 2；前面没有 `--do` → 退出 2（`bad_params_json_exits_two` / `params_needs_a_do` / `args` 单测）。
- [x] `--ops` 指向不存在的文件 → 退出 2 且消息带文件名；内容不是数组 → 退出 2 且提示 "must be a JSON array"（`bad_ops_file_exits_two`）。
- [x] **改完存盘再读回来，改动还在**：`note_add` + `transpose` + `--save`，重新 `load_project` 后 2 个音符、音高 `[72, 79]`（`batch_edits_survive_a_save`）。
- [x] **无 `--open` 时从默认工程起**：`--save` 产出可被 `load_project` 读入的合法工程（`saving_without_open_writes_a_default_project`）。
- [x] **新工程的轨道上没有 clip**，所以 `--do midi.note_add` 退出 4、不写文件——这是 §9 记录的已知限制，专门用一条测试钉住（`a_new_project_has_no_clip_to_edit`）。
- [x] **失败批次不写任何文件**：默认工程上 `--do midi.transpose --save` → 退出 4，目标 `.glz` 不存在（`a_failed_batch_writes_nothing`）。
- [x] 未知命令 id → 退出 4、`failed.stage = "command"`、`kind = "unknown"`、不写文件（`unknown_command_exits_four`）。
- [x] **`--wav` 渲染的是编辑后的文档**：`transpose +12 + --save + --wav` 的 wav 与「`--render` 那份存盘文件」**逐字节一致**，与「不跑命令直接 `--wav`」不同（`wav_renders_the_edited_document`）。
- [x] `--render` 与 `--ops` / `--do` 同时出现 → 退出 2，报错提到 `--wav`（`render_and_batch_are_exclusive`）。
- [x] `--render` 既有行为不变：`render_cli.rs` 的 `render_subcommand_writes_a_wav_without_a_window` 保持绿。
- [x] 工程文件版本过新（把 `version: 10` 改成 99）→ 退出 3，消息含 `v99`（`load_failure_exits_three`）。
- [x] `--json` 时 stdout 可被 `serde_json` 整体解析；`saved` / `render` / `failed` 键在对应情况发生前**不存在**（`json_envelope_shape`）。
- [x] `--save` 走原子写并留下 `.bak`：连存两次后 `.glz.bak` 存在、`.glz.tmp` 已被 rename 掉（`save_keeps_the_atomic_semantics`）。
- [x] 非命令阶段失败带 `stage`：`ops` / `load` 都没有 `kind`，`command` 才有（`the_envelope_names_the_stage_for_non_command_failures` + 退出码矩阵单测 `exit_codes_match_the_documented_matrix`）。
- [x] **零回归**：默认工程 `--render` 与 S4 之前**逐字节一致**（detached worktree 对比，见 §10）。
- [x] `fmt` / `clippy --workspace --all-targets -D warnings` / `test --workspace`（**1 245 项**）/ `build --workspace` 全绿。

## 9. 风险与已知取舍

| 风险 | 应对 |
| :--- | :--- |
| **`editing-model.md` 的命令是给「编辑器里的一个选区」设计的**（大量命令靠 `indices` 选音符，缺省才作用于全部） | 这正是它可用的原因：缺省 = 全 clip。但**索引是位置**，`note_add` 之后索引会变，所以批次里的 `indices` 必须由调用方自己按顺序算清。写进手册，不加「命名选区」这种额外概念。 |
| 命令面只有 `midi.*`，改不了音色 | 明确写在非目标里：S4 交付的是**运输层**，不是词汇表。`graph.*` 与稳定参数名另开 spec（见 S4-D1）。在那之前，改音色仍然开 GUI。 |
| `--save` 的 `.bak` 在脚本里会堆文件 | 照 `save_project` 的语义走（S4-D11）。MCP 侧应写新文件而不是反复覆盖同一份；这条写进 S5 的安全边界。 |
| JSON 信封是**新**的对外契约，一旦发布就得稳 | 键名取最保守的一套（`ok` / `commands` / `failed` / `kind`），不塞内部字段（`revision` / `journal` / `history`）。`kind` 用 `CommandError` 的变体名，变体改名就是破坏性变更。 |
| 一次跑几十条命令时没有进度 | 不做。批次的量级是「几十步、毫秒级」，加进度条只会让输出不再是纯净的 JSON。真要长任务，那是有音频渲染的场景，`--wav` 本身会打印帧数。 |
| 失败即停，不能「跳过坏命令继续」 | 刻意不做。批次的语义是**一次编辑**，跳过坏命令等于带着未知状态继续写文件；需要容错就由调用方拆成多次调用。 |
| **新工程的轨道上没有 clip**，而 16 条命令全都作用于既有 clip | 所以「从零造一首曲子」仍需要一份模板工程。这是词汇表的问题，不是运输层的问题：`clip.add` 一类命令是下一个自然增量，且它连同 `--open` 缺省值一起才真正解决「从空开始」。暂记为已知限制，用 `a_new_project_has_no_clip_to_edit` 钉住现状。 |
| `--commands` 的 `enabled` 有 15/16 恒为 `true` | 注册表里只有 `midi.change_velocity` 挂了 `has_notes`。所以 `--commands` 的主要价值是**发现 id 与参数**，不是「预览可用性」；别指望它能告诉你某个编辑在语义上合不合适（那要等命令真正跑起来才知道，错误码 4 + `kind = "bad_params"`）。 |
| `Session` 的 panic 兜底把 panic 变成错误 | 已经是现状（`catch_unwind` + 回滚），CLI 退出码 `4` + `kind = "panicked"`。**不要**在 CLI 里再套一层 `catch_unwind`。 |

## 10. 交付与零回归证据

### 10.1 落地

| 文件 | 内容 |
| :--- | :--- |
| `crates/glitcherz-app/src/args.rs`（新，396 行） | `StepSource`（`Inline` / `Ops`）、`Mode`（Help / ListDevices / Commands / Render / Batch / Gui）、`Args`；**互斥规则只在 `Args::mode()` 一处判断**，不在各调用点重复。11 项单测覆盖 `--params` 与前置 `--do` 的绑定、`--ops` 的 argv 位置、互斥报错、`--help` 的最高优先级 |
| `crates/glitcherz-app/src/batch.rs`（新，450 行） | `parse_ops` / `load_ops`（含 `-` 读 stdin）/ `resolve` / `run` / `Report` / `Failure`（带 `stage` 与 `exit_code`）。8 项单测覆盖 ops 文档的合法与非法形状、argv 顺序、失败即停、信封键的「缺席非 null」、退出码矩阵 |
| `crates/glitcherz-app/src/render_cmd.rs`（新，72 行） | `--bits` 解析与 `RenderArgs → RenderOptions`，`--render` 与 `--wav` 共用 |
| `crates/glitcherz-app/src/main.rs`（279 行） | 只留 `main` + 六个模式的分派；`batch_command` 的编排（load → resolve → run → save → render）与打印 |
| `crates/glitcherz-app/tests/batch_cli.rs`（新，614 行） | 18 项真子进程用例，覆盖 §8 除零回归与门禁外的全部条目 |
| `crates/glitcherz-ui/src/headless.rs` | `render_project_to_wav` 拆成「载入 + `render_doc_to_wav`」，后者吃 `&Project`；前者行为逐字节不变 |
| `crates/glitcherz-app/Cargo.toml` + `Cargo.lock` | 加 `glitcherz-model` / `glitcherz-command` / `serde_json`；lock 只多两条依赖边 |
| **未动** | `glitcherz-command`（一行没改，这是本 spec 的核心约束）、`glitcherz-engine`、`glitcherz-model`、`PROJECT_VERSION = 10`、任何 `realtime/` 代码 |

### 10.2 零回归硬证据

用 `8c3ec61`（S4 spec commit，最后一个 pre-S4 代码状态）的 detached worktree + 独立 `CARGO_TARGET_DIR` 编出 pre-S4 二进制，与 post-S4 对同一份含 4 个音符的工程跑 `--render --rate 22050 --bits 16 --to 1920`：

| | pre-S4（`8c3ec61`） | post-S4 |
| :--- | :--- | :--- |
| 帧数 | 44 288 | 44 288 |
| 字节数 | 177 196 | 177 196 |
| `cmp` | — | **逐字节一致** |

「`--wav` 渲染的是内存文档」这一条另有独立证据：同一份工程 `transpose +12` 后 `--save` + `--wav`，与前一次运行 `--render` 那份存盘文件**逐字节一致**，而与不跑命令的 `--wav` 不同。

### 10.3 门禁

| 项 | 结果 |
| :--- | :--- |
| `cargo fmt --all` | 干净 |
| `cargo clippy --workspace --all-targets -- -D warnings` | **0 警告** |
| `cargo test --workspace` | **1 245 项通过，0 失败**（S3 交付时 1 208，本阶段新增 39：21 单测 + 18 子进程） |
| `cargo build --workspace` | 通过 |

