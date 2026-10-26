# 内建效果器与统一参数平滑 Spec

> 状态：草案（2026-10-26）。E1 是纯基础设施改造，**不改任何音频输出**；E2 起逐个上效果器。
> 上游：`node-graph.md`（节点图 / `install_node` / 旁路语义）、`editing-model.md`（`begin_param_edit` 手势合并）、`control-tracks.md`（自动化走 `set_param`）。
> 关联：`AGENTS.md`「实时音频铁律」、`.agents/glossary.md`「音频处理」。

## 1. 问题

引擎当前只有一个内建效果器：反馈延迟（`realtime/delay.rs`）。要做的常用效果器
（EQ / 压缩 / 混响 / 合唱 / 镶边）没有一个能"照现状再加一个"就落地，原因是三处
扩展点是扁平的、硬编码的：

| 扩展点 | 现状 | 加 6 个效果器的后果 |
| :-- | :-- | :-- |
| 种类判别 | `VoiceSpec::Builtin` 在 Effect 节点上**隐式**等于 delay（`model/src/lib.rs:937`） | 无法表达"这是一个 EQ 节点" |
| 参数存储 | `NodeDoc::Effect` 只有一个 `delay: DelayParams` 字段 | 6 个效果器 → 7 个字段并存，无法区分有效载荷 |
| 参数寻址 | `ParamId` 是扁平全局枚举，`route_slot()` 把 `DelayTime`/`DelayFeedback`/`DelayMix` 写死在 match 里（`graph.rs:751`） | 每效果 4~8 个新变体 ≈ +30 变体，`route_slot` 同步膨胀 |
| UI 参数面 | `InlineParam` 枚举 + 手写 `DELAY_INLINE` 表 + Inspector 里手写 `delay_sliders` | 每个效果器要在 4 处 UI 代码里各写一遍 |

另外两个缺失的横向能力：

- **参数无平滑**：`DelayProcessor::set_param` 直接改写字段，拖时间滑块会瞬间跳读位置（咔嗒）。效果器一多，压缩阈值、混响衰减、EQ 增益全都需要无阶跃变化。
- **无立体声联动**：压缩器若左右独立检测，中间声像的素材会"左右漂"；联动开关是混音器级效果的必需项。

## 2. 目标形态

- 新增一个内建效果器 = **一个 processor 文件 + 一条 descriptor 表 + 一条菜单项**，其余（滑条、inline 行、自动化目标、序列化、undo 标签）全部由表驱动。
- 所有内建效果器参数**统一**经同一个平滑器进入 DSP，阶跃不出声。
- 动态类效果器（压缩，后续 gate/limiter）支持**立体声联动**，文档持久化，切换不需重建节点。

## 3. 关键判断：参数必须槽位化，而不是继续加 `ParamId` 变体

`ParamId` 是"全局参数名"枚举，历史上服务的是固定三元组（voice / delay / strip）。
它的语义是 **参数种类**，不是 **参数位置**。内建效果器一旦有多个实例（两轨各一个
混响、同一链上两个 EQ），全局名字既不能区分类别也不能区分实例——真正的寻址靠的是
`EngineCommand::SetNodeParam { node_id, id }` 里的 `node_id`，`ParamId` 只需要表达
"该节点的第几个参数"。

因此新增：

```rust
/// Built-in effect parameter addressed by **slot index** into the node's
/// `EffectDescriptor` (0..MAX_EFFECT_PARAMS).
BuiltinParam(u8),
```

`ParamId::DelayTime` / `DelayFeedback` / `DelayMix` 三个旧变体**保留**，仅服务
`track_delay` 固定三元组（老路径），在 `DelayProcessor::set_param` 里转发到同一个
槽位。三元组本身（`chain_processor`）在 E 系列结束前不动，之后单独做退役（参照
`lane::show` 的退役方式），避免这次改造牵扯轨道装配。

## 4. 决策

