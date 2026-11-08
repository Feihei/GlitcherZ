# Z2：Vocoder Spec

> 状态：定稿（2026-11-08）。上游：`plan.md` Z / B 系列（Q3 决议：固定 16 带 + 外部 carrier）、`spec/dual-audio-input.md`（Z0 已交付）、`spec/sidechain-compressor.md`（Z1 已交付）、`spec/effects.md`（E 系列框架）。
> 范围：只做**一个双口效果器**：`Main` = carrier，`Mod` = modulator，16 带分析/合成 + 清浊旁路。不做 formant shift / freeze / 自定义带数 / 内置载波。

## 1. 背景

Z0 交出两个语义独立的音频入口，Z1 验证了「Mod 只做控制信号」。Vocoder 是 Z 系列的第二个真实消费者，也是第一个要求 **Mod 路逐样本波形** 的效果器：侧链只需要一个峰值，vocoder 要的是完整频谱包络。

它必须自己在两路上各开一个 16 带滤波器组：

```
modulator ──► 16 × BPF ──► |x| ──► attack/release ──► 逐带包络 env[i]
carrier   ──► 16 × BPF ──► out[i] = bp_carrier[i] · env[i] ──► Σ + 清浊旁路 ──► mix
```

可复用的既有资产：

| 资产 | 位置 | 用途 |
| :--- | :--- | :--- |
| Simper SVF（TPT） | `realtime/eq.rs` 的 `svf_coeffs` / `tpt_g` | 每带的双二阶骨架，`bp = k·v1`、`hp = x − k·v1 − v2` |
| `Processor::process_with_aux` | `realtime/processor.rs`（Z0-D3） | 两路逐样本波形，自带 PDC 对齐 |
| `EffectParamBank` | `realtime/effect/bank.rs` | 8 个槽位的范围 / 曲线 / 平滑 / 速率 |
| `BuiltinEffectKind::audio_inputs()` | engine 与 model 各一份 | 节点自动装成 `NodeKind::Dual`，画布自动两口，**零额外接线代码** |

## 2. 目标 / 非目标

**目标**：新增内置效果器 `Vocoder`：固定 16 带对数带组、双路带通、逐带 attack/release 包络、带组之外的噪声激励旁路（清浊）、dry/wet 与输出增益；全部状态构造期预分配，`process_with_aux` 零分配；`PROJECT_VERSION` 不动；单口节点的逐样本行为与删除语义零变化。

**非目标**：formant shift / 频带搬移、freeze / hold、可变带数、内置载波振荡器、逐带编辑器、立体声宽度 / 逐带声像、音高检测式的清浊分类器、把 modulator 当 Main 的自动判断、CLAP/VST3 插件效果的第二个口。

## 3. 决策

