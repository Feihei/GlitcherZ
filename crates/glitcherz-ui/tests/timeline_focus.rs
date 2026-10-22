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
    fn frame(&mut self, events: Vec<egui::Event>) {
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
        let muted: [bool; 0] = [];
        let loop_range = glitcherz_model::SongLoop::default();
        let mut out = ctx.run_ui(raw, |root| {
            // Same order as the app: the router resolves hover focus before
            // the panels run; the transport + tab bar are neutral chrome.
            router.begin_frame(root.ctx());
            let transport = egui::Panel::top("glitcherz_transport").show(root, |ui| {
                ui.horizontal(|ui| {
                    let _ = ui.button("▶ Play").clicked();
                    let _ = ui.button("■ Stop").clicked();
                    ui.separator();
                    ui.label("Tempo");
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
                let _ = timeline::show(
                    ui,
                    state,
                    &*tracks,
                    &[],
                    TimelineParams {
                        muted: &muted,
                        selected_track: 0,
                        selected_auto: 0,
                        playhead_ticks: 0.0,
                        playing: false,
                        edit_target: None,
                        loop_range: &loop_range,
                        markers: &[],
                        bar_ticks: glitcherz_model::TICKS_PER_BAR,
                    },
                    router,
                );
            });
            // Chrome registration, mirroring the app (transport + tabs).
            router.register_neutral(transport.response.rect);
        });
        out.textures_delta.clear();
    }

    /// A click (press + release, no movement). Four frames, matching what
    /// a real platform delivers: hover, press, hold, release.
    fn click(&mut self, pos: egui::Pos2) {
        self.frame(vec![hover(pos)]);
        self.frame(vec![hover(pos), press(pos, true)]);
        self.frame(vec![hover(pos)]);
        self.frame(vec![hover(pos), press(pos, false)]);
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
            .read_response(egui::Id::new("glitcherz_timeline_row").with((false, track)))
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
        .read_response(egui::Id::new("glitcherz_timeline_row").with((false, 0usize)))
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
        .read_response(egui::Id::new("glitcherz_timeline_row").with((false, 0usize)))
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
    egui::Event::Key {
        key: k,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::default(),
    }
}
