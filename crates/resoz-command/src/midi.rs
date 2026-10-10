//! MIDI note editing (the piano roll's command surface).
//!
//! Every command follows the same contract: it locates its target, mutates
//! through [`Session::project_mut`], restores the sequence sort invariant,
//! and returns the note's new index. Returning the index matters — sorting
//! permutes the vector, so an index-based selection held across the call
//! would otherwise point at a different note.

use resoz_model::{MidiClip, Note};
use serde_json::{Value, json};

use crate::registry::{always, i64_or, indices};
use crate::{Session, bad_params};

/// Every MIDI command.
pub fn specs() -> Vec<crate::CommandSpec> {
    vec![
        crate::cmd!(
            "midi.note_add",
            "Add Note",
            ["Midi"],
            None,
            "{track, clip, note, start_ticks, length_ticks?: 240, velocity?: 100, channel?: 0}",
            always,
            note_add
        ),
        crate::cmd!(
            "midi.note_edit",
            "Edit Note",
            ["Midi"],
            None,
            "{track, clip, index, note?, start_ticks?, length_ticks?, velocity?}",
            always,
            note_edit
        ),
        crate::cmd!(
            "midi.note_delete",
            "Delete Notes",
            ["Midi"],
            Some("Delete"),
            "{track, clip, indices: [n]}",
            always,
            note_delete
        ),
        crate::cmd!(
            "midi.notes_move",
            "Move Notes",
            ["Midi"],
            None,
            "{track, clip, indices: [n], ticks?: 0, semitones?: 0}",
            always,
            notes_move
        ),
        crate::cmd!(
            "midi.change_velocity",
            "Change Velocity",
            ["Midi", "Event"],
            None,
            "{track, clip, indices: [n], set?|add?|scale?}",
            crate::registry::has_notes,
            change_velocity
        ),
        crate::cmd!(noundo "midi.query", "Query Notes", [], None, "{track, clip}", always, query),
    ]
}

/// Mutable clip at `track`/`clip` from the parameters.
fn clip_mut<'a>(
    session: &'a mut Session,
    command: &str,
    params: &Value,
) -> crate::Result<&'a mut MidiClip> {
    let track = i64_or(params, "track", 0);
    let clip = i64_or(params, "clip", 0);
    session
        .project_mut()
        .midi_tracks
        .get_mut(track.max(0) as usize)
        .and_then(|t| t.clips.get_mut(clip.max(0) as usize))
        .ok_or_else(|| bad_params(command, format!("no clip {clip} on track {track}")))
}

/// Mutable note slice of the targeted clip.
fn notes_mut<'a>(
    session: &'a mut Session,
    command: &str,
    params: &Value,
) -> crate::Result<&'a mut Vec<Note>> {
    Ok(clip_mut(session, command, params)?.notes_mut())
}

/// Position of `note` in the sequence, by value.
fn index_of(notes: &[Note], note: &Note) -> usize {
    notes.iter().position(|n| n == note).unwrap_or(0)
}

fn note_add(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let note = Note {
        start: i64_or(params, "start_ticks", 0).max(0),
        length: i64_or(params, "length_ticks", 240).max(1),
        note: i64_or(params, "note", 60).clamp(0, 127) as u8,
        velocity: i64_or(params, "velocity", i64::from(resoz_model::DEFAULT_VELOCITY))
            .clamp(i64::from(resoz_model::MIN_VELOCITY), 127) as u8,
        channel: i64_or(params, "channel", 0).clamp(0, 15) as u8,
    };
    let notes = notes_mut(session, "midi.note_add", params)?;
    notes.push(note);
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    let index = index_of(notes, &note);
    Ok(json!({ "index": index, "notes": notes.len() }))
}

fn note_edit(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let raw = i64_or(params, "index", -1);
    if raw < 0 {
        return Err(bad_params("midi.note_edit", "`index` is required"));
    }
    let index = raw as usize;
    let notes = notes_mut(session, "midi.note_edit", params)?;
    let note = notes
        .get_mut(index)
        .ok_or_else(|| bad_params("midi.note_edit", "index out of range"))?;
    if let Some(value) = params.get("note").and_then(Value::as_i64) {
        note.note = value.clamp(0, 127) as u8;
    }
    if let Some(value) = params.get("start_ticks").and_then(Value::as_i64) {
        note.start = value.max(0);
    }
    if let Some(value) = params.get("length_ticks").and_then(Value::as_i64) {
        note.length = value.max(1);
    }
    if let Some(value) = params.get("velocity").and_then(Value::as_i64) {
        note.velocity = value.clamp(i64::from(resoz_model::MIN_VELOCITY), 127) as u8;
    }
    if let Some(value) = params.get("channel").and_then(Value::as_i64) {
        note.channel = value.clamp(0, 15) as u8;
    }
    let edited = *note;
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    Ok(json!({ "note": edited, "index": index_of(notes, &edited) }))
}

fn note_delete(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let mut selected = indices(params, "indices");
    selected.sort_unstable();
    selected.dedup();
    let notes = notes_mut(session, "midi.note_delete", params)?;
    let before = notes.len();
    // Walk with an explicit position: `retain` cannot see the index, and
    // looking each note up by value would hit the wrong copy of a duplicate.
    let mut position = 0usize;
    notes.retain(|_| {
        let keep = !selected.contains(&position);
        position += 1;
        keep
    });
    Ok(json!({ "deleted": before - notes.len() }))
}

fn notes_move(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let selected = indices(params, "indices");
    let ticks = i64_or(params, "ticks", 0);
    let semitones = i64_or(params, "semitones", 0);
    let notes = notes_mut(session, "midi.notes_move", params)?;
    for (index, note) in notes.iter_mut().enumerate() {
        if selected.contains(&index) {
            note.start = (note.start + ticks).max(0);
            note.note = (i64::from(note.note) + semitones).clamp(0, 127) as u8;
        }
    }
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    Ok(json!({ "moved": selected.len() }))
}

fn change_velocity(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let selected = indices(params, "indices");
    let set = params.get("set").and_then(Value::as_i64);
    let add = params.get("add").and_then(Value::as_i64);
    let scale = params
        .get("scale")
        .and_then(Value::as_f64)
        .filter(|f| f.is_finite());
    if set.is_none() && add.is_none() && scale.is_none() {
        return Err(bad_params(
            "midi.change_velocity",
            "give `set`, `add` or `scale`",
        ));
    }
    let notes = notes_mut(session, "midi.change_velocity", params)?;
    let lo = i64::from(resoz_model::MIN_VELOCITY);
    let mut changed = 0usize;
    for (index, note) in notes.iter_mut().enumerate() {
        if !selected.contains(&index) {
            continue;
        }
        let next = if let Some(value) = set {
            value
        } else if let Some(value) = add {
            i64::from(note.velocity) + value
        } else {
            (f64::from(note.velocity) * scale.expect("checked above")).round() as i64
        };
        note.velocity = next.clamp(lo, 127) as u8;
        changed += 1;
    }
    Ok(json!({ "changed": changed }))
}

fn query(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let track = i64_or(params, "track", 0).max(0) as usize;
    let clip = i64_or(params, "clip", 0).max(0) as usize;
    let notes = session
        .project()
        .midi_tracks
        .get(track)
        .and_then(|t| t.clips.get(clip))
        .map(|c| c.notes().to_vec())
        .ok_or_else(|| bad_params("midi.query", "no such clip"))?;
    Ok(json!({ "notes": notes }))
}
