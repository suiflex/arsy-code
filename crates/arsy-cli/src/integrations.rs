//! Inspection only: imported content is never an execution grant.
use crate::{usage, Command, Diagnostic};
use arsy_code::compat::{CompatibilityImporter, Ecosystem};
use serde_json::{json, Value};
use std::path::Path;

pub fn parse(
    kind: &str,
    args: Vec<String>,
    source: Option<String>,
    event: Option<String>,
) -> Result<Command, Diagnostic> {
    if source
        .as_deref()
        .is_some_and(|source| !matches!(source, "arsy" | "claude" | "codex" | "omp"))
    {
        return Err(usage("--source requires arsy, claude, codex, or omp"));
    }
    if kind == "mcp" && event.is_some() {
        return Err(usage("--event applies only to hook list"));
    }
    if kind == "hook" && source.as_deref().is_some_and(|source| source != "claude") {
        return Err(usage(
            "hook inspection currently supports --source claude only",
        ));
    }
    let name = match args.as_slice() {
        [action] if action == "list" => None,
        [action, name] if kind == "mcp" && action == "show" => Some(name.clone()),
        _ => {
            return Err(usage(if kind == "mcp" {
                "mcp requires `list` or `show <NAME>`; --source claude|codex|omp. Connection management is not implemented."
            } else {
                "hook requires `list`; --event <NAME>, --source claude"
            }))
        }
    };
    Ok(Command::Inspect {
        kind: kind.into(),
        name,
        source,
        event,
    })
}

pub fn inspect(
    root: &Path,
    kind: &str,
    name: Option<&str>,
    source: Option<&str>,
    event: Option<&str>,
    extra_config: Option<&Path>,
) -> Result<Value, Diagnostic> {
    inspect_with(
        root,
        kind,
        name,
        source,
        event,
        extra_config,
        &user_declarations(kind),
    )
}

/// The same inspection, told what the operator's own home declares.
///
/// Passed in rather than read here, so what a listing contains does not depend
/// on whose machine it runs on — which is exactly what a test needs to say.
#[allow(clippy::too_many_arguments)]
pub fn inspect_with(
    root: &Path,
    kind: &str,
    name: Option<&str>,
    source: Option<&str>,
    event: Option<&str>,
    extra_config: Option<&Path>,
    user: &[Value],
) -> Result<Value, Diagnostic> {
    let cwd = std::env::current_dir().map_err(crate::storage_failed)?;
    let working = if cwd.starts_with(root) {
        cwd.as_path()
    } else {
        root
    };
    let importer = CompatibilityImporter::new(root);
    let mut entries = Vec::new();
    // What the engine actually built, so a listing can say which declarations
    // run rather than repeating that none do.
    // Only what reads it loads it: an OMP-only listing never did, and a
    // broken arsy.json should not start failing it.
    let needs_config = kind == "hook" || source.is_none_or(|source| source != "omp");
    let config = if needs_config {
        crate::load_config(root, working, extra_config)?
    } else {
        arsy_kernel::config::Config::default()
    };
    let loaded = (kind == "hook").then(|| crate::hook_engine(root, &config));
    if kind == "mcp" {
        entries.extend(configured(&config, name, source));
    }
    for ecosystem in [Ecosystem::Claude, Ecosystem::Codex, Ecosystem::Omp] {
        if source.is_some_and(|source| source != ecosystem.as_str())
            || read_live(kind, ecosystem, &config)
        {
            continue;
        }
        if kind == "hook" && ecosystem != Ecosystem::Claude {
            continue;
        }
        let declarations = if kind == "mcp" {
            importer
                .mcp_declarations(ecosystem, working)
                .map(|mut in_workspace| {
                    // Claude keeps user-scope connections in the operator's home
                    // rather than in the checkout, so a workspace with no
                    // `.mcp.json` still has connections the operator declared.
                    if ecosystem == Ecosystem::Claude {
                        in_workspace.extend(user.iter().cloned());
                    }
                    in_workspace
                })
        } else {
            importer.hook_declarations().map(|mut in_workspace| {
                // The engine loads the operator's own `~/.claude/settings.json`
                // too, so a listing of the workspace alone named a subset of
                // what runs — and the `/hooks` switches could not reach a home
                // hook to turn it off.
                in_workspace.extend(user.iter().cloned());
                in_workspace
            })
        }
        .map_err(|error| {
            Diagnostic::error(
                "ARSY-CMP-1001",
                format!("{} import failed: {error}", ecosystem.as_str()),
                "fix the source configuration; no connection or hook was executed",
            )
        })?;
        for mut entry in declarations {
            if name.is_some_and(|name| entry["name"].as_str() != Some(name)) {
                continue;
            }
            if event.is_some_and(|event| {
                entry["event"].as_str() != Some(event)
                    && entry["original_event"].as_str() != Some(event)
            }) {
                continue;
            }
            entry["ecosystem"] = json!(ecosystem.as_str());
            entry["runtime_status"] = json!(match &loaded {
                Some(loaded) => runtime_status(loaded, root, entry["source"].as_str()),
                None => "not_loaded",
            });
            if kind == "hook" {
                annotate_hook(&mut entry);
            }
            entries.push(entry);
        }
    }
    if name.is_some() && entries.is_empty() {
        return Err(usage(format!(
            "no MCP connection or declaration is named `{}`; list the available names with `arsy mcp list` (`/mcp` in the TUI)",
            name.unwrap_or_default()
        )));
    }
    if name.is_some() && entries.len() > 1 {
        return Err(usage(format!(
            "`{}` is declared by {} sources; select one with --source arsy|claude|codex|omp",
            name.unwrap_or_default(),
            entries.len()
        )));
    }
    let mut report = json!({
        "entries": entries,
        "status": "inspection_complete",
        "notice": match &loaded {
            // A hook listing is no longer only a reading of files: some of what
            // it names will run, and saying otherwise would be false.
            Some(loaded) if !loaded.is_empty() => "Hooks marked `loaded` run on this workspace's turns. A repository's own hooks run only where `[project.\"<path>\"] trust_level = \"trusted\"` vouches for it.",
            Some(_) => "No executable hook is loaded for this workspace. Provider-owned integrations are managed by the provider.",
            None => "Definitions and declarations only; ARSY has not connected. Provider-owned integrations are managed by the provider.",
        },
    });
    if let Some(loaded) = loaded {
        // Where the engine looked, including the operator's own files, which
        // an import of this workspace never sees.
        report["sources"] = json!(loaded.sources);
    }
    Ok(report)
}

