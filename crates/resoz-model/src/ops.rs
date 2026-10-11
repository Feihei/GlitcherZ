//! Event operations (M3, from the SoundCraft survey): quantize, velocity and
//! duration transforms over a slice of notes.
//!
//! Contract (AGENTS.md "MIDI ops 纯函数化"):
//! - every function is `fn(&mut [Note], Options)` — in place, **never changes
//!   the slice length**, so it can be applied to a selection;
//! - pitches that leave 0..=127 are clamped to the boundary, never dropped
//!   (a transpose must not silently delete notes);
//! - functions do not re-sort; the caller restores the sequence invariant;
//! - randomness is a seeded XorShift, so every operation is reproducible and
//!   unit-testable.

use crate::Note;

/// Default seed for the deterministic RNG (`0x5EED_50DC_4AF7_0001`).
pub const DEFAULT_SEED: u64 = 0x5EED_50DC_4AF7_0001;

/// Minimal XorShift64* — deterministic, no dependencies.
struct XorShift(u64);

impl XorShift {
    /// Uniform `0.0..1.0` (top 24 bits of the * product, scaled by 2^24).
    fn next_f32(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let product = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((product >> 40) as f32) / 16_777_216.0 // 24 bits → [0, 1)
    }
}

/// `f32` → nearest `i64` (half away from zero is fine for grid math).
fn round_i64(value: f32) -> i64 {
    value.round() as i64
}

/// Applies a slice-based op to only the notes at `indices` (the selection
/// form every command uses).
///
/// The selected notes are copied out, transformed, and written back: the op
/// keeps its clean `&mut [Note]` signature, and indices in `indices` beyond
/// the slice are ignored. Ramp / Legato orderings are computed across the
/// selection only, which is the intended "operate on the selection" semantic.
pub fn apply_to_selected(notes: &mut [Note], indices: &[usize], op: impl FnOnce(&mut [Note])) {
    if indices.is_empty() {
        return;
    }
    let mut targets = Vec::with_capacity(indices.len());
    let mut chosen = Vec::with_capacity(indices.len());
    for (index, note) in notes.iter().enumerate() {
        if indices.contains(&index) {
            targets.push(index);
            chosen.push(*note);
        }
    }
    let before = chosen.len();
    op(&mut chosen);
    chosen.truncate(before); // ops promise not to change the length; enforce it
    for (target, updated) in targets.into_iter().zip(chosen) {
        notes[target] = updated;
    }
}

// ---- Transpose ---------------------------------------------------------------------------

/// Transposes by `semitones`, clamping to 0..=127 without dropping notes.
pub fn transpose(notes: &mut [Note], semitones: i32) {
    for note in notes {
        note.note = (i32::from(note.note) + semitones).clamp(0, 127) as u8;
    }
}

// ---- Quantize ----------------------------------------------------------------------------

/// Quantize parameters (8 knobs, Pro Tools "Event Operations / Quantize").
#[derive(Debug, Clone, PartialEq)]
pub struct QuantizeOptions {
    /// Grid spacing (tick).
    pub grid_ticks: i64,
    /// 0..=1 — how far a note travels toward its target line (Pro Tools' Q).
    pub strength: f32,
    /// 0..=1 — delay every other line by `swing * grid / 3`; 1.0 feels
    /// triplet-like.
    pub swing: f32,
    /// Only notes closer than this fraction of a half-grid to a line are
    /// quantized at all.
    pub include_within: f32,
    /// Notes already closer than this fraction of a half-grid are left alone
    /// (protects an already-tight performance; the most useful knob).
    pub exclude_within: f32,
    /// Snap note ends to lines too.
    pub quantize_ends: bool,
    /// Keep the original length (end anchored); otherwise the end moves with
    /// the start.
    pub preserve_duration: bool,
    /// 0..=1 — random offset after quantizing, up to half a grid, so the
    /// result does not feel machine-exact.
    pub random: f32,
}

impl QuantizeOptions {
    /// Straight 16th-note quantize: full strength, no swing, no tolerance.
    pub fn sixteenths(grid_ticks: i64) -> Self {
        Self {
            grid_ticks,
            strength: 1.0,
            swing: 0.0,
            include_within: 1.0,
            exclude_within: 0.0,
            quantize_ends: false,
            preserve_duration: false,
            random: 0.0,
        }
    }
}

