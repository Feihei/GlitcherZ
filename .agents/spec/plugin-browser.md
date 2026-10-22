# Spec — 插件浏览器与添加节点菜单（plugin-browser）

> 状态：**定稿**（2026-10-22，三项决策已拍板；同日 v2 修订：① 取消"换源"动作，换源 = 删了重建；② 扫描改**前台分片 + 进度条**，不用后台线程）。
> 关联：`plugin-gui.md`（G4 面板按选中节点分派、U8 Inspector 收口）、`node-graph.md`（D2 端口剖面、D6 悬空语义、D19 旁路重连、P5e.2 连线中点插入）。
> 验收锚点：启动时插件扫描带进度条、窗口不冻结；热启动列表毫秒级打开；200+ 插件目录下靠"层级 + 过滤框"三次以内点击直达；加节点 = **一步 undo**、引擎只 install 一次。
> 覆盖关系：本 spec 接管 `plan.md` U8 中"Instrument 面板收敛为**源选择** + preset + 参数 + 编辑器区"的**源选择**部分——源选择移到 patch，Inspector 只留 preset / 参数 / 编辑器。

## 1 背景与问题

| # | 现状（代码锚点） | 痛点 |
| :--- | :--- | :--- |
| P1 | 插件列表是四个**惰性** `Option` 缓存（`glitcherz-ui/src/lib.rs:578-584`，`scanned_instruments` / `scanned_effects` / `scanned_vst3_*` 在 `:3509-3560`） | 首次展开菜单时**在 UI 线程同步**执行 `list_clap_plugins()` / `list_vst3_plugins()`，逐 dylib `LoadLibrary` 读元数据（`clap-host:242`、`vst3-host:323`）。几百个插件的目录直接卡若干秒，窗口无响应 |
| P2 | CLAP 乐器与 CLAP 效果各调一次 `list_clap_plugins()`，VST3 同理 | 同一批库被**重复加载最多 4 次全量**，纯粹浪费 |
| P3 | 添加节点的右键菜单只有 `Instrument` / `Effect (Delay)` 两个叶子（`patch.rs:1256-1276`），一律生成 `VoiceSpec::Builtin`（`lib.rs:1950-1961`），声源要再去 Inspector 换 | 建节点与选声源是**两个地方两步操作**；且"先装内置、再换装插件"= 两次 `install_node` + 一次回收 |
| P4 | Inspector（Synth tab）里同时有源选择（Built-in / CLAP / VST3 / SoundFont，`lib.rs:4877-4990`）、effects 快捷加载（`:5125-5170`）、preset、参数栈、插件 GUI 区 | 一个面板承担"结构变更 + 参数编辑"两件事；且源选择是四项平铺按钮，插件一多同样没法找 |
| P5 | 叶子菜单是平铺 `menu_button` 列表 | 插件上规模后"滚不完"，无可搜索性 |

## 2 目标与非目标

**目标**：

1. 添加节点时**先定类别、再定声源**，节点带最终声源出生，只 install 一次、只记一步 undo。
2. 添加菜单改为**多级层级 + 过滤框**（Blender `Add` 菜单 / ComfyUI 节点搜索的形态），插件上规模仍可直达。
3. 插件目录**启动时一次扫描 + 磁盘缓存 + 进度条**；warm 启动零 dylib 加载，冷启动可见进度。
4. 一次扫描产出统一目录，乐器/效果两个视图在查询时分流（消灭 P2 的重复全量）。
5. Inspector 收敛为**纯参数视图**；结构变更（增 / 删 / 连线）全部归 patch。
6. 两处入口（添加 / 插线中）复用**同一个 picker**。

**非目标**（本期不做）：

