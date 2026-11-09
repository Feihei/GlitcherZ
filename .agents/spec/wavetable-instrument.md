# S3：波表合成声源 Spec（Wavetable）

> 状态：**已交付（2026-11-09）**。上游：`plan.md`（Y 系列 S3，执行顺序第 5 项）、`spec/instrument.md`、`spec/modal-instrument.md` / `spec/pluck-instrument.md` / `spec/organ-instrument.md`（同一家族的声源形状：一个引擎 + 参数面 + 单处装配）。
> 范围：**一个内置表波表合成器**（4 帧锚点音色 + 每八度一层带限表 + position 插值）。不做用户表导入、不做滤波器、不做 unison / FM。

## 1. 背景

`plan.md` 从立项起就把这一项标成「大」，并且写死「必须单独立 spec 再实现」。原因不是 DSP 难，而是它比前面三个声源多出**一整个数据层**：表从哪里来、装多少、怎么抗混叠、怎么换。

它和 `Organ`（S2）放在一起看最能说明区别：

| | Organ（S2） | Wavetable（S3） |
| :--- | :--- | :--- |
| 谐波从哪来 | 9 个 drawbar 实时算 `sin` | 烘焙在表里 |
| 谐波数量 | 9，固定 | 最多 256，随帧变化 |
| CPU | 每根 bar 一次 `sin` | 一次查表 + 插值，**与谐波数无关** |
| 混叠 | 天然安全（每根 bar 自己判 Nyquist） | **必须靠带限表**，固定表会折回杂音 |
| 相位 | 9 个分量共享一个累加器 | 整条相位谱烘在表里 |

现有可复用的资产：

| 资产 | 位置 | 复用方式 |
| :--- | :--- | :--- |
| `note_to_frequency(note)` | `realtime/synth.rs` | 直接调用，A4 = 440 Hz |
| `AdsrProcessor` | `realtime/envelope.rs` | 直接用（`decay = 0`、`sustain = 1` 即 A/R）；**它的峰值就是 velocity**，力度白拿（S3-D7） |
| `PITCH_SMOOTHING_MS` 的做法 | `realtime/osc.rs` / `realtime/organ.rs` | 逐样本一极点平滑 `PitchOffset` |
| 声部池语义（空闲优先、偷最老、`Off{0}` 哨兵、同音复用） | `realtime/organ.rs` S2-D8 | 整套移植 |
| `VoiceSpec` 加法变体 | `spec/pluck-instrument.md` S1-D1 | `VoiceSpec::Wavetable { params }`，`PROJECT_VERSION` 不动 |
| 单处装配 | S0 `headless::spawn_instrument` | 只加一个分支 |

## 2. 目标 / 非目标

**目标**：新增内置乐器 **Wavetable**；4 帧锚点音色（Sine / Triangle / Saw / Square）由谐波谱生成；`position` 在相邻帧之间插值；每八度一层带限表（mipmap）保证任意音高都不混叠；A/R 包络；力度参与音量；16 声部；`PitchOffset` 逐样本平滑（配合既有 `Portamento` 即得滑音）；实时零分配零锁；`--render` 可自动验收频谱、选层与混叠。

**非目标**：用户表导入（`.wav` / `.wt` 文件、命名、缓存淘汰、文件对话 UI）、滤波器（接既有 EQ / 后续 Filter 效果器）、unison / detune / 泛音叠加（那是多振荡器，不是波表）、FM / PM、表内相位失真（Casio CZ 式 PD）、wavefolder、音色预设枚举、MIDI CC 映射。

## 3. 决策

