//! Plucked-string instrument (spec `.agents/spec/pluck-instrument.md`).
//!
//! Karplus-Strong: one delay line per voice, one one-pole low-pass inside the
//! loop, one noise burst per pluck. The delay length sets the pitch; the loop
//! filter decides how fast the high harmonics die relative to the fundamental;
//! the loop gain is solved from the requested T60.
//!
//! Three properties drive the DSP choices:
//!
//! - **The loop gain is not the T60 gain.** The one-pole attenuates the
//!   fundamental by `|H(f0)| < 1` on every pass, so writing `exp(-ln1000 /
//!   (f0·T60))` straight into the loop would make `decay` a suggestion that
//!   shortens as the user darkens the tone. The gain is divided by `|H(f0)|`
//!   and capped at `0.99995`: a very heavy damper wins over a long `decay`,
//!   which is what a real damper does.
//! - **Note-off damps.** Unlike the modal strike (B1), a plucked part is
//!   played with note lengths, so `Off { note }` starts a `release_s` fade
//!   applied to both the loop and the output — muting only the output would
//!   leave the string ringing for one more loop period (up to 120 ms in the
//!   bass).
//! - **Everything is pre-allocated.** Each voice owns a `MAX_DELAY` ring
//!   (64 KiB at f32) built in [`PluckProcessor::new`]; `process` /
//!   `note_event` / `set_param` only do plain arithmetic over it.

use std::f32::consts::TAU;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;
use crate::realtime::synth::note_to_frequency;

/// Voice budget, matching the other built-in instruments.
pub const MAX_VOICES: usize = 16;

/// Ring capacity per voice in samples. At 48 kHz this reaches 2.9 Hz (below
/// MIDI 0); at 96 kHz it still covers MIDI 0 comfortably (S1-D4).
pub const MAX_DELAY: usize = 16_384;

/// Upper bound of the excitation burst (one loop period, clamped).
const MAX_EXCITATION_FRAMES: usize = 4_096;

/// Mono scratch ceiling, same role as the other generators'.
const SCRATCH_FRAMES: usize = 1024;

/// A voice is freed once its ring energy is below this (−120 dBFS; S1-D5).
const IDLE_EPS: f32 = 1.0e-6;

/// Blanket-stop fade (Stop / Panic), identical to the drum kit / modal K6.
const BLANKET_SECONDS: f32 = 0.01;

/// Pitch-offset bound in semitones, same as the other built-in sources.
const PITCH_OFFSET_LIMIT: f32 = 24.0;

/// `ln(1000)` — T60 → per-period gain conversion.
const LN_1000: f32 = 6.907_755_3;

/// The loop gain can never reach unity; see the module docs (S1-D3).
const GAIN_CEILING: f32 = 0.999_95;

/// Excitation amplitude at full velocity, before the `velocity^1.5` law.
const EXCITATION_LEVEL: f32 = 0.8;

/// One plucked string.
struct Voice {
    /// Circular delay line; its length is the loop's maximum, not the note's.
    ring: Vec<f32>,
    /// Next write position.
    write: usize,
    note: Option<u8>,
    /// Monotonic allocation stamp (steal = smallest).
    age: u64,
    active: bool,
    /// Fractional loop delay in samples (`fs / f0`, clamped).
    delay: f32,
    delay_int: usize,
    delay_frac: f32,
    /// One-pole damping state and coefficient (`y = a·x + (1−a)·y`).
    lp: f32,
    lp_a: f32,
    /// Loop gain for the requested T60 (S1-D3).
    gain: f32,
    /// Damper gain applied **once per loop pass** (`1.0` = undamped). A
    /// per-sample fade would compound every pass and over-damp the string;
    /// one factor per rewrite is exactly one factor per period.
    damp_gain: f32,
    /// Excitation burst cursor / length, amplitude, low-pass state and
    /// coefficient. Frozen at note-on (S1-D6).
    exc_pos: usize,
    exc_len: usize,
    exc_amp: f32,
    exc_lp: f32,
    exc_lp_a: f32,
    /// Peak of what this block wrote into the ring (release detection).
    peak: f32,
}

