# 平台键位与提示修缮（macOS / Linux）

> ID: F-050 · 域: UI / PLUG
> 状态：**已交付**（2026-11-25；实施要点见 `implementation.md` F-050）
> 关联：`snap.md`（S1/S6 的 Ctrl 语义）、`clipboard.md`（D4 已用 `Modifiers::COMMAND`）、`timeline.md`（D31 `Ctrl` + 中键）、`plugin-gui.md`（§4 内嵌仅 Windows）、`recording.md`（WASAPI loopback 的平台边界）

行号基于 `2f2b0da`（2026-11-25）。实施时以符号为准，行号只作定位。

## 1. 问题

项目长期只在 Win64 上实测。朋友反馈 macOS 上"没有 Delete 键、Backspace 删不掉"——已由 `af9ee07` 修掉（`Key::Delete` / `Key::Backspace` 双收）。同类问题还有一整批没有修：macOS 上 `Ctrl` 与 `Cmd` 是两个不同的修饰键，而代码把这个前缀当成了一件事。

egui 0.37 的平台语义（`egui-winit` `lib.rs:556-564`）：

| 按下的键 | `modifiers.ctrl` | `modifiers.mac_cmd` | `modifiers.command` |
| :--- | :--- | :--- | :--- |
| macOS `Control` | true | false | **false** |
| macOS `Cmd` | **false** | true | true |
| Windows/Linux `Ctrl` | true | false | true（`command = ctrl`） |

`consume_key` 的匹配规则（`egui` `data/input/modifiers.rs:300-328`）：`pattern.ctrl` 要求 `self.ctrl` 为真；`pattern.command` 要求 `self.command` 为真。于是：

- `Modifiers::COMMAND` 在三个平台都对（Win/Linux = Ctrl，macOS = Cmd）——**这是项目里唯一正确的写法**，目前只有剪贴板（`clipboard.rs`、`piano_roll.rs:2901-2903`、`timeline.rs:4517-4518`、`patch.rs:5409-5411`）用了它。
- `Modifiers::CTRL` 在 macOS 上只认物理 Control，**Cmd 全部失效**。
- 直接读 `i.modifiers.ctrl` 在 macOS 上同样只认物理 Control。

也就是说：同一个按键族里，滚轮缩放、吸附开关、标尺画 loop 已经接受 `Cmd`（它们写的是 `ctrl || command`），而全选、成组、加选、绘制都只认物理 `Ctrl`。这不是"少一个功能"，是同一套语义两半各走一边。

### 1.1 全局快捷键：不只是失效，还会误触发音

`keyboard.rs:692-728` 用 `if modifiers.ctrl && pressed` 做全局快捷键白名单（`Ctrl+S/N/O/Z/Y/L`），`_ => continue` 兜住"其他 Ctrl 组合不发音"。macOS 上 `Cmd` 不进这个分支，事件落到 `:730` 的 `(key, pressed)` 匹配与 `KEYMAP`（`lib.rs:231`，A/W/S/E/D/F/T/G/Y/H… 的音位）：

| 按下 | 实际发生 |
| :--- | :--- |
| `Cmd+Z` | 命中 `:731` `Key::Z` → **降八度**（不是撤销） |
| `Cmd+S` / `Cmd+D` / `Cmd+G` | 落到 `KEYMAP` → **弹出一个音**（不是保存 / 不是命令族） |
| `Cmd+A` | `KEYMAP` 的 A → 弹 C4（不是全选） |
| `Cmd+X` | 被 `egui-winit` 折成 `Event::Cut` 后丢弃（无副作用，但 Cut 无人处理） |

比"快捷键不生效"更糟：用户在 Mac 上按习惯的 `Cmd+Z`，文档被改了八度、还发了一声。

### 1.2 鼠标侧：修饰键读取不统一

