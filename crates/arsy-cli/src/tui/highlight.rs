//! Colour for code: the fenced blocks in an answer and the files a read
//! returns.
//!
//! Parsed with syntect's bundled grammars, so a string spanning lines, a
//! type, a function name and a keyword used as a field are told apart the way
//! an editor tells them apart. The colours are not syntect's: each scope is
//! mapped onto a theme role, so `/theme` and `mono` apply to code as they do to
//! everything else.
//!
//! The bundle has no grammar for TOML, INI or a few shell spellings, so those
//! keep the lexical scanner below — keywords, strings, numbers, comments —
//! rather than losing colour.

use arsy_tui::{Line, Role, Style};
use std::sync::OnceLock;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// The bundled grammars, loaded on the first block that needs one.
fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The grammar a fence's language or a file's extension names. TypeScript is
/// read as JavaScript, which it extends; the bundle has no grammar of its own
/// for it.
fn grammar(lang: &str) -> Option<&'static SyntaxReference> {
    let lang = lang.trim().to_ascii_lowercase();
    let lang = match lang.as_str() {
        "ts" | "tsx" | "typescript" | "mts" | "cts" => "js",
        other => other,
    };
    if lang.is_empty() {
        return None;
    }
    syntaxes()
        .find_syntax_by_token(lang)
        .filter(|syntax| syntax.name != "Plain Text")
}

/// Which role a scope is drawn in, most specific first: the first prefix that
/// matches any scope on the stack, innermost scope first, decides.
fn roles() -> &'static [(Scope, Role)] {
    static ROLES: OnceLock<Vec<(Scope, Role)>> = OnceLock::new();
    ROLES.get_or_init(|| {
        [
            ("comment", Role::Dim),
            ("punctuation.definition.comment", Role::Dim),
            ("string", Role::Ok),
            ("constant.numeric", Role::Run),
            ("constant.language", Role::Run),
            ("constant.character", Role::Run),
            ("entity.name.function", Role::Model),
            ("support.function", Role::Model),
            ("variable.function", Role::Model),
            ("entity.name.type", Role::Cwd),
            ("entity.name.class", Role::Cwd),
            ("entity.name.struct", Role::Cwd),
            ("entity.name.enum", Role::Cwd),
            ("support.type", Role::Cwd),
            ("support.class", Role::Cwd),
            ("storage", Role::Accent),
            ("keyword", Role::Accent),
        ]
        .into_iter()
        .filter_map(|(scope, role)| Scope::new(scope).ok().map(|scope| (scope, role)))
        .collect()
    })
}

fn role_of(stack: &ScopeStack) -> Role {
    stack
        .as_slice()
        .iter()
        .rev()
        .find_map(|scope| role_for(*scope))
        .unwrap_or(Role::Assistant)
}

/// The role of the first prefix in `roles()` that matches `scope`.
fn role_for(scope: Scope) -> Option<Role> {
    roles()
        .iter()
        .find(|(prefix, _)| prefix.is_prefix_of(scope))
        .map(|(_, role)| *role)
}

/// The `Highlighter` the markdown renderer takes: one row per line of `code`,
/// or `None` for a language neither the grammars nor the scanner knows, which
/// keeps the plain block.
pub fn highlight(lang: &str, code: &str) -> Option<Vec<Line>> {
    match grammar(lang) {
        Some(syntax) => Some(parsed(syntax, code)),
        None => lexical(lang, code),
    }
}

fn parsed(syntax: &SyntaxReference, code: &str) -> Vec<Line> {
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    code.split('\n')
        .map(|text| {
            // The grammars are the newline-terminated set, so each line is
            // parsed with its newline and drawn without it.
            let ops = state
                .parse_line(&format!("{text}\n"), syntaxes())
                .unwrap_or_default();
            let mut line = Line::new();
            let mut at = 0;
            for (index, op) in ops {
                let index = index.min(text.len());
                if index > at {
                    line = line.push(&text[at..index], Style::new(role_of(&stack)));
                    at = index;
                }
                let _ = stack.apply(&op);
            }
            if at < text.len() {
                line = line.push(&text[at..], Style::new(role_of(&stack)));
            }
            line
        })
        .collect()
}

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

/// The lexical scanner, for a language the grammars do not cover.
fn lexical(lang: &str, code: &str) -> Option<Vec<Line>> {
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
        // The string is the string role; its escape may be marked apart.
        assert!(
            first
                .iter()
                .any(|(text, role)| text.starts_with('"') && *role == Role::Ok),
            "{first:?}"
        );
        let (comment, role) = first.last().unwrap();
        assert!(comment.ends_with("note") && *role == Role::Dim, "{first:?}");
        assert_eq!(roles(&lines[1]), [("42".to_owned(), Role::Run)]);
        assert_eq!(lines[0].text(), "let x = \"a\\\"b\"; // note");
    }

    #[test]
    fn a_block_comment_carries_across_lines() {
        let lines = highlight("ts", "a /* one\ntwo */ b").unwrap();
        assert_eq!(lines[1].spans[0].style.role, Role::Dim);
        assert_eq!(roles(&lines[1]).last().unwrap().1, Role::Assistant);
    }

    /// A grammar tells a type and a function name apart from the words
    /// around them, which the scanner could not.
    #[test]
    fn a_grammar_colours_types_and_function_names() {
        let lines = highlight(
            "rs",
            "fn parse(input: &str) {}\nstruct Config { port: u16 }",
        )
        .unwrap();
        let spans = roles(&lines[0]);
        assert!(
            spans.contains(&("parse".to_owned(), Role::Model)),
            "{spans:?}"
        );
        let spans = roles(&lines[1]);
        assert!(
            spans.contains(&("Config".to_owned(), Role::Cwd)),
            "{spans:?}"
        );
        assert_eq!(lines[1].text(), "struct Config { port: u16 }");
    }

    /// TOML has no bundled grammar and still gets the scanner's colour.
    #[test]
    fn a_language_without_a_grammar_falls_back_to_the_scanner() {
        let lines = highlight("toml", "name = \"arsy\" # note").unwrap();
        assert!(roles(&lines[0]).contains(&("\"arsy\"".to_owned(), Role::Ok)));
    }

    #[test]
    fn an_unknown_language_keeps_the_plain_block() {
        assert!(highlight("brainfuck", "+++").is_none());
        assert!(highlight("", "x").is_none());
    }
}
