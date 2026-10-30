//! Arpeggiator MIDI effect (spec `.agents/spec/midi-nodes.md` §5.1).
//!
//! Holds the currently pressed keys, advances one step per division boundary
//! and emits arp-owned note pairs on the block-rate event bus. Input notes are
//! **not** passed through (the original chord would double the arpeggio); only
//! a blanket `Off { note: 0 }` is forwarded so a transport stop reaches the
//! downstream instrument. No audio is produced — the node is MIDI-only.
//!
//! Real-time rules: fixed-size arrays, no allocation, no locks, no panics.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::events::NodeEvents;
use crate::realtime::processor::Processor;
use crate::realtime::sequencer::TICKS_PER_QUARTER;

/// Maximum number of keys held at once (extra keys are ignored).
pub const MAX_HELD_NOTES: usize = 16;

/// Step divisions, in the same order as the document's `division` index
/// (1/2, 1/4, 1/8, 1/8T, 1/16, 1/16T, 1/32, 1/64).
pub const ARP_DIVISION_COUNT: usize = 8;

const ARP_DIVISION_TICKS: [f64; ARP_DIVISION_COUNT] = [
    TICKS_PER_QUARTER as f64 * 2.0,
    TICKS_PER_QUARTER as f64,
    TICKS_PER_QUARTER as f64 / 2.0,
    TICKS_PER_QUARTER as f64 / 3.0,
    TICKS_PER_QUARTER as f64 / 4.0,
    TICKS_PER_QUARTER as f64 / 6.0,
    TICKS_PER_QUARTER as f64 / 8.0,
    TICKS_PER_QUARTER as f64 / 16.0,
];

/// Harmony rule: extra notes stacked on every held key (spec §5.1).
pub const ARP_HARMONY_COUNT: usize = 7;

/// Semitone intervals added to each held key, in `harmony` index order.
const HARMONY_INTERVALS: [&[u8]; ARP_HARMONY_COUNT] = [
    &[0],                                        // Off
    &[0, 12],                                    // Octave
    &[0, 7],                                     // Fifth
    &[0, 4],                                     // Major third
    &[0, 3],                                     // Minor third
    &[0, 2, 4, 6, 8, 10, 12],                    // Whole tone
    &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], // Chromatic
];

/// Step order (mirrors the document's `ArpMode`, transported as an `f32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Up,
    Down,
    UpDown,
    DownUp,
    Random,
}

/// Arpeggiator MIDI effect.
pub struct ArpeggiatorProcessor {
    division: u8,
    gate: f32,
    mode: Mode,
    octaves: u8,
    latch: bool,
    /// Harmony rule index into [`HARMONY_INTERVALS`].
    harmony: u8,
    /// Probability (0..1) of skipping a step without a note-on.
    skip: f32,
    /// Sorted held keys (ascending), the arp sequence source.
    held: [u8; MAX_HELD_NOTES],
    held_len: usize,
    /// Number of physically pressed keys (drives latch chord replacement).
    physical: u8,
    /// Position within the current sequence.
    cursor: usize,
    /// Absolute step index already consumed (`i64::MAX` = not anchored).
    step_index: i64,
    /// Currently sounding arp note.
    sounding: Option<u8>,
    /// Absolute tick at which the sounding note must stop.
    off_at: f64,
    /// Latest input velocity.
    velocity: f32,
    /// Seeded XorShift32 state (reproducible per node).
    rng: u32,
    /// Transport was active on the previous block.
    armed: bool,
    /// A blanket all-notes-off must be forwarded this block.
    blanket: bool,
}

impl ArpeggiatorProcessor {
    /// `seed` keeps the `Random` order reproducible per node.
    pub fn new(seed: u32) -> Self {
        Self {
            division: 4,
            gate: 0.5,
            mode: Mode::Up,
            octaves: 1,
            latch: false,
            harmony: 0,
            skip: 0.0,
            held: [0; MAX_HELD_NOTES],
            held_len: 0,
            physical: 0,
            cursor: 0,
            step_index: i64::MAX,
            sounding: None,
            off_at: 0.0,
            velocity: 0.8,
            rng: seed.max(1),
            armed: false,
            blanket: false,
        }
    }

