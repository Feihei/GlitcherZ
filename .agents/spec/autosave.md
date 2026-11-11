# Spec: 自动保存与崩溃恢复（Auto-save & Crash Recovery）

> 状态：已交付（2026-11-03，S3a）。实现：`glitcherz-ui/src/autosave.rs` +
> `glitcherz_model::file::save_autosave`；细节见 `implementation.md`「自动保存与崩溃恢复」。
> 关联：`spec/plugin-state.md`（插件状态 blob 的捕获时机）。

## 1. 问题

S1-2 之后手存是原子的、也有 `.bak`，但**只要没手存，一次崩溃就全丢**——
undo 栈随进程消失，临时文件也没写。review 里这条与「保存非原子」并列，因为
两者叠加后的用户感受是"这软件会吃掉我的歌"。

## 2. 核心决策

| # | 决策 | 结论 | 理由 |
| :-- | :-- | :-- | :-- |
| D1 | 落点 | 已存盘工程 → **同目录 `<name>.glz.autosave`**；从未存盘 → `<app data>/autosave/Untitled.glz` | 快照跟着工程走：换机器 / 分享目录时未保存的编辑也在里面。未存盘工程没有工程目录，只能借 app data（`eframe::storage_dir`） |
| D2 | 不用 `.bak` | 独立后缀，不复用 `<name>.glz.bak` | `.bak` 的语义是"上一次手存的版本"，与"30 秒前的自动快照"混用会让两个功能都说不清 |
| D3 | 恢复信号 | **文件存在 = 上一轮没干净退出**。不做 mtime 比较 | 干净退出（手存 / 明确丢弃 / 文档为 clean）都会删除快照，所以残留即崩溃；省掉"哪个更新"的猜测，也让规则可穷举、可测试 |
| D4 | 触发节奏 | 文档脏 && 距上次编辑 **静默 30 s**，或距上次写 **≥ 60 s**（连续编辑时） | 静默 30 s 覆盖"停下想了想"的常见节奏；60 s 兜住连续拖参数的极端情况，避免一次会话完全不落盘 |
| D5 | 写入内容 | **克隆一份文档 + 现场捕获插件状态**后写克隆 | 直接把 blob 写进活文档会 bump revision 与 dirty，于是每 30 s 自触发一次 → 写盘死循环。克隆是深拷贝（每 ≥60 s 一次，可接受），换来"恢复出来的工程连插件状态都是新的" |
| D6 | 不复用 `save_project` | `save_autosave`：原子写、**不写 `.bak`**、**不 `validate`** | 快照不是用户要的版本，旁边长一个 `.bak` 只是垃圾；而"文档瞬时非法"不该成为丢掉用户工作的理由——加载路径仍会 validate，坏快照会在恢复时报错 |
| D7 | 恢复后状态 | 文档标 **dirty**，`project_path` 指回原工程 | 快照从未以该工程保存过，标题要带 `*`、关闭要询问；下次 Ctrl+S 才真正落成 `<name>.glz` |
| D8 | 快照何时删除 | 手存成功、明确丢弃（Exit 路径）、窗口关闭时文档为 clean | 崩溃（Drop 都跑不到）与"脏着被强杀"保留快照——这正是它存在的用途 |
| D9 | 提示形式 | 启动 **modal**（每项：Recover / 年龄 / 大小 + Discard all / Later） | 状态栏一行不足以阻止"悄悄丢工作"；Later 只关提示不删文件，下次启动仍会问 |
| D10 | 平台缺 app data | `storage_dir` 为 `None` 时只快照已存盘工程 | 没有目录就不猜路径；已存盘工程仍受保护 |

## 3. 数据流

```
每帧末尾 (ui())
  session.revision() / is_dirty()  ──►  Clock::observe(revision, dirty, now)
                                          │
                     due(now)? ──否──────► 返回
                            │是
                            ▼
        snapshot = project.clone() + 现场插件 blob（apply_plugin_state）
                            │
                   save_autosave(&snapshot, target)   // 原子：.tmp → sync → rename
                            │
                     Clock::mark_written(now)          // 成功与失败都记：失败下一分钟再试
```

启动：

```
GlitcherZApp::new
  candidates(recent, storage_dir) ──► 每个 recent 工程查 <name>.glz.autosave，再加 Untitled
        │ 非空
        ▼
  recovery_dialog（modal）── Recover ──► queue_blocking_job → load_project → adopt_document(dirty)
                          ├─ Discard all ──► 删除全部快照
                          └─ Later ──────► 仅关闭提示（文件保留）
```

## 4. 已知边界

- **未保存窗口**：最后一次编辑到崩溃不足 30 s 的部分会丢——这是"静默 30 s"的代价，
  与其它 DAW 一致。
- **大工程克隆**：每次快照深拷贝一次 `Project`（含插件 state blob）。万级音符 +
  多个 Dexed 实例时单次几十 MB，仍是每 ≥60 s 一次；若将来成为瓶颈，可改成
  序列化时借用文档 + 只替换 blob 段。
- **快照不清理旧工程**：换工程后旧工程目录里的快照仍在（除非经过 Save As 路径），
  下次启动会作为候选列出——用户可用 Discard all 一次性清掉。
- **autosave 文件会被版本库/同步盘带上**：`.gitignore` 未加规则（工程目录不归仓库管）。
  分享工程时可删。

## 5. 未做

- 崩溃后**自动**恢复（不询问）。当前一律询问：恢复会替换当前文档，静默替换更危险。
- 多文档 / 会话恢复（同时恢复多个工程的编辑状态）。
- 快照压缩或增量（当前是完整 RON 文本）。
