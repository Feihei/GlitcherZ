//! Three-band EQ with a low cut (spec `.agents/spec/effects.md` E2a).
//!
//! Signal path, in order: **low shelf → bell → high shelf → low cut**.
//!
//! - The shelves are first-order TPT one-poles (Zavalishin), shaped into a
//!   shelf by `y = x + (A − 1)·LP(x)` (low) and `y = x + (A − 1)·(x − LP(x))`
//!   (high). At DC the low shelf reaches `A` and the high shelf stays at 1; at
//!   Nyquist it is the other way round.
//! - The bell is the Simper SVF's band-pass output scaled to unity at the
//!   center: `y = x + (A − 1)·BP`. The SVF damping is `k = 1/Q`, so `mid_q`
//!   maps directly onto the resonance users expect.
//! - The low cut reuses the same SVF with a fixed Butterworth damping
//!   (`k = √2`) and the high-pass output.
//!
//! **Flat is a sample-exact passthrough.** With all three gains at 0 dB
//! `A − 1` is exactly zero, so every stage returns its input bit for bit; with
//! `low_cut` at its 20 Hz minimum the cut stage is bypassed (it still runs, so
//! switching it back on cannot click — see [`EqProcessor::process`]).
//!
//! All state is pre-allocated; `process` is allocation-, lock- and panic-free.

use std::f32::consts::{PI, SQRT_2};

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::effect::EffectParamBank;
use crate::realtime::effect::descriptor::{EffectDescriptor, ParamSpec, StereoClass, Unit};
use crate::realtime::processor::Processor;

/// Slot of the low-shelf gain (dB).
pub const SLOT_LOW_GAIN: u8 = 0;
/// Slot of the low-shelf corner frequency.
pub const SLOT_LOW_FREQ: u8 = 1;
/// Slot of the bell gain (dB).
pub const SLOT_MID_GAIN: u8 = 2;
/// Slot of the bell center frequency.
pub const SLOT_MID_FREQ: u8 = 3;
/// Slot of the bell Q.
pub const SLOT_MID_Q: u8 = 4;
/// Slot of the high-shelf gain (dB).
pub const SLOT_HIGH_GAIN: u8 = 5;
/// Slot of the high-shelf corner frequency.
pub const SLOT_HIGH_FREQ: u8 = 6;
/// Slot of the low-cut corner frequency.
pub const SLOT_LOW_CUT: u8 = 7;

/// `low_cut` at this value means "no low cut" (spec E2a: no separate switch).
pub const LOW_CUT_OFF_HZ: f32 = 20.0;

static EQ_PARAMS: [ParamSpec; 8] = [
    ParamSpec::gain("low_gain", Unit::Db, -18.0, 18.0, 0.0)
        .short("low")
        .inline_row(),
    ParamSpec::freq("low_freq", 30.0, 500.0, 100.0),
    ParamSpec::gain("mid_gain", Unit::Db, -18.0, 18.0, 0.0)
        .short("mid")
        .inline_row(),
    ParamSpec::freq("mid_freq", 200.0, 5_000.0, 1_000.0),
    ParamSpec::ratio("mid_q", 0.3, 8.0, 1.0),
    ParamSpec::gain("high_gain", Unit::Db, -18.0, 18.0, 0.0)
        .short("hi")
        .inline_row(),
    ParamSpec::freq("high_freq", 1_500.0, 16_000.0, 8_000.0),
    ParamSpec::freq("low_cut", LOW_CUT_OFF_HZ, 500.0, LOW_CUT_OFF_HZ)
        .short("cut")
        .inline_row(),
];

/// Static parameter table (spec `.agents/spec/effects.md` E2a).
pub static DESCRIPTOR: EffectDescriptor = EffectDescriptor {
    name: "EQ",
    params: &EQ_PARAMS,
    stereo: StereoClass::Mono,
};

