//! The six MCP tools (S5, spec `mcp-server.md` §5).
//!
//! Every tool is a thin adapter over code that already existed: the command
//! registry (S4), `save_project`, and `render_doc_to_wav`. Nothing here
//! decides how an edit works — only which knob the client turns.
//!
//! Failure split (spec §4.3): a request that does not match the tool's
//! `inputSchema` is `Err` and becomes a JSON-RPC `-32602`; an operation that
//! ran and failed is `Ok(ToolResult { is_error: true, .. })` so the model can
//! read the reason and try something else.

use glitcherz_model::{NodeDoc, Project, VoiceSpec};
use glitcherz_ui::export_audio;
use glitcherz_ui::headless::{RenderOptions, SpawnPolicy};
use serde_json::{Map, Value, json};

use crate::render_cmd::parse_bits;

use super::Server;

/// One tool call's answer, already serialised.
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

impl ToolResult {
    fn ok(value: Value) -> Self {
        Self {
            text: value.to_string(),
            is_error: false,
        }
    }

    /// A failed operation: `stage` uses the S4 vocabulary (`load` / `command`
    /// / `save` / `render`) plus `path` for sandbox refusals.
    fn failed(stage: &str, error: impl Into<String>) -> Self {
        Self::ok_failure(json!({"ok": false, "stage": stage, "error": error.into()}))
    }

    fn ok_failure(value: Value) -> Self {
        Self {
            text: value.to_string(),
            is_error: true,
        }
    }
}

/// The `tools/list` payload.
pub fn list() -> Value {
    json!({"tools": [
        {
            "name": "project_open",
            "description": "Load a .glz project into the session, replacing whatever was open. \
                Paths resolve inside the server's sandbox root. Returns a summary of the document. \
                A failed open keeps the previous document.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Project file, relative to the sandbox root."}
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "name": "project_save",
            "description": "Write the session document to a .glz file (atomic write, keeps a .bak). \
                Without `path` it saves back to the file that was opened.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Destination, relative to the sandbox root. Omit to overwrite the opened file."}
                },
                "required": [],
                "additionalProperties": false
            }
        },
        {
            "name": "project_info",
            "description": "Summarise the session document: track and clip counts, note count, \
                length, tempo, dirty flag, undo depth, and how many nodes use a plugin.",
            "inputSchema": {"type": "object", "properties": {}, "required": [], "additionalProperties": false}
        },
        {
            "name": "commands_list",
            "description": "Enumerate the edit commands this session accepts. Each entry has an id, \
                a human label, its parameter documentation, and whether it applies right now \
                (with the reason when it does not).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "only_enabled": {"type": "boolean", "description": "Drop commands that cannot run against the current document."}
                },
                "required": [],
                "additionalProperties": false
            }
        },
        {
            "name": "commands_run",
            "description": "Run edit commands in order. Each command is atomic (it either applies \
                completely or not at all). By default the batch stops at the first failure; the \
                report lists what applied and what was skipped. `dry_run` applies everything and \
                then rolls the document back, which is how to validate an edit before keeping it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "commands": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string", "description": "A command id from commands_list."},
                                "params": {"type": "object", "description": "Command parameters; omit for {}."}
                            },
                            "required": ["id"],
                            "additionalProperties": false
                        }
                    },
                    "stop_on_error": {"type": "boolean", "description": "Stop at the first failure (default true)."},
                    "dry_run": {"type": "boolean", "description": "Roll the document back after running (default false)."}
                },
                "required": ["commands"],
                "additionalProperties": false
            }
        },
        {
            "name": "render_wav",
            "description": "Render the session document (not the file on disk) to a stereo WAV. \
                Plugin nodes are rendered without loading the plugin binary unless the server was \
                started with --allow-plugins; the result's `warnings` names every node affected.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Output wav, relative to the sandbox root."},
                    "rate": {"type": "integer", "minimum": 1, "description": "Sample rate in Hz (default 44100)."},
                    "bits": {"type": "string", "enum": ["16", "24", "32f"], "description": "Bit depth (default 24)."},
                    "to_ticks": {"type": "integer", "minimum": 1, "description": "Render up to this tick; omit for the whole song."}
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }
    ]})
}

