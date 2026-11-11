# Spec: 节拍器与录音（Metronome & Recording）

> 状态：实施中（2026-11-01）。R1 节拍器、R2 录音骨架、R3 硬件 MIDI 输入、**R6 轨输入口**、
> **R4 音频录音**、**R5 收尾**、**R7 MIDI 烘培 / R8 控制烘培**已交付；
> PDC 自动对齐并入 S2-3，Timeline 曲线出口再采样留作后续。
> 关联：`spec/audio-track.md`（音频轨播放与 trim）、`spec/tempo-meter.md`（tick 时钟 / 拍号）。
> 前置事实：引擎已有完整的 tick 时钟（`Sequencer::position_ticks` + `base_bpm` +
> `bar_ticks`）与 block 边界事件量化（≤ 1 block ≈ 2.7 ms @48 kHz/128）；
> `AudioClip` 已是「path + source_offset + source_len + start_ticks」；
> 但**数据流是单向的**（UI → 引擎），没有引擎 → UI 的事件回传通道。

## 1. 问题

引擎目前只能"播"，不能"录"，也没有可跟的拍：

- **没有节拍器**：录音时无法跟拍；即使不录音，练句也需要固定参考。
- **没有 MIDI 输入**：依赖里没有 `midir`，`engine.start()` 只建 `build_output_stream`。
  用户只能"画"音符，不能弹着录。
- **没有音频输入**：`cpal` 只用了输出侧，麦克风/乐器进不来。
- **没有采集通道**：即使有输入，也没有把「输入事件 + 精确时间戳」送回控制线程的路径。

三者不是三个独立功能：录音 = 节拍器（参考）+ 输入（来源）+ 时间戳（对齐）。
少任何一个都不可用，所以作为一个闭环设计与交付。

## 2. 核心决策

