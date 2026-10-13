# 编辑交互模型 Spec（M4）

> 状态：定稿（2026-10-13，经三轮拷问式 Q&A + 用户确认；同日三处修订：删 `t+e`、时间轴去 Draw、实测反馈改模态——见 §3.3）。
> 上游：`plan.md` M1–M3（命令层 / ops / 手势合并已落地）、`spec/timeline.md`（clip 模型）。
> 本 spec 定义 piano roll 与 timeline 的**编辑交互模型**，替换 M2.2–M3.8 演进出的全手势模型。

## 1. 背景与问题

M3.8 之后的现状：编辑种类 = `f(指针落点 × 修饰键)`，代码里堆了三层补丁：

- **隐藏热区**：`EDGE_PX` ±8px 边缘判定 + 平手规则（短音符全身是缘）；
- **修饰键多义**：Shift 一词四义（追加框选 / toggle / 复制拖动 / 八度微调）；
- **三分支歧义**：click / drag / 双击靠 4px 阈值区分。

实测反馈链：M3.7 拖动漂移 → M3.8 空白单击误触创建 → 边缘区 5→8px。根因：**语义决策源在指针坐标这个连续量上**——8px 热区、4px 阈值、平手规则全是对连续量强行二分的税。

目标：语义决策源移到**离散状态**（常驻状态 + 一次性命令），指针只提供参数。

## 2. 核心模型

三层，命名经用户校正（早期"工具层/模态层"表述废弃）：

| 层 | 说明 | 成员 |
| :--- | :--- | :--- |
| 常驻状态 | 长期驻留，决定左键拖动的含义；roll 与 timeline 各持一份，互不影响 | **Select**（唯一） |
| 粘性命令 | 一次唤起、可连续同类操作、Esc 退出；唯一需要"状态感"的命令 | **D**（Draw，仅钢琴窗） |
| 一次性命令族 | 按键唤起、一次操作、Esc 取消、完成后回到先前状态；**命令不改变常驻状态** | G / S / T / K / Shift+D |

**为什么只有 Select 一个常驻态**：移动/修剪等高频率变换各有一个 gesture 快捷方式（拖音符=移动、拖缘=trim），命令形态（G/T）提供轴/缘的显式限定，双入口覆盖全部频率，无需更多常驻态。

**为什么 D 是粘性而非一次性**：音符输入是爆发式的（一个乐句 5–50 个），一次性 draw（d-拖-d-拖）输入流断裂，不可用。时间轴没有 D：clip 摆放是低频刻意操作（一场编曲 5–20 个），无爆发场景，双击=创建 + 拖缘调长已覆盖（见 §4）。

**为什么命令在 draw 态下同样生效**：画完新音符自动选中（M3.8 reselect 机制），直接按 g/t 调整，成本 1 次按键而非 1 次模式往返——这消解了 Ableton 式双态"draw 态里要调整"的切换税。

### 扩展判据（未来新能力落点）

| 特征 | 落点 | 例 |
| :--- | :--- | :--- |
| 一次性空间操作（在指针位置创造/框选/切割） | 一次性命令族 | K 切刀 |
| 对已有选区的连续变换（移/缩/拉） | 一次性命令族 | G / S / T |
| 需要连续同类操作（输入爆发） | 粘性命令 | D（仅钢琴窗） |
| —— | 常驻态永远只有 Select | —— |

## 3. 状态与命令语义

### 3.1 Select（常驻态）

| 落点 | 手势 | 行为 | 命令层 |
| :--- | :--- | :--- | :--- |
| 音符 | 单击 | 单选（Shift/Ctrl toggle） | 纯 UI |
| 音符 | 拖动 | 移动选区（= G 的无限定形态，双入口） | `midi.notes_move` |
| 音符缘 | 拖动 | trim 该端（= T 的无限定形态，双入口；多选=选区 bbox 缘） | `midi.notes_edit_delta` |
| 空白 | 单击 | 取消选择 | 纯 UI |
| 空白 | 拖动 | 橡皮筋框选（Shift/Ctrl 追加） | 纯 UI |
| 任意 | Esc | 取消进行中命令；无则清选择 | —— |

M3.8 边界规则"空白拖动永远=画"**废除**：空拖在 Select 态回归框选（M2.2 原语义），误触创建音符从根上消失。

### 3.2 Draw（粘性命令，`d` 进入 · Esc 退出；仅钢琴窗）

| 落点 | 手势 | 行为 |
| :--- | :--- | :--- |
| 空白 | 拖动 | 画音符（press=on / release=off，最短一格，不超 loop） |
| 音符 | 单击 | 选中 |
| 音符 | 拖动 | 移动（调整刚画的音符无需退出 draw 态） |
| 音符缘 | 拖动 | trim |

时间轴不设 Draw 态，clip 创建走双击（见 §4）。

### 3.3 模态命令族（2026-10-13 实测反馈改定：Blender 式模态）

