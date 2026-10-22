# Mouse selection

How toastty's local (terminal-native) text selection behaves. The model
lives in `crates/toastty-term/src/selection.rs` and the `Term` selection
section of `term.rs`; the mouse wiring is in `crates/toastty/src/main.rs`.

## Gestures

| Gesture | Selects |
| --- | --- |
| Drag | Characters, row-major |
| Double-click (+ drag) | Words — see [Word characters](#word-characters) |
| Triple-click (+ drag) | Whole *logical* lines: a soft-wrapped line is selected as one, like kitty / Ghostty / Alacritty |
| Alt+drag or Ctrl+Alt+drag | Rectangular block. Ctrl+Alt is kitty's binding and works under window managers (Openbox, Xfce, …) that grab Alt+drag to move windows |
| Shift + any of the above | Same, even when the app has enabled mouse tracking (DECSET 1000/1002/1003) |
| Drag past the top / bottom edge | Auto-scrolls through scrollback (~20 lines/s) while the button is held |

- **Shift bypasses mouse tracking.** This is the xterm convention, shared
  by kitty, Alacritty, WezTerm, Ghostty and iTerm2, and assumed by tmux
  and herdr. While mouse tracking is on, a Shift+left press starts a
  local selection, and that press, its drag motion and its release are
  never reported to the app. Releasing Shift mid-drag doesn't hand the
  release to the app.
- **Selection works on the alternate screen too** (less, man, vim with
  `mouse=`, or herdr/tmux/vim `mouse=a` via Shift). Switching screens
  (DECSET 47/1047/1049) clears the selection.
- **A plain click selects nothing.** It doesn't create a one-cell
  selection, and it doesn't touch the clipboard, so clicking to focus
  the window or to dismiss a highlight keeps whatever you copied
  elsewhere. A selection only starts once the pointer leaves the cell
  it was pressed on.
- **Wide characters are atomic.** Selecting either half of a width-2
  character (CJK, emoji) selects all of it, in both the highlight and
  the copied text.

## After release

When you release the button, the selected text is copied to the
clipboard (copy-on-select). The highlight **stays visible**, as in other
terminals, so Ctrl+Shift+C / Cmd+C still copies it afterwards. The
highlight is cleared by:

- the next click (including a click that goes to a mouse-tracking app),
- typing or IME input,
- output that changes the content of any selected row. An identical
  redraw or a style-only change doesn't count. Output that just scrolls
  the selected rows up keeps the selection, because it's pinned to line
  ids,
- any selected row being evicted from scrollback,
- resize, RIS, or a screen switch.

## Copied text

- Rows are joined with `\n`, except across a soft wrap, where they're
  joined directly so a wrapped line comes back as one line.
- Trailing whitespace is trimmed only at hard line ends and at the end
  of the selection. Spaces that happen to sit at a soft-wrap boundary
  are kept, so `abcd efgh` wrapped after the space still copies as
  `abcd efgh`.
- The blank pad cell a wide character leaves in the last column, when
  it didn't fit there and wrapped, is dropped. toastty marks that cell
  when it prints the wide char, so a real space in the last column is
  kept even when the next row starts with a wide character.
- Block selections copy one line per row, trimmed per row.
- If part of a selection has been evicted from scrollback while you
  were dragging, the rows that are still retained are copied.

## Word characters

Double-click treats every Unicode letter and digit (`é`, `ß`, `日`, `ж`,
`٣`, …) as a word character, plus the characters in
`selection.word_chars`:

```toml
[selection]
# Default: keeps paths, URLs and e-mail addresses together.
word_chars = "_-./~:?&=%+#@"
```

With the default, `café_x`, `~/src/main.rs`,
`https://example.com/a?b=c&d=1` and `user@host` are each selected whole.
Set `word_chars = "_"` for the classic identifier rule, or `""` for
letters and digits only. Words also continue across soft wraps.