| # | 决策 | 结论 | 理由 |
| :-- | :-- | :-- | :-- |
| D1 | 种类判别 | `NodeDoc::Effect` 的载荷改为**内部 tag 枚举** `params: EffectParams`（`#[serde(tag = "kind", rename_all = "snake_case")]`），枚举变体即种类 | 单一真源：种类和参数不可能不一致；RON 里 `params: (kind: "compressor", ...)` 自解释 |
| D2 | 默认值 | `impl Default for EffectParams = Delay(DelayParams::default())` | 老工程（无 `params` 字段）自动落到 Delay，行为不变 |
| D3 | 文件版本 | `PROJECT_VERSION: 7 → 8`，新增 `migrate_v7_to_v8`：把旧 `Effect.delay` 搬进 `params` | 属于"字段退役 + 被替代"，按 `file.rs` 头部政策走破坏性迁移链，不用"两个字段并存"的临时态 |
| D4 | 迁移桥 | `NodeDoc::Effect` 保留 `#[serde(default, skip_serializing_if = "Option::is_none")] legacy_delay: Option<DelayParams>`，只在读旧文件时有值，迁移后清空 | 让 v7 文件能解析（未知字段会被 serde 丢弃）；新文件永不写该字段，运行时无歧义 |
| D5 | 参数寻址 | 引擎侧 `ParamId::BuiltinParam(u8)`；**槽位号 = descriptor 数组下标**，由效果器模块内的 `const SLOT_*` 常量命名 | 加入/调整参数不改引擎分派；UI 与 DSP 共用同一份下标 |
| D6 | 描述表 | 每效果一张 `static DESCRIPTOR: EffectDescriptor`（name / params / stereo class），`EffectDescriptor::params: &'static [ParamSpec]`，`MAX_EFFECT_PARAMS = 12` | 表驱动 UI：滑条、inline 行、自动化菜单、参数推送循环全部遍历它 |
| D7 | 参数容器 | 运行时 `EffectParamBank`（定长 `[Smoothed; MAX_EFFECT_PARAMS]` + `&'static [ParamSpec]`），`set_param` 只需 `bank.set(slot, v)` 一次实现 | 每个效果器不再各写一遍 match；槽位越界静默忽略（RT 安全） |
| D8 | **平滑统一** | 平滑是 **processor 内部职责**，`Processor` trait 不变；所有内建效果器（含 `GainProcessor` / `ChannelStripProcessor` 的 gain/pan）参数一律走 `Smoothed` | 图侧只做块边界投递，不为平滑加协议；顺手修掉推子的阶梯噪声 |
| D9 | 平滑器 | 一阶低通（one-pole）`current += (target − current) · coeff`，`coeff = 1 − e^(−1/(τ·sr))`，`τ = ms/1000`；`ms = 0` 时 `coeff = 1`（吸附） | CPU 最省、无过冲；参数变更只需算一次系数（构造期与 `set_param` 时），**不在逐样本热路径算 `exp`** |
| D10 | 平滑域 | `Curve::Linear` 在线性域平滑；`Curve::Log` 在 `ln` 域平滑（`exp(ln_target)` 消费），频率/时间类参数听感等比例 | 20 Hz→40 Hz 与 10 kHz→20 kHz 都应是"一个八度"，线性域会前慢后快 |
| D11 | 消费速率 | `Rate::Sample`（增益/混合/反馈等逐样本可用）与 `Rate::Block`（EQ 系数、压缩阈值曲线、混响衰减等每块取一次）。块率参数造成 ≤1 块（128 帧 ≈ 2.7 ms）阶梯，配 ≥10 ms 平滑后不可闻 | 逐样本重算双二阶/指数衰减代价过高；这是 DAW 通行做法 |
| D12 | 启动语义 | `Smoothed` 带 `started` 标记：**首次 `process` 之前**的 `set_param` 直接吸附，之后才走斜坡 | 保住现有测试（`set_param` 后立刻断言输出）与离线渲染（构造期即文档值）；否则每个效果器首块都会淡入 |
| D13 | 立体声联动 | descriptor 标 `StereoClass::{Mono, Linkable { default }}`；`Linkable` 的效果器参数结构体带 `stereo_link: bool`（serde default = true），并有 `STEREO_LINK` 标记的槽位（`ms = 0` 吸附） | 联动开关走参数路径而非重建节点：切开时要保住压缩包络/混响尾音 |
| D14 | 联动数学 | 联动时检测器取 `max(|L|, |R|)`，两侧共用同一增益衰减；非联动时逐声道独立检测。**两侧检测器状态永远预分配**（构造期），切换只在块边界改选择位 | 切换零分配零重建；`max` 而非功率和，保持峰值检测语义可断言 |
| D15 | 元数据真源 | 文档字段 `stereo_link` 是持久化真源；UI 编辑沿用 delay 滑条的既有模式（改文档 + 下发参数），引擎构造期以文档值初始化 | 与 `send_node_param` 现有双写模式一致，undo 一步 |
| D16 | 延迟补偿口子 | `Processor` 新增默认方法 `fn latency_samples(&self) -> usize { 0 }`，v1 全部返回 0 | 为后续过采样饱和/限幅器预留，避免届时改 trait 全仓编译 |
| D17 | 不做节点化 | **Echo 不新建节点**（现有 Delay 即回声，升级为 ping-pong + tempo-sync 模式）；**Gain 不新建节点**（通道条已是增益级，缺的只是链路中 trim） | 两个都能表意会造成用户困惑与重复维护 |

