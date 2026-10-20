//! Node patch (P5d MVP, spec §9 P5d): egui self-drawn node graph view and cable editing.
//!
//! Responsibility boundary: the patch only does **view + interaction intent** — pan (middle
//! drag)/zoom (wheel), node box drawing (color-coded by type), cable drawing, drag-to-connect
//! interaction, point/marquee selection and deletion, right-click node adding, Ctrl+right-drag
//! cable cutting (Blender-style knife) and cable-midpoint insert/delete menus. All side
//! effects are returned via [`PatchAction`] and applied to the document + engine in one place
//! by `GlitcherZApp::apply_patch_actions` (the patch never touches the engine/guards directly).
//!
//! Coordinate convention: node positions are stored in **graph space** and persisted with
//! the project (`GraphDoc::layout`, exported on save / imported on load; P5c dual identity
//! — the patch deals with document node_ids + fixed entities and is unaware of engine ids).

use std::collections::HashMap;

use eframe::egui;
use egui::{Color32, FontId, Pos2, Rect, Sense, Vec2};

use glitcherz_model::{
    AutoParam, CableDoc, CableKind, GraphDoc, LayoutDoc, NodeDoc, PortRef, VoiceSpec,
};

/// Patch entity identity: document nodes + fixed entities (master output /
/// the arrangement timeline).
///
/// T4 (spec `.agents/spec/timeline.md`): the per-track source boxes merged into
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

/// Kind of node the user wants to add via the patch context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddNodeKind {
    /// Instrument node (built-in synth voice, MIDI entry).
    Instrument,
    /// Effect node (built-in delay).
    Effect,
    /// Mixer node (P5f D15/D21): the explicit multi-channel summing point;
    /// absorbs the old ChannelStrip.
    Mixer,
}

