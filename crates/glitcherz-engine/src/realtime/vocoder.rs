//! 16-band vocoder (spec `.agents/spec/vocoder.md`, Z2).
//!
//! Two band banks, one per audio input. The **modulator** (`Mod` port) is
//! analysed: each band's magnitude drives a one-pole envelope follower with
//! separate attack / release coefficients. The **carrier** (`Main` port) is
//! filtered by an identical bank and multiplied band for band by those
//! envelopes:
//!
//! ```text
//! modulator → BPF[k] → |x| → attack/release → env[k]
//! carrier   → BPF[k] → bp[k] · env[k] ─┐
//!                                      ├→ Σ + air noise → mix
//! modulator → HPF(air_hp) → env_air → noise ─┘
//! ```
//!
//! Three decisions shape the code:
//!
//! - **Bands are `k · v1`, not `v1`.** The raw band-pass tap of the Simper SVF
//!   peaks at `1/k = Q`, so an analysis + synthesis pair would scale with
//!   `Q²`. Normalising both sides to unity peak keeps a full-scale sine
//!   carrier at full scale while `q` only changes selectivity (Z2-D3).
//! - **Envelope one-poles carry a hard floor** ([`ENV_FLOOR`]). In f32 a
//!   one-pole stalls a few 1e-5 above its target; left alone that residue
//!   multiplies the carrier forever and a silent modulator would never reach
//!   silence (Z2-D4).
//! - **`mix = 0` still runs the filters.** It is the E-series sample-exact
//!   passthrough invariant, but short-circuiting the DSP would make the first
//!   block after re-opening the mix a cold start (Z2-D7).
//!
//! The "voiced / unvoiced" half of a vocoder is a noise bypass above the top
//! band rather than a pitch classifier (Z2-D5): a classifier that guesses
//! wrong gates the noise on and off audibly, which sounds worse than not
//! classifying at all.
//!
//! All state is fixed-size and pre-allocated; `process_with_aux` is
//! allocation-, lock- and panic-free.

use std::f32::consts::{PI, SQRT_2};

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::EffectParamBank;
use crate::realtime::effect::descriptor::{EffectDescriptor, ParamSpec, StereoClass, Unit};
use crate::realtime::processor::Processor;

/// Number of analysis / synthesis bands (fixed, Z2-D1).
pub const BANDS: usize = 16;

/// Channels the vocoder keeps state for (`AudioBuffer` is stereo in this
/// pipeline).
const CHANNELS: usize = 2;

/// Slot of the envelope attack time (ms).
pub const SLOT_ATTACK: u8 = 0;
/// Slot of the envelope release time (ms).
pub const SLOT_RELEASE: u8 = 1;
/// Slot of the band Q.
pub const SLOT_Q: u8 = 2;
/// Slot of the lowest band centre (Hz).
pub const SLOT_LOW: u8 = 3;
/// Slot of the highest band centre (Hz).
pub const SLOT_HIGH: u8 = 4;
/// Slot of the sibilance (noise bypass) amount.
pub const SLOT_SIBILANCE: u8 = 5;
/// Slot of the dry/wet mix.
pub const SLOT_MIX: u8 = 6;
/// Slot of the output trim (dB).
pub const SLOT_OUTPUT: u8 = 7;

static VOCODER_PARAMS: [ParamSpec; 8] = [
    ParamSpec::time("attack", 0.5, 100.0, 8.0),
    ParamSpec::time("release", 5.0, 500.0, 60.0),
    ParamSpec::ratio("q", 0.7, 16.0, 4.0),
    ParamSpec::freq("low", 40.0, 1_000.0, 120.0),
    ParamSpec::freq("high", 1_000.0, 16_000.0, 7_000.0),
    ParamSpec::gain("sibilance", Unit::Percent, 0.0, 1.0, 0.25)
        .short("sib")
        .inline_row(),
    ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 1.0).inline_row(),
    ParamSpec::gain("output", Unit::Db, -24.0, 12.0, 0.0).short("out"),
];

/// Static parameter table (spec `.agents/spec/vocoder.md` §4).
pub static DESCRIPTOR: EffectDescriptor = EffectDescriptor {
    name: "Vocoder",
    params: &VOCODER_PARAMS,
    stereo: StereoClass::Mono,
};