    fn step_ticks(&self) -> f64 {
        ARP_DIVISION_TICKS[self.division.min(ARP_DIVISION_COUNT as u8 - 1) as usize]
    }

    /// Inserts `note` into the sorted held set (duplicates and overflow are
    /// ignored).
    fn hold(&mut self, note: u8) {
        if self.held[..self.held_len].contains(&note) {
            return;
        }
        if self.held_len == MAX_HELD_NOTES {
            return;
        }
        let at = self.held[..self.held_len]
            .iter()
            .position(|&key| key > note)
            .unwrap_or(self.held_len);
        self.held.copy_within(at..self.held_len, at + 1);
        self.held[at] = note;
        self.held_len += 1;
    }

    fn unhold(&mut self, note: u8) {
        if let Some(at) = self.held[..self.held_len]
            .iter()
            .position(|&key| key == note)
        {
            self.held.copy_within(at + 1..self.held_len, at);
            self.held_len -= 1;
        }
    }

    /// Sequence entry `index` over the harmony and octave layers.
    fn sequence_note(&self, index: usize) -> u8 {
        let intervals = HARMONY_INTERVALS[self.harmony.min(ARP_HARMONY_COUNT as u8 - 1) as usize];
        let key = self.held[index % self.held_len];
        let layer = index / self.held_len;
        let interval = intervals[layer % intervals.len()];
        let octave = layer / intervals.len();
        key.saturating_add(interval)
            .saturating_add(12 * octave as u8)
            .min(127)
    }

    fn sequence_len(&self) -> usize {
        let intervals = HARMONY_INTERVALS[self.harmony.min(ARP_HARMONY_COUNT as u8 - 1) as usize];
        self.held_len * intervals.len() * usize::from(self.octaves)
    }

    /// Advances the cursor and returns the next arp note.
    fn next_note(&mut self) -> u8 {
        let len = self.sequence_len();
        let index = match self.mode {
            Mode::Up => {
                let index = self.cursor % len;
                self.cursor = (self.cursor + 1) % len;
                index
            }
            Mode::Down => {
                let index = len - 1 - (self.cursor % len);
                self.cursor = (self.cursor + 1) % len;
                index
            }
            Mode::UpDown => {
                // 0,1,…,len-1,len-2,…,1 — endpoints are not repeated.
                let span = if len <= 1 { 1 } else { 2 * len - 2 };
                let phase = self.cursor % span;
                self.cursor = (self.cursor + 1) % span;
                if phase < len { phase } else { span - phase }
            }
            Mode::DownUp => {
                // len-1,…,0,1,…,len-2 — the mirror image of UpDown.
                let span = if len <= 1 { 1 } else { 2 * len - 2 };
                let phase = self.cursor % span;
                self.cursor = (self.cursor + 1) % span;
                if phase < len {
                    len - 1 - phase
                } else {
                    span - phase
                }
            }
            Mode::Random => (self.next_rand() as usize) % len,
        };
        self.sequence_note(index)
    }

    fn next_rand(&mut self) -> u32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x.max(1);
        x
    }

    fn release(&mut self, out: &mut NodeEvents) {
        if let Some(note) = self.sounding.take() {
            out.push_note(NoteEvent::Off { note });
        }
    }

    /// Clears all keys and stops the current step (blanket all-notes-off).
    fn all_notes_off(&mut self) {
        self.held_len = 0;
        self.physical = 0;
        self.cursor = 0;
        self.sounding = None;
        self.step_index = i64::MAX;
    }

    /// Loads the document parameters. Returns `false` for parameters this
    /// processor does not know.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_params(
        &mut self,
        division: u8,
        gate: f32,
        mode: u8,
        octaves: u8,
        latch: bool,
        harmony: u8,
        skip: f32,
    ) {
        self.set_param(ParamId::ArpDivision, f32::from(division));
        self.set_param(ParamId::ArpGate, gate);
        self.set_param(ParamId::ArpMode, f32::from(mode));
        self.set_param(ParamId::ArpOctaves, f32::from(octaves));
        self.set_param(ParamId::ArpLatch, if latch { 1.0 } else { 0.0 });
        self.set_param(ParamId::ArpHarmony, f32::from(harmony));
        self.set_param(ParamId::ArpSkip, skip);
    }
}

