//! Plugin catalog: the scan-result table behind the add-node picker.
//!
//! Spec: `.agents/spec/plugin-browser.md` (U9). This module is deliberately
//! egui-free — it owns the data model, the cache-validity plan and the
//! merge/sort helpers as **pure functions**, so the scan protocol can be
//! tested without a display, a plugin directory or a real plugin binary.
//!
//! Division of labour:
//! - [`collect_from_disk`] is the only disk-touching entry (read_dir + stat);
//! - [`plan_refresh`] diffs the cache against the disk state and says which
//!   modules must be (re)scanned (spec D7);
//! - [`merge`] folds the fresh scan results into the kept entries and sorts
//!   them for the picker (spec D8);
//! - encode/decode move the cache through `eframe` storage (spec D11).
//!
//! One module can yield several entries (a VST3 bundle may expose multiple
//! classes), which is why cache validity is tracked **per module path**, not
//! per entry.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Version of the on-disk cache layout. A mismatch forces a full rescan.
pub const CACHE_VERSION: u32 = 1;

/// Plugin binary format. Both formats are scanned in one pass (spec D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginFormat {
    /// CLAP (`.clap` single file).
    Clap,
    /// VST3 (single file or bundle directory).
    Vst3,
}

impl PluginFormat {
    /// Stable lower-case tag, used for cache directory keys only.
    pub fn as_str(self) -> &'static str {
        match self {
            PluginFormat::Clap => "clap",
            PluginFormat::Vst3 => "vst3",
        }
    }

    /// Tie-break rank for the picker ordering.
    fn sort_rank(self) -> u8 {
        match self {
            PluginFormat::Clap => 0,
            PluginFormat::Vst3 => 1,
        }
    }
}

/// One instantiable plugin description.
///
/// `(path, id)` is the identity: the module path alone is ambiguous because a
/// single VST3 bundle can register several classes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEntry {
    /// Binary format.
    pub format: PluginFormat,
    /// Module path (single file or bundle), also the cache key.
    pub path: String,
    /// CLAP plugin id / VST3 component class id.
    pub id: String,
    /// Display name from the plugin descriptor.
    pub name: String,
    /// Classification hint only (spec D9): the picker shows everything under
    /// `Show all plugins…`, so a wrong guess never hides an entry.
    pub is_instrument: bool,
    /// Module modification time (ms since the Unix epoch) — cache validity.
    pub mtime_ms: i64,
    /// Module byte length — cache validity (catches same-second edits).
    pub size: u64,
}

/// One candidate module found on disk, carrying its stat.
///
/// The stat travels with the candidate so a rescanned entry can be built
/// without touching the filesystem twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateFile {
    /// Binary format of this module.
    pub format: PluginFormat,
    /// Module path.
    pub path: PathBuf,
    /// Modification time (ms since the Unix epoch).
    pub mtime_ms: i64,
    /// Byte length (directory bundles report their own metadata length).
    pub size: u64,
}

impl CandidateFile {
    /// Stats `path`. Returns `None` when the file vanished or cannot be read
    /// — a missing candidate is simply not part of the plan.
    pub fn stat(format: PluginFormat, path: impl Into<PathBuf>) -> Option<Self> {
        let path = path.into();
        let metadata = std::fs::metadata(&path).ok()?;
        let mtime_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since_epoch| since_epoch.as_millis() as i64)
            .unwrap_or(0);
        Some(Self {
            format,
            path,
            mtime_ms,
            size: metadata.len(),
        })
    }
}

/// The persisted plugin table (spec §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCache {
    /// Layout version; see [`CACHE_VERSION`].
    pub version: u32,
    /// Search directories at scan time, tagged as `"<format>:<path>"`.
    ///
    /// A change to either format's directory set invalidates the whole cache
    /// (spec D7) — the cache cannot know which entries the change affects.
    #[serde(default)]
    pub dirs: Vec<String>,
    /// Scanned entries, sorted by display name.
    #[serde(default)]
    pub entries: Vec<PluginEntry>,
}

impl Default for PluginCache {
    fn default() -> Self {
        Self {
            version: CACHE_VERSION,
            dirs: Vec::new(),
            entries: Vec::new(),
        }
    }
}

