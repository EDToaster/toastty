//! Terminal state: ring-buffer grid, scrollback, cursor, modes, damage set.
//!
//! See [`docs/decisions/scrollback.md`](../../docs/decisions/scrollback.md) and
//! [`docs/decisions/redraw-policy.md`](../../docs/decisions/redraw-policy.md).

#![forbid(unsafe_code)]
