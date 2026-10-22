# 拍号与 tempo 控制（U3）Spec

> 状态：**已实现**（U3a 拍号 2026-10-22 / U3b tempo 曲线 2026-10-22）。实现要点与踩坑见 `.agents/implementation.md` 的「拍号（U3a）」「tempo 曲线（U3b）」两节。
> 上游：`timeline.md` D17（control clip 投递窗口）、`node-graph.md` D7（`auto_routes`）、U1（seek / loop 基建）、U2（文件与 dirty）。

## 1. 背景与问题

1. **拍号写死 4/4**：`TICKS_PER_BAR = TICKS_PER_STEP * 16` 是常量（model 与 engine 各一份，`crates/glitcherz-ui/tests/tick_base_consistency.rs` 锁同步）；SMF 导出写死 `FF 58 04 4 2 24 8`；timeline 小节网格、loop 吸附、arrangement 末尾取整、piano roll 小节线全按 4/4 算。
2. **tempo 只有单一值**：`Sequencer::set_bpm` 一个 f32，20..300 钳制。想要 120→180 渐快、或尾声 rallentando，只能手拧，且拧完无法随工程保存（BPM 在 metadata 里能存，但"变化过程"存不了）。

## 2. 目标形态

- **拍号**：文档级**静态**字段（一个工程一个拍号），由它派生 `ticks_per_bar`，所有网格/SMF 跟着走。
- **tempo 曲线**：control 轨 + **固定的 `Timeline In` 节点**——patch 里有一个与 `Timeline`（纯源）对称的纯汇端点，默认存在、不可删不可增，v1 只有一个 `Tempo` 入端口；control 轨的自动化出口连过去，曲线画在 tick 轴上（与 LMMS 的 automation → tempo 同构）。

## 3. 关键判断：tempo 与普通参数（含未来的 CC）不是同一类

| 维度 | 普通参数（gain / cutoff / plugin param） | tempo（本 spec） | MIDI CC（U4） |
| :--- | :--- | :--- | :--- |
| 形态 | 节点参数值 | **时钟**（tick↔sample 换算率） | **事件流** |
| 消费点 | `Processor::set_param`，块内任意时刻都成立 | 必须在 `Sequencer::advance()` **之前**；块级常数 | block 内按采样时间戳注入目标节点 MIDI 输入 |
| 值域 | 各参数自定义 | 20..300 Hz 连续 | 0..127 整数 |
| 基数语义 | 手动值 + 曲线覆盖 | **base BPM + 曲线覆盖** | 无 base（事件即事实） |
| 并发 | 每个节点各一份 | **全局唯一**（一个 `Timeline In`） | 每个目标端口一份 |
| 量化误差 | 无（参数即时生效） | 块级（48k/512 ≈ 10.7 ms 阶梯） | 采样级 |

结论：**UI 表达同构（control 轨 + 一条线），引擎消费必须分家**。tempo 不能走 `ParamId` → `slot_set_param` → `Processor::set_param` 那条通路——那里要求目标槽位有 processor，而 tempo 没有、也不该有（它不属于音频图）。

### 3.1 这不是信号环，是「自指时钟」

"control 轨画在 timeline 上，又去控制 timeline 的 tempo"看着像环，但它**不是** P6 那种需要 Feedback 节点打断的**块内代数环**：

```text
position_n ──(曲线采样)──> tempo_n ──(积分一个块)──> position_{n+1}
```

第 n 块的 tempo 只取决于**已经走完的** position_n，块内认为 tempo 恒定，下一块才更新位置——这是一次前向欧拉积分（ODE），因果完全成立，没有"同一块内自己依赖自己"的代数环。所以它**不需要** Feedback 节点，也不需要额外一块延迟；需要的只是两件工程上的事：

1. **顺序**：曲线必须在 `advance()` 消费 tempo 之前采样（今天 `evaluate_automation()` 排在 `advance()` 之后，见 D4）。
2. **分层**：`base_bpm`（用户拧的值）与 `bpm_override`（每块重算）分开，曲线外才能干净地回落（见 D3）。

之所以"看起来危险"其实有三层保护，值得写下来防止以后被改坏：

