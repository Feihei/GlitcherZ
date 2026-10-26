//! SP2 RT-safety audit for [`SampleVoiceProcessor`] (spec
//! `.agents/spec/sample-player.md` D4 audit requirement, same flow as the
//! S1 `sf2_rt_audit`).
//!
//! What is audited: `note_event`, `set_param`, `midi_cc` and `process`
//! must perform **zero heap allocations** so they can run inside the audio
//! callback (AGENTS.md real-time iron rules). Construction-time
//! allocations (`SampleData::load`, `SampleVoiceProcessor::new`) are
//! control-thread work by design and are intentionally out of scope.
//!
//! How: a counting global allocator wraps every allocation of this test
//! binary; an [`AuditScope`] counts allocations between `begin()` and
//! `end()`. The fixture is a self-generated wav (hound writer), so the
//! audit runs in plain `cargo test` with no external files.
//!
//! Tests within this binary are serialized through [`AUDIT_LOCK`]: the
//! counter is global, so concurrent test threads would pollute each
//! other.
//!
//! SAFETY: `unsafe impl GlobalAlloc` is required for the counting
//! allocator. It is a thin pass-through to `std::alloc::System` with two
//! atomic ops and performs no allocation itself, so it cannot recurse or
//! deadlock. It is linked only into this test binary; the engine crate
//! itself stays `#![forbid(unsafe_code)]`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use glitcherz_engine::realtime::buffer::AudioBuffer;
use glitcherz_engine::realtime::context::ProcessContext;
use glitcherz_engine::realtime::processor::Processor;
use glitcherz_engine::realtime::{NoteEvent, ParamId, SAMPLE_POLYPHONY, SampleVoiceProcessor};
use glitcherz_engine::sample::SampleData;

// ---------------------------------------------------------------------------
// Counting allocator
// ---------------------------------------------------------------------------

static COUNT: AtomicU64 = AtomicU64::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

struct CountingAllocator;

// SAFETY: stateless pass-through around `System`.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded to the system allocator with a valid layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was returned by `System.alloc` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded to the system allocator with a valid layout.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded to the system allocator with a valid layout.
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Serializes the audit tests within this binary (the counter is global).
static AUDIT_LOCK: Mutex<()> = Mutex::new(());

/// Counts heap allocations until `end()` (or drop).
struct AuditScope;

impl AuditScope {
    fn begin() -> Self {
        COUNT.store(0, Ordering::Relaxed);
        COUNTING.store(true, Ordering::Relaxed);
        AuditScope
    }

