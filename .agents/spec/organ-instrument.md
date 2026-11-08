# S2：拉杆风琴声源 Spec（Organ / Drawbar）

> 状态：已交付（2026-11-09）。上游：`plan.md`（Y 系列 S2，执行顺序第 5 项）、`spec/instrument.md`（synth 的声部 / note 语义）、`spec/modal-instrument.md` / `spec/pluck-instrument.md`（同一家族的声源形状：一个引擎 + 宏参数 + 单处装配）。
> 范围：**一个加法式拉杆风琴**（9 根 drawbar 正弦 + 打击泛音）。不做 scanner vibrato / chorus、不做 foldback、不做音轮漏音与键噪。

## 1. 背景

`plan.md` 把 Organ 列为三个待做乐器的第二项（「9 个 drawbar 正弦 + 打击泛音 + 滑音；参数多但线性叠加，无算法风险」）。它是三件里唯一**没有算法风险**的：频谱就是若干个整数比正弦的加权和，难的地方全在参数面与 UI。

不能直接用内置合成器凑：`SynthProcessor` 只有一个振荡器（正弦 / 三角 / 锯齿 / 方波）过一个 SVF，拉杆风琴要的是**同时**发声的 9 个严格谐波分量，各自独立电平、**没有滤波器**（音色完全由注册决定）。给每个音塞 9 个 `OscProcessor` 也行，但那是 9 个独立的相位累加器——它们会各自漂移，16' 与 8' 之间的拍频随时间变化，这是听感上立刻能发现的错。

现有可复用的资产：

| 资产 | 位置 | 复用方式 |
| :--- | :--- | :--- |
| `note_to_frequency(note)` | `realtime/synth.rs` | 直接调用，A4 = 440 Hz |
| `AdsrProcessor` | `realtime/envelope.rs` | 直接用（`decay = 0`、`sustain = 1` 即 A/R）；`trigger` 从**当前电平**重新起音，天然支持重触发与偷声部 |
| `PITCH_SMOOTHING_MS` 的做法 | `realtime/osc.rs` | 逐样本一极点平滑 `PitchOffset`，同一常量与语义 |
| 声部池语义（空闲优先、偷最老、`Off{0}` 哨兵） | `realtime/synth.rs` D10–D12 | 移植；同音重触发改为**复用槽位**（见 S2-D8） |
| `VoiceSpec` 加法变体 | `spec/pluck-instrument.md` S1-D1 | `VoiceSpec::Organ { params }`，`PROJECT_VERSION` 不动 |
| 单处装配 | S0 `headless::spawn_instrument` | 只加一个分支 |

## 2. 目标 / 非目标

**目标**：新增内置乐器 **Organ**（拉杆风琴）；9 根 drawbar（16' … 1'，0..8 级）；打击泛音（2nd / 3rd + 独立衰减）；A/R 包络；16 声部；`PitchOffset` 逐样本平滑（配合既有 `Portamento` 即得滑音）；实时零分配零锁；`--render` 可自动验收谐波结构与电平阶梯。

**非目标**：scanner vibrato / chorus / rotary（Leslie）模拟、drawbar foldback、音轮漏音（leakage）、键噪（key click）、percussion 的 single-trigger（"按住旧键时不再触发"）语义、逐 drawbar 波形或微失谐、注册预设枚举、MIDI CC 映射。

## 3. 决策

