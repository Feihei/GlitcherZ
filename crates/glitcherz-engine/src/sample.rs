//! # Non-real-time audio sample loading (U5 / X series)
//!
//! Control-thread side of the sample player instrument (spec
//! `.agents/spec/sample-player.md` SP1 / D6 / D7, extended by
//! `.agents/spec/audio-decoding.md`): file IO and decoding. **Nothing here may
//! run on the audio callback** — decoding allocates the whole frame buffer, so
//! it is strictly UI/control-thread work, following the same precedent as
//! plugin instantiation and SF2 loading. The RT rendering processor lives in
//! [`crate::realtime::sample`] and only receives the already-built
//! `Arc<SampleData>`.
//!
//! WAV keeps the original `hound` path byte-for-byte (X series D2). All other
//! containers go through `symphonia`, which decodes on the same control thread
//! into the exact same interleaved f32 stereo layout.
//!
//! [`SampleLibrary`] is the session-level multi-sample cache (D7): unlike
//! the SF2 single library, a sampler uses one file per instrument, so the
//! cache is keyed by path and shared via `Arc`.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hound::{SampleFormat, WavReader};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Extensions offered by the sample file pickers and understood by
/// [`SampleData::load`] (spec `audio-decoding.md` D8). Single source of truth:
/// the UI reads this constant instead of hard-coding `["wav"]`.
///
/// `.wav` is decoded by `hound`; everything else is probed and decoded by
/// `symphonia`. Ogg here means Vorbis only — Symphonia 0.6 has no Opus decoder.
pub const AUDIO_FILE_EXTENSIONS: &[&str] = &["wav", "flac", "mp3", "ogg", "m4a", "aiff"];

/// One decoded stereo frame (interleaved L/R).
pub type StereoFrame = [f32; 2];

/// Error while loading a sample file. The application layer can just
/// `Display` it.
///
/// The two decoders have different error types, so `Decode` stores the
/// formatted message instead of a `#[from]` conversion.
#[derive(Debug, thiserror::Error)]
pub enum SampleError {
    #[error("failed to read or decode sample file: {0}")]
    Decode(String),
    #[error("unsupported audio format: {0}")]
    UnsupportedFormat(String),
    #[error("sample has no audio frames")]
    Empty,
    #[error("sample has more than 2 channels ({0})")]
    TooManyChannels(u16),
}

/// Frames aggregated into one waveform column (min/max pair) for timeline
/// rendering (U6 D6). At 256 frames a one-minute stereo file yields ~10k
/// columns (~80 KB) — cheap enough to always compute at load time.
pub const PEAKS_RESOLUTION: usize = 256;

/// Common result of both decode paths, before `peaks` and `SampleData` are
/// built. Keeping the decoder name lets the load log say which path ran.
#[derive(Debug)]
struct DecodedAudio {
    frames: Vec<StereoFrame>,
    sample_rate: u32,
    channels: u16,
    decoder: &'static str,
}

/// An audio file decoded to normalized f32 stereo frames (spec D3).
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
    /// Decodes an audio file. **Control thread only** (file IO + full decode,
    /// see D6 / `audio-decoding.md` D10).
    ///
    /// `.wav` goes to [`decode_wav`] so existing projects are bit-identical;
    /// every other extension (including a missing or wrong one) is handed to
    /// [`decode_symphonia`], which probes by content. Mono is duplicated to
    /// L/R, >2 channels are rejected, and a trailing odd sample in a stereo
    /// stream is dropped.
    pub fn load(path: &Path) -> Result<Self, SampleError> {
        let is_wav = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"));
        let decoded = if is_wav {
            decode_wav(path)?
        } else {
            decode_symphonia(path)?
        };

        if decoded.frames.is_empty() {
            return Err(SampleError::Empty);
        }

        log::info!(
            "decoded sample '{}' ({} frames, {} ch, {} Hz, via {}) from {}",
            path.file_stem().unwrap_or_default().to_string_lossy(),
            decoded.frames.len(),
            decoded.channels,
            decoded.sample_rate,
            decoded.decoder,
            path.display()
        );

        Ok(Self {
            peaks: compute_peaks(&decoded.frames).into_boxed_slice(),
            frames: decoded.frames.into_boxed_slice(),
            sample_rate: decoded.sample_rate,
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

    /// Sample rate of the source file (Hz). The RT reader compensates for a
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

/// Original WAV path, kept unchanged for zero regression (spec D2 / D3).
/// Accepts PCM integer (8/16/24/32-bit) or IEEE float (32-bit) with 1 or 2
/// channels; anything wider is rejected rather than silently dropped.
fn decode_wav(path: &Path) -> Result<DecodedAudio, SampleError> {
    let mut reader = WavReader::open(path).map_err(|err| SampleError::Decode(err.to_string()))?;
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
                .map(|s| {
                    s.map(|v| v as f32 / max)
                        .map_err(|err| SampleError::Decode(err.to_string()))
                })
                .collect::<Result<_, _>>()?
        }
        SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.map_err(|err| SampleError::Decode(err.to_string())))
            .collect::<Result<_, _>>()?,
    };

    Ok(DecodedAudio {
        frames: interleave_frames(&interleaved, spec.channels as usize),
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        decoder: "hound",
    })
}

