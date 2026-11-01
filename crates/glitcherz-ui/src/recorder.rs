//! MIDI recording state machine (spec `.agents/spec/recording.md` R2/R6).
//!
//! The engine only stamps live events and hands them back (see the capture tap
//! in `glitcherz-engine`); **all** recording semantics live here: pre-roll
//! filtering, note pairing, loop-pass overwrite and the take → clip
//! conversion. That split is deliberate (spec D6) — the engine never learns
//! what "record" means, and this module is testable without audio hardware or
//! a running engine.
//!
//! R6 moved ownership from a selected track to the document: every track whose
//! `Timeline In` recording input is cabled is armed, and one playback can feed
//! several of them at once (D22/D23). Each armed track keeps its own take; the
//! UI writes one clip per non-empty take inside a single undo step (D15).
//!
//! Phases:
//!
//! ```text
//! Idle ──record──▶ PreRoll ──transport reaches start_ticks──▶ Recording
//!                    │                                          │
//!                    └──────────── stop / cancel ───────────────┘
//! ```
//!
//! During pre-roll the transport plays normally (so the user hears the click
//! and the arrangement) and captured events are **dropped by timestamp** — the
//! filter is `ticks >= start_ticks`, not a frame-order guess, so it is exact
//! even when a block's worth of events arrives late.

use std::collections::BTreeMap;

use glitcherz_engine::realtime::CapturedNote;
use glitcherz_engine::realtime::commands::NoteEvent;
use glitcherz_model::{MAX_ARRANGEMENT_TICKS, MAX_CLIP_NOTES, MidiClip, Note};

/// Where the recorder is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordPhase {
    /// Not recording.
    Idle,
    /// Transport running towards `start_ticks`; events are discarded.
    PreRoll,
    /// Capturing.
    Recording,
}

/// One closed note, in absolute transport ticks.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TakenNote {
    start: f64,
    end: f64,
    note: u8,
    velocity: u8,
}

/// Per-track open-note table and closed material.
struct TrackTake {
    /// Open note-ons by pitch: `(start_ticks, velocity)`.
    open: [Option<(f64, u8)>; 128],
    /// Closed notes, absolute ticks.
    taken: Vec<TakenNote>,
}

impl TrackTake {
    fn new() -> Self {
        Self {
            open: [None; 128],
            taken: Vec::new(),
        }
    }

    fn clear(&mut self) {
        self.open = [None; 128];
        self.taken.clear();
    }
}

/// MIDI recording state (one take session, one take per armed track).
pub struct MidiRecorder {
    phase: RecordPhase,
    /// Tracks armed by the `Timeline In` wiring when the take started. Sorted
    /// and deduplicated; the recorder never adds to it mid-take (a cable added
    /// while recording starts capturing on the next take).
    tracks: Vec<usize>,
    /// Absolute tick where capture starts; the clip's start position.
    pub start_ticks: f64,
    /// Per-armed-track material.
    takes: BTreeMap<usize, TrackTake>,
    /// Last observed transport position (loop-wrap detection).
    last_position: f64,
    /// Completed loop passes. Each wrap **clears** every take (D12: the last
    /// pass wins, no comping).
    pub passes: u32,
}

impl Default for MidiRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl MidiRecorder {
    /// Idle recorder.
    pub fn new() -> Self {
        Self {
            phase: RecordPhase::Idle,
            tracks: Vec::new(),
            start_ticks: 0.0,
            takes: BTreeMap::new(),
            last_position: 0.0,
            passes: 0,
        }
    }

    /// Current phase.
    pub fn phase(&self) -> RecordPhase {
        self.phase
    }

    /// Whether a take is in progress (pre-roll counts: the transport is
    /// already rolling and the button must offer "stop").
    pub fn is_active(&self) -> bool {
        self.phase != RecordPhase::Idle
    }

    /// Whether events are actually being kept.
    pub fn is_capturing(&self) -> bool {
        self.phase == RecordPhase::Recording
    }

    /// Tracks armed when the take started, in track order.
    pub fn tracks(&self) -> &[usize] {
        &self.tracks
    }

    /// Number of notes closed so far (what the status line reports).
    pub fn note_count(&self) -> usize {
        self.takes.values().map(|t| t.taken.len()).sum()
    }

    /// Last transport position observed by [`Self::observe_position`]. Used as
    /// the take's end when the transport has already rewound by the time the
    /// take is closed.
    pub fn last_position(&self) -> f64 {
        self.last_position
    }

