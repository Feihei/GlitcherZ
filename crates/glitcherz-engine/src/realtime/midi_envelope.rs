//! MIDI-triggered envelope control source (spec
//! `.agents/spec/midi-nodes.md` §5.3).
//!
//! Note-ons drive an ADSR that emits one scaled value per block on the control
//! output. Unlike the synth's internal envelope this is **routable**: the
//! destination chooses a parameter, a CC or a pitch bend (`AutoDest`), so the
//! same envelope can open a filter, ride a fader or inject a controller.
//!
//! Real-time rules: no allocation, no locks, no panics.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::events::NodeEvents;
use crate::realtime::processor::Processor;

/// Envelope stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// MIDI-triggered envelope control source.
pub struct MidiEnvelopeProcessor {
    attack: f32,
    decay: f32,
    sustain: f32,
    release: f32,
    min: f32,
    max: f32,
    stage: Stage,
    level: f32,
    /// Segment start value and duration (seconds) for the current ramp.
    from: f32,
    to: f32,
    duration: f32,
    elapsed: f32,
}

impl MidiEnvelopeProcessor {
    pub fn new() -> Self {
        Self {
            attack: 0.01,
            decay: 0.2,
            sustain: 0.7,
            release: 0.4,
            min: 0.0,
            max: 1.0,
            stage: Stage::Idle,
            level: 0.0,
            from: 0.0,
            to: 0.0,
            duration: 0.0,
            elapsed: 0.0,
        }
    }

    /// Loads the document parameters.
    pub fn apply_params(&mut self, attack: f32, decay: f32, sustain: f32, release: f32) {
        self.set_param(ParamId::EnvAttack, attack);
        self.set_param(ParamId::EnvDecay, decay);
        self.set_param(ParamId::EnvSustain, sustain);
        self.set_param(ParamId::EnvRelease, release);
    }

    fn enter(&mut self, stage: Stage) {
        self.from = self.level;
        self.elapsed = 0.0;
        self.to = match stage {
            Stage::Attack => 1.0,
            Stage::Decay => self.sustain,
            Stage::Release => 0.0,
            Stage::Idle | Stage::Sustain => self.level,
        };
        self.duration = match stage {
            Stage::Attack => self.attack,
            Stage::Decay => self.decay,
            Stage::Release => self.release,
            Stage::Idle | Stage::Sustain => 0.0,
        };
        self.stage = stage;
        if self.duration <= 0.0 {
            // Zero-length segment: jump to the target and advance immediately.
            self.level = self.to;
            self.next_stage();
        }
    }

    fn next_stage(&mut self) {
        match self.stage {
            Stage::Attack => self.enter(Stage::Decay),
            Stage::Decay => {
                self.stage = Stage::Sustain;
                self.level = self.sustain;
            }
            Stage::Release => {
                self.stage = Stage::Idle;
                self.level = 0.0;
            }
            Stage::Idle | Stage::Sustain => {}
        }
    }

    fn update(&mut self, dt: f32) {
        match self.stage {
            Stage::Idle => self.level = 0.0,
            Stage::Sustain => self.level = self.sustain,
            Stage::Attack | Stage::Decay | Stage::Release => {
                self.elapsed += dt;
                let progress = if self.duration > 0.0 {
                    (self.elapsed / self.duration).min(1.0)
                } else {
                    1.0
                };
                self.level = self.from + (self.to - self.from) * progress;
                if progress >= 1.0 {
                    self.next_stage();
                }
            }
        }
        self.level = self.level.clamp(0.0, 1.0);
    }
}

impl Default for MidiEnvelopeProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl Processor for MidiEnvelopeProcessor {
    fn process(&mut self, _ctx: &ProcessContext, _buffer: &mut AudioBuffer) {}

