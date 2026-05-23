#!/usr/bin/env bash
# Exercise everything toastty currently supports. Run inside toastty
# to evaluate output visually.
#
# Usage:
#   cargo run --release          # launches toastty
#   # then in the shell that opens:
#   ./scripts/feature_demo.sh

set -u

esc=$'\033'
csi="${esc}["
st="${esc}\\"

cleanup() {
    # Restore any modes the M7/M8 sections leave dangling if you Ctrl-C
    # out partway through.
    printf '%s?2004l' "$csi"
    printf '%s?1004l' "$csi"
    printf '%s?1002l' "$csi"
    printf '%s?1000l' "$csi"
    printf '%s?1006l' "$csi"
    printf '%s<u' "$csi"
    printf '%s?2026l' "$csi"
    printf '%s?2027l' "$csi"
    printf '%s?2048l' "$csi"
}
trap cleanup EXIT

section() {
    printf '\n%s1;7m %s %s0m\n' "$csi" "$1" "$csi"
}
note() {
    printf '%s2;3m   %s%s0m\n' "$csi" "$1" "$csi"
}

# ────────────────────────────────────────────────────────────────
section "1. Plain text + UTF-8"
echo "ASCII:  The quick brown fox jumps over the lazy dog"
echo "Latin:  jalapeño, naïve, café, façade"
echo "Greek:  αβγδεζη θικλμνξ οπρστυ φχψω"
echo "CJK:    你好世界 (single-codepoint wide chars — 2 cells each since M8)"
note "VS16 emoji (❤️) and ZWJ clusters need DECSET 2027 — see M8.2 below"

# ────────────────────────────────────────────────────────────────
section "2. C0 control characters"
# Carriage return: cursor to col 0, no erase. "BBB" should overwrite
# the first 3 of "aaaaa", leaving "aa" visible after.
printf 'CR:        '
printf 'aaaaa\rBBB\n'
printf '           (expect: "BBBaa" — CR moves cursor, doesn'\''t erase)\n\n'

# Backspace: cursor back one cell, no erase. 2 BS from col 5 ("hello")
# → col 3 ("hel"), then "WORLD" overwrites cols 3-7.
printf 'BS:        '
printf 'hello\b\bWORLD\n'
printf '           (expect: "helWORLD" — BS moves cursor, doesn'\''t erase)\n\n'

# Tab: every 8 cols.
printf 'TAB:       '
printf 'a\tb\tc\td\te\n'
printf '           (expect: a, b, c, d, e separated by tab stops)\n\n'

# BEL: silent (we don'\''t bell yet; just verify it doesn'\''t mangle output).
printf 'BEL:       silent (no audible bell, output unaffected): '; printf '\a'; echo '✓'

# ────────────────────────────────────────────────────────────────
section "3. 16-color foreground (SGR 30–37, 90–97)"
printf '  normal: '
for i in 30 31 32 33 34 35 36 37; do printf "%s%dm  %d " "$csi" "$i" "$i"; done
printf '%s0m\n' "$csi"
printf '  bright: '
for i in 90 91 92 93 94 95 96 97; do printf "%s%dm  %d " "$csi" "$i" "$i"; done
printf '%s0m\n' "$csi"

# ────────────────────────────────────────────────────────────────
section "4. 16-color background (SGR 40–47, 100–107)"
printf '  normal: '
for i in 40 41 42 43 44 45 46 47; do printf "%s%dm  %d %s0m " "$csi" "$i" "$i" "$csi"; done
echo
printf '  bright: '
for i in 100 101 102 103 104 105 106 107; do printf "%s%dm  %d %s0m " "$csi" "$i" "$i" "$csi"; done
echo

# ────────────────────────────────────────────────────────────────
section "5. SGR attributes"
printf '  %s1mbold%s0m  '          "$csi" "$csi"
printf '%s3mitalic%s0m  '          "$csi" "$csi"
printf '%s4munderline%s0m  '       "$csi" "$csi"
printf '%s7mreverse%s0m\n'         "$csi" "$csi"
note "all four attrs supported; faint/blink/strikethrough are NOT (M6)"

