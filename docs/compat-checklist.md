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

Test input → expected result (xterm). Done in [#20](https://github.com/EDToaster/toastty/pull/20): see `crates/toastty-term/src/term/vt.rs` and its tests.

- [x] **P1 — REP `CSI Ps b`** (repeat the preceding graphic character). `a\e[3b` → `aaaa`. Wide characters repeat too; the count is capped at one screenful.
- [x] **P1 — CNL `CSI Ps E` / CPL `CSI Ps F`** (cursor to column 1 of the next or previous line — the left margin under DECSLRM).
- [x] **P1 — HPA ``CSI Ps ` ``, HPR `CSI Ps a`, VPR `CSI Ps e`** (absolute/relative horizontal moves and relative vertical moves). CHA (`G`) and VPA (`d`) already work.
- [x] **P1 — Tab stops.**
  - HTS `ESC H`, TBC `CSI 0 g` / `CSI 3 g`, CHT `CSI Ps I` and CBT `CSI Ps Z` are implemented on per-column stops (default every 8 columns).
  - Tab stops reset on RIS, DECSTR and resize. HT / CHT stop at the right margin when the cursor is inside it.
- [x] **P1 — SCOSC / SCORC `CSI s` / `CSI u`** (save/restore cursor, no private marker). They share the DECSC slot with `ESC 7` / `ESC 8` and `?1048`, as in xterm. `CSI s` means SCOSC only while DECLRMM (`?69`) is off; the kitty `CSI > u` / `CSI < u` / `CSI = u` / `CSI ? u` forms are unaffected.
- [x] **P1 — IRM `CSI 4 h` / `CSI 4 l`** (insert mode). `abc\r\e[4hX` → `Xabc`; text shifted past the right margin is dropped.
- [x] **P2 — ANSI-mode DECRQM `CSI Ps $ p`** (no `?`). Replies `CSI Ps ; Pm $ y`: IRM reports set/reset, KAM (2) and LNM (20) permanently reset, SRM (12) permanently set, anything else `0`.
- [x] **P2 — DECALN `ESC # 8`** (fill the screen with `E`, reset the margins, home the cursor; used by vttest).
- [x] **P2 — DA3 `CSI = c`.** Replies `DCS ! | 00000000 ST`, like xterm.
- [x] **P2 — DECSLRM / DECLRMM `?69`.**
  - `CSI Pl ; Pr s` sets the margins while `?69` is on. It homes the cursor; pairs with `Pl >= Pr` are ignored.
  - Inside the margins: autowrap, CR / NEL / CNL, IND / RI / LF scrolling, SU / SD, IL / DL and ICH / DCH all stay within the rectangle. CUF / CUB stop at the margins.
  - Nothing scrolls into scrollback while the margins narrow the screen. Wraps inside margins aren't marked as soft wraps, so they don't join lines on reflow.
  - Leaving `?69` or resizing drops the margins.
  - Image placements don't follow column-limited scrolls.
- [x] **Also: DECSTR `CSI ! p`** (soft reset — needed for the tab-stop reset above). Resets IRM, DECOM, DECAWM, DECCKM, DECKPAM, both margin pairs, SGR, cursor visibility, the saved cursor and tab stops.
- [x] **Also: CUU / CUD stop at the DECSTBM margins** when the cursor starts inside the scroll region (xterm; needed for DECOM).

### DEC private modes (`apply_decset`)

All of these report through DECRQM `CSI ? Ps $ p` and reset on RIS. Done in [#20](https://github.com/EDToaster/toastty/pull/20).

- [x] **P1 — DECCKM `?1`** (application cursor keys).
  - Unmodified Arrow, Home and End keys send `SS3 A` / `SS3 B` / `SS3 C` / `SS3 D` / `SS3 H` / `SS3 F` while set. With a modifier they keep the CSI form, as in xterm.
  - The alternate-screen wheel-to-arrows translation uses the same form.
  - The kitty keyboard encoder is unaffected.
- [x] **P1 — DECKPAM / DECKPNM `ESC =` / `ESC >`** (application keypad), plus DECNKM `?66` as the same flag.
  - Behaviour follows xterm's default `numLock: true`: with NumLock off the keypad sends `SS3 p`..`SS3 y`, `SS3 n` (`.`), `SS3 M` (Enter), `SS3 k` / `m` / `j` / `o` / `X` / `l` (`+ - * / = ,`). With NumLock on it types as usual.
  - Keypad digits tell NumLock apart through their logical key (a digit or a navigation key). Operator keys use the platform NumLock bit, or else the state the last keypad digit showed. The default is "on", so a shell that enables `smkx` (oh-my-zsh) keeps typing `+` and Enter normally.
  - Legacy encoding only.
- [x] **P1 — DECAWM `?7`** (autowrap). `\e[?7l` + long line overwrites the last column, and the cursor stays there. A wide character at the edge lands in the last two columns.
- [x] **P1 — DECOM `?6`** (origin mode).
  - CUP / HVP / VPA / CHA / HPA are relative to the DECSTBM (and DECSLRM) margins and clamped to them. CPR (`CSI 6 n` / `CSI ? 6 n`) reports relative coordinates.
  - Setting or resetting the mode homes the cursor. DECSC / DECRC save and restore DECOM and DECAWM.
- [x] **P2 — `?5` DECSCNM** (reverse video). The renderer swaps the theme's default fg / bg and keeps the background alpha.
- [x] **P2 — `?12`** (cursor blink, att610). Toggles blinking and keeps the DECSCUSR shape.
- [x] **P2 — `?1007`** (alternate scroll). On by default (the old behaviour). Resetting it stops the alternate screen turning wheel motion into arrow keys.
- [x] **P2 — `?1036` / `?1039`** (meta/alt sends escape). Both are on by default. Once both are reset, Alt-modified keys are sent without the ESC prefix (Ghostty's behaviour, rather than xterm's 8-bit meta).

### Replies and queries

- [x] **P1 — XTGETTCAP `DCS + q <hex> ST`.** Done: per-name replies for `TN`, `Co`, `Tc`, `RGB`, `setrgbf`/`setrgbb`, `Ss`/`Se`, `Sync`, bracketed paste / focus caps, and `Ms` when OSC 52 writes are allowed (`toastty-term` `dcs_query`).
  - There's no reply today.
  - Neovim queries `Tc` / `RGB` / `setrgbf` to enable truecolor, and `TN` / `Ms` and others.
  - Answer the capabilities toastty implements; reply `DCS 0 + r ST` for unknown ones.
- [x] **P1 — DECRQSS `DCS $ q <Pt> ST`.** Done: `m` (xterm colon form), `r`, ` q`, `"p`, `t`, `$|`; others reply `DCS 0 $ r ST`.
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

- [x] **P0 — No truecolor signal to apps.** Done: child env sets `COLORTERM=truecolor`, `TERM_PROGRAM=toastty`, `TERM_PROGRAM_VERSION`; XTGETTCAP / DECRQSS answered (neovim auto-enables `termguicolors`).
  - The child environment doesn't set `COLORTERM=truecolor`, and the XTGETTCAP / DECRQSS probes above go unanswered.
  - Neovim (auto `termguicolors`), bat, delta, helix, micro, ripgrep's `--colors` and others may fall back to 256 colours even though toastty renders 24-bit SGR fine.
  - Fix: set `COLORTERM=truecolor`, answer XTGETTCAP `Tc` / `RGB`, and answer DECRQSS `m`.

## 3. SGR rendition

`Style` only stores bold, italic, single underline and reverse. Everything else is
parsed and dropped (screenshot check: `printf '\e[2mdim \e[9mstrike \e[4:3mcurly\e[0m'`).

- [x] **P1 — Dim / faint (SGR 2, cleared by 22).** Glyph and decoration colour blended 50% toward the displayed bg (sRGB); 22 clears bold and dim.
- [x] **P1 — Strikethrough (SGR 9 / 29).** Font-derived position/thickness (swash strikeout metrics, xterm 3/8-ascent fallback).
- [x] **P1 — Double underline: SGR 21 and `4:2`**, with `4:0` / 24 clearing every style. Styles live in an `UnderlineStyle` enum on the cell.
- [ ] **P1 — Curly / dotted / dashed underlines (`4:3`–`4:5`).** Parsed and stored per cell, but still drawn as a single underline. Neovim and helix use curly underlines for diagnostics.
- [ ] **P1 — Underline colour SGR 58 / 59.** It's parsed into `cursor_underline_color` only for the kitty Unicode-placeholder image-id trick, and never stored per cell or rendered.
- [x] **P2 — Overline (SGR 53 / 55).**
- [x] **P2 — Blink (SGR 5 / 6 / 25).** Own phase on the cursor-blink state machine, armed only while blinking cells are visible; `[text] blink` switch.
- [x] **P2 — Hidden / conceal (SGR 8 / 28).** No glyph or decorations (xterm draws them in bg); bg + selection paint; copy keeps the text.
- [x] Follow-up: `StyleFlags` is now a bit byte + `UnderlineStyle` byte (2 bytes); `Cell` shrank from 24 to 20 bytes.

## 4. User-facing features

- [x] **P1 — Font zoom.** Ctrl+= / Ctrl+- / Ctrl+0 (Cmd on macOS) to grow, shrink and reset the font size at runtime, re-rasterising and resizing the grid (PTY winsize plus in-band resize).
  - Done: `toastty::font_zoom` (bindings incl. Ctrl++ / Ctrl+Shift+= / keypad, 1 px steps clamped to 6..=72 px, reload rebasing); `Renderer::set_font_size` re-rasterises in place without reloading the font database; regrid reuses the window-resize path (`resync_grid` + 2048 report). Documented in the README.
- [x] **P1 — Clickable plain-text URLs.** Only OSC 8 links can be opened today (Ctrl/Cmd+click). Detect `http(s)://`, `file://` and similar URLs in the visible text, underline them on hover with the modifier held, and open them on modifier+click. Must handle soft-wrapped lines and trailing punctuation. Done: `toastty_protocols::url_detect` + `Term::link_at` (soft wraps, wide/grapheme cells, scrollback), Ctrl/Cmd-hover underline + hand cursor, shared safe opener for OSC 8 and detected URLs.
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
