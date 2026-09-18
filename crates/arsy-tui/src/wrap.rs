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

    let mut rows: Vec<Line> = Vec::new();
    let mut row = Line::new();
    let mut used = 0;

    for span in &line.spans {
        for word in split_keeping_spaces(span.text()) {
            let word_width = UnicodeWidthStr::width(word);

            // A space that falls exactly at the edge is dropped rather than
            // carried to the next row, where it would indent it by one.
            if word.trim().is_empty() && used + word_width > width {
                rows.push(std::mem::take(&mut row));
                used = 0;
                continue;
            }

            if used + word_width <= width {
                row = row.push_span(Span::new(word, span.style));
                used += word_width;
                continue;
            }

            // Does not fit here. Start a new row unless this row is empty, in
            // which case the word is wider than the width and has to be split.
            if used > 0 && word_width <= width {
                rows.push(std::mem::take(&mut row));
                used = 0;
                if word.trim().is_empty() {
                    continue;
                }
                row = row.push_span(Span::new(word, span.style));
                used = word_width;
                continue;
            }

            for chunk in split_to_width(word, width, used) {
                if used + UnicodeWidthStr::width(chunk.as_str()) > width {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
                used += UnicodeWidthStr::width(chunk.as_str());
                row = row.push_span(Span::new(chunk, span.style));
            }
        }
    }
    rows.push(row);

    // A line of only spaces wraps to nothing visible; give back one blank row
    // rather than an empty vector, so a caller counting rows is not surprised.
    if rows.is_empty() {
        rows.push(Line::blank());
    }
    rows
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
