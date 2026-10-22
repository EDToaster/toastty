//! Parser for the Kitty graphics protocol APC control payload.
//!
//! Wire format: `ESC _ G <key>=<value>,<key>=<value>,... ; <body bytes> ESC \`.
//! This module parses ONLY the `<key>=<value>,...` portion (the
//! "header"). The body is the optional base64-encoded image data and is
//! handled separately by the decoder.
//!
//! Key types follow kitty 0.49.2's parser, not the per-action meaning:
//! every key has one wire type (unsigned 32-bit, signed 32-bit, or a
//! one-character enum for `a`, `t`, `o`, `d`) whatever the action, and the
//! action decides what the value means. `q=` and `m=` are numbers:
//! `q>=2` is silent, any non-zero `m=` means more. The animation actions reuse many placement keys
//! (`c`, `r`, `x`, `y`, `z`, `X`, `Y`, `s`, `v`, `C`); read them through
//! [`Header::frame`], [`Header::animation`] and [`Header::composition`]
//! instead of the placement-named fields.
//!
//! Like kitty, an unknown key, a bad number (sign on an unsigned key,
//! `+`, hex, overflow), a bad enum value, whitespace, or an empty or
//! trailing `,` pair makes the whole header malformed; the handler drops
//! such commands without a reply (WIRE-6).
//!
//! Reference: <https://sw.kovidgoyal.net/kitty/graphics-protocol/#control-data>.

use std::str;
use thiserror::Error;

/// Parsed key=value header from the start of a Kitty APC payload.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Header {
    /// `a=`. Defaults to [`Action::Transmit`].
    pub action: Action,
    /// `f=`. Defaults to [`Format::Rgba32`]. Any number parses; an
    /// unsupported one is [`Format::Unknown`], rejected when a transmit
    /// decodes (kitty: `EINVAL:Unknown image format: N`).
    pub format: Format,
    /// `t=`. Defaults to [`Transmission::Direct`].
    pub transmission: Transmission,
    /// `m=`. True iff more chunks follow (continuation flag).
    pub more: bool,
    /// `i=` image id, or 0 when the client wants the terminal to
    /// assign one.
    pub image_id: u32,
    /// `I=` image number. Pre-approved trade-off: we accept on
    /// transmit but require `i=` on subsequent ops.
    pub image_number: u32,
    /// `p=` placement id. 0 == "unnamed".
    pub placement_id: u32,
    /// `q=` quiet level.
    pub quiet: Quiet,
    /// `s=` source image width in pixels (raw RGB/RGBA only).
    pub source_width: u32,
    /// `v=` source image height in pixels (raw RGB/RGBA only).
    pub source_height: u32,
    /// `S=` total payload size (for the transmission, in bytes).
    pub size: u32,
    /// `O=` byte offset into the file (file transmission only).
    pub offset: u32,
    /// `x=` source pixel x-origin to start clipping from.
    pub src_x: u32,
    /// `y=` source pixel y-origin to start clipping from.
    pub src_y: u32,
    /// `w=` clipped width in source pixels (0 = full).
    pub src_w: u32,
    /// `h=` clipped height in source pixels (0 = full).
    pub src_h: u32,
    /// `X=` intra-cell pixel x-offset within the FIRST cell at which to
    /// start displaying the image (must be smaller than the cell width).
    /// This is a sub-cell pixel offset applied at render time — it does
    /// NOT change which cell the placement starts at (that comes from
    /// the cursor). Field name kept as `cell_x` for compatibility; the
    /// semantics are pixels-in-cell (M3).
    pub cell_x: u32,
    /// `Y=` intra-cell pixel y-offset within the FIRST cell at which to
    /// start displaying the image (must be smaller than the cell
    /// height). Sub-cell pixel offset applied at render time; does NOT
    /// change the starting cell (M3).
    pub cell_y: u32,
    /// `c=` width in cells, or 0 to derive from source.
    pub cols: u32,
    /// `r=` height in cells, or 0 to derive from source.
    pub rows: u32,
    /// `C=` cursor movement policy: true iff `C=1` (do not move).
    /// Derived from [`Header::cursor_movement`].
    pub cursor_no_move: bool,
    /// Raw `C=` value. Placement: cursor movement (`1` = don't move).
    /// Composition (`a=c`): the blend mode (`1` = overwrite).
    pub cursor_movement: u32,
    /// `N=` usage hint (`1` = transient). Accepted and currently
    /// ignored, which the spec allows.
    pub usage_hint: u32,
    /// `z=` z-index. Negatives render below text.
    pub z: i32,
    /// `o=` compression: present iff zlib.
    pub compression: Compression,
    /// `d=` delete spec. Only meaningful when `action == Delete`.
    pub delete: DeleteSpec,
    /// `U=` unicode-placeholder mode. When true, ops target the
    /// placeholder pipeline rather than the cursor.
    pub unicode_placeholder: bool,
    /// `P=`, `Q=` — relative-placement parent reference (M13). `P=` is
    /// the parent image id, `Q=` the parent placement id. When both are
    /// non-zero on an `a=p`, the new placement is positioned relative to
    /// the referenced parent placement at `(h_offset, v_offset)` cells.
    pub parent_image: u32,
    pub parent_placement: u32,
    /// `H=` — horizontal (column) cell offset of a relative placement
    /// from its parent's origin. Signed. M13.
    pub h_offset: i32,
    /// `V=` — vertical (row) cell offset of a relative placement from
    /// its parent's origin. Signed. M13.
    pub v_offset: i32,
}

