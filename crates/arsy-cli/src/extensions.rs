//! `arsy skill list` and `arsy plugin`: what is loaded, and what may load.
//!
//! Skills are data — listing one grants nothing — so that command only reads.
//! Plugins carry compute, so installing one shows the capabilities it asks for
//! and stops until an operator says yes; a refresh re-reads sources but can
//! never widen what was approved.

use crate::{storage_failed, usage, Command, Diagnostic, Emitter, Invocation, Output};
use arsy_code::{
    compat::{CompatibilityImporter, Ecosystem},
    plugin::{ExtensionSet, Grant, Installed, Registry},
};
use serde_json::{json, Value};
use std::{io::Write, path::PathBuf};

pub fn parse_skill(arguments: &crate::ParsedArguments) -> Result<Command, Diagnostic> {
    if arguments
        .source
        .as_deref()
        .is_some_and(|source| !matches!(source, "claude" | "codex" | "omp"))
    {
        return Err(usage("--source requires claude, codex, or omp"));
    }
    match arguments.positional.first().map(String::as_str) {
        Some("list") if arguments.positional.len() == 1 => Ok(Command::SkillList {
            source: arguments.source.clone(),
        }),
        _ => Err(usage("skill requires `list` [--source <ECOSYSTEM>]")),
    }
}

const PLUGIN_HELP: &str = "plugin requires `list [--capabilities]`, `install <SOURCE>`, \
                           `inspect <ID>`, `remove <ID>`, or `refresh [ID] [--dry-run]`";

pub fn parse_plugin(arguments: &crate::ParsedArguments) -> Result<Command, Diagnostic> {
    let mut positional = arguments.positional.clone();
    let Some(action) = positional.first().cloned() else {
        return Err(usage(PLUGIN_HELP));
    };
    positional.remove(0);
    let one = |positional: Vec<String>, what: &str| -> Result<String, Diagnostic> {
        crate::only_argument(positional, &format!("plugin {action}"), what)
    };
    match action.as_str() {
        "list" => {
            if !positional.is_empty() {
                return Err(usage("plugin list takes no positional argument"));
            }
            Ok(Command::PluginList {
                capabilities: arguments.capabilities,
            })
        }
        "install" => Ok(Command::PluginInstall {
            source: PathBuf::from(one(positional, "<SOURCE>")?),
            force: arguments.force,
        }),
        "run" => Ok(Command::PluginRun {
            id: one(positional, "<ID>")?,
            input: arguments.to.clone().unwrap_or_default(),
        }),
        "inspect" => Ok(Command::PluginInspect {
            id: one(positional, "<ID>")?,
        }),
        "remove" => Ok(Command::PluginRemove {
            id: one(positional, "<ID>")?,
        }),
        "refresh" => {
            if positional.len() > 1 {
                return Err(usage("plugin refresh takes at most one plugin ID"));
            }
            Ok(Command::PluginRefresh {
                id: positional.into_iter().next(),
                dry_run: arguments.dry_run,
            })
        }
        other => Err(usage(format!(
            "unknown plugin subcommand `{other}`\n{PLUGIN_HELP}"
        ))),
    }
}

/// `arsy skill list`: every skill declared by the ecosystems in this workspace.
pub fn skills(
    invocation: &Invocation,
    source: Option<&str>,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let importer = CompatibilityImporter::new(&root);
    let mut skills = Vec::new();
    for ecosystem in [Ecosystem::Claude, Ecosystem::Codex, Ecosystem::Omp] {
        if source.is_some_and(|source| source != ecosystem.as_str()) {
            continue;
        }
        for mut skill in importer.skill_declarations(ecosystem).map_err(|error| {
            Diagnostic::error(
                "ARSY-CMP-1001",
                format!("{} skills could not be read: {error}", ecosystem.as_str()),
                "fix the source directory; no skill was loaded",
            )
        })? {
            skill["ecosystem"] = json!(ecosystem.as_str());
            // A skill is instructions. It is authority-free by construction,
            // and saying so is the point of listing it.
            skill["authority"] = json!("data_only");
            skills.push(skill);
        }
    }
    let report = json!({"skills": skills, "source": source});
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        json!({"skills": human_skills(&report)})
    });
    Ok(0)
}

fn human_skills(report: &Value) -> String {
    let skills = report["skills"].as_array().map_or(&[][..], Vec::as_slice);
    if skills.is_empty() {
        return "No skills are declared in this workspace.".to_owned();
    }
    let mut text = format!("{} skill(s), all data-only\n", skills.len());
    for skill in skills {
        text.push_str(&format!(
            "  {} · {} · {}\n",
            skill["name"].as_str().unwrap_or("?"),
            skill["ecosystem"].as_str().unwrap_or("?"),
            skill["source"].as_str().unwrap_or("?"),
        ));
    }
    text
}

