//! `toastty present FILE`: show a Markdown deck one slide at a time.
//!
//! A client of the Markdown block protocol (OSC 7700, see
//! `docs/markdown.md`) and nothing else: it runs in a shell inside a
//! toastty window and talks to the terminal only through escape
//! sequences on its controlling tty. No toastty internals are touched.
//!
//! - Slides are separated by a line holding only `---` (outside fenced
//!   code), as in Marp and reveal.js. A YAML front-matter block at the
//!   top is skipped, and HTML comments (`<!-- … -->`) are dropped, so
//!   they can hold speaker notes.
//! - Each slide is drawn on the primary screen (rich blocks are ignored
//!   on the alternate screen): erase the display, send the slide as one
//!   block, then ask for the cursor position (DSR 6) to learn how many
//!   rows the block took. A slide that fits is drawn again, centred
//!   vertically, and a page counter goes on the last row. Each redraw is
//!   wrapped in synchronized output (DECSET 2026), so only the final
//!   frame is shown.
//! - The file is re-read when it changes on disk, and the slide is
//!   redrawn when the window size changes.
//! - Outside toastty (or under a multiplexer) the slides are shown as
//!   plain Markdown text, the same fallback `toastty md` uses.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::md_cmd;

/// Split a deck into slides.
///
/// A separator is a line that is exactly `---` (up to three spaces of
/// indent and trailing whitespace allowed) outside a fenced code block.
/// Front matter at the very top (`---` … `---` with only YAML-looking
/// lines between) is skipped. HTML comments outside code are removed.
/// Slides that are empty after that are dropped.
#[must_use]
pub fn split_slides(source: &str) -> Vec<String> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let source = source.replace("\r\n", "\n");
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let start = front_matter_end(&lines).unwrap_or(0);

    let mut slides = Vec::new();
    let mut cur = String::new();
    let mut fence: Option<(u8, usize)> = None;
    let mut in_comment = false;
    for line in &lines[start..] {
        let body = line.trim_end_matches('\n');
        if let Some((ch, len)) = fence {
            cur.push_str(line);
            if is_fence_close(body, ch, len) {
                fence = None;
            }
            continue;
        }
        if in_comment {
            if let Some(end) = body.find("-->") {
                in_comment = false;
                let rest = &body[end + 3..];
                if !rest.trim().is_empty() {
                    cur.push_str(rest);
                    cur.push('\n');
                }
            }
            continue;
        }
        if is_separator(body) {
            slides.push(std::mem::take(&mut cur));
            continue;
        }
        if let Some(f) = fence_open(body) {
            fence = Some(f);
            cur.push_str(line);
            continue;
        }
        let (kept, open) = strip_comments(body);
        in_comment = open;
        if kept.len() != body.len() && kept.trim().is_empty() {
            // A line that held only a comment: drop it entirely.
            continue;
        }
        cur.push_str(&kept);
        if line.ends_with('\n') {
            cur.push('\n');
        }
    }
    slides.push(cur);
    slides
        .into_iter()
        .map(|s| s.trim_matches('\n').to_string() + "\n")
        .filter(|s| !s.trim().is_empty())
        .collect()
}

fn is_separator(line: &str) -> bool {
    let indent = line.len() - line.trim_start_matches(' ').len();
    indent <= 3 && line.trim() == "---"
}

/// `Some((fence char, fence length))` when `line` opens a fenced block.
fn fence_open(line: &str) -> Option<(u8, usize)> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return None;
    }
    let ch = *t.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let len = t.bytes().take_while(|b| *b == ch).count();
    if len < 3 {
        return None;
    }
    // A backtick fence's info string can't contain backticks.
    if ch == b'`' && t[len..].contains('`') {
        return None;
    }
    Some((ch, len))
}

fn is_fence_close(line: &str, ch: u8, len: usize) -> bool {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return false;
    }
    let n = t.bytes().take_while(|b| *b == ch).count();
    n >= len && t[n..].trim().is_empty()
}

