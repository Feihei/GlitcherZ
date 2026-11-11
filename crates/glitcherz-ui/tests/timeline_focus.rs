//! Timeline keyboard-focus regression coverage (M4b follow-up).
//!
//! The lane rows are the keyboard focus carriers, and focus must SURVIVE
//! layout changes around them. M4b initially drew the focus chip (and the
//! knife / modal hints and the status message) directly in the parent ui:
//! the chip appears exactly when focus is lost and disappears the frame
//! after a click grants it. That toggling shifted the auto ids of every
//! row below (egui auto ids are positional), so the focused lane's id
//! stopped existing and egui's dead-man switch cleared focus — the chip
//! came back, the ids shifted back, and no click could ever hold focus.
//!
//! The rows are now pinned with `UiBuilder::id` (an explicit id, immune
//! to sibling-count changes); these tests pin that property by letting
//! the layout settle for several frames after each interaction. They also
//! pin the row STACKING: the rows are laid out with a raw `new_child`,
//! which (unlike `ui.horizontal`) does not advance the parent cursor —
//! without the explicit `advance_cursor_after_rect` every row paints on
//! top of the previous one and only the last-drawn row is visible.

use eframe::egui;
use glitcherz_model::{MidiClip, MidiTrack, TICKS_PER_BAR};
use glitcherz_ui::timeline::{self, TimelineParams, TimelineState};

const SCREEN: egui::Vec2 = egui::vec2(800.0, 600.0);
/// Probe x: past the row header (108 px) and inside the lanes, away from
/// the toolbar buttons (which sit at the far left of the toolbar row).
const LANE_X: f32 = 500.0;

struct Fixture {
    ctx: egui::Context,
    state: TimelineState,
    /// Hover-follow focus arbiter, mirroring the app's per-frame flow.
    router: glitcherz_ui::focus::FocusRouter,
    tracks: Vec<MidiTrack>,
    /// View-tab selection (the app's MainView; only Timeline matters
    /// here, but the tab row must exist to mirror its layout).
    view: bool,
    /// Tempo DragValue target (the real transport panel has one — a
    /// focus-interested interactive widget).
    tempo: f64,
    /// A widget above the timeline whose presence varies per frame — the
    /// shape of whatever the app really does to churn the layout.
    churn: bool,
    time: f64,
}

impl Fixture {
    fn new() -> Self {
        let clip = MidiClip {
            start_ticks: 0,
            span_ticks: 2 * TICKS_PER_BAR,
            ..MidiClip::default()
        };
        let mut tracks = vec![MidiTrack::default(), MidiTrack::default()];
        tracks[0].clips.push(clip);
        Self {
            ctx: egui::Context::default(),
            state: TimelineState::default(),
            router: glitcherz_ui::focus::FocusRouter::new(),
            tracks,
            view: true,
            tempo: 120.0,
            churn: false,
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
            // Without an explicit screen rect egui lays out into a huge
            // default area and pointer interaction stops matching.
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
            viewports,
            ..Default::default()
        }
    }

