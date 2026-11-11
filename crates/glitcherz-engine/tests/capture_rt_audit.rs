//! RT-safety and integration audit for the capture tap (`spec/recording.md`
//! D6/D7).
//!
//! What is audited:
//!
//! 1. Live events are stamped with the **transport position** at the block
//!    boundary the audio thread consumed them on.
//! 2. The tap is **off by default** — a session that never records captures
//!    nothing.
//! 3. **Sequencer-scheduled** notes are never captured: a recording must not
//!    re-record the arrangement.
//! 4. Capturing on the audio thread performs **zero heap allocations** (a full
//!    ring increments a counter instead of blocking or growing).
//!
//! How: the same counting-allocator flow as `metronome_rt_audit` — a global
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
use glitcherz_engine::realtime::capture::{CaptureTap, CapturedNote, capture_channel};
use glitcherz_engine::realtime::commands::{
    EngineCommand, NoteEvent, SequencerCommand, command_channel,
};
use glitcherz_engine::realtime::graph::{GraphCompiler, GraphUpdate};
use glitcherz_engine::realtime::sequencer::{
    RtMidiClip, RtNote, Sequencer, TICKS_PER_BAR, TICKS_PER_STEP, empty_midi_clip_table,
};
use rtrb::{Consumer, Producer};

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

/// Builds a runner around the default graph plus the control-side halves of
/// the capture ring.
fn make_runner() -> (
    BlockRunner,
    Producer<EngineCommand>,
    Producer<GraphUpdate>,
    Consumer<CapturedNote>,
) {
    let (tx, rx) = command_channel(64);
    let (graph_tx, graph_rx) = glitcherz_engine::realtime::voice::structure_channel();
    let (recycle_tx, recycle_rx) = glitcherz_engine::realtime::voice::recycle_channel();
    drop(recycle_rx);
    let (audio_trash_tx, audio_trash_rx) =
        glitcherz_engine::realtime::audio_track::audio_clip_trash_channel();
    drop(audio_trash_rx);
    let (midi_trash_tx, midi_trash_rx) =
        glitcherz_engine::realtime::sequencer::midi_clip_trash_channel();
    drop(midi_trash_rx);
    let (capture_tx, capture_rx) = capture_channel(64);
    let overflow = std::sync::Arc::new(AtomicU64::new(0));
    let (_compiler, graph) = GraphCompiler::default_graph(SR, BLOCK);
    let feedback = Feedback::new();
    let runner = BlockRunner::new(
        BLOCK,
        2,
        SR,
        graph,
        Sequencer::new(SR),
        rx,
        graph_rx,
        recycle_tx,
        audio_trash_tx,
        midi_trash_tx,
        CaptureTap::new(capture_tx, overflow),
        feedback,
    );
    (runner, tx, graph_tx, capture_rx)
}

/// Renders `frames` stereo frames (the buffer is sized by the caller).
fn render(runner: &mut BlockRunner, buffer: &mut [f32], frames: usize) {
    let mut rendered = 0;
    while rendered < frames {
        let n = BLOCK.min(frames - rendered);
        let slice = &mut buffer[rendered * 2..(rendered + n) * 2];
        runner.render_interleaved(slice);
        rendered += n;
    }
}

/// Drains everything the tap captured.
fn drain(capture: &mut Consumer<CapturedNote>) -> Vec<CapturedNote> {
    let mut out = Vec::new();
    while let Ok(captured) = capture.pop() {
        out.push(captured);
    }
    out
}

fn note_on(note: u8) -> EngineCommand {
    EngineCommand::Note {
        track: 0,
        event: NoteEvent::On {
            note,
            velocity: 1.0,
        },
    }
}