/// Runs one tool. `Err` means the request did not match the schema.
pub fn call(server: &mut Server, name: &str, args: &Value) -> Result<ToolResult, String> {
    match name {
        "project_open" => project_open(server, args),
        "project_save" => project_save(server, args),
        "project_info" => Ok(project_info(server)),
        "commands_list" => commands_list(server, args),
        "commands_run" => commands_run(server, args),
        "render_wav" => render_wav(server, args),
        other => Err(format!("unknown tool `{other}`")),
    }
}

// ---------------------------------------------------------------- project

fn project_open(server: &mut Server, args: &Value) -> Result<ToolResult, String> {
    let raw = required_string(args, "path")?;
    let path = match server.sandbox.resolve(&raw) {
        Ok(path) => path,
        Err(err) => return Ok(ToolResult::failed("path", err)),
    };
    match glitcherz_model::file::load_project(&path) {
        Ok(project) => {
            server.session.replace(project);
            server.session.mark_clean();
            server.source = Some(path);
            let mut value = summary(server);
            if let Value::Object(map) = &mut value {
                map.insert("ok".to_string(), json!(true));
                map.insert("opened".to_string(), json!(true));
            }
            Ok(ToolResult::ok(value))
        }
        // The session keeps whatever it had; a failed open must not wipe the
        // document the client is working on.
        Err(err) => Ok(ToolResult::failed(
            "load",
            format!("{}: {err}", path.display()),
        )),
    }
}

fn project_save(server: &mut Server, args: &Value) -> Result<ToolResult, String> {
    let requested = optional_string(args, "path")?;
    let target = match requested {
        Some(raw) => match server.sandbox.resolve(&raw) {
            Ok(path) => path,
            Err(err) => return Ok(ToolResult::failed("path", err)),
        },
        None => match server.source.clone() {
            Some(path) => path,
            None => {
                return Ok(ToolResult::failed(
                    "path",
                    "no project path; pass \"path\" or open a project first",
                ));
            }
        },
    };
    if let Err(err) = glitcherz_model::file::save_project(server.session.project(), &target) {
        return Ok(ToolResult::failed(
            "save",
            format!("{}: {err}", target.display()),
        ));
    }
    server.session.mark_clean();
    let bytes = std::fs::metadata(&target)
        .map(|meta| meta.len())
        .unwrap_or(0);
    server.source = Some(target.clone());
    Ok(ToolResult::ok(json!({
        "ok": true,
        "path": target.display().to_string(),
        "bytes": bytes,
    })))
}

fn project_info(server: &Server) -> ToolResult {
    let mut value = summary(server);
    if let Value::Object(map) = &mut value {
        map.insert("ok".to_string(), json!(true));
    }
    ToolResult::ok(value)
}

/// The one summary shape `project_open` and `project_info` both return.
fn summary(server: &Server) -> Value {
    let project = server.session.project();
    let length_ticks = export_audio::export_end_ticks(project);
    let clips = project
        .midi_tracks
        .iter()
        .map(|track| track.clips.len())
        .sum::<usize>()
        + project
            .audio_tracks
            .iter()
            .map(|track| track.clips.len())
            .sum::<usize>();
    let notes = project
        .midi_tracks
        .iter()
        .flat_map(|track| track.clips.iter())
        .map(|clip| clip.notes().len())
        .sum::<usize>();
    json!({
        "path": server.source.as_ref().map(|path| path.display().to_string()),
        "title": project.metadata.name,
        "tempo_bpm": project.metadata.tempo_bpm,
        "midi_tracks": project.midi_tracks.len(),
        "audio_tracks": project.audio_tracks.len(),
        "clips": clips,
        "notes": notes,
        "length_ticks": length_ticks,
        "length_seconds": f64::from(length_ticks) * project.sec_per_tick(),
        "dirty": server.session.is_dirty(),
        "undo_depth": server.session.undo_history().len(),
        "plugin_nodes": plugin_nodes(project).len(),
    })
}

