# LFO 控制源节点 Spec

> 状态：**已实施**（2026-10-31）。M1–M4 全部落地：model 协议、`LfoProcessor`、UI 选择器 / 节点 / Inspector 预览、单测 / RT 审计 / graph 串联测试。
> 关联：`midi-nodes.md`（控制源 / 控制边协议）、`curve-map.md`（归一化输出与目标映射）、`control-tracks.md`（曲线 / 自动化）。

## 1. 背景与目标

现有控制源只有两类：`Portamento`（MIDI 驱动滑音）与 `MidiEnvelope`（note-on 触发包络），以及一个 `Curve Map` 映射节点。缺少**自由运行、周期可调**的调制源，因此无法做 tremolo、自动滤波扫频、慢速 pan 摆动、随机阶梯调制等常见用法。

本 spec 新增 **LFO 控制源节点**：

- 1 个 control out，无音频 / MIDI 端口；
- 输出归一化 `[-1, 1]`（与 `curve-map.md` D1 一致），可直连参数 / CC / pitch bend，也可先过 `Curve Map`；
- 参数：Waveform、Rate（Hz 或音符分频）、Depth、S&H；
- 自由运行，transport 停止时相位冻结，停止期间**不再投递**控制值。

**非目标**

- 不做相对 / 加法调制（见 §9 Q1）。
- 不做 MIDI key-sync（无 MIDI 输入端口，见 D2）。
- 不做多相 / 多路 LFO（每个节点一路）。

## 2. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| D1 | **控制源节点** | `NodeDoc::Lfo { node_id, track, params }`；`has_control_out = true`，`accepts_control = false`，`has_audio_ports = false`，无 MIDI 端口。 |
| D2 | **自由运行，无 MIDI 输入** | 不加 key-sync。相位只在 transport 从停止位置 0 启动时归零；暂停 / 继续不归零。 |
| D3 | **双时间基准** | `sync = false`：Rate 是固定 Hz（0.01–40），按节点内累计的 transport 活跃秒数推进；`sync = true`：Rate 是音符分频（1/1…1/32），相位由 `ctx.song_ticks` 派生，自动跟随 tempo 曲线、seek 与 loop。 |
| D4 | **停止即静默** | `ctx.transport_active == false` 时冻结相位并令 `out.control = None`（不投递），避免停止的 LFO 持续覆盖用户手动调参。 |
| D5 | **归一化输出，不声明轴** | 输出 `wave × depth` 并 clamp 到 `[-1,1]`；`Processor::map_spec()` 返回 `None`，目标使用自己的默认轴（D2 of `curve-map.md`）。Depth 因此是「调制幅度」而不是「目标量程」。 |
| D6 | **S&H 可复现** | 每个周期边界从节点内 XorShift32 取新值；种子 = `node_id`，离线渲染与实时得到同一序列，不做系统随机。 |
| D7 | **块级更新** | 与现有控制源一致，每块输出一个值（`process_events`）。目标侧平滑由接收方负责；Rate 上限 40 Hz 低于常见块率的一半。 |
| D8 | **无版本迁移** | 新增 `NodeDoc` 变体是**附加**的；旧工程没有该节点。`PROJECT_VERSION` 保持 9，仅把加载期 `sanitized()` 挂进 `GraphDoc::migrate`。 |

## 3. 参数

| 参数 | 范围 | 默认 | 说明 |
| :--- | :--- | :--- | :--- |
| `waveform` | Sine / Triangle / Saw / Square / SampleHold | Sine | UI 下拉；节点体显示短名。 |
| `rate_hz` | 0.01–40 | 1.0 | `sync = false` 时生效。 |
| `sync` | bool | false | 切到音符分频。 |
| `division` | 0–5 | 2（1/4） | `sync = true` 时生效；表见下。 |
| `depth` | 0–1 | 0.5 | 输出缩放；0 = 恒为轴中点。 |

分频表（一拍 = `TICKS_PER_QUARTER` = 960 ticks）：

