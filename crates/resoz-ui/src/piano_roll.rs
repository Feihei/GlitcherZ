//! 钢琴卷帘编辑器组件（egui 自绘）。
//!
//! 交互约定：
//! - 左键点空白处：以吸附网格创建音符，并可立即拖拽移动；
//! - 左键按住已有音符拖拽：移动（起始位置 + 音高）；
//! - 右键：删除指针下的音符；
//! - 鼠标滚轮：垂直滚动音高视野。
//!
//! 时间轴固定"整段循环铺满可用宽度"（fit-to-width），暂无缩放；
//! 时间吸附粒度为 [`SNAP_TICKS`]。播放头由引擎回传的音乐位置驱动。

use eframe::egui;

use resoz_model::{Clip, Note, TICKS_PER_QUARTER, TICKS_PER_STEP};

/// 每个半音行的高度（像素）。
pub const ROW_HEIGHT: f32 = 9.0;

/// 卷帘区域高度（像素，32 个半音行）。
pub const ROLL_HEIGHT: f32 = ROW_HEIGHT * 32.0;

/// 可见行数。
const VISIBLE_ROWS: i32 = (ROLL_HEIGHT / ROW_HEIGHT) as i32;

/// 时间吸附粒度（tick）——1/64 音符。
pub const SNAP_TICKS: u32 = 6;

/// 新建音符的默认时长（tick）——一个 16 分音符。
const DEFAULT_NOTE_LENGTH: u32 = TICKS_PER_STEP;

/// 跨帧持久的状态（滚动位置 + 拖拽中的音符）。
#[derive(Debug, Default)]
pub struct RollState {
    /// 顶部可见的 MIDI 音符编号（f32 便于平滑滚动）。
    pub top_note: f32,
    /// 正在拖拽的音符（片段内索引 + 抓取点相对音符起点的时间偏移）。
    drag: Option<DragNote>,
}

#[derive(Debug)]
struct DragNote {
    index: usize,
    grab_offset_ticks: f32,
}

impl RollState {
    /// 初始视野顶部为 C6（84），向下展示约两个半八度。
    pub fn new() -> Self {
        Self {
            top_note: 84.0,
            drag: None,
        }
    }
}

fn is_black_key(note: i32) -> bool {
    matches!(note.rem_euclid(12), 1 | 3 | 6 | 8 | 10)
}

