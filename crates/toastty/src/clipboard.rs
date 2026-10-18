//! Platform clipboard plumbing: CLIPBOARD vs PRIMARY.
//!
//! X11 and Wayland have two text buffers a terminal cares about:
//!
//! - `CLIPBOARD` — explicit copy (Ctrl+Shift+C / OSC 52 `c`), pasted
//!   with Ctrl+Shift+V.
//! - `PRIMARY` — implicitly set by selecting text, pasted with the
//!   middle mouse button or Shift+Insert (xterm, Alacritty, kitty,
//!   VTE all follow this convention).
//!
//! macOS and Windows only have the clipboard. There, every request
//! for [`ClipboardTarget::Primary`] falls back to the clipboard so OSC
//! 52 `p` reads/writes and middle-click paste still do something
//! useful, while "write the finished selection to PRIMARY" becomes a
//! no-op (the selection already goes to the clipboard).

pub use toastty_protocols::clipboard::{ClipboardTarget, ClipboardTargets};

/// True on platforms with a separate PRIMARY selection — the same cfg
/// arboard gates its `LinuxClipboardKind` extension traits on.
pub const HAS_PRIMARY: bool = cfg!(all(
    unix,
    not(any(
        target_os = "macos",
        target_os = "android",
        target_os = "emscripten"
    ))
));

/// The buffer actually used for `target` on this platform.
#[must_use]
pub fn effective_target(target: ClipboardTarget) -> ClipboardTarget {
    if HAS_PRIMARY {
        target
    } else {
        ClipboardTarget::Clipboard
    }
}

/// Distinct buffers to touch for a multi-target write, after platform
/// fallback. On macOS `cp` collapses to one clipboard write.
#[must_use]
pub fn effective_targets(targets: ClipboardTargets) -> Vec<ClipboardTarget> {
    let mut out = Vec::with_capacity(2);
    if targets.clipboard {
        out.push(ClipboardTarget::Clipboard);
    }
    if targets.primary {
        let t = effective_target(ClipboardTarget::Primary);
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Buffer Shift+Insert and middle-click paste from: PRIMARY where it
/// exists (xterm / Alacritty), else the clipboard.
#[must_use]
pub fn selection_paste_source() -> ClipboardTarget {
    effective_target(ClipboardTarget::Primary)
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
fn linux_kind(target: ClipboardTarget) -> arboard::LinuxClipboardKind {
    match target {
        ClipboardTarget::Clipboard => arboard::LinuxClipboardKind::Clipboard,
        ClipboardTarget::Primary => arboard::LinuxClipboardKind::Primary,
    }
}

/// Read text from `target` (with platform fallback).
///
/// # Errors
/// Propagates arboard's error (empty buffer, non-text content, server
/// unavailable).
pub fn get_text(
    cb: &mut arboard::Clipboard,
    target: ClipboardTarget,
) -> Result<String, arboard::Error> {
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    ))]
    {
        use arboard::GetExtLinux;
        cb.get().clipboard(linux_kind(target)).text()
    }
    #[cfg(not(all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    )))]
    {
        let _ = target;
        cb.get_text()
    }
}

/// Write `text` to `target` (with platform fallback).
///
/// # Errors
/// Propagates arboard's error.
pub fn set_text(
    cb: &mut arboard::Clipboard,
    target: ClipboardTarget,
    text: String,
) -> Result<(), arboard::Error> {
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    ))]
    {
        use arboard::SetExtLinux;
        cb.set().clipboard(linux_kind(target)).text(text)
    }
    #[cfg(not(all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    )))]
    {
        let _ = target;
        cb.set_text(text)
    }
}

/// Clear `target` (with platform fallback).
///
/// # Errors
/// Propagates arboard's error.
pub fn clear(cb: &mut arboard::Clipboard, target: ClipboardTarget) -> Result<(), arboard::Error> {
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    ))]
    {
        use arboard::ClearExtLinux;
        cb.clear_with().clipboard(linux_kind(target))
    }
    #[cfg(not(all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    )))]
    {
        let _ = target;
        cb.clear()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: ClipboardTargets = ClipboardTargets {
        clipboard: true,
        primary: true,
    };

    #[test]
    fn effective_targets_dedupes_on_fallback() {
        let got = effective_targets(BOTH);
        if HAS_PRIMARY {
            assert_eq!(
                got,
                vec![ClipboardTarget::Clipboard, ClipboardTarget::Primary]
            );
        } else {
            assert_eq!(got, vec![ClipboardTarget::Clipboard]);
        }
        let p_only = ClipboardTargets {
            clipboard: false,
            primary: true,
        };
        assert_eq!(
            effective_targets(p_only),
            vec![effective_target(ClipboardTarget::Primary)]
        );
        assert!(effective_targets(ClipboardTargets::default()).is_empty());
    }

    #[test]
    fn primary_falls_back_without_primary() {
        assert_eq!(
            effective_target(ClipboardTarget::Clipboard),
            ClipboardTarget::Clipboard
        );
        let expect = if HAS_PRIMARY {
            ClipboardTarget::Primary
        } else {
            ClipboardTarget::Clipboard
        };
        assert_eq!(effective_target(ClipboardTarget::Primary), expect);
        assert_eq!(selection_paste_source(), expect);
    }
}
