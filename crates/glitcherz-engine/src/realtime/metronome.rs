//! Transport metronome (spec `.agents/spec/recording.md` D1–D5).
//!
//! The click is **not** a graph node: it is mixed into the master output after
//! the graph has run, for two reasons.
//!
//! 1. *The clock lives here.* The tick domain (`Sequencer::position_ticks` +
//!    `base_bpm` + the meter) is only exact at a block boundary; a click
//!    triggered from the control thread would jitter by a whole UI frame.
//!    Firing at the block boundary also guarantees the click and the sequenced
//!    notes land on the **same** quantization point, so their relative timing
//!    is exact even though both are block-quantized.
//! 2. *Independence.* A user who deletes the master chain, bypasses the bus or
//!    mutes every track must still hear the click. Coming after the graph also
//!    keeps it out of the master peak meter and out of automation.
//!
//! The offline export builds a `BlockRunner` that never receives
//! `SetMetronome`, so a rendered file can never contain a click (D2).
//!
//! RT safety: the two waveform tables are generated once on the constructing
//! (control) thread; [`Metronome::tick`] only reads them, writes the caller's
//! pre-allocated block buffer and does integer/float arithmetic — no
//! allocation, no locks, no transcendental calls.

use crate::realtime::sequencer::Sequencer;

/// Click length (seconds). Short enough not to smear the beat, long enough to
/// survive small speakers.
const CLICK_SECONDS: f32 = 0.040;

/// Accent (bar downbeat) partial frequency, Hz.
const ACCENT_HZ: f32 = 1_600.0;

/// Weak-beat partial frequency, Hz.
const BEAT_HZ: f32 = 1_000.0;

/// Accent level relative to the weak beat (linear).
const ACCENT_LEVEL: f32 = 1.0;

/// Weak-beat level relative to the accent (linear).
const BEAT_LEVEL: f32 = 0.62;

/// Decay time constant (seconds) of the click envelope.
const DECAY_SECONDS: f32 = 0.009;

/// Attack ramp (seconds) — long enough to avoid a DC click at note start.
const ATTACK_SECONDS: f32 = 0.0008;

/// Second-partial ratio and level, which give the click a "wood block" body
/// instead of a bare sine beep.
const PARTIAL_RATIO: f32 = 2.02;
const PARTIAL_LEVEL: f32 = 0.34;

/// One click waveform, pre-rendered at the stream's sample rate.
#[derive(Debug)]
struct ClickWave {
    samples: Vec<f32>,
}

impl ClickWave {
    /// Renders one click. `frequency` is the fundamental; the table is peak
    /// normalized to `level` so accent and weak beats differ only by level.
    fn render(sample_rate: f32, frequency: f32, level: f32) -> Self {
        let frames = ((CLICK_SECONDS * sample_rate).round() as usize).max(1);
        let mut samples = Vec::with_capacity(frames);
        let mut peak = 0.0f32;
        for i in 0..frames {
            let t = i as f32 / sample_rate;
            let attack = (t / ATTACK_SECONDS).min(1.0);
            let envelope = attack * (-t / DECAY_SECONDS).exp();
            let fundamental = (std::f32::consts::TAU * frequency * t).sin();
            let partial =
                (std::f32::consts::TAU * frequency * PARTIAL_RATIO * t).sin() * PARTIAL_LEVEL;
            let sample = (fundamental + partial) * envelope;
            peak = peak.max(sample.abs());
            samples.push(sample);
        }
        // Peak-normalize, then apply the level: one table per beat kind, and
        // the accent/weak difference stays a pure gain.
        let scale = if peak > 0.0 { level / peak } else { 0.0 };
        for sample in &mut samples {
            *sample *= scale;
        }
        Self { samples }
    }
}

/// The transport metronome.
#[derive(Debug)]
pub struct Metronome {
    enabled: bool,
    gain: f32,
    accent: ClickWave,
    normal: ClickWave,
    /// Playback cursor into the active table; `None` = silent.
    cursor: Option<Cursor>,
    /// Beat index of the previous block (absolute, `floor(pos / beat)`).
    last_beat: Option<i64>,
    /// Transport position of the previous block, for loop-wrap detection.
    last_pos: f64,
    /// Scratch: how many frames of the active click are left to play.
    remaining: usize,
}

/// Which table is playing and how far into it we are.
#[derive(Debug, Clone, Copy)]
struct Cursor {
    accent: bool,
    pos: usize,
}

