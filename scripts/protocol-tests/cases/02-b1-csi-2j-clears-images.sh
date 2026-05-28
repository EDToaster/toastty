source "$PROTOCOL_TESTS_DIR/lib.sh"

title="B1 — CSI 2J / 3J must clear images"

description="Spec: 'The clear screen escape code (usually ESC[2J) should also clear all images.' Toastty's erase_display 2/3 arm calls grid.clear_visible(...) but never image_grid.clear() (term.rs:2189-2193) — images survive a clear."

expected="Press 'c' to send CSI 2J + CSI 3J + cursor home. On a fixed build, the red square AND all the test prose vanish. On buggy toastty, the prose clears but the red square remains floating on screen."

run() {
    transmit_and_place 1 48 255 0 0
    printf '\n'
    prompt "Press 'c' to send CSI 2J + 3J. Any other key to skip."
    if [[ "$(wait_key)" == "c" ]]; then
        printf '%s[3J%s[2J%s[H' "$esc" "$esc" "$esc"
        printf 'After CSI 2J + 3J + home.\n'
        printf 'Spec: nothing else on screen. Buggy: the red square is still visible.\n'
    fi
}