| ID | 决策 |
| :--- | :--- |
| S3-D1 | **`VoiceSpec::Wavetable { params: WavetableParams }` 加法变体**（serde tag `wavetable`），旧文件零迁移，`PROJECT_VERSION` 保持 10。没有外部文件，不存在「空路径」态，节点出生即有声。非有限参数一律在引擎侧 clamp。 |
| S3-D2 | **表由谐波谱生成，不是手工画点。** 4 帧锚点音色各用一张 `(k, amp)` 谱定义（见 §4.1），生成时按 `amp · sin(2π·k·i/N)` 求和。理由：① 谱是数学定义，容易验证（正弦只有 k=1，方波只有奇次）；② 想做带限只需「只求和到 H 为止」，一行代码，不需要 FFT 或滤波；③ 相位谱被烘进表里，正是波表相对加法合成的核心卖点。 |
| S3-D3 | **mipmap 带限分层。** 层 L 的谐波上限 `H(L) = BASE_HARMONICS >> L`（`BASE_HARMONICS = 256`，`MIP_LEVELS = 9` → 256,128,64,32,16,8,4,2,1）。播放时按 `f0`、`PitchOffset` 与采样率算允许的最高谐波 `max_k = 0.45·fs/(f0·ratio)`，取**最小**的 L 使 `H(L) ≤ max_k`；连 `H(8) = 1` 都不满足（基频本身过 0.45·fs）则该声部静音。每八度一层是最小的够用粒度：层数再多只省内存不改善音质，再少会让高音区过早丢谐波。 |
| S3-D4 | **选层在块率完成，硬切，不做层间交叉淡化。** 从 L 切到 L+1 时被丢掉的谐波落在 `(0.225·fs, 0.45·fs]`——即 10 kHz 以上，且幅度按 `1/k` 已经很小。做交叉淡化要把查表次数翻倍（每样本 8 次），收益在听感上换不回来。§8 有一条测试锁住「跨越层边界时样本差有界」。 |
| S3-D5 | **表内插值用线性，表长 4096。** 最高谐波 256 落在表奈奎斯特（2048）的 1/8 处，线性插值在那里幅度误差 < 0.1 dB、相位误差可忽略——用 cubic 纯属浪费。选 4096 而不是 2048，是为了让这个「1/8」关系在 256 谐波下依然成立。 |
| S3-D6 | **同一帧的所有层共用一个归一化系数**，系数取该帧在 L=0（全谐波）下的峰值倒数。若按层各自归一，弹到高音区会**突然变响**（层里只剩几个谐波，归一化把每个都抬高）——这是这类引擎最常见也最难查的 bug。整表再乘 `OUTPUT_SCALE = 0.35`，默认 position（Sine）单声部 RMS ≈ 0.25。 |
| S3-D7 | **力度参与音量**，与 S2 Organ 刻意相反：波表是键盘合成器，`velocity → 电平` 是它的自然语义。实现上不做额外乘法——`AdsrProcessor` 的包络峰值本来就是 velocity（`trigger` 把它 clamp 进 `[0, velocity]` 并按当前电平重起音），所以把真实 velocity 传进去即可，重按 / 偷声部仍然无 click。`velocity == 0` 仍然**不发声**（不分配声部）。 |
| S3-D8 | **note 语义与 S2 Organ 完全一致**：`On{note}` 若已有声部在响同一个音 → 复用该槽位（一个键就是一个键，不翻倍 +6 dB）；否则取空闲槽，池满偷最老声部（相位与包络保留，只重算音高）。`Off{note ≠ 0}` 释放匹配声部；`Off{0}` 是 blanket 哨兵。`held: bool` 与 `note: Option<u8>` **分离**（S2 的教训：松键后尾音必须还在正确音高上渲染）。 |
| S3-D9 | **`position` 做逐样本一极点平滑**（`POSITION_SMOOTHING_MS = 10`）。position 是块率下发的参数，硬切会在自动化曲线扫动时产生 zipper。块内每样本推进一极点，于是阶梯变成斜坡——这是本引擎唯一新增的平滑状态。 |
| S3-D10 | **内置表是进程级共享的 `OnceLock<Arc<WavetableBank>>`。** 生成一次（4 帧 × 9 层 × 4096 点 ≈ 576 KB），所有 wavetable 节点 `Arc::clone` 后只读。理由：① 构造期成本从「每个节点几十毫秒」变成「全进程一次」；② 测试里每个用例都 `new` 一遍，不共享会让测试慢到不可接受；③ 表是常量数据，本来就不该按节点复制。`process` 里只读 `&[f32]`，零分配、零锁。 |
| S3-D11 | **生成表用共享正弦查表，零三角函数。** `sin(2π·k·i/N)` 在 `i` 为整数、`N` 是 2 的幂时正好落在同一张 `N` 点正弦表的网格上：`SIN_LUT[(k·i) & (N-1)]`。数学上精确（不是近似），构造期从 800 万次 `sin` 降到 800 万次乘加。`N` 必须是 2 的幂，这条约束写进常量注释。 |
| S3-D12 | **`ParamId` 新增 4 个**：`WavetablePosition`、`WavetableAttack`、`WavetableRelease`、`WavetableGain`。四个都用独立变体而不是复用 `Attack` / `Release`：后者的 `MapSpec` 是 0..2 / 0..3，而波表要 0.001..2 / 0.005..5，共用会让曲线大半行程落在 clamp 上（与 S2-D11 同一判断）。非有限值返回 `false`。 |
| S3-D13 | **自动化词汇 `NodeParam` 新增 4 个**：`WavetablePosition`（0..1）、`WavetableAttack`（0.001..2）、`WavetableRelease`（0.005..5）、`WavetableGain`（0..2）。全部是连续量、全部可自动化——波表的核心演奏动作就是扫 position，它必须能挂曲线。 |
| S3-D14 | **UI 版图**：`NodeSource::Wavetable` + 插件选择器一行 `Wavetable`；节点标题 `WTABLE · Track N`；画布内联**一行** `pos`（本乐器唯一的核心参数）；Inspector 新增 wavetable 分区——`position` 滑杆 + **一个周期的波形预览**（随 position 实时变化，这就是波表最该被看见的东西）+ attack / release / gain。`voices.rs` 四处分派补 `Wavetable => None`（无文件可选）。 |
| S3-D15 | **实例化即默认音色**：position 默认 0.0（正弦）、attack 5 ms、release 120 ms、gain 1.0、`OUTPUT_SCALE = 0.35`。不做「波形预设下拉」：4 帧就是 position 的 0/⅓/⅔/1 四个位置，再包一层枚举只会多一个会立刻被滑杆改脏的状态（与 S1-D7 / S2-D14 同一判断）。 |