    /// Enters pre-roll for `tracks`, capturing from `start_ticks` on. The
    /// transport position is recorded too, so the first wrap is detected even
    /// if the loop turns before the first note arrives.
    pub fn begin(&mut self, mut tracks: Vec<usize>, start_ticks: f64, position: f64) {
        tracks.sort_unstable();
        tracks.dedup();
        self.phase = RecordPhase::PreRoll;
        self.tracks = tracks;
        self.start_ticks = start_ticks;
        self.takes = self.tracks.iter().map(|&t| (t, TrackTake::new())).collect();
        self.last_position = position;
        self.passes = 0;
    }

    /// Drops everything and returns to idle (cancel, project new/open).
    pub fn reset(&mut self) {
        self.phase = RecordPhase::Idle;
        self.tracks.clear();
        self.takes.clear();
        self.passes = 0;
    }

    /// Feeds one frame's transport position through the state machine:
    /// promotes pre-roll to recording and clears every take when the transport
    /// jumps backwards (a loop wrap, or a seek — either way the material
    /// collected so far belongs to a position we left).
    pub fn observe_position(&mut self, position: f64) {
        if self.phase == RecordPhase::PreRoll && position >= self.start_ticks {
            self.phase = RecordPhase::Recording;
        }
        if self.is_capturing() && position + 0.5 < self.last_position {
            for take in self.takes.values_mut() {
                take.clear();
            }
            self.passes = self.passes.saturating_add(1);
        }
        self.last_position = position;
    }

    /// Feeds one captured event.
    ///
    /// Only **track-addressed** events are recorded (R6 D22): a device wired to
    /// an instrument directly is a live performance path, not a take. An event
    /// stamped at or after `start_ticks` is itself proof that the pre-roll is
    /// over, so the phase is promoted here as well as in
    /// [`Self::observe_position`]: the frame order (drain vs. observe) then
    /// cannot drop the first notes of a take.
    pub fn push(&mut self, captured: CapturedNote) {
        if self.phase == RecordPhase::Idle || captured.ticks < self.start_ticks {
            return;
        }
        let Some(track) = captured.track else {
            return;
        };
        let Some(take) = self.takes.get_mut(&track) else {
            return;
        };
        self.phase = RecordPhase::Recording;
        match captured.event {
            NoteEvent::On { note, velocity } => {
                let pitch = usize::from(note & 0x7f);
                // A retrigger without an off (a lost event, or the previous
                // note's off landing after a wrap) closes the old one here
                // rather than leaking it until stop.
                if let Some((start, vel)) = take.open[pitch].take() {
                    close(take, start, captured.ticks, note, vel);
                }
                take.open[pitch] = Some((captured.ticks, velocity_to_u8(velocity)));
            }
            NoteEvent::Off { note } => {
                let pitch = usize::from(note & 0x7f);
                if let Some((start, velocity)) = take.open[pitch].take() {
                    close(take, start, captured.ticks, note, velocity);
                }
            }
        }
    }

    /// Ends one track's take early (R6 D33: its input cable was removed
    /// mid-recording). Returns the track's clip-relative notes; `None` when the
    /// track was not armed or nothing was played. When the last armed track
    /// leaves, the session returns to idle.
    pub fn finish_track(&mut self, track: usize, position: f64) -> Option<Vec<Note>> {
        let end = position.max(self.start_ticks + 1.0);
        let notes = self.close_take(track, end)?;
        self.tracks.retain(|&t| t != track);
        if self.tracks.is_empty() {
            self.reset();
        }
        Some(notes)
    }

    /// Ends the take: closes whatever is still held and converts every track's
    /// result to clip-relative [`Note`]s, sorted by track. Empty tracks are
    /// omitted; the caller writes no clip and records no undo step for them.
    pub fn finish(&mut self, position: f64) -> Vec<(usize, Vec<Note>)> {
        let end = position.max(self.start_ticks + 1.0);
        let tracks: Vec<usize> = self.tracks.clone();
        let mut out = Vec::new();
        for track in tracks {
            if let Some(notes) = self.close_take(track, end) {
                out.push((track, notes));
            }
        }
        self.phase = RecordPhase::Idle;
        self.tracks.clear();
        self.takes.clear();
        self.passes = 0;
        out
    }

