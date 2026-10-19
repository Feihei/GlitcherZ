//! # glitcherz-host_gate
//!
//! Plugin-host infrastructure shared by the CLAP and VST3 hosts: the atomic
//! handoff gate that serializes the control thread's main-thread plugin calls
//! (state load/save, parameter read) against the audio thread's `process` on
//! the same plugin instance.
//!
//! Why: plugins keep mutable engine state across calls and match note-offs by
//! pitch; a main-thread state load racing an in-flight `process` violates the
//! host-API threading contract (clack's guard is main-thread-only; VST3's
//! getState/setState are control-thread calls) and can tear the plugin's
//! engine state. The gate makes the control thread wait until the audio
//! thread has parked in a gated `process` before touching the plugin.
//!
//! Audio-side cost: two atomic loads per process block (no locks, no
//! allocation) — RT-safe.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Atomic handoff gate between the control thread's main-thread plugin calls
/// and the audio thread's `process` on the same plugin instance. Shared via
/// `Arc` between the instance's guard (control side) and node (audio side).
#[derive(Default)]
pub struct StateGate {
    /// Set by the control thread while a main-thread plugin call is in
    /// flight; the audio thread skips processing while it is set.
    loading: AtomicBool,
    /// Acknowledgement: the audio thread sets this inside a `process` that
    /// observed `loading` — once observed, no `process` is in flight.
    suspended: AtomicBool,
    /// The audio thread has processed the node at least once (a load before
    /// that point needs no handoff — the node is not in the graph yet).
    started: AtomicBool,
}

impl StateGate {
    /// Upper bound for waiting on the audio-thread acknowledgement (a full
    /// process block is ~2.7 ms at 48 kHz / 128 frames; the bound only
    /// matters when the node was recycled and never acknowledges).
    const HANDOFF_TIMEOUT: Duration = Duration::from_millis(50);

    /// Audio-side: marks the node as started (the node is being processed)
    /// and returns whether `process` should park — the control thread has a
    /// main-thread plugin call in flight. When true, the caller
    /// acknowledges (done here), keeps the output silent, and returns.
    pub fn audio_should_park(&self) -> bool {
        self.started.store(true, Ordering::Release);
        if self.loading.load(Ordering::Acquire) {
            self.suspended.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Blocks until the audio thread has parked in a gated `process` (or the
    /// bounded timeout passes). Only waits when the node is actually being
    /// processed; a spawn-time load happens before the node enters the graph.
    /// Control-thread only (sleeps while waiting).
    pub fn begin_host_call(&self) {
        if !self.started.load(Ordering::Acquire) {
            return;
        }
        self.suspended.store(false, Ordering::Release);
        self.loading.store(true, Ordering::Release);
        let deadline = Instant::now() + Self::HANDOFF_TIMEOUT;
        while !self.suspended.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                return; // degraded: proceed anyway (node likely recycled)
            }
            std::thread::sleep(Duration::from_micros(250));
        }
    }

    /// Ends a main-thread plugin call: clears `loading` first (the audio
    /// thread resumes), then the acknowledgement.
    pub fn end_host_call(&self) {
        self.loading.store(false, Ordering::Release);
        self.suspended.store(false, Ordering::Release);
    }
}