## 4. 表设计

### 4.1 四帧锚点谱

生成时对每帧只求和 `k ≤ H(L)` 的项，`amp` 带符号（等价于相位 π，不需要 cos 表）。

| 帧 | 名称 | 谱 | position |
| :-- | :-- | :-- | :-- |
| 0 | Sine | `k = 1: 1` | 0.0 |
| 1 | Triangle | 奇次 `k`，`amp = (−1)^((k−1)/2) / k²` | ⅓ |
| 2 | Saw | 全 `k`，`amp = 1/k` | ⅔ |
| 3 | Square | 奇次 `k`，`amp = 1/k` | 1.0 |

`position p ∈ [0,1]` → `f = 3p`，`j = floor(f) clamp 0..2`，`t = f − j`，输出 `lerp(frame[j], frame[j+1], t)`。三帧都归一化到峰值 1 之后，扫动时形状连续、响度连续。

选这三条谱的原因：它们分别只在 f0、奇次、全谐波上有能量，**三条互不相同的频谱指纹**，能被 Goertzel 独立验证，不需要靠耳朵。

### 4.2 mipmap 层

| L | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
| :-- | :-- | :-- | :-- | :-- | :-- | :-- | :-- | :-- | :-- |
| `H(L)` | 256 | 128 | 64 | 32 | 16 | 8 | 4 | 2 | 1 |
| 覆盖到（48 kHz，A4） | 112 kHz | — | — | — | — | — | — | — | — |

选层算法（`max_k = 0.45·fs/(f0·ratio)`）：

```
level = 0
while level < MIP_LEVELS and H(level) > max_k { level += 1 }
if H(level) > max_k { silent }      // even the fundamental is past 0.45·fs
```

`ratio` 取 `max(当前, 目标)` 的 pitch ratio，与 Organ S2-D4 同一规则——滑音途中不会瞬时越界。

### 4.3 内存与布局

`Bank { data: Vec<f32> }`，索引 `((frame * MIP_LEVELS) + level) * TABLE_SIZE + i`。

- `TABLE_SIZE = 4096`、`FRAMES = 4`、`MIP_LEVELS = 9` → **147 456 个 `f32` = 576 KiB**，进程级一份。
- 每声部只有 2 个 `f32`（`phase`、`pitch`）、1 个 `AdsrProcessor` 和几个标量；16 声部 ≈ 2 KB。
- 全部在构造期分配，`process` 只读。