    /// Runs one frame, mirroring the app's layout exactly: transport and
    /// status panels, the view tab bar, then the timeline inside a
    /// CentralPanel — the focus bug only reproduces in this nesting, not
    /// with the timeline alone in the central area. 0.1s per frame — same
    /// pinning as the roll harness: a whole click (4 steps = 0.4s) stays
    /// under egui's 0.8s click ceiling and exceeds its 0.3s double-click
    /// window.
    fn frame(&mut self, events: Vec<egui::Event>) -> Vec<glitcherz_ui::timeline::TimelineAction> {
        self.time += 0.1;
        let raw = self.raw(events);
        let Self {
            ctx,
            state,
            router,
            tracks,
            view,
            tempo,
            churn,
            ..
        } = self;
        let loop_range = glitcherz_model::SongLoop::default();
        let mut actions = Vec::new();
        let mut out = ctx.run_ui(raw, |root| {
            // Same order as the app: the router resolves hover focus before
            // the panels run; the transport + tab bar are neutral chrome.
            router.begin_frame(root.ctx());
            let transport = egui::Panel::top("glitcherz_transport").show(root, |ui| {
                ui.horizontal(|ui| {
                    // Mirrors the app's icon-only transport row.
                    let _ = ui.button("▶").clicked();
                    let _ = ui.button("⏹").clicked();
                    ui.separator();
                    let _ = ui.add(
                        egui::DragValue::new(tempo)
                            .range(40.0..=300.0)
                            .suffix(" BPM"),
                    );
                    ui.separator();
                    let _ = ui.button("📂 Open").clicked();
                    let _ = ui.button("💾 Save").clicked();
                });
            });
            egui::Panel::bottom("glitcherz_status").show(root, |ui| {
                ui.label("status");
            });
            egui::Panel::top("glitcherz_views").show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(view, true, "Timeline");
                    ui.selectable_value(view, false, "Edit");
                });
            });
            egui::CentralPanel::default().show(root, |ui| {
                if *churn {
                    ui.weak("churn");
                }
                actions = timeline::show(
                    ui,
                    state,
                    &*tracks,
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
                        meter: &glitcherz_model::MeterMap::default(),
                        sec_per_tick: 60.0 / (120.0 * 960.0),
                        waveform_of: &|_| None,
                        clip_groups: &[],
                    },
                    router,
                );
            });
            // Chrome registration, mirroring the app (transport + tabs).
            router.register_neutral(transport.response.rect);
        });
        out.textures_delta.clear();
        actions
    }

    /// A click (press + release, no movement). Four frames, matching what
    /// a real platform delivers: hover, press, hold, release.
    fn click(&mut self, pos: egui::Pos2) {
        self.frame(vec![hover(pos)]);
        self.frame(vec![hover(pos), press(pos, true)]);
        self.frame(vec![hover(pos)]);
        self.frame(vec![hover(pos), press(pos, false)]);
    }

    /// A press-drag-release along one row; returns every action the frames
    /// emitted.
    fn drag(&mut self, from: egui::Pos2, to: egui::Pos2) -> Vec<timeline::TimelineAction> {
        let mut actions = self.frame(vec![hover(from)]);
        actions.extend(self.frame(vec![hover(from), press(from, true)]));
        actions.extend(self.frame(vec![hover(to)]));
        actions.extend(self.frame(vec![hover(to)]));
        actions.extend(self.frame(vec![hover(to), press(to, false)]));
        actions
    }

    fn owns_focus(&self) -> bool {
        self.state.owns_keyboard_focus(&self.ctx)
    }

    /// Clicks down the probe line until a click lands in a lane whose
    /// focus survives the settling frames afterwards (the frame after the
    /// click hides the "no keyboard focus" chip — a parent-ui widget
    /// disappearing). Returns the lane y.
    fn click_into_a_lane(&mut self) -> f32 {
        let mut y = 60.0;
        while y < 560.0 {
            self.click(egui::pos2(LANE_X, y));
            if self.owns_focus() {
                let mut held = true;
                for _ in 0..4 {
                    self.frame(vec![]);
                    held &= self.owns_focus();
                }
                if held {
                    return y;
                }
            }
            y += 4.0;
        }
        panic!("no timeline lane held keyboard focus after a click");
    }
}

/// A click into a lane grants focus, and focus must survive the layout
/// settling afterwards. If the row ids were positional auto ids, the chip
/// disappearing the next frame shifted them and egui's dead-man switch
/// cleared focus — the "always yellow, clicks do nothing" bug.
#[test]
fn a_click_holds_focus_across_the_following_frames() {
    let mut f = Fixture::new();
    let _ = f.click_into_a_lane();
}

/// Status messages and hints appear as parent-ui widgets while a lane
/// holds focus (e.g. G on an empty selection queues a status message that
/// renders next frame and vanishes the frame after). Focus must survive
/// both the appearing and the disappearing.
#[test]
fn appearing_status_and_hints_do_not_steal_the_keyboard() {
    let mut f = Fixture::new();
    let y = f.click_into_a_lane();
    // G on an empty selection is a no-op that queues a status message
    // ("nothing selected — G / Shift+D / T need a selection").
    f.frame(vec![hover(egui::pos2(LANE_X, y)), key(egui::Key::G)]);
    for _ in 0..3 {
        f.frame(vec![]);
        assert!(
            f.owns_focus(),
            "focus died while the status/hint widgets were appearing"
        );
    }
}

