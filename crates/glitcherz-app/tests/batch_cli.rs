//! End-to-end tests for the S4 batch command surface (spec `cli-commands.md`).
//!
//! Every test drives the real binary as a subprocess: the point of S4 is the
//! process boundary (args in, files and exit codes out), which unit tests
//! cannot cover.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use glitcherz_model::{MidiClip, Note, Project};
use serde_json::Value;

/// A per-test scratch directory (the test harness runs them in parallel).
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glitcherz_s4_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn glitcherz(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_glitcherz"))
        .args(args)
        .output()
        .expect("run glitcherz")
}

fn glitcherz_stdin(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_glitcherz"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn glitcherz");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for glitcherz")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json(output: &Output) -> Value {
    serde_json::from_str(&stdout(output)).unwrap_or_else(|err| {
        panic!(
            "stdout was not JSON ({err}):\nstdout:\n{}\nstderr:\n{}",
            stdout(output),
            stderr(output)
        )
    })
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

/// Writes a project with one clip holding `notes` on track 0.
fn write_project(path: &Path, notes: &[(u8, i64)]) {
    let mut project = Project::default();
    let mut clip = MidiClip::new(0);
    for (note, start) in notes {
        clip.sequence.notes.push(Note {
            start: *start,
            length: 240,
            note: *note,
            velocity: 100,
            channel: 0,
        });
    }
    project.midi_tracks[0].clips.push(clip);
    glitcherz_model::file::save_project(&project, path).expect("save fixture project");
}

fn note_count(path: &Path) -> usize {
    let project = glitcherz_model::file::load_project(path).expect("load project");
    project.midi_tracks[0]
        .clips
        .first()
        .map(|clip| clip.notes().len())
        .unwrap_or(0)
}

fn pitches(path: &Path) -> Vec<u8> {
    let project = glitcherz_model::file::load_project(path).expect("load project");
    project.midi_tracks[0]
        .clips
        .first()
        .map(|clip| clip.notes().iter().map(|n| n.note).collect())
        .unwrap_or_default()
}

// ---- `--commands` --------------------------------------------------------------------------

#[test]
fn commands_lists_the_registry() {
    let out = glitcherz(&["--commands", "--json"]);
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let registry = json(&out);
    let list = registry.as_array().expect("a JSON array");
    assert!(
        list.len() >= 16,
        "the registry should list every command, got {}",
        list.len()
    );
    let query = list
        .iter()
        .find(|entry| entry["id"] == "midi.query")
        .expect("midi.query is registered");
    assert_eq!(query["params"], "{track, clip}");
    assert_eq!(query["enabled"], true);
    assert!(query.get("label").is_some());
    // `--commands` is also readable without `--json`.
    let human = glitcherz(&["--commands"]);
    assert_eq!(exit_code(&human), 0);
    assert!(stdout(&human).contains("midi.transpose"));
}

#[test]
fn commands_gate_on_the_open_project() {
    let dir = scratch("gate");
    let project = dir.join("song.glz");
    write_project(&project, &[(60, 0)]);

    // A default project has no notes, so `midi.change_velocity` is gated off.
    let empty = json(&glitcherz(&["--commands", "--json"]));
    let gated = empty
        .as_array()
        .expect("array")
        .iter()
        .find(|entry| entry["id"] == "midi.change_velocity")
        .expect("registered");
    assert_eq!(gated["enabled"], false);
    assert_eq!(gated["disabled_reason"], "there are no notes to edit");

    // Pointing at a project with a note turns it on.
    let out = glitcherz(&[
        "--commands",
        "--json",
        "--open",
        project.to_str().expect("utf-8 path"),
    ]);
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let registry = json(&out);
    let enabled = registry
        .as_array()
        .expect("array")
        .iter()
        .find(|entry| entry["id"] == "midi.change_velocity")
        .expect("registered");
    assert_eq!(enabled["enabled"], true);
    assert!(
        enabled["disabled_reason"].is_null(),
        "an available command has no reason to give"
    );
}

// ---- reading ------------------------------------------------------------------------------