- **tick 域稳定**：曲线横坐标是 tick，tempo 改变的是 tick→秒的换算率，不会移动曲线自己的 x 坐标。所以"改自己的值 → 自己的位置变了 → 值又变"这种自激不可能发生。（若把曲线索引在**秒**域，就会真的失控：tempo 越高读得越快、值越高。这正是"按 tick 采样"这个选择的隐含价值。）
- **位置单调**：`position_ticks` 只增（除非 seek/回卷），不存在多解或振荡。
- **块级常数**：一个块内 tempo 不变，误差上界 = 块长（48k/512 ≈ 10.7 ms），不会累积漂移（位置是积分出来的，不是每块重新算的）。

**真正的"横向拉伸"风险在未来，不在 U3**：beat 域的时间轴不随 tempo 变形，所以 MIDI/control clip 都安全。但 U6 的 audio clip 一旦以秒为单位播放，tempo 变化就会让波形在 tick 轴上伸缩——那时必须明确约定"锚定起点 tick + 按源时长拉伸"还是"不随 tempo 变调变速"，这是 U6 spec 的事，此处只登记。

## 4. 决策

| # | 决策 | 内容 |
| :--- | :--- | :--- |
| D1 | **tempo 目标是固定的 `Timeline In` 节点，不是用户可建的 conductor 节点** | patch 里与 `Timeline`（纯源，n 个出口）对称的**纯汇**节点：0 个出口、一个端口表（v1 只有 `Tempo` 一个入端口）。身份固定、**默认存在**、不可删除、不可新增第二个——与 `Master`／`Timeline` 同级（`PatchId::TimelineIn`，不进 `graph.nodes`，不占 node_id，无 RT processor）。备选方案与取舍见 §4.1。 |
| D2 | **单例由「身份」保证，不靠校验** | 它不是一个可实例化的对象，所以"第二个"在结构上不存在，`GraphDoc::validate` 不需要写单例规则，也不会有"用户把节点删了、工程里没有 tempo 目标"的存档。校验只需约束连线：指向 `TimelineIn` 的线必须是 `Automation`，`param` 只能是 `AutoParam::Tempo`。 |
| D3 | **base + override 两层 tempo** | `Sequencer` 保留 `base_bpm`（用户拧的值）+ `bpm_override: Option<f32>`（每块由自动化重算，无覆盖时清空）。`ticks_per_sample()` 用 effective。曲线**绝对覆盖**而非叠加——tempo 语义是"这里就是 140"，不是"+20"。 |
| D4 | **块级量化 + 评估顺序前移** | `evaluate_automation()` 从 `advance()` 之后移到之前（用 `position_before` 采样），tempo 与块起始 tick 对齐；否则第一块永远用未覆盖的 base 跑 10 ms。块内认为 bpm 恒定，最大误差 = 块长。 |
| D5 | **拍号静态，不自动化** | 拍号改小节长度，牵动网格、loop 吸附、arrangement 取整、SMF、piano roll；做成可自动化等于把"变长小节"引入所有时间数学。U3 只做文档级标量。`Timeline In` 也**不**预留拍号端口。 |
| D6 | **文件版本不动（仍 v6）** | `time_signature` 带 serde default（4/4），旧文档读出来行为与今天完全一致，属于加法；不需要 `version 7` 分支。原 plan 里"v6→v7 迁移"的判断作废。 |
| D7 | **`Timeline In` 只接自动化线** | `accepts(CableKind::Automation, PatchId::TimelineIn)` = true，音频/MIDI 线一律拒绝；端口 `Tempo` 不收用户选的参数（拖上去直接成线，跳过参数弹窗）。Synth 面板的 "Pick Parameter…" 里也加一条 `Song Tempo`，走同一条电缆路径。 |
| D8 | **改拍号不改写任何事件** | 拍号是**度量定义**，不是数据变换：note / clip / marker / loop 的 tick 值一律不动，不需要迁移，也不需要"按新拍号重排"的选项。变的只有**引用"小节"的换算**（ruler 标号、bar 网格、loop 吸附、arrangement 末尾取整、quantize-to-bar、SMF `FF 58`）。与"改调号不改音高"同性质，比"移调"弱——不存在移调那种数据改写。 |
| D9 | **BPM 恒指四分音符** | `tempo_bpm` 的单位**不随分母变**：6/8 下 120 BPM = 每分钟 120 个四分音符（附点四分 = 80）。这样 tempo 曲线、SMF tempo 事件、U7 离线渲染三者只有一个换算口径；把"每分钟拍数"随分母解释会让同一个 BPM 值在不同拍号下铺出不同的秒数，属于隐形的语义分叉。 |