## 5. 数据结构

```rust
// crates/glitcherz-engine/src/realtime/effect/mod.rs

pub const MAX_EFFECT_PARAMS: usize = 12;

/// One parameter's static metadata; **slot index = position in the slice**.
pub struct ParamSpec {
    pub name: &'static str,      // English UI label ("threshold", "mix")
    pub unit: Unit,              // Db | Hz | Seconds | Ms | Ratio | Percent | Pan | Bool
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub curve: Curve,            // Linear | Log (UI axis AND smoothing domain)
    pub smoothing_ms: f32,       // 0.0 = snap (discrete params)
    pub rate: Rate,              // Sample | Block
    pub flags: ParamFlags,       // AUTOMATABLE | STEREO_LINK | INLINE
}

pub struct EffectDescriptor {
    pub kind: BuiltinEffectKind,          // must match the model enum
    pub name: &'static str,               // "Compressor"
    pub params: &'static [ParamSpec],
    pub stereo: StereoClass,
}

/// Per-sample (or per-block) one-pole smoother. RT-safe, no allocation.
pub struct Smoothed {
    current: f32,
    target: f32,
    coeff: f32,
    log: bool,
    started: bool,
}

impl Smoothed {
    pub fn new(spec: &ParamSpec, sample_rate: f32) -> Self;  // snaps to spec.default
    pub fn set(&mut self, value: f32);        // clamps to spec range; snaps when !started
    pub fn tick(&mut self) -> f32;            // advances one sample, marks started
    pub fn advance(&mut self, frames: usize) -> f32; // block-rate variant
    pub fn value(&self) -> f32;               // current, no advance
    pub fn snap(&mut self, value: f32);       // current = target = value
}

/// Fixed-capacity runtime parameter bank.
pub struct EffectParamBank {
    slots: [Smoothed; MAX_EFFECT_PARAMS],
    specs: &'static [ParamSpec],
}
```

模型侧：

```rust
// crates/glitcherz-model/src/lib.rs

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EffectParams {
    Delay { time_s: f32, feedback: f32, mix: f32 },
    Eq(Box<EqParams>),                 // 变体肥大者走 Box，避免 NodeDoc 膨胀
    Compressor(Box<CompressorParams>),
    Reverb(Box<ReverbParams>),
    Chorus(ChorusParams),
    Flanger(FlangerParams),
}

// `#[derive(Default)]` 不支持 struct 变体，手写（D2）。
impl Default for EffectParams {
    fn default() -> Self {
        Self::Delay {
            time_s: 0.3,
            feedback: 0.35,
            mix: 0.0,
        }
    }
}
```

> `Box` 与否由 `size_of` 决定，不是 spec 契约：`#[serde(tag)]` 的内部 tag 对
> newtype 变体要求内层序列化为 map/struct，`Box<T>` 透明，序列化形状不受影响。
> `Eq(Box<EqParams>)` 这样的 newtype 变体在 RON 里展开为
> `(kind: "eq", low_gain: 0.0, …)`。

**引擎侧不依赖 model**（`glitcherz-engine` 的依赖里没有 `glitcherz-model`），所以种类
判别在引擎里另有一个 `BuiltinEffectKind` 枚举，只有名字和变体顺序与 `EffectParams` 的
tag 对应；两者之间的映射写在 UI 的 `install_node`（照 `rt_curve_interp` 的既有转换点），
不做双向 `From` blanket impl。**descriptor 表归引擎所有**，`glitcherz-ui` 已经依赖
engine，直接读同一张表生成滑条——不复制第二份参数规格。

## 6. 统一平滑规则

