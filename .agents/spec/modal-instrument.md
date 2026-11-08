# B1：模态打击声源 Spec（Mallet / 诵钵）

> 状态：定稿（2026-11-08）。上游：`plan.md` Z / B 系列（B1 决策）、`spec/instrument.md`（synth 的声部 / note 语义）、`spec/drum-kit.md`（`VoiceSpec` 加法变体与一击到底的先例）。
> 范围：只做**一个单次敲击（strike）的模态合成器**。bow / rub 持续摩擦、逐模态编辑器、预设文件系统不在本 spec（用户 2026-11-08 决议 Q2：只做敲击）。

## 1. 背景

用户要「敲击金属器具的合成器，类似 LMMS 内置的 mallet，最好能做出敲诵钵那种悠长的声音」。

这件事 **不能** 用现有内置合成器（`SynthProcessor`：osc → SVF → ADSR）或 Y 系列的 Pluck 凑出来：

| 维度 | Synth / Pluck | 诵钵要的 |
| :--- | :--- | :--- |
| 频谱 | 谐波（1/n、1/n² 或延迟线谐波） | **非谐波**模态比（1 : 2.71 : 5.15…） |
| 包络 | ADSR 全局包络 | **每个模态各自的衰减**（低频模态 20 s，高次模态 3 s） |
| 生命期 | note-off 决定 | 敲一下响到底，note-off 不影响尾音 |
| 拍频 | 无 | 同一模态的一对轻微失谐产生 1–5 Hz 拍频，是「活」的来源 |

这正是 STK ModalBar / LMMS Mallets 的路线：**一组二阶谐振器 + 一次宽带激励**。一个引擎用不同预设表即可覆盖 mallet（短衰减、近谐波）与 bowl（长衰减、强拍频）。

现有可复用的资产：

| 资产 | 位置 | 复用方式 |
| :--- | :--- | :--- |
| `note_to_frequency(note)` | `realtime/synth.rs` | 直接调用，A4 = 440 Hz |
| 声部池语义（先释放同音、空闲槽优先、偷最老） | `realtime/synth.rs` D10–D12 | 移植到本模块，语义一致 |
| 一击到底 / note-off 忽略 / blanket fade | `realtime/drum_kit.rs` K6 | 照搬（含 `Off{0}` 哨兵） |
| `VoiceSpec` 加法变体 | `spec/drum-kit.md` K1 | `VoiceSpec::Modal { params }`，`PROJECT_VERSION` 不动 |
| 单处装配 | S0 `headless::spawn_instrument` | 只加一个分支 |

## 2. 目标 / 非目标

**目标**：新增内置乐器 **Mallet**（模态敲击），预设族 `Mallet / Vibraphone / Bell / Singing Bowl`；单次敲击激励；每模态独立衰减；失谐对产生拍频；16 声部；实时零分配零锁；`--render` 可自动验收长尾与频谱。

**非目标**：bow / rub 持续激励、逐模态频率/衰减编辑、用户预设文件、敲击位置（strike position）滤波、立体声展开（v1 单声道送所有通道）、Muted / harmonics 变体、采样混合（layered sample）。

## 3. 决策

