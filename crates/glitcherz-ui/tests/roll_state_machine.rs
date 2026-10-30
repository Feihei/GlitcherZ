//! M4 editing-interaction model: the state × target × qualifier → intent
//! mapping matrix, driven through real egui frames.
//!
//! The piano roll classifies gestures from **discrete state** (persistent
//! mode + armed one-shot command), so the interesting cases are exactly
//! the combinations: which mode is active, where the press landed, which
//! command (and qualifier) is armed. Each test below is one cell of that
//! matrix, asserted through the intent the frame actually emitted.
//!
//! The harness mirrors `egui_event_semantics.rs`: synthetic pointer and
//! key events replayed into a real `egui::Context`. One addition — the
//! roll's on-screen geometry is not known statically (the toolbar row's
//! height is egui's business), so the fixture calibrates pitch 60's row
//! by probing clicks and derives every other position from `ROW_HEIGHT`
//! and the measured panel width.

use eframe::egui;
use glitcherz_model::{AutoPoint, ControlClip, MidiClip, MidiTrack, Note, Sequence, TICKS_PER_BAR};
use glitcherz_ui::piano_roll::{
    CURVE_LANE_COLLAPSED_HEIGHT, ClipRef, OverlayLane, ROW_HEIGHT, RollIntent, RollMode, RollState,
    RollSubject,
};

const SCREEN: egui::Vec2 = egui::vec2(800.0, 600.0);

/// A roll fixture: one clip with two notes, a live egui context, and the
/// measured screen geometry.
///
/// - note A: pitch 60, ticks [0..240]
/// - note B: pitch 62, ticks [480..720]
/// - loop: one bar (3840 ticks), default grid 1/16 (240 ticks)
struct Fixture {
    ctx: egui::Context,
    /// One track holding the focus clip — the roll takes the project table
    /// now (P3: a subject addresses clips by track/clip index). A test may
    /// push a ghost clip in and point the subject's context at it.
    tracks: Vec<MidiTrack>,
    /// What the roll shows; defaults to the single focus clip.
    subject: RollSubject,
    state: RollState,
    /// Hover-follow focus arbiter, mirroring the app's per-frame flow.
    router: glitcherz_ui::focus::FocusRouter,
    /// Roll rect left edge (the roll spans the panel's full width).
    left: f32,
    ticks_to_x: f32,
    /// Screen y of pitch 60's row (calibrated, not derived).
    y60: f32,
    time: f64,
    /// Overlay curve lanes handed to the roll this frame (P4). The owned
    /// data lives here; `frame()` re-borrows it into `OverlayLane`s.
    lanes: Vec<LaneFixture>,
}

/// One P4 lane: the clip, its curve, and the value axis the caller derives
/// from the automation destination.
struct LaneFixture {
    clip: ClipRef,
    data: ControlClip,
    axis: (f32, f32, bool),
    label: String,
}

impl Fixture {
    fn new() -> Self {
        let clip = MidiClip {
            loop_ticks: TICKS_PER_BAR,
            sequence: Sequence {
                notes: vec![
                    Note {
                        start: 0,
                        length: 240,
                        note: 60,
                        ..Note::default()
                    },
                    Note {
                        start: 480,
                        length: 240,
                        note: 62,
                        ..Note::default()
                    },
                ],
                ..Sequence::default()
            },
            ..MidiClip::default()
        };
        let mut fixture = Self {
            ctx: egui::Context::default(),
            tracks: vec![MidiTrack {
                clips: vec![clip],
                ..MidiTrack::default()
            }],
            subject: RollSubject::single(0, 0),
            state: RollState::new(),
            router: glitcherz_ui::focus::FocusRouter::new(),
            left: 0.0,
            ticks_to_x: 0.0,
            y60: 0.0,
            time: 0.0,
            lanes: Vec::new(),
        };
        fixture.measure();
        fixture.calibrate();
        fixture
    }

