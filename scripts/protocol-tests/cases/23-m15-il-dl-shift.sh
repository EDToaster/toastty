source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M15 — IL (CSI L) / DL (CSI M) must shift images with the text"

description="Spec: kitty shifts images on Insert Line / Delete Line. IL pushes the lines at and below the cursor DOWN within the scroll region (opening blank rows); DL pulls them UP. Toastty's insert_line/delete_line moved the text cells but left placements behind. The fix mirrors the text shift on the image layer, bounded by [cursor_row, region_end)."

expected="A blue square sits a few rows below the cursor. Press 'i' to feed 'CSI 2 L' (insert 2 lines at the cursor) — the image moves DOWN 2 rows. Press 'd' to feed 'CSI 2 M' (delete 2 lines at the cursor) — the image moves UP 2 rows. Buggy: text shifts but the image stays anchored where it was."

run() {
    cursor_to 8 4
    transmit_and_place 1 64 30 90 220    # blue square at row 8
    cursor_to 9 4
    printf '%s[2m(text line below the image, also shifts)%s[0m' "$esc" "$esc"

    # Park the cursor a couple rows ABOVE the image so IL/DL act on it.
    cursor_to 6 1

    cursor_to 18 1
    prompt "Press 'i' = IL 2 (image down), 'd' = DL 2 (image up). Any other key skips."
    local k
    k="$(wait_key)"
    case "$k" in
        i) cursor_to 6 1; printf '%s[2L' "$esc" ;;   # IL 2
        d) cursor_to 6 1; printf '%s[2M' "$esc" ;;   # DL 2
    esac

    cursor_to 23 1
    printf "Spec: 'i' moves the image down 2 rows; 'd' moves it up 2 rows.\n"
}
