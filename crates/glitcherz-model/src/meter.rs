//! Piecewise time-signature evaluation (Met M3; spec `.agents/spec/meter-map.md`).
//!
//! A project carries one base meter plus a sorted table of
//! [`MeterMarker`]s ("from this tick on, the meter is X"). [`MeterMap`]
//! expands that into a segment table **once**, so every bar/tick query is a
//! binary search plus integer arithmetic instead of a linear scan — the ruler
//! draws thousands of grid lines per frame.
//!
//! Bar numbering accumulates across segments: a marker placed at bar 5 starts
//! the next segment at bar 5, so numbers never skip or repeat. Markers that
//! do not sit on a bar line (a hand-edited file) are snapped **down** to the
//! containing segment's grid, which is the tolerant read `meter-map.md` D2
//! requires — `Project::validate` does not reject them.

use crate::{MAX_ARRANGEMENT_TICKS, MeterMarker, TimeSignature};

/// One segment of the meter map: from `tick` on, bars have `signature` and the
/// first bar of the segment is numbered `bar` (0-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterSegment {
    /// Absolute segment start (a bar line; 0 for the first segment).
    pub tick: u32,
    /// 0-based number of the bar starting at `tick`.
    pub bar: u32,
    /// The meter valid from `tick` until the next segment.
    pub signature: TimeSignature,
}

/// Fallback used by the queries when a map was built with no segments (only
/// possible through a hand-rolled [`Default`]-like construction).
const FALLBACK: MeterSegment = MeterSegment {
    tick: 0,
    bar: 0,
    signature: TimeSignature::COMMON,
};

/// Precomputed piecewise meter table (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeterMap {
    segments: Vec<MeterSegment>,
}

impl Default for MeterMap {
    fn default() -> Self {
        Self {
            segments: vec![MeterSegment {
                tick: 0,
                bar: 0,
                signature: TimeSignature::COMMON,
            }],
        }
    }
}

impl MeterMap {
    /// Builds the segment table from the base meter and the document's
    /// markers. Markers are sorted here (files are validated as sorted, but
    /// the map also tolerates hand-written input) and aligned down to the
    /// containing segment's bar grid (D2); markers at tick 0 or duplicates are
    /// dropped — tick 0's meter is the base signature (D3).
    pub fn new(base: TimeSignature, markers: &[MeterMarker]) -> Self {
        let mut segments = Vec::with_capacity(markers.len() + 1);
        segments.push(MeterSegment {
            tick: 0,
            bar: 0,
            signature: base,
        });

        let mut sorted = markers.to_vec();
        sorted.sort_by_key(|marker| marker.tick);

        for marker in sorted {
            if marker.tick == 0 || u64::from(marker.tick) > MAX_ARRANGEMENT_TICKS {
                continue;
            }
            let Some(last) = segments.last().copied() else {
                break;
            };
            let bar_ticks = last.signature.ticks_per_bar().max(1);
            let offset = marker.tick.saturating_sub(last.tick);
            let aligned = last.tick + (offset / bar_ticks) * bar_ticks;
            if aligned <= last.tick {
                continue;
            }
            let bars = (aligned - last.tick) / bar_ticks;
            segments.push(MeterSegment {
                tick: aligned,
                bar: last.bar + bars,
                signature: TimeSignature::new(
                    marker.signature.numerator,
                    marker.signature.denominator,
                ),
            });
        }

        Self { segments }
    }

    /// The segment table, in ascending `tick` order. Always non-empty for maps
    /// built with [`MeterMap::new`]; UI grid drawing iterates it directly.
    pub fn segments(&self) -> &[MeterSegment] {
        &self.segments
    }

