//! Application-wide clipboard (spec `clipboard.md`, F-040).
//!
//! The clipboard holds **value snapshots**, never selection indices: undo or a
//! deletion invalidates `ClipSel` / `RollState::selection` entries, so a paste
//! has to be able to work from a detached copy. The enum tags the content kind,
//! and a paste only runs in a view that accepts that kind (spec D1 / D5).

use egui::Pos2;
use glitcherz_model::{AudioClip, CableDoc, ControlClip, MidiClip, NodeDoc, Note};

use crate::timeline::TimelineLane;

/// One clipboard slot for the whole app (spec D1).
#[derive(Clone)]
pub(crate) enum Clipboard {
    Clips(Vec<ClipCopy>),
    Notes(NotesCopy),
    Nodes(NodesCopy),
}

impl Clipboard {
    /// Human-readable content kind for the mismatched-paste status message.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Clipboard::Clips(_) => "clips",
            Clipboard::Notes(_) => "notes",
            Clipboard::Nodes(_) => "nodes",
        }
    }
}

/// A cloned clip plus where it sat in the arrangement when copied.
#[derive(Clone)]
pub(crate) struct ClipCopy {
    pub lane: TimelineLane,
    pub track: usize,
    /// Absolute start tick at copy time; the paste keeps relative offsets.
    pub start: u32,
    pub clip: ClipSnapshot,
}

/// Lane-tagged clip body (the three timeline kinds).
#[derive(Clone)]
pub(crate) enum ClipSnapshot {
    Midi(MidiClip),
    Control(ControlClip),
    Audio(AudioClip),
}

impl ClipSnapshot {
    /// The clip's arrangement start (all three kinds carry `start_ticks`).
    pub(crate) fn start_ticks(&self) -> u32 {
        match self {
            ClipSnapshot::Midi(clip) => clip.start_ticks,
            ClipSnapshot::Control(clip) => clip.start_ticks,
            ClipSnapshot::Audio(clip) => clip.start_ticks,
        }
    }
}

/// Selected piano-roll notes, with the earliest start kept for alignment.
#[derive(Clone)]
pub(crate) struct NotesCopy {
    pub notes: Vec<Note>,
    /// Earliest clip-local start among `notes`.
    pub min_start: i64,
}

/// Selected patch nodes with their graph-space positions, the cables wholly
/// inside the selection, and the cluster's top-left corner (paste aligns it).
#[derive(Clone)]
pub(crate) struct NodesCopy {
    pub nodes: Vec<(NodeDoc, Pos2)>,
    pub cables: Vec<CableDoc>,
    pub bbox_min: Pos2,
}

impl NodesCopy {
    /// Shifts every collected position so the cluster's top-left corner lands
    /// on `at` (spec §4.3). The relative layout inside the cluster is kept.
    pub(crate) fn placed(&self, at: Pos2) -> Vec<Pos2> {
        let delta = at - self.bbox_min;
        self.nodes.iter().map(|(_, pos)| *pos + delta).collect()
    }

    /// Collects the nodes and cables of a selection. `cables` are filtered to
    /// those whose **both** endpoints are document nodes in the set: a cable
    /// crossing the selection boundary would otherwise drag in an unselected
    /// node (spec §4.3).
    pub(crate) fn collect(
        nodes: Vec<(NodeDoc, Pos2)>,
        cables: impl Iterator<Item = CableDoc>,
    ) -> Option<Self> {
        if nodes.is_empty() {
            return None;
        }
        let ids: Vec<u64> = nodes.iter().map(|(node, _)| node.node_id()).collect();
        let inside = |port: &glitcherz_model::PortRef| matches!(port, glitcherz_model::PortRef::Node(id) if ids.contains(id));
        let cables = cables
            .filter(|cable| inside(&cable.from) && inside(&cable.to))
            .collect();
        let bbox_min = nodes
            .iter()
            .map(|(_, pos)| *pos)
            .fold(Pos2::new(f32::INFINITY, f32::INFINITY), |acc, p| {
                Pos2::new(acc.x.min(p.x), acc.y.min(p.y))
            });
        Some(NodesCopy {
            nodes,
            cables,
            bbox_min,
        })
    }
}

/// Pasted starts: the earliest copy lands exactly on `anchor`, every other copy
/// keeps its distance from it (spec D5 / §4.1).
pub(crate) fn pasted_starts(copies: &[ClipCopy], anchor: u32) -> Vec<u32> {
    let Some(min) = copies.iter().map(|c| c.start).min() else {
        return Vec::new();
    };
    copies
        .iter()
        .map(|c| anchor.saturating_add(c.start - min))
        .collect()
}