| # | 决策 | 结论 | 理由 |
| :-- | :-- | :-- | :-- |
| D1 | 节拍器位置 | **在 `BlockRunner` 末级合成**，混在图输出之后、`render_interleaved` 之前；**不进节点图** | tick 时钟在 `Sequencer` 里，只有 block 边界准（±2.7 ms）；从 UI 发 `Note` 抖动 >10 ms 跟不了拍。不进图 = 用户删掉 master 链、旁通总线，click 依然在；也天然不参与自动化/效果 |
| D2 | 导出隔离 | 离线引擎（`start_offline`）的节拍器**恒关闭**，从不接受 `SetMetronome` | 导出走同一套 `BlockRunner`，一个开关漏掉就会把 click 印进交付文件。用"离线默认关 + UI 不给离线引擎发命令"双保险 |
| D3 | click 音色 | **程序合成**，构造期预生成两张短表（重拍 / 弱拍，≈40 ms 指数衰减正弦 + 瞬态噪声），运行期只做读表 + 乘加 | 零资源文件、零运行时分配；重拍 1600 Hz / 弱拍 1000 Hz，重拍 +3 dB，辨识度高且不刺耳 |
| D4 | 触发精度 | block 边界触发，与 note 事件同策略 | click 与音符落在**同一量化点**，相对时序误差为 0；比"click 精确到采样、音符粗到 block"更好听 |
| D5 | 拍长 | `beat_ticks = TICKS_PER_QUARTER × 4 / denominator`，`bar = numerator × beat` | 与 `ticks_per_bar_of()` 同源，避免两套换算；4/4 下 960 ticks |
| D6 | 引擎侧录音语义 | 引擎**不认识 record**，只提供一条 **capture tap**：block 边界给每个输入事件打上 `position_ticks`，经 SPSC ring 回传 | 录音的过滤 / 配对 / pre-roll / 落 clip 全在 UI，引擎改动面最小，且 tap 可在其他场景复用（MIDI 学习、输入监视） |
| D7 | tap 成本 | `SetCapture(bool)` 关闭时不入队、不计数，`push` 失败只累加溢出计数（RT 安全，无日志） | 实时铁律：回调内不能分配、不能打日志。溢出必须在 UI 可见（状态栏告警），不能静默丢 |
| D8 | MIDI 输入源 | 两条：**电脑键盘**（无硬件也能验证、也能用）+ **`midir`** 硬件；两者都先发到引擎再做 tap 打戳 | 时间戳统一由引擎给，避免"键盘走 UI 时钟、硬件走引擎时钟"的两套误差 |
| D9 | 音频输入 | 新增 **`NodeDoc::Input` 图内节点**（用户裁定），`InputProcessor` 从 SPSC ring 拉帧；输入回调同时写 **两条 ring**：监听 ring（节点消费）+ 录音 ring（录音器消费） | 监听可自由路由进效果链/任意 mix，符合"节点图是一等公民"；两条 ring 让"录干声、听湿声"成立，也让不监听也能录 |
| D10 | 输入节点默认接线 | 新建时**不连任何输出**，gain 默认 0 dB；UI 明示"未连接 = 无声" | 自动连到 master 会在用户插上麦克风的瞬间啸叫。宁可多一步拖线 |
| D11 | 录音素材落盘 | 工程旁 `<stem>.media/`，clip 存**相对路径**；`Project::base_dir`（不序列化）在加载时设为工程目录 | 顺手解决 review 里「换机器全变静音」。绝对路径继续按绝对处理，插件路径不受影响 |
| D12 | 循环录音 | loop 打开时**每个 pass 覆盖上一 pass**（只留最后一遍），不做 comping / take lanes | 第一版价值最高的是"练到满意为止"，comping 是另一个量级的功能；覆盖语义可预测 |
| D13 | 录音对齐 | 先做**手动 offset**（采样数，transport 的 `Latency` 控件可调，默认 0，会话级不随工程保存）；**PDC 已于 S2-3 落地**（`latency_samples` 接进图编译期，见 `node-graph.md` §11），但录音链路自动对齐仍属后续项 | 手动 offset 已可用；自动对齐需要汇总输入设备 → 轨道链的往返延迟，PDC 只覆盖图内部分 |
| D14 | pre-roll | 录音前 `pre_roll_bars`（0–2，默认 1）内**正常播放素材与 click，但不采集**；UI 丢弃 `ticks < record_start_ticks` 的事件 | 这是 pre-roll 而非 count-in（不从负数起拍），实现只需一次比较，引擎零改动 |
| D15 | 一个 take = 一个 undo 步 | 录音停止时一次性写入 clip 的 notes，走一次 `begin_edit` / `commit_edit` | 逐音符入栈会让 undo 栈被一次录音灌满 200 步 |
| D16 | armed track 的确定 | ~~**当前选中的 MIDI 轨**……~~ **已被 D22 取代**（选中轨不再参与录音，归属由轨输入口的连线决定） | 原方案把 arming 做进选择，零额外 UI；但"选中轨"与"实际演奏路径"是两条互不相干的线，录完回放往往不是同一个乐器。R6 用显式接线取代 |
| D17 | take 的结束时机 | Stop / Pause / 走带自动停止 / 再按一次 Rec **都结束 take**，且**先落 clip 再发 Stop**（Stop 会把位置倒回 0） | Pause 后继续会产生一个跨暂停的长音；自动停止（到编排末尾）也必须落盘，否则 take 丢失 |
| D18 | 设备绑定落在哪 | 写进**文档**（`NodeDoc::Keyboard.source = MidiSource::Device(端口名)`），不做应用配置 | 绑定是"这条输入走哪个乐器"的一部分，跟着工程走才可分享/可撤销；模型早已有该字段与"两个节点不得绑同一源"的校验，R3 只是把它接上 |
| D19 | 硬件事件的送达路径 | 输入回调直接把 `EngineCommand` 推入命令队列（`CommandSender` = `Arc<Mutex<Producer>>`），**不经 UI 中转** | 经 UI 会多一帧 16–50 ms 延迟，弹奏直接不可用；回调在驱动线程（非 RT），持锁成本可忽略 |
| D20 | 硬件控制器（CC / bend） | 只发**节点寻址**（`EngineCommand::CtrlAtNode`），不进 capture tap、不走 track 扇出 | 控制器是演奏表情，必须跟随输入节点的连线；自动化录制是另一个功能，顺手捕获会造成"录一次音多出一堆 CC" |