/// 绘制并交互一帧卷帘。直接原地修改 `clip`，返回本帧是否发生编辑
/// （调用方据此把片段重新下发到音频线程）。
pub fn show(
    ui: &mut egui::Ui,
    clip: &mut Clip,
    state: &mut RollState,
    playhead_ticks: f32,
    playing: bool,
) -> bool {
    let loop_ticks = clip.loop_ticks.max(TICKS_PER_STEP).max(1);
    let mut edited = false;

    // 工具条：滚动 + 提示。
    ui.horizontal(|ui| {
        if ui.button("▲").clicked() {
            state.top_note = (state.top_note + 12.0).min(127.0);
        }
        if ui.button("▼").clicked() {
            state.top_note = (state.top_note - 12.0).max(VISIBLE_ROWS as f32);
        }
        ui.weak("左键绘制/拖拽 · 右键删除 · 滚轮滚动音高");
    });

    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, ROLL_HEIGHT),
        egui::Sense::click_and_drag(),
    );

    // 滚轮：垂直滚动音高视野。
    if response.hovered() {
        let scroll = ui.ctx().input(|i| i.smooth_scroll_delta().y);
        if scroll != 0.0 {
            state.top_note =
                (state.top_note + scroll / ROW_HEIGHT).clamp(VISIBLE_ROWS as f32 - 1.0, 127.0);
            ui.ctx().request_repaint();
        }
    }

    let ticks_to_x = width / loop_ticks as f32;
    let painter = ui.painter_at(rect);
    let top_note = state.top_note.round() as i32;
    let min_note = top_note - VISIBLE_ROWS + 1;

    // 1. 行背景（黑键更暗）。
    for note in min_note..=top_note {
        if !(0..=127).contains(&note) {
            continue;
        }
        let y = rect.top() + (top_note - note) as f32 * ROW_HEIGHT;
        let color = if is_black_key(note) {
            egui::Color32::from_gray(26)
        } else {
            egui::Color32::from_gray(40)
        };
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(rect.left(), y), egui::vec2(width, ROW_HEIGHT)),
            0.0,
            color,
        );
    }

    // 2. 垂直网格：每步细线，每拍粗线。
    let mut tick = 0u32;
    while tick <= loop_ticks {
        let x = rect.left() + tick as f32 * ticks_to_x;
        let is_beat = tick % TICKS_PER_QUARTER == 0;
        let stroke = egui::Stroke::new(
            if is_beat { 1.5 } else { 1.0 },
            if is_beat {
                egui::Color32::from_gray(95)
            } else {
                egui::Color32::from_gray(60)
            },
        );
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            stroke,
        );
        tick += TICKS_PER_STEP;
    }

    // 3. 音符块。
    for (index, note) in clip.notes.iter().enumerate() {
        let note_i = i32::from(note.note);
        if !(min_note..=top_note).contains(&note_i) {
            continue;
        }
        let y = rect.top() + (top_note as f32 - note.note as f32) * ROW_HEIGHT;
        let x = rect.left() + note.start as f32 * ticks_to_x;
        let w = (note.length as f32 * ticks_to_x).max(3.0);
        let dragging = state.drag.as_ref().is_some_and(|d| d.index == index);
        let fill = if dragging {
            egui::Color32::from_rgb(120, 200, 140)
        } else {
            egui::Color32::from_rgb(64, 160, 96)
        };
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x, y + 1.0),
                egui::pos2(x + w, y + ROW_HEIGHT - 1.0),
            ),
            2.0,
            fill,
        );
    }

    // 4. 播放头（音乐位置对循环长度取模）。
    if playing {
        let pos = playhead_ticks.rem_euclid(loop_ticks as f32);
        let x = rect.left() + pos * ticks_to_x;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(2.0, egui::Color32::YELLOW),
        );
    }

    // 5. 交互。
    let pointer_tick = |pos: egui::Pos2| ((pos.x - rect.left()) / ticks_to_x).max(0.0);
    let pointer_note = |pos: egui::Pos2| {
        (top_note as f32 - (pos.y - rect.top()) / ROW_HEIGHT)
            .round()
            .clamp(0.0, 127.0) as u8
    };
    let hit_note = |pos: egui::Pos2, clip: &Clip| -> Option<usize> {
        let tick = pointer_tick(pos) as u32;
        let note = pointer_note(pos);
        clip.notes
            .iter()
            .position(|n| n.note == note && tick >= n.start && tick < n.start + n.length)
    };
    let snap = |ticks: f32| -> u32 { (ticks / SNAP_TICKS as f32).round() as u32 * SNAP_TICKS };

    if response.drag_started() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(index) = hit_note(pos, clip) {
                let grab_offset = pointer_tick(pos) - clip.notes[index].start as f32;
                state.drag = Some(DragNote {
                    index,
                    grab_offset_ticks: grab_offset,
                });
            } else {
                // 空白处按下：创建音符并进入拖拽。
                let start = snap(pointer_tick(pos)).min(loop_ticks - 1);
                clip.notes.push(Note {
                    start,
                    length: DEFAULT_NOTE_LENGTH,
                    note: pointer_note(pos),
                    velocity: 0.9,
                });
                state.drag = Some(DragNote {
                    index: clip.notes.len() - 1,
                    grab_offset_ticks: 0.0,
                });
                edited = true;
            }
        }
    }

    if response.dragged() {
        if let (Some(pos), Some(drag)) = (response.interact_pointer_pos(), &state.drag) {
            if let Some(note) = clip.notes.get_mut(drag.index) {
                let start = snap(pointer_tick(pos) - drag.grab_offset_ticks)
                    .min(loop_ticks.saturating_sub(1));
                if note.start != start || note.note != pointer_note(pos) {
                    note.start = start;
                    note.note = pointer_note(pos);
                    edited = true;
                }
            }
        }
    }

    if response.drag_stopped() {
        state.drag = None;
    }

    if response.secondary_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(index) = hit_note(pos, clip) {
                clip.notes.remove(index);
                state.drag = None;
                edited = true;
            }
        }
    }

    edited
}

#[cfg(test)]
mod tests {
    use super::{RollState, is_black_key};

    #[test]
    fn black_key_detection() {
        assert!(!is_black_key(60)); // C4
        assert!(is_black_key(61)); // C#4
        assert!(is_black_key(63)); // D#4
        assert!(!is_black_key(64)); // E4
        assert!(is_black_key(70)); // A#4
        assert!(!is_black_key(0));
        assert!(is_black_key(-4)); // -4 → 音级 8（G#），且 rem_euclid 不 panic
    }

    #[test]
    fn roll_state_defaults() {
        let state = RollState::new();
        assert!((state.top_note - 84.0).abs() < 1e-6, "初始视野顶部为 C6");
        assert!(state.drag.is_none());
    }
}