    /// Learns the panel geometry: the roll is allocated at the panel's
    /// full width, so its left edge is the panel's left edge.
    fn measure(&mut self) {
        let mut rect = egui::Rect::NOTHING;
        let raw = self.raw(vec![]);
        let mut out = self.ctx.run_ui(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                rect = ui.max_rect();
            });
        });
        out.textures_delta.clear();
        self.left = rect.left();
        self.ticks_to_x = rect.width() / self.tracks[0].clips[0].loop_ticks as f32;
    }

    /// Probes clicks down a vertical line until one lands on note A,
    /// recording the row's screen y. The probe x sits inside note A
    /// (tick ≈ 90) and below the toolbar, so it can never hit the grid
    /// combo or the mode chip.
    fn calibrate(&mut self) {
        let mut y = 60.0;
        while y < 520.0 {
            for x in [30.0, 45.0] {
                if self.click_hits_note_a(egui::pos2(x, y)) {
                    self.y60 = y;
                    return;
                }
            }
            y += 2.0;
        }
        panic!("calibration failed: note A (pitch 60) was never hit");
    }

    fn click_hits_note_a(&mut self, pos: egui::Pos2) -> bool {
        self.click(pos);
        self.state.selection == vec![0]
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
            // Without an explicit screen rect egui lays out into a huge
            // default area and pointer interaction stops matching.
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
            viewports,
            ..Default::default()
        }
    }

    /// Runs one frame of the roll; returns the intent it emitted.
    fn frame(&mut self, events: Vec<egui::Event>) -> Option<RollIntent> {
        // 0.1s per frame. Two egui input thresholds pin this value:
        // `max_click_duration` (0.8s) — a press held longer is no longer a
        // click, so a 4-frame click (press → release spans two steps) must
        // stay under 0.4s; and `max_double_click_delay` (0.3s) — successive
        // probe clicks must NOT pair up, so a whole click (4 steps = 0.4s)
        // must exceed it. 0.1 satisfies both.
        self.time += 0.1;
        let raw = self.raw(events);
        let Self {
            ctx,
            tracks,
            subject,
            state,
            router,
            lanes,
            ..
        } = self;
        // P4: the harness owns the lane data; the roll borrows it for the
        // frame (exactly what the app does with the project's control clips).
        let lane_views: Vec<OverlayLane<'_>> = lanes
            .iter()
            .map(|lane| OverlayLane {
                clip: lane.clip,
                data: &lane.data,
                label: lane.label.clone(),
            })
            .collect();
        let mut intent = None;
        let mut out = ctx.run_ui(raw, |ctx| {
            // Same order as the app: the router resolves hover focus before
            // the widgets run.
            router.begin_frame(ctx.ctx());
            egui::CentralPanel::default().show(ctx, |ui| {
                intent = glitcherz_ui::piano_roll::show(
                    ui,
                    tracks,
                    &lane_views,
                    subject,
                    state,
                    0.0,
                    false,
                    glitcherz_model::TICKS_PER_BAR,
                    router,
                );
            });
        });
        out.textures_delta.clear();
        intent
    }

    /// Screen position of a tick / pitch pair.
    fn pos(&self, tick: f32, pitch: i32) -> egui::Pos2 {
        egui::pos2(
            self.left + tick * self.ticks_to_x,
            self.y60 + (60 - pitch) as f32 * ROW_HEIGHT,
        )
    }

    /// A full press-drag-release; returns every intent the frames emitted.
    fn drag(&mut self, from: egui::Pos2, to: egui::Pos2) -> Vec<RollIntent> {
        self.drag_with(from, to, egui::Modifiers::default())
    }

    fn drag_with(
        &mut self,
        from: egui::Pos2,
        to: egui::Pos2,
        modifiers: egui::Modifiers,
    ) -> Vec<RollIntent> {
        // egui 0.36 carries modifier state as its own event, so it has to
        // be announced alongside the pointer events.
        let announce = egui::Event::ModifiersChanged(modifiers);
        let mut intents = Vec::new();
        // Leading hover-only frame: egui needs the pointer to be over the
        // widget before the press for the interaction to register.
        intents.extend(self.frame(vec![announce.clone(), hover(from)]));
        intents.extend(self.frame(vec![
            announce.clone(),
            hover(from),
            press(from, true, modifiers),
        ]));
        // The first move frame starts the drag (and carries its setup
        // intent, if any); a second one is where the drag emits its
        // increment — a real drag has many move frames.
        intents.extend(self.frame(vec![announce.clone(), hover(to)]));
        intents.extend(self.frame(vec![announce.clone(), hover(to)]));
        intents.extend(self.frame(vec![announce, hover(to), press(to, false, modifiers)]));
        intents
    }

    /// A click (press + release, no movement). Four frames, matching what
    /// a real platform delivers: hover, press, hold, release.
    fn click(&mut self, pos: egui::Pos2) -> Vec<RollIntent> {
        let mut intents = Vec::new();
        intents.extend(self.frame(vec![hover(pos)]));
        intents.extend(self.frame(vec![
            hover(pos),
            press(pos, true, egui::Modifiers::default()),
        ]));
        intents.extend(self.frame(vec![hover(pos)]));
        intents.extend(self.frame(vec![
            hover(pos),
            press(pos, false, egui::Modifiers::default()),
        ]));
        intents
    }

    fn tap_key(&mut self, key: egui::Key) -> Option<RollIntent> {
        self.frame(vec![key_event(key, egui::Modifiers::default())])
    }

    fn tap_key_with(&mut self, key: egui::Key, modifiers: egui::Modifiers) -> Option<RollIntent> {
        self.frame(vec![
            egui::Event::ModifiersChanged(modifiers),
            key_event(key, modifiers),
        ])
    }

    /// A modal transform: the pointer parks at `from` (the grab point),
    /// the command key arms it, the pointer sweeps to `to` (an optional
    /// qualifier key tapped mid-modal), and a click confirms. Returns
    /// every intent the frames emitted — the confirm intent is last.
    fn modal(
        &mut self,
        key: egui::Key,
        from: egui::Pos2,
        to: egui::Pos2,
        qualifier: Option<egui::Key>,
    ) -> Vec<RollIntent> {
        let mut intents = Vec::new();
        intents.extend(self.frame(vec![hover(from)]));
        intents.extend(self.tap_key(key));
        intents.extend(self.frame(vec![hover(to)]));
        if let Some(q) = qualifier {
            intents.extend(self.tap_key(q));
        }
        intents.extend(self.click(to));
        intents
    }

    /// Screen position of `(tick, value)` inside overlay lane `index` (P4).
    ///
    /// Derived, not probed: the lane stack starts right below the roll and
    /// the velocity strip, and `y60` pins the roll's own top (the calibrated
    /// row begins one pixel above the first pixel that hits, because of
    /// `HIT_SLOP`).
    fn lane_pos(&self, index: usize, tick: f32, value: f32) -> egui::Pos2 {
        // The roll publishes the lane rects it just laid out — no geometry is
        // re-derived here (the stack sits below the roll *and* the velocity
        // strip, and both heights are the widget's business).
        let rect = self.state.lane_rects[index];
        let (min, max, _) = self.lanes[index].axis;
        let norm = ((value - min) / (max - min)).clamp(0.0, 1.0);
        egui::pos2(
            rect.left() + tick * self.ticks_to_x,
            rect.bottom() - norm * rect.height(),
        )
    }

    /// The confirming intent of a modal sweep (the last MoveNotes /
    /// ScaleNotes / Resize intent a `modal()` run produced).
    fn confirm_of(intents: &[RollIntent]) -> Option<&RollIntent> {
        intents.iter().rev().find(|intent| {
            matches!(
                intent,
                RollIntent::MoveNotes { .. }
                    | RollIntent::ScaleNotes { .. }
                    | RollIntent::ResizeEnd { .. }
                    | RollIntent::ResizeStart { .. }
            )
        })
    }
}