## 3. 数据流

### 3.1 节拍器（R1，已交付）

```
UI 开关 → EngineCommand::Sequencer(SetMetronome{enabled, gain})
        → BlockRunner.apply_command → Metronome::configure
每块：sequencer.advance(ctx)
     → BlockRunner 读 position_ticks()，与上一块的 beat index 比较
       （跨过边界 / loop 回卷 → 触发一次 click，块首起音）
     → Metronome::render(&mut click_buf)         // 清空 → 叠加正在衰减的 voice
     → render_interleaved: out = graph.master + click_buf[pos]
导出：start_offline 的 BlockRunner 保持 enabled = false，且 UI 不向它发命令
```

### 3.2 MIDI 录音（R2 + R3）

```
来源：电脑键盘（UI 按键） / midir 驱动线程回调（D19：经 CommandSender 直推命令队列）
     → EngineCommand::Note / NoteAtNode / CtrlAtNode{node_id, ctrl}
       （D20：硬件 CC 与 pitch bend 只走节点寻址，不进 tap、不走 track 扇出）
     → BlockRunner 消费命令时：先发声音（监听，零额外延迟），
       若 capture 开启则 push CapturedNote{ticks: position_ticks(), event}
     → capture ring（容量 8192，满则丢弃 + 溢出计数）
绑定：Keyboard 节点的 source（D18）；UI 每帧把「设备 → 该节点连到的乐器」快照推给 hub，
     未接线且正在录音时回落 armed track（与电脑键盘同规则）
     ※ **R6 取消此回落**：归属改由「设备接在哪个轨输入口」决定，见 §5 D22/D33
UI 每帧：drain → 丢弃 ticks < record_start_ticks（pre-roll）→ on/off 配对成 Note；
         走带从后往前跳 = 循环回卷 → 清空上一 pass（D12）
停止：一次性写入 armed track 的新 clip（开始位置吸附到小节，span 向上取整到整小节）
     ※ R6 之后 = 写入「接了输入口的那些轨」，每条各一份 take
```

### 3.3 音频录音（R4）

```
cpal 流回调（capture 端点，或 render 端点的 WASAPI loopback —— R9）
  ├─→ ring A（监听）→ InputProcessor（图内节点）→ 用户接的任意下游
  └─→ ring B（录音）→ UI 每帧 drain → 内存 Vec<f32>
停止 → 写 <stem>.media/rec-<时间戳>-<track>.wav → 建 AudioClip（相对路径）
     → SetAudioClips（既有结构队列路径，RT 侧零改动）
```

**录音源**（R9 D34）：`RecordSource::{Default, Capture(name), Loopback(name)}`。
三条路都进同一个 `AudioInput`，所以监听、take 写入、干区校验、undo 全都不分叉。

### 3.4 轨输入口（R6）

接线即归属，事件级数据流（以设备 MIDI 为例）：

```
Keyboard 节点 ──MIDI out──► Timeline In · Track 3 输入口   (cable kind = Midi)
UI 派发（读 cable，不读选中状态）：EngineCommand::Note { track: 3 }
  ※ 同一设备的其它 MIDI cable（直连乐器）在接管下**休眠**：留在文档里、画成休眠样式、
    但不产生任何派发（D33）。分层请走轨出口扇出。
  → BlockRunner：先 graph.track_note(3) 扇出到"轨出口连的乐器"（＝监听，与回放同一条路）
                 再 capture.note(ticks, event, track: Some(3))
  → UI 每帧 drain → 按 track 分组 → 每组一个 take（同一 undo 步）
  → 停止 → take_clip() → 写进 midi_tracks[3].clips

其余三类源（同一条"端口 kind = cable kind"规则）：
Arpeggiator ──MIDI──► Track 5 输入口      （R7：事件派发级 tap，不进命令队列）
乐器/混音/Input ──audio──► Audio 2 输入口 （R4：节点 post 缓冲 tap）
LFO/curve ──automation──► Curve 1 输入口  （R8：块边界采样 tap）
```

