//! `arsy mcp`: manage MCP connection definitions, and probe one.
//!
//! Definitions live in `arsy.json` beside everything else ARSY resolves, so
//! `--scope user|workspace` is the configuration layer that owns the definition
//! and the layer decides the connection's trust label. Writing one is not
//! connecting: only `test` contacts a server, and it never invokes a tool.

use crate::{load_config, usage, Command, Diagnostic, Emitter, Invocation, Output};
use arsy_code::mcp::{Connection, McpError, RealChannels};
use arsy_kernel::config::{
    self, Layer, McpServer, McpTransport, DEFAULT_MCP_MAX_BODY_BYTES, DEFAULT_MCP_TIMEOUT_MS,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Which configuration layer a definition is written to. Enterprise files are
/// not writable from here: capping a fleet is an administrator's act, performed
/// with the administrator's own tools.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
    User,
    Workspace,
}

impl Scope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Workspace => "workspace",
        }
    }

    fn parse(value: Option<&str>) -> Result<Self, Diagnostic> {
        match value {
            // `user` is the default because a connection an operator adds is
            // theirs, not something a cloned repository inherits.
            None | Some("user") => Ok(Self::User),
            Some("workspace") => Ok(Self::Workspace),
            Some(other) => Err(usage(format!(
                "--scope must be `user` or `workspace`, not `{other}`"
            ))),
        }
    }

    fn path(self, root: &Path) -> Result<PathBuf, Diagnostic> {
        match self {
            Self::Workspace => Ok(root.join(".arsy").join(config::CONFIG_FILE)),
            Self::User => config::user_config().ok_or_else(|| {
                Diagnostic::error(
                    crate::ARSY_CFG_1000,
                    "this platform has no user configuration directory",
                    "use `--scope workspace`, or set the platform's configuration home",
                )
            }),
        }
    }

    const fn layer(self) -> Layer {
        match self {
            Self::User => Layer::User,
            Self::Workspace => Layer::Workspace,
        }
    }
}

const HELP: &str = "mcp requires `list`, `show <NAME>`, `add <NAME> --transport <stdio|http> \
                    (--command <CMD> [ARGS...] | --url <URL>)`, `import`, `remove <NAME>`, \
                    `enable <NAME>`, `disable <NAME>`, or `test <NAME>`";

pub fn parse(arguments: &crate::ParsedArguments) -> Result<Command, Diagnostic> {
    let mut positional = arguments.positional.clone();
    let Some(action) = positional.first().cloned() else {
        return Err(usage(HELP));
    };
    positional.remove(0);

    // `list` and `show` also read the declarations imported from other
    // ecosystems, so they stay on the inspection path.
    if matches!(action.as_str(), "list" | "show") {
        return crate::integrations::parse(
            "mcp",
            std::iter::once(action).chain(positional).collect(),
            arguments.source.clone(),
            arguments.event.clone(),
        );
    }

    let scope = Scope::parse(arguments.scope.as_deref())?;
    if action == "import" {
        if !positional.is_empty() {
            return Err(usage("mcp import takes no connection name"));
        }
        return Ok(Command::McpImport { scope });
    }
    let name = if positional.is_empty() {
        return Err(usage(format!("mcp {action} requires a connection name")));
    } else {
        positional.remove(0)
    };
    if !crate::config_edit::is_writable(&name) {
        return Err(usage(format!(
            "`{name}` cannot be used as a connection name"
        )));
    }
    named_action(&action, name, scope, arguments, positional)
}

/// The subcommands that act on one connection by name.
///
/// Split from `parse` so that reading which actions take a name, and what each
/// does with the rest of the line, is not also reading how a line is taken
/// apart.
fn named_action(
    action: &str,
    name: String,
    scope: Scope,
    arguments: &crate::ParsedArguments,
    positional: Vec<String>,
) -> Result<Command, Diagnostic> {
    match action {
        "add" => Ok(Command::McpAdd {
            server: definition(&name, arguments, positional)?,
            scope,
        }),
        "remove" | "enable" | "disable" => {
            if !positional.is_empty() {
                return Err(usage(format!("mcp {action} takes one connection name")));
            }
            Ok(match action {
                "remove" => Command::McpRemove { name, scope },
                other => Command::McpEnable {
                    name,
                    enabled: other == "enable",
                    scope,
                },
            })
        }
        "test" => {
            if !positional.is_empty() {
                return Err(usage("mcp test takes one connection name"));
            }
            Ok(Command::McpTest {
                name,
                timeout_ms: match arguments.timeout {
                    None => None,
                    Some(seconds) => Some(
                        seconds
                            .checked_mul(1_000)
                            .ok_or_else(|| usage("--timeout is too large"))?,
                    ),
                },
            })
        }
        other => Err(usage(format!("unknown mcp subcommand `{other}`\n{HELP}"))),
    }
}