/// Remove `<!-- … -->` from one line. Returns the remaining text and
/// whether a comment is still open at the end of the line. Comments
/// inside inline code spans (`` `<!-- x -->` ``) are kept.
#[allow(clippy::many_single_char_names)]
fn strip_comments(line: &str) -> (String, bool) {
    let b = line.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut copied = 0;
    while i < b.len() {
        if b[i] == b'`' {
            let n = b[i..].iter().take_while(|c| **c == b'`').count();
            // Skip to the closing run of exactly `n` backticks, if any.
            let mut j = i + n;
            let mut close = None;
            while j < b.len() {
                if b[j] == b'`' {
                    let m = b[j..].iter().take_while(|c| **c == b'`').count();
                    if m == n {
                        close = Some(j + m);
                        break;
                    }
                    j += m;
                } else {
                    j += 1;
                }
            }
            i = close.unwrap_or(i + n);
            continue;
        }
        if b[i..].starts_with(b"<!--") {
            out.push_str(&line[copied..i]);
            match line[i + 4..].find("-->") {
                Some(k) => {
                    i += 4 + k + 3;
                    copied = i;
                }
                None => return (out, true),
            }
            continue;
        }
        i += 1;
    }
    out.push_str(&line[copied..]);
    (out, false)
}

/// Index of the first line after a leading front-matter block.
fn front_matter_end(lines: &[&str]) -> Option<usize> {
    let first = lines.first()?.trim_end();
    if first != "---" {
        return None;
    }
    for (i, line) in lines.iter().enumerate().skip(1) {
        let t = line.trim_end();
        if t == "---" || t == "..." {
            return (i > 1).then_some(i + 1);
        }
        let yamlish = t.is_empty()
            || t.starts_with(' ')
            || t.starts_with('#')
            || t.starts_with("- ")
            || t.split_once(':').is_some_and(|(k, _)| {
                !k.is_empty()
                    && k.chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
            });
        if !yamlish {
            return None;
        }
    }
    None
}

/// What a key press asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Next,
    Prev,
    First,
    Last,
    /// A digit typed toward a "go to slide N" (finished by Enter).
    Digit(u8),
    /// Enter: go to the typed slide number, or next when none.
    Enter,
    /// Backspace: delete a typed digit, or previous slide when none.
    Backspace,
    Reload,
    Quit,
    /// Bytes we don't act on.
    Ignore,
}

/// A cursor position report (`CSI row ; col R`), 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorReport {
    pub row: u16,
    pub col: u16,
}

/// One decoded input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Key(Command),
    Cursor(CursorReport),
}

/// Decode one event from the front of `buf`. Returns the event and the
/// number of bytes it took, or `None` when `buf` ends in the middle of
/// an escape sequence (wait for more). A lone ESC at the end of the
/// buffer is reported as `Quit` only when `eof` (no more bytes came).
#[must_use]
pub fn decode(buf: &[u8], eof: bool) -> Option<(Input, usize)> {
    let b0 = *buf.first()?;
    let key = |c, n| Some((Input::Key(c), n));
    match b0 {
        0x1b => {}
        b' ' | b'l' | b'j' | b'n' => return key(Command::Next, 1),
        b'h' | b'k' | b'p' => return key(Command::Prev, 1),
        b'g' => return key(Command::First, 1),
        b'G' => return key(Command::Last, 1),
        b'r' => return key(Command::Reload, 1),
        b'q' | 0x03 | 0x04 => return key(Command::Quit, 1),
        b'\r' | b'\n' => return key(Command::Enter, 1),
        0x7f | 0x08 => return key(Command::Backspace, 1),
        b'0'..=b'9' => return key(Command::Digit(b0 - b'0'), 1),
        _ => return key(Command::Ignore, 1),
    }
    let Some(&b1) = buf.get(1) else {
        return eof.then_some((Input::Key(Command::Quit), 1));
    };
    match b1 {
        b'[' => {}
        b'O' => {
            let &b2 = buf.get(2)?;
            let c = match b2 {
                b'C' | b'B' => Command::Next,
                b'D' | b'A' => Command::Prev,
                b'H' => Command::First,
                b'F' => Command::Last,
                _ => Command::Ignore,
            };
            return key(c, 3);
        }
        // ESC followed by something else: treat the ESC as its own key.
        _ => return key(Command::Quit, 1),
    }
    // CSI: parameters/intermediates 0x20..=0x3f, final 0x40..=0x7e.
    let mut end = 2;
    loop {
        let &b = buf.get(end)?;
        if (0x40..=0x7e).contains(&b) {
            break;
        }
        if !(0x20..=0x3f).contains(&b) {
            return key(Command::Ignore, end);
        }
        end += 1;
    }
    let params = &buf[2..end];
    let fin = buf[end];
    let n = end + 1;
    let nums: Vec<u16> = std::str::from_utf8(params)
        .unwrap_or("")
        .split(';')
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    let c = match (fin, nums.as_slice()) {
        (b'R', [row, col]) => {
            return Some((
                Input::Cursor(CursorReport {
                    row: *row,
                    col: *col,
                }),
                n,
            ));
        }
        (b'C' | b'B', _) | (b'~', [6, ..]) => Command::Next,
        (b'D' | b'A', _) | (b'~', [5, ..]) => Command::Prev,
        (b'H', _) | (b'~', [1 | 7, ..]) => Command::First,
        (b'F', _) | (b'~', [4 | 8, ..]) => Command::Last,
        _ => Command::Ignore,
    };
    key(c, n)
}

