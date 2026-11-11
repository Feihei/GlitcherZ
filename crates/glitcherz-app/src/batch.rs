//! Batch execution over the command registry (S4, spec `cli-commands.md`).
//!
//! The registry owns every transactional guarantee — a snapshot before each
//! command, rollback on error or panic, undo entries, the journal. This module
//! only decides *what runs in what order* and what the outcome looks like.
//!
//! A batch is all-or-nothing: the first failing command ends the run and the
//! caller writes nothing, so `--save` / `--wav` can never publish a
//! half-edited document. That is the same promise `Session::execute` makes for
//! a single command, lifted one level up.

use std::io::Read;
use std::path::Path;

use glitcherz_command::{CommandError, Session};
use serde_json::{Map, Value, json};

use crate::args::StepSource;

/// A resolved step: the id plus its parameter object.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub id: String,
    pub params: Value,
}

/// One command's outcome.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub id: String,
    pub result: Value,
}

/// Why a batch stopped early, or which stage failed.
#[derive(Debug, Clone)]
pub enum Failure {
    /// The starting document could not be read (exit 3).
    Load(String),
    /// `--ops` could not be read or parsed (exit 2).
    Resolve(String),
    /// A command failed (exit 4).
    Step { id: String, error: CommandError },
    /// Writing the result failed (exit 3).
    Save(String),
    /// Rendering the result failed (exit 3).
    Render(String),
}

impl Failure {
    /// The process exit code this failure maps to.
    pub fn exit_code(&self) -> u8 {
        match self {
            Failure::Resolve(_) => 2,
            Failure::Load(_) | Failure::Save(_) | Failure::Render(_) => 3,
            Failure::Step { .. } => 4,
        }
    }

    /// Short machine-readable stage name.
    fn stage(&self) -> &'static str {
        match self {
            Failure::Load(_) => "load",
            Failure::Resolve(_) => "ops",
            Failure::Step { .. } => "command",
            Failure::Save(_) => "save",
            Failure::Render(_) => "render",
        }
    }

    /// The failure as one human-readable line.
    pub fn message(&self) -> String {
        match self {
            Failure::Load(err)
            | Failure::Resolve(err)
            | Failure::Save(err)
            | Failure::Render(err) => err.clone(),
            Failure::Step { error, .. } => error.to_string(),
        }
    }

    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("stage".to_string(), Value::String(self.stage().to_string()));
        map.insert("error".to_string(), Value::String(self.message()));
        if let Failure::Step { id, error } = self {
            map.insert("id".to_string(), Value::String(id.clone()));
            map.insert(
                "kind".to_string(),
                Value::String(error_kind(error).to_string()),
            );
        }
        Value::Object(map)
    }
}

/// `CommandError` variant name, lowercased — the vocabulary an MCP client
/// branches on ("you typed a bad id" vs "that edit does not apply right now").
pub fn error_kind(error: &CommandError) -> &'static str {
    match error {
        CommandError::Unknown(_) => "unknown",
        CommandError::Unavailable(_, _) => "unavailable",
        CommandError::BadParams { .. } => "bad_params",
        CommandError::Panicked { .. } => "panicked",
    }
}

/// Everything the invocation produced, ready to print.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// The `--open` path, when there was one.
    pub project: Option<String>,
    pub outcomes: Vec<Outcome>,
    pub failure: Option<Failure>,
    pub saved: Option<String>,
    pub render: Option<RenderInfo>,
}

#[derive(Debug, Clone)]
pub struct RenderInfo {
    pub path: String,
    pub frames: u32,
    pub sample_rate: u32,
}

impl Report {
    /// The machine-readable envelope (`--json`).
    ///
    /// Optional keys are *absent*, not `null`: consumers test for the key,
    /// which is harder to get wrong than testing for emptiness.
    pub fn to_json(&self) -> Value {
        let mut root = Map::new();
        root.insert("ok".to_string(), Value::Bool(self.failure.is_none()));
        if let Some(project) = &self.project {
            root.insert("project".to_string(), Value::String(project.clone()));
        }
        root.insert(
            "commands".to_string(),
            Value::Array(
                self.outcomes
                    .iter()
                    .map(
                        |outcome| json!({ "id": outcome.id, "ok": true, "result": outcome.result }),
                    )
                    .collect(),
            ),
        );
        if let Some(failure) = &self.failure {
            root.insert("failed".to_string(), failure.to_json());
        }
        if let Some(saved) = &self.saved {
            root.insert("saved".to_string(), Value::String(saved.clone()));
        }
        if let Some(render) = &self.render {
            root.insert(
                "render".to_string(),
                json!({
                    "path": render.path,
                    "frames": render.frames,
                    "sample_rate": render.sample_rate,
                }),
            );
        }
        Value::Object(root)
    }