/// Below this the filter states are flushed (keeps denormals out of the loop).
const DENORMAL_EPS: f32 = 1e-18;
/// Lowest frequency any stage may be tuned to.
const MIN_HZ: f32 = 20.0;

/// Per-channel filter state: two one-poles (shelves) and two SVFs (bell, cut).
#[derive(Clone, Copy, Default)]
struct ChannelState {
    low_shelf: f32,
    bell_ic1: f32,
    bell_ic2: f32,
    high_shelf: f32,
    cut_ic1: f32,
    cut_ic2: f32,
}

impl ChannelState {
    /// Flushes denormal residue in every integrator.
    fn flush(&mut self) {
        for state in [
            &mut self.low_shelf,
            &mut self.bell_ic1,
            &mut self.bell_ic2,
            &mut self.high_shelf,
            &mut self.cut_ic1,
            &mut self.cut_ic2,
        ] {
            if state.abs() < DENORMAL_EPS {
                *state = 0.0;
            }
        }
    }
}

/// Per-block coefficients (everything the sample loop needs).
#[derive(Clone, Copy)]
struct Coefficients {
    /// One-pole `G = g/(1+g)` of the low shelf.
    low_g: f32,
    /// One-pole `G` of the high shelf.
    high_g: f32,
    bell_a1: f32,
    bell_a2: f32,
    bell_a3: f32,
    bell_k: f32,
    cut_a1: f32,
    cut_a2: f32,
    cut_a3: f32,
    cut_k: f32,
}

impl Default for Coefficients {
    fn default() -> Self {
        // Everything at the "flat" tuning; `update` overwrites it on the first
        // block anyway, but a sane identity keeps `new` total.
        let mut coeffs = Self {
            low_g: 0.0,
            high_g: 0.0,
            bell_a1: 1.0,
            bell_a2: 0.0,
            bell_a3: 0.0,
            bell_k: 1.0,
            cut_a1: 1.0,
            cut_a2: 0.0,
            cut_a3: 0.0,
            cut_k: SQRT_2,
        };
        coeffs.update(100.0, 1_000.0, 1.0, 8_000.0, LOW_CUT_OFF_HZ, 48_000.0);
        coeffs
    }
}

/// Prewarped TPT gain `g = tan(π·fc/sr)`, clamped below Nyquist.
fn tpt_g(freq: f32, sample_rate: f32) -> f32 {
    let fc = freq.clamp(MIN_HZ, sample_rate * 0.45);
    (PI * fc / sample_rate).tan()
}

/// SVF integration coefficients for prewarped `g` and damping `k`.
fn svf_coeffs(g: f32, k: f32) -> (f32, f32, f32) {
    let a1 = 1.0 / (1.0 + g * (g + k));
    let a2 = g * a1;
    (a1, a2, g * a2)
}

impl Coefficients {
    fn update(
        &mut self,
        low_freq: f32,
        mid_freq: f32,
        mid_q: f32,
        high_freq: f32,
        low_cut: f32,
        sample_rate: f32,
    ) {
        let low_g = tpt_g(low_freq, sample_rate);
        self.low_g = low_g / (1.0 + low_g);
        let high_g = tpt_g(high_freq, sample_rate);
        self.high_g = high_g / (1.0 + high_g);

        // `mid_q` is clamped by the spec range, so `k` stays in 0.125..3.33.
        self.bell_k = 1.0 / mid_q.clamp(0.3, 8.0);
        let (a1, a2, a3) = svf_coeffs(tpt_g(mid_freq, sample_rate), self.bell_k);
        self.bell_a1 = a1;
        self.bell_a2 = a2;
        self.bell_a3 = a3;

        self.cut_k = SQRT_2;
        let (a1, a2, a3) = svf_coeffs(tpt_g(low_cut, sample_rate), self.cut_k);
        self.cut_a1 = a1;
        self.cut_a2 = a2;
        self.cut_a3 = a3;
    }
}

