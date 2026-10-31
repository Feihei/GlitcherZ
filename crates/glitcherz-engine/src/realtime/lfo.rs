//! Free-running LFO control source (spec `.agents/spec/lfo.md`).
//!
//! Emits one normalized `[-1, 1]` value per block on the control bus, so it
//! can drive a parameter, a CC or a pitch bend through the same target-derived
//! axis as a curve (spec `curve-map.md` D1/D5).
//!
//! Time base:
//!
//! - `sync = false`: phase advances by playing seconds × `rate_hz`;
//! - `sync = true`: phase is derived from `ctx.song_ticks / division_ticks`,
//!   which follows tempo curves, seeks and loop wraps.
//!
//! The transport being stopped freezes the phase **and** stops delivery
//! (`control = None`), so a stopped LFO cannot keep overwriting the parameter
//! the user is trying to edit by hand.
//!
//! Real-time rules: no allocation, no locks, no panics.

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::events::NodeEvents;
use crate::realtime::processor::Processor;

/// One sync cycle in ticks, parallel to the model's `LFO_DIVISION_LABELS`
/// (1/1 … 1/32; 960 ticks per quarter).
pub const LFO_DIVISION_TICKS: [f64; 6] = [3840.0, 1920.0, 960.0, 480.0, 240.0, 120.0];

/// Lowest free-running rate (Hz). Slower than this is not a usable modulation
/// and would make the phase accumulate for minutes before moving.
pub const LFO_MIN_RATE_HZ: f32 = 0.01;
/// Highest free-running rate (Hz): below half a typical 512-frame block rate
/// (≈ 93 Hz at 48 kHz) and inside the range of audible modulation.
pub const LFO_MAX_RATE_HZ: f32 = 40.0;

/// LFO waveform count (engine index form).
pub const LFO_WAVEFORM_COUNT: u8 = 5;

/// Normalized LFO processor.
pub struct LfoProcessor {
    /// 0 = Sine, 1 = Triangle, 2 = Saw, 3 = Square, 4 = Sample & Hold.
    waveform: u8,
    /// Free-running rate (Hz) when `sync` is off.
    rate_hz: f32,
    /// Output depth (0..1).
    depth: f32,
    /// Musical sync mode.
    sync: bool,
    /// Sync division index.
    division: u8,
    /// Current phase in `0..1`.
    phase: f32,
    /// Accumulated **playing** seconds (Hz mode).
    seconds: f64,
    /// Current Sample & Hold value.
    hold: f32,
    /// XorShift32 state (seeded from the document node id).
    rng: u32,
    /// Whether the transport was active on the previous block: the
    /// stopped→playing edge is where a transport starting from zero resets.
    was_active: bool,
}

impl LfoProcessor {
    /// Creates the processor. `seed` should be the document node id so the
    /// Sample & Hold sequence is reproducible across realtime and offline
    /// rendering (spec D6).
    pub fn new(seed: u32) -> Self {
        let mut processor = Self {
            waveform: 0,
            rate_hz: 1.0,
            depth: 0.5,
            sync: false,
            division: 2,
            phase: 0.0,
            seconds: 0.0,
            hold: 0.0,
            rng: seed | 1,
            was_active: false,
        };
        processor.hold = processor.next_random();
        processor
    }

    /// Loads the document parameters (all values are clamped defensively).
    pub fn apply_params(
        &mut self,
        waveform: u8,
        rate_hz: f32,
        sync: bool,
        division: u8,
        depth: f32,
    ) {
        self.waveform = waveform.min(LFO_WAVEFORM_COUNT - 1);
        self.rate_hz = if rate_hz.is_finite() {
            rate_hz.clamp(LFO_MIN_RATE_HZ, LFO_MAX_RATE_HZ)
        } else {
            1.0
        };
        self.depth = if depth.is_finite() {
            depth.clamp(0.0, 1.0)
        } else {
            0.5
        };
        self.sync = sync;
        self.division = division.min(LFO_DIVISION_TICKS.len() as u8 - 1);
    }

    /// Xorshift32 → `[-1, 1]`. The state is never zero (`new` ORs bit 0), so
    /// the sequence cannot collapse.
    fn next_random(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    /// Raw waveform at `phase` in `[-1, 1]` (before `depth`).
    fn wave_value(&self, phase: f32) -> f32 {
        match self.waveform {
            1 => {
                if phase < 0.5 {
                    4.0 * phase - 1.0
                } else {
                    3.0 - 4.0 * phase
                }
            }
            2 => 2.0 * phase - 1.0,
            3 => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            4 => self.hold,
            _ => (std::f32::consts::TAU * phase).sin(),
        }
    }

    /// Advances the phase for this block. Returns the previous phase so the
    /// caller can refresh Sample & Hold on a cycle boundary.
    fn advance(&mut self, ctx: &ProcessContext) -> f32 {
        let previous = self.phase;
        if self.sync {
            let ticks =
                LFO_DIVISION_TICKS[usize::from(self.division).min(LFO_DIVISION_TICKS.len() - 1)];
            let turns = ctx.song_ticks / ticks;
            self.phase = turns.rem_euclid(1.0) as f32;
        } else {
            let seconds = ctx.frames as f64 / f64::from(ctx.sample_rate.max(1.0));
            self.seconds += seconds;
            let turns = self.seconds * f64::from(self.rate_hz);
            self.phase = turns.rem_euclid(1.0) as f32;
        }
        previous
    }
}

impl Default for LfoProcessor {
    fn default() -> Self {
        Self::new(1)
    }
}

impl Processor for LfoProcessor {
    fn process(&mut self, _ctx: &ProcessContext, _buffer: &mut AudioBuffer) {}

