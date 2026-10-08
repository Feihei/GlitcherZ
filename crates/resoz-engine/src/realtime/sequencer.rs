//! Step sequencer and piano-roll scheduling (audio thread side, block-boundary
//! sample clock, multi-track).
//!
//! The scheduling model is **pure window scanning**: the sequencer only keeps
//! a musical position (in ticks), and each block schedules in one pass all
//! steps/notes falling due within the `[t0, t1)` window.
//! BPM changes only affect future rates (the position is counted in ticks, so
//! the musical position is naturally preserved);
//! loop wrap-around is derived from the window mapping, with no incremental
//! clock bookkeeping.

use crate::realtime::DEFAULT_MIDI_TRACKS;
use crate::realtime::MAX_MIDI_TRACKS;
use crate::realtime::automation::{MAX_AUTO_TRACKS, RtAutoTrack};
use crate::realtime::commands::NoteEvent;
use crate::realtime::context::ProcessContext;

/// Number of sequencer steps.
pub const PATTERN_STEPS: usize = 16;

/// Note time base: ticks per quarter note (consistent with the `resoz_model` convention).
pub const TICKS_PER_QUARTER: u32 = 96;

/// Ticks per step (a sixteenth note).
pub const TICKS_PER_STEP: u32 = 24;

/// Maximum number of piano-roll notes (capacity constraint via `Copy` on the audio thread side).
pub const MAX_CLIP_NOTES: usize = 128;

/// Track sound source (engine-side representation, mirroring `resoz_model::TrackSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackSource {
    /// 16-step pattern (default).
    #[default]
    Steps,
    /// Piano-roll clip.
    Clip,
}

/// Audio-thread-side representation of a single step.
///
/// Kept `Copy`: an entire pattern can be passed as one command through the
/// lock-free queue in a single block, zero allocation on the real-time side.
/// Mirrors `resoz_model::Step` (see AGENTS.md "Data Model").
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SequencerStep {
    /// Whether this step fires.
    pub enabled: bool,
    /// MIDI note number.
    pub note: u8,
    /// Velocity (0..1).
    pub velocity: f32,
}

/// Audio-thread-side representation of a piano-roll note (mirrors `resoz_model::Note`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RtNote {
    /// Start position (in ticks, relative to the clip start).
    pub start_ticks: u32,
    /// Length (in ticks).
    pub length_ticks: u32,
    /// MIDI note number.
    pub note: u8,
    /// Velocity (0..1).
    pub velocity: f32,
}

/// Audio-thread-side representation of a piano-roll clip (mirrors `resoz_model::Clip`, fixed-capacity `Copy`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtClip {
    /// Loop length (in ticks).
    pub loop_ticks: u32,
    /// Note collection (the first `note_count` are valid).
    pub notes: [RtNote; MAX_CLIP_NOTES],
    /// Number of valid notes.
    pub note_count: usize,
}

impl Default for RtClip {
    fn default() -> Self {
        Self {
            loop_ticks: TICKS_PER_STEP * PATTERN_STEPS as u32,
            notes: [RtNote::default(); MAX_CLIP_NOTES],
            note_count: 0,
        }
    }
}

impl RtClip {
    /// Appends a note; silently dropped beyond [`MAX_CLIP_NOTES`].
    pub fn push(&mut self, note: RtNote) {
        if self.note_count < MAX_CLIP_NOTES {
            self.notes[self.note_count] = note;
            self.note_count += 1;
        }
    }
}

