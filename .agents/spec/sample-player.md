# Spec: 采样播放器乐器（Sample Player Instrument）

> 状态：已收官（2026-10-24，SP1–SP5 全部落地）。对应 `plan.md`「U5 采样播放器 instrument」。
> 前置：U4 已交付；U8/E 系列后 Inspector 已收敛，添加节点统一走 patch
> `Add ▸ Instrument`（spec `plugin-browser.md`）。

## 1. 问题

声源现有三类：内置合成器、插件（CLAP / VST3）、SoundFont。缺"把一个
wav 文件当乐器弹"的能力——鼓组单件、人声 chop、合成器导出的一句
riff，都是 LMMS / Ableton 的 Simpler / DrumSynth 一类常态用法。U5 补
采样播放器 instrument；U6 的 audio track（时间轴铺 wav）复用本 spec
的解码与缓存基建。

## 2. 选型

**`hound`（wav 解码）**：纯 Rust、零 unsafe、MIT/Unlicense 双许可、
只做 wav（读写 PCM 8/16/24/32-bit int 与 32/64-bit float、单声道 /
立体声）。wav-only 恰好是 v1 范围；全格式解码的 symphonia 依赖面大，
不引入。解码只发生在控制线程（`SampleData` 构造期），RT 路径不 touch
hound。

## 3. 核心决策

| # | 决策 | 结论 | 理由 |
| :-- | :-- | :-- | :-- |
| D1 | 播放语义 | `play_mode` 文档字段：**Gate**（note-off 进 release）/ **OneShot**（忽略 pitch-matched off，走完样本），默认 Gate | 用户确认（2026-10-24）：鼓/镲用 OneShot、Pad/Bass 用 Gate，一个声源覆盖两类场景 |
| D2 | release 处理 | **固定 ~10ms 线性 fade**（按采样率折算帧数），无包络控件 | 用户确认：样本即音色（SF2 思路），仅防 note-off 硬切咔嗒 |
| D3 | 音高映射 | `play_rate = 2^((note − root)/12) × wav_sr / engine_sr`，**线性插值**逐帧读取 | root note 相对重采样；采样率补偿与音高比率统一为单一步进系数，不单独做重采样预处理；线性插值防降调丢样本 |
| D4 | voice 池 | 自写定长池 `SAMPLE_POLYPHONY = 16`，构造期预分配，结束 swap 回收，池满偷 **age 最大**（最老）者；voice 触发时定格 rate（参数变更只影响后续 note-on） | 参考 `VoiceCollection` 模式（vendored rustysynth-ext）；rate 回溯改正在采样器界非公认行为 |
| D5 | OneShot 与 blanket off | OneShot 只忽略 **pitch-matched** off；`Off { note: 0 }`（未按住 note 0）= 全释放，OneShot 也停 | 与 SF2 `held` 位图同款语义：Stop/Panic/切轨必须能停采样；"触发走完"只针对松键 |
| D6 | 代码落点 | engine 两模块镜像 sf2：`sample`（非实时：解码 / 缓存）+ `realtime/sample.rs`（RT processor） | 文件 IO 与解码严禁进音频回调 |
| D7 | 缓存 | `SampleLibrary`：`HashMap<PathBuf, Arc<SampleData>>` 会话级多样本缓存，path 命中复用，v1 无上限 | 与 sf2 单库不同：采样器一轨一样本，天然多文件；重装/undo 不重读大文件 |
| D8 | 文档落点 | `VoiceSpec::Sample { path, root_note, play_mode, gain }`，serde default 加法，PROJECT_VERSION 不动 | 同 SF2 D5/D6 先例；root 默认 60（C4，与引擎 `note 69 = A4` 注释同惯例），gain 默认 1.0 |
| D9 | 文件丢失 | 重开工程 wav 缺失 → 回落内置 synth + log，文档字段保留（重试机会） | 同 plugin-state / SF2 D8 先例，宁缺勿炸 |
| D10 | 参数 | `ParamId::{SampleRootNote, SampleGain, SamplePlayMode}`（0 = Gate，≥ 0.5 = OneShot），`set_node_param` 非重建下发；velocity → voice 增益线性 `vel/127` | 镜像 `Sf2BankSelect` 家族；Inspector 与 patch inline（G5 注册表 `SAMPLE_INLINE` = root + gain）双入口同源 |
| D11 | 测试 | 单测自生成 wav fixture（hound `WavWriter` 写毫秒级正弦/满幅），无外部文件、常规 CI 可跑；静音/NaN 两组铁律测试 + 分配计数审计（同 S1 流程，自证零分配） | AGENTS.md 强制；fixture 生成比 SF2 手建 RIFF 字节流简单 |

