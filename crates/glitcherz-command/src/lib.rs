//! Transactional edits and undo history over the project document (M1.4).
//!
//! Every mutation the UI performs goes through a [`CommandSpec`] registered
//! in [`registry`]. [`Session::execute`] gives three guarantees:
//!
//! 1. **A command cannot leave the document half-edited.** The document is
//!    held behind an [`Arc`]; a snapshot is taken before the command runs and
//!    restored if the command returns an error or panics.
//! 2. **A panic inside a command is an error, not a crash.** The unwind is
//!    caught at the boundary and the document is rolled back.
//! 3. **Undo is O(1) per step.** History entries are whole-document
//!    snapshots; pushing one clones an [`Arc`], not the [`Project`].
//! 4. **Every mutation leaves a replayable trace.** Journaled commands append
//!    their `(id, params)` to [`Session::journal`], the record macros and
//!    agent scripting replay — see [`JournalEntry`].
//!
//! Continuous gestures (a fader drag, dragging a note's velocity) must not
//! produce one undo step per frame. [`Session::execute_merged`] collapses
//! consecutive calls sharing a gesture key into a single step, and
//! [`Session::end_merge`] closes the gesture.
//!
//! Commands receive plain data ([`serde_json::Value`]) rather than closures so
//! that the registry stays enumerable — see [`CommandInfo`] for the
//! self-describing form the UI and tests walk.

// The command boundary must never panic outside `execute`'s catch_unwind.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use std::sync::Arc;

use glitcherz_model::Project;
use serde_json::Value;

mod midi;
mod registry;

pub use registry::{
    CommandInfo, CommandSpec, Enabled, Run, always, command_specs, f64_or, find_command,
    has_midi_tracks, has_notes, i64_or, indices, midi_specs, str_param,
};

/// Default number of undo steps retained.
pub const DEFAULT_HISTORY_LIMIT: usize = 200;

/// Default number of retained journal entries.
///
/// Merged gestures (fader drags, velocity painting) journal one entry per
/// frame, so the journal is trimmed instead of growing unbounded.
pub const DEFAULT_JOURNAL_LIMIT: usize = 500;

/// One journaled command invocation: `(command id, params)`.
pub type JournalEntry = (String, Value);

/// A command-layer failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    /// No command is registered under this id.
    #[error("unknown command `{0}`")]
    Unknown(String),
    /// The command is registered but not applicable right now.
    #[error("{0} is unavailable: {1}")]
    Unavailable(String, String),
    /// The parameters did not describe a valid edit.
    #[error("{command}: {message}")]
    BadParams { command: String, message: String },
    /// The command panicked; the document was rolled back.
    #[error("{command} panicked: {message}")]
    Panicked { command: String, message: String },
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, CommandError>;

/// What a command returns — typically the new index of whatever it touched.
///
/// Commands that reorder the sequence must return the new index: sorting
/// permutes the vector, so an index-based selection held across the call would
/// silently point at a different note.
pub type CommandOutcome = Value;

/// Builds a [`CommandError::BadParams`].
pub fn bad_params(command: &str, message: impl Into<String>) -> CommandError {
    CommandError::BadParams {
        command: command.to_string(),
        message: message.into(),
    }
}

#[derive(Debug, Clone)]
struct HistoryEntry {
    label: String,
    doc: Arc<Project>,
}

/// The project document plus its undo history and gesture state.
///
/// Cloning a `Session` is cheap and shares the document; editing either copy
/// triggers copy-on-write, so a snapshot costs one atomic increment.
#[derive(Debug, Clone)]
pub struct Session {
    doc: Arc<Project>,
    undo: Vec<HistoryEntry>,
    redo: Vec<HistoryEntry>,
    /// Gesture key for undo coalescing (see [`Session::execute_merged`]).
    merge_key: Option<String>,
    /// Bumped on every document change; the UI caches derived state on it.
    revision: u64,
    /// Whether there are unsaved changes.
    dirty: bool,
    history_limit: usize,
    /// Nesting counter for [`Session::suspend_history`].
    suspend_depth: u32,
    /// Parameterized log of executed commands (macro recording / agent replay).
    journal: Vec<JournalEntry>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new(Project::default())
    }
}

