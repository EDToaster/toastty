source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M12 — d=c / d=q / d=x / d=y / d=z selectors"

description="Spec selectors that previously fell through to a no-op: c = placements under the cursor cell; q = placements at cell (x,y) with z-index z=; x = placements in a column; y = placements in a row (the real 'delete by row'); z = placements with a given z-index. Lowercase removes placements; uppercase also frees orphaned bytes."

# Inline helper: place a previously-transmitted image as a 3x3-cell square.
place_square() {
    local id="$1" pid="$2" row="$3" col="$4"; shift 4
    local extra="${1:-}"
    cursor_to "$row" "$col"
    if [[ -n "$extra" ]]; then
        place_image "$id" "$pid" "c=3,r=3,$extra"
    else
        place_image "$id" "$pid" "c=3,r=3"
    fi
}

# Re-draw the four scratch squares used by every sub-step.
setup_grid() {
    transmit_solid 1 48 220 40 40        # red
    transmit_solid 2 48 40 200 60        # green
    transmit_solid 3 48 60 120 240       # blue
    transmit_solid 4 48 230 200 40       # yellow
}

run() {
    setup_grid
    cursor_to 2 1
    printf 'Choose a selector to exercise:\n'
    cursor_to 3 1
    printf '  c = under cursor   q = cell+z   x = column   y = row   z = z-index\n'

    cursor_to 5 1
    prompt "Press one of: c q x y z"
    local sel
    sel="$(wait_one_of "cqxyz")"

    case "$sel" in
        c)
            # Two squares; cursor parked on the first one's cell.
            place_square 1 1 10 6         # rows 10-12, cols 6-8
            place_square 2 1 10 30        # rows 10-12, cols 30-32
            cursor_to 14 1
            printf 'Left red + right green. d=c deletes the one under the cursor.\n'
            cursor_to 11 7                # inside the LEFT square
            prompt "Cursor is on the left square. Press SPACE → a=d,d=c."
            wait_space
            printf '%s_Ga=d,d=c,q=2%s\\' "$esc" "$esc"
            cursor_to 16 1
            printf 'Spec: left (cursor-cell) square gone; right survives.\n'
            ;;
        q)
            # Two squares overlapping the SAME cell but different z.
            place_square 1 1 10 6 "z=5"   # z=5
            place_square 2 1 10 6 "z=9"    # z=9, same cell, drawn on top
            cursor_to 14 1
            printf 'Two squares stacked at cols 6-8, rows 10-12 (z=5 and z=9).\n'
            prompt "Press SPACE → a=d,d=q,x=6,y=10,z=9 (cell col6/row10, z=9)."
            wait_space
            printf '%s_Ga=d,d=q,x=6,y=10,z=9,q=2%s\\' "$esc" "$esc"
            cursor_to 16 1
            printf 'Spec: only the z=9 placement at that cell is removed; z=5 remains.\n'
            ;;
        x)
            place_square 1 1 6 6          # column band at cols 6-8
            place_square 2 1 16 6         # also cols 6-8 (same column)
            place_square 3 1 10 30        # cols 30-32 (different column)
            cursor_to 20 1
            printf 'Two squares in cols 6-8, one in cols 30-32.\n'
            prompt "Press SPACE → a=d,d=x,x=6 (delete column 6)."
            wait_space
            printf '%s_Ga=d,d=x,x=6,q=2%s\\' "$esc" "$esc"
            cursor_to 21 1
            printf 'Spec: both cols-6-8 squares gone; the cols-30-32 square stays.\n'
            ;;
        y)
            place_square 1 1 10 6         # rows 10-12
            place_square 2 1 10 30        # rows 10-12 (same row band)
            place_square 3 1 18 6         # rows 18-20 (different row)
            cursor_to 22 1
            printf 'Two squares on rows 10-12, one on rows 18-20.\n'
            prompt "Press SPACE → a=d,d=y,y=10 (delete row 10)."
            wait_space
            printf '%s_Ga=d,d=y,y=10,q=2%s\\' "$esc" "$esc"
            cursor_to 23 1
            printf 'Spec: both rows-10-12 squares gone; the rows-18-20 square stays.\n'
            ;;
        z)
            place_square 1 1 6 6 "z=4"    # z=4
            place_square 2 1 10 30 "z=4"   # z=4 (different position)
            place_square 3 1 16 6 "z=7"    # z=7
            cursor_to 20 1
            printf 'Two z=4 squares (anywhere) + one z=7 square.\n'
            prompt "Press SPACE → a=d,d=Z,z=4 (uppercase: delete z=4 AND free bytes)."
            wait_space
            printf '%s_Ga=d,d=Z,z=4,q=2%s\\' "$esc" "$esc"
            cursor_to 21 1
            printf 'Spec: both z=4 squares gone (bytes freed); the z=7 square stays.\n'
            ;;
    esac
}
