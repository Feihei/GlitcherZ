//! Command line parsing (S0 for `--render`, S4 for the batch surface).
//!
//! Parsed by hand: the app's dependency set is the workspace crates plus
//! `serde_json`, so no argument-parsing crate either. Every mutual-exclusion
//! rule lives in [`Args::mode`] rather than being re-checked at each call
//! site.

use std::path::PathBuf;

use serde_json::Value;

/// One source of batch steps, in argv order.
#[derive(Debug, Clone, PartialEq)]
pub enum StepSource {
    /// `--do <id> [--params <json>]`; the parameters are already validated.
    Inline { id: String, params: Value },
    /// `--ops <file|->`, resolved at execution time so the error can name the
    /// file. `-` reads stdin.
    Ops(PathBuf),
}

/// What the invocation asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Help,
    ListDevices,
    /// `--commands`: enumerate the command registry.
    Commands,
    /// `--render <in> <out>`: one-shot project-to-wav, no batch.
    Render {
        input: PathBuf,
        output: PathBuf,
    },
    /// `--ops` / `--do` / `--save` / `--wav`: load, run, maybe write.
    Batch,
    /// `--mcp`: serve the MCP protocol on stdio (S5).
    Mcp,
    /// No mode flag: open the main window.
    Gui,
}

/// Command line arguments.
pub struct Args {
    /// MIDI note auto-triggered at startup (69 = A4).
    pub note: u8,
    /// Hold a note at startup (diagnostic; the default is silent).
    pub autoplay: bool,
    /// Project file loaded at startup (or the batch's starting document).
    pub open: Option<PathBuf>,
    pub gain: f64,
    pub seconds: Option<f64>,
    pub list_devices: bool,
    pub help: bool,
    /// Headless one-shot render: input project + output wav (S0).
    pub render: Option<(PathBuf, PathBuf)>,
    pub render_rate: u32,
    pub render_bits: Option<String>,
    pub render_to: Option<u32>,
    /// Batch steps in argv order (S4).
    pub steps: Vec<StepSource>,
    /// Write the batch result here (S4).
    pub save: Option<PathBuf>,
    /// Render the batch result here (S4).
    pub wav: Option<PathBuf>,
    pub commands: bool,
    pub json: bool,
    /// Serve MCP on stdio (S5).
    pub mcp: bool,
    /// Sandbox root for MCP path arguments (default: the current directory).
    pub root: Option<PathBuf>,
    /// Let the MCP server load CLAP / VST3 binaries (off by default).
    pub allow_plugins: bool,
    /// Argument parsing error (reported before anything starts).
    pub error: Option<String>,
}

impl Args {
    /// Resolves which mode the flags describe, or why they contradict.
    pub fn mode(&self) -> Result<Mode, String> {
        // Two flags that print and exit win over everything else, in the
        // order the help text documents.
        if self.help {
            return Ok(Mode::Help);
        }
        if self.list_devices {
            return Ok(Mode::ListDevices);
        }
        let batch = self.has_batch();
        if self.mcp {
            if self.render.is_some() {
                return Err("--mcp cannot be combined with --render".to_string());
            }
            if self.commands || batch {
                return Err(
                    "--mcp cannot be combined with --commands / --ops / --do / --save / --wav"
                        .to_string(),
                );
            }
            return Ok(Mode::Mcp);
        }
        // Silently ignoring these would leave an operator believing the
        // sandbox or the plugin policy is in force when it is not.
        if self.root.is_some() || self.allow_plugins {
            return Err("--root and --allow-plugins only apply to --mcp".to_string());
        }
        if self.commands {
            if self.render.is_some() {
                return Err("--commands cannot be combined with --render".to_string());
            }
            if batch {
                return Err(
                    "--commands cannot be combined with --ops / --do / --save / --wav".to_string(),
                );
            }
            return Ok(Mode::Commands);
        }
        if let Some((input, output)) = &self.render {
            if batch {
                // Guessing from arity ("one path means there is no input
                // project") is the kind of parsing that bites later.
                return Err("--render takes its own <in.glz> <out.wav>; \
                     use --wav to render what --open plus --ops produced"
                    .to_string());
            }
            return Ok(Mode::Render {
                input: input.clone(),
                output: output.clone(),
            });
        }
        if batch {
            return Ok(Mode::Batch);
        }
        Ok(Mode::Gui)
    }

    /// Whether the flags describe a batch run of any kind.
    pub fn has_batch(&self) -> bool {
        !self.steps.is_empty() || self.save.is_some() || self.wav.is_some()
    }
}

pub fn parse(raw: impl Iterator<Item = String>) -> Args {
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
        steps: Vec::new(),
        save: None,
        wav: None,
        commands: false,
        json: false,
        mcp: false,
        root: None,
        allow_plugins: false,
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
                _ => fail(&mut args, "--render needs <in.glz> <out.wav>"),
            },
            "--rate" => {
                if let Some(v) = it.next().and_then(|s| s.parse::<u32>().ok()) {
                    args.render_rate = v;
                } else {
                    fail(&mut args, "--rate needs a positive sample rate");
                }
            }
            "--bits" => args.render_bits = it.next(),
            "--to" => {
                if let Some(v) = it.next().and_then(|s| s.parse::<u32>().ok()) {
                    args.render_to = Some(v);
                } else {
                    fail(&mut args, "--to needs a tick count");
                }
            }
            "--do" => match it.next() {
                Some(id) => args.steps.push(StepSource::Inline {
                    id,
                    params: Value::Object(Default::default()),
                }),
                None => fail(&mut args, "--do needs a command id"),
            },
            // `--params` attaches to the `--do` immediately before it, so a
            // batch reads as `--do a --params {…} --do b --params {…}`.
            "--params" => match (it.next(), args.steps.last_mut()) {
                (Some(text), Some(StepSource::Inline { params, .. })) => {
                    match serde_json::from_str::<Value>(&text) {
                        Ok(value) if value.is_object() => *params = value,
                        Ok(_) => fail(&mut args, "--params must be a JSON object"),
                        Err(err) => fail(&mut args, format!("--params is not valid JSON: {err}")),
                    }
                }
                (Some(_), None) => fail(&mut args, "--params must follow a --do <id>"),
                _ => fail(&mut args, "--params needs a JSON object"),
            },
            "--ops" => match it.next() {
                Some(path) => args.steps.push(StepSource::Ops(PathBuf::from(path))),
                None => fail(&mut args, "--ops needs <file|->"),
            },
            "--save" => match it.next() {
                Some(path) => args.save = Some(PathBuf::from(path)),
                None => fail(&mut args, "--save needs <out.glz>"),
            },
            "--wav" => match it.next() {
                Some(path) => args.wav = Some(PathBuf::from(path)),
                None => fail(&mut args, "--wav needs <out.wav>"),
            },
            "--commands" => args.commands = true,
            "--mcp" => args.mcp = true,
            "--root" => match it.next() {
                Some(path) => args.root = Some(PathBuf::from(path)),
                None => fail(&mut args, "--root needs a directory"),
            },
            "--allow-plugins" => args.allow_plugins = true,
            "--json" => args.json = true,
            "--help" | "-h" => args.help = true,
            other => eprintln!("Ignoring unknown argument: {other} (see --help for usage)"),
        }
    }
    args
}

/// Records the first error: later ones are usually consequences of it.
fn fail(args: &mut Args, message: impl Into<String>) {
    if args.error.is_none() {
        args.error = Some(message.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(raw: &[&str]) -> Args {
        parse(raw.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn render_args_capture_paths_and_options() {
        let args = parse_args(&[
            "--render", "song.glz", "out.wav", "--rate", "48000", "--bits", "16", "--to", "3840",
        ]);
        assert!(args.error.is_none());
        assert_eq!(
            args.render,
            Some((PathBuf::from("song.glz"), PathBuf::from("out.wav")))
        );
        assert_eq!(args.render_rate, 48_000);
        assert_eq!(args.render_bits.as_deref(), Some("16"));
        assert_eq!(args.render_to, Some(3840));
        assert_eq!(
            args.mode().unwrap(),
            Mode::Render {
                input: PathBuf::from("song.glz"),
                output: PathBuf::from("out.wav"),
            }
        );
    }

    #[test]
    fn render_needs_both_paths() {
        let args = parse_args(&["--render", "song.glz"]);
        assert!(args.error.is_some());
        assert!(args.render.is_none());
    }

    #[test]
    fn no_flags_means_the_window() {
        assert_eq!(parse_args(&[]).mode().unwrap(), Mode::Gui);
    }

    #[test]
    fn a_do_without_params_uses_an_empty_object() {
        let args = parse_args(&["--do", "midi.query"]);
        assert!(args.error.is_none());
        assert_eq!(
            args.steps,
            vec![StepSource::Inline {
                id: "midi.query".to_string(),
                params: Value::Object(Default::default()),
            }]
        );
        assert_eq!(args.mode().unwrap(), Mode::Batch);
    }

    #[test]
    fn params_attach_to_the_preceding_do() {
        let args = parse_args(&[
            "--do",
            "midi.transpose",
            "--params",
            r#"{"semitones": 12}"#,
            "--do",
            "midi.query",
        ]);
        assert!(args.error.is_none());
        assert_eq!(args.steps.len(), 2);
        match &args.steps[0] {
            StepSource::Inline { id, params } => {
                assert_eq!(id, "midi.transpose");
                assert_eq!(params["semitones"], 12);
            }
            other => panic!("expected an inline step, got {other:?}"),
        }
        assert!(matches!(
            &args.steps[1],
            StepSource::Inline { id, .. } if id == "midi.query"
        ));
    }

    #[test]
    fn ops_sits_in_argv_order() {
        let args = parse_args(&["--do", "a.one", "--ops", "steps.json", "--do", "b.two"]);
        assert!(args.error.is_none());
        assert!(matches!(args.steps[0], StepSource::Inline { .. }));
        assert_eq!(args.steps[1], StepSource::Ops(PathBuf::from("steps.json")));
        assert!(matches!(args.steps[2], StepSource::Inline { .. }));
    }

    #[test]
    fn params_errors_are_reported() {
        // Not JSON at all.
        assert!(
            parse_args(&["--do", "a", "--params", "{oops"])
                .error
                .is_some()
        );
        // Valid JSON, but not an object.
        assert!(
            parse_args(&["--do", "a", "--params", "[1,2]"])
                .error
                .is_some()
        );
        // No preceding --do.
        assert!(parse_args(&["--params", "{}"]).error.is_some());
    }

    #[test]
    fn save_or_wav_alone_still_counts_as_a_batch() {
        assert_eq!(
            parse_args(&["--open", "in.glz", "--save", "out.glz"])
                .mode()
                .unwrap(),
            Mode::Batch
        );
        assert_eq!(
            parse_args(&["--wav", "out.wav"]).mode().unwrap(),
            Mode::Batch
        );
    }

    #[test]
    fn render_and_batch_are_exclusive() {
        let err = parse_args(&["--render", "in.glz", "out.wav", "--do", "midi.query"])
            .mode()
            .unwrap_err();
        assert!(err.contains("--wav"), "the error points at the fix: {err}");
    }

    #[test]
    fn commands_is_exclusive_too() {
        assert_eq!(parse_args(&["--commands"]).mode().unwrap(), Mode::Commands);
        assert!(
            parse_args(&["--commands", "--save", "x.glz"])
                .mode()
                .is_err()
        );
        assert!(
            parse_args(&["--commands", "--render", "a", "b"])
                .mode()
                .is_err()
        );
    }

    #[test]
    fn help_wins_over_everything() {
        let args = parse_args(&["--help", "--render", "a", "b", "--commands"]);
        assert_eq!(args.mode().unwrap(), Mode::Help);
    }
}