/// What a refresh still has to do (spec D7).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanPlan {
    /// Cache entries whose module is unchanged — usable immediately.
    pub keep: Vec<PluginEntry>,
    /// Modules that are new or changed, in discovery order.
    pub rescan: Vec<CandidateFile>,
    /// Cache paths that no longer exist on disk (logged; nothing to reclaim).
    pub removed: Vec<String>,
}

impl ScanPlan {
    /// `true` when no module has to be scanned (warm start, spec §5.1).
    pub fn is_warm(&self) -> bool {
        self.rescan.is_empty()
    }
}

/// Disk snapshot: the search directories plus every candidate module.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiskScanInput {
    /// Tagged search directories (see [`PluginCache::dirs`]).
    pub dirs: Vec<String>,
    /// Candidate modules with stats.
    pub candidates: Vec<CandidateFile>,
}

/// Enumerates the search directories and stats every candidate module.
///
/// Only the UI thread calls this (spec D6); it is a couple of `read_dir`
/// passes plus one `stat` per plugin file.
pub fn collect_from_disk() -> DiskScanInput {
    let mut input = DiskScanInput::default();
    absorb(
        &mut input,
        PluginFormat::Clap,
        glitcherz_clap_host::clap_search_paths(),
        glitcherz_clap_host::scan_clap_files(),
    );
    absorb(
        &mut input,
        PluginFormat::Vst3,
        glitcherz_vst3_host::vst3_search_paths(),
        glitcherz_vst3_host::scan_vst3_files(),
    );
    input
}

fn absorb(
    input: &mut DiskScanInput,
    format: PluginFormat,
    dirs: Vec<PathBuf>,
    files: Vec<PathBuf>,
) {
    for dir in dirs {
        let tagged = format!("{}:{}", format.as_str(), dir.display());
        if !input.dirs.contains(&tagged) {
            input.dirs.push(tagged);
        }
    }
    // A few hundred modules at most: the quadratic dedup is cheaper than
    // building a keyed set (and keeps the candidate order stable).
    for file in files {
        let Some(candidate) = CandidateFile::stat(format, file) else {
            continue;
        };
        let duplicate = input
            .candidates
            .iter()
            .any(|existing| existing.format == candidate.format && existing.path == candidate.path);
        if !duplicate {
            input.candidates.push(candidate);
        }
    }
}

