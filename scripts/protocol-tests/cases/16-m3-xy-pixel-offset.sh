source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M3 — X= / Y= are intra-cell PIXEL offsets, not cell offsets"

description="Spec: X / Y are the x/y offset IN PIXELS within the first cell at which to start displaying the image (must be smaller than the cell). The starting cell comes from the cursor. The old toastty treated X/Y as a CELL offset and shifted the placement's starting cell (and the renderer ignored them entirely)."

expected="Two copies of the same image are placed at the SAME cursor cell (row 8, col 8). The first has X=0,Y=0; the second (press 'o') has X=half-cell, Y=half-cell. Spec: the second copy occupies the SAME cells but is nudged a few pixels right+down inside the first cell. Buggy-old: X/Y would shift the whole placement by whole cells (or do nothing at all)."

run() {
    # Cell pixel size -> pick sub-cell offsets strictly smaller than a cell.
    query_cell_px
    local off_x=$(( CELL_PW / 2 ))
    local off_y=$(( CELL_PH / 2 ))
    (( off_x < 1 )) && off_x=1
    (( off_y < 1 )) && off_y=1

    transmit_split_rg 1 48 48          # left red / right green, to see the shift

    cursor_to 8 8
    place_image 1 1 "c=4,r=3"          # baseline: X=0,Y=0 at this cell
    note "baseline placed at row 8, col 8 with X=0,Y=0 (p=1)"

    cursor_to 4 1
    prompt "Press 'o' to place the SAME image at the SAME cell with X=$off_x,Y=$off_y (p=2)."
    wait_one_of "o" >/dev/null

    cursor_to 8 8
    place_image 1 2 "c=4,r=3,X=$off_x,Y=$off_y,z=1"   # z=1 so it draws on top

    cursor_to 14 1
    printf 'Spec: the second copy covers the SAME 4x3 cells (rows 8..10, cols 8..11)\n'
    printf 'but is offset by (%d, %d) pixels INSIDE the first cell — a sub-cell nudge\n' "$off_x" "$off_y"
    printf 'right and down. Buggy-old: it would jump whole cells, or not move at all.\n'
}
