source "$PROTOCOL_TESTS_DIR/lib.sh"

title="B5 — d=I must free bytes only when no placements remain"

description="Spec: uppercase delete variants 'will delete the image data as well, provided that the image is not referenced elsewhere'. Toastty's d=I unconditionally drops registry bytes (term.rs:3292-3298), so re-placing the image afterwards fails. Depends on B4 also being fixed (this exercise sends p= as a selector)."

expected="Two placements of image 1 are shown. Press 'd' → d=I,i=1,p=1 (delete LEFT only, also free bytes if none remain). Press 'r' → a=p,i=1,p=3 to re-place. Spec: only left vanishes after 'd'; the new placement appears after 'r' because bytes are still around. Buggy: 'd' wipes everything AND frees bytes, so 'r' fails silently (or ENOENT)."

run() {
    transmit_solid 1 48 80 180 255       # cyan

    cursor_to 12 4
    place_image 1 1
    cursor_to 12 30
    place_image 1 2
    cursor_to 16 1
    printf 'Left = p=1     Right = p=2\n'

    cursor_to 18 1
    prompt "Press 'd' to send a=d,d=I,i=1,p=1 (delete left placement; preserve bytes since p=2 still references)."
    if [[ "$(wait_key)" == "d" ]]; then
        printf '%s_Ga=d,d=I,i=1,p=1,q=2%s\\' "$esc" "$esc"
        cursor_to 19 1
        printf 'Now press SPACE for stage 2.\n'
        wait_space
        cursor_to 12 60
        place_image 1 3                   # re-place as p=3 on the far right
        cursor_to 21 1
        printf 'Spec: cyan square reappears at far right (bytes were retained).\n'
        printf 'Buggy: nothing new appears — bytes were freed despite p=2 surviving.\n'
    fi
}
