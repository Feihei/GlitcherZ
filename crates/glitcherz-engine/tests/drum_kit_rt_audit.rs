//! W1 RT-safety audit for [`DrumKitProcessor`] (spec `.agents/spec/drum-kit.md`
//! K4; same flow as `sample_rt_audit`).
//!
//! Audited: `note_event`, `set_param`, `midi_cc` and `process` must perform
//! zero heap allocations. Construction (`SampleData::load`,
//! `DrumKitProcessor::new`) is control-thread work and out of scope.
//!
//! SAFETY: the counting allocator is a thin pass-through to
//! `std::alloc::System`; it is linked only into this test binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use glitcherz_engine::realtime::buffer::AudioBuffer;
use glitcherz_engine::realtime::context::ProcessContext;
use glitcherz_engine::realtime::processor::Processor;
use glitcherz_engine::realtime::{
    DRUM_POLYPHONY, DrumKitProcessor, DrumSlotSpec, NoteEvent, ParamId,
};
use glitcherz_engine::sample::SampleData;

static COUNT: AtomicU64 = AtomicU64::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded with a valid layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded with a valid layout.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded with a valid layout.
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

static AUDIT_LOCK: Mutex<()> = Mutex::new(());

struct AuditScope;

impl AuditScope {
    fn begin() -> Self {
        COUNT.store(0, Ordering::Relaxed);
        COUNTING.store(true, Ordering::Relaxed);
        AuditScope
    }

    fn end(&self) -> u64 {
        COUNTING.store(false, Ordering::Relaxed);
        COUNT.load(Ordering::Relaxed)
    }
}

impl Drop for AuditScope {
    fn drop(&mut self) {
        COUNTING.store(false, Ordering::Relaxed);
    }
}

fn assert_zero_alloc(scope: &AuditScope, what: &str) {
    let n = scope.end();
    assert!(n == 0, "{what} performed {n} heap allocations (expected 0)");
}

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

fn fixture_path(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "glitcherz-drum-audit-{name}-{}.wav",
        std::process::id()
    ));
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("fixture create");
    for i in 0..48_000usize {
        let v = ((i as f32 / 48_000.0).min(1.0) * 0.5 * i16::MAX as f32) as i16;
        writer.write_sample(v).expect("left");
        writer.write_sample(v).expect("right");
    }
    writer.finalize().expect("fixture finalize");
    path
}

fn kit(count: usize) -> DrumKitProcessor {
    let mut data = Vec::with_capacity(count);
    for i in 0..count {
        let path = fixture_path(&format!("{i}"));
        let decoded = Arc::new(SampleData::load(&path).expect("decode"));
        let _ = std::fs::remove_file(&path);
        data.push(decoded);
    }
    let specs: Vec<DrumSlotSpec> = data
        .into_iter()
        .enumerate()
        .map(|(i, data)| DrumSlotSpec {
            note: 36 + i as u8,
            data,
            gain: 1.0,
            pan: 0.0,
            tune_semitones: 0.0,
            choke: Some(1),
        })
        .collect();
    DrumKitProcessor::new(&specs, SR)
}

fn buffer() -> AudioBuffer {
    AudioBuffer::new(2, BLOCK)
}

#[test]
fn note_events_and_process_do_not_allocate() {
    let _guard = AUDIT_LOCK.lock().expect("audit lock");
    let mut processor = kit(DRUM_POLYPHONY);
    let mut out = buffer();
    out.set_len(BLOCK);
    let scope = AuditScope::begin();
    for note in 36..36 + DRUM_POLYPHONY as u8 {
        processor.note_event(NoteEvent::On {
            note,
            velocity: 1.0,
        });
    }
    // Force a voice-steal pass as well.
    for _ in 0..DRUM_POLYPHONY as u32 {
        processor.note_event(NoteEvent::On {
            note: 36,
            velocity: 0.8,
        });
    }
    processor.process(&ctx(), &mut out);
    assert_zero_alloc(&scope, "note_event + process");
    assert!(out.channel(0).unwrap().iter().all(|v| v.is_finite()));
}

#[test]
fn control_parameters_do_not_allocate() {
    let _guard = AUDIT_LOCK.lock().expect("audit lock");
    let mut processor = kit(4);
    let scope = AuditScope::begin();
    for note in 36..40 {
        processor.set_param(ParamId::DrumSlotGain(note), 0.5);
        processor.set_param(ParamId::DrumSlotPan(note), -0.5);
        processor.set_param(ParamId::DrumSlotTune(note), 3.0);
        processor.set_param(ParamId::DrumSlotChoke(note), 2.0);
    }
    processor.set_param(ParamId::DrumGain, 0.9);
    processor.set_param(ParamId::PitchOffset, 2.0);
    processor.midi_cc(0, 7, 100);
    assert_zero_alloc(&scope, "set_param + midi_cc");
}

#[test]
fn blanket_stop_does_not_allocate() {
    let _guard = AUDIT_LOCK.lock().expect("audit lock");
    let mut processor = kit(4);
    let mut out = buffer();
    out.set_len(BLOCK);
    for note in 36..40 {
        processor.note_event(NoteEvent::On {
            note,
            velocity: 1.0,
        });
    }
    processor.process(&ctx(), &mut out);
    let scope = AuditScope::begin();
    processor.note_event(NoteEvent::Off { note: 0 });
    processor.process(&ctx(), &mut out);
    assert_zero_alloc(&scope, "blanket stop + process");
}