/// The timeline's own variable hints (knife, modal, status messages) come
/// and go while a lane holds focus. Before this was pinned they were
/// drawn directly in the parent ui, so a hint appearing shifted every
/// row's positional auto id: focus stayed parked on a phantom id, every
/// focus-gated key went dead, and the "no keyboard focus" chip came back
/// no matter how often you clicked. K tapped every frame toggles its hint
/// and exercises exactly that churn — it is also consumed by the focused
/// row, so the test covers keys reaching the timeline at all.
#[test]
fn focus_survives_the_rows_own_hints_coming_and_going() {
    let mut f = Fixture::new();
    let y = f.click_into_a_lane();
    for _ in 0..4 {
        f.frame(vec![hover(egui::pos2(LANE_X, y)), key(egui::Key::K)]);
        assert!(
            f.owns_focus(),
            "focus died while the timeline's own hints were coming and going"
        );
    }
}

/// The toolbar (zoom, add/remove track) sits above the lanes, so clicking
/// one of its buttons is a click *outside* the focused lane for egui, which
/// surrenders the lane's focus (`SurrenderFocusOn::Clicks`). The hover-follow
/// router only hands focus back once the pointer is inside a lane — with the
/// pointer parked on the button that was just clicked, G / T / Shift+D /
/// Delete / Esc stayed dead. The toolbar row is registered as part of the
/// focused row's surface, so the keys survive the click.
#[test]
fn clicking_a_toolbar_button_keeps_the_lanes_keyboard_focus() {
    let mut f = Fixture::new();
    let _ = f.click_into_a_lane();
    assert!(f.owns_focus(), "a clicked lane holds the keyboard");
    // Find a toolbar button by probing the row above the lanes (the layout
    // stack puts it below the transport and view-tab panels): a zoom button
    // is the observable one (`px_per_bar` moves).
    let mut hit = None;
    'outer: for y in (36..160).step_by(4) {
        for x in (2..320).step_by(4) {
            let before = f.state.px_per_bar;
            f.click(egui::pos2(x as f32, y as f32));
            if f.state.px_per_bar != before {
                hit = Some(egui::pos2(x as f32, y as f32));
                break 'outer;
            }
        }
    }
    let at = hit.expect("no toolbar button changed the zoom");
    // The router resolves at the start of a frame, so the pointer resting on
    // the button is what must restore the focus — exactly the user's state
    // right after clicking a tool.
    f.frame(vec![hover(at)]);
    assert!(
        f.owns_focus(),
        "the toolbar click must not hand the keyboard away while the pointer is still on it"
    );
    for _ in 0..3 {
        f.frame(vec![hover(at)]);
        assert!(
            f.owns_focus(),
            "and it must stay while the pointer rests there"
        );
    }
}

/// Anything appearing or disappearing ABOVE the timeline must not re-id
/// the rows: egui keeps focus on the id, so a moved row leaves focus
/// parked on a phantom — no row reports focus, the keys stop working and
/// the "no keyboard focus" chip comes back after every click.
#[test]
fn focus_survives_layout_churn_above_the_timeline() {
    let mut f = Fixture::new();
    let _ = f.click_into_a_lane();
    for i in 0..4 {
        f.churn = i % 2 == 0;
        f.frame(vec![]);
        assert!(
            f.owns_focus(),
            "focus died when the layout above the timeline changed"
        );
    }
}

/// The rows must stack vertically, each on its own line. The rows are
/// laid out with a raw `new_child`, which does not advance the parent
/// cursor; without the explicit `advance_cursor_after_rect` every row
/// laid out at the same spot, painted over the previous one, and only
/// the last-drawn row was visible (the "only track 4 remains" bug).
/// Reads each row container's rect back out of egui's widget stack.
#[test]
fn rows_stack_vertically_instead_of_overlapping() {
    let mut f = Fixture::new();
    f.frame(vec![]);
    f.frame(vec![]);
    let rect = |track: usize| {
        f.ctx
            .read_response(egui::Id::unique("glitcherz_timeline_row").with((false, track)))
            .unwrap_or_else(|| panic!("row {track} is missing from the widget stack"))
            .rect
    };
    let first = rect(0);
    let second = rect(1);
    assert!(
        second.top() >= first.bottom() - 0.5,
        "rows overlap: {first:?} vs {second:?}"
    );
}

