source "$PROTOCOL_TESTS_DIR/lib.sh"

title="M6 — file (t=f) and temp-file (t=t) transmission mediums"

description="Spec: terminals must support transferring pixel data via a file path (t=f), a temp file that is deleted after reading (t=t), and shared memory (t=s). Toastty previously rejected every non-direct medium with ENOTSUP. This case writes a real RGBA image to a temp file and transmits it by PATH (base64-encoded), not inline."

expected="On a fixed build, a solid magenta square appears for t=f (file left intact), and after pressing 't' a solid cyan square appears for t=t with the temp file deleted afterwards. On buggy toastty, nothing renders (ENOTSUP) for either."

# ---------------------------------------------------------------------------
# Inline helpers (path-based mediums — lib.sh only covers inline t=d).
# ---------------------------------------------------------------------------

# Write a solid-color N×N RGBA image to a file. args: path N r g b
write_solid_file() {
    local path="$1" n="$2" r="$3" g="$4" b="$5"
    python3 - "$path" "$n" "$r" "$g" "$b" <<'PYEOF'
import sys
path, n, r, g, b = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
with open(path, "wb") as f:
    f.write(bytes([r, g, b, 255]) * (n * n))
PYEOF
}

# base64-encode a path (the body for a t=f / t=t transmit).
b64_path() {
    python3 -c 'import base64,sys; sys.stdout.write(base64.b64encode(sys.argv[1].encode()).decode())' "$1"
}

# Transmit+place an image stored in a file. args: id N path medium(f|t)
transmit_and_place_file() {
    local id="$1" n="$2" path="$3" medium="$4"
    local payload
    payload="$(b64_path "$path")"
    printf '%s_Ga=T,f=32,s=%d,v=%d,t=%s,i=%d,q=2;%s%s\\' \
        "$esc" "$n" "$n" "$medium" "$id" "$payload" "$esc"
}

run() {
    local tmp_f tmp_t
    tmp_f="$(mktemp "${TMPDIR:-/tmp}/toastty-m6-file-XXXXXX.rgba")"
    tmp_t="$(mktemp "${TMPDIR:-/tmp}/toastty-m6-temp-XXXXXX.rgba")"

    # --- t=f: read file, do NOT delete ---
    write_solid_file "$tmp_f" 48 255 0 255   # magenta
    transmit_and_place_file 1 48 "$tmp_f" f
    printf '\n'
    if [[ -e "$tmp_f" ]]; then
        ok "t=f: source file still present after transmit (correct — t=f never deletes)"
    else
        fail "t=f: source file was deleted (WRONG — only t=t deletes)"
    fi
    note "Above: a magenta square should be visible (t=f file transmission)."
    rm -f "$tmp_f"

    printf '\n'
    prompt "Press 't' to test t=t (temp file, deleted after read). Any other key to skip."
    if [[ "$(wait_key)" == "t" ]]; then
        printf '\n'
        write_solid_file "$tmp_t" 48 0 255 255   # cyan
        transmit_and_place_file 2 48 "$tmp_t" t
        printf '\n'
        if [[ -e "$tmp_t" ]]; then
            fail "t=t: temp file still present (WRONG — t=t must delete after reading)"
        else
            ok "t=t: temp file deleted after read (correct)"
        fi
        note "Above: a cyan square should be visible (t=t temp-file transmission)."
    fi
    rm -f "$tmp_t"
}
