# Z1：侧链压缩 Spec

> 状态：定稿（2026-11-08）；**Z1 已交付**（同日，验收见 §7）。上游：`plan.md` Z / B 系列、`spec/dual-audio-input.md`（Z0 已交付）、`spec/effects.md`（E2b Compressor）。
> 范围：只做**一个复用现有 DSP 的双口压缩器**。Vocoder（Z2）、键控 HPF、外部键监听不在本 spec。

## 1. 背景

Z0 已经把两个语义独立的音频入口（`Main` = 0、`Mod` = 1）连同 PDC 分流做完，`Ring Mod` 只是数学样例。Z1 是 Z0 的**第一个真实消费者**：用户要的「kick 触发 bass 压缩」需要检测源来自另一条信号链，而不是压缩器自己的输入。

这是 Z1 排在被 Z0 验证过、且先于 Vocoder 的原因：侧链只需要 Mod 路的**峰值**（对逐样本对齐不敏感），而 Vocoder 要逐样本波形 + 严格相位对齐。先做要求低的那一个。

现状可直接复用的部分（`crates/glitcherz-engine/src/realtime/compressor.rs`）：

| 资产 | 说明 |
| :--- | :--- |
| `compute_gr_db(level_db, threshold, ratio, knee)` | 纯函数增益计算，含软拐点二次插值；与检测源无关，侧链只需换输入 |
| `step_gr(channel, peak, settings)` | GR（dB）的一阶平滑，attack/release 系数已就绪，含 f32 停滞保护 |
| `mix_factor(gr_db, makeup_db, mix)` | `mix = 0` 逐样本直通的既有不变量 |
| `EffectMeters` | GR 表 + 输出峰值，每块写一次 |
| `EffectParamBank` | 槽位参数，**越界槽返回 `0.0` / `false`**（`bank.rs:58-87`） |

## 2. 目标 / 非目标

**目标**：新增「Sidechain Compressor」内置效果器，用 Mod 口做外部键控检测；Main 是程序素材，Mod 是键控源；除检测源外，增益计算、平滑、makeup、mix、GR 表、参数自动化全部沿用 E2b 语义；`Compressor` 节点逐样本行为与删除语义零变化；`PROJECT_VERSION` 不动。

**非目标**：键控 HPF（把低频踢鼓从键控里滤掉）、外部键监听 / solo（listen）、前视（lookahead）、自动 makeup、程序依赖 release、多段压缩、M/S 键控、侧链电平表、CLAP/VST3 插件效果的侧链口。

## 3. 决策