| ID | 决策 |
| :--- | :--- |
| Z2-D1 | **固定 16 带，对数均匀**：`f_k = low · (high/low)^(k/15)`，`k = 0..15`。带数不可调（Q3 定稿），`low` / `high` 只移动带中心。默认 `low = 120 Hz`、`high = 7 kHz`，相邻带比值 1.311（≈ 4.5 半音）。 |
| Z2-D2 | **`Main` = carrier，`Mod` = modulator**（与 Z1 的 Main=program / Mod=key 一致）。分析链**只读 aux**，合成链**只读 Main**。Mod 口未接线 = 包络恒 0 → 输出静音（Z-D12 的物理直觉）；**绝不回落成「用 Main 自己分析」**——那会把 vocoder 变成一个莫名其妙的动态滤波器。 |
| Z2-D3 | **每带一个 Simper SVF 带通，两路各 16 个，左右声道各一套**：`bp = k·v1`（**单位峰值增益**，不是 `v1` 的 `Q` 倍）。分析侧取 `\|bp\|`，合成侧 `bp · env[i]` 后求和。共用同一组系数（`k = 1/Q`），所以「用满幅正弦载波」时输出电平与 `Q` 无关——调 `Q` 只改选择性，不改响度。 |
| Z2-D4 | **包络跟随 = 整流 + 双系数一极点**：`env += (target − env)·c`，`target = \|bp_analysis\|`，上升用 `attack`、下降用 `release`，`c = 1 − exp(−1/(ms·1e−3·sr))` 每块算一次。`env < ENV_FLOOR (1e−6)` 直接归零：f32 一极点会停在目标上方几个 1e−5，那段残量会在静音段持续漏出 carrier。 |
| Z2-D5 | **清浊处理 = 带组之外的噪声激励旁路**，不做分类器。modulator 过 `air_hp = max(high, 4 kHz)` 的二阶 Butterworth 高通（`k = √2`）→ 同一套包络跟随（attack 固定 2 ms、release 40 ms）→ 乘**确定性 xorshift32 噪声** → 按 `sibilance` 混入。HP 起点不低于带组顶端，所以这条路径只补 16 带盖不到的气声 / 齿音，不与带组重复。**不做音高检测**：浊音段完全由载波的谐波结构承载，误分类会让噪声忽开忽关，比不做更糟。 |
| Z2-D6 | **参数 8 个**（`MAX_EFFECT_PARAMS = 12` 有余量）：`attack` / `release` / `q` / `low` / `high` / `sibilance` / `mix` / `output`。前五个块率（喂系数），后三个逐样本。内联两行 `sib` / `mix`。 |
| Z2-D7 | **`mix = 0` 逐样本直通**：`out = main·(1−mix) + wet·gain·mix`，`mix` 精确为 `0` 时乘 0（与 E 系列不变量一致）。滤波器与包络**照常推进**——不是省 CPU 的短路，而是让重新打开 `mix` 时没有冷启动 click（同 EQ「总是积分、只在启用时应用」）。 |
| Z2-D8 | **零延迟**：带通是 2 极点，不报告 latency，PDC 不变；两路的对齐由上游节点延迟 + Z0 的 `pdc_delay` 分流保证。`StereoClass::Mono`：参数共用，**状态逐声道独立**（左载波 × 左调制）。 |
| Z2-D9 | **全部构造期预分配**：`[BandState; 16] × 2 路 × 2 声道` + `[f32; 16]` 包络 + 每块系数表 + 噪声种子，无一 `Vec`、无一 `Box`。`effect_rt_audit` 覆盖 `process_with_aux`。 |
| Z2-D10 | **`BuiltinEffectKind::Vocoder`**，`audio_inputs() = 2`。engine 与 model 各一份枚举（两者互不依赖），model 侧 `EffectParams::Vocoder(Box<VocoderParams>)`、serde tag `vocoder`、label `"Vocoder"`、`ALL` 追加。`PROJECT_VERSION = 10` 不动。 |
| Z2-D11 | **Nyquist 防御**：带中心或 `air_hp` 越过 `0.45·sr` 时该带 `on = false`——不积分、增益 0、包络归零。低采样率（22.05 kHz 离线渲染）下把 `high` 拉到 16 kHz 也只保留能落地的带，不混叠。 |
| Z2-D12 | **不做自动电平归一**：`output`（dB，−24..+12，默认 0）由用户补偿。vocoder 的输出电平同时依赖 carrier 与 modulator 的谱能量，任何自动归一都会在演奏中抽吸。 |
| Z2-D13 | **`high ≥ max(2·low, 60 Hz)` 在 DSP 侧兜底**：两个参数范围重叠（`low` 到 1 kHz、`high` 从 1 kHz 起），用户可以把它们推到相等，此时 16 个带会叠在同一个中心频率上（共振爆炸）。系数更新时对 `high` 取 `max(high, 2·low)`，不报错、不禁止拖动。 |

## 4. 参数表

| # | 参数 | 范围 | 单位 | 曲线 | 平滑 | 速率 | 说明 |
| :-- | :-- | :-- | :-- | :-- | :-- | :-- | :-- |
| 0 | `attack` | 0.5..100 | ms | Log | 20 | Block | 包络上升一极点的时间常数 |
| 1 | `release` | 5..500 | ms | Log | 20 | Block | 包络下降一极点的时间常数；越长越「拖」 |
| 2 | `q` | 0.7..16 | ratio | Log | 20 | Block | 每带 `Q = 1/k`；越高越「机器人」、齿缝越明显 |
| 3 | `low` | 40..1000 | Hz | Log | 20 | Block | 最低带中心 |
| 4 | `high` | 1000..16000 | Hz | Log | 20 | Block | 最高带中心；DSP 侧保证 `≥ 2·low` |
| 5 | `sibilance` | 0..1 | % | Linear | 10 | Sample | 清浊旁路量；`0` = 关（默认 0.25） |
| 6 | `mix` | 0..1 | % | Linear | 10 | Sample | `0` = 逐样本直通（默认 1） |
| 7 | `output` | −24..12 | dB | Linear | 10 | Sample | 只作用于 wet 路径（默认 0） |

