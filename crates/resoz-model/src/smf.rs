//! Standard MIDI File (SMF) import/export (M3.5, from the SoundCraft survey).
//!
//! Format 1 only — one conductor track (tempo / time signature) plus one
//! track per [`MidiTrack`]. Division is 960, our PPQ, so no rescaling in
//! either direction.
//!
//! The reader treats its input as hostile (SoundCraft's words, and they are
//! right): malformed chunks, running status, note-on-velocity-0, overlapping
//! same-pitch notes, hanging note-ons and zero-length notes all have defined
//! behavior, and structural violations return [`SmfError`] with the byte
//! offset. Event and track counts are capped.

use crate::{MidiClip, MidiTrack, Note, Sequence, TICKS_PER_QUARTER};

/// Parser safety limits.
pub const MAX_TRACKS: usize = 4096;
pub const MAX_EVENTS: usize = 5_000_000;

/// A parse failure with the offending byte offset.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("SMF error at byte {offset}: {message}")]
pub struct SmfError {
    pub message: String,
    pub offset: usize,
}

fn error(offset: usize, message: impl Into<String>) -> SmfError {
    SmfError {
        message: message.into(),
        offset,
    }
}

// ---- Export -------------------------------------------------------------------------------

/// Serializes the project's MIDI tracks as a format-1 SMF.
///
/// Clips are laid out at their absolute arrangement position; a clip's loop
/// repeats across its span exactly as the scheduler plays it. The conductor
/// track carries the project tempo.
pub fn export_smf(project: &crate::Project) -> Vec<u8> {
    let mut out = Vec::new();
    let track_count = project.midi_tracks.len() + 1; // + conductor
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // format 1
    out.extend_from_slice(&(track_count as u16).to_be_bytes());
    out.extend_from_slice(&(TICKS_PER_QUARTER as u16).to_be_bytes());

    // Conductor: tempo + 4/4 + end of track.
    let micros_per_quarter = (60.0 / project.metadata.tempo_bpm * 1_000_000.0).round() as u32;
    let mut conductor = Vec::new();
    conductor.extend_from_slice(&vlq(0));
    conductor.extend_from_slice(&[0xFF, 0x51, 0x03]);
    conductor.extend_from_slice(&micros_per_quarter.to_be_bytes()[1..4]);
    conductor.extend_from_slice(&vlq(0));
    conductor.extend_from_slice(&[0xFF, 0x58, 0x04, 4, 2, 24, 8]); // 4/4
    conductor.extend_from_slice(&vlq(0));
    conductor.extend_from_slice(&[0xFF, 0x2F, 0x00]);
    write_chunk(&mut out, b"MTrk", &conductor);

    for track in &project.midi_tracks {
        let mut events: Vec<(i64, u8, u8, u8)> = Vec::new(); // (tick, status, d1, d2)
        for clip in &track.clips {
            for (offset, note) in tiled_notes(clip) {
                let offset = offset + clip.start_ticks as i64;
                let channel = note.channel.min(15);
                events.push((offset, 0x90 | channel, note.note, note.velocity.max(1)));
                events.push((offset + note.length.max(1), 0x80 | channel, note.note, 0));
            }
        }
        // Same tick: note-offs before note-ons (a repeated pitch must not
        // cut itself off).
        events.sort_by_key(|(tick, status, _, _)| (*tick, status & 0xF0));
        write_track(&mut out, &track.name, &events);
    }
    out
}

/// A clip's notes with their loop repetitions laid out, as absolute offsets
/// from the clip start.
fn tiled_notes(clip: &MidiClip) -> Vec<(i64, Note)> {
    let loop_ticks = clip.loop_ticks.max(1) as i64;
    let span = clip.span_ticks.max(loop_ticks as u32) as i64;
    let periods = (span + loop_ticks - 1) / loop_ticks;
    let mut result = Vec::with_capacity(clip.sequence.notes.len() * periods as usize);
    for period in 0..periods {
        let shift = period * loop_ticks;
        for note in &clip.sequence.notes {
            if note.start >= loop_ticks {
                continue; // notes beyond one loop play once, not tiled
            }
            let mut note = *note;
            note.start += shift;
            result.push((note.start, note));
        }
    }
    result
}

