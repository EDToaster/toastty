source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M7 — RIS (ESC c) must reset the screen and clear images"

description="Spec (kitty 'Interaction with other terminal actions'): a full reset (RIS, ESC c) clears all visible images. Toastty additionally performs the conventional hard reset: cursor home, default SGR, scroll region back to full screen, screen + scrollback cleared. Previously esc_dispatch had no 'c' arm, so RIS was a silent no-op and stale images/text survived."

expected="An image and some bright text are on screen. Press 'r' to feed RIS (ESC c). Spec: the image vanishes, the text is gone, and the cursor jumps to home (top-left). Buggy: everything stays exactly as it was."

run() {
    cursor_to 3 4
    printf '%s[1;33mRIS should wipe this yellow line%s[0m' "$esc" "$esc"
    cursor_to 6 4
    transmit_and_place 1 64 0 160 220    # cyan-ish square at row 6

    cursor_to 16 1
    prompt "Press 'r' to feed RIS (ESC c). Any other key skips."
    if [[ "$(wait_key)" == "r" ]]; then
        printf '%sc' "$esc"
    fi

    cursor_to 22 1
    printf 'Spec: screen cleared, image gone, cursor homed. Buggy: nothing changed.\n'
}
