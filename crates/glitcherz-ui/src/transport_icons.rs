//! Icon-only widgets for the transport bar.
//!
//! Two problems live here, and both come from the same place: an icon button is
//! a glyph in a padded frame, so egui's defaults (8 pt horizontal padding plus
//! 8 pt item spacing) put 24 pt of air between two 14 pt glyphs. The row then
//! reads as a handful of scattered buttons rather than one transport cluster,
//! which is exactly the width the bar is short of.
//!
//! - [`icon_button`] keeps egui's [`egui::Button`] (frame, hover, selection,
//!   accessibility all come for free) and only narrows the horizontal padding.
//! - [`metronome_button`] paints its own glyph. The text faces carry no
//!   metronome — `♩` is a note, not the instrument — and no code point in
//!   Unicode is a mechanical metronome, so the icon is drawn: a tapered case
//!   with the pendulum rod slanting through it.

use eframe::egui;

/// Horizontal padding of an icon button. Wide enough that the click target
/// stays comfortable (22 pt with a 14 pt glyph), narrow enough that a run of
/// six icons stops looking like six separate decisions.
const ICON_PADDING_X: f32 = 4.0;

/// Width floor of an icon button (glyph + 2 × [`ICON_PADDING_X`]), so a run of
/// icons with different glyph advances still steps evenly.
const ICON_MIN_WIDTH: f32 = 22.0;

/// An icon-only transport button: [`egui::Button`] with tightened padding.
///
/// The padding is restored before returning, so callers can interleave other
/// widgets without inheriting the icon metrics.
pub(crate) fn icon_button(ui: &mut egui::Ui, button: egui::Button<'_>) -> egui::Response {
    let previous = ui.spacing().button_padding;
    ui.spacing_mut().button_padding.x = ICON_PADDING_X;
    let response = ui.add(button.min_size(egui::vec2(ICON_MIN_WIDTH, 0.0)));
    ui.spacing_mut().button_padding = previous;
    response
}

/// The metronome toggle — a mechanical metronome drawn by hand.
///
/// Shape and colour are negotiable; the hit area is not: it matches
/// [`icon_button`] so the icon sits in the row like every other button, and it
/// uses `interact_selectable` so hover, press and the "on" fill come from the
/// same style as the neighbouring toggles.
pub(crate) fn metronome_button(ui: &mut egui::Ui, on: bool) -> egui::Response {
    let size = egui::vec2(ICON_MIN_WIDTH, ui.spacing().interact_size.y);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let visuals = ui.style().interact_selectable(&response, on);
    let painter = ui.painter();
    painter.rect(
        rect,
        visuals.corner_radius,
        visuals.bg_fill,
        visuals.bg_stroke,
        egui::StrokeKind::Inside,
    );
    let (case, rod, weight) = metronome_shape(rect.shrink(3.0));
    let stroke = egui::Stroke::new(1.4, visuals.fg_stroke.color);
    // One closed path, so the corners join instead of showing four bald ends.
    painter.line(vec![case[0], case[1], case[2], case[3], case[0]], stroke);
    painter.line_segment(rod, stroke);
    painter.line_segment(weight, stroke);
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// Geometry of the metronome inside `rect`, in drawing order:
/// the four case corners, the pendulum rod, and its weight mark.
///
/// A real metronome is a tapered case (narrow at the top, wide at the bottom)
/// with a rod pivoting near the bottom centre; drawn small, the rod has to be
/// visibly off vertical or the icon reads as a plain bell. Split out so the
/// proportions can be tested without a paint context.
fn metronome_shape(rect: egui::Rect) -> ([egui::Pos2; 4], [egui::Pos2; 2], [egui::Pos2; 2]) {
    let center = rect.center();
    let (width, height) = (rect.width(), rect.height());
    let top_half = 0.17 * width;
    let bottom_half = 0.33 * width;
    let case = [
        egui::pos2(center.x - top_half, rect.top()),
        egui::pos2(center.x + top_half, rect.top()),
        egui::pos2(center.x + bottom_half, rect.bottom()),
        egui::pos2(center.x - bottom_half, rect.bottom()),
    ];

    // Pivot low and slightly left of centre, tip high on the right: one
    // diagonal across the case, stopping short of the top plate so the rod
    // does not look like it pierces the lid.
    let pivot = egui::pos2(center.x - 0.13 * width, rect.bottom() - 0.10 * height);
    let tip = egui::pos2(center.x + 0.10 * width, rect.top() + 0.16 * height);
    let rod = [pivot, tip];

    // Weight: a short bar across the rod, a little above its middle.
    let along = tip - pivot;
    let normal = egui::vec2(-along.y, along.x).normalized() * (0.11 * width);
    let at = pivot + along * 0.55;
    let weight = [at - normal, at + normal];

    (case, rod, weight)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The size the icon actually gets: a 22 pt button with 3 pt of breathing
    /// room on each side.
    fn shape() -> ([egui::Pos2; 4], [egui::Pos2; 2], [egui::Pos2; 2]) {
        metronome_shape(egui::Rect::from_min_max(
            egui::pos2(0.0, 0.0),
            egui::pos2(16.0, 14.0),
        ))
    }

    /// Half-width of the tapered case at height `y`, by linear interpolation
    /// between the top edge and the base.
    fn case_half_width_at(case: &[egui::Pos2; 4], y: f32) -> f32 {
        let top_half = (case[1].x - case[0].x) * 0.5;
        let base_half = (case[2].x - case[3].x) * 0.5;
        let t = (y - case[0].y) / (case[3].y - case[0].y);
        top_half + (base_half - top_half) * t
    }

    #[test]
    fn the_case_tapers_between_a_narrow_top_and_a_wide_base() {
        let (case, _, _) = shape();
        // 0 = top-left, 1 = top-right, 2 = bottom-right, 3 = bottom-left.
        let top_width = case[1].x - case[0].x;
        let base_width = case[2].x - case[3].x;
        let height = case[3].y - case[0].y;
        assert!(
            top_width < base_width * 0.7,
            "the case must read as a trapezoid, got top {top_width} vs base {base_width}"
        );
        // Taller than wide, like the instrument — otherwise it reads as a bag.
        assert!(
            base_width < height,
            "case is too squat: {base_width} wide, {height} tall"
        );
        // Symmetric about the vertical axis, or it looks hand-drawn by mistake.
        assert!((case[0].x + case[1].x - case[2].x - case[3].x).abs() < 1e-3);
        assert!(case[0].y < case[3].y && case[1].y < case[2].y);
    }

    #[test]
    fn the_pendulum_stays_inside_the_case_and_is_visibly_slanted() {
        let (case, rod, weight) = shape();
        let [pivot, tip] = rod;
        let center_x = case[0].x + (case[1].x - case[0].x) * 0.5;
        for point in [pivot, tip, weight[0], weight[1]] {
            let half = case_half_width_at(&case, point.y);
            assert!(
                (point.x - center_x).abs() < half,
                "pendulum leaves the case at {point:?} (half-width there {half})"
            );
        }
        // A near-vertical rod reads as a bell rather than a metronome.
        let slant = (tip.x - pivot.x).abs() / (pivot.y - tip.y).abs();
        assert!(slant > 0.15, "rod is too upright: slope {slant}");
        assert_ne!(weight[0], weight[1]);
    }
}