impl Voice {
    /// Control-thread construction: the ring is the only allocation.
    fn new() -> Self {
        Self {
            ring: vec![0.0; MAX_DELAY],
            write: 0,
            note: None,
            age: 0,
            active: false,
            delay: 2.0,
            delay_int: 2,
            delay_frac: 0.0,
            lp: 0.0,
            lp_a: 1.0,
            gain: 0.0,
            damp_gain: 1.0,
            exc_pos: 0,
            exc_len: 0,
            exc_amp: 0.0,
            exc_lp: 0.0,
            exc_lp_a: 1.0,
            peak: 0.0,
        }
    }
}

/// Plucked-string instrument (generator semantics: the block is overwritten).
pub struct PluckProcessor {
    sample_rate: f32,
    voices: Vec<Voice>,
    /// Mono scratch; one voice renders here per block, then sums into the
    /// output.
    scratch: AudioBuffer,
    stamps: u64,
    decay_s: f32,
    damping: f32,
    brightness: f32,
    release_s: f32,
    gain: f32,
    pitch_ratio: f32,
    /// White-noise state (xorshift32); deterministic for a given event order.
    rng: u32,
    /// Set whenever a coefficient-level parameter changes; the pool is
    /// retuned at the next block boundary (S1-D9).
    tables_dirty: bool,
}

impl PluckProcessor {
    /// Control-thread construction. A non-positive sample rate falls back to
    /// 48 kHz (construction tolerance, not a real-time path).
    pub fn new(sample_rate: f32) -> Self {
        let sr = if sample_rate.is_finite() && sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            sample_rate: sr,
            voices: (0..MAX_VOICES).map(|_| Voice::new()).collect(),
            scratch: AudioBuffer::new(1, SCRATCH_FRAMES),
            stamps: 0,
            decay_s: 4.0,
            damping: 0.35,
            brightness: 0.6,
            release_s: 0.15,
            gain: 1.0,
            pitch_ratio: 1.0,
            rng: 0x2468_ace1,
            tables_dirty: false,
        }
    }

    /// Voices holding energy (sounding or fading). RT-safe.
    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|voice| voice.active).count()
    }

    /// Delay length in samples of the most recently triggered voice (tests).
    pub fn delay_samples(&self) -> f32 {
        self.voices
            .iter()
            .filter(|voice| voice.active)
            .map(|voice| voice.delay)
            .fold(0.0, f32::max)
    }

    fn next_stamp(&mut self) -> u64 {
        self.stamps = self.stamps.wrapping_add(1);
        self.stamps
    }

    /// First silent slot, else the oldest voice (S1-D6).
    fn allocate(&self) -> usize {
        if let Some(index) = self.voices.iter().position(|voice| !voice.active) {
            return index;
        }
        let mut oldest = 0;
        for (i, voice) in self.voices.iter().enumerate() {
            if voice.age < self.voices[oldest].age {
                oldest = i;
            }
        }
        oldest
    }

    /// Loop delay for a note (clamped to the ring), plus the excitation
    /// length for that pitch.
    fn delay_for(&self, note: u8) -> f32 {
        let f0 = note_to_frequency(note) * self.pitch_ratio;
        (self.sample_rate / f0).clamp(2.0, (MAX_DELAY - 2) as f32)
    }

    /// Retunes every sounding voice after a coefficient-level change.
    fn retune_all(&mut self) {
        let damping = self.damping;
        let decay_s = self.decay_s;
        let sample_rate = self.sample_rate;
        let pitch_ratio = self.pitch_ratio;
        for voice in &mut self.voices {
            if !voice.active {
                continue;
            }
            retune(voice, pitch_ratio, damping, decay_s, sample_rate);
        }
    }

    fn trigger(&mut self, note: u8, velocity: f32) {
        let velocity = if velocity.is_finite() {
            velocity.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if velocity <= 0.0 {
            return;
        }
        // Same note already ringing: pluck it again (S1-D6). Otherwise take a
        // free slot or steal the oldest one.
        let index = self
            .voices
            .iter()
            .position(|voice| voice.active && voice.note == Some(note))
            .unwrap_or_else(|| self.allocate());
        let age = self.next_stamp();
        let delay = self.delay_for(note);
        let exc_len = (delay.round() as usize).clamp(1, MAX_EXCITATION_FRAMES);
        let cutoff = (400.0 * (self.brightness * 5.0).exp2() * (0.35 + 0.65 * velocity))
            .min(0.45 * self.sample_rate);
        let exc_lp_a = (1.0 - (-TAU * cutoff / self.sample_rate).exp()).clamp(1.0e-4, 1.0);
        let damping = self.damping;
        let decay_s = self.decay_s;
        let sample_rate = self.sample_rate;
        let pitch_ratio = self.pitch_ratio;
        let voice = &mut self.voices[index];
        voice.note = Some(note);
        voice.age = age;
        voice.active = true;
        voice.delay = delay;
        voice.damp_gain = 1.0;
        voice.exc_pos = 0;
        voice.exc_len = exc_len;
        voice.exc_amp = EXCITATION_LEVEL * velocity.powf(1.5);
        voice.exc_lp = 0.0;
        voice.exc_lp_a = exc_lp_a;
        voice.peak = 1.0;
        // The ring content is deliberately kept: a retrigger adds to the
        // ringing string and a stolen tail glides to the new pitch.
        retune(voice, pitch_ratio, damping, decay_s, sample_rate);
    }

    /// Damps every sounding voice (blanket stop).
    fn damp_all(&mut self, seconds: f32) {
        let sample_rate = self.sample_rate;
        for voice in &mut self.voices {
            if voice.active {
                voice.damp_gain = pass_gain(seconds, voice.delay, sample_rate);
            }
        }
    }
}

