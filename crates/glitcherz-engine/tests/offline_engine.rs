//! U7 offline-engine tests (spec `.agents/spec/export-audio.md` X1):
//! [`AudioEngine::start_offline`] assembles the same queues / compiler /
//! runner / feedback as the live engine without touching any audio device,
//! and [`AudioEngine::render_offline_block`] drives blocks through the
//! normal path (command consumption at block boundaries, sequencer
//! scheduling, graph processing).
//!
//! These run on machines without audio hardware — the whole point of the
//! offline engine (CI included).

use glitcherz_engine::AudioEngine;
use glitcherz_engine::realtime::{EngineCommand, NoteEvent, SequencerCommand};

const SR: u32 = 48_000;
const BLOCK: usize = glitcherz_engine::realtime::FIXED_BLOCK_SIZE;

fn stereo_block() -> Vec<f32> {
    vec![0.0f32; BLOCK * 2]
}

/// Wires one default track (voice + delay) into the master output —
/// `add_midi_track` cables `instrument → delay` itself (P5f D18) and only
/// leaves the mixing to the caller.
fn wire_default_track(engine: &mut AudioEngine) {
    let [_, delay] = engine.add_midi_track().expect("default track slots");
    let master = engine
        .node_id_of_kind(glitcherz_engine::realtime::NodeKind::Master)
        .expect("empty graph still has the master node");
    assert!(engine.connect_nodes(delay, master, 0));
}

#[test]
fn offline_engine_renders_notes_and_stops_at_arrangement_end() {
    let mut engine = AudioEngine::start_offline(SR).expect("offline engine builds");
    assert_eq!(engine.info().sample_rate, SR);
    assert_eq!(engine.info().out_channels, 2);
    wire_default_track(&mut engine);
    // Structure updates ride the queue and land at the next block boundary —
    // one silent block first, or the note below would be consumed before the
    // track routing table exists.
    let mut out = stereo_block();
    engine.render_offline_block(&mut out);
    engine.send(EngineCommand::Sequencer(SequencerCommand::Seek(0.0)));
    engine.send(EngineCommand::Sequencer(SequencerCommand::Play));
    // Play's block sweep releases everything held (`note_off_all`) — it must
    // land BEFORE the note is queued, or the note is silenced within the
    // same block (a fresh envelope releases from level zero = instant
    // silence). The export path never queues manual notes at all (notes
    // come from the sequencer's clip scheduling, which runs after Play).
    engine.render_offline_block(&mut out);

    // A held manual note (no off): audible while playing.
    engine.send(EngineCommand::Note {
        track: 0,
        event: NoteEvent::On {
            note: 69,
            velocity: 1.0,
        },
    });

    let mut spoke = false;
    for _ in 0..750 {
        let mut out = stereo_block();
        assert!(
            engine.render_offline_block(&mut out),
            "offline engine must consume blocks"
        );
        assert!(engine.is_playing(), "transport must report playing");
        if out.iter().any(|s| s.abs() > 1e-4) {
            spoke = true;
        }
        if !engine.is_playing() {
            break;
        }
    }
    assert!(spoke, "the held note must be audible in the render");

    // Stop the transport: the held note releases (default ADSR release is
    // 0.2 s ≈ 80 blocks — render past it) and the output falls silent.
    engine.send(EngineCommand::Sequencer(SequencerCommand::Stop));
    let mut rang = false;
    for _ in 0..16 {
        let mut out = stereo_block();
        engine.render_offline_block(&mut out);
        if out.iter().any(|s| s.abs() > 1e-4) {
            rang = true;
        }
    }
    assert!(rang, "release blocks must still ring (0.2 s default)");
    // Drain the remaining release (~64 more blocks), then demand silence.
    for _ in 16..96 {
        let mut out = stereo_block();
        engine.render_offline_block(&mut out);
    }
    let mut quiet = true;
    for _ in 96..130 {
        let mut out = stereo_block();
        engine.render_offline_block(&mut out);
        if out.iter().any(|s| s.abs() > 1e-4) {
            quiet = false;
        }
    }
    assert!(quiet, "post-release output must fall silent");
    assert!(!engine.is_playing());
}

#[test]
fn offline_engine_reports_progress_ticks() {
    let mut engine = AudioEngine::start_offline(SR).expect("offline engine builds");
    engine.send(EngineCommand::Sequencer(SequencerCommand::Seek(0.0)));
    engine.send(EngineCommand::Sequencer(SequencerCommand::Play));
    let mut out = stereo_block();
    engine.render_offline_block(&mut out);
    let after_one = engine.musical_ticks();
    assert!(after_one > 0.0, "one block must advance the tick counter");
    engine.render_offline_block(&mut out);
    let after_two = engine.musical_ticks();
    assert!(after_two > after_one, "ticks keep advancing");
}

#[test]
fn live_engine_rejects_offline_blocks() {
    // `start()` requires a real output device; on machines without one it
    // errors before the runner ever moves — so the only portable way to
    // exercise the `None` branch is a live engine that did start. CI boxes
    // without audio get the start error here and skip the assertion.
    if let Ok(mut engine) = AudioEngine::start() {
        let mut out = stereo_block();
        assert!(
            !engine.render_offline_block(&mut out),
            "a live engine has no offline runner"
        );
    }
}