| 位置 | 语义 | macOS 现状 |
| :--- | :--- | :--- |
| `timeline.rs:4147` / `4185` / `4366` | Ctrl+点击加选、框选 additive | `Cmd+点击` 变成"替换选择"，空处点击清空 |
| `patch.rs:3782` → `3887` / `4053` | `Ctrl`+右键拖切线、`Shift+A` 守卫 | `Cmd` 不切线；右键反而落进添加菜单 |
| `piano_roll.rs:2460` | `additive = shift \|\| ctrl` | `Cmd+点击` 不加选 |

对照已经正确的：`piano_roll.rs:2223` / `2406` / `2455` / `3917`、`timeline.rs:1612` / `2104` / `3548` / `4389`、`status_bar.rs:191`（全部 `ctrl || command`）。

### 1.3 手势：触控板没有中键、捏合被守卫挡住

- **中键**：`patch.rs:4086`（平移）、`timeline.rs:1683`（平移）、`piano_roll.rs:2274`（双轴平移）、`mixer_panel.rs:102`（条带平移）、`timeline.rs:4387`（`Ctrl`+中键静音）。MacBook 触控板与 Magic Mouse 没有中键。
  - Patch 最严重：滚轮被定义为缩放（`patch.rs:3755-3767`），平移只有中键一条路 → 触控板上**完全不能平移**。
- **捏合**：`egui-winit` 把 `PinchGesture` 折成 `Event::Zoom`（`lib.rs:597-606`），进 `zoom_delta` 时**不带任何修饰键**。卷帘（`piano_roll.rs:2222-2224`）与时间轴（`timeline.rs:1617`）都写成 `if ctrl { … }` 才处理，于是捏合被整段跳过；只有 Patch 认。`timeline.rs:1622` 的注释写着"the touch pinch case"，守卫把它挡在门外。

### 1.4 键盘之外

- **插件默认搜索目录只有 Windows 的**：`clap-host/src/lib.rs:122-140`、`vst3-host/src/lib.rs:166-187` 只读 `ProgramFiles` / `LOCALAPPDATA` / `APPDATA`。macOS / Linux 上只能靠 `GLITCHERZ_CLAP_PATH` / `GLITCHERZ_VST3_PATH`，标准目录一个都没加。（VST3 bundle 的 `Contents/<platform dir>/` 查找已正确：`raw.rs:46-79`。）
- **"WASAPI loopback" 文案不设平台门**：`settings.rs:286` / `351`、`patch.rs:5206` 在 macOS 上照样列出"what this output plays"，那里没有这个能力。
- **界面提示与菜单一律写 `Ctrl`**：`transport.rs:131-236`（`shortcut_text`）、`patch.rs:3735`、`piano_roll.rs:1992`、`timeline.rs:1731` / `1964` / `2277`、`export_ops.rs:288`、`status_bar.rs:199`、`transport.rs:308`。
- **文档口径自相矛盾**：`docs/shortcuts.md:3` 写"Ctrl 是物理 Control 键，macOS 上也是（不是 Cmd）"，`spec/snap.md:24` 写"Ctrl（macOS Cmd）"，`spec/clipboard.md` D4 把 Cmd 缺口称作"历史问题"。三处必须先统一，否则代码改成什么样都能找到"文档依据"。

