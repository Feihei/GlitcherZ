# 术语表 — GlitcherZ

> 代码注释、doc comment、日志、UI 文案、错误消息一律使用英文，且**术语必须与本表一致**。
> 新概念出现时先在这里登记再使用，避免同义漂移（如 delay/effect/FX 混用）。

## 架构与线程

| 中文 | 英文 | 约定 |
| :--- | :--- | :--- |
| 实时 | real-time | 缩写 RT 仅用于复合词 `RT-safe` |
| 实时安全 | RT-safe | 指满足实时铁律（无分配/无锁/无系统调用/无 panic） |
| 音频回调 | audio callback | 专指 cpal 回调线程执行的路径 |
| 音频块 | audio block | 一次回调处理的帧集合 |
| 处理图 | processing graph | 引擎的节点图整体 |
| 节点 | node | 图中处理器；文档身份 = `node_id` |
| 连线 | cable | 图中的边（音频/MIDI/自动化三类） |
| 主输出 | master output | 保留节点 `MASTER_NODE_ID = 1` |
| 无锁 | lock-free | 只用于描述队列/通信，不泛化 |
| 命令队列 | command queue | 控制线程 → 实时侧的结构命令通道 |
| 环形队列 | ring queue | `rtrb` 等无锁环形缓冲 |
| 拓扑排序 | topological sort | GraphCompiler 的执行序推导 |
| 悬空 | dangling | 指针/守卫失去宿主的状态 |
| 扇出 / 扇入 | fan-out / fan-in | 节点出边数 / 入边数上限语义 |
| 定容 | fixed-capacity | 引擎侧 `Copy` 数组容量上限语义（区别于 Vec） |
| 槽位 | slot | 定容数组中的下标位置 |

## 双身份与守卫（插件宿主）

| 中文 | 英文 | 约定 |
| :--- | :--- | :--- |
| 双身份 | dual identity | 文档 `node_id`（持久化）vs 引擎侧 id（会话内单调） |
| 寻址表 | addressing table | UI 持有的 doc→engine id 映射 |
| 守卫 | guard | 延长插件动态库寿命的 RAII 持有者 |
| 退役 | retire | 守卫从在役区转入退役区等待回收 |
| 回收回执 | recycling receipt | 实时侧节点真正离图后的确认消息 |
| 插件宿主 | plugin host | CLAP / VST3 适配层统称 |
| 实例化 | instantiate | 非实时线程上创建插件实例 |
| 扫描 | scan | 枚举磁盘上的插件描述 |
| 插件目录 | plugin catalog | 扫描结果的内存表（`PluginEntry` 列表）；Recent 另存 UI 状态；spec `plugin-browser.md` |
| 插件条目 | plugin entry | catalog 的一条 `(format, path, id)`；一个模块可出多条（VST3 多 class） |
| 插件浏览器 | plugin browser | 添加/换源用的层级菜单 + 可搜索选择器整体 |
| 浏览选择器 | picker | 带过滤框的浮层列表（`PickFilter` 收窄类别/格式，`PickerPlace` 决定落点：`Add` / `Insert`） |
| 拼接落点 | picker place | 选中项的去处：`Add { at }` 悬空出生 / `Insert { cable, at }` 拼进音频线；不写 picker mode |
| 过滤框 | filter field | picker 顶部单行输入，分词 AND 匹配 name + path |
| 添加菜单 | add menu | patch 右键的多级菜单（类别 → 格式 → 条目） |
| 换声源 | swap source | 删节点 + 添加新节点，**不迁移参数**（无 Replace 动作，2026-10-22 定）；不写 replace source |
| 最近使用 | recent | picker 空查询时置顶的每类 5 条历史选择，存 eframe storage |
| 节点源 | node source | 节点出生时携带的最终声源（`NodeSource`，插件条目已含身份），取代旧的 `AddNodeKind`；不写 add node kind |

## 音频处理

