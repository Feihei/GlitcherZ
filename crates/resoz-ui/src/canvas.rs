//! 节点画布（P5d MVP，spec §9 P5d）：egui 自绘节点图视图与连线编辑。
//!
//! 职责边界：画布只做**视图 + 交互意图**——平移/缩放、节点框绘制
//! （类型着色）、cable 连线绘制、拉线交互、选中与删除。所有副作用
//! 经 [`CanvasAction`] 返回，由 `ResoZApp::apply_canvas_actions` 统一
//! 落文档 + 引擎（画布不直接持有引擎/守卫）。
//!
//! 坐标约定：节点位置存**图空间**坐标（会话内存，不持久化；P5c 双
//! 身份约定——画布面对文档 node_id + 固定实体，不感知引擎 id）。

use std::collections::HashMap;

use eframe::egui;
use egui::{Color32, FontId, Pos2, Rect, Sense, Vec2};

use resoz_model::{AutoParam, CableKind, GraphDoc, NodeDoc, PortRef, VoiceSpec};

/// 画布实体身份：文档节点 + 固定实体（主输出 / 时间轴源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanvasId {
    /// 文档节点（持久化 node_id，≥ 2）。
    Node(u64),
    /// 主输出（保留身份，D6）。
    Master,
    /// 时间轴 MIDI 轨出口。
    MidiTrack(usize),
    /// 时间轴自动化轨出口。
    AutoTrack(usize),
}

/// 画布一帧内产生的交互意图（由调用方落库，画布不做副作用）。
#[derive(Debug, Clone)]
pub enum CanvasAction {
    /// 音频拉线：源文档节点 → 目标（节点或主输出）。
    ConnectAudio { from_doc: u64, to: PortRef },
    /// MIDI 拉线：时间轴轨道 → 乐器文档节点。
    ConnectMidi { track: usize, to_doc: u64 },
    /// 自动化拉线（用户已从弹出菜单选定参数）。
    ConnectAuto {
        index: usize,
        to_doc: u64,
        param: AutoParam,
    },
    /// 删除连线（文档连线表下标）。
    DeleteCable(usize),
    /// 删除节点（文档 node_id；仅效果器会被接受，调用方校验）。
    DeleteNode(u64),
    /// 把某节点的主属轨道设为当前编辑轨道。
    SelectTrack(usize),
}

/// 画布依赖的外部数据快照（不可变借用，规避借用冲突）。
pub struct CanvasDeps<'a> {
    /// 项目图文档（nodes + cables）。
    pub doc: &'a GraphDoc,
    /// 活跃 MIDI 轨数。
    pub track_count: usize,
    /// 自动化轨道数。
    pub auto_count: usize,
    /// 给定文档节点 id，返回可自动化的参数选项 `(标签, 参数)`。
    pub auto_options: &'a dyn Fn(u64) -> Vec<(String, AutoParam)>,
}

/// 拖拽中的连线（源 = 拖出的输出口所属实体）。
#[derive(Debug, Clone, Copy)]
struct CableDrag {
    from: CanvasId,
}

/// 画布跨帧状态。
#[derive(Debug, Default)]
pub struct CanvasState {
    /// 视口平移（图空间坐标，图原点在画布左上角时为 (0,0)）。
    pan: Vec2,
    /// 缩放系数（0.35..3.0）。
    zoom: f32,
    /// 实体 → 图空间位置（会话内存；缺失时按轨道链自动布点）。
    positions: HashMap<CanvasId, Pos2>,
    /// 当前选中项（节点或连线）。
    selection: Option<Selection>,
    /// 进行中的拖拽。
    drag: Option<DragKind>,
    /// 自动化拉线放下后待选参数：`(轨道下标, 目标文档节点, 弹出位置)`。
    pending_auto: Option<(usize, u64, Pos2)>,
}

#[derive(Debug, Clone, Copy)]
enum Selection {
    Node(CanvasId),
    Cable(usize),
}

#[derive(Debug, Clone, Copy)]
enum DragKind {
    /// 拖动视口。
    Pan,
    /// 拖动节点。
    Node(CanvasId),
    /// 拉线中（源输出口）。
    Cable(CableDrag),
}

