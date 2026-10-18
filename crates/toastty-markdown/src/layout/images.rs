//! Image sources and the resource policy: `data:` URIs always, local
//! files only when allowed, remote URLs never.

use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine as _;

use crate::{Raster, ResourcePolicy};

/// Decoded images may not exceed this many bytes (RGBA8).
pub(crate) const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;
/// Encoded file size cap for local images.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DIM: u32 = 16_384;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Denied {
    /// `http:`, `https:` and any other URL scheme: never fetched.
    Remote,
    /// A local path while local files are disabled.
    LocalDisabled,
    /// Could not read or decode the image.
    Failed(String),
}

fn decode(bytes: &[u8]) -> Result<Raster, Denied> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Denied::Failed(e.to_string()))?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| Denied::Failed(e.to_string()))?;
    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return Err(Denied::Failed("empty image".into()));
    }
    Ok(Raster {
        width,
        height,
        pixels: rgba.into_raw(),
        baseline: None,
    })
}

fn data_uri(url: &str) -> Result<Raster, Denied> {
    let rest = &url[5..];
    let (meta, payload) = rest
        .split_once(',')
        .ok_or_else(|| Denied::Failed("bad data: URI".into()))?;
    if !meta.ends_with(";base64") {
        return Err(Denied::Failed(
            "only base64 data: URIs are supported".into(),
        ));
    }
    let mime = meta.trim_end_matches(";base64").to_ascii_lowercase();
    if !mime.is_empty() && !mime.starts_with("image/") {
        return Err(Denied::Failed("not an image".into()));
    }
    let clean: String = payload
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(clean.trim_end_matches('='))
        .or_else(|_| {
            base64::engine::general_purpose::STANDARD_NO_PAD.decode(clean.trim_end_matches('='))
        })
        .map_err(|e| Denied::Failed(e.to_string()))?;
    decode(&bytes)
}

fn has_scheme(url: &str) -> bool {
    // `scheme:` with a scheme of 2+ chars (so `C:\foo` stays a path).
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    scheme.len() >= 2
        && scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(hex) = std::str::from_utf8(&b[i + 1..i + 3])
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The local path `url` refers to, if it is a local reference.
fn local_path(url: &str, base: Option<&Path>) -> Option<PathBuf> {
    let raw = if let Some(rest) = url.strip_prefix("file://") {
        rest.strip_prefix("localhost").unwrap_or(rest).to_owned()
    } else if has_scheme(url) {
        return None;
    } else {
        url.to_owned()
    };
    let raw = raw.split(['?', '#']).next().unwrap_or("");
    let p = PathBuf::from(percent_decode(raw));
    if p.is_absolute() {
        Some(p)
    } else {
        // Relative paths need a base directory; never resolve against
        // the terminal's own working directory.
        base.map(|b| b.join(p))
    }
}

/// Load the image `url` under `policy`.
pub(crate) fn load(url: &str, policy: &ResourcePolicy) -> Result<Raster, Denied> {
    let url = url.trim();
    if url.len() > 5 && url[..5].eq_ignore_ascii_case("data:") {
        return data_uri(url);
    }
    // Protocol-relative `//host/x` is remote too.
    if (has_scheme(url) && !url.starts_with("file://")) || url.starts_with("//") {
        return Err(Denied::Remote);
    }
    if !policy.allow_local_files {
        return Err(Denied::LocalDisabled);
    }
    let path = local_path(url, policy.base_dir.as_deref())
        .ok_or_else(|| Denied::Failed("relative path with no base directory".into()))?;
    let file = std::fs::File::open(&path).map_err(|e| Denied::Failed(e.to_string()))?;
    let meta = file.metadata().map_err(|e| Denied::Failed(e.to_string()))?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Err(Denied::Failed("not a regular file or too large".into()));
    }
    let mut bytes = vec![];
    file.take(MAX_FILE_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| Denied::Failed(e.to_string()))?;
    decode(&bytes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A tiny PNG (`w`×`h`, solid colour) encoded with the image crate.
    pub(crate) fn png(w: u32, h: u32, c: [u8; 4]) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba(c));
        let mut out = std::io::Cursor::new(vec![]);
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    pub(crate) fn data_png(w: u32, h: u32, c: [u8; 4]) -> String {
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png(w, h, c))
        )
    }

    #[test]
    fn data_uri_decodes() {
        let r = load(&data_png(3, 2, [1, 2, 3, 255]), &ResourcePolicy::default()).unwrap();
        assert_eq!((r.width, r.height), (3, 2));
        assert_eq!(&r.pixels[..4], &[1, 2, 3, 255]);
    }

    #[test]
    fn remote_is_never_fetched() {
        let allow = ResourcePolicy {
            allow_local_files: true,
            base_dir: None,
        };
        for u in [
            "https://example.com/a.png",
            "http://x/y.png",
            "ftp://h/f.png",
            "//cdn/x.png",
        ] {
            let r = load(u, &allow);
            assert_eq!(r, Err(Denied::Remote), "{u}");
        }
        assert_eq!(
            load("https://example.com/a.png", &allow),
            Err(Denied::Remote)
        );
    }

    #[test]
    fn local_files_need_opt_in() {
        let dir = std::env::temp_dir().join(format!("toastty-md-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a b.png"), png(4, 4, [9, 9, 9, 255])).unwrap();
        assert_eq!(
            load("a%20b.png", &ResourcePolicy::default()),
            Err(Denied::LocalDisabled)
        );
        let abs = dir.join("a b.png");
        assert_eq!(
            load(abs.to_str().unwrap(), &ResourcePolicy::default()),
            Err(Denied::LocalDisabled)
        );
        let allow = ResourcePolicy {
            allow_local_files: true,
            base_dir: Some(dir.clone()),
        };
        assert_eq!(load("a%20b.png", &allow).unwrap().width, 4);
        assert_eq!(load(abs.to_str().unwrap(), &allow).unwrap().height, 4);
        // Relative without a base directory is refused.
        let nobase = ResourcePolicy {
            allow_local_files: true,
            base_dir: None,
        };
        assert!(matches!(load("a%20b.png", &nobase), Err(Denied::Failed(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_fails_cleanly() {
        assert!(matches!(
            load("data:image/png;base64,AAAA", &ResourcePolicy::default()),
            Err(Denied::Failed(_))
        ));
        assert!(matches!(
            load("data:text/plain;base64,AAAA", &ResourcePolicy::default()),
            Err(Denied::Failed(_))
        ));
    }
}
