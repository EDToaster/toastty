//! Shell choice resolution.

use std::ffi::OsString;
use std::path::PathBuf;

use toastty_config::ShellConfig;

use crate::cli::CommandOverride;

/// Resolve `cfg.program` (which may be `"auto"`) plus `cfg.args` into a
/// `(program, args)` pair suitable for a [`toastty_pty::PtySpec`].
///
/// - `program == "auto"` → `$SHELL` → `/bin/sh`
/// - otherwise treat `program` as the path verbatim
///
/// Returns `OsString` args because `PtySpec::args` is `Vec<OsString>`.
#[must_use]
pub fn resolve_shell(cfg: &ShellConfig) -> (PathBuf, Vec<OsString>) {
    let program: PathBuf = if cfg.program == "auto" {
        std::env::var_os("SHELL").map_or_else(|| PathBuf::from("/bin/sh"), PathBuf::from)
    } else {
        PathBuf::from(&cfg.program)
    };
    let args = cfg.args.iter().map(OsString::from).collect();
    (program, args)
}

/// Pick what to launch in the PTY: a CLI override wins; otherwise fall
/// back to the configured shell.
#[must_use]
pub fn resolve_command(
    cli_override: Option<CommandOverride>,
    cfg: &ShellConfig,
) -> (PathBuf, Vec<OsString>) {
    match cli_override {
        Some(cmd) => (cmd.program, cmd.args),
        None => resolve_shell(cfg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_program_is_used() {
        let cfg = ShellConfig {
            program: "/bin/zsh".into(),
            args: vec!["-l".into()],
        };
        let (prog, args) = resolve_shell(&cfg);
        assert_eq!(prog, PathBuf::from("/bin/zsh"));
        assert_eq!(args, vec![OsString::from("-l")]);
    }

    #[test]
    fn auto_falls_back_to_bin_sh_when_shell_unset() {
        // SAFETY: tests run single-threaded for env_remove. We restore.
        let original = std::env::var_os("SHELL");
        // SAFETY: single-threaded test access.
        unsafe { std::env::remove_var("SHELL") };

        let cfg = ShellConfig {
            program: "auto".into(),
            args: vec![],
        };
        let (prog, _) = resolve_shell(&cfg);
        assert_eq!(prog, PathBuf::from("/bin/sh"));

        if let Some(s) = original {
            // SAFETY: single-threaded test access.
            unsafe { std::env::set_var("SHELL", s) };
        }
    }

    #[test]
    fn args_are_passed_through() {
        let cfg = ShellConfig {
            program: "/bin/bash".into(),
            args: vec!["-l".into(), "-i".into()],
        };
        let (_, args) = resolve_shell(&cfg);
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], OsString::from("-l"));
        assert_eq!(args[1], OsString::from("-i"));
    }
}
