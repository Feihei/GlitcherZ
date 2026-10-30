//! Portamento / glide MIDI effect (spec `.agents/spec/midi-nodes.md` §5.2).
//!
//! Passes every note through unchanged and emits one semitone offset per
//! block on the control output. On a new note-on the offset starts at
//! `previous_note - new_note` (so the destination voice begins at the old
//! pitch) and decays to zero over `time_ms`. The downstream instrument applies
//! it as `ParamId::PitchOffset` (per-sample smoothing) or as a 14-bit bend via
//! `AutoDest::PitchBend`.
//!
//! Real-time rules: fixed-size arrays, no allocation, no locks, no panics.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::events::NodeEvents;
use crate::realtime::processor::Processor;

/// Maximum note events forwarded from one block.
pub const MAX_PENDING_NOTES: usize = 16;

/// Portamento MIDI effect.
pub struct PortamentoProcessor {
    time_ms: f32,
    min: f32,
    max: f32,
    legato_only: bool,
    /// Last note-on pitch (reference for the next glide).
    previous: Option<u8>,
    /// Current offset in semitones.
    offset: f32,
    /// Physically held keys.
    held: u8,
    /// Notes received since the last block, forwarded in order.
    pending: [Option<NoteEvent>; MAX_PENDING_NOTES],
    pending_len: usize,
}

impl PortamentoProcessor {
    pub fn new() -> Self {
        Self {
            time_ms: 120.0,
            min: -24.0,
            max: 24.0,
            legato_only: false,
            previous: None,
            offset: 0.0,
            held: 0,
            pending: [None; MAX_PENDING_NOTES],
            pending_len: 0,
        }
    }

    /// Loads the document parameters.
    pub fn apply_params(&mut self, time_ms: f32, min: f32, max: f32, legato_only: bool) {
        self.set_param(ParamId::PortTime, time_ms);
        self.set_param(ParamId::PortMin, min);
        self.set_param(ParamId::PortMax, max);
        self.set_param(ParamId::PortLegato, if legato_only { 1.0 } else { 0.0 });
    }

    fn push_pending(&mut self, event: NoteEvent) {
        let index = self.pending_len;
        if index >= MAX_PENDING_NOTES {
            return;
        }
        self.pending[index] = Some(event);
        self.pending_len += 1;
    }

    fn clear_pending(&mut self) {
        self.pending_len = 0;
    }
}

impl Default for PortamentoProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl Processor for PortamentoProcessor {
    fn process(&mut self, _ctx: &ProcessContext, _buffer: &mut AudioBuffer) {}

    fn process_events(&mut self, ctx: &ProcessContext, out: &mut NodeEvents) {
        // Forward the notes received since the last block, in input order.
        for event in self.pending[..self.pending_len].iter().flatten() {
            out.push_note(*event);
        }
        self.clear_pending();
        // Block-rate glide: `offset *= exp(-dt / tau)`, exact for an
        // exponential approach to zero. A zero `tau` snaps immediately.
        let dt = ctx.frames as f64 / f64::from(ctx.sample_rate.max(1.0));
        let tau = f64::from(self.time_ms) / 1000.0;
        if self.time_ms <= 0.0 || tau <= 0.0 {
            self.offset = 0.0;
        } else if self.offset != 0.0 {
            let coefficient = (-dt / tau).exp() as f32;
            self.offset *= coefficient;
            if self.offset.abs() < 1e-4 {
                self.offset = 0.0;
            }
        }
        // Normalized `[-1, 1]` against the node's own semitone scale: the
        // target's axis no longer has to be semitones (spec `curve-map.md`
        // D1/D6). `min == max` has no scale to speak of — hold the centre.
        let offset = self.offset.clamp(self.min, self.max);
        let span = self.max - self.min;
        out.control = Some(if span.abs() > f32::EPSILON {
            ((offset - self.min) / span * 2.0 - 1.0).clamp(-1.0, 1.0)
        } else {
            0.0
        });
    }

    /// D6: the glide range is the axis — the offset is emitted normalized
    /// against it, so the destination gets semitones back regardless of what
    /// its own default axis says.
    fn map_spec(&self) -> Option<crate::realtime::map::MapSpec> {
        let (min, max) = if self.max < self.min {
            (self.max, self.min)
        } else {
            (self.min, self.max)
        };
        Some(crate::realtime::map::MapSpec::linear(min, max))
    }

    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { note, .. } => {
                let legato = self.held > 0;
                if let Some(previous) = self.previous {
                    // Start the voice at the old pitch, then glide to the new
                    // one: a +4 semitone jump starts at -4.
                    let delta = f32::from(previous) - f32::from(note);
                    if !self.legato_only || legato {
                        self.offset = delta.clamp(self.min, self.max);
                    } else {
                        self.offset = 0.0;
                    }
                } else {
                    self.offset = 0.0;
                }
                self.previous = Some(note);
                self.held = self.held.saturating_add(1);
                self.push_pending(event);
                true
            }
            // Blanket all-notes-off: drop the reference so a later note does
            // not glide from stale pitch, and forward the blanket downstream.
            NoteEvent::Off { note: 0 } => {
                self.held = 0;
                self.previous = None;
                self.offset = 0.0;
                self.clear_pending();
                self.push_pending(event);
                true
            }
            NoteEvent::Off { note: _ } => {
                self.held = self.held.saturating_sub(1);
                self.push_pending(event);
                true
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::PortTime => {
                if value.is_finite() {
                    self.time_ms = value.clamp(0.0, 2000.0);
                }
                true
            }
            ParamId::PortMin => {
                if value.is_finite() {
                    self.min = value.clamp(-24.0, 24.0);
                }
                true
            }
            ParamId::PortMax => {
                if value.is_finite() {
                    self.max = value.clamp(-24.0, 24.0);
                }
                true
            }
            ParamId::PortLegato => {
                self.legato_only = value >= 0.5;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PortamentoProcessor;
    use crate::realtime::commands::{NoteEvent, ParamId};
    use crate::realtime::context::ProcessContext;
    use crate::realtime::events::{NodeEvents, RtEvent};
    use crate::realtime::processor::Processor;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn ctx() -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        }
    }

