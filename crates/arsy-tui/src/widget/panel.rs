//! A panel: a block of colour, not a box of lines.
//!
//! [`bordered_box`](super::bordered_box) draws a frame out of `╭ │ ╰`. This
//! draws nothing of the sort. What makes a panel readable is the tint itself —
//! a header strip in the category's colour carrying the name and, at the right
//! edge, how long the call took, with the body on a darker tint beneath it and
//! a thin accent stripe down the left.
//!
//! The distinction matters more than it sounds. A transcript full of
//! line-drawing reads as a stack of wireframes; the same transcript as blocks
//! of colour reads as a sequence of steps, which is what it is.
//!
//! The fill is the whole trick: every row is padded to the full width *in its
//! own background style*. A tint that stops where its text stops is a
//! highlight, not a panel.

use crate::{
    line::Line,
    style::{Role, Style},
};

use super::boxes::MIN_WIDTH;

/// The stripe down the left edge. A half-block, so it reads as an edge rather
/// than as a character someone typed.
const STRIPE: &str = "▌";

/// How many columns the stripe and the space after it take.
const LEAD: usize = 2;

pub struct PanelSpec<'a> {
    pub width: usize,
    /// The icon and name, in the category's accent.
    pub title: Line,
    /// Pinned to the right edge of the header — the duration.
    pub trailer: Option<Line>,
    /// The stripe, and the colour the title is painted in.
    pub accent: Role,
    /// The header strip's tint.
    pub head_bg: Role,
    /// The body's tint, darker than the header's.
    pub body_bg: Role,
    pub body: &'a [Line],
}

impl<'a> PanelSpec<'a> {
    pub fn new(width: usize, accent: Role, head_bg: Role, body_bg: Role, body: &'a [Line]) -> Self {
        Self {
            width,
            title: Line::new(),
            trailer: None,
            accent,
            head_bg,
            body_bg,
            body,
        }
    }

    #[must_use]
    pub fn title(mut self, title: Line) -> Self {
        self.title = title;
        self
    }

    #[must_use]
    pub fn trailer(mut self, trailer: Line) -> Self {
        self.trailer = Some(trailer);
        self
    }
}

/// Draw the panel as rows, every one exactly `width` columns.
pub fn panel(spec: &PanelSpec<'_>) -> Vec<Line> {
    let width = spec.width.max(MIN_WIDTH);
    let mut rows = Vec::with_capacity(spec.body.len() + 1);
    rows.push(header(width, spec));
    for line in spec.body {
        rows.push(body_row(width, line.clone(), spec.accent, spec.body_bg));
    }
    rows
}

/// `▌ ⌕ code.references chargeGateway                        240ms `
///
/// The title is cut before the trailer is, because the duration is the same
/// handful of columns every time and the name is not — eliding the number
/// would lose the one part a reader is comparing between calls.
fn header(width: usize, spec: &PanelSpec<'_>) -> Line {
    let bg = spec.head_bg;
    let trailer = spec.trailer.clone().unwrap_or_default();
    // One column of padding after the duration, so it does not sit flush
    // against the terminal's edge.
    let trailer_width = if trailer.is_empty() {
        0
    } else {
        trailer.width() + 1
    };
    let room = width.saturating_sub(LEAD).saturating_sub(trailer_width);
    let title = on(spec.title.clone().fit(room), bg);

    let mut row = Line::of(STRIPE, Style::new(spec.accent).on(bg)).push(" ", Style::new(bg).on(bg));
    for span in title.spans {
        row = row.push_span(span);
    }
    // Pad out to where the trailer begins; the gap is the panel, not a gutter.
    row = row.pad_to(width.saturating_sub(trailer_width), Style::new(bg).on(bg));
    if !trailer.is_empty() {
        for span in on(trailer, bg).spans {
            row = row.push_span(span);
        }
        row = row.push(" ", Style::new(bg).on(bg));
    }
    row
}

