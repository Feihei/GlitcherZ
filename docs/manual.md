# GlitcherZ 用户手册

界面文案是英文的，正文保留原文以便对照屏幕。

## 界面

窗口从上到下是四条：

| 区域 | 内容 |
| :--- | :--- |
| 走带栏 | `File` / `Edit` 菜单；`⏮` `▶ Play` `■ Stop` `⏺ Rec` `⏭`；`Loop`、`Click`、上/下一个标记；`Tempo`、`Meter`；右侧 `Master` 推子、立体声电平表和 panic 按钮 |
| 标签栏 | `Timeline` / `Edit` / `Patch` / `Inspector` |
| 主区域 | 当前标签的内容 |
| 状态栏 | 当前编辑目标、音符名、键盘映射、`Pos 小节.拍`、最近一条消息（失败是红色） |

`Edit` 菜单只有四项：`Undo`（`Ctrl+Z`）、`Redo`（`Ctrl+Y`）、`Undo History…`、
`Preferences…`。`Undo History…` 是浮窗，最新在上，当前行高亮，灰行可以 redo，点一行就跳到那一步之后。

## 工程

| 操作 | 位置 |
| :--- | :--- |
| 新建 | `File ▸ New`（`Ctrl+N`） |
| 打开 | `File ▸ Open…`（`Ctrl+O`）、`File ▸ Open Recent` |
| 保存 | `File ▸ Save`（`Ctrl+S`） |
| 另存为 | `File ▸ Save As…`（`Ctrl+Shift+S`） |
| 导入 MIDI | `File ▸ Import MIDI…`（`.mid` / `.midi` / `.smf`） |
| 导出 MIDI | `File ▸ Export MIDI…` |
| 导出音频 | `File ▸ Export Audio…` |
| 收集媒体 | `File ▸ Collect Media Files…` |
| 重定位缺失媒体 | `File ▸ Relocate Missing Media…` |

标题栏出现 `*` 表示有未保存改动。新建 / 打开 / 关窗时如果文档是脏的，会先弹
`Unsaved changes`（`Save` / `Discard changes` / `Cancel`）。

保存是崩溃安全的：先写 `.tmp` 再原子重命名，上一版留成 `.bak`。细节见
[project-files.md](project-files.md)。

打开工程时如果有采样 / SoundFont / wav 找不到了，会弹一个缺失列表，可以
`Locate Folder…` 选目录按文件名重定位，或 `Continue without them` 先继续。

## 轨道

三种轨道，各自独立计数，都可以在 `Timeline` 工具栏或 `Patch` 的 `Timeline` 节点上加 / 减：

| 轨道 | 上限 | 用途 |
| :--- | ---: | :--- |
| MIDI | 16 | 音符片段，送进乐器节点 |
| Curve（控制轨） | 16 | 自动化曲线片段，连到某个参数 |
| Audio | 8 | 直接放 wav 片段 |

单轨最多 32 个片段，编曲总长 256 小节（4/4 下）。

- 点行首选中轨道；`Alt` + 点击只选这一个，框选可以多选。
- 轨道名：右键行首的轨道名 → 就地编辑，`Enter` 确认、`Esc` 取消，清空则回落 `Track N`。
- 片段名：`F2` 或 `Alt` + 双击片段。
- 行首的小圆点可以上下拖动调整轨道顺序。
- 静音、独奏、音量、声像在 `Mixer` 节点的通道条上，不在行首。

视图操作：`Ctrl` + 滚轮水平缩放，`Ctrl` + `Alt` + 滚轮改行高，中键拖拽平移。

## 编曲时间轴