    fn process_events(&mut self, ctx: &ProcessContext, out: &mut NodeEvents) {
        let dt = ctx.frames as f32 / ctx.sample_rate.max(1.0);
        self.update(dt);
        out.control = Some(self.min + (self.max - self.min) * self.level);
    }

    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { .. } => {
                if self.stage == Stage::Idle {
                    self.level = 0.0;
                }
                self.enter(Stage::Attack);
                true
            }
            // A blanket off enters release too: the transport stop must not
            // leave a destination parameter stuck at its last value.
            NoteEvent::Off { .. } => {
                if self.stage != Stage::Idle {
                    self.enter(Stage::Release);
                }
                true
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::EnvAttack => {
                if value.is_finite() {
                    self.attack = value.clamp(0.0, 5.0);
                }
                true
            }
            ParamId::EnvDecay => {
                if value.is_finite() {
                    self.decay = value.clamp(0.0, 5.0);
                }
                true
            }
            ParamId::EnvSustain => {
                if value.is_finite() {
                    self.sustain = value.clamp(0.0, 1.0);
                }
                true
            }
            ParamId::EnvRelease => {
                if value.is_finite() {
                    self.release = value.clamp(0.0, 10.0);
                }
                true
            }
            ParamId::EnvMin => {
                if value.is_finite() {
                    self.min = value.clamp(-127.0, 127.0);
                }
                true
            }
            ParamId::EnvMax => {
                if value.is_finite() {
                    self.max = value.clamp(-127.0, 127.0);
                }
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MidiEnvelopeProcessor;
    use crate::realtime::commands::{NoteEvent, ParamId};
    use crate::realtime::context::ProcessContext;
    use crate::realtime::events::NodeEvents;
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

    fn step(processor: &mut MidiEnvelopeProcessor, blocks: usize) -> f32 {
        let mut control = 0.0;
        for _ in 0..blocks {
            let mut out = NodeEvents::default();
            processor.process_events(&ctx(), &mut out);
            control = out.control.unwrap_or(0.0);
        }
        control
    }

    #[test]
    fn silent_until_a_note_arrives() {
        let mut processor = MidiEnvelopeProcessor::new();
        assert_eq!(step(&mut processor, 4), 0.0);
    }

    #[test]
    fn attack_reaches_full_scale() {
        let mut processor = MidiEnvelopeProcessor::new();
        processor.set_param(ParamId::EnvAttack, 0.1);
        processor.set_param(ParamId::EnvDecay, 0.0);
        processor.set_param(ParamId::EnvSustain, 1.0);
        processor.note_event(note_on(60));
        // 0.1 s / 2.67 ms ≈ 38 blocks.
        let halfway = step(&mut processor, 19);
        assert!(halfway > 0.3 && halfway < 0.7, "halfway = {halfway}");
        let full = step(&mut processor, 25);
        assert!((full - 1.0).abs() < 1e-3, "full = {full}");
    }

    #[test]
    fn decay_falls_to_sustain() {
        let mut processor = MidiEnvelopeProcessor::new();
        processor.set_param(ParamId::EnvAttack, 0.0);
        processor.set_param(ParamId::EnvDecay, 0.1);
        processor.set_param(ParamId::EnvSustain, 0.4);
        processor.note_event(note_on(60));
        // Attack is instant; decay starts immediately (the first block is
        // already slightly below 1.0).
        let first = step(&mut processor, 1);
        assert!(first > 0.9, "first block = {first}");
        let sustained = step(&mut processor, 45);
        assert!((sustained - 0.4).abs() < 1e-3, "sustained = {sustained}");
    }

    #[test]
    fn release_returns_to_zero() {
        let mut processor = MidiEnvelopeProcessor::new();
        processor.set_param(ParamId::EnvAttack, 0.0);
        processor.set_param(ParamId::EnvDecay, 0.0);
        processor.set_param(ParamId::EnvSustain, 0.8);
        processor.set_param(ParamId::EnvRelease, 0.1);
        processor.note_event(note_on(60));
        step(&mut processor, 2);
        processor.note_event(NoteEvent::Off { note: 60 });
        let falling = step(&mut processor, 19);
        assert!(falling > 0.0 && falling < 0.8, "falling = {falling}");
        assert_eq!(step(&mut processor, 30), 0.0);
    }

    #[test]
    fn output_scale_maps_the_normalized_level() {
        let mut processor = MidiEnvelopeProcessor::new();
        processor.set_param(ParamId::EnvAttack, 0.0);
        processor.set_param(ParamId::EnvDecay, 0.0);
        processor.set_param(ParamId::EnvSustain, 1.0);
        processor.set_param(ParamId::EnvMin, 10.0);
        processor.set_param(ParamId::EnvMax, 20.0);
        processor.note_event(note_on(60));
        assert_eq!(step(&mut processor, 2), 20.0);
        processor.set_param(ParamId::EnvSustain, 0.0);
        assert_eq!(step(&mut processor, 2), 10.0);
    }

    #[test]
    fn blanket_off_starts_the_release() {
        let mut processor = MidiEnvelopeProcessor::new();
        processor.set_param(ParamId::EnvAttack, 0.0);
        processor.set_param(ParamId::EnvDecay, 0.0);
        processor.set_param(ParamId::EnvSustain, 1.0);
        processor.set_param(ParamId::EnvRelease, 0.05);
        processor.note_event(note_on(60));
        assert_eq!(step(&mut processor, 1), 1.0);
        processor.note_event(NoteEvent::Off { note: 0 });
        assert!(step(&mut processor, 1) < 1.0);
    }
}
