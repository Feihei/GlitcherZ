//! S3 integration tests for [`Sf2VoiceProcessor`] (spec
//! `.agents/spec/sf2-loader.md`).
//!
//! All tests run against the hand-built fixtures from `tests/common` in
//! plain `cargo test`; a real-soundfont smoke test is `#[ignore]`d behind
//! `GLITCHERZ_SF2_PATH` (D9: fixture files are local data). The mandatory
//! effect/voice test pairs (AGENTS.md) are `silent_without_notes` and
//! `extreme_events_stay_finite`.

use std::io::Cursor;
use std::sync::Arc;

use glitcherz_engine::realtime::buffer::AudioBuffer;
use glitcherz_engine::realtime::context::ProcessContext;
use glitcherz_engine::realtime::processor::Processor;
use glitcherz_engine::realtime::{NoteEvent, ParamId, SF2_POLYPHONY, Sf2VoiceProcessor};
use glitcherz_engine::{Sf2Error, Sf2Library};
use rustysynth_ext::SoundFont;

mod common;

const SR: f32 = 48_000.0;
const BLOCK: usize = 128;

fn ctx(frames: usize) -> ProcessContext {
    ProcessContext {
        sample_rate: SR,
        frames,
        playback_sample_pos: 0,
        song_ticks: 0.0,
        transport_dt_ticks: 0.0,
        transport_active: true,
        sec_per_tick: 60.0 / (120.0 * 960.0),
    }
}

fn make_processor(bytes: &[u8]) -> Sf2VoiceProcessor {
    let font = Arc::new(SoundFont::new(&mut Cursor::new(bytes)).expect("fixture must parse"));
    Sf2VoiceProcessor::new(&font, SR).expect("processor must build")
}

/// Renders `blocks` blocks into a stereo buffer, returning the buffer and
/// the peak of the final render (mirrors the graph's per-block flow:
/// set_len, clear, process).
fn render_blocks(proc: &mut Sf2VoiceProcessor, blocks: usize) -> (AudioBuffer, f32) {
    let mut buffer = AudioBuffer::new(2, BLOCK);
    for _ in 0..blocks {
        buffer.set_len(BLOCK);
        buffer.clear();
        proc.process(&ctx(BLOCK), &mut buffer);
    }
    let peak = peak_of(&buffer);
    (buffer, peak)
}

fn peak_of(buffer: &AudioBuffer) -> f32 {
    (0..buffer.channels())
        .filter_map(|ch| buffer.channel(ch))
        .flat_map(|c| c.iter())
        .fold(0.0f32, |m, s| m.max(s.abs()))
}

fn note_on(note: u8) -> NoteEvent {
    NoteEvent::On {
        note,
        velocity: 1.0,
    }
}

/// Blocks to render for a note to fully release (~0.125 s release plus
/// effect tail; matches the S1 audit numbers).
const RELEASE_BLOCKS: usize = 128;

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

#[test]
fn rejects_out_of_range_sample_rate() {
    let bytes = common::minimal_sf2_bytes();
    let font = Arc::new(SoundFont::new(&mut Cursor::new(&bytes[..])).unwrap());
    let err = Sf2VoiceProcessor::new(&font, 8_000.0).unwrap_err();
    assert!(
        matches!(err, Sf2Error::Synth(_)),
        "expected a synthesizer settings error, got {err:?}"
    );
}

#[test]
fn builds_with_full_polyphony_budget() {
    // The polyphony constant must stay inside the library's valid range
    // (8..=256), otherwise every construction would fail.
    assert!(
        (8..=256).contains(&SF2_POLYPHONY),
        "SF2_POLYPHONY must stay in the valid range"
    );
    let bytes = common::minimal_sf2_bytes();
    let font = Arc::new(SoundFont::new(&mut Cursor::new(&bytes[..])).unwrap());
    Sf2VoiceProcessor::new(&font, SR).expect("default construction must succeed");
}

// ---------------------------------------------------------------------------
// Mandatory voice tests (AGENTS.md)
// ---------------------------------------------------------------------------

