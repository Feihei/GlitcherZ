//! U6 RT-safety audit for [`AudioTrackProcessor`] (spec
//! `.agents/spec/audio-track.md` D7, same flow as `sf2_rt_audit` /
//! `sample_rt_audit`).
//!
//! What is audited: `process` and `swap_clips` must perform **zero heap
//! allocations** so they can run inside the audio callback (AGENTS.md
//! real-time iron rules) — in particular the swap must **move** the old
//! table out (the drop happens on the control thread via the trash queue).
//! Construction-time work (fixture decode, table build) is out of scope.
//!
//! How: a counting global allocator wraps every allocation of this test
//! binary; an [`AuditScope`] counts allocations between `begin()` and
//! `end()`. Tests are serialized through [`AUDIT_LOCK`].
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

use glitcherz_engine::realtime::audio_track::{
    AudioClipTable, AudioTrackProcessor, RtAudioClip, empty_clip_table,
};
use glitcherz_engine::realtime::buffer::AudioBuffer;
use glitcherz_engine::realtime::context::ProcessContext;
use glitcherz_engine::realtime::processor::Processor;
use glitcherz_engine::sample::SampleData;

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

fn ctx(song_ticks: f64, active: bool) -> ProcessContext {
    ProcessContext {
        sample_rate: SR,
        frames: BLOCK,
        playback_sample_pos: 0,
        song_ticks,
        transport_dt_ticks: 5.12,
        transport_active: active,
    }
}

fn ramp_fixture_path() -> PathBuf {
    let path = std::env::temp_dir().join("glitcherz-audiotrack-audit.wav");
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

fn table_with(data: &Arc<SampleData>, start: f64, span: f64) -> AudioClipTable {
    let mut table = empty_clip_table();
    table[0] = Some(RtAudioClip {
        start_ticks: start,
        span_ticks: span,
        data: Arc::clone(data),
        gain: 1.0,
    });
    table[1] = Some(RtAudioClip {
        start_ticks: start + 200.0,
        span_ticks: span,
        data: Arc::clone(data),
        gain: 0.5,
    });
    table
}

#[test]
fn sustained_playback_is_allocation_free() {
    let _guard = AUDIT_LOCK.lock().unwrap();
    let data = Arc::new(SampleData::load(&ramp_fixture_path()).expect("decode"));
    let mut proc = AudioTrackProcessor::new(table_with(&data, 0.0, 96_000.0));
    let mut buffer = AudioBuffer::new(2, BLOCK);

    let scope = AuditScope::begin();
    for block in 0..64u64 {
        buffer.set_len(BLOCK);
        buffer.clear();
        proc.process(&ctx(block as f64 * 5.12, true), &mut buffer);
    }
    assert_zero_alloc(&scope, "sustained clip playback");
}

#[test]
fn clip_table_swaps_are_allocation_free_and_drop_free() {
    let _guard = AUDIT_LOCK.lock().unwrap();
    let data = Arc::new(SampleData::load(&ramp_fixture_path()).expect("decode"));
    let mut proc = AudioTrackProcessor::new(table_with(&data, 0.0, 96_000.0));
    let mut buffer = AudioBuffer::new(2, BLOCK);
    // Parked outside the scope: displaced tables must outlive the audit
    // window (dropping them inside would count as a deallocation here —
    // the trash queue owns that on the control thread). Pre-allocated so
    // the audit window never sees the Vec grow.
    let mut displaced: Vec<AudioClipTable> = Vec::with_capacity(16);

    let scope = AuditScope::begin();
    for i in 0..16u32 {
        let start = f64::from(i) * 300.0;
        let old = proc.swap_clips(table_with(&data, start, 48_000.0));
        displaced.push(old);
        buffer.set_len(BLOCK);
        buffer.clear();
        proc.process(&ctx(start, true), &mut buffer);
        // Also exercise the inactive (stopped) path.
        proc.process(&ctx(start, false), &mut buffer);
    }
    assert_zero_alloc(&scope, "clip table swaps + renders");
    drop(displaced); // Control-thread-side drops, outside the audit window.
}