| 命令 | 键 | 语义 | 限定键 | 命令层 |
| :--- | :--- | :--- | :--- | :--- |
| Grab | `g` | 移动选区 | `X`=只改时间 / `Y`=只改音高（mid-modal 可切换，Blender 惯例） | `midi.notes_move` / `MoveResizeClip` |
| Scale | `s` | 时间缩放：`k = 指针距 pivot ÷ 抓取距 pivot`；pivot=**选区包围盒起点**（从起点缩放保持音符间相对关系，2026-10-13 实测指认）；**不吸附网格**；长度钳 ≥1 tick | 无 | **新增 `midi.notes_scale`** |
| Stretch | `t` | 缘拉伸，**默认拉终点**（修长度是高频场景） | `t+s`=拉起点（mid-modal 可切） | `midi.notes_edit_delta` |
| Cut | `k` | 切刀：**沿划动轨迹切割所有被穿过的音符**（按轨迹与音符行交点取 tick），**无需预选**；单击=零长划（切过指针 tick 的全部音符）；切口在松开时一批落下（一次 stroke=一步 undo） | 无 | **新增 `midi.notes_split`（cuts 批量）/ `clip_split`** |
| Duplicate | `Shift+d` | 复制选区并移动：复制→选中副本→立即进入模态 Grab；复制与确认移动共享 gesture id=一步 undo | `X`/`Y` 同 Grab | `midi.notes_duplicate` + `notes_move` |

**G/S/T/Shift+D 的模态语义**（实测反馈：不要"按键后还要拖动"）：

- 按键即进入模态：选区以 **ghost 半透明预览**实时跟随指针（逐帧重算，不写文档），LMB 单击**落定**（发射一次性 intent），RMB/Esc **取消**（预览未落盘，取消零成本、零 undo）；
- **拖动不是模态的手势**——拖动是"没有 G 命令时"的 tweak 手势（§3.1 的双入口）；
- 因模态期间文档不变，选区 index 全程有效，落定 intent 一步算完（无增量发射）；
- 模态期间命令族不链接：`d/g/s/t/k` 被忽略（`s` 在 Stretch 模态内=start 限定），只有 X/Y/Esc/RMB 有响应。

要点：

- **bare=最常见变体，限定键=次常见变体**（统一规则）：G 最常见是自由移动，故 X/Y 做轴约束；T 最常见是拉终点，故 S 切起点。**`t+e` 已删除**——与裸 T 完全重合，纯冗余（2026-10-13 用户指认）。bare T 默认拉终点而非"指针在 bbox 左/右半侧决定"——后者把语义决策权交回指针位置（本模型要消除的），且窄选区不稳定。
- **多选同时拉伸**（用户确认）：拉起点=所有选中音符 start 跟随指针（bbox 左缘）；拉终点=所有 length 延伸（bbox 右缘）。
- **Shift+D 与 Ctrl+拖动复制的双入口**：Ctrl+拖=复制移动 gesture（M3.8 已有，保留为快捷方式）；Shift+D=显式命令形态。二者共享"一次复制拖动=一步 undo"。
- **限定键的上下文消歧**：`s` 在无命令时=Scale 模态；在 `t` 模态内=Stretch 的 start 限定。
- **空选区**：G/S/T/Shift+D = no-op（状态栏提示）；K 不需要选区（位置工具），无落点 = no-op。
- **Delete 键删除选区**（2026-10-13 实测反馈：RMB 之外补键盘路径）。
- **按键消费**：roll 持焦时所有已处理按键 `consume_key`，`EventFilter` 认领 Escape/Tab/箭头——否则 egui 的箭头焦点导航会把焦点挪走（nudge 一次即失效），未消费按键在部分平台触发系统 beep。

## 4. 分编辑器工具集

| | 钢琴窗 | 时间轴 |
| :--- | :--- | :--- |
| 常驻态 | Select | Select |
| 粘性命令 | D | —（无） |
| 一次性命令 | G · S · T · K · Shift+D | G · T · K · Shift+D（**S 砍掉**；Shift+D 为 M4b 补齐） |

时间轴差异（用户确认 + 推导）：

- **S 砍掉**：clip 只是时间容器，内容时间伸缩没有音乐语义；要伸缩内容回 piano roll 对着音符做；
- **无 Draw**（2026-10-13 用户指认）：粘性 D 的唯一存在理由是输入爆发，clip 摆放无此场景；双击空白轨=建 1 bar clip（T3 已有），拖缘/T 调长。Logic 式"拖出创建"不作为一期——若补，作一次性命令而非粘性态；
- **K**：K 唤起切刀（可连续切），点击把指针下 clip 在整 bar 吸附处切成两个；两半各 ≥1 bar，否则一帧状态提示拒绝；内容为 loop 克隆（两半同 loop）；
- **clip 多选（M4b 已交付）**：点击/Ctrl 追加/跨轨框选；Delete 批删一步 undo；
- **G（M4b 决策）**：自由移动 = 时间 bar delta + 同类轨 row delta（刚性整体移动，行 delta 全局钳制不逐 clip）；X 切换时间锁（只水平）；
- **T（M4b 决策）**：默认拉终点，Shift+T 拉起点；bar 对齐、≥1 bar、不出编排边界；
- **Shift+D（M4b 决策）**：克隆选区进入 grab 式模态，LMB 落定副本、RMB/Esc 副本不落盘；落定后选区转移到副本；
- **重叠策略（M4b 决策）**：部分重叠允许（LMMS 式弱起场景）；目标**完全覆盖**既有 clip 时该 clip 留原位 + 状态提示，批次其余照常落下。

