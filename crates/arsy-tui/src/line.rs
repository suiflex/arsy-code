//! A row that stays measurable until the moment it is written.
//!
//! The renderer used to hand around rows that were already full of escape
//! codes. Measuring one meant stripping the codes back out; composing two
//! meant measuring both first; re-theming one was impossible. A [`Line`] is a
//! sequence of [`Span`]s that each know their [`Style`], so width is a sum
//! rather than a parse, and the escapes appear only in [`Line::render`].
//!
//! Untrusted text — a tool's output, a model's answer, a server's log — is
//! sanitised on the way in by [`Span::new`], so a control character cannot
//! reach the terminal and move the cursor out from under the renderer.

use crate::style::{Role, Style};
use unicode_width::UnicodeWidthStr;

/// Reset, after any styled run.
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";

/// One run of text with one style.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Span {
    text: String,
    pub style: Style,
}

impl Span {
    /// Text with a style, sanitised.
    ///
    /// Control characters are dropped rather than escaped or refused: this is
    /// the boundary where a tool's output becomes something the terminal will
    /// act on, and a row that silently loses a stray `\r` is better than one
    /// that repaints the line above it. Tabs and newlines go too — a span is
    /// one row, and a caller that wants two says so by making two.
    pub fn new(text: impl AsRef<str>, style: impl Into<Style>) -> Self {
        Self {
            text: text
                .as_ref()
                .chars()
                .filter(|character| !character.is_control())
                .collect(),
            style: style.into(),
        }
    }

