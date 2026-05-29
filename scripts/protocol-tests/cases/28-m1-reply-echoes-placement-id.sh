source "$PROTOCOL_TESTS_DIR/lib.sh"

title="m1 — OK/error replies echo the placement id (p=)"

description="Spec: when a client supplies a placement id, the terminal echoes it back in the reply, e.g. ESC_Gi=<id>,p=<placement>;OK ESC\\. Toastty's reply encoder previously emitted only i= and I=, dropping p= entirely (reply.rs encode_ok/encode_error)."

expected="Press 's' to send a transmit with i=5,p=3 (q omitted → verbose). The captured reply should contain BOTH i=5 and p=3 before the ;OK. Buggy: reply has i=5 but NO p=3."

run() {
    local payload
    payload=$(python3 -c 'import base64,sys; sys.stdout.write(base64.b64encode(bytes([255,0,0,255])).decode())')

    cursor_to 12 1
    printf 'Command: \\e_Ga=t,i=5,p=3,f=32,s=1,v=1,t=d;<payload>\\e\\\\\n'

    cursor_to 14 1
    prompt "Press 's' to send the i=5,p=3 transmit. Any other key skips."
    if [[ "$(wait_key)" == "s" ]]; then
        local reply
        reply=$(send_and_capture "a=t,i=5,p=3,f=32,s=1,v=1,t=d" "$payload")
        cursor_to 16 1
        if [[ -z "$reply" ]]; then
            fail "No reply captured."
            note "Expected a verbose OK reply echoing i=5 and p=3."
        elif [[ "$reply" == *"p=3"* && "$reply" == *"i=5"* ]]; then
            ok   "Reply echoes both i=5 and p=3 — spec-compliant."
            note "Raw: $reply"
        else
            fail "Reply did NOT echo p=3."
            note "Raw: $reply"
            note "Buggy: placement id dropped from the reply."
        fi
    fi
}
