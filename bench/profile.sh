#!/usr/bin/env bash
# Profile toastty with the in-process pprof sampler on a private Xvfb
# display. Build first: cargo build --profile profiling --features pprof
#   bench/profile.sh <out.folded> -- <command...>
# BIN overrides the binary; CFG names a config.toml to use.
set -euo pipefail
out=$1; shift; [ "$1" = "--" ] && shift
bin=${BIN:-target/profiling/toastty}
work=$(mktemp -d)
N=99; while [ -e /tmp/.X$N-lock ] || [ -e /tmp/.X11-unix/X$N ]; do N=$((N+1)); done
Xvfb :$N -screen 0 2560x1440x24 -nolisten tcp >"$work/xvfb.log" 2>&1 &
xvfb=$!
trap 'kill $xvfb 2>/dev/null; rm -rf "$work"' EXIT
for i in $(seq 50); do [ -e /tmp/.X11-unix/X$N ] && break; sleep 0.1; done
mkdir -p "$work/cfg/toastty"
if [ -n "${CFG:-}" ]; then cp "$CFG" "$work/cfg/toastty/config.toml"; else "$bin" --print-default-config >"$work/cfg/toastty/config.toml"; fi
env -u WAYLAND_DISPLAY DISPLAY=":$N" MESA_VK_WSI_DEBUG=sw XDG_CONFIG_HOME="$work/cfg" \
    HISTFILE=/dev/null RUST_LOG=warn TOASTTY_STATS="$out.stats.json" TOASTTY_PPROF="$out" \
    "$bin" -- "$@"