## 2. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| D1 | **命令类 = `Modifiers::COMMAND`，同时接受物理 `Ctrl`** | 三平台等价：Win/Linux `Ctrl`、macOS `Cmd`。`Modifiers::COMMAND` 在 macOS 上不认物理 `Control`，而物理 `Control` 在 Mac 上仍有用户习惯（且部分 chord 与系统不冲突），所以统一 helper 两个 pattern 都试，**先具体后笼统**（`Shift+A` 先于 `A`）。 |
| D2 | **"按住改手势"同样走 `mod1`** | 吸附开关、加选、切线、框选 additive 全部 `ctrl \|\| command`。与 D1 合成一个概念：**mod1 = 命令修饰键**。 |
| D3 | **收口到一个模块** | 新模块 `crates/glitcherz-ui/src/mod_keys.rs`：`consume_mod1(i, key)`、`consume_mod1_shift(i, key)`（或接受 `Modifiers` 的组合形式）、`mod1_held(i)`、`mod_key_label()`。`lib.rs` 加 `mod mod_keys;`。禁止在业务代码里再写 `Modifiers::CTRL` / `i.modifiers.ctrl`（白名单见 D5）。 |
| D4 | **`keyboard.rs` 的守卫改 mod1，未知组合不发音** | `:692` 用 `mod1_held`；`_ => continue` 保留。这样 `Cmd+字母` 永远进不了 `KEYMAP`，Mac 上不再"按 Cmd 弹音"。同时 `:599-648` 的 `!modifiers.ctrl` / `modifiers.ctrl` 一律换 `mod1`。 |
| D5 | **唯一例外：`Ctrl+Tab` 保持物理 Control** | macOS 的 `Cmd+Tab` 是系统应用切换器；chip 焦点轮换留 `Ctrl+Tab` / `Ctrl+Shift+Tab`（`editing.rs:397-408` 不动，仅补注释与文档说明）。同理 `Ctrl+←/→` 改为 `mod1` 后 `Cmd+←/→` 可用，物理 `Ctrl+←/→` 在 macOS 默认绑"切换空间"，属于系统抢占、我们不与之争（文档写明）。 |
| D6 | **不重复造判定** | 已经写 `ctrl \|\| command` 的地方（1.2 对照表）只做**表达统一**（换成 helper），不加新行为，避免"改一处忘一处"。 |
| D7 | **捏合不再要求修饰键** | 卷帘 / 时间轴改成"`zoom_delta_2d` 非中性即缩放"：纯捏合、`Cmd/Ctrl+滚轮`、`Opt/Alt+滚轮` 走同一分支；`wheel` 分支只在缩放中性时处理滚动。Patch 已经是这个形状，不动。 |
| D8 | **Patch 平移给触控板补一条路** | 中键拖动保留；新增 `Shift+滚轮` = 双轴平移（`egui` 的 `horizontal_scroll_modifier` 已把 `Shift` 下的纵向 delta 折进 `wheel.x`，两轴都能拿到）。纯滚轮仍是缩放（保持 F-047 的"滚轮即缩放"结论）。不选 `Alt+拖`：Linux 多数 WM 用 `Alt+拖` 搬窗口。 |
| D9 | **中键静音保留，`H` 作为触控板入口** | `Ctrl`+中键（D31）不动；文档把 `H` / `Alt+H` 写在"触控板无中键时"的替代位置。 |
| D10 | **插件目录按 OS 分支，Windows 列表零改动** | 抽 `standard_*_dirs_for(os, env)` 纯函数（注入 OS 与环境变量，可在 Windows 上测 macOS/Linux 分支）；环境变量仍最优先。macOS CLAP：`/Library/Audio/Plug-Ins/CLAP`、`~/Library/Audio/Plug-Ins/CLAP`；macOS VST3：`/Library/Audio/Plug-Ins/VST3`、`~/Library/Audio/Plug-Ins/VST3`；Linux CLAP：`~/.clap`、`/usr/lib/clap`、`/usr/local/lib/clap`；Linux VST3：`~/.vst3`、`/usr/local/lib/vst3`、`/usr/lib/vst3`。`~` 用 `HOME`（Unix）解析，不引新依赖。 |
| D11 | **WASAPI 文案按平台** | 非 Windows 不显示 loopback 选项与说明；`settings.rs` / `patch.rs:5206` 一处判定。 |
| D12 | **提示文案按平台渲染** | `mod_key_label()` → macOS `⌘`、其余 `Ctrl`；`Alt` / `Shift` 照字面（`⌥`(U+2325)、`⌃`(U+2303) 字形缺失，见 `glossary.md:199`，不印）。`⌘`(U+2318) 栈里有（`implementation.md` F 系列字符核对）。菜单 `shortcut_text` 与所有静态提示行走同一个 helper。 |
| D13 | **文档口径统一到 D1** | `shortcuts.md:3` 改写为"下文 `Ctrl` 在 macOS 上读作 `⌘`，唯 `Ctrl+Tab` 例外"；表格不再逐行重复两种写法。`snap.md:24` 已与 D1 一致；`clipboard.md` D4 删掉"历史问题"注记（本特性即该缺口的收口）。 |
| D14 | **测试锁平台语义，不锁平台** | 集成测试用两种修饰键组合喂同一段事件：`{ctrl:true, command:true}`（Win/Linux）、`{command:true, mac_cmd:true, ctrl:false}`（macOS Cmd）→ 断言行为一致。现有 `ctrl()` fixture 补 `cmd()` 变体，不改已有断言。 |
| D15 | **不动格式与引擎** | 无 RON / `PROJECT_VERSION` 变更；不碰 `realtime/`；插件宿主只改扫描目录，不改加载/实例化。 |
| D16 | **不做** | 输入法（中文轨名）在 macOS 上的实测与修；键盘布局重映射（AZERTY / QWERTZ 的音位）；插件 GUI 内嵌的 macOS/Linux 实现（`plugin-gui.md` 已定 Windows-only）；`Cmd+W` / `Cmd+Q` 的自定义菜单项；`⌥` / `⌃` 字形补齐。 |