/// The definition `mcp add` writes. Arguments after the name belong to the
/// command, which is why they are positional rather than a repeated flag.
fn definition(
    name: &str,
    arguments: &crate::ParsedArguments,
    args: Vec<String>,
) -> Result<McpServer, Diagnostic> {
    let transport = match (
        arguments.transport.as_deref(),
        arguments.command.as_deref(),
        arguments.url.as_deref(),
    ) {
        (Some("stdio") | None, Some(command), None) => McpTransport::Stdio {
            command: command.to_owned(),
            args,
            env: Default::default(),
        },
        (Some("http"), None, Some(url)) => {
            if !args.is_empty() {
                return Err(usage("an http connection takes no command arguments"));
            }
            McpTransport::Http {
                url: url.to_owned(),
                headers: Default::default(),
            }
        }
        (Some("stdio"), None, _) => Err(usage("a stdio connection needs --command <CMD>"))?,
        (Some("http"), _, None) => Err(usage("an http connection needs --url <URL>"))?,
        (Some(other), _, _) if !matches!(other, "stdio" | "http") => Err(usage(format!(
            "--transport must be `stdio` or `http`, not `{other}`"
        )))?,
        // Both or neither: the transport would be a guess, and a guess here
        // decides whether a program runs locally or a body leaves the machine.
        _ => Err(usage(
            "give exactly one of --command <CMD> (stdio) or --url <URL> (http)",
        ))?,
    };
    // Only emptiness is refused. These values are escaped on the way into the
    // table, so a backslash or a quote no longer threatens the file -- and
    // refusing them would rule out every Windows path. The server's name is a
    // different matter: it becomes a bare table header, so it stays restricted.
    for value in [
        Some(transport.target()),
        arguments.command.clone(),
        arguments.url.clone(),
    ]
    .into_iter()
    .flatten()
    {
        if value.trim().is_empty() {
            return Err(usage("a connection needs a non-empty command or URL"));
        }
    }
    Ok(McpServer {
        name: name.to_owned(),
        transport,
        enabled: true,
        // Replaced by the layer's own authority when the file is read back.
        trust: arsy_kernel::capability::PolicySource::User,
        timeout_ms: arguments
            .timeout
            .and_then(|seconds| seconds.checked_mul(1_000))
            .unwrap_or(DEFAULT_MCP_TIMEOUT_MS),
        max_body_bytes: DEFAULT_MCP_MAX_BODY_BYTES,
    })
}

/// Where one connection lives in the configuration document.
fn path_of(name: &str) -> [&str; 3] {
    ["mcp", "server", name]
}

/// A configuration file that is not JSON stops the edit rather than being
/// replaced by one holding only this connection.
fn config_broken(error: String) -> Diagnostic {
    Diagnostic::error(
        crate::ARSY_CFG_1000,
        error,
        "fix the configuration file, then run the command again",
    )
}

fn definition_json(server: &McpServer) -> Value {
    let mut object = serde_json::Map::new();
    match &server.transport {
        McpTransport::Stdio { command, args, .. } => {
            object.insert("transport".to_owned(), json!("stdio"));
            object.insert("command".to_owned(), json!(command));
            if !args.is_empty() {
                object.insert("args".to_owned(), json!(args));
            }
        }
        McpTransport::Http { url, .. } => {
            object.insert("transport".to_owned(), json!("http"));
            object.insert("url".to_owned(), json!(url));
        }
    }
    if server.timeout_ms != DEFAULT_MCP_TIMEOUT_MS {
        object.insert("timeout_ms".to_owned(), json!(server.timeout_ms));
    }
    Value::Object(object)
}

