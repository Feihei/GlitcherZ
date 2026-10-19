//! # Non-real-time SoundFont loading (SF2)
//!
//! Control-thread side of the SF2 sound source (spec
//! `.agents/spec/sf2-loader.md` S2 / D2 / D3): file IO, parsing and preset
//! enumeration. **Nothing here may run on the audio callback** —
//! `SoundFont::new` parses the whole RIFF tree and decodes sample data, so
//! it is strictly UI/control-thread work, following the same precedent as
//! plugin instantiation. The RT rendering processor lives in
//! [`crate::realtime`] (S3) and only receives the already-built
//! `Arc<SoundFont>`.
//!
//! S1 audit verdict (spec §7): `rustysynth-ext` 1.4.0 render paths are
//! allocation-free; loading is the only heavy part and stays here.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustysynth_ext::SoundFont;

/// Error while loading a SoundFont. The application layer can just
/// `Display` it.
#[derive(Debug, thiserror::Error)]
pub enum Sf2Error {
    #[error("failed to read sound font file: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid sound font: {0}")]
    Parse(#[from] rustysynth_ext::SoundFontError),
    #[error("sound font contains no presets addressable by bank/program (u8)")]
    NoPresets,
}

/// One preset of a loaded SoundFont, addressed as `bank:program` (D7).
///
/// Presets whose bank or program number does not fit into `u8` are skipped
/// during enumeration (tracked in the S1–S6 spec; no real-world GM font is
/// known to exceed it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sf2Preset {
    pub bank: u8,
    pub program: u8,
    pub name: String,
}

/// A SoundFont loaded and parsed on the control thread (D3).
///
/// Owns the shared [`SoundFont`] (cheap `Arc` clones for the audio-side
/// processor in S3) plus the preset list enumerated once at load time for
/// UI dropdowns. Cheap to share, expensive to build — load once per file.
#[derive(Debug)]
pub struct Sf2Library {
    font: Arc<SoundFont>,
    source_path: PathBuf,
    presets: Vec<Sf2Preset>,
}

impl Sf2Library {
    /// Loads and parses a SoundFont file. **Control thread only** (file IO
    /// + full RIFF parse + sample decode; see D3).
    pub fn load(path: &Path) -> Result<Self, Sf2Error> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let font = Arc::new(SoundFont::new(&mut reader)?);

        let presets = enumerate_presets(&font);
        if presets.is_empty() {
            return Err(Sf2Error::NoPresets);
        }

        log::info!(
            "loaded sound font '{}' ({} presets) from {}",
            font.get_info().get_bank_name(),
            presets.len(),
            path.display()
        );

        Ok(Self {
            font,
            source_path: path.to_path_buf(),
            presets,
        })
    }

    /// The shared, parsed sound font. Hand this (an `Arc` clone) to the
    /// audio-side processor (S3); do not parse on the audio thread.
    pub fn font(&self) -> &Arc<SoundFont> {
        &self.font
    }

    /// The path this library was loaded from (for diagnostics and for the
    /// document's `VoiceSpec::SoundFont { path }` field, S4).
    pub fn path(&self) -> &Path {
        &self.source_path
    }

    /// All presets, in sound font file order (stable across loads of the
    /// same file).
    pub fn presets(&self) -> &[Sf2Preset] {
        &self.presets
    }

    /// Looks up a preset by its `bank:program` address (D7 track-level
    /// addressing).
    pub fn find_preset(&self, bank: u8, program: u8) -> Option<&Sf2Preset> {
        self.presets
            .iter()
            .find(|p| p.bank == bank && p.program == program)
    }
}

fn enumerate_presets(font: &SoundFont) -> Vec<Sf2Preset> {
    let mut presets = Vec::new();
    for preset in font.get_presets() {
        let (Ok(bank), Ok(program)) = (
            u8::try_from(preset.get_bank_number()),
            u8::try_from(preset.get_patch_number()),
        ) else {
            // D7 addresses presets as u8:u8; out-of-range entries (never
            // seen in practice) cannot be selected from a track.
            log::warn!(
                "skipping preset '{}' with out-of-range address {}:{}",
                preset.get_name(),
                preset.get_bank_number(),
                preset.get_patch_number()
            );
            continue;
        };
        presets.push(Sf2Preset {
            bank,
            program,
            name: preset.get_name().to_owned(),
        });
    }
    presets
}
