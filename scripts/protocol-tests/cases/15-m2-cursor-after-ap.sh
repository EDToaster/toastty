source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M2 — standalone a=p moves the cursor (unless C=1)"

description="Spec: a placement with a=p and C=0 (default) moves the cursor by (cols, rows-1), exactly like a=T. The old toastty never advanced the cursor on a=p. Press 'a' to run the default-C=0 case, then 'c' to run the C=1 case."

expected="Default (C=0): an already-transmitted image is placed c=4,r=3 at row 6,col 8; 'X' prints at the cursor and must land just to the RIGHT of the square (row 8, col 12 — moved by cols=4, rows-1=2), so it is VISIBLE. With C=1: the cursor must NOT move, so it stays at the image's top-left (row 14, col 8); 'Y' is therefore drawn AT that covered cell and is HIDDEN BEHIND the square — which is correct, because a default z=0 image renders in front of text. If C=1 were broken and the cursor moved, 'Y' would instead appear to the right of the square."

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
    printf 'Spec C=0: "X" lands at row 8, col 12 — just RIGHT of the square,\n'
    printf '          so it is visible (cursor moved by cols=4, rows-1=2).\n'
    printf 'Spec C=1: cursor stays at the square top-left (row 14, col 8), so "Y"\n'
    printf '          is HIDDEN BEHIND the square (z=0 images draw over text).\n'
    printf 'Buggy-old: a=p never moved the cursor, so "X" would also stay top-left.\n'
}
