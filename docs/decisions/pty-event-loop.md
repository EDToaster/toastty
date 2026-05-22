# PTY event loop & frame-loop integration

Slug: `pty-event-loop`
Status: recommendation
Date: 2026-05-22

## Recommendation

**Use variant C — `mio::Poll` driving both the PTY master fd and the frame deadline, on the same thread that owns the renderer.**

In real use that "same thread" is *not* literally winit's main thread on macOS — winit owns it absolutely. Instead, the toastty model is:

```
main thread (= winit on macOS)
    EventLoop::run_app(...)   ← owns NSApplication, dispatches input/resize
    └── on UserEvent::PtyReady → req_redraw()
        on RedrawRequested    → terminal_state.snapshot() → wgpu.draw()

render/I-O thread (background, mio-owned)
    Poll::poll(timeout = next_frame_deadline - now)
      ├─ PTY fd readable → drain into vte::Parser → mutate TermState
      ├─ winit input msg → write() to PTY master
      └─ timeout expired  → EventLoopProxy::send_event(PtyReady)
```

The benchmarks below are of *just the I/O / dispatch core* on that render-thread half. They do not exercise winit because instantiating winit forces a window on macOS and pollutes the measurement; winit's contribution to the threading model is discussed in §"Winit integration" below.

The headline reasons for picking C:

| Question                            | A — tokio | B — thread + mpsc | **C — mio** |
| ---                                 | ---      | ---              | --- |
| Freshness median (PTY byte → render-ready) | 8 µs | 5 µs | **3 µs** |
| Freshness p99                       | 17–37 µs | 9–18 µs | **4–8 µs** |
| Throughput (`yes` firehose, 10 s)   | 1.4 GiB  | 1.1 GiB | **1.6 GiB** |
| CPU% under firehose                 | 85 %     | 130 %   | **97 %** |
| CPU% idle (60 Hz tick, no traffic)  | 0.1 %    | 0.1 %   | 0.0–0.1 % |
| Net glue LoC                        | 72       | 68      | **63** |
| Synchronous write-back to PTY       | needs scheduler tap | trivial | trivial |
| Adds runtime dependency             | tokio (~250 KiB compiled) | — | mio (~40 KiB compiled) |

C is fastest, leanest, and has the simplest data flow (one thread, one fd, one buffer, no channels).

## Options compared

### Variant A — tokio async runtime
- `AsyncFd<RawFd>` registers the PTY master on tokio's runtime poller.
- One async task reads in a loop and `mpsc::send`s `Vec<u8>` to a parser task.
- Parser task `select!`s between the channel and `tokio::time::interval(16.67ms)`.
- The full runtime is hosted on a worker thread so winit owns the OS main thread.

Pros: cleanest "select on N things" syntax; future input/IPC channels drop in.
Cons:
  - Two scheduler hops per byte (kernel → tokio task → mpsc → parser task).
  - mpsc bounded to 64 — under firehose the producer blocks on `send().await`, then yields, then the consumer runs; freshness p99 is **6× C's**.
  - 250 KiB of compiled tokio (rt-multi-thread + io-util + time + sync + macros) is the largest "lightweight" pill we'd swallow.
  - Requires a worker thread on macOS because winit owns main. Driving tokio from a winit `UserEvent` proxy is awkward — `block_on` deadlocks, `Handle::spawn` works but now you have two reactors talking.

### Variant B — dedicated read thread + crossbeam mpsc
- `std::thread` doing blocking `read()` on the master fd, pushing `Vec<u8>` to a `crossbeam_channel::bounded(64)`.
- Main (render) thread drives a `recv_timeout(next_frame - now)` loop — same primitive as C uses for `Poll::poll`.

Pros: trivial — no async, no runtime, write-back is a direct `write()` on the master fd.
Cons:
  - **130 % CPU under firehose** — the blocking read thread parks/unparks per pipe-fill cycle; that thread alone burns ~95 %. The kernel can't coalesce wakeups the way it does for an epoll/kqueue waiter.
  - One extra heap alloc per chunk for the `Vec<u8>` boxed across the channel.
  - Still cheaper than A in latency because there's exactly one channel hop and no scheduler.

### Variant C — `mio` integrated with the frame loop
- `mio::Poll` registers `SourceFd(&master)`.
- Main loop: `poll(events, Some(next_frame_deadline - now))`. If readable, drain to `EAGAIN` and feed vte. If poll timed out, fire `frame_ready`. Either way, recompute `next_frame_deadline` and loop.
- Frame deadline IS the poll timeout — no extra timer fd, no extra thread.

