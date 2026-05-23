//! Parser for the Kitty graphics protocol APC control payload.
//!
//! Wire format: `ESC _ G <key>=<value>,<key>=<value>,... ; <body bytes> ESC \`.
//! This module parses ONLY the `<key>=<value>,...` portion (the
//! "header"). The body is the optional base64-encoded image data and is
//! handled separately by the decoder. The parser is tolerant of unknown
//! keys (kitty itself reserves room for future extensions).
//!
//! Reference: <https://sw.kovidgoyal.net/kitty/graphics-protocol/#control-data>.

use std::str;
use thiserror::Error;

/// Parsed key=value header from the start of a Kitty APC payload.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Header {
    /// `a=`. Defaults to [`Action::Transmit`].
    pub action: Action,
    /// `f=`. Defaults to [`Format::Rgba32`].
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
    /// `X=` X cell offset on the grid (0..cols).
    pub cell_x: u32,
    /// `Y=` Y cell offset on the grid (0..rows).
    pub cell_y: u32,
    /// `c=` width in cells, or 0 to derive from source.
    pub cols: u32,
    /// `r=` height in cells, or 0 to derive from source.
    pub rows: u32,
    /// `C=` cursor movement policy: 0 == move, 1 == do-not-move.
    pub cursor_no_move: bool,
    /// `z=` z-index. Negatives render below text.
    pub z: i32,
    /// `o=` compression: present iff zlib.
    pub compression: Compression,
    /// `d=` delete spec. Only meaningful when `action == Delete`.
    pub delete: DeleteSpec,
    /// `U=` unicode-placeholder mode. When true, ops target the
    /// placeholder pipeline rather than the cursor.
    pub unicode_placeholder: bool,
    /// `P=`, `Q=` — image parent (composition / chained transmits).
    /// Kept for forwards-compat but not acted on.
    pub parent_image: u32,
    pub parent_placement: u32,
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
}

/// Parse a Kitty graphics APC header.
///
/// `bytes` is the header portion (the part before the `;` body
/// separator). The leading `G` byte must be present — most callers
/// strip the surrounding `ESC _` / `ESC \` framing before invoking
/// this. The parser tolerates trailing whitespace and unknown keys.
pub fn parse(bytes: &[u8]) -> Result<Header, KittyHeaderError> {
    let s = str::from_utf8(bytes).map_err(|_| KittyHeaderError::NotKittyApc)?;
    let s = s.strip_prefix('G').ok_or(KittyHeaderError::NotKittyApc)?;
    let mut h = Header::default();
    if s.trim().is_empty() {
        return Ok(h);
    }
    for pair in s.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| KittyHeaderError::BadPair(pair.to_string()))?;
        apply(&mut h, key.trim(), value.trim())?;
    }
    Ok(h)
}

fn parse_u32(key: &str, value: &str) -> Result<u32, KittyHeaderError> {
    value.parse::<u32>().map_err(|_| KittyHeaderError::BadInt {
        key: key.to_string(),
        value: value.to_string(),
    })
}

fn parse_i32(key: &str, value: &str) -> Result<i32, KittyHeaderError> {
    value.parse::<i32>().map_err(|_| KittyHeaderError::BadInt {
        key: key.to_string(),
        value: value.to_string(),
    })
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
            h.format = match value {
                "24" => Format::Rgb24,
                "32" => Format::Rgba32,
                "100" => Format::Png,
                _ => {
                    return Err(KittyHeaderError::BadEnum {
                        key: key.to_string(),
                        value: value.to_string(),
                    });
                }
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
        "m" => {
            h.more = match value {
                "0" => false,
                "1" => true,
                _ => {
                    return Err(KittyHeaderError::BadEnum {
                        key: key.to_string(),
                        value: value.to_string(),
                    });
                }
            };
        }
        "i" => h.image_id = parse_u32(key, value)?,
        "I" => h.image_number = parse_u32(key, value)?,
        "p" => h.placement_id = parse_u32(key, value)?,
        "q" => {
            h.quiet = match value {
                "0" => Quiet::Verbose,
                "1" => Quiet::NoOk,
                "2" => Quiet::Silent,
                _ => {
                    return Err(KittyHeaderError::BadEnum {
                        key: key.to_string(),
                        value: value.to_string(),
                    });
                }
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
            h.cursor_no_move = match value {
                "0" => false,
                "1" => true,
                _ => {
                    return Err(KittyHeaderError::BadEnum {
                        key: key.to_string(),
                        value: value.to_string(),
                    });
                }
            };
        }
        "z" => h.z = parse_i32(key, value)?,
        "o" => {
            h.compression = match value {
                "z" => Compression::Zlib,
                "" => Compression::None,
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
        "U" => {
            h.unicode_placeholder = match value {
                "0" => false,
                "1" => true,
                _ => {
                    return Err(KittyHeaderError::BadEnum {
                        key: key.to_string(),
                        value: value.to_string(),
                    });
                }
            };
        }
        "P" => h.parent_image = parse_u32(key, value)?,
        "Q" => h.parent_placement = parse_u32(key, value)?,
        // Unknown keys are tolerated for forwards compat. Future kitty
        // versions may add fields; we want existing terminals to keep
        // working with them.
        _ => {}
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
    fn unknown_key_tolerated() {
        let h = parse(b"Ga=T,f=100,xx=42,i=1").unwrap();
        assert_eq!(h.action, Action::TransmitAndPlace);
        assert_eq!(h.image_id, 1);
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
    fn whitespace_tolerated() {
        let h = parse(b"G a = T , f = 100 ").unwrap();
        assert_eq!(h.action, Action::TransmitAndPlace);
        assert_eq!(h.format, Format::Png);
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
