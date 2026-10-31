# 内置合成器：多波形 + 滤波器接线

> 状态：M1–M4 已实施（2026-10-31）
> 相关：`.agents/spec/effects.md`（`Unit::Enum` / `Smoothed`）、`.agents/spec/midi-nodes.md`（内置 ADSR 与 MidiEnvelope 的区别）

## 1. 背景

内置合成器最初是路线图第 1 步的验证用正弦源，之后只叠加了 ADSR：

- `SynthProcessor = OscProcessor + AdsrProcessor`，波形固定 Sine；
- `ParamId::FilterCutoff / FilterResonance / FilterMode`、`SynthParams` 三个字段、
  Inspector 三个控件、`SvfProcessor` 全部存在——但 **SVF 从未进入处理链**，
  三个参数落在 `SynthProcessor::set_param` 的 `_ => false` 上，滑块只改文档不响。
- 离线导出只 `SynthProcessor::new(sr)`，连 ADSR 都没下发（导出与实时不一致）。

## 2. 目标

1. 内置振荡器支持 **Sine / Triangle / Saw / Square**，默认 Sine（老工程听感不变）。
2. 把 `SvfProcessor` 真正接进合成器，让 Inspector 的 Cutoff / Resonance / Filter Mode 生效。
3. cutoff / resonance 加 10 ms 一阶平滑，拖动不产生 zipper 噪声。
4. 离线导出与实时一致：`SynthParams` 全字段在导出引擎里同样下发。

## 3. 决策

| ID | 决策 |
| :--- | :--- |
| D1 | 波形用**索引**跨层传递（`SynthWaveform` → `index()` → `ParamId::OscWaveform`），与 LFO / `Unit::Enum` 的既有约定一致；模型侧枚举 `SynthWaveform` 只负责词汇与 UI 标签。 |
| D2 | 锯齿、方波用 **PolyBLEP** 抗锯齿；三角波用解析式（谐波按 1/n² 衰减，混叠可忽略）。Sine 保持 `sin(2πφ)`，相位定义不变。 |
| D3 | 合成器链顺序为 **osc → SVF → amp envelope**（VA 的 VCF → VCA），滤波器状态随声部生命周期存在。 |
| D4 | `SvfProcessor` 内部做 10 ms 一阶平滑：cutoff 在 log 域（等比移动）、resonance 在线性域；构造后首次 `process` 前 `set_param` 直接 snap（沿用 `Smoothed` D12 的起步语义）。 |
| D5 | `SynthParams` 新增 `waveform: SynthWaveform`，`serde(default)`，默认 Sine —— 不 bump `PROJECT_VERSION`（与 E4 的 `DelayParams` 扩展同例）。 |
| D6 | `ParamId::SineFrequency / SineGain` 更名 `OscFrequency / OscGain`，DSP 类型 `SineProcessor` → `OscProcessor`（文件 `sine.rs` → `osc.rs`）。名字必须与"它现在产生四种波形"一致。 |
| D7 | 波形是离散参数，**不进自动化**（`NodeParam` 不加变体）；cutoff 沿用既有 `AutoParam::FilterCutoff`，接线后曲线/LFO 自动对其生效。 |

## 4. 处理链

```
MIDI note ──► OscProcessor (waveform, pitch offset, per-sample glide)
           ──► SvfProcessor (LP/HP/BP, cutoff+resonance smoothed)
           ──► AdsrProcessor (linear ADSR, velocity peak)
```

- Inspector：`Waveform`（横向 4 个选项）、`Cutoff Freq`、`Resonance`、`Filter Mode`、`Volume Envelope`。
- patch 节点体内联行仍为 `cut / res`（波形切换只在 Inspector，避免节点体过长）。

## 5. 默认值

| 参数 | 默认 | 说明 |
| :--- | :--- | :--- |
| waveform | Sine | 老工程无该字段时保持原音色 |
| cutoff_hz | 12 kHz | 对基频与低次谐波几乎无衰减，Sine 听感不变 |
| resonance | 0.2 | 轻微峰值 |
| filter_mode | LowPass | 0 |

## 6. 已知边界

- 运行中切换波形不做 crossfade，相位连续但瞬时值可能跳变；ADSR 起音段（默认 10 ms）通常掩盖它。
- `FilterMode` 仍是离散参数，不可自动化。
- 频率极高（接近 Nyquist）时 PolyBLEP 的 dt 接近 0.5，修正项退化，但不会产生 NaN/Inf。

## 7. 验收

- 引擎：四种波形各自过零率 ≈ 基频；满幅输入无 NaN/Inf；SVF 接线后 cutoff 200 Hz 低通可测衰减；cutoff 阶跃呈 ramp 而非整块跳变。
- 模型：旧文档（无 `waveform` 字段）反序列化为 Sine。
- UI：Inspector 切换波形/滤波模式写入文档并实时下发；导出路径下发全部 `SynthParams`。
## 8. 复音（D8–D12）

内置合成器从单声部升级为**固定 16 声部**，voice 池完全在 `SynthProcessor` 内部，
图、节点协议、文档格式与序列化都不改动。

| ID | 决策 |
| :--- | :--- |
| D8 | `MAX_VOICES = 16`，与 `MAX_PENDING_OFFS_PER_TRACK`（16）对齐：音序器每轨最多同时保留 16 个待释放长音，更大的池从编排路径永远触达不到。虚拟键盘同样受此上限约束。 |
| D9 | 每个 voice 是完整链路 `OscProcessor → SvfProcessor → AdsrProcessor`，各自持有相位与滤波器状态；参数写入**广播**到所有 voice，`Attack/Decay/Sustain/Release/PitchOffset/Cutoff/Resonance/FilterMode/Waveform` 因此对整组声部一致。 |
| D10 | 分配顺序：① 第一个 `env.is_idle()` 的槽；② 无空闲槽时偷**最老**（allocation stamp 最小）的 voice，并从其当前包络电平重新 Attack（`AdsrProcessor::trigger` 的既有语义），避免硬切。Release 尾音期间槽位仍被占用。 |
| D11 | 同音先 release：`On{n}` 到达时先把所有 `note == n` 的 voice 送 Off，再分配新槽。这样重复触发是同音淡出 + 新音起音，不会让两条同音叠加成 +6 dB。 |
| D12 | `Off{n}` 只释放 `note == n` 的 voice；**`Off{0}` 保持全释放哨兵**（`RtGraph::note_off_all` 依赖它）。代价：真实弹到 MIDI note 0 时它的 off 会释放全部声部——键盘最低音极少使用，接受。 |

补充约定：

- `ParamId::OscFrequency` 是单声部时代的"直接改频率"直控口，复音下**只作用于最新触发的 voice**（不动正在响的和弦），保留旧行为。
- Idle 的 voice 在 `process` 里整段跳过（无音时 CPU 归零）；voice 在**预分配的单声道 scratch** 里渲染后累加到输出，全程零分配。
- 没有 limiter / 自动增益：叠加超过 1.0 由 master 推子与 velocity 负责（与采样、插件声源一致）。

验收：

- 两个音符分别独奏的输出之和 ≈ 同时演奏的输出（可加性）；
- `Off` 只让对应音消失，其余声部继续；
- 同音重触发后旧 voice 进入 release、结束后 `active_voices()` 回到 1；
- 连续 17 个 `On` 时 `active_voices() ≤ 16`，输出有限；
- `Off{0}` 释放全部声部。