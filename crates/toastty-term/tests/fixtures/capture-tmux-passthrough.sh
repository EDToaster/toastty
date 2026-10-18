#!/bin/sh
# Regenerates the tmux passthrough fixtures used by
# tests/tmux_passthrough.rs. Needs tmux >= 3.3 and util-linux `script`.
#
#   tmux-pane-input.bin    bytes the app wrote into the tmux pane
#                          (`pipe-pane -O`): wrapped `ESC P tmux ; ... ESC \`
#   tmux-outer-output.bin  bytes tmux wrote to its own (outer) terminal,
#                          captured through `script`; tmux unwraps the
#                          passthrough before forwarding it
set -eu
out=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'tmux -L toastty-fixture kill-server 2>/dev/null || true; rm -rf "$work"' EXIT

cat >"$work/app.sh" <<'EOF'
#!/bin/sh
sleep 0.5
# OSC 52 clipboard write (BEL-terminated), OSC 2 title (ST-terminated).
printf '\033Ptmux;\033\033]52;c;aGVsbG8=\007\033\\'
printf '\033Ptmux;\033\033]2;wrapped title\033\033\\\033\\'
# OSC 8 hyperlink around text, all inside one passthrough.
printf '\033Ptmux;\033\033]8;;https://example.com\033\033\\link\033\033]8;;\033\033\\\033\\'
# Kitty graphics transmit of a 1x1 RGBA image, id 42.
printf '\033Ptmux;\033\033_Ga=t,f=32,s=1,v=1,i=42;/wAA/w==\033\033\\\033\\'
printf 'AFTER'
sleep 0.5
EOF
chmod +x "$work/app.sh"

TERM=xterm-256color script -q -e -c "tmux -f /dev/null -L toastty-fixture \
  new-session -x 80 -y 24 $work/app.sh \; \
  set -g allow-passthrough on \; \
  pipe-pane -O 'cat > $work/pane.bin'" "$work/script.log" </dev/null >/dev/null 2>&1

# Strip util-linux script's "Script started/done" header and footer.
python3 - "$work/script.log" "$out/tmux-outer-output.bin" <<'EOF'
import sys
data = open(sys.argv[1], 'rb').read()
if data.startswith(b'Script started'):
    data = data[data.index(b'\n') + 1:]
tail = data.rfind(b'\nScript done')
if tail != -1:
    data = data[:tail]
open(sys.argv[2], 'wb').write(data)
EOF
cp "$work/pane.bin" "$out/tmux-pane-input.bin"
tmux -V