- 不做"换源"动作（D3：删了重建，参数不迁移）；
- 不做插件 vendor 分组层级（`ClapPluginInfo` / `Vst3PluginInfo` 未读 vendor，需扩 host 元数据接口）；
- 不做收藏 / 隐藏 / 黑名单 / 重命名；
- 不做子进程扫描、不做后台线程扫描（D6：前台分片，理由见 §5.2）；
- 不做插件实例化的异步化（D13：同步 `spawn_rt`，见 §11.6）；
- **不动引擎、不动文档格式**（`PROJECT_VERSION` 不变，`NodeDoc` / `VoiceSpec` 字段零改动）；
- 不做音频/MIDI 端口的可用性过滤（能否作效果器一律由用户在 `Show all plugins…` 里自行决定）。

## 3 决策记录

| # | 决策 | 结论 | 理由 |
| :--- | :--- | :--- | :--- |
| D1 | 类型前置 | 添加时第一层就选 `Instrument` / `Effect`，节点带最终 `VoiceSpec` 出生 | 消掉"先建内置再换装"的两次 install + 一次回收；端口剖面（`node-graph.md` D2）在建节点时就已确定 |
| D2 | 职责切分 | **结构操作归 patch**（添加 / 删除 / 连线），**参数编辑归 Inspector** | Inspector 不该出现会重建处理器的动作；与 G4"按选中节点分派"一致 |
| D3 | **不提供"换源"** | 换声源 = 删掉节点 + 添加新节点，**不迁移任何参数 / 状态** | 跨声源参数语义不可复用（CLAP 原始域、VST3 归一化域、内置字段、SoundFont bank/program 各不同），迁移只会造出"看起来接上了其实毫无意义"的假象；删+建让状态干净、代码少一条路径。用户 2026-10-22 拍板 |
| D4 | 菜单形态 | 三级层级 + **叶子层过滤框**，不用裸 `menu_button` 长列表 | Blender `Add` 菜单顶部有搜索行、ComfyUI 双击出搜索框；插件规模下层级负责"分类"，过滤框负责"直达" |
| D5 | 全局搜索行 | `Add` 菜单顶部一行 `Search plugins…`，跨类别直达，按条目自身类别自动决定建 Instrument 还是 Effect | 与 ComfyUI 双击语义一致；熟手不必走三级 |
| D6 | **扫描在主线程、启动时一次、分片推进** | 启动时一次扫描 + 进度条；**每帧末尾只扫一个模块**，不建后台线程 | 用户 2026-10-22 拍板：启动一次性拿全列表，带进度条可观察。副产品：主线程 COM 已由 winit 初始化，**不需要任何 `CoInitializeEx`**（后台线程才需要，见 §5.2） |
| D7 | 缓存有效性 | 键 = `(path, mtime_ms, size)`；`CACHE_VERSION` 或**搜索目录集**变化 → 全量重扫；其余增量 | mtime+size 是插件升级的可靠信号；目录集变化（改 env）必须重扫 |
| D8 | 单趟扫描 | 一次全量产出 `Vec<PluginEntry>`，乐器/效果视图在查询时按 `(format, is_instrument)` 过滤 | 消灭 P2 的 4 次全量；缓存也只有一份 |
| D9 | 分类兜底 | 分类只决定**默认归属**，每个 picker 底部常驻 `Show all plugins…` | CLAP 靠 `features` 含 `instrument`、VST3 靠 `subCategories` 含 `Instrument` 判定，混血插件（drum machine / vocoder / 带音频入的合成器）必然有分错的 |
| D10 | Recent | 每类别最近 5 条，写 storage，跨会话 | 80% 的添加是同一两个插件，Recent 让常见路径变成一次点击 |
| D11 | 缓存落点 | eframe storage（`persistence` 已开），key `"plugin_cache"` | 零路径管道；详见 §9 Q3 的推荐理由 |
| D12 | Add 不自动连线 | 新节点按 `node-graph.md` D6 **悬空出生**；"插到现有链路"由连线中点菜单承担（复用同一 picker） | `insert_effect_common`（`lib.rs:3232`）现在会隐式插到轨链尾（mixer 前），这条隐式捷径与"图是显式的"相冲突 |
| D13 | 实例化仍同步 | 选中即 `spawn_rt`，期间状态栏 `Loading <name>…` + `request_repaint` | 异步实例化要处理插件线程亲和性（VST3 controller 的 main-thread 契约），风险远超本 spec，列 §11.6 |
| D14 | 分片粒度 | 每帧**最多一个模块**，且在**帧末**执行 | 帧末执行保证"挂住时屏幕上已经画出了肇事文件名"；一个模块一帧让冷启动进度可预期（200 个库 ≈ 200 帧 ≈ 3–7 s，期间 UI 可用） |