| 操作 | 结果 |
| :--- | :--- |
| 拖动片段主体 | 移动（吸附到小节） |
| 拖动片段右边缘 | 扩展/缩短片段长度；平铺片段的周期随长度缩放 |
| 双击空轨道 | 插入一小节片段；音频轨会弹出 wav 选择框 |
| 双击片段 | 进 `Edit` 标签（MIDI → 钢琴卷帘，Curve → 曲线编辑器） |
| `Alt` + 双击片段 / `F2` | 原地重命名片段 |
| 右键片段 | 删除 |
| 标尺点击 / 拖动 | 定位播放头 |
| 标尺 `Ctrl` + 拖动 | 画 song loop；拖 loop 边缘可改范围 |
| 标尺右键 | 加标记；在标记旗上右键删除 |
| 右键行首轨道名 | 重命名轨道 |
| 拖行首圆点 | 调整轨道顺序 |
| 轨道空白处单击 | 让时间轴拿到键盘焦点（`G` / `T` / `K` 等命令的前提） |

时间轴的模态命令（需要焦点，工具栏会显示当前状态）：

| 键 | 作用 |
| :--- | :--- |
| `G` | 抓取移动选区；`X` 锁定纯时间轴、`Y` 锁定纯轨道行 |
| `Shift+D` | 复制选区并进入抓取；同样支持 `X` / `Y` |
| `T` | 扩展/缩短选区尾边；模态中按 `S` 切到首边 |
| `Shift+T`（`Alt+T` 备用） | 平铺：向右拖动增加整段重复、往左减少；已平铺片段再按一次 = 调整重复份数；1 小节的片段也能平铺 |
| `K` | 刀片：点一次切一刀（按小节吸附），`Esc` / 右键取消 |
| `Delete` | 删除选中片段 |
| `Ctrl+G` / `Ctrl+Shift+G` | 编组 / 解组选中片段 |
| `Esc` / 右键 | 取消当前模态 |

模态命令按下即生效、跟随鼠标，左键落下，右键或 `Esc` 放弃（Blender 式，卷帘里同名同义）。

## 编辑视图

`Edit` 标签跟随时间轴选区：选中 MIDI 片段就是钢琴卷帘，选中 Curve 片段就是曲线编辑器，
音频片段没有编辑器（音频轨直接用片段本身的裁剪窗口）。

### 钢琴卷帘

两种常驻模式：

| 模式 | 进入 | 行为 |
| :--- | :--- | :--- |
| `Select` | 默认 | 左键拖拽 = 移动选中音符 / 拖边缘裁剪 / 空白处框选 |
| `Draw` | 按 `D` 切换；`Esc` 也能退出 | 在空白处拖拽 = 画音符 |

命令与手势：

| 操作 | 结果 |
| :--- | :--- |
| 双击空白 | 插入一个默认长度的音符 |
| 拖音符主体 / 拖边缘 | 移动 / 裁剪（不限次数的手势形式） |
| `Ctrl` + 拖音符 | 复制 |
| `Shift` / `Ctrl` + 点击 | 加选 / 切换选择 |
| 右键音符 | 删除 |
| 方向键 | 上下一律、`Shift` 上下八度、左右一格 |
| 滚轮 | 上下滚动音高；`Shift` + 滚轮沿时间轴滚动 |
| `Ctrl` + 滚轮 / `Ctrl` + `Alt` + 滚轮 | 时间轴缩放 / 音高缩放 |
| 中键拖拽 | 平移 |
| `G` / `S` / `T` / `K` / `Shift+D` | 抓取 / 缩放 / 拉伸（`T` 后按 `S` 切首边）/ 刀片 / 复制 |
| `X` / `Y` | 模态中锁时间轴 / 锁音高 |
| `Esc` / 右键 | 取消模态 |

多个片段共享一条时间轴时，只有**焦点片段**可写，同轨其它片段以 ghost 形式只读显示，
方便对齐。

### 曲线编辑器

| 操作 | 结果 |
| :--- | :--- |
| 点击空白 | 加锚点 |
| 拖锚点 | 移动 |
| 右键 | 删除该处锚点 |
| 工具 `Line` / `Draw` / `Curve` | 折线（直线段）/ 徒手画（自动切成直线）/ 平滑（单调三次） |

曲线值域是归一化的 `[-1, 1]`，与目标参数无关；轴标注显示换算后的真实单位
（例如 `20…18000 Log`）。换成别的目标参数，曲线的形状不变。

## Patch 接线板

节点图。滚轮缩放，中键拖拽平移，框选多选，`Delete` 删除选中节点或连线。

