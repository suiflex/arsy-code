//! Word wrapping, which the renderer has never had.
//!
//! Long rows were cut with an ellipsis, so a sentence wider than the terminal
//! simply lost its end. That is tolerable for a file path and wrong for an
//! assistant's answer, which is the thing an operator is actually reading.
//!
//! Wrapping works on [`Line`]s rather than on strings so a span's style
//! survives the break, and on printed columns rather than characters so a CJK
//! answer wraps where it looks like it should.

use crate::line::{Line, Span};
use crate::style::Style;
use unicode_width::UnicodeWidthStr;

/// Break `line` into rows no wider than `width` columns.
///
/// Breaks at spaces where it can. A word longer than the whole width — a URL,
/// a base64 blob, a path with no separators — is broken mid-word rather than
/// allowed to overflow, because overflowing is what tears a bordered card.
///
/// A `width` of zero yields the line unchanged: a caller that has no room has
/// a layout bug, and silently returning nothing would hide it.
pub fn wrap(line: &Line, width: usize) -> Vec<Line> {
    if width == 0 || line.width() <= width {
        return vec![line.clone()];
    }

    let mut wrapped = Wrapping::new(width);
    for span in &line.spans {
        wrapped.place(span);
    }
    wrapped.finish()
}

/// Wrap `body` behind `lead`, as a list item or a quoted line wraps: the
/// first row starts with `lead`, every later one with `hang`, and the body
/// fills the columns left beside them.
///
/// `hang` is expected to be as wide as `lead`. With no room left for a body,
/// this is [`wrap`] of the two joined.
pub fn wrap_hanging(lead: &Line, body: &Line, hang: &Line, width: usize) -> Vec<Line> {
    let indent = lead.width();
    let behind = |head: &Line, row: Line| {
        head.spans
            .iter()
            .cloned()
            .chain(row.spans)
            .fold(Line::new(), Line::push_span)
    };
    if width == 0 || indent == 0 || indent >= width {
        return wrap(&behind(lead, body.clone()), width);
    }
    wrap(body, width - indent)
        .into_iter()
        .enumerate()
        .map(|(index, row)| behind(if index == 0 { lead } else { hang }, row))
        .collect()
}

/// The rows a wrap has produced, and the one still being filled.
///
/// The row in progress is state rather than a local because a break happens in
/// the middle of a span's words rather than at the edge of one: a word too wide
/// for the whole row is cut into pieces, and each piece has to be placed with
/// whatever the previous piece left behind.
struct Wrapping {
    width: usize,
    rows: Vec<Line>,
    row: Line,
    used: usize,
}

impl Wrapping {
    fn new(width: usize) -> Self {
        Self {
            width,
            rows: Vec::new(),
            row: Line::new(),
            used: 0,
        }
    }

    /// The rows built, with the row still being filled last.
    fn finish(mut self) -> Vec<Line> {
        self.rows.push(self.row);
        self.rows
    }

    /// Break the row here: it is finished, and the next word starts a new one.
    fn break_row(&mut self) {
        self.rows.push(std::mem::take(&mut self.row));
        self.used = 0;
    }

    /// Put one span's words on the rows, breaking where they do not fit.
    fn place(&mut self, span: &Span) {
        for word in split_keeping_spaces(span.text()) {
            self.place_word(word, span.style);
        }
    }

    /// Put one word on the row being filled, breaking and cutting as needed.
    fn place_word(&mut self, word: &str, style: Style) {
        let word_width = UnicodeWidthStr::width(word);

        // A space that falls exactly at the edge is dropped rather than
        // carried to the next row, where it would indent it by one.
        if word.trim().is_empty() && self.used + word_width > self.width {
            self.break_row();
            return;
        }

        if self.used + word_width <= self.width {
            self.push(word, style, word_width);
            return;
        }

        // Does not fit here. Start a new row unless this row is empty, in
        // which case the word is wider than the width and has to be split.
        if self.used > 0 && word_width <= self.width {
            self.break_row();
            if word.trim().is_empty() {
                return;
            }
            self.push(word, style, word_width);
            return;
        }

        for chunk in split_to_width(word, self.width, self.used) {
            let chunk_width = UnicodeWidthStr::width(chunk.as_str());
            if self.used + chunk_width > self.width {
                self.break_row();
            }
            self.push(&chunk, style, chunk_width);
        }
    }

    /// Add one piece of a span to the row being filled.
    fn push(&mut self, text: &str, style: Style, columns: usize) {
        self.row = std::mem::take(&mut self.row).push_span(Span::new(text, style));
        self.used += columns;
    }
}

