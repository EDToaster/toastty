source "$PROTOCOL_TESTS_DIR/lib.sh"

title="B10 — Unicode placeholder image_id decoding"

description="Spec: a placeholder cell decodes image_id from foreground color (low 24 bits) PLUS the 3rd diacritic (high 8 bits); the underline color is the placement_id. Toastty mis-reads underline as the high byte of image_id and ignores the 3rd diacritic (term.rs:3557-3575, 1781-1808)."

expected="Image id=5 is transmitted and registered as a virtual placement with placement_id=1. Press 's' to render a placeholder cell with fg=5 (indexed), underline=1, diacritics (row=0, col=0). Spec: a small blue patch appears at the placeholder cell. Buggy: the cell decodes image_id as (1<<8|5)=261 and lookup fails — nothing renders, or wrong image."

run() {
    transmit_solid 5 64 0 100 255 "U=1"          # blue, virtual transmit
    place_image 5 1 "U=1"                        # virtual placement, p=1

    cursor_to 14 1
    printf 'Image 5 transmitted, virtual placement p=1 created.\n'
    cursor_to 16 1
    prompt "Press 's' to render the placeholder cell at (row 18, col 4)."
    if [[ "$(wait_key)" == "s" ]]; then
        cursor_to 18 4
        printf '%s[38;5;5m%s[58;5;1m%s[4m' "$esc" "$esc" "$esc"
        placeholder_cell 0 0
        printf '%s[0m' "$esc"
        cursor_to 20 1
        printf 'Spec: a blue patch (top-left cell of image 5) appears at row 18 col 4.\n'
        printf 'Buggy: cell is blank or shows the wrong image (lookup misses).\n'
    fi
}