| ID | 决策 |
| :--- | :--- |
| Z1-D1 | **新增种类 `BuiltinEffectKind::SidechainCompressor`，不改 `Compressor`。** 端口剖面是**节点种类**的属性（Z0-D2 已定：由 `params.kind().audio_inputs()` 推导），所以「能接侧链」必须是种类级差异。若把现有 `Compressor` 改成 2 口：所有旧工程里的压缩器都会变成 `NodeKind::Dual`，而 D19 规定 Dual 即使只有 1 条入边也不旁路 → **旧工程删除压缩器会断链**，这是行为回归；画布上每个压缩器还会多一个空 Mod 口。代价只是 Add 菜单多一项。 |
| Z1-D2 | **`sidechain` 是显式开关，默认 off（`0.0`），占 slot 8。** 关闭时用 `Main` 自己检测，与普通 `Compressor` 完全等价——新建的 `Sidechain Comp` 立刻可用，不会出现「节点装好却因为没接 Mod 线而毫无反应」。接好键控源后再把它打开。需要开关的另一个原因：`process_with_aux` 拿到的 `aux` 在 Mod 口未接线时是**静音缓冲而非 `None`**（Z0-D4 共享 scratch），处理器无法区分「没接线」与「接了静音源」；开关给了用户一个**显式**切换检测源、A/B 对比而不拆线的手段。默认 off 的代价是接线后要多一步，但默认 on 的代价是首次使用时像坏节点——取前者。 |
| Z1-D3 | **侧链开启时恒定只读 aux，绝不回落内部检测。** 这条**修订 Z0 spec 的 Z-D12**（原文「Z1 侧链压缩则回落内部检测」）：键控源安静时压缩器放行是侧链压缩的**正确**语义——kick 不打时 bass 不该被自己压下去，那正是侧链要消除的效果。若按 aux 静音回落内部检测，压缩器会在键控空隙里突然开始压主信号，听感上表现为忽轻忽重的抽吸。未接线 = 恒静音键控 = 不压缩，符合直觉。 |
| Z1-D4 | **检测源 = `max(|L|, |R|)` 的单探测器，一个 GR 同时作用于两个输出声道。** 键控是控制信号，不是音频路径；左右独立检测会让侧链 GR 随键控声像漂移。**侧链模式下 `stereo_link` 无效**（恒单探测器）。 |
| Z1-D5 | **复用 `CompressorProcessor`，不新写 DSP。** 一个 struct 服务两种 descriptor：普通 `Compressor` 的参数表没有 slot 8 → `EffectParamBank::is_on(8)` 返回 `false` → 天然走内部检测。构造入口 `CompressorProcessor::sidechain(sample_rate, meters)` 选 `SIDECHAIN_DESCRIPTOR`；`new` / `with_meters` 保持 8 参数表。 |
| Z1-D6 | **参数表 = E2b 的 8 项 + slot 8 `sidechain` 开关**，总 9 项（`MAX_EFFECT_PARAMS = 12` 有余量）。descriptor `name = "Sidechain Comp"`（节点标题宽度）。除新增槽位外，0–7 的范围、单位、曲线、平滑、速率**逐项与 E2b 相同**。 |
| Z1-D7 | **model 层共享 `CompressorParams`，新增 `sidechain: bool`（`#[serde(default)]`，即 `false`）。** `EffectParams::SidechainCompressor(Box<CompressorParams>)`。普通 `Compressor` 变体上该字段恒为默认值且**不可达**（参数表来自 descriptor，Inspector 不会显示 slot 8），所以不构成脏状态；重复 8 个字段的独立 struct 维护成本更高。 |
| Z1-D8 | **零延迟不变**：前馈、无前视，`latency_samples() = 0`。Main 与 Mod 的对齐由上游延迟节点 + Z0 的 PDC 分流保证，本 spec 不新增延迟语义。 |
| Z1-D9 | **明确不做**：键控 HPF、listen / solo key、lookahead、自动 makeup、程序依赖 release、多段、M/S 键控、侧链电平表、把 `sidechain` 做成 `ParamFlags` 而不是普通开关（它不需要 UI 特判，`Unit::Bool` 已够）。 |

## 4. 参数表

| # | 参数 | 范围 | 单位 | 曲线 | 平滑 | 速率 | 说明 |
| :-- | :-- | :-- | :-- | :-- | :-- | :-- | :-- |
| 0 | `threshold` | −60..0 | dB | Linear | 15 ms | Block | 同 E2b |
| 1 | `ratio` | 1..20 | ratio | Log | 20 ms | Block | 同 E2b |
| 2 | `attack` | 0.1..200 | ms | Log | 20 ms | Block | 同 E2b |
| 3 | `release` | 5..2000 | ms | Log | 20 ms | Block | 同 E2b |
| 4 | `knee` | 0..24 | dB | Linear | 15 ms | Block | 同 E2b |
| 5 | `makeup` | 0..24 | dB | Linear | 10 ms | Sample | 同 E2b |
| 6 | `mix` | 0..1 | % | Linear | 10 ms | Sample | 同 E2b；`0` = 逐样本直通 |
| 7 | `stereo_link` | 0/1 | bool | —— | 0（吸附） | Block | 仅内部检测时生效（Z1-D4） |
| 8 | `sidechain` | 0/1 | bool | —— | 0（吸附） | Block | **默认 0**：内部检测（等价 `Compressor`）；1 = 只读 Mod 口 |

