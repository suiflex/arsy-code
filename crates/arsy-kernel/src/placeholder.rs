//! `${VAR}` and `${VAR:-default}` in MCP declarations: Claude Code's syntax,
//! which ARSY's own `mcp.server.<name>.env` and `.headers` read the same way.

/// Expand every placeholder in `text`, or name the first variable that is unset
/// and has no default. Only the braced form is recognised, as in Claude Code;
/// a bare `$VAR` is left as written.
pub fn expand(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut pieces = text.split("${");
    let mut out = pieces.next().unwrap_or_default().to_owned();
    for piece in pieces {
        out.push_str(&piece_expanded(piece, lookup)?);
    }
    Ok(out)
}

/// What follows one `${`: a placeholder and the text after it, or, with no
/// closing brace, the text as written.
fn piece_expanded(piece: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    match piece.split_once('}') {
        Some((inner, tail)) => Ok(resolve(inner, lookup)? + tail),
        None => Ok(format!("${{{piece}")),
    }
}

/// Whether `text` would read a variable if it were expanded: a closed `${NAME}`
/// with a name, since `expand` leaves an unclosed `${` as written.
pub fn has_placeholder(text: &str) -> bool {
    text.split("${").skip(1).any(|piece| {
        piece
            .split_once('}')
            .is_some_and(|(inner, _)| !inner.is_empty() && !inner.starts_with(":-"))
    })
}

fn resolve(inner: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let (name, default) = match inner.split_once(":-") {
        Some((name, default)) => (name, Some(default)),
        None => (inner, None),
    };
    lookup(name)
        .filter(|value| !value.is_empty() || default.is_none())
        .or_else(|| default.map(str::to_owned))
        .ok_or_else(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(name: &str) -> Option<String> {
        match name {
            "TOKEN" => Some("abc".to_owned()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn placeholders_expand_fall_back_or_name_what_is_missing() {
        assert_eq!(expand("Bearer ${TOKEN}", &lookup).unwrap(), "Bearer abc");
        assert_eq!(expand("${MISSING:-local}/db", &lookup).unwrap(), "local/db");
        assert_eq!(expand("${EMPTY:-fallback}", &lookup).unwrap(), "fallback");
        assert_eq!(expand("${EMPTY}", &lookup).unwrap(), "");
        assert_eq!(
            expand("$TOKEN ${unclosed", &lookup).unwrap(),
            "$TOKEN ${unclosed"
        );
        assert_eq!(expand("a ${MISSING} b", &lookup), Err("MISSING".to_owned()));
        assert!(has_placeholder("${TOKEN}"));
        assert!(!has_placeholder("$TOKEN"));
        // An unclosed or nameless marker expands to itself, so reads nothing.
        assert!(!has_placeholder("hunter2 ${"));
        assert!(!has_placeholder("hunter2 ${TOKEN"));
        assert!(!has_placeholder("hunter2 ${}"));
        assert!(!has_placeholder("${:-hunter2}"));
        assert!(has_placeholder("${ ${TOKEN}"));
    }
}
