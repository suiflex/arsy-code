//! A change drawn as a diff rather than as text that happens to start with
//! `+` and `-`.
//!
//! The tools already write their changes in one of two shapes: `fs.edit` and
//! `fs.write` number every line (`+  12 │ text`), and a patch or an approval
//! preview is unified (`+text`, `@@`, `*** Update File:`). Both are read here
//! into rows with the sign and the number set off in a gutter, added and
//! removed lines in the theme's ok and error roles, and — where one block of
//! lines replaced another of the same length — the words that changed shown
//! reversed, so the eye lands on the edit and not on the line around it.
//! Reversing takes no colour of its own, so it reads the same in `mono`.

use arsy_tui::{Line, Role, Style};
use similar::{ChangeTag, TextDiff};

/// How many lines a diff added and removed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiffStats {
    pub added: usize,
    pub removed: usize,
}

impl DiffStats {
    /// `+3 −2`, the two counts in their own colours.
    pub fn line(self) -> Line {
        Line::of(format!("+{}", self.added), Role::Ok)
            .push(" ", Role::Plain)
            .push(format!("−{}", self.removed), Role::Err)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    /// `--- a`, `+++ b`, `*** Update File: c`: which file this is.
    Header,
    /// `@@ … @@`: where in the file.
    Hunk,
    Added,
    Removed,
    /// Unchanged, or anything else the tool wrote alongside the diff.
    Context,
}

struct Row<'a> {
    kind: Kind,
    /// The line number and its bar, when the tool wrote one.
    gutter: &'a str,
    text: &'a str,
}

/// `text` as diff rows and what it added and removed, or `None` when it
/// adds and removes nothing and so is not a diff at all.
pub fn diff_lines(text: &str) -> Option<(Vec<Line>, DiffStats)> {
    let rows: Vec<Row<'_>> = text.lines().map(classify).collect();
    let stats = DiffStats {
        added: rows.iter().filter(|row| row.kind == Kind::Added).count(),
        removed: rows.iter().filter(|row| row.kind == Kind::Removed).count(),
    };
    if stats.added + stats.removed == 0 {
        return None;
    }
    let mut lines = Vec::with_capacity(rows.len());
    let mut index = 0;
    while index < rows.len() {
        let removed = run(&rows, index, Kind::Removed);
        let added = run(&rows, index + removed, Kind::Added);
        // One block replaced by another as long: line i became line i, so
        // the words between them can be compared.
        if removed > 0 && removed == added {
            let (old, new) = rows[index..index + removed * 2].split_at(removed);
            let pairs: Vec<(Line, Line)> = old
                .iter()
                .zip(new)
                .map(|(o, n)| changed_pair(o, n))
                .collect();
            lines.extend(pairs.iter().map(|(o, _)| o.clone()));
            lines.extend(pairs.into_iter().map(|(_, n)| n));
            index += removed * 2;
        } else {
            lines.push(plain_row(&rows[index]));
            index += 1;
        }
    }
    Some((lines, stats))
}

/// How many rows of `kind` start at `from`.
fn run(rows: &[Row<'_>], from: usize, kind: Kind) -> usize {
    rows.get(from..).map_or(0, |rest| {
        rest.iter().take_while(|row| row.kind == kind).count()
    })
}

fn classify(line: &str) -> Row<'_> {
    let kind = if line.starts_with("+++") || line.starts_with("---") || line.starts_with("***") {
        Kind::Header
    } else if line.starts_with("@@") {
        Kind::Hunk
    } else if line.starts_with('+') {
        Kind::Added
    } else if line.starts_with('-') {
        Kind::Removed
    } else {
        Kind::Context
    };
    let (gutter, text) = match kind {
        Kind::Added | Kind::Removed => split_gutter(&line[1..]),
        _ => ("", line),
    };
    Row { kind, gutter, text }
}

/// `  12 │ text` as its gutter and its text; a line with no number is all text.
fn split_gutter(rest: &str) -> (&str, &str) {
    const BAR: &str = " │ ";
    match rest.split_once(BAR) {
        Some((number, text))
            if !number.trim().is_empty() && number.trim().bytes().all(|b| b.is_ascii_digit()) =>
        {
            (&rest[..number.len() + BAR.len()], text)
        }
        _ => ("", rest),
    }
}

