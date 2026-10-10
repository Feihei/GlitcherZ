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