## 4. 实现拆分

- [x] **R1 节拍器（引擎内，独立可交付）**
  新增 `realtime/metronome.rs`（预生成 click 表 + voice 播放 + 触发判定）；
  `Sequencer` 增加 `numerator`/`denominator` 与 `beat_ticks()`；
  `SequencerCommand::SetMetronome{enabled, gain}`；`BlockRunner` 在 `process_block`
  之后混入 click；`AudioEngine::set_metronome`；transport 区一个开关按钮；
  测试：按拍触发 / 关闭静音 / 拍号变化 / 导出隔离 / RT 零分配审计。✅ 2026-11-01
- [x] **R2 录音骨架（MIDI，无硬件可验收）** ✅ 2026-11-01
  引擎侧：`realtime/capture.rs`（`CaptureTap` + `CapturedNote{ticks, event}` + 8192 容量 ring +
  溢出计数）、`EngineCommand::SetCapture(bool)`、`AudioEngine::{set_capture, drain_captured,
  capture_overflow}`；`BlockRunner::apply_command` 在 `Note` / `NoteAtNode` 两个入口先取
  `position_ticks()` 再入库（sequencer 自己排的音符不经过命令队列，因此天然不入 ring）。
  UI 侧：`recorder.rs`（`RecordPhase::{Idle, PreRoll, Recording}` + on/off 配对 + 循环回卷
  清空 + `take_clip`）+ `start/finish/cancel/toggle_recording` + `poll_recording` 每帧 drain
  + transport 区 `● Rec` 按钮与 pre-roll 选择（0–2 小节）+ `R` 快捷键；Stop / Pause / 自动停止
  / 再按 Rec 都在倒回之前落 clip（D17），一次 take = 一个 undo 步。
  测试：capture 单元 5 项（关闭不入队 / 打戳 / 满环计数 / 关闭不丢已采集）、
  `tests/capture_rt_audit.rs` 4 项（带位置戳 / 默认关闭 / **sequencer 音符不入 ring** /
  捕获期间零堆分配）、recorder 单元 13 项（pre-roll 丢弃 / 相对位置 / 按停结束 / 重触发 /
  循环回卷 / 空 take / 边界取整 / 不 loop / 力度映射）。
- [x] **R3 硬件 MIDI 输入** ✅ 2026-11-01
  引擎：`midir 0.11` 依赖；`engine::CommandSender`（= `Arc<Mutex<Producer<EngineCommand>>>`）
  让非 UI 线程也能推命令，`AudioEngine::command_sender()` 暴露克隆；`midi_input` 模块
  （`MidiInputHub` / `MidiTarget::{Node, Track}` / `MidiInputStats`，端口枚举 + 差量开合 +
  回调内固定数组复制路由快照，零分配）；`EngineCommand::CtrlAtNode` + `RtGraph::node_ctrl`
  把 CC / pitch bend 送进既有 `dispatch_ctrl`。原始消息解析为纯函数：note on/off、
  note-on-vel-0、CC、14-bit pitch bend；系统消息、aftertouch、program change 一律丢弃
  （没有消费者，且不能让 active sensing 灌满命令队列）。
  UI：`sync_midi_bindings` 每帧把「设备 → 该节点连到的乐器」快照推给 hub（未接线且正在录音
  时回落到 armed track，与电脑键盘同规则）；Inspector 在选中键盘节点时显示输入源面板
  （电脑键盘 / 各端口，已被别的节点占用的端口禁用）、连接状态、目标乐器与"刷新设备"。
  设备绑定是**文档编辑**（一次 undo），不是应用配置（D18）。
  测试：`midi_input` 单元 8 项（note on/off、vel-0、CC、bend 居中/上下限、系统消息丢弃、
  node 目标、track 目标拒收 CC、满队列报告丢弃）+ `node_ctrl_reaches_only_the_addressed_slot`。
