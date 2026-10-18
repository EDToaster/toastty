#!/bin/bash
export HOME=/tmp/home-rec XDG_RUNTIME_DIR=/tmp/run-rec SHELL=/bin/bash PS1='$ ' PATH=/tmp/rec/bin:$PATH
/home/user/.local/bin/herdr server stop >/dev/null 2>&1
exec /home/user/.local/bin/herdr