    fn process_events(&mut self, ctx: &ProcessContext, out: &mut NodeEvents) {
        if !ctx.transport_active {
            self.was_active = false;
            out.control = None;
            return;
        }
        if !self.was_active {
            self.was_active = true;
            // Stop rewinds the transport to zero; coming from a pause keeps a
            // non-zero position. Only the former restarts the LFO (spec D2).
            if ctx.song_ticks <= 0.0 {
                self.phase = 0.0;
                self.seconds = 0.0;
                self.hold = self.next_random();
            }
        }
        let previous = self.advance(ctx);
        if self.phase < previous {
            self.hold = self.next_random();
        }
        out.control = Some((self.wave_value(self.phase) * self.depth).clamp(-1.0, 1.0));
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            ParamId::LfoRate => {
                if value.is_finite() {
                    self.rate_hz = value.clamp(LFO_MIN_RATE_HZ, LFO_MAX_RATE_HZ);
                }
                true
            }
            ParamId::LfoWaveform => {
                if value.is_finite() {
                    self.waveform =
                        value.round().clamp(0.0, f32::from(LFO_WAVEFORM_COUNT - 1)) as u8;
                }
                true
            }
            ParamId::LfoDepth => {
                if value.is_finite() {
                    self.depth = value.clamp(0.0, 1.0);
                }
                true
            }
            ParamId::LfoSync => {
                if value.is_finite() {
                    self.sync = value >= 0.5;
                }
                true
            }
            ParamId::LfoDivision => {
                if value.is_finite() {
                    self.division = value
                        .round()
                        .clamp(0.0, (LFO_DIVISION_TICKS.len() - 1) as f32)
                        as u8;
                }
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LFO_DIVISION_TICKS, LFO_MAX_RATE_HZ, LFO_MIN_RATE_HZ, LfoProcessor};
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::events::NodeEvents;
    use crate::realtime::processor::Processor;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn ctx(song_ticks: f64, active: bool) -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: 0,
            song_ticks,
            transport_dt_ticks: 0.0,
            transport_active: active,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        }
    }

    fn emit(processor: &mut LfoProcessor, ctx: &ProcessContext) -> Option<f32> {
        let mut out = NodeEvents::default();
        processor.process_events(ctx, &mut out);
        out.control
    }

    fn sine(depth: f32) -> LfoProcessor {
        let mut processor = LfoProcessor::new(7);
        processor.apply_params(0, 1.0, false, 2, depth);
        processor
    }

    #[test]
    fn waveform_table_matches_the_spec() {
        fn with_wave(wave: u8) -> LfoProcessor {
            let mut processor = LfoProcessor::new(1);
            processor.waveform = wave;
            processor
        }
        for (wave, expected) in [
            (0u8, [0.0f32, 1.0, 0.0, -1.0]),
            (1, [-1.0, 0.0, 1.0, 0.0]),
            (2, [-1.0, -0.5, 0.0, 0.5]),
            (3, [1.0, 1.0, -1.0, -1.0]),
        ] {
            let processor = with_wave(wave);
            for (phase, want) in [0.0, 0.25, 0.5, 0.75].into_iter().zip(expected) {
                let got = processor.wave_value(phase);
                assert!((got - want).abs() < 1e-5, "wave {wave} at {phase}: {got}");
            }
        }
    }

    #[test]
    fn depth_scales_and_zero_is_silent_at_the_axis_center() {
        let mut full = sine(1.0);
        let mut half = sine(0.5);
        let mut muted = sine(0.0);
        let t = ctx(0.0, true);
        // One block has already advanced the phase by 128 / 48,000.
        assert!(emit(&mut full, &t).unwrap().abs() < 0.02);
        assert!(emit(&mut half, &t).unwrap().abs() < 0.01);
        assert_eq!(emit(&mut muted, &t), Some(0.0));

        // A quarter cycle at 1 Hz = 12,000 samples = 93.75 blocks.
        for _ in 0..93 {
            emit(&mut full, &t);
            emit(&mut half, &t);
            emit(&mut muted, &t);
        }
        let full = emit(&mut full, &t).unwrap();
        let half = emit(&mut half, &t).unwrap();
        let muted = emit(&mut muted, &t).unwrap();
        assert!(full > 0.9, "full: {full}");
        assert!((half - full * 0.5).abs() < 1e-3, "half: {half}");
        assert_eq!(muted, 0.0);
    }