fn hover(pos: egui::Pos2) -> egui::Event {
    egui::Event::PointerMoved(pos)
}

fn press(pos: egui::Pos2, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers,
    }
}

fn key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

fn shift() -> egui::Modifiers {
    egui::Modifiers {
        shift: true,
        ..Default::default()
    }
}

fn ctrl() -> egui::Modifiers {
    egui::Modifiers {
        ctrl: true,
        ..Default::default()
    }
}

fn has_add_note(intents: &[RollIntent]) -> bool {
    intents
        .iter()
        .any(|intent| matches!(intent, RollIntent::AddNote { .. }))
}

fn has_move(intents: &[RollIntent]) -> bool {
    intents
        .iter()
        .any(|intent| matches!(intent, RollIntent::MoveNotes { .. }))
}

// ---- Persistent mode: Select vs Draw -----------------------------------------------------

#[test]
fn select_mode_empty_drag_marquees_instead_of_painting() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    // A drag across empty space (pitch 70, far from both notes).
    let intents = f.drag(f.pos(1000.0, 70), f.pos(1400.0, 75));
    assert!(
        !has_add_note(&intents),
        "M3.8's 'empty drag always draws' rule is abolished: got {intents:?}"
    );
    assert!(
        f.state.selection.is_empty(),
        "the band covered nothing, so the selection is empty"
    );
}

#[test]
fn select_mode_empty_drag_marquee_selects_what_it_covers() {
    let mut f = Fixture::new();
    f.state.selection = vec![];
    // The band spans ticks 0..800 and rows 58..64: both notes.
    f.drag(f.pos(0.0, 64), f.pos(800.0, 58));
    assert_eq!(f.state.selection, vec![0, 1], "the band covered both notes");
}

#[test]
fn draw_mode_paints_on_an_empty_drag_and_d_toggles_back() {
    let mut f = Fixture::new();
    f.tap_key(egui::Key::D);
    assert_eq!(f.state.mode, RollMode::Draw);

    let intents = f.drag(f.pos(1000.0, 70), f.pos(1400.0, 70));
    assert!(
        intents.iter().any(|intent| matches!(
            intent,
            RollIntent::AddNote {
                note: 70,
                start: 960,
                ..
            }
        )),
        "draw mode paints at the press point: got {intents:?}"
    );
    assert_eq!(
        f.state.mode,
        RollMode::Draw,
        "painting does not leave draw mode"
    );

    // `d` again toggles back; an empty drag no longer paints.
    f.tap_key(egui::Key::D);
    assert_eq!(f.state.mode, RollMode::Select);
    let intents = f.drag(f.pos(1000.0, 70), f.pos(1400.0, 70));
    assert!(!has_add_note(&intents), "got {intents:?}");
}

#[test]
fn esc_leaves_draw_mode() {
    let mut f = Fixture::new();
    f.tap_key(egui::Key::D);
    f.tap_key(egui::Key::Escape);
    assert_eq!(f.state.mode, RollMode::Select);
    let intents = f.drag(f.pos(1000.0, 70), f.pos(1400.0, 70));
    assert!(!has_add_note(&intents), "got {intents:?}");
}

// ---- Modal command family: Grab ------------------------------------------------------------

#[test]
fn a_modal_grab_follows_the_pointer_and_confirms_with_a_click() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];

    let intents = f.modal(egui::Key::G, f.pos(240.0, 61), f.pos(960.0, 55), None);
    let confirmed = Fixture::confirm_of(&intents).expect("the click confirms the move");
    match confirmed {
        RollIntent::MoveNotes {
            delta_start,
            delta_note,
            gesture: Some(_),
            ..
        } => {
            assert_eq!(
                (*delta_start, *delta_note),
                (720, -6),
                "a free move carries both axes"
            );
        }
        other => panic!("expected a move, got {other:?}"),
    }
    assert!(!f.state.modal_active(), "the confirm clears the modal");
}

