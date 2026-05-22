//! Demo: spawn `/bin/echo "Hello, toastty!"` under a PTY and print
//! what comes back. Smallest possible "real" PTY usage.
//!
//! Run with: `cargo run -p toastty-pty --example spawn_echo`

use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};
use toastty_pty::{Pty, PtySpec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let spec = PtySpec::program("/bin/echo").arg("Hello, toastty!");
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
            // Give the kernel a beat to make any remaining bytes readable.
            thread::sleep(Duration::from_millis(20));
        } else if child_exited {
            break;
        }
    }
    println!("--- end ---");
    Ok(())
}
