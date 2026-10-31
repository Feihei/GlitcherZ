# MIDI 生成工具（W4 随机化 + W5 strum）Spec

> 状态：定稿（2026-10-31）。上游：`editing-model.md`（G/S/T 命令族）、`midi-nodes.md`（实时琶音器）。本 spec 只覆盖**一次性写入文档**的 MIDI 生成编辑，不新增引擎节点。

## 1. 背景

用户提出：MIDI clip editor 需要「随机化」与「strum」两项生成式编辑。

现状核查（2026-10-31）：

- `glitcherz-model/src/ops.rs` 已有 seeded XorShift（`DEFAULT_SEED` / `next_f32`）、`apply_to_selected` 选区适配器、`VelocityOp::Humanize`；
- 命令层 `midi.quantize` / `midi.velocity_op` / `midi.duration_op` 已注册；
- **UI 侧完全没有接线**（全仓搜不到这些命令的调用），roll 工具栏只有网格与模式 chip；
- strum（和弦起音错位）无任何实现。

## 2. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| G1 | 纯函数、不改 slice 长度 | 两条 op 都遵守 AGENTS.md「MIDI ops 纯函数化」：`fn(&mut [Note], Options, seed)`，原地改、不增删、不 re-sort；调用方（命令层）负责排序。 |
| G2 | 一次性写死，不做演奏期随机 | 随机化的结果直接进文档（可撤销、可保存、SMF 可导出）。「每次播放都不同」需要引擎按 occurrence 播种，记为未来项，**本期不做**。 |
| G3 | 概率 = 跳过，不是删除 | `probability` 决定一个音符是否被扰动；未命中的音符保持原值。slice 长度不变，选区索引语义不破。真正的「随机删音」属结构性编辑，另立命令。 |
| G4 | 随机化四个半范围 | `pitch`（±半音，0..=24）、`start`（±tick）、`length`（±tick）、`velocity`（±，0..=127）。每个范围 0 = 该字段不动；clamp 到合法域（pitch 0..=127、start ≥ 0、length ≥ 1、velocity 1..=127）。 |
| G5 | strum 和弦分组 = 起始时间窗口 | 按 `start` 排序后，与组内**首音**距离 ≤ `group_window_ticks` 的音符归为一组；跨组不影响。窗口 0 只合并完全同一时刻的音符。 |
| G6 | 起音顺序 | `Up` = 低音先（上扫）；`Down` = 高音先（下扫）；`Alternate` = 每组交替，**第一组为 Up**；`Random` = 组内 seeded Fisher–Yates。 |
| G7 | strum 只改 start | 第 k 个起音 `start += k · spread_ticks`，另加 `±jitter_ticks`；**length 不变**（记谱时值保持，音符结束时间随之错开），最终 `start ≥ 0`。 |
| G8 | 命令返回 `indices` | 两条命令都返回 `{ affected, indices }`：排序可能置换索引，调用方（roll）用 `reselect_from` 把选区跟到编辑后的音符上。 |
| G9 | UI = roll 工具栏内联面板 | `Randomize` / `Strum` 两个 toggle；参数行就在工具栏下方（不弹窗、不占主视口），`Apply` 发一次命令、关闭面板；`Cancel` 关闭不动文档。面板参数是会话内视图状态，不落盘、不进 undo。 |
| G10 | 与 Arpeggiator 的边界 | strum 是**写进文档的起音错位**（结果可见、可再编辑）；Arpeggiator（`midi-nodes.md`）是**实时重排/重复**，不改文档。二者不共用代码，也不互相代替。 |

## 3. 命令接口

```text
midi.randomize {track, clip, indices?, pitch? 0..24, start? 0..3840,
                length? 0..3840, velocity? 0..127,
                probability? 0..1 (default 1), seed?}   → {randomized, indices}
midi.strum     {track, clip, indices?, direction? up|down|alternate|random,
                spread_ticks? 0..3840, group_window_ticks? 0..3840,
                velocity_ramp? -127..127, jitter_ticks? 0..960, seed?}
                                                        → {strummed, indices}
```

`indices` 缺省 = 整 clip。所有数值参数在命令层再钳一次（UI 只是第一道闸）。

## 4. 验收

- model：seeded 可复现、四字段 clamp、零范围/零概率恒等、probability 只跳过不删除；strum 的 Up/Down/Alternate/Random、窗口分组、`length` 不变、jitter/ramp clamp。
- command：选区与整 clip 两条路径都返回合法 `indices`；空参数不 panic。
- UI：工具栏面板可开合，Apply 后选区跟随（`reselect_from`）。
- 全仓 `fmt` / `clippy -D warnings` / `test` 绿。

## 5. 明确不做

- 演奏期随机（G2）。
- 随机删音 / 随机复制（结构性编辑，单独立项）。
- 音阶约束随机（先观察实际使用，再决定是否加 scale mask）。
- strum 的 swing/量化交互（先量化再 strum，或反之，由用户顺序决定）。