- [x] **R6 轨输入口（录音归属，R4 的前置）** ✅ 2026-11-01
  模型：`PortRef::TrackIn { kind, track }` + `TrackKind::{Midi, Audio, Curve}`、版本 v10、
  validate（单源 / 类型匹配 / 轨号范围，音频轨由 `Project::validate` 复核）；删除 D16 的
  "选中轨回落"（实现已无该分支）。
  `Timeline In` 端口表扩展（tempo 口保持 0 号）：patch 左边缘每轨一口 + 命中 / 连线 / 剪线；
  被接管的设备直连 cable 画成休眠样式 + tooltip。
  派发：设备的 cable 按终点分流（轨输入口 → `Note{track}`，乐器 → `NoteAtNode`），
  `EngineCommand::Ctrl{track}`；`midi_input` 绑定快照改为"节点目标 + 轨目标"（纯派生）。
  录音：`CapturedNote.track`（`NoteAtNode` 记 `None` → 丢弃）、recorder 按 track 分组多 take、
  D27 干区校验、D29 `SetRecordMute`、D30 警告、D32 punch out、D33 轨输入接管
  （派发纯派生 + 中途改接线结束该轨 take，无 mute 状态）。
  测试：validate（单源 / 类型不匹配 / 轨号越界 / 项目音频范围）、patch 端口表与休眠判定、
  sequencer `SetRecordMute`、capture 打轨戳、recorder 多轨同录；全仓 fmt/clippy/test 绿。
- [x] **R4 音频录音**（改用轨输入口，`Input` 不再带 `track` 字段）✅ 2026-11-01
  `NodeDoc::Input{node_id, channel, gain}` + `InputProcessor`（`realtime/input.rs`）；
  `AudioInput`（cpal 输入流 + monitor/take 两条 `crossbeam_queue::ArrayQueue` +
  `Arc<InputStats>` 原子量），按引擎采样率打开、只接受 f32、不重采样；
  `.media/` + 相对路径（`Project::base_dir` / `resolve_media_path` / `media_path_string`）；
  Input 节点 audio out → 音频轨输入口 = 录音归属；take 写 32-bit float mono wav，
  手动延迟 offset 平移 clip 起点。音频 + MIDI take 同一次 undo；无设备 / 未保存工程拒绝开录。
- [x] **R5 收尾** ✅ 2026-11-01
  录音后自动选中新 clip（MIDI 与音频都选中，并切到 Timeline）、take 命名
  （`Take N · bar M`）、Audio In 节点与 Inspector 的输入电平条（按节点 channel）、
  输入 ring 溢出告警。
  PDC 自动对齐待 S2-3；立体声对 / 多设备输入留作后续。
- [x] **R7 MIDI 烘培**（D26 第二类 tap）✅ 2026-11-01：`RtGraph` 在事件派发处按
  node_id + 轨位图抓取本块事件（`baked_notes`），`BlockRunner` 用块末 transport tick
  写入 capture ring；`EngineCommand::SetMidiBake{node_id, tracks}` / `ClearBakes`。
  UI 把 Arp / Portamento → MIDI 轨输入识别为可录，Rec 时下发位图、Stop 时清空。
- [x] **R8 控制烘培**（D26 第四类 tap）✅ 2026-11-01：块边界抓取控制节点的
  `NodeEvents.control`（`baked_controls`），经独立 `CapturedControl` ring 回到 UI；
  `ControlRecorder` 分组 + 循环覆盖，`control_take_clip` 生成 `ControlClip`
  （≤64 锚点，与引擎固定表一致；整小节 span）。MIDI / 音频 / 曲线 take 同一次 undo。
  **限制**：源仅支持**控制输出节点**（LFO / Envelope / Portamento / Curve Map）；
  Timeline 曲线出口 → 曲线输入的再采样尚未实现，按 Rec 会明确拒绝。

- [x] **R9 系统声音内录**（WASAPI loopback）✅ 2026-11-03
  `RecordSource`（Default / Capture / Loopback）+ `record_sources()` 枚举；
  loopback 走 render 端点的 `supported_output_configs()`，按端点原生采样率开流（无重采样）；
  Audio In 节点与 Inspector 的设备菜单分「Inputs / System playback (loopback)」两组，选择持久化到
  `UiState.{input_device, input_loopback}`；按 Rec 时若源为 loopback 明确提示
  "本工程回放也会进 take"。