| ID | 决策 |
| :--- | :--- |
| B1-D1 | **`VoiceSpec::Modal { params: ModalParams }` 加法变体**（serde tag `modal`），旧文件零迁移，`PROJECT_VERSION` 保持 10。模态声源没有外部文件，**不存在「空路径」态**，因此也不需要「空 = 静音」的回落规则——非法 / 非有限参数一律在引擎侧 clamp 到合法域。 |
| B1-D2 | **每个模态是一个复旋转子**（quadrature oscillator）：状态 `(re, im)`，每采样 `re' = c·re − s·im`、`im' = s·re + c·im`（`c = cos θ`、`s = sin θ`，θ = 2πf/fs），衰减**每块乘一次** `exp(−frames/(τ·fs))`。理由：① 直接写两极点递归 `y[n] = 2r cosθ·y[n−1] − r²y[n−2]` 时 `r` 在 20 s 尾音下是 `0.9999979`，f32 在 1.0 附近的分辨率只有 `1.19e−7`，衰减率误差可达数个百分点；② 块率衰减把 `exp` 从内层循环挪到每块，16 模态 × 16 声部 × 375 块/s 仍可忽略，且 f32 乘法级联稳定；③ 冲激响应幅度天然归一（单位冲激 → `|a| = 1`），不需要按 `1/sin θ` 做增益补偿。 |
| B1-D3 | **容量**：`MAX_PARTIALS = 8`（预设表条目）、`MAX_RESONATORS = 16`（**带拍频的条目占 2 个谐振器**）、`MAX_VOICES = 16`、`MAX_EXCITATION_FRAMES = 512`。全部在构造期预分配；热路径零分配、零锁、零 syscall。 |
| B1-D4 | **单次敲击激励**：note-on 生成一段噪声脉冲——长度由 `hardness` 决定（0.5 ms … 8.5 ms），一阶低通截止由 `brightness` 决定（300 Hz … 19 kHz），包络 `(1 − n/L)²` 保证尾部无 click。激励只送给**当前这一个**声部。`Off{n}`（n ≠ 0）**忽略**——敲击乐的尾音不该被松键掐断；`Off{0}` 是 blanket 哨兵（Stop / Panic），所有声部进入 10 ms 线性 fade。声部在 `max(|re|, |im|) < 1e−3`（约 −60 dB）时释放槽位。 |
| B1-D5 | **重复触发 / 偷声部不重置状态**：同音重击 = 在仍在响的状态上叠加一次激励（物理上就是再敲一下）；池满时偷最老的声部，**保留谐振器状态并把系数重算到新音高**——尾音会滑到新音高，被新敲击的噪声脉冲掩蔽，但没有清零带来的波形跳变。零状态清零是明确否决的方案（必然 click）。 |
| B1-D6 | **参数族**（`ModalParams`）：`preset`（离散，默认 Mallet）、`decay_scale`（0.25..4，默认 1，乘到每个模态的 T60）、`beating`（0..2，默认 1，乘到预设的拍频偏移）、`hardness`（0..1，默认 0.7）、`brightness`（0..1，默认 0.7）、`gain`（0..2，默认 1）。`hardness` / `brightness` 只影响**下一次敲击**（激励在 note-on 时冻结），其余实时作用于正在响的尾音。 |
| B1-D7 | **预设表在引擎侧是 `const` 数据**，模型只携带 `ModalPreset` 枚举（serde 索引 + UI 标签）。不新建预设文件系统（`effects.md` 已明确内建效果无预设系统，乐器沿用）。四个预设：`Mallet`、`Vibraphone`、`Bell`、`Singing Bowl`。v1 **不暴露逐模态编辑**——宏参数（decay / beating / hardness / brightness / gain）已能覆盖「短金属敲击 ↔ 长诵钵」的整个区间；逐模态表格是另一个量级的 UI（8 行 × 4 列），留作后续。 |
| B1-D8 | **衰减以 T60 定义**：预设表里的 `decay_s` 是 −60 dB 的时间，`τ = T60 / ln(1000)`。这样「诵钵 26 s」与「mallet 1.2 s」在同一量纲上可读，且声部释放阈值（−60 dB）与 T60 自然重合。 |
| B1-D9 | **Nyquist 防御**：任何模态的瞬时频率 `f0 · ratio · 2^(pitch/12) ± beat/2 ≥ 0.45·fs` 时该谐振器本块**不参与**（增益 0、状态冻结）。高音区自然只留下低次模态，不产生混叠或旋转子失稳。 |
| B1-D10 | **`ParamId` 新增 6 个**：`ModalPreset` / `ModalDecay` / `ModalBeating` / `ModalHardness` / `ModalBrightness` / `ModalGain`。`PitchOffset` 沿用既有语义（半音，块级作用于所有正在响的声部）。非有限值一律返回 `false`（与 `synth.rs` / `drum_kit.rs` 一致）。 |
| B1-D11 | **自动化词汇**：`NodeParam` 新增 `ModalDecay` / `ModalBeating` / `ModalHardness` / `ModalBrightness` / `ModalGain`，映射到对应 `ParamId`；**`preset` 不进自动化**（离散参数与 `SynthWaveform` 同例，`instrument.md` D7）。 |
| B1-D12 | **装配只改一处**：`headless::spawn_instrument` 加 `VoiceSpec::Modal` 分支；实时 / 离线共用。参数下发走两条既有路径——离线在构造期 seed（`modal_param_values`），实时靠 `engine_sync::push_node_params` 重放。 |
| B1-D13 | **UI 版图**：`NodeSource::Modal` + 插件选择器一行 `Mallet`（tooltip 列出四个预设）；Inspector 新增 modal 分区（预设下拉 + 5 个滑杆，滑杆走 `send_node_param` + 合并 undo）；画布节点内联两行 `decay` / `bright`；节点标题 `MALLET · <preset> · Track N`。 |
| B1-D14 | **不做**：bow / rub 持续激励、敲击位置滤波、逐模态编辑器、立体声模态分布、预设文件、MIDI CC 映射（`midi_cc` 返回 `true` 静默消费，与鼓机同例）、自动化 `preset`。 |

## 4. 预设表

