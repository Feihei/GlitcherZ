//! Static parameter descriptors for built-in effects.
//!
//! Every built-in effect owns exactly one [`EffectDescriptor`]; **slot index =
//! position in [`EffectDescriptor::params`]**. The engine addresses parameters
//! with [`ParamId::BuiltinParam`](crate::realtime::commands::ParamId), so a new
//! effect never adds a `ParamId` variant, and the UI builds its sliders, inline
//! rows and automation targets by walking the same table.
//!
//! Nothing here is real-time: tables are `static`, `ParamSpec` is `Copy` and
//! [`ParamSpec::clamp`] is pure arithmetic (RT-safe if called on the audio
//! thread).

use crate::realtime::effect::smoothed::Smoothed;

/// Maximum parameter count one built-in effect can address.
pub const MAX_EFFECT_PARAMS: usize = 12;

/// Parameter unit; drives UI formatting and the slider label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// Decibels.
    Db,
    /// Hertz.
    Hz,
    /// Seconds (`0.3` = 300 ms). Delay time, pre-delay, reverb decay.
    Seconds,
    /// Milliseconds (`250.0` = 250 ms). Compressor attack / release, where the
    /// interesting range sits below 1 s.
    Ms,
    /// Unitless ratio (Q, compression ratio).
    Ratio,
    /// 0..1 shown as a percentage.
    Percent,
    /// Stereo position (-1 = hard left, +1 = hard right).
    Pan,
    /// Discrete 0 / 1 (on/off).
    Bool,
}

/// Value curve: the UI slider axis **and** the domain the smoother works in.
///
/// [`Curve::Log`] requires `min > 0` (enforced by [`validate_descriptor`]) and
/// makes equal ratios feel like equal moves: 20 → 40 Hz takes as long as
/// 10 → 20 kHz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Curve {
    /// Linear domain.
    Linear,
    /// Logarithmic domain (`ln` while smoothing, `exp` on read).
    Log,
}

/// How often a parameter's smoothed value is consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rate {
    /// Advanced once per sample (gains, mix, feedback).
    Sample,
    /// Advanced once per block (coefficients: EQ frequencies, compressor
    /// thresholds, reverb decay).
    Block,
}

/// Optional parameter properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ParamFlags(u8);

impl ParamFlags {
    /// No flags.
    pub const NONE: Self = Self(0);
    /// Eligible as an automation target (continuous parameters only).
    pub const AUTOMATABLE: Self = Self(1 << 0);
    /// The stereo-link switch of a detector-based effect (D13).
    pub const STEREO_LINK: Self = Self(1 << 1);
    /// Gets a row on the patch node's inline strip. Effects with more than
    /// three parameters mark only the handful that shape the sound most, so
    /// the canvas node does not grow into a column of slider rows; the
    /// Inspector always shows every slot.
    pub const INLINE: Self = Self(1 << 2);

    /// Whether `flag` is set.
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }

    /// Returns `self` with `flag` added.
    pub const fn with(self, flag: Self) -> Self {
        Self(self.0 | flag.0)
    }
}

/// One parameter's static metadata.
#[derive(Debug, Clone, Copy)]
pub struct ParamSpec {
    /// English UI label (`"time"`, `"mix"`, `"threshold"`).
    pub name: &'static str,
    /// Compact label for the patch inline row (defaults to `name`).
    pub short: &'static str,
    /// Display unit.
    pub unit: Unit,
    /// Lower bound (inclusive).
    pub min: f32,
    /// Upper bound (inclusive).
    pub max: f32,
    /// Value a freshly built node starts with.
    pub default: f32,
    /// Slider axis and smoothing domain.
    pub curve: Curve,
    /// One-pole time constant in milliseconds; `0.0` = snap (discrete).
    pub smoothing_ms: f32,
    /// Consumption rate.
    pub rate: Rate,
    /// Optional properties.
    pub flags: ParamFlags,
}

