//! Timeline track rename: right-click a track's header cell, type, Enter.
//!
//! The editor is an inline singleline field drawn **in the header cell** — the
//! row is not re-laid-out while it is open — and it reports through
//! [`TimelineAction::RenameMidiTrack`], so the app applies it as one undoable
//! document edit. Esc cancels without emitting anything.
//!
//! These tests drive the real `timeline::show` through egui with synthetic
//! input; nothing here mocks the widget layer, so they fail if the right-click
//! binding, the focus hand-off or the commit condition regress.

use eframe::egui;
use glitcherz_model::{AudioTrack, ControlTrack, MidiTrack, TICKS_PER_BAR};
use glitcherz_ui::timeline::{self, TimelineAction, TimelineLane, TimelineParams, TimelineState};

const SCREEN: egui::Vec2 = egui::vec2(800.0, 600.0);
/// Inside a row's header cell: past the 12 px reorder grip, inside the name
/// button (the header column is 108 px wide).
const HEADER_X: f32 = 60.0;

struct Fixture {
    ctx: egui::Context,
    state: TimelineState,
    router: glitcherz_ui::focus::FocusRouter,
    tracks: Vec<MidiTrack>,
    controls: Vec<ControlTrack>,
    audios: Vec<AudioTrack>,
    time: f64,
}

impl Fixture {
    fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            state: TimelineState::default(),
            router: glitcherz_ui::focus::FocusRouter::new(),
            // Two unnamed tracks: the header shows the generated "Track N"
            // labels, which is exactly the "empty name" case the inline
            // editor has to round-trip.
            tracks: vec![MidiTrack::default(), MidiTrack::default()],
            // One row per non-MIDI kind, so the rename binding is covered for
            // all three tables (they share one header cell helper).
            controls: vec![ControlTrack::default()],
            audios: vec![AudioTrack::default()],
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
                        curve_axis: (0.0, 127.0, false),
                        waveform_of: &|_| None,
                    },
                    router,
                );
            });
        });
        out.textures_delta.clear();
        actions
    }

    /// A click with an explicit button (press, hold, release).
    fn click(&mut self, pos: egui::Pos2, button: egui::PointerButton) -> Vec<TimelineAction> {
        let _ = self.frame(vec![hover(pos)]);
        let _ = self.frame(vec![hover(pos), press(pos, button, true)]);
        let _ = self.frame(vec![hover(pos)]);
        self.frame(vec![hover(pos), press(pos, button, false)])
    }

    /// Scans the left column for the row whose click reports a selection
    /// matching `hit` and returns its y. Avoids hard-coding the toolbar/ruler
    /// heights.
    fn header_y(&mut self, hit: impl Fn(&TimelineAction) -> bool) -> f32 {
        let mut y = 40.0;
        while y < SCREEN.y - 40.0 {
            let actions = self.click(egui::pos2(HEADER_X, y), egui::PointerButton::Primary);
            if actions.iter().any(&hit) {
                return y;
            }
            y += 4.0;
        }
        panic!("no timeline header answered a click on the scan line");
    }

    fn midi_header_y(&mut self, index: usize) -> f32 {
        self.header_y(|a| matches!(a, TimelineAction::SelectTrack(t) if *t == index))
    }
}

/// Right-click a track name, type, Enter → the rename action carries the typed
/// name and the editor closes.
#[test]
fn right_click_types_and_enter_renames_the_track() {
    let mut f = Fixture::new();
    let y = f.midi_header_y(1);
    let _ = f.click(egui::pos2(HEADER_X, y), egui::PointerButton::Secondary);
    // The click lands one frame before the text field exists; that frame then
    // asks for focus and the next one has it.
    let _ = f.frame(vec![]);
    let _ = f.frame(vec![]);

    let actions = f.frame(vec![
        egui::Event::Text("Bass".to_owned()),
        key(egui::Key::Enter),
    ]);
    assert!(
        actions.iter().any(
            |a| matches!(a, TimelineAction::RenameTrack { lane, track, name }
                if *lane == TimelineLane::Midi && *track == 1 && name == "Bass")
        ),
        "expected RenameTrack {{ Midi, 1, \"Bass\" }}, got {actions:?}"
    );

    // The editor is closed: a following Enter does nothing.
    let actions = f.frame(vec![key(egui::Key::Enter)]);
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, TimelineAction::RenameTrack { .. })),
        "the editor must close after committing, got {actions:?}"
    );
}

/// All three row kinds share the header cell helper: a curve row reports into
/// the control table and an audio row into the audio table (the action carries
/// the lane, and the app writes the matching table).
#[test]
fn curve_and_audio_rows_rename_into_their_own_tables() {
    let mut f = Fixture::new();

    let y = f.header_y(|a| matches!(a, TimelineAction::SelectControlTrack(_)));
    let _ = f.click(egui::pos2(HEADER_X, y), egui::PointerButton::Secondary);
    let _ = f.frame(vec![]);
    let _ = f.frame(vec![]);
    let actions = f.frame(vec![
        egui::Event::Text("Cutoff".to_owned()),
        key(egui::Key::Enter),
    ]);
    assert!(
        actions.iter().any(
            |a| matches!(a, TimelineAction::RenameTrack { lane, track, name }
                if *lane == TimelineLane::Control && *track == 0 && name == "Cutoff")
        ),
        "expected the curve row to rename the control track, got {actions:?}"
    );

    let y = f.header_y(|a| matches!(a, TimelineAction::SelectAudioTrack(_)));
    let _ = f.click(egui::pos2(HEADER_X, y), egui::PointerButton::Secondary);
    let _ = f.frame(vec![]);
    let _ = f.frame(vec![]);
    let actions = f.frame(vec![
        egui::Event::Text("Vox".to_owned()),
        key(egui::Key::Enter),
    ]);
    assert!(
        actions.iter().any(
            |a| matches!(a, TimelineAction::RenameTrack { lane, track, name }
                if *lane == TimelineLane::Audio && *track == 0 && name == "Vox")
        ),
        "expected the audio row to rename the audio track, got {actions:?}"
    );
}

/// Esc abandons the edit: the draft is dropped and nothing is reported.
#[test]
fn escape_cancels_without_renaming() {
    let mut f = Fixture::new();
    let y = f.midi_header_y(0);
    let _ = f.click(egui::pos2(HEADER_X, y), egui::PointerButton::Secondary);
    let _ = f.frame(vec![]);
    let _ = f.frame(vec![]);

    let actions = f.frame(vec![
        egui::Event::Text("ignored".to_owned()),
        key(egui::Key::Escape),
    ]);
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, TimelineAction::RenameTrack { .. })),
        "Esc must not rename, got {actions:?}"
    );
    // Closed, and the next frame does not resurrect it either.
    let actions = f.frame(vec![key(egui::Key::Enter)]);
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, TimelineAction::RenameTrack { .. })),
        "the editor must be gone after Esc, got {actions:?}"
    );
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

fn key(k: egui::Key) -> egui::Event {
    egui::Event::Key {
        key: k,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::default(),
    }
}
