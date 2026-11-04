# 扩展音频解码：symphonia 接入 Spec

> 状态：X1–X3 已交付（2026-11-04，文档 `a75655e` / 实现 `f1d4feb`）。
> 上游：`sample-player.md`（U5 采样播放器）、`audio-track.md`（U6 音频轨 / 波形）、
> `media.md`（S3b 媒体相对路径与收集）、`AGENTS.md`「实时音频铁律」。
> 决策来源：2026-11-02 外部 DSP crate 选型讨论；`fundsp` 不引依赖（只作算法参考），
> `symphonia` 整体引入补音频格式。

## 1. 问题

目前所有采样入口（采样播放器 instrument / audio clip / 鼓机 slot）只认 WAV：

- `engine/src/sample.rs::SampleData::load` 用 `hound` 解码 PCM int/float WAV，
  产出 `Box<[StereoFrame]>` + `peaks` + 源采样率，控制线程独占；
- 三处文件对话框（`ui/timeline_ops.rs` / `ui/voices.rs` 两处）写死
  `.add_filter("Wave", &["wav"])`；
- 用户拖 flac / mp3 / ogg / m4a 进工程，要么被系统对话框挡住，要么在
  `hound` 里报 `Decode`，且这个错误看不出「格式不支持」。

WAV 路径是稳定的：`SampleData` 的内存布局、`peaks` 计算、`SampleLibrary` 缓存键、
RT 侧的逐样本比率播放，全部依赖它，不值得为了统一解码器而换掉。

## 2. 目标

1. 采样播放器 / 音频轨 / 鼓机 slot 能读 flac、mp3、ogg(Vorbis)、m4a(AAC-LC)、aiff；
2. **WAV 行为逐样本不变**（hound 路径一行不改，现有测试全绿）；
3. 解码仍严格在控制线程，`SampleData` / RT processor / 图协议 / 文档格式零改动；
4. 支持格式清单只有一处真源，UI 过滤器与解码能力不会各自漂移。

## 3. 决策

| # | 决策 | 结论 | 理由 |
| :-- | :-- | :-- | :-- |
| D1 | 依赖 | `symphonia 0.6.1`，`default-features = false`，显式开 `flac / mp3 / mpa / ogg / vorbis / aac / isomp4 / aiff / pcm / wav` | 0.6 重写了 core API（`GenericAudioBufferRef` / `Option<Packet>`），网上 0.5 示例不可照抄；默认 feature 会拉 SIMD→`rustfft` 与全部 metadata reader，一次性解码用不上 |
| D2 | 分路 | 按扩展名分路：`.wav` → hound；其它一律 → symphonia（内容探测 + 扩展名 hint） | WAV 零回归；无扩展名 / 改错扩展名的文件仍由 probe 按魔数识别 |
| D3 | 写路径 | 导出 / 录音的 WAV 写入继续用 `hound`，**不做** symphonia 编码 | symphonia 0.6 是纯解码库，没有编码器 |
| D4 | 统一产物 | 两条分路都返回 `DecodedAudio { frames: Vec<StereoFrame>, sample_rate: u32, channels: u16, decoder: &'static str }`，`SampleData::load` 只负责 `compute_peaks` + 落字段 | 解码器差异收在一个函数内；`SampleData` 字段与访问器不动 |
| D5 | 声道 | 1 声道复制到 L/R；2 声道按交错 L/R 配对；>2 声道 `TooManyChannels` 直接拒绝，**不**丢声道 | 与 hound 路径语义一致；RT 读取永不分支声道数 |
| D6 | 交错 | symphonia 是 planar；用 `GenericAudioBufferRef::copy_to_vec_interleaved::<f32>()` 一次转成交错 f32（自带整数/浮点归一化），再 `chunks_exact(ch)` 组装 `StereoFrame` | 不自己写 8 种采样类型的归一化；该方法**覆盖**目标 Vec，故每包用临时 Vec 再 extend |
| D7 | 采样率 | 取首个解码块的 `spec().rate()`；后续块 rate 不一致直接报错，不静音混用 | `SampleVoiceProcessor` 的播放步进依赖单一源采样率 |
| D8 | 扩展名清单 | `engine::sample::AUDIO_FILE_EXTENSIONS: &[&str]` 为唯一真源，UI 三处过滤器直接引用 | 加格式只改一处；`engine` 本来就是解码能力的所有者 |
| D9 | 错误 | `SampleError::Decode(String)` 统一承载 hound / symphonia / IO 错误；保留 `Empty`、`TooManyChannels(u16)`；新增 `UnsupportedFormat(String)` 表达「probe 认不出 / 没有音频轨」 | 现有 `#[from] hound::Error` 无法容纳第二种错误源；UI 只需 `Display` |
| D10 | 线程 | `SampleData::load` / `SampleLibrary::load` 仍是控制线程独占；新增的解码函数不进 `realtime` 模块 | 解码整文件分配，铁律第 1 条 |
| D11 | 文档与路径 | `PROJECT_VERSION` 不动；`media_paths` / 收集 / 重定位按文件名与大小工作，与扩展名无关，不改 | 文档只存路径字符串；`ui/media.rs` 无格式假设 |
| D12 | 许可证 | symphonia 是 **MPL-2.0**：可依赖、可随二进制分发，但**不得把它的源码拷进本项目 `.rs`**；在 `THIRD-PARTY-NOTICES.md` 登记上游地址与许可证 | 与 `fundsp`（MIT/Apache，可抄算法）恰好相反，分工是「依赖 symphonia、参考 fundsp」 |
| D13 | 真格式 fixture | 不入库二进制 flac/mp3；单测用 hound 现写的 WAV fixture **直接喂 `decode_symphonia`** 来覆盖新分路（probe 按扩展名 hint 识别 RIFF/WAVE） | 仓库零额外素材，且能逐样本对比两条分路；真实压缩格式留手工冒烟 |

