source "$PROTOCOL_TESTS_DIR/lib.sh"

title="B8 — both i= and I= set must return EINVAL"

description="Spec: 'Specifying both i and I keys in any command is an error. The terminal must reply with an EINVAL error message, unless silenced.' Toastty silently accepts the command (header.rs:289-290, handler.rs:178-215)."

expected="Press 's' to send a 1×1 transmit with i=1 AND I=2. The reply (if any) is captured and printed below. Spec: reply contains EINVAL. Buggy: reply is 'OK' (the transmit succeeded) or empty."

run() {
    # 1×1 RGBA = 4 bytes = "AAAAAP8=" only if we wanted (FF,FF,FF,FF). Use a
    # tiny opaque-black pixel: 00,00,00,FF -> AAAA/wA= would be wrong; use
    # base64 of the 4 bytes 00 00 00 ff.
    local payload
    payload=$(python3 -c 'import base64,sys; sys.stdout.write(base64.b64encode(bytes([0,0,0,255])).decode())')

    cursor_to 12 1
    printf 'Command: \\e_Ga=t,i=1,I=2,f=32,s=1,v=1,t=d;<payload>\\e\\\\\n'

    cursor_to 14 1
    prompt "Press 's' to send the conflicting-keys transmit. Any other key skips."
    if [[ "$(wait_key)" == "s" ]]; then
        local reply
        reply=$(send_and_capture "a=t,i=1,I=2,f=32,s=1,v=1,t=d" "$payload")
        cursor_to 16 1
        if [[ -z "$reply" ]]; then
            fail "No reply captured."
            note "Buggy: when both i= and I= are set, spec demands EINVAL even if silenced isn't applied."
        elif [[ "$reply" == *EINVAL* ]]; then
            ok   "Reply contains EINVAL — spec-compliant."
            note "Raw: $reply"
        else
            fail "Reply did NOT contain EINVAL."
            note "Raw: $reply"
            note "Buggy: command was accepted as a normal transmit."
        fi
    fi
}
