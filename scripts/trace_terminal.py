#!/usr/bin/env python3
"""Wrap a command in a PTY and log every byte it writes to its stdout.

Usage:
    scripts/trace_terminal.py [--log FILE] -- COMMAND [ARGS...]

Default log file: /tmp/toastty-trace.bin.

After the command exits, prints a summary of the escape sequences the
command emitted (APC, DCS, OSC, CSI queries). CSI cursor motions and
SGR are suppressed because they dominate the noise; everything else is
shown so you can see what queries a TUI like yazi sends to detect
terminal features.

Example:
    scripts/trace_terminal.py -- yazi
"""
import os
import pty
import sys


def fmt(b):
    return "".join(chr(c) if 32 <= c < 127 else f"\\x{c:02x}" for c in b)


def main():
    args = sys.argv[1:]
    log_path = "/tmp/toastty-trace.bin"
    if args and args[0] == "--log":
        if len(args) < 2:
            print(__doc__, file=sys.stderr)
            sys.exit(1)
        log_path = args[1]
        args = args[2:]
    if not args or args[0] != "--":
        print(__doc__, file=sys.stderr)
        sys.exit(1)
    cmd = args[1:]
    if not cmd:
        print("missing command", file=sys.stderr)
        sys.exit(1)

    logf = open(log_path, "wb")

    def master_read(fd):
        data = os.read(fd, 4096)
        logf.write(data)
        logf.flush()
        return data

    try:
        pty.spawn(cmd, master_read=master_read)
    finally:
        logf.close()

    with open(log_path, "rb") as f:
        data = f.read()

    print(
        f"\n--- captured {len(data)} bytes from child to {log_path}",
        file=sys.stderr,
    )
    print(
        "--- escape sequences emitted (CSI cursor/SGR suppressed):",
        file=sys.stderr,
    )

    counts = {}
    i, n = 0, len(data)
    while i < n:
        b = data[i]
        if b == 0x1B and i + 1 < n:
            nxt = data[i + 1]
            if nxt == 0x5F:  # APC ESC _
                end = data.find(b"\x1b\\", i + 2)
                if end == -1:
                    end = n
                payload = data[i + 2 : end]
                sep = payload.find(b";")
                head = payload[:sep] if sep >= 0 else payload
                blen = len(payload) - (sep + 1) if sep >= 0 else 0
                if blen <= 16:
                    body = payload[sep + 1 :] if sep >= 0 else b""
                    print(f"  APC  \\x1b_{fmt(head)};{fmt(body)}\\x1b\\\\")
                else:
                    print(
                        f"  APC  \\x1b_{fmt(head)};<{blen}B base64 body>\\x1b\\\\"
                    )
                counts["APC"] = counts.get("APC", 0) + 1
                i = end + 2
                continue
            if nxt == 0x50:  # DCS ESC P
                end = data.find(b"\x1b\\", i + 2)
                if end == -1:
                    end = n
                inner = data[i + 2 : end]
                clip = inner[:60]
                tail = "..." if len(inner) > 60 else ""
                print(f"  DCS  \\x1bP{fmt(clip)}{tail}\\x1b\\\\")
                counts["DCS"] = counts.get("DCS", 0) + 1
                i = end + 2
                continue
            if nxt == 0x5D:  # OSC ESC ]
                j = i + 2
                term_len = 0
                while j < n:
                    if data[j] == 0x07:
                        term_len = 1
                        break
                    if (
                        data[j] == 0x1B
                        and j + 1 < n
                        and data[j + 1] == 0x5C
                    ):
                        term_len = 2
                        break
                    j += 1
                payload = data[i + 2 : j]
                term = "\\x07" if term_len == 1 else "\\x1b\\\\"
                print(f"  OSC  \\x1b]{fmt(payload)}{term}")
                counts["OSC"] = counts.get("OSC", 0) + 1
                i = j + term_len
                continue
            if nxt == 0x5B:  # CSI ESC [
                j = i + 2
                while j < n and not (0x40 <= data[j] <= 0x7E):
                    j += 1
                if j >= n:
                    i = n
                    continue
                params = data[i + 2 : j]
                final = chr(data[j])
                # Suppress cursor moves & SGR — they swamp the output.
                # Show device queries / status / window-mgr / decrqm.
                interesting = final in "cnpt" or b"$" in params
                if interesting:
                    print(f"  CSI  \\x1b[{fmt(params)}{final}")
                    counts["CSI(query)"] = counts.get("CSI(query)", 0) + 1
                else:
                    counts["CSI(other)"] = counts.get("CSI(other)", 0) + 1
                i = j + 1
                continue
            # ESC + single byte (e.g. ESC =, ESC >)
            print(f"  ESC  \\x1b{fmt(bytes([nxt]))}")
            counts["ESC"] = counts.get("ESC", 0) + 1
            i += 2
            continue
        i += 1

    print("\n--- summary:", file=sys.stderr)
    for k in sorted(counts):
        print(f"  {k}: {counts[k]}", file=sys.stderr)


if __name__ == "__main__":
    main()