/// Envelope floor: below this an envelope snaps to zero (Z2-D4).
const ENV_FLOOR: f32 = 1e-6;
/// Band and air filter states below this are flushed (denormal guard).
const DENORMAL_EPS: f32 = 1e-18;
/// The air (sibilance) high-pass never sits below this frequency, so the
/// bypass only covers what the band bank cannot (Z2-D5).
const AIR_MIN_HZ: f32 = 4_000.0;
/// Fixed air detector timing: fast enough for consonants, slow enough not to
/// chatter (Z2-D5).
const AIR_ATTACK_MS: f32 = 2.0;
/// Air detector release (ms).
const AIR_RELEASE_MS: f32 = 40.0;
/// Fraction of the sample rate the band centres may not reach (Z2-D11).
const NYQUIST_LIMIT: f32 = 0.45;

/// One TPT integrator pair (the two states of a Simper SVF).
#[derive(Clone, Copy, Default)]
struct SvfState {
    ic1: f32,
    ic2: f32,
}

impl SvfState {
    /// Advances one sample and returns `(v1, v2)` — the band-pass and
    /// low-pass taps before scaling. RT-safe.
    #[inline]
    fn step(&mut self, x: f32, coeff: &Coeffs) -> (f32, f32) {
        let v3 = x - self.ic2;
        let v1 = coeff.a1 * self.ic1 + coeff.a2 * v3;
        let v2 = self.ic2 + coeff.a2 * self.ic1 + coeff.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        (v1, v2)
    }

    /// Flushes denormal residue (per block; RT-safe).
    fn flush(&mut self) {
        if self.ic1.abs() < DENORMAL_EPS {
            self.ic1 = 0.0;
        }
        if self.ic2.abs() < DENORMAL_EPS {
            self.ic2 = 0.0;
        }
    }
}

/// Per-block filter coefficients of one band (or the air high-pass).
#[derive(Clone, Copy)]
struct Coeffs {
    a1: f32,
    a2: f32,
    a3: f32,
    /// Damping `k` (`= 1/Q` for a band, `√2` for the air high-pass).
    k: f32,
    /// `false` when the centre is at or above Nyquist: the band is skipped
    /// entirely instead of aliasing (Z2-D11).
    on: bool,
}

impl Coeffs {
    /// Inert coefficients: no frequency computed yet.
    const INERT: Self = Self {
        a1: 1.0,
        a2: 0.0,
        a3: 0.0,
        k: 1.0,
        on: false,
    };

    /// Band-pass coefficients for `freq` with quality factor `q`.
    fn band(freq: f32, q: f32, sample_rate: f32) -> Self {
        Self::with_k(freq, 1.0 / q.max(0.05), sample_rate)
    }

    /// Second-order Butterworth high-pass (used by the air detector).
    fn high_pass(freq: f32, sample_rate: f32) -> Self {
        Self::with_k(freq, SQRT_2, sample_rate)
    }

    fn with_k(freq: f32, k: f32, sample_rate: f32) -> Self {
        if !freq.is_finite() || freq <= 0.0 || freq >= sample_rate * NYQUIST_LIMIT {
            return Self { k, ..Self::INERT };
        }
        // Prewarped TPT gain, same shape as `eq.rs`.
        let g = (PI * freq / sample_rate).tan();
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        Self {
            a1,
            a2,
            a3: g * a2,
            k,
            on: true,
        }
    }
}

/// Attack / release one-pole coefficients of an envelope follower.
#[derive(Clone, Copy)]
struct EnvCoeffs {
    attack: f32,
    release: f32,
}

impl EnvCoeffs {
    fn new(attack_ms: f32, release_ms: f32, sample_rate: f32) -> Self {
        Self {
            attack: one_pole(attack_ms, sample_rate),
            release: one_pole(release_ms, sample_rate),
        }
    }
}

/// One-pole coefficient for a time constant in milliseconds.
fn one_pole(ms: f32, sample_rate: f32) -> f32 {
    let samples = (ms.max(0.01) * 0.001 * sample_rate).max(1.0);
    1.0 - (-1.0 / samples).exp()
}

