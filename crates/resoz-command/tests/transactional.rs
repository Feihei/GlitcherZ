//! The guarantees [`Session::execute`] makes, exercised end to end.

use resoz_command::{CommandError, Session, command_specs};
use resoz_model::{MidiClip, Note, Project};
use serde_json::json;

/// A session with one track holding one clip of four notes.
fn session_with_notes() -> Session {
    let mut project = Project::default();
    let mut clip = MidiClip::default();
    for (index, start) in [0i64, 240, 480, 720].into_iter().enumerate() {
        clip.notes_mut().push(Note {
            start,
            length: 240,
            note: 60 + index as u8,
            velocity: 100,
            channel: 0,
        });
    }
    clip.sort();
    project.midi_tracks[0].clips.push(clip);
    Session::new(project)
}

fn notes(session: &Session) -> Vec<Note> {
    session.project().midi_tracks[0].clips[0].notes().to_vec()
}

#[test]
fn a_command_edits_the_document_and_records_one_step() {
    let mut session = session_with_notes();
    assert!(!session.can_undo());

    let outcome = session
        .execute("midi.note_edit", &json!({ "index": 0, "velocity": 40 }))
        .expect("edit should succeed");

    assert_eq!(outcome["index"], 0);
    assert_eq!(notes(&session)[0].velocity, 40);
    assert!(session.can_undo());
    assert_eq!(session.undo_label(), Some("Edit Note"));
    assert!(session.is_dirty());
}

#[test]
fn undo_restores_the_document_and_redo_reapplies_it() {
    let mut session = session_with_notes();
    let before = notes(&session);

    session
        .execute("midi.note_edit", &json!({ "index": 1, "velocity": 20 }))
        .unwrap();
    assert_eq!(notes(&session)[1].velocity, 20);

    assert!(session.undo());
    assert_eq!(notes(&session), before, "undo restores the notes exactly");
    assert_eq!(session.redo_label(), Some("Edit Note"));

    assert!(session.redo());
    assert_eq!(notes(&session)[1].velocity, 20, "redo reapplies the edit");
}

#[test]
fn a_failing_command_leaves_no_partial_edit() {
    let mut session = session_with_notes();
    let before = notes(&session);

    // Index 99 does not exist: the command must fail before mutating.
    let error = session
        .execute("midi.note_edit", &json!({ "index": 99, "velocity": 10 }))
        .expect_err("out-of-range index should fail");

    assert!(matches!(error, CommandError::BadParams { .. }));
    assert_eq!(notes(&session), before, "the document is untouched");
    assert!(!session.can_undo(), "a failed command records no undo step");
}

#[test]
fn a_missing_target_is_reported_without_touching_the_document() {
    let mut session = session_with_notes();
    let before = notes(&session);

    let error = session
        .execute(
            "midi.note_edit",
            &json!({ "track": 7, "clip": 0, "index": 0, "velocity": 10 }),
        )
        .expect_err("a nonexistent track should fail");

    assert!(matches!(error, CommandError::BadParams { .. }));
    assert_eq!(notes(&session), before);
    assert!(!session.can_undo());
}

#[test]
fn an_unknown_command_is_rejected() {
    let mut session = session_with_notes();
    let error = session
        .execute("midi.does_not_exist", &json!({}))
        .expect_err("unknown ids must not run");
    assert!(matches!(error, CommandError::Unknown(_)));
}

#[test]
fn one_drag_is_one_undo_step() {
    let mut session = session_with_notes();
    // The note starts at velocity 100; the drag ends somewhere else so the
    // undo assertion is meaningful.
    for velocity in [30, 45, 60, 75, 90] {
        session
            .execute_merged(
                "midi.note_edit",
                &json!({ "index": 0, "velocity": velocity }),
                "velocity:0:0",
            )
            .expect("drag frames should succeed");
    }
    session.end_merge();

    assert_eq!(notes(&session)[0].velocity, 90, "the last frame wins");
    assert_eq!(
        session.undo_history(),
        vec!["Edit Note"],
        "five frames collapse into one step"
    );
    assert!(session.undo());
    assert_eq!(
        notes(&session)[0].velocity,
        100,
        "one undo returns to the pre-gesture value, not to the previous frame"
    );
    assert!(!session.can_undo(), "the whole gesture was a single step");
}