impl Processor for PluckProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        if frames == 0 || buffer.channels() == 0 {
            return;
        }
        for ch in 0..buffer.channels() {
            if let Some(dst) = buffer.channel_mut(ch) {
                dst.fill(0.0);
            }
        }
        if self.tables_dirty {
            self.tables_dirty = false;
            self.retune_all();
        }

        let output_gain = self.gain;
        let Self {
            voices,
            scratch,
            rng,
            ..
        } = self;
        scratch.set_len(frames.min(SCRATCH_FRAMES));
        let rendered = scratch.len();
        for voice in voices.iter_mut() {
            if !voice.active {
                continue;
            }
            voice.peak = 0.0;
            scratch.clear();
            if let Some(mono) = scratch.channel_mut(0) {
                render_voice(voice, &mut *rng, mono, output_gain);
            }
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
        // Free the voices whose ring has gone quiet (S1-D5). The damper no
        // longer has a threshold of its own: it works through the ring, so
        // "the ring is empty" covers both an undamped decay and a damped one.
        for voice in voices.iter_mut() {
            if voice.active && voice.exc_pos >= voice.exc_len && voice.peak < IDLE_EPS {
                voice.active = false;
            }
        }
        // Defensive: a host block larger than the scratch stays silent rather
        // than keeping stale samples.
        if rendered < frames {
            for ch in 0..buffer.channels() {
                if let Some(dst) = buffer.channel_mut(ch) {
                    for sample in &mut dst[rendered..] {
                        *sample = 0.0;
                    }
                }
            }
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        if !value.is_finite() {
            return false;
        }
        match id {
            ParamId::PluckDecay => {
                let decay = value.clamp(0.1, 20.0);
                if decay != self.decay_s {
                    self.decay_s = decay;
                    self.tables_dirty = true;
                }
            }
            ParamId::PluckDamping => {
                let damping = value.clamp(0.0, 1.0);
                if damping != self.damping {
                    self.damping = damping;
                    self.tables_dirty = true;
                }
            }
            ParamId::PluckBrightness => self.brightness = value.clamp(0.0, 1.0),
            ParamId::PluckRelease => self.release_s = value.clamp(0.005, 2.0),
            ParamId::PluckGain => self.gain = value.clamp(0.0, 2.0),
            ParamId::PitchOffset => {
                let semitones = value.clamp(-PITCH_OFFSET_LIMIT, PITCH_OFFSET_LIMIT);
                self.pitch_ratio = (semitones / 12.0).exp2();
                self.tables_dirty = true;
            }
            _ => return false,
        }
        true
    }

    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { note, velocity } => self.trigger(note, velocity),
            // A plucked string is played with note lengths: the matching
            // note-off damps it (S1-D5). A different note-off must not.
            NoteEvent::Off { note: 0 } => self.damp_all(BLANKET_SECONDS),
            NoteEvent::Off { note } => {
                let sample_rate = self.sample_rate;
                let seconds = self.release_s;
                for voice in &mut self.voices {
                    if voice.active && voice.note == Some(note) {
                        voice.damp_gain = pass_gain(seconds, voice.delay, sample_rate);
                    }
                }
            }
        }
        true
    }

    /// The pluck engine has no CC surface; consuming the event keeps the graph
    /// quiet (same rationale as the drum kit / modal instrument).
    fn midi_cc(&mut self, _channel: u8, _controller: u8, _value: u8) -> bool {
        true
    }
}