| ID | 决策 |
| :--- | :--- |
| S2-D1 | **`VoiceSpec::Organ { params: OrganParams }` 加法变体**（serde tag `organ`），旧文件零迁移，`PROJECT_VERSION` 保持 10。声源没有外部文件，不存在「空路径」态，节点出生即有声；非法 / 非有限参数一律在引擎侧 clamp。 |
| S2-D2 | **9 根 drawbar 共享一个相位累加器**，基准取 **16'**（`dp = 0.5·f0/fs`，`p` 在 `0..1` 卷绕）。9 个 drawbar 相对 8' 的比值是 `0.5 / 1.5 / 1 / 2 / 3 / 4 / 5 / 6 / 8`，全部是 `0.5·f0` 的整数倍，即 `k ∈ {1, 3, 2, 4, 6, 8, 10, 12, 16}`，所以第 k 根就是 `sin(TAU · fract(k·p))`。一个累加器保证 9 个分量的相位关系**永不漂移**（9 个独立累加器会在分钟级尺度上互相滑动，16' 与 8' 的拍频会变），也省掉 8 份状态。 |
| S2-D3 | **电平阶梯 3 dB/级**：`level_gain(L) = 10^(−0.15·(8−L))`，`L = 8` → 1.0、`L = 4` → 0.251、`L = 1` → 0.089；`L = 0` 特判为**精确 0**（关掉，不是 −24 dB）。输出再乘 `OUTPUT_SCALE = 0.25`：默认注册 `888000000` 的单声部峰值 ≈ `3 × 0.25 = 0.75`，与内置合成器的单位振荡器同一量级，多声部叠加时靠 `gain` 与 Master 收。 |
| S2-D4 | **Nyquist 防御**：某根 drawbar 令 `k · 0.5 · f0 ≥ 0.45·fs` 时本块**不参与**（掩码在块边界按目标音高算，判定用 `max(当前, 目标)` 的比值，滑音途中也不会瞬时越界）。高音区自然掉上方的 drawbar（音色变薄），与模态引擎 B1-D9 同一条规则；v1 **不做 foldback**（见 §8）。 |
| S2-D5 | **包络只有 A/R**：复用 `AdsrProcessor`，`decay = 0`、`sustain = 1.0`（零时长的段在同一样本内跳过）。默认 `attack = 5 ms`、`release = 40 ms`——风琴没有 ADSR，但键开关必须有斜坡，否则每个音的头尾都是 click。 |
| S2-D6 | **力度不参与音量**（真实音轮风琴的键触点不测力度），`trigger` 恒传 `velocity = 1.0`。`velocity == 0` 仍然**不发声**：与另外两个引擎一致，也兼容「velocity 0 当 note-off」的 MIDI 习惯。响度来源是注册 + `gain`，这是乐器本身的性格，不是遗漏。 |
| S2-D7 | **打击泛音 = 一根额外的正弦**，取 2nd（4'，`k = 4`）或 3rd（2⅔'，`k = 6`）谐波，带自己的指数衰减（T60 = `percussion_decay`），**加在总包络之前**（键松开时打击音一起被关掉，与真机一致）。每次 note-on 都重新触发——真机是 single-trigger（按住别的键时不再触发打击），这条差异明确写进文档。`percussion = 0` 时整段跳过，零成本。衰减用逐样本一极点（`perc -= perc·c`，`c = 1 − exp(−ln1000/(T60·fs))`），比块率乘法少一次台阶。 |
| S2-D8 | **note 语义**：`On{note}` 若已有声部在响同一个音 → **复用该槽位**（从当前电平重新起音 + 重触发打击），不新占声部——一个键就是一个键，重复按不该翻倍 +6 dB；否则取空闲槽，池满偷最老声部（`AdsrProcessor::trigger` 从当前电平起音，偷声部天然无 click）。`Off{note}`（≠0）释放匹配声部；`Off{0}` 是 blanket 哨兵（Stop / Panic）。 |
| S2-D9 | **不做内置滑音**。`PitchOffset` 按 `OscProcessor` 的同一常量（5 ms 一极点）**逐样本**平滑，因此既有的 `Portamento` MIDI FX（块率发 `PitchOffset`）直接给出滑音，且白拿 `legato_only` / `min` / `max` 三个开关。再做一个引擎内的 glide 只会是同一功能的有损副本，还关不掉。 |
| S2-D10 | **`ParamId` 新增 7 个**：`OrganBar(u8)`（payload = drawbar 索引 0..8）、`OrganPercussion`、`OrganPercussionDecay`、`OrganPercussionHarmonic`、`OrganAttack`、`OrganRelease`、`OrganGain`。`OrganBar` 用 payload 变体而不是 9 个枚举变体（与 `DrumSlotGain(u8)` 同例），`set_param` 里只有一条分支。非有限值返回 `false`；`OrganBar` 索引 ≥ 9 也返回 `false`。 |
| S2-D11 | **自动化词汇 `NodeParam` 新增 14 个**：`OrganBar(u8)` × 9（标签 `16'` … `1'`）+ `OrganPercussion` / `OrganPercussionDecay` / `OrganAttack` / `OrganRelease` / `OrganGain`。**打击谐波不进自动化**（离散值，与 `ModalPreset` / `SynthWaveform` 同例）。`OrganAttack` / `OrganRelease` **不**复用 `NodeParam::Attack` / `Release`：后者的 `MapSpec` 是 0..2 / 0..3，而风琴的合法区间是 0.001..0.2 / 0.005..1.0，共用会让曲线 90% 的行程落在 clamp 上。`OrganBar` 的 `MapSpec` 是 0..8。 |
| S2-D12 | **装配只改一处**：`headless::spawn_instrument` 加 `VoiceSpec::Organ` 分支；离线在构造期 seed（`organ_param_values`，9 根 bar + 6 个宏共 15 条），实时靠 `engine_sync::push_node_params` 重放（与 S1 / B1 同形）。 |
| S2-D13 | **UI 版图**：`NodeSource::Organ` + 插件选择器一行 `Organ`（tooltip 说明拉杆风琴）；节点标题 `ORGAN · Track N`；画布内联两行 `8'`（drawbar 2）与 `perc`（最常拧的两个）；Inspector 新增 organ 分区——3×3 的 drawbar 网格（每格 0..8 整数滑杆，标签用 footage）+ 打击（level / decay / 2nd·3rd 切换）+ attack / release / gain。`voices.rs` 的四处分派补 `Organ => None` / `=> {}`（无文件可选）。 |
| S2-D14 | **不做注册预设枚举**（与 S1-D7 同一判断）：`888000000` 之类的注册就是 9 个整数，枚举出来立刻被滑杆改脏，还多一个「我在哪个注册里」的状态。默认值直接取 `888000000`，其余交给用户。 |