| index | 0 | 1 | 2 | 3 | 4 | 5 |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| 标签 | 1/1 | 1/2 | 1/4 | 1/8 | 1/16 | 1/32 |
| 周期 ticks | 3840 | 1920 | 960 | 480 | 240 | 120 |

`depth = 0.5`、目标为默认参数轴时，调制覆盖轴的中心 ±25%；`depth = 1` 覆盖全轴（`-1..1`）。

## 4. 波形定义

`φ = phase ∈ [0,1)`，输出先乘 `depth`：

| Waveform | 公式 | φ=0 | φ=0.25 | φ=0.5 | φ=0.75 |
| :--- | :--- | ---: | ---: | ---: | ---: |
| Sine | `sin(2πφ)` | 0 | +1 | 0 | −1 |
| Triangle | `φ<0.5: 4φ−1`；否则 `3−4φ` | −1 | 0 | +1 | 0 |
| Saw | `2φ−1` | −1 | −0.5 | 0 | +0.5 |
| Square | `φ<0.5: +1`；否则 `−1` | +1 | +1 | −1 | −1 |
| SampleHold | 周期内保持 `hold`，边界取新随机值 | 新随机 | 保持 | 保持 | 保持 |

> Triangle 与 Saw 在 φ=0 从 −1 起跳，Sine 从 0 起，Square 从 +1 起——与各自「相位零点」的直觉一致；UI 预览按此绘制。

## 5. 时间语义

```
每块 process_events(ctx, out):
  if !ctx.transport_active:
      was_active = false
      return                      // control = None（D4）
  if !was_active && ctx.song_ticks <= 0.0:
      phase = 0; seconds = 0; hold = next_random()   // transport 从 0 启动
  was_active = true

  if sync:
      prev = phase
      phase = fract(ctx.song_ticks / division_ticks)
  else:
      prev = phase
      seconds += ctx.frames / ctx.sample_rate
      phase = fract(seconds × rate_hz)
  if phase < prev:                 // 跨越周期边界
      hold = next_random()
  out.control = Some(waveform(phase) × depth)
```

- **Stop**（位置归零）→ 下一次 Play 的第一个块 `song_ticks <= 0`，相位归零。
- **Pause → Play**（位置非零）→ Hz 模式的 `seconds` 继续累计；sync 模式由位置派生，自然接着走。
- **Seek / Loop**：sync 模式跟随位置；Hz 模式不受影响（自由运行）。
- **停止期间**相位冻结、且不投递；继续播放时恢复投递。
- 离线渲染（Export Audio）走同一 `ProcessContext`，因此与实时一致。

## 6. 数据与代码落点

```rust
// model
pub enum LfoWaveform { Sine, Triangle, Saw, Square, SampleHold }
pub struct LfoParams { pub waveform, pub rate_hz, pub sync, pub division, pub depth }
NodeDoc::Lfo { node_id: u64, track: usize, params: LfoParams }
pub const LFO_DIVISION_COUNT: usize = 6;
pub const LFO_DEFAULT_DIVISION: u8 = 2;

// engine
pub struct LfoProcessor { waveform: u8, rate_hz, depth, sync, division, phase, seconds, hold, rng, was_active }
pub const LFO_DIVISION_TICKS: [f64; 6] = [3840.0, 1920.0, 960.0, 480.0, 240.0, 120.0];
ParamId::{LfoRate, LfoWaveform, LfoDepth, LfoSync, LfoDivision}
```