#[test]
fn a_modal_grab_locks_the_x_axis() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];

    // X pressed mid-modal: only the time component survives.
    let intents = f.modal(
        egui::Key::G,
        f.pos(240.0, 61),
        f.pos(960.0, 55),
        Some(egui::Key::X),
    );
    match Fixture::confirm_of(&intents).expect("the click confirms") {
        RollIntent::MoveNotes {
            delta_start,
            delta_note,
            ..
        } => {
            assert_eq!(*delta_note, 0, "X locks the pitch axis");
            assert_eq!(*delta_start, 720, "time still moves");
        }
        other => panic!("expected a move, got {other:?}"),
    }
}

#[test]
fn a_modal_grab_locks_the_y_axis() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];

    let intents = f.modal(
        egui::Key::G,
        f.pos(240.0, 61),
        f.pos(960.0, 55),
        Some(egui::Key::Y),
    );
    match Fixture::confirm_of(&intents).expect("the click confirms") {
        RollIntent::MoveNotes {
            delta_start,
            delta_note,
            ..
        } => {
            assert_eq!(*delta_start, 0, "Y locks the time axis");
            assert_eq!(*delta_note, -6, "pitch still moves");
        }
        other => panic!("expected a move, got {other:?}"),
    }
}

#[test]
fn an_axis_lock_can_switch_mid_modal() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.frame(vec![hover(f.pos(240.0, 61))]);
    f.tap_key(egui::Key::G);
    assert!(f.state.modal_active(), "the grab is modal right away");

    // Sweep freely, then lock time mid-modal and keep sweeping.
    f.frame(vec![hover(f.pos(480.0, 55))]);
    f.tap_key(egui::Key::X);
    f.frame(vec![hover(f.pos(720.0, 55))]);
    let intents = f.click(f.pos(720.0, 55));
    match Fixture::confirm_of(&intents).expect("the click confirms") {
        RollIntent::MoveNotes {
            delta_start,
            delta_note,
            ..
        } => {
            // The lock zeroes the pitch target: the confirm applies only
            // the time component (snap(720) - snap(240)).
            assert_eq!((*delta_start, *delta_note), (480, 0));
        }
        other => panic!("expected a move, got {other:?}"),
    }
}

// ---- Modal command family: Scale ------------------------------------------------------------

#[test]
fn a_modal_scale_scales_around_the_bbox_start() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1]; // bbox [0..720], pivot = 0 (the start)

    // Grab 720 ticks right of the pivot; the pointer twice as far doubles.
    let intents = f.modal(egui::Key::S, f.pos(720.0, 70), f.pos(1440.0, 70), None);
    match Fixture::confirm_of(&intents).expect("the click confirms") {
        RollIntent::ScaleNotes {
            pivot_tick, factor, ..
        } => {
            assert_eq!(*pivot_tick, 0, "the pivot is the bbox start");
            assert!((factor - 2.0).abs() < 0.01, "got {factor}");
        }
        other => panic!("expected a scale, got {other:?}"),
    }
    assert!(!f.state.modal_active());
}

// ---- Modal command family: Stretch -----------------------------------------------------------

#[test]
fn a_modal_stretch_pulls_the_end_edge_by_default() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1]; // bbox [0..720]

    let intents = f.modal(egui::Key::T, f.pos(240.0, 61), f.pos(1200.0, 61), None);
    match Fixture::confirm_of(&intents).expect("the click confirms") {
        RollIntent::ResizeEnd { delta_ticks, .. } => {
            assert_eq!(*delta_ticks, 480, "bbox right edge 720 -> 1200");
        }
        other => panic!("expected an end stretch, got {other:?}"),
    }
}

#[test]
fn t_s_switches_the_stretch_to_the_start_edge_mid_modal() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];

    let intents = f.modal(
        egui::Key::T,
        f.pos(240.0, 61),
        f.pos(480.0, 61),
        Some(egui::Key::S),
    );
    match Fixture::confirm_of(&intents).expect("the click confirms") {
        RollIntent::ResizeStart { delta_ticks, .. } => {
            assert_eq!(*delta_ticks, 480, "bbox left edge 0 -> 480");
        }
        other => panic!("expected a start trim, got {other:?}"),
    }
}

// ---- Command family: the knife (K) -----------------------------------------------------------

#[test]
fn the_knife_cuts_without_any_selection() {
    let mut f = Fixture::new();
    f.state.selection.clear(); // the knife is a position tool, not a selection one
    f.tap_key(egui::Key::K);
    assert!(f.state.knife_armed, "K arms without a selection");

    let intents = f.click(f.pos(120.0, 60)); // inside note A [0..240]
    assert!(
        intents
            .iter()
            .any(|intent| matches!(intent, RollIntent::SplitNotes { cuts, .. } if cuts.as_slice() == [(0, 120)])),
        "every note spanning the tick is cut: got {intents:?}"
    );
    assert!(!f.state.knife_armed, "one click, one cut batch");
}

#[test]
fn a_knife_stroke_cuts_along_the_swept_path() {
    let mut f = Fixture::new();
    f.state.selection.clear();
    f.tap_key(egui::Key::K);

    // Sweep along note A's row: one cut, in the middle of the covered span
    // ([60..180] ticks -> 120).
    let intents = f.drag(f.pos(60.0, 60), f.pos(180.0, 60));
    let cuts: Vec<_> = intents
        .iter()
        .filter(|intent| matches!(intent, RollIntent::SplitNotes { .. }))
        .collect();
    assert_eq!(cuts.len(), 1, "the batch lands on release: got {intents:?}");
    assert!(
        matches!(&cuts[0], RollIntent::SplitNotes { cuts, .. } if cuts.as_slice() == [(0, 120)]),
        "got {:?}",
        cuts[0]
    );
    assert!(!f.state.knife_armed, "one stroke per arming");
}