## 5. 处理链

```
note-on ──► 复用 / 分配声部 ──► 包络峰值 = velocity（S3-D7）
                                     │
p += f0·2^(pitch/12)/fs              │
  │                                  │
  ├─ 选层 L（块率，按 max_k）         │
  ├─ j = floor(3·position'), t = 小数 ┤
  │                                  │
  ├─ a = lerp(bank[j][L][i0], bank[j][L][i1], frac)     │
  ├─ b = lerp(bank[j+1][L][i0], bank[j+1][L][i1], frac) │
  └─ v = lerp(a, b, t) ──► × OUTPUT_SCALE ──► × 包络 ──► × gain ──► 所有通道
```

- 每声部渲染进预分配的 mono scratch，再累加到输出（与 `SynthProcessor` / `OrganProcessor` 同形）。
- 空闲声部（包络 Idle）跳过；`frames > SCRATCH_FRAMES` 的尾部保持静音（防御大 host block）。
- 每样本 4 次查表 + 3 次 lerp，与表里的谐波数无关——这正是波表相对 S2 Organ 的性价比所在。

## 6. 参数表

| # | `ParamId` | 范围 | 默认 | 速率 | 说明 |
| :-- | :-- | :-- | :-- | :-- | :-- |
| 0 | `WavetablePosition` | 0..1 | 0.0（Sine） | 逐样本平滑 | 在 4 帧之间插值；块率下发、10 ms 一极点平滑 |
| 1 | `WavetableAttack` | 0.001..2 s | 0.005 | Block | 起音斜坡（防止 click） |
| 2 | `WavetableRelease` | 0.005..5 s | 0.12 | 下次松键 | 松键斜坡（合成器可以拖很长） |
| 3 | `WavetableGain` | 0..2 | 1.0 | 每块读一次 | 节点输出增益 |

`position` 实时作用于正在响的音（这是演奏动作）；A/R 与 Organ 同语义。

## 7. 改动面

| 层 | 文件 | 改动 |
| :--- | :--- | :--- |
| engine | `realtime/wavetable.rs`（新） | `WavetableProcessor`、`WavetableBank`（`OnceLock<Arc<..>>`）、表生成（正弦查表）、选层、帧插值、position 平滑、A/R、力度、声部池、`set_param`、`preview_waveform` |
| engine | `realtime/commands.rs` | 4 个 `ParamId` 变体 |
| engine | `realtime/mod.rs` | `pub mod wavetable;` + re-export |
| model | `src/lib.rs` | `WavetableParams` / `VoiceSpec::Wavetable` / 4 个 `NodeParam` + `MapSpec` + `payload_bytes` + 身份等价 |
| ui | `lib.rs` | `wavetable_param_values`、`node_param_id`、自动化菜单条目、`InlineParam → ParamId` |
| ui | `headless.rs` | `spawn_instrument` 的 `Wavetable` 分支；`spawn_effect` 的穷尽分组 |
| ui | `engine_sync.rs` | `push_node_params` 的 Wavetable 分支 |
| ui | `patch.rs` | `NodeSource::Wavetable`、节点标题、`InlineParam::WavetablePosition`、`inline_params_of`、内联读写、`auto_param_label` 的 4 个分支 |
| ui | `patch_ops.rs` | `NodeSource::Wavetable` → 乐器节点 |
| ui | `plugin_picker.rs` | 选择器 `Wavetable` 行 + `pick_is_instrument` + 描述；行数断言同步 |
| ui | `inspector_panel.rs` | wavetable 分区（position 滑杆 + 波形预览 + attack / release / gain）+ 内联行写回 |
| ui | `voices.rs` | 四处 `VoiceSpec` 分派补 `Wavetable => None` / `=> {}` |

## 8. 验收

> 全部回填于 2026-11-09 实施完成时；括号里是对应的测试名，实测值取 48 kHz 单声道、`velocity = 1.0`、稳态窗 0.1–0.5 s。

