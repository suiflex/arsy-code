//! Markdown projection for assistant responses.
//!
//! The parser stays in the presentation crate. Syntax highlighting is supplied
//! by the host through [`Highlighter`], so this crate does not need a parser for
//! every programming language the CLI can encounter.

use crate::{wrap, wrap_hanging, Line, Role, Style};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

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
    table: Option<TableState>,
    /// How many spans at the start of `current` are a block's marker (a list
    /// bullet, a quote bar) rather than its text, so a wrapped row can hang
    /// under the text instead of starting under the marker.
    lead: usize,
    /// Each open link's target and the text read inside it so far.
    links: Vec<(String, String)>,
}

/// A table being read. It is drawn only once it has ended, because every
/// column's width depends on the widest cell below it.
struct TableState {
    alignments: Vec<Alignment>,
    /// Finished rows, the header first when there is one.
    rows: Vec<Vec<Line>>,
    head: bool,
    row: Vec<Line>,
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
            table: None,
            lead: 0,
            links: Vec::new(),
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
                if let Some((_, read)) = self.links.last_mut() {
                    read.push_str(&text);
                }
                self.append(&text, self.style())
            }
            // On the composer's surface, so a code span reads as code rather
            // than as the accent a link is drawn in.
            Event::Code(text) => {
                if let Some((_, read)) = self.links.last_mut() {
                    read.push_str(&text);
                }
                self.append(&text, Style::new(Role::Accent).on(Role::InputBg))
            }
            Event::SoftBreak => self.append(" ", self.style()),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.flush_line();
                self.push_line(Line::of("─".repeat(self.width.max(1)), Role::Dim));
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
            Tag::Paragraph => self.open_row(),
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.open_row();
                // Told apart by weight and colour rather than prefixed with
                // its hashes. The `#` is source syntax: leaving it in is what
                // makes a rendered answer still read as raw markdown, which is
                // the complaint this renderer exists to answer. H1 is also
                // underlined when it ends.
                self.styles.push(match heading_number(level) {
                    1 | 2 => Style::new(Role::Accent).bold(),
                    _ => Style::new(Role::Assistant).bold(),
                });
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth += 1;
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
        self.open_row();
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
        self.append_lead(
            &format!("{}{prefix}", "  ".repeat(depth)),
            Style::new(Role::Accent),
        );
    }

    /// Start a row inside the quotes that are open with their bars, so every
    /// paragraph of a quote is marked and not only its first.
    fn open_row(&mut self) {
        if self.current.is_empty() && self.quote_depth > 0 {
            self.append_lead(&"│ ".repeat(self.quote_depth), Style::new(Role::Dim));
        }
    }

    /// Add a block's marker to the row: kept out of the text a wrap breaks,
    /// so continuation rows hang under the text.
    fn append_lead(&mut self, text: &str, style: Style) {
        let marker_only = self.current.spans.len() == self.lead;
        self.current = std::mem::take(&mut self.current).push(text, style);
        if marker_only {
            self.lead = self.current.spans.len();
        }
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
            Tag::Emphasis => self.style().italic(),
            Tag::Strong => self.style().bold(),
            Tag::Strikethrough => Style::new(Role::Dim),
            Tag::Link { dest_url, .. } => {
                self.links.push((dest_url.into_string(), String::new()));
                Style::new(Role::Accent)
            }
            _ => self.style(),
        };
        self.styles.push(style);
    }

    fn start_table(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Table(alignments) => {
                self.flush_line();
                self.table = Some(TableState {
                    alignments,
                    rows: Vec::new(),
                    head: false,
                    row: Vec::new(),
                });
            }
            // Header cells arrive straight under the head, with no row of
            // their own, so the head is the row.
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.head = true;
                }
                self.styles.push(self.style().bold());
            }
            Tag::TableRow | Tag::TableCell => self.current = Line::new(),
            _ => {}
        }
    }

    fn end_table(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.current);
                if let Some(table) = self.table.as_mut() {
                    table.row.push(cell);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if tag == TagEnd::TableHead {
                    self.styles.pop();
                }
                if let Some(table) = self.table.as_mut() {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    for line in table_lines(&table, self.width) {
                        self.lines.push(line);
                    }
                }
                self.blank();
            }
            _ => {}
        }
    }
    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                self.blank();
            }
            TagEnd::Heading(level) => {
                self.styles.pop();
                let underline = (level == HeadingLevel::H1)
                    .then(|| "─".repeat(self.current.width().clamp(1, self.width.max(1))));
                self.flush_line();
                if let Some(rule) = underline {
                    self.push_line(Line::of(rule, Role::Accent));
                }
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
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                self.end_link();
            }
            TagEnd::TableCell | TagEnd::TableRow | TagEnd::TableHead | TagEnd::Table => {
                self.end_table(tag)
            }
            TagEnd::CodeBlock => unreachable!("code blocks are handled before normal events"),
            TagEnd::Image => {}
            TagEnd::FootnoteDefinition => {}
            _ => {}
        }
    }

    fn finish_code(&mut self) {
        let Some(code) = self.code.take() else { return };
        // Set off by a gutter rather than fenced: the backticks are source
        // syntax, and printed they made a rendered answer read as raw
        // markdown, the same complaint the heading hashes were dropped for.
        let text = code.text.strip_suffix('\n').unwrap_or(&code.text);
        let highlighted = self
            .highlighter
            .and_then(|highlight| highlight(&code.language, text));
        let lines = highlighted.unwrap_or_else(|| {
            text.split('\n')
                .map(|line| Line::of(line, Role::Dim))
                .collect()
        });
        let gutter = Line::of("│ ", Role::Dim);
        if !code.language.is_empty() {
            self.push_line(Line::of(format!("── {} ──", code.language), Role::Dim));
        }
        for line in lines {
            self.lines
                .extend(wrap_hanging(&gutter, &line, &gutter, self.width));
        }
        self.blank();
    }

    /// Show where a link goes when its text does not already say so. The
    /// target is what a reader needs to follow it from a terminal.
    fn end_link(&mut self) {
        let Some((target, text)) = self.links.pop() else {
            return;
        };
        let shown = target.is_empty()
            || target.starts_with('#')
            || target == text
            || target.strip_prefix("mailto:") == Some(text.as_str());
        if !shown {
            self.append(&format!(" ({target})"), Style::new(Role::Dim));
        }
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
        let lead = std::mem::take(&mut self.lead);
        if self.current.is_empty() {
            return;
        }
        let mut line = std::mem::take(&mut self.current);
        if lead == 0 {
            self.push_line(line);
            return;
        }
        let body = Line {
            spans: line.spans.split_off(lead.min(line.spans.len())),
        };
        let hang = hanging(&line);
        self.lines
            .extend(wrap_hanging(&line, &body, &hang, self.width));
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

/// A table as bordered rows no wider than `width`: columns as wide as their
/// widest cell, narrowed from the widest down when the table does not fit, a
/// cell too long for its column wrapped inside it, and a rule under the head.
fn table_lines(table: &TableState, width: usize) -> Vec<Line> {
    let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }
    let widths = column_widths(table, columns, width);
    let border = Style::new(Role::Dim);
    let rule = |left: &str, join: &str, right: &str| {
        let bars: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        Line::of(format!("{left}{}{right}", bars.join(join)), border)
    };
    let mut lines = vec![rule("┌", "┬", "┐")];
    for (index, row) in table.rows.iter().enumerate() {
        let cells: Vec<Vec<Line>> = (0..columns)
            .map(|column| {
                let cell = row.get(column).cloned().unwrap_or_default();
                wrap(&cell, widths[column])
            })
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        lines.extend((0..height).map(|step| grid_row(&cells, step, &widths, &table.alignments)));
        if index == 0 && table.head && table.rows.len() > 1 {
            lines.push(rule("├", "┼", "┤"));
        }
    }
    lines.push(rule("└", "┴", "┘"));
    lines
}

