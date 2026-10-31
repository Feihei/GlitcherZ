//! W1 drum kit: key→sample mapping, choke groups, pan and blanket stop
//! (spec `.agents/spec/drum-kit.md`).
//!
//! Fixtures are self-generated wavs, so the suite runs without external
//! assets. The mandatory pairs (AGENTS.md) are `silent_without_notes` and
//! `extreme_events_stay_finite`.

use std::path::PathBuf;
use std::sync::Arc;

use glitcherz_engine::realtime::buffer::AudioBuffer;
use glitcherz_engine::realtime::context::ProcessContext;
use glitcherz_engine::realtime::processor::Processor;
use glitcherz_engine::realtime::{
    DRUM_MAX_SLOTS, DRUM_POLYPHONY, DrumKitProcessor, DrumSlotSpec, NoteEvent, ParamId,
};
use glitcherz_engine::sample::SampleData;

const SR: f32 = 48_000.0;
const BLOCK: usize = 128;

fn ctx() -> ProcessContext {
    ProcessContext {
        sample_rate: SR,
        frames: BLOCK,
        playback_sample_pos: 0,
        song_ticks: 0.0,
        transport_dt_ticks: 0.0,
        transport_active: true,
        sec_per_tick: 60.0 / (120.0 * 960.0),
    }
}

/// Writes a mono 16-bit constant-amplitude wav and loads it.
fn flat(name: &str, frames: usize, value: f32) -> Arc<SampleData> {
    let path = temp_path(name);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("fixture create");
    for _ in 0..frames {
        writer
            .write_sample((value * i16::MAX as f32) as i16)
            .expect("fixture write");
    }
    writer.finalize().expect("fixture finalize");
    let data = SampleData::load(&path).expect("fixture decode");
    let _ = std::fs::remove_file(&path);
    Arc::new(data)
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("glitcherz-drum-{name}-{}.wav", std::process::id()))
}

fn slot(note: u8, data: Arc<SampleData>, pan: f32, choke: Option<u8>) -> DrumSlotSpec {
    DrumSlotSpec {
        note,
        data,
        gain: 1.0,
        pan,
        tune_semitones: 0.0,
        choke,
    }
}

fn render(processor: &mut DrumKitProcessor, blocks: usize) -> AudioBuffer {
    let mut buffer = AudioBuffer::new(2, BLOCK);
    for _ in 0..blocks {
        buffer.set_len(BLOCK);
        buffer.clear();
        processor.process(&ctx(), &mut buffer);
    }
    buffer
}

fn peak(channel: &[f32]) -> f32 {
    channel.iter().fold(0.0f32, |acc, v| acc.max(v.abs()))
}

fn hit(processor: &mut DrumKitProcessor, note: u8, velocity: f32) {
    processor.note_event(NoteEvent::On { note, velocity });
}

#[test]
fn silent_without_notes() {
    let mut processor = DrumKitProcessor::new(&[], SR);
    let buffer = render(&mut processor, 1);
    for channel in 0..2 {
        assert!(
            buffer.channel(channel).unwrap().iter().all(|v| *v == 0.0),
            "an empty kit must stay silent"
        );
    }
}

#[test]
fn unmapped_key_is_silent_and_mapped_key_plays() {
    let data = flat("map", 2_000, 0.5);
    let mut processor = DrumKitProcessor::new(&[slot(36, data, 0.0, None)], SR);

    hit(&mut processor, 40, 1.0);
    assert!(
        peak(render(&mut processor, 1).channel(0).unwrap()) < 1e-6,
        "an unmapped key must not make sound"
    );

    hit(&mut processor, 36, 1.0);
    assert!(peak(render(&mut processor, 1).channel(0).unwrap()) > 0.1);
}

#[test]
fn extreme_events_stay_finite() {
    let data = flat("extreme", 4_000, 1.0);
    let mut processor = DrumKitProcessor::new(&[slot(36, data, 0.0, None)], SR);
    for _ in 0..DRUM_POLYPHONY * 2 {
        hit(&mut processor, 36, 1.0);
    }
    let buffer = render(&mut processor, 4);
    for channel in 0..2 {
        assert!(
            buffer
                .channel(channel)
                .unwrap()
                .iter()
                .all(|v| v.is_finite()),
            "full-scale stacking must stay finite"
        );
    }
    // Zero velocity is a note-off, not a hit.
    hit(&mut processor, 36, 0.0);
    let buffer = render(&mut processor, 1);
    assert!(buffer.channel(0).unwrap().iter().all(|v| v.is_finite()));
}