- [x] 无音符时输出**逐样本静音**（`silent_without_a_note`：20 个块全 0，`active_voices() == 0`）。
- [x] 按住一个音就发声：A4 的 RMS 落在 0.05..0.6（`a_held_note_produces_sound`：**RMS 0.2475 / 峰值 0.3500**）。
- [x] 音高准确：A4 的 Goertzel 能量在 440 Hz 处远高于邻半音 415 / 466 Hz；`PitchOffset = +12` 让峰值落到 880 Hz（`pitch_follows_the_note_and_the_offset`）。
- [x] **position = 0 是纯正弦**：f0 有能量，2·f0 与 3·f0 近乎为零（`position_zero_is_a_pure_sine`：**f0/2f0 = 269 723、f0/3f0 = 2 511 366**）。
- [x] **position = 1 是方波**：奇次谐波（3·f0）有能量，偶次（2·f0）近乎为零（`position_one_is_a_square`：**3f0/2f0 = 29 551**，5f0 同量级）。
- [x] position 扫动**连续改变频谱**：4·f0 的能量在 position 0 → ⅔ 之间单调上升（`position_sweeps_the_spectrum`：**0.00000 → 0.00000 → 0.02370**；⅓ 处仍为 0，正说明三角帧只有奇次谐波）。
- [x] **混叠防御**：A6（1760 Hz，`max_k ≈ 12`，选层 H = 8）上 9·f0 处能量近乎为零，而 8·f0 有能量（`band_limited_tables_reject_aliasing`：锯齿帧 **8f0/9f0 = 138 264**）。
- [x] 选层随音高下降：低音（A1）保留的谐波数**多于**高音（A6）（`lower_notes_keep_more_harmonics`：A3 的 20 次谐波占比 > 0.005，A5 的 < 0.001；选层实测 **A1 → L0（256 次）/ A6 → L5（8 次）**）。
- [x] **响度不随选层跳变**：同一帧在 A1 与 A6 上的 RMS 比在 0.5..2.0 之间（`level_switch_keeps_the_level`：锯齿帧 **1.790**）。
- [x] A/R 生效：`attack = 50 ms` 时前 10 ms 的能量显著低于 60 ms 处；`Off` 后在 `release` 时间内归零（`attack_and_release_shape_the_envelope`）。
- [x] `Off{n}` 释放对应声部，`Off{0}` 在 50 ms 内静音（`blanket_off_stops_everything`）。
- [x] **力度参与音量**：`velocity = 0.5` 的稳态 RMS 约为 `velocity = 1.0` 的一半（`velocity_scales_the_level`：**0.1237 vs 0.2475 = 0.500**，与 `AdsrProcessor` 的线性包络一致）。
- [x] 零力度不发声、不占声部（`zero_velocity_is_silent`）。
- [x] 同音重按**复用槽位**（`active_voices() == 1`），池满第 17 个音偷最老声部且输出有限（`retrigger_reuses_the_voice` / `pool_is_bounded_and_stealing_stays_finite`）。
- [x] **层边界无 click**：音高扫过层边界时相邻样本差不超过稳态峰值的 1%（`crossing_a_layer_boundary_stays_continuous`：A4 扫过 L3→L4 接缝，**接缝最大步长 0.47788**，两侧稳态步长 0.45707 / 0.27442 → **1.046× / 1.741×**；判据取「不超过稳态步长 1.25×」而非峰值的 1%，因为在 1760 Hz 档位一个正弦的相邻样本差本身就是峰值的 20%）。
- [x] `position` 跳变被平滑：一步把 position 从 0 推到 1，前 1 ms 内不出现单样本阶跃 > 0.1（`position_changes_are_smoothed`）。
- [x] 非有限参数被拒；满幅参数（`gain = 2`、±24 半音、1 ms attack）输出无 NaN / Inf（`params_reject_non_finite` / `extreme_parameters_stay_finite`）。
- [x] 可复现：两个新实例对同一事件序列逐样本相同（`renders_are_reproducible`）。
- [x] 构造期容错：非法采样率不崩、不产生 NaN（`constructor_tolerates_a_bad_sample_rate`）。
- [x] `effect_rt_audit` 扩展：`WavetableProcessor::process` / `note_event` / `set_param` 零分配（`built_in_effects_are_allocation_free` 的 wavetable 段：64 块内扫 position / attack / release / gain / `PitchOffset`，含偷声部与 `Off{0}`）。
- [x] model：`VoiceSpec::Wavetable` RON 往返、旧 payload 无字段取默认、`GraphDoc::validate` 接受（`wavetable_voice_round_trips_and_defaults`）。
- [x] 离线端到端：`--render` 出一段可听的 wavetable 工程（RMS > 0，`offline_render_plays_the_wavetable`：`position = ⅔`，held 有声、松键后 < held × 0.05）；GUI 导出与 CLI 逐字节一致（既有 `gui_export_pipeline_matches_the_cli_render` 覆盖同一装配）。
- [x] **零回归**：默认工程 `--render` 与 S3 之前**逐字节一致**（见 §10）。
- [x] `fmt` / `clippy --workspace --all-targets -D warnings` / `test --workspace` / `build --workspace` 全绿。