## 4. 数据流

```
加载：Add ▸ Instrument ▸ Sample… → rfd 选 .wav（控制线程）
      → sample::load（hound 解码 → SampleData { frames: Box<[[f32;2]>, sr }）
      → SampleLibrary 缓存 Arc<SampleData> → SampleVoiceProcessor 构造期持 Arc

播放：轨声源 = VoiceSpec::Sample → 图节点（MIDI 入边）
      → note_on：池取 voice，rate = 2^((note−root)/12)·wav_sr/engine_sr，
        gain = velocity·文档 gain → 线性插值逐帧读 → 求和进 buffer
      → Gate off / 偷音 / 采样越界 → 10ms fade → swap 回收
```

## 5. 实现拆分

- [x] **SP1** engine `sample` 模块（非实时）：`SampleData`（解码为 f32
  立体声，单声道复制双声道）+ `SampleLibrary` 缓存 + thiserror 错误
  （`SampleError::{Decode, Empty, TooManyChannels}`——hound 把所有 IO
  失败包进自己的 Error，单一 `Decode` 变体即可）；fixture 生成助手
  ✅ 2026-10-24（交错流整块 `chunks_exact(2)` 组帧，勿逐样本 push）
- [x] **SP2** engine `realtime/sample.rs`：`SampleVoiceProcessor`
  （池 16 + held 位图 + 线性插值 + fade + 偷 age 最大者）+ 三个
  `ParamId` + 铁律测试 + 分配计数审计
  ✅ 2026-10-24（`tests/sample_voice.rs` 16 项 +
  `tests/sample_rt_audit.rs` 4 项零分配实证）
- [x] **SP3** model：`VoiceSpec::Sample` + `SamplePlayMode` + RON 往返
  （含 partial default：60/Gate/1.0）✅ 2026-10-24
- [x] **SP4** UI：`NodeSource::Sample { path }` + Add 菜单项 + install_node
  双路径（D9 回落）+ Inspector Sample 分支（文件名 / root DragValue /
  gain 滑条 / play mode 下拉，undoable + 即时下发）+ patch 节点标签
  （文件 stem）+ `SAMPLE_INLINE`（root + gain）+ 会话 `sample_library`
  ✅ 2026-10-24（`sample_section` 沿用 `adsr_sliders` 手势合并模式）
- [x] **SP5** 端到端验收：wav fixture → 触发 → 输出数学验证（音高比率、
  release 长度、OneShot 全长）；全仓 fmt/clippy/test 绿
  ✅ 2026-10-24（543 项全绿 + 5 秒冒烟）

### 实施勘误（写代码时推翻的两处预想）

- **审计 scope 边界**：`AudioBuffer::new`（构造期分配）必须建在
  `AuditScope::begin()` **之前**，否则 3 次构造分配被计入（假阳性）。
- **fixture 波形选择**：ramp 样本逐块上升，块间峰值不可比——增益 /
  衰减 / 复音堆叠类断言必须用**恒幅** fixture；"输出 = 源 × rate"
  类数学断言才用 ramp（整数游标处插值 frac=0 精确）。

## 6. 边界与明确不做

- **不做** loop 循环点（wav smpl/cue chunk 解析留给实测需求；长 pad
  靠 Gate + 样本长度）。
- **不做** 采样内切片（slicing / beat chop）、Round-Robin、交叉淡化。
- **不做** 音频格式扩展（mp3/flac/ogg → 若有需求走 symphonia 另立项）。
- **不做** 高质量重采样（sinc 多相滤波）——线性插值 v1 够用，实测劣化
  再议（届时可复用 D3 的单一系数接口替换内核）。
- **不做** 24-bit 之外的抖动 / 采样率预处理（读取期比率步进已补偿）。- **不做** 采样参数的自动化目标（`AutoParam` 无 Sample 变体；与 SoundFont
  的 Cutoff 选项同等现状——菜单出现但不响应）。实测有需求再立项。