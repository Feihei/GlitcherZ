//! Arrangement clip commands (F-039 V2; spec `command-vocabulary.md` D6).
//!
//! `clip.add` creates an empty MIDI clip so an agent can build a song from an
//! empty project: add a clip, write notes with `midi.note_add`, transpose,
//! render. Control / audio clips need a target / media path and stay out of
//! V2.

use glitcherz_model::{MAX_ARRANGEMENT_TICKS, MAX_CLIPS_PER_TRACK, MidiClip};
use serde_json::{Value, json};

use crate::registry::{always, i64_or};
use crate::{Session, bad_params};

/// Every clip command.
pub fn specs() -> Vec<crate::CommandSpec> {
    vec![
        crate::cmd!(
            "clip.add",
            "Add Clip",
            ["Event", "Edit"],
            None,
            "{track, lane? midi, start_ticks? 0, span_ticks? bar, name?} → {clip, start_ticks, span_ticks, loop_ticks}",
            always,
            add
        ),
        crate::cmd!(
            "clip.link_data",
            "Link Clip Data",
            ["Event", "Edit"],
            None,
            "{track, clip, source_track, source_clip} → {data, refs}",
            always,
            link_data
        ),
        crate::cmd!(
            "clip.unlink_data",
            "Unlink Clip Data",
            ["Event", "Edit"],
            None,
            "{track, clip} → {data, refs, unlinked}",
            always,
            unlink_data
        ),
        crate::cmd!(
            "clip.tile",
            "Tile Clip",
            ["Event", "Edit"],
            None,
            "{track, clip, count} → {clips, count}",
            always,
            tile
        ),
        crate::cmd!(
            "clip.fit_window",
            "Fit Window to Data",
            ["Event", "Edit"],
            None,
            "{track, clip, edge?: both} → {source_offset, span_ticks, length_ticks}",
            always,
            fit_window
        ),
    ]
}

fn add(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let lane = params.get("lane").and_then(Value::as_str).unwrap_or("midi");
    if lane != "midi" {
        return Err(bad_params(
            "clip.add",
            format!("lane `{lane}` is not supported yet (midi only)"),
        ));
    }
    let raw_track = i64_or(params, "track", 0);
    if raw_track < 0 {
        return Err(bad_params("clip.add", "`track` must be a MIDI track"));
    }
    let start = i64_or(params, "start_ticks", 0).max(0);
    if u64::try_from(start).map_or(true, |start| start >= MAX_ARRANGEMENT_TICKS) {
        return Err(bad_params(
            "clip.add",
            "`start_ticks` is outside the arrangement",
        ));
    }
    let start = start as u32;
    // The default length follows the meter segment the clip starts in (Met M4).
    let bar = session.project().ticks_per_bar_at(start);
    let max_span =
        i64::from((MAX_ARRANGEMENT_TICKS - u64::from(start)).min(u64::from(u32::MAX)) as u32);
    let span = i64_or(params, "span_ticks", i64::from(bar)).clamp(1, max_span.max(1)) as u32;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // F-046 R14: a clip always carries a readable handle; an absent `name`
    // gets the next `clip_N` rather than staying blank.
    let name = if name.is_empty() {
        session.project().next_clip_name()
    } else {
        name
    };

    let track = raw_track as usize;
    if session.project().midi_tracks.get(track).is_none() {
        return Err(bad_params("clip.add", format!("no MIDI track {raw_track}")));
    }
    if session.project().midi_tracks[track].clips.len() >= MAX_CLIPS_PER_TRACK {
        return Err(bad_params(
            "clip.add",
            format!("track already holds {MAX_CLIPS_PER_TRACK} clips"),
        ));
    }
    let data = session
        .project_mut()
        .push_midi_data(glitcherz_model::MidiData {
            length_ticks: span,
            ..glitcherz_model::MidiData::default()
        });
    let track = &mut session.project_mut().midi_tracks[track];
    track.clips.push(MidiClip {
        name,
        start_ticks: start,
        span_ticks: span,
        data,
        ..MidiClip::default()
    });
    let index = track.clips.len() - 1;
    Ok(json!({
        "clip": index,
        "start_ticks": start,
        "span_ticks": span,
        "loop_ticks": span,
    }))
}
/// F-041 M11: `tile N` is a one-shot duplicate operation. Appends `count - 1`
/// copies that repeat the same window and share the content; nothing is
/// grouped and no tile count is stored. Returns the appended clip indices.
fn tile(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let track = i64_or(params, "track", 0).max(0) as usize;
    let clip = i64_or(params, "clip", 0).max(0) as usize;
    let count = i64_or(params, "count", 1).max(1) as u32;
    let project = session.project_mut();
    let Some(base) = project
        .midi_tracks
        .get(track)
        .and_then(|t| t.clips.get(clip))
        .cloned()
    else {
        return Err(bad_params(
            "clip.tile",
            format!("no clip {clip} on track {track}"),
        ));
    };
    let Some(len) = project.midi_tracks.get(track).map(|t| t.clips.len()) else {
        return Err(bad_params("clip.tile", format!("no track {track}")));
    };
    let copies = (count.saturating_sub(1) as usize).min(MAX_CLIPS_PER_TRACK.saturating_sub(len));
    // F-046 R14: each repeat is its own clip, so each gets its own name.
    let mut taken = project.clip_names();
    let mut names = Vec::with_capacity(copies);
    for _ in 0..copies {
        let name = glitcherz_model::copy_name(&base.name, "clip", &taken);
        taken.push(name.clone());
        names.push(name);
    }
    let Some(midi_track) = project.midi_tracks.get_mut(track) else {
        return Err(bad_params("clip.tile", format!("no track {track}")));
    };
    let first = midi_track.clips.len();
    for (k, name) in names.into_iter().enumerate() {
        let k = k + 1;
        let mut copy = base.clone();
        copy.name = name;
        copy.start_ticks = base
            .start_ticks
            .saturating_add(base.span_ticks.saturating_mul(k as u32));
        copy.loop_ticks = None;
        midi_track.clips.push(copy);
    }
    let clips: Vec<usize> = (first..first + copies).collect();
    Ok(json!({ "clips": clips, "count": copies as u32 + 1 }))
}