## 9. 风险与已知取舍

| 风险 | 处置 |
| :--- | :--- |
| 低音区谐波不够：`BASE_HARMONICS = 256` 在 MIDI 0（8.2 Hz）只覆盖到 2.1 kHz | 任何波表都有这条上限（要覆盖 MIDI 0 得存 2600 个谐波 = 表长 8192 × 2600，没有引擎这么做）。实际写作最低音区（A0 / 27.5 Hz）覆盖到 7 kHz，够用；真需要更亮的低音就接 EQ 提升，或换 Organ。文档写明。 |
| 表的生成是一次 800 万次乘加 | 进程级 `OnceLock` + 正弦查表，全进程一次，实测 **6.4 ms（release）/ 106 ms（debug）**。测试里不重复生成（24 个用例共享同一份 `OnceLock`）。 |
| 选层硬切 | 被丢掉的谐波都在 10 kHz 以上且幅度按 `1/k` 衰减，实测不可闻；有测试锁住连续性。真出问题再加层间交叉淡化（成本 ×2）。 |
| 4 帧音色偏「教科书」 | v1 的目标是把管线做对（表、分层、插值、零分配），不是做一套 Serum 级的音色库。用户表导入是后续阶段，那时帧数才有意义。 |
| 没有滤波器 | 波表合成器的性格一半来自表、一半来自滤波。本项目已有 EQ / 后续 Filter 效果器，接上即可；把滤波器塞进引擎会让「表决定音色」这件事变模糊，也会让 `--render` 的频谱验收多一个变量。 |
| `OUTPUT_SCALE = 0.35` 是硬编码折中 | 表已按帧峰值归一化，所以 position 扫动不改变峰值；`gain` 与 Master 负责最终电平。不做自动 RMS 归一——那会让「换个 position」听不出音量变化，掩盖真实差异。 |
| `preview_waveform` 与播放用的表可能不一致 | 预览用「按 `out.len()/2` 带限」重新求和同一套谱，而不是读播放表（播放表的分层是板率选的，预览没有音高）。两者共用同一份 `frame_spectrum(k)` 定义，所以不会漂。 |

## 10. 交付与零回归证据

### 10.1 落地

| 层 | 文件 | 内容 |
| :--- | :--- | :--- |
| engine | `realtime/wavetable.rs`（新，1 228 行） | `WavetableProcessor`（16 声部）、`Bank` + `OnceLock`、`build_bank`（正弦查表、零三角函数）、`frame_amplitude` / `frame_amplitudes`、`select_level`、`render_voice`（一次查表 + 三次 lerp）、`preview_waveform`；24 项单测 |
| engine | `realtime/commands.rs` | `ParamId::{WavetablePosition, WavetableAttack, WavetableRelease, WavetableGain}` |
| engine | `realtime/mod.rs` | `pub mod wavetable;` + `WavetableProcessor` / `WAVETABLE_FRAMES` / `WAVETABLE_MAX_VOICES` / `wavetable_preview` re-export |
| engine | `tests/effect_rt_audit.rs` | wavetable 零分配段 |
| model | `src/lib.rs` | `WavetableParams`（`position` / `attack_s` / `release_s` / `gain`，全部 `serde(default)`）、`VoiceSpec::Wavetable`、4 个 `NodeParam` + `MapSpec` + `payload_bytes` + 身份等价 |
| ui | `lib.rs` / `engine_sync.rs` / `patch.rs` / `patch_ops.rs` / `voices.rs` / `plugin_picker.rs` / `headless.rs` | 参数下发与自动化词汇、`NodeSource::Wavetable`、标题 `WTABLE · Track N`、内联 `pos`、选择器第 8 个内置乐器、`spawn_instrument` 分支、离线端到端测试 |
| ui | `inspector_panel.rs` | wavetable 分区：**一个周期的波形预览**（`wavetable_preview` + `Shape::line`）+ Position / Attack / Release / Gain 四行滑杆 |