# ────────────────────────────────────────────────────────────────
section "6. Combined attributes"
printf '  %s1;31mbold red%s0m\n'                "$csi" "$csi"
printf '  %s1;3;33mbold italic yellow%s0m\n'    "$csi" "$csi"
printf '  %s4;36munderline cyan%s0m\n'          "$csi" "$csi"
printf '  %s7;1mbold reverse%s0m\n'             "$csi" "$csi"

# ────────────────────────────────────────────────────────────────
section "7. Reset behaviors (must set BOTH fg and bg first)"
# Full reset (SGR 0): clears fg AND bg AND all attrs.
printf '  %s1;33;41mbold yellow on red%s0m  after SGR 0 (full reset)\n' "$csi" "$csi"

# SGR 39: default-fg only — bg should stay red.
printf '  %s33;41myellow on red%s39m  after SGR 39 (default-fg, bg still red)%s0m\n' "$csi" "$csi" "$csi"

# SGR 49: default-bg only — fg should stay yellow.
printf '  %s33;41myellow on red%s49m  after SGR 49 (default-bg, fg still yellow)%s0m\n' "$csi" "$csi" "$csi"

# ────────────────────────────────────────────────────────────────
section "8. Cursor moves (CUP/CUF/CUB/CUU/CUD)"
note "  pin a marker, move around it, then continue"
printf 'baseline------------------\n'
printf 'X'                # writes X at col 0
printf '%s10C'  "$csi"    # CUF 10 → col 10
printf 'Y'                # writes Y at col 10
printf '%s10D'  "$csi"    # CUB 10 → back to col 1 (after X)
printf 'Z\n'              # writes Z at col 1
printf '(expect: X Z      Y on the previous line)\n'

# Absolute position via CSI H
printf 'home-marker '
saved_row=$(printf '%s6n' "$csi")  # cursor position query (not handled, harmless)
printf '\n\n\n'
printf '%s3A' "$csi"   # CUU 3
printf '%s30C' "$csi"  # CUF 30
printf '<--cursor up 3, forward 30%s\n\n\n' "$csi"

# ────────────────────────────────────────────────────────────────
section "9. Erase line (EL 0/1/2)"
# Each demo: print 20 digits as a ruler, position cursor at col 10,
# then EL with the variant, then \n. The description follows on the
# *next* line so EL behaviour is visible without being overwritten.

# EL 0: erase from cursor to EOL.
printf '12345678901234567890\r'   # cursor at col 0, 20 digits visible
printf '%s10C' "$csi"              # cursor to col 10
printf '%s0K\n' "$csi"             # erase cols 10..19
printf '           ^ EL 0: cols 10..EOL cleared (expect: "1234567890" then blanks)\n'
echo

# EL 1: erase from BOL through cursor (inclusive).
printf '12345678901234567890\r'
printf '%s10C' "$csi"              # cursor to col 10
printf '%s1K\n' "$csi"             # erase cols 0..10 (inclusive)
printf '           ^ EL 1: cols 0..cursor cleared (expect: 11 blanks, then "2345678901" — note cursor cell IS cleared)\n'
echo

# EL 2: erase entire line.
printf '12345678901234567890\r'
printf '%s10C' "$csi"              # cursor to col 10
printf '%s2K\n' "$csi"             # erase whole line
printf '           ^ EL 2: whole line cleared (expect: empty line)\n'

# ────────────────────────────────────────────────────────────────
section "10. Alt screen (DECSET 1049)"
note "will switch to a blank alt screen, hold 2s, then restore"
sleep 1
printf '%s?1049h' "$csi"           # enter alt
printf '%s2J%s1;1H' "$csi" "$csi"  # clear, home
printf '*** ALT SCREEN ***\n\n'
printf 'If you see this for ~2s and then your terminal restores\n'
printf 'the *previous* content (this script up to the last note),\n'
printf 'alt-screen save/restore is working.\n'
sleep 2
printf '%s?1049l' "$csi"           # leave alt
echo "(back to primary screen — previous output should still be here)"

# ────────────────────────────────────────────────────────────────
section "11. 256-color + truecolor (M6)"
note "256-color (CSI 38;5;N)"
printf '  256-color: '
for n in 1 9 17 25 196 220 51; do
    printf '%s38;5;%dmcolor-%d%s0m  ' "$csi" "$n" "$n" "$csi"
done
echo