/// Multi-track sequencer: one global transport clock (BPM / gate / musical
/// position), per-track independent sound source (step mode or piano-roll
/// clip) and gate tails, converting due events into `(MIDI track, NoteEvent)`
/// at block boundaries on the sample clock.
///
/// Automation tracks are independent entities parallel to MIDI tracks (D7):
/// [`Sequencer`] only holds their curves and their own loop lengths; the
/// delivery target is determined by the node graph's automation routing
/// table, which this struct knows nothing about.
///
/// The clock is counted in ticks (musical position), converted to the sample
/// clock via BPM;
/// gate offs are recorded as absolute sample positions and emitted at the
/// block boundary containing them.
///
/// Precision note: events are emitted uniformly at the **block boundary**
/// containing their moment (≤ 1 block ≈ 2.7 ms jitter @48 kHz / 128-block),
/// per-sample offsets are left for a future version.
///
/// All methods RT-safe: no allocation, no locks, no panics (`advance` events
/// are sent through a callback, never collected or heap-allocated).
pub struct Sequencer {
    sample_rate: f32,
    playing: bool,
    bpm: f32,
    /// Gate ratio of the step length (0.05..1, only used by the step source;
    /// clips use note lengths).
    gate: f32,
    position_ticks: f64,
    /// Active MIDI track count (D12 dynamic tracks: sent from the control side
    /// via `SetMidiTrackCount`, scheduling only scans `0..track_count`;
    /// unconfigured tracks have default-empty patterns/clips).
    track_count: usize,
    sources: [TrackSource; MAX_MIDI_TRACKS],
    patterns: [[SequencerStep; PATTERN_STEPS]; MAX_MIDI_TRACKS],
    clips: [RtClip; MAX_MIDI_TRACKS],
    /// Automation track entities (independent of MIDI tracks, see D7).
    autos: [RtAutoTrack; MAX_AUTO_TRACKS],
    /// Per-track pending NoteOffs (absolute sample position where the
    /// gate/length ends + note).
    pending_offs: [Option<(u64, u8)>; MAX_MIDI_TRACKS],
}

