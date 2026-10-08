#![forbid(unsafe_code)]

//! ResoZ desktop app entry point: starts the audio engine (resoz-engine) and
//! opens the main window (resoz-ui). Currently roadmap step 3 -- full
//! synthesizer (ADSR envelope + SVF filter) and virtual keyboard.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use resoz_engine::AudioEngine;

/// Command line arguments (parsed manually, keeping dependencies minimal).
struct Args {
    /// MIDI note auto-triggered at startup (69 = A4).
    note: u8,
    /// Start silent (do not auto-trigger a note).
    no_autoplay: bool,
    /// Project file loaded at startup.
    open: Option<PathBuf>,
    gain: f64,
    seconds: Option<f64>,
    list_devices: bool,
    help: bool,
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = parse_args(std::env::args().skip(1));
    if args.help {
        print_usage();
        return ExitCode::SUCCESS;
    }
    if args.list_devices {
        println!("Available output devices:");
        for name in resoz_engine::available_outputs() {
            println!("  - {name}");
        }
        return ExitCode::SUCCESS;
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

    let ui_config = resoz_ui::UiConfig {
        gain: args.gain,
        auto_play_note: (!args.no_autoplay).then_some(args.note),
        auto_close_after: args.seconds.map(|s| Duration::from_secs_f64(s.max(0.0))),
        open_path: args.open,
    };

    if let Err(err) = resoz_ui::run(engine, ui_config) {
        eprintln!("Main window exited abnormally: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn print_usage() {
    println!(
        "Usage: resoz [--note <0-127>] [--no-autoplay] [--gain <0.0-1.0>] [--seconds <secs>] [--list-devices]\n\
         Defaults: opens the main window and auto-triggers an A4 note (sustained tone).\n\
         Play with the on-screen keyboard or the computer keyboard (A W S E D ..., Z/X to shift octaves); close the window to quit.\n\
         --seconds auto-closes the window after N seconds (for smoke tests)."
    );
}

fn parse_args(raw: impl Iterator<Item = String>) -> Args {
    let mut args = Args {
        note: 69,
        no_autoplay: false,
        open: None,
        gain: 0.8,
        seconds: None,
        list_devices: false,
        help: false,
    };
    let mut it = raw;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--note" => {
                if let Some(v) = it.next().and_then(|s| s.parse::<i32>().ok()) {
                    args.note = v.clamp(0, 127) as u8;
                }
            }
            "--freq" => {
                // Legacy flag compatibility: convert the frequency to the
                // nearest MIDI note.
                if let Some(f) = it.next().and_then(|s| s.parse::<f64>().ok()) {
                    if f.is_finite() && f > 0.0 {
                        let note = (69.0 + 12.0 * (f / 440.0).log2()).round() as i32;
                        args.note = note.clamp(0, 127) as u8;
                    }
                }
            }
            "--no-autoplay" => args.no_autoplay = true,
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
            "--help" | "-h" => args.help = true,
            other => eprintln!("Ignoring unknown argument: {other} (see --help for usage)"),
        }
    }
    args
}
