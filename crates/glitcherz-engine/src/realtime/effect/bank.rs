//! Fixed-capacity runtime parameter bank for built-in effects.
//!
//! One bank per effect processor instance. `set` is the single entry point for
//! every `ParamId::BuiltinParam(slot)` update, so effects never write their own
//! parameter dispatch: a processor only reads slots (`tick` / `advance` /
//! `value`) and the descriptor table decides ranges, smoothing and rate.

use crate::realtime::effect::EffectDescriptor;
use crate::realtime::effect::descriptor::{MAX_EFFECT_PARAMS, ParamSpec};
use crate::realtime::effect::smoothed::Smoothed;

/// Slot-addressed parameter storage.
pub struct EffectParamBank {
    descriptor: &'static EffectDescriptor,
    slots: [Smoothed; MAX_EFFECT_PARAMS],
}

impl EffectParamBank {
    /// Builds a bank snapped to every spec's default. Construction time only.
    pub fn new(descriptor: &'static EffectDescriptor, sample_rate: f32) -> Self {
        let slots = std::array::from_fn(|index| match descriptor.params.get(index) {
            Some(spec) => Smoothed::new(spec, sample_rate),
            None => Smoothed::inert(),
        });
        Self { descriptor, slots }
    }

    /// The effect's static description. RT-safe.
    pub fn descriptor(&self) -> &'static EffectDescriptor {
        self.descriptor
    }

    /// Sets a slot's target. Returns `false` for an out-of-range slot (the
    /// caller may then report the parameter as unrecognized). RT-safe.
    pub fn set(&mut self, slot: u8, value: f32) -> bool {
        match self.slots.get_mut(slot as usize) {
            Some(smoothed) if (slot as usize) < self.descriptor.params.len() => {
                smoothed.set(value);
                true
            }
            _ => false,
        }
    }

    /// Jumps a slot to `value` immediately. RT-safe.
    pub fn snap(&mut self, slot: u8, value: f32) -> bool {
        match self.slots.get_mut(slot as usize) {
            Some(smoothed) if (slot as usize) < self.descriptor.params.len() => {
                smoothed.snap(value);
                true
            }
            _ => false,
        }
    }

    /// Current (non-advanced) value of a slot; `0.0` when out of range.
    /// RT-safe.
    pub fn value(&self, slot: u8) -> f32 {
        match self.slots.get(slot as usize) {
            Some(smoothed) if (slot as usize) < self.descriptor.params.len() => smoothed.value(),
            _ => 0.0,
        }
    }

    /// Advances one slot by one sample and returns it. RT-safe.
    pub fn tick(&mut self, slot: u8) -> f32 {
        match self.slots.get_mut(slot as usize) {
            Some(smoothed) if (slot as usize) < self.descriptor.params.len() => smoothed.tick(),
            _ => 0.0,
        }
    }

    /// Advances one slot by a whole block and returns it (block-rate
    /// consumption). RT-safe.
    pub fn advance(&mut self, slot: u8, frames: usize) -> f32 {
        match self.slots.get_mut(slot as usize) {
            Some(smoothed) if (slot as usize) < self.descriptor.params.len() => {
                smoothed.advance(frames)
            }
            _ => 0.0,
        }
    }

    /// Whether a discrete slot reads as on (`>= 0.5`). RT-safe.
    pub fn is_on(&self, slot: u8) -> bool {
        self.value(slot) >= 0.5
    }

    /// Spec of a slot, if it exists.
    pub fn spec(&self, slot: u8) -> Option<&'static ParamSpec> {
        self.descriptor.spec(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::EffectParamBank;
    use crate::realtime::effect::descriptor::{
        EffectDescriptor, ParamFlags, ParamSpec, StereoClass, Unit,
    };

    const SR: f32 = 48_000.0;

    static PARAMS: [ParamSpec; 3] = [
        ParamSpec::time("time", 0.005, 1.0, 0.3),
        ParamSpec::gain("feedback", Unit::Percent, 0.0, 0.9, 0.35),
        ParamSpec::switch_with("link", 1.0, ParamFlags::STEREO_LINK),
    ];

    static DESCRIPTOR: EffectDescriptor = EffectDescriptor {
        name: "Test",
        params: &PARAMS,
        stereo: StereoClass::Linkable { default_link: true },
    };

    #[test]
    fn defaults_come_from_the_descriptor() {
        let bank = EffectParamBank::new(&DESCRIPTOR, SR);
        assert!((bank.value(0) - 0.3).abs() < 1e-6);
        assert!((bank.value(1) - 0.35).abs() < 1e-6);
        assert!(bank.is_on(2));
        assert_eq!(bank.descriptor().name, "Test");
        assert_eq!(bank.spec(0).map(|s| s.name), Some("time"));
    }

    #[test]
    fn out_of_range_slots_are_ignored() {
        let mut bank = EffectParamBank::new(&DESCRIPTOR, SR);
        assert!(!bank.set(3, 1.0), "slot past the table must be rejected");
        assert!(!bank.set(200, 1.0));
        assert_eq!(bank.value(3), 0.0);
        assert_eq!(bank.tick(3), 0.0);
        assert_eq!(bank.advance(3, 16), 0.0);
        assert!(!bank.is_on(3));
        assert!(bank.spec(3).is_none());
    }

    #[test]
    fn set_clamps_to_the_spec_range() {
        let mut bank = EffectParamBank::new(&DESCRIPTOR, SR);
        assert!(bank.set(1, 4.0));
        assert!((bank.value(1) - 0.9).abs() < 1e-6);
        assert!(bank.set(1, -1.0));
        assert_eq!(bank.value(1), 0.0);
        assert!(bank.set(1, f32::NAN));
        assert_eq!(bank.value(1), 0.0, "non-finite values are ignored");
    }

    #[test]
    fn pre_start_sets_snap_and_later_sets_ramp() {
        let mut bank = EffectParamBank::new(&DESCRIPTOR, SR);
        bank.set(1, 0.8);
        assert!((bank.value(1) - 0.8).abs() < 1e-6, "pre-start set snaps");
        bank.tick(1); // started
        bank.set(1, 0.0);
        let first = bank.tick(1);
        assert!(first > 0.0 && first < 0.8, "runtime set ramps, got {first}");
    }
}