/// Loop attenuation for a −60 dB damper in `seconds`, applied once per pass.
///
/// A string of `delay` samples goes around the loop once per `delay` samples,
/// so the factor is `exp(−ln1000 · delay / (seconds · fs))`. Applying a
/// per-sample fade instead would compound every pass and over-damp the string.
fn pass_gain(seconds: f32, delay: f32, sample_rate: f32) -> f32 {
    let frames = (seconds.max(0.001) * sample_rate).max(1.0);
    (-LN_1000 * delay / frames).exp().clamp(0.0, 1.0)
}

/// Recomputes one voice's loop coefficients from the current parameters.
///
/// The ring content is untouched: retuning a sounding string glides it to the
/// new pitch instead of restarting it (S1-D6). The delay is rebuilt from the
/// note and the current pitch ratio, which is what makes `PitchOffset` reach
/// sounding voices.
fn retune(voice: &mut Voice, pitch_ratio: f32, damping: f32, decay_s: f32, sample_rate: f32) {
    let note = voice.note.unwrap_or(69);
    let f0_hz = note_to_frequency(note) * pitch_ratio;
    voice.delay = (sample_rate / f0_hz).clamp(2.0, (MAX_DELAY - 2) as f32);
    voice.delay_int = voice.delay.floor() as usize;
    voice.delay_frac = voice.delay - voice.delay_int as f32;
    // The clamped delay is the pitch the string will actually play.
    let f0 = sample_rate / voice.delay;

    // Loop low-pass: 32·f0 (bright) down to 4·f0 (dark), never past Nyquist.
    let cutoff = (f0 * (2.0 + 3.0 * (1.0 - damping)).exp2()).min(0.45 * sample_rate);
    let lp_a = (1.0 - (-TAU * cutoff / sample_rate).exp()).clamp(1.0e-4, 1.0);
    voice.lp_a = lp_a;

    // |H(f0)| of that one-pole, so the loop gain compensates for the loss the
    // filter itself introduces at the fundamental (S1-D3).
    let omega = TAU * f0 / sample_rate;
    let (sin_w, cos_w) = omega.sin_cos();
    let b = 1.0 - lp_a;
    let mag = lp_a / ((1.0 - b * cos_w).powi(2) + (b * sin_w).powi(2)).sqrt();
    let target = (-LN_1000 / (f0 * decay_s.max(0.01))).exp();
    voice.gain = (target / mag.max(1.0e-3)).clamp(0.0, GAIN_CEILING);
}

