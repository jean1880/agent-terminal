//! Markdown → a small widget model (pure; no GTK).
//!
//! Assistant text is parsed with `pulldown-cmark` into [`Block`]s: runs of Pango markup for
//! prose (rendered as wrapping, selectable labels) and verbatim code (rendered in a monospace
//! source view). Re-parsing the whole message on every streamed delta is cheap at chat sizes,
//! and because earlier blocks compare equal the widget layer only rebuilds the tail.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Inline code background/foreground in the brand palette.
const INLINE_CODE_BG: &str = "#2a2340";
const INLINE_CODE_FG: &str = "#e2d4ff";
const LINK_FG: &str = "#9fc4ff";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Pango markup for one prose block.
    Text {
        markup: String,
        kind: TextKind,
    },
    /// A fenced or indented code block (verbatim text, trailing newline trimmed).
    Code {
        lang: Option<String>,
        text: String,
    },
    Rule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Paragraph,
    Heading(u8),
    Quote,
    List,
}

/// Escapes text for Pango markup.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// One open list level: `Some(n)` is ordered with the next number.
struct ListLevel {
    next: Option<u64>,
}

#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    markup: String,
    kind: Option<TextKind>,
    lists: Vec<ListLevel>,
    quote_depth: usize,
    code: Option<(Option<String>, String)>,
    /// Table cells collected as plain text, rendered as an aligned monospace block.
    table: Option<Vec<Vec<String>>>,
    cell: Option<String>,
}

impl Builder {
    fn text_kind(&self) -> TextKind {
        if !self.lists.is_empty() {
            TextKind::List
        } else if self.quote_depth > 0 {
            TextKind::Quote
        } else {
            TextKind::Paragraph
        }
    }

    fn begin_text(&mut self, kind: TextKind) {
        if self.kind.is_none() {
            self.kind = Some(kind);
        }
    }

    fn flush_text(&mut self) {
        let markup = self.markup.trim_end().to_owned();
        self.markup.clear();
        if let Some(kind) = self.kind.take() {
            if !markup.trim().is_empty() {
                self.blocks.push(Block::Text { markup, kind });
            }
        }
    }

    fn push_inline(&mut self, s: &str) {
        if let Some(cell) = self.cell.as_mut() {
            cell.push_str(s);
            return;
        }
        let kind = self.text_kind();
        self.begin_text(kind);
        self.markup.push_str(s);
    }

    fn push_markup(&mut self, s: &str) {
        if self.cell.is_some() {
            return; // Tables are plain text.
        }
        let kind = self.text_kind();
        self.begin_text(kind);
        self.markup.push_str(s);
    }

    fn newline_in_block(&mut self) {
        if !self.markup.is_empty() && !self.markup.ends_with('\n') {
            self.markup.push('\n');
        }
    }
}

/// Parses markdown into blocks.
pub fn parse(markdown: &str) -> Vec<Block> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut b = Builder::default();
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(tag) => start(&mut b, tag),
            Event::End(tag) => end(&mut b, tag),
            Event::Text(text) => {
                if let Some((_, code)) = b.code.as_mut() {
                    code.push_str(&text);
                } else if let Some(cell) = b.cell.as_mut() {
                    cell.push_str(&text);
                } else {
                    b.push_inline(&escape(&text));
                }
            }
            Event::Code(code) => {
                if let Some(cell) = b.cell.as_mut() {
                    cell.push_str(&code);
                } else {
                    b.push_markup(&format!(
                        "<span font_family=\"monospace\" bgcolor=\"{INLINE_CODE_BG}\" fgcolor=\"{INLINE_CODE_FG}\">\u{2009}{}\u{2009}</span>",
                        escape(&code)
                    ));
                }
            }
            Event::SoftBreak => b.push_inline(" "),
            Event::HardBreak => b.push_inline("\n"),
            Event::Rule => {
                b.flush_text();
                b.blocks.push(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                b.push_inline(if done { "☑ " } else { "☐ " });
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                if let Some((_, code)) = b.code.as_mut() {
                    code.push_str(&html);
                } else {
                    b.push_inline(&escape(&html));
                }
            }
            Event::FootnoteReference(r) => b.push_inline(&escape(&format!("[{r}]"))),
            Event::InlineMath(m) | Event::DisplayMath(m) => b.push_inline(&escape(&m)),
        }
    }
    // Streaming text may stop mid-block: whatever is open is flushed as it stands.
    if let Some((lang, text)) = b.code.take() {
        b.blocks.push(Block::Code {
            lang,
            text: text.trim_end_matches('\n').to_owned(),
        });
    }
    b.flush_text();
    b.blocks
}

