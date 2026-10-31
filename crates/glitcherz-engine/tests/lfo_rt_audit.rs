//! RT-safety audit for the LFO control source (spec `.agents/spec/lfo.md`
//! §8): `set_param` and `process_events` must perform **zero heap
//! allocations** so the node can run inside the audio callback (AGENTS.md
//! real-time iron rules). Construction is control-thread work and sits
//! outside every audit scope.
//!
//! How: a counting global allocator wraps every allocation of this test
//! binary; an [`AuditScope`] counts allocations between `begin()` and `end()`.
//! Tests are serialized through [`AUDIT_LOCK`] because the counter is global.
//!
//! SAFETY: `unsafe impl GlobalAlloc` is required for the counting allocator. It
//! is a thin pass-through to `std::alloc::System` with one atomic op and
//! performs no allocation itself, so it cannot recurse or deadlock. It is
//! linked only into this test binary; the engine crate itself stays
//! `#![forbid(unsafe_code)]`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use glitcherz_engine::realtime::LfoProcessor;
use glitcherz_engine::realtime::buffer::AudioBuffer;
use glitcherz_engine::realtime::commands::ParamId;
use glitcherz_engine::realtime::context::ProcessContext;
use glitcherz_engine::realtime::events::NodeEvents;
use glitcherz_engine::realtime::processor::Processor;

// ---------------------------------------------------------------------------
// Counting allocator
// ---------------------------------------------------------------------------

static COUNT: AtomicU64 = AtomicU64::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);
static AUDIT_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    AUDIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

fn ctx(active: bool, song_ticks: f64) -> ProcessContext {
    ProcessContext {
        sample_rate: SR,
        frames: BLOCK,
        playback_sample_pos: 0,
        song_ticks,
        transport_dt_ticks: 0.0,
        transport_active: active,
        sec_per_tick: 60.0 / (120.0 * 960.0),
    }
}

#[test]
fn lfo_set_param_and_process_events_do_not_allocate() {
    let _guard = lock();
    let mut processor = LfoProcessor::new(7);
    let mut out = NodeEvents::default();
    let mut buffer = AudioBuffer::new(1, BLOCK);

    let scope = AuditScope::begin();
    let _ = processor.set_param(ParamId::LfoRate, 2.0);
    let _ = processor.set_param(ParamId::LfoWaveform, 4.0);
    let _ = processor.set_param(ParamId::LfoDepth, 0.75);
    let _ = processor.set_param(ParamId::LfoSync, 1.0);
    let _ = processor.set_param(ParamId::LfoDivision, 3.0);
    assert_zero_alloc(&scope, "LFO set_param");

    // Playing path (Hz and sync covered by the parameter switch above).
    let playing = AuditScope::begin();
    for i in 0..64 {
        processor.process(&ctx(true, i as f64 * 128.0), &mut buffer);
        out.clear();
        processor.process_events(&ctx(true, i as f64 * 128.0), &mut out);
    }
    assert_zero_alloc(&playing, "LFO process_events (playing)");

    // Stopped path clears the bus without touching the phase.
    let stopped = AuditScope::begin();
    out.clear();
    processor.process_events(&ctx(false, 0.0), &mut out);
    assert_zero_alloc(&stopped, "LFO process_events (stopped)");
}
