//! Full synthesizer: oscillator → ADSR envelope (single voice).

use crate::convert;
use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::envelope::AdsrProcessor;
use crate::realtime::processor::Processor;
use crate::realtime::sine::SineProcessor;

/// MIDI note number → frequency (Hz). A4 (69) = 440 Hz.
/// Values are passed from the instrument / UI layer, not a per-sample hot
/// path (computed in f64 then converted to f32 via convert).
pub fn note_to_frequency(note: u8) -> f32 {
    convert::to_f32(440.0 * 2.0f64.powf((note as f64 - 69.0) / 12.0))
}

/// Single-voice synthesizer: sine oscillator + ADSR envelope.
///
/// Note semantics (MIDI):
/// - `On { note, velocity }`: the oscillator changes frequency immediately
///   and the envelope enters Attack from the current level (retriggering
///   during release re-attacks);
/// - `Off { .. }`: the envelope enters Release (the single-voice
///   implementation ignores the specific note).
///
/// No built-in filter — the processing chain chains an
/// [`SvfProcessor`](crate::realtime::SvfProcessor) after it.
pub struct SynthProcessor {
    osc: SineProcessor,
    env: AdsrProcessor,
}

impl SynthProcessor {
    /// When `sample_rate <= 0`, internal modules each fall back to 48 kHz
    /// (construction-time tolerance).
    ///
    /// The oscillator gain is normalized to 1: the voice amplitude is
    /// entirely determined by the envelope (velocity → sustain); the
    /// reverberant main gain / filter sit downstream in the chain.
    pub fn new(sample_rate: f32) -> Self {
        let mut osc = SineProcessor::new(sample_rate);
        osc.set_param(ParamId::SineGain, 1.0);
        Self {
            osc,
            env: AdsrProcessor::new(sample_rate),
        }
    }

    /// Current oscillator frequency (Hz). RT-safe.
    pub fn frequency(&self) -> f32 {
        self.osc.frequency()
    }

    /// Current envelope level (for meters / visualization). RT-safe.
    pub fn envelope_level(&self) -> f32 {
        self.env.level()
    }
}

impl Processor for SynthProcessor {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        self.osc.process(ctx, buffer);
        self.env.process(ctx, buffer);
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::SineFrequency | ParamId::SineGain | ParamId::PitchOffset => {
                self.osc.set_param(id, value)
            }
            ParamId::Attack | ParamId::Decay | ParamId::Sustain | ParamId::Release => {
                self.env.set_param(id, value)
            }
            _ => false,
        }
    }

    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { note, velocity } => {
                self.osc.set_frequency(note_to_frequency(note));
                self.env.note_event(NoteEvent::On { note, velocity })
            }
            NoteEvent::Off { .. } => self.env.note_event(event),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SynthProcessor, note_to_frequency};
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::{NoteEvent, ParamId};
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;
    use crate::testing::count_positive_crossings;

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

    fn render(synth: &mut SynthProcessor, blocks: usize) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::new();
        for _ in 0..blocks {
            buf.set_len(BLOCK);
            buf.clear();
            synth.process(&ctx(), &mut buf);
            out.extend_from_slice(buf.channel(0).unwrap());
        }
        out
    }

    fn note_on(note: u8) -> NoteEvent {
        NoteEvent::On {
            note,
            velocity: 1.0,
        }
    }

    #[test]
    fn note_to_frequency_table() {
        assert!((note_to_frequency(69) - 440.0).abs() < 0.01, "A4 = 440 Hz");
        assert!(
            (note_to_frequency(60) - 261.63).abs() < 0.05,
            "C4 ≈ 261.63 Hz"
        );
        assert!((note_to_frequency(81) - 880.0).abs() < 0.02, "A5 = 880 Hz");
    }

    // Standard test 1: output is silent without a note.
    #[test]
    fn silent_without_note() {
        let mut synth = SynthProcessor::new(SR);
        let out = render(&mut synth, 20);
        assert!(out.iter().all(|s| s.abs() < 1e-12));
    }

    #[test]
    fn note_on_plays_note_frequency() {
        let mut synth = SynthProcessor::new(SR);
        synth.set_param(ParamId::Attack, 0.0);
        synth.set_param(ParamId::Decay, 0.0);
        synth.set_param(ParamId::Sustain, 0.8);
        assert!(
            synth.note_event(note_on(69)),
            "synthesizer should consume the note event"
        );

        // Skipping attack does not matter — A=0 goes straight to sustain;
        // 1 second @ A4 → 440 zero crossings.
        let out = render(&mut synth, SR as usize / BLOCK);
        let crossings = count_positive_crossings(&out);
        assert!(
            (crossings as i64 - 440).abs() <= 2,
            "crossings = {crossings}"
        );
        let peak = out.iter().fold(0.0f32, |a, s| a.max(*s));
        assert!(
            (peak - 0.8).abs() < 0.05,
            "sustain level should be about 0.8, got {peak}"
        );
    }

    #[test]
    fn note_off_fades_to_silence() {
        let mut synth = SynthProcessor::new(SR);
        synth.set_param(ParamId::Attack, 0.0);
        synth.set_param(ParamId::Decay, 0.0);
        synth.set_param(ParamId::Sustain, 0.8);
        synth.set_param(ParamId::Release, 0.2);
        synth.note_event(note_on(60));
        let _ = render(&mut synth, 20);

        synth.note_event(NoteEvent::Off { note: 60 });
        let out = render(&mut synth, 80); // 10240 samples > release 0.2 s
        let tail = &out[out.len() - BLOCK..];
        assert!(
            tail.iter().all(|s| s.abs() < 0.01),
            "should decay to silence after note off"
        );
    }

    // Standard test 2: output contains no NaN/Inf under extreme velocity/frequency.
    #[test]
    fn extreme_params_stay_finite() {
        let mut synth = SynthProcessor::new(SR);
        synth.set_param(ParamId::SineFrequency, SR / 2.0);
        synth.note_event(NoteEvent::On {
            note: 127,
            velocity: f32::NAN,
        });
        let out = render(&mut synth, 40);
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn params_route_to_osc_and_envelope() {
        let mut synth = SynthProcessor::new(SR);
        assert!(synth.set_param(ParamId::SineFrequency, 220.0));
        assert!((synth.frequency() - 220.0).abs() < 1e-3);
        assert!(synth.set_param(ParamId::Attack, 0.5));
        assert!(synth.set_param(ParamId::Release, 0.5));
        assert!(
            !synth.set_param(ParamId::MasterGain, 1.0),
            "master gain is not the synthesizer's responsibility"
        );
        assert!(
            !synth.set_param(ParamId::FilterCutoff, 1000.0),
            "the filter is not the synthesizer's responsibility"
        );
    }
}