impl ParamSpec {
    /// Gain-like continuous parameter: linear domain, per-sample, 10 ms.
    pub const fn gain(name: &'static str, unit: Unit, min: f32, max: f32, default: f32) -> Self {
        Self {
            name,
            short: name,
            unit,
            min,
            max,
            default,
            curve: Curve::Linear,
            smoothing_ms: 10.0,
            rate: Rate::Sample,
            flags: ParamFlags::AUTOMATABLE,
        }
    }

    /// Stereo position: linear domain, per-sample, 15 ms.
    pub const fn pan(name: &'static str, min: f32, max: f32, default: f32) -> Self {
        Self {
            name,
            short: name,
            unit: Unit::Pan,
            min,
            max,
            default,
            curve: Curve::Linear,
            smoothing_ms: 15.0,
            rate: Rate::Sample,
            flags: ParamFlags::AUTOMATABLE,
        }
    }

    /// Overrides the compact inline label.
    pub const fn short(mut self, short: &'static str) -> Self {
        self.short = short;
        self
    }

    /// Marks this parameter for the patch node's inline strip.
    pub const fn inline_row(mut self) -> Self {
        self.flags = self.flags.with(ParamFlags::INLINE);
        self
    }

    /// dB level consumed at block rate (compressor threshold, knee).
    ///
    /// Distinct from [`ParamSpec::gain`], which is a per-sample gain stage:
    /// these feed a curve or a coefficient and only need one update per block.
    pub const fn level(name: &'static str, min: f32, max: f32, default: f32) -> Self {
        Self {
            name,
            short: name,
            unit: Unit::Db,
            min,
            max,
            default,
            curve: Curve::Linear,
            smoothing_ms: 15.0,
            rate: Rate::Block,
            flags: ParamFlags::AUTOMATABLE,
        }
    }

    /// Frequency: log domain, per-block, 20 ms.
    pub const fn freq(name: &'static str, min: f32, max: f32, default: f32) -> Self {
        Self {
            name,
            short: name,
            unit: Unit::Hz,
            min,
            max,
            default,
            curve: Curve::Log,
            smoothing_ms: 20.0,
            rate: Rate::Block,
            flags: ParamFlags::AUTOMATABLE,
        }
    }

    /// Time constant in milliseconds: log domain, per-block, 20 ms.
    ///
    /// Use this when the stored value **is** milliseconds (compressor attack /
    /// release). Anything measured in seconds — delay time, pre-delay — uses
    /// [`ParamSpec::seconds`].
    pub const fn time(name: &'static str, min: f32, max: f32, default: f32) -> Self {
        Self {
            name,
            short: name,
            unit: Unit::Ms,
            min,
            max,
            default,
            curve: Curve::Log,
            smoothing_ms: 20.0,
            rate: Rate::Block,
            flags: ParamFlags::AUTOMATABLE,
        }
    }

    /// Time in **seconds**: log domain, per-block, 20 ms.
    pub const fn seconds(name: &'static str, min: f32, max: f32, default: f32) -> Self {
        Self {
            name,
            short: name,
            unit: Unit::Seconds,
            min,
            max,
            default,
            curve: Curve::Log,
            smoothing_ms: 20.0,
            rate: Rate::Block,
            flags: ParamFlags::AUTOMATABLE,
        }
    }

    /// Unitless ratio: log domain, per-block, 20 ms.
    pub const fn ratio(name: &'static str, min: f32, max: f32, default: f32) -> Self {
        Self {
            name,
            short: name,
            unit: Unit::Ratio,
            min,
            max,
            default,
            curve: Curve::Log,
            smoothing_ms: 20.0,
            rate: Rate::Block,
            flags: ParamFlags::AUTOMATABLE,
        }
    }

    /// Discrete switch (0 / 1): snaps at the block boundary.
    pub const fn switch(name: &'static str, default: f32) -> Self {
        Self {
            name,
            short: name,
            unit: Unit::Bool,
            min: 0.0,
            max: 1.0,
            default,
            curve: Curve::Linear,
            smoothing_ms: 0.0,
            rate: Rate::Block,
            flags: ParamFlags::NONE,
        }
    }

    /// Discrete switch carrying extra flags (e.g. [`ParamFlags::STEREO_LINK`]).
    pub const fn switch_with(name: &'static str, default: f32, flags: ParamFlags) -> Self {
        let mut spec = Self::switch(name, default);
        spec.flags = flags;
        spec
    }