impl Metronome {
    /// Builds the click tables. **Not RT-safe** (allocates); call before the
    /// stream starts, on the control thread. Starts **disabled**.
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = if sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            enabled: false,
            gain: 1.0,
            accent: ClickWave::render(sample_rate, ACCENT_HZ, ACCENT_LEVEL),
            normal: ClickWave::render(sample_rate, BEAT_HZ, BEAT_LEVEL),
            cursor: None,
            last_beat: None,
            last_pos: 0.0,
            remaining: 0,
        }
    }

    /// Enables the click and sets its linear gain (`0..=1`, clamped). RT-safe.
    pub fn configure(&mut self, enabled: bool, gain: f32) {
        self.enabled = enabled;
        self.gain = if gain.is_finite() {
            gain.clamp(0.0, 1.0)
        } else {
            1.0
        };
        if !enabled {
            self.cursor = None;
            self.remaining = 0;
        }
    }

    /// Whether the click is enabled. RT-safe.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Drops any in-flight click and re-anchors beat tracking to `pos`.
    ///
    /// Called on play / stop / pause / seek: a seek must not fire the beat it
    /// happened to land on, while starting **exactly** on a beat must fire it
    /// (the bar-one downbeat). A resume in the middle of a beat therefore
    /// waits for the next beat instead of clicking immediately.
    pub fn resync(&mut self, pos: f64, beat_ticks: u32) {
        self.cursor = None;
        self.remaining = 0;
        self.last_pos = pos;
        self.last_beat = Some(Self::beat_index(pos, beat_ticks) - Self::on_beat(pos, beat_ticks));
    }

    /// Fills `out` with this block's click (mono; the caller adds it to both
    /// output channels) and advances the beat tracker. RT-safe.
    ///
    /// `out.len()` is the block length; it is fully overwritten.
    pub fn tick(&mut self, sequencer: &Sequencer, out: &mut [f32]) {
        out.fill(0.0);
        let beat_ticks = sequencer.beat_ticks();
        if beat_ticks == 0 {
            return;
        }
        let pos = sequencer.position_ticks();
        let index = Self::beat_index(pos, beat_ticks);

        if self.enabled && sequencer.is_playing() {
            let wrapped = pos + 0.5 < self.last_pos;
            let advanced = self.last_beat.is_some_and(|last| index != last);
            if wrapped || advanced {
                self.last_beat = Some(index);
                let downbeat = index.rem_euclid(i64::from(sequencer.numerator())) == 0;
                self.cursor = Some(Cursor {
                    accent: downbeat,
                    pos: 0,
                });
                self.remaining = if downbeat {
                    self.accent.samples.len()
                } else {
                    self.normal.samples.len()
                };
            }
        }
        self.last_beat = Some(index);
        self.last_pos = pos;

        if !self.enabled {
            return;
        }
        let Some(cursor) = self.cursor else {
            return;
        };
        let table = if cursor.accent {
            &self.accent.samples
        } else {
            &self.normal.samples
        };
        let frames = out.len().min(self.remaining);
        for (slot, sample) in out.iter_mut().zip(table[cursor.pos..].iter()).take(frames) {
            *slot = sample * self.gain;
        }
        self.cursor = Some(Cursor {
            accent: cursor.accent,
            pos: cursor.pos + frames,
        });
        self.remaining = self.remaining.saturating_sub(frames);
        if self.remaining == 0 {
            self.cursor = None;
        }
    }

    /// `floor(pos / beat)` — the absolute beat index.
    fn beat_index(pos: f64, beat_ticks: u32) -> i64 {
        (pos / f64::from(beat_ticks)).floor() as i64
    }

    /// Whether `pos` sits on a beat boundary (within one tick), used to decide
    /// whether a transport start should click immediately.
    fn on_beat(pos: f64, beat_ticks: u32) -> i64 {
        let index = Self::beat_index(pos, beat_ticks);
        let distance = pos - index as f64 * f64::from(beat_ticks);
        i64::from(distance < 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::Metronome;
    use crate::realtime::commands::NoteEvent;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::sequencer::{Sequencer, SequencerSink, ticks_per_beat_of};

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    /// Discards every scheduled event — the metronome tests only care about
    /// the click, never about which notes play.
    struct NullSink;

    impl SequencerSink for NullSink {
        fn note(&mut self, _track: usize, _event: NoteEvent) {}
    }

    /// A playing sequencer at `bpm`, advanced in 128-frame blocks at 48 kHz.
    fn playing(bpm: f32) -> Sequencer {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(bpm);
        seq.play();
        seq
    }

    /// Advances `blocks` blocks, returning the click amplitude of each block.
    fn run(seq: &mut Sequencer, metronome: &mut Metronome, blocks: usize) -> Vec<f32> {
        let mut peaks = Vec::with_capacity(blocks);
        let mut buf = [0.0f32; BLOCK];
        let mut sink = NullSink;
        let mut sample_pos = 0u64;
        for _ in 0..blocks {
            let ctx = ProcessContext {
                sample_rate: SR,
                frames: BLOCK,
                playback_sample_pos: sample_pos,
                song_ticks: seq.position_ticks(),
                transport_dt_ticks: seq.transport_dt_ticks(BLOCK),
                transport_active: seq.is_transport_active(),
                sec_per_tick: seq.base_sec_per_tick(),
            };
            seq.advance(&ctx, &mut sink);
            metronome.tick(seq, &mut buf);
            peaks.push(buf.iter().fold(0.0f32, |a, s| a.max(s.abs())));
            sample_pos += BLOCK as u64;
        }
        peaks
    }

    #[test]
    fn ticks_per_beat_matches_the_meter() {
        assert_eq!(ticks_per_beat_of(4), 960);
        assert_eq!(ticks_per_beat_of(8), 480);
        assert_eq!(ticks_per_beat_of(16), 240);
    }

    #[test]
    fn silent_when_disabled() {
        let mut seq = playing(120.0);
        let mut metronome = Metronome::new(48_000.0);
        // 400 blocks ≈ 1 s ≈ 2 beats at 120 bpm.
        let peaks = run(&mut seq, &mut metronome, 400);
        assert!(
            peaks.iter().all(|p| *p == 0.0),
            "disabled click must be silent"
        );
    }

    #[test]
    fn clicks_once_per_beat() {
        let mut seq = playing(120.0);
        let mut metronome = Metronome::new(48_000.0);
        metronome.configure(true, 1.0);
        metronome.resync(0.0, seq.beat_ticks());
        // 1 second at 120 bpm = 2 beats → onsets at block 0 and ~block 187.
        let peaks = run(&mut seq, &mut metronome, 400);
        let onsets: Vec<usize> = peaks
            .iter()
            .enumerate()
            .filter(|(i, p)| **p > 0.0 && (*i == 0 || peaks[i - 1] == 0.0))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(onsets.len(), 3, "onsets = {onsets:?}");
        // The first block starts exactly on the beat.
        assert_eq!(onsets[0], 0);
        // 500 ms at 128 frames / 48 kHz ≈ 187.5 blocks.
        assert!((onsets[1] as i64 - 187).abs() <= 1, "onsets = {onsets:?}");
        assert!((onsets[2] as i64 - 375).abs() <= 1, "onsets = {onsets:?}");
    }

    #[test]
    fn stopped_transport_is_silent() {
        let mut seq = Sequencer::new(48_000.0);
        seq.set_bpm(120.0);
        let mut metronome = Metronome::new(48_000.0);
        metronome.configure(true, 1.0);
        metronome.resync(0.0, seq.beat_ticks());
        let peaks = run(&mut seq, &mut metronome, 200);
        assert!(peaks.iter().all(|p| *p == 0.0));
    }

    #[test]
    fn seek_does_not_click_mid_beat() {
        let mut seq = playing(120.0);
        let mut metronome = Metronome::new(48_000.0);
        metronome.configure(true, 1.0);
        // Land 100 ticks into a beat: the resume must wait for the next beam.
        seq.seek(1_060.0);
        metronome.resync(1_060.0, seq.beat_ticks());
        let mut buf = [0.0f32; 128];
        metronome.tick(&seq, &mut buf);
        assert!(
            buf.iter().all(|s| *s == 0.0),
            "a mid-beat resume must not click immediately"
        );
    }

    #[test]
    fn accent_marks_the_downbeat() {
        let mut seq = playing(120.0);
        let mut metronome = Metronome::new(48_000.0);
        metronome.configure(true, 1.0);
        metronome.resync(0.0, seq.beat_ticks());
        let peaks = run(&mut seq, &mut metronome, 400);
        // Block 0 is the bar downbeat (accent), block ~187 is a weak beat.
        let first = peaks[0];
        let second = peaks[187];
        assert!(first > 0.0 && second > 0.0);
        assert!(
            first > second,
            "accent {first} should be louder than {second}"
        );
    }
}
