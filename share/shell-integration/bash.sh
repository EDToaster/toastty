# toastty shell integration — bash.
#
# Source this file from your `~/.bashrc`:
#
#     [ -n "$TOASTTY" ] && source /path/to/toastty/share/shell-integration/bash.sh
#
# Provides:
#  - OSC 7  — advertise the current working directory.
#  - OSC 133 — semantic prompt markers (A/B/C/D).
#
# Readline-safe: every escape sequence is wrapped in `\[ ... \]` so PS1
# column accounting stays correct.

# Only enable inside toastty.
if [ "${TOASTTY:-}" != "1" ]; then
    return
fi

# Lazily decide if printf supports \e (some old bashes do not).
_toastty_esc=$'\033'

# OSC 7 — current working directory.
_toastty_set_cwd() {
    # %xx-encode the bytes of $PWD per RFC 3986. bash printf doesn't have
    # a built-in percent-encoder; we hand-roll one over the bytes.
    local LC_ALL=C path="$PWD" encoded= i ch
    for (( i = 0; i < ${#path}; i++ )); do
        ch="${path:i:1}"
        case "$ch" in
            [a-zA-Z0-9/._~-]) encoded+="$ch" ;;
            *) encoded+=$(printf '%%%02X' "'$ch") ;;
        esac
    done
    printf '%sOSC 7 unused\r' '' >/dev/null  # noop to satisfy older bashes
    printf '%s]7;file://%s%s%s\\' "$_toastty_esc" "${HOSTNAME:-localhost}" "$encoded" "$_toastty_esc"
}

# OSC 133 markers.
_toastty_prompt_start()   { printf '\[%s]133;A%s\\\]' "$_toastty_esc" "$_toastty_esc"; }
_toastty_prompt_end()     { printf '\[%s]133;B%s\\\]' "$_toastty_esc" "$_toastty_esc"; }
_toastty_command_start()  { printf '%s]133;C%s\\' "$_toastty_esc" "$_toastty_esc"; }
_toastty_command_done()   { printf '%s]133;D;%s%s\\' "$_toastty_esc" "$1" "$_toastty_esc"; }

# Hook into PROMPT_COMMAND so we re-emit OSC 7 and OSC 133;D on every
# prompt redraw. The exit code from the last command is captured first
# so subsequent commands don't clobber `$?`.
_toastty_precmd() {
    local exit_code=$?
    _toastty_command_done "$exit_code"
    _toastty_set_cwd
}
# Prepend so user-supplied PROMPT_COMMAND content still runs.
case ";${PROMPT_COMMAND-};" in
    *";_toastty_precmd;"*) ;;
    *) PROMPT_COMMAND="_toastty_precmd${PROMPT_COMMAND:+; $PROMPT_COMMAND}" ;;
esac

# Inject A (prompt-start) and B (prompt-end / command-start) markers
# around the existing PS1. Readline-safe brackets keep column math
# correct.
PS1="$(_toastty_prompt_start)${PS1}$(_toastty_prompt_end)"

# Trap DEBUG so we can emit OSC 133;C right before the user's command
# actually executes. Skip the trap itself.
trap '[[ "$BASH_COMMAND" != _toastty_precmd ]] && _toastty_command_start' DEBUG