内联行：`sib`（slot 5）、`mix`（slot 6）。Inspector 走 descriptor 表，无需特判。

## 5. 每块执行序

```
process_with_aux(ctx, buffer, aux):
    frames = buffer.len(); channels = min(buffer.channels(), 2)
    if frames == 0 || channels == 0: return

    // 1. 块率参数 → 系数
    q, low, high        = advance(SLOT_Q / SLOT_LOW / SLOT_HIGH, frames)
    high                = max(high, 2·low)                       // D13
    for k in 0..16:
        f = low · (high/low)^(k/15)
        on = f < 0.45·sr
        (a1, a2, a3) = svf_coeffs(tpt_g(f), 1/q)
    air: hp_f = max(high, 4 kHz); on_air = hp_f < 0.45·sr
    attack_c, release_c = 1 − exp(−1/(ms·1e−3·sr))               // 每块一次
    air_attack_c, air_release_c                                      // 固定 2 / 40 ms

    // 2. 逐样本
    for i in 0..frames:
        mix  = params.tick(SLOT_MIX)
        sib  = params.tick(SLOT_SIBILANCE)
        gain = db_gain(params.tick(SLOT_OUTPUT))
        for ch in 0..channels:
            let m = aux[ch][i]  (缺声道取 0.0，缺 aux 全 0)
            let c = buffer[ch][i]
            let mut wet = 0.0
            for k in 0..16:
                if !on[k]: continue
                let am = bp_step(analysis[ch][k], m) · k_damp     // |bp| 单位峰值
                env[k] = follow(env[k], |am|, attack_c, release_c)
                let cs = bp_step(synth[ch][k], c) · k_damp
                wet += cs · env[k]
            // 清浊：带组之外的噪声激励
            if on_air && sib > 0:
                let a = hp_step(air[ch], m)
                air_env[ch] = follow(air_env[ch], |a|, air_attack_c, air_release_c)
                wet += next_noise(ch) · air_env[ch] · sib
            buffer[ch][i] = c · (1 − mix) + wet · gain · mix

    // 3. 收尾：denormal flush（每块一次），静音段 env 归零
```

要点：

- **aux 读取按声道 + 越界取 0**：`aux.channel(ch).or_else(|| aux.channel(0))`，`aux.len()` 可能与 `frames` 不同（离线最后一块）。
- **`aux == None` 整块包络为 0** → `wet = 0` → 输出 = `main·(1−mix)`。`process`（单口图不可达）走同一条路径。
- **`on = false` 的带整段跳过**并把 `env[k]` 置 0：否则它重新生效时会带着旧包络弹出。
- **状态在参数变化时不重置**：改 `q` / `low` / `high` 只换系数，SVF 积分器继续跑，因此拖参数不会清空尾音（可能有一个小的瞬态，可接受）。

## 6. 各层改动

| 层 | 文件 | 改动 |
| :--- | :--- | :--- |
| engine | `realtime/vocoder.rs`（新） | `BANDS` / `SLOT_*` / `VOC_PARAMS` / `DESCRIPTOR` / `BandState` / `ChannelState` / `VocoderProcessor`；`Processor::process` + `process_with_aux` + `set_param`；单元测试 |
| engine | `realtime/mod.rs` | `pub mod vocoder;` + `pub use vocoder::VocoderProcessor;` |
| engine | `realtime/effect/mod.rs` | `BuiltinEffectKind::Vocoder` + `ALL` 追加 + `audio_inputs() → 2` + `descriptor()` 分发 |
| model | `src/lib.rs` | `VocoderParams`（8 字段，全 `serde(default)`）；`BuiltinEffectKind::Vocoder`（label `"Vocoder"`、`audio_inputs() → 2`、`ALL` 追加）；`EffectParams::Vocoder(Box<VocoderParams>)` + `kind()` + `as_vocoder()` / `as_vocoder_mut()`；`effect_slot_axis` 8 行 |
| ui | `src/effect.rs` | `engine_kind` / `default_params` / `processor` / `value` / `set_value` 五处分派；meters 断言不变（vocoder 不带表） |
| ui | `src/patch*.rs` / `headless.rs` | **无需改动**：端口数由 `NodeDoc::audio_inputs()` 推导（Z0 已接通），`spawn_effect` 走 `effect::processor` 的通用路径 |

## 7. 验收

