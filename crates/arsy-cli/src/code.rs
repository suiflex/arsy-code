//! `arsy code`: semantic navigation from the command line.
//!
//! The same operations a turn calls, reached the same way — through the tool
//! runtime, so an operator's lookup is policy-checked and recorded exactly as
//! the model's is. What it adds is a surface a person and a measurement can
//! both use: `--tier text` pins the answer to the textual tier, which is how
//! "the semantic answer is better" stops being a claim and becomes a number.

use crate::{usage, Command, Diagnostic, Emitter, Invocation, Output};
use serde_json::{json, Value};

pub fn parse(arguments: &crate::ParsedArguments) -> Result<Command, Diagnostic> {
    let mut positional = arguments.positional.clone();
    if positional.is_empty() {
        return Err(usage(HELP));
    }
    let action = positional.remove(0);
    let one = |positional: Vec<String>, what: &str| -> Result<String, Diagnostic> {
        crate::only_argument(positional, &format!("code {action}"), what)
    };
    let tier = match arguments.tier.as_deref() {
        None | Some("auto") => Tier::Auto,
        Some("text") => Tier::Text,
        Some(other) => {
            return Err(usage(format!(
                "--tier must be `auto` or `text`, not `{other}`"
            )))
        }
    };
    match action.as_str() {
        "symbol" => Ok(Command::CodeSymbol {
            name: one(positional, "<NAME>")?,
            tier,
            limit: arguments.limit,
        }),
        "explain" => Ok(Command::CodeInspect {
            operation: "code.explain",
            symbol: one(positional, "<SYMBOL_ID>")?,
        }),
        "references" => Ok(Command::CodeInspect {
            operation: "code.references",
            symbol: one(positional, "<SYMBOL_ID>")?,
        }),
        "diagnostics" => Ok(Command::CodeDiagnostics {
            path: one(positional, "<PATH>")?,
        }),
        other => Err(usage(format!("unknown code action `{other}`\n{HELP}"))),
    }
}

const HELP: &str = "\
code takes an action:
  arsy code symbol <NAME> [--tier auto|text] [--limit <N>]  where a name is declared
  arsy code explain <SYMBOL_ID>       what one declaration is
  arsy code references <SYMBOL_ID>    what a change to it could affect
  arsy code diagnostics <PATH>        what a language server says is wrong";

/// Which tier answers. `Auto` walks the tiers; `Text` pins the floor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tier {
    Auto,
    Text,
}

pub fn symbol(
    invocation: &Invocation,
    name: &str,
    tier: Tier,
    limit: Option<usize>,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let mut input = json!({"name": name});
    if tier == Tier::Text {
        input["tier"] = json!("text");
    }
    if let Some(limit) = limit {
        input["limit"] = json!(limit);
    }
    call(invocation, "code.symbol", &input, emitter)
}

pub fn inspect(
    invocation: &Invocation,
    operation: &str,
    symbol: &str,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    call(invocation, operation, &json!({"symbol": symbol}), emitter)
}

pub fn diagnostics(
    invocation: &Invocation,
    path: &str,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    call(
        invocation,
        "code.diagnostics",
        &json!({"path": path}),
        emitter,
    )
}