/// 音频节点框尺寸。
const NODE_SIZE: Vec2 = Vec2::new(150.0, 64.0);
/// 时间轴源（MIDI / 自动化）框尺寸。
const SOURCE_SIZE: Vec2 = Vec2::new(132.0, 40.0);
/// 画布固定高度。
const CANVAS_HEIGHT: f32 = 300.0;
/// 输出口圆半径 / 命中半径。
const PORT_RADIUS: f32 = 6.0;
const PORT_HIT_RADIUS: f32 = 12.0;
/// 连线命中距离。
const CABLE_HIT_DIST: f32 = 8.0;
/// 缩放范围。
const ZOOM_MIN: f32 = 0.35;
const ZOOM_MAX: f32 = 3.0;

/// 类型着色（spec P5d）：描边按类型区分，填充为深色底。
fn node_color(id: CanvasId, doc: &GraphDoc) -> (Color32, Color32) {
    let stroke = match id {
        CanvasId::Master => Color32::from_rgb(230, 110, 90),
        CanvasId::MidiTrack(_) => Color32::from_rgb(90, 200, 110),
        CanvasId::AutoTrack(_) => Color32::from_rgb(230, 190, 70),
        CanvasId::Node(id) => match doc.node_by_id(id) {
            Some(NodeDoc::Instrument { .. }) => Color32::from_rgb(90, 160, 220),
            Some(NodeDoc::Effect { .. }) => Color32::from_rgb(220, 150, 70),
            Some(NodeDoc::ChannelStrip { .. }) => Color32::from_rgb(110, 190, 120),
            None => Color32::from_gray(120),
        },
    };
    let fill = match id {
        CanvasId::Master => Color32::from_rgb(52, 32, 30),
        CanvasId::MidiTrack(_) => Color32::from_rgb(28, 44, 30),
        CanvasId::AutoTrack(_) => Color32::from_rgb(48, 42, 24),
        CanvasId::Node(_) => Color32::from_rgb(34, 38, 46),
    };
    (stroke, fill)
}

/// 连线颜色（按类型）。
fn cable_color(kind: CableKind) -> Color32 {
    match kind {
        CableKind::Audio => Color32::from_rgb(150, 152, 160),
        CableKind::Midi => Color32::from_rgb(90, 200, 110),
        CableKind::Automation => Color32::from_rgb(230, 190, 70),
    }
}

/// 实体框尺寸。
fn size_of(id: CanvasId) -> Vec2 {
    match id {
        CanvasId::MidiTrack(_) | CanvasId::AutoTrack(_) => SOURCE_SIZE,
        _ => NODE_SIZE,
    }
}

/// 实体标签。
fn label_of(id: CanvasId, doc: &GraphDoc) -> String {
    match id {
        CanvasId::Master => "主输出".to_string(),
        CanvasId::MidiTrack(t) => format!("MIDI 轨 {}", t + 1),
        CanvasId::AutoTrack(i) => format!("自动化 {}", i + 1),
        CanvasId::Node(node_id) => match doc.node_by_id(node_id) {
            Some(NodeDoc::Instrument { voice, track, .. }) => match voice {
                VoiceSpec::Builtin => format!("合成器 · 轨 {}", track + 1),
                VoiceSpec::Clap { plugin_id, .. } => {
                    format!("♪ {}", truncate(plugin_id, 16))
                }
                VoiceSpec::Vst3 { class_id, .. } => format!("♪ {}", truncate(class_id, 16)),
            },
            Some(NodeDoc::Effect { voice, track, .. }) => match voice {
                VoiceSpec::Builtin => format!("延迟 · 轨 {}", track + 1),
                VoiceSpec::Clap { plugin_id, .. } => {
                    format!("Fx {}", truncate(plugin_id, 14))
                }
                VoiceSpec::Vst3 { class_id, .. } => format!("Fx {}", truncate(class_id, 14)),
            },
            Some(NodeDoc::ChannelStrip { track, .. }) => format!("通道条 · 轨 {}", track + 1),
            None => format!("节点 {node_id}"),
        },
    }
}