#[test]
fn query_returns_the_notes() {
    let dir = scratch("query");
    let project = dir.join("song.glz");
    write_project(&project, &[(60, 0), (64, 240)]);
    let path = project.to_str().expect("utf-8 path");

    let out = glitcherz(&[
        "--open",
        path,
        "--do",
        "midi.query",
        "--params",
        r#"{"track": 0, "clip": 0}"#,
        "--json",
    ]);
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let envelope = json(&out);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["project"], path);
    let result = &envelope["commands"][0]["result"]["notes"];
    assert_eq!(result.as_array().expect("notes").len(), 2);
    assert_eq!(result[0]["note"], 60);
    assert_eq!(result[1]["note"], 64);

    // The same query in human mode mentions the command id.
    let human = glitcherz(&[
        "--open",
        path,
        "--do",
        "midi.query",
        "--params",
        r#"{"track": 0, "clip": 0}"#,
    ]);
    assert_eq!(exit_code(&human), 0);
    assert!(stdout(&human).contains("ok  midi.query"));
}

// ---- ordering -----------------------------------------------------------------------------

#[test]
fn argv_order_is_execution_order() {
    let dir = scratch("order");
    let project = dir.join("song.glz");
    write_project(&project, &[(60, 0)]);
    let path = project.to_str().expect("utf-8 path");

    // One step comes from `--ops`, sandwiched between two `--do`s.
    let steps = dir.join("steps.json");
    std::fs::write(
        &steps,
        r#"[{"id": "midi.transpose", "params": {"semitones": 12}}]"#,
    )
    .expect("write ops");
    let steps = steps.to_str().expect("utf-8 path");
    let mixed = glitcherz(&[
        "--open",
        path,
        "--do",
        "midi.note_add",
        "--params",
        r#"{"track": 0, "clip": 0, "note": 72, "start_ticks": 960}"#,
        "--ops",
        steps,
        "--do",
        "midi.query",
        "--params",
        r#"{"track": 0, "clip": 0}"#,
        "--json",
    ]);
    assert_eq!(exit_code(&mixed), 0, "stderr: {}", stderr(&mixed));
    let mixed = json(&mixed);
    assert_eq!(
        mixed["commands"].as_array().expect("commands").len(),
        3,
        "note_add, transpose, query"
    );

    // The equivalent all-`--ops` batch must produce the same final query.
    let all = dir.join("all.json");
    std::fs::write(
        &all,
        r#"[
            {"id": "midi.note_add", "params": {"track": 0, "clip": 0, "note": 72, "start_ticks": 960}},
            {"id": "midi.transpose", "params": {"semitones": 12}},
            {"id": "midi.query", "params": {"track": 0, "clip": 0}}
        ]"#,
    )
    .expect("write ops");
    let flat = glitcherz(&[
        "--open",
        path,
        "--ops",
        all.to_str().expect("utf-8 path"),
        "--json",
    ]);
    assert_eq!(exit_code(&flat), 0, "stderr: {}", stderr(&flat));
    let flat = json(&flat);

    assert_eq!(
        mixed["commands"][2]["result"], flat["commands"][2]["result"],
        "argv order and file order must reach the same document"
    );
    // Sanity: the transposed notes really moved an octave (60 -> 72, 72 -> 84).
    let notes = flat["commands"][2]["result"]["notes"]
        .as_array()
        .expect("notes");
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0]["note"], 72);
    assert_eq!(notes[1]["note"], 84);
}

#[test]
fn ops_reads_stdin() {
    let dir = scratch("stdin");
    let project = dir.join("song.glz");
    write_project(&project, &[(60, 0)]);
    let out = glitcherz_stdin(
        &[
            "--open",
            project.to_str().expect("utf-8 path"),
            "--ops",
            "-",
            "--json",
        ],
        r#"[{"id": "midi.query", "params": {"track": 0, "clip": 0}}]"#,
    );
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let envelope = json(&out);
    assert_eq!(envelope["commands"][0]["result"]["notes"][0]["note"], 60);
}

// ---- argument errors (exit 2) -------------------------------------------------------------

