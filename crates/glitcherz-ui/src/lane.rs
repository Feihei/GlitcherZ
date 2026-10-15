//! Automation lane editor (egui self-drawn).
//!
//! Interaction conventions match the piano roll: left-click on empty space to add an anchor
//! (and drag it right away), drag an anchor to move it (time snaps horizontally, parameter
//! value vertically), right-click to delete the nearest anchor. The vertical axis maps to the
//! parameter's real value range (log axis for filter cutoff), and the curve is linearly
//! interpolated between anchors.
//!
//! Like the piano roll, this module is a view: it reads the clip and returns
//! [`LaneIntent`]s; the caller applies them undoably and feeds the new drag
//! index back via [`LaneState::set_drag`]. Dragging emits one intent per frame
//! and is meant to run through a merged undo gesture.

use eframe::egui;

use glitcherz_model::{AutoParam, ControlClip, TICKS_PER_STEP};

/// Lane height (pixels).
pub const LANE_HEIGHT: f32 = 96.0;

/// Time snap granularity (tick) — a 1/32 note (240 ticks at 960 PPQ).
///
/// (Was 12 at 96 PPQ; missed in the M1.1 rescale because nothing here derived
/// from `TICKS_PER_STEP`.)
pub const SNAP_TICKS: u32 = glitcherz_model::TICKS_PER_STEP / 2;

/// An edit the lane wants performed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LaneIntent {
    /// Insert or move an anchor (one frame of a drag included).
    UpsertPoint { tick: u32, value: f32 },
    /// Delete the anchor nearest to `tick`.
    RemoveNear { tick: u32 },
}

/// Cross-frame drag state (current index of the dragged anchor).
#[derive(Debug, Default)]
pub struct LaneState {
    drag: Option<usize>,
}

impl LaneState {
    /// Re-resolves the drag target after an intent was applied: upserting can
    /// reorder the curve, so the index the caller returns is authoritative.
    pub fn set_drag(&mut self, index: Option<usize>) {
        self.drag = index;
    }

    /// Whether a drag gesture is in progress.
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Drops any drag in progress (before undo/redo).
    pub fn clear_drag(&mut self) {
        self.drag = None;
    }
}

/// Parameter axis metadata: (min, max, logarithmic?).
fn axis(param: &AutoParam) -> (f32, f32, bool) {
    match param {
        AutoParam::TrackGain => (0.0, 1.5, false),
        AutoParam::TrackPan => (-1.0, 1.0, false),
        AutoParam::FilterCutoff => (20.0, 18_000.0, true),
        // Plugin parameter range unknown: display normalized 0..1 (interpreted by the plugin).
        AutoParam::PluginParam { .. } => (0.0, 1.0, false),
    }
}

/// Real value → 0..1 normalization.
fn to_norm(value: f32, min: f32, max: f32, log: bool) -> f32 {
    let norm = if log {
        let (lo, hi) = (f64::from(min).log10(), f64::from(max).log10());
        ((f64::from(value).log10() - lo) / (hi - lo)) as f32
    } else {
        (value - min) / (max - min)
    };
    norm.clamp(0.0, 1.0)
}

/// 0..1 normalization → real value.
fn from_norm(norm: f32, min: f32, max: f32, log: bool) -> f32 {
    let norm = norm.clamp(0.0, 1.0);
    if log {
        let (lo, hi) = (f64::from(min).log10(), f64::from(max).log10());
        10f64.powf(lo + (hi - lo) * f64::from(norm)) as f32
    } else {
        min + (max - min) * norm
    }
}

