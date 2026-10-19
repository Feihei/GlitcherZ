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