## 3. 改动清单

按文件，逐条对应决策。**「改」= 换 helper / 换 pattern；「补」= 新增行为。**

| 文件 | 位置 | 改动 | 决策 |
| :--- | :--- | :--- | :--- |
| `ui/src/mod_keys.rs` | 新文件 | `consume_mod1` / `consume_mod1_with` / `mod1_held` / `mod_key_label` + 单测 | D1–D3, D12 |
| `ui/src/lib.rs` | 模块表 | `mod mod_keys;` | D3 |
| `ui/src/keyboard.rs` | `:599` `:606` `:613` `:621` `:628` `:669` | `!modifiers.ctrl` / `modifiers.ctrl` → `!mod1_held` / `mod1_held` | D1, D4 |
| `ui/src/keyboard.rs` | `:635` `:642` | `Ctrl+←/→` → `mod1`（`Cmd+←/→` 生效） | D1, D5 |
| `ui/src/keyboard.rs` | `:692-728` | 白名单判定改 `mod1_held`；`_ => continue` 保留 | D4 |
| `ui/src/editing.rs` | `:397-408` | **不改**，补注释说明为何留物理 Ctrl；`docs` 同步 | D5 |
| `ui/src/piano_roll.rs` | `:2892-2896` | `Modifiers::CTRL` → mod1（`Shift+A` 先） | D1 |
| `ui/src/piano_roll.rs` | `:2460` | `shift \|\| ctrl` → `shift \|\| mod1` | D2 |
| `ui/src/piano_roll.rs` | `:2222-2224` | 去掉 `ctrl` 守卫（D7） | D7 |
| `ui/src/timeline.rs` | `:1501-1509` | `Ctrl+A` / `Ctrl+Shift+A` → mod1 | D1 |
| `ui/src/timeline.rs` | `:4547-4554` | `Ctrl+G` / `Ctrl+Shift+G` → mod1 | D1 |
| `ui/src/timeline.rs` | `:4147` `:4185` `:4366` | 加选 / 框选 / 点选读 `mod1_held` | D2 |
| `ui/src/timeline.rs` | `:1617` | 去掉 `ctrl` 守卫（D7） | D7 |
| `ui/src/timeline.rs` | `:4674-4675` `:4706-4707` | `!ctrl && !command` 换成 `!mod1`（行为等价，表达统一） | D6 |
| `ui/src/patch.rs` | `:3782` | `mod1_held`（切线、`Shift+A` 守卫、右键互斥一并生效） | D2 |
| `ui/src/patch.rs` | `:3755-3776` | `zoom_delta` 之外补 `Shift+滚轮` 平移分支 | D8 |
| `ui/src/transport.rs` | `:131` `:138` `:158` `:165` `:226` `:236` | `shortcut_text` 走 `mod_key_label()` | D12 |
| `ui/src/transport.rs` | `:308` | hover 文案同 D12 | D12 |
| `ui/src/patch.rs` | `:3735` | 静态提示行：`Ctrl` → label；补 `Shift+wheel` 平移 | D8, D12 |
| `ui/src/piano_roll.rs` | `:1992` | 同上 | D12 |
| `ui/src/timeline.rs` | `:1731` `:1964` `:2277` | 同上（`:1964` 同时是"触控板无中键"的唯一指引位） | D9, D12 |
| `ui/src/export_ops.rs` | `:288` | 同上 | D12 |
| `ui/src/status_bar.rs` | `:199` | hover "Ctrl is held" → label | D12 |
| `ui/src/settings.rs` | `:286` `:351` | 非 Windows 隐藏 loopback | D11 |
| `ui/src/patch.rs` | `:5206` | 同上 | D11 |
| `clap-host/src/lib.rs` | `:122-150` | `standard_clap_dirs_for` + OS 分支 + 纯函数单测 | D10 |
| `vst3-host/src/lib.rs` | `:166-198` | 同上 | D10 |
| `ui/tests/` | `roll_state_machine.rs` / `timeline_focus.rs` / `timeline_mute.rs` | 补 `cmd()` fixture，同一断言跑两种修饰键 | D14 |
| `docs/shortcuts.md` | `:3` 与全篇 | 顶部口径改写；`Ctrl+Tab` 例外标注；中键给 `H` 替代 | D13 |
| `docs/manual.md` | `:22-32` `:91` `:111` `:174` `:210-216` `:259` `:292` | 同上口径；`Ctrl` 写法不动，顶部一句映射 | D13 |
| `docs/README.md` | `:75` `:88` | 同上 | D13 |
| `.agents/glossary.md` | 词条区 | 登记 `mod1`（命令修饰键）与 `⌘` 的用法 | D12 |
| `.agents/spec/snap.md` | `:24` | 保持（已与 D1 一致）；`Ctrl` → mod1 表述 | D13 |
| `.agents/spec/clipboard.md` | D4 | 删"历史问题，不在本特性范围" | D13 |
| `AGENTS.md` | 文档索引 | 加 `platform-keys.md` | — |

