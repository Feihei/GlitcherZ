//! Audio input capture (R4, spec `.agents/spec/recording.md` §3.3).
//!
//! One cpal stream feeds two lock-free queues. The stream is a capture
//! endpoint by default, or a **render** endpoint captured through WASAPI
//! loopback (R9) — "record what this machine plays" without a virtual cable.
//!
//! * **monitor**: consumed by the graph's `InputProcessor`, so the input can
//!   be routed into effects / a mixer like any other source (D9);
//! * **record**: consumed by the UI on the control thread while a take runs.
//!
//! Both are `crossbeam_queue::ArrayQueue`s shared through `Arc`, which is what
//! makes the handles survive a full graph rebuild: the processor holds a clone,
//! the callback holds a clone, and no consumer has to be handed back and forth.
//! Nothing in the callbacks allocates, locks or logs.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;

use crate::engine::EngineError;

/// Monitor ring capacity: ~170 ms of stereo at 48 kHz. Deep enough that a
/// stalled block never chops the monitoring path; a full ring drops samples
/// (counted), never blocks.
pub const INPUT_RING_CAPACITY: usize = 16_384;

/// Record ring capacity: ~0.7 s of stereo at 48 kHz. The UI drains it every
/// frame, so a full ring means the UI fell far behind (counted, not silent).
pub const RECORD_RING_CAPACITY: usize = 65_536;

/// One shared lock-free sample queue.
#[derive(Clone)]
pub struct InputRing {
    queue: Arc<ArrayQueue<f32>>,
}

impl InputRing {
    /// Allocates the queue. **Not RT-safe**.
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: Arc::new(ArrayQueue::new(capacity.max(1))),
        }
    }

    /// Pushes one interleaved sample. `false` when the ring is full.
    /// **RT-safe** (bounded lock-free push, no allocation).
    #[inline]
    pub fn push(&self, sample: f32) -> bool {
        self.queue.push(sample).is_ok()
    }

    /// Pops one interleaved sample. **RT-safe**.
    #[inline]
    pub fn pop(&self) -> Option<f32> {
        self.queue.pop()
    }

    /// Drops everything currently queued (control thread only).
    pub fn clear(&self) {
        while self.queue.pop().is_some() {}
    }

    /// Number of queued samples (approximate; diagnostics).
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether nothing is queued (diagnostics).
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

/// Input counters shared with the UI.
#[derive(Debug, Default)]
pub struct InputStats {
    /// Samples received from the device.
    received: AtomicU64,
    /// Samples dropped because a ring was full.
    dropped: AtomicU64,
    /// Samples the monitor processor had to fill with silence (underrun).
    underruns: AtomicU64,
    /// Peak absolute input level, f32 bits (relaxed, one read per frame).
    peak_bits: AtomicU32,
}

impl InputStats {
    /// Samples received since the stream opened.
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// Samples dropped on a full ring.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Monitor underrun samples.
    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    /// Peak input level, `0..` (linear).
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.load(Ordering::Relaxed))
    }

    /// Publishes an underrun count (audio thread, absolute value).
    pub fn set_underruns(&self, value: u64) {
        self.underruns.store(value, Ordering::Relaxed);
    }
}

/// Where a recording stream takes its samples from (R9).
///
/// WASAPI captures a **render** endpoint as easily as a capture one: cpal sees
/// the endpoint's data flow and sets `AUDCLNT_STREAMFLAGS_LOOPBACK` itself
/// (`host/wasapi/device.rs`), so "record what this machine plays" needs no
/// extra crate and no virtual cable — just a separate source kind, because the
/// two paths differ in config probing (see [`choose_config`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RecordSource {
    /// The system default input device.
    #[default]
    Default,
    /// A named capture endpoint (microphone, interface, virtual cable input).
    Capture(String),
    /// A named render endpoint, captured through WASAPI loopback: everything
    /// this output device plays, including other applications.
    Loopback(String),
}

impl RecordSource {
    /// Endpoint name, `None` for the system default.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Default => None,
            Self::Capture(name) | Self::Loopback(name) => Some(name),
        }
    }

    /// Whether this source listens to an output endpoint.
    pub fn is_loopback(&self) -> bool {
        matches!(self, Self::Loopback(_))
    }
}

