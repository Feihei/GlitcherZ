//! Project file plumbing (U2): window title, MIDI import/export helpers, and
//! the placement arithmetic for imported tracks.
//!
//! Everything here is pure or filesystem-only. The app shell owns the dialogs,
//! undo guards and engine pushes, so this module stays testable without an
//! egui context.

use std::path::Path;

use glitcherz_model::{MAX_MIDI_TRACKS, MidiTrack, Project, smf};

use crate::display_name;

/// Window title: `GlitcherZ — <name>` plus a `*` while the document has
/// unsaved changes. An unsaved project is called `Untitled`.
pub fn window_title(path: Option<&Path>, dirty: bool) -> String {
    let name = path.map_or_else(|| "Untitled".to_string(), display_name);
    let marker = if dirty { "*" } else { "" };
    format!("GlitcherZ — {name}{marker}")
}

/// Base file name offered by the "Export MIDI" dialog (the project's own
/// stem, `Untitled.mid` before the first save).
pub fn midi_export_name(path: Option<&Path>) -> String {
    let stem = path
        .and_then(Path::file_stem)
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "Untitled".to_string());
    format!("{stem}.mid")
}

/// Whether a track can accept imported content without losing anything: no
/// notes and no controller events in any of its clips.
pub fn is_empty_track(track: &MidiTrack) -> bool {
    track
        .clips
        .iter()
        .all(|clip| clip.sequence.notes.is_empty() && clip.sequence.ctrls.is_empty())
}

/// Placement plan for importing tracks into a project that already has
/// `tracks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPlan {
    /// Existing empty tracks to fill, in track order.
    pub reuse: Vec<usize>,
    /// Tracks that need a brand-new MIDI track (nodes + cables).
    pub append: usize,
    /// Imported tracks that do not fit under [`MAX_MIDI_TRACKS`].
    pub dropped: usize,
}

/// Empty tracks are reused before new ones are appended, so importing into a
/// fresh project does not leave an empty track behind (U2).
pub fn plan_import(tracks: &[MidiTrack], imported: usize) -> ImportPlan {
    let reuse: Vec<usize> = tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| is_empty_track(track))
        .map(|(index, _)| index)
        .take(imported)
        .collect();
    let free = MAX_MIDI_TRACKS.saturating_sub(tracks.len());
    let append = imported.saturating_sub(reuse.len()).min(free);
    let dropped = imported.saturating_sub(reuse.len() + append);
    ImportPlan {
        reuse,
        append,
        dropped,
    }
}

/// Writes the project as a format-1 SMF; returns the byte count.
pub fn write_midi(path: &Path, project: &Project) -> std::io::Result<usize> {
    let bytes = smf::export_smf(project);
    std::fs::write(path, &bytes)?;
    Ok(bytes.len())
}

/// Reads a MIDI file into tracks plus the file's document-level header (its
/// meter, U3a). The hostile-input parsing lives in `glitcherz_model::smf`;
/// this adds the IO and a display-ready error.
pub fn read_midi(path: &Path) -> Result<smf::SmfImport, String> {
    let bytes = std::fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
    smf::import_smf(&bytes).map_err(|err| format!("{}: {err}", path.display()))
}

