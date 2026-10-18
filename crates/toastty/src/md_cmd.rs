//! `toastty md [FILE|-]`: print a Markdown document as a rich block.
//!
//! Runs inside a shell in a toastty window (no GUI is started). When
//! the terminal is toastty (`TERM_PROGRAM=toastty`) and no terminal
//! multiplexer sits in between, the document is sent as an OSC 7700
//! block (see `docs/markdown.md`); otherwise the raw Markdown is printed,
//! like `cat`. Inside tmux / herdr / screen / zellij rich rendering
//! isn't available yet (multiplexers drop or mangle the private OSC), so
//! the helper falls back to plain text and says so on stderr when
//! stderr is a terminal.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// Largest OSC payload the parser accepts (`toastty_parser::MAX_OSC_LEN`),
/// minus headroom for the `7700;md;cwd=…;` prefix. Bigger documents are
/// printed as plain text.
pub const MAX_ENCODED_LEN: usize = toastty_parser::MAX_OSC_LEN - 64 * 1024;

/// A terminal multiplexer detected from the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Multiplexer {
    Tmux,
    Herdr,
    Screen,
    Zellij,
}

impl Multiplexer {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::Herdr => "herdr",
            Self::Screen => "screen",
            Self::Zellij => "zellij",
        }
    }
}

fn is_set(env: &dyn Fn(&str) -> Option<String>, key: &str) -> bool {
    env(key).is_some_and(|v| !v.is_empty())
}

/// Which multiplexer (if any) the helper runs under.
#[must_use]
pub fn detect_multiplexer(env: &dyn Fn(&str) -> Option<String>) -> Option<Multiplexer> {
    if is_set(env, "TMUX") {
        Some(Multiplexer::Tmux)
    } else if is_set(env, "HERDR_PANE_ID") || is_set(env, "HERDR_ENV") {
        Some(Multiplexer::Herdr)
    } else if is_set(env, "STY") {
        Some(Multiplexer::Screen)
    } else if is_set(env, "ZELLIJ") {
        Some(Multiplexer::Zellij)
    } else {
        None
    }
}

/// What to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Bytes for stdout.
    pub stdout: Vec<u8>,
    /// Note for stderr (printed only when stderr is a terminal).
    pub note: Option<String>,
}

