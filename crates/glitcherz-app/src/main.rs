#![forbid(unsafe_code)]

//! GlitcherZ desktop app entry point: starts the audio engine (glitcherz-engine)
//! and opens the main window (glitcherz-ui).
//!
//! The engine starts silent. `--autoplay` deliberately holds one note so a
//! smoke run can prove the audio path end to end without touching the UI.
//!
//! Four headless modes share this binary (S0 + S4 + S5, specs
//! `cli-commands.md` and `mcp-server.md`): `--render` bounces a project to a
//! wav, `--ops` / `--do` run a batch of registry commands against a project,
//! `--commands` enumerates that registry, and `--mcp` serves the same command
//! surface to an MCP client over stdio. None of them create a window or an
//! audio device.

mod args;
mod batch;
mod mcp;
mod render_cmd;

use std::process::ExitCode;
use std::time::Duration;

use glitcherz_command::Session;
use glitcherz_engine::AudioEngine;
use glitcherz_model::Project;

use args::{Args, Mode};

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = args::parse(std::env::args().skip(1));
    if let Some(err) = &args.error {
        eprintln!("{err}");
        print_usage();
        return ExitCode::from(2);
    }
    match args.mode() {
        Err(err) => {
            eprintln!("{err}");
            print_usage();
            ExitCode::from(2)
        }
        Ok(Mode::Help) => {
            print_usage();
            ExitCode::SUCCESS
        }
        Ok(Mode::ListDevices) => {
            println!("Available output devices:");
            for name in glitcherz_engine::available_outputs() {
                println!("  - {name}");
            }
            ExitCode::SUCCESS
        }
        Ok(Mode::Commands) => commands_command(&args),
        Ok(Mode::Render { input, output }) => render_command(&args, &input, &output),
        Ok(Mode::Batch) => batch_command(&args),
        Ok(Mode::Mcp) => mcp_command(&args),
        Ok(Mode::Gui) => gui(&args),
    }
}

/// The window (the only mode that opens one).
fn gui(args: &Args) -> ExitCode {
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
        open_path: args.open.clone(),
    };

    if let Err(err) = glitcherz_ui::run(engine, ui_config) {
        eprintln!("Main window exited abnormally: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn render_args(args: &Args) -> render_cmd::RenderArgs<'_> {
    render_cmd::RenderArgs {
        rate: args.render_rate,
        bits: args.render_bits.as_deref(),
        end_ticks: args.render_to,
    }
}

/// Headless one-shot render (S0): no window, no output device, no event loop.
fn render_command(args: &Args, input: &std::path::Path, output: &std::path::Path) -> ExitCode {
    match render_cmd::one_shot(input, output, render_args(args)) {
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
            ExitCode::from(3)
        }
    }
}

/// Enumerates the command registry (S4).
///
/// `enabled` is resolved against `--open` (or a default project), so the
/// answer includes *why* a command is unavailable right now.
fn commands_command(args: &Args) -> ExitCode {
    let project = match starting_project(args) {
        Ok(project) => project,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(3);
        }
    };
    let session = Session::new(project);
    let infos: Vec<glitcherz_command::CommandInfo> = glitcherz_command::command_specs()
        .iter()
        .map(|spec| spec.info(&session))
        .collect();
    if args.json {
        // `CommandInfo` is already `Serialize`; going through a `Value` keeps
        // this file free of a direct `serde` dependency.
        let registry = serde_json::to_value(&infos).unwrap_or(serde_json::Value::Null);
        println!("{}", to_json(&registry));
        return ExitCode::SUCCESS;
    }
    for info in &infos {
        println!("{:<22} {:<18} {}", info.id, info.label, info.params);
    }
    println!(
        "{} commands; --json adds menu, shortcut and why a command is unavailable",
        infos.len()
    );
    ExitCode::SUCCESS
}

/// Loads `--open`, or starts from a default document (S4-D4).
fn starting_project(args: &Args) -> Result<Project, String> {
    match &args.open {
        Some(path) => glitcherz_model::file::load_project(path)
            .map_err(|err| format!("{}: {err}", path.display())),
        None => Ok(Project::default()),
    }
}

