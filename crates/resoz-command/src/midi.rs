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
            "{track, clip, indices: [n], ticks?: 0, semitones?: 0} → {indices: [n]}",
            always,
            notes_move
        ),
        crate::cmd!(
            "midi.notes_edit_delta",
            "Resize Notes",
            ["Midi"],
            None,
            "{track, clip, indices: [n], delta_start?: 0, delta_length?: 0} → {indices: [n]}",
            always,
            notes_edit_delta
        ),
        crate::cmd!(
            "midi.notes_duplicate",
            "Duplicate Notes",
            ["Midi"],
            Some("Duplicate"),
            "{track, clip, indices: [n]} → {indices: [n]}",
            always,
            notes_duplicate
        ),
        crate::cmd!(
            "midi.notes_scale",
            "Scale Notes",
            ["Midi"],
            Some("Scale"),
            "{track, clip, indices: [n], pivot_tick, factor} → {indices: [n]}",
            always,
            notes_scale
        ),
        crate::cmd!(
            "midi.notes_split",
            "Split Notes",
            ["Midi"],
            Some("Split"),
            "{track, clip, indices: [n], tick} → {split: n, indices: [n]}",
            always,
            notes_split
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
        crate::cmd!(
            "midi.quantize",
            "Quantize",
            ["Midi", "Event"],
            None,
            "{track, clip, indices?, grid_ticks?, strength? 0..1, swing? 0..1, include_within? 0..1, exclude_within? 0..1, quantize_ends?, preserve_duration?, random? 0..1}",
            always,
            quantize_cmd
        ),
        crate::cmd!(
            "midi.transpose",
            "Transpose",
            ["Midi", "Event"],
            None,
            "{track, clip, indices?, semitones}",
            always,
            transpose_cmd
        ),
        crate::cmd!(
            "midi.velocity_op",
            "Velocity Op",
            ["Midi", "Event"],
            None,
            "{track, clip, indices?, op: set|add|scale|limit|ramp|humanize, value?, from?, to?, min?, max?}",
            always,
            velocity_op_cmd
        ),
        crate::cmd!(
            "midi.duration_op",
            "Duration Op",
            ["Midi", "Event"],
            None,
            "{track, clip, indices?, op: set|add|scale|legato|remove_overlap, value?, gap?}",
            always,
            duration_op_cmd
        ),
        crate::cmd!(query "midi.query", "Query Notes", [], None, "{track, clip}", always, query),
    ]
}

/// The `indices` parameter, or every note when absent (whole-clip operation).
fn indices_or_all(params: &Value, len: usize) -> Vec<usize> {
    let selected = indices(params, "indices");
    if selected.is_empty() {
        (0..len).collect()
    } else {
        selected
    }
}

fn quantize_cmd(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let options = resoz_model::QuantizeOptions {
        grid_ticks: i64_or(params, "grid_ticks", i64::from(resoz_model::TICKS_PER_STEP)),
        strength: crate::registry::f64_or(params, "strength", 1.0) as f32,
        swing: crate::registry::f64_or(params, "swing", 0.0) as f32,
        include_within: crate::registry::f64_or(params, "include_within", 1.0) as f32,
        exclude_within: crate::registry::f64_or(params, "exclude_within", 0.0) as f32,
        quantize_ends: params
            .get("quantize_ends")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        preserve_duration: params
            .get("preserve_duration")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        random: crate::registry::f64_or(params, "random", 0.0) as f32,
    };
    let notes = notes_mut(session, "midi.quantize", params)?;
    let selected = indices_or_all(params, notes.len());
    resoz_model::apply_to_selected(notes, &selected, |slice| {
        resoz_model::quantize(slice, &options)
    });
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    Ok(json!({ "quantized": selected.len() }))
}

fn transpose_cmd(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let semitones = i64_or(params, "semitones", 0).clamp(-127, 127) as i32;
    let notes = notes_mut(session, "midi.transpose", params)?;
    let selected = indices_or_all(params, notes.len());
    resoz_model::apply_to_selected(notes, &selected, |slice| {
        resoz_model::transpose(slice, semitones)
    });
    // Pitch is part of the sort key: same-start notes may reorder.
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    Ok(json!({ "transposed": selected.len() }))
}

