//! Arrangement timeline view (timeline spec T3, `docs/spec/timeline.md`).
//!
//! Rows = tracks (MIDI + control); each row shows its clip blocks on an
//! absolute-tick ruler (bars). Interactions:
//!
//! - drag a clip body → move (snapped to bars);
//! - drag its right edge → resize (≥ 1 bar, snapped to bars);
//! - double-click an empty lane spot → insert a one-bar clip;
//! - double-click a clip → jump to the Edit tab (piano roll / curve lane);
//! - right-click a clip → delete;
//! - header: click to select the track, toggle mute (MIDI rows only).
//!
//! Pure view + intent: position/span edits are applied directly to the passed
//! track slices (pure document data), while everything that needs the engine
//! (re-push clips) or cross-panel state (selection, editor target) is reported
//! back as [`TimelineAction`]s.

use eframe::egui;
use egui::{
    Align2, Color32, EventFilter, FontId, Id, Key, Modifiers, Pos2, Rect, Sense, Stroke,
    StrokeKind, Vec2, pos2,
};
use resoz_model::{
    ControlClip, ControlTrack, MAX_ARRANGEMENT_BARS, MAX_CLIPS_PER_TRACK, MidiClip, MidiTrack,
    TICKS_PER_BAR,
};

/// Fixed width of the row-header column.
pub const HEADER_WIDTH: f32 = 108.0;
/// Height of one track row.
pub const ROW_HEIGHT: f32 = 28.0;
/// Height of the bar-number ruler.
const RULER_HEIGHT: f32 = 20.0;
/// Half-width of the grab zone at a clip's right edge (resize handle).
const EDGE_GRAB: f32 = 6.0;
/// Minimum timeline content width (bars), even for an empty arrangement.
const MIN_CONTENT_BARS: u32 = 16;
/// Accent color for selected clips (matches the roll's selection family).
const SELECTED: Color32 = Color32::from_rgb(240, 190, 80);

/// One selected clip, addressed across the arrangement (M4b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipSel {
    pub is_control: bool,
    pub track: usize,
    pub clip: usize,
}

/// Which row kind a drag belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Midi(usize),
    Control(usize),
}

impl RowKind {
    fn key(self) -> (bool, usize) {
        match self {
            RowKind::Midi(t) => (false, t),
            RowKind::Control(t) => (true, t),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    Move {
        row: RowKind,
        clip: usize,
        grab_ticks: f32,
    },
    Resize {
        row: RowKind,
        clip: usize,
    },
}

/// Rubber band in progress (screen coordinates; may span several rows).
#[derive(Debug, Clone, Copy)]
struct Marquee {
    origin: Pos2,
    current: Pos2,
    /// Shift/Ctrl held when the band started: union with the pre-existing
    /// selection instead of replacing it.
    additive: bool,
}

/// Cross-frame interaction state.
#[derive(Debug)]
pub struct TimelineState {
    /// Horizontal zoom: pixels per bar.
    pub px_per_bar: f32,
    drag: Option<Drag>,
    /// Selected clips across tracks (M4b).
    pub selection: Vec<ClipSel>,
    /// Rubber band in progress.
    marquee: Option<Marquee>,
    /// Set by the row that sees `drag_stopped` while a band is live; the
    /// band is cleared only after show() has folded that frame's hits
    /// (later rows must still report on the final frame).
    marquee_ending: bool,
    /// Whether any lane row owned keyboard focus last frame (toolbar hint).
    had_focus: bool,
    /// The focused lane row's widget id — the app asks egui whether it
    /// still holds focus to gate the virtual piano against the command
    /// keys (G/T/K/D collide with the FL layout).
    focus_id: Option<Id>,
}

impl Default for TimelineState {
    fn default() -> Self {
        Self {
            px_per_bar: 56.0,
            drag: None,
            selection: Vec::new(),
            marquee: None,
            marquee_ending: false,
            had_focus: false,
            focus_id: None,
        }
    }
}

impl TimelineState {
    /// Whether a lane row owns keyboard focus *right now*, per egui (no
    /// frame lag, no staleness once the timeline stops being laid out).
    pub fn owns_keyboard_focus(&self, ctx: &egui::Context) -> bool {
        self.focus_id
            .is_some_and(|id| ctx.memory(|m| m.has_focus(id)))
    }
}

/// One action for the caller (engine pushes / cross-panel state changes).
///
/// Clip edits carry their data (`is_control` picks the track table): the
/// timeline is a view and must not mutate the document — the caller turns
/// these into undoable edits (M1.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineAction {
    /// A MIDI track's clips changed; re-push to the engine.
    MidiEdited(usize),
    /// A control track's clips changed; re-push to the engine.
    ControlEdited(usize),
    /// Create a clip on a track at the snapped start tick.
    AddClip {
        is_control: bool,
        track: usize,
        start_ticks: u32,
    },
    /// Move and/or resize a clip (one frame of a drag; the caller merges the
    /// gesture into a single undo step).
    MoveResizeClip {
        is_control: bool,
        track: usize,
        clip: usize,
        start_ticks: u32,
        span_ticks: u32,
    },
    /// Delete the clip at `clip`.
    RemoveClip {
        is_control: bool,
        track: usize,
        clip: usize,
    },
    /// Delete every clip in the list (one undo step; per track the indices
    /// are applied descending by the caller).
    RemoveClipsBatch {
        clips: Vec<ClipSel>,
    },
    SelectTrack(usize),
    SelectControlTrack(usize),
    ToggleMute(usize, bool),
    OpenMidiEditor {
        track: usize,
        clip: usize,
    },
    OpenControlEditor {
        track: usize,
        clip: usize,
    },
    AddMidiTrack,
    RemoveLastMidiTrack,
}

/// Per-frame view inputs (immutable).
/// The clip currently targeted by the floating editor window (highlighted on
/// the timeline so a double-click gives immediate visual feedback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditHighlight {
    /// `false` = MIDI row, `true` = control row.
    pub is_control: bool,
    pub track: usize,
    pub clip: usize,
}