/// The deck and where we are in it.
#[derive(Debug)]
pub struct Deck {
    pub slides: Vec<String>,
    pub index: usize,
    /// Digits typed toward "go to slide N".
    pub pending: Option<usize>,
}

/// What the deck wants after a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Redraw,
    /// Only the status line changed.
    Status,
    Reload,
    Quit,
    Nothing,
}

impl Deck {
    #[must_use]
    pub fn new(slides: Vec<String>) -> Self {
        Self {
            slides,
            index: 0,
            pending: None,
        }
    }

    fn go(&mut self, to: usize) -> Step {
        let to = to.min(self.slides.len().saturating_sub(1));
        if to == self.index {
            Step::Nothing
        } else {
            self.index = to;
            Step::Redraw
        }
    }

    /// Apply a command.
    pub fn apply(&mut self, cmd: Command) -> Step {
        let had_pending = self.pending.is_some();
        let step = match cmd {
            Command::Digit(d) => {
                let n = self.pending.unwrap_or(0).saturating_mul(10) + usize::from(d);
                self.pending = Some(n.min(99_999));
                return Step::Status;
            }
            Command::Backspace if had_pending => {
                self.pending = self.pending.and_then(|n| (n >= 10).then_some(n / 10));
                return Step::Status;
            }
            Command::Enter if had_pending => {
                let n = self.pending.take().unwrap_or(1);
                self.go(n.max(1) - 1)
            }
            Command::Next | Command::Enter => self.go(self.index + 1),
            Command::Prev | Command::Backspace => self.go(self.index.saturating_sub(1)),
            Command::First => self.go(0),
            Command::Last => self.go(usize::MAX),
            Command::Reload => Step::Reload,
            Command::Quit => return Step::Quit,
            Command::Ignore => return Step::Nothing,
        };
        self.pending = None;
        if step == Step::Nothing && had_pending {
            Step::Status
        } else {
            step
        }
    }

    /// Replace the slides (file changed), keeping the position in range.
    pub fn replace(&mut self, slides: Vec<String>) {
        self.slides = slides;
        self.index = self.index.min(self.slides.len().saturating_sub(1));
    }

    /// The page counter for the last row, right-aligned in `cols`.
    #[must_use]
    pub fn status(&self, cols: usize) -> String {
        let text = match self.pending {
            Some(n) => format!("go to {n}_  "),
            None => format!("{} / {}  ", self.index + 1, self.slides.len()),
        };
        let w = text.chars().count();
        if w >= cols {
            text
        } else {
            format!("{}{text}", " ".repeat(cols - w))
        }
    }
}

/// Rows above a block of `height` rows that centre it on a screen of
/// `rows` rows, keeping the last row for the status line. `None` when
/// the block doesn't fit (it is drawn from the top and may scroll).
#[must_use]
pub fn centre_offset(height: u16, rows: u16) -> Option<u16> {
    let usable = rows.checked_sub(1)?;
    (height <= usable).then(|| (usable - height) / 2)
}

