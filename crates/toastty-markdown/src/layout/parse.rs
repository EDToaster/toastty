//! Markdown events → a small block tree the layout walks.

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};

/// Inline text style flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    /// Index into [`Doc::links`].
    pub link: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Inline {
    Text(String, Style),
    Code(String, Style),
    Math {
        tex: String,
        display: bool,
        style: Style,
    },
    Image {
        url: String,
        alt: String,
        style: Style,
    },
    FootnoteRef(String, Style),
    SoftBreak,
    HardBreak,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Item {
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Block {
    Heading(u8, Vec<Inline>),
    Paragraph(Vec<Inline>),
    List {
        start: Option<u64>,
        tight: bool,
        items: Vec<Item>,
    },
    Quote(Option<BlockQuoteKind>, Vec<Block>),
    Code {
        lang: String,
        text: String,
    },
    Math(String),
    Mermaid(String),
    Table {
        aligns: Vec<Alignment>,
        head: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Rule,
    /// Raw HTML block, shown as plain text.
    Html(String),
    Footnote(String, Vec<Block>),
}

#[derive(Debug, Default)]
pub(crate) struct Doc {
    pub blocks: Vec<Block>,
    pub links: Vec<String>,
}

pub(crate) fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_MATH
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_GFM
}

/// Nesting deeper than this is flattened (guards against deeply nested quotes).
const MAX_DEPTH: usize = 48;

enum Frame {
    Root,
    Para,
    Heading(u8),
    Quote(Option<BlockQuoteKind>),
    List(Option<u64>),
    Item(Option<bool>),
    Code(String, String),
    Html(String),
    Footnote(String),
    Table(Vec<Alignment>),
    TableHead,
    TableRow,
    TableCell,
    /// A container past [`MAX_DEPTH`]; its children go to the parent.
    Flat,
}

struct Open {
    frame: Frame,
    blocks: Vec<Block>,
    inlines: Vec<Inline>,
    /// Lists: items; tables: head and body rows.
    items: Vec<Item>,
    rows: Vec<Vec<Vec<Inline>>>,
    cells: Vec<Vec<Inline>>,
    /// List looseness: any item held a paragraph.
    loose: bool,
}

impl Open {
    fn new(frame: Frame) -> Self {
        Self {
            frame,
            blocks: vec![],
            inlines: vec![],
            items: vec![],
            rows: vec![],
            cells: vec![],
            loose: false,
        }
    }
}

struct Builder {
    stack: Vec<Open>,
    style: Vec<Style>,
    links: Vec<String>,
    /// Image being collected: (url, alt text so far, style).
    image: Option<(String, String, Style, usize)>,
    depth: usize,
}

impl Builder {
    fn top(&mut self) -> &mut Open {
        self.stack.last_mut().expect("root frame")
    }

    fn style(&self) -> Style {
        self.style.last().copied().unwrap_or_default()
    }

    fn push_inline(&mut self, inline: Inline) {
        if let Some((_, alt, _, _)) = &mut self.image {
            match inline {
                Inline::Text(t, _) | Inline::Code(t, _) => alt.push_str(&t),
                Inline::Math { tex, .. } => alt.push_str(&tex),
                Inline::SoftBreak | Inline::HardBreak => alt.push(' '),
                _ => {}
            }
            return;
        }
        let top = self.top();
        // Tight list items and other containers get inline content with
        // no paragraph around it.
        if let Inline::Text(t, s) = &inline
            && let Some(Inline::Text(prev, ps)) = top.inlines.last_mut()
            && ps == s
        {
            prev.push_str(t);
            return;
        }
        top.inlines.push(inline);
    }

    /// Wrap loose inlines of the current container into a paragraph.
    fn flush_inlines(&mut self) {
        let top = self.top();
        if !top.inlines.is_empty() {
            let inl = std::mem::take(&mut top.inlines);
            top.blocks.push(Block::Paragraph(inl));
        }
    }

    fn push_block(&mut self, block: Block) {
        self.flush_inlines();
        self.top().blocks.push(block);
    }

    fn start(&mut self, tag: Tag<'_>) {
        let container = matches!(
            tag,
            Tag::BlockQuote(_) | Tag::List(_) | Tag::Item | Tag::FootnoteDefinition(_)
        );
        if container {
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                self.stack.push(Open::new(Frame::Flat));
                return;
            }
        }
        let frame = match tag {
            Tag::Paragraph => Frame::Para,
            Tag::Heading { level, .. } => Frame::Heading(heading_level(level)),
            Tag::BlockQuote(kind) => Frame::Quote(kind),
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_owned()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                Frame::Code(lang, String::new())
            }
            Tag::HtmlBlock => Frame::Html(String::new()),
            Tag::List(start) => Frame::List(start),
            Tag::Item => Frame::Item(None),
            Tag::FootnoteDefinition(label) => Frame::Footnote(label.to_string()),
            Tag::Table(aligns) => Frame::Table(aligns),
            Tag::TableHead => Frame::TableHead,
            Tag::TableRow => Frame::TableRow,
            Tag::TableCell => Frame::TableCell,
            Tag::Emphasis => {
                let s = Style {
                    italic: true,
                    ..self.style()
                };
                self.style.push(s);
                return;
            }
            Tag::Strong => {
                let s = Style {
                    bold: true,
                    ..self.style()
                };
                self.style.push(s);
                return;
            }
            Tag::Strikethrough => {
                let s = Style {
                    strike: true,
                    ..self.style()
                };
                self.style.push(s);
                return;
            }
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.to_string());
                let s = Style {
                    link: Some(self.links.len() - 1),
                    ..self.style()
                };
                self.style.push(s);
                return;
            }
            Tag::Image { dest_url, .. } => {
                let depth = usize::from(self.image.is_some());
                match &mut self.image {
                    Some((_, _, _, d)) => *d += 1,
                    None => {
                        self.image =
                            Some((dest_url.to_string(), String::new(), self.style(), depth));
                    }
                }
                return;
            }
            // Superscript, subscript, definition lists and metadata are
            // not enabled; treat anything else as a transparent span.
            _ => {
                self.style.push(self.style());
                return;
            }
        };
        if matches!(frame, Frame::Para | Frame::Heading(_) | Frame::Table(_)) {
            self.flush_inlines();
        }
        if matches!(frame, Frame::Para)
            && let Some(Open {
                frame: Frame::Item(_),
                ..
            }) = self.stack.last()
        {
            // A paragraph directly in an item makes the list loose,
            // unless it is the only thing in a tight list (pulldown
            // omits paragraphs in tight lists).
            let n = self.stack.len();
            if n >= 2 {
                self.stack[n - 2].loose = true;
            }
        }
        self.stack.push(Open::new(frame));
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Image => {
                if let Some((url, alt, style, depth)) = self.image.take() {
                    if depth > 0 {
                        self.image = Some((url, alt, style, depth - 1));
                    } else {
                        self.push_inline(Inline::Image { url, alt, style });
                    }
                }
                return;
            }
            TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::List(_)
            | TagEnd::Item
            | TagEnd::FootnoteDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell => {}
            _ => {
                self.style.pop();
                return;
            }
        }
        if matches!(
            tag,
            TagEnd::BlockQuote(_) | TagEnd::List(_) | TagEnd::Item | TagEnd::FootnoteDefinition
        ) {
            self.depth = self.depth.saturating_sub(1);
        }
        if self.stack.len() <= 1 {
            return;
        }
        let mut open = self.stack.pop().expect("frame");
        match std::mem::replace(&mut open.frame, Frame::Root) {
            Frame::Para => self.push_block(Block::Paragraph(open.inlines)),
            Frame::Heading(level) => self.push_block(Block::Heading(level, open.inlines)),
            Frame::Quote(kind) => {
                flush(&mut open);
                self.push_block(Block::Quote(kind, open.blocks));
            }
            Frame::Code(lang, text) => {
                let block = match lang.to_ascii_lowercase().as_str() {
                    "math" => Block::Math(text.trim_end().to_owned()),
                    "mermaid" => Block::Mermaid(text),
                    _ => Block::Code { lang, text },
                };
                self.push_block(block);
            }
            Frame::Html(text) => self.push_block(Block::Html(text)),
            Frame::List(start) => {
                let tight = !open.loose;
                self.push_block(Block::List {
                    start,
                    tight,
                    items: open.items,
                });
            }
            Frame::Item(task) => {
                flush(&mut open);
                if open
                    .blocks
                    .iter()
                    .filter(|b| matches!(b, Block::Paragraph(_)))
                    .count()
                    > 1
                {
                    self.top().loose = true;
                }
                let top = self.top();
                top.items.push(Item {
                    task,
                    blocks: open.blocks,
                });
            }
            Frame::Footnote(label) => {
                flush(&mut open);
                self.push_block(Block::Footnote(label, open.blocks));
            }
            Frame::Table(aligns) => {
                let mut rows = open.rows;
                let head = if rows.is_empty() {
                    vec![]
                } else {
                    rows.remove(0)
                };
                self.push_block(Block::Table { aligns, head, rows });
            }
            Frame::TableHead | Frame::TableRow => {
                self.top().rows.push(open.cells);
            }
            Frame::TableCell => {
                self.top().cells.push(open.inlines);
            }
            Frame::Flat => {
                flush(&mut open);
                for b in open.blocks {
                    self.push_block(b);
                }
                let items = open.items;
                for it in items {
                    for b in it.blocks {
                        self.push_block(b);
                    }
                }
            }
            Frame::Root => {}
        }
    }

    fn text(&mut self, text: &str) {
        match &mut self.top().frame {
            Frame::Code(_, buf) | Frame::Html(buf) => buf.push_str(text),
            _ => {
                let s = self.style();
                self.push_inline(Inline::Text(text.to_owned(), s));
            }
        }
    }
}

