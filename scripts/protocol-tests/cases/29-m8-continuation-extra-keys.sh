source "$PROTOCOL_TESTS_DIR/lib.sh"

title="m8 — continuation chunks tolerate extra/incidental keys"

description="Spec: continuation chunks 'must have only the m and optionally q keys'; terminals should ignore extras, not validate them. Reference kitty ignores everything except m= (and the running payload) on continuation chunks. Toastty previously compared format/compression/action/source dims between chunks and rejected any mismatch with EINVAL (handler.rs headers_continuation_compatible)."

expected="Press 's' to send a 2-chunk upload+display whose SECOND chunk carries a stray key (S=999,f=100) it should never have. On a fixed build a solid blue square renders and NO error reply is produced. Buggy: the upload is rejected with EINVAL and nothing renders."

# ---------------------------------------------------------------------------
# Inline helper: send a solid N×N image as two chunks, displaying it (a=T).
# The continuation chunk carries the extra keys passed in $6.
# args: id N r g b extra_keys_on_continuation
# ---------------------------------------------------------------------------
chunked_place_with_extra_cont() {
    local id="$1" n="$2" r="$3" g="$4" b="$5" extra="$6"
    local payload c1 c2 half
    payload=$(python3 <<PYEOF
import base64, sys
sys.stdout.write(base64.b64encode(bytes([$r, $g, $b, 255]) * ($n * $n)).decode())
PYEOF
)
    half=$(( ${#payload} / 2 ))
    c1="${payload:0:half}"
    c2="${payload:half}"
    # First chunk: full header, a=T (transmit+display), more coming (m=1).
    printf '%s_Ga=T,f=32,s=%d,v=%d,t=d,i=%d,m=1;%s%s\\' \
        "$esc" "$n" "$n" "$id" "$c1" "$esc"
    # Final chunk: m=0 plus stray incidental keys that must be ignored.
    printf '%s_Gi=%d,%s,m=0;%s%s\\' \
        "$esc" "$id" "$extra" "$c2" "$esc"
}

run() {
    cursor_to 12 1
    printf 'First chunk:  \\e_Ga=T,f=32,s=32,v=32,t=d,i=8,m=1;<half>\\e\\\\\n'
    cursor_to 13 1
    printf 'Second chunk: \\e_Gi=8,S=999,f=100,m=0;<half>\\e\\\\  (S=,f= are stray)\n'

    cursor_to 15 1
    prompt "Press 's' to send the 2-chunk upload with stray keys on the continuation. Any other key skips."
    if [[ "$(wait_key)" == "s" ]]; then
        drain_input
        cursor_to 17 1
        chunked_place_with_extra_cont 8 32 0 0 255 "S=999,f=100"
        # Capture any reply (with q omitted the only reply would be an error,
        # since a=T success is verbose-OK too; we just check for EINVAL).
        local reply
        reply=$(capture_reply 1)
        cursor_to 22 1
        if [[ "$reply" == *EINVAL* ]]; then
            fail "Continuation rejected with EINVAL — bug present."
            note "Raw: $reply"
        else
            ok   "No EINVAL — continuation accepted (a blue square should appear above)."
            note "Raw reply: ${reply:-<none>}"
        fi
    fi
}