fn velocity_op_cmd(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let name = params.get("op").and_then(Value::as_str).unwrap_or("");
    let op = match name {
        "set" => resoz_model::VelocityOp::Set(i64_or(params, "value", 100).clamp(1, 127) as u8),
        "add" => resoz_model::VelocityOp::Add(i64_or(params, "value", 0).clamp(-126, 126) as i32),
        "scale" => {
            resoz_model::VelocityOp::Scale(crate::registry::f64_or(params, "value", 1.0) as f32)
        }
        "limit" => resoz_model::VelocityOp::Limit {
            min: i64_or(params, "min", 1).clamp(1, 127) as u8,
            max: i64_or(params, "max", 127).clamp(1, 127) as u8,
        },
        "ramp" => resoz_model::VelocityOp::Ramp {
            from: i64_or(params, "from", 40).clamp(1, 127) as u8,
            to: i64_or(params, "to", 100).clamp(1, 127) as u8,
        },
        "humanize" => {
            resoz_model::VelocityOp::Humanize(i64_or(params, "value", 20).clamp(0, 126) as u8)
        }
        other => {
            return Err(bad_params(
                "midi.velocity_op",
                format!("`op` must be set|add|scale|limit|ramp|humanize, got {other:?}"),
            ));
        }
    };
    let notes = notes_mut(session, "midi.velocity_op", params)?;
    let selected = indices_or_all(params, notes.len());
    resoz_model::apply_to_selected(notes, &selected, |slice| {
        resoz_model::apply_velocity(slice, &op, resoz_model::DEFAULT_SEED)
    });
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    Ok(json!({ "changed": selected.len() }))
}