    /// Closes and removes one track's material, mapping it to clip-relative
    /// notes. `None` when nothing was played.
    fn close_take(&mut self, track: usize, end: f64) -> Option<Vec<Note>> {
        let mut take = self.takes.remove(&track)?;
        for pitch in 0..take.open.len() {
            if let Some((start, velocity)) = take.open[pitch].take() {
                close(&mut take, start, end, pitch as u8, velocity);
            }
        }
        if take.taken.is_empty() {
            return None;
        }
        let mut notes: Vec<Note> = take
            .taken
            .drain(..)
            .map(|taken| {
                let start = (taken.start - self.start_ticks).round().max(0.0) as i64;
                let length = (taken.end - taken.start).round().max(1.0) as i64;
                Note {
                    start,
                    length,
                    note: taken.note,
                    velocity: taken.velocity.max(1),
                    channel: 0,
                }
            })
            .collect();
        notes.sort_by_key(|n| (n.start, n.note, n.length, n.velocity));
        Some(notes)
    }
}

/// Closes one open note into `take` (bounds-checked by [`MAX_CLIP_NOTES`]).
fn close(take: &mut TrackTake, start: f64, end: f64, note: u8, velocity: u8) {
    if take.taken.len() >= MAX_CLIP_NOTES {
        return;
    }
    // A zero-length note is legal MIDI but useless in a roll; one tick is
    // the model's minimum and keeps it visible/editable.
    take.taken.push(TakenNote {
        start,
        end: end.max(start + 1.0),
        note,
        velocity,
    });
}

/// Velocity `0.0..=1.0` → MIDI wire range `1..=127` (never 0: a note with
/// velocity 0 is a note-off on the wire).
fn velocity_to_u8(velocity: f32) -> u8 {
    if !velocity.is_finite() {
        return 100;
    }
    (velocity.clamp(0.0, 1.0) * 127.0).round().clamp(1.0, 127.0) as u8
}

/// Wraps a finished take into the clip that carries it.
///
/// The clip starts on the bar line the take was armed for and spans the whole
/// bars its material covers (`loop_ticks == span_ticks`: a take must not
/// loop). The span is clamped so the clip stays inside the arrangement bound;
/// notes past that bound keep their data but no longer play.
///
/// Returns `None` for an empty take — the caller then writes nothing.
pub fn take_clip(start_ticks: f64, notes: &[Note], bar_ticks: u32) -> Option<MidiClip> {
    if notes.is_empty() {
        return None;
    }
    let bar = u64::from(bar_ticks.max(1));
    let start =
        (start_ticks.round().max(0.0) as u64).min(MAX_ARRANGEMENT_TICKS.saturating_sub(bar));
    let end_rel = notes.iter().map(|n| n.end()).max().unwrap_or(0).max(0) as u64;
    let span = (end_rel.div_ceil(bar) * bar).max(bar);
    let limit = MAX_ARRANGEMENT_TICKS.saturating_sub(start).max(1);
    let mut clip = MidiClip::new(start as u32);
    clip.span_ticks = span.min(limit) as u32;
    clip.loop_ticks = clip.span_ticks;
    clip.sequence.notes = notes.to_vec();
    Some(clip)
}

#[cfg(test)]
mod tests {
    use super::{MidiRecorder, RecordPhase, take_clip, velocity_to_u8};
    use glitcherz_engine::realtime::CapturedNote;
    use glitcherz_engine::realtime::commands::NoteEvent;
    use glitcherz_model::{MAX_ARRANGEMENT_TICKS, Note};

    fn captured(ticks: f64, event: NoteEvent, track: Option<usize>) -> CapturedNote {
        CapturedNote {
            ticks,
            event,
            track,
        }
    }

    fn on(ticks: f64, note: u8) -> CapturedNote {
        captured(
            ticks,
            NoteEvent::On {
                note,
                velocity: 1.0,
            },
            Some(3),
        )
    }

    fn off(ticks: f64, note: u8) -> CapturedNote {
        captured(ticks, NoteEvent::Off { note }, Some(3))
    }

    fn on_track(ticks: f64, note: u8, track: usize) -> CapturedNote {
        captured(
            ticks,
            NoteEvent::On {
                note,
                velocity: 1.0,
            },
            Some(track),
        )
    }

    fn off_track(ticks: f64, note: u8, track: usize) -> CapturedNote {
        captured(ticks, NoteEvent::Off { note }, Some(track))
    }

    /// A recorder already out of pre-roll at tick 960, armed for track 3.
    fn recording() -> MidiRecorder {
        let mut recorder = MidiRecorder::new();
        recorder.begin(vec![3], 960.0, 0.0);
        recorder.observe_position(960.0);
        assert_eq!(recorder.phase(), RecordPhase::Recording);
        recorder
    }