/// Interaction intents produced within one patch frame (applied by the caller; the patch has
/// no side effects).
#[derive(Debug, Clone)]
pub enum PatchAction {
    /// Add a new node of `kind` at graph-space position `at` (right-click menu). The new
    /// node is born dangling (no cables; D6 dangling semantics) — the user wires it up.
    AddNode { kind: AddNodeKind, at: Pos2 },
    /// Audio cable drag: source document node → target (node or master output).
    /// `in_port` binds the cable to the target's input port — the mixer channel
    /// (D17; 0 for single-input nodes / master).
    ConnectAudio {
        from_doc: u64,
        to: PortRef,
        in_port: u8,
    },
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
    /// Delete a node (document node_id; any instrument / effect / channel strip is
    /// deletable — master / timeline are fixed and never selected as document nodes).
    DeleteNode(u64),
    /// Insert a new node of `kind` into the audio cable at document index `cable`,
    /// replacing `A → B` with `A → new → B` (cable-midpoint menu). `at` = graph-space
    /// position where the user clicked (the new node is placed there).
    InsertNode {
        cable: usize,
        kind: AddNodeKind,
        at: Pos2,
    },
    /// Set a node's owning track as the current editing track.
    SelectTrack(usize),
    /// Double-clicked a document node: jump to the Synth panel with that
    /// node as the editing target (spec `plugin-gui.md` §6 — the patch
    /// analogue of double-clicking a timeline clip to open the Edit view).
    OpenNode(u64),
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
    /// Graph-space position where the add-node context menu was opened (consumed when a
    /// node kind is picked).
    add_menu_at: Option<Pos2>,
    /// Ctrl + right-drag cable cut in progress: screen-space pointer path.
    cut_path: Option<Vec<Pos2>>,
    /// Cable-midpoint menu awaiting a pick: `(cable index, popup screen position,
    /// graph-space insert position, frame it was opened)`. `u64::MAX` marks "opened this
    /// frame" so the same-frame click does not immediately close it.
    pending_cable_menu: Option<(usize, Pos2, Pos2, u64)>,
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
            add_menu_at: None,
            cut_path: None,
            pending_cable_menu: None,
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
/// Mixer box minimum height (scales with channel/input port count).
const NODE_HEIGHT_MIN: f32 = 64.0;
/// Vertical spacing between mixer input ports (P5f: one port per channel).
const PORT_SPACING: f32 = 30.0;
/// Timeline box size per output port row (width fixed, height scales).
const TIMELINE_WIDTH: f32 = 150.0;
const TIMELINE_ROW_HEIGHT: f32 = 26.0;
const TIMELINE_MIN_HEIGHT: f32 = 40.0;
/// Fixed patch height.
const CANVAS_HEIGHT: f32 = 300.0;
/// Output port circle radius / hit radius (generous so port drags are easy to start).
const PORT_RADIUS: f32 = 6.0;
const PORT_HIT_RADIUS: f32 = 18.0;
/// Cable hit distance.
const CABLE_HIT_DIST: f32 = 8.0;
/// Cable-cut hit distance (pointer path within this distance of a cable severs it).
const CUT_HIT_DIST: f32 = 10.0;
/// Cable midpoint control dot radius / hit radius.
const MIDPOINT_RADIUS: f32 = 4.0;
const MIDPOINT_HIT_RADIUS: f32 = 10.0;
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
            Some(NodeDoc::Mixer { .. }) => Color32::from_rgb(110, 190, 120),
            Some(NodeDoc::ChannelStrip { .. }) => Color32::from_gray(140),
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

/// Entity box size. Mixer boxes scale with the channel (input port) count so
/// every channel gets its own input port dot on the left edge.
fn size_of(id: PatchId, midi_count: usize, auto_count: usize, doc: &GraphDoc) -> Vec2 {
    match id {
        PatchId::Timeline => timeline_size(midi_count, auto_count),
        PatchId::Node(node_id) => match doc.node_by_id(node_id) {
            Some(NodeDoc::Mixer { inputs, .. }) => Vec2::new(
                NODE_SIZE.x,
                (NODE_HEIGHT_MIN).max(*inputs as f32 * PORT_SPACING + 20.0),
            ),
            _ => NODE_SIZE,
        },
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
                VoiceSpec::SoundFont { bank, program, .. } => {
                    format!("SF2 {bank}:{program} · Track {}", track + 1)
                }
                VoiceSpec::Clap { plugin_id, .. } => {
                    format!("♪ {}", truncate(plugin_id, 16))
                }
                VoiceSpec::Vst3 { class_id, .. } => format!("♪ {}", truncate(class_id, 16)),
            },
            Some(NodeDoc::Effect { voice, track, .. }) => match voice {
                VoiceSpec::Builtin | VoiceSpec::SoundFont { .. } => {
                    format!("Delay · Track {}", track + 1)
                }
                VoiceSpec::Clap { plugin_id, .. } => {
                    format!("Fx {}", truncate(plugin_id, 14))
                }
                VoiceSpec::Vst3 { class_id, .. } => format!("Fx {}", truncate(class_id, 14)),
            },
            Some(NodeDoc::Mixer { inputs, .. }) => format!("Mixer · {} ch", inputs),
            Some(NodeDoc::ChannelStrip { track, .. }) => {
                format!("Strip (legacy) · Track {}", track + 1)
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
    /// Document node ids currently selected (fixed entities — timeline /
    /// master — excluded). Read by the app to resolve the Synth panel's
    /// editing target (spec `plugin-gui.md` §6).
    pub fn selected_doc_nodes(&self) -> Vec<u64> {
        self.selection
            .nodes
            .iter()
            .filter_map(|id| match id {
                PatchId::Node(doc) => Some(*doc),
                PatchId::Timeline | PatchId::Master => None,
            })
            .collect()
    }

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
        doc: &GraphDoc,
    ) -> Option<Rect> {
        let p = *self.positions.get(&id)?;
        Some(Rect::from_min_size(
            self.to_screen(rect_min, p),
            size_of(id, midi_count, auto_count, doc) * self.zoom,
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
        doc: &GraphDoc,
    ) -> Vec<(usize, Pos2)> {
        let Some(r) = self.screen_rect(rect_min, id, midi_count, auto_count, doc) else {
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

    /// Input ports of one entity: `(channel index, screen position)` pairs
    /// (P5f D17: a mixer exposes one port per channel spread along its left
    /// edge; other nodes/master have the single port 0; the timeline has none
    /// — it is a pure source, D14/T4).
    fn in_ports_screen(
        &self,
        rect_min: Pos2,
        id: PatchId,
        midi_count: usize,
        auto_count: usize,
        doc: &GraphDoc,
    ) -> Vec<(u8, Pos2)> {
        let Some(r) = self.screen_rect(rect_min, id, midi_count, auto_count, doc) else {
            return Vec::new();
        };
        let n = match id {
            PatchId::Timeline => return Vec::new(),
            PatchId::Master => 1,
            PatchId::Node(node_id) => match doc.node_by_id(node_id) {
                Some(node) if node.is_mixer() => {
                    if let NodeDoc::Mixer { inputs, .. } = node {
                        *inputs as usize
                    } else {
                        1
                    }
                }
                _ => 1,
            },
        };
        (0..n)
            .map(|i| {
                let y = r.top() + (i as f32 + 0.5) * (r.height() / n.max(1) as f32);
                (i as u8, Pos2::new(r.left(), y))
            })
            .collect()
    }

    /// Fill in default placements for missing entities (never overwrites existing positions;
    /// prunes stale entries).
    fn ensure_positions(&mut self, doc: &GraphDoc, track_count: usize, _auto_count: usize) {
        let mut wanted: Vec<PatchId> = Vec::new();

        // One column per track: instrument → effect stack → mixer.
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
                NodeDoc::Mixer { .. } => (PatchId::Node(node.node_id()), Pos2::new(700.0, y)),
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

    /// Set the graph-space position of a freshly added node (called by the glue after
    /// committing an [`PatchAction::AddNode`]; `ensure_positions` never overwrites it).
    pub fn place_node(&mut self, node_id: u64, at: Pos2) {
        self.positions.insert(PatchId::Node(node_id), at);
    }

    /// Snapshot the current positions into a document layout (called by the glue right
    /// before saving). Only entities that still exist are exported; stale node entries
    /// (already deleted from the document) are dropped.
    pub fn export_layout(&self, doc: &GraphDoc) -> LayoutDoc {
        let mut layout = LayoutDoc::default();
        for (id, p) in &self.positions {
            match *id {
                PatchId::Node(node_id) => {
                    if doc.node_by_id(node_id).is_some() {
                        layout.nodes.insert(node_id, [p.x, p.y]);
                    }
                }
                PatchId::Timeline => layout.timeline = Some([p.x, p.y]),
                PatchId::Master => layout.master = Some([p.x, p.y]),
            }
        }
        layout
    }

    /// Seed positions from a document layout (called after load / new project;
    /// clears the session view state — missing entries fall back to auto-placement
    /// on the next `ensure_positions`).
    pub fn import_layout(&mut self, layout: &LayoutDoc) {
        self.positions.clear();
        for (node_id, xy) in &layout.nodes {
            self.positions
                .insert(PatchId::Node(*node_id), Pos2::new(xy[0], xy[1]));
        }
        if let Some([x, y]) = layout.timeline {
            self.positions.insert(PatchId::Timeline, Pos2::new(x, y));
        }
        if let Some([x, y]) = layout.master {
            self.positions.insert(PatchId::Master, Pos2::new(x, y));
        }
    }

    /// Hit-test an output port (screen distance) → `(entity, port index)`.
    fn hit_out_port(
        &self,
        rect_min: Pos2,
        sp: Pos2,
        midi_count: usize,
        auto_count: usize,
        doc: &GraphDoc,
    ) -> Option<(PatchId, usize)> {
        for &id in self.positions.keys() {
            for (port, p) in self.out_ports_screen(rect_min, id, midi_count, auto_count, doc) {
                if p.distance(sp) <= PORT_HIT_RADIUS {
                    return Some((id, port));
                }
            }
        }
        None
    }

    /// Hit-test an input port (screen distance) → `(entity, channel index)`.
    /// Used for mixer-channel-targeted cable drops (D17).
    fn hit_in_port(
        &self,
        rect_min: Pos2,
        sp: Pos2,
        midi_count: usize,
        auto_count: usize,
        doc: &GraphDoc,
    ) -> Option<(PatchId, u8)> {
        for &id in self.positions.keys() {
            for (port, p) in self.in_ports_screen(rect_min, id, midi_count, auto_count, doc) {
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
        doc: &GraphDoc,
    ) -> Option<PatchId> {
        for &id in self.positions.keys() {
            if let Some(r) = self.screen_rect(rect_min, id, midi_count, auto_count, doc) {
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
        doc: &GraphDoc,
    ) -> Vec<PatchId> {
        self.positions
            .keys()
            .copied()
            .filter(|&id| {
                self.screen_rect(rect_min, id, midi_count, auto_count, doc)
                    .is_some_and(|r| r.intersects(marquee))
            })
            .collect()
    }

    /// Endpoint screen positions of a document cable → bezier control points
    /// (`None` when either endpoint has no position / the ports don't resolve).
    /// The endpoint lands on the input port matching `cable.in_port` (D17:
    /// the mixer channel the cable is bound to).
    fn cable_controls(
        &self,
        rect_min: Pos2,
        cable: &CableDoc,
        midi_count: usize,
        auto_count: usize,
        doc: &GraphDoc,
    ) -> Option<[Pos2; 4]> {
        let (from, from_port) = patch_port_from(&cable.from, midi_count)?;
        let to = patch_port_to(&cable.to)?;
        let p0 = self
            .out_ports_screen(rect_min, from, midi_count, auto_count, doc)
            .into_iter()
            .find(|(k, _)| *k == from_port)?
            .1;
        let p3 = self
            .in_ports_screen(rect_min, to, midi_count, auto_count, doc)
            .into_iter()
            .find(|(k, _)| *k == cable.in_port)?
            .1;
        Some(bezier_controls(p0, p3))
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
            let Some(c) = self.cable_controls(rect_min, cable, midi_count, auto_count, doc) else {
                continue;
            };
            for pt in bezier_points(c, 24) {
                if pt.distance(sp) <= CABLE_HIT_DIST {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Hit-test a cable midpoint control dot (returns the cable index).
    fn hit_cable_midpoint(
        &self,
        rect_min: Pos2,
        sp: Pos2,
        doc: &GraphDoc,
        midi_count: usize,
        auto_count: usize,
    ) -> Option<usize> {
        for (i, cable) in doc.cables.iter().enumerate() {
            let Some(c) = self.cable_controls(rect_min, cable, midi_count, auto_count, doc) else {
                continue;
            };
            if bezier_points(c, 24)[12].distance(sp) <= MIDPOINT_HIT_RADIUS {
                return Some(i);
            }
        }
        None
    }

    /// All cables whose sampled curve passes within [`CUT_HIT_DIST`] of the
    /// screen-space pointer path (Blender-style knife hit test).
    fn cables_crossing_path(
        &self,
        rect_min: Pos2,
        path: &[Pos2],
        doc: &GraphDoc,
        midi_count: usize,
        auto_count: usize,
    ) -> Vec<usize> {
        let mut hits = Vec::new();
        if path.len() < 2 {
            return hits;
        }
        for (i, cable) in doc.cables.iter().enumerate() {
            let Some(c) = self.cable_controls(rect_min, cable, midi_count, auto_count, doc) else {
                continue;
            };
            'cable: for pt in bezier_points(c, 32) {
                for seg in path.windows(2) {
                    if dist_point_seg(pt, seg[0], seg[1]) <= CUT_HIT_DIST {
                        hits.push(i);
                        break 'cable;
                    }
                }
            }
        }
        hits
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
        if let Some(id) = self.hit_node(rect_min, sp, midi_count, auto_count, doc) {
            if shift {
                self.selection.toggle_node(id);
            } else if !self.selection.contains_node(id) {
                self.selection = Selection {
                    nodes: vec![id],
                    cables: Vec::new(),
                };
            }
            Some(id)
        } else if let Some(i) = self.hit_cable_midpoint(rect_min, sp, doc, midi_count, auto_count) {
            // Click on a cable's midpoint control dot: open the insert/delete menu.
            let graph_at = Pos2::new(
                self.pan.x + (sp.x - rect_min.x) / self.zoom,
                self.pan.y + (sp.y - rect_min.y) / self.zoom,
            );
            self.pending_cable_menu = Some((i, sp + Vec2::new(4.0, 4.0), graph_at, u64::MAX));
            None
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
                matches!(node, NodeDoc::Effect { .. } | NodeDoc::Mixer { .. })
            }
            CableKind::Midi => matches!(node, NodeDoc::Instrument { .. }),
            // Mixer-fader automation is deferred (P5f decision): instruments only.
            CableKind::Automation => matches!(node, NodeDoc::Instrument { .. }),
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

/// Shortest distance from point `p` to the segment `a → b`.
fn dist_point_seg(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.dot(ab);
    if len2 <= f32::EPSILON {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.distance(a + ab * t)
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
        ui.weak("Right-click to add node · Ctrl+right-drag to cut cables · click a cable dot to insert/delete · click to select · drag bg to box-select · middle-drag to pan · scroll to zoom · drag ports to connect · Delete to remove");
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
    let ctrl = ui.ctx().input(|i| i.modifiers.ctrl);

    // Shorthands for the port geometry (the timeline box grows with tracks).
    let (midi_count, auto_count) = (deps.track_count, deps.auto_count);

    // ---- Press: hit-test and enter the matching drag ----
    // egui 0.36 (click+drag sense) only starts a drag after the pointer moves
    // decisively; in-place presses are handled by the `clicked` path below.
    if drag_started {
        let sp = pointer.unwrap_or(rect.min);
        if let Some((source, port)) = state.hit_out_port(rect.min, sp, midi_count, auto_count, doc)
        {
            state.selection = Selection::default();
            state.drag = Some(DragKind::Cable(CableDrag { from: source, port }));
        } else if let Some(id) = state.hit_node(rect.min, sp, midi_count, auto_count, doc) {
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

    // ---- Double-click a document node: jump to the Synth panel editing it ----
    // (Fixed entities — timeline / master — have no panel of their own.)
    if response.double_clicked()
        && let Some(sp) = pointer
        && let Some(PatchId::Node(doc_id)) =
            state.hit_node(rect.min, sp, midi_count, auto_count, doc)
    {
        actions.push(PatchAction::OpenNode(doc_id));
    }

    // ---- Right-click: add-node context menu (any position; the new node appears where
    // the user clicked). With Ctrl held the right button belongs to the cable-cut
    // interaction instead (Blender-style knife) and the menu is suppressed. ----
    if response.secondary_clicked()
        && !ctrl
        && let Some(sp) = pointer
    {
        state.add_menu_at = Some(Pos2::new(
            state.pan.x + (sp.x - rect.min.x) / state.zoom,
            state.pan.y + (sp.y - rect.min.y) / state.zoom,
        ));
    }
    // egui manages the open/close lifecycle (Escape / click elsewhere close it); the
    // closure runs only while the menu is open. egui 0.36's `context_menu` closure
    // returns `()`, so the pick is collected through a captured outer variable.
    let mut picked_kind: Option<AddNodeKind> = None;
    if !(response.secondary_clicked() && ctrl) {
        response.context_menu(|ui| {
            ui.set_min_width(120.0);
            ui.strong("Add Node");
            if ui.button("Instrument").clicked() {
                picked_kind = Some(AddNodeKind::Instrument);
            }
            if ui.button("Effect (Delay)").clicked() {
                picked_kind = Some(AddNodeKind::Effect);
            }
            if ui.button("Mixer").clicked() {
                picked_kind = Some(AddNodeKind::Mixer);
            }
        });
    }
    if let Some(kind) = picked_kind
        && let Some(at) = state.add_menu_at.take()
    {
        actions.push(PatchAction::AddNode { kind, at });
    }

    // ---- Ctrl + right-drag: cut cables the pointer path crosses ----
    if response.drag_started_by(egui::PointerButton::Secondary) && ctrl {
        state.cut_path = Some(pointer.map(|p| vec![p]).unwrap_or_default());
    }
    if state.cut_path.is_some()
        && response.dragged_by(egui::PointerButton::Secondary)
        && let Some(sp) = pointer
    {
        let path = state.cut_path.as_mut().expect("checked above");
        if path.last() != Some(&sp) {
            path.push(sp);
        }
        if ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
            state.cut_path = None;
        }
    }
    if response.drag_stopped_by(egui::PointerButton::Secondary) {
        if let Some(path) = state.cut_path.take()
            && path.len() >= 2
        {
            let mut cuts = state.cables_crossing_path(rect.min, &path, doc, midi_count, auto_count);
            cuts.sort_unstable();
            cuts.dedup();
            // Descending index order: each removal shifts later document indices.
            for i in cuts.into_iter().rev() {
                actions.push(PatchAction::DeleteCable(i));
            }
        }
        // Cable indices may have shifted; drop any stale cable selection.
        state.selection.cables.clear();
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
                    && let Some(target) = state.hit_node(rect.min, sp, midi_count, auto_count, doc)
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
                                // D17: the cable binds to the target's input
                                // port. Dropping on a mixer picks the exact
                                // channel port when hit precisely, else the
                                // first free channel; dropping anywhere else
                                // is port 0. Slots are strictly single-input
                                // (D14): the new cable replaces the old one
                                // on the same slot; no free channel → reject.
                                let in_port = match target {
                                    PatchId::Node(to_doc)
                                        if doc.node_by_id(to_doc).is_some_and(|n| n.is_mixer()) =>
                                    {
                                        match state
                                            .hit_in_port(rect.min, sp, midi_count, auto_count, doc)
                                        {
                                            Some((PatchId::Node(_), port)) => Some(port),
                                            _ => doc.first_free_channel(to_doc),
                                        }
                                    }
                                    _ => Some(0),
                                };
                                if let Some(in_port) = in_port {
                                    actions.push(PatchAction::ConnectAudio {
                                        from_doc,
                                        to,
                                        in_port,
                                    });
                                }
                            }
                        }
                        PatchId::Master => {}
                    }
                }
            }
            Some(DragKind::BoxSelect { start, add }) => {
                if let Some(sp) = pointer {
                    let marquee = Rect::from_two_pos(start, sp);
                    let hits = state.nodes_in_rect(rect.min, marquee, midi_count, auto_count, doc);
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
        let Some(bez) = state.cable_controls(rect.min, cable, midi_count, auto_count, doc) else {
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
            bez,
            false,
            Color32::TRANSPARENT,
            stroke,
        ));
        let mid = bezier_points(bez, 24)[12];
        // Label automation cables with the endpoint parameter above the midpoint.
        if cable.kind == CableKind::Automation {
            painter.text(
                mid + Vec2::new(0.0, -12.0),
                egui::Align2::CENTER_CENTER,
                auto_param_label(&cable.param),
                FontId::proportional(11.0),
                cable_color(CableKind::Automation),
            );
        }
        // Midpoint control dot (click → insert node / delete cable menu).
        let hovered = pointer.is_some_and(|sp| sp.distance(mid) <= MIDPOINT_HIT_RADIUS);
        let r = if hovered {
            MIDPOINT_RADIUS + 2.0
        } else {
            MIDPOINT_RADIUS
        };
        painter.circle_filled(mid, r, cable_color(cable.kind));
        painter.circle_stroke(mid, r, egui::Stroke::new(1.0, Color32::BLACK));
    }

    // ---- Cable-cut visuals: pointer path + cables about to be severed ----
    if let Some(path) = &state.cut_path
        && path.len() >= 2
    {
        for i in state.cables_crossing_path(rect.min, path, doc, midi_count, auto_count) {
            if let Some(bez) =
                state.cable_controls(rect.min, &doc.cables[i], midi_count, auto_count, doc)
            {
                painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                    bez,
                    false,
                    Color32::TRANSPARENT,
                    egui::Stroke::new(4.0, Color32::from_rgb(230, 70, 70)),
                ));
            }
        }
        painter.add(egui::Shape::line(
            path.clone(),
            egui::Stroke::new(1.5, Color32::from_rgb(230, 70, 70)),
        ));
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
                .out_ports_screen(rect.min, drag.from, midi_count, auto_count, doc)
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
                if let Some(target) = state.hit_node(rect.min, sp, midi_count, auto_count, doc) {
                    if accepts(kind, target, doc) && drag.from != target {
                        if let Some(r) =
                            state.screen_rect(rect.min, target, midi_count, auto_count, doc)
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
        let Some(r) = state.screen_rect(rect.min, id, midi_count, auto_count, doc) else {
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
        for (port, p) in state.out_ports_screen(rect.min, id, midi_count, auto_count, doc) {
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
        // Input ports: mixers draw one per channel (P5f D17), other nodes /
        // master the single port 0.
        for (_, p) in state.in_ports_screen(rect.min, id, midi_count, auto_count, doc) {
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
        egui::Area::new(egui::Id::new("glitcherz_patch_auto_menu"))
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

    // ---- Cable midpoint menu (click the control dot): insert node / delete ----
    // Stamp the opening frame first so the click that opened the menu (any_click in
    // the same frame) does not close it immediately.
    if let Some((_, _, _, frame)) = &mut state.pending_cable_menu
        && *frame == u64::MAX
    {
        *frame = ui.ctx().cumulative_frame_nr();
    }
    if let Some((cable_i, menu_pos, at, opened)) = state.pending_cable_menu {
        #[derive(Clone, Copy)]
        enum CablePick {
            Insert(AddNodeKind),
            Delete,
        }
        let mut picked: Option<CablePick> = None;
        egui::Area::new(egui::Id::new("glitcherz_patch_cable_menu"))
            .fixed_pos(menu_pos)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(140.0);
                    ui.strong("Cable");
                    let is_audio = doc
                        .cables
                        .get(cable_i)
                        .is_some_and(|c| c.kind == CableKind::Audio);
                    if is_audio && ui.button("Insert Effect (Delay)").clicked() {
                        picked = Some(CablePick::Insert(AddNodeKind::Effect));
                    }
                    if is_audio && ui.button("Insert Mixer").clicked() {
                        picked = Some(CablePick::Insert(AddNodeKind::Mixer));
                    }
                    if ui.button("Delete Cable").clicked() {
                        picked = Some(CablePick::Delete);
                    }
                });
            });
        match picked {
            Some(CablePick::Insert(kind)) => {
                actions.push(PatchAction::InsertNode {
                    cable: cable_i,
                    kind,
                    at,
                });
                state.pending_cable_menu = None;
            }
            Some(CablePick::Delete) => {
                actions.push(PatchAction::DeleteCable(cable_i));
                state.pending_cable_menu = None;
            }
            None => {
                let close = ui
                    .ctx()
                    .input(|i| i.pointer.any_click() || i.key_pressed(egui::Key::Escape));
                if close && ui.ctx().cumulative_frame_nr() != opened {
                    state.pending_cable_menu = None;
                }
            }
        }
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use glitcherz_model::Project;

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

    /// A freshly added node's placed position must survive `ensure_positions`
    /// (the auto-placement only fills *missing* entries). Mirrors the real glue
    /// order: document node pushed first, then `place_node`.
    #[test]
    fn place_node_position_survives_ensure_positions() {
        let mut project = Project::default();
        let new_id = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Mixer {
            node_id: new_id,
            track: 0,
            inputs: 2,
            channels: vec![Default::default(); 2],
            out_pan: 0.0,
            out_muted: false,
        });
        let doc = &project.graph;
        let mut state = PatchState::default();
        state.ensure_positions(doc, project.midi_tracks.len(), project.control_tracks.len());

        let at = Pos2::new(555.0, 77.0);
        state.place_node(new_id, at);
        state.ensure_positions(doc, project.midi_tracks.len(), project.control_tracks.len());

        assert_eq!(state.positions.get(&PatchId::Node(new_id)), Some(&at));
    }

    /// Layout export/import: stale ids are dropped, existing positions roundtrip.
    #[test]
    fn export_layout_filters_stale_and_roundtrips() {
        let mut project = Project::default();
        let id = project.graph.next_node_id();
        project.graph.nodes.push(NodeDoc::Mixer {
            node_id: id,
            track: 0,
            inputs: 2,
            channels: vec![Default::default(); 2],
            out_pan: 0.0,
            out_muted: false,
        });
        let doc = &project.graph;
        let mut state = PatchState::default();
        state.ensure_positions(doc, project.midi_tracks.len(), project.control_tracks.len());
        // Stale entry: a node id that is no longer in the document.
        state
            .positions
            .insert(PatchId::Node(9999), Pos2::new(1.0, 2.0));

        let layout = state.export_layout(doc);
        assert!(layout.nodes.contains_key(&id), "live node exported");
        assert!(!layout.nodes.contains_key(&9999), "stale id dropped");
        assert!(layout.timeline.is_some());
        assert!(layout.master.is_some());

        let mut fresh = PatchState::default();
        fresh.import_layout(&layout);
        for (want_id, want_p) in &state.positions {
            if *want_id == PatchId::Node(9999) {
                assert!(!fresh.positions.contains_key(want_id));
            } else {
                assert_eq!(fresh.positions.get(want_id), Some(want_p));
            }
        }
    }

    /// Cable-cut hit test: a path hugging a cable's curve hits it; a far-away path
    /// hits nothing.
    #[test]
    fn cut_path_hits_crossing_cable_only() {
        let mut project = Project::default();
        let mixer = project.graph.first_mixer().unwrap().node_id();
        project
            .graph
            .cables
            .push(CableDoc::audio(mixer, PortRef::Master));
        let idx = project.graph.cables.len() - 1;
        let doc = &project.graph;
        let (midi, auto) = (project.midi_tracks.len(), project.control_tracks.len());
        let mut state = PatchState::default();
        state.ensure_positions(doc, midi, auto);

        let rect_min = Pos2::ZERO; // pan = 0, zoom = 1 → screen == graph space
        let bez = state
            .cable_controls(rect_min, &doc.cables[idx], midi, auto, doc)
            .expect("cable endpoints resolve");
        let path = bezier_points(bez, 8);
        let hits = state.cables_crossing_path(rect_min, &path, doc, midi, auto);
        assert!(hits.contains(&idx), "path along the cable must hit it");

        let far = [Pos2::new(-3000.0, -3000.0), Pos2::new(-2500.0, -2500.0)];
        assert!(
            state
                .cables_crossing_path(rect_min, &far, doc, midi, auto)
                .is_empty(),
            "a path far from all cables must hit nothing"
        );
    }

    #[test]
    fn dist_point_seg_basics() {
        let a = Pos2::new(0.0, 0.0);
        let b = Pos2::new(10.0, 0.0);
        assert_eq!(dist_point_seg(Pos2::new(5.0, 3.0), a, b), 3.0);
        assert_eq!(dist_point_seg(Pos2::new(-4.0, 0.0), a, b), 4.0);
        // Beyond the end: clamps to the endpoint b.
        assert_eq!(dist_point_seg(Pos2::new(15.0, 0.0), a, b), 5.0);
        assert!((dist_point_seg(Pos2::new(15.0, 4.0), a, b) - 41.0f32.sqrt()).abs() < 1e-4);
        assert_eq!(dist_point_seg(Pos2::new(1.0, 0.0), a, a), 1.0);
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
        let mixer = doc.first_mixer().unwrap().node_id();
        let delay = doc.builtin_delay_of_track(0).unwrap().node_id();

        // Audio: effects/mixers/master output accept; instruments reject.
        assert!(accepts(CableKind::Audio, PatchId::Node(delay), doc));
        assert!(accepts(CableKind::Audio, PatchId::Node(mixer), doc));
        assert!(accepts(CableKind::Audio, PatchId::Master, doc));
        assert!(!accepts(CableKind::Audio, PatchId::Node(instrument), doc));
        // MIDI: instruments accept; everything else rejects.
        assert!(accepts(CableKind::Midi, PatchId::Node(instrument), doc));
        assert!(!accepts(CableKind::Midi, PatchId::Node(mixer), doc));
        assert!(!accepts(CableKind::Midi, PatchId::Master, doc));
        // Automation: instruments accept (mixer-fader automation deferred).
        assert!(accepts(
            CableKind::Automation,
            PatchId::Node(instrument),
            doc
        ));
        assert!(!accepts(CableKind::Automation, PatchId::Node(mixer), doc));
        assert!(accepts(
            CableKind::Automation,
            PatchId::Node(instrument),
            doc
        ));
        assert!(!accepts(CableKind::Automation, PatchId::Master, doc));
    }
}
