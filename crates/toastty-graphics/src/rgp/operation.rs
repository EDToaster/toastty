//! Parser for the Ratty Graphics Protocol APC payload.
//!
//! Wire format: `ESC _ ratty;g;<verb>[;<key=value>...] ESC \`. The
//! `toastty-parser` APC scanner strips the introducer and terminator;
//! this module is handed the bytes between (starting with the
//! `ratty;g;` namespace prefix).
//!
//! The `r` verb's payload mode carries a trailing base64 chunk
//! *after* the last `key=value` pair, separated by the same `;` —
//! e.g. `r;id=1;fmt=glb;source=payload;more=0;<base64>`. The parser
//! recognises this shape by treating any token that does NOT contain
//! `=` as the payload, but only when `source=payload` has been seen.
//!
//! Reference: <https://github.com/orhun/ratty/blob/main/protocols/graphics.md>.

use thiserror::Error;

/// Top-level RGP operation parsed off the wire.
#[derive(Debug, Clone, PartialEq)]
pub enum RgpOperation {
    /// `s` — support query. The terminal must reply with the
    /// capability string from [`crate::rgp::reply::support_reply`].
    SupportQuery,

    /// `r` — register an asset by id.
    Register {
        /// Object id chosen by the client.
        id: u32,
        /// Declared format (`glb` or `obj`).
        format: RgpFormat,
        /// Where the asset bytes come from.
        source: RgpRegisterSource,
    },

    /// `p` — place a previously registered object.
    Place {
        /// Registered object id.
        id: u32,
        /// Anchor cell + cell span.
        anchor: RgpAnchor,
        /// Style + transform fields.
        style: RgpPlacementStyle,
    },

    /// `u` — partial update to a previously placed object's style.
    /// Anchor/cell span are NOT mutable; only mutable style fields
    /// are present here as `Option`s. Absent fields preserve the
    /// existing placement value.
    Update {
        /// Registered object id.
        id: u32,
        /// Sparse style/transform overrides.
        update: RgpPlacementUpdate,
    },

    /// `d` — delete. `id == None` means "delete all RGP placements".
    Delete {
        /// Optional placement id.
        id: Option<u32>,
    },
}

/// Declared payload format (`fmt=` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RgpFormat {
    /// `fmt=glb` — binary glTF.
    Glb,
    /// `fmt=obj` — Wavefront OBJ. Parsed off the wire for forwards
    /// compat; the v1 renderer rejects it at load time.
    Obj,
}

/// Where the bytes for `r` come from.
#[derive(Debug, Clone, PartialEq)]
pub enum RgpRegisterSource {
    /// `path=<name>` — leaf-name lookup against the embedded asset
    /// bundle (or, if configured, the user's asset directory). See
    /// `docs/decisions/rgp-protocol.md` §1 for the policy.
    Path {
        /// Leaf-name (untrusted — the resolver does the validation).
        name: String,
    },

    /// `source=payload;more=<0|1>;<base64>` — bytes inline.
    Payload {
        /// Optional `name=` for diagnostics. Not used as a filename.
        name: Option<String>,
        /// `more=1` ⇒ more chunks follow; `more=0` ⇒ final chunk.
        more: bool,
        /// Raw payload bytes (base64-decoded by the parser).
        data: Vec<u8>,
    },
}

/// Cell-space anchor + span from the `p` verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgpAnchor {
    /// `row=` — anchor row at the centre of the placement.
    pub row: u16,
    /// `col=` — anchor column at the centre of the placement.
    pub col: u16,
    /// `w=` — width in terminal cells.
    pub cols: u16,
    /// `h=` — height in terminal cells.
    pub rows: u16,
}

/// Style + transform fields on a placement.
///
/// Defaults match the RGP spec: scale 1.0, depth 0.0, brightness 1.0,
/// no translation / no rotation, identity non-uniform scale, no
/// animation, no color tint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RgpPlacementStyle {
    /// `animate=` (default `false`). When true, the renderer applies
    /// a default animation. v1 = slow spin around Y.
    pub animate: bool,
    /// `scale=` uniform multiplier (default `1.0`).
    pub scale: f32,
    /// `depth=` z-offset; maps to NDC via the convention in
    /// `docs/decisions/rgp-protocol.md` §3. Default `0.0` is
    /// co-planar with text.
    pub depth: f32,
    /// `color=RRGGBB` optional tint.
    pub color: Option<[u8; 3]>,
    /// `brightness=` output multiplier (default `1.0`).
    pub brightness: f32,
    /// `(px, py, pz)` translation relative to the anchor (default `0`).
    pub offset: [f32; 3],
    /// `(rx, ry, rz)` rotation in degrees (default `0`).
    pub rotation: [f32; 3],
    /// `(sx, sy, sz)` non-uniform scale (default `1`).
    pub scale3: [f32; 3],
}