/// Descriptions of every node whose voice is a CLAP / VST3 plugin.
fn plugin_nodes(project: &Project) -> Vec<String> {
    let mut found = Vec::new();
    for node in &project.graph.nodes {
        let (node_id, kind, voice) = match node {
            NodeDoc::Instrument { node_id, voice, .. } => (*node_id, "instrument", voice),
            NodeDoc::Effect { node_id, voice, .. } => (*node_id, "effect", voice),
            _ => continue,
        };
        let label = match voice {
            VoiceSpec::Clap { plugin_id, .. } => format!("CLAP `{plugin_id}`"),
            VoiceSpec::Vst3 { class_id, .. } => format!("VST3 `{class_id}`"),
            _ => continue,
        };
        found.push(format!("node {node_id} ({kind}) is {label}"));
    }
    found
}

// --------------------------------------------------------------- commands

fn commands_list(server: &mut Server, args: &Value) -> Result<ToolResult, String> {
    let only_enabled = optional_bool(args, "only_enabled")?.unwrap_or(false);
    let commands: Vec<Value> = glitcherz_command::command_specs()
        .iter()
        .map(|spec| spec.info(&server.session))
        .filter(|info| !only_enabled || info.enabled)
        .map(|info| {
            json!({
                "id": info.id,
                "label": info.label,
                "params": info.params,
                "menu": info.menu,
                "shortcut": info.shortcut,
                "enabled": info.enabled,
                "disabled_reason": info.disabled_reason,
            })
        })
        .collect();
    Ok(ToolResult::ok(json!({"ok": true, "commands": commands})))
}

fn commands_run(server: &mut Server, args: &Value) -> Result<ToolResult, String> {
    let steps = args
        .get("commands")
        .and_then(Value::as_array)
        .ok_or_else(|| "`commands` must be an array".to_string())?;
    if steps.is_empty() {
        return Err("`commands` must not be empty".to_string());
    }
    let stop_on_error = optional_bool(args, "stop_on_error")?.unwrap_or(true);
    let dry_run = optional_bool(args, "dry_run")?.unwrap_or(false);

    // Validate the whole list before touching the document: a typo in the
    // last step must not leave the first nine applied.
    let mut parsed: Vec<(String, Value)> = Vec::with_capacity(steps.len());
    for step in steps {
        let id = step
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "each command needs a string `id`".to_string())?
            .to_string();
        let params = step
            .get("params")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        if !params.is_object() {
            return Err(format!("`params` of `{id}` must be an object"));
        }
        parsed.push((id, params));
    }

    let before = server.session.project_arc();
    let mut results: Vec<Value> = Vec::with_capacity(parsed.len());
    let mut failure: Option<Map<String, Value>> = None;
    let mut skipped: Vec<Value> = Vec::new();
    for (index, (id, params)) in parsed.iter().enumerate() {
        match server.session.execute(id, params) {
            Ok(value) => results.push(json!({"id": id, "ok": true, "result": value})),
            Err(err) => {
                let kind = crate::batch::error_kind(&err);
                let message = err.to_string();
                results.push(json!({"id": id, "ok": false, "kind": kind, "error": message}));
                let mut map = Map::new();
                map.insert("stage".to_string(), json!("command"));
                map.insert("id".to_string(), json!(id));
                map.insert("kind".to_string(), json!(kind));
                map.insert("error".to_string(), json!(message));
                failure = Some(map);
                if stop_on_error {
                    skipped = parsed[index + 1..]
                        .iter()
                        .map(|(id, _)| json!(id))
                        .collect();
                    break;
                }
            }
        }
    }
    if dry_run {
        // `restore` puts the exact pre-call document back, so a dry run
        // leaves the session bit-for-bit unchanged even after a failure.
        server.session.restore(before);
    }

    let ok = failure.is_none();
    let mut report = json!({
        "ok": ok,
        "applied": !dry_run,
        "results": results,
        "skipped": skipped,
    });
    if let (Value::Object(map), Some(failure)) = (&mut report, failure) {
        for (key, value) in failure {
            map.insert(key, value);
        }
    }
    Ok(if ok {
        ToolResult::ok(report)
    } else {
        ToolResult::ok_failure(report)
    })
}