## 4. 支持矩阵

| 扩展名 | 容器 / 编码 | feature | 备注 |
| :-- | :-- | :-- | :-- |
| `.wav` | RIFF/WAVE PCM | —（走 hound） | 行为完全不变 |
| `.flac` | FLAC | `flac` | 原生 FLAC 或 Ogg FLAC |
| `.mp3` | MP3 | `mp3` | `mpa` 一并开，覆盖 MP1/MP2 变体 |
| `.ogg` | Ogg / Vorbis | `ogg` + `vorbis` | **不含 Opus**，0.6 无该解码器，不写进清单 |
| `.m4a` | ISO/MP4 / AAC-LC | `isomp4` + `aac` | ALAC 不启用 |
| `.aiff` | AIFF PCM | `aiff` + `pcm` | |

`AUDIO_FILE_EXTENSIONS = ["wav", "flac", "mp3", "ogg", "m4a", "aiff"]`。
`wav` / `pcm` feature 仍然开启：单测要走 symphonia 解码 WAV fixture，
且 exotic WAV（hound 拒绝时）可以回落。

## 5. 数据流

```
picker (Audio file ▸ wav/flac/mp3/ogg/m4a/aiff)
  → 文档只存路径字符串（media_paths / 收集 / 重定位照旧）
  → SampleLibrary::load(path)                    [控制线程]
  → SampleData::load(path)
      ├─ .wav  → decode_wav  (hound，原逻辑)     ┐
      └─ else  → decode_symphonia                ┴→ DecodedAudio
  → compute_peaks(&frames) → SampleData { frames, peaks, sample_rate, source_path }
  → Arc<SampleData> → RT processor / timeline 波形         [音频线程只读]
```

symphonia 分路细节：

```
File → MediaSourceStream → Hint(extension) → get_probe().format(...)
  → default_track(TrackType::Audio)
  → get_codecs().make(&track.codec_params, &DecoderOptions::default())
  → loop next_packet() -> Option<Packet>
      ├─ track_id 不符：跳过
      └─ decoder.decode(&packet) -> GenericAudioBufferRef
           → spec().rate() / spec().channels().count()
           → copy_to_vec_interleaved::<f32>(&mut tmp)
           → chunks_exact(ch) → push StereoFrame
```

## 6. 实现拆分

- [x] **X1 文档 + 依赖**：本 spec、`plan.md` 新节、`AGENTS.md` 索引、
      `Cargo.toml` 依赖、`THIRD-PARTY-NOTICES.md` MPL 条目（文档与依赖分开提交）
- [x] **X2 解码分路**：`engine/src/sample.rs` 拆 `decode_wav` / `decode_symphonia`，
      `SampleError` 按 D9 调整，`AUDIO_FILE_EXTENSIONS` 导出，单测按 §7
- [x] **X3 UI 与用户文档**：三处对话框过滤器读同一常量、Inspector/patch 的
      「.wav」hover 文案改通用措辞、`docs/manual.md` / `docs/project-files.md`
      同步「支持的音频格式」

## 7. 测试契约