impl Session {
    /// Wraps a document with an empty history.
    pub fn new(project: Project) -> Self {
        Self {
            doc: Arc::new(project),
            undo: Vec::new(),
            redo: Vec::new(),
            merge_key: None,
            revision: 1,
            dirty: false,
            history_limit: DEFAULT_HISTORY_LIMIT,
            suspend_depth: 0,
            journal: Vec::new(),
        }
    }

    /// Read-only document access.
    pub fn project(&self) -> &Project {
        &self.doc
    }

    /// Shared handle to the current document (cheap; used to take a snapshot).
    pub fn project_arc(&self) -> Arc<Project> {
        Arc::clone(&self.doc)
    }

    /// Mutable document access, copying on write.
    ///
    /// Prefer running a registered command: a direct mutation here is not
    /// recorded in the history and will not be undoable.
    pub fn project_mut(&mut self) -> &mut Project {
        self.revision = self.revision.wrapping_add(1);
        self.dirty = true;
        Arc::make_mut(&mut self.doc)
    }

    /// Replaces the whole document (open / new); clears the history.
    pub fn replace(&mut self, project: Project) {
        self.doc = Arc::new(project);
        self.undo.clear();
        self.redo.clear();
        self.merge_key = None;
        self.journal.clear();
        self.revision = self.revision.wrapping_add(1);
        self.dirty = false;
    }

