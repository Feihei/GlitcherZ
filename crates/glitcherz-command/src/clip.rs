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
    vec![crate::cmd!(
        "clip.add",
        "Add Clip",
        ["Event", "Edit"],
        None,
        "{track, lane? midi, start_ticks? 0, span_ticks? bar, name?} → {clip, start_ticks, span_ticks, loop_ticks}",
        always,
        add
    )]
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
    let Some(track) = session.project_mut().midi_tracks.get_mut(track) else {
        return Err(bad_params("clip.add", format!("no MIDI track {raw_track}")));
    };
    if track.clips.len() >= MAX_CLIPS_PER_TRACK {
        return Err(bad_params(
            "clip.add",
            format!("track already holds {MAX_CLIPS_PER_TRACK} clips"),
        ));
    }
    track.clips.push(MidiClip {
        name,
        start_ticks: start,
        span_ticks: span,
        loop_ticks: span,
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