## 4. 里程碑

| 里程碑 | 内容 | 依赖 | 规模 |
| :--- | :--- | :--- | :--- |
| **M1** helper | `mod_keys.rs` + `mod mod_keys;` + 单测（两种修饰键组合、`Shift+A` 先于 `A`、label 两平台） | — | 小 |
| **M2** 全局键 | `keyboard.rs`（守卫 / 白名单 / `Cmd+←→`）+ `editing.rs` 注释 + 回归：`Cmd+Z` 不再降八度 | M1 | 小 |
| **M3** 视图键与鼠标 | 卷帘 `A`/`Shift+A`/additive、时间轴 `A`/`G`/加选、patch 切线 | M1 | 中 |
| **M4** 手势 | 捏合（卷帘 / 时间轴）+ patch `Shift+滚轮` 平移 + 提示行 | M3 | 中 |
| **M5** 文案 | 全部 `shortcut_text` 与静态提示行走 label | M4 | 小 |
| **M6** 插件目录 | CLAP / VST3 的 OS 分支 + 纯函数单测（Windows 上验 mac/Linux 分支） | — | 小 |
| **M7** 文档 | `shortcuts` / `manual` / `README` / `glossary` / spec 注记 | M5、M6 | 小 |

顺序：**M1 → M2 → M3 / M6（可并行）→ M4 → M5 → M7**。M1–M3 建议合成一个或多个 commit 但**不要跨里程碑混提交**，每步都能单独回滚。

## 5. 验收

