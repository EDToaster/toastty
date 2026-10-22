//! Resolve and read the `[shader] paths` files (M13).
//!
//! Renderer-agnostic: the binary turns each [`ShaderFile`] into a
//! `toastty_render::PostShaderSource`. Resolution rules:
//!
//! - `~` and `~/…` expand to `$HOME` (`~user` is not supported and is
//!   taken literally).
//! - Absolute paths are used as is.
//! - Relative paths are relative to the directory of the config file in
//!   use, or the XDG config dir (`$XDG_CONFIG_HOME/toastty`,
//!   `~/.config/toastty`) when running on defaults. See [`base_dir`].

use std::path::{Path, PathBuf};

use toastty_config::{Config, ConfigSource};

/// One shader file that was read successfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaderFile {
    /// Resolved path (what the user sees in error messages).
    pub path: PathBuf,
    /// File contents.
    pub source: String,
}

/// A shader file that couldn't be read. Displays as `path: message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaderFileError {
    pub path: PathBuf,
    /// The I/O error kind. On a watcher-triggered reload the caller can
    /// skip `NotFound`: editors that save by rename briefly remove the
    /// file.
    pub kind: std::io::ErrorKind,
    pub message: String,
}

impl std::fmt::Display for ShaderFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

/// Directory that relative shader paths are resolved against, for a
/// config loaded from `source`. `None` when there is no config file and
/// no XDG/HOME to derive one from (relative paths then stay relative to
/// the CWD).
#[must_use]
pub fn base_dir(source: &ConfigSource) -> Option<PathBuf> {
    base_dir_with(source, Config::default_path().as_deref())
}

/// [`base_dir`] with the default config path passed in (testable
/// without touching the process environment).
fn base_dir_with(source: &ConfigSource, default_path: Option<&Path>) -> Option<PathBuf> {
    let file = match source {
        ConfigSource::File(p) => p.as_path(),
        ConfigSource::Defaults => default_path?,
    };
    file.parent().map(Path::to_path_buf)
}

/// Resolve every configured path, in order. `base` comes from
/// [`base_dir`]; `$HOME` is read from the environment.
#[must_use]
pub fn resolve_paths(raw: &[String], base: Option<&Path>) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    raw.iter()
        .map(|r| resolve_path(r, base, home.as_deref()))
        .collect()
}

/// Resolve one configured path. See the module docs for the rules.
#[must_use]
pub fn resolve_path(raw: &str, base: Option<&Path>, home: Option<&Path>) -> PathBuf {
    if let Some(home) = home.filter(|h| !h.as_os_str().is_empty()) {
        if raw == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    let p = Path::new(raw);
    match base {
        Some(base) if p.is_relative() => base.join(p),
        _ => p.to_path_buf(),
    }
}

/// Read every file in `paths`, in order. All-or-nothing: if any file
/// can't be read, returns one error per failing file.
pub fn load(paths: &[PathBuf]) -> Result<Vec<ShaderFile>, Vec<ShaderFileError>> {
    let mut files = Vec::with_capacity(paths.len());
    let mut errors = Vec::new();
    for path in paths {
        match std::fs::read_to_string(path) {
            Ok(source) => files.push(ShaderFile {
                path: path.clone(),
                source,
            }),
            Err(e) => errors.push(ShaderFileError {
                path: path.clone(),
                kind: e.kind(),
                message: e.to_string(),
            }),
        }
    }
    if errors.is_empty() {
        Ok(files)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expands_to_home() {
        let home = Path::new("/h");
        let base = Path::new("/cfg");
        assert_eq!(
            resolve_path("~/s/crt.glsl", Some(base), Some(home)),
            PathBuf::from("/h/s/crt.glsl")
        );
        assert_eq!(
            resolve_path("~", Some(base), Some(home)),
            PathBuf::from("/h")
        );
        // `~user` isn't expanded; it's a relative path.
        assert_eq!(
            resolve_path("~bob/x.glsl", Some(base), Some(home)),
            PathBuf::from("/cfg/~bob/x.glsl")
        );
        // No HOME: `~/` stays literal (relative to the base).
        assert_eq!(
            resolve_path("~/x.glsl", Some(base), None),
            PathBuf::from("/cfg/~/x.glsl")
        );
    }

    #[test]
    fn relative_joins_base_absolute_kept() {
        let base = Path::new("/cfg/toastty");
        assert_eq!(
            resolve_path("shaders/a.wgsl", Some(base), None),
            PathBuf::from("/cfg/toastty/shaders/a.wgsl")
        );
        assert_eq!(
            resolve_path("/abs/a.glsl", Some(base), None),
            PathBuf::from("/abs/a.glsl")
        );
        assert_eq!(resolve_path("a.glsl", None, None), PathBuf::from("a.glsl"));
    }

    #[test]
    fn base_dir_from_file_or_default() {
        let file = ConfigSource::File(PathBuf::from("/etc/t/my.toml"));
        assert_eq!(
            base_dir_with(&file, Some(Path::new("/x/toastty/config.toml"))),
            Some(PathBuf::from("/etc/t"))
        );
        assert_eq!(
            base_dir_with(
                &ConfigSource::Defaults,
                Some(Path::new("/x/toastty/config.toml"))
            ),
            Some(PathBuf::from("/x/toastty"))
        );
        assert_eq!(base_dir_with(&ConfigSource::Defaults, None), None);
        // The env-reading wrapper runs.
        let _ = base_dir(&ConfigSource::Defaults);
    }

    #[test]
    fn resolve_paths_keeps_order() {
        let base = Path::new("/cfg");
        let raw = vec!["b.glsl".to_string(), "/a.wgsl".to_string()];
        assert_eq!(
            resolve_paths(&raw, Some(base)),
            vec![PathBuf::from("/cfg/b.glsl"), PathBuf::from("/a.wgsl")]
        );
    }

    #[test]
    fn load_reads_files_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.glsl");
        let b = dir.path().join("b.wgsl");
        std::fs::write(&a, "void mainImage() {}").unwrap();
        std::fs::write(&b, "fn mainImage() {}").unwrap();
        let files = load(&[b.clone(), a.clone()]).unwrap();
        assert_eq!(
            files,
            vec![
                ShaderFile {
                    path: b,
                    source: "fn mainImage() {}".into()
                },
                ShaderFile {
                    path: a,
                    source: "void mainImage() {}".into()
                },
            ]
        );
        assert_eq!(load(&[]).unwrap(), Vec::new());
    }

    #[test]
    fn load_reports_every_failing_file() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok.glsl");
        std::fs::write(&ok, "x").unwrap();
        let missing = dir.path().join("missing.glsl");
        let not_utf8 = dir.path().join("bin.glsl");
        std::fs::write(&not_utf8, [0xff, 0xfe, 0x00]).unwrap();

        let errs = load(&[missing.clone(), ok, not_utf8.clone()]).unwrap_err();
        assert_eq!(errs.len(), 2);
        assert_eq!(errs[0].path, missing);
        assert_eq!(errs[0].kind, std::io::ErrorKind::NotFound);
        let shown = errs[0].to_string();
        assert!(
            shown.starts_with(&format!("{}: No such file", missing.display())),
            "{shown}"
        );
        assert_eq!(errs[1].path, not_utf8);
        assert_eq!(errs[1].kind, std::io::ErrorKind::InvalidData);
        // A directory is not a shader file either.
        let errs = load(&[dir.path().to_path_buf()]).unwrap_err();
        assert_eq!(errs.len(), 1);
    }
}
