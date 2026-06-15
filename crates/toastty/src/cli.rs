//! Command-line argument parsing.
//!
//! Tiny, dependency-free. We don't take `clap` for a handful of flags —
//! if the surface grows past ~5 flags, swap to `clap` then.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use toastty_config::Config;

/// Program + args override coming from the CLI. When `Some`, the binary
/// launches this instead of the configured shell — same convention as
/// `xterm -e`, `alacritty -e`, `kitty <cmd>`.
#[derive(Debug, PartialEq, Eq)]
pub struct CommandOverride {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

/// What the CLI args told the binary to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Run the terminal normally. `command` overrides the configured
    /// shell when present; `working_directory` overrides the CWD the
    /// shell (or command) is spawned in.
    Run {
        command: Option<CommandOverride>,
        working_directory: Option<PathBuf>,
    },
    /// Print the default config (TOML) to stdout and exit.
    PrintDefaultConfig,
    /// Print the help text to stdout and exit.
    PrintHelp,
    /// Print the version string to stdout and exit.
    PrintVersion,
}

/// An argument we didn't recognise.
#[derive(Debug, PartialEq, Eq)]
pub struct ParseError {
    pub arg: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown argument: {}", self.arg)
    }
}

impl std::error::Error for ParseError {}

/// Parse `argv[1..]`. Returns the resolved [`Action`] or a
/// [`ParseError`] naming the bad flag.
///
/// Argument convention:
/// - Known flags (`--help`, `--version`, `--print-default-config`,
///   `--working-directory`) are consumed in any order.
/// - The first bare token (no leading `-`), OR a `--` separator, OR
///   `-e` / `--command`, switches the parser into "command mode": every
///   remaining argument is passed through verbatim as `program` +
///   `args`. This matches kitty (`kitty <cmd> [args]`) and xterm
///   (`xterm -e <cmd> [args]`) so users coming from either feel at home.
pub fn parse<I, S>(args: I) -> Result<Action, ParseError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut action_kind: Option<ActionKind> = None;
    let mut command: Option<CommandOverride> = None;
    let mut working_directory: Option<PathBuf> = None;
    let mut iter = args.into_iter().map(Into::into);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--print-default-config" => action_kind = Some(ActionKind::PrintDefaultConfig),
            "--help" | "-h" => action_kind = Some(ActionKind::PrintHelp),
            "--version" | "-V" => action_kind = Some(ActionKind::PrintVersion),
            // Initial CWD for the spawned shell/command. Value form
            // (`--working-directory=PATH`) is handled below; the
            // space-separated form consumes the next token.
            "--working-directory" | "-d" => {
                let Some(dir) = iter.next() else {
                    return Err(ParseError {
                        arg: format!("{arg} requires a path"),
                    });
                };
                working_directory = Some(PathBuf::from(dir));
            }
            // `--` separator, `-e`, `--command`: everything that follows
            // is the command to run. We don't second-guess it; even
            // `-h` after the separator is part of the command.
            "--" | "-e" | "--command" => {
                command = Some(collect_command(&mut iter, &arg)?);
                break;
            }
            other if other.starts_with("--working-directory=") => {
                let val = &other["--working-directory=".len()..];
                working_directory = Some(PathBuf::from(val));
            }
            other if other.starts_with('-') => {
                return Err(ParseError {
                    arg: other.to_string(),
                });
            }
            // First bare positional: treat as program; collect the rest
            // as args. Same as kitty's positional command form.
            _ => {
                let mut args_vec = Vec::new();
                for rest in iter.by_ref() {
                    args_vec.push(OsString::from(rest));
                }
                command = Some(CommandOverride {
                    program: PathBuf::from(arg),
                    args: args_vec,
                });
                break;
            }
        }
    }
    Ok(match action_kind {
        Some(ActionKind::PrintDefaultConfig) => Action::PrintDefaultConfig,
        Some(ActionKind::PrintHelp) => Action::PrintHelp,
        Some(ActionKind::PrintVersion) => Action::PrintVersion,
        None => Action::Run {
            command,
            working_directory,
        },
    })
}