/// Whether the file a declaration came from is one the engine loaded.
///
/// Matched on the whole path rather than its tail: the operator's own
/// `~/.claude/settings.json` and the repository's end in the same characters,
/// and taking the first of those to match reported one file's status against
/// the other's declarations.
fn runtime_status(
    loaded: &arsy_code::hook::Loaded,
    root: &Path,
    source: Option<&str>,
) -> &'static str {
    let Some(source) = source else {
        return "not_loaded";
    };
    let declared = root.join(source);
    let same = |candidate: &Path| {
        candidate == declared
            || std::fs::canonicalize(candidate).ok() == std::fs::canonicalize(&declared).ok()
    };
    loaded
        .sources
        .iter()
        .find(|candidate| same(&candidate.path))
        .map_or("not_loaded", |candidate| {
            if candidate.status == "loaded" {
                "loaded"
            } else {
                "not_loaded"
            }
        })
}

/// Whether a tool's MCP servers are already in the resolved configuration,
/// because ARSY reads that tool live. Its declarations are then listed from
/// there, once, with the state they really have.
fn read_live(kind: &str, ecosystem: Ecosystem, config: &arsy_kernel::config::Config) -> bool {
    kind == "mcp"
        && matches!(ecosystem, Ecosystem::Claude | Ecosystem::Codex)
        && config.compat_enabled(ecosystem.as_str())
}

