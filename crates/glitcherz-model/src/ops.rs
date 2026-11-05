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

use crate::{DrumSlot, Note};

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

// ---- Time scale --------------------------------------------------------------------------

/// Time-scales notes around `pivot_tick` (the piano roll's Scale command):
/// every start moves to `pivot + (start - pivot) * factor` and every length
/// multiplies by `factor`, so ends scale with starts. `factor` 1.0 is the
/// identity; below 1 compresses the selection toward the pivot, above 1
/// stretches it away — the pivot itself never moves.
///
/// Per-note clamping (start ≥ 0, length ≥ 1) may swallow part of a scale:
/// a note squeezed against tick 0 stops there while the rest of the
/// selection keeps moving. The op never drops, adds or reorders notes, so
/// it keeps the slice-length contract of every op in this module.
pub fn scale_notes(notes: &mut [Note], pivot_tick: i64, factor: f32) {
    let pivot = pivot_tick as f32;
    for note in notes {
        note.start = round_i64(pivot + (note.start as f32 - pivot) * factor).max(0);
        note.length = round_i64(note.length as f32 * factor).max(1);
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

// ---- Randomize (W4) ----------------------------------------------------------------------

/// Random perturbation half-ranges. Each field is "±N": `pitch: 2` means up
/// to two semitones either way, `start: 40` up to 40 ticks either way. A zero
/// range leaves that field untouched.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RandomizeOptions {
    pub pitch: i32,
    pub start: i64,
    pub length: i64,
    pub velocity: u8,
    /// 0..=1 — the chance that a given note is perturbed at all. A skipped
    /// note is **not** deleted (the slice-length contract).
    pub probability: f32,
}

impl Default for RandomizeOptions {
    fn default() -> Self {
        Self {
            pitch: 0,
            start: 0,
            length: 0,
            velocity: 0,
            probability: 1.0,
        }
    }
}

/// Symmetric draw in `-amplitude..=amplitude`.
fn symmetric_f32(rng: &mut XorShift, amplitude: f32) -> f32 {
    (rng.next_f32() * 2.0 - 1.0) * amplitude
}

/// Randomizes pitch / start / length / velocity in place (W4).
///
/// Clamps keep every note legal (pitch 0..=127, start ≥ 0, length ≥ 1,
/// velocity 1..=127). The slice length never changes, so this is safe on a
/// selection; the caller restores the sort invariant.
pub fn randomize(notes: &mut [Note], options: &RandomizeOptions, seed: u64) {
    let probability = if options.probability.is_finite() {
        options.probability.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if probability <= 0.0 {
        return;
    }
    let mut rng = XorShift(seed);
    for note in notes {
        if probability < 1.0 && rng.next_f32() >= probability {
            continue;
        }
        if options.pitch > 0 {
            let delta = round_i64(symmetric_f32(&mut rng, options.pitch as f32));
            note.note = (i32::from(note.note) + delta as i32).clamp(0, 127) as u8;
        }
        if options.start > 0 {
            let delta = round_i64(symmetric_f32(&mut rng, options.start as f32));
            note.start = (note.start + delta).max(0);
        }
        if options.length > 0 {
            let delta = round_i64(symmetric_f32(&mut rng, options.length as f32));
            note.length = (note.length + delta).max(1);
        }
        if options.velocity > 0 {
            let delta = symmetric_f32(&mut rng, f32::from(options.velocity));
            note.velocity = (f32::from(note.velocity) + delta).round().clamp(1.0, 127.0) as u8;
        }
    }
}

// ---- Strum (W5) --------------------------------------------------------------------------

/// Attack order within a chord group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrumDirection {
    /// Low note first ("upstroke").
    Up,
    /// High note first ("downstroke").
    Down,
    /// Alternates per group, starting with [`StrumDirection::Up`].
    Alternate,
    /// Seeded shuffle per group.
    Random,
}

/// Strum parameters (W5). Everything is measured in ticks, relative to each
/// note's own start.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrumOptions {
    /// Gap between consecutive attacks.
    pub spread_ticks: i64,
    /// Notes whose starts fall within this window of the group's first note
    /// form one chord group.
    pub group_window_ticks: i64,
    /// Velocity delta from the first to the last attack (may be negative).
    pub velocity_ramp: i32,
    /// Extra ± tick jitter per note.
    pub jitter_ticks: i64,
    pub direction: StrumDirection,
}

