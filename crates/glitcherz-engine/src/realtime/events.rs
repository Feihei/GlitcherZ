//! Block-rate MIDI event and control bus (spec `.agents/spec/midi-nodes.md`).
//!
//! Nodes exchange note/controller events and one scalar control value per
//! block through this fixed-capacity bus. The audio callback only moves
//! `Copy` values; nothing here allocates or locks.

use crate::realtime::commands::NoteEvent;
use crate::realtime::sequencer::RtCtrl;

/// Maximum events a node can emit in one block. Matches the plugin hosts'
/// pending-event queue so a burst never overruns a CLAP/VST3 target.
pub const MAX_NODE_EVENTS: usize = 32;
/// Maximum outgoing event/control cables of one node.
pub const MAX_EVENT_FANOUT: usize = 8;
/// Global event/control cable capacity.
pub const MAX_EVENT_CABLES: usize = 128;

/// One event on the node bus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RtEvent {
    /// Note on / off.
    Note(NoteEvent),
    /// Controller event (CC / pitch bend / ...).
    Ctrl(RtCtrl),
}

/// One block of outgoing events plus the node's scalar control output.
#[derive(Debug, Clone, Copy)]
pub struct NodeEvents {
    events: [Option<RtEvent>; MAX_NODE_EVENTS],
    len: u8,
    /// Control value emitted this block (`None` = nothing emitted).
    pub control: Option<f32>,
}

impl Default for NodeEvents {
    fn default() -> Self {
        Self {
            events: [None; MAX_NODE_EVENTS],
            len: 0,
            control: None,
        }
    }
}

impl NodeEvents {
    /// Empties the bus (one call at the start of every block).
    pub fn clear(&mut self) {
        self.len = 0;
        self.control = None;
    }

    /// Appends a note event; silently dropped when the bus is full.
    pub fn push_note(&mut self, note: NoteEvent) {
        self.push(RtEvent::Note(note));
    }

    /// Appends a controller event; silently dropped when the bus is full.
    pub fn push_ctrl(&mut self, ctrl: RtCtrl) {
        self.push(RtEvent::Ctrl(ctrl));
    }

    /// Appends an event; silently dropped when the bus is full (the oldest
    /// entries win — an overflowing source keeps its first events).
    pub fn push(&mut self, event: RtEvent) {
        let idx = usize::from(self.len);
        if idx >= MAX_NODE_EVENTS {
            return;
        }
        self.events[idx] = Some(event);
        self.len += 1;
    }

    /// Number of queued events.
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    /// Whether no event is queued.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Iterates the queued events in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = RtEvent> + '_ {
        self.events[..self.len()].iter().flatten().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_NODE_EVENTS, NodeEvents, RtEvent};
    use crate::realtime::commands::NoteEvent;
    use crate::realtime::sequencer::{RtCtrl, RtCtrlKind};

    fn note_on(note: u8) -> NoteEvent {
        NoteEvent::On {
            note,
            velocity: 1.0,
        }
    }

    #[test]
    fn push_keeps_order_and_clear_empties_the_bus() {
        let mut bus = NodeEvents::default();
        bus.push_note(note_on(60));
        bus.push_ctrl(RtCtrl {
            tick_ticks: 0,
            channel: 0,
            kind: RtCtrlKind::Cc(74),
            value: 99,
        });
        bus.control = Some(0.5);
        assert_eq!(bus.len(), 2);
        assert_eq!(
            bus.iter().collect::<Vec<_>>(),
            vec![
                RtEvent::Note(note_on(60)),
                RtEvent::Ctrl(RtCtrl {
                    tick_ticks: 0,
                    channel: 0,
                    kind: RtCtrlKind::Cc(74),
                    value: 99,
                }),
            ]
        );
        bus.clear();
        assert!(bus.is_empty());
        assert_eq!(bus.control, None);
    }

    #[test]
    fn overflow_drops_new_events_instead_of_panicking() {
        let mut bus = NodeEvents::default();
        for note in 0..(MAX_NODE_EVENTS + 10) {
            bus.push_note(note_on((note % 128) as u8));
        }
        assert_eq!(bus.len(), MAX_NODE_EVENTS);
    }
}
