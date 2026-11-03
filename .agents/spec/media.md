# Spec: 媒体路径 · 收集 · 重定位（Media Paths）

> 状态：已交付（2026-11-03，S3b）。实现：`glitcherz-model`（`media_paths` /
> `rewrite_media_paths` / `missing_media` / `resolve_media_path` 规范化）+
> `glitcherz-ui/src/media.rs`（收集落点 / 按名搜索）+ File 菜单两项动作。
> 关联：`review-2026-11-01.md` §2「工程可移植」、
> `spec/recording.md` D11（录音素材落 `<stem>.media/` + 相对路径，本 spec 把
> 这条规则推广到全部媒体）。

## 1. 问题

review 的结论很直白：`path: String` 存的是绝对路径，换机器 / 分享工程 → 全静音。
R4 只修了录音素材（写相对、读时按工程目录解析），其余三类媒体仍是绝对路径，
而且**根本没有"文件在哪"的检查**——缺文件时用户只看到一段没有声音的工程。

## 2. 核心决策

| # | 决策 | 结论 | 理由 |
| :-- | :-- | :-- | :-- |
| M1 | 存储形式 | 路径一律以 `/` 分隔；在工程目录下就存相对路径，否则保留绝对路径 | 相对路径让整个工程文件夹可搬移；`/` 让 Windows 写的工程能在 macOS 打开（`\` 在 unix 是普通字符）。绝对路径继续可用——外部共享库不必强行搬进来 |
| M2 | 覆盖范围 | 四类：采样乐器的 wav、SoundFont、鼓机 slot、音频 clip。**插件二进制（CLAP/VST3）不算媒体** | 插件是"装的软件"，不是工程资产；把它算进"收集媒体"会把几百 MB 的二进制复制进工程 |
| M3 | 解析点 | 全部集中在 `Project::resolve_media_path`，由 `obtain_sample_voice` / `obtain_sf2_voice` / `obtain_drum_kit_voice` / `build_audio_clip_table` 调用；**缓存键用解析后的绝对路径** | 一个函数决定"相对怎么变绝对"，其余地方不再各写一遍 join；缓存按绝对路径才不会让两个工程的同名媒体互相污染 |
| M4 | 收集 | `File ▸ Collect Media Files…`：把不在 `<stem>.media/` 里的媒体复制进去并重写路径（**一步 undo**）。要求工程已存盘 | 媒体目录定义在工程文件旁边，未存盘的工程没有"旁边" |
| M5 | 收集命名 | 沿用原文件名；同名且**大小不同**才加 ` (2)`；同名且大小相同视为已收集（跳过复制） | 两个不同来源的 `kick.wav` 不能互相覆盖；同大小视为同一文件，重复执行"收集"保持幂等（300 MB 的 SoundFont 不会被复制成两份） |
| M6 | 收集后不重载引擎 | 只改文档，不 `apply_document` | 引擎里已经解码好的数据就是这些字节的副本，路径变化不影响本次会话的播放 |
| M7 | 重定位 | `File ▸ Relocate Missing Media…`：选一个文件夹，按**文件名**在其下 BFS 搜索（深度 ≤ 4、预算 20000 条、大小写不敏感），命中即重写（一步 undo），随后 `apply_document()` | 用户通常只记得"我把它挪到 Samples 文件夹了"；按名字找比让人逐个点选省事。重写后必须重建图——节点里装的是旧路径的源 |
| M8 | 缺失检测 | 只在**打开 / 恢复 / 收集 / 重定位**后算一次 `missing_media()`；非空时弹 modal（列出路径 + `Locate Folder…` / `Continue without them`） | 播放中途轮询会在用户正编辑时弹出；"Continue" 关掉后本次会话不再打扰 |
| M9 | 不做自动修复 | 不用 `../` 猜测、不在收藏目录里瞎找、不静默替换 | 猜错了比"少一个文件"更糟：用户会以为工程是好的 |

## 3. 数据流

```
存路径（picker 返回绝对路径）
  ├─ 采样 / SoundFont / 鼓机 slot / 音频 clip → Project::media_path_string()
  │     base_dir 存在且文件在它下面 → 相对路径（/ 分隔）
  │     否则 → 绝对路径
  ▼
读路径（install / 离线装配）
  Project::resolve_media_path() → PathBuf（`\` 折回 `/`，相对则 join base_dir）
  ▼
收集：media_paths() → 逐个 collect_target() + copy → rewrite_media_paths()（undo: "Collect Media"）
  ▼
重定位：missing_media() → find_by_name(dir) → rewrite_media_paths()（undo: "Relocate Media"）→ apply_document()
```

## 4. 已知边界

- **旧工程不会被自动改写**：打开一个全绝对路径的老工程，路径仍是绝对的（直到用户执行"收集"）。
  这是有意的——静默改工程文件是另一个量级的风险。
- **重定位只按文件名匹配**：同名不同内容的文件会被接受。用户能从随后的听感发现，且可以再换。
- **搜索深度 4 / 预算 20000**：`Samples/Artist/Album/Kick.wav` 命中；指向整个磁盘的误选会在预算耗尽后
  安静返回（界面报"没有匹配的文件"）。
- **`Continue without them` 之后**：缺失文件保持缺失，音频 clip 静音、采样/SF2 节点退化为静音或内置合成器
  （与 D8 缺文件回落一致），直到下次打开工程时再次提示。
- **工程目录改名**：`base_dir` 在每次保存/打开时刷新，所以工程自己改名不受影响；只搬工程文件而不搬
  `<stem>.media/` 才会缺。

## 5. 未做

- 缺失媒体的"下次自动记住上次定位的目录"。
- 收集时的按类型分子目录（`media/wav`、`media/sf2`）。
- 打包成 zip / 单文件工程。
