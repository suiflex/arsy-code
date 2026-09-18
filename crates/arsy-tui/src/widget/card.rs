//! The tool card: a tinted panel with a titled rule and a trailer.
//!
//! [`bordered_box`](super::bordered_box) draws the plain frame the classic
//! style has always used. This is the other shape — the one the mockup asks
//! for: the same border arithmetic, but every row painted onto a category
//! background, the title in that category's accent, and a trailer pinned to
//! the right of the top rule where the duration goes.
//!
//! The fill is the part that is easy to get wrong. A panel whose tint stops
//! where its text stops is not a panel, it is a highlight, so every row is
//! padded to the full width *with the background style* rather than with plain
//! spaces.

use crate::{
    line::Line,
    style::{Role, Style},
};

use super::boxes::{interior, MIN_WIDTH};

/// What to draw.
pub struct CardSpec<'a> {
    pub width: usize,
    /// Let into the top rule, in the card's accent.
    pub title: Line,
    /// Pinned to the right of the top rule — the duration, in the mockup.
    pub trailer: Option<Line>,
    /// Let into the bottom rule: the status the card finished with.
    pub status: Option<Line>,
    /// The category's border colour.
    pub border: Role,
    /// The category's panel colour. `None` draws an untinted card, which is
    /// what a neutral theme gets.
    pub background: Option<Role>,
    pub body: &'a [Line],
}