## 4. Drawbar 表

基准相位 `p` 以 **16'** 的速率推进：`dp = 0.5 · f0 / fs`。

| # | 面板 | footage | 相对 8' | 相位倍数 k |
| :-- | :-- | :-- | :-- | :-- |
| 0 | `16'` | 低八度 | 0.5 | 1 |
| 1 | `5⅓'` | 16' 的三次谐波 | 1.5 | 3 |
| 2 | `8'` | 同度 | 1.0 | 2 |
| 3 | `4'` | 八度 | 2.0 | 4 |
| 4 | `2⅔'` | 三次谐波 | 3.0 | 6 |
| 5 | `2'` | 两个八度 | 4.0 | 8 |
| 6 | `1⅗'` | 五次谐波 | 5.0 | 10 |
| 7 | `1⅓'` | 六次谐波 | 6.0 | 12 |
| 8 | `1'` | 三个八度 | 8.0 | 16 |

打击泛音：`2nd` → `k = 4`（4'），`3rd` → `k = 6`（2⅔'）。

电平阶梯（`L = 0` 精确关断）：

| L | 8 | 7 | 6 | 5 | 4 | 3 | 2 | 1 | 0 |
| :-- | :-- | :-- | :-- | :-- | :-- | :-- | :-- | :-- | :-- |
| gain | 1.000 | 0.708 | 0.501 | 0.355 | 0.251 | 0.178 | 0.126 | 0.089 | 0 |

默认注册 `888000000`（16' / 5⅓' / 8' 全开），`Σ = 3.0` → 单声部峰值 ≈ 0.75。

## 5. 处理链

```
note-on ──► 复用 / 分配声部 ──► 打击包络 = 1（若 percussion > 0）
                                     │
p += 0.5·f0·2^(pitch/12)/fs           │
  │                                  │
  ├─ k=1 ─► sin ─× L0 ─┐             │
  ├─ k=3 ─► sin ─× L1 ─┤             │
  │        …           ├─► Σ ─(×OUTPUT_SCALE)─► × (包络 + …) ─► × gain ─► 所有通道
  ├─ k=16 ─► sin ─× L8 ┘             │
  └─ k=4|6 ─► sin ─× percussion ─────┘   （打击电平随自己的指数衰减下降）
```

- 每声部渲染进预分配的 mono scratch，再累加到输出（与 `SynthProcessor` 同形）。
- 空闲声部（包络 Idle）跳过；`frames > SCRATCH_FRAMES` 的尾部保持静音（防御大 host block）。
- 只有**掩码里活着**的 drawbar 进入内层循环：典型注册是 3–5 根，不是 9 根。

## 6. 参数表