/// Read and split the deck file.
fn load(path: &Path) -> anyhow::Result<Vec<String>> {
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("toastty present: {}: {e}", path.display()))?;
    let src = String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("toastty present: {}: not valid UTF-8", path.display()))?;
    let slides = split_slides(&src);
    if slides.is_empty() {
        anyhow::bail!("toastty present: {}: no slides", path.display());
    }
    Ok(slides)
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Puts the tty in raw mode and restores it (and the cursor) on drop.
struct RawTty {
    tty: File,
    saved: rustix::termios::Termios,
}

impl RawTty {
    fn new(tty: File) -> anyhow::Result<Self> {
        use rustix::termios::{OptionalActions, OutputModes, tcgetattr, tcsetattr};
        let saved = tcgetattr(tty.as_fd())?;
        let mut raw = saved.clone();
        raw.make_raw();
        // Keep `\n` → `\r\n` for the plain-text fallback.
        raw.output_modes |= OutputModes::OPOST | OutputModes::ONLCR;
        tcsetattr(tty.as_fd(), OptionalActions::Flush, &raw)?;
        Ok(Self { tty, saved })
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        // Leave a clean screen, cursor home and visible.
        let _ = self
            .tty
            .write_all(b"\x1b[?2026l\x1b[0m\x1b[H\x1b[2J\x1b[?25h");
        let _ = self.tty.flush();
        let _ = rustix::termios::tcsetattr(
            self.tty.as_fd(),
            rustix::termios::OptionalActions::Flush,
            &self.saved,
        );
    }
}

struct Presenter {
    raw: RawTty,
    path: PathBuf,
    cwd: Option<PathBuf>,
    rich: bool,
    deck: Deck,
    /// Bytes read but not decoded yet.
    input: Vec<u8>,
    /// Size the current slide was drawn at.
    drawn_size: (u16, u16),
    /// Whether the current slide left the last row free for the status.
    status_row: bool,
}

impl Presenter {
    fn size(&self) -> (u16, u16) {
        rustix::termios::tcgetwinsize(self.raw.tty.as_fd())
            .map_or((80, 24), |w| (w.ws_col.max(1), w.ws_row.max(1)))
    }

    /// Wait up to `timeout` for input; append what arrives to `input`.
    /// Returns whether any bytes came.
    fn read_some(&mut self, timeout: Duration) -> anyhow::Result<bool> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let ts = Timespec {
            tv_sec: i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(timeout.subsec_nanos()),
        };
        let mut fds = [PollFd::new(&self.raw.tty, PollFlags::IN)];
        match poll(&mut fds, Some(&ts)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => return Ok(false),
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
        let mut buf = [0u8; 1024];
        let n = self.raw.tty.read(&mut buf)?;
        if n == 0 {
            anyhow::bail!("toastty present: terminal closed");
        }
        self.input.extend_from_slice(&buf[..n]);
        Ok(true)
    }