- `model`：`LfoWaveform::ALL/label/index`、`LfoParams::{default,sanitized}`、`NodeDoc` 端口谓词、`GraphDoc::validate` 的控制源错误文案、`migrate` 的 sanitize。
- `engine`：`realtime/lfo.rs` + `mod.rs` 导出；`ParamId` 扩展；`ProcessContext` 已足够（不新增字段）。
- `ui`：
  - `NodeSource::Lfo` + `builtin_midi_fx()` 条目（`MIDI FX…` 选择器，归控制源族）；
  - `node_out_port_kind` → `Automation`；输入端口数 = 0；
  - `node_color` / `label_of` / `opens_inspector` / 默认摆放列；
  - 两处 `build_node_processor`（实时装配 + 离线装配）；
  - Inspector 参数：ComboBox（Waveform）、Checkbox（Sync）、DragValue（Rate / Division）、Slider（Depth）+ 单周期波形预览。

## 7. 分阶段

| 阶段 | 内容 | 验收 |
| :--- | :--- | :--- |
| **M1 协议 + 模型** ✅ | `LfoWaveform` / `LfoParams` / `NodeDoc::Lfo` / 端口谓词 / sanitize / validate | `cargo test -p glitcherz-model`；旧工程可加载，LFO 可保存往返 |
| **M2 引擎** ✅ | `LfoProcessor`、5 个 `ParamId`、波形数学、双时间基准、S&H、停止静默 | 单元测试覆盖波形 / depth / sync / 停止 / 恢复；RT 审计零分配 |
| **M3 UI** ✅ | 选择器条目、节点颜色/标签/端口、Inspector、波形预览、两处装配 | 手动：添加 LFO → 连到 cutoff / pan / CC，播放听到调制；停止后可手动调参；同步模式改 BPM 频率跟随 |
| **M4 回归** ✅ | 选择器列表测试、节点端口测试、离线引擎串联测试 | `cargo fmt && cargo clippy --workspace -- -D warnings && cargo test --workspace` |

**实施备注（2026-10-31）**

- 分频标签在 model（`LFO_DIVISION_LABELS`），tick 表在 engine（`LFO_DIVISION_TICKS`），与 Arpeggiator 的「model 只放顺序常量、engine 放时间表」分工一致。
- 节点标签显示 `LFO · Sine 1.00 Hz · 50%`（sync 时改显 `1/4`）；Inspector 用两个周期的波形预览，S&H 预览交替两个固定演示值，真实随机序列由节点 ID 种子决定。
- 停止时 `out.control = None`（不是输出 0）：目标保持最后写入值，用户可以马上手动调参而不被下一块覆盖。

## 8. 测试计划

- 波形：五个波形在 `φ = 0 / 0.25 / 0.5 / 0.75` 的取值；全相位有限值。
- Depth：`depth = 0` 恒 0；`depth = 0.5` 输出幅度减半；clamp 不越界。
- 同步：1/4 周期 = 960 ticks；`song_ticks` 推进半周期后输出反相；loop / seek 位置派生一致。
- 自由运行：暂停→恢复继续累计；Stop→Play 归零；停止块 `control = None`。
- S&H：同周期内保持不变，跨周期变化；同 `node_id` 两实例序列一致。
- RT：`set_param` / `process_events` 零堆分配。
- UI：`MIDI FX…` 列表出现 LFO；LFO 节点无输入端口、输出端口为 `Automation`。

## 9. 开放问题

- **Q1 相对调制**：当前控制协议是**绝对覆盖**（归一化值映射到目标轴）。LFO 接到 cutoff 时会把参数拉到轴范围内的振荡，而不是围绕用户当前设定值摆动。真正的「围绕当前值」需要引擎 `deliver_dest` 支持加法模式并读取目标当前值（类似 `MidiEnvelope` 的绝对语义同样存在此限制）。是否要做，需单独立项。
- **Q2 Phase / Offset**：首版未做 Phase 偏移与直流 Offset（用户选择为基础 + 随机 + 同步档）。后续加字段是向后兼容的，但要同步扩 `ParamId` 与 Inspector。
- **Q3 波形扩展**：Reverse Saw、Exponential、Custom 曲线（可复用 `AutoPoint`）尚未纳入。
- **Q4 UI 相位指示**：节点体是否显示播放中的相位游标（需要 UI 读引擎块状态，目前无回传通道）。