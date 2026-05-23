#!/usr/bin/env bash
# M11 demo — Kitty graphics (M11a, shipped) and Sixel (M11b, not yet
# implemented). Run inside toastty to evaluate visually.
#
# Usage:
#   cargo run --release          # launches toastty
#   # then in the shell that opens:
#   ./scripts/m11_demo.sh
#
# The script reads scripts/image.jpg, converts to PNG once via sips /
# magick / convert (cached as scripts/image.png), then emits the image
# through the Kitty graphics protocol APC envelope:
#   ESC _ G <header> ; <base64-payload> ESC \
#
# Each demo section pauses briefly so you can observe the result.

set -u

esc=$'\033'
csi="${esc}["
st="${esc}\\"
apc="${esc}_"

# Kitty graphics + the M11a query path generate replies on the PTY.
# Two cleanup duties:
#   1) TTY echo is on by default. Replies written by toastty land in
#      the PTY slave's input buffer; cooked-mode + ECHO means each byte
#      is echoed back to the master, which toastty then renders. You'd
#      see `^[_Gi=1;OK^[\` scattered through the demo output. Disable
#      ECHO for the duration of the script.
#   2) Bytes still pile up in stdin even with ECHO off. Drain on exit
#      so they don't show as "typed" input at the next prompt.
old_stty=$(stty -g 2>/dev/null || true)
stty -echo 2>/dev/null || true

drain_stdin() {
    while IFS= read -r -t 0.05 -n 4096 -s _drain 2>/dev/null; do :; done
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
pause() {
    sleep "${1:-1.5}"
}

# ─────────────────────────────────────────────────────────────────
# Setup: ensure scripts/image.png exists (base64-encode it once).

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
JPG="$SCRIPT_DIR/image.jpg"
PNG="$SCRIPT_DIR/image.png"

if [ ! -f "$JPG" ]; then
    echo "Missing demo image: $JPG" >&2
    exit 1
fi

if [ ! -f "$PNG" ] || [ "$JPG" -nt "$PNG" ]; then
    if command -v sips >/dev/null 2>&1; then
        sips -s format png "$JPG" --out "$PNG" >/dev/null 2>&1
    elif command -v magick >/dev/null 2>&1; then
        magick "$JPG" "$PNG"
    elif command -v convert >/dev/null 2>&1; then
        convert "$JPG" "$PNG"
    else
        echo "Need sips, magick, or convert to convert JPG -> PNG" >&2
        exit 1
    fi
fi

# Single-line base64 (no wraps) — kitty tolerates wraps but cleaner without.
B64=$(base64 < "$PNG" | tr -d '\n')
TOTAL=${#B64}

note "Loaded $PNG ($(wc -c < "$PNG") bytes; base64=${TOTAL} chars)"
pause 1

# ─────────────────────────────────────────────────────────────────
section "M11a.1 — Basic transmit + place (a=T)"
note "Single-APC transmit-and-display. f=100 = PNG, c=15,r=10 = scale"
note "to 15 cells wide x 10 cells tall. Cursor lands below the image."
pause 1
printf '%sGa=T,f=100,i=1,c=15,r=10;%s%s' "$apc" "$B64" "$st"
echo "<- cursor lands here after the placement"
pause 2

# ─────────────────────────────────────────────────────────────────
section "M11a.2 — Transmit once, place twice"
note "a=t uploads to the registry but doesn't display. a=p places an"
note "already-uploaded image. Image id 2 placed at two cursor positions."
pause 1
# Upload only (a=t, q=2 silences the OK reply).
printf '%sGa=t,f=100,i=2,q=2;%s%s' "$apc" "$B64" "$st"
# Place 1
note "Placement 1:"
printf '%sGa=p,i=2,c=10,r=6;%s' "$apc" "$st"
# Place 2 with a different size
note "Placement 2 (same image, smaller):"
printf '%sGa=p,i=2,c=6,r=4;%s' "$apc" "$st"
echo
pause 2

# ─────────────────────────────────────────────────────────────────
section "M11a.3 — Sub-rect placement (x/y/w/h within source)"
note "Show only the top-left quadrant of the image. x=0,y=0 start;"
note "w=98,h=130 are pixels within the 195x259 source."
pause 1
printf '%sGa=T,f=100,i=3,x=0,y=0,w=98,h=130,c=10,r=8;%s%s' "$apc" "$B64" "$st"
echo
pause 2

note "And the bottom-right quadrant for contrast:"
printf '%sGa=T,f=100,i=4,x=97,y=129,w=98,h=130,c=10,r=8;%s%s' "$apc" "$B64" "$st"
echo
pause 2

# ─────────────────────────────────────────────────────────────────
section "M11a.4 — Z-index (text behind / over image)"
note "Place image at z=-1 (behind text). Then move cursor back up into"
note "the image area and print labelled text. Text should render over"
note "the image because text z >= 0 > image z = -1."
pause 1
printf '%sGa=T,f=100,i=5,c=20,r=8,z=-1;%s%s' "$apc" "$B64" "$st"
# After placement, cursor sits one row below the image. Move up into
# the middle of the image, right a few columns, and write a label.
printf '%s4A' "$csi"   # CUU 4 → middle of the 8-row-tall image
printf '%s5C' "$csi"   # CUF 5 → indent
printf '%s1;33;41m TEXT OVER IMAGE %s0m' "$csi" "$csi"
# Drop back below the image so the next section starts on a fresh row.
printf '%s4B\r\n' "$csi"
pause 2

# ─────────────────────────────────────────────────────────────────
section "M11a.5 — Chunked upload (m=1 continuation + m=0 final)"
note "Splitting base64 into 4 KB chunks. First chunk carries the full"
note "header; continuation chunks only need i= and m=. Last chunk has"
note "m=0 to signal end. The renderer holds off displaying until m=0."
pause 1

chunk_size=4096
i=0
first=1
chunks=0
while [ $i -lt $TOTAL ]; do
    end=$((i + chunk_size))
    [ $end -gt $TOTAL ] && end=$TOTAL
    slice="${B64:$i:$((end - i))}"
    if [ $first -eq 1 ]; then
        if [ $end -lt $TOTAL ]; then
            printf '%sGa=T,f=100,i=6,c=15,r=10,m=1;%s%s' "$apc" "$slice" "$st"
        else
            # The whole payload fits in one chunk after all.
            printf '%sGa=T,f=100,i=6,c=15,r=10,m=0;%s%s' "$apc" "$slice" "$st"
        fi
        first=0
    elif [ $end -lt $TOTAL ]; then
        printf '%sGi=6,m=1;%s%s' "$apc" "$slice" "$st"
    else
        printf '%sGi=6,m=0;%s%s' "$apc" "$slice" "$st"
    fi
    chunks=$((chunks + 1))
    i=$end
done
echo "<- image displayed after $chunks chunks reassembled"
pause 2

# ─────────────────────────────────────────────────────────────────
section "M11a.6 — Delete (a=d,d=i,i=ID)"
note "Place image 7, sleep 1.5s, then delete it. The image should"
note "disappear; cells underneath are repainted."
pause 1
printf '%sGa=T,f=100,i=7,c=12,r=8;%s%s' "$apc" "$B64" "$st"
echo "<- image 7 placed; deleting in 1.5s"
sleep 1.5
# Delete by id. d=i deletes placements; d=I would delete + free.
printf '%sGa=d,d=i,i=7;%s' "$apc" "$st"
echo "<- image 7 deleted"
pause 2

# ─────────────────────────────────────────────────────────────────
section "M11a.7 — Unicode placeholder (tmux passthrough)"
note "Apps emit the placeholder codepoint U+10EEEE in cells where the"
note "image should appear. The SGR fg 256-color slot encodes the image"
note "id (low byte). Diacritics on each placeholder cell encode (row,"
note "col) within the source image grid."
note ""
note "This is the path tmux uses to forward kitty graphics through. We"
note "emit it ourselves below to verify the M11a integration."
pause 1
# Upload as image 8 (no display).
printf '%sGa=t,f=100,i=8,U=1,q=2;%s%s' "$apc" "$B64" "$st"
# Set SGR fg = Indexed256(8) so the placeholder cells resolve to image id 8.
printf '%s38;5;8m' "$csi"
# Print a 4-row x 6-col placeholder grid.
# Row diacritics: U+0305 (row 0), U+030D (row 1), U+030E (row 2), U+0310 (row 3).
# Col diacritics: U+0305 (col 0), U+030D (col 1), ..., U+0312 (col 5).
# bash $'...' interprets \u escapes (bash 4.2+).
for r_idx in 0 1 2 3; do
    case $r_idx in
        0) row_diacritic=$'̅' ;;
        1) row_diacritic=$'̍' ;;
        2) row_diacritic=$'̎' ;;
        3) row_diacritic=$'̐' ;;
    esac
    for c_idx in 0 1 2 3 4 5; do
        case $c_idx in
            0) col_diacritic=$'̅' ;;
            1) col_diacritic=$'̍' ;;
            2) col_diacritic=$'̎' ;;
            3) col_diacritic=$'̐' ;;
            4) col_diacritic=$'̒' ;;
            5) col_diacritic=$'̽' ;;
        esac
        printf '\U0010EEEE%s%s' "$row_diacritic" "$col_diacritic"
    done
    printf '\n'