/// Row `step` of a table row whose cells were wrapped into `cells`: each
/// cell's line at that step, padded to its column, between borders.
fn grid_row(cells: &[Vec<Line>], step: usize, widths: &[usize], alignments: &[Alignment]) -> Line {
    let border = Style::new(Role::Dim);
    cells
        .iter()
        .zip(widths)
        .zip(alignments.iter().map(Some).chain(std::iter::repeat(None)))
        .fold(
            Line::of("│", border),
            |line, ((rows, width), alignment)| {
                let text = rows.get(step).cloned().unwrap_or_default();
                joined(
                    line.push(" ", Style::PLAIN),
                    align(text, *width, alignment.copied()),
                )
                .push(" │", border)
            },
        )
}

/// Each column's width: its widest cell, then the widest column narrowed a
/// column at a time until the table fits, never below one column.
fn column_widths(table: &TableState, columns: usize, width: usize) -> Vec<usize> {
    let mut widths = vec![1; columns];
    let cells = table.rows.iter().flat_map(|row| row.iter().enumerate());
    for (column, cell) in cells {
        widths[column] = widths[column].max(cell.width());
    }
    // `│` plus ` cell │` per column.
    let room = width.saturating_sub(1 + 3 * columns);
    while widths.iter().sum::<usize>() > room {
        let Some(widest) = (0..columns).max_by_key(|&column| widths[column]) else {
            break;
        };
        if widths[widest] <= 1 {
            break;
        }
        widths[widest] -= 1;
    }
    widths
}

/// A cell padded to `width` on the side its column's alignment asks for.
fn align(cell: Line, width: usize, alignment: Option<Alignment>) -> Line {
    let gap = width.saturating_sub(cell.width());
    let (before, after) = match alignment {
        Some(Alignment::Right) => (gap, 0),
        Some(Alignment::Center) => (gap / 2, gap - gap / 2),
        _ => (0, gap),
    };
    joined(Line::of(" ".repeat(before), Style::PLAIN), cell).push(" ".repeat(after), Style::PLAIN)
}