添加节点：空白处右键或 `Shift+A`。

| 菜单项 | 产生 |
| :--- | :--- |
| `Search Nodes…` | 打开节点浏览器，可搜名字或模块路径 |
| `Instrument…` | 乐器：`Synth`（内置）、`SoundFont`、`Sample`、`Drum Kit`，以及 CLAP / VST3 乐器插件 |
| `Effect…` | 效果：`Delay`、`EQ`、`Compressor`、`Reverb`、`Chorus`、`Flanger`，以及 CLAP / VST3 效果插件 |
| `MIDI FX…` | `Arpeggiator`、`Portamento`、`MIDI Envelope`、`LFO`、`Curve Map` |
| `Mixer` | 多通道求和节点（1–32 通道，可加 / 减通道） |
| `Keyboard In (computer)` | 电脑键盘输入节点，一个源只允许一个节点 |
| `Audio Input (microphone)` | 实时输入节点，带设备选择和电平表 |
| `Feedback Tap` | 反馈抽头：把输入晚一个块求和，让环路合法 |
| `Rescan Plug-ins` | 重新扫描插件 |

`SoundFont` / `Sample` / `Drum Kit` 出生时不带文件（沉默），在节点上的 `Load File…` /
`Load Samples…` 按钮里选，换文件不需要重建节点和连线。

三种连线，从输出口拖到输入口即可：

| 线 | 从 | 到 |
| :--- | :--- | :--- |
| MIDI | `Timeline` 的轨道口（`T1`…）或 `Keyboard In` / MIDI FX 输出 | 乐器节点的 MIDI 输入 |
| Audio | 乐器 / 效果的音频输出 | 效果输入、`Mixer` 通道、`Master` |
| Automation | `Timeline` 的曲线口（`C1`…）或控制源输出（`LFO`、`MIDI Envelope`、`Portamento`） | 节点端口——松手后弹出参数表，选一个目标 |

`Ctrl` + 右键拖拽 = 划断连线。点连线中点会弹菜单，可以把效果直接插进这条线，或删掉它。

双击节点进 `Inspector`（乐器 / 效果 / 混音 / MIDI FX / LFO / Curve Map / Audio Input）；
双击固定的 `Timeline` 节点回到编曲视图。

## Inspector 与混音

`Inspector` 显示当前节点：

| 节点 | 面板内容 |
| :--- | :--- |
| 乐器 | 内置合成器的波形与滤波、ADSR、滑音；`SoundFont` 的 bank / patch 与文件；`Sample` 的根音与增益；`Drum Kit` 的每个槽位 |
| 效果 | 该效果的全部参数；插件效果用插件自己的 GUI（`Open Editor`） |
| `Mixer` | 通道条：每路推子、声像、静音、独奏、电平表，加 / 减通道；输出级的声像与静音 |
| 插件乐器 | 参数列表（可右键接到自动化）+ `Open Editor` |
| `Audio Input` | 设备选择、通道电平表 |

`Master` 推子和电平表在走带栏右侧，属于整首歌，不在 Inspector 里。

## 自动化

两条路：

1. **曲线轨**：`+ Curve` 加一条控制轨，在 `Patch` 里从它的 `C1` 口拖一条 automation 线到
   目标节点，松手时选参数（节点参数、`CC` 预设、pitch bend、tempo 都在列表里）。
   然后在时间轴双击曲线片段，进 `Edit` 画点。
2. **控制源节点**：`LFO` / `MIDI Envelope` / `Portamento` 的 control 输出拖到目标参数，
   发的是实时控制流，不需要画曲线。

改目标参数不会破坏曲线形状（曲线存归一化值，轴在连线上）。tempo 也可以被曲线驱动：
走带栏的 `Tempo` 会显示 `→ 128.0 auto` 表示时钟正被 `Timeline In` 的曲线接管。

## 插件

支持 CLAP 和 VST3 的乐器与效果。启动时扫描一次并缓存，装 / 删插件后用
`Patch` 右键 → `Rescan Plug-ins`。

搜索路径（先环境变量，再标准目录）：