// Standard test 1: output is silent without a note.
#[test]
fn silent_without_notes() {
    let mut proc = make_processor(&common::minimal_sf2_bytes());
    let (buffer, peak) = render_blocks(&mut proc, 10);
    assert!(peak < 1e-9, "expected digital silence, got peak {peak}");
    assert!(buffer.channel(0).unwrap().iter().all(|s| s.abs() < 1e-9));
}

// Standard test 2: extreme inputs stay finite (NaN velocity, over-range
// velocity, widest notes, out-of-MIDI-range note numbers).
#[test]
fn extreme_events_stay_finite() {
    let mut proc = make_processor(&common::minimal_sf2_bytes());
    assert!(proc.note_event(NoteEvent::On {
        note: 127,
        velocity: f32::NAN,
    }));
    assert!(proc.note_event(NoteEvent::On {
        note: 0,
        velocity: f32::INFINITY,
    }));
    // Out-of-MIDI-range key: kept out of the held bitmap (shift guard),
    // ignored by the synthesizer — must not panic either way.
    assert!(proc.note_event(NoteEvent::On {
        note: 200,
        velocity: 2.0,
    }));
    assert!(proc.note_event(NoteEvent::Off { note: 200 }));

    let (buffer, _) = render_blocks(&mut proc, 8);
    let all_finite = (0..buffer.channels())
        .filter_map(|ch| buffer.channel(ch))
        .flat_map(|c| c.iter())
        .all(|s| s.is_finite());
    assert!(all_finite, "output must stay finite under extreme events");
}

#[test]
fn odd_buffer_shapes_do_not_panic() {
    let mut proc = make_processor(&common::minimal_sf2_bytes());
    proc.note_event(note_on(60));

    // Zero-frame block.
    let mut buffer = AudioBuffer::new(2, BLOCK);
    buffer.set_len(0);
    proc.process(&ctx(0), &mut buffer);

    // Partial block (short host buffer tail).
    buffer.set_len(64);
    buffer.clear();
    proc.process(&ctx(64), &mut buffer);
    assert!(peak_of(&buffer) > 0.0, "partial block should carry audio");

    // Mono buffer (only channel 0 written, channel 1 skipped).
    let mut mono = AudioBuffer::new(1, BLOCK);
    mono.set_len(BLOCK);
    proc.process(&ctx(BLOCK), &mut mono);
    assert!(peak_of(&mono) > 0.0);
}

// ---------------------------------------------------------------------------
// Note lifecycle
// ---------------------------------------------------------------------------

#[test]
fn note_is_audible_on_both_channels_and_fades() {
    let mut proc = make_processor(&common::minimal_sf2_bytes());
    proc.note_event(note_on(60));
    let (buffer, sounding_peak) = render_blocks(&mut proc, 8);
    assert!(
        sounding_peak > 0.01,
        "note must be audible, peak {sounding_peak}"
    );
    for ch in 0..2 {
        let peak = buffer
            .channel(ch)
            .unwrap()
            .iter()
            .fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.01, "channel {ch} must carry signal");
    }

    proc.note_event(NoteEvent::Off { note: 60 });
    let (_, tail_peak) = render_blocks(&mut proc, RELEASE_BLOCKS);
    assert!(
        tail_peak < sounding_peak * 0.05,
        "note should fade after release (tail {tail_peak} vs {sounding_peak})"
    );
}

#[test]
fn velocity_zero_note_on_is_a_note_off() {
    let mut proc = make_processor(&common::minimal_sf2_bytes());
    proc.note_event(NoteEvent::On {
        note: 60,
        velocity: 0.0,
    });
    let (_, peak) = render_blocks(&mut proc, 8);
    assert!(peak < 1e-9, "velocity 0 note-on must not sound, got {peak}");
}

// ---------------------------------------------------------------------------
// Transport stop semantics (held-note bitmap)
// ---------------------------------------------------------------------------