| # | `ParamId` | 范围 | 默认 | 速率 | 说明 |
| :-- | :-- | :-- | :-- | :-- | :-- |
| 0–8 | `OrganBar(0..8)` | 0..8（整数） | `[8,8,8,0,0,0,0,0,0]` | Block | 逐根 drawbar 电平（3 dB/级） |
| 9 | `OrganPercussion` | 0..1 | 0.0 | 下次起音 | 打击泛音电平；0 = 关（整段跳过） |
| 10 | `OrganPercussionDecay` | 0.1..2.0 s | 0.6 | 下次起音 | 打击泛音的 T60 |
| 11 | `OrganPercussionHarmonic` | 0..1 | 0.0（2nd） | 下次起音 | `< 0.5` = 2nd（4'），`≥ 0.5` = 3rd（2⅔'）；离散 |
| 12 | `OrganAttack` | 0.001..0.2 s | 0.005 | Block | 起音斜坡（防止 click） |
| 13 | `OrganRelease` | 0.005..1.0 s | 0.04 | 下次松键 | 松键斜坡 |
| 14 | `OrganGain` | 0..2 | 1.0 | 每块读一次 | 节点输出增益 |

drawbar 电平实时作用于正在响的声部（拉杆就是演奏动作）；打击三件套在 note-on 时冻结（对已经在响的打击音无效，也不会中途换谐波）。

## 7. 改动面

| 层 | 文件 | 改动 |
| :--- | :--- | :--- |
| engine | `realtime/organ.rs`（新） | `OrganProcessor`、共享相位、drawbar 掩码、打击泛音、A/R 包络、音高平滑、voice 池、note 语义、`set_param` |
| engine | `realtime/commands.rs` | 7 个 `ParamId` 变体 |
| engine | `realtime/mod.rs` | `pub mod organ;` + re-export |
| model | `src/lib.rs` | `OrganParams` / `VoiceSpec::Organ` / 14 个 `NodeParam` + `MapSpec` + `payload_bytes` |
| ui | `lib.rs` | `organ_param_values`、`node_param_id`、自动化菜单条目、`InlineParam → ParamId` |
| ui | `headless.rs` | `spawn_instrument` 的 `Organ` 分支；`spawn_effect` 的穷尽分组 |
| ui | `engine_sync.rs` | `push_node_params` 的 Organ 分支 |
| ui | `patch.rs` | `NodeSource::Organ`、节点标题、`InlineParam::{OrganBar8, OrganPercussion}`、`ORGAN_INLINE`、`inline_params_of`、内联读写、`auto_param_label` 的 7 个分支 |
| ui | `patch_ops.rs` | `NodeSource::Organ` → 乐器节点 |
| ui | `plugin_picker.rs` | 选择器 `Organ` 行 + `pick_is_instrument` + 描述；行数断言同步 |
| ui | `inspector_panel.rs` | organ 分区（3×3 drawbar 网格 + 打击 + attack / release / gain）+ 内联行写回 |
| ui | `voices.rs` | 四处 `VoiceSpec` 分派补 `Organ => None` / `=> {}` |

## 8. 验收

> 全部回填于实施完成时；括号里是对应的测试名，`→` 后是实测值。