impl Sequencer {
    /// Falls back to 48 kHz when `sample_rate <= 0` (construction-time tolerance).
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate: if sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            playing: false,
            bpm: 120.0,
            gate: 0.8,
            position_ticks: 0.0,
            track_count: DEFAULT_MIDI_TRACKS,
            sources: [TrackSource::default(); MAX_MIDI_TRACKS],
            patterns: [[SequencerStep::default(); PATTERN_STEPS]; MAX_MIDI_TRACKS],
            clips: [RtClip::default(); MAX_MIDI_TRACKS],
            autos: [RtAutoTrack::default(); MAX_AUTO_TRACKS],
            pending_offs: [None; MAX_MIDI_TRACKS],
        }
    }

    /// Whether playing. RT-safe.
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Current step index (`⌊musical position / step length⌋ mod 16`, used by
    /// the UI to highlight the "sounding step").
    /// RT-safe.
    pub fn step_index(&self) -> usize {
        (self.position_ticks / f64::from(TICKS_PER_STEP)).floor() as usize % PATTERN_STEPS
    }

    /// Musical position (in ticks, accumulated from the play start). RT-safe.
    pub fn position_ticks(&self) -> f64 {
        self.position_ticks
    }

    /// Ticks per sample (f64 intermediate computation).
    fn ticks_per_sample(&self) -> f64 {
        let bpm = if self.bpm.is_finite() {
            self.bpm.clamp(20.0, 300.0)
        } else {
            120.0
        };
        // ticks/second = BPM/60 × TPQN; divide by the sample rate to get ticks/sample.
        f64::from(bpm) / 60.0 * f64::from(TICKS_PER_QUARTER) / f64::from(self.sample_rate)
    }

    /// Play from step 0 (musical position reset to zero). RT-safe.
    pub fn play(&mut self) {
        self.playing = true;
        self.position_ticks = 0.0;
        self.pending_offs = [None; MAX_MIDI_TRACKS];
    }

    /// Stops scheduling and clears all gate tails.
    /// The caller should send `NoteOff` to all tracks to release envelopes. RT-safe.
    pub fn stop(&mut self) {
        self.playing = false;
        self.pending_offs = [None; MAX_MIDI_TRACKS];
    }

    /// Sets the active MIDI track count (clamped to [`MAX_MIDI_TRACKS`]; when
    /// shrinking, gate tails of higher-numbered tracks are cleared too). RT-safe.
    pub fn set_track_count(&mut self, count: usize) {
        self.track_count = count.min(MAX_MIDI_TRACKS);
        if self.pending_offs.len() > self.track_count {
            for pending in self.pending_offs[self.track_count..].iter_mut() {
                *pending = None;
            }
        }
    }

    /// Current active MIDI track count. RT-safe.
    pub fn track_count(&self) -> usize {
        self.track_count
    }

    /// Sets BPM (20..300, non-finite values ignored). The musical position is
    /// counted in ticks, so tempo changes do not affect already-played content,
    /// only the subsequent rate. RT-safe.
    pub fn set_bpm(&mut self, bpm: f32) {
        if bpm.is_finite() {
            self.bpm = bpm.clamp(20.0, 300.0);
        }
    }

    /// Sets the gate ratio (0.05..1, non-finite values ignored). RT-safe.
    pub fn set_gate(&mut self, gate: f32) {
        if gate.is_finite() {
            self.gate = gate.clamp(0.05, 1.0);
        }
    }

    /// Replaces a track's step pattern in one block (out-of-range tracks ignored). RT-safe.
    pub fn set_pattern(&mut self, track: usize, steps: [SequencerStep; PATTERN_STEPS]) {
        if let Some(slot) = self.patterns.get_mut(track) {
            *slot = steps;
        }
    }

    /// Replaces a track's piano-roll clip in one block (out-of-range tracks ignored). RT-safe.
    pub fn set_clip(&mut self, track: usize, clip: RtClip) {
        if let Some(slot) = self.clips.get_mut(track) {
            *slot = clip;
        }
    }

    /// Switches a track's sound source (out-of-range tracks ignored). RT-safe.
    pub fn set_source(&mut self, track: usize, source: TrackSource) {
        if let Some(slot) = self.sources.get_mut(track) {
            *slot = source;
        }
    }

    /// Replaces one automation track in a block (out-of-range indices ignored;
    /// empty curve = clear the track).
    /// The curve carries no target — the delivery endpoint is determined by the
    /// node graph's automation routing table. RT-safe.
    pub fn set_auto_track(&mut self, index: usize, auto: RtAutoTrack) {
        if let Some(slot) = self.autos.get_mut(index) {
            *slot = auto;
        }
    }

    /// Read-only access to all automation track entities (for evaluators to
    /// sample per the routing table). RT-safe.
    pub fn auto_tracks(&self) -> &[RtAutoTrack; MAX_AUTO_TRACKS] {
        &self.autos
    }

    /// Advances one block: sends gate offs and note ons falling due within the
    /// `[t0, t1)` (tick) window via `emit(track, event)` (offs before ons
    /// within the same block).
    /// RT-safe.
    pub fn advance(&mut self, ctx: &ProcessContext, emit: &mut dyn FnMut(usize, NoteEvent)) {
        if !self.playing {
            return;
        }
        let tps = self.ticks_per_sample();
        let samples_per_tick = 1.0 / tps;
        let dt_ticks = ctx.frames as f64 * tps;
        let t0 = self.position_ticks;
        let t1 = t0 + dt_ticks;
        let block_start = ctx.playback_sample_pos;

        // 1. Due gate offs (before this block's note ons, keeping each track's handoff clean).
        for (track, pending) in self.pending_offs.iter_mut().enumerate() {
            if let Some((time, note)) = *pending {
                if time < block_start + ctx.frames as u64 {
                    emit(track, NoteEvent::Off { note });
                    *pending = None;
                }
            }
        }

        // 2. Schedule note ons within the window per track by sound source (scan active tracks only, D12).
        for track in 0..self.track_count {
            match self.sources[track] {
                TrackSource::Steps => {
                    self.schedule_steps(track, t0, t1, block_start, samples_per_tick, emit);
                }
                TrackSource::Clip => {
                    self.schedule_clip(track, t0, t1, block_start, samples_per_tick, emit);
                }
            }
        }

        self.position_ticks = t1;
    }

    /// Step source: steps starting within the window (24 ticks per step).
    #[allow(clippy::too_many_arguments)]
    fn schedule_steps(
        &mut self,
        track: usize,
        t0: f64,
        t1: f64,
        block_start: u64,
        samples_per_tick: f64,
        emit: &mut dyn FnMut(usize, NoteEvent),
    ) {
        let step_ticks = f64::from(TICKS_PER_STEP);
        // Steps starting exactly on the window boundary must also fire: ceil(t0) .. ceil(t1).
        let first = (t0 / step_ticks).ceil() as i64;
        let last = (t1 / step_ticks).ceil() as i64;
        for k in first..last {
            let start_tick = k as f64 * step_ticks;
            if start_tick < t0 {
                continue; // floating-point boundary guard
            }
            let pattern_index = (k as usize) % PATTERN_STEPS;
            let step = self.patterns[track][pattern_index];
            if !step.enabled {
                continue;
            }
            let velocity = sanitize_velocity(step.velocity);
            emit(
                track,
                NoteEvent::On {
                    note: step.note,
                    velocity,
                },
            );
            // gate off time (tick) → absolute sample position.
            let off_tick = start_tick + step_ticks * f64::from(self.gate);
            self.set_pending_off(
                track,
                off_tick,
                t0,
                block_start,
                samples_per_tick,
                step.note,
            );
        }
    }

    /// Piano-roll source: note occurrences starting within the window
    /// (including every occurrence across loop wrap-arounds).
    #[allow(clippy::too_many_arguments)]
    fn schedule_clip(
        &mut self,
        track: usize,
        t0: f64,
        t1: f64,
        block_start: u64,
        samples_per_tick: f64,
        emit: &mut dyn FnMut(usize, NoteEvent),
    ) {
        let clip = self.clips[track];
        if clip.loop_ticks == 0 || clip.note_count == 0 {
            return;
        }
        let loop_ticks = f64::from(clip.loop_ticks);
        for note in &clip.notes[..clip.note_count] {
            let start_offset = f64::from(note.start_ticks);
            let length = f64::from(note.length_ticks.max(1));
            // The m-th occurrence is at m·L + s; find the first m with ≥ t0
            // (may be negative, clamped to 0).
            let mut m = ((t0 - start_offset) / loop_ticks).ceil();
            if m < 0.0 {
                m = 0.0;
            }
            loop {
                let start_tick = m * loop_ticks + start_offset;
                if start_tick >= t1 {
                    break;
                }
                if start_tick >= t0 {
                    let velocity = sanitize_velocity(note.velocity);
                    emit(
                        track,
                        NoteEvent::On {
                            note: note.note,
                            velocity,
                        },
                    );
                    let off_tick = start_tick + length;
                    self.set_pending_off(
                        track,
                        off_tick,
                        t0,
                        block_start,
                        samples_per_tick,
                        note.note,
                    );
                }
                m += 1.0;
            }
        }
    }

    /// Records a gate off: converted to an absolute sample position, landing
    /// at least in the next block (guaranteeing the off comes after the on
    /// already emitted in the same block). RT-safe.
    #[allow(clippy::too_many_arguments)]
    fn set_pending_off(
        &mut self,
        track: usize,
        off_tick: f64,
        t0: f64,
        block_start: u64,
        samples_per_tick: f64,
        note: u8,
    ) {
        let offset_samples = ((off_tick - t0) * samples_per_tick).max(1.0);
        self.pending_offs[track] = Some((block_start + offset_samples as u64, note));
    }
}

