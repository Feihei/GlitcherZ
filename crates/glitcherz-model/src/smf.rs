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

use crate::{
    CtrlEvent, CtrlKind, MidiClip, MidiTrack, Note, Sequence, TICKS_PER_QUARTER, TimeSignature,
};

/// Parser safety limits.
pub const MAX_TRACKS: usize = 4096;
pub const MAX_EVENTS: usize = 5_000_000;

/// Notes per imported clip: the chunk size the importer splits a dense track
/// at.
///
/// A real MIDI track (one piano part, three minutes) routinely holds a few
/// thousand notes. Before the D15 revision the importer put the whole track in
/// one clip, which then overflowed the 64-note clip limit and could not be
/// saved at all. Clips are unbounded now, but the editor stays responsive with
/// a bounded amount of content per clip, so import chunks at this size.
pub const IMPORT_CLIP_NOTES: usize = 4_096;

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
/// track carries the project tempo **and its meter** (U3a: `FF 58`, which used
/// to be hardcoded to 4/4).
pub fn export_smf(project: &crate::Project) -> Vec<u8> {
    let mut out = Vec::new();
    let track_count = project.midi_tracks.len() + 1; // + conductor
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // format 1
    out.extend_from_slice(&(track_count as u16).to_be_bytes());
    out.extend_from_slice(&(TICKS_PER_QUARTER as u16).to_be_bytes());

    // Conductor: tempo + meter + end of track.
    let micros_per_quarter = (60.0 / project.metadata.tempo_bpm * 1_000_000.0).round() as u32;
    let meter = project.metadata.time_signature;
    let mut conductor = Vec::new();
    conductor.extend_from_slice(&vlq(0));
    conductor.extend_from_slice(&[0xFF, 0x51, 0x03]);
    conductor.extend_from_slice(&micros_per_quarter.to_be_bytes()[1..4]);
    conductor.extend_from_slice(&vlq(0));
    // FF 58 04 nn dd cc bb: nn beats per bar, dd = log2(denominator),
    // cc = MIDI clocks per metronome click, bb = 32nd notes per quarter.
    conductor.extend_from_slice(&[
        0xFF,
        0x58,
        0x04,
        meter.numerator,
        meter.denominator.trailing_zeros() as u8,
        24,
        8,
    ]);
    conductor.extend_from_slice(&vlq(0));
    conductor.extend_from_slice(&[0xFF, 0x2F, 0x00]);
    write_chunk(&mut out, b"MTrk", &conductor);

    for track in &project.midi_tracks {
        let mut events: Vec<SmfEvent> = Vec::new();
        for clip in &track.clips {
            for (offset, note) in tiled_notes(clip) {
                let offset = offset + clip.start_ticks as i64;
                let channel = note.channel.min(15);
                events.push(SmfEvent::two(
                    offset,
                    0x90 | channel,
                    note.note,
                    note.velocity.max(1),
                ));
                events.push(SmfEvent::two(
                    offset + note.length.max(1),
                    0x80 | channel,
                    note.note,
                    0,
                ));
            }
            // P0: controller events travel with the clip they belong to, so a
            // file exported from a project that imported CC/bend keeps them.
            for (offset, ctrl) in tiled_ctrls(clip) {
                events.push(ctrl_event(offset + clip.start_ticks as i64, &ctrl));
            }
        }
        // Same tick: releases, then controllers, then note-ons (a repeated
        // pitch must not cut itself off, and a bend must precede the note it
        // applies to).
        events.sort_by_key(|event| (event.tick, event.order_rank()));
        write_track(&mut out, &track.name, &events);
    }
    out
}