| 参数类 | 典型参数 | 曲线 | 平滑 | 速率 |
| :-- | :-- | :-- | :-- | :-- |
| 增益类 | 推子、makeup、low/high gain、反馈、mix | Linear | 10 ms | Sample |
| 声像/宽度 | pan、width | Linear | 15 ms | Sample |
| 频率类 | EQ 频点、低切、阻尼截止 | Log | 20 ms | Block |
| 时间类 | delay time、pre-delay、attack/release | Log | 20 ms | Block |
| 比率类 | ratio、mid Q | Log | 20 ms | Block |
| 离散开关 | bypass、filter mode、stereo link、ping-pong | —— | 0（吸附） | Block |

规则：

1. **不许有"不平滑的连续参数"**。新增参数忘记填 `smoothing_ms` 时，descriptor 一致性测试直接失败（见 §11）。
2. `ms = 0` 只允许用于**真正离散**的参数（枚举/开关），它们不参与插值，块边界直接生效。
3. 平滑器在**构造期按文档值吸附**（D12），所以新建节点、打开工程、离线渲染都不会有起播淡入。
4. `mix = 0` 的采样级直通不变量保留：平滑稳态下 `mix` 精确等于 0，`dry + (wet − dry)·mix` 精确等于 `dry`。
   **不做**"mix 为 0 就跳过湿路径"的优化——跳过会让混响/延迟尾音冻结再突现，CPU 实测不够再议。

## 7. 立体声联动

```rust
pub enum StereoClass {
    /// One parameter set drives both channels identically (EQ / delay /
    /// reverb / modulation).
    Mono,
    /// Detector-based effect: per-channel state exists, but the instance can
    /// lock them together.
    Linkable { default_link: bool },
}
```

| 效果器 | StereoClass | 说明 |
| :-- | :-- | :-- |
| EQ-3 | `Mono` | 单套参数驱动两声道；v1 不做 L/R 独立均衡 |
| Compressor | `Linkable { default_link: true }` | 联动：`max(|L|,|R|)` → 同一 GR；独立：逐声道检测（适合双单声道素材） |
| Reverb | `Mono` | 参数单套；`width` 是立体声展开度，不是联动开关 |
| Chorus / Flanger | `Mono` | 参数单套；LFO 左右相位差由 `width` 控制 |

联动切换的瞬态：从独立切回联动时，两声道 GR 会在 release 时间内收敛到一起（不做交叉淡化）。
这是主流 DAW 的通行行为，写进 §13「已知取舍」。

## 8. 效果器名册

排序依据：复用度 → 工作量 → 价值。槽位号即数组下标。

### E2a EQ-3（`eq`）

低架 + 峰值 + 高架 + 低切。复用 `realtime/svf.rs` 的 TPT 数学：架式滤波器用一阶
TPT 高低架，峰值用 SVF 带通/峰值输出（`k` 由 Q 反解）。

| # | 参数 | 范围 | 单位 | 曲线 | 平滑 | 速率 |
| :-- | :-- | :-- | :-- | :-- | :-- | :-- |
| 0 | `low_gain` | −18..+18 | dB | Linear | 10 ms | Sample |
| 1 | `low_freq` | 30..500 | Hz | Log | 20 ms | Block |
| 2 | `mid_gain` | −18..+18 | dB | Linear | 10 ms | Sample |
| 3 | `mid_freq` | 200..5000 | Hz | Log | 20 ms | Block |
| 4 | `mid_q` | 0.3..8 | ratio | Log | 20 ms | Block |
| 5 | `high_gain` | −18..+18 | dB | Linear | 10 ms | Sample |
| 6 | `high_freq` | 1500..16000 | Hz | Log | 20 ms | Block |
| 7 | `low_cut` | 20..500 | Hz | Log | 20 ms | Block |

`low_cut = 20 Hz` 视为关闭（不做单独开关，少一个状态）。Stereo：`Mono`。

### E2b Compressor（`compressor`）

前馈、峰值检测、dB 域软拐点。增益计算：`over = x_db − threshold`，拐点内二次插值，
`ratio` 之外 1:1 直通；attack/release 是对 **GR（dB）** 的一阶平滑。

| # | 参数 | 范围 | 单位 | 曲线 | 平滑 | 速率 |
| :-- | :-- | :-- | :-- | :-- | :-- | :-- |
| 0 | `threshold` | −60..0 | dB | Linear | 15 ms | Block |
| 1 | `ratio` | 1..20 | ratio | Log | 20 ms | Block |
| 2 | `attack` | 0.1..200 | ms | Log | 20 ms | Block |
| 3 | `release` | 5..2000 | ms | Log | 20 ms | Block |
| 4 | `knee` | 0..24 | dB | Linear | 15 ms | Block |
| 5 | `makeup` | 0..24 | dB | Linear | 10 ms | Sample |
| 6 | `mix` | 0..1 | % | Linear | 10 ms | Sample |
| 7 | `stereo_link` | 0/1 | bool | —— | 0（吸附） | Block |

