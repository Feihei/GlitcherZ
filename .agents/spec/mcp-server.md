# S5：MCP Server Spec

> 状态：**已交付（2026-11-09）**。上游：`plan.md`（Y 系列 S5，执行顺序第 6 项的后半）、`spec/cli-commands.md`（S4：命令面与失败语义）、`spec/node-graph.md`。
> 范围：**在无窗口进程里把「打开 → 改 → 存 → 听」暴露成一个 MCP server**。不做命令词汇扩容（`NodeParam` 稳定名、`clip.add` 另排一期），不做 resources / prompts / sampling / progress，不做 HTTP / SSE 传输。

## 1. 背景

`plan.md` 2026-11-07 立项时给 MCP 记了四条硬约束。S4 交付之后，其中两条已经变成事实：

| 立项时的判断 | 2026-11-09 的状态 |
| :--- | :--- |
| 需要**命令面**（`glitcherz-command` 只覆盖模型编辑） | ✅ 已就位。16 条 `midi.*` 命令 + 事务语义 + 失败分类（`unknown` / `unavailable` / `bad_params` / `panicked`） |
| 需要**运行中工程的访问方式** | ✅ 已定：独立 headless 进程持有一份 `Session`，不做 IPC 到 GUI |
| **没有 render 的 MCP 是盲操作** | ✅ 已就位。`render_doc_to_wav(&Project, …)` 渲染内存文档，S4 已用双断言验过 |
| 需要 **headless session 层** | ❌ 仍是 `glitcherz-ui::headless`（`pub(crate)` 为主，只导出渲染两函数） |
| 需要 **安全边界**（写文件、插件执行第三方代码） | ❌ 空白。`--render` / 批次都假设「调用者是自己人」 |

所以 S5 的实质工作是三件：**协议外壳 + 会话状态 + 权限边界**。命令面一行不改。

### 1.1 关于「提升为 `glitcherz-session` crate」

`plan.md` 把这条写成 S5 的前置。S0-D1 的原话是「S5 真需要独立 server 进程时再原样提升」。现在到了要判断的时刻，结论是**不提升**，理由在 S5-D1。

## 2. 目标 / 非目标

**目标**：`glitcherz --mcp` 在 stdio 上讲 MCP（JSON-RPC 2.0），暴露 6 个工具，让客户端能 ① 打开工程并读到摘要；② 枚举命令面；③ 按序执行一批命令（可 dry-run）；④ 存盘；⑤ 渲染 wav；⑥ 全程受 `--root` 沙箱与「默认禁用插件」约束。零新增 crate、零新增外部依赖、不建窗口、不开音频设备。

**非目标**：

- **不新增任何命令词汇**（S5-D14，用户 2026-11-09 拍板）。`graph.*` / `clip.add` / `NodeParam` 稳定字符串名是独立一期——它要定命名规范、要迁移 40+ 参数、要决定 `node_id` 跨工程不稳定怎么办。塞进 S5 会把「协议 + 安全」撑成三份设计。
- **不暴露 undo / redo / history**（S5-D15）。`Session` 有这些 API，但「客户端什么时候该撤销」没有好答案；错批次的恢复路径是重新 `project_open`。
- 不做 resources / prompts / sampling / roots / logging / progress 通知。`capabilities` 只声明 `tools`。
- 不做 HTTP / SSE 传输，不做多客户端，不做认证（本地 stdio，进程即信任边界）。
- 不改 `PROJECT_VERSION`、不改工程文件格式、不动 GUI、不动 `--render` / `--ops` / `--commands` 的既有行为与逐字节输出。

## 3. 决策

