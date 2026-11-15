//! Ruler seek: a bare click (press + release without a 6 pt move) on a ruler
//! must place the playhead — egui 0.37 never fires `drag_started_by` for an
//! in-place click, so the seek cannot hang off the drag gesture alone.
//!
//! The harness mirrors `timeline_playhead.rs` / `egui_event_semantics.rs`:
//! synthetic pointer events replayed into a real `egui::Context`. The ruler
//! rect is learned from the frame's shapes (the `from_gray(24)` backdrop both
//! rulers paint), so no layout constant is duplicated here.

use eframe::egui;
use glitcherz_model::{MidiClip, MidiData, MidiTrack, Sequence, TICKS_PER_BAR};
use glitcherz_ui::piano_roll::{RollIntent, RollState, RollSubject};
use glitcherz_ui::timeline::{self, TimelineParams, TimelineState};

const SCREEN: egui::Vec2 = egui::vec2(1000.0, 600.0);
const RULER_BACKDROP: egui::Color32 = egui::Color32::from_gray(24);

fn press(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    }
}

fn hover(pos: egui::Pos2) -> egui::Event {
    egui::Event::PointerMoved(pos)
}

fn raw_input(events: Vec<egui::Event>, time: f64) -> egui::RawInput {
    // Pointer hit-testing needs the root viewport's inner rect; without it
    // the events never match any widget (mirrors roll_state_machine.rs).
    let mut viewports = egui::ViewportIdMap::default();
    viewports.insert(
        egui::ViewportId::ROOT,
        egui::ViewportInfo {
            inner_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
            ..Default::default()
        },
    );
    egui::RawInput {
        events,
        time: Some(time),
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
        viewports,
        ..Default::default()
    }
}

/// Runs `timeline::show` for one frame and returns its actions.
fn timeline_frame(
    ctx: &egui::Context,
    state: &mut TimelineState,
    router: &mut glitcherz_ui::focus::FocusRouter,
    events: Vec<egui::Event>,
    time: f64,
) -> Vec<timeline::TimelineAction> {
    let midi = vec![MidiTrack::default()];
    let control = vec![];
    let audio = vec![];
    let loop_range = glitcherz_model::SongLoop::default();
    let raw = raw_input(events, time);
    let mut actions = Vec::new();
    let mut out = ctx.run_ui(raw, |root| {
        router.begin_frame(root.ctx());
        egui::CentralPanel::default().show(root, |ui| {
            actions = timeline::show(
                ui,
                state,
                &midi,
                &control,
                &audio,
                TimelineParams {
                    selected_track: 0,
                    selected_auto: 0,
                    selected_audio: 0,
                    playhead_ticks: 0.0,
                    playing: false,
                    edit_target: None,
                    loop_range: &loop_range,
                    markers: &[],
                    meter_markers: &[],
                    meter: &glitcherz_model::MeterMap::default(),
                    sec_per_tick: 60.0 / (120.0 * 960.0),
                    waveform_of: &|_| None,
                    clip_groups: &[],
                    midi_data: &[],
                    curve_data: &[],
                },
                router,
            );
        });
    });
    out.textures_delta.clear();
    actions
}

/// The ruler's screen rect, learned from the backdrop rect both rulers fill.
fn ruler_rect_from(out: &egui::FullOutput) -> Option<egui::Rect> {
    out.shapes.iter().find_map(|clipped| match &clipped.shape {
        egui::Shape::Rect(rect) if rect.fill == RULER_BACKDROP => Some(rect.rect),
        _ => None,
    })
}

