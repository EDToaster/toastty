# toastty shell integration — zsh.
#
# Source this from your `~/.zshrc`:
#
#     [ -n "$TOASTTY" ] && source /path/to/toastty/share/shell-integration/zsh.sh
#
# Provides:
#  - OSC 7   — advertise the current working directory.
#  - OSC 133 — semantic prompt markers (A/B/C/D).
#
# Uses `%{...%}` around escape sequences so zsh prompt column math
# stays correct.

[[ "$TOASTTY" == "1" ]] || return

_toastty_esc=$'\033'

# OSC 7 — current working directory.
_toastty_set_cwd() {
    local host="${HOST:-localhost}"
    # zsh has a built-in URL-encoder via parameter expansion (q-style)
    # but we hand-roll for portability across versions. We feed the raw
    # `$PWD` (not `%~`, which would expand the tilde) so spaces and
    # `%` survive the round-trip to a valid `file://` URL.
    local encoded
    encoded="$(print -nr -- "$PWD" | command awk 'BEGIN{
        for (i=0; i<256; i++) ord[sprintf("%c",i)] = i
    } { s=$0; out="";
        for (i=1; i<=length(s); i++) {
            c=substr(s,i,1);
            if (c ~ /[A-Za-z0-9\/._~-]/) out=out c;
            else out=out sprintf("%%%02X", ord[c]);
        }
        print out
    }')"
    # M10-followup C3: emit the percent-encoded path, not the raw `$PWD`.
    # Without this, a directory like `/tmp/with space` produces
    # `file:///tmp/with space` — invalid; the toastty OSC 7 parser
    # would percent-decode the wrong bytes.
    print -n "${_toastty_esc}]7;file://${host}${encoded}${_toastty_esc}\\"
}

# OSC 133 markers, wrapped for prompt math.
_toastty_prompt_start()  { print -n "%{${_toastty_esc}]133;A${_toastty_esc}\\%}"; }
_toastty_prompt_end()    { print -n "%{${_toastty_esc}]133;B${_toastty_esc}\\%}"; }
_toastty_command_start() { print -n "${_toastty_esc}]133;C${_toastty_esc}\\"; }
_toastty_command_done()  { print -n "${_toastty_esc}]133;D;$1${_toastty_esc}\\"; }

# precmd: OSC 133;D + OSC 7 every time the prompt is about to draw.
_toastty_precmd() {
    local exit_code=$?
    _toastty_command_done "$exit_code"
    _toastty_set_cwd
}
typeset -ga precmd_functions
if (( ! ${precmd_functions[(I)_toastty_precmd]} )); then
    precmd_functions+=_toastty_precmd
fi

# preexec: OSC 133;C right before the user's command runs.
_toastty_preexec() { _toastty_command_start; }
typeset -ga preexec_functions
if (( ! ${preexec_functions[(I)_toastty_preexec]} )); then
    preexec_functions+=_toastty_preexec
fi

# Wrap PS1 with A / B markers. Use `$(...)` so changes to PS1 still get
# wrapped. Idempotent.
if [[ "$PS1" != *"]133;A"* ]]; then
    PS1="$(_toastty_prompt_start)${PS1}$(_toastty_prompt_end)"
fi
