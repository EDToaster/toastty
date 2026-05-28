//! End-to-end (sans-renderer) integration: PTY → mio reader → parser → Term.
//!
//! Builds the same data pipeline `main.rs` does without the wgpu/winit
//! halves. Exercises the `Vec<u8>` payload path between
//! `toastty-io::spawn_pty_reader_with_sink` and the binary's parser.

use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use toastty_io::{EventSink, SinkClosed, UserEvent, spawn_pty_reader_with_sink};
use toastty_parser::Parser;
use toastty_pty::{Pty, PtySpec, WinSize};
use toastty_term::Term;

use toastty::geometry::grid_dims_from_pixels;
use toastty::keyboard::encode_key;
use toastty::shell::resolve_shell;
use toastty_config::ShellConfig;
use toastty_window::{KeyState, LogicalKey, Modifiers, NamedKey};

struct ChannelSink(Mutex<mpsc::Sender<UserEvent>>);
impl EventSink for ChannelSink {
    fn send(&self, ev: UserEvent) -> Result<(), SinkClosed> {
        let g = self.0.lock().map_err(|_| SinkClosed)?;
        g.send(ev).map_err(|_| SinkClosed)
    }
}

#[test]
fn pty_echo_drives_term_through_parser() {
    // Spawn `echo hello` under a PTY at a known grid size.
    let spec = PtySpec::program("/bin/echo")
        .arg("hello, M5")
        .size(WinSize {
            rows: 24,
            cols: 80,
            pixel_width: 1280,
            pixel_height: 800,
        });
    let pty = Pty::spawn(&spec).expect("spawn echo");

    let (tx, rx) = mpsc::channel();
    let sink = ChannelSink(Mutex::new(tx));
    let _reader = spawn_pty_reader_with_sink(pty.master_fd(), sink).expect("reader");

    let mut parser = Parser::new();
    let mut term = Term::new(24, 80, 0);

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut closed = false;
    while !closed && Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(UserEvent::PtyBytes(b)) => parser.advance(&mut term, &b),
            Ok(UserEvent::PtyClosed) => closed = true,
            Err(_) => {}
        }
    }
    assert!(closed, "expected PtyClosed event");

    // Find "hello, M5" somewhere on the visible grid.
    let mut found = false;
    let (rows, _) = term.size();
    for r in 0..rows {
        let row = term.row(r);
        let s: String = row
            .cells
            .iter()
            .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
            .collect();
        if s.contains("hello, M5") {
            found = true;
            break;
        }
    }
    assert!(found, "expected `hello, M5` somewhere on the grid");
    drop(pty);
}

#[test]
fn keyboard_encoder_round_trips_through_cat() {
    // `cat` echoes whatever we write. Encode some keys, write them in,
    // confirm the parsed grid contains the bytes.
    let spec = PtySpec::program("/bin/cat").size(WinSize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    });
    let pty = Pty::spawn(&spec).expect("spawn cat");

    let (tx, rx) = mpsc::channel();
    let sink = ChannelSink(Mutex::new(tx));
    let _reader = spawn_pty_reader_with_sink(pty.master_fd(), sink).expect("reader");

    // Encode "hi\r" the way the binary would.
    let bytes_h = encode_key(
        &LogicalKey::Character("h".into()),
        Some("h"),
        Modifiers::empty(),
        0,
        KeyState::Pressed,
        false,
    )
    .unwrap();
    let bytes_i = encode_key(
        &LogicalKey::Character("i".into()),
        Some("i"),
        Modifiers::empty(),
        0,
        KeyState::Pressed,
        false,
    )
    .unwrap();
    let bytes_enter = encode_key(
        &LogicalKey::Named(NamedKey::Enter),
        None,
        Modifiers::empty(),
        0,
        KeyState::Pressed,
        false,
    )
    .unwrap();
    pty.write(&bytes_h).unwrap();
    pty.write(&bytes_i).unwrap();
    pty.write(&bytes_enter).unwrap();

    let mut parser = Parser::new();
    let mut term = Term::new(24, 80, 0);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut total = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(UserEvent::PtyBytes(b)) => {
                total.extend_from_slice(&b);
                parser.advance(&mut term, &b);
                let s = String::from_utf8_lossy(&total);
                if s.contains("hi") {
                    break;
                }
            }
            Ok(UserEvent::PtyClosed) => break,
            Err(_) => {}
        }
    }
    let s = String::from_utf8_lossy(&total);
    assert!(s.contains("hi"), "cat echo missing 'hi': {s:?}");
    drop(pty);
}