GR 表（E2b 收尾项）：`Arc<EffectMeters> { gr_db: AtomicF32, out_peak_l/r: AtomicF32 }`，
构造期由节点持有、UI 按文档 node_id 查表读取；每块写一次，沿用 `AtomicF32` 的
`Relaxed` 语义。**不做**动态注册表——meter 句柄随节点生命周期走。

### E3 Reverb（`reverb`）

**FDN-8**（8 条延迟线 + Hadamard 反馈矩阵 + 每线一阶阻尼 + 低切），不用 Freeverb：
8 comb 的金属尾是公认弱点，而这条产品线要对标 LMMS/Ableton。延迟线长度取
20–70 ms 的互质样本数（按采样率缩放），反馈增益由 RT60 反解
`g_i = 10^(−3·len_i/(decay·sr))`，因此**不需要长缓冲区**：8 × ≈70 ms × 立体声
≈ 220 KB，加 pre-delay 200 ms 两级 ≈ 155 KB，合计 ≈ 375 KB，构造期一次分配。

| # | 参数 | 范围 | 单位 | 曲线 | 平滑 | 速率 |
| :-- | :-- | :-- | :-- | :-- | :-- | :-- |
| 0 | `pre_delay` | 0..200 | ms | Linear | 20 ms | Block |
| 1 | `decay` | 0.2..12 | s | Log | 20 ms | Block |
| 2 | `damping` | 0..1 | % | Linear | 20 ms | Block |
| 3 | `low_cut` | 20..500 | Hz | Log | 20 ms | Block |
| 4 | `width` | 0..1 | % | Linear | 15 ms | Sample |
| 5 | `mix` | 0..1 | % | Linear | 10 ms | Sample |

### E4 ModDelay 核心 → Chorus / Flanger

共享一个核心：≤4 个读抽头、分数延迟（**线性插值**，与 `DelayProcessor` 的整数抽头
不同，调制效果必须有）、正弦 LFO、L/R 相位差由 `width` 决定。

| Chorus `chorus` | 范围 | 曲线 | 平滑 | 速率 |
| :-- | :-- | :-- | :-- | :-- |
| `rate` | 0.05..8 Hz | Log | 20 ms | Block |
| `depth` | 0..1 | Linear | 10 ms | Sample |
| `width` | 0..1 | Linear | 15 ms | Sample |
| `mix` | 0..1 | Linear | 10 ms | Sample |

| Flanger `flanger` | 范围 | 曲线 | 平滑 | 速率 |
| :-- | :-- | :-- | :-- | :-- |
| `rate` | 0.02..5 Hz | Log | 20 ms | Block |
| `depth` | 0..1 | Linear | 10 ms | Sample |
| `feedback` | −0.95..0.95 | Linear | 10 ms | Sample |
| `width` | 0..1 | Linear | 15 ms | Sample |
| `mix` | 0..1 | Linear | 10 ms | Sample |

Chorus 中心延迟 12–30 ms 双抽头、无反馈；Flanger 中心延迟 0.5–5 ms、单抽头 + 反馈。
`feedback` 上限 ±0.95 而非 ±1：环内还有插值读数，留 5 % 余量防自激发散。

### 后续候选（未立项）

Saturation（需 2×/4× 过采样，裸 `tanh` 会糊）、Bitcrusher、Phaser（全通链）、
Master Limiter（前视，要吃 D16 的 `latency_samples`，并需要 PDC）。

## 9. 数据流

```
添加：patch 右键 Add ▸ Effect ▸ Compressor
      → NodeSource::Builtin(BuiltinEffectKind::Compressor)
      → NodeDoc::Effect { params: EffectParams::Compressor(default) }
      → install_node → graph 构造 CompressorProcessor::new(sr)
      → 构造期按文档值 snap 全部平滑器（无起播淡入）

调参：Inspector 滑条 / inline 行 / 自动化曲线
      → 改文档（begin_param_edit/commit_param_edit，一步 undo）
      → SetNodeParam { node_id, ParamId::BuiltinParam(SLOT_THRESHOLD), v }
      → 块边界 slot_set_param → EffectParamBank::set(slot, v)
      → 平滑器斜坡（Sample 速率逐样本推进 / Block 速率每块推进）
      → DSP 消费
```

