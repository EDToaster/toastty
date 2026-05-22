use std::ffi::OsString;
use std::path::PathBuf;

/// PTY window size, in cells and pixels. Pixel dimensions are optional —
/// some apps (mode 2048, kitty graphics protocol) want them; legacy apps
/// only read rows/cols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinSize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl Default for WinSize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// Configuration for spawning a child under a PTY.
#[derive(Debug, Clone)]
pub struct PtySpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub working_dir: Option<PathBuf>,
    pub size: WinSize,
}

impl PtySpec {
    pub fn program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            working_dir: None,
            size: WinSize::default(),
        }
    }

    /// Default shell from `$SHELL`, falling back to `/bin/sh`, with the
    /// current process's environment inherited.
    pub fn shell() -> Self {
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
        Self::program(shell).with_current_env()
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn with_current_env(mut self) -> Self {
        self.env.extend(std::env::vars_os());
        self
    }

    pub fn working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    pub fn size(mut self, size: WinSize) -> Self {
        self.size = size;
        self
    }
}
