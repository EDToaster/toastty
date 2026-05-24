#!/usr/bin/env bash
# M12 demo — Ratty Graphics Protocol (RGP). Run inside toastty to
# render a 3D cube as part of the terminal surface.
#
# Usage:
#   cargo run --release          # launches toastty
#   # then in the shell that opens:
#   ./scripts/m12_demo.sh
#
# What the script does:
#   1. Sends a support-query (`s`) and reads the capability reply.
#   2. Registers the bundled procedural unit cube via the
#      `path=cube` lookup (no external asset needed).
#   3. Places the cube at a few cell-grid anchors with different
#      rotations / colors / brightness to show the protocol's
#      transform + tint fields.
#   4. Updates an existing placement's rotation via the `u` verb.
#   5. Cleans up with `d` (delete all RGP placements).
#
# Wire format: `ESC _ ratty;g;<verb>;<k=v>;... ESC \`. Decision
# §1 in docs/decisions/rgp-protocol.md explains the path policy.

set -u

esc=$'\033'
csi="${esc}["
st="${esc}\\"
apc="${esc}_"

# Same TTY-echo gymnastics as the M11 demo — RGP support query
# emits a reply that the terminal echoes back through the PTY if
# we leave echo on.
old_stty=$(stty -g 2>/dev/null || true)
stty -echo 2>/dev/null || true

drain_stdin() {
    while IFS= read -r -t 0.05 -n 4096 -s _drain 2>/dev/null; do :; done
}
restore_terminal() {
    if [ -n "${old_stty:-}" ]; then
        stty "${old_stty}" 2>/dev/null || true
    else
        stty echo 2>/dev/null || true
    fi
    drain_stdin
}
trap restore_terminal EXIT

rgp() {
    # Frame an RGP body in APC.
    printf '%s%s%s' "${apc}ratty;g;$1" "${st}"
}

section() {
    printf '%s\n--- %s ---%s\n' "${csi}1m" "$1" "${csi}0m"
}

section "1. Support query (capabilities reply on stdin, drained silently)"
rgp 's'
sleep 0.2
drain_stdin

section "2. Clear the screen + home"
printf '%s2J%sH' "${csi}" "${csi}"

section "3. Register the bundled cube under id 1"
rgp 'r;id=1;fmt=glb;path=cube'

section "4. Place at row 2, col 4, 6x4 cells, default depth"
rgp 'p;id=1;row=2;col=4;w=6;h=4;rx=20;ry=30'
sleep 1.5

section "5. Add a second placement with a color tint + brightness"
rgp 'r;id=2;fmt=glb;path=cube'
rgp 'p;id=2;row=2;col=20;w=6;h=4;rx=-15;ry=45;color=ff8844;brightness=1.4'
sleep 1.5

section "6. Add a third placement BEHIND text (depth=+5)"
rgp 'r;id=3;fmt=glb;path=cube'
rgp 'p;id=3;row=8;col=4;w=6;h=4;ry=60;depth=5;color=66ccff'
# Print some text on top of where the cube sits to show occlusion.
printf '%s10;6H%sCUBE SITS BEHIND THIS TEXT (depth=+5)%s' "${csi}" "${csi}33m" "${csi}0m"
sleep 2

section "7. Add a fourth placement IN FRONT of text (depth=-5)"
rgp 'r;id=4;fmt=glb;path=cube'
rgp 'p;id=4;row=8;col=20;w=6;h=4;ry=-60;depth=-5;color=cc66ff'
printf '%s10;22HTHIS TEXT IS BEHIND CUBE%s' "${csi}" "${csi}0m"
sleep 2

section "8. Update placement 1 (rotate continuously)"
for ang in 0 30 60 90 120 150 180 210 240 270 300 330 360; do
    rgp "u;id=1;ry=${ang}"
    sleep 0.08
done

section "9. Delete placement 2 (others remain)"
rgp 'd;id=2'
sleep 1

section "10. Delete all RGP state"
rgp 'd'
sleep 0.5

printf '%s\nDemo done. Restoring terminal.%s\n' "${csi}1m" "${csi}0m"
