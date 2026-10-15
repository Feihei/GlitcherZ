//! Musical grid resolution (M2.1, from the SoundCraft survey).
//!
//! The roll's snap granularity is a [`GridValue`] instead of a hardcoded
//! tick constant. A note-value grid derives its tick width from the PPQ;
//! the wall-clock variants (Seconds / Frames / Samples) derive from the
//! transport at call time, so their tick width follows tempo changes.
//!
//! Grid *lines* are always re-anchored on every bar line (never counted
//! from tick 0 across the whole clip): under a non-4/4 meter a fixed
//! offset would drift out of alignment with the bar lines.

/// A musical note value with optional dot / triplet modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteValue {
    /// Divisions of a whole note: 4 = quarter, 8 = eighth, ... 64 = 1/64.
    pub division: u8,
    /// Dotted: one and a half times the base length.
    pub dotted: bool,
    /// Triplet: two thirds of the base length.
    pub triplet: bool,
}

impl NoteValue {
    /// Tick width at `ticks_per_quarter`.
    ///
    /// 960 PPQ divides every supported value exactly (a 1/64 triplet is 40
    /// ticks), which is why M1.1 chose it.
    pub fn ticks(&self, ticks_per_quarter: u32) -> i64 {
        let base = i64::from(ticks_per_quarter) * 4 / i64::from(self.division);
        match (self.dotted, self.triplet) {
            (true, true) => base * 3 / 2 * 2 / 3, // unusual but well-defined
            (true, false) => base * 3 / 2,
            (false, true) => base * 2 / 3,
            (false, false) => base,
        }
    }

    /// Human label ("1/8", "1/8.", "1/8T") for menus.
    pub fn label(&self) -> String {
        let dot = if self.dotted { "." } else { "" };
        let tri = if self.triplet { "T" } else { "" };
        format!("1/{}{dot}{tri}", self.division)
    }
}

/// Snap granularity of a grid-aligned editor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GridValue {
    /// A musical note value (the only variant the UI exposes today).
    Note(NoteValue),
    /// Fixed wall-clock seconds.
    Seconds(f64),
    /// Fixed frame count at `fps`.
    Frames(u32),
    /// Fixed sample count.
    Samples(u32),
}

impl GridValue {
    /// Tick width of one grid cell.
    ///
    /// Note values depend only on the PPQ; the wall-clock variants also need
    /// the transport (tempo / frame rate / sample rate) at the moment of the
    /// call, so their result follows tempo changes. Returns `None` for a
    /// degenerate (zero or negative) configuration.
    pub fn ticks(
        &self,
        ticks_per_quarter: u32,
        bpm: f64,
        sample_rate: f32,
        fps: f32,
    ) -> Option<i64> {
        // A quarter note lasts 60/bpm seconds and spans ppq ticks, so
        // seconds -> ticks is `t * bpm * ppq / 60` — sample-rate free.
        let seconds_to_ticks = |seconds: f64| {
            (seconds > 0.0 && bpm > 0.0)
                .then(|| (seconds * bpm * f64::from(ticks_per_quarter) / 60.0).round() as i64)
        };
        match *self {
            GridValue::Note(value) => {
                let ticks = value.ticks(ticks_per_quarter);
                (ticks > 0).then_some(ticks)
            }
            GridValue::Seconds(seconds) => seconds_to_ticks(seconds),
            GridValue::Frames(frames) => ((fps > 0.0)
                .then_some(f64::from(frames) / f64::from(fps)))
            .and_then(seconds_to_ticks),
            GridValue::Samples(samples) => ((sample_rate > 0.0)
                .then_some(f64::from(samples) / f64::from(sample_rate)))
            .and_then(seconds_to_ticks),
        }
    }

    /// Every note value the UI offers in its grid menu, in musical order.
    pub fn note_presets() -> [NoteValue; 12] {
        [
            NoteValue {
                division: 4,
                dotted: false,
                triplet: false,
            },
            NoteValue {
                division: 4,
                dotted: true,
                triplet: false,
            },
            NoteValue {
                division: 4,
                dotted: false,
                triplet: true,
            },
            NoteValue {
                division: 8,
                dotted: false,
                triplet: false,
            },
            NoteValue {
                division: 8,
                dotted: true,
                triplet: false,
            },
            NoteValue {
                division: 8,
                dotted: false,
                triplet: true,
            },
            NoteValue {
                division: 16,
                dotted: false,
                triplet: false,
            },
            NoteValue {
                division: 16,
                dotted: true,
                triplet: false,
            },
            NoteValue {
                division: 16,
                dotted: false,
                triplet: true,
            },
            NoteValue {
                division: 32,
                dotted: false,
                triplet: false,
            },
            NoteValue {
                division: 64,
                dotted: false,
                triplet: false,
            },
            NoteValue {
                division: 64,
                dotted: false,
                triplet: true,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::{GridValue, NoteValue};
    use glitcherz_model::TICKS_PER_QUARTER;

    fn nv(division: u8, dotted: bool, triplet: bool) -> NoteValue {
        NoteValue {
            division,
            dotted,
            triplet,
        }
    }

    #[test]
    fn note_values_divide_the_quarter_exactly() {
        assert_eq!(nv(4, false, false).ticks(TICKS_PER_QUARTER), 960);
        assert_eq!(nv(8, false, false).ticks(TICKS_PER_QUARTER), 480);
        assert_eq!(nv(16, false, false).ticks(TICKS_PER_QUARTER), 240);
        assert_eq!(nv(64, false, false).ticks(TICKS_PER_QUARTER), 60);
    }

    #[test]
    fn dotted_and_triplet_modify_exactly() {
        // Dotted quarter = 1.5 beats.
        assert_eq!(nv(4, true, false).ticks(TICKS_PER_QUARTER), 1440);
        // Quarter triplet = 2/3 beat.
        assert_eq!(nv(4, false, true).ticks(TICKS_PER_QUARTER), 640);
        // Eighth triplet = 1/3 beat — the value 96 PPQ could not express.
        assert_eq!(nv(8, false, true).ticks(TICKS_PER_QUARTER), 320);
        // 1/64 triplet: 40 ticks, exactly.
        assert_eq!(nv(64, false, true).ticks(TICKS_PER_QUARTER), 40);
    }

    #[test]
    fn every_preset_yields_a_positive_grid() {
        for value in GridValue::note_presets() {
            let ticks = GridValue::Note(value).ticks(TICKS_PER_QUARTER, 120.0, 48_000.0, 30.0);
            assert!(ticks.is_some_and(|t| t > 0), "{}", value.label());
        }
    }

    #[test]
    fn wall_clock_grids_follow_tempo() {
        // One second @ 120 BPM = 2 beats = 1920 ticks.
        let grid = GridValue::Seconds(1.0);
        assert_eq!(
            grid.ticks(TICKS_PER_QUARTER, 120.0, 48_000.0, 30.0),
            Some(1920)
        );
        // @ 60 BPM the same second is one beat = 960 ticks.
        assert_eq!(
            grid.ticks(TICKS_PER_QUARTER, 60.0, 48_000.0, 30.0),
            Some(960)
        );
        // Degenerate inputs refuse rather than return zero.
        assert_eq!(
            GridValue::Seconds(0.0).ticks(TICKS_PER_QUARTER, 120.0, 48_000.0, 30.0),
            None
        );
        assert_eq!(
            GridValue::Frames(30).ticks(TICKS_PER_QUARTER, 120.0, 48_000.0, 0.0),
            None
        );
        assert_eq!(
            GridValue::Samples(0).ticks(TICKS_PER_QUARTER, 120.0, 48_000.0, 30.0),
            None
        );
    }
}