pub struct TimelineParams<'a> {
    /// Mute flags of the MIDI tracks (channel-strip node state).
    pub muted: &'a [bool],
    pub selected_track: usize,
    pub selected_auto: usize,
    pub playhead_ticks: f32,
    pub playing: bool,
    /// Clip to outline as "being edited" (from the floating editor window).
    pub edit_target: Option<EditHighlight>,
}

/// Placement semantics shared by MIDI and control clips.
trait Placement {
    fn start(&self) -> u32;
    fn span(&self) -> u32;
}

impl Placement for MidiClip {
    fn start(&self) -> u32 {
        self.start_ticks
    }
    fn span(&self) -> u32 {
        self.span_ticks
    }
}

impl Placement for ControlClip {
    fn start(&self) -> u32 {
        self.start_ticks
    }
    fn span(&self) -> u32 {
        self.span_ticks
    }
}

struct RowColors {
    lane_bg: Color32,
    grid: Color32,
    clip_fill: Color32,
    clip_stroke: Color32,
    clip_text: Color32,
    /// Outline for the clip currently open in the editor window.
    edit_highlight: Color32,
}

impl RowColors {
    fn midi(selected: bool) -> Self {
        Self {
            lane_bg: Color32::from_gray(if selected { 34 } else { 28 }),
            grid: Color32::from_gray(46),
            clip_fill: if selected {
                Color32::from_rgb(72, 126, 210)
            } else {
                Color32::from_rgb(58, 102, 176)
            },
            clip_stroke: Color32::from_rgb(140, 180, 240),
            clip_text: Color32::from_rgb(225, 235, 250),
            edit_highlight: Color32::from_rgb(120, 225, 255),
        }
    }

    fn control(selected: bool) -> Self {
        Self {
            lane_bg: Color32::from_gray(if selected { 34 } else { 28 }),
            grid: Color32::from_gray(46),
            clip_fill: if selected {
                Color32::from_rgb(196, 132, 52)
            } else {
                Color32::from_rgb(160, 108, 44)
            },
            clip_stroke: Color32::from_rgb(235, 195, 130),
            clip_text: Color32::from_rgb(250, 240, 220),
            edit_highlight: Color32::from_rgb(255, 215, 110),
        }
    }
}

enum HeaderKind {
    Midi {
        index: usize,
        name: String,
        muted: bool,
        selected: bool,
    },
    Control {
        index: usize,
        selected: bool,
    },
}