done
printf '%s0m' "$csi"
note "If you see the image stitched together from the placeholder cells,"
note "the M11a unicode-placeholder path is working."
pause 2

# ─────────────────────────────────────────────────────────────────
section "M11a.8 — Query (a=q): existing vs missing image"
note "Query for image id 1 (placed earlier in this script — should be"
note "in the registry). Then query for id 99 (never uploaded)."
note "Replies land on stdin; the EXIT trap drains them so they don't"
note "leak into your interactive shell."
pause 1
printf '%sGa=q,i=1;%s' "$apc" "$st"
note "Queried i=1: registry should respond OK."
printf '%sGa=q,i=99;%s' "$apc" "$st"
note "Queried i=99: registry should respond ENOENT."
pause 1.5

# ─────────────────────────────────────────────────────────────────
section "M11b — Sixel (not yet implemented)"
note "Sixel is DCS-framed (ESC P ... ESC \\\\), not APC. The renderer"
note "currently does not understand Sixel sequences — anything emitted"
note "below will either be ignored or render as visible escape bytes."
note "M11b will wire a Sixel decoder into the same image registry the"
note "Kitty path populates, so the rendering side is reused."
pause 1
# A tiny valid Sixel sequence: a 2x2 red square. Most terminals that
# support sixel render this; toastty M11a does not.
note "Emitting a 2x2 red sixel — expect nothing to appear (M11b)."
printf '%sP0;0;0q"1;1;2;2#0;2;100;0;0#0~~$-~~%s' "${esc}P" "$st"
# (Yes, that's an intentionally-weird literal: ESC P after the prefix.
# Sixel introducer is ESC P, terminator is ST.)
note "If you see anything appear, M11b is unexpectedly partially wired."
pause 2

# ─────────────────────────────────────────────────────────────────
section "Done"
note "Try: kitty +kitten icat $JPG  (if installed) for a real-world test."
note "Try: chafa -f kitty -s 40x20 $JPG  (if installed)."
note "Cleanup: deleting all displayed images so the next prompt is clean."
# d=a deletes all placements; d=A also frees the registry.
printf '%sGa=d,d=A;%s' "$apc" "$st"
sleep 1