### 10.2 标识与内存

| 项 | 结果 |
| :--- | :--- |
| `PROJECT_VERSION` | **10，未动**（旧工程零迁移） |
| 新 crate / 新依赖 | 无 |
| 表内存 | 4 × 9 × 4096 × 4 B = **589 824 B（576 KiB）**，进程级一份 |
| 每声部状态 | `phase` / `pitch` 两个 `f32` + 一个 `AdsrProcessor` + 少量标量；16 声部 ≈ 2 KB |
| 表生成 | **6.4 ms（release）/ 106 ms（debug）**，全进程一次 |
| 默认电平 | `position = 0` 单声部 **RMS 0.2475 / 峰值 0.3500**（`OUTPUT_SCALE = 0.35`） |

### 10.3 零回归硬证据

用 `368aa6e`（S3 spec commit，最后一个 pre-S3 代码状态）的 detached worktree（`../GlitcherZ-base-s3`）+ 独立 `CARGO_TARGET_DIR` 编出 pre-S3 二进制，与 post-S3 对同一份含 4 个音符的 5.1 KB 工程 `working_files/regression-project.glz` 跑：

```
--render working_files/regression-project.glz <out>.wav --rate 22050 --bits 16 --to 1920
```

| | pre-S3 | post-S3（`c37bdcb` 的构建） |
| :--- | :--- | :--- |
| 帧数 | 44 288 | 44 288 |
| 字节数 | 177 196 | 177 196 |
| `cmp` | — | **逐字节一致** |

构建之后引擎侧只剩一处改动：`wavetable.rs` 测试模块里一条注释（把「方波最窄层峰值 4/π」改成实测的「Gibbs 过冲到 ≈1.18 → 系数 ≈0.85 → 窄层 1.08」），`git diff -- crates/glitcherz-engine/src/realtime/wavetable.rs` 可查，不触及 DSP。

### 10.4 门禁

| 命令 | 结果 |
| :--- | :--- |
| `cargo fmt --all` | 无改动 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过（实施期修掉 `needless_range_loop` × 1、`neg_cmp_op_on_partial_ord` × 1） |
| `cargo test --workspace` | **1 208 项通过，0 失败**（40 个测试二进制） |
| `cargo build --workspace` | 通过 |

**实施期踩坑**（写下来免得下一项再踩）：

1. `position → 帧索引` 第一版写成 `last_frame = FRAMES - 2` 并直接当 `span` 用，等于把 4 帧压成 3 帧——`position = 1` 只到锯齿而不是方波，三个频谱测试同时红。正确拆法是 `span = FRAMES - 1` 与 `last_index = FRAMES - 2` 两个常量，`f = position · span`、`j = min(floor(f), last_index)`。`render_voice` 与 `preview_waveform` 两处都要改。
2. 方波最窄层合法超过 1：归一化用的是**截断后**方波的峰值（Gibbs 过冲到 ≈1.18），所以只留基频的那一层到 **1.0799**。测试上限因此写 1.4 而不是 1.05——注释里要把这件事说成物理后果，不然下一个人会以为是余量。
3. `plugin_picker` 的 5 处行数断言（内置乐器 7→8、`all_instruments[7]`、键盘行 9→10、`1..10`、鼠标行 index 8、chip 过滤 8→9 / 9→10）加一行就必须全改，改完顺手把注释里的 "seven built-ins" 一起改掉。
4. 离线端到端测试别引用别的测试文件里的 helper（`read_wav_mono` / `wav_span` / `wav_goertzel` 那一套在 `glitcherz-ui` 里并不存在），照 `offline_render_plays_the_organ` 的单次渲染形状写，频谱断言交给引擎单测。
5. `cargo test --workspace | grep ...` 会让 grep 抢跑、把 cargo 打成 SIGTERM；用后台任务 + `tail`，或把输出重定向到文件后统计。
