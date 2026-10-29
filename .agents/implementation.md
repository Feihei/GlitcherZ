# 实现细节 — GlitcherZ

> 记录已落地模块的实现要点、协议细节与版本 gotchas。新增模块落地后在此追加条目。

## 已落地

### mixer 通道条上画布（U14，spec `node-graph.md` D22，2026-10-28）

- **一行一个通道，压在端口上**：绘制段在 inline 参数循环之后（后注册 → 指针优先于节点拖拽），位置**由 `in_ports_screen` 的端口坐标推导**（`channel_strip_rect(port_pos, box, zoom)`），而不是另起一套均分——端口位置一改，条自动跟随，不会出现"推子飘在两根线中间"。条高 22 < `PORT_SPACING` 30，所以 2 通道（`port_area` 只有 48 图空间、step 24）时上下仍各留 1 px。
- **左边界必须是 22，不是端口半径 6**：拉线的判定半径是 `PORT_HIT_RADIUS`（18），条从端口圆点右侧 22 起才开始，否则按 fader 会变成拉线。右边界 = 盒右 − `INLINE_INSET`(8)；150 宽的盒子里 fader 76 + M/S 各 18 刚好，**所以没有 pan**。
- **`egui::Button` 又一次不可用**：M/S chip 只有 `22 × zoom` 高，原生按钮 18 pt 下限会顶出条外——与 D15 的 `action_button` 同一病因，故 `channel_toggle` 自绘（fill + stroke + 居中文字，字号 `min(9z, 高×0.8)`），命中靠 `allocate_rect(Sense::click())`。激活色：mute 红 (190,85,70)、solo 黄 (205,175,65)，文字用深灰（20）保证在彩底上可读。
- **fader 命中区取整条高度**：rail 只有 3 pt 高，直接 `allocate_rect(rail)` 抓不住；命中矩形用 rail 的 x 范围 × strip 的 y 范围，与 inline 滑条的 `hit` 同一思路。
- **写入复用既有三条通道**：`InlineParam::{MixerGain,MixerMute,MixerSolo}(ch)` → `inline_doc_value`（读 `channels[ch]`，索引越界返回 0 而非 panic——通道删除与绘制可能差一帧）→ `PatchAction::SetInlineParam` → `apply_inline_param`。fader 用 `commit_param_edit`，**merge key 必须带通道号**（`mixer-gain-{ch}`），否则连续拖两个通道的推子会合并成一步 undo；M/S 是离散点击，`begin_edit`/`commit_edit` 各一步。
- **同步是结构性的，不是同步代码**：两侧都读写 `NodeDoc::Mixer.channels`，引擎侧都发 `ParamId::MixerGain/Mute/Solo(ch)`（`set_node_param`），所以"patch 改完 Inspector 显示旧值"这类漂移不可能发生——不需要任何额外的一致性逻辑。

### 一批 UI 修正（2026-10-23：meter 横放 / timeline mute 移除 / 编辑器坐标 / 参数面板门控 / picker 点击）

- **Master meter 横放**：`master_meter` 改两条水平条（上 L 下 R，各 7px 高），`allocate_exact_size(ui.available_width(), 18.0)` 吃掉走带行剩余宽度；调用顺序 = gain 推子 → `Panic` → meter（meter 在 Panic 右侧）。
- **timeline 的 M 按钮删除**（mute 归 mixer 节点）：`TimelineAction::ToggleMute` / `TimelineParams::muted` / `HeaderKind::Midi::muted` / `track_mixer_channel_state` 全链删除；MIDI header 名字按钮宽度 `HEADER_WIDTH - 56` → `- 16`（只剩 grip 占位）。
- **插件 GUI 坐标（重要根因修正）**：`reposition_rect_platform` 删除 `ClientToScreen` origin 减法。egui 坐标 = client 相对（egui_winit 以 `Pos2::ZERO` 构造 `screen_rect`），child 窗口坐标 = 父 client 相对，`rect * ppp` 直接对上。旧代码多减 origin = 容器上移一个标题栏高度——这就是从 G2 起一直存在的"GUI 盖住按钮 / Editor 标题"的根因。
- **Instrument 参数面板按 voice 门控**：第二个 Inspector Instrument 块里先解析 `voice_is_vst3` / `voice_is_clap`，`Plugin Params` / `Plugin Params CLAP` 两个 CollapsingHeader 各自只在对应 voice 时渲染；built-in / SoundFont 乐器不再出现空面板（与 effect 面板"有参数数据才出现"的规则对齐）。
- **picker 鼠标点击失效（egui hit-testing 坑，必记）**：egui 0.31+ 的点击命中停在指针下**最上层** widget，不穿透——picker 行内 `Label`（hover sense）注册在行 response **之后**，把点击吸掉了，所以只有 Enter（键盘不走 hit-test）能选。修法：`allocate_exact_size(Sense::hover())` 占位 → 画内容 → **最后** `ui.interact(rect, 显式id, Sense::click())` 注册（行成为自己标签的上层）。行 id 显式化还顺带解决了"Recent 分组条件渲染导致自动 id 漂移"的隐患；`mouse_click_picks_a_row` headless 测试锁住行为（`ctx.read_response` 取行 rect → 跨帧 press/release）。注意 read_response 要等 popup **settle 帧之后**（开帧是 sizing pass，行可能没画）。

### patch 节点内联操作行 + 双击路由（U10，2026-10-23）

- **几何单一来源**：`size_of` 为 Timeline（2 行）与 Mixer（1 行）预留 `NODE_TITLE_BAND + action_rows * ACTION_ROW_HEIGHT` 的头部带；端口位置改由 `port_area()` 计算——上界在头部带之下、下界再减去内联滑块行（`inline_row_count * INLINE_ROW_HEIGHT * zoom`）。`out_ports_screen` / `in_ports_screen` 同时供**绘制与命中测试**使用，所以只改一处就保证"看到的位置"与"能拖到的位置"一致；漏掉下界会让 mixer 最后一条端口漂到 `Out Pan` 滑块上。
- **按钮要自绘，不能 `egui::Button`**（2026-10-28 修正，spec `sf2-loader.md` D15）：`egui::Button` 的最小高度是 `spacing.interact_size.y`（18 pt），而操作行在屏幕上只有 `18 × zoom` 高——0.6–1.0 缩放区间按钮比行还高，被居中后顶出去盖住节点标题（缩到 0.6 以下整行消失，所以只在中间区间露出）。改为 `action_button`：`painter` 自绘底+描边+截断文字（`LayoutJob` + `text::TextWrapping::truncate_at_width`）、字号按行高封顶、painter 裁到按钮矩形，再用 `ui.allocate_rect(rect, Sense::click())` 注册命中。禁用态仍保留 hover 文案（"为什么点不了"）。几何集中到 `action_row_band` / `action_button_rects`，回归测试 `action_buttons_stay_inside_their_band_at_any_zoom` 在 0.6 / 0.75 / 1.0 / 1.75 四个缩放点断言按钮不出带、不出盒。旧实现（`scope_builder` + `add_enabled` + 按 zoom 缩 `button_padding`）解决的是"禁用态不可表达"，新实现两者兼顾。
- **zoom 门槛**：`ACTION_MIN_ZOOM = 0.6` 以下不画按钮（缩略视图里按钮只有十几个像素，点了只会误触）。
- **动作全转发既有实现**：`add_midi_track` / `remove_last_midi_track` / `add_control_track` / `remove_control_track` / `mixer_add_channel` / `mixer_remove_channel`，undo 粒度与引擎同步沿用原路径。`- Slot` 的禁用条件抄 `mixer_remove_channel` 的拒绝条件（最后一槽仍有音频线），把"点了没反应"提前说清。
- **双击路由要门控**：`opens_inspector(node)` 只放行 Instrument / Effect / Mixer。Inspector 在没有编辑目标时会 fallback 到"当前轨的乐器"，所以让 VK / Feedback / ChannelStrip 也能跳过去 = 显示别人的参数，比不反应更糟。Timeline 跳 `MainView::Timeline`，Master（参数在走带条）与 Timeline In（纯汇）不响应。

### SF2 音色库加载器 S1–S6（2026-10-19 收官，spec 见 `.agents/spec/sf2-loader.md`）

- **S1 RT 审计**（结论细节在 spec §7）：`rustysynth-ext = "=1.4.0"` 热路径零堆分配实证（计数 `#[global_allocator]`，测试 `tests/sf2_rt_audit.rs`），`VoiceCollection` 即预分配池，voice 池 fork 预案（D4）解除；唯一 panic 点 = `render` 等长契约（调用侧保证）。**审计 harness 的坑**：断言 label 的 `format!` 在计数窗口内求值会被计入分配（首跑真实库的假阳性），label 构建须在 scope 之外；计数器全局，测试须 `Mutex` 串行。
- **共享测试 fixture**：`tests/common/mod.rs` 手建最小合法 SF2 字节流（单 preset 正弦 + loop + exclusive class，bank/program 可参数化 `minimal_sf2_bytes_at(bank, program)`）——纯 `cargo test` 无外部文件可跑全部路径；真实库测试统一走 `#[ignore]` + `GLITCHERZ_SF2_PATH` 环境变量（本地数据不入库，D9）。**手写 RIFF 的坑**：ibag terminator 的 generator index 必须指向 igen 全部记录之后（差一条 → zone 丢 `sampleID` → 被当 global zone → 0 region → 静音且不报错）。
- **S2 非实时模块 `engine::sf2`**：`Sf2Library::load(path)`（控制线程：BufReader + `SoundFont::new`，产物 `Arc<SoundFont>` 交 RT 侧不再解析）+ 加载期一次性 `presets(): &[Sf2Preset{bank: u8, program: u8, name}]` 枚举（D7 轨级 u8:u8 寻址；**bank/program 超出 u8 的 preset 跳过 + log::warn**，全部不可寻址 → `Sf2Error::NoPresets`）+ `find_preset(bank, program)`（UI 下拉与 S4 VoiceSpec 寻址共用）；`Sf2Error{Io, Parse, NoPresets}` thiserror。rustysynth-ext 自 S2 起为 engine 正式依赖。测试 `tests/sf2_load.rs` 8 场景（含截断/垃圾字节/越界 bank），真实库（MuseScore_General/FatBoy）load + 枚举 + **preset 地址唯一性**验证通过。
- **S3 实时渲染 `realtime/sf2.rs` `Sf2VoiceProcessor`**：实现 `Processor`，spec §7 接入要点全部落实——① `SynthesizerSettings.block_size = 128`（对齐 `FIXED_BLOCK_SIZE`）；② stereo render 缓冲构造期预分配，`process` 内经 `render(&mut l, &mut r)` 等长切片填入（排除库唯一 panic 点）后 `copy_channel` 进图缓冲（目标帧数小于 render 帧数时截断，mono 缓冲只写 ch0）；③ 轨级 bank:program 经 `process_midi_message(0xB0/0xC0)` 下发（零分配，S1 已实证），`set_master_volume` 纯写入挂 `ParamId`。**参数面**：`ParamId::{Sf2BankSelect, Sf2ProgramChange, Sf2MasterVolume}`（bank/program 为字面整数参数：`f32.round() as i32`，NaN 饱和转换为 0；负值不钳制——库侧 `set_bank/set_patch` 纯字段写、仅作 preset_lookup HashMap 键，未命中走 GM/默认 preset 回落，已核实安全；master volume 非有限值忽略）；`Sf2Error::Synth` 变体承接 `SynthesizerSettings` 越界（采样率 16000..=192000 / block 8..=1024 / 复音 8..=256）。**复音** `SF2_POLYPHONY = 64`。**毯式 note-off 语义**：引擎 transport stop 发 `Off { note: 0 }`，pitch-matched 接收端会忽略 → 挂音；以 `held: u128` 位图跟踪按下键，`Off { note: 0 }` 且 key 0 未按住 = `note_off_all()` 全释放，key 0 按住 = 正常 pitch-matched 释放（`held_note_zero_release_stays_pitch_matched` 测试锁定）。**边界**：velocity f32→MIDI 七位（NaN/Inf 钳制）；越界 note（≥128）照常转发 synth（内部静默忽略）但 `1u128 << note` 前有 `<128` 守卫（否则 panic）。测试 `tests/sf2_voice.rs` 13 场景 + 真实库 smoke（`#[ignore]` + `GLITCHERZ_SF2_PATH`）。**测试坑**：GM 默认 CC#91 reverb send=40，被拒绝的音符仍带 ~0.003 尾音残余——绝对阈值断言（`< 0.001`）会假失败，须先渲染 baseline 再相对比较；fixture `two_preset_sf2_bytes` 的 preset keyRange 以 pgen generator `lo | hi << 8` 编码（`20..=40`）。
- **S4 model `VoiceSpec::SoundFont`**（spec D5/D6/D8）：`SoundFont { path: String, bank: u8, program: u8 }` 内部 tag 枚举新变体（tag = `"sound_font"`，serde default 加法字段，PROJECT_VERSION 不动；bank/program 缺失加载为 0:0）。测试：往返覆盖进 `voice_spec_ron_roundtrip` + 新 `voice_spec_soundfont_serde_compatibility`（tag 形态 + partial 字段默认）。**两个 serde gotcha**：RON 紧凑输出无空格（断言 `contains("type:\"sound_font\"")`，带空格版本假失败）；`#[serde(default)]` 的默认值是类型默认（`u8::default()` = 0）而非实例值。**UI 侧连带补齐 8 处非穷尽 match**（编译必需）：`install_node` Instrument+SoundFont = 完整文档恢复路径——非实时线程 `Sf2Library::load` → `Sf2VoiceProcessor::new` → 构造期 `set_param(Sf2BankSelect/Sf2ProgramChange)` 下发（D7 地址在装入前重放），文件丢失/解析失败回落 `SynthProcessor` + `log::error`（D8，文档字段保留）；Effect+SoundFont 回落 `DelayProcessor`（防御性，UI 不产生该组合）；`voice_plugin_key`/`track_external_fx` → `None`（S5 前无 preset 库键/外挂效果器面板）；preset load/reset 的 `&mut` match → `{}`（SF2 无 state blob）。patch 图标签 `SF2 {bank}:{program} · Track N`。
- **S5 UI 音色库面板**：Synth 面板 Instrument 区加 `SoundFont…` 按钮（`rfd::FileDialog` filter `sf2`/`sf3`，与项目保存同一同步弹窗先例）→ 调用侧 begin_edit + 文档改 `VoiceSpec::SoundFont{path, bank: 0, program: 0}` + `load_soundfont_voice` + commit_edit（与 CLAP/VST3 换声源同一 undo 分工：改文档在调用侧，函数只做 engine 替换）。**会话单库**（spec §6 v1 不做多库）：App 字段 `sf2_library: Option<Arc<Sf2Library>>`；`obtain_sf2_voice(path, sr)` = path 匹配复用缓存 / 未命中 `Sf2Library::load` 并替换缓存，`install_node` 与面板共用（同一工程重开秒装，避免每次 install 重读几十 MB 文件）。`load_soundfont_voice`：retire_guard（外部插件切 SF2）→ obtain → `set_param(Sf2BankSelect/Sf2ProgramChange)` 构造期下发 → `replace_node` → 清其他格式面板缓存。**preset 下拉**：当前轨声源为 SoundFont 时显示 ComboBox（`sf2_preset_{sel}`），preset 列表从共享库枚举后**快照 clone 出来**（`Vec<(u8,u8,String)>`，释放 `self.sf2_library` 借用——否则 ComboBox 回调里 undoable 编辑的 `&mut self` 借用冲突）；`set_soundfont_preset` = begin_edit + 文档改 bank/program + `engine.set_node_param(engine_id, Sf2BankSelect/Sf2ProgramChange)`（节点寻址即时下发，FIFO 顺序保证 bank 先于 program）+ commit_edit("Select SF2 Preset")；undo/redo 经 apply_document → install_node 重放。库未加载（跨会话或路径不符）显示 `(soundfont file not loaded this session — reopen it)`；`Built-in Synth` 高亮条件改为 `!is_external && !is_sf2`（源选择行本身已于 U9 E5 删除）。真实库端到端验证：MuseScore_General smoke（256 preset 枚举 + preset 切换 + 有声输出）通过。
- **S6 SF3 feature 常开 + 端到端验收**：workspace `rustysynth-ext = { version = "=1.4.0", features = ["sf3"] }`——lewton 0.10（Vorbis 解码），新增传递依赖 lewton/ogg/byteorder/tinyvec 全纯 Rust、无 C 构建链（跨平台 CI 前提保持）。SF3 解码只发生在 `SoundFont::new`（加载期，控制线程 D3），RT 热路径代码零改动。**S1 审计复跑（sf3 开启后）**：fixture 9 场景 + 真实库 MuseScore_General（256 preset 逐个 bank/program 切换 + 和弦 + render 逐计数）全部 0 分配；真实库端到端验收 `-- --ignored` 全绿（sf2_load / sf2_rt_audit / sf2_voice 三套）。UI 侧 S5 的文件对话框 filter 已含 `sf3`，无需改动。本机无 .sf3 样本文件，SF3 解码路径的实证验证留待拿到真实 SF3 库后补跑（依赖面与解码线程已静态核实）。
- **S8 SF3 实测 + 奇数块补齐（2026-10-28）**：真实 SF3（Airfont380 Stanway Grand，3 MB）首次加载报 `invalid sound font: the INFO list contains an unknown ID '?LIS'`。**根因不是解码器而是 RIFF 遍历**：该文件 `smpl` 长度 3 099 967（奇数），RIFF 要求其后补 1 字节对齐、且该补齐字节**计入外层 LIST 长度**，而上游 `soundfont_sampledata::read` 的 `while bytes_read() < end` 循环读完 `size` 就下一个块 → 错位一字节，下一块 id 读成 `?LIS`（`?` 是非 ASCII 替换符）。**SF3 的 `smpl` 是拼接的 Ogg 流，长度奇数是常态**，所以这不是"某个坏文件"而是"所有 SF3 都挂"。修法：`BinaryReader::discard_padding(reader, size)`（奇数 size 吃掉 1 字节）在 `sdta` / `INFO` / `pdta` 三处循环尾部各调一次；回归测试 `reads_an_odd_sized_smpl_chunk_followed_by_another_chunk`（合成字节流，无外部 fixture）。修复后该文件 `Sf2Library::load` → 1 preset `0:0 Steinway Grand`，debug 构建解码 5.2 s（release 会明显更快）。**注意**：`.vendor/rustysynth-ext` 是独立 package（workspace `exclude`），其单测只能在该目录内 `cargo test` 跑，仓库 CI 不覆盖；3 个 `test_load_*` 失败源于缺上游 samples fixture，与本地改动无关。
- **S7 preset 合并语义调查（2026-10-20，结论：保持相加语义，"vel 100 偏轻"非引擎 bug）**：起因 = 钢琴类 patch 虚拟键盘（vel≈0.9）有声、piano roll（默认 vel 100/0.7874）偏轻，疑 preset zone 合并语义错误。**实证过程**：① vendored `rustysynth-ext` 到 `.vendor/`（根 `Cargo.toml` path 依赖 + `[workspace] exclude`，保留 sf3 feature），新增 preset 层 `explicit: u64` 位掩码 + SF2 2.04 §8.2.3 的 14 个非法 preset generator 过滤（`PRESET_INVALID_GENERATORS`，当前测试字体无此类项零行为差异）+ 公开诊断 API `PresetRegion::get_generator()`；② 实现 SF2 §8.5.2 覆盖语义（preset 显式 generator 覆盖 inst 值，仅 initialAttenuation 相加）后 vel 1.0 峰值从 0.0912 **暴跌 150 倍至 0.0006**——假设证伪，回退相加语义。**根因**：MuseScore_General（机器生成字体）的 preset 值是**覆盖意图与相加意图的混合体**——覆盖意图如 `attackVolEnv=-7973`（10ms 整值）/`5573`（25s 哑音层技巧），相加意图如 FF 层 `initialFilterFc=-343`（微调偏移，覆盖语义下 cutoff 变 6.7Hz 近静音）。纯覆盖或纯相加都无法两全，但相加优雅降级、覆盖造成灾难，故 MeltySynth/RustySynth 家族的相加行为是正确选择。**vel 100 偏轻的真实构成**（note 60 匹配多 zone 叠加）：`key 0..127 vel 75..107` zone（attn 14dB + `modEnvToFilterFc=-2350` 暗化 → 用 MF 采样）+ `key 60..127 vel 64..107` zone（attn 17dB + attack 25s 哑音层）+ rustysynth velocity→gain 曲线（`2·20·log10(vel/127)` Polyphone 风格，vel 100 → -4.2dB）= 字体设计使然，非 bug。**外部参照全部不可用**：webfetch 损坏；scoop 装 FluidSynth / cargo fetch oxisynth 的 GitHub 下载均被 sandbox 拦截。测试 `real_soundfont_velocity_sweep` 的 dump 已扩展为打印每 preset zone 的 key/vel range + 显式 generator 清单（`generator_name()` 辅助）+ inst 层滤波/包络参数，供后续人耳对照。vendored crate 自身 3 个失败单测 = 缺上游 testdata fixture（registry 包不含），与本地修改无关。

### 时间轴入图（P3，2026-10-08）