    fn note_on(note: u8) -> NoteEvent {
        NoteEvent::On {
            note,
            velocity: 1.0,
        }
    }

    /// Runs `blocks` blocks and returns the last emitted control value.
    /// Runs `blocks` blocks and returns the glide offset in **semitones** —
    /// the bus carries it normalized (`[-1, 1]`, spec `curve-map.md` D1) and
    /// the node's own semitone range is the map that decodes it.
    fn run(processor: &mut PortamentoProcessor, blocks: usize) -> f32 {
        let map = processor.map_spec().expect("portamento declares its map");
        let mut control = 0.0;
        for _ in 0..blocks {
            let mut out = NodeEvents::default();
            processor.process_events(&ctx(), &mut out);
            control = out.control.unwrap_or(0.0);
        }
        map.map(control)
    }

    #[test]
    fn passes_notes_through_in_order() {
        let mut processor = PortamentoProcessor::new();
        processor.note_event(note_on(60));
        processor.note_event(NoteEvent::Off { note: 60 });
        let mut out = NodeEvents::default();
        processor.process_events(&ctx(), &mut out);
        let events: Vec<NoteEvent> = out
            .iter()
            .filter_map(|event| match event {
                RtEvent::Note(note) => Some(note),
                _ => None,
            })
            .collect();
        assert_eq!(events, vec![note_on(60), NoteEvent::Off { note: 60 }]);
    }

    #[test]
    fn first_note_does_not_glide() {
        let mut processor = PortamentoProcessor::new();
        processor.note_event(note_on(60));
        assert_eq!(run(&mut processor, 1), 0.0);
    }

    #[test]
    fn jump_starts_at_the_old_pitch_and_decays_to_zero() {
        let mut processor = PortamentoProcessor::new();
        processor.set_param(ParamId::PortTime, 120.0);
        processor.note_event(note_on(60));
        run(&mut processor, 1);
        // +4 semitones: the voice starts four semitones below the new note.
        processor.note_event(note_on(64));
        let first = run(&mut processor, 1);
        assert!(
            (-4.0..=-3.85).contains(&first),
            "expected a near -4 semitone offset, got {first}"
        );
        // 1 s ≈ 8.3 time constants: effectively zero.
        let settled = run(&mut processor, 370);
        assert!(
            settled.abs() < 0.01,
            "offset must converge to zero: {settled}"
        );
    }

    #[test]
    fn downward_jump_is_positive() {
        let mut processor = PortamentoProcessor::new();
        processor.set_param(ParamId::PortTime, 500.0);
        processor.note_event(note_on(64));
        run(&mut processor, 1);
        processor.note_event(note_on(60));
        let first = run(&mut processor, 1);
        assert!(first > 3.9, "64 → 60 starts about +4 st: {first}");
    }

    #[test]
    fn legato_only_skips_when_no_key_is_held() {
        let mut processor = PortamentoProcessor::new();
        processor.set_param(ParamId::PortLegato, 1.0);
        processor.note_event(note_on(60));
        processor.note_event(NoteEvent::Off { note: 60 });
        run(&mut processor, 1);
        processor.note_event(note_on(64));
        assert_eq!(run(&mut processor, 1), 0.0);

        // With legato off the glide happens regardless.
        let mut processor = PortamentoProcessor::new();
        processor.set_param(ParamId::PortLegato, 0.0);
        processor.note_event(note_on(60));
        processor.note_event(NoteEvent::Off { note: 60 });
        run(&mut processor, 1);
        processor.note_event(note_on(64));
        assert!(run(&mut processor, 1) < -3.9);
    }

    #[test]
    fn blanket_off_clears_state_and_is_forwarded() {
        let mut processor = PortamentoProcessor::new();
        processor.note_event(note_on(60));
        processor.note_event(note_on(64));
        processor.note_event(NoteEvent::Off { note: 0 });
        let mut out = NodeEvents::default();
        processor.process_events(&ctx(), &mut out);
        let events: Vec<NoteEvent> = out
            .iter()
            .filter_map(|event| match event {
                RtEvent::Note(note) => Some(note),
                _ => None,
            })
            .collect();
        assert_eq!(events, vec![NoteEvent::Off { note: 0 }]);
        assert_eq!(out.control, Some(0.0));
    }

    #[test]
    fn output_is_clamped_to_the_configured_scale() {
        let mut processor = PortamentoProcessor::new();
        processor.set_param(ParamId::PortTime, 1000.0);
        processor.set_param(ParamId::PortMin, -2.0);
        processor.set_param(ParamId::PortMax, 2.0);
        processor.note_event(note_on(60));
        run(&mut processor, 1);
        processor.note_event(note_on(72));
        // -12 semitones clamps to the -2 low end (one block of glide has
        // already decayed it a hair).
        let value = run(&mut processor, 1);
        assert!((-2.0..=-1.9).contains(&value), "clamped = {value}");
    }

    #[test]
    fn zero_time_snaps_to_the_new_pitch() {
        let mut processor = PortamentoProcessor::new();
        processor.set_param(ParamId::PortTime, 0.0);
        processor.note_event(note_on(60));
        processor.note_event(note_on(64));
        assert_eq!(run(&mut processor, 2), 0.0);
    }
}