/// dB → linear, cached against the last input.
///
/// The smoothers return a constant once they settle, so in steady state this
/// skips the `powf` for every sample of every block.
#[derive(Clone, Copy)]
struct DbGain {
    db: f32,
    linear: f32,
}

impl DbGain {
    fn new() -> Self {
        // NaN never compares equal, so the first `update` always computes.
        Self {
            db: f32::NAN,
            linear: 1.0,
        }
    }

    fn update(&mut self, db: f32) -> f32 {
        if db != self.db {
            self.db = db;
            self.linear = 10f32.powf(db * 0.05);
        }
        self.linear
    }
}

/// Three-band EQ with a low cut (1 in, 1 out, zero latency).
pub struct EqProcessor {
    sample_rate: f32,
    params: EffectParamBank,
    coefficients: Coefficients,
    states: [ChannelState; 2],
    low_gain: DbGain,
    mid_gain: DbGain,
    high_gain: DbGain,
}

impl EqProcessor {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction tolerance).
    pub fn new(sample_rate: f32) -> Self {
        let sr = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            sample_rate: sr,
            params: EffectParamBank::new(&DESCRIPTOR, sr),
            coefficients: Coefficients::default(),
            states: [ChannelState::default(); 2],
            low_gain: DbGain::new(),
            mid_gain: DbGain::new(),
            high_gain: DbGain::new(),
        }
    }

    /// Current (settled) value of a slot, for tests and read-back.
    pub fn value(&self, slot: u8) -> f32 {
        self.params.value(slot)
    }

    /// Processes one sample through all four stages. RT-safe.
    fn step(
        state: &mut ChannelState,
        x: f32,
        coeffs: &Coefficients,
        low_a: f32,
        mid_a: f32,
        high_a: f32,
        cut_on: bool,
    ) -> f32 {
        // 1. Low shelf: `A` at DC, unity at Nyquist.
        let v = (x - state.low_shelf) * coeffs.low_g;
        let low_lp = v + state.low_shelf;
        state.low_shelf = low_lp + v;
        let mut y = x + (low_a - 1.0) * low_lp;

        // 2. Bell: unity at DC and Nyquist, `A` at the center.
        let v3 = y - state.bell_ic2;
        let bell_v1 = coeffs.bell_a1 * state.bell_ic1 + coeffs.bell_a2 * v3;
        let bell_v2 = state.bell_ic2 + coeffs.bell_a2 * state.bell_ic1 + coeffs.bell_a3 * v3;
        state.bell_ic1 = 2.0 * bell_v1 - state.bell_ic1;
        state.bell_ic2 = 2.0 * bell_v2 - state.bell_ic2;
        y += (mid_a - 1.0) * (bell_v1 * coeffs.bell_k);

        // 3. High shelf: unity at DC, `A` at Nyquist.
        let v = (y - state.high_shelf) * coeffs.high_g;
        let high_lp = v + state.high_shelf;
        state.high_shelf = high_lp + v;
        y += (high_a - 1.0) * (y - high_lp);

        // 4. Low cut (Butterworth high-pass). Always integrated, but only
        // applied above the "off" position, so turning it on has no transient.
        let v3 = y - state.cut_ic2;
        let cut_v1 = coeffs.cut_a1 * state.cut_ic1 + coeffs.cut_a2 * v3;
        let cut_v2 = state.cut_ic2 + coeffs.cut_a2 * state.cut_ic1 + coeffs.cut_a3 * v3;
        state.cut_ic1 = 2.0 * cut_v1 - state.cut_ic1;
        state.cut_ic2 = 2.0 * cut_v2 - state.cut_ic2;
        if cut_on {
            y - coeffs.cut_k * cut_v1 - cut_v2
        } else {
            y
        }
    }
}