// ----------------------------------------------------------------- render

fn render_wav(server: &mut Server, args: &Value) -> Result<ToolResult, String> {
    let raw = required_string(args, "path")?;
    let path = match server.sandbox.resolve(&raw) {
        Ok(path) => path,
        Err(err) => return Ok(ToolResult::failed("path", err)),
    };
    let bits_raw = optional_string(args, "bits")?;
    let bits = match parse_bits(bits_raw.as_deref()) {
        Ok(bits) => bits,
        Err(err) => return Ok(ToolResult::failed("params", err)),
    };
    let options = RenderOptions {
        sample_rate: optional_u32(args, "rate")?.unwrap_or(44_100),
        bits,
        end_ticks: optional_u32(args, "to_ticks")?,
    };
    let policy = if server.allow_plugins {
        SpawnPolicy::Offline
    } else {
        SpawnPolicy::OfflineNoPlugins
    };
    // Collected before the render: the render itself only logs the refusal.
    let mut warnings: Vec<String> = Vec::new();
    if !server.allow_plugins {
        for node in plugin_nodes(server.session.project()) {
            warnings.push(format!(
                "{node}; its plugin was not loaded (start the server with --allow-plugins to use it)"
            ));
        }
    }
    let project = server.session.project();
    match glitcherz_ui::headless::render_doc_to_wav_with(project, &path, options, policy) {
        Ok(report) => Ok(ToolResult::ok(json!({
            "ok": true,
            "path": path.display().to_string(),
            "frames": report.frames,
            "sample_rate": report.sample_rate,
            "bits": bits_token(bits),
            "seconds": if report.sample_rate == 0 {
                0.0
            } else {
                f64::from(report.frames) / f64::from(report.sample_rate)
            },
            "warnings": warnings,
        }))),
        Err(err) => Ok(ToolResult::failed("render", err)),
    }
}

fn bits_token(bits: export_audio::Bits) -> &'static str {
    match bits {
        export_audio::Bits::Int16 => "16",
        export_audio::Bits::Int24 => "24",
        export_audio::Bits::Float32 => "32f",
    }
}

// ------------------------------------------------------------ arg helpers

fn required_string(args: &Value, key: &str) -> Result<String, String> {
    optional_string(args, key)?.ok_or_else(|| format!("`{key}` is required"))
}

fn optional_string(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn optional_bool(args: &Value, key: &str) -> Result<Option<bool>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("`{key}` must be a boolean")),
    }
}

