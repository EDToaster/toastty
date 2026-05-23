#!/usr/bin/env bash
# Exercise everything toastty currently supports (post-M5 + perf fixes
# + redraw fix). Run inside toastty to evaluate output visually.
#
# Usage:
#   cargo run --release          # launches toastty
#   # then in the shell that opens:
#   ./scripts/feature_demo.sh
#
# Sections that are *expected* to misbehave (the bugs we know about)
# are flagged with [BROKEN] so you can confirm what's currently wrong.

set -u

esc=$'\033'
csi="${esc}["
st="${esc}\\"

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
echo "CJK:    你好世界 (wide chars — likely misaligned for now)"
note "[partial] wide chars (CJK) should occupy 2 cells but currently take 1"

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
section "11. Known broken (the bugs we know about)"
note "[BROKEN] 256-color (CSI 38;5;N) — SGR multi-param parsing leak"
printf '  256-color: '
for n in 1 9 17 25 196 220 51; do
    printf '%s38;5;%dmcolor-%d%s0m  ' "$csi" "$n" "$n" "$csi"
done
echo

note "[BROKEN] truecolor (CSI 38;2;R;G;B) — same bug as above"
printf '  truecolor: '
for spec in "255;0;0" "0;255;0" "0;0;255" "200;100;50" "180;32;100"; do
    printf '%s38;2;%smcolor%s0m  ' "$csi" "$spec" "$csi"
done
echo
note "the last one (180;32;100) is the canonical leak case — the 32"
note "should be interpreted as a B value, not as 'fg green'"

note "[NOT SUPPORTED] OSC 0/1/2 window title (M6)"
printf '%s]0;hello from toastty demo%s' "$esc" "$st"
note "  ^ if window title changed to 'hello from toastty demo', M6 is done"

note "[NOT SUPPORTED] DECSCUSR cursor shape (M6)"
printf '%s5 q' "$csi"    # blinking bar
note "  cursor should be a blinking bar above; M6 wires it"
printf '%s0 q' "$csi"    # reset cursor shape

note "[NOT SUPPORTED] OSC 8 hyperlinks (M10)"
printf '%s]8;;https://example.com%sclick me%s]8;;%s\n' "$esc" "$st" "$esc" "$st"
note "  ^ should be underlined + clickable in M10"

# ────────────────────────────────────────────────────────────────
section "Done"
note "If colors leak between sections (esp. into this final line), report"
note "the SGR multi-param parsing fix is incomplete."