#[test]
fn blanket_off_releases_held_notes() {
    let mut proc = make_processor(&common::minimal_sf2_bytes());
    for note in [60, 64, 67] {
        proc.note_event(note_on(note));
    }
    let (_, sounding_peak) = render_blocks(&mut proc, 8);
    assert!(sounding_peak > 0.01);

    // Transport sweep: `Off { note: 0 }` with no held key 0 releases
    // everything.
    proc.note_event(NoteEvent::Off { note: 0 });
    let (_, tail_peak) = render_blocks(&mut proc, RELEASE_BLOCKS);
    assert!(
        tail_peak < sounding_peak * 0.05,
        "blanket off must release held notes (tail {tail_peak} vs {sounding_peak})"
    );
}

#[test]
fn held_note_zero_release_stays_pitch_matched() {
    let mut proc = make_processor(&common::minimal_sf2_bytes());
    proc.note_event(note_on(0));
    proc.note_event(note_on(60));
    render_blocks(&mut proc, 4);

    // Key 0 IS held: this must be a pitch-matched release, leaving note 60
    // sounding.
    proc.note_event(NoteEvent::Off { note: 0 });
    let (_, still_peak) = render_blocks(&mut proc, 4);
    assert!(
        still_peak > 0.01,
        "note 60 must keep sounding after releasing key 0, peak {still_peak}"
    );

    // Key 0 no longer held: the same event now acts as the blanket sweep.
    proc.note_event(NoteEvent::Off { note: 0 });
    let (_, tail_peak) = render_blocks(&mut proc, RELEASE_BLOCKS);
    assert!(
        tail_peak < still_peak * 0.05,
        "second Off{{note: 0}} must sweep the remaining note (tail {tail_peak})"
    );
}

// ---------------------------------------------------------------------------
// Parameter surface
// ---------------------------------------------------------------------------

#[test]
fn param_routing() {
    let mut proc = make_processor(&common::minimal_sf2_bytes());
    assert!(proc.set_param(ParamId::Sf2BankSelect, 0.0));
    assert!(proc.set_param(ParamId::Sf2ProgramChange, 0.0));
    assert!(proc.set_param(ParamId::Sf2MasterVolume, 0.5));
    assert!(!proc.set_param(ParamId::SineFrequency, 440.0));
    assert!(!proc.set_param(ParamId::MasterGain, 1.0));
    assert!(!proc.set_param(ParamId::Attack, 0.1));
}

#[test]
fn master_volume_mutes_and_restores() {
    let mut proc = make_processor(&common::minimal_sf2_bytes());
    proc.note_event(note_on(60));
    let (_, sounding) = render_blocks(&mut proc, 4);
    assert!(sounding > 0.01);

    proc.set_param(ParamId::Sf2MasterVolume, 0.0);
    let (_, muted) = render_blocks(&mut proc, 4);
    assert!(muted < 1e-9, "volume 0 must mute, got {muted}");

    // Non-finite values are ignored — the note must still sound afterwards.
    proc.set_param(ParamId::Sf2MasterVolume, f32::NAN);
    proc.set_param(ParamId::Sf2MasterVolume, 0.5);
    let (_, restored) = render_blocks(&mut proc, 4);
    assert!(restored > 0.01, "volume 0.5 must restore the note");
}

