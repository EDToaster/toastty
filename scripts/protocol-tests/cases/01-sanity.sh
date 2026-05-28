source "$PROTOCOL_TESTS_DIR/lib.sh"

title="Sanity — basic image transmit + display"

description="Sends a 32×32 red RGBA square inline via a=T,f=32,t=d. Exercises the direct-transmission + display path that the audit confirmed as already compliant."

expected="A solid red square (~32×32 px) appears below 'Test:'. The cursor lands on a fresh line. No error chatter, no stray escape glyphs."

run() {
    transmit_and_place 1 32 255 0 0
    printf '\n'
}
