# toastty shell integration — fish.
#
# Source this from your `~/.config/fish/config.fish`:
#
#     if test "$TOASTTY" = "1"
#         source /path/to/toastty/share/shell-integration/fish.fish
#     end
#
# Provides:
#  - OSC 7   — advertise the current working directory.
#  - OSC 133 — semantic prompt markers (A/B/C/D).

if test "$TOASTTY" != "1"
    exit
end

# OSC 7 — emitted on every directory change.
function __toastty_set_cwd --on-variable PWD
    # `string escape --style=url` is the canonical RFC-3986 path
    # encoder in modern fish (>= 3.0).
    set -l host (hostname 2>/dev/null; or echo localhost)
    printf '\e]7;file://%s%s\e\\' $host (string escape --style=url -- $PWD)
end
# Emit once on startup so the very first prompt advertises the cwd.
__toastty_set_cwd

# OSC 133 markers.
function __toastty_prompt_start  ; printf '\e]133;A\e\\'         ; end
function __toastty_prompt_end    ; printf '\e]133;B\e\\'         ; end
function __toastty_command_start ; printf '\e]133;C\e\\'         ; end
function __toastty_command_done  ; printf '\e]133;D;%s\e\\' $argv[1] ; end

# Hook into fish's prompt + preexec/postexec events.
function __toastty_preexec --on-event fish_preexec
    __toastty_command_start
end

function __toastty_postexec --on-event fish_postexec
    __toastty_command_done $status
end

# Wrap `fish_prompt` so it emits OSC 133;A at the very start and
# OSC 133;B right at the end of the prompt string.
functions -c fish_prompt __toastty_orig_prompt 2>/dev/null
function fish_prompt
    __toastty_prompt_start
    __toastty_orig_prompt
    __toastty_prompt_end
end
