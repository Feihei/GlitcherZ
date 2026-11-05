# 鼓机乐器（W1）Spec

> 状态：定稿（2026-10-31）。上游：`sample-player.md`（U5 采样播放器：解码 + `SampleLibrary` 缓存 + 插值读取）。

## 1. 背景

用户提出：鼓机乐器——一个 instrument 节点内加载多个短采样，按 MIDI 音高触发对应音色。

现状（2026-10-31）：

- `SampleVoiceProcessor` 是**单文件采样器**：一个节点只持一份 `Arc<SampleData>`、一个 root note（`realtime/sample.rs`），没有 note→sample 映射、没有 per-slot 增益/声像、没有 choke。
- `SampleLibrary` 已是 `path → Arc<SampleData>` 的会话缓存（命中复用、无淘汰），可直接当鼓机音色池。
- `render_voice` 的线性插值读取与 16 槽 swap-compact voice 池可复用（鼓机独立实现同类逻辑，不改造采样器）。

## 2. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| K1 | 新声源变体 | `VoiceSpec::DrumKit { slots: Vec<DrumSlot>, gain: f32 }`，加法变体 + serde default，**`PROJECT_VERSION` 不动**；空 `slots` = 静音（沿用 U13「空节点静音」，不回落内置合成器）。 |
| K2 | 槽位模型 | `DrumSlot { note: u8, path: String, gain: f32, pan: f32, tune_semitones: f32, choke: Option<u8> }`。`note` 是触发键（默认 36 = GM kick）；`gain` 0..2 默认 1；`pan` -1..1；`tune` ±24 半音；`choke` 同组互斥。 |
| K3 | 槽位上限与唯一性 | 文档上限 **32 槽**；`note` 必须唯一（重复由 `validate` 拒绝，避免"哪一份生效"的歧义）。 |
| K4 | RT 结构 | `realtime/drum_kit.rs`：构造期把槽位摊成定容表 `[Option<DrumSlotRuntime>; 128]`（note 直索引，O(1)）；voice 池 **24** 槽、构造期预分配、swap-compact 回收；立体声 scratch 预分配。热路径零分配、零锁。 |
| K5 | 1× 原生速率 | 默认不重采样；`tune_semitones` 是唯一的音高系数：`rate = 2^(tune/12) · wav_rate / engine_rate`。鼓机**不**做 note→音高移调（键位决定触发哪个采样，不决定音高）。 |
| K6 | 全 OneShot | 鼓机通常一击到底：音高匹配的 note-off 忽略，只有 transport blanket（Stop / Panic）会让所有 voice 进入 10 ms fade。 |
| K7 | choke group | 新音触发时，同 `choke` 组内所有正在发声的 voice 进入 10 ms fade 后由新音接管（开镲闭镲类）。 |
| K8 | 参数下发 | `ParamId::{DrumGain, DrumSlotGain(note), DrumSlotPan(note), DrumSlotTune(note), DrumSlotChoke(note)}`；`DrumSlotTune` 只影响后续触发的 voice（触发时冻结 rate），其余实时作用。非有限值丢弃。 |
| K9 | 音色池 | UI 侧复用会话 `sample_library`（U5 的 `SampleLibrary`）；同一路径的槽位共享同一 `Arc<SampleData>`。单槽解码失败只让该槽静默并 `set_error`，不拖垮整个节点。 |
| K10 | 多选加载的 note 分配 | 纯函数 `assign_drum_notes(taken, count)`：从 **36** 起顺序找空闲键（跳过已占用，到 127 回绕），保证一次多选立即得到可用映射；文件名里的数字**不**参与推断（文档表格可改）。 |
| K11 | UI 版图 | patch 节点：`Load Samples…` 按钮 + `N slots` 标签（与 SF2 的 `Load File…` 同族）；Inspector：`drum_section` 表格（key / file stem / gain / pan / tune / choke + 移除）。节点内不铺 32 行表格（空间不够，缩放后不可点）。 |
| K12 | 一步 undo | 加槽/删槽/改参数都走既有 `begin_edit`/`commit_param_edit` 守卫；`Load Samples…` 整次加载 = 一步 undo。 |
| K13 | 键位可读可改（2026-11-05） | 触发键在表格里显示为 **`C2 · 36`**（`note_name` 唯一实现，C4 = 60 科学音高），可拖动改键；落到已占用键时用 `nearest_free_note`（`ops.rs`，等距上优先）吸附到最近空闲键，不覆盖邻居。改键走 `move_drum_slot` + `rebuild_drum_kit`（RT 表按 key 索引，必须换 processor；`replace_node` 保留 node id / 连线 / MIDI 路由），**一次拖放 = 一次重建 = 一步 undo**（拖动中不提交，松手才落）。手册/术语沿用 K11 的表格语义。 |
| K14 | 卷帘键名列（2026-11-05） | `piano_roll::show` 左侧固定 `KEYS_WIDTH = 64 px` 键盘列：白/黑键块 + 每个八度的 C 行标 `C2` 等键名。卷帘的 `rect` 收窄为音符区，所有 tick/命中/框选仍由 `geom` 派生；velocity lane 与 curve lane 用 `allocate_roll_strip` 同样右移，保持时间轴对齐。键名列下方（lane 高度段）填深色 gutter，不读作空洞。 |

## 3. 引擎接口

```rust
pub struct DrumSlotSpec {
    pub note: u8,
    pub data: Arc<SampleData>,
    pub gain: f32,
    pub pan: f32,
    pub tune_semitones: f32,
    pub choke: Option<u8>,
}

impl DrumKitProcessor {
    /// Control thread only (table + pool + scratch allocation).
    pub fn new(specs: &[DrumSlotSpec], sample_rate: f32) -> Self;
}
```

`process` 生成器语义（覆盖输出缓冲），`note_event`/`set_param` 全部 RT-safe。

## 4. 验收

- engine：静音输入 → 静音、满幅 → 无 NaN/Inf、note 映射与未映射键、pan 左右声道、choke 互斥、blanket 停音、24 复音上限、gain/tune 参数。
- RT 审计：`drum_kit_rt_audit`（复用 `sample_rt_audit` 的计数分配器）触发/压测/控制面零分配。
- model：`DrumSlot` / `VoiceSpec::DrumKit` RON 往返；旧文档读出空槽位；validate（上限 32、note 唯一）。
- model：`note_name` 八度基准（60 → `C4`、36 → `C2`）、`nearest_free_note`（空闲直取、等距上优先、0/127 边界、全满 → None）。
- UI：`assign_drum_notes` 分配纯函数（跳过占用、回绕、上限）；`move_drum_slot` 改键后槽位数与顺序不变、`validate` 通过。
- UI：卷帘 `roll_state_machine` 集成测试按 `KEYS_WIDTH` 校准（键名列占位后音符区起点右移、fit 比例按剩余宽度算）。
- 全仓 `fmt` / `clippy -D warnings` / `test` 绿。

## 5. 明确不做

- per-slot 包络 / 滤波（先靠素材本身与整体 gain）。
- 每槽 Gate 模式（K6 全 OneShot）。
- 鼓机专属步进编辑器（鼓机吃普通 MIDI clip + W4/W5 生成工具）。
- 槽位参数进自动化词汇表（`AutoParam::Node`），待实测需求。