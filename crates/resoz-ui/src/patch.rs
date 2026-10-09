//! Node patch (P5d MVP, spec §9 P5d): egui self-drawn node graph view and cable editing.
//!
//! Responsibility boundary: the patch only does **view + interaction intent** — pan (middle
//! drag)/zoom (wheel), node box drawing (color-coded by type), cable drawing, drag-to-connect
//! interaction, point/marquee selection and deletion. All side effects are returned via
//! [`PatchAction`] and applied to the document + engine in one place by
//! `ResoZApp::apply_patch_actions` (the patch never touches the engine/guards directly).
//!
//! Coordinate convention: node positions are stored in **graph space** (session memory, not
//! persisted; P5c dual identity — the patch deals with document node_ids + fixed entities and
//! is unaware of engine ids).

use std::collections::HashMap;

use eframe::egui;
use egui::{Color32, FontId, Pos2, Rect, Sense, Vec2};

use resoz_model::{AutoParam, CableKind, GraphDoc, NodeDoc, PortRef, VoiceSpec};

/// Patch entity identity: document nodes + fixed entities (master output /
/// the arrangement timeline).
///
/// T4 (spec `docs/spec/timeline.md`): the per-track source boxes merged into
/// **one explicit Timeline node** — no inputs, `n` outputs (n = MIDI tracks +
/// control tracks), undeletable. `PortRef::MidiTrack/AutoTrack` semantics are
/// unchanged; this is a pure presentation-layer merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatchId {
    /// Document node (persisted node_id, ≥ 2).
    Node(u64),
    /// Master output (reserved identity, D6).
    Master,
    /// The arrangement timeline (one box; one output port per track).
    Timeline,
}

/// Interaction intents produced within one patch frame (applied by the caller; the patch has
/// no side effects).
#[derive(Debug, Clone)]
pub enum PatchAction {
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

/// Snapshot of external data the patch depends on (immutable borrows to avoid borrow conflicts).
pub struct PatchDeps<'a> {
    /// Project graph document (nodes + cables).
    pub doc: &'a GraphDoc,
    /// Active MIDI track count.
    pub track_count: usize,
    /// Automation track count.
    pub auto_count: usize,
    /// Given a document node id, return automatable parameter options `(label, parameter)`.
    pub auto_options: &'a dyn Fn(u64) -> Vec<(String, AutoParam)>,
}

/// Cable being dragged (source = the entity + output port owning the drag).
#[derive(Debug, Clone, Copy)]
struct CableDrag {
    from: PatchId,
    /// Output port index (only [`PatchId::Timeline`] has more than one).
    port: usize,
}

/// Patch cross-frame state.
#[derive(Debug)]
pub struct PatchState {
    /// Viewport pan (graph-space coordinates; (0,0) when the graph origin is at the patch
    /// top-left).
    pan: Vec2,
    /// Zoom factor (0.35..3.0; starts at 1.0 — `f32::default()` would be 0.0 and collapse
    /// the whole graph onto the pan point, the top-left corner).
    zoom: f32,
    /// Entity → graph-space position (session memory; missing entries are auto-placed along
    /// track chains).
    positions: HashMap<PatchId, Pos2>,
    /// Current selection (nodes and/or cables).
    selection: Selection,
    /// Drag in progress.
    drag: Option<DragKind>,
    /// Parameters awaiting selection after dropping an automation cable drag: `(track index,
    /// target document node, popup position)`.
    pending_auto: Option<(usize, u64, Pos2)>,
}

impl Default for PatchState {
    fn default() -> Self {
        Self {
            pan: Vec2::ZERO,
            zoom: 1.0,
            positions: HashMap::default(),
            selection: Selection::default(),
            drag: None,
            pending_auto: None,
        }
    }
}

/// Current selection (multi-select: marquee box selection can select several nodes).
#[derive(Debug, Default, Clone)]
struct Selection {
    nodes: Vec<PatchId>,
    cables: Vec<usize>,
}

impl Selection {
    fn contains_node(&self, id: PatchId) -> bool {
        self.nodes.contains(&id)
    }

    fn toggle_node(&mut self, id: PatchId) {
        if let Some(i) = self.nodes.iter().position(|&n| n == id) {
            self.nodes.remove(i);
        } else {
            self.nodes.push(id);
        }
    }

