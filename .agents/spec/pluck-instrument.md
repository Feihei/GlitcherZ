# S1：拨弦声源 Spec（Pluck / Karplus-Strong）

> 状态：定稿（2026-11-08）。上游：`plan.md`（Y 系列 S1，Z / B 执行顺序第 5 项）、`spec/instrument.md`（声部 / note 语义）、`spec/drum-kit.md`（`VoiceSpec` 加法变体先例）、`spec/modal-instrument.md`（同一家族的声源形状：一个引擎 + 宏参数）。
> 范围：**单次拨弦**的 Karplus-Strong 延迟线。不做弓 / 摩擦激励、不做逐谐波编辑、不做采样混合。

## 1. 背景

`plan.md` 把 Pluck 列为三个待做乐器里性价比最高的一项（「激励噪声 + 延迟线 + 阻尼低通，RT 零分配，可复用 SVF / ADSR」）。B1 的模态引擎已经证明「一个引擎 + 宏参数 + 单处装配」这条形状可行，但 **Pluck 不能复用它的谐振器组**：

| 维度 | Pluck（Karplus-Strong） | Modal（B1） |
| :--- | :--- | :--- |
| 结构 | **一根弦**：一条延迟线 + 一个环路阻尼滤波 | 一组并联二阶谐振器 |
| 频谱 | 严格谐波（f0, 2f0, 3f0…），由延迟长度决定 | 非谐波比（1 : 2.71 : 5.15…），由模态表决定 |
| 衰减 | 全 harmonics 共享一个环路增益，阻尼滤波让高次先死 | 每个模态各自一个 T60 |
| 激励 | 一个周期的噪声（弦的初始形状） | 一次极短的敲击脉冲 |
| 参数 | 弦长（音高）、衰减、阻尼、激励亮度 | 预设表 + 衰减倍率 + 拍频 |

两者同属「激励—共鸣」，但 DSP 没有可共用的一行代码，所以是独立模块（与 B1 同级的 `realtime/pluck.rs`）。

现有可复用的资产：

| 资产 | 位置 | 复用方式 |
| :--- | :--- | :--- |
| `note_to_frequency(note)` | `realtime/synth.rs` | 直接调用，A4 = 440 Hz |
| 声部池语义（同音复用、空闲槽优先、偷最老） | `realtime/modal.rs` / `synth.rs` | 移植；偷声部保留状态 |
| 一击到底 / `Off{0}` 哨兵 | `realtime/drum_kit.rs` K6、`modal.rs` | 照搬哨兵语义，但**拨弦有阻尼**（见 S1-D5） |
| `VoiceSpec` 加法变体 | `spec/drum-kit.md` K1、`spec/modal-instrument.md` B1-D1 | `VoiceSpec::Pluck { params }`，`PROJECT_VERSION` 不动 |
| 单处装配 | S0 `headless::spawn_instrument` | 只加一个分支 |

## 2. 目标 / 非目标

**目标**：新增内置乐器 **Pluck**（Karplus-Strong 拨弦）；音高准确（含 `PitchOffset`）；`decay` / `damping` / `brightness` / `release` / `gain` 五个宏参数；16 声部；note-off 阻尼；实时零分配零锁；`--render` 可自动验收音高、T60、阻尼与阻尼衰减。

**非目标**：弓 / 摩擦（bow / rub）连续激励、拨弦位置梳状滤波（pick position）、失谐双弦 / unison、逐谐波编辑、琴体共鸣（body resonance）、立体声展开、预设枚举（理由见 S1-D7）、MIDI CC 映射。

## 3. 决策