/// Default `a=` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Action {
    /// `a=t` — transmit only.
    #[default]
    Transmit,
    /// `a=T` — transmit and place at the current cursor.
    TransmitAndPlace,
    /// `a=p` — place an already-transmitted image.
    Place,
    /// `a=d` — delete.
    Delete,
    /// `a=q` — query.
    Query,
    /// `a=f` — animation frame (`Enotsup` in M11a).
    Frame,
    /// `a=a` — animate (`Enotsup` in M11a).
    Animate,
    /// `a=c` — composition (`Enotsup` in M11a).
    Compose,
}

/// `a=f` keys (spec: "Keys for animation frame loading").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameKeys {
    /// `x=` / `y=`: where in the image the frame data goes, in pixels.
    pub x: u32,
    pub y: u32,
    /// `c=`: 1-based frame whose data is the base of a new frame
    /// (0 = transparent black).
    pub base_frame: u32,
    /// `r=`: 1-based frame being edited (0 = create a new frame).
    pub edit_frame: u32,
    /// `z=`: gap to the next frame in ms (0 = default, negative =
    /// gapless).
    pub gap_ms: i32,
    /// `X=`: blend mode (`1` = overwrite, else alpha blend).
    pub blend_mode: u32,
    /// `Y=`: background colour, 32-bit RGBA.
    pub background: u32,
}

/// `a=a` keys (spec: "Keys for animation control").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimationKeys {
    /// `s=`: 1 = stop, 2 = run but wait for new frames, 3 = run.
    pub state: u32,
    /// `r=`: 1-based frame affected by `z=`.
    pub frame: u32,
    /// `z=`: new gap for `frame`, in ms.
    pub gap_ms: i32,
    /// `c=`: 1-based frame to make current.
    pub current_frame: u32,
    /// `v=`: loop count (0 ignored, 1 infinite, n = n-1 loops).
    pub loops: u32,
}

/// `a=c` keys (spec: "Keys for animation frame composition").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositionKeys {
    /// `c=`: 1-based frame being edited (destination).
    pub dest_frame: u32,
    /// `r=`: 1-based frame whose data is overlaid (source).
    pub src_frame: u32,
    /// `x=` / `y=`: destination rectangle origin.
    pub dest_x: u32,
    pub dest_y: u32,
    /// `w=` / `h=`: rectangle size (0 = whole).
    pub width: u32,
    pub height: u32,
    /// `X=` / `Y=`: source rectangle origin.
    pub src_x: u32,
    pub src_y: u32,
    /// `C=`: blend mode (`1` = overwrite).
    pub blend_mode: u32,
}