/// Words and the runs of spaces between them, in order, so a break can happen
/// at a space without losing the spacing inside a row.
fn split_keeping_spaces(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_space = None;
    for (at, character) in text.char_indices() {
        let space = character == ' ';
        match in_space {
            Some(was) if was != space => {
                parts.push(&text[start..at]);
                start = at;
                in_space = Some(space);
            }
            None => in_space = Some(space),
            _ => {}
        }
    }
    if start < text.len() {
        parts.push(&text[start..]);
    }
    parts
}

/// Cut an over-long word into pieces that fit, the first one taking whatever
/// is left of the current row.
///
/// A glyph wider than the whole width is dropped. There is no arrangement of
/// columns that shows it, and the promise this function exists to keep is that
/// nothing overflows — a row one column too wide is what pushes a card's
/// border out of line and tears the frame. Dropping is visible and local;
/// overflowing corrupts everything drawn around it.
fn split_to_width(word: &str, width: usize, already_used: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut room = width.saturating_sub(already_used).max(1);
    let mut used = 0;
    for character in word.chars() {
        let step = UnicodeWidthStr::width(character.to_string().as_str());
        if step > width {
            continue;
        }
        if used + step > room {
            chunks.push(std::mem::take(&mut chunk));
            room = width.max(1);
            used = 0;
        }
        chunk.push(character);
        used += step;
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Role;

    fn texts(rows: &[Line]) -> Vec<String> {
        rows.iter().map(Line::text).collect()
    }

    #[test]
    fn a_row_that_fits_is_left_alone() {
        let line = Line::of("short enough", Role::Plain);
        assert_eq!(texts(&wrap(&line, 40)), ["short enough"]);
        assert_eq!(
            texts(&wrap(&line, 0)),
            ["short enough"],
            "no room is a layout bug, not a reason to return nothing"
        );
    }

    #[test]
    fn breaking_happens_at_spaces_and_never_overflows() {
        let line = Line::of("the gateway call inherited the outer deadline", Role::Plain);
        let rows = wrap(&line, 20);
        for row in &rows {
            assert!(
                row.width() <= 20,
                "{:?} is {} wide",
                row.text(),
                row.width()
            );
        }
        assert!(rows.len() > 1);
        assert_eq!(
            rows.iter()
                .map(Line::text)
                .collect::<String>()
                .replace(' ', ""),
            "thegatewaycallinheritedtheouterdeadline",
            "wrapping loses no text"
        );
    }

    /// A URL has nowhere to break, and overflowing is what tears a card.
    #[test]
    fn a_word_wider_than_the_width_is_split_rather_than_overflowed() {
        let line = Line::of(
            "https://example.internal/a/very/long/path/indeed",
            Role::Accent,
        );
        let rows = wrap(&line, 12);
        for row in &rows {
            assert!(row.width() <= 12, "{:?}", row.text());
        }
        assert_eq!(
            rows.iter().map(Line::text).collect::<String>(),
            "https://example.internal/a/very/long/path/indeed"
        );
    }

    /// The break must not lose which span a word came from, or a wrapped
    /// answer would come out one flat colour.
    #[test]
    fn style_survives_the_break() {
        let line = Line::of("alpha beta ", Role::Ok).push("gamma delta", Role::Err);
        let rows = wrap(&line, 11);
        assert!(rows.len() > 1);
        let roles: Vec<Role> = rows
            .iter()
            .flat_map(|row| row.spans.iter().map(|span| span.style.role))
            .collect();
        assert!(roles.contains(&Role::Ok) && roles.contains(&Role::Err));
    }

    /// A glyph that cannot fit at any position is dropped rather than allowed
    /// to overflow: one column too wide is what tears a bordered card.
    #[test]
    fn a_glyph_wider_than_the_width_is_dropped_not_overflowed() {
        let line = Line::of("日本", Role::Plain);
        let rows = wrap(&line, 1);
        for row in &rows {
            assert!(row.width() <= 1, "{:?} is {} wide", row.text(), row.width());
        }
        assert_eq!(
            rows.iter().map(Line::text).collect::<String>(),
            "",
            "nothing two columns wide can be shown in one column"
        );
    }

    /// Two columns per glyph, so a CJK answer wraps where it looks like it
    /// should rather than at twice the width.
    #[test]
    fn wrapping_counts_columns_not_characters() {
        let line = Line::of("日本語のテキスト", Role::Plain);
        let rows = wrap(&line, 6);
        for row in &rows {
            assert!(row.width() <= 6, "{:?} is {} wide", row.text(), row.width());
        }
        assert_eq!(
            rows.iter().map(Line::text).collect::<String>(),
            "日本語のテキスト"
        );
    }
}