    /// Ask for the cursor position and wait (briefly) for the reply.
    /// Keys that arrive meanwhile stay queued in `input`.
    fn cursor_row(&mut self) -> anyhow::Result<Option<u16>> {
        self.raw.tty.write_all(b"\x1b[6n")?;
        self.raw.tty.flush()?;
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let mut i = 0;
            while i < self.input.len() {
                match decode(&self.input[i..], false) {
                    Some((Input::Cursor(r), n)) => {
                        self.input.drain(i..i + n);
                        return Ok(Some(r.row));
                    }
                    Some((_, n)) => i += n,
                    None => break,
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.read_some(left)? {
                return Ok(None);
            }
        }
    }

    fn slide_bytes(&self) -> Vec<u8> {
        let env = |k: &str| std::env::var(k).ok();
        let src = &self.deck.slides[self.deck.index];
        md_cmd::plan(src, self.cwd.as_deref(), &env).stdout
    }

    fn status_bytes(&self, cols: u16, rows: u16) -> Vec<u8> {
        format!(
            "\x1b[{rows};1H\x1b[2K\x1b[2m{}\x1b[0m",
            self.deck.status(usize::from(cols))
        )
        .into_bytes()
    }

    fn draw(&mut self) -> anyhow::Result<()> {
        let (cols, rows) = self.size();
        self.drawn_size = (cols, rows);
        let slide = self.slide_bytes();
        let mut out = Vec::with_capacity(slide.len() * 2 + 64);
        out.extend_from_slice(b"\x1b[?2026h\x1b[?25l\x1b[0m\x1b[H\x1b[2J");
        out.extend_from_slice(&slide);
        self.raw.tty.write_all(&out)?;
        // The block leaves the cursor at column 0 of the row below it,
        // so the row number of the cursor is the block's height + 1.
        let fits = match self.cursor_row()? {
            Some(row) if self.rich => {
                let height = row.saturating_sub(1);
                // A block taller than the screen scrolled; the cursor is
                // then on the last row, so a full-height reply means
                // "doesn't fit" either way.
                centre_offset(height, rows).filter(|_| row < rows)
            }
            Some(row) => (row < rows).then_some(0),
            None => None,
        };
        self.status_row = fits.is_some();
        let mut out = Vec::new();
        if let Some(top) = fits {
            if top > 0 {
                out.extend_from_slice(b"\x1b[H\x1b[2J");
                out.extend_from_slice(format!("\x1b[{};1H", top + 1).as_bytes());
                out.extend_from_slice(&slide);
            }
            out.extend_from_slice(&self.status_bytes(cols, rows));
        }
        out.extend_from_slice(b"\x1b[?2026l");
        self.raw.tty.write_all(&out)?;
        self.raw.tty.flush()?;
        Ok(())
    }

    fn draw_status(&mut self) -> anyhow::Result<()> {
        // Only when the last row is ours (the slide fit); otherwise
        // writing there would overwrite part of the block.
        if !self.status_row {
            return Ok(());
        }
        let (cols, rows) = self.size();
        let out = [
            b"\x1b7".as_slice(),
            &self.status_bytes(cols, rows),
            b"\x1b8",
        ]
        .concat();
        self.raw.tty.write_all(&out)?;
        self.raw.tty.flush()?;
        Ok(())
    }

    fn reload(&mut self) -> anyhow::Result<()> {
        if let Ok(slides) = load(&self.path) {
            self.deck.replace(slides);
        }
        self.draw()
    }

    fn run(&mut self) -> anyhow::Result<()> {
        // Push whatever is on screen into the scrollback, so the shell
        // session is still there to scroll back to after the talk.
        let (_, rows) = self.size();
        let push = format!("\x1b[{rows};1H{}", "\n".repeat(usize::from(rows)));
        self.raw.tty.write_all(push.as_bytes())?;
        self.draw()?;
        let mut last_mtime = mtime(&self.path);
        loop {
            let got = self.read_some(Duration::from_millis(250))?;
            if !got {
                // A lone ESC that stayed alone is a real Escape press.
                if self.input == [0x1b] {
                    self.input.clear();
                    return Ok(());
                }
                if self.size() != self.drawn_size {
                    self.draw()?;
                }
                let m = mtime(&self.path);
                if m != last_mtime {
                    last_mtime = m;
                    self.reload()?;
                }
                continue;
            }
            while let Some((ev, n)) = decode(&self.input, false) {
                self.input.drain(..n);
                let Input::Key(cmd) = ev else { continue };
                match self.deck.apply(cmd) {
                    Step::Redraw => self.draw()?,
                    Step::Status => self.draw_status()?,
                    Step::Reload => self.reload()?,
                    Step::Quit => return Ok(()),
                    Step::Nothing => {}
                }
            }
        }
    }
}

/// Run `toastty present FILE`.
///
/// # Errors
/// The file can't be read or has no slides, or there is no terminal.
pub fn run(file: &Path) -> anyhow::Result<()> {
    let slides = load(file)?;
    let tty = File::options()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| anyhow::anyhow!("toastty present: needs a terminal (/dev/tty: {e})"))?;
    let env = |k: &str| std::env::var(k).ok();
    let rich = md_cmd::detect_multiplexer(&env).is_none()
        && env("TERM_PROGRAM").as_deref() == Some("toastty");
    let cwd = md_cmd::base_dir_for(Some(file), std::env::current_dir().ok());
    let mut p = Presenter {
        raw: RawTty::new(tty)?,
        path: file.to_path_buf(),
        cwd,
        rich,
        deck: Deck::new(slides),
        input: Vec::new(),
        drawn_size: (0, 0),
        status_row: false,
    };
    p.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_rule_lines() {
        let s = split_slides("# One\n\ntext\n\n---\n\n# Two\n---\n# Three\n");
        assert_eq!(s, ["# One\n\ntext\n", "# Two\n", "# Three\n"]);
    }