#[test]
fn bank_and_program_address_presets() {
    let mut proc = make_processor(&common::two_preset_sf2_bytes());

    // Default preset is 0:0 (full key range): note 60 sounds.
    proc.note_event(note_on(60));
    let (_, open_peak) = render_blocks(&mut proc, 4);
    assert!(open_peak > 0.01, "preset 0:0 must play note 60");
    proc.note_event(NoteEvent::Off { note: 60 });
    render_blocks(&mut proc, RELEASE_BLOCKS);

    // Baseline: residual reverb tail of the released note (GM default
    // CC#91 = 40 keeps a small tail ringing past the release envelope).
    let (_, baseline) = render_blocks(&mut proc, 4);

    // Program 1 (keyRange 20..=40): note 60 adds no energy beyond the
    // baseline, note 30 is audible.
    proc.set_param(ParamId::Sf2BankSelect, 0.0);
    proc.set_param(ParamId::Sf2ProgramChange, 1.0);
    proc.note_event(note_on(60));
    let (_, silent_peak) = render_blocks(&mut proc, 4);
    assert!(
        silent_peak < baseline + 0.001,
        "preset 0:1 must reject note 60 (peak {silent_peak} vs baseline {baseline})"
    );
    proc.note_event(NoteEvent::Off { note: 60 });

    proc.note_event(note_on(30));
    let (_, low_peak) = render_blocks(&mut proc, 4);
    assert!(
        low_peak > 0.01,
        "preset 0:1 must play note 30 (in key range), peak {low_peak}"
    );
    proc.note_event(NoteEvent::Off { note: 30 });
    render_blocks(&mut proc, RELEASE_BLOCKS);

    // Back to program 0: note 60 sounds again.
    proc.set_param(ParamId::Sf2BankSelect, 0.0);
    proc.set_param(ParamId::Sf2ProgramChange, 0.0);
    proc.note_event(note_on(60));
    let (_, back_peak) = render_blocks(&mut proc, 4);
    assert!(back_peak > 0.01, "switching back to 0:0 must play note 60");
}

// ---------------------------------------------------------------------------
// Real-soundfont smoke test (opt-in: needs a local .sf2 file)
// ---------------------------------------------------------------------------