- [x] 无音符时输出**逐样本静音**（`silent_without_a_note`）→ 全部样本精确 0.0。
- [x] 按住一个音就发声：A4 的 RMS 落在 0.05..0.6（`a_held_note_produces_sound`）→ 默认 `888000000`：RMS **0.3062**、峰值 **0.6249**（`3 × OUTPUT_SCALE` 的理论值 0.75 减去相位不相干的损失）。
- [x] 音高准确：A4 的 Goertzel 能量在 440 Hz 处远高于邻半音 415 / 466 Hz；`PitchOffset = +12` 让峰值落到 880 Hz（`pitch_follows_the_note_and_the_offset`）。
- [x] 谐波结构由 drawbar 决定：只开 `8'` 时 f0 远强于 2·f0，只开 `4'` 时反过来（`drawbars_set_the_harmonic_spectrum`）→ 只开 `8'` 时 f0 / 2f0 能量比 **1 689 272**（≈ 纯正弦）。
- [x] `16'` 单独打开时能量落在 f0/2（`sixteen_foot_adds_the_sub_octave`）。
- [x] 电平阶梯是 3 dB/级：同一根在 `L = 8` 与 `L = 4` 的能量比 ≈ 12 dB（`level_taper_is_three_db_per_step`）→ `8 → 4` 比值 **0.251**（正好 −12.0 dB）、`8 → 1` 比值 **0.089**（−21.0 dB）。
- [x] 所有 bar 归零 = 静音，即使有音符（`all_drawbars_down_is_silent`）。
- [x] A/R 生效：`attack = 50 ms` 时前 10 ms 的能量显著低于 60 ms 处；`Off` 后在 `release` 时间内归零（`attack_and_release_shape_the_envelope`）。
- [x] `Off{n}` 释放对应声部，`Off{0}` 在 50 ms 内静音（`blanket_off_stops_everything`）。
- [x] 打击泛音：`percussion = 1` 时起音瞬间 3rd / 2nd 谐波有能量、0.6 s 后显著下降；`percussion = 0` 时没有这段（`percussion_adds_a_decaying_harmonic`）→ `percussion = 1` 早 / 晚能量比 **1 479 044**；`percussion = 0` 时早期 2 次谐波仅 **0.00054**（来自 8' 自身的尾巴，不是打击）。
- [x] 打击谐波切换真的换峰：2nd 时 2·f0 强于 3·f0，3rd 时反过来（`percussion_harmonic_switches_the_peak`）。
- [x] 同音重按**复用槽位**（`active_voices() == 1`，不翻倍），池满第 17 个音偷最老声部且输出有限（`retrigger_reuses_the_voice` / `pool_is_bounded_and_stealing_stays_finite`）。
- [x] Nyquist：只开 `1'`（k = 16）时，足够高的音（16·f0 > Nyquist）输出静音（`upper_drawbars_stop_above_nyquist`）→ C8 上 `1'` 独占时峰值 **0.000000**（掩码生效，不是「很小」）。
- [x] 非有限参数被拒、非法 bar 索引被拒；满幅参数（`gain = 2`、±24 半音、1 ms attack）输出无 NaN / Inf（`params_reject_non_finite` / `extreme_parameters_stay_finite`）。
- [x] 零力度不发声（`zero_velocity_is_silent`）。
- [x] 可复现：两个新实例对同一事件序列逐样本相同（`renders_are_reproducible`）。
- [x] 构造期容错：非法采样率不崩、不产生 NaN（`constructor_tolerates_a_bad_sample_rate`）。
- [x] `effect_rt_audit` 扩展：`OrganProcessor::process` / `note_event` / `set_param` 零分配（含偷声部、`Off{0}`、`PitchOffset` 重调音）→ 每块扫 9 根 drawbar + 6 个宏 + `PitchOffset` + note-on / note-off / `Off{0}`，计数全 0。
- [x] model：`VoiceSpec::Organ` RON 往返、旧 payload 无字段取默认、`GraphDoc::validate` 接受（`organ_voice_round_trips_and_defaults`）。
- [x] 离线端到端：`--render` 出一段可听的 organ 工程（RMS > 0，`offline_render_plays_the_organ`）；GUI 导出与 CLI 逐字节一致（既有测试覆盖同一装配）→ 6 s 渲染，按住窗口 RMS **0.42235**（`bars: [8; 9]` + `percussion: 0.8`），松键后 **0.00000**。
- [x] **零回归**：默认工程 `--render` 与 S2 之前**逐字节一致**（detached worktree 对比，见 §10）→ 44288 帧 / 177196 字节，`cmp` 一致。
- [x] `fmt` / `clippy --workspace --all-targets -D warnings` / `test --workspace` / `build --workspace` 全绿 → **1175 passed / 0 failed**。

## 9. 风险与已知取舍

| 风险 | 处置 |
| :--- | :--- |
| CPU：每声部每样本最多 9 次 `sinf`（16 声部全满 = 每小时钟 144 次/样本） | 只有掩码里活着的 drawbar 进内层循环，典型注册 3–5 根；`percussion = 0` 时打击段整体跳过。真跑不动再换查表 + 线性插值（正弦不需要 PolyBLEP，换成表是等价的）——先用可读的实现，问题出现再优化。 |
| 高音区上方 drawbar 被 Nyquist 掩码吃掉，音色变薄 | 真机在同样位置是 **foldback**（谐波折回低八度），听感更连续。v1 不做 foldback：它是每根 bar 一条额外的频率映射，且与「注册决定音色」的直觉相反。文档写明高音区会变薄。 |
| 打击泛音是「每次触发」而不是真机的 single-trigger | 真机按住旧键时新键不再触发打击（保持音色的层次感）。复刻它需要一个全局的「当前有几个键按着」状态，而引擎是按声部独立的；差异在演奏和弦时能听出来，但方向是「更响」不是「更错」。 |
| 力度不起作用 | 真实音轮风琴的键触点就是开关，力度是 0 或 1。文档写明，避免用户以为节点坏了。想让键盘有动态就换 Mallet / Pluck。 |
| `OUTPUT_SCALE = 0.25` 是硬编码的响度折中 | 9 根全开时单声部峰值 2.25（会顶到 limiter）。真机也是「拉满更响」，不做自动归一——归一化会让「加一根 bar」听不出变化，那是更糟的错。 |
| 没有 vibrato / chorus / rotary | 这三件是风琴性格的另一半，但都是**效果**而不是声源：项目已有 Chorus / Phaser，用户自己接即可，v1 不在乐器里内置一份。 |
| 9 根 drawbar 都在自动化菜单里 → 菜单变长 | 这是本条乐器最核心的可自动化对象（拉杆扫动是风琴演奏的标志性动作）。菜单按「drawbar → 打击 → 包络 → gain」排序，与 Inspector 版图一致。 |

