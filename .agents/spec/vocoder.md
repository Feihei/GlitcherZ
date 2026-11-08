# Z2：Vocoder Spec

> 状态：定稿（2026-11-08）；**Z2 已交付**（同日，验收见 §7，交付记录见 §10）。上游：`plan.md` Z / B 系列（Q3 决议：固定 16 带 + 外部 carrier）、`spec/dual-audio-input.md`（Z0 已交付）、`spec/sidechain-compressor.md`（Z1 已交付）、`spec/effects.md`（E 系列框架）。
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
- **电平标定**：两个满幅正弦（同频、落在带中心）实测 RMS ≈ 0.62（−4 dB）——差的那部分是包络的整流均值损失，不是缺陷；白噪载波会明显更低，所以 `output` 是必要的手动档。`matched_sines_land_near_unity` 锁住这条。

## 6. 各层改动

| 层 | 文件 | 改动 |
| :--- | :--- | :--- |
| engine | `realtime/vocoder.rs`（新） | `BANDS` / `SLOT_*` / `VOC_PARAMS` / `DESCRIPTOR` / `BandState` / `ChannelState` / `VocoderProcessor`；`Processor::process` + `process_with_aux` + `set_param`；单元测试 |
| engine | `realtime/mod.rs` | `pub mod vocoder;` + `pub use vocoder::VocoderProcessor;` |
| engine | `realtime/effect/mod.rs` | `BuiltinEffectKind::Vocoder` + `ALL` 追加 + `audio_inputs() → 2` + `descriptor()` 分发 |
| model | `src/lib.rs` | `VocoderParams`（8 字段，全 `serde(default)`）；`BuiltinEffectKind::Vocoder`（label `"Vocoder"`、`audio_inputs() → 2`、`ALL` 追加）；`EffectParams::Vocoder(Box<VocoderParams>)` + `kind()` + `as_vocoder()` / `as_vocoder_mut()`；`effect_slot_axis` 8 行 |
| ui | `src/effect.rs` | `engine_kind` / `default_params` / `processor` / `value` / `set_value` 五处分派；meters 断言不变（vocoder 不带表） |
| ui | `src/patch*.rs` / `headless.rs` | **无需改动**：端口数由 `NodeDoc::audio_inputs()` 推导（Z0 已接通），`spawn_effect` 走 `effect::processor` 的通用路径 |

## 7. 验收（✅ 2026-11-08 全部通过）

- [x] 描述符合规：`descriptor_is_well_formed`（8 槽、内联两行 `sib`/`mix`、`Mono` 不可联动）；model 默认值与 descriptor 逐项一致由既有 `model_defaults_match_the_engine_descriptors` 与 `axis_table_matches_the_engine_descriptors` 覆盖（`DocEffectKind::ALL` 遍历，新种类自动进网）。
- [x] 静音语义：`silent_modulator_stops_the_output`（aux 全 0 → 输出逐样本 `0.0`，不是「很小」）、`missing_aux_is_a_silent_modulator`（`process` 路径）、`dry_mix_is_a_sample_exact_passthrough`（`mix = 0` 与输入逐位相等）。
- [x] 包络跟随：`envelope_follows_the_modulator_gate`（0.2–0.4 s 开窗，开启段 RMS > 前后各 20 倍）、`attack_time_controls_the_onset`（1 ms vs 50 ms，起音前 6 ms 能量差 3 倍以上）、`release_length_changes_the_tail`（5 ms vs 500 ms）。
- [x] 频率选择性：`output_follows_the_modulator_frequency` —— 白噪载波 + 300 Hz modulator，Goertzel `|X(300)|` > 5 × `|X(3k)|`。
- [x] 带边界：`low_and_high_move_the_bank` —— 同一 100 Hz 素材，`low = 200` 时 RMS 0.023，`low = 80` 时 0.324（14 倍），并断言被覆盖时 RMS > 0.1（防止「两边都为零」的假绿）。
- [x] `q` 选择性：`q_controls_selectivity` —— carrier 1 kHz、modulator 2 kHz：`q = 1` RMS 1.05，`q = 16` RMS 0.018。
- [x] 清浊旁路：`sibilance_adds_a_noise_excited_top_end` —— 150 Hz 正弦载波 + 白噪调制，9 kHz 处 Goertzel 从 5.4e-7（`sibilance = 0`）升到 2.2e-3（`sibilance = 1`）。
- [x] 声道独立：`channels_use_their_own_modulator` —— aux 只有左声道有信号 → 左声道有声、右声道**恰好 0.0**。
- [x] 极端参数：`extreme_parameters_stay_finite`（`q = 16`、`low = high = 1 kHz`、满幅方波对、`output = +12`、`sibilance = 1`）、`non_finite_parameters_fall_back_to_defaults`（NaN / ∞ 落回合法值）。
- [x] 可复现：`renders_are_reproducible` —— 两个新实例对同一素材逐样本相同（噪声旁路用固定种子，离线渲染才可能逐字节稳定）。
- [x] 电平标定：`matched_sines_land_near_unity` —— 两个满幅正弦落在带中心时 RMS ∈ 0.45..0.85（实测 0.62），锁住 Z2-D3 的「`k·v1` 单位峰值」。
- [x] `effect_rt_audit`：`VocoderProcessor process_with_aux/set_param` 零分配（参数每块都在变，系数的 `powf` 重算也在审计范围内）。
- [x] model：`vocoder_has_two_ports_and_round_trips_its_macros` —— `audio_inputs() == 2`、label `"Vocoder"`、RON 往返、旧 payload `(kind:"vocoder")` 取默认、`GraphDoc::validate` 接受 `in_port = 1`。
- [x] 离线端到端：`offline_render_honours_the_vocoder_modulator` —— 同一工程只改 Mod 线：没接线渲染**无信号**（不是载波直通），接线后出声且 RMS > 0.005。
- [x] **零回归**：pre-Z2 二进制（`d223fb0` + 独立 `CARGO_TARGET_DIR`）与 post-Z2 对默认工程 `--render --rate 22050 --bits 16 --to 1920` 各 44288 帧 / 177196 字节，`cmp` 逐字节一致。
- [x] `fmt` / `clippy --workspace --all-targets -D warnings` / `test --workspace`（1136 项）/ `build --workspace` 全绿。

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

## 10. 交付与零回归证据

| 项 | 结果 |
| :--- | :--- |
| 落地 | 新 `realtime/vocoder.rs`（16 带带通 × 2 路 × 2 声道、双系数包络、清浊旁路、8 槽位、18 项单测）；`realtime/mod.rs` 模块 + re-export；`effect/mod.rs` 名册 / `audio_inputs()` / descriptor；model 的 `VocoderParams` / `BuiltinEffectKind::Vocoder` / `EffectParams::Vocoder` / `effect_slot_axis`；UI `effect.rs` 五处分派。 |
| 零改动的部分 | 画布、装配（`spawn_effect` 走 `effect::processor`）、删除语义、PDC、`PROJECT_VERSION` —— 全部沿用 Z0 的「端口剖面由种类推导」。 |
| 门禁 | `fmt --check`、`clippy --workspace --all-targets -D warnings`、`test --workspace` **1136 passed / 0 failed**、`build --workspace`。 |
| 实施期修订 | ① NaN 防御写成 `!(freq > 0.0)` 会被 clippy 的 `neg_cmp_op_on_partial_ord` 拒，改 `!freq.is_finite() \|\| freq <= 0.0`。② 新离线测试沿用 `"dry"/"wet"` 临时文件名会与 Z0/Z1 撞车（`save_temp` 只按 tag + pid 区分），改用 `z2_dry` / `z2_wet`。 |