/// The user flow that reported "commands don't work": click a clip to
/// select it (which also focuses the lane), then press G — the move
/// modal must arm rather than flash "nothing selected".
#[test]
fn clicking_a_clip_then_g_arms_the_move_modal() {
    let mut f = Fixture::new();
    f.frame(vec![]);
    f.frame(vec![]);
    let row = f
        .ctx
        .read_response(egui::Id::unique("glitcherz_timeline_row").with((false, 0usize)))
        .expect("row 0 is missing from the widget stack")
        .rect;
    // The fixture clip covers bars 0-2 from the lane start; the lane
    // begins right of the ~108 px header, so a probe a little past that
    // lands inside the clip.
    let pos = egui::pos2(row.left() + 130.0, row.center().y);
    f.click(pos);
    f.frame(vec![hover(pos)]);
    f.frame(vec![hover(pos), key(egui::Key::G)]);
    assert!(
        f.state.modal_active(),
        "G on a selected clip must arm the move modal"
    );
    // And focus must still be held for the modal keys (X / Esc / click).
    assert!(f.owns_focus());
}

/// 2026-11-04: Alt+T must arm the tile modal (not the plain-T stretch).
#[test]
fn alt_t_arms_the_tile_modal() {
    let mut f = Fixture::new();
    f.frame(vec![]);
    f.frame(vec![]);
    let row = f
        .ctx
        .read_response(egui::Id::unique("glitcherz_timeline_row").with((false, 0usize)))
        .expect("row 0 is missing from the widget stack")
        .rect;
    let pos = egui::pos2(row.left() + 130.0, row.center().y);
    f.click(pos);
    f.frame(vec![hover(pos)]);
    f.frame(vec![
        hover(pos),
        key_with(egui::Key::T, egui::Modifiers::ALT),
    ]);
    assert!(
        f.state.tiling_active(),
        "Alt+T on a selected clip must arm the tile modal"
    );
    assert!(f.owns_focus());
}

/// 2026-11-04: holding Alt can drop the egui cursor on Windows
/// (CursorLeft → PointerGone), so arming the tile modal must NOT require a
/// live pointer — it latches its grab on the first valid one instead. The
/// first user report was exactly this: the 1-bar refusal message proved the
/// key arrived, but the follow-up press on a 2-bar clip silently failed.
#[test]
fn alt_t_arms_tile_without_a_pointer() {
    let mut f = Fixture::new();
    f.frame(vec![]);
    f.frame(vec![]);
    let row = f
        .ctx
        .read_response(egui::Id::unique("glitcherz_timeline_row").with((false, 0usize)))
        .expect("row 0 is missing from the widget stack")
        .rect;
    let pos = egui::pos2(row.left() + 130.0, row.center().y);
    f.click(pos);
    f.frame(vec![hover(pos)]);
    // The key press arrives on a frame where the cursor is gone.
    f.frame(vec![
        egui::Event::PointerGone,
        key_with(egui::Key::T, egui::Modifiers::ALT),
    ]);
    assert!(
        f.state.tiling_active(),
        "Alt+T must arm the tile modal even when the pointer is unavailable"
    );
}

