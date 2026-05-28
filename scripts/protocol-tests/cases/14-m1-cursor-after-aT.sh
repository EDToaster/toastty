source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M1 — cursor end position after a=T"

description="Spec / reference kitty: after a cursor-moving placement the cursor does 'c->x += cols; c->y += rows - 1;' — it lands on the image's LAST row, one column past its right edge. The old toastty advanced row by 'rows' (off by one) and reset the column to start_col (dropping the 'cols' advance)."

expected="An image c=4,r=3 is placed at row 6, col 8 (1-based). The cursor moves, and we immediately print 'X' where it landed. Spec: 'X' sits at row 8 (start row 6 + rows-1 = 2) and col 12 (start col 8 + cols 4). Buggy-old: 'X' would land at row 9, col 8."

run() {
    # 1-based cursor: row 6, col 8 (toastty internal start_row=5, start_col=7).
    cursor_to 6 8
    transmit_and_place 1 48 0 160 220 "c=4,r=3"   # cyan, 4 cols x 3 rows
    # The cursor has now moved per the placement. Mark where it landed.
    printf 'X'

    cursor_to 14 1
    printf 'Spec: the "X" above is on the image LAST row (row 8) and one column\n'
    printf 'past the right edge (col 12). The image spans rows 6..8, cols 8..11.\n'
    printf 'Buggy-old: "X" lands one row too low (row 9) and back at col 8.\n'
}