#[test]
fn a_knife_stroke_across_two_rows_cuts_both_notes() {
    let mut f = Fixture::new();
    f.state.selection.clear();

    // Stroke 1 sweeps along A's row ([60..180] -> mid 120); re-arm;
    // stroke 2 sweeps along B's row ([520..680] -> mid 600).
    f.tap_key(egui::Key::K);
    let intents = f.drag(f.pos(60.0, 60), f.pos(180.0, 60));
    assert!(
        intents
            .iter()
            .any(|intent| matches!(intent, RollIntent::SplitNotes { cuts, .. } if cuts.as_slice() == [(0, 120)])),
        "got {intents:?}"
    );
    f.tap_key(egui::Key::K);
    let intents = f.drag(f.pos(520.0, 62), f.pos(680.0, 62));
    assert!(
        intents
            .iter()
            .any(|intent| matches!(intent, RollIntent::SplitNotes { cuts, .. } if cuts.as_slice() == [(1, 600)])),
        "got {intents:?}"
    );
}

#[test]
fn a_knife_click_with_nothing_under_it_is_a_no_op() {
    let mut f = Fixture::new();
    f.state.selection.clear();
    f.tap_key(egui::Key::K);
    f.click(f.pos(1000.0, 70)); // no note spans tick 1000
    assert_eq!(
        f.state.take_status(),
        Some("Cut: nothing under the knife".to_string())
    );
    assert!(!f.state.knife_armed, "the arming is consumed either way");
}

// ---- Command family: Duplicate ----------------------------------------------------------------

#[test]
fn shift_d_duplicates_and_moves_the_copies_in_a_modal() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];

    f.frame(vec![hover(f.pos(240.0, 61))]); // the grab point
    let armed = f.tap_key_with(egui::Key::D, shift());
    let dup_gesture = match &armed {
        Some(RollIntent::Duplicate { gesture, .. }) => *gesture,
        other => panic!("expected a duplicate, got {other:?}"),
    };
    assert!(f.state.modal_active(), "the copies move in a modal Grab");

    // Sweep + confirm: the confirming move shares the duplicate's gesture
    // id, so copy + move is one undo step.
    f.frame(vec![hover(f.pos(480.0, 61))]);
    let intents = f.click(f.pos(480.0, 61));
    let confirm_gesture = intents.iter().find_map(|intent| match intent {
        RollIntent::MoveNotes {
            delta_start,
            gesture: Some(g),
            ..
        } => Some((*delta_start, *g)),
        _ => None,
    });
    assert_eq!(
        confirm_gesture,
        Some((240, dup_gesture)),
        "the copy and the move are one gesture: got {intents:?}"
    );
}

// ---- Command family edge cases -----------------------------------------------------------------

#[test]
fn transforms_on_an_empty_selection_are_no_ops_with_a_message() {
    let mut f = Fixture::new();
    f.state.selection = vec![];
    for (key, message) in [
        (egui::Key::G, "Grab: nothing selected"),
        (egui::Key::S, "Scale: nothing selected"),
        (egui::Key::T, "Stretch: nothing selected"),
    ] {
        f.tap_key(key);
        assert!(!f.state.modal_active(), "{key:?} must not arm");
        assert_eq!(f.state.take_status(), Some(message.to_string()));
    }
    // K needs no selection: it arms, and an empty click explains itself.
    f.tap_key(egui::Key::K);
    assert!(f.state.knife_armed);
    assert_eq!(f.state.take_status(), None);
    f.click(f.pos(1000.0, 70));
    assert_eq!(
        f.state.take_status(),
        Some("Cut: nothing under the knife".to_string())
    );

    // And an empty drag stays a marquee — nothing is painted or moved.
    let intents = f.drag(f.pos(1000.0, 70), f.pos(1400.0, 70));
    assert!(intents.is_empty(), "got {intents:?}");
}

#[test]
fn esc_cancels_the_modal_before_anything_is_applied() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.frame(vec![hover(f.pos(240.0, 61))]);
    f.tap_key(egui::Key::G);
    f.tap_key(egui::Key::Escape);
    assert!(!f.state.modal_active());

    // The drag that would have moved the selection is a plain marquee now.
    let intents = f.drag(f.pos(1000.0, 70), f.pos(1400.0, 70));
    assert!(
        !has_move(&intents),
        "a cancelled modal must not run: got {intents:?}"
    );
}

#[test]
fn rmb_cancels_the_modal_instead_of_deleting() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.frame(vec![hover(f.pos(240.0, 61))]);
    f.tap_key(egui::Key::G);
    f.frame(vec![hover(f.pos(480.0, 61))]);

    // RMB press + release over note A.
    let pos = f.pos(60.0, 60);
    f.frame(vec![hover(pos)]);
    f.frame(vec![
        hover(pos),
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        },
    ]);
    f.frame(vec![hover(pos)]);
    let intents = f.frame(vec![
        hover(pos),
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        },
    ]);
    assert!(
        !intents
            .iter()
            .any(|intent| matches!(intent, RollIntent::DeleteNotes { .. })),
        "RMB during a modal cancels, never deletes: got {intents:?}"
    );
    assert!(!f.state.modal_active(), "the modal is cancelled");
    assert_eq!(f.state.selection, vec![0, 1]);
}