pub fn add(
    invocation: &Invocation,
    server: &McpServer,
    scope: Scope,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    emitter.result(add_in(&root, server, scope)?);
    Ok(0)
}

/// Write one definition and describe what was written, for a caller that
/// reports it its own way.
pub(crate) fn add_in(root: &Path, server: &McpServer, scope: Scope) -> Result<Value, Diagnostic> {
    let path = scope.path(root)?;
    let current = read(&path)?;
    if crate::config_edit::contains(&current, &path_of(&server.name)).map_err(config_broken)? {
        return Err(usage(format!(
            "`{}` is already defined in {}; remove it first",
            server.name,
            path.display()
        )));
    }
    let updated = crate::config_edit::set(
        &current,
        &["mcp", "server"],
        &server.name,
        definition_json(server),
    )
    .map_err(config_broken)?;
    write(&path, &updated)?;
    Ok(json!({
        "connection": server.name,
        "scope": scope.as_str(),
        "path": path.display().to_string(),
        "transport": server.transport.kind(),
        "target": server.transport.target(),
        "trust": arsy_kernel::config::policy_source(scope.layer()).to_string(),
        "connected": false,
    }))
}

/// `arsy mcp import`: adopt the operator's Claude connections as ARSY ones.
///
/// Reading a declaration is not connecting to it, so a declared connection is
/// inert until it is written into `arsy.json` — this is the step that adopts
/// one. It is explicit rather than automatic because adopting a definition is
/// what decides that a program may be started or a body may leave the machine,
/// and that decision belongs to the operator, not to whatever a file happened
/// to say.
///
/// A name that is already defined is left exactly as it is: an import must
/// never quietly repoint a connection the operator configured themselves.
pub fn import(
    invocation: &Invocation,
    scope: Scope,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let path = scope.path(&root)?;
    let mut current = read(&path)?;
    let mut imported = Vec::new();
    let mut skipped = Vec::new();
    for declaration in declared_for_import(&root)? {
        let server = server_from_declaration(&declaration)?;
        if crate::config_edit::contains(&current, &path_of(&server.name)).map_err(config_broken)?
            || imported.contains(&server.name)
        {
            skipped.push(server.name);
            continue;
        }
        current = crate::config_edit::set(
            &current,
            &["mcp", "server"],
            &server.name,
            definition_json(&server),
        )
        .map_err(config_broken)?;
        imported.push(server.name);
    }
    if !imported.is_empty() {
        write(&path, &current)?;
    }
    emitter.result(json!({
        "imported": imported,
        "already_defined": skipped,
        "scope": scope.as_str(),
        "path": path.display().to_string(),
        "connected": false,
    }));
    Ok(0)
}

/// Every Claude MCP declaration this workspace and this operator carry.
fn declared_for_import(root: &Path) -> Result<Vec<Value>, Diagnostic> {
    let importer = arsy_code::compat::CompatibilityImporter::new(root);
    let working = std::env::current_dir().unwrap_or_else(|_| root.to_path_buf());
    let working = if working.starts_with(root) {
        working
    } else {
        root.to_path_buf()
    };
    // The operator's own file comes first. A name is imported once and a
    // repeat is skipped, so whichever is read first wins — and a definition
    // the operator wrote must not be displaced by one a checkout carries.
    let mut declarations = Vec::new();
    if let Some(home) = arsy_kernel::config::home_config_file(".claude.json") {
        declarations.extend(
            arsy_code::compat::user_mcp_declarations(&home).map_err(|error| {
                Diagnostic::error(
                    "ARSY-CMP-1001",
                    format!("{} could not be read: {error}", home.display()),
                    "fix the file, or move the connection into arsy.json by hand",
                )
            })?,
        );
    }
    declarations.extend(
        importer
            .mcp_declarations(arsy_code::compat::Ecosystem::Claude, &working)
            .map_err(|error| {
                Diagnostic::error(
                    "ARSY-CMP-1001",
                    format!("claude import failed: {error}"),
                    "fix the source configuration; no connection was written",
                )
            })?,
    );
    Ok(declarations)
}

