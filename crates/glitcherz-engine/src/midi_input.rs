//! Hardware MIDI input (R3, spec `.agents/spec/recording.md` §3.2).
//!
//! *Non-real-time*: [`MidiInputHub`] owns the `midir` connections, and their
//! callbacks run on the driver's own thread — never on the audio callback.
//! Every callback parses one raw message, resolves the routing snapshot shared
//! with the UI and pushes [`EngineCommand`]s straight into the command queue
//! through [`CommandSender`]. That is the whole point of the hub: a note played
//! on a keyboard controller reaches the engine at the next block boundary
//! instead of waiting for the UI's next frame (16-50 ms of avoidable latency).
//!
//! The document owns the binding ([`glitcherz_model::MidiSource::Device`] on a
//! keyboard node); this module only executes it. Nothing here knows what a
//! track, a take or a cable is — the UI resolves the node's cabled instruments
//! and hands over a target list.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use midir::{MidiInput, MidiInputConnection};

use crate::engine::CommandSender;
use crate::realtime::commands::{EngineCommand, NoteEvent};
use crate::realtime::sequencer::{RtCtrl, RtCtrlKind};

/// Where one hardware input port delivers its events.
///
/// A live performance source plays the instruments its node is cabled to; while
/// recording with nothing cabled, the UI adds the armed track instead, so the
/// input is never silently dead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiTarget {
    /// One instrument node, addressed by engine node ID.
    Node(u64),
    /// A track's sound source (recording fallback, mirrors the computer
    /// keyboard's behaviour). Notes only — controllers always follow cables.
    Track(usize),
}

/// Upper bound on targets copied out of the routing lock per message. The
/// callback copies into a stack array so it never allocates; a snapshot longer
/// than this is truncated (the document's fan-out cap is far smaller).
const MAX_CALLBACK_TARGETS: usize = 8;

/// One open port: the connection (kept alive by `_conn`) plus the target list
/// its callback reads.
struct OpenPort {
    port: String,
    targets: Arc<Mutex<Vec<MidiTarget>>>,
    _conn: MidiInputConnection<()>,
}

/// Cumulative counters for the status bar. Written by the callbacks, read by
/// the UI — a pair of relaxed additions per message is cheaper than any lock
/// and cannot be starved.
#[derive(Debug, Default)]
pub struct MidiInputStats {
    /// Messages received (including ones that map to no engine event).
    received: AtomicU64,
    /// Messages or commands dropped because a queue was full.
    dropped: AtomicU64,
}

