//! Document ↔ engine bridge for built-in effects (spec
//! `.agents/spec/effects.md` §5/D1).
//!
//! The model stores a tagged [`EffectParams`] payload; the engine exposes a
//! [`BuiltinEffectKind`] plus a static descriptor table. This module is the
//! single place that maps one onto the other, and the single place that knows
//! which document field backs which parameter slot — every UI surface (patch
//! inline rows, Inspector sliders, param push, menu labels) goes through it, so
//! a new effect only adds arms here.

use std::sync::Arc;

use glitcherz_engine::realtime::{
    BuiltinEffectKind, ChorusProcessor, CompressorProcessor, DelayProcessor, EffectDescriptor,
    EffectMeters, EqProcessor, FlangerProcessor, ParamId, ParamSpec, Processor, ReverbProcessor,
    Unit, chorus, compressor, delay, eq, flanger, reverb,
};
use glitcherz_model::{BuiltinEffectKind as DocEffectKind, EffectParams};

/// A freshly built built-in effect processor.
pub struct BuiltinProcessor {
    /// The node the graph will install.
    pub processor: Box<dyn Processor>,
    /// Meter handle for effects that publish one (the compressor). The caller
    /// keeps a clone in its `node_id → meters` map; `None` for the rest.
    pub meters: Option<Arc<EffectMeters>>,
}

/// Engine kind of a document payload.
pub fn engine_kind(params: &EffectParams) -> BuiltinEffectKind {
    match params.kind() {
        DocEffectKind::Delay => BuiltinEffectKind::Delay,
        DocEffectKind::Eq => BuiltinEffectKind::Eq,
        DocEffectKind::Compressor => BuiltinEffectKind::Compressor,
        DocEffectKind::Reverb => BuiltinEffectKind::Reverb,
        DocEffectKind::Chorus => BuiltinEffectKind::Chorus,
        DocEffectKind::Flanger => BuiltinEffectKind::Flanger,
    }
}

/// Static descriptor for a document payload.
pub fn descriptor(params: &EffectParams) -> &'static EffectDescriptor {
    engine_kind(params).descriptor()
}

/// Default payload for a newly created node of `kind`.
pub fn default_params(kind: DocEffectKind) -> EffectParams {
    match kind {
        DocEffectKind::Delay => EffectParams::Delay(glitcherz_model::DelayParams::default()),
        DocEffectKind::Eq => EffectParams::Eq(Box::default()),
        DocEffectKind::Compressor => EffectParams::Compressor(Box::default()),
        DocEffectKind::Reverb => EffectParams::Reverb(Box::default()),
        DocEffectKind::Chorus => EffectParams::Chorus(Box::default()),
        DocEffectKind::Flanger => EffectParams::Flanger(Box::default()),
    }
}

/// Builds the processor for `params` and seeds **every** slot from the
/// document.
///
/// Seeding matters: the smoothers snap while the node has not rendered yet
/// (D12), so a restored project starts at its saved values instead of ramping
/// from the descriptor defaults — no fade-in, and the first block already
/// matches the document (D15).
pub fn processor(params: &EffectParams, sample_rate: f32) -> BuiltinProcessor {
    let mut meters = None;
    let mut processor: Box<dyn Processor> = match engine_kind(params) {
        BuiltinEffectKind::Delay => Box::new(DelayProcessor::new(sample_rate)),
        BuiltinEffectKind::Eq => Box::new(EqProcessor::new(sample_rate)),
        BuiltinEffectKind::Compressor => {
            let handle = Arc::new(EffectMeters::new());
            meters = Some(handle.clone());
            Box::new(CompressorProcessor::with_meters(sample_rate, Some(handle)))
        }
        BuiltinEffectKind::Reverb => Box::new(ReverbProcessor::new(sample_rate)),
        BuiltinEffectKind::Chorus => Box::new(ChorusProcessor::new(sample_rate)),
        BuiltinEffectKind::Flanger => Box::new(FlangerProcessor::new(sample_rate)),
    };
    let descriptor = descriptor(params);
    for slot in 0..descriptor.params.len() {
        let slot = slot as u8;
        processor.set_param(ParamId::BuiltinParam(slot), value(params, slot));
    }
    BuiltinProcessor { processor, meters }
}

