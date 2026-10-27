//! RT-safety audit for the built-in effects (spec `.agents/spec/effects.md`
//! §11 test 8).
//!
//! What is audited: `set_param` and `process` on [`EqProcessor`],
//! [`CompressorProcessor`] and [`DelayProcessor`] must perform **zero heap
//! allocations** so they can run inside the audio callback (AGENTS.md
//! real-time iron rules). Construction-time work (`new`, the `Arc` for the
//! meter handle) is control-thread work by design and sits outside every
//! audit scope.
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use glitcherz_engine::realtime::buffer::AudioBuffer;
use glitcherz_engine::realtime::commands::ParamId;
use glitcherz_engine::realtime::context::ProcessContext;
use glitcherz_engine::realtime::processor::Processor;
use glitcherz_engine::realtime::{
    ChorusProcessor, CompressorProcessor, DelayProcessor, EffectMeters, EqProcessor,
    FlangerProcessor, ReverbProcessor,
};

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
// Fixtures (built outside every audit scope)
// ---------------------------------------------------------------------------

const SR: f32 = 48_000.0;
const BLOCK: usize = 128;
const SLOTS: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

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

/// A pre-filled stereo buffer (allocated before the scope opens).
fn fixture() -> AudioBuffer {
    let mut buffer = AudioBuffer::new(2, BLOCK);
    buffer.set_len(BLOCK);
    for ch in 0..2 {
        if let Some(channel) = buffer.channel_mut(ch) {
            for (index, sample) in channel.iter_mut().enumerate() {
                *sample = ((index as f32) * 0.05).sin() * 0.8;
            }
        }
    }
    buffer
}

/// Drives one processor through `blocks` blocks of parameter churn and audio.
fn exercise(processor: &mut dyn Processor, buffer: &mut AudioBuffer, blocks: usize) {
    for block in 0..blocks {
        for (index, slot) in SLOTS.iter().enumerate() {
            // Sweep every slot so no branch stays cold.
            let value = ((block + index) as f32 * 0.37).sin();
            processor.set_param(ParamId::BuiltinParam(*slot), value);
            processor.set_param(ParamId::BuiltinParam(*slot), f32::NAN);
        }
        processor.process(&ctx(), buffer);
    }
}

/// Every built-in effect, in one test.
///
/// Deliberately **one** test function: the allocation counter is global and
/// the test harness runs test functions on parallel threads, whose bookkeeping
/// allocations would land inside another test's counting window (flaky, and
/// not the audited code's fault). A single test means a single worker thread
/// while `COUNTING` is on.
#[test]
fn built_in_effects_are_allocation_free() {
    // Eq: parameter churn while rendering.
    let mut eq = EqProcessor::new(SR);
    let mut eq_buffer = fixture();
    exercise(&mut eq, &mut eq_buffer, 2);
    let scope = AuditScope::begin();
    exercise(&mut eq, &mut eq_buffer, 64);
    assert_zero_alloc(&scope, "EqProcessor process/set_param");

    // Compressor: same, with a meter handle attached (the RT path writes it).
    let meters = Arc::new(EffectMeters::new());
    let mut comp = CompressorProcessor::with_meters(SR, Some(meters));
    let mut comp_buffer = fixture();
    exercise(&mut comp, &mut comp_buffer, 2);
    let scope = AuditScope::begin();
    exercise(&mut comp, &mut comp_buffer, 64);
    assert_zero_alloc(&scope, "CompressorProcessor process/set_param");

    // Delay: the E1 baseline, kept in the same audit so a regression in the
    // shared parameter bank shows up here too.
    let mut delay = DelayProcessor::new(SR);
    let mut delay_buffer = fixture();
    exercise(&mut delay, &mut delay_buffer, 2);
    let scope = AuditScope::begin();
    exercise(&mut delay, &mut delay_buffer, 64);
    assert_zero_alloc(&scope, "DelayProcessor process/set_param");

    // Reverb: the FDN runs feedback, filters and the Hadamard matrix per
    // sample; it must stay allocation-free.
    let mut reverb = ReverbProcessor::new(SR);
    let mut reverb_buffer = fixture();
    exercise(&mut reverb, &mut reverb_buffer, 2);
    let scope = AuditScope::begin();
    exercise(&mut reverb, &mut reverb_buffer, 64);
    assert_zero_alloc(&scope, "ReverbProcessor process/set_param");

    // Chorus / Flanger: fractional reads and (for the flanger) feedback every
    // sample; both must stay allocation-free.
    let mut chorus = ChorusProcessor::new(SR);
    let mut chorus_buffer = fixture();
    exercise(&mut chorus, &mut chorus_buffer, 2);
    let scope = AuditScope::begin();
    exercise(&mut chorus, &mut chorus_buffer, 64);
    assert_zero_alloc(&scope, "ChorusProcessor process/set_param");

    let mut flanger = FlangerProcessor::new(SR);
    let mut flanger_buffer = fixture();
    exercise(&mut flanger, &mut flanger_buffer, 2);
    let scope = AuditScope::begin();
    exercise(&mut flanger, &mut flanger_buffer, 64);
    assert_zero_alloc(&scope, "FlangerProcessor process/set_param");

    // The meter handle itself is written from the audio thread.
    let meters = EffectMeters::new();
    let scope = AuditScope::begin();
    for block in 0..256 {
        meters.store(-(block as f32) * 0.01, 0.5, 0.25);
        let _ = meters.read();
    }
    assert_zero_alloc(&scope, "EffectMeters store/read");
}