| ID | 决策 |
| :--- | :--- |
| S1-D1 | **`VoiceSpec::Pluck { params: PluckParams }` 加法变体**（serde tag `pluck`），旧文件零迁移，`PROJECT_VERSION` 保持 10。和 B1 一样没有外部文件，**不存在「空路径」态**，节点出生即有声。 |
| S1-D2 | **DSP 是一根数字延迟线 + 一个环路一阶低通**：<br>`read = 环形缓冲[w − D]`（分数延迟用线性插值补足）<br>`lp' = a·read + (1−a)·lp`（阻尼低通，状态 per voice）<br>`ring[w] = exc + g·lp'`<br>`out = read`<br>环路周期 `D = fs / f0`，`f0 = note_to_frequency(note) · 2^(pitch/12)`。读写顺序保证 `read` 永远是 `D` 个样本之前写入的值，因此**环路周期就是 `D`**，没有隐含的额外样本延迟。 |
| S1-D3 | **环路增益由 T60 反解，并按阻尼滤波在 f0 的衰减补偿**。`g_target = exp(−ln1000 / (f0 · decay_s))`（T60 秒内的周期数 = `f0 · T60`）。阻尼低通本身在 f0 处有 `|H(f0)| < 1`，所以实际写入 `g = min(0.99995, g_target / |H(f0)|)`，其中 `\|H(f0)\| = a / sqrt(1 − 2(1−a)cos ω + (1−a)²)`、`ω = 2π f0 / fs`。这样 `decay` 描述的是**基频的 T60**，而高次谐波按阻尼滤波自然地先衰减。`0.99995` 是硬上限：阻尼很重时 `g_target / \|H(f0)\|` 会大于 1，此时**阻尼赢**、实际尾音短于 `decay`——这是物理正确的，不是 bug。 |
| S1-D4 | **环形缓冲固定容量**：`MAX_DELAY = 16384` 样本/声部（48 kHz 下最低 2.9 Hz，96 kHz 下覆盖到 MIDI 0），`MAX_VOICES = 16`，`MAX_EXCITATION_FRAMES = 4096`，`SCRATCH_FRAMES = 1024`。每声部一条 `Vec<f32>`，构造期一次性分配（16 × 64 KiB = **1 MiB 固定占用**），热路径零分配。`D` 在 `[2, MAX_DELAY − 2]` 内 clamp（插值要读 `w − i − 1`）。 |
| S1-D5 | **note 语义与 B1 不同：拨弦会被松键按住**。`Off{n}`（n ≠ 0）匹配到某个正在响的声部 → 该声部进入**阻尼**（per-sample 指数淡出，T60 = `release_s`，默认 0.15 s）；`Off{0}` 是 blanket 哨兵（Stop / Panic），所有声部进入 10 ms 阻尼。声部在环内容峰值 < 1e−4 且淡出系数 < 1e−4 时释放槽位。理由：MIDI 音符长度是拨弦乐器唯一的「闷音」手段，忽略它会让人写不出《加州旅馆》那种断奏。 |
| S1-D6 | **激励 = 一个周期的噪声**：长度 = `round(D)`（clamp 到 `MAX_EXCITATION_FRAMES`），每个样本是一阶低通后的均匀噪声（xorshift32，确定性），幅值 `0.35 · velocity^1.5`，低通截止 `400 Hz · 2^(5 · brightness) · (0.35 + 0.65 · velocity)`（上限 `0.45 fs`）。激励**加性注入**环缓冲：同音重击 = 在仍在响的弦上再拨一下；池满偷最老声部时**保留环状态、只重算延迟长度与环路系数**，尾音滑到新音高而不是清零（与 B1-D5 同一语义）。 |
| S1-D7 | **不做预设枚举**（与 B1-D7 相反，且是刻意的）。B1 的预设表是**结构不同**（模态比 / 每模态衰减），只能内建；Pluck 的「木吉他 / 贝斯 / 竖琴 / 古筝」只是三个标量的组合，预设会立刻被滑杆改脏，还多出一个「我现在在哪个预设里」的状态问题。五个宏参数直接给到用户，比四个会失真的预设更诚实。 |
| S1-D8 | **音高准确性与 clamp**：`D` clamp 后实际音高是 `fs / D`，环路增益与阻尼截止都用这个**实际 f0** 计算，因此极端音高下衰减时间仍然正确，只是音高不再跟随。`PitchOffset` 沿用既有语义（半音，块级，±24），改的是所有声部的 `D`，不重建缓冲区。 |
| S1-D9 | **`ParamId` 新增 5 个**：`PluckDecay` / `PluckDamping` / `PluckBrightness` / `PluckRelease` / `PluckGain`。非有限值一律返回 `false`（与 `synth.rs` / `modal.rs` / `drum_kit.rs` 一致）。`decay` / `damping` / `PitchOffset` 改动会重算所有声部的环路系数（`tables_dirty`），不改缓冲内容。 |
| S1-D10 | **自动化词汇**：`NodeParam` 新增同名 5 个变体，映射到对应 `ParamId`；5 个都进自动化（都是连续量，没有离散预设）。`MapSpec`：`decay` 线性 0.1..20、`damping` / `brightness` 线性 0..1、`release` 线性 0.005..2、`gain` 线性 0..2。 |
| S1-D11 | **装配只改一处**：`headless::spawn_instrument` 加 `VoiceSpec::Pluck` 分支；离线在构造期 seed（`pluck_param_values`），实时靠 `engine_sync::push_node_params` 重放（与内置合成器 / Modal 同形）。 |
| S1-D12 | **UI 版图**：`NodeSource::Pluck` + 插件选择器一行 `Pluck`（tooltip 说明 Karplus-Strong）；节点标题 `PLUCK · Track N`；画布内联两行 `decay` / `damp`（最常调的两个）；Inspector 新增 pluck 分区（5 个滑杆）。`voices.rs` 的四处分派补 `Pluck => None` / `=> {}`（无文件可选）。 |