#[test]
fn bad_ops_file_exits_two() {
    let dir = scratch("bad_ops");
    let missing = dir.join("nope.json");
    let out = glitcherz(&["--ops", missing.to_str().expect("utf-8 path")]);
    assert_eq!(exit_code(&out), 2, "stdout: {}", stdout(&out));
    assert!(
        stdout(&out).contains("nope.json"),
        "stdout: {}",
        stdout(&out)
    );

    let malformed = dir.join("broken.json");
    std::fs::write(&malformed, r#"{"id": "midi.query"}"#).expect("write");
    let out = glitcherz(&["--ops", malformed.to_str().expect("utf-8 path")]);
    assert_eq!(exit_code(&out), 2, "not an array is an input error");
    assert!(
        stdout(&out).contains("JSON array"),
        "stdout: {}",
        stdout(&out)
    );
}

#[test]
fn bad_params_json_exits_two() {
    let out = glitcherz(&["--do", "midi.query", "--params", "{oops"]);
    assert_eq!(exit_code(&out), 2);
    assert!(stderr(&out).contains("not valid JSON"));
}

#[test]
fn params_needs_a_do() {
    let out = glitcherz(&["--params", "{}"]);
    assert_eq!(exit_code(&out), 2);
    assert!(stderr(&out).contains("--do"));
}

#[test]
fn render_and_batch_are_exclusive() {
    let out = glitcherz(&["--render", "in.glz", "out.wav", "--do", "midi.query"]);
    assert_eq!(exit_code(&out), 2);
    assert!(
        stderr(&out).contains("--wav"),
        "the error should point at the fix: {}",
        stderr(&out)
    );
}

// ---- writing ------------------------------------------------------------------------------

#[test]
fn batch_edits_survive_a_save() {
    let dir = scratch("save");
    let project = dir.join("song.glz");
    let out_path = dir.join("out.glz");
    write_project(&project, &[(60, 0)]);

    let out = glitcherz(&[
        "--open",
        project.to_str().expect("utf-8 path"),
        "--do",
        "midi.note_add",
        "--params",
        r#"{"track": 0, "clip": 0, "note": 67, "start_ticks": 480}"#,
        "--do",
        "midi.transpose",
        "--params",
        r#"{"semitones": 12}"#,
        "--save",
        out_path.to_str().expect("utf-8 path"),
        "--json",
    ]);
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let envelope = json(&out);
    assert_eq!(envelope["saved"], out_path.to_str().expect("utf-8 path"));

    assert_eq!(note_count(&out_path), 2, "the added note was saved");
    assert_eq!(
        pitches(&out_path),
        vec![72, 79],
        "both notes were transposed an octave"
    );
}

#[test]
fn saving_without_open_writes_a_default_project() {
    let dir = scratch("new");
    let out_path = dir.join("fresh.glz");
    let out = glitcherz(&["--save", out_path.to_str().expect("utf-8 path")]);
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    // It round-trips through `load_project`, so the file is a legal project.
    assert_eq!(note_count(&out_path), 0);
    let project = glitcherz_model::file::load_project(&out_path).expect("load saved default");
    assert!(!project.midi_tracks.is_empty());
}

#[test]
fn a_new_project_has_no_clip_to_edit() {
    // Known limitation (spec §9): every `midi.*` command addresses an existing
    // clip, and a brand-new project's tracks carry none — so a batch that
    // composes from nothing needs a template project. Locked here so the
    // behaviour cannot change silently.
    let dir = scratch("noclip");
    let out_path = dir.join("fresh.glz");
    let out = glitcherz(&[
        "--do",
        "midi.note_add",
        "--params",
        r#"{"track": 0, "clip": 0, "note": 60, "start_ticks": 0}"#,
        "--save",
        out_path.to_str().expect("utf-8 path"),
    ]);
    assert_eq!(exit_code(&out), 4, "stdout: {}", stdout(&out));
    assert!(stdout(&out).contains("no clip"), "stdout: {}", stdout(&out));
    assert!(!out_path.exists(), "a failed batch writes nothing");
}

#[test]
fn a_failed_batch_writes_nothing() {
    let dir = scratch("atomic");
    let out_path = dir.join("out.glz");
    let out = glitcherz(&[
        // No `--open`: a default project has no clip, so the transpose fails.
        "--do",
        "midi.transpose",
        "--params",
        r#"{"semitones": 12}"#,
        "--save",
        out_path.to_str().expect("utf-8 path"),
    ]);
    assert_eq!(exit_code(&out), 4, "stdout: {}", stdout(&out));
    assert!(!out_path.exists(), "a failed batch must not publish a file");
}

#[test]
fn unknown_command_exits_four() {
    let dir = scratch("unknown");
    let out_path = dir.join("out.glz");
    let out = glitcherz(&[
        "--do",
        "midi.nope",
        "--save",
        out_path.to_str().expect("utf-8 path"),
        "--json",
    ]);
    assert_eq!(exit_code(&out), 4);
    let envelope = json(&out);
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["failed"]["stage"], "command");
    assert_eq!(envelope["failed"]["id"], "midi.nope");
    assert_eq!(envelope["failed"]["kind"], "unknown");
    assert!(!out_path.exists());
}

