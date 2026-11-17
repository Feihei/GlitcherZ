//! One snap value for every editing surface.
//!
//! Spec `.agents/spec/snap.md` (S1/S2) and `.agents/spec/snap-granularity.md`
//! (F-045). The five ad-hoc implementations this replaced (piano-roll grid,
//! timeline bar snap, ruler closure, curve band) all differ only in **which
//! grid** they target; they now share [`Snap`].
//!
//! Two families live here:
//!
//! * [`SnapGrid::Fixed`] — a plain tick width. The piano roll's note values and
//!   the curve editor's step are meter-independent, and neither has a
//!   [`MeterMap`] in scope, so they keep snapping numerically
//!   ([`Snap::ticks`]).
//! * [`SnapGrid::Bar`] / [`SnapGrid::Beat`] — **musical** grids whose width is
//!   read from the meter segment the pointer is in (F-045 D2/D4). Those need
//!   the map, so they go through [`Snap::at`] / [`Snap::width_at`].
//!
//! A zero grid is gone: [`Snap::OFF`] is the Ctrl override, and it is
//! numerically identical to `Fixed(1)` (F-045 §1.2) — the `off` flag exists so
//! the status bar can say "Snap off" and so the magnetic boundary pass can be
//! skipped, not because it snaps differently.

use glitcherz_model::MeterMap;

/// One step of the fine grid: a 1/16 note (a quarter is 960 ticks).
pub const TICKS_PER_STEP: i64 = 240;

/// The grid a drag targets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SnapGrid {
    /// Whole bars of the containing meter segment.
    Bar,
    /// Beats of the containing meter segment (4/4 = 960, 6/8 = 480).
    #[default]
    Beat,
    /// A fixed 1/16 grid, independent of the meter.
    Step,
    /// A fixed width in ticks (piano roll / curve legacy grids).
    Fixed(i64),
}

impl SnapGrid {
    /// What the timeline toolbar offers (F-045 D1). No `Off`, no tick: Ctrl is
    /// the tick override, and a menu entry aliasing it would be redundant.
    pub const CHOICES: [SnapGrid; 3] = [SnapGrid::Bar, SnapGrid::Beat, SnapGrid::Step];

    pub fn label(self) -> &'static str {
        match self {
            SnapGrid::Bar => "Bar",
            SnapGrid::Beat => "Beat",
            SnapGrid::Step => "Step",
            SnapGrid::Fixed(_) => "Fixed",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            SnapGrid::Bar => "Drags land on bar lines",
            SnapGrid::Beat => "Drags land on beats",
            SnapGrid::Step => "Drags land on 1/16 steps",
            SnapGrid::Fixed(_) => "Drags land on a fixed tick grid",
        }
    }

    /// Whether the width follows the meter (Bar / Beat) instead of a constant.
    pub fn is_musical(self) -> bool {
        matches!(self, SnapGrid::Bar | SnapGrid::Beat)
    }
}

/// Snap granularity: a grid kind plus the Ctrl override.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snap {
    grid: SnapGrid,
    off: bool,
}

impl Snap {
    /// The Ctrl override: nearest tick. Equivalent to `on(1)` numerically;
    /// `off` only drives the status bar and the magnetic-boundary bypass.
    pub const OFF: Snap = Snap {
        grid: SnapGrid::Fixed(1),
        off: true,
    };

    /// A fixed-width grid (piano roll, curve editor).
    pub fn on(grid: i64) -> Self {
        Snap {
            grid: SnapGrid::Fixed(grid.max(1)),
            off: false,
        }
    }

    /// A musical grid (arrangement timeline, F-045).
    pub fn musical(grid: SnapGrid) -> Self {
        Snap { grid, off: false }
    }

    pub fn kind(self) -> SnapGrid {
        self.grid
    }

    pub fn is_off(self) -> bool {
        self.off
    }

    /// The fixed cell width in ticks; `1` for a musical grid and for `OFF`
    /// (both need [`Snap::width_at`] / the tick path instead).
    pub fn grid(self) -> i64 {
        match self.grid {
            SnapGrid::Fixed(width) => width.max(1),
            SnapGrid::Step => TICKS_PER_STEP,
            SnapGrid::Bar | SnapGrid::Beat => 1,
        }
    }

