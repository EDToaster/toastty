# Agent guide

Rules for coding agents working in this repo. The architecture is in
`docs/architecture.md`, and the design decisions are in `docs/decisions/`.

## Workflow

### Prefer worktrees

Do your work in a git worktree, not in the main checkout. The user and
other agents are often working in `/home/toasty/src/toastty` at the same
time, and a worktree keeps your branch, build and uncommitted changes out
of their way.

```sh
git worktree add .claude/worktrees/<name> -b <branch> main
cd .claude/worktrees/<name>
```

- `.claude/` is gitignored, so worktrees under `.claude/worktrees/` never
  show up in `git status`.
- Each worktree has its own `target/`. Build and screenshot the binary
  from *your* worktree (`./target/release/toastty`), never the one in the
  main checkout. That binary is someone else's build of a different
  commit.
- When the branch is merged or abandoned, remove the worktree:
  `git worktree remove .claude/worktrees/<name>`.

Work directly in the main checkout only when the user asks for it.

### UI and behaviour changes go through a PR

Every change to the UI or to how the program behaves (rendering, input,
escape-sequence handling, config, CLI) goes on a branch and is published
as a PR with `gh pr create`. That holds unless the user explicitly says
not to. Don't push those changes straight to `main`.

The PR description must include **visual evidence** that the change
works. Run the real app on a virtual screen (next section), drive it,
take screenshots, and embed the images in the PR body (see
[Attaching images to a PR](#attaching-images-to-a-pr)). When the change
fixes something visible, show a before (built from `main`) and an after.
Unit and snapshot tests are still required. The screenshots are extra
evidence, not a replacement for tests.

## Running toastty on a virtual screen

You can run the full app off-screen in Xvfb, then send it keyboard and
mouse input and take screenshots. Nothing appears on the user's desktop.

### Safety rules (read first)

The user's desktop is Hyprland (Wayland) with XWayland on display `:0`.
An earlier session broke the user's `:0`: `Xvfb -displayfd` picked `:0`
anyway, unlinked XWayland's socket, and new X11 apps couldn't start. So:

- **Never** use `Xvfb -displayfd` or `xvfb-run` without `-a`. Always pick
  an explicit display number of 99 or higher, and check first that it's
  free (the start snippet below does this).
- **Never** run `xdotool` or `import` without `DISPLAY` set to *your*
  virtual display. With the default `DISPLAY` they act on the user's real
  desktop: they type into the user's windows and screenshot the user's
  screen.
- **Never** `pkill`/`killall` toastty, Xvfb or bash. The user's own
  terminal is probably toastty. Kill only the PIDs you recorded.
- **Isolate state.** Launch toastty with a throwaway config
  (`XDG_CONFIG_HOME`) and a shell that writes no history
  (`HISTFILE=/dev/null`, `bash --norc --noprofile`). Otherwise test
  commands end up in the user's `~/.zsh_history` / `~/.bash_history`.
  Use the user's real shell and config only when that's what you are
  testing, and still set `HISTFILE=/dev/null`.

### Start

Shell state does not carry over between agent tool calls, so write
everything to files in a scratch directory (`$S`, e.g. your session's
scratchpad) and read it back each time.

```sh
S=/path/to/scratch; mkdir -p "$S"

# Virtual display: the first free display number from 99 up.
N=99; while [ -e /tmp/.X$N-lock ] || [ -e /tmp/.X11-unix/X$N ]; do N=$((N+1)); done
Xvfb :$N -screen 0 1280x800x24 -nolisten tcp >"$S/xvfb.log" 2>&1 &
echo $! >"$S/xvfb.pid"; echo ":$N" >"$S/display"
for i in $(seq 50); do [ -e /tmp/.X11-unix/X$N ] && break; sleep 0.1; done

# Throwaway default config.
mkdir -p "$S/cfg/toastty"
./target/release/toastty --print-default-config >"$S/cfg/toastty/config.toml"

# toastty running a clean bash.
env -u WAYLAND_DISPLAY DISPLAY=":$N" MESA_VK_WSI_DEBUG=sw \
    XDG_CONFIG_HOME="$S/cfg" HISTFILE=/dev/null \
    ./target/release/toastty -- bash --norc --noprofile >"$S/toastty.log" 2>&1 &
echo $! >"$S/toastty.pid"

export DISPLAY=":$N"
W=$(xdotool search --sync --class toastty | head -1); echo "$W" >"$S/window"
xdotool windowfocus --sync "$W"
```

Why each piece is there:

- `env -u WAYLAND_DISPLAY` makes winit use X11 (the Xvfb display)
  instead of the user's compositor.
- `MESA_VK_WSI_DEBUG=sw` is required. Xvfb has no DRI3, so without it
  RADV can't present and toastty exits with `renderer init failed: no
  compatible GPU adapter found`. Rendering still runs on the real GPU;
  only presentation goes through software.
- There's no window manager, so use `xdotool windowfocus`.
  `windowactivate` needs a WM and fails.
- `toastty -- <cmd>` runs any program in place of the shell, e.g.
  `toastty -- vim file`. Run `toastty --help` for the other flags.
- Debug aids: `RUST_LOG=debug`, `TOASTTY_PTY_LOG=<path>` (logs the PTY
  byte stream), `TOASTTY_DEBUG=1` (FPS overlay). Read `$S/toastty.log`
  when something looks wrong.

In later calls, restore with `export DISPLAY=$(cat "$S/display");
W=$(cat "$S/window")`.

### Commands you can send

All of these were checked against toastty on Xvfb. `xdotool` commands
can be chained in one call (`xdotool mousemove 10 10 click 1 sleep 0.2
key Return`). Leave a short `sleep` (0.3–1 s) before a screenshot so the
frame is presented.

**Keyboard**

| Action | Command |
|---|---|
| Type text | `xdotool type --delay 15 'echo hi'` |
| Named key | `xdotool key Return` (also `Escape`, `Tab`, `BackSpace`, `Up`, `Page_Up`, `Home`, `F5`, …) |
| Chord | `xdotool key ctrl+c`, `ctrl+shift+c`, `alt+x`, `shift+Up` |
| Hold / release a modifier | `xdotool keydown shift key Page_Up keyup shift` |
| Several keys | `xdotool key Escape colon q exclam Return` (or `type ':q!'` then `key Return`) |

`type` types characters, so a newline in the string doesn't press Enter.
Send `key Return` separately.

**Mouse** (coordinates are pixels in the window; the window sits at 0,0)

| Action | Command |
|---|---|
| Move | `xdotool mousemove 200 300` |
| Left / middle / right click | `xdotool click 1` / `click 2` / `click 3` |
| Double / triple click (word / line select) | `xdotool click --repeat 2 --delay 80 1` (`--repeat 3` for a line) |
| Wheel up / down | `xdotool click 4` / `click 5`; scroll a lot with `click --repeat 15 --delay 30 4` |
| Horizontal wheel | `xdotool click 6` / `click 7` (one button per `click`; not yet checked against toastty) |
| Drag (select) | `xdotool mousemove 20 100 mousedown 1 mousemove 400 300 mouseup 1` |
| Shift-click etc. | `xdotool keydown shift click 1 keyup shift` |

Cells are about 10 px wide and 19 px tall with the default config (rows
are centred at y ≈ 9, 28, 47, …). Don't guess where text is. Take a
screenshot first, find the text, then click it. Remember that `clear`
moves everything up to row 0.

**Window**

| Action | Command |
|---|---|
| Resize (triggers reflow + SIGWINCH) | `xdotool windowsize "$W" 800 400` |
| Query size | `xdotool getwindowgeometry "$W"` |
| Focus | `xdotool windowfocus --sync "$W"` |

Check a resize from inside the shell with `stty size`.

**Clipboard.** Copy (`ctrl+shift+c`), paste (`ctrl+shift+v`) and
PRIMARY middle-click paste (`click 2`) all work within the virtual
display. They never touch the user's real clipboard. `xclip`/`xsel` are
not installed, so check a paste by pasting it into the shell
(`echo pasted: ` + `ctrl+shift+v`).

**Seeing what the app receives.** To check the exact bytes toastty sends
for keys or mouse events, run `cat -v` and look at the echo. In the
user's shell, `cat` is an alias for `bat`, so use `command cat -v`.
Enable mouse reporting first, e.g.
`printf '\e[?1000;1006h'; command cat -v; printf '\e[?1000;1006l'`. A
click then shows up as `^[[<0;20;17M` (press) / `m` (release).

**Screenshots**

| Action | Command |
|---|---|
| Whole virtual screen | `import -window root "$S/shot.png"` |
| Just the toastty window | `import -window "$W" "$S/shot.png"` |
| Crop a region (WxH+X+Y) | `import -window root -crop 400x120+0+0 "$S/shot.png"` |
| Stack images vertically / side by side | `magick a.png b.png -append out.png` / `+append` |

Check every screenshot (open the PNG) before you rely on it.

### Limitations

- This exercises toastty's **X11** path only. It doesn't cover Wayland
  input, the Wayland clipboard, IME or fractional scaling.
- There's no compositor, so the transparent background shows up opaque.
- The scale factor is 1, so HiDPI rendering isn't covered.

### Stop and clean up (always)

Clean up before you finish, even if something failed partway. Kill only
the PIDs you recorded:

```sh
kill $(cat "$S/toastty.pid") 2>/dev/null; sleep 0.5
kill $(cat "$S/xvfb.pid") 2>/dev/null; sleep 0.5
ps -p "$(cat "$S/toastty.pid"),$(cat "$S/xvfb.pid")" -o pid=,cmd=   # expect no output
ls /tmp/.X11-unix/                                                  # your X<N> socket is gone
```

The shell inside toastty exits when toastty does. If you launched extra
long-running programs some other way, kill them too. Report anything you
couldn't clean up to the user.

## Attaching images to a PR

`gh` can't upload images into a PR body, so screenshots go on the
orphan branch `pr-assets`, which holds only images. The PR then embeds
them by commit-pinned raw URL. This keeps the PNGs out of `main`'s
history. Use a worktree for it, so your branch's checkout isn't touched:

```sh
A=.claude/worktrees/pr-assets
if git ls-remote --exit-code --heads origin pr-assets >/dev/null; then
  git fetch origin pr-assets && git worktree add "$A" -B pr-assets origin/pr-assets
else
  git worktree add --orphan -b pr-assets "$A"
fi
mkdir -p "$A/<your-branch>" && cp "$S"/*.png "$A/<your-branch>/"
git -C "$A" add <your-branch> && git -C "$A" commit -m "assets: <your-branch> screenshots"
git -C "$A" push origin pr-assets
SHA=$(git -C "$A" rev-parse HEAD)
git worktree remove "$A"
```

Embed each image in the PR body (`gh pr create --body-file …` or
`gh pr edit <n> --body-file …`):

```markdown
![after: selection spans the wrapped line](https://raw.githubusercontent.com/EDToaster/toastty/<SHA>/<your-branch>/after.png)
```

Give each image a caption saying what it shows and what to look for.
After posting, run `gh pr view <n> --web` or fetch one image URL and
check that it returns 200.