/// 2026-11-04 follow-up: after arming without a pointer (Alt drops the
/// cursor), the pointer must come back, latch the grab, and the confirming
/// click must land the tile as ONE action. Regression for "the hint bar
/// shows Tile but moving the mouse shows no ghost and the click does
/// nothing".
#[test]
fn alt_t_click_confirms_tile_after_pointer_returns() {
    let mut f = Fixture::new();
    f.frame(vec![]);
    f.frame(vec![]);
    let row = f
        .ctx
        .read_response(egui::Id::unique("glitcherz_timeline_row").with((false, 0usize)))
        .expect("row 0 is missing from the widget stack")
        .rect;
    let pos = egui::pos2(row.left() + 130.0, row.center().y);
    f.click(pos);
    // Arm with the cursor gone (Alt drops it on Windows).
    f.frame(vec![
        egui::Event::PointerGone,
        key_with(egui::Key::T, egui::Modifiers::ALT),
    ]);
    assert!(f.state.tiling_active(), "tile modal must be armed");
    // Pointer returns; the grab latches and the preview follows.
    f.frame(vec![hover(pos)]);
    assert!(
        f.state.tiling_active(),
        "modal must survive the pointer returning"
    );
    // Confirming click lands the tile.
    let mut actions = Vec::new();
    actions.extend(f.frame(vec![hover(pos), press(pos, true)]));
    actions.extend(f.frame(vec![hover(pos)]));
    actions.extend(f.frame(vec![hover(pos), press(pos, false)]));
    let tiled: Vec<_> = actions
        .iter()
        .filter_map(|a| match a {
            glitcherz_ui::timeline::TimelineAction::TileClipsBatch { tiles } => {
                Some((tiles[0].span_ticks, tiles[0].loop_ticks))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        tiled,
        vec![(
            2 * glitcherz_model::TICKS_PER_BAR,
            glitcherz_model::TICKS_PER_BAR
        )],
        "confirming click at the grab point keeps the already-tiled \
         clip unchanged (2-bar container, 1-bar period) — no nesting"
    );
}

/// 2026-11-04: the user's exact flow — confirm a T stretch, then Alt+T in
/// the very next instants. If the tile arm/ghost/confirm chain breaks after
/// a just-confirmed Stretch, this reproduces it.
#[test]
fn alt_t_works_right_after_a_confirmed_stretch() {
    let mut f = Fixture::new();
    f.frame(vec![]);
    f.frame(vec![]);
    let row = f
        .ctx
        .read_response(egui::Id::unique("glitcherz_timeline_row").with((false, 0usize)))
        .expect("row 0 is missing from the widget stack")
        .rect;
    let pos = egui::pos2(row.left() + 130.0, row.center().y);
    f.click(pos);
    // T stretch: arm, move, confirm (one-bar clip grows to two bars here in
    // the app; the action lands through the app layer, so the clip stays
    // 2 bars in the fixture — what matters is the modal handoff).
    f.frame(vec![hover(pos), key(egui::Key::T)]);
    assert!(f.state.modal_active(), "T must arm the stretch modal");
    // Confirming the stretch at the same spot still emits a (no-op) batch —
    // stretch_targets reports every selection clip unconditionally. What
    // matters here is the modal handoff below.
    let mut actions = Vec::new();
    actions.extend(f.frame(vec![hover(pos)]));
    actions.extend(f.frame(vec![hover(pos), press(pos, true)]));
    actions.extend(f.frame(vec![hover(pos)]));
    actions.extend(f.frame(vec![hover(pos), press(pos, false)]));
    assert!(actions.iter().all(|a| matches!(
        a,
        glitcherz_ui::timeline::TimelineAction::StretchClipsBatch { .. }
    )));
    // Immediately Alt+T with the cursor dropped by Alt.
    f.frame(vec![
        egui::Event::PointerGone,
        key_with(egui::Key::T, egui::Modifiers::ALT),
    ]);
    assert!(
        f.state.tiling_active(),
        "tile modal must arm after a stretch"
    );
    // Pointer returns and the user confirms.
    f.frame(vec![hover(pos)]);
    let mut actions = Vec::new();
    actions.extend(f.frame(vec![hover(pos), press(pos, true)]));
    actions.extend(f.frame(vec![hover(pos)]));
    actions.extend(f.frame(vec![hover(pos), press(pos, false)]));
    let tiled: Vec<_> = actions
        .iter()
        .filter_map(|a| match a {
            glitcherz_ui::timeline::TimelineAction::TileClipsBatch { tiles } => {
                Some((tiles[0].span_ticks, tiles[0].loop_ticks))
            }
            _ => None,
        })
        .collect();
    assert!(
        !tiled.is_empty(),
        "confirming click after a stretch must land the tile"
    );
}

/// 2026-11-04: Shift+T is the primary tile qualifier — on Windows Alt+letter
/// raises SC_KEYMENU whose menu mode swallows the following mouse events
/// (D29). The modal must arm even with the cursor gone, mirroring Alt+T.
#[test]
fn shift_t_arms_the_tile_modal() {
    let mut f = Fixture::new();
    f.frame(vec![]);
    f.frame(vec![]);
    let row = f
        .ctx
        .read_response(egui::Id::unique("glitcherz_timeline_row").with((false, 0usize)))
        .expect("row 0 is missing from the widget stack")
        .rect;
    let pos = egui::pos2(row.left() + 130.0, row.center().y);
    f.click(pos);
    f.frame(vec![
        egui::Event::PointerGone,
        key_with(egui::Key::T, egui::Modifiers::SHIFT),
    ]);
    assert!(
        f.state.tiling_active(),
        "Shift+T on a selected clip must arm the tile modal"
    );
}

/// 2026-11-04: double-clicking empty lane space inserts a clip AND selects
/// it, so G / T / Shift+D work right away without an extra pick.
#[test]
fn double_click_insert_selects() {
    let mut f = Fixture::new();
    f.frame(vec![]);
    f.frame(vec![]);
    let row = f
        .ctx
        .read_response(egui::Id::unique("glitcherz_timeline_row").with((false, 0usize)))
        .expect("row 0 is missing from the widget stack")
        .rect;
    let empty = egui::pos2(row.left() + 400.0, row.center().y);
    // A fast double click: two press/release pairs one frame apart, inside
    // egui's 0.3s multi-click window.
    let mut actions = Vec::new();
    actions.extend(f.frame(vec![hover(empty), press(empty, true)]));
    actions.extend(f.frame(vec![hover(empty), press(empty, false)]));
    actions.extend(f.frame(vec![hover(empty), press(empty, true)]));
    actions.extend(f.frame(vec![hover(empty), press(empty, false)]));
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, glitcherz_ui::timeline::TimelineAction::AddClip { .. })),
        "double-click on empty lane space must insert a clip"
    );
    let sel = f.state.selected();
    assert_eq!(sel.len(), 1, "the inserted clip must be the only selection");
    assert_eq!(sel[0].lane, glitcherz_ui::timeline::TimelineLane::Midi);
    assert_eq!(sel[0].track, 0);
    assert_eq!(sel[0].clip, 1);
}

