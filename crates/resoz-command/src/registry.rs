//! The command registry: one module per menu area, each exposing `specs()`.
//!
//! A command is identified by a namespaced string id (`midi.note_add`)
//! rather than a closure, so the registry is enumerable: the UI can build
//! menus from it and the self-check test can run every command without
//! knowing what any of them do.

use std::sync::OnceLock;

use serde::Serialize;
use serde_json::Value;

use crate::Session;

/// A command implementation.
pub type Run = fn(&mut Session, &Value) -> crate::Result<Value>;

/// Decides whether a command applies right now; `Err(reason)` explains why
/// not, and the reason is shown in the UI.
pub type Enabled = fn(&Session) -> std::result::Result<(), String>;

/// A registered command.
pub struct CommandSpec {
    /// Namespaced identifier, e.g. `midi.note_add`.
    pub id: &'static str,
    /// Human-readable name for menus and undo labels.
    pub label: &'static str,
    /// Menu path, e.g. `["Event", "MIDI Operations"]`; empty = not in a menu.
    pub menu: &'static [&'static str],
    /// Keyboard shortcut, if one is bound.
    pub shortcut: Option<&'static str>,
    /// Human-readable parameter documentation.
    pub params: &'static str,
    /// Applicability check.
    pub enabled: Enabled,
    /// The implementation.
    pub run: Run,
    /// Whether the command records an undo step.
    pub undoable: bool,
}

impl std::fmt::Debug for CommandSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandSpec")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("undoable", &self.undoable)
            .finish()
    }
}

/// Serializable description of a command, for menus and introspection.
#[derive(Debug, Clone, Serialize)]
pub struct CommandInfo {
    pub id: String,
    pub label: String,
    pub menu: Vec<String>,
    pub shortcut: Option<String>,
    pub params: String,
    pub enabled: bool,
    /// Why the command is unavailable, when it is.
    pub disabled_reason: Option<String>,
}

impl CommandSpec {
    /// Resolves this command's availability against a session.
    pub fn info(&self, session: &Session) -> CommandInfo {
        let enabled = (self.enabled)(session);
        CommandInfo {
            id: self.id.to_string(),
            label: self.label.to_string(),
            menu: self.menu.iter().map(|m| m.to_string()).collect(),
            shortcut: self.shortcut.map(str::to_string),
            params: self.params.to_string(),
            enabled: enabled.is_ok(),
            disabled_reason: enabled.err(),
        }
    }
}

/// `cmd!(id, label, [menu…], shortcut, params, enabled, run)` — an undoable
/// command. `cmd!(noundo …)` for queries and transport moves, which have no
/// meaningful "before" state.
#[macro_export]
macro_rules! cmd {
    (noundo $id:literal, $label:literal, [$($m:literal),*], $sc:expr, $params:literal, $en:expr, $run:expr) => {
        $crate::CommandSpec {
            id: $id,
            label: $label,
            menu: &[$($m),*],
            shortcut: $sc,
            params: $params,
            enabled: $en,
            run: $run,
            undoable: false,
        }
    };
    ($id:literal, $label:literal, [$($m:literal),*], $sc:expr, $params:literal, $en:expr, $run:expr) => {
        $crate::CommandSpec {
            id: $id,
            label: $label,
            menu: &[$($m),*],
            shortcut: $sc,
            params: $params,
            enabled: $en,
            run: $run,
            undoable: true,
        }
    };
}

/// Every registered command.
pub fn command_specs() -> &'static [CommandSpec] {
    static SPECS: OnceLock<Vec<CommandSpec>> = OnceLock::new();
    SPECS.get_or_init(|| {
        let mut v = Vec::new();
        v.extend(crate::midi::specs());
        v
    })
}

/// Looks up a command by id.
pub fn find_command(id: &str) -> Option<&'static CommandSpec> {
    command_specs().iter().find(|c| c.id == id)
}

/// The MIDI commands that belong in a menu (those without one are internal).
///
/// Returns an owned vector: [`crate::midi::specs`] builds a fresh `Vec`, so
/// handing out references into it would dangle.
pub fn midi_specs() -> Vec<CommandSpec> {
    crate::midi::specs()
        .into_iter()
        .filter(|c| !c.menu.is_empty())
        .collect()
}

// ---- shared enablement -------------------------------------------------------------------

/// Always applicable.
pub fn always(_: &Session) -> std::result::Result<(), String> {
    Ok(())
}

/// The document has at least one MIDI track.
pub fn has_midi_tracks(session: &Session) -> std::result::Result<(), String> {
    if session.project().midi_tracks.is_empty() {
        Err("the project has no MIDI tracks".into())
    } else {
        Ok(())
    }
}

/// The document has at least one note somewhere.
pub fn has_notes(session: &Session) -> std::result::Result<(), String> {
    if session
        .project()
        .midi_tracks
        .iter()
        .flat_map(|t| t.clips.iter())
        .any(|c| !c.notes().is_empty())
    {
        Ok(())
    } else {
        Err("there are no notes to edit".into())
    }
}

// ---- parameter helpers -------------------------------------------------------------------
//
// These are part of the crate's public surface for command implementations
// (including ones living outside this crate, e.g. the M3 event operations),
// so unused ones here are not dead code.

/// Reads an integer parameter, falling back to `default`.
pub fn i64_or(params: &Value, key: &str, default: i64) -> i64 {
    params
        .get(key)
        .and_then(|v| {
            v.as_i64().or_else(|| {
                v.as_f64()
                    .filter(|f| f.is_finite())
                    .map(|f| f.round() as i64)
            })
        })
        .unwrap_or(default)
}

/// Reads a float parameter, falling back to `default`.
pub fn f64_or(params: &Value, key: &str, default: f64) -> f64 {
    params
        .get(key)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .unwrap_or(default)
}

/// Reads a string parameter.
pub fn str_param<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

/// Reads an index array parameter (e.g. a selection).
pub fn indices(params: &Value, key: &str) -> Vec<usize> {
    params
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_u64)
                .map(|v| v as usize)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_namespaced_and_documented() {
        let mut seen = HashSet::new();
        for command in command_specs() {
            assert!(
                seen.insert(command.id),
                "duplicate command id {}",
                command.id
            );
            assert!(!command.label.is_empty(), "{} has no label", command.id);
            assert!(
                command.id.contains('.'),
                "ids must be namespaced: {}",
                command.id
            );
            assert!(
                !command.params.is_empty(),
                "{} must document its parameters",
                command.id
            );
        }
    }

    #[test]
    fn menu_paths_start_with_a_known_area() {
        const AREAS: &[&str] = &["Edit", "Event", "Midi", "View", "Transport"];
        for command in command_specs() {
            if let Some(area) = command.menu.first() {
                assert!(
                    AREAS.contains(area),
                    "{}: unknown menu area `{area}`",
                    command.id
                );
            }
        }
    }

    #[test]
    fn command_info_reports_why_a_command_is_unavailable() {
        let session = Session::default();
        // A default project has no notes, so the velocity command is gated off.
        let spec = find_command("midi.change_velocity").expect("registered");
        let info = spec.info(&session);
        assert!(!info.enabled);
        assert_eq!(
            info.disabled_reason.as_deref(),
            Some("there are no notes to edit"),
            "the reason is surfaced for the UI"
        );
        assert_eq!(info.menu, vec!["Midi", "Event"]);
        assert_eq!(info.shortcut.as_deref(), None);

        // `midi.query` is a plain read and is never gated.
        let query = find_command("midi.query").expect("registered");
        assert!(query.info(&session).enabled);
        assert!(!query.undoable, "a query has no before state");
    }
}