/// Draws the whole arrangement (toolbar, ruler, rows) and returns the actions
/// accumulated this frame.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut TimelineState,
    midi_tracks: &[MidiTrack],
    control_tracks: &[ControlTrack],
    p: TimelineParams<'_>,
) -> Vec<TimelineAction> {
    let mut actions = Vec::new();
    let px_per_tick = state.px_per_bar / TICKS_PER_BAR as f32;
    let midi_end = midi_tracks
        .iter()
        .flat_map(|t| t.clips.iter())
        .map(|c| c.start() + c.span())
        .max()
        .unwrap_or(0);
    let control_end = control_tracks
        .iter()
        .flat_map(|t| t.clips.iter())
        .map(|c| c.start() + c.span())
        .max()
        .unwrap_or(0);
    let max_end = midi_end.max(control_end);
    let content_ticks = max_end.max(MIN_CONTENT_BARS * TICKS_PER_BAR);
    let content_px = content_ticks as f32 * px_per_tick + state.px_per_bar;

    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        // Toolbar: zoom + track add/remove + hints.
        ui.horizontal(|ui| {
            ui.strong("Arrangement");
            if ui.button("-").on_hover_text("Zoom out").clicked() {
                state.px_per_bar = (state.px_per_bar / 1.3).max(14.0);
            }
            if ui.button("+").on_hover_text("Zoom in").clicked() {
                state.px_per_bar = (state.px_per_bar * 1.3).min(280.0);
            }
            ui.separator();
            if ui.button("+ Track").clicked() {
                actions.push(TimelineAction::AddMidiTrack);
            }
            if ui.button("- Track").clicked() {
                actions.push(TimelineAction::RemoveLastMidiTrack);
            }
            ui.weak("Drag move · edge resize · dbl-click insert/edit · right-click delete · marquee: select · Del: delete");
        });
        // The focus chip mirrors the roll's: keyboard shortcuts are
        // focus-gated, and the gate must be visible to be diagnosable.
        if !state.had_focus {
            ui.weak(egui::RichText::new("no keyboard focus — click the timeline").color(Color32::YELLOW));
        }

        // Ruler: bar numbers over the content width.
        ui.horizontal(|ui| {
            ui.allocate_exact_size(Vec2::new(HEADER_WIDTH, RULER_HEIGHT), Sense::hover());
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(content_px, RULER_HEIGHT), Sense::hover());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 0.0, Color32::from_gray(24));
            let step = if state.px_per_bar < 28.0 { 4 } else { 1 };
            let bars = content_ticks / TICKS_PER_BAR;
            let mut bar = 0;
            while bar <= bars {
                let x = rect.left() + bar as f32 * state.px_per_bar;
                painter.line_segment(
                    [pos2(x, rect.top() + 4.0), pos2(x, rect.bottom())],
                    Stroke::new(1.0, Color32::from_gray(55)),
                );
                painter.text(
                    pos2(x + 3.0, rect.top() + 2.0),
                    Align2::LEFT_TOP,
                    format!("{}", bar + 1),
                    FontId::proportional(10.0),
                    Color32::from_gray(150),
                );
                bar += step;
            }
        });

        // Marquee selection: every row reports the clips its lane segment
        // of the band covers; show() combines them into the selection once
        // all rows have run (M4b: the band may span several rows).
        let mut marquee_hits = Vec::new();
        state.had_focus = false;

        // MIDI rows.
        for (i, track) in midi_tracks.iter().enumerate() {
            let name = if track.name.is_empty() {
                format!("Track {}", i + 1)
            } else {
                track.name.clone()
            };
            let header = HeaderKind::Midi {
                index: i,
                name,
                muted: p.muted.get(i).copied().unwrap_or(false),
                selected: p.selected_track == i,
            };
            let highlight = p
                .edit_target
                .and_then(|e| (!e.is_control && e.track == i).then_some(e.clip));
            lane_row(
                ui,
                state,
                &mut actions,
                &mut marquee_hits,
                header,
                RowKind::Midi(i),
                &track.clips,
                px_per_tick,
                content_px,
                p.playhead_ticks,
                RowColors::midi(p.selected_track == i),
                highlight,
                &|clip| format!("{} notes", clip.notes().len()),
            );
        }

        // Control rows.
        if !control_tracks.is_empty() {
            ui.add_space(4.0);
            ui.weak("Control tracks (delivery endpoint = node-graph cable)");
            for (i, track) in control_tracks.iter().enumerate() {
                let header = HeaderKind::Control {
                    index: i,
                    selected: p.selected_auto == i,
                };
                let highlight = p
                    .edit_target
                    .and_then(|e| (e.is_control && e.track == i).then_some(e.clip));
                lane_row(
                    ui,
                    state,
                    &mut actions,
                    &mut marquee_hits,
                    header,
                    RowKind::Control(i),
                    &track.clips,
                    px_per_tick,
                    content_px,
                    p.playhead_ticks,
                    RowColors::control(p.selected_auto == i),
                    highlight,
                    &|clip| {
                        if clip.curve.is_empty() {
                            "empty".to_string()
                        } else {
                            format!("{} anchors", clip.curve.len())
                        }
                    },
                );
            }
        }

        // Fold the rows' marquee reports into the selection: replace with
        // the band's hits, or union with what was selected when it started.
        // An ending band folds this final frame first, then clears below.
        if let Some(m) = &state.marquee {
            let mut selection = if m.additive {
                state.selection.clone()
            } else {
                Vec::new()
            };
            for (row, hits) in &marquee_hits {
                let (is_control, track) = row.key();
                for clip in hits {
                    let sel = ClipSel {
                        is_control,
                        track,
                        clip: *clip,
                    };
                    if !selection.contains(&sel) {
                        selection.push(sel);
                    }
                }
            }
            state.selection = selection;
        }
        if state.marquee_ending {
            state.marquee = None;
            state.marquee_ending = false;
        }
    });
    actions
}

