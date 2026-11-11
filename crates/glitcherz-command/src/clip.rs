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
    let new_data = session.project_mut().push_midi_data(source);
    let target = session
        .project_mut()
        .midi_tracks
        .get_mut(track)
        .and_then(|t| t.clips.get_mut(clip))
        .ok_or_else(|| bad_params("clip.unlink_data", "clip vanished"))?;
    target.data = new_data;
    Ok(json!({ "data": new_data, "refs": 1, "unlinked": true }))
}