    /// Index of the segment containing `tick` (the last start `<= tick`).
    fn index_of(&self, tick: u32) -> usize {
        let mut lo = 0usize;
        let mut hi = self.segments.len();
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if self.segments.get(mid).is_some_and(|seg| seg.tick <= tick) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// The segment containing `tick`.
    pub fn segment_of(&self, tick: u32) -> &MeterSegment {
        self.segments.get(self.index_of(tick)).unwrap_or(&FALLBACK)
    }

    /// The meter in effect at `tick`.
    pub fn at(&self, tick: u32) -> TimeSignature {
        self.segment_of(tick).signature
    }

    /// Bar length in ticks at `tick`.
    pub fn ticks_per_bar_at(&self, tick: u32) -> u32 {
        self.at(tick).ticks_per_bar().max(1)
    }

    /// Beat length in ticks at `tick`.
    pub fn ticks_per_beat_at(&self, tick: u32) -> u32 {
        self.at(tick).ticks_per_beat().max(1)
    }

    /// 0-based `(bar, beat)` of `tick` within its segment.
    pub fn bar_beat_at(&self, tick: u32) -> (u32, u32) {
        let seg = self.segment_of(tick);
        let bar_ticks = seg.signature.ticks_per_bar().max(1);
        let beat_ticks = seg.signature.ticks_per_beat().max(1);
        let offset = tick.saturating_sub(seg.tick);
        (
            seg.bar + offset / bar_ticks,
            (offset % bar_ticks) / beat_ticks,
        )
    }

    /// Tick of the 0-based bar line `bar`, if it lies inside the map.
    pub fn bar_start(&self, bar: u32) -> Option<u32> {
        for (index, seg) in self.segments.iter().enumerate() {
            let next_bar = self.segments.get(index + 1).map(|next| next.bar);
            if next_bar.is_none_or(|limit| bar < limit) {
                if bar < seg.bar {
                    return None;
                }
                let bar_ticks = seg.signature.ticks_per_bar().max(1);
                return seg
                    .tick
                    .checked_add((bar - seg.bar).checked_mul(bar_ticks)?);
            }
        }
        None
    }

    /// The first bar line strictly after `tick`.
    pub fn next_bar(&self, tick: u32) -> u32 {
        let index = self.index_of(tick);
        let Some(seg) = self.segments.get(index) else {
            return tick;
        };
        let bar_ticks = seg.signature.ticks_per_bar().max(1);
        let offset = tick.saturating_sub(seg.tick);
        let next = seg
            .tick
            .saturating_add((offset / bar_ticks + 1) * bar_ticks);
        // A segment boundary is itself a bar line: the segment length is a
        // whole number of the previous segment's bars, so `next` lands on it.
        match self.segments.get(index + 1) {
            Some(next_seg) if next > next_seg.tick => next_seg.tick,
            _ => next,
        }
    }

    /// The last bar line strictly before `tick` (0 at the very start).
    pub fn prev_bar(&self, tick: u32) -> u32 {
        let index = self.index_of(tick);
        let Some(seg) = self.segments.get(index) else {
            return 0;
        };
        if tick > seg.tick {
            let bar_ticks = seg.signature.ticks_per_bar().max(1);
            return seg.tick + ((tick - seg.tick - 1) / bar_ticks) * bar_ticks;
        }
        let Some(prev) = index.checked_sub(1).and_then(|i| self.segments.get(i)) else {
            return 0;
        };
        let bar_ticks = prev.signature.ticks_per_bar().max(1);
        seg.tick.saturating_sub(bar_ticks)
    }

    /// Snaps `raw` to the nearest bar line of **its own segment** (the grid is
    /// relative to the segment start, so a 6/8 section keeps a 2880-tick grid
    /// even when the preceding 4/4 section used 3840).
    pub fn snap_bar(&self, raw: f64) -> u32 {
        if !raw.is_finite() || raw <= 0.0 {
            return 0;
        }
        let tick = raw.min(f64::from(u32::MAX)) as u32;
        let index = self.index_of(tick);
        let Some(seg) = self.segments.get(index) else {
            return tick;
        };
        let bar_ticks = f64::from(seg.signature.ticks_per_bar().max(1));
        let rel = (f64::from(tick) - f64::from(seg.tick)) / bar_ticks;
        let snapped = f64::from(seg.tick) + rel.round() * bar_ticks;
        let snapped = snapped.clamp(0.0, f64::from(u32::MAX)) as u32;
        match self.segments.get(index + 1) {
            // Rounding up at the end of a segment lands exactly on the next
            // segment's first bar line.
            Some(next_seg) if snapped > next_seg.tick => next_seg.tick,
            _ => snapped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MeterMap, MeterSegment};
    use crate::{MeterMarker, TICKS_PER_BAR, TimeSignature};

    fn marker(tick: u32, numerator: u8, denominator: u8) -> MeterMarker {
        MeterMarker {
            tick,
            signature: TimeSignature::new(numerator, denominator),
        }
    }

    #[test]
    fn an_empty_table_behaves_like_the_single_meter() {
        let map = MeterMap::new(TimeSignature::COMMON, &[]);
        assert_eq!(
            map.segments(),
            &[MeterSegment {
                tick: 0,
                bar: 0,
                signature: TimeSignature::COMMON
            }]
        );
        assert_eq!(map.at(TICKS_PER_BAR * 3 + 1), TimeSignature::COMMON);
        assert_eq!(map.bar_beat_at(0), (0, 0));
        assert_eq!(map.bar_beat_at(TICKS_PER_BAR), (1, 0));
        assert_eq!(map.bar_beat_at(TICKS_PER_BAR + 960), (1, 1));
        assert_eq!(map.bar_start(2), Some(TICKS_PER_BAR * 2));
        assert_eq!(map.next_bar(TICKS_PER_BAR), TICKS_PER_BAR * 2);
        assert_eq!(map.next_bar(0), TICKS_PER_BAR);
        assert_eq!(map.prev_bar(TICKS_PER_BAR * 2), TICKS_PER_BAR);
        assert_eq!(map.prev_bar(0), 0);
        assert_eq!(map.snap_bar(f64::from(TICKS_PER_BAR) - 1.0), TICKS_PER_BAR);
        assert_eq!(map.snap_bar(f64::from(TICKS_PER_BAR) + 1.0), TICKS_PER_BAR);
    }

    #[test]
    fn a_marker_at_bar_five_switches_to_six_eight_without_skipping_numbers() {
        let start = TICKS_PER_BAR * 5;
        let map = MeterMap::new(TimeSignature::COMMON, &[marker(start, 6, 8)]);
        assert_eq!(map.at(start - 1), TimeSignature::COMMON);
        assert_eq!(map.at(start), TimeSignature::new(6, 8));
        // 6/8 = 6 * 480 = 2880 ticks.
        assert_eq!(map.ticks_per_bar_at(start), 2880);
        assert_eq!(map.ticks_per_beat_at(start), 480);
        assert_eq!(map.bar_beat_at(start), (5, 0));
        assert_eq!(map.bar_beat_at(start + 480), (5, 1));
        assert_eq!(map.bar_beat_at(start + 2880), (6, 0));
        assert_eq!(map.bar_start(6), Some(start + 2880));
        assert_eq!(map.next_bar(start), start + 2880);
        assert_eq!(map.prev_bar(start), start - TICKS_PER_BAR);
    }

    #[test]
    fn bar_start_round_trips_across_segments() {
        let start = TICKS_PER_BAR * 4;
        let map = MeterMap::new(
            TimeSignature::COMMON,
            &[marker(start, 3, 4), marker(start + 2880 * 3, 6, 8)],
        );
        assert_eq!(map.bar_start(0), Some(0));
        assert_eq!(map.bar_start(3), Some(TICKS_PER_BAR * 3));
        assert_eq!(map.bar_start(4), Some(start));
        assert_eq!(map.bar_start(6), Some(start + 2880 * 2));
        assert_eq!(map.bar_start(7), Some(start + 2880 * 3));
        for bar in 0..10u32 {
            let tick = map.bar_start(bar).expect("bar is inside the map");
            assert_eq!(map.bar_beat_at(tick), (bar, 0));
        }
    }

    #[test]
    fn a_marker_off_the_grid_snaps_down_to_the_bar_line() {
        let start = TICKS_PER_BAR * 2;
        let map = MeterMap::new(TimeSignature::COMMON, &[marker(start + 100, 3, 4)]);
        assert_eq!(map.segments()[1].tick, start);
        assert_eq!(map.segments()[1].bar, 2);
        assert_eq!(map.at(start), TimeSignature::new(3, 4));

        // A marker that snaps onto the previous segment's start is dropped.
        let map = MeterMap::new(TimeSignature::COMMON, &[marker(10, 3, 4)]);
        assert_eq!(map.segments().len(), 1);
        // Tick 0 is the base meter's job (D3), so a marker there is dropped too.
        let map = MeterMap::new(TimeSignature::COMMON, &[marker(0, 3, 4)]);
        assert_eq!(map.segments().len(), 1);
    }

    #[test]
    fn markers_are_numbered_in_order_regardless_of_input_order() {
        let a = TICKS_PER_BAR * 2;
        // Four 3/4 bars after the first marker, so it sits on that grid.
        let b = a + 2880 * 4;
        let map = MeterMap::new(TimeSignature::COMMON, &[marker(b, 6, 8), marker(a, 3, 4)]);
        assert_eq!(map.segments().len(), 3);
        assert_eq!(map.segments()[1].tick, a);
        assert_eq!(map.segments()[1].bar, 2);
        assert_eq!(map.segments()[2].tick, b);
        assert_eq!(map.segments()[2].bar, 6);
    }

    #[test]
    fn snapping_uses_the_current_segment_grid() {
        let start = TICKS_PER_BAR * 2;
        let map = MeterMap::new(TimeSignature::COMMON, &[marker(start, 6, 8)]);
        // Just past the segment start: nearest 6/8 bar line is the start.
        assert_eq!(map.snap_bar(f64::from(start) + 100.0), start);
        // Closer to the next 2880 line.
        assert_eq!(map.snap_bar(f64::from(start) + 2000.0), start + 2880);
        // In the 4/4 region the old 3840 grid still applies.
        assert_eq!(
            map.snap_bar(f64::from(TICKS_PER_BAR) - 100.0),
            TICKS_PER_BAR
        );
        assert_eq!(map.snap_bar(-5.0), 0);
    }

    #[test]
    fn next_and_prev_bar_step_across_a_boundary() {
        let start = TICKS_PER_BAR * 2;
        let map = MeterMap::new(TimeSignature::COMMON, &[marker(start, 6, 8)]);
        // Last tick of the 4/4 segment steps onto the boundary.
        assert_eq!(map.next_bar(start - 1), start);
        // First tick of the 6/8 segment steps to its first internal bar line.
        assert_eq!(map.next_bar(start), start + 2880);
        assert_eq!(map.prev_bar(start + 1), start);
        assert_eq!(map.prev_bar(start), start - TICKS_PER_BAR);
    }
}