## 10. 实现拆分

- [x] **E1 框架**（不改声音，全仓回归绿）
  - [x] engine：`realtime/effect/` 新模块（`mod.rs` / `smoothed.rs` / `bank.rs` / `descriptor.rs`），
        `ParamId::BuiltinParam(u8)`，`Processor::latency_samples()` 默认 0
  - [x] `Smoothed` + `EffectParamBank` + descriptor 一致性测试（槽位界内、`min < default < max`、
        `Curve::Log ⇒ min > 0`、连续参数必须有 `smoothing_ms > 0`）
  - [x] `DelayProcessor` 迁到 `EffectParamBank`，旧 `ParamId::Delay*` 转发；`mix = 0` 直通与
        `single_tap_lands_at_configured_time` 等既有测试**一条不改**通过
  - [x] `GainProcessor` / `ChannelStripProcessor` 的 gain/pan 迁到 `Smoothed`（D8）
  - [x] model：`EffectParams` 枚举 + `legacy_delay` 桥 + `migrate_v7_to_v8` + `PROJECT_VERSION = 8`；
        旧工程 fixture（v7 带 delay 载荷）加载后参数不丢（`file.rs::v7_delay_payload_moves_into_effect_params`）
  - [x] UI：`effect_section`（表驱动滑条组）、inline 行由 descriptor 生成、
        `Add ▸ Effect`/`Insert ▸ Effect` 菜单遍历效果器名册；`delay_sliders` / `DELAY_INLINE` / `InlineParam` 手写项退役
- [x] **E2** EQ-3 + Compressor（含 GR 表）
  - [x] E2a `realtime/eq.rs`：一阶 TPT 低/高架 + SVF 峰值（`k = 1/Q`）+ SVF 低切（`k = √2`）；
        平直设置（三增益 0 dB + `low_cut = 20`）是**采样级直通**，直接断言 `out == input`
  - [x] E2b `realtime/compressor.rs`：`compute_gr_db` 纯函数（硬拐点/软拐点，两段在 `t = knee` 处
        数值与斜率都连续）+ GR 域 attack/release 一阶平滑；`stereo_link` 取 `max(|L|,|R|)`
  - [x] `effect/meters.rs`：`EffectMeters { gr_db, peak_l, peak_r }`（`AtomicF32`，每块写一次）；
        `Arc` 由 UI 按 `node_id` 持有，节点生命周期即句柄生命周期
  - [x] model：`EqParams` / `CompressorParams` + `EffectParams::{Eq, Compressor}`（`Box`），
        两端 `BuiltinEffectKind` 各加两变体；无版本号变更（tag 变体是纯增量）
  - [x] UI：`effect::processor()` 按种类构造并**用文档值预置全部槽位**（首次 process 前吸附，无淡入）；
        4 处硬编码 `DelayProcessor` 构造点全部改为 kind-aware；Inspector 加 GR 条
  - [x] `tests/effect_rt_audit.rs`：Eq / Compressor / Delay / meters 的 `set_param`+`process` 零分配
- [ ] **E3** Reverb（FDN-8）
- [ ] **E4** `ModDelay` 核心 → Chorus / Flanger；`DelayProcessor` 升级 ping-pong + tempo-sync
- [ ] **E5（可选）** Saturation（过采样）/ Bitcrusher / Phaser / Limiter + PDC

### 落地时必须核对的 UI 触点

| 位置 | 现在的样子 | E1 后 |
| :-- | :-- | :-- |
| `ui/patch.rs:346, 405` | `NodeSource::BuiltinDelay` | `NodeSource::Builtin(BuiltinEffectKind)` |
| `ui/patch.rs:1830, 2580` | 两处硬编码 "Built-in Delay" 按钮 | 遍历名册生成 |
| `ui/patch.rs:32..105` | `InlineParam` + `DELAY_INLINE` | 由 descriptor 生成 `InlineRow` |
| `ui/lib.rs:6553..6792` | `delay_sliders` 手写三滑条 | `effect_section` 表驱动 |
| `ui/lib.rs:1667` | 参数推送手写三元组 | 遍历 descriptor |
| `ui/lib.rs:2350` | 构造 `NodeDoc::Effect { delay }` | `params: EffectParams::…` |
| `engine/graph.rs:2110` | `chain_processor(kind)` 固定三元组 | 新增 `effect_processor(kind, sr)`，三元组不动 |

## 11. 测试契约

