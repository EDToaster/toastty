//! User shader source → validated WGSL plus what it reads.
//!
//! GPU-free, so the prelude, the error mapping and the analysis are unit
//! tested without a device. The GPU side ([`super::PostChain`]) takes the
//! WGSL from here.
//!
//! GLSL gets a prelude before the user's file (`#version` and the
//! declarations must come first), so a line in the wrapped source maps
//! back to the user's file by subtracting the prelude's line count, and
//! an epilogue after it (its `main()`, which must follow `mainImage`:
//! naga wants callees declared before their callers, so a forward
//! prototype in the prelude doesn't work once `mainImage` calls helpers).
//! WGSL gets everything after the user's file: module-scope order doesn't
//! matter there, and directives (`enable`, `requires`, `diagnostic`) must
//! come before any declaration. Errors outside the user's file have no
//! user line.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};

use naga::valid::{Capabilities, ValidationFlags, Validator};
use naga::{Module, ShaderStage, SourceLocation, Span};

use super::analysis;
use super::uniforms::MEMBERS;

/// A post-process shader to compile: its path (picks the language by
/// extension and names it in errors) and its source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostShaderSource {
    /// Where the shader came from. `.glsl` / `.frag` are GLSL, `.wgsl`
    /// is WGSL.
    pub path: PathBuf,
    /// The file's contents.
    pub source: String,
}

/// A shader that failed to compile. Locations are in the user's file
/// (1-based), not in the prelude-wrapped source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaderError {
    /// The shader's path, as given in [`PostShaderSource::path`].
    pub path: PathBuf,
    /// Line in the user's file, when the error points into it.
    pub line: Option<u32>,
    /// Column (1-based, in bytes) on `line`.
    pub column: Option<u32>,
    /// What went wrong.
    pub message: String,
}

impl fmt::Display for ShaderError {
    /// `path:line:col: message`, dropping the parts that are unknown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path.display())?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
            if let Some(col) = self.column {
                write!(f, ":{col}")?;
            }
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for ShaderError {}

/// Shader language, from the file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Language {
    Glsl,
    Wgsl,
}

impl Language {
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "glsl" | "frag" => Some(Self::Glsl),
            "wgsl" => Some(Self::Wgsl),
            _ => None,
        }
    }
}

/// What a compiled shader reads, for the animation policy.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct ShaderInputs {
    /// Bit mask over [`MEMBERS`] of the uniforms the shader reads.
    pub used: u64,
    /// `TOASTTY_EVENT_DURATION`, in seconds.
    pub event_duration: Option<f32>,
}

impl ShaderInputs {
    /// True when the shader reads any of the members in `mask`.
    pub(crate) fn reads(&self, mask: u64) -> bool {
        self.used & mask != 0
    }

    /// The union of two shaders' needs (a chain needs what any of its
    /// shaders needs; the longest duration wins).
    pub(crate) fn union(self, other: Self) -> Self {
        Self {
            used: self.used | other.used,
            event_duration: match (self.event_duration, other.event_duration) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
        }
    }
}

/// A shader ready for `wgpu`: WGSL source with a fragment entry point.
#[derive(Debug, Clone)]
pub(crate) struct CompiledShader {
    pub path: PathBuf,
    pub wgsl: String,
    pub entry_point: String,
    pub inputs: ShaderInputs,
}

/// The user's source between the prelude and the epilogue.
#[derive(Debug)]
pub(crate) struct Wrapped {
    pub text: String,
    /// Lines before the user's first line.
    pub prelude_lines: u32,
    /// Lines in the user's file.
    pub user_lines: u32,
}

impl Wrapped {
    fn new(prelude: &str, user: &str, epilogue: &str) -> Self {
        debug_assert!(prelude.is_empty() || prelude.ends_with('\n'));
        let prelude_lines = count_lines(prelude);
        let mut text = String::with_capacity(prelude.len() + user.len() + epilogue.len() + 1);
        text.push_str(prelude);
        text.push_str(user);
        let mut user_lines = count_lines(user);
        if !user.ends_with('\n') {
            text.push('\n');
            user_lines += 1;
        }
        text.push_str(epilogue);
        Self {
            text,
            prelude_lines,
            user_lines,
        }
    }

