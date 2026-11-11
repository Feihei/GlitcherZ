//! Offline audio export (U7, spec `.agents/spec/export-audio.md`; S3c adds
//! bit depth, sample-rate choice and per-track stems).
//!
//! Renders the project to wav without the real-time stream: an offline
//! [`AudioEngine`] drives the same block path the cpal callback takes, while a
//! `hound` writer consumes the interleaved output. The job is **sliced across
//! frames** (D1, the U9 plugin-scan precedent): the app pumps a handful of
//! blocks at the end of each UI frame, so long exports neither freeze the
//! interface nor block cancellation.
//!
//! Snapshot semantics (D6): everything the render uses — clip tables, plugin
//! instances with their state, parameters — is copied into the offline engine
//! when the job starts; document edits during the export do not affect the
//! running render.
//!
//! Stems (S3c D3): one job can render several **passes** over the same
//! assembled engine — the mix first, then one file per audible track. A new
//! pass swaps the clip tables (via the caller's setup callback) instead of
//! re-instantiating the graph, so a Dexed-sized session pays plugin startup
//! once. Between passes the job renders a few seconds of silence and throws it
//! away, so the previous pass's reverb/delay tails cannot leak into the next
//! file's head.

use std::collections::HashMap;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use glitcherz_engine::AudioEngine;
use glitcherz_engine::realtime::{EngineCommand, SequencerCommand};
use glitcherz_model::Project;

use crate::PluginGuard;

/// Blocks rendered per UI frame (~90 ms of audio at 48 kHz / 128 frames —
/// smooth progress, well under one frame budget even on slow machines).
pub const BLOCKS_PER_FRAME: usize = 34;

/// Tail rendered past the end point: one second, so delays and releases
/// ring out into the file.
pub const TAIL_SECONDS: f64 = 1.0;

/// Silence rendered between passes (S3c D3). Two seconds is enough for a
/// reverb tail to fall under the 16-bit noise floor, i.e. the next stem starts
/// from clean state instead of the previous track's decay.
pub const FLUSH_SECONDS: f64 = 2.0;

/// Which span of the arrangement to render (D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportRange {
    /// `[0, arrangement end)` — the whole timeline content, song loop
    /// ignored (clips keep their own loop tiling; that is content).
    FullSong,
    /// `[loop start, loop end)` — the enabled song-loop span as set in the
    /// ruler.
    SongLoop,
}

impl ExportRange {
    /// Resolves the `[start, end)` tick window (f64 transport domain).
    /// `None` when `SongLoop` is requested but no valid loop is enabled.
    pub fn window(self, project: &Project) -> Option<(f64, f64)> {
        match self {
            ExportRange::FullSong => {
                let end = export_end_ticks(project);
                Some((0.0, f64::from(end)))
            }
            ExportRange::SongLoop => {
                let loop_range = &project.metadata.loop_range;
                if loop_range.enabled
                    && loop_range.end_ticks > loop_range.start_ticks
                    && loop_range.end_ticks > 0
                {
                    Some((
                        f64::from(loop_range.start_ticks),
                        f64::from(loop_range.end_ticks),
                    ))
                } else {
                    None
                }
            }
        }
    }
}

/// Sample encoding of the written file (S3c D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bits {
    /// 16-bit PCM: the smallest, most compatible file.
    Int16,
    /// 24-bit PCM (the default): the usual delivery depth.
    Int24,
    /// 32-bit float: what the engine actually computes, no clamping to the
    /// PCM range before the file.
    Float32,
}

impl Bits {
    /// Every choice, in menu order (16 → 24 → 32f).
    pub const ALL: [Bits; 3] = [Bits::Int16, Bits::Int24, Bits::Float32];

    /// Menu label.
    pub fn label(self) -> &'static str {
        match self {
            Bits::Int16 => "16-bit",
            Bits::Int24 => "24-bit",
            Bits::Float32 => "32-bit float",
        }
    }

    fn wav_spec(self, sample_rate: u32) -> hound::WavSpec {
        match self {
            Bits::Int16 => hound::WavSpec {
                channels: 2,
                sample_rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
            Bits::Int24 => hound::WavSpec {
                channels: 2,
                sample_rate,
                bits_per_sample: 24,
                sample_format: hound::SampleFormat::Int,
            },
            Bits::Float32 => hound::WavSpec {
                channels: 2,
                sample_rate,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        }
    }
}

/// What the file says it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavFormat {
    pub sample_rate: u32,
    pub bits: Bits,
}

