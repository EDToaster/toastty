source "$PROTOCOL_TESTS_DIR/lib.sh"

title="B6 — d=p must delete by cell coords (x=, y=)"

description="Spec: d=p deletes all placements intersecting the cell (x, y), where x/y are LOWERCASE cell coords. Toastty implements d=p as a filter on (image_id, placement_id) from i=/p= (term.rs:3305-3311) — wrong selector entirely."

expected="An orange 3x3-cell square covers 1-based rows 12-14, cols 10-12 (image_id=7). Press 'd' to send d=p,x=12,y=12 (a cell inside the square). Spec: square is deleted. Buggy: square survives (the buggy filter looks for i=0, p=0 instead of the cell)."

run() {
    transmit_solid 7 64 255 140 0        # orange 64x64, image_id=7

    cursor_to 12 10
    # NOTE: toastty's a=p path does not auto-derive the natural cell span
    # from a previously-transmitted image (handler.rs:495 passes img 0x0,
    # so without c=/r= the placement collapses to a single 1x1 cell). Give
    # an explicit 3x3-cell span so there is a real multi-cell delete target,
    # matching the Rust oracle `b6_delete_by_cell...` (c=3,r=3).
    place_image 7 0 "c=3,r=3"            # rows 12-14, cols 10-12 (1-based)
    cursor_to 18 1
    printf 'Orange 3x3-cell square at rows 12-14, cols 10-12 (image_id 7).\n'

    cursor_to 20 1
    prompt "Press 'd' to send a=d,d=p,x=12,y=12 (delete placements at cell col=12,row=12)."
    if [[ "$(wait_key)" == "d" ]]; then
        printf '%s_Ga=d,d=p,x=12,y=%d,q=2%s\\' "$esc" "$(canvas_row 12)" "$esc"
        cursor_to 21 1
        printf 'Spec: orange square is gone. Buggy: it is still visible.\n'
    fi
}