    /// Whether the document has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Marks the document clean (after a successful save).
    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    /// Change counter; bumped on every mutation.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Whether an undo step is available.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether a redo step is available.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Label of the step [`Session::undo`] would revert.
    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|h| h.label.as_str())
    }

    /// Label of the step [`Session::redo`] would reapply.
    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|h| h.label.as_str())
    }

    /// Undo labels, oldest first (for a history panel).
    pub fn undo_history(&self) -> Vec<String> {
        self.undo.iter().map(|h| h.label.clone()).collect()
    }

    /// Maximum number of retained undo steps.
    pub fn history_limit(&self) -> usize {
        self.history_limit
    }

    /// Sets the undo depth, trimming the oldest entries if it shrank.
    pub fn set_history_limit(&mut self, limit: usize) {
        self.history_limit = limit.max(1);
        self.trim_history();
    }

    fn trim_history(&mut self) {
        if let Some(excess) = self.undo.len().checked_sub(self.history_limit) {
            self.undo.drain(..excess);
        }
    }

    /// Records an undo point for an edit performed outside the registry.
    ///
    /// `before` must be the document as it was *before* the edit — pass
    /// [`Session::project_arc`] captured ahead of the mutation. A snapshot
    /// identical to the current document is ignored, so calling this after an
    /// edit that turned out to be a no-op is harmless.
    ///
    /// This also ends any gesture in progress: an out-of-band edit and a
    /// merged gesture cannot share one step.
    pub fn push_undo(&mut self, label: &str, before: Arc<Project>) {
        if self.suspend_depth > 0 {
            return;
        }
        self.end_merge();
        if Arc::ptr_eq(&before, &self.doc) {
            return;
        }
        self.undo.push(HistoryEntry {
            label: label.to_string(),
            doc: before,
        });
        self.redo.clear();
        self.trim_history();
    }

    /// Like [`Session::push_undo`], but consecutive calls sharing `key`
    /// collapse into one step — the out-of-band twin of
    /// [`Session::execute_merged`].
    ///
    /// Use for parameter drags: the first changed frame records the
    /// pre-gesture snapshot, later frames are no-ops. End the gesture with
    /// [`Session::end_merge`] so the next drag of the same control starts a
    /// fresh step.
    pub fn push_undo_merged(&mut self, label: &str, before: Arc<Project>, key: &str) {
        if self.merge_key.as_deref() == Some(key) {
            return; // mid-gesture: the first frame's snapshot still stands
        }
        self.push_undo(label, before);
        // `push_undo` ends merges, so the key must be set afterwards.
        self.merge_key = Some(key.to_string());
    }

    /// Restores the document to a snapshot without touching the history.
    ///
    /// For rolling back a multi-step edit whose engine side failed; the
    /// document and the engine must stay one.
    pub fn restore(&mut self, snapshot: Arc<Project>) {
        if Arc::ptr_eq(&snapshot, &self.doc) {
            return;
        }
        self.doc = snapshot;
        self.revision = self.revision.wrapping_add(1);
    }

    /// Suspends history recording (nesting counter).
    ///
    /// A compound edit ("Add MIDI Track" grows the mixer, which is itself an
    /// undoable edit) must cost one undo step, not one per sub-operation.
    /// Call [`Session::resume_history`] exactly once per suspend; while the
    /// counter is above zero, undo steps are silently dropped.
    pub fn suspend_history(&mut self) {
        self.suspend_depth += 1;
    }

    /// Ends one level of history suspension.
    pub fn resume_history(&mut self) {
        self.suspend_depth = self.suspend_depth.saturating_sub(1);
    }

    /// Whether history recording is currently suspended.
    pub fn history_suspended(&self) -> bool {
        self.suspend_depth > 0
    }

    /// The journal of executed commands, oldest first.
    ///
    /// Every command whose [`CommandSpec::journal`] flag is set appends its
    /// `(id, params)` here on success — including `noundo` mutations (there
    /// is no undo to replay them, so the journal is the only record) and
    /// every frame of a merged gesture. Queries are not recorded.
    pub fn journal(&self) -> &[JournalEntry] {
        &self.journal
    }

    /// Drains the journal, leaving it empty.
    ///
    /// For exporting a macro or replaying the session elsewhere: take the
    /// entries and hand the buffer back to the log.
    pub fn take_journal(&mut self) -> Vec<JournalEntry> {
        std::mem::take(&mut self.journal)
    }

    /// Discards all journal entries.
    pub fn clear_journal(&mut self) {
        self.journal.clear();
    }

    fn record_journal(&mut self, id: &str, params: &Value) {
        self.journal.push((id.to_string(), params.clone()));
        let excess = self.journal.len().saturating_sub(DEFAULT_JOURNAL_LIMIT);
        if excess > 0 {
            self.journal.drain(..excess);
        }
    }

    /// Reverts the most recent step. Returns `false` when there is none.
    pub fn undo(&mut self) -> bool {
        self.end_merge();
        let Some(entry) = self.undo.pop() else {
            return false;
        };
        let current = std::mem::replace(&mut self.doc, entry.doc);
        self.redo.push(HistoryEntry {
            label: entry.label,
            doc: current,
        });
        self.revision = self.revision.wrapping_add(1);
        self.dirty = true;
        true
    }

    /// Reapplies the most recently undone step.
    pub fn redo(&mut self) -> bool {
        self.end_merge();
        let Some(entry) = self.redo.pop() else {
            return false;
        };
        let current = std::mem::replace(&mut self.doc, entry.doc);
        self.undo.push(HistoryEntry {
            label: entry.label,
            doc: current,
        });
        self.revision = self.revision.wrapping_add(1);
        self.dirty = true;
        true
    }

    /// Runs a registered command transactionally.
    ///
    /// On success the edit is kept and an undo step is pushed. On an error or
    /// a panic the document is rolled back to its pre-command state, so a
    /// failed command leaves nothing behind.
    pub fn execute(&mut self, id: &str, params: &Value) -> Result<CommandOutcome> {
        // A plain execute ends any gesture: the next merged call must start a
        // fresh step.
        self.end_merge();

        let spec = find_command(id).ok_or_else(|| CommandError::Unknown(id.to_string()))?;
        if let Err(reason) = (spec.enabled)(self) {
            return Err(CommandError::Unavailable(id.to_string(), reason));
        }

        let before = Arc::clone(&self.doc);
        let dirty_before = self.dirty;
        let params = params.clone();
        // `AssertUnwindSafe`: on unwind the document is restored to `before`,
        // so a half-applied edit cannot escape the boundary.
        let attempt =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (spec.run)(self, &params)));

        match attempt {
            Ok(Ok(value)) => {
                if spec.undoable && !self.history_suspended() && !Arc::ptr_eq(&before, &self.doc) {
                    self.undo.push(HistoryEntry {
                        label: spec.label.to_string(),
                        doc: before,
                    });
                    self.redo.clear();
                    self.trim_history();
                }
                if spec.journal {
                    self.record_journal(id, &params);
                }
                Ok(value)
            }
            Ok(Err(error)) => {
                // A command that reported failure after mutating must not
                // leave its partial edit behind.
                if !Arc::ptr_eq(&before, &self.doc) {
                    self.doc = before;
                    self.revision = self.revision.wrapping_add(1);
                    self.dirty = dirty_before;
                }
                Err(error)
            }
            Err(payload) => {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                self.doc = before;
                self.revision = self.revision.wrapping_add(1);
                self.dirty = dirty_before;
                Err(CommandError::Panicked {
                    command: id.to_string(),
                    message,
                })
            }
        }
    }

    /// Executes as one step of a continuous gesture.
    ///
    /// Consecutive calls sharing `key` collapse into a single undo step: the
    /// snapshot pushed by the first call is kept and the ones after it are
    /// dropped, so the surviving step still restores the pre-gesture document.
    /// Call [`Session::end_merge`] when the gesture ends.
    ///
    /// ```
    /// # use glitcherz_command::Session;
    /// # use glitcherz_model::{MidiClip, Note, Project};
    /// # use serde_json::json;
    /// let mut project = Project::default();
    /// let mut clip = MidiClip::default();
    /// clip.notes_mut().push(Note::default());
    /// project.midi_tracks[0].clips.push(clip);
    /// let mut session = Session::new(project);
    ///
    /// for velocity in [40u8, 70, 100] {
    ///     session
    ///         .execute_merged(
    ///             "midi.note_edit",
    ///             &json!({ "index": 0, "velocity": velocity }),
    ///             "velocity",
    ///         )
    ///         .unwrap();
    /// }
    /// session.end_merge();
    /// assert_eq!(session.undo_history().len(), 1, "one drag, one undo step");
    /// ```
    pub fn execute_merged(
        &mut self,
        id: &str,
        params: &Value,
        key: &str,
    ) -> Result<CommandOutcome> {
        let continuing = self.merge_key.as_deref() == Some(key);
        let before_len = self.undo.len();
        let result = self.execute(id, params);
        if continuing && result.is_ok() && self.undo.len() == before_len + 1 && before_len > 0 {
            // The earlier entry still holds the pre-gesture document.
            self.undo.pop();
        }
        self.merge_key = Some(key.to_string());
        result
    }

    /// Ends the gesture started by [`Session::execute_merged`].
    pub fn end_merge(&mut self) {
        self.merge_key = None;
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;
    use glitcherz_model::{MidiClip, Note, Project};
    use serde_json::json;

    fn session_with_note() -> Session {
        let mut project = Project::default();
        let mut clip = MidiClip::default();
        clip.notes_mut().push(Note::default());
        project.midi_tracks[0].clips.push(clip);
        Session::new(project)
    }

    #[test]
    fn successful_commands_are_journaled_with_params() {
        let mut session = session_with_note();
        session
            .execute("midi.note_add", &json!({ "start_ticks": 480 }))
            .unwrap();
        assert_eq!(session.journal().len(), 1);
        assert_eq!(session.journal()[0].0, "midi.note_add");
        assert_eq!(session.journal()[0].1["start_ticks"], 480);
    }

    #[test]
    fn queries_and_failed_commands_leave_no_entry() {
        let mut session = session_with_note();
        session
            .execute("midi.query", &json!({ "track": 0, "clip": 0 }))
            .unwrap();
        assert!(session.journal().is_empty(), "queries are not journaled");

        // Out-of-range index: the command errors, nothing is recorded.
        let error = session.execute("midi.note_edit", &json!({ "index": 99, "velocity": 100 }));
        assert!(error.is_err());
        assert!(
            session.journal().is_empty(),
            "failed commands are not journaled"
        );
    }

    #[test]
    fn merged_gestures_journal_each_frame() {
        let mut session = session_with_note();
        for velocity in [40u8, 70, 100] {
            session
                .execute_merged(
                    "midi.note_edit",
                    &json!({ "index": 0, "velocity": velocity }),
                    "v",
                )
                .unwrap();
        }
        session.end_merge();
        assert_eq!(
            session.undo_history().len(),
            1,
            "one gesture, one undo step"
        );
        assert_eq!(
            session.journal().len(),
            3,
            "each frame is a real invocation"
        );
        assert_eq!(
            session.journal()[2].1["velocity"],
            100,
            "last frame wins on replay"
        );
    }

    #[test]
    fn take_journal_drains_and_replace_clears() {
        let mut session = session_with_note();
        session
            .execute("midi.note_add", &json!({ "start_ticks": 480 }))
            .unwrap();
        let taken = session.take_journal();
        assert_eq!(taken.len(), 1);
        assert!(session.journal().is_empty(), "take drains the buffer");

        session
            .execute("midi.note_add", &json!({ "start_ticks": 960 }))
            .unwrap();
        session.replace(Project::default());
        assert!(
            session.journal().is_empty(),
            "a new document starts a fresh journal"
        );
    }
}