impl Default for StrumOptions {
    fn default() -> Self {
        Self {
            spread_ticks: 0,
            group_window_ticks: 0,
            velocity_ramp: 0,
            jitter_ticks: 0,
            direction: StrumDirection::Up,
        }
    }
}

/// Staggers chord attacks in place (W5): notes are grouped by start time
/// (within `group_window_ticks`), ordered by pitch (or shuffled), and each
/// attack moves by `position · spread_ticks`. `length` is **never** changed,
/// so the notes keep their notated duration and their ends stagger with the
/// starts. The slice length is unchanged and the caller re-sorts.
pub fn strum(notes: &mut [Note], options: &StrumOptions, seed: u64) {
    if notes.is_empty() {
        return;
    }
    let spread = options.spread_ticks.max(0);
    let window = options.group_window_ticks.max(0);
    let jitter = options.jitter_ticks.max(0);
    let mut rng = XorShift(seed);

    // Time order, ties broken by pitch: a stable basis for chord grouping.
    let mut order: Vec<usize> = (0..notes.len()).collect();
    order.sort_by_key(|&i| (notes[i].start, notes[i].note, i));

    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut anchor = 0i64;
    for index in order {
        if current.is_empty() {
            anchor = notes[index].start;
            current.push(index);
        } else if notes[index].start - anchor <= window {
            current.push(index);
        } else {
            groups.push(std::mem::take(&mut current));
            anchor = notes[index].start;
            current.push(index);
        }
    }
    if !current.is_empty() {
        groups.push(current);
    }

    for (group_index, mut group) in groups.into_iter().enumerate() {
        group.sort_by_key(|&i| (notes[i].note, notes[i].start, i));
        match options.direction {
            StrumDirection::Up => {}
            StrumDirection::Down => group.reverse(),
            StrumDirection::Alternate => {
                if group_index % 2 == 1 {
                    group.reverse();
                }
            }
            StrumDirection::Random => {
                // Fisher–Yates, seeded.
                for i in (1..group.len()).rev() {
                    let j = (rng.next_f32() * (i as f32 + 1.0)) as usize;
                    group.swap(i, j.min(i));
                }
            }
        }
        let count = group.len();
        for (position, &index) in group.iter().enumerate() {
            let offset = position as i64 * spread;
            let wobble = if jitter > 0 {
                round_i64(symmetric_f32(&mut rng, jitter as f32))
            } else {
                0
            };
            notes[index].start = (notes[index].start + offset + wobble).max(0);
            if options.velocity_ramp != 0 && count > 1 {
                let t = position as f32 / (count - 1) as f32;
                let delta = options.velocity_ramp as f32 * t;
                notes[index].velocity = (f32::from(notes[index].velocity) + delta)
                    .round()
                    .clamp(1.0, 127.0) as u8;
            }
        }
    }
}

// ---- Drum kit key assignment (W1) --------------------------------------------------------

/// MIDI note name in scientific pitch notation (`60` → `C4`, `36` → `C2`).
///
/// One implementation for the whole app: the status bar, the event list, the
/// patch caption and the drum table must never disagree on the octave base.
pub fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let octave = i32::from(note) / 12 - 1;
    format!("{}{octave}", NAMES[usize::from(note) % 12])
}