/// Turns decoder output (interleaved f32, 1 or 2 channels) into stereo
/// frames. A trailing incomplete frame is dropped, matching the old hound path.
fn interleave_frames(interleaved: &[f32], channels: usize) -> Vec<StereoFrame> {
    match channels {
        1 => interleaved.iter().map(|&v| [v, v]).collect(),
        _ => interleaved
            .chunks_exact(2)
            .map(|pair| [pair[0], pair[1]])
            .collect(),
    }
}

/// Decodes any Symphonia-supported container (FLAC / MP3 / Ogg Vorbis /
/// ISO-MP4 AAC-LC / AIFF / WAV) on the control thread.
///
/// `GenericAudioBufferRef::copy_to_vec_interleaved` normalizes every integer
/// sample format to f32 and **resizes** its destination Vec, so one reusable
/// scratch Vec feeds `interleave_frames` without a per-packet allocation.
fn decode_symphonia(path: &Path) -> Result<DecodedAudio, SampleError> {
    let file = File::open(path).map_err(|err| SampleError::Decode(err.to_string()))?;
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(ext);
    }
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|err| SampleError::UnsupportedFormat(err.to_string()))?;

    // Clone the parameters so the immutable borrow of `format` ends before
    // the packet loop starts mutating it.
    let (track_id, params) = {
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| SampleError::UnsupportedFormat("no audio track".into()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .cloned()
            .ok_or_else(|| {
                SampleError::UnsupportedFormat("audio track has no codec parameters".into())
            })?;
        (track.id, params)
    };
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|err| SampleError::UnsupportedFormat(err.to_string()))?;

    let mut frames: Vec<StereoFrame> = Vec::new();
    let mut scratch: Vec<f32> = Vec::new();
    let mut sample_rate = 0u32;
    let mut channels = 0usize;

    while let Some(packet) = format
        .next_packet()
        .map_err(|err| SampleError::Decode(err.to_string()))?
    {
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A single corrupt packet is recoverable; everything else is not.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(err) => return Err(SampleError::Decode(err.to_string())),
        };
        let spec = decoded.spec();
        let rate = spec.rate();
        let ch = spec.channels().count();
        if ch == 0 || decoded.frames() == 0 {
            continue;
        }
        if ch > 2 {
            return Err(SampleError::TooManyChannels(
                ch.min(u16::MAX as usize) as u16
            ));
        }
        if sample_rate == 0 {
            sample_rate = rate;
            channels = ch;
        } else if rate != sample_rate {
            return Err(SampleError::Decode("sample rate changed mid-stream".into()));
        }
        if ch != channels {
            return Err(SampleError::Decode(
                "channel count changed mid-stream".into(),
            ));
        }

        decoded.copy_to_vec_interleaved::<f32>(&mut scratch);
        frames.extend(interleave_frames(&scratch, ch));
    }

    Ok(DecodedAudio {
        frames,
        sample_rate,
        channels: channels as u16,
        decoder: "symphonia",
    })
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

    use super::{SampleData, SampleError, SampleLibrary, decode_symphonia, decode_wav};

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

    /// X series D13: both paths normalize i16 by /32768, so the same WAV
    /// fixture must decode to bit-identical frames.
    #[test]
    fn symphonia_path_matches_hound_for_the_same_wav() {
        let path = write_wav_fixture("parity", 48_000, 2, &ramp(16));
        let hound = decode_wav(&path).expect("hound decode");
        let symphonia = decode_symphonia(&path).expect("symphonia decode");
        assert_eq!(hound.sample_rate, symphonia.sample_rate);
        assert_eq!(hound.channels, symphonia.channels);
        assert_eq!(hound.frames, symphonia.frames);
    }

    #[test]
    fn symphonia_path_duplicates_mono() {
        let path = write_wav_fixture("symphonia-mono", 22_050, 1, &ramp(6));
        let decoded = decode_symphonia(&path).expect("symphonia decode");
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.sample_rate, 22_050);
        assert_eq!(decoded.frames.len(), 6);
        for frame in &decoded.frames {
            assert_eq!(frame[0], frame[1]);
        }
    }

    #[test]
    fn symphonia_path_rejects_more_than_two_channels() {
        let path = write_wav_fixture("symphonia-surround", 48_000, 3, &ramp(6));
        match decode_symphonia(&path) {
            Err(SampleError::TooManyChannels(3)) => {}
            other => panic!("expected TooManyChannels(3), got {other:?}"),
        }
    }

    /// Content probing still works when the extension is unknown, so a
    /// renamed file is not silently rejected (spec D2).
    #[test]
    fn unknown_extension_is_probed_by_content() {
        let source = write_wav_fixture("probe-src", 48_000, 2, &ramp(8));
        let path = std::env::temp_dir().join("glitcherz-sample-renamed.bin");
        std::fs::copy(&source, &path).expect("copy fixture");
        let data = SampleData::load(&path).expect("content-probed decode");
        assert_eq!(data.frames().len(), 4);
        assert_eq!(data.sample_rate(), 48_000);
    }

    #[test]
    fn garbage_bytes_are_rejected_without_panicking() {
        let path = std::env::temp_dir().join("glitcherz-sample-garbage.flac");
        std::fs::write(&path, b"not an audio file").expect("write fixture");
        match SampleData::load(&path) {
            Err(SampleError::UnsupportedFormat(_)) | Err(SampleError::Decode(_)) => {}
            other => panic!("expected a decode/probe error, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }
}
