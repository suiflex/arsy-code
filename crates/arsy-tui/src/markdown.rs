//! Markdown projection for assistant responses.
//!
//! The parser stays in the presentation crate. Syntax highlighting is supplied
//! by the host through [`Highlighter`], so this crate does not need a parser for
//! every programming language the CLI can encounter.

use crate::{wrap, Line, Role, Style};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Host-provided syntax highlighting without coupling the TUI to tree-sitter.
pub type Highlighter = fn(lang: &str, code: &str) -> Option<Vec<Line>>;

/// Render Markdown into width-bounded semantic rows.
///
/// Unsupported raw HTML is treated as inert text. Fenced code is highlighted
/// when the host supplies a result and otherwise remains a dim code block.
pub fn render(markdown: &str, width: usize, highlighter: Option<Highlighter>) -> Vec<Line> {
    let mut renderer = MarkdownRenderer::new(width, highlighter);
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    for event in Parser::new_ext(markdown, options) {
        renderer.event(event);
    }
    renderer.finish()
}

struct MarkdownRenderer {
    width: usize,
    highlighter: Option<Highlighter>,
    lines: Vec<Line>,
    current: Line,
    styles: Vec<Style>,
    lists: Vec<ListState>,
    quote_depth: usize,
    code: Option<CodeState>,
}

struct ListState {
    ordered: bool,
    next: u64,
}

struct CodeState {
    language: String,
    text: String,
}

impl MarkdownRenderer {
    fn new(width: usize, highlighter: Option<Highlighter>) -> Self {
        Self {
            width,
            highlighter,
            lines: Vec::new(),
            current: Line::new(),
            styles: vec![Style::new(Role::Assistant)],
            lists: Vec::new(),
            quote_depth: 0,
            code: None,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        if self.code.is_some() {
            self.code_event(event);
        } else {
            self.normal_event(event);
        }
    }

    fn code_event(&mut self, event: Event<'_>) {
        match event {
            Event::End(TagEnd::CodeBlock) => self.finish_code(),
            Event::Text(text) | Event::Code(text) | Event::Html(text) => self
                .code
                .as_mut()
                .expect("code block exists")
                .text
                .push_str(&text),
            Event::SoftBreak | Event::HardBreak => self
                .code
                .as_mut()
                .expect("code block exists")
                .text
                .push('\n'),
            _ => {}
        }
    }

    fn normal_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.append(&text, self.style())
            }
            Event::Code(text) => self.append(&text, Style::new(Role::Accent)),
            Event::SoftBreak => self.append(" ", self.style()),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.flush_line();
                self.push_line(Line::of("────────────────", Role::Dim));
                self.blank();
            }
            Event::TaskListMarker(checked) => self.append(
                if checked { "[x] " } else { "[ ] " },
                Style::new(Role::Accent),
            ),
            Event::FootnoteReference(name) => self.append(&format!("[^{name}]"), self.style()),
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.append(&text, Style::new(Role::Accent))
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Item => self.start_item(),
            Tag::CodeBlock(kind) => self.start_code(kind),
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } => {
                self.start_style(tag)
            }
            Tag::Table(_) | Tag::TableHead | Tag::TableRow | Tag::TableCell => {
                self.start_table(tag)
            }
            Tag::Paragraph | Tag::Heading { .. } | Tag::BlockQuote(_) | Tag::List(_) => {
                self.start_block(tag)
            }
            Tag::Image { .. } => self.append("[image]", Style::new(Role::Dim)),
            Tag::FootnoteDefinition(_) | Tag::MetadataBlock(_) => {}
            _ => {}
        }
    }

    fn start_block(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_line();
                let prefix = "#".repeat(heading_number(level));
                self.append(&format!("{prefix} "), Style::new(Role::Accent).bold());
                self.styles.push(Style::new(Role::Accent).bold());
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth += 1;
                self.append(&"│ ".repeat(self.quote_depth), Style::new(Role::Dim));
            }
            Tag::List(start) => self.lists.push(ListState {
                ordered: start.is_some(),
                next: start.unwrap_or(1),
            }),
            _ => {}
        }
    }

    fn start_item(&mut self) {
        self.flush_line();
        let depth = self.lists.len().saturating_sub(1);
        let prefix = if let Some(list) = self.lists.last_mut() {
            if list.ordered {
                let value = format!("{}. ", list.next);
                list.next = list.next.saturating_add(1);
                value
            } else {
                "• ".to_owned()
            }
        } else {
            "• ".to_owned()
        };
        self.append(
            &format!("{}{prefix}", "  ".repeat(depth)),
            Style::new(Role::Accent),
        );
    }

    fn start_code(&mut self, kind: CodeBlockKind<'_>) {
        self.flush_line();
        let language = match kind {
            CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_owned(),
            CodeBlockKind::Indented => String::new(),
        };
        self.code = Some(CodeState {
            language,
            text: String::new(),
        });
    }

    fn start_style(&mut self, tag: Tag<'_>) {
        let style = match tag {
            Tag::Emphasis | Tag::Strong => self.style().bold(),
            Tag::Strikethrough => Style::new(Role::Dim),
            Tag::Link { .. } => Style::new(Role::Accent),
            _ => self.style(),
        };
        self.styles.push(style);
    }

    fn start_table(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Table(_) | Tag::TableHead => {}
            Tag::TableRow => {
                self.flush_line();
                self.append("│", Style::new(Role::Dim));
            }
            Tag::TableCell => self.append(" ", self.style()),
            _ => {}
        }
    }
    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                self.blank();
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush_line();
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.blank();
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.flush_line();
                self.blank();
            }
            TagEnd::Item => self.flush_line(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.styles.pop();
            }
            TagEnd::TableCell => self.append(" │", Style::new(Role::Dim)),
            TagEnd::TableRow => {
                self.flush_line();
                self.blank();
            }
            TagEnd::TableHead => {}
            TagEnd::Table => {
                self.flush_line();
                self.blank();
            }
            TagEnd::CodeBlock => unreachable!("code blocks are handled before normal events"),
            TagEnd::Image => {}
            TagEnd::FootnoteDefinition => {}
            _ => {}
        }
    }

    fn finish_code(&mut self) {
        let Some(code) = self.code.take() else { return };
        let fence = if code.language.is_empty() {
            "```".to_owned()
        } else {
            format!("```{}", code.language)
        };
        self.push_line(Line::of(fence, Role::Dim));
        let highlighted = self
            .highlighter
            .and_then(|highlight| highlight(&code.language, &code.text));
        if let Some(lines) = highlighted {
            for line in lines {
                self.push_line(line);
            }
        } else {
            for line in code.text.split('\n') {
                self.push_line(Line::of(line, Role::Dim));
            }
        }
        self.push_line(Line::of("```", Role::Dim));
        self.blank();
    }

    fn style(&self) -> Style {
        self.styles
            .last()
            .copied()
            .unwrap_or(Style::new(Role::Assistant))
    }

    fn append(&mut self, text: &str, style: Style) {
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.flush_line();
            }
            self.current = self.current.clone().push(part, style);
        }
    }

    fn flush_line(&mut self) {
        if !self.current.is_empty() {
            let line = std::mem::take(&mut self.current);
            self.push_line(line);
        }
    }

    fn push_line(&mut self, line: Line) {
        self.lines.extend(wrap(&line, self.width));
    }

    fn blank(&mut self) {
        if !self.lines.last().is_some_and(Line::is_empty) {
            self.lines.push(Line::blank());
        }
    }

    fn finish(mut self) -> Vec<Line> {
        self.flush_line();
        while self.lines.last().is_some_and(Line::is_empty) {
            self.lines.pop();
        }
        self.lines
    }
}