/// Runs a batch of registry commands, then optionally saves and renders (S4).
fn batch_command(args: &Args) -> ExitCode {
    let mut report = batch::Report::default();

    // `--ops` is resolved first: an unreadable or malformed batch is an input
    // problem, and it must be reported before anything touches the project.
    let steps = match batch::resolve(&args.steps) {
        Ok(steps) => steps,
        Err(failure) => {
            report.failure = Some(failure);
            return finish_batch(args, &report);
        }
    };

    let project = match &args.open {
        Some(path) => match glitcherz_model::file::load_project(path) {
            Ok(project) => {
                report.project = Some(path.display().to_string());
                project
            }
            Err(err) => {
                report.failure = Some(batch::Failure::Load(format!("{}: {err}", path.display())));
                return finish_batch(args, &report);
            }
        },
        None => Project::default(),
    };

    let mut session = Session::new(project);
    let (outcomes, failure) = batch::run(&mut session, &steps);
    report.outcomes = outcomes;
    if let Some(failure) = failure {
        report.failure = Some(failure);
        return finish_batch(args, &report);
    }

    if let Some(path) = &args.save {
        match glitcherz_model::file::save_project(session.project(), path) {
            Ok(()) => report.saved = Some(path.display().to_string()),
            Err(err) => {
                report.failure = Some(batch::Failure::Save(format!("{}: {err}", path.display())));
                return finish_batch(args, &report);
            }
        }
    }

    if let Some(path) = &args.wav {
        let options = match render_args(args).options() {
            Ok(options) => options,
            Err(err) => {
                report.failure = Some(batch::Failure::Render(err));
                return finish_batch(args, &report);
            }
        };
        // The *edited* document, not the one on disk (S4-D6).
        match glitcherz_ui::headless::render_doc_to_wav(session.project(), path, options) {
            Ok(rendered) => {
                report.render = Some(batch::RenderInfo {
                    path: path.display().to_string(),
                    frames: rendered.frames,
                    sample_rate: rendered.sample_rate,
                })
            }
            Err(err) => {
                report.failure = Some(batch::Failure::Render(format!("{}: {err}", path.display())));
            }
        }
    }

    finish_batch(args, &report)
}

/// Prints the report and maps the failure to an exit code.
fn finish_batch(args: &Args, report: &batch::Report) -> ExitCode {
    if args.json {
        println!("{}", to_json(&report.to_json()));
        // stdout stays pure JSON; a human reading the terminal still needs the
        // message somewhere.
        if let Some(failure) = &report.failure {
            eprintln!("{}", failure_message(failure));
        }
    } else {
        print!("{}", report.to_text());
    }
    match &report.failure {
        None => ExitCode::SUCCESS,
        Some(failure) => ExitCode::from(failure.exit_code()),
    }
}

/// Serves the MCP protocol on stdio (S5).
fn mcp_command(args: &Args) -> ExitCode {
    let root = args
        .root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    ExitCode::from(mcp::run(&root, args.allow_plugins, args.open.as_deref()))
}

fn failure_message(failure: &batch::Failure) -> String {
    match failure {
        batch::Failure::Step { id, error } => format!("{id}: {error}"),
        other => other.message(),
    }
}

fn to_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value)
        .unwrap_or_else(|err| format!("{{\"serialization_error\":\"{err}\"}}"))
}

fn print_usage() {
    println!(
        "Usage:\n  \
         glitcherz [--open <project>] [--autoplay [--note <0-127>]] [--gain <0.0-1.0>] [--seconds <secs>] [--list-devices]\n  \
         glitcherz --render <in.glz> <out.wav> [--rate <hz>] [--bits 16|24|32f] [--to <ticks>]\n  \
         glitcherz [--open <in.glz>] (--ops <file|-> | --do <id> [--params <json>])... [--save <out.glz>] [--wav <out.wav>] [--json]\n  \
         glitcherz --commands [--open <in.glz>] [--json]\n  \
         glitcherz --mcp [--root <dir>] [--open <in.glz>] [--allow-plugins]\n\
         Defaults: opens the main window and stays silent.\n\
         Play with the on-screen keyboard or the computer keyboard (A W S E D ..., Z/X to shift octaves); close the window to quit.\n\
         --autoplay holds one note from startup (diagnostic: proves the audio path);\n\
         --seconds auto-closes the window after N seconds (for smoke tests);\n\
         --render bounces a project to a stereo wav without opening a window (44.1 kHz / 24-bit by default).\n\
         --ops runs a JSON array of {{id, params}} commands (\"-\" reads stdin); --do runs one, in argv order;\n\
         without --open the batch starts from a new project; --save / --wav write the result, and a batch\n\
         that fails writes nothing.\n\
         --commands lists the registry (--json adds menu, shortcut and disabled reasons).\n\
         --mcp serves that same command surface as MCP tools over stdio: one JSON-RPC object per\n\
         line, nothing but protocol on stdout, every client-supplied path confined to --root\n\
         (default: the current directory), and plugin binaries not loaded unless --allow-plugins.\n\
         Exit codes: 0 ok, 1 engine or window failure, 2 bad arguments, 3 project or render failure, 4 command failed."
    );
}
