# 术语表 — ResoZ

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
| 湿声 / 干声 | wet / dry | 只作形容词或独立名词 |
| 包络 | envelope | ADSR 音量包络 |
| 截止频率 | cutoff frequency | 字段/参数名 `cutoff` |
| 共振 | resonance | Simper SVF 语义 |
| 低通 / 高通 / 带通 | low-pass / high-pass / band-pass | 滤波模式枚举 |
| 增益 | gain | 线性增益；电平用 level |
| 声像 | pan | -1..1 |
| 静音 | mute | 通道条按钮状态 |
| 电平表 | level meter | 峰值表；峰值保持 peak hold |
| 推子 | fader | 混音器音量滑块 |
| 冲洗反规范数 | flush denormals | 指注入 0.0 消除 denormal 的处理 |

## 时间轴与音序

| 中文 | 英文 | 约定 |
| :--- | :--- | :--- |
| 走带 | transport | 播放/停止控制整体 |
| 播放头 | playhead | 当前播放位置指示 |
| 片段 | clip | 钢琴卷帘编辑的音符集合 |
| 音符 | note | MIDI 音符；音名如 `A4` |
| 音长 | gate | 音符占步长的比例；不用 length/gate time 混称 |
| 步进 | step | 步进音序器的格 |
| 循环长度 | loop length | tick 数（默认 384） |
| 吸附 | snap | 时间/音高网格量化 |
| 自动化 | automation | 参数随时间的曲线 |
| 曲线 | curve | 锚点序列实体 |
| 泳道 | lane | 自动化曲线的编辑器行 |
| 锚点 | anchor | 曲线上的控制点 |

## 文件与 UI

| 中文 | 英文 | 约定 |
| :--- | :--- | :--- |
| 项目文件 | project file | `.rez`（RON 格式） |
| 持久化 | persisted | eframe storage / 文档序列化语境 |
| 冒烟测试 | smoke test | `--seconds` 自动关窗跑通 |
| 节点画布 | node canvas | P5d 的图视图编辑器 |
| 钢琴卷帘 | piano roll | 音符片段编辑器 |
| 步进音序器 | step sequencer | 16 步网格编辑器 |
| 混音器 | mixer | 多轨电平/推子区 |
| 走带控制 | transport bar | 顶部播放/速度/文件工具条 |

## 文案风格

- **注释/doc comment**：完整英文句子，技术文档语气；模块综述 `//!` 保持原信息量。
- **日志**（`log::`）：小写开头、简洁陈述句；错误消息包含上下文值（如 `track {n}`）。
- **UI 文案**：菜单/按钮 Title Case 或短句；括号内的补充说明用 `(no X found; ...)` 形式。
- **错误枚举**（thiserror）：`#[error("...")]` 消息小写开头、含占位符上下文。
- **断言消息**：`assert!(..., "must ...")` 描述期望行为而非现状。
