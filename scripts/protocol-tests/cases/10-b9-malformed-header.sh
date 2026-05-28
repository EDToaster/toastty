source "$PROTOCOL_TESTS_DIR/lib.sh"

title="B9 — malformed headers must return EINVAL, not silence"

description="Spec: malformed input should reply EINVAL when the client supplied i= or I=. Toastty's term.rs:3130-3134 swallows HandlerError::BadHeader (there's even an existing comment acknowledging the gap)."

expected="Press 's' to send a transmit with an invalid format value (f=999). Spec: reply contains EINVAL. Buggy: no reply at all — the parse error is dropped."

run() {
    cursor_to 12 1
    printf 'Command: \\e_Ga=t,i=1,f=999,s=1,v=1,t=d;<payload>\\e\\\\\n'

    cursor_to 14 1
    prompt "Press 's' to send the bad-format transmit. Any other key skips."
    if [[ "$(wait_key)" == "s" ]]; then
        local reply
        reply=$(send_and_capture "a=t,i=1,f=999,s=1,v=1,t=d" "AAAA")
        cursor_to 16 1
        if [[ -z "$reply" ]]; then
            fail "No reply captured."
            note "Buggy: parse error was swallowed without informing the client."
        elif [[ "$reply" == *EINVAL* ]]; then
            ok   "Reply contains EINVAL — spec-compliant."
            note "Raw: $reply"
        else
            fail "Reply did not contain EINVAL."
            note "Raw: $reply"
        fi
    fi
}
