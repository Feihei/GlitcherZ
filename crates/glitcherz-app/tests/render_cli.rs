//! End-to-end smoke test for the headless `--render` entry point (S0).
//! It must run without a window, an audio device or an eframe event loop.

use std::path::PathBuf;
use std::process::Command;

use glitcherz_model::{MidiClip, Note, Project};

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glitcherz_cli_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn render_subcommand_writes_a_wav_without_a_window() {
    let dir = scratch_dir();
    let project_path = dir.join("song.glz");
    let output = dir.join("song.wav");

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
    glitcherz_model::file::save_project(&project, &project_path).expect("save project");

    let result = Command::new(env!("CARGO_BIN_EXE_glitcherz"))
        .arg("--render")
        .arg(&project_path)
        .arg(&output)
        .args(["--rate", "22050", "--bits", "16", "--to", "1920"])
        .output()
        .expect("run glitcherz --render");
    assert!(
        result.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    let bytes = std::fs::read(&output).expect("read rendered wav");
    assert!(bytes.len() > 44, "wav must contain audio frames");
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");

    let _ = std::fs::remove_file(project_path);
    let _ = std::fs::remove_file(output);
    let _ = std::fs::remove_dir(&dir);
}
