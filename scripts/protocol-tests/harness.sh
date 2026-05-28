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
    printf '  %s[n]%sext  %s[p]%srev  %s[r]%serun  %s[q]%suit' \
        "$bold" "$reset_sgr" "$bold" "$reset_sgr" \
        "$bold" "$reset_sgr" "$bold" "$reset_sgr"
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

# ---- render one page ----

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

        print_header "$((idx + 1))" "$total" "$title"
        print_section "Description:"
        printf '  %s\n\n' "$description"
        print_section "Test:"
        printf '\n'
        run
        printf '\n'
        print_section "Expected:"
        printf '  %s\n' "$expected"
        print_footer "$((idx + 1))" "$total"
    )
}

cleanup() {
    reset_terminal
}
trap cleanup EXIT

# ---- main loop ----

idx=0
while true; do
    show_case "$idx"
    IFS= read -r -s -n 1 key || key="q"
    case "$key" in
        n|N|' '|'')
            ((idx < total - 1)) && idx=$((idx + 1))
            ;;
        p|P|b|B)
            ((idx > 0)) && idx=$((idx - 1))
            ;;
        r|R)
            : # redraw current page
            ;;
        q|Q)
            exit 0
            ;;
        $'\e')
            # Drop any remaining bytes from an arrow / escape sequence.
            IFS= read -r -s -t 0.05 -n 2 _ || true
            ;;
    esac
done
