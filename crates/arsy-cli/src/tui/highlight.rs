//! Colour for the fenced code in an assistant's answer.
//!
//! ponytail: lexical, not a grammar — keywords, strings, numbers, comments
//! for a handful of languages. A string spanning lines or a keyword used as a
//! field is coloured wrongly; a tree-sitter grammar per language fixes that if
//! it ever matters more than a dependency does.

use arsy_tui::{Line, Role, Style};

/// What one language looks like to the scanner.
struct Syntax {
    keywords: &'static [&'static str],
    line_comment: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    quotes: &'static [char],
}

const C_BLOCK: Option<(&str, &str)> = Some(("/*", "*/"));

fn syntax(lang: &str) -> Option<Syntax> {
    let lang = lang.to_ascii_lowercase();
    Some(match lang.as_str() {
        "rust" | "rs" => Syntax {
            keywords: &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
                "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
                "trait", "true", "type", "unsafe", "use", "where", "while", "Some", "None", "Ok",
                "Err",
            ],
            line_comment: &["//"],
            block_comment: C_BLOCK,
            // Not `'`: it opens a lifetime far more often than a char.
            quotes: &['"'],
        },
        "js" | "javascript" | "jsx" | "ts" | "typescript" | "tsx" | "mjs" | "cjs" => Syntax {
            keywords: &[
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "default",
                "else",
                "export",
                "extends",
                "false",
                "finally",
                "for",
                "from",
                "function",
                "if",
                "import",
                "in",
                "instanceof",
                "interface",
                "let",
                "new",
                "null",
                "of",
                "return",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "type",
                "typeof",
                "undefined",
                "var",
                "while",
                "yield",
            ],
            line_comment: &["//"],
            block_comment: C_BLOCK,
            quotes: &['"', '\'', '`'],
        },
        "py" | "python" => Syntax {
            keywords: &[
                "and", "as", "async", "await", "break", "class", "continue", "def", "elif", "else",
                "except", "False", "finally", "for", "from", "if", "import", "in", "is", "lambda",
                "None", "not", "or", "pass", "raise", "return", "self", "True", "try", "while",
                "with", "yield",
            ],
            line_comment: &["#"],
            block_comment: None,
            quotes: &['"', '\''],
        },
        "go" | "golang" => Syntax {
            keywords: &[
                "break",
                "case",
                "chan",
                "const",
                "continue",
                "default",
                "defer",
                "else",
                "false",
                "for",
                "func",
                "go",
                "if",
                "import",
                "interface",
                "map",
                "nil",
                "package",
                "range",
                "return",
                "select",
                "struct",
                "switch",
                "true",
                "type",
                "var",
            ],
            line_comment: &["//"],
            block_comment: C_BLOCK,
            quotes: &['"', '`'],
        },
        "sh" | "bash" | "shell" | "zsh" | "console" => Syntax {
            keywords: &[
                "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function",
                "if", "in", "local", "return", "then", "while",
            ],
            line_comment: &["#"],
            block_comment: None,
            quotes: &['"', '\''],
        },
        "json" | "jsonc" => Syntax {
            keywords: &["true", "false", "null"],
            line_comment: &["//"],
            block_comment: C_BLOCK,
            quotes: &['"'],
        },
        "toml" | "ini" => Syntax {
            keywords: &["true", "false"],
            line_comment: &["#"],
            block_comment: None,
            quotes: &['"', '\''],
        },
        "yaml" | "yml" => Syntax {
            keywords: &["true", "false", "null", "yes", "no"],
            line_comment: &["#"],
            block_comment: None,
            quotes: &['"', '\''],
        },
        _ => return None,
    })
}

/// The `Highlighter` the markdown renderer takes: one row per line of `code`,
/// or `None` for a language it does not know, which keeps the plain block.
pub fn highlight(lang: &str, code: &str) -> Option<Vec<Line>> {
    let syntax = syntax(lang)?;
    let mut in_block = false;
    Some(
        code.split('\n')
            .map(|line| syntax.line(line, &mut in_block))
            .collect(),
    )
}