    fn only_notes(finished: Vec<(usize, Vec<Note>)>) -> Vec<Note> {
        assert_eq!(finished.len(), 1, "one non-empty take expected");
        finished.into_iter().next().expect("take").1
    }

    #[test]
    fn pre_roll_discards_events_until_the_start_tick() {
        let mut recorder = MidiRecorder::new();
        recorder.begin(vec![0], 1920.0, 0.0);
        recorder.observe_position(500.0);
        assert_eq!(recorder.phase(), RecordPhase::PreRoll);
        recorder.push(captured(
            500.0,
            NoteEvent::On {
                note: 60,
                velocity: 1.0,
            },
            Some(0),
        ));
        assert_eq!(recorder.note_count(), 0, "pre-roll material is dropped");

        recorder.observe_position(1920.0);
        assert_eq!(recorder.phase(), RecordPhase::Recording);
        recorder.push(captured(
            1920.0,
            NoteEvent::On {
                note: 60,
                velocity: 1.0,
            },
            Some(0),
        ));
        recorder.push(captured(2400.0, NoteEvent::Off { note: 60 }, Some(0)));
        assert_eq!(recorder.note_count(), 1);
    }

    #[test]
    fn a_stamped_event_promotes_out_of_pre_roll_by_itself() {
        // The drain/observe order within a frame must not matter: an event at
        // or past the start tick proves the pre-roll is over.
        let mut recorder = MidiRecorder::new();
        recorder.begin(vec![0], 960.0, 0.0);
        recorder.push(captured(
            960.0,
            NoteEvent::On {
                note: 60,
                velocity: 1.0,
            },
            Some(0),
        ));
        assert_eq!(recorder.phase(), RecordPhase::Recording);
        recorder.push(captured(1200.0, NoteEvent::Off { note: 60 }, Some(0)));
        assert_eq!(recorder.finish(1200.0).len(), 1);
    }

    #[test]
    fn node_addressed_events_are_not_recorded() {
        // R6: a device wired straight to an instrument is a performance path;
        // only a cable into a track recording input arms a take.
        let mut recorder = recording();
        recorder.push(captured(
            1000.0,
            NoteEvent::On {
                note: 60,
                velocity: 1.0,
            },
            None,
        ));
        recorder.push(captured(1100.0, NoteEvent::Off { note: 60 }, None));
        assert_eq!(recorder.note_count(), 0);
    }

