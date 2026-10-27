//! SP2 integration tests for [`SampleVoiceProcessor`] (spec
//! `.agents/spec/sample-player.md`).
//!
//! Fixtures are self-generated wavs (hound `WavWriter` writing a few
//! milliseconds of a linear ramp), so everything runs in plain
//! `cargo test` with no external files. The mandatory voice test pairs
//! (AGENTS.md) are `silent_without_notes` and `extreme_events_stay_finite`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use glitcherz_engine::realtime::buffer::AudioBuffer;
use glitcherz_engine::realtime::context::ProcessContext;
use glitcherz_engine::realtime::processor::Processor;
use glitcherz_engine::realtime::{NoteEvent, ParamId, SAMPLE_POLYPHONY, SampleVoiceProcessor};
use glitcherz_engine::sample::SampleData;

const SR: f32 = 48_000.0;
const BLOCK: usize = 128;
const ROOT: u8 = 60;

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

/// Writes a mono 16-bit ramp wav (`src[i] = slope · i`) and returns its
/// path; a ramp makes "output = source read at rate r" assertions exact at
/// integer cursor positions (interpolation frac = 0).
fn ramp_fixture(name: &str, frames: usize, sample_rate: u32, slope: f32) -> PathBuf {
    let path = std::env::temp_dir().join(format!("glitcherz-voice-{name}.wav"));
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("fixture create");
    for i in 0..frames {
        let v = (slope * i as f32).clamp(-1.0, 1.0);
        writer
            .write_sample((v * i16::MAX as f32) as i16)
            .expect("fixture write");
    }
    writer.finalize().expect("fixture finalize");
    path
}

fn make_processor(fixture: &Path) -> SampleVoiceProcessor {
    let data = Arc::new(SampleData::load(fixture).expect("fixture must decode"));
    SampleVoiceProcessor::new(data, SR)
}

