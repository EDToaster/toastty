source "$PROTOCOL_TESTS_DIR/lib.sh"

title="m2 — very-negative z draws below the cell background"

description="Spec (z-index banding): z < INT32_MIN/2 draws BELOW everything including the cell background colors; INT32_MIN/2 <= z < 0 draws below text but ABOVE the cell background. Toastty previously split only on the sign of z, so all negative-z images drew above the cell bg."

expected="Two steps over the SAME row of blue-background cells. Step 1 (z=-2000000000): the green image is hidden BEHIND the blue background (you see blue, not green). Step 2 (z=-1): the green image shows ON TOP of the blue background (you see green). If step 1 already shows green, the below-cell-bg band is not honored."

# Paint a row of cells with a solid blue background (SGR 48) starting at
# canvas row `r`, `cols` cells wide, beginning at column `start_col`.
paint_bg_row() {
    local r="$1" start_col="$2" cols="$3"
    cursor_to "$r" "$start_col"
    printf '%s[48;2;0;0;255m' "$esc"   # blue background
    local i
    for (( i=0; i<cols; i++ )); do printf ' '; done
    printf '%s[0m' "$esc"
}

run() {
    # Green 64x64 image, id=9.
    transmit_solid 9 64 0 200 0

    # ---- Step 1: very negative z -> behind the cell background ----
    paint_bg_row 6 10 4
    cursor_to 6 10
    place_image 9 0 "c=4,r=1,z=-2000000000"
    cursor_to 8 1
    printf 'Step 1: z=-2000000000 over a blue-bg row (cols 10-13, canvas row 6).\n'
    cursor_to 9 1
    printf 'Spec: image hidden behind blue bg. Buggy: green shows over blue.\n'

    cursor_to 11 1
    prompt "Press SPACE for step 2 (z=-1, same cells)."
    wait_space

    # ---- Step 2: small negative z -> above bg, below text ----
    paint_bg_row 14 10 4
    cursor_to 14 10
    place_image 9 0 "c=4,r=1,z=-1"
    cursor_to 16 1
    printf 'Step 2: z=-1 over a blue-bg row (cols 10-13, canvas row 14).\n'
    cursor_to 17 1
    printf 'Spec: green image visible OVER the blue bg.\n'
}
