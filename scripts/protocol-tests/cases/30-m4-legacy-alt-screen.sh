source "$PROTOCOL_TESTS_DIR/lib.sh"

title="m4 — DECSET 47 / 1047 legacy alt screen isolates + restores images"

description="Spec/parity: only DECSET 1049 was honored; the older 47 / 1047 alt-buffer variants hit the catch-all and did nothing. The fix routes 47/1047 through the same enter/exit path as 1049, so the primary image grid is stashed on enter (alt starts blank) and restored on exit (no image leak across the switch). 1048 (cursor save/restore only) is handled separately and is not exercised here."

expected="A magenta square (image 1) on the PRIMARY screen. Press 'a' to enter the alt screen via DECSET 47 (\\x1b[?47h): the square must vanish (alt grid is empty/isolated). A cyan square (image 2) is placed on the alt screen. Press 'x' to exit via \\x1b[?47l: the alt cyan square must disappear and the PRIMARY magenta square must reappear unchanged."

# Inline helper: place a previously-transmitted image as a 3x3-cell square
# at canvas-relative (row,col) so it is actually visible.
place_square() {
    local id="$1" pid="$2" row="$3" col="$4"
    cursor_to "$row" "$col"
    place_image "$id" "$pid" "c=3,r=3"
}

run() {
    # Transmit both images up front (registry survives the alt switch;
    # only the placement grid is stashed/restored).
    transmit_solid 1 48 200 60 200       # magenta, image 1 (primary)
    transmit_solid 2 48 60 180 200       # cyan, image 2 (alt)

    # PRIMARY: show the magenta square.
    place_square 1 1 3 6
    cursor_to 7 1
    printf 'PRIMARY screen: magenta square = image 1.\n'

    cursor_to 22 1
    prompt "Press 'a' to enter the alt screen via DECSET 47 (\\x1b[?47h)."
    if [[ "$(wait_key)" == "a" ]]; then
        printf '%s[?47h' "$esc"          # enter alt buffer (mode 47)

        # On the (now blank) alt screen, place the cyan square.
        place_square 2 1 3 6
        cursor_to 7 1
        printf 'ALT screen (mode 47): magenta is gone; cyan square = image 2.\n'

        cursor_to 22 1
        prompt "Press 'x' to exit the alt screen via \\x1b[?47l."
        if [[ "$(wait_key)" == "x" ]]; then
            printf '%s[?47l' "$esc"      # exit alt buffer (mode 47)
            cursor_to 7 1
            printf 'Back on PRIMARY: cyan gone, magenta square (image 1) restored.\n'
            cursor_to 23 1
            printf 'Spec: 47/1047 stash + restore the primary image grid like 1049.\n'
        fi
    fi
}