impl Header {
    /// The keys of an `a=f` command, under their frame-loading meaning.
    #[must_use]
    pub fn frame(&self) -> FrameKeys {
        FrameKeys {
            x: self.src_x,
            y: self.src_y,
            base_frame: self.cols,
            edit_frame: self.rows,
            gap_ms: self.z,
            blend_mode: self.cell_x,
            background: self.cell_y,
        }
    }

    /// The keys of an `a=a` command, under their animation-control
    /// meaning.
    #[must_use]
    pub fn animation(&self) -> AnimationKeys {
        AnimationKeys {
            state: self.source_width,
            frame: self.rows,
            gap_ms: self.z,
            current_frame: self.cols,
            loops: self.source_height,
        }
    }

    /// The keys of an `a=c` command, under their composition meaning.
    #[must_use]
    pub fn composition(&self) -> CompositionKeys {
        CompositionKeys {
            dest_frame: self.cols,
            src_frame: self.rows,
            dest_x: self.src_x,
            dest_y: self.src_y,
            width: self.src_w,
            height: self.src_h,
            src_x: self.cell_x,
            src_y: self.cell_y,
            blend_mode: self.cursor_movement,
        }
    }
}

/// `f=` image format selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// `f=24` — raw RGB888.
    Rgb24,
    /// `f=32` — raw RGBA8888.
    #[default]
    Rgba32,
    /// `f=100` — PNG bytes.
    Png,
    /// Any other `f=` number. kitty parses it and rejects it only when
    /// a transmit decodes, with `EINVAL:Unknown image format: N`.
    Unknown(u32),
}

/// `t=` transmission medium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transmission {
    /// `t=d` — inline base64 (default).
    #[default]
    Direct,
    /// `t=f` — file path on disk.
    File,
    /// `t=t` — temporary file path (we should delete after use).
    TempFile,
    /// `t=s` — shared memory.
    Shared,
}

/// `q=` quietness level. Controls whether the terminal replies with
/// `OK` / errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quiet {
    /// `q=0` — reply to everything (default).
    #[default]
    Verbose,
    /// `q=1` — suppress OK, still send errors.
    NoOk,
    /// `q=2` — suppress all replies.
    Silent,
}

/// `o=` compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// No compression (default).
    #[default]
    None,
    /// `o=z` — zlib (deflate).
    Zlib,
}

/// `d=` delete specifier. We store the raw byte; semantics are applied
/// by the handler. Uppercase variants free the underlying image bytes
/// in addition to removing placements; lowercase only remove
/// placements. Reference:
/// <https://sw.kovidgoyal.net/kitty/graphics-protocol/#deleting-images>.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeleteSpec {
    /// Raw spec byte. `0` when no `d=` was specified.
    pub byte: u8,
}

impl DeleteSpec {
    /// True iff this is an uppercase delete — also frees the bytes.
    #[must_use]
    pub fn free_bytes(self) -> bool {
        self.byte.is_ascii_uppercase()
    }

    /// True when the spec means "delete all" (any case of 'a').
    #[must_use]
    pub fn is_all(self) -> bool {
        self.byte.eq_ignore_ascii_case(&b'a')
    }
}

/// Errors from [`parse`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum KittyHeaderError {
    /// Not a Kitty graphics APC payload (missing the `G` prefix).
    #[error("not a kitty graphics APC payload (missing 'G' prefix)")]
    NotKittyApc,
    /// A `key=value` pair was malformed.
    #[error("malformed key=value pair: {0:?}")]
    BadPair(String),
    /// A numeric value didn't parse as an integer.
    #[error("malformed integer value for key {key:?}: {value:?}")]
    BadInt { key: String, value: String },
    /// An unknown enum-tag value.
    #[error("unknown enum value for key {key:?}: {value:?}")]
    BadEnum { key: String, value: String },
    /// A key kitty doesn't know. kitty rejects the whole command.
    #[error("unknown key {0:?}")]
    UnknownKey(String),
}

