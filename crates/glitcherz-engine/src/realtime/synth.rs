//! Full polyphonic synthesizer: 16 × (oscillator → filter → ADSR envelope).
//!
//! Spec `.agents/spec/instrument.md` D8–D12: the voice pool lives entirely
//! inside this node, so the graph, the parameter protocol and the document
//! format are untouched by the upgrade from one voice to sixteen.

use crate::convert;
use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::envelope::AdsrProcessor;
use crate::realtime::osc::OscProcessor;
use crate::realtime::processor::Processor;
use crate::realtime::svf::SvfProcessor;

/// Maximum simultaneous voices of a built-in synthesizer.
///
/// Matches the sequencer's `MAX_PENDING_OFFS_PER_TRACK` (16): the arrangement
/// can never keep more than 16 long notes alive on one track, and the virtual
/// keyboard shares the ceiling (spec `instrument.md` D8).
pub const MAX_VOICES: usize = 16;

// D8: the pool and the arrangement's pending-off ceiling must not drift apart.
const _: () = assert!(MAX_VOICES == crate::realtime::sequencer::MAX_PENDING_OFFS_PER_TRACK);

/// Upper bound of the mono scratch buffer. The graph's fixed block is 128
/// frames; the slack only protects a larger host block from being truncated.
const SCRATCH_FRAMES: usize = 1024;

/// MIDI note number → frequency (Hz). A4 (69) = 440 Hz.
/// Values are passed from the instrument / UI layer, not a per-sample hot
/// path (computed in f64 then converted to f32 via convert).
pub fn note_to_frequency(note: u8) -> f32 {
    convert::to_f32(440.0 * 2.0f64.powf((note as f64 - 69.0) / 12.0))
}

/// One complete voice: oscillator → state-variable filter → ADSR envelope.
///
/// Each voice owns its phase and filter state, so a chord's notes do not
/// modulate each other.
struct Voice {
    /// MIDI note this voice is playing; `None` once it has been released.
    ///
    /// A released voice keeps its slot until its envelope reaches Idle (the
    /// tail is still audible), which is what makes stealing well defined.
    note: Option<u8>,
    /// Monotonic allocation stamp: the smallest stamp is the oldest voice and
    /// the first candidate for stealing.
    age: u64,
    osc: OscProcessor,
    filter: SvfProcessor,
    env: AdsrProcessor,
}

impl Voice {
    fn new(sample_rate: f32) -> Self {
        let mut osc = OscProcessor::new(sample_rate);
        // The oscillator is unity: the envelope alone shapes the amplitude
        // (velocity → peak → sustain), exactly as in the single-voice days.
        osc.set_param(ParamId::OscGain, 1.0);
        Self {
            note: None,
            age: 0,
            osc,
            filter: SvfProcessor::new(sample_rate),
            env: AdsrProcessor::new(sample_rate),
        }
    }

    /// A slot is free only after its release tail has fully decayed.
    fn is_free(&self) -> bool {
        self.env.is_idle()
    }

    /// Sends the release edge without forgetting the rendered tail.
    fn release(&mut self, note: u8) {
        self.env.note_event(NoteEvent::Off { note });
        self.note = None;
    }

    fn trigger(&mut self, note: u8, velocity: f32, age: u64) {
        self.note = Some(note);
        self.age = age;
        self.osc.set_frequency(note_to_frequency(note));
        self.env.note_event(NoteEvent::On { note, velocity });
    }
}

/// Polyphonic built-in synthesizer.
///
/// Note semantics (spec `instrument.md` D10–D12):
/// - `On { note, velocity }`: every voice already playing `note` is released
///   first (same-note retrigger), then a free slot takes the note — or the
///   oldest voice is stolen when all 16 are busy;
/// - `Off { note }` with `note != 0`: releases the voices playing that note;
/// - `Off { note: 0 }`: blanket release sentinel used by
///   `RtGraph::note_off_all` — releases every voice.
///
/// The filter sits **before** the amp envelope (VCF → VCA, spec D3), and both
/// cutoff / resonance are smoothed inside [`SvfProcessor`].
pub struct SynthProcessor {
    voices: [Voice; MAX_VOICES],
    /// Mono scratch: one voice renders here per block, then sums into the
    /// output. Allocated once (construction is not RT); zero allocation in
    /// `process`.
    scratch: AudioBuffer,
    /// Allocation stamp source; wraps after 2^64 note-ons.
    stamps: u64,
}

impl SynthProcessor {
    /// When `sample_rate <= 0`, internal modules each fall back to 48 kHz
    /// (construction-time tolerance, not on the real-time path).
    pub fn new(sample_rate: f32) -> Self {
        Self {
            voices: std::array::from_fn(|_| Voice::new(sample_rate)),
            scratch: AudioBuffer::new(1, SCRATCH_FRAMES),
            stamps: 0,
        }
    }