/// Whether the project holds no musical content at all (every MIDI track and
/// control track empty).
///
/// Importing into such a project is really "open this file", so the file's
/// meter is adopted (U3a). Once there is content, an import is an *addition*:
/// silently changing the meter would re-measure the grid under the user's
/// existing clips, so the project keeps its own and the status line reports
/// the difference.
pub fn is_unwritten(project: &Project) -> bool {
    project.midi_tracks.iter().all(is_empty_track)
        && project
            .control_tracks
            .iter()
            .all(|track| track.clips.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    use glitcherz_model::{MidiClip, Note};

    /// A project with one track holding one clip of `notes` notes.
    fn project_with_notes(count: usize) -> Project {
        let mut project = Project::default();
        let mut clip = MidiClip::new(0);
        for i in 0..count {
            clip.sequence.notes.push(Note {
                start: i as i64 * glitcherz_model::TICKS_PER_STEP as i64,
                length: glitcherz_model::TICKS_PER_STEP as i64,
                note: 60 + (i % 12) as u8,
                velocity: 100,
                channel: 0,
            });
        }
        project.midi_tracks[0].clips.push(clip);
        project
    }

    fn track_with_notes(name: &str) -> MidiTrack {
        let mut track = MidiTrack {
            name: name.to_string(),
            clips: Vec::new(),
        };
        let mut clip = MidiClip::new(0);
        clip.sequence.notes.push(Note {
            start: 0,
            length: 960,
            note: 64,
            velocity: 90,
            channel: 0,
        });
        track.clips.push(clip);
        track
    }

    #[test]
    fn title_marks_unsaved_changes() {
        assert_eq!(window_title(None, false), "GlitcherZ — Untitled");
        assert_eq!(window_title(None, true), "GlitcherZ — Untitled*");
        let path = Path::new("/music/song.glz");
        assert_eq!(window_title(Some(path), false), "GlitcherZ — song.glz");
        assert_eq!(window_title(Some(path), true), "GlitcherZ — song.glz*");
    }

    #[test]
    fn export_name_follows_the_project_stem() {
        assert_eq!(midi_export_name(None), "Untitled.mid");
        assert_eq!(
            midi_export_name(Some(Path::new("/a/b/Song.glz"))),
            "Song.mid"
        );
        // A path without a stem falls back to the default name.
        assert_eq!(midi_export_name(Some(Path::new("/"))), "Untitled.mid");
    }

    #[test]
    fn empty_track_means_no_notes_and_no_ctrls() {
        let empty = MidiTrack::default();
        assert!(is_empty_track(&empty));
        // A blank clip is still empty.
        let blank = MidiTrack {
            name: "blank".to_string(),
            clips: vec![MidiClip::new(0)],
        };
        assert!(is_empty_track(&blank));
        assert!(!is_empty_track(&track_with_notes("lead")));
    }

    #[test]
    fn import_reuses_empty_tracks_before_appending() {
        // A fresh project ships with DEFAULT_MIDI_TRACKS empty tracks, so a
        // small import lands entirely in them.
        let project = Project::default();
        let plan = plan_import(&project.midi_tracks, 3);
        assert_eq!(plan.reuse, vec![0, 1, 2]);
        assert_eq!(plan.append, 0);
        assert_eq!(plan.dropped, 0);

        // More tracks than empty slots: the rest are appended.
        let plan = plan_import(
            &project.midi_tracks,
            glitcherz_model::DEFAULT_MIDI_TRACKS + 2,
        );
        assert_eq!(plan.reuse.len(), glitcherz_model::DEFAULT_MIDI_TRACKS);
        assert_eq!(plan.append, 2);
        assert_eq!(plan.dropped, 0);
    }

    #[test]
    fn import_appends_past_populated_tracks() {
        let tracks = vec![track_with_notes("a"), track_with_notes("b")];
        let plan = plan_import(&tracks, 2);
        assert!(plan.reuse.is_empty());
        assert_eq!(plan.append, 2);
        assert_eq!(plan.dropped, 0);
    }

    #[test]
    fn import_respects_the_track_cap() {
        // 14 populated + 1 empty (15 tracks) + 5 imported: the empty track is
        // reused, one track is appended (cap 16), three are dropped.
        let mut tracks: Vec<MidiTrack> = (0..MAX_MIDI_TRACKS - 2)
            .map(|i| track_with_notes(&format!("t{i}")))
            .collect();
        tracks.push(MidiTrack::default());
        assert_eq!(tracks.len(), MAX_MIDI_TRACKS - 1);
        let plan = plan_import(&tracks, 5);
        assert_eq!(plan.reuse, vec![MAX_MIDI_TRACKS - 2]);
        assert_eq!(plan.append, 1);
        assert_eq!(plan.dropped, 3);

        // A full, populated project swallows nothing.
        let full: Vec<MidiTrack> = (0..MAX_MIDI_TRACKS)
            .map(|i| track_with_notes(&format!("t{i}")))
            .collect();
        let plan = plan_import(&full, 4);
        assert!(plan.reuse.is_empty());
        assert_eq!(plan.append, 0);
        assert_eq!(plan.dropped, 4);
    }

    #[test]
    fn smf_file_roundtrip_keeps_the_notes() {
        let dir = std::env::temp_dir().join(format!(
            "glitcherz_u2_smf_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("roundtrip.mid");
        let project = project_with_notes(3);
        let bytes = write_midi(&path, &project).expect("write smf");
        assert!(bytes > 0);
        let tracks = read_midi(&path).expect("read smf");
        assert_eq!(tracks.tracks.len(), 1);
        assert_eq!(tracks.tracks[0].clips.len(), 1);
        assert_eq!(tracks.tracks[0].clips[0].sequence.notes.len(), 3);
        // The imported clip sits at the earliest note, which is the start.
        assert_eq!(tracks.tracks[0].clips[0].start_ticks, 0);
        // Our own export always carries a meter, so the roundtrip sees 4/4.
        assert_eq!(
            tracks.time_signature,
            Some(glitcherz_model::TimeSignature::COMMON)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritten_project_adopts_an_imported_meter() {
        // A fresh project has no content: an import there is really "open".
        assert!(is_unwritten(&Project::default()));
        assert!(!is_unwritten(&project_with_notes(1)));

        let mut project = Project::default();
        project
            .control_tracks
            .push(glitcherz_model::ControlTrack::default());
        assert!(
            is_unwritten(&project),
            "an empty control track is still no content"
        );
        project.control_tracks[0]
            .clips
            .push(glitcherz_model::ControlClip::new(0));
        assert!(!is_unwritten(&project), "a control clip is content too");
    }
}
