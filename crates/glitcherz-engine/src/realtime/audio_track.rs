//! Real-time audio-track clip playback (U6, spec `.agents/spec/audio-track.md`
//! D1 / D4 / D7).
//!
//! [`AudioTrackProcessor`] is the generator node behind one
//! `NodeDoc::AudioTrack`: it plays wav material across the transport's tick
//! domain as a **trim window** (U11). The wav runs at its **native rate** —
//! only a sample-rate conversion, never a pitch-changing stretch — and each
//! clip selects a window (`source_offset`/`source_len`, seconds) into it.
//!
//! Playback math (U11 D1/D2): per output frame `j` the clip's elapsed time is
//! `elapsed = (song_ticks + j·dt/frames − start) · sec_per_tick` seconds; the
//! read frame is `(source_offset + elapsed) · wav_sample_rate`. The mapping is
//! a **pure function of the transport position** (loop wrap / seek need no
//! cursor bookkeeping), and `sec_per_tick` is derived from the **base** tempo
//! (D2): audio does not follow the tempo curve. Frames outside
//! `[0, source_len)` or past the wav tail are silent.
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

/// One audio clip as seen by the audio thread: placement in the tick domain,
/// its trim window into the wav (seconds), and the shared, already-decoded
/// sample data.
///
/// `start_ticks` is `f64` to match the sequencer's transport position without
/// rounding at the block boundary; `source_offset`/`source_len` are seconds
/// (U11 D1: native-rate playback, no stretch).
#[derive(Debug, Clone)]
pub struct RtAudioClip {
    /// Absolute start position (transport ticks).
    pub start_ticks: f64,
    /// Trim window start inside the wav (seconds, `>= 0`).
    pub source_offset: f64,
    /// Trim window length (seconds, `> 0`).
    pub source_len: f64,
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
        let sec_per_tick = ctx.sec_per_tick;
        for clip in self.clips.iter().flatten() {
            render_clip(clip, t0, dt, sec_per_tick, frames, buffer);
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

/// Renders one clip across the block (U11 D1): the wav plays at its native
/// rate — only a sample-rate conversion — the clip being a trim window into
/// the source. For output frame `j` the clip's elapsed time is
/// `(t − start) · sec_per_tick` seconds; outputs outside `[0, source_len)`
/// contribute nothing (clip boundaries cut mid-block at exactly the right
/// tick), and reads past the wav tail are silent. RT-safe: linear
/// interpolation over pre-decoded frames.
fn render_clip(
    clip: &RtAudioClip,
    t0: f64,
    dt: f64,
    sec_per_tick: f64,
    frames: usize,
    buffer: &mut AudioBuffer,
) {
    let src = clip.data.frames();
    if src.len() < 2 || clip.source_len.is_nan() || clip.source_len <= 0.0 {
        return;
    }
    let Some((out_l, out_r)) = buffer.stereo_channels_mut() else {
        return;
    };
    let start = clip.start_ticks;
    let offset = if clip.source_offset.is_finite() {
        clip.source_offset.max(0.0)
    } else {
        0.0
    };
    let len = clip.source_len;
    let wav_sr = f64::from(clip.data.sample_rate());
    let last = src.len() as f64;
    let gain = if clip.gain.is_finite() {
        clip.gain
    } else {
        1.0
    };
    for j in 0..frames {
        let t = t0 + (j as f64 / frames as f64) * dt;
        let elapsed = (t - start) * sec_per_tick;
        // NaN (a poisoned clip start) or outside the window: silent — the
        // explicit `is_nan` keeps a NaN from poisoning the buffer.
        if elapsed.is_nan() || elapsed < 0.0 || elapsed >= len {
            continue;
        }
        let pos = (offset + elapsed) * wav_sr;
        // Before the window's file position or past the wav tail: silent.
        if pos.is_nan() || pos < 0.0 || pos >= last {
            continue;
        }
        let i = pos.floor();
        let frac = (pos - i) as f32;
        let i = i as usize;
        // `pos < last` and `src.len() >= 2`, but the last frame has no
        // successor — interpolate against itself there.
        let (a, b) = if i + 1 < src.len() {
            (src[i], src[i + 1])
        } else {
            (src[i], src[i])
        };
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