/// Every connection the resolved configuration holds — ARSY's own and those
/// Claude Code and Codex declare — in the same row shape the imported
/// declarations use so one listing can show both.
///
/// Reading a definition is not connecting: `runtime_status` is `not_loaded`
/// for every row here, exactly as it is for an import. Launch env and headers
/// are listed by name only; their values may be credentials.
fn configured(
    config: &arsy_kernel::config::Config,
    name: Option<&str>,
    source: Option<&str>,
) -> Vec<Value> {
    let mut entries = Vec::new();
    for server in config.mcp_servers() {
        let provenance = config.mcp_provenance(&server.name);
        let ecosystem = provenance.map_or("arsy", |provenance| provenance.label.as_str());
        if name.is_some_and(|name| name != server.name)
            || source.is_some_and(|source| source != ecosystem)
        {
            continue;
        }
        let mut entry = json!({
            "name": server.name,
            "source": provenance.map_or_else(
                || format!("{} ({})", arsy_kernel::config::CONFIG_FILE, server.trust),
                |provenance| provenance.path.display().to_string(),
            ),
            "ecosystem": ecosystem,
            "transport": server.transport.kind(),
            "trust": server.trust.to_string(),
            "level": if provenance.is_some() { "mapped" } else { "native" },
            "enabled": server.enabled,
            "runtime_status": "not_loaded",
            "timeout_ms": server.timeout_ms,
            "max_body_bytes": server.max_body_bytes,
        });
        match &server.transport {
            arsy_kernel::config::McpTransport::Stdio { command, args, env } => {
                entry["command"] = json!(command);
                entry["args"] = json!(args);
                entry["env_keys"] = json!(env.keys().collect::<Vec<_>>());
            }
            arsy_kernel::config::McpTransport::Http { url, headers } => {
                entry["url"] = json!(url);
                entry["header_keys"] = json!(headers.keys().collect::<Vec<_>>());
            }
        }
        entries.push(entry);
    }
    entries
}

/// Add what the lifecycle engine would make of a declared hook: whether the
/// event exists in this build, what it may do, and what happens if it fails.
///
/// A declaration says what its author intended; these three say what ARSY would
/// actually do with it, which is the difference `arsy hook list` has to show.
fn annotate_hook(entry: &mut Value) {
    use arsy_code::hook::{EffectClass, LifecycleEvent};

    let Some(event) = entry["event"].as_str().and_then(LifecycleEvent::parse) else {
        entry["effect_class"] = json!("none");
        entry["on_failure"] = json!("not_dispatched");
        return;
    };
    // An imported hook is repository content: it may observe and it may deny,
    // but it is registered as a gate only where the event is one that gates.
    entry["effect_class"] = serde_json::to_value(match event.failure_policy() {
        arsy_code::hook::FailurePolicy::FailClosed => EffectClass::Gate,
        arsy_code::hook::FailurePolicy::FailOpen => EffectClass::Observe,
    })
    .unwrap_or(Value::Null);
    entry["on_failure"] = serde_json::to_value(event.failure_policy()).unwrap_or(Value::Null);
}

/// Render the inspection for a person instead of echoing the machine record.
///
/// Each row keeps the fields that decide whether a declaration is the one its
/// author intended — the command or URL that a connection *would* run, the
/// trust label, and the mapping level. The full record stays available under
/// `--output json`, which is what a machine reads.
pub fn human_report(
    report: &Value,
    kind: &str,
    source: Option<&str>,
    event: Option<&str>,
) -> Value {
    let entries = report["entries"]
        .as_array()
        .expect("inspection produces an entries array");
    if entries.is_empty() {
        return json!({
            "declarations": nothing_found(kind, source, event),
            "notice": report["notice"],
        });
    }
    // One entry is a `show`, or a listing narrowed to a single server: detail
    // is the whole point of asking for it.
    let listing = if kind == "mcp" && entries.len() > 1 && !verbose() {
        card(entries)
    } else {
        long_listing(entries, kind)
    };
    json!({"declarations": listing, "notice": report["notice"]})
}

/// Whether the listing is asked to show every field of every entry.
///
/// Off by default: the fields that repeat identically on every row say
/// nothing, and a screenful of them buries the rows that differ. Asking for
/// one server brings them back, and so does `ARSY_MCP_VERBOSE=1`.
fn verbose() -> bool {
    std::env::var_os("ARSY_MCP_VERBOSE").is_some_and(|value| value != "0" && !value.is_empty())
}

