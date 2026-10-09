//! Reading and writing of the project file (`.rez`).
//!
//! File format: RON-serialized [`ProjectFile`] (version number + project
//! document); pretty output for easy manual inspection and version diffing.
//!
//! Backward compatibility policy: `version` records the file format version;
//! **new fields always get defaults via `#[serde(default)]`** (see the crate
//! documentation conventions), so missing fields are automatically upgraded
//! when reading older files; unknown fields are ignored by serde. Currently
//! all files with a version ≤ [`PROJECT_VERSION`] are read with the same
//! field-level tolerance; if a future version requires a destructive
//! migration, handle it by branching on `version` in this module.

use std::path::Path;

use crate::Project;

/// Current version of the project file format (v4: arrangement timeline
/// clips — `MidiClip` / `ControlClip` per track, D14-D17; the step sequencer
/// model is retired).
///
/// Compatibility policy (Q5 decision): **no legacy read paths** --
/// structural changes are naturally tolerated via serde field-level defaults
/// for missing fields; fields in old files that cannot be mapped (like the
/// old `tracks` array) are ignored, and the loaded result is the default
/// document.
pub const PROJECT_VERSION: u32 = 4;

fn default_version() -> u32 {
    PROJECT_VERSION
}

/// Top-level structure of a `.rez` project file: version number + project
/// document.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct ProjectFile {
    /// File format version.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Project document.
    #[serde(default)]
    pub project: Project,
}