impl Default for RgpPlacementStyle {
    fn default() -> Self {
        Self {
            animate: false,
            scale: 1.0,
            depth: 0.0,
            color: None,
            brightness: 1.0,
            offset: [0.0; 3],
            rotation: [0.0; 3],
            scale3: [1.0; 3],
        }
    }
}

/// Sparse override for the `u` verb. Every field is `Option`-wrapped;
/// `None` means "preserve the existing placement value."
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RgpPlacementUpdate {
    pub animate: Option<bool>,
    pub scale: Option<f32>,
    pub depth: Option<f32>,
    pub color: Option<[u8; 3]>,
    pub brightness: Option<f32>,
    /// `[px, py, pz]` — each axis is independently optional.
    pub offset: [Option<f32>; 3],
    /// `[rx, ry, rz]`.
    pub rotation: [Option<f32>; 3],
    /// `[sx, sy, sz]`.
    pub scale3: [Option<f32>; 3],
}

impl RgpPlacementStyle {
    /// Merge a sparse update onto this style, overwriting only the
    /// fields the update set. Used by `RgpScene::apply_update`.
    pub fn apply(&mut self, u: &RgpPlacementUpdate) {
        if let Some(v) = u.animate {
            self.animate = v;
        }
        if let Some(v) = u.scale {
            self.scale = v;
        }
        if let Some(v) = u.depth {
            self.depth = v;
        }
        if let Some(v) = u.color {
            self.color = Some(v);
        }
        if let Some(v) = u.brightness {
            self.brightness = v;
        }
        for i in 0..3 {
            if let Some(v) = u.offset[i] {
                self.offset[i] = v;
            }
            if let Some(v) = u.rotation[i] {
                self.rotation[i] = v;
            }
            if let Some(v) = u.scale3[i] {
                self.scale3[i] = v;
            }
        }
    }
}

/// Parse errors. Tag-only; the rejected bytes are not preserved.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RgpParseError {
    /// Payload did not start with the `ratty;g;` namespace prefix.
    #[error("missing `ratty;g;` namespace prefix")]
    NotRgp,
    /// Payload was not valid UTF-8.
    #[error("payload is not valid UTF-8")]
    NotUtf8,
    /// No verb token after the namespace prefix.
    #[error("missing verb")]
    MissingVerb,
    /// Verb was not one of `s`, `r`, `p`, `u`, `d`.
    #[error("unknown verb `{0}`")]
    UnknownVerb(String),
    /// A required field for the verb was missing.
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    /// A field that was present had a value the parser could not
    /// interpret (bad integer, bad float, malformed color, etc.).
    #[error("malformed value for field `{0}`")]
    MalformedField(&'static str),
    /// Payload-mode register: base64 decode failed.
    #[error("payload is not valid base64")]
    BadBase64,
    /// `fmt=` was not `glb` or `obj`.
    #[error("unsupported format `{0}`")]
    UnsupportedFormat(String),
}

/// Required namespace prefix for an RGP APC payload (after the
/// `toastty-parser` APC scanner has stripped `ESC _` / `ESC \`).
pub const RGP_PREFIX: &[u8] = b"ratty;g;";