/// Writes a mono 16-bit **constant-amplitude** wav. Sustained-voice tests
/// use this instead of a ramp: a flat waveform keeps the per-block peak
/// stable, so gain / fade / stacking assertions compare blocks directly.
fn flat_fixture(name: &str, frames: usize, sample_rate: u32, value: f32) -> PathBuf {
    let path = std::env::temp_dir().join(format!("glitcherz-voice-{name}.wav"));
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("fixture create");
    for _ in 0..frames {
        writer
            .write_sample((value.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .expect("fixture write");
    }
    writer.finalize().expect("fixture finalize");
    path
}

fn render_blocks(proc: &mut SampleVoiceProcessor, blocks: usize) -> (AudioBuffer, f32) {
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

/// Left channel of the first rendered block after a fresh note-on.
fn first_block_left(proc: &mut SampleVoiceProcessor) -> Vec<f32> {
    let (buffer, _) = render_blocks(proc, 1);
    buffer.channel(0).unwrap().to_vec()
}

// ---------------------------------------------------------------------------
// Mandatory voice tests (AGENTS.md)
// ---------------------------------------------------------------------------

#[test]
fn silent_without_notes() {
    let fixture = ramp_fixture("silent", 1024, 48_000, 1.0 / 1024.0);
    let mut proc = make_processor(&fixture);
    let (buffer, peak) = render_blocks(&mut proc, 10);
    assert!(peak < 1e-9, "expected digital silence, got peak {peak}");
    assert!(buffer.channel(0).unwrap().iter().all(|s| s.abs() < 1e-9));
}

#[test]
fn extreme_events_stay_finite() {
    let fixture = ramp_fixture("extreme", 1024, 48_000, 1.0);
    let mut proc = make_processor(&fixture);
    proc.note_event(NoteEvent::On {
        note: 127,
        velocity: f32::NAN,
    });
    proc.note_event(NoteEvent::On {
        note: 0,
        velocity: f32::INFINITY,
    });
    // Out-of-MIDI-range key: kept out of the held bitmap (shift guard),
    // rendered as a normal voice — must not panic.
    proc.note_event(NoteEvent::On {
        note: 200,
        velocity: 2.0,
    });
    proc.note_event(NoteEvent::Off { note: 200 });
    proc.set_param(ParamId::SampleGain, f32::NAN);
    proc.set_param(ParamId::SampleGain, f32::INFINITY);
    proc.set_param(ParamId::SampleRootNote, f32::NAN);
    proc.set_param(ParamId::SamplePlayMode, f32::NAN);
    let (buffer, peak) = render_blocks(&mut proc, 8);
    assert!(
        peak.is_finite() && peak <= SAMPLE_POLYPHONY as f32,
        "expected finite bounded output, got {peak}"
    );
    assert!(
        buffer.channel(1).unwrap().iter().all(|s| s.is_finite()),
        "right channel must stay finite"
    );
}

// ---------------------------------------------------------------------------
// Playback math (D3)
// ---------------------------------------------------------------------------

#[test]
fn rate_identity_at_root_note() {
    // wav rate == engine rate and note == root → the output reproduces the
    // source frames one to one.
    let n = 512;
    let fixture = ramp_fixture("identity", n, 48_000, 1.0 / n as f32);
    let mut proc = make_processor(&fixture);
    proc.note_event(note_on(ROOT));
    let left = first_block_left(&mut proc);
    for (f, &sample) in left.iter().enumerate().take(120) {
        let expected = f as f32 / n as f32;
        assert!(
            (sample - expected).abs() < 2e-3,
            "frame {f}: expected {expected}, got {sample}"
        );
    }
}

#[test]
fn pitch_shift_doubles_playback_rate() {
    // +12 semitones above root → the cursor advances at exactly 2×.
    let n = 1024;
    let fixture = ramp_fixture("octave", n, 48_000, 1.0 / n as f32);
    let mut proc = make_processor(&fixture);
    proc.note_event(note_on(ROOT + 12));
    let left = first_block_left(&mut proc);
    for (f, &sample) in left.iter().enumerate().take(120) {
        let expected = (2 * f) as f32 / n as f32;
        assert!(
            (sample - expected).abs() < 2e-3,
            "frame {f}: expected {expected}, got {sample}"
        );
    }
}

#[test]
fn sample_rate_mismatch_is_compensated() {
    // 44.1 kHz source in a 48 kHz engine: the cursor advances at
    // 44100/48000 = 0.91875 frames per engine frame. Frame 160 hits the
    // integer cursor 147 (and 320 → 294), so the assertions stay
    // interpolation-exact.
    let n = 2048;
    let fixture = ramp_fixture("srcomp", n, 44_100, 1.0 / n as f32);
    let mut proc = make_processor(&fixture);
    proc.note_event(note_on(ROOT));
    let mut left = Vec::new();
    for _ in 0..3 {
        let (buffer, _) = render_blocks(&mut proc, 1);
        left.extend_from_slice(buffer.channel(0).unwrap());
    }
    for f in [0usize, 160, 320] {
        let expected = (f as f64 * 44_100.0 / 48_000.0).round() as usize;
        let expected = expected as f32 / n as f32;
        assert!(
            (left[f] - expected).abs() < 2e-3,
            "frame {f}: expected {expected}, got {}",
            left[f]
        );
    }
}

#[test]
fn root_note_param_retargets_subsequent_notes() {
    // Moving the root note down an octave makes the same key play at 2×.
    let n = 1024;
    let fixture = ramp_fixture("retarget", n, 48_000, 1.0 / n as f32);
    let mut proc = make_processor(&fixture);
    proc.set_param(ParamId::SampleRootNote, (ROOT - 12) as f32);
    proc.note_event(note_on(ROOT));
    let left = first_block_left(&mut proc);
    let expected = 2.0f32 / n as f32;
    assert!((left[1] - expected).abs() < 2e-3, "got {}", left[1]);
}

// ---------------------------------------------------------------------------
// Play modes and release (D1 / D2 / D5)
// ---------------------------------------------------------------------------

/// Renders `blocks` after a note-off and returns the peak of each block.
fn block_peaks_after_off(proc: &mut SampleVoiceProcessor, note: u8, blocks: usize) -> Vec<f32> {
    proc.note_event(NoteEvent::Off { note });
    let mut peaks = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        let (_, peak) = render_blocks(proc, 1);
        peaks.push(peak);
    }
    peaks
}

#[test]
fn gate_release_fades_then_silences() {
    let fixture = flat_fixture("gate", 8192, 48_000, 1.0);
    let mut proc = make_processor(&fixture);
    proc.note_event(note_on(ROOT));
    let (_, peak) = render_blocks(&mut proc, 1);
    let live = peak;
    assert!(live > 0.9, "sample must sound at full level, got {live}");

    // 10 ms at 48 kHz = 480 frames = 3.75 blocks: the first post-off block
    // still sounds (fade just started), silence arrives within 4 blocks
    // (512 ≥ 480 frames) plus the swap-compact block.
    let peaks = block_peaks_after_off(&mut proc, ROOT, 6);
    assert!(
        peaks[0] > 0.5 * live,
        "first release block must still sound, got {} (live {live})",
        peaks[0]
    );
    assert!(
        peaks.iter().skip(4).all(|p| *p < 1e-6),
        "release must finish within ~10 ms + compaction, got {peaks:?}"
    );
}

#[test]
fn gate_release_is_not_a_hard_cut() {
    // The very first post-off frame must be at (nearly) full level — a
    // linear fade starting at 1.0, not an instant drop to zero.
    let n = 8192;
    let fixture = ramp_fixture("soft", n, 48_000, 1.0 / n as f32);
    let mut proc = make_processor(&fixture);
    proc.note_event(note_on(ROOT));
    let (_, live) = render_blocks(&mut proc, 1);
    proc.note_event(NoteEvent::Off { note: ROOT });
    let left = first_block_left(&mut proc);
    assert!(
        left[0] > 0.9 * live,
        "fade frame 0 must be near the live level, got {} (live {live})",
        left[0]
    );
}

#[test]
fn one_shot_ignores_pitch_matched_off() {
    let n = 8192;
    let fixture = ramp_fixture("oneshot", n, 48_000, 1.0 / n as f32);
    let mut proc = make_processor(&fixture);
    proc.set_param(ParamId::SamplePlayMode, 1.0);
    proc.note_event(note_on(ROOT));
    let (_, live) = render_blocks(&mut proc, 1);
    // Pitch-matched off must not stop a OneShot voice.
    proc.note_event(NoteEvent::Off { note: ROOT });
    let (_, after_off) = render_blocks(&mut proc, 1);
    assert!(
        after_off > 0.9 * live,
        "OneShot must keep playing through a matched note-off, got {after_off} (live {live})"
    );
}

#[test]
fn one_shot_stops_on_blanket_sweep() {
    let fixture = flat_fixture("blanket", 8192, 48_000, 1.0);
    let mut proc = make_processor(&fixture);
    proc.set_param(ParamId::SamplePlayMode, 1.0);
    proc.note_event(note_on(ROOT));
    let (_, live) = render_blocks(&mut proc, 1);
    assert!(live > 0.9);
    // Blanket sweep (no held key 0): must stop OneShot too (D5).
    let peaks = block_peaks_after_off(&mut proc, 0, 6);
    assert!(
        peaks[0] > 0.5 * live,
        "fade must be audible first, got {} (live {live})",
        peaks[0]
    );
    assert!(
        peaks.iter().skip(4).all(|p| *p < 1e-6),
        "blanket stop must silence OneShot, got {peaks:?}"
    );
}

#[test]
fn velocity_zero_note_on_is_a_note_off() {
    let n = 8192;
    let fixture = ramp_fixture("veloff", n, 48_000, 1.0 / n as f32);
    let mut proc = make_processor(&fixture);
    proc.note_event(note_on(ROOT));
    render_blocks(&mut proc, 1);
    proc.note_event(NoteEvent::On {
        note: ROOT,
        velocity: 0.0,
    });
    let peaks = block_peaks_after_off(&mut proc, 255, 6);
    assert!(
        peaks.iter().skip(4).all(|p| *p < 1e-6),
        "velocity-0 note-on must release the voice, got {peaks:?}"
    );
}

// ---------------------------------------------------------------------------
// Voice pool (D4)
// ---------------------------------------------------------------------------

#[test]
fn pool_cap_is_sixteen_voices() {
    // Sixteen triggers of the same key stack 16 identical voices; the 17th
    // steals the oldest instead of growing past the budget. With a shared
    // flat waveform the peak is exactly (voice count × single-voice peak).
    let fixture = flat_fixture("pool", 4096, 48_000, 1.0);
    let mut proc = make_processor(&fixture);

    for _ in 0..16 {
        proc.note_event(note_on(ROOT));
    }
    let (_, peak16) = render_blocks(&mut proc, 1);
    let single = peak16 / 16.0;

    proc.note_event(note_on(ROOT));
    let (_, peak17) = render_blocks(&mut proc, 1);
    assert!(
        (peak17 - peak16).abs() < 4e-3,
        "17th trigger must steal (stay at 16 voices), got {peak17} vs {peak16}"
    );
    assert!(
        peak17.is_finite() && peak17 > 15.0 * single,
        "voice stack must still sound, got {peak17}"
    );
}

#[test]
fn natural_end_recycles_the_voice() {
    // Sample shorter than two blocks: after playing out, later blocks are
    // silent and the pool is free again (a new note sounds normally).
    let n = 64;
    let fixture = ramp_fixture("short", n, 48_000, 1.0 / n as f32);
    let mut proc = make_processor(&fixture);
    proc.note_event(note_on(ROOT));
    let (_, _) = render_blocks(&mut proc, 4);
    let (buffer, peak) = render_blocks(&mut proc, 1);
    assert!(
        peak < 1e-9,
        "sample must have played out, got {peak} ({} frames)",
        buffer.len()
    );
    proc.note_event(note_on(ROOT));
    let (_, peak) = render_blocks(&mut proc, 1);
    assert!(peak > 0.9, "voice slot must be recycled, got {peak}");
}

#[test]
fn gain_param_applies_to_sounding_voices() {
    let fixture = flat_fixture("gain", 2048, 48_000, 1.0);
    let mut proc = make_processor(&fixture);
    proc.note_event(note_on(ROOT));
    let (_, full) = render_blocks(&mut proc, 1);
    proc.set_param(ParamId::SampleGain, 0.5);
    let (_, half) = render_blocks(&mut proc, 1);
    assert!(
        (half - 0.5 * full).abs() < 4e-3,
        "gain must apply immediately, got {half} (full {full})"
    );
}

#[test]
fn velocity_scales_the_voice() {
    let n = 2048;
    let fixture = ramp_fixture("velocity", n, 48_000, 1.0 / n as f32);
    let mut proc = make_processor(&fixture);
    proc.note_event(NoteEvent::On {
        note: ROOT,
        velocity: 0.25,
    });
    let (_, quarter) = render_blocks(&mut proc, 1);
    let mut proc2 = make_processor(&fixture);
    proc2.note_event(note_on(ROOT));
    let (_, full) = render_blocks(&mut proc2, 1);
    assert!(
        (quarter - 0.25 * full).abs() < 4e-3,
        "velocity must scale linearly, got {quarter} (full {full})"
    );
}

// ---------------------------------------------------------------------------
// Housekeeping
// ---------------------------------------------------------------------------

#[test]
fn sample_accessor_returns_the_source() {
    let n = 128;
    let fixture = ramp_fixture("accessor", n, 48_000, 1.0 / n as f32);
    let proc = make_processor(&fixture);
    assert_eq!(proc.sample().frames().len(), n);
    assert_eq!(proc.sample().sample_rate(), 48_000);
}

/// Source frames consumed by one block, read back out of the ramp fixture
/// (`value = slope · cursor`, so the value delta *is* the cursor delta).
fn cursor_advance(block: &[f32], slope: f32) -> f32 {
    (block[block.len() - 1] - block[0]) / slope
}

/// P1/D11: the sample player bends through `PitchOffset`, and the bend is a
/// **glide**: the block right after the change has not reached the new rate
/// yet (a block-rate snap would land exactly on it).
#[test]
fn pitch_offset_glides_into_the_new_read_rate() {
    let slope = 1.0e-4;
    // Long enough that a 2× read never runs off the end during the test.
    let fixture = ramp_fixture("pitch-offset", 30_000, 48_000, slope);
    let mut proc = make_processor(&fixture);
    proc.note_event(note_on(ROOT));

    let base = cursor_advance(&first_block_left(&mut proc), slope);
    assert!(
        (base - 127.0).abs() < 1.0,
        "root note reads one frame per frame, got {base}"
    );

    proc.set_param(ParamId::PitchOffset, 12.0);
    let first = cursor_advance(&first_block_left(&mut proc), slope);
    assert!(
        first > base && first < 1.8 * base,
        "the first block must still be ramping, got {first} (base {base})"
    );

    // The 5 ms smoother settles within a few blocks; then it is an octave.
    for _ in 0..8 {
        let _ = first_block_left(&mut proc);
    }
    let settled = cursor_advance(&first_block_left(&mut proc), slope);
    assert!(
        (settled / base - 2.0).abs() < 0.02,
        "an octave up reads twice as fast, got {settled} vs {base}"
    );
}

/// The offset is the one pitch control that reaches **sounding** voices
/// (unlike `SampleRootNote`, whose trigger-time freeze is what D4 asks for).
#[test]
fn pitch_offset_moves_a_voice_that_is_already_playing() {
    let slope = 1.0e-4;
    let fixture = ramp_fixture("pitch-live", 30_000, 48_000, slope);
    let mut proc = make_processor(&fixture);
    proc.note_event(note_on(ROOT));
    // Settle the voice itself; no new note-on happens after this point.
    let _ = render_blocks(&mut proc, 4);
    proc.set_param(ParamId::PitchOffset, -12.0);
    for _ in 0..8 {
        let _ = first_block_left(&mut proc);
    }
    let slowed = cursor_advance(&first_block_left(&mut proc), slope);
    assert!(
        (slowed / 127.0 - 0.5).abs() < 0.02,
        "an octave down halves the rate without a retrigger, got {slowed}"
    );
}