/// Model layer error.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("file read/write failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("project file parse failed: {0}")]
    Parse(#[from] ron::error::SpannedError),
    #[error("project file serialization failed: {0}")]
    Serialize(#[from] ron::error::Error),
    #[error("invalid project file graph structure: {0}")]
    InvalidGraph(String),
}

/// Saves the project as a `.rez` file (RON pretty format). **Not real-time**
/// (disk IO).
pub fn save_project(project: &Project, path: &Path) -> Result<(), ModelError> {
    project.validate().map_err(ModelError::InvalidGraph)?;
    let file = ProjectFile {
        version: PROJECT_VERSION,
        project: project.clone(),
    };
    let text = ron::ser::to_string_pretty(&file, ron::ser::PrettyConfig::new())?;
    std::fs::write(path, text)?;
    Ok(())
}

/// Parses project file text.
///
/// First probes the top-level structure with `ron::Value`: if it contains
/// the `project` / `version` keys it parses as the standard [`ProjectFile`]
/// wrapper; otherwise it parses as a bare `Project` (development-time
/// temporary format). You cannot rely only on "whether `ProjectFile` parses"
/// -- serde would treat all fields of a bare Project as unknown fields,
/// ignore them, and "successfully" produce an empty project, silently losing
/// data.
pub fn parse_project(text: &str) -> Result<Project, ModelError> {
    let is_wrapped = ron::from_str::<ron::value::Value>(text)
        .ok()
        .map(|value| match value {
            ron::value::Value::Map(map) => map.keys().any(|key| {
                matches!(
                    key,
                    ron::value::Value::String(key) if key == "project" || key == "version"
                )
            }),
            _ => false,
        })
        .unwrap_or(false);

    let project = if is_wrapped {
        let file: ProjectFile = ron::from_str(text)?;
        file.project
    } else {
        ron::from_str(text)?
    };
    project.validate().map_err(ModelError::InvalidGraph)?;
    Ok(project)
}

/// Loads the project from disk. **Not real-time** (disk IO).
pub fn load_project(path: &Path) -> Result<Project, ModelError> {
    let text = std::fs::read_to_string(path)?;
    parse_project(&text)
}

#[cfg(test)]
mod tests {
    use super::{PROJECT_VERSION, ProjectFile, load_project, parse_project, save_project};
    use crate::{
        AutoParam, AutoPoint, CableDoc, MASTER_NODE_ID, MidiClip, ModelError, Note, Project,
        TICKS_PER_BAR,
    };

    fn demo_project() -> Project {
        let mut project = Project::default();
        project.metadata.name = "Test Project".to_string();
        project.metadata.tempo_bpm = 140.0;
        project.midi_tracks[0].name = "Bass".to_string();
        let mut clip = MidiClip::new(TICKS_PER_BAR);
        clip.span_ticks = TICKS_PER_BAR;
        clip.notes = vec![
            Note {
                start: 0,
                length: 48,
                note: 60,
                velocity: 0.9,
            },
            Note {
                start: 24,
                length: 24,
                note: 64,
                velocity: 1.0,
            },
        ];
        project.midi_tracks[0].clips.push(clip);
        // Control track 0: one bar clip, cabled to track 0's channel strip
        // gain.
        project.control_tracks.push(crate::ControlTrack {
            name: String::new(),
            clips: vec![crate::ControlClip {
                start_ticks: 0,
                span_ticks: TICKS_PER_BAR,
                loop_ticks: TICKS_PER_BAR,
                curve: vec![AutoPoint {
                    tick: 0,
                    value: 1.0,
                }],
            }],
        });
        let strip = project.graph.channel_strip_of_track(0).unwrap().node_id();
        project
            .graph
            .cables
            .push(CableDoc::automation(0, strip, AutoParam::TrackGain));
        // Patch canvas layout (view data; roundtrip must preserve it).
        project.graph.layout.nodes.insert(strip, [700.0, 32.0]);
        project.graph.layout.timeline = Some([16.0, 32.0]);
        project.graph.layout.master = Some([930.0, 200.0]);
        project
    }

    // End to end: save → reload → Project equal (required by AGENTS.md
    // step 7).
    #[test]
    fn save_load_roundtrip() {
        let project = demo_project();
        let path = std::env::temp_dir().join(format!("resoz_roundtrip_{}.rez", std::process::id()));

        save_project(&project, &path).expect("save failed");
        let loaded = load_project(&path).expect("load failed");
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            project, loaded,
            "save→load should fully restore the project"
        );
    }

    #[test]
    fn saved_file_carries_version() {
        let path = std::env::temp_dir().join(format!("resoz_version_{}.rez", std::process::id()));
        save_project(&Project::default(), &path).expect("save failed");
        let text = std::fs::read_to_string(&path).expect("read failed");
        let _ = std::fs::remove_file(&path);

        let file: ProjectFile = ron::from_str(&text).expect("parse failed");
        assert_eq!(file.version, PROJECT_VERSION);
    }

    #[test]
    fn missing_track_fields_survive_load() {
        // Simulates a hand-edited file: midi_tracks has only a name.
        let path = std::env::temp_dir().join(format!("resoz_compat_{}.rez", std::process::id()));
        std::fs::write(
            &path,
            r#"(
    version: 4,
    project: (
        midi_tracks: [(name: "name only"), (), (), ()],
    ),
)"#,
        )
        .expect("write failed");
        let loaded = load_project(&path).expect("load failed");
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.midi_tracks[0].name, "name only");
        assert!((loaded.metadata.tempo_bpm - 120.0).abs() < 1e-9);
        // Missing graph field = empty graph (whatever the document says, D13).
        assert!(loaded.graph.nodes.is_empty());
        // Missing layout field = empty layout (UI falls back to auto-placement).
        assert!(loaded.graph.layout.nodes.is_empty());
        assert!(loaded.graph.layout.timeline.is_none());
    }

    #[test]
    fn bare_project_text_is_accepted() {
        // The development-time bare Project format should be readable
        // directly.
        let project = demo_project();
        let text = ron::ser::to_string(&project).expect("serialization failed");
        let parsed = parse_project(&text).expect("parse failed");
        assert_eq!(project, parsed);
    }

    #[test]
    fn invalid_graph_is_rejected_on_load() {
        // Cable references a nonexistent node: report an error instead of
        // truncating (the file layer is the last line of defense).
        // Note: PortRef is serialized as snake_case via rename_all
        // (node/master).
        let text = r#"(
    version: 4,
    project: (
        graph: (cables: [(kind: audio, from: node(99), to: master)]),
    ),
)"#;
        assert!(matches!(
            parse_project(text),
            Err(ModelError::InvalidGraph(_))
        ));
        // Automation track port out of range.
        let text = r#"(
    version: 4,
    project: (
        graph: (cables: [(kind: automation, from: auto_track(3), to: node(2))]),
    ),
)"#;
        assert!(matches!(
            parse_project(text),
            Err(ModelError::InvalidGraph(_))
        ));
    }

    #[test]
    fn reserved_master_node_id_is_rejected() {
        let text = r#"(
    version: 4,
    project: (
        graph: (nodes: [(type: "channel_strip", node_id: 1, track: 0)]),
    ),
)"#;
        assert!(matches!(
            parse_project(text),
            Err(ModelError::InvalidGraph(_))
        ));
        let _ = MASTER_NODE_ID;
    }
}
