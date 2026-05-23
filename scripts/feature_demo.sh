#!/usr/bin/env bash
# Exercise the modern-input features added in M7: bracketed paste,
# focus events, mouse reporting (SGR 1006 + 1002), and the kitty
# keyboard progressive enhancement.
#
# Usage:
#   cargo run --release
#   # then inside the toastty window:
#   ./scripts/feature_demo.sh
#
# Each section enables a protocol and then prompts you to interact.
# We restore default state on exit via a trap.

set -u

esc=$'\033'
csi="${esc}["

section() {
    printf '\n%s1;7m %s %s0m\n' "$csi" "$1" "$csi"
}
note() {
    printf '%s2;3m   %s%s0m\n' "$csi" "$1" "$csi"
}

cleanup() {
    # Disable everything we enabled.
    printf '%s?2004l' "$csi"   # bracketed paste off
    printf '%s?1004l' "$csi"   # focus events off
    printf '%s?1002l' "$csi"   # button-motion mouse off
    printf '%s?1000l' "$csi"   # x10 mouse off
    printf '%s?1006l' "$csi"   # sgr encoding off
    printf '%s<u' "$csi"        # pop kitty keyboard flags
    printf '\n[demo done — modes restored]\n'
}
trap cleanup EXIT

# ─────────────────────────────────────────────────────────────────
section "M7.1 — Bracketed paste (DECSET 2004)"
note "Enabling bracketed paste. Press Cmd+V (macOS) or Ctrl+Shift+V."
note "Your pasted text will be wrapped in ESC[200~ ... ESC[201~"
note "(visible as the surrounding control sequences in the readout below)."
printf '%s?2004h' "$csi"
printf 'paste here, then press Enter > '
read -r pasted
printf '%s?2004l' "$csi"
printf 'received: %q\n' "$pasted"

# ─────────────────────────────────────────────────────────────────
section "M7.2 — Focus events (DECSET 1004)"
note "Enabling focus reporting. Click outside toastty, then back in."
note "You should see ESC[O on blur and ESC[I on focus."
note "Press Enter when done."
printf '%s?1004h' "$csi"
read -r _
printf '%s?1004l' "$csi"

# ─────────────────────────────────────────────────────────────────
section "M7.3 — Mouse reporting (SGR 1006 + 1002)"
note "Enabling click + drag tracking with SGR encoding."
note "Click and drag the mouse inside the window."
note "You'll see ESC[<0;C;R M (press) / m (release) sequences."
note "Press Enter when done."
printf '%s?1002h%s?1006h' "$csi" "$csi"
read -r _
printf '%s?1002l%s?1006l' "$csi" "$csi"

# ─────────────────────────────────────────────────────────────────
section "M7.4 — Kitty keyboard protocol (CSI u, disambiguate + events)"
note "Pushing flags = 3 (disambiguate + report event types)."
note "Press a, A, Ctrl+A, Ctrl+Shift+A. They should now emit"
note "distinct CSI u sequences (e.g. CSI 97;6:1 u for Ctrl+Shift+A)."
note "Press Enter when done."
printf '%s>3u' "$csi"
read -r _
printf '%s<u' "$csi"

note "Done. See $0 source for the raw escape sequences used."