`ratio` 相对 f0；`gain` 是相对峰值；`decay_s` 是 T60；`beat` 是该模态拍频对的总偏移（Hz，0 = 单谐振器）。实际偏移 = `beat · beating`，两个谐振器分别为 `f ± offset/2`。

**Mallet**（木琴 / 马林巴，近谐波、短衰减，无拍频）

| ratio | gain | decay_s | beat |
| :-- | :-- | :-- | :-- |
| 1.00 | 1.00 | 1.2 | 0 |
| 3.93 | 0.45 | 0.7 | 0 |
| 9.24 | 0.18 | 0.35 | 0 |
| 16.00 | 0.07 | 0.20 | 0 |

**Vibraphone**（金属棒，中等衰减，基频与 4 次模态带电机式拍频）

| ratio | gain | decay_s | beat |
| :-- | :-- | :-- | :-- |
| 1.00 | 1.00 | 6.0 | 1.6 |
| 4.00 | 0.35 | 3.0 | 1.6 |
| 10.00 | 0.15 | 1.5 | 0 |
| 15.90 | 0.06 | 0.8 | 0 |

**Bell**（钟 / 管钟，非谐波，长衰减，前四个模态各带拍频）

| ratio | gain | decay_s | beat |
| :-- | :-- | :-- | :-- |
| 0.56 | 0.70 | 14.0 | 0.6 |
| 0.92 | 0.90 | 11.0 | 0.5 |
| 1.19 | 1.00 | 9.0 | 0.4 |
| 1.71 | 0.55 | 7.0 | 0.3 |
| 2.00 | 0.50 | 5.0 | 0 |
| 2.74 | 0.35 | 4.0 | 0 |
| 3.00 | 0.30 | 3.5 | 0 |
| 4.07 | 0.20 | 2.5 | 0 |

> 8 条目中 4 个带拍频 → 4×2 + 4 = **12 个谐振器** ≤ 16。

**Singing Bowl**（诵钵，极长衰减，强拍频，1 : 2.71 : 5.15 的钵体模态）

| ratio | gain | decay_s | beat |
| :-- | :-- | :-- | :-- |
| 1.00 | 1.00 | 26.0 | 1.2 |
| 2.71 | 0.45 | 18.0 | 0.9 |
| 5.15 | 0.22 | 12.0 | 0.7 |
| 7.90 | 0.10 | 8.0 | 0.5 |
| 11.60 | 0.05 | 5.0 | 0 |

> 5 条目中 4 个带拍频 → 8 + 1 = **9 个谐振器**。

振幅归一：每个预设的 `Σ gain` 不同（Mallet 1.70 / Vibraphone 1.56 / Bell 4.50 / Bowl 1.82），激励幅度按 `1/Σgain` 归一，再乘 `gain` 参数与 `velocity^1.5`，四个预设响度接近。

## 5. 处理链

```
note-on ──► 激励生成（噪声 → 一阶 LP → (1−n/L)² 窗）
                 │
                 ├─► 谐振器 0 (re,im) ─┐
                 ├─► 谐振器 1          │ 每采样复旋转
                 │   …                 ├─► Σ · gain · velocity ──► 所有输出通道
                 └─► 谐振器 15         ┘ 每块 exp 衰减
```

- 声部渲染进预分配的 mono scratch，再累加到输出（与 `SynthProcessor` 同形）。
- 空闲声部（已释放）零成本跳过。
- `frames > MAX_EXCITATION_FRAMES` 时激励截断到上限，其余采样只做旋转（防御大 host block）。

## 6. 参数表

| # | `ParamId` | 范围 | 默认 | 速率 | 说明 |
| :-- | :-- | :-- | :-- | :-- | :-- |
| — | `ModalPreset` | 0..3 | 0（Mallet） | Block（吸附） | 换表不重置谐振器状态 |
| — | `ModalDecay` | 0.25..4.0 | 1.0 | Block | 所有模态 T60 的倍率 |
| — | `ModalBeating` | 0.0..2.0 | 1.0 | Block | 预设拍频偏移的倍率（0 = 无拍频） |
| — | `ModalHardness` | 0.0..1.0 | 0.7 | 下次敲击 | 激励长度 |
| — | `ModalBrightness` | 0.0..1.0 | 0.7 | 下次敲击 | 激励低通截止 |
| — | `ModalGain` | 0.0..2.0 | 1.0 | Sample（每块读一次） | 节点输出增益 |

## 7. 改动面