| 格式 | 环境变量 | 标准目录（Windows） |
| :--- | :--- | :--- |
| CLAP | `GLITCHERZ_CLAP_PATH` | `%ProgramFiles%\Common Files\CLAP`、`%LOCALAPPDATA%\Programs\Common\CLAP`、`%LOCALAPPDATA%\CLAP`、`%APPDATA%\CLAP` |
| VST3 | `GLITCHERZ_VST3_PATH` | `%ProgramFiles%\Common Files\VST3`、`%LOCALAPPDATA%\Programs\Common\VST3`、`%APPDATA%\VST3` |

环境变量用系统路径分隔符（Windows 是 `;`）列多个目录。目录内不递归，只扫一层
（VST3 的 bundle 目录算一个条目）。

插件的参数值与完整状态（插件自己序列化的 state）随工程保存，重开工程会回灌。
单个插件保存状态失败只记日志，不会让整次保存失败。

## 录音

录音前要先让轨道"布防"：时间轴每条轨都有一个输入口，只有接上源才会录。

| 类型 | 接法 |
| :--- | :--- |
| MIDI | `Patch` 里把 `Keyboard In`（或某个 MIDI FX）的 MIDI 输出拖到目标轨的 `T1` 口 |
| 音频 | 把 `Audio Input` 节点的音频输出拖到音频轨的 `A1` 口 |

走带栏 `⏺ Rec` 或按 `R` 开始。录制设置：

| 控件 | 作用 |
| :--- | :--- |
| `Pre-roll` | 0–2 小节预备拍，这段不计入录音 |
| `Mute others` | 录音时静音其它 MIDI 轨的已排音符 |
| `Latency` | 手动输入延迟补偿（样数），正值把音频片段往前挪；仅本次会话有效，不存进工程 |

录音中再按一次 `R` 或 `Space` 是 punch out（收尾但继续播放），`■ Stop` 收尾并回到开头。

音频 take 写进 `<工程名>.media/`，文件名 `rec-<时间戳>-a<轨号>.wav`，所以**必须先保存工程**
才能录音频——未命名工程会提示先存盘。

## 导出

### 音频

`File ▸ Export Audio…`：

| 选项 | 说明 |
| :--- | :--- |
| `Full song` | 从 0 到编曲末尾 |
| `Song loop` | 只导标尺上启用中的 loop 范围（没画时禁用） |
| `Depth` | `16-bit` / `24-bit`（默认）/ `32-bit float` |
| `Rate` | 设备当前采样率 + 44.1 / 48 / 88.2 / 96 kHz |
| `Also write stems` | 混音之后再逐轨渲染，一轨一个文件 |

- 立体声 wav，末尾多渲 1 秒让延迟与混响尾巴收干净。
- 导出用离线引擎，选 96 kHz 不需要 96 kHz 硬件；音频片段按时长播放，换采样率不变调。
- stems 的命名是 `<混音文件名>_<轨名>.wav`（未命名轨是 `MIDI 1` / `Audio 1`），重名追加
  ` 2`、` 3`；非法字符换成 `_`。stem 里保留自动化与 tempo 曲线——那是"这条轨听起来的样子"。
- 导出期间编辑文档不影响这次渲染（开始时已快照）。一次只允许一个导出任务。
- 取消会删掉当前那个没写完的文件，已完成的 pass 保留。

### MIDI

`File ▸ Export MIDI…` 把 MIDI 轨写成标准 MIDI 文件（SMF）。

## 偏好设置

`Edit ▸ Preferences…`：

| 项 | 说明 |
| :--- | :--- |
| `Device` | 输出设备，`System default` 是跟随系统 |
| `Sample rate` | 设备默认，或 44.1 / 48 / 88.2 / 96 / 176.4 / 192 kHz |
| `Buffer size` | 输出缓冲（样数），越小延迟越低、越容易爆音 |

改完点 `Apply`：先建新引擎，成功才替换并重建整张图；失败保留旧引擎并在窗口里报错。
插拔设备后点 `Refresh devices` 重新枚举。配置存在应用数据目录，下次启动自动套用，
设备不在就回落默认并提示。
