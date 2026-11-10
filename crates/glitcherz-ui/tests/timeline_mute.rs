//! Clip mute (B2): `Ctrl` + middle click toggles one clip, `H` mutes the
//! selection, `Alt+H` reveals (unmutes) the whole arrangement.
//!
//! These tests drive the real `timeline::show` through egui with synthetic
//! input — nothing here mocks the widget layer, so they fail if the button
//! binding, the focus gate or the reported action regresses. The view never
//! mutates the document: every case asserts on the [`TimelineAction`] the app
//! would apply.

use eframe::egui;
use glitcherz_model::{AudioTrack, ControlTrack, MidiClip, MidiTrack, TICKS_PER_BAR};
use glitcherz_ui::timeline::{
    self, ClipSel, TimelineAction, TimelineLane, TimelineParams, TimelineState,
};

const SCREEN: egui::Vec2 = egui::vec2(800.0, 600.0);
/// Probe x: past the 108 px header column and inside the first bar of the
/// lane (the clip starts at tick 0, one bar is 56 px wide).
const LANE_X: f32 = 150.0;

struct Fixture {
    ctx: egui::Context,
    state: TimelineState,
    router: glitcherz_ui::focus::FocusRouter,
    tracks: Vec<MidiTrack>,
    controls: Vec<ControlTrack>,
    audios: Vec<AudioTrack>,
    time: f64,
    modifiers: egui::Modifiers,
}

impl Fixture {
    fn new() -> Self {
        // Two clips on one MIDI track: one under the probe line, one later in
        // the arrangement (so reveal-all has something to count).
        let mut tracks = vec![MidiTrack::default()];
        tracks[0].clips.push(MidiClip {
            start_ticks: 0,
            span_ticks: 2 * TICKS_PER_BAR,
            ..MidiClip::default()
        });
        tracks[0].clips.push(MidiClip {
            start_ticks: 6 * TICKS_PER_BAR,
            span_ticks: TICKS_PER_BAR,
            ..MidiClip::default()
        });
        Self {
            ctx: egui::Context::default(),
            state: TimelineState::default(),
            router: glitcherz_ui::focus::FocusRouter::new(),
            tracks,
            controls: Vec::new(),
            audios: Vec::new(),
            time: 0.0,
            modifiers: egui::Modifiers::default(),
        }
    }

    fn raw(&self, mut events: Vec<egui::Event>) -> egui::RawInput {
        // egui tracks the held modifier state through `ModifiersChanged`
        // (winit sends it on every modifier transition); the pointer handlers
        // read that state, so a Ctrl+middle click needs it, not just the
        // event's own snapshot.
        events.insert(0, egui::Event::ModifiersChanged(self.modifiers));
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

    /// One frame, returning the actions the timeline reported.
    fn frame(&mut self, events: Vec<egui::Event>) -> Vec<TimelineAction> {
        self.time += 0.1;
        let raw = self.raw(events);
        let Self {
            ctx,
            state,
            router,
            tracks,
            controls,
            audios,
            ..
        } = self;
        let loop_range = glitcherz_model::SongLoop::default();
        let mut actions = Vec::new();
        let mut out = ctx.run_ui(raw, |root| {
            egui::CentralPanel::default().show(root, |ui| {
                actions = timeline::show(
                    ui,
                    state,
                    &*tracks,
                    controls.as_slice(),
                    audios.as_slice(),
                    TimelineParams {
                        selected_track: 0,
                        selected_auto: 0,
                        selected_audio: 0,
                        playhead_ticks: 0.0,
                        playing: false,
                        edit_target: None,
                        loop_range: &loop_range,
                        markers: &[],
                        bar_ticks: TICKS_PER_BAR,
                        sec_per_tick: 60.0 / (120.0 * 960.0),
                        waveform_of: &|_| None,
                        clip_groups: &[],
                    },
                    router,
                );
            });
        });
        out.textures_delta.clear();
        actions
    }

    /// A click with an explicit button (hover, press, hold, release).
    fn click(&mut self, pos: egui::Pos2, button: egui::PointerButton) -> Vec<TimelineAction> {
        let _ = self.frame(vec![hover(pos)]);
        let _ = self.frame(vec![hover(pos), press(pos, button, true)]);
        let _ = self.frame(vec![hover(pos)]);
        self.frame(vec![hover(pos), press(pos, button, false)])
    }

    /// Clicks down the probe line until a left click lands on the clip (the
    /// view reports it by selecting it) and returns that y.
    fn lane_y_on_the_clip(&mut self) -> f32 {
        let mut y = 60.0;
        while y < SCREEN.y - 40.0 {
            let _ = self.click(egui::pos2(LANE_X, y), egui::PointerButton::Primary);
            if !self.state.selected().is_empty() {
                return y;
            }
            y += 4.0;
        }
        panic!("no timeline lane answered a click on the probe line");
    }
}

fn hover(pos: egui::Pos2) -> egui::Event {
    egui::Event::PointerMoved(pos)
}

fn press(pos: egui::Pos2, button: egui::PointerButton, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers: egui::Modifiers::default(),
    }
}

