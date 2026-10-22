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
use glitcherz_model::{MidiClip, Note, Sequence, TICKS_PER_BAR};
use glitcherz_ui::piano_roll::{ROW_HEIGHT, RollIntent, RollMode, RollState};

const SCREEN: egui::Vec2 = egui::vec2(800.0, 600.0);

/// A roll fixture: one clip with two notes, a live egui context, and the
/// measured screen geometry.
///
/// - note A: pitch 60, ticks [0..240]
/// - note B: pitch 62, ticks [480..720]
/// - loop: one bar (3840 ticks), default grid 1/16 (240 ticks)
struct Fixture {
    ctx: egui::Context,
    clip: MidiClip,
    state: RollState,
    /// Hover-follow focus arbiter, mirroring the app's per-frame flow.
    router: glitcherz_ui::focus::FocusRouter,
    /// Roll rect left edge (the roll spans the panel's full width).
    left: f32,
    ticks_to_x: f32,
    /// Screen y of pitch 60's row (calibrated, not derived).
    y60: f32,
    time: f64,
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
            clip,
            state: RollState::new(),
            router: glitcherz_ui::focus::FocusRouter::new(),
            left: 0.0,
            ticks_to_x: 0.0,
            y60: 0.0,
            time: 0.0,
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
        self.ticks_to_x = rect.width() / self.clip.loop_ticks as f32;
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
            clip,
            state,
            router,
            ..
        } = self;
        let mut intent = None;
        let mut out = ctx.run_ui(raw, |ctx| {
            // Same order as the app: the router resolves hover focus before
            // the widgets run.
            router.begin_frame(ctx.ctx());
            egui::CentralPanel::default().show(ctx, |ui| {
                intent = glitcherz_ui::piano_roll::show(
                    ui,
                    clip,
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
