//! Mixer channel title cell (Inspector): the whole strip-title rect must answer
//! a **right**-click, because that is what opens the inline rename field.
//!
//! The console lives inside `GlitcherZApp::mixer_section`, which needs an audio
//! engine to run, so this mirrors just the cell: an `add_sized` frameless
//! `Button` (the same widget the timeline track header uses). Two things are
//! easy to get wrong and both are pinned here:
//!
//! - a bare `Label` is selectable text by default, so its glyph run shows the
//!   I-beam cursor and swallows right-clicks — only the blank tail of the strip
//!   used to answer. A Button owns the entire rect instead;
//! - the rect, not the label's own rect, is the target: the title is truncated
//!   and short names leave empty space on the right that must still be clickable.

use eframe::egui;

const SCREEN: egui::Vec2 = egui::vec2(600.0, 400.0);
/// Mirrors `MIXER_STRIP_W` in the crate.
const STRIP_W: f32 = 96.0;
/// Mirrors `MIXER_STRIP_TITLE_MIN_H` in the crate.
const TITLE_H: f32 = 18.0;
/// Cell index the click probe aims at (a cell in the middle of the row).
const TARGET: usize = 2;

/// One frame of the strip-title row; returns the clicked cell's rect.
fn frame(
    ctx: &egui::Context,
    events: Vec<egui::Event>,
    t: f64,
    hits: &mut usize,
    rect: &mut Option<egui::Rect>,
) {
    let mut viewports = egui::ViewportIdMap::default();
    viewports.insert(
        egui::ViewportId::ROOT,
        egui::ViewportInfo {
            inner_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
            ..Default::default()
        },
    );
    let raw = egui::RawInput {
        events,
        time: Some(t),
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
        viewports,
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |root| {
        egui::CentralPanel::default().show(root, |ui| {
            for i in 0..4 {
                let resp = ui
                    .add_sized(
                        [STRIP_W, TITLE_H],
                        egui::Button::new(egui::RichText::new(format!("Ch {}", i + 1)).size(11.0))
                            .frame(false)
                            .fill(egui::Color32::TRANSPARENT)
                            .truncate(),
                    )
                    .on_hover_text("Right-click to rename this channel");
                if i == TARGET {
                    *rect = Some(resp.rect);
                }
                if i == TARGET && resp.secondary_clicked() {
                    *hits += 1;
                }
            }
        });
    });
    out.textures_delta.clear();
}

/// Runs a right-click at `offset_x` inside the target cell and reports whether
/// it reached the cell widget.
fn right_click_at(offset_x: f32) -> usize {
    let ctx = egui::Context::default();
    let mut hits = 0;
    let mut rect = None;
    // Two settling frames: the first lays the row out, `rect` is read after.
    for k in 0..2 {
        frame(&ctx, vec![], k as f64 * 0.1, &mut hits, &mut rect);
    }
    let cell = rect.expect("the target cell must be laid out");
    let pos = egui::pos2(cell.left() + offset_x, cell.center().y);

    let mut t = 0.2;
    let mut step = |events: Vec<egui::Event>, hits: &mut usize, rect: &mut Option<egui::Rect>| {
        t += 0.1;
        frame(&ctx, events, t, hits, rect);
    };
    let hover = egui::Event::PointerMoved(pos);
    step(vec![hover.clone()], &mut hits, &mut rect);
    for pressed in [true, false] {
        step(
            vec![
                hover.clone(),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Secondary,
                    pressed,
                    modifiers: Default::default(),
                },
            ],
            &mut hits,
            &mut rect,
        );
    }
    step(vec![], &mut hits, &mut rect);
    hits
}

#[test]
fn a_right_click_on_the_glyph_run_is_reported() {
    // The regression: with a selectable `Label` this click went to the text
    // selection interaction instead of the cell.
    assert_eq!(
        right_click_at(6.0),
        1,
        "right-clicking the channel name itself must open the rename editor"
    );
}

#[test]
fn a_right_click_on_the_blank_tail_is_reported() {
    // A short name leaves empty space on the right; that space must still
    // belong to the cell.
    assert_eq!(
        right_click_at(STRIP_W - 4.0),
        1,
        "right-clicking the empty tail of the title cell must open the editor"
    );
}
