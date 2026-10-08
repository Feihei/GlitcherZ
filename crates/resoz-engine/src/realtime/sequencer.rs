//! Timeline clip scheduling (audio thread side, block-boundary sample clock,
//! multi-track) — timeline spec T2 (D14-D16).
//!
//! The scheduling model is **pure window scanning**: the sequencer only keeps
//! a musical position (in ticks) and the per-track clip tables; each block
//! schedules in one pass all note occurrences falling due within the absolute
//! tick window `[t0, t1)`. Clips are placed at absolute positions
//! (`start_ticks`), stay audible for `span_ticks`, and their note content
//! repeats every `loop_ticks` within the span (D14 tiling semantics — the
//! UI presents one period for now).
//!
//! BPM changes only affect future rates (the position is counted in ticks, so
//! the musical position is naturally preserved); no incremental clock
//! bookkeeping is kept.
//!
//! Arrangement end (D16): the latest clip end rounded up to a bar; playback
//! stops automatically when the transport reaches it (pending note offs keep
//! draining so the last notes release naturally). An empty arrangement
//! (no clips at all) is unbounded and free-runs.

use crate::realtime::MAX_MIDI_TRACKS;
use crate::realtime::automation::{MAX_AUTO_TRACKS, RtControlClip};
use crate::realtime::commands::NoteEvent;
use crate::realtime::context::ProcessContext;

/// Note time base: ticks per quarter note (consistent with the `resoz_model` convention).
pub const TICKS_PER_QUARTER: u32 = 96;

/// Ticks per step (a sixteenth note).
pub const TICKS_PER_STEP: u32 = 24;

/// Maximum number of notes per clip (D15; mirrors `resoz_model::MAX_CLIP_NOTES`).
pub const MAX_CLIP_NOTES: usize = 64;

/// Maximum number of clips per track (D15; mirrors `resoz_model::MAX_CLIPS_PER_TRACK`).
pub const MAX_TRACK_CLIPS: usize = 8;

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

/// Audio-thread-side representation of a MIDI clip placed on the arrangement
/// timeline (mirrors `resoz_model::MidiClip`, fixed-capacity `Copy`; D14/D15).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtMidiClip {
    /// Absolute start position on the arrangement (in ticks).
    pub start_ticks: u32,
    /// How long the clip stays active (in ticks).
    pub span_ticks: u32,
    /// Content loop length (in ticks).
    pub loop_ticks: u32,
    /// Note collection (the first `note_count` are valid, relative to the clip start).
    pub notes: [RtNote; MAX_CLIP_NOTES],
    /// Number of valid notes.
    pub note_count: usize,
}

impl Default for RtMidiClip {
    fn default() -> Self {
        Self {
            start_ticks: 0,
            // span 0 = inactive: an all-default clip table is an EMPTY track
            // (the scheduler skips span-0 clips); every real clip carries its
            // own placement.
            span_ticks: 0,
            loop_ticks: TICKS_PER_BAR,
            notes: [RtNote::default(); MAX_CLIP_NOTES],
            note_count: 0,
        }
    }
}

/// Ticks per bar (4/4) — default clip span/loop length.
pub const TICKS_PER_BAR: u32 = TICKS_PER_STEP * 16;

impl RtMidiClip {
    /// Appends a note; silently dropped beyond [`MAX_CLIP_NOTES`].
    pub fn push(&mut self, note: RtNote) {
        if self.note_count < MAX_CLIP_NOTES {
            self.notes[self.note_count] = note;
            self.note_count += 1;
        }
    }
}