/// Pasted notes: the earliest note lands on `anchor` (clip-local), the rest
/// keep their relative offsets (spec §4.2). Starts never go below zero.
pub(crate) fn pasted_notes(copy: &NotesCopy, anchor: i64) -> Vec<Note> {
    copy.notes
        .iter()
        .map(|note| {
            let mut pasted = *note;
            pasted.start = (anchor + (note.start - copy.min_start)).max(0);
            pasted
        })
        .collect()
}

/// First-match index assignment: `targets` are value-equal copies appended to
/// `notes`, so each target claims one still-unclaimed slot. Mirrors the
/// command layer's `locate` (the paste must reselect what it just wrote).
pub(crate) fn locate_notes(notes: &[Note], targets: &[Note]) -> Vec<usize> {
    let mut claimed = vec![false; notes.len()];
    let mut found = Vec::with_capacity(targets.len());
    for want in targets {
        if let Some((at, _)) = notes
            .iter()
            .enumerate()
            .find(|(at, note)| !claimed[*at] && *note == want)
        {
            claimed[at] = true;
            found.push(at);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use glitcherz_model::PortRef;

    fn note(start: i64, pitch: u8) -> Note {
        Note {
            start,
            length: 240,
            note: pitch,
            velocity: 100,
            channel: 0,
        }
    }

    fn notes_copy(notes: Vec<Note>) -> NotesCopy {
        let min_start = notes.iter().map(|n| n.start).min().unwrap_or(0);
        NotesCopy { notes, min_start }
    }

    #[test]
    fn notes_keep_relative_offsets_and_land_on_the_anchor() {
        let copy = notes_copy(vec![note(960, 60), note(1200, 64), note(480, 67)]);
        let pasted = pasted_notes(&copy, 4800);
        // The earliest source (480) lands on the anchor; the input order is
        // kept (the caller sorts after writing).
        assert_eq!(
            pasted.iter().map(|n| n.start).collect::<Vec<_>>(),
            [5280, 5520, 4800]
        );
        assert_eq!(
            pasted.iter().map(|n| n.note).collect::<Vec<_>>(),
            [60, 64, 67]
        );
    }

    #[test]
    fn a_negative_anchor_clamps_to_zero() {
        let copy = notes_copy(vec![note(0, 60), note(240, 62)]);
        let pasted = pasted_notes(&copy, -120);
        assert_eq!(pasted[0].start, 0);
        assert_eq!(pasted[1].start, 120);
    }

    #[test]
    fn locations_use_first_match_assignment_like_the_command_layer() {
        let original = note(0, 60);
        let copy = vec![original, note(240, 62)];
        let mut all = copy.clone();
        all.extend(pasted_notes(&notes_copy(copy.clone()), 0));
        all.sort_by_key(|n| (n.start, n.note, n.channel, n.length, n.velocity));
        let found = locate_notes(&all, &pasted_notes(&notes_copy(copy), 0));
        assert_eq!(
            found,
            [0, 2],
            "identical value pairs claim the first free slot"
        );
    }

    fn node(id: u64, gain: f32) -> NodeDoc {
        NodeDoc::Feedback {
            node_id: id,
            track: 0,
            gain,
        }
    }

    #[test]
    fn node_cluster_aligns_its_bbox_corner_and_keeps_relative_positions() {
        let nodes = vec![
            (node(2, 0.5), Pos2::new(100.0, 200.0)),
            (node(3, 0.5), Pos2::new(140.0, 160.0)),
            (node(4, 0.5), Pos2::new(120.0, 300.0)),
        ];
        let copy = NodesCopy::collect(nodes, std::iter::empty()).unwrap();
        assert_eq!(copy.bbox_min, Pos2::new(100.0, 160.0));
        let placed = copy.placed(Pos2::new(10.0, 20.0));
        assert_eq!(
            placed,
            vec![
                Pos2::new(10.0, 60.0),
                Pos2::new(50.0, 20.0),
                Pos2::new(30.0, 160.0)
            ]
        );
    }

    #[test]
    fn only_cables_whose_both_ends_are_selected_are_collected() {
        let nodes = vec![
            (node(2, 0.5), Pos2::ZERO),
            (node(3, 0.5), Pos2::new(10.0, 0.0)),
        ];
        let cables = vec![
            CableDoc::audio_to_channel(PortRef::Node(2), PortRef::Node(3), 0),
            CableDoc::audio_to_channel(PortRef::Node(2), PortRef::Node(99), 0),
            CableDoc::audio_to_channel(PortRef::MidiTrack(0), PortRef::Node(3), 0),
        ];
        let copy = NodesCopy::collect(nodes, cables.into_iter()).unwrap();
        assert_eq!(
            copy.cables.len(),
            1,
            "only the fully selected cable survives"
        );
    }
}