/// Parse a Kitty graphics APC header.
///
/// `bytes` is the header portion (the part before the `;` body
/// separator). The leading `G` byte must be present — most callers
/// strip the surrounding `ESC _` / `ESC \` framing before invoking
/// this. Whitespace, empty pairs and unknown keys are errors, as in
/// kitty.
pub fn parse(bytes: &[u8]) -> Result<Header, KittyHeaderError> {
    let s = str::from_utf8(bytes).map_err(|_| KittyHeaderError::NotKittyApc)?;
    let s = s.strip_prefix('G').ok_or(KittyHeaderError::NotKittyApc)?;
    let mut h = Header::default();
    if s.is_empty() {
        return Ok(h);
    }
    // Strict like kitty 0.49.2: no whitespace, no empty `,,` or trailing
    // `,` pairs. Each of those makes kitty drop the command silently.
    for pair in s.split(',') {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| KittyHeaderError::BadPair(pair.to_string()))?;
        apply(&mut h, key, value)?;
    }
    Ok(h)
}

fn bad_int(key: &str, value: &str) -> KittyHeaderError {
    KittyHeaderError::BadInt {
        key: key.to_string(),
        value: value.to_string(),
    }
}

/// Decimal digits only (kitty rejects `+`, `-`, hex and overflow).
fn parse_u32(key: &str, value: &str) -> Result<u32, KittyHeaderError> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad_int(key, value));
    }
    value.parse::<u32>().map_err(|_| bad_int(key, value))
}

/// kitty's signed keys (`z`, `H`, `V`): an optional `-` and a magnitude
/// that fits 32 unsigned bits, wrapped into `i32`. So `z=2147483648` and
/// `z=-2147483649` are accepted (as kitty does) and `z=4294967296` is not.
fn parse_i32(key: &str, value: &str) -> Result<i32, KittyHeaderError> {
    let (neg, digits) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let magnitude = parse_u32(key, digits).map_err(|_| bad_int(key, value))?;
    let v = magnitude as i32;
    Ok(if neg { v.wrapping_neg() } else { v })
}

