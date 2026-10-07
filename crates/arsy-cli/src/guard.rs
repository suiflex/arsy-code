//! Adding and removing hooks in ARSY's own `guard.json`, the operator's or the
//! project's: `arsy hook add|remove` and the `/hooks` dialog.
//!
//! Only ARSY's file is ever written. Claude's and Codex's files are read as
//! lower layers and stay theirs.

use crate::{config_edit, mcp::Scope, usage, Command, Diagnostic, Emitter, Invocation, Output};
use arsy_code::hook::{LifecycleEvent, EXTERNAL_EVENTS};
use serde_json::json;
use std::path::{Path, PathBuf};

pub const FILE: &str = "guard.json";

/// The `guard.json` a scope names.
pub fn path(root: &Path, scope: Scope) -> Result<PathBuf, String> {
    match scope {
        Scope::User => arsy_kernel::config::config_home()
            .map(|home| home.join(FILE))
            .ok_or_else(|| "this platform has no ARSY configuration home".to_owned()),
        Scope::Workspace => Ok(root.join(arsy_code::workspace::HARNESS_STATE).join(FILE)),
    }
}

/// Which scope's file `file` is, when it is one of ARSY's two.
#[cfg(feature = "tui")]
pub fn scope_of(root: &Path, file: &Path) -> Option<Scope> {
    [Scope::User, Scope::Workspace]
        .into_iter()
        .find(|scope| path(root, *scope).is_ok_and(|own| own == file))
}

fn check_event(event: &str) -> Result<(), String> {
    if LifecycleEvent::from_external(event).is_some() {
        Ok(())
    } else {
        Err(format!(
            "`{event}` is not an event ARSY runs; use one of {}",
            EXTERNAL_EVENTS.join(", ")
        ))
    }
}

/// Append a `command` hook to the scope's `guard.json`, and return the file.
pub fn add(
    root: &Path,
    scope: Scope,
    event: &str,
    matcher: Option<&str>,
    command: &str,
    timeout: Option<u64>,
) -> Result<PathBuf, String> {
    check_event(event)?;
    let file = path(root, scope)?;
    crate::settings::rewrite(&file, |guard| {
        config_edit::add_hook(guard, event, matcher, command, timeout)
    })?;
    Ok(file)
}

/// Remove the entry at `position` under `event` from the scope's
/// `guard.json`, keeping every switched-off hook switched off.
pub fn remove(root: &Path, scope: Scope, event: &str, position: usize) -> Result<PathBuf, String> {
    check_event(event)?;
    let file = path(root, scope)?;
    crate::settings::rewrite(&file, |guard| {
        config_edit::remove_hook(guard, event, position)
    })?;
    // `hook.disabled` is written to the operator's own arsy.json by `/hooks`.
    if let Some(config) = crate::config_load::operator_user_config() {
        if config.exists() {
            let source = file.display().to_string();
            crate::settings::rewrite(&config, |settings| {
                config_edit::shift_disabled(settings, &source, event, position)
            })?;
        }
    }
    Ok(file)
}

/// `path#Event[position].handler`, the key the engine gives a declaration,
/// taken apart.
#[cfg(feature = "tui")]
pub fn parse_declaration(declaration: &str) -> Option<(PathBuf, String, usize)> {
    let (file, rest) = declaration.rsplit_once('#')?;
    let (event, rest) = rest.split_once('[')?;
    let (position, _) = rest.split_once(']')?;
    Some((
        PathBuf::from(file),
        event.to_owned(),
        position.parse().ok()?,
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    Add {
        event: String,
        matcher: Option<String>,
        command: String,
        timeout: Option<u64>,
        scope: Scope,
    },
    Remove {
        event: String,
        position: usize,
        scope: Scope,
    },
}

const HELP: &str = "hook requires `list`, `add --event <EVENT> [--matcher <PATTERN>] --command <CMD> [--timeout <SECONDS>]`, or `remove <EVENT> <POSITION>`; add and remove take --scope <user|workspace>";

/// `hook add` and `hook remove`, or `None` for the inspection parser.
pub fn parse(arguments: &crate::ParsedArguments) -> Option<Result<Command, Diagnostic>> {
    let scope = || Scope::parse(arguments.scope.as_deref());
    match arguments.positional.first().map(String::as_str) {
        Some("add") => Some((|| {
            if arguments.positional.len() != 1 {
                return Err(usage(HELP));
            }
            Ok(Command::Hook {
                request: Request::Add {
                    event: arguments.event.clone().ok_or_else(|| usage(HELP))?,
                    matcher: arguments.matcher.clone(),
                    command: arguments.command.clone().ok_or_else(|| usage(HELP))?,
                    timeout: arguments.timeout,
                    scope: scope()?,
                },
            })
        })()),
        Some("remove") => Some((|| match arguments.positional.as_slice() {
            [_, event, position] => Ok(Command::Hook {
                request: Request::Remove {
                    event: event.clone(),
                    position: position
                        .parse()
                        .map_err(|_| usage("the position is a whole number, from `hook list`"))?,
                    scope: scope()?,
                },
            }),
            _ => Err(usage(HELP)),
        })()),
        _ => None,
    }
}

pub fn run(
    invocation: &Invocation,
    request: &Request,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let refused = |reason: String| {
        Diagnostic::error(
            "ARSY-CFG-1000",
            reason,
            "`arsy hook list` shows every hook and where it is declared",
        )
    };
    let (scope, message) = match request {
        Request::Add {
            event,
            matcher,
            command,
            timeout,
            scope,
        } => {
            let file = add(&root, *scope, event, matcher.as_deref(), command, *timeout)
                .map_err(refused)?;
            let mut message = format!("Added a {event} hook to {}.", file.display());
            if *scope == Scope::Workspace {
                message.push_str(" It runs once this directory is trusted.");
            }
            (scope, message)
        }
        Request::Remove {
            event,
            position,
            scope,
        } => {
            let file = remove(&root, *scope, event, *position).map_err(refused)?;
            (
                scope,
                format!("Removed {event} hook {position} from {}.", file.display()),
            )
        }
    };
    emitter.result(if emitter.output == Output::Json {
        json!({"scope": scope.as_str(), "message": message})
    } else {
        json!({"hook": message})
    });
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "tui")]
    #[test]
    fn a_declaration_key_comes_apart() {
        assert_eq!(
            parse_declaration("/w/.arsy/guard.json#PreToolUse[2].0"),
            Some((
                PathBuf::from("/w/.arsy/guard.json"),
                "PreToolUse".to_owned(),
                2
            ))
        );
        assert_eq!(parse_declaration("/w/.codex/config.toml#notify"), None);
    }

    #[test]
    fn a_project_hook_lands_in_the_project_guard_and_can_be_removed() {
        let root = tempfile::tempdir().unwrap();
        let file = add(root.path(), Scope::Workspace, "Stop", None, "done.sh", None).unwrap();
        assert_eq!(file, root.path().join(".arsy/guard.json"));
        #[cfg(feature = "tui")]
        assert_eq!(scope_of(root.path(), &file), Some(Scope::Workspace));

        remove(root.path(), Scope::Workspace, "Stop", 0).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert!(written["hooks"].get("Stop").is_none());
    }

    #[test]
    fn an_event_arsy_does_not_run_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let refused = add(
            root.path(),
            Scope::Workspace,
            "Notification",
            None,
            "x",
            None,
        );
        assert!(refused.unwrap_err().contains("PreToolUse"));
        assert!(!root.path().join(".arsy/guard.json").exists());
    }
}
