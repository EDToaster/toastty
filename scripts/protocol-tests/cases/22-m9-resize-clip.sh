source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M9 — Window resize must clip image placements to the new geometry"

description="Spec: kitty clips image placements when the window is resized. Toastty's Term::resize previously changed geometry without trimming placements, so an image anchored near the old bottom became a phantom (its row_range pointed past the new last row). The fix walks image_grid after resize, clips each placement to the new rows/cols, and drops any whose range collapses to empty."

expected="An image is placed near the BOTTOM of the window. This trigger is window-driven (a resize is not an escape sequence), so it can't be fired automatically. MANUALLY drag the toastty window SMALLER — short enough that the image's old bottom rows fall off the new bottom edge. Spec: the image is clipped at the new bottom edge (or disappears entirely if its top is now off-screen) and no phantom row is left painted past the bottom. Buggy: the image keeps its old span and paints onto rows that no longer exist."

run() {
    cursor_to 2 1
    printf '%s[2mManually resize the toastty window SMALLER and watch the image below.%s[0m' "$esc" "$esc"

    # Place the image low on the screen. We use a tall band so part of it
    # is guaranteed to fall past a modest shrink.
    cursor_to 18 4
    transmit_and_place 1 96 220 120 0    # orange square low on screen

    cursor_to 23 1
    printf 'No keypress drives this — drag the window edge up to shrink rows.\n'
    prompt "Press any key when done observing to return to the harness."
    wait_key >/dev/null
}
