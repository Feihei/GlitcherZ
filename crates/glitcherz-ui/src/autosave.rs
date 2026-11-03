//! Auto-save and crash recovery (S3a).
//!
//! A dirty document is snapshotted next to its project file every so often;
//! the snapshot of `song.glz` lives at `song.glz.autosave`, and a document that
//! has never been saved uses `<app data>/autosave/Untitled.glz`.
//!
//! The snapshot is **deleted** whenever the document reaches the disk (manual
//! save) or the user walks away from it (discard / clean exit). That makes file
//! existence the whole recovery signal: a snapshot sitting there at startup
//! means the previous run died with unsaved work. No timestamp comparison, no
//! guessing which copy is newer — a rule that is easy to reason about and easy
//! to test.
//!
//! Timing lives in [`Clock`] (pure arithmetic): snapshot once the user has been
//! quiet for [`QUIET`], and at least every [`SPACING`] while they keep editing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use glitcherz_model::{ModelError, Project, autosave_path, save_autosave};

/// Quiet time after the last edit before a snapshot is written.
pub const QUIET: Duration = Duration::from_secs(30);

/// Longest a continuously edited document goes unsnapshotted.
pub const SPACING: Duration = Duration::from_secs(60);

/// File name of the never-saved document's snapshot.
const UNTITLED_FILE: &str = "Untitled.glz";

/// Sub-directory of the app's storage directory holding untitled snapshots.
const AUTOSAVE_DIR: &str = "autosave";

/// Snapshot of a document that has never been saved, inside the app data
/// directory (`None` when the platform has no such directory).
pub fn untitled_path(app_dir: Option<&Path>) -> Option<PathBuf> {
    app_dir.map(|dir| dir.join(AUTOSAVE_DIR).join(UNTITLED_FILE))
}

/// Where the current document's snapshot belongs: beside the project file, or
/// in app data while it is still untitled.
pub fn target(project_path: Option<&Path>, app_dir: Option<&Path>) -> Option<PathBuf> {
    match project_path {
        Some(path) => Some(autosave_path(path)),
        None => untitled_path(app_dir),
    }
}

/// Writes the snapshot (creating the `autosave/` directory when needed).
/// **Not real-time** — disk IO on the UI thread, at most once per [`SPACING`].
pub fn write(project: &Project, path: &Path) -> Result<(), ModelError> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    save_autosave(project, path)
}

/// Removes a snapshot. Missing files are fine: every cleanup path is
/// best-effort and must never turn into an error the user could not act on.
pub fn discard(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// One snapshot found at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The snapshot file to load.
    pub autosave: PathBuf,
    /// The project it belongs to; `None` for a never-saved document.
    pub project: Option<PathBuf>,
    /// Snapshot size in bytes (shown in the prompt).
    pub bytes: u64,
    /// Snapshot modification time (shown in the prompt as an age).
    pub saved_at: Option<SystemTime>,
}

impl Candidate {
    /// Name to show in the recovery prompt: the project's file name, or
    /// `Untitled` for a snapshot of a document that was never saved.
    pub fn display_name(&self) -> String {
        self.project
            .as_deref()
            .and_then(Path::file_name)
            .map_or_else(
                || "Untitled".to_string(),
                |name| name.to_string_lossy().into_owned(),
            )
    }
}

/// Snapshots worth offering at startup: one per recently opened project, plus
/// the never-saved document's. Order follows `recent` (newest first), the
/// untitled snapshot last.
pub fn candidates(recent: &[PathBuf], app_dir: Option<&Path>) -> Vec<Candidate> {
    let mut found = Vec::new();
    for project in recent {
        if let Some(candidate) = inspect(autosave_path(project), Some(project.clone())) {
            found.push(candidate);
        }
    }
    if let Some(path) = untitled_path(app_dir)
        && let Some(candidate) = inspect(path, None)
    {
        found.push(candidate);
    }
    found
}

/// A snapshot that exists as a regular file.
fn inspect(path: PathBuf, project: Option<PathBuf>) -> Option<Candidate> {
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(Candidate {
        autosave: path,
        project,
        bytes: meta.len(),
        saved_at: meta.modified().ok(),
    })
}

/// Age of a snapshot, for the prompt: "just now", "12 min ago", "3 h ago".
pub fn age_label(saved_at: Option<SystemTime>, now: SystemTime) -> String {
    let Some(saved_at) = saved_at else {
        return "time unknown".to_string();
    };
    let Ok(age) = now.duration_since(saved_at) else {
        // A clock that jumped backwards must not print a negative age.
        return "just now".to_string();
    };
    let secs = age.as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86_400 {
        format!("{} h ago", secs / 3600)
    } else {
        format!("{} d ago", secs / 86_400)
    }
}

/// When the next snapshot is due. The app feeds one observation per frame and
/// asks [`Clock::due`] at the end of the frame; all arithmetic is on `Instant`s
/// handed in by the caller, which keeps the schedule testable without sleeping.
#[derive(Debug, Clone)]
pub struct Clock {
    /// Last document revision seen.
    revision: u64,
    /// When the document last changed.
    last_edit: Option<Instant>,
    /// When a snapshot was last written (the app's start time until then).
    last_write: Instant,
    /// Revision that the snapshot on disk matches; equal to `revision` means
    /// nothing new to write.
    written_revision: u64,
    /// Document has unsaved changes.
    dirty: bool,
}

