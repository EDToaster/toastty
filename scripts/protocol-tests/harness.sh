#!/usr/bin/env bash
# Navigable test harness for kitty graphics protocol compliance fixes.
#
# Usage:
#   cargo run --release           # launch toastty
#   # then inside the toastty shell:
#   ./scripts/protocol-tests/harness.sh
#
# Each "page" is one .sh file in ./cases/, sourced as bash. A case file
# defines four things:
#
#   title="Short title shown bold/underlined"
#   description="One or two sentences explaining what this exercises."
#   expected="What you should see when the implementation is correct."
#   run() {
#     # printf escape sequences, read for user keypresses, etc.
#     # No timers — gate state changes on user input.
#   }
#
# The harness sources each case in a subshell, prints the header,
# calls run(), then prints the expected text and a nav bar. Keys:
#
#   n  next       p  prev       r  rerun       q  quit

set -u

PROTOCOL_TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export PROTOCOL_TESTS_DIR
CASES_DIR="${CASES_DIR:-$PROTOCOL_TESTS_DIR/cases}"

esc=$'\033'
bold="${esc}[1m"
underline="${esc}[4m"
dim="${esc}[2m"
reset_sgr="${esc}[0m"
cyan="${esc}[36m"

# ---- terminal reset between pages ----

reset_terminal() {
    # Exit alt screen if a case left us there.
    printf '%s[?1049l' "$esc"
    # Drop all kitty images (placements + bytes). q=2 = no reply.
    printf '%s_Ga=d,d=A,q=2%s\\' "$esc" "$esc"
    # Soft reset: scroll region, modes, SGR.
    printf '%s[!p' "$esc"
    printf '%s[r' "$esc"
    printf '%s[0m' "$esc"
    # Clear scrollback + screen, home cursor.
    printf '%s[3J%s[2J%s[H' "$esc" "$esc" "$esc"
    # Eat any pending kitty replies or stale keystrokes.
    local _drain
    while IFS= read -r -s -t 0.05 -n 1 _drain; do :; done
}

# ---- chrome ----

hr() {
    local glyph="${1:-═}"
    local cols
    cols=$(tput cols 2>/dev/null || echo 80)
    local line
    printf -v line '%*s' "$cols" ''
    printf '%s' "${line// /$glyph}"
}

print_header() {
    local idx="$1" total="$2" title="$3"
    printf '%s%s%s\n' "$cyan" "$(hr ═)" "$reset_sgr"
    printf '  %s%s[%d/%d] %s%s\n' "$bold" "$underline" "$idx" "$total" "$title" "$reset_sgr"
    printf '%s%s%s\n\n' "$cyan" "$(hr ═)" "$reset_sgr"
}

print_section() {
    printf '%s%s%s\n' "$bold" "$1" "$reset_sgr"
}

print_footer() {
    local idx="$1" total="$2"
    printf '\n%s%s%s\n' "$dim" "$(hr ─)" "$reset_sgr"
    printf '  %s[n]%sext  %s[p]%srev  %s[r]%serun  %s[m]%senu  %s[q]%suit' \
        "$bold" "$reset_sgr" "$bold" "$reset_sgr" \
        "$bold" "$reset_sgr" "$bold" "$reset_sgr" \
        "$bold" "$reset_sgr"
    printf '%s[%d/%d]%s\n' "$dim" "$idx" "$total" "$reset_sgr"
}

# ---- discover cases ----