/// Renders one voice into a mono scratch (overwrite).
fn render_voice(voice: &mut Voice, rng: &mut u32, out: &mut [f32], output_gain: f32) {
    let len = voice.ring.len();
    if len < 4 {
        return;
    }
    let i0 = voice.delay_int.max(2).min(len - 2);
    let frac = voice.delay_frac;
    let feed = voice.gain;
    let lp_a = voice.lp_a;
    let lp_b = 1.0 - lp_a;
    let exc_a = voice.exc_lp_a;
    let exc_b = 1.0 - exc_a;
    let exc_amp = voice.exc_amp;
    let mut exc_left = voice.exc_len.saturating_sub(voice.exc_pos);
    let mut peak = voice.peak;

    for sample in out.iter_mut() {
        let w = voice.write;
        // `read` is the value written `delay` samples ago; reading before the
        // write keeps the loop period free of an extra sample.
        let a_index = if w >= i0 { w - i0 } else { w + len - i0 };
        let b_index = if a_index >= 1 { a_index - 1 } else { len - 1 };
        let read = voice.ring[a_index] * (1.0 - frac) + voice.ring[b_index] * frac;

        voice.lp = lp_a * read + lp_b * voice.lp;
        let mut excitation = 0.0;
        if exc_left > 0 {
            let noise = next_noise(rng);
            voice.exc_lp = exc_a * noise + exc_b * voice.exc_lp;
            excitation = voice.exc_lp * exc_amp;
            exc_left -= 1;
            voice.exc_pos += 1;
        }

        let written = voice.lp * feed * voice.damp_gain + excitation;
        voice.ring[w] = written;
        voice.write = if w + 1 >= len { 0 } else { w + 1 };
        if written.abs() > peak {
            peak = written.abs();
        }
        *sample = read * output_gain;
    }
    voice.peak = peak;
}

/// xorshift32 → −1..1. Deterministic and allocation-free.
fn next_noise(state: &mut u32) -> f32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    (x as f32 / u32::MAX as f32) * 2.0 - 1.0
}

#[cfg(test)]
mod tests {
    use super::{MAX_VOICES, PluckProcessor};
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::{NoteEvent, ParamId};
    use crate::realtime::context::ProcessContext;
    use crate::realtime::processor::Processor;
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;
    /// A4 = 440 Hz.
    const A4: u8 = 69;
    /// A2 = 110 Hz: low enough that one loop period is 436 samples, which makes
    /// the fractional delay and the damper arithmetic visible.
    const A2: u8 = 45;

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

    fn blocks_for(seconds: f32) -> usize {
        (seconds * SR / BLOCK as f32).round() as usize
    }

    fn render(processor: &mut PluckProcessor, blocks: usize) -> Vec<f32> {
        let mut buffer = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(blocks * BLOCK);
        for _ in 0..blocks {
            buffer.set_len(BLOCK);
            processor.process(&ctx(), &mut buffer);
            out.extend_from_slice(buffer.channel(0).expect("mono"));
        }
        out
    }

    fn note_on(note: u8, velocity: f32) -> NoteEvent {
        NoteEvent::On { note, velocity }
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// Slice by seconds, clamped to the rendered length.
    fn window(samples: &[f32], from: f32, to: f32) -> &[f32] {
        let start = ((from * SR) as usize).min(samples.len());
        let end = ((to * SR) as usize).min(samples.len());
        &samples[start..end.max(start)]
    }

    /// Goertzel magnitude at `frequency` (comparisons only, not calibrated).
    fn goertzel(samples: &[f32], frequency: f32) -> f32 {
        let omega = TAU * frequency / SR;
        let coeff = 2.0 * omega.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &sample in samples {
            let s0 = sample + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        power.max(0.0).sqrt() / samples.len() as f32
    }

    fn set(processor: &mut PluckProcessor, id: ParamId, value: f32) {
        assert!(processor.set_param(id, value), "{id:?} rejected {value}");
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs()))
    }