| 中文 | 英文 | 约定 |
| :--- | :--- | :--- |
| 缓冲区 | buffer | 全部预分配；不出现"动态分配的 buffer" |
| 采样率 | sample rate | 不用 sample-rate 缩写 SR 于公共 API 文档（测试内部可用） |
| 声源 / 乐器 | instrument | 内置合成器与外部插件声源统称；不用 voice source |
| 通道条 | channel strip | 每轨的增益/声像/静音节点 |
| 效果器 | effect | 插入轨道链的处理节点；不用 FX / plug-in effect |
| 旁路 | bypass | remove_node 的直连语义 |
| 延迟 | delay | 时间线效果器；回声语义用 echo（测试中） |
| 音色库 | soundfont | SF2/SF3 音色文件；`VoiceSpec::SoundFont` 变体；不用"音源库"作代码术语 |
| 音色预设 | preset | SoundFont 内的 bank:program 声源；区别于插件预设（plugin preset） |
| 采样 | sample | SoundFont 内嵌 PCM 采样数据；不进文档 |
| 湿声 / 干声 | wet / dry | 只作形容词或独立名词 |
| 均衡器 | EQ / equalizer | `BuiltinEffectKind::Eq`；三段式（低架/峰值/高架）+ 低切 |
| 低架 / 高架 | low shelf / high shelf | 只抬/只降低频或高频，另一端保持单位增益 |
| 峰值（滤波） | bell / peaking | 单频段隆起或凹陷；参数为增益 + 中心频率 + Q |
| 低切 | low cut | 高通滤波；`low_cut` 取最小值即关闭，不设独立开关 |
| 压缩器 | compressor | `BuiltinEffectKind::Compressor`；前馈峰值检测 |
| 增益衰减 | gain reduction (GR) | 压缩量，单位 dB 且 `<= 0`；表头显示时取反为"压了多少" |
| 软拐点 | soft knee | `knee` 宽度的 dB 区间内二次插值，两端与硬拐点斜率连续 |
| 立体声联动 | stereo link | 左右共用 `max(|L|,|R|)` 检测器，得到同一 GR |
| 包络 | envelope | ADSR 音量包络 |
| 截止频率 | cutoff frequency | 字段/参数名 `cutoff` |
| 共振 | resonance | Simper SVF 语义 |
| 低通 / 高通 / 带通 | low-pass / high-pass / band-pass | 滤波模式枚举 |
| 增益 | gain | 线性增益；电平用 level |
| 声像 | pan | -1..1 |
| 静音 | mute | 通道条按钮状态 |
| 电平表 | level meter | 峰值表；峰值保持 peak hold |
| 推子 | fader | 混音器音量滑块 |
| 主控组 | master section | 走带条右侧的 master gain + 立体声电平表 + Panic；全局量，不属于任何节点 |
| 全停 | panic | 释放所有发声音符（逐 pitch 扫 0..127），transport 与参数不动；区别于 Stop |
| 冲洗反规范数 | flush denormals | 指注入 0.0 消除 denormal 的处理 |
| 参数槽位 | parameter slot | 内建效果器参数按 descriptor 下标寻址（`ParamId::BuiltinParam`）；不写 param index |
| 效果描述表 | effect descriptor | 每效果一张 `EffectDescriptor`：名称 + 参数规格 + 立体声类别；UI 与 DSP 的单一真源 |
| 参数平滑 | parameter smoothing | 一阶低通平滑器 `Smoothed`；"平滑"专指此机制，不泛化为 interpolation |
| 立体声联动 | stereo link | 动态类效果的检测器联动开关；不写 stereo chain / stereo couple |
| 干湿比 | mix | 参数名统一 `mix`（不用 wet/dry 作参数名） |
| 反馈 | feedback | 延迟/镶边的回授量，参数名 `feedback`；P6 的反馈**节点**写作大写 `Feedback` |

## 时间轴与音序

| 中文 | 英文 | 约定 |
| :--- | :--- | :--- |
| 走带 | transport | 播放/停止控制整体 |
| 播放头 | playhead | 当前播放位置指示 |
| 片段 | clip | 钢琴卷帘编辑的音符集合 |
| 音符 | note | MIDI 音符；音名如 `A4` |
| 延音尾 | gate tail | 音符 off 事件前的延续播放（pending off 机制）；项目级"音长 gate"已随 Steps 退役，音符收尾用自身 length |
| 循环长度 | loop length | tick 数（默认 384） |
| 吸附 | snap | 时间/音高网格量化 |
| 自动化 | automation | 参数随时间的曲线 |
| 曲线 | curve | 锚点序列实体 |
| 泳道 | lane | 自动化曲线的编辑器行 |
| 锚点 | anchor | 曲线上的控制点 |

## 文件与 UI

| 中文 | 英文 | 约定 |
| :--- | :--- | :--- |
| 项目文件 | project file | `.glz`（RON 格式） |
| 持久化 | persisted | eframe storage / 文档序列化语境 |
| 冒烟测试 | smoke test | `--seconds` 自动关窗跑通 |
| 节点画布 | node patch | P5d 的 patch 图视图编辑器（Pure Data 语境，连线图叫 patch） |
| 钢琴卷帘 | piano roll | 音符片段编辑器 |
| 混音器 | mixer | 多轨电平/推子区 |
| 走带控制 | transport bar | 顶部播放/速度/文件工具条 |

## 编辑交互（M4，spec/editing-model.md）