    /// Unstyled text.
    pub fn plain(text: impl AsRef<str>) -> Self {
        Self::new(text, Role::Plain)
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Printed columns, which is not the byte length and not the character
    /// count: a CJK glyph is two columns wide and an accent is none.
    pub fn width(&self) -> usize {
        UnicodeWidthStr::width(self.text.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// One row of the transcript, composer, or a widget's interior.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub const fn new() -> Self {
        Self { spans: Vec::new() }
    }

    /// One span's worth of row.
    pub fn of(text: impl AsRef<str>, style: impl Into<Style>) -> Self {
        Self::new().push(text, style)
    }

    /// A blank row.
    pub fn blank() -> Self {
        Self::new()
    }

    #[must_use]
    pub fn push(mut self, text: impl AsRef<str>, style: impl Into<Style>) -> Self {
        let span = Span::new(text, style);
        if !span.is_empty() {
            self.spans.push(span);
        }
        self
    }

    #[must_use]
    pub fn push_plain(self, text: impl AsRef<str>) -> Self {
        self.push(text, Role::Plain)
    }

    #[must_use]
    pub fn push_span(mut self, span: Span) -> Self {
        if !span.is_empty() {
            self.spans.push(span);
        }
        self
    }

    /// Pad to `width` columns with spaces, so a card's interior rows all reach
    /// its right border. A row already at or past the width is left alone.
    #[must_use]
    pub fn pad_to(self, width: usize, style: impl Into<Style>) -> Self {
        let missing = width.saturating_sub(self.width());
        if missing == 0 {
            return self;
        }
        self.push(" ".repeat(missing), style)
    }

    /// Printed columns across every span.
    pub fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(Span::is_empty)
    }

    /// The row's text with no styling, which is what a test asserts on and
    /// what a non-colour terminal is given.
    pub fn text(&self) -> String {
        self.spans.iter().map(Span::text).collect()
    }

    /// Cut to `width` printed columns, keeping each surviving span's style.
    ///
    /// A double-width glyph that straddles the edge is dropped rather than
    /// half-printed, so the result never overflows the width it was given.
    #[must_use]
    pub fn truncate(self, width: usize) -> Self {
        if self.width() <= width {
            return self;
        }
        let mut out = Self::new();
        let mut used = 0;
        for span in self.spans {
            if used >= width {
                break;
            }
            let mut kept = String::new();
            for character in span.text.chars() {
                let step = UnicodeWidthStr::width(character.to_string().as_str());
                if used + step > width {
                    used = width;
                    break;
                }
                kept.push(character);
                used += step;
            }
            out = out.push(kept, span.style);
        }
        out
    }

    /// Cut to `width` columns with an ellipsis where text was dropped.
    ///
    /// The ellipsis takes one of the columns, so the result still fits. This
    /// is what the renderer has always done to an over-long row — a path, a
    /// command, a tool summary — and the `…` is the only signal that anything
    /// was lost, so it is not optional.
    #[must_use]
    pub fn fit(self, width: usize) -> Self {
        if self.width() <= width {
            return self;
        }
        let style = self.spans.last().map_or(Style::PLAIN, |span| span.style);
        self.truncate(width.saturating_sub(1)).push("…", style)
    }

    /// Serialise to what the terminal is given.
    ///
    /// With `colour` off this is the plain text, so `--no-color` and `NO_COLOR`
    /// cost nothing at the call site. With it on, each styled run is wrapped in
    /// its palette code and a reset — the shape the renderer has always
    /// written, so a row survives being cut by something that only knows how
    /// to look for `\x1b[0m`.
    ///
    /// Neighbouring spans that share a style are written as one run. They mean
    /// the same thing painted either way, but a row assembled in pieces — text
    /// and then the ellipsis that replaced its tail — would otherwise carry an
    /// escape at every seam, and how a row was built is not something the
    /// terminal should be able to tell.
    pub fn render(&self, palette: &crate::Palette, colour: bool) -> String {
        if !colour {
            return self.text();
        }
        let mut out = String::with_capacity(self.width() + 16);
        let mut spans = self.spans.iter().peekable();
        while let Some(span) = spans.next() {
            let mut run = span.text.clone();
            while spans.peek().is_some_and(|next| next.style == span.style) {
                run.push_str(&spans.next().expect("peeked").text);
            }
            if span.style.is_plain() {
                out.push_str(&run);
                continue;
            }
            if span.style.bold {
                out.push_str(BOLD);
            }
            if let Some(code) = palette.code(span.style.role) {
                out.push_str(code);
            }
            out.push_str(&run);
            out.push_str(RESET);
        }
        out
    }
}

impl From<Span> for Line {
    fn from(span: Span) -> Self {
        Self::new().push_span(span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::builtin_palette;

    fn palette() -> crate::Palette {
        builtin_palette("dark").expect("dark is a built-in theme")
    }

    /// A control character in a tool's output must not reach the terminal: a
    /// stray carriage return would repaint the row above the one being drawn.
    #[test]
    fn a_span_drops_control_characters_from_untrusted_text() {
        let span = Span::plain("ok\r\x1b[2Jgone\n");
        assert_eq!(span.text(), "ok[2Jgone");
        assert!(!span.text().contains('\x1b'));
        assert!(!span.text().contains('\r'));
    }

    /// Width is columns, not bytes and not characters.
    #[test]
    fn width_counts_printed_columns() {
        assert_eq!(Span::plain("abc").width(), 3);
        assert_eq!(Span::plain("日本").width(), 4, "CJK glyphs are two columns");
        assert_eq!(
            Line::of("ab", Role::Accent).push("cd", Role::Dim).width(),
            4,
            "width is a sum across spans, not a parse of escapes"
        );
    }

    /// Truncation keeps styles and never overflows, including where a
    /// double-width glyph straddles the edge.
    #[test]
    fn truncation_keeps_style_and_never_overflows() {
        let line = Line::of("hello ", Role::Accent).push("world", Role::Err);
        let cut = line.clone().truncate(8);
        assert_eq!(cut.text(), "hello wo");
        assert_eq!(cut.width(), 8);
        assert_eq!(cut.spans[0].style.role, Role::Accent);
        assert_eq!(cut.spans[1].style.role, Role::Err);

        assert_eq!(line.clone().truncate(99), line, "a short row is untouched");

        // The second glyph would take columns 3 and 4; at a width of 3 it is
        // dropped whole rather than printed as half a character.
        let wide = Line::of("日本", Role::Plain).truncate(3);
        assert_eq!(wide.text(), "日");
        assert_eq!(wide.width(), 2);
    }

    /// Colour off is the plain text, so `--no-color` costs nothing at the call
    /// site; colour on wraps each styled run and resets after it.
    #[test]
    fn rendering_matches_the_shape_the_renderer_has_always_written() {
        let line = Line::of("done", Role::Ok)
            .push_plain(" · ")
            .push("88ms", Style::new(Role::Dim).bold());
        assert_eq!(line.render(&palette(), false), "done · 88ms");

        let palette = palette();
        let painted = line.render(&palette, true);
        let ok = palette.code(Role::Ok).expect("ok has a colour");
        assert!(painted.starts_with(ok), "{painted:?}");
        assert!(painted.contains("\x1b[0m · "), "plain runs stay unpainted");
        assert!(
            painted.contains("\x1b[1m"),
            "bold is an attribute, not a role"
        );
    }

    /// An over-long row is cut with the ellipsis the renderer has always
    /// shown, and the ellipsis is inside the width rather than past it.
    #[test]
    fn fitting_marks_what_it_dropped_and_still_fits() {
        let line = Line::of("a command far too long", Role::Accent);
        let cut = line.clone().fit(10);
        assert_eq!(cut.text(), "a command…");
        assert_eq!(cut.width(), 10);
        assert_eq!(
            cut.spans.last().expect("a span").style.role,
            Role::Accent,
            "the ellipsis is painted with the text it replaced"
        );
        assert_eq!(line.clone().fit(99), line, "a short row keeps no ellipsis");
    }

    /// How a row was assembled must not be visible: a row built in two pieces
    /// of one style paints as one run, not as two with a seam between them.
    #[test]
    fn neighbouring_spans_of_one_style_paint_as_one_run() {
        let palette = palette();
        let split = Line::of("out", Role::Dim).push("…", Role::Dim);
        let whole = Line::of("out…", Role::Dim);
        assert_eq!(split.render(&palette, true), whole.render(&palette, true));
        assert_eq!(split.render(&palette, true).matches("\x1b[0m").count(), 1);
    }

    /// Padding reaches a card's right border and never shortens a row that is
    /// already past it.
    #[test]
    fn padding_fills_to_a_width_but_never_trims() {
        let line = Line::of("ab", Role::Plain).pad_to(5, Role::Plain);
        assert_eq!(line.text(), "ab   ");
        let long = Line::of("abcdef", Role::Plain).pad_to(3, Role::Plain);
        assert_eq!(long.text(), "abcdef");
    }
}
