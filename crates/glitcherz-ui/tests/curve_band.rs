//! Inline curve band (U4b, spec `control-tracks.md` D9/D10).
//!
//! The band reads egui responses, so nothing about it is testable from plain
//! numbers: these tests drive real input through `timeline::show` and pin the
//! two properties that would silently rot —
//!
//! 1. the band is actually there (a click inside it yields
//!    `TimelineAction::CurvePoint` for the clip under the pointer), and
//! 2. its vertical axis is the **real 7-bit CC range** (D7), not a normalized
//!    0..1 — the whole point of storing raw CC values is that a curve written
//!    at 74 means 74.

use eframe::egui;
use glitcherz_model::{AutoParam, AutoPoint, ControlClip, ControlTrack, MidiTrack, TICKS_PER_BAR};
use glitcherz_ui::timeline::{self, TimelineAction, TimelineParams, TimelineState};

const SCREEN: egui::Vec2 = egui::vec2(900.0, 600.0);
/// Probe x: past the row header and well inside the clip.
const LANE_X: f32 = 400.0;

struct Fixture {
    ctx: egui::Context,
    state: TimelineState,
    router: glitcherz_ui::focus::FocusRouter,
    track: MidiTrack,
    control: ControlTrack,
    param: AutoParam,
    /// Actions the view reported on the last frame.
    actions: Vec<TimelineAction>,
    time: f64,
}

impl Fixture {
    /// One MIDI row above one control row whose clip is 64 bars long with a
    /// one-bar loop — the curve therefore repeats 64 times, the "absolute
    /// ticks, repeated per loop" layout the band draws.
    fn new() -> Self {
        let mut clip = ControlClip::new(0);
        clip.span_ticks = 64 * TICKS_PER_BAR;
        clip.loop_ticks = TICKS_PER_BAR;
        clip.curve = vec![
            AutoPoint {
                tick: 0,
                value: 0.0,
            },
            AutoPoint {
                tick: TICKS_PER_BAR / 2,
                value: 127.0,
            },
        ];
        let mut control = ControlTrack::default();
        control.clips.push(clip);
        Self {
            ctx: egui::Context::default(),
            state: TimelineState::default(),
            router: glitcherz_ui::focus::FocusRouter::new(),
            track: MidiTrack::default(),
            control,
            param: AutoParam::Cc {
                controller: 74,
                channel: 0,
            },
            actions: Vec::new(),
            time: 0.0,
        }
    }

    fn raw(&self, events: Vec<egui::Event>) -> egui::RawInput {
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
            time: Some(self.time),
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
            viewports,
            ..Default::default()
        }
    }

    fn frame(&mut self, events: Vec<egui::Event>) {
        self.time += 0.1;
        let raw = self.raw(events);
        let Self {
            ctx,
            state,
            router,
            track,
            control,
            param,
            actions,
            ..
        } = self;
        let loop_range = glitcherz_model::SongLoop::default();
        let mut out = ctx.run_ui(raw, |root| {
            router.begin_frame(root.ctx());
            egui::CentralPanel::default().show(root, |ui| {
                *actions = timeline::show(
                    ui,
                    state,
                    std::slice::from_ref(track),
                    std::slice::from_ref(control),
                    TimelineParams {
                        selected_track: 0,
                        // The band only opens for the selected control track.
                        selected_auto: 0,
                        playhead_ticks: 0.0,
                        playing: false,
                        edit_target: None,
                        loop_range: &loop_range,
                        markers: &[],
                        bar_ticks: TICKS_PER_BAR,
                        curve_param: Some(param),
                    },
                    router,
                );
            });
            router.register_neutral(root.min_rect());
        });
        out.textures_delta.clear();
    }

    /// A click (press + release, no movement) in four frames, matching what a
    /// real platform delivers.
    fn click(&mut self, pos: egui::Pos2) {
        let hover = egui::Event::PointerMoved(pos);
        self.frame(vec![hover.clone()]);
        self.frame(vec![hover.clone(), press_at(pos, true)]);
        self.frame(vec![hover.clone()]);
        self.frame(vec![hover, press_at(pos, false)]);
    }

    /// Clicks down the probe line and returns the `CurvePoint` actions it
    /// produced, plus the y that produced the first one. `None` = no click
    /// ever landed inside a curve band.
    fn probe(&mut self) -> Option<(f32, Vec<TimelineAction>)> {
        let mut y = 40.0;
        while y < 560.0 {
            self.click(egui::pos2(LANE_X, y));
            let hits: Vec<TimelineAction> = self
                .actions
                .iter()
                .filter(|a| matches!(a, TimelineAction::CurvePoint { .. }))
                .cloned()
                .collect();
            if !hits.is_empty() {
                return Some((y, hits));
            }
            y += 4.0;
        }
        None
    }
}