#[test]
fn a_different_gesture_key_starts_a_new_step() {
    let mut session = session_with_notes();
    session
        .execute_merged(
            "midi.note_edit",
            &json!({ "index": 0, "velocity": 40 }),
            "vel:0",
        )
        .unwrap();
    session
        .execute_merged(
            "midi.note_edit",
            &json!({ "index": 1, "velocity": 50 }),
            "vel:1",
        )
        .unwrap();
    session.end_merge();
    assert_eq!(session.undo_history().len(), 2, "two gestures, two steps");
}

#[test]
fn a_query_records_no_undo_step() {
    let mut session = session_with_notes();
    let outcome = session.execute("midi.query", &json!({})).unwrap();
    assert_eq!(outcome["notes"].as_array().map(Vec::len), Some(4));
    assert!(
        !session.can_undo(),
        "a read-only command has no before state"
    );
}

#[test]
fn snapshots_share_the_document_until_one_side_edits() {
    let mut session = session_with_notes();
    let snapshot = session.project_arc();
    assert!(
        std::sync::Arc::ptr_eq(&snapshot, &session.project_arc()),
        "taking a snapshot must not copy the document"
    );

    session
        .execute("midi.note_edit", &json!({ "index": 0, "velocity": 33 }))
        .unwrap();

    assert!(
        !std::sync::Arc::ptr_eq(&snapshot, &session.project_arc()),
        "editing triggers copy-on-write, leaving the snapshot intact"
    );
    assert_eq!(snapshot.midi_tracks[0].clips[0].notes()[0].velocity, 100);
}

#[test]
fn a_new_edit_discards_the_redo_stack() {
    let mut session = session_with_notes();
    session
        .execute("midi.note_edit", &json!({ "index": 0, "velocity": 40 }))
        .unwrap();
    assert!(session.undo());
    assert!(session.can_redo());

    session
        .execute("midi.note_edit", &json!({ "index": 0, "velocity": 60 }))
        .unwrap();
    assert!(
        !session.can_redo(),
        "redo history is invalidated by a new edit"
    );
}

#[test]
fn history_is_bounded() {
    let mut session = session_with_notes();
    session.set_history_limit(3);
    for velocity in [10, 20, 30, 40, 50] {
        session
            .execute(
                "midi.note_edit",
                &json!({ "index": 0, "velocity": velocity }),
            )
            .unwrap();
    }
    assert_eq!(session.undo_history().len(), 3);
    // The oldest steps were dropped, so undoing runs out before the edit that
    // set velocity 10.
    let mut undone = 0;
    while session.undo() {
        undone += 1;
    }
    assert_eq!(undone, 3);
    assert_eq!(
        notes(&session)[0].velocity,
        20,
        "stops at the oldest retained step"
    );
}

#[test]
fn every_command_survives_empty_params() {
    // No command may panic when handed no parameters on a default document.
    for command in command_specs() {
        let mut session = Session::default();
        if let Err(CommandError::Panicked { .. }) = session.execute(command.id, &json!({})) {
            panic!("{} panicked on empty params", command.id);
        }
    }
}

#[test]
fn no_command_panics_on_a_populated_document() {
    let populated = session_with_notes().project().clone();
    for command in command_specs() {
        // A fresh session per command, so one cannot influence the next.
        let mut session = Session::new(populated.clone());
        if let Err(CommandError::Panicked { .. }) = session.execute(command.id, &json!({})) {
            panic!("{} panicked", command.id);
        }
    }
}

#[test]
fn suspend_history_hides_nested_steps() {
    let mut session = session_with_notes();
    session.suspend_history();
    session
        .execute("midi.note_edit", &json!({ "index": 0, "velocity": 40 }))
        .unwrap();
    let before = session.project_arc();
    // A direct edit during suspension records nothing...
    session.project_mut().metadata.name = "renamed".into();
    session.push_undo("Nested Edit", before);
    session.resume_history();
    assert!(
        session.undo_history().is_empty(),
        "suspended edits leave no undo steps"
    );
    assert_eq!(session.project().metadata.name, "renamed");
}