    /// The human-readable transcript.
    pub fn to_text(&self) -> String {
        let mut text = String::new();
        if let Some(project) = &self.project {
            text.push_str(&format!("loaded {project}\n"));
        }
        for outcome in &self.outcomes {
            text.push_str(&format!("ok  {:<20} {}\n", outcome.id, outcome.result));
        }
        if let Some(failure) = &self.failure {
            // Every `CommandError` variant already names its command, so the id
            // is not repeated as a column here.
            text.push_str(&format!("error  {}\n", failure.message()));
        }
        if let Some(saved) = &self.saved {
            text.push_str(&format!("saved {saved}\n"));
        }
        if let Some(render) = &self.render {
            text.push_str(&format!(
                "rendered {} frames at {} Hz to {}\n",
                render.frames, render.sample_rate, render.path
            ));
        }
        text
    }
}

/// Reads one `--ops` source into steps.
///
/// `-` means stdin, so a caller can pipe a batch in without a temp file.
pub fn load_ops(source: &Path) -> Result<Vec<Step>, String> {
    let stdin = source.as_os_str() == "-";
    let text = if stdin {
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .map_err(|err| "stdin: ".to_string() + &err.to_string())?;
        buffer
    } else {
        std::fs::read_to_string(source).map_err(|err| format!("{}: {err}", source.display()))?
    };
    parse_ops(&text).map_err(|err| {
        if stdin {
            format!("stdin: {err}")
        } else {
            format!("{}: {err}", source.display())
        }
    })
}

/// Parses an ops document: `[{"id": "...", "params": {…}}, …]`.
///
/// Unknown keys inside a step are rejected rather than ignored — a typo like
/// `"command"` would otherwise turn into "unknown command ``" much later.
pub fn parse_ops(text: &str) -> Result<Vec<Step>, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|err| format!("not valid JSON: {err}"))?;
    let array = value
        .as_array()
        .ok_or_else(|| "the ops document must be a JSON array".to_string())?;
    let mut steps = Vec::with_capacity(array.len());
    for (index, item) in array.iter().enumerate() {
        let object = item
            .as_object()
            .ok_or_else(|| format!("step {index} is not a JSON object"))?;
        if let Some(unknown) = object.keys().find(|key| *key != "id" && *key != "params") {
            return Err(format!(
                "step {index} has an unknown key `{unknown}` (only `id` and `params` are allowed)"
            ));
        }
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("step {index} needs an `id` string"))?;
        let params = object
            .get("params")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        if !params.is_object() {
            return Err(format!("step {index} `params` must be a JSON object"));
        }
        steps.push(Step {
            id: id.to_string(),
            params,
        });
    }
    Ok(steps)
}

/// Resolves every step source in argv order.
pub fn resolve(sources: &[StepSource]) -> Result<Vec<Step>, Failure> {
    let mut steps = Vec::new();
    for source in sources {
        match source {
            StepSource::Inline { id, params } => steps.push(Step {
                id: id.clone(),
                params: params.clone(),
            }),
            StepSource::Ops(path) => steps.extend(load_ops(path).map_err(Failure::Resolve)?),
        }
    }
    Ok(steps)
}

