//! Atomic scalar feedback from the audio thread to the UI.

use std::sync::atomic::{AtomicU32, Ordering};

/// Atomic `f32`: stored as a bit pattern in an [`AtomicU32`], used for the
/// audio thread to feed floating-point scalars (peak levels etc.) back to
/// the UI.
///
/// All methods are **RT-safe** (lock-free, no allocation; `Relaxed` ordering
/// suffices — a single-value snapshot needs no synchronization of other
/// memory).
#[derive(Debug, Default)]
pub struct AtomicF32 {
    bits: AtomicU32,
}

impl AtomicF32 {
    /// Constructed on a non-real-time thread.
    pub fn new(value: f32) -> Self {
        Self {
            bits: AtomicU32::new(value.to_bits()),
        }
    }

    /// Writes. RT-safe.
    pub fn store(&self, value: f32) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }

    /// Reads. RT-safe.
    pub fn load(&self) -> f32 {
        f32::from_bits(self.bits.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::AtomicF32;

    #[test]
    fn roundtrip() {
        let a = AtomicF32::new(1.5);
        assert!((a.load() - 1.5).abs() < 1e-9);
        a.store(-0.25);
        assert!((a.load() - (-0.25)).abs() < 1e-9);
        a.store(0.0);
        assert_eq!(a.load(), 0.0);
    }
}