## 5. 轨输入口与录音归属（R6，2026-11-01 用户裁定）

用户裁定（原话要点）：**patch 是显式基准，也是现实舞台设备连接的虚拟化表示**，所以录音前
由用户自己把设备接到想录的那条轨；`Timeline out` 保持纯输出，**输入口加在 `Timeline In`**
（它原本就有 tempo 自动化汇）；监听上 play / record 分离；输入轨在录音头之后必须为空。
以下 D21–D33 是据此推出的完整口径。

| # | 决策 | 结论 | 理由 |
| :-- | :-- | :-- | :-- |
| D21 | 轨输入口落点 | 加在 **`Timeline In`**（现有 tempo 口之外）：tempo 保持 0 号口（已有 tempo cable 不受影响），其后按 **MIDI → 曲线 → 音频** 排出每轨一个口（与 `Timeline` 出口同序）。曲线轨口先留在端口表里，R8 才接采集 | `Timeline`（出口）与 `Timeline In`（入口）本来就是一对：时间轴发出什么、什么进入时间轴。把录音口放进 `Timeline` 会破坏"它只是源"的既有约定（`patch.rs` 里 `Timeline` 的 `in_ports_screen` 直接返回空，注释写明 D14/T4） |
| D22 | 录音归属（取代 D16） | **归属 = 接在该轨输入口上的那条线**。按下 Rec 时枚举"输入口有 cable 的轨"，这就是 armed 集合；**没有任何输入接线 → 拒绝开录**并给出接线指引 | 用户裁定：默认工程不预接，必须先手动连线才能录。归属不再是 UI 状态，而是文档事实，可保存、可分享、可 undo |
| D23 | 单一源原则 | **一个轨输入口最多 1 条 cable**（第二条被 validate 拒绝）；**一个源可以扇出到任意多个轨输入口**（同一段演奏同时录进 Track 1 / Track 3，各写一份 take，同一个 undo 步） | 用户裁定。附带好处：汇点恒定单源，采集端不需要求和缓冲，也不需要"多源混音后如何归属"的规则 |
| D24 | 连线规则 | **类型匹配即可连**：MIDI cable → MIDI 轨输入口，audio cable → 音频轨输入口，automation cable → 曲线轨输入口。源不限于设备节点：乐器输出、混音器输出、MIDI 效果、LFO 都能接进对应轨 | 用户裁定（"类似烘焙的功能"）。端口语义 = **磁带机输入**：数据进时间轴，**不新增引擎图边**。对**非设备节点**（乐器 / 混音 / LFO）而言完全不改变信号流；对**设备节点**接入时按 D33 接管其出口——这是刻意的，为的是让听到的 = 录到的 |
| D25 | 采集点按源类型分级 | 四级 tap，共用一套"录音会话"（Rec 时下发 `{ 目标轨, 源, kind, 起始 tick }`，Stop 收回）：①设备 MIDI = 命令级（现成 tap）；②节点 MIDI（Arp / Portamento 输出）= 事件派发级；③音频 = 节点 post 缓冲级；④控制 = 块边界采样级 | 采集点必须跟着"数据产生的地方"走：命令级 tap 看不见引擎内部生成的音符，节点 post 缓冲才看得见效果器处理后的音频 |
| D26 | 分期矩阵（**允许连 ≠ 现在能录**） | Keyboard → MIDI 轨 ✅ **R6**；乐器/混音/Input → 音频轨 ✅ **R4**；Arp/Portamento → MIDI 轨 ❌ **R7**；LFO/curve → 曲线轨 ❌ **R8**。未实现的组合**连线允许**（模型一次做对），但按 Rec 时**明确拒绝**并说明"该类型的录音尚未实现" | 允许连是为了不返工模型；静默空录是最坏的失败方式，宁可当场报错 |
| D27 | 干区校验 | 按 Rec 的**第一步**校验目标轨的干区：冲突条件可压成一条 `clip.start_ticks + clip.span_ticks > record_start`（`span ≥ 1`，所以"跨越起点"与"起点之后"一并覆盖；恰好收在 `record_start` 放行）。冲突 → 拒绝并报位置（"Track 5 bar 9 already has a clip — delete or move it first"），**不动走带、不改 capture 状态** | 用户裁定（"录音头之后必须是空轨"）。精确成区间冲突而不是"整轨为空"，是为了保住最常见的"在已有素材之后续录"。重叠会双声、会让"一次 take = 一个 undo 步"失效 |
| D28 | 输入轨静音 | **不需要**单独的输入轨静音机制 | D27 保证干区之后没有旧素材，干区之前的内容早已播过（pre-roll 期间照常可听，正好用来跟拍）——用户要的"原本内容静音"由 D27 恒真地满足 |
| D29 | 其他轨监听 | 一个 `SetRecordMute { 位图 }`：默认"其他轨照常播放"，可切"只监听输入"（其余轨全静）。sequencer 侧按轨**丢 `On`、照发 `Off`** | 用户裁定"其他轨是否播放由选项决定"。只丢 `On` 是为了不在静音瞬间留下挂音 |
| D30 | 监听缺失警告 | 目标轨出口没接乐器（MIDI）/没接任何下游（音频）时警告"录得进去但听不见"。接管（D33）之后**该设备完全无声**——直连回路已休眠，没有兜底，所以这条警告是必读级，不是提示级 | 用户同意。这条是 D24 + D33 的必然产物：数据进时间轴与声音去哪是两件事 |
| D31 | 事件派发 | 设备的 cable 终点是轨输入口 → `EngineCommand::Note { track }`（监听顺带成立：轨出口 → 乐器）；终点是乐器 → 仍走 `NoteAtNode`；**该设备一旦有轨输入口 cable，`NoteAtNode` 一律不派发**（D33 接管）。新增 `EngineCommand::Ctrl { track, ctrl }` 让 CC / pitch bend 跟随同一路由（否则接了轨输入口就丢控制器） | 复用 R3 已有的 `MidiTarget::Track` 路径，引擎不新增图级通路 |
| D32 | 走带与采集解耦 | `● Rec` = 开始采集并走带；`Play`（录音中按下）= **停止采集、继续走带**（punch out）；`Stop` = 结束 take 并倒回 | 用户裁定"play 与 record 按钮分离"。punch out 保留走带是最常见的写法，也让"录一段然后接着听"不用两次操作 |
| D33 | **轨输入接管**（track takeover） | 设备节点只要有一条出线以某个轨输入口为终点，**该设备的出口就是那些轨**：直连节点的 cable 不再参与派发。这是**永久**语义，不是录音期临时静音——闲置 / 录音 / 回放三态一致。实现为派发时的**纯派生**（`(tracks, node_targets)` 由文档现算，不存 mute 标志，因此没有"恢复"这一步）；电脑键盘与 midi hub 快照共用同一函数；CC / bend 同规则。分层能力不损失：改用轨出口扇出（D23）。**可见性**：被接管的直连 cable 在 patch 里画成休眠样式 + tooltip（"Device is routed into Track 3; direct cables are dormant"），**不自动删除**。**录音中途删掉轨输入口 cable → 当帧结束 take 并提示**，不留半截 take。**音频不适用**（信号流，见 D24 / D30） | 替代"录音期临时静音"的原案（R2）：原案有四个问题——① 恢复点会漏（Stop / Pause / punch out / 自动停 / panic / 换工程 / 拔线），② 闲置期仍然双重发声（那才是弹得最多的时候），③ 对音频不成立（静音输入节点的其它 audio cable = 监听消失），④ 分层在录音时被掐掉而回放里没有。派生式接管把四个问题一起消掉：无状态即无恢复，三态一致即无模式切换，双重发声从根上消失 |

