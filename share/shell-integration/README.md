# toastty shell integration

Shell snippets that wire the OS escape sequences toastty consumes — OSC 7
(cwd) and OSC 133 (semantic prompt markers) — into bash, zsh, and fish.

Sourcing one of these inside a toastty session gives you:

- **Per-prompt cwd advertising** (OSC 7). Future toastty UI pieces can use
  the live cwd (e.g. status line, "open new tab at this directory").
- **Command boundaries** (OSC 133;A/B/C/D). Lets toastty distinguish
  prompts from command output from command-finished moments — required for
  command-level navigation ("jump to previous prompt") and for showing the
  exit code of the last command in the chrome.

## Activating

Each snippet checks the `TOASTTY` environment variable before doing
anything. toastty's PTY spawn sets `TOASTTY=1` so the integration is
inert under other terminals.

### bash

Add to your `~/.bashrc`:

```bash
[ -n "$TOASTTY" ] && source /path/to/toastty/share/shell-integration/bash.sh
```

### zsh

Add to your `~/.zshrc`:

```zsh
[[ -n "$TOASTTY" ]] && source /path/to/toastty/share/shell-integration/zsh.sh
```

### fish

Add to your `~/.config/fish/config.fish`:

```fish
if test "$TOASTTY" = "1"
    source /path/to/toastty/share/shell-integration/fish.fish
end
```

## What each snippet does

| OSC | Sent when | Encoded as |
|-----|-----------|-----------|
| 7   | New prompt | `ESC ] 7 ; file://<host>/<percent-encoded-pwd> ESC \` |
| 133;A | Prompt start | wrapped in readline-safe brackets so `PS1` column math is correct |
| 133;B | End of prompt / start of typed command | same wrapping as `;A` |
| 133;C | Command begins executing | bare sequence — no prompt math to preserve |
| 133;D;`$?` | Command finished | exit code included |

## Caveats

- The bash snippet uses the `DEBUG` trap. If your `~/.bashrc` already sets
  a `DEBUG` trap for something else, source toastty's snippet last so the
  outer trap wins.
- fish 2.x doesn't have `string escape --style=url`. Upgrade to 3+ or
  drop in your own URL-encoder.
- These snippets emit ASCII only; non-UTF-8 paths get percent-encoded
  byte by byte (RFC 3986).

## Manual verification

Sourcing a snippet must never kill the host shell. Verify by hand:

```fish
# In a non-toastty fish session (TOASTTY unset / != "1"):
source /path/to/toastty/share/shell-integration/fish.fish
# Shell must still be alive — the snippet `return`s out of itself,
# rather than `exit`ing the session.
```

Repeat the same for bash and zsh with `unset TOASTTY` first. The shell
prompt should reappear; no automated CI coverage because the CI image
doesn't bundle all three shells.