- **MIDI 连线表**：`ExecPlan::midi_targets`（每 MIDI 轨扇出 ≤ `MAX_MIDI_FANOUT`=4）——音序器与键盘演奏的音符事件经 `RtGraph::track_note` 扇出到全部连线目标（`note_off_all` 同样遍历连线表）；默认图每轨连本轨声源（复刻旧语义）。编译器侧 `connect_midi/disconnect_midi`（重复连线幂等、空槽/越界拒绝、扇出超限报 `MidiFanOutOverflow`）。
- **自动化连线表（D7）**：`RtAutomation` 退化为**纯锚点曲线**（`enabled + points`，不再内嵌目标参数）；新增 `RtAutoTrack { loop_ticks = 384（一小节，可独立设置）, curve }` 独立实体（`MAX_AUTO_TRACKS`=16），与 MIDI 轨道音源循环解耦。`ExecPlan::auto_routes[16]` 终点 = `(槽位, 参数)`，控制侧以轨道坐标 `(track, param)` 经 `route_slot`（混音三项→通道条、延迟三项→延迟、其余→声源）解析；`RtGraph::auto_set_param` 查表投递，未连线静默。
- **命令面**：`SequencerCommand::SetAutomation` → `SetAutoTrack { index, auto }`；`AudioEngine` 新增 `set_auto_track`（命令队列）/ `set_auto_route` / `clear_auto_route`（编译器→结构队列，队列满快照回滚）。
- **UI 适配**：`send_automation` 同步下发曲线（`SetAutoTrack`）与连线终点（`set_auto_route`）；自动化轨道索引 = `track × MAX_AUTOMATIONS(4) + 文档 Vec 槽位`（Vec 只追加/原位替换，槽位稳定）；循环长度随泳道显示口径（片段长或一小节），P4 起由文档 v2 显式携带。
- **结构队列扩容 8 → 64**：项目加载一次性下发最多 16 条自动化连线 + 每轨声源恢复，原容量会静默丢连线。
- Sequencer 内核零改动（`advance` 回调签名不变）；现有音序/自动化端到端测试全部经连线路径通过。

### CLAP 宿主（`glitcherz-clap-host`）

- **参数枚举**：`ClapInstance::list_params()` → `raw::enumerate_params`——直接经 `clap_plugin.get_extension` 取 `clap_plugin_params` vtable 指针直调（clack 0.2 的 `PluginParams` 包装无读取方法，raw 模块为唯一 unsafe 边界）；返回 `ClapParamInfo{id, name, module, min/max/default}`。测试插件实现一个可自动化 Amplitude 参数（ID 1，0..1，默认 0.5，f64 位模式原子共享），参数事件驱动渲染振幅。
- **UI 接入**：`clap_voice_params`（轨道 → 参数快照）在装填 CLAP 声源时填入；参数面板 CollapsingHeader 内滑块直接调参（当前值存 `clap_voice_values`（UI 会话内，track → param id → f64），变更经 `send_track_param(ParamId::PluginParam)` 下发，块粒度下一 process 生效；重载插件/切回内置/文档重应用时重置为插件默认值；state 持久化落地后，重载路径经 `load_state` + `read_param_values` 重播种滑块缓存）；自动化目标选择器合并 CLAP（曲线值 = 插件值域原始值）与 VST3（曲线值 = 归一化 0..1）两份列表，并含外挂效果器参数（端点 = 效果器节点）。
- **真实插件兼容性复现**：`dexed_instrument_note_on_produces_audio`（clap-host 测试）——标准扫描目录发现 `dexed*.clap` 时实例化 + NoteOn + 16 块内断言非零输出，找不到则跳过；用于把"宿主桥接问题"与"插件状态/默认补丁问题"（宿主无 gui/state 扩展，Dexed 跑在内部默认补丁上）分开定位。

### 插件状态持久化（CLAP state extension，2026-10-15，spec 见 `.agents/spec/plugin-state.md`）

- **宿主侧**：workspace `clack-extensions` features 加 `"state"`（clack 0.2 的 `PluginState` 包装**有**完整 save/load 方法，与 params 不同、无需裸调 vtable）。`GlitcherZHostShared.state_ext: OnceLock<Option<PluginState>>` 在 `initializing()` 经 `InitializingPluginHandle::get_extension` 捕获，spawn 后 `access_shared_handler` 读出存进 `ClapInstanceGuard`；`GlitcherZHostMainThread` 实现 `HostStateImpl`（mark_dirty → no-op，采集时机见 D3）。
- **API**：`ClapInstanceGuard::{save_state() -> Vec<u8>, load_state(&[u8])}`（main thread，错误变体 `NoState/StateSave/StateLoad`）；`ClapInstance::read_param_values()` → `HashMap<u32, f64>`——raw vtable `get_value`（复用 enumerate 的扩展抓取模式），供状态加载后刷新滑块缓存。
- **数据落点（D1/D2）**：`VoiceSpec::Clap/Vst3` 各加 `state: Vec<u8>`（`#[serde(default, skip_serializing_if = "Vec::is_empty")]`）——旧文件无感，`PROJECT_VERSION` 不动。**注意**：VoiceSpec 是内部 tag 枚举（`type: "clap"` snake_case），RON legacy 形态测试别写成 `(Clap: ...)`。
- **数据流（D3/D4）**：保存 = `do_save` → `capture_plugin_states()`（engine_id 经 `node_map` 反查 doc_id，两段式：guard 循环采集 blobs → `project_mut()` 写文档；单实例失败 log + 跳过，D7）→ 序列化；恢复 = `install_node` spawn 成功后 `state` 非空即 `load_state` + `read_param_values` 回填 `clap_voice_values`。面板换声源（`load_clap_voice`/构造点）产 `state: Vec::new()`；切内置声源状态随 `VoiceSpec::Builtin` 替换自然丢弃（换声源动作 E5 后不存在，构造点统一走 `create_patch_node_on`）。undo/redo 经 `apply_document → install_node` 走同一恢复路径（但状态 blob 本身不进 undo 图，D5——保存前 undo 回退到旧快照时 state 为旧值）。
- **测试**：clap-host `state_roundtrip_restores_amplitude`（设参 0.25 → save（8 字节）→ 新实例 load → get_value 读回 0.25，新实例先验证默认 0.5）；model `voice_spec_state_serde_compatibility`（空 state 不序列化 + legacy 无 state 字段可加载）。
- **VST3**（S5，2026-10-15）：`HostMemoryStream`（`IBStream` 内存流 COM 对象，控制线程专用；读/写/seek/tell 全实现）+ `Vst3InstanceGuard::{save_state, load_state, read_param_values}`——save = 组件 `getState` 写入内存流后整块取出；load = 组件 `setState` + 控制器 `setComponentState` 同步（**两个消费者各一条独立流**：ComRef 不走 SmartPtr 毯子实现，无 vtable 透传 seek，直接免掉 rewind）；save 时插件保持活跃（与 CLAP 镜像一致的简化宿主姿态，Dexed 实测兼容；规范最稳妥顺序是先于 setActive，留待生命周期重构收紧）；测试插件工厂改**逐实例创建**（原 OnceLock 单例与 kManyInstances 相悖且测不了"新实例回落默认值"），SineComponent 以 8 字节 LE f64 经 `getState/setState` 持久化 Gain（`IParameterChanges` 逐队列消费、末点胜出、作用于振幅），控制器镜像读回 = D6 滑块刷新载体（归一化滑块 UI 待接入，`read_param_values` 已预留）。

### 节点图（`realtime::graph`，P1+P2，spec 见 `.agents/spec/node-graph.md`）

- **项目文件 v2（P4）**：`PROJECT_VERSION = 2`，无 v1 读路径（serde 字段级默认值容忍缺失）。新增 `Project.graph: GraphDoc`（过渡形态，spec §7 完整 nodes+cables 随 P5 落地）——`midi_links`（MIDI 跨轨扇出，上限 16；加载期校验越界/自连/超容报 `ModelError::InvalidGraph`）；`Automation::loop_ticks` 显式携带（默认 384），泳道显示口径同步回写。引擎侧配套 `AudioEngine::connect_midi_track/disconnect_midi_track`（快照回滚），UI `push_all_params` 加载时重放连线。
- **`RtGraph`**：定容 64 槽位，每节点一个专属立体声 128 帧输出缓冲；处理按预编译拓扑序遍历——清零 → 多入边自动求和（≤8）→ 原地 `Processor::process`；主输出槽缓冲即 cpal 写出源。
- **`ExecPlan`**（全 `Copy`）：Kahn 拓扑排序 + 入边表 + 每轨 voice/delay/channel 路由表（P3 由时间轴连线表替代）；编译期拒绝环（D10）与入边超限。
- **结构更新协议（D5）**：控制侧 `GraphCompiler`（全 `Copy`，队列满可快照回滚）编译计划并产出 `GraphUpdate::SetTopology { plan, deployments, removed }`；音频线程块边界整体换计划 + 指针级装卸，零分配；被移除节点连同输出缓冲经 `RecycledNode`（已扩展携带 `output: AudioBuffer`）回执队列送回控制线程释放。
- **`ChannelStripProcessor`**：增益/声像（线性律）/静音升级为真节点（原 `TrackStrip` 求和段逻辑）；默认拓扑复刻旧 Mixer（`声源 → 延迟 → 通道条 → 主输出`），旧 mixer 全部数学测试以默认图形式通过。
- **行为变化**：`set_builtin_voice` 现在回收外部插件节点（旧实现驻留），切回插件需重新实例化（状态不保留）；UI 守卫经回收 `node_id` 正常配对释放。
- P1+P2 时 `mixer.rs` 曾随固定 Mixer 一起删除（`MixerLevels` → `GraphLevels`；`VoiceUpdate` → `GraphUpdate`）；P5f 重建 `mixer.rs` 承载 `MixerProcessor`（见下节）。

### Mixer 节点与单输入语义（P5f，D14–D21，2026-10-09）

- **单输入纪律（D14/D16）**：compile 期按 `NodeKind`（`Plain` / `Mixer` / `Master`）校验入边——Plain 与 Master ≤1、Mixer ≤32 且 `in_port` 唯一，超限整个计划拒绝（`FanInOverflow`）。隐式加合废止：新连线落到已占用 `(to, in_port)` 槽位时**顶掉旧线**（`add_cable` 严格替换语义，精确重复幂等）。
- **`MixerProcessor`（D15/D20）**：语义分两段——**per-channel 权重**（gain × pan 声像系数 × mute × solo）在图求和循环逐边乘（求和后通道分离不复存在，权重必须在求和时生效），经 `Processor::input_weights()` 暴露——带默认实现返回 `None` = 单位权重，全部内置与 CLAP/VST3 节点零改动，仅 Mixer 覆写；权重在 `set_param` 时重算缓存。**输出段**（pan/mute）原地处理，与旧通道条同数学。solo 作用域 = 单台 mixer（任一通道 solo → 未 solo 通道权重清零），级联 mixer 各自独立。
- **参数路径（D5/D20）**：`ParamId::{MixerGain(u8), MixerPan(u8), MixerMute(u8), MixerSolo(u8), MixerOutPan, MixerOutMute}`——推子/pan/mute/solo 全走节点参数路径（块边界生效），**永不触发拓扑重建**。
- **默认图（D18，2026-10-29 精简）**：默认 = 一条可弹 MIDI 轨（内置乐器 → `mixer.ch0`）+ 单个全局 Mixer（默认 1 通道）→ master + Computer 键盘 → 该乐器 + 每条 audio 轨一个未接线的生成器节点；默认**不挂 delay**。新建 MIDI 轨只增加空轨道位（`track_present` 表标记占用，不建节点/不接线/不扩 mixer），用户在 patch 自行搭链。`allocate_track_chain` 仅保留给引擎参考拓扑/测试，其电费点（`track_channel`）= 轨道链末端 delay。
- **旁路修订（D19）**：`remove_node` 旁路重连仅适用于 ≤1 入边节点（效果器照旧旁通，直连保留目标 `in_port`）；mixer/master 多入边无处重连——删除即弃线。
- **model（Q-B 决议）**：`NodeDoc::Mixer{node_id, track, inputs: u8(1..=32), channels: Vec<MixerChannel>, out_pan, out_muted}` 替代 ChannelStrip（旧变体保留仅作加载读路径，新文档不再产出）；加载期自动迁移——strip → 1 通道 mixer（原 gain/pan/mute 进通道 0）、master 入边 >1 时插入合成 mixer 承接。文档校验（load/save 双端）：非 mixer ≤1 音频入边、mixer `in_port` 唯一且 < `inputs`、通道数 1..=32。
- **UI**：mixer 节点画 N 个输入口（节点盒随通道数伸缩）+ 按口命中落线（精确口命中优先、否则自动落第一个空通道——任意空口可落，D21）+ 调音台风格面板（每通道 solo/mute/竖直推子/pan + 输出段 pan/mute + 通道增删按钮 1..=32，占用口不可删）；ChannelStrip 面板退役。
- **轨寻址（P5f ④）**：UI `track_channel_target(track)` 沿轨链连线走到首个 mixer 节点得 `(mixer_doc_id, in_port)`；轨推子/mute 读写对应 mixer 通道（文档与引擎参数路径同步），未接入 mixer 的轨回落默认值。`register_track_chain` 的 channel 槽位退化为电平点（传 delay）。
- **容量**：`MAX_IN_EDGES` 8→32（仅 mixer 可达）、`MAX_CABLES` 128→256（32 口 mixer 单节点即占 32 线）；32 通道求和 ≈8K FMA/块 ≈ 8µs，48kHz 下 <0.5% 单核。

### 引擎核心（`glitcherz-engine::realtime`）