    #[test]
    fn events_are_made_relative_to_the_start_tick() {
        let mut recorder = recording();
        recorder.push(on(1200.0, 64));
        recorder.push(off(1440.0, 64));
        let notes = only_notes(recorder.finish(1500.0));
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].start, 240, "1200 - 960");
        assert_eq!(notes[0].length, 240);
        assert_eq!(notes[0].note, 64);
    }

    #[test]
    fn a_held_note_is_closed_at_stop() {
        let mut recorder = recording();
        recorder.push(on(1000.0, 67));
        let notes = only_notes(recorder.finish(1400.0));
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].start, 40);
        assert_eq!(notes[0].length, 400);
    }

    #[test]
    fn a_retrigger_closes_the_previous_note() {
        let mut recorder = recording();
        recorder.push(on(1000.0, 60));
        recorder.push(on(1200.0, 60));
        recorder.push(off(1400.0, 60));
        let notes = only_notes(recorder.finish(1400.0));
        assert_eq!(notes.len(), 2, "the second note-on closes the first");
        assert_eq!(notes[0].start, 40);
        assert_eq!(notes[0].length, 200);
        assert_eq!(notes[1].start, 240);
    }

    #[test]
    fn two_armed_tracks_get_independent_takes() {
        let mut recorder = MidiRecorder::new();
        recorder.begin(vec![1, 4], 960.0, 0.0);
        recorder.observe_position(960.0);
        recorder.push(on_track(1000.0, 60, 1));
        recorder.push(on_track(1000.0, 67, 4));
        recorder.push(off_track(1200.0, 60, 1));
        // Track 4's note is left held; stop closes it.
        let finished = recorder.finish(1300.0);
        assert_eq!(finished.len(), 2);
        assert_eq!(finished[0].0, 1);
        assert_eq!(finished[0].1.len(), 1);
        assert_eq!(finished[1].0, 4);
        assert_eq!(finished[1].1[0].note, 67);
    }

    #[test]
    fn an_unarmed_track_is_ignored() {
        let mut recorder = recording();
        recorder.push(on_track(1000.0, 62, 7));
        assert_eq!(recorder.note_count(), 0);
    }

    #[test]
    fn finishing_one_track_leaves_the_others_recording() {
        let mut recorder = MidiRecorder::new();
        recorder.begin(vec![1, 4], 960.0, 0.0);
        recorder.observe_position(960.0);
        recorder.push(on_track(1000.0, 60, 1));
        recorder.push(on_track(1000.0, 67, 4));
        let notes = recorder
            .finish_track(1, 1200.0)
            .expect("track 1 has material");
        assert_eq!(notes.len(), 1);
        assert_eq!(recorder.tracks(), &[4]);
        assert_eq!(recorder.note_count(), 0, "track 4 still holds its note");
        let rest = recorder.finish(1300.0);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].0, 4);
    }

    #[test]
    fn a_loop_wrap_keeps_only_the_last_pass() {
        let mut recorder = MidiRecorder::new();
        recorder.begin(vec![3, 4], 960.0, 0.0);
        recorder.observe_position(960.0);
        recorder.push(on(1000.0, 60));
        recorder.push(off(1100.0, 60));
        recorder.push(on_track(1000.0, 70, 4));
        assert_eq!(recorder.note_count(), 1);
        // Transport jumped back to the loop start: the first pass is dropped.
        recorder.observe_position(0.0);
        assert_eq!(recorder.note_count(), 0);
        assert_eq!(recorder.passes, 1);
        recorder.push(on(1000.0, 62));
        recorder.push(off(1100.0, 62));
        let notes = only_notes(recorder.finish(1200.0));
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].note, 62);
    }

    #[test]
    fn finish_on_an_empty_take_records_nothing() {
        let mut recorder = recording();
        assert!(recorder.finish(1000.0).is_empty());
        assert_eq!(recorder.phase(), RecordPhase::Idle);
    }

    #[test]
    fn reset_returns_to_idle() {
        let mut recorder = recording();
        recorder.push(on(1000.0, 60));
        recorder.reset();
        assert!(!recorder.is_active());
        assert_eq!(recorder.phase(), RecordPhase::Idle);
        assert_eq!(recorder.note_count(), 0);
    }

    #[test]
    fn velocity_maps_into_the_wire_range() {
        assert_eq!(velocity_to_u8(1.0), 127);
        assert_eq!(velocity_to_u8(0.0), 1, "0 would read as a note-off");
        assert_eq!(velocity_to_u8(f32::NAN), 100);
    }

    #[test]
    fn take_clip_spans_whole_bars_and_does_not_loop() {
        // One bar is 960 ticks (4/4 at 960 PPQ); a note ending at 1000 spills
        // into the second bar, so the clip must span two.
        let notes = vec![
            Note {
                start: 0,
                length: 480,
                ..Note::default()
            },
            Note {
                start: 720,
                length: 280,
                ..Note::default()
            },
        ];
        let clip = take_clip(1920.0, &notes, 960).expect("non-empty take");
        assert_eq!(clip.start_ticks, 1920);
        assert_eq!(clip.span_ticks, 1920, "1000 ticks rounds up to two bars");
        assert_eq!(clip.loop_ticks, clip.span_ticks, "a take must not loop");
        assert_eq!(clip.notes().len(), 2);
    }

    #[test]
    fn take_clip_keeps_at_least_one_bar() {
        let notes = vec![Note {
            start: 0,
            length: 10,
            ..Note::default()
        }];
        let clip = take_clip(0.0, &notes, 960).expect("non-empty take");
        assert_eq!(clip.span_ticks, 960);
    }

    #[test]
    fn take_clip_rounds_a_fractional_start_and_stays_in_bounds() {
        // A transport position read back as f32 can be a hair off the bar.
        let notes = vec![Note::default()];
        let clip = take_clip(1920.4, &notes, 960).expect("non-empty take");
        assert_eq!(clip.start_ticks, 1920);

        // At the very end of the arrangement the span is clipped, not wrapped.
        let last_bar = (MAX_ARRANGEMENT_TICKS - 960) as f64;
        let clip = take_clip(last_bar + 100.0, &notes, 960).expect("non-empty take");
        assert_eq!(
            u64::from(clip.start_ticks) + u64::from(clip.span_ticks),
            MAX_ARRANGEMENT_TICKS
        );
    }

    #[test]
    fn take_clip_is_none_for_an_empty_take() {
        assert!(take_clip(0.0, &[], 960).is_none());
    }
}