/// Exercises the full control-thread handoff: `Sf2Library::load` ->
/// `Arc<SoundFont>` -> `Sf2VoiceProcessor`, preset switching on a real
/// font, and audible finite output. Enable with `GLITCHERZ_SF2_PATH`, e.g.
/// `GLITCHERZ_SF2_PATH=C:\sf2\TimGM6mb.sf2 cargo test -p glitcherz-engine --test sf2_voice -- --ignored`
#[test]
#[ignore = "set GLITCHERZ_SF2_PATH to a real .sf2 file to run this smoke test"]
fn real_soundfont_smoke() {
    let Ok(path) = std::env::var("GLITCHERZ_SF2_PATH") else {
        eprintln!("GLITCHERZ_SF2_PATH not set; skipping real-soundfont smoke test");
        return;
    };
    let library =
        Sf2Library::load(std::path::Path::new(&path)).expect("sound font must load and parse");
    let mut proc = Sf2VoiceProcessor::new(library.font(), SR).expect("processor must build");

    let mut audible = 0usize;
    for preset in library.presets().iter().take(4) {
        proc.set_param(ParamId::Sf2BankSelect, preset.bank as f32);
        proc.set_param(ParamId::Sf2ProgramChange, preset.program as f32);
        proc.note_event(note_on(60));
        let (buffer, peak) = render_blocks(&mut proc, 4);
        let finite = (0..buffer.channels())
            .filter_map(|ch| buffer.channel(ch))
            .flat_map(|c| c.iter())
            .all(|s| s.is_finite());
        assert!(
            finite,
            "preset {}:{} produced non-finite output",
            preset.bank, preset.program
        );
        if peak > 0.001 {
            audible += 1;
        }
        proc.note_event(NoteEvent::Off { note: 60 });
        render_blocks(&mut proc, RELEASE_BLOCKS);
    }
    assert!(
        audible > 0,
        "at least one of the first presets must be audible"
    );
}
/// Diagnostic against a real font: walks every preset, prints its render
/// peak, and asserts the GM piano (bank 0 / program 0) is audible. Enable
/// with `GLITCHERZ_SF2_PATH`.
#[test]
#[ignore = "set GLITCHERZ_SF2_PATH to a real .sf2 file to run this diagnostic"]
fn real_soundfont_preset_audibility() {
    let Ok(path) = std::env::var("GLITCHERZ_SF2_PATH") else {
        eprintln!("GLITCHERZ_SF2_PATH not set; skipping real-soundfont audibility diagnostic");
        return;
    };
    let library =
        Sf2Library::load(std::path::Path::new(&path)).expect("sound font must load and parse");
    let mut proc = Sf2VoiceProcessor::new(library.font(), SR).expect("processor must build");

    let mut silent: Vec<String> = Vec::new();
    for preset in library.presets() {
        proc.set_param(ParamId::Sf2BankSelect, preset.bank as f32);
        proc.set_param(ParamId::Sf2ProgramChange, preset.program as f32);
        proc.note_event(note_on(60));
        let (_buffer, peak) = render_blocks(&mut proc, 4);
        eprintln!(
            "{:3}:{:<3} peak={:.4} {}",
            preset.bank, preset.program, peak, preset.name
        );
        if peak <= 0.001 {
            silent.push(format!(
                "{}:{} {}",
                preset.bank, preset.program, preset.name
            ));
        }
        proc.note_event(NoteEvent::Off { note: 60 });
        render_blocks(&mut proc, RELEASE_BLOCKS);
    }
    eprintln!(
        "silent presets ({} of {}):",
        silent.len(),
        library.presets().len()
    );
    for entry in &silent {
        eprintln!("  {entry}");
    }

    library
        .find_preset(0, 0)
        .expect("bank 0 / program 0 must exist");
    assert!(
        !silent.iter().any(|s| s.starts_with("0:0 ")),
        "the GM piano (0:0) must be audible, but it rendered silence"
    );
}
/// Full RT-graph replay of the UI's SoundFont load and preset-switch paths
/// against a real font: structure-queue node replacement (load), then
/// command-queue preset switching (the preset picker). Both paths must end
/// audible for the GM piano (0:0). Enable with `GLITCHERZ_SF2_PATH`.
#[test]
#[ignore = "set GLITCHERZ_SF2_PATH to a real .sf2 file to run this diagnostic"]
fn real_soundfont_full_graph_paths() {
    use glitcherz_engine::realtime::BlockRunner;
    use glitcherz_engine::realtime::commands::{EngineCommand, command_channel};
    use glitcherz_engine::realtime::graph::GraphCompiler;
    use glitcherz_engine::realtime::sequencer::Sequencer;
    use glitcherz_engine::realtime::voice::{recycle_channel, structure_channel};

    let Ok(path) = std::env::var("GLITCHERZ_SF2_PATH") else {
        eprintln!("GLITCHERZ_SF2_PATH not set; skipping real-soundfont full-graph diagnostic");
        return;
    };
    let library =
        Sf2Library::load(std::path::Path::new(&path)).expect("sound font must load and parse");

    let (cmd_tx, cmd_rx) = command_channel(64);
    let (mut structure_tx, structure_rx) = structure_channel();
    let (recycle_tx, _recycle_rx) = recycle_channel();
    let (audio_trash_tx, _audio_trash_rx) =
        glitcherz_engine::realtime::audio_track::audio_clip_trash_channel();
    let (mut compiler, graph) = GraphCompiler::default_graph(SR, BLOCK);
    let feedback = glitcherz_engine::realtime::Feedback::new();
    let runner = BlockRunner::new(
        BLOCK,
        2,
        SR,
        graph,
        Sequencer::new(SR),
        cmd_rx,
        structure_rx,
        recycle_tx,
        audio_trash_tx,
        feedback,
    );
    let mut runner = runner;
    let mut cmd_tx = cmd_tx;

    // Path 1 — UI load: preset addressed on the control thread, node swapped
    // in via the structure queue (mirrors `install_node`'s SoundFont branch).
    let voice_id = compiler
        .track_voice_node_id(0)
        .expect("track 0 voice node must exist");
    let mut processor = Sf2VoiceProcessor::new(library.font(), SR).expect("processor must build");
    processor.set_param(ParamId::Sf2BankSelect, 0.0);
    processor.set_param(ParamId::Sf2ProgramChange, 0.0);
    let (update, _kept) = compiler
        .replace_node(voice_id, Box::new(processor))
        .map_err(|_| ())
        .expect("node replacement must be accepted");
    structure_tx.push(update).expect("structure queue not full");

    let mut out = vec![0.0f32; 48_000 * 2];
    runner.render_interleaved(&mut out[..1024 * 2]); // let the swap land
    cmd_tx
        .push(EngineCommand::Note {
            track: 0,
            event: NoteEvent::On {
                note: 60,
                velocity: 0.9,
            },
        })
        .expect("queue not full");
    runner.render_interleaved(&mut out);
    let load_peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
    eprintln!("after load path (0:0), render peak = {load_peak:.4}");
    assert!(
        load_peak > 0.001,
        "the freshly loaded GM piano (0:0) must be audible through the graph"
    );

    // Path 2 — preset picker: command-queue switch to another preset, then
    // back to 0:0 (mirrors `set_soundfont_preset`).
    cmd_tx
        .push(EngineCommand::SetNodeParam {
            node_id: voice_id,
            id: ParamId::Sf2BankSelect,
            value: 0.0,
        })
        .expect("queue not full");
    cmd_tx
        .push(EngineCommand::SetNodeParam {
            node_id: voice_id,
            id: ParamId::Sf2ProgramChange,
            value: 1.0,
        })
        .expect("queue not full");
    out.fill(0.0);
    runner.render_interleaved(&mut out[..1024 * 2]);
    runner.render_interleaved(&mut out);
    let switch_peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
    eprintln!("after preset switch (0:1), render peak = {switch_peak:.4}");
    assert!(switch_peak > 0.001, "preset 0:1 must be audible");

    cmd_tx
        .push(EngineCommand::SetNodeParam {
            node_id: voice_id,
            id: ParamId::Sf2ProgramChange,
            value: 0.0,
        })
        .expect("queue not full");
    cmd_tx
        .push(EngineCommand::Note {
            track: 0,
            event: NoteEvent::On {
                note: 64,
                velocity: 0.9,
            },
        })
        .expect("queue not full");
    out.fill(0.0);
    runner.render_interleaved(&mut out);
    let back_peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
    eprintln!("after switching back (0:0), render peak = {back_peak:.4}");
    assert!(
        back_peak > 0.001,
        "switching back to 0:0 must be audible (note: previous note may still ring)"
    );
}
/// Generator index -> SF2 2.04 short name, for dump readability.
fn generator_name(index: usize) -> &'static str {
    match index {
        0 => "startAddrsOfs",
        1 => "endAddrsOfs",
        2 => "startLoopAddrsOfs",
        3 => "endLoopAddrsOfs",
        4 => "startAddrsCoarseOfs",
        5 => "modLfoToPitch",
        6 => "vibLfoToPitch",
        7 => "modEnvToPitch",
        8 => "initialFilterFc",
        9 => "initialFilterQ",
        10 => "modLfoToFilterFc",
        11 => "modEnvToFilterFc",
        12 => "endAddrsCoarseOfs",
        13 => "modLfoToVolume",
        15 => "chorusSend",
        16 => "reverbSend",
        17 => "pan",
        21 => "delayModLfo",
        22 => "freqModLfo",
        23 => "delayVibLfo",
        24 => "freqVibLfo",
        25 => "delayModEnv",
        26 => "attackModEnv",
        27 => "holdModEnv",
        28 => "decayModEnv",
        29 => "sustainModEnv",
        30 => "releaseModEnv",
        31 => "keynumToModEnvHold",
        32 => "keynumToModEnvDecay",
        33 => "delayVolEnv",
        34 => "attackVolEnv",
        35 => "holdVolEnv",
        36 => "decayVolEnv",
        37 => "sustainVolEnv",
        38 => "releaseVolEnv",
        39 => "keynumToVolEnvHold",
        40 => "keynumToVolEnvDecay",
        41 => "instrument",
        43 => "keyRange",
        44 => "velRange",
        45 => "startLoopAddrsCoarseOfs",
        46 => "keynum",
        47 => "velocity",
        48 => "initialAttenuation",
        50 => "endLoopAddrsCoarseOfs",
        51 => "coarseTune",
        52 => "fineTune",
        53 => "sampleID",
        54 => "sampleModes",
        56 => "scaleTuning",
        57 => "exclusiveClass",
        58 => "overridingRootKey",
        _ => "?",
    }
}

