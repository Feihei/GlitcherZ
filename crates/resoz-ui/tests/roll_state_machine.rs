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
use resoz_model::{MidiClip, Note, Sequence, TICKS_PER_BAR};
use resoz_ui::piano_roll::{
    Axis, PendingCmd, ROW_HEIGHT, RollIntent, RollMode, RollState, StretchEdge,
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
    clip: MidiClip,
    state: RollState,
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
            ctx, clip, state, ..
        } = self;
        let mut intent = None;
        let mut out = ctx.run_ui(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                intent = resoz_ui::piano_roll::show(ui, clip, state, 0.0, false);
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

// ---- One-shot command family: Grab ---------------------------------------------------------

#[test]
fn armed_grab_moves_the_selection_with_an_x_axis_lock() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.tap_key(egui::Key::G);
    f.tap_key(egui::Key::X);
    assert_eq!(
        f.state.pending,
        Some(PendingCmd::Grab {
            axis: Some(Axis::Time)
        })
    );

    // A diagonal drag: only the time component may survive.
    let intents = f.drag(f.pos(240.0, 61), f.pos(960.0, 55));
    let moves: Vec<_> = intents
        .iter()
        .filter(|intent| matches!(intent, RollIntent::MoveNotes { .. }))
        .collect();
    assert!(
        !moves.is_empty(),
        "an armed grab must move: got {intents:?}"
    );
    for intent in moves {
        let RollIntent::MoveNotes {
            delta_start,
            delta_note,
            ..
        } = intent
        else {
            unreachable!()
        };
        assert_eq!(*delta_note, 0, "X locks the pitch axis");
        assert!(*delta_start > 0, "time still moves");
    }
    assert_eq!(f.state.pending, None, "the command is consumed by its drag");
}

#[test]
fn armed_grab_moves_the_selection_with_a_y_axis_lock() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.tap_key(egui::Key::G);
    f.tap_key(egui::Key::Y);

    let intents = f.drag(f.pos(240.0, 61), f.pos(960.0, 55));
    let moves: Vec<_> = intents
        .iter()
        .filter(|intent| matches!(intent, RollIntent::MoveNotes { .. }))
        .collect();
    assert!(!moves.is_empty(), "got {intents:?}");
    for intent in moves {
        let RollIntent::MoveNotes {
            delta_start,
            delta_note,
            ..
        } = intent
        else {
            unreachable!()
        };
        assert_eq!(*delta_start, 0, "Y locks the time axis");
        assert!(*delta_note < 0, "pitch still moves");
    }
}

#[test]
fn an_axis_lock_can_switch_mid_drag() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.tap_key(egui::Key::G);

    // First frames: a free diagonal move.
    let mut intents = Vec::new();
    intents.extend(f.frame(vec![hover(f.pos(240.0, 61))]));
    intents.extend(f.frame(vec![
        hover(f.pos(240.0, 61)),
        press(f.pos(240.0, 61), true, egui::Modifiers::default()),
    ]));
    intents.extend(f.frame(vec![hover(f.pos(480.0, 55))]));
    let free = intents.iter().find_map(|intent| match intent {
        RollIntent::MoveNotes {
            delta_start,
            delta_note,
            ..
        } => Some((*delta_start, *delta_note)),
        _ => None,
    });
    assert_eq!(free, Some((240, -6)), "a free move carries both axes");

    // Lock time mid-drag: the pitch walks back to the grab position. The
    // compensation lands on the key frame itself — the drag keeps
    // reporting `dragged` there, so no pointer movement is needed.
    let locked = f.tap_key(egui::Key::X).and_then(|intent| match intent {
        RollIntent::MoveNotes {
            delta_start,
            delta_note,
            ..
        } => Some((delta_start, delta_note)),
        _ => None,
    });
    assert_eq!(
        locked,
        Some((0, 6)),
        "the locked axis compensates back to the grab pitch"
    );

    // …and the free axis keeps tracking the pointer afterwards.
    let moved = f
        .frame(vec![hover(f.pos(720.0, 55))])
        .and_then(|intent| match intent {
            RollIntent::MoveNotes {
                delta_start,
                delta_note,
                ..
            } => Some((delta_start, delta_note)),
            _ => None,
        });
    assert_eq!(
        moved,
        Some((240, 0)),
        "time still moves, pitch stays locked"
    );
}

