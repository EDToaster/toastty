source "$PROTOCOL_TESTS_DIR/lib.sh"

title="B11 — Unicode placeholder diacritic inheritance"

description="Spec: a placeholder cell with 0 diacritics inherits (row, col+1, id_msb) from its left neighbor when foreground + underline match. Toastty unconditionally reads diacritics.first() as row and .get(1) as col, defaulting both to 0 (term.rs:1785-1808) — no inheritance, so every bare cell paints source (0, 0)."

expected="A 2-column image (left red, right green) is registered as image 5 placement 1. Press 's' to render TWO adjacent placeholder cells: the first carries diacritics (row=0,col=0), the second has NO diacritics. Spec: red cell, then green cell (second inherited col=1). Buggy: red cell, then red cell (second defaults to col=0). Also depends on B10 being fixed for the lookup to work at all."

run() {
    # The placeholder source mapping samples a cell_pw×cell_ph tile per
    # cell at (col*cell_pw, row*cell_ph) (term.rs:1832), NOT image_w/cols.
    # So the red/green split must fall exactly on the cell_pw boundary:
    # build an image two cells wide (left cell red, right cell green) by
    # querying the live cell pixel size.
    query_cell_px                                # sets CELL_PW / CELL_PH
    transmit_split_rg 5 $((2 * CELL_PW)) "$CELL_PH" "U=1"  # cell-aligned split
    place_image 5 1 "c=2,r=1,U=1"                # virtual, 2 cells wide

    cursor_to 14 1
    printf 'Image 5 (left cell red | right cell green), cell=%sx%s px, virtual p=1.\n' "$CELL_PW" "$CELL_PH"
    cursor_to 16 1
    prompt "Press 's' to render 2 placeholder cells side-by-side at (row 18, col 4)."
    if [[ "$(wait_key)" == "s" ]]; then
        cursor_to 18 4
        printf '%s[38;5;5m%s[58;5;1m%s[4m' "$esc" "$esc" "$esc"
        placeholder_cell 0 0                     # explicit row=0, col=0
        placeholder_bare                          # bare — should inherit col=1
        printf '%s[0m' "$esc"
        cursor_to 20 1
        printf 'Spec: left cell red, right cell green.\n'
        printf 'Buggy: both cells red (bare cell defaulted to col=0).\n'
    fi
}
