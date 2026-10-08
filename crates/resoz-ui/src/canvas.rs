//! Node canvas (P5d MVP, spec §9 P5d): egui self-drawn node graph view and cable editing.
//!
//! Responsibility boundary: the canvas only does **view + interaction intent** — pan/zoom, node
//! box drawing (color-coded by type), cable drawing, drag-to-connect interaction, selection and
//! deletion. All side effects are returned via [`CanvasAction`] and applied to the document +
//! engine in one place by `ResoZApp::apply_canvas_actions` (the canvas never touches the
//! engine/guards directly).
//!
//! Coordinate convention: node positions are stored in **graph space** (session memory, not
//! persisted; P5c dual identity — the canvas deals with document node_ids + fixed entities and
//! is unaware of engine ids).

use std::collections::HashMap;

use eframe::egui;
use egui::{Color32, FontId, Pos2, Rect, Sense, Vec2};

use resoz_model::{AutoParam, CableKind, GraphDoc, NodeDoc, PortRef, VoiceSpec};

/// Canvas entity identity: document nodes + fixed entities (master output / timeline sources).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanvasId {
    /// Document node (persisted node_id, ≥ 2).
    Node(u64),
    /// Master output (reserved identity, D6).
    Master,
    /// Timeline MIDI track source.
    MidiTrack(usize),
    /// Timeline automation track source.
    AutoTrack(usize),
}

/// Interaction intents produced within one canvas frame (applied by the caller; the canvas has
/// no side effects).
#[derive(Debug, Clone)]
pub enum CanvasAction {
    /// Audio cable drag: source document node → target (node or master output).
    ConnectAudio { from_doc: u64, to: PortRef },
    /// MIDI cable drag: timeline track → instrument document node.
    ConnectMidi { track: usize, to_doc: u64 },
    /// Automation cable drag (parameter already picked by the user from the popup menu).
    ConnectAuto {
        index: usize,
        to_doc: u64,
        param: AutoParam,
    },
    /// Delete a cable (index into the document cable table).
    DeleteCable(usize),
    /// Delete a node (document node_id; only effects are accepted, validated by the caller).
    DeleteNode(u64),
    /// Set a node's owning track as the current editing track.
    SelectTrack(usize),
}

/// Snapshot of external data the canvas depends on (immutable borrows to avoid borrow conflicts).
pub struct CanvasDeps<'a> {
    /// Project graph document (nodes + cables).
    pub doc: &'a GraphDoc,
    /// Active MIDI track count.
    pub track_count: usize,
    /// Automation track count.
    pub auto_count: usize,
    /// Given a document node id, return automatable parameter options `(label, parameter)`.
    pub auto_options: &'a dyn Fn(u64) -> Vec<(String, AutoParam)>,
}

/// Cable being dragged (source = the entity owning the dragged output port).
#[derive(Debug, Clone, Copy)]
struct CableDrag {
    from: CanvasId,
}

/// Canvas cross-frame state.
#[derive(Debug, Default)]
pub struct CanvasState {
    /// Viewport pan (graph-space coordinates; (0,0) when the graph origin is at the canvas
    /// top-left).
    pan: Vec2,
    /// Zoom factor (0.35..3.0).
    zoom: f32,
    /// Entity → graph-space position (session memory; missing entries are auto-placed along
    /// track chains).
    positions: HashMap<CanvasId, Pos2>,
    /// Current selection (node or cable).
    selection: Option<Selection>,
    /// Drag in progress.
    drag: Option<DragKind>,
    /// Parameters awaiting selection after dropping an automation cable drag: `(track index,
    /// target document node, popup position)`.
    pending_auto: Option<(usize, u64, Pos2)>,
}

#[derive(Debug, Clone, Copy)]
enum Selection {
    Node(CanvasId),
    Cable(usize),
}

#[derive(Debug, Clone, Copy)]
enum DragKind {
    /// Pan the viewport.
    Pan,
    /// Drag a node.
    Node(CanvasId),
    /// Cable dragging in progress (source output port).
    Cable(CableDrag),
}

/// Audio node box size.
const NODE_SIZE: Vec2 = Vec2::new(150.0, 64.0);
/// Timeline source (MIDI / automation) box size.
const SOURCE_SIZE: Vec2 = Vec2::new(132.0, 40.0);
/// Fixed canvas height.
const CANVAS_HEIGHT: f32 = 300.0;
/// Output port circle radius / hit radius.
const PORT_RADIUS: f32 = 6.0;
const PORT_HIT_RADIUS: f32 = 12.0;
/// Cable hit distance.
const CABLE_HIT_DIST: f32 = 8.0;
/// Zoom range.
const ZOOM_MIN: f32 = 0.35;
const ZOOM_MAX: f32 = 3.0;

/// Type coloring (spec P5d): stroke varies by type, fill is a dark base.
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