    #[test]
    fn silent_without_a_note() {
        let mut pluck = PluckProcessor::new(SR);
        let out = render(&mut pluck, 40);
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(pluck.active_voices(), 0);
    }

    #[test]
    fn a_pluck_produces_sound() {
        let mut pluck = PluckProcessor::new(SR);
        pluck.note_event(note_on(A4, 1.0));
        let out = render(&mut pluck, blocks_for(1.0));
        let attack = rms(window(&out, 0.0, 0.1));
        assert!(
            attack > 0.02 && attack < 0.6,
            "attack RMS {attack} should sit in the instrument range"
        );
        assert!(peak(&out) < 1.0, "one note must not clip: {}", peak(&out));
        // The first loop period is silent by construction (the burst has to
        // travel around once); everything after it is not.
        assert!(
            rms(window(&out, 0.0, 0.002)) < attack * 0.1,
            "the very first samples carry no burst yet"
        );
        assert!(rms(window(&out, 0.01, 0.05)) > attack * 0.5);
    }

    #[test]
    fn pitch_follows_the_note_and_the_offset() {
        let mut pluck = PluckProcessor::new(SR);
        pluck.note_event(note_on(A4, 1.0));
        let out = render(&mut pluck, blocks_for(1.0));
        let span = window(&out, 0.2, 0.8);
        let centre = goertzel(span, 440.0);
        let below = goertzel(span, 415.30);
        let above = goertzel(span, 466.16);
        assert!(
            centre > below * 4.0 && centre > above * 4.0,
            "A4 must land on 440 Hz: centre {centre}, neighbours {below} / {above}"
        );
        assert!(
            (pluck.delay_samples() - SR / 440.0).abs() < 1.0,
            "loop delay should be one period: {}",
            pluck.delay_samples()
        );

        // PitchOffset reaches sounding voices: +12 semitones doubles the loop
        // rate without touching the ring content.
        let mut shifted = PluckProcessor::new(SR);
        set(&mut shifted, ParamId::PitchOffset, 12.0);
        shifted.note_event(note_on(A4, 1.0));
        let out = render(&mut shifted, blocks_for(1.0));
        let span = window(&out, 0.2, 0.8);
        let octave_up = goertzel(span, 880.0);
        let original = goertzel(span, 440.0);
        assert!(
            octave_up > original * 4.0,
            "PitchOffset +12 should sound an octave up: {octave_up} vs {original}"
        );
    }

    #[test]
    fn decay_matches_the_requested_t60() {
        let mut pluck = PluckProcessor::new(SR);
        set(&mut pluck, ParamId::PluckDecay, 2.0);
        set(&mut pluck, ParamId::PluckDamping, 0.0);
        pluck.note_event(note_on(A2, 1.0));
        let out = render(&mut pluck, blocks_for(3.0));
        // Measure the fundamental only: the harmonics carry the damping
        // filter's own decay, the fundamental is what `decay` promises.
        let early = goertzel(window(&out, 0.5, 0.6), 110.0);
        let late = goertzel(window(&out, 1.5, 1.6), 110.0);
        let ratio = early / late.max(1.0e-12);
        // 1 s of a 2 s T60 is −30 dB = ×31.6.
        let measured_t60 = 60.0 / (20.0 * ratio.log10()) * 1.0;
        assert!(
            (measured_t60 - 2.0).abs() < 0.7,
            "requested 2 s, measured {measured_t60:.2} s (ratio {ratio:.1})"
        );
    }

