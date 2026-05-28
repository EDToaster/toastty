source "$PROTOCOL_TESTS_DIR/lib.sh"

title="B7 — d=r must delete images with id in [x..y]"

description="Spec (kitty 0.33+): d=r deletes all images whose id is between x and y, inclusive — lowercase x/y carry image ids. Toastty treats d=r as 'clear row at Y=' using the uppercase Y key (term.rs:3313-3318). The real 'delete by row' selector (y/Y) is itself unimplemented."

expected="Three squares with ids 5, 8, 12 are placed in a row. Press 'd' to send d=r,x=4,y=10. Spec: ids 5 and 8 deleted (in range), id 12 survives. Buggy: the command is misinterpreted as 'clear row at Y=0' and nothing visible changes."

run() {
    transmit_solid 5  40 255 0 0          # red,    id 5
    transmit_solid 8  40 0 255 0          # green,  id 8
    transmit_solid 12 40 0 0 255          # blue,   id 12

    cursor_to 12 4
    place_image 5 0
    cursor_to 12 22
    place_image 8 0
    cursor_to 12 40
    place_image 12 0

    cursor_to 16 1
    printf 'Left red = id 5     Middle green = id 8     Right blue = id 12\n'

    cursor_to 18 1
    prompt "Press 'd' to send a=d,d=r,x=4,y=10 (delete ids 4..10)."
    if [[ "$(wait_key)" == "d" ]]; then
        printf '%s_Ga=d,d=r,x=4,y=10,q=2%s\\' "$esc" "$esc"
        cursor_to 19 1
        printf 'Spec: red and green vanish; blue stays. Buggy: nothing changes.\n'
    fi
}