/// Nearest key to `want` that `taken` does not occupy (spec `drum-kit.md`
/// K3: trigger keys stay unique).
///
/// Searches outward from `want` — upward first, so a kick dragged onto its
/// own octave lands on the next free key above rather than below — and
/// clamps to the MIDI range. `None` only when all 128 keys are taken, which a
/// 32-slot kit cannot reach, but the caller must not assume it.
pub fn nearest_free_note(taken: &[u8], want: u8) -> Option<u8> {
    let mut used = [false; 128];
    for &note in taken {
        used[usize::from(note & 0x7F)] = true;
    }
    if !used[usize::from(want)] {
        return Some(want);
    }
    for delta in 1..=127u8 {
        // `checked_add` guards the `u8` wrap, not the MIDI range: 1 + 127 is a
        // valid byte and would index past the end of the table.
        if let Some(note) = want
            .checked_add(delta)
            .filter(|note| usize::from(*note) < used.len() && !used[usize::from(*note)])
        {
            return Some(note);
        }
        if let Some(note) = want
            .checked_sub(delta)
            .filter(|note| !used[usize::from(*note)])
        {
            return Some(note);
        }
    }
    None
}

/// Free MIDI keys for a multi-file drum-kit load (spec `drum-kit.md` K10).
///
/// Starts at [`crate::DRUM_DEFAULT_NOTE`] (36, GM kick), skips keys already
/// used by `taken`, wraps around after 127, and returns at most `count` keys.
/// The caller caps `count` by the remaining slot budget.
pub fn assign_drum_notes(taken: &[u8], count: usize) -> Vec<u8> {
    let mut used = [false; 128];
    for &note in taken {
        used[usize::from(note & 0x7F)] = true;
    }
    let mut out = Vec::with_capacity(count.min(128));
    let mut key = crate::DRUM_DEFAULT_NOTE;
    let mut scanned = 0;
    while out.len() < count && scanned < 128 {
        let index = usize::from(key);
        if !used[index] {
            used[index] = true;
            out.push(key);
        }
        key = if key == 127 { 0 } else { key + 1 };
        scanned += 1;
    }
    out
}