/// Value of `slot` in the document payload (`0.0` for an unknown slot).
pub fn value(params: &EffectParams, slot: u8) -> f32 {
    match params {
        EffectParams::Delay(delay) => match slot {
            delay::SLOT_TIME => delay.time_s,
            delay::SLOT_FEEDBACK => delay.feedback,
            delay::SLOT_MIX => delay.mix,
            delay::SLOT_PING_PONG => bool_value(delay.ping_pong),
            delay::SLOT_SYNC => bool_value(delay.sync),
            delay::SLOT_DIVISION => delay.sync_division as f32,
            _ => 0.0,
        },
        EffectParams::Eq(eq) => match slot {
            eq::SLOT_LOW_GAIN => eq.low_gain,
            eq::SLOT_LOW_FREQ => eq.low_freq,
            eq::SLOT_MID_GAIN => eq.mid_gain,
            eq::SLOT_MID_FREQ => eq.mid_freq,
            eq::SLOT_MID_Q => eq.mid_q,
            eq::SLOT_HIGH_GAIN => eq.high_gain,
            eq::SLOT_HIGH_FREQ => eq.high_freq,
            eq::SLOT_LOW_CUT => eq.low_cut,
            _ => 0.0,
        },
        EffectParams::Compressor(comp) => match slot {
            compressor::SLOT_THRESHOLD => comp.threshold,
            compressor::SLOT_RATIO => comp.ratio,
            compressor::SLOT_ATTACK => comp.attack,
            compressor::SLOT_RELEASE => comp.release,
            compressor::SLOT_KNEE => comp.knee,
            compressor::SLOT_MAKEUP => comp.makeup,
            compressor::SLOT_MIX => comp.mix,
            compressor::SLOT_LINK => bool_value(comp.stereo_link),
            _ => 0.0,
        },
        EffectParams::Reverb(rev) => match slot {
            reverb::SLOT_PRE_DELAY => rev.pre_delay,
            reverb::SLOT_DECAY => rev.decay,
            reverb::SLOT_DAMPING => rev.damping,
            reverb::SLOT_LOW_CUT => rev.low_cut,
            reverb::SLOT_WIDTH => rev.width,
            reverb::SLOT_MIX => rev.mix,
            _ => 0.0,
        },
        EffectParams::Chorus(ch) => match slot {
            chorus::SLOT_RATE => ch.rate,
            chorus::SLOT_DEPTH => ch.depth,
            chorus::SLOT_WIDTH => ch.width,
            chorus::SLOT_MIX => ch.mix,
            _ => 0.0,
        },
        EffectParams::Flanger(fl) => match slot {
            flanger::SLOT_RATE => fl.rate,
            flanger::SLOT_DEPTH => fl.depth,
            flanger::SLOT_FEEDBACK => fl.feedback,
            flanger::SLOT_WIDTH => fl.width,
            flanger::SLOT_MIX => fl.mix,
            _ => 0.0,
        },
    }
}

/// Writes `slot` in the document payload (unknown slots are ignored).
pub fn set_value(params: &mut EffectParams, slot: u8, value: f32) {
    match params {
        EffectParams::Delay(delay) => match slot {
            delay::SLOT_TIME => delay.time_s = value,
            delay::SLOT_FEEDBACK => delay.feedback = value,
            delay::SLOT_MIX => delay.mix = value,
            delay::SLOT_PING_PONG => delay.ping_pong = value >= 0.5,
            delay::SLOT_SYNC => delay.sync = value >= 0.5,
            delay::SLOT_DIVISION => delay.sync_division = value.round().clamp(0.0, 7.0) as u8,
            _ => {}
        },
        EffectParams::Eq(eq) => match slot {
            eq::SLOT_LOW_GAIN => eq.low_gain = value,
            eq::SLOT_LOW_FREQ => eq.low_freq = value,
            eq::SLOT_MID_GAIN => eq.mid_gain = value,
            eq::SLOT_MID_FREQ => eq.mid_freq = value,
            eq::SLOT_MID_Q => eq.mid_q = value,
            eq::SLOT_HIGH_GAIN => eq.high_gain = value,
            eq::SLOT_HIGH_FREQ => eq.high_freq = value,
            eq::SLOT_LOW_CUT => eq.low_cut = value,
            _ => {}
        },
        EffectParams::Compressor(comp) => match slot {
            compressor::SLOT_THRESHOLD => comp.threshold = value,
            compressor::SLOT_RATIO => comp.ratio = value,
            compressor::SLOT_ATTACK => comp.attack = value,
            compressor::SLOT_RELEASE => comp.release = value,
            compressor::SLOT_KNEE => comp.knee = value,
            compressor::SLOT_MAKEUP => comp.makeup = value,
            compressor::SLOT_MIX => comp.mix = value,
            compressor::SLOT_LINK => comp.stereo_link = value >= 0.5,
            _ => {}
        },
        EffectParams::Reverb(rev) => match slot {
            reverb::SLOT_PRE_DELAY => rev.pre_delay = value,
            reverb::SLOT_DECAY => rev.decay = value,
            reverb::SLOT_DAMPING => rev.damping = value,
            reverb::SLOT_LOW_CUT => rev.low_cut = value,
            reverb::SLOT_WIDTH => rev.width = value,
            reverb::SLOT_MIX => rev.mix = value,
            _ => {}
        },
        EffectParams::Chorus(ch) => match slot {
            chorus::SLOT_RATE => ch.rate = value,
            chorus::SLOT_DEPTH => ch.depth = value,
            chorus::SLOT_WIDTH => ch.width = value,
            chorus::SLOT_MIX => ch.mix = value,
            _ => {}
        },
        EffectParams::Flanger(fl) => match slot {
            flanger::SLOT_RATE => fl.rate = value,
            flanger::SLOT_DEPTH => fl.depth = value,
            flanger::SLOT_FEEDBACK => fl.feedback = value,
            flanger::SLOT_WIDTH => fl.width = value,
            flanger::SLOT_MIX => fl.mix = value,
            _ => {}
        },
    }
}