| 层 | 文件 | 改动 |
| :--- | :--- | :--- |
| engine | `realtime/modal.rs`（新） | 预设 const 表、`ModalProcessor`、复旋转子、激励、voice 池、note 语义、`set_param` |
| engine | `realtime/commands.rs` | 6 个 `ParamId` 变体 |
| engine | `realtime/mod.rs` | `pub mod modal;` + re-export |
| model | `src/lib.rs` | `ModalPreset` / `ModalParams` / `VoiceSpec::Modal` / `NodeParam` 五个变体 + `MapSpec` 范围 |
| ui | `headless.rs` | `spawn_instrument` 的 `VoiceSpec::Modal` 分支；`modal_param_values` |
| ui | `engine_sync.rs` | `push_node_params` 的 Modal 分支 |
| ui | `lib.rs` | `node_param_id` 映射、自动化菜单条目 |
| ui | `patch_ops.rs` | `NodeSource::Modal` → 乐器节点 |
| ui | `plugin_picker.rs` | 选择器 `Mallet` 行 + `pick_is_instrument` |
| ui | `patch.rs` | `NodeSource::Modal`、节点标题、`inline_params_of`、`InlineParam::{ModalDecay, ModalBrightness}` 读写 |
| ui | `inspector_panel.rs` | modal 分区（预设 + 5 滑杆） |
| ui | `voices.rs` | 若需按节点缓存参数值则沿用既有 `node_param_values` 机制；不新增文件选择 |

## 8. 验收

- [ ] 无音符时输出**逐样本静音**（长尾已衰完的池不残留 DC）。
- [ ] Singing Bowl 预设：note-on 后 10 s 仍有可测能量（RMS > −60 dBFS），且 30 s 内归零。
- [ ] 频谱峰落在非谐波比：Singing Bowl 的 f0 / 2.71f0 / 5.15f0 附近各有峰（用 Goertzel 或过零率 + 自相关断言，不引入 FFT 依赖）。
- [ ] 拍频：`beating = 1` 时响度包络以约 `beat` Hz 起伏（对包络做低通后数峰）；`beating = 0` 时起伏消失。
- [ ] `decay_scale = 4` 的尾音能量在 10 s 后显著高于 `decay_scale = 0.25`（同一预设、同一敲击）。
- [ ] `Off{n}` 不掐断尾音；`Off{0}` 在 10 ms 内静音。
- [ ] 同音重击 = 状态叠加（幅度约 +6 dB 而非替换）；池满第 17 击偷最老声部，输出仍有限。
- [ ] `hardness` / `brightness` 改变下一次敲击的高频能量（brightness 高 → 高频占比高）。
- [ ] 满幅参数（`gain = 2`、极端音高 0 / 127、NaN 输入）输出无 NaN / Inf。
- [ ] `effect_rt_audit` 扩展：`ModalProcessor::process` / `note_event` / `set_param` 零分配。
- [ ] model：`VoiceSpec::Modal` RON 往返、旧 payload 无字段取默认、`GraphDoc::validate` 接受。
- [ ] 离线端到端：`--render` 出一段可听的 modal 工程（Bowl 预设，`--to` 覆盖尾音），GUI 导出与 CLI 逐字节一致。
- [ ] **零回归**：默认工程 `--render` 与 B1 之前**逐字节一致**（Modal 是纯加法变体）。
- [ ] `fmt` / `clippy --workspace --all-targets -D warnings` / `test --workspace` / `build --workspace` 全绿。

## 9. 风险与已知取舍

| 风险 | 处置 |
| :--- | :--- |
| 长尾音占满 16 个声部（Bowl 单音 26 s） | 这是乐器本身的物理属性；偷声部保留状态并重算系数（B1-D5），不会 click。文档写明「长尾预设同时可叠加的敲击数受 16 声部限制」。 |
| 每块衰减在内层循环外 → 2.7 ms 幅度台阶 | 最坏是 Mallet 的 0.2 s 模态（每块 ≈ 1.3%），落在 −37 dB 边带，听感不可闻；换来 f32 长尾的数值稳定（B1-D2）。 |
| 换预设时正在响的尾音滑到新模态比 | 与偷声部同一机制（保留状态、重算系数）；新预设的敲击通常紧随其后，遮蔽滑音。换预设**不重置**池是有意选择——重置会 click。 |
| 高音区高次模态超过 Nyquist 被丢弃 | B1-D9 的显式防御：音色在高音区自然变「闷」，不产生混叠。 |
| 4 个预设的 `Σgain` 差异大 | 激励按 `1/Σgain` 归一（§4 末尾），`gain` 参数再统一调节。 |
| v1 没有逐模态编辑，调不出「非预设」的音色 | 宏参数覆盖「短金属 ↔ 长诵钵」区间；真正自由的模态编辑（8×4 表格 + 编辑器）是独立工程量，留待后续 spec。 |
