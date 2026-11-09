//! The two rendering entry points.
//!
//! `--render <in.glz> <out.wav>` is the one-shot form from S0. `--wav <out>`
//! renders whatever a `--open` + `--ops` batch produced — the *edited*
//! in-memory document, not the file on disk, which is the whole point of
//! having a command surface in front of the renderer.

use std::path::Path;

use glitcherz_ui::export_audio::Bits;
use glitcherz_ui::headless::{RenderOptions, RenderReport, render_project_to_wav};

/// `--rate` / `--bits` / `--to`, shared by both forms.
#[derive(Debug, Clone, Copy)]
pub struct RenderArgs<'a> {
    pub rate: u32,
    pub bits: Option<&'a str>,
    pub end_ticks: Option<u32>,
}

impl RenderArgs<'_> {
    pub fn options(&self) -> Result<RenderOptions, String> {
        Ok(RenderOptions {
            sample_rate: self.rate,
            bits: parse_bits(self.bits)?,
            end_ticks: self.end_ticks,
        })
    }
}

/// Parses `--bits` (`16` / `24` / `32f`, default 24).
pub fn parse_bits(raw: Option<&str>) -> Result<Bits, String> {
    match raw.unwrap_or("24") {
        "16" => Ok(Bits::Int16),
        "24" => Ok(Bits::Int24),
        "32" | "32f" => Ok(Bits::Float32),
        other => Err(format!("Unknown --bits {other} (use 16, 24 or 32f)")),
    }
}

/// Renders a project file straight to a wav (`--render`).
pub fn one_shot(input: &Path, output: &Path, args: RenderArgs<'_>) -> Result<RenderReport, String> {
    render_project_to_wav(input, output, args.options()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_parse_and_reject() {
        assert_eq!(parse_bits(None).unwrap(), Bits::Int24);
        assert_eq!(parse_bits(Some("16")).unwrap(), Bits::Int16);
        assert_eq!(parse_bits(Some("24")).unwrap(), Bits::Int24);
        assert_eq!(parse_bits(Some("32")).unwrap(), Bits::Float32);
        assert_eq!(parse_bits(Some("32f")).unwrap(), Bits::Float32);
        assert!(parse_bits(Some("8")).is_err());
    }

    #[test]
    fn options_carry_the_flags_through() {
        let args = RenderArgs {
            rate: 22_050,
            bits: Some("16"),
            end_ticks: Some(1920),
        };
        let options = args.options().unwrap();
        assert_eq!(options.sample_rate, 22_050);
        assert_eq!(options.bits, Bits::Int16);
        assert_eq!(options.end_ticks, Some(1920));
    }
}