/// What a wrapped row starts with under a block's marker: the quote bars
/// again, and blanks where a bullet or a number stood.
fn hanging(lead: &Line) -> Line {
    lead.spans.iter().fold(Line::new(), |line, span| {
        let text: String = span
            .text()
            .chars()
            .map(|c| if c == '│' { c } else { ' ' })
            .collect();
        line.push(text, span.style)
    })
}

/// `head` followed by every span of `tail`, each keeping its own style.
fn joined(head: Line, tail: Line) -> Line {
    tail.spans.into_iter().fold(head, Line::push_span)
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
                "Heading",
                "───────",
                "",
                "A bold word, emphasis, and code.",
                "",
                "• one",
                "• two",
            ]
        );
        assert!(lines[0].spans[0].style.bold);
        let spans = &lines[3].spans;
        assert!(spans
            .iter()
            .any(|span| span.style.italic && !span.style.bold));
        assert!(spans
            .iter()
            .any(|span| span.text() == "code" && span.style.bg == Some(Role::InputBg)));
    }

    /// A wrapped list item, quote line, or code line keeps its place: the
    /// continuation hangs under the text, behind the quote bar or gutter.
    #[test]
    fn wrapped_rows_hang_under_their_marker() {
        let lines = render(
            "- one two three four\n\n> five six seven eight\n>\n> nine\n\n```\nabcdefghijklmnop\n```",
            12,
            None,
        );
        // Trimmed: a row broken at a space keeps it, which is invisible.
        let rows: Vec<String> = text(&lines)
            .iter()
            .map(|row| row.trim_end().to_owned())
            .collect();
        assert_eq!(
            rows,
            [
                "• one two",
                "  three four",
                "",
                "│ five six",
                "│ seven",
                "│ eight",
                "",
                "│ nine",
                "",
                "│ abcdefghij",
                "│ klmnop",
            ]
        );
    }

    #[test]
    fn links_show_their_target_unless_the_text_is_the_target() {
        let lines = render(
            "[docs](https://x.dev/docs) and <https://x.dev> and [top](#top)",
            80,
            None,
        );
        assert_eq!(
            text(&lines),
            ["docs (https://x.dev/docs) and https://x.dev and top"]
        );
    }

    #[test]
    fn headings_differ_by_level_and_rules_span_the_width() {
        let lines = render("## Two\n\n### Three\n\n---", 20, None);
        assert_eq!(text(&lines)[0], "Two");
        assert_eq!(lines[0].spans[0].style.role, Role::Accent);
        assert_eq!(lines[2].spans[0].style.role, Role::Assistant);
        assert!(lines[2].spans[0].style.bold);
        assert_eq!(lines[4].width(), 20);
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
        assert!(!rendered.contains("```"), "fences are source, not output");
        assert!(rendered.contains("│ name ="));
        assert!(rendered.contains("\"arsy\""));
    }

    /// A table is drawn as a grid: aligned columns, a rule under the head,
    /// and no blank line between rows.
    #[test]
    fn tables_render_as_aligned_grids() {
        let lines = render(
            "| Crate | Role |\n|---|--:|\n| arsy-kernel | domain |\n| arsy-cli | host |",
            80,
            None,
        );
        assert_eq!(
            text(&lines),
            [
                "┌─────────────┬────────┐",
                "│ Crate       │   Role │",
                "├─────────────┼────────┤",
                "│ arsy-kernel │ domain │",
                "│ arsy-cli    │   host │",
                "└─────────────┴────────┘",
            ]
        );
        assert!(lines[1].spans.iter().any(|span| span.style.bold));
    }

    /// A table wider than the screen narrows its widest column and wraps the
    /// cell inside it, so every row still fits and the borders line up.
    #[test]
    fn a_wide_table_wraps_inside_its_cells() {
        let lines = render(
            "| Crate | Role |\n|---|---|\n| arsy-code | fs, search, syntax, LSP, DAP, edit, git, shell |",
            30,
            None,
        );
        assert!(
            lines.iter().all(|line| line.width() <= 30),
            "{:?}",
            text(&lines)
        );
        let widths: Vec<usize> = lines.iter().map(Line::width).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "{:?}",
            text(&lines)
        );
        assert!(lines.len() > 6, "the long cell wrapped: {:?}", text(&lines));
    }

    #[test]
    fn host_highlighter_replaces_plain_code_rows() {
        fn highlight(lang: &str, code: &str) -> Option<Vec<Line>> {
            assert_eq!(lang, "rust");
            Some(vec![Line::of(code.to_uppercase(), Role::Accent)])
        }
        let lines = render("```rust\nlet x = 1;\n```", 80, Some(highlight));
        assert_eq!(text(&lines), ["── rust ──", "│ LET X = 1;"]);
        assert_eq!(lines[1].spans.last().unwrap().style.role, Role::Accent);
    }
}
