# Compatibility and feature checklist

An audit of toastty (main @ `9659f67`, October 2026) against xterm `ctlseqs` and the
features users expect from a modern terminal emulator (Ghostty, kitty, WezTerm,
foot, iTerm2). Each unchecked item is a known gap; tick it off in the PR that
fixes it and link the PR.

How this was checked:

- **Terminal core:** about 60 escape sequences were fed through `Parser` → `Term`
  in a throwaway integration test. The resulting grid and the PTY replies were
  compared with xterm's documented behaviour.
- **Live runs:** problems visible to users were then confirmed in a real toastty
  window under Xvfb, using `printf`, Python `curses` and `tmux`.

Legend: **P0** breaks common apps, **P1** wrong or missing standard behaviour that
apps notice, **P2** modern feature or nicety.

## 1. VT core conformance

### Character sets

- [ ] **P0 — DEC Special Graphics (line drawing).**
  - Nothing is implemented: `ESC ( 0`, `ESC ) 0`, SO/SI (G0/G1 switching), or `ESC ( B` back to ASCII.
  - Under the default `TERM=xterm-256color`, ncurses draws boxes with `smacs`/`rmacs` (`acsc`). Every `box()` and `ACS_*` character therefore renders as `lqqqk` / `x` / `mqqqj`.
  - Reproduce with `printf '\e(0lqqk\e(B\n'`, or a Python `curses` script that calls `stdscr.box()`.
  - Affects dialog, whiptail, mc, alsamixer and anything using ncurses borders.
  - Fix: track G0–G3 designations plus GL shift state in `Term`, and map `0x5f–0x7e` through the VT100 graphics table in `print`. Include SS2/SS3 if cheap.

### CSI sequences currently ignored

Test input → expected result (xterm):

