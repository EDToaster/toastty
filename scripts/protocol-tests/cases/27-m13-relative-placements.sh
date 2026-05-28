source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M13 — relative placements (P=/Q=/H=/V=) with ENOPARENT"

description="Spec: a=p with P=<parent image> and Q=<parent placement> creates a RELATIVE placement, positioned at the parent placement's cell origin plus (H,V) cells. The cursor never moves for a relative placement, and the child follows the parent when the screen scrolls. References a missing parent reply ENOPARENT. Press 'a' to place a parent + relative child, 's' to scroll them together, 'e' to exercise the ENOPARENT error path."

expected="After 'a': a cyan parent placement appears (4x3 cells) and an orange child appears 2 columns right and 1 row below it; the cursor does NOT move (the 'A' marker stays where it started). After 's': both images shift up together by one row (the child stays at parent+offset). After 'e': a relative placement referencing a non-existent parent is rejected with an ENOPARENT reply (printed below); no orange child is created for that step."

# Inline: place a previously-transmitted image RELATIVE to a parent.
# args: child_id child_pid parent_id parent_pid H V [extra_keys]
place_relative_quiet() {
    local id="$1" pid="$2" pimg="$3" pplace="$4" h="$5" v="$6"; shift 6
    local extra="${1:-}"
    local keys="a=p,i=$id,p=$pid,P=$pimg,Q=$pplace,H=$h,V=$v,q=2"
    [[ -n "$extra" ]] && keys+=",$extra"
    printf '%s_G%s;%s\\' "$esc" "$keys" "$esc"
}

run() {
    # Pre-transmit two images (no display yet).
    transmit_solid 1 48 0 160 220     # cyan  (parent)
    transmit_solid 2 48 220 120 0     # orange (relative child)

    cursor_to 3 1
    prompt "Press 'a' to place the parent + a relative child."
    wait_one_of "a" >/dev/null

    # Place the parent (named placement 10) at row 5, col 4.
    cursor_to 5 4
    place_image 1 10 "c=4,r=3"        # absolute parent placement

    # Drop a marker at a known cursor location, then create the child.
    # The relative placement must NOT move the cursor, so the marker we
    # print AFTER it lands exactly where the cursor already was.
    cursor_to 14 4
    printf 'A'                        # cursor now at row 14, col 5
    # Child of (image 1, placement 10), offset H=2 cols, V=1 row.
    place_relative_quiet 2 20 1 10 2 1
    printf 'B'                        # 'B' must abut 'A' (cursor unmoved)

    cursor_to 18 1
    printf 'Parent: cyan 4x3 at row 5,col 4. Child: orange at row 6,col 6.\n'
    printf 'Cursor unmoved by the relative placement: "AB" are adjacent.\n'

    prompt "Press 's' to scroll the screen up (parent + child move together)."
    wait_one_of "s" >/dev/null
    # A full-screen scroll: park the cursor on the last row and emit a
    # newline so the screen scrolls up by one. The relative child should
    # track its parent up by the same row.
    cursor_to 24 1
    printf '\n'
    cursor_to 20 1
    printf 'Both images shifted up by one row; child still at parent+(2,1).\n'

    prompt "Press 'e' to exercise the ENOPARENT error path."
    wait_one_of "e" >/dev/null
    # Reference a parent image/placement that does not exist. Use q=0 so
    # the terminal actually replies; capture and display it.
    drain_input
    local reply
    reply=$(send_and_capture "a=p,i=2,p=99,P=222,Q=333,H=1,V=1")
    cursor_to 22 1
    if [[ "$reply" == *ENOPARENT* ]]; then
        ok "ENOPARENT reply received: $reply"
    else
        fail "expected ENOPARENT, got: ${reply:-<no reply>}"
    fi
    note "No child placement is created when the parent does not exist."
}