/// Moves one drum slot to `to`, snapping to the nearest free key when `to` is
/// already taken (spec `drum-kit.md` K13).
///
/// Returns the key the slot landed on, or `None` when nothing moved (unknown
/// `from`, every key taken, or the slot already sits on the resolved key).
/// The slot keeps its position in `slots`, so the Inspector table never
/// reshuffles rows under a drag; only the trigger key changes.
pub fn reassign_drum_slot(slots: &mut [DrumSlot], from: u8, to: u8) -> Option<u8> {
    if !slots.iter().any(|slot| slot.note == from) {
        return None;
    }
    let taken: Vec<u8> = slots
        .iter()
        .map(|slot| slot.note)
        .filter(|note| *note != from)
        .collect();
    let target = nearest_free_note(&taken, to)?;
    if target == from {
        return None;
    }
    let slot = slots.iter_mut().find(|slot| slot.note == from)?;
    slot.note = target;
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_SEED, DurationOp, QuantizeOptions, RandomizeOptions, StrumDirection, StrumOptions,
        VelocityOp, apply_duration, apply_to_selected, apply_velocity, quantize, randomize,
        round_i64, scale_notes, strum, transpose,
    };
    use crate::{DrumSlot, Note};

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

    #[test]
    fn time_scale_stretches_starts_and_lengths_around_the_pivot() {
        let mut notes = vec![note(240, 240, 60, 100), note(480, 240, 62, 100)];
        scale_notes(&mut notes, 0, 2.0);
        assert_eq!((notes[0].start, notes[0].length), (480, 480));
        assert_eq!((notes[1].start, notes[1].length), (960, 480));
    }

    #[test]
    fn time_scale_around_a_mid_selection_pivot_keeps_the_pivot_fixed() {
        // Selection spans [240..720]; pivot at its center (480): the left
        // half doubles away leftward, the right half rightward.
        let mut notes = vec![note(240, 240, 60, 100), note(480, 240, 62, 100)];
        scale_notes(&mut notes, 480, 2.0);
        assert_eq!((notes[0].start, notes[0].length), (0, 480));
        assert_eq!(
            (notes[1].start, notes[1].length),
            (480, 480),
            "the pivot itself never moves"
        );
    }

    #[test]
    fn time_scale_below_one_compresses_toward_the_pivot() {
        let mut notes = vec![note(240, 240, 60, 100), note(480, 240, 62, 100)];
        scale_notes(&mut notes, 480, 0.5);
        assert_eq!((notes[0].start, notes[0].length), (360, 120));
        assert_eq!((notes[1].start, notes[1].length), (480, 120));
    }

    #[test]
    fn time_scale_clamps_starts_and_lengths_per_note() {
        // Squeezed against tick 0 the note stops there; the rest of the
        // selection would keep moving.
        let mut notes = vec![note(240, 240, 60, 100)];
        scale_notes(&mut notes, 480, 8.0);
        assert_eq!(notes[0].start, 0, "a start below zero floors at 0");
        assert_eq!(notes[0].length, 1920, "240 × 8");

        let mut tiny = vec![note(0, 240, 60, 100)];
        scale_notes(&mut tiny, 0, 0.001);
        assert_eq!(tiny[0].length, 1, "a collapsed length floors at one tick");
    }

    #[test]
    fn time_scale_of_one_is_the_identity() {
        let mut notes = vec![note(240, 240, 60, 100), note(960, 96, 62, 100)];
        let before = notes.clone();
        scale_notes(&mut notes, 480, 1.0);
        assert_eq!(notes, before);
    }

    #[test]
    fn time_scale_keeps_the_slice_length_and_touches_only_the_selection() {
        // The ops contract: in place, never changes the slice length, so it
        // can be applied to a selection.
        let mut notes = vec![
            note(0, 96, 60, 100),
            note(240, 96, 62, 100),
            note(480, 96, 64, 100),
        ];
        apply_to_selected(&mut notes, &[0, 2], |slice| scale_notes(slice, 240, 0.5));
        assert_eq!(notes.len(), 3, "the slice length never changes");
        assert_eq!((notes[0].start, notes[0].length), (120, 48));
        assert_eq!(
            (notes[1].start, notes[1].length),
            (240, 96),
            "the unselected note is untouched"
        );
        assert_eq!((notes[2].start, notes[2].length), (360, 48));
    }

    fn starts_by_pitch(notes: &[Note]) -> Vec<(u8, i64)> {
        let mut pairs: Vec<(u8, i64)> = notes.iter().map(|n| (n.note, n.start)).collect();
        pairs.sort_unstable();
        pairs
    }

    fn chord() -> Vec<Note> {
        vec![
            note(0, 240, 60, 100),
            note(0, 240, 64, 90),
            note(0, 240, 67, 80),
        ]
    }

    /// W4: the same seed replays identically and every field stays legal.
    #[test]
    fn randomize_is_bounded_and_seeded() {
        let source = vec![
            note(0, 240, 60, 100),
            note(480, 240, 127, 1),
            note(960, 1, 0, 127),
        ];
        let options = RandomizeOptions {
            pitch: 12,
            start: 120,
            length: 240,
            velocity: 40,
            probability: 1.0,
        };
        let mut first = source.clone();
        let mut second = source.clone();
        randomize(&mut first, &options, 42);
        randomize(&mut second, &options, 42);
        assert_eq!(first, second, "seeded replay");
        assert_ne!(first, source, "ranges are large enough to move something");
        for note in &first {
            assert!((0..=127).contains(&note.note));
            assert!(note.start >= 0);
            assert!(note.length >= 1);
            assert!((1..=127).contains(&note.velocity));
        }
        assert_eq!(first.len(), source.len(), "no note may be dropped");
    }

    #[test]
    fn randomize_with_zero_ranges_is_the_identity() {
        let mut notes = vec![note(120, 240, 60, 100), note(480, 480, 64, 80)];
        let before = notes.clone();
        randomize(&mut notes, &RandomizeOptions::default(), DEFAULT_SEED);
        assert_eq!(notes, before);
    }

    /// A low probability skips notes instead of deleting them: the slice
    /// keeps its length, and probability 0 is a full no-op.
    #[test]
    fn randomize_probability_skips_without_deleting() {
        let mut notes: Vec<Note> = (0..32).map(|i| note(i * 240, 240, 60, 100)).collect();
        let before = notes.clone();
        let options = RandomizeOptions {
            pitch: 0,
            start: 0,
            length: 0,
            velocity: 10,
            probability: 0.0,
        };
        randomize(&mut notes, &options, 3);
        assert_eq!(notes, before, "probability 0 is a no-op");

        let options = RandomizeOptions {
            probability: 0.5,
            ..options
        };
        randomize(&mut notes, &options, 3);
        assert_eq!(notes.len(), before.len());
        let untouched = notes
            .iter()
            .zip(&before)
            .filter(|(after, before)| *after == *before)
            .count();
        assert!(
            untouched > 0 && untouched < notes.len(),
            "a 0.5 chance must skip some and touch others (saw {untouched}/32)"
        );
    }

    /// W5: Up strums from the lowest note, spreading starts by one gap.
    #[test]
    fn strum_up_spaces_from_the_lowest_note() {
        let mut notes = chord();
        strum(
            &mut notes,
            &StrumOptions {
                spread_ticks: 120,
                direction: StrumDirection::Up,
                ..StrumOptions::default()
            },
            1,
        );
        assert_eq!(starts_by_pitch(&notes), vec![(60, 0), (64, 120), (67, 240)]);
        assert!(
            notes.iter().all(|n| n.length == 240),
            "lengths never change"
        );
    }

    #[test]
    fn strum_down_reverses_the_attack_order() {
        let mut notes = chord();
        strum(
            &mut notes,
            &StrumOptions {
                spread_ticks: 120,
                direction: StrumDirection::Down,
                ..StrumOptions::default()
            },
            1,
        );
        assert_eq!(starts_by_pitch(&notes), vec![(60, 240), (64, 120), (67, 0)]);
    }

    #[test]
    fn strum_groups_only_notes_inside_the_window() {
        let mut notes = vec![
            note(0, 240, 60, 100),
            note(30, 240, 64, 100),
            note(960, 240, 67, 100),
        ];
        // Window 60: the first two group, the third starts a new group.
        strum(
            &mut notes,
            &StrumOptions {
                spread_ticks: 120,
                group_window_ticks: 60,
                direction: StrumDirection::Up,
                ..StrumOptions::default()
            },
            1,
        );
        assert_eq!(
            starts_by_pitch(&notes),
            vec![(60, 0), (64, 150), (67, 960)],
            "the lone note keeps its own start"
        );
    }

    #[test]
    fn strum_alternates_between_groups() {
        let mut notes = vec![
            note(0, 240, 60, 100),
            note(0, 240, 67, 100),
            note(960, 240, 62, 100),
            note(960, 240, 69, 100),
        ];
        strum(
            &mut notes,
            &StrumOptions {
                spread_ticks: 100,
                group_window_ticks: 0,
                direction: StrumDirection::Alternate,
                ..StrumOptions::default()
            },
            1,
        );
        assert_eq!(
            starts_by_pitch(&notes),
            vec![(60, 0), (62, 1060), (67, 100), (69, 960)],
            "group 1 is an upstroke, group 2 a downstroke"
        );
    }

    #[test]
    fn strum_random_is_seeded() {
        let options = StrumOptions {
            spread_ticks: 60,
            group_window_ticks: 0,
            direction: StrumDirection::Random,
            ..StrumOptions::default()
        };
        let mut first = chord();
        let mut second = chord();
        strum(&mut first, &options, 99);
        strum(&mut second, &options, 99);
        assert_eq!(first, second);
    }

    #[test]
    fn strum_ramp_and_jitter_stay_clamped() {
        let mut notes = chord();
        strum(
            &mut notes,
            &StrumOptions {
                spread_ticks: 60,
                group_window_ticks: 0,
                velocity_ramp: 100,
                jitter_ticks: 5,
                direction: StrumDirection::Up,
            },
            5,
        );
        // Up order 60 → 64 → 67 gets 100% → ~150 → 180, clamped at 127.
        let by_pitch = |pitch: u8| notes.iter().find(|n| n.note == pitch).unwrap().velocity;
        assert_eq!(by_pitch(60), 100);
        assert_eq!(by_pitch(64), 127);
        assert_eq!(by_pitch(67), 127);
        assert!(notes.iter().all(|n| n.start >= 0 && n.length == 240));
    }

    #[test]
    fn strum_with_zero_spread_is_the_identity() {
        let mut notes = chord();
        let before = notes.clone();
        strum(&mut notes, &StrumOptions::default(), 1);
        assert_eq!(notes, before);
    }

    /// W1: new drum slots start at GM kick (36), skip used keys and wrap.
    #[test]
    fn drum_keys_start_at_36_and_skip_taken() {
        assert_eq!(super::assign_drum_notes(&[], 3), vec![36, 37, 38]);
        assert_eq!(super::assign_drum_notes(&[36, 38], 2), vec![37, 39]);
        let taken: Vec<u8> = (36..=127).collect();
        assert_eq!(
            super::assign_drum_notes(&taken, 2),
            vec![0, 1],
            "after 127 the scan wraps to 0"
        );
        assert_eq!(
            super::assign_drum_notes(&[], 200).len(),
            128,
            "there are only 128 MIDI keys"
        );
    }

    /// One octave convention for the whole app: middle C is C4 (60).
    #[test]
    fn note_names_are_scientific_pitch() {
        assert_eq!(super::note_name(60), "C4", "middle C");
        assert_eq!(super::note_name(61), "C#4");
        assert_eq!(super::note_name(36), "C2", "GM kick");
        assert_eq!(super::note_name(42), "F#2", "GM closed hi-hat");
        assert_eq!(super::note_name(0), "C-1");
        assert_eq!(super::note_name(127), "G9");
    }

    /// A moved slot snaps to the closest free key, preferring upward.
    #[test]
    fn nearest_free_note_searches_outward() {
        assert_eq!(super::nearest_free_note(&[], 36), Some(36), "free target");
        assert_eq!(
            super::nearest_free_note(&[35, 36], 36),
            Some(37),
            "equidistant: upward wins"
        );
        assert_eq!(
            super::nearest_free_note(&[36, 37], 36),
            Some(35),
            "35 is closer than 38"
        );
        assert_eq!(super::nearest_free_note(&[126, 127], 127), Some(125));
        assert_eq!(super::nearest_free_note(&[0], 0), Some(1), "clamped at 0");
        assert_eq!(
            super::nearest_free_note(&(0..=127).collect::<Vec<_>>(), 60),
            None
        );
    }

    /// Moving a slot keeps the table order and never duplicates a key.
    #[test]
    fn reassigning_a_slot_snaps_and_keeps_order() {
        let mut slots = vec![
            DrumSlot {
                note: 36,
                ..DrumSlot::default()
            },
            DrumSlot {
                note: 38,
                ..DrumSlot::default()
            },
            DrumSlot {
                note: 42,
                ..DrumSlot::default()
            },
        ];
        assert_eq!(super::reassign_drum_slot(&mut slots, 36, 40), Some(40));
        assert_eq!(
            slots.iter().map(|slot| slot.note).collect::<Vec<_>>(),
            vec![40, 38, 42],
            "order is stable; only the moved key changed"
        );
        // 38 is taken → snap to the nearest free key, never overwrite.
        assert_eq!(super::reassign_drum_slot(&mut slots, 40, 38), Some(39));
        let keys: Vec<u8> = slots.iter().map(|slot| slot.note).collect();
        assert_eq!(keys, vec![39, 38, 42]);
        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), keys.len(), "keys stay unique");
        // No-op and unknown source.
        assert_eq!(super::reassign_drum_slot(&mut slots, 39, 39), None);
        assert_eq!(super::reassign_drum_slot(&mut slots, 60, 61), None);
    }
}
