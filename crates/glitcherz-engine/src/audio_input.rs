//! Audio input capture (R4, spec `.agents/spec/recording.md` §3.3).
//!
//! One cpal input stream feeds two lock-free queues:
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

/// Input stream description for the UI / take writer.
#[derive(Debug, Clone)]
pub struct AudioInputInfo {
    /// Device name.
    pub device_name: String,
    /// Number of input channels the ring carries.
    pub channels: usize,
    /// Sample rate (Hz). Always the engine's output rate (no resampling, D4).
    pub sample_rate: u32,
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

/// Names of the host's input devices, sorted, de-duplicated.
///
/// **Control thread only** (device enumeration opens the driver). A host
/// that cannot enumerate returns an empty list; `open` then still falls back
/// to the system default, so a picker failure never kills recording.
pub fn input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut names: Vec<String> = devices.map(|device| device.to_string()).collect();
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup();
    names
}

/// Finds `wanted` among the host's input devices (exact name first, then a
/// case-insensitive match), or explains why it is gone.
fn device_by_name(host: &cpal::Host, wanted: &str) -> Result<cpal::Device, EngineError> {
    let devices: Vec<cpal::Device> = host.input_devices()?.collect();
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
        "input device \"{wanted}\" is not available (unplugged?). Available: {}",
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

impl AudioInput {
    /// Opens `device_name`, or the system default when it is `None`.
    ///
    /// No resampling (spec "不做的事"): a device that cannot deliver the
    /// engine's rate is a hard error, so what is recorded is byte-for-byte
    /// what the timeline expects.
    pub fn open(sample_rate: u32, device_name: Option<&str>) -> Result<Self, EngineError> {
        let host = cpal::default_host();
        let device = match device_name {
            Some(name) => device_by_name(&host, name)?,
            None => host
                .default_input_device()
                .ok_or(EngineError::NoInputDevice)?,
        };
        let device_name = device.to_string();
        let chosen = choose_config(&device, sample_rate, &device_name)?;
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

/// Picks an f32 input config at `sample_rate`, preferring mono, then stereo,
/// then whatever the device offers.
///
/// The failure text lists what the device *does* offer: "no f32 at 48 kHz"
/// alone leaves the user with no next step, and a picker needs to explain
/// why the device they chose cannot be used.
fn choose_config(
    device: &cpal::Device,
    sample_rate: u32,
    device_name: &str,
) -> Result<cpal::SupportedStreamConfig, EngineError> {
    let mut chosen: Option<cpal::SupportedStreamConfigRange> = None;
    let mut offered: Vec<String> = Vec::new();
    if let Ok(ranges) = device.supported_input_configs() {
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
                Some(current) => preference(range.channels(), current.channels()),
            };
            if better {
                chosen = Some(range);
            }
        }
    }
    let range = chosen.ok_or_else(|| {
        EngineError::InputUnsupported(format!(
            "{device_name}: no f32 input config at {sample_rate} Hz (no resampling is \
             performed); device offers [{}]",
            if offered.is_empty() {
                "nothing".to_string()
            } else {
                offered.join(", ")
            }
        ))
    })?;
    Ok(range.with_sample_rate(sample_rate))
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