    #[test]
    fn decay_sets_the_tail_length() {
        let tail = |decay: f32| {
            let mut pluck = PluckProcessor::new(SR);
            set(&mut pluck, ParamId::PluckDecay, decay);
            pluck.note_event(note_on(A2, 1.0));
            let out = render(&mut pluck, blocks_for(2.0));
            rms(window(&out, 1.5, 1.6))
        };
        let short = tail(0.3);
        let long = tail(8.0);
        assert!(
            long > short * 10.0,
            "decay 8 s tail {long} should dwarf decay 0.3 s tail {short}"
        );
    }

    #[test]
    fn damping_darkens_the_spectrum() {
        let tilt = |damping: f32| {
            let mut pluck = PluckProcessor::new(SR);
            set(&mut pluck, ParamId::PluckDamping, damping);
            pluck.note_event(note_on(A2, 1.0));
            let out = render(&mut pluck, blocks_for(1.0));
            let span = window(&out, 0.2, 0.6);
            goertzel(span, 440.0) / goertzel(span, 110.0).max(1.0e-12)
        };
        let bright = tilt(0.0);
        let dark = tilt(1.0);
        assert!(
            bright > dark * 5.0,
            "high damping must thin the high harmonics: {bright} vs {dark}"
        );
    }

    #[test]
    fn brightness_changes_the_attack() {
        let high = |brightness: f32| {
            let mut pluck = PluckProcessor::new(SR);
            set(&mut pluck, ParamId::PluckBrightness, brightness);
            pluck.note_event(note_on(A2, 1.0));
            let out = render(&mut pluck, blocks_for(0.15));
            goertzel(window(&out, 0.0, 0.02), 4_000.0)
        };
        let dull = high(0.0);
        let bright = high(1.0);
        assert!(
            bright > dull * 4.0,
            "brightness should raise the 4 kHz content of the attack: {bright} vs {dull}"
        );
    }

    #[test]
    fn note_off_damps_the_string() {
        let mut pluck = PluckProcessor::new(SR);
        pluck.note_event(note_on(A4, 1.0));
        let before = render(&mut pluck, blocks_for(0.3));
        let held = rms(window(&before, 0.2, 0.3));
        assert!(held > 0.0);
        pluck.note_event(NoteEvent::Off { note: A4 });
        let after = render(&mut pluck, blocks_for(0.6));
        let damped = rms(window(&after, 0.25, 0.3));
        assert!(
            damped < held * 0.05,
            "note-off must damp the string: {held} → {damped}"
        );

        // The same note left alone keeps ringing.
        let mut held_on = PluckProcessor::new(SR);
        held_on.note_event(note_on(A4, 1.0));
        let out = render(&mut held_on, blocks_for(0.6));
        let free = rms(window(&out, 0.45, 0.55));
        assert!(
            free > held * 0.2,
            "without note-off the string must still ring: {free}"
        );
    }

    #[test]
    fn blanket_off_stops_everything() {
        let mut pluck = PluckProcessor::new(SR);
        for note in [A2, 57, A4] {
            pluck.note_event(note_on(note, 1.0));
        }
        let _ = render(&mut pluck, blocks_for(0.2));
        pluck.note_event(NoteEvent::Off { note: 0 });
        let out = render(&mut pluck, blocks_for(0.4));
        let tail = rms(window(&out, 0.1, 0.4));
        assert!(tail < 1.0e-4, "blanket stop left {tail} behind");
        assert_eq!(pluck.active_voices(), 0);
    }

    #[test]
    fn retrigger_adds_to_the_ringing_string() {
        let mut pluck = PluckProcessor::new(SR);
        pluck.note_event(note_on(A2, 1.0));
        let first = render(&mut pluck, blocks_for(0.6));
        let ring = rms(window(&first, 0.5, 0.6));
        pluck.note_event(note_on(A2, 1.0));
        let again = render(&mut pluck, blocks_for(0.2));
        let struck = rms(window(&again, 0.02, 0.12));
        assert!(
            struck > ring * 3.0,
            "a retrigger must re-excite the string: {ring} → {struck}"
        );
        assert_eq!(pluck.active_voices(), 1, "one slot per note");
    }