每个新效果器**必须**带（AGENTS.md 铁律 + 本项目增量）：

1. 静音输入 → 静音输出（含极端参数组合）。
2. 满幅输入 → 无 NaN/Inf。
3. `mix = 0` 采样级直通（稳态，逐样本相等）。
4. 参数钳制：越界值落在 `spec.min..spec.max`，非有限值被忽略且不 panic。
5. 平滑阶跃响应：阶跃后 5τ 内收敛到目标 ±1 %，单调无过冲；`ms = 0` 的参数同块生效。
6. `Curve::Log` 参数在 20 Hz 与 20 kHz 两端的平滑步长都是等比例（±1 % 内）。
7. 立体声联动：仅 L 声道满幅时，联动下 R 声道得到**同一 GR**；独立下 R 无 GR。
8. `rt_audit` 零分配（复用 `tests/sample_rt_audit.rs` 的计数分配器；注意 `AudioBuffer`
   构造必须在审计 scope 之外）。
9. 数学内核尽量纯函数化（压缩器的 gain computer、EQ 的系数求解），无设备可测。

框架级测试：descriptor 一致性（§10 E1）、v7 → v8 迁移（旧工程参数不丢）、
槽位越界静默忽略。

## 12. 边界与明确不做

- **不做** Echo 节点、Gain 节点（D17）。
- **不做** 每效果的 sidechain 输入（压缩器外部键控需要图的新端口类型，另立项）。
- **不做** 效果的 L/R 完全独立参数（除联动开关）；EQ 单套参数。
- **不做** 延迟时间的分数采样插值（v1 整数抽头 + 块率平滑）；调制效果内部用插值，两者不冲突。
- **不做** 尾音冻结 / 节点自动休眠（mix 为 0 仍跑湿路径，见 §6）。
- **不做** 自动化的离散参数（`STEREO_LINK` 等 `smoothing_ms = 0` 的槽位不进自动化菜单）。
- **不做** 效果的预设系统（`presets/` 目前是插件状态 blob；内建效果器的预设等实测需求）。
- **不迁移** `track_delay` 三元组（E 系列结束后单独退役）。

## 13. 已知取舍

| 取舍 | 影响 | 何时重议 |
| :-- | :-- | :-- |
| 块率参数有 ≤2.7 ms 阶梯 | EQ 频点、压缩阈值在极快自动化下可能有轻微"台阶" | 若实测可闻 → 改块内线性插值参数 |
| 联动切换不走交叉淡化 | 切换瞬间 GR 在 release 时间内收敛，可能有轻微电平跳变 | 若实测明显 → 加 5 ms 交叉淡化 |
| 平滑对 `set_param` 一律生效（首次除外） | 自动化曲线极陡处会被 10 ms 平滑"抹圆" | 这类"瞬变"应改用 CC/事件而非参数——语义本就不同（见 `control-tracks.md` §3） |

## 14. 实施勘误（E1）

落地过程中与上文规格有出入、或规格未预见的点，按 `sample-player.md` 的惯例记在这里；
**代码为准，本节的判断已回写进实现**。

| # | 位置 | 规格原样 | 实际做法 | 原因 |
| :-- | :-- | :-- | :-- | :-- |
| 1 | `Unit` | 只有 `Ms` | 新增 `Unit::Seconds` 与 `ParamSpec::seconds()`，`Ms` 保留给"存储值就是毫秒"的参数（压缩器 attack/release） | Delay 的 `time_s` 存的是**秒**（0.005..1.0）。硬挂 `Unit::Ms` 会让 250 ms 的抽头显示成 "0.2 ms"。格式化按数量级切换：< 1 s 显示 ms，≥ 1 s 显示 s |
| 2 | `Smoothed::tick` | `current += (target − current) · coeff` | 当 `current + delta·coeff == current` 时直接吸附到 `target` | **f32 精度停滞**：`delta·coeff` 跌到 state 半个 ULP 以下时加法恒等于自身，一阶低通会永久停在目标前 ~1e-5 处。测试 `replace_node_keeps_identity_and_recycles_old` 以 1e-6 容差暴露了它（停在 0.12500358） |
| 3 | `delay_samples` | 未规定取整方式 | `(t · sr).round()` | 对数域平滑把时间过了 `ln`/`exp`，0.05 s × 48 kHz 会算成 2399.99…，截断后少一个样本，`channels_delay_independently` 直接失败 |
| 4 | 既有测试 | 「一条不改」 | Delay / Gain 相关测试全部保持断言不变；**图级**测试（`master_gain_applies_after_summing` 等）加了"先跑若干块让平滑沉降"的循环 | D8 把推子/主增益也纳入平滑后，首块不再等于静态增益值——这是**规格想要的行为变化**，不是回归。改的是等待方式，不是期望值 |
| 5 | `ParamSpec.short` | 未在 §5 列出 | 新增 `short` 字段（默认等于 `name`），`.short("fdbk")` 覆盖 | inline 行标签沿用既有 UI 约定（`time` / `fdbk` / `mix`），同时保持 descriptor 是唯一真源 |
| 6 | `EffectDescriptor.kind` | §5 写了 `pub kind: BuiltinEffectKind` | 未加该字段；种类由 `BuiltinEffectKind::descriptor()` 反查 | 引擎侧 `descriptor` 已经挂在枚举的关联函数上，结构体再存一份会给出两个真源 |