| ID | 决策 |
| :--- | :--- |
| S5-D1 | **不新建 crate，MCP = `glitcherz-app --mcp` 第四种 headless 模式。** 三条理由：① 「独立进程」这个真需求已满足——客户端 spawn 的就是独立进程，`--mcp` 与 GUI 不共享任何内存；② `headless` 的 `spawn_node` / `Media` / `SpawnInfo` 与 UI 的 `install_node` 缓存深度耦合（`AppMedia` 就是 `&mut GlitcherZApp`），外迁成独立 crate 是一次大重构且**没有第二个消费者**；③ MCP 与 `--render` / `--ops` 共用 `Session`、`render_doc_to_wav`、`save_project`，同一份代码。代价是 MCP 进程仍链接 egui/winit 编译产物，但 `--mcp` 不初始化它们，只多几 MB 常驻。 |
| S5-D2 | **不新增外部依赖，JSON-RPC 手写。** MCP 的 stdio 传输就是「一行一个 JSON 对象」，`serde_json` 已在依赖里。为此引入 `rmcp` / `jsonrpc-core` 之类只为省 200 行，却把协议版本、错误语义、握手时序全部交给第三方——而这三样正是本期要定死的东西。 |
| S5-D3 | **传输只有 stdio**，newline-delimited。stdout **只**写协议响应（一行一个 JSON、不 pretty-print），所有日志走 stderr（`env_logger` 默认行为）。stdin EOF → 正常退出 0。这是硬约束：任何 `println!` 混进 stdout 都会让客户端解析立即失败。 |
| S5-D4 | **协议版本**：支持集 `2024-11-05` / `2025-03-26` / `2025-06-18`。客户端请求的版本在集合内 → 原样回显；不在（含缺省）→ 回 `2025-06-18`。三版对「只有 tools」的服务端没有行为差异，回显是为了让客户端不必降级。 |
| S5-D5 | **6 个工具**（用户 2026-11-09 拍板）：`project_open` / `project_save` / `project_info` / `commands_list` / `commands_run` / `render_wav`。够写完整的创作闭环，又不至于把「打开一个工程」拆成三步。 |
| S5-D6 | **工具失败不走 JSON-RPC error。** 工具执行失败（文件读不到、命令不接受这个编辑、渲染失败）返回 `isError: true` + `content[0].text` 是 JSON 错误对象——错误是给**模型**读的，模型要能看到并自己纠正。JSON-RPC error 只留给协议层用错：坏 JSON `-32700`、不是合法请求 `-32600`、未知方法 `-32601`、参数不是对象/未知工具 `-32602`。 |
| S5-D7 | **`commands_run` 的原子性边界在「单条命令」，不在「批次」。** 每条命令仍由 `Session::execute` 保证「不能留下半个编辑」（快照 + 回滚 + panic 兜底），但**不宣称批次全或无**——session 的文档是活的，第 3 条失败时前 2 条已经生效。默认 `stop_on_error: true`：失败即停，剩余命令在报告里列为 `skipped`。S4 能做到全或无是因为它不写文件；MCP 没有这个杠杆，就不假装有。 |
| S5-D8 | **`commands_run` 支持 `dry_run: true`。** 调用前捕获 `Arc<Project>` 快照，跑完全部命令后 `Session::restore` 回去，报告里 `applied: false`。这让 agent 能先验证一套编辑再落地——比「先跑坏了再重开」便宜得多，也是 S5-D7 的补偿。dry_run 下 `project_save` / `render_wav` 看到的仍是旧文档。 |
| S5-D9 | **安全边界 = root 沙箱。** `--root <dir>`（缺省 = 进程 cwd）。`project_open` / `project_save` / `render_wav` 的路径参数一律先按 root 解析（相对路径 join，绝对路径原样），再做**词法规整**（消 `.` / `..`），然后校验 `starts_with(root)`，越界返回 `escapes the sandbox root`。绝对路径同样受检——否则「绝对路径」就是绕过沙箱的后门。 |
| S5-D10 | **插件默认禁用，`--allow-plugins` 显式打开。** 禁用时 `VoiceSpec` 为 CLAP / VST3 的节点渲染为静音，`render_wav` 结果里的 `warnings` 逐节点说明。理由：插件 = `dlopen` 任意 dll / so，一个被 prompt 注入的 MCP 客户端可以让服务器执行任意本地代码；这是 MCP 场景独有的风险（S4 的批次是人手敲参数，MCP 是模型拼参数）。实现上把 `spawn_node` 的 `live: bool` 换成 `SpawnPolicy`（见 §5.3）。 |
| S5-D11 | **`project_save` 复用 `save_project`**：`<name>.glz.tmp` + rename 原子写、保留 `.bak`、写前 `validate()`。与 CLI 同一路径，不另写裸 `fs::write`。 |
| S5-D12 | **`render_wav` 复用 `render_doc_to_wav`**，`rate` / `bits` / `to_ticks` 与 CLI 的 `--rate` / `--bits` / `--to` 同名同义；`bits` 取值 `"16" | "24" | "32f"`，直接调 `render_cmd::parse_bits`（一个解析器，两种入口）。 |
| S5-D13 | **模块切分** `crates/glitcherz-app/src/mcp/`：`mod.rs`（服务循环 + 方法分派）、`protocol.rs`（帧结构 + 错误码 + 握手）、`tools.rs`（6 个工具 + JSON Schema + 执行）、`sandbox.rs`（root 解析）。`main.rs` 只多一个 `Mode::Mcp` 分支。理由是 `tools.rs` 会随词汇增长，不能和传输层缠在一起。 |
| S5-D14 | **不做命令词汇扩容**。见非目标。 |
| S5-D15 | **不暴露 undo / redo / history**。见非目标。 |
| S5-D16 | **`project_info` 的字段固定**（客户端可以硬编码读）：`path` / `title` / `tempo_bpm` / `midi_tracks` / `audio_tracks` / `clips` / `notes` / `length_ticks` / `length_seconds` / `dirty` / `undo_depth` / `plugin_nodes`。`project_open` 返回同一份摘要（加 `opened: true`），两个工具共用一个 `summary()` 函数。 |
| S5-D17 | **`--mcp` 允许 `--open <path>` 预载**，失败即退出 3（不进服务循环）——客户端拿到的是「启动失败」，而不是每个工具都回一个「没有工程」。`--mcp` 与 `--render` / `--ops` / `--do` / `--save` / `--wav` / `--commands` 互斥（退出 2）。 |
| S5-D18 | **`--mcp` 的退出码**：`0` stdin EOF 或 `shutdown` 后正常结束 / `1` stdout 写失败（对端消失）/ `2` 参数错误（`--root` 不存在、与其它模式混用）/ `3` `--open` 预载失败。工具失败**不**影响进程退出码——那是每个响应的事。 |