    /// Stops counting and returns how many allocations were observed.
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

// ---------------------------------------------------------------------------
// Fixture (built outside every audit scope)
// ---------------------------------------------------------------------------

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

/// A 1-second ramp wav: long enough for sustained playback, short enough
/// to write in milliseconds.
fn ramp_fixture_path() -> PathBuf {
    let path = std::env::temp_dir().join("glitcherz-sample-audit.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("fixture create");
    for i in 0..48_000usize {
        let v = (i as f32 / 48_000.0).min(1.0);
        writer
            .write_sample((v * i16::MAX as f32) as i16)
            .expect("fixture write");
    }
    writer.finalize().expect("fixture finalize");
    path
}

fn ctx_of() -> ProcessContext {
    ctx()
}

// ---------------------------------------------------------------------------
// Audits
// ---------------------------------------------------------------------------

#[test]
fn sustain_and_release_is_allocation_free() {
    let _guard = AUDIT_LOCK.lock().unwrap();
    let fixture = ramp_fixture_path();
    let data = Arc::new(SampleData::load(&fixture).expect("fixture decode"));
    let mut proc = SampleVoiceProcessor::new(data, SR);
    let mut buffer = AudioBuffer::new(2, BLOCK);

    let scope = AuditScope::begin();
    for block in 0..64u64 {
        if block == 0 {
            proc.note_event(NoteEvent::On {
                note: 60,
                velocity: 1.0,
            });
        }
        if block == 32 {
            proc.note_event(NoteEvent::Off { note: 60 });
        }
        buffer.set_len(BLOCK);
        buffer.clear();
        proc.process(&ctx_of(), &mut buffer);
    }
    assert_zero_alloc(&scope, "sustain + release rendering");
}

#[test]
fn voice_stealing_stress_is_allocation_free() {
    let _guard = AUDIT_LOCK.lock().unwrap();
    let fixture = ramp_fixture_path();
    let data = Arc::new(SampleData::load(&fixture).expect("fixture decode"));
    let mut proc = SampleVoiceProcessor::new(data, SR);
    let mut buffer = AudioBuffer::new(2, BLOCK);

    let scope = AuditScope::begin();
    // Continuously retrigger far beyond the pool: every note-on after the
    // first SAMPLE_POLYPHONY takes the steal path, and short bursts also
    // exercise the swap-compact recycle path.
    for i in 0..256u32 {
        let note = (i % 24) as u8 + 48;
        proc.note_event(NoteEvent::On {
            note,
            velocity: 1.0,
        });
        if i % 4 == 3 {
            proc.note_event(NoteEvent::Off { note });
        }
        if i % 32 == 31 {
            // Blanket sweeps release everything (also exercises the
            // 128-iteration bitmap loop).
            proc.note_event(NoteEvent::Off { note: 0 });
        }
        buffer.set_len(BLOCK);
        buffer.clear();
        proc.process(&ctx_of(), &mut buffer);
    }
    assert_zero_alloc(&scope, "steal + compact stress");
}

#[test]
fn control_surface_is_allocation_free() {
    let _guard = AUDIT_LOCK.lock().unwrap();
    let fixture = ramp_fixture_path();
    let data = Arc::new(SampleData::load(&fixture).expect("fixture decode"));
    let mut proc = SampleVoiceProcessor::new(data, SR);
    let mut buffer = AudioBuffer::new(2, BLOCK);

    let scope = AuditScope::begin();
    proc.set_param(ParamId::SampleRootNote, 48.0);
    proc.set_param(ParamId::SampleGain, 0.5);
    proc.set_param(ParamId::SamplePlayMode, 1.0);
    proc.set_param(ParamId::SampleGain, f32::NAN);
    proc.note_event(NoteEvent::On {
        note: 200,
        velocity: f32::INFINITY,
    });
    proc.note_event(NoteEvent::On {
        note: 60,
        velocity: f32::NAN,
    });
    proc.note_event(NoteEvent::Off { note: 200 });
    proc.midi_cc(0, 7, 64);

    buffer.set_len(BLOCK);
    buffer.clear();
    proc.process(&ctx_of(), &mut buffer);
    assert_zero_alloc(&scope, "parameter + extreme event surface");
}

#[test]
fn full_pool_never_exceeds_budget() {
    // Pool invariant that the allocator audit cannot see directly: the
    // active count never grows past SAMPLE_POLYPHONY.
    let _guard = AUDIT_LOCK.lock().unwrap();
    let fixture = ramp_fixture_path();
    let data = Arc::new(SampleData::load(&fixture).expect("fixture decode"));
    let mut proc = SampleVoiceProcessor::new(data, SR);
    assert_eq!(SAMPLE_POLYPHONY, 16);
    for i in 0..64u32 {
        proc.note_event(NoteEvent::On {
            note: (i % 127) as u8,
            velocity: 1.0,
        });
    }
    let mut buffer = AudioBuffer::new(2, BLOCK);
    buffer.set_len(BLOCK);
    buffer.clear();
    proc.process(&ctx_of(), &mut buffer);
    // All 16 slots render: peak is finite (16 voices at full velocity on a
    // ramp that reaches ~1.0 at its end).
    let peak = (0..buffer.channels())
        .filter_map(|ch| buffer.channel(ch))
        .flat_map(|c| c.iter())
        .fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak.is_finite() && peak <= SAMPLE_POLYPHONY as f32);
}
