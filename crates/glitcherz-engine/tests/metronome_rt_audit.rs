//! RT-safety and integration audit for the transport metronome
//! (`spec/recording.md` D1–D5).
//!
//! What is audited:
//!
//! 1. `BlockRunner::render_interleaved` performs **zero heap allocations** with
//!    the click enabled and firing — the click tables are built once at
//!    construction and `Metronome::tick` only reads them.
//! 2. The click is audible with an **empty graph** (it is mixed after the
//!    graph, so muting/deleting everything cannot silence it).
//! 3. A stopped transport is silent even with the click enabled.
//!
//! How: the same counting-allocator flow as `audio_track_rt_audit` — a global
//! allocator wraps `System`, and an `AuditScope` counts between `begin()` and
//! `end()`. Tests are serialized because the counter is process-global.
//!
//! SAFETY: `unsafe impl GlobalAlloc` is required for the counting allocator.
//! It is a thin pass-through to `std::alloc::System` with two atomic ops and
//! allocates nothing itself, so it cannot recurse or deadlock. It is linked
//! only into this test binary; the engine crate stays `#![forbid(unsafe_code)]`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use glitcherz_engine::realtime::block::{BlockRunner, Feedback};
use glitcherz_engine::realtime::commands::{EngineCommand, SequencerCommand, command_channel};
use glitcherz_engine::realtime::graph::GraphCompiler;
use glitcherz_engine::realtime::sequencer::Sequencer;
use rtrb::Producer;

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

const SR: f32 = 48_000.0;
const BLOCK: usize = 128;

/// Builds a runner around the default (empty) graph.
fn make_runner() -> (BlockRunner, Producer<EngineCommand>) {
    let (tx, rx) = command_channel(64);
    let (_graph_tx, graph_rx) = glitcherz_engine::realtime::voice::structure_channel();
    let (recycle_tx, _recycle_rx) = glitcherz_engine::realtime::voice::recycle_channel();
    let (audio_trash_tx, audio_trash_rx) =
        glitcherz_engine::realtime::audio_track::audio_clip_trash_channel();
    drop(audio_trash_rx);
    let (midi_trash_tx, midi_trash_rx) =
        glitcherz_engine::realtime::sequencer::midi_clip_trash_channel();
    drop(midi_trash_rx);
    let (_compiler, graph) = GraphCompiler::default_graph(SR, BLOCK);
    let feedback = Feedback::new();
    let sequencer = Sequencer::new(SR);
    let runner = BlockRunner::new(
        BLOCK,
        2,
        SR,
        graph,
        sequencer,
        rx,
        graph_rx,
        recycle_tx,
        audio_trash_tx,
        midi_trash_tx,
        glitcherz_engine::realtime::CaptureTap::detached(),
        feedback,
    );
    (runner, tx)
}

/// Enables the click and starts the transport (commands land at the next
/// block boundary).
fn start_clicking(tx: &mut Producer<EngineCommand>) {
    tx.push(EngineCommand::Sequencer(SequencerCommand::SetMetronome {
        enabled: true,
        gain: 0.8,
    }))
    .expect("command queue not full");
    tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
        .expect("command queue not full");
}

/// Renders `frames` frames of stereo output, returning the peak amplitude.
fn render_peak(runner: &mut BlockRunner, buffer: &mut [f32], frames: usize) -> f32 {
    let mut rendered = 0;
    let mut peak = 0.0f32;
    while rendered < frames {
        let n = BLOCK.min(frames - rendered);
        let slice = &mut buffer[rendered * 2..(rendered + n) * 2];
        runner.render_interleaved(slice);
        for sample in slice.iter() {
            peak = peak.max(sample.abs());
        }
        rendered += n;
    }
    peak
}

#[test]
fn click_is_audible_with_an_empty_graph() {
    let _guard = AUDIT_LOCK.lock().expect("audit lock");
    let (mut runner, mut tx) = make_runner();
    start_clicking(&mut tx);
    let mut buffer = vec![0.0f32; SR as usize * 2];
    let peak = render_peak(&mut runner, &mut buffer, SR as usize);
    assert!(
        peak > 0.1,
        "the click must be mixed after the graph; peak = {peak}"
    );
}

#[test]
fn click_stays_off_until_enabled() {
    let _guard = AUDIT_LOCK.lock().expect("audit lock");
    let (mut runner, mut tx) = make_runner();
    tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
        .expect("command queue not full");
    let mut buffer = vec![0.0f32; SR as usize * 2];
    let peak = render_peak(&mut runner, &mut buffer, SR as usize);
    assert_eq!(peak, 0.0, "no click until SetMetronome arrives");
}

#[test]
fn stopped_transport_renders_no_click() {
    let _guard = AUDIT_LOCK.lock().expect("audit lock");
    let (mut runner, mut tx) = make_runner();
    tx.push(EngineCommand::Sequencer(SequencerCommand::SetMetronome {
        enabled: true,
        gain: 1.0,
    }))
    .expect("command queue not full");
    let mut buffer = vec![0.0f32; SR as usize * 2];
    let peak = render_peak(&mut runner, &mut buffer, SR as usize);
    assert_eq!(peak, 0.0, "a stopped transport must not click");
}

#[test]
fn clicking_render_allocates_nothing() {
    let _guard = AUDIT_LOCK.lock().expect("audit lock");
    let (mut runner, mut tx) = make_runner();
    start_clicking(&mut tx);
    // Warm up: consume the commands, let the first click start.
    let mut buffer = vec![0.0f32; BLOCK * 4 * 2];
    render_peak(&mut runner, &mut buffer, BLOCK * 4);
    // Audit ~1 second: several clicks fire and decay to silence inside it.
    let mut block = vec![0.0f32; BLOCK * 2];
    let scope = AuditScope::begin();
    for _ in 0..(SR as usize / BLOCK) {
        runner.render_interleaved(&mut block);
    }
    let allocations = scope.end();
    assert_eq!(
        allocations, 0,
        "the metronome path performed {allocations} heap allocations (expected 0)"
    );
}