    /// Whether this parameter is discrete (no interpolation).
    pub const fn is_discrete(&self) -> bool {
        matches!(self.unit, Unit::Bool) || self.smoothing_ms <= 0.0
    }

    /// Whether this parameter may appear in the automation target list.
    pub const fn is_automatable(&self) -> bool {
        !self.is_discrete() && self.flags.contains(ParamFlags::AUTOMATABLE)
    }

    /// Clamps `value` into `[min, max]`; non-finite input falls back to
    /// `default` (RT-safe, no panic).
    pub fn clamp(&self, value: f32) -> f32 {
        if value.is_finite() {
            value.clamp(self.min, self.max)
        } else {
            self.default
        }
    }

    /// Builds the smoother this spec describes (construction time only).
    pub fn smoothed(&self, sample_rate: f32) -> Smoothed {
        Smoothed::new(self, sample_rate)
    }
}

/// Stereo capability of an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StereoClass {
    /// One parameter set drives both channels identically.
    Mono,
    /// Detector-based effect: per-channel state exists, but the instance can
    /// lock both channels together (`max(|L|, |R|)` → one gain reduction).
    Linkable {
        /// Whether a freshly built node starts linked.
        default_link: bool,
    },
}

/// Static description of one built-in effect.
#[derive(Debug)]
pub struct EffectDescriptor {
    /// Human-readable name (`"Delay"`).
    pub name: &'static str,
    /// Parameter table; **the slot index is the array index**.
    pub params: &'static [ParamSpec],
    /// Stereo capability.
    pub stereo: StereoClass,
}

impl EffectDescriptor {
    /// Spec for `slot`, or `None` when out of range (never panics).
    pub fn spec(&self, slot: u8) -> Option<&ParamSpec> {
        self.params.get(slot as usize)
    }

    /// Whether this effect exposes a stereo-link switch.
    pub fn is_linkable(&self) -> bool {
        matches!(self.stereo, StereoClass::Linkable { .. })
    }

    /// Whether a freshly built node starts linked (false when not linkable).
    pub fn default_link(&self) -> bool {
        match self.stereo {
            StereoClass::Linkable { default_link } => default_link,
            StereoClass::Mono => false,
        }
    }

    /// Default slot of the stereo-link switch, if any.
    pub fn link_slot(&self) -> Option<u8> {
        self.params
            .iter()
            .position(|spec| spec.flags.contains(ParamFlags::STEREO_LINK))
            .map(|index| index as u8)
    }

    /// Slots marked [`ParamFlags::INLINE`], in table order (patch node rows).
    pub fn inline_slots(&self) -> impl Iterator<Item = u8> + '_ {
        self.params
            .iter()
            .enumerate()
            .filter(|(_, spec)| spec.flags.contains(ParamFlags::INLINE))
            .map(|(index, _)| index as u8)
    }
}