## 4 插件目录模型

```rust
// glitcherz-ui/src/plugin_catalog.rs（新模块；纯数据 + 纯函数，可脱离 egui 测试）

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginFormat { Clap, Vst3 }

/// 一条可实例化的插件描述。path 与 id 一起构成身份（VST3 一个模块可含多个 class）。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginEntry {
    pub format: PluginFormat,
    pub path: String,         // 模块路径（也是缓存键）
    pub id: String,           // CLAP plugin id / VST3 class id
    pub name: String,         // descriptor 显示名
    pub is_instrument: bool,  // 仅作分类提示（D9）
    pub mtime_ms: i64,
    pub size: u64,
}

pub const CACHE_VERSION: u32 = 1;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PluginCache {
    pub version: u32,
    pub dirs: Vec<String>,          // 扫描时的搜索目录集（D7）
    pub entries: Vec<PluginEntry>,
}
```

落点：`eframe::Storage` key `"plugin_cache"`，RON（`serde` + `ron` 已是依赖）。单条约 200 B，1000 条约 200 KB —— 退出时（含 eframe 周期自动存盘）写盘，量级无压力。

`is_instrument` 的来源沿用现有实现（不改 host 元数据接口）：CLAP `descriptor.features()` 含 `"instrument"`（`clap-host:233-236`）；VST3 `PClassInfo2.subCategories` 含 `"Instrument"`（`vst3-host:263`）。

## 5 扫描协议

### 5.1 启动时序

| 阶段 | 动作 | 耗时量级 |
| :--- | :--- | :--- |
| 帧 0（app 构造） | storage 读 `plugin_cache` → `entries`；`plan_refresh(entries, dirs)`（`read_dir` + 逐文件 `stat`） | <15 ms |
| 帧 0 判定 | `plan.rescan` 为空 → 目录立即可用，**无进度条**（warm 启动） | — |
| 帧 1..N | 每帧**末尾**扫 `pending[0]` 一个模块；顶部 2 px 进度条 + 状态栏 `Scanning plug-ins 37/210 — Kontakt.vst3` | 每模块 5–300 ms |
| 完成帧 | `merge` → 按 name 排序 → 写 storage → 清进度条 | — |

**扫描期间的行为**：窗口完全可用（走带、编辑、patch 操作都不受影响）；`Add ▸ Instrument ▸ CLAP…` 打开 picker 时，列表只显示已扫完的条目 + 顶部 `Scanning… 37/210`；内置项（Built-in Synth / Delay）不受影响。冷启动（首次运行、无缓存）即 `pending` = 全部文件，进度从 0 推进。

### 5.2 为什么是主线程（D6 的取舍）

**收益**：

- 仓库当前**没有任何 `CoInitialize` 调用**，VST3 元数据读取（`factory.cast::<IPluginFactory2>()` 走 `QueryInterface`）依赖 eframe/winit 在主线程做的 COM 初始化。放前台 = 什么都不用补；放后台线程就得自己 `CoInitializeEx`，且多一层跨线程状态与通道。
- 无 `crossbeam-channel`、无共享可变状态、无"扫描线程死了没人知道"。

**代价（必须承认）**：