/// F-041 M10.4: one-shot edge fit. `edge` is `left`, `right` or `both`
/// (default). Left snaps to the bar containing the first note-on inside the
/// window; right snaps to the bar containing the last note-off inside it.
/// Fitting one side keeps the opposite edge. `window_follow` is left unchanged
/// — this is an explicit adjustment, not a mode switch.
fn fit_window(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let track = i64_or(params, "track", 0).max(0) as usize;
    let clip = i64_or(params, "clip", 0).max(0) as usize;
    let edge = params.get("edge").and_then(Value::as_str).unwrap_or("both");
    if !matches!(edge, "left" | "right" | "both") {
        return Err(bad_params(
            "clip.fit_window",
            "`edge` must be left, right or both",
        ));
    }
    let (data_idx, offset, span, start, end, length, first, last) = {
        let project = session.project();
        let Some(target) = project
            .midi_tracks
            .get(track)
            .and_then(|t| t.clips.get(clip))
        else {
            return Err(bad_params(
                "clip.fit_window",
                format!("no clip {clip} on track {track}"),
            ));
        };
        let Some(data) = project.midi_data.get(target.data as usize) else {
            return Err(bad_params(
                "clip.fit_window",
                "clip references missing data",
            ));
        };
        let window_end = target
            .source_offset
            .saturating_add(i64::from(target.span_ticks));
        let first = data
            .notes()
            .iter()
            .filter(|note| note.start >= target.source_offset && note.start < window_end)
            .map(|note| note.start)
            .min();
        let last = data
            .notes()
            .iter()
            .filter(|note| note.start < window_end && note.end() > target.source_offset)
            .map(|note| note.end())
            .max();
        let end = data
            .notes()
            .iter()
            .map(|note| note.start.saturating_add(note.length))
            .max()
            .unwrap_or(0);
        (
            target.data,
            target.source_offset,
            target.span_ticks,
            target.start_ticks,
            end,
            i64::from(data.length_ticks),
            first,
            last,
        )
    };
    if end > length {
        let new_len = u32::try_from(end).unwrap_or(u32::MAX);
        let source = {
            let project = session.project();
            if project.midi_data_refs(data_idx) > 1 {
                project.midi_data.get(data_idx as usize).cloned()
            } else {
                None
            }
        };
        let project = session.project_mut();
        let data_idx = if let Some(source) = source {
            let new_idx = project.push_midi_data_copy(&source);
            if let Some(target) = project
                .midi_tracks
                .get_mut(track)
                .and_then(|t| t.clips.get_mut(clip))
            {
                target.data = new_idx;
            }
            new_idx
        } else {
            data_idx
        };
        if let Some(data) = project.midi_data_mut(data_idx) {
            data.length_ticks = data.length_ticks.max(new_len);
        }
    }
    let bar = i64::from(session.project().ticks_per_bar_at(start).max(1));
    // F-045 D10: the structural floor is one beat, not one bar — a fit on a
    // short clip must not inflate it to a whole bar.
    let beat = i64::from(session.project().ticks_per_beat_at(start).max(1));
    let mut new_offset = offset;
    let mut new_span = i64::from(span);
    if matches!(edge, "left" | "both")
        && let Some(first) = first
    {
        let right = offset.saturating_add(i64::from(span));
        let floor = first.div_euclid(bar).saturating_mul(bar);
        if floor < right {
            new_offset = floor;
            new_span = (right - floor).max(1);
        }
    }
    if matches!(edge, "right" | "both")
        && let Some(last) = last
    {
        let ceil = last
            .saturating_add(bar.saturating_sub(1))
            .div_euclid(bar)
            .saturating_mul(bar);
        new_span = (ceil - new_offset).max(beat);
    }
    let span = u32::try_from(new_span).unwrap_or(u32::MAX);
    let Some(target) = session
        .project_mut()
        .midi_tracks
        .get_mut(track)
        .and_then(|t| t.clips.get_mut(clip))
    else {
        return Err(bad_params("clip.fit_window", "clip vanished"));
    };
    target.source_offset = new_offset;
    target.span_ticks = span;
    Ok(json!({
        "source_offset": new_offset,
        "span_ticks": span,
        "length_ticks": end.max(0),
        "edge": edge,
    }))
}