### 5.1 版本与迁移

- 文件格式 **v9 → v10**：新增 `PortRef::TrackIn { kind, track }`（`TrackKind::{Midi, Audio, Curve}`）。
- **无迁移函数**：v9 文档里不存在该变体，原样加载；新变体属"新增枚举分支"，不是字段语义变更。
- 旧构建读到 v10 文件按既有策略直接拒绝（`ModelError::NewerVersion`），不会"读到一半丢数据"。
- **默认工程不预接**：`default_graph_doc` 保持 `Keyboard → Instrument` 直连不变，新工程按 Rec 会得到
  "还没有轨道输入接线"的提示 —— 这是用户要的行为，不是缺陷。

## 6. 不做的事（本轮）

- **comping / take lanes / punch in-out**：需要 take 管理层，独立 spec。**覆盖式录音**
  （punch 进已有 clip 的区间并替换它）同属这一类，D27 直接拒绝重叠区间。
- **一个轨输入口接多个源**：D23 单源原则；要叠两路就先各自成轨，再用扇出 / 混音器表达。
- **count-in（负数起拍）**：pre-roll 已覆盖 90% 场景；负数 transport 会污染
  `position_ticks` 的 `f64 → u32` 边界与 loop 语义。
- **音频输入的采样率转换**：输入设备按引擎采样率打开，失败则报错不回退。
- **输入节点的多设备混音**：第一版只支持单一输入设备（一个 stream）。
- **节拍器音色自定义**：不做采样替换，合成表够用。