/// `revision` stands for "never observed" — a real revision starts at 1.
const UNSEEN: u64 = u64::MAX;

impl Clock {
    /// Starts the clock as if a snapshot had just been written: the first
    /// [`SPACING`] of a session cannot fire.
    pub fn new(now: Instant) -> Self {
        Self {
            revision: UNSEEN,
            last_edit: None,
            last_write: now,
            written_revision: UNSEEN,
            dirty: false,
        }
    }

    /// Restart for a swapped document (open / new / recover). Nothing is known
    /// about the new document yet, so it arms from scratch.
    pub fn reset(&mut self, now: Instant) {
        *self = Self::new(now);
    }

    /// Records this frame's document state.
    pub fn observe(&mut self, revision: u64, dirty: bool, now: Instant) {
        if revision != self.revision {
            self.revision = revision;
            self.last_edit = Some(now);
        }
        self.dirty = dirty;
    }

    /// Whether to write a snapshot now: the document is dirty, the snapshot is
    /// stale, and either the user went quiet or [`SPACING`] elapsed.
    pub fn due(&self, now: Instant) -> bool {
        self.dirty
            && self.revision != self.written_revision
            && (self
                .last_edit
                .is_none_or(|at| now.duration_since(at) >= QUIET)
                || now.duration_since(self.last_write) >= SPACING)
    }

    /// Records a successful (or failed — no point retrying every frame) write.
    pub fn mark_written(&mut self, now: Instant) {
        self.last_write = now;
        self.written_revision = self.revision;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn a_quiet_pause_arms_the_snapshot() {
        let base = Instant::now();
        let mut clock = Clock::new(base);
        clock.observe(2, true, at(base, 0));

        assert!(!clock.due(at(base, 0)), "a snapshot mid-edit is noise");
        assert!(!clock.due(at(base, 29)), "quiet time not reached yet");
        assert_eq!(QUIET, Duration::from_secs(30));
        assert!(clock.due(at(base, 30)), "30 s of quiet arms it");

        clock.mark_written(at(base, 30));
        assert!(!clock.due(at(base, 31)), "nothing changed since the write");

        // A new edit re-arms, but the spacing floor still applies.
        clock.observe(3, true, at(base, 35));
        assert!(!clock.due(at(base, 40)), "spacing floor holds it back");
        assert!(clock.due(at(base, 65)), "quiet again after the new edit");
    }

    #[test]
    fn continuous_editing_still_snapshots_on_the_spacing() {
        let base = Instant::now();
        let mut clock = Clock::new(base);
        // A revision bump every 10 s: the quiet window never opens.
        for step in 0..5 {
            clock.observe(2 + step, true, at(base, step * 10));
            assert!(
                !clock.due(at(base, step * 10)),
                "spacing must not fire before 60 s (step {step})"
            );
        }
        clock.observe(7, true, at(base, 50));
        assert!(!clock.due(at(base, 50)));
        assert_eq!(SPACING, Duration::from_secs(60));
        assert!(clock.due(at(base, 60)), "the spacing floor flushes it");
    }

    #[test]
    fn a_clean_document_is_never_snapshotted() {
        let base = Instant::now();
        let mut clock = Clock::new(base);
        clock.observe(2, false, at(base, 0));
        assert!(!clock.due(at(base, 10_000)));

        // Saving does not need the clock: a clean document stops asking.
        clock.observe(3, true, at(base, 0));
        clock.mark_written(at(base, 30));
        clock.observe(4, false, at(base, 40));
        assert!(!clock.due(at(base, 10_000)));
    }

    #[test]
    fn snapshots_are_found_beside_recent_projects_and_in_app_data() {
        let dir = std::env::temp_dir().join(format!("glitcherz_autosave_{}", std::process::id()));
        let app_dir = dir.join("app");
        let project = dir.join("song.glz");
        let _ = std::fs::create_dir_all(&app_dir);

        assert!(candidates(std::slice::from_ref(&project), Some(&app_dir)).is_empty());

        let snap = autosave_path(&project);
        std::fs::write(&snap, "snapshot").expect("write snapshot");
        let untitled = untitled_path(Some(&app_dir)).expect("app data dir");
        std::fs::create_dir_all(untitled.parent().expect("parent")).expect("create dir");
        std::fs::write(&untitled, "untitled snapshot").expect("write untitled");

        let found = candidates(std::slice::from_ref(&project), Some(&app_dir));
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].autosave, snap);
        assert_eq!(found[0].project, Some(project.clone()));
        assert_eq!(found[0].display_name(), "song.glz");
        assert_eq!(found[1].project, None);
        assert_eq!(found[1].display_name(), "Untitled");

        discard(&snap);
        discard(&untitled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn age_labels_stay_readable() {
        let now = SystemTime::now();
        assert_eq!(
            age_label(Some(now - Duration::from_secs(5)), now),
            "just now"
        );
        assert_eq!(
            age_label(Some(now - Duration::from_secs(12 * 60)), now),
            "12 min ago"
        );
        assert_eq!(
            age_label(Some(now - Duration::from_secs(3 * 3600)), now),
            "3 h ago"
        );
        assert_eq!(
            age_label(Some(now - Duration::from_secs(2 * 86_400)), now),
            "2 d ago"
        );
        assert_eq!(age_label(None, now), "time unknown");
    }
}