shopt -s nullglob
cases=("$CASES_DIR"/*.sh)
total=${#cases[@]}

if [[ $total -eq 0 ]]; then
    printf 'No test cases found in %s\n' "$CASES_DIR" >&2
    exit 1
fi

# Cache each case's title by sourcing it in a subshell (run() is not
# invoked, so this has no side effects on the terminal).
titles=()
for f in "${cases[@]}"; do
    t=$(
        title="(no title)"
        # shellcheck disable=SC1090
        source "$f" >/dev/null 2>&1
        printf '%s' "$title"
    )
    titles+=("$t")
done

# ---- render one page ----

# Query the cursor's current row via DSR (ESC [ 6 n → ESC [ row;col R).
# Falls back to 12 if the terminal doesn't answer.
query_cursor_row() {
    local resp="" c
    printf '%s[6n' "$esc"
    while IFS= read -r -s -t 0.5 -n 1 c; do
        resp+="$c"
        [[ "$c" == "R" ]] && break
    done
    local re='\[([0-9]+);[0-9]+R'
    if [[ "$resp" =~ $re ]]; then
        printf '%d' "${BASH_REMATCH[1]}"
    else
        printf '12'
    fi
}

show_case() {
    local idx="$1"
    local file="${cases[$idx]}"
    reset_terminal
    (
        # Defaults; the sourced file overrides.
        title="(no title)"
        description="(no description)"
        expected="(no expected blurb)"
        run() { :; }
        # shellcheck disable=SC1090
        source "$file"

        # Header band: title + the blurbs the case is about. Printed first
        # so they're readable before any keypress.
        print_header "$((idx + 1))" "$total" "$title"
        print_section "Description:"
        printf '  %s\n\n' "$description"
        print_section "Expected:"
        printf '  %s\n\n' "$expected"
        print_section "Test:"
        printf '\n'
        # The test canvas starts at the current row. Cases position with
        # cursor_to (relative to HARNESS_CANVAS_TOP), so their output always
        # lands below this header regardless of how far the blurbs wrapped.
        export HARNESS_CANVAS_TOP
        HARNESS_CANVAS_TOP="$(query_cursor_row)"
        run
        # Pin the nav footer to the bottom so case output can't shift it.
        local screen_rows
        screen_rows=$(tput lines 2>/dev/null || echo 40)
        printf '%s[%d;1H' "$esc" "$((screen_rows - 2))"
        print_footer "$((idx + 1))" "$total"
    )
}

cleanup() {
    reset_terminal
}
trap cleanup EXIT

# ---- menu ----

# Render the selection menu and read a choice. Sets the global MENU_CHOICE
# to the chosen 0-based index, or "q" to quit. Re-prompts on invalid input.
# Chrome is printed straight to the terminal (no command substitution, so
# stdout isn't captured).
show_menu() {
    reset_terminal
    printf '%s%s%s\n' "$cyan" "$(hr ═)" "$reset_sgr"
    printf '  %s%skitty graphics protocol — test harness%s\n' "$bold" "$underline" "$reset_sgr"
    printf '%s%s%s\n\n' "$cyan" "$(hr ═)" "$reset_sgr"
    print_section "Select a test:"
    printf '\n'
    local i
    for i in "${!cases[@]}"; do
        printf '  %s%2d%s  %s\n' "$bold" "$((i + 1))" "$reset_sgr" "${titles[$i]}"
    done
    printf '\n%s%s%s\n' "$dim" "$(hr ─)" "$reset_sgr"
    while true; do
        printf '  Enter number %s[1-%d]%s, or %s[q]%suit: ' \
            "$bold" "$total" "$reset_sgr" "$bold" "$reset_sgr"
        local sel
        IFS= read -r sel || { MENU_CHOICE="q"; return; }
        case "$sel" in
            q|Q) MENU_CHOICE="q"; return ;;
            ''|*[!0-9]*) : ;;  # non-numeric → re-prompt
            *)
                if ((sel >= 1 && sel <= total)); then
                    MENU_CHOICE="$((sel - 1))"
                    return
                fi
                ;;
        esac
    done
}

# ---- main loop ----

while true; do
    # Start at the menu; selection sets the active page.
    show_menu
    [[ "$MENU_CHOICE" == "q" ]] && exit 0
    idx="$MENU_CHOICE"

    # Page view for the chosen test; navigate or return to the menu.
    to_menu=0
    while true; do
        show_case "$idx"
        # Inner key loop: ONLY navigation keys break out to trigger a
        # re-render. Stray bytes — leftover DSR/CSI query replies (e.g. the
        # cell-size or cursor-position responses) — are drained and ignored
        # WITHOUT breaking, so they can't cause a re-render that would
        # re-issue those queries and spin forever.
        while true; do
            IFS= read -r -s -n 1 key || key="q"
            case "$key" in
                n|N|' '|'')
                    ((idx < total - 1)) && idx=$((idx + 1))
                    break
                    ;;
                p|P|b|B)
                    ((idx > 0)) && idx=$((idx - 1))
                    break
                    ;;
                r|R)
                    break # redraw current page
                    ;;
                m|M)
                    to_menu=1
                    break
                    ;;
                q|Q)
                    exit 0
                    ;;
                $'\e')
                    # Drain the rest of an escape/CSI/DSR sequence and ignore.
                    while IFS= read -r -s -t 0.05 -n 1 _; do :; done
                    ;;
                *)
                    : # unknown / stray reply byte — ignore, keep reading
                    ;;
            esac
        done
        ((to_menu)) && break
    done
done
