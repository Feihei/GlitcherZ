//! End-to-end tests for the S5 MCP server (spec `mcp-server.md` §8).
//!
//! Every test drives the real binary over a real pipe: the point of S5 is the
//! process boundary (JSON in, JSON out, files on disk), which unit tests
//! cannot cover. The harness reads exactly one line per request, so a test
//! fails loudly if the server ever answers a notification or writes anything
//! that is not a protocol frame.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use glitcherz_model::{MidiClip, NodeDoc, Note, Project, VoiceSpec};
use serde_json::{Value, json};

/// A per-test scratch directory (the harness runs tests in parallel).
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glitcherz_s5_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// A live MCP server process.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    /// Every response line seen, for the "stdout is pure JSON" check.
    seen: Vec<String>,
}

impl Mcp {
    fn start(root: &Path, extra: &[&str]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_glitcherz"));
        command
            .arg("--mcp")
            .arg("--root")
            .arg(root)
            .args(extra)
            .env("RUST_LOG", "off")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn glitcherz --mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut mcp = Self {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 0,
            seen: Vec::new(),
        };
        let response = mcp.request("initialize", json!({"protocolVersion": "2025-06-18"}));
        assert!(response.get("result").is_some(), "{response}");
        mcp.write(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        mcp
    }

    fn write(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        writeln!(stdin, "{line}").expect("write request");
        stdin.flush().expect("flush");
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read response");
        assert!(read > 0, "the server closed stdout");
        self.seen.push(line.clone());
        serde_json::from_str(&line)
            .unwrap_or_else(|err| panic!("stdout was not JSON ({err}): {line}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write(&frame.to_string());
        let response = self.read_line();
        assert_eq!(
            response["id"],
            json!(id),
            "response id mismatch: {response}"
        );
        response
    }

    /// Calls a tool and returns `(is_error, parsed report)`.
    fn call(&mut self, tool: &str, arguments: Value) -> (bool, Value) {
        let response = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        assert!(
            response.get("error").is_none(),
            "{tool} answered a protocol error: {response}"
        );
        let is_error = response["result"]["isError"]
            .as_bool()
            .unwrap_or_else(|| panic!("no isError in {response}"));
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text content in {response}"));
        let report: Value = serde_json::from_str(text)
            .unwrap_or_else(|err| panic!("{tool} text was not JSON ({err}): {text}"));
        assert_eq!(
            is_error,
            !report["ok"].as_bool().unwrap_or(true),
            "{tool}: isError and ok disagree: {report}"
        );
        (is_error, report)
    }

    /// Closes stdin and returns the exit code.
    fn finish(mut self) -> i32 {
        drop(self.stdin.take());
        let status = self.child.wait().expect("wait for glitcherz");
        status.code().unwrap_or(-1)
    }
}

/// Writes a project with one clip of `notes` on track 0.
fn write_project(path: &Path, notes: &[u8]) {
    let mut project = Project::default();
    let data = project.push_midi_data(glitcherz_model::MidiData {
        length_ticks: 3840,
        ..glitcherz_model::MidiData::default()
    });
    for (index, note) in notes.iter().enumerate() {
        project.midi_data_mut(data).unwrap().notes_mut().push(Note {
            start: index as i64 * 240,
            length: 240,
            note: *note,
            velocity: 100,
            channel: 0,
        });
    }
    project.midi_tracks[0].clips.push(MidiClip {
        start_ticks: 0,
        span_ticks: 3840,
        data,
        ..MidiClip::default()
    });
    glitcherz_model::file::save_project(&project, path).expect("save fixture project");
}

fn note_pitches(path: &Path) -> Vec<u8> {
    let project = glitcherz_model::file::load_project(path).expect("load project");
    project
        .midi_clip_notes(0, 0)
        .expect("clip notes")
        .iter()
        .map(|note| note.note)
        .collect()
}

fn glitcherz(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_glitcherz"))
        .args(args)
        .output()
        .expect("run glitcherz")
}

// ------------------------------------------------------------- handshake

#[test]
fn initialize_echoes_the_version_and_declares_tools() {
    let dir = scratch("init");
    let mut mcp = Mcp::start(&dir, &[]);
    // `start` already handshook; ask again to inspect the payload.
    let response = mcp.request("initialize", json!({"protocolVersion": "2025-03-26"}));
    let result = &response["result"];
    assert_eq!(result["protocolVersion"], json!("2025-03-26"));
    assert_eq!(result["serverInfo"]["name"], json!("glitcherz"));
    assert!(result["serverInfo"]["version"].is_string());
    assert!(result["capabilities"]["tools"].is_object());
    assert_eq!(mcp.finish(), 0);
}

#[test]
fn an_unknown_protocol_version_falls_back_to_the_latest() {
    let dir = scratch("version");
    let mut mcp = Mcp::start(&dir, &[]);
    let response = mcp.request("initialize", json!({"protocolVersion": "1999-01-01"}));
    assert_eq!(response["result"]["protocolVersion"], json!("2025-06-18"));
    assert_eq!(mcp.finish(), 0);
}

#[test]
fn the_tool_list_is_the_six_tools_with_object_schemas() {
    let dir = scratch("tools");
    let mut mcp = Mcp::start(&dir, &[]);
    let response = mcp.request("tools/list", json!({}));
    let tools = response["result"]["tools"].as_array().expect("tools");
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
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], json!("object"), "{tool}");
        assert!(tool["inputSchema"]["required"].is_array(), "{tool}");
        assert!(
            tool["inputSchema"]["properties"]
                .as_object()
                .is_some_and(
                    |properties| !properties.is_empty() || tool["name"] == json!("project_info")
                ),
            "{tool}"
        );
        assert!(
            tool["description"].as_str().is_some_and(|d| d.len() > 20),
            "{tool}"
        );
    }
    assert_eq!(mcp.finish(), 0);
}