/// The listing as one bordered card: a row per server, and the fields they all
/// share stated once underneath instead of once per row.
fn card(entries: &[Value]) -> String {
    const INDENT: &str = "  ";

    let rows: Vec<(String, String, String)> = entries
        .iter()
        .map(|entry| {
            let name = entry["name"].as_str().unwrap_or("?").to_owned();
            let transport = entry["transport"].as_str().unwrap_or("").to_owned();
            (name, transport, target(entry))
        })
        .collect();
    let name_width = rows.iter().map(|(name, ..)| name.chars().count()).max();
    let name_width = name_width.unwrap_or_default();
    let kind_width = rows
        .iter()
        .map(|(_, transport, _)| transport.chars().count())
        .max()
        .unwrap_or_default();

    let mut lines = vec![headline(entries)];
    lines.push(String::new());
    for (index, (name, transport, target)) in rows.iter().enumerate() {
        let loaded = entries[index]["runtime_status"] == "loaded";
        lines.push(format!(
            "{} {name:name_width$}  {transport:kind_width$}  {target}",
            if loaded { "●" } else { "○" },
        ));
    }
    if let Some(shared) = shared_fields(entries) {
        lines.push(String::new());
        lines.push(shared);
    }

    let width = lines
        .iter()
        .map(|line| visible_width(line))
        .max()
        .unwrap_or_default();
    let rule = "─".repeat(width + 2);
    // The emitter labels this value, so the card starts on the line after it
    // rather than beside the label, where its first row would sit indented
    // further than the rest.
    let mut card = format!("\n{INDENT}╭{rule}╮\n");
    for line in &lines {
        let pad = " ".repeat(width - visible_width(line));
        card.push_str(&format!("{INDENT}│ {line}{pad} │\n"));
    }
    card.push_str(&format!("{INDENT}╰{rule}╯\n"));
    card.push_str(&format!(
        "{INDENT}`/mcp show <NAME>` for one in full · `ARSY_MCP_VERBOSE=1` for every field\n"
    ));
    card
}

/// The one line a reader takes away before any row.
fn headline(entries: &[Value]) -> String {
    let loaded = entries
        .iter()
        .filter(|entry| entry["runtime_status"] == "loaded")
        .count();
    let unsupported = entries
        .iter()
        .filter(|entry| entry["level"] == "unsupported")
        .count();
    let mut headline = format!("MCP · {} declared", entries.len());
    if unsupported > 0 {
        headline.push_str(&format!(" · {unsupported} unsupported"));
    }
    headline.push_str(&match loaded {
        0 => " · none loaded".to_owned(),
        loaded if loaded == entries.len() => " · all loaded".to_owned(),
        loaded => format!(" · {loaded} loaded"),
    });
    headline
}

/// What a row says about where the server is, in as few characters as carry
/// the answer: the program that runs it, or the host it is reached at.
pub(crate) fn target(entry: &Value) -> String {
    if let Some(url) = entry["url"].as_str() {
        return mask_token(url);
    }
    let Some(command) = entry["command"].as_str() else {
        return String::new();
    };
    let program = command.rsplit('/').next().unwrap_or(command);
    // `npx -y the-server` names a package, not npx, so the first argument that
    // is not a flag is what a reader is looking for.
    let named = entry["args"]
        .as_array()
        .and_then(|args| {
            args.iter()
                .filter_map(Value::as_str)
                .find(|arg| !arg.starts_with('-'))
        })
        .filter(|_| matches!(program, "npx" | "uvx" | "bunx" | "pnpm" | "yarn"));
    match named {
        Some(package) => format!("{program} {package}"),
        None => program.to_owned(),
    }
}

/// The fields every entry agrees on, stated once. `None` when they disagree,
/// because then they belong on the rows and the verbose listing has them.
fn shared_fields(entries: &[Value]) -> Option<String> {
    let same = |key: &str| {
        let first = entries.first()?[key].as_str()?;
        entries
            .iter()
            .all(|entry| entry[key].as_str() == Some(first))
            .then(|| first.to_owned())
    };
    let source = same("source");
    let trust = same("trust");
    let level = same("level");
    let stated: Vec<String> = [source, trust.map(|t| format!("trust {t}")), level]
        .into_iter()
        .flatten()
        .collect();
    (!stated.is_empty()).then(|| format!("all: {}", stated.join(" · ")))
}

/// Printed columns, so the border lands in the same place on every row.
fn visible_width(line: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(line)
}

/// Every field of every entry, which is what `show` and a narrowed listing
/// want and what `ARSY_MCP_VERBOSE` asks for.
fn long_listing(entries: &[Value], kind: &str) -> String {
    let mut listing = summary(entries, kind);
    for entry in entries {
        let label = entry["name"]
            .as_str()
            .or_else(|| entry["original_event"].as_str())
            .unwrap_or("hook");
        let detail = entry["transport"]
            .as_str()
            .or_else(|| entry["matcher"].as_str())
            .unwrap_or("");
        let status = if entry["runtime_status"] == "loaded" {
            "loaded"
        } else {
            "not loaded"
        };
        listing.push_str(&format!("\n  {label} · {detail} · {status}\n"));
        for line in details(kind, entry) {
            listing.push_str(&format!("    {line}\n"));
        }
    }
    listing
}

