source "$PROTOCOL_TESTS_DIR/lib.sh"

title="B2 — Alt-screen 1049 must preserve primary images"

description="Spec: primary and alt screens maintain INDEPENDENT image lists. Toastty has one shared ImageGrid (term.rs:2606-2656); alt-enter wipes it and alt-exit wipes it again, so the primary image is permanently lost."

expected="Step 1 places a blue square on the primary. Step 2 enters alt screen — the square hides while we're there. Step 3 exits back to primary. On a fixed build, the blue square reappears. On buggy toastty, the primary screen comes back empty."

run() {
    transmit_and_place 1 48 0 80 255
    printf '\n'
    prompt "Press SPACE to enter alt screen (DECSET 1049)."
    wait_space
    printf '%s[?1049h%s[H' "$esc" "$esc"
    printf '── ALT SCREEN ──\n\n'
    printf 'Primary content is hidden. Press SPACE to return.\n'
    wait_space
    printf '%s[?1049l' "$esc"
    printf '\nBack on primary. The blue square above should still be visible.\n'
}
