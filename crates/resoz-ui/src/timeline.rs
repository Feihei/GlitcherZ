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

/// One move of the G modal (M4b.3): the clip at `from` lands on `to_track`
/// (same kind only) at `start_ticks`. One action = one undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipMove {
    pub from: ClipSel,
    pub to_track: usize,
    pub start_ticks: u32,
}

/// One copy of the Shift+D modal (M4b.4): the content of the clip at
/// `from` is cloned onto `to_track` (same kind only) at `start_ticks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipDup {
    pub from: ClipSel,
    pub to_track: usize,
    pub start_ticks: u32,
}

/// One span edit of the T modal (M4b.3): the clip at `sel` gets a new
/// `start_ticks`/`span_ticks` (bar-aligned, ≥ 1 bar, same track).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipSpanEdit {
    pub sel: ClipSel,
    pub start_ticks: u32,
    pub span_ticks: u32,
}

/// Modal transform kind (Blender-style, M4b.3): armed by G/T, the ghost
/// follows the pointer, LMB finalizes, RMB/Esc cancels with zero cost
/// (the document is not touched until the finalize).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModalKind {
    /// G: free move — bar delta in time + row delta (same kind only).
    /// X toggles the time lock (row delta forced to 0).
    Grab { time_locked: bool },
    /// Shift+D: duplicate the selection — the clones enter a grab-style
    /// modal; the originals stay untouched until the drop.
    Duplicate { time_locked: bool },
    /// T: stretch the end edge of every selected clip (Shift+T: the start
    /// edge). Bar delta only.
    Stretch { start_edge: bool },
}

/// Live modal state. `origin` snapshots the selection at arm time; since
/// nothing mutates the document during the modal, those indices stay valid.
#[derive(Debug, Clone)]
struct Modal {
    kind: ModalKind,
    origin: Vec<ClipSel>,
    /// Pointer position when the modal was armed.
    grab: Pos2,
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
    /// Knife armed via K (M4b.2): the next LMB click cuts the clip under
    /// the pointer at the bar-snapped position. Stays armed for multiple
    /// cuts; Esc / RMB disarms.
    knife_armed: bool,
    /// One-frame status message (invalid cut explanation); the toolbar
    /// drains it the same frame it is set.
    status: Option<String>,
    /// Live modal transform (G/T), if any.
    modal: Option<Modal>,
    /// Set when a lane row sees the modal-confirming LMB click; show()
    /// finalizes after all rows have run and clears the flag.
    modal_confirm: bool,
    /// Set when a lane row sees a modal-cancelling RMB; show() clears the
    /// modal after all rows have run.
    modal_cancel: bool,
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
            knife_armed: false,
            status: None,
            modal: None,
            modal_confirm: false,
            modal_cancel: false,
        }
    }
}