    /// Map a location in the wrapped source to `(line, column)` in the
    /// user's file. `None` when it falls in the prelude.
    pub(crate) fn user_location(&self, loc: SourceLocation) -> Option<(u32, u32)> {
        let line = loc.line_number.checked_sub(self.prelude_lines)?;
        (1..=self.user_lines)
            .contains(&line)
            .then_some((line, loc.line_position))
    }

    fn span_location(&self, span: Span) -> Option<(u32, u32)> {
        if !span.is_defined() || span.to_range()?.end > self.text.len() {
            return None;
        }
        self.user_location(span.location(&self.text))
    }
}

fn count_lines(s: &str) -> u32 {
    u32::try_from(s.bytes().filter(|&b| b == b'\n').count()).unwrap_or(u32::MAX)
}

/// The GLSL prelude: Shadertoy / Ghostty globals as an unnamed uniform
/// block (so `iTime` names the member directly and nothing is copied per
/// fragment) and `iChannel0` as a texture + sampler pair.
pub(crate) fn glsl_prelude() -> String {
    let mut s = String::from("#version 450 core\n");
    s.push_str("layout(set = 0, binding = 0, std140) uniform ToasttyUniforms {\n");
    for m in MEMBERS {
        let _ = writeln!(s, "    {} {}{};", m.glsl, m.name, m.glsl_array);
    }
    s.push_str("};\n");
    s.push_str(
        "layout(set = 0, binding = 1) uniform texture2D iChannel0_tex;\n\
         layout(set = 0, binding = 2) uniform sampler iChannel0_smp;\n\
         #define iChannel0 sampler2D(iChannel0_tex, iChannel0_smp)\n\
         #define texture2D texture\n\
         #define CURSORSTYLE_BLOCK 0\n\
         #define CURSORSTYLE_BLOCK_HOLLOW 1\n\
         #define CURSORSTYLE_BAR 2\n\
         #define CURSORSTYLE_UNDERLINE 3\n\
         #define CURSORSTYLE_LOCK 4\n\
         layout(location = 0) out vec4 toastty_frag_color;\n",
    );
    s
}

/// The GLSL epilogue: `main()` hands `gl_FragCoord.xy` (y down, pixel
/// centres at .5) to `mainImage` unchanged.
const GLSL_EPILOGUE: &str = "void main() {\n\
                             \x20   vec4 color = vec4(0.0);\n\
                             \x20   mainImage(color, gl_FragCoord.xy);\n\
                             \x20   toastty_frag_color = color;\n\
                             }\n";

/// The WGSL prelude (appended after the user's file): the same block as
/// `var<uniform> toastty`, the `iChannel0` texture and sampler, the
/// cursor-style constants and the fragment entry point.
pub(crate) fn wgsl_prelude() -> String {
    let mut s = String::from("struct ToasttyUniforms {\n");
    for m in MEMBERS {
        let _ = writeln!(s, "    {}: {},", m.name, m.wgsl);
    }
    s.push_str("};\n");
    s.push_str(
        "@group(0) @binding(0) var<uniform> toastty: ToasttyUniforms;\n\
         @group(0) @binding(1) var iChannel0: texture_2d<f32>;\n\
         @group(0) @binding(2) var iChannel0Sampler: sampler;\n\
         const CURSORSTYLE_BLOCK: i32 = 0;\n\
         const CURSORSTYLE_BLOCK_HOLLOW: i32 = 1;\n\
         const CURSORSTYLE_BAR: i32 = 2;\n\
         const CURSORSTYLE_UNDERLINE: i32 = 3;\n\
         const CURSORSTYLE_LOCK: i32 = 4;\n\
         @fragment\n\
         fn toastty_fs_main(@builtin(position) frag_coord: vec4f) -> @location(0) vec4f {\n\
         \x20   return mainImage(frag_coord.xy);\n\
         }\n",
    );
    s
}

/// WGSL entry point name the prelude declares.
const WGSL_ENTRY: &str = "toastty_fs_main";

