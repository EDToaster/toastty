//! Terminal-identity environment variables exported to the child.
//!
//! `TERM` alone can't say "24-bit colour works": toastty exports
//! `TERM=xterm-256color` until its terminfo is commonly installed, and
//! that entry only describes the 256-colour palette. The de-facto signal
//! is `COLORTERM=truecolor` — neovim's `detect_termguicolors`, bat,
//! delta, helix, micro, the `supports-color` npm package and most other
//! colour-aware tools check it first, and it survives `su` / `sudo -E`
//! and (with `SendEnv`) ssh, where in-band XTGETTCAP / DECRQSS probes may
//! not be attempted.
//!
//! `TERM_PROGRAM` / `TERM_PROGRAM_VERSION` follow Ghostty
//! (`TERM_PROGRAM=ghostty`, `TERM_PROGRAM_VERSION=<version>`), `WezTerm`
//! and iTerm2, so apps can identify the emulator without a round trip.
//! Setting them explicitly also overrides values leaked from whatever
//! terminal launched toastty (which would otherwise misidentify us).

/// `(name, value)` pairs to set on the child, applied after the
/// inherited environment so they win.
pub fn identity_env() -> [(&'static str, &'static str); 3] {
    [
        ("COLORTERM", "truecolor"),
        ("TERM_PROGRAM", "toastty"),
        ("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION")),
    ]
}

/// Apply [`identity_env`] to a spawn spec.
pub fn with_identity_env(mut spec: toastty_pty::PtySpec) -> toastty_pty::PtySpec {
    for (k, v) in identity_env() {
        spec = spec.env_remove(k).env(k, v);
    }
    spec
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn lookup(spec: &toastty_pty::PtySpec, key: &str) -> Vec<OsString> {
        spec.env
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    }

    #[test]
    fn identity_env_advertises_truecolor_and_program() {
        let env = identity_env();
        assert!(env.contains(&("COLORTERM", "truecolor")));
        assert!(env.contains(&("TERM_PROGRAM", "toastty")));
        let version = env.iter().find(|(k, _)| *k == "TERM_PROGRAM_VERSION");
        assert_eq!(version.unwrap().1, env!("CARGO_PKG_VERSION"));
        assert_ne!(env!("CARGO_PKG_VERSION"), "");
    }

    #[test]
    fn with_identity_env_overrides_leaked_values() {
        let spec = toastty_pty::PtySpec::program("/bin/sh")
            .env("COLORTERM", "256")
            .env("TERM_PROGRAM", "ghostty")
            .env("TERM_PROGRAM_VERSION", "1.1.2")
            .env("HOME", "/home/x");
        let spec = with_identity_env(spec);
        assert_eq!(lookup(&spec, "COLORTERM"), ["truecolor"]);
        assert_eq!(lookup(&spec, "TERM_PROGRAM"), ["toastty"]);
        assert_eq!(
            lookup(&spec, "TERM_PROGRAM_VERSION"),
            [env!("CARGO_PKG_VERSION")]
        );
        assert_eq!(lookup(&spec, "HOME"), ["/home/x"]);
    }
}