#[test]
fn load_failure_exits_three() {
    let dir = scratch("newer");
    let project = dir.join("song.glz");
    write_project(&project, &[(60, 0)]);
    let text = std::fs::read_to_string(&project).expect("read");
    let bumped = text.replacen(
        &format!("version: {}", glitcherz_model::PROJECT_VERSION),
        "version: 99",
        1,
    );
    assert_ne!(bumped, text, "the fixture should carry a version field");
    std::fs::write(&project, bumped).expect("write");

    let out = glitcherz(&[
        "--open",
        project.to_str().expect("utf-8 path"),
        "--do",
        "midi.query",
    ]);
    assert_eq!(exit_code(&out), 3, "stdout: {}", stdout(&out));
    assert!(stdout(&out).contains("v99"), "stdout: {}", stdout(&out));
}

#[test]
fn save_keeps_the_atomic_semantics() {
    let dir = scratch("bak");
    let project = dir.join("song.glz");
    let out_path = dir.join("out.glz");
    write_project(&project, &[(60, 0), (64, 240)]);
    let input = project.to_str().expect("utf-8 path");

    for _ in 0..2 {
        let out = glitcherz(&[
            "--open",
            input,
            "--save",
            out_path.to_str().expect("utf-8 path"),
        ]);
        assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    }
    // `save_project` keeps the replaced revision beside the file.
    assert!(
        out_path.with_extension("glz.bak").exists(),
        "second save keeps a .bak"
    );
    assert!(
        !out_path.with_extension("glz.tmp").exists(),
        "the temp file is renamed away"
    );
    assert_eq!(note_count(&out_path), 2);
}

// ---- rendering ----------------------------------------------------------------------------

#[test]
fn wav_renders_the_edited_document() {
    let dir = scratch("wav");
    let project = dir.join("song.glz");
    write_project(&project, &[(60, 0), (64, 240)]);
    let input = project.to_str().expect("utf-8 path");
    let edited = dir.join("edited.wav");
    let roundtrip = dir.join("roundtrip.wav");
    let plain = dir.join("plain.wav");
    let saved = dir.join("saved.glz");

    // Edit, save and render in one process.
    let out = glitcherz(&[
        "--open",
        input,
        "--do",
        "midi.transpose",
        "--params",
        r#"{"semitones": 12}"#,
        "--save",
        saved.to_str().expect("utf-8 path"),
        "--wav",
        edited.to_str().expect("utf-8 path"),
        "--rate",
        "22050",
        "--bits",
        "16",
        "--to",
        "1920",
        "--json",
    ]);
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(json(&out)["render"]["sample_rate"], 22050);

    // Rendering the saved file must match: same document, same assembly.
    let out = glitcherz(&[
        "--render",
        saved.to_str().expect("utf-8 path"),
        roundtrip.to_str().expect("utf-8 path"),
        "--rate",
        "22050",
        "--bits",
        "16",
        "--to",
        "1920",
    ]);
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let edited_bytes = std::fs::read(&edited).expect("read edited wav");
    let roundtrip_bytes = std::fs::read(&roundtrip).expect("read roundtrip wav");
    assert_eq!(
        edited_bytes, roundtrip_bytes,
        "--wav must render the edited document, not the file on disk"
    );

    // And the edit is audible: no batch means a different wav.
    let out = glitcherz(&[
        "--open",
        input,
        "--wav",
        plain.to_str().expect("utf-8 path"),
        "--rate",
        "22050",
        "--bits",
        "16",
        "--to",
        "1920",
    ]);
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let plain_bytes = std::fs::read(&plain).expect("read plain wav");
    assert_ne!(
        edited_bytes, plain_bytes,
        "the transposed render must differ from the unedited one"
    );
}

// ---- envelope shape -----------------------------------------------------------------------

#[test]
fn json_envelope_shape() {
    let dir = scratch("envelope");
    let project = dir.join("song.glz");
    write_project(&project, &[(60, 0)]);
    let out = glitcherz(&[
        "--open",
        project.to_str().expect("utf-8 path"),
        "--do",
        "midi.query",
        "--params",
        r#"{"track": 0, "clip": 0}"#,
        "--json",
    ]);
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let envelope = json(&out);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["commands"][0]["id"], "midi.query");
    assert_eq!(envelope["commands"][0]["ok"], true);
    assert!(
        envelope.get("saved").is_none(),
        "keys are absent, not null, when the flag was not given"
    );
    assert!(envelope.get("render").is_none());
    assert!(envelope.get("failed").is_none());
    // Nothing but JSON on stdout.
    assert!(stdout(&out).trim_start().starts_with('{'));
    assert!(stdout(&out).trim_end().ends_with('}'));
}