- [ ] 描述符合规：`validate_descriptor` 通过，8 个槽位范围 / 默认 / 曲线自洽；model 默认值与 descriptor 默认值逐项一致（既有 `model_defaults_match_the_engine_descriptors` 覆盖）。
- [ ] 静音语义：有载波、aux 全 0 → 输出逐样本 0（`mix = 1`）；`aux = None` 同结果；`mix = 0` → 逐样本直通（bit-exact）。
- [ ] 包络跟随：carrier = 带中心正弦、modulator = 同频门控突发 → 突发段输出 RMS 明显高于空隙段；`attack` 越长，起始段能量越低。
- [ ] 频率选择性：modulator = 300 Hz 正弦、carrier = 白噪 → 输出能量集中在 300 Hz 附近（Goertzel：`|X(300)| >> |X(3000)|`）。
- [ ] 带边界：同一 100 Hz modulator，`low = 200` 时输出近静音，`low = 80` 时有能量（带组真的在动）。
- [ ] `q` 选择性：`q = 12` 时偏离带中心的 modulator 贡献显著小于 `q = 1`。
- [ ] 清浊旁路：carrier = 150 Hz 正弦、modulator = 白噪；`sibilance = 1` 时 9 kHz 处有能量，`sibilance = 0` 时近静音。
- [ ] 声道独立：只有 aux 左声道有信号 → 左声道有声、右声道静音。
- [ ] 极端参数：`q = 16`、`low = high`、满幅方波 carrier + 满幅 modulator、NaN 参数 → 输出全部 `is_finite()`。
- [ ] `effect_rt_audit`：`VocoderProcessor process_with_aux` 零分配（含 `set_param`）。
- [ ] model：`EffectParams::Vocoder` RON 往返、旧 payload 缺字段取默认、`GraphDoc::validate` 接受 `in_port = 1`、`effect_slot_axis` 八行与 descriptor 一致。
- [ ] 离线端到端：`--render` 一段 vocoder 工程（carrier + modulator 两轨）出声且 RMS > 0。
- [ ] **零回归**：默认工程 `--render` 与 Z2 之前逐字节一致。
- [ ] `fmt` / `clippy --workspace --all-targets -D warnings` / `test --workspace` / `build --workspace` 全绿。

## 8. 风险与已知取舍

| 风险 | 处置 |
| :--- | :--- |
| 用户按「效果器插在 vocal 轨上」的 DAW 直觉接线，把 vocal 接 Main、合成器接 Mod → 听到的是「合成器被 vocal 的谱包络塑形」吗？不是，方向反了：Main 是 carrier | 手册里用一句话钉死：**Main 接载波（合成器 / 噪音），Mod 接人声**；并写明「接反了不会有声音错误，只是载波与调制源互换」。参数提示与节点标题都不足以表达，必须写进文档。 |
| 输出电平取决于两路素材（宽带噪声载波比正弦载波低得多），用户会觉得「vocoder 特别小声」 | `output` 默认 0 dB 但范围到 +12；手册提示「先调 output，再调 Q」。自动归一被明确否决（D12）。 |
| 16 带 × 2 路 × 2 声道 = 每帧 64 个 SVF，是当前最重的内置效果 | 全部预分配、无分配无锁；系数每块算一次；`q = 16` 时也不改复杂度。若将来 CPU 吃紧，优化方向是「分析侧降到 8 带 + 合成侧全 16 带」这类结构性取舍，不在本 spec。 |
| 清浊旁路用噪声，齿音在长 release 下会有轻微「嘶」的尾巴 | `sibilance` 默认 0.25 而不是 1；旁路有自己的固定 2/40 ms 包络，不受 `release` 影响（否则慢 release 会把齿音拖成一片）。 |
| 低采样率下 `high = 16 kHz` 会让大半带失效 | D11 的 Nyquist 防御保证不混叠；手册不承诺「任意采样率下 16 带全活」。 |
| modulator 源误接 Master → 人声漏进主输出 | Z0 已写明的通用说明，手册的 vocoder 小节再指一次。 |

## 9. 边界与明确不做

- `PROJECT_VERSION` 不动、文档零新增字段、旧工程零迁移（纯加法变体）。
- 不做 formant shift、freeze、可变带数、内置载波、逐带编辑、逐带声像、M/S 处理。
- 不做音高检测 / 清浊分类器（D5 的取舍），不做 listen / solo modulator。
- 不给 CLAP / VST3 效果器开第二个口；不引入新 crate；不新增 `ParamId`（全部走 `BuiltinParam(slot)`）。