    #[test]
    fn pool_is_bounded_and_stealing_stays_finite() {
        let mut pluck = PluckProcessor::new(SR);
        for index in 0..(MAX_VOICES as u8 + 1) {
            pluck.note_event(note_on(40 + index, 1.0));
        }
        let out = render(&mut pluck, blocks_for(1.0));
        assert!(pluck.active_voices() <= MAX_VOICES);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(peak(&out) < 20.0, "16 voices stacked: {}", peak(&out));
    }

    #[test]
    fn params_reject_non_finite() {
        let mut pluck = PluckProcessor::new(SR);
        for id in [
            ParamId::PluckDecay,
            ParamId::PluckDamping,
            ParamId::PluckBrightness,
            ParamId::PluckRelease,
            ParamId::PluckGain,
            ParamId::PitchOffset,
        ] {
            assert!(!pluck.set_param(id, f32::NAN), "{id:?} accepted NaN");
            assert!(!pluck.set_param(id, f32::INFINITY), "{id:?} accepted ∞");
        }
        // An unknown id is not this instrument's.
        assert!(!pluck.set_param(ParamId::MasterGain, 0.5));
        // Out-of-range values clamp instead of producing a broken voice.
        set(&mut pluck, ParamId::PluckDecay, 1.0e9);
        set(&mut pluck, ParamId::PluckDamping, -5.0);
        set(&mut pluck, ParamId::PluckGain, 1.0e9);
        pluck.note_event(note_on(A4, 1.0));
        let out = render(&mut pluck, blocks_for(0.5));
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn extreme_parameters_stay_finite() {
        for note in [0u8, 127] {
            let mut pluck = PluckProcessor::new(SR);
            set(&mut pluck, ParamId::PluckGain, 2.0);
            set(&mut pluck, ParamId::PluckDamping, 0.0);
            set(&mut pluck, ParamId::PluckDecay, 20.0);
            set(&mut pluck, ParamId::PitchOffset, 24.0);
            pluck.note_event(note_on(note, 1.0));
            let out = render(&mut pluck, blocks_for(1.0));
            assert!(
                out.iter().all(|s| s.is_finite()),
                "note {note} produced non-finite samples"
            );
        }
        // A sample rate low enough that the highest note hits the clamp.
        let mut pluck = PluckProcessor::new(8_000.0);
        pluck.note_event(note_on(120, 1.0));
        let mut buffer = AudioBuffer::new(1, BLOCK);
        for _ in 0..20 {
            buffer.set_len(BLOCK);
            pluck.process(&ctx(), &mut buffer);
            assert!(buffer.channel(0).unwrap().iter().all(|s| s.is_finite()));
        }
    }

    #[test]
    fn zero_velocity_is_silent() {
        let mut pluck = PluckProcessor::new(SR);
        pluck.note_event(note_on(A4, 0.0));
        assert_eq!(pluck.active_voices(), 0);
        let out = render(&mut pluck, 20);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn renders_are_reproducible() {
        let render_once = || {
            let mut pluck = PluckProcessor::new(SR);
            pluck.note_event(note_on(A2, 0.8));
            pluck.note_event(note_on(A4, 0.6));
            let mut out = render(&mut pluck, blocks_for(0.4));
            pluck.note_event(NoteEvent::Off { note: A4 });
            out.extend_from_slice(&render(&mut pluck, blocks_for(0.2)));
            out
        };
        assert_eq!(render_once(), render_once());
    }

    #[test]
    fn constructor_tolerates_a_bad_sample_rate() {
        let mut pluck = PluckProcessor::new(0.0);
        pluck.note_event(note_on(A4, 1.0));
        // The fallback rate is 48 kHz, so the fixture context still matches.
        let out = render(&mut pluck, blocks_for(0.2));
        assert!(rms(&out) > 0.0);
    }
}