fn key(k: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key: k,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

/// Ctrl + middle click on a clip reports the toggle; the same gesture again
/// (with the clip now muted) reports the inverse. A plain middle click — the
/// pan button — never toggles.
#[test]
fn ctrl_middle_click_toggles_the_clip_under_the_pointer() {
    let mut f = Fixture::new();
    let y = f.lane_y_on_the_clip();
    let pos = egui::pos2(LANE_X, y);

    // Plain middle click: pan only, no mute.
    let actions = f.click(pos, egui::PointerButton::Middle);
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, TimelineAction::SetClipMuted { .. })),
        "a plain middle click must not toggle mute, got {actions:?}"
    );

    f.modifiers = egui::Modifiers {
        ctrl: true,
        ..egui::Modifiers::default()
    };
    let actions = f.click(pos, egui::PointerButton::Middle);
    let expected = ClipSel {
        lane: TimelineLane::Midi,
        track: 0,
        clip: 0,
    };
    assert!(
        actions.iter().any(
            |a| matches!(a, TimelineAction::SetClipMuted { clips, muted }
                if *muted && clips == &vec![expected])
        ),
        "Ctrl+middle click must mute the clip under the pointer, got {actions:?}"
    );

    // The app would now write the flag into the clip table.
    f.tracks[0].clips[0].muted = true;
    let actions = f.click(pos, egui::PointerButton::Middle);
    assert!(
        actions.iter().any(
            |a| matches!(a, TimelineAction::SetClipMuted { clips, muted }
                if !*muted && clips == &vec![expected])
        ),
        "the second gesture must clear the flag, got {actions:?}"
    );
}

/// `H` mutes the selection, `Alt+H` reveals every muted clip in the
/// arrangement. Both are focus-gated like G / T / K (plain `H` is also a
/// virtual-piano key).
#[test]
fn h_mutes_the_selection_and_alt_h_reveals_everything() {
    let mut f = Fixture::new();
    let _ = f.lane_y_on_the_clip();
    // Let the focus grant settle (the view reads focus from the frame before).
    for _ in 0..3 {
        let _ = f.frame(vec![]);
    }
    assert!(
        f.state.owns_keyboard_focus(&f.ctx),
        "the clicked lane must own the keyboard before the shortcut is live"
    );

    let actions = f.frame(vec![key(egui::Key::H, egui::Modifiers::default())]);
    assert!(
        actions.iter().any(
            |a| matches!(a, TimelineAction::SetClipMuted { clips, muted }
            if *muted
                && clips
                    == &vec![ClipSel {
                        lane: TimelineLane::Midi,
                        track: 0,
                        clip: 0
                    }])
        ),
        "H must mute the selected clip, got {actions:?}"
    );

    // Both clips muted (the app-level result of two such gestures).
    f.tracks[0].clips[0].muted = true;
    f.tracks[0].clips[1].muted = true;
    let alt = egui::Modifiers {
        alt: true,
        ..egui::Modifiers::default()
    };
    let actions = f.frame(vec![key(egui::Key::H, alt)]);
    assert!(
        actions.iter().any(
            |a| matches!(a, TimelineAction::SetClipMuted { clips, muted }
                if !*muted && clips.len() == 2)
        ),
        "Alt+H must reveal every muted clip, got {actions:?}"
    );

    // Nothing muted any more: the shortcut reports nothing and does not
    // invent an empty edit.
    f.tracks[0].clips[0].muted = false;
    f.tracks[0].clips[1].muted = false;
    let actions = f.frame(vec![key(egui::Key::H, alt)]);
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, TimelineAction::SetClipMuted { .. })),
        "reveal-all on an unmuted arrangement must be a no-op, got {actions:?}"
    );
}
