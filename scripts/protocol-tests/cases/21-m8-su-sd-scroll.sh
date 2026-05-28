source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M8 — CSI S (SU) / CSI T (SD) must scroll text and images"

description="Spec: 'CSI Ps S' scrolls the screen UP Ps lines; 'CSI Ps T' scrolls it DOWN Ps lines (default 1). Post-blocker the region-scroll path already moves images, so SU/SD must move them too. The private form 'CSI ? ... S' is XTSMGRAPHICS and the 5-param 'CSI ;;;; T' is highlight-mouse-tracking — neither may scroll. Previously neither S nor T was implemented."

expected="A magenta square sits mid-screen. Press 'u' to feed 'CSI 3 S' (scroll up 3) — image + text move UP 3 rows. Press 'd' to feed 'CSI 3 T' (scroll down 3) — they move DOWN 3 rows. Press 'x' to feed XTSMGRAPHICS 'CSI ?1;0;0 S' — nothing should move."

run() {
    cursor_to 10 4
    printf '%s[2m── marker line at row 10 ──%s[0m' "$esc" "$esc"
    cursor_to 11 4
    transmit_and_place 1 64 200 0 200    # magenta square at row 11

    cursor_to 20 1
    prompt "Press 'u' = SU 3, 'd' = SD 3, 'x' = XTSMGRAPHICS probe (no-op). Any other key skips."
    local k
    k="$(wait_key)"
    case "$k" in
        u) printf '%s[3S' "$esc" ;;
        d) printf '%s[3T' "$esc" ;;
        x) printf '%s[?1;0;0S' "$esc" ;;   # XTSMGRAPHICS — must NOT scroll
    esac

    cursor_to 23 1
    printf "Spec: 'u'/'d' move image+text by 3 rows; 'x' leaves everything put.\n"
}