    #[test]
    fn rules_inside_code_fences_are_kept() {
        let src = "# A\n\n```yaml\n---\nkey: v\n---\n```\n\n---\n\n~~~~\n---\n~~~\n---\n~~~~\n";
        let s = split_slides(src);
        assert_eq!(s.len(), 2, "{s:?}");
        assert!(s[0].contains("```yaml\n---\nkey: v\n---\n```"));
        assert_eq!(s[1], "~~~~\n---\n~~~\n---\n~~~~\n");
    }

    #[test]
    fn other_rules_and_indented_dashes_are_not_separators() {
        let s = split_slides("a\n\n***\n\nb\n\n----\n\n    ---\n");
        assert_eq!(s.len(), 1);
        assert!(is_separator("---"));
        assert!(is_separator("   ---  "));
        assert!(!is_separator("    ---"));
        assert!(!is_separator("- - -"));
    }

    #[test]
    fn front_matter_is_skipped() {
        let s = split_slides("---\nmarp: true\ntheme: gaia\n---\n\n# Title\n---\n# Two\n");
        assert_eq!(s, ["# Title\n", "# Two\n"]);
        // Not YAML: the leading `---` is a separator and the first
        // (empty) slide is dropped.
        let s = split_slides("---\n# Title\n\nSome text.\n---\n# Two\n");
        assert_eq!(s, ["# Title\n\nSome text.\n", "# Two\n"]);
    }

    #[test]
    fn empty_slides_are_dropped_and_crlf_works() {
        let s = split_slides("---\n\n---\n# A\r\n---\r\n```\r\nx\r\n```\r\n---\n\n");
        assert_eq!(s, ["# A\n", "```\nx\n```\n"]);
    }

    #[test]
    fn comments_become_speaker_notes() {
        let s =
            split_slides("# A\n<!-- note -->\ntext <!-- x --> more\n<!--\nmulti\nline\n-->\nend\n");
        assert_eq!(s, ["# A\ntext  more\nend\n"]);
        // Comments inside code are literal.
        let s = split_slides("`<!-- a -->` and ``x ` <!-- b -->`` <!-- c -->\n");
        assert_eq!(s, ["`<!-- a -->` and ``x ` <!-- b -->`` \n"]);
        let s = split_slides("```html\n<!-- keep -->\n```\n");
        assert_eq!(s, ["```html\n<!-- keep -->\n```\n"]);
    }

    #[test]
    fn decodes_keys() {
        let k = |b: &[u8]| decode(b, true);
        assert_eq!(k(b" "), Some((Input::Key(Command::Next), 1)));
        assert_eq!(k(b"\x1b[C"), Some((Input::Key(Command::Next), 3)));
        assert_eq!(k(b"\x1b[D"), Some((Input::Key(Command::Prev), 3)));
        assert_eq!(k(b"\x1bOC"), Some((Input::Key(Command::Next), 3)));
        assert_eq!(k(b"\x1b[5~"), Some((Input::Key(Command::Prev), 4)));
        assert_eq!(k(b"\x1b[6~"), Some((Input::Key(Command::Next), 4)));
        assert_eq!(k(b"\x1b[H"), Some((Input::Key(Command::First), 3)));
        assert_eq!(k(b"\x1b[4~"), Some((Input::Key(Command::Last), 4)));
        assert_eq!(k(b"\x1b[1;5C"), Some((Input::Key(Command::Next), 6)));
        assert_eq!(k(b"7"), Some((Input::Key(Command::Digit(7)), 1)));
        assert_eq!(k(b"\r"), Some((Input::Key(Command::Enter), 1)));
        assert_eq!(k(b"q"), Some((Input::Key(Command::Quit), 1)));
        assert_eq!(k(b"\x03"), Some((Input::Key(Command::Quit), 1)));
        assert_eq!(k(b"\x1b"), Some((Input::Key(Command::Quit), 1)));
        // Mid-sequence: wait for more.
        assert_eq!(decode(b"\x1b", false), None);
        assert_eq!(decode(b"\x1b[", true), None);
        assert_eq!(decode(b"\x1b[12;4", true), None);
    }