## 15. 实施勘误（E2）

| # | 位置 | 规格原样 | 实际做法 | 原因 |
| :-- | :-- | :-- | :-- | :-- |
| 1 | `ParamFlags` | 只有 `AUTOMATABLE` / `STEREO_LINK` | 新增 `INLINE` + `ParamSpec::inline_row()`，patch 节点的 inline 行只遍历 `inline_slots()` | EQ 有 8 个参数，全塞进节点会把画布节点撑成一列滑条；Inspector 仍然显示全部槽位。Delay 三个参数全部标 `INLINE`，行为不变 |
| 2 | `ParamSpec` | §5 只列了构造器语义，未列 `level` | 新增 `ParamSpec::level()`（dB、Linear、15 ms、Block） | 阈值/拐点是"喂给曲线或系数的 dB 值"，与 `gain()`（逐样本增益级）语义不同；混用会让 threshold 走 10 ms 逐样本平滑 |
| 3 | EQ 低切 | `low_cut = 20 Hz` 视为关闭 | 关闭时**仍然积分**滤波器，只在输出端选择直通 | 若真正跳过，长期关闭后突然打开会从陈旧/零状态起步 → 咔嗒声。恒积分 + 输出选择既无咔嗒又是真旁通（代价约 6 次乘法/样本） |
| 4 | EQ 平直 | 未规定 | 三增益 0 dB 且低切关闭时是**采样级直通**，测试用 `assert_eq!(out, input)` | `A − 1` 恰好为 0，四个级联级都退化为恒等；这条不变量和 delay 的 `mix = 0` 同一性质，用来钉住"没动参数就不该碰声音" |
| 5 | EQ 架式斜率 | 一阶 TPT 高低架 | 保持一阶，但**拐点两倍频程外才到约 3/4 的增益**（12 kHz 处对 3 kHz 拐点的 -12 dB 架式得 -10 dB） | 一阶架式就是 6 dB/oct 过渡，这是物理结果不是 bug。测试按实际响应写死区间；若日后觉得不够"硬"，改二阶并同步改 §8 的说明 |
| 6 | Compressor `mix` | §8 表未给默认值 | 默认 `1.0`（全湿） | 插入式压缩器的常识默认；`mix = 0` 的采样级直通不变量照旧成立 |
| 7 | Compressor GR 平滑 | "attack/release 是对 GR 的一阶平滑" | 用与 `Smoothed` 相同的 `coeff_for()` 系数公式，并带**相同的 f32 停滞保护**（§14 第 2 条） | 两个平滑器必须是同一套时间常数语义，否则"5τ 收敛"在测试与耳朵上对不上 |
| 8 | GR 表 | 未规定表内语义 | `gr_db` 存**本块内最负**的 GR（UI 取反显示为压了多少 dB）；`peak_l/r` 存本块输出峰值 | 峰值保持比"最后一块的瞬时值"更接近硬件表头，也更容易断言 |
| 9 | 立体声联动 | 未规定非联动时的表头 | 联动时只有 `gr[0]` 有意义，表头读它 | 非联动是诊断场景；表头显示两声道中最需要关注的那个，避免加第二个表 |
| 10 | rt_audit | §11 要求零分配 | 单独 `tests/effect_rt_audit.rs`，**合成一个测试函数** | 计数分配器是全局的，测试运行器并行跑多个测试函数时，别人的簿记分配会落进本测试的计数窗口（已实测：并行必挂、`--test-threads=1` 才过）。合成单测试 = 只有一个工作线程，稳定 |