/// One step of an envelope follower: rises at `attack`, falls at `release`,
/// and snaps to zero below [`ENV_FLOOR`].
#[inline]
fn follow(env: f32, target: f32, coeffs: &EnvCoeffs) -> f32 {
    let coeff = if target > env {
        coeffs.attack
    } else {
        coeffs.release
    };
    let next = env + (target - env) * coeff;
    if next < ENV_FLOOR { 0.0 } else { next }
}

/// xorshift32 → `-1..1` (same generator as `modal.rs`, seeded per channel so
/// the stereo air path is decorrelated and every render is reproducible).
#[inline]
fn next_noise(state: &mut u32) -> f32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    (x as f32 / u32::MAX as f32) * 2.0 - 1.0
}

/// Per-channel DSP state: two band banks, the air detector and its noise seed.
struct ChannelState {
    analysis: [SvfState; BANDS],
    synth: [SvfState; BANDS],
    env: [f32; BANDS],
    air: SvfState,
    air_env: f32,
    noise: u32,
}

impl Default for ChannelState {
    fn default() -> Self {
        Self {
            analysis: [SvfState::default(); BANDS],
            synth: [SvfState::default(); BANDS],
            env: [0.0; BANDS],
            air: SvfState::default(),
            air_env: 0.0,
            noise: 0x9E37_79B9,
        }
    }
}

impl ChannelState {
    fn flush(&mut self) {
        for state in &mut self.analysis {
            state.flush();
        }
        for state in &mut self.synth {
            state.flush();
        }
        self.air.flush();
    }
}

/// 16-band vocoder (Main = carrier, Mod = modulator, zero latency).
pub struct VocoderProcessor {
    sample_rate: f32,
    params: EffectParamBank,
    bands: [Coeffs; BANDS],
    air: Coeffs,
    env: EnvCoeffs,
    air_env: EnvCoeffs,
    channels: [ChannelState; CHANNELS],
    /// Cached `10^(dB/20)` of the output trim, so the per-sample path never
    /// calls `powf` while the smoother sits still.
    output_db: f32,
    output_gain: f32,
}

impl VocoderProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction tolerance).
    pub fn new(sample_rate: f32) -> Self {
        let sr = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        let mut channels = [ChannelState::default(), ChannelState::default()];
        // Decorrelated air noise: identical seeds would collapse the stereo
        // image of the bypass into mono.
        channels[1].noise = 0x85EB_CA6B;
        let mut vocoder = Self {
            sample_rate: sr,
            params: EffectParamBank::new(&DESCRIPTOR, sr),
            bands: [Coeffs::INERT; BANDS],
            air: Coeffs::INERT,
            env: EnvCoeffs::new(8.0, 60.0, sr),
            air_env: EnvCoeffs::new(AIR_ATTACK_MS, AIR_RELEASE_MS, sr),
            channels,
            output_db: f32::NAN,
            output_gain: 1.0,
        };
        vocoder.update_coefficients(1);
        vocoder
    }

    /// Current (settled) value of a slot, for tests and read-back.
    pub fn value(&self, slot: u8) -> f32 {
        self.params.value(slot)
    }

    /// Number of bands (always [`BANDS`]); read by tests and docs.
    pub fn band_count(&self) -> usize {
        BANDS
    }

    /// Linear output trim for `db` (cached).
    fn output_gain(&mut self, db: f32) -> f32 {
        if db != self.output_db {
            self.output_db = db;
            self.output_gain = 10f32.powf(db * 0.05);
        }
        self.output_gain
    }

    /// Recomputes the band table, the air high-pass and the envelope
    /// coefficients for one block (Z2-D1/D5/D13).
    fn update_coefficients(&mut self, frames: usize) {
        let attack = self.params.advance(SLOT_ATTACK, frames);
        let release = self.params.advance(SLOT_RELEASE, frames);
        let q = self.params.advance(SLOT_Q, frames).max(0.05);
        let low = self.params.advance(SLOT_LOW, frames).max(1.0);
        // D13: the two ranges overlap, so a user can drag `high` down to
        // `low`; 16 bands stacked on one centre would resonate hard.
        let high = self.params.advance(SLOT_HIGH, frames).max(low * 2.0);

        self.env = EnvCoeffs::new(attack, release, self.sample_rate);

        let step = (high / low).max(1.0).powf(1.0 / (BANDS - 1) as f32);
        let mut freq = low;
        for band in &mut self.bands {
            *band = Coeffs::band(freq, q, self.sample_rate);
            freq *= step;
        }
        self.air = Coeffs::high_pass(high.max(AIR_MIN_HZ), self.sample_rate);
    }

    /// Runs one sample of the full chain for one channel, returning the wet
    /// signal (before mix and output trim).
    fn wet_sample(&mut self, channel: usize, carrier: f32, modulator: f32, sibilance: f32) -> f32 {
        // Split borrows so the state can be mutated while `self.bands` and the
        // envelope coefficients are read.
        let Self {
            bands,
            air,
            env,
            air_env,
            channels,
            ..
        } = self;
        let Some(state) = channels.get_mut(channel) else {
            return 0.0;
        };
        let mut wet = 0.0;
        for (index, coeff) in bands.iter().enumerate() {
            if !coeff.on {
                // Above Nyquist: drop the band and forget its envelope, or it
                // would pop back with stale energy when it becomes usable.
                state.env[index] = 0.0;
                continue;
            }
            let (v1, _) = state.analysis[index].step(modulator, coeff);
            let target = (coeff.k * v1).abs();
            state.env[index] = follow(state.env[index], target, env);
            let (carrier_v1, _) = state.synth[index].step(carrier, coeff);
            wet += coeff.k * carrier_v1 * state.env[index];
        }
        // The air detector always runs, even at `sibilance = 0`: the skip
        // would leave a stale envelope behind and the next turn-up would
        // start with a burst.
        if air.on {
            let (v1, v2) = state.air.step(modulator, air);
            let hp = modulator - air.k * v1 - v2;
            state.air_env = follow(state.air_env, hp.abs(), air_env);
            if sibilance > 0.0 {
                wet += next_noise(&mut state.noise) * state.air_env * sibilance;
            }
        }
        wet
    }

    fn flush_denormals(&mut self) {
        for state in &mut self.channels {
            state.flush();
            if state.air_env < DENORMAL_EPS {
                state.air_env = 0.0;
            }
        }
    }
}

