//! Demo: spawn `/bin/sh -c '...'` with a few commands and print output.
//!
//! Run with: `cargo run -p toastty-pty --example spawn_shell`

use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};
use toastty_pty::{Pty, PtySpec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let script = "echo line one; echo line two; pwd; date; uname -s";
    let spec = PtySpec::program("/bin/sh").arg("-c").arg(script);
    let mut pty = Pty::spawn(&spec)?;
    pty.set_nonblocking(true)?;

    println!("--- PTY output ---");
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut child_exited = false;

    while Instant::now() < deadline {
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                std::io::stdout().write_all(&buf[..n])?;
                std::io::stdout().flush()?;
            }
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
        if !child_exited && pty.try_wait()?.is_some() {
            child_exited = true;
            thread::sleep(Duration::from_millis(20));
        } else if child_exited {
            break;
        }
    }
    println!("--- end ---");
    Ok(())
}