impl Default for QuantizeOptions {
    fn default() -> Self {
        Self::sixteenths(crate::TICKS_PER_STEP as i64)
    }
}

/// Quantizes starts (and optionally ends) in place.
pub fn quantize(notes: &mut [Note], options: &QuantizeOptions) {
    quantize_with_seed(notes, options, DEFAULT_SEED);
}

/// [`quantize`] with an explicit RNG seed (tests, replay).
pub fn quantize_with_seed(notes: &mut [Note], options: &QuantizeOptions, seed: u64) {
    let grid = options.grid_ticks.max(1);
    let mut rng = XorShift(seed);
    for note in notes {
        let line = nearest_line(note.start, grid, options.swing);
        let distance = (note.start - line).abs() as f32;
        // Tolerances are fractions of a half grid.
        let half = grid as f32 / 2.0;
        if distance / half > options.include_within || distance / half < options.exclude_within {
            continue;
        }
        let moved = round_i64(note.start as f32 + (line - note.start) as f32 * options.strength);
        let jitter = if options.random > 0.0 {
            round_i64((rng.next_f32() * 2.0 - 1.0) * options.random * grid as f32 / 2.0)
        } else {
            0
        };
        let new_start = (moved + jitter).max(0);
        if options.preserve_duration {
            let end = note.start + note.length;
            note.start = new_start;
            note.length = (end - note.start).max(1);
        } else {
            note.start = new_start;
            if options.quantize_ends {
                let end_line = nearest_line(note.start + note.length, grid, 0.0);
                note.length = (end_line - note.start).max(1);
            }
        }
    }
}

/// Nearest grid line to `t`, honoring swing: odd-numbered lines are delayed
/// by `swing * grid / 3`, and because of that delay the nearest line may not
/// be the arithmetically nearest one — so search the three candidates.
fn nearest_line(t: i64, grid: i64, swing: f32) -> i64 {
    let swing_off = round_i64(swing.clamp(0.0, 1.0) * grid as f32 / 3.0);
    let k = (t + grid / 2).div_euclid(grid);
    let mut best = k * grid;
    let mut best_distance = i64::MAX;
    for kk in k - 1..=k + 1 {
        let line = kk * grid + if kk.rem_euclid(2) == 1 { swing_off } else { 0 };
        let distance = (t - line).abs();
        if distance < best_distance {
            best_distance = distance;
            best = line;
        }
    }
    best.max(0)
}

// ---- Velocity ----------------------------------------------------------------------------

/// Velocity transforms (Pro Tools "Event Operations / Velocity").
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VelocityOp {
    Set(u8),
    Add(i32),
    Scale(f32),
    /// Clamp into a window.
    Limit {
        min: u8,
        max: u8,
    },
    /// Linear crescendo from `from` to `to`, ordered by start time.
    Ramp {
        from: u8,
        to: u8,
    },
    /// Random ±`amount`.
    Humanize(u8),
}

/// Applies the transform in place. `seed` drives [`VelocityOp::Humanize`];
/// pass [`DEFAULT_SEED`] outside of tests.
pub fn apply_velocity(notes: &mut [Note], op: &VelocityOp, seed: u64) {
    match *op {
        VelocityOp::Set(value) => {
            for note in notes {
                note.velocity = value.clamp(1, 127);
            }
        }
        VelocityOp::Add(delta) => {
            for note in notes {
                note.velocity = (i32::from(note.velocity) + delta).clamp(1, 127) as u8;
            }
        }
        VelocityOp::Scale(factor) => {
            for note in notes {
                if factor.is_finite() {
                    note.velocity = (f32::from(note.velocity) * factor)
                        .round()
                        .clamp(1.0, 127.0) as u8;
                }
            }
        }
        VelocityOp::Limit { min, max } => {
            let (min, max) = (min.clamp(1, 127), max.clamp(1, 127).max(min));
            for note in notes {
                note.velocity = note.velocity.clamp(min, max);
            }
        }
        VelocityOp::Ramp { from, to } => {
            // Order by start time; ties keep their relative order (stable).
            let mut order: Vec<usize> = (0..notes.len()).collect();
            order.sort_by_key(|i| notes[*i].start);
            let last = order.len().saturating_sub(1);
            for (position, index) in order.into_iter().enumerate() {
                let t = if last == 0 {
                    f32::from(from)
                } else {
                    position as f32 / last as f32
                };
                notes[index].velocity = (f32::from(from) + (f32::from(to) - f32::from(from)) * t)
                    .round()
                    .clamp(1.0, 127.0) as u8;
            }
        }
        VelocityOp::Humanize(amount) => {
            let mut rng = XorShift(seed);
            for note in notes {
                let jitter = (rng.next_f32() * 2.0 - 1.0) * f32::from(amount);
                note.velocity = (f32::from(note.velocity) + jitter)
                    .round()
                    .clamp(1.0, 127.0) as u8;
            }
        }
    }
}