/// Parse a buffered RGP APC payload.
///
/// The payload must start with [`RGP_PREFIX`] — caller is expected
/// to demux based on the first byte (`G` ⇒ Kitty, `ratty;g;` ⇒ RGP).
// The full grammar is a flat list of optional fields; the function
// is mostly a per-key match arm. Splitting it artificially just
// fragments the match.
#[allow(clippy::too_many_lines)]
pub fn parse(payload: &[u8]) -> Result<RgpOperation, RgpParseError> {
    let Some(rest) = payload.strip_prefix(RGP_PREFIX) else {
        return Err(RgpParseError::NotRgp);
    };
    let text = std::str::from_utf8(rest).map_err(|_| RgpParseError::NotUtf8)?;

    let mut parts = text.split(';');
    let verb = parts.next().ok_or(RgpParseError::MissingVerb)?;

    // First pass: collect every `key=value` pair AND remember any
    // bare token (no `=`) we encounter after `source=payload` has
    // appeared — that's the trailing base64 chunk for `r`.
    let mut id: Option<u32> = None;
    let mut format: Option<String> = None;
    let mut path: Option<String> = None;
    let mut source: Option<String> = None;
    let mut more: Option<bool> = None;
    let mut name: Option<String> = None;
    let mut row: Option<u16> = None;
    let mut col: Option<u16> = None;
    let mut w: Option<u16> = None;
    let mut h: Option<u16> = None;
    let mut animate: Option<bool> = None;
    let mut scale: Option<f32> = None;
    let mut depth: Option<f32> = None;
    let mut color: Option<[u8; 3]> = None;
    let mut brightness: Option<f32> = None;
    let mut px: Option<f32> = None;
    let mut py: Option<f32> = None;
    let mut pz: Option<f32> = None;
    let mut rx: Option<f32> = None;
    let mut ry: Option<f32> = None;
    let mut rz: Option<f32> = None;
    let mut sx: Option<f32> = None;
    let mut sy: Option<f32> = None;
    let mut sz: Option<f32> = None;
    let mut payload_chunk: Option<&str> = None;

    for part in parts {
        if part.is_empty() {
            continue;
        }
        // A bare token (no `=`) is the trailing payload chunk for
        // `r;source=payload`. Outside that context we ignore it.
        // Base64-padded chunks (e.g. `d29ybGQ=`) split into
        // (key="d29ybGQ", value=""), so the `_` arm of the match
        // below also has to catch the payload — see Ratty's parser
        // for the precedent.
        let Some((key, value)) = part.split_once('=') else {
            if verb == "r" && source.as_deref() == Some("payload") {
                payload_chunk = Some(part);
                break;
            }
            continue;
        };
        match key {
            "id" => id = Some(parse_u32("id", value)?),
            "fmt" => format = Some(value.to_string()),
            "path" => path = Some(value.to_string()),
            "source" => source = Some(value.to_string()),
            "more" => more = Some(parse_bool("more", value)?),
            "name" => name = Some(value.to_string()),
            "row" => row = Some(parse_u16("row", value)?),
            "col" => col = Some(parse_u16("col", value)?),
            "w" => w = Some(parse_u16("w", value)?),
            "h" => h = Some(parse_u16("h", value)?),
            "animate" => animate = Some(parse_bool("animate", value)?),
            "scale" => scale = Some(parse_f32("scale", value)?),
            "depth" => depth = Some(parse_f32("depth", value)?),
            // Ratty accepts both `color=` and `tint=` aliases.
            "color" | "tint" => color = Some(parse_color(value)?),
            "brightness" => brightness = Some(parse_f32("brightness", value)?),
            "px" => px = Some(parse_f32("px", value)?),
            "py" => py = Some(parse_f32("py", value)?),
            "pz" => pz = Some(parse_f32("pz", value)?),
            "rx" => rx = Some(parse_f32("rx", value)?),
            "ry" => ry = Some(parse_f32("ry", value)?),
            "rz" => rz = Some(parse_f32("rz", value)?),
            "sx" => sx = Some(parse_f32("sx", value)?),
            "sy" => sy = Some(parse_f32("sy", value)?),
            "sz" => sz = Some(parse_f32("sz", value)?),
            _ => {
                // Forward-compat: silently ignore unknown keys —
                // EXCEPT in payload mode, where a trailing base64
                // chunk with `=` padding parses as a fake
                // `key=value` pair. Convention: payload comes last,
                // so the first unknown token under payload mode IS
                // the payload.
                if verb == "r" && source.as_deref() == Some("payload") {
                    payload_chunk = Some(part);
                    break;
                }
            }
        }
    }

    match verb {
        "s" => Ok(RgpOperation::SupportQuery),
        "r" => parse_register(
            id,
            format,
            path,
            source,
            more,
            name,
            payload_chunk,
        ),
        "p" => Ok(RgpOperation::Place {
            id: id.ok_or(RgpParseError::MissingField("id"))?,
            anchor: RgpAnchor {
                row: row.ok_or(RgpParseError::MissingField("row"))?,
                col: col.ok_or(RgpParseError::MissingField("col"))?,
                cols: w.ok_or(RgpParseError::MissingField("w"))?,
                rows: h.ok_or(RgpParseError::MissingField("h"))?,
            },
            style: RgpPlacementStyle {
                animate: animate.unwrap_or(false),
                scale: scale.unwrap_or(1.0),
                depth: depth.unwrap_or(0.0),
                color,
                brightness: brightness.unwrap_or(1.0),
                offset: [px.unwrap_or(0.0), py.unwrap_or(0.0), pz.unwrap_or(0.0)],
                rotation: [rx.unwrap_or(0.0), ry.unwrap_or(0.0), rz.unwrap_or(0.0)],
                scale3: [sx.unwrap_or(1.0), sy.unwrap_or(1.0), sz.unwrap_or(1.0)],
            },
        }),
        "u" => Ok(RgpOperation::Update {
            id: id.ok_or(RgpParseError::MissingField("id"))?,
            update: RgpPlacementUpdate {
                animate,
                scale,
                depth,
                color,
                brightness,
                offset: [px, py, pz],
                rotation: [rx, ry, rz],
                scale3: [sx, sy, sz],
            },
        }),
        "d" => Ok(RgpOperation::Delete { id }),
        other => Err(RgpParseError::UnknownVerb(other.to_string())),
    }
}