## 10. 交付与零回归证据

> 实施完成后回填。

### 落地

| 层 | 文件 | 改动 |
| :--- | :--- | :--- |
| engine | `realtime/organ.rs`（新，约 400 行实现 + 约 480 行测试） | `OrganProcessor`：单一共享相位累加器（16' 基准）+ 9 根 drawbar 的 `k` 倍数求值 + Nyquist 掩码 + 打击泛音（逐样本一极点衰减）+ A/R 包络 + 16 声部池 + 音高一极点平滑 + `set_param` |
| engine | `realtime/commands.rs` | `ParamId::OrganBar(u8)` + 6 个宏 |
| engine | `realtime/mod.rs` | `pub mod organ;` + re-export（`OrganProcessor`、`ORGAN_MAX_VOICES`、`DRAWBAR_COUNT`） |
| model | `src/lib.rs` | `OrganParams` + `VoiceSpec::Organ { #[serde(default)] params }` + 7 个 `NodeParam` + 6 条 `MapSpec` + `payload_bytes` + 身份等价 |
| ui | `lib.rs` | `ORGAN_FOOTAGES`（footage 字符串唯一来源）、`organ_param_values`（15 条）、`node_param_id`、自动化菜单、`inline_param_id` |
| ui | `patch.rs` / `patch_ops.rs` | `InlineParam::{OrganBar8, OrganPercussion}` + `ORGAN_INLINE` + `NodeSource::Organ` + 节点标题 + `auto_param_label` |
| ui | `headless.rs` / `engine_sync.rs` | `spawn_instrument` 的 Organ 分支（离线构造期 seed 15 条）+ `push_node_params` 重放 |
| ui | `inspector_panel.rs` | `organ_section`（3×3 drawbar 网格 + 打击三件套 + attack / release / gain）+ 内联写回 |
| ui | `plugin_picker.rs` / `voices.rs` | Add 菜单 `Organ` 行（内置乐器 6 → 7）+ 四处分派的「无文件」分支 |

### 标识

- `PROJECT_VERSION` 保持 **10**，旧工程零迁移（`VoiceSpec::Organ` 是 serde tag 加法，`#[serde(default)]` 让缺字段的旧 payload 读作默认 `888000000`）。
- 无新 crate、无新依赖、无新文件格式。
- 内存：每节点 `16 × 9 × 4 B` 的 bar 状态 ≈ 0.6 KB，加 1024 帧 scratch 与 16 个时间戳 = 构造期一次分配（约 5 KB）。

### 零回归硬证据

用 `063e650`（S2 spec commit，最后一个 pre-S2 代码状态）的 detached worktree + 独立 `CARGO_TARGET_DIR`（`target-s2-base`）编出 pre-S2 二进制，与 post-S2 对同一份 5.1 KB / 含 4 个音符的工程跑：

```
glitcherz --render <in.glz> <out.wav> --rate 22050 --bits 16 --to 1920
```

两份 WAV 各 **44288 帧 / 177196 字节，`cmp` 逐字节一致**。

### 门禁

| 项 | 结果 |
| :--- | :--- |
| `cargo fmt --all --check` | OK |
| `cargo clippy --workspace --all-targets -- -D warnings` | 无 error、无 warning |
| `cargo test --workspace` | **1175 passed / 0 failed**（S2 新增 organ 19 项 + model 1 项 + 离线端到端 1 项） |
| `cargo build --workspace` | OK |