impl Syntax {
    fn line(&self, text: &str, in_block: &mut bool) -> Line {
        let comment = Style::new(Role::Dim);
        let mut line = Line::new();
        let mut rest = text;
        while !rest.is_empty() {
            if *in_block {
                let (_, end) = self.block_comment.expect("only set with a block comment");
                // forgeguard: allow FG-ALG-002 -- the search consumes what it scans; one pass per line
                let taken = rest.find(end).map_or(rest.len(), |at| {
                    *in_block = false;
                    at + end.len()
                });
                line = line.push(&rest[..taken], comment);
                rest = &rest[taken..];
                continue;
            }
            if self.line_comment.iter().any(|open| rest.starts_with(open)) {
                return line.push(rest, comment);
            }
            if let Some((open, _)) = self
                .block_comment
                .filter(|(open, _)| rest.starts_with(open))
            {
                *in_block = true;
                line = line.push(open, comment);
                rest = &rest[open.len()..];
                continue;
            }
            let first = rest.chars().next().expect("rest is not empty");
            let taken = if self.quotes.contains(&first) {
                let taken = string_len(rest, first);
                line = line.push(&rest[..taken], Style::new(Role::Ok));
                taken
            } else if first.is_ascii_digit() {
                let taken = word_len(rest);
                line = line.push(&rest[..taken], Style::new(Role::Run));
                taken
            } else if first.is_alphabetic() || first == '_' {
                let taken = word_len(rest);
                let word = &rest[..taken];
                let style = if self.keywords.contains(&word) {
                    Style::new(Role::Accent)
                } else {
                    Style::new(Role::Assistant)
                };
                line = line.push(word, style);
                taken
            } else {
                let taken = first.len_utf8();
                line = line.push(&rest[..taken], Style::new(Role::Assistant));
                taken
            };
            rest = &rest[taken..];
        }
        line
    }
}

/// Bytes in the identifier or number `text` starts with.
fn word_len(text: &str) -> usize {
    text.find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(text.len())
}

/// Bytes in the string `text` starts with, up to its closing `quote` or the
/// end of the line, stepping over a backslash escape.
fn string_len(text: &str, quote: char) -> usize {
    let mut escaped = false;
    for (at, c) in text.char_indices().skip(1) {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            c if c == quote => return at + c.len_utf8(),
            _ => {}
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(line: &Line) -> Vec<(String, Role)> {
        line.spans
            .iter()
            .filter(|span| !span.text().trim().is_empty())
            .map(|span| (span.text().to_owned(), span.style.role))
            .collect()
    }

    #[test]
    fn keywords_strings_numbers_and_comments_get_their_roles() {
        let lines = highlight("rust", "let x = \"a\\\"b\"; // note\n42").unwrap();
        assert_eq!(lines.len(), 2);
        let first = roles(&lines[0]);
        assert_eq!(first[0], ("let".to_owned(), Role::Accent));
        assert!(first.contains(&("\"a\\\"b\"".to_owned(), Role::Ok)));
        assert_eq!(first.last().unwrap(), &("// note".to_owned(), Role::Dim));
        assert_eq!(roles(&lines[1]), [("42".to_owned(), Role::Run)]);
        assert_eq!(lines[0].text(), "let x = \"a\\\"b\"; // note");
    }

    #[test]
    fn a_block_comment_carries_across_lines() {
        let lines = highlight("ts", "a /* one\ntwo */ b").unwrap();
        assert_eq!(lines[1].spans[0].style.role, Role::Dim);
        assert_eq!(roles(&lines[1]).last().unwrap().1, Role::Assistant);
    }

    #[test]
    fn an_unknown_language_keeps_the_plain_block() {
        assert!(highlight("brainfuck", "+++").is_none());
        assert!(highlight("", "x").is_none());
    }
}