### 4.1 D1 的四个备选（为什么选 B）

| 方案 | 做法 | 代价 |
| :--- | :--- | :--- |
| A | 给 master 节点加 `AutoParam::Tempo` | 零新实体，但"master 音量"和"全局时钟"挤一个节点语义；master 当前只收 Audio 线，破例后 `accepts` 出现特例 |
| **B（选定）** 固定 `Timeline In` 节点（纯汇） | 与 Timeline 对称的固定端点，端口表可扩展 | 新 `PortRef` 变体 + patch 画一个固定 box + 拖拽落点 + 哨兵路由。与 `Master`/`Timeline` 同一套现成机制 |
| C | 用户可建的 `NodeDoc::Conductor` 节点 | 需要 node_id、add 菜单、删除守卫、place、undo label、validate 单例规则；换来的自由度为零（多一个也没意义） |
| D | `PortRef::TimelineIn` 但 patch 不画 | 模型最省，但这条线在 patch 里没有落点，拖拽无从发生，只能从 Synth 面板选——丢掉"在 patch 里看见信号去向"的可见性 |

B 的关键收益是**结构即约束**：不可增、不可删、端口表可以往后加（见 §4.2），全部由"固定端点"这一个事实推出来，不需要额外状态去维护它。

### 4.2 未来的 `Timeline In` 端口（登记，不在 U3）

| 候选 | 适合放进来的理由 | 备注 |
| :--- | :--- | :--- |
| `Tempo` | v1 唯一端口 | 本 spec |
| `Swing` | 同样是全局时钟整形，值语义、块级生效 | 与 tempo 共享 `Sequencer` 的时钟路径，实现成本低 |
| `MasterTune`（全局音分） | 全局标量，没有天然归属节点 | 也可做成 master 节点参数，待定 |
| 拍号 | — | **不放**（D5）：会引入变长小节 |
| transport 命令（play/stop/loop） | — | **不放**：那是命令不是连续值，control 曲线没法定时序语义 |

端口表只是"值 → sequencer 字段"的映射，新增一个端口 = 端口表加一项 + 块内多一个 `match` 分支，不动节点身份与文件版本。

## 5. 数据模型

```rust
// glitcherz-model
pub struct TimeSignature {        // 默认 4/4
    pub numerator: u8,            // 1..=16
    pub denominator: u8,          // 1|2|4|8|16|32
}
pub struct ProjectMetadata {
    /* ... 现有字段 ... */
    #[serde(default)] pub time_signature: TimeSignature,
}

// 固定端点（与 Master / Timeline 同级）：没有 NodeDoc 变体、没有 node_id
PortRef::TimelineIn                                  // 纯汇：只接 Automation 线，0 个出口

AutoParam::Tempo                                     // 仅对 TimelineIn 的 Tempo 端口合法
```

派生（model 层单一真源）：

```rust
impl TimeSignature {
    /// ticks_per_bar = TPQ × 4 / denominator × numerator
    pub fn ticks_per_bar(&self) -> u32 {
        TICKS_PER_QUARTER * 4 / self.denominator as u32 * self.numerator as u32
    }
}
```