/// Cable color (by type).
fn cable_color(kind: CableKind) -> Color32 {
    match kind {
        CableKind::Audio => Color32::from_rgb(150, 152, 160),
        CableKind::Midi => Color32::from_rgb(90, 200, 110),
        CableKind::Automation => Color32::from_rgb(230, 190, 70),
    }
}

/// Entity box size.
fn size_of(id: CanvasId) -> Vec2 {
    match id {
        CanvasId::MidiTrack(_) | CanvasId::AutoTrack(_) => SOURCE_SIZE,
        _ => NODE_SIZE,
    }
}

/// Entity label.
fn label_of(id: CanvasId, doc: &GraphDoc) -> String {
    match id {
        CanvasId::Master => "Master Out".to_string(),
        CanvasId::MidiTrack(t) => format!("MIDI Track {}", t + 1),
        CanvasId::AutoTrack(i) => format!("Automation {}", i + 1),
        CanvasId::Node(node_id) => match doc.node_by_id(node_id) {
            Some(NodeDoc::Instrument { voice, track, .. }) => match voice {
                VoiceSpec::Builtin => format!("Synth · Track {}", track + 1),
                VoiceSpec::Clap { plugin_id, .. } => {
                    format!("♪ {}", truncate(plugin_id, 16))
                }
                VoiceSpec::Vst3 { class_id, .. } => format!("♪ {}", truncate(class_id, 16)),
            },
            Some(NodeDoc::Effect { voice, track, .. }) => match voice {
                VoiceSpec::Builtin => format!("Delay · Track {}", track + 1),
                VoiceSpec::Clap { plugin_id, .. } => {
                    format!("Fx {}", truncate(plugin_id, 14))
                }
                VoiceSpec::Vst3 { class_id, .. } => format!("Fx {}", truncate(class_id, 14)),
            },
            Some(NodeDoc::ChannelStrip { track, .. }) => {
                format!("Channel Strip · Track {}", track + 1)
            }
            None => format!("Node {node_id}"),
        },
    }
}

