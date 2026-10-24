//! U6 integration tests for [`AudioTrackProcessor`] (spec
//! `.agents/spec/audio-track.md` A2 / D1 / D7).
//!
//! Fixtures are self-generated wavs (hound `WavWriter`), so everything runs
//! in plain `cargo test`. The mandatory voice test pairs (AGENTS.md) are
//! `silence_when_transport_inactive` and `extreme_tables_stay_finite`.

use std::path::PathBuf;
use std::sync::Arc;

use glitcherz_engine::realtime::audio_track::{
    AudioClipTable, AudioTrackProcessor, RtAudioClip, empty_clip_table,
};
use glitcherz_engine::realtime::buffer::AudioBuffer;
use glitcherz_engine::realtime::context::ProcessContext;
use glitcherz_engine::realtime::processor::Processor;
use glitcherz_engine::sample::SampleData;

const SR: f32 = 48_000.0;
const BLOCK: usize = 128;
/// Ticks per block at 120 BPM, 960 PPQ, 48 kHz:
/// 120/60 × 960 / 48000 × 128 = 512 ticks… per bar? — per block:
/// 2 × 960 / 48000 × 128 = 5.12 ticks.
const DT_TICKS: f64 = 5.12;

fn ctx_at(song_ticks: f64) -> ProcessContext {
    ProcessContext {
        sample_rate: SR,
        frames: BLOCK,
        playback_sample_pos: 0,
        song_ticks,
        transport_dt_ticks: DT_TICKS,
        transport_active: true,
    }
}

fn ctx_inactive() -> ProcessContext {
    ProcessContext {
        transport_active: false,
        ..ctx_at(0.0)
    }
}

/// Writes a mono 16-bit **constant-amplitude** wav (gain / stacking
/// assertions compare block peaks directly).
fn flat_fixture(name: &str, frames: usize) -> PathBuf {
    let path = std::env::temp_dir().join(format!("glitcherz-audiotrack-{name}.wav"));
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("fixture create");
    for _ in 0..frames {
        writer.write_sample(i16::MAX).expect("fixture write");
    }
    writer.finalize().expect("fixture finalize");
    path
}

/// Writes a mono 16-bit ramp wav (`src[i] = i/n`): "output = source read at
/// the stretched rate" assertions are exact at integer wav positions.
fn ramp_fixture(name: &str, frames: usize) -> PathBuf {
    let path = std::env::temp_dir().join(format!("glitcherz-audiotrack-{name}.wav"));
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("fixture create");
    for i in 0..frames {
        let v = i as f32 / frames as f32;
        writer
            .write_sample((v * i16::MAX as f32) as i16)
            .expect("fixture write");
    }
    writer.finalize().expect("fixture finalize");
    path
}

fn load(path: &std::path::Path) -> Arc<SampleData> {
    Arc::new(SampleData::load(path).expect("fixture must decode"))
}

fn table(clips: &[RtAudioClip]) -> AudioClipTable {
    let mut t = empty_clip_table();
    for (slot, clip) in clips.iter().enumerate().take(t.len()) {
        t[slot] = Some(clip.clone());
    }
    t
}

fn one_clip(data: Arc<SampleData>, start: f64, span: f64, gain: f32) -> AudioClipTable {
    table(&[RtAudioClip {
        start_ticks: start,
        span_ticks: span,
        data,
        gain,
    }])
}