| 中文 | 英文 | 约定 |
| :--- | :--- | :--- |
| 常驻状态 | persistent state | 编辑器中长期驻留的状态，决定左键拖动含义；只有 Select 一种 |
| 粘性命令 | sticky command | 一次唤起、可连续同类操作、Esc 退出的命令；仅钢琴窗的 Draw（`d`）——时间轴无（clip 摆放低频，双击创建已覆盖） |
| 一次性命令 | one-shot command | 按键唤起、一次操作、Esc 取消、不改变常驻态；Grab/Scale/Stretch/Cut/Duplicate |
| 命令族 | command family | 一次性命令的集合称谓；新变换能力按 spec §2 判据入族 |
| 限定键 | qualifier key | 命令 pending 期间按下的上下文键：Grab 的 X/Y（轴锁定）、Stretch 的 S（起点）；统一规则 bare=最常见变体，限定键=次常见变体 |
| 选择态 | Select state | 默认常驻态；拖音符=移动、拖缘=trim、空拖=框选 |
| 绘制态 | Draw state | `d` 进入的粘性命令态（仅钢琴窗）；空拖=画音符 |
| 轴锁定 | axis lock | Grab 中按 X=只改时间 / Y=只改音高（Blender 惯例，mid-drag 可切换） |
| 整体移动 | grab | 命令 `g`；与 Select 态"拖音符"为同一操作的双入口 |
| 时间缩放 | scale | 命令 `s`；绕选区中心按比例伸缩起点与时值，不吸附网格 |
| 缘拉伸 | stretch | 命令 `t`；默认拉终点，`t+s` 拉起点（`t+e` 与裸 T 重合，已删） |
| 切分 | cut / split | 命令 `k`；在指针 tick 处把音符/clip 一分为二 |
| 复制并移动 | duplicate & move | `Shift+d`；复制选区后立即 arm Grab |
| 操作行 | action row | 节点框内、标题之下的一行小按钮（Timeline 的 `+/- MIDI` / `+/- Curve`、Mixer 的 `+/- Slot`）；占头部带高度，端口从它下方开始铺 |
| 双击路由 | double-click routing | patch 双击的目标分派：节点 → Inspector（仅当有参数可显示）、Timeline → Timeline tab、其余实体无动作 |

## 多 clip 卷帘与音高（R 系列，spec/roll-multiclip.md）

| 中文 | 英文 | 约定 |
| :--- | :--- | :--- |
| 编辑主体 | roll subject | `RollSubject`：`focus` + `context`；不写 roll target（`EditTarget` 是 tab 分派） |
| 焦点片段 | focus clip | context 中唯一可写的一个；音符编辑全部落在它身上 |
| 幽灵片段 | ghost clip | 非焦点的可见片段，低 alpha 只读渲染，**不参与命中测试**；不写 locked clip（"锁定"只由 ghost 表达，没有可切换的锁状态） |
| 编辑上下文 | roll context | 同屏可见片段集合，上限 `MAX_ROLL_CONTEXT = 8`，来源 = timeline 选区 |
| 焦点条 | focus chips | Edit tab 顶部切换 focus 的 chip 行 |
| 叠加泳道 | overlay lane | 卷帘下方、与音符共享横轴的曲线泳道；速度泳道是其特例（focus 专属） |
| 音高偏移 | pitch offset | `ParamId::PitchOffset`，f32 半音，逐样本作用于**在唱** voice（与 root note 只影响后续 voice 相反） |
| 音高曲线 | pitch curve | 驱动音高的控制曲线统称；不写 pitch envelope（那是 ADSR） |
| 弯音 | pitch bend | MIDI `0xE0`，14bit、每通道一条；曲线侧为 `AutoParam::PitchBend{channel, range_semitones}` |
| 弯音范围 | bend range | 曲线"半音"到 14bit 的换算基准，文档字段（默认 2.0，钳 0.5..24） |
| 泳道折叠 | lane collapse | 把叠加泳道压到 18px 只留标题；view-state，不落文档 |
| 音色分流 | voice routing | 同一 `AutoDest::PitchBend` 按节点自答分流：`midi_pitch_bend` 收则走 MIDI，否则回落 `PitchOffset` |

## 符号白名单（egui 默认字体，硬约束）

egui 0.36 默认字体（Ubuntu-Light + NotoEmoji + emoji-icon-font）**没有的字形一律渲染成方块**。窗口内出现的文案（按钮/标签/状态栏/浮窗）只允许以下符号：

- **允许**：`` `+` `` `-` `×`(U+00D7) `·`(U+00B7) `—`(U+2014) `–`(U+2013) `…`(U+2026) `♪`(U+266A) `▶` `■` `📂` `💾`
- **禁止（已踩坑）**：全角 `＋`(U+FF0B) / `－`(U+FF0D)、`✕`(U+2715)、`▲`(U+25B2) / `▼`(U+25BC)、`→ ← ↑ ↓`(U+2190–2193，仅等宽 Hack 字体有，窗口文案中不可用)
- **ASCII 替代**：上下滚动按钮 `^` / `v`；箭头 `->`；关闭按钮 `×`
- `→` 只允许出现在**注释与 log 消息**中（终端/编辑器渲染，不走 egui）
- 验证方法：用 fontTools 读 `epaint_default_fonts` 包内 ttf 的 cmap 对照（见 2026-10-09 工作日志），不要凭感觉猜

## 文案风格

- **注释/doc comment**：完整英文句子，技术文档语气；模块综述 `//!` 保持原信息量。
- **日志**（`log::`）：小写开头、简洁陈述句；错误消息包含上下文值（如 `track {n}`）。
- **UI 文案**：菜单/按钮 Title Case 或短句；括号内的补充说明用 `(no X found; ...)` 形式。
- **错误枚举**（thiserror）：`#[error("...")]` 消息小写开头、含占位符上下文。
- **断言消息**：`assert!(..., "must ...")` 描述期望行为而非现状。
