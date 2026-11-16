# 聚合 Curve 输入口（UI）Spec

> ID: F-044 · 域: UI / PATCH / EDIT
> 状态：**已实施**（2026-11-12）。M1 已落地，纯 UI 改动，不动 model / engine / 工程文件。
> **修订 2026-11-16**：聚合口从结构输入口上方移到**下方**（盒子底部条带）；Mixer 不再
> 是特例，同样增高 `CURVE_PORT_ROW`（原「从端口区借位」使通道行距随通道数向
> `PORT_SPACING` 爬升，见 D2 修订）。另修复：2 通道 Mixer 曾被 `audio_inputs()==2`
> 误判为 Z0 双输入效果器而在端口名上叠加 "Main"/"Mod"（D4 修订）。
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
| D2 | **专用几何槽** | 新增图空间常量 `CURVE_PORT_ROW = 16`。**修订 2026-11-16**：槽位于盒子**底部**（inline 行之下），所有可控制目标（含 Mixer）的 `size_of` 高度统一 +16；`port_area` 底部让出该槽。原方案（槽在 header 之下、Mixer 不增高从端口区借位）废弃——借位使 n 通道的行距变成 `(n·30 − 16)/n`，随通道数增加向 30 爬升（实测 24→24.7→26→26.8→27.3…）；增高后端口区恢复完整的 `max(n·30, 64)`，n≥3 行距恒为 30。副作用：默认摆放下相邻轨的 1–2 通道 Mixer（盒高 138 > 132 轨距）会重叠 6px；n≥3 通道 Mixer 在修订前就已超出轨距，属既有类别，用户拖动即可。 |
| D3 | **端点解析** | `cable_controls` 在 `cable.kind == Automation` 且目标实体有聚合口时，终点取 `curve_port_screen`；否则沿用 `in_ports_screen[cable.in_port]`。`Timeline In` 的 Tempo 口与各轨录音口（本身已按 cable 类型着色）保持不变。 |
| D4 | **绘制** | 与输入口同一视觉语言：节点底色挖洞 + 3 px 黄色环（`cable_color(Automation)`）+ 黑色细描边；右侧 `PORT_NAME_GAP` 处显示 10 pt 灰字 `Curve`，与 `Main` / `Mod` / 轨道端口名同一字号与间距。**修订 2026-11-16**：Z0 的 `Main` / `Mod` 端口标注只画在双输入效果器上（`dual_input_caption` 带 `!is_mixer()` 守卫）——2 通道 Mixer 的 `audio_inputs()` 同样返回 2，原判断会在 Mixer 通道名标签的同一坐标上叠加 "Main"/"Mod"，形成文字重叠。 |
| D5 | **标签与扇形展开** | 中点参数标签继续由 `auto_param_label_at` 生成（效果器 slot 用 descriptor 短名）。对 `(from, to)` 相同的 Automation 电缆组按文档顺序分配 lane，贝塞尔控制点垂直偏移 `(lane - (n-1)/2) * 16 px`（中点因此相差约 12 px），只移动控制点、不移动端点。偏移在 `cable_controls_indexed` 内派生，绘制、命中、切割、中点菜单共用同一几何。 |
| D6 | **无数据变更** | 聚合口是纯显示层派生：`CableDoc.in_port` 对 Automation 仍可保持 0，不新增字段，不需要工程迁移。 |
| D7 | **拖放交互不变** | 落线仍按目标节点矩形命中后弹参数菜单；不需要精确命中聚合口。聚合口不参与 `hit_in_port`，不影响 audio / MIDI 的通道选择。 |

## 3. 几何（2026-11-16 修订）

```
节点框（graph space，可控制目标）
┌──────────────────────────────┐
│ 标题                          │  ← title band
├──────────────────────────────┤
│ ● 结构输入口 0                │  ← port_area：header 之下、
│ ● 结构输入口 1                │    (inline 行 + CURVE_PORT_ROW) 之上均分
│──────────────────────────────│
│ inline 参数行                 │
│──────────────────────────────│
│ ◯ Curve                      │  ← CURVE_PORT_ROW = 16（聚合口在槽中心）
└──────────────────────────────┘
```

- 盒高：所有可控制目标（Instrument / Effect / **Mixer** / Feedback）统一
  `base + inline_rows·16 + CURVE_PORT_ROW`；Mixer 不再有「不增高」特例。
- 聚合口：`(r.left(), r.bottom() − CURVE_PORT_ROW/2 · zoom)`。
- 结构口区（`port_area`）：`[r.top() + header_band, r.bottom() − (inline + 16)·zoom]`，
  Mixer 通道口在该区内均分——行距 `max(n·30, 64)/n`，n≥3 恒为 30。
- 单结构口节点：口仍在盒子竖直中心（盒高不变，口位不变），与输出口对齐。
- inline 行块顶边 = `r.bottom() − (rows·16 + CURVE_PORT_ROW)·zoom`，由
  `inline_rows_top()` 唯一提供（绘制与命中同源）；`curve_inlet_row()` 返回的已是高度，
  调用点不得再次乘 `CURVE_PORT_ROW`。

## 4. 测试

1. `curve_port_screen`：Instrument / Effect / Mixer / Feedback 有口；Curve Map / Keyboard / LFO / Arpeggiator / Portamento / MidiEnvelope / Input / Timeline / Timeline In / Master 无。
2. 目标节点高度比非目标节点多 `CURVE_PORT_ROW`（Mixer 同样增高）；固定实体尺寸不变。
3. 聚合口位于盒子底部条带内，且在**最后一个**结构口之下（间距 ≥ 15 px）。
4. Automation 电缆的终点等于聚合口；Audio / MIDI 电缆终点仍是结构口。
5. 同 `(from, to)` 的两条 Automation 电缆中点相差 `AUTOMATION_LANE_GAP`，标签不重叠。
6. 现有 D27 测试（结构口颜色跟随结构信号）保持通过。
7. 修订新增：Mixer 通道行距与通道数无关（n≥3 恒为 `PORT_SPACING`），聚合口不与通道口相触。
8. 修订新增：`dual_input_caption` 只给双输入效果器返回 `Main`/`Mod`；2 通道 Mixer 返回空。
9. 修订新增：`inline_rows_stay_inside_the_box`——1–6 通道 Mixer（1 行 pan）与内置 Synth（2 行）的 inline 行块顶边必须 ≥ header 底边，且行块高度 + `CURVE_PORT_ROW` 正好等于 `size_of` 的预留空间。

## 5. 里程碑

- **M1（本 spec 全部内容）**：常量与几何、`curve_port_screen`、端点解析、绘制、扇形偏移、单测、`control-tracks.md` §9 注解同步。