/// Input stream description for the UI / take writer.
#[derive(Debug, Clone)]
pub struct AudioInputInfo {
    /// Device name.
    pub device_name: String,
    /// Number of input channels the ring carries.
    pub channels: usize,
    /// Sample rate (Hz). Always the engine's output rate (no resampling, D4).
    pub sample_rate: u32,
    /// Whether the samples are this machine's playback (WASAPI loopback).
    pub loopback: bool,
}

/// The live input stream plus the handles its callback and consumers share.
pub struct AudioInput {
    monitor: InputRing,
    record: InputRing,
    monitor_enabled: Arc<AtomicBool>,
    record_enabled: Arc<AtomicBool>,
    stats: Arc<InputStats>,
    info: AudioInputInfo,
    _stream: cpal::Stream,
}

/// Every source a take can record from: the host's capture endpoints, then its
/// render endpoints (WASAPI loopback), each group sorted by name.
///
/// **Control thread only** (enumeration opens the driver). A host that cannot
/// enumerate returns an empty list; `open` still falls back to the system
/// default, so a picker failure never kills recording.
pub fn record_sources() -> Vec<RecordSource> {
    let host = cpal::default_host();
    let mut sources = Vec::new();
    if let Ok(devices) = host.input_devices() {
        sources.extend(devices.map(|device| RecordSource::Capture(device.to_string())));
    }
    if let Ok(devices) = host.output_devices() {
        sources.extend(devices.map(|device| RecordSource::Loopback(device.to_string())));
    }
    sources.sort_by_key(RecordSource::sort_key);
    sources.dedup();
    sources
}

impl RecordSource {
    /// Capture endpoints before render endpoints, then by lowercased name.
    fn sort_key(&self) -> (bool, String) {
        (
            self.is_loopback(),
            self.name().unwrap_or_default().to_lowercase(),
        )
    }
}