## 4. 处理链

```
note-on ──► 噪声 ─► 一阶 LP（brightness × velocity）─► × amp ─┐
                                                              │
        ┌─────────────────────────────────────────────────────┘
        ▼
   ring[w] = exc + g · lp'          lp' = a·read + (1−a)·lp
        │                                   ▲
        │                                   │
        └──► read = ring[w − D] ────────────┘
                    │
                    └──► 输出（× gain，× 阻尼淡出，Σ 到所有通道）
```

- 声部渲染进预分配的 mono scratch，再累加到输出（与 `ModalProcessor` 同形）。
- 空闲声部零成本跳过；`frames > SCRATCH_FRAMES` 的尾部保持静音（防御大 host block）。
- **第一个周期是静音**：激励要走完一圈才到输出。D = 2 时 0.04 ms，A4 时 2.3 ms，MIDI 0 @48 kHz 时 122 ms —— 这是 Karplus-Strong 的固有性质，不是延迟缺陷；低音区用户会听到一点「起音晚」。

## 5. 参数表

| # | `ParamId` | 范围 | 默认 | 速率 | 说明 |
| :-- | :-- | :-- | :-- | :-- | :-- |
| 0 | `PluckDecay` | 0.1..20 s | 4.0 | Block | 基频 T60（阻尼很重时会更短，见 S1-D3） |
| 1 | `PluckDamping` | 0.0..1.0 | 0.35 | Block | 环路低通：`fc = f0 · 2^(2 + 3(1−damping))`，即 32·f0（亮）→ 4·f0（暗） |
| 2 | `PluckBrightness` | 0.0..1.0 | 0.6 | 下次拨弦 | 激励低通截止 `400 Hz · 2^(5·brightness)`，再乘力度系数 |
| 3 | `PluckRelease` | 0.005..2 s | 0.15 | 下次松键 | note-off 阻尼的 T60 |
| 4 | `PluckGain` | 0.0..2.0 | 1.0 | 每块读一次 | 节点输出增益 |

`decay` / `damping` 实时作用于正在响的弦（重算环路系数，缓冲内容不动）；`brightness` / `release` 只影响下一次拨弦 / 下一次松键；`gain` 逐块读一次。

## 6. 改动面

| 层 | 文件 | 改动 |
| :--- | :--- | :--- |
| engine | `realtime/pluck.rs`（新） | `PluckProcessor`、环形延迟线、分数延迟、阻尼低通、环路增益反解、激励、voice 池、note 语义、`set_param` |
| engine | `realtime/commands.rs` | 5 个 `ParamId` 变体 |
| engine | `realtime/mod.rs` | `pub mod pluck;` + re-export |
| model | `src/lib.rs` | `PluckParams` / `VoiceSpec::Pluck` / 5 个 `NodeParam` + `MapSpec` 范围 + `payload_bytes` |
| ui | `lib.rs` | `pluck_param_values`、`node_param_id`、自动化菜单条目、`InlineParam → ParamId` |
| ui | `headless.rs` | `spawn_instrument` 的 `Pluck` 分支；`spawn_effect` 的穷尽分组 |
| ui | `engine_sync.rs` | `push_node_params` 的 Pluck 分支 |
| ui | `patch.rs` | `NodeSource::Pluck`、节点标题、`InlineParam::{PluckDecay, PluckDamping}`、`PLUCK_INLINE`、`inline_params_of`、内联读写 |
| ui | `patch_ops.rs` | `NodeSource::Pluck` → 乐器节点 |
| ui | `plugin_picker.rs` | 选择器 `Pluck` 行 + `pick_is_instrument` + 描述；行数断言同步 |
| ui | `inspector_panel.rs` | pluck 分区（5 滑杆）+ 内联行写回 |
| ui | `voices.rs` | 四处 `VoiceSpec` 分派补 `Pluck => None` / `=> {}` |

## 7. 验收

> 全部回填于实施完成时；括号里是对应的测试名。