// ---- One-shot command family: Scale --------------------------------------------------------

#[test]
fn armed_scale_stretches_the_selection_around_the_bbox_center() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1]; // bbox [0..720], pivot 360
    f.tap_key(egui::Key::S);

    // Grab 360 ticks right of the pivot, then drag twice as far.
    let intents = f.drag(f.pos(720.0, 70), f.pos(1080.0, 70));
    let scaled = intents.iter().find_map(|intent| match intent {
        RollIntent::ScaleNotes {
            pivot_tick, factor, ..
        } => Some((*pivot_tick, *factor)),
        _ => None,
    });
    assert_eq!(scaled, Some((360, 2.0)), "got {intents:?}");
    assert_eq!(f.state.pending, None, "the command is consumed by its drag");
}

// ---- One-shot command family: Stretch ------------------------------------------------------

#[test]
fn armed_stretch_pulls_the_end_edge_by_default() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1]; // bbox [0..720]
    f.tap_key(egui::Key::T);

    let intents = f.drag(f.pos(240.0, 61), f.pos(1200.0, 61));
    let last = intents.iter().rev().find_map(|intent| match intent {
        RollIntent::ResizeEnd { delta_ticks, .. } => Some(*delta_ticks),
        _ => None,
    });
    assert_eq!(
        last,
        Some(480),
        "bbox right edge 720 → 1200: got {intents:?}"
    );
}

#[test]
fn t_s_switches_the_stretch_to_the_start_edge() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.tap_key(egui::Key::T);
    f.tap_key(egui::Key::S);
    assert_eq!(
        f.state.pending,
        Some(PendingCmd::Stretch {
            edge: StretchEdge::Start
        })
    );

    let intents = f.drag(f.pos(240.0, 61), f.pos(480.0, 61));
    let last = intents.iter().rev().find_map(|intent| match intent {
        RollIntent::ResizeStart { delta_ticks, .. } => Some(*delta_ticks),
        _ => None,
    });
    assert_eq!(last, Some(480), "bbox left edge 0 → 480: got {intents:?}");
}

// ---- One-shot command family: Cut ----------------------------------------------------------

#[test]
fn armed_cut_splits_the_selection_at_the_pointer() {
    let mut f = Fixture::new();
    f.state.selection = vec![0]; // note [0..240]
    f.tap_key(egui::Key::K);

    let intents = f.click(f.pos(120.0, 60));
    assert!(
        intents
            .iter()
            .any(|intent| matches!(intent, RollIntent::SplitNotes { tick: 120, .. })),
        "got {intents:?}"
    );
    assert_eq!(f.state.pending, None, "one invocation, one cut");
}

#[test]
fn a_cut_drag_cuts_once_at_the_press_point() {
    let mut f = Fixture::new();
    f.state.selection = vec![0];
    f.tap_key(egui::Key::K);

    let intents = f.drag(f.pos(120.0, 60), f.pos(600.0, 60));
    let cuts: Vec<_> = intents
        .iter()
        .filter(|intent| matches!(intent, RollIntent::SplitNotes { .. }))
        .collect();
    assert_eq!(cuts.len(), 1, "one stroke = one cut: got {intents:?}");
    assert!(
        matches!(cuts[0], RollIntent::SplitNotes { tick: 120, .. }),
        "the cut lands at the press point, not the release"
    );
    assert_eq!(f.state.pending, None);
}

#[test]
fn a_cut_with_nothing_under_the_pointer_is_a_no_op() {
    let mut f = Fixture::new();
    f.state.selection = vec![0];
    f.tap_key(egui::Key::K);
    f.click(f.pos(1000.0, 70)); // no selected note spans tick 1000
    assert_eq!(
        f.state.take_status(),
        Some("Cut: no selected note at the pointer".to_string())
    );
    assert_eq!(f.state.pending, None, "the command is consumed either way");
}

// ---- One-shot command family: Duplicate ----------------------------------------------------