#[allow(clippy::too_many_lines)] // long match arm — each key is one line.
fn apply(h: &mut Header, key: &str, value: &str) -> Result<(), KittyHeaderError> {
    match key {
        "a" => {
            h.action = match value {
                "t" => Action::Transmit,
                "T" => Action::TransmitAndPlace,
                "p" => Action::Place,
                "d" => Action::Delete,
                "q" => Action::Query,
                "f" => Action::Frame,
                "a" => Action::Animate,
                "c" => Action::Compose,
                _ => {
                    return Err(KittyHeaderError::BadEnum {
                        key: key.to_string(),
                        value: value.to_string(),
                    });
                }
            };
        }
        "f" => {
            h.format = match parse_u32(key, value)? {
                24 => Format::Rgb24,
                32 => Format::Rgba32,
                100 => Format::Png,
                n => Format::Unknown(n),
            };
        }
        "t" => {
            h.transmission = match value {
                "d" => Transmission::Direct,
                "f" => Transmission::File,
                "t" => Transmission::TempFile,
                "s" => Transmission::Shared,
                _ => {
                    return Err(KittyHeaderError::BadEnum {
                        key: key.to_string(),
                        value: value.to_string(),
                    });
                }
            };
        }
        // A number in kitty: any non-zero value means "more chunks".
        "m" => h.more = parse_u32(key, value)? != 0,
        "i" => h.image_id = parse_u32(key, value)?,
        "I" => h.image_number = parse_u32(key, value)?,
        "p" => h.placement_id = parse_u32(key, value)?,
        // A number in kitty: `q=2` and above are silent.
        "q" => {
            h.quiet = match parse_u32(key, value)? {
                0 => Quiet::Verbose,
                1 => Quiet::NoOk,
                _ => Quiet::Silent,
            };
        }
        "s" => h.source_width = parse_u32(key, value)?,
        "v" => h.source_height = parse_u32(key, value)?,
        "S" => h.size = parse_u32(key, value)?,
        "O" => h.offset = parse_u32(key, value)?,
        "x" => h.src_x = parse_u32(key, value)?,
        "y" => h.src_y = parse_u32(key, value)?,
        "w" => h.src_w = parse_u32(key, value)?,
        "h" => h.src_h = parse_u32(key, value)?,
        "X" => h.cell_x = parse_u32(key, value)?,
        "Y" => h.cell_y = parse_u32(key, value)?,
        "c" => h.cols = parse_u32(key, value)?,
        "r" => h.rows = parse_u32(key, value)?,
        "C" => {
            // A number, not a flag: `a=c` uses it as the blend mode, so
            // `C=2` must parse.
            h.cursor_movement = parse_u32(key, value)?;
            h.cursor_no_move = h.cursor_movement == 1;
        }
        "N" => h.usage_hint = parse_u32(key, value)?,
        "z" => h.z = parse_i32(key, value)?,
        "o" => {
            h.compression = match value {
                "z" => Compression::Zlib,
                _ => {
                    return Err(KittyHeaderError::BadEnum {
                        key: key.to_string(),
                        value: value.to_string(),
                    });
                }
            };
        }
        "d" => {
            // `d=` is a single ASCII character (case-sensitive).
            let b = value.as_bytes();
            if b.len() != 1 {
                return Err(KittyHeaderError::BadEnum {
                    key: key.to_string(),
                    value: value.to_string(),
                });
            }
            h.delete = DeleteSpec { byte: b[0] };
        }
        // A number in kitty; any non-zero value enables it.
        "U" => h.unicode_placeholder = parse_u32(key, value)? != 0,
        "P" => h.parent_image = parse_u32(key, value)?,
        "Q" => h.parent_placement = parse_u32(key, value)?,
        "H" => h.h_offset = parse_i32(key, value)?,
        "V" => h.v_offset = parse_i32(key, value)?,
        // kitty rejects the whole command on an unknown key, so a client
        // can't rely on one being ignored.
        _ => return Err(KittyHeaderError::UnknownKey(key.to_string())),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_header_parses_to_defaults() {
        let h = parse(b"G").unwrap();
        assert_eq!(h.action, Action::Transmit);
        assert_eq!(h.format, Format::Rgba32);
        assert_eq!(h.image_id, 0);
        assert_eq!(h.z, 0);
        assert!(!h.more);
    }

    #[test]
    fn missing_g_prefix_is_error() {
        let err = parse(b"a=T,f=100").unwrap_err();
        assert_eq!(err, KittyHeaderError::NotKittyApc);
    }

    #[test]
    fn transmit_and_place_parses() {
        let h = parse(b"Ga=T,f=100,s=4,v=4,i=1").unwrap();
        assert_eq!(h.action, Action::TransmitAndPlace);
        assert_eq!(h.format, Format::Png);
        assert_eq!(h.source_width, 4);
        assert_eq!(h.source_height, 4);
        assert_eq!(h.image_id, 1);
    }

    #[test]
    fn negative_z_index_parses() {
        let h = parse(b"Ga=p,z=-100").unwrap();
        assert_eq!(h.z, -100);
    }

    #[test]
    fn formats_round_trip() {
        assert_eq!(parse(b"Gf=24").unwrap().format, Format::Rgb24);
        assert_eq!(parse(b"Gf=32").unwrap().format, Format::Rgba32);
        assert_eq!(parse(b"Gf=100").unwrap().format, Format::Png);
    }

    #[test]
    fn actions_round_trip() {
        let cases = [
            ("t", Action::Transmit),
            ("T", Action::TransmitAndPlace),
            ("p", Action::Place),
            ("d", Action::Delete),
            ("q", Action::Query),
            ("f", Action::Frame),
            ("a", Action::Animate),
            ("c", Action::Compose),
        ];
        for (s, expected) in cases {
            let h = parse(format!("Ga={s}").as_bytes()).unwrap();
            assert_eq!(h.action, expected, "for a={s}");
        }
    }

    #[test]
    fn unknown_key_is_error_like_kitty() {
        // kitty 0.49.2 drops `a=t,…,W=5` without a reply or an image.
        for h in [
            &b"Ga=T,f=100,xx=42,i=1"[..],
            b"Ga=t,i=1,W=5",
            b"Ga=t,i=1,b=1",
        ] {
            assert!(matches!(parse(h), Err(KittyHeaderError::UnknownKey(_))));
        }
    }

    /// Numbers follow kitty's parser (checked live against 0.49.2).
    #[test]
    fn numbers_follow_kitty() {
        for bad in [
            &b"Ga=t,i=+5"[..],
            b"Ga=t,i=0x10",
            b"Ga=t,i=-1",
            b"Ga=t,i=4294967296",
            b"Ga=t,s=abc",
            b"Ga=t,s=",
            b"Ga=t,X=-1",
            b"Ga=p,P=-1",
            b"Ga=T,z=4294967296",
            b"Ga=T,x=-5",
            b"Ga=t,q=-1",
            b"Ga=t,m=-1",
            b"Ga=t,o=",
            b"Ga=t,o=y",
            b"Ga=t,t=x",
            b"Ga=",
            b"Ga=d,d=xx",
            b"Ga=d,d=",
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        assert_eq!(parse(b"Ga=t,i=0004117").unwrap().image_id, 4117);
        assert_eq!(parse(b"Ga=T,z=2147483648").unwrap().z, i32::MIN);
        assert_eq!(parse(b"Ga=T,z=-2147483649").unwrap().z, i32::MAX);
        assert_eq!(parse(b"Ga=T,z=-7").unwrap().z, -7);
        assert_eq!(parse(b"Ga=t,N=1").unwrap().usage_hint, 1);
        assert!(parse(b"Ga=t,U=2").unwrap().unicode_placeholder);
        // `q=` and `m=` are numbers too (review of #37): kitty treats
        // `q=3` as silent and `m=2` as `m=1`.
        assert_eq!(parse(b"Ga=t,q=3").unwrap().quiet, Quiet::Silent);
        assert!(parse(b"Ga=t,m=2").unwrap().more);
    }

    #[test]
    fn unknown_format_number_parses() {
        assert_eq!(parse(b"Gf=8").unwrap().format, Format::Unknown(8));
        assert!(parse(b"Gf=png").is_err());
    }

    /// `C` is a number (WIRE-3): `a=c,C=2` must parse, and only `C=1`
    /// means "don't move the cursor".
    #[test]
    fn capital_c_is_a_number() {
        let h = parse(b"Ga=c,i=1,C=2").unwrap();
        assert_eq!(h.cursor_movement, 2);
        assert!(!h.cursor_no_move);
        assert_eq!(h.composition().blend_mode, 2);
        assert!(parse(b"Ga=T,C=1").unwrap().cursor_no_move);
    }

    #[test]
    fn animation_views_reinterpret_overloaded_keys() {
        let h = parse(b"Ga=f,i=1,x=1,y=2,c=3,r=4,z=-5,X=1,Y=4278190335").unwrap();
        let f = h.frame();
        assert_eq!(
            (f.x, f.y, f.base_frame, f.edit_frame, f.gap_ms),
            (1, 2, 3, 4, -5)
        );
        assert_eq!((f.blend_mode, f.background), (1, 0xFF00_00FF));
        let a = parse(b"Ga=a,i=1,s=3,v=2,r=1,z=100,c=2")
            .unwrap()
            .animation();
        assert_eq!(
            (a.state, a.loops, a.frame, a.gap_ms, a.current_frame),
            (3, 2, 1, 100, 2)
        );
        let c = parse(b"Ga=c,i=1,c=2,r=1,x=3,y=4,w=5,h=6,X=7,Y=8,C=1")
            .unwrap()
            .composition();
        assert_eq!(
            (c.dest_frame, c.src_frame, c.dest_x, c.dest_y),
            (2, 1, 3, 4)
        );
        assert_eq!(
            (c.width, c.height, c.src_x, c.src_y, c.blend_mode),
            (5, 6, 7, 8, 1)
        );
    }

    #[test]
    fn malformed_pair_is_error() {
        let err = parse(b"Ga=T,broken").unwrap_err();
        assert!(matches!(err, KittyHeaderError::BadPair(_)));
    }

    #[test]
    fn bad_int_is_error() {
        let err = parse(b"Gi=abc").unwrap_err();
        assert!(matches!(err, KittyHeaderError::BadInt { .. }));
    }

    #[test]
    fn bad_enum_is_error() {
        let err = parse(b"Ga=X").unwrap_err();
        assert!(matches!(err, KittyHeaderError::BadEnum { .. }));
    }

    #[test]
    fn quiet_levels() {
        assert_eq!(parse(b"Gq=0").unwrap().quiet, Quiet::Verbose);
        assert_eq!(parse(b"Gq=1").unwrap().quiet, Quiet::NoOk);
        assert_eq!(parse(b"Gq=2").unwrap().quiet, Quiet::Silent);
    }

    #[test]
    fn more_flag_parses() {
        assert!(parse(b"Gm=1").unwrap().more);
        assert!(!parse(b"Gm=0").unwrap().more);
    }

    #[test]
    fn compression_parses() {
        assert_eq!(parse(b"Go=z").unwrap().compression, Compression::Zlib);
    }

    #[test]
    fn delete_spec_carries_byte_and_case() {
        let h = parse(b"Ga=d,d=a").unwrap();
        assert!(h.delete.is_all());
        assert!(!h.delete.free_bytes());
        let h = parse(b"Ga=d,d=A").unwrap();
        assert!(h.delete.is_all());
        assert!(h.delete.free_bytes());
    }

    #[test]
    fn unicode_placeholder_flag_parses() {
        let h = parse(b"Ga=T,U=1,i=5").unwrap();
        assert!(h.unicode_placeholder);
    }

    #[test]
    fn parent_image_keys_parse() {
        let h = parse(b"GP=7,Q=3").unwrap();
        assert_eq!(h.parent_image, 7);
        assert_eq!(h.parent_placement, 3);
    }

    #[test]
    fn relative_offset_keys_parse() {
        let h = parse(b"Ga=p,i=2,P=7,Q=3,H=2,V=1").unwrap();
        assert_eq!(h.parent_image, 7);
        assert_eq!(h.parent_placement, 3);
        assert_eq!(h.h_offset, 2);
        assert_eq!(h.v_offset, 1);
    }

    #[test]
    fn relative_offset_keys_accept_negative() {
        let h = parse(b"Ga=p,i=2,P=7,Q=3,H=-4,V=-2").unwrap();
        assert_eq!(h.h_offset, -4);
        assert_eq!(h.v_offset, -2);
    }

    #[test]
    fn whitespace_and_empty_pairs_rejected_like_kitty() {
        for bad in [
            &b"G a = T , f = 100 "[..],
            b"Ga=t, i=1",
            b"Ga=t,i =1",
            b"Ga=t,i= 1",
            b"Ga=t,i=1,",
            b"Ga=t,,i=1",
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        assert_eq!(parse(b"Ga=T,f=100").unwrap().format, Format::Png);
    }

    #[test]
    fn source_geometry_round_trips() {
        let h = parse(b"Gs=64,v=32,x=4,y=8,w=20,h=10").unwrap();
        assert_eq!(h.source_width, 64);
        assert_eq!(h.source_height, 32);
        assert_eq!(h.src_x, 4);
        assert_eq!(h.src_y, 8);
        assert_eq!(h.src_w, 20);
        assert_eq!(h.src_h, 10);
    }

    #[test]
    fn cell_geometry_round_trips() {
        let h = parse(b"GX=2,Y=3,c=10,r=5").unwrap();
        assert_eq!(h.cell_x, 2);
        assert_eq!(h.cell_y, 3);
        assert_eq!(h.cols, 10);
        assert_eq!(h.rows, 5);
    }

    #[test]
    fn cursor_no_move_flag() {
        assert!(parse(b"Ga=T,C=1").unwrap().cursor_no_move);
        assert!(!parse(b"Ga=T,C=0").unwrap().cursor_no_move);
    }

    #[test]
    fn payload_size_offset() {
        let h = parse(b"GS=1000,O=12").unwrap();
        assert_eq!(h.size, 1000);
        assert_eq!(h.offset, 12);
    }

    #[test]
    fn image_number_parses() {
        let h = parse(b"GI=42").unwrap();
        assert_eq!(h.image_number, 42);
    }
}
