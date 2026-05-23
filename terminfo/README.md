# toastty terminfo

`toastty.terminfo` declares the terminfo capabilities `toastty`
actually implements as of M6. Install it once on every machine you
plan to run `toastty` from:

```sh
tic -x terminfo/toastty.terminfo
```

This writes a compiled entry into `$HOME/.terminfo/t/toastty` (or
the system `terminfo` tree if run as root). Re-run after every pull
that touches this file.

After installation, you can opt in to the `toastty` entry by setting

```sh
export TERM=toastty
```

in your shell rc. Curses-aware apps (vim, tmux, less, htop, fzf, ...)
will pick up the richer capability set automatically.

## Default behavior

Until the entry is installed, `toastty`'s binary spawns child
processes with `TERM=xterm-256color`. That's a near-superset of what
we currently implement (256-color + truecolor + alt screen + DECSCUSR
+ OSC 0/2 title), so the most-popular TUIs behave correctly out of
the box. The exceptions — which `xterm-256color` *advertises* but
`toastty` doesn't yet handle — are mouse reporting and bracketed
paste. Both land in M7.

## What's in the entry

- 256-color (`colors#256`) and truecolor (`Tc`)
- Alt screen (`smcup`/`rmcup` = mode 1049)
- Cursor motion (`cuu1`, `cud1`, `cuf1`, `cub1`, `cup`, `home`, `hpa`,
  `vpa`)
- SGR (`sgr0`, `bold`, `sitm`/`ritm`, `smul`/`rmul`, `rev`, `setaf`,
  `setab`)
- Erase (`ed`, `el`, `el1`, `dch`, `dch1`, `dl`, `dl1`, `ich`, `il`,
  `il1`, `ech`, `clear`)
- Scroll region (`csr`, `ri`, `ind`, `indn`, `rin`)
- Function + arrow + page keys (`kf1..kf12`, `kcuu1..kcub1`,
  `khome`/`kend`, `kpp`/`knp`, `kich1`/`kdch1`)
- Window title (`tsl`/`fsl` via OSC 2 + BEL)
- Cursor shape (`Ss`/`Se` via DECSCUSR)

## What's NOT in the entry (deliberately)

- Mouse (`kmous` / XM extensions) — M7
- Bracketed paste (`BE` / `BD`) — M7
- Focus events — M7
- Kitty keyboard protocol — M7
- Sixel / Kitty graphics — M11
- Hyperlinks (OSC 8) — M10

## Verifying the install

```sh
tic -x terminfo/toastty.terminfo
tput -T toastty colors            # → 256
tput -T toastty Tc; echo OK        # → OK (truecolor advertised)
tput -T toastty bold | xxd | head  # → 1b 5b 31 6d   (ESC [ 1 m)
```

If `tput -T toastty <foo>` errors with "unknown terminal type", `tic`
didn't write to a path that ncurses searches — check `infocmp -D` for
the active search list.
