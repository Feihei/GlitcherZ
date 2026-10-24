//! Real-time audio-track clip playback (U6, spec `.agents/spec/audio-track.md`
//! D1 / D4 / D7).
//!
//! [`AudioTrackProcessor`] is the generator node behind one
//! `NodeDoc::AudioTrack`: it reads wav material across the transport's tick
//! domain, stretched to each clip's span (tape-style — resizing changes
//! speed, tempo changes repitch).
//!
//! Playback math (D1): the read frame is a **pure function of the transport
//! position** — `frame = (song_tick − start)/span × wav_frames` — so loop
//! wrap and seek need no cursor bookkeeping or special cases: the position
//! *is* the state. Per output frame `j`, the clip tick is
//! `song_ticks + j·dt/frames`, interpolated linearly between wav frames.
//!
//! Clip table updates (D4): the table carries `Arc<SampleData>` (not
//! `Copy`), so it travels through the **structure queue**, not the command
//! queue. Replacing the table moves the old one out (`mem::replace`, no
//! drop on the audio thread); the BlockRunner forwards it to the **trash
//! queue** for deallocation on the control thread — the same
//! "recycle, never drop in place" protocol as node removal.
//!
//! RT safety: `process` is fixed loops over pre-allocated memory plus
//! linear interpolation arithmetic; no allocation, no locks, no syscalls.

use std::sync::Arc;

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;
use crate::realtime::sequencer::MAX_TRACK_CLIPS;
use crate::sample::SampleData;

/// Trash-queue capacity for displaced clip tables (U6 D4). The control
/// thread drains every UI frame; 64 slots absorb even a pathological edit
/// storm before updates start being deferred.
pub const AUDIO_CLIP_TRASH_CAPACITY: usize = 64;

/// Creates the trash channel pair (U6 D4): the producer end goes to the
/// block runner (the audio thread pushes displaced tables), the consumer
/// end to the engine (the control thread drains and drops them).
/// **Not RT-safe**; call only before the stream starts.
pub fn audio_clip_trash_channel() -> (
    rtrb::Producer<AudioClipTable>,
    rtrb::Consumer<AudioClipTable>,
) {
    rtrb::RingBuffer::new(AUDIO_CLIP_TRASH_CAPACITY)
}

/// One audio clip as seen by the audio thread: placement in the tick domain
/// plus the shared, already-decoded sample data.
///
/// `start_ticks`/`span_ticks` are `f64` to match the sequencer's transport
/// position without rounding at the block boundary.
#[derive(Debug, Clone)]
pub struct RtAudioClip {
    /// Absolute start position (transport ticks).
    pub start_ticks: f64,
    /// Playback window the wav is stretched across (ticks).
    pub span_ticks: f64,
    /// Decoded stereo frames (control-thread decode; shared, never
    /// reallocated on the audio side).
    pub data: Arc<SampleData>,
    /// Clip gain (linear).
    pub gain: f32,
}

/// Clip table of one audio track: at most [`MAX_TRACK_CLIPS`] clips,
/// indexed exactly like the document's `AudioTrack::clips` order.
pub type AudioClipTable = [Option<RtAudioClip>; MAX_TRACK_CLIPS];

/// An empty table (no clips — silent track).
pub fn empty_clip_table() -> AudioClipTable {
    std::array::from_fn(|_| None)
}

/// The audio-track generator node (one per `NodeDoc::AudioTrack`).
///
/// Output semantics: **overwrites** the node buffer (the graph pre-clears
/// it anyway; clearing here keeps the processor self-contained for direct
/// tests). Multiple overlapping clips on the same track sum.
#[derive(Debug)]
pub struct AudioTrackProcessor {
    clips: AudioClipTable,
}

impl AudioTrackProcessor {
    /// Builds the node with its initial clip table. **Control thread only**
    /// (the table carries `Arc` clones made here).
    pub fn new(clips: AudioClipTable) -> Self {
        Self { clips }
    }

    /// Swaps in a new clip table, returning the old one **without dropping
    /// it** (the caller forwards it to the trash queue; `mem::replace` is a
    /// move, so nothing deallocates on the audio thread). RT-safe.
    pub fn swap_clips(&mut self, clips: AudioClipTable) -> AudioClipTable {
        std::mem::replace(&mut self.clips, clips)
    }

    /// The current clip table (diagnostics / tests).
    pub fn clips(&self) -> &AudioClipTable {
        &self.clips
    }
}

impl Processor for AudioTrackProcessor {
    /// Renders every clip active in this block's tick window into the node
    /// buffer. RT-safe: fixed loops, plain float arithmetic, no allocation.
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        let frames = buffer.len();
        if frames == 0 || !ctx.transport_active {
            // Stopped / paused transport: digital silence (a frozen
            // `song_ticks` would otherwise replay the same frames forever).
            buffer.clear();
            return;
        }

        buffer.clear();
        let t0 = ctx.song_ticks;
        let dt = ctx.transport_dt_ticks;
        for clip in self.clips.iter().flatten() {
            render_clip(clip, t0, dt, frames, buffer);
        }
    }

    /// Audio tracks have no parameters (the mixer channel owns level/pan).
    fn set_param(&mut self, _id: ParamId, _value: f32) -> bool {
        false
    }

    /// U6: moves the old table out instead of dropping it — the caller
    /// forwards it to the trash queue for control-thread deallocation.
    /// RT-safe (a move, no deallocation).
    fn swap_audio_clips(&mut self, clips: AudioClipTable) -> Option<AudioClipTable> {
        Some(std::mem::replace(&mut self.clips, clips))
    }
}

/// Renders one clip across the block: for output frame `j` the clip tick is
/// `t0 + j·dt/frames`; frames outside `[start, start+span)` contribute
/// nothing (clip boundaries cut mid-block without clicks at exactly the
/// right tick). RT-safe: linear interpolation over pre-decoded frames.
fn render_clip(clip: &RtAudioClip, t0: f64, dt: f64, frames: usize, buffer: &mut AudioBuffer) {
    let src = clip.data.frames();
    if src.len() < 2 || clip.span_ticks <= 0.0 {
        return;
    }
    let last = (src.len() - 1) as f64;
    let Some((out_l, out_r)) = buffer.stereo_channels_mut() else {
        return;
    };
    let start = clip.start_ticks;
    let span = clip.span_ticks;
    let gain = if clip.gain.is_finite() {
        clip.gain
    } else {
        1.0
    };
    for j in 0..frames {
        let t = t0 + (j as f64 / frames as f64) * dt;
        if t < start || t >= start + span {
            continue;
        }
        let pos = ((t - start) / span) * last;
        let i = pos.floor();
        let frac = (pos - i) as f32;
        let i = i as usize;
        // `pos <= last` guarantees i+1 exists (src.len() >= 2).
        let a = src[i];
        let b = src[i + 1];
        let l = a[0] + (b[0] - a[0]) * frac;
        let r = a[1] + (b[1] - a[1]) * frac;
        out_l[j] += l * gain;
        out_r[j] += r * gain;
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioTrackProcessor, empty_clip_table};

    #[test]
    fn empty_table_starts_silent() {
        let processor = AudioTrackProcessor::new(empty_clip_table());
        assert!(processor.clips().iter().all(|c| c.is_none()));
    }
}
