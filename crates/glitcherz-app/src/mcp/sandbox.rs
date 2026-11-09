//! The MCP path sandbox (S5, spec `mcp-server.md` §6).
//!
//! Every path an MCP client supplies goes through [`Sandbox::resolve`]: it is
//! joined to the root, normalised lexically, and rejected unless it still
//! starts with the root. Absolute paths are checked too — otherwise "just use
//! an absolute path" would be the way around the whole rule.

use std::path::{Component, Path, PathBuf};

/// One resolved root plus its real (symlink-free) form.
#[derive(Debug, Clone)]
pub struct Sandbox {
    /// Lexical absolute root: what relative paths are joined to, and what
    /// appears in results (no `\\?\` prefix).
    root: PathBuf,
    /// `root` after `canonicalize`, used only for the symlink recheck.
    real_root: PathBuf,
}

impl Sandbox {
    /// Anchors the sandbox at `raw`, resolved against the current directory.
    pub fn new(raw: &Path) -> Result<Self, String> {
        let cwd = std::env::current_dir()
            .map_err(|err| format!("cannot read the current directory: {err}"))?;
        let root = normalize(&if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            cwd.join(raw)
        });
        let real_root = root
            .canonicalize()
            .map_err(|err| format!("{}: {err}", root.display()))?;
        if !real_root.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }
        Ok(Self { root, real_root })
    }

    /// The root as it appears in results.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a client-supplied path, or explains why it is out of bounds.
    pub fn resolve(&self, raw: &str) -> Result<PathBuf, String> {
        if raw.trim().is_empty() {
            return Err("path is empty".to_string());
        }
        let requested = Path::new(raw);
        let joined = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.join(requested)
        };
        let normalized = normalize(&joined);
        if !normalized.starts_with(&self.root) {
            return Err(format!("`{raw}` escapes the sandbox root"));
        }
        // Lexical normalisation cannot see a symlink inside the root that
        // points outside it. When the parent directory exists we ask the
        // filesystem: this catches the ordinary case (a link to a folder that
        // already exists). The TOCTOU window between this check and the write
        // is documented as a known gap in spec §9.
        if let Some(parent) = normalized.parent()
            && let Ok(real_parent) = parent.canonicalize()
            && !real_parent.starts_with(&self.real_root)
        {
            return Err(format!("`{raw}` escapes the sandbox root"));
        }
        Ok(normalized)
    }
}

/// Drops `.`, folds `..` into the parent, and keeps the rest verbatim.
///
/// Pure string surgery on purpose: the target of a write does not exist yet,
/// so `canonicalize` is not an option. `..` above a root (drive or `/`) is a
/// no-op, and [`Sandbox::resolve`]'s prefix check is what actually rejects it.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> (Sandbox, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("glitcherz_s5_sandbox_{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create root");
        let sandbox = Sandbox::new(&root).expect("sandbox");
        (sandbox, root)
    }

    #[test]
    fn relative_paths_land_under_the_root() {
        let (sandbox, root) = sandbox();
        let resolved = sandbox.resolve("song.glz").expect("resolve");
        assert_eq!(resolved, root.join("song.glz"));
    }

    #[test]
    fn dot_segments_are_folded() {
        let (sandbox, root) = sandbox();
        let resolved = sandbox.resolve("./a/./b/../c.wav").expect("resolve");
        assert_eq!(resolved, root.join("a/c.wav"));
    }

    #[test]
    fn parent_traversal_is_rejected() {
        let (sandbox, _root) = sandbox();
        let err = sandbox.resolve("../escape.wav").expect_err("must reject");
        assert!(err.contains("escapes the sandbox root"), "{err}");
        // The absolute form of the same escape must be rejected too.
        let absolute = sandbox.root().parent().expect("parent").join("escape.wav");
        let err = sandbox
            .resolve(&absolute.display().to_string())
            .expect_err("must reject absolute");
        assert!(err.contains("escapes the sandbox root"), "{err}");
    }

    #[test]
    fn an_absolute_path_inside_the_root_is_allowed() {
        let (sandbox, root) = sandbox();
        let inside = root.join("stems").join("kick.wav");
        let resolved = sandbox
            .resolve(&inside.display().to_string())
            .expect("resolve");
        assert_eq!(resolved, inside);
    }

    #[test]
    fn an_empty_path_is_rejected() {
        let (sandbox, _root) = sandbox();
        assert!(sandbox.resolve("  ").is_err());
    }

    #[test]
    fn a_missing_root_is_an_error() {
        let missing = std::env::temp_dir().join("glitcherz_s5_no_such_root_xyz");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(Sandbox::new(&missing).is_err());
    }
}