#[test]
fn captured_events_carry_the_transport_position() {
    let _guard = AUDIT_LOCK.lock().expect("audit lock");
    let (mut runner, mut tx, _graph_tx, mut capture) = make_runner();
    let mut buffer = vec![0.0f32; BLOCK * 2];

    tx.push(EngineCommand::SetCapture(true))
        .expect("queue not full");
    tx.push(EngineCommand::Sequencer(SequencerCommand::Seek(f64::from(
        TICKS_PER_BAR,
    ))))
    .expect("queue not full");
    tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
        .expect("queue not full");
    // Consume the setup commands at this block boundary; the transport is now
    // sitting on bar 1 and running.
    render(&mut runner, &mut buffer, BLOCK);

    tx.push(note_on(69)).expect("queue not full");
    render(&mut runner, &mut buffer, BLOCK);

    let captured = drain(&mut capture);
    assert_eq!(captured.len(), 1, "one live event, one capture");
    assert_eq!(
        captured[0].event,
        NoteEvent::On {
            note: 69,
            velocity: 1.0
        }
    );
    // Block-quantized, so the stamp is at most one block past the seek target
    // (128 frames at 120 BPM ≈ 2.6 ticks).
    let drift = (captured[0].ticks - f64::from(TICKS_PER_BAR)).abs();
    assert!(
        drift < 20.0,
        "stamp {} should be one block after tick {TICKS_PER_BAR} (drift {drift})",
        captured[0].ticks
    );
}

#[test]
fn capture_is_off_until_enabled() {
    let _guard = AUDIT_LOCK.lock().expect("audit lock");
    let (mut runner, mut tx, _graph_tx, mut capture) = make_runner();
    let mut buffer = vec![0.0f32; BLOCK * 2];

    tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
        .expect("queue not full");
    render(&mut runner, &mut buffer, BLOCK);
    tx.push(note_on(64)).expect("queue not full");
    render(&mut runner, &mut buffer, BLOCK);

    assert!(
        drain(&mut capture).is_empty(),
        "a session that never records must capture nothing"
    );
}

#[test]
fn sequencer_notes_are_not_captured() {
    let _guard = AUDIT_LOCK.lock().expect("audit lock");
    let (mut runner, mut tx, mut graph_tx, mut capture) = make_runner();

    // One bar of 16ths on track 0, so the sequencer definitely fires.
    let mut clip = RtMidiClip {
        span_ticks: TICKS_PER_BAR,
        length_ticks: TICKS_PER_BAR,
        ..RtMidiClip::default()
    };
    for step in 0..16 {
        clip.push(RtNote {
            start_ticks: step * TICKS_PER_STEP,
            length_ticks: TICKS_PER_STEP,
            note: 60 + (step as u8 % 4),
            channel: 0,
            velocity: 100,
        });
    }
    let mut clips = empty_midi_clip_table();
    clips[0] = clip;
    graph_tx
        .push(GraphUpdate::SetMidiClips { track: 0, clips })
        .expect("queue not full");

    tx.push(EngineCommand::SetCapture(true))
        .expect("queue not full");
    tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
        .expect("queue not full");

    // Slightly over one bar at the default 120 BPM (2 s per bar).
    let mut buffer = vec![0.0f32; SR as usize * 2];
    render(&mut runner, &mut buffer, SR as usize);

    assert!(
        drain(&mut capture).is_empty(),
        "the sequencer's own notes must never enter the capture ring"
    );
}

#[test]
fn capturing_allocates_nothing() {
    let _guard = AUDIT_LOCK.lock().expect("audit lock");
    let (mut runner, mut tx, _graph_tx, mut capture) = make_runner();
    let mut block = vec![0.0f32; BLOCK * 2];

    tx.push(EngineCommand::SetCapture(true))
        .expect("queue not full");
    tx.push(EngineCommand::Sequencer(SequencerCommand::Play))
        .expect("queue not full");
    // Warm up: commands consumed, transport running.
    render(&mut runner, &mut buffer_of(BLOCK * 4), BLOCK * 4);

    // Queue one live note so the capture path runs inside the audited region.
    tx.push(note_on(72)).expect("queue not full");
    let scope = AuditScope::begin();
    for _ in 0..(SR as usize / BLOCK) {
        runner.render_interleaved(&mut block);
    }
    let allocations = scope.end();
    assert_eq!(
        allocations, 0,
        "the capture path performed {allocations} heap allocations (expected 0)"
    );

    let captured = drain(&mut capture);
    assert_eq!(
        captured.len(),
        1,
        "the note pushed above should be captured"
    );
}

/// A throw-away stereo buffer (kept out of the audit scope by the caller).
fn buffer_of(frames: usize) -> Vec<f32> {
    vec![0.0f32; frames * 2]
}
