# GlitcherZ 用户文档

GlitcherZ 是一款免费、开源的数字音频工作站，支持 Windows / macOS / Linux，工作流参考 LMMS：
在一台电脑上完成写旋律、编排、混音、导出。

界面文案是英文的，本文档保留英文原文（`File ▸ Export Audio…`），方便和屏幕对上。

| 文档 | 内容 |
| :--- | :--- |
| [manual.md](manual.md) | 主手册：界面、轨道、编曲、钢琴卷帘、Patch 接线、混音、自动化、插件、录音、导出 |
| [shortcuts.md](shortcuts.md) | 快捷键与鼠标手势速查（全局 / 时间轴 / 卷帘 / Patch） |
| [project-files.md](project-files.md) | 工程文件与磁盘布局：`.glz`、`.bak`、`.autosave`、`<name>.media/` |

开发向文档（架构、spec、实现记录）在仓库的 `.agents/` 目录，不在本目录。

## 运行

需要 Rust 工具链（stable）。

```bash
cargo run -p glitcherz-app --release
```

编译出的可执行文件叫 `glitcherz`。两个诊断参数：

| 参数 | 作用 |
| :--- | :--- |
| `--list-devices` | 打印所有音频输出设备后退出 |
| `--seconds N` | N 秒后自动关窗（CI 冒烟用） |
| `--render <in.glz> <out.wav>` | 不开窗口、不占用音频设备，直接把工程渲染成 wav |
| `--commands` | 列出命令行能跑的编辑命令（`--json` 附带菜单 / 快捷键 / 不可用原因） |
| `--ops <file\|->` / `--do <id>` | 对一份工程按顺序跑一批编辑命令，可配 `--save` / `--wav` |
| `--mcp` | 把命令面开成 MCP 工具，跑在 stdio 上给 AI 客户端用 |

命令行渲染（批量 / CI / 脚本）：

```bash
glitcherz --render song.glz song.wav --rate 44100 --bits 24 --to 7680
```

`--rate`（默认 44100）、`--bits`（`16` / `24` / `32f`，默认 24）、`--to`（截止 tick，默认整曲）都是可选的。
它和窗口里的 `File ▸ Export Audio…` 走同一套离线装配，只输出一个混音文件（不做 stems / song loop）。

命令行改工程（脚本 / agent）：

```bash
glitcherz --open song.glz --ops steps.json --save out.glz --wav out.wav
```

命令来自窗口菜单用的同一张注册表，批次是**全或无**的（任一命令失败就不写文件），
`--wav` 渲染的是刚改完的内存文档。详见 [`manual.md`](manual.md#命令行批量命令)。

给 AI 客户端（脚本 / agent）：

```bash
glitcherz --mcp --root ~/Music/glitcherz
```

在客户端里把上面这行配成 stdio MCP server 即可：六个工具（开 / 存 / 摘要 / 列命令 / 跑命令 / 渲 wav），
路径只能落在 `--root` 里，插件二进制默认不加载（`--allow-plugins` 才放开）。
详见 [`manual.md`](manual.md#命令行mcp-server)。
所有模式共用一套退出码（0 成功 / 1 引擎 / 2 参数 / 3 工程或渲染 / 4 命令失败）。

## 五分钟做出第一段音乐

1. **启动。** 新工程自带一条 MIDI 轨（已接好一个内置 `Synth`）、一条 Curve 轨、
   一条 Audio 轨、一个 `Mixer`（1 个通道）和一个 `Keyboard In (computer)` 节点。
   切到 `Patch` 标签能看到整张图。
2. **弹几个音。** 切到 `Patch` 视图，按 `A W S E D F T G Y H U J K O L P ;` 是半音阶
   （`Z` / `X` 降 / 升音区，也就是 `Keyboard In` 节点上的 `Oct -` / `Oct +`）。
   声音走 `Keyboard In → Synth → Mixer → Master`，节点音条上对应的键会跟着亮。
3. **画一段。** 在 `Timeline` 的空轨道上双击 → 插入一小节片段；双击片段进入 `Edit`
   画音符（双击空白处插入音符，拖拽移动，右键删除）。
4. **排成曲。** 在标尺上 `Ctrl` + 拖拽画出 song loop，按 `Space` 播放听循环。
   顶部 `Tempo` 改速度，`Meter` 改拍号。
5. **导出。** `File ▸ Export Audio…` → 选 `Full song` 或 `Song loop`、位深（默认 24-bit）、
   采样率 → `Choose File…`。

> 走带键（`Space` / `Home` / `End` 等）在任何焦点下都有效；`G` `T` `K` 这类字母命令
> 只在时间轴或卷帘拿到键盘焦点时有效——点一下那片区域，工具栏左侧的
> `no keyboard focus` 提示消失就是了。

## 常见问题

| 现象 | 原因与处理 |
| :--- | :--- |
| 按键盘没声音 | 看 `Keyboard In` 节点是否有一条 MIDI 线连到乐器；再看乐器是否接到了 `Mixer` / `Master`。`Ctrl+L` 可以快速把选中乐器接到电脑键盘。 |
| 播放时完全没声音 | `Edit ▸ Preferences…` 里换一个输出设备；设备被拔掉时窗口顶部会报 `Last output error`。右侧 `Master` 有电平表，可先确认引擎在出声。 |
| 按 `R` 提示 `no input` | 录音要求轨道的时间轴输入口被接上：`Patch` 里把 `Keyboard In` 的 MIDI 输出拖到 `T1`（MIDI 轨），或把 `Audio Input` 拖到 `A1`（音频轨）。默认工程没有接。 |
| 插件列表是空的 | `Patch` 空白处右键 → `Rescan Plug-ins`。插件需装在标准目录，或用 `GLITCHERZ_CLAP_PATH` / `GLITCHERZ_VST3_PATH` 指定（见 [manual.md](manual.md#插件)）。 |
| 换台机器打开工程，采样全丢了 | 存盘前用 `File ▸ Collect Media Files…` 把外部素材收进 `<工程名>.media/`；已经丢了的用 `File ▸ Relocate Missing Media…` 按文件名找回。 |
| 崩溃后工作还在吗 | 在。启动时会弹 `Recover unsaved work`，见 [project-files.md](project-files.md#自动保存与崩溃恢复)。 |
| 导出采样率能高于声卡吗 | 能。下拉第一项是当前设备采样率，其余是 44.1 / 48 / 88.2 / 96 kHz；导出用离线引擎，选 96 kHz 不需要 96 kHz 声卡。 |
