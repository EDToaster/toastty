#!/bin/bash
# Run reference kitty on a private Xvfb display, for side-by-side comparison
# with toastty (see docs/kitty-graphics-checklist.md).
#
# Usage: kitty-ref.sh <scratchdir> [first-display-number]
#
# Follows the safety rules in AGENTS.md: picks the first free display >= 120
# (never :0), records PIDs in <scratchdir>, and runs a clean bash with no
# history. Stop it with:
#   kill "$(cat <scratchdir>/kitty.pid)"; kill "$(cat <scratchdir>/kxvfb.pid)"
S="${1:?scratch dir}"; N="${2:-120}"; mkdir -p "$S"
while [ -e /tmp/.X$N-lock ] || [ -e /tmp/.X11-unix/X$N ]; do N=$((N+1)); done
Xvfb :$N -screen 0 1280x800x24 -nolisten tcp >"$S/kxvfb.log" 2>&1 &
echo $! >"$S/kxvfb.pid"; echo ":$N" >"$S/kdisplay"
for i in $(seq 50); do [ -e /tmp/.X11-unix/X$N ] && break; sleep 0.1; done
mkdir -p "$S/kcfg"
cat > "$S/kcfg/kitty.conf" <<'EOF'
font_size 11
remember_window_size no
initial_window_width 1000
initial_window_height 600
confirm_os_window_close 0
shell_integration disabled
window_padding_width 0
EOF
# Xvfb has no GPU: kitty needs software GL.
env -u WAYLAND_DISPLAY DISPLAY=":$N" LIBGL_ALWAYS_SOFTWARE=1 \
  KITTY_CONFIG_DIRECTORY="$S/kcfg" HISTFILE=/dev/null PS1='$ ' \
  kitty --class kittyref -o linux_display_server=x11 -- bash --norc --noprofile >"$S/kitty.log" 2>&1 &
echo $! >"$S/kitty.pid"
W=$(DISPLAY=":$N" timeout 20 xdotool search --sync --class kittyref | head -1); echo "$W" >"$S/kwindow"
DISPLAY=":$N" xdotool windowfocus --sync "$W"
echo "display=:$N window=$W kitty_pid=$(cat "$S/kitty.pid") xvfb_pid=$(cat "$S/kxvfb.pid")"
