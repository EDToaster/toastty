source "$PROTOCOL_TESTS_DIR/lib.sh"

title="B4 — d=i,i=N,p=M must delete only that placement"

description="Spec: 'If you specify a p key for the placement id as well, then only the placement with the specified image id and placement id will be deleted.' Toastty's d=i branch ignores p= and removes EVERY placement of the image (term.rs:3292-3298)."

expected="Two placements of image 1 sit side by side: p=1 on the left, p=2 on the right. Press 'd' to send d=i,i=1,p=1. On a fixed build, only the LEFT square vanishes. On buggy toastty, BOTH vanish."

run() {
    transmit_solid 1 48 200 60 200       # purple, registered but not placed

    # a=p can't auto-derive natural size (handler.rs:495 → 1x1 without c/r),
    # so give an explicit 3x3-cell span to make each placement visible.
    cursor_to 12 4
    place_image 1 1 "c=3,r=3"             # left: placement_id 1
    cursor_to 12 30
    place_image 1 2 "c=3,r=3"             # right: placement_id 2
    cursor_to 16 1
    printf 'Left = p=1     Right = p=2\n'

    cursor_to 18 1
    prompt "Press 'd' to send a=d,d=i,i=1,p=1. Any other key skips."
    if [[ "$(wait_key)" == "d" ]]; then
        printf '%s_Ga=d,d=i,i=1,p=1,q=2%s\\' "$esc" "$esc"
        cursor_to 19 1
        printf 'After d=i,p=1: spec → only left gone. Buggy → both gone.\n'
    fi
}
