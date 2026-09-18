//! The bordered card, once.
//!
//! The renderer had five of these — the launch banner, the approval dialog,
//! the thinking block, the shell card and the tool card — each with its own
//! copy of the same border arithmetic, spelled differently enough that they
//! had drifted apart in the details and would have had to be fixed five times.
//!
//! They are one shape: a rule with a title let into it, rows inset by one
//! column on each side, and a rule under them that may carry a title of its
//! own. The arithmetic below is the arithmetic those five were doing, so the
//! cards come out byte for byte where they did before.

use crate::{
    line::Line,
    style::{Role, Style},
};

/// Narrower than this and a card has no interior worth drawing, so every
/// caller's width is raised to it rather than producing a broken frame.
pub const MIN_WIDTH: usize = 20;

/// How many columns the frame itself takes: a border and a space on each side.
const FRAME: usize = 4;

/// What to draw. Titles are [`Line`]s rather than strings because a header
/// carries more than one style — an icon in the card's accent, a name, a
/// summary dimmed beside it.
pub struct BoxSpec<'a> {
    pub width: usize,
    /// Let into the top rule. `None` draws an unbroken rule.
    pub top_title: Option<Line>,
    /// Let into the bottom rule, where the cards put their status.
    pub bottom_title: Option<Line>,
    pub border: Style,
    /// Interior rows. Each is cut to the interior width and padded to it, so
    /// the right border lands in the same column on every row.
    pub body: &'a [Line],
}

impl<'a> BoxSpec<'a> {
    pub fn new(width: usize, border: Style, body: &'a [Line]) -> Self {
        Self {
            width,
            top_title: None,
            bottom_title: None,
            border,
            body,
        }
    }

    #[must_use]
    pub fn top(mut self, title: Line) -> Self {
        self.top_title = Some(title);
        self
    }

    #[must_use]
    pub fn bottom(mut self, title: Line) -> Self {
        self.bottom_title = Some(title);
        self
    }
}

/// Draw the card as rows, ready to be rendered or measured.
pub fn bordered_box(spec: &BoxSpec<'_>) -> Vec<Line> {
    let width = spec.width.max(MIN_WIDTH);
    let inner = width.saturating_sub(FRAME);
    let mut rows = Vec::with_capacity(spec.body.len() + 2);

    rows.push(top_rule(width, spec.top_title.as_ref(), spec.border));
    for line in spec.body {
        rows.push(body_row(line.clone(), inner, spec.border));
    }
    rows.push(bottom_rule(width, spec.bottom_title.as_ref(), spec.border));
    rows
}

/// A rule with an optional title let into it, two columns from the left.
///
/// The title is cut to what is left after the corners and that two-column
/// lead, so a long one shortens the trailing rule to nothing rather than
/// pushing the closing corner past the width.
pub fn rule(width: usize, open: char, close: char, title: Option<&Line>, border: Style) -> Line {
    rule_with_lead(width, open, close, title, border, 2)
}

/// Draw a rule with a caller-selected lead before its title.
pub fn rule_with_lead(
    width: usize,
    open: char,
    close: char,
    title: Option<&Line>,
    border: Style,
    lead: usize,
) -> Line {
    let Some(title) = title else {
        return Line::of(
            format!("{open}{}{close}", "─".repeat(width.saturating_sub(2))),
            border,
        );
    };
    let room = width.saturating_sub(1 + lead + 1);
    let title = title.clone().truncate(room);
    let trailing = width.saturating_sub(1 + lead + title.width() + 1);

    let mut row = Line::of(open.to_string(), border).push("─".repeat(lead), border);
    for span in title.spans {
        row = row.push_span(span);
    }
    row.push(format!("{}{close}", "─".repeat(trailing)), border)
}

/// The rule a card opens with, title and all.
pub fn top_rule(width: usize, title: Option<&Line>, border: Style) -> Line {
    rule(width.max(MIN_WIDTH), '╭', '╮', title, border)
}

/// The rule a card closes with. The cards put their status in this one.
pub fn bottom_rule(width: usize, title: Option<&Line>, border: Style) -> Line {
    rule(width.max(MIN_WIDTH), '╰', '╯', title, border)
}

/// The interior width a card of this width has for its rows.
pub const fn interior(width: usize) -> usize {
    let width = if width < MIN_WIDTH { MIN_WIDTH } else { width };
    width - FRAME
}

/// One interior row: border, space, content cut and padded to the interior,
/// space, border.
pub fn body_row(line: Line, inner: usize, border: Style) -> Line {
    let content = line.fit(inner).pad_to(inner, Role::Plain);
    let mut row = Line::of("│", border).push_plain(" ");
    for span in content.spans {
        row = row.push_span(span);
    }
    row.push_plain(" ").push("│", border)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Role;

    fn spec_rows(rows: &[Line]) -> Vec<String> {
        rows.iter().map(Line::text).collect()
    }

    /// Every row of a card is exactly the card's width, which is the thing the
    /// five hand-rolled copies each had to get right on their own.
    #[test]
    fn every_row_is_exactly_the_cards_width() {
        let body = [
            Line::of("short", Role::Dim),
            Line::of("a much longer row than the interior can hold", Role::Dim),
            Line::blank(),
        ];
        let spec = BoxSpec::new(40, Style::new(Role::Border), &body)
            .top(Line::of(" $ cargo test ", Role::Accent))
            .bottom(Line::of(" ✓ done (88ms) ", Role::Ok));
        for row in bordered_box(&spec) {
            assert_eq!(row.width(), 40, "{:?}", row.text());
        }
    }

    /// A width below the minimum is raised rather than drawn broken.
    #[test]
    fn a_width_below_the_minimum_is_raised() {
        let body = [Line::of("x", Role::Plain)];
        let spec = BoxSpec::new(4, Style::new(Role::Border), &body);
        for row in bordered_box(&spec) {
            assert_eq!(row.width(), MIN_WIDTH);
        }
    }

    /// A title longer than the rule shortens the trailing rule to nothing
    /// instead of pushing the corner past the width.
    #[test]
    fn an_over_long_title_never_pushes_the_corner_out() {
        let body: [Line; 0] = [];
        let spec = BoxSpec::new(24, Style::new(Role::Border), &body).top(Line::of(
            " $ a command far too long for this card ",
            Role::Accent,
        ));
        let rows = bordered_box(&spec);
        assert_eq!(rows[0].width(), 24);
        assert!(rows[0].text().starts_with("╭──"));
        assert!(rows[0].text().ends_with('╮'));
    }

    /// No title means an unbroken rule, which is what the thinking block's
    /// bottom has always drawn.
    #[test]
    fn a_rule_without_a_title_is_unbroken() {
        let body: [Line; 0] = [];
        let rows = bordered_box(&BoxSpec::new(20, Style::new(Role::Border), &body));
        assert_eq!(spec_rows(&rows)[0], "╭──────────────────╮");
        assert_eq!(spec_rows(&rows)[1], "╰──────────────────╯");
    }

    /// A body row keeps the style of what it holds; only the frame is border.
    #[test]
    fn a_body_row_keeps_its_own_styles() {
        let body = [Line::of("ok", Role::Ok).push(" then ", Role::Dim)];
        let rows = bordered_box(&BoxSpec::new(20, Style::new(Role::Border), &body));
        let roles: Vec<Role> = rows[1].spans.iter().map(|span| span.style.role).collect();
        assert!(roles.contains(&Role::Ok) && roles.contains(&Role::Dim));
        assert_eq!(roles.first(), Some(&Role::Border));
        assert_eq!(roles.last(), Some(&Role::Border));
    }
}