impl WavFormat {
    /// Defaults for a device running at `sample_rate`: 24-bit, since that is
    /// what a mix gets delivered in.
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            bits: Bits::Int24,
        }
    }
}

/// Sample rate choices offered by the dialog. The device rate is always
/// included (first) even when it is not one of the standard ones.
pub fn sample_rate_choices(device_rate: u32) -> Vec<u32> {
    let mut rates = vec![device_rate];
    for rate in [44_100, 48_000, 88_200, 96_000] {
        if !rates.contains(&rate) {
            rates.push(rate);
        }
    }
    rates
}

/// Which track a stem pass renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackSel {
    Midi(usize),
    Audio(usize),
}

/// One file the job writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPass {
    pub path: PathBuf,
    /// Banner label ("Mix", "Bass", "Drums").
    pub label: String,
    /// The track this pass isolates; `None` = the whole arrangement.
    pub target: Option<TrackSel>,
}

impl ExportPass {
    pub fn mix(path: PathBuf) -> Self {
        Self {
            path,
            label: "Mix".to_string(),
            target: None,
        }
    }

    pub fn stem(path: PathBuf, label: String, target: TrackSel) -> Self {
        Self {
            path,
            label,
            target: Some(target),
        }
    }
}

/// `parent/stem_suffix.wav` next to `mix` — the stem files land beside the
/// mix, never in a different directory than the one the user picked.
pub fn stem_path(mix: &Path, suffix: &str) -> PathBuf {
    let stem = mix
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "Untitled".to_string());
    let name = format!("{stem}_{}.wav", sanitize_file_part(suffix));
    mix.with_file_name(name)
}