impl<'a> CardSpec<'a> {
    pub fn new(width: usize, border: Role, body: &'a [Line]) -> Self {
        Self {
            width,
            title: Line::new(),
            trailer: None,
            status: None,
            border,
            background: None,
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

    #[must_use]
    pub fn status(mut self, status: Line) -> Self {
        self.status = Some(status);
        self
    }

    #[must_use]
    pub const fn background(mut self, background: Role) -> Self {
        self.background = Some(background);
        self
    }
}

/// Draw the card as rows.
pub fn card(spec: &CardSpec<'_>) -> Vec<Line> {
    let width = spec.width.max(MIN_WIDTH);
    let inner = interior(width);
    let panel = spec.background;

    let mut rows = Vec::with_capacity(spec.body.len() + 2);
    rows.push(top(width, spec, panel));
    for line in spec.body {
        rows.push(body_row(line.clone(), inner, spec.border, panel));
    }
    rows.push(bottom(width, spec, panel));
    rows
}

/// `╭── title ──────── trailer ─╮`, with the trailer pushed to the right.
///
/// The title is cut first and the trailer keeps its columns, because a card
/// whose duration was the thing elided would have elided the one part that is
/// the same width every time.
fn top(width: usize, spec: &CardSpec<'_>, panel: Option<Role>) -> Line {
    let border = styled(spec.border, panel);
    let lead = 2;
    let trailer = spec.trailer.clone().unwrap_or_default();
    let trailer_width = if trailer.is_empty() {
        0
    } else {
        trailer.width() + 1
    };

    let room = width
        .saturating_sub(1 + lead + 1)
        .saturating_sub(trailer_width);
    let title = repaint(spec.title.clone().fit(room), panel);
    let filled = 1 + lead + title.width() + trailer_width + 1;
    let rule = width.saturating_sub(filled);

    let mut row = Line::of(format!("╭{}", "─".repeat(lead)), border);
    for span in title.spans {
        row = row.push_span(span);
    }
    row = row.push("─".repeat(rule), border);
    if !trailer.is_empty() {
        // The one space between the rule and the trailer belongs to the card,
        // so a caller cannot accidentally leave the duration flush against the
        // rule or floating two columns off the corner.
        row = row.push(" ", border);
        for span in repaint(trailer, panel).spans {
            row = row.push_span(span);
        }
    }
    row.push("╮", border)
}

/// `╰── status ─────╯`, or an unbroken rule when the card has no status.
fn bottom(width: usize, spec: &CardSpec<'_>, panel: Option<Role>) -> Line {
    let border = styled(spec.border, panel);
    let Some(status) = spec.status.clone() else {
        return Line::of(format!("╰{}╯", "─".repeat(width.saturating_sub(2))), border);
    };
    let lead = 2;
    let status = repaint(status.fit(width.saturating_sub(1 + lead + 1)), panel);
    let rule = width.saturating_sub(1 + lead + status.width() + 1);

    let mut row = Line::of(format!("╰{}", "─".repeat(lead)), border);
    for span in status.spans {
        row = row.push_span(span);
    }
    row.push(format!("{}╯", "─".repeat(rule)), border)
}

/// One interior row, padded to the panel's full interior so the tint reaches
/// the right border instead of stopping where the text does.
fn body_row(line: Line, inner: usize, border: Role, panel: Option<Role>) -> Line {
    let frame = styled(border, panel);
    let filler = panel.map_or(Style::PLAIN, |bg| Style::new(Role::Plain).on(bg));
    let content = repaint(line.fit(inner), panel).pad_to(inner, filler);

    let mut row = Line::of("│", frame).push(" ", filler);
    for span in content.spans {
        row = row.push_span(span);
    }
    row.push(" ", filler).push("│", frame)
}

/// A foreground role on the card's panel.
fn styled(role: Role, panel: Option<Role>) -> Style {
    let style = Style::new(role);
    match panel {
        Some(bg) => style.on(bg),
        None => style,
    }
}

/// Put every span of a line onto the card's panel, keeping its own colour.
fn repaint(line: Line, panel: Option<Role>) -> Line {
    let Some(bg) = panel else {
        return line;
    };
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

    fn tinted(width: usize, body: &[Line]) -> Vec<Line> {
        card(
            &CardSpec::new(width, Role::ToolMcpBorder, body)
                .title(Line::of(" ⌘ mcp.call observability ", Role::ToolMcpAccent))
                .trailer(Line::of("612ms", Role::Dim))
                .status(Line::of(" ✓ done ", Role::Ok))
                .background(Role::ToolMcpBg),
        )
    }

    /// Every row is exactly the card's width — the promise the five hand-rolled
    /// borders each had to keep on their own.
    #[test]
    fn every_row_is_exactly_the_cards_width() {
        let body = [
            Line::of("p99 30.0s · timeout ceiling reached", Role::Dim),
            Line::blank(),
            Line::of(
                "a row far wider than this card's interior can ever hold",
                Role::Dim,
            ),
        ];
        for width in [MIN_WIDTH, 32, 72, 120] {
            for row in tinted(width, &body) {
                assert_eq!(row.width(), width, "{:?} at width {width}", row.text());
            }
        }
    }

    /// The trailer is pinned to the right of the top rule, one column in from
    /// the corner, which is where the mockup puts the duration.
    #[test]
    fn the_trailer_sits_against_the_right_corner() {
        let body: [Line; 0] = [];
        let top = tinted(60, &body).remove(0);
        let text = top.text();
        assert!(text.starts_with("╭──"), "{text}");
        assert!(text.ends_with(" 612ms╮"), "{text}");
        assert_eq!(top.width(), 60);
    }

    /// A card with no room for both keeps the trailer and elides the title:
    /// the duration is the same width every time, the title is not.
    #[test]
    fn a_narrow_card_elides_the_title_and_keeps_the_trailer() {
        let body: [Line; 0] = [];
        let top = tinted(MIN_WIDTH, &body).remove(0);
        assert_eq!(top.width(), MIN_WIDTH);
        assert!(top.text().contains("612ms"), "{}", top.text());
    }

    /// The tint reaches the right border. A panel that stops where its text
    /// stops is a highlight, not a panel.
    #[test]
    fn the_panel_fills_the_whole_row() {
        let body = [Line::of("short", Role::Dim)];
        let rows = tinted(40, &body);
        for row in &rows {
            let painted = row.width();
            let on_panel: usize = row
                .spans
                .iter()
                .filter(|span| span.style.bg == Some(Role::ToolMcpBg))
                .map(crate::Span::width)
                .sum();
            assert_eq!(on_panel, painted, "{:?} is not fully tinted", row.text());
        }
    }

    /// A body row keeps its own colours on top of the panel rather than being
    /// flattened to one.
    #[test]
    fn body_text_keeps_its_colour_on_the_panel() {
        let body = [Line::of("+ added", Role::Ok).push(" - removed", Role::Err)];
        let row = tinted(40, &body).remove(1);
        let roles: Vec<Role> = row.spans.iter().map(|span| span.style.role).collect();
        assert!(
            roles.contains(&Role::Ok) && roles.contains(&Role::Err),
            "{roles:?}"
        );
        assert!(row
            .spans
            .iter()
            .all(|span| span.style.bg == Some(Role::ToolMcpBg)));
    }

    /// Without a background the card is a plain frame, which is what a neutral
    /// theme draws.
    #[test]
    fn a_card_without_a_panel_paints_no_background() {
        let body = [Line::of("plain", Role::Dim)];
        let rows =
            card(&CardSpec::new(40, Role::Border, &body).title(Line::of(" $ ls ", Role::Accent)));
        for row in &rows {
            assert_eq!(row.width(), 40);
            assert!(row.spans.iter().all(|span| span.style.bg.is_none()));
        }
    }
}
