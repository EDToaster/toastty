//! Demo: spawn a small bash script under a PTY, parse its output through
//! `toastty-term`, then print the resulting grid to stdout as plain text.
//!
//! This exercises the parser → term → grid path end-to-end. There's no
//! color rendering yet — that's the renderer milestone. Use it to verify
//! that printed text lands on the right row/column after SGR + cursor
//! moves.
//!
//! Run with: `cargo run -p toastty-term --example render_to_stdout`

use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};
use toastty_parser::Parser;
use toastty_pty::{Pty, PtySpec, WinSize};
use toastty_term::Term;

const ROWS: u16 = 12;
const COLS: u16 = 60;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A handful of SGR + cursor-motion sequences that we know the M3 term
    // handles: simple colors, bold-green, a couple of cursor moves, and a
    // CR-LF.
    let script = concat!(
        "printf '\\033[31mhello\\033[0m\\n';",
        "printf '\\033[1;32mbold green\\033[0m\\n';",
        // Move up 2 rows, right 30 cols, print a yellow word.
        "printf '\\033[2A\\033[30C\\033[33mover here\\033[0m\\n\\n';",
        "echo done",
    );

    let spec = PtySpec::program("/bin/bash")
        .arg("-c")
        .arg(script)
        .size(WinSize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        });
    let mut pty = Pty::spawn(&spec)?;
    pty.set_nonblocking(true)?;

    let mut term = Term::new(ROWS, COLS, 0);
    let mut parser = Parser::new();

    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut child_exited = false;

    while Instant::now() < deadline {
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => parser.advance(&mut term, &buf[..n]),
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
        if !child_exited && pty.try_wait()?.is_some() {
            child_exited = true;
            // Give the kernel a beat to make remaining bytes readable.
            thread::sleep(Duration::from_millis(20));
        } else if child_exited {
            // Drain any final bytes that arrived after the child exited.
            match pty.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => parser.advance(&mut term, &buf[..n]),
            }
        }
    }

    let (rows, cols) = term.size();
    let mut out = std::io::stdout().lock();
    writeln!(out, "--- toastty-term grid ({rows}x{cols}) ---")?;
    for r in 0..rows {
        let line: String = term.row(r).cells.iter().map(|c| c.ch).collect();
        // Trim trailing blanks for legibility.
        writeln!(out, "{}", line.trim_end())?;
    }
    writeln!(out, "--- cursor: {:?} ---", term.cursor())?;
    Ok(())
}