    #[test]
    fn decodes_cursor_reports_between_keys() {
        let buf = b"\x1b[12;1Rl";
        let (ev, n) = decode(buf, false).unwrap();
        assert_eq!(ev, Input::Cursor(CursorReport { row: 12, col: 1 }));
        assert_eq!(
            decode(&buf[n..], false).unwrap().0,
            Input::Key(Command::Next)
        );
    }

    #[test]
    fn deck_navigation_clamps() {
        let mut d = Deck::new(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(d.apply(Command::Prev), Step::Nothing);
        assert_eq!(d.apply(Command::Next), Step::Redraw);
        assert_eq!(d.apply(Command::Last), Step::Redraw);
        assert_eq!(d.index, 2);
        assert_eq!(d.apply(Command::Next), Step::Nothing);
        assert_eq!(d.apply(Command::First), Step::Redraw);
        assert_eq!(d.index, 0);
        assert_eq!(d.apply(Command::Quit), Step::Quit);
    }

    #[test]
    fn typed_numbers_jump() {
        let mut d = Deck::new((0..12).map(|i| i.to_string()).collect());
        assert_eq!(d.apply(Command::Digit(1)), Step::Status);
        assert_eq!(d.apply(Command::Digit(1)), Step::Status);
        assert_eq!(d.status(14), "   go to 11_  ");
        assert_eq!(d.apply(Command::Backspace), Step::Status);
        assert_eq!(d.pending, Some(1));
        assert_eq!(d.apply(Command::Digit(0)), Step::Status);
        assert_eq!(d.apply(Command::Enter), Step::Redraw);
        assert_eq!(d.index, 9);
        assert_eq!(d.pending, None);
        // Past the end clamps; 0 means the first slide.
        d.apply(Command::Digit(9));
        d.apply(Command::Digit(9));
        d.apply(Command::Enter);
        assert_eq!(d.index, 11);
        d.apply(Command::Digit(0));
        d.apply(Command::Enter);
        assert_eq!(d.index, 0);
        // Navigating clears a half-typed number.
        d.apply(Command::Digit(5));
        assert_eq!(d.apply(Command::Next), Step::Redraw);
        assert_eq!(d.pending, None);
        // Enter / Backspace with nothing typed move.
        assert_eq!(d.apply(Command::Enter), Step::Redraw);
        assert_eq!(d.index, 2);
        assert_eq!(d.apply(Command::Backspace), Step::Redraw);
        assert_eq!(d.index, 1);
    }

    #[test]
    fn reload_keeps_position_in_range() {
        let mut d = Deck::new(vec!["a".into(), "b".into(), "c".into()]);
        d.apply(Command::Last);
        d.replace(vec!["x".into()]);
        assert_eq!(d.index, 0);
        assert_eq!(d.status(8), "  1 / 1  ".chars().skip(1).collect::<String>());
    }

    #[test]
    fn centring() {
        assert_eq!(centre_offset(10, 25), Some(7));
        assert_eq!(centre_offset(24, 25), Some(0));
        assert_eq!(centre_offset(25, 25), None);
        assert_eq!(centre_offset(1, 1), None);
    }

    #[test]
    fn slides_are_sent_as_md_blocks() {
        let s = split_slides("# One\n---\n# Two\n");
        let env = |k: &str| (k == "TERM_PROGRAM").then(|| "toastty".to_string());
        for slide in &s {
            let out = md_cmd::plan(slide, None, &env).stdout;
            let text = std::str::from_utf8(&out).unwrap();
            let body = &text[2..text.len() - 2];
            let parts: Vec<&[u8]> = body.split(';').map(str::as_bytes).collect();
            let parsed = toastty_term::parse_osc_7700(&parts).unwrap();
            assert_eq!(&parsed.source, slide);
        }
    }
}
