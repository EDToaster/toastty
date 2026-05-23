//! Command-line argument parsing.
//!
//! Tiny, dependency-free. We don't take `clap` for two flags — if
//! the surface grows past ~5 flags, swap to `clap` then.

use std::io::Write;

use toastty_config::Config;

/// What the CLI args told the binary to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Run the terminal normally.
    Run,
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
pub fn parse<I, S>(args: I) -> Result<Action, ParseError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut action = Action::Run;
    for arg in args {
        match arg.as_ref() {
            "--print-default-config" => action = Action::PrintDefaultConfig,
            "--help" | "-h" => action = Action::PrintHelp,
            "--version" | "-V" => action = Action::PrintVersion,
            other => {
                return Err(ParseError {
                    arg: other.to_string(),
                });
            }
        }
    }
    Ok(action)
}

/// Human-readable help text. Stable enough to test against.
pub fn help_text() -> &'static str {
    "\
toastty — lightweight GPU-accelerated terminal emulator

USAGE:
    toastty [FLAGS]

FLAGS:
    --print-default-config    Print the default TOML config to stdout and exit.
                              Useful for bootstrapping: `toastty --print-default-config > ~/.config/toastty/config.toml`
    -h, --help                Print this help and exit
    -V, --version             Print the version and exit
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

    #[test]
    fn no_args_runs() {
        assert_eq!(parse::<_, &str>([]).unwrap(), Action::Run);
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
    fn unknown_arg_errors() {
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
    fn default_config_round_trips_through_toml() {
        let mut buf = Vec::new();
        write_default_config(&mut buf).unwrap();
        let s = std::str::from_utf8(&buf).unwrap();

        // Sanity: contains every top-level section.
        for section in ["[font]", "[theme]", "[cursor]", "[shell]", "[scrollback]"] {
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
    fn version_text_includes_pkg_version() {
        assert!(version_text().contains(env!("CARGO_PKG_VERSION")));
    }
}