    /// One cell's width at absolute tick `at`. Bar / Beat resolve their width
    /// from the segment containing `at`, so a drag that crosses a meter change
    /// switches basis at the boundary (F-045 D4) instead of drifting.
    pub fn width_at(self, meter: &MeterMap, at: u32) -> i64 {
        match self.grid {
            SnapGrid::Bar => i64::from(meter.ticks_per_bar_at(at)).max(1),
            SnapGrid::Beat => i64::from(meter.ticks_per_beat_at(at)).max(1),
            SnapGrid::Step => TICKS_PER_STEP,
            SnapGrid::Fixed(width) => width.max(1),
        }
    }

    /// Snaps **absolute** ticks to the grid, never negative.
    ///
    /// Bar / Beat round inside the pointer's own segment and clamp a round-up
    /// at a segment end to the next segment's first line — the same contract
    /// as [`MeterMap::snap_bar`].
    pub fn at(self, meter: &MeterMap, raw: f32) -> u32 {
        if self.off {
            return raw.round().max(0.0) as u32;
        }
        match self.grid {
            SnapGrid::Bar => meter.snap_bar(f64::from(raw.max(0.0))),
            SnapGrid::Beat => meter.snap_beat(f64::from(raw.max(0.0))),
            SnapGrid::Step | SnapGrid::Fixed(_) => self.ticks(raw) as u32,
        }
    }

    /// Nearest grid line in ticks, never negative.
    ///
    /// Fixed grids only: a musical grid has no width without a [`MeterMap`]
    /// and rounds to the nearest tick here. Use [`Snap::at`] on the timeline.
    pub fn ticks(self, raw: f32) -> i64 {
        if self.off {
            return raw.round().max(0.0) as i64;
        }
        let grid = self.grid();
        ((raw / grid as f32).round() as i64).max(0) * grid
    }

    /// [`Snap::ticks`] in the `f64` paths (ruler / loop / curve axes).
    pub fn ticks_f64(self, raw: f64) -> f64 {
        if self.off {
            raw.round().max(0.0)
        } else {
            ((raw / self.grid() as f64).round() * self.grid() as f64).max(0.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glitcherz_model::MeterMap;

    fn meter() -> MeterMap {
        MeterMap::new(glitcherz_model::TimeSignature::COMMON, &[])
    }

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

    #[test]
    fn off_and_fixed_one_agree_on_every_position() {
        // F-045 §1.2: Off is Fixed(1) plus a status-bar flag, nothing more.
        let one = Snap::on(1);
        for raw in [0.0f32, 0.4, 0.6, 100.4, 959.5, 12_345.9] {
            assert_eq!(Snap::OFF.ticks(raw), one.ticks(raw));
            assert_eq!(
                Snap::OFF.ticks_f64(f64::from(raw)),
                one.ticks_f64(f64::from(raw))
            );
        }
    }

    #[test]
    fn musical_grids_read_their_width_from_the_meter() {
        let map = meter(); // 4/4: 3840 per bar, 960 per beat
        assert_eq!(Snap::musical(SnapGrid::Bar).width_at(&map, 0), 3840);
        assert_eq!(Snap::musical(SnapGrid::Beat).width_at(&map, 0), 960);
        assert_eq!(Snap::musical(SnapGrid::Step).width_at(&map, 0), 240);
        assert_eq!(Snap::on(120).width_at(&map, 0), 120);
    }

    #[test]
    fn beat_snapping_lands_on_beats_not_bars() {
        let map = meter();
        let snap = Snap::musical(SnapGrid::Beat);
        assert_eq!(snap.at(&map, 0.0), 0);
        assert_eq!(snap.at(&map, 500.0), 960);
        assert_eq!(snap.at(&map, 1400.0), 960);
        assert_eq!(snap.at(&map, 1500.0), 1920);
        assert_eq!(snap.at(&map, 3839.0), 3840);
    }

    #[test]
    fn step_snapping_is_meter_independent() {
        let map = meter();
        let snap = Snap::musical(SnapGrid::Step);
        assert_eq!(snap.at(&map, 100.0), 0);
        assert_eq!(snap.at(&map, 130.0), 240);
        assert_eq!(snap.at(&map, -5.0), 0);
    }
}
