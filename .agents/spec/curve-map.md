# 曲线归一化与 Curve Map 节点 Spec

> 状态：**已实施**（2026-10-30）。D1–D11 全部落地：C1 协议+迁移、C2 Curve Map 节点、C3 UI（C4 pitch 输入口同日先期交付）。`PROJECT_VERSION` 8 → 9。
> 关联：`control-tracks.md`（曲线编辑 / `AutoDest`）、`midi-nodes.md`（D6 控制边、D13 滑音）、`effects.md`（参数轴）、`sf2-loader.md`。

## 1. 背景与问题

当前曲线的采样值直接存在**目标域**里：

- 参数目标：`AutoParam::Node` 对应的参数轴值（cutoff Hz、gain、ADSR 秒……）；
- CC 目标：0..127；
- bend 目标：半音（`range_semitones` 决定满量程）；
- Tempo：BPM。

这带来三个问题：

1. **换目标就要重画曲线**：同一条曲线从 cutoff 改到 pan，旧值没有意义；
2. **控制源与目标耦合**：`Portamento` / `MidiEnvelope` 输出半音，必须靠 `AutoDest::PitchBend{range}` 猜目标量纲；
3. **pitch 输入口不一致**：内置合成器用 `PitchOffset`（±24 st），SF2 用 14-bit bend（range 由 preset 决定），插件用自身 bend range；同一条“音高曲线”在三类目标上语义不同。

本 spec 把曲线统一成 **归一化信号**，把「范围 + 映射方式」交给目标端口（或显式的 Curve Map 节点）。

## 2. 目标与非目标

**目标**

- 曲线 / 控制源统一输出 `[-1, 1]`（0 = 轴中心）。
- 目标端口声明 `{ min, max, mode }`，由编译期解析；引擎 `deliver_dest` 一次换算。
- 新增 **Curve Map** 控制节点：显式选择范围与映射方式（Linear / Log / Exp），可插在曲线与控制源之间。
- 内置 / 采样 / SF2 / 插件的 pitch 输入口语义统一，SF2 增加显式 bend range（1–24 st）。

**非目标**

- 不改音频图与曲线编辑手势。
- 不改 CC / 14-bit bend 的 wire 格式。
- 不做 MIDI 2.0 / MPE。

## 3. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| D1 | **归一化域 `[-1, 1]`** | 曲线点与 `NodeEvents.control` 都存归一化值；0 是轴中心。参数轴的非线性只影响显示，不影响存储。 |
| D2 | **目标端口携带 `MapSpec`** | `MapSpec { min: f32, max: f32, mode: MapMode::{Linear, Log, Exp} }`。参数目标取 `node_param_axis`；CC 固定 `0..127` + Linear；bend 取节点的 bend range；`PitchOffset` 固定 `-24..24` + Linear；Tempo 取 `20..1000` + Log。 |
| D3 | **映射公式统一以 `t = (u+1)/2` 为输入** | Linear：`min + t·(max-min)`；Log：`min·(max/min)^t`（要求 `min>0`）；Exp：`min + t²·(max-min)`。`max<min` 或 Log 的下界非正时回落 Linear。 |
| D4 | **映射在编译期解析，不进入实时回调的参数轴查询** | `CableDoc` / `AutoRoute` 编译成引擎侧 `Dest { slot, map }`；`deliver_dest` 只做算术。目标轴变化（如 SF2 bend range）触发一次结构更新。 |
| D5 | **Curve Map 是控制节点** | 1 Control in / 1 Control out、无音频/MIDI 端口；参数 `min`、`max`、`mode`；透传输入值并按自己的 `MapSpec` 重映射。它同时是 UI 上的“映射说明卡”（节点体显示范围与模式）。 |
| D6 | **Curve Map 覆盖目标默认 map** | 目标前存在 Curve Map 时，以 map 节点的 `MapSpec` 为准；否则用目标默认 map（D2）。map 节点本身不做单位换算（输入输出都是归一化）。 |
| D7 | **全局迁移 v8 → v9** | 每条自动化 / 控制线按目标旧轴把每个曲线点逆映射到 `[-1,1]`。Tempo 用 Log 逆映射。无目标曲线保持数值并 clamp 到 `[-1,1]`。节点控制线（Portamento / MidiEnvelope）直接按其半音范围逆映射。 |
| D8 | **pitch 输入口三类** | 内置 / 采样：`Pitch` → `ParamId::PitchOffset`（±24 st）；SF2：`Pitch Bend (±bend_range st)`，`bend_range` 是 `VoiceSpec::SoundFont` 的 1..24 参数，引擎通过 RPN 0 写入 rustysynth；CLAP/VST3：`Pitch Bend (±2 st)`（插件自身 range）。 |
| D9 | **SF2 bend range 走标准 RPN** | `ParamId::Sf2BendRange` 在 `set_param` 中发 `CC101=0, CC100=0, CC6=range, CC38=0`；rustysynth 的 `Channel::get_pitch_bend_range` 即生效，不改 vendor。 |
| D10 | **实时安全** | 映射是每块每目标一次标量运算；`MapSpec` 是 `Copy`；无表查询、无分配、无锁。 |
| D11 | **显示轴不迁移** | 曲线编辑器的 y 轴仍显示目标域值（由 `node_param_axis` + `MapSpec` 正向换算）；用户看到的刻度不变，只是存储层归一化。 |