/// The ARSY definition a mapped declaration describes.
pub(crate) fn server_from_declaration(declaration: &Value) -> Result<McpServer, Diagnostic> {
    let text = |key: &str| declaration.get(key).and_then(Value::as_str).unwrap_or("");
    let name = text("name").to_owned();
    if !crate::config_edit::is_writable(&name) {
        return Err(usage(format!(
            "`{name}` cannot be used as a connection name; add it by hand"
        )));
    }
    let transport = match text("transport") {
        "stdio" => McpTransport::Stdio {
            command: text("command").to_owned(),
            args: declaration
                .get("args")
                .and_then(Value::as_array)
                .map(|args| {
                    args.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            env: Default::default(),
        },
        "http" => McpTransport::Http {
            url: text("url").to_owned(),
            headers: Default::default(),
        },
        other => {
            return Err(usage(format!(
                "`{name}` declares the `{other}` transport, which ARSY does not \
                 connect over; add it by hand if it can be reached another way"
            )))
        }
    };
    Ok(McpServer {
        name,
        transport,
        // Declared elsewhere and adopted here: the operator asked for it, and
        // the layer it lands in is what decides its authority.
        enabled: true,
        trust: arsy_kernel::config::policy_source(Layer::User),
        timeout_ms: DEFAULT_MCP_TIMEOUT_MS,
        max_body_bytes: DEFAULT_MCP_MAX_BODY_BYTES,
    })
}

pub fn remove(
    invocation: &Invocation,
    name: &str,
    scope: Scope,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let path = scope.path(&root)?;
    let current = read(&path)?;
    if !crate::config_edit::contains(&current, &path_of(name)).map_err(config_broken)? {
        return Err(usage(format!(
            "no connection named `{name}` is defined in {}",
            path.display()
        )));
    }
    write(
        &path,
        &crate::config_edit::remove(&current, &path_of(name)).map_err(config_broken)?,
    )?;
    emitter.result(json!({
        "connection": name,
        "scope": scope.as_str(),
        "path": path.display().to_string(),
        "removed": true,
    }));
    Ok(0)
}

/// `enable` and `disable`: flip one key, leaving the definition in place.
pub fn set_enabled(
    invocation: &Invocation,
    name: &str,
    enabled: bool,
    scope: Scope,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let config = load_config(&root, &working, invocation.config.as_deref())?;
    let declared = config.mcp_provenance(name).is_some();
    emitter.result(set_enabled_in(&root, name, enabled, scope, declared)?);
    Ok(0)
}

/// Flip the key and describe the result, for a caller that reports it its own
/// way.
///
/// `declared_elsewhere` says Claude Code or Codex declares the connection, so
/// this file may hold only the toggle for it.
pub(crate) fn set_enabled_in(
    root: &Path,
    name: &str,
    enabled: bool,
    scope: Scope,
    declared_elsewhere: bool,
) -> Result<Value, Diagnostic> {
    // Checked here rather than only where a command line is parsed: the /mcp
    // dialog passes names straight from another tool's file.
    if !crate::config_edit::is_writable(name) {
        return Err(usage(format!(
            "`{name}` cannot be written to arsy.json; switch it off in the file that declares it"
        )));
    }
    let path = scope.path(root)?;
    let current = read(&path)?;
    let updated = match crate::config_edit::set_existing(
        &current,
        &path_of(name),
        "enabled",
        Value::Bool(enabled),
    )
    .map_err(config_broken)?
    {
        Some(updated) => updated,
        // The bundled connection, and one Claude Code or Codex declares, exist
        // before this file is read, so there is nothing in it to amend yet.
        // Writing the toggle alone is what the loader expects: it amends the
        // declaration rather than restating a command the operator never
        // wrote. Any other name really is undefined.
        None if declared_elsewhere || name == arsy_kernel::config::BUNDLED_MCP_SERVER => {
            crate::config_edit::set(
                &current,
                &["mcp", "server"],
                name,
                json!({ "enabled": enabled }),
            )
            .map_err(config_broken)?
        }
        None => {
            return Err(usage(format!(
                "no connection named `{name}` is defined in {}",
                path.display()
            )))
        }
    };
    write(&path, &updated)?;
    Ok(json!({
        "connection": name,
        "scope": scope.as_str(),
        "path": path.display().to_string(),
        "enabled": enabled,
    }))
}

/// Connect every enabled server and collect the tools they offer.
///
/// Called once per turn, at the boundary, so a server added or enabled between
/// turns becomes callable at the next one and a definition edited mid-turn
/// cannot change what the model was already told.
///
/// A server that will not connect is reported as a warning and skipped. The
/// alternative — failing the turn — would make one broken definition in a user
/// config file stop all work in every workspace, which is a worse failure than
/// a turn that runs with fewer tools and says so.
pub fn connect_enabled(
    config: &arsy_kernel::config::Config,
    emitter: &mut Emitter,
) -> (
    Option<arsy_code::agent::mcpops::Connections>,
    Vec<arsy_code::agent::DynamicTool>,
) {
    let channels = RealChannels {
        http: || -> Box<dyn arsy_kernel::provider::wire::WireTransport> {
            Box::new(arsy_kernel::provider::http::HttpTransport::default())
        },
        // Nothing is painting the terminal on this route, so a server's own
        // logging belongs on stderr where a pipeline can capture it.
        log: arsy_code::mcp::stderr_log_sink(),
    };
    let mut connections = std::collections::BTreeMap::new();
    let mut tools = Vec::new();
    for server in config.mcp_servers().filter(|server| server.enabled) {
        // A first connection is bounded by what it discovers; a ceiling only
        // exists once one has been recorded, and nothing records one across
        // processes yet.
        match Connection::open(server, None, &channels) {
            Ok(connection) => {
                tools.extend(arsy_code::agent::mcpops::tools_of(&connection));
                connections.insert(server.name.clone(), connection);
            }
            Err(error) => emitter.diagnostic(&Diagnostic::warning(
                "ARSY-MCP-1000",
                format!("MCP server `{}` is unavailable: {error}", server.name),
                format!(
                    "check it with `arsy mcp test {}`, or disable it",
                    server.name
                ),
            )),
        }
    }
    if connections.is_empty() {
        return (None, Vec::new());
    }
    (
        Some(std::sync::Arc::new(std::sync::Mutex::new(connections))),
        tools,
    )
}

/// `arsy mcp test`: connect, negotiate, discover, disconnect.
///
/// No tool is invoked, and nothing is recorded against a session: this is a
/// probe an operator runs to find out whether a definition works.
pub fn test(
    invocation: &Invocation,
    name: &str,
    timeout_ms: Option<u64>,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let config = load_config(&root, &working, invocation.config.as_deref())?;
    let mut server = config.mcp_server(name).cloned().ok_or_else(|| {
        usage(format!(
            "no connection named `{name}` is configured; list them with `arsy mcp list`"
        ))
    })?;
    if let Some(timeout_ms) = timeout_ms {
        server.timeout_ms = timeout_ms;
    }

    let channels = RealChannels {
        http: || -> Box<dyn arsy_kernel::provider::wire::WireTransport> {
            Box::new(arsy_kernel::provider::http::HttpTransport::default())
        },
        // Nothing is painting the terminal on this route, so a server's own
        // logging belongs on stderr where a pipeline can capture it.
        log: arsy_code::mcp::stderr_log_sink(),
    };
    let started = std::time::Instant::now();
    // A probe holds no ceiling: it reports everything the server offers so an
    // operator can see what a real connection would be admitted with.
    let connection =
        Connection::open(&server, None, &channels).map_err(|error| failed(name, &error))?;
    let report = json!({
        "connection": name,
        "transport": server.transport.kind(),
        "target": server.transport.target(),
        "trust": server.trust.to_string(),
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "server": connection.server(),
        "discovery": connection.discovery(),
        "would_admit": connection.ceiling(),
        "tool_invoked": false,
    });
    connection.close().map_err(|error| failed(name, &error))?;
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        json!({"connection": human_test(&report)})
    });
    Ok(0)
}