impl Processor for VocoderProcessor {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        // A single-input graph never supplies aux: the modulator is silent,
        // so every envelope stays at zero (Z2-D2).
        self.process_with_aux(ctx, buffer, None);
    }

    fn process_with_aux(
        &mut self,
        _ctx: &ProcessContext,
        buffer: &mut AudioBuffer,
        aux: Option<&AudioBuffer>,
    ) {
        let frames = buffer.len();
        let channels = buffer.channels().min(CHANNELS);
        if frames == 0 || channels == 0 {
            return;
        }
        self.update_coefficients(frames);

        for index in 0..frames {
            let mix = self.params.tick(SLOT_MIX).clamp(0.0, 1.0);
            let sibilance = self.params.tick(SLOT_SIBILANCE).clamp(0.0, 1.0);
            let output_db = self.params.tick(SLOT_OUTPUT);
            let gain = self.output_gain(output_db);
            for ch in 0..channels {
                let carrier = buffer
                    .channel(ch)
                    .and_then(|channel| channel.get(index).copied())
                    .unwrap_or(0.0);
                let modulator = aux
                    .and_then(|a| a.channel(ch).or_else(|| a.channel(0)))
                    .and_then(|channel| channel.get(index).copied())
                    .unwrap_or(0.0);
                let wet = self.wet_sample(ch, carrier, modulator, sibilance);
                if let Some(sample) = buffer
                    .channel_mut(ch)
                    .and_then(|channel| channel.get_mut(index))
                {
                    // `mix = 0` leaves the sample untouched bit for bit.
                    *sample = carrier * (1.0 - mix) + wet * gain * mix;
                }
            }
        }

        self.flush_denormals();
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::BuiltinParam(slot) => self.params.set(slot, value),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BANDS, Coeffs, DESCRIPTOR, EnvCoeffs, SLOT_ATTACK, SLOT_HIGH, SLOT_LOW, SLOT_MIX,
        SLOT_OUTPUT, SLOT_Q, SLOT_RELEASE, SLOT_SIBILANCE, SvfState, VocoderProcessor,
    };
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::effect::validate_descriptor;
    use crate::realtime::processor::Processor;
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn ctx(frames: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        }
    }

    fn set(proc: &mut VocoderProcessor, slot: u8, value: f32) {
        assert!(proc.set_param(ParamId::BuiltinParam(slot), value));
    }

    fn sine(freq: f32, amp: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| (TAU * freq * i as f32 / SR).sin() * amp)
            .collect()
    }

    /// Deterministic full-band noise, `-amp..amp`.
    fn white(amp: f32, len: usize) -> Vec<f32> {
        let mut seed = 0x1234_5678u32;
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                ((seed as f32 / u32::MAX as f32) * 2.0 - 1.0) * amp
            })
            .collect()
    }

    fn square(freq: f32, amp: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let phase = (i as f32 * freq / SR).fract();
                if phase < 0.5 { amp } else { -amp }
            })
            .collect()
    }

    /// Renders a mono carrier against a mono modulator, block by block.
    fn render(proc: &mut VocoderProcessor, carrier: &[f32], modulator: &[f32]) -> Vec<f32> {
        let mut main = AudioBuffer::new(1, BLOCK);
        let mut aux = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(carrier.len());
        for (offset, chunk) in carrier.chunks(BLOCK).enumerate() {
            let frames = chunk.len();
            let start = offset * BLOCK;
            main.set_len(frames);
            aux.set_len(frames);
            if let Some(channel) = main.channel_mut(0) {
                channel[..frames].copy_from_slice(chunk);
            }
            if let Some(channel) = aux.channel_mut(0) {
                let end = (start + frames).min(modulator.len());
                let available = end.saturating_sub(start);
                channel[..available].copy_from_slice(&modulator[start..end]);
                for sample in &mut channel[available..frames] {
                    *sample = 0.0;
                }
            }
            proc.process_with_aux(&ctx(frames), &mut main, Some(&aux));
            if let Some(channel) = main.channel(0) {
                out.extend_from_slice(&channel[..frames]);
            }
        }
        out
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// Amplitude of `freq` in `samples` (single-bin DFT; no FFT dependency).
    fn goertzel(samples: &[f32], freq: f32) -> f32 {
        let len = samples.len();
        if len == 0 {
            return 0.0;
        }
        let omega = TAU * freq / SR;
        let coeff = 2.0 * omega.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &x in samples {
            let s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        power.max(0.0).sqrt() / (len as f32 * 0.5)
    }

    fn window(samples: &[f32], from_s: f32, to_s: f32) -> &[f32] {
        let start = ((from_s * SR) as usize).min(samples.len());
        let end = ((to_s * SR) as usize).min(samples.len());
        &samples[start..end]
    }

    #[test]
    fn descriptor_is_well_formed() {
        assert_eq!(validate_descriptor(&DESCRIPTOR), Ok(()));
        assert_eq!(DESCRIPTOR.params.len(), 8);
        let inline: Vec<u8> = DESCRIPTOR.inline_slots().collect();
        assert_eq!(inline, vec![SLOT_SIBILANCE, SLOT_MIX], "two canvas rows");
        assert!(!DESCRIPTOR.is_linkable(), "no detector to link");
    }

    #[test]
    fn unity_band_gain_holds_for_every_q() {
        // Z2-D3: `k · v1` peaks at 1.0, so the analysis / synthesis pair does
        // not scale with Q². This is what keeps "more Q" from meaning
        // "louder", and it is the whole reason for the extra `k` multiply.
        for q in [1.0f32, 4.0, 16.0] {
            let coeff = Coeffs::band(1_000.0, q, SR);
            let mut state = SvfState::default();
            let input = sine(1_000.0, 1.0, 4_800);
            let settled: Vec<f32> = input
                .iter()
                .map(|x| {
                    let (v1, _) = state.step(*x, &coeff);
                    coeff.k * v1
                })
                .collect();
            let amplitude = rms(window(&settled, 0.05, 0.09)) * std::f32::consts::SQRT_2;
            assert!(
                (amplitude - 1.0).abs() < 0.05,
                "q = {q}: unity peak expected, got {amplitude}"
            );
        }
    }

    #[test]
    fn silent_modulator_stops_the_output() {
        let mut proc = VocoderProcessor::new(SR);
        let carrier = sine(220.0, 0.8, 4_800);
        let modulator = vec![0.0; carrier.len()];
        let out = render(&mut proc, &carrier, &modulator);
        assert!(
            out.iter().all(|s| *s == 0.0),
            "no modulator means no output at all, not a leak"
        );
    }

    #[test]
    fn missing_aux_is_a_silent_modulator() {
        let mut proc = VocoderProcessor::new(SR);
        let carrier = sine(220.0, 0.8, 1_024);
        let mut buffer = AudioBuffer::new(1, BLOCK);
        buffer.set_len(BLOCK);
        if let Some(channel) = buffer.channel_mut(0) {
            channel.copy_from_slice(&carrier[..BLOCK]);
        }
        proc.process(&ctx(BLOCK), &mut buffer);
        assert!(buffer.channel(0).unwrap().iter().all(|s| *s == 0.0));
    }

    #[test]
    fn dry_mix_is_a_sample_exact_passthrough() {
        let mut proc = VocoderProcessor::new(SR);
        set(&mut proc, SLOT_MIX, 0.0);
        let carrier = sine(440.0, 0.6, 2_048);
        let modulator = white(0.9, carrier.len());
        let out = render(&mut proc, &carrier, &modulator);
        assert_eq!(out, carrier, "mix = 0 must not touch a single bit");
    }

    #[test]
    fn envelope_follows_the_modulator_gate() {
        let mut proc = VocoderProcessor::new(SR);
        set(&mut proc, SLOT_SIBILANCE, 0.0);
        let len = (SR * 0.8) as usize;
        let carrier = sine(1_000.0, 0.7, len);
        // Modulator opens 0.2..0.4 s only.
        let modulator: Vec<f32> = sine(1_000.0, 0.7, len)
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                let t = i as f32 / SR;
                if (0.2..0.4).contains(&t) { s } else { 0.0 }
            })
            .collect();
        let out = render(&mut proc, &carrier, &modulator);
        let before = rms(window(&out, 0.1, 0.19));
        let during = rms(window(&out, 0.25, 0.39));
        let after = rms(window(&out, 0.65, 0.79));
        assert!(
            during > before * 20.0 && during > after * 20.0,
            "gate should open and close: {before} / {during} / {after}"
        );
    }

    #[test]
    fn attack_time_controls_the_onset() {
        let len = (SR * 0.3) as usize;
        let gated: Vec<f32> = sine(1_000.0, 0.7, len)
            .into_iter()
            .enumerate()
            .map(|(i, s)| if i as f32 / SR >= 0.1 { s } else { 0.0 })
            .collect();

        let mut fast = VocoderProcessor::new(SR);
        set(&mut fast, SLOT_ATTACK, 1.0);
        set(&mut fast, SLOT_SIBILANCE, 0.0);
        let fast_out = render(&mut fast, &sine(1_000.0, 0.7, len), &gated);

        let mut slow = VocoderProcessor::new(SR);
        set(&mut slow, SLOT_ATTACK, 50.0);
        set(&mut slow, SLOT_SIBILANCE, 0.0);
        let slow_out = render(&mut slow, &sine(1_000.0, 0.7, len), &gated);

        let fast_onset = rms(window(&fast_out, 0.1, 0.106));
        let slow_onset = rms(window(&slow_out, 0.1, 0.106));
        assert!(
            fast_onset > slow_onset * 3.0,
            "a 1 ms attack must beat a 50 ms one: {fast_onset} vs {slow_onset}"
        );
    }

    #[test]
    fn output_follows_the_modulator_frequency() {
        let mut proc = VocoderProcessor::new(SR);
        set(&mut proc, SLOT_SIBILANCE, 0.0);
        let len = (SR * 0.4) as usize;
        let carrier = white(0.5, len);
        let modulator = sine(300.0, 0.8, len);
        let out = render(&mut proc, &carrier, &modulator);
        let analysis = window(&out, 0.15, 0.4);
        let near = goertzel(analysis, 300.0);
        let far = goertzel(analysis, 3_000.0);
        assert!(
            near > far * 5.0,
            "the bank must track the modulator: 300 Hz {near} vs 3 kHz {far}"
        );
    }

    #[test]
    fn low_and_high_move_the_bank() {
        let len = (SR * 0.3) as usize;
        let carrier = sine(100.0, 0.8, len);
        let modulator = sine(100.0, 0.8, len);

        let mut above = VocoderProcessor::new(SR);
        set(&mut above, SLOT_LOW, 200.0);
        set(&mut above, SLOT_HIGH, 7_000.0);
        set(&mut above, SLOT_SIBILANCE, 0.0);
        let excluded = render(&mut above, &carrier, &modulator);

        let mut covering = VocoderProcessor::new(SR);
        set(&mut covering, SLOT_LOW, 80.0);
        set(&mut covering, SLOT_HIGH, 7_000.0);
        set(&mut covering, SLOT_SIBILANCE, 0.0);
        let included = render(&mut covering, &carrier, &modulator);

        let excluded_rms = rms(window(&excluded, 0.1, 0.3));
        let included_rms = rms(window(&included, 0.1, 0.3));
        assert!(
            included_rms > 0.1,
            "the covered case must actually make sound, got {included_rms}"
        );
        assert!(
            excluded_rms < included_rms * 0.15,
            "100 Hz sits outside a 200 Hz..7 kHz bank: {excluded_rms} vs {included_rms}"
        );
    }

    #[test]
    fn matched_sines_land_near_unity() {
        // Z2-D3's practical consequence: two full-scale sines at a band centre
        // come out around 0.62 (−4 dB), not 0.05 and not 4.0. The missing
        // share is the envelope follower's mean rectification, not a bug.
        let mut proc = VocoderProcessor::new(SR);
        set(&mut proc, SLOT_SIBILANCE, 0.0);
        let len = (SR * 0.3) as usize;
        let carrier = sine(1_000.0, 1.0, len);
        let modulator = sine(1_000.0, 1.0, len);
        let level = rms(window(&render(&mut proc, &carrier, &modulator), 0.1, 0.3));
        assert!(
            (0.45..0.85).contains(&level),
            "unity-ish output expected, got {level}"
        );
    }

    #[test]
    fn q_controls_selectivity() {
        let len = (SR * 0.3) as usize;
        // Carrier at 1 kHz, modulator an octave up: a wide band still leaks
        // it, a narrow one rejects it.
        let carrier = sine(1_000.0, 0.8, len);
        let modulator = sine(2_000.0, 0.8, len);

        let mut wide = VocoderProcessor::new(SR);
        set(&mut wide, SLOT_Q, 1.0);
        set(&mut wide, SLOT_SIBILANCE, 0.0);
        let wide_out = rms(window(&render(&mut wide, &carrier, &modulator), 0.1, 0.3));

        let mut narrow = VocoderProcessor::new(SR);
        set(&mut narrow, SLOT_Q, 16.0);
        set(&mut narrow, SLOT_SIBILANCE, 0.0);
        let narrow_out = rms(window(&render(&mut narrow, &carrier, &modulator), 0.1, 0.3));

        assert!(
            narrow_out < wide_out * 0.5,
            "high Q must reject the off-band modulator: {narrow_out} vs {wide_out}"
        );
    }

    #[test]
    fn sibilance_adds_a_noise_excited_top_end() {
        let len = (SR * 0.4) as usize;
        let carrier = sine(150.0, 0.8, len);
        let modulator = white(0.5, len);

        let mut dry_top = VocoderProcessor::new(SR);
        set(&mut dry_top, SLOT_SIBILANCE, 0.0);
        let quiet = render(&mut dry_top, &carrier, &modulator);

        let mut wet_top = VocoderProcessor::new(SR);
        set(&mut wet_top, SLOT_SIBILANCE, 1.0);
        let noisy = render(&mut wet_top, &carrier, &modulator);

        let quiet_hf = goertzel(window(&quiet, 0.2, 0.4), 9_000.0);
        let noisy_hf = goertzel(window(&noisy, 0.2, 0.4), 9_000.0);
        assert!(
            noisy_hf > quiet_hf * 10.0,
            "the bypass must light up 9 kHz: {quiet_hf} vs {noisy_hf}"
        );
    }

    #[test]
    fn channels_use_their_own_modulator() {
        let mut proc = VocoderProcessor::new(SR);
        set(&mut proc, SLOT_SIBILANCE, 0.0);
        let frames = BLOCK;
        let mut main = AudioBuffer::new(2, BLOCK);
        main.set_len(frames);
        let carrier = sine(1_000.0, 0.7, frames);
        for ch in 0..2 {
            main.channel_mut(ch).unwrap().copy_from_slice(&carrier);
        }
        // Aux: signal on the left, silence on the right.
        let mut aux = AudioBuffer::new(2, BLOCK);
        aux.set_len(frames);
        aux.channel_mut(0).unwrap().copy_from_slice(&carrier);
        aux.channel_mut(1).unwrap().fill(0.0);

        proc.process_with_aux(&ctx(frames), &mut main, Some(&aux));
        let left = rms(main.channel(0).unwrap());
        let right = rms(main.channel(1).unwrap());
        assert!(left > 0.0, "left channel should be vocoded");
        assert_eq!(right, 0.0, "a silent modulator channel stays silent");
    }

    #[test]
    fn extreme_parameters_stay_finite() {
        let mut proc = VocoderProcessor::new(SR);
        set(&mut proc, SLOT_Q, 16.0);
        // D13: `low == high` is legal input; the DSP lifts `high` to 2·low.
        set(&mut proc, SLOT_LOW, 1_000.0);
        set(&mut proc, SLOT_HIGH, 1_000.0);
        set(&mut proc, SLOT_SIBILANCE, 1.0);
        set(&mut proc, SLOT_OUTPUT, 12.0);
        let len = (SR * 0.2) as usize;
        let carrier = square(120.0, 1.0, len);
        let modulator = square(700.0, 1.0, len);
        let out = render(&mut proc, &carrier, &modulator);
        assert!(
            out.iter().all(|s| s.is_finite()),
            "a full-scale square pair must not blow up"
        );
    }

    #[test]
    fn non_finite_parameters_fall_back_to_defaults() {
        let mut proc = VocoderProcessor::new(SR);
        assert!(proc.set_param(ParamId::BuiltinParam(SLOT_Q), f32::NAN));
        assert!(proc.value(SLOT_Q).is_finite(), "NaN must not stick");
        assert!(proc.set_param(ParamId::BuiltinParam(SLOT_MIX), f32::INFINITY));
        let mix = proc.value(SLOT_MIX);
        assert!(mix.is_finite() && (0.0..=1.0).contains(&mix));
        assert!(
            !proc.set_param(ParamId::MasterGain, 1.0),
            "not our parameter"
        );
    }

    #[test]
    fn renders_are_reproducible() {
        // The air path is noise-driven; two instances must still agree sample
        // for sample, or offline renders would stop being byte-stable.
        let len = (SR * 0.1) as usize;
        let carrier = square(180.0, 0.5, len);
        let modulator = white(0.7, len);
        let mut first = VocoderProcessor::new(SR);
        let mut second = VocoderProcessor::new(SR);
        assert_eq!(
            render(&mut first, &carrier, &modulator),
            render(&mut second, &carrier, &modulator)
        );
    }

    #[test]
    fn release_length_changes_the_tail() {
        let len = (SR * 0.6) as usize;
        let carrier = sine(1_000.0, 0.7, len);
        let modulator: Vec<f32> = sine(1_000.0, 0.7, len)
            .into_iter()
            .enumerate()
            .map(|(i, s)| if i as f32 / SR < 0.1 { s } else { 0.0 })
            .collect();

        let mut short = VocoderProcessor::new(SR);
        set(&mut short, SLOT_RELEASE, 5.0);
        set(&mut short, SLOT_SIBILANCE, 0.0);
        let short_tail = rms(window(&render(&mut short, &carrier, &modulator), 0.12, 0.2));

        let mut long = VocoderProcessor::new(SR);
        set(&mut long, SLOT_RELEASE, 500.0);
        set(&mut long, SLOT_SIBILANCE, 0.0);
        let long_tail = rms(window(&render(&mut long, &carrier, &modulator), 0.12, 0.2));

        assert!(
            long_tail > short_tail * 3.0,
            "500 ms release must ring on: {long_tail} vs {short_tail}"
        );
    }

    #[test]
    fn envelope_follower_snaps_to_zero() {
        // Guards `ENV_FLOOR`: a plain one-pole would descend forever.
        let coeffs = EnvCoeffs::new(1.0, 10.0, SR);
        let mut env = 1.0;
        for _ in 0..(SR as usize) {
            env = super::follow(env, 0.0, &coeffs);
        }
        assert_eq!(env, 0.0);
        assert_eq!(BANDS, 16);
    }
}