/// Velocity sweep against a real font: the piano roll delivers the document
/// default velocity 100 (~0.787) while the virtual keyboard sends ~0.9..1.0;
/// reports the render peak per velocity so a velrange gap in the font is
/// visible. Enable with `GLITCHERZ_SF2_PATH`.
#[test]
#[ignore = "set GLITCHERZ_SF2_PATH to a real .sf2 file to run this diagnostic"]
fn real_soundfont_velocity_sweep() {
    let Ok(path) = std::env::var("GLITCHERZ_SF2_PATH") else {
        eprintln!("GLITCHERZ_SF2_PATH not set; skipping real-soundfont velocity sweep");
        return;
    };
    let library =
        Sf2Library::load(std::path::Path::new(&path)).expect("sound font must load and parse");

    for (bank, program, label) in [
        (0u8, 0u8, "Grand Piano"),
        (0, 1, "Bright Grand"),
        (0, 3, "Honky-Tonk"),
    ] {
        for velocity in [1.0f32, 0.9, 100.0 / 127.0, 0.5, 0.25, 0.1] {
            let mut proc =
                Sf2VoiceProcessor::new(library.font(), SR).expect("processor must build");
            proc.set_param(ParamId::Sf2BankSelect, f32::from(bank));
            proc.set_param(ParamId::Sf2ProgramChange, f32::from(program));
            proc.note_event(NoteEvent::On { note: 60, velocity });
            // Full-envelope peak: every block's peak (not just the last),
            // so a slow piano attack is not mistaken for silence.
            let mut full_peak = 0.0f32;
            for _ in 0..64 {
                let (_buffer, peak) = render_blocks(&mut proc, 1);
                full_peak = full_peak.max(peak);
            }
            eprintln!("{label:<14} velocity {velocity:.4} -> full peak {full_peak:.4}");
        }
    }

    // Dump the piano's velocity-layer structure: every instrument region
    // covering key 60 with its vel range, so the dip at MIDI velocity 100
    // can be attributed to a font layer or a matching bug.
    let font = library.font();
    for preset in font.get_presets() {
        if preset.get_bank_number() == 0 && preset.get_patch_number() == 0 {
            eprintln!("--- preset '{}' regions ---", preset.get_name());
            for region in preset.get_regions() {
                let pvs = region.get_velocity_range_start();
                let pve = region.get_velocity_range_end();
                let pks = region.get_key_range_start();
                let pke = region.get_key_range_end();
                let explicit: Vec<String> = (0..61usize)
                    .filter_map(|i| {
                        region
                            .get_generator(i)
                            .map(|v| format!("{}={v}", generator_name(i)))
                    })
                    .collect();
                eprintln!(
                    "  preset zone key {pks}..{pke} vel {pvs}..{pve} explicit=[{}]",
                    explicit.join(", ")
                );
                let inst = &font.get_instruments()[region.get_instrument_id()];
                for inst_region in inst.get_regions() {
                    let ks = inst_region.get_key_range_start();
                    let ke = inst_region.get_key_range_end();
                    if ks <= 60 && 60 <= ke {
                        eprintln!(
                            "    -> inst '{}' vel {}..{} attn={:.1}dB cutoff={:.0}Hz Q={:.0} modEnvToFc={} attackModEnv={:.3}s sustainModEnv={:.1}dB decayModEnv={:.3}s attack={:.4}s sustain={:.1}dB decay={:.3}s release={:.3}s",
                            inst.get_name(),
                            inst_region.get_velocity_range_start(),
                            inst_region.get_velocity_range_end(),
                            inst_region.get_initial_attenuation(),
                            inst_region.get_initial_filter_cutoff_frequency(),
                            inst_region.get_initial_filter_q(),
                            inst_region.get_modulation_envelope_to_filter_cutoff_frequency(),
                            inst_region.get_attack_modulation_envelope(),
                            inst_region.get_sustain_modulation_envelope(),
                            inst_region.get_decay_modulation_envelope(),
                            inst_region.get_attack_volume_envelope(),
                            inst_region.get_sustain_volume_envelope(),
                            inst_region.get_decay_volume_envelope(),
                            inst_region.get_release_volume_envelope(),
                        );
                    }
                }
            }
        }
    }
}
