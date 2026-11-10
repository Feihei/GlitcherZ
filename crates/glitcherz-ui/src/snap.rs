//! One snap value for every editing surface.
//!
//! Spec `.agents/spec/snap.md` (S1/S2). The five ad-hoc implementations this
//! replaces (piano-roll grid, timeline bar snap, ruler closure, curve band)
//! all differ only in **which grid** they target; they now share [`Snap`], and
//! a zero grid means off: positions still round to the nearest tick so the tick
//! domain stays integral, but no grid line is targeted.

/// Snap granularity in ticks. `grid <= 0` means off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snap {
    grid: i64,
}

impl Snap {
    /// No snapping: nearest tick only.
    pub const OFF: Snap = Snap { grid: 0 };

    pub fn on(grid: i64) -> Self {
        Snap { grid: grid.max(1) }
    }

    pub fn is_off(self) -> bool {
        self.grid <= 0
    }

    /// The grid width in ticks; `1` when off (for minimum-length clamps).
    pub fn grid(self) -> i64 {
        self.grid.max(1)
    }

    /// Nearest grid line in ticks, never negative.
    pub fn ticks(self, raw: f32) -> i64 {
        if self.is_off() {
            raw.round().max(0.0) as i64
        } else {
            ((raw / self.grid as f32).round() as i64).max(0) * self.grid
        }
    }

    /// [`Snap::ticks`] in the `f64` paths (ruler / loop / curve axes).
    pub fn ticks_f64(self, raw: f64) -> f64 {
        if self.is_off() {
            raw.round().max(0.0)
        } else {
            ((raw / self.grid as f64).round() * self.grid as f64).max(0.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_rounds_to_the_nearest_grid_line() {
        let snap = Snap::on(240);
        assert_eq!(snap.ticks(0.0), 0);
        assert_eq!(snap.ticks(100.0), 0);
        assert_eq!(snap.ticks(120.0), 240); // exactly half rounds away from zero
        assert_eq!(snap.ticks(359.0), 240);
        assert_eq!(snap.ticks(360.0), 480);
        assert_eq!(snap.ticks(-50.0), 0); // negative positions clamp to 0
    }

    #[test]
    fn off_rounds_to_the_nearest_tick() {
        assert_eq!(Snap::OFF.ticks(100.4), 100);
        assert_eq!(Snap::OFF.ticks(100.6), 101);
        assert_eq!(Snap::OFF.ticks(-3.0), 0);
        assert_eq!(Snap::OFF.ticks_f64(119.7), 120.0);
        assert_eq!(Snap::OFF.ticks_f64(-0.2), 0.0);
        assert!(Snap::OFF.is_off());
        assert_eq!(Snap::OFF.grid(), 1);
    }

    #[test]
    fn on_f64_matches_the_f32_path() {
        let snap = Snap::on(3840);
        assert_eq!(snap.ticks_f64(5759.0), 3840.0);
        assert_eq!(snap.ticks_f64(5761.0), 7680.0);
        assert_eq!(snap.ticks_f64(-100.0), 0.0);
    }
}