/// Run one semantic operation and print what it found.
///
/// The operation is dispatched directly rather than through the model-facing
/// tool, because `--tier` is an operator's affordance and a measurement's: the
/// tool translation deliberately does not carry it, so nothing a model sends
/// can pin the answer to a weaker tier.
///
/// Everything else is the turn's path exactly — the same registry, the same
/// policy evaluation, the same artifact — so an operator sees the bytes a turn
/// would have seen.
fn call(
    invocation: &Invocation,
    operation: &str,
    input: &Value,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let config = crate::load_config(&root, &working, invocation.config.as_deref())?;
    // A one-shot command, not a turn: its plan/validation state has no
    // session or task to share, so a fresh scope is the correct isolation,
    // not an approximation of one.
    let scope = arsy_kernel::domain::SessionId::new().to_string();
    let runtime = crate::agent_runtime(&root, &config, false, &scope, None, None, None, emitter)?;
    let workspace = arsy_code::resource::Workspace::open(&root)
        .map_err(|error| crate::storage_failed(error.to_string()))?;
    let registry = arsy_code::operations::registry(
        &workspace,
        std::sync::Arc::new(crate::artifact_store(&root)?),
        arsy_kernel::artifact::unix_time_ms(),
        arsy_code::operations::Reachable::from_config(&config),
        &scope,
        // A single inspection dispatches one read-only operation; it opens no
        // session, so it offers no durable checklist either.
        arsy_code::operations::TurnState::default(),
    )
    .map_err(|error| crate::storage_failed(error.to_string()))?;
    let kind = arsy_kernel::operation::OperationKind::new(operation)
        .map_err(|error| crate::storage_failed(error.to_string()))?;
    let contract = registry.contract(&kind).ok_or_else(|| {
        Diagnostic::error(
            "ARSY-EXE-1002",
            format!("`{operation}` is not available in this build"),
            "semantic navigation needs the workspace operations to be registered",
        )
    })?;
    let request = arsy_kernel::operation::OperationRequest {
        id: arsy_kernel::domain::OperationId::new(),
        kind,
        actor: crate::actor(),
        requirements: arsy_code::operations::requirements(contract, input, &root),
        input: input.clone(),
    };
    let grants = match runtime.authorize(&request).approve() {
        Ok(grants) => grants,
        Err(reason) => {
            return Err(Diagnostic::error(
                "ARSY-POL-1002",
                reason,
                "allow the read in policy, or run this where an operator can approve it",
            ))
        }
    };
    let result = runtime.dispatch(operation, &request, &grants, std::time::Instant::now());
    if !result.success {
        return Err(Diagnostic::error(
            "ARSY-EXE-1001",
            result.output,
            "check the symbol id came from `arsy code symbol`, and that policy allows reading",
        ));
    }

    emitter.result(if emitter.output == Output::Json {
        result.metadata
    } else {
        json!({"code": human(&result.metadata)})
    });
    Ok(0)
}

fn human(found: &Value) -> String {
    let provider = found["provider"].as_str().unwrap_or("none");
    if let Some(symbols) = found["symbols"].as_array() {
        if symbols.is_empty() {
            return "Nothing declares that name.".to_owned();
        }
        let mut text = format!("{} match(es) · {provider}\n", symbols.len());
        for symbol in symbols {
            text.push_str(&format!(
                "  {} · {}\n    {}\n",
                symbol["name"].as_str().unwrap_or("?"),
                symbol["location"]["uri"].as_str().unwrap_or("?"),
                symbol["id"].as_str().unwrap_or("?"),
            ));
        }
        return text;
    }
    if let Some(summary) = found["summary"].as_str() {
        return format!("{provider}\n{summary}");
    }
    if let Some(callers) = found["callers"].as_array() {
        let mut text = format!("{} file(s) could be affected · {provider}\n", callers.len());
        for caller in callers {
            text.push_str(&format!(
                "  {}\n",
                caller["location"]["uri"].as_str().unwrap_or("?")
            ));
        }
        return text;
    }
    if let Some(diagnostics) = found["diagnostics"].as_array() {
        if diagnostics.is_empty() {
            return "The language server reports nothing.".to_owned();
        }
        let mut text = format!("{} diagnostic(s) · {provider}\n", diagnostics.len());
        for diagnostic in diagnostics {
            text.push_str(&format!(
                "  severity {}: {}\n",
                diagnostic["severity"],
                diagnostic["message"].as_str().unwrap_or("")
            ));
        }
        return text;
    }
    serde_json::to_string_pretty(found).unwrap_or_else(|_| found.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&str]) -> Result<Command, Diagnostic> {
        crate::parse(args.iter().map(|argument| (*argument).to_owned())).map(|it| it.command)
    }

    #[test]
    fn the_tier_is_explicit_and_only_the_two_that_exist_are_accepted() {
        assert_eq!(
            command(&["code", "symbol", "TaskGraph"]).unwrap(),
            Command::CodeSymbol {
                name: "TaskGraph".to_owned(),
                tier: Tier::Auto,
                limit: None,
            }
        );
        assert_eq!(
            command(&["code", "symbol", "TaskGraph", "--tier", "text"]).unwrap(),
            Command::CodeSymbol {
                name: "TaskGraph".to_owned(),
                tier: Tier::Text,
                limit: None,
            }
        );
        // `lsp` is a tier the chain may reach, not one a caller can demand:
        // pinning it would fail wherever no server is configured, which is
        // most workspaces.
        assert!(command(&["code", "symbol", "X", "--tier", "lsp"]).is_err());
        assert!(command(&["code"]).is_err());
        assert!(command(&["code", "guess", "X"]).is_err());
        assert!(command(&["code", "symbol"]).is_err());
    }
}