## 7. 系统声音内录（R9，2026-11-03）

需求原话：*"wasapi 是不是支持 loopback 录音，也就是目前 usb audio 播放的声音可以内录到
audio 轨上，现在好像还不支持"*。

**真机验证**（`Wasapi` host，默认输出 `扬声器 (USB AUDIO DAC)`，mix `F32 2 ch @ 48 kHz`）：
在 render 端点上开一条"输入流"，回采 1.2 s 的 440 Hz 测试音 → 58560 帧，
`ch0 = ch1 = 0.2000`，与源幅度一致。结论：**能力一直在 cpal 里，只是 UI 只枚举了输入设备。**

| # | 决策 | 理由 |
| :-- | :-- | :-- |
| D34 | 内录是**录音源的一种**，不是新节点、不是虚拟设备 | `RecordSource::{Default, Capture(name), Loopback(name)}` 只描述"这条流从哪来"。图里的 `Input` 节点、monitor/take 双 ring、dry-region 校验、clip 落盘与 undo 全部分叉不变 |
| D35 | 用 cpal 自带的 loopback，**不引 `wasapi` crate** | cpal 0.18 的 WASAPI 后端在 `data_flow == eRender` 时自己加 `AUDCLNT_STREAMFLAGS_LOOPBACK`（`host/wasapi/device.rs`）。零新依赖，跨平台分支也不用写 |
| D36 | loopback 的配置从 `supported_output_configs()` 取，**必须端点原生采样率** | cpal 对输出设备的 `supported_input_configs()` 返回空、`default_input_config()` 直接 `UnsupportedOperation`；且共享模式 loopback 不走输出路径的 `AUTOCONVERTPCM`，引擎采样率不被支持时 `Initialize` 会失败。失败文案列出端点实际支持的格式，并提示在 Windows 声音设置里改端点采样率 |
| D37 | 内录会把**本工程自己的回放**录进 take | 引擎输出与该端点同一个设备时这是物理必然。按 Rec 的启动消息里明说，不自动静音、不临时改路由——用户可能就是想内录整机混音（含本工程）。想只录外部声音就把引擎输出换到另一个设备，或先停走带 |
| D38 | 同一时刻只有一条录音流 | 内录 + 麦克风同时录需要第二条 stream 与混音归属规则，属 R4 既有"单一输入设备"限制的延伸，本轮不做 |

**连接方式**：Audio In 节点的设备 chip（或 Inspector 的录音源菜单）→
`System playback (loopback)` 分组（`↩ 设备名`），选中即换流并重建图（`Input` 节点重新 latch 新 ring）。选择持久化到
`UiState.{input_device, input_loopback}`；设备拔掉时回落系统默认并在日志/状态栏说明。