    fn toggle_cable(&mut self, i: usize) {
        if let Some(k) = self.cables.iter().position(|&c| c == i) {
            self.cables.remove(k);
        } else {
            self.cables.push(i);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum DragKind {
    /// Drag a node (all selected nodes move together when the pressed node is
    /// part of the current selection).
    Node(PatchId),
    /// Cable dragging in progress (source output port).
    Cable(CableDrag),
    /// Marquee box selection in progress (`start` = screen-space anchor,
    /// `add` = Shift append mode instead of replacing the selection).
    BoxSelect { start: Pos2, add: bool },
}

/// Audio node box size.
const NODE_SIZE: Vec2 = Vec2::new(150.0, 64.0);
/// Timeline box size per output port row (width fixed, height scales).
const TIMELINE_WIDTH: f32 = 150.0;
const TIMELINE_ROW_HEIGHT: f32 = 26.0;
const TIMELINE_MIN_HEIGHT: f32 = 40.0;
/// Fixed patch height.
const CANVAS_HEIGHT: f32 = 300.0;
/// Output port circle radius / hit radius.
const PORT_RADIUS: f32 = 6.0;
const PORT_HIT_RADIUS: f32 = 12.0;
/// Cable hit distance.
const CABLE_HIT_DIST: f32 = 8.0;
/// Zoom range.
const ZOOM_MIN: f32 = 0.35;
const ZOOM_MAX: f32 = 3.0;

/// Timeline output port count (MIDI tracks first, then control tracks).
fn timeline_port_count(midi_count: usize, auto_count: usize) -> usize {
    midi_count + auto_count
}

/// A timeline output port index → its `PortRef` (0..midi = MIDI tracks).
fn timeline_port_ref(port: usize, midi_count: usize) -> PortRef {
    if port < midi_count {
        PortRef::MidiTrack(port)
    } else {
        PortRef::AutoTrack(port - midi_count)
    }
}

/// Timeline box size (height scales with the port count).
fn timeline_size(midi_count: usize, auto_count: usize) -> Vec2 {
    Vec2::new(
        TIMELINE_WIDTH,
        (TIMELINE_ROW_HEIGHT * timeline_port_count(midi_count, auto_count) as f32 + 14.0)
            .max(TIMELINE_MIN_HEIGHT),
    )
}

/// Type coloring (spec P5d): stroke varies by type, fill is a dark base.
fn node_color(id: PatchId, doc: &GraphDoc) -> (Color32, Color32) {
    let stroke = match id {
        PatchId::Master => Color32::from_rgb(230, 110, 90),
        PatchId::Timeline => Color32::from_rgb(170, 120, 225),
        PatchId::Node(id) => match doc.node_by_id(id) {
            Some(NodeDoc::Instrument { .. }) => Color32::from_rgb(90, 160, 220),
            Some(NodeDoc::Effect { .. }) => Color32::from_rgb(220, 150, 70),
            Some(NodeDoc::ChannelStrip { .. }) => Color32::from_rgb(110, 190, 120),
            None => Color32::from_gray(120),
        },
    };
    let fill = match id {
        PatchId::Master => Color32::from_rgb(52, 32, 30),
        PatchId::Timeline => Color32::from_rgb(38, 30, 50),
        PatchId::Node(_) => Color32::from_rgb(34, 38, 46),
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
fn size_of(id: PatchId, midi_count: usize, auto_count: usize) -> Vec2 {
    match id {
        PatchId::Timeline => timeline_size(midi_count, auto_count),
        _ => NODE_SIZE,
    }
}

/// Entity label.
fn label_of(id: PatchId, doc: &GraphDoc) -> String {
    match id {
        PatchId::Master => "Master Out".to_string(),
        PatchId::Timeline => "Timeline".to_string(),
        PatchId::Node(node_id) => match doc.node_by_id(node_id) {
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

impl PatchState {
    /// Graph space → screen space.
    fn to_screen(&self, rect_min: Pos2, p: Pos2) -> Pos2 {
        Pos2::new(
            rect_min.x + (p.x - self.pan.x) * self.zoom,
            rect_min.y + (p.y - self.pan.y) * self.zoom,
        )
    }

    /// Entity screen rect (returns `None` when no position exists).
    ///
    /// Single source of truth for entity geometry: box size is scaled by the
    /// zoom factor here so drawing, ports and ALL hit tests stay in sync.
    fn screen_rect(
        &self,
        rect_min: Pos2,
        id: PatchId,
        midi_count: usize,
        auto_count: usize,
    ) -> Option<Rect> {
        let p = *self.positions.get(&id)?;
        Some(Rect::from_min_size(
            self.to_screen(rect_min, p),
            size_of(id, midi_count, auto_count) * self.zoom,
        ))
    }

    /// Output ports of one entity: `(port index, screen position)` pairs. The
    /// timeline exposes one port per track spread along its right edge; audio
    /// nodes have a single port; the master output has none.
    fn out_ports_screen(
        &self,
        rect_min: Pos2,
        id: PatchId,
        midi_count: usize,
        auto_count: usize,
    ) -> Vec<(usize, Pos2)> {
        let Some(r) = self.screen_rect(rect_min, id, midi_count, auto_count) else {
            return Vec::new();
        };
        match id {
            PatchId::Master => Vec::new(),
            PatchId::Timeline => {
                let n = timeline_port_count(midi_count, auto_count);
                (0..n)
                    .map(|i| {
                        let y = r.top() + (i as f32 + 0.5) * (r.height() / n.max(1) as f32);
                        (i, Pos2::new(r.right(), y))
                    })
                    .collect()
            }
            PatchId::Node(_) => vec![(0, Pos2::new(r.right(), r.center().y))],
        }
    }

    /// Input port screen position (audio nodes and master output; the timeline
    /// has none — it is a pure source, D14/T4).
    fn in_port_screen(
        &self,
        rect_min: Pos2,
        id: PatchId,
        midi_count: usize,
        auto_count: usize,
    ) -> Option<Pos2> {
        match id {
            PatchId::Timeline => None,
            other => self
                .screen_rect(rect_min, other, midi_count, auto_count)
                .map(|r| Pos2::new(r.left(), r.center().y)),
        }
    }

    /// Fill in default placements for missing entities (never overwrites existing positions;
    /// prunes stale entries).
    fn ensure_positions(&mut self, doc: &GraphDoc, track_count: usize, _auto_count: usize) {
        let mut wanted: Vec<PatchId> = Vec::new();

        // One column per track: instrument → effect stack → channel strip.
        let mut effect_slot = vec![0usize; track_count];
        for node in &doc.nodes {
            let track = node.track().min(track_count.saturating_sub(1));
            let y = 32.0 + track as f32 * 132.0;
            let (id, pos) = match node {
                NodeDoc::Instrument { .. } => (PatchId::Node(node.node_id()), Pos2::new(260.0, y)),
                NodeDoc::Effect { voice, .. } => {
                    let k = if matches!(voice, VoiceSpec::Builtin) {
                        0
                    } else {
                        1 + effect_slot[track]
                    };
                    effect_slot[track] += 1;
                    (
                        PatchId::Node(node.node_id()),
                        Pos2::new(480.0, y + k as f32 * 64.0),
                    )
                }
                NodeDoc::ChannelStrip { .. } => {
                    (PatchId::Node(node.node_id()), Pos2::new(700.0, y))
                }
            };
            wanted.push(id);
            self.positions.entry(id).or_insert(pos);
        }

        // The single explicit Timeline node (T4), top-left of the graph.
        wanted.push(PatchId::Timeline);
        self.positions
            .entry(PatchId::Timeline)
            .or_insert(Pos2::new(16.0, 32.0));

        wanted.push(PatchId::Master);
        let y_center = (32.0 + track_count.max(1) as f32 * 132.0) * 0.5;
        self.positions
            .entry(PatchId::Master)
            .or_insert(Pos2::new(930.0, y_center));

        // Prune positions of deleted entities to prevent unbounded hashmap growth.
        self.positions.retain(|id, _| wanted.contains(id));
    }

    /// Hit-test an output port (screen distance) → `(entity, port index)`.
    fn hit_out_port(
        &self,
        rect_min: Pos2,
        sp: Pos2,
        midi_count: usize,
        auto_count: usize,
    ) -> Option<(PatchId, usize)> {
        for &id in self.positions.keys() {
            for (port, p) in self.out_ports_screen(rect_min, id, midi_count, auto_count) {
                if p.distance(sp) <= PORT_HIT_RADIUS {
                    return Some((id, port));
                }
            }
        }
        None
    }

    /// Hit-test an entity box.
    fn hit_node(
        &self,
        rect_min: Pos2,
        sp: Pos2,
        midi_count: usize,
        auto_count: usize,
    ) -> Option<PatchId> {
        for &id in self.positions.keys() {
            if let Some(r) = self.screen_rect(rect_min, id, midi_count, auto_count) {
                if r.contains(sp) {
                    return Some(id);
                }
            }
        }
        None
    }

    /// All entities whose screen rect intersects the marquee (box selection).
    fn nodes_in_rect(
        &self,
        rect_min: Pos2,
        marquee: Rect,
        midi_count: usize,
        auto_count: usize,
    ) -> Vec<PatchId> {
        self.positions
            .keys()
            .copied()
            .filter(|&id| {
                self.screen_rect(rect_min, id, midi_count, auto_count)
                    .is_some_and(|r| r.intersects(marquee))
            })
            .collect()
    }

    /// Hit-test a cable (returns the index into the document cable table).
    fn hit_cable(
        &self,
        rect_min: Pos2,
        sp: Pos2,
        doc: &GraphDoc,
        midi_count: usize,
        auto_count: usize,
    ) -> Option<usize> {
        for (i, cable) in doc.cables.iter().enumerate() {
            let (Some((from, from_port)), Some(to)) = (
                patch_port_from(&cable.from, midi_count),
                patch_port_to(&cable.to),
            ) else {
                continue;
            };
            let (Some((_, p0)), Some(p3)) = (
                self.out_ports_screen(rect_min, from, midi_count, auto_count)
                    .into_iter()
                    .find(|(k, _)| *k == from_port),
                self.in_port_screen(rect_min, to, midi_count, auto_count),
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

    /// Point-select at screen position `sp`, shared by the press-drag path and
    /// the plain-click path (egui 0.36 disambiguates clicks from drags, so an
    /// in-place press never produces `drag_started` — only `clicked`).
    ///
    /// Returns the point-selected node (if any) so the caller can do the
    /// patch-panel track linkage.
    fn point_select(
        &mut self,
        rect_min: Pos2,
        sp: Pos2,
        doc: &GraphDoc,
        midi_count: usize,
        auto_count: usize,
        shift: bool,
    ) -> Option<PatchId> {
        if let Some(id) = self.hit_node(rect_min, sp, midi_count, auto_count) {
            if shift {
                self.selection.toggle_node(id);
            } else if !self.selection.contains_node(id) {
                self.selection = Selection {
                    nodes: vec![id],
                    cables: Vec::new(),
                };
            }
            Some(id)
        } else if let Some(i) = self.hit_cable(rect_min, sp, doc, midi_count, auto_count) {
            if shift {
                self.selection.toggle_cable(i);
            } else {
                self.selection = Selection {
                    nodes: Vec::new(),
                    cables: vec![i],
                };
            }
            None
        } else {
            // Click on empty background clears the selection.
            if !shift {
                self.selection = Selection::default();
            }
            None
        }
    }
}

/// Patch-panel linkage: point-selecting a document node switches the main
/// editing track (the timeline / master have no track binding).
fn track_of_node(picked: Option<PatchId>, doc: &GraphDoc) -> Option<usize> {
    match picked {
        Some(PatchId::Node(node_id)) => doc.node_by_id(node_id).map(|node| node.track()),
        _ => None,
    }
}

/// `PortRef` → cable source entity + output port index (`midi_count` = MIDI
/// track count; timeline ports are MIDI tracks first, then control tracks).
fn patch_port_from(port: &PortRef, midi_count: usize) -> Option<(PatchId, usize)> {
    match port {
        PortRef::Node(id) => Some((PatchId::Node(*id), 0)),
        PortRef::MidiTrack(t) => Some((PatchId::Timeline, *t)),
        PortRef::AutoTrack(i) => Some((PatchId::Timeline, midi_count + i)),
        PortRef::Master => None, // The master output is not a valid audio source.
    }
}

/// `PortRef` → cable target entity.
fn patch_port_to(port: &PortRef) -> Option<PatchId> {
    match port {
        PortRef::Node(id) => Some(PatchId::Node(*id)),
        PortRef::Master => Some(PatchId::Master),
        _ => None,
    }
}

/// Cable kind → whether the target entity accepts it (port type rules, spec §7/D3).
fn accepts(kind: CableKind, target: PatchId, doc: &GraphDoc) -> bool {
    if let PatchId::Node(id) = target {
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
        matches!(target, PatchId::Master) && kind == CableKind::Audio
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

/// Draw one frame of the patch and collect interaction intents.
pub fn show(ui: &mut egui::Ui, state: &mut PatchState, deps: PatchDeps<'_>) -> Vec<PatchAction> {
    let mut actions = Vec::new();
    let doc = deps.doc;
    state.ensure_positions(doc, deps.track_count, deps.auto_count);

    // ---- Toolbar ----
    ui.horizontal(|ui| {
        ui.strong("Node Graph");
        if ui.button("-").clicked() {
            state.zoom = (state.zoom / 1.2).clamp(ZOOM_MIN, ZOOM_MAX);
        }
        if ui.button("+").clicked() {
            state.zoom = (state.zoom * 1.2).clamp(ZOOM_MIN, ZOOM_MAX);
        }
        if ui.button("Reset").clicked() {
            state.pan = Vec2::ZERO;
            state.zoom = 1.0;
        }
        ui.weak("Click to select · drag bg to box-select · middle-drag to pan · scroll to zoom · drag ports to connect · Delete to remove");
    });

    // ---- Patch ----
    // Fill the remaining central-panel height (dedicated Patch view); fall back
    // to the fixed minimum when the available space is smaller.
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(
            ui.available_width(),
            ui.available_height().max(CANVAS_HEIGHT),
        ),
        Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_rgb(22, 24, 28));

    // Wheel zoom (plain wheel and Ctrl+wheel / pinch, anchored at the pointer).
    // NOTE: `zoom_delta()` is a *multiplicative* factor whose neutral value is
    // 1.0 (NOT 0.0) — comparing against 0.0 would zoom ×2 every frame and pin
    // the view at ZOOM_MAX.
    let (zoom_factor, scroll_y) = ui
        .ctx()
        .input(|i| (i.zoom_delta(), i.smooth_scroll_delta.y));
    if response.hovered() {
        let mult = if zoom_factor != 1.0 {
            // Ctrl+wheel / pinch: egui already gives the multiplicative factor.
            zoom_factor
        } else if scroll_y != 0.0 {
            // Plain wheel: convert the scroll delta into a zoom multiplier.
            1.0 + scroll_y * 0.005
        } else {
            1.0
        };
        if mult != 1.0 {
            let old = state.zoom;
            state.zoom = (state.zoom * mult).clamp(ZOOM_MIN, ZOOM_MAX);
            if let Some(sp) = response.hover_pos() {
                let graph = state.pan + (sp - rect.min) / old;
                state.pan = graph - (sp - rect.min) / state.zoom;
            }
        }
    }

    let pointer = response.interact_pointer_pos();
    let drag_started = response.drag_started_by(egui::PointerButton::Primary);
    let drag_stopped = response.drag_stopped_by(egui::PointerButton::Primary);
    let shift = ui.ctx().input(|i| i.modifiers.shift);

    // Shorthands for the port geometry (the timeline box grows with tracks).
    let (midi_count, auto_count) = (deps.track_count, deps.auto_count);

    // ---- Press: hit-test and enter the matching drag ----
    // egui 0.36 (click+drag sense) only starts a drag after the pointer moves
    // decisively; in-place presses are handled by the `clicked` path below.
    if drag_started {
        let sp = pointer.unwrap_or(rect.min);
        if let Some((source, port)) = state.hit_out_port(rect.min, sp, midi_count, auto_count) {
            state.selection = Selection::default();
            state.drag = Some(DragKind::Cable(CableDrag { from: source, port }));
        } else if let Some(id) = state.hit_node(rect.min, sp, midi_count, auto_count) {
            let picked = state.point_select(rect.min, sp, doc, midi_count, auto_count, shift);
            if !shift && let Some(track) = track_of_node(picked, doc) {
                actions.push(PatchAction::SelectTrack(track));
            }
            state.drag = Some(DragKind::Node(id));
        } else {
            // Background drag: marquee box selection (Shift appends instead of
            // replacing). A plain background click never reaches here — it is
            // a `clicked` and clears the selection there.
            if !shift {
                state.selection = Selection::default();
            }
            state.drag = Some(DragKind::BoxSelect {
                start: sp,
                add: shift,
            });
        }
    }

    // ---- Plain click (press + release without decisive drag): point-select ----
    if response.clicked()
        && let Some(sp) = pointer
    {
        let picked = state.point_select(rect.min, sp, doc, midi_count, auto_count, shift);
        if !shift && let Some(track) = track_of_node(picked, doc) {
            actions.push(PatchAction::SelectTrack(track));
        }
    }

    // ---- Drag in progress ----
    // Middle button: pan the viewport.
    if response.dragged_by(egui::PointerButton::Middle) {
        let d = ui.ctx().input(|i| i.pointer.delta());
        state.pan -= d / state.zoom;
    }
    if response.dragged_by(egui::PointerButton::Primary)
        && let Some(DragKind::Node(id)) = state.drag
    {
        let d = response.drag_delta() / state.zoom;
        if state.selection.contains_node(id) && state.selection.nodes.len() > 1 {
            // Moving a selected node moves the whole selection.
            for nid in state.selection.nodes.clone() {
                if let Some(p) = state.positions.get_mut(&nid) {
                    *p += d;
                }
            }
        } else if let Some(p) = state.positions.get_mut(&id) {
            *p += d;
        }
    }

    // ---- Drag released: commit cable / marquee ----
    if drag_stopped {
        match state.drag.take() {
            Some(DragKind::Cable(drag)) => {
                if let Some(sp) = pointer
                    && let Some(target) = state.hit_node(rect.min, sp, midi_count, auto_count)
                {
                    match drag.from {
                        // Timeline port: MIDI tracks first, then control tracks
                        // (mapping lives in `timeline_port_ref`).
                        PatchId::Timeline => {
                            match timeline_port_ref(drag.port, midi_count) {
                                PortRef::MidiTrack(track) => {
                                    if accepts(CableKind::Midi, target, doc)
                                        && let PatchId::Node(to_doc) = target
                                    {
                                        actions.push(PatchAction::ConnectMidi { track, to_doc });
                                    }
                                }
                                PortRef::AutoTrack(index) => {
                                    if accepts(CableKind::Automation, target, doc)
                                        && let PatchId::Node(to_doc) = target
                                    {
                                        // The cable is committed after the parameter is
                                        // picked in the popup menu.
                                        state.pending_auto =
                                            Some((index, to_doc, sp + Vec2::new(4.0, 4.0)));
                                    }
                                }
                                _ => {}
                            }
                        }
                        PatchId::Node(from_doc) => {
                            if accepts(CableKind::Audio, target, doc) && drag.from != target {
                                let to = match target {
                                    PatchId::Node(id) => PortRef::Node(id),
                                    PatchId::Master => PortRef::Master,
                                    _ => PortRef::Master,
                                };
                                actions.push(PatchAction::ConnectAudio { from_doc, to });
                            }
                        }
                        PatchId::Master => {}
                    }
                }
            }
            Some(DragKind::BoxSelect { start, add }) => {
                if let Some(sp) = pointer {
                    let marquee = Rect::from_two_pos(start, sp);
                    let hits = state.nodes_in_rect(rect.min, marquee, midi_count, auto_count);
                    if add {
                        for id in hits {
                            if !state.selection.contains_node(id) {
                                state.selection.nodes.push(id);
                            }
                        }
                    } else {
                        state.selection = Selection {
                            nodes: hits,
                            cables: Vec::new(),
                        };
                    }
                }
            }
            _ => {}
        }
    }

    // ---- Draw cables (beneath nodes) ----
    for (i, cable) in doc.cables.iter().enumerate() {
        let (Some((from, from_port)), Some(to)) = (
            patch_port_from(&cable.from, midi_count),
            patch_port_to(&cable.to),
        ) else {
            continue;
        };
        let (Some((_, p0)), Some(p3)) = (
            state
                .out_ports_screen(rect.min, from, midi_count, auto_count)
                .into_iter()
                .find(|(k, _)| *k == from_port),
            state.in_port_screen(rect.min, to, midi_count, auto_count),
        ) else {
            continue;
        };
        let selected = state.selection.cables.contains(&i);
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
        let kind = match drag.from {
            PatchId::Timeline => {
                if drag.port < midi_count {
                    CableKind::Midi
                } else {
                    CableKind::Automation
                }
            }
            PatchId::Node(_) => CableKind::Audio,
            PatchId::Master => CableKind::Audio,
        };
        if let Some(sp) = pointer {
            let p0 = state
                .out_ports_screen(rect.min, drag.from, midi_count, auto_count)
                .into_iter()
                .find(|(k, _)| *k == drag.port)
                .map(|(_, p)| p);
            if let Some(p0) = p0 {
                painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                    bezier_controls(p0, sp),
                    false,
                    Color32::TRANSPARENT,
                    egui::Stroke::new(2.0, cable_color(kind).gamma_multiply(0.7)),
                ));
                // Highlight the hovered target.
                if let Some(target) = state.hit_node(rect.min, sp, midi_count, auto_count) {
                    if accepts(kind, target, doc) && drag.from != target {
                        if let Some(r) = state.screen_rect(rect.min, target, midi_count, auto_count)
                        {
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
    }

    // ---- Draw nodes ----
    for &id in state.positions.keys() {
        let Some(r) = state.screen_rect(rect.min, id, midi_count, auto_count) else {
            continue;
        };
        if !r.intersects(rect) {
            continue;
        }
        let (stroke, fill) = node_color(id, doc);
        let selected = state.selection.contains_node(id);
        painter.rect_filled(r, 4.0, fill);
        painter.rect_stroke(
            r,
            4.0,
            egui::Stroke::new(if selected { 2.5 } else { 1.2 }, stroke),
            egui::StrokeKind::Middle,
        );

        let text = label_of(id, doc);
        let text_pos = match id {
            PatchId::Timeline => Pos2::new(r.center().x, r.top() + 12.0),
            _ => Pos2::new(r.center().x, r.top() + 18.0),
        };
        painter.text(
            text_pos,
            egui::Align2::CENTER_CENTER,
            text,
            FontId::proportional(13.0),
            Color32::from_rgb(225, 225, 228),
        );

        // Output ports (the timeline draws one per track with a small label).
        for (port, p) in state.out_ports_screen(rect.min, id, midi_count, auto_count) {
            let port_color = match id {
                PatchId::Timeline if port < midi_count => cable_color(CableKind::Midi),
                PatchId::Timeline => cable_color(CableKind::Automation),
                _ => cable_color(CableKind::Audio),
            };
            painter.circle_filled(p, PORT_RADIUS, port_color);
            painter.circle_stroke(p, PORT_RADIUS, egui::Stroke::new(1.0, Color32::BLACK));
            if let PatchId::Timeline = id {
                let label = if port < midi_count {
                    format!("T{}", port + 1)
                } else {
                    format!("A{}", port - midi_count + 1)
                };
                painter.text(
                    Pos2::new(p.x - 10.0, p.y),
                    egui::Align2::RIGHT_CENTER,
                    label,
                    FontId::proportional(10.0),
                    Color32::from_gray(170),
                );
            }
        }
        if let Some(p) = state.in_port_screen(rect.min, id, midi_count, auto_count) {
            painter.circle_filled(p, PORT_RADIUS, Color32::from_gray(90));
            painter.circle_stroke(p, PORT_RADIUS, egui::Stroke::new(1.0, Color32::BLACK));
        }
    }

    // ---- Marquee (box selection in progress, drawn on top) ----
    if let Some(DragKind::BoxSelect { start, .. }) = state.drag
        && let Some(sp) = pointer
    {
        let m = Rect::from_two_pos(start, sp);
        painter.rect_filled(m, 2.0, Color32::from_rgba_unmultiplied(120, 170, 255, 26));
        painter.rect_stroke(
            m,
            2.0,
            egui::Stroke::new(1.0, Color32::from_rgb(120, 170, 255)),
            egui::StrokeKind::Middle,
        );
    }

    // ---- Delete removes the whole selection (while hovering the patch) ----
    if response.hovered() && ui.ctx().input(|i| i.key_pressed(egui::Key::Delete)) {
        // Cables first, in descending index order (each removal shifts later
        // indices); nodes after (node deletion may cascade attached cables in
        // the caller).
        let mut cables = state.selection.cables.clone();
        cables.sort_unstable_by(|a, b| b.cmp(a));
        cables.dedup();
        for i in cables {
            actions.push(PatchAction::DeleteCable(i));
        }
        for id in state.selection.nodes.clone() {
            if let PatchId::Node(node_id) = id {
                actions.push(PatchAction::DeleteNode(node_id));
            }
        }
        if !state.selection.nodes.is_empty() || !state.selection.cables.is_empty() {
            state.selection = Selection::default();
        }
    }

    // ---- Automation parameter popup menu ----
    if let Some((index, to_doc, menu_pos)) = state.pending_auto {
        let options = (deps.auto_options)(to_doc);
        let mut picked: Option<AutoParam> = None;
        egui::Area::new(egui::Id::new("resoz_patch_auto_menu"))
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
            actions.push(PatchAction::ConnectAuto {
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

    /// Regression: `#[derive(Default)]` gave `zoom: 0.0`, collapsing the whole
    /// graph onto the pan point (top-left corner) until Reset was clicked.
    #[test]
    fn default_zoom_is_one_not_zero() {
        let state = PatchState::default();
        assert_eq!(state.zoom, 1.0);
    }

    #[test]
    fn ensure_positions_covers_all_entities() {
        let project = Project::default();
        let doc = &project.graph;
        let track_count = project.midi_tracks.len();
        let auto_count = project.control_tracks.len();
        let mut state = PatchState::default();
        state.ensure_positions(doc, track_count, auto_count);

        for node in &doc.nodes {
            assert!(
                state.positions.contains_key(&PatchId::Node(node.node_id())),
                "document node {} should have a position",
                node.node_id()
            );
        }
        assert!(state.positions.contains_key(&PatchId::Master));
        // T4: one explicit Timeline entity instead of per-track source boxes.
        assert!(state.positions.contains_key(&PatchId::Timeline));
        assert_eq!(
            state
                .positions
                .keys()
                .filter(|id| **id == PatchId::Timeline)
                .count(),
            1
        );
    }

    #[test]
    fn timeline_ports_map_to_track_refs() {
        // 2 MIDI + 3 control tracks → 5 ports: 0..2 = MIDI, 2..5 = control.
        assert_eq!(
            timeline_port_ref(0, 2),
            PortRef::MidiTrack(0),
            "port 0 = MIDI track 0"
        );
        assert_eq!(
            timeline_port_ref(1, 2),
            PortRef::MidiTrack(1),
            "port 1 = MIDI track 1"
        );
        assert_eq!(
            timeline_port_ref(2, 2),
            PortRef::AutoTrack(0),
            "port 2 = control track 0"
        );
        assert_eq!(
            timeline_port_ref(4, 2),
            PortRef::AutoTrack(2),
            "port 4 = control track 2"
        );
        assert_eq!(timeline_port_count(2, 3), 5);
    }

    #[test]
    fn port_type_rules_reject_mismatches() {
        let project = Project::default();
        let doc = &project.graph;
        let instrument = doc.instrument_of_track(0).unwrap().node_id();
        let strip = doc.channel_strip_of_track(0).unwrap().node_id();
        let delay = doc.builtin_delay_of_track(0).unwrap().node_id();

        // Audio: effects/channel strips/master output accept; instruments reject.
        assert!(accepts(CableKind::Audio, PatchId::Node(delay), doc));
        assert!(accepts(CableKind::Audio, PatchId::Node(strip), doc));
        assert!(accepts(CableKind::Audio, PatchId::Master, doc));
        assert!(!accepts(CableKind::Audio, PatchId::Node(instrument), doc));
        // MIDI: instruments accept; everything else rejects.
        assert!(accepts(CableKind::Midi, PatchId::Node(instrument), doc));
        assert!(!accepts(CableKind::Midi, PatchId::Node(strip), doc));
        assert!(!accepts(CableKind::Midi, PatchId::Master, doc));
        // Automation: instruments/channel strips accept.
        assert!(accepts(CableKind::Automation, PatchId::Node(strip), doc));
        assert!(accepts(
            CableKind::Automation,
            PatchId::Node(instrument),
            doc
        ));
        assert!(!accepts(CableKind::Automation, PatchId::Master, doc));
    }
}
