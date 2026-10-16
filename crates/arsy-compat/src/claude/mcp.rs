//! MCP servers Claude Code declares: its local and user scopes in
//! `~/.claude.json`, and the repository's `.mcp.json`.
//!
//! Claude expands `${VAR}` and `${VAR:-default}` in a declaration, so this
//! does too -- except into a URL or header a repository wrote, which would send
//! the operator's variables to a host the repository chose.

use crate::{
    mcp::{empty_seed, place, required, string_map, strings, Declared},
    read, Context, Scope,
};
use arsy_kernel::config::{CompatSeed, LaunchEnv, McpTransport};
use arsy_kernel::placeholder;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub(crate) fn seeds(context: &Context) -> Vec<CompatSeed> {
    let project_file = context.root.join(".mcp.json");
    let mut project = json_seed(
        context,
        "claude",
        &project_file,
        Scope::Workspace,
        &read::json(&project_file),
        |document| document.get("mcpServers"),
    );
    let disabled = disabled_project_servers(context);
    project
        .mcp_servers
        .iter_mut()
        .filter(|server| disabled.contains(&server.name))
        .for_each(|server| server.enabled = false);

    let Some(user_file) = &context.homes.claude_json else {
        return vec![project];
    };
    let user = read::json(user_file);
    let root = context.root.to_string_lossy().into_owned();
    vec![
        json_seed(
            context,
            "claude",
            user_file,
            Scope::User,
            &user,
            |document| document.get("projects")?.get(&root)?.get("mcpServers"),
        ),
        project,
        json_seed(
            context,
            "claude",
            user_file,
            Scope::User,
            &user,
            |document| document.get("mcpServers"),
        ),
    ]
}

/// `disabledMcpjsonServers` from the operator's Claude settings. It can only
/// switch a repository's server off, so it is honoured from any of the files.
fn disabled_project_servers(context: &Context) -> BTreeSet<String> {
    let files = [
        context
            .homes
            .claude_dir
            .as_ref()
            .map(|dir| dir.join("settings.json")),
        Some(context.root.join(".claude/settings.json")),
        Some(context.root.join(".claude/settings.local.json")),
    ];
    files
        .into_iter()
        .flatten()
        .filter_map(|path| read::json(&path).ok().flatten())
        .flat_map(|settings| strings(&settings, "disabledMcpjsonServers"))
        .collect()
}

fn json_seed(
    context: &Context,
    label: &str,
    path: &Path,
    scope: Scope,
    document: &Result<Option<Value>, String>,
    select: impl Fn(&Value) -> Option<&Value>,
) -> CompatSeed {
    let mut seed = empty_seed(label, path);
    let servers = match document {
        Err(error) => {
            seed.notes.push(format!("{} {error}", path.display()));
            return seed;
        }
        Ok(document) => document
            .as_ref()
            .and_then(&select)
            .and_then(Value::as_object),
    };
    for (name, value) in servers.into_iter().flatten() {
        place(
            &mut seed,
            context,
            scope,
            name,
            server(context, scope, value),
        );
    }
    seed
}

fn server(context: &Context, scope: Scope, value: &Value) -> Result<Declared, String> {
    let kind = match value.get("type").and_then(Value::as_str) {
        Some(kind) => kind,
        None if value.get("url").is_some() => "http",
        None => "stdio",
    };
    let mut problems = Vec::new();
    let transport = match kind {
        "stdio" => stdio(context, value, &mut problems)?,
        "http" | "streamable-http" => http(context, scope, value, &mut problems)?,
        "sse" => return Err("uses the SSE transport, which ARSY does not connect over".to_owned()),
        other => return Err(format!("declares the unknown transport `{other}`")),
    };
    Ok(Declared {
        transport,
        enabled: value
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        timeout_ms: None,
        problems,
    })
}

fn stdio(
    context: &Context,
    value: &Value,
    problems: &mut Vec<String>,
) -> Result<McpTransport, String> {
    let command = required(value, "command")?;
    let mut expand = |text: &str| expanded(context, text, problems);
    Ok(McpTransport::Stdio {
        command: expand(&command),
        args: strings(value, "args")
            .iter()
            .map(|arg| expand(arg))
            .collect(),
        env: string_map(value, "env")
            .into_iter()
            .map(|(key, text)| (key, expand(&text)))
            .collect::<BTreeMap<_, _>>()
            .into(),
    })
}

fn http(
    context: &Context,
    scope: Scope,
    value: &Value,
    problems: &mut Vec<String>,
) -> Result<McpTransport, String> {
    let url = required(value, "url")?;
    let headers = string_map(value, "headers");
    let reads_variables = placeholder::has_placeholder(&url)
        || headers
            .values()
            .any(|text| placeholder::has_placeholder(text));
    if scope == Scope::Workspace && reads_variables {
        problems.push(
            "a repository file may not put the operator's variables into a URL or header"
                .to_owned(),
        );
        return Ok(McpTransport::Http {
            url,
            headers: LaunchEnv::default(),
        });
    }
    let mut expand = |text: &str| expanded(context, text, problems);
    Ok(McpTransport::Http {
        url: expand(&url),
        headers: headers
            .into_iter()
            .map(|(name, text)| (name, expand(&text)))
            .collect::<BTreeMap<_, _>>()
            .into(),
    })
}

fn expanded(context: &Context, text: &str, problems: &mut Vec<String>) -> String {
    placeholder::expand(text, context.env).unwrap_or_else(|name| {
        problems.push(format!("`{name}` is not set"));
        text.to_owned()
    })
}