/// Keeps a track name usable as a file name on all three platforms: path
/// separators, the Windows-reserved characters and control codes become `_`,
/// runs of whitespace collapse, and an empty result falls back to `track`.
pub fn sanitize_file_part(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_space = false;
    for ch in name.chars() {
        let bad =
            matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || ch.is_control();
        let ch = if bad { '_' } else { ch };
        if ch == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(ch);
    }
    // A trailing dot or space is illegal on Windows and gets silently
    // stripped by the OS, which would break the path we just wrote.
    let trimmed = out.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        "track".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Latest clip end across **all** track kinds — MIDI, control and audio
/// (U7 D5: the sequencer's arrangement end only knows MIDI/control
/// content; an audio clip ending later would be cut by the auto-stop).
/// Rounded up to a whole bar, `0` for an empty arrangement.
pub fn export_end_ticks(project: &Project) -> u32 {
    let mut latest = 0u64;
    for track in &project.midi_tracks {
        for clip in &track.clips {
            latest = latest.max(u64::from(clip.start_ticks) + u64::from(clip.span_ticks));
        }
    }
    for track in &project.control_tracks {
        for clip in &track.clips {
            latest = latest.max(u64::from(clip.start_ticks) + u64::from(clip.span_ticks));
        }
    }
    for track in &project.audio_tracks {
        for clip in &track.clips {
            // U11: the tick footprint is derived from the seconds window.
            latest = latest
                .max(u64::from(clip.start_ticks) + u64::from(project.audio_clip_span_ticks(clip)));
        }
    }
    // Met M4: ceil to the meter segment's next bar line (a clip already on
    // a line keeps that line).
    if latest == 0 {
        0
    } else {
        let tick = (latest - 1).min(u64::from(u32::MAX)) as u32;
        project.meter_map().next_bar(tick)
    }
}

/// Callback the job runs when a pass starts: the caller swaps the clip tables
/// for `target` (the graph itself stays assembled). The doc→engine node map is
/// the one the assembly built — audio clip tables are addressed by engine id.
pub type PassSetup<'a> = &'a mut dyn FnMut(&mut AudioEngine, &HashMap<u64, u64>, Option<TrackSel>);

/// One running export. The app holds `Option<ExportJob>`; dropping it
/// (cancel or finish) releases the offline engine and — with it — the
/// plugin guards, unloading the offline plugin instances.
pub struct ExportJob {
    pub engine: AudioEngine,
    /// Offline plugin guards (D4): collected during assembly, dropped with
    /// the job — never mixed into the live engine's guard list. Never
    /// *read*: holding them until the job ends is the whole point (the
    /// dynamic libraries stay loaded while their nodes render).
    #[allow(dead_code)]
    pub(crate) guards: Vec<PluginGuard>,
    /// Doc node id → engine node id of the assembled graph (S3c: stem passes
    /// re-address the audio clip tables through it).
    pub(crate) node_map: HashMap<u64, u64>,
    writer: Option<hound::WavWriter<BufWriter<std::fs::File>>>,
    /// One interleaved stereo block.
    block: Vec<f32>,
    format: WavFormat,
    window: (f64, f64),
    passes: Vec<ExportPass>,
    index: usize,
    /// Tail blocks left in the current pass (`0` while the transport has not
    /// reached the end).
    tail_left: usize,
    /// Discarded blocks left between passes (S3c D3).
    flush_left: usize,
    /// The current pass reached its end and still has to be closed.
    finishing: bool,
    /// Set by the Cancel button; the next pump drops the job.
    pub cancelled: bool,
    /// Files completed so far.
    outputs: Vec<PathBuf>,
}

impl ExportJob {
    /// Assembles the writer half (the engine half is the app's assembly
    /// job — it needs the session caches, which live on `GlitcherZApp`).
    ///
    /// `passes` is never empty in practice (the mix is always first); an empty
    /// list finishes immediately with no output. The first pass's writer is
    /// opened here, the rest as the job advances.
    pub(crate) fn new(
        engine: AudioEngine,
        guards: Vec<PluginGuard>,
        node_map: HashMap<u64, u64>,
        format: WavFormat,
        window: (f64, f64),
        passes: Vec<ExportPass>,
    ) -> hound::Result<Self> {
        let writer = match passes.first() {
            Some(pass) => Some(open_writer(pass, format)?),
            None => None,
        };
        Ok(Self {
            engine,
            guards,
            node_map,
            writer,
            block: vec![0.0f32; glitcherz_engine::realtime::FIXED_BLOCK_SIZE * 2],
            format,
            window,
            passes,
            index: 0,
            tail_left: tail_blocks(format.sample_rate),
            flush_left: 0,
            finishing: false,
            cancelled: false,
            outputs: Vec::new(),
        })
    }

    /// The file currently being written (status messages).
    pub fn path(&self) -> &Path {
        self.passes
            .get(self.index)
            .map_or_else(|| Path::new(""), |pass| pass.path.as_path())
    }

    /// Banner label of the current pass.
    pub fn label(&self) -> &str {
        self.passes
            .get(self.index)
            .map_or("", |pass| pass.label.as_str())
    }

    /// `(current, total)` pass numbers, 1-based for display.
    pub fn pass_number(&self) -> (usize, usize) {
        (self.index + 1, self.passes.len())
    }

    /// Files already written (finished passes).
    pub fn outputs(&self) -> &[PathBuf] {
        &self.outputs
    }

    /// The format the files are written in.
    pub fn format(&self) -> WavFormat {
        self.format
    }

    /// Render progress in `0..=1` across **all** passes (ticks inside the
    /// window plus the tail; finished passes count as 1).
    pub fn progress(&self) -> f32 {
        let total = self.passes.len().max(1) as f32;
        let span = (self.window.1 - self.window.0).max(1.0);
        let ticks = f64::from(self.engine.musical_ticks());
        let inner = ((ticks - self.window.0) / span).clamp(0.0, 1.0) as f32;
        ((self.index as f32) + inner) / total
    }

    /// Renders up to `blocks` blocks. Returns `true` when **every** pass is
    /// finished (end + tail reached) or the job was cancelled — the caller
    /// drops the job and reports.
    ///
    /// `setup` runs at the start of every pass but the first; the assembly
    /// already pushed the mix's clip tables.
    pub fn pump(&mut self, blocks: usize, setup: PassSetup<'_>) -> bool {
        for _ in 0..blocks {
            if self.cancelled {
                return true;
            }
            // Between passes: throw away the previous pass's effect tails.
            if self.flush_left > 0 {
                if !self.engine.render_offline_block(&mut self.block) {
                    return true; // not an offline engine — bail out
                }
                self.flush_left -= 1;
                continue;
            }
            // The pass reached its end: finalize it, then either move on or
            // report the whole job finished.
            if self.finishing {
                self.finishing = false;
                if !self.close_pass() {
                    return true; // disk failure — stop the export
                }
                if self.index + 1 >= self.passes.len() {
                    return true;
                }
                self.index += 1;
                self.flush_left = flush_blocks(self.format.sample_rate);
                continue;
            }
            if self.pass_finished() {
                self.finishing = true;
                continue;
            }
            // First block after a flush: open the next writer and let the
            // caller re-address the clip tables before anything renders.
            if self.writer.is_none() && !self.begin_pass(setup) {
                return true;
            }
            // Park the transport at the window end: the tail is a static
            // render of whatever the effects are still ringing (the
            // sequencer's own auto-stop lands here too — the floor the
            // assembly set keeps it from firing early).
            if self.engine.is_playing() && f64::from(self.engine.musical_ticks()) >= self.window.1 {
                let _ = self.engine.pause();
            }
            if !self.engine.render_offline_block(&mut self.block) {
                return true; // not an offline engine — bail out
            }
            if !self.write_block() {
                return true; // disk failure — stop the export
            }
            if !self.engine.is_playing() {
                self.tail_left = self.tail_left.saturating_sub(1);
            }
        }
        false
    }

    /// The current pass wrote its tail out. A pass without an open writer has
    /// not started yet — treating that as "finished" would skip it silently.
    fn pass_finished(&self) -> bool {
        self.writer.is_some() && !self.engine.is_playing() && self.tail_left == 0
    }

    /// Finalizes the current pass's wav file and records it.
    fn close_pass(&mut self) -> bool {
        let Some(writer) = self.writer.take() else {
            return true;
        };
        match writer.finalize() {
            Ok(()) => {
                if let Some(pass) = self.passes.get(self.index) {
                    self.outputs.push(pass.path.clone());
                }
                true
            }
            Err(err) => {
                log::error!("Export: finalizing {} failed: {err}", self.path().display());
                false
            }
        }
    }

    /// Opens the writer for `self.index`, hands the clip tables back to the
    /// caller, and restarts the transport at the window start.
    fn begin_pass(&mut self, setup: PassSetup<'_>) -> bool {
        let Some(pass) = self.passes.get(self.index) else {
            return false;
        };
        match open_writer(pass, self.format) {
            Ok(writer) => self.writer = Some(writer),
            Err(err) => {
                log::error!("Export: creating {} failed: {err}", pass.path.display());
                return false;
            }
        }
        let target = pass.target;
        setup(&mut self.engine, &self.node_map, target);
        self.engine.seek_ticks(self.window.0);
        let _ = self
            .engine
            .send(EngineCommand::Sequencer(SequencerCommand::Play));
        self.tail_left = tail_blocks(self.format.sample_rate);
        true
    }

    /// Encodes one interleaved stereo block into the writer. `false` on a
    /// write error (disk full / removed).
    fn write_block(&mut self) -> bool {
        let Some(writer) = self.writer.as_mut() else {
            return false;
        };
        match self.format.bits {
            Bits::Int16 => {
                for &sample in &self.block {
                    let sample = finite_or_silence(sample);
                    if writer
                        .write_sample((sample * f32::from(i16::MAX)) as i16)
                        .is_err()
                    {
                        return false;
                    }
                }
            }
            Bits::Int24 => {
                // hound writes three bytes for a 24-bit spec; the value is
                // carried in an i32 scaled to 24-bit full scale.
                const FULL_SCALE: f32 = 8_388_607.0;
                for &sample in &self.block {
                    let sample = finite_or_silence(sample);
                    if writer.write_sample((sample * FULL_SCALE) as i32).is_err() {
                        return false;
                    }
                }
            }
            Bits::Float32 => {
                for &sample in &self.block {
                    // Floats keep the engine's own overshoot: only NaN/Inf are
                    // replaced, everything else is written as computed.
                    let sample = if sample.is_finite() { sample } else { 0.0 };
                    if writer.write_sample(sample).is_err() {
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// A sample clamped into `-1..=1`, with non-finite values written as silence
/// (a NaN in the buffer must not become a click).
fn finite_or_silence(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn open_writer(
    pass: &ExportPass,
    format: WavFormat,
) -> hound::Result<hound::WavWriter<BufWriter<std::fs::File>>> {
    hound::WavWriter::create(&pass.path, format.bits.wav_spec(format.sample_rate))
}

/// Tail length in blocks: one second past the end point.
fn tail_blocks(sample_rate: u32) -> usize {
    seconds_to_blocks(TAIL_SECONDS, sample_rate)
}

/// Inter-pass silence in blocks.
fn flush_blocks(sample_rate: u32) -> usize {
    seconds_to_blocks(FLUSH_SECONDS, sample_rate)
}

fn seconds_to_blocks(seconds: f64, sample_rate: u32) -> usize {
    let sr = sample_rate.max(1) as f64;
    (seconds * sr / glitcherz_engine::realtime::FIXED_BLOCK_SIZE as f64).ceil() as usize
}

#[cfg(test)]
mod tests {
    use glitcherz_model::{
        AudioClip, AudioTrack, ControlClip, ControlTrack, MidiClip, MidiTrack, Project, SongLoop,
        TICKS_PER_BAR,
    };

    use super::{
        Bits, ExportJob, ExportPass, ExportRange, TrackSel, WavFormat, export_end_ticks,
        sample_rate_choices, sanitize_file_part, stem_path,
    };

    fn project_with(midi_ends: &[u32], control_ends: &[u32], audio_ends: &[u32]) -> Project {
        let mut project = Project {
            midi_tracks: vec![MidiTrack::default()],
            ..Project::default()
        };
        for &end in midi_ends {
            project.midi_tracks[0].clips.push(MidiClip {
                start_ticks: end.saturating_sub(TICKS_PER_BAR),
                span_ticks: TICKS_PER_BAR,
                ..MidiClip::default()
            });
        }
        project.control_tracks = vec![ControlTrack::default()];
        for &end in control_ends {
            project.control_tracks[0].clips.push(ControlClip {
                start_ticks: end.saturating_sub(TICKS_PER_BAR),
                span_ticks: TICKS_PER_BAR,
                ..ControlClip::default()
            });
        }
        project.audio_tracks = vec![AudioTrack::default()];
        for &end in audio_ends {
            project.audio_tracks[0].clips.push(AudioClip {
                path: "x.wav".to_string(),
                start_ticks: end.saturating_sub(TICKS_PER_BAR),
                // One bar at the default 120 BPM (U11: seconds, not ticks).
                source_offset: 0.0,
                source_len: 2.0,
                gain: 1.0,
                ..AudioClip::default()
            });
        }
        project
    }

    #[test]
    fn empty_project_exports_nothing() {
        let mut project = Project::default();
        project.midi_tracks.clear();
        assert_eq!(export_end_ticks(&project), 0);
        assert_eq!(
            ExportRange::FullSong.window(&project),
            Some((0.0, 0.0)),
            "an empty project exports an empty (tail-only) file"
        );
    }

    #[test]
    fn audio_clips_extend_the_export_end() {
        // The sequencer's arrangement end covers MIDI/control only; the
        // export end must take audio clips into account (D5).
        let project = project_with(&[TICKS_PER_BAR], &[], &[4 * TICKS_PER_BAR]);
        assert_eq!(export_end_ticks(&project), 4 * TICKS_PER_BAR);

        let project = project_with(&[3 * TICKS_PER_BAR], &[2 * TICKS_PER_BAR], &[]);
        assert_eq!(export_end_ticks(&project), 3 * TICKS_PER_BAR);
    }

    #[test]
    fn end_rounds_up_to_a_bar() {
        // 1.5 bars of MIDI content (span 5760 of a 3840-tick bar) → 2 bars.
        let mut project = project_with(&[TICKS_PER_BAR], &[], &[]);
        project.midi_tracks[0].clips[0].span_ticks = TICKS_PER_BAR + TICKS_PER_BAR / 2;
        assert_eq!(export_end_ticks(&project), 2 * TICKS_PER_BAR);
    }

    #[test]
    fn song_loop_window_requires_an_enabled_loop() {
        let mut project = project_with(&[4 * TICKS_PER_BAR], &[], &[]);
        assert_eq!(ExportRange::SongLoop.window(&project), None);

        project.metadata.loop_range = SongLoop {
            enabled: true,
            start_ticks: TICKS_PER_BAR,
            end_ticks: 2 * TICKS_PER_BAR,
        };
        assert_eq!(
            ExportRange::SongLoop.window(&project),
            Some((f64::from(TICKS_PER_BAR), f64::from(2 * TICKS_PER_BAR)))
        );
    }

    #[test]
    fn bit_depths_match_the_wav_spec() {
        let spec = |bits: Bits| bits.wav_spec(48_000);
        let sixteen = spec(Bits::Int16);
        assert_eq!(
            (sixteen.bits_per_sample, sixteen.sample_format),
            (16, hound::SampleFormat::Int)
        );
        let twentyfour = spec(Bits::Int24);
        assert_eq!(
            (twentyfour.bits_per_sample, twentyfour.sample_format),
            (24, hound::SampleFormat::Int)
        );
        let float = spec(Bits::Float32);
        assert_eq!(
            (float.bits_per_sample, float.sample_format),
            (32, hound::SampleFormat::Float)
        );
        assert_eq!(float.channels, 2);
        assert_eq!(
            Bits::ALL.map(Bits::label),
            ["16-bit", "24-bit", "32-bit float"]
        );
    }

    #[test]
    fn the_device_rate_leads_the_sample_rate_choices() {
        let rates = sample_rate_choices(44_100);
        assert_eq!(rates[0], 44_100);
        assert!(!rates[1..].contains(&44_100), "no duplicate entries");
        assert!(rates.contains(&48_000) && rates.contains(&96_000));

        // A non-standard device rate stays selectable.
        let rates = sample_rate_choices(32_000);
        assert_eq!(rates[0], 32_000);
    }

    #[test]
    fn stem_names_survive_hostile_track_names() {
        assert_eq!(sanitize_file_part("Bass"), "Bass");
        assert_eq!(sanitize_file_part("Drums / Room"), "Drums _ Room");
        assert_eq!(sanitize_file_part("a:b*c?\"<>|"), "a_b_c_____");
        assert_eq!(sanitize_file_part("   "), "track");
        assert_eq!(sanitize_file_part(""), "track");
        assert_eq!(
            sanitize_file_part("trailing.  "),
            "trailing",
            "Windows strips these"
        );
        assert_eq!(sanitize_file_part("two  spaces"), "two spaces");

        let mix = std::path::Path::new("/songs/Demo Song.wav");
        assert_eq!(
            stem_path(mix, "Lead"),
            std::path::PathBuf::from("/songs/Demo Song_Lead.wav")
        );
    }

    /// S3c: one job renders the mix and every stem into its own file, with the
    /// requested format, and calls the setup callback for each pass after the
    /// first.
    #[test]
    fn a_multi_pass_job_writes_every_pass_in_the_requested_format() {
        let dir = std::env::temp_dir().join(format!("glitcherz_stems_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let format = WavFormat {
            sample_rate: 22_050,
            bits: Bits::Int24,
        };
        let passes = vec![
            ExportPass::mix(dir.join("mix.wav")),
            ExportPass::stem(dir.join("mix_bass.wav"), "Bass".into(), TrackSel::Midi(0)),
            ExportPass::stem(
                dir.join("mix_drums.wav"),
                "Drums".into(),
                TrackSel::Audio(0),
            ),
        ];
        let engine = glitcherz_engine::AudioEngine::start_offline(format.sample_rate)
            .expect("offline engine");
        let mut job = ExportJob::new(
            engine,
            Vec::new(),
            std::collections::HashMap::new(),
            format,
            (0.0, 0.0),
            passes.clone(),
        )
        .expect("job");

        let mut setup_calls: Vec<Option<TrackSel>> = Vec::new();
        let mut setup =
            |_engine: &mut glitcherz_engine::AudioEngine,
             _map: &std::collections::HashMap<u64, u64>,
             target: Option<TrackSel>| { setup_calls.push(target) };
        // An empty window renders the tail (1 s) and then flushes (2 s) before
        // the next pass; a generous block count finishes the whole job.
        let mut finished = false;
        for _ in 0..400 {
            if job.pump(64, &mut setup) {
                finished = true;
                break;
            }
        }
        assert!(finished, "three passes must finish: {}", job.progress());
        assert_eq!(
            setup_calls,
            vec![Some(TrackSel::Midi(0)), Some(TrackSel::Audio(0))],
            "the mix needs no re-address; each stem does"
        );
        assert_eq!(job.outputs().len(), 3);

        for pass in &passes {
            let reader = hound::WavReader::open(&pass.path).expect("written wav");
            let spec = reader.spec();
            assert_eq!(spec.sample_rate, 22_050);
            assert_eq!(spec.bits_per_sample, 24);
            assert_eq!(spec.channels, 2);
            assert!(reader.len() > 0, "each pass writes a tail of silence");
        }

        for pass in &passes {
            let _ = std::fs::remove_file(&pass.path);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