fn sign(kind: Kind) -> (&'static str, Role) {
    match kind {
        Kind::Added => ("+", Role::Ok),
        Kind::Removed => ("-", Role::Err),
        _ => (" ", Role::Dim),
    }
}

fn plain_row(row: &Row<'_>) -> Line {
    match row.kind {
        Kind::Header => Line::of(expand_tabs(row.text), Style::new(Role::Accent).bold()),
        Kind::Hunk => Line::of(expand_tabs(row.text), Role::Accent),
        Kind::Context => Line::of(expand_tabs(row.text), Role::Dim),
        Kind::Added | Kind::Removed => {
            let (mark, role) = sign(row.kind);
            lead(row, mark, role).push(expand_tabs(row.text), role)
        }
    }
}

/// The sign and the gutter, before a changed line's text.
fn lead(row: &Row<'_>, mark: &str, role: Role) -> Line {
    Line::of(mark, role).push(row.gutter, Role::Dim)
}

/// A removed line and the added line that replaced it, with the words each
/// lost or gained reversed.
fn changed_pair(old: &Row<'_>, new: &Row<'_>) -> (Line, Line) {
    let (old_text, new_text) = (expand_tabs(old.text), expand_tabs(new.text));
    let mut before = lead(old, "-", Role::Err);
    let mut after = lead(new, "+", Role::Ok);
    let diff = TextDiff::from_words(old_text.as_str(), new_text.as_str());
    for change in diff.iter_all_changes() {
        let word = change.value();
        match change.tag() {
            ChangeTag::Equal => {
                before = before.push(word, Role::Err);
                after = after.push(word, Role::Ok);
            }
            ChangeTag::Delete => before = before.push(word, Style::new(Role::Err).reverse()),
            ChangeTag::Insert => after = after.push(word, Style::new(Role::Ok).reverse()),
        }
    }
    (before, after)
}

/// A span drops tabs as control characters; four columns keeps the indent.
fn expand_tabs(text: &str) -> String {
    text.replace('\t', "    ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::text).collect()
    }

    /// The numbered shape `fs.edit` writes keeps its gutter, and a replaced
    /// line marks only the word that changed.
    #[test]
    fn a_numbered_edit_marks_the_changed_word() {
        let (lines, stats) =
            diff_lines("-  12 │ let x = 1;\n+  12 │ let x = 2;\nupdated a.rs (10 bytes)").unwrap();
        assert_eq!(
            stats,
            DiffStats {
                added: 1,
                removed: 1
            }
        );
        assert_eq!(
            texts(&lines),
            [
                "-  12 │ let x = 1;",
                "+  12 │ let x = 2;",
                "updated a.rs (10 bytes)"
            ]
        );
        let reversed: Vec<&str> = lines[1]
            .spans
            .iter()
            .filter(|span| span.style.reverse)
            .map(|span| span.text())
            .collect();
        assert_eq!(reversed, ["2;"]);
        assert_eq!(lines[0].spans[1].style.role, Role::Dim, "the gutter is dim");
    }

    /// A unified patch: headers and hunks are told apart, and blocks of
    /// different lengths are drawn whole rather than compared word by word.
    #[test]
    fn a_unified_patch_is_read_line_by_line() {
        let patch = "*** Update File: a.rs\n@@ fn main\n ctx\n-gone\n+one\n+two\n\tdone";
        let (lines, stats) = diff_lines(patch).unwrap();
        assert_eq!(
            stats,
            DiffStats {
                added: 2,
                removed: 1
            }
        );
        assert_eq!(lines[0].spans[0].style.role, Role::Accent);
        assert!(lines
            .iter()
            .all(|line| line.spans.iter().all(|span| !span.style.reverse)));
        assert_eq!(lines.last().unwrap().text(), "    done");
    }

    #[test]
    fn text_with_no_change_is_not_a_diff() {
        assert!(diff_lines("updated a.rs (3 bytes)").is_none());
        assert!(diff_lines("").is_none());
    }

    #[test]
    fn stats_read_as_counts() {
        assert_eq!(
            DiffStats {
                added: 3,
                removed: 2
            }
            .line()
            .text(),
            "+3 −2"
        );
    }
}