#[test]
fn timeline_ruler_bare_click_places_the_playhead() {
    let ctx = egui::Context::default();
    let mut state = TimelineState::default();
    let mut router = glitcherz_ui::focus::FocusRouter::new();

    // Frame 1: learn where the ruler sits.
    let mut out = ctx.run_ui(raw_input(vec![], 0.0), |root| {
        router.begin_frame(root.ctx());
        egui::CentralPanel::default().show(root, |ui| {
            let midi = vec![MidiTrack::default()];
            let loop_range = glitcherz_model::SongLoop::default();
            let _ = timeline::show(
                ui,
                &mut state,
                &midi,
                &[],
                &[],
                TimelineParams {
                    selected_track: 0,
                    selected_auto: 0,
                    selected_audio: 0,
                    playhead_ticks: 0.0,
                    playing: false,
                    edit_target: None,
                    loop_range: &loop_range,
                    markers: &[],
                    meter_markers: &[],
                    meter: &glitcherz_model::MeterMap::default(),
                    sec_per_tick: 60.0 / (120.0 * 960.0),
                    waveform_of: &|_| None,
                    clip_groups: &[],
                    midi_data: &[],
                    curve_data: &[],
                },
                &mut router,
            );
        });
    });
    out.textures_delta.clear();
    let ruler = ruler_rect_from(&out).expect("timeline ruler backdrop not found");

    // A click inside the visible part of the ruler: the ruler may span past
    // the screen (its width is the zoomed content width), so anchor the
    // click to a fixed offset from its left edge instead of its center.
    let click_pos = egui::pos2(ruler.left() + 200.0, ruler.center().y);
    let _ = timeline_frame(&ctx, &mut state, &mut router, vec![hover(click_pos)], 0.1);
    let press_frame = timeline_frame(
        &ctx,
        &mut state,
        &mut router,
        vec![hover(click_pos), press(click_pos, true)],
        0.2,
    );
    let release_frame = timeline_frame(
        &ctx,
        &mut state,
        &mut router,
        vec![hover(click_pos), press(click_pos, false)],
        0.3,
    );

    let seeks: Vec<_> = press_frame
        .iter()
        .chain(&release_frame)
        .filter_map(|a| match a {
            timeline::TimelineAction::Seek { ticks } => Some(*ticks),
            _ => None,
        })
        .collect();
    assert_eq!(
        seeks.len(),
        1,
        "a bare ruler click must seek exactly once, got {seeks:?}"
    );
    assert!(seeks[0] > 0.0, "a mid-ruler click must seek past tick 0");
}

#[test]
fn timeline_ruler_drag_still_scrubs() {
    let ctx = egui::Context::default();
    let mut state = TimelineState::default();
    let mut router = glitcherz_ui::focus::FocusRouter::new();

    let mut out = ctx.run_ui(raw_input(vec![], 0.0), |root| {
        router.begin_frame(root.ctx());
        egui::CentralPanel::default().show(root, |ui| {
            let midi = vec![MidiTrack::default()];
            let loop_range = glitcherz_model::SongLoop::default();
            let _ = timeline::show(
                ui,
                &mut state,
                &midi,
                &[],
                &[],
                TimelineParams {
                    selected_track: 0,
                    selected_auto: 0,
                    selected_audio: 0,
                    playhead_ticks: 0.0,
                    playing: false,
                    edit_target: None,
                    loop_range: &loop_range,
                    markers: &[],
                    meter_markers: &[],
                    meter: &glitcherz_model::MeterMap::default(),
                    sec_per_tick: 60.0 / (120.0 * 960.0),
                    waveform_of: &|_| None,
                    clip_groups: &[],
                    midi_data: &[],
                    curve_data: &[],
                },
                &mut router,
            );
        });
    });
    out.textures_delta.clear();
    let ruler = ruler_rect_from(&out).expect("timeline ruler backdrop not found");

    let start = egui::pos2(ruler.left() + 40.0, ruler.center().y);
    let _ = timeline_frame(&ctx, &mut state, &mut router, vec![hover(start)], 0.1);
    let press_frame = timeline_frame(
        &ctx,
        &mut state,
        &mut router,
        vec![hover(start), press(start, true)],
        0.2,
    );
    let mut drag_frame = timeline_frame(
        &ctx,
        &mut state,
        &mut router,
        vec![hover(egui::pos2(start.x + 60.0, ruler.center().y))],
        0.3,
    );
    drag_frame.extend(timeline_frame(
        &ctx,
        &mut state,
        &mut router,
        vec![hover(egui::pos2(start.x + 120.0, ruler.center().y))],
        0.4,
    ));

    let press_seeks = press_frame
        .iter()
        .filter(|a| matches!(a, timeline::TimelineAction::Seek { .. }))
        .count();
    let drag_seeks = drag_frame
        .iter()
        .filter(|a| matches!(a, timeline::TimelineAction::Seek { .. }))
        .count();
    assert_eq!(
        press_seeks, 0,
        "an in-place press must not seek yet (the click path owns it)"
    );
    assert!(
        drag_seeks >= 2,
        "a drag must scrub on every moved frame, got {drag_seeks}"
    );
}