/// Hover-follow focus (Blender-style context): merely hovering a lane
/// grants it the keyboard — no click needed.
#[test]
fn hovering_a_lane_grants_focus_without_a_click() {
    let mut f = Fixture::new();
    f.frame(vec![]);
    f.frame(vec![]);
    // Read row 0's real rect out of the widget stack instead of guessing a
    // y — the toolbar / ruler heights are egui's business.
    let row = f
        .ctx
        .read_response(egui::Id::unique("glitcherz_timeline_row").with((false, 0usize)))
        .expect("row 0 is missing from the widget stack")
        .rect;
    f.frame(vec![hover(egui::pos2(LANE_X, row.center().y))]);
    assert!(f.owns_focus(), "hovering a lane must grant it the keyboard");

    // Remote-desktop sessions deliver the pointer like a touch: after every
    // gesture egui-winit pushes `PointerGone` (CursorLeft / touch end), which
    // clears `latest_pos`. The router must fall back to the retained last
    // position, or hover-follow would be permanently dead in those sessions.
    f.frame(vec![egui::Event::PointerGone]);
    assert!(
        f.owns_focus(),
        "pointer-gone (remote session) must not drop the hover-follow focus"
    );
}

/// Hovering the neutral chrome (transport) releases the lane's focus; the
/// pointer is free (no button down), so the hover decision applies.
#[test]
fn hovering_the_transport_releases_the_lane() {
    let mut f = Fixture::new();
    let y = f.click_into_a_lane();
    assert!(f.owns_focus());
    f.frame(vec![hover(egui::pos2(60.0, 12.0))]); // inside the transport panel
    assert!(
        !f.owns_focus(),
        "moving to the transport must release the lane focused at y={y}"
    );
}

