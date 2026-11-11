//! W4/W5 command surface: randomize and strum over a clip or a selection.

use glitcherz_command::Session;
use glitcherz_model::{MidiClip, Note, Project};
use serde_json::json;

fn note(start: i64, pitch: u8, velocity: u8) -> Note {
    Note {
        start,
        length: 240,
        note: pitch,
        velocity,
        channel: 0,
    }
}

fn session_with(notes: Vec<Note>) -> Session {
    let mut project = Project::default();
    let data = project.push_midi_data(glitcherz_model::MidiData {
        length_ticks: 384,
        ..glitcherz_model::MidiData::default()
    });
    project
        .midi_data_mut(data)
        .unwrap()
        .notes_mut()
        .extend(notes);
    project.midi_data_mut(data).unwrap().sort();
    project.midi_tracks[0].clips.push(MidiClip {
        start_ticks: 0,
        span_ticks: 384,
        data,
        ..MidiClip::default()
    });
    Session::new(project)
}

fn notes(session: &Session) -> Vec<Note> {
    session
        .project()
        .midi_clip_notes(0, 0)
        .expect("clip notes")
        .to_vec()
}

fn chord() -> Vec<Note> {
    vec![note(0, 60, 100), note(0, 64, 90), note(0, 67, 80)]
}

#[test]
fn randomize_whole_clip_reports_every_note() {
    let mut session = session_with(chord());
    let outcome = session
        .execute(
            "midi.randomize",
            &json!({
                "pitch": 3, "start": 0, "length": 0,
                "velocity": 20, "probability": 1.0, "seed": 11,
            }),
        )
        .expect("randomize should succeed");
    assert_eq!(outcome["randomized"], 3);
    assert_eq!(outcome["indices"].as_array().map(Vec::len), Some(3));
    assert!(session.can_undo());
    assert_eq!(session.undo_label(), Some("Randomize"));
    for note in notes(&session) {
        assert!((0..=127).contains(&note.note));
        assert!(note.start >= 0);
        assert!(note.length >= 1);
        assert!((1..=127).contains(&note.velocity));
    }
}

#[test]
fn randomize_selection_leaves_other_notes_untouched() {
    let mut session = session_with(vec![
        note(0, 60, 100),
        note(480, 62, 100),
        note(960, 64, 100),
    ]);
    let before = notes(&session);
    session
        .execute(
            "midi.randomize",
            &json!({ "indices": [1], "pitch": 5, "seed": 2 }),
        )
        .expect("selection randomize should succeed");
    let after = notes(&session);
    assert_eq!(after.len(), 3, "no note may be deleted");
    let untouched = after
        .iter()
        .filter(|n| *n == &before[0] || *n == &before[2])
        .count();
    assert_eq!(untouched, 2, "notes outside the selection stay put");
}

#[test]
fn strum_spreads_a_chord_from_the_lowest_note() {
    let mut session = session_with(chord());
    let outcome = session
        .execute(
            "midi.strum",
            &json!({ "spread_ticks": 120, "direction": "up", "group_window_ticks": 0 }),
        )
        .expect("strum should succeed");
    assert_eq!(outcome["strummed"], 3);
    let mut starts: Vec<(u8, i64)> = notes(&session).iter().map(|n| (n.note, n.start)).collect();
    starts.sort_unstable();
    assert_eq!(starts, vec![(60, 0), (64, 120), (67, 240)]);
    assert!(
        notes(&session).iter().all(|n| n.length == 240),
        "strum never changes the notated length"
    );
}

#[test]
fn strum_reports_indices_that_track_the_re_sorted_notes() {
    let mut session = session_with(vec![note(0, 60, 100), note(0, 72, 100)]);
    let outcome = session
        .execute(
            "midi.strum",
            &json!({ "spread_ticks": 240, "direction": "down" }),
        )
        .expect("strum should succeed");
    let indices: Vec<usize> = outcome["indices"]
        .as_array()
        .expect("indices")
        .iter()
        .map(|v| v.as_u64().unwrap() as usize)
        .collect();
    let after = notes(&session);
    assert_eq!(indices.len(), 2);
    for index in indices {
        let note = after.get(index).expect("reported index must exist");
        assert!(note.start == 0 || note.start == 240);
    }
    // Down: the high note attacks first.
    let high = after.iter().find(|n| n.note == 72).unwrap();
    let low = after.iter().find(|n| n.note == 60).unwrap();
    assert_eq!(high.start, 0);
    assert_eq!(low.start, 240);
}

#[test]
fn generator_commands_survive_missing_parameters() {
    let mut session = session_with(chord());
    // Both default to track 0 / clip 0 and whole-clip scope.
    session
        .execute("midi.randomize", &json!({}))
        .expect("empty randomize params default to a no-op");
    session
        .execute("midi.strum", &json!({}))
        .expect("empty strum params default to a no-op");
    assert_eq!(notes(&session).len(), 3);
}
