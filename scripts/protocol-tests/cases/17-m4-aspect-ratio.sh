source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M4 — aspect ratio when only one of c= / r= is given"

description="Spec: when only c= (or only r=) is given, the other axis is derived from the SOURCE aspect ratio in pixel space: rows = ceil(cols * cell_pw * img_h / (img_w * cell_ph)). The old toastty derived the missing axis from the image's NATURAL cell count, ignoring the requested axis — so a wide image asked to be c=10 would still be too tall."

# Inline helper (NOT added to lib.sh): transmit a W x H solid-color RGBA
# image. lib.sh's transmit_solid is square-only.
transmit_solid_wh() {
    local id="$1" w="$2" h="$3" r="$4" g="$5" b="$6"; shift 6
    local extra="${1:-}"
    local payload
    payload=$(python3 <<PYEOF
import base64, sys
sys.stdout.write(base64.b64encode(bytes([$r, $g, $b, 255]) * ($w * $h)).decode())
PYEOF
)
    local keys="a=t,f=32,s=$w,v=$h,t=d,i=$id,q=2"
    [[ -n "$extra" ]] && keys+=",$extra"
    printf '%s_G%s;%s%s\\' "$esc" "$keys" "$payload" "$esc"
}

expected="A 200x100 (2:1, wide) image is placed with ONLY c=10. Spec: rows are derived from the aspect ratio. With a 10x20-px cell that is rows = ceil(10*10*100 / (200*20)) = ceil(2.5) = 3, so the picture stays a wide, undistorted 10x3 band. Buggy-old: rows = ceil(100/20) = 5, making it too tall (squished vertically)."

run() {
    query_cell_px
    note "cell pixel size reported as ${CELL_PW}x${CELL_PH}"

    transmit_solid_wh 1 200 100 200 60 200     # 200x100 magenta (2:1)

    cursor_to 6 2
    place_image 1 1 "c=10"                      # only c= ; rows derived

    cursor_to 14 1
    printf 'Spec (10x20 cell): a 10-cols x 3-rows band — rows derived from the\n'
    printf 'source 2:1 aspect ratio: ceil(10*10*100 / (200*20)) = 3.\n'
    printf 'Buggy-old: 10 cols x 5 rows — vertically squished, ratio broken.\n'
}