/// One body row: the stripe, the text, and tint to the right edge.
fn body_row(width: usize, line: Line, accent: Role, bg: Role) -> Line {
    let filler = Style::new(bg).on(bg);
    let content = on(line.fit(width.saturating_sub(LEAD)), bg);

    let mut row = Line::of(STRIPE, Style::new(accent).on(bg)).push(" ", filler);
    for span in content.spans {
        row = row.push_span(span);
    }
    row.pad_to(width, filler)
}

/// Put every span onto the panel's tint, keeping the colour it already had.
fn on(line: Line, bg: Role) -> Line {
    let mut out = Line::new();
    for span in line.spans {
        let style = span.style.on(bg);
        out = out.push(span.text(), style);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawn(width: usize, body: &[Line]) -> Vec<Line> {
        panel(
            &PanelSpec::new(
                width,
                Role::ToolMcpAccent,
                Role::ToolMcpHeadBg,
                Role::ToolMcpBg,
                body,
            )
            .title(Line::of("⌘ mcp.call observability", Role::ToolMcpAccent))
            .trailer(Line::of("612ms", Role::Dim)),
        )
    }

    /// The thing the whole redesign is about: a panel is colour, not lines.
    #[test]
    fn a_panel_contains_no_box_drawing_at_all() {
        let body = [Line::of("p99 30.0s · timeout ceiling reached", Role::Dim)];
        for row in drawn(72, &body) {
            let text = row.text();
            for glyph in ['╭', '╮', '╰', '╯', '│', '─'] {
                assert!(!text.contains(glyph), "{glyph} found in {text:?}");
            }
        }
    }

    /// Every row reaches the right edge, which is what makes it read as a
    /// block of colour rather than as a highlight behind some words.
    #[test]
    fn every_row_fills_the_width_on_its_tint() {
        let body = [
            Line::of("short", Role::Dim),
            Line::blank(),
            Line::of(
                "a row far wider than this panel can possibly hold",
                Role::Dim,
            ),
        ];
        for width in [MIN_WIDTH, 40, 72, 120] {
            for row in drawn(width, &body) {
                assert_eq!(row.width(), width, "{:?} at width {width}", row.text());
                let tinted: usize = row
                    .spans
                    .iter()
                    .filter(|span| span.style.bg.is_some())
                    .map(crate::Span::width)
                    .sum();
                assert_eq!(tinted, width, "not tinted edge to edge: {:?}", row.text());
            }
        }
    }

    /// The duration sits at the right edge, one column in.
    #[test]
    fn the_duration_sits_at_the_right_edge() {
        let body: [Line; 0] = [];
        let head = drawn(60, &body).remove(0);
        assert!(head.text().ends_with("612ms "), "{:?}", head.text());
        assert!(head.text().starts_with("▌ ⌘ mcp.call"), "{:?}", head.text());
    }

    /// A narrow panel elides the name and keeps the number, because the number
    /// is what a reader compares between calls.
    #[test]
    fn a_narrow_panel_keeps_the_duration_and_elides_the_name() {
        let body: [Line; 0] = [];
        let head = drawn(MIN_WIDTH, &body).remove(0);
        assert_eq!(head.width(), MIN_WIDTH);
        assert!(head.text().contains("612ms"), "{:?}", head.text());
    }

    /// The header is a different tint from the body; that contrast is what
    /// separates one call from the next without a line between them.
    #[test]
    fn the_header_and_body_carry_different_tints() {
        let body = [Line::of("output", Role::Dim)];
        let rows = drawn(40, &body);
        let bg_of = |row: &Line| row.spans.last().expect("a span").style.bg;
        assert_eq!(bg_of(&rows[0]), Some(Role::ToolMcpHeadBg));
        assert_eq!(bg_of(&rows[1]), Some(Role::ToolMcpBg));
    }

    /// Body text keeps its own colour on the tint rather than being flattened.
    #[test]
    fn body_text_keeps_its_colour() {
        let body = [Line::of("+ added", Role::Ok).push(" - removed", Role::Err)];
        let row = drawn(40, &body).remove(1);
        let roles: Vec<Role> = row.spans.iter().map(|span| span.style.role).collect();
        assert!(
            roles.contains(&Role::Ok) && roles.contains(&Role::Err),
            "{roles:?}"
        );
    }
}