note "truecolor (CSI 38;2;R;G;B)"
printf '  truecolor: '
for spec in "255;0;0" "0;255;0" "0;0;255" "200;100;50" "180;32;100"; do
    printf '%s38;2;%smcolor%s0m  ' "$csi" "$spec" "$csi"
done
echo
note "the last one (180;32;100) is the canonical leak case — the 32"
note "must be the B value, NOT 'fg green'"

# ────────────────────────────────────────────────────────────────
section "12. Window title (OSC 0/2) — M6"
printf '%s]0;hello from toastty demo%s' "$esc" "$st"
note "  ^ window title should now read 'hello from toastty demo'"
sleep 1
printf '%s]2;toastty M6 demo%s' "$esc" "$st"
note "  ^ now retitled to 'toastty M6 demo' via OSC 2 (window-only)"
sleep 1

# ────────────────────────────────────────────────────────────────
section "13. Cursor shape (DECSCUSR) — M6"
note "switching cursor through all 6 DECSCUSR shapes (Ps=1..6)"
note "Ps=1 → block blinking, Ps=2 → block steady"
note "Ps=3 → underline blinking, Ps=4 → underline steady"
note "Ps=5 → bar blinking, Ps=6 → bar steady"
note "blink doesn't animate yet (M9); shape change is visible"
for ps in 1 2 3 4 5 6; do
    printf '%s%d q' "$csi" "$ps"
    printf '   Ps=%d (watch the cursor)\n' "$ps"
    sleep 0.6
done
# Restore default block.
printf '%s0 q' "$csi"
note "  cursor restored to default (block blinking, Ps=0)"

# ────────────────────────────────────────────────────────────────
section "14. Not supported yet"
note "[NOT SUPPORTED] OSC 8 hyperlinks (M10)"
printf '%s]8;;https://example.com%sclick me%s]8;;%s\n' "$esc" "$st" "$esc" "$st"
note "  ^ should be underlined + clickable in M10"

# ────────────────────────────────────────────────────────────────
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
note ""
note "NOTE: with disambiguate on, Enter is ALSO reframed as CSI u"
note "(13;1:1u press / 13;1:3u release), so 'read' can't see a newline."
note "Auto-advancing in 8 seconds — type during the window."
printf '%s>3u' "$csi"
sleep 8
printf '%s<u' "$csi"
note "Flags popped — Enter is back to \\r."

# ─────────────────────────────────────────────────────────────────
section "M8.1 — Synchronized output (DECSET 2026)"
note "BSU/ESU brackets keep the renderer from showing partial frames."
note "Sending BSU, writing 5 lines with 200ms delays, then ESU."
note "Lines should appear *atomically* at ESU, not one-by-one."
sleep 1
printf '%s?2026h' "$csi"          # BSU
for i in 1 2 3 4 5; do
    printf '  line %d (you should NOT see this until ESU)\n' "$i"
    sleep 0.2
done
sleep 0.5
printf '%s?2026l' "$csi"          # ESU
note "If you saw 5 lines appear at once after ~1.5s, mode 2026 is working."
sleep 1

note ""
note "Watchdog: 1s after BSU with no ESU, the renderer force-flushes"
note "and the next frame is a corrective full redraw."
sleep 1
printf '%s?2026h' "$csi"          # BSU only, no ESU
printf '  watchdog target: written under BSU, no ESU follows.\n'
sleep 1.5
printf '  visible? then the watchdog kicked in.\n'
printf '%s?2026l' "$csi"          # cleanup
sleep 1

# ─────────────────────────────────────────────────────────────────
section "M8.2 — Grapheme cluster widths (DECSET 2027)"
note "Single-codepoint wide chars (CJK, base emoji) already render at"
note "width 2 — Term::print uses unicode-width regardless of mode 2027."
echo '  你好世界    ← 4 CJK ideographs, expect 8 cells wide'
echo '  |-------|   ← 8-col ruler'
note ""
note "Enabling DECSET 2027 — apps signal they want cluster-width honoring"
note "for VS16 / ZWJ clusters in the renderer's cluster snap."
printf '%s?2027h' "$csi"
echo '  你好世界    ← mode 2027 active'
echo '  |-------|'
printf '%s?2027l' "$csi"
note ""
note "Known limitation (M9): Term::print is still per-codepoint, so"
note "VS16-presented ❤️ and ZWJ family clusters disagree between the"
note "cell grid (1 cell) and renderer geometry (2 cells). Cluster-aware"
note "print() lands in M9."
sleep 1