// ---- Duration ----------------------------------------------------------------------------

/// Duration transforms (Pro Tools "Event Operations / Duration").
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DurationOp {
    Set(i64),
    Add(i64),
    Scale(f32),
    /// Extend each note to `gap` before the next note starts (by time order
    /// within the slice). The last note keeps its length.
    Legato {
        gap: i64,
    },
    /// Truncate a note where the next note of the same pitch starts —
    /// de-clusters a melody line into a single voice.
    RemoveOverlap,
}

/// Applies the transform in place.
pub fn apply_duration(notes: &mut [Note], op: &DurationOp) {
    match *op {
        DurationOp::Set(value) => {
            for note in notes {
                note.length = value.max(1);
            }
        }
        DurationOp::Add(delta) => {
            for note in notes {
                note.length = (note.length + delta).max(1);
            }
        }
        DurationOp::Scale(factor) => {
            for note in notes {
                if factor.is_finite() {
                    note.length = ((note.length as f32 * factor).round() as i64).max(1);
                }
            }
        }
        DurationOp::Legato { gap } => {
            let mut order: Vec<usize> = (0..notes.len()).collect();
            order.sort_by_key(|i| notes[*i].start);
            for window in order.windows(2) {
                let (current, next) = (window[0], window[1]);
                let target = (notes[next].start - gap - notes[current].start).max(1);
                notes[current].length = notes[current].length.max(target);
            }
        }
        DurationOp::RemoveOverlap => {
            let mut order: Vec<usize> = (0..notes.len()).collect();
            order.sort_by_key(|i| (notes[*i].start, i32::from(notes[*i].note)));
            for window in order.windows(2) {
                let (current, next) = (window[0], window[1]);
                if notes[current].note == notes[next].note
                    && notes[next].start < notes[current].start + notes[current].length
                {
                    notes[current].length = (notes[next].start - notes[current].start).max(1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_SEED, DurationOp, QuantizeOptions, VelocityOp, apply_duration, apply_to_selected,
        apply_velocity, quantize, round_i64, transpose,
    };
    use crate::Note;

    fn note(start: i64, length: i64, pitch: u8, velocity: u8) -> Note {
        Note {
            start,
            length,
            note: pitch,
            velocity,
            channel: 0,
        }
    }

    fn pitches(notes: &[Note]) -> Vec<u8> {
        notes.iter().map(|n| n.note).collect()
    }

    #[test]
    fn transpose_clamps_instead_of_dropping() {
        let mut notes = vec![note(0, 96, 2, 100), note(96, 96, 125, 100)];
        transpose(&mut notes, 10);
        assert_eq!(pitches(&notes), vec![12, 127], "clamped at both ends");
        transpose(&mut notes, -30);
        assert_eq!(pitches(&notes), vec![0, 97]);
        assert_eq!(notes.len(), 2, "no note may be dropped");
    }

    #[test]
    fn quantize_pulls_notes_to_lines() {
        let mut notes = vec![note(17, 96, 60, 100), note(250, 96, 62, 100)];
        let options = QuantizeOptions::sixteenths(240);
        quantize(&mut notes, &options);
        assert_eq!(notes[0].start, 0);
        assert_eq!(notes[1].start, 240);
        assert_eq!(
            notes[0].length, 96,
            "duration moves with the start by default"
        );
    }

    #[test]
    fn exclude_within_protects_accurate_notes() {
        let mut notes = vec![note(3, 96, 60, 100), note(200, 96, 62, 100)];
        let options = QuantizeOptions {
            exclude_within: 0.1, // within 5% of a grid... of a half grid = 12 ticks
            ..QuantizeOptions::sixteenths(240)
        };
        quantize(&mut notes, &options);
        assert_eq!(notes[0].start, 3, "3 ticks from line 0: protected");
        assert_eq!(notes[1].start, 240, "40 ticks off: quantized");
    }

    #[test]
    fn strength_interpolates_toward_the_line() {
        let mut notes = vec![note(120, 96, 60, 100)];
        let options = QuantizeOptions {
            strength: 0.5,
            ..QuantizeOptions::sixteenths(240)
        };
        quantize(&mut notes, &options);
        assert_eq!(notes[0].start, 60, "halfway to the line at 0");
    }

    #[test]
    fn swing_searches_three_candidate_lines() {
        // Grid 240, swing 1.0 → odd lines delayed by 80. A note at 300 sits
        // between line k=1 (240+80=320) and k=2 (480): the swing-delayed
        // line 320 is nearer than the arithmetically-nearest 240+120=360?
        // 300 is 20 from 320 — nearest_line must find it, not 240.
        let mut notes = vec![note(300, 96, 60, 100)];
        let options = QuantizeOptions {
            swing: 1.0,
            ..QuantizeOptions::sixteenths(240)
        };
        quantize(&mut notes, &options);
        assert_eq!(notes[0].start, 320, "landed on the swing-delayed line");
    }

    #[test]
    fn preserve_duration_anchors_the_end() {
        let mut notes = vec![note(300, 60, 60, 100)];
        let options = QuantizeOptions {
            preserve_duration: true,
            ..QuantizeOptions::sixteenths(240)
        };
        quantize(&mut notes, &options);
        assert_eq!(notes[0].start, 240);
        assert_eq!(notes[0].length, 120, "end stayed at 360");
    }

    #[test]
    fn quantize_random_is_seeded_and_bounded() {
        // 300 is unambiguously nearest to line 240 (the exact half-grid 120
        // would be a tie, and either winner is legal).
        let make = || -> Vec<Note> { vec![note(300, 96, 60, 100)] };
        let options = QuantizeOptions {
            random: 0.5,
            ..QuantizeOptions::sixteenths(240)
        };
        let mut a = make();
        let mut b = make();
        quantize(&mut a, &options);
        quantize(&mut b, &options);
        assert_eq!(a[0].start, b[0].start, "same default seed, same result");
        assert!(
            (a[0].start - 240).abs() <= 60,
            "jitter bounded by half a grid"
        );
    }

    #[test]
    fn velocity_ops_cover_the_menu() {
        let mut notes = vec![note(0, 96, 60, 100), note(240, 96, 62, 50)];
        apply_velocity(&mut notes, &VelocityOp::Set(70), DEFAULT_SEED);
        assert_eq!(
            notes.iter().map(|n| n.velocity).collect::<Vec<_>>(),
            vec![70, 70]
        );
        apply_velocity(&mut notes, &VelocityOp::Add(-30), DEFAULT_SEED);
        assert_eq!(notes[0].velocity, 40);
        apply_velocity(&mut notes, &VelocityOp::Scale(2.0), DEFAULT_SEED);
        assert_eq!(notes[0].velocity, 80);
        apply_velocity(&mut notes, &VelocityOp::Add(60), DEFAULT_SEED);
        assert_eq!(
            notes.iter().map(|n| n.velocity).collect::<Vec<_>>(),
            vec![127, 127],
            "clamped at the top"
        );
        apply_velocity(
            &mut notes,
            &VelocityOp::Limit { min: 60, max: 90 },
            DEFAULT_SEED,
        );
        assert_eq!(
            notes.iter().map(|n| n.velocity).collect::<Vec<_>>(),
            vec![90, 90],
            "squeezed into the window"
        );
    }

    #[test]
    fn ramp_builds_a_crescendo_in_time_order() {
        let mut notes = vec![
            note(480, 96, 60, 100),
            note(0, 96, 60, 100),
            note(240, 96, 60, 100),
        ];
        apply_velocity(
            &mut notes,
            &VelocityOp::Ramp { from: 40, to: 100 },
            DEFAULT_SEED,
        );
        let mut by_start: Vec<(i64, u8)> = notes.iter().map(|n| (n.start, n.velocity)).collect();
        by_start.sort_unstable();
        assert_eq!(by_start, vec![(0, 40), (240, 70), (480, 100)]);
    }

    #[test]
    fn humanize_is_deterministic_per_seed() {
        let make = || -> Vec<Note> { vec![note(0, 96, 60, 80), note(240, 96, 62, 80)] };
        let mut a = make();
        let mut b = make();
        let mut c = make();
        apply_velocity(&mut a, &VelocityOp::Humanize(30), DEFAULT_SEED);
        apply_velocity(&mut b, &VelocityOp::Humanize(30), DEFAULT_SEED);
        apply_velocity(&mut c, &VelocityOp::Humanize(30), DEFAULT_SEED + 1);
        assert_eq!(
            a.iter().map(|n| n.velocity).collect::<Vec<_>>(),
            b.iter().map(|n| n.velocity).collect::<Vec<_>>()
        );
        // Different seed: overwhelmingly different result for 2 notes.
        assert_ne!(
            a.iter().map(|n| n.velocity).collect::<Vec<_>>(),
            c.iter().map(|n| n.velocity).collect::<Vec<_>>()
        );
        for v in a.iter().map(|n| n.velocity) {
            assert!((50..=110).contains(&v), "jitter bounded by ±amount");
        }
    }

    #[test]
    fn duration_set_add_scale_clamp_to_one_tick() {
        let mut notes = vec![note(0, 96, 60, 100)];
        apply_duration(&mut notes, &DurationOp::Add(-1000));
        assert_eq!(notes[0].length, 1);
        apply_duration(&mut notes, &DurationOp::Scale(0.0));
        assert_eq!(notes[0].length, 1);
        apply_duration(&mut notes, &DurationOp::Set(240));
        assert_eq!(notes[0].length, 240);
    }

    #[test]
    fn legato_extends_to_the_next_note_minus_gap() {
        let mut notes = vec![
            note(0, 96, 60, 100),
            note(240, 96, 62, 100),
            note(480, 96, 64, 100),
        ];
        apply_duration(&mut notes, &DurationOp::Legato { gap: 12 });
        assert_eq!(notes[0].length, 228, "0 → 240-12");
        assert_eq!(notes[1].length, 228);
        assert_eq!(notes[2].length, 96, "the last note keeps its length");
    }

    #[test]
    fn remove_overlap_truncates_same_pitch_stacks() {
        let mut notes = vec![
            note(0, 480, 60, 100),
            note(240, 480, 60, 100), // same pitch, overlapping
            note(300, 96, 62, 100),  // other pitch: untouched
        ];
        apply_duration(&mut notes, &DurationOp::RemoveOverlap);
        assert_eq!(notes[0].length, 240, "truncated where the twin starts");
        assert_eq!(notes[1].length, 480);
        assert_eq!(notes[2].length, 96);
    }

    #[test]
    fn round_half_away_from_zero() {
        assert_eq!(round_i64(2.5), 3);
        assert_eq!(round_i64(-2.5), -3, "f32::round rounds half away from zero");
        assert_eq!(round_i64(2.4), 2);
    }

    #[test]
    fn apply_to_selected_touches_only_the_selection() {
        let mut notes = vec![
            note(0, 96, 60, 100),
            note(240, 96, 62, 100),
            note(480, 96, 64, 100),
        ];
        apply_to_selected(&mut notes, &[0, 2], |slice| transpose(slice, 12));
        assert_eq!(
            pitches(&notes),
            vec![72, 62, 76],
            "the unselected note is untouched"
        );
    }

    #[test]
    fn apply_to_selected_ignores_out_of_range_indices() {
        let mut notes = vec![note(0, 96, 60, 100)];
        apply_to_selected(&mut notes, &[0, 5, 9], |slice| transpose(slice, 1));
        assert_eq!(pitches(&notes), vec![61]);
    }
}