/// Multi-track sequencer over the arrangement timeline: one global transport
/// clock (BPM / musical position), per-track clip tables and gate tails,
/// converting due events into `(MIDI track, NoteEvent)` at block boundaries on
/// the sample clock.
///
/// Control (automation) clips are independent entities parallel to MIDI
/// tracks (D7/D17): [`Sequencer`] only holds their placements and curves; the
/// delivery target is determined by the node graph's automation routing table
/// and the delivery window by the clip span — this struct knows nothing about
/// either.
///
/// The clock is counted in ticks (musical position), converted to the sample
/// clock via BPM; note offs are recorded as absolute sample positions and
/// emitted at the block boundary containing them.
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
    position_ticks: f64,
    /// Active MIDI track count (D12 dynamic tracks: sent from the control side
    /// via `SetMidiTrackCount`, scheduling only scans `0..track_count`;
    /// unconfigured tracks have default-empty clip tables).
    track_count: usize,
    /// Per-track clip tables (D15: ≤ [`MAX_TRACK_CLIPS`] clips each, batch
    /// replaced via `SetTrackClips`).
    clips: [[RtMidiClip; MAX_TRACK_CLIPS]; MAX_MIDI_TRACKS],
    /// Per-control-track clip tables (D15/D17).
    control_clips: [[RtControlClip; MAX_TRACK_CLIPS]; MAX_AUTO_TRACKS],
    /// Arrangement end in ticks (D16: the latest clip end rounded up to a
    /// bar); `0` = unbounded (empty arrangement, free run).
    arrangement_end_ticks: u32,
    /// Per-track pending NoteOffs (absolute sample position where the
    /// length ends + note).
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
            position_ticks: 0.0,
            track_count: crate::realtime::DEFAULT_MIDI_TRACKS,
            clips: [[RtMidiClip::default(); MAX_TRACK_CLIPS]; MAX_MIDI_TRACKS],
            control_clips: [[RtControlClip::default(); MAX_TRACK_CLIPS]; MAX_AUTO_TRACKS],
            arrangement_end_ticks: 0,
            pending_offs: [None; MAX_MIDI_TRACKS],
        }
    }

    /// Whether playing. RT-safe.
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Current step index (`⌊musical position / step length⌋ mod 16`, used by
    /// the UI as a coarse playhead hint).
    /// RT-safe.
    pub fn step_index(&self) -> usize {
        (self.position_ticks / f64::from(TICKS_PER_STEP)).floor() as usize % 16
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

    /// Play from position 0 (musical position reset to zero). RT-safe.
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

    /// Replaces one MIDI track's clip table in one block (D15 batch; out-of-range
    /// tracks ignored). Also refreshes the arrangement end. RT-safe.
    pub fn set_track_clips(&mut self, track: usize, clips: [RtMidiClip; MAX_TRACK_CLIPS]) {
        if let Some(slot) = self.clips.get_mut(track) {
            *slot = clips;
            self.refresh_arrangement_end();
        }
    }

    /// Replaces one control track's clip table in one block (D15 batch;
    /// out-of-range indices ignored). Also refreshes the arrangement end. RT-safe.
    pub fn set_control_clips(&mut self, index: usize, clips: [RtControlClip; MAX_TRACK_CLIPS]) {
        if let Some(slot) = self.control_clips.get_mut(index) {
            *slot = clips;
            self.refresh_arrangement_end();
        }
    }

    /// Read-only access to all control clip tables (for the block runner to
    /// sample per the routing table). RT-safe.
    pub fn control_clips(&self) -> &[[RtControlClip; MAX_TRACK_CLIPS]; MAX_AUTO_TRACKS] {
        &self.control_clips
    }

    /// Arrangement end in ticks (D16; `0` = unbounded). RT-safe.
    pub fn arrangement_end_ticks(&self) -> u32 {
        self.arrangement_end_ticks
    }

    /// D16: arrangement end = the latest clip end (`start + span`) across all
    /// MIDI and control clips, rounded up to a whole bar.
    fn refresh_arrangement_end(&mut self) {
        let mut latest: u32 = 0;
        for track in &self.clips {
            for clip in track {
                latest = latest.max(clip.start_ticks.saturating_add(clip.span_ticks));
            }
        }
        for track in &self.control_clips {
            for clip in track {
                latest = latest.max(clip.start_ticks.saturating_add(clip.span_ticks));
            }
        }
        self.arrangement_end_ticks = if latest == 0 {
            0
        } else {
            latest.div_ceil(TICKS_PER_BAR) * TICKS_PER_BAR
        };
    }

    /// Advances one block: sends note offs and note ons falling due within the
    /// `[t0, t1)` (tick) window via `emit(track, event)` (offs before ons
    /// within the same block). The window is clamped at the arrangement end
    /// (D16): when the transport reaches it, scheduling stops and the
    /// transport auto-stops; pending offs keep draining afterwards so the
    /// last notes release naturally.
    /// RT-safe.
    pub fn advance(&mut self, ctx: &ProcessContext, emit: &mut dyn FnMut(usize, NoteEvent)) {
        let tps = self.ticks_per_sample();
        let samples_per_tick = 1.0 / tps;
        let block_start = ctx.playback_sample_pos;

        // 1. Due note offs — always processed, even after an auto-stop, so
        //    the arrangement's final notes still release (offs before ons,
        //    keeping each track's handoff clean).
        for (track, pending) in self.pending_offs.iter_mut().enumerate() {
            if let Some((time, note)) = *pending {
                if time < block_start + ctx.frames as u64 {
                    emit(track, NoteEvent::Off { note });
                    *pending = None;
                }
            }
        }

        if !self.playing {
            return;
        }

        // 2. This block's window, clamped at the arrangement end (D16).
        let dt_ticks = ctx.frames as f64 * tps;
        let t0 = self.position_ticks;
        let mut t1 = t0 + dt_ticks;
        let mut stop_at_end = false;
        if self.arrangement_end_ticks > 0 {
            let end = f64::from(self.arrangement_end_ticks);
            if t1 >= end {
                t1 = end;
                stop_at_end = true;
            }
        }

        // 3. Schedule note ons within the window per track (scan active tracks only, D12).
        if t1 > t0 {
            for track in 0..self.track_count {
                self.schedule_track_clips(track, t0, t1, block_start, samples_per_tick, emit);
            }
        }

        self.position_ticks = t1;
        if stop_at_end {
            // Keep the pending offs: they drain in the following blocks and
            // release the arrangement's last notes.
            self.playing = false;
        }
    }

    /// One track's clip table: for every clip audible in the window, note
    /// occurrences (`start + m·loop + note.start`, strictly before the clip
    /// end) starting within the window fire an On + a length-based pending Off.
    #[allow(clippy::too_many_arguments)]
    fn schedule_track_clips(
        &mut self,
        track: usize,
        t0: f64,
        t1: f64,
        block_start: u64,
        samples_per_tick: f64,
        emit: &mut dyn FnMut(usize, NoteEvent),
    ) {
        let clips = self.clips[track];
        for clip in &clips {
            if clip.note_count == 0 || clip.loop_ticks == 0 || clip.span_ticks == 0 {
                continue;
            }
            let start = f64::from(clip.start_ticks);
            let clip_end = start + f64::from(clip.span_ticks);
            let loop_ticks = f64::from(clip.loop_ticks);
            for note in &clip.notes[..clip.note_count] {
                let start_offset = f64::from(note.start_ticks);
                let length = f64::from(note.length_ticks.max(1));
                // The m-th occurrence is at start + m·loop + s; find the first
                // m with an on-time ≥ t0 (may be negative, clamped to 0).
                let mut m = ((t0 - start - start_offset) / loop_ticks).ceil();
                if m < 0.0 {
                    m = 0.0;
                }
                loop {
                    let on_tick = start + m * loop_ticks + start_offset;
                    if on_tick >= t1 || on_tick >= clip_end {
                        break; // past the window or past the clip span
                    }
                    if on_tick >= t0 {
                        let velocity = sanitize_velocity(note.velocity);
                        emit(
                            track,
                            NoteEvent::On {
                                note: note.note,
                                velocity,
                            },
                        );
                        self.set_pending_off(
                            track,
                            on_tick + length,
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
    }

    /// Records a note off: converted to an absolute sample position, landing
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
    use super::{MAX_CLIP_NOTES, MAX_TRACK_CLIPS, RtMidiClip, RtNote, Sequencer, TICKS_PER_BAR};
    use crate::realtime::MAX_MIDI_TRACKS;
    use crate::realtime::automation::{MAX_AUTO_TRACKS, RtAutoPoint, RtControlClip};
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
        run_from(seq, 0, blocks)
    }

    /// Same as [`run`], but the fake sample clock continues from `start`
    /// (pending offs are absolute sample positions — spanning several `run`
    /// calls needs a continuous clock).
    fn run_from(seq: &mut Sequencer, start: u64, blocks: usize) -> Vec<Event> {
        let mut events = Vec::new();
        let mut pos = start;
        for _ in 0..blocks {
            seq.advance(&ctx(pos), &mut |track, event| {
                events.push((track, event));
            });
            pos += BLOCK as u64;
        }
        events
    }

    fn clip_note(start: u32, length: u32, note: u8) -> RtNote {
        RtNote {
            start_ticks: start,
            length_ticks: length,
            note,
            velocity: 0.9,
        }
    }

    /// One clip on one track: `start`/`span`/`loop` in ticks + notes.
    fn place(
        seq: &mut Sequencer,
        track: usize,
        start: u32,
        span: u32,
        loop_ticks: u32,
        notes: &[RtNote],
    ) {
        let mut clip = RtMidiClip {
            start_ticks: start,
            span_ticks: span,
            loop_ticks,
            ..RtMidiClip::default()
        };
        for note in notes {
            clip.push(*note);
        }
        let mut clips = [RtMidiClip::default(); MAX_TRACK_CLIPS];
        clips[0] = clip;
        seq.set_track_clips(track, clips);
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
    fn clip_notes_fire_at_their_positions_with_lengths() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        place(
            &mut seq,
            0,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, 24, 60), clip_note(24, 48, 64)],
        );
        seq.play();

        let events = run(&mut seq, 375);
        assert_eq!(
            ons(&events),
            vec![(0, 60), (0, 64)],
            "notes should fire in order of start"
        );
        // every note has an off (by its own length, not a gate).
        let offs = events
            .iter()
            .filter(|(_, e)| matches!(e, NoteEvent::Off { .. }))
            .count();
        assert_eq!(offs, 2);
    }

    #[test]
    fn clip_plays_at_absolute_position() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // Clip placed on the second bar (384 ticks = 96_000 samples @120 BPM).
        place(
            &mut seq,
            0,
            384,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, 24, 72)],
        );
        seq.play();

        // First bar: silence.
        assert!(
            ons(&run(&mut seq, 375)).is_empty(),
            "nothing should fire before the clip's absolute start"
        );
        // Continue into the second bar (no re-play: the transport keeps
        // running): the note fires once at its absolute position.
        let events = run(&mut seq, 400);
        assert_eq!(ons(&events), vec![(0, 72)]);
        // Position is beyond one bar and still inside the arrangement.
        assert!(seq.position_ticks() >= f64::from(TICKS_PER_BAR));
        assert!(seq.is_playing());
    }

    #[test]
    fn clip_tiles_within_span_and_stops_at_span_end() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // span = 2 × loop: the note fires twice (once per period), no third time.
        place(
            &mut seq,
            0,
            0,
            2 * TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, 24, 60)],
        );
        seq.play();

        // 2 bars = 768 ticks = 192_000 samples = 1_500 blocks; run 3 bars worth.
        let fired: Vec<u8> = run(&mut seq, 2_255)
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
            vec![60, 60],
            "content should tile once per loop within the span, then stop"
        );
        // Auto-stop at the arrangement end (D16: 2 bars rounded up).
        assert!(
            !seq.is_playing(),
            "transport should stop at the arrangement end"
        );
    }

    #[test]
    fn arrangement_end_is_latest_clip_end_ceiled_to_bar() {
        let mut seq = Sequencer::new(SR);
        place(
            &mut seq,
            0,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, 24, 60)],
        );
        assert_eq!(seq.arrangement_end_ticks(), TICKS_PER_BAR);
        // A later clip on another track extends the arrangement
        // (480 + 96 = 576 → ceiled to 2 bars).
        place(&mut seq, 1, 384 + 96, 96, 96, &[]);
        assert_eq!(
            seq.arrangement_end_ticks(),
            2 * TICKS_PER_BAR,
            "end = latest clip end ceiled to a bar"
        );
        // Control clips count too.
        let mut clips = [RtControlClip::default(); MAX_TRACK_CLIPS];
        clips[0] = RtControlClip {
            start_ticks: 5 * TICKS_PER_BAR,
            span_ticks: TICKS_PER_BAR,
            ..RtControlClip::default()
        };
        seq.set_control_clips(0, clips);
        assert_eq!(seq.arrangement_end_ticks(), 6 * TICKS_PER_BAR);
        // Removing all content makes the arrangement unbounded again.
        seq.set_track_clips(0, [RtMidiClip::default(); MAX_TRACK_CLIPS]);
        seq.set_track_clips(1, [RtMidiClip::default(); MAX_TRACK_CLIPS]);
        seq.set_control_clips(0, [RtControlClip::default(); MAX_TRACK_CLIPS]);
        assert_eq!(seq.arrangement_end_ticks(), 0);
    }

    #[test]
    fn auto_stop_keeps_pending_offs_draining() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // One note right before the arrangement end: its off lands after the stop.
        place(
            &mut seq,
            0,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(TICKS_PER_BAR - 24, 48, 60)],
        );
        seq.play();
        // @48 kHz: the stop lands in block 750 (384 ticks = 96_000 samples);
        // run exactly up to it so the off (408 ticks = 102_000 samples) is
        // still pending.
        run(&mut seq, 750);

        assert!(!seq.is_playing(), "auto-stopped at the arrangement end");
        // The note's off (408 ticks = 102_000 samples) lands after the stop
        // point: it must still drain in the following blocks.
        let events = run_from(&mut seq, 750 * 128, 100);
        assert!(
            events
                .iter()
                .any(|(_, e)| matches!(e, NoteEvent::Off { .. })),
            "the pending off should still fire after the auto-stop"
        );
        assert!(
            events
                .iter()
                .all(|(_, e)| matches!(e, NoteEvent::Off { .. })),
            "no new note ons after the auto-stop"
        );
    }

    #[test]
    fn empty_arrangement_free_runs() {
        let mut seq = Sequencer::new(SR);
        seq.play();
        run(&mut seq, 2_000);
        assert!(
            seq.is_playing(),
            "an empty arrangement has no end: no auto-stop"
        );
        assert_eq!(seq.arrangement_end_ticks(), 0);
    }

    #[test]
    fn two_tracks_fire_on_the_same_clock() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        place(
            &mut seq,
            0,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, 24, 60)],
        );
        place(
            &mut seq,
            1,
            0,
            TICKS_PER_BAR,
            TICKS_PER_BAR,
            &[clip_note(0, 24, 72)],
        );
        seq.play();

        // 100 blocks = 12_800 samples: covers both note offs (24 and 48 ticks
        // → 6_000 and 12_000 samples).
        let events = run(&mut seq, 100);
        assert_eq!(
            ons(&events),
            vec![(0, 60), (1, 72)],
            "both tracks fire on the same clock, in track order"
        );
        let offs = events
            .iter()
            .filter(|(_, e)| matches!(e, NoteEvent::Off { .. }))
            .count();
        assert_eq!(offs, 2, "every note should have a length-based off");
    }

    #[test]
    fn clip_over_capacity_is_truncated() {
        let mut clip = RtMidiClip::default();
        for i in 0..(MAX_CLIP_NOTES + 10) {
            clip.push(clip_note(i as u32 * 24, 24, 60));
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
    fn bpm_change_alters_event_rate() {
        let mut seq = Sequencer::new(SR);
        // 16 notes per bar (every 16th note), unbounded arrangement.
        let mut clip = RtMidiClip {
            span_ticks: 256 * 384,
            loop_ticks: 384,
            ..RtMidiClip::default()
        };
        for i in 0..16u32 {
            clip.push(clip_note(i * 24, 24, 60));
        }
        let mut clips = [RtMidiClip::default(); MAX_TRACK_CLIPS];
        clips[0] = clip;
        seq.set_track_clips(0, clips);

        let count_ons = |events: &[Event]| {
            events
                .iter()
                .filter(|(_, e)| matches!(e, NoteEvent::On { .. }))
                .count()
        };
        // 1500 blocks = 192_000 samples = 768 ticks @120 BPM = 2 bars → 32 ons;
        // at 240 BPM the same samples span 4 bars → 64 ons.
        seq.set_bpm(120.0);
        seq.play();
        let slow = count_ons(&run(&mut seq, 1_500));
        seq.set_bpm(240.0);
        seq.play();
        let fast = count_ons(&run(&mut seq, 1_500));
        assert_eq!(slow, 32);
        assert_eq!(fast, 64, "240 BPM should fire exactly twice as many notes");
    }

    #[test]
    fn stop_quiets_every_track() {
        let mut seq = Sequencer::new(SR);
        for track in 0..4 {
            place(&mut seq, track, 0, 256 * 384, 384, &[clip_note(0, 24, 60)]);
        }
        seq.play();
        let events = run(&mut seq, 1);
        assert_eq!(
            ons(&events).len(),
            4,
            "the first four tracks should fire at position 0 simultaneously"
        );

        seq.stop();
        let events = run(&mut seq, 375);
        assert!(events.is_empty(), "no more events after stopping");
        assert!(!seq.is_playing());
    }

    #[test]
    fn note_off_comes_before_next_note_on() {
        let mut seq = Sequencer::new(SR);
        seq.set_bpm(120.0);
        // Two consecutive 16th notes: on@0 (24 ticks), on@24; the first off
        // must be emitted in the block before the second on.
        place(
            &mut seq,
            0,
            0,
            256 * 384,
            384,
            &[clip_note(0, 24, 60), clip_note(24, 24, 64)],
        );
        seq.play();

        let events = run(&mut seq, 60);
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
    fn control_clips_are_independent_entities() {
        let mut seq = Sequencer::new(SR);
        let mut clips = [RtControlClip::default(); MAX_TRACK_CLIPS];
        clips[5] = RtControlClip {
            start_ticks: 0,
            span_ticks: 384,
            loop_ticks: 96, // decoupled from any MIDI clip loop (D7)
            curve: {
                let mut curve = crate::realtime::automation::RtAutomation::default();
                curve.push(RtAutoPoint {
                    tick: 0,
                    value: 0.5,
                });
                curve
            },
        };
        seq.set_control_clips(5, clips);
        seq.set_control_clips(MAX_AUTO_TRACKS, clips); // out-of-range index ignored
        let tables = seq.control_clips();
        assert_eq!(tables[5][5].loop_ticks, 96);
        assert_eq!(tables[5][5].curve.point_count, 1);
        assert!(
            tables
                .iter()
                .enumerate()
                .all(|(i, t)| i == 5 || !t[0].curve.is_active()),
            "other control tracks should keep default empty curves"
        );
        assert!(
            tables[5]
                .iter()
                .enumerate()
                .all(|(i, c)| i == 5 || !c.curve.is_active()),
            "other slots of track 5 should stay empty"
        );
        // Control clip writes do not disturb MIDI scheduling.
        place(&mut seq, 0, 0, 256 * 384, 384, &[clip_note(0, 24, 60)]);
        seq.play();
        assert_eq!(ons(&run(&mut seq, 30)), vec![(0, 60)]);
    }

    #[test]
    fn track_count_gates_scheduling() {
        let mut seq = Sequencer::new(SR);
        place(&mut seq, 5, 0, 256 * 384, 384, &[clip_note(0, 24, 60)]);
        seq.set_track_count(4);
        seq.play();
        assert!(
            ons(&run(&mut seq, 30)).is_empty(),
            "tracks beyond the active count should not schedule"
        );
        seq.set_track_count(8);
        seq.play(); // reset the transport so position 0 falls back into the scheduling window
        assert_eq!(ons(&run(&mut seq, 30)), vec![(5, 60)]);
        // Clamped to the capacity limit.
        seq.set_track_count(MAX_MIDI_TRACKS + 10);
        assert_eq!(seq.track_count(), MAX_MIDI_TRACKS);
    }
}