/// 自动化连线终点参数的短标签。
fn auto_param_label(param: &AutoParam) -> String {
    match param {
        AutoParam::TrackGain => "音量".to_string(),
        AutoParam::TrackPan => "声像".to_string(),
        AutoParam::FilterCutoff => "截止".to_string(),
        AutoParam::PluginParam { name, .. } => name.clone(),
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

impl CanvasState {
    /// 图空间 → 屏幕空间。
    fn to_screen(&self, rect_min: Pos2, p: Pos2) -> Pos2 {
        Pos2::new(
            rect_min.x + (p.x - self.pan.x) * self.zoom,
            rect_min.y + (p.y - self.pan.y) * self.zoom,
        )
    }

    /// 实体屏幕矩形（不存在位置时返回 `None`）。
    fn screen_rect(&self, rect_min: Pos2, id: CanvasId) -> Option<Rect> {
        let p = *self.positions.get(&id)?;
        Some(Rect::from_min_size(
            self.to_screen(rect_min, p),
            size_of(id),
        ))
    }

    /// 输出口屏幕位置（源与音频节点都有；主输出没有）。
    fn out_port_screen(&self, rect_min: Pos2, id: CanvasId) -> Option<Pos2> {
        match id {
            CanvasId::Master => None,
            other => self
                .screen_rect(rect_min, other)
                .map(|r| Pos2::new(r.right(), r.center().y)),
        }
    }

    /// 输入口屏幕位置（音频节点与主输出；源没有）。
    fn in_port_screen(&self, rect_min: Pos2, id: CanvasId) -> Option<Pos2> {
        match id {
            CanvasId::MidiTrack(_) | CanvasId::AutoTrack(_) => None,
            other => self
                .screen_rect(rect_min, other)
                .map(|r| Pos2::new(r.left(), r.center().y)),
        }
    }

    /// 补齐缺失实体的默认布点（不覆盖已有位置；清理失效条目）。
    fn ensure_positions(&mut self, doc: &GraphDoc, track_count: usize, auto_count: usize) {
        let mut wanted: Vec<CanvasId> = Vec::new();

        // 每轨一列：MIDI 源 → 乐器 → 效果器栈 → 通道条。
        let mut effect_slot = vec![0usize; track_count];
        for node in &doc.nodes {
            let track = node.track().min(track_count.saturating_sub(1));
            let y = 32.0 + track as f32 * 132.0;
            let (id, pos) = match node {
                NodeDoc::Instrument { .. } => (CanvasId::Node(node.node_id()), Pos2::new(200.0, y)),
                NodeDoc::Effect { voice, .. } => {
                    let k = if matches!(voice, VoiceSpec::Builtin) {
                        0
                    } else {
                        1 + effect_slot[track]
                    };
                    effect_slot[track] += 1;
                    (
                        CanvasId::Node(node.node_id()),
                        Pos2::new(420.0, y + k as f32 * 64.0),
                    )
                }
                NodeDoc::ChannelStrip { .. } => {
                    (CanvasId::Node(node.node_id()), Pos2::new(640.0, y))
                }
            };
            wanted.push(id);
            self.positions.entry(id).or_insert(pos);
        }
        for t in 0..track_count {
            let id = CanvasId::MidiTrack(t);
            wanted.push(id);
            self.positions
                .entry(id)
                .or_insert(Pos2::new(16.0, 32.0 + t as f32 * 132.0));
        }
        let y_bottom = 32.0 + track_count as f32 * 132.0 + 24.0;
        for i in 0..auto_count {
            let id = CanvasId::AutoTrack(i);
            wanted.push(id);
            self.positions
                .entry(id)
                .or_insert(Pos2::new(16.0, y_bottom + i as f32 * 56.0));
        }
        wanted.push(CanvasId::Master);
        let y_center = (32.0 + track_count.max(1) as f32 * 132.0) * 0.5;
        self.positions
            .entry(CanvasId::Master)
            .or_insert(Pos2::new(870.0, y_center));

        // 清理已删除实体的位置，防 hashmap 无界增长。
        self.positions.retain(|id, _| wanted.contains(id));
    }

    /// 命中输出口（屏幕距离）。
    fn hit_out_port(&self, rect_min: Pos2, sp: Pos2) -> Option<CanvasId> {
        for &id in self.positions.keys() {
            if let Some(port) = self.out_port_screen(rect_min, id) {
                if port.distance(sp) <= PORT_HIT_RADIUS {
                    return Some(id);
                }
            }
        }
        None
    }

    /// 命中实体框。
    fn hit_node(&self, rect_min: Pos2, sp: Pos2) -> Option<CanvasId> {
        for &id in self.positions.keys() {
            if let Some(r) = self.screen_rect(rect_min, id) {
                if r.contains(sp) {
                    return Some(id);
                }
            }
        }
        None
    }

    /// 命中连线（返回文档连线表下标）。
    fn hit_cable(&self, rect_min: Pos2, sp: Pos2, doc: &GraphDoc) -> Option<usize> {
        for (i, cable) in doc.cables.iter().enumerate() {
            let (Some(from), Some(to)) = (canvas_port_from(&cable.from), canvas_port_to(&cable.to))
            else {
                continue;
            };
            let (Some(p0), Some(p3)) = (
                self.out_port_screen(rect_min, from),
                self.in_port_screen(rect_min, to),
            ) else {
                continue;
            };
            for pt in bezier_points(bezier_controls(p0, p3), 24) {
                if pt.distance(sp) <= CABLE_HIT_DIST {
                    return Some(i);
                }
            }
        }
        None
    }
}

/// `PortRef` → 连线源实体。
fn canvas_port_from(port: &PortRef) -> Option<CanvasId> {
    match port {
        PortRef::Node(id) => Some(CanvasId::Node(*id)),
        PortRef::MidiTrack(t) => Some(CanvasId::MidiTrack(*t)),
        PortRef::AutoTrack(i) => Some(CanvasId::AutoTrack(*i)),
        PortRef::Master => None, // 主输出不是合法音频源。
    }
}

/// `PortRef` → 连线目标实体。
fn canvas_port_to(port: &PortRef) -> Option<CanvasId> {
    match port {
        PortRef::Node(id) => Some(CanvasId::Node(*id)),
        PortRef::Master => Some(CanvasId::Master),
        _ => None,
    }
}

/// 拉线类型 → 目标实体是否接受（端口类型规则，spec §7/D3）。
fn accepts(kind: CableKind, target: CanvasId, doc: &GraphDoc) -> bool {
    if let CanvasId::Node(id) = target {
        let Some(node) = doc.node_by_id(id) else {
            return false;
        };
        match kind {
            CableKind::Audio => {
                matches!(node, NodeDoc::Effect { .. } | NodeDoc::ChannelStrip { .. })
            }
            CableKind::Midi => matches!(node, NodeDoc::Instrument { .. }),
            CableKind::Automation => {
                matches!(
                    node,
                    NodeDoc::Instrument { .. } | NodeDoc::ChannelStrip { .. }
                )
            }
        }
    } else {
        matches!(target, CanvasId::Master) && kind == CableKind::Audio
    }
}

/// 三次贝塞尔控制点：水平伸出量随距离伸缩。
fn bezier_controls(p0: Pos2, p3: Pos2) -> [Pos2; 4] {
    let dx = ((p3.x - p0.x).abs() * 0.5).clamp(40.0, 140.0);
    [
        p0,
        Pos2::new(p0.x + dx, p0.y),
        Pos2::new(p3.x - dx, p3.y),
        p3,
    ]
}

/// 两点线性插值。
fn lerp2(a: Pos2, b: Pos2, t: f32) -> Pos2 {
    Pos2::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
}

/// 贝塞尔均匀采样（命中检测用）。
fn bezier_points(c: [Pos2; 4], samples: usize) -> Vec<Pos2> {
    (0..=samples)
        .map(|i| {
            let t = i as f32 / samples as f32;
            let (a, b, d, e) = (c[0], c[1], c[2], c[3]);
            let ab = lerp2(a, b, t);
            let bd = lerp2(b, d, t);
            let de = lerp2(d, e, t);
            let abbd = lerp2(ab, bd, t);
            let bdde = lerp2(bd, de, t);
            lerp2(abbd, bdde, t)
        })
        .collect()
}

/// 绘制一帧画布并收集交互意图。
pub fn show(ui: &mut egui::Ui, state: &mut CanvasState, deps: CanvasDeps<'_>) -> Vec<CanvasAction> {
    let mut actions = Vec::new();
    let doc = deps.doc;
    state.ensure_positions(doc, deps.track_count, deps.auto_count);

    // ---- 工具条 ----
    ui.horizontal(|ui| {
        ui.strong("节点图");
        if ui.button("－").clicked() {
            state.zoom = (state.zoom / 1.2).clamp(ZOOM_MIN, ZOOM_MAX);
        }
        if ui.button("＋").clicked() {
            state.zoom = (state.zoom * 1.2).clamp(ZOOM_MIN, ZOOM_MAX);
        }
        if ui.button("复位").clicked() {
            state.pan = Vec2::ZERO;
            state.zoom = 1.0;
        }
        ui.weak("拖背景平移 · Ctrl+滚轮缩放 · 拖输出口拉线 · 选中后 Delete 删除");
    });

    // ---- 画布 ----
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), CANVAS_HEIGHT),
        Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_rgb(22, 24, 28));

    // Ctrl+滚轮缩放（指针锚定）。
    let zoom_delta = ui.ctx().input(|i| i.zoom_delta());
    if response.hovered() && zoom_delta != 0.0 {
        let old = state.zoom;
        state.zoom = (state.zoom * (1.0 + zoom_delta)).clamp(ZOOM_MIN, ZOOM_MAX);
        if let Some(sp) = response.hover_pos() {
            let graph = state.pan + (sp - rect.min) / old;
            state.pan = graph - (sp - rect.min) / state.zoom;
        }
    }

    let pointer = response.interact_pointer_pos();
    let drag_started = response.drag_started();
    let drag_stopped = response.drag_stopped();

    // ---- 按下：命中检测并转入对应拖拽 ----
    if drag_started {
        let sp = pointer.unwrap_or(rect.min);
        if let Some(source) = state.hit_out_port(rect.min, sp) {
            state.selection = None;
            state.drag = Some(DragKind::Cable(CableDrag { from: source }));
        } else if let Some(id) = state.hit_node(rect.min, sp) {
            state.selection = Some(Selection::Node(id));
            // 点节点顺带切换主编辑轨道（画布与面板联动）。
            match id {
                CanvasId::Node(node_id) => {
                    if let Some(node) = doc.node_by_id(node_id) {
                        actions.push(CanvasAction::SelectTrack(node.track()));
                    }
                }
                CanvasId::MidiTrack(t) => actions.push(CanvasAction::SelectTrack(t)),
                _ => {}
            }
            state.drag = Some(DragKind::Node(id));
        } else if let Some(i) = state.hit_cable(rect.min, sp, doc) {
            state.selection = Some(Selection::Cable(i));
            state.drag = None;
        } else {
            state.selection = None;
            state.drag = Some(DragKind::Pan);
        }
    }

    // ---- 拖拽进行中 ----
    if response.dragged() {
        match state.drag {
            Some(DragKind::Pan) => {
                state.pan -= response.drag_delta() / state.zoom;
            }
            Some(DragKind::Node(id)) => {
                if let Some(p) = state.positions.get_mut(&id) {
                    *p += response.drag_delta() / state.zoom;
                }
            }
            _ => {}
        }
    }

    // ---- 拉线松开：命中目标并生成动作 ----
    if drag_stopped {
        if let Some(DragKind::Cable(drag)) = state.drag.take() {
            if let Some(sp) = pointer {
                if let Some(target) = state.hit_node(rect.min, sp) {
                    let kind = match drag.from {
                        CanvasId::MidiTrack(t) => {
                            if accepts(CableKind::Midi, target, doc) {
                                if let CanvasId::Node(to_doc) = target {
                                    actions.push(CanvasAction::ConnectMidi { track: t, to_doc });
                                }
                                None
                            } else {
                                Some(CableKind::Midi)
                            }
                        }
                        CanvasId::AutoTrack(index) => {
                            if accepts(CableKind::Automation, target, doc) && drag.from != target {
                                if let CanvasId::Node(to_doc) = target {
                                    // 参数在弹出菜单选定后再落线。
                                    state.pending_auto =
                                        Some((index, to_doc, sp + Vec2::new(4.0, 4.0)));
                                }
                            }
                            None
                        }
                        CanvasId::Node(from_doc) => {
                            if accepts(CableKind::Audio, target, doc) && drag.from != target {
                                let to = match target {
                                    CanvasId::Node(id) => PortRef::Node(id),
                                    CanvasId::Master => PortRef::Master,
                                    _ => PortRef::Master,
                                };
                                actions.push(CanvasAction::ConnectAudio { from_doc, to });
                            }
                            None
                        }
                        CanvasId::Master => None,
                    };
                    let _ = kind; // 类型不符：静默放弃（MVP 不弹提示）。
                }
            }
        }
    }

    // ---- 绘制连线（节点之下）----
    for (i, cable) in doc.cables.iter().enumerate() {
        let (Some(from), Some(to)) = (canvas_port_from(&cable.from), canvas_port_to(&cable.to))
        else {
            continue;
        };
        let (Some(p0), Some(p3)) = (
            state.out_port_screen(rect.min, from),
            state.in_port_screen(rect.min, to),
        ) else {
            continue;
        };
        let selected = matches!(state.selection, Some(Selection::Cable(j)) if j == i);
        let stroke = egui::Stroke::new(
            if selected { 3.5 } else { 2.0 },
            if selected {
                Color32::WHITE
            } else {
                cable_color(cable.kind)
            },
        );
        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
            bezier_controls(p0, p3),
            false,
            Color32::TRANSPARENT,
            stroke,
        ));
        // 自动化线在中点标注终点参数。
        if cable.kind == CableKind::Automation {
            let mid = bezier_points(bezier_controls(p0, p3), 24)[12];
            painter.text(
                mid + Vec2::new(0.0, -10.0),
                egui::Align2::CENTER_CENTER,
                auto_param_label(&cable.param),
                FontId::proportional(11.0),
                cable_color(CableKind::Automation),
            );
        }
    }

    // ---- 拉线预览 ----
    if let Some(DragKind::Cable(drag)) = state.drag {
        if let (Some(sp), Some(p0)) = (pointer, state.out_port_screen(rect.min, drag.from)) {
            let kind = match drag.from {
                CanvasId::MidiTrack(_) => CableKind::Midi,
                CanvasId::AutoTrack(_) => CableKind::Automation,
                _ => CableKind::Audio,
            };
            painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                bezier_controls(p0, sp),
                false,
                Color32::TRANSPARENT,
                egui::Stroke::new(2.0, cable_color(kind).gamma_multiply(0.7)),
            ));
            // 悬停目标高亮。
            if let Some(target) = state.hit_node(rect.min, sp) {
                if accepts(kind, target, doc) && drag.from != target {
                    if let Some(r) = state.screen_rect(rect.min, target) {
                        painter.rect_stroke(
                            r,
                            4.0,
                            egui::Stroke::new(2.0, Color32::WHITE),
                            egui::StrokeKind::Outside,
                        );
                    }
                }
            }
        }
    }

    // ---- 绘制节点 ----
    for (&id, &pos) in state.positions.iter() {
        let size = size_of(id);
        let min = state.to_screen(rect.min, pos);
        let r = Rect::from_min_size(min, size * state.zoom);
        if !r.intersects(rect) {
            continue;
        }
        let (stroke, fill) = node_color(id, doc);
        let selected = matches!(state.selection, Some(Selection::Node(sel)) if sel == id);
        painter.rect_filled(r, 4.0, fill);
        painter.rect_stroke(
            r,
            4.0,
            egui::Stroke::new(if selected { 2.5 } else { 1.2 }, stroke),
            egui::StrokeKind::Middle,
        );

        let text = label_of(id, doc);
        let text_pos = match id {
            CanvasId::MidiTrack(_) | CanvasId::AutoTrack(_) => r.center(),
            _ => Pos2::new(r.center().x, r.top() + 18.0),
        };
        painter.text(
            text_pos,
            egui::Align2::CENTER_CENTER,
            text,
            FontId::proportional(
                if matches!(id, CanvasId::MidiTrack(_) | CanvasId::AutoTrack(_)) {
                    12.0
                } else {
                    13.0
                },
            ),
            Color32::from_rgb(225, 225, 228),
        );

        // 输入口 / 输出口。
        let port_color = if matches!(id, CanvasId::MidiTrack(_)) {
            cable_color(CableKind::Midi)
        } else if matches!(id, CanvasId::AutoTrack(_)) {
            cable_color(CableKind::Automation)
        } else {
            cable_color(CableKind::Audio)
        };
        if let Some(p) = state.out_port_screen(rect.min, id) {
            painter.circle_filled(p, PORT_RADIUS, port_color);
            painter.circle_stroke(p, PORT_RADIUS, egui::Stroke::new(1.0, Color32::BLACK));
        }
        if let Some(p) = state.in_port_screen(rect.min, id) {
            painter.circle_filled(p, PORT_RADIUS, Color32::from_gray(90));
            painter.circle_stroke(p, PORT_RADIUS, egui::Stroke::new(1.0, Color32::BLACK));
        }
    }

    // ---- Delete 删除选中项（悬停画布时）----
    if response.hovered() && ui.ctx().input(|i| i.key_pressed(egui::Key::Delete)) {
        match state.selection {
            Some(Selection::Cable(i)) => {
                actions.push(CanvasAction::DeleteCable(i));
                state.selection = None;
            }
            Some(Selection::Node(CanvasId::Node(node_id))) => {
                actions.push(CanvasAction::DeleteNode(node_id));
                state.selection = None;
            }
            _ => {}
        }
    }

    // ---- 自动化参数弹出菜单 ----
    if let Some((index, to_doc, menu_pos)) = state.pending_auto {
        let options = (deps.auto_options)(to_doc);
        let mut picked: Option<AutoParam> = None;
        egui::Area::new(egui::Id::new("resoz_canvas_auto_menu"))
            .fixed_pos(menu_pos)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(140.0);
                    ui.strong("自动化目标参数");
                    for (label, param) in &options {
                        if ui.button(label).clicked() {
                            picked = Some(param.clone());
                        }
                    }
                });
            });
        if let Some(param) = picked {
            actions.push(CanvasAction::ConnectAuto {
                index,
                to_doc,
                param,
            });
            state.pending_auto = None;
        } else if ui
            .ctx()
            .input(|i| i.pointer.any_click() || i.key_pressed(egui::Key::Escape))
        {
            state.pending_auto = None;
        }
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use resoz_model::Project;

    #[test]
    fn ensure_positions_covers_all_entities() {
        let project = Project::default();
        let doc = &project.graph;
        let track_count = project.midi_tracks.len();
        let auto_count = project.automation_tracks.len();
        let mut state = CanvasState::default();
        state.ensure_positions(doc, track_count, auto_count);

        for node in &doc.nodes {
            assert!(
                state
                    .positions
                    .contains_key(&CanvasId::Node(node.node_id())),
                "文档节点 {} 应有位置",
                node.node_id()
            );
        }
        assert!(state.positions.contains_key(&CanvasId::Master));
        for t in 0..track_count {
            assert!(state.positions.contains_key(&CanvasId::MidiTrack(t)));
        }
        for i in 0..auto_count {
            assert!(state.positions.contains_key(&CanvasId::AutoTrack(i)));
        }
    }

    #[test]
    fn port_type_rules_reject_mismatches() {
        let project = Project::default();
        let doc = &project.graph;
        let instrument = doc.instrument_of_track(0).unwrap().node_id();
        let strip = doc.channel_strip_of_track(0).unwrap().node_id();
        let delay = doc.builtin_delay_of_track(0).unwrap().node_id();

        // 音频：效果器/通道条/主输出接受，乐器拒绝。
        assert!(accepts(CableKind::Audio, CanvasId::Node(delay), doc));
        assert!(accepts(CableKind::Audio, CanvasId::Node(strip), doc));
        assert!(accepts(CableKind::Audio, CanvasId::Master, doc));
        assert!(!accepts(CableKind::Audio, CanvasId::Node(instrument), doc));
        // MIDI：乐器接受，其余拒绝。
        assert!(accepts(CableKind::Midi, CanvasId::Node(instrument), doc));
        assert!(!accepts(CableKind::Midi, CanvasId::Node(strip), doc));
        assert!(!accepts(CableKind::Midi, CanvasId::Master, doc));
        // 自动化：乐器/通道条接受。
        assert!(accepts(CableKind::Automation, CanvasId::Node(strip), doc));
        assert!(accepts(
            CableKind::Automation,
            CanvasId::Node(instrument),
            doc
        ));
        assert!(!accepts(CableKind::Automation, CanvasId::Master, doc));
    }
}
