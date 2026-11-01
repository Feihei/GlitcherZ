# Spec: 节拍器与录音（Metronome & Recording）

> 状态：实施中（2026-11-01）。R1 节拍器、R2 录音骨架、R3 硬件 MIDI 输入已交付，R4–R5 待实施。
> 关联：`plan.md`「S2 补最短的板」、`review-2026-11-01.md`、
> `spec/audio-track.md`（音频轨播放与 trim）、`spec/tempo-meter.md`（tick 时钟 / 拍号）。
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
| D13 | 录音对齐 | 先做**手动 offset**（采样数，UI 可调，默认 0）；PDC（`latency_samples` 接进图编译期）落地后再自动算 | 自动对齐依赖 PDC 基础设施，S2-3 尚未实施；手动 offset 能立刻可用 |
| D14 | pre-roll | 录音前 `pre_roll_bars`（0–2，默认 1）内**正常播放素材与 click，但不采集**；UI 丢弃 `ticks < record_start_ticks` 的事件 | 这是 pre-roll 而非 count-in（不从负数起拍），实现只需一次比较，引擎零改动 |
| D15 | 一个 take = 一个 undo 步 | 录音停止时一次性写入 clip 的 notes，走一次 `begin_edit` / `commit_edit` | 逐音符入栈会让 undo 栈被一次录音灌满 200 步 |
| D16 | armed track 的确定 | **当前选中的 MIDI 轨**，不再单独做 arming UI；录制期间若键盘节点没有任何连线，按键直接发给 armed track（否则"没接线就录不了"） | 第一版把 arming 做进选择，零额外 UI；有连线时行为完全不变（仍走 vk 路由），只是把"没连线也能录"补上 |
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
UI 每帧：drain → 丢弃 ticks < record_start_ticks（pre-roll）→ on/off 配对成 Note；
         走带从后往前跳 = 循环回卷 → 清空上一 pass（D12）
停止：一次性写入 armed track 的新 clip（开始位置吸附到小节，span 向上取整到整小节）
```

### 3.3 音频录音（R4）

```
cpal 输入流回调（输入设备线程）
  ├─→ ring A（监听）→ InputProcessor（图内节点）→ 用户接的任意下游
  └─→ ring B（录音）→ UI 每帧 drain → 内存 Vec<f32>
停止 → 写 <stem>.media/rec-<时间戳>-<track>.wav → 建 AudioClip（相对路径）
     → SetAudioClips（既有结构队列路径，RT 侧零改动）
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
- [ ] **R4 音频录音**
  `NodeDoc::Input{node_id, track, channel, gain}` + `InputProcessor`；
  `AudioInput`（cpal 输入流 + 双 ring + `Arc<InputStats>` 原子量）；
  `.media/` + 相对路径解析（`Project::base_dir` / `resolve_path`）；
  录音落盘与 clip 创建；手动延迟 offset。
- [ ] **R5 收尾**
  录音后自动选中新 clip、take 命名、输入电平表、状态栏溢出告警；
  PDC 落地后自动对齐；多输入通道（立体声对 / 多设备）。

## 5. 不做的事（本轮）

- **comping / take lanes / punch in-out**：需要 take 管理层，独立 spec。
- **count-in（负数起拍）**：pre-roll 已覆盖 90% 场景；负数 transport 会污染
  `position_ticks` 的 `f64 → u32` 边界与 loop 语义。
- **音频输入的采样率转换**：输入设备按引擎采样率打开，失败则报错不回退。
- **输入节点的多设备混音**：第一版只支持单一输入设备（一个 stream）。
- **节拍器音色自定义**：不做采样替换，合成表够用。
