//! Offline audio export (U7, spec `.agents/spec/export-audio.md`).
//!
//! Renders the project to a 16-bit stereo wav without the real-time stream:
//! an offline [`AudioEngine`] drives the same block path the cpal callback
//! takes, while a `hound` writer consumes the interleaved output. The job
//! is **sliced across frames** (D1, the U9 plugin-scan precedent): the app
//! pumps a handful of blocks at the end of each UI frame, so long exports
//! neither freeze the interface nor block cancellation.
//!
//! Snapshot semantics (D6): everything the render uses — clip tables,
//! plugin instances with their state, parameters — is copied into the
//! offline engine when the job starts; document edits during the export do
//! not affect the running render.

use std::io::BufWriter;
use std::path::{Path, PathBuf};

use glitcherz_engine::AudioEngine;
use glitcherz_model::Project;

use crate::PluginGuard;

/// Blocks rendered per UI frame (~90 ms of audio at 48 kHz / 128 frames —
/// smooth progress, well under one frame budget even on slow machines).
pub const BLOCKS_PER_FRAME: usize = 34;

/// Tail rendered past the end point: one second, so delays and releases
/// ring out into the file.
pub const TAIL_SECONDS: f64 = 1.0;

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

/// Latest clip end across **all** track kinds — MIDI, control and audio
/// (U7 D5: the sequencer's arrangement end only knows MIDI/control
/// content; an audio clip ending later would be cut by the auto-stop).
/// Rounded up to a whole bar, `0` for an empty arrangement.
pub fn export_end_ticks(project: &Project) -> u32 {
    let bar = u64::from(project.ticks_per_bar());
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
            latest = latest.max(u64::from(clip.start_ticks) + u64::from(clip.span_ticks));
        }
    }
    let end = if latest == 0 {
        0
    } else {
        latest.div_ceil(bar) * bar
    };
    end.min(u64::from(u32::MAX)) as u32
}

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
    writer: hound::WavWriter<BufWriter<std::fs::File>>,
    /// One interleaved stereo block.
    block: Vec<f32>,
    /// Render window end (ticks); the tail counts down after reaching it.
    end_ticks: f64,
    start_ticks: f64,
    /// Tail blocks left (`0` while the transport has not reached the end).
    tail_left: usize,
    /// Set by the Cancel button; the next pump drops the job.
    pub cancelled: bool,
    output_path: PathBuf,
}

impl ExportJob {
    /// Assembles the writer half (the engine half is the app's assembly
    /// job — it needs the session caches, which live on `GlitcherZApp`).
    pub(crate) fn new(
        engine: AudioEngine,
        guards: Vec<PluginGuard>,
        path: &Path,
        sample_rate: u32,
        window: (f64, f64),
    ) -> hound::Result<Self> {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(path, spec)?;
        let tail = tail_blocks(sample_rate);
        Ok(Self {
            engine,
            guards,
            writer,
            block: vec![0.0f32; glitcherz_engine::realtime::FIXED_BLOCK_SIZE * 2],
            start_ticks: window.0,
            end_ticks: window.1,
            tail_left: tail,
            cancelled: false,
            output_path: path.to_path_buf(),
        })
    }

    /// The file being written (status messages).
    pub fn path(&self) -> &Path {
        &self.output_path
    }

    /// Render progress in `0..=1` (ticks inside the window; the tail sits
    /// at 1.0).
    pub fn progress(&self) -> f32 {
        let span = (self.end_ticks - self.start_ticks).max(1.0);
        let ticks = f64::from(self.engine.musical_ticks());
        ((ticks - self.start_ticks) / span).clamp(0.0, 1.0) as f32
    }

    /// Renders up to `blocks` blocks into the writer. Returns `true` when
    /// the export is **finished** (end + tail reached) or cancelled — the
    /// caller drops the job and reports.
    pub fn pump(&mut self, blocks: usize) -> bool {
        for _ in 0..blocks {
            if self.cancelled {
                return true;
            }
            // Park the transport at the window end: the tail is a static
            // render of whatever the effects are still ringing (the
            // sequencer's own auto-stop lands here too — the floor the
            // assembly set keeps it from firing early).
            if self.engine.is_playing() && f64::from(self.engine.musical_ticks()) >= self.end_ticks
            {
                let _ = self.engine.pause();
            }
            if !self.engine.is_playing() && self.tail_left == 0 {
                return true;
            }
            if !self.engine.render_offline_block(&mut self.block) {
                return true; // not an offline engine — bail out
            }
            // 16-bit conversion, interleaved as delivered.
            for &sample in &self.block {
                let value = if sample.is_finite() {
                    sample.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                let Ok(()) = self.writer.write_sample((value * 32_767.0) as i16) else {
                    return true; // disk failure — stop the export
                };
            }
            if !self.engine.is_playing() {
                self.tail_left = self.tail_left.saturating_sub(1);
            }
        }
        false
    }

    /// Finalizes the wav file (writes the RIFF sizes). The app calls this
    /// when `pump` reported completion and the job was not cancelled.
    pub fn finalize(self) -> hound::Result<PathBuf> {
        self.writer.finalize()?;
        Ok(self.output_path)
    }
}

/// Tail length in blocks: one second past the end point.
fn tail_blocks(sample_rate: u32) -> usize {
    let sr = sample_rate.max(1) as f64;
    (TAIL_SECONDS * sr / glitcherz_engine::realtime::FIXED_BLOCK_SIZE as f64).ceil() as usize
}

#[cfg(test)]
mod tests {
    use glitcherz_model::{
        AudioClip, AudioTrack, ControlClip, ControlTrack, MidiClip, MidiTrack, Project, SongLoop,
        TICKS_PER_BAR,
    };

    use super::{ExportRange, export_end_ticks};

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
                span_ticks: TICKS_PER_BAR,
                gain: 1.0,
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
}