fn sanitize_velocity(velocity: f32) -> f32 {
    if velocity.is_finite() {
        velocity.clamp(0.0, 1.0)
    } else {
        0.9
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CLIP_NOTES, PATTERN_STEPS, RtClip, RtNote, Sequencer, SequencerStep, TICKS_PER_STEP,
        TrackSource,
    };
    use crate::realtime::DEFAULT_MIDI_TRACKS;
    use crate::realtime::MAX_MIDI_TRACKS;
    use crate::realtime::automation::{MAX_AUTO_TRACKS, RtAutoPoint, RtAutoTrack};
    use crate::realtime::commands::NoteEvent;
    use crate::realtime::context::ProcessContext;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn ctx(pos: u64) -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: pos,
        }
    }

    type Event = (usize, NoteEvent);

    /// Advances `blocks` blocks, collecting all (track, event) pairs.
    fn run(seq: &mut Sequencer, blocks: usize) -> Vec<Event> {
        let mut events = Vec::new();
        let mut pos = 0u64;
        for _ in 0..blocks {
            seq.advance(&ctx(pos), &mut |track, event| {
                events.push((track, event));
            });
            pos += BLOCK as u64;
        }
        events
    }

    fn step(note: u8) -> SequencerStep {
        SequencerStep {
            enabled: true,
            note,
            velocity: 0.9,
        }
    }

    fn clip_note(start: u32, length: u32, note: u8) -> RtNote {
        RtNote {
            start_ticks: start,
            length_ticks: length,
            note,
            velocity: 0.9,
        }
    }

    fn ons(events: &[Event]) -> Vec<(usize, u8)> {
        events
            .iter()
            .filter_map(|(t, e)| match e {
                NoteEvent::On { note, .. } => Some((*t, *note)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plays_per_track_patterns_with_gate_offs() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0); // sixteenth note = 24 ticks = 6000 samples
        let mut track0 = [SequencerStep::default(); PATTERN_STEPS];
        track0[0] = step(60);
        track0[1] = step(64);
        let mut track1 = [SequencerStep::default(); PATTERN_STEPS];
        track1[0] = step(72);
        seq.set_pattern(0, track0);
        seq.set_pattern(1, track1);
        seq.play();

        let events = run(&mut seq, 375); // 1 s
        assert_eq!(
            ons(&events),
            vec![(0, 60), (1, 72), (0, 64)],
            "both tracks should fire on the same clock"
        );
        let offs = events
            .iter()
            .filter(|(_, e)| matches!(e, NoteEvent::Off { .. }))
            .count();
        assert_eq!(offs, 3, "every note should have a gate off");
        assert!(matches!(events[0], (0, NoteEvent::On { .. })));
        assert!(matches!(events[1], (1, NoteEvent::On { .. })));
    }

    #[test]
    fn disabled_steps_emit_nothing() {
        let mut seq = Sequencer::new(SR);
        seq.play();
        let events = run(&mut seq, 375);
        assert!(events.is_empty(), "fully disabled pattern should be silent");
        // 48000 samples = 192 ticks = 8 steps.
        assert_eq!(seq.step_index(), 8);
    }

    #[test]
    fn clip_notes_fire_at_their_positions_with_lengths() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        let mut clip = RtClip::default();
        clip.push(clip_note(0, 24, 60));
        clip.push(clip_note(24, 48, 64));
        seq.set_clip(0, clip);
        seq.set_source(0, TrackSource::Clip);
        seq.play();

        let events = run(&mut seq, 375);
        assert_eq!(
            ons(&events),
            vec![(0, 60), (0, 64)],
            "notes should fire in order of start"
        );
        // every note has an off (by its own length, not the gate).
        let offs = events
            .iter()
            .filter(|(_, e)| matches!(e, NoteEvent::Off { .. }))
            .count();
        assert_eq!(offs, 2);
    }

    #[test]
    fn clip_loops_around() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        let mut clip = RtClip::default();
        clip.push(clip_note(0, 24, 60));
        seq.set_clip(0, clip);
        seq.set_source(0, TrackSource::Clip);
        seq.play();

        // Loop length 384 ticks = 96000 samples = 750 blocks; running 2.5 loops → fires 3 times.
        let ons_count = run(&mut seq, 1_900)
            .iter()
            .filter(|(_, e)| matches!(e, NoteEvent::On { .. }))
            .count();
        assert_eq!(ons_count, 3, "clip should loop around per loop_ticks");
    }

    #[test]
    fn source_switching_uses_only_active_source() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(48);
        seq.set_pattern(0, pattern);
        let mut clip = RtClip::default();
        clip.push(clip_note(0, 24, 72));
        seq.set_clip(0, clip);
        seq.play();

        seq.set_source(0, TrackSource::Steps);
        assert_eq!(ons(&run(&mut seq, 30)), vec![(0, 48)]);

        seq.play();
        seq.set_source(0, TrackSource::Clip);
        assert_eq!(ons(&run(&mut seq, 30)), vec![(0, 72)]);
    }

    #[test]
    fn clip_over_capacity_is_truncated() {
        let mut clip = RtClip::default();
        for i in 0..(MAX_CLIP_NOTES + 10) {
            clip.push(clip_note(i as u32 * TICKS_PER_STEP, 24, 60));
        }
        assert_eq!(
            clip.note_count, MAX_CLIP_NOTES,
            "notes beyond capacity should be dropped"
        );
    }

    #[test]
    fn bpm_change_preserves_musical_position() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        seq.play();
        run(&mut seq, 100); // 12800 samples = 51.2 ticks @120BPM
        let before = seq.position_ticks();
        seq.set_bpm(240.0);
        assert!(
            (seq.position_ticks() - before).abs() < 1e-9,
            "tempo change must not jump the position"
        );
    }

    #[test]
    fn bpm_change_alters_step_rate() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        for step_slot in &mut pattern {
            *step_slot = step(60);
        }
        seq.set_pattern(0, pattern);

        // 370 blocks = 47360 samples: at 120 BPM 189.44 ticks (8 steps),
        // at 240 BPM 378.88 ticks (16 steps) — both windows avoid step boundaries.
        let count_ons = |events: &[Event]| {
            events
                .iter()
                .filter(|(_, e)| matches!(e, NoteEvent::On { .. }))
                .count()
        };
        seq.set_bpm(120.0);
        seq.play();
        let slow = count_ons(&run(&mut seq, 370));

        seq.set_bpm(240.0);
        seq.play();
        let fast = count_ons(&run(&mut seq, 370));
        assert_eq!(
            fast,
            slow * 2,
            "240 BPM fire rate should be exactly twice 120 BPM's"
        );
    }

    #[test]
    fn stop_quiets_every_track() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        for track in 0..DEFAULT_MIDI_TRACKS {
            seq.set_pattern(track, pattern);
        }
        seq.play();
        let events = run(&mut seq, 1);
        assert_eq!(
            ons(&events).len(),
            DEFAULT_MIDI_TRACKS,
            "default four tracks should fire step 0 simultaneously"
        );

        seq.stop();
        let events = run(&mut seq, 375);
        assert!(events.is_empty(), "no more events after stopping");
        assert!(!seq.is_playing());
    }

    #[test]
    fn step_clock_wraps_every_16_steps() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        pattern[15] = step(72);
        seq.set_pattern(0, pattern);
        seq.play();

        // 20 steps (480 ticks ≈ 120_000 samples) + margin: step 0 fires twice.
        let fired: Vec<u8> = run(&mut seq, 950)
            .iter()
            .filter_map(|(t, e)| {
                if *t == 0 {
                    match e {
                        NoteEvent::On { note, .. } => Some(*note),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            fired,
            vec![60, 72, 60],
            "step clock should wrap after 16 steps"
        );
    }

    #[test]
    fn gate_off_comes_before_next_note_on_at_full_gate() {
        let mut seq = Sequencer::new(SR);
        seq.set_gate(1.0);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        pattern[1] = step(64);
        seq.set_pattern(0, pattern);
        seq.play();

        let events = run(&mut seq, 60);
        // 60 blocks = 7680 samples covers step 0 (on@0, off@5999) and step 1's on@6000.
        let kinds: Vec<&str> = events
            .iter()
            .map(|(_, e)| match e {
                NoteEvent::On { .. } => "on",
                NoteEvent::Off { .. } => "off",
            })
            .collect();
        assert_eq!(kinds, vec!["on", "off", "on"]);
    }

    #[test]
    fn playing_and_step_feedback() {
        let mut seq = Sequencer::new(SR);
        assert!(!seq.is_playing());
        seq.play();
        assert!(seq.is_playing());
        // 5760 samples = 23.04 ticks < 24: still within step 0.
        run(&mut seq, 45);
        assert_eq!(seq.step_index(), 0);
        run(&mut seq, 5); // 6400 samples = 25.6 ticks: entering step 1
        assert_eq!(seq.step_index(), 1);
    }

    #[test]
    fn auto_tracks_are_independent_entities() {
        let mut seq = Sequencer::new(SR);
        let mut auto = RtAutoTrack {
            loop_ticks: 96, // decoupled from the MIDI track loop (D7)
            ..RtAutoTrack::default()
        };
        auto.curve.push(RtAutoPoint {
            tick: 0,
            value: 0.5,
        });
        seq.set_auto_track(5, auto);
        seq.set_auto_track(MAX_AUTO_TRACKS, auto); // out-of-range index ignored
        let tracks = seq.auto_tracks();
        assert_eq!(tracks[5].loop_ticks, 96);
        assert_eq!(tracks[5].curve.point_count, 1);
        assert!(
            tracks
                .iter()
                .enumerate()
                .all(|(i, t)| i == 5 || !t.curve.is_active()),
            "other automation tracks should keep default empty curves"
        );
        // MIDI tracks are unaffected by automation track writes: patterns schedule as usual.
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        seq.set_pattern(0, pattern);
        seq.play();
        assert_eq!(ons(&run(&mut seq, 30)), vec![(0, 60)]);
    }

    #[test]
    fn track_count_gates_scheduling() {
        let mut seq = Sequencer::new(SR);
        let mut pattern = [SequencerStep::default(); PATTERN_STEPS];
        pattern[0] = step(60);
        seq.set_pattern(5, pattern);
        seq.set_track_count(4);
        seq.play();
        assert!(
            ons(&run(&mut seq, 30)).is_empty(),
            "tracks beyond the active count should not schedule"
        );
        seq.set_track_count(8);
        seq.play(); // reset the transport so step 0 falls back into the scheduling window
        assert_eq!(ons(&run(&mut seq, 30)), vec![(5, 60)]);
        // Clamped to the capacity limit.
        seq.set_track_count(MAX_MIDI_TRACKS + 10);
        assert_eq!(seq.track_count(), MAX_MIDI_TRACKS);
    }
}