    /// Frequency of the most recently allocated voice (Hz).
    ///
    /// Kept for the single-voice tests and for the `OscFrequency` direct
    /// control path; the polyphonic state itself is exposed by
    /// [`Self::active_voices`]. RT-safe.
    pub fn frequency(&self) -> f32 {
        self.voices[self.newest_voice_index()].osc.frequency()
    }

    /// Highest envelope level across the pool (for meters). RT-safe.
    pub fn envelope_level(&self) -> f32 {
        self.voices
            .iter()
            .fold(0.0f32, |peak, voice| peak.max(voice.env.level()))
    }

    /// Number of voices that have not reached Idle yet (sounding + releasing).
    /// RT-safe.
    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|voice| !voice.is_free()).count()
    }

    /// Index of the most recently allocated voice.
    ///
    /// Ties (all stamps equal before the first note) resolve to slot 0 so the
    /// direct-frequency parameter and [`Self::frequency`] stay deterministic.
    fn newest_voice_index(&self) -> usize {
        let mut index = 0;
        for (i, voice) in self.voices.iter().enumerate() {
            if voice.age > self.voices[index].age {
                index = i;
            }
        }
        index
    }

    /// Picks the slot for a new note: the first free voice, otherwise the
    /// oldest one (D10). RT-safe: fixed 16-element scan.
    fn allocate(&self) -> usize {
        if let Some(index) = self.voices.iter().position(Voice::is_free) {
            return index;
        }
        let mut index = 0;
        for (i, voice) in self.voices.iter().enumerate() {
            if voice.age < self.voices[index].age {
                index = i;
            }
        }
        index
    }

    /// Releases every voice currently holding `note` (D11/D12). RT-safe.
    fn release_note(&mut self, note: u8) {
        for voice in &mut self.voices {
            if voice.note == Some(note) {
                voice.release(note);
            }
        }
    }

    /// Next allocation stamp.
    fn next_stamp(&mut self) -> u64 {
        self.stamps = self.stamps.wrapping_add(1);
        self.stamps
    }
}

impl Processor for SynthProcessor {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        if frames == 0 || buffer.channels() == 0 {
            return;
        }
        // The synthesizer is a source: it owns its output entirely.
        for ch in 0..buffer.channels() {
            if let Some(dst) = buffer.channel_mut(ch) {
                dst.fill(0.0);
            }
        }
        let Self {
            voices, scratch, ..
        } = self;
        scratch.set_len(frames);
        let frames = scratch.len();
        for voice in voices.iter_mut() {
            // Idle voices cost nothing: no oscillator, no filter, no envelope.
            if voice.env.is_idle() {
                continue;
            }
            scratch.clear();
            voice.osc.process(ctx, scratch);
            voice.filter.process(ctx, scratch);
            voice.env.process(ctx, scratch);
            let Some(mono) = scratch.channel(0) else {
                continue;
            };
            for ch in 0..buffer.channels() {
                if let Some(dst) = buffer.channel_mut(ch) {
                    for (out, sample) in dst.iter_mut().zip(mono.iter()) {
                        *out += *sample;
                    }
                }
            }
        }
        // Defensive: a host block larger than the scratch stays silent rather
        // than keeping stale samples.
        if frames < buffer.len() {
            for ch in 0..buffer.channels() {
                if let Some(dst) = buffer.channel_mut(ch) {
                    for sample in &mut dst[frames..] {
                        *sample = 0.0;
                    }
                }
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            // D12 note: the direct-frequency control predates the voice pool,
            // so it addresses only the newest voice instead of retuning a
            // whole chord. Every other parameter is broadcast (D9).
            ParamId::OscFrequency => {
                let index = self.newest_voice_index();
                self.voices[index].osc.set_param(id, value)
            }
            ParamId::OscGain | ParamId::OscWaveform | ParamId::PitchOffset => {
                let mut handled = false;
                for voice in &mut self.voices {
                    handled |= voice.osc.set_param(id, value);
                }
                handled
            }
            ParamId::FilterCutoff | ParamId::FilterResonance | ParamId::FilterMode => {
                let mut handled = false;
                for voice in &mut self.voices {
                    handled |= voice.filter.set_param(id, value);
                }
                handled
            }
            ParamId::Attack | ParamId::Decay | ParamId::Sustain | ParamId::Release => {
                let mut handled = false;
                for voice in &mut self.voices {
                    handled |= voice.env.set_param(id, value);
                }
                handled
            }
            _ => false,
        }
    }

    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { note, velocity } => {
                // D11: a retrigger fades the old note out before the new voice
                // starts, so duplicates never stack into +6 dB.
                self.release_note(note);
                let index = self.allocate();
                let age = self.next_stamp();
                self.voices[index].trigger(note, velocity, age);
                true
            }
            NoteEvent::Off { note: 0 } => {
                // D12: `Off{0}` is the blanket sentinel (`note_off_all`).
                for voice in &mut self.voices {
                    if voice.note.is_some() {
                        voice.release(0);
                    }
                }
                true
            }
            NoteEvent::Off { note } => {
                self.release_note(note);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_VOICES, SynthProcessor, note_to_frequency};
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

