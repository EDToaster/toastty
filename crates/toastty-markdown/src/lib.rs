//! Markdown rich blocks: lay out and rasterize a Markdown document into
//! an RGBA image whose height is a whole number of terminal rows, plus
//! the plain text and links that sit "under" each row (so selection,
//! copy, search and Ctrl+click keep working on the rendered block).
//!
//! This crate knows nothing about the terminal grid or the GPU. The
//! terminal hands it the Markdown source and a [`LayoutParams`]
//! (available width, cell size, base font size, theme, resource policy)
//! and gets a [`RenderedBlock`] back. See `docs/markdown.md` for the
//! protocol and the overall design.
//!
//! Modules:
//! - [`layout`]: Markdown → boxes → pixels (headings, emphasis, lists,
//!   task lists, quotes, code with highlighting, tables, links, rules,
//!   images, inline and display maths, Mermaid).
//! - [`math`]: LaTeX maths → [`Raster`] (RaTeX).
//! - [`mermaid`]: Mermaid source → [`Raster`] (mermaid-rs-renderer + resvg).

pub mod layout;
pub mod math;
pub mod mermaid;

use std::path::PathBuf;

/// Straight-alpha RGBA8 colour.
pub type Rgba = [u8; 4];

/// A rasterized image: tightly packed, row-major, top-down RGBA8 with
/// **straight** (non-premultiplied) alpha — the same layout as
/// `toastty_graphics::ImageData`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// For inline content (inline maths): distance in pixels from the
    /// top of the raster to the baseline, so it can sit on the text
    /// baseline. `None` for block content.
    pub baseline: Option<u32>,
}

/// Colours used to draw a block. Derived by the terminal from its
/// current palette so blocks match the user's theme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Default text colour.
    pub fg: Rgba,
    /// Terminal background. The block's own background is transparent
    /// except where it draws panels (code, quotes, tables), so this is
    /// for contrast decisions and for opaque sub-renders (diagrams).
    pub bg: Rgba,
    /// The 16 ANSI colours (0–15), for accents and syntax highlighting.
    pub ansi: [Rgba; 16],
}

impl Default for Theme {
    fn default() -> Self {
        // Matches toastty's default dark palette closely enough for
        // tests; the terminal always passes its real palette.
        let ansi = [
            [0x1d, 0x1f, 0x21, 0xff],
            [0xcc, 0x66, 0x66, 0xff],
            [0xb5, 0xbd, 0x68, 0xff],
            [0xf0, 0xc6, 0x74, 0xff],
            [0x81, 0xa2, 0xbe, 0xff],
            [0xb2, 0x94, 0xbb, 0xff],
            [0x8a, 0xbe, 0xb7, 0xff],
            [0xc5, 0xc8, 0xc6, 0xff],
            [0x66, 0x66, 0x66, 0xff],
            [0xd5, 0x4e, 0x53, 0xff],
            [0xb9, 0xca, 0x4a, 0xff],
            [0xe7, 0xc5, 0x47, 0xff],
            [0x7a, 0xa6, 0xda, 0xff],
            [0xc3, 0x97, 0xd8, 0xff],
            [0x70, 0xc0, 0xb1, 0xff],
            [0xea, 0xea, 0xea, 0xff],
        ];
        Self {
            fg: [0xd8, 0xd8, 0xd8, 0xff],
            bg: [0x14, 0x14, 0x18, 0xff],
            ansi,
        }
    }
}

/// Where images referenced by the document may come from. Remote URLs
/// (`http:`, `https:` …) are never fetched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourcePolicy {
    /// Allow `![](relative/or/absolute/path.png)` to read local files.
    /// Off by default: the block arrives over the PTY, possibly from a
    /// remote host, and must not make the terminal read the user's
    /// files. `data:` URIs are always allowed.
    pub allow_local_files: bool,
    /// Directory relative image paths resolve against (the sender's
    /// working directory, from the protocol's `cwd=` key).
    pub base_dir: Option<PathBuf>,
}

/// Everything layout needs from the terminal.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutParams {
    /// Number of terminal columns the block spans (the full grid width).
    pub cols: u16,
    /// Cell size in physical pixels. The block is `cols * cell_w` wide
    /// and `rows * cell_h` tall.
    pub cell_w: u32,
    pub cell_h: u32,
    /// Base body font size in physical pixels (the terminal font size
    /// times the display scale). Headings etc. scale from this.
    pub font_px: f32,
    pub theme: Theme,
    pub resources: ResourcePolicy,
    /// Hard cap on rows (DoS guard). Content past it is cut and a
    /// "… truncated" line is drawn on the last row.
    pub max_rows: u16,
}

impl LayoutParams {
    /// Block width in pixels.
    #[must_use]
    pub fn width_px(&self) -> u32 {
        u32::from(self.cols) * self.cell_w
    }
}

/// One run of plain text placed on a terminal row, starting at `col`.
/// The terminal writes it into the cells under the block so selection
/// and copy return the rendered text. Each `char` of `text` occupies
/// one cell (wide chars two), clipped to the grid width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRun {
    pub col: u16,
    pub text: String,
}

/// A clickable link, expressed in cells of the block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    pub row: u16,
    pub cols: std::ops::Range<u16>,
    pub url: String,
}

/// The result of laying out a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedBlock {
    /// Rows the block occupies (`image.height == rows * cell_h`).
    pub rows: u16,
    /// The rendered block, `cols * cell_w` × `rows * cell_h`.
    pub image: Raster,
    /// Plain text per row (`text[r]` are the runs on block row `r`);
    /// `text.len() == rows`.
    pub text: Vec<Vec<TextRun>>,
    pub links: Vec<LinkSpan>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid parameters: {0}")]
    Params(&'static str),
    #[error("render failed: {0}")]
    Render(String),
}

/// Owns the fonts and caches (syntax sets, font database) a render
/// needs. Create once and reuse: building one loads system fonts.
pub struct Renderer {
    fonts: cosmic_text::FontSystem,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer").finish_non_exhaustive()
    }
}

impl Renderer {
    /// Renderer using the system fonts plus the bundled fallbacks.
    #[must_use]
    pub fn new() -> Self {
        Self::with_font_system(cosmic_text::FontSystem::new())
    }

    /// Renderer over a caller-built font system. Tests use this with a
    /// database holding only bundled fonts so output is identical on
    /// every machine.
    #[must_use]
    pub fn with_font_system(fonts: cosmic_text::FontSystem) -> Self {
        Self { fonts }
    }

    /// Lay out and rasterize `markdown`.
    ///
    /// # Errors
    /// [`Error::Params`] for a zero-sized cell or zero columns.
    pub fn render(&mut self, markdown: &str, params: &LayoutParams) -> Result<RenderedBlock, Error> {
        if params.cols == 0 || params.cell_w == 0 || params.cell_h == 0 {
            return Err(Error::Params("zero-sized grid"));
        }
        layout::render(&mut self.fonts, markdown, params)
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}