/// Validates a descriptor table; only used by tests and debug assertions.
///
/// Rules (spec `.agents/spec/effects.md` §10):
/// - at most [`MAX_EFFECT_PARAMS`] parameters, at least one;
/// - `min < max` and `default` inside the range;
/// - `Curve::Log` requires `min > 0`;
/// - `Unit::Bool` ⇒ `smoothing_ms == 0`; continuous ⇒ `smoothing_ms > 0`;
/// - parameter names are unique and non-empty.
pub fn validate_descriptor(descriptor: &EffectDescriptor) -> Result<(), String> {
    let params = descriptor.params;
    if params.is_empty() {
        return Err(format!("{}: no parameters", descriptor.name));
    }
    if params.len() > MAX_EFFECT_PARAMS {
        return Err(format!(
            "{}: {} parameters exceed MAX_EFFECT_PARAMS ({MAX_EFFECT_PARAMS})",
            descriptor.name,
            params.len()
        ));
    }
    for (slot, spec) in params.iter().enumerate() {
        if spec.name.is_empty() || spec.short.is_empty() {
            return Err(format!(
                "{}: slot {slot} has an empty name or short label",
                descriptor.name
            ));
        }
        if spec.min >= spec.max {
            return Err(format!(
                "{}: slot {slot} ({}) has min >= max",
                descriptor.name, spec.name
            ));
        }
        if !(spec.min..=spec.max).contains(&spec.default) {
            return Err(format!(
                "{}: slot {slot} ({}) default {} is outside min..max",
                descriptor.name, spec.name, spec.default
            ));
        }
        if spec.curve == Curve::Log && spec.min <= 0.0 {
            return Err(format!(
                "{}: slot {slot} ({}) is logarithmic but min <= 0",
                descriptor.name, spec.name
            ));
        }
        let discrete = spec.is_discrete();
        if spec.unit == Unit::Bool && !discrete {
            return Err(format!(
                "{}: slot {slot} ({}) is a switch but has smoothing",
                descriptor.name, spec.name
            ));
        }
        if spec.unit != Unit::Bool && discrete {
            return Err(format!(
                "{}: slot {slot} ({}) is continuous but has no smoothing",
                descriptor.name, spec.name
            ));
        }
    }
    for (index, spec) in params.iter().enumerate() {
        if params[..index].iter().any(|other| other.name == spec.name) {
            return Err(format!(
                "{}: duplicate parameter name {}",
                descriptor.name, spec.name
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Curve, ParamFlags, ParamSpec, StereoClass, Unit, validate_descriptor};
    use crate::realtime::effect::EffectDescriptor;

    const OK_PARAMS: [ParamSpec; 2] = [
        ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 0.0),
        ParamSpec::switch_with("link", 1.0, ParamFlags::STEREO_LINK),
    ];

    fn descriptor(params: &'static [ParamSpec]) -> EffectDescriptor {
        EffectDescriptor {
            name: "Test",
            params,
            stereo: StereoClass::Mono,
        }
    }

    #[test]
    fn well_formed_descriptor_passes() {
        assert_eq!(validate_descriptor(&descriptor(&OK_PARAMS)), Ok(()));
    }

    #[test]
    fn continuous_parameter_requires_smoothing() {
        let mut spec = ParamSpec::freq("cutoff", 20.0, 20_000.0, 1000.0);
        spec.smoothing_ms = 0.0;
        let params: &'static [ParamSpec] = Box::leak(vec![spec].into_boxed_slice());
        assert!(validate_descriptor(&descriptor(params)).is_err());
    }

    #[test]
    fn log_parameter_requires_positive_min() {
        let spec = ParamSpec::freq("cutoff", 0.0, 20_000.0, 1000.0);
        let params: &'static [ParamSpec] = Box::leak(vec![spec].into_boxed_slice());
        assert!(validate_descriptor(&descriptor(params)).is_err());
        assert_eq!(spec.curve, Curve::Log);
    }

    #[test]
    fn default_must_be_inside_range() {
        let spec = ParamSpec::gain("gain", Unit::Db, -18.0, 18.0, 24.0);
        let params: &'static [ParamSpec] = Box::leak(vec![spec].into_boxed_slice());
        assert!(validate_descriptor(&descriptor(params)).is_err());
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let params = [
            ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 0.0),
            ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 0.0),
        ];
        assert!(validate_descriptor(&descriptor(Box::leak(Box::new(params)))).is_err());
    }

    #[test]
    fn link_slot_and_default_link() {
        let d = EffectDescriptor {
            name: "Test",
            params: &OK_PARAMS,
            stereo: StereoClass::Linkable { default_link: true },
        };
        assert!(d.is_linkable());
        assert!(d.default_link());
        assert_eq!(d.link_slot(), Some(1));
        assert_eq!(d.spec(0).map(|s| s.name), Some("mix"));
        assert!(d.spec(2).is_none());
    }

    #[test]
    fn clamp_handles_non_finite_input() {
        let spec = ParamSpec::gain("mix", Unit::Percent, 0.0, 1.0, 0.25);
        assert_eq!(spec.clamp(f32::NAN), 0.25);
        assert_eq!(spec.clamp(2.0), 1.0);
        assert_eq!(spec.clamp(-1.0), 0.0);
        assert!(!spec.is_discrete());
        assert!(spec.is_automatable());
    }
}