- 插件在元数据读取阶段挂住（demo 弹窗、license 校验、损坏库）会**冻结 UI**；后台线程方案下 UI 还能动，只是扫描永不完成。
- in-process 无法 kill 挂住的调用。**不做防护**。唯一缓解见 D14：扫描发生在**帧末且一次一个**，所以挂住时屏幕上已经画出了 `Scanning … — <那个插件>`，用户能定位并手动移除该文件。

### 5.3 扫描/合并三个纯函数

```text
plan_refresh(cache, dirs) -> ScanPlan
  1. cache.version != CACHE_VERSION || cache.dirs != dirs  → 丢弃 entries（全量）
  2. files = union(scan_clap_files(), scan_vst3_files())     // read_dir，非递归（现有约定）
  3. 逐文件 stat(mtime, size)：
       cache 命中且 mtime/size 一致 → keep
       否则                          → rescan（新增或已变更）
  4. cache 中存在但 files 中不存在   → drop
  返回 ScanPlan { keep, rescan, removed }

scan_one(path) -> Vec<PluginEntry>   // 包一层 list_clap_plugins_in / list_vst3_plugins_in
                                     // 单文件失败 → log::warn + 空（现有行为）

merge(keep, scanned, removed) -> Vec<PluginEntry>   // 按 name 不区分大小写排序
```

单文件失败的语义：**跳过、不阻塞**（沿用 `list_*_in` 的 `log::warn` 后返回空）；失败的条目不会进缓存，下次启动会重试一次。

### 5.4 UI 侧状态

```rust
pub struct PluginCatalog {
    entries: Vec<PluginEntry>,        // 已确认可用（缓存命中 + 已扫完的增量）
    pending: Vec<PathBuf>,            // 待扫模块（D14：每帧取第一个）
    total: usize,                     // 本轮计划总数
    current: Option<String>,          // 当前/刚扫完的文件名（进度条文案）
    recent: Vec<(PluginFormat, String, String)>, // D10：最近 5 条 (format, path, id)
}
```

- `entries` 是 picker 的唯一数据源，查询时零磁盘 IO、零 dylib 加载。
- 进度条：顶部 2 px（`egui::TopBottomPanel` 内一行 `ProgressBar`，`desired_width` 撑满），文案放状态栏；扫描完成后自动消失，不占布局。
- `Rescan Plugins` 放在 `Add` 菜单底部：清 `entries` → 重跑 `plan_refresh`（空缓存 ⇒ 全量）→ 回到分片推进；不清 Recent。扫描进行中该项禁用。

## 6 添加菜单与 picker

### 6.1 结构

```text
Add ▸ 🔍 Search plugins…                 ← D5 全局，跨类别
    ▸ Instrument ▸ Built-in Synth        ← 动作：直接建
                 ▸ SoundFont…            ← 动作：文件对话框 → 建
                 ▸ CLAP…                 ← picker(kind=Instrument, format=Clap)
                 ▸ VST3…                 ← picker(kind=Instrument, format=Vst3)
    ▸ Effect     ▸ Built-in Delay        ← 动作：直接建
                 ▸ CLAP…                 ← picker(kind=Effect, format=Clap)
                 ▸ VST3…                 ← picker(kind=Effect, format=Vst3)
    ▸ Mixer
    ▸ Feedback Tap
    ▸ Keyboard In
```

- `Mixer` / `Feedback Tap` / `Keyboard In` 保持一级（没有声源维度，多套一层只是噪音）。
- 内置效果器目前只有 Delay（README 承诺的 EQ / 压缩 / 混响是后续功能）。**增长路径**：内置效果器 ≥2 个时，`Effect ▸ Built-in` 改为打开同一个 picker（数据源 = 内置注册表），菜单代码不动——picker 只吃条目列表，内置条目可用同一结构描述（`format` 位置放 `Builtin` 变体）。
- 连线中点菜单的 `Insert Effect` 复用同一 picker（换效果器的实际路径，见 §8.2）。

### 6.2 过滤规则

