//! Capture tap (spec `.agents/spec/recording.md` D6/D7): the real-time thread's
//! only recording responsibility.
//!
//! The engine deliberately **does not know what recording is**. It offers one
//! narrow service instead: every live input event that reaches the command
//! queue is stamped with the transport position at the block boundary and
//! pushed back to the control thread. Everything else — pre-roll filtering,
//! note pairing, take assembly, undo — stays in the UI, which is also why the
//! tap is reusable for MIDI-learn and input monitoring later.
//!
//! Like every other cross-thread channel in this module it is an SPSC ring:
//! the audio thread only ever pushes (and only while enabled), the control
//! thread only ever drains.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rtrb::{Consumer, Producer, RingBuffer};

use crate::realtime::commands::NoteEvent;

/// Default capacity of the capture ring.
///
/// Sized for a dense take rather than a burst: one bar of 16ths on four
/// octaves is still well under a hundred events, so 8192 leaves room for
/// several seconds of heavy playing even when the UI stalls for a moment.
/// A full ring drops the event and bumps the overflow counter (never blocks,
/// never allocates).
pub const CAPTURE_QUEUE_CAPACITY: usize = 8192;

/// One captured live event with the transport position it landed on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapturedNote {
    /// Transport position in ticks at the block boundary the event was
    /// consumed (block-quantized, like every other event in the engine).
    pub ticks: f64,
    /// The event itself.
    pub event: NoteEvent,
}

/// Creates the capture ring. **Not RT-safe** (allocates); call before starting
/// the stream.
pub fn capture_channel(capacity: usize) -> (Producer<CapturedNote>, Consumer<CapturedNote>) {
    RingBuffer::new(capacity)
}

/// The real-time half of the capture tap, owned by
/// [`BlockRunner`](crate::realtime::BlockRunner).
///
/// Disabled by default: while off, `note` returns immediately, so a session
/// that never records pays one branch per live event and nothing else.
pub struct CaptureTap {
    tx: Producer<CapturedNote>,
    enabled: bool,
    /// Published drop count (shared with the control thread). Written only by
    /// this thread, read with `Relaxed` — a torn read is impossible for a
    /// `u64` and a stale value only delays the status-bar warning one frame.
    overflow: Arc<AtomicU64>,
    /// Local mirror of the counter. Kept local so a normal capture costs no
    /// atomic operation at all; the atomic is only stored when it changes.
    dropped: u64,
}

impl CaptureTap {
    /// Constructs. **Not RT-safe** (takes ownership of a pre-built producer).
    pub fn new(tx: Producer<CapturedNote>, overflow: Arc<AtomicU64>) -> Self {
        Self {
            tx,
            enabled: false,
            overflow,
            dropped: 0,
        }
    }

    /// Enables or disables the tap. Disabling does **not** clear the ring: the
    /// UI drains whatever was captured before the switch and decides what to
    /// keep. **RT-safe**.
    pub fn configure(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Whether the tap is currently enabled. **RT-safe**.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// A tap nobody reads: captured events land in a ring whose consumer was
    /// dropped, so they are silently lost.
    ///
    /// For harnesses that build a [`BlockRunner`](crate::realtime::BlockRunner)
    /// without exercising recording (RT audits, the CLAP host's offline
    /// runner), which would otherwise have to spell out a channel.
    /// **Not RT-safe** (allocates the ring).
    pub fn detached() -> Self {
        let (tx, rx) = capture_channel(CAPTURE_QUEUE_CAPACITY);
        drop(rx);
        Self::new(tx, Arc::new(AtomicU64::new(0)))
    }

    /// Stamps and queues one live event. **RT-safe**: no allocation, no
    /// logging, no panic — a full ring drops the event and counts it.
    pub fn note(&mut self, ticks: f64, event: NoteEvent) {
        if !self.enabled {
            return;
        }
        if self.tx.push(CapturedNote { ticks, event }).is_err() {
            self.dropped = self.dropped.saturating_add(1);
            self.overflow.store(self.dropped, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CaptureTap, capture_channel};
    use crate::realtime::commands::NoteEvent;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tap(
        capacity: usize,
    ) -> (
        CaptureTap,
        rtrb::Consumer<super::CapturedNote>,
        Arc<AtomicU64>,
    ) {
        let (tx, rx) = capture_channel(capacity);
        let overflow = Arc::new(AtomicU64::new(0));
        (CaptureTap::new(tx, overflow.clone()), rx, overflow)
    }

    fn on(note: u8) -> NoteEvent {
        NoteEvent::On {
            note,
            velocity: 1.0,
        }
    }

    #[test]
    fn disabled_tap_drops_everything_without_counting() {
        let (mut tap, mut rx, overflow) = tap(8);
        tap.note(0.0, on(60));
        assert!(rx.pop().is_err());
        assert_eq!(overflow.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn enabled_tap_stamps_the_position() {
        let (mut tap, mut rx, _) = tap(8);
        tap.configure(true);
        tap.note(960.0, on(64));
        let captured = rx.pop().expect("event should be queued");
        assert_eq!(captured.ticks, 960.0);
        assert_eq!(captured.event, on(64));
    }

    #[test]
    fn full_ring_counts_drops_instead_of_blocking() {
        // One usable slot: the second event must not block or panic.
        let (mut tap, mut rx, overflow) = tap(1);
        tap.configure(true);
        tap.note(0.0, on(60));
        tap.note(1.0, on(61));
        tap.note(2.0, on(62));
        assert_eq!(overflow.load(Ordering::Relaxed), 2);
        assert_eq!(rx.pop().map(|c| c.ticks), Ok(0.0));
        // The counter is cumulative, so draining does not reset it.
        tap.note(3.0, on(63));
        assert_eq!(overflow.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn disabling_keeps_what_was_already_captured() {
        let (mut tap, mut rx, _) = tap(8);
        tap.configure(true);
        tap.note(0.0, on(60));
        tap.configure(false);
        tap.note(1.0, on(61));
        assert_eq!(rx.pop().map(|c| c.event), Ok(on(60)));
        assert!(rx.pop().is_err());
    }
}
