//! Mixer strip row navigation (Inspector): the row must be laid out in a child
//! Ui with an **explicit** height, and the middle-drag pan must move the
//! horizontal scroll offset.
//!
//! Pins an egui 0.36 gotcha that cost a round trip: `ui.horizontal` hands its
//! child a max_rect one row tall (~18 px). A `ScrollArea::horizontal` nested in
//! there gets a viewport of that height — the strips still overflow and paint,
//! so nothing LOOKS broken, but the middle-drag hit box and the scroll bar both
//! collapse into an 18 px band at the top of the row ("pan does nothing").
//! `mixer_section` therefore builds the row with `ui.new_child` + an explicit
//! height and gates the pan on the band it measured. This test reproduces that
//! layout and fails if the row goes back to `ui.horizontal`.
//!
//! It also pins the **fixed-size title cell**. Inside a horizontal scroll area
//! the content Ui's available width is the viewport width (not infinity) and
//! every column already drawn eats into what the next one sees. A wrapping
//! label therefore re-flows as the row grows: "Ch 10" onwards ended up one
//! glyph per line and the title cell grew tenfold. Truncation is the fix.

use eframe::egui;

const SCREEN: egui::Vec2 = egui::vec2(920.0, 760.0);
/// Mirrors `MIXER_STRIP_SCROLL` in the crate.
const STRIP_SCROLL: &str = "glitcherz_mixer_strip_scroll";
/// Mirrors `MIXER_STRIP_ROW` in the crate.
const STRIP_ROW: &str = "glitcherz_mixer_strip_row";
/// Mirrors `MIXER_STRIP_MIN_H` in the crate (first-frame bootstrap).
const STRIP_MIN_H: f32 = 240.0;
/// Mirrors `MIXER_STRIP_CONTROLS_W` in the crate.
const CONTROLS_W: f32 = 120.0;
/// Mirrors `MIXER_STRIP_W` in the crate; `TITLE_H` is the strip title cell
/// (`MIXER_STRIP_TITLE_MIN_H` = 18 in the crate, floored from one line of body
/// text — 20 here keeps a little slack).
const STRIP_W: f32 = 96.0;
const TITLE_H: f32 = 20.0;
/// One strip column: label + S/M row + meter + fader + pan.
const CHANNELS: usize = 20;

#[derive(Clone, Copy)]
struct Probe {
    /// The strip band on screen — the middle-drag hit box.
    band: egui::Rect,
    content: egui::Vec2,
    offset: f32,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            band: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::ZERO),
            content: egui::Vec2::ZERO,
            offset: 0.0,
        }
    }
}

/// One frame of the Inspector's mixer console (parameter stack above, strip row
/// at the bottom), with the pan block `mixer_section` runs.
fn frame(
    ctx: &egui::Context,
    events: Vec<egui::Event>,
    t: f64,
    view: &mut Option<(egui::Id, egui::Rect)>,
    strip_h: &mut f32,
    title_h: &mut Vec<f32>,
) -> Probe {
    let mut p = Probe::default();
    title_h.clear();
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
    let out = ctx.run_ui(raw, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Inspector");
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    // Parameter stack above the console.
                    let mut v = 0.5_f32;
                    for i in 0..40 {
                        ui.horizontal(|ui| {
                            ui.label(format!("param {i}"));
                            let _ =
                                ui.add_sized([200.0, 20.0], egui::Slider::new(&mut v, 0.0..=1.0));
                        });
                    }

                    let strip_width = (ui.available_width() - CONTROLS_W).max(160.0);

                    // ---- pan (same shape as `mixer_section`) ----
                    let c = ui.ctx();
                    if let Some((scroll_id, band)) = *view
                        && c.pointer_hover_pos().is_some_and(|pos| band.contains(pos))
                    {
                        let (down, delta) = c.input(|i| {
                            (
                                i.pointer.button_down(egui::PointerButton::Middle),
                                i.pointer.delta(),
                            )
                        });
                        if down
                            && delta != egui::Vec2::ZERO
                            && let Some(mut s) =
                                egui::containers::scroll_area::State::load(c, scroll_id)
                        {
                            s.offset.x = (s.offset.x - delta.x).max(0.0);
                            s.store(c, scroll_id);
                        }
                    }

                    let row_rect = egui::Rect::from_min_size(
                        ui.cursor().min,
                        egui::vec2(ui.available_width(), *strip_h),
                    );
                    let mut row = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(row_rect)
                            .layout(egui::Layout::left_to_right(egui::Align::Min))
                            .id_salt(STRIP_ROW),
                    );
                    let (scroll_id, band) = {
                        let strips = egui::ScrollArea::horizontal()
                            .id_salt(STRIP_SCROLL)
                            .max_width(strip_width)
                            .auto_shrink([true, true])
                            .show(&mut row, |ui| {
                                ui.horizontal(|ui| {
                                    for i in 0..CHANNELS {
                                        ui.vertical(|ui| {
                                            // Same title cell as `mixer_section`:
                                            // a fixed-size rect with a
                                            // truncating label inside, so it can
                                            // never wrap no matter how little
                                            // width the column has left.
                                            let (cell, resp) = ui.allocate_exact_size(
                                                egui::vec2(STRIP_W, TITLE_H),
                                                egui::Sense::click(),
                                            );
                                            let mut cell_ui = ui.new_child(
                                                egui::UiBuilder::new().max_rect(cell).layout(
                                                    egui::Layout::left_to_right(
                                                        egui::Align::Center,
                                                    ),
                                                ),
                                            );
                                            let title = cell_ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(format!("Ch {}", i + 1))
                                                        .weak(),
                                                )
                                                .truncate(),
                                            );
                                            let _ = resp.on_hover_text(format!("Ch {}", i + 1));
                                            title_h.push(title.rect.height());
                                            let _ =
                                                ui.add_sized([96.0, 20.0], egui::Button::new("S"));
                                            let _ = ui.allocate_exact_size(
                                                egui::vec2(96.0, 10.0),
                                                egui::Sense::hover(),
                                            );
                                            let mut f = 0.5_f32;
                                            let _ = ui.add_sized(
                                                [96.0, 110.0],
                                                egui::Slider::new(&mut f, 0.0..=1.5).vertical(),
                                            );
                                            let mut p = 0.0_f32;
                                            let _ = ui.add_sized(
                                                [96.0, 20.0],
                                                egui::Slider::new(&mut p, -1.0..=1.0).text("Pan"),
                                            );
                                        });
                                    }
                                });
                            });
                        p.content = strips.content_size;
                        p.offset = strips.state.offset.x;
                        (
                            strips.id,
                            egui::Rect::from_min_size(
                                strips.inner_rect.min,
                                egui::vec2(
                                    strips.inner_rect.width(),
                                    strips.content_size.y.min(strips.inner_rect.height()),
                                ),
                            ),
                        )
                    };
                    p.band = band;
                    *view = Some((scroll_id, band));

                    row.separator();
                    row.vertical(|ui| {
                        ui.weak("Channels");
                        let _ = ui.button("+ Ch");
                    });
                    let used = row.min_rect();
                    ui.advance_cursor_after_rect(used);
                    // Same formula as `mixer_section`: content + the bar it
                    // allocates (0 when floating).
                    *strip_h = (p.content.y + ui.spacing().scroll.allocated_width()).max(1.0);
                });
        });
    });
    let mut out = out;
    out.textures_delta.clear();
    p
}