#[test]
fn a_click_confirms_the_modal_rather_than_selecting() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.frame(vec![hover(f.pos(240.0, 61))]);
    f.tap_key(egui::Key::G);

    // A click on note A confirms the transform; it never re-selects.
    let intents = f.click(f.pos(60.0, 60));
    assert!(
        has_move(&intents),
        "the click is the confirmation: got {intents:?}"
    );
    assert_eq!(
        f.state.selection,
        vec![0, 1],
        "a confirming click does not change the selection"
    );
}

#[test]
fn other_keys_are_ignored_while_a_modal_owns_the_keyboard() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.frame(vec![hover(f.pos(240.0, 61))]);
    f.tap_key(egui::Key::T);
    f.tap_key(egui::Key::G); // ignored: the family never chains
    assert!(f.state.modal_active(), "the stretch modal survives");

    // ...but the qualifier still applies: `s` flips the stretch to its
    // start edge.
    f.frame(vec![hover(f.pos(480.0, 61))]);
    f.tap_key(egui::Key::S);
    let intents = f.click(f.pos(480.0, 61));
    assert!(
        intents.iter().any(|intent| matches!(
            intent,
            RollIntent::ResizeStart {
                delta_ticks: 480,
                ..
            }
        )),
        "got {intents:?}"
    );
}

#[test]
fn d_is_ignored_during_a_modal_and_toggles_after_cancelling() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.frame(vec![hover(f.pos(240.0, 61))]);
    f.tap_key(egui::Key::G);
    f.tap_key(egui::Key::D);
    assert!(f.state.modal_active(), "d does not chain into a modal");
    assert_eq!(f.state.mode, RollMode::Select, "nor does it toggle");

    f.tap_key(egui::Key::Escape);
    f.tap_key(egui::Key::D);
    assert_eq!(f.state.mode, RollMode::Draw, "free again after the cancel");
}

#[test]
fn the_command_family_runs_in_draw_mode_too() {
    let mut f = Fixture::new();
    f.tap_key(egui::Key::D);
    f.state.selection = vec![0, 1];
    let intents = f.modal(
        egui::Key::G,
        f.pos(240.0, 61),
        f.pos(960.0, 55),
        Some(egui::Key::X),
    );
    assert!(
        intents
            .iter()
            .any(|intent| matches!(intent, RollIntent::MoveNotes { delta_note: 0, .. })),
        "got {intents:?}"
    );
    assert_eq!(
        f.state.mode,
        RollMode::Draw,
        "a command never changes the persistent mode"
    );
}

// ---- Keyboard hygiene: Delete, arrows, key consumption ------------------------------------------

#[test]
fn the_delete_key_removes_the_selection() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    let intent = f.tap_key(egui::Key::Delete);
    assert!(
        matches!(intent, Some(RollIntent::DeleteNotes { ref indices }) if indices.as_slice() == [0, 1]),
        "got {intent:?}"
    );
}

#[test]
fn arrows_nudge_repeatedly_without_losing_focus() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    // Two taps in a row: the first must not move focus away from the roll
    // (egui's arrow focus navigation), or the second would be swallowed.
    let first = f.tap_key(egui::Key::ArrowUp);
    let second = f.tap_key(egui::Key::ArrowUp);
    assert!(
        matches!(&first, Some(RollIntent::MoveNotes { delta_note: 1, .. })),
        "got {first:?}"
    );
    assert!(
        matches!(&second, Some(RollIntent::MoveNotes { delta_note: 1, .. })),
        "the second tap must still nudge: got {second:?}"
    );
}

#[test]
fn keyboard_ownership_tracks_egui_focus_exactly() {
    // The app gates the virtual piano on `owns_keyboard_focus`; it must
    // mirror egui's own focus state with no frame lag and no staleness.
    let mut f = Fixture::new();
    // The fixture's calibration clicks already focus the roll.
    assert!(
        f.state.owns_keyboard_focus(&f.ctx),
        "a focused roll owns the keyboard"
    );
    if let Some(id) = f.state.focus_id() {
        f.ctx.memory_mut(|m| m.surrender_focus(id));
    }
    // Park the pointer off the roll (over the toolbar — not a registered
    // keyboard surface) BEFORE settling: with hover-follow focus, a pointer
    // still over the roll re-acquires focus the very next frame by design.
    f.frame(vec![hover(egui::pos2(10.0, 20.0))]);
    assert!(
        !f.state.owns_keyboard_focus(&f.ctx),
        "a roll without focus does not own the keyboard"
    );
}

// ---- Gesture shortcuts (the unlimited forms of G / T / Shift+D) ---------------------------

