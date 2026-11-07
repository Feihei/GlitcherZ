#![forbid(unsafe_code)]

//! GlitcherZ desktop app entry point: starts the audio engine (glitcherz-engine)
//! and opens the main window (glitcherz-ui).
//!
//! The engine starts silent. `--autoplay` deliberately holds one note so a
//! smoke run can prove the audio path end to end without touching the UI.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use glitcherz_engine::AudioEngine;
use glitcherz_ui::headless::RenderOptions;

/// Command line arguments (parsed manually, keeping dependencies minimal).
struct Args {
    /// MIDI note auto-triggered at startup (69 = A4).
    note: u8,
    /// Hold a note at startup (diagnostic; the default is silent).
    autoplay: bool,
    /// Project file loaded at startup.
    open: Option<PathBuf>,
    gain: f64,
    seconds: Option<f64>,
    list_devices: bool,
    help: bool,
    /// Headless render: input project + output wav (S0).
    render: Option<(PathBuf, PathBuf)>,
    render_rate: u32,
    render_bits: Option<String>,
    render_to: Option<u32>,
    /// Argument parsing error (reported before anything starts).
    error: Option<String>,
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = parse_args(std::env::args().skip(1));
    if let Some(err) = &args.error {
        eprintln!("{err}");
        print_usage();
        return ExitCode::FAILURE;
    }
    if args.help {
        print_usage();
        return ExitCode::SUCCESS;
    }
    if args.list_devices {
        println!("Available output devices:");
        for name in glitcherz_engine::available_outputs() {
            println!("  - {name}");
        }
        return ExitCode::SUCCESS;
    }
    if let Some((input, output)) = args.render {
        return render_command(
            &input,
            &output,
            args.render_rate,
            args.render_bits.as_deref(),
            args.render_to,
        );
    }

    let engine = match AudioEngine::start() {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("Failed to start the audio engine: {err}");
            return ExitCode::FAILURE;
        }
    };
    let info = engine.info().clone();
    println!(
        "Output device: {} ({} Hz, {} channels)",
        info.device_name, info.sample_rate, info.out_channels
    );

    let ui_config = glitcherz_ui::UiConfig {
        gain: args.gain,
        auto_play_note: args.autoplay.then_some(args.note),
        auto_close_after: args.seconds.map(|s| Duration::from_secs_f64(s.max(0.0))),
        open_path: args.open,
    };

    if let Err(err) = glitcherz_ui::run(engine, ui_config) {
        eprintln!("Main window exited abnormally: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Headless render (S0): no window, no output device, no eframe event loop.
fn render_command(
    input: &Path,
    output: &Path,
    rate: u32,
    bits: Option<&str>,
    end_ticks: Option<u32>,
) -> ExitCode {
    use glitcherz_ui::export_audio::Bits;

    let bits = match bits.unwrap_or("24") {
        "16" => Bits::Int16,
        "24" => Bits::Int24,
        "32" | "32f" => Bits::Float32,
        other => {
            eprintln!("Unknown --bits {other} (use 16, 24 or 32f)");
            return ExitCode::FAILURE;
        }
    };
    let options = RenderOptions {
        sample_rate: rate,
        bits,
        end_ticks,
    };
    match glitcherz_ui::headless::render_project_to_wav(input, output, options) {
        Ok(report) => {
            println!(
                "Rendered {} frames at {} Hz to {}",
                report.frames,
                report.sample_rate,
                output.display()
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("Render failed: {err}");
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    println!(
        "Usage: glitcherz [--open <project>] [--autoplay [--note <0-127>]] [--gain <0.0-1.0>] \
         [--seconds <secs>] [--list-devices]\n\
         Usage: glitcherz --render <in.glz> <out.wav> [--rate <hz>] [--bits 16|24|32f] [--to <ticks>]\n\
         Defaults: opens the main window and stays silent.\n\
         Play with the on-screen keyboard or the computer keyboard (A W S E D ..., Z/X to shift octaves); close the window to quit.\n\
         --autoplay holds one note from startup (diagnostic: proves the audio path);\n\
         --seconds auto-closes the window after N seconds (for smoke tests);\n\
         --render bounces a project to a stereo wav without opening a window (44.1 kHz / 24-bit by default)."
    );
}

fn parse_args(raw: impl Iterator<Item = String>) -> Args {
    let mut args = Args {
        note: 69,
        autoplay: false,
        open: None,
        gain: 0.8,
        seconds: None,
        list_devices: false,
        help: false,
        render: None,
        render_rate: 44_100,
        render_bits: None,
        render_to: None,
        error: None,
    };
    let mut it = raw;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--note" => {
                if let Some(v) = it.next().and_then(|s| s.parse::<i32>().ok()) {
                    args.note = v.clamp(0, 127) as u8;
                    // Naming a note is an explicit request to hear it.
                    args.autoplay = true;
                }
            }
            "--freq" => {
                // Legacy flag compatibility: convert the frequency to the
                // nearest MIDI note.
                if let Some(f) = it.next().and_then(|s| s.parse::<f64>().ok()) {
                    if f.is_finite() && f > 0.0 {
                        let note = (69.0 + 12.0 * (f / 440.0).log2()).round() as i32;
                        args.note = note.clamp(0, 127) as u8;
                        args.autoplay = true;
                    }
                }
            }
            "--autoplay" => args.autoplay = true,
            // Compatibility alias from the days when autoplay was the default.
            "--no-autoplay" => args.autoplay = false,
            "--open" => {
                if let Some(v) = it.next() {
                    args.open = Some(PathBuf::from(v));
                }
            }
            "--gain" => {
                if let Some(v) = it.next().and_then(|s| s.parse().ok()) {
                    args.gain = v;
                }
            }
            "--seconds" => {
                if let Some(v) = it.next().and_then(|s| s.parse().ok()) {
                    args.seconds = Some(v);
                }
            }
            "--list-devices" => args.list_devices = true,
            "--render" => match (it.next(), it.next()) {
                (Some(input), Some(output)) => {
                    args.render = Some((PathBuf::from(input), PathBuf::from(output)));
                }
                _ => args.error = Some("--render needs <in.glz> <out.wav>".to_string()),
            },
            "--rate" => {
                if let Some(v) = it.next().and_then(|s| s.parse::<u32>().ok()) {
                    args.render_rate = v;
                } else {
                    args.error = Some("--rate needs a positive sample rate".to_string());
                }
            }
            "--bits" => args.render_bits = it.next(),
            "--to" => {
                if let Some(v) = it.next().and_then(|s| s.parse::<u32>().ok()) {
                    args.render_to = Some(v);
                } else {
                    args.error = Some("--to needs a tick count".to_string());
                }
            }
            "--help" | "-h" => args.help = true,
            other => eprintln!("Ignoring unknown argument: {other} (see --help for usage)"),
        }
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_args_capture_paths_and_options() {
        let raw = [
            "--render", "song.glz", "out.wav", "--rate", "48000", "--bits", "16", "--to", "3840",
        ];
        let args = parse_args(raw.into_iter().map(String::from));
        assert!(args.error.is_none());
        assert_eq!(
            args.render,
            Some((PathBuf::from("song.glz"), PathBuf::from("out.wav")))
        );
        assert_eq!(args.render_rate, 48_000);
        assert_eq!(args.render_bits.as_deref(), Some("16"));
        assert_eq!(args.render_to, Some(3840));
    }

    #[test]
    fn render_needs_both_paths() {
        let args = parse_args(["--render", "song.glz"].into_iter().map(String::from));
        assert!(args.error.is_some());
        assert!(args.render.is_none());
    }
}