## 5. 撤销与合并

沿用 M1.4 / M3.7 基建，无新机制：

- 一次拖动 = 一步 undo：`execute_merged` + gesture id；
- 命令 + 后续拖动共享 gesture key：Shift+D 的复制与移动、K 的连续划切（一次调用内）；
- 命令唤起后 Esc/RMB = 取消模态（ghost 预览未落盘，无实际编辑则不记录 undo）；
- 落定（LMB）= 一次 intent = 一步 undo；K 一次 stroke 的全部切口一批落下 = 一步 undo；
- undo/redo 后选区经 `reselect_from` 重解析（排序可能置换 index，M3.8 机制）。

## 6. 实现映射

### 复用（零改动）

- 意图层：`RollIntent` / `TimelineAction` / `LaneIntent` 枚举不变；
- 命令：`midi.notes_move` / `notes_edit_delta` / `notes_duplicate` / `note_add` / `note_edit` / `MoveResizeClip` / `AddClip`；
- 选区机制：`RollState.selection` / `retarget` / `reselect_from`（M3.8）；
- `execute_merged` + gesture id；`tests/egui_event_semantics.rs`（egui 0.36 click/drag 判别锁）。

### 新增

| 项 | 内容 |
| :--- | :--- |
| 命令 `midi.notes_scale` | `{indices, pivot_tick, factor}`；factor 由 UI 算（k=指针距/抓取距，pivot=选区 bbox 起点），命令层钳制 |
| 命令 `midi.notes_split` | `{cuts: [{index, tick}]}`：按各 tick 切分指定音符（pitch/velocity 保留，两半 length≥1）；单击=单项，knife stroke=批量 |
| 命令 `clip_split` | `{track, clip, tick}`：clip 在 tick 处切为两个（span 校验 ≤8/轨） |
| 批量动作（M4b） | `SplitClip` / `MoveClipsBatch`（`relocate_in`：源轨降序 remove + 目的轨 append，append 不位移索引）/ `StretchClipsBatch` / `DuplicateClipsBatch`——各一步 undo；满轨静默拒绝 |
| 交互状态机 | `RollState` / `TimelineState` 增 `state: Select`（roll 另有 `Draw`）+ `modal`（G/S/T/Shift+D 模态变换，ghost 预览）+ `knife_armed`/`knife`（K）；按 `consume_key` 消费已处理按键 |
| 视觉反馈 | 工具栏/状态栏显示常驻态与模态命令 chip（含限定键高亮）；模态 ghost 预览、knife 划痕；cursor 随落点/状态变化 |
| 测试 | 状态 × 落点 × 限定键 → intent 映射矩阵测试；模态 LMB 落定/RMB·Esc 取消；knife 轨迹切割（含无选区）；Delete 键；箭头连续 nudge；空选区 no-op |

velocity lane 保持独立手势（刷力度），不进状态机。

## 7. 明确不做

- Reaper 式全可配置 modifier 矩阵；
- FL/Pro Tools 式常驻工具组（双态 + 命令族已覆盖其场景）；
- Blender 式 knife 多段连切（K 一次调用一批切口；逐点连切未来可评估 `Shift+k`）。

## 8. 分阶段

- [x] **M4a 钢琴窗**：状态机（Select/Draw）+ G/S/T/Shift+D/K + 视觉反馈 + 映射矩阵测试（2026-10-13 交付，M4a.4 实测修订为模态语义）
- [x] **M4b 时间轴**：Select/T/K + clip 多选 + Shift+D + G（无 S；2026-10-13 交付）
- [ ] **未来命令族扩展**：quantize / legato / glue / transpose = 同族零侵入

## 附：主流 DAW 交互家族（选型依据 2026-10-13）

| 家族 | 代表 | 机制 |
| :--- | :--- | :--- |
| 双态切换 | Ableton / Bitwig | Edit 态 + Draw 态（Ctrl+B），变换全走手势 |
| 常驻工具组 | FL Studio / Cubase / Logic / Pro Tools | 工具常驻可见，靠切换 + 肌肉记忆 |
| 修饰键矩阵 | Reaper | 无常驻工具，context × modifier 全可配置 |
| 参照系 | Blender | 常驻工具 + G/R/S/K 一次性模态算子 |

无一家采用"全部平铺的一次性工具"；一次性命令（split/duplicate/quantize）在 DAW 中本就大量存在且均作用于选区。本方案 = 双态（Ableton 家族）+ 命令族（Blender 式一次性模态）。
