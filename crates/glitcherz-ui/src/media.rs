//! Project media bookkeeping (S3b): deciding where a collected file lands, and
//! finding a relocated one.
//!
//! Both operations are pure filesystem helpers — the app owns the dialogs and
//! the undo step, this module owns the naming rules, so the rules can be tested
//! without a window.

use std::path::{Path, PathBuf};

/// How deep a relocation search walks below the folder the user picked. Four
/// levels covers `Album/Samples/Drums/Kicks` without turning a mistyped root
/// (`C:\`) into a full-disk scan.
pub const SEARCH_DEPTH: usize = 4;

/// Entries a relocation search may inspect before giving up. A guard against
/// picking a huge folder by accident; real sample folders are far below this.
pub const SEARCH_BUDGET: usize = 20_000;

/// Where `source` should be copied inside `media_dir`.
///
/// The source's own file name is preferred. If a file with that name is already
/// there **and is a different size**, `name (2).ext`, `name (3).ext` … are
/// tried — two `kick.wav` from different packs must not overwrite each other.
/// An existing file of the same size counts as the same file: re-collecting a
/// 300 MB SoundFont must not duplicate it (the caller skips the copy).
pub fn collect_target(media_dir: &Path, source: &Path) -> PathBuf {
    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "media".to_string());
    let source_len = std::fs::metadata(source).ok().map(|meta| meta.len());
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem.to_string(), format!(".{extension}")),
        _ => (name.clone(), String::new()),
    };
    let mut n = 1;
    loop {
        let file = if n == 1 {
            name.clone()
        } else {
            format!("{stem} ({n}){extension}")
        };
        let candidate = media_dir.join(&file);
        match std::fs::metadata(&candidate) {
            // Nothing there, or the same file already collected.
            Err(_) => return candidate,
            Ok(meta) if Some(meta.len()) == source_len => return candidate,
            Ok(_) => n += 1,
        }
    }
}

/// Finds a file called `file_name` anywhere under `root` (breadth-first, at
/// most `depth` levels below it). Used by "relocate missing media": the user
/// points at the folder they moved their samples to and every missing file is
/// matched by name.
pub fn find_by_name(root: &Path, file_name: &str, depth: usize) -> Option<PathBuf> {
    let mut budget = SEARCH_BUDGET;
    find_by_name_inner(root, file_name, depth, &mut budget)
}

fn find_by_name_inner(
    dir: &Path,
    file_name: &str,
    depth: usize,
    budget: &mut usize,
) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_file() {
            // Case-insensitive on purpose: a project written on Windows and
            // relocated on a case-sensitive filesystem still has to find it.
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(file_name))
            {
                return Some(path);
            }
        } else if kind.is_dir() && depth > 0 {
            subdirs.push(path);
        }
    }
    for subdir in subdirs {
        if let Some(found) = find_by_name_inner(&subdir, file_name, depth - 1, budget) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{collect_target, find_by_name};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("glitcherz_media_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_free_name_is_used_as_is() {
        let dir = temp_dir("free");
        let media = dir.join("song.media");
        let source = dir.join("kick.wav");
        std::fs::write(&source, b"1234").expect("write");

        assert_eq!(collect_target(&media, &source), media.join("kick.wav"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn name_clashes_get_a_suffix_but_the_same_file_does_not() {
        let dir = temp_dir("clash");
        let media = dir.join("song.media");
        std::fs::create_dir_all(&media).expect("media dir");
        let source = dir.join("kick.wav");
        std::fs::write(&source, b"1234").expect("write");
        std::fs::write(media.join("kick.wav"), b"different length").expect("existing");

        assert_eq!(
            collect_target(&media, &source),
            media.join("kick (2).wav"),
            "a different file with the same name must not be overwritten"
        );

        // Same length = already collected: the caller skips the copy and the
        // path stays stable (re-running "collect" is idempotent).
        std::fs::write(media.join("kick (2).wav"), b"1234").expect("same size");
        assert_eq!(collect_target(&media, &source), media.join("kick (2).wav"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relocation_finds_a_file_by_name_inside_a_picked_folder() {
        let dir = temp_dir("relocate");
        let nested = dir.join("Pack").join("Drums").join("Kicks");
        std::fs::create_dir_all(&nested).expect("nested dir");
        std::fs::write(nested.join("Kick_01.wav"), b"x").expect("write");

        assert_eq!(
            find_by_name(&dir, "kick_01.wav", 4),
            Some(nested.join("Kick_01.wav")),
            "matching ignores case"
        );
        assert_eq!(find_by_name(&dir, "Snare.wav", 4), None);
        assert_eq!(
            find_by_name(&dir, "Kick_01.wav", 1),
            None,
            "the depth limit is respected (Pack/Drums/Kicks is three levels down)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
