//! Built-in effect framework: descriptors, parameter smoothing, slot banks.
//!
//! See spec `.agents/spec/effects.md`. The pieces:
//!
//! - [`descriptor`]: static tables ([`ParamSpec`], [`EffectDescriptor`],
//!   [`StereoClass`]) — slot index = array index, the single source of truth
//!   for ranges, defaults, curves, smoothing and stereo capability;
//! - [`smoothed`]: [`Smoothed`], the one-pole smoother every continuous
//!   parameter runs through (linear/log domain, sample/block rate);
//! - [`bank`]: [`EffectParamBank`], the fixed-capacity slot storage a processor
//!   owns; `set` is the only parameter entry point.
//!
//! Effects are addressed by [`ParamId::BuiltinParam`](crate::realtime::commands::ParamId)
//! (slot index) instead of one `ParamId` variant per parameter, so adding an
//! effect never touches the engine's parameter dispatch. Each effect module
//! owns its DSP **and** its descriptor (e.g. [`crate::realtime::delay`]); this
//! module only holds the framework and the roster.

pub mod bank;
pub mod descriptor;
pub mod smoothed;

pub use bank::EffectParamBank;
pub use descriptor::{
    Curve, EffectDescriptor, MAX_EFFECT_PARAMS, ParamFlags, ParamSpec, Rate, StereoClass, Unit,
    validate_descriptor,
};
pub use smoothed::Smoothed;

/// Built-in effect kinds the engine can instantiate.
///
/// Deliberately separate from the model's `EffectParams`: the engine crate does
/// not depend on `glitcherz-model`, and the UI maps one to the other when it
/// installs a node (same pattern as `rt_curve_interp`). Descriptor tables live
/// with the DSP, so the roster is the only place that lists kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinEffectKind {
    /// Feedback delay (the only built-in effect so far).
    Delay,
}

impl BuiltinEffectKind {
    /// Every built-in effect, in menu order.
    pub const ALL: &'static [BuiltinEffectKind] = &[BuiltinEffectKind::Delay];

    /// Static descriptor of this kind.
    pub const fn descriptor(self) -> &'static EffectDescriptor {
        match self {
            BuiltinEffectKind::Delay => &crate::realtime::delay::DESCRIPTOR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BuiltinEffectKind, validate_descriptor};

    #[test]
    fn every_descriptor_is_well_formed() {
        for kind in BuiltinEffectKind::ALL {
            let descriptor = kind.descriptor();
            assert!(
                validate_descriptor(descriptor).is_ok(),
                "{}: {:?}",
                descriptor.name,
                validate_descriptor(descriptor)
            );
            assert!(!descriptor.name.is_empty());
        }
    }

    #[test]
    fn roster_has_no_duplicates() {
        let mut names: Vec<&str> = BuiltinEffectKind::ALL
            .iter()
            .map(|kind| kind.descriptor().name)
            .collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate effect names in the roster");
    }
}