| 项 | 规则 |
| :--- | :--- |
| 匹配字段 | `name` + `path`（漏了 path 就搜不到同名不同厂商的库） |
| 分词 | `query.split_whitespace()` 全部命中（AND），大小写不敏感子串 |
| 空查询 | 分组显示 `Recent`（≤5）+ `All`（按 name 排序） |
| 零命中 | `No match for "xxx"` + `Show all plugins…`（清空查询，D9 兜底） |
| 排序 | 名称不区分大小写；稳定次序以 `(format, path, id)` 断开 |

### 6.3 picker 交互细则

不写成 `ui.menu_button` 子菜单，而是自绘浮层（`egui::Area` + `Frame`）——原因：需要菜单内放 `TextEdit`、滚动区、键盘导航，egui 原生 submenu 对长列表与文本输入支持不佳。形态与 Blender `Add` 菜单顶部的搜索行一致。

| 项 | 规格 |
| :--- | :--- |
| 尺寸 | 420 × 380（列表区高度自适应，上限 300） |
| 打开 | 输入框 `request_focus` + 全选 |
| 关闭 | `Esc` / 点击外部 / 选中条目 |
| 行高 | 22；左 `name`（超宽省略号），右淡色 `CLAP` / `VST3` 标签 |
| Tooltip | 完整模块路径 |
| 键盘 | `↑`/`↓` 移动高亮（初始第 0 行）、`Enter` 确认——输入框聚焦时仍拦截 |
| 选中 | 关闭 → 建节点；状态栏 `Loading <name>…` + `ctx.request_repaint()`（实例化同步，D13） |

两处复用（D3 取消换源后只剩两态）：

```rust
pub enum PickerMode {
    Add { category: NodeCategory, at: Pos2 },
    Insert { cable: usize, at: Pos2 },   // 连线中点
}
```

## 7 节点创建路径

`AddNodeKind` 现在的语义是"类别 + 具体"混装（`Instrument` / `Effect` / `Mixer` / `VirtualKeyboard` / `Feedback`，`patch.rs:201-217`）。拆成两层：

```rust
pub enum NodeCategory { Instrument, Effect, Utility }

pub enum NodeSource {
    Builtin(BuiltinId),                    // BuiltinId::Synth | BuiltinId::Delay
    SoundFont { path: String, bank: u8, program: u8 },
    Clap { path: String, plugin_id: String },
    Vst3 { path: String, class_id: String },
    Mixer,
    Feedback,
    Keyboard,
}
```

`create_patch_node(source, at)` 的顺序（关键是不留空壳、只装一次）：

```text
1. 容量 / 唯一性校验（MAX_NODES、一源一键盘节点）
2. 插件源先 spawn_rt（非实时线程）——失败 → 状态栏报错 + 返回 None，文档不动
3. 用最终 VoiceSpec 构造 NodeDoc → push
4. install_node() 一次（插件节点用已 spawn 的 instance，不再有"换装"）
5. 插件参数缓存按新 node_id 填充（沿用 load_*_voice 的枚举逻辑）
6. commit_edit("Add Instrument" / "Add Effect")  ← 一步 undo
7. place_node + register_track_chain
```

**不变式（测试断言）**：添加一个插件节点，引擎侧 `node_map` 只新增 1 条，`retiring_guards` 不增长（旧路径必然出现一次换装回收）。

## 8 Inspector 收口

### 8.1 删掉

（`lib.rs:4877-4990` 的源选择行）

- `Built-in Synth` / `CLAP Instruments…` / `VST3 Instruments…` / `SoundFont…` 四个按钮；
- `Effects` 段里的 `CLAP Effects…` / `VST3 Effects…`（D12：插入链路归连线中点菜单）。

**保留**：SoundFont 预设下拉（那是术语表里的"音色预设 preset"，不是源选择）、preset 工具条、参数栈、`Show Editor` / `Close Editor`、内联参数（G5）、`Clear All Effects`（针对该轨效果节点的批量删除，不是选择器，移到效果节点列表头部）。