impl Processor for EqProcessor {
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        let channels = buffer.channels().min(2);
        if frames == 0 || channels == 0 {
            return;
        }
        // Block-rate parameters (D11): one step per block.
        let low_freq = self.params.advance(SLOT_LOW_FREQ, frames);
        let mid_freq = self.params.advance(SLOT_MID_FREQ, frames);
        let mid_q = self.params.advance(SLOT_MID_Q, frames);
        let high_freq = self.params.advance(SLOT_HIGH_FREQ, frames);
        let low_cut = self.params.advance(SLOT_LOW_CUT, frames);
        self.coefficients.update(
            low_freq,
            mid_freq,
            mid_q,
            high_freq,
            low_cut,
            self.sample_rate,
        );
        // A hair above the minimum keeps float noise from flickering the stage.
        let cut_on = low_cut > LOW_CUT_OFF_HZ * 1.001;
        // Copied out of `self` so the channel loop only borrows `states` and
        // `params` (nothing re-reads the coefficients per sample).
        let coeffs = self.coefficients;

        for index in 0..frames {
            // Gains are per-sample parameters, but shared by both channels:
            // tick once per frame so stereo does not double the ramp.
            let low_a = self.low_gain.update(self.params.tick(SLOT_LOW_GAIN));
            let mid_a = self.mid_gain.update(self.params.tick(SLOT_MID_GAIN));
            let high_a = self.high_gain.update(self.params.tick(SLOT_HIGH_GAIN));
            for ch in 0..channels {
                let Some(channel) = buffer.channel_mut(ch) else {
                    continue;
                };
                let Some(state) = self.states.get_mut(ch) else {
                    continue;
                };
                let Some(sample) = channel.get_mut(index) else {
                    continue;
                };
                *sample = Self::step(state, *sample, &coeffs, low_a, mid_a, high_a, cut_on);
            }
        }
        for state in &mut self.states[..channels] {
            state.flush();
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::BuiltinParam(slot) => self.params.set(slot, value),
            _ => false,
        }
    }

    fn latency_samples(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::{
        DESCRIPTOR, EqProcessor, LOW_CUT_OFF_HZ, SLOT_HIGH_FREQ, SLOT_HIGH_GAIN, SLOT_LOW_CUT,
        SLOT_LOW_FREQ, SLOT_LOW_GAIN, SLOT_MID_FREQ, SLOT_MID_GAIN, SLOT_MID_Q,
    };
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::effect::validate_descriptor;
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

    fn run_through(eq: &mut EqProcessor, signal: &[f32]) -> Vec<f32> {
        let mut buf = AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(BLOCK) {
            buf.set_len(chunk.len());
            buf.channel_mut(0).unwrap().copy_from_slice(chunk);
            eq.process(&ctx(), &mut buf);
            out.extend_from_slice(buf.channel(0).unwrap());
        }
        out
    }

    fn sine(freq: f32, seconds: f32) -> Vec<f32> {
        let n = (SR * seconds) as usize;
        (0..n).map(|i| (TAU * freq * i as f32 / SR).sin()).collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| *s * *s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// Window used for gain measurements, in samples.
    ///
    /// One block is far too short at low frequencies: 128 samples is 0.16 of a
    /// 60 Hz period, and the RMS of a partial cycle can be off by 3 dB. 4096
    /// samples covers 4+ cycles down to 50 Hz.
    const TAIL: usize = 4_096;

    /// Gain in dB over the last [`TAIL`] samples, i.e. after the filters have
    /// settled.
    fn tail_gain_db(input: &[f32], output: &[f32]) -> f32 {
        let from = output.len().saturating_sub(TAIL);
        20.0 * (rms(&output[from..]) / rms(&input[from..])).log10()
    }

    fn set(eq: &mut EqProcessor, slot: u8, value: f32) {
        assert!(
            eq.set_param(ParamId::BuiltinParam(slot), value),
            "slot {slot} must be accepted"
        );
    }

    #[test]
    fn descriptor_is_well_formed() {
        assert_eq!(validate_descriptor(&DESCRIPTOR), Ok(()));
        assert_eq!(DESCRIPTOR.inline_slots().count(), 4);
    }

    /// The flat setting (`A − 1 == 0`, low cut off) is a bit-exact passthrough.
    #[test]
    fn flat_settings_pass_through_exactly() {
        let mut eq = EqProcessor::new(SR);
        let input = sine(440.0, 0.2);
        let out = run_through(&mut eq, &input);
        assert_eq!(out, input, "flat EQ must not touch a single sample");
    }

    #[test]
    fn silence_in_silence_out() {
        let mut eq = EqProcessor::new(SR);
        set(&mut eq, SLOT_LOW_GAIN, 12.0);
        set(&mut eq, SLOT_MID_GAIN, -18.0);
        set(&mut eq, SLOT_HIGH_GAIN, 18.0);
        set(&mut eq, SLOT_LOW_CUT, 400.0);
        let out = run_through(&mut eq, &vec![0.0; BLOCK * 16]);
        assert!(out.iter().all(|s| s.abs() < 1e-9));
    }

    #[test]
    fn full_scale_stays_finite_with_extreme_settings() {
        let mut eq = EqProcessor::new(SR);
        set(&mut eq, SLOT_LOW_GAIN, 18.0);
        set(&mut eq, SLOT_MID_GAIN, 18.0);
        set(&mut eq, SLOT_HIGH_GAIN, 18.0);
        set(&mut eq, SLOT_MID_Q, 8.0);
        set(&mut eq, SLOT_MID_FREQ, SR * 0.4);
        let input = sine(1_000.0, 0.5);
        let out = run_through(&mut eq, &input);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(out.iter().fold(0.0f32, |a, s| a.max(s.abs())) < 32.0);
    }

    #[test]
    fn low_shelf_boosts_the_bass_and_leaves_the_treble_alone() {
        let mut eq = EqProcessor::new(SR);
        set(&mut eq, SLOT_LOW_GAIN, 12.0);
        set(&mut eq, SLOT_LOW_FREQ, 100.0);

        // Two octaves below the corner the shelf is within a fraction of a dB
        // of its full +12 dB (a first-order shelf, so it approaches A gently).
        let low = sine(25.0, 1.0);
        let out = run_through(&mut eq, &low);
        let gain = tail_gain_db(&low, &out);
        assert!(
            (11.0..12.5).contains(&gain),
            "25 Hz should reach most of the +12 dB shelf, got {gain:.2} dB"
        );

        let high = sine(5_000.0, 0.5);
        let out = run_through(&mut eq, &high);
        let gain = tail_gain_db(&high, &out);
        assert!(gain.abs() < 0.5, "5 kHz must stay flat, got {gain:.2} dB");
    }

    #[test]
    fn bell_peaks_at_its_center_frequency() {
        let mut eq = EqProcessor::new(SR);
        set(&mut eq, SLOT_MID_GAIN, 12.0);
        set(&mut eq, SLOT_MID_FREQ, 1_000.0);
        set(&mut eq, SLOT_MID_Q, 1.0);

        let center = sine(1_000.0, 0.5);
        let out = run_through(&mut eq, &center);
        let gain = tail_gain_db(&center, &out);
        assert!(
            (11.0..13.0).contains(&gain),
            "the bell should hit +12 dB at its center, got {gain:.2} dB"
        );

        let far = sine(6_000.0, 0.5);
        let out = run_through(&mut eq, &far);
        let gain = tail_gain_db(&far, &out);
        assert!(
            gain.abs() < 1.5,
            "6 kHz with Q=1 must be near unity, got {gain:.2} dB"
        );
    }

    #[test]
    fn high_shelf_cuts_the_treble_and_leaves_the_bass_alone() {
        let mut eq = EqProcessor::new(SR);
        set(&mut eq, SLOT_HIGH_GAIN, -12.0);
        set(&mut eq, SLOT_HIGH_FREQ, 3_000.0);

        // Two octaves above the corner. A first-order shelf transitions at
        // 6 dB/oct, so it reaches roughly three quarters of the shelf here —
        // the full -12 dB only appears well above the corner.
        let high = sine(12_000.0, 0.5);
        let out = run_through(&mut eq, &high);
        let gain = tail_gain_db(&high, &out);
        assert!(
            (-11.0..-8.5).contains(&gain),
            "12 kHz should be well down the -12 dB shelf, got {gain:.2} dB"
        );

        let low = sine(200.0, 0.5);
        let out = run_through(&mut eq, &low);
        let gain = tail_gain_db(&low, &out);
        assert!(gain.abs() < 0.5, "200 Hz must stay flat, got {gain:.2} dB");
    }

    #[test]
    fn low_cut_at_the_minimum_passes_and_above_it_blocks() {
        let bass = sine(50.0, 0.5);

        let mut open = EqProcessor::new(SR);
        let out = run_through(&mut open, &bass);
        let gain = tail_gain_db(&bass, &out);
        assert!(
            gain.abs() < 0.05,
            "at {LOW_CUT_OFF_HZ} Hz the low cut is off, got {gain:.2} dB"
        );

        let mut cutting = EqProcessor::new(SR);
        set(&mut cutting, SLOT_LOW_CUT, 200.0);
        let out = run_through(&mut cutting, &bass);
        let gain = tail_gain_db(&bass, &out);
        assert!(
            gain < -9.0,
            "a 200 Hz low cut must take 50 Hz down hard, got {gain:.2} dB"
        );
    }

    #[test]
    fn dc_is_blocked_only_when_the_low_cut_is_engaged() {
        let one_second = vec![1.0f32; SR as usize];

        let mut open = EqProcessor::new(SR);
        let out = run_through(&mut open, &one_second);
        assert!(
            (out[out.len() - 1] - 1.0).abs() < 1e-6,
            "flat EQ passes DC, got {}",
            out[out.len() - 1]
        );

        let mut cutting = EqProcessor::new(SR);
        set(&mut cutting, SLOT_LOW_CUT, 100.0);
        let out = run_through(&mut cutting, &one_second);
        assert!(
            out[out.len() - 1].abs() < 1e-3,
            "low cut removes DC, got {}",
            out[out.len() - 1]
        );
    }

    #[test]
    fn params_are_clamped_and_unknown_ids_rejected() {
        let mut eq = EqProcessor::new(SR);
        set(&mut eq, SLOT_LOW_GAIN, 99.0);
        assert!((eq.value(SLOT_LOW_GAIN) - 18.0).abs() < 1e-6);
        set(&mut eq, SLOT_LOW_GAIN, -99.0);
        assert!((eq.value(SLOT_LOW_GAIN) + 18.0).abs() < 1e-6);
        set(&mut eq, SLOT_MID_Q, 0.0);
        assert!((eq.value(SLOT_MID_Q) - 0.3).abs() < 1e-6);
        set(&mut eq, SLOT_LOW_CUT, f32::NAN);
        assert!((eq.value(SLOT_LOW_CUT) - LOW_CUT_OFF_HZ).abs() < 1e-6);

        assert!(!eq.set_param(ParamId::BuiltinParam(42), 1.0));
        assert!(!eq.set_param(ParamId::MasterGain, 1.0));
    }

    #[test]
    fn runtime_gain_change_ramps_instead_of_jumping() {
        let mut eq = EqProcessor::new(SR);
        run_through(&mut eq, &vec![0.0; BLOCK]);
        set(&mut eq, SLOT_LOW_GAIN, 12.0);
        // The first sample after the change cannot already be at +12 dB.
        assert!(
            (eq.value(SLOT_LOW_GAIN) - 0.0).abs() < 1e-6,
            "the smoother must not jump"
        );
        for _ in 0..200 {
            run_through(&mut eq, &vec![0.0; BLOCK]);
        }
        assert!((eq.value(SLOT_LOW_GAIN) - 12.0).abs() < 0.05);
    }
}