/// `true` as `1.0` (switch slots are floats on the parameter path).
fn bool_value(on: bool) -> f32 {
    if on { 1.0 } else { 0.0 }
}

/// Range of a slot straight from the descriptor (`0..1` when unknown).
pub fn range(params: &EffectParams, slot: u8) -> (f32, f32) {
    match descriptor(params).spec(slot) {
        Some(spec) => (spec.min, spec.max),
        None => (0.0, 1.0),
    }
}

/// Slider text for one value ("12.5 ms", "-3.0 dB", "45 %").
pub fn format_value(spec: &ParamSpec, value: f32) -> String {
    match spec.unit {
        Unit::Db => format!("{value:+.1} dB"),
        Unit::Hz if value >= 1000.0 => format!("{:.2} kHz", value / 1000.0),
        Unit::Hz => format!("{value:.0} Hz"),
        // Seconds are the storage unit; below one second the millisecond form
        // is what a user actually reads off a delay readout.
        Unit::Seconds if value < 1.0 => format!("{:.1} ms", value * 1000.0),
        Unit::Seconds => format!("{value:.2} s"),
        Unit::Ms if value >= 1000.0 => format!("{:.2} s", value / 1000.0),
        Unit::Ms => format!("{value:.1} ms"),
        Unit::Percent => format!("{:.0} %", value * 100.0),
        Unit::Ratio => format!("{value:.2}"),
        Unit::Pan if value.abs() < 0.005 => "C".to_string(),
        Unit::Pan if value < 0.0 => format!("L{:.0}", -value * 100.0),
        Unit::Pan => format!("R{:.0}", value * 100.0),
        Unit::Enum => enum_label(spec.choices, value),
        Unit::Bool if value >= 0.5 => "On".to_string(),
        Unit::Bool => "Off".to_string(),
    }
}