/// The count line, so a long listing states up front how much of it is usable.
fn summary(entries: &[Value], kind: &str) -> String {
    let noun = if kind == "mcp" { "MCP server" } else { "hook" };
    let plural = if entries.len() == 1 { "" } else { "s" };
    let unsupported = entries
        .iter()
        .filter(|entry| entry["level"] == "unsupported")
        .count();
    // Counted rather than asserted. The rows below report each entry's real
    // `runtime_status`, so a summary that always said "none loaded" contradicted
    // the listing it introduces the moment anything was loaded — and the count
    // line is the part a reader takes away.
    let loaded = entries
        .iter()
        .filter(|entry| entry["runtime_status"] == "loaded")
        .count();
    let mut summary = format!("{} {noun}{plural} declared", entries.len());
    if unsupported > 0 {
        summary.push_str(&format!(", {unsupported} unsupported"));
    }
    summary.push_str(&match loaded {
        0 => "; none loaded\n".to_owned(),
        loaded if loaded == entries.len() => "; all loaded\n".to_owned(),
        loaded => format!("; {loaded} loaded\n"),
    });
    summary
}

/// Hide the password in any `scheme://user:password@host` the line carries.
///
/// A connection string is an argument like any other, so the listing printed
/// it whole: an operator who runs `/mcp` with someone watching, or scrolls
/// back through it later, has published the database's password.
///
/// Everything else stays. The host, the port and the database name are what
/// tell two servers apart, and none of them is the secret.
///
/// ponytail: only the userinfo form is covered, and only where the password
/// is encoded as a URL requires. A password carrying an unencoded `/` ends the
/// authority early and is left alone; a secret passed as its own flag —
/// `--token abc` — is not covered either, because which flags carry one
/// differs per server, and guessing wrong either leaks it or hides something
/// needed.
fn masked(line: &str) -> String {
    line.split(' ')
        .map(mask_token)
        .collect::<Vec<_>>()
        .join(" ")
}

/// One whitespace-separated word, with its password hidden if it has one.
fn mask_token(token: &str) -> String {
    let Some(scheme) = token.find("://") else {
        return token.to_owned();
    };
    let authority_at = scheme + 3;
    // The userinfo ends at the last `@` before the path starts; a password may
    // legitimately contain one, so the last is the separator, not the first.
    let authority_end = token[authority_at..]
        .find(['/', '?', '#'])
        .map_or(token.len(), |end| authority_at + end);
    let Some(at) = token[authority_at..authority_end].rfind('@') else {
        return token.to_owned();
    };
    let userinfo = &token[authority_at..authority_at + at];
    let Some(colon) = userinfo.find(':') else {
        return token.to_owned();
    };
    format!(
        "{}{}:•••{}",
        &token[..authority_at],
        &userinfo[..colon],
        &token[authority_at + at..]
    )
}