    fn note_off(note: u8) -> NoteEvent {
        NoteEvent::Off { note }
    }

    /// A flat sustain with no attack/decay, so two solo renders can be summed.
    fn sustained(synth: &mut SynthProcessor) {
        synth.set_param(ParamId::Attack, 0.0);
        synth.set_param(ParamId::Decay, 0.0);
        synth.set_param(ParamId::Sustain, 1.0);
        synth.set_param(ParamId::Release, 0.05);
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
        assert_eq!(synth.active_voices(), 0);
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

        synth.note_event(note_off(60));
        let out = render(&mut synth, 80); // 10240 samples > release 0.2 s
        let tail = &out[out.len() - BLOCK..];
        assert!(
            tail.iter().all(|s| s.abs() < 0.01),
            "should decay to silence after note off"
        );
        assert_eq!(synth.active_voices(), 0);
    }

    // Standard test 2: output contains no NaN/Inf under extreme velocity/frequency.
    #[test]
    fn extreme_params_stay_finite() {
        let mut synth = SynthProcessor::new(SR);
        synth.set_param(ParamId::OscFrequency, SR / 2.0);
        synth.set_param(ParamId::OscWaveform, 3.0);
        synth.set_param(ParamId::FilterCutoff, 20.0);
        synth.set_param(ParamId::FilterResonance, 0.98);
        synth.note_event(NoteEvent::On {
            note: 127,
            velocity: f32::NAN,
        });
        let out = render(&mut synth, 40);
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn params_route_to_osc_filter_and_envelope() {
        let mut synth = SynthProcessor::new(SR);
        assert!(synth.set_param(ParamId::OscFrequency, 220.0));
        assert!((synth.frequency() - 220.0).abs() < 1e-3);
        assert!(synth.set_param(ParamId::OscWaveform, 2.0));
        assert!(synth.set_param(ParamId::FilterCutoff, 1_000.0));
        assert!(synth.set_param(ParamId::FilterResonance, 0.5));
        assert!(synth.set_param(ParamId::FilterMode, 1.0));
        assert!(synth.set_param(ParamId::Attack, 0.5));
        assert!(synth.set_param(ParamId::Release, 0.5));
        assert!(
            !synth.set_param(ParamId::MasterGain, 1.0),
            "master gain is not the synthesizer's responsibility"
        );
    }

    /// Spec `instrument.md` D3: the filter is wired into the voice, so a low
    /// cutoff audibly removes a high note (the old code dropped the parameter).
    #[test]
    fn filter_cutoff_reshapes_the_voice() {
        let peak_of = |cutoff: f32| {
            let mut synth = SynthProcessor::new(SR);
            synth.set_param(ParamId::Attack, 0.0);
            synth.set_param(ParamId::Decay, 0.0);
            synth.set_param(ParamId::Sustain, 1.0);
            synth.set_param(ParamId::FilterCutoff, cutoff);
            synth.set_param(ParamId::FilterResonance, 0.1);
            synth.note_event(note_on(108)); // C8 ≈ 4186 Hz
            let out = render(&mut synth, 40);
            out.iter().fold(0.0f32, |a, s| a.max(s.abs()))
        };
        let open = peak_of(12_000.0);
        let closed = peak_of(200.0);
        assert!(open > 0.8, "open filter should pass the note, peak {open}");
        assert!(
            closed < open * 0.3,
            "200 Hz low-pass must attenuate a 4 kHz note ({closed} vs {open})"
        );
    }

    /// The waveform parameter reaches the oscillator (spec D1).
    #[test]
    fn waveform_is_forwarded_to_the_oscillator() {
        let mut synth = SynthProcessor::new(SR);
        assert!(synth.set_param(ParamId::OscWaveform, 1.0));
        synth.set_param(ParamId::Attack, 0.0);
        synth.set_param(ParamId::Decay, 0.0);
        synth.set_param(ParamId::Sustain, 1.0);
        synth.note_event(note_on(69));
        let out = render(&mut synth, SR as usize / BLOCK);
        let crossings = count_positive_crossings(&out);
        assert!(
            (crossings as i64 - 440).abs() <= 4,
            "triangle crossings = {crossings}"
        );
    }

    /// D8/D10 acceptance: a chord is exactly the sum of its soloed voices —
    /// each voice owns its oscillator and filter state.
    #[test]
    fn a_chord_equals_the_sum_of_its_voices() {
        let solo = |note: u8| {
            let mut synth = SynthProcessor::new(SR);
            sustained(&mut synth);
            synth.note_event(note_on(note));
            render(&mut synth, 40)
        };
        let low = solo(60);
        let high = solo(64);

        let mut chord = SynthProcessor::new(SR);
        sustained(&mut chord);
        chord.note_event(note_on(60));
        chord.note_event(note_on(64));
        assert_eq!(chord.active_voices(), 2);
        let both = render(&mut chord, 40);

        for i in 0..both.len() {
            let expected = low[i] + high[i];
            assert!(
                (both[i] - expected).abs() < 1e-4,
                "sample {i}: chord {} vs solo sum {expected}",
                both[i]
            );
        }
    }

    /// D12 acceptance: `Off{n}` releases only the voices playing `n`.
    #[test]
    fn note_off_releases_only_that_voice() {
        let mut synth = SynthProcessor::new(SR);
        sustained(&mut synth);
        synth.note_event(note_on(60));
        synth.note_event(note_on(72));
        let _ = render(&mut synth, 10);
        assert_eq!(synth.active_voices(), 2);

        synth.note_event(note_off(60));
        let rest = render(&mut synth, 40); // 0.107 s > 0.05 s release
        assert_eq!(synth.active_voices(), 1, "the C5 voice must keep playing");
        let peak = rest.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(peak > 0.5, "the remaining voice should still sound, {peak}");
        let crossings = count_positive_crossings(&rest);
        assert!(
            (crossings as i64 - 56).abs() <= 4, // 72 → C5 ≈ 523 Hz over 0.107 s
            "remaining crossings = {crossings}"
        );

        synth.note_event(note_off(72));
        let tail = render(&mut synth, 80);
        assert_eq!(synth.active_voices(), 0);
        assert!(tail[tail.len() - BLOCK..].iter().all(|s| s.abs() < 0.01));
    }

    /// D11: a retrigger releases the old voice and starts a new one; once the
    /// release tail is gone exactly one voice is left.
    #[test]
    fn same_note_releases_before_retrigger() {
        let mut synth = SynthProcessor::new(SR);
        sustained(&mut synth);
        synth.note_event(note_on(69));
        let _ = render(&mut synth, 5);
        assert_eq!(synth.active_voices(), 1);

        synth.note_event(note_on(69));
        assert_eq!(
            synth.active_voices(),
            2,
            "the old voice releases while the new one attacks"
        );

        let _ = render(&mut synth, 40); // > 0.05 s release
        assert_eq!(
            synth.active_voices(),
            1,
            "the released duplicate must be gone"
        );
    }

    /// D8/D10: the pool never grows past 16 voices; the 17th note steals the
    /// oldest one and the output stays finite.
    #[test]
    fn voice_pool_is_bounded_and_steals() {
        let mut synth = SynthProcessor::new(SR);
        sustained(&mut synth);
        for note in 40..40 + (MAX_VOICES as u8 + 1) {
            synth.note_event(note_on(note));
        }
        assert_eq!(synth.active_voices(), MAX_VOICES);
        let out = render(&mut synth, 20);
        assert!(out.iter().all(|s| s.is_finite()));
    }

    /// D12: `Off{0}` is the blanket sentinel (`note_off_all`).
    #[test]
    fn note_zero_releases_every_voice() {
        let mut synth = SynthProcessor::new(SR);
        sustained(&mut synth);
        for note in [40, 44, 48] {
            synth.note_event(note_on(note));
        }
        assert_eq!(synth.active_voices(), 3);

        synth.note_event(note_off(0));
        let out = render(&mut synth, 80);
        assert_eq!(synth.active_voices(), 0);
        assert!(out[out.len() - BLOCK..].iter().all(|s| s.abs() < 0.01));
    }

    /// The direct-frequency parameter predates the voice pool: it retunes
    /// only the newest voice, leaving a held chord intact.
    #[test]
    fn direct_frequency_retunes_only_the_newest_voice() {
        let mut synth = SynthProcessor::new(SR);
        sustained(&mut synth);
        synth.note_event(note_on(60));
        synth.note_event(note_on(72));
        synth.set_param(ParamId::OscFrequency, 880.0);
        assert!((synth.frequency() - 880.0).abs() < 1e-3);
        // Both notes still sound: the older voice was not retuned away.
        let out = render(&mut synth, 20);
        assert!(out.iter().any(|s| s.abs() > 0.1));
    }
}