1. **零回归**：`sample.rs` 现有 5 个 WAV 测试逐条保持通过（含
   `rejects_more_than_two_channels` / `rejects_missing_file_as_decode_error`）；
   `audio_track` / `drum_kit` / `sample_*` 集成与 rt_audit 全部不改断言。
2. **分路等价**：同一 WAV fixture 分别走 `decode_wav` 与 `decode_symphonia`，
   逐帧 `assert_eq!`（16-bit PCM 两条路径应完全相同）、采样率相同。
3. mono 复制、stereo 配对、尾端奇数样本丢弃（新分路各一条）。
4. >2 声道（多声道 WAV 经 symphonia）→ `TooManyChannels`。
5. 垃圾字节文件 → `UnsupportedFormat` 或 `Decode`，不 panic、不留半成品缓存。
6. 采样率中途变化 → `Decode`，不静音拼接（构造成本高时降级为纯函数断言）。
7. `AUDIO_FILE_EXTENSIONS` 与 UI 过滤器一致（常量直接引用，无需重复测试）。
8. 新增解码路径**不进** rt_audit 范围：它是控制线程行为，
   `sample_rt_audit.rs` 仍只测 `SampleVoiceProcessor`。

## 8. 边界与明确不做

- **不做**编码器：导出 / 录音仍只写 WAV。
- **不做**元数据展示（ID3 / Vorbis comment）、封面、专辑信息。
- **不做**流式解码 / 边播边读：整文件解码进内存，与现有 WAV 语义一致；
  长音频文件的内存占用是已知取舍。
- **不做**采样率重采样：`SampleData` 存源采样率，RT 播放与 audio track 的
  比率换算照旧（`rubato` 未引入）。
- **不做**多轨 / 多曲目选择：`default_track(TrackType::Audio)` 一条。
- **不做** Opus / ALAC / WavPack / 视频容器（Matroska）。
- **不**因本次改动 bump `PROJECT_VERSION`，文档格式无变化。
- **不**改 `ui/media.rs`：收集与重定位只看文件名与大小。

## 9. 已知取舍

| 取舍 | 影响 | 何时重议 |
| :-- | :-- | :-- |
| 整文件解码 | 一首 5 分钟立体声 48 kHz ≈ 115 MB | 用户开始加载长采样 / 出现内存压力 → 改为磁盘流式 + 分块缓存 |
| 0.6 core API 较新 | 上游 0.6 仍可能微调，CI 需锁版本 | 有稳定 0.6.x 后再放宽版本约束 |
| `Decode(String)` 丢掉结构化错误 | UI 只能显示文本，无法按错误种类分流 | 需要「格式不支持时弹重定位对话框」时再拆枚举 |

## 10. 实施勘误（X1–X3）

| # | 规格原样 | 实际做法 | 原因 |
| :-- | :-- | :-- | :-- |
| 1 | §5 写 `get_probe().format(...)` | `get_probe().probe(&hint, mss, FormatOptions, MetadataOptions)` 返回 `Box<dyn FormatReader>` | 0.6 重写了 probe API，0.5 示例里的 `format()` / `ProbeResult` 已不存在 |
| 2 | §5 写 `get_codecs().make(...)` | `make_audio_decoder(&AudioCodecParameters, &AudioDecoderOptions)` | 同上；`Track.codec_params` 是 `Option<CodecParameters>`，需 `.audio()` 取出再 clone，避免 borrow 与 `next_packet()` 冲突 |
| 3 | `Hint` 路径 | `symphonia::core::formats::probe::Hint` | 0.6 没有 `core::probe` 模块 |
| 4 | §7.2 逐帧 `assert_eq!` | 真的逐帧相等，无容差 | 两条路径的 i16→f32 都除以 32768（`symphonia-core/audio/conv.rs`），算出来是同一个 f32 |
| 5 | D6 每包一个临时 Vec | 复用单个 `scratch: Vec<f32>` | `copy_to_vec_interleaved` 内部 `dst.resize(...)` 覆盖目标，不 append；复用后每包零额外分配 |
| 6 | 错误分流 | probe / 建解码器失败 → `UnsupportedFormat`，解包失败 → `Decode`，单包 decode error 跳过继续 | 坏包可恢复、坏格式不可恢复；与 `symphonia-play` 的处理一致 |
| 7 | 采样率变更 | 首块记录 rate，后续块不同直接 `Decode`（channel 数同理） | RT 播放步进只认单一源采样率，静默拼接会产生变速 |