fn start(b: &mut Builder, tag: Tag) {
    match tag {
        Tag::Paragraph => {
            if b.lists.is_empty() {
                b.flush_text();
            }
        }
        Tag::Heading { level, .. } => {
            b.flush_text();
            let n = match level {
                HeadingLevel::H1 => 1,
                HeadingLevel::H2 => 2,
                HeadingLevel::H3 => 3,
                HeadingLevel::H4 => 4,
                HeadingLevel::H5 => 5,
                HeadingLevel::H6 => 6,
            };
            b.begin_text(TextKind::Heading(n));
            let size = match n {
                1 => "x-large",
                2 => "large",
                _ => "medium",
            };
            b.markup
                .push_str(&format!("<span size=\"{size}\" weight=\"bold\">"));
        }
        Tag::BlockQuote(_) => {
            b.flush_text();
            b.quote_depth += 1;
        }
        Tag::CodeBlock(kind) => {
            b.flush_text();
            let lang = match kind {
                CodeBlockKind::Fenced(info) => info
                    .split(|c: char| c.is_whitespace() || c == ',')
                    .next()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                CodeBlockKind::Indented => None,
            };
            b.code = Some((lang, String::new()));
        }
        Tag::List(first) => {
            if b.lists.is_empty() {
                b.flush_text();
            } else {
                b.newline_in_block();
            }
            b.lists.push(ListLevel { next: first });
        }
        Tag::Item => {
            b.newline_in_block();
            let depth = b.lists.len().saturating_sub(1);
            let indent = "    ".repeat(depth);
            let bullet = match b.lists.last_mut() {
                Some(ListLevel { next: Some(n) }) => {
                    let s = format!("{n}.");
                    *n += 1;
                    s
                }
                _ => if depth == 0 { "•" } else { "◦" }.to_owned(),
            };
            b.push_markup(&format!("{indent}{bullet} "));
        }
        Tag::Emphasis => b.push_markup("<i>"),
        Tag::Strong => b.push_markup("<b>"),
        Tag::Strikethrough => b.push_markup("<s>"),
        Tag::Link { dest_url, .. } => {
            b.push_markup(&format!(
                "<a href=\"{}\"><span fgcolor=\"{LINK_FG}\">",
                escape(&dest_url)
            ));
        }
        Tag::Image { .. } => b.push_markup("<i>[image: "),
        Tag::Table(_) => {
            b.flush_text();
            b.table = Some(Vec::new());
        }
        Tag::TableHead | Tag::TableRow => {
            if let Some(t) = b.table.as_mut() {
                t.push(Vec::new());
            }
        }
        Tag::TableCell => b.cell = Some(String::new()),
        Tag::FootnoteDefinition(_)
        | Tag::HtmlBlock
        | Tag::DefinitionList
        | Tag::DefinitionListTitle
        | Tag::DefinitionListDefinition
        | Tag::MetadataBlock(_)
        | Tag::Superscript
        | Tag::Subscript => {}
    }
}

fn end(b: &mut Builder, tag: TagEnd) {
    match tag {
        TagEnd::Paragraph => {
            if b.lists.is_empty() {
                b.flush_text();
            }
        }
        TagEnd::Heading(_) => {
            b.markup.push_str("</span>");
            b.flush_text();
        }
        TagEnd::BlockQuote(_) => {
            b.flush_text();
            b.quote_depth = b.quote_depth.saturating_sub(1);
        }
        TagEnd::CodeBlock => {
            if let Some((lang, text)) = b.code.take() {
                b.blocks.push(Block::Code {
                    lang,
                    text: text.trim_end_matches('\n').to_owned(),
                });
            }
        }
        TagEnd::List(_) => {
            b.lists.pop();
            if b.lists.is_empty() {
                b.flush_text();
            }
        }
        TagEnd::Item => {}
        TagEnd::Emphasis => b.push_markup("</i>"),
        TagEnd::Strong => b.push_markup("</b>"),
        TagEnd::Strikethrough => b.push_markup("</s>"),
        TagEnd::Link => b.push_markup("</span></a>"),
        TagEnd::Image => b.push_markup("]</i>"),
        TagEnd::TableCell => {
            if let (Some(cell), Some(row)) =
                (b.cell.take(), b.table.as_mut().and_then(|t| t.last_mut()))
            {
                row.push(cell.trim().to_owned());
            }
        }
        TagEnd::Table => {
            if let Some(rows) = b.table.take() {
                b.blocks.push(Block::Code {
                    lang: Some("table".to_owned()),
                    text: render_table(&rows),
                });
            }
        }
        TagEnd::TableHead
        | TagEnd::TableRow
        | TagEnd::FootnoteDefinition
        | TagEnd::HtmlBlock
        | TagEnd::DefinitionList
        | TagEnd::DefinitionListTitle
        | TagEnd::DefinitionListDefinition
        | TagEnd::MetadataBlock(_)
        | TagEnd::Superscript
        | TagEnd::Subscript => {}
    }
}