`BLOCK` = 128 帧 ≈ 2.7 ms 的阶梯，与 E2b 一致（D11）。

## 5. 每块执行序

对 `CompressorProcessor` 的改动（`process` 与 `process_with_aux` 都收敛到一个内部 `run`）：

```
run(ctx, buffer, aux):
    frames = buffer.len(); channels = min(buffer.channels(), 2)
    if frames == 0 || channels == 0: return
    读块率参数 threshold / ratio / attack / release / knee
    linked   = params.is_on(SLOT_LINK)
    keyed    = params.is_on(SLOT_SIDECHAIN)        // 8 参数表 → 恒 false
    key      = if keyed { aux } else { None }      // 显式开关，无检测回落

    for index in 0..frames:
        mix    = params.tick(SLOT_MIX)
        makeup = params.tick(SLOT_MAKEUP)

        if let Some(key) = key:
            peak = max(|key[0][index]|, |key[1][index]|)      // 越界读 0.0
            gr   = step_gr(0, peak, settings)
            factor = mix_factor(gr, makeup, mix)
            对每个声道：sample *= factor
        else if linked:
            peak = max(|buffer[0][index]|, |buffer[1][index]|)
            gr   = step_gr(0, peak, settings)
            factor = mix_factor(gr, makeup, mix)
            对每个声道：sample *= factor
        else:
            逐声道 step_gr(ch, |x|, settings) → factor → sample *= factor
```

要点：

- **`key` 优先于 `linked`**：侧链模式下 `stereo_link` 不参与（Z1-D4）。
- **aux 读取必须带长度防御**：`aux.len()` 可能与 `frames` 不同（离线渲染最后一块），越界取 `0.0`，不 panic、不分配。
- **`block_gr` 与 `out_peak` 语义不变**：GR 表照旧每块写一次，侧链模式显示的就是键控触发的 GR。
- **零分配**：新分支只做 `channel()` + `get()` 读取，`effect_rt_audit` 覆盖。

## 6. 各层改动

| 层 | 文件 | 改动 |
| :--- | :--- | :--- |
| engine | `realtime/compressor.rs` | `SLOT_SIDECHAIN = 8`；`SIDECHAIN_PARAMS: [ParamSpec; 9]`；`SIDECHAIN_DESCRIPTOR`；`CompressorProcessor::sidechain(sr, meters)`；`process` 重构为 `run(ctx, buffer, key)` + `process_with_aux` 覆写 |
| engine | `realtime/effect/mod.rs` | `BuiltinEffectKind::SidechainCompressor` + `ALL` 追加 + `audio_inputs() → 2` + `descriptor()` 分发 |
| model | `src/lib.rs` | `CompressorParams.sidechain`（**default false**）；`BuiltinEffectKind::SidechainCompressor`（serde `sidechain_compressor`，label `"Sidechain Comp"`，`audio_inputs() → 2`，`ALL` 追加）；`EffectParams::SidechainCompressor(Box<CompressorParams>)` + `kind()` + `as_sidechain_compressor()`；参数表 slot 8 → `M::linear(0.0, 1.0)` |
| ui | `src/effect.rs` | `engine_kind` / `default_params` / `processor` / `value` / `set_value` 五处分派；`processor` 用 `CompressorProcessor::sidechain(sr, Some(handle))` |
| ui | `src/patch*.rs` | **无需改动**：端口数由 `NodeDoc::audio_inputs()` 推导，Z0 已接通 |

`NodeDoc::audio_inputs()` 的既有条件已经覆盖新种类：

```rust
NodeDoc::Effect { voice, params, .. }
    if matches!(voice, VoiceSpec::Builtin) && params.kind().audio_inputs() == 2 => 2,
```

## 7. 验收（✅ 2026-11-08 全部通过）