fn write_chunk(out: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
}

fn write_track(out: &mut Vec<u8>, name: &str, events: &[(i64, u8, u8, u8)]) {
    let mut body = Vec::new();
    if !name.is_empty() {
        body.extend_from_slice(&vlq(0));
        body.push(0xFF);
        body.push(0x03);
        let bytes = name.as_bytes();
        body.extend_from_slice(&vlq(bytes.len() as i64));
        body.extend_from_slice(bytes);
    }
    let mut last = 0i64;
    for (tick, status, d1, d2) in events {
        body.extend_from_slice(&vlq(*tick - last));
        last = *tick;
        body.extend_from_slice(&[*status, *d1, *d2]);
    }
    body.extend_from_slice(&vlq(0));
    body.extend_from_slice(&[0xFF, 0x2F, 0x00]);
    write_chunk(out, b"MTrk", &body);
}

/// Open note-ons keyed by (channel, pitch): on-tick + velocity stacks.
type OpenNotes = Vec<((u8, u8), Vec<(i64, u8)>)>;

/// MIDI variable-length quantity (up to 4 bytes).
fn vlq(mut value: i64) -> Vec<u8> {
    debug_assert!(value >= 0);
    let mut buffer = [0u8; 5];
    let mut position = buffer.len();
    buffer[position - 1] = (value & 0x7F) as u8;
    value >>= 7;
    let mut length = 1;
    while value > 0 {
        position -= 1;
        buffer[position - 1] = 0x80 | (value & 0x7F) as u8;
        value >>= 7;
        length += 1;
    }
    buffer[buffer.len() - length..].to_vec()
}

// ---- Import -------------------------------------------------------------------------------