- `TICKS_PER_BAR` 常量**保留**为 `4/4` 的默认值（clip 默认 span、旧测试），所有**读文档**的路径改用派生值；engine 侧加 `Sequencer::bar_ticks` 字段（配套自由函数 `ticks_per_bar_of(num, den)` 做第二份公式，`tick_base_consistency` 对锁），由新命令 `SetTimeSignature{numerator, denominator}` 下发（arrangement 末尾取整 D16 用它）。
- **排列上限保持绝对 tick 数**：`MAX_ARRANGEMENT_TICKS = 256 × 3840` 不改、也不乘派生值——它是数组/校验的绝对边界。换算成小节数的话，3/4 是 341 小节、7/8 是 292 小节，都还能接受；文案从「256 小节」改成「4/4 下 256 小节的等长上限」，**行为零变化**。
- **改拍号后的既有对象停在原 tick**（D8 的直接推论）：loop 端点、marker、clip 结尾不自动吸附/换算，可能因此不再落在大节线上——由用户自己拖，不做隐式移动。UI 只需保证下一次*拖动*时用新网格吸附。
- `CableDoc { kind: Automation, to: PortRef::TimelineIn, param: AutoParam::Tempo }`。
- 校验：指向 `TimelineIn` 的线必须是 `Automation`；`AutoParam::Tempo` 只允许出现在 `TimelineIn` 上（`AutoParam` 其它变体不允许指向它）。
- **端口表**（v1）：`enum TimelineInPort { Tempo }`，patch 与引擎各一份，两者由 `glitcherz-ui/tests/tick_base_consistency.rs` 同类测试锁同步；未来加 `Swing` 等只需扩这个枚举（见 §4.2）。

> **实现注记（2026-10-22 落地后）**：v1 只有一个端口，所以没有真的引入 `TimelineInPort` 枚举——文档侧用 `AutoParam::Tempo`（`is_timeline_in_only()`），引擎侧用 `AutoRoute.slot == TIMELINE_IN` 哨兵，patch 侧端口数直接写 1 并在 `patch.rs` 单测里锁住（`timeline_in_ports_and_cable_rules`）。加第二个端口（如 `Swing`）时再把这个枚举补上，`AutoRoute` 也需多一个 `port` 字段——届时 `TIMELINE_IN` 哨兵的含义从"唯一端口"变成"端口表索引"。

## 6. 引擎实现

```text
UI ──cable→ engine.set_timeline_in_route(auto_index)      // 单端口，无需 port 参数
                └─ 不进 GraphCompiler 的 node_slot 查找 → AutoRoute{slot: TIMELINE_IN, param: 占位}
                                                     (TIMELINE_IN = u16::MAX - 1，与 NO_EDGE 区分)

每个 block:
  1. position_before = sequencer.position_ticks()
  2. if playing { evaluate_automation(position_before) }   // ← 前移（D4）
       ├ 命中 TimelineIn → sequencer.set_bpm_override(value)
       └ 其余            → graph.auto_set_param(index, value)   // 原路径
  3. sequencer.advance(&ctx, …)                            // 用 effective bpm
  4. graph.process_block(&ctx)
```

- `Sequencer`：`bpm` 改名 `base_bpm`；新增 `bpm_override: Option<f32>`；`ticks_per_sample()` 取 `bpm_override.unwrap_or(base_bpm)` 再钳制。`play`/`stop`/`pause` 清 override（停止后回到用户拧的 BPM，不需要 UI 补推）。
- `evaluate_automation` 每块先清 override，再按"首个覆盖当前位置的 clip"值设置——clip 之间有空隙时回落 base（D3）。注意这与普通参数的"clip 外保持最后值"不同，是**有意的**：时钟必须有一个确定的基值，否则一段 tempo 曲线结束后速度会永远停在最后一个点。
- `AutoRoute` 加 `TIMELINE_IN` 哨兵语义：`slot_set_param` 不再被调用；`auto_set_param` 保持不动（哨兵不落进 `slots`，自然跳过）。
- patch 侧：`PatchId::TimelineIn` 作为固定 box 画在 timeline 右侧（0 出口、左侧一个端口点），拖放 → `PatchAction::ConnectTempoIn { index }`（新动作，跳过参数弹窗）。

## 7. 边界与验收

