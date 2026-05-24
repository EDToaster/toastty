#!/usr/bin/env bash
# Renders a single RGP cube and rotates it continuously around Y (or
# the axes named in $1). Press Ctrl-C to stop.
#
# Usage:
#   cargo run --release          # launches toastty
#   # then in the spawned shell:
#   ./scripts/m12_rotate.sh         # spin around Y
#   ./scripts/m12_rotate.sh xy      # spin around X and Y
#   ./scripts/m12_rotate.sh xyz     # spin around all three axes
#
# Tuneables (env):
#   STEP_DEG=3       Degrees per frame (default 3)
#   FRAME_MS=33      Sleep between frames in ms (default 33 ~ 30 fps)
#   ROW=3            Anchor row (top edge of placement)
#   COL=8            Anchor col (left edge of placement)
#   W=12             Width in cells
#   H=8              Height in cells
#   COLOR=88ccff     RRGGBB tint
#   BRIGHTNESS=1.2   Lighting multiplier

set -u

axes="${1:-y}"
STEP_DEG="${STEP_DEG:-3}"
FRAME_MS="${FRAME_MS:-33}"
ROW="${ROW:-3}"
COL="${COL:-8}"
W="${W:-12}"
H="${H:-8}"
COLOR="${COLOR:-88ccff}"
BRIGHTNESS="${BRIGHTNESS:-1.2}"

esc=$'\033'
csi="${esc}["
st="${esc}\\"
apc="${esc}_"

# TTY-echo discipline: support-query reply for `s` would echo back
# through cooked-mode otherwise. We don't query here, but the same
# discipline keeps the demo output clean.
old_stty=$(stty -g 2>/dev/null || true)
stty -echo 2>/dev/null || true

drain_stdin() {
    while IFS= read -r -t 0.05 -n 4096 -s _drain 2>/dev/null; do :; done
}
restore_terminal() {
    # Best-effort: delete all RGP placements + restore tty.
    printf '%sratty;g;d%s' "${apc}" "${st}" 2>/dev/null || true
    if [ -n "${old_stty:-}" ]; then
        stty "${old_stty}" 2>/dev/null || true
    else
        stty echo 2>/dev/null || true
    fi
    drain_stdin
}
trap restore_terminal EXIT INT TERM

rgp() {
    printf '%s%s%s' "${apc}ratty;g;$1" "${st}"
}

# Convert ms to fractional seconds for `sleep`.
sleep_frac="$(awk -v ms="${FRAME_MS}" 'BEGIN { printf "%.3f", ms / 1000.0 }')"

# Clear, home cursor, hide it.
printf '%s2J%sH%s?25l' "${csi}" "${csi}" "${csi}"

# Register + initial place.
rgp "r;id=1;fmt=glb;path=cube"
rgp "p;id=1;row=${ROW};col=${COL};w=${W};h=${H};color=${COLOR};brightness=${BRIGHTNESS};rx=20;ry=0;rz=10"

# Print a label so the cube has something to occlude/be-occluded by
# when you experiment with `depth=`.
label_row=$(( ROW + H + 2 ))
label_col=$(( COL + 1 ))
printf '%s%d;%dH%s%s< rotating RGP cube — Ctrl-C to stop >%s' \
    "${csi}" "${label_row}" "${label_col}" \
    "${csi}1;37m" "" "${csi}0m"

ang=0
while :; do
    ang=$(( (ang + STEP_DEG) % 360 ))

    # Build the `u` body. Always rotate around Y unless overridden.
    body="u;id=1"
    case "${axes}" in
        *x*) body="${body};rx=${ang}" ;;
    esac
    case "${axes}" in
        *y*) body="${body};ry=${ang}" ;;
    esac
    case "${axes}" in
        *z*) body="${body};rz=${ang}" ;;
    esac
    rgp "${body}"

    sleep "${sleep_frac}"
done
