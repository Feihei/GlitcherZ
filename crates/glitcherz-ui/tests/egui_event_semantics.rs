//! Regression test for the egui 0.36 event semantics the patch relies on.
//!
//! egui 0.36 disambiguates clicks from drags for widgets that sense both
//! (`Sense::click_and_drag`): an in-place press NEVER produces `drag_started`,
//! only a release-frame `clicked`. The patch point-select therefore runs on
//! both paths. If a future egui upgrade changes this contract, these tests
//! catch it before the patch interactions silently break.

use eframe::egui;

const SCREEN: egui::Vec2 = egui::vec2(800.0, 600.0);
const CENTER: egui::Pos2 = egui::pos2(400.0, 300.0);
/// Below the probed widget (it only covers the top `SCREEN.y - 50`) — dead
/// space no widget claims.
const OUTSIDE: egui::Pos2 = egui::pos2(400.0, 580.0);

#[derive(Default)]
struct Probe {
    clicked: bool,
    drag_started_primary: bool,
    dragged: bool,
    dragged_primary: bool,
    dragged_middle: bool,
    hovered: bool,
    smooth_y: f32,
    zoom_delta: f32,
    global_primary_clicked: bool,
    press_origin: Option<egui::Pos2>,
    interact_pos: Option<egui::Pos2>,
}

/// Run one egui frame with the given events and probe the patch-like widget.
fn frame(ctx: &egui::Context, events: Vec<egui::Event>, t: f64) -> Probe {
    let mut p = Probe::default();
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
        viewports,
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            let (_, r) = ui.allocate_exact_size(
                egui::vec2(SCREEN.x, SCREEN.y - 50.0),
                egui::Sense::click_and_drag(),
            );
            p.clicked = r.clicked();
            p.drag_started_primary = r.drag_started_by(egui::PointerButton::Primary);
            p.dragged = r.dragged();
            p.dragged_primary = r.dragged_by(egui::PointerButton::Primary);
            p.dragged_middle = r.dragged_by(egui::PointerButton::Middle);
            p.hovered = r.hovered();
        });
        p.smooth_y = ctx.input(|i| i.smooth_scroll_delta.y);
        p.zoom_delta = ctx.input(|i| i.zoom_delta());
        p.global_primary_clicked =
            ctx.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary));
        let (origin, interact) =
            ctx.input(|i| (i.pointer.press_origin(), i.pointer.interact_pos()));
        p.press_origin = origin;
        p.interact_pos = interact;
    });
    out.textures_delta.clear();
    p
}

fn hover(pos: egui::Pos2) -> egui::Event {
    egui::Event::PointerMoved(pos)
}

fn press(button: egui::PointerButton, pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers: Default::default(),
    }
}

fn wheel(delta: egui::Vec2, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Line,
        delta,
        modifiers,
        phase: egui::TouchPhase::Move,
    }
}

#[test]
fn plain_click_never_starts_a_drag_but_fires_clicked() {
    let ctx = egui::Context::default();
    frame(&ctx, vec![hover(CENTER)], 0.0);
    frame(
        &ctx,
        vec![
            hover(CENTER),
            press(egui::PointerButton::Primary, CENTER, true),
        ],
        0.1,
    );
    let hold = frame(&ctx, vec![hover(CENTER)], 0.3);
    let release = frame(
        &ctx,
        vec![
            hover(CENTER),
            press(egui::PointerButton::Primary, CENTER, false),
        ],
        0.4,
    );

    assert!(
        !hold.drag_started_primary,
        "in-place press must not start a drag"
    );
    assert!(!hold.dragged);
    assert!(
        release.clicked,
        "press+release without movement must be a click (patch point-select depends on this)"
    );
}

#[test]
fn press_plus_movement_starts_a_drag() {
    let ctx = egui::Context::default();
    frame(&ctx, vec![hover(CENTER)], 0.0);
    frame(
        &ctx,
        vec![
            hover(CENTER),
            press(egui::PointerButton::Primary, CENTER, true),
        ],
        0.1,
    );
    let moved = frame(&ctx, vec![hover(egui::pos2(410.0, 300.0))], 0.2);

    assert!(
        moved.drag_started_primary && moved.dragged,
        "moving 10pt (> max_click_dist 6pt) must start a drag"
    );
}

/// Middle-drag is pan-only: egui's UNFILTERED `drag_started`/`dragged` fire
/// for any button, so a middle-drag would otherwise enter the left-button
/// gesture paths (marquee, draw, clip move). The roll / timeline filter
/// every gesture entry by `drag_*_by(Primary)` — this test locks the egui
/// side of that contract (a middle-drag reports `dragged()` true while
/// `dragged_by(Primary)` stays false).
#[test]
fn middle_drag_never_reports_as_a_primary_drag() {
    let ctx = egui::Context::default();
    frame(&ctx, vec![hover(CENTER)], 0.0);
    frame(
        &ctx,
        vec![
            hover(CENTER),
            press(egui::PointerButton::Middle, CENTER, true),
        ],
        0.1,
    );
    let moved = frame(&ctx, vec![hover(egui::pos2(412.0, 296.0))], 0.2);

    assert!(moved.dragged, "egui reports the middle drag as a drag");
    assert!(
        moved.dragged_middle,
        "dragged_by(Middle) must be true during a middle-drag"
    );
    assert!(
        !moved.drag_started_primary && !moved.dragged_primary,
        "middle-drag must NOT look like a primary drag (roll/timeline pan depends on this)"
    );
    assert!(
        !moved.clicked,
        "a middle-drag with movement must not produce a primary click either"
    );
}