fn details(kind: &str, entry: &Value) -> Vec<String> {
    let text = |key: &str| entry[key].as_str().unwrap_or("unknown");
    let mut rows = vec![format!(
        "source: {} ({})",
        text("source"),
        text("ecosystem")
    )];
    if kind == "mcp" {
        rows.push(match entry["command"].as_str() {
            Some(command) => {
                let args = entry["args"]
                    .as_array()
                    .map(|args| {
                        args.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                masked(&format!("command: {command} {args}"))
                    .trim_end()
                    .to_owned()
            }
            None => masked(&format!("url: {}", text("url"))),
        });
        let mut state = format!("trust: {} · level: {}", text("trust"), text("level"));
        if entry["enabled"] == Value::Bool(false) {
            state.push_str(" · disabled by its source");
        }
        rows.push(state);
    } else {
        rows.push(format!("lifecycle: {}", text("event")));
        let handlers = entry["handlers"]
            .as_array()
            .map(|handlers| {
                handlers
                    .iter()
                    .filter_map(|handler| handler["type"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        // A declared event can carry an empty handler list; a blank field reads
        // as a rendering fault rather than as the hook doing nothing.
        let handlers = if handlers.is_empty() {
            "none".to_owned()
        } else {
            handlers
        };
        rows.push(format!(
            "handlers: {handlers} · effect: {} · level: {}",
            text("effect"),
            text("level")
        ));
        rows.push(format!(
            "engine: {} · on failure: {}",
            text("effect_class"),
            text("on_failure")
        ));
    }
    rows
}

/// Claude's user-scope connections, from the operator's own `~/.claude.json`.
///
/// A file that cannot be read or does not parse yields nothing rather than
/// failing the listing: it is not this workspace's file, and a listing that
/// refuses to show the workspace's own connections because of it is worse
/// than one that is short.
fn user_declarations(kind: &str) -> Vec<serde_json::Value> {
    // Both kinds live in the operator's home and both are loaded from there:
    // MCP connections in `~/.claude.json`, hooks in `~/.claude/settings.json`.
    let (file, read): (&str, fn(&Path) -> Vec<serde_json::Value>) = match kind {
        "hook" => (".claude/settings.json", |path| {
            arsy_code::compat::user_hook_declarations(path).unwrap_or_default()
        }),
        _ => (".claude.json", |path| {
            arsy_code::compat::user_mcp_declarations(path).unwrap_or_default()
        }),
    };
    arsy_kernel::config::home_config_file(file)
        .map(|path| read(&path))
        .unwrap_or_default()
}

/// An empty result is ambiguous on its own, so it names what was read and which
/// filters were applied — otherwise a typo in `--event` looks like a missing
/// integration.
fn nothing_found(kind: &str, source: Option<&str>, event: Option<&str>) -> String {
    // `--source` narrows what `inspect` reads, so the list has to narrow with
    // it: naming a file that was skipped sends the reader to the wrong place.
    let searched = if kind == "mcp" {
        [
            (".mcp.json and ~/.claude.json (claude)", "claude"),
            (".codex/config.toml (codex)", "codex"),
            ("the nearest .omp/mcp.json (omp)", "omp"),
        ]
        .into_iter()
        .filter(|(_, ecosystem)| source.is_none_or(|source| source == *ecosystem))
        .map(|(file, _)| file)
        .collect::<Vec<_>>()
        .join(", ")
    } else {
        ".claude/settings.json, .claude/settings.local.json".to_owned()
    };
    let filters: Vec<String> = [
        source.map(|s| format!("--source {s}")),
        event.map(|e| format!("--event {e}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut message = format!("No declarations matched; searched {searched}.");
    if !filters.is_empty() {
        message.push_str(&format!(" Filters applied: {}.", filters.join(", ")));
    }
    message
}

#[cfg(test)]
mod tests {
    /// The fixtures alone. What the operator's own `~/.claude.json` declares is
    /// their business and would make this assert on whoever ran it.
    fn fixtures(
        root: &Path,
        kind: &str,
        name: Option<&str>,
        source: Option<&str>,
        event: Option<&str>,
        extra: Option<&Path>,
    ) -> Result<Value, Diagnostic> {
        inspect_with(root, kind, name, source, event, extra, &[])
    }

    /// The engine loads the operator's own `~/.claude/settings.json`, so the
    /// listing has to name those hooks too: a hook that runs and cannot be
    /// switched off is a switch that lies.
    #[test]
    fn a_hook_listing_names_the_operators_own_hooks_beside_the_workspaces() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".claude")).unwrap();
        std::fs::write(
            root.path().join(".claude/settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"repo.sh"}]}]}}"#,
        )
        .unwrap();
        let operators = home.path().join("settings.json");
        std::fs::write(
            &operators,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"mine.sh"}]}]}}"#,
        )
        .unwrap();
        let user = arsy_code::compat::user_hook_declarations(&operators).unwrap();

        let report = inspect_with(root.path(), "hook", None, None, None, None, &user).unwrap();
        let entries = report["entries"].as_array().unwrap();
        let sources: Vec<&str> = entries
            .iter()
            .filter_map(|entry| entry["source"].as_str())
            .collect();
        assert!(
            sources
                .iter()
                .any(|source| source.contains("settings.json")),
            "{sources:?}"
        );
        assert!(
            sources
                .iter()
                .any(|source| *source == operators.display().to_string()),
            "the operator's own file is listed: {sources:?}"
        );

        // The key a switch would write is the one the engine builds for that
        // file, so turning it off reaches the rule rather than nothing.
        let declaration = entries
            .iter()
            .find(|entry| entry["source"].as_str() == Some(&operators.display().to_string()))
            .and_then(|entry| entry["declaration"].as_str())
            .unwrap()
            .to_owned();
        assert_eq!(
            declaration,
            format!("{}#PreToolUse[0].0", operators.display())
        );
    }

    #[test]
    fn an_omp_listing_does_not_depend_on_arsy_json() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".arsy")).unwrap();
        std::fs::write(root.path().join(".arsy/arsy.json"), "{not json").unwrap();
        assert!(fixtures(root.path(), "mcp", None, Some("claude"), None, None).is_err());
        assert!(fixtures(root.path(), "mcp", None, Some("omp"), None, None).is_ok());
    }

    fn declared(name: &str, command: &str, args: &[&str], source: &str) -> Value {
        json!({
            "name": name,
            "transport": "stdio",
            "command": command,
            "args": args,
            "source": source,
            "trust": "untrusted",
            "level": "mapped",
            "runtime_status": "not loaded",
        })
    }

    /// The rows differ; the fields every row repeats do not, and a screenful
    /// of those buries the rows that carry the answer.
    #[test]
    fn a_card_states_the_shared_fields_once_and_squares_its_border() {
        let entries = vec![
            declared(
                "mongodb",
                "npx",
                &["-y", "mongodb-mcp-server"],
                "~/.claude.json",
            ),
            declared(
                "websift",
                "/Users/me/.cargo/bin/websift",
                &["mcp"],
                "~/.claude.json",
            ),
        ];
        let drawn = card(&entries);

        assert!(drawn.contains("MCP · 2 declared · none loaded"), "{drawn}");
        // The package, not the runner: `npx` names nothing a reader can use.
        assert!(drawn.contains("npx mongodb-mcp-server"), "{drawn}");
        assert!(drawn.contains("websift"), "{drawn}");
        assert_eq!(
            drawn.matches("~/.claude.json").count(),
            1,
            "the source is stated once, not once per row:\n{drawn}"
        );

        // Every row reaches the same column, whatever it holds.
        let widths: Vec<usize> = drawn
            .lines()
            .filter(|line| line.contains('│') || line.contains('╭') || line.contains('╰'))
            .map(visible_width)
            .collect();
        assert!(widths.windows(2).all(|pair| pair[0] == pair[1]), "{drawn}");
    }

    /// Two servers that disagree about a field have it on their own rows, not
    /// asserted for both.
    #[test]
    fn a_field_the_entries_disagree_on_is_not_stated_for_all_of_them() {
        let entries = vec![
            declared("a", "x", &[], "~/.claude.json"),
            declared("b", "y", &[], ".mcp.json"),
        ];
        assert!(!card(&entries).contains("all: ~/.claude.json"));
        assert!(
            card(&entries).contains("trust untrusted"),
            "what they do agree on is still stated"
        );
    }

    /// A connection string is an argument, and the listing used to print it
    /// whole. The password is the one part of it nobody watching needs.
    #[test]
    fn a_listed_command_keeps_its_target_and_hides_its_password() {
        assert_eq!(
            masked("command: npx -y mongodb-mcp-server --connectionString mongodb://root:tYytHtubfP@10.2.238.111:31847/"),
            "command: npx -y mongodb-mcp-server --connectionString mongodb://root:•••@10.2.238.111:31847/"
        );
        // An unencoded `@` in the password is common, so the last one in the
        // authority is the separator rather than the first.
        assert_eq!(
            mask_token("postgresql://po_mulham:b2p@rCX60!@10.2.237.129:5432/oss_rba_test"),
            "postgresql://po_mulham:•••@10.2.237.129:5432/oss_rba_test"
        );
        assert_eq!(
            mask_token("postgresql://TDB_HM8135:6pqbhkqvpt1a30!@10.2.238.22:5432/oss_rba"),
            "postgresql://TDB_HM8135:•••@10.2.238.22:5432/oss_rba"
        );
        // Nothing to hide, nothing changed.
        assert_eq!(
            mask_token("https://stitch.googleapis.com/mcp"),
            "https://stitch.googleapis.com/mcp"
        );
        assert_eq!(
            mask_token("mongodb://10.2.238.111:31847/"),
            "mongodb://10.2.238.111:31847/"
        );
        assert_eq!(mask_token("--profile"), "--profile");
        assert_eq!(mask_token(""), "");
    }

    use super::*;

    #[test]
    fn inspection_commands_validate_arguments_and_read_real_fixtures() {
        for args in [
            vec!["mcp", "list"],
            vec!["mcp", "show", "x", "--source", "codex"],
            vec!["hook", "list", "--event", "PreToolUse"],
        ] {
            assert!(crate::parse(args.into_iter().map(str::to_owned)).is_ok());
        }
        for args in [
            vec!["mcp"],
            vec!["hook", "run"],
            vec!["mcp", "list", "--event", "Stop"],
            vec!["hook", "list", "--source", "invalid"],
        ] {
            assert!(crate::parse(args.into_iter().map(str::to_owned)).is_err());
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/compat/claude/input")
            .canonicalize()
            .unwrap();
        let hooks = fixtures(
            &root,
            "hook",
            None,
            Some("claude"),
            Some("PreToolUse"),
            None,
        )
        .unwrap();
        assert!(!hooks["entries"].as_array().unwrap().is_empty());
        assert_eq!(hooks["entries"][0]["runtime_status"], "not_loaded");
        let mcp = fixtures(&root, "mcp", None, Some("claude"), None, None).unwrap();
        assert!(!mcp["entries"].as_array().unwrap().is_empty());
        assert!(fixtures(&root, "mcp", Some("missing"), Some("claude"), None, None).is_err());
        // Naming no ecosystem keeps every declaration the `claude` filter
        // found, unchanged. It is a superset rather than an equal set because
        // ARSY's own bundled connection is always declared, so the unfiltered
        // listing carries it too.
        let unfiltered = fixtures(&root, "mcp", None, None, None, None).unwrap();
        let unfiltered = unfiltered["entries"].as_array().unwrap();
        for entry in mcp["entries"].as_array().unwrap() {
            assert!(unfiltered.contains(entry), "{entry} was dropped or altered");
        }
        assert!(unfiltered
            .iter()
            .any(|entry| entry["name"] == arsy_kernel::config::FLUXGUARD_MCP_SERVER));
        let listing = human_report(&mcp, "mcp", Some("claude"), None)["declarations"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(listing.contains("not loaded"), "{listing}");
        assert!(listing.contains("declared; none loaded"), "{listing}");

        // The count line and the rows read the same field, so one can never say
        // nothing is loaded while the other names something that is.
        let mixed = json!({
            "entries": [
                {"name": "a", "transport": "stdio", "runtime_status": "loaded"},
                {"name": "b", "transport": "stdio", "runtime_status": "not_loaded"},
            ],
            "notice": "",
        });
        let mixed = human_report(&mixed, "mcp", None, None)["declarations"]
            .as_str()
            .unwrap()
            .to_owned();
        // Two entries are drawn as a card, so the headline is where the count
        // lands; the marks on the rows read the same field it does.
        assert!(mixed.contains("MCP · 2 declared · 1 loaded"), "{mixed}");
        assert_eq!(mixed.matches('●').count(), 1, "{mixed}");
        assert_eq!(mixed.matches('○').count(), 1, "{mixed}");

        let all = json!({
            "entries": [{"name": "a", "transport": "stdio", "runtime_status": "loaded"}],
            "notice": "",
        });
        let all = human_report(&all, "mcp", None, None)["declarations"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(all.contains("1 MCP server declared; all loaded"), "{all}");
        assert!(!all.contains("none loaded"), "{all}");
        // The record itself is never printed at a person: only the fields that
        // say what a connection would run.
        assert!(!listing.contains("\"runtime_status\""), "{listing}");
        assert!(
            listing.contains("command: ") || listing.contains("url: "),
            "{listing}"
        );

        // An empty result names what was read and which filters narrowed it.
        let empty = fixtures(
            &root,
            "hook",
            None,
            Some("claude"),
            Some("NoSuchEvent"),
            None,
        )
        .unwrap();
        let empty = human_report(&empty, "hook", Some("claude"), Some("NoSuchEvent"))
            ["declarations"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(empty.contains(".claude/settings.json"), "{empty}");
        assert!(empty.contains("--event NoSuchEvent"), "{empty}");

        // `--source` skips the other ecosystems, so naming their files would
        // send the reader to a file that was never read.
        let scoped = human_report(
            &json!({"entries": [], "notice": ""}),
            "mcp",
            Some("codex"),
            None,
        )["declarations"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(scoped.contains(".codex/config.toml"), "{scoped}");
        assert!(!scoped.contains(".mcp.json"), "{scoped}");
        assert!(!scoped.contains(".omp/mcp.json"), "{scoped}");
    }

    #[test]
    fn hook_rows_report_lifecycle_handlers_and_unsupported_events() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/compat/claude/input")
            .canonicalize()
            .unwrap();
        let hooks = inspect(&root, "hook", None, Some("claude"), None, None).unwrap();
        let listing = human_report(&hooks, "hook", None, None)["declarations"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(listing.contains("lifecycle: "), "{listing}");
        assert!(listing.contains("handlers: "), "{listing}");
        assert!(listing.contains("effect: "), "{listing}");
    }
}
