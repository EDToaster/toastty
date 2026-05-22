//! Demo: spawn a shell script that emits a mix of escape sequences,
//! pipe the output through the parser, and log every event.
//!
//! Run with: `cargo run -p toastty-parser --example parse_log`

use std::thread;
use std::time::{Duration, Instant};
use toastty_parser::{Params, Parser, Perform};
use toastty_pty::{Pty, PtySpec};

struct Logger;

fn fmt_params(params: &Params) -> String {
    let vs: Vec<Vec<u16>> = params.iter().map(<[u16]>::to_vec).collect();
    format!("{vs:?}")
}

impl Perform for Logger {
    fn print(&mut self, c: char) {
        println!("print  {c:?}");
    }
    fn execute(&mut self, byte: u8) {
        let name = match byte {
            0x07 => "BEL",
            0x08 => "BS",
            0x09 => "HT",
            0x0A => "LF",
            0x0D => "CR",
            _ => "?",
        };
        println!("exec   {byte:#04x} ({name})");
    }
    fn csi_dispatch(
        &mut self,
        params: &Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        println!(
            "CSI    {} intermediates={:?} action={:?}",
            fmt_params(params),
            std::str::from_utf8(intermediates).unwrap_or("<non-utf8>"),
            action,
        );
    }
    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let s: Vec<String> = params
            .iter()
            .map(|p| String::from_utf8_lossy(p).to_string())
            .collect();
        let term = if bell_terminated { "BEL" } else { "ST" };
        println!("OSC    {s:?} ({term}-terminated)");
    }
    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        println!(
            "ESC    intermediates={:?} byte={:?}",
            std::str::from_utf8(intermediates).unwrap_or("<non-utf8>"),
            byte as char,
        );
    }
    fn hook(&mut self, params: &Params, intermediates: &[u8], _: bool, action: char) {
        println!(
            "DCS<<  {} intermediates={:?} action={:?}",
            fmt_params(params),
            std::str::from_utf8(intermediates).unwrap_or("<non-utf8>"),
            action,
        );
    }
    fn put(&mut self, byte: u8) {
        println!("DCS..  {byte:#04x}");
    }
    fn unhook(&mut self) {
        println!("DCS>>");
    }
    fn apc_start(&mut self) {
        println!("APC<<");
    }
    fn apc_chunk(&mut self, bytes: &[u8]) {
        println!("APC..  {} bytes", bytes.len());
    }
    fn apc_end(&mut self) {
        println!("APC>>");
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A script that emits one of each event class we care about.
    let script = r"
echo plain
printf '\033[31mRED \033[0m'
printf '\033[1;32mBOLD\033[0m\n'
printf '\033]0;Window Title\033\\'
printf '\033]8;;https://example.com\007Hyperlink\033]8;;\033\\\n'
printf '\033[2J\033[H'
printf '\033Pq0\033\\'
printf '\033_Gf=24,s=1;hello-apc\033\\'
echo done
";

    let spec = PtySpec::program("/bin/sh").arg("-c").arg(script);
    let mut pty = Pty::spawn(&spec)?;
    pty.set_nonblocking(true)?;

    let mut parser = Parser::new();
    let mut logger = Logger;

    println!("--- events ---");

    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut child_exited = false;
    while Instant::now() < deadline {
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => parser.advance(&mut logger, &buf[..n]),
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
        if !child_exited && pty.try_wait()?.is_some() {
            child_exited = true;
            thread::sleep(Duration::from_millis(30));
        } else if child_exited {
            break;
        }
    }

    println!("--- end ---");
    Ok(())
}