/// Short label for an automation cable endpoint parameter.
fn auto_param_label(param: &AutoParam) -> String {
    match param {
        AutoParam::TrackGain => "Volume".to_string(),
        AutoParam::TrackPan => "Pan".to_string(),
        AutoParam::FilterCutoff => "Cutoff".to_string(),
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
    /// Graph space → screen space.
    fn to_screen(&self, rect_min: Pos2, p: Pos2) -> Pos2 {
        Pos2::new(
            rect_min.x + (p.x - self.pan.x) * self.zoom,
            rect_min.y + (p.y - self.pan.y) * self.zoom,
        )
    }

    /// Entity screen rect (returns `None` when no position exists).
    fn screen_rect(&self, rect_min: Pos2, id: CanvasId) -> Option<Rect> {
        let p = *self.positions.get(&id)?;
        Some(Rect::from_min_size(
            self.to_screen(rect_min, p),
            size_of(id),
        ))
    }

    /// Output port screen position (sources and audio nodes have one; the master output does
    /// not).
    fn out_port_screen(&self, rect_min: Pos2, id: CanvasId) -> Option<Pos2> {
        match id {
            CanvasId::Master => None,
            other => self
                .screen_rect(rect_min, other)
                .map(|r| Pos2::new(r.right(), r.center().y)),
        }
    }

    /// Input port screen position (audio nodes and master output; sources do not).
    fn in_port_screen(&self, rect_min: Pos2, id: CanvasId) -> Option<Pos2> {
        match id {
            CanvasId::MidiTrack(_) | CanvasId::AutoTrack(_) => None,
            other => self
                .screen_rect(rect_min, other)
                .map(|r| Pos2::new(r.left(), r.center().y)),
        }
    }

    /// Fill in default placements for missing entities (never overwrites existing positions;
    /// prunes stale entries).
    fn ensure_positions(&mut self, doc: &GraphDoc, track_count: usize, auto_count: usize) {
        let mut wanted: Vec<CanvasId> = Vec::new();

        // One column per track: MIDI source → instrument → effect stack → channel strip.
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

        // Prune positions of deleted entities to prevent unbounded hashmap growth.
        self.positions.retain(|id, _| wanted.contains(id));
    }

    /// Hit-test an output port (screen distance).
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

    /// Hit-test an entity box.
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

    /// Hit-test a cable (returns the index into the document cable table).
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

/// `PortRef` → cable source entity.
fn canvas_port_from(port: &PortRef) -> Option<CanvasId> {
    match port {
        PortRef::Node(id) => Some(CanvasId::Node(*id)),
        PortRef::MidiTrack(t) => Some(CanvasId::MidiTrack(*t)),
        PortRef::AutoTrack(i) => Some(CanvasId::AutoTrack(*i)),
        PortRef::Master => None, // The master output is not a valid audio source.
    }
}

/// `PortRef` → cable target entity.
fn canvas_port_to(port: &PortRef) -> Option<CanvasId> {
    match port {
        PortRef::Node(id) => Some(CanvasId::Node(*id)),
        PortRef::Master => Some(CanvasId::Master),
        _ => None,
    }
}

/// Cable kind → whether the target entity accepts it (port type rules, spec §7/D3).
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

/// Cubic bezier control points: horizontal extension scales with distance.
fn bezier_controls(p0: Pos2, p3: Pos2) -> [Pos2; 4] {
    let dx = ((p3.x - p0.x).abs() * 0.5).clamp(40.0, 140.0);
    [
        p0,
        Pos2::new(p0.x + dx, p0.y),
        Pos2::new(p3.x - dx, p3.y),
        p3,
    ]
}

/// Linear interpolation between two points.
fn lerp2(a: Pos2, b: Pos2, t: f32) -> Pos2 {
    Pos2::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
}

/// Uniform bezier sampling (for hit testing).
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

/// Draw one frame of the canvas and collect interaction intents.
pub fn show(ui: &mut egui::Ui, state: &mut CanvasState, deps: CanvasDeps<'_>) -> Vec<CanvasAction> {
    let mut actions = Vec::new();
    let doc = deps.doc;
    state.ensure_positions(doc, deps.track_count, deps.auto_count);

    // ---- Toolbar ----
    ui.horizontal(|ui| {
        ui.strong("Node Graph");
        if ui.button("－").clicked() {
            state.zoom = (state.zoom / 1.2).clamp(ZOOM_MIN, ZOOM_MAX);
        }
        if ui.button("＋").clicked() {
            state.zoom = (state.zoom * 1.2).clamp(ZOOM_MIN, ZOOM_MAX);
        }
        if ui.button("Reset").clicked() {
            state.pan = Vec2::ZERO;
            state.zoom = 1.0;
        }
        ui.weak("Drag bg to pan · Ctrl+wheel to zoom · drag ports to connect · select + Delete to remove");
    });

    // ---- Canvas ----
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), CANVAS_HEIGHT),
        Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_rgb(22, 24, 28));

    // Ctrl+wheel zoom (anchored at the pointer).
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

    // ---- Press: hit-test and enter the matching drag ----
    if drag_started {
        let sp = pointer.unwrap_or(rect.min);
        if let Some(source) = state.hit_out_port(rect.min, sp) {
            state.selection = None;
            state.drag = Some(DragKind::Cable(CableDrag { from: source }));
        } else if let Some(id) = state.hit_node(rect.min, sp) {
            state.selection = Some(Selection::Node(id));
            // Clicking a node also switches the main editing track (canvas-panel linkage).
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

    // ---- Drag in progress ----
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

    // ---- Cable drag released: hit-test the target and emit actions ----
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
                                    // The cable is committed after the parameter is picked in
                                    // the popup menu.
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
                    let _ = kind; // Type mismatch: silently abandon (MVP shows no prompt).
                }
            }
        }
    }

    // ---- Draw cables (beneath nodes) ----
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
        // Label automation cables with the endpoint parameter at the midpoint.
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

    // ---- Cable drag preview ----
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
            // Highlight the hovered target.
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

    // ---- Draw nodes ----
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

        // Input / output ports.
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

    // ---- Delete removes the selection (while hovering the canvas) ----
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

    // ---- Automation parameter popup menu ----
    if let Some((index, to_doc, menu_pos)) = state.pending_auto {
        let options = (deps.auto_options)(to_doc);
        let mut picked: Option<AutoParam> = None;
        egui::Area::new(egui::Id::new("resoz_canvas_auto_menu"))
            .fixed_pos(menu_pos)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(140.0);
                    ui.strong("Automation Target Param");
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
        let auto_count = project.control_tracks.len();
        let mut state = CanvasState::default();
        state.ensure_positions(doc, track_count, auto_count);

        for node in &doc.nodes {
            assert!(
                state
                    .positions
                    .contains_key(&CanvasId::Node(node.node_id())),
                "document node {} should have a position",
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

        // Audio: effects/channel strips/master output accept; instruments reject.
        assert!(accepts(CableKind::Audio, CanvasId::Node(delay), doc));
        assert!(accepts(CableKind::Audio, CanvasId::Node(strip), doc));
        assert!(accepts(CableKind::Audio, CanvasId::Master, doc));
        assert!(!accepts(CableKind::Audio, CanvasId::Node(instrument), doc));
        // MIDI: instruments accept; everything else rejects.
        assert!(accepts(CableKind::Midi, CanvasId::Node(instrument), doc));
        assert!(!accepts(CableKind::Midi, CanvasId::Node(strip), doc));
        assert!(!accepts(CableKind::Midi, CanvasId::Master, doc));
        // Automation: instruments/channel strips accept.
        assert!(accepts(CableKind::Automation, CanvasId::Node(strip), doc));
        assert!(accepts(
            CableKind::Automation,
            CanvasId::Node(instrument),
            doc
        ));
        assert!(!accepts(CableKind::Automation, CanvasId::Master, doc));
    }
}