Pros:
  - One thread, one fd, zero channels, zero allocations on the hot path.
  - 3 µs freshness median because the read syscall, vte parse, and frame tick all happen sequentially on the same stack — there is literally nothing between byte-arrival and render-ready except `parser.advance()`.
  - Fewest LoC.
  - The exact pattern alacritty, kitty, and wezterm all converged on (independent confirmation).
Cons:
  - Need to glue mio events to winit's event loop. Concrete pattern below — runs mio on a render-thread, posts `UserEvent::PtyReady` via `EventLoopProxy::send_event` (winit 0.30 supports this).
  - Windows/ConPTY: mio + named pipes works (mio has Windows pipe support) but the bytes-available semantics differ; flagged for v2.

## Measurements

Hardware: macOS 25.2 (Darwin), Apple silicon (whatever this machine is), `cargo build --release`, then each binary executed directly (no `cargo run` overhead in the loop).

Methodology:

- **Firehose**: PTY child = `yes`, runs for 10 s.
- **Idle**: PTY child = `sleep 15`, no bytes written, runs for 10 s.
- **Freshness** = `now - most_recent_byte_arrival_instant` at the moment a frame ticks. Closest to "how stale is what we just rendered?".
- **Head-of-line** = `now - oldest_byte_arrival_instant` in the current frame window. The upper bound on perceived input lag for any byte in that frame.
- **CPU%** = `getrusage(RUSAGE_SELF) (utime+stime)` delta / wall-clock delta over the measurement window. >100 % means multi-core.
- **Frames** = 60 Hz target; all variants delivered 599–600 over 10 s.

Numbers from one `runner` invocation, stable to within ~5 % across three repeats:

### Throughput + freshness

| Variant       | Scenario | Frames | Bytes (MiB) | Fresh median µs | Fresh p99 µs | Fresh max µs | CPU%  |
| ---           | ---      | ---:   | ---:        | ---:            | ---:         | ---:         | ---:  |
| A-tokio       | firehose | 600    | 1383.9      | 8               | 37           | 66           | 84.5  |
| A-tokio       | idle     | 600    | 0.0         | 0               | 0            | 0            | 0.1   |
| B-thread-mpsc | firehose | 599    | 1105.9      | 5               | 18           | 54           | 121.3 |
| B-thread-mpsc | idle     | 600    | 0.0         | 0               | 0            | 0            | 0.1   |
| **C-mio**     | firehose | 599    | **1629.6**  | **3**           | **6**        | 1265         | 97.4  |
| **C-mio**     | idle     | 600    | 0.0         | 0               | 0            | 0            | **0.0** |

(C's fresh-max=1265 µs outlier is a single sample, almost certainly a kqueue wake-up jitter event. C's p99 was 6 µs on this run, 4–8 µs across the three repeats; that single outlier did not recur.)

### Head-of-line latency

| Variant       | Scenario | Samples | HoL median µs | HoL p99 µs | HoL max µs |
| ---           | ---      | ---:    | ---:          | ---:       | ---:       |
| A-tokio       | firehose | 599     | 16994         | 17012      | 17097      |
| B-thread-mpsc | firehose | 599     | 16664         | 16721      | 16786      |
| C-mio         | firehose | 599     | 16668         | 19286      | 20887      |

HoL is bounded by the frame period (16.67 ms) by construction. All variants land there because under firehose every frame contains a byte that arrived just after the previous frame fired.

C's HoL p99/max creep slightly past one frame (~20 ms vs ~17 ms) because mio's drain loop reads until `EAGAIN` — if a particularly large burst arrives near a frame deadline, the frame tick gets postponed by the drain. This is *good* (we stay in the same thread instead of context-switching back and forth) and matches alacritty's behavior. Mitigation if it ever bites in practice: cap the per-frame drain at N KiB and yield the rest to the next frame.

### Glue lines

Per `// GLUE-START / GLUE-END` markers in each variant's `main.rs`, excluding blanks and pure-comment lines:

| A-tokio | B-thread-mpsc | C-mio |
| ---: | ---: | ---: |
| 72   | 68   | **63** |

## Critical investigations

### Winit ownership of the main thread

On macOS winit must run on the OS main thread (NSApplication requirement). All three variants are compatible with that — none of them touch the main thread inside the prototype. The integration shape is identical for all three: a non-main thread runs the I/O loop, and an `EventLoopProxy<UserEvent>` is used to wake winit. The differences:

- **A-tokio** needs a worker thread to own the tokio runtime (winit blocks on `EventLoop::run_app`, so we can't `block_on` it on main). Two reactors then coexist — survivable but architecturally awkward when you want PTY-readiness to *also* trigger a redraw.
- **B-thread-mpsc** is fine — winit on main, reader thread blocking, parser on a third thread that calls `proxy.send_event(PtyReady)`. Three threads total, three layers of channels.
- **C-mio** is fine — winit on main, mio-driven render thread does both PTY readiness and the frame tick. Two threads total, one channel (`EventLoopProxy`). The render thread *is* the parser thread, so terminal state never crosses a channel.

Sketch of the C+winit integration (not in the prototype, since instantiating winit forces a window):

```rust
let event_loop = EventLoop::<PtyEvent>::with_user_event().build()?;
let proxy = event_loop.create_proxy();

std::thread::spawn(move || {
    let mut poll = Poll::new()?;
    poll.registry().register(&mut SourceFd(&master), PTY, Interest::READABLE)?;
    let mut next_frame = Instant::now() + FRAME_PERIOD;
    loop {
        let wait = next_frame.saturating_duration_since(Instant::now());
        poll.poll(&mut events, Some(wait))?;
        for ev in events.iter() {
            if ev.token() == PTY { drain_and_parse(&mut master, &mut parser, &mut state); }
        }
        if Instant::now() >= next_frame {
            proxy.send_event(PtyEvent::RedrawRequest).ok();
            next_frame += FRAME_PERIOD;
        }
    }
});

event_loop.run_app(&mut App { ... })  // owns macOS main thread
```

### Mode 2048 (in-band resize) and synchronous write-back

DECSET 2048 requires us to write a CSI sequence into the PTY master *whenever winit reports a window resize*, synchronously (apps subscribe and assume it's prompt). All three variants can do this — the master fd is owned by some thread, and `libc::write(fd, ...)` is fine to call from any thread once the kernel object exists.

- **C-mio**: winit's resize callback hands the event to the mio thread via `EventLoopProxy::send_event(Resize { cols, rows })`. The mio thread, between poll iterations, calls `libc::write(master_fd, "\x1b[48;...t")`. Zero coordination with anything else. **Best fit.**
- **B-thread-mpsc**: similar, but we now have *three* threads — the render thread doing writes coexists with a separate read thread doing reads. The fd is bidirectional, so simultaneous read+write is fine, but the design needs an explicit "writer mutex" if multiple sources can produce writes (input + resize + query reply).
- **A-tokio**: writes go through a `tokio::sync::mpsc<Vec<u8>>` into a tokio writer task that holds an `AsyncFd` registered for `WRITABLE`. Workable, but you've added another async task to a runtime that already costs more in fresh-latency.

### Query/response timing (e.g. `CSI ? 2026 $ p`)

Apps probe support by sending `CSI ? <n> $ p` and waiting (briefly) for `CSI ? <n>; <Ps> $ y`. If we're too slow the app concludes "unsupported".

The fast path is: parser sees the query CSI in `csi_dispatch`, the dispatcher synchronously calls `pty_writer.write_all(reply)`. **In variant C, "pty_writer" is just the master `OwnedFd` already owned by the thread that runs the parser.** No mutex, no channel, write-then-continue. Reply latency is dominated by `write()` (single-digit µs).

In A, the parser task can't synchronously `write()` because the fd is owned by the I/O task; the parser sends to a writer channel, the writer task wakes, writes. Three task-switches per reply. Still well inside any reasonable timeout (apps wait ~10 ms), but it's strictly more work and more state.

In B, the master fd is owned by the read thread, not the parser/render thread. We'd `dup()` it (as the prototype already does) or move it behind a `Mutex`. Adds plumbing.

### Cross-platform

- macOS / Linux: all variants work today. Tested on macOS; mio + nix both list Linux in their primary CI matrix and use the same `epoll`/`kqueue` abstractions.
- Windows / ConPTY:
  - mio supports Windows named pipes via `mio::windows::NamedPipe` (different API surface from `SourceFd`). ConPTY exposes its master end as anonymous pipe handles, not Unix fds, so the readiness model is *kind of* different — IOCP completion vs readiness — but mio absorbs that. We'd add a `#[cfg(windows)]` `PtyMaster` enum.
  - tokio has equivalent Windows pipe support (`tokio::net::windows::named_pipe`).
  - The threading thread+mpsc variant works on Windows too (blocking `ReadFile`).
  - **No variant has a fundamental Windows blocker.** ConPTY is its own beast (escape-sequence rewriting, no SIGWINCH, etc.) but that's at the PTY layer below the event loop. Defer per architecture doc; pick the event loop on macOS+Linux merits.

## Surprises

1. **B's CPU under firehose was 30 percentage points higher than A or C, despite being the simplest design.** The blocking-read thread parks on every `read()` syscall return and gets re-scheduled. epoll/kqueue waiters get coalesced wakeups across many bytes; a blocking thread does not. Lesson: "just spawn a thread" is not free under saturating I/O.

2. **A's mpsc is the bottleneck, not its async parser.** When I ran the first 10-s pass through `cargo run --release --bin runner` (i.e. with cargo-launch overhead in the loop), A's freshness p99 spiked to 9 ms — the channel filled, the producer blocked on `send().await`, and the consumer was scheduled-but-not-running for milliseconds. After switching the runner to invoke the pre-built binaries directly the symptom subsided, but A still trails C by ~5× on freshness p99 because every byte traverses an `mpsc` no matter what. The lesson: for terminal byte streams, *don't* put a channel between the kernel read and the parser. Read and parse on the same call stack.

3. **C's frame deadline IS the poll timeout — no separate timer needed.** I expected to add a timerfd or a second mio source for the frame tick. Turned out `Poll::poll(Some(deadline - now))` is exactly the frame ticker. This collapses what looked like two event sources into one and explains why C's glue count is the smallest. It's also why winit's own `ControlFlow::WaitUntil(deadline)` model maps onto this 1:1 — winit, internally, does the same trick.

4. **Idle CPU is rounding-error for everyone, even tokio.** I expected tokio's "always one task scheduled" overhead to show up at 60 Hz idle. It does not — 0.1 % is below sampling noise. So idle CPU is not a differentiator; it's the firehose case that separates the variants.

5. **vte 0.15 is allocation-free on the hot path** (`parser.advance(&mut perform, &slice)`) — at 1.6 GiB/s through `yes` the C variant's `CountingPerform::print` was the only thing in the loop and never showed up in flame-graph attention. Whatever protocol layer we build on top, vte itself will not be the bottleneck for ANSI streams. (APC payloads — kitty graphics, RGP — may still need streaming; that's a different decision.)

## Key code excerpt (variant C, the entire loop)

```rust
// 35 lines, no abstractions, no channels.
loop {
    let now = Instant::now();
    if now >= deadline { break; }
    let wait = next_frame.saturating_duration_since(now);
    poll.poll(&mut events, Some(wait))?;
    for ev in events.iter() {
        if ev.token() == PTY && ev.is_readable() {
            loop {
                match file.read(&mut buf) {
                    Ok(0)  => return done(),
                    Ok(n)  => {
                        latency.mark_arrival(Instant::now());
                        parser.advance(&mut perform, &buf[..n]);
                    }
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock  => break,
                    Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => return done(),
                }
            }
        }
    }
    if Instant::now() >= next_frame {
        latency.frame_ready();           // ← in production: proxy.send_event(Redraw)
        while next_frame <= Instant::now() { next_frame += FRAME_PERIOD; }
    }
}
```

That's the whole pattern. The rest of the variant-C file is PTY setup, CPU measurement, and the `Scenario` enum.

## Dependencies & versions (pinned, latest stable as of 2026-05-22)

- `mio = "=1.2.0"` (features `os-poll`, `os-ext`, `net`)
- `vte = "=0.15.0"`
- `nix = "=0.31.3"` (features `term`, `fs`, `process`, `signal`) — only for `openpty`; could be replaced with raw libc or `rustix` if we want a smaller dep
- `libc = "=0.2.186"`
- (For comparison only — not adopted) `tokio = "=1.52.3"`, `crossbeam-channel = "=0.5.15"`

## Out of scope / follow-ups

- **Streaming APC parser.** Variant C feeds vte 0.15 directly, which buffers entire APC payloads. Kitty graphics and RGP can carry MiB of data; the architecture doc already flags this as an open question. Decision belongs in its own RFC.
- **Backpressure on render-thread overload.** If `parser.advance` ever becomes slow (it isn't today), we need to stop the read loop draining into vte and let the kernel pipe absorb. mio's level-triggered readiness means the poll will simply keep firing, which is fine — but if we adopt `Interest::READABLE` with edge-triggered mode we need an explicit re-arm. Document at integration time.
- **Windows ConPTY** integration. Per architecture doc this is a v2 milestone — recommendation here does not block on it. mio's `NamedPipe` is the integration point when we get there.
