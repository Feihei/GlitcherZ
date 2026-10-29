//! Mixer channel title cell (Inspector): the whole strip-title rect must answer
//! a **right**-click, because that is what opens the inline rename field.
//!
//! The console lives inside `GlitcherZApp::mixer_section`, which needs an audio
//! engine to run, so this mirrors just the cell: an `allocate_exact_size` rect
//! with `Sense::click()` and a truncating `Label` drawn into it by a child Ui.
//! Two things are easy to get wrong and both are pinned here:
//!
//! - a bare `Label` only senses hover, so `secondary_clicked()` never fires —
//!   the hit test has to live on the allocated rect, not the glyph run;
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
                let (cell, resp) =
                    ui.allocate_exact_size(egui::vec2(STRIP_W, TITLE_H), egui::Sense::click());
                if i == TARGET {
                    *rect = Some(cell);
                }
                let mut cell_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(cell)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                cell_ui.add(
                    egui::Label::new(egui::RichText::new(format!("Ch {}", i + 1)).weak())
                        .truncate(),
                );
                if i == TARGET && resp.secondary_clicked() {
                    *hits += 1;
                }
            }
        });
    });
    out.textures_delta.clear();
}

#[test]
fn a_right_click_on_the_title_cell_is_reported() {
    let ctx = egui::Context::default();
    let mut hits = 0;
    let mut rect = None;
    // Two settling frames: the first lays the row out, `rect` is read after.
    for k in 0..2 {
        frame(&ctx, vec![], k as f64 * 0.1, &mut hits, &mut rect);
    }
    let cell = rect.expect("the target cell must be laid out");
    // Aim at the right edge of the cell, past the glyphs: a short name leaves
    // empty space there, and that space must still open the editor.
    let pos = egui::pos2(cell.right() - 4.0, cell.center().y);

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

    assert_eq!(
        hits, 1,
        "a right-click on the strip title cell must be reported (it opens the rename editor)"
    );
}