/// One row: header cell + clip lane (grid, clips, playhead, interactions).
#[allow(clippy::too_many_arguments)]
fn lane_row<T: Placement>(
    ui: &mut egui::Ui,
    state: &mut TimelineState,
    actions: &mut Vec<TimelineAction>,
    marquee_hits: &mut Vec<(RowKind, Vec<usize>)>,
    header: HeaderKind,
    row: RowKind,
    clips: &[T],
    px_per_tick: f32,
    content_px: f32,
    playhead_ticks: f32,
    colors: RowColors,
    highlight: Option<usize>,
    clip_label: &dyn Fn(&T) -> String,
) {
    ui.horizontal(|ui| {
        // Header cell.
        match header {
            HeaderKind::Midi {
                index,
                name,
                muted,
                selected,
            } => {
                let mut btn = egui::Button::new(egui::RichText::new(name).size(11.0));
                if selected {
                    btn = btn.fill(Color32::from_rgb(60, 90, 140));
                }
                if ui
                    .add_sized([HEADER_WIDTH - 44.0, ROW_HEIGHT - 4.0], btn)
                    .clicked()
                {
                    actions.push(TimelineAction::SelectTrack(index));
                }
                let mut mute_btn = egui::Button::new(if muted { "M" } else { "On" });
                if muted {
                    mute_btn = mute_btn.fill(Color32::from_rgb(140, 60, 50));
                }
                if ui
                    .add_sized([40.0, ROW_HEIGHT - 4.0], mute_btn)
                    .on_hover_text("Toggle mute")
                    .clicked()
                {
                    actions.push(TimelineAction::ToggleMute(index, !muted));
                }
            }
            HeaderKind::Control { index, selected } => {
                let mut btn = egui::Button::new(
                    egui::RichText::new(format!("Curve {}", index + 1)).size(11.0),
                );
                if selected {
                    btn = btn.fill(Color32::from_rgb(120, 90, 40));
                }
                if ui
                    .add_sized([HEADER_WIDTH, ROW_HEIGHT - 4.0], btn)
                    .clicked()
                {
                    actions.push(TimelineAction::SelectControlTrack(index));
                }
            }
        }

        // Lane surface.
        let (lane, response) =
            ui.allocate_exact_size(Vec2::new(content_px, ROW_HEIGHT), Sense::click_and_drag());
        // The row owns keyboard focus while in use (command keys land in
        // M4b.2+; the plumbing is here from the start). Any press counts.
        if response.clicked() || response.is_pointer_button_down_on() {
            response.request_focus();
        }
        if response.has_focus() {
            state.had_focus = true;
            state.focus_id = Some(response.id);
        }
        let painter = ui.painter_at(lane);
        painter.rect_filled(lane, 0.0, colors.lane_bg);

        // Bar grid.
        let bars = (content_px / px_per_tick / TICKS_PER_BAR as f32).ceil() as u32;
        for bar in 0..=bars {
            let x = lane.left() + bar as f32 * px_per_tick * TICKS_PER_BAR as f32;
            if x > lane.right() {
                break;
            }
            painter.line_segment(
                [pos2(x, lane.top()), pos2(x, lane.bottom())],
                Stroke::new(1.0, colors.grid),
            );
        }

        let tick_at = |x: f32| ((x - lane.left()) / px_per_tick).max(0.0);

        let (is_control, track) = row.key();

        // Clip blocks.
        for (index, clip) in clips.iter().enumerate() {
            let x0 = lane.left() + clip.start() as f32 * px_per_tick;
            let x1 = lane.left() + (clip.start() + clip.span()) as f32 * px_per_tick;
            let rect = Rect::from_min_max(
                pos2(x0 + 1.0, lane.top() + 2.0),
                pos2(x1 - 1.0, lane.bottom() - 2.0),
            );
            if rect.right() < lane.left() || rect.left() > lane.right() || rect.width() < 2.0 {
                continue;
            }
            painter.rect_filled(rect, 3.0, colors.clip_fill);
            painter.rect_stroke(
                rect,
                3.0,
                Stroke::new(1.0, colors.clip_stroke),
                StrokeKind::Inside,
            );
            if state.selection.contains(&ClipSel {
                is_control,
                track,
                clip: index,
            }) {
                painter.rect_stroke(rect, 3.0, Stroke::new(2.0, SELECTED), StrokeKind::Inside);
            }
            if highlight == Some(index) {
                painter.rect_stroke(
                    rect,
                    3.0,
                    Stroke::new(2.0, colors.edit_highlight),
                    StrokeKind::Inside,
                );
            }
            painter.text(
                pos2(rect.left() + 4.0, rect.center().y),
                Align2::LEFT_CENTER,
                clip_label(clip),
                FontId::proportional(10.0),
                colors.clip_text,
            );
        }

        // Playhead.
        let playhead_x = lane.left() + playhead_ticks * px_per_tick;
        if lane.left() <= playhead_x && playhead_x <= lane.right() {
            painter.line_segment(
                [
                    pos2(playhead_x, lane.top()),
                    pos2(playhead_x, lane.bottom()),
                ],
                Stroke::new(1.5, Color32::from_rgb(235, 90, 90)),
            );
        }

        // Interactions (all edits are emitted as actions; nothing mutates the
        // document here).

        // Double-click: open the clip's editor, or insert a new clip on empty
        // space.
        if response.double_clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let tick = tick_at(pos.x);
                if let Some(index) = hit_clip(clips, tick) {
                    match row {
                        RowKind::Midi(t) => actions.push(TimelineAction::OpenMidiEditor {
                            track: t,
                            clip: index,
                        }),
                        RowKind::Control(t) => actions.push(TimelineAction::OpenControlEditor {
                            track: t,
                            clip: index,
                        }),
                    }
                } else if clips.len() < MAX_CLIPS_PER_TRACK {
                    let bound = MAX_ARRANGEMENT_BARS * TICKS_PER_BAR - TICKS_PER_BAR;
                    let (is_control, track) = match row {
                        RowKind::Midi(t) => (false, t),
                        RowKind::Control(t) => (true, t),
                    };
                    actions.push(TimelineAction::AddClip {
                        is_control,
                        track,
                        start_ticks: snap_bar(tick).min(bound),
                    });
                }
            }
        }

        // Drag start: on a clip, pick move vs resize by the grab zone at
        // the right edge (and select it if it was not); on empty space,
        // start a rubber band (M4b selection).
        if response.drag_started() {
            if let Some(pos) = response.interact_pointer_pos() {
                let tick = tick_at(pos.x);
                if let Some(index) = hit_clip(clips, tick) {
                    let additive = ui.ctx().input(|i| i.modifiers.shift || i.modifiers.ctrl);
                    let sel = ClipSel {
                        is_control,
                        track,
                        clip: index,
                    };
                    if additive {
                        if let Some(at) = state.selection.iter().position(|s| *s == sel) {
                            state.selection.remove(at);
                        } else {
                            state.selection.push(sel);
                        }
                    } else if !state.selection.contains(&sel) {
                        state.selection = vec![sel];
                    }
                    let edge_x = lane.left()
                        + (clips[index].start() + clips[index].span()) as f32 * px_per_tick;
                    if (pos.x - edge_x).abs() <= EDGE_GRAB {
                        state.drag = Some(Drag::Resize { row, clip: index });
                    } else {
                        let grab = tick - clips[index].start() as f32;
                        state.drag = Some(Drag::Move {
                            row,
                            clip: index,
                            grab_ticks: grab,
                        });
                    }
                } else {
                    let additive = ui.ctx().input(|i| i.modifiers.shift || i.modifiers.ctrl);
                    state.marquee = Some(Marquee {
                        origin: pos,
                        current: pos,
                        additive,
                    });
                }
            }
        }

        if response.dragged() {
            if let Some(pos) = response.interact_pointer_pos() {
                if let Some(m) = state.marquee.as_mut() {
                    m.current = pos;
                } else {
                    match state.drag {
                        Some(Drag::Move {
                            clip, grab_ticks, ..
                        }) if clip < clips.len() => {
                            let span = clips[clip].span();
                            let limit = MAX_ARRANGEMENT_BARS * TICKS_PER_BAR - span;
                            let start = snap_bar(tick_at(pos.x) - grab_ticks).min(limit);
                            if clips[clip].start() != start {
                                let (is_control, track) = match row {
                                    RowKind::Midi(t) => (false, t),
                                    RowKind::Control(t) => (true, t),
                                };
                                actions.push(TimelineAction::MoveResizeClip {
                                    is_control,
                                    track,
                                    clip,
                                    start_ticks: start,
                                    span_ticks: span,
                                });
                            }
                        }
                        Some(Drag::Resize { clip, .. }) if clip < clips.len() => {
                            let start = clips[clip].start();
                            let limit = MAX_ARRANGEMENT_BARS * TICKS_PER_BAR - start;
                            let span = snap_bar(tick_at(pos.x) - start as f32)
                                .clamp(TICKS_PER_BAR, limit.max(TICKS_PER_BAR));
                            if clips[clip].span() != span {
                                let (is_control, track) = match row {
                                    RowKind::Midi(t) => (false, t),
                                    RowKind::Control(t) => (true, t),
                                };
                                actions.push(TimelineAction::MoveResizeClip {
                                    is_control,
                                    track,
                                    clip,
                                    start_ticks: start,
                                    span_ticks: span,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        if response.drag_stopped() {
            state.drag = None;
            if state.marquee.is_some() {
                state.marquee_ending = true;
            }
        }

        // Marquee hit reporting: every row contributes the clips its lane
        // segment of the band covers, every frame the band is live.
        if let Some(m) = &state.marquee {
            let band = Rect::from_two_pos(m.origin, m.current);
            let hits = clips
                .iter()
                .enumerate()
                .filter(|(_, clip)| {
                    let x0 = lane.left() + clip.start() as f32 * px_per_tick;
                    let x1 = lane.left() + (clip.start() + clip.span()) as f32 * px_per_tick;
                    Rect::from_min_max(pos2(x0, lane.top()), pos2(x1, lane.bottom()))
                        .intersects(band)
                })
                .map(|(index, _)| index)
                .collect();
            marquee_hits.push((row, hits));
        }

        // The band itself, clipped to this row's lane segment.
        if let Some(m) = &state.marquee {
            let band = Rect::from_two_pos(m.origin, m.current);
            let segment = band.intersect(lane);
            if segment.is_positive() {
                painter.rect_filled(
                    segment,
                    0.0,
                    Color32::from_rgba_unmultiplied(80, 140, 220, 40),
                );
                painter.rect_stroke(
                    segment,
                    0.0,
                    Stroke::new(1.0, Color32::from_rgb(80, 140, 220)),
                    StrokeKind::Inside,
                );
            }
        }

        // Right-click: cancel a live band, otherwise delete the clip under
        // the pointer.
        if response.secondary_clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                if state.marquee.take().is_none() {
                    if let Some(index) = hit_clip(clips, tick_at(pos.x)) {
                        actions.push(TimelineAction::RemoveClip {
                            is_control,
                            track,
                            clip: index,
                        });
                        state.drag = None;
                    }
                }
            }
        }

        // Keyboard: focus-gated, roll-style. M4b.1 wires Escape (clear the
        // selection / cancel the band) and Delete (remove the selection as
        // one undo step); the command family lands in M4b.2+.
        if response.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    response.id,
                    EventFilter {
                        escape: true,
                        ..Default::default()
                    },
                )
            });
            let escape = ui
                .ctx()
                .input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
            let delete = ui
                .ctx()
                .input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete));
            if escape {
                state.selection.clear();
                state.marquee = None;
                state.marquee_ending = false;
            }
            if delete && !state.selection.is_empty() {
                actions.push(TimelineAction::RemoveClipsBatch {
                    clips: state.selection.clone(),
                });
                state.selection.clear();
            }
        }
    });
}

fn hit_clip<T: Placement>(clips: &[T], tick: f32) -> Option<usize> {
    clips
        .iter()
        .position(|c| tick >= c.start() as f32 && tick < (c.start() + c.span()) as f32)
}

/// Snaps to the nearest whole bar (0 stays 0).
fn snap_bar(ticks: f32) -> u32 {
    let bar = TICKS_PER_BAR as f32;
    ((ticks / bar).round().max(0.0) as u32) * TICKS_PER_BAR
}