/// Hovering nothing registered is sticky: the last owner keeps the
/// keyboard. The row header (left of the lane surface) is not a registered
/// keyboard surface, so hovering it must not hand focus around — and must
/// not release it either.
#[test]
fn focus_is_sticky_over_unregistered_space() {
    let mut f = Fixture::new();
    let y = f.click_into_a_lane();
    // Over the row's header buttons, NOT over the registered lane surface.
    f.frame(vec![hover(egui::pos2(30.0, y))]);
    assert!(
        f.owns_focus(),
        "focus from the lane at y={y} must survive idle hover over unregistered ui"
    );
}

fn hover(pos: egui::Pos2) -> egui::Event {
    egui::Event::PointerMoved(pos)
}

fn press(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::default(),
    }
}

fn key(k: egui::Key) -> egui::Event {
    key_with(k, egui::Modifiers::default())
}

fn key_with(k: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key: k,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

/// The timeline's keyboard surface is the whole panel, not the lane strips.
///
/// The rows are only ~40 px tall at the top of a tall central area, so a
/// pointer parked anywhere below them used to be "outside every surface":
/// no focus, every focus-gated shortcut dead, and the keys fell through to
/// the virtual piano (the reported "timeline shortcuts don't work, keys play
/// notes" with a perfectly healthy pointer). The roll never had the bug
/// because it registers its whole panel — this pins the same rule here.
#[test]
fn hovering_below_the_last_row_keeps_the_timelines_keyboard() {
    let mut f = Fixture::new();
    f.frame(vec![]);
    f.frame(vec![]);
    let row = f
        .ctx
        .read_response(egui::Id::unique("glitcherz_timeline_row").with((false, 0usize)))
        .expect("row 0 is missing from the widget stack")
        .rect;
    // Below every row, still inside the central panel.
    let empty = egui::pos2(LANE_X, row.bottom() + 150.0);
    f.frame(vec![hover(empty)]);
    assert!(
        f.owns_focus(),
        "the panel — not just the lane strips — must own the timeline's keyboard"
    );
}

/// Same rule above the rows: the toolbar and the ruler belong to the panel
/// too. This used to work only AFTER some lane had been hovered once (the
/// old band was registered for `focus_id`, which starts out `None`), so a
/// fresh window could not be driven from its own toolbar.
#[test]
fn the_toolbar_belongs_to_the_panel_surface_from_the_first_frame() {
    let mut f = Fixture::new();
    f.frame(vec![]);
    f.frame(vec![]);
    let row = f
        .ctx
        .read_response(egui::Id::unique("glitcherz_timeline_row").with((false, 0usize)))
        .expect("row 0 is missing from the widget stack")
        .rect;
    // The ruler / toolbar strip just above the first row.
    let above = egui::pos2(LANE_X, row.top() - 20.0);
    f.frame(vec![hover(above)]);
    assert!(
        f.owns_focus(),
        "hovering the timeline's own toolbar must hand it the keyboard"
    );
}
/// Regression (2026-11-10): dragging a clip's edge must resize it. egui only
/// fires `drag_started` after the pointer travelled past its click distance,
/// so classifying by the *current* pointer put an edge drag outside the clip
/// and it fell through to the rubber band — the pointer cursor changed on
/// hover, but the drag selected instead of resizing.
#[test]
fn dragging_a_clip_edge_resizes_instead_of_starting_a_band() {
    let mut f = Fixture::new();
    let y = f.click_into_a_lane();

    // Walk right until the clicks stop selecting the clip: `edge` is the last
    // x still inside its right edge (one two-bar clip sits on track 0).
    let mut edge = 0.0;
    for x in (120..=700).step_by(2) {
        f.click(egui::pos2(x as f32, y));
        if f.state.selection.is_empty() {
            break;
        }
        edge = x as f32;
    }
    assert!(edge >= 120.0, "no clip was selected along the probe line");

    // Press on the edge (inside `EDGE_GRAB`), release more than a bar past
    // it: the press point is inside the clip, the current pointer is not.
    let to_x = edge + f.state.px_per_bar + 10.0;
    let actions = f.drag(egui::pos2(edge - 2.0, y), egui::pos2(to_x, y));
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, timeline::TimelineAction::MoveResizeClip { .. })),
        "an edge drag must resize, not start a rubber band: {actions:?}"
    );
}
