# 聚合 Curve 输入口（UI）Spec

> ID: F-044 · 域: UI / PATCH / EDIT
> 状态：**已实施**（2026-11-12）。M1 已落地，纯 UI 改动，不动 model / engine / 工程文件。
> 关联：`control-tracks.md`（automation 电缆）、`curve-map.md`（归一化与映射）、`midi-nodes.md`（节点控制源扇出）。

## 1. 背景与目标

`CableKind::Automation` 电缆当前复用途目标节点的**结构输入口**作为落点
（`patch.rs` 的 `node_in_port_kind` 注释与 D27）：一条黄色虚线可以落在青色或绿色的
MIDI / 音频环上。端口环显示的是节点消费的结构信号，线条与端口因此互相矛盾。

同一节点的多个参数被控制时（例如三条 curve 轨分别控制 cutoff、resonance、gain，
或一个 LFO 扇出到多个参数），所有 Automation 电缆的终点完全相同，贝塞尔几何也完全
重合，视觉上只剩一条线；中点参数标签逐条叠在同一位置，用户无法分辨控制目标。

本 spec 交付一个**聚合 Curve 输入口**：

- 每个可被通知参数控制的节点，在左边缘顶部常驻一个**独立的黄色环口**；
- 所有指向该节点的 Automation 电缆都落到这个口，不再占用结构输入口；
- 连线上保留参数名标签，并让同源 / 同目标的多条线**扇形展开**，标签不再重叠。

**非目标**

- 不做「一条曲线控制多个目标」（C5，2026-11-11 拍板暂不做）。一条曲线仍只控制一个参数。
- 不做「每个参数一个输入口」（`param-ports` B 阶段）。
- 不改 model 的 `CableDoc`、engine 的 `AutoRoute` / `EventCable`，不改 `PROJECT_VERSION`。
- 不改 Curve Map 节点唯一的控制入口（它本身就只有一个黄色口）。

## 2. 决策

| # | 决策 | 内容 |
| :-- | :-- | :-- |
| D1 | **目标节点集合** | `NodeDoc::accepts_control()` 且不是 `CurveMap`：Instrument / Effect / Mixer / Feedback。Curve Map 的唯一入口本来就是控制口，继续沿用原口。Keyboard / LFO / Arpeggiator / Portamento / MidiEnvelope / Input / Audio Track 不显示聚合口。 |
| D2 | **专用几何槽** | 新增图空间常量 `CURVE_PORT_ROW = 16`。非 Mixer 目标节点（Instrument / Effect / Feedback）的 `size_of` 高度 +16；port_area 顶部（`header_band` 之后）统一预留 16。Mixer 复用现有 port area 的顶部空间、**盒子不增高**（轨道列按 132 px 堆叠，避免相邻轨混音器重叠），其通道口从预留槽下方开始均分。聚合口位于该槽中心；双输入效果器的结构口同样从槽下方开始。 |
| D3 | **端点解析** | `cable_controls` 在 `cable.kind == Automation` 且目标实体有聚合口时，终点取 `curve_port_screen`；否则沿用 `in_ports_screen[cable.in_port]`。`Timeline In` 的 Tempo 口与各轨录音口（本身已按 cable 类型着色）保持不变。 |
| D4 | **绘制** | 与输入口同一视觉语言：节点底色挖洞 + 3 px 黄色环（`cable_color(Automation)`）+ 黑色细描边；右侧 `PORT_NAME_GAP` 处显示 10 pt 灰字 `Curve`，与 `Main` / `Mod` / 轨道端口名同一字号与间距。 |
| D5 | **标签与扇形展开** | 中点参数标签继续由 `auto_param_label_at` 生成（效果器 slot 用 descriptor 短名）。对 `(from, to)` 相同的 Automation 电缆组按文档顺序分配 lane，贝塞尔控制点垂直偏移 `(lane - (n-1)/2) * 16 px`（中点因此相差约 12 px），只移动控制点、不移动端点。偏移在 `cable_controls_indexed` 内派生，绘制、命中、切割、中点菜单共用同一几何。 |
| D6 | **无数据变更** | 聚合口是纯显示层派生：`CableDoc.in_port` 对 Automation 仍可保持 0，不新增字段，不需要工程迁移。 |
| D7 | **拖放交互不变** | 落线仍按目标节点矩形命中后弹参数菜单；不需要精确命中聚合口。聚合口不参与 `hit_in_port`，不影响 audio / MIDI 的通道选择。 |

## 3. 几何

```
节点框（graph space）
┌──────────────────────────────┐
│ 标题                          │  ← title band
├──────────────────────────────┤
│ ◯ Curve                      │  ← CURVE_PORT_ROW = 16（聚合口在槽中心）
│                              │
│ ● Main / 通道 0               │  ← 原 port_area，向下移 16
│ ● 通道 1                     │
│──────────────────────────────│
│ inline 参数行                 │
└──────────────────────────────┘
```

- 聚合口：`(r.left(), r.top() + (header_band + CURVE_PORT_ROW/2) * zoom)`。
- 单结构口节点：结构口仍在（下移后的）`port_area` 中心，与聚合口中心距约 20 px。
- Mixer：盒子高度不变，预留槽占用现有 port area 顶部；单通道也按 port area 均分。
- Mixer / 双输入效果器：结构口在 `port_area.top`（已含 `CURVE_PORT_ROW`）以下均分。

## 4. 测试

1. `curve_port_screen`：Instrument / Effect / Mixer / Feedback 有口；Curve Map / Keyboard / LFO / Arpeggiator / Portamento / MidiEnvelope / Input / Timeline / Timeline In / Master 无。
2. 目标节点高度比非目标节点多 `CURVE_PORT_ROW`；固定实体尺寸不变。
3. 聚合口位于 `header_band` 与第一个结构口之间，且在节点框内。
4. Automation 电缆的终点等于聚合口；Audio / MIDI 电缆终点仍是结构口。
5. 同 `(from, to)` 的两条 Automation 电缆中点相差 `AUTOMATION_LANE_GAP`，标签不重叠。
6. 现有 D27 测试（结构口颜色跟随结构信号）保持通过。

## 5. 里程碑

- **M1（本 spec 全部内容）**：常量与几何、`curve_port_screen`、端点解析、绘制、扇形偏移、单测、`control-tracks.md` §9 注解同步。