impl MidiInputStats {
    /// Total messages seen since the hub was created.
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// Total events dropped on a full queue.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// MIDI input failure. Every variant is a *user-visible* condition: the UI
/// shows it instead of pretending the device works.
#[derive(Debug, thiserror::Error)]
pub enum MidiInputError {
    #[error("MIDI input unavailable: {0}")]
    Backend(#[from] midir::InitError),
    #[error("MIDI port connection failed: {0}")]
    Connect(#[from] midir::ConnectError<MidiInput>),
    #[error("MIDI port '{0}' is no longer present")]
    PortMissing(String),
}

/// The set of open hardware MIDI ports and their routing snapshots.
///
/// Ports are opened lazily: [`Self::set_bindings`] is the single entry point and
/// diffs the desired binding list against what is open, so calling it every
/// frame is cheap and idempotent.
#[derive(Default)]
pub struct MidiInputHub {
    open: Vec<OpenPort>,
    /// Port names seen at the last enumeration (cached for the binding menu).
    ports: Vec<String>,
    stats: Arc<MidiInputStats>,
}

impl MidiInputHub {
    /// An empty hub: no ports open, no enumeration done yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enumerates the input ports the backend currently reports. Allocates
    /// (driver queries), so this is a UI-thread / menu action, not a per-frame
    /// call.
    pub fn list_ports() -> Result<Vec<String>, MidiInputError> {
        let input = MidiInput::new("GlitcherZ")?;
        let mut names = Vec::new();
        for port in input.ports() {
            if let Ok(name) = input.port_name(&port) {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    /// Re-runs [`Self::list_ports`] and caches the result.
    pub fn refresh_ports(&mut self) -> Result<(), MidiInputError> {
        self.ports = Self::list_ports()?;
        Ok(())
    }

    /// Port names from the last [`Self::refresh_ports`].
    pub fn ports(&self) -> &[String] {
        &self.ports
    }

    /// Ports currently open, in binding order.
    pub fn open_ports(&self) -> impl Iterator<Item = &str> {
        self.open.iter().map(|p| p.port.as_str())
    }

    /// Whether `port` has a live connection.
    pub fn is_open(&self, port: &str) -> bool {
        self.open.iter().any(|p| p.port == port)
    }

    /// Counters shared with the callbacks.
    pub fn stats(&self) -> Arc<MidiInputStats> {
        self.stats.clone()
    }

    /// Applies the desired `(port, targets)` binding list: opens ports that
    /// appeared, drops ports that left the list (or vanished from the backend),
    /// and republishes the target list of the ports that stay.
    ///
    /// Ports that are bound but absent from the backend are **skipped and
    /// reported** in the returned list rather than failing the whole batch: one
    /// unplugged controller must not take the other ports down with it. Only a
    /// backend failure (no MIDI support at all, port present but refusing to
    /// open) is an `Err`. The caller decides how loudly to report a missing
    /// port — a device unplugged mid-session is a normal condition, not an
    /// error every frame.
    pub fn set_bindings(
        &mut self,
        bindings: &[(String, Vec<MidiTarget>)],
        sender: &CommandSender,
    ) -> Result<Vec<String>, MidiInputError> {
        // 1. Close ports that are no longer wanted.
        self.open
            .retain(|open| bindings.iter().any(|(port, _)| port == &open.port));

        let mut missing = Vec::new();
        for (port, targets) in bindings {
            // 2. Existing connection: just republish the targets.
            if let Some(open) = self.open.iter_mut().find(|o| &o.port == port) {
                *open.targets.lock().unwrap_or_else(PoisonError::into_inner) = targets.clone();
                continue;
            }
            // 3. New binding: open the port.
            if !self.ports.iter().any(|p| p == port) {
                missing.push(port.clone());
                continue;
            }
            self.open
                .push(self.open_port(port, targets.clone(), sender)?);
        }
        Ok(missing)
    }

    /// Opens one port and spawns its callback.
    fn open_port(
        &self,
        port: &str,
        targets: Vec<MidiTarget>,
        sender: &CommandSender,
    ) -> Result<OpenPort, MidiInputError> {
        let input = MidiInput::new("GlitcherZ")?;
        let handle = input
            .ports()
            .into_iter()
            .find(|p| input.port_name(p).is_ok_and(|n| n == port))
            .ok_or_else(|| MidiInputError::PortMissing(port.to_string()))?;

        let targets = Arc::new(Mutex::new(targets));
        let cb_targets = targets.clone();
        let cb_sender = sender.clone();
        let stats = self.stats.clone();
        let conn = input.connect(
            &handle,
            "glitcherz-in",
            move |_stamp_us, message, _| {
                stats.received.fetch_add(1, Ordering::Relaxed);
                let Some(parsed) = parse_message(message) else {
                    return;
                };
                // Copy the routing snapshot into a stack array: holding the
                // lock across the sends would serialize the callback against
                // the UI thread for no reason.
                let mut buf = [MidiTarget::Track(0); MAX_CALLBACK_TARGETS];
                let count = {
                    let guard = cb_targets.lock().unwrap_or_else(PoisonError::into_inner);
                    let count = guard.len().min(MAX_CALLBACK_TARGETS);
                    buf[..count].copy_from_slice(&guard[..count]);
                    count
                };
                for target in &buf[..count] {
                    if !deliver(&cb_sender, *target, parsed) {
                        stats.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
            (),
        )?;

        Ok(OpenPort {
            port: port.to_string(),
            targets,
            _conn: conn,
        })
    }
}

/// Sends one parsed message to one target. Returns `false` when the command
/// queue was full (the caller counts the drop).
fn deliver(sender: &CommandSender, target: MidiTarget, parsed: ParsedEvent) -> bool {
    let (node_id, track) = match target {
        MidiTarget::Node(node_id) => (node_id, None),
        MidiTarget::Track(track) => (0, Some(track)),
    };
    match parsed {
        ParsedEvent::Note(event) => match track {
            Some(track) => sender.send(EngineCommand::Note { track, event }),
            None => sender.send(EngineCommand::NoteAtNode { node_id, event }),
        },
        // Controllers follow cables only: a recording fallback track has no
        // documented controller destination (and capturing CC would need the
        // automation path, not the note one).
        ParsedEvent::Ctrl(ctrl) => match track {
            Some(_) => false,
            None => sender.send(EngineCommand::CtrlAtNode { node_id, ctrl }),
        },
    }
}

/// What one raw MIDI message means to the engine.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ParsedEvent {
    Note(NoteEvent),
    Ctrl(RtCtrl),
}

/// Parses a raw MIDI message (status byte included).
///
/// Handles what an instrument can act on today: note on/off, control change and
/// pitch bend. Everything else — system common/real-time (`>= 0xF0`, clock and
/// sysex floods), aftertouch and program change (no consumer: SF2 programs go
/// through the document's parameter path) — returns `None`, so a controller
/// spamming active-sensing bytes cannot flood the command queue.
///
/// Running status is not reassembled: `midir` delivers one complete message per
/// callback on every backend it supports.
fn parse_message(message: &[u8]) -> Option<ParsedEvent> {
    let status = *message.first()?;
    if status >= 0xF0 {
        return None;
    }
    let channel = status & 0x0F;
    match status & 0xF0 {
        // Note off, and the note-on-with-velocity-0 idiom.
        0x80 => Some(ParsedEvent::Note(NoteEvent::Off {
            note: *message.get(1)?,
        })),
        0x90 => {
            let note = *message.get(1)?;
            let velocity = *message.get(2)?;
            if velocity == 0 {
                Some(ParsedEvent::Note(NoteEvent::Off { note }))
            } else {
                Some(ParsedEvent::Note(NoteEvent::On {
                    note,
                    velocity: f32::from(velocity) / 127.0,
                }))
            }
        }
        0xB0 => Some(ParsedEvent::Ctrl(RtCtrl::new(
            0,
            channel,
            RtCtrlKind::Cc(*message.get(1)?),
            i32::from(*message.get(2)?),
        ))),
        // Pitch bend: LSB first, 14-bit unsigned, centred on 8192.
        0xE0 => {
            let lsb = i32::from(*message.get(1)?);
            let msb = i32::from(*message.get(2)?);
            Some(ParsedEvent::Ctrl(RtCtrl::new(
                0,
                channel,
                RtCtrlKind::PitchBend,
                ((msb << 7) | lsb) - 8192,
            )))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{MidiTarget, ParsedEvent, deliver, parse_message};
    use crate::engine::CommandSender;
    use crate::realtime::commands::{EngineCommand, NoteEvent, command_channel};
    use crate::realtime::sequencer::{RtCtrl, RtCtrlKind};

    fn sender() -> (CommandSender, rtrb::Consumer<EngineCommand>) {
        let (tx, rx) = command_channel(16);
        (CommandSender::from_producer(tx), rx)
    }

    #[test]
    fn note_on_and_off_are_parsed() {
        assert_eq!(
            parse_message(&[0x90, 60, 100]),
            Some(ParsedEvent::Note(NoteEvent::On {
                note: 60,
                velocity: 100.0 / 127.0
            }))
        );
        assert_eq!(
            parse_message(&[0x80, 60, 0]),
            Some(ParsedEvent::Note(NoteEvent::Off { note: 60 }))
        );
    }

    #[test]
    fn note_on_with_velocity_zero_is_a_note_off() {
        // The idiom half of the controller world uses instead of 0x80.
        assert_eq!(
            parse_message(&[0x90, 64, 0]),
            Some(ParsedEvent::Note(NoteEvent::Off { note: 64 }))
        );
    }

    #[test]
    fn control_change_carries_its_number_and_channel() {
        assert_eq!(
            parse_message(&[0xB3, 1, 127]),
            Some(ParsedEvent::Ctrl(RtCtrl::new(0, 3, RtCtrlKind::Cc(1), 127)))
        );
    }

    #[test]
    fn pitch_bend_is_centred_and_fourteen_bit() {
        let centre = parse_message(&[0xE0, 0x00, 0x40]);
        assert_eq!(
            centre,
            Some(ParsedEvent::Ctrl(RtCtrl::new(
                0,
                0,
                RtCtrlKind::PitchBend,
                0
            )))
        );
        let up = parse_message(&[0xE0, 0x7F, 0x7F]);
        assert_eq!(
            up,
            Some(ParsedEvent::Ctrl(RtCtrl::new(
                0,
                0,
                RtCtrlKind::PitchBend,
                8191
            )))
        );
        let down = parse_message(&[0xE0, 0x00, 0x00]);
        assert_eq!(
            down,
            Some(ParsedEvent::Ctrl(RtCtrl::new(
                0,
                0,
                RtCtrlKind::PitchBend,
                -8192
            )))
        );
    }

    #[test]
    fn system_and_unsupported_messages_are_dropped() {
        // Clock, active sensing, sysex start: a controller streams these and
        // none of them may reach the command queue.
        assert_eq!(parse_message(&[0xF8]), None);
        assert_eq!(parse_message(&[0xFE]), None);
        assert_eq!(parse_message(&[0xF0, 0x7E]), None);
        // Aftertouch / program change: no consumer today.
        assert_eq!(parse_message(&[0xD0, 40]), None);
        assert_eq!(parse_message(&[0xC0, 5]), None);
        // Truncated message.
        assert_eq!(parse_message(&[]), None);
        assert_eq!(parse_message(&[0x90, 60]), None);
    }

    #[test]
    fn a_node_target_receives_node_addressed_commands() {
        let (tx, mut rx) = sender();
        assert!(deliver(
            &tx,
            MidiTarget::Node(7),
            ParsedEvent::Note(NoteEvent::On {
                note: 60,
                velocity: 1.0
            })
        ));
        assert!(matches!(
            rx.pop(),
            Ok(EngineCommand::NoteAtNode { node_id: 7, .. })
        ));
    }

    #[test]
    fn a_track_target_takes_notes_but_not_controllers() {
        // The recording fallback routes notes to a track; controllers have no
        // track-addressed path, so they are refused rather than misdelivered.
        let (tx, mut rx) = sender();
        assert!(deliver(
            &tx,
            MidiTarget::Track(2),
            ParsedEvent::Note(NoteEvent::Off { note: 60 })
        ));
        assert!(matches!(rx.pop(), Ok(EngineCommand::Note { track: 2, .. })));
        assert!(!deliver(
            &tx,
            MidiTarget::Track(2),
            ParsedEvent::Ctrl(RtCtrl::new(0, 0, RtCtrlKind::Cc(7), 100))
        ));
        assert!(rx.pop().is_err(), "nothing may be queued for the track");
    }

    #[test]
    fn a_full_queue_reports_the_drop() {
        let (tx, mut rx) = sender();
        for _ in 0..16 {
            assert!(deliver(
                &tx,
                MidiTarget::Node(1),
                ParsedEvent::Note(NoteEvent::Off { note: 1 })
            ));
        }
        assert!(
            !deliver(
                &tx,
                MidiTarget::Node(1),
                ParsedEvent::Note(NoteEvent::Off { note: 1 })
            ),
            "the seventeenth command cannot fit and must be reported"
        );
        while rx.pop().is_ok() {}
    }
}