fn heading_number(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::text).collect()
    }

    #[test]
    fn renders_blocks_and_inline_styles() {
        let lines = render(
            "# Heading\n\nA **bold** word, *emphasis*, and `code`.\n\n- one\n- two",
            80,
            None,
        );
        assert_eq!(
            text(&lines),
            [
                "# Heading",
                "",
                "A bold word, emphasis, and code.",
                "",
                "• one",
                "• two",
            ]
        );
        assert!(lines[0].spans[0].style.bold);
        assert!(lines[2]
            .spans
            .iter()
            .any(|span| span.style.role == Role::Accent));
    }

    #[test]
    fn wraps_quotes_tables_and_fences_without_overflow() {
        let lines = render(
            "> a quoted sentence\n\n| a | b |\n|---|---|\n| c | d |\n\n```toml\nname = \"arsy\"\n```",
            12,
            None,
        );
        assert!(lines.iter().all(|line| line.width() <= 12));
        let rendered = text(&lines).join("\n");
        assert!(rendered.contains("│ a quoted"));
        assert!(rendered.contains("```toml"));
        assert!(rendered.contains("name ="));
        assert!(rendered.contains("\"arsy\""));
    }

    #[test]
    fn host_highlighter_replaces_plain_code_rows() {
        fn highlight(lang: &str, code: &str) -> Option<Vec<Line>> {
            assert_eq!(lang, "rust");
            Some(vec![Line::of(code.to_uppercase(), Role::Accent)])
        }
        let lines = render("```rust\nlet x = 1;\n```", 80, Some(highlight));
        assert_eq!(text(&lines), ["```rust", "LET X = 1;", "```"]);
        assert_eq!(lines[1].spans[0].style.role, Role::Accent);
    }
}