- [x] `sidechain = 1` + Main 恒定 0.5 + Mod 周期性脉冲 → GR 只在脉冲期间下降，Main 输出被压；脉冲结束 GR 按 release 回升。`sidechain_reduces_only_while_the_key_is_loud`
- [x] `sidechain = 1` 且 Mod 全零 → **恒不压缩**（GR = 0），Main 逐样本直通。锁住 Z1-D3，防止有人把「aux 静音回落内部检测」加回来。`silent_key_never_compresses_the_programme` + 离线端到端 `offline_render_honours_sidechain_keying` 的 dry 分支
- [x] `sidechain = 1`、Main 高于阈值、Mod 高于阈值 → 输出的降幅由 **Mod 电平**决定，与 Main 电平无关。`sidechain_reduction_ignores_the_programme_level`
- [x] `sidechain = 0` → 与 `CompressorProcessor::new` 在相同参数下**逐样本一致**。`sidechain_switch_off_matches_internal_detection`
- [x] 两路不串音。`key_signal_does_not_leak_into_the_output`（program 静音 + 键控响 → 输出仍静音，但 GR 已反应）
- [x] PDC：**由 Z0 的 `dual_input_pdc_delays_the_mod_port_to_match_the_slow_main_branch` 覆盖**（Main 声明 10 帧 latency → Mod 边补 10 帧）。侧链复用同一条分流通路，Z1 不重复该测试。
- [x] `effect_rt_audit`：`CompressorProcessor sidechain process_with_aux` 零分配
- [x] 文档侧：`sidechain_compressor_has_two_ports_and_a_persistent_switch` —— `GraphDoc::validate` 接受 `(SidechainCompressor, in_port = 1)`、RON 往返保留 `sidechain`、旧 payload 默认**关闭**键控
- [x] 离线端到端：`headless::assemble_offline` 把 `SidechainCompressor` 装成 `NodeKind::Dual`，dry / wet / internal 三方 RMS 断言
- [x] **零回归**：默认工程 `--render --rate 22050 --bits 16 --to 1920`，pre-Z1（`aaeb958` + 独立 `CARGO_TARGET_DIR`）与 post-Z1 各 **44288 帧 / 177196 字节，`cmp` 逐字节一致**
- [x] `fmt` / `clippy --workspace --all-targets -D warnings` / `test --workspace` / `build --workspace` 全绿
- [x] 普通压缩器：`plain_compressor_has_no_sidechain_slot`（slot 8 被拒、传 aux 也不改变输出）

## 8. 风险与已知取舍

| 风险 | 处置 |
| :--- | :--- |
| 用户接了 Mod 线却忘了打开 `sidechain` → 「侧链不起作用」 | 默认 `sidechain = off` 是**有意**的：新建节点立刻按普通压缩器工作，不会出现「装好却毫无反应」；`docs/manual.md` 明确写「接好键控源后打开 `sidechain`」。取默认 off 而非 on，因为 on 的首次体验更像坏节点（没接线 = 完全不压），而 off 只是一个可直接 A/B 的开关。 |
| 把键控源也接到 Master → 键控声漏进主输出 + 可能的反馈 | Z0 已在 `docs/manual.md` 写明 modulator 源不要接 master；Z1 复用同一段说明。 |
| 侧链键控带低频内容（整段 kick）→ 低频能量主导检测 | 这是侧链压缩的固有特性，键控 HPF 已列为非目标（D9）。用户可先对键控源挂 EQ。 |
| 一个 struct 服务两种参数表 → slot 8 在普通 `Compressor` 上是死字段 | 不可达（descriptor 无该槽，Inspector 不显示、`set` 返回 `false`）；`sidechain` 字段在普通压缩器上被 serde 写成默认值，无害冗余。 |
| 侧链模式下 `stereo_link` 无效，用户可能以为开关坏了 | 参数表保留该槽（切换种类会保留参数），文档与参数提示写明「仅内部检测时生效」。 |