fn optional_u32(args: &Value, key: &str) -> Result<Option<u32>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a non-negative integer")),
        Some(_) => Err(format!("`{key}` must be a non-negative integer")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use glitcherz_model::MidiClip;
    use glitcherz_model::Note;
    use serde_json::json;

    fn server() -> Server {
        let root = std::env::temp_dir().join(format!("glitcherz_s5_tools_{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create root");
        Server::new(
            super::super::sandbox::Sandbox::new(&root).expect("sandbox"),
            false,
        )
    }

    fn with_note(server: &mut Server) {
        let mut project = Project::default();
        let mut clip = MidiClip::new(0);
        clip.sequence.notes.push(Note {
            start: 0,
            length: 240,
            note: 60,
            velocity: 100,
            channel: 0,
        });
        project.midi_tracks[0].clips.push(clip);
        server.session.replace(project);
    }

    #[test]
    fn the_tool_list_has_the_six_tools() {
        let list = list();
        let tools = list["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect();
        assert_eq!(
            names,
            vec![
                "project_open",
                "project_save",
                "project_info",
                "commands_list",
                "commands_run",
                "render_wav",
            ]
        );
    }

    #[test]
    fn every_tool_schema_is_an_object() {
        let list = list();
        for tool in list["tools"].as_array().expect("tools array") {
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], json!("object"), "{tool}");
            assert!(schema["required"].is_array(), "{tool}");
            assert!(schema["properties"].is_object(), "{tool}");
        }
    }

    #[test]
    fn project_info_reports_the_document() {
        let mut server = server();
        with_note(&mut server);
        let result = project_info(&server);
        assert!(!result.is_error);
        let value: Value = serde_json::from_str(&result.text).expect("json");
        assert_eq!(value["notes"], json!(1));
        assert_eq!(value["clips"], json!(1));
        assert_eq!(value["plugin_nodes"], json!(0));
        assert!(value["length_ticks"].as_u64().unwrap_or(0) > 0);
    }

    #[test]
    fn commands_list_includes_the_midi_commands() {
        let mut server = server();
        with_note(&mut server);
        let result = commands_list(&mut server, &json!({})).expect("call");
        let value: Value = serde_json::from_str(&result.text).expect("json");
        let commands = value["commands"].as_array().expect("commands");
        assert_eq!(commands.len(), 16);
        assert!(
            commands
                .iter()
                .any(|entry| entry["id"] == json!("midi.transpose"))
        );
        assert!(matches!(commands[0]["enabled"], Value::Bool(_)));
    }

    #[test]
    fn commands_run_applies_in_order() {
        let mut server = server();
        with_note(&mut server);
        let result = commands_run(
            &mut server,
            &json!({"commands": [{"id": "midi.transpose", "params": {"semitones": 12}}]}),
        )
        .expect("call");
        assert!(!result.is_error, "{}", result.text);
        assert_eq!(
            server.session.project().midi_tracks[0].clips[0].notes()[0].note,
            72
        );
    }

    #[test]
    fn a_failing_batch_stops_and_reports_skipped() {
        let mut server = server();
        with_note(&mut server);
        let result = commands_run(
            &mut server,
            &json!({"commands": [
                {"id": "midi.transpose", "params": {"semitones": 1}},
                {"id": "midi.nope"},
                {"id": "midi.transpose", "params": {"semitones": 1}},
            ]}),
        )
        .expect("call");
        assert!(result.is_error);
        let value: Value = serde_json::from_str(&result.text).expect("json");
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["stage"], json!("command"));
        assert_eq!(value["kind"], json!("unknown"));
        assert_eq!(value["skipped"], json!(["midi.transpose"]));
        // The first command did apply: the batch is not all-or-nothing.
        assert_eq!(
            server.session.project().midi_tracks[0].clips[0].notes()[0].note,
            61
        );
    }

    #[test]
    fn dry_run_leaves_the_document_untouched() {
        let mut server = server();
        with_note(&mut server);
        let before = server.session.project_arc();
        let result = commands_run(
            &mut server,
            &json!({
                "commands": [{"id": "midi.transpose", "params": {"semitones": 12}}],
                "dry_run": true,
            }),
        )
        .expect("call");
        assert!(!result.is_error);
        let value: Value = serde_json::from_str(&result.text).expect("json");
        assert_eq!(value["applied"], json!(false));
        assert_eq!(
            server.session.project().midi_tracks[0].clips[0].notes()[0].note,
            60
        );
        assert_eq!(server.session.project_arc(), before);
    }

    #[test]
    fn a_schema_violation_is_an_error_not_a_tool_failure() {
        let mut server = server();
        assert!(call(&mut server, "project_open", &json!({})).is_err());
        assert!(call(&mut server, "render_wav", &json!({"path": 7})).is_err());
        assert!(call(&mut server, "nope", &json!({})).is_err());
    }

    #[test]
    fn a_path_outside_the_root_is_a_tool_failure() {
        let mut server = server();
        let result = call(&mut server, "project_open", &json!({"path": "../x.glz"})).expect("call");
        assert!(result.is_error);
        let value: Value = serde_json::from_str(&result.text).expect("json");
        assert_eq!(value["stage"], json!("path"));
        assert!(
            value["error"]
                .as_str()
                .unwrap_or_default()
                .contains("escapes the sandbox root")
        );
    }
}