/// Blank every `#version` line (the prelude has its own), keeping the
/// line itself so line numbers stay put.
pub(crate) fn blank_version_lines(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        if line.trim_start().starts_with("#version") {
            // Keep the line break (`\n` or `\r\n`).
            let body = line.trim_end_matches(['\r', '\n']);
            out.push_str(&line[body.len()..]);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Wrap a GLSL user file.
pub(crate) fn wrap_glsl(user: &str) -> Wrapped {
    Wrapped::new(&glsl_prelude(), &blank_version_lines(user), GLSL_EPILOGUE)
}

/// Wrap a WGSL user file.
pub(crate) fn wrap_wgsl(user: &str) -> Wrapped {
    Wrapped::new("", user, &wgsl_prelude())
}

/// Compile one shader: parse, validate, analyse, and (for GLSL) emit
/// WGSL. Every error found is returned.
pub(crate) fn compile(src: &PostShaderSource) -> Result<CompiledShader, Vec<ShaderError>> {
    let err = |line: Option<(u32, u32)>, message: String| ShaderError {
        path: src.path.clone(),
        line: line.map(|l| l.0),
        column: line.map(|l| l.1),
        message,
    };
    let Some(lang) = Language::from_path(&src.path) else {
        return Err(vec![err(
            None,
            "unknown shader type: use a .glsl, .frag or .wgsl file".to_owned(),
        )]);
    };
    let wrapped = match lang {
        Language::Glsl => wrap_glsl(&src.source),
        Language::Wgsl => wrap_wgsl(&src.source),
    };
    let module = match lang {
        Language::Glsl => {
            let mut frontend = naga::front::glsl::Frontend::default();
            frontend
                .parse(
                    &naga::front::glsl::Options::from(ShaderStage::Fragment),
                    &wrapped.text,
                )
                .map_err(|errors| {
                    errors
                        .errors
                        .iter()
                        .map(|e| {
                            let loc = wrapped.span_location(e.meta);
                            err(loc, explain(lang, &e.kind.to_string(), loc.is_some()))
                        })
                        .collect::<Vec<_>>()
                })?
        }
        Language::Wgsl => naga::front::wgsl::parse_str(&wrapped.text).map_err(|e| {
            let loc = e.labels().find_map(|(span, _)| wrapped.span_location(span));
            let mut message = e.message().to_owned();
            if let Some((_, label)) = e.labels().next()
                && !label.is_empty()
                && label != message
            {
                message = format!("{message} ({label})");
            }
            vec![err(loc, explain(lang, &message, loc.is_some()))]
        })?,
    };
    let info = Validator::new(ValidationFlags::all(), Capabilities::default())
        .validate(&module)
        .map_err(|e| {
            let loc = pick_location(&wrapped, e.spans().map(|(span, _)| span));
            vec![err(
                loc,
                explain(lang, &error_chain(e.as_inner()), loc.is_some()),
            )]
        })?;
    let entry_point = match lang {
        Language::Glsl => module
            .entry_points
            .iter()
            .find(|ep| ep.stage == ShaderStage::Fragment)
            .map_or_else(|| "main".to_owned(), |ep| ep.name.clone()),
        Language::Wgsl => WGSL_ENTRY.to_owned(),
    };
    let inputs = inputs_of(&module, &entry_point);
    let wgsl = match lang {
        Language::Glsl => {
            naga::back::wgsl::write_string(&module, &info, naga::back::wgsl::WriterFlags::empty())
                .map_err(|e| vec![err(None, format!("internal: GLSL → WGSL failed: {e}"))])?
        }
        Language::Wgsl => wrapped.text,
    };
    Ok(CompiledShader {
        path: src.path.clone(),
        wgsl,
        entry_point,
        inputs,
    })
}

/// [`ShaderInputs`] of a validated module.
///
/// The analysis runs on a compacted copy: naga's GLSL frontend leaves a
/// dead `AccessIndex` for every block member in every function, and
/// compaction drops expressions no statement uses (and functions nothing
/// calls).
pub(crate) fn inputs_of(module: &Module, entry: &str) -> ShaderInputs {
    let mut module = module.clone();
    naga::compact::compact(&mut module, naga::compact::KeepUnused::No);
    let module = &module;
    let block = module.global_variables.iter().find_map(|(h, g)| {
        g.binding
            .as_ref()
            .is_some_and(|b| b.group == 0 && b.binding == 0)
            .then_some(h)
    });
    ShaderInputs {
        used: block.map_or(0, |b| analysis::uniforms_read(module, b, entry)),
        event_duration: analysis::event_duration(module),
    }
}

/// An error and its `source()` chain, joined with `: ` (naga's
/// validation errors nest: function → expression → cause).
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(inner) = cur {
        let part = inner.to_string();
        if !s.contains(&part) {
            s.push_str(": ");
            s.push_str(&part);
        }
        cur = inner.source();
    }
    s
}

/// What the error says when `mainImage` is missing or has the wrong
/// signature.
const GLSL_SIGNATURE: &str =
    "the shader must define `void mainImage(out vec4 fragColor, in vec2 fragCoord)`";
const WGSL_SIGNATURE: &str = "the shader must define `fn mainImage(fragCoord: vec2f) -> vec4f`";

/// Rewrite messages that commonly need it. An error outside the user's
/// file that involves `mainImage` (or the WGSL entry point calling it)
/// means a missing `mainImage` or one with the wrong signature: say what
/// it must look like.
fn explain(lang: Language, message: &str, in_user_file: bool) -> String {
    if ["iChannel1", "iChannel2", "iChannel3"]
        .iter()
        .any(|c| message.contains(c))
    {
        return format!("{message} (only iChannel0, the terminal image, is available)");
    }
    if !in_user_file {
        match lang {
            Language::Glsl if message.contains("mainImage") => return GLSL_SIGNATURE.to_owned(),
            Language::Wgsl if message.contains("mainImage") || message.contains(WGSL_ENTRY) => {
                return WGSL_SIGNATURE.to_owned();
            }
            _ => {}
        }
    }
    message.to_owned()
}

/// The location to report for an error with these spans: the narrowest
/// span in the user's file (the others are the enclosing function and the
/// like, or the prelude). `None` when no span is in the user's file.
fn pick_location<'a>(
    wrapped: &Wrapped,
    spans: impl Iterator<Item = &'a Span>,
) -> Option<(u32, u32)> {
    spans
        .filter_map(|span| {
            let loc = wrapped.span_location(*span)?;
            Some((span.to_range()?.len(), loc))
        })
        .min_by_key(|(len, _)| *len)
        .map(|(_, loc)| loc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post::uniforms::{bell_mask, focus_mask, mask_of, mouse_mask, time_mask};

    fn src(name: &str, source: &str) -> PostShaderSource {
        PostShaderSource {
            path: PathBuf::from(name),
            source: source.to_owned(),
        }
    }

    fn compile_ok(name: &str, source: &str) -> CompiledShader {
        match compile(&src(name, source)) {
            Ok(c) => c,
            Err(errs) => panic!("{name} failed: {errs:?}"),
        }
    }

    fn compile_err(name: &str, source: &str) -> Vec<ShaderError> {
        compile(&src(name, source)).expect_err("should fail")
    }

    const GLSL_PASS: &str = "void mainImage(out vec4 fragColor, in vec2 fragCoord) {\n\
                             \x20   fragColor = texture(iChannel0, fragCoord / iResolution.xy);\n\
                             }\n";

    const WGSL_PASS: &str = "fn mainImage(fragCoord: vec2f) -> vec4f {\n\
                             \x20   return textureSample(iChannel0, iChannel0Sampler, fragCoord / toastty.iResolution.xy);\n\
                             }\n";

    /// naga's offsets for the block (std140 from the GLSL prelude, the
    /// uniform address-space layout from the WGSL one) match
    /// [`crate::post::uniforms::PostUniforms`].
    #[test]
    fn uniform_layout_matches_both_preludes() {
        for (name, source) in [("l.glsl", GLSL_PASS), ("l.wgsl", WGSL_PASS)] {
            let w = match Language::from_path(Path::new(name)).unwrap() {
                Language::Glsl => wrap_glsl(source),
                Language::Wgsl => wrap_wgsl(source),
            };
            let module = if name.ends_with("glsl") {
                naga::front::glsl::Frontend::default()
                    .parse(
                        &naga::front::glsl::Options::from(ShaderStage::Fragment),
                        &w.text,
                    )
                    .unwrap()
            } else {
                naga::front::wgsl::parse_str(&w.text).unwrap()
            };
            let (_, block) = module
                .global_variables
                .iter()
                .find(|(_, g)| g.binding.as_ref().is_some_and(|b| b.binding == 0))
                .unwrap();
            let naga::TypeInner::Struct { members, span } = &module.types[block.ty].inner else {
                panic!("block is not a struct");
            };
            assert_eq!(members.len(), MEMBERS.len(), "{name}");
            for (m, expected) in members.iter().zip(MEMBERS) {
                assert_eq!(m.name.as_deref(), Some(expected.name), "{name}");
                assert_eq!(
                    m.offset as usize, expected.offset,
                    "{name}: {}",
                    expected.name
                );
            }
            assert_eq!(
                *span as usize,
                size_of::<crate::post::uniforms::PostUniforms>(),
                "{name}"
            );
        }
    }

    #[test]
    fn language_from_extension() {
        assert_eq!(
            Language::from_path(Path::new("a.glsl")),
            Some(Language::Glsl)
        );
        assert_eq!(
            Language::from_path(Path::new("a.FRAG")),
            Some(Language::Glsl)
        );
        assert_eq!(
            Language::from_path(Path::new("a.wgsl")),
            Some(Language::Wgsl)
        );
        assert_eq!(Language::from_path(Path::new("a.txt")), None);
        assert_eq!(Language::from_path(Path::new("glsl")), None);
    }

    #[test]
    fn display_formats_known_parts() {
        let mut e = ShaderError {
            path: PathBuf::from("/s/crt.glsl"),
            line: Some(3),
            column: Some(7),
            message: "bad".to_owned(),
        };
        assert_eq!(e.to_string(), "/s/crt.glsl:3:7: bad");
        e.column = None;
        assert_eq!(e.to_string(), "/s/crt.glsl:3: bad");
        e.line = None;
        assert_eq!(e.to_string(), "/s/crt.glsl: bad");
    }

    #[test]
    fn prelude_keeps_user_line_numbers() {
        let user = "// one\n// two\nvoid mainImage(out vec4 c, in vec2 p) { c = vec4(1.0); }\n";
        let w = wrap_glsl(user);
        let line3 = w.text.lines().nth(w.prelude_lines as usize + 2).unwrap();
        assert!(line3.starts_with("void mainImage"), "{line3}");
        let w = wrap_wgsl(user);
        assert_eq!(w.prelude_lines, 0);
        assert!(w.text.starts_with(user));
    }

    #[test]
    fn version_line_is_blanked_in_place() {
        let user = "#version 300 es\nfoo\n  #version 450\r\nbar";
        let out = blank_version_lines(user);
        assert_eq!(out, "\nfoo\n\r\nbar");
        // And a shader with its own #version still compiles.
        compile_ok("v.glsl", &format!("#version 330\n{GLSL_PASS}"));
    }

    #[test]
    fn glsl_error_points_at_the_user_line() {
        let errs = compile_err(
            "bad.glsl",
            "// comment\n\nvoid mainImage(out vec4 c, in vec2 p) {\n    c = vec4(nope);\n}\n",
        );
        let e = &errs[0];
        assert_eq!(e.line, Some(4), "{e}");
        assert!(e.column.is_some());
        assert!(e.message.contains("nope"), "{e}");
        assert!(e.to_string().starts_with("bad.glsl:4:"), "{e}");
    }

    #[test]
    fn wgsl_error_points_at_the_user_line() {
        let errs = compile_err(
            "bad.wgsl",
            "\nfn mainImage(p: vec2f) -> vec4f {\n    return vec4f(nope);\n}\n",
        );
        let e = &errs[0];
        assert_eq!(e.line, Some(3), "{e}");
        assert!(e.message.contains("nope"), "{e}");
    }

    #[test]
    fn wgsl_validation_error_points_at_the_user_line() {
        // Parses, but fails validation (returns the wrong type).
        let errs = compile_err(
            "bad.wgsl",
            "fn mainImage(p: vec2f) -> vec4f {\n    let unused = p;\n    return 1.0;\n}\n",
        );
        let e = &errs[0];
        assert_eq!(e.line, Some(3), "{e}");
    }

    #[test]
    fn ichannel1_gives_a_clean_error() {
        let errs = compile_err(
            "multi.glsl",
            "void mainImage(out vec4 c, in vec2 p) {\n    c = texture(iChannel1, p);\n}\n",
        );
        let e = &errs[0];
        assert_eq!(e.line, Some(2), "{e}");
        assert!(e.message.contains("iChannel1"), "{e}");
        assert!(e.message.contains("only iChannel0"), "{e}");
    }

    #[test]
    fn missing_or_mistyped_main_image_is_explained() {
        for (name, source) in [
            ("missing.wgsl", "fn other() {}\n"),
            (
                "ret.wgsl",
                "fn mainImage(p: vec2f) -> vec3f { return vec3f(1.0); }\n",
            ),
            (
                "args.wgsl",
                "fn mainImage() -> vec4f { return vec4f(1.0); }\n",
            ),
        ] {
            let errs = compile_err(name, source);
            assert_eq!(errs[0].line, None, "{name}");
            assert_eq!(errs[0].message, WGSL_SIGNATURE, "{name}");
        }
        for (name, source) in [
            ("missing.glsl", "void other() {}\n"),
            (
                "args.glsl",
                "void mainImage(out vec4 c) { c = vec4(1.0); }\n",
            ),
            ("ret.glsl", "vec4 mainImage(vec2 p) { return vec4(1.0); }\n"),
        ] {
            let errs = compile_err(name, source);
            assert_eq!(errs[0].line, None, "{name}");
            assert_eq!(errs[0].message, GLSL_SIGNATURE, "{name}");
        }
    }

    #[test]
    fn wgsl_directives_compile() {
        compile_ok("enable.wgsl", &format!("enable f16;\n{WGSL_PASS}"));
        compile_ok(
            "requires.wgsl",
            &format!(
                "requires pointer_composite_access;\ndiagnostic(off, derivative_uniformity);\n{WGSL_PASS}"
            ),
        );
        // Lines still map one to one.
        let errs = compile_err(
            "enable.wgsl",
            "enable f16;\n\nfn mainImage(p: vec2f) -> vec4f {\n    return nope;\n}\n",
        );
        assert_eq!(errs[0].line, Some(4), "{}", errs[0]);
    }

    #[test]
    fn location_prefers_the_narrowest_span_in_the_user_file() {
        // User line 2 holds a function header and a narrower expression;
        // the narrowest span of all is in the WGSL prelude (after the
        // user's lines).
        let user = "// x\nfn mainImage(p: vec2f) -> vec4f { return vec4f(1.0); }\n";
        let w = wrap_wgsl(user);
        let expr_at = user.find("vec4f(1.0)").unwrap();
        let header = Span::from(5..user.len() - 1);
        let expr = Span::from(expr_at..user.find("; }").unwrap());
        let prelude = Span::from(user.len() + 3..user.len() + 4);
        let col = u32::try_from(expr_at - 5).unwrap() + 1;
        assert_eq!(
            pick_location(&w, [header, prelude, expr].iter()),
            Some((2, col))
        );
        assert_eq!(pick_location(&w, [prelude].iter()), None);
    }

    #[test]
    fn unknown_extension_is_an_error() {
        let errs = compile_err("x.hlsl", GLSL_PASS);
        assert!(errs[0].message.contains("unknown shader type"));
    }

    #[test]
    fn pass_through_compiles_in_both_languages() {
        let g = compile_ok("p.glsl", GLSL_PASS);
        assert_eq!(g.inputs.used, mask_of(&["iResolution"]));
        assert!(g.wgsl.contains("@fragment"));
        let w = compile_ok("p.wgsl", WGSL_PASS);
        assert_eq!(w.inputs.used, mask_of(&["iResolution"]));
        assert_eq!(w.entry_point, WGSL_ENTRY);
    }

    #[test]
    fn static_shader_is_static() {
        let shader = "vec3 tint(vec3 c) { return c * iBackgroundColor; }\n\
                      void mainImage(out vec4 c, in vec2 p) {\n\
                      \x20   vec4 s = texture(iChannel0, p / iResolution.xy);\n\
                      \x20   c = vec4(tint(s.rgb) + iPalette[int(p.x) % 16], 1.0);\n\
                      }\n";
        let c = compile_ok("s.glsl", shader);
        assert!(!c.inputs.reads(time_mask()));
        assert!(!c.inputs.reads(mouse_mask() | focus_mask() | bell_mask()));
        assert!(c.inputs.reads(mask_of(&["iPalette"])));
        assert!(c.inputs.reads(mask_of(&["iBackgroundColor"])));
        assert_eq!(c.inputs.event_duration, None);
    }

    #[test]
    fn time_read_in_a_nested_helper_is_found() {
        let shader = "float wave(float x) { return sin(x + iTime); }\n\
                      float outer(float x) { if (x > 0.0) { return wave(x); } return 0.0; }\n\
                      void mainImage(out vec4 c, in vec2 p) { c = vec4(outer(p.x)); }\n";
        let c = compile_ok("t.glsl", shader);
        assert!(c.inputs.reads(time_mask()));
        let shader = "fn wave(x: f32) -> f32 { return sin(x + toastty.iTime); }\n\
                      fn outer(x: f32) -> f32 { for (var i = 0; i < 2; i++) { if x > 0.0 { return wave(x); } } return 0.0; }\n\
                      fn mainImage(p: vec2f) -> vec4f { return vec4f(outer(p.x)); }\n";
        let c = compile_ok("t.wgsl", shader);
        assert!(c.inputs.reads(time_mask()));
    }

    #[test]
    fn unused_helper_does_not_count() {
        let shader = "float unused() { return iTime; }\n\
                      void mainImage(out vec4 c, in vec2 p) { c = vec4(iMouse.zw, 0.0, 1.0); }\n";
        let c = compile_ok("u.glsl", shader);
        assert!(!c.inputs.reads(time_mask()));
        assert!(c.inputs.reads(mouse_mask()));
    }

    #[test]
    fn bell_array_counts_as_time_and_bell() {
        let shader = "void mainImage(out vec4 c, in vec2 p) { c = vec4(iBells[1].w); }\n";
        let c = compile_ok("b.glsl", shader);
        assert!(c.inputs.reads(time_mask()));
        assert!(c.inputs.reads(bell_mask()));
        assert!(!c.inputs.reads(focus_mask()));
    }

    #[test]
    fn duration_constant_is_read_in_both_languages() {
        let g = compile_ok(
            "d.glsl",
            &format!("const float TOASTTY_EVENT_DURATION = 1.5;\n{GLSL_PASS}"),
        );
        assert_eq!(g.inputs.event_duration, Some(1.5));
        let w = compile_ok(
            "d.wgsl",
            &format!("const TOASTTY_EVENT_DURATION: f32 = 2.25;\n{WGSL_PASS}"),
        );
        assert_eq!(w.inputs.event_duration, Some(2.25));
    }

    #[test]
    fn palette_index_does_not_copy_the_array() {
        // A dynamic index into iPalette must index the uniform in place,
        // not load all 256 entries into a local first.
        let shader = "void mainImage(out vec4 c, in vec2 p) {\n\
                      \x20   int i = int(p.x) & 255;\n\
                      \x20   c = vec4(iPalette[i], 1.0);\n\
                      }\n";
        let c = compile_ok("pal.glsl", shader);
        assert!(
            !c.wgsl.contains("array<vec3<f32>, 256>")
                || c.wgsl.matches("array<vec3<f32>, 256>").count() == 1,
            "palette copied:\n{}",
            c.wgsl
        );
    }

    #[test]
    fn ghostty_style_shader_compiles() {
        // Uses the Ghostty-only names, the cursor macros and texture2D.
        let shader = "#version 330 core\n\
                      void mainImage(out vec4 fragColor, in vec2 fragCoord) {\n\
                      \x20   vec2 uv = fragCoord / iResolution.xy;\n\
                      \x20   vec4 col = texture2D(iChannel0, uv);\n\
                      \x20   float d = distance(fragCoord, iCurrentCursor.xy);\n\
                      \x20   if (iCurrentCursorStyle == CURSORSTYLE_BAR) { col.r += 0.1; }\n\
                      \x20   col.rgb += iCurrentCursorColor.rgb * exp(-d * 0.05) * step(iTimeCursorChange, iTime);\n\
                      \x20   col.rgb += iPreviousCursorColor.rgb * float(iFocus) * iTimeDelta * iFrameRate;\n\
                      \x20   col.rgb *= vec3(float(iFrame % 2), iDate.w, iChannelTime[0] + iChannelResolution[0].x);\n\
                      \x20   fragColor = col + iMouse * 0.0;\n\
                      }\n";
        let c = compile_ok("ghostty.glsl", shader);
        assert!(c.inputs.reads(time_mask()));
        assert!(c.inputs.reads(mouse_mask()));
        assert!(c.inputs.reads(focus_mask()));
    }
}
