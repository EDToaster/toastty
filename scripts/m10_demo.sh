#!/usr/bin/env bash
# Demo just the M10 (shell integration) features in isolation. Run inside
# toastty to evaluate visually.
#
# Usage:
#   cargo run --release          # launches toastty
#   # then in the shell that opens:
#   ./scripts/m10_demo.sh
#
# M10 introduces five OSC handlers:
#   OSC 7    current working directory
#   OSC 133  semantic prompt markers (A/B/C/D)
#   OSC 4    palette query + per-index runtime override
#   OSC 8    hyperlinks (Cmd-Left / Ctrl-Left to open)
#   OSC 52   clipboard read/write (gated by [security] config)

set -u

esc=$'\033'
csi="${esc}["
st="${esc}\\"

# OSC 4 ? and OSC 52 ? both make toastty write a reply back on the PTY.
# Two cleanup duties:
#   1) TTY ECHO is on by default. Replies land in the PTY slave's input
#      buffer; cooked-mode echoes each byte back to the master, which
#      toastty renders as text mid-demo. Disable echo for the duration.
#   2) Bytes still pile up in stdin. Drain on exit so they don't show
#      up as "typed" input at the next prompt.
old_stty=$(stty -g 2>/dev/null || true)
stty -echo 2>/dev/null || true

drain_stdin() {
    while IFS= read -r -t 0.05 -n 1024 -s _drain 2>/dev/null; do :; done
}
restore_terminal() {
    if [ -n "${old_stty:-}" ]; then
        stty "$old_stty" 2>/dev/null || true
    fi
    drain_stdin
}
trap restore_terminal EXIT

section() {
    printf '\n%s1;7m %s %s0m\n' "$csi" "$1" "$csi"
}
note() {
    printf '%s2;3m   %s%s0m\n' "$csi" "$1" "$csi"
}

# ─────────────────────────────────────────────────────────────────
section "M10.1 — OSC 7 (current working directory)"
note "Apps emit OSC 7 ; file://host/path after every cd. toastty stores"
note "the path on Term::cwd(); future UI (new-tab inheritance, status"
note "bar) reads it from there."
note "Emitting cwd = $PWD"
printf '%s]7;file://localhost%s%s' "$esc" "$PWD" "$st"
note "  ESC ] 7 ; file://localhost${PWD} ESC \\\\"
sleep 1

# ─────────────────────────────────────────────────────────────────
section "M10.2 — OSC 133 (semantic prompt markers)"
note "Apps mark prompt boundaries so future keybinds can jump between"
note "prompts and tab strips can show command exit status."
note "  A = prompt start    B = prompt end"
note "  C = command start   D[;exit] = command finished"
printf '%s]133;A%s' "$esc" "$st"
printf '%s]133;B%s' "$esc" "$st"
echo "(emitted A and B around this line — prompt-bounded)"
printf '%s]133;C%s' "$esc" "$st"
printf '%s]133;D;0%s' "$esc" "$st"
note "(emitted C then D;0 — command with exit code 0)"
note "Marks are stored in Term::prompt_marks() — VecDeque, capped 4096."
sleep 1

# ─────────────────────────────────────────────────────────────────
section "M10.3 — OSC 4 (palette query + set)"
note "Override palette index 1 (red) to bright magenta, then print a red"
note "char using SGR 31 — it should render magenta."
printf '%s]4;1;rgb:ff/00/ff%s' "$esc" "$st"
printf '   %s[31mred-but-magenta-now%s[0m\n' "$csi" "$csi"
sleep 1
note "Querying index 1 — toastty replies on the PTY:"
printf '%s]4;1;?%s' "$esc" "$st"
note "  reply format: ESC ] 4 ; 1 ; rgb:RRRR/GGGG/BBBB ESC \\\\"
note "  (4-digit hex per channel; the bytes go to your shell's stdin)"
sleep 1
# Restore the default red so the rest of the demo isn't tinted.
printf '%s]4;1;rgb:80/00/00%s' "$esc" "$st"
note "(restored index 1 to default-ish red)"
printf '   %s[31mback to red%s[0m\n' "$csi" "$csi"
sleep 1

# ─────────────────────────────────────────────────────────────────
section "M10.4 — OSC 8 (hyperlinks)"
note "Wrap text in an OSC 8 hyperlink. toastty stamps the enclosed cells"
note "with a NonZeroU16 hyperlink id, the renderer draws an underline"
note "strip, and modified click opens the URL via the OS browser."
note "  Cmd-Left on macOS, Ctrl-Left elsewhere."
printf '   Visit '
printf '%s]8;;https://example.com%sexample.com%s]8;;%s' "$esc" "$st" "$esc" "$st"
echo ' (try clicking it)'
sleep 1
note "Multiple links on one line:"
printf '   '
printf '%s]8;;https://github.com%sGitHub%s]8;;%s' "$esc" "$st" "$esc" "$st"
printf ' / '
printf '%s]8;;https://anthropic.com%sAnthropic%s]8;;%s' "$esc" "$st" "$esc" "$st"
printf ' / '
printf '%s]8;;https://rust-lang.org%sRust%s]8;;%s' "$esc" "$st" "$esc" "$st"
echo
sleep 1

# ─────────────────────────────────────────────────────────────────
section "M10.5 — OSC 52 (clipboard, gated by [security])"
note "OSC 52 set + query. Both gates default OFF:"
note "  [security]"
note "  osc_52_read  = false"
note "  osc_52_write = false"
note "Enable in toastty's config to exercise these."
note ""
note "Emitting OSC 52 ; c ; <base64 of \"hello\">:"
printf '%s]52;c;aGVsbG8=%s' "$esc" "$st"
note "  if osc_52_write = true, your clipboard now contains \"hello\""
sleep 1
note "Emitting OSC 52 ; c ; ?  (read request):"
printf '%s]52;c;?%s' "$esc" "$st"
note "  if osc_52_read = true, your shell sees an OSC 52 reply byte"
note "  stream with the base64-encoded clipboard contents"
sleep 1

# ─────────────────────────────────────────────────────────────────
section "Done — shell integration"
note "Shell snippets that emit these OSCs from your prompt hooks live in"
note "share/shell-integration/{bash.sh, zsh.sh, fish.fish}."
note "Source the matching file from your shell rc to get OSC 7 + OSC 133"
note "emitted automatically around every prompt and cd."
