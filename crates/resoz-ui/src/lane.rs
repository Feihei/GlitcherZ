//! 自动化泳道编辑器（egui 自绘）。
//!
//! 交互约定与钢琴卷帘一致：左键点空白加锚点（可顺手拖拽）、按住锚点
//! 拖拽移动（横向时间吸附、纵向参数值）、右键删除最近锚点。纵轴按
//! 参数真实值域映射（滤波截止用对数轴），曲线在锚点间线性插值。

use eframe::egui;

use resoz_model::{AutoParam, AutoPoint, Automation, TICKS_PER_STEP};

/// 泳道高度（像素）。
pub const LANE_HEIGHT: f32 = 96.0;

/// 时间吸附粒度（tick）——1/32 音符。
pub const SNAP_TICKS: u32 = 12;

/// 跨帧拖拽状态（被拖拽锚点的当前下标）。
#[derive(Debug, Default)]
pub struct LaneState {
    drag: Option<usize>,
}

/// 参数轴元数据：(最小值, 最大值, 是否对数)。
fn axis(param: AutoParam) -> (f32, f32, bool) {
    match param {
        AutoParam::TrackGain => (0.0, 1.5, false),
        AutoParam::TrackPan => (-1.0, 1.0, false),
        AutoParam::FilterCutoff => (20.0, 18_000.0, true),
    }
}

/// 真实值 → 0..1 归一化。
fn to_norm(value: f32, min: f32, max: f32, log: bool) -> f32 {
    let norm = if log {
        let (lo, hi) = (f64::from(min).log10(), f64::from(max).log10());
        ((f64::from(value).log10() - lo) / (hi - lo)) as f32
    } else {
        (value - min) / (max - min)
    };
    norm.clamp(0.0, 1.0)
}

/// 0..1 归一化 → 真实值。
fn from_norm(norm: f32, min: f32, max: f32, log: bool) -> f32 {
    let norm = norm.clamp(0.0, 1.0);
    if log {
        let (lo, hi) = (f64::from(min).log10(), f64::from(max).log10());
        10f64.powf(lo + (hi - lo) * f64::from(norm)) as f32
    } else {
        min + (max - min) * norm
    }
}

/// 绘制并交互一帧泳道。直接原地修改 `automation`，返回本帧是否编辑
/// （调用方据此把曲线重新下发到音频线程）。
pub fn show(
    ui: &mut egui::Ui,
    automation: &mut Automation,
    loop_ticks: u32,
    playhead_ticks: f32,
    playing: bool,
    state: &mut LaneState,
) -> bool {
    let loop_ticks = loop_ticks.max(TICKS_PER_STEP).max(1);
    let (min, max, log) = axis(automation.param);
    let mut edited = false;

    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, LANE_HEIGHT),
        egui::Sense::click_and_drag(),
    );
    let ticks_to_x = width / loop_ticks as f32;
    let painter = ui.painter_at(rect);
    let value_y = |value: f32| rect.bottom() - to_norm(value, min, max, log) * LANE_HEIGHT;
    let y_value = |y: f32| from_norm((rect.bottom() - y) / LANE_HEIGHT, min, max, log);

    // 背景 + 归一化中线。
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(22));
    let mid = from_norm(0.5, min, max, log);
    painter.line_segment(
        [
            egui::pos2(rect.left(), value_y(mid)),
            egui::pos2(rect.right(), value_y(mid)),
        ],
        egui::Stroke::new(1.0, egui::Color32::from_gray(45)),
    );

    // 曲线段（相邻锚点连线）。
    let curve_color = egui::Color32::from_rgb(90, 160, 220);
    if automation.points.len() >= 2 {
        for window in automation.points.windows(2) {
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

    // 锚点。
    for (index, point) in automation.points.iter().enumerate() {
        let x = rect.left() + point.tick as f32 * ticks_to_x;
        let y = value_y(point.value);
        let color = if state.drag == Some(index) {
            egui::Color32::WHITE
        } else {
            egui::Color32::from_rgb(120, 180, 240)
        };
        painter.circle_filled(egui::pos2(x, y), 4.0, color);
    }

    // 播放头。
    if playing {
        let pos = playhead_ticks.rem_euclid(loop_ticks as f32);
        let x = rect.left() + pos * ticks_to_x;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(2.0, egui::Color32::YELLOW),
        );
    }

    // 交互。
    let hit_point = |pos: egui::Pos2, automation: &Automation| -> Option<usize> {
        automation.points.iter().position(|p| {
            let x = rect.left() + p.tick as f32 * ticks_to_x;
            let y = value_y(p.value);
            (pos.x - x).abs() <= 6.0 && (pos.y - y).abs() <= 8.0
        })
    };
    let snap = |ticks: f32| -> u32 { (ticks / SNAP_TICKS as f32).round() as u32 * SNAP_TICKS };

    if response.drag_started() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(index) = hit_point(pos, automation) {
                state.drag = Some(index);
            } else {
                let tick = snap((pos.x - rect.left()) / ticks_to_x).min(loop_ticks - 1);
                automation.upsert_point(AutoPoint {
                    tick,
                    value: y_value(pos.y),
                });
                state.drag = automation.points.iter().position(|p| p.tick == tick);
                edited = true;
            }
        }
    }

    if response.dragged() {
        if let (Some(pos), Some(index)) = (response.interact_pointer_pos(), state.drag) {
            if index < automation.points.len() {
                // 先移除再 upsert：拖拽可能改变 tick 排序。
                automation.points.remove(index);
                automation.upsert_point(AutoPoint {
                    tick: snap((pos.x - rect.left()) / ticks_to_x).min(loop_ticks - 1),
                    value: y_value(pos.y),
                });
                state.drag = automation
                    .points
                    .iter()
                    .position(|p| p.tick == snap((pos.x - rect.left()) / ticks_to_x));
                edited = true;
            }
        }
    }

    if response.drag_stopped() {
        state.drag = None;
    }

    if response.secondary_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let tick = ((pos.x - rect.left()) / ticks_to_x).max(0.0) as u32;
            if automation.remove_near(tick, SNAP_TICKS / 2) {
                state.drag = None;
                edited = true;
            }
        }
    }

    edited
}

#[cfg(test)]
mod tests {
    use super::{from_norm, to_norm};
    use resoz_model::AutoParam;

    #[test]
    fn axis_roundtrip_linear() {
        let (min, max, log) = super::axis(AutoParam::TrackGain);
        assert!(!log);
        let norm = to_norm(0.75, min, max, log);
        assert!((from_norm(norm, min, max, log) - 0.75).abs() < 1e-6);
    }

    #[test]
    fn axis_roundtrip_log() {
        let (min, max, log) = super::axis(AutoParam::FilterCutoff);
        assert!(log);
        let norm = to_norm(1_200.0, min, max, log);
        assert!((from_norm(norm, min, max, log) - 1_200.0).abs() < 1.0);
    }

    #[test]
    fn values_clamped_to_axis() {
        let (min, max, log) = super::axis(AutoParam::TrackPan);
        assert!(to_norm(-99.0, min, max, log) <= 0.0);
        assert!(to_norm(99.0, min, max, log) >= 1.0);
    }
}