fn duration_op_cmd(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let name = params.get("op").and_then(Value::as_str).unwrap_or("");
    let op = match name {
        "set" => resoz_model::DurationOp::Set(i64_or(params, "value", 240)),
        "add" => resoz_model::DurationOp::Add(i64_or(params, "value", 0)),
        "scale" => {
            resoz_model::DurationOp::Scale(crate::registry::f64_or(params, "value", 1.0) as f32)
        }
        "legato" => resoz_model::DurationOp::Legato {
            gap: i64_or(params, "gap", 0),
        },
        "remove_overlap" => resoz_model::DurationOp::RemoveOverlap,
        other => {
            return Err(bad_params(
                "midi.duration_op",
                format!("`op` must be set|add|scale|legato|remove_overlap, got {other:?}"),
            ));
        }
    };
    let notes = notes_mut(session, "midi.duration_op", params)?;
    let selected = indices_or_all(params, notes.len());
    resoz_model::apply_to_selected(notes, &selected, |slice| {
        resoz_model::apply_duration(slice, &op)
    });
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    Ok(json!({ "changed": selected.len() }))
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

/// New positions of `edited` notes after a sort: for each edited note in
/// order, the first not-yet-claimed equal note. Value-equal twins are
/// interchangeable, so first-match assignment is always valid — this is how
/// an index-based selection survives a command that re-sorts the clip.
fn locate(notes: &[Note], edited: &[Note]) -> Vec<usize> {
    let mut claimed = vec![false; notes.len()];
    let mut found = Vec::with_capacity(edited.len());
    for want in edited {
        if let Some((at, _)) = notes
            .iter()
            .enumerate()
            .find(|(at, note)| !claimed[*at] && *note == want)
        {
            claimed[at] = true;
            found.push(at);
        }
    }
    found
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
    // The mutated values are what we relocate after the sort: the caller's
    // selection indices go stale the moment notes cross each other.
    let mut moved = Vec::with_capacity(selected.len());
    for (index, note) in notes.iter_mut().enumerate() {
        if selected.contains(&index) {
            note.start = (note.start + ticks).max(0);
            note.note = (i64::from(note.note) + semitones).clamp(0, 127) as u8;
            moved.push(*note);
        }
    }
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    Ok(json!({ "moved": moved.len(), "indices": locate(notes, &moved) }))
}

/// Uniform resize of the selection: `delta_start` shifts every start (trim
/// the left edge), `delta_length` stretches every end (right edge / draw).
/// Per-note clamping (start ≥ 0, length ≥ 1) may swallow part of the delta;
/// the returned indices track where each edited note ended up.
fn notes_edit_delta(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let selected = indices(params, "indices");
    if selected.is_empty() {
        return Err(bad_params("midi.notes_edit_delta", "`indices` is required"));
    }
    let delta_start = i64_or(params, "delta_start", 0);
    let delta_length = i64_or(params, "delta_length", 0);
    if delta_start == 0 && delta_length == 0 {
        return Err(bad_params(
            "midi.notes_edit_delta",
            "at least one of `delta_start`, `delta_length` must be nonzero",
        ));
    }
    let notes = notes_mut(session, "midi.notes_edit_delta", params)?;
    let mut edited = Vec::with_capacity(selected.len());
    for (index, note) in notes.iter_mut().enumerate() {
        if selected.contains(&index) {
            note.start = (note.start + delta_start).max(0);
            note.length = (note.length + delta_length).max(1);
            edited.push(*note);
        }
    }
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    Ok(json!({ "edited": edited.len(), "indices": locate(notes, &edited) }))
}

/// Copies of the selected notes (Ctrl + drag in the piano roll). Returns the
/// copies' new indices so the UI can select and drag *them* — value-equal to
/// the originals, so first-match assignment places them on the twin slots.
fn notes_duplicate(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let selected = indices(params, "indices");
    let notes = notes_mut(session, "midi.notes_duplicate", params)?;
    let copies: Vec<Note> = selected
        .iter()
        .filter_map(|index| notes.get(*index))
        .copied()
        .collect();
    if copies.is_empty() {
        return Err(bad_params(
            "midi.notes_duplicate",
            "`indices` must name existing notes",
        ));
    }
    notes.extend(copies.iter().copied());
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    Ok(json!({ "created": copies.len(), "indices": locate(notes, &copies) }))
}

/// Time-scales the selection around `pivot_tick` by `factor` (the piano
/// roll's Scale command). The UI computes the factor from the pointer's
/// distance to the pivot relative to the grab distance; the command
/// clamps the result per note (start ≥ 0, length ≥ 1) and reports where
/// the edited notes landed after the re-sort.
fn notes_scale(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let selected = indices(params, "indices");
    if selected.is_empty() {
        return Err(bad_params("midi.notes_scale", "`indices` is required"));
    }
    let pivot_tick = i64_or(params, "pivot_tick", 0);
    let factor = crate::registry::f64_or(params, "factor", 1.0) as f32;
    if !factor.is_finite() || factor <= 0.0 {
        return Err(bad_params(
            "midi.notes_scale",
            "`factor` must be a positive number",
        ));
    }
    let notes = notes_mut(session, "midi.notes_scale", params)?;
    resoz_model::apply_to_selected(notes, &selected, |slice| {
        resoz_model::scale_notes(slice, pivot_tick, factor);
    });
    // The mutated values are what we relocate after the sort: the caller's
    // selection indices go stale the moment notes cross each other.
    let edited: Vec<Note> = selected
        .iter()
        .filter_map(|index| notes.get(*index).copied())
        .collect();
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    Ok(json!({ "scaled": edited.len(), "indices": locate(notes, &edited) }))
}

/// Splits every selected note that spans `tick` into two halves (the Cut
/// command's `k`): pitch, velocity and channel are preserved on both
/// halves. A note that starts or ends exactly on the tick is left alone —
/// a zero-length half is unrepresentable. Selected notes that do not span
/// the tick stay selected, so the result reads as "the same selection,
/// cut where I clicked".
fn notes_split(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let selected = indices(params, "indices");
    if selected.is_empty() {
        return Err(bad_params("midi.notes_split", "`indices` is required"));
    }
    if params.get("tick").and_then(Value::as_i64).is_none() {
        return Err(bad_params("midi.notes_split", "`tick` is required"));
    }
    let tick = i64_or(params, "tick", 0).max(0);
    let notes = notes_mut(session, "midi.notes_split", params)?;
    // Rebuild the vector: a split replaces one note with two, so the
    // length changes and the in-place op contract does not apply here.
    let mut cut = 0usize;
    let mut rebuilt: Vec<Note> = Vec::with_capacity(notes.len() + selected.len());
    let mut results: Vec<Note> = Vec::with_capacity(selected.len());
    for (index, note) in notes.iter().enumerate() {
        let chosen = selected.contains(&index);
        if chosen && note.start < tick && tick < note.end() {
            let left = Note {
                length: tick - note.start,
                ..*note
            };
            let right = Note {
                start: tick,
                length: note.end() - tick,
                ..*note
            };
            results.push(left);
            results.push(right);
            rebuilt.push(left);
            rebuilt.push(right);
            cut += 1;
        } else {
            if chosen {
                results.push(*note);
            }
            rebuilt.push(*note);
        }
    }
    *notes = rebuilt;
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    Ok(json!({ "split": cut, "indices": locate(notes, &results) }))
}

fn change_velocity(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let selected = indices(params, "indices");
    let set = params.get("set").and_then(Value::as_i64);
    let add = params.get("add").and_then(Value::as_i64);
    let scale = params
        .get("scale")
        .and_then(Value::as_f64)
        .filter(|f| f.is_finite());
    // `set_each` carries absolute per-note values ({"3": 90, ...}) — the
    // velocity lane's drag emits one frame with many targets, each at the
    // pointer height. Exempt from the set/add/scale requirement below.
    let set_each: Vec<(usize, i64)> = params
        .get("set_each")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| Some((k.parse::<usize>().ok()?, v.as_i64()?)))
                .collect()
        })
        .unwrap_or_default();
    if set.is_none() && add.is_none() && scale.is_none() && set_each.is_empty() {
        return Err(bad_params(
            "midi.change_velocity",
            "give `set`, `add`, `scale` or `set_each`",
        ));
    }
    let notes = notes_mut(session, "midi.change_velocity", params)?;
    let lo = i64::from(resoz_model::MIN_VELOCITY);
    let mut changed = 0usize;
    for (index, note) in notes.iter_mut().enumerate() {
        let next = if let Some(value) = set_each
            .iter()
            .find(|(target, _)| *target == index)
            .map(|(_, value)| *value)
        {
            value
        } else if !selected.contains(&index) {
            continue;
        } else if let Some(value) = set {
            value
        } else if let Some(value) = add {
            i64::from(note.velocity) + value
        } else {
            // The guard above rejects the call unless one of `set`/`add`/
            // `scale`/`set_each` is present, so `scale` is `Some` here;
            // 1.0 is a no-op fallback rather than a panic if that changes.
            (f64::from(note.velocity) * scale.unwrap_or(1.0)).round() as i64
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
