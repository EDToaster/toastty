source "$PROTOCOL_TESTS_DIR/lib.sh"

title="B12 — U=1 (virtual placement) must be invisible"

description="Spec: U=1 creates a VIRTUAL placement — the image is registered but not displayed; subsequent U+10EEEE cells provide the visible references. Toastty's handler.rs:429-447 still emits a visible placement at the cursor AND advances the cursor when U=1 is set."

expected="Print 'BEFORE>', then send a=T with U=1 (a 48×48 yellow square), then print '<AFTER'. Spec: 'BEFORE><AFTER' on one line — the virtual transmit added nothing visible and didn't move the cursor. Buggy: a yellow square appears between BEFORE> and <AFTER, and the cursor jumps past it."

run() {
    cursor_to 14 1
    printf 'BEFORE>'
    transmit_and_place 99 48 255 220 0 "U=1"     # yellow, virtual
    printf '<AFTER\n'

    cursor_to 18 1
    printf 'Spec: the line above reads "BEFORE><AFTER" with no gap and no image.\n'
    printf 'Buggy: a yellow square sits between BEFORE> and <AFTER, and they\n'
    printf 'land on different rows because the cursor was advanced.\n'
}
