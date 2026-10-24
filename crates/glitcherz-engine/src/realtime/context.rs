//! Per-block processing context.

/// Context of one fixed processing block. `Copy`, constructed by the audio
/// thread at each block boundary.
#[derive(Debug, Clone, Copy)]
pub struct ProcessContext {
    /// Sample rate (Hz).
    pub sample_rate: f32,
    /// Number of samples in this block (≤ `AudioBuffer` capacity).
    pub frames: usize,
    /// Global position of the block's first sample (accumulated sample count
    /// since the audio stream started).
    pub playback_sample_pos: u64,
    /// Song position (transport ticks) at the **start** of this block
    /// (U6): the tick-domain anchor for timeline-placed audio clips.
    /// Frozen while the transport is stopped.
    pub song_ticks: f64,
    /// Tick width of this block — how far the transport advances across it
    /// at the current effective tempo (U6). Computed after automation
    /// evaluation so a tempo curve governing this block is already applied.
    pub transport_dt_ticks: f64,
    /// Whether the transport is playing (U6): audio clips emit silence
    /// while stopped/paused (a frozen `song_ticks` would otherwise replay
    /// the same frames every block).
    pub transport_active: bool,
}

impl ProcessContext {
    /// Block start time (seconds). Time calculations uniformly use f64 (see
    /// AGENTS.md "Coding Standards").
    pub fn start_seconds(&self) -> f64 {
        crate::convert::samples_to_seconds(self.playback_sample_pos, self.sample_rate)
    }
}

#[cfg(test)]
mod tests {
    use super::ProcessContext;

    #[test]
    fn start_seconds_uses_f64_math() {
        let ctx = ProcessContext {
            sample_rate: 48_000.0,
            frames: 128,
            playback_sample_pos: 48_000,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
        };
        assert!((ctx.start_seconds() - 1.0).abs() < 1e-12);
    }
}