| 场景 | 期望 |
| :--- | :--- |
| 曲线覆盖播放中循环回卷 | 回卷到 loop_start 后按该 tick 的曲线值；base 不变 |
| seek 到曲线中段 | 立刻按目标 tick 采样，下一块生效（≤ 1 块延迟） |
| tempo clip 之间有空隙 | 空隙内 = base BPM；进 clip 立刻切换（块级） |
| 停止 / 暂停 | override 清空 → base BPM；再播重新采样 |
| 没有任何 tempo 线 | 行为与今天完全一致（纯加法，零回归；`Timeline In` 只是块空 box） |
| 断开 tempo 线（删 cable） | route 回 `NO_EDGE`，tempo 回 base |
| 拖到 `Timeline In` 的音频线 / MIDI 线 | 拒绝（`accepts` 规则），与拖到 Master 上的非法线同款反馈 |
| undo / redo / do_open | 走既有 `apply_document` → 重建图 + 重推 route，无需新路径 |
| 拍号 3/4 | timeline 网格每 3 拍一小节、loop 吸附 3 拍、arrangement 末尾按 3 拍取整、SMF 导出 `FF 58 04 3 2 24 8` |
| 拍号 6/8 | `ticks_per_bar = 960×4/8×6 = 2880`（分母是"几分音符为一拍"） |
| 4/4 → 6/8 切换 | 小节**长度不变**（2880 = 3×960 = 6×480），只有分组/重音显示与 SMF 分母变；既有事件 tick 逐字节不变 |
| 4/4 → 2/4 切换 | 小节长度 3840 → 1920；既有 loop / marker 停在原 tick（可能不再落在小节线上），不隐式吸附；下一次手动拖动按新网格 |
| 任何拍号切换后 | note / clip / marker / loop 的 tick 全部不变（D8）；文件里 `version` 仍为 6 |
| 导入一份 3/4 的 MIDI 到空白工程 | 继承 3/4（`is_unwritten` 为真时"导入"等价"打开"），状态栏注明 `meter set to 3/4` |
| 导入一份 3/4 的 MIDI 到已有内容的工程 | 保留工程自身拍号，状态栏注明 `file is 3/4, project stays 4/4`（不隐式重划网格） |
| 没有 `FF 58` 的文件 | `SmfImport::time_signature = None`，工程拍号不变 |
| 旧 v6 文档 | 读出来 4/4，输出与今天逐字节同构（`version` 仍为 6） |

## 8. 分阶段

**U3a 拍号（先做，风险低）**
1. model：`TimeSignature` + `ProjectMetadata.time_signature` + `ticks_per_bar()` + validate（**无数据迁移**，见 D8）；
2. engine：`Sequencer::ticks_per_bar` 字段 + `SetTimeSignature` 命令，arrangement 取整改用字段；
3. SMF：导出写真实拍号；导入读拍号（现在直接丢），返回 `SmfImport{tracks, time_signature}`，空白工程才继承（见 §7）；
4. UI：transport 上加拍号控件（`4/4` 下拉），timeline/piano roll 网格与 loop 吸附用派生值，改拍号 = 一步 undo；
5. 测试：派生数学（4/4、3/4、6/8、7/8）、SMF 往返、旧文档默认。`crates/glitcherz-ui/tests/tick_base_consistency.rs`（`TICKS_PER_BAR == 3840`）**保持绿**——常量仍是 4/4 的定义，只是不再是唯一真源。

**U3b tempo 曲线（依赖 U3a 的控件位置，但代码不强耦合）**
1. model：`PortRef::TimelineIn` + `AutoParam::Tempo` + validate 连线规则；
2. patch：`PatchId::TimelineIn` 固定 box（复用 Master 那套绘制/命中）、`accepts` 规则、`PatchAction::ConnectTempoIn`；
3. compiler / engine：`TIMELINE_IN` 哨兵路由、`set_timeline_in_route`；
4. engine：`base_bpm`/`bpm_override` 分层、`evaluate_automation` 顺序前移、每块清 override；
5. UI：Synth 面板 "Pick Parameter…" 加 `Song Tempo` 条目；走带 BPM 显示 effective 值（可选：加 `feedback.bpm` 原子，播放中显示曲线值并标 `auto`）；
6. 测试：曲线覆盖/回落、loop 回卷、seek 重采样、停播复位、非法连线拒绝、无线零回归。

**后续（不在 U3）**
- SMF 导入把 tempo 变化读成 tempo 曲线（`import_smf` 目前整段丢弃 tempo 事件，见 implementation.md 的已知取舍）；
- U4 的 CC 输出是**另一条通路**（事件注入，不是参数投递），届时复用本 spec §3 的分家原则；
- U7 离线渲染必须用同一套 effective BPM 求值，否则导出的音频与实时播放不等价；
- U6 audio clip 与 tempo 曲线的交互（波形在 tick 轴上的伸缩语义）要在 U6 spec 里定。