#[test]
fn push_undo_merged_collapses_out_of_band_gestures() {
    let mut session = session_with_notes();
    let mut velocity = 90;
    for _ in 0..5 {
        velocity -= 10;
        let before = session.project_arc();
        session.project_mut().midi_tracks[0].clips[0].notes_mut()[0].velocity = velocity;
        session.push_undo_merged("Fader", before, "fader:0");
    }
    session.end_merge();
    assert_eq!(
        session.undo_history(),
        vec!["Fader"],
        "five frames, one step"
    );
    assert!(session.undo());
    assert_eq!(
        session.project().midi_tracks[0].clips[0].notes()[0].velocity,
        100
    );
}

#[test]
fn restore_rolls_back_without_touching_history() {
    let mut session = session_with_notes();
    session
        .execute("midi.note_edit", &json!({ "index": 0, "velocity": 40 }))
        .unwrap();
    let snapshot = session.project_arc();
    session
        .execute("midi.note_edit", &json!({ "index": 1, "velocity": 20 }))
        .unwrap();
    assert_eq!(session.undo_history().len(), 2);

    session.restore(snapshot);
    assert_eq!(
        session.undo_history().len(),
        2,
        "restore is not an undo step"
    );
    assert_eq!(
        session.project().midi_tracks[0].clips[0].notes()[0].velocity,
        40
    );
}

#[test]
fn quantize_respects_the_selection() {
    let mut session = session_with_notes();
    // Three notes at 0 / 250 / 500 (only some on the grid).
    session.project_mut().midi_tracks[0].clips[0].notes_mut()[1].start = 250;
    session.project_mut().midi_tracks[0].clips[0].notes_mut()[2].start = 500;
    session
        .execute(
            "midi.quantize",
            &json!({ "track": 0, "clip": 0, "indices": [1], "grid_ticks": 240 }),
        )
        .unwrap();
    let notes = session.project().midi_tracks[0].clips[0].notes();
    assert_eq!(notes[0].start, 0, "unselected: untouched");
    assert_eq!(notes[1].start, 240, "selected: pulled to the line");
    assert_eq!(notes[2].start, 500, "unselected: untouched");

    // Undo restores the pre-quantize position.
    assert!(session.undo());
    let notes = session.project().midi_tracks[0].clips[0].notes();
    assert_eq!(notes[1].start, 250);
}

#[test]
fn velocity_ramp_follows_time_order_within_the_selection() {
    let mut session = session_with_notes();
    session.project_mut().midi_tracks[0].clips[0].notes_mut()[0].start = 480;
    session
        .execute(
            "midi.velocity_op",
            &json!({ "track": 0, "clip": 0, "op": "ramp", "from": 40, "to": 100 }),
        )
        .unwrap();
    let mut by_start: Vec<(i64, u8)> = session.project().midi_tracks[0].clips[0]
        .notes()
        .iter()
        .map(|n| (n.start, n.velocity))
        .collect();
    by_start.sort_unstable();
    let velocities: Vec<u8> = by_start.into_iter().map(|(_, v)| v).collect();
    assert!(
        velocities.first() < velocities.last(),
        "crescendo by time order"
    );
}

#[test]
fn legato_extends_each_note_to_the_next() {
    let mut session = session_with_notes();
    session
        .execute(
            "midi.duration_op",
            &json!({ "track": 0, "clip": 0, "op": "legato", "gap": 0 }),
        )
        .unwrap();
    let notes = session.project().midi_tracks[0].clips[0].notes();
    for window in notes.windows(2) {
        assert_eq!(
            window[0].start + window[0].length,
            window[1].start,
            "each note ends exactly where the next begins"
        );
    }
}

#[test]
fn unknown_op_names_are_rejected_without_panicking() {
    let mut session = session_with_notes();
    let error = session
        .execute(
            "midi.velocity_op",
            &json!({ "track": 0, "clip": 0, "op": "explode" }),
        )
        .expect_err("unknown op must be a bad-params error");
    assert!(error.to_string().contains("set|add|scale"));
}
