#!/usr/bin/env bash
# Render an .obj or .glb file as a spinning RGP object inside toastty.
#
# Usage:
#   ./scripts/render_obj.sh <path-to.obj|.glb>
#
# Env overrides:
#   ROW=4 COL=8 W=20 H=12   placement anchor + cell span
#   FMT=obj|glb             override format autodetection

set -eu

if [ $# -lt 1 ]; then
    printf 'usage: %s <file.obj|file.glb>\n' "$0" >&2
    exit 2
fi

model_path=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
if [ ! -f "${model_path}" ]; then
    printf 'file not found: %s\n' "${model_path}" >&2
    exit 1
fi

if [ -z "${FMT:-}" ]; then
    case "${model_path##*.}" in
        obj|OBJ) FMT=obj ;;
        glb|GLB) FMT=glb ;;
        *)
            printf 'cannot infer format from extension; set FMT=obj or FMT=glb\n' >&2
            exit 1
            ;;
    esac
fi

ROW="${ROW:-4}"
COL="${COL:-8}"
W="${W:-20}"
H="${H:-12}"

esc=$'\033'
apc="${esc}_"
st="${esc}\\"

rgp() { printf '%s%s%s' "${apc}ratty;g;$1" "${st}"; }

rgp "r;id=1;fmt=${FMT};path=${model_path}"
rgp "p;id=1;row=${ROW};col=${COL};w=${W};h=${H};animate=1"