#[test]
fn the_gesture_shortcuts_still_work_without_arming_anything() {
    let mut f = Fixture::new();
    f.state.selection = vec![0];
    // Drag the note body: a free move, both axes.
    let intents = f.drag(f.pos(60.0, 60), f.pos(300.0, 64));
    let moved = intents.iter().find_map(|intent| match intent {
        RollIntent::MoveNotes {
            delta_start,
            delta_note,
            ..
        } => Some((*delta_start, *delta_note)),
        _ => None,
    });
    assert_eq!(moved, Some((240, 4)), "got {intents:?}");
    assert!(!f.state.modal_active(), "a gesture never arms a command");

    // Drag the note's right edge: an end trim.
    f.state.selection = vec![0];
    let intents = f.drag(f.pos(236.0, 60), f.pos(480.0, 60));
    let trimmed = intents.iter().rev().find_map(|intent| match intent {
        RollIntent::ResizeEnd { delta_ticks, .. } => Some(*delta_ticks),
        _ => None,
    });
    assert_eq!(trimmed, Some(240), "note end 240 -> 480: got {intents:?}");
}

#[test]
fn ctrl_drag_still_copies_the_selection() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    let intents = f.drag_with(f.pos(60.0, 60), f.pos(300.0, 60), ctrl());
    assert!(
        intents
            .iter()
            .any(|intent| matches!(intent, RollIntent::Duplicate { .. })),
        "got {intents:?}"
    );
    assert!(
        has_move(&intents),
        "the copies are dragged: got {intents:?}"
    );
}

/// P3 (spec `roll-multiclip.md` D6): a ghost clip is painted but has no
/// presence in the hit test. Clicking a ghost note must land on empty space —
/// if it ever selects the *focus* clip's note at that index, the ghost leaked
/// into `note_at`.
#[test]
fn a_ghost_note_is_painted_but_never_hit() {
    let mut f = Fixture::new();
    let bar = TICKS_PER_BAR;
    // Ghost clip one bar after the focus, same pitch row as note A.
    f.tracks[0].clips.push(MidiClip {
        start_ticks: bar,
        span_ticks: bar,
        loop_ticks: bar,
        sequence: Sequence {
            notes: vec![Note {
                start: 0,
                length: 240,
                note: 60,
                ..Note::default()
            }],
            ..Sequence::default()
        },
        ..MidiClip::default()
    });
    f.subject = RollSubject::assemble(ClipRef::midi(0, 0), &[ClipRef::midi(0, 1)], |_| 2, |_| 0);

    // The fit now spans both clips, so a tick is half the single-clip scale
    // the fixture measured.
    let t_x = f.ticks_to_x / 2.0;
    // Sanity first: the focus note is still selectable at its own position,
    // so a failure below cannot be blamed on a stale geometry.
    f.click(egui::pos2(f.left + 5.0, f.y60));
    assert_eq!(f.state.selection, vec![0], "the focus note must still hit");

    // The ghost's note body, one bar to the right.
    let x = f.left + bar as f32 * t_x + 5.0;
    f.click(egui::pos2(x, f.y60));
    assert!(
        f.state.selection.is_empty(),
        "a ghost note must not be selectable (selection {:?})",
        f.state.selection
    );
}

/// P3 (spec `roll-multiclip.md` D2): the view may scroll past the focus clip,
/// but drawing stays inside it. A draw drag over a ghost creates nothing —
/// otherwise the note would land clamped to tick 0, far from the pointer.
#[test]
fn drawing_stops_at_the_focus_clips_edges() {
    let mut f = Fixture::new();
    let bar = TICKS_PER_BAR;
    f.tracks[0].clips.push(MidiClip {
        start_ticks: bar,
        span_ticks: bar,
        loop_ticks: bar,
        ..MidiClip::default()
    });
    f.subject = RollSubject::assemble(ClipRef::midi(0, 0), &[ClipRef::midi(0, 1)], |_| 2, |_| 0);
    f.state.mode = RollMode::Draw;
    // Two clips in the fit ⇒ half the single-clip pixel scale.
    let t_x = f.ticks_to_x / 2.0;
    let (left, y60) = (f.left, f.y60);
    let at = move |tick: f32| egui::pos2(left + tick * t_x, y60);

    // Sanity: inside the focus clip a draw drag creates a note.
    let inside = f.drag(at(1000.0), at(1200.0));
    assert!(
        inside
            .iter()
            .any(|intent| matches!(intent, RollIntent::AddNote { .. })),
        "inside the clip a draw must create: got {inside:?}"
    );

    // One bar to the right — over the ghost — nothing is created.
    let outside = f.drag(at(bar as f32 + 100.0), at(bar as f32 + 300.0));
    assert!(
        !outside
            .iter()
            .any(|intent| matches!(intent, RollIntent::AddNote { .. })),
        "a draw over a ghost must not create: got {outside:?}"
    );
}

// ---------------------------------------------------------------------------
// P4: overlay curve lanes
// ---------------------------------------------------------------------------

/// A fixture with one automation lane: a ±2-semitone pitch curve with an
/// anchor at each end, exactly what a bend clip looks like.
fn lane_fixture() -> Fixture {
    let mut fixture = Fixture::new();
    let data = ControlClip {
        start_ticks: 0,
        span_ticks: TICKS_PER_BAR,
        loop_ticks: TICKS_PER_BAR,
        curve: vec![
            AutoPoint {
                tick: 0,
                value: -2.0,
            },
            AutoPoint {
                tick: 960,
                value: 2.0,
            },
        ],
        ..ControlClip::default()
    };
    fixture.lanes.push(LaneFixture {
        clip: ClipRef::control(0, 0),
        data,
        axis: (-2.0, 2.0, false),
        label: "Pitch Bend ±2.0 st".to_string(),
    });
    // One idle frame lays the lanes out, so `lane_rects` is populated for the
    // pointer math below (the fixture measures geometry by *running* frames).
    let _ = fixture.frame(vec![]);
    fixture
}