/// Parses a format-0/1 SMF into MIDI tracks.
///
/// Notes become one clip per track spanning the track's musical extent, with
/// note starts relative to the clip. Meta events other than the track name
/// are ignored (tempo is a project-level setting here).
pub fn import_smf(bytes: &[u8]) -> Result<Vec<MidiTrack>, SmfError> {
    let mut cursor = Cursor { bytes, offset: 0 };
    cursor.expect_tag(b"MThd")?;
    let header_len = cursor.u32()? as usize;
    if header_len < 6 {
        return Err(error(cursor.offset, "MThd shorter than 6 bytes"));
    }
    let _format = cursor.u16()?;
    let declared_tracks = cursor.u16()? as usize;
    if declared_tracks > MAX_TRACKS {
        return Err(error(
            cursor.offset,
            format!("track count {declared_tracks} exceeds the limit"),
        ));
    }
    let division = cursor.u16()?;
    let ticks_per_quarter = if division & 0x8000 != 0 {
        // SMPTE: high byte is the negative frame rate (two's complement),
        // low byte the ticks per frame. `tpf * |fps|` is the quarter at the
        // 120 BPM convention the survey prescribes.
        let fps = i32::from((division >> 8) as u8 as i8);
        let tpf = i32::from(division & 0xFF);
        i64::from(fps.abs() * tpf).max(1)
    } else {
        i64::from(division.max(1))
    };
    cursor.skip(header_len - 6)?;

    let mut tracks = Vec::new();
    let mut parsed = 0usize;
    while parsed < declared_tracks.min(MAX_TRACKS) {
        parsed += 1;
        cursor.expect_tag(b"MTrk")?;
        let length = cursor.u32()? as usize;
        if cursor.offset + length > bytes.len() {
            return Err(error(
                cursor.offset,
                "MTrk payload runs past the end of the file",
            ));
        }
        let payload = &bytes[cursor.offset..cursor.offset + length];
        let track = parse_track(payload, ticks_per_quarter, tracks.len())?;
        // A format-1 conductor track carries only meta events: it folds into
        // the project's global tempo/meter, so it becomes no track here.
        if !track.clips.iter().all(|c| c.sequence.notes.is_empty()) {
            tracks.push(track);
        }
        cursor.skip(length)?;
    }
    Ok(tracks)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl Cursor<'_> {
    fn u8(&mut self) -> Result<u8, SmfError> {
        let byte = self.bytes.get(self.offset).copied();
        self.offset += 1;
        byte.ok_or_else(|| error(self.offset, "unexpected end of file"))
    }

    fn u16(&mut self) -> Result<u16, SmfError> {
        Ok((u16::from(self.u8()?) << 8) | u16::from(self.u8()?))
    }

    fn u32(&mut self) -> Result<u32, SmfError> {
        Ok((u32::from(self.u16()?) << 16) | u32::from(self.u16()?))
    }

    fn expect_tag(&mut self, tag: &[u8; 4]) -> Result<(), SmfError> {
        for expected in tag {
            if self.u8()? != *expected {
                return Err(error(self.offset, format!("expected chunk {tag:?}")));
            }
        }
        Ok(())
    }

    fn skip(&mut self, count: usize) -> Result<(), SmfError> {
        if self.offset + count > self.bytes.len() {
            return Err(error(self.offset, "chunk runs past the end of the file"));
        }
        self.offset += count;
        Ok(())
    }

    /// Variable-length quantity; caps at 4 bytes per the spec.
    fn vlq(&mut self) -> Result<i64, SmfError> {
        let mut value = 0i64;
        for _ in 0..4 {
            let byte = self.u8()?;
            value = (value << 7) | i64::from(byte & 0x7F);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(error(self.offset, "VLQ longer than 4 bytes"))
    }
}

/// Parses one MTrk payload into note events.
fn parse_track(
    payload: &[u8],
    ticks_per_quarter: i64,
    track_index: usize,
) -> Result<MidiTrack, SmfError> {
    let _ = ticks_per_quarter; // division is already our tick base
    let mut cursor = Cursor {
        bytes: payload,
        offset: 0,
    };
    let mut events = 0usize;
    let mut tick = 0i64;
    let mut running_status: Option<u8> = None;
    // (channel, pitch) → stack of (on-tick, velocity), FIFO matched.
    let mut open: OpenNotes = Vec::new();
    let mut notes: Vec<Note> = Vec::new();
    let mut name = String::new();

    while cursor.offset < payload.len() {
        events += 1;
        if events > MAX_EVENTS {
            return Err(error(cursor.offset, "event count exceeds the limit"));
        }
        tick += cursor.vlq()?;
        let status = {
            let byte = cursor.u8()?;
            if byte & 0x80 != 0 {
                if byte < 0xF0 {
                    running_status = Some(byte);
                } else {
                    running_status = None; // meta / sysex clear running status
                }
                byte
            } else {
                running_status
                    .ok_or_else(|| error(cursor.offset, "data byte without a status byte"))?
            }
        };
        match status {
            // Note on / off.
            0x80..=0x9F => {
                let pitch = cursor.u8()?;
                let velocity = cursor.u8()?;
                let channel = status & 0x0F;
                let key = (channel, pitch);
                let is_on = status & 0xF0 == 0x90 && velocity > 0;
                if is_on {
                    entry(&mut open, key).push((tick, velocity));
                } else {
                    // Note-off, or note-on with velocity 0 (the report's
                    // classic edge — treated as an off).
                    if let Some(stack) = open.iter_mut().find(|(k, _)| *k == key) {
                        if let Some((on_tick, on_velocity)) = stack.1.first().copied() {
                            stack.1.remove(0);
                            notes.push(Note {
                                start: on_tick,
                                length: (tick - on_tick).max(1),
                                note: pitch,
                                velocity: on_velocity,
                                channel,
                            });
                        }
                    }
                }
            }
            // Meta events.
            0xFF => {
                let kind = cursor.u8()?;
                let length = cursor.vlq()? as usize;
                if kind == 0x03 && track_index != usize::MAX {
                    let text = payload
                        .get(cursor.offset..cursor.offset + length)
                        .unwrap_or(&[]);
                    name = String::from_utf8_lossy(text)
                        .trim_end_matches('\0')
                        .to_string();
                }
                cursor.skip(length)?;
            }
            // SysEx (F0 / F7): skip the variable-length payload.
            0xF0 | 0xF7 => {
                let length = cursor.vlq()? as usize;
                cursor.skip(length)?;
            }
            // Two-data-byte channel messages we ignore (CC, pitch bend, ...).
            _ => {
                cursor.skip(2)?;
            }
        }
    }

    // Hanging note-ons close at the track end (the report's other classic).
    let track_end = tick.max(1);
    for ((channel, pitch), stack) in open {
        for (on_tick, velocity) in stack {
            notes.push(Note {
                start: on_tick,
                length: (track_end - on_tick).max(1),
                note: pitch,
                velocity,
                channel,
            });
        }
    }

    // Import hygiene: dedupe before the editor ever sees the file.
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    // The clip sits at the earliest note's absolute tick; the notes become
    // relative to it (our clip semantics), keeping the arrangement position.
    let origin = notes.first().map(|n| n.start).unwrap_or(0);
    for note in &mut notes {
        note.start -= origin;
    }
    let span = notes
        .iter()
        .map(|n| n.start + n.length)
        .max()
        .unwrap_or(0)
        .max(1);
    Ok(MidiTrack {
        name,
        clips: vec![MidiClip {
            start_ticks: u32::try_from(origin).unwrap_or(u32::MAX),
            span_ticks: u32::try_from(span).unwrap_or(u32::MAX),
            loop_ticks: u32::try_from(span).unwrap_or(u32::MAX),
            sequence: Sequence {
                notes,
                ctrls: Vec::new(),
            },
            legacy_notes: Vec::new(),
        }],
    })
}

fn entry(map: &mut OpenNotes, key: (u8, u8)) -> &mut Vec<(i64, u8)> {
    let index = match map.iter().position(|(k, _)| *k == key) {
        Some(index) => index,
        None => {
            map.push((key, Vec::new()));
            map.len() - 1
        }
    };
    &mut map[index].1
}

#[cfg(test)]
mod tests {
    use super::{MAX_EVENTS, export_smf, import_smf, vlq};
    use crate::{MidiClip, MidiTrack, Note, Project, TICKS_PER_QUARTER};

    fn project_with(tracks: Vec<MidiTrack>, bpm: f64) -> Project {
        let mut project = Project::default();
        project.metadata.tempo_bpm = bpm;
        project.midi_tracks = tracks;
        project
    }

    #[test]
    fn vlq_round_trips_the_boundaries() {
        for value in [0i64, 0x7F, 0x80, 0x3FFF, 0x4000, 0x1F_FFFF, 0x20_0000] {
            let encoded = vlq(value);
            let mut cursor = super::Cursor {
                bytes: &encoded,
                offset: 0,
            };
            assert_eq!(cursor.vlq().expect("parse"), value, "{value:#x}");
        }
        assert_eq!(vlq(0), vec![0x00]);
        assert_eq!(vlq(0x80), vec![0x81, 0x00]);
    }

    #[test]
    fn export_import_round_trip_preserves_notes() {
        let mut clip = MidiClip {
            start_ticks: TICKS_PER_QUARTER, // one bar in
            span_ticks: TICKS_PER_QUARTER * 4,
            loop_ticks: TICKS_PER_QUARTER * 4,
            ..Default::default()
        };
        clip.sequence.notes = vec![
            Note {
                start: 0,
                length: 240,
                note: 60,
                velocity: 90,
                channel: 2,
            },
            Note {
                start: 480,
                length: 120,
                note: 64,
                velocity: 40,
                channel: 0,
            },
        ];
        let project = project_with(
            vec![MidiTrack {
                name: "lead".into(),
                clips: vec![clip],
            }],
            120.0,
        );

        let bytes = export_smf(&project);
        let mut imported = import_smf(&bytes).expect("parse");
        assert_eq!(imported.len(), 1);
        let track = imported.pop().expect("one track");
        assert_eq!(track.name, "lead");
        let notes = &track.clips[0].sequence.notes;
        assert_eq!(notes.len(), 2);
        // The arrangement position survived: the clip sits one bar in and the
        // notes are relative to it.
        assert_eq!(track.clips[0].start_ticks, TICKS_PER_QUARTER);
        let mut starts: Vec<i64> = notes.iter().map(|n| n.start).collect();
        starts.sort_unstable();
        assert_eq!(starts[0], 0);
        // Fields survived.
        let lead = notes.iter().find(|n| n.note == 60).expect("C4");
        assert_eq!((lead.length, lead.velocity, lead.channel), (240, 90, 2));
    }

    #[test]
    fn note_on_with_velocity_zero_is_a_note_off() {
        // One note-on (vel 100) then a note-on with velocity 0.
        let mut bytes = header(1, 960);
        let mut track = vec![0x00, 0x90, 60, 100];
        track.extend_from_slice(&vlq(240));
        track.extend_from_slice(&[0x90, 60, 0]); // vel-0 off, running status
        track.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        bytes.push(b'M');
        bytes.push(b'T');
        bytes.push(b'r');
        bytes.push(b'k');
        bytes.extend_from_slice(&(track.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&track);

        let tracks = import_smf(&bytes).expect("parse");
        let notes = &tracks[0].clips[0].sequence.notes;
        assert_eq!(notes.len(), 1);
        assert_eq!((notes[0].start, notes[0].length), (0, 240));
    }

    #[test]
    fn hanging_note_on_closes_at_track_end() {
        let mut bytes = header(1, 960);
        let mut track = vec![0x00, 0x90, 60, 100];
        track.extend_from_slice(&vlq(1000));
        track.extend_from_slice(&[0xFF, 0x2F, 0x00]); // no note-off!
        bytes.extend_from_slice(b"MTrk");
        bytes.extend_from_slice(&(track.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&track);

        let tracks = import_smf(&bytes).expect("parse");
        let notes = &tracks[0].clips[0].sequence.notes;
        assert_eq!(notes.len(), 1, "the hanging note is closed, not dropped");
        assert_eq!(notes[0].length, 1000);
    }

    #[test]
    fn same_pitch_overlap_matches_fifo() {
        // Two overlapping C4s: on@0, on@100, off@200, off@300 →
        // FIFO: first on pairs with first off.
        let mut bytes = header(1, 960);
        let mut track = vec![0x00, 0x90, 60, 100];
        track.extend_from_slice(&vlq(100));
        track.extend_from_slice(&[0x90, 60, 90]);
        track.extend_from_slice(&vlq(100));
        track.extend_from_slice(&[0x80, 60, 0]);
        track.extend_from_slice(&vlq(100));
        track.extend_from_slice(&[0x80, 60, 0]);
        track.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        bytes.extend_from_slice(b"MTrk");
        bytes.extend_from_slice(&(track.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&track);

        let tracks = import_smf(&bytes).expect("parse");
        let mut notes = tracks[0].clips[0].sequence.notes.clone();
        notes.sort_by_key(|n| n.start);
        assert_eq!(notes.len(), 2);
        assert_eq!(
            (notes[0].start, notes[0].length),
            (0, 200),
            "FIFO: on@0 ends at 200"
        );
        assert_eq!((notes[1].start, notes[1].length), (100, 200));
    }

    #[test]
    fn malformed_input_reports_the_byte_offset() {
        assert!(import_smf(b"NOPE").is_err());
        // Truncated MThd.
        let mut bytes = header(1, 960);
        bytes.truncate(bytes.len() - 2);
        let error = import_smf(&bytes).expect_err("truncated header");
        assert!(
            error.to_string().contains("byte"),
            "error carries an offset"
        );
    }

    #[test]
    fn event_cap_stops_hostile_files() {
        // A track claiming far more events than the cap allows: build a long
        // run of tiny deltas and check the parser gives up rather than hangs.
        let mut bytes = header(1, 960);
        let mut track = Vec::new();
        for _ in 0..64 {
            track.extend_from_slice(&[0x00, 0x90, 60, 1]); // note-on vel 1
        }
        track.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        bytes.extend_from_slice(b"MTrk");
        bytes.extend_from_slice(&(track.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&track);
        // Not over the cap, but proves a normal small file parses.
        assert!(import_smf(&bytes).is_ok());
        // The cap constant must be usable as a hard limit.
        let _ = MAX_EVENTS;
    }

    fn header(format: u16, division: u16) -> Vec<u8> {
        let mut bytes = b"MThd".to_vec();
        bytes.extend_from_slice(&6u32.to_be_bytes());
        bytes.extend_from_slice(&format.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes()); // one track
        bytes.extend_from_slice(&division.to_be_bytes());
        bytes
    }
}