## 4. 协议

### 4.1 帧

一行一个 JSON 对象（`\n` 分隔，行内不加换行）。请求 = 有 `id`，通知 = 无 `id`。服务端对请求回一个响应（`id` 原样回显），对通知**不回**。

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{...}}
{"jsonrpc":"2.0","id":1,"result":{...}}
{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"unknown tool `foo`"}}
```

`id` 只做透传：接受数字、字符串、null，回显时原样放回。不做 batch 数组（2025-06-18 已从规范移除），收到数组回 `-32600`。

### 4.2 握手

| 客户端 | 服务端 |
| :--- | :--- |
| `initialize` | `{"protocolVersion": <回显或 2025-06-18>, "capabilities": {"tools": {}}, "serverInfo": {"name": "glitcherz", "version": env!("CARGO_PKG_VERSION")}}` |
| `notifications/initialized` | 不回 |
| `ping` | `{}` |
| `tools/list` | `{"tools": [...]}` |
| `tools/call` | `{"content": [{"type": "text", "text": "<json>"}], "isError": <bool>}` |
| `shutdown`（非标准，容错） | `{}` 后退出 0；也接受直接 EOF |

`initialize` 之前收到 `tools/*` 不拒绝（不少客户端会抢先发），但 `initialize` 必须存在才算握手完成——不强制，只是不检查。

### 4.3 工具结果形态

成功与失败都是**一个 JSON 对象**塞进 `content[0].text`：

```json
{"ok": true,  "path": "...", "frames": 44288, ...}
{"ok": false, "stage": "command", "id": "midi.transpose", "kind": "bad_params", "error": "..."}
```

`isError` 与 `ok` **同真同假**（两条通道，客户端读哪条都对）。`stage` 复用 S4 的词表：`load` / `command` / `save` / `render` / `path`（新增，沙箱越界）。

**参数类型不对算协议错误，不算工具失败**：`rate: "44100"`、少给必填字段这类「请求没匹配 schema」的事回 `-32602`。区别在责任方——schema 不符是客户端没照清单发，操作失败是服务端试过了没成。

## 5. 工具

### 5.1 清单

| 工具 | 参数 | 结果（`ok: true` 时） |
| :--- | :--- | :--- |
| `project_open` | `path: string` | `summary()` + `opened: true` |
| `project_save` | `path?: string` | `path`, `bytes` |
| `project_info` | — | `summary()` |
| `commands_list` | `only_enabled?: bool` | `commands: [{id, label, params, menu, shortcut, enabled, disabled_reason}]` |
| `commands_run` | `commands: [{id, params?}]`, `stop_on_error?: bool = true`, `dry_run?: bool = false` | `applied: bool`, `results: [{id, ok, result?/error?}]`, `skipped: [string]` |
| `render_wav` | `path: string`, `rate?: u32 = 44100`, `bits?: "16"|"24"|"32f" = "24"`, `to_ticks?: u32` | `path`, `frames`, `sample_rate`, `bits`, `seconds`, `warnings: [string]` |

`summary()` = S5-D16 的 12 个字段。

### 5.2 语义细节

- **`project_open`** 替换整个 session（`Session::replace`，历史清空）。路径必须在 root 内且可解析；失败时**保留原 session**——不做「打开失败把工程清空」这种事。
- **`project_save`** 无 `path` 且 session 没有来源路径 → `stage: "path"` 报 `no project path; pass "path" or open a project first`。给了 `path` 就存到那里并**把它记为新的来源路径**（等价 GUI 的 Save As）。
- **`commands_run`** 的 `params` 省略等同 `{}`。`id` 未知 → 该条 `kind: "unknown"`。`dry_run` 与 `stop_on_error` 正交：dry_run 也照常停机，只是最后回滚。
- **`render_wav`** 渲染**当前 session 的内存文档**（S4-D6 的同一条原则），不是重读磁盘。`to_ticks` 省略 = 全曲。
- **`warnings`** 只在插件被禁用且有插件节点时非空；其余情况是 `[]`（存在但不是 null）。

### 5.3 插件禁用怎么落地

`headless::spawn_node` 现在接 `live: bool`。改为：

```rust
pub enum SpawnPolicy {
    /// GUI live graph: full bookkeeping, plugins allowed.
    Live,
    /// Offline render (CLI / GUI export): no bookkeeping, plugins allowed.
    Offline,
    /// MCP without `--allow-plugins`: no bookkeeping, plugin voices are silent.
    OfflineNoPlugins,
}
```

`spawn_instrument` / `spawn_effect` 在 `VoiceSpec::{Clap, Vst3}` 分支里查 `policy.plugins()`，为假则 `silent()` + `log::warn!`。`assemble_offline` 多一个 `policy: SpawnPolicy` 参数（4 个调用点同步改：GUI 导出、`render_doc_to_wav`、两处测试）。

`warnings` **不**从 `spawn_node` 里回传（那要给 `SpawnInfo` 加字段、要跨 `Media` 泛型）。MCP 层在渲染前扫一遍文档（`NodeDoc::Instrument{voice}/Effect{voice}` 里 `voice.is_plugin()`），得到节点列表直接当 `warnings`——同一份信息，零侵入。

## 6. 沙箱

```rust
pub fn resolve(root: &Path, raw: &str) -> Result<PathBuf, String>
```

1. 空串 → `Err("path is empty")`。
2. 绝对路径原样，相对路径 `root.join(raw)`。
3. 词法规整：逐段消 `.`，遇 `..` 弹出上层（弹到根则保留根，即 `..` 不能上溯到 `root` 之上——先弹再判，最终由 `starts_with` 兜底）。
4. `starts_with(root)` 为假 → `Err("... escapes the sandbox root")`。
5. 父目录存在时 `canonicalize` 父目录并与 root 的 canonical 形式再校验一次（挡 root 内指向外部的 symlink 的常见情形）。

**已知缺口**（写进 §9）：symlink 的 TOCTOU；`root` 自身若是 symlink，以 canonical 形式为准。

## 7. 改动面

| 文件 | 动作 |
| :--- | :--- |
| `crates/glitcherz-app/src/mcp/mod.rs` | 新增。服务循环、方法分派、`Server` 结构（`Session` + `root` + `allow_plugins` + 来源路径） |
| `crates/glitcherz-app/src/mcp/protocol.rs` | 新增。帧解析、错误码、握手常量、响应构造 |
| `crates/glitcherz-app/src/mcp/tools.rs` | 新增。6 个工具的 `inputSchema` + 执行 + `summary()` |
| `crates/glitcherz-app/src/mcp/sandbox.rs` | 新增。`resolve()` + 单测 |
| `crates/glitcherz-app/src/args.rs` | `Mode::Mcp`、`--mcp` / `--root` / `--allow-plugins`、互斥规则 |
| `crates/glitcherz-app/src/main.rs` | 一个分支 + `mcp_command()` |
| `crates/glitcherz-app/src/batch.rs` | 可能抽 `error_kind` 复用（已 `pub`，直接用） |
| `crates/glitcherz-app/src/render_cmd.rs` | `parse_bits` 提升为 `pub(crate)` |
| `crates/glitcherz-ui/src/headless.rs` | `SpawnPolicy` 替换 `live: bool`；`assemble_offline` 多一个参数 |
| `crates/glitcherz-ui/src/engine_sync.rs`、`export_ops.rs` | 调用点改 `SpawnPolicy::Live` / `Offline` |
| `crates/glitcherz-app/tests/mcp_cli.rs` | 新增。真子进程端到端 |

`glitcherz-command`、`glitcherz-model`、`glitcherz-engine` **一行不改**。

## 8. 验收

| # | 验收项 | 判据 |
| :--- | :--- | :--- |
| 1 | `initialize` 回握手 | `protocolVersion == 请求值`（请求 `2025-06-18`），`capabilities.tools` 存在，`serverInfo.name == "glitcherz"` |
| 2 | 未知版本回落 | 请求 `2019-01-01` → 回 `2025-06-18` |
| 3 | `tools/list` 恰好 6 个 | 名字集合与 §5.1 完全一致 |
| 4 | 每个工具 schema 合法 | `type == "object"`，`required` 是数组，`properties` 非空 |
| 5 | 通知无响应 | 发 `notifications/initialized` 后 stdout 无新行 |
| 6 | `ping` | 回 `{}` |
| 7 | 未知方法 | `error.code == -32601` |
| 8 | 坏 JSON 不致命 | 发 `{oops` → `-32700`，进程继续服务下一条 |
| 9 | 未知工具 | `tools/call` 一个不存在的名字 → `-32602` |
| 10 | `project_open` 正常 | 4 音符工程 → `notes == 4`、`clips == 1`、`length_ticks > 0` |
| 11 | `project_open` 越界 | `../../etc/passwd` → `isError: true`、`stage == "path"` |
| 12 | `project_open` 文件不存在 | `stage == "load"`，且随后 `project_info` 仍报**原**工程 |
| 13 | `project_save` 无路径 | `stage == "path"` |
| 14 | `project_save` 往返 | 存盘后 `load_project` 读回，音符数与音高一致 |
| 15 | `commands_run` 生效 | `midi.transpose +12` → 存盘后读回音高 +12 |
| 16 | `commands_run` 停机 | 第 2 条用坏 id → `ok: false`、`kind == "unknown"`、`skipped` 含第 3 条 |
| 17 | `commands_run` dry_run | 跑 `transpose` 后 `project_info` 的音高**未变**、`applied == false` |
| 18 | `commands_list` | 16 条，含 `midi.transpose`，`enabled` 字段为 bool |
| 19 | `render_wav` 与 CLI 对拍 | 同工程同参数（`--rate 22050 --bits 16 --to 1920`）与 `--render` 产出的 wav **逐字节一致** |
| 20 | `render_wav` 越界 | 输出路径 `../escape.wav` → `stage == "path"`，且文件不存在 |
| 21 | 插件默认禁用 | 含 CLAP 声源节点的工程 → `warnings` 非空、渲染不崩、wav 有帧 |
| 22 | stdout 纯净 | 端到端用例全程 `stdout` 每一行都是合法 JSON（`serde_json::from_str` 全通过） |
| 23 | stdin EOF 退出 | 关 stdin → 进程退出码 0 |
| 24 | `--mcp` 与批次互斥 | `--mcp --ops x.json` → 退出 2 |
| 25 | 零回归 | `--render` 对同一工程与 pre-S5 二进制逐字节一致 |

## 9. 风险与已知取舍

| 风险 | 说明 | 处理 |
| :--- | :--- | :--- |
| 手写协议子集的完备性 | 只实现 S5 需要的 6 个方法 + `ping` + `shutdown`；客户端若依赖 `resources/*` 会收到 `-32601` | 明确声明 `capabilities` 只有 `tools`；未知方法回标准错误码，客户端应自行降级 |
| symlink 逃逸 | 词法规整挡不住 root 内指向外部的软链接 | 写入前对父目录做 canonical 复检；残余 TOCTOU 记入本节，不假装解决 |
| 无 undo 暴露 | 坏批次只能重开工程 | `dry_run` 是主要防线；`project_save` 的 `.bak` 是最后一道 |
| 同步阻塞 | 大工程渲染期间无响应，MCP 的 progress 通知没做 | v1 接受；客户端通常有超时 |
| MCP 进程仍链接 egui/winit | 体积与编译时间不变，常驻多几 MB | `--mcp` 不初始化任何 UI 类型；真要剥离得先解决 D1 说的耦合 |
| `render_wav` 与 `--render` 分叉 | 两者都走 `render_doc_to_wav`，但入口不同 | 验收 #19 用逐字节对拍钉住 |
| 沙箱只管客户端给的路径 | 工程内部引用的媒体 / SF2 路径不做沙箱校验（等同于在 GUI 里打开一个不信任的工程） | 记入文档；需要更严时另立「工程信任模型」 |

## 10. 交付与证据

### 10.1 落地

| 文件 | 内容 |
| :--- | :--- |
| `crates/glitcherz-app/src/mcp/mod.rs` | `Server`（`Session` + `Sandbox` + `allow_plugins` + 来源路径）、`handle()` 分派、`run()` 的 stdio 循环、10 项单测 |
| `crates/glitcherz-app/src/mcp/protocol.rs` | 帧解析、错误码、`negotiate()`、响应构造，10 项单测 |
| `crates/glitcherz-app/src/mcp/tools.rs` | 6 个工具的 `inputSchema` + 执行 + `summary()` + 参数读取器，9 项单测 |
| `crates/glitcherz-app/src/mcp/sandbox.rs` | `Sandbox::new` / `resolve` / `normalize`，6 项单测 |
| `crates/glitcherz-app/src/args.rs` | `Mode::Mcp`、`--mcp` / `--root` / `--allow-plugins`、互斥规则（`--root` / `--allow-plugins` 脱离 `--mcp` 直接退出 2） |
| `crates/glitcherz-app/src/main.rs` | `mcp_command()` + 一个分派分支 + usage 三行 |
| `crates/glitcherz-app/tests/mcp_cli.rs` | 23 项真子进程端到端用例 |
| `crates/glitcherz-ui/src/headless.rs` | `SpawnPolicy{Live, Offline, OfflineNoPlugins}`、4 个「插件被拒」分支、`render_doc_to_wav_with(…, policy)`；`render_doc_to_wav` 变成 `Offline` 薄包装 |
| `crates/glitcherz-ui/src/engine_sync.rs`、`export_ops.rs` | 调用点改 `SpawnPolicy::Live` / `Offline` |

`glitcherz-command` / `glitcherz-model` / `glitcherz-engine` **一行未改**；无新 crate、无新外部依赖；`PROJECT_VERSION = 10` 未动。

### 10.2 验收结果

§8 的 25 条全部达标：

| # | 结论 |
| :--- | :--- |
| 1–2 | 版本回显 / 回落都有用例（`initialize_echoes_the_version_and_declares_tools`、`an_unknown_protocol_version_falls_back_to_the_latest`） |
| 3–4 | 6 个工具名逐一比对；每个 schema 都是 `type: object` + `required` 数组 + 非空 `properties` |
| 5–9 | 通知无响应（用「通知后紧跟 ping，看下一帧 id」判定，不依赖超时）、ping `{}`、未知方法 `-32601`、坏 JSON `-32700` 后继续服务、未知工具 `-32602` |
| 10–14 | `project_open` 摘要 4 音符 / 1 片段；越界 `stage: "path"`；文件不存在 `stage: "load"` 且原文档仍在；无路径保存 `stage: "path"`；保存后 `load_project` 读回音高一致 |
| 15–17 | `+12` 生效；坏 id 停机 + `skipped` + 首条已生效（证明「不是全或无」）；`dry_run` 后保存出的文件仍是原音高 |
| 18 | 16 条命令，`only_enabled` 过滤后数量变少且全为 `true`，`midi.change_velocity` 在空工程上带 `disabled_reason` |
| 19 | `render_wav`（22050 / 16 / 1920）与 `--render` 的 wav **逐字节相等** |
| 20 | 输出路径 `../escape.wav` → `stage: "path"`，且文件未产生 |
| 21 | 含 CLAP 声源节点的工程：`plugin_nodes == 1`、渲染有帧、`warnings` 一条且含 plugin id |
| 22 | 端到端用例全程记录每一行 stdout，`read_line` 逐行 `serde_json::from_str`，并断言帧数恰好等于请求数 |
| 23–24 | 关 stdin → 0；`--mcp --ops` / `--mcp --render` → 2；`--root` 脱离 `--mcp` → 2；`--root` 不存在 → 2；`--open` 预载失败 → 3 |
| 25 | 见 §10.3 |

### 10.3 零回归硬证据

用 `f4fafe9`（S5 spec commit，最后一个 pre-S5 代码状态）的 detached worktree + 独立
`CARGO_TARGET_DIR` 编出 pre-S5 二进制，与 post-S5 对 `working_files/regression-project.glz`
跑 `--render --rate 22050 --bits 16 --to 1920`：

```
pre : 44 288 frames → 177 196 字节
post: 44 288 frames → 177 196 字节
cmp: 逐字节一致    sha256 04ab77f91e0ae7594d943f2b8feed633d1e51766211a244fbc65d8d3d9d24fb2
```

另一条独立证据在端到端测试里：`render_wav` 与 `--render` 同参数产出的两个 wav
`assert!(cli == via_mcp)`——证明 `render_doc_to_wav_with` 没有改变渲染行为，只是多了一个策略参数。

### 10.4 门禁

| 项 | 结果 |
| :--- | :--- |
| `cargo fmt` | 干净 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 零警告 |
| `cargo test --workspace` | **1 303 项通过 / 0 失败**（S4 是 1 245，新增 58：35 单测 + 23 子进程） |
| `cargo build --workspace` | 通过 |

### 10.5 已知边界（未解决，不算欠账）

- symlink TOCTOU：`resolve` 的 canonical 复检与真正的写入之间有窗口。
- 无 undo 工具：坏批次只能重开工程；`dry_run` 与 `save_project` 的 `.bak` 是两道防线。
- 大工程渲染期间同步阻塞，没有 progress 通知。
- 工程内部引用的媒体 / SF2 路径不做沙箱校验（见 §9 最后一行）。