- [ ] **P1 — REP `CSI Ps b`** (repeat the preceding graphic character). `a\e[3b` → `aaaa`. `docs/protocols.md` lists it as supported.
- [ ] **P1 — CNL `CSI Ps E` / CPL `CSI Ps F`** (cursor to column 1 of the next or previous line).
- [ ] **P1 — HPA ``CSI Ps ` ``, HPR `CSI Ps a`, VPR `CSI Ps e`** (absolute/relative horizontal moves and relative vertical moves). CHA (`G`) and VPA (`d`) already work.
- [ ] **P1 — Tab stops.**
  - Missing: HTS `ESC H`, TBC `CSI 0 g` / `CSI 3 g`, CHT `CSI Ps I`, CBT `CSI Ps Z`.
  - Tabs are currently fixed at every 8 columns.
  - Tab stops must also reset on RIS/DECSTR and resize.
- [ ] **P1 — SCOSC / SCORC `CSI s` / `CSI u`** (save/restore cursor, no private marker). `ESC 7` / `ESC 8` work. `CSI s` must stay unambiguous with DECSLRM once left/right margins exist: it means SCOSC only when DECLRMM (`?69`) is off.
- [ ] **P1 — IRM `CSI 4 h` / `CSI 4 l`** (insert mode). `abc\r\e[4hX` → `Xabc`.
- [ ] **P2 — ANSI-mode DECRQM `CSI Ps $ p`** (no `?`). There's no reply today.
- [ ] **P2 — DECALN `ESC # 8`** (fill the screen with `E`; used by vttest).
- [ ] **P2 — DA3 `CSI = c`.** No reply today.
- [ ] **P2 — DECSLRM / DECLRMM `?69`.** Already listed as Tier 4 in `protocols.md`.

### DEC private modes (`apply_decset`, see `TODO(modes): 1, 7, 12`)

- [ ] **P1 — DECCKM `?1`** (application cursor keys).
  - Arrow, Home and End keys must send `SS3 A` etc. while set.
  - `TERM=xterm-256color` advertises `kcuu1=\EOA` and `smkx=\E[?1h\E=`, so apps expect this.
  - DECRQM `?1` currently answers `0` (not recognised).
- [ ] **P1 — DECKPAM / DECKPNM `ESC =` / `ESC >`** (application keypad).
- [ ] **P1 — DECAWM `?7`** (autowrap).
  - `\e[?7l` + long line should overwrite the last column; today it wraps.
  - DECRQM `?7` answers `0`.
- [ ] **P1 — DECOM `?6`** (origin mode). CUP should be relative to the scroll region while set.
- [ ] **P2 — `?5` DECSCNM** (reverse video), **`?12`** (cursor blink, att610), **`?1007`** (alternate scroll), **`?1036` / `?1039`** (meta/alt sends escape).

### Replies and queries

- [ ] **P1 — XTGETTCAP `DCS + q <hex> ST`.**
  - There's no reply today.
  - Neovim queries `Tc` / `RGB` / `setrgbf` to enable truecolor, and `TN` / `Ms` and others.
  - Answer the capabilities toastty implements; reply `DCS 0 + r ST` for unknown ones.
- [ ] **P1 — DECRQSS `DCS $ q <Pt> ST`.**
  - There's no reply today.
  - Needed at least for `m` (SGR; apps probe truecolor by setting a colour and reading it back), `r` (DECSTBM) and ` q` (DECSCUSR).
- [ ] **P1 — OSC 10 / 12 queries return hardcoded colours** (`d8d8d8`, `f2d94d`), not the active theme's fg and cursor. OSC 11 is correct.
- [ ] **P1 — OSC 10 / 11 / 12 set is accepted but ignored.** Also add the resets OSC 104 / 110 / 111 / 112.
- [ ] **P1 — Title stack `CSI 22 t` / `CSI 23 t`** does nothing, although `protocols.md` says it's supported.
- [ ] **P2 — OSC 21** (kitty colour protocol), listed as Tier 2.
- [ ] **P2 — XTQMODKEYS `CSI ? Pp m`.** No reply today.

### Bell

- [ ] **P1 — BEL (0x07) is dropped.**
  - There's no audible bell, visual bell or urgency hint.
  - Add a `[bell]` config (visual flash / window urgency via winit `request_user_attention` / none).
  - Make sure BEL used as an OSC terminator stays unaffected (it already is).

## 2. Truecolor signalling

- [ ] **P0 — No truecolor signal to apps.**
  - The child environment doesn't set `COLORTERM=truecolor`, and the XTGETTCAP / DECRQSS probes above go unanswered.
  - Neovim (auto `termguicolors`), bat, delta, helix, micro, ripgrep's `--colors` and others may fall back to 256 colours even though toastty renders 24-bit SGR fine.
  - Fix: set `COLORTERM=truecolor`, answer XTGETTCAP `Tc` / `RGB`, and answer DECRQSS `m`.

## 3. SGR rendition

`Style` only stores bold, italic, single underline and reverse. Everything else is
parsed and dropped (screenshot check: `printf '\e[2mdim \e[9mstrike \e[4:3mcurly\e[0m'`).

- [ ] **P1 — Dim / faint (SGR 2, cleared by 22).**
- [ ] **P1 — Strikethrough (SGR 9 / 29).**
- [ ] **P1 — Underline styles `4:0`–`4:5`** (none, single, double, curly, dotted, dashed) and **SGR 21** (double underline). Neovim and helix use curly underlines for diagnostics. Today `4:3` renders as a plain underline.
- [ ] **P1 — Underline colour SGR 58 / 59.** It's parsed into `cursor_underline_color` only for the kitty Unicode-placeholder image-id trick, and never stored per cell or rendered.
- [ ] **P2 — Overline (SGR 53 / 55).**
- [ ] **P2 — Blink (SGR 5 / 6 / 25).** Can share the cursor-blink timer; respect a config switch.
- [ ] **P2 — Hidden / conceal (SGR 8 / 28).** Text currently stays visible.
- [ ] Follow-up: these likely need `StyleFlags` to grow. Revisit the `TODO(cell-layout)` packed-style plan in `cell.rs` when doing this.

## 4. User-facing features

- [ ] **P1 — Font zoom.** Ctrl+= / Ctrl+- / Ctrl+0 (Cmd on macOS) to grow, shrink and reset the font size at runtime, re-rasterising and resizing the grid (PTY winsize plus in-band resize).
- [ ] **P1 — Clickable plain-text URLs.** Only OSC 8 links can be opened today (Ctrl/Cmd+click). Detect `http(s)://`, `file://` and similar URLs in the visible text, underline them on hover with the modifier held, and open them on modifier+click. Must handle soft-wrapped lines and trailing punctuation.
- [ ] **P1 — Scrollback search** (Ctrl+Shift+F / Cmd+F): incremental, highlight all matches, next/previous, regex optional.
- [ ] **P1 — Configurable keybindings** (`[keybindings]` table). Copy, paste and Shift+Insert are hardcoded today.
- [ ] **P2 — Prompt navigation.** Jump to previous/next prompt using the OSC 133 marks the terminal already records (`Term::prompt_marks`). Optionally select or copy the last command's output.
- [ ] **P2 — Tabs, splits and new windows** (or at least "new window" inheriting the OSC 7 cwd).
- [ ] **P2 — Desktop notifications** (OSC 9, OSC 777;notify, OSC 99) and **progress** (OSC 9;4, ConEmu; listed as Tier 2).
- [ ] **P2 — Light/dark theme switching** (mode 2031 notifications, `CSI ? 996 n` query) driven by winit's `ThemeChanged`.
- [ ] **P2 — Drag and drop files** to paste shell-quoted paths.
- [ ] **P2 — Keyboard selection / vi copy mode**, a scrollbar, and minimum-contrast enforcement.

## 5. Identity

- [ ] **P2 — toastty advertises itself as kitty.**
  - The child environment sets `KITTY_WINDOW_ID=1` (to steer yazi), and XTVERSION answers `toastty (kitty 0.42.0)`.
  - Apps may then use kitty-only features toastty doesn't implement: text sizing (OSC 66), OSC 99 notifications, the kitty clipboard protocol (OSC 5522), and remote control.
  - Revisit once the target apps probe for capabilities instead.

## Already solid

Kitty keyboard protocol, kitty graphics (including Unicode placeholders), Sixel, RGP,
synchronized output (2026), grapheme clustering (2027), in-band resize (2048),
reflow on resize, IME preedit, ligatures, OSC 4 / 7 / 8 / 52 / 133, bracketed paste,
mouse reporting (all encodings), focus events, DECSCUSR, DECSTBM, config hot-reload,
transparent backgrounds and the scroll-to-bottom chip.