pub fn list(
    invocation: &Invocation,
    capabilities: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let (installed, broken) = Registry::open(&root).list().map_err(storage_failed)?;
    let report = json!({
        "plugins": installed.iter().map(|plugin| describe(plugin, capabilities)).collect::<Vec<_>>(),
        "unreadable": broken
            .iter()
            .map(|(id, error)| json!({"id": id, "error": error.to_string()}))
            .collect::<Vec<_>>(),
    });
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        json!({"plugins": human_list(&report)})
    });
    Ok(0)
}

fn describe(plugin: &Installed, capabilities: bool) -> Value {
    let mut entry = json!({
        "id": plugin.manifest.id,
        "version": plugin.manifest.version,
        "api": plugin.manifest.api,
        "signature": plugin.signature,
        "loadable": plugin.loadable(),
        "approved_version": plugin.grant.approved_version,
    });
    if capabilities {
        entry["requested"] = json!(plugin
            .manifest
            .capabilities
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>());
        entry["granted"] = json!(plugin.grant.granted);
        entry["beyond_grant"] = json!(plugin.grant.excess(&plugin.manifest));
    }
    entry
}

fn human_list(report: &Value) -> String {
    let plugins = report["plugins"].as_array().map_or(&[][..], Vec::as_slice);
    let unreadable = report["unreadable"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    if plugins.is_empty() && unreadable.is_empty() {
        return "No plugins are installed.".to_owned();
    }
    let mut text = format!("{} plugin(s)\n", plugins.len());
    for plugin in plugins {
        text.push_str(&format!(
            "  {} {} · signature {} · {}\n",
            plugin["id"].as_str().unwrap_or("?"),
            plugin["version"].as_str().unwrap_or("?"),
            plugin["signature"].as_str().unwrap_or("?"),
            if plugin["loadable"] == Value::Bool(true) {
                "loadable"
            } else {
                "held back: it asks for more than was approved"
            },
        ));
        if let Some(requested) = plugin["requested"].as_array() {
            text.push_str(&format!(
                "    requested: {}\n",
                requested
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    for entry in unreadable {
        text.push_str(&format!(
            "  {} · unreadable: {}\n",
            entry["id"].as_str().unwrap_or("?"),
            entry["error"].as_str().unwrap_or("?"),
        ));
    }
    text
}

/// `arsy plugin run`: invoke a plugin the same way a model would.
///
/// Through the tool runtime rather than the extension host directly, so the
/// operator's own invocation is policy-checked, recorded, and rendered exactly
/// as one the model asks for -- a plugin an operator can run by hand and a
/// plugin the agent can call are the same plugin under the same rules.
pub fn run(
    invocation: &Invocation,
    id: &str,
    input: &str,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let config = crate::load_config(&root, &working, invocation.config.as_deref())?;
    let scope = arsy_kernel::domain::SessionId::new().to_string();
    let runtime = crate::agent_runtime(&root, &config, false, &scope, None, None, None, emitter)?;
    let result = runtime.invoke("plugin.invoke", &json!({"plugin": id, "input": input}));
    emitter.result(json!({
        "plugin": id,
        "ran": result.success,
        "output": result.output,
        "duration_ms": u64::try_from(result.duration.as_millis()).unwrap_or(u64::MAX),
        "artifact": result.artifact.map(|id| id.to_string()),
    }));
    // A plugin that refused or failed is a failed invocation, not a failed
    // command: the exit code says which so a script can branch on it.
    Ok(if result.success { 0 } else { 6 })
}

pub fn inspect(
    invocation: &Invocation,
    id: &str,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let plugin = Registry::open(&root)
        .get(id)
        .map_err(|error| not_installed(id, &error))?;
    emitter.result(json!({
        "id": plugin.manifest.id,
        "version": plugin.manifest.version,
        "api": plugin.manifest.api,
        "entrypoint": plugin.manifest.entrypoint,
        "directory": plugin.directory.display().to_string(),
        // No signature is verified by this build, so publisher identity is
        // reported as unestablished rather than implied by the file's presence.
        "signature": plugin.signature,
        "publisher": Value::Null,
        "requested": plugin
            .manifest
            .capabilities
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        "granted": plugin.grant.granted,
        "beyond_grant": plugin.grant.excess(&plugin.manifest),
        "loadable": plugin.loadable(),
        "limits": {
            "fuel": DEFAULT_FUEL,
            "timeout_ms": DEFAULT_TIMEOUT_MS,
            "memory_bytes": DEFAULT_MEMORY_BYTES,
            "output_bytes": DEFAULT_OUTPUT_BYTES,
        },
    }));
    Ok(0)
}

/// Invocation limits a plugin runs under. Reported by `inspect` so an operator
/// sees the bounds before approving anything.
const DEFAULT_FUEL: u64 = 100_000_000;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_MEMORY_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_OUTPUT_BYTES: u64 = 1024 * 1024;

/// `arsy plugin install`: show what is asked for, then stop until told to go on.
pub fn install(
    invocation: &Invocation,
    source: &std::path::Path,
    force: bool,
    tty: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let (manifest, _) = Registry::inspect_source(source).map_err(|error| {
        usage(format!(
            "{} is not an installable plugin: {error}",
            source.display()
        ))
    })?;
    let requested: Vec<String> = manifest
        .capabilities
        .iter()
        .map(ToString::to_string)
        .collect();

    if !force {
        if !tty {
            return Err(Diagnostic::error(
                "ARSY-POL-1001",
                format!(
                    "installing `{}` grants {}; it was not confirmed",
                    manifest.id,
                    if requested.is_empty() {
                        "no capability".to_owned()
                    } else {
                        requested.join(", ")
                    }
                ),
                "re-run with --force to install non-interactively, having read the capabilities above",
            ));
        }
        let mut terminal = std::io::stdout();
        let _ = writeln!(
            terminal,
            "{} {} requests: {}\nInstall? [y/N] ",
            manifest.id,
            manifest.version,
            if requested.is_empty() {
                "no capability".to_owned()
            } else {
                requested.join(", ")
            }
        );
        let _ = terminal.flush();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|error| storage_failed(error.to_string()))?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            emitter.result(json!({
                "plugin": manifest.id,
                "installed": false,
                "reason": "not confirmed",
            }));
            return Ok(0);
        }
    }

    let now = arsy_kernel::artifact::unix_time_ms();
    let installed = Registry::open(&root)
        .install(source, &Grant::for_manifest(&manifest, now), now)
        .map_err(|error| usage(format!("`{}` could not be installed: {error}", manifest.id)))?;
    emitter.result(json!({
        "plugin": installed.manifest.id,
        "version": installed.manifest.version,
        "installed": true,
        "granted": installed.grant.granted,
        "directory": installed.directory.display().to_string(),
        "effective_at": "the next turn boundary",
    }));
    Ok(0)
}

pub fn remove(invocation: &Invocation, id: &str, emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let removed = Registry::open(&root)
        .remove(id)
        .map_err(|error| not_installed(id, &error))?;
    emitter.result(json!({
        "plugin": id,
        "removed": true,
        // The grant lived beside the plugin, so uninstalling revoked it: a
        // reinstall asks for approval again.
        "revoked": removed.grant.granted,
    }));
    Ok(0)
}

/// `arsy plugin refresh`: re-read sources and report what would change.
///
/// A refresh from the command line has no turn to sit between, so it stages and
/// commits in one step; `--dry-run` stages and discards, which is how an
/// operator sees the plan without adopting it.
pub fn refresh(
    invocation: &Invocation,
    id: Option<&str>,
    dry_run: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let registry = Registry::open(&root);
    // The baseline is what a turn boundary last committed, not what happens to
    // be on disk: a source edited while nothing was running is exactly the
    // change this command exists to report.
    let mut set = ExtensionSet::new(registry.loaded_versions().map_err(storage_failed)?);
    let staged = set.refresh(&registry, id).map_err(storage_failed)?.clone();
    let committed = if dry_run {
        set.discard();
        false
    } else {
        // No turn is in flight when the command line asks, so the boundary is
        // now; the record it writes is what the next runtime starts from.
        set.commit_at_turn_boundary();
        registry
            .record_loaded(set.loaded())
            .map_err(storage_failed)?;
        true
    };
    let mut report = staged.report();
    report["dry_run"] = json!(dry_run);
    report["committed"] = json!(committed);
    report["loaded"] = json!(set
        .loaded()
        .iter()
        .map(|(id, version)| json!({"id": id, "version": version}))
        .collect::<Vec<_>>());
    emitter.result(report);
    Ok(0)
}

fn not_installed(id: &str, error: &arsy_code::plugin::PluginError) -> Diagnostic {
    match error {
        arsy_code::plugin::PluginError::NotInstalled(_) => usage(format!(
            "no plugin `{id}` is installed; list them with `arsy plugin list`"
        )),
        other => storage_failed(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&str]) -> Result<Command, Diagnostic> {
        crate::parse(args.iter().map(|argument| (*argument).to_owned())).map(|it| it.command)
    }

    #[test]
    fn plugin_and_skill_subcommands_validate_their_arguments() {
        assert_eq!(
            command(&["plugin", "list", "--capabilities"]).unwrap(),
            Command::PluginList { capabilities: true }
        );
        assert_eq!(
            command(&["plugin", "refresh"]).unwrap(),
            Command::PluginRefresh {
                id: None,
                dry_run: false
            }
        );
        assert_eq!(
            command(&["plugin", "refresh", "a.b", "--dry-run"]).unwrap(),
            Command::PluginRefresh {
                id: Some("a.b".to_owned()),
                dry_run: true
            }
        );
        assert_eq!(
            command(&["skill", "list", "--source", "claude"]).unwrap(),
            Command::SkillList {
                source: Some("claude".to_owned())
            }
        );
        for refused in [
            vec!["plugin"],
            vec!["plugin", "install"],
            vec!["plugin", "inspect"],
            vec!["plugin", "remove", "a", "b"],
            vec!["plugin", "refresh", "a", "b"],
            vec!["plugin", "quarantine", "a"],
            vec!["skill"],
            vec!["skill", "list", "extra"],
            vec!["skill", "list", "--source", "borrowed"],
        ] {
            assert!(command(&refused).is_err(), "{refused:?}");
        }
    }
}
