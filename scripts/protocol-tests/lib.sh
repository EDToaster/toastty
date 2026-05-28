# Shared helpers for protocol-test cases.
# Cases source this via:  source "$PROTOCOL_TESTS_DIR/lib.sh"

esc=$'\033'

# ---------------------------------------------------------------------------
# Chrome
# ---------------------------------------------------------------------------

prompt() {
    printf '\n%s[2m▶ %s%s[0m\n' "$esc" "$1" "$esc"
}

ok()   { printf '%s[32m✓ %s%s[0m\n' "$esc" "$1" "$esc"; }
fail() { printf '%s[31m✗ %s%s[0m\n' "$esc" "$1" "$esc"; }
note() { printf '%s[2m  %s%s[0m\n' "$esc" "$1" "$esc"; }

# ---------------------------------------------------------------------------
# Cursor
# ---------------------------------------------------------------------------

cursor_to()       { printf '%s[%d;%dH' "$esc" "$1" "$2"; }
cursor_save()     { printf '%s7' "$esc"; }
cursor_restore()  { printf '%s8' "$esc"; }

# ---------------------------------------------------------------------------
# Keypress input
# ---------------------------------------------------------------------------

# Block for one key; print it to stdout.
wait_key() {
    local _k
    IFS= read -r -s -n 1 _k
    printf '%s' "$_k"
}

# Block until SPACE is pressed.
wait_space() {
    while :; do
        local k
        IFS= read -r -s -n 1 k
        [[ "$k" == ' ' ]] && return
    done
}

# Block until one of the given letter keys is pressed; print it.
# Usage: wait_one_of "cqx"
wait_one_of() {
    local choices="$1"
    while :; do
        local k
        IFS= read -r -s -n 1 k
        case "$choices" in *"$k"*) printf '%s' "$k"; return ;; esac
    done
}

# Drain any pending input (replies + stale keystrokes).
drain_input() {
    local _d
    while IFS= read -r -s -t 0.05 -n 1 _d; do :; done
}

# ---------------------------------------------------------------------------
# Kitty graphics — image transmission helpers (all silenced via q=2)
# ---------------------------------------------------------------------------

# Transmit a solid-color RGBA image. Does NOT display it.
# args: id N r g b [extra_keys]
transmit_solid() {
    local id="$1" n="$2" r="$3" g="$4" b="$5"; shift 5
    local extra="${1:-}"
    local payload
    payload=$(python3 <<PYEOF
import base64, sys
sys.stdout.write(base64.b64encode(bytes([$r, $g, $b, 255]) * ($n * $n)).decode())
PYEOF
)
    local keys="a=t,f=32,s=$n,v=$n,t=d,i=$id,q=2"
    [[ -n "$extra" ]] && keys+=",$extra"
    printf '%s_G%s;%s%s\\' "$esc" "$keys" "$payload" "$esc"
}

# Transmit + display at current cursor (a=T).
# args: id N r g b [extra_keys]
transmit_and_place() {
    local id="$1" n="$2" r="$3" g="$4" b="$5"; shift 5
    local extra="${1:-}"
    local payload
    payload=$(python3 <<PYEOF
import base64, sys
sys.stdout.write(base64.b64encode(bytes([$r, $g, $b, 255]) * ($n * $n)).decode())
PYEOF
)
    local keys="a=T,f=32,s=$n,v=$n,t=d,i=$id,q=2"
    [[ -n "$extra" ]] && keys+=",$extra"
    printf '%s_G%s;%s%s\\' "$esc" "$keys" "$payload" "$esc"
}

# Place a previously-transmitted image at current cursor.
# args: id placement_id [extra_keys]
place_image() {
    local id="$1" pid="$2"; shift 2
    local extra="${1:-}"
    local keys="a=p,i=$id,p=$pid,q=2"
    [[ -n "$extra" ]] && keys+=",$extra"
    printf '%s_G%s;%s\\' "$esc" "$keys" "$esc"
}