#[test]
fn resize_propagates_through_pty_and_term() {
    // Spawn a long-running sh; resize; ask for stty size; verify the new
    // dimensions land on the grid.
    let spec = PtySpec::program("/bin/sh").size(WinSize {
        rows: 24,
        cols: 80,
        pixel_width: 800,
        pixel_height: 480,
    });
    let mut pty = Pty::spawn(&spec).expect("spawn sh");

    let (tx, rx) = mpsc::channel();
    let sink = ChannelSink(Mutex::new(tx));
    let _reader = spawn_pty_reader_with_sink(pty.master_fd(), sink).expect("reader");

    // Resize using the same helper main.rs uses.
    let (cols, rows) = grid_dims_from_pixels(1280, 800, 10.0, 20.0);
    assert_eq!((cols, rows), (128, 40));
    pty.resize(WinSize {
        rows,
        cols,
        pixel_width: 1280,
        pixel_height: 800,
    })
    .expect("resize");

    pty.write(b"stty size\n").expect("write stty");
    pty.write(b"exit\n").expect("write exit");

    let mut parser = Parser::new();
    let mut term = Term::new(rows, cols, 0);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut closed = false;
    let mut all = Vec::new();
    while !closed && Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(UserEvent::PtyBytes(b)) => {
                all.extend_from_slice(&b);
                parser.advance(&mut term, &b);
            }
            Ok(UserEvent::PtyClosed) => closed = true,
            Err(_) => {}
        }
    }
    let s = String::from_utf8_lossy(&all);
    assert!(
        s.contains("40 128"),
        "expected '40 128' in shell output: {s:?}"
    );

    drop(pty);
}

/// End-to-end mode handshake: feed `\x1b[?2004h` into the parser via a real
/// PTY, then verify `Term::bracketed_paste()` is on.
#[test]
fn pty_drives_bracketed_paste_mode_through_term() {
    let spec = PtySpec::program("/bin/sh")
        .args(["-c", "printf '\\033[?2004h'; exit 0"])
        .size(WinSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
        });
    let pty = Pty::spawn(&spec).expect("spawn sh");

    let (tx, rx) = mpsc::channel();
    let sink = ChannelSink(Mutex::new(tx));
    let _reader = spawn_pty_reader_with_sink(pty.master_fd(), sink).expect("reader");

    let mut parser = Parser::new();
    let mut term = Term::new(24, 80, 0);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(UserEvent::PtyBytes(b)) => {
                parser.advance(&mut term, &b);
            }
            Ok(UserEvent::PtyClosed) => break,
            Err(_) => {}
        }
        if term.bracketed_paste() {
            break;
        }
    }
    assert!(term.bracketed_paste(), "DECSET 2004 should set bracketed_paste");
    drop(pty);
}

