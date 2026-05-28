source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M2 — standalone a=p moves the cursor (unless C=1)"

description="Spec: a placement with a=p and C=0 (default) moves the cursor by (cols, rows-1), exactly like a=T. The old toastty never advanced the cursor on a=p. Press 'a' to run the default-C=0 case, then 'c' to run the C=1 case."

expected="Default (C=0): an already-transmitted image is placed c=4,r=3 at row 6,col 8; 'X' is printed at the cursor and must land at row 8, col 12. With C=1: the image is placed but the cursor must NOT move — 'Y' prints right where it started (row 14, col 8)."

run() {
    # Pre-transmit two images (no display yet).
    transmit_solid 1 48 0 160 220     # cyan
    transmit_solid 2 48 220 120 0     # orange

    cursor_to 4 1
    prompt "Press 'a' for the default a=p (C=0) case."
    wait_one_of "a" >/dev/null

    cursor_to 6 8
    place_image 1 0 "c=4,r=3"         # a=p, C=0 default -> cursor moves
    printf 'X'

    cursor_to 12 1
    prompt "Press 'c' for the a=p,C=1 case (cursor must NOT move)."
    wait_one_of "c" >/dev/null

    cursor_to 14 8
    place_image 2 0 "c=4,r=3,C=1"     # a=p, C=1 -> cursor stays put
    printf 'Y'

    cursor_to 20 1
    printf 'Spec C=0: "X" lands at row 8, col 12 (moved by cols=4, rows-1=2).\n'
    printf 'Spec C=1: "Y" stays at row 14, col 8 (no cursor motion).\n'
    printf 'Buggy-old: a=p never moved the cursor, so "X" stayed at row 6, col 8.\n'
}
