//! Platform seam for the `vst3` crate's generated enum types.
//!
//! The bindings spell every enum twice: the constant containers
//! (`Vst::MediaTypes`, `Vst::BusDirections`, `Vst::ProcessModes`,
//! `Vst::SymbolicSampleSizes`, `Steinberg::IBStream_::IStreamSeekMode`, ...)
//! are `vst3::support::DefaultEnumType`, which the crate aliases to `c_int`
//! on Windows and `c_uint` everywhere else, while the struct fields and
//! interface parameters that receive those values are declared as `int32`.
//!
//! On Windows both are `i32`, so the values line up on their own; on macOS
//! and Linux they are distinct types and every hand-off has to be converted.
//! Keeping that conversion in one place is what makes the host build on all
//! three desktop targets.
//!
//! Both conversions are total for the values that actually occur: enum
//! discriminants are small non-negative constants, so the checked variants
//! never fail and the fallbacks below are never taken in practice.

use vst3::Steinberg::int32;

/// Converts a generated enum constant to the `int32` slot that consumes it
/// (struct fields, interface arguments).
///
/// The fallback keeps the call sites panic-free — the realtime iron rules
/// forbid panics on the audio path — and costs nothing for valid values.
#[inline]
pub(crate) fn to_int32<T>(value: T) -> int32
where
    T: TryInto<int32>,
{
    value.try_into().unwrap_or(0)
}

/// Converts an `int32` (an interface parameter or a plugin-reported value)
/// back to a generated enum type, for `match` arms over enum constants.
///
/// `T` is inferred from the patterns: with `T` unnameable from outside the
/// `vst3` crate (`DefaultEnumType` lives in a private module) this is the
/// only way to match an incoming `int32` against the constants.
#[inline]
pub(crate) fn to_enum<T>(value: int32) -> T
where
    T: TryFrom<int32> + Default,
{
    T::try_from(value).unwrap_or_default()
}