/// Label of an [`Unit::Enum`] value (out-of-range falls back to `"-"`).
fn enum_label(choices: &[&str], value: f32) -> String {
    let index = if value.is_finite() {
        value.round().max(0.0) as usize
    } else {
        0
    };
    choices.get(index).copied().unwrap_or("-").to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        default_params, descriptor, engine_kind, format_value, processor, range, set_value, value,
    };
    use glitcherz_engine::realtime::{
        BuiltinEffectKind, compressor, delay, eq, validate_descriptor,
    };
    use glitcherz_model::{BuiltinEffectKind as DocEffectKind, DelayParams, EffectParams};

    fn delay_params() -> EffectParams {
        EffectParams::Delay(DelayParams {
            time_s: 0.25,
            feedback: 0.5,
            mix: 0.75,
            ..Default::default()
        })
    }

    fn eq_params() -> EffectParams {
        EffectParams::Eq(Box::new(glitcherz_model::EqParams {
            low_gain: 6.0,
            mid_q: 2.0,
            low_cut: 80.0,
            ..Default::default()
        }))
    }

    fn compressor_params() -> EffectParams {
        EffectParams::Compressor(Box::new(glitcherz_model::CompressorParams {
            threshold: -24.0,
            stereo_link: false,
            ..Default::default()
        }))
    }

    #[test]
    fn kind_maps_to_the_engine_roster() {
        assert_eq!(engine_kind(&delay_params()), BuiltinEffectKind::Delay);
        assert_eq!(descriptor(&delay_params()).name, "Delay");
        assert_eq!(engine_kind(&eq_params()), BuiltinEffectKind::Eq);
        assert_eq!(descriptor(&eq_params()).name, "EQ");
        assert_eq!(
            engine_kind(&compressor_params()),
            BuiltinEffectKind::Compressor
        );
        assert_eq!(descriptor(&compressor_params()).name, "Compressor");
        for kind in DocEffectKind::ALL {
            assert_eq!(
                validate_descriptor(descriptor(&default_params(*kind))),
                Ok(())
            );
        }
    }

    #[test]
    fn slots_read_and_write_the_document_fields() {
        let mut params = delay_params();
        assert!((value(&params, delay::SLOT_TIME) - 0.25).abs() < 1e-6);
        set_value(&mut params, delay::SLOT_MIX, 0.1);
        assert!((value(&params, delay::SLOT_MIX) - 0.1).abs() < 1e-6);
        // Out-of-range slots are inert, never a panic.
        assert_eq!(value(&params, 9), 0.0);
        set_value(&mut params, 9, 1.0);

        let mut params = eq_params();
        assert!((value(&params, eq::SLOT_LOW_GAIN) - 6.0).abs() < 1e-6);
        assert!((value(&params, eq::SLOT_MID_Q) - 2.0).abs() < 1e-6);
        set_value(&mut params, eq::SLOT_HIGH_GAIN, -4.0);
        assert!((value(&params, eq::SLOT_HIGH_GAIN) + 4.0).abs() < 1e-6);

        let mut params = compressor_params();
        assert_eq!(value(&params, compressor::SLOT_LINK), 0.0);
        set_value(&mut params, compressor::SLOT_LINK, 1.0);
        assert_eq!(value(&params, compressor::SLOT_LINK), 1.0);
        assert!(params.as_compressor().unwrap().stereo_link);
    }

    #[test]
    fn ranges_come_from_the_descriptor() {
        let params = delay_params();
        let (min, max) = range(&params, delay::SLOT_TIME);
        assert!((min - 0.005).abs() < 1e-9);
        assert!((max - 1.0).abs() < 1e-9);
        assert_eq!(range(&params, 42), (0.0, 1.0));
    }

    #[test]
    fn formatting_follows_the_unit() {
        let params = delay_params();
        let spec = descriptor(&params).spec(delay::SLOT_TIME).unwrap();
        assert_eq!(format_value(spec, 0.25), "250.0 ms");
        assert_eq!(format_value(spec, 1.5), "1.50 s");
        let mix = descriptor(&params).spec(delay::SLOT_MIX).unwrap();
        assert_eq!(format_value(mix, 0.5), "50 %");
        let eq_spec = descriptor(&eq_params()).spec(eq::SLOT_LOW_GAIN).unwrap();
        assert_eq!(format_value(eq_spec, -3.0), "-3.0 dB");
        let attack = descriptor(&compressor_params())
            .spec(compressor::SLOT_ATTACK)
            .unwrap();
        assert_eq!(format_value(attack, 10.0), "10.0 ms");
    }

    #[test]
    fn enum_values_format_with_their_labels() {
        let params = default_params(DocEffectKind::Delay);
        let spec = descriptor(&params).spec(delay::SLOT_DIVISION).unwrap();
        assert_eq!(format_value(spec, 0.0), "1/1");
        assert_eq!(format_value(spec, 2.0), "1/4");
        assert_eq!(format_value(spec, 7.0), "1/8D");
        assert_eq!(format_value(spec, 99.0), "-", "out-of-range falls back");
    }

    #[test]
    fn delay_stereo_and_sync_slots_round_trip() {
        let mut params = default_params(DocEffectKind::Delay);
        set_value(&mut params, delay::SLOT_PING_PONG, 1.0);
        set_value(&mut params, delay::SLOT_SYNC, 1.0);
        set_value(&mut params, delay::SLOT_DIVISION, 3.0);
        assert_eq!(value(&params, delay::SLOT_PING_PONG), 1.0);
        assert_eq!(value(&params, delay::SLOT_SYNC), 1.0);
        assert_eq!(value(&params, delay::SLOT_DIVISION), 3.0);
        let delay_params = params.as_delay().unwrap();
        assert!(delay_params.ping_pong && delay_params.sync);
        assert_eq!(delay_params.sync_division, 3);
    }

    /// Guards the one thing that would silently drift: the document defaults
    /// and the engine descriptor defaults must agree, or a new node starts at
    /// values the Inspector never showed.
    #[test]
    fn model_defaults_match_the_engine_descriptors() {
        for kind in DocEffectKind::ALL {
            let params = default_params(*kind);
            for (slot, spec) in descriptor(&params).params.iter().enumerate() {
                let document = value(&params, slot as u8);
                assert!(
                    (document - spec.default).abs() < 1e-6,
                    "{kind:?} slot {slot} ({}): document {document} vs descriptor {}",
                    spec.name,
                    spec.default
                );
            }
        }
    }

    #[test]
    fn processors_are_seeded_from_the_document() {
        for kind in DocEffectKind::ALL {
            let params = default_params(*kind);
            let built = processor(&params, 48_000.0);
            assert_eq!(
                built.meters.is_some(),
                *kind == DocEffectKind::Compressor,
                "{kind:?} meter handle"
            );
        }
    }

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn ctx() -> glitcherz_engine::realtime::ProcessContext {
        glitcherz_engine::realtime::ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: 0,
            song_ticks: 0.0,
            transport_dt_ticks: 0.0,
            transport_active: true,
            sec_per_tick: 60.0 / (120.0 * 960.0),
        }
    }

    /// Renders a mono signal in fixed blocks through a built-in effect.
    fn render(
        processor: &mut dyn glitcherz_engine::realtime::Processor,
        signal: &[f32],
    ) -> Vec<f32> {
        let mut buffer = glitcherz_engine::realtime::AudioBuffer::new(1, BLOCK);
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks(BLOCK) {
            let frames = chunk.len();
            buffer.set_len(frames);
            if let Some(channel) = buffer.channel_mut(0) {
                channel[..frames].copy_from_slice(chunk);
            }
            processor.process(&ctx(), &mut buffer);
            if let Some(channel) = buffer.channel(0) {
                out.extend_from_slice(&channel[..frames]);
            }
        }
        out
    }

    /// End to end: document value → `set_value` → `processor` → DSP response.
    ///
    /// This is the seam a slot-order mistake would break silently — the slider
    /// would move, the document would change, and the sound would not.
    #[test]
    fn document_values_reach_the_dsp() {
        let mut params = default_params(DocEffectKind::Eq);
        set_value(&mut params, eq::SLOT_LOW_GAIN, 12.0);
        let mut builtin = processor(&params, SR);

        let seconds = 1.0;
        let frames = (SR * seconds) as usize;
        let input: Vec<f32> = (0..frames)
            .map(|i| (std::f32::consts::TAU * 25.0 * i as f32 / SR).sin())
            .collect();
        let out = render(builtin.processor.as_mut(), &input);

        let tail = 4_096;
        let rms = |s: &[f32]| (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt();
        let gain_db = 20.0 * (rms(&out[frames - tail..]) / rms(&input[frames - tail..])).log10();
        assert!(
            (10.5..12.5).contains(&gain_db),
            "the document's +12 dB low shelf should boost 25 Hz, got {gain_db:.2} dB"
        );
    }

    #[test]
    fn compressor_document_reaches_the_dsp() {
        let mut params = default_params(DocEffectKind::Compressor);
        set_value(&mut params, compressor::SLOT_KNEE, 0.0);
        set_value(&mut params, compressor::SLOT_ATTACK, 1.0);
        let mut builtin = processor(&params, SR);

        let input = vec![0.5f32; BLOCK * 64];
        let out = render(builtin.processor.as_mut(), &input);
        let tail = out[out.len() - 1];
        // -18 dB threshold, 4:1, 0.5 → -6.02 dBFS ⇒ ~-9 dB GR ⇒ ~0.178.
        assert!(
            (tail / 0.1776 - 1.0).abs() < 0.02,
            "expected the default curve to settle near 0.178, got {tail}"
        );
    }
}