/// Points one MIDI clip at another clip's shared content (F-041).
fn link_data(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let track = i64_or(params, "track", 0).max(0) as usize;
    let clip = i64_or(params, "clip", 0).max(0) as usize;
    let source_track = i64_or(params, "source_track", -1);
    let source_clip = i64_or(params, "source_clip", -1);
    if source_track < 0 || source_clip < 0 {
        return Err(bad_params(
            "clip.link_data",
            "source_track and source_clip are required",
        ));
    }
    let source_track = source_track as usize;
    let source_clip = source_clip as usize;
    let data = session
        .project()
        .midi_tracks
        .get(source_track)
        .and_then(|t| t.clips.get(source_clip))
        .map(|clip| clip.data)
        .ok_or_else(|| {
            bad_params(
                "clip.link_data",
                format!("no source clip {source_clip} on track {source_track}"),
            )
        })?;
    let target = session
        .project_mut()
        .midi_tracks
        .get_mut(track)
        .and_then(|t| t.clips.get_mut(clip))
        .ok_or_else(|| bad_params("clip.link_data", format!("no clip {clip} on track {track}")))?;
    target.data = data;
    let refs = session.project().midi_data_refs(data);
    Ok(json!({ "data": data, "refs": refs }))
}

/// Gives one MIDI clip its own copy of the shared content (F-041).
fn unlink_data(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let track = i64_or(params, "track", 0).max(0) as usize;
    let clip = i64_or(params, "clip", 0).max(0) as usize;
    let data = session
        .project()
        .midi_tracks
        .get(track)
        .and_then(|t| t.clips.get(clip))
        .map(|clip| clip.data)
        .ok_or_else(|| {
            bad_params(
                "clip.unlink_data",
                format!("no clip {clip} on track {track}"),
            )
        })?;
    let refs = session.project().midi_data_refs(data);
    if refs <= 1 {
        return Ok(json!({ "data": data, "refs": refs, "unlinked": false }));
    }
    let source = session
        .project()
        .midi_data
        .get(data as usize)
        .cloned()
        .ok_or_else(|| {
            bad_params(
                "clip.unlink_data",
                format!("clip references missing data {data}"),
            )
        })?;
    let new_data = session.project_mut().push_midi_data_copy(&source);
    let target = session
        .project_mut()
        .midi_tracks
        .get_mut(track)
        .and_then(|t| t.clips.get_mut(clip))
        .ok_or_else(|| bad_params("clip.unlink_data", "clip vanished"))?;
    target.data = new_data;
    Ok(json!({ "data": new_data, "refs": 1, "unlinked": true }))
}