fn render(proc: &mut AudioTrackProcessor, ctx: &ProcessContext) -> Vec<f32> {
    let mut buffer = AudioBuffer::new(2, BLOCK);
    buffer.set_len(BLOCK);
    buffer.clear();
    proc.process(ctx, &mut buffer);
    buffer.channel(0).unwrap().to_vec()
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

// ---------------------------------------------------------------------------
// Mandatory voice tests (AGENTS.md)
// ---------------------------------------------------------------------------

#[test]
fn silence_when_transport_inactive() {
    let data = load(&flat_fixture("stop", 4096));
    let mut proc = AudioTrackProcessor::new(one_clip(data, 0.0, 960.0, 1.0));
    // The clip covers the position — but the transport is stopped, so the
    // block must be silent (a frozen position would otherwise replay the
    // same frames forever).
    let out = render(&mut proc, &ctx_inactive());
    assert!(
        out.iter().all(|s| s.abs() < 1e-9),
        "stopped transport must be silent"
    );
}

#[test]
fn extreme_tables_stay_finite() {
    let data = load(&flat_fixture("extreme", 4096));
    let mut proc = AudioTrackProcessor::new(table(&[
        RtAudioClip {
            start_ticks: 0.0,
            span_ticks: 960.0,
            data: Arc::clone(&data),
            gain: f32::NAN,
        },
        RtAudioClip {
            start_ticks: f64::NAN,
            span_ticks: -1.0,
            data: Arc::clone(&data),
            gain: f32::INFINITY,
        },
        RtAudioClip {
            start_ticks: 0.0,
            span_ticks: 0.0,
            data: Arc::clone(&data),
            gain: 1.0,
        },
    ]));
    let out = render(&mut proc, &ctx_at(0.0));
    assert!(
        out.iter().all(|s| s.is_finite()),
        "NaN gain must not poison the render"
    );
}

// ---------------------------------------------------------------------------
// Playback math (D1): frame = (t − start)/span × wav_frames
// ---------------------------------------------------------------------------

#[test]
fn flat_clip_plays_at_unit_gain() {
    let data = load(&flat_fixture("flat", 4096));
    let mut proc = AudioTrackProcessor::new(one_clip(data, 0.0, 960.0, 1.0));
    let out = render(&mut proc, &ctx_at(0.0));
    assert!(
        out.iter().all(|s| (*s - 1.0).abs() < 2e-3),
        "constant wav must render at unit amplitude, got {:?}..",
        &out[..4]
    );
}

#[test]
fn stretching_the_span_slows_the_read() {
    // D1: doubling the span halves the read rate (tape-style stretching).
    let n = 4096.0f64;
    let ramp = load(&ramp_fixture("ramp", n as usize));

    let mut fast = AudioTrackProcessor::new(one_clip(Arc::clone(&ramp), 0.0, 960.0, 1.0));
    let out_fast = render(&mut fast, &ctx_at(0.0));

    let mut slow = AudioTrackProcessor::new(one_clip(ramp, 0.0, 1920.0, 1.0));
    let out_slow = render(&mut slow, &ctx_at(0.0));

    // Frame j's tick is j·dt/frames (j×0.04); its wav position is
    // tick/span × (n−1), so the ratio between the two outputs at every
    // frame is exactly the span ratio (2.0). Small frames carry 16-bit
    // quantization noise, so the tolerance scales accordingly.
    for j in [1usize, 32, 64, 96, 127] {
        let ratio = out_fast[j] / out_slow[j];
        assert!(
            (ratio - 2.0).abs() < 0.08,
            "frame {j}: span ratio must drive the read rate, got {ratio} ({}/{}",
            out_fast[j],
            out_slow[j]
        );
    }
}

#[test]
fn mid_clip_start_reads_the_proportional_offset() {
    // Loop wrap / seek need no cursor: starting mid-clip reads from the
    // proportional wav position directly (frame 0 at tick 480 of a 960-span
    // clip = the wav's midpoint).
    let n = 4096;
    let ramp = load(&ramp_fixture("mid", n));
    let mut proc = AudioTrackProcessor::new(one_clip(ramp, 0.0, 960.0, 1.0));
    let out = render(&mut proc, &ctx_at(480.0));
    // Frame 0 reads wav position 0.5 × (4096-1); the ramp's value there is
    // 0.5 (slope 1/4096 per frame).
    assert!(
        (out[0] - 0.5).abs() < 2e-3,
        "mid-clip start must read the midpoint, got {}",
        out[0]
    );
}

#[test]
fn clip_boundaries_cut_exactly() {
    // Outside [start, start+span) nothing plays — including a block that
    // starts exactly at the clip end.
    let data = load(&flat_fixture("edge", 4096));
    let mut proc = AudioTrackProcessor::new(one_clip(data, 0.0, 960.0, 1.0));
    let before_end = render(&mut proc, &ctx_at(954.88));
    assert!(peak(&before_end) > 0.9, "inside the span must sound");
    let at_end = render(&mut proc, &ctx_at(960.0));
    assert!(
        peak(&at_end) < 1e-9,
        "a block starting at span end must be silent"
    );
    let before_start = render(&mut proc, &ctx_at(-5.12));
    assert!(
        peak(&before_start) < 1e-9,
        "before the start must be silent"
    );
}

#[test]
fn overlapping_clips_sum() {
    let data = load(&flat_fixture("sum", 4096));
    let mut proc = AudioTrackProcessor::new(table(&[
        RtAudioClip {
            start_ticks: 0.0,
            span_ticks: 960.0,
            data: Arc::clone(&data),
            gain: 0.25,
        },
        RtAudioClip {
            start_ticks: 0.0,
            span_ticks: 1920.0,
            data: Arc::clone(&data),
            gain: 0.5,
        },
    ]));
    let out = render(&mut proc, &ctx_at(0.0));
    assert!(
        out.iter().all(|s| (*s - 0.75).abs() < 2e-3),
        "overlapping clips must sum their gains, got {:?}..",
        &out[..4]
    );
}

#[test]
fn clip_gain_scales_the_output() {
    let data = load(&flat_fixture("gain", 4096));
    let mut proc = AudioTrackProcessor::new(one_clip(data, 0.0, 960.0, 0.5));
    let out = render(&mut proc, &ctx_at(0.0));
    assert!(
        out.iter().all(|s| (*s - 0.5).abs() < 2e-3),
        "clip gain must scale, got {:?}..",
        &out[..4]
    );
}

// ---------------------------------------------------------------------------
// Clip table swap (D4)
// ---------------------------------------------------------------------------

#[test]
fn swap_clips_moves_the_old_table_out() {
    let data = load(&flat_fixture("swap", 4096));
    let first = one_clip(Arc::clone(&data), 0.0, 960.0, 1.0);
    let second = one_clip(Arc::clone(&data), 100.0, 1920.0, 1.0);
    let mut proc = AudioTrackProcessor::new(first);
    let displaced = proc.swap_clips(second);
    // The displaced table is the old one, moved out — same Arc identity.
    let old = displaced[0].as_ref().expect("slot 0 populated");
    assert!(Arc::ptr_eq(&old.data, &data));
    // And the new table is live.
    let new = proc.clips()[0].as_ref().expect("new slot 0 populated");
    assert!((new.start_ticks - 100.0).abs() < f64::EPSILON);
}

/// The **trait** path (`&mut dyn Processor`) must consume tables too —
/// `RtGraph::set_audio_clips` goes through it. This locks the override in
/// the `impl Processor` block (it was once misplaced in the inherent impl,
/// silently leaving the trait default's `None` active).
#[test]
fn swap_clips_via_the_processor_trait_consumes_tables() {
    use glitcherz_engine::realtime::processor::Processor;

    let data = load(&flat_fixture("trait", 4096));
    let first = one_clip(Arc::clone(&data), 0.0, 960.0, 1.0);
    let second = one_clip(Arc::clone(&data), 50.0, 480.0, 1.0);
    let mut proc: Box<dyn Processor> = Box::new(AudioTrackProcessor::new(first));
    let displaced = proc
        .swap_audio_clips(second)
        .expect("the trait override must consume the table");
    let old = displaced[0].as_ref().expect("old slot 0 populated");
    assert!(Arc::ptr_eq(&old.data, &data));
}

#[test]
fn empty_table_is_silent() {
    let mut proc = AudioTrackProcessor::new(empty_clip_table());
    let out = render(&mut proc, &ctx_at(0.0));
    assert!(peak(&out) < 1e-9);
}

// ---------------------------------------------------------------------------
// Peaks (D6)
// ---------------------------------------------------------------------------

#[test]
fn peaks_cover_the_frames_and_fold_channels() {
    // 600 frames → ceil(600/256) = 3 columns; a full-scale middle bucket
    // must show ±~1 in its pair.
    let n = 600;
    let path = std::env::temp_dir().join("glitcherz-audiotrack-peaks.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("fixture create");
    for i in 0..n {
        // Half wave: 0..300 ramp up to full scale, 300..600 back down.
        let v = if i < 300 {
            i as f32 / 300.0
        } else {
            (600 - i) as f32 / 300.0
        };
        writer
            .write_sample((v * i16::MAX as f32) as i16)
            .expect("fixture write");
    }
    writer.finalize().expect("fixture finalize");
    let data = SampleData::load(&path).expect("decode");
    let peaks = data.peaks();
    assert_eq!(peaks.len(), 3, "600 frames / 256 per column");
    // Column 0 spans frames 0..256 → peak ≈ 255/300 ≈ 0.85.
    assert!((peaks[0].1 - 255.0 / 300.0).abs() < 2e-3);
    assert!(peaks[0].0.abs() < 1e-3, "ramp starts at zero");
    // Column 1 spans the full-scale middle.
    assert!((peaks[1].1 - 1.0).abs() < 2e-3);
}