/// Encode `source` as an OSC 7700 Markdown block, ST-terminated.
/// `cwd` becomes the `cwd=` key (relative image paths resolve against
/// it when the user allows local files).
#[must_use]
pub fn encode_osc(source: &str, cwd: Option<&Path>) -> Vec<u8> {
    let mut out = b"\x1b]7700;md;".to_vec();
    if let Some(dir) = cwd {
        out.extend_from_slice(b"cwd=");
        out.extend_from_slice(
            STANDARD
                .encode(dir.as_os_str().as_encoded_bytes())
                .as_bytes(),
        );
    }
    out.push(b';');
    out.extend_from_slice(STANDARD.encode(source.as_bytes()).as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

fn raw(source: &str) -> Vec<u8> {
    let mut out = source.as_bytes().to_vec();
    if !source.is_empty() && !source.ends_with('\n') {
        out.push(b'\n');
    }
    out
}

/// Decide what to print for `source`.
#[must_use]
pub fn plan(source: &str, cwd: Option<&Path>, env: &dyn Fn(&str) -> Option<String>) -> Output {
    if let Some(m) = detect_multiplexer(env) {
        return Output {
            stdout: raw(source),
            note: Some(format!(
                "toastty md: rich rendering isn't available inside {} yet; showing plain text",
                m.name()
            )),
        };
    }
    if env("TERM_PROGRAM").as_deref() != Some("toastty") {
        return Output {
            stdout: raw(source),
            note: None,
        };
    }
    if source.len().div_ceil(3) * 4 > MAX_ENCODED_LEN {
        return Output {
            stdout: raw(source),
            note: Some(
                "toastty md: document too large for rich rendering; showing plain text".into(),
            ),
        };
    }
    Output {
        stdout: encode_osc(source, cwd),
        note: None,
    }
}

/// The `cwd=` directory for a document: the file's directory, or the
/// current directory for stdin.
#[must_use]
pub fn base_dir_for(file: Option<&Path>, current_dir: Option<PathBuf>) -> Option<PathBuf> {
    let current = current_dir?;
    match file {
        Some(f) => {
            let parent = f.parent().filter(|p| !p.as_os_str().is_empty());
            Some(match parent {
                Some(p) if p.is_absolute() => p.to_path_buf(),
                Some(p) => current.join(p),
                None => current,
            })
        }
        None => Some(current),
    }
}

/// Run `toastty md`: read `file` (`None` or `-` = stdin) and print it.
///
/// # Errors
/// Reading the input or writing stdout failed, or the input isn't UTF-8.
pub fn run(file: Option<&Path>) -> anyhow::Result<()> {
    let file = file.filter(|f| f.as_os_str() != "-");
    let source = if let Some(path) = file {
        std::fs::read(path).map_err(|e| anyhow::anyhow!("toastty md: {}: {e}", path.display()))?
    } else {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        buf
    };
    let source = String::from_utf8(source)
        .map_err(|_| anyhow::anyhow!("toastty md: input is not valid UTF-8"))?;
    let cwd = base_dir_for(file, std::env::current_dir().ok());
    let env = |k: &str| std::env::var(k).ok();
    let out = plan(&source, cwd.as_deref(), &env);
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&out.stdout)?;
    stdout.flush()?;
    if let Some(note) = out.note {
        let stderr = std::io::stderr();
        if stderr.is_terminal() {
            let _ = writeln!(stderr.lock(), "{note}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k| pairs.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn osc_round_trips_through_the_term_parser() {
        let bytes = encode_osc("# Hi\n", Some(Path::new("/tmp/d")));
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("\x1b]7700;md;cwd="));
        assert!(s.ends_with("\x1b\\"));
        let body = &s[2..s.len() - 2];
        let parts: Vec<&[u8]> = body.split(';').map(str::as_bytes).collect();
        let parsed = toastty_term::parse_osc_7700(&parts).unwrap();
        assert_eq!(parsed.kind, "md");
        assert_eq!(parsed.source, "# Hi\n");
        assert_eq!(parsed.cwd.as_deref(), Some(Path::new("/tmp/d")));
    }

    #[test]
    fn osc_without_cwd_has_empty_keys() {
        let bytes = encode_osc("x", None);
        assert_eq!(bytes, b"\x1b]7700;md;;eA==\x1b\\");
    }

    #[test]
    fn toastty_gets_the_block() {
        let env = env_of(&[("TERM_PROGRAM", "toastty")]);
        let out = plan("# a", None, &env);
        assert!(out.stdout.starts_with(b"\x1b]7700;md;"));
        assert_eq!(out.note, None);
    }

    #[test]
    fn other_terminals_get_raw_markdown() {
        for env in [env_of(&[]), env_of(&[("TERM_PROGRAM", "iTerm.app")])] {
            let out = plan("# a", None, &env);
            assert_eq!(out.stdout, b"# a\n");
            assert_eq!(out.note, None);
        }
        // Already newline-terminated / empty input is left alone.
        assert_eq!(plan("x\n", None, &env_of(&[])).stdout, b"x\n");
        assert_eq!(plan("", None, &env_of(&[])).stdout, b"");
    }

    #[test]
    fn multiplexers_fall_back_with_a_note() {
        let cases = [
            ("TMUX", "tmux"),
            ("HERDR_PANE_ID", "herdr"),
            ("HERDR_ENV", "herdr"),
            ("STY", "screen"),
            ("ZELLIJ", "zellij"),
        ];
        for (key, name) in cases {
            let env = env_of(&[("TERM_PROGRAM", "toastty"), (key, "1")]);
            let out = plan("# a", None, &env);
            assert_eq!(out.stdout, b"# a\n", "{key}");
            let note = out.note.unwrap();
            assert!(note.contains(name), "{note}");
            assert!(note.contains("plain text"));
        }
        // Empty values don't count.
        let env = env_of(&[("TERM_PROGRAM", "toastty"), ("TMUX", "")]);
        assert!(plan("a", None, &env).stdout.starts_with(b"\x1b]7700"));
    }

    #[test]
    fn huge_documents_fall_back() {
        let env = env_of(&[("TERM_PROGRAM", "toastty")]);
        let big = "a".repeat(MAX_ENCODED_LEN / 4 * 3 + 10);
        let out = plan(&big, None, &env);
        assert!(!out.stdout.starts_with(b"\x1b]"));
        assert!(out.note.is_some());
        let fits = "a".repeat(MAX_ENCODED_LEN / 4 * 3 - 3);
        assert!(plan(&fits, None, &env).stdout.starts_with(b"\x1b]7700"));
    }

    #[test]
    fn base_dir_is_the_files_directory() {
        let cur = PathBuf::from("/home/u");
        assert_eq!(
            base_dir_for(Some(Path::new("docs/a.md")), Some(cur.clone())),
            Some(PathBuf::from("/home/u/docs"))
        );
        assert_eq!(
            base_dir_for(Some(Path::new("a.md")), Some(cur.clone())),
            Some(cur.clone())
        );
        assert_eq!(
            base_dir_for(Some(Path::new("/srv/x/a.md")), Some(cur.clone())),
            Some(PathBuf::from("/srv/x"))
        );
        assert_eq!(base_dir_for(None, Some(cur.clone())), Some(cur));
        assert_eq!(base_dir_for(None, None), None);
    }
}
