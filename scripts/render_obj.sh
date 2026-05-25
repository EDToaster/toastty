#!/usr/bin/env bash
# Render an .obj file as a spinning RGP object inside toastty.
#
# Usage:
#   ./scripts/render_obj.sh <path-to.obj>
#
# Env overrides:
#   ROW=4 COL=8 W=20 H=12   placement anchor + cell span

set -eu

if [ $# -lt 1 ]; then
    printf 'usage: %s <file.obj>\n' "$0" >&2
    exit 2
fi

obj_path=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
if [ ! -f "${obj_path}" ]; then
    printf 'file not found: %s\n' "${obj_path}" >&2
    exit 1
fi

ROW="${ROW:-4}"
COL="${COL:-8}"
W="${W:-20}"
H="${H:-12}"

esc=$'\033'
apc="${esc}_"
st="${esc}\\"

rgp() { printf '%s%s%s' "${apc}ratty;g;$1" "${st}"; }

rgp "r;id=1;fmt=obj;path=${obj_path}"
rgp "p;id=1;row=${ROW};col=${COL};w=${W};h=${H};animate=1"