    #[test]
    fn sync_phase_follows_song_position() {
        let mut processor = LfoProcessor::new(1);
        // Square is unambiguous at the half-cycle boundary.
        processor.apply_params(3, 1.0, true, 2, 1.0);
        assert_eq!(emit(&mut processor, &ctx(0.0, true)), Some(1.0));
        // Half of a 1/4 cycle (960 ticks) = exact phase 0.5.
        assert_eq!(emit(&mut processor, &ctx(480.0, true)), Some(-1.0));
        // A full cycle returns to +1.
        assert_eq!(emit(&mut processor, &ctx(960.0, true)), Some(1.0));
        // A seek to 3/4 cycle is still derived from the position.
        assert_eq!(emit(&mut processor, &ctx(720.0, true)), Some(-1.0));
    }

    #[test]
    fn transport_stops_delivery_and_freezes_phase() {
        let mut processor = sine(1.0);
        let t = ctx(0.0, true);
        for _ in 0..94 {
            emit(&mut processor, &t);
        }
        let frozen = processor.phase;
        assert!(frozen > 0.0);
        assert_eq!(emit(&mut processor, &ctx(0.0, false)), None);
        assert_eq!(
            processor.phase, frozen,
            "phase must not advance while stopped"
        );
        // Delivery resumes on the next playing block.
        assert!(emit(&mut processor, &t).is_some());
    }

    #[test]
    fn resume_from_pause_keeps_hz_phase_and_stop_restarts() {
        let mut processor = sine(1.0);
        let t = ctx(0.0, true);
        for _ in 0..94 {
            emit(&mut processor, &t);
        }
        let before_pause = processor.phase;
        assert!(before_pause > 0.0);
        // Pause keeps the transport position, so the stop→play reset does not
        // fire; the phase continues from where it was.
        assert_eq!(emit(&mut processor, &ctx(480.0, false)), None);
        emit(&mut processor, &ctx(480.0, true));
        assert!(processor.phase > before_pause);

        // Stop rewinds to zero: the next playing block restarts the LFO.
        assert_eq!(emit(&mut processor, &ctx(0.0, false)), None);
        emit(&mut processor, &ctx(0.0, true));
        assert!(
            processor.phase < 0.01,
            "phase after a stop restart: {}",
            processor.phase
        );
    }

    #[test]
    fn sample_and_hold_changes_once_per_cycle() {
        let mut processor = LfoProcessor::new(42);
        processor.apply_params(4, 1.0, false, 2, 1.0);
        let t = ctx(0.0, true);
        let first = emit(&mut processor, &t).unwrap();
        // Within the cycle the value is held.
        for _ in 0..90 {
            assert_eq!(emit(&mut processor, &t), Some(first));
        }
        // 1 Hz cycle = 48,000 samples = 375 blocks.
        let mut different = false;
        let mut last = first;
        for _ in 0..300 {
            let value = emit(&mut processor, &t).unwrap();
            different |= value != last;
            last = value;
        }
        assert!(different, "S&H never changed across cycle boundaries");
    }

    #[test]
    fn same_seed_produces_the_same_sequence() {
        let mut a = LfoProcessor::new(123);
        let mut b = LfoProcessor::new(123);
        a.apply_params(4, 10.0, false, 2, 1.0);
        b.apply_params(4, 10.0, false, 2, 1.0);
        let t = ctx(0.0, true);
        for _ in 0..500 {
            assert_eq!(emit(&mut a, &t), emit(&mut b, &t));
        }
    }

    #[test]
    fn params_clamp_into_range() {
        let mut processor = LfoProcessor::new(1);
        processor.set_param(ParamId::LfoRate, 1_000.0);
        processor.set_param(ParamId::LfoDepth, -3.0);
        processor.set_param(ParamId::LfoWaveform, 99.0);
        processor.set_param(ParamId::LfoDivision, 99.0);
        processor.set_param(ParamId::LfoSync, 1.0);
        assert_eq!(processor.rate_hz, LFO_MAX_RATE_HZ);
        assert_eq!(processor.depth, 0.0);
        assert_eq!(processor.waveform, 4);
        assert_eq!(processor.division as usize, LFO_DIVISION_TICKS.len() - 1);
        assert!(processor.sync);

        processor.set_param(ParamId::LfoRate, 0.0);
        assert_eq!(processor.rate_hz, LFO_MIN_RATE_HZ);
        // Non-finite writes are ignored.
        processor.set_param(ParamId::LfoRate, f32::NAN);
        assert_eq!(processor.rate_hz, LFO_MIN_RATE_HZ);
    }
}