### 8.2 换声源 = 删了重建（D3）

| 节点类型 | 换源流程 | 代价 |
| :--- | :--- | :--- |
| 效果器 | `Delete`（`node-graph.md` D19 旁路重连：`A→fx→B` 自动变 `A→B`）→ 右键连线中点 → `Insert Effect` → picker | 链路不断，几乎无痛 |
| 乐器 | `Delete`（出线 + MIDI 入线一并移除）→ `Add ▸ Instrument ▸ CLAP…` → 重拉 MIDI 与音频两根线 | 多两次拖线；**用户已确认接受**（参数本来也搬不过去） |

删节点本身是一步 undo；重建是另一步 undo —— 不合并为一步（想让"删+建"变成一步就得记住旧节点身份，等于把 Replace 偷渡回来）。

**后路（不在本期）**：若换源变成高频痛点，低成本做法是复用已有的"删 + 建 + 复连原连线"三件套做成一个动作，**不需要任何参数迁移逻辑**（那正是 D3 排除的部分）。

## 9 已决记录（2026-10-22 拍板）

| # | 问题 | 决定 | 备注 |
| :--- | :--- | :--- | :--- |
| Q1 | 换源入口 | **C：删了重建**（D3） | 参数跨声源不可复用，Replace 只剩"保连线"的价值，不值得单开一条状态迁移路径 |
| Q2 | 扫描进程模型 | **前台主线程 + 启动一次 + 分片 + 进度条**（D6/D14） | 比后台线程少一层 COM 初始化与跨线程状态；挂死会冻结 UI（已接受，缓解见 §5.2） |
| Q3 | 缓存落点 | **eframe storage**（D11） | 见下 |

Q3 推荐理由：缓存是**派生数据**，不需要用户可见、不需要外部工具读写；而 eframe storage 已经是本项目唯一持久化通道（最近文件就在里面）。再开一个自建 app-data 目录会让持久化分裂成"半在 storage 半在目录"，将来搬时还得写迁移。**触发迁移的条件**写在这里以免遗忘：当引入 settings / 预设库根目录时（`lib.rs:203` 的 `PathBuf::from("presets")` 是 cwd 相对路径，从别的目录启动就找不到，这个坑迟早要修），一次性把 storage + presets 一起搬到 app-data，缓存顺手迁移。

## 10 分阶段实施