1. **macOS（真机或 D14 的合成事件）**：`Cmd+S` 保存、`Cmd+O` 打开、`Cmd+Z` 撤销且**不降八度**、`Cmd+Shift+Z`/`Cmd+Y` 重做、`Cmd+A` 全选（时间轴与卷帘各自作用域）、`Cmd+G`/`Cmd+Shift+G` 成组/解组、`Cmd+←/→` 按小节跳、`Cmd+L` 试听接线。
2. **物理 `Ctrl` 在 macOS 上仍可用**（`Ctrl+S` 等），`Ctrl+Tab` / `Ctrl+Shift+Tab` 仍是 chip 焦点轮换的唯一入口。
3. **Win/Linux 零回归**：既有测试（`roll_state_machine` / `timeline_focus` / `timeline_mute` / `egui_event_semantics`）断言全部原样通过；`Ctrl+中键` 静音、`Ctrl+滚轮` 缩放、`Shift+拖` 加选行为不变。
4. **审计**：`rg -n "modifiers\.ctrl|Modifiers::CTRL" crates/glitcherz-ui` 只剩 `mod_keys.rs` 内的实现与 `editing.rs:397-408`（带注释）。
5. **手势**：卷帘与时间轴的捏合能缩放（无修饰键）；Patch 在触控板上能用 `Shift+滚轮` 平移；三处提示行不再教用户按中键。
6. **文案**：macOS 上菜单与提示行显示 `⌘`，Win/Linux 显示 `Ctrl`；无新 tofu（只用到 `⌘`）。
7. **插件**：`clap_search_paths()` / `vst3_search_paths()` 在 macOS / Linux 返回标准目录（单测注入 OS）；Windows 返回值与改造前逐项一致。
8. **构建门禁**：`cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace` 全绿（沙箱外）。

## 6. 风险

| 风险 | 说明 | 缓解 |
| :--- | :--- | :--- |
| 双 pattern 导致"一个键跑两条命令" | 同时接受 `COMMAND` 与 `CTRL`，若顺序写错（笼统先于具体）会吞掉 `Shift+A` | helper 内部固定"先带 `Shift` 后裸键"；`mod_keys` 单测锁顺序 |
| macOS 系统抢占 chord | `Cmd+Tab` / `Cmd+Space` / `Cmd+Q` / `Cmd+H` / `Ctrl+←→` 都由系统或 winit 默认菜单先吃 | D5 显式留物理 `Ctrl+Tab`；文档列出"被系统占用"的组合；验收只断言能到的那些 |
| 捏合与滚轮同分支后误缩放 | 触控板惯性滚动（`MouseWheelSource::Momentum`）在卷帘被当成缩放 | 只在 `zoom_delta` 非中性时进缩放分支（D7）；滚动仍走 `smooth_scroll_delta`，两条通道互斥 |
| Patch `Shift+滚轮` 与框选 `Shift` 叠加 | 按住 `Shift` 拖框时再滚轮会变成平移 | 可接受（Blender 也如此）；提示行写明 |
| 插件目录误扫 | macOS 上 `~/Library/Audio/Plug-Ins` 下大量第三方 bundle，扫描变慢或崩 | 只扫规范目录、非递归、`.clap`/`.vst3` 后缀；慢与崩都记在 `plugin-browser.md` 既有失败处理路径 |
| 文案改动面广 | `shortcut_text` / 提示行十余处，容易漏 | M5 用 `rg "Ctrl\+"` 全仓清查（含 `docs/`），验收第 4 条同法 |
| 无法在 Windows 上验 macOS | 键位行为只能靠合成事件 | D14 的两种修饰键矩阵 + 真机 checklist（第 5 节 1–2 条）由有 Mac 的人跑一遍 |

## 7. 明确不做

- macOS 中文输入法（IME）在文本框里的实测与修——单独记，不进本特性。
- 非 QWERTY 布局的电脑键盘音位重映射（`KEYMAP` 用的是逻辑键）。
- 插件 GUI 内嵌的 macOS / Linux 实现（`plugin-gui.md` §4 已定 Windows-only）。
- 可配置键位表 / 用户自定义快捷键。
- `⌥`(U+2325) / `⌃`(U+2303) 字形（栈里没有，`glossary.md:199` 已记录）。
- 触控板三指/四指手势、`DoubleTapGesture`（`egui-winit` 直接丢弃，见 `lib.rs:590-595`）。