#[test]
fn roll_ruler_bare_click_places_the_playhead() {
    let ctx = egui::Context::default();
    let data = MidiData {
        sequence: Sequence::default(),
        length_ticks: TICKS_PER_BAR,
    };
    let clip = MidiClip {
        start_ticks: 0,
        span_ticks: TICKS_PER_BAR,
        data: 0,
        ..MidiClip::default()
    };
    let tracks = vec![MidiTrack {
        clips: vec![clip],
        ..MidiTrack::default()
    }];
    let midi_data = vec![data];
    let subject = RollSubject::single(0, 0);
    let mut state = RollState::new();
    let mut router = glitcherz_ui::focus::FocusRouter::new();
    let meter = glitcherz_model::MeterMap::default();

    let frame = |ctx: &egui::Context,
                 state: &mut RollState,
                 router: &mut glitcherz_ui::focus::FocusRouter,
                 events: Vec<egui::Event>,
                 time: f64|
     -> Option<RollIntent> {
        let raw = raw_input(events, time);
        let mut intent = None;
        let mut out = ctx.run_ui(raw, |root| {
            router.begin_frame(root.ctx());
            egui::CentralPanel::default().show(root, |ui| {
                intent = glitcherz_ui::piano_roll::show(
                    ui,
                    &tracks,
                    &midi_data,
                    &[],
                    &subject,
                    state,
                    0.0,
                    false,
                    &meter,
                    router,
                );
            });
        });
        out.textures_delta.clear();
        intent
    };

    // Frame 1: learn where the ruler sits.
    let mut out = ctx.run_ui(raw_input(vec![], 0.0), |root| {
        router.begin_frame(root.ctx());
        egui::CentralPanel::default().show(root, |ui| {
            let _ = glitcherz_ui::piano_roll::show(
                ui,
                &tracks,
                &midi_data,
                &[],
                &subject,
                &mut state,
                0.0,
                false,
                &meter,
                &mut router,
            );
        });
    });
    out.textures_delta.clear();
    let ruler = ruler_rect_from(&out).expect("roll ruler backdrop not found");

    // Click 60% across the ruler: past one bar, so the bar-snapped tick is
    // nonzero and the intent cannot be confused with "nothing happened".
    let click_pos = egui::pos2(ruler.left() + ruler.width() * 0.6, ruler.center().y);
    let _ = frame(&ctx, &mut state, &mut router, vec![hover(click_pos)], 0.1);
    let press_intent = frame(
        &ctx,
        &mut state,
        &mut router,
        vec![hover(click_pos), press(click_pos, true)],
        0.2,
    );
    let release_intent = frame(
        &ctx,
        &mut state,
        &mut router,
        vec![hover(click_pos), press(click_pos, false)],
        0.3,
    );

    let seeks: Vec<i64> = [&press_intent, &release_intent]
        .into_iter()
        .filter_map(|i| match i {
            Some(RollIntent::Seek { tick }) => Some(*tick),
            _ => None,
        })
        .collect();
    assert_eq!(
        seeks.len(),
        1,
        "a bare roll-ruler click must seek exactly once, got {seeks:?}"
    );
    assert!(seeks[0] > 0, "a 60%-across click must land past bar 1");
}
