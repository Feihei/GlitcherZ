# CLI / MCP 命令词汇表（节点参数稳定名 + graph.set_param + clip.add）

> ID: F-039 · 域: PATCH / EDIT / IO
> 状态：设计（2026-11-11）
> 关联：`node-graph.md`、`cli-commands.md`、`mcp-server.md`、`control-tracks.md` §9

## 1. 问题

S5 收官时记下的硬阻塞：MCP 已能改音符（`midi.*` 19 条命令，F-038），但：

- **改不了「用什么音色」**——`NodeParam` 只有 Rust 枚举与 UI 显示名，没有稳定字符串名；
- **开不了新 clip**——没有 `clip.add`，agent 只能编辑既有内容，不能从空工程搭出可听片段。

结果：`commands_run` 能「改音符」，却无法「换乐器 / 开新段落」，agent 工作流在半途断掉。

## 2. 决策

| ID | 问题 | 结论 |
| :--- | :--- | :--- |
| D1 | 参数如何命名 | 每个 `NodeParam` 变体一个 snake_case 稳定 key（`attack` / `feedback_gain` / `mixer_out_pan` …）。key 是**公开 API**，枚举内部仍强类型；新增变体必须同时加 key 与往返测试 |
| D2 | 带下标的参数 | `effect_slot` / `organ_bar` / `mixer_channel` 用 `param` + 可选 `index` / `channel` 寻址；`ChannelParam` 用 `channel_param`（`gain` / `pan` / `mute` / `solo`） |
| D3 | 节点寻址 | `node` = `node_id`（u64，稳定）；不用轨道内索引（节点可增删，索引会漂移） |
| D4 | 值类型 | 命令一律 `value: f64`；离散参数（mute / solo）按 ≥ 0.5 归一；命令按参数描述符 clamp，与 UI 完全一致 |
| D5 | 插件参数 | **V1 不做**。CLAP / VST3 参数 id 由宿主协商，稳定性与索引语义要单独设计；插件节点在 `graph.set_param` 返回 `unsupported_node` |
| D6 | 新建 clip | `clip.add` 仅 MIDI（V2）；control clip 需要目标 / 轴范围、audio 需要媒体路径，分别另立 |
| D7 | MCP 工具面 | 不新增工具，复用既有 `commands_run`；命令注册后自动出现在 `commands_list`，保持 6 个工具 |
| D8 | 参数文档 | `CommandSpec.params` 必须列出类型与范围；`graph.query` 返回节点 + 可写参数 key，供 agent 发现 |

## 3. 命令

### 3.1 `graph.set_param`

```json
{"track":0,"node":12,"param":"attack","value":0.05}
{"track":0,"node":12,"param":"effect_slot","index":1,"value":0.5}
{"track":0,"node":5,"param":"mixer_channel","channel":2,"channel_param":"pan","value":-0.25}
```

返回 `{node, param, value}`。失败码：`unknown_node` / `unknown_param` / `unsupported_node` / `bad_params`（值非有限数）。undo 一步、journal。

### 3.2 `graph.query`

`{track}` → `{nodes:[{node_id, kind, label, params:[{key, min, max, value, kind}]}]}`。只读、不 undo、不进 journal。

### 3.3 `clip.add`

```json
{"track":0,"lane":"midi","start_ticks":0,"span_ticks":3840,"name":"Verse"}
```

缺省：`start_ticks: 0`、`span_ticks` = 当前拍号一小节、`name` 自动。返回 `{clip, start_ticks, span_ticks}`。上限 `MAX_CLIPS_PER_TRACK`；undo 一步。

## 4. 里程碑

| 里程碑 | 内容 | 依赖 | 规模 |
| :--- | :--- | :--- | :--- |
| **V1** 参数词汇表 | `NodeParam::key()` / `from_key()` + `graph.set_param` + `graph.query` | — | 中 |
| **V2** `clip.add` | `clip.add`（MIDI） | — | 小 |
| **V3** 端到端 | MCP `commands_run` 测试、CLI、`docs/`、plan 同步 | V1、V2 | 小 |

## 5. 明确不做

- 插件参数写入（V1 范围外，见 D5）。
- `NodeParam` 改名 / 迁移（只新增 key，不动枚举与文档格式）。
- 非 MIDI clip 的新建（control / audio）。
- 命令别名与版本化。

## 6. 验收

- `NodeParam` 每个变体都有 key，`from_key(key)` 往返；key 唯一性测试。
- `graph.set_param` 改参数后文档读回正确值，且引擎收到参数更新。
- `clip.add` 后 `midi.note_add` 能写入该 clip。
- MCP 端到端：空工程 → `clip.add` → `midi.note_add` → `midi.transpose` → `render` 非静音。
- MCP 工具数仍为 6；命令数 +3。