#[test]
fn pan_places_a_hit_in_one_channel() {
    let data = flat("pan", 2_000, 0.5);
    let mut processor = DrumKitProcessor::new(&[slot(36, data, -1.0, None)], SR);
    hit(&mut processor, 36, 1.0);
    let buffer = render(&mut processor, 1);
    let left = peak(buffer.channel(0).unwrap());
    let right = peak(buffer.channel(1).unwrap());
    assert!(left > 0.1, "hard-left hit must sound (L={left}, R={right})");
    assert!(
        right < 1e-4,
        "hard-left hit must not leak right (R={right})"
    );
}

#[test]
fn choke_group_cuts_the_previous_hit() {
    let data = flat("choke", 20_000, 0.5);
    let mut processor = DrumKitProcessor::new(
        &[
            slot(42, Arc::clone(&data), -1.0, Some(1)),
            slot(46, data, 1.0, Some(1)),
        ],
        SR,
    );
    // Open hat on the left channel, then a choked hit on the right.
    hit(&mut processor, 42, 1.0);
    let first = render(&mut processor, 1);
    assert!(peak(first.channel(0).unwrap()) > 0.1);

    hit(&mut processor, 46, 1.0);
    let buffer = render(&mut processor, 6); // 10 ms fade ≈ 4 blocks
    assert!(
        peak(buffer.channel(0).unwrap()) < 1e-4,
        "the choked voice must fade out of the left channel"
    );
    assert!(
        peak(buffer.channel(1).unwrap()) > 0.1,
        "the new hit keeps sounding on the right"
    );
}

#[test]
fn blanket_stop_silences_every_voice() {
    let data = flat("stop", 8_000, 0.5);
    let mut processor = DrumKitProcessor::new(&[slot(36, data, 0.0, None)], SR);
    for _ in 0..4 {
        hit(&mut processor, 36, 1.0);
    }
    processor.note_event(NoteEvent::Off { note: 0 });
    let buffer = render(&mut processor, 8);
    assert!(
        peak(buffer.channel(0).unwrap()) < 1e-6,
        "transport stop must silence one-shots"
    );
}

#[test]
fn slot_count_ignores_the_document_cap() {
    let data = flat("cap", 100, 0.5);
    let specs: Vec<DrumSlotSpec> = (0..DRUM_MAX_SLOTS as u8 + 4)
        .map(|note| slot(note, Arc::clone(&data), 0.0, None))
        .collect();
    let processor = DrumKitProcessor::new(&specs, SR);
    assert_eq!(processor.slot_count(), DRUM_MAX_SLOTS);
}

#[test]
fn params_clamp_and_reject_non_finite_values() {
    let data = flat("params", 1_000, 0.5);
    let mut processor = DrumKitProcessor::new(&[slot(36, data, 0.0, None)], SR);
    assert!(!processor.set_param(ParamId::DrumGain, f32::NAN));
    assert!(!processor.set_param(ParamId::DrumSlotGain(36), f32::INFINITY));
    assert!(processor.set_param(ParamId::DrumGain, 4.0));
    assert!(processor.set_param(ParamId::DrumSlotGain(36), 5.0));
    assert!(processor.set_param(ParamId::DrumSlotPan(36), -5.0));
    assert!(processor.set_param(ParamId::DrumSlotTune(36), 100.0));
    assert!(processor.set_param(ParamId::DrumSlotChoke(36), 3.0));
    assert!(processor.set_param(ParamId::PitchOffset, 12.0));
    assert!(!processor.set_param(ParamId::PitchOffset, f32::NAN));
    assert!(!processor.set_param(ParamId::OscFrequency, 440.0));
    hit(&mut processor, 36, 1.0);
    let buffer = render(&mut processor, 1);
    assert!(buffer.channel(0).unwrap().iter().all(|v| v.is_finite()));
}
