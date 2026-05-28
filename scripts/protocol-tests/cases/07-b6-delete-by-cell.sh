source "$PROTOCOL_TESTS_DIR/lib.sh"

title="B6 — d=p must delete by cell coords (x=, y=)"

description="Spec: d=p deletes all placements intersecting the cell (x, y), where x/y are LOWERCASE cell coords. Toastty implements d=p as a filter on (image_id, placement_id) from i=/p= (term.rs:3305-3311) — wrong selector entirely."

expected="An orange square is placed at row 12 col 10 with image_id=7. Press 'd' to send d=p,x=12,y=12 (a cell deep inside the image). Spec: square is deleted. Buggy: square survives (the buggy filter looks for i=0, p=0 instead of the cell)."

run() {
    transmit_solid 7 64 255 140 0        # orange, image_id=7

    cursor_to 12 10
    place_image 7 0                       # placement_id 0 (anonymous)
    cursor_to 18 1
    printf 'Orange square at (row 12, col 10), image_id 7, placement_id 0.\n'

    cursor_to 20 1
    prompt "Press 'd' to send a=d,d=p,x=12,y=12 (delete placements at cell col=12,row=12)."
    if [[ "$(wait_key)" == "d" ]]; then
        printf '%s_Ga=d,d=p,x=12,y=12,q=2%s\\' "$esc" "$esc"
        cursor_to 21 1
        printf 'Spec: orange square is gone. Buggy: it is still visible.\n'
    fi
}