/// Consume the remainder of `iter` as `program` + `args`. The separator
/// (`--` / `-e` / `--command`) requires *some* following token to name
/// the program.
fn collect_command<I>(iter: &mut I, separator: &str) -> Result<CommandOverride, ParseError>
where
    I: Iterator<Item = String>,
{
    let Some(program) = iter.next() else {
        return Err(ParseError {
            arg: format!("{separator} requires a command"),
        });
    };
    let args = iter.map(OsString::from).collect();
    Ok(CommandOverride {
        program: PathBuf::from(program),
        args,
    })
}

enum ActionKind {
    PrintDefaultConfig,
    PrintHelp,
    PrintVersion,
}

/// Human-readable help text. Stable enough to test against.
pub fn help_text() -> &'static str {
    "\
toastty — lightweight GPU-accelerated terminal emulator

USAGE:
    toastty [FLAGS] [-- | -e | --command] <command> [args...]
    toastty [FLAGS] <command> [args...]

FLAGS:
    -d, --working-directory <PATH>
                              Spawn the shell (or command) in <PATH> instead of
                              the current directory. Accepts `--working-directory=<PATH>` too.
    --print-default-config    Print the default TOML config to stdout and exit.
                              Useful for bootstrapping: `toastty --print-default-config > ~/.config/toastty/config.toml`
    -h, --help                Print this help and exit
    -V, --version             Print the version and exit

COMMAND:
    Anything after the first bare positional, after `--`, or after
    `-e`/`--command` is passed through as the program to launch in the
    PTY (instead of the configured shell). Examples:
        toastty bash -c 'echo hi; sleep 5'
        toastty -e htop
        toastty -- python -m http.server
"
}

/// Version string, sourced from Cargo metadata.
#[must_use]
pub fn version_text() -> String {
    format!("toastty {}", env!("CARGO_PKG_VERSION"))
}

