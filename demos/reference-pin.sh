#!/usr/bin/env bash
# Interactive fixture for Reference Pin. Run inside toastty; no network,
# persistent files, environment changes, or real deployment commands.
set -eu

printf '\033[2J\033[H'
printf '\033[1;36mReference Pin\033[0m  |  synthetic deployment log\n\n'
printf 'Pin these three lines as your working reference:\n\n'
printf '  deployment: api-staging\n'
printf '  request-id: req-8cf2a90\n'
printf '  expected:  HTTP 200\n\n'
printf 'Select the three lines, then press Ctrl+Shift+P (Cmd+Shift+P on macOS).\n'
printf 'Press Enter here to stream new output. The reference stays visible.\n'
read -r _

for step in $(seq 1 45); do
    printf '\033[90m[%02d/45]\033[0m Checking synthetic worker %02d ... \033[32mready\033[0m\n' "$step" "$step"
    sleep 0.06
done
printf '\n\033[1;32mChecks complete.\033[0m The original reference has scrolled away.\n'
printf 'Use Copy on the card, or keep typing while the reference stays in view.\n'
printf 'Dismiss clears it; Escape still belongs to your shell.\n'
