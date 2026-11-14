//! Find the clipboard/paste size that breaks input, and show *why*.
//!
//! Hypothesis under test
//! ---------------------
//! toastty's `write_pty` (crates/toastty/src/main.rs) does ONE
//! fire-and-forget `libc::write` and **ignores the returned byte count**
//! (see `Pty::write` in crates/toastty-pty/src/pty.rs). The PTY master is
//! non-blocking in production (the mio reader dups the master and sets
//! `O_NONBLOCK`; because `dup` shares the open file description, the write
//! fd becomes non-blocking too).
//!
//! So when a paste blob is larger than the kernel's PTY input buffer, a
//! single `write()` accepts only a prefix and returns a short count. The
//! dropped tail includes the bracketed-paste terminator `\x1b[201~`, which
//! leaves the foreground app stuck in "collecting a paste" mode: plain
//! keystrokes get swallowed as paste content, while Ctrl+C (SIGINT) still
//! escapes. That matches the reported symptom.
//!
//! Measured result (macOS): in **raw mode** — what zle / helix / zellij put
//! the tty in — a single write is capped at ~1022 bytes (the tty input
//! ring), and this cap holds even when the child is actively draining
//! stdin: the kernel copies into the ring once per syscall and returns.
//! **Canonical** (cooked) mode does not truncate, which is why the bug only
//! shows up inside real TUIs / shells. This harness measures both.
//!
//! Run:  cargo run -p toastty-pty --example paste_threshold

use std::os::fd::AsRawFd;
use toastty_pty::{Pty, PtySpec};

/// Bracketed-paste markers, mirroring `toastty::paste`.
const START: &[u8] = b"\x1b[200~";
const END: &[u8] = b"\x1b[201~";

#[derive(Clone, Copy)]
enum Mode {
    /// Default line discipline (cooked). A one-line (newline-free) blob is
    /// capped at MAX_CANON here.
    Canonical,
    /// `cfmakeraw` — what a TUI / shell line editor puts the tty in.
    Raw,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Canonical => "canonical (cooked, no newline)",
            Mode::Raw => "raw (cfmakeraw)",
        }
    }
}

/// Put the tty behind `fd` into raw mode. Operates on the master fd, which
/// configures the shared line discipline.
fn make_raw(fd: std::os::fd::BorrowedFd<'_>) {
    // SAFETY: fd is a valid open tty fd; termios is fully initialised by
    // tcgetattr before use.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd.as_raw_fd(), &mut t) == 0 {
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(fd.as_raw_fd(), libc::TCSANOW, &t);
        }
    }
}

/// Spawn a child that never reads its stdin (`sleep`), so the PTY input
/// buffer fills and stays full — the deterministic stand-in for a
/// momentarily-busy app. Then do a SINGLE non-blocking write of `size`
/// bytes (exactly what `write_pty` does) and report how many the kernel
/// accepted.
fn single_write_accepted(size: usize, mode: Mode) -> usize {
    let spec = PtySpec::program("/bin/sleep").arg("30");
    let pty = Pty::spawn(&spec).expect("spawn sleep");

    if let Mode::Raw = mode {
        make_raw(pty.master_fd());
    }
    // Production condition: non-blocking master (see module docs).
    pty.set_nonblocking(true).expect("set nonblocking");

    // A one-line block: no newline anywhere in the payload.
    let blob = vec![b'x'; size];
    match pty.write(&blob) {
        Ok(n) => n,
        // WouldBlock == the buffer was already full; zero accepted.
        Err(toastty_pty::PtyError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
        Err(e) => panic!("write error: {e}"),
    }
}

/// Binary-search the largest single write fully accepted, in `[lo, hi]`
/// where `lo` is known-good and `hi` is known-truncated.
fn refine_threshold(mut lo: usize, mut hi: usize, mode: Mode) -> usize {
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if single_write_accepted(mid, mode) == mid {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

fn sweep(mode: Mode) {
    println!("\n=== {} ===", mode.label());
    println!("  {:>8}  {:>8}  {:>8}  {}", "request", "accepted", "dropped", "");

    let sizes = [
        256, 512, 768, 1024, 1280, 1536, 1792, 2048, 3072, 4096, 8192, 16384, 65536,
    ];
    let mut last_full = 0usize;
    let mut first_short = 0usize;
    for &size in &sizes {
        let accepted = single_write_accepted(size, mode);
        let dropped = size - accepted;
        let flag = if dropped > 0 { "  <-- TRUNCATED" } else { "" };
        println!("  {size:>8}  {accepted:>8}  {dropped:>8}{flag}");
        if dropped == 0 {
            last_full = size;
        } else if first_short == 0 {
            first_short = size;
        }
    }

    if first_short == 0 {
        println!("  (no truncation observed up to {} bytes)", sizes[sizes.len() - 1]);
        return;
    }

    let threshold = refine_threshold(last_full, first_short, mode);
    println!(
        "\n  THRESHOLD: a single write is fully accepted up to {threshold} bytes; \
         {} bytes truncates.",
        threshold + 1
    );

    // Show the real-world consequence: a bracketed paste whose total size
    // just clears the threshold loses its `\x1b[201~` terminator.
    let text_len = threshold + 1; // total blob = START + text + END > threshold
    let mut paste = Vec::with_capacity(START.len() + text_len + END.len());
    paste.extend_from_slice(START);
    paste.extend(std::iter::repeat(b'x').take(text_len));
    paste.extend_from_slice(END);

    let spec = PtySpec::program("/bin/sleep").arg("30");
    let pty = Pty::spawn(&spec).expect("spawn sleep");
    if let Mode::Raw = mode {
        make_raw(pty.master_fd());
    }
    pty.set_nonblocking(true).expect("set nonblocking");
    let accepted = pty.write(&paste).expect("write");
    let terminator_start = paste.len() - END.len();
    let terminator_delivered = accepted >= paste.len();
    println!(
        "  bracketed paste of {} bytes: {} accepted -> closing \\x1b[201~ {}",
        paste.len(),
        accepted,
        if terminator_delivered {
            "delivered".to_string()
        } else {
            format!(
                "DROPPED (needed byte {}, only {} written) => app stuck in paste mode",
                terminator_start, accepted
            )
        }
    );
}

fn main() {
    println!("Measuring PTY single-write acceptance (fire-and-forget, like write_pty).");
    println!("A child that never reads stdin stands in for a momentarily-busy app.");
    sweep(Mode::Canonical);
    sweep(Mode::Raw);
    println!(
        "\nFix: write_pty must loop over short writes (and retry on EWOULDBLOCK / \
         queue on writability) so the whole paste — terminator included — is delivered."
    );
}
