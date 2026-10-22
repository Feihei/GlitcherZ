//! Reading and writing of the project file (`.glz`).
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

/// Current version of the project file format (v6: MIDI tick base 96 → 960
/// PPQ and notes moved from the clip into `MidiClip::sequence` — M1).
///
/// v5 and older documents go through [`Project::migrate_v5_to_v6`] (ticks
/// ×10, legacy notes folded into the sequence); v4 documents additionally go
/// through [`Project::migrate`] (ChannelStrip → mixer, master multi-input →
/// synthetic mixer).
pub const PROJECT_VERSION: u32 = 6;

fn default_version() -> u32 {
    PROJECT_VERSION
}

/// Top-level structure of a `.glz` project file: version number + project
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

/// Saves the project as a `.glz` file (RON pretty format). **Not real-time**
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
    let wrapped = ron::from_str::<ron::value::Value>(text).ok().map(|value| {
        matches!(value, ron::value::Value::Map(ref map) if map.keys().any(|key| {
            matches!(
                key,
                ron::value::Value::String(key) if key == "project" || key == "version"
            )
        }))
    });

    let (mut project, version) = match wrapped {
        // The bare-`Project` development format carries no version, so it is
        // read as current.
        Some(true) => {
            let file: ProjectFile = ron::from_str(text)?;
            (file.project, file.version)
        }
        _ => (ron::from_str(text)?, PROJECT_VERSION),
    };
    project.migrate();
    if version < PROJECT_VERSION {
        project.migrate_v5_to_v6();
    }
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
        AutoParam, AutoPoint, CableDoc, MASTER_NODE_ID, MidiClip, ModelError, NodeDoc, Note,
        Project, TICKS_PER_BAR,
    };

    fn demo_project() -> Project {
        let mut project = Project::default();
        project.metadata.name = "Test Project".to_string();
        project.metadata.tempo_bpm = 140.0;
        project.midi_tracks[0].name = "Bass".to_string();
        let mut clip = MidiClip::new(TICKS_PER_BAR);
        clip.span_ticks = TICKS_PER_BAR;
        clip.sequence.notes = vec![
            Note {
                start: 0,
                length: 480,
                note: 60,
                velocity: crate::DEFAULT_VELOCITY,
                channel: 0,
            },
            Note {
                start: 240,
                length: 240,
                note: 64,
                velocity: 127,
                channel: 0,
            },
        ];
        project.midi_tracks[0].clips.push(clip);
        // Control track 0: one bar clip, cabled to the global mixer's
        // channel 0 gain.
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
        let mixer = project.graph.first_mixer().unwrap().node_id();
        project
            .graph
            .cables
            .push(CableDoc::automation(0, mixer, AutoParam::TrackGain));
        // Patch canvas layout (view data; roundtrip must preserve it).
        project.graph.layout.nodes.insert(mixer, [700.0, 32.0]);
        project.graph.layout.timeline = Some([16.0, 32.0]);
        project.graph.layout.master = Some([930.0, 200.0]);
        project
    }

    // End to end: save → reload → Project equal (required by AGENTS.md
    // step 7).
    #[test]
    fn save_load_roundtrip() {
        let project = demo_project();
        let path =
            std::env::temp_dir().join(format!("glitcherz_roundtrip_{}.glz", std::process::id()));

        save_project(&project, &path).expect("save failed");
        let loaded = load_project(&path).expect("load failed");
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            project, loaded,
            "save→load should fully restore the project"
        );
    }

    // U1: the song loop range and markers live on the metadata and survive
    // the roundtrip.
    #[test]
    fn metadata_loop_and_markers_roundtrip() {
        let mut project = demo_project();
        project.metadata.loop_range = crate::SongLoop {
            enabled: true,
            start_ticks: crate::TICKS_PER_BAR,
            end_ticks: 4 * crate::TICKS_PER_BAR,
        };
        project.metadata.markers = vec![
            crate::Marker {
                tick: 0,
                name: "Intro".to_string(),
            },
            crate::Marker {
                tick: 4 * crate::TICKS_PER_BAR,
                name: "Drop".to_string(),
            },
        ];
        let path = std::env::temp_dir().join(format!("glitcherz_meta_{}.glz", std::process::id()));

        save_project(&project, &path).expect("save failed");
        let loaded = load_project(&path).expect("load failed");
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.metadata.loop_range, project.metadata.loop_range);
        assert_eq!(loaded.metadata.markers, project.metadata.markers);
        assert!(loaded.metadata.loop_range.is_active());
    }

    // U1: old documents without the new metadata fields load with defaults
    // (additive fields, PROJECT_VERSION unchanged).
    #[test]
    fn metadata_defaults_load_old_document() {
        let text = r#"(
    version: 6,
    project: (
        metadata: (name: "old", tempo_bpm: 100.0),
    ),
)"#;
        let loaded = parse_project(text).expect("old metadata should load");
        assert_eq!(loaded.metadata.name, "old");
        assert!((loaded.metadata.tempo_bpm - 100.0).abs() < 1e-9);
        assert_eq!(loaded.metadata.loop_range, crate::SongLoop::default());
        assert!(!loaded.metadata.loop_range.is_active());
        assert!(loaded.metadata.markers.is_empty());
    }

    #[test]
    fn saved_file_carries_version() {
        let path =
            std::env::temp_dir().join(format!("glitcherz_version_{}.glz", std::process::id()));
        save_project(&Project::default(), &path).expect("save failed");
        let text = std::fs::read_to_string(&path).expect("read failed");
        let _ = std::fs::remove_file(&path);

        let file: ProjectFile = ron::from_str(&text).expect("parse failed");
        assert_eq!(file.version, PROJECT_VERSION);
    }

    #[test]
    fn missing_track_fields_survive_load() {
        // Simulates a hand-edited file: midi_tracks has only a name.
        let path =
            std::env::temp_dir().join(format!("glitcherz_compat_{}.glz", std::process::id()));
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

    // ---- v4 → v5 migration (P5f: single-input + mixer) ----

    #[test]
    fn v4_strip_and_master_fanin_migrate_to_mixer() {
        // A v4-style document: per-track strip cabled to master (4× summing
        // into master was legal under the old D4, illegal under D16).
        let text = r#"(
    version: 4,
    project: (
        midi_tracks: [(), (), (), ()],
        graph: (
            nodes: [
                (type: "instrument", node_id: 2, track: 0),
                (type: "effect", node_id: 3, track: 0),
                (type: "channel_strip", node_id: 4, track: 0, gain: 0.6, pan: -0.5, muted: true),
                (type: "instrument", node_id: 5, track: 1),
                (type: "effect", node_id: 6, track: 1),
                (type: "channel_strip", node_id: 7, track: 1),
            ],
            cables: [
                (kind: audio, from: node(2), to: node(3)),
                (kind: audio, from: node(3), to: node(4)),
                (kind: audio, from: node(4), to: master),
                (kind: audio, from: node(5), to: node(6)),
                (kind: audio, from: node(6), to: node(7)),
                (kind: audio, from: node(7), to: master),
                (kind: midi, from: midi_track(0), to: node(2)),
            ],
        ),
    ),
)"#;
        let project = parse_project(text).expect("v4 document should migrate and load");
        // No strip remains; both strips became 2-channel mixers.
        assert!(
            !project
                .graph
                .nodes
                .iter()
                .any(|n| matches!(n, crate::NodeDoc::ChannelStrip { .. }))
        );
        let mixers: Vec<_> = project.graph.mixers().collect();
        assert_eq!(
            mixers.len(),
            3,
            "two converted strips + one synthetic master mixer"
        );
        let NodeDoc::Mixer {
            channels, inputs, ..
        } = mixers[0]
        else {
            panic!("expected a mixer node");
        };
        assert_eq!(*inputs, 2);
        assert_eq!(channels.len(), 2);
        assert!((channels[0].gain - 0.6).abs() < 1e-6);
        assert!((channels[0].pan + 0.5).abs() < 1e-6);
        assert!(channels[0].muted, "strip mute should carry into channel 0");

        // Master received 2 cables → a synthetic mixer now absorbs them.
        let master_ins = project
            .graph
            .cables
            .iter()
            .filter(|c| c.kind == crate::CableKind::Audio && c.to == crate::PortRef::Master)
            .count();
        assert_eq!(master_ins, 1, "master must be single-input after migration");
        let synth_mixer = project
            .graph
            .cables
            .iter()
            .find(|c| c.kind == crate::CableKind::Audio && c.to == crate::PortRef::Master)
            .and_then(|c| match c.from {
                crate::PortRef::Node(id) => Some(id),
                _ => None,
            })
            .expect("the single master feed should come from the synthetic mixer");
        assert!(project.graph.node_by_id(synth_mixer).unwrap().is_mixer());
        // Re-validating the migrated document passes (idempotent shape).
        project
            .validate()
            .expect("migrated document should validate");
    }

    /// A v5 document: notes live directly on the clip, ticks are 96 PPQ and
    /// velocity is 0..1. Loading must preserve the musical result.
    const V5_DOC: &str = r#"(
    version: 5,
    project: (
        midi_tracks: [
            (
                name: "Lead",
                clips: [
                    (
                        start_ticks: 384,
                        span_ticks: 384,
                        loop_ticks: 384,
                        notes: [
                            (start: 0, length: 48, note: 60, velocity: 0.9),
                            (start: 24, length: 24, note: 64, velocity: 1.0),
                        ],
                    ),
                ],
            ),
        ],
        control_tracks: [
            (
                name: "Ride",
                clips: [
                    (
                        start_ticks: 192,
                        span_ticks: 384,
                        loop_ticks: 384,
                        curve: [(tick: 0, value: 0.0), (tick: 96, value: 1.0)],
                    ),
                ],
            ),
        ],
    ),
)"#;

    #[test]
    fn v5_ticks_are_rescaled_by_ten() {
        let project = parse_project(V5_DOC).expect("v5 document should migrate and load");
        let clip = &project.midi_tracks[0].clips[0];
        // 384 ticks @96 PPQ = one bar → 3840 @960 PPQ, still one bar.
        assert_eq!(clip.start_ticks, 3840);
        assert_eq!(clip.span_ticks, 3840);
        assert_eq!(clip.loop_ticks, 3840);
        assert_eq!(clip.start_ticks, crate::TICKS_PER_BAR);
        // Automation clips and their anchors rescale too.
        let ctrl = &project.control_tracks[0].clips[0];
        assert_eq!(ctrl.start_ticks, 1920);
        let ticks: Vec<u32> = ctrl.curve.iter().map(|p| p.tick).collect();
        assert_eq!(ticks, vec![0, 960]);
    }

    #[test]
    fn v5_notes_move_into_the_sequence_with_midi_velocity() {
        let project = parse_project(V5_DOC).expect("v5 document should migrate and load");
        let clip = &project.midi_tracks[0].clips[0];
        assert_eq!(clip.notes().len(), 2, "legacy notes must not be lost");
        assert!(
            clip.legacy_notes.is_empty(),
            "the legacy field is consumed, not carried"
        );
        // Notes are sorted by (start, note) after the upgrade.
        let first = &clip.notes()[0];
        assert_eq!((first.start, first.note, first.length), (0, 60, 480));
        assert_eq!(first.velocity, 114, "0.9 × 127 = 114.3, rounded to 114");
        let second = &clip.notes()[1];
        assert_eq!((second.start, second.note, second.length), (240, 64, 240));
        assert_eq!(second.velocity, 127, "1.0 × 127 is the maximum");
        assert!(
            clip.sequence
                .notes
                .windows(2)
                .all(|w| (w[0].start, w[0].note) <= (w[1].start, w[1].note)),
            "the sort invariant must hold after migration"
        );
    }

    #[test]
    fn v6_documents_are_left_alone() {
        let mut project = Project::default();
        project.midi_tracks[0]
            .clips
            .push(MidiClip::new(TICKS_PER_BAR));
        let text = ron::ser::to_string_pretty(
            &ProjectFile {
                version: PROJECT_VERSION,
                project: project.clone(),
            },
            ron::ser::PrettyConfig::new(),
        )
        .expect("serialize");
        let back = parse_project(&text).expect("current document should load");
        assert_eq!(
            back.midi_tracks[0].clips[0].start_ticks, TICKS_PER_BAR,
            "a v6 document must not be rescaled again"
        );
        assert_eq!(back.midi_tracks.len(), project.midi_tracks.len());
    }

    #[test]
    fn legacy_velocity_of_zero_becomes_the_lowest_note_on() {
        // A v5 velocity of 0 means a silent note; in MIDI wire terms the
        // lowest legal note-on is 1, never 0 (that is a note-off).
        let legacy = crate::LegacyNote {
            start: 0,
            length: 24,
            note: 60,
            velocity: 0.0,
        };
        assert_eq!(legacy.upgrade().velocity, crate::MIN_VELOCITY);
        let nan = crate::LegacyNote {
            velocity: f32::NAN,
            ..legacy
        };
        assert_eq!(
            nan.upgrade().velocity,
            crate::DEFAULT_VELOCITY,
            "a non-finite velocity falls back to the default"
        );
    }
}
