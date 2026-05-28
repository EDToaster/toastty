source "$PROTOCOL_TESTS_DIR/lib.sh"

title="B3 — DECSTBM partial-region scroll must move images"

description="Spec: when a scroll region is active and scrolling occurs inside it, images entirely within the region must scroll with the text and be clipped if they'd leave it. Toastty calls image_grid.shift_rows_* only on the FULL-region branch of region_scroll_up/down (term.rs:1491-1515, 1535-1564)."

expected="An image is placed inside a scroll region. Press 's' to feed 3 LFs at the region bottom. Spec: the image moves UP 3 rows with the text. Buggy: text scrolls under the image; the image stays put."

run() {
    # Carve out a scroll region from canvas row 10 to 20 (1-based,
    # inclusive). canvas_row maps to the absolute screen rows the harness
    # placed the canvas at, so the region lines up with the cursor_to image.
    printf '%s[%d;%dr' "$esc" "$(canvas_row 10)" "$(canvas_row 20)"

    cursor_to 10 1
    printf '%s[2m═══ scroll region top (row 10) ═══%s[0m' "$esc" "$esc"
    cursor_to 16 1
    transmit_and_place 1 48 0 200 0    # green square at row 16
    cursor_to 20 1
    printf '%s[2m═══ scroll region bottom (row 20) ═══%s[0m' "$esc" "$esc"

    cursor_to 22 1
    prompt "Press 's' to scroll 3 LFs at the bottom of the region. Any other key skips."
    if [[ "$(wait_key)" == "s" ]]; then
        cursor_to 20 1
        printf '\n\n\n'
    fi

    # Restore full-screen scroll region before handing back to the harness.
    printf '%s[r' "$esc"
    # (region restore is screen-global; no canvas mapping needed)
    cursor_to 23 1
    printf 'Spec: green moved from row 16 → row 13. Buggy: still anchored at row 16.\n'
}
