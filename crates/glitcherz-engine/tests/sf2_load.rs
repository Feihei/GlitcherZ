//! S2 tests for the non-real-time SF2 loading module (`glitcherz_engine::sf2`,
//! spec `.agents/spec/sf2-loader.md` S2 / D2 / D3 / D7).
//!
//! Uses the shared hand-built minimal SF2 fixture (see `tests/common`), so
//! plain `cargo test` covers load / preset enumeration / error paths without
//! any external file. A `#[ignore]`d test replays the checks against a real
//! sound font via `GLITCHERZ_SF2_PATH` (local data, not committed).

mod common;

use std::io::Write;
use std::path::PathBuf;

use common::{minimal_sf2_bytes, minimal_sf2_bytes_at};
use glitcherz_engine::{Sf2Error, Sf2Library, Sf2Preset};

fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("glitcherz-sf2-test-{name}"));
    let mut file = std::fs::File::create(&path).expect("temp file must be creatable");
    file.write_all(bytes).expect("temp file must be writable");
    path
}

#[test]
fn load_enumerates_the_single_preset() {
    let path = temp_file("minimal.sf2", &minimal_sf2_bytes());
    let library = Sf2Library::load(&path).expect("minimal fixture must load");

    assert_eq!(library.path(), path.as_path());
    assert_eq!(
        library.presets(),
        &[Sf2Preset {
            bank: 0,
            program: 0,
            name: "SinePreset".to_owned(),
        }]
    );
}

#[test]
fn load_keeps_bank_and_program_addressing() {
    // bank 1 / program 40, as a drum-ish bank layout would use.
    let path = temp_file("banked.sf2", &minimal_sf2_bytes_at(1, 40));
    let library = Sf2Library::load(&path).expect("banked fixture must load");

    let preset = library.find_preset(1, 40).expect("preset must be found");
    assert_eq!(preset.name, "SinePreset");
    assert!(library.find_preset(0, 40).is_none());
    assert!(library.find_preset(1, 0).is_none());
}

#[test]
fn font_is_shared_for_the_rt_processor() {
    let path = temp_file("shared.sf2", &minimal_sf2_bytes());
    let library = Sf2Library::load(&path).expect("minimal fixture must load");

    // The S3 processor takes an Arc clone without re-parsing.
    let font = std::sync::Arc::clone(library.font());
    assert!(!font.get_presets().is_empty());
}

#[test]
fn missing_file_is_an_io_error() {
    let mut path = std::env::temp_dir();
    path.push("glitcherz-sf2-test-does-not-exist.sf2");
    let err = Sf2Library::load(&path).expect_err("missing file must fail");
    assert!(matches!(err, Sf2Error::Io(_)), "got: {err}");
}

#[test]
fn garbage_bytes_are_a_parse_error() {
    let path = temp_file("garbage.sf2", b"this is not a riff file at all".as_slice());
    let err = Sf2Library::load(&path).expect_err("garbage must fail");
    assert!(matches!(err, Sf2Error::Parse(_)), "got: {err}");
}

#[test]
fn truncated_file_is_a_parse_error() {
    let full = minimal_sf2_bytes();
    let path = temp_file("truncated.sf2", &full[..full.len() / 2]);
    let err = Sf2Library::load(&path).expect_err("truncated file must fail");
    assert!(matches!(err, Sf2Error::Parse(_)), "got: {err}");
}

#[test]
fn out_of_range_bank_is_skipped_and_reported_as_no_presets() {
    // bank 300 does not fit the u8:u8 track addressing (D7) — the only
    // preset is skipped, which must surface as NoPresets rather than an
    // empty library.
    let path = temp_file("widebank.sf2", &minimal_sf2_bytes_at(300, 0));
    let err = Sf2Library::load(&path).expect_err("out-of-range bank must fail");
    assert!(matches!(err, Sf2Error::NoPresets), "got: {err}");
}

/// Replays load + enumeration against a real sound font (recommended:
/// MuseScore_General.sf2 or TimGM6mb.sf2). Enable with the
/// `GLITCHERZ_SF2_PATH` environment variable, e.g.
/// `GLITCHERZ_SF2_PATH=C:\sf2\MuseScore_General.sf2 cargo test -p glitcherz-engine --test sf2_load -- --ignored`
/// (fixture files are local data and intentionally not committed).
#[test]
#[ignore = "set GLITCHERZ_SF2_PATH to a real .sf2 file to run this test"]
fn real_soundfont_loads_and_enumerates() {
    let Ok(path) = std::env::var("GLITCHERZ_SF2_PATH") else {
        eprintln!("GLITCHERZ_SF2_PATH not set; skipping real-soundfont load test");
        return;
    };

    let library = Sf2Library::load(std::path::Path::new(&path)).expect("real sound font must load");

    let presets = library.presets();
    assert!(presets.len() > 1, "real sound font must have presets");

    // A GM font always has bank 0 / program 0 (the piano).
    let piano = library
        .find_preset(0, 0)
        .expect("bank 0 / program 0 must exist");
    assert!(!piano.name.is_empty());

    // Preset addresses are unique: the UI dropdown and track addressing
    // rely on find_preset returning exactly one entry.
    let mut addresses: Vec<(u8, u8)> = presets.iter().map(|p| (p.bank, p.program)).collect();
    addresses.sort_unstable();
    addresses.dedup();
    assert_eq!(
        addresses.len(),
        presets.len(),
        "preset addresses must be unique"
    );
}
