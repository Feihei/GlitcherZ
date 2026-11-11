# 工程信任模型（Project Trust）

> ID: F-042 · 域: PROJ / IO
> 状态：设计（2026-11-11）
> 关联：`cli-commands.md`、`mcp-server.md`、`media.md`、`plugin-host.md`

## 1. 问题

`--root` / `--allow-plugins` 只约束**客户端传入的路径**与插件加载；工程文件内部引用的媒体 / SF2 / 插件路径在打开时照单全收。打开一个不信任的 `.glz` 等于允许它读取工程目录外的任意文件，或在允许插件时 `dlopen` 任意二进制。

## 2. 决策

| ID | 决策 |
| :--- | :--- |
| D1 | 信任单位是**工程**：一次打开一个工程，确认覆盖它引用的全部外部文件。 |
| D2 | 「外部」= 绝对路径，或解析后不在工程目录子树内。相对路径 + 工程内文件永远可信。 |
| D3 | UI 打开时先审计媒体（AudioClip / Sample / DrumKit）、SoundFont、插件路径。有外部引用 → 弹确认框，默认**仅内部**：外部媒体 / SF2 不加载（走既有 missing 回退），插件不实例化；文档里的路径保留，用户可重新授权。 |
| D4 | CLI `--open` 默认仅内部；`--trust-project` 显式允许。MCP 的 `--open` 同样受启动参数控制，客户端不能自行提权。 |
| D5 | 不迁移文件格式、不改路径存储；只改**加载策略**。 |
| D6 | 不做签名 / 哈希白名单 / 插件二进制验证（宿主无法跨平台稳定验证）。 |

## 3. 实现要点

- `Project::external_refs(base_dir) -> Vec<ExternalRef { kind, path, label }>`：审计媒体 / SF2 / 插件路径。
- 打开流程：`audit → dialog → load(external_allowed)`；`missing_media_dialog` 复用。
- 引擎装配按 `external_allowed` 跳过外部 SF2 / 样本 / 插件；状态栏汇总跳过数量。
- CLI / MCP：`--trust-project` 传入同一个 `external_allowed`。

## 4. 验收

- 外部媒体 / SF2 路径不被读取；对话框可授权；内部路径行为不变。
- CLI 未加 `--trust-project` 时外部引用不加载并给出明确提示。
- 无新 crate、不碰实时线程、`PROJECT_VERSION` 不动。