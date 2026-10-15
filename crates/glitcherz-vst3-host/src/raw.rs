//! The only module allowed to use `unsafe`: VST3 dynamic module loading
//! and the COM boundary.
//!
//! # SAFETY (module-level rationale)
//!
//! This module only encapsulates the VST3 module dynamic library loading
//! conventions:
//! - Resolve the module binary path (single file or bundle directory
//!   layout);
//! - After `LoadLibrary`/`dlopen` (via `libloading`), call the exported
//!   symbol `GetPluginFactory` (per the VST3 spec: returns a factory
//!   pointer with a +1 reference count, owned by the host; [`ComPtr`]
//!   RAII takes it over here);
//! - Optionally call `InitDll` (Windows SDK convention).
//!
//! Beyond these boundaries, no manual pointer arithmetic is performed;
//! the soundness of the plugin code itself is the plugin author's
//! responsibility (as with all DAW hosts, the soundness of loaded
//! third-party plugins cannot be proven at compile time). Behavioral
//! verification lives in the `tests` of `lib.rs` (invalid files must
//! return errors rather than UB).
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use libloading::Library;
use vst3::ComPtr;
use vst3::Steinberg::IPluginFactory;

/// Module loading errors.
#[derive(Debug, thiserror::Error)]
pub enum RawError {
    #[error("Path is not a valid VST3 module (neither a file nor a parseable bundle directory)")]
    Layout,
    #[error("Failed to load dynamic library: {0}")]
    Load(String),
    #[error("Missing GetPluginFactory export symbol")]
    MissingFactorySymbol,
    #[error("GetPluginFactory returned a null pointer")]
    NullFactory,
}

/// Platform-specific bundle content directory name (VST3 bundle layout
/// convention).
fn platform_content_dir() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "x86_64-win",
        ("windows", "x86") => "x86-win",
        ("windows", "aarch64") => "arm64-win",
        // macOS bundles use Contents/MacOS directly.
        ("macos", _) => "MacOS",
        ("linux", "x86_64") => "x86_64-linux",
        ("linux", "aarch64") => "aarch64-linux",
        _ => "x86_64-linux",
    }
}

/// Platform-specific module binary file name (inside a bundle).
fn binary_file_name(stem: &str) -> String {
    match std::env::consts::OS {
        "windows" => format!("{stem}.vst3"),
        "linux" => format!("{stem}.so"),
        _ => stem.to_string(),
    }
}

/// Resolves the actual binary path of the VST3 module.
///
/// - Single-file module: returned directly (Windows allows DLLs renamed
///   to `.vst3`);
/// - Bundle directory: `Contents/<platform dir>/<conventional file name>`;
///   when the conventional name is absent, falls back to the first file
///   in the directory (tolerating non-standard naming).
pub(crate) fn module_binary_path(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    let dir = path.join("Contents").join(platform_content_dir());
    let stem = path.file_stem()?.to_str()?;
    let expected = dir.join(binary_file_name(stem));
    if expected.is_file() {
        return Some(expected);
    }
    let entries = std::fs::read_dir(&dir).ok()?;
    entries
        .flatten()
        .map(|entry| entry.path())
        .find(|candidate| candidate.is_file())
}

/// A loaded VST3 module: dynamic library handle + plugin factory.
///
/// **Field order is drop order**: `factory` (holding references to all
/// plugin COM objects) must be dropped before `_library`; otherwise
/// unloading the dynamic library would leave the factory's internal code
/// dangling.
pub(crate) struct Vst3Module {
    pub(crate) factory: ComPtr<IPluginFactory>,
    _library: Library,
}

/// Loads a VST3 module and extracts the plugin factory.
pub(crate) fn load_module(path: &Path) -> Result<Vst3Module, RawError> {
    let binary = module_binary_path(path).ok_or(RawError::Layout)?;
    // SAFETY: See the module documentation. Loading a third-party dynamic
    // library itself is managed by libloading; the entry symbols are
    // called per the VST3 spec, and failure paths return Err rather than
    // UB.
    unsafe {
        let library = Library::new(&binary).map_err(|err| RawError::Load(err.to_string()))?;
        if let Ok(init_dll) = library.get::<unsafe extern "system" fn() -> i32>(b"InitDll") {
            let _ = init_dll();
        }
        let get_factory: libloading::Symbol<unsafe extern "system" fn() -> *mut c_void> = library
            .get(b"GetPluginFactory")
            .map_err(|_| RawError::MissingFactorySymbol)?;
        let raw = get_factory();
        let factory =
            ComPtr::<IPluginFactory>::from_raw(raw.cast()).ok_or(RawError::NullFactory)?;
        Ok(Vst3Module {
            factory,
            _library: library,
        })
    }
}