// Owned `Option<String>` parameters are passed by value because the
// happy path moves them into `RgpRegisterSource::{Path, Payload}`;
// the failure paths just drop them. Taking by reference would force
// a clone on success.
#[allow(clippy::needless_pass_by_value)]
fn parse_register(
    id: Option<u32>,
    format: Option<String>,
    path: Option<String>,
    source: Option<String>,
    more: Option<bool>,
    name: Option<String>,
    payload_chunk: Option<&str>,
) -> Result<RgpOperation, RgpParseError> {
    let id = id.ok_or(RgpParseError::MissingField("id"))?;
    let fmt_str = format.ok_or(RgpParseError::MissingField("fmt"))?;
    let format = match fmt_str.as_str() {
        "glb" => RgpFormat::Glb,
        "obj" => RgpFormat::Obj,
        other => return Err(RgpParseError::UnsupportedFormat(other.to_string())),
    };

    // Path-based: `path=<name>` and no `source=payload`.
    if let Some(name) = path {
        return Ok(RgpOperation::Register {
            id,
            format,
            source: RgpRegisterSource::Path { name },
        });
    }

    // Payload-based: `source=payload` and a trailing base64 chunk.
    // `payload_chunk` may be empty for an explicit empty chunk (rare,
    // but the spec doesn't forbid it).
    if source.as_deref() == Some("payload") {
        let chunk = payload_chunk.unwrap_or("");
        let data = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            chunk,
        )
        .map_err(|_| RgpParseError::BadBase64)?;
        return Ok(RgpOperation::Register {
            id,
            format,
            source: RgpRegisterSource::Payload {
                name,
                more: more.unwrap_or(false),
                data,
            },
        });
    }

    Err(RgpParseError::MissingField("path or source=payload"))
}

fn parse_u32(key: &'static str, value: &str) -> Result<u32, RgpParseError> {
    value.parse().map_err(|_| RgpParseError::MalformedField(key))
}

fn parse_u16(key: &'static str, value: &str) -> Result<u16, RgpParseError> {
    value.parse().map_err(|_| RgpParseError::MalformedField(key))
}

fn parse_f32(key: &'static str, value: &str) -> Result<f32, RgpParseError> {
    value.parse().map_err(|_| RgpParseError::MalformedField(key))
}

fn parse_bool(key: &'static str, value: &str) -> Result<bool, RgpParseError> {
    match value {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => Err(RgpParseError::MalformedField(key)),
    }
}

fn parse_color(value: &str) -> Result<[u8; 3], RgpParseError> {
    let v = value.strip_prefix('#').unwrap_or(value);
    if v.len() != 6 {
        return Err(RgpParseError::MalformedField("color"));
    }
    let r = u8::from_str_radix(&v[0..2], 16).map_err(|_| RgpParseError::MalformedField("color"))?;
    let g = u8::from_str_radix(&v[2..4], 16).map_err(|_| RgpParseError::MalformedField("color"))?;
    let b = u8::from_str_radix(&v[4..6], 16).map_err(|_| RgpParseError::MalformedField("color"))?;
    Ok([r, g, b])
}