# Transmit a W×H image with left half red and right half green. Useful for
# placement-coordinate tests where we need to tell columns apart visually.
# args: id W H [extra_keys]
transmit_split_rg() {
    local id="$1" w="$2" h="$3"; shift 3
    local extra="${1:-}"
    local payload
    payload=$(python3 <<PYEOF
import base64, sys
w, h = $w, $h
half = w // 2
data = bytearray()
for y in range(h):
    for x in range(w):
        if x < half:
            data += bytes([255, 0, 0, 255])
        else:
            data += bytes([0, 255, 0, 255])
sys.stdout.write(base64.b64encode(bytes(data)).decode())
PYEOF
)
    local keys="a=t,f=32,s=$w,v=$h,t=d,i=$id,q=2"
    [[ -n "$extra" ]] && keys+=",$extra"
    printf '%s_G%s;%s%s\\' "$esc" "$keys" "$payload" "$esc"
}

# ---------------------------------------------------------------------------
# Reply capture (for tests that need to inspect the terminal's APC response)
# ---------------------------------------------------------------------------

# Read bytes from stdin until ~0.2s of silence. Print as escape-safe text.
capture_reply() {
    local first_timeout="${1:-1}"
    local reply="" c
    if IFS= read -r -s -t "$first_timeout" -n 1 c; then
        reply+="$c"
        while IFS= read -r -s -t 0.2 -n 1 c; do
            reply+="$c"
        done
    fi
    # Render ESC as \e for human-readable output.
    local out="" i len="${#reply}"
    for (( i=0; i<len; i++ )); do
        local ch="${reply:i:1}"
        if [[ "$ch" == $'\033' ]]; then out+='\e'; else out+="$ch"; fi
    done
    printf '%s' "$out"
}

# Send a kitty APC command (caller-controlled keys + payload) and return the
# reply string. Drains stdin first to avoid mixing with prior replies.
# args: keys [payload]
send_and_capture() {
    local keys="$1" payload="${2:-}"
    drain_input
    printf '%s_G%s;%s%s\\' "$esc" "$keys" "$payload" "$esc"
    capture_reply 1
}

# ---------------------------------------------------------------------------
# Cell pixel size (CSI 16t)
# ---------------------------------------------------------------------------

# Query the terminal's cell pixel size and set globals CELL_PW / CELL_PH.
# Reply format: ESC [ 6 ; <height> ; <width> t. Falls back to 10x20.
query_cell_px() {
    CELL_PW=10
    CELL_PH=20
    drain_input
    printf '%s[16t' "$esc"
    local resp="" c
    while IFS= read -r -s -t 1 -n 1 c; do
        resp+="$c"
        [[ "$c" == "t" ]] && break
    done
    local re='\[6;([0-9]+);([0-9]+)t'
    if [[ "$resp" =~ $re ]]; then
        CELL_PH="${BASH_REMATCH[1]}"
        CELL_PW="${BASH_REMATCH[2]}"
    fi
}

# ---------------------------------------------------------------------------
# Unicode placeholders
# ---------------------------------------------------------------------------

# Emit a U+10EEEE placeholder cell carrying (row, col, [id_msb]) diacritics.
# args: row col [msb]
placeholder_cell() {
    local row="$1" col="$2" msb="${3:-}"
    # Pass values via argv (and a quoted heredoc) so an empty msb can't
    # interpolate into `diacritics[]` and produce a Python syntax error.
    python3 - "$row" "$col" "$msb" <<'PYEOF'
import sys
diacritics = [
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A,
    0x034B, 0x034C, 0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365,
]
row, col, msb = sys.argv[1], sys.argv[2], sys.argv[3]
s = chr(0x10EEEE) + chr(diacritics[int(row)]) + chr(diacritics[int(col)])
if msb != "":
    s += chr(diacritics[int(msb)])
sys.stdout.write(s)
PYEOF
}

# Emit a U+10EEEE placeholder cell with NO diacritics (used to exercise
# left-neighbor inheritance per B11).
placeholder_bare() {
    python3 -c 'import sys; sys.stdout.write(chr(0x10EEEE))'
}