/// Draw and interact one frame of the lane. Modifies `clip` in place and returns whether an edit
/// happened this frame (the caller then re-pushes the curve to the audio thread).
///
/// `param` is the cable endpoint parameter (D7: the curve does not carry a target), used only
/// for vertical axis range mapping.
pub fn show(
    ui: &mut egui::Ui,
    clip: &ControlClip,
    param: &AutoParam,
    loop_ticks: u32,
    playhead_ticks: f32,
    playing: bool,
    state: &mut LaneState,
) -> Option<LaneIntent> {
    let loop_ticks = loop_ticks.max(TICKS_PER_STEP).max(1);
    let (min, max, log) = axis(param);
    let mut intent: Option<LaneIntent> = None;

    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, LANE_HEIGHT),
        egui::Sense::click_and_drag(),
    );
    let ticks_to_x = width / loop_ticks as f32;
    let painter = ui.painter_at(rect);
    let value_y = |value: f32| rect.bottom() - to_norm(value, min, max, log) * LANE_HEIGHT;
    let y_value = |y: f32| from_norm((rect.bottom() - y) / LANE_HEIGHT, min, max, log);

    // Background + normalized midline.
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(22));
    let mid = from_norm(0.5, min, max, log);
    painter.line_segment(
        [
            egui::pos2(rect.left(), value_y(mid)),
            egui::pos2(rect.right(), value_y(mid)),
        ],
        egui::Stroke::new(1.0, egui::Color32::from_gray(45)),
    );

    // Curve segments (lines between adjacent anchors).
    let curve_color = egui::Color32::from_rgb(90, 160, 220);
    if clip.curve.len() >= 2 {
        for window in clip.curve.windows(2) {
            painter.line_segment(
                [
                    egui::pos2(
                        rect.left() + window[0].tick as f32 * ticks_to_x,
                        value_y(window[0].value),
                    ),
                    egui::pos2(
                        rect.left() + window[1].tick as f32 * ticks_to_x,
                        value_y(window[1].value),
                    ),
                ],
                egui::Stroke::new(1.5, curve_color),
            );
        }
    }

    // Anchors.
    for (index, point) in clip.curve.iter().enumerate() {
        let x = rect.left() + point.tick as f32 * ticks_to_x;
        let y = value_y(point.value);
        let color = if state.drag == Some(index) {
            egui::Color32::WHITE
        } else {
            egui::Color32::from_rgb(120, 180, 240)
        };
        painter.circle_filled(egui::pos2(x, y), 4.0, color);
    }

    // Playhead.
    if playing {
        let pos = playhead_ticks.rem_euclid(loop_ticks as f32);
        let x = rect.left() + pos * ticks_to_x;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(2.0, egui::Color32::YELLOW),
        );
    }

    // Interaction.
    let hit_point = |pos: egui::Pos2, clip: &ControlClip| -> Option<usize> {
        clip.curve.iter().position(|p| {
            let x = rect.left() + p.tick as f32 * ticks_to_x;
            let y = value_y(p.value);
            (pos.x - x).abs() <= 6.0 && (pos.y - y).abs() <= 8.0
        })
    };
    let snap = |ticks: f32| -> u32 { (ticks / SNAP_TICKS as f32).round() as u32 * SNAP_TICKS };

    if response.drag_started() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(index) = hit_point(pos, clip) {
                state.drag = Some(index);
            } else {
                let tick = snap((pos.x - rect.left()) / ticks_to_x).min(loop_ticks - 1);
                intent = Some(LaneIntent::UpsertPoint {
                    tick,
                    value: y_value(pos.y),
                });
            }
        }
    }

    if response.dragged() {
        if let (Some(pos), Some(index)) = (response.interact_pointer_pos(), state.drag) {
            if index < clip.curve.len() {
                intent = Some(LaneIntent::UpsertPoint {
                    tick: snap((pos.x - rect.left()) / ticks_to_x).min(loop_ticks - 1),
                    value: y_value(pos.y),
                });
            }
        }
    }

    if response.drag_stopped() {
        state.drag = None;
    }

    if response.secondary_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let tick = ((pos.x - rect.left()) / ticks_to_x).max(0.0) as u32;
            intent = Some(LaneIntent::RemoveNear { tick });
        }
    }

    intent
}

#[cfg(test)]
mod tests {
    use super::{from_norm, to_norm};
    use glitcherz_model::AutoParam;

    #[test]
    fn axis_roundtrip_linear() {
        let (min, max, log) = super::axis(&AutoParam::TrackGain);
        assert!(!log);
        let norm = to_norm(0.75, min, max, log);
        assert!((from_norm(norm, min, max, log) - 0.75).abs() < 1e-6);
    }

    #[test]
    fn axis_roundtrip_log() {
        let (min, max, log) = super::axis(&AutoParam::FilterCutoff);
        assert!(log);
        let norm = to_norm(1_200.0, min, max, log);
        assert!((from_norm(norm, min, max, log) - 1_200.0).abs() < 1.0);
    }

    #[test]
    fn values_clamped_to_axis() {
        let (min, max, log) = super::axis(&AutoParam::TrackPan);
        assert!(to_norm(-99.0, min, max, log) <= 0.0);
        assert!(to_norm(99.0, min, max, log) >= 1.0);
    }
}