- `AudioBuffer`、`Processor`（`set_param` + `note_event`）、`ProcessorChain`、`BlockRunner`（固定 128 块拆分 + 块边界命令消费 + `Feedback` 原子回传：主电平/位置/步进/走带/**每轨电平**）；
- **`Mixer`**：4 条 `TrackStrip`（`SynthProcessor → DelayProcessor`）并行处理 + 线性声像/增益/静音求和 → 主链（`MasterGain`）；`MixerLevels` 每块回传各轨 post-fader 峰值；
- 完整合成器链（每轨）：正弦振荡器 + 线性 ADSR → TPT SVF（低通/高通/带通）→ 反馈延迟（`DelayProcessor`：预分配环形缓冲 1 s 上限、feedback ≤ 0.9、反规范数冲洗、每声道独立写指针）；
- `Sequencer`（音频线程块边界采样时钟调度）：**纯窗口扫描**模型——只维护音乐位置（tick，1 四分音符 = 96），每块把 `[t0, t1)` 内到期事件一次性调度出来；BPM 变化只改未来速率（音乐位置不跳），循环回绕由窗口映射推导；**每轨双音源**（16 步步进模式 / 钢琴卷帘片段，`SetSource` 切换），卷帘音符按自身 start/length 触发；事件按 `(track, NoteEvent)` 送入对应轨道（≤1 块 ≈ 2.7 ms 抖动，逐样本偏移待后续）。

### 结构更新与节点回收（`realtime::voice`）

- `VoiceUpdate` 结构更新队列（rtrb 携 `Box<dyn Processor>`，**非 Copy**，控制侧分配 / 实时侧块边界只消费）；
- `TrackStrip` 声源**双槽预分配**（内置 `SynthProcessor` 常驻 + `external_voice: Option<Box<dyn Processor>>`，换装 = 指针写入零分配）；
- CLAP 效果器槽：`TrackStrip` 信号顺序固化为 `声源 → DelayProcessor → 外部效果器槽[0..4]`（`effect_slots: [Option<Box<dyn Processor>>; 4]` 预分配）；
- **节点回收回执队列**（`recycle_channel`，音频 → 控制方向）——被替换的旧节点由音频线程塞回执队列、控制线程 `AudioEngine::reclaim_nodes` 排空释放，UI 按 `node_id` 丢弃配对守卫后安全卸载动态库；实时线程**永不 drop 堆对象**（回收队列满时放弃卸装而非就地 drop）；
- UI 侧 `voice_guards` 注册表持有 `ClapInstanceGuard` / `Vst3InstanceGuard`（**不可提前丢弃**——卸载动态库会使实时节点悬空，进程退出统一回收）。

### 自动化（路线图第 8 步）

- `glitcherz-model` 的 `Automation`（`AutoParam` = 音量/声像/截止 + `AutoPoint{tick,value}` 有序列表，线性插值、两端保持端值）挂 `Track.automations`；
- 引擎侧 `RtAutomation`（定容 64 锚点 Copy）经 `SetAutomation` 命令 upsert（同参替换 / 空槽插入 / 空曲线=删除，每轨 ≤4 条）；
- 求值器在 `BlockRunner` 每块**仅播放中**按"循环后的音乐位置"（步进源 = 一小节、卷帘源 = 片段 loop_ticks 取模）采样并经混音器内部路径直写参数（不走命令队列）；UI 在走带停止沿重推手动参数实现"停止后回到手动值"；
- 插件参数桥接端到端（引擎集成测试锁定）：自动化曲线 → `AutoParam::PluginParam{param_id}` → `ParamId::PluginParam` → 求值器 → `TrackStrip`（`PluginParam` 只路由外部声源）→ `ClapNode` → `ParamValueEvent` → 插件生效。

### 控制轨 CC 输出与 inline 曲线带（U4a/U4b，spec `control-tracks.md`，2026-10-23）

- **CC 是电缆的"目标模式"，不是新的电缆类型**（D1）：端点仍走 `CableKind::Automation`（`AutoTrack → Node`），拖线、删除、单源约束、`validate` 全部零改动——节点目标已拒绝 timeline-only 参数、`TimelineIn` 只收 `Tempo`，所以 CC 自动落在正确位置，**结构本身保证合法性**。
- **`AutoDest` 分派**（D2）：引擎侧 `AutoRoute{slot, param: ParamId}` 换成 `AutoRoute{slot, dest: AutoDest}`，`Param(ParamId) | Cc{controller, channel}`；`RtGraph::auto_set_param` 改名 `auto_deliver`，按 dest 分派到 `slot_set_param` 或 `slot_midi_cc`。`Processor::midi_cc(channel, controller, value) -> bool` 是**新开的一条显式通道**（D3），不复用 `NoteEvent` 变体——音符路径的消费方（SF2 voice 池、插件事件槽）与 CC 无关，混进来只会让两边的分支都变脏。
- **值域与去重**（D4/D7）：曲线存**真实 7bit CC 值**（不是归一化的 0..1），纵轴 0..127；投递时 `clamp(0,127).round()`（round 而非 truncate，否则曲线停在 0.4 附近永远发 0），`last_cc: [i16; MAX_AUTO_TRACKS]` 记上次发送值（`-1` = 从未发）→ **变化才发**。`apply_update` 末尾把 `last_cc` 清成全 `-1`：重定向到新目标后必须立刻收到一次当前值，否则要等曲线动起来才生效。
- **时间戳取块边界**（D5）：CLAP 走 `MidiEvent::new(0, 0, data)`（`time = 0`），SF2 走块首 `process_midi_message`；采样级 CC 需要每采样事件队列，收益不可辨，明确不做。
- **格式差异**（D6）：CLAP 组装 `0xB0 | channel` 注入输入事件流；SF2 **忽略外来 channel**（该节点所有音符都发在固定 `CHANNEL`，尊重外来 channel 反而静默失效）；**VST3 明确不支持**——`vst3-0.3.0` 的 `DataEvent` 只有 `kMidiSysEx`，`kLegacyMIDICCOutEvent` 是插件**输出**语义，宿主用它属未定义行为，`Vst3Node::midi_cc` 返回 `false`。
- **UI 接线**：终点行多一条 `MIDI CC…`（就地 `CC`/`ch` 两个 DragValue，release 才提交 = 一次 undo）、端点显示 `cc_label()`、拖线菜单对 instrument 追加 8 条预设（1/2/7/10/11/64/71/74）；`set_auto_endpoint` 按 `param.cc_of()` 在 `set_auto_cc_route` / `set_auto_route` 间分派，文档重推（undo/open）那条路径同样分流。
- **~~inline 曲线带~~（U4b D9/D10，已由 U12 取代）**：选中控制轨时，其行**下方**展开 84 px 高的带（`curve_band()`）……（历史记录，见下方 U12）
- **~~单一编辑入口~~（D10，已由 U12 取代）**：`EditTarget::Control` / `control_editor_body` / `OpenControlEditor` 全部删除……（历史记录，见下方 U12）
- **手势与 undo**：点击加锚（`dragging: false` → 单步 `Add Anchor`）、拖拽移动（`curve:{track}:{clip}` 合并键，drag end 收合并）、右键删最近锚（半格容差）。单击已有锚点时**不移动它**——低缩放下 6 px 命中容差可能跨过整格吸附，顺手改掉才是 bug。（U12 后这些手势搬进 Edit tab 的曲线编辑器，方法改名 `curve_upsert` / `curve_remove_point`。）
- **U12（2026-10-26，spec `control-tracks.md` §8，取代 D9/D10）**：曲线编辑回到 **Edit tab**，timeline 只留只读预览。
  - **曲线类型**：`ControlClip.interp: CurveInterp{Linear, Smooth}`（serde 加性字段，`PROJECT_VERSION` 不变）。`Linear` = 现状分段线性（freehand / polyline 共用）；`Smooth` = **单调三次 Hermite（PCHIP 风格）**——内部切线取相邻割线的加权调和平均、异号归零，端点取单侧割线，**不过冲**。model `sample_curve`（`Vec`）与 engine `RtAutomation::value_at`（定容数组）同构实现 + 镜像测试。
  - **timeline 预览**：内联可编辑带退役（删 `curve_band` / `CurvePoint` / `CurveRemove` / `CurveDragEnd` / `timeline_curve_point` / `timeline_curve_remove` / `TimelineState::curve_drag`）；`lane_row` 加 `clip_body` 回调，控制行在 **clip 块内**用 `draw_control_preview()` 采样 `glitcherz_model::sample_curve` 画只读曲线（与引擎同数学），不再多一行。
  - **Edit tab 曲线编辑器**：`edit_clip: Option<EditTarget::{Midi{..}, Control{..}}>`；双击控制 clip → `TimelineAction::OpenControlEditor{track, clip}`（取代 `ShowControlCurve`）→ `control_editor_body()`。画布显示**一个 loop 周期**；三工具 `CurveTool::{Draw, Line, Curve}`（Draw/Line 写 `Linear`，Curve 写 `Smooth`，`set_clip_interp` 一步 undo）；交互沿用 `curve_upsert`（drag 合并 undo）/ `curve_remove_point`；`curve_tool`/`curve_drag` 为 app 瞬态字段（`set_control_edit_target` 按 clip 的 `interp` 同步工具）。

### CLAP 宿主（`glitcherz-clap-host`，路线图第 9 步）

- `glitcherz-test-plugin`（cdylib）：clack-plugin 单音正弦乐器 fixture（`org.glitcherz.sine`，instrument+stereo 特性），供宿主端到端测试；
- 宿主 crate（唯一允许 unsafe 的边界 = `raw::load_entry`，模块级 SAFETY 文档 + 无效文件测试）：`clap_search_paths` / `scan_clap_files`（标准目录 + `GLITCHERZ_CLAP_PATH`）/ `list_clap_plugins`（描述枚举 + instrument 特性识别）；`ClapInstance::spawn_rt(path, id, config)` 控制线程加载 → 实例化 → 激活，产出守卫 `ClapInstanceGuard`（持有动态库 + 实例，必须与节点同寿命）+ `ClapNode`；
- `ClapNode` 实现 `glitcherz-engine::realtime::Processor`：音符/参数经定容事件槽（`MAX_PENDING_EVENTS=32`）暂存，每块以 `NoteOn/OffEvent` / `ParamValueEvent` 发出（块粒度）；`ParamId::PluginParam(u32)` 已加入参数空间（自动化桥接的落点）；音频经构造期预分配的 `AudioPorts` + 立体声临时缓冲渲染回 `AudioBuffer`；首块自动 `start_processing`，任何失败输出静音（无 panic）；处理用统一 `PluginAudioProcessor`（`as_started_mut`/`start_processing` 运行时切换）；
- 分层决定：**engine 保持零 CLAP 依赖**——clap-host 依赖 engine（实现其 Processor trait）。

### VST3 宿主（`glitcherz-vst3-host`）

- 架构镜像 `glitcherz-clap-host`：扫描（单文件 / bundle 目录，标准目录 + `GLITCHERZ_VST3_PATH`）→ 非实时线程实例化（IComponent + IEditController + IConnectionPoint 互连 + 总线激活）→ `Vst3Node`；
- **参数语义差异**：VST3 参数以**归一化 0..1** 经 `IParameterChanges` 传递（CLAP 是原始值）——自动化求值器天然桥接；UI 层滑块直接工作在归一化域（plain↔normalized 换算需逐参调控制器，v1 不做，见「插件参数桥接收尾」节）；
- 乐器渲染到节点内预分配立体声缓冲；效果器自动接通输入通路（引擎缓冲 → 独立输入缓冲 → 插件 → 独立输出缓冲 → 拷回引擎缓冲），kSample32 单精度；
- 生命周期：`Vst3InstanceGuard` 持有动态库与全部 COM 对象，**必须与 `Vst3Node` 同寿命且晚于其释放**；Node 释放时 `setProcessing(false)`；Guard 释放时 `setActive(false)` + `terminate()` + 断开连接，最后才卸载动态库；
- 已集成：`VoiceSpec::Vst3{path, class_id}` 进 model 与项目序列化；UI 守卫注册表 + 乐器/效果器懒扫描缓存 + 每轨参数列表。

### 插件参数桥接收尾（效果器面板 + VST3 归一化滑块，2026-10-15）

- **效果器参数寻址**：遗留任务标注的「区分声源/效果器参数空间」由**图节点寻址**自然消解——效果器参数经 `AudioEngine::set_node_param(engine_id, ParamId::PluginParam(id), value)` 直达节点自身的 `set_param`（ClapNode/Vst3Node 早已实现），无需新的参数空间。
- **缓存键位**：效果器参数缓存（`clap_effect_params` / `clap_effect_values` / `vst3_effect_params` / `vst3_effect_values`）以**文档 node_id** 为键（跨引擎重建稳定；下发时经 `node_map` 反查 engine id），乐器缓存沿用 track 键。填充路径两条：live 插入（`load_clap_effect` / `load_vst3_effect`，`insert_effect_common` 改回 `Option<doc_id>`）、文档重建（`install_node` 的 Effect 两分支）；**E5 删掉了 live 插入这条，只剩文档重建**；清理路径三处：删除节点、`clear_external_effects`、`apply_document` 整批 clear。
- **域约定**：CLAP 滑块与自动化曲线 = 插件原始值域（min/max 来自参数枚举）；VST3 滑块与自动化曲线 = 归一化 0..1（`ParameterInfo` 不携带 plain 值域，plain↔normalized 换算需逐参调控制器，v1 明确不做——归一化滑块诚实且跨插件一致）。VST3 当前值经 S5 的 `read_param_values`（控制器 `getParamNormalized` 读回）播种会话缓存 `vst3_voice_values`（乐器）/ `vst3_effect_values`（效果器）。
- **自动化目标**：`track_external_fx(track)` 辅助统一产出效果器清单（doc_id + 显示标签 + 格式），轨道面板与自动化选择器共用；效果器参数作为自动化端点落在效果器节点上（`AutoParam::PluginParam` 域随格式）。

### 实测反馈修订（大参数量插件：Dexed CLAP 156 / VST3 2238 参数，2026-10-15）

- **扫描列表重复**：每个插件出现两次（Dexed / xstudio bridge / plugdata）——根因是扫描器枚举了工厂里全部类而**未按 category 过滤**：组件的 category 是 `Audio Module Class`，编辑控制器是 `Component Controller Class` 且常与组件同名。修复：`fallback_class_info` 只放行 `Audio Module Class`（控制器本就该经 `getControllerClassId` 定位，不靠枚举发现）；测试插件工厂补枚举控制器类（category 用规范值）钉住回归（`scanner_filters_out_controller_classes`）。
- **面板卡顿**：每帧渲染全部滑块（2238 个）+ 每帧克隆全部参数结构体。修复：乐器参数面板改**虚拟化行**（`ScrollArea::show_rows`，只布局可见行）+ 过滤框（`clap_param_filter` / `vst3_param_filter`）；取值用**下标索引 + 延迟提交**模式——闭包内只读参数表、把变更收集进局部 `Vec`，出闭包后再写缓存/发参数（既避免每帧 clone 参数 Vec，又绕开「闭包内读参数表 + 写缓存」的借用冲突）。CLAP 面板按 `module` 路径稳定排序并显示 `module / name`（Dexed 的 OP1/OP2 分组可读）；FX 面板包 ScrollArea；自动化目标菜单同样过滤 + 虚拟化（`auto_param_filter`）。
- **跨格式残留**：VST3 → CLAP 切换后 VST3 面板残留——`load_clap_voice` / `load_vst3_voice` 互删对方格式的面板缓存（`voice_params` + `vst3_voice_values` ↔ `clap_voice_params` + `clap_voice_values`）——E5 后没有切换，这段清理随函数一起删除。
- **GUI 缺口确认**：宿主无 CLAP gui 扩展 / VST3 `IPlugView`（无头运行），已入 plan.md 遗留项（需独立 spec）。

### 乐器预设系统 v1（2026-10-15）

- **存储**：`presets/<插件键>/<名字>.glzpreset`，文件内容 = **原始状态字节**（不包 host 信封——blob 格式本就插件私有，文件名即预设名）。插件键 `preset_plugin_key(format, path, id)` = `<format>-<路径 hash8>-<id 片段>`：路径 hash 让不同安装的同名插件互不串库，id 片段经 `sanitize_filename_fragment`（64/24 字符上限，非法字符 → `_`）。库根 = 工作目录下 `presets/`（dev 运行即仓库根）。
- **加载/重置 = 可撤销文档编辑**：顺序是先活体 `guard.load_state(&blob)`（插件拒收 → 报错返回，文档不动）→ `refresh_voice_slider_cache`（CLAP 守卫新增 `read_param_values`——原方法在 `ClapInstance` 上，守卫注册后 UI 够不着）→ `begin_edit` 写 `VoiceSpec.state` → `commit_edit("Load Preset")`。undo/redo 走 apply_document 重装即自然回滚。
- **出厂默认快照**：`voice_default_state`（track → blob）在 spawn 时抓取——**必须先于文档 state 恢复**（四条路径：install_node 乐器两分支 + `load_clap_voice`/`load_vst3_voice`；E5 后只剩前两条）。Reset = 活体加载快照 + 滑块回填 + 文档 state 置空（空 state = 重载即出厂默认，与 D4 语义一致）。
- **Save As = 文件操作**（同 project save 类，不进 undo）；Delete 仅对当前选中预设可用。内置声源隐藏预设行。
- **守卫定位**：`instrument_engine_guard(track)` 经乐器 doc 节点反查 engine id 再扫 `voice_guards`——不能直接按 track 匹配（效果器守卫同轨注册）。
- **后续**：效果器预设（机制可平移，缓存键换 doc id）；预设重命名/导入导出按需再加。

### Dexed .syx 预设导入（dev 工具，2026-10-15）

- **state 格式逆向结论**：Dexed 的 CLAP state = `VC2!` 魔数 + u32 LE XML 长度 + XML + `\0`。XML 根 `dexedState`（cutoff/gain/opSwitch/mpe… 属性 + `currentProgram`），子元素 `dexedBlob` 带**两个** JUCE 专有 base64 属性：`base64:sysex`（整盘 4104 字节 DX7 bank）与 `base64:program`（161 字节 `data[]`）。**加载时 `memcpy(data, program, 161)` 覆盖引擎数据——换音色必须替换 program，只换 sysex 无效**（cart 只是库存储）。
- **编码**：`MemoryBlock::toBase64Encoding` 是 JUCE 专有变体（非 RFC 4648）：字母表 = `.` + `A-Z` + `a-z` + `0-9` + `+`（`.`=0，无 `/` 无 `=` 填充，尾组零填充），格式 = `"SIZE." + 编码流`，位序 MSB-first 6-bit 组。
- **`data[161]` 布局**：`[0..155]` = 解包音色（6 op × 21：EG rate/level×8、BP、scale depth×2（+11/12 曲线位包、+13 rate scaling、+14 AMS、+15 KVS、+16 output、+17 mode、+18 coarse、+19 fine、**+20 detune**）→ pitch EG 8（126..134）→ algorithm@134 → feedback@135 → osc sync@136 → LFO speed/delay/pmd/lamd（137..141）→ lfo keysync/wave/pmsens 位包（141..144）→ transpose@144 → 名字 10 字节（145..155））；`[155]` = opSwitch 位包（运行时由 XML opSwitch 属性同步）；`[156..161]` = 控制器杂项。`Cartridge::unpackProgram` 已 1:1 移植为 `unpack_voice`（含 `normparm` 越界归一化）。
- **导入器**：`dexed_import_ym2612_banks`（`#[ignore]`，`DEXED_IMPORT_BANK_DIR` 指定 bank 目录）：校验 bank 头/校验和 → 模板 XML 双属性替换（sysex = 整盘 bank、program = 逐音色解包 + 模板尾部继承）→ `load_state` → `save_state` 回读解码逐字节校验 program → bank 首音色 NoteOn 出声断言 → 写 `presets/<键>/<bank>-NN-<音色名>.glzpreset`。`dexed_dump_param_table` 可导出插件参数表（id/名字/域）供排查。**2026-10-19 修**：sysex 属性原漏写 `4104.` size 前缀（`toBase64Encoding` 约定 `"SIZE."+编码流`），JUCE `fromBase64Encoding` 会把 `"7DL"` 当长度 7 解析 → `cart.load()` 堆越界读、cart 数据损坏（随后 `memcpy(data, program, 161)` 覆盖故不影响即时音色，但影响 program 切换与再保存）；已补前缀并重跑导入迁移 128 个 preset（现 `sysex="4104.…"`）。
- 已入库：YM2612-Dexed-Presets 四个 bank × 32 = **128 个音色**（BASS_BRASS / KEYS_BELLS / MORPH / SYNTH），预设文件为本地用户数据（.gitignore 已排除）。
- **Dexed 声音问题定位与修复（2026-10-16，诊断 harness `dexed_preset_render_profile` / `dexed_engine_playback_repro` / `dexed_live_load_race`，均 `#[ignore]`）**：导入数据无问题（`unpack_voice` 与上游 `unpackProgram` 逐字段一致、program 字节独立解码复核；引擎级单音符 velocity 127 / 长音符 4s 全程持续、note-off 正常）。修复三处——
  ① **Stop/Play 按 pitch 释放**：`Sequencer::take_pending_offs`（`std::mem::replace` 清槽带出 `(time, note)`）+ `BlockRunner::release_pending_notes`（Play/Stop 先逐轨按音符号 Off，再 `note_off_all` 兜底群发——内置 synth 的 env 忽略音符号，pitch 匹配插件（Dexed `keyup` 按 `midi_note` 查找）靠逐轨 Off；原 `Off{note:0}` 永远不匹配 → 挂死音 = "Stop 停不了"）。
  ② **循环长音符 retrigger guard**：`schedule_track_clips` 同音符重触发前先清槽发 Off（否则单槽 pending off 被覆盖丢失，Dexed 挂死复音堆积成循环杂音）；回归测试 `stop_drains_pending_offs_by_pitch` / `long_note_in_looping_clip_retriggers_with_releases`。
  ③ **`StateGate`（`glitcherz_engine::host_gate`，CLAP + VST3 镜像）**：guard 的 `save_state`/`load_state`/`read_param_values` 包裹 `begin_host_call`/`end_host_call`；`ClapNode::process`/`Vst3Node::process` 开头 `audio_should_park()`——宿主调用在飞时停靠应答、输出置零（仅原子操作）；spawn 时节点未处理不等待。**注意**：实测证明竞态并未污染 Dexed state（竞态 A（绕门）/B（走门）/C（干净对照）三组渲染逐字节一致，此前的"竞态复现"是 morph 音色自身快衰减 pluck 的假阳性——race harness 的 race 循环必须以与对照相同的 blob 结尾，否则最后加载的音色包络会被误读为"损坏"），gate 是 clack 主线程契约的加固而非实测缺陷修复。
  ④ **每轨多音符 pending-off 槽位（2026-10-19，本次 click 根因）**：`Sequencer.pending_offs` 原为 `[Option<(u64,u8)>; MAX_MIDI_TRACKS]`——**每轨仅 1 槽**。同轨第二个同时发声音符（用户实测场景：一个 clip 里的两个长音 B flat + F）的 On 会直接**覆盖**第一个音符的 pending-off，第一个音符永不 note-off；Dexed 按音高匹配释放、`Off{note:0}` 也清不掉 → 挂死 voice 堆积，听感即"琶音般的很多个正弦波 / 换 preset 后短促 click 或无声"（内置 synth 的 env 忽略音符号，故听不出该症状）。改为每轨定长多槽 `[[Option<(u64,u8)>; MAX_PENDING_OFFS_PER_TRACK=16]; MAX_MIDI_TRACKS]`：`set_pending_off` 先复用同音高槽、否则取首个空槽；retrigger guard 扫描全部槽；`take_pending_offs` 返回嵌套数组、`release_pending_notes` 逐槽释放（全程 RT-safe 固定数组）。回归测试 `simultaneous_notes_on_one_track_both_release`；端到端 `dexed_engine_playback_repro` 增补"和弦 Bb3+F3 持续发声 + Stop 后 tail peak=0"断言。
  **结论：根因在宿主的音序器 note-off 追踪，与 Dexed 插件及预设数据无关**（`unpack_voice` 与官方 `unpackProgram` 对 4 bank×32 音色 0 差异、program 字节正确、单音符引擎路径与 live-load 竞态路径均正常）。诊断命令：`cargo test -p glitcherz-clap-host <harness名> -- --ignored --nocapture`。

### 插件 GUI 宿主（plugin-gui spec，G1–G3，2026-10-20）

G1（gui 扩展宿主）/ G2（Win32 嵌入容器）/ G3（参数回接）已落地，Dexed 实测通过；决策 D1–D10 与阶段明细见 `spec/plugin-gui.md` 与 plan.md。跨版本 gotchas：

- **CLAP gui 契约**：`destroy` 即 `closed(was_destroyed=true)` 的**宿主确认**（宿主主动关闭后插件才回调 closed；spec 初稿方向写反已修）。gui/flush 调用**不过 StateGate**（[main-thread] 调用与 process 的并发安全由插件契约保证，D7 同款裁决）；GUI 编辑**不进 undo**（preset save 捕获 state 覆盖持久化，D8）；tab 切走 = hide、切轨/retire = destroy（D9）。
- **test-plugin 观察通道**：cdylib 的 static 宿主进程读不到——插件内观察状态（gui 调用序列位图 GuiCallMask、destroy 计数）经**只读参数**暴露，宿主侧 `read_param_values` 读回断言。改 test-plugin 后须显式 `cargo build -p glitcherz-test-plugin`——**dll 不随 clap-host 测试自动重建**（测试仍链旧 dll，症状是"改了没生效"）。
- **Win32 容器（`glitcherz-ui::plugin_window`，G2）**：宿主自建 STATIC 子容器、插件真窗口为孙窗口（宿主永不直接操作插件 HWND）。三个实测坑：① 容器**不能带 `WS_DISABLED`**——禁用父窗口阻断整个子树的鼠标命中与键盘焦点；② reposition **去抖**——同 rect 不再 `SetWindowPos`，否则周期性 `WM_WINDOWPOSCHANGED` 干扰插件内部拖拽（hide 后强制重放一次）；③ 嵌入区**必须在 ScrollArea 外**——原生子窗口不受 egui 裁剪，放滚动区内会留残影。
- **G3 flush 必须 raw vtable 直调**：clack 的 `PluginParams::flush` 包装要 `InactivePluginMainThreadHandle`（实例已激活拿不到）、`flush_active` 要 processor 独占 handle（processor 已移交音频线程的 ClapNode）——两条包装路都走不通；`raw::flush_params` 直调 `clap_plugin_params::flush`（与 `enumerate_params` 同模式）：空 in EventBuffer + 栈上 out EventBuffer（容量 64），解析 `ParamValueEvent` 回 `(id, value)`。
- **clack wrapper 的 flush 分派规则**：对**活跃**实例，宿主调 flush 时 wrapper 分派给 `PluginAudioProcessorParams::flush`（audio 侧实现），inactive 才走 main-thread 版本——**插件侧两个 flush 实现都要写 out queue**（只实现 main-thread 版本时活跃实例回读永远为空，且测试链的是旧 dll 时更难定位）。
- **UI 轮询时序（G3）**：`ui()` 尾部、编辑器 open 且非 `gui_closed` 时轮询 `poll_param_flush`（原子标志 take 语义），回读值同步 `clap_voice_values` 滑块缓存 + `send_track_param(ParamId::PluginParam)` 下发引擎；缓存写入与下发在 guard 借用结束之后做（借用冲突）。轮询放 `gui_closed` 的 else 分支，避开插件自毁边角。

### 键盘输入节点（G6，spec `plugin-gui.md` §8，2026-10-20；2026-10-22 改版为输入端口）

`NodeDoc::Keyboard{node_id, track, source}` 是文档里**唯一没有引擎对应物的节点变体**：不进引擎图、不建 processor、不占 slot（`install_node` 首行早退，`node_map` 里查不到它）。它的语义全靠文档的**绑定 + 连线**（"A 设计"，2026-10-22 定）：

- **绑定（A 设计）**：`source: MidiSource = Computer | Device(name)`，`#[serde(default)] = Computer`。**一源一节点**由 `validate` 强制（`validate_rejects_two_nodes_bound_to_the_same_source`）；`create_patch_node` 在已有 Computer 节点时拒绝再加一个（否则写出自己 validate 会拒的文档）。`track` 只作 patch 摆放分组，**不再是路由依据**。
- **无 fallback**：`send_to_vk_targets` 在 `vk_targets` 为空时只发一次状态提示（"cable the input node to an instrument (Ctrl+L)"）然后静音——旧的 `EngineCommand::Note { track }` 回落分支已删除。`send_note_event_at(doc, event)` 只做 `node_map` 查表，查不到就丢（节点刚删）。`--auto-play-note` 例外：它直发 `EngineCommand::Note { track }`，因为那是音频链诊断，不是路由。
- **路由纯函数**：`GraphDoc::computer_keyboard() -> Option<node_id>` + `keyboard_routes_from(vk) -> Vec<u64>`。沿 `kind == Midi && from == Node(id)` 做 BFS，最后**按文档序遍历 nodes 收集全部 Instrument**（不是 BFS 出队顺序——多目标时结果确定）。非 instrument 终点会被遍历但永不返回（MIDI 不穿透效果器）。
- **默认工程**：`default_graph_doc` 末尾追加一个 `Computer` 节点（id = 现有最大 id + 1）并 `keyboard_midi(vk, track0 voice)`——新工程开箱能弹，但那条线是可见可删的真实连线。默认模板（2026-10-29 精简）节点 = 乐器 + mixer + keyboard + 每条 audio 轨的生成器节点；连线 = MIDI 自连 + 乐器→mixer.ch0 + mixer→master + keyboard→乐器。
- **多出线（2026-10-22）**：一个输出口可拉多条 Midi 连线（多出 / 单入，与 track outlet 同规则），一次按键送到全部目标 → 无混音器即可叠音。`connect_keyboard_midi` 是「追加 + 去重 + 上限 `MAX_MIDI_FANOUT`(4)」——旧的 `retain` 同源连线把它锁成单目标，正是"只能拉一条线"的根因。上限只在 dispatch 层拦（文档 `validate` 放行，加载别人多连线的工程不会整份失败）。
- **音符号直达**：引擎 `EngineCommand::NoteAtNode { node_id, event }` → `RtGraph::node_note`（slot 线性扫描找 `node_id`，无分配；未知 id 返回 false 静默丢弃）。这条路径**绕开轨道扇出表**，因此能弹一个没接时间轴 MIDI 线的悬空 instrument。
- **UI 状态机**：`GlitcherZApp::vk_targets: Vec<u64>`（文档 instrument id，文档序），每帧 `ui()` 开头由 `computer_keyboard()` 重算；`send_to_vk_targets` 按索引循环逐个投递。**只在"掉出路由"的目标上补 Off**：新增目标不切断正在响的音，删节点/连线时旧目标全部掉出 → 全部释放（否则挂音永远收不到 note-off——与 Dexed "Stop 停不了" 同一类坑）。
- **`Ctrl+L` 快速试听**：`toggle_audition_route` 把选中的 instrument 接到 / 断开电脑键盘节点；全连 → 只删这几条，未全连 → 补缺的那几条并检查 `MAX_MIDI_FANOUT`。不重写手搭的其他连线。快捷键在全局 handler 的 Ctrl 分支里，不与演奏映射冲突。
- **屏幕键盘退役（2026-10-22）**：`keyboard_section`（on-screen 键盘 + `Keyboard — Track N` 头部 + `mouse_down_notes`/`BLACK_SEMIS`）整段删除；状态栏新增 `Keys → N synth(s) T1 T3`（空则 `none`）。副作用：键位提示不再常显（如需再补，放输入节点 tooltip）。
- **无引擎节点路径的守卫**：`create_patch_node` 对键盘节点跳过"引擎安装是否成功"的判定（否则会被判失败回滚），`delete_graph_node` / `remove_last_midi_track` 对 `node_map` 缺失的节点不做 `remove_node`/`retire_guard`。

### VST3 编辑器镜像（G7，spec `plugin-gui.md` §9，2026-10-21）

CLAP（G1–G3）的 VST3 镜像。UI 层（`plugin_window` 容器 / 面板占位区 / reposition / 销毁时序）**原样复用**，格式分派全在 guard 层——`PluginGuard::{Clap,Vst3}` 的两个 `open_editor`/`close_editor` 同签名。

- **新模块 `vst3-host/gui.rs`**（与 `host_impl.rs` 的音频线程契约分开）：`HostPlugFrame`（`IPlugFrame::resizeView` → 原子打包 w/h；**回调里不调 `onSize`**，宿主这时还没改窗口尺寸）、`HostComponentHandler`（`IComponentHandler` + `IComponentHandler2`；`performEdit` 入队，`beginEdit/endEdit/restartComponent/setDirty/startGroupEdit/finishGroupEdit` 只应答）、`EditQueue`（128 槽原子环形队列：`performEdit` 可能来自插件 GUI 线程**或**音频线程，所以只能原子操作；CAS 抢写指针，抢输时宁可丢一条也不能写坏；UI 每帧 `drain`）。
- **`NoopComponentHandler` 删除**：它的 `performEdit` 空实现正是 G7 要补的缺口；handler 现在持有 `Arc<EditQueue>`，guard 持有同一 Arc 供轮询。
- **guard 新增字段**：`view`（`Option<ComPtr<IPlugView>>`，声明在最前 → 先于 plug frame/controller 释放）、`editing`（`Option<ComPtr<IEditControllerHostEditing>>`，spawn 时 `cast` 一次）、`gui`/`edits`/`plug_frame_ptr`/`_plug_frame`。`tear_down()` 第一步改成 `close_editor()`（视图会回调 controller 与 plug frame，必须死在 terminate 之前）。
- **`open_editor` 序列**：`createView("editor")` → `setFrame(plug frame)` → 可选 `IPlugViewContentScaleSupport::setContentScaleFactor(ppp)` → `isPlatformTypeSupported(kPlatformTypeHWND)` → `attached(hwnd, kPlatformTypeHWND)` → `getSize`。**每一处失败都撤销 attach 并 `setFrame(null)`**，不留半开视图（E2 失败清理的 CLAP 同款）。返回 `ViewSize{width,height}`（物理像素，UI 除 ppp 转 points）。
- **尺寸握手（与 spec 原文相反）**：插件 `resizeView` 只落进 `GuiHostState`；UI 帧尾 `editor_resize_request()` 取走后更新 `editor.size` 并记 `PluginEditor.pending_ack`，**等容器真的 `reposition_screen_rect` 之后**才 `ack_editor_size()` → `IPlugView::onSize(rect)`。先 ack 后改窗口会让等 `onSize` 的插件按旧几何重排。
- **参数回接（插件 → 宿主）**：VST3 没有 CLAP `request_flush` 那种"宿主拉"的通道，只有 `performEdit` 的"推"。UI 帧尾 `take_param_edits()` 把 `(id, normalized)` 交给 `GuiParamBatch{is_clap:false}`，写 `vst3_voice_values` 缓存 + `send_node_param(PluginParam)` 下发引擎（归一化域与引擎桥同域，无需换算）。
- **宿主滑块 → 插件 GUI（spec 方向修正）**：正确接口是 `IEditControllerHostEditing::beginEditFromHost(id)` → `setParamNormalized(id,v)` → `endEditFromHost(id)`；`beginEdit/performEdit/endEdit` 是插件→宿主方向（`IComponentHandler`），spec 原文写反。控制器不实现该扩展时退化为单独 `setParamNormalized`。UI 出口 `sync_vst3_host_param(doc_id,id,value)`，接在三处：VST3 乐器参数面板、VST3 效果参数面板、节点内联插件参数（面板/内联互换时 GUI 与缓存一起动）。
- **测试插件**：`SineController` 加只读探针参数 **id 2 `GuiCalls`**（值 = 掩码/1024，10 位以内是精确二进制小数），掩码由 `SinePlugView` 写：attached/removed/getSize/onSize/setFrame/canResize/resizeView 达成/performEdit 达成/beginEditFromHost/endEditFromHost。`SinePlugView` 是**无窗口假视图**：`attached` 时调 `IPlugFrame::resizeView(320×240)` 并调 `IComponentHandler::performEdit(1, 0.75)`（CLAP 假 GUI `hide` 钩子的镜像）；`createView` 只认 `"editor"`，返回 `ComWrapper::to_com_ptr().into_raw()`（+1 引用交给宿主，factory 同款），并把自身 COM 指针存进 `AtomicPtr` 供回调使用（`&` 下改原子不需要 unsafe）。
- **测试**：`gui` 3 项（队列往返 / 溢出丢新 / 尺寸请求 take 一次）；`vst3-host` 3 项（`editor_view_round_trip` = attached→getSize→resize 请求→onSize 确认→removed，含重复 close 与重开；`perform_edit_reaches_the_host_queue`；`host_param_change_reaches_the_controller` 含 begin/end 位）。`parameter_enumeration_reports_gain` 期望值改 2 个参数。
- **踩坑**：`cargo test --workspace` **不会**重建 cdylib fixture（G7 首次跑测试时链到 10-16 的旧 dll，症状 = `NoEditorView` + 只有 1 个参数）。改过 `glitcherz-test-vst3` 后先 `cargo build -p glitcherz-test-vst3` 再跑测试；`test_plugin_path()` 只检查文件存在，旧 dll 不会报警。

### 反馈环路（P6，spec `node-graph.md` D10 修订，2026-10-21）

显式 **Feedback 节点**：环路必须经过它才合法，不含它的环照旧 `TopologyError::Cycle`。

- **1 块延迟 = 排序反转，不是缓冲**（本轮最省事的一刀）：编译期把「入 Feedback 的连线」视为**抽头**（tap）——该边不计入 Feedback 节点自身的入度（它构造上就是根），反而给**源**方的入度 +1，于是 Feedback 先跑、抽头源后跑，运行时抽头源缓冲里还留着上一块的输出。无额外延迟缓冲、无跨块状态、`ExecPlan` 依旧全 `Copy`。延迟 = `BLOCK` 样本（128 @ 48k ≈ 2.67 ms），写进 spec 供用户预期。
- **为何反转安全**：`F → … → X` 叠加 `X → F` 本来就是环，反转只是把这条既有的环约束换成"先 F 后 X"，不会凭空造出新环。释放抽头源的时机在**处理到 Feedback 节点时**（`checked_sub` 一次）；`checked_sub` 失败即自环（纯延迟没有源），报 `Cycle`。
- **`FeedbackProcessor` 只做 gain**：默认 0.5（裸环 unity 会持续或发散），`set_param(FeedbackGain)` clamp 0..1，unity 走 early-return fast path。文档侧 `NodeDoc::Feedback{node_id, track, gain}`（`#[serde(default)]` 默认 0.5）。
- **无环的 Feedback 同样延迟一块**：不特判"这节点其实没成环"，语义统一（否则图上"有没有环"会静默改变一处延迟，用户没法推理）。
- **删除不走旁路**：既有 D19 旁路要求 `in_count <= 1`，`bypassable` 额外排除 `NodeKind::Feedback`——旁路会把环重连成直连（多半直接变非法环），所以它的连线直接丢弃（mixer 同款）。
- **UI 接入**：patch 右键 "Feedback Tap"（`AddNodeKind::Feedback`，粉紫配色、默认落 x=600）、`inline_param_id`/`apply_inline_param` 支持 `FeedbackGain`（undo 键 `feedback-gain`）、`install_node` 装 `FeedbackProcessor` + `set_param`、`kind` 推导 `match node { Mixer→Mixer, Feedback→Feedback, _→Plain }`。
- **测试 5 项**（`graph.rs`）：`feedback_tap_is_exempt_from_cycle_rejection`（编译通过 + 断言 Feedback 排在 mixer 之前）、`feedback_tap_delays_one_block_and_applies_gain`（`y[n]=1+0.5y[n-1]` → 1 / 1.5 / 1.75 / 1.875，逐块钉住递推）、`feedback_tap_without_a_cycle_still_delays`、`feedback_self_loop_is_rejected`、`removing_a_feedback_node_drops_its_cables`。
- **踩坑**：`FeedbackProcessor` 忘了补进 `realtime/mod.rs` 的 `pub use graph::{…}`，UI 侧 `unresolved import`——新 `Processor` 类型照例要过一遍 re-export。

### 项目文件与模型（`glitcherz-model`）

- `Project`（metadata + gate + master_gain + 4×`Track`）；`Track` = 名称 + `Pattern` + `Clip`（`Note{start,length,note,velocity}` 列表 + loop_ticks，tick 基准：`TICKS_PER_QUARTER=96`/步=24/小节=384）+ 音源枚举（`VoiceSpec`: `Builtin | Clap | Vst3`）+ 混音（增益/声像/静音）+ `SynthParams` + `DelayParams`，全部 serde/RON + 字段级 `#[serde(default)]`；引擎侧镜像 `RtClip`（定容 128 音符 `Copy`，超容截断）经 `SetClip` 命令整块下发；
- `ProjectFile{version: u32, project}`（当前 `PROJECT_VERSION = 1`），`save_project`（RON pretty）/ `load_project` / `parse_project`；解析前用 `ron::Value` 探测顶层结构（含 `project`/`version` 键 → 包装格式，否则回退裸 `Project` 开发期格式——serde 会把裸格式的字段当未知字段"成功"忽略，必须先探测）；
- 程序化 `Default` 全部手写（派生 Default 会绕过 serde 字段默认值，曾导致 gate/tempo 为 0 的真 bug）。

### UI（`glitcherz-ui`，eframe 0.36 glow）

- **File 菜单**（U2 收口）：New / Open… / Open Recent ▸ / Save / Save As… / Import MIDI… / Export MIDI…，快捷键 `Ctrl+N` / `Ctrl+O` / `Ctrl+S` / `Ctrl+Shift+S`（优先于演奏键盘映射）；最近文件经 eframe `persistence` 存 RON，容量 8；窗口标题每帧按「文件名 + 未保存 `*`」重算；状态栏消息（8 秒自动消失）；`--open <文件>` 启动加载；
- 走带 + BPM、**自动化泳道**（`lane` 自绘组件：参数三选 + 启用开关 + 锚点点按/拖拽/右键删除，播放头走线，截止频率对数轴）、**混音器面板**（轨道选择/静音/post-fader 电平条/竖直推子/声像）、16 步网格（播放位置黄色描边）、**钢琴卷帘编辑器**（`piano_roll` 自绘组件：点空白绘制、拖拽移动、右键删除、滚轮滚动音高、播放头走线；fit-to-width 时间轴、1/64 吸附）、画笔音符/音长、滤波器/ADSR/延迟面板（作用于选中轨道）、虚拟键盘（屏幕 + 电脑键盘）、主输出电平表；
- 音序器音乐位置经 `Feedback.position_ticks`（AtomicF32）回传驱动播放头。

#### 文件管理（U2，2026-10-22）

- **`project_files` 模块**：只放纯函数与文件 IO（标题格式化、导出默认名、空轨判定、导入落位计划、SMF 读写包装），app 壳负责对话框 / undo / 引擎推送，因此不必起 egui 上下文即可测试。
- **导入落位**（`plan_import`）：先复用**空轨**（无音符且无 CC 任意 clip）再追加新轨，容量按 `MAX_MIDI_TRACKS`，超出部分丢弃并报数。`Project::default()` 有 1 条空 MIDI 轨，所以「新建 → 小导入」落在它里面，其余按需追加。
- **整次导入 = 一步 undo**：外层 `begin_edit` + `suspend_history`，内层 `add_midi_track()` 自身的 `commit_edit` 在挂起期间被丢弃；收尾 `resume_history` 后一次性 commit。轨道内容写入后逐轨 `send_track_clips`（不走 `apply_document` 全量重建，节点已由 `add_midi_track` 装好）。
- **`Session::is_dirty` 的可信度是前提**（此前形同虚设）：任何每帧执行的 `project_mut()` 都会置脏，于是保存后标题立刻变 `*`。三个元凶全部改为「读局部值 → `changed()` 才写」或「读取走 `project()`」——走带条的 tempo `DragValue`、Synth 面板的 master gain `Slider`、`control_editor_body` 的 `auto_endpoint` 读取。**新增每帧代码时不要直接 `DragValue::new(&mut self.project_mut()...)`**。
- **未保存确认**：`FileAction{New, Open(PathBuf), Exit}` 统一走 `request_file_action`；dirty 时挂进 `pending_file_action` 并弹 `egui::Modal`（Save / Discard changes / Cancel）。**Save 失败或用户取消保存时对话框保持打开、动作不执行**（`do_save` 因此返回 `bool`）。关窗拦截 = `viewport().close_requested()` → `CancelClose` → 答复后 `allow_close` 回放；auto-close 冒烟路径直接置 `allow_close` 绕过。modal 打开期间跳过 `handle_keyboard_input`（否则 Space 会边弹窗边播放）。
- **已知取舍**：undo 退回到已保存状态仍显示 `*`（要清必须比内容 hash，成本不值）；SMF 导入不搬文件里的 tempo（`import_smf` 只回轨道与拍号），沿用当前工程 BPM。

#### 拍号（U3a，2026-10-22）

- **模型**：`TimeSignature{numerator, denominator}`（`serde(default)` → 旧文档读出 4/4，**PROJECT_VERSION 仍 6**）；`new()` 钳制（分子 1..=16、分母向上取到 2 的幂 ≤32），`is_valid()` 才是 `Project::validate` 的严格检查。派生 `ticks_per_bar()` = `TPQ × 4 / den × num`、`ticks_per_beat()`（= 分母的音符时值，用于 `Pos bar.beat` 读数）。`TICKS_PER_BAR` 常量保留为 4/4 默认（新建 clip 的兜底），不再是唯一真源。
- **引擎镜像**：`glitcherz-engine` 不依赖 model，`ticks_per_bar_of()` 是第二份公式，由 `tests/tick_base_consistency.rs` 对锁（4/4 3/4 2/4 5/4 6/8 7/8 9/8 全比对）。`Sequencer::bar_ticks` 只在 `SetTimeSignature` 时变，改拍号**不碰 transport 位置**，只触发 `refresh_arrangement_end`（排列末尾按新小节取整）。
- **UI 侧**：`TimelineParams.bar_ticks` / `piano_roll::show(..., bar_ticks, ...)` 把文档拍号传进两个自绘视图；`snap_bar` / `knife_target` / `grab_targets` / `stretch_targets` 全部显式接收 bar 长度（原来都闭包捕获 `TICKS_PER_BAR`）。走带拍号下拉一步 undo，且**只改网格不改事件**。
- **排列上限是 tick 而不是小节**：`MAX_ARRANGEMENT_TICKS` 保持原值（4/4 下 256 小节），视图宽度按 `ceil(limit / bar_ticks)` 铺满——7/8 下能放 292 小节、3/4 下 341 小节，内容上限始终是同一段时间。
- **改拍号后的既有对象停在原 tick**：loop 端点、marker、clip 不吸附也不换算（可能不再落在大节线上）；下一次手动拖动才按新网格吸附。这是"拍号是记谱不是变换"的直接结论，别在 UI 里加"自动重排"。
- **SMF**：导出写真实 `FF 58`（`dd = log2(denominator)`，`cc=24` / `bb=8` 不变）；`import_smf` 返回 `SmfImport{tracks, time_signature}`，首个 `FF 58` 胜出。**导入策略**：`project_files::is_unwritten`（所有 MIDI 轨无音符无 CC 且无控制 clip）时继承文件拍号（此时"导入"等价于"打开"），否则保留工程拍号并在状态栏说明差异——一次导入不应偷偷重划已有内容的网格。

#### tempo 曲线（U3b，2026-10-22）

- **`Timeline In` 不是节点，是身份**：`PortRef::TimelineIn` 与 `PortRef::Master` 同级——无 node_id、不进 `graph.nodes`、无 engine processor、没有 `AddNodeKind`。单例由"不可实例化"保证，所以 `validate` 里**没有**任何"只能有一个"的规则，只有连线规则三条：指向它的线必须是 `Automation`；`AutoParam::Tempo` 只能指向它；它自己的端口表 v1 只有 `Tempo`（`AutoParam` 落成节点参数一律拒绝）。patch 里它就是一个固定 box（`PatchId::TimelineIn`，1 个入端口、0 个出端口），`LayoutDoc.timeline_in` 记住它的位置。
- **引擎用哨兵路由，不走 `slot`**：`AutoRoute.slot == TIMELINE_IN`（`u16::MAX - 1`，与 `NO_EDGE = u16::MAX` 区分）表示"这条曲线驱动时钟"。`GraphCompiler::set_timeline_in_route()` 直接写这个哨兵，不做 `node_slot` 查找；`RtGraph::auto_set_param` 见到哨兵立即返回（它永远不能落进 `slots`）；block runner 反过来需要一个**只读**的 `RtGraph::auto_route(index)` 来分辨"节点目标 / 时钟目标"。
- **顺序是功能的一半（D4）**：`evaluate_automation` 从 `advance()` **之后**移到**之前**，用块起始位置采样。放在之后会让每一块都用上一块的 tempo 跑 10 ms——曲线永远慢一拍，长 ramp 上能听出来。改这一处时同时改 `run_next_block` 的步骤注释，否则下一个人会"顺手"挪回去。
- **每块先清 override**：`evaluate_automation` 开头 `clear_bpm_override()`，再按"首个覆盖当前位置的 clip"设置。空隙里自动回落 `base_bpm`，这与普通参数"clip 外保持最后值"是**有意相反**的：时钟必须有一个确定的基值，否则一段 tempo 曲线结束后速度会永远停在最后一点（D3）。
- **borrow 拆分的写法**：在 `evaluate_automation` 里同时要 `&sequencer.control_clips()` 与 `&mut graph`，还要随后 `&mut sequencer`。用 `let Self { sequencer, graph, .. } = self;` 拿到两个字段，再把命中结果收进**固定容量栈数组**（`[(usize, f32); MAX_AUTO_TRACKS]` + 计数），最后统一投递——不要为了图省事 `Vec`，那是 RT 违规。
- **`Sequencer` 两层 tempo**：`base_bpm`（用户拧的）+ `bpm_override: Option<f32>`（每块重算），`effective_bpm()` 统一钳制 20..300（`MIN_BPM`/`MAX_BPM`，非有限值回落 120）。`play` / `stop` / `pause` 清 override；`seek` **不清**——seek 后下一块立刻按目标 tick 重采样，这正是 D4 想要的（覆盖值只活一块）。
- **UI**：patch 里从控制轨出口拖到 `Timeline In` 的 box 上直接成线（`PatchAction::ConnectTempoIn`，跳过参数弹窗——单端口没有可选项）；Synth 面板的 `Pick Parameter…` 顶部固定一条 `Song Tempo (BPM)`（`AutoTarget::TimelineIn`）。走带 BPM 字段永远显示**用户基值**，播放中若曲线接管，旁边追加 `→ 143.2 auto`（读 `Feedback.bpm` 原子）。曲线纵轴 20..300 对数轴（`lane::axis`）。
- **`param_id_of` 改返回 `Option<ParamId>`**：`Tempo` 没有也不该有 `ParamId`（它不进 `set_param`）。返回 `Some(default)` 之类的占位值会在将来某次重构里悄悄把 tempo 送进节点通路，宁可让类型系统顶住。调用点只有两处（`apply_document` 的自动化分支、`set_auto_endpoint`）。
- **测试**：model `timeline_in_accepts_only_tempo_automation`（3 条非法连线逐一拒绝）+ RON 往返（tag `timeline_in` / `Tempo`）；engine `tempo_override_overrides_base_until_cleared` / `transport_edges_drop_the_tempo_override` / `tempo_curve_overrides_the_transport_clock`（位置按 `bpm/60×TPQ/sr` 公式算，**别按 48 kHz 硬编码**——block 测试的 harness 是 44.1 kHz）/ `tempo_curve_gap_falls_back_to_base` / `no_tempo_route_keeps_the_base_bpm`（含"接线 → 生效 → 剪线 → 回落"）；patch `timeline_in_ports_and_cable_rules`。
- **默认模板（2026-10-29）**：`Project::default()` = 1 条 MIDI 轨（带内置乐器、无 delay）+ 1 条控制轨 + 1 条 audio 轨（生成器节点默认未接 mixer）+ 单通道全局 mixer。新建 MIDI 轨只加空轨道位，自动化/音频轨可由工具栏显式添加。

#### 钢琴卷帘拖动协议（`piano_roll::DragSelection`）

拖动手势**不持有音符索引**，改为「冻结锚点 + 增量发射」，两条不变量：

- **冻结锚点**：抓取瞬间把音符的 `start` / `end` 存进 `anchor_start` / `anchor_end`。每条 MIDI 命令结束都会`sort_by_key` 重排 clip，索引在第一帧之后就失效了；锚点让手势始终跟着用户真正抓住的那个音符。
- **增量发射**：每帧计算**绝对目标**，只发出 `目标 - 已累计` 的差额（`applied_start` / `applied_end` / `applied_note`）。命令侧是相对增量（`start + ticks`），若每帧重发抓取点起的总偏移会按 1+2+3+… 复利累加，音符会「飞走」。

`clamped_move` 可能吞掉部分 delta，因此累加的是**实际生效值**（从返回的 `MoveNotes` 里读回），不是请求值。`resolve_created` 改为接收新音符的 `start` / `length`（`note_add` 刚建时的几何），不再靠索引回读。

#### 命中判定必须与绘制共用同一套几何

音符的**绘制矩形**由 `note_rect()` 独占定义，`note_at()`（命中）、`note_edges()`（边缘 trim 判定）、marquee 相交测试全部复用它。绘制与命中一旦各写一份就会静默分叉：

- tick 区间判定（`start <= tick < end`）与绘制的 `max(3.0)` 最小宽度不一致——短音符看起来比它的 tick 区间宽；
- 左闭右开区间**排除了右边缘本身**，于是拖尾落在空白处，触发"新建音符"而不是拉伸末端。

命中矩形在绘制矩形之外再扩 `HIT_SLOP`（x 4 px / y 1 px）——音符末端是 1–2 px 的瞄准目标，按下点经常落在绘制矩形外侧几像素，没有宽容度就会被判成空白。`note_edges()`（trim 的视觉边缘）与绘制保持精确、不带 slop。

`edge_zone` 的两个 8 px（`EDGE_PX`）区在窄音符上会重叠，此时按**离哪条边更近**决定，避免拖头却拉伸尾。

#### 手势分类必须用按下点，不能用 `drag_started` 帧的指针位置

**这是"拖头变移动、拖尾变新建"的真正根因。** egui 要等指针移动超过 `max_click_dist`（默认 6 pt）才报告 `drag_started`，因此该帧的 `interact_pointer_pos()` 已经比按下点滑出 6+ pt：抓在音符头部的判定读成"音符中部"（→ 整体移动），抓在尾部外侧的判定读成"空白"（→ 新建音符）。移动之所以碰巧正常，只是因为音符够宽、滑出 6 pt 后仍在中部。

修法：`drag_started` 分支改读 `input(|i| i.pointer.press_origin())`（"Where did the current click/drag originate?"），命中判定、边缘判定、移动锚点全部以**按下点**为准。副作用是移动锚点也变精确了——音符第一帧就贴合指针，不再先跳 6 pt。该契约由 `tests/egui_event_semantics.rs::drag_started_keeps_the_press_point_separate_from_the_current_pos` 钉住（若未来 egui 改掉 6 pt 阈值语义，测试先红）。

`clicked()` 帧不能这么改——`press_origin` 在松开后变 `None`，click 判定继续用 `interact_pointer_pos()`（egui 的 click 要求移动 < 6 pt，位置足够接近按下点）。

#### M4a 编辑交互模型（状态 × 目标 × 限定键）

钢琴卷帘的语义决策源从"指针落点 × 修饰键"移到**离散状态**（spec 见 `spec/editing-model.md`）：常驻态只有 Select，`d` 切到粘性 Draw；G/S/T/K/Shift+D 是一次性命令族（`RollState::arm` 唤起、拖拽执行、Esc 取消、`drag_stopped` 消费）。映射矩阵由 `tests/roll_state_machine.rs` 的 22 项集成测试逐格钉住（合成事件灌进真实 egui 帧，断言发射的 intent）。

落地时撞上三个 egui 0.36 契约，全部写进了测试脚手架的注释里：

1. **合成帧的时间步长同时受两个阈值夹击**：`max_click_duration`（0.8 s）内不松开就不是 click（4 帧 click 的 press→release 跨 2 步，步长必须 < 0.4 s）；`max_double_click_delay`（0.3 s）内两次 click 会配对成双击（整次 click 跨 4 步，步长必须 > 0.075 s）。取 0.1 s/帧。用 1 s/帧会让**每次 click 都被判成 drag**（按住超时 → `is_decidedly_dragging`），症状是选择逻辑全走 drag_started 分支、`response.clicked()` 永不触发。
2. **Escape 会被 egui 的焦点机制在半路拦截**：`Memory::begin_pass` 在任何 widget 布局之前就把 `focused_widget` 清空（除非该 widget 的 `EventFilter` 认领 Escape）。卷帘要靠 `set_focus_lock_filter(id, EventFilter { escape: true, .. })` 才能收到那个"取消命令"的按键。
3. **`drag_started` 帧上 `dragged` 同时为真**：手势的装配 intent（Ctrl-drag 的 Duplicate、draw 的 AddNote）会被同帧的增量发射覆盖——一个 intent/帧的架构下，复制拖动自 M3.8 起一直是坏的（只移动、不复制，且无集成测试覆盖）。修法：`response.dragged() && !(response.drag_started() && intent.is_some())`，装配帧只装配，增量从下一帧开始。

另修两个连带问题：工具栏提示文字过长会**撑大 ui 的可用宽度**（egui 每次分配后都把 `max_rect` 扩到包含子 rect，卷帘于是比面板还宽、画到屏幕外）——加 `.truncate()`；`apply_axis` 原先先锁 `pending` 命令，但 armed Grab 的 pending 要等 `drag_stopped` 才消费，mid-drag 按 X/Y 锁的是过期条目——改为运行中的 drag 优先。

**M4a.4 实测反馈修订**（同日，Blender 式模态改定，spec §3.3 同步）：

- **G/S/T/Shift+D 改为模态**：不再"arm 后等拖拽"——按键即进入模态，选区以 ghost 半透明预览逐帧跟随指针（`Modal::frame` 纯几何计算，不写文档），LMB 落定（`Modal::finalize` 一步算出总量 intent），RMB/Esc 取消。因模态期间文档不变，选区 index 全程有效，无需快照/恢复命令；取消零成本是"预览不落盘"的直接红利。拖动回归纯 tweak 手势。
- **K 改为轨迹切刀**：stroke 沿划动路径收集切口（`knife_crossing`：斜穿行取与行中线交点，同行扫过取覆盖区间中点；触边界不切，防零长半音符），**无需预选**；切口在 `drag_stopped` 一批落下（`SplitNotes { cuts: Vec<(usize, i64)> }`）——中途落盘会让 split 的重排序毁掉后续帧的 index 测试，批量是唯一安全形态。`midi.notes_split` 签名从 `{indices, tick}` 改为 `{cuts: [{index, tick}]}`。
- **Scale pivot 改为选区 bbox 起点**（原 bbox 中心）：从起点缩放保证音符间相对关系不动（起点左侧没有音符，命令层钳制形同虚设——正确性白拿）。
- **按键卫生**：所有已处理按键 `consume_key`（modifiers 逻辑匹配，Shift+D 先于 d 消费）；`EventFilter` 扩到 `horizontal_arrows/vertical_arrows: true`——egui 的箭头焦点导航正是"上下箭头 nudge 一次后焦点逃逸"的根源；K 键 beep 疑似未消费按键在 Windows 的系统 beep，消费后待复测。新增 Delete 键删除选区。

**M4a.5–M4a.7 实测连环修订**（同日）：

- **M4a.5 ghost 可见性**：模态期间选中原音符全不透明会把半透明 ghost 盖住（S/T 起点附近最明显）。按 Blender 惯例改为原位调暗（alpha 60）、预览实色（ghost alpha 110→170）；identity 帧（刚按键未动）不触发调暗。
- **M4a.6 焦点门可视化**："D/G/T/S 全部无响应、指针正常"——排查结论是 `has_focus()` 门不可见（夹具因校准点击天然有焦点，焦点门从未被测试覆盖；代码层无回归路径）。两处落地：工具栏无焦点时显示黄色提示 chip（`RollState.had_focus` 记上帧）；焦点获取从"分类为 click 才请求"加固为**任意按下即获焦**（慢按住既非 click 也非 drag 的边角）。诊断分叉：按 D 有声=焦点门关；无声=事件没到 egui（OS/输入法层；winit 在 Windows 建窗即禁 IME，egui-winit 只在 TextEdit 获焦时开 IME）。
- **M4a.7 虚拟钢琴让键**：roll 获焦后 D/G/T/S/K 同时是 FL 布局琴键会发声。roll 把 widget id 注册进 `RollState`，app 的电脑键盘处理经 `owns_keyboard_focus(ctx)`（egui `Memory::has_focus` 实时查询，无帧延迟、无 stale）跳过 note-on；note-off 仍放行防卡音。规则一句话：**点了 roll 键盘归编辑器，点别处归虚拟钢琴**。

#### M4b 时间轴命令族（选区 → 切刀 → 模态 → 复制）

四段交付，全部复用 M4a 的模态机制与 M4b.1 的焦点管线；决策项（spec §4）用户拍板：**G=自由移动+X 锁时间；重叠=部分允许、完全拒绝；K 吸附整 bar；Shift+D 复制纳入时间轴**。

- **M4b.1 选区模型**：`ClipSel{is_control, track, clip}` 跨轨选区；跨行框选由各行上报自己段内命中、`show()` 帧末统一合并——**带必须在看到 `drag_stopped` 的行之后延迟一帧清除**（`marquee_ending`），否则同帧行序靠后的行会丢末帧命中。焦点管线对齐 roll（任意按下即获焦 + Escape filter + 焦点 chip + 虚拟钢琴门控扩展）。Delete 批删 `RemoveClipsBatch`（每轨索引降序应用防位移，一步 undo）。
- **M4b.2 切刀**：K 唤起（单次消费），点击在 `snap_bar` 处切开；两侧各 ≥1 bar 否则一帧状态提示拒绝。`split_clip_in` 纯函数：左半保留 start 缩短 span，右半携带切口之后的内容（平移到新原点，跨界音符在两半延续）插在 index+1；两半各自单周期（`loop = span`，见文末"单周期不变量"）。armed 期间指针归切刀（无拖动无框选）。
- **M4b.3 模态 G/T**：G = 时间 bar delta + 同类轨 row delta 的**刚性移动**（行 delta 全局钳制不逐 clip，组内相对关系不动）；X 切时间锁。T 默认拉终点、Shift+T 拉起点（bar 对齐、≥1 bar、不出编排边界）。ghost 经**行几何注册表**跨行绘制（目标可落同类任意行，行先注册 `lane` rect，ghost 帧行末统一画）；原位变暗 alpha 60（M4a.5 惯例）。落定校验 `filter_covered`：目标**完全覆盖**既有（非移动）clip 时该 clip 留原位+提示，批次其余照常落下（部分重叠允许）。`relocate_in`：每源轨降序 remove + 目的轨 append——**append 不位移任何剩余索引**，跨轨移动因此不需要索引重映射。
- **M4b.4 Shift+D**：克隆选区进入 grab 式模态（`ModalKind::Duplicate` 与 Grab 共享 `grab_targets` 数学与 X 锁），LMB 落定 `DuplicateClipsBatch`（内容克隆 append 到目标轨、选区转移到副本），RMB/Esc 副本不落盘。

测试抓到的真 bug：stretch 首版把 **bar 数当 tick 直接加**（`bar_delta` 未乘 `TICKS_PER_BAR`），位移量小 3840 倍——纯函数单测（指针 3 bar → 期望整 bar 对齐）当场抓住。教训照旧：几何数学全部收进纯函数并单测，UI 只做取值与绘制。

### 命令集（轨道定向）

- `SetParam`（主链）/ `SetTrackParam{track}` / `Note{track}` / `Sequencer(Play|Stop|SetBpm|SetGate|SetPattern{track}|SetClip|SetSource|SetAutomation)`；模式整块 `Copy` 数组经队列传递。

## 版本注意（gotchas）

- cpal 0.18；
- egui 0.36：`App::ui(&mut Ui)`、`egui::Panel`、`Response` 无 `pressed/released`（用 `is_pointer_button_down_on`）、`DragValue::range(RangeInclusive)`、`Painter::rect_stroke(rect, radius, stroke, StrokeKind)`；
- clack 0.2：`clack-extensions` 宿主侧 trait 需 `clack-host` feature；`Pckn::new` 需显式 u16/u32 字面量；`start_processing` 返回 `&mut Started`；`PluginInstance::new` 收 `&CStr`；`Pckn` 类型导出路径是 `clack_common::events::Pckn`（`events::pckn` 模块私有）；`ParamValueEvent::new(time, param_id, pckn, value)`；`EventBuffer` 经 `as_input()/as_output()` 包装后 `as_raw()/as_raw_mut()` 取 raw 指针（G3 flush 直调用到）；
- RON/serde：定长数组（`[Track; 4]`/`[Step; 16]`）在 RON 里是**圆括号**（serde tuple 路径），方括号只用于 `Vec`；
- VST3 参数为归一化 0..1（CLAP 为原始值），跨格式展示需换算。

### egui 0.36 焦点契约（M4b 时间轴“永远 no focus”根因）

症状：时间轴工具栏黄色 “no keyboard focus” 常亮，点击也救不回来；所有 G/T/S/K/Delete 全死；钢琴窗同套机制却正常。

根因是 **widget id 的位置相关性**：

- `Ui::next_auto_id()` 是**纯计数器**（`Id::new(next_auto_id_salt)`），不掺 ui 自己的 id；
- `ui.horizontal` / `Ui::new_child` 默认走 `IdSource::Child`：子 Ui 的 `unique_id = parent.id.with(salt).with(父计数器)`，且内部自增种子取自该值——所以**父 Ui 里任意 widget 增减，都会整体平移下面所有子 Ui 及其后代的 id**；
- `ui.push_id` **救不了**：它只给盐值，`unique_id` 仍含计数器；
- 解法是 `UiBuilder::new().id(explicit_id)`（`IdSource::Explicit`）：子 Ui 的 id 与上方一切无关。

故障链条：焦点 chip / knife·modal 提示 / 状态消息本来直接画在行的父 Ui 里，它们的**出现与消失**把每行 `horizontal` 的 id 整体挪了一位；egui 的焦点是**绑 id**的，`Focus::end_pass` 的死人开关还给刚获焦的 id 一帧宽限（`recently_gained_focus`），于是焦点永久停在一个没有任何 widget 认领的**幽灵 id**上——每行 `has_focus()` 恒假，键全死，chip 重新点亮；而 chip 点亮本身又会把 id 挪回去，形成闪烁。**点击永远救不回来**，因为救活的那一帧正是 chip 消失、id 再次漂移的那一帧。

修复（`timeline.rs`）：

- 每行改用 `ui.new_child(UiBuilder::new().layout(Layout::left_to_right(Align::Center)).id(Id::new("glitcherz_timeline_row").with(row.key())))`，行内 widget 拿到稳定的 id；
- 所有可变提示搬进 toolbar 的 `ui.horizontal` **子 Ui**（与 roll 一致）：子 Ui 只占父层一个槽，内容多少不影响下游顺带——顺带修掉“状态消息一来整排行往下跳一行”的视觉 bug；
- `owns_keyboard_focus` 增加“本帧有行真的报告了焦点”：只看 egui 的 focus id 时，幽灵 id 会让它谎报 true（从而把虚拟钢琴静音、而按键其实到不了任何东西）；
- 焦点锁 filter 认领 `horizontal_arrows`/`vertical_arrows`（roll 自 M4a.4 起如此）：未认领的方向键会把焦点“导航”给别的 widget。

守护测试 `crates/glitcherz-ui/tests/timeline_focus.rs`：用真实 egui 帧复刻 app 的面板结构（transport 含 DragValue、status bar、view tabs、CentralPanel），断言焦点能扛住①点击后的若干帧 ②自身提示的来去 ③行上方任意 widget 的增删（该用例在修复前失败）。

#### 实测追加的两课（2026-10-14，用户复测暴露）

上一节修复后真实跑 app 暴露两个独立问题，根源都是"单元测试覆盖不到的 egui 行为"：

**① `new_child` 不推进父光标 → 整个时间轴只剩最后一行**。`Ui::new_child` 的文档明说它"不在父 Ui 里分配空间"，需要调用方自己 `advance_cursor_after_rect(child.min_rect())`——`ui.horizontal` 内部（`scope_dyn`）就是这么做的。但只补 advance 还不够：`Layout::left_to_right(Align::Center)` 的**frame 会撑满可用高度**（`Layout::next_frame_ignore_wrap`：水平布局 + `vertical_align()==Center` → `frame_size.y = available_rect.height()`），`expand_to_include_rect(frame_rect)` 又把这个 frame 计入 min_rect——`ui.horizontal` 之所以没这个问题，是它先用 `allocate_ui_with_layout` 把子 Ui 高度钳到 `interact_size.y`。所以行的正确写法是：`new_child(UiBuilder::new().layout(...).id(...).max_rect(可用矩形钳到 ROW_HEIGHT))` + 行画完后 `advance_cursor_after_rect(row_ui.min_rect())`。症状：所有行叠在同一位置，只有最后画的行可见，"时间轴只剩 track4，上面全是灰的"；焦点 fixture 测试全绿——它只断言 id/焦点，从不断言行位置。守护测试 `rows_stack_vertically_instead_of_overlapping`：从 egui widget stack 读回行容器矩形，断言纵向堆叠不重叠。

**② `drag_started` 只在指针移动后才触发 → 纯点击选不中 clip**。egui 0.36 对 `Sense::click_and_drag` 做点击/拖动消歧：*"If the widget also senses clicks, this will only become true if the pointer has moved a bit"*（超过 `max_click_dist`）。M4b.1 把 clip 选中放在 `drag_started` 分支里，于是"点一下 clip"永远不进选区，G/T/S/Shift+D 全部闪一句 "nothing selected"。钢琴窗早在自己的一轮里踩过同一个坑（`piano_roll.rs` clicked 分支注释）。修复：镜像 roll，加 `response.clicked()` 处理——命中 clip 则选中（Shift/Ctrl 追加/移除），空白处取消选择（带修饰键的空白点击不动选区）；拖动路径原样保留（真拖动时 `drag_started` 照常触发，二者互斥）。规范本就写明"点击=选中、空白单击=取消"（editing-model.md 交互表），是实现没对齐规范。守护测试 `clicking_a_clip_then_g_arms_the_move_modal` 直接钉住用户报障的操作流（点击 clip → G → 模态亮起）；`TimelineState` 补 `modal_active()` 观测访问器（与 roll 的测试访问器对齐）。

**③ 状态消息一帧即逝 = 拒绝原因完全不可读（2026-10-14）**。用户复测"按 G 只见黄字一闪"——那条黄字是 `state.status` 拒绝消息，toolbar 用 `take()` 当帧抽走，人眼根本看不清内容。G 路径下只可能是 "nothing selected" 或 "mixed selection"，闪现本身证明按键已到达 timeline（焦点门是通的）、只是按 G 时选区为空。修复：状态消息改为**粘性 5 秒**（`status: Option<(String, Instant)>` + `set_status` helper，新消息顶替旧消息，过期自动清除）；同时在 toolbar 加持久的 **"N selected"** 选区计数 chip——"我的点击到底选没选中"从此一眼可判。拒绝消息若在模态 finalize 期间产生（`&state.modal` 还被借用），先存入局部 `refused_msg` 再在借用结束后 flush。

#### 命令族对齐 roll（2026-10-14，用户复测反馈）

用户复测通过 G/Shift+D 后提出三处与 roll 不一致，全部统一：

- **X/Y 轴锁**：`ModalKind::Grab/Duplicate` 的 `time_locked: bool` 改为 `locked: Option<Axis>`（`Axis::{Time, Row}`），镜像 roll 的 `Move { axis: Option<Axis> }`。Blender 约定（roll `Axis` doc 注释定音）：**X = 仅时间轴移动**（行冻结，即原 `time_locked` 的行为——原字段名"锁时间"实为"锁定到时间轴"，改名消除歧义）、**Y = 仅行轴移动**（时间冻结，新增）。同键再按解锁、另一轴接管（`apply_axis` 同款三态）。
- **T+S 拉伸**：Shift+T 退役；bare `T` 武装尾缘拉伸，模态存活时 `S` 翻转为首缘（模态存活时键盘只答限定键——镜像 roll 的 "A modal transform owns the keyboard" 规则）。
- **单次刀**：K 从开关切换改为只武装；一次 LMB 点击消费武装（成功切割 / 拒绝提示 / 空白均可——roll 的 stroke 完成即收刀同理）；Esc / 右键取消不消费。

单测 `grab_moves_by_bars_and_rows_within_kind` 更新字段并补 Row 锁断言（时间冻结、行仍动）。
#### clip 单周期不变量：resize 拖长自动平铺 bug（2026-10-20）

症状：clip 第一小节输入音符后拖长尾部，播放时音符在新增小节**自动重复**（pattern 平铺效果），预期应是空白小节。

根因是 **UI 打破了 D14 的单周期前提**：引擎调度器按 `loop_ticks` 周期在 `[start, start+span)` 内平铺音符（spec 预埋的未来特性，`sequencer.rs` 的 `m·loop` 循环），而 UI resize/stretch 只写 `span_ticks` 不动 `loop_ticks`（恒为默认 1 bar）——`span > loop` 后平铺自动生效。同一裂口还让 split 产生 `span < loop` 的**非法文档**（模型校验 `span ≥ loop`，保存即报错）。

修复 = UI 层所有改 `span_ticks` 的路径维持 `loop_ticks == span_ticks` 不变量（引擎平铺语义原样保留，`span == loop` 时天然无重复）：

- `timeline_move_resize_clip` / `timeline_stretch_clips_batch`（`lib.rs`）：写 span 同时写 loop；
- `split_clip_in`（`timeline.rs`）：两半各自 `loop = span`；`Placement` trait 新增 `set_loop` + `retain_from(offset)`——右半内容取切口之后部分并平移到新原点（**跨界音符两半延续**：右半以剩余尾巴从 0 开始；`sequence.ctrls` 与 `curve` 锚点按 `tick ≥ offset` 平移过滤），左半内容**原样保留**（超出新 span 的音符/锚点只是隐藏，拖回长度即恢复——数据永不丢）；
- piano roll / lane 绘制过滤 `start ≥ loop_ticks` 的遗留内容（防溢出画布右侧；交互早已按位置匹配 + `min(loop_ticks - 1)` clamp，超界项点不到）。

新增小节从此**空白可输入**（piano roll 宽度 = `loop_ticks` = span，拖长即扩宽编辑区）。测试：`split_halves_keep_start_end_and_content` 重写（含跨界音符断言）、新增 `split_shifts_control_curve_anchors`。引擎平铺测试 `clip_tiles_within_span_and_stops_at_span_end` 不变（未来特性仍被锁定）。
#### roll / timeline 视图导航与轨道重排（2026-10-21）

**piano roll**：`RollState` 新增 `zoom_x`（0.2–64×，对 fit-to-width 的乘子）/ `zoom_y`（0.4–6×，对 `ROW_HEIGHT` 的乘子）/ `scroll_x`（视口左缘的 tick）。几何全面参数化：`ticks_to_x = width/loop_ticks * zoom_x`、`row_h = ROW_HEIGHT * zoom_y`、`visible_rows = ROLL_HEIGHT/row_h`、**`t0_x = rect.left() - scroll_x * ticks_to_x` 是横向唯一几何源**（网格/音符/ghost/播放头/velocity lane/knife/命中全部由它派生，绘制与命中不漂移）；`note_rect/note_edges/note_hit_rect/note_at/knife_crossing` 签名改为显式携带 `t0_x/row_h`。交互：Ctrl+滚轮 = 时间轴缩放（`zoom_scroll_anchor` 纯函数锚定指针下 tick，clamp 0..loop-width）；Ctrl+Alt+滚轮 = 行高缩放（egui `zoom_delta_2d()` 的 vertical-only 分支 `(zx-zy).abs()>1e-4` 判定，纯 Ctrl 是 uniform 只缩时间轴）；Shift+滚轮 = 水平滚动（egui `horizontal_scroll_modifier` 把 delta 折进 `wheel.x`）；纯滚轮 = 纵向滚动（`top_note` clamp 下界随行高变 `rows-1`）；中键拖动 = 双轴平移（`dragged_by(Middle)` + `pointer.delta()`）。

**timeline**：`TimelineState` 新增 `row_zoom`（0.5–3×）+ `rows_rect`/`rows_viewport_min`（**上一帧**行区域屏幕矩形与视口原点）。关键时序：导航输入在 `ScrollArea::show` **之前**处理——zoom 与 offset 补偿同帧生效；若放在闭包内改 state 会被 ScrollArea end 的 store 覆盖。水平缩放锚定公式：`tick = (offset.x + ptr.x - vmin.x) / old_ppt`，新 `offset.x = tick * new_ppt - (ptr.x - vmin.x)`（`vmin` 取内容 ui 的 `clip_rect().min`，滚动方向上等于视口左上）。Ctrl+Alt+滚轮缩行高（`ROW_HEIGHT * row_zoom` 贯穿 lane_row/grab_targets 的 row_delta）；中键平移直接 load/store `egui::containers::scroll_area::State`（`egui::ScrollArea::State` 不是合法路径——State 未从顶层导出）。纯滚轮/Shift 滚轮保持 ScrollArea::both 原生行为。**网格铺满编排上限**（2026-10-21 追加）：lane/ruler 宽度 = `MAX_ARRANGEMENT_BARS`(256) × px_per_bar，不再随 clip 末端截断——G 模态 ghost / 插入 / drop 任何位置都有 bar 线可对齐，滚动范围与所有交互的 clamp 上限一致；网格与 ruler 绘制**视口裁剪**（`ui.clip_rect()` 换算 first/last bar index，ruler 端 `div_ceil(step)` 对齐刻度步长），256 bar 全铺不涨每帧绘制量。`MIN_CONTENT_BARS` 删除。

**轨道重排**：MIDI 轨头左侧 12px 六点抓手（`Sense::drag()`，hover 显示 Grab 光标，name 按钮缩 12px 让位）。拖拽期间 `state.reorder = Some(from)`，每帧 `reorder_target(ptr.y, midi_lane_rects)` 返回（插入位 `to`，指示线 y）——`to` 是**原数组坐标**的插入位置（行上半 = i、下半 = i+1、末行下 = len）；释放（全局 `pointer.primary_released()`，非 grip 的 `drag_stopped`——指针已跨行）且 `to ∉ {from, from+1}` 时发 `TimelineAction::ReorderMidiTrack`。app 层 `reorder_midi_track`：`track_permutation(len, from, to)`（old→new 双射，测试全组合验证）驱动轨表重排 + `NodeDoc::Instrument/Effect/ChannelStrip` 的 track 字段重映射（**Mixer/Keyboard/Feedback 不动**——track 仅 patch 分组，与 `remove_last_midi_track` 的排除一致）+ `PortRef::MidiTrack` 连线重映射 + `selected_track/edit_clip/timeline selection` 跟随，`apply_document()` 全量重建（undo/redo 同路径零额外代码）。

**坑（中键 pan 与左键手势串台）**：egui 的 `drag_started()/dragged()/drag_stopped()` **不区分按钮**——任何键的拖动都触发，中键 pan 落地后中键拖动会同时进入左键手势（marquee/绘制/clip 移动）。修复 = roll/timeline 所有手势入口改为 `drag_*_by(PointerButton::Primary)`（`clicked()/double_clicked()` 默认就只算 Primary，无需改；patch.rs 自 P5d.1 起一直按按钮过滤，正是先例）。守护测试 `middle_drag_never_reports_as_a_primary_drag`（egui_event_semantics.rs）锁定该契约。

**坑（模态变换失焦 + 落定无效）**：G/T/Shift+D 模态不是真实拖动——router 的 `any_down` 冻结只保护按住按钮的手势，hover-follow 照常运转（指针越过内容边缘到达 neutral chrome 即 surrender，Esc/轴限定键全灭）；且确认点击是 `response.clicked()`（组件矩形内），指针已在组件外时点击落不进任何 lane。修复两层：① `FocusRouter::set_focus_hold(bool)`——模态/armed knife 期间由组件每帧置位，`begin_frame` 跳过整个 hover-follow（与 any_down 同级的冻结；每帧设置所以模态结束自动解）；② 确认/取消改读**全局** `i.pointer.button_clicked(Primary/Secondary)`（timeline 在 finalize 段、roll 在 show() 尾部兜底——lane 内点击先被原有路径消费掉 modal，两路天然互斥；确认 frame 用 `latest_pos()` 与 ghost 预览同源）。守护测试：`focus_hold_pins_the_owner_through_neutral_hover`（focus.rs）+ `a_click_outside_any_widget_is_globally_visible`（egui_event_semantics.rs，锁定"死区点击全局可见、Response 沉默"契约）。**注意**：focus 单测里被聚焦的 id 必须有真实 widget（`ui.interact`）——egui 的 focus dead-man switch 会清掉无人认领的 id。

**坑**：PowerShell 5.1 `Get-Content -Raw` 默认按 ANSI(GBK) 解码 UTF-8 文件 + `Set-Content -Encoding UTF8` 会加 BOM——非 ASCII（→/—/·）全部乱码且不可逆（部分字符吞换行合并行）。**改源文件一律用 edit 工具或 Python（显式 `encoding='utf-8'`）**；已损坏时 `git checkout -- <file>` 恢复后重放编辑。

### 插件目录与启动扫描（U9 E1/E2，spec `plugin-browser.md`，2026-10-22）

**模块切分**：`glitcherz-ui/src/plugin_catalog.rs` 完全不依赖 egui —— 数据模型（`PluginEntry` / `PluginCache`）、缓存 diff（`plan_refresh`）、合并排序（`merge` / `sort_entries` / `view`）、编码（RON）、扫描状态机（`PluginCatalog`）与 `scan_module` 都在这里，帧循环只做三件事：画进度条、取一个模块、喂回结果。八个纯函数单测 + 两个状态机单测不碰磁盘、不碰插件二进制。

**缓存有效性**（易踩点）：
- 有效性按**模块路径**判定，不是按条目 —— 一个 VST3 bundle 可能注册多个 class，它们的 `mtime/size` 相同，必须同生共死（`plan_keeps_unchanged_module_and_all_of_its_classes` 锁定）。
- 目录集比较用**打标签的扁平字符串** `"clap:<path>"` / `"vst3:<path>"`，任一格式的搜索目录变化都触发全量重扫，但缓存字段只有一个 `Vec<String>`。
- 版本不符或目录集变化时，老的条目路径全部进 `removed`（仅用于日志），`keep` 清空。

**接线**：四个惰性 `Option<Vec<...>>`（`clap_plugins` / `clap_effects` / `vst3_plugins` / `vst3_effects`）全部删除，换成单个 `plugin_catalog`。原先 `scanned_*()` 里"首次展开菜单时全盘扫描"的整段逻辑消失——这正是原来 UI 卡数秒的根因，且 CLAP 乐器/效果各扫一遍、VST3 同理（最多 4 次全量）。现在菜单只剩查询期过滤：`plugin_entries(format, want_instrument)`（薄包一层 `view`）；空列表提示由 `plugin_empty_hint` 区分"扫描中"与"真的没有"。

**帧序契约（D14）**：扫描发生在 `ui()` 的**最后**——`self.plugin_scan_step()` 取 `pending` 队首扫一个模块，紧跟 `request_repaint()` 让队列以帧率推进（否则只有 50 ms 的显示刷新节奏，冷启动 200 个模块要十几秒）。进度条在 `ui()` 开头以 `TopBottomPanel` 画出，文案取 `upcoming()` = **队首**（下一帧才扫的那个），因此插件挂死时屏幕上已经写着它的文件名 —— 这是 in-process 扫描唯一能给的可观测性。

**持久化**：`save()` 里 `storage.set_string("plugin_cache", ron)`；部分扫完的目录也是合法缓存（下次启动 keep 已扫的、只补剩余）。eframe 退出/周期写盘，崩溃丢缓存只损失一次冷扫。

**Rescan**：patch 右键菜单底部的 `Rescan Plug-ins` → `PatchAction::RescanPlugins` → `PluginCatalog::rescan()`（塞一个空缓存，`dirs` 不匹配 ⇒ 全量）。E3 重写添加菜单时该项随之搬走。

**实测**：冷启动扫描期间日志里能看到 Cardinal 的 CLAP 描述符枚举会拉起整套 Rack 上下文（`Creating module widget ...`，约 140 ms）——正是"启动时一次性付清"优于"点开菜单才付"的实例。热启动（缓存命中）**零 dylib 加载**：第二次运行 `RUST_LOG=info` 输出中 Cardinal/Rack 日志条数为 0。

### 插件选择器与添加菜单（U9 E3，spec `plugin-browser.md`，2026-10-23）

**模块切分**：`glitcherz-ui/src/plugin_picker.rs` 只有浮层与它的纯函数（`PickFilter` / `PickChoice` / `RecentPlugin` / `matches_query` / `visible` / `visible_recents` / `remember`），数据源仍是 `plugin_catalog` 的条目表 —— picker 不碰磁盘、不实例化插件。**菜单**在 `patch.rs`（`PatchAction::OpenPluginPicker` / `PickSoundFont`），**建节点**在 `lib.rs`（`create_patch_node(NodeSource, at)`）—— 三者只通过 action 通信，patch 依旧零副作用。

**为什么是 `Area` 而不是 `menu_button`**：叶子层要放 `TextEdit` + 滚动区 + 键盘导航，egui 原生 submenu 对这三样都不友好（spec D4）。popup 用 `Order::Foreground`，`constrain_to(ctx.content_rect())` 保证不出屏。**注意 `ctx.screen_rect()` 在 egui 0.36 已改名 `content_rect()`**。

**键盘归属**（易踩点）：
- `TextEdit` 的 `event_filter` 默认包含 `vertical_arrows`，单行框也会吃掉 ↑↓；所以 picker **先** `input_mut(|i| i.consume_key(...))` 读走 ↑↓/Enter/Esc，**再**画输入框。`consume_key` 只标记 consumed（`filtered_events` 不过滤 consumed），因此真正让 Enter 不被输入框吞掉的是 `.return_key(None)` —— 单行框默认把 Enter 当"结束输入"（`transition: surrender_focus`）。
- 输入框用固定 `Id`（`query_id()`）：egui 自动生成的 id 会随 popup 位置变化，导致开帧 `request_focus` 落空、下一帧丢焦点。
- 打开帧的点击/`Esc` 必须忽略一次（`just_opened` 标志）：开菜单那一下的 pointer click 与新 popup 的"点击外部关闭"是同一帧同一事件，不设这道闸，popup 会自己关掉。
- picker 打开时 `handle_keyboard_input` 整段跳过，否则空格（播放）/Delete 会穿透到走带与 patch。

**行渲染**：22 px 的 `allocate_exact_size` + `Sense::click()` 自绘背景，`ui.new_child(max_rect)` 里左 `Label::truncate()`（宽度 = 总宽 − 格式标签宽）、右 `right_to_left` 布局放 `CLAP`/`VST3` 小字标签；`response.on_hover_text(path)` 给全路径。Recent 段与 All 段共用同一份行列表，键盘高亮索引因此天然连续（`rows` 每帧重建，选中索引 clamp）。

**单次 install（D1）**：`NodeSource` 扁平枚举（`BuiltinSynth` / `BuiltinDelay` / `SoundFont` / `ClapInstrument` / `Vst3Instrument` / `ClapEffect` / `Vst3Effect` / `Mixer` / `Feedback` / `Keyboard`）直接给出 `VoiceSpec`，`create_patch_node` push 文档后**只调一次** `install_node`。旧路径"先建内置、再从面板换装"的两次 install + 一次回收消失。

**失败即回滚**：`install_node` 对插件加载失败是**静默降级**（回落到 SynthProcessor / DelayProcessor），因此新节点若"声称是插件"却在 `voice_guards` 里找不到自己的 engine_id，就当失败处理：`session.restore(before)` + 状态栏报错，文档不留空壳。SoundFont 不在此列（缺文件回落到内置声源是既定语义，`sf2-loader.md` D8）。

**Recent（D10）**：`UiState.recent_plugins`（`#[serde(default)]`，跨会话）。`remember()` 按 `(format, path, id)` 去重后插到队首，再按 `is_instrument` 各自截断 5 条。点 Recent 里的条目即使插件已被卸载也会尝试实例化 —— 失败走上面的回滚路径并报错，比藏起来更可诊断。

**测试**：8 项单测。4 项用 `ctx.run_ui(RawInput)` 跑 headless 帧（键盘选取、Esc 取消、Recent 置顶、开帧点击不自关），不需要窗口/显示后端 —— 与 `tests/egui_event_semantics.rs` 同一套路，可回归"popup 在真实帧循环里能画出并返回结果"。

**E4 已补齐**：连线中点菜单改为 `Insert ▸ Effect ▸ …` + `Insert ▸ Mixer`，picker 增 `PickerPlace::Insert`（见下一节）。

### 连线中点插入复用 picker（U9 E4，spec `plugin-browser.md`，2026-10-23）

**同一 popup 两个落点**：`PickerPlace`（`Add { at }` / `Insert { cable, at }`）挂在 `GlitcherZApp.picker_place` 上，`PickFilter`（类别 + 格式）仍是列表筛选。`PatchAction::OpenPluginPicker` 里 `at: Pos2` 换成 `place: PickerPlace`，菜单与 picker 因此不需要第二份浮层代码；`PickerPlace::at()` 给需要坐标的调用点留一个统一出口。

**菜单**：`Cable ▸ Insert ▸ Effect ▸ {Built-in Delay, ──, CLAP…, VST3…}` + `Insert ▸ Mixer`，音频线才有 `Insert` 分支；MIDI / 自动化线只剩 `Delete Cable`。和 `Add` 菜单同一套"先类别后来源"的形状，两处出生节点的地方读起来一致。

**三个必须守的约束**：
1. **归属轨道**（修复既有缺陷）：`create_patch_node` 原先把 `selected_track` 写进节点的 `track` 字段，插进别的轨的链路时归属就错了。拆出 `create_patch_node_on(source, at, track)`，插入路径用 `graph.node_by_id(上游节点).track()` —— 处理器永远跟着被插的链路走，`register_track_chain` 也刷对那张表。
2. **一步 undo**：建节点、断旧线、接两根线各自 `commit_edit`，不合起来就是 4 步。用 `session.suspend_history()` 包住这三段、末尾 `commit_edit("Insert Node", before)` 提交一个快照（与 `Import MIDI` 同一套路）。`create_patch_node_on` 失败时先 `resume_history()` 再返回，靠它自己的 `session.restore` 把文档退回原位，线一根没动。
3. **槽位不变**：纯函数 `splice_endpoints(&CableDoc) -> Result<(上游 node, 下游 PortRef, in_port), &str>`，`A → B` 变 `A → new → B` 时下游的 `in_port` 必须原样搬过去（D17，mixer 推子与线缆的绑定）。非音频线、`from` 不是节点的线直接报错，比原先散在函数体里的三处 `if` 好测。

**可插入源白名单**：`NodeSource::fits_audio_insert()` —— `BuiltinDelay` / `ClapEffect` / `Vst3Effect` / `Mixer`。乐器进音频线会没有 MIDI 输入、永久静音，所以不是"能跑就先允许"。picker 侧已经用 `NodeCategory::Effect` 筛选，这层是动作层的兜底。

**测试**：`splice_tests` 5 项（槽位保留、master 目标、MIDI 拒收、无上游节点拒收、可插入源白名单）。App 级断言（`node_map` 只加 1 条、`retiring_guards` 不增长）无法在无窗口环境下做 —— `GlitcherZApp::new` 需要 `eframe::CreationContext`，仓库也没有 headless app harness，故以 `cargo run --seconds 5` 冒烟 + 代码结构（单次 `install_node` + 无回收分支）代替。

### Inspector 收口（U9 E5，spec `plugin-browser.md` §8，2026-10-23）

**删掉的两块**：① 乐器源选择行（`Built-in Synth` / `CLAP Instruments…` / `VST3 Instruments…` / `SoundFont…`）；② 同段的 `CLAP Effects…` / `VST3 Effects…` 快捷加载。删完之后 Inspector 只剩 preset / 参数栈 / `Show Editor` / 内联参数 —— D2 的"结构归 patch、参数归 Inspector"这才真正成立。

**保留的两块（容易误删）**：SoundFont **preset** 下拉（`Bank n` + `<program>. <name>` 两个 ComboBox）是术语表里的 preset，不是源选择；`Clear All Effects` 不是选择器而是批量删除，挪到效果节点列表头部（`fx_track_view`，即面板没有窄化成单个效果节点时；窄化时标题会被读成"清掉这一个"）。

**连带删掉的死代码**（"换源 = 删了重建"的直接结果，D3）：`set_builtin_instrument`、`load_soundfont_voice`、`load_clap_voice`、`load_vst3_voice`、`insert_effect_common`、`load_clap_effect`、`load_vst3_effect`、`plugin_entries`、`plugin_empty_hint`、`node_track`。这些都是"已存在节点换处理器"或"隐式插到轨链尾"的路径，E3/E4 之后它们既无人调用也与新语义冲突：`insert_effect_common` 会绕过 D6 悬空出生、把效果器悄悄接到 mixer 前面，正是 D12 要消掉的隐式捷径。净减 **549 行**，`node_map` 只有 `install_node` 一个入口，`cargo clippy --workspace --all-targets -D warnings` 无 `#[expect(dead_code)]` 兜底。

**读取顺序提醒**：本文档前面几节（S5 面板、插件参数桥接、乐器预设、插件状态持久化）里出现的 `load_clap_voice` / `load_vst3_voice` / `load_soundfont_voice` / `load_clap_effect` / `load_vst3_effect` / `insert_effect_common` 都是 E5 之前的**历史路径**；现在的活体装配只有 `install_node`（`apply_document` / undo / redo / 加节点 / 插线中）一条。

**验证**：全仓 497 测试绿（删除全程没动测试），`cargo run -p glitcherz-app -- --seconds 5` 首帧正常、按时退出。Dexed（CLAP）与 200+ VST3 目录的手工验收仍按 spec §10 门槛留给人工过一遍。

### Inspector 收口：改名、mixer 分派、编辑器折叠头（U8，2026-10-23）

**改名**：`MainView::Synth` → `Inspector`，`synth_panel` → `inspector_panel`，tab 文案 `Synth / Mixer` → `Inspector`。面板标题不再带通道前缀（`Synth — Instrument #3` → `Instrument #3`），因为面板身份已由 tab 表明。

**面板只剩"选中节点的参数"**：三处结构管理搬走——① mixer 控制台改 `mixer_section(ui, mixer_doc)`，只有 `target_doc` 是 `NodeDoc::Mixer` 时才画（原先无论选什么都会画一遍"first mixer"）；② 混音台头部的 `+ Track` / `- Track` 删除（timeline 工具栏本来就有同名按钮，且那是轨道管理不是混音参数）；③ `automation_section` 拆解：`automation_strip`（新增，timeline 底部 `egui::Panel::bottom("glitcherz_automation")`）+ `add_control_track` / `remove_control_track` + `auto_target_row`（端点选择器，原样搬过来，`index` 改成入参）+ 删除 `clip_section` 死文案。曲线管理是轨道级状态，跟轨道放一起。

**`Panel::bottom` 的声明顺序**：必须写在 `timeline::show` **之前**（面板先占位，再让内容吃剩余高度）；`timeline::show` 内部的 ScrollArea 是 `auto_shrink(false)`，放后面会把 strip 挤出屏幕。

**插件编辑器改折叠头**（原来 `Show Editor` / `Close Editor` 按钮，展开后原生子窗口会盖住按钮）：

- 用 `CollapsingHeader::new("Editor").id_salt("plugin_editor_header")`，三角独占一行在占位矩形上方，原生窗口只能覆盖 body 的 rect，永远压不到开关。
- **`.open(Some(self.editor_user_open))`**：让 app 的 `editor_user_open` 成为唯一真值。若交给 egui memory 自己记，插件自毁 GUI（`gui_closed()` → 清 flag）那条路径会把 header 留在展开态，下一帧"header 开着但 flag 是 false"就把它又拉起来——死循环。强制 open 后点击不再自动 toggle，改由 `header_response.clicked()` 手动翻转 flag（`close_plugin_editor()` / `plugin_editor_wants_open = true`）+ `request_repaint()`，下一帧 header 跟上。
- body 只在展开时布局：会话可能比点击晚一帧（`plugin_editor_wants_open` 在本帧尾部才被消费），因此 body 里用 `if let Some(editor) = ... && Some(editor.doc_node) == target` 兜住 `None`，不画占位文字（避免一帧闪烁）。
- 折叠后立刻 `close_plugin_editor()`，不存在"隐藏但仍然加载"的中间态；`editor.rect` 留旧值无害（会话已销毁）。

### 跨平台构建：VST3 枚举类型 + eframe 的 Linux 后端（2026-10-23）

症状：Windows 通过，`ubuntu-latest` / `macos-latest` 各自报一类错（CI 从没绿过）。

**① `vst3` 0.3 的枚举类型按平台分叉**：生成的绑定把每个枚举写两遍 —— 常量容器（`Vst::MediaTypes`、`Vst::BusDirections`、`Vst::ProcessModes`、`Vst::SymbolicSampleSizes`、`Vst::Event_::EventTypes`、`Steinberg::IBStream_::IStreamSeekMode`、`PClassInfo_::ClassCardinality` …）的类型是 `vst3::support::DefaultEnumType` = `c_int`（Windows）/ `c_uint`（其他），而**接收这些值的字段与接口参数一律写成 `int32`**（`ProcessSetup::processMode`、`ProcessData::symbolicSampleSize`、`IComponent::getBusCount(type, dir)` 等）；也有少数常量的类型本身就是 `int32`（如 `ParameterFlags`），天然对齐、不用转换。Windows 上两者都是 `i32` 所以看不出问题，macOS/Linux 上是两种类型，`cargo build` 直接 `E0308`。

接缝收在两个 `compat` 里（宿主 `src/compat.rs`、fixture 内同名小函数）：`to_int32`（常量 → `int32` 槽）/ `to_uint32`（`BusInfo::flags` 这类 `uint32` 槽）/ `to_enum`（`int32` 参数 → 枚举类型，好在 `match` 里直接对常量分支）。泛型走 `TryInto`/`TryFrom`，两个方向都能编译，不必写 `#[allow(unnecessary_cast)]`——`as` 在 Windows 上是多余的、在 Linux 上是必需的，正是 `clippy -D warnings` 会卡住的那种差别。判 `match` 的入参一律先 `to_enum(...)` 再对常量，避免 `m if m == ...` 这种守卫写法。

**② eframe 关掉默认 features 会连 winit 的 Linux 后端一起关掉**：workspace 里 `eframe = { default-features = false, features = ["default_fonts"] }`（为了不拉 wgpu），`glitcherz-ui` 只补了 `glow`/`persistence`，于是 `x11`/`wayland` 两个 feature 也没了 —— winit 在 Linux 上 `compile_error!("The platform you're compiling for is not supported by winit")`。`glitcherz-ui` 现在显式带 `wayland` + `x11`（在 Windows/macOS 上是空操作），`Cargo.lock` 因此多出 X11/Wayland 栈（另有一批只进 lock、不进构建图的 `egui-wgpu`/`wgpu` 条目，无影响）。`winit/default` 附带的两项没跟着开：`wayland-dlopen`（免运行期 `libwayland` 硬依赖）与 `wayland-csd-adwaita`（GNOME Wayland 下自绘窗口边框）——CI 装了 `libwayland-dev`，链接期没问题；要在 GNOME 上跑得舒服再单独补 feature。

**验证方式**：Windows 上无法直接编译 Linux/macOS，`cargo check` 交叉 target 又会卡在 `alsa-sys` 的 pkg-config。用 `[patch.crates-io]` 指一份把 `DefaultEnumType` 改成恒为 `c_uint` 的 `vst3` 副本，就能在 Windows 上跑出 macOS/Linux 的 `E0308`，改完即绿；feature 侧用 `cargo tree --target x86_64-unknown-linux-gnu -e features -i winit` 确认 `x11`/`wayland` 真的上来了。

### Master 组上走带条：立体声表 + Panic（U8 收尾，2026-10-23）

**为什么挪**：Inspector 的契约是"只显示选中节点的参数"，可它底部压着 `Master Gain` 与 `Master Out Level` —— 两个全局量。视觉上它们紧贴 `Cutoff Freq` / `Volume Envelope` 那组节点参数（同在一个 `Grid` 里），很容易被读成"master 的滤波/包络"。现在 master 组进 `transport_panel`（`master_section`），`Panel::top` 不管切到哪个 tab 都在，符合"走带条上的东西属于整首歌"。Inspector 侧只剩节点参数，并在 `Grid` 前加了一行 `Node Parameters — filter / envelope / delay of the node above, not the master bus`。

**立体声峰值链路**：`GraphLevels.master_lr: [f32; 2]`（新增 `slot_peak_lr`，逐声道扫；`master` 仍保留为两者的 max，历史单值调用点与测试不动）→ `Feedback.{level_l, level_r}` → `AudioEngine::master_peak_lr()` → 两个 `PeakMeter`（各 600 ms hold）。UI 侧自绘两条竖条：`-60..0 dB` 映射，`-9 dB` 以下绿、以上琥珀、`-1 dB` 以上红（红=过载是音频表惯例，与股票红绿无关）。用 `ProgressBar` 做不到——它只有一条且是横向的。

**Panic 为什么不发 `Off { note: 0 }`**：`note_off_all` 的毯式 off 到不了按 pitch 匹配的插件（Dexed 的挂音问题已记在 SF2/CLAP 两节）。真实释放要逐 pitch 走 0..127，但 **CLAP / VST3 两个宿主的 pending 队列都只有 32 条**（`MAX_PENDING_EVENTS`），一个 block 内塞 128 条会静默丢事件。所以 `RtGraph::panic()` 只设 `panic_sweep = Some(0)` 游标，`process_block` 开头每块发 `PANIC_SWEEP_BATCH = 24` 个 pitch 再推进（24 而非 32，给同 block 的常规事件留余量）：128/24 = 6 块，48 kHz / 128 frames 下约 16 ms。`EngineCommand::Panic` 额外先 `release_pending_notes()`（按 pitch 释放音序器已取走的待关音符）。**语义是静音不是停止**：transport 继续跑、参数一个不动，`--seconds` 冒烟与 `panic_sweeps_every_pitch_across_bounded_batches`（断言 128 个不同 pitch 全部到达）锁定。

**验收坑**：master gain 的 `Slider` 用 `custom_formatter` 显示 dB（线性 0..1.5 → `-∞..+3.5 dB`）；拖动仍走 `begin_param_edit` / `commit_param_edit("master-gain")` / `drag_stopped` 三步，否则一次拖动会塞满 undo 栈，或者每帧写文档让标题永远带 `*`。
### 采样播放器 instrument（U5，spec `sample-player.md`，2026-10-24）

**模块镜像 SF2 布局**：`engine::sample`（非实时，hound 解码）+ `realtime/sample.rs`（RT processor），装配走 `install_node` 唯一入口。`SampleData` 把任何位深（int 8/16/24/32 经 hound 统一 `samples::<i32>` 有符号化 + 归一化，float 32）解码为 f32 立体声帧盒；**交错流组帧必须整块 `chunks_exact(2)`**（立体声）或 `into_iter().map(|v| [v, v])`（单声道），逐样本 push 会把右声道丢掉。`SampleLibrary` 是 path → `Arc<SampleData>` 的多样本会话缓存（与 sf2 单库不同：采样器一轨一样本），`obtain_sample_voice` 未命中才解码。

**RT 内核三个易错点**：

1. **步进系数合一**：`rate = 2^((note−root)/12) · wav_sr / engine_sr`——音高比率与采样率补偿是同一个 per-engine-frame 步进，不单独做重采样预处理；线性插值读取。voice 触发时 rate 定格（root note 参数变更只影响后续 note-on，sampler 惯例）。
2. **OneShot 与毯式 off 的区分**：`Off { note: 0 }`（`held` 位图 key 0 未按住）= blanket sweep → **所有** voice 进 fade（含 OneShot，Stop/Panic 必须能停）；pitch-matched off 只进 Gate voice。voice 存 `one_shot` 快照（触发时的 play mode）。
3. **池回收**：`voices[0..active_count]` 紧凑区，块尾 swap-compact（`!active` 的与尾部交换，交换后不推进索引重查）；池满偷 age 最大者（age 每块 +1）。渲染写预分配 scratch 再 `copy_channel` 覆盖 buffer（生成器语义，同 SF2）。

**测试方法论（两条坑）**：① RT 审计的 `AuditScope` 内不许建 `AudioBuffer`——构造分配会被计入（3 次假阳性）；② ramp fixture 逐块上升、块间峰值不可比，**增益/衰减/复音堆叠断言必须用恒幅 fixture**（`flat_fixture`），"输出 = 源 × rate" 数学断言才用 ramp（整数游标处 frac=0 插值精确）。复音上限验证技巧：同 note 触发 16 次（同样本同 rate 同相位）→ 峰值 = 16 × 单 voice；第 17 次后仍 16×即证明偷音。

**UI 接线**：`Add ▸ Instrument ▸ Sample…`（`PatchAction::PickSample` → rfd wav 对话框 → `NodeSource::Sample` → `create_patch_node` 一步 install）；Inspector `sample_section` 挂在 sf2 preset picker 之后（文件名 / root DragValue / play mode ComboBox / gain Slider，root+gain 走 `begin_param_edit("sample-params")` 手势合并，mode 是离散一步 undo）；patch 内联 `SAMPLE_INLINE`（root+gain）经 `apply_inline_param` 与面板同源；节点标签 `SAMPLE {stem}·{note名} · Track N`（`note_name`：`60 → C4`）。install_node 文件丢失回落 `SynthProcessor`（文档字段保留），`Effect + Sample` 防御性回落 Delay。`param_id_of`/`AutoParam` 未加 Sample 变体——automation 菜单的 Cutoff 选项对 Sample voice 不响应（与 SoundFont 同现状，spec §6 明确不做）。
### Audio track（U6，spec `audio-track.md`，2026-10-24）

**播放数学完全闭合在 tick 域**：帧位 = `(song_tick − start)/span × wav_frames`——比例映射是**纯函数**，loop 回卷 / seek 跳中段零游标零特判（位置即状态）。拖长 clip = 变速、tempo 变化 = 变调（磁带效应，用户确认取舍）。每输出帧 j 的 clip tick = `song_ticks + j·dt/frames`，线性插值读 wav（U5 `render_voice` 内核平移）。stop/pause 静音由 `ctx.transport_active` 门控——**没有它，冻结的 song_ticks 会每块重放同一帧区间**（循环卡带）。

**非 Copy 数据的 RT 通道设计**（`Arc<SampleData>` 不能进 `EngineCommand`——其 Copy 是铁律）：

- **clip 表走 structure 队列**：`GraphUpdate::SetAudioClips{node_id, table}`（结构队列本来就传 `Box<dyn Processor>`）。`RtGraph::set_audio_clips` 按 node_id 线性扫 slot → 调 **`Processor::swap_audio_clips` trait 默认方法**（None = 未消费，调用方把传入表进 trash）。
- **displaced 表走 trash 队列**：`mem::replace` 是 move，音频线程零 dealloc；旧表 push `rtrb::RingBuffer<AudioClipTable>`（容量 64），控制线程 UI 帧尾 `engine.reclaim_audio_clips()` drain+drop。满则 park 单槽（`audio_trash_pending`）下块重试；park 被占的理论满 `mem::forget`（有界泄漏）优于 RT dealloc——唯一生产者（UI）同时是唯一 drainer，逻辑不可达。
- **`rtrb::push` 失败把值还回来**：`Err(PushError::Full(v))` 解构取回，不是消耗。

**ctx 构造顺序**（block.rs）：automation evaluate **之后**、advance 之前——`song_ticks` = evaluate 前的 position（本块起点）、`transport_dt_ticks` = evaluate 后 bpm 的步进（与 advance 内部同公式，`Sequencer::transport_dt_ticks` 公开）、`transport_active` = is_playing。**顺序错了 tempo 曲线就滞后一块**。

**`swap_audio_clips` 放错 impl 块的静默失效**：曾写在 `impl AudioTrackProcessor`（固有块）而非 `impl Processor`——trait 走默认 `None`，`set_audio_clips` 永不消费表，编译零警告（固有方法被测试用到不算 dead）。修复后加 **trait 路径测试**（`Box<dyn Processor>` 上调 `swap_audio_clips` 断言 Some）锁定——这是 trait 默认方法 + 固有同名的经典陷阱。

**timeline 三轨化**：`is_control: bool` → `TimelineLane{Midi, Control, Audio}` 枚举（`ClipSel`/action 字段全线 lane 化，编译器强制补全）。行交互全靠 `Placement` trait 对 `AudioClip` 的实现：`set_loop` no-op（loop==span 恒成立）、**`retain_from(offset)` = start += offset; span -= offset——split 右半段自动播 wav 后半**（比例映射的语义红利）。M4b modal（G/T/Shift+D）对 audio 关闭：`arm_modal` 过滤 Audio 选区（两表推理逻辑不动，v1 边界）；K 切刀/点选/框选/拖动/resize/右键删/批删全继承。**lane_row 内变量遮蔽坑**：`let (is_control, track) = row.key()` 脚本替换成 `let lane = ...` 后与行内 Rect 变量 `lane` 冲突——枚举值改名 `lane_kind`（块级遮蔽无碍，函数级遮蔽会炸掉后续所有 Rect 用法）。

**波形"所见即所听"**：`SampleData` 加载期算 peaks（256 帧/列 min/max，双声道折叠）；`draw_waveform` 列 j ↔ peaks 子区间 [j/n,(j+1)/n) 的合并极值——与 RT 读取同一比例映射。UI 帧内波形源 = `SampleLibrary::get`（**只读查找，帧内不解码**——`&dyn Fn(&str) -> Option<Arc>` 不能是 FnMut，load 的 `&mut self` 进不了闭包）。

**建 clip 的 span 换算**：wav 秒 × bpm/60 × TICKS_PER_QUARTER → ticks（clamp ≥1 bar、≤ 排列边界 − start）。
### Export Audio 离线渲染（U7，spec `export-audio.md`，2026-10-24）

**离线引擎 = 同一装配减去 cpal**：`AudioEngine::start_offline(sample_rate)` 与 `start()` 唯一的差别是不建流——`_stream: Option<Stream>` + `runner: Option<BlockRunner>`（实时把 runner move 进闭包后字段留 `None`；离线留 `Some`，`render_offline_block` 转发 `render_interleaved`）。无设备依赖 → headless/CI 可跑。装配/命令/结构队列全部复用，离线引擎的高层 API（add_node/connect_nodes/set_auto_route/send_audio_clips…）与 live 完全同路径。

**`SetArrangementFloor`（本 feature 新增的引擎命令）**：D16 自停的 `arrangement_end` 只统计 MIDI/control clip（`refresh_arrangement_end`），audio clip 终点更晚时自停先发生 → `transport_active=false` → AudioTrackProcessor 静音切断。floor 让 `arrangement_end = max(内容, floor)`。live 引擎永不推（零影响）；导出装配推 `export_end_ticks`（全 clip 终点 max）。**任何"sequencer 内容之外还有时间轴内容"的后续特性（录音？）都要重访这个坑**。

**Play 的 `note_off_all` 吞同块 note**：`apply_command(Play)` 会群发 blanket off——FIFO 队列里 Play 之前发的手动 Note 在**同块**被消费：note 触发 envelope → 紧跟 note_off_all → release 从 level≈0 开始 = 瞬间静音。**导出无此坑**（音符全部来自 clip 调度，advance 在 Play 生效后的块里跑）；`offline_engine` 测试的时序注释锁定该认知。手动 note 测试必须：装配块 → Play → **再渲染一块** → Note。

**离线装配 = `spawn_offline_node`（不是 install_node 的参数化）**：install_node 一半代码是 UI 缓存记账（clap_voice_params/voice_default_state/voice_guards——**离线 guard 混入主列表会被 engine_id 撞号误回收**）。独立 spawn 只走声音路径（构造/spawn+load_state/构造期 set_param），guard 收集到 job 本地的 `Vec`，随 job drop 统一释放。VST3 的 `guard.load_state` 是 `&self`（StateGate 内部处理）、CLAP 是 `&mut`——`Ok(instance)` 的 mut 两家不同。

**帧间分片**（U9 扫描先例）：帧尾 `pump_export` 渲染 34 块（~90ms 音频）+ `request_repaint`；进度 = `musical_ticks / (end-start)`；到达终点 `pause()`（停在 end，tail 期间 transport_active=false 只放效果尾——audio 内容按设计止于 end）。空工程 window=(0,0)：Play 后立即 pause，只渲染 1s 尾音（空文件带尾）。Cancel 直接 drop job（半成品文件留给用户）。**writer finalize 才写 RIFF 头尺寸**——取消的半成品 wav 头是错的，可接受（文档写明）。

**测试口径**：ADSR 默认 release **0.2s**（≈96 块@48k/128），不是采样播放器的 10ms——"Stop 后静音"断言要给足块数；探针（临时 debug 测试 + println + assert!(false)）是定位"链路哪一环无声"的最快手段，删净即可。

### clip 级 controller 事件（P0，spec `roll-multiclip.md`，2026-10-27）

**`Sequence.ctrls` 此前是四段全空**：模型自 M1.3 就有 `CtrlEvent`/`CtrlKind`（含 `PitchBend`），但 SMF 导入的兜底分支是 `_ => cursor.skip(2)?`（CC/bend/aftertouch/program **全部丢弃**）、导出只写 `0x90`/`0x80`、`RtMidiClip` 只有 `notes`、UI 无编辑入口——所以 P0 的实际范围是"导入解析 + 引擎调度 + 导出写入"三段，不是 spec 最初写的"导出补齐"。

**`advance` 的回调换成 trait**：原先 `emit: &mut dyn FnMut(usize, NoteEvent)`；加 ctrl 若再加一个闭包参数，block runner 需要两个闭包同时持有 `&mut RtGraph`（两次可变借用，编译不过）。改为 `sink: &mut dyn SequencerSink`（`note` / `ctrl` 两方法，`ctrl` 默认空实现让纯音符 sink 保持单方法），block runner 侧 `GraphSink(&mut RtGraph)` 一个适配器把两类事件送进同一张图。

**同 block 内的顺序是 ctrl → note**：pitch bend / program change 设定的是"音符起始状态"，晚一块到就是错音高/错音色。调度里 ctrl 循环放在 note 循环**之前**；SMF 导出同样把同 tick 顺序定为 off(0) → ctrl(1) → on(2)（`SmfEvent::order_rank`）。两者对齐，导入导出往返后行为一致。

**`write_track` 必须按 status 决定 data byte 数**：原格式 `(tick, status, d1, d2)` 固定写 3 字节；`0xC0`（program）与 `0xD0`（channel pressure）只有 **1 个** data byte，多写一个会让后续每个事件错位（文件仍能被"解析"，只是全乱）。改 `SmfEvent{tick,status,data1,data2,data_len}` + `two()`/`one()`。回归测试 `single_data_byte_ctrls_do_not_shift_later_events` 用 program→pressure→note 序列锁定。

**导入的 `origin` 改为"最早事件"**：原来只看 `notes.first()`；controller-only 的轨（只有弯音没有音符）会落 origin=0 且 span=1。现在 `min(note.start, ctrl.tick)`，span 也把 `ctrl.tick + 1` 算进去。

**`RtCtrlKind` 不能用 `#[derive(Default)]` + `#[default]`**：`#[default]` 只允许 unit 变体，首变体是 `Cc(u8)` → 手写 `Default`（`Cc(0)`），供 `[RtCtrl; MAX_CLIP_CTRLS]` 的数组初始化。

**定容与内存（2026-10-28 已作废，见下条）**：~~`MAX_CLIP_CTRLS = 32`…队列按元素大小预分配故可接受~~——该取舍正是 64 音符上限的成因，D15 修订后定容数组整体退场。

### clip 表改走结构队列（D15 修订，2026-10-28）

**触发**：导入一段 294 音符的 MIDI → 保存报 `MIDI track #0 clip #0 holds 294 notes, exceeding the limit 64`。旧 D15 让整张 clip 表（`[RtMidiClip; 8]`，音符/controller 定容数组）搭 `SequencerCommand::SetTrackClips` 这个 `Copy` 变体过命令队列；rtrb 按**最大变体**预分配 1024 个槽，于是"单 clip 音符数"和"队列内存"直接相乘：64 音符 ≈ 9.5 KB/槽 ≈ 9.7 MB 队列，512 音符就要 ~54 MB。真实 MIDI 一轨上千音符，上限根本抬不动。

**改法**：`RtMidiClip` 的 `notes`/`ctrls` 变成 `Vec`（控制线程拥有），`MidiClipTable = [RtMidiClip; MAX_TRACK_CLIPS]`。表经**结构队列**（`GraphUpdate::SetMidiClips`，与 `SetAudioClips` 同队列）下发；block runner 在结构更新循环里 `sequencer.set_track_clips(track, table)`，返回值是被顶下来的旧表，立刻走 `trash_midi_clips`（满则 park 到 `midi_trash_pending`，下一块重试；极端情况下 `mem::forget` 有界泄漏，绝不在音频线程 drop）。控制线程每帧 `AudioEngine::reclaim_midi_clips()` 排空释放。与 U6 D4 的 audio clip 表完全同构。

**control clip 表也迁出命令队列（同一修订的第二步）**：`ControlClipTable = [RtControlClip; MAX_TRACK_CLIPS]` 是纯 `Copy`（每条曲线定容 `[RtAutoPoint; 64]`，无堆所有权），但它有 32 × ~544 B ≈ 17 KB。留在命令队列就是 1024 槽 × 17 KB ≈ 17 MB 的启动预分配，且把 `SequencerCommand` 的每个变体都撑大。改成 `GraphUpdate::SetControlClips` 后，代价挪到结构队列的一次性 256 × 17 KB ≈ 4.5 MB。**它不需要垃圾回执**（没有 `Vec`/`Arc` 要释放），block runner 直接 `sequencer.set_control_clips(index, table)` 原地覆盖。这是一条通用判据（D25 的补充）：**载荷大就要走结构队列，载荷带堆所有权才需要回执队列**。

**`Sequencer.control_clips` 必须「直接填堆」，不只是装箱**：16 轨 × 32 clip × 544 B ≈ 278 KB 内联会让每次 `Sequencer` move 变成 278 KB 栈拷贝。第一步只写成 `Box::new(std::array::from_fn(...))` —— **这不够**：`Box::new(expr)` 要先把整个数组在栈上造出来再拷进堆，峰值仍是 ~550 KB。在 Windows 上主线程默认只有 1 MB 栈，实测 `cargo run` 一启动就 `STATUS_STACK_OVERFLOW (0xc00000fd)`；而 `cargo test` 的测试线程有 2 MB，所以单测全绿、app 挂——**这就是为什么这个 bug 没被测试抓住**。正解是 `Box<[ControlClipTable]>` + `(0..N).map(...).collect()`：`collect` 走 `Vec → into_boxed_slice`，逐元素写进堆缓冲区，栈上只有单个元素（17 KB）的临时量。通用教训：**给实时结构加定容大数组前先算 `size_of`，超过 ~16 KB 就该装箱；装箱还要确认临时量没有落回栈上**。

**为什么 `Vec` 而不是更大的定容数组**：队列里带 `Vec` 的变体只占表头（~24 B ×2 ×32 clip ≈ 1.5 KB），音符本体在堆上且长度随内容；定容数组则是 `32 × N × 12 B` 的无条件内存，且每次编辑要整表 memcpy（`push_all_clips` 每轨一份 × 16 轨）。`Vec` 版 `push`/`push_ctrl` 天然只能控制线程调用（可能扩容），文档注释已锁死这一点。

**`schedule_track_clips` 的借用调整**：原实现 `let clips = self.clips[track];` 靠 `Copy` 复制整表来避开 `&self` / `&mut self.pending_offs` 冲突。`Vec` 不可 `Copy`，改为两个不相交字段借用（`&self.clips[track]` + `&mut self.pending_offs[track]`），并把 `set_pending_off` 从方法降为自由函数（接收 `&mut` 槽位数组）——否则调用它要再借整个 `self`。

**导入分块**：`smf::split_into_clips` 按 `IMPORT_CLIP_NOTES = 4096` 切块，切点落在 start tick 变化处（和弦不跨 clip），块起点 = 块内最早事件，`loop == span` 单周期；块数超过 `MAX_CLIPS_PER_TRACK` 时用 `div_ceil` 放大块长，不丢材料。UI 在导入状态栏补一句「N dense track(s) split into M clips」，避免一排气 clip 看着像 bug。

**行为变化**：单 clip 音符上限从 64 抬到文档级 `MAX_CLIP_NOTES = 65536`（仅防病态文件）；`MAX_CLIP_CTRLS` 常量删除（controller 也换成 `Vec`，密集弯音不再被 32 截断）；`SequencerCommand::SetTrackClips` / `SetControlClips` 两个变体删除，命令枚举回到小结构；`MAX_CLIPS_PER_TRACK` 8 → 32（model 与 engine 的 `MAX_TRACK_CLIPS` 同步），一轨可以放 32 个段落 clip。


**只投递 CC 与 pitch bend**：`graph::slot_ctrl` 对 `ChannelPressure`/`PolyPressure`/`Program` 明确返回 `false`——内置声源没有对应的消费面，SF2 的 program 走 `ParamId::Sf2ProgramChange` 参数路径。VST3 宿主无 `Processor::midi_cc` 实现（`vst3-0.3.0` 的 `DataEvent` 只有 SysEx），因此拿到默认 `false`，与 U4 D6 一致。

**内置声源对 bend 返回 `false` 是有意的**：P1 会让它们的音高走 `ParamId::PitchOffset`（逐样本 `Smoothed`），既能表达细腻 vibrato，也不需要用户定义 bend range。

### 卷帘坐标系绝对化（P2，spec `roll-multiclip.md`，2026-10-27）

**`RollGeom` 是唯一的坐标真相**：`{ origin_tick, t0_x, ticks_to_x, top_note, row_h, area }`，`x_of(local) = t0_x + (origin + local) * ticks_to_x`、`local_at(x)` 是其逆。绘制（音符/网格/播放头/modal ghost）、命中（`note_at`/`note_edges`）、刀切（`knife_crossing`）、框选、velocity 泳道全部走它。原来这些是 5–7 个参数的自由函数，每加一处绘制就复制一次公式；现在 `note_rect(&note)` 一个参数，P3 的 ghost 只是换 `origin_tick`。

**tick 仍全程 clip-local**：只有几何知道 origin。`pointer_tick` 变成 `geom.local_at(pos.x).max(0.0)`——钳制必须在**局部**坐标做，先钳绝对 tick 会让 clip 起点左侧的点击变成负的局部 tick。命令、模态、`clamped_move`、`selection_bbox`、`scale_max_k` 一行未改，这是"纯重构"能成立的原因。

**`scroll_x` 从局部 tick 变绝对 tick**：`ViewWindow { start, end }` 收口滚动钳制（原来三处在 zoom / wheel / middle-drag 里各写一遍 `clamp(0.0, loop - width/t_x)`）。P2 的窗口就是 `[clip.start, clip.start + loop]`，P3 换成 context 跨度即可。

**`RollState.view_origin` 是必需的，不是可选优化**：state 跨 target 存活，绝对 scroll 会让"打开一个从第 9 小节开始的 clip"继承上一个 clip 的 scroll（整个落在窗外）。origin 变化时重锚到自身起点，首帧 `None` 也走这条路径——这正是旧的"打开 clip 看到它开头"行为。

**等价性怎么保证**：`t0_x = rect.left() - scroll * t_x`，scroll = origin 时，音符 x = `t0_x + (origin + local) * t_x = rect.left() + local * t_x`，与改造前逐像素相同。37 项原测试全部通过（3 处直接调自由函数的改为调方法，断言未改），另加 `clips_with_different_origins_share_one_time_axis`（同一 `t0_x` 下两个 origin 的音符相隔一个 origin 的距离、逆映射各自正确）与 `the_scroll_window_is_the_clip_span`。

**fit 基准与 margin 留给 P3**：spec 原写"fit 改 context 跨度、钳制加 margin"，实测 `span_ticks ≥ loop_ticks`（活跃时长可长于内容），用 span 拟合会让单 clip 视图缩小，违反等价验收；P3 改用"context 内各 clip `start + loop` 的最大跨度"，单 clip 退化为现在的 `loop_ticks`。


## 多 clip 卷帘（P3）：subject / ghost / 焦点

**`RollSubject` 是"这一帧画什么"的唯一来源**：`{ focus: usize, context: Vec<ClipRef>, dropped: usize }`，`ClipRef { lane, track, clip }`。`focus` 是 `context` 的下标（不是独立的 clip 引用），这样"焦点必须在可见集合里"由类型结构保证，不需要额外断言。`single(track, clip)` 是退化构造，`assemble(focus, selection, clips)` 是 app 侧装配入口——`clips(track)` 回调用来判断引用是否已失效（选区活得比 clip 久）。

**ghost = 换一个 origin 的同一套几何**：`geom.at_origin(clip.start_ticks)` 之后直接复用 `note_rect`，颜色换成 `GHOST_NOTE`（`from_rgba_premultiplied(14,35,21,56)`，即 focus 色 22% alpha 的预乘值）。**它必须是 `const`**，而 `Color32::from_rgba_unmultiplied` 不是 const fn——所以写预乘值，注释里记原色。ghost 循环里没有"命中测试"代码可删，因为它压根没被喂进 `note_at`；测试 `a_ghost_note_is_painted_but_never_hit` 用"同坐标点 focus 音符仍能选中"做对照，防止坐标算错造成的假通过。

**`over_focus` 是 ghost 带来的新边界**：窗口现在是 context 跨度，指针可以落在 focus 之外。创建音符的 `snap(...).min(loop_ticks-1)` 会把窗外点击变成"在 tick 0 创建"，所以入口先判 `(0.0..loop_ticks).contains(&geom.local_at(x))`，窗外单击只清空选择 + 状态提示，窗外空拖不起框选（ghost 不可选，框选必然空手）。

**焦点切换是纯 view-state**：`set_roll_focus` 只改 `subject.focus` 并 `clear_gesture()`（顺带清 selection——索引属于旧 focus）。chip 条与 Tab 都在 `midi_editor_body` 里、`show()` 之前绘制，所以 `show` 不需要知道"焦点可能换人"。**Tab 用 `input_mut().consume_key` 消费**，否则 egui 的焦点遍历会先吃掉它；这也意味着多 clip 时编辑器内 Tab 不再切换控件焦点（单 clip 时不消费，保持原行为）。

**`EditTarget` 失去 `Copy`**：`Midi(RollSubject)` 带 `Vec`，`edit_panel` 改为 `self.edit_clip.clone()`。80 字节级 clone 每帧一次，比"拆成只读引用 + 内部再取一次 &mut self"容易读得多。

**track 重排要改全体**：`reorder_midi_track` 原来只重映射 focus 的 `track`，现在必须遍历 `context_mut()`——漏改会让 ghost 指向另一条轨的音符。


## Edit tab 跟随 timeline 选区（2026-10-28）

**Edit tab 没有自己的开关**：目标由 `edit_clip` 变成"从 `timeline_state.selection` 派生"（`sync_edit_target` 每帧在视图分发前跑一次）。双击不再是唯一入口，只是"选中 + 切到 Edit"的快捷方式；反过来，切走选区（点空白、Esc、删除、换选）就自动关掉编辑器。`× Close` 按钮因此退役——面板不再拥有自己的生命周期，空选区时它像 Inspector 一样给提示（"回 Timeline 选一个 clip"），只选中 audio clip 时明说音频 clip 没有编辑器。

**`edit_selection` 快照是必需的，不是缓存**：装配 subject 只在**选区变化**时发生（spec `roll-multiclip.md` E15 的约束）。没有快照就只能每帧重装，那会把 chip 条 / Tab 换来的 focus 抹回"最后选中的那个"。快照同时被 `set_edit_target` / `set_control_edit_target` 写，所以 ← → 翻 clip、双击打开这些不走选区的路径也不会触发下一次重装。

**谁当焦点 = 选区里最后一个"活着且可编辑"的 clip**（`lane != Audio` 且 `clips.get(clip)` 命中）：反向扫描等价于"最后点的那个"，与双击落点一致；存活性检查是因为选区活得比 clip/轨道久（删轨、undo 都会留下死索引），死索引不能进 subject。

**目标消失时不再回退到 clip 0**：`midi_editor_body` / `control_editor_body` 原来会把越界索引钳到 0，选区驱动的语义下那等于"悄悄换了 clip"。现在置空 `edit_clip` 交给下一帧的 `sync_edit_target` 重算——选区里还有活着的 clip 就换过去，全死了就回到提示态。

**行选中不再关编辑器**：`SelectTrack` / `SelectControlTrack` / `SelectAudioTrack` 原来顺手清 `edit_clip`，现在只改行高亮——轨道选中与 clip 选区是两件事，后者才是 Edit tab 的输入。`do_open_now` 额外清 `timeline_state.selection`（它索引的是**上一个**文档），`reorder_midi_track` 在重映射选区后同步快照。


## 音高通路（P1）：一个目标，两种投递

**曲线存半音，不存归一化**：`AutoParam::PitchBend { channel, range_semitones }` 的曲线值域是 `[-range, +range]` 半音（默认 ±2）。range 是**文档字段**而不是常量，因为同一个曲线在不同音源上可以是 ±2 的揉弦也可能是 ±12 的大滑音；存归一化再乘 range 会让"改了 range 曲线跟着变形"，存半音则改 range 只影响投递换算。`pitch_bend(channel, range)` 构造器钳制 `0..=15` / `0.5..=24`（NaN 回落 2.0），这与 MIDI 14 bit 的可表示精度对齐（±24 半音时 1 LSB ≈ 0.003 半音）。

**只有 `AutoDest::PitchBend` 一个目标，没有"内置源 PitchBend / 插件 PitchBend"两个**：spec 早期草案按声源类型分支，实测那样等于把"哪类节点吃什么"这张表搬到 RT 线程维护，节点自己最清楚。现在的投递顺序是：

```rust
if self.slot_midi_pitch_bend(route.slot, channel, bend) { return; }   // CLAP / SF2
let _ = self.slot_set_param(route.slot, ParamId::PitchOffset, value); // synth / sample
```

`slot_midi_pitch_bend` 只有实现了 `Processor::midi_pitch_bend` 的节点返回 true；内置合成器与采样播放器返回 false，于是自然落到逐样本参数。**VST3 两边都没有 = 静音失败**，这是有意的（VST3 侧要给逐样本音高需要 `ProcessData` 里的 `PitchBend` 事件，宿主层还没接，spec 记为"不支持"而非"假装支持"）。UI 悬停文案把这个差异直接写给用户看。

**14 bit 换算与去重**：`ratio = (value / range).clamp(-1, 1)`，正向 `×8191`、负向 `×8192`（MIDI 的 0..16383 中心在 8192，两侧不对称，用同一个 8191 会让 +1 全量偏差 1 LSB）。`last_bend: [i32; MAX_AUTO_TRACKS]` 与 `last_cc` 同构——同一 block 内重复值不重复下发，否则 16 分音符的平坦段每块发一次 bend，把插件的事件队列喂爆。

**逐样本平滑是必须的，块级会响**：曲线在拖拽时逐帧跳值，块级跳变在低频上就是 click。`SineProcessor` 用 self 携带的 `pitch_ratio/target/coeff`（`coeff_for(5.0ms, sr)`）逐样本 `ratio += (target - ratio) * coeff`，然后 `phase += (base_inc * ratio).clamp(0.0, 0.5)`——`clamp` 上界是 0.5（每样本半周期），防止极端弯音把相位增量推过 Nyquist。`SampleVoiceProcessor` 的难点是 16 个 voice 共享同一个 block 级平滑：**每块先填一条 `pitch_ramp: Box<[f32]>`，再让每个 voice 按 `voice.pos += voice.rate * ramp[f]` 读**，这样 16 个 voice 的滑音曲线完全一致，也避免每个 voice 各存一份状态（原先 voice 结构体是纯 POD，加了状态就得在 note-on 时重置，容易漏）。`render_voice` 因此涨到 8 个参数，`#[allow(clippy::too_many_arguments)]` + 注释说明比拆结构体更可读。

**测试用"半周期"验证而不是听感**：`pitch_offset_glides_within_one_block` 断言块内前半段与后半段的过零间隔比例趋近目标 ratio；`pitch_offset_moves_a_voice_that_is_already_playing`（`tests/sample_voice.rs`，配 `cursor_advance` helper）专门覆盖"note 已经开声之后再弯音"——这是最容易漏的路径，因为 ramp 只在 `process` 开头填，voice 若在块中途 pop 进来就会用到上一块的旧值。


## 曲线泳道（P4）与折叠（P5）

**卷帘不认识电缆**：`OverlayLane<'a> { clip: ClipRef, data: &'a ControlClip, axis: (f32, f32, bool), label: String }` 由 app 侧 `midi_editor_body` 组装——纵轴 `(min, max, symmetric)` 来自 `curve::axis(&param)`，`param` 来自 `control_track_param(project, track)`（按 `PortRef::AutoTrack` + `CableKind::Automation` 反查电缆）。卷帘只负责"给一个矩形，按这个值域画折线"。这样自动化目标换了、值域变了，卷帘一行不用改。

**时间轴复用而不是再推一遍**：每条 lane `allocate_exact_size` 拿到自己的 rect 后，`geom.at_origin(clip.start_ticks).over(rect)`——`over` 换掉 `area` 但保留 `origin/t0_x/ticks_to_x`。于是泳道与音符**逐像素对齐**，`a_lane_shares_the_time_axis_with_the_notes` 就是拿同一 tick 在两个 rect 里的 x 相减断言为 0。

**`ViewWindow` 要并上 lane 的跨度**：context 里可能有"没有音符但有一条长曲线"的 clip，只看音符会让窗口截掉曲线尾巴。`show` 里用 `fold(..., |w, lane| w.union(ViewWindow::of(...)))`，`union` 取 min/max。

**逐像素采样画折线**：不是"连锚点"，而是对矩形内每个 x 求 tick → `value_at(phase)` → y。原因是曲线是分段插值 + 可能循环，连锚点会在 loop 边界画出一条横跨全屏的假线。空曲线（`points` 空）时 `value_at` 返回 None，直接断开（不画 0 线）。`rem_euclid` 处理循环：`phase = (tick - clip.start) mod loop`。

**拖拽分类靠 `press_origin()`**：单击（加锚点）与拖动（移动锚点）在 `response.dragged()` 上无法区分——第一帧拖动就已经有 delta。判据是"按下时的位置是否落在某个锚点的 `ANCHOR_RADIUS` 内"：是则 `CurveUpsert { from: Some(tick), dragging: true }`，否则 `from: None`（新建）。松手发 `CurveDragEnd` → app 层 `end_param_edit()` 收口 undo 事务。这个 bug 在测试里表现得很典型：`dragging_a_lane_anchor_reports_the_anchor_it_moved` 最初收到 `from: Some(960)`（拖到的新位置）而不是原点。

**`lane_rects` 是布局的公开产物，测试用它而不是自己推**：测试 fixture 一开始自己按 `ROLL_HEIGHT + LANE_HEIGHT * i` 推算泳道 y，漏了 velocity 条，导致"点 (x, 0.0) 得到 0.25"。现在 `RollState.lane_rects: Vec<egui::Rect>` 每帧由 `show` 填充，`lane_pos()` 直接查。同理折叠：**点击处理发生在布局之后**，所以切换折叠状态当帧的 rect 还是旧的，`the_lane_toggle_collapses_the_lane` 必须多跑一帧才看到 18 px。

**折叠是 view-state，按 track 索引**：`collapsed_lanes: Vec<usize>` 存的是 control lane 的序号（= `lanes` 数组下标），不落文档——它是"我暂时不想看这条"的临时偏好，跟工程语义无关。折叠后高度 `CURVE_LANE_COLLAPSED_HEIGHT = 18.0`（够放一个小方块 + label），方块点击在 `curve_lanes` 里 `continue` 掉，不进入曲线交互。


## 节点参数自动化词汇表（U4c / A，2026-10-28）

**缺的从来不是通路，是词汇表。** 引擎的自动化链路 `set_auto_route(index, node_id, ParamId)` → `auto_deliver` → `slot_set_param` 一直能写任意节点的任意 `ParamId`（`graph.rs` 里 `auto_route_delivers_curve_values_to_target_param` 就是 mixer 通道 gain 的端到端断言）。用户看到"gain 收不到控制"，是因为 `AutoParam` 只有 7 个变体 + `accepts()` 只认乐器 + `auto_options` 只出 Cutoff/插件参数 —— 菜单里压根没有 gain 这一项。

实现要点：

- **地址进 model，翻译在 UI**：`AutoParam::Node(NodeParam)`，`NodeParam` 覆盖 mixer 通道/输出级、效果器 slot、Feedback/SF2/Sample gain、resonance 与 ADSR。engine 的 `ParamId` 仍只由 `lib.rs::node_param_id` 单点翻译（model 不依赖 engine，沿用原分层）。加性变体 → `PROJECT_VERSION` 不变，旧工程零迁移。
- **两个菜单合一**：`node_auto_options(doc, node_id, clap, vst3)` 是 patch 弹窗与 Inspector Target 行的唯一枚举来源。以前两处各写一遍，正是"mixer 通道从没进过列表"的原因。`plugin_params_of` 负责乐器/效果器的参数表分家（`clap_voice_params` vs `clap_effect_params`），顺带把内置效果器的 descriptor slot 补进 Inspector 菜单（原先只有 CLAP/VST3 效果器）。
- **曲线轴需要节点上下文**：`curve::axis(&AutoParam)` 保持上下文无关（各参数对齐面板 range），但效果器 slot 的真实范围在节点 descriptor 里 → 新增 `axis_in(param, doc, target)`，三个绘制点（Edit tab 曲线编辑器、卷帘 overlay、timeline 只读预览）全部改走它。timeline 的 `TimelineParams::curve_param: Option<&AutoParam>` 因此换成 `curve_axis: (f32, f32, bool)` 元组（预览只需要轴，不需要参数身份）。
- **只校验 mixer 家族**：`check_node_param` 强校验 `channel < inputs` 与 `MixerOut*` 必须在 mixer 上 —— 通道索引是文档能主动作废的唯一参数身份（删通道）。resonance/SF2 gain 这类与 voice 绑定的参数不校验：换 voice 会让老工程保存失败，而引擎对不认识的参数本来就返回 `false`。
- **删通道要在同一步收尾**：`mixer_remove_channel` 除了 `inputs -= 1` + `channels.pop()`，还要 `retain` 掉指向该通道的 automation 电缆并 `clear_auto_route(index)`，否则下次保存被校验拒绝、引擎也会继续往闲置通道写值。

**已知边界**：参数仍是块边界写入（≤2.7 ms）。内置效果器有自己的 `Smoothed`，但 **mixer 通道权重是块内常量**（`input_weights()` 每块读一次，`sum_into_weighted` 逐样本乘同一权重），所以 LFO 形态的 gain 曲线会有 375 Hz 台阶；推子慢推听不出来。块内线性斜坡是 A2 的后续项：`Processor` 增加"上一块权重"接口 + 求和循环在 `from`/`to` 之间插值，代价是 mixer 求和数学测试的期望值要跟着改成"块内均值"。