#[test]
fn notifications_get_no_reply() {
    let dir = scratch("notify");
    let mut mcp = Mcp::start(&dir, &[]);
    // A notification followed by a request: if the notification answered, the
    // next line would not carry this id.
    mcp.write(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    mcp.write(r#"{"jsonrpc":"2.0","method":"notifications/whatever"}"#);
    let response = mcp.request("ping", json!({}));
    assert_eq!(response["result"], json!({}));
    assert_eq!(mcp.finish(), 0);
}

#[test]
fn protocol_misuse_uses_standard_error_codes() {
    let dir = scratch("errors");
    let mut mcp = Mcp::start(&dir, &[]);

    let response = mcp.request("resources/list", json!({}));
    assert_eq!(response["error"]["code"], json!(-32601));

    let response = mcp.request("tools/call", json!({"name": "nope"}));
    assert_eq!(response["error"]["code"], json!(-32602));

    // Broken JSON must not end the session.
    mcp.write("{oops");
    let response = mcp.read_line();
    assert_eq!(response["error"]["code"], json!(-32700));
    assert_eq!(response["id"], Value::Null);

    let response = mcp.request("ping", json!({}));
    assert_eq!(response["result"], json!({}));
    assert_eq!(mcp.finish(), 0);
}

#[test]
fn stdout_carries_protocol_frames_only() {
    let dir = scratch("pure");
    write_project(&dir.join("song.glz"), &[60]);
    let mut mcp = Mcp::start(&dir, &[]);
    let (_error, _report) = mcp.call("project_open", json!({"path": "song.glz"}));
    let (_error, _report) = mcp.call("project_info", json!({}));
    let (_error, _report) = mcp.call("render_wav", json!({"path": "out.wav", "to_ticks": 480}));
    let frames = mcp.seen.clone();
    assert_eq!(mcp.finish(), 0);
    // `read_line` already asserts each line parses; this pins the count so a
    // stray `println!` shows up as an extra frame. The first line is the
    // handshake `Mcp::start` performs.
    assert_eq!(frames.len(), 4, "{frames:?}");
}

// --------------------------------------------------------------- project

#[test]
fn project_open_reports_a_summary() {
    let dir = scratch("open");
    write_project(&dir.join("song.glz"), &[60, 64, 67, 72]);
    let mut mcp = Mcp::start(&dir, &[]);
    let (error, report) = mcp.call("project_open", json!({"path": "song.glz"}));
    assert!(!error, "{report}");
    assert_eq!(report["ok"], json!(true));
    assert_eq!(report["opened"], json!(true));
    assert_eq!(report["notes"], json!(4));
    assert_eq!(report["clips"], json!(1));
    assert!(report["midi_tracks"].as_u64().unwrap_or(0) >= 1);
    assert_eq!(report["plugin_nodes"], json!(0));
    assert!(report["length_ticks"].as_u64().unwrap_or(0) > 0);
    assert!(
        report["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("song.glz"))
    );
    assert_eq!(mcp.finish(), 0);
}

#[test]
fn opening_outside_the_root_is_refused() {
    let dir = scratch("escape");
    let mut mcp = Mcp::start(&dir, &[]);
    let (error, report) = mcp.call("project_open", json!({"path": "../escape.glz"}));
    assert!(error);
    assert_eq!(report["stage"], json!("path"));
    assert!(
        report["error"]
            .as_str()
            .unwrap_or_default()
            .contains("escapes the sandbox root"),
        "{report}"
    );
    assert_eq!(mcp.finish(), 0);
}

#[test]
fn a_failed_open_keeps_the_current_document() {
    let dir = scratch("keep");
    write_project(&dir.join("song.glz"), &[60, 62]);
    let mut mcp = Mcp::start(&dir, &[]);
    let (error, _report) = mcp.call("project_open", json!({"path": "song.glz"}));
    assert!(!error);
    let (error, report) = mcp.call("project_open", json!({"path": "missing.glz"}));
    assert!(error);
    assert_eq!(report["stage"], json!("load"));
    let (error, report) = mcp.call("project_info", json!({}));
    assert!(!error);
    assert_eq!(
        report["notes"],
        json!(2),
        "the document must survive: {report}"
    );
    assert_eq!(mcp.finish(), 0);
}

#[test]
fn saving_without_a_path_and_without_an_open_project_fails() {
    let dir = scratch("nosave");
    let mut mcp = Mcp::start(&dir, &[]);
    let (error, report) = mcp.call("project_save", json!({}));
    assert!(error);
    assert_eq!(report["stage"], json!("path"));
    assert_eq!(mcp.finish(), 0);
}

#[test]
fn edits_survive_a_save() {
    let dir = scratch("save");
    write_project(&dir.join("song.glz"), &[60]);
    let mut mcp = Mcp::start(&dir, &[]);
    let (error, _report) = mcp.call("project_open", json!({"path": "song.glz"}));
    assert!(!error);
    let (error, report) = mcp.call(
        "commands_run",
        json!({"commands": [{"id": "midi.transpose", "params": {"semitones": 12}}]}),
    );
    assert!(!error, "{report}");
    let (error, report) = mcp.call("project_save", json!({"path": "edited.glz"}));
    assert!(!error, "{report}");
    assert!(report["bytes"].as_u64().unwrap_or(0) > 0);
    assert_eq!(note_pitches(&dir.join("edited.glz")), vec![72]);
    assert_eq!(note_pitches(&dir.join("song.glz")), vec![60]);
    assert_eq!(mcp.finish(), 0);
}

#[test]
fn a_pathless_save_overwrites_the_opened_file() {
    let dir = scratch("resave");
    write_project(&dir.join("song.glz"), &[60]);
    let mut mcp = Mcp::start(&dir, &[]);
    let (error, _report) = mcp.call("project_open", json!({"path": "song.glz"}));
    assert!(!error);
    let (error, _report) = mcp.call(
        "commands_run",
        json!({"commands": [{"id": "midi.transpose", "params": {"semitones": 5}}]}),
    );
    assert!(!error);
    let (error, report) = mcp.call("project_save", json!({}));
    assert!(!error, "{report}");
    assert_eq!(note_pitches(&dir.join("song.glz")), vec![65]);
    assert_eq!(mcp.finish(), 0);
}

// -------------------------------------------------------------- commands

#[test]
fn commands_list_enumerates_the_registry() {
    let dir = scratch("list");
    // Deliberately nothing open: `Project::default()` owns clips but no notes,
    // so the selection-based commands report why they cannot run (S4 pinned
    // the same fact in `a_new_project_has_no_clip_to_edit`).
    let mut mcp = Mcp::start(&dir, &[]);
    let (error, report) = mcp.call("commands_list", json!({}));
    assert!(!error, "{report}");
    let commands = report["commands"].as_array().expect("commands");
    assert_eq!(commands.len(), 25);
    assert!(
        commands
            .iter()
            .any(|entry| entry["id"] == json!("midi.transpose"))
    );
    assert!(commands.iter().all(|entry| entry["enabled"].is_boolean()));
    let change = commands
        .iter()
        .find(|entry| entry["id"] == json!("midi.change_velocity"))
        .expect("midi.change_velocity");
    assert_eq!(change["enabled"], json!(false));
    assert!(change["disabled_reason"].is_string());

    // `only_enabled` drops the unavailable ones.
    let (error, report) = mcp.call("commands_list", json!({"only_enabled": true}));
    assert!(!error, "{report}");
    let enabled = report["commands"].as_array().expect("commands");
    assert!(enabled.len() < commands.len());
    assert!(enabled.iter().all(|entry| entry["enabled"] == json!(true)));
    assert_eq!(mcp.finish(), 0);
}

#[test]
fn a_batch_stops_at_the_first_failure_and_reports_the_rest_as_skipped() {
    let dir = scratch("stop");
    write_project(&dir.join("song.glz"), &[60]);
    let mut mcp = Mcp::start(&dir, &[]);
    let (error, _report) = mcp.call("project_open", json!({"path": "song.glz"}));
    assert!(!error);
    let (error, report) = mcp.call(
        "commands_run",
        json!({"commands": [
            {"id": "midi.transpose", "params": {"semitones": 1}},
            {"id": "midi.does_not_exist"},
            {"id": "midi.transpose", "params": {"semitones": 1}},
        ]}),
    );
    assert!(error);
    assert_eq!(report["ok"], json!(false));
    assert_eq!(report["stage"], json!("command"));
    assert_eq!(report["kind"], json!("unknown"));
    assert_eq!(report["skipped"], json!(["midi.transpose"]));
    // The first command did apply — the session is live, not transactional
    // across the whole batch.
    let (error, report) = mcp.call("project_info", json!({}));
    assert!(!error);
    assert_eq!(report["notes"], json!(1));
    assert_eq!(report["dirty"], json!(true));
    assert_eq!(mcp.finish(), 0);
}

#[test]
fn dry_run_leaves_the_document_untouched() {
    let dir = scratch("dry");
    write_project(&dir.join("song.glz"), &[60]);
    let mut mcp = Mcp::start(&dir, &[]);
    let (error, _report) = mcp.call("project_open", json!({"path": "song.glz"}));
    assert!(!error);
    let (error, report) = mcp.call(
        "commands_run",
        json!({"commands": [{"id": "midi.transpose", "params": {"semitones": 12}}], "dry_run": true}),
    );
    assert!(!error, "{report}");
    assert_eq!(report["applied"], json!(false));
    // Save after the dry run: the file must still hold the original pitch.
    let (error, _report) = mcp.call("project_save", json!({"path": "after.glz"}));
    assert!(!error);
    assert_eq!(note_pitches(&dir.join("after.glz")), vec![60]);
    assert_eq!(mcp.finish(), 0);
}

// ---------------------------------------------------------------- scratch build

/// F-039 V3: an agent can go from an empty clip to an audible render without
/// touching the UI — discover the graph, write a parameter, add a clip, write
/// a note, then render.
#[test]
fn scratch_build_adds_a_clip_notes_and_renders() {
    let dir = scratch("scratch-build");
    let mut mcp = Mcp::start(&dir, &[]);

    // Graph vocabulary: find the default instrument, write one parameter.
    let (error, report) = mcp.call(
        "commands_run",
        json!({"commands": [{"id": "graph.query", "params": {}}]}),
    );
    assert!(!error, "{report}");
    let nodes = report["results"][0]["result"]["nodes"]
        .as_array()
        .expect("nodes");
    let node = nodes
        .iter()
        .find(|entry| entry["kind"] == json!("instrument"))
        .and_then(|entry| entry["node"].as_u64())
        .expect("the default template carries an instrument");
    let (error, report) = mcp.call(
        "commands_run",
        json!({"commands": [
            {"id": "graph.set_param", "params": {"node": node, "param": "attack", "value": 0.1}}
        ]}),
    );
    assert!(!error, "{report}");

    // Add a clip; the report carries its index for the following commands.
    let (error, report) = mcp.call(
        "commands_run",
        json!({"commands": [
            {"id": "clip.add", "params": {"track": 0, "name": "Scratch"}}
        ]}),
    );
    assert!(!error, "{report}");
    let clip = report["results"][0]["result"]["clip"]
        .as_u64()
        .expect("clip index");

    // Render the empty clip, then the same range with one loud note.
    let (error, report) = mcp.call("render_wav", json!({"path": "empty.wav", "to_ticks": 3840}));
    assert!(!error, "{report}");
    let empty = std::fs::read(dir.join("empty.wav")).expect("empty render");

    let (error, report) = mcp.call(
        "commands_run",
        json!({"commands": [
            {"id": "midi.note_add", "params": {
                "track": 0, "clip": clip, "note": 69,
                "start_ticks": 0, "length_ticks": 960, "velocity": 120
            }},
            {"id": "midi.transpose", "params": {"track": 0, "clip": clip, "semitones": 0}}
        ]}),
    );
    assert!(!error, "{report}");
    let (error, report) = mcp.call("render_wav", json!({"path": "note.wav", "to_ticks": 3840}));
    assert!(!error, "{report}");
    let with_note = std::fs::read(dir.join("note.wav")).expect("note render");

    assert!(with_note.len() > 44, "the render has samples");
    assert_ne!(empty, with_note, "the note must change the rendered audio");
    assert_eq!(mcp.finish(), 0);
}

// ---------------------------------------------------------------- render

#[test]
fn render_matches_the_cli_byte_for_byte() {
    let dir = scratch("render");
    let project = dir.join("song.glz");
    write_project(&project, &[60, 64, 67, 72]);
    let cli_out = dir.join("cli.wav");
    let output = glitcherz(&[
        "--render",
        &project.display().to_string(),
        &cli_out.display().to_string(),
        "--rate",
        "22050",
        "--bits",
        "16",
        "--to",
        "1920",
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    let mut mcp = Mcp::start(&dir, &[]);
    let (error, _report) = mcp.call("project_open", json!({"path": "song.glz"}));
    assert!(!error);
    let (error, report) = mcp.call(
        "render_wav",
        json!({"path": "mcp.wav", "rate": 22050, "bits": "16", "to_ticks": 1920}),
    );
    assert!(!error, "{report}");
    assert_eq!(report["sample_rate"], json!(22050));
    assert_eq!(report["bits"], json!("16"));
    assert!(report["frames"].as_u64().unwrap_or(0) > 0);
    assert_eq!(report["warnings"], json!([]));
    assert_eq!(mcp.finish(), 0);

    let cli = std::fs::read(&cli_out).expect("read cli wav");
    let via_mcp = std::fs::read(dir.join("mcp.wav")).expect("read mcp wav");
    assert!(!cli.is_empty());
    assert_eq!(
        cli.len(),
        via_mcp.len(),
        "the two renders must be the same size"
    );
    assert!(cli == via_mcp, "the two renders must be identical");
}

#[test]
fn a_render_outside_the_root_is_refused() {
    let dir = scratch("renderexit");
    write_project(&dir.join("song.glz"), &[60]);
    let mut mcp = Mcp::start(&dir, &[]);
    let (error, _report) = mcp.call("project_open", json!({"path": "song.glz"}));
    assert!(!error);
    let (error, report) = mcp.call("render_wav", json!({"path": "../escape.wav"}));
    assert!(error);
    assert_eq!(report["stage"], json!("path"));
    assert!(!dir.parent().expect("parent").join("escape.wav").exists());
    assert_eq!(mcp.finish(), 0);
}

#[test]
fn plugins_are_disabled_by_default_and_reported() {
    let dir = scratch("plugins");
    let mut project = Project::default();
    let data = project.push_midi_data(glitcherz_model::MidiData {
        length_ticks: 3840,
        ..glitcherz_model::MidiData::default()
    });
    project.midi_data_mut(data).unwrap().notes_mut().push(Note {
        start: 0,
        length: 240,
        note: 60,
        velocity: 100,
        channel: 0,
    });
    project.midi_tracks[0].clips.push(MidiClip {
        start_ticks: 0,
        span_ticks: 3840,
        data,
        ..MidiClip::default()
    });
    // Replace track 0's instrument with a CLAP voice pointing at a file that
    // does not exist: if the server tried to load it, the render would log an
    // error and fall back, but the warning list is what proves the policy.
    let mut patched = false;
    for node in &mut project.graph.nodes {
        if let NodeDoc::Instrument { voice, .. } = node {
            *voice = VoiceSpec::Clap {
                path: dir.join("missing.clap").display().to_string(),
                plugin_id: "com.example.test".to_string(),
                state: Vec::new(),
            };
            patched = true;
            break;
        }
    }
    assert!(patched, "the default project must own a track-0 instrument");
    glitcherz_model::file::save_project(&project, &dir.join("plugin.glz"))
        .expect("save fixture project");

    let mut mcp = Mcp::start(&dir, &[]);
    let (error, report) = mcp.call("project_open", json!({"path": "plugin.glz"}));
    assert!(!error, "{report}");
    assert_eq!(report["plugin_nodes"], json!(1));

    let (error, report) = mcp.call("render_wav", json!({"path": "plugin.wav", "to_ticks": 960}));
    assert!(!error, "{report}");
    assert!(report["frames"].as_u64().unwrap_or(0) > 0);
    let warnings = report["warnings"].as_array().expect("warnings");
    assert_eq!(warnings.len(), 1, "{report}");
    assert!(
        warnings[0]
            .as_str()
            .unwrap_or_default()
            .contains("com.example.test"),
        "{report}"
    );
    assert_eq!(mcp.finish(), 0);
}

// ------------------------------------------------------------------ cli

#[test]
fn mcp_is_exclusive_with_the_batch_flags() {
    let dir = scratch("exclusive");
    let root = dir.display().to_string();
    let output = glitcherz(&["--mcp", "--root", &root, "--ops", "list.json"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let output = glitcherz(&["--mcp", "--root", &root, "--render", "a.glz", "b.wav"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn mcp_flags_without_mcp_are_rejected() {
    let dir = scratch("flags");
    let root = dir.display().to_string();
    let output = glitcherz(&["--root", &root]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let output = glitcherz(&["--allow-plugins"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn a_missing_root_exits_two() {
    let dir = scratch("badroot");
    let missing = dir.join("nope").display().to_string();
    let output = glitcherz(&["--mcp", "--root", &missing]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn a_failed_preload_exits_three() {
    let dir = scratch("badopen");
    let root = dir.display().to_string();
    let output = glitcherz(&["--mcp", "--root", &root, "--open", "missing.glz"]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
}

#[test]
fn closing_stdin_exits_zero() {
    let dir = scratch("eof");
    let mcp = Mcp::start(&dir, &[]);
    assert_eq!(mcp.finish(), 0);
}
