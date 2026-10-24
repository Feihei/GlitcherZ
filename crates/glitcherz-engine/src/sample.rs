//! # Non-real-time wav sample loading (U5)
//!
//! Control-thread side of the sample player instrument (spec
//! `.agents/spec/sample-player.md` SP1 / D6 / D7): file IO and hound
//! decoding. **Nothing here may run on the audio callback** — decoding
//! allocates the whole frame buffer, so it is strictly UI/control-thread
//! work, following the same precedent as plugin instantiation and SF2
//! loading. The RT rendering processor lives in
//! [`crate::realtime::sample`] and only receives the already-built
//! `Arc<SampleData>`.
//!
//! [`SampleLibrary`] is the session-level multi-sample cache (D7): unlike
//! the SF2 single library, a sampler uses one wav per instrument, so the
//! cache is keyed by path and shared via `Arc`.

use std::collections::HashMap;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hound::{SampleFormat, WavReader};

/// One decoded stereo frame (interleaved L/R).
pub type StereoFrame = [f32; 2];

/// Error while loading a wav sample. The application layer can just
/// `Display` it.
///
/// hound wraps all underlying IO failures into its own error (file open,
/// truncated stream, decode), so a single `Decode` variant covers them.
#[derive(Debug, thiserror::Error)]
pub enum SampleError {
    #[error("failed to read or decode sample file: {0}")]
    Decode(#[from] hound::Error),
    #[error("sample has no audio frames")]
    Empty,
    #[error("sample has more than 2 channels ({0})")]
    TooManyChannels(u16),
}

/// Frames aggregated into one waveform column (min/max pair) for timeline
/// rendering (U6 D6). At 256 frames a one-minute stereo file yields ~10k
/// columns (~80 KB) — cheap enough to always compute at load time.
pub const PEAKS_RESOLUTION: usize = 256;

/// A wav file decoded to normalized f32 stereo frames (spec D3).
///
/// Mono sources are duplicated into both channels at load time, so the RT
/// reader never branches on channel count. The source sample rate is kept
/// alongside: the playback step factor folds pitch ratio and sample-rate
/// compensation into a single per-engine-frame increment (D3), so no
/// resampling pass ever touches the stored frames.
///
/// Waveform peaks (one min/max pair per [`PEAKS_RESOLUTION`] frames, both
/// channels folded) are computed once at load time for timeline rendering
/// (U6 D6) — the RT path never touches them.
///
/// Cheap to share (`Arc`), expensive to build — decode once per file.
#[derive(Debug)]
pub struct SampleData {
    frames: Box<[StereoFrame]>,
    peaks: Box<[(f32, f32)]>,
    sample_rate: u32,
    source_path: PathBuf,
}

impl SampleData {
    /// Decodes a wav file. **Control thread only** (file IO + full decode,
    /// see D6).
    ///
    /// Accepted inputs: PCM integer (8/16/24/32-bit) or IEEE float
    /// (32-bit) — everything hound can decode via `i32`/`f32` samples —
    /// with 1 or 2 channels. Anything with more channels is rejected
    /// (`TooManyChannels`) rather than silently dropping channels. A
    /// trailing odd sample in a stereo stream (truncated file) is dropped.
    pub fn load(path: &Path) -> Result<Self, SampleError> {
        let mut reader = WavReader::open(path)?;
        let spec = reader.spec();
        if spec.channels > 2 {
            return Err(SampleError::TooManyChannels(spec.channels));
        }

        // hound's `samples::<i32>` handles every integer bit depth with a
        // unified signed range (8-bit wav is unsigned; hound re-centres it),
        // so one normalization divisor covers all of them.
        let interleaved: Vec<f32> = match spec.sample_format {
            SampleFormat::Int => {
                let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|s| s.map(|v| v as f32 / max))
                    .collect::<Result<_, _>>()?
            }
            SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        };

        let frames: Vec<StereoFrame> = if spec.channels == 1 {
            interleaved.into_iter().map(|v| [v, v]).collect()
        } else {
            interleaved
                .chunks_exact(2)
                .map(|pair| [pair[0], pair[1]])
                .collect()
        };

        if frames.is_empty() {
            return Err(SampleError::Empty);
        }

        log::info!(
            "decoded sample '{}' ({} frames, {} ch, {} Hz, {} bit {:?}) from {}",
            path.file_stem().unwrap_or_default().to_string_lossy(),
            frames.len(),
            spec.channels,
            spec.sample_rate,
            spec.bits_per_sample,
            spec.sample_format,
            path.display()
        );

        Ok(Self {
            peaks: compute_peaks(&frames).into_boxed_slice(),
            frames: frames.into_boxed_slice(),
            sample_rate: spec.sample_rate,
            source_path: path.to_path_buf(),
        })
    }

    /// The decoded stereo frames. Hand this to the audio-side processor
    /// (via an `Arc` clone); do not decode on the audio thread.
    pub fn frames(&self) -> &[StereoFrame] {
        &self.frames
    }

    /// Waveform peaks for rendering: one `(min, max)` pair per
    /// [`PEAKS_RESOLUTION`] frames, both channels folded (the pair spans
    /// the louder channel's excursion). The final partial bucket is
    /// included, so `peaks.len() == ceil(frames / PEAKS_RESOLUTION)`.
    pub fn peaks(&self) -> &[(f32, f32)] {
        &self.peaks
    }

