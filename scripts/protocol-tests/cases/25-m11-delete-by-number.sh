source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M11 — d=n / d=N delete by image NUMBER (most recent wins)"

description="Spec: d=n deletes the most recent image with image-number I=. Toastty did not track an image-number→id map, so d=n was a no-op. The fix records number→most-recent-id at registration; d=n,I=K resolves K to the newest id and deletes its placements (N also frees bytes). NOTE: a transmit may not carry both i= and I= (B8), so number-tagged images use I= only and the terminal assigns the id."

# Inline helper: transmit-and-place a solid WxW square at the current cursor
# using image-NUMBER only (no i=). The terminal auto-assigns the id.
# args: num W r g b
transmit_place_numbered() {
    local num="$1" w="$2" r="$3" g="$4" b="$5"
    local payload
    payload=$(python3 <<PYEOF
import base64, sys
sys.stdout.write(base64.b64encode(bytes([$r, $g, $b, 255]) * ($w * $w)).decode())
PYEOF
)
    printf '%s_Ga=T,f=32,s=%d,v=%d,t=d,I=%d,c=3,r=3,q=2;%s%s\\' \
        "$esc" "$w" "$w" "$num" "$payload" "$esc"
}

run() {
    cursor_to 12 4
    transmit_place_numbered 7 48 220 40 40    # red square, I=7 (older)
    cursor_to 12 30
    transmit_place_numbered 7 48 40 200 60    # green square, I=7 (newer)

    cursor_to 16 1
    printf 'Left red = I=7 (registered first)   Right green = I=7 (registered last).\n'

    cursor_to 18 1
    prompt "Press 'd' to send a=d,d=n,I=7 (delete NEWEST image numbered 7; keep bytes)."
    if [[ "$(wait_key)" == "d" ]]; then
        printf '%s_Ga=d,d=n,I=7,q=2%s\\' "$esc" "$esc"
        cursor_to 19 1
        printf 'Spec: green (newest) vanishes; red stays. Press SPACE for stage 2.\n'
        wait_space
        cursor_to 21 1
        prompt "Press 'b' to send a=d,d=N,I=7 (uppercase: delete newest AND free its bytes)."
        if [[ "$(wait_key)" == "b" ]]; then
            printf '%s_Ga=d,d=N,I=7,q=2%s\\' "$esc" "$esc"
            cursor_to 22 1
            printf 'Spec: the newest image numbered 7 has its bytes freed.\n'
        fi
    fi
}