impl Processor for ArpeggiatorProcessor {
    fn process(&mut self, _ctx: &ProcessContext, _buffer: &mut AudioBuffer) {}

    fn process_events(&mut self, ctx: &ProcessContext, out: &mut NodeEvents) {
        if self.blanket {
            self.blanket = false;
            out.push_note(NoteEvent::Off { note: 0 });
            return;
        }
        if !ctx.transport_active {
            // Stopping releases the sounding note but keeps the held keys so a
            // resume can continue from the same chord.
            self.release(out);
            self.armed = false;
            self.step_index = i64::MAX;
            return;
        }
        let step = self.step_ticks();
        if step <= 0.0 {
            return;
        }
        if !self.armed {
            self.armed = true;
            // The block that arms the arp fires the step containing its start,
            // so the first note after pressing a key is immediate.
            let current = (ctx.song_ticks / step).floor() as i64;
            self.step_index = current - 1;
        }
        // Gate end first, then a new step (the order matters when one block
        // crosses a boundary: off → on, never on → off on the same pitch).
        if self.sounding.is_some() && ctx.song_ticks >= self.off_at {
            self.release(out);
        }
        let current = (ctx.song_ticks / step).floor() as i64;
        if current > self.step_index {
            // One step event pair per block at most (spec D12): skipped steps
            // collapse into this one, which keeps the bus bounded.
            self.step_index = current;
            self.release(out);
            if self.held_len > 0 {
                let note = self.next_note();
                // Random skip: the step stays silent but the cursor still
                // advances, so the pattern does not stall on one note.
                let skipped = self.skip > 0.0 && {
                    let roll = f64::from(self.next_rand()) / f64::from(u32::MAX);
                    roll < f64::from(self.skip)
                };
                if !skipped {
                    out.push_note(NoteEvent::On {
                        note,
                        velocity: self.velocity,
                    });
                    self.sounding = Some(note);
                    self.off_at = ctx.song_ticks + step * f64::from(self.gate);
                }
            }
        }
    }

    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { note, velocity } => {
                if self.latch && self.physical == 0 {
                    // A new chord replaces the latched one.
                    self.held_len = 0;
                    self.cursor = 0;
                }
                self.physical = self.physical.saturating_add(1);
                self.hold(note);
                if velocity.is_finite() {
                    self.velocity = velocity.clamp(0.0, 1.0);
                }
                true
            }
            // `note: 0` is the documented blanket all-notes-off (transport
            // stop / panic). Forward it downstream after clearing the state.
            NoteEvent::Off { note: 0 } => {
                self.all_notes_off();
                self.blanket = true;
                true
            }
            NoteEvent::Off { note } => {
                self.physical = self.physical.saturating_sub(1);
                if !self.latch {
                    self.unhold(note);
                    if self.held_len == 0 {
                        self.cursor = 0;
                    }
                }
                true
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::ArpDivision => {
                if value.is_finite() {
                    self.division = value.round().clamp(0.0, (ARP_DIVISION_COUNT - 1) as f32) as u8;
                }
                true
            }
            ParamId::ArpGate => {
                if value.is_finite() {
                    self.gate = value.clamp(0.05, 1.0);
                }
                true
            }
            ParamId::ArpMode => {
                if value.is_finite() {
                    self.mode = match value.round() as i32 {
                        0 => Mode::Up,
                        1 => Mode::Down,
                        2 => Mode::UpDown,
                        3 => Mode::DownUp,
                        _ => Mode::Random,
                    };
                }
                true
            }
            ParamId::ArpOctaves => {
                if value.is_finite() {
                    self.octaves = value.round().clamp(1.0, 4.0) as u8;
                }
                true
            }
            ParamId::ArpLatch => {
                self.latch = value >= 0.5;
                true
            }
            ParamId::ArpHarmony => {
                if value.is_finite() {
                    self.harmony = value.round().clamp(0.0, (ARP_HARMONY_COUNT - 1) as f32) as u8;
                }
                true
            }
            ParamId::ArpSkip => {
                if value.is_finite() {
                    self.skip = value.clamp(0.0, 0.9);
                }
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ARP_DIVISION_COUNT, ARP_DIVISION_TICKS, ArpeggiatorProcessor, MAX_HELD_NOTES};

    use crate::realtime::commands::{NoteEvent, ParamId};
    use crate::realtime::context::ProcessContext;
    use crate::realtime::events::NodeEvents;
    use crate::realtime::processor::Processor;
    use crate::realtime::sequencer::TICKS_PER_QUARTER;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;
    /// One block is five ticks at 120 BPM (960 ticks per quarter).
    const BLOCK_TICKS: f64 = BLOCK as f64 / SR as f64 * 2.0 * TICKS_PER_QUARTER as f64;

    fn ctx(ticks: f64, active: bool) -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: 0,
            song_ticks: ticks,
            transport_dt_ticks: BLOCK_TICKS,
            transport_active: active,
            sec_per_tick: 60.0 / (120.0 * TICKS_PER_QUARTER as f64),
        }
    }

    fn note_on(note: u8) -> NoteEvent {
        NoteEvent::On {
            note,
            velocity: 1.0,
        }
    }

    fn note_off(note: u8) -> NoteEvent {
        NoteEvent::Off { note }
    }

    /// Runs `blocks` blocks starting at `start`, collecting every emitted
    /// event. `advance` controls the tick step (defaults to one block).
    fn run(processor: &mut ArpeggiatorProcessor, start: f64, blocks: usize) -> Vec<NoteEvent> {
        let mut events = Vec::new();
        for index in 0..blocks {
            let mut out = NodeEvents::default();
            processor.process_events(&ctx(start + index as f64 * BLOCK_TICKS, true), &mut out);
            events.extend(out.iter().filter_map(|event| match event {
                crate::realtime::events::RtEvent::Note(note) => Some(note),
                _ => None,
            }));
        }
        events
    }

    #[test]
    fn division_table_matches_the_document_order() {
        assert_eq!(ARP_DIVISION_TICKS.len(), ARP_DIVISION_COUNT);
        assert_eq!(ARP_DIVISION_TICKS[0], TICKS_PER_QUARTER as f64 * 2.0);
        assert_eq!(ARP_DIVISION_TICKS[1], TICKS_PER_QUARTER as f64);
        assert_eq!(ARP_DIVISION_TICKS[3], TICKS_PER_QUARTER as f64 / 3.0);
        assert_eq!(ARP_DIVISION_TICKS[4], 240.0);
        assert_eq!(ARP_DIVISION_TICKS[7], 60.0);
    }

    #[test]
    fn emits_nothing_without_held_keys() {
        let mut processor = ArpeggiatorProcessor::new(1);
        let events = run(&mut processor, 0.0, 48);
        assert!(events.is_empty());
    }

    #[test]
    fn emits_one_step_per_division_boundary() {
        let mut processor = ArpeggiatorProcessor::new(1);
        processor.note_event(note_on(60));
        // 1/16 = 240 ticks; 46 blocks ≈ 235 ticks → exactly one boundary.
        let events = run(&mut processor, 0.0, 46);
        assert_eq!(
            events,
            vec![note_on(60), note_off(60)],
            "one on/off pair per step boundary"
        );
    }

    #[test]
    fn gate_closes_before_the_next_step() {
        let mut processor = ArpeggiatorProcessor::new(1);
        processor.set_param(ParamId::ArpGate, 0.5);
        processor.note_event(note_on(60));
        // Step 0 anchored at t=0; gate ends at 120 ticks (block 24).
        let events = run(&mut processor, 0.0, 25);
        assert_eq!(events, vec![note_on(60), note_off(60)]);
        // Still the same step: nothing new until tick 240.
        let rest = run(&mut processor, 25.0 * BLOCK_TICKS, 20);
        assert!(rest.is_empty(), "gate must not retrigger within one step");
    }

    #[test]
    fn modes_choose_the_documented_order() {
        // Up: 60, 64, 67 then wraps.
        let mut up = ArpeggiatorProcessor::new(1);
        for note in [60, 64, 67] {
            up.note_event(note_on(note));
        }
        let up_on: Vec<u8> = run(&mut up, 0.0, 236)
            .into_iter()
            .filter_map(|event| match event {
                NoteEvent::On { note, .. } => Some(note),
                _ => None,
            })
            .collect();
        assert_eq!(up_on, vec![60, 64, 67, 60, 64, 67]);

        // Down: reverse order.
        let mut down = ArpeggiatorProcessor::new(1);
        down.set_param(ParamId::ArpMode, 1.0);
        for note in [60, 64, 67] {
            down.note_event(note_on(note));
        }
        let down_on: Vec<u8> = run(&mut down, 0.0, 236)
            .into_iter()
            .filter_map(|event| match event {
                NoteEvent::On { note, .. } => Some(note),
                _ => None,
            })
            .collect();
        assert_eq!(down_on, vec![67, 64, 60, 67, 64, 60]);

        // UpDown: endpoints once (60,64,67,64,60,64,67).
        let mut up_down = ArpeggiatorProcessor::new(1);
        up_down.set_param(ParamId::ArpMode, 2.0);
        for note in [60, 64, 67] {
            up_down.note_event(note_on(note));
        }
        let up_down_on: Vec<u8> = run(&mut up_down, 0.0, 283)
            .into_iter()
            .filter_map(|event| match event {
                NoteEvent::On { note, .. } => Some(note),
                _ => None,
            })
            .collect();
        assert_eq!(up_down_on, vec![60, 64, 67, 64, 60, 64, 67]);
    }

    #[test]
    fn octaves_stack_above_the_root_key() {
        let mut processor = ArpeggiatorProcessor::new(1);
        processor.set_param(ParamId::ArpOctaves, 2.0);
        processor.note_event(note_on(60));
        let events: Vec<u8> = run(&mut processor, 0.0, 180)
            .into_iter()
            .filter_map(|event| match event {
                NoteEvent::On { note, .. } => Some(note),
                _ => None,
            })
            .collect();
        assert_eq!(events, vec![60, 72, 60, 72]);
    }

    #[test]
    fn latch_keeps_the_chord_and_a_new_note_replaces_it() {
        let mut processor = ArpeggiatorProcessor::new(1);
        processor.set_param(ParamId::ArpLatch, 1.0);
        processor.note_event(note_on(60));
        processor.note_event(note_on(64));
        // Release one key: latch keeps both.
        processor.note_event(note_off(64));
        processor.note_event(note_off(60));
        let events: Vec<u8> = run(&mut processor, 0.0, 120)
            .into_iter()
            .filter_map(|event| match event {
                NoteEvent::On { note, .. } => Some(note),
                _ => None,
            })
            .collect();
        assert!(events.contains(&60) && events.contains(&64));

        // A fresh note after all keys are up replaces the latched chord.
        let mut processor = ArpeggiatorProcessor::new(1);
        processor.set_param(ParamId::ArpLatch, 1.0);
        processor.note_event(note_on(60));
        processor.note_event(note_off(60));
        processor.note_event(note_on(72));
        let events: Vec<u8> = run(&mut processor, 0.0, 60)
            .into_iter()
            .filter_map(|event| match event {
                NoteEvent::On { note, .. } => Some(note),
                _ => None,
            })
            .collect();
        assert!(!events.contains(&60), "old chord must be gone: {events:?}");
        assert!(events.contains(&72));
    }

    #[test]
    fn blanket_off_clears_and_is_forwarded() {
        let mut processor = ArpeggiatorProcessor::new(1);
        processor.note_event(note_on(60));
        run(&mut processor, 0.0, 2);
        processor.note_event(NoteEvent::Off { note: 0 });
        let mut out = NodeEvents::default();
        processor.process_events(&ctx(BLOCK_TICKS * 2.0, true), &mut out);
        let events: Vec<NoteEvent> = out
            .iter()
            .filter_map(|event| match event {
                crate::realtime::events::RtEvent::Note(note) => Some(note),
                _ => None,
            })
            .collect();
        assert_eq!(events, vec![NoteEvent::Off { note: 0 }]);
        // And the held set is gone: later blocks stay silent.
        assert!(run(&mut processor, BLOCK_TICKS * 3.0, 48).is_empty());
    }

    #[test]
    fn disabled_transport_releases_the_sounding_note() {
        let mut processor = ArpeggiatorProcessor::new(1);
        processor.note_event(note_on(60));
        run(&mut processor, 0.0, 1);
        let mut out = NodeEvents::default();
        processor.process_events(&ctx(BLOCK_TICKS, false), &mut out);
        let events: Vec<NoteEvent> = out
            .iter()
            .filter_map(|event| match event {
                crate::realtime::events::RtEvent::Note(note) => Some(note),
                _ => None,
            })
            .collect();
        assert_eq!(events, vec![note_off(60)]);
    }

    #[test]
    fn held_capacity_is_bounded() {
        let mut processor = ArpeggiatorProcessor::new(1);
        for note in 0..(MAX_HELD_NOTES as u8 + 8) {
            processor.note_event(note_on(40 + note));
        }
        // The 17th key is ignored, but the arb still runs on the first 16.
        assert_eq!(processor.held_len, MAX_HELD_NOTES);
    }

    fn on_notes(processor: &mut ArpeggiatorProcessor, start: f64, blocks: usize) -> Vec<u8> {
        run(processor, start, blocks)
            .into_iter()
            .filter_map(|event| match event {
                NoteEvent::On { note, .. } => Some(note),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn down_up_mode_mirrors_up_down() {
        let mut processor = ArpeggiatorProcessor::new(1);
        processor.set_param(ParamId::ArpMode, 3.0);
        for note in [60, 64, 67] {
            processor.note_event(note_on(note));
        }
        // One full cycle plus the first repeat: 67,64,60,64,67,64,60.
        assert_eq!(
            on_notes(&mut processor, 0.0, 283),
            vec![67, 64, 60, 64, 67, 64, 60]
        );
    }

    #[test]
    fn harmony_stacks_the_selected_interval() {
        // Fifth: root, fifth, then the same pair one octave up.
        let mut fifth = ArpeggiatorProcessor::new(1);
        fifth.set_param(ParamId::ArpHarmony, 2.0);
        fifth.note_event(note_on(60));
        assert_eq!(on_notes(&mut fifth, 0.0, 236), vec![60, 67, 60, 67, 60, 67]);

        // Major third.
        let mut third = ArpeggiatorProcessor::new(1);
        third.set_param(ParamId::ArpHarmony, 3.0);
        third.note_event(note_on(60));
        assert_eq!(on_notes(&mut third, 0.0, 236), vec![60, 64, 60, 64, 60, 64]);

        // Whole tone ladder spans root..octave (7 notes per cycle).
        let mut ladder = ArpeggiatorProcessor::new(1);
        ladder.set_param(ParamId::ArpHarmony, 5.0);
        ladder.note_event(note_on(60));
        let first = on_notes(&mut ladder, 0.0, 330);
        assert_eq!(first, vec![60, 62, 64, 66, 68, 70, 72, 60]);
    }

    #[test]
    fn chromatic_harmony_reaches_the_octave() {
        let mut processor = ArpeggiatorProcessor::new(1);
        processor.set_param(ParamId::ArpHarmony, 6.0);
        processor.note_event(note_on(60));
        let notes = on_notes(&mut processor, 0.0, 570);
        let octave = notes.iter().position(|&note| note == 72).expect("octave");
        assert_eq!(octave, 12, "twelve semitones between root and octave");
    }

    #[test]
    fn skip_never_drops_notes_at_zero_and_drops_some_when_enabled() {
        let mut clean = ArpeggiatorProcessor::new(1);
        clean.note_event(note_on(60));
        assert_eq!(on_notes(&mut clean, 0.0, 236).len(), 6);

        let mut sparse = ArpeggiatorProcessor::new(1);
        sparse.set_param(ParamId::ArpSkip, 0.5);
        sparse.note_event(note_on(60));
        // Deterministic seed: some steps are skipped, but the pattern keeps
        // advancing (a non-zero number of notes still plays).
        let notes = on_notes(&mut sparse, 0.0, 236);
        assert!(!notes.is_empty());
        assert!(notes.len() < 6, "skip must drop steps: {notes:?}");
    }
}