/// Renders table rows as aligned plain text with a rule under the header.
fn render_table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for (r, row) in rows.iter().enumerate() {
        let line: Vec<String> = (0..cols)
            .map(|i| {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                let pad = widths[i].saturating_sub(cell.chars().count());
                format!("{cell}{}", " ".repeat(pad))
            })
            .collect();
        out.push_str(line.join("  │  ").trim_end());
        out.push('\n');
        if r == 0 && rows.len() > 1 {
            let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            out.push_str(&rule.join("──┼──"));
            out.push('\n');
        }
    }
    out.trim_end_matches('\n').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(markup: &str, kind: TextKind) -> Block {
        Block::Text {
            markup: markup.to_owned(),
            kind,
        }
    }

    #[test]
    fn paragraphs_with_inline_styles_and_escaping() {
        let blocks = parse("Hello **bold** _it_ ~~gone~~ a < b & c\n\nSecond");
        assert_eq!(
            blocks,
            vec![
                text(
                    "Hello <b>bold</b> <i>it</i> <s>gone</s> a &lt; b &amp; c",
                    TextKind::Paragraph
                ),
                text("Second", TextKind::Paragraph),
            ]
        );
    }

    #[test]
    fn inline_code_is_monospace_and_escaped() {
        let blocks = parse("run `a<b>`");
        let Block::Text { markup, .. } = &blocks[0] else {
            panic!("expected text: {blocks:?}")
        };
        assert!(markup.contains("font_family=\"monospace\""));
        assert!(markup.contains("a&lt;b&gt;"));
    }

    #[test]
    fn headings_and_rules() {
        let blocks = parse("# Title\n\n---\n\n### Small");
        assert_eq!(
            blocks,
            vec![
                text(
                    "<span size=\"x-large\" weight=\"bold\">Title</span>",
                    TextKind::Heading(1)
                ),
                Block::Rule,
                text(
                    "<span size=\"medium\" weight=\"bold\">Small</span>",
                    TextKind::Heading(3)
                ),
            ]
        );
    }

    #[test]
    fn fenced_code_keeps_language_and_text_verbatim() {
        let blocks = parse("Before\n\n```rust title\nfn a() -> u8 { 1 < 2 }\n```\nAfter");
        assert_eq!(
            blocks,
            vec![
                text("Before", TextKind::Paragraph),
                Block::Code {
                    lang: Some("rust".into()),
                    text: "fn a() -> u8 { 1 < 2 }".into()
                },
                text("After", TextKind::Paragraph),
            ]
        );
    }

    #[test]
    fn unclosed_fence_while_streaming_is_still_code() {
        let blocks = parse("```py\nprint(1)\npri");
        assert_eq!(
            blocks,
            vec![Block::Code {
                lang: Some("py".into()),
                text: "print(1)\npri".into()
            }]
        );
    }

    #[test]
    fn lists_are_one_block_with_bullets_numbers_and_nesting() {
        let blocks = parse("- one\n- two\n  - deep\n\n1. first\n2. second\n- [x] done");
        assert_eq!(
            blocks,
            vec![
                text("• one\n• two\n    ◦ deep", TextKind::List),
                text("1. first\n2. second", TextKind::List),
                text("• ☑ done", TextKind::List),
            ]
        );
    }

    #[test]
    fn links_and_quotes() {
        let blocks = parse("> quoted [site](https://x.test/?a=1&b=2)");
        let Block::Text { markup, kind } = &blocks[0] else {
            panic!("expected text")
        };
        assert_eq!(*kind, TextKind::Quote);
        assert!(markup.starts_with("quoted <a href=\"https://x.test/?a=1&amp;b=2\">"));
        assert!(markup.ends_with("site</span></a>"));
    }

    #[test]
    fn tables_render_as_aligned_monospace() {
        let blocks = parse("| a | bb |\n|---|---|\n| ccc | d |");
        assert_eq!(
            blocks,
            vec![Block::Code {
                lang: Some("table".into()),
                text: "a    │  bb\n─────┼────\nccc  │  d".into()
            }]
        );
    }

    #[test]
    fn earlier_blocks_are_stable_while_streaming() {
        let a = parse("Para one.\n\nPara tw");
        let b = parse("Para one.\n\nPara two is longer");
        assert_eq!(a[0], b[0]);
        assert_ne!(a[1], b[1]);
    }

    #[test]
    fn empty_input_has_no_blocks() {
        assert!(parse("").is_empty());
        assert!(parse("   \n\n").is_empty());
    }

    #[test]
    fn escape_covers_markup_characters() {
        assert_eq!(
            escape("<a href='x'>&\""),
            "&lt;a href=&#39;x&#39;&gt;&amp;&quot;"
        );
    }
}