/// A click on empty space (no widget under the pointer) must still show up
/// in the GLOBAL pointer state: the roll / timeline modal transforms
/// confirm / cancel on LMB / RMB *anywhere*, including outside the view
/// (the pointer has often travelled past the content edge by then). This
/// locks the egui side of that contract: `pointer.button_clicked` is
/// global, `Response::clicked` is per-widget.
#[test]
fn a_click_outside_any_widget_is_globally_visible() {
    let ctx = egui::Context::default();
    frame(&ctx, vec![hover(OUTSIDE)], 0.0);
    frame(
        &ctx,
        vec![
            hover(OUTSIDE),
            press(egui::PointerButton::Primary, OUTSIDE, true),
        ],
        0.1,
    );
    // A quick press+release on dead space: a global click, but the widget
    // (which only covers the top of the screen) sees nothing.
    let release = frame(
        &ctx,
        vec![
            hover(OUTSIDE),
            press(egui::PointerButton::Primary, OUTSIDE, false),
        ],
        0.2,
    );

    assert!(
        !release.clicked,
        "the widget does not cover OUTSIDE — its Response must stay silent"
    );
    assert!(
        release.global_primary_clicked,
        "the click on dead space must be globally visible (modal confirm-anywhere depends on this)"
    );
}

/// The piano roll classifies a grab (move / trim / draw) by where the
/// press *landed*. That only works because `press_origin` still reports
/// the press point on the `drag_started` frame — `interact_pos` has
/// already travelled past `max_click_dist` by then, which is exactly how
/// edge grabs used to degrade into moves and tail grabs into new notes.
#[test]
fn drag_started_keeps_the_press_point_separate_from_the_current_pos() {
    let ctx = egui::Context::default();
    frame(&ctx, vec![hover(CENTER)], 0.0);
    frame(
        &ctx,
        vec![
            hover(CENTER),
            press(egui::PointerButton::Primary, CENTER, true),
        ],
        0.1,
    );
    let dragged_to = egui::pos2(CENTER.x + 12.0, CENTER.y);
    let moved = frame(&ctx, vec![hover(dragged_to)], 0.2);

    assert!(moved.drag_started_primary);
    assert_eq!(
        moved.press_origin,
        Some(CENTER),
        "press_origin must be where the button went down, not where it is now"
    );
    assert_eq!(
        moved.interact_pos,
        Some(dragged_to),
        "interact_pos is the live position — the one a grab must NOT be classified by"
    );
    assert_ne!(
        moved.press_origin, moved.interact_pos,
        "if these ever agree on the drag_started frame, the 6pt threshold is gone"
    );
}

#[test]
fn plain_wheel_reaches_smooth_scroll_delta() {
    let ctx = egui::Context::default();
    frame(&ctx, vec![hover(CENTER)], 0.0);
    let p = frame(
        &ctx,
        vec![
            hover(CENTER),
            wheel(egui::vec2(0.0, 1.0), Default::default()),
        ],
        0.1,
    );

    assert!(p.hovered, "patch widget must be hovered for wheel handling");
    assert!(
        p.smooth_y != 0.0,
        "plain wheel events must land in smooth_scroll_delta (patch wheel zoom depends on this)"
    );
}

#[test]
fn zoom_delta_neutral_is_one_not_zero() {
    // `zoom_delta()` is a MULTIPLICATIVE factor: 1.0 = no zoom, NOT 0.0.
    // Comparing it against 0.0 (as the patch once did) is always true and
    // zooms the view ×2 every frame, pinning it at max.
    let ctx = egui::Context::default();
    let idle = frame(&ctx, vec![hover(CENTER)], 0.0);
    assert_eq!(
        idle.zoom_delta, 1.0,
        "no zoom gesture this frame → neutral factor 1.0"
    );

    frame(&ctx, vec![hover(CENTER)], 0.1);
    let plain = frame(
        &ctx,
        vec![
            hover(CENTER),
            wheel(egui::vec2(0.0, 1.0), Default::default()),
        ],
        0.2,
    );
    assert_eq!(
        plain.zoom_delta, 1.0,
        "plain wheel (no ctrl) must NOT feed zoom_delta — it goes to smooth_scroll_delta"
    );
}

#[test]
fn ctrl_wheel_reaches_zoom_delta() {
    let ctx = egui::Context::default();
    let mods = egui::Modifiers {
        ctrl: true,
        ..Default::default()
    };
    frame(&ctx, vec![hover(CENTER)], 0.0);
    let p = frame(
        &ctx,
        vec![hover(CENTER), wheel(egui::vec2(0.0, 1.0), mods)],
        0.1,
    );

    assert!(
        p.zoom_delta > 1.0,
        "ctrl+wheel must produce a zoom factor via zoom_delta (COMMAND == ctrl off macOS)"
    );
}