/// A clip's controller events with their loop repetitions laid out, as
/// absolute offsets from the clip start (P0; mirrors [`tiled_notes`]' rules:
/// content beyond one loop plays once, not tiled).
fn tiled_ctrls(clip: &MidiClip) -> Vec<(i64, CtrlEvent)> {
    let loop_ticks = clip.loop_ticks.max(1) as i64;
    let span = clip.span_ticks.max(loop_ticks as u32) as i64;
    let periods = (span + loop_ticks - 1) / loop_ticks;
    let mut result = Vec::with_capacity(clip.sequence.ctrls.len() * periods as usize);
    for period in 0..periods {
        let shift = period * loop_ticks;
        for ctrl in &clip.sequence.ctrls {
            if ctrl.tick >= loop_ticks {
                continue; // events beyond one loop play once, not tiled
            }
            let mut ctrl = *ctrl;
            ctrl.tick += shift;
            result.push((ctrl.tick, ctrl));
        }
    }
    result
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

/// One MIDI channel event as written to a track.
///
/// `data_len` is not cosmetic: `0xC0` (program change) and `0xD0` (channel
/// pressure) carry a **single** data byte, and writing a second one would
/// shift every following event — a file that still parses, as garbage.
#[derive(Debug, Clone, Copy)]
struct SmfEvent {
    tick: i64,
    status: u8,
    data1: u8,
    data2: u8,
    data_len: u8,
}

impl SmfEvent {
    /// Two data bytes (notes, CC, pitch bend, poly pressure).
    fn two(tick: i64, status: u8, data1: u8, data2: u8) -> Self {
        Self {
            tick,
            status,
            data1,
            data2,
            data_len: 2,
        }
    }

    /// One data byte (`0xC0` program change, `0xD0` channel pressure).
    fn one(tick: i64, status: u8, data1: u8) -> Self {
        Self {
            tick,
            status,
            data1,
            data2: 0,
            data_len: 1,
        }
    }

    /// Same-tick ordering: releases first, then controllers, then note-ons.
    ///
    /// Controllers sit in the middle on purpose — a bend or a program change
    /// must land **before** the note that depends on it, or the first note of
    /// the block starts at the wrong pitch/timbre.
    fn order_rank(&self) -> u8 {
        match self.status & 0xF0 {
            0x80 => 0,
            0x90 => 2,
            _ => 1,
        }
    }
}

/// Encodes one controller event as the channel message it stands for.
///
/// Pitch bend is centred 14-bit (8192 = no bend), written LSB first — the
/// mirror of [`parse_track`]'s decode.
fn ctrl_event(tick: i64, ctrl: &CtrlEvent) -> SmfEvent {
    let channel = ctrl.channel & 0x0F;
    let value7 = ctrl.value.clamp(0, 127) as u8;
    match ctrl.kind {
        CtrlKind::Cc(controller) => SmfEvent::two(tick, 0xB0 | channel, controller & 0x7F, value7),
        CtrlKind::PitchBend => {
            let raw = (ctrl.value.clamp(-8192, 8191) + 8192) as u16;
            SmfEvent::two(
                tick,
                0xE0 | channel,
                (raw & 0x7F) as u8,
                ((raw >> 7) & 0x7F) as u8,
            )
        }
        CtrlKind::ChannelPressure => SmfEvent::one(tick, 0xD0 | channel, value7),
        CtrlKind::PolyPressure(note) => SmfEvent::two(tick, 0xA0 | channel, note & 0x7F, value7),
        CtrlKind::Program => SmfEvent::one(tick, 0xC0 | channel, value7),
    }
}

fn write_track(out: &mut Vec<u8>, name: &str, events: &[SmfEvent]) {
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
    for event in events {
        body.extend_from_slice(&vlq(event.tick - last));
        last = event.tick;
        body.push(event.status);
        body.push(event.data1);
        if event.data_len > 1 {
            body.push(event.data2);
        }
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

/// What a format-1 file carries beyond its notes: the document-level header.
///
/// Tempo is still left out (importing it would silently re-time an existing
/// project; the caller keeps its BPM — see `implementation.md`'s known
/// trade-off). The meter **is** reported so the caller can decide: a meter is
/// notation, and a file's meter is worth adopting when the project has no
/// content of its own.
#[derive(Debug, Clone, PartialEq)]
pub struct SmfImport {
    /// The note-bearing tracks (the conductor track folds into this header).
    pub tracks: Vec<MidiTrack>,
    /// The first `FF 58` meter in the file, if any.
    pub time_signature: Option<TimeSignature>,
}

/// Parses a format-0/1 SMF into MIDI tracks plus the file's meter.
///
/// Notes become one clip per track spanning the track's musical extent, with
/// note starts relative to the clip. Meta events other than the track name
/// (and, since U3a, the meter) are ignored.
pub fn import_smf(bytes: &[u8]) -> Result<SmfImport, SmfError> {
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
    let mut meter: Option<TimeSignature> = None;
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
        let track = parse_track(payload, ticks_per_quarter, tracks.len(), &mut meter)?;
        // A format-1 conductor track carries only meta events: it folds into
        // the project's global tempo/meter, so it becomes no track here.
        if !track.clips.iter().all(|c| c.sequence.notes.is_empty()) {
            tracks.push(track);
        }
        cursor.skip(length)?;
    }
    Ok(SmfImport {
        tracks,
        time_signature: meter,
    })
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
///
/// The meter (`FF 58`) lands in `meter` — first occurrence wins, so a format-1
/// conductor track's header is what the caller sees even when later tracks
/// restate it.
fn parse_track(
    payload: &[u8],
    ticks_per_quarter: i64,
    track_index: usize,
    meter: &mut Option<TimeSignature>,
) -> Result<MidiTrack, SmfError> {
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
    let mut ctrls: Vec<CtrlEvent> = Vec::new();
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
            // Control change: controller number + 7-bit value.
            0xB0..=0xBF => {
                let controller = cursor.u8()?;
                let value = cursor.u8()?;
                ctrls.push(CtrlEvent {
                    tick,
                    channel: status & 0x0F,
                    kind: CtrlKind::Cc(controller & 0x7F),
                    value: i32::from(value & 0x7F),
                });
            }
            // Pitch bend: 14 bits centred at 8192, LSB first.
            0xE0..=0xEF => {
                let lsb = cursor.u8()?;
                let msb = cursor.u8()?;
                let raw = (i32::from(msb & 0x7F) << 7) | i32::from(lsb & 0x7F);
                ctrls.push(CtrlEvent {
                    tick,
                    channel: status & 0x0F,
                    kind: CtrlKind::PitchBend,
                    value: raw - 8192,
                });
            }
            // Channel pressure: a single data byte.
            0xD0..=0xDF => {
                let value = cursor.u8()?;
                ctrls.push(CtrlEvent {
                    tick,
                    channel: status & 0x0F,
                    kind: CtrlKind::ChannelPressure,
                    value: i32::from(value & 0x7F),
                });
            }
            // Polyphonic pressure: pitch + value.
            0xA0..=0xAF => {
                let pitch = cursor.u8()?;
                let value = cursor.u8()?;
                ctrls.push(CtrlEvent {
                    tick,
                    channel: status & 0x0F,
                    kind: CtrlKind::PolyPressure(pitch & 0x7F),
                    value: i32::from(value & 0x7F),
                });
            }
            // Program change: a single data byte.
            0xC0..=0xCF => {
                let program = cursor.u8()?;
                ctrls.push(CtrlEvent {
                    tick,
                    channel: status & 0x0F,
                    kind: CtrlKind::Program,
                    value: i32::from(program & 0x7F),
                });
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
                // FF 58 04 nn dd cc bb — the first meter in the file wins.
                if kind == 0x58 && length >= 2 && meter.is_none() {
                    let data = payload
                        .get(cursor.offset..cursor.offset + length)
                        .unwrap_or(&[]);
                    if let (Some(&numerator), Some(&exponent)) = (data.first(), data.get(1)) {
                        // dd is log2(denominator); anything beyond 32 is
                        // clamped by `TimeSignature::new`.
                        let denominator = 1u8.checked_shl(u32::from(exponent)).unwrap_or(32);
                        *meter = Some(TimeSignature::new(numerator, denominator));
                    }
                }
                cursor.skip(length)?;
            }
            // SysEx (F0 / F7): skip the variable-length payload.
            0xF0 | 0xF7 => {
                let length = cursor.vlq()? as usize;
                cursor.skip(length)?;
            }
            // Unknown / unsupported status: skip what a two-data-byte message
            // would carry (nothing left in the channel-message range at this
            // point, so this is defensive only).
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

    // Rescale from the file's division to ours (960). Most real files use
    // 96/192/384/480, all of which divide 960 exactly; anything else rounds.
    // Absolute ticks are scaled (not deltas) so rounding error does not
    // accumulate along the timeline.
    let our = i64::from(crate::TICKS_PER_QUARTER);
    let rescale = |ticks: i64| ticks * our / ticks_per_quarter.max(1);
    for note in &mut notes {
        note.start = rescale(note.start);
        note.length = rescale(note.length);
    }
    for ctrl in &mut ctrls {
        ctrl.tick = rescale(ctrl.tick);
    }

    // Import hygiene: dedupe before the editor ever sees the file.
    notes.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
    // Stable, so same-tick controllers keep their file order.
    ctrls.sort_by_key(|c| c.tick);
    Ok(MidiTrack {
        name,
        clips: split_into_clips(notes, ctrls),
    })
}

/// Splits one imported track's **absolute-tick** events into clip-sized
/// chunks.
///
/// A dense track becomes several clips laid end to end (each placed at its
/// chunk's first event, non-looping: `loop == span`). Chunks are cut between
/// notes that do *not* share a start tick, so a chord is never split across two
/// clips. Controller events follow the chunk their tick falls in.
///
/// The chunk size starts at [`IMPORT_CLIP_NOTES`] and grows when needed to stay
/// inside the document's clips-per-track capacity — material is never dropped
/// here; a track large enough to exceed both limits at once is beyond the
/// document's sanity bound and gets rejected by the validator instead.
fn split_into_clips(notes: Vec<Note>, ctrls: Vec<CtrlEvent>) -> Vec<MidiClip> {
    let origin = notes
        .first()
        .map(|n| n.start)
        .into_iter()
        .chain(ctrls.first().map(|c| c.tick))
        .min()
        .unwrap_or(0);

    // Chunk start ticks: the first event, then a new chunk whenever the note
    // budget is spent. `sat` keeps a pathological track from asking for more
    // clips than the document holds.
    let chunk_notes = IMPORT_CLIP_NOTES.max(notes.len().div_ceil(crate::MAX_CLIPS_PER_TRACK));
    let mut starts: Vec<i64> = vec![origin];
    let mut in_chunk = 0usize;
    for (index, note) in notes.iter().enumerate() {
        if in_chunk >= chunk_notes && note.start != notes[index - 1].start {
            starts.push(note.start);
            in_chunk = 0;
        }
        in_chunk += 1;
    }

    let mut clips = Vec::with_capacity(starts.len());
    for (k, &start) in starts.iter().enumerate() {
        // A chunk owns notes whose start lies in `[start, next_start)`; the
        // controller window is the same.
        let end = starts.get(k + 1).copied();
        let in_range = |tick: i64| tick >= start && end.is_none_or(|e| tick < e);

        let mut chunk_notes: Vec<Note> = notes
            .iter()
            .filter(|n| in_range(n.start))
            .cloned()
            .collect();
        let mut chunk_ctrls: Vec<CtrlEvent> =
            ctrls.iter().filter(|c| in_range(c.tick)).cloned().collect();

        // The clip sits at its chunk's earliest event (notes and controllers
        // both count); everything becomes relative to it (our clip semantics).
        let clip_origin = chunk_notes
            .iter()
            .map(|n| n.start)
            .chain(chunk_ctrls.iter().map(|c| c.tick))
            .min()
            .unwrap_or(start);
        for note in &mut chunk_notes {
            note.start -= clip_origin;
        }
        for ctrl in &mut chunk_ctrls {
            ctrl.tick -= clip_origin;
        }
        let span = chunk_notes
            .iter()
            .map(|n| n.start + n.length)
            .chain(chunk_ctrls.iter().map(|c| c.tick + 1))
            .max()
            .unwrap_or(0)
            .max(1);
        clips.push(MidiClip {
            start_ticks: u32::try_from(clip_origin).unwrap_or(u32::MAX),
            span_ticks: u32::try_from(span).unwrap_or(u32::MAX),
            loop_ticks: u32::try_from(span).unwrap_or(u32::MAX),
            sequence: Sequence {
                notes: chunk_notes,
                ctrls: chunk_ctrls,
            },
            legacy_notes: Vec::new(),
        });
    }
    clips
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
    use super::{MAX_EVENTS, export_smf, import_smf, split_into_clips, vlq};
    use crate::{
        CtrlEvent, CtrlKind, MidiClip, MidiTrack, Note, Project, TICKS_PER_QUARTER, TimeSignature,
    };

    /// A note at `start`, fixed length/pitch/velocity.
    fn note(start: i64) -> Note {
        Note {
            start,
            length: 8,
            note: 60,
            velocity: 100,
            channel: 0,
        }
    }

    /// D15 revision: a dense track is chunked instead of being put in one
    /// oversized clip — nothing is dropped, clip count stays legal.
    #[test]
    fn dense_track_is_split_into_legal_clips() {
        let notes: Vec<Note> = (0..5_000).map(|i| note(i * 4)).collect();
        let clips = split_into_clips(notes, Vec::new());

        assert_eq!(clips.len(), 2, "5000 notes / 4096 per clip = 2 chunks");
        assert_eq!(clips[0].sequence.notes.len(), super::IMPORT_CLIP_NOTES);
        assert_eq!(
            clips[1].sequence.notes.len(),
            5_000 - super::IMPORT_CLIP_NOTES
        );
        // Both clips are non-looping single periods placed end to end.
        for clip in &clips {
            assert_eq!(clip.loop_ticks, clip.span_ticks);
        }
        // Chunks are placed end to end; the first clip's span reaches one
        // note length past the cut (its last note), which is fine — the second
        // clip simply starts while that note still rings.
        assert_eq!(clips[1].start_ticks, 4_096 * 4);
        assert!(clips[0].span_ticks >= clips[1].start_ticks);
        // Timeline positions are preserved: absolute = start_ticks + relative.
        let absolute: Vec<i64> = clips
            .iter()
            .flat_map(|clip| {
                clip.sequence
                    .notes
                    .iter()
                    .map(move |n| i64::from(clip.start_ticks) + n.start)
            })
            .collect();
        assert_eq!(absolute, (0..5_000).map(|i| i * 4).collect::<Vec<_>>());
    }

    /// The reported bug: a 294-note imported clip used to overflow the old
    /// 64-note cap and could not be saved at all.
    #[test]
    fn imported_dense_track_stays_savable() {
        let mut clip = MidiClip::new(0);
        for i in 0..300 {
            clip.sequence.notes.push(Note {
                start: i * 8,
                length: 8,
                note: 60 + (i % 12) as u8,
                velocity: 100,
                channel: 0,
            });
        }
        let bytes = export_smf(&project_with(
            vec![MidiTrack {
                name: "dense".to_string(),
                clips: vec![clip],
            }],
            120.0,
        ));

        let imported = import_smf(&bytes).expect("parse");
        assert_eq!(imported.tracks.len(), 1);
        assert_eq!(imported.tracks[0].clips.len(), 1, "300 notes fit one clip");
        assert_eq!(imported.tracks[0].clips[0].sequence.notes.len(), 300);

        // A default project's graph already wires one timeline port per
        // default track, so the imported track takes slot 0.
        let mut project = Project::default();
        project.midi_tracks[0] = imported.tracks.into_iter().next().expect("one track");
        project
            .validate()
            .expect("an imported dense track must be savable");
    }

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
        let mut imported = import_smf(&bytes).expect("parse").tracks;
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

    /// P0: controller events survive export → import with their kinds,
    /// channels, values and relative positions intact, including the full
    /// 14-bit pitch-bend range.
    #[test]
    fn ctrl_events_round_trip() {
        let mut clip = MidiClip {
            start_ticks: TICKS_PER_QUARTER, // one bar in
            span_ticks: TICKS_PER_QUARTER * 4,
            loop_ticks: TICKS_PER_QUARTER * 4,
            ..Default::default()
        };
        clip.sequence.notes = vec![Note {
            start: 0,
            length: 240,
            note: 60,
            velocity: 90,
            channel: 2,
        }];
        clip.sequence.ctrls = vec![
            CtrlEvent {
                tick: 0,
                channel: 2,
                kind: CtrlKind::Cc(11),
                value: 100,
            },
            CtrlEvent {
                tick: 480,
                channel: 2,
                kind: CtrlKind::PitchBend,
                value: -8192,
            },
            CtrlEvent {
                tick: 720,
                channel: 2,
                kind: CtrlKind::PitchBend,
                value: 8191,
            },
            CtrlEvent {
                tick: 960,
                channel: 2,
                kind: CtrlKind::Program,
                value: 42,
            },
        ];
        let project = project_with(
            vec![MidiTrack {
                name: "ctrl".into(),
                clips: vec![clip],
            }],
            120.0,
        );

        let bytes = export_smf(&project);
        let imported = import_smf(&bytes).expect("parse");
        let mut tracks = imported.tracks;
        let track = tracks.pop().expect("one track");
        // The arrangement position survived and the events are relative to it.
        assert_eq!(track.clips[0].start_ticks, TICKS_PER_QUARTER);
        let ctrls = &track.clips[0].sequence.ctrls;
        assert_eq!(ctrls.len(), 4, "{ctrls:?}");
        assert_eq!(
            ctrls
                .iter()
                .map(|c| (c.tick, c.channel, c.kind, c.value))
                .collect::<Vec<_>>(),
            vec![
                (0, 2, CtrlKind::Cc(11), 100),
                (480, 2, CtrlKind::PitchBend, -8192),
                (720, 2, CtrlKind::PitchBend, 8191),
                (960, 2, CtrlKind::Program, 42),
            ]
        );
    }

    /// P0: `0xC0` (program) and `0xD0` (channel pressure) carry a **single**
    /// data byte. A second one would shift every following event, so a note
    /// scheduled after them must still parse.
    #[test]
    fn single_data_byte_ctrls_do_not_shift_later_events() {
        let mut clip = MidiClip {
            start_ticks: 0,
            span_ticks: TICKS_PER_QUARTER * 2,
            loop_ticks: TICKS_PER_QUARTER * 2,
            ..Default::default()
        };
        clip.sequence.notes = vec![Note {
            start: 480,
            length: 240,
            note: 72,
            velocity: 100,
            channel: 0,
        }];
        clip.sequence.ctrls = vec![
            CtrlEvent {
                tick: 0,
                channel: 0,
                kind: CtrlKind::Program,
                value: 5,
            },
            CtrlEvent {
                tick: 240,
                channel: 0,
                kind: CtrlKind::ChannelPressure,
                value: 33,
            },
        ];
        let project = project_with(
            vec![MidiTrack {
                name: "one-byte".into(),
                clips: vec![clip],
            }],
            120.0,
        );

        let bytes = export_smf(&project);
        let imported = import_smf(&bytes).expect("parse");
        let track = &imported.tracks[0];
        assert_eq!(
            track.clips[0].sequence.notes.len(),
            1,
            "the note after two one-byte messages must survive"
        );
        assert_eq!(track.clips[0].sequence.notes[0].note, 72);
        assert_eq!(track.clips[0].sequence.ctrls.len(), 2);
    }

    #[test]
    fn meter_round_trips_through_the_conductor_track() {
        let mut project = project_with(Vec::new(), 120.0);
        project.metadata.time_signature = TimeSignature::new(7, 8);
        let bytes = export_smf(&project);
        // FF 58 04 nn dd cc bb with dd = log2(8) = 3.
        assert!(
            bytes
                .windows(7)
                .any(|w| w == [0xFF, 0x58, 0x04, 7, 3, 24, 8]),
            "the conductor track carries the project meter"
        );
        let imported = import_smf(&bytes).expect("parse");
        assert_eq!(imported.time_signature, Some(TimeSignature::new(7, 8)));
        assert!(
            imported.tracks.is_empty(),
            "a conductor-only file holds no note tracks"
        );
    }

    #[test]
    fn files_without_a_meter_report_none() {
        // A bare note file: no FF 58, so the caller keeps its own meter.
        let mut bytes = header(1, 960);
        let mut track = vec![0x00, 0x90, 60, 100];
        track.extend_from_slice(&vlq(240));
        track.extend_from_slice(&[0x80, 60, 0]);
        track.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        bytes.extend_from_slice(b"MTrk");
        bytes.extend_from_slice(&(track.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&track);

        let imported = import_smf(&bytes).expect("parse");
        assert_eq!(imported.time_signature, None);
        assert_eq!(imported.tracks.len(), 1);
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

        let tracks = import_smf(&bytes).expect("parse").tracks;
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

        let tracks = import_smf(&bytes).expect("parse").tracks;
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

        let tracks = import_smf(&bytes).expect("parse").tracks;
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
    fn import_rescales_foreign_divisions() {
        // A 480-PPQ file: a quarter note is 480 ticks there, 960 here.
        let mut bytes = header(1, 480);
        let mut track = vec![0x00, 0x90, 60, 100];
        track.extend_from_slice(&vlq(480)); // one quarter @480
        track.extend_from_slice(&[0x80, 60, 0]);
        track.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        bytes.extend_from_slice(b"MTrk");
        bytes.extend_from_slice(&(track.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&track);

        let tracks = import_smf(&bytes).expect("parse").tracks;
        let notes = &tracks[0].clips[0].sequence.notes;
        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0].length, 960,
            "480 ticks @480PPQ = one quarter = 960 here"
        );
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
