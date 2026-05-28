source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M5 — re-emitting (image_id, placement_id) REPLACES, not stacks"

description="Spec: 'If you send two placements with the same image id and placement id the second one will replace the first.' The old toastty called image_grid.add() unconditionally, so re-placing i=1,p=1 accumulated layers (memory growth + z-fight on equal z)."

expected="Image i=1,p=1 is placed at row 6,col 8. Press 'm' to RE-place the SAME i=1,p=1 at a new spot (row 10,col 30). Spec: exactly ONE placement survives — it MOVES to the new location and the old one disappears. Buggy-old: BOTH copies remain on screen."

run() {
    transmit_solid 1 48 220 60 60      # red

    cursor_to 6 8
    place_image 1 1 "c=4,r=3"          # first placement of (i=1, p=1)
    note "placed (i=1, p=1) at row 6, col 8"

    cursor_to 3 1
    prompt "Press 'm' to re-place the SAME (i=1, p=1) at row 10, col 30."
    wait_one_of "m" >/dev/null

    cursor_to 10 30
    place_image 1 1 "c=4,r=3"          # re-emit same id pair -> must replace

    cursor_to 16 1
    printf 'Spec: ONLY the new copy (row 10, col 30) is visible — the original at\n'
    printf 'row 6, col 8 is gone (replaced). Buggy-old: both copies stay on screen.\n'
}
