# Terminal Protocols

Tracking list of escape sequences, modes, and extensions toastty intends to support. Priority reflects how broadly apps actually use the protocol today, not how new it is.

## Tier 0 — Core (required for anything to work)

| Protocol | Notes |
| --- | --- |
| ECMA-48 / ANSI X3.64 | CSI, cursor movement, erase, scrolling regions, character sets |
| VT100 / VT220 / VT520 | DEC private modes (DECSET/DECRST), origin mode, etc. |
| xterm `ctlseqs` | De facto superset; the reference doc |
| UTF-8 | Wide chars (East Asian width), combining marks |
| PTY | `openpty()` on Unix; ConPTY on Windows (later) |
| terminfo entry | Ship as `toastty` (plus `xterm-256color` fallback) |
| SGR colors | 16 / 256 / 24-bit truecolor |
| Alternate screen buffer | DECSET 1049 |

## Tier 1 — Expected by modern apps

| Protocol | Sequence / Mode | Notes |
| --- | --- | --- |
| Mouse reporting | DECSET 9 / 1000 / 1002 / 1003, encodings 1005 / 1006 / 1015 | SGR (1006) required by tmux, neovim, helix. Without an encoding mode, reports use legacy X11 `CSI M Cb Cx Cy` (coords > 223 sent as xterm's `0x00` past-end marker); 1005 = UTF-8, 1015 = urxvt. Encodings are mutually exclusive like xterm: last set wins, reset only clears the matching mode. DECSET 9 = X10 presses only, no modifiers |
| Bracketed paste | DECSET 2004 | Pastes are sanitised in both modes (xterm `allowPasteControls: false`): C0 controls except HT/CR/LF, DEL and C1 are stripped, so embedded `ESC[200~`/`ESC[201~` can't be forged; CRLF/LF become CR |
| Focus in/out events | DECSET 1004 | |
| Clipboard | OSC 52 | Read + write, gated by `[security]`. Selection param: `c` clipboard, `p` PRIMARY (clipboard on macOS), `s`/empty both, `q`/`0-7` ignored; multiple chars combine, queries read the first supported one. Padded or unpadded base64; empty/invalid data clears the selection (xterm). Query replies mirror the request's BEL/ST terminator. OSC strings are capped at 8 MiB (`toastty_parser::MAX_OSC_LEN`) |
| PRIMARY selection | X11 / Wayland | Finished mouse selections go to PRIMARY as well as the clipboard. Middle-click (when the app isn't tracking the mouse, or with Shift) and Shift+Insert paste PRIMARY; on macOS they paste the clipboard |
| Hyperlinks | OSC 8 | |
| Current working directory | OSC 7 | `file://` URL; lets new tabs inherit cwd |
| Window/icon title | OSC 0 / 1 / 2 | Plus title stack (CSI 22/23) |
| Foreground/background/cursor color | OSC 10 / 11 / 12 | Query + set |
| Palette query/set | OSC 4 | |
| Cursor shape | DECSCUSR (CSI q) | Bar / block / underline + blink |
| REP | CSI b | Repeat preceding character |
| Mode query | DECRQM / DECRPM | Apps probe support |

## Tier 2 — Modern extensions (the 2024–2026 wave)

| Protocol | Sequence / Mode | Notes |
| --- | --- | --- |
| Synchronized output | DECSET 2026 (BSU/ESU) | Atomic redraws; tmux/neovim/Textual rely on it. Must-have |
| Grapheme cluster processing | DECSET 2027 | Terminal Unicode Core; opt-in correct emoji/ZWJ width |
| In-band resize notifications | DECSET 2048 | Stream-based resize, no SIGWINCH race |
| Kitty keyboard protocol | CSI u + progressive enhancement flags | Push/pop via `CSI > flags u` / `CSI < u`. Neovim/helix/zellij use it |
| Semantic prompts | OSC 133 (FinalTerm) | Powers prompt jumping, command status, shell integration |
| ConEmu progress | OSC 9;4 | Surfaced in tab/taskbar UI |
| XTGETTCAP | DCS + q ... ST | Programmatic terminfo query |
| Extended color query | OSC 21 | Hex-format color queries |
| Complex script placement | OSC 66 | Kitty extension for Indic/Arabic shaping width |

## Tier 3 — Graphics

Image support strategy: **Kitty graphics protocol primary, Sixel fallback.** iTerm2 inline images explicitly deferred (Kitty + Sixel cover the same use cases and the codepaths are already enough surface area).

| Protocol | Notes |
| --- | --- |
| Kitty graphics protocol | Primary. Includes unicode placeholder extension for tmux passthrough. Supports placement, z-index, animations. Chunked uploads (`m=1`) reassembled in the handler, not the parser |
| Sixel | Fallback for apps/environments that only emit Sixel. Older but broadest legacy support |
| Ratty Graphics Protocol (RGP) | Experimental. Inline 3D objects via APC sequences; OBJ/GLB asset registration. We are an early adopter — goal is to seed ecosystem support beyond the reference Ratty implementation |

> **APC framing caveat.** Both Kitty graphics and RGP use APC (`ESC _ ... ST`), and `vte 0.15.0` silently drops APC payloads — there is no `apc_dispatch` hook. toastty ships its own streaming APC scanner (~90 LoC). Handlers receive `start(header)` / `chunk(&[u8])` / `end()` rather than buffering whole payloads, which matters when an RGP `.glb` upload is 50 MB. See [decisions/streaming-apc.md](./decisions/streaming-apc.md).

## Tier 4 — Niceties / later

| Protocol | Notes |
| --- | --- |
| DECSLRM | Left/right margins; vim wants it |
| BiDi rendering | Arabic/Hebrew correctness |
| IME support | CJK input |
| Ligatures + font fallback | Render-side, not protocol |
| Reflow on resize | Rewrap scrollback when columns change |
| Width tables | Track Unicode UCD; gate behavior on mode 2027 |

## References

- xterm `ctlseqs.txt` — canonical control sequence reference
- VT510 manual (DEC)
- Kitty protocol docs: <https://sw.kovidgoyal.net/kitty/protocol-extensions/>
- Contour VT extensions: <https://contour-terminal.org/vt-extensions/>
- Terminal Unicode Core: <https://github.com/contour-terminal/terminal-unicode-core>
- In-band resize spec: <https://gist.github.com/rockorager/e695fb2924d36b2bcf1fff4a3704bd83>
- Synchronized Output spec: <https://gist.github.com/christianparpart/d8a62cc1ab659194337d73e399004036>
- Ratty Graphics Protocol: <https://github.com/orhun/ratty/blob/main/protocols/graphics.md>
- State of Terminal Emulation 2025: <https://www.jeffquast.com/post/state-of-terminal-emulation-2025/>
- `vttest` — compliance test suite
