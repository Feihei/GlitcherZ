//! Timeline playhead geometry: every lane kind must start its content at the
//! same x (so the playhead is one straight vertical line), and adjacent lanes
//! must butt together vertically (so the per-lane segments join).

use eframe::egui;
use glitcherz_model::{AudioTrack, ControlTrack, MidiTrack, SongLoop, TICKS_PER_BAR};
use glitcherz_ui::timeline::{self, TimelineParams, TimelineState};

const SCREEN: egui::Vec2 = egui::vec2(1000.0, 600.0);
const PLAYHEAD: f32 = 2.0 * TICKS_PER_BAR as f32;
const PLAYHEAD_COLOR: egui::Color32 = egui::Color32::from_rgb(235, 90, 90);

#[test]
fn playhead_segments_share_one_x_and_join_vertically() {
    let ctx = egui::Context::default();
    let mut state = TimelineState::default();
    let mut router = glitcherz_ui::focus::FocusRouter::new();
    let midi = vec![MidiTrack::default(), MidiTrack::default()];
    let control = vec![ControlTrack::default()];
    let audio = vec![AudioTrack::default()];
    let loop_range = SongLoop::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
        time: Some(0.0),
        ..Default::default()
    };

    let mut out = ctx.run_ui(raw, |root| {
        router.begin_frame(root.ctx());
        egui::CentralPanel::default().show(root, |ui| {
            let _ = timeline::show(
                ui,
                &mut state,
                &midi,
                &control,
                &audio,
                TimelineParams {
                    selected_track: 0,
                    selected_auto: 0,
                    selected_audio: 0,
                    playhead_ticks: PLAYHEAD,
                    playing: false,
                    edit_target: None,
                    loop_range: &loop_range,
                    markers: &[],
                    meter: &glitcherz_model::MeterMap::default(),
                    sec_per_tick: 60.0 / (120.0 * 960.0),
                    waveform_of: &|_| None,
                    clip_groups: &[],
                },
                &mut router,
            );
        });
    });
    out.textures_delta.clear();

    let mut segments = Vec::new();
    for clipped in &out.shapes {
        if let egui::Shape::LineSegment { points, stroke } = &clipped.shape
            && stroke.color == PLAYHEAD_COLOR
        {
            let (a, b) = (points[0].y.min(points[1].y), points[0].y.max(points[1].y));
            segments.push((points[0].x, a, b));
        }
    }

    assert_eq!(
        segments.len(),
        4,
        "one playhead segment per lane (2 MIDI + 1 control + 1 audio)"
    );
    segments.sort_by(|a, b| a.1.total_cmp(&b.1));
    let x = segments[0].0;
    for (ix, _, _) in &segments {
        assert!(
            (*ix - x).abs() < 0.01,
            "playhead x drifted between lanes: {ix} vs {x}"
        );
    }
    // The two MIDI lanes are adjacent in the arrangement: their segments must
    // touch (no inter-row gap), i.e. the line is continuous through them.
    assert!(
        (segments[0].2 - segments[1].1).abs() < 0.01,
        "adjacent MIDI lanes leave a gap: {} vs {}",
        segments[0].2,
        segments[1].1
    );
}