## 4. 数据结构

```rust
// model
pub enum MapMode { Linear, Log, Exp }
pub struct MapSpec { pub min: f32, pub max: f32, pub mode: MapMode }

// 曲线点存储（原样，只是值域改为 [-1,1]）
pub struct AutoPoint { pub tick: u64, pub value: f32, pub interp: CurveInterp }

// Curve Map 节点
NodeDoc::CurveMap { node_id: u64, track: usize, params: CurveMapParams }
pub struct CurveMapParams { pub min: f32, pub max: f32, pub mode: MapMode }

// engine
pub struct Dest { pub slot: u16, pub map: MapSpec }   // 取代 AutoDest 的裸 dest
fn map_value(map: MapSpec, norm: f32) -> f32          // RT-safe，clamp 输入
```

`AutoDest::{Param, Cc, PitchBend}` 保留为**目标种类**；`MapSpec` 作为它的伴随字段进入 `AutoRoute` / `EventCable`。`deliver_dest` 先 `map_value` 再分派。

## 5. UI

- `curve.rs`：`axis` / `to_norm` / `from_norm` 改为「归一化存储 + MapSpec 显示」；y 轴刻度按目标轴正向换算，曲线形状在换目标时保持不变。
- `patch.rs`：
  - 目标菜单里每一项显示实际范围（如 `Cutoff 20–18k (Log)`、`Pitch ±24 st`、`Pitch Bend ±12 st`）。
  - Add 菜单新增 **Curve Map**（MIDI FX / control 族）。
  - Control 线可落在 Curve Map 的输入；Curve Map 的输出再落到目标。
  - Curve Map 节点体显示 `min..max · Linear/Log/Exp`，双击进 Inspector。
- Inspector：`min` / `max` DragValue + `mode` ComboBox；SF2 面板新增 `Bend Range`（1–24 st）。
- 内置 / 采样乐器的目标菜单不再出现 `Pitch Bend`；插件的仍为 `Pitch Bend ±2 st`。

## 6. 分阶段

| 阶段 | 内容 | 验收 |
| :--- | :--- | :--- |
| **C1 协议 + 迁移** ✅ | `MapSpec`、`AutoRoute`/`EventCable` 携带 map、`deliver_dest` 换算、v8→v9 迁移 | 旧工程加载后曲线形状不变（轴正向换算一致）；CC/bend/Tempo 逐项 |
| **C2 Curve Map 节点** ✅ | model + engine 节点（Control in/out，透传重映射）+ 校验 | 节点串联、范围/模式生效、Control 入 ≤1 |
| **C3 UI** ✅ | 曲线轴换算、目标菜单范围显示、Add 菜单、节点体、Inspector | 手动：曲线换目标不变形；map 节点改范围即时生效 |
| **C4 pitch 输入口** ✅ | 内置 `Pitch`、SF2 `bend_range` + RPN、插件 `±2` | SF2 设 12 st 后远距离滑音可到位；内置 ±24 内不受限 |

**实施备注（C2/C3 落地时的两处细化）**

- 进入 Curve Map 的那一跳**强制恒等**（engine `is_curve_map_in` / 时间轴 `auto_deliver` 同规则）：上游控制源（包络/滑音）声明的轴只对「越过 map 之后」的目标生效，否则同一个刻度会被算两次。
- `validate` 对 map 输入的限制**不分来源**（时间轴曲线与节点控制源共用一个直通寄存器），而普通节点的 `control in ≤ 1` 仍只约束节点源——这是两者唯一的差别。

## 7. 被否决的备选

| 方案 | 否决理由 |
| :--- | :--- |
| 只归一化 MIDI 控制源，不动时间轴曲线 | 两套协议长期并存，换目标仍要重画曲线；用户明确要全局改。 |
| 每个 Processor 自己解释归一化值（`set_param_normalized`） | 要改全部内置处理器、CLAP/VST3 宿主与插件参数桥；且参数轴本来就在文档侧（`node_param_axis`）。 |
| 曲线存储保持目标域，只加 Curve Map 节点 | 解决不了“换目标重画”，也解决不了控制源与目标耦合。 |
| 用 MIDI 标准 bend range 固定 ±2 | 与 Portamento 默认 ±24 冲突，远距离滑音直接丢失。 |
| 归一化后所有轴都用 Linear | cutoff / Tempo 的低端分辨率会差一个量级；且既有 Log 轴经验被浪费。 |

## 8. 开放问题

- **多目标曲线**：一条曲线连多个目标且轴不同时，归一化后如何显示？当前 `auto_routes` 是单目标，节点控制线也是单目标；若未来允许扇出，UI 需要一个“主目标”概念。
- **Exp 公式**：`min + t²(max-min)` 会在高端压缩；是否需要可调曲率参数。
- **迁移警告**：无目标曲线被 clamp 后应提示一次，还是静默保留。
- **插件参数的轴**：CLAP raw / VST3 0..1 的轴表已有；归一化后插件参数是否需要独立的映射方式（部分插件参数本身是对数刻度）。