/// Write the default config (serialized as TOML) to `out`.
///
/// Separated from `main` so it can be unit-tested without going
/// through stdout.
pub fn write_default_config<W: Write>(out: &mut W) -> std::io::Result<()> {
    let toml = toml::to_string_pretty(&Config::defaults())
        .expect("Config::defaults() should always serialize");
    out.write_all(toml.as_bytes())?;
    out.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_action() -> Action {
        Action::Run {
            command: None,
            working_directory: None,
        }
    }

    #[test]
    fn no_args_runs() {
        assert_eq!(parse::<_, &str>([]).unwrap(), run_action());
    }

    #[test]
    fn print_default_config_flag() {
        assert_eq!(
            parse(["--print-default-config"]).unwrap(),
            Action::PrintDefaultConfig
        );
    }

    #[test]
    fn help_long_and_short() {
        assert_eq!(parse(["--help"]).unwrap(), Action::PrintHelp);
        assert_eq!(parse(["-h"]).unwrap(), Action::PrintHelp);
    }

    #[test]
    fn version_long_and_short() {
        assert_eq!(parse(["--version"]).unwrap(), Action::PrintVersion);
        assert_eq!(parse(["-V"]).unwrap(), Action::PrintVersion);
    }

    #[test]
    fn last_flag_wins_for_actions() {
        assert_eq!(
            parse(["--help", "--print-default-config"]).unwrap(),
            Action::PrintDefaultConfig
        );
    }

    #[test]
    fn unknown_flag_errors() {
        let err = parse(["--nonsense"]).unwrap_err();
        assert_eq!(err.arg, "--nonsense");
    }

    #[test]
    fn parse_error_display_is_useful() {
        let err = ParseError {
            arg: "--nope".into(),
        };
        assert_eq!(err.to_string(), "unknown argument: --nope");
    }

    #[test]
    fn bare_positional_starts_command() {
        let action = parse(["bash", "-c", "echo hi"]).unwrap();
        let Action::Run {
            command: Some(cmd), ..
        } = action
        else {
            panic!("expected Run with command");
        };
        assert_eq!(cmd.program, PathBuf::from("bash"));
        assert_eq!(
            cmd.args,
            vec![OsString::from("-c"), OsString::from("echo hi")]
        );
    }

    #[test]
    fn dash_e_separator_starts_command() {
        let action = parse(["-e", "htop", "--", "-d", "5"]).unwrap();
        let Action::Run {
            command: Some(cmd), ..
        } = action
        else {
            panic!("expected Run with command");
        };
        assert_eq!(cmd.program, PathBuf::from("htop"));
        assert_eq!(
            cmd.args,
            vec![
                OsString::from("--"),
                OsString::from("-d"),
                OsString::from("5"),
            ]
        );
    }

    #[test]
    fn double_dash_separator_starts_command() {
        let action = parse(["--", "python", "-m", "http.server"]).unwrap();
        let Action::Run {
            command: Some(cmd), ..
        } = action
        else {
            panic!("expected Run with command");
        };
        assert_eq!(cmd.program, PathBuf::from("python"));
        assert_eq!(
            cmd.args,
            vec![OsString::from("-m"), OsString::from("http.server")]
        );
    }

    #[test]
    fn flags_before_command_are_consumed() {
        // Action-bearing flags before the command still resolve their
        // action; the command stays attached to Run only if no action
        // wins. (`--version` here wins.)
        assert_eq!(parse(["--version", "bash"]).unwrap(), Action::PrintVersion);
    }

    #[test]
    fn dash_e_without_program_errors() {
        let err = parse(["-e"]).unwrap_err();
        assert!(err.arg.contains("-e"));
    }

    #[test]
    fn working_directory_space_form() {
        let action = parse(["--working-directory", "/tmp/work"]).unwrap();
        assert_eq!(
            action,
            Action::Run {
                command: None,
                working_directory: Some(PathBuf::from("/tmp/work")),
            }
        );
    }

    #[test]
    fn working_directory_equals_form() {
        let action = parse(["--working-directory=/tmp/work"]).unwrap();
        assert_eq!(
            action,
            Action::Run {
                command: None,
                working_directory: Some(PathBuf::from("/tmp/work")),
            }
        );
    }

    #[test]
    fn working_directory_short_flag() {
        let action = parse(["-d", "/var/log"]).unwrap();
        assert_eq!(
            action,
            Action::Run {
                command: None,
                working_directory: Some(PathBuf::from("/var/log")),
            }
        );
    }

    #[test]
    fn working_directory_combines_with_command() {
        // `--working-directory` is independent of the command override:
        // spawn `bash` but in `/srv`.
        let action = parse(["--working-directory", "/srv", "-e", "bash"]).unwrap();
        let Action::Run {
            command: Some(cmd),
            working_directory: Some(dir),
        } = action
        else {
            panic!("expected Run with command + working_directory");
        };
        assert_eq!(cmd.program, PathBuf::from("bash"));
        assert_eq!(dir, PathBuf::from("/srv"));
    }

    #[test]
    fn working_directory_without_path_errors() {
        let err = parse(["--working-directory"]).unwrap_err();
        assert!(err.arg.contains("--working-directory"));
    }

    #[test]
    fn command_arg_with_leading_dash_passes_through() {
        // After the separator, leading-dash args are part of the
        // command — they must NOT be re-interpreted as toastty flags.
        let action = parse(["--", "bash", "-c", "exit 0"]).unwrap();
        let Action::Run {
            command: Some(cmd), ..
        } = action
        else {
            panic!("expected Run with command");
        };
        assert_eq!(cmd.program, PathBuf::from("bash"));
        assert_eq!(
            cmd.args,
            vec![OsString::from("-c"), OsString::from("exit 0")]
        );
    }

    #[test]
    fn default_config_round_trips_through_toml() {
        let mut buf = Vec::new();
        write_default_config(&mut buf).unwrap();
        let s = std::str::from_utf8(&buf).unwrap();

        // Sanity: contains every top-level section.
        for section in [
            "[font]",
            "[theme]",
            "[cursor]",
            "[shell]",
            "[scrollback]",
            "[window]",
        ] {
            assert!(s.contains(section), "missing section {section} in:\n{s}");
        }

        // Round-trip: must parse back into the same Config.
        let parsed: Config = toml::from_str(s).expect("output must parse as Config");
        assert_eq!(parsed, Config::defaults());
    }

    #[test]
    fn help_text_mentions_print_default_config() {
        assert!(help_text().contains("--print-default-config"));
    }

    #[test]
    fn help_text_mentions_command_passthrough() {
        let help = help_text();
        assert!(help.contains("-e") || help.contains("--command"));
        assert!(help.to_lowercase().contains("command"));
    }

    #[test]
    fn version_text_includes_pkg_version() {
        assert!(version_text().contains(env!("CARGO_PKG_VERSION")));
    }
}