#[test]
fn strip_row_band_is_real_and_middle_drag_pans() {
    let ctx = egui::Context::default();
    let mut view: Option<(egui::Id, egui::Rect)> = None;
    let mut strip_h = STRIP_MIN_H;
    let mut title_h = Vec::new();
    let _ = frame(&ctx, vec![], 0.0, &mut view, &mut strip_h, &mut title_h);
    let p = frame(&ctx, vec![], 0.1, &mut view, &mut strip_h, &mut title_h);

    // The band must be the strip itself, not one collapsed text row.
    assert!(
        p.band.height() >= 150.0,
        "strip band height {} is collapsed (the `ui.horizontal` regression)",
        p.band.height()
    );
    // 20 strips overflow any panel width, so there is something to pan.
    assert!(
        p.content.x > p.band.width() + 100.0,
        "strips do not overflow: content {} vs band {}",
        p.content.x,
        p.band.width()
    );
    assert_eq!(p.offset, 0.0);

    // Middle-drag from the middle of a fader (the part of the strip the
    // collapsed band used to miss).
    let start = egui::pos2(p.band.center().x, p.band.top() + 150.0);
    let _ = frame(
        &ctx,
        vec![egui::Event::PointerMoved(start)],
        0.2,
        &mut view,
        &mut strip_h,
        &mut title_h,
    );
    let _ = frame(
        &ctx,
        vec![egui::Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Middle,
            pressed: true,
            modifiers: Default::default(),
        }],
        0.3,
        &mut view,
        &mut strip_h,
        &mut title_h,
    );
    for k in 1..4 {
        let pos = start - egui::vec2(20.0 * k as f32, 0.0);
        let _ = frame(
            &ctx,
            vec![egui::Event::PointerMoved(pos)],
            0.3 + 0.05 * k as f64,
            &mut view,
            &mut strip_h,
            &mut title_h,
        );
    }
    let p = frame(&ctx, vec![], 1.0, &mut view, &mut strip_h, &mut title_h);
    assert!(
        p.offset > 0.0,
        "middle-drag over the strips must pan the row (offset {})",
        p.offset
    );
}

/// Every strip title must stay ONE line tall, including the columns far into
/// the row. Regression: the strip Ui is a child of the horizontal scroll area,
/// whose content width is the *viewport* width — each column drawn before it
/// eats into what the next one is given, so a wrapping label re-flowed "Ch 10"
/// onwards to one glyph per line and the title cell grew tenfold ("Ch 1-9 are
/// one line, Ch 10+ each letter on its own line"). Fixed size + truncation is
/// the invariant this pins.
#[test]
fn strip_titles_stay_one_line_for_every_channel() {
    let ctx = egui::Context::default();
    let mut view: Option<(egui::Id, egui::Rect)> = None;
    let mut strip_h = STRIP_MIN_H;
    let mut title_h = Vec::new();
    let _ = frame(&ctx, vec![], 0.0, &mut view, &mut strip_h, &mut title_h);
    let _ = frame(&ctx, vec![], 0.1, &mut view, &mut strip_h, &mut title_h);

    assert_eq!(
        title_h.len(),
        CHANNELS,
        "every strip must draw exactly one title cell"
    );
    for (i, h) in title_h.iter().enumerate() {
        assert!(
            *h <= TITLE_H + 1.0,
            "title of Ch {} is {h} px tall — it wrapped (fixed {TITLE_H} px cell expected)",
            i + 1
        );
    }
}