fn flush(open: &mut Open) {
    if !open.inlines.is_empty() {
        let inl = std::mem::take(&mut open.inlines);
        open.blocks.push(Block::Paragraph(inl));
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

pub(crate) fn parse(markdown: &str) -> Doc {
    let mut b = Builder {
        stack: vec![Open::new(Frame::Root)],
        style: vec![],
        links: vec![],
        image: None,
        depth: 0,
    };
    for ev in Parser::new_ext(markdown, options()) {
        match ev {
            Event::Start(tag) => b.start(tag),
            Event::End(tag) => b.end(tag),
            Event::Text(t) | Event::InlineHtml(t) => b.text(&t),
            Event::Code(t) => {
                let s = b.style();
                b.push_inline(Inline::Code(t.to_string(), s));
            }
            Event::InlineMath(t) => {
                let s = b.style();
                b.push_inline(Inline::Math {
                    tex: t.to_string(),
                    display: false,
                    style: s,
                });
            }
            Event::DisplayMath(t) => {
                let s = b.style();
                b.push_inline(Inline::Math {
                    tex: t.trim().to_string(),
                    display: true,
                    style: s,
                });
            }
            Event::Html(t) => match &mut b.top().frame {
                Frame::Html(buf) => buf.push_str(&t),
                _ => b.push_block(Block::Html(t.to_string())),
            },
            Event::FootnoteReference(label) => {
                let s = b.style();
                b.push_inline(Inline::FootnoteRef(label.to_string(), s));
            }
            Event::SoftBreak => b.push_inline(Inline::SoftBreak),
            Event::HardBreak => b.push_inline(Inline::HardBreak),
            Event::Rule => b.push_block(Block::Rule),
            Event::TaskListMarker(done) => {
                if let Some(Open {
                    frame: Frame::Item(task),
                    ..
                }) = b.stack.last_mut()
                {
                    *task = Some(done);
                }
            }
        }
    }
    // Close anything left open (pulldown always balances, but be safe).
    while b.stack.len() > 1 {
        let open = b.stack.pop().expect("frame");
        let mut open = open;
        flush(&mut open);
        for blk in open.blocks {
            b.push_block(blk);
        }
    }
    b.flush_inlines();
    let root = b.stack.pop().expect("root");
    Doc {
        blocks: root.blocks,
        links: b.links,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_core_structure() {
        let d = parse(
            "# Title\n\nHello **bold** [x](http://a)\n\n- a\n- [x] b\n\n```rust\nfn main(){}\n```\n",
        );
        assert!(matches!(d.blocks[0], Block::Heading(1, _)));
        assert!(matches!(d.blocks[1], Block::Paragraph(_)));
        let Block::List { tight, items, .. } = &d.blocks[2] else {
            panic!()
        };
        assert!(tight);
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].task, Some(true));
        assert!(matches!(&d.blocks[3], Block::Code { lang, .. } if lang == "rust"));
        assert_eq!(d.links, vec!["http://a".to_owned()]);
    }

    #[test]
    fn loose_lists_and_fenced_math_mermaid() {
        let d = parse("- a\n\n- b\n\n```math\nx^2\n```\n\n```mermaid\ngraph TD\n```\n");
        assert!(matches!(&d.blocks[0], Block::List { tight: false, .. }));
        assert_eq!(d.blocks[1], Block::Math("x^2".into()));
        assert_eq!(d.blocks[2], Block::Mermaid("graph TD\n".into()));
    }

    #[test]
    fn image_alt_text_is_collected() {
        let d = parse("![an *alt*](x.png)");
        let Block::Paragraph(inl) = &d.blocks[0] else {
            panic!()
        };
        assert_eq!(
            inl[0],
            Inline::Image {
                url: "x.png".into(),
                alt: "an alt".into(),
                style: Style::default()
            }
        );
    }

    #[test]
    fn deep_nesting_is_flattened() {
        fn depth(b: &[Block]) -> usize {
            b.iter()
                .map(|b| match b {
                    Block::Quote(_, c) => 1 + depth(c),
                    _ => 0,
                })
                .max()
                .unwrap_or(0)
        }
        let src = ">".repeat(500) + " deep";
        let d = parse(&src);
        assert!(depth(&d.blocks) <= MAX_DEPTH);
    }
}