    /// Sample rate of the source wav (Hz). The RT reader compensates for a
    /// mismatch against the engine rate inside the step factor (D3).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The path this sample was loaded from (cache identity + document
    /// `VoiceSpec::Sample { path }` field).
    pub fn path(&self) -> &Path {
        &self.source_path
    }
}

/// Load-time waveform aggregation (U6 D6): one min/max pair per
/// [`PEAKS_RESOLUTION`]-frame bucket, folding both channels into the pair
/// that spans the larger excursion.
fn compute_peaks(frames: &[StereoFrame]) -> Vec<(f32, f32)> {
    let mut peaks = Vec::with_capacity(frames.len().div_ceil(PEAKS_RESOLUTION));
    for bucket in frames.chunks(PEAKS_RESOLUTION) {
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for &[l, r] in bucket {
            // Fold both channels: the waveform column should show the
            // loudest excursion of either channel.
            let lo = l.min(r);
            let hi = l.max(r);
            min = min.min(lo);
            max = max.max(hi);
        }
        peaks.push((min, max));
    }
    peaks
}

/// Session-level multi-sample cache (spec D7): path → decoded
/// `Arc<SampleData>`.
///
/// Load misses go through [`SampleLibrary::load`], which decodes once and
/// caches; subsequent requests for the same path (node reinstall, undo/redo
/// graph rebuild, document reopen) reuse the decoded buffer instead of
/// re-reading the file. v1 has no eviction policy — samples live for the
/// session.
#[derive(Debug, Default)]
pub struct SampleLibrary {
    entries: HashMap<PathBuf, Arc<SampleData>>,
}

impl SampleLibrary {
    /// Creates an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads `path`, or returns the cached decode if this path was already
    /// loaded this session. **Control thread only** (decodes on miss).
    pub fn load(&mut self, path: &Path) -> Result<Arc<SampleData>, SampleError> {
        if let Some(data) = self.entries.get(path) {
            return Ok(Arc::clone(data));
        }
        let data = Arc::new(SampleData::load(path)?);
        self.entries.insert(path.to_path_buf(), Arc::clone(&data));
        Ok(data)
    }

    /// Cached decode of `path`, if it was loaded this session (U6: the
    /// timeline's waveform lookup — read-only, never decodes). RT-irrelevant
    /// (control thread only), but deliberately non-mutating so it can be
    /// shared behind `&` in view closures.
    pub fn get(&self, path: &Path) -> Option<Arc<SampleData>> {
        self.entries.get(path).cloned()
    }

    /// Number of cached samples (diagnostics / tests).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{SampleData, SampleError, SampleLibrary};

    /// Writes a small wav fixture with hound and returns its path. The file
    /// lands in the system temp dir; callers get a unique name per test.
    fn write_wav_fixture(name: &str, sample_rate: u32, channels: u16, samples: &[f32]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("glitcherz-sample-{name}.wav"));
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create fixture");
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            writer.write_sample(v).expect("write fixture");
        }
        writer.finalize().expect("finalize fixture");
        path
    }

    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32 / n as f32).collect()
    }

    #[test]
    fn loads_stereo_fixture_with_expected_frames() {
        let samples = ramp(8);
        let path = write_wav_fixture("stereo", 48_000, 2, &samples);
        let data = SampleData::load(&path).expect("decode");
        assert_eq!(data.frames().len(), 4, "8 interleaved samples / 2 ch");
        assert_eq!(data.sample_rate(), 48_000);
        assert_eq!(data.path(), path.as_path());
        // Interleaved: frame i = [samples[2i], samples[2i + 1]].
        assert!((data.frames()[0][0] - 0.0).abs() < 1e-3);
        assert!((data.frames()[0][1] - 1.0 / 8.0).abs() < 1e-3);
        assert!((data.frames()[3][0] - 6.0 / 8.0).abs() < 1e-3);
        assert!((data.frames()[3][1] - 7.0 / 8.0).abs() < 1e-3);
    }

    #[test]
    fn mono_fixture_is_duplicated_to_both_channels() {
        let samples = ramp(6);
        let path = write_wav_fixture("mono", 44_100, 1, &samples);
        let data = SampleData::load(&path).expect("decode");
        assert_eq!(data.frames().len(), 6);
        for frame in data.frames() {
            assert_eq!(frame[0], frame[1], "mono duplicated L/R");
        }
        assert_eq!(data.sample_rate(), 44_100);
    }

    #[test]
    fn library_caches_by_path() {
        let path = write_wav_fixture("cache", 48_000, 1, &ramp(4));
        let mut lib = SampleLibrary::new();
        assert!(lib.is_empty());
        let first = lib.load(&path).expect("first load");
        assert_eq!(lib.len(), 1);
        let second = lib.load(&path).expect("cached load");
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "same path must reuse the decoded buffer"
        );
        assert_eq!(lib.len(), 1, "no duplicate entry");
    }

    #[test]
    fn rejects_more_than_two_channels() {
        let path = write_wav_fixture("surround", 48_000, 3, &ramp(6));
        match SampleData::load(&path) {
            Err(SampleError::TooManyChannels(3)) => {}
            other => panic!("expected TooManyChannels(3), got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_file_as_decode_error() {
        let path = std::env::temp_dir().join("glitcherz-sample-does-not-exist.wav");
        match SampleData::load(&path) {
            Err(SampleError::Decode(_)) => {}
            other => panic!("expected Decode error, got {other:?}"),
        }
    }
}
