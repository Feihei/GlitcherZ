//! Audio → UI meters for built-in effects (spec `.agents/spec/effects.md` E2b).
//!
//! A processor owns an `Option<Arc<EffectMeters>>` handed to it at construction
//! time (the control thread keeps the other `Arc` in a `node_id → meters` map).
//! There is deliberately **no dynamic registry**: the handle lives exactly as
//! long as the node, so a recycled node id can never pick up a stale meter.
//!
//! Writes happen once per block ([`AtomicF32::store`] is `Relaxed`), reads from
//! the UI thread are plain loads — a torn or one-block-stale reading is
//! harmless for a meter.

use crate::realtime::atomic::AtomicF32;

/// Gain reduction and output peaks of one effect instance.
#[derive(Debug, Default)]
pub struct EffectMeters {
    /// Most negative gain reduction of the last block (dB, `0` = none).
    gr_db: AtomicF32,
    /// Output peak of the last block, left channel.
    peak_l: AtomicF32,
    /// Output peak of the last block, right channel.
    peak_r: AtomicF32,
}

/// Snapshot of an [`EffectMeters`], read on the UI thread.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterReading {
    /// Gain reduction in dB, `<= 0`.
    pub gr_db: f32,
    /// Left output peak (`0..`).
    pub peak_l: f32,
    /// Right output peak (`0..`).
    pub peak_r: f32,
}

impl EffectMeters {
    /// Construction time only (the UI keeps this `Arc`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes one block's figures. RT-safe.
    pub fn store(&self, gr_db: f32, peak_l: f32, peak_r: f32) {
        self.gr_db.store(gr_db);
        self.peak_l.store(peak_l);
        self.peak_r.store(peak_r);
    }

    /// Reads the latest snapshot. RT-safe (called from the UI thread).
    pub fn read(&self) -> MeterReading {
        MeterReading {
            gr_db: self.gr_db.load(),
            peak_l: self.peak_l.load(),
            peak_r: self.peak_r.load(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EffectMeters;

    #[test]
    fn stores_and_reads_a_snapshot() {
        let meters = EffectMeters::new();
        assert_eq!(meters.read(), super::MeterReading::default());
        meters.store(-6.5, 0.25, 0.5);
        let reading = meters.read();
        assert!((reading.gr_db + 6.5).abs() < 1e-6);
        assert!((reading.peak_l - 0.25).abs() < 1e-6);
        assert!((reading.peak_r - 0.5).abs() < 1e-6);
    }
}