/// Diffs the cache against the disk snapshot (spec §5.3).
///
/// `dirs` must be [`DiskScanInput::dirs`]; a changed directory set or a
/// version bump drops every cached entry (full rescan).
pub fn plan_refresh(
    cache: &PluginCache,
    dirs: &[String],
    candidates: &[CandidateFile],
) -> ScanPlan {
    let cache_valid = cache.version == CACHE_VERSION && cache.dirs == dirs;

    // Group cached entries by module path: validity is per module, but a
    // module may contribute several entries.
    let mut by_path: HashMap<String, Vec<PluginEntry>> = HashMap::new();
    if cache_valid {
        for entry in &cache.entries {
            by_path
                .entry(entry.path.clone())
                .or_default()
                .push(entry.clone());
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut plan = ScanPlan::default();
    for candidate in candidates {
        let path = candidate.path.to_string_lossy().into_owned();
        if !seen.insert(path.clone()) {
            continue;
        }
        let unchanged = by_path.get(&path).is_some_and(|entries| {
            entries.first().is_some_and(|entry| {
                entry.mtime_ms == candidate.mtime_ms && entry.size == candidate.size
            })
        });
        if unchanged {
            if let Some(entries) = by_path.get(&path) {
                plan.keep.extend(entries.iter().cloned());
            }
        } else {
            plan.rescan.push(candidate.clone());
        }
    }

    let mut removed: Vec<String> = if cache_valid {
        by_path
            .keys()
            .filter(|path| !seen.contains(*path))
            .cloned()
            .collect()
    } else {
        // An invalidated cache means every old entry is gone in practice.
        cache
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect()
    };
    removed.sort();
    removed.dedup();
    plan.removed = removed;
    plan
}

/// Folds fresh scan results into the kept entries and sorts them (spec D8).
pub fn merge(keep: Vec<PluginEntry>, scanned: Vec<PluginEntry>) -> Vec<PluginEntry> {
    let mut entries = keep;
    entries.extend(scanned);
    sort_entries(&mut entries);
    entries
}

/// Sorts in place for the picker: display name (case-insensitive) first,
/// then format, module path and id as stable tie-breaks.
///
/// Also used after each incremental append so a partially scanned list reads
/// in order while the scan is still running.
pub fn sort_entries(entries: &mut [PluginEntry]) {
    entries.sort_by_cached_key(|entry| {
        (
            entry.name.to_lowercase(),
            entry.format.sort_rank(),
            entry.path.clone(),
            entry.id.clone(),
        )
    });
}

/// One picker view (spec D8): entries are partitioned at query time, never
/// scanned twice.
pub fn view(
    entries: &[PluginEntry],
    format: PluginFormat,
    want_instrument: bool,
) -> impl Iterator<Item = &PluginEntry> + '_ {
    entries
        .iter()
        .filter(move |entry| entry.format == format && entry.is_instrument == want_instrument)
}

/// Serializes the cache for `eframe` storage (spec D11).
pub fn encode(cache: &PluginCache) -> Option<String> {
    match ron::ser::to_string_pretty(cache, ron::ser::PrettyConfig::new()) {
        Ok(text) => Some(text),
        Err(err) => {
            log::warn!("Failed to encode the plugin cache: {err}");
            None
        }
    }
}

/// Parses a cache blob. `None` on malformed input (treated as "no cache",
/// which only costs a cold scan).
pub fn decode(text: &str) -> Option<PluginCache> {
    match ron::from_str::<PluginCache>(text) {
        Ok(cache) => Some(cache),
        Err(err) => {
            log::warn!("Failed to decode the plugin cache: {err}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime scan state
// ---------------------------------------------------------------------------

/// Runtime scan state (spec §5.4).
///
/// The frame loop drives it: read [`PluginCatalog::upcoming`] while drawing
/// the progress banner, call [`PluginCatalog::next`] at the **end** of the
/// frame, then feed the result back through [`PluginCatalog::accept`]
/// (spec D14 — the frame only ever blocks on a module it has already
/// announced on screen).
pub struct PluginCatalog {
    /// Search directories this catalog was built from (cache key, spec D7).
    dirs: Vec<String>,
    /// Usable entries, sorted for the picker.
    entries: Vec<PluginEntry>,
    /// Modules still waiting for a scan, in discovery order.
    pending: VecDeque<CandidateFile>,
    /// Modules scanned in this pass.
    scanned: usize,
    /// Modules this pass started with.
    total: usize,
}

impl PluginCatalog {
    /// Builds the catalog from the persisted blob (spec D11).
    ///
    /// A missing or malformed blob simply means "cold start"; a warm cache
    /// with no changed module yields an idle catalog with no scan to run.
    pub fn load(cache_text: Option<&str>) -> Self {
        let cache = cache_text.and_then(decode).unwrap_or_default();
        Self::with_snapshot(&cache, collect_from_disk())
    }

    /// Testable constructor: plan a refresh against an explicit snapshot.
    pub fn with_snapshot(cache: &PluginCache, snapshot: DiskScanInput) -> Self {
        let plan = plan_refresh(cache, &snapshot.dirs, &snapshot.candidates);
        if !plan.removed.is_empty() {
            log::info!(
                "{} plugin module(s) referenced by the cache are gone",
                plan.removed.len()
            );
        }
        let mut entries = plan.keep;
        sort_entries(&mut entries);
        Self {
            dirs: snapshot.dirs,
            entries,
            total: plan.rescan.len(),
            pending: plan.rescan.into(),
            scanned: 0,
        }
    }

    /// Usable entries (cache hits plus everything scanned so far).
    pub fn entries(&self) -> &[PluginEntry] {
        &self.entries
    }

    /// Module the next [`PluginCatalog::next`] will read — the name shown in
    /// the progress banner before the frame blocks on it (spec D14).
    pub fn upcoming(&self) -> Option<&CandidateFile> {
        self.pending.front()
    }

    /// Takes the next module to scan. The caller scans it and calls
    /// [`PluginCatalog::accept`] whatever the outcome (progress must advance
    /// even when a module yields nothing).
    pub fn take_next(&mut self) -> Option<CandidateFile> {
        self.pending.pop_front()
    }

    /// Folds one module's scan result into the table.
    pub fn accept(&mut self, scanned: Vec<PluginEntry>) {
        self.scanned += 1;
        self.entries = merge(std::mem::take(&mut self.entries), scanned);
    }

    /// `true` while modules remain to be scanned.
    pub fn is_scanning(&self) -> bool {
        !self.pending.is_empty()
    }

    /// `(scanned, total)` for this pass.
    pub fn progress(&self) -> (usize, usize) {
        (self.scanned, self.total)
    }

    /// Forces a full re-scan, ignoring every cached stat (`Rescan Plugins`).
    pub fn rescan(&mut self) {
        // An empty cache with no directories never matches, so every
        // candidate ends up in the rescan list (spec §5.4).
        *self = Self::with_snapshot(&PluginCache::default(), collect_from_disk());
    }

    /// Snapshot for persistence.
    pub fn to_cache(&self) -> PluginCache {
        PluginCache {
            version: CACHE_VERSION,
            dirs: self.dirs.clone(),
            entries: self.entries.clone(),
        }
    }
}

/// Scans one module into entries (blocking; UI thread only, spec D6).
///
/// A module that fails to load yields an empty list — the existing
/// `list_*_plugins_in` helpers already log the reason.
pub fn scan_module(candidate: &CandidateFile) -> Vec<PluginEntry> {
    let path = candidate.path.to_string_lossy().into_owned();
    let entry = |id: String, name: String, is_instrument: bool| PluginEntry {
        format: candidate.format,
        path: path.clone(),
        id,
        name,
        is_instrument,
        mtime_ms: candidate.mtime_ms,
        size: candidate.size,
    };
    match candidate.format {
        PluginFormat::Clap => glitcherz_clap_host::list_clap_plugins_in(&candidate.path)
            .into_iter()
            .map(|info| entry(info.id, info.name, info.is_instrument))
            .collect(),
        PluginFormat::Vst3 => glitcherz_vst3_host::list_vst3_plugins_in(&candidate.path)
            .into_iter()
            .map(|info| entry(info.cid, info.name, info.is_instrument))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        format: PluginFormat,
        path: &str,
        id: &str,
        name: &str,
        is_instrument: bool,
        mtime_ms: i64,
        size: u64,
    ) -> PluginEntry {
        PluginEntry {
            format,
            path: path.to_string(),
            id: id.to_string(),
            name: name.to_string(),
            is_instrument,
            mtime_ms,
            size,
        }
    }

    fn candidate(format: PluginFormat, path: &str, mtime_ms: i64, size: u64) -> CandidateFile {
        CandidateFile {
            format,
            path: PathBuf::from(path),
            mtime_ms,
            size,
        }
    }

    fn cache_with(dirs: &[&str], entries: Vec<PluginEntry>) -> PluginCache {
        PluginCache {
            version: CACHE_VERSION,
            dirs: dirs.iter().map(|dir| (*dir).to_string()).collect(),
            entries,
        }
    }

    #[test]
    fn plan_keeps_unchanged_module_and_all_of_its_classes() {
        // A VST3 bundle with two classes: one stat must keep both entries.
        let cache = cache_with(
            &["vst3:C:/vst3"],
            vec![
                entry(
                    PluginFormat::Vst3,
                    "C:/vst3/A.vst3",
                    "aaa",
                    "Alpha",
                    true,
                    100,
                    4096,
                ),
                entry(
                    PluginFormat::Vst3,
                    "C:/vst3/A.vst3",
                    "bbb",
                    "Beta",
                    false,
                    100,
                    4096,
                ),
            ],
        );
        let plan = plan_refresh(
            &cache,
            &["vst3:C:/vst3".to_string()],
            &[candidate(PluginFormat::Vst3, "C:/vst3/A.vst3", 100, 4096)],
        );
        assert!(plan.is_warm());
        assert_eq!(plan.keep.len(), 2);
        assert_eq!(plan.keep[0].id, "aaa");
        assert_eq!(plan.keep[1].id, "bbb");
        assert!(plan.removed.is_empty());
    }

    #[test]
    fn plan_rescans_when_mtime_or_size_changes() {
        let cache = cache_with(
            &["clap:C:/clap"],
            vec![entry(
                PluginFormat::Clap,
                "C:/clap/x.clap",
                "x",
                "X",
                true,
                100,
                10,
            )],
        );
        let dirs = ["clap:C:/clap".to_string()];

        let newer = plan_refresh(
            &cache,
            &dirs,
            &[candidate(PluginFormat::Clap, "C:/clap/x.clap", 200, 10)],
        );
        assert_eq!(newer.rescan.len(), 1);
        assert!(newer.keep.is_empty());

        // Same timestamp, different length: a same-second rebuild.
        let resized = plan_refresh(
            &cache,
            &dirs,
            &[candidate(PluginFormat::Clap, "C:/clap/x.clap", 100, 11)],
        );
        assert_eq!(resized.rescan.len(), 1);
        assert!(resized.keep.is_empty());
    }

    #[test]
    fn plan_detects_new_and_vanished_modules() {
        let cache = cache_with(
            &["clap:C:/clap"],
            vec![
                entry(
                    PluginFormat::Clap,
                    "C:/clap/gone.clap",
                    "g",
                    "Gone",
                    true,
                    100,
                    10,
                ),
                entry(
                    PluginFormat::Clap,
                    "C:/clap/kept.clap",
                    "k",
                    "Kept",
                    true,
                    100,
                    10,
                ),
            ],
        );
        let plan = plan_refresh(
            &cache,
            &["clap:C:/clap".to_string()],
            &[
                candidate(PluginFormat::Clap, "C:/clap/kept.clap", 100, 10),
                candidate(PluginFormat::Clap, "C:/clap/new.clap", 300, 20),
            ],
        );
        assert_eq!(plan.keep.len(), 1);
        assert_eq!(plan.keep[0].id, "k");
        assert_eq!(plan.rescan.len(), 1);
        assert_eq!(plan.rescan[0].path, PathBuf::from("C:/clap/new.clap"));
        assert_eq!(plan.removed, vec!["C:/clap/gone.clap".to_string()]);
    }

    #[test]
    fn plan_full_rescan_on_version_or_directory_change() {
        let entries = vec![entry(
            PluginFormat::Clap,
            "C:/clap/x.clap",
            "x",
            "X",
            true,
            100,
            10,
        )];
        let candidates = [candidate(PluginFormat::Clap, "C:/clap/x.clap", 100, 10)];

        // Directory set changed → everything is rescanned, old paths reported.
        let dirs_moved = cache_with(&["clap:D:/other"], entries.clone());
        let plan = plan_refresh(&dirs_moved, &["clap:C:/clap".to_string()], &candidates);
        assert!(plan.keep.is_empty());
        assert_eq!(plan.rescan.len(), 1);
        assert_eq!(plan.removed, vec!["C:/clap/x.clap".to_string()]);

        // Layout version changed → same treatment.
        let mut stale = cache_with(&["clap:C:/clap"], entries);
        stale.version = CACHE_VERSION + 1;
        let plan = plan_refresh(&stale, &["clap:C:/clap".to_string()], &candidates);
        assert!(plan.keep.is_empty());
        assert_eq!(plan.rescan.len(), 1);
    }

    #[test]
    fn plan_deduplicates_repeated_candidates() {
        // The same directory can appear twice (env override + standard path).
        let cache = PluginCache::default();
        let plan = plan_refresh(
            &cache,
            &["clap:C:/clap".to_string()],
            &[
                candidate(PluginFormat::Clap, "C:/clap/x.clap", 100, 10),
                candidate(PluginFormat::Clap, "C:/clap/x.clap", 100, 10),
            ],
        );
        assert_eq!(plan.rescan.len(), 1);
    }

    #[test]
    fn merge_sorts_case_insensitively_with_stable_tiebreak() {
        let merged = merge(
            vec![entry(
                PluginFormat::Vst3,
                "C:/vst3/z.vst3",
                "z",
                "zeta",
                false,
                1,
                1,
            )],
            vec![
                entry(
                    PluginFormat::Vst3,
                    "C:/vst3/b.vst3",
                    "b",
                    "Beta",
                    false,
                    1,
                    1,
                ),
                entry(
                    PluginFormat::Clap,
                    "C:/clap/a.clap",
                    "a",
                    "alpha",
                    true,
                    1,
                    1,
                ),
                // Same name, different format: CLAP sorts before VST3.
                entry(
                    PluginFormat::Vst3,
                    "C:/vst3/a.vst3",
                    "a2",
                    "Alpha",
                    false,
                    1,
                    1,
                ),
            ],
        );
        let names: Vec<&str> = merged.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "Alpha", "Beta", "zeta"]);
        assert_eq!(merged[0].format, PluginFormat::Clap);
    }

    #[test]
    fn cache_ron_roundtrips_and_rejects_garbage() {
        let cache = cache_with(
            &["clap:C:/clap", "vst3:C:/vst3"],
            vec![entry(
                PluginFormat::Clap,
                "C:/clap/x.clap",
                "x",
                "X",
                true,
                100,
                10,
            )],
        );
        let text = encode(&cache).expect("encode");
        assert_eq!(decode(&text), Some(cache));
        assert!(decode("this is not ron").is_none());
    }

    #[test]
    fn view_partitions_by_format_and_kind() {
        let entries = vec![
            entry(PluginFormat::Clap, "p1", "1", "Inst", true, 0, 0),
            entry(PluginFormat::Clap, "p2", "2", "Fx", false, 0, 0),
            entry(PluginFormat::Vst3, "p3", "3", "Vst3Inst", true, 0, 0),
        ];
        let clap_instruments: Vec<&str> = view(&entries, PluginFormat::Clap, true)
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(clap_instruments, vec!["1"]);
        let vst3_effects: Vec<&str> = view(&entries, PluginFormat::Vst3, false)
            .map(|entry| entry.id.as_str())
            .collect();
        assert!(vst3_effects.is_empty());
    }

    #[test]
    fn catalog_scans_pending_modules_and_reports_progress() {
        let snapshot = DiskScanInput {
            dirs: vec!["clap:C:/clap".to_string()],
            candidates: vec![
                candidate(PluginFormat::Clap, "C:/clap/a.clap", 1, 2),
                candidate(PluginFormat::Clap, "C:/clap/b.clap", 3, 4),
            ],
        };
        let mut catalog = PluginCatalog::with_snapshot(&PluginCache::default(), snapshot);
        assert!(catalog.is_scanning());
        assert_eq!(catalog.progress(), (0, 2));
        // The banner announces the module the frame is about to block on.
        assert_eq!(
            catalog.upcoming().map(|next| next.path.clone()),
            Some(PathBuf::from("C:/clap/a.clap"))
        );

        let first = catalog.take_next().expect("first module");
        assert_eq!(
            catalog.upcoming().map(|next| next.path.clone()),
            Some(PathBuf::from("C:/clap/b.clap"))
        );
        catalog.accept(vec![entry(
            PluginFormat::Clap,
            &first.path.to_string_lossy(),
            "a",
            "Alpha",
            true,
            1,
            2,
        )]);
        assert_eq!(catalog.progress(), (1, 2));
        assert_eq!(catalog.entries().len(), 1);

        let _second = catalog.take_next().expect("second module");
        // A module that yields nothing (failed load) still advances progress.
        catalog.accept(Vec::new());
        assert!(!catalog.is_scanning());
        assert_eq!(catalog.progress(), (2, 2));
        assert!(catalog.take_next().is_none());
        assert_eq!(catalog.entries().len(), 1);
        assert_eq!(catalog.to_cache().entries.len(), 1);
        assert_eq!(catalog.to_cache().dirs, vec!["clap:C:/clap".to_string()]);
    }

    #[test]
    fn catalog_from_warm_cache_is_idle() {
        let cache = cache_with(
            &["clap:C:/clap"],
            vec![entry(
                PluginFormat::Clap,
                "C:/clap/x.clap",
                "x",
                "X",
                true,
                100,
                10,
            )],
        );
        let snapshot = DiskScanInput {
            dirs: cache.dirs.clone(),
            candidates: vec![candidate(PluginFormat::Clap, "C:/clap/x.clap", 100, 10)],
        };
        let catalog = PluginCatalog::with_snapshot(&cache, snapshot);
        assert!(!catalog.is_scanning());
        assert_eq!(catalog.progress(), (0, 0));
        assert_eq!(catalog.entries().len(), 1);
        assert!(catalog.upcoming().is_none());
    }
}