- [x] **E1 目录与缓存内核**（不接 UI）✅ 2026-10-22：`crates/glitcherz-ui/src/plugin_catalog.rs` —— `PluginFormat` / `PluginEntry` / `PluginCache`（`version` + 打标签的 `dirs` `"<format>:<path>"`）/ `CandidateFile::stat`（`mtime_ms` + `size`）/ `collect_from_disk`（唯一读盘入口，`clap_search_paths`+`scan_clap_files` / `vst3_search_paths`+`scan_vst3_files`，目录与候选各自去重）/ `plan_refresh`（按模块路径分组：一个模块的多个 class 同生共死；mtime/size 命中即 keep；版本或目录集变化 → 全量 + 老路径进 `removed`）/ `merge` + `sort_entries`（`sort_by_cached_key`：name 小写 → format → path → id）/ `view`（格式 × 乐器查询期分流）/ `encode`/`decode`（RON）。8 项单测：多 class 保持、mtime/size 变更、新增+消失、版本与目录集失效、候选去重、排序（大小写与 format 次序）、RON 往返 + 垃圾输入、view 分区。`glitcherz-ui` 增 `ron` 依赖；全仓 fmt/clippy/test 绿（482 项）
- [x] **E2 扫描接线** ✅ 2026-10-22：`PluginCatalog` 状态机（`load` / `with_snapshot` / `upcoming` / `take_next` / `accept` / `progress` / `is_scanning` / `rescan` / `to_cache`）+ `scan_module`（按格式调 `list_*_plugins_in`，失败得空、进度照走）；`ui()` 开头画 `TopBottomPanel` 进度条（文案 = **队首**模块名，D14 的可观测性保证），帧尾扫一个模块并 `request_repaint`；四个惰性 `Option` 删除，菜单改走 `plugin_entries` / `plugin_empty_hint`；`save()` 用 `storage.set_string("plugin_cache")` 持久化（部分缓存合法）；patch 右键底部加 `Rescan Plug-ins`（`PatchAction::RescanPlugins`，E3 搬家）。单测 +2（pending 推进/进度/失败仍推进、warm 缓存即空闲），全仓 484 项绿。实测：热启动零 dylib 加载（Cardinal/Rack 日志条数 0），冷启动可观察 Cardinal 描述符枚举拉起整套 Rack 上下文（~140 ms）
- [ ] **E3 picker 与菜单**：`plugin_picker.rs`（浮层 + 过滤 + 键盘 + Recent）+ `Add` 菜单三级结构 + 全局 `Search plugins…`；验收：200+ 插件目录下三次以内点击或两三个字符直达
- [ ] **E4 创建路径重构**：`NodeCategory` / `NodeSource` 拆分、`create_patch_node` 一次 install、连线中点 `Insert` 复用 picker；验收：一步 undo + §7 不变式
- [ ] **E5 Inspector 收口**：删源选择行与 effects 快捷加载、`Clear All Effects` 挪到效果节点列表头部；验收：面板只剩 preset / 参数 / 编辑器 / 内联参数，且删除源选择后无残留缓存字段（`clap_plugins` / `clap_effects` / `vst3_*` 四个 `Option` 应被 `PluginCatalog` 取代）

门槛：`cargo fmt && cargo clippy --workspace -- -D warnings && cargo test --workspace` 全绿；手工验收 Dexed（CLAP）+ 本地 200+ VST3 目录。

## 11 风险与开放问题

1. **挂死插件冻结 UI**（D6 的固有代价）：帧末单模块扫描保证屏幕上已显示肇事文件名（D14）；in-process 无法 kill，不做防护。若实测成为问题，唯一根治是子进程扫描（本 spec 排除）。
2. **缓存漂移**：插件原地替换但 mtime/size 未变（罕见，打包器保留时间戳时会遇到）→ `Rescan Plugins` 兜底。
3. **分类错误**：混血插件分错层（D9）→ `Show all plugins…` 兜底。
4. **缓存写盘时机**：eframe storage 退出/周期写盘；崩溃丢失 → 下次冷扫（冷扫有进度条，不再是"静默卡死"）。
5. **冷启动时长**：200 个模块 × 每帧 1 个 = 200 帧；若平均 30 ms/模块则 ≈ 6 s 才凑齐列表。可接受（期间 UI 可用、内置项可用），若实测太慢，退路是把 D14 改成"每帧预算 6 ms 内尽量多扫"（牺牲"挂住时显示的是哪一个"的精度）。
6. **同步实例化卡 UI**（D13）：选大型采样器仍会顿几百毫秒到数秒。开放问题：能否异步 `spawn_rt`——VST3 `IEditController` 有 main-thread 契约（`plugin-gui.md` §9 的 `IPlugView` 侧），需实测后才敢动；本期只做"状态栏 loading 提示"，不承诺不卡。
7. **与 U8 的范围重叠**：本 spec 覆盖 U8 的"源选择"项并额外删掉 effects 快捷加载；U8 剩余项（automation lane 迁 timeline、master 电平表挪位、死文案清理）不受影响，仍需单独收口。
8. **`is_instrument` 的语义边界**：判定只看单个 feature / subcategory 字符串，未做"是否接受音频输入"的探测——效果器视图里出现带音频入的乐器、乐器视图里出现纯 MIDI 盒子的情况都会存在；D9 是唯一防线。