/// The lane shares the roll's time axis: the same tick sits at the same x in
/// the lane and in the note area (this is the whole point of P4).
#[test]
fn a_lane_shares_the_time_axis_with_the_notes() {
    let f = lane_fixture();
    for tick in [0.0, 960.0, 1920.0] {
        assert_eq!(
            f.lane_pos(0, tick, 0.0).x,
            f.pos(tick, 60).x,
            "tick {tick} must line up"
        );
    }
}

#[test]
fn a_lane_click_adds_an_anchor_at_the_clicked_value() {
    let mut f = lane_fixture();
    let pos = f.lane_pos(0, 1920.0, 0.0);
    let intents = f.click(pos);
    let upsert = intents
        .iter()
        .rev()
        .find_map(|intent| match intent {
            RollIntent::CurveUpsert {
                track,
                clip,
                from,
                tick,
                value,
                dragging,
            } => Some((*track, *clip, *from, *tick, *value, *dragging)),
            _ => None,
        })
        .expect("a lane click must upsert an anchor");
    assert_eq!((upsert.0, upsert.1), (0, 0), "writes its own control clip");
    assert_eq!(upsert.2, None, "the click created the anchor");
    assert_eq!(upsert.3, 1920);
    assert!(
        upsert.4.abs() < 0.1,
        "the middle of a ±2 axis is 0, got {}",
        upsert.4
    );
    assert!(!upsert.5, "a click is a single undo step");
}

#[test]
fn dragging_a_lane_anchor_reports_the_anchor_it_moved() {
    let mut f = lane_fixture();
    let from = f.lane_pos(0, 0.0, -2.0);
    let to = f.lane_pos(0, 1440.0, 0.0);
    let intents = f.drag(from, to);
    assert!(
        intents.iter().any(|intent| matches!(
            intent,
            RollIntent::CurveUpsert {
                from: Some(0),
                dragging: true,
                ..
            }
        )),
        "the drag must move the anchor that was grabbed: {intents:?}"
    );
    assert!(
        intents
            .iter()
            .any(|intent| matches!(intent, RollIntent::CurveDragEnd)),
        "the release closes the merged undo gesture: {intents:?}"
    );
    // The anchor lands where the pointer was released, still as one of the
    // moved-from frames (the harness sends two move frames; a real drag sends
    // one per frame, each tracking the anchor the previous frame left behind).
    assert!(
        intents.iter().any(|intent| matches!(
            intent,
            RollIntent::CurveUpsert {
                tick: 1440,
                from: Some(0),
                dragging: true,
                ..
            }
        )),
        "the drag lands on the release tick: {intents:?}"
    );
}

#[test]
fn a_lane_right_click_deletes_the_anchor_under_it() {
    let mut f = lane_fixture();
    let pos = f.lane_pos(0, 960.0, 2.0);
    let button = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Secondary,
        pressed,
        modifiers: egui::Modifiers::default(),
    };
    let intents = [
        f.frame(vec![hover(pos)]),
        f.frame(vec![hover(pos), button(true)]),
        f.frame(vec![hover(pos)]),
        f.frame(vec![hover(pos), button(false)]),
    ];
    let removed = intents
        .iter()
        .flatten()
        .find_map(|intent| match intent {
            RollIntent::CurveRemove { track, clip, tick } => Some((*track, *clip, *tick)),
            _ => None,
        })
        .expect("right-click must delete an anchor");
    assert_eq!(removed, (0, 0, 960), "the anchor under the pointer goes");
}

/// P5: the caption's toggle folds the lane down to its header — and that
/// click must not also drop an anchor on the curve underneath.
#[test]
fn the_lane_toggle_collapses_the_lane() {
    let mut f = lane_fixture();
    let toggle_at = |f: &Fixture| {
        let rect = f.state.lane_rects[0];
        egui::pos2(rect.left() + 9.0, rect.top() + 9.0)
    };
    let pos = toggle_at(&f);
    let intents = f.click(pos);
    assert!(
        !intents
            .iter()
            .any(|intent| matches!(intent, RollIntent::CurveUpsert { .. })),
        "the toggle is not a curve edit: {intents:?}"
    );
    assert_eq!(f.state.collapsed_lanes, vec![0]);
    // The rect lags by one frame: layout runs before the click is handled, so
    // the fold shows up on the next pass (one frame in the app).
    let _ = f.frame(vec![]);
    assert!(
        (f.state.lane_rects[0].height() - CURVE_LANE_COLLAPSED_HEIGHT).abs() < 0.5,
        "collapsed height = {}",
        f.state.lane_rects[0].height()
    );

    // Clicking the same square again restores the lane.
    let pos = toggle_at(&f);
    let _ = f.click(pos);
    assert!(f.state.collapsed_lanes.is_empty());
    let _ = f.frame(vec![]);
    assert!(
        f.state.lane_rects[0].height() > CURVE_LANE_COLLAPSED_HEIGHT,
        "expanded again: {}",
        f.state.lane_rects[0].height()
    );
}