#[test]
fn shift_d_duplicates_and_arms_grab() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];

    let armed = f.tap_key_with(egui::Key::D, shift());
    assert!(
        matches!(armed, Some(RollIntent::Duplicate { .. })),
        "got {armed:?}"
    );
    assert_eq!(
        f.state.pending,
        Some(PendingCmd::Grab { axis: None }),
        "the duplicate arms Grab on the copies"
    );

    // The drag that follows moves the copies (the copy and the move share
    // the gesture id, so it is all one undo step).
    let intents = f.drag(f.pos(240.0, 61), f.pos(480.0, 61));
    assert!(has_move(&intents), "got {intents:?}");
    assert_eq!(f.state.pending, None);
}

// ---- Command family edge cases -------------------------------------------------------------

#[test]
fn transforms_on_an_empty_selection_are_no_ops_with_a_message() {
    let mut f = Fixture::new();
    f.state.selection = vec![];
    for (key, message) in [
        (egui::Key::G, "Grab: nothing selected"),
        (egui::Key::S, "Scale: nothing selected"),
        (egui::Key::T, "Stretch: nothing selected"),
        (egui::Key::K, "Cut: nothing selected"),
    ] {
        f.tap_key(key);
        assert_eq!(f.state.pending, None, "{key:?} must not arm");
        assert_eq!(f.state.take_status(), Some(message.to_string()));
    }
    // And an empty drag stays a marquee — nothing is painted or moved.
    let intents = f.drag(f.pos(1000.0, 70), f.pos(1400.0, 70));
    assert!(intents.is_empty(), "got {intents:?}");
}

#[test]
fn esc_cancels_the_armed_command_before_it_runs() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.tap_key(egui::Key::G);
    f.tap_key(egui::Key::Escape);
    assert_eq!(f.state.pending, None);

    // The drag that would have moved the selection is a plain marquee now.
    let intents = f.drag(f.pos(1000.0, 70), f.pos(1400.0, 70));
    assert!(
        !has_move(&intents),
        "a cancelled command must not run: got {intents:?}"
    );
}

#[test]
fn a_plain_click_cancels_an_armed_transform() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.tap_key(egui::Key::G);
    f.click(f.pos(60.0, 60)); // note A's body
    assert_eq!(f.state.pending, None);
    assert_eq!(f.state.selection, vec![0], "the click still selects");
}

#[test]
fn other_keys_are_ignored_while_a_command_is_pending() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.tap_key(egui::Key::T);
    f.tap_key(egui::Key::G); // ignored: the family never chains
    assert_eq!(
        f.state.pending,
        Some(PendingCmd::Stretch {
            edge: StretchEdge::End
        })
    );
    // ...but the qualifier still applies.
    f.tap_key(egui::Key::S);
    assert_eq!(
        f.state.pending,
        Some(PendingCmd::Stretch {
            edge: StretchEdge::Start
        })
    );
}

#[test]
fn d_cancels_a_pending_transform_and_toggles_the_mode() {
    let mut f = Fixture::new();
    f.state.selection = vec![0, 1];
    f.tap_key(egui::Key::G);
    f.tap_key(egui::Key::D);
    assert_eq!(
        f.state.pending, None,
        "the state command cancels the transform"
    );
    assert_eq!(f.state.mode, RollMode::Draw);
}

#[test]
fn the_command_family_runs_in_draw_mode_too() {
    let mut f = Fixture::new();
    f.tap_key(egui::Key::D);
    f.state.selection = vec![0, 1];
    f.tap_key(egui::Key::G);
    f.tap_key(egui::Key::X);

    let intents = f.drag(f.pos(240.0, 61), f.pos(960.0, 55));
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
    assert_eq!(f.state.pending, None, "a gesture never arms a command");

    // Drag the note's right edge: an end trim.
    f.state.selection = vec![0];
    let intents = f.drag(f.pos(236.0, 60), f.pos(480.0, 60));
    let trimmed = intents.iter().rev().find_map(|intent| match intent {
        RollIntent::ResizeEnd { delta_ticks, .. } => Some(*delta_ticks),
        _ => None,
    });
    assert_eq!(trimmed, Some(240), "note end 240 → 480: got {intents:?}");
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
