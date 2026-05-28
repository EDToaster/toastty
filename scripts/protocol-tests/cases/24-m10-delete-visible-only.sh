source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M10 — d=a / d=A scope to placements VISIBLE on screen"

description="Spec: a/A deletes 'all placements visible on screen', not literally everything. Toastty used to call image_grid.clear() (all) and drain the whole registry on A. The fix deletes only placements whose row range intersects the visible rows; uppercase A frees bytes only for images with no surviving placement."

expected="Two cyan squares (image 1, image 2). One sits high on screen (rows 3-5), the other low (rows 18-20). Both are on-screen, so d=a removes BOTH. Press 'd' to send a=d,d=a (lowercase, keep bytes). Then press 'r' to re-place image 1 (bytes were retained → it reappears). Spec: both vanish on 'd'; image 1 reappears on 'r'."

# Inline helper: place a previously-transmitted image as a 3x3-cell square
# at (row,col) so it is actually visible (a=p does not auto-derive size).
place_square() {
    local id="$1" pid="$2" row="$3" col="$4"
    cursor_to "$row" "$col"
    place_image "$id" "$pid" "c=3,r=3"
}

run() {
    transmit_solid 1 48 80 180 255       # cyan, image 1
    transmit_solid 2 48 80 180 255       # cyan, image 2

    place_square 1 1 3 6                  # high on screen (rows 3-5)
    place_square 2 1 18 6                 # low on screen (rows 18-20)

    cursor_to 7 1
    printf 'Top square = image 1     Bottom square = image 2 (both visible).\n'

    cursor_to 22 1
    prompt "Press 'd' to send a=d,d=a (delete all VISIBLE placements; keep bytes)."
    if [[ "$(wait_key)" == "d" ]]; then
        printf '%s_Ga=d,d=a,q=2%s\\' "$esc" "$esc"
        cursor_to 23 1
        printf 'Both squares should now be gone. Press SPACE for stage 2.\n'
        wait_space
        place_square 1 2 3 40            # re-place image 1 (bytes retained)
        cursor_to 23 1
        printf 'Spec: image 1 reappears (d=a kept bytes). d=A instead would free them.\n'
    fi
}