impl TimelineState {
    /// Whether a lane row owns keyboard focus *right now*, per egui (no
    /// frame lag, no staleness once the timeline stops being laid out).
    ///
    /// Both parts matter. egui keeps focus ON ITS ID even after the row
    /// that claimed it moved to another id (positional auto ids — see
    /// `lane_row`), so a focus-id-only check would report "focused" while
    /// every row's `has_focus` is false: the focus is parked on a phantom,
    /// all focus-gated keys are dead, and the toolbar chip — which reads
    /// the rows, not this — disagrees with what this says. Requiring a row
    /// to have reported focus keeps the three views consistent.
    pub fn owns_keyboard_focus(&self, ctx: &egui::Context) -> bool {
        self.had_focus
            && self
                .focus_id
                .is_some_and(|id| ctx.memory(|m| m.has_focus(id)))
    }
    /// Whether a modal transform (G / T / Shift+D) is live right now.
    /// The integration tests drive real egui frames and can only reach
    /// state through public accessors — same observability as the roll.
    pub fn modal_active(&self) -> bool {
        self.modal.is_some()
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
    /// Split the clip at `clip` at `at_ticks` (bar-snapped by the view):
    /// the left half keeps the start, the right half clones the content
    /// (loop-based clips both play the same loop). One undo step.
    SplitClip {
        is_control: bool,
        track: usize,
        clip: usize,
        at_ticks: u32,
    },
    /// Move every clip in the list (G modal finalize; may cross tracks of
    /// the same kind). One undo step.
    MoveClipsBatch {
        moves: Vec<ClipMove>,
    },
    /// Stretch every clip in the list (T modal finalize). One undo step.
    StretchClipsBatch {
        spans: Vec<ClipSpanEdit>,
    },
    /// Clone every clip in the list onto its target (Shift+D finalize;
    /// originals untouched). One undo step.
    DuplicateClipsBatch {
        dups: Vec<ClipDup>,
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
pub(crate) trait Placement {
    fn start(&self) -> u32;
    fn span(&self) -> u32;
    fn set_start(&mut self, v: u32);
    fn set_span(&mut self, v: u32);
}

impl Placement for MidiClip {
    fn start(&self) -> u32 {
        self.start_ticks
    }
    fn span(&self) -> u32 {
        self.span_ticks
    }
    fn set_start(&mut self, v: u32) {
        self.start_ticks = v;
    }
    fn set_span(&mut self, v: u32) {
        self.span_ticks = v;
    }
}

impl Placement for ControlClip {
    fn start(&self) -> u32 {
        self.start_ticks
    }
    fn span(&self) -> u32 {
        self.span_ticks
    }
    fn set_start(&mut self, v: u32) {
        self.start_ticks = v;
    }
    fn set_span(&mut self, v: u32) {
        self.span_ticks = v;
    }
}

/// Splits `clips[index]` at `at` in place (M4b.2): the left half keeps the
/// start with a shortened span, the right half is inserted at `index + 1`
/// with cloned content (loop-based clips both play the same loop). Returns
/// false untouched when the index is out of bounds, the track is full, or
/// `at` is not strictly inside the clip.
pub(crate) fn split_clip_in<T: Placement + Clone>(
    clips: &mut Vec<T>,
    at: u32,
    index: usize,
) -> bool {
    let Some(c) = clips.get(index) else {
        return false;
    };
    let (start, end) = (c.start(), c.start() + c.span());
    if at <= start || at >= end || clips.len() >= MAX_CLIPS_PER_TRACK {
        return false;
    }
    let mut right = clips[index].clone();
    right.set_start(at);
    right.set_span(end - at);
    clips[index].set_span(at - start);
    clips.insert(index + 1, right);
    true
}

/// Looks up a selected clip's (start, span) across both track kinds.
fn clip_geometry(
    midi_tracks: &[MidiTrack],
    control_tracks: &[ControlTrack],
    sel: ClipSel,
) -> Option<(u32, u32)> {
    if sel.is_control {
        let c = control_tracks.get(sel.track)?.clips.get(sel.clip)?;
        Some((c.start(), c.span()))
    } else {
        let c = midi_tracks.get(sel.track)?.clips.get(sel.clip)?;
        Some((c.start(), c.span()))
    }
}

/// Clamps a row delta so the whole selection stays inside its kind's rows
/// (rigid move: one delta for everyone, clamped globally, not per clip).
fn clamp_row_delta(row_delta: i32, sel: &[ClipSel], rows: usize) -> i32 {
    if sel.is_empty() || rows == 0 {
        return 0;
    }
    let min_t = sel.iter().map(|s| s.track).min().unwrap_or(0);
    let max_t = sel.iter().map(|s| s.track).max().unwrap_or(0);
    let lo = -(min_t as i32);
    let hi = (rows - 1) as i32 - max_t as i32;
    row_delta.clamp(lo.min(0), hi.max(0))
}

/// Whether `target` (start, span) completely covers any clip in `existing`
/// (start, span) — the refused case (M4b decision: partial overlap is
/// allowed for pickups, total coverage buries an existing clip).
fn fully_covers(target: (u32, u32), existing: &[(u32, u32)]) -> bool {
    let t_end = target.0 + target.1;
    existing
        .iter()
        .any(|(s, sp)| *s >= target.0 && *s + *sp <= t_end)
}

/// All clips of a track as (start, span) pairs, skipping every index in
/// `skip` (the moving set's own originals do not block their targets).
fn track_spans<T: Placement>(clips: &[T], skip: &[usize]) -> Vec<(u32, u32)> {
    clips
        .iter()
        .enumerate()
        .filter(|(i, _)| !skip.contains(i))
        .map(|(_, c)| (c.start(), c.span()))
        .collect()
}

/// The G / Shift+D modal's per-clip targets (moves and clones share the
/// grab math). `None` when the pointer is unavailable (no ghost this
/// frame) or the modal is a Stretch.
fn grab_targets(
    modal: &Modal,
    ptr: Option<Pos2>,
    px_per_bar: f32,
    rows_midi: usize,
    rows_control: usize,
    midi_tracks: &[MidiTrack],
    control_tracks: &[ControlTrack],
) -> Option<Vec<ClipMove>> {
    let (ModalKind::Grab { time_locked } | ModalKind::Duplicate { time_locked }) = modal.kind
    else {
        return None;
    };
    let ptr = ptr?;
    let bar_delta = ((ptr.x - modal.grab.x) / px_per_bar).round() as i64;
    let mut row_delta = ((ptr.y - modal.grab.y) / ROW_HEIGHT).round() as i32;
    let is_control = modal.origin.first().map(|s| s.is_control).unwrap_or(false);
    let rows = if is_control { rows_control } else { rows_midi };
    row_delta = clamp_row_delta(row_delta, &modal.origin, rows);
    if time_locked {
        row_delta = 0;
    }
    Some(
        modal
            .origin
            .iter()
            .filter_map(|sel| {
                let (start, _) = clip_geometry(midi_tracks, control_tracks, *sel)?;
                let to_track = (sel.track as i32 + row_delta).max(0) as usize;
                let new_start = (start as i64 + bar_delta * TICKS_PER_BAR as i64).max(0) as u32;
                Some(ClipMove {
                    from: *sel,
                    to_track,
                    start_ticks: new_start,
                })
            })
            .collect(),
    )
}

/// The T modal's per-clip span edits (bar-aligned, ≥ 1 bar, within the
/// arrangement). `None` when the pointer is unavailable or not a Stretch.
fn stretch_targets(
    modal: &Modal,
    ptr: Option<Pos2>,
    px_per_bar: f32,
    midi_tracks: &[MidiTrack],
    control_tracks: &[ControlTrack],
) -> Option<Vec<ClipSpanEdit>> {
    let ModalKind::Stretch { start_edge } = modal.kind else {
        return None;
    };
    let ptr = ptr?;
    let bar_delta = ((ptr.x - modal.grab.x) / px_per_bar).round() as i64;
    let tick_delta = bar_delta * TICKS_PER_BAR as i64;
    let bar = TICKS_PER_BAR as i64;
    let max_end = MAX_ARRANGEMENT_BARS as i64 * bar;
    Some(
        modal
            .origin
            .iter()
            .filter_map(|sel| {
                let (start, span) = clip_geometry(midi_tracks, control_tracks, *sel)?;
                let (new_start, new_span) = if start_edge {
                    let d = tick_delta.clamp(-(start as i64), span as i64 - bar);
                    ((start as i64 + d) as u32, (span as i64 - d) as u32)
                } else {
                    let d = tick_delta.clamp(bar - span as i64, max_end - (start + span) as i64);
                    (start, (span as i64 + d) as u32)
                };
                Some(ClipSpanEdit {
                    sel: *sel,
                    start_ticks: new_start,
                    span_ticks: new_span,
                })
            })
            .collect(),
    )
}

/// One modal target awaiting overlap validation: the clip at `sel` wants
/// the (start, span) rectangle on row `to_track` (kind in `is_control`).
type ModalTarget = (ClipSel, (u32, u32), usize, bool);

/// Post-finalize validation for a move/stretch batch (M4b decision ②):
/// clips whose target completely covers an existing (non-moving) clip are
/// refused and stay at their origin; the rest go through.
fn filter_covered(
    targets: Vec<ModalTarget>,
    midi_tracks: &[MidiTrack],
    control_tracks: &[ControlTrack],
) -> (Vec<ModalTarget>, usize) {
    let moving: Vec<ClipSel> = targets.iter().map(|(sel, _, _, _)| *sel).collect();
    let mut kept = Vec::new();
    let mut refused = 0;
    for (sel, target, to_track, is_control) in targets {
        let skip: Vec<usize> = moving
            .iter()
            .filter(|m| m.is_control == is_control && m.track == to_track)
            .map(|m| m.clip)
            .collect();
        let existing = if is_control {
            track_spans(&control_tracks[to_track].clips, &skip)
        } else {
            track_spans(&midi_tracks[to_track].clips, &skip)
        };
        if fully_covers(target, &existing) {
            refused += 1;
        } else {
            kept.push((sel, target, to_track, is_control));
        }
    }
    (kept, refused)
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

    // The scroll area carries an explicit id: all rows below live off its
    // content ui's positional auto ids, so if anything above it comes or
    // goes every row silently re-id's — and since egui keeps focus ON THE
    // ID, the focused lane turns into a phantom (no row reports focus
    // again, every focus-gated key dies, and the "no keyboard focus" chip
    // stays lit no matter how often you click).
    egui::ScrollArea::both()
        .auto_shrink(false)
        .id_salt("resoz_timeline_scroll")
        .show(ui, |ui| {
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
            ui.separator();
            // Every variable hint must live INSIDE this child ui. Drawn in
            // the toolbar's parent instead, appearing and disappearing
            // would shift the rows' positional auto ids (see lane_row) and
            // make every row jump down and back whenever a hint or status
            // message comes and goes.
            if !state.had_focus {
                // Keyboard shortcuts are focus-gated; make the invisible
                // gate visible so a dead key is diagnosable at a glance.
                ui.weak(
                    egui::RichText::new("no keyboard focus — click the timeline")
                        .color(Color32::YELLOW),
                );
            }
            if state.knife_armed {
                ui.weak(
                    egui::RichText::new("Knife: click a clip to cut (bar-snapped) · Esc / right-click to stop")
                        .color(Color32::from_rgb(240, 190, 80)),
                );
            }
            match &state.modal {
                Some(Modal {
                    kind: ModalKind::Grab { time_locked },
                    ..
                }) => {
                    ui.weak(
                        egui::RichText::new(format!(
                            "Move: LMB drop · RMB / Esc cancel{}",
                            if *time_locked {
                                " · X: time lock ON (vertical disabled)"
                            } else {
                                " · X: lock time"
                            }
                        ))
                        .color(Color32::from_rgb(140, 200, 250)),
                    );
                }
                Some(Modal {
                    kind: ModalKind::Duplicate { time_locked },
                    ..
                }) => {
                    ui.weak(
                        egui::RichText::new(format!(
                            "Duplicate: LMB drop the copies · RMB / Esc cancel{}",
                            if *time_locked {
                                " · X: time lock ON (vertical disabled)"
                            } else {
                                " · X: lock time"
                            }
                        ))
                        .color(Color32::from_rgb(140, 200, 250)),
                    );
                }
                Some(Modal {
                    kind: ModalKind::Stretch { start_edge },
                    ..
                }) => {
                    ui.weak(
                        egui::RichText::new(if *start_edge {
                            "Stretch start: LMB drop · RMB / Esc cancel · T: other edge"
                        } else {
                            "Stretch end: LMB drop · RMB / Esc cancel · Shift+T: pull start"
                        })
                        .color(Color32::from_rgb(140, 200, 250)),
                    );
                }
                None => {}
            }
            if let Some(msg) = state.status.take() {
                ui.weak(egui::RichText::new(msg).color(Color32::YELLOW));
            }
            // The static hint goes last and truncates: an overflowing child
            // expands the ui's available rect, and the rows below would be
            // laid out wider than the panel itself.
            ui.add(
                egui::Label::new(
                    egui::RichText::new(
                        "Drag move · edge resize · dbl-click insert/edit · right-click delete · marquee: select · Del: delete · K: knife",
                    )
                    .weak(),
                )
                .truncate(),
            );
        });

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
        // Row geometry registry for the modal ghosts (they can land on any
        // row of the same kind, so they are drawn after all rows have run).
        let mut geom: Vec<(RowKind, Rect)> = Vec::new();
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
                &mut geom,
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
                    &mut geom,
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

        // Modal transform (M4b.3): finalize on the confirming click, cancel
        // on RMB, otherwise draw the ghosts (they may land on any row of
        // the same kind, so they go through the geometry registry).
        if state.modal.is_some() {
            let ptr = ui.ctx().pointer_hover_pos();
            let px_per_bar = state.px_per_bar;
            if state.modal_cancel || state.modal_confirm {
                if state.modal_confirm {
                    if let Some(m) = &state.modal {
                        match m.kind {
                            ModalKind::Grab { .. } | ModalKind::Duplicate { .. } => {
                                if let Some(moves) = grab_targets(
                                    m, ptr, px_per_bar, midi_tracks.len(),
                                    control_tracks.len(), midi_tracks, control_tracks,
                                ) {
                                    let targets: Vec<_> = moves
                                        .into_iter()
                                        .filter_map(|mv| {
                                            let (_, span) = clip_geometry(
                                                midi_tracks, control_tracks, mv.from,
                                            )?;
                                            Some((
                                                mv.from,
                                                (mv.start_ticks, span),
                                                mv.to_track,
                                                mv.from.is_control,
                                            ))
                                        })
                                        .collect();
                                    let (kept, refused) = filter_covered(
                                        targets, midi_tracks, control_tracks,
                                    );
                                    if refused > 0 {
                                        state.status = Some(format!(
                                            "{refused} clip(s) held: target fully covers an existing clip"
                                        ));
                                    }
                                    match m.kind {
                                        ModalKind::Duplicate { .. } => {
                                            if !kept.is_empty() {
                                                actions.push(TimelineAction::DuplicateClipsBatch {
                                                    dups: kept
                                                        .into_iter()
                                                        .map(|(from, (start_ticks, _), to_track, _)| {
                                                            ClipDup { from, to_track, start_ticks }
                                                        })
                                                        .collect(),
                                                });
                                            }
                                        }
                                        _ => {
                                            if !kept.is_empty() {
                                                actions.push(TimelineAction::MoveClipsBatch {
                                                    moves: kept
                                                        .into_iter()
                                                        .map(|(from, (start_ticks, _), to_track, _)| {
                                                            ClipMove { from, to_track, start_ticks }
                                                        })
                                                        .collect(),
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                            ModalKind::Stretch { .. } => {
                                if let Some(spans) = stretch_targets(
                                    m, ptr, px_per_bar, midi_tracks, control_tracks,
                                ) {
                                    let targets: Vec<_> = spans
                                        .iter()
                                        .map(|e| {
                                            (
                                                e.sel,
                                                (e.start_ticks, e.span_ticks),
                                                e.sel.track,
                                                e.sel.is_control,
                                            )
                                        })
                                        .collect();
                                    let (kept, refused) = filter_covered(
                                        targets, midi_tracks, control_tracks,
                                    );
                                    if refused > 0 {
                                        state.status = Some(format!(
                                            "{refused} clip(s) held: target fully covers an existing clip"
                                        ));
                                    }
                                    if !kept.is_empty() {
                                        actions.push(TimelineAction::StretchClipsBatch {
                                            spans: kept
                                                .into_iter()
                                                .map(|(sel, (start_ticks, span_ticks), _, _)| {
                                                    ClipSpanEdit { sel, start_ticks, span_ticks }
                                                })
                                                .collect(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
                state.modal = None;
                state.modal_confirm = false;
                state.modal_cancel = false;
            } else if let Some(m) = &state.modal {
                // Ghost preview: solid blocks at each target (alpha 170),
                // over dimmed originals (the rows already drew them dim).
                let targets = match m.kind {
                    ModalKind::Grab { .. } | ModalKind::Duplicate { .. } => grab_targets(
                        m, ptr, px_per_bar, midi_tracks.len(), control_tracks.len(),
                        midi_tracks, control_tracks,
                    )
                    .map(|ms| {
                        ms.into_iter()
                            .filter_map(|mv| {
                                let (_, span) =
                                    clip_geometry(midi_tracks, control_tracks, mv.from)?;
                                Some((mv.from.is_control, mv.to_track, mv.start_ticks, span))
                            })
                            .collect::<Vec<_>>()
                    }),
                    ModalKind::Stretch { .. } => stretch_targets(
                        m, ptr, px_per_bar, midi_tracks, control_tracks,
                    )
                    .map(|es| {
                        es.into_iter()
                            .map(|e| (e.sel.is_control, e.sel.track, e.start_ticks, e.span_ticks))
                            .collect::<Vec<_>>()
                    }),
                };
                if let Some(targets) = targets {
                    for (is_control, track, start, span) in targets {
                        let Some((_, lane)) = geom
                            .iter()
                            .find(|(row, _)| row.key() == (is_control, track))
                        else {
                            continue;
                        };
                        let x0 = lane.left() + start as f32 * px_per_tick;
                        let x1 = lane.left() + (start + span) as f32 * px_per_tick;
                        let rect = Rect::from_min_max(
                            pos2(x0 + 1.0, lane.top() + 2.0),
                            pos2(x1 - 1.0, lane.bottom() - 2.0),
                        );
                        let ghost = if is_control {
                            Color32::from_rgba_unmultiplied(235, 195, 130, 170)
                        } else {
                            Color32::from_rgba_unmultiplied(140, 180, 240, 170)
                        };
                        let painter = ui.painter_at(*lane);
                        painter.rect_filled(rect, 3.0, ghost);
                    }
                }
            }
        }
    });
    actions
}

/// One row: header cell + clip lane (grid, clips, playhead, interactions).
#[allow(clippy::too_many_arguments)]
fn lane_row<T: Placement>(
    parent: &mut egui::Ui,
    state: &mut TimelineState,
    actions: &mut Vec<TimelineAction>,
    marquee_hits: &mut Vec<(RowKind, Vec<usize>)>,
    geom: &mut Vec<(RowKind, Rect)>,
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
    // The row's widget ids must be STABLE. The lane is the keyboard focus
    // carrier, and egui auto ids are positional: anything above the rows
    // appearing or disappearing (the focus chip, the knife / modal hints,
    // the status message — all drawn after the rows were show()'d this
    // way) shifts every row's id, so the focused lane "disappears" and the
    // focus stays parked on a phantom id forever (egui's dead-man switch
    // grants one frame of grace, which only hides the problem for a frame:
    // `has_focus` reports false, so every focus-gated key is dead and the
    // "no keyboard focus" chip comes back no matter how often you click).
    // `push_id` cannot help (it salts the parent id but still mixes in the
    // positional counter); `UiBuilder::id` gives the row an id that is
    // independent of everything drawn above it.
    //
    // The max_rect must be clamped to ONE row. A raw `new_child` (unlike
    // `ui.horizontal`, which pre-allocates only `interact_size.y`) hands
    // the child the FULL remaining rect, and a Center-aligned horizontal
    // frame then fills its whole available height (`Layout::
    // next_frame_ignore_wrap`): the first row would swallow the entire
    // scroll area and every later row would land below the fold — the
    // "only the last track remains" bug.
    let mut row_rect = parent.available_rect_before_wrap();
    row_rect.max.y = row_rect.min.y + ROW_HEIGHT;
    let mut row_ui = parent.new_child(
        egui::UiBuilder::new()
            // Same layout as `ui.horizontal`, but with an explicit id.
            .layout(egui::Layout::left_to_right(egui::Align::Center))
            .id(egui::Id::new("resoz_timeline_row").with(row.key()))
            .max_rect(row_rect),
    );
    let ui = &mut row_ui;
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
            let mut btn =
                egui::Button::new(egui::RichText::new(format!("Curve {}", index + 1)).size(11.0));
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
    geom.push((row, lane));

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
        // Modal convention (M4a.5): originals dim while the ghost is
        // live, so the preview stays visible where they overlap.
        let selected = state.selection.contains(&ClipSel {
            is_control,
            track,
            clip: index,
        });
        let fill = if selected && state.modal.is_some() {
            Color32::from_rgba_unmultiplied(
                colors.clip_fill.r(),
                colors.clip_fill.g(),
                colors.clip_fill.b(),
                60,
            )
        } else {
            colors.clip_fill
        };
        painter.rect_filled(rect, 3.0, fill);
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

    // Knife (K armed): an LMB click cuts the clip under the pointer at
    // the bar-snapped position; the knife stays armed for more cuts.
    if state.knife_armed && state.modal.is_none() && response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if clips.len() >= MAX_CLIPS_PER_TRACK {
                state.status = Some("this track is full — cannot split".to_string());
            } else {
                match knife_target(clips, tick_at(pos.x)) {
                    Some((index, at)) => {
                        actions.push(TimelineAction::SplitClip {
                            is_control,
                            track,
                            clip: index,
                            at_ticks: at,
                        });
                        state.selection.retain(|s| {
                            s.is_control != is_control || s.track != track || s.clip != index
                        });
                    }
                    None if hit_clip(clips, tick_at(pos.x)).is_some() => {
                        state.status =
                            Some("a cut needs at least one bar on both sides".to_string());
                    }
                    None => {}
                }
            }
        }
    }

    // Knife preview: while armed, mark where the cut would land (the
    // snapped bar line through the hovered lane).
    if state.knife_armed && response.hovered() {
        if let Some(pos) = response.hover_pos() {
            let at = snap_bar(tick_at(pos.x));
            let x = lane.left() + at as f32 * px_per_tick;
            painter.line_segment(
                [pos2(x, lane.top()), pos2(x, lane.bottom())],
                Stroke::new(1.5, Color32::from_rgb(240, 190, 80)),
            );
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
    }

    // Double-click: open the clip's editor, or insert a new clip on empty
    // space. Not while a modal owns the pointer.
    if response.double_clicked() && state.modal.is_none() {
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
    // start a rubber band (M4b selection). While the knife is armed or
    // a modal is live the pointer belongs to them: no drags, no bands.
    if response.drag_started() && !state.knife_armed && state.modal.is_none() {
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
                let edge_x =
                    lane.left() + (clips[index].start() + clips[index].span()) as f32 * px_per_tick;
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
                Rect::from_min_max(pos2(x0, lane.top()), pos2(x1, lane.bottom())).intersects(band)
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

    // A plain click (press + release without moving past the click
    // threshold) picks the clip under the pointer. egui only reports
    // `drag_started` once the pointer MOVED, so without this a click
    // selects nothing and every command key (G / T / Shift+D) just
    // flashes "nothing selected" — the roll hit the same trap. Clicking
    // empty space deselects; a modifier click on empty space leaves the
    // selection alone.
    if response.clicked() && !state.knife_armed && state.modal.is_none() {
        if let Some(pos) = response.interact_pointer_pos() {
            let additive = ui.ctx().input(|i| i.modifiers.shift || i.modifiers.ctrl);
            match hit_clip(clips, tick_at(pos.x)) {
                Some(index) => {
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
                }
                None if additive => {}
                None => state.selection.clear(),
            }
        }
    }

    // Modal pointer handling: LMB click confirms, RMB cancels (and
    // suppresses delete while the modal is live).
    if state.modal.is_some() {
        if response.clicked() {
            state.modal_confirm = true;
        }
        if response.secondary_clicked() {
            state.modal_cancel = true;
        }
    }

    // Right-click: cancel a live band, disarm the knife, otherwise
    // delete the clip under the pointer.
    if response.secondary_clicked() && state.modal.is_none() {
        if state.knife_armed {
            state.knife_armed = false;
        } else if let Some(pos) = response.interact_pointer_pos() {
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
                    // egui's arrow-key focus navigation is the same trap
                    // the roll hit in M4a.4: unclaimed directional keys
                    // hand focus to whatever widget points that way.
                    horizontal_arrows: true,
                    vertical_arrows: true,
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
        let knife = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::K));
        // Shift+T must be consumed before plain T (consume_key ignores
        // the shift qualifier when matching).
        let shift_t = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::T));
        let t = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::T));
        let g = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::G));
        // Shift+D must be consumed before plain D would ever match
        // (consume_key ignores the shift qualifier when matching).
        let shift_d = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::D));
        let x = ui
            .ctx()
            .input_mut(|i| i.consume_key(Modifiers::NONE, Key::X));
        if knife {
            state.knife_armed = !state.knife_armed;
            state.modal = None;
        }
        // X toggles the time lock while a Grab / Duplicate modal is live.
        if x {
            if let Some(modal) = &mut state.modal {
                match &mut modal.kind {
                    ModalKind::Grab { time_locked } | ModalKind::Duplicate { time_locked } => {
                        *time_locked = !*time_locked;
                    }
                    ModalKind::Stretch { .. } => {}
                }
            }
        }
        // Arm G / Shift+D / T (Blender-style modal; arming one tool
        // clears the other).
        if (g || shift_d || t || shift_t) && state.modal.is_none() {
            state.knife_armed = false;
            let arm = if g {
                Arm::Grab
            } else if shift_d {
                Arm::Duplicate
            } else {
                Arm::Stretch {
                    start_edge: shift_t,
                }
            };
            arm_modal(state, ui.ctx(), arm);
        }
        if escape {
            if state.modal.is_some() {
                state.modal = None;
            } else {
                state.selection.clear();
                state.marquee = None;
                state.marquee_ending = false;
                state.knife_armed = false;
            }
        }
        if delete && state.modal.is_none() && !state.selection.is_empty() {
            actions.push(TimelineAction::RemoveClipsBatch {
                clips: state.selection.clone(),
            });
            state.selection.clear();
        }
    }
    // `new_child` does not allocate any space in the parent (egui 0.36
    // docs) — `ui.horizontal` reserves its child's min_rect via
    // `scope_dyn`; a raw `new_child` must hand the parent its extent
    // explicitly or every row lays out at the parent's unmoved cursor.
    parent.advance_cursor_after_rect(row_ui.min_rect());
}

/// Which modal G / Shift+D / T arms (M4b.3/4).
enum Arm {
    Grab,
    Duplicate,
    Stretch { start_edge: bool },
}

/// Arms a modal from the current selection. G and Shift+D require a
/// single track kind (cross-kind moves are meaningless); all need a
/// non-empty selection and a pointer position to grab from.
fn arm_modal(state: &mut TimelineState, ctx: &egui::Context, arm: Arm) {
    if state.selection.is_empty() {
        state.status =
            Some("nothing selected \u{2014} G / Shift+D / T need a selection".to_string());
        return;
    }
    let Some(ptr) = ctx.pointer_hover_pos() else {
        return;
    };
    let kind = match arm {
        Arm::Grab => {
            let first = state.selection[0].is_control;
            if state.selection.iter().any(|s| s.is_control != first) {
                state.status =
                    Some("mixed selection: G moves one track kind at a time".to_string());
                return;
            }
            ModalKind::Grab { time_locked: false }
        }
        Arm::Duplicate => {
            let first = state.selection[0].is_control;
            if state.selection.iter().any(|s| s.is_control != first) {
                state.status =
                    Some("mixed selection: Shift+D copies one track kind at a time".to_string());
                return;
            }
            ModalKind::Duplicate { time_locked: false }
        }
        Arm::Stretch { start_edge } => ModalKind::Stretch { start_edge },
    };
    state.modal = Some(Modal {
        kind,
        origin: state.selection.clone(),
        grab: ptr,
    });
}

fn hit_clip<T: Placement>(clips: &[T], tick: f32) -> Option<usize> {
    clips
        .iter()
        .position(|c| tick >= c.start() as f32 && tick < (c.start() + c.span()) as f32)
}

/// Knife targeting (M4b.2): resolve a click at `tick` into the clip index
/// and a bar-snapped cut position, requiring at least one bar of clip on
/// both sides of the cut. `None` when the click misses every clip or the
/// snapped position is too close to either edge.
fn knife_target<T: Placement>(clips: &[T], tick: f32) -> Option<(usize, u32)> {
    let index = hit_clip(clips, tick)?;
    let clip = &clips[index];
    let at = snap_bar(tick);
    let left_ok = at >= clip.start() + TICKS_PER_BAR;
    let right_ok = at + TICKS_PER_BAR <= clip.start() + clip.span();
    (left_ok && right_ok).then_some((index, at))
}

/// Snaps to the nearest whole bar (0 stays 0).
fn snap_bar(ticks: f32) -> u32 {
    let bar = TICKS_PER_BAR as f32;
    ((ticks / bar).round().max(0.0) as u32) * TICKS_PER_BAR
}

#[cfg(test)]
mod tests {
    use super::*;
    use resoz_model::Sequence;

    fn clip(start: u32, span: u32) -> MidiClip {
        MidiClip {
            start_ticks: start,
            span_ticks: span,
            ..MidiClip::new(start)
        }
    }

    #[test]
    fn knife_targets_the_bar_line_under_the_pointer() {
        let clips = vec![clip(0, 8 * TICKS_PER_BAR)];
        // 2.6 bars in → snaps to bar 3.
        let tick = 2.6 * TICKS_PER_BAR as f32;
        assert_eq!(
            knife_target(&clips, tick),
            Some((0, 3 * TICKS_PER_BAR)),
            "the cut lands on the nearest bar line"
        );
    }

    #[test]
    fn knife_rejects_cuts_without_a_bar_on_both_sides() {
        let clips = vec![clip(0, 2 * TICKS_PER_BAR)];
        // Middle of a 2-bar clip snaps to bar 1 — valid.
        assert_eq!(
            knife_target(&clips, TICKS_PER_BAR as f32),
            Some((0, TICKS_PER_BAR))
        );
        // Near the start edge: snap lands on bar 0 → zero-length left half.
        assert_eq!(knife_target(&clips, 0.2 * TICKS_PER_BAR as f32), None);
        // Near the end edge: snap lands on the clip's end → zero right half.
        assert_eq!(knife_target(&clips, 1.8 * TICKS_PER_BAR as f32), None);
        // One-bar clips can never be split.
        assert_eq!(
            knife_target(
                &[clip(4 * TICKS_PER_BAR, TICKS_PER_BAR)],
                4.4 * TICKS_PER_BAR as f32
            ),
            None
        );
    }

    #[test]
    fn knife_misses_empty_lane() {
        let clips = vec![clip(0, 4 * TICKS_PER_BAR)];
        assert_eq!(knife_target(&clips, 6.0 * TICKS_PER_BAR as f32), None);
    }

    #[test]
    fn split_halves_keep_start_end_and_content() {
        let mut clips = vec![MidiClip {
            start_ticks: 2 * TICKS_PER_BAR,
            span_ticks: 4 * TICKS_PER_BAR,
            ..MidiClip::new(2 * TICKS_PER_BAR)
        }];
        let original = clips[0].clone();
        let at = 4 * TICKS_PER_BAR;
        assert!(split_clip_in(&mut clips, at, 0));
        assert_eq!(clips.len(), 2);
        // Left: same start, span ends at the cut.
        assert_eq!(clips[0].start_ticks, 2 * TICKS_PER_BAR);
        assert_eq!(clips[0].span_ticks, 2 * TICKS_PER_BAR);
        // Right: starts at the cut, runs to the old end, same loop/content.
        assert_eq!(clips[1].start_ticks, at);
        assert_eq!(clips[1].span_ticks, 2 * TICKS_PER_BAR);
        assert_eq!(clips[1].loop_ticks, original.loop_ticks);
        assert_eq!(clips[1].sequence, original.sequence);
    }

    #[test]
    fn split_refuses_edges_and_out_of_bounds() {
        let mut clips = vec![clip(0, 2 * TICKS_PER_BAR)];
        assert!(!split_clip_in(&mut clips, 0, 0), "at the start");
        assert!(
            !split_clip_in(&mut clips, 2 * TICKS_PER_BAR, 0),
            "at the end"
        );
        assert!(!split_clip_in(&mut clips, TICKS_PER_BAR, 5), "bad index");
        assert_eq!(clips.len(), 1, "failed splits must not mutate");
    }

    #[test]
    fn split_respects_the_per_track_cap() {
        let mut clips: Vec<MidiClip> = (0..MAX_CLIPS_PER_TRACK)
            .map(|i| clip(i as u32 * 2 * TICKS_PER_BAR, 2 * TICKS_PER_BAR))
            .collect();
        assert!(!split_clip_in(&mut clips, TICKS_PER_BAR, 0));
        assert_eq!(clips.len(), MAX_CLIPS_PER_TRACK);
    }

    #[test]
    fn row_delta_clamps_globally_not_per_clip() {
        // Selection on rows 0 and 1 of three rows: delta +2 must clamp to
        // +1 so the whole group fits, not push row 1 out.
        let sel = vec![
            ClipSel {
                is_control: false,
                track: 0,
                clip: 0,
            },
            ClipSel {
                is_control: false,
                track: 1,
                clip: 0,
            },
        ];
        assert_eq!(clamp_row_delta(2, &sel, 3), 1);
        // Track 0 anchors the group: no downward move at all.
        assert_eq!(clamp_row_delta(-5, &sel, 3), 0);
        // Selection on rows 1 and 2 can move down by one, not further.
        let low = vec![
            ClipSel {
                is_control: false,
                track: 1,
                clip: 0,
            },
            ClipSel {
                is_control: false,
                track: 2,
                clip: 0,
            },
        ];
        assert_eq!(clamp_row_delta(-1, &low, 3), -1);
        assert_eq!(clamp_row_delta(-5, &low, 3), -1);
    }

    #[test]
    fn full_coverage_is_refused_but_crossing_is_not() {
        // Target 0..8 fully covers 2..4: refused.
        assert!(fully_covers((0, 8), &[(2, 2)]));
        // Target 0..4 crosses 2..6: partial overlap, allowed.
        assert!(!fully_covers((0, 4), &[(2, 4)]));
        // Touching edges are not coverage.
        assert!(!fully_covers((0, 4), &[(4, 2)]));
    }

    #[test]
    fn grab_moves_by_bars_and_rows_within_kind() {
        let midi = vec![MidiTrack {
            clips: vec![clip(2 * TICKS_PER_BAR, 2 * TICKS_PER_BAR)],
            ..MidiTrack::default()
        }];
        let modal = Modal {
            kind: ModalKind::Grab { time_locked: false },
            origin: vec![ClipSel {
                is_control: false,
                track: 0,
                clip: 0,
            }],
            grab: pos2(100.0, 100.0),
        };
        // Pointer 1.6 bars right, one row down.
        let ptr = pos2(100.0 + 1.6 * 56.0, 100.0 + ROW_HEIGHT);
        let targets = grab_targets(&modal, Some(ptr), 56.0, 2, 1, &midi, &[]).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].to_track, 1);
        // 1.6 bars → rounds to 2 bars.
        assert_eq!(targets[0].start_ticks, 4 * TICKS_PER_BAR);
        // Time lock kills the vertical delta only.
        let locked = Modal {
            kind: ModalKind::Grab { time_locked: true },
            ..modal.clone()
        };
        let targets = grab_targets(&locked, Some(ptr), 56.0, 2, 1, &midi, &[]).unwrap();
        assert_eq!(targets[0].to_track, 0);
        assert_eq!(targets[0].start_ticks, 4 * TICKS_PER_BAR);
    }

    #[test]
    fn stretch_keeps_one_bar_minimum() {
        let midi = vec![MidiTrack {
            clips: vec![clip(2 * TICKS_PER_BAR, 2 * TICKS_PER_BAR)],
            ..MidiTrack::default()
        }];
        let grab = pos2(0.0, 0.0);
        // End edge, pointer 3 bars left: span would go negative → clamps
        // to one bar.
        let end = Modal {
            kind: ModalKind::Stretch { start_edge: false },
            origin: vec![ClipSel {
                is_control: false,
                track: 0,
                clip: 0,
            }],
            grab,
        };
        let targets =
            stretch_targets(&end, Some(pos2(-3.0 * 56.0, 0.0)), 56.0, &midi, &[]).unwrap();
        assert_eq!(targets[0].span_ticks, TICKS_PER_BAR);
        assert_eq!(targets[0].start_ticks, 2 * TICKS_PER_BAR);
        // Start edge, pointer 3 bars right: start would pass the end →
        // clamps to one bar remaining.
        let start = Modal {
            kind: ModalKind::Stretch { start_edge: true },
            origin: vec![ClipSel {
                is_control: false,
                track: 0,
                clip: 0,
            }],
            grab,
        };
        let targets =
            stretch_targets(&start, Some(pos2(3.0 * 56.0, 0.0)), 56.0, &midi, &[]).unwrap();
        assert_eq!(targets[0].start_ticks, 3 * TICKS_PER_BAR);
        assert_eq!(targets[0].span_ticks, TICKS_PER_BAR);
    }
}