fn human_test(report: &Value) -> String {
    let mut text = format!(
        "{} · {} · {}\n  reached {} {} speaking {} in {} ms\n",
        report["connection"].as_str().unwrap_or("?"),
        report["transport"].as_str().unwrap_or("?"),
        report["target"].as_str().unwrap_or("?"),
        report["server"]["name"].as_str().unwrap_or("?"),
        report["server"]["version"].as_str().unwrap_or("?"),
        report["server"]["protocol_version"].as_str().unwrap_or("?"),
        report["elapsed_ms"],
    );
    for (label, key) in [
        ("tools", "tools"),
        ("resources", "resources"),
        ("prompts", "prompts"),
    ] {
        let entries = report["discovery"][key]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if entries.is_empty() {
            continue;
        }
        text.push_str(&format!(
            "  {label}: {}\n",
            entries
                .iter()
                .filter_map(|entry| entry["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    text.push_str("  no tool was invoked; the connection was closed\n");
    text
}

fn failed(name: &str, error: &McpError) -> Diagnostic {
    Diagnostic::error(
        error.code(),
        format!("connection `{name}` failed: {error}"),
        if error.is_retryable() {
            "check the command or URL, then retry"
        } else {
            "this failure is not retried automatically; fix the credential or enable the connection"
        },
    )
}

fn read(path: &Path) -> Result<String, Diagnostic> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(body),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(Diagnostic::error(
            crate::ARSY_CFG_1000,
            format!("{} is unreadable: {error}", path.display()),
            "check the file's permissions",
        )),
    }
}

fn write(path: &Path, body: &str) -> Result<(), Diagnostic> {
    crate::replace_file(path, body.as_bytes()).map_err(|error| {
        Diagnostic::error(
            crate::ARSY_CFG_1000,
            format!("cannot write {}: {error}", path.display()),
            "check the file's permissions",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&str]) -> Result<Command, Diagnostic> {
        crate::parse(args.iter().map(|argument| (*argument).to_owned())).map(|it| it.command)
    }

    #[test]
    fn a_toggle_refuses_a_name_arsy_json_cannot_hold() {
        let root = tempfile::tempdir().unwrap();
        let refused =
            set_enabled_in(root.path(), "bad\"name", false, Scope::Workspace, true).unwrap_err();
        assert!(
            refused.message.contains("cannot be written"),
            "{}",
            refused.message
        );
        assert!(!root.path().join(".arsy").exists(), "nothing was written");
    }

    #[test]
    fn an_import_adopts_a_declaration_once_and_never_repoints_one() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"mcpServers": {"docs": {"command": "docs-server", "args": ["--serve"]},
                               "api": {"type": "http", "url": "https://api.test/mcp"}}}"#,
        )
        .unwrap();

        let declarations =
            arsy_code::compat::user_mcp_declarations(&home.path().join(".claude.json")).unwrap();
        assert_eq!(declarations.len(), 2, "{declarations:?}");

        let docs = declarations
            .iter()
            .find(|entry| entry["name"] == json!("docs"))
            .expect("the stdio declaration is mapped");
        let server = server_from_declaration(docs).unwrap();
        assert_eq!(server.name, "docs");
        assert!(server.enabled, "an adopted connection is usable");
        assert_eq!(
            server.transport,
            McpTransport::Stdio {
                command: "docs-server".to_owned(),
                args: vec!["--serve".to_owned()],
                env: Default::default(),
            }
        );
        // Adopted into a layer the operator owns, not left at the authority of
        // the file it was read from.
        assert_eq!(
            server.trust,
            arsy_kernel::config::policy_source(Layer::User)
        );

        // A name already defined is left alone rather than repointed.
        let existing = crate::config_edit::set(
            "{}",
            &["mcp", "server"],
            "docs",
            definition_json(&McpServer {
                name: "docs".to_owned(),
                transport: McpTransport::Stdio {
                    command: "mine".to_owned(),
                    args: Vec::new(),
                    env: Default::default(),
                },
                enabled: true,
                trust: arsy_kernel::config::policy_source(Layer::User),
                timeout_ms: DEFAULT_MCP_TIMEOUT_MS,
                max_body_bytes: DEFAULT_MCP_MAX_BODY_BYTES,
            }),
        )
        .unwrap();
        assert!(crate::config_edit::contains(&existing, &path_of("docs")).unwrap());
        assert!(existing.contains("mine"), "{existing}");
        let _ = workspace;
    }

    #[test]
    fn add_demands_exactly_one_transport_target() {
        let Ok(Command::McpAdd { server, scope }) = command(&[
            "mcp",
            "add",
            "docs",
            "--command",
            "mcp-docs",
            "--",
            "--root",
            ".",
        ]) else {
            panic!("a stdio definition parses");
        };
        assert_eq!(scope, Scope::User, "a definition defaults to the operator");
        assert_eq!(
            server.transport,
            McpTransport::Stdio {
                command: "mcp-docs".to_owned(),
                args: vec!["--root".to_owned(), ".".to_owned()],
                env: Default::default(),
            },
            "arguments after `--` belong to the command, flags of ARSY's do not"
        );
        assert!(server.enabled);
        assert_eq!(server.timeout_ms, DEFAULT_MCP_TIMEOUT_MS);

        let Ok(Command::McpAdd { server, scope }) = command(&[
            "mcp",
            "add",
            "remote",
            "--transport",
            "http",
            "--url",
            "https://mcp.example.test/mcp",
            "--scope",
            "workspace",
        ]) else {
            panic!("an http definition parses");
        };
        assert_eq!(scope, Scope::Workspace);
        assert_eq!(
            server.transport,
            McpTransport::Http {
                url: "https://mcp.example.test/mcp".to_owned(),
                headers: Default::default(),
            }
        );

        // Neither target, both targets, and a mismatched transport are all
        // refused rather than guessed at.
        for refused in [
            vec!["mcp", "add", "docs"],
            vec![
                "mcp",
                "add",
                "docs",
                "--command",
                "x",
                "--url",
                "https://y.test",
            ],
            vec![
                "mcp",
                "add",
                "docs",
                "--transport",
                "http",
                "--command",
                "x",
            ],
            vec![
                "mcp",
                "add",
                "docs",
                "--transport",
                "stdio",
                "--url",
                "https://y.test",
            ],
            vec![
                "mcp",
                "add",
                "docs",
                "--transport",
                "carrier-pigeon",
                "--command",
                "x",
            ],
            vec!["mcp", "add"],
            vec![
                "mcp",
                "add",
                "docs",
                "--command",
                "x",
                "--scope",
                "enterprise",
            ],
            // A name that cannot be written back as TOML would produce a file
            // that no longer loads.
            vec!["mcp", "add", "d\"s", "--command", "x"],
        ] {
            assert!(command(&refused).is_err(), "{refused:?}");
        }
    }

    /// A Windows path is a normal command. It was refused while these values
    /// went into the file unescaped, which left `arsy mcp add` unusable on
    /// Windows; the escaping is what makes accepting it safe.
    #[test]
    fn a_command_carrying_backslashes_and_quotes_is_accepted() {
        let added = command(&[
            "mcp",
            "add",
            "fixture",
            "--command",
            r"D:\a\arsy-code\target\debug\arsy.exe",
            "--",
            r#"--note="a" b"#,
        ])
        .expect("a Windows path is a command like any other");
        let Command::McpAdd { server, .. } = added else {
            panic!("expected an add");
        };
        assert_eq!(
            server.transport,
            McpTransport::Stdio {
                command: r"D:\a\arsy-code\target\debug\arsy.exe".to_owned(),
                args: vec![r#"--note="a" b"#.to_owned()],
                env: Default::default(),
            }
        );

        assert!(
            command(&["mcp", "add", "fixture", "--command", "  "]).is_err(),
            "an empty command is still refused"
        );
    }

    #[test]
    fn lifecycle_verbs_take_one_name_and_a_scope() {
        assert_eq!(
            command(&["mcp", "disable", "docs", "--scope", "workspace"]).unwrap(),
            Command::McpEnable {
                name: "docs".to_owned(),
                enabled: false,
                scope: Scope::Workspace,
            }
        );
        assert_eq!(
            command(&["mcp", "remove", "docs"]).unwrap(),
            Command::McpRemove {
                name: "docs".to_owned(),
                scope: Scope::User,
            }
        );
        assert_eq!(
            command(&["mcp", "test", "docs", "--timeout", "5"]).unwrap(),
            Command::McpTest {
                name: "docs".to_owned(),
                timeout_ms: Some(5_000),
            }
        );
        for refused in [
            vec!["mcp"],
            vec!["mcp", "enable"],
            vec!["mcp", "remove", "docs", "extra"],
            vec!["mcp", "conjure", "docs"],
        ] {
            assert!(command(&refused).is_err(), "{refused:?}");
        }
        // `list` and `show` stay on the inspection path, which also reads the
        // declarations imported from other ecosystems.
        assert!(matches!(
            command(&["mcp", "list"]).unwrap(),
            Command::Inspect { .. }
        ));
    }

    #[test]
    fn a_written_definition_round_trips_through_the_configuration_loader() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(arsy_kernel::config::CONFIG_FILE);
        let stdio = McpServer {
            name: "docs".to_owned(),
            transport: McpTransport::Stdio {
                command: "mcp-docs".to_owned(),
                args: vec!["--root".to_owned(), ".".to_owned()],
                env: Default::default(),
            },
            enabled: true,
            trust: arsy_kernel::capability::PolicySource::User,
            timeout_ms: 5_000,
            max_body_bytes: DEFAULT_MCP_MAX_BODY_BYTES,
        };
        let body =
            crate::config_edit::set("", &["mcp", "server"], &stdio.name, definition_json(&stdio))
                .unwrap();
        std::fs::write(&path, &body).unwrap();
        let config = arsy_kernel::config::Config::load(&[(Layer::User, path.clone())]).unwrap();
        let loaded = config
            .mcp_server("docs")
            .expect("the definition loads back");
        assert_eq!(loaded.transport, stdio.transport);
        assert_eq!(loaded.timeout_ms, 5_000);
        assert!(loaded.enabled);
        assert_eq!(loaded.trust, arsy_kernel::capability::PolicySource::User);

        // A Windows path, a quoted argument, and a tab survive the round trip:
        // the definition is serialized rather than pasted, so nothing in a
        // value can end the string it is written into.
        let awkward = McpServer {
            name: "awkward".to_owned(),
            transport: McpTransport::Stdio {
                command: r"D:\a\arsy-code\target\debug\arsy.exe".to_owned(),
                args: vec![r#"--note="a" b"#.to_owned(), "\ttabbed".to_owned()],
                env: Default::default(),
            },
            ..stdio.clone()
        };
        let awkward_path = directory.path().join("awkward.json");
        std::fs::write(
            &awkward_path,
            crate::config_edit::set(
                "",
                &["mcp", "server"],
                &awkward.name,
                definition_json(&awkward),
            )
            .unwrap(),
        )
        .unwrap();
        let config = arsy_kernel::config::Config::load(&[(Layer::User, awkward_path)]).unwrap();
        assert_eq!(
            config
                .mcp_server("awkward")
                .expect("an awkward definition loads back")
                .transport,
            awkward.transport
        );

        // Disabling flips one key and leaves the rest of the definition alone.
        let disabled = crate::config_edit::set_existing(
            &body,
            &path_of("docs"),
            "enabled",
            Value::Bool(false),
        )
        .unwrap()
        .unwrap();
        std::fs::write(&path, &disabled).unwrap();
        let config = arsy_kernel::config::Config::load(&[(Layer::User, path.clone())]).unwrap();
        assert!(!config.mcp_server("docs").unwrap().enabled);
        assert_eq!(
            config.mcp_server("docs").unwrap().transport,
            stdio.transport,
            "toggling enabled must not disturb the transport"
        );

        // A workspace layer may define a connection, but it is untrusted.
        let config =
            arsy_kernel::config::Config::load(&[(Layer::Workspace, path.clone())]).unwrap();
        assert_eq!(
            config.mcp_server("docs").unwrap().trust,
            arsy_kernel::capability::PolicySource::Workspace
        );

        // Removing takes the whole definition with it.
        let removed = crate::config_edit::remove(&disabled, &path_of("docs")).unwrap();
        std::fs::write(&path, &removed).unwrap();
        let config = arsy_kernel::config::Config::load(&[(Layer::User, path)]).unwrap();
        assert!(config.mcp_server("docs").is_none());
    }
}