/// Finds `wanted` in `devices` (exact name first, then case-insensitive), or
/// explains why it is gone.
fn device_by_name(
    devices: Vec<cpal::Device>,
    wanted: &str,
    role: &str,
) -> Result<cpal::Device, EngineError> {
    if let Some(device) = devices.iter().find(|device| device.to_string() == wanted) {
        return Ok(device.clone());
    }
    let lowered = wanted.to_lowercase();
    if let Some(device) = devices
        .iter()
        .find(|device| device.to_string().to_lowercase() == lowered)
    {
        return Ok(device.clone());
    }
    Err(EngineError::InputUnsupported(format!(
        "{role} device \"{wanted}\" is not available (unplugged?). Available: {}",
        if devices.is_empty() {
            "none".to_string()
        } else {
            devices
                .iter()
                .map(|device| device.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    )))
}

/// Resolves a source to its endpoint and says whether it is a render endpoint
/// (loopback) — checked from the endpoint itself, not from the source kind, so
/// a stale pick cannot quietly turn into the wrong direction.
fn resolve_source(
    host: &cpal::Host,
    source: &RecordSource,
) -> Result<(cpal::Device, bool), EngineError> {
    match source {
        RecordSource::Default => Ok((
            host.default_input_device()
                .ok_or(EngineError::NoInputDevice)?,
            false,
        )),
        RecordSource::Capture(name) => {
            let devices: Vec<cpal::Device> = host.input_devices()?.collect();
            Ok((device_by_name(devices, name, "input")?, false))
        }
        RecordSource::Loopback(name) => {
            let devices: Vec<cpal::Device> = host.output_devices()?.collect();
            Ok((device_by_name(devices, name, "output")?, true))
        }
    }
}

impl AudioInput {
    /// Opens `source` at the engine's rate.
    ///
    /// No resampling (spec "不做的事"): a device that cannot deliver the
    /// engine's rate is a hard error, so what is recorded is byte-for-byte
    /// what the timeline expects. That bites loopback harder than capture —
    /// shared-mode loopback is never converted by the OS — so the error names
    /// the rates the endpoint does run.
    pub fn open(sample_rate: u32, source: &RecordSource) -> Result<Self, EngineError> {
        let host = cpal::default_host();
        let (device, loopback) = resolve_source(&host, source)?;
        let device_name = device.to_string();
        let chosen = choose_config(&device, sample_rate, &device_name, loopback)?;
        let channels = chosen.channels() as usize;
        let monitor = InputRing::new(INPUT_RING_CAPACITY);
        let record = InputRing::new(RECORD_RING_CAPACITY);
        let monitor_enabled = Arc::new(AtomicBool::new(false));
        let record_enabled = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(InputStats::default());

        let cb_monitor = monitor.clone();
        let cb_record = record.clone();
        let cb_monitor_on = monitor_enabled.clone();
        let cb_record_on = record_enabled.clone();
        let cb_stats = stats.clone();
        let stream = device.build_input_stream(
            chosen.config(),
            move |data: &[f32], _info: &cpal::InputCallbackInfo| {
                // Non-allocating, non-blocking callback: every sample either
                // enters a ring or bumps a counter.
                cb_stats
                    .received
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                let mut peak = 0.0f32;
                for &sample in data {
                    let abs = sample.abs();
                    if abs > peak {
                        peak = abs;
                    }
                }
                cb_stats.peak_bits.store(peak.to_bits(), Ordering::Relaxed);
                if cb_monitor_on.load(Ordering::Relaxed) {
                    for &sample in data {
                        if !cb_monitor.push(sample) {
                            cb_stats.dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                if cb_record_on.load(Ordering::Relaxed) {
                    for &sample in data {
                        if !cb_record.push(sample) {
                            cb_stats.dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            },
            move |err| {
                log::error!("audio input stream error: {err}");
            },
            None,
        )?;
        stream.play()?;
        Ok(Self {
            monitor,
            record,
            monitor_enabled,
            record_enabled,
            stats,
            info: AudioInputInfo {
                device_name,
                channels,
                sample_rate,
                loopback,
            },
            _stream: stream,
        })
    }

    /// Monitor ring handle for the graph's `InputProcessor`.
    pub fn monitor_ring(&self) -> InputRing {
        self.monitor.clone()
    }

    /// Whether the monitor path is fed (set by the UI around graph rebuilds).
    pub fn set_monitor_enabled(&self, enabled: bool) {
        self.monitor_enabled.store(enabled, Ordering::Relaxed);
        if !enabled {
            self.monitor.clear();
        }
    }

    /// Starts / stops feeding the take ring. Enabling clears stale samples
    /// first, so a take never begins with material from before it was armed.
    pub fn set_record_enabled(&self, enabled: bool) {
        if enabled {
            self.record.clear();
            self.record_enabled.store(true, Ordering::Relaxed);
        } else {
            self.record_enabled.store(false, Ordering::Relaxed);
        }
    }

    /// Drains the take ring into `out`; returns the number of samples moved.
    pub fn drain_recorded(&self, out: &mut Vec<f32>) -> usize {
        let mut count = 0;
        while let Some(sample) = self.record.pop() {
            out.push(sample);
            count += 1;
        }
        count
    }

    /// Shared counters.
    pub fn stats(&self) -> Arc<InputStats> {
        self.stats.clone()
    }

    /// Device description.
    pub fn info(&self) -> &AudioInputInfo {
        &self.info
    }
}

/// Picks an f32 config at `sample_rate` for a capture or loopback stream.
///
/// The two directions answer different questions. cpal reports an output
/// device's formats through `supported_output_configs` and returns **empty**
/// from `supported_input_configs` ("assume no input formats"), so a loopback
/// stream has to probe the render side; and because shared-mode loopback never
/// gets the output path's automatic format conversion, only natively supported
/// rates work.
///
/// The failure text lists what the device *does* offer: "no f32 at 48 kHz"
/// alone leaves the user with no next step, and a picker needs to explain why
/// the device they chose cannot be used.
fn choose_config(
    device: &cpal::Device,
    sample_rate: u32,
    device_name: &str,
    loopback: bool,
) -> Result<cpal::SupportedStreamConfig, EngineError> {
    let mut chosen: Option<cpal::SupportedStreamConfigRange> = None;
    let mut offered: Vec<String> = Vec::new();
    // The two directions return distinct iterator types, so normalise to a
    // Vec before the shared selection loop.
    let probe: Result<Vec<cpal::SupportedStreamConfigRange>, cpal::Error> = if loopback {
        device
            .supported_output_configs()
            .map(std::iter::Iterator::collect)
    } else {
        device
            .supported_input_configs()
            .map(std::iter::Iterator::collect)
    };
    let mut probe_error = None;
    match probe {
        Ok(ranges) => {
            for range in ranges {
                offered.push(format!(
                    "{:?} {}ch {}..{} Hz",
                    range.sample_format(),
                    range.channels(),
                    range.min_sample_rate(),
                    range.max_sample_rate()
                ));
                if range.sample_format() != cpal::SampleFormat::F32 {
                    continue;
                }
                if sample_rate < range.min_sample_rate() || sample_rate > range.max_sample_rate() {
                    continue;
                }
                let better = match &chosen {
                    None => true,
                    // A system mix is inherently multichannel: taking mono
                    // first would drop half of what the machine plays.
                    Some(current) if loopback => {
                        stereo_preference(range.channels(), current.channels())
                    }
                    Some(current) => preference(range.channels(), current.channels()),
                };
                if better {
                    chosen = Some(range);
                }
            }
        }
        Err(err) => probe_error = Some(err.to_string()),
    }
    let range = chosen.ok_or_else(|| {
        let reason = if loopback {
            format!(
                "{device_name}: WASAPI loopback is not resampled by the OS, so {sample_rate} Hz \
                 must be a rate this output endpoint runs natively — it is not"
            )
        } else {
            format!("{device_name}: no f32 input config at {sample_rate} Hz (no resampling)")
        };
        EngineError::InputUnsupported(format!(
            "{reason}; device offers [{}{}]{}",
            if offered.is_empty() {
                "nothing".to_string()
            } else {
                offered.join(", ")
            },
            if loopback {
                " — set the endpoint to the engine rate in Windows sound settings, or pick \
                  another device"
            } else {
                ""
            },
            probe_error.map_or_else(String::new, |err| format!(" ({err})")),
        ))
    })?;
    Ok(range.with_sample_rate(sample_rate))
}

/// Channel preference for loopback: stereo, then mono, then the smallest.
fn stereo_preference(candidate: u16, current: u16) -> bool {
    let rank = |channels: u16| match channels {
        2 => 0,
        1 => 1,
        n => 2 + n,
    };
    rank(candidate) < rank(current)
}

/// Channel preference: mono first (one channel is all a take needs), then
/// stereo, then the smallest available.
fn preference(candidate: u16, current: u16) -> bool {
    let rank = |channels: u16| match channels {
        1 => 0,
        2 => 1,
        n => 2 + n,
    };
    rank(candidate) < rank(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The picker groups by kind, so the sort key must put every capture
    /// endpoint before every loopback one (and keep names case-insensitive
    /// alphabetical within a group).
    #[test]
    fn capture_sources_sort_before_loopback() {
        let mut sources = vec![
            RecordSource::Loopback("USB DAC".to_string()),
            RecordSource::Capture("mic B".to_string()),
            RecordSource::Loopback("Realtek".to_string()),
            RecordSource::Capture("Mic A".to_string()),
        ];
        sources.sort_by_key(RecordSource::sort_key);
        assert_eq!(
            sources,
            vec![
                RecordSource::Capture("Mic A".to_string()),
                RecordSource::Capture("mic B".to_string()),
                RecordSource::Loopback("Realtek".to_string()),
                RecordSource::Loopback("USB DAC".to_string()),
            ]
        );
    }

    /// `Default` is an input device: it must never be treated as loopback, or
    /// the engine would look for a render endpoint it was never given.
    #[test]
    fn default_source_is_a_capture_source() {
        assert_eq!(RecordSource::default(), RecordSource::Default);
        assert!(!RecordSource::default().is_loopback());
        assert_eq!(RecordSource::default().name(), None);
        assert!(RecordSource::Loopback("x".to_string()).is_loopback());
    }
}