# ─────────────────────────────────────────────────────────────────
section "M8.3 — In-band resize notifications (DECSET 2048)"
note "Enabling mode 2048. Resize the toastty window — the kernel sends"
note "SIGWINCH, and toastty emits a CSI 48 ; rows ; cols ; ph ; pw t"
note "report on the PTY's read side, in order with stdout (no race)."
note ""
note "Your shell may show garbage characters as the sequence arrives;"
note "an app using mode 2048 parses them as a structured resize event."
note "Press Enter when you're done resizing."
printf '%s?2048h' "$csi"
read -r _
printf '%s?2048l' "$csi"

# ─────────────────────────────────────────────────────────────────
section "M10.1 — OSC 7 (current working directory)"
note "Advertise the cwd. toastty stores it on Term::cwd(); future UI"
note "can surface it. Try with a path containing a space."
printf '%s]7;file://localhost%s%s' "$esc" "$PWD" "$st"
note "Emitted: ESC ] 7 ; file://localhost${PWD} ESC \\\\"
sleep 1

# ─────────────────────────────────────────────────────────────────
section "M10.2 — OSC 133 (semantic prompt markers)"
note "Mark prompt start / end / command start / command finished with"
note "exit code. toastty records these for command-level navigation."
printf '%s]133;A%s' "$esc" "$st"  # prompt start
printf '%s]133;B%s' "$esc" "$st"  # prompt end
echo "(simulated: A=prompt_start, B=prompt_end emitted around this line)"
printf '%s]133;C%s' "$esc" "$st"  # command start
printf '%s]133;D;0%s' "$esc" "$st" # command finished
note "Emitted: A,B,C,D markers; check t.prompt_marks() in tests."
sleep 1

# ─────────────────────────────────────────────────────────────────
section "M10.3 — OSC 4 (palette query + set)"
note "Override palette index 1 (red) to bright magenta, then echo a red"
note "char — it should show up magenta. Reset with default afterwards."
printf '%s]4;1;rgb:ff/00/ff%s' "$esc" "$st"
printf '%s[31mred-but-magenta-now%s[0m\n' "$csi" "$csi"
sleep 1
note "Querying index 1 — toastty replies on the PTY with the new value:"
printf '%s]4;1;?%s' "$esc" "$st"
note "(the reply is consumed by your shell; look for an ESC ] 4 ; 1 ;"
note " rgb:ffff/0000/ffff ESC \\\\ sequence)"
sleep 1
# Restore the default red so the rest of the demo isn't tinted.
printf '%s]4;1;rgb:80/00/00%s' "$esc" "$st"

# ─────────────────────────────────────────────────────────────────
section "M10.4 — OSC 8 (hyperlinks)"
note "Wrap text in an OSC 8 hyperlink. toastty stamps the cells with a"
note "hyperlink id, renders an underline strip, and Cmd-click (macOS) /"
note "Ctrl-click (Linux) opens the URL via the OS browser."
printf '  Visit '
printf '%s]8;;https://example.com%sexample.com%s]8;;%s' "$esc" "$st" "$esc" "$st"
echo ' (Cmd/Ctrl-click)'
sleep 1

# ─────────────────────────────────────────────────────────────────
section "M10.5 — OSC 52 (clipboard, gated by [security])"
note "OSC 52 set + query. Both gates default OFF — to exercise this,"
note "enable osc_52_read/osc_52_write in your toastty config first."
note "Emitting OSC 52 ; c ; <base64-of-hello>:"
printf '%s]52;c;aGVsbG8=%s' "$esc" "$st"
note "If write is enabled, your clipboard now contains \"hello\"."
note "Emitting OSC 52 ; c ; ? (read request):"
printf '%s]52;c;?%s' "$esc" "$st"
note "If read is enabled, your shell will see an OSC 52 reply byte"
note "stream — the reply contains the base64-encoded clipboard."
sleep 1

# ─────────────────────────────────────────────────────────────────
section "Done"
note "If colors leak between sections (esp. into this final line),"
note "the SGR multi-param parsing fix is incomplete."