- [ ] 无音符时输出**逐样本静音**（`silent_without_a_note`）。
- [ ] 拨弦发声：第一个周期的静音之后能量出现，A4 的 RMS ∈ 0.02..0.6（`a_pluck_produces_sound`）。
- [ ] 音高准确：A4（note 69）的 Goertzel 能量在 440 Hz 处远高于 466 Hz / 415 Hz（半音邻域），且 `PitchOffset = +12` 让峰值落到 880 Hz（`pitch_follows_the_note_and_the_offset`）。
- [ ] `decay` 控制尾音：`decay = 0.3` 与 `8.0` 在 1.5 s 处的能量差至少 10 倍（`decay_sets_the_tail_length`），且实测 T60 与请求值的偏差在 ±35% 内（`decay_matches_the_requested_t60`）。
- [ ] `damping` 变暗：`damping = 1.0` 时 4·f0 的能量显著低于 `damping = 0.0`（`damping_darkens_the_spectrum`）。
- [ ] `brightness` 改变起音的频谱：高亮度时前 30 ms 的高频占比更高（`brightness_changes_the_attack`）。
- [ ] note-off 阻尼：`Off{69}` 后 0.5 s 能量衰减到松键前的 10% 以下；不发 `Off` 时仍有能量（`note_off_damps_the_string`）。
- [ ] `Off{0}` 在 50 ms 内静音（`blanket_off_stops_everything`）。
- [ ] 同音重击 = 在响的弦上叠加（能量上升），池满第 17 个音偷最老声部且输出有限（`pool_is_bounded_and_stealing_stays_finite`）。
- [ ] 非有限参数被拒，满幅参数（`gain = 2`、极端音高、NaN）输出无 NaN / Inf（`params_reject_non_finite` / `extreme_parameters_stay_finite`）。
- [ ] 零力度不发声（`zero_velocity_is_silent`）。
- [ ] 可复现：两个新实例对同一事件序列逐样本相同（`renders_are_reproducible`）。
- [ ] `effect_rt_audit` 扩展：`PluckProcessor::process` / `note_event` / `set_param` 零分配。
- [ ] model：`VoiceSpec::Pluck` RON 往返、旧 payload 无字段取默认、`GraphDoc::validate` 接受（`pluck_voice_round_trips_and_defaults`）。
- [ ] 离线端到端：`--render` 出一段可听的 pluck 工程（RMS > 0，松键后尾音收敛，`offline_render_plays_the_pluck`）；GUI 导出与 CLI 逐字节一致（既有测试覆盖同一装配）。
- [ ] **零回归**：默认工程 `--render` 与 S1 之前**逐字节一致**（detached worktree 对比，见 §9）。
- [ ] `fmt` / `clippy --workspace --all-targets -D warnings` / `test --workspace` / `build --workspace` 全绿。

## 8. 风险与已知取舍

| 风险 | 处置 |
| :--- | :--- |
| 16 声部 × 64 KiB = 1 MiB 固定内存，每个 Pluck 节点一份 | 延迟线是拨弦的唯一实现方式；1 MiB 在桌面应用里可接受，且构造期一次性分配（RT 铁律要求）。`MAX_DELAY` 按 96 kHz / MIDI 0 定，192 kHz 下极低音会被 clamp 到 2.9 Hz 附近（听不见的差异）。 |
| `decay` 很长 + `damping` 很重时实际尾音短于 `decay` | 环路增益的 `0.99995` 上限（S1-D3）：阻尼吃掉的能量无法靠增益补回来。文档写明「阻尼越重，尾音越短」。 |
| 低音第一个周期静音（MIDI 0 约 122 ms） | Karplus-Strong 的固有性质；MIDI 0 本身不是音乐用音域，C1（32.7 Hz，30 ms）以内基本无感。 |
| 线性插值的分数延迟在高频有一点点额外衰减 | 与阻尼滤波同向（高次谐波先死），听感上是「弦的阻尼」，不是失真；且 DC 处插值精确 → 基频调准。 |
| 无拨弦位置梳状滤波，音色比真吉他「平」 | 梳状滤波需要每声部的激励历史缓冲（又是一份 64 KiB），收益是音色细节；`brightness` + 力度联动已能覆盖「指弹 ↔ 硬拨」的主轴。留待后续。 |
| 池满偷声部时尾音滑到新音高 | 与 B1-D5 同一取舍：清零必 click，滑音被新激励的噪声掩蔽，是更小的恶。 |
| 阻尼是逐样本乘性淡出，`release` 很短时可能有 zipper | 淡出系数是常数（不是逐块跳变），T60 = 5 ms 时每样本乘 0.9963，包络本身平滑；没有阶跃。 |

## 9. 交付与零回归证据

> 实施完成后回填。