/// End-to-end mode handshake: same idea for focus reporting (DECSET 1004).
#[test]
fn pty_drives_focus_reporting_mode_through_term() {
    let spec = PtySpec::program("/bin/sh")
        .args(["-c", "printf '\\033[?1004h'; exit 0"])
        .size(WinSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
        });
    let pty = Pty::spawn(&spec).expect("spawn sh");

    let (tx, rx) = mpsc::channel();
    let sink = ChannelSink(Mutex::new(tx));
    let _reader = spawn_pty_reader_with_sink(pty.master_fd(), sink).expect("reader");

    let mut parser = Parser::new();
    let mut term = Term::new(24, 80, 0);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(UserEvent::PtyBytes(b)) => {
                parser.advance(&mut term, &b);
            }
            Ok(UserEvent::PtyClosed) => break,
            Err(_) => {}
        }
        if term.report_focus() {
            break;
        }
    }
    assert!(term.report_focus(), "DECSET 1004 should enable focus reporting");
    drop(pty);
}

#[test]
fn shift_letter_under_kitty_disambiguate_writes_plain_uppercase_to_pty() {
    // Repro for the helix-in-zellij bug: zellij enables kitty
    // disambiguate (CSI > 1 u). With only that flag set, the spec says
    // Shift+E goes on the wire as the byte "E", not as "CSI 101;2 u".
    // If we emit the CSI u form here, zellij decodes "lowercase e +
    // shift modifier" and forwards plain "e" to the inner child, so
    // capital letters never reach helix.
    //
    // We round-trip the encoded bytes through `cat` and confirm the
    // PTY hands back "HE" — not "\x1b[101;2u" or "he".
    let spec = PtySpec::program("/bin/cat").size(WinSize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    });
    let pty = Pty::spawn(&spec).expect("spawn cat");

    let (tx, rx) = mpsc::channel();
    let sink = ChannelSink(Mutex::new(tx));
    let _reader = spawn_pty_reader_with_sink(pty.master_fd(), sink).expect("reader");

    // Flag bit 1 == disambiguate-only, the same value zellij pushes.
    let kitty_flags = 1u8;
    let bytes_h = encode_key(
        &LogicalKey::Character("h".into()),
        Some("H"),
        Modifiers::SHIFT,
        kitty_flags,
        KeyState::Pressed,
        false,
    )
    .expect("Shift+H must encode");
    let bytes_e = encode_key(
        &LogicalKey::Character("e".into()),
        Some("E"),
        Modifiers::SHIFT,
        kitty_flags,
        KeyState::Pressed,
        false,
    )
    .expect("Shift+E must encode");
    // Direct byte-level check before the PTY round-trip: this is the
    // exact moral inverse of the bytes seen in /tmp/toastty.log.
    assert_eq!(bytes_h, b"H");
    assert_eq!(bytes_e, b"E");

    let bytes_enter = encode_key(
        &LogicalKey::Named(NamedKey::Enter),
        None,
        Modifiers::empty(),
        kitty_flags,
        KeyState::Pressed,
        false,
    )
    .unwrap();
    pty.write(&bytes_h).unwrap();
    pty.write(&bytes_e).unwrap();
    pty.write(&bytes_enter).unwrap();

    let mut parser = Parser::new();
    let mut term = Term::new(24, 80, 0);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut total = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(UserEvent::PtyBytes(b)) => {
                total.extend_from_slice(&b);
                parser.advance(&mut term, &b);
                let s = String::from_utf8_lossy(&total);
                if s.contains("HE") {
                    break;
                }
            }
            Ok(UserEvent::PtyClosed) => break,
            Err(_) => {}
        }
    }
    let s = String::from_utf8_lossy(&total);
    assert!(s.contains("HE"), "cat echo missing 'HE': {s:?}");
    assert!(
        !s.contains("\x1b[101;2u"),
        "raw CSI-u for Shift+E leaked onto the wire: {s:?}",
    );
    drop(pty);
}

#[test]
fn shell_resolution_falls_back_to_bin_sh() {
    let cfg = ShellConfig {
        program: "auto".into(),
        args: vec![],
    };
    let (prog, _args) = resolve_shell(&cfg);
    // We can't assert exactly because $SHELL may be set; just ensure
    // the path is plausible.
    let s = prog.to_string_lossy();
    assert!(!s.is_empty());
}