/// Runs the steps against `session`, stopping at the first failure.
///
/// Returns the outcomes collected so far plus the failure, if any. The caller
/// must not write anything when this returns a failure.
pub fn run(session: &mut Session, steps: &[Step]) -> (Vec<Outcome>, Option<Failure>) {
    let mut outcomes = Vec::with_capacity(steps.len());
    for step in steps {
        match session.execute(&step.id, &step.params) {
            Ok(result) => outcomes.push(Outcome {
                id: step.id.clone(),
                result,
            }),
            Err(error) => {
                let failure = Failure::Step {
                    id: step.id.clone(),
                    error,
                };
                return (outcomes, Some(failure));
            }
        }
    }
    (outcomes, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glitcherz_model::{MidiClip, Note, Project};

    fn project_with_one_note() -> Project {
        let mut project = Project::default();
        let data = project.push_midi_data(glitcherz_model::MidiData {
            length_ticks: 960,
            ..glitcherz_model::MidiData::default()
        });
        project.midi_data_mut(data).unwrap().notes_mut().push(Note {
            start: 0,
            length: 960,
            note: 60,
            velocity: 100,
            channel: 0,
        });
        project.midi_tracks[0].clips.push(MidiClip {
            start_ticks: 0,
            span_ticks: 960,
            data,
            ..MidiClip::default()
        });
        project
    }

    fn inline(id: &str, params: Value) -> StepSource {
        StepSource::Inline {
            id: id.to_string(),
            params,
        }
    }

    #[test]
    fn ops_parse_accepts_the_documented_shape() {
        let steps = parse_ops(
            r#"[{"id": "midi.query", "params": {"track": 0}}, {"id": "midi.transpose"}]"#,
        )
        .expect("valid ops");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].params["track"], 0);
        assert_eq!(steps[1].params, Value::Object(Map::new()));
    }

    #[test]
    fn ops_parse_rejects_the_shapes_that_hide_typos() {
        assert!(
            parse_ops(r#"{"id": "midi.query"}"#).is_err(),
            "not an array"
        );
        assert!(parse_ops(r#"[{"params": {}}]"#).is_err(), "no id");
        assert!(
            parse_ops(r#"[{"id": "midi.query", "params": []}]"#).is_err(),
            "params must be an object"
        );
        assert!(
            parse_ops(r#"[{"id": "a", "command": "b"}]"#).is_err(),
            "unknown keys are rejected"
        );
        assert!(parse_ops("not json").is_err());
    }

    #[test]
    fn resolve_keeps_argv_order() {
        let steps = resolve(&[
            inline("midi.transpose", json!({ "semitones": 12 })),
            inline("midi.query", json!({})),
        ])
        .expect("resolves");
        assert_eq!(
            steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["midi.transpose", "midi.query"]
        );
    }

    #[test]
    fn a_failing_step_stops_the_run_and_reports_its_kind() {
        let mut session = Session::new(project_with_one_note());
        let steps = resolve(&[
            inline("midi.transpose", json!({ "semitones": 12 })),
            inline("midi.nope", json!({})),
            inline("midi.transpose", json!({ "semitones": 12 })),
        ])
        .expect("resolves");
        let (outcomes, failure) = run(&mut session, &steps);
        assert_eq!(outcomes.len(), 1, "only the first command ran");
        let failure = failure.expect("fails");
        assert_eq!(failure.exit_code(), 4);
        assert_eq!(failure.stage(), "command");
        match &failure {
            Failure::Step { id, error } => {
                assert_eq!(id, "midi.nope");
                assert_eq!(error_kind(error), "unknown");
            }
            other => panic!("expected a step failure, got {other:?}"),
        }
    }

    #[test]
    fn a_clean_run_reports_every_outcome() {
        let mut session = Session::new(project_with_one_note());
        let steps = resolve(&[
            inline("midi.transpose", json!({ "semitones": 12 })),
            inline("midi.query", json!({ "track": 0, "clip": 0 })),
        ])
        .expect("resolves");
        let (outcomes, failure) = run(&mut session, &steps);
        assert!(failure.is_none());
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[1].result["notes"][0]["note"], 72);
    }

    #[test]
    fn the_envelope_omits_keys_instead_of_nulling_them() {
        let report = Report {
            project: Some("in.glz".to_string()),
            outcomes: vec![Outcome {
                id: "midi.query".to_string(),
                result: json!({ "notes": [] }),
            }],
            failure: None,
            saved: None,
            render: None,
        };
        let value = report.to_json();
        assert_eq!(value["ok"], true);
        assert_eq!(value["commands"][0]["id"], "midi.query");
        assert!(value.get("saved").is_none());
        assert!(value.get("render").is_none());
        assert!(value.get("project").is_some());
    }

    #[test]
    fn the_envelope_names_the_stage_for_non_command_failures() {
        let report = Report {
            project: None,
            outcomes: Vec::new(),
            failure: Some(Failure::Load("in.glz: nope".to_string())),
            saved: None,
            render: None,
        };
        let value = report.to_json();
        assert_eq!(value["ok"], false);
        assert_eq!(value["failed"]["stage"], "load");
        assert_eq!(value["failed"]["error"], "in.glz: nope");
        assert!(value["failed"].get("kind").is_none());
    }

    #[test]
    fn exit_codes_match_the_documented_matrix() {
        assert_eq!(Failure::Resolve("x".into()).exit_code(), 2);
        assert_eq!(Failure::Load("x".into()).exit_code(), 3);
        assert_eq!(Failure::Save("x".into()).exit_code(), 3);
        assert_eq!(Failure::Render("x".into()).exit_code(), 3);
        assert_eq!(
            Failure::Step {
                id: "a".into(),
                error: CommandError::Unknown("a".into())
            }
            .exit_code(),
            4
        );
    }
}