fn press_at(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    }
}

fn point_value(action: &TimelineAction) -> f32 {
    match action {
        TimelineAction::CurvePoint { value, .. } => *value,
        other => panic!("expected a CurvePoint, got {other:?}"),
    }
}

fn point_tick(action: &TimelineAction) -> (usize, usize, u32, bool) {
    match action {
        TimelineAction::CurvePoint {
            track,
            clip,
            tick,
            dragging,
            ..
        } => (*track, *clip, *tick, *dragging),
        other => panic!("expected a CurvePoint, got {other:?}"),
    }
}

/// A click inside the band lands an anchor on the clip under the pointer, as
/// a single undo step (no drag), and the anchor stays inside the loop — the
/// band repeats the curve, but the document only stores one loop's worth.
#[test]
fn clicking_the_band_adds_an_anchor_in_loop_space() {
    let mut f = Fixture::new();
    let (y, hits) = f.probe().expect("no click landed inside a curve band");
    let (track, clip, tick, dragging) = point_tick(&hits[0]);
    assert_eq!((track, clip), (0, 0), "wrong track / clip");
    assert!(!dragging, "a plain click is a single undo step, not a drag");
    assert!(
        tick < TICKS_PER_BAR,
        "anchor tick {tick} escaped the one-bar loop"
    );
    // The probe stopped at the band's top edge (within a few px — the row
    // above can grow a hint and shift the band by one step), which is the top
    // of the axis. A normalized 0..1 axis would top out at 1.0 here.
    let top = point_value(&hits[0]);
    assert!(top > 110.0, "the axis top should be near CC 127, got {top}");
    // 72 px lower, near the bottom edge: the value must be near 0.
    f.click(egui::pos2(LANE_X, y + 72.0));
    let low = f
        .actions
        .iter()
        .find(|a| matches!(a, TimelineAction::CurvePoint { .. }))
        .map(point_value)
        .expect("the lower probe missed the band");
    assert!(low < 30.0, "the axis bottom should be near CC 0, got {low}");
}

/// Dragging is one gesture, not one edit per frame: the band reports
/// `dragging: true` with the previous tick as `from` (so the caller can drop
/// the old anchor) and closes the gesture with `CurveDragEnd` on release.
#[test]
fn dragging_an_anchor_is_one_gesture() {
    let mut f = Fixture::new();
    let (y, _) = f.probe().expect("no click landed inside a curve band");
    // Away from the probe's anchor in x, so the gesture does not start on it.
    let start = egui::pos2(LANE_X + 60.0, y + 20.0);
    let moved = egui::pos2(start.x + 30.0, start.y - 16.0);

    f.frame(vec![egui::Event::PointerMoved(start)]);
    f.frame(vec![press_at(start, true)]);
    // Past egui's drag threshold: this frame starts the gesture. egui reports
    // `drag_started` AND `dragged` here, but only the start arm may write —
    // otherwise a gesture would move its anchor twice per frame.
    f.frame(vec![egui::Event::PointerMoved(moved)]);
    let first: Vec<&TimelineAction> = f
        .actions
        .iter()
        .filter(|a| matches!(a, TimelineAction::CurvePoint { dragging: true, .. }))
        .collect();
    assert_eq!(first.len(), 1, "the drag frame must report one CurvePoint");

    f.frame(vec![egui::Event::PointerMoved(
        moved + egui::vec2(10.0, -6.0),
    )]);
    let step = f
        .actions
        .iter()
        .find(|a| matches!(a, TimelineAction::CurvePoint { dragging: true, .. }))
        .expect("the second drag frame reported nothing");
    match step {
        TimelineAction::CurvePoint { from, .. } => assert!(
            from.is_some(),
            "a continuing drag carries the anchor's previous tick in `from`"
        ),
        other => panic!("expected a CurvePoint, got {other:?}"),
    }

    f.frame(vec![press_at(moved, false)]);
    assert!(
        f.actions
            .iter()
            .any(|a| matches!(a, TimelineAction::CurveDragEnd)),
        "releasing must close the merged undo gesture"
    );
}
