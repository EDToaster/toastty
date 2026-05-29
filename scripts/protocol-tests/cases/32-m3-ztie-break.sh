source "$PROTOCOL_TESTS_DIR/lib.sh"

title="m3 — equal-z tie-break by image id (lower id below)"

description="Spec: if two images with the same z-index overlap, the image with the LOWER id is considered to have the lower z-index (draws under). Toastty previously broke ties by insertion order; now the renderer sorts by (z, image_id)."

expected="Two overlapping images at the SAME z=0, placed over the same cells. id=3 is RED, id=8 is BLUE; both are placed at the same spot with id=3 transmitted/placed LAST. Spec: BLUE (higher id) wins on top regardless of placement order — you should see BLUE. If you see RED, the tie-break still uses insertion order."

run() {
    # Solid red (id=3) and solid blue (id=8), 64x64 each.
    transmit_solid 8 64 0 0 255          # blue, higher id
    transmit_solid 3 64 255 0 0          # red, lower id

    # Place the LOWER-id (red) image LAST so insertion order would put it
    # on top; the spec tie-break must instead put the higher-id blue on top.
    cursor_to 6 10
    place_image 8 0 "c=4,r=2,z=0"        # blue, same cells, z=0
    cursor_to 6 10
    place_image 3 0 "c=4,r=2,z=0"        # red, same cells, z=0, placed last

    cursor_to 10 1
    printf 'Two z=0 images overlap at canvas rows 6-7, cols 10-13.\n'
    cursor_to 11 1
    printf 'id=3 RED (placed last), id=8 BLUE. Spec: higher id (BLUE) on top.\n'
    cursor_to 12 1
    printf 'Buggy (insertion order): RED on top.\n'
}
