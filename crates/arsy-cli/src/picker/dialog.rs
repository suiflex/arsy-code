//! The dialogs a bare `/mcp`, `/hooks`, `/skill`, `/session` or `/settings`
//! opens, and the frame loop they repaint through.

use super::wizard::write_config;
#[cfg(feature = "tui")]
use crate::*;
use arsy_kernel::provider::Effort;
use serde_json::Value;
use std::io::{self, Write};
use std::path::Path;
pub(crate) fn mcp_choices(
    root: &Path,
    invocation: &Invocation,
) -> Result<Vec<(Value, tui::McpChoice)>, Diagnostic> {
    let report =
        integrations::inspect(root, "mcp", None, None, None, invocation.config.as_deref())?;
    let mut rows: Vec<(Value, tui::McpChoice)> = Vec::new();
    for entry in report["entries"].as_array().into_iter().flatten() {
        let text = |key: &str| entry[key].as_str().unwrap_or_default().to_owned();
        let name = text("name");
        // The first row under a name is the one a toggle acts on: ARSY's own
        // definition is listed first, and it is the one that runs.
        if rows.iter().any(|(_, choice)| choice.name == name) {
            continue;
        }
        // A row the resolved configuration holds carries its real trust; a
        // declaration nothing reads live is labelled untrusted and starts
        // nothing until adopted.
        let native = entry["trust"] != "untrusted";
        let detail = match entry["url"].as_str() {
            Some(url) => url.to_owned(),
            None => std::iter::once(text("command"))
                .chain(
                    entry["args"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned),
                )
                .collect::<Vec<_>>()
                .join(" "),
        };
        let choice = tui::McpChoice {
            name,
            source: text("ecosystem"),
            trust: if native { text("trust") } else { String::new() },
            target: integrations::target(entry),
            detail,
            enabled: native.then(|| entry["enabled"].as_bool().unwrap_or(false)),
        };
        rows.push((entry.clone(), choice));
    }
    Ok(rows)
}

/// Every lifecycle hook declared, as the dialog offers it.
#[cfg(feature = "tui")]
pub(crate) fn hook_choices(
    root: &Path,
    invocation: &Invocation,
) -> Result<Vec<tui::HookChoice>, Diagnostic> {
    let report =
        integrations::inspect(root, "hook", None, None, None, invocation.config.as_deref())?;
    let config = load_config_for(invocation)?;
    Ok(report["entries"]
        .as_array()
        .into_iter()
        .flatten()
        // Only a declaration the engine registers is a declaration switching
        // off can reach: an unsupported event, or a handler type this build
        // never runs, would take the toggle, write a key that matches nothing,
        // and then read back as "off" — a change that never happened.
        .filter(|entry| entry["level"] == "mapped" && entry["handlers"][0]["type"] == "command")
        .filter_map(|entry| {
            let declaration = entry["declaration"].as_str()?.to_owned();
            Some(tui::HookChoice {
                declaration,
                event: entry["event"].as_str().unwrap_or("?").to_owned(),
                matcher: entry["matcher"].as_str().unwrap_or("*").to_owned(),
                source: format!(
                    "{} · {}",
                    entry["ecosystem"].as_str().unwrap_or("?"),
                    entry["source"].as_str().unwrap_or("?")
                ),
                enabled: !config
                    .hook_disabled()
                    .contains(entry["declaration"].as_str()?),
                removable: crate::guard::parse_declaration(entry["declaration"].as_str()?)
                    .is_some_and(|(file, _, _)| crate::guard::scope_of(root, &file).is_some()),
            })
        })
        .collect())
}

#[cfg(feature = "tui")]
pub(crate) fn run_hook_dialog(
    invocation: &Invocation,
    stdout: &mut io::Stdout,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
) -> Result<(), Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    let mut rows = hook_choices(&root, invocation)?;
    let mut dialog = tui::HookDialogState::new(rows);
    let mut changed = 0usize;
    let mut drawn = 0;
    loop {
        drawn = repaint_dialog(
            stdout,
            colour,
            drawn,
            &dialog.render(tui::terminal_width(), colour),
        )?;
        let action = match next_dialog_key(&mut dialog, keys, decoder, |dialog: &mut _, key| {
            dialog.handle_key(key)
        }) {
            Keyed::Ended => break,
            Keyed::Redraw => continue,
            Keyed::Acted(tui::HookAction::Close) => {
                break;
            }
            Keyed::Acted(tui::HookAction::Toggle(index)) => index,
            Keyed::Acted(tui::HookAction::Add {
                scope,
                event,
                matcher,
                command,
            }) => {
                let matcher = (!matcher.is_empty()).then_some(matcher.as_str());
                let added =
                    crate::guard::add(&root, file_scope(scope), &event, matcher, &command, None);
                dialog.notice = Some(tui::safe_text(&match added {
                    Ok(file) => {
                        changed += 1;
                        let trust = if scope == tui::SettingsScope::Project {
                            " It runs once this directory is trusted."
                        } else {
                            ""
                        };
                        format!("Added a {event} hook to {}.{trust}", file.display())
                    }
                    Err(reason) => reason,
                }));
                rows = hook_choices(&root, invocation)?;
                dialog.reload(rows);
                continue;
            }
            Keyed::Acted(tui::HookAction::Remove(index)) => {
                let declaration = dialog.choices[index].declaration.clone();
                let removed = crate::guard::parse_declaration(&declaration)
                    .and_then(|(file, event, position)| {
                        crate::guard::scope_of(&root, &file).map(|scope| (scope, event, position))
                    })
                    .ok_or_else(|| "only a hook in ARSY's own guard.json can be removed".to_owned())
                    .and_then(|(scope, event, position)| {
                        crate::guard::remove(&root, scope, &event, position)
                    });
                dialog.notice = Some(tui::safe_text(&match removed {
                    Ok(file) => {
                        changed += 1;
                        format!("Removed the hook from {}.", file.display())
                    }
                    Err(reason) => reason,
                }));
                rows = hook_choices(&root, invocation)?;
                dialog.reload(rows);
                continue;
            }
        };
        let choice = dialog.choices[action].clone();
        let enabled = !choice.enabled;
        // `hook.disabled` is an object of booleans, so a switch-off writes
        // `true` for the declaration and a switch-on removes the key: a key
        // that is absent is the same as one that says the hook runs.
        let change = write_config(|config| {
            if enabled {
                config_edit::remove(config, &["hook", "disabled", &choice.declaration])
            } else {
                config_edit::set(
                    config,
                    &["hook", "disabled"],
                    &choice.declaration,
                    serde_json::Value::Bool(true),
                )
            }
        });
        dialog.notice = Some(match change {
            Ok(()) => {
                let state = if enabled { "on" } else { "off" };
                changed += 1;
                format!("hook `{}` switched {state}.", choice.matcher)
            }
            Err(reason) => reason,
        });
        rows = hook_choices(&root, invocation)?;
        dialog.reload(rows);
    }
    close_dialog(stdout, drawn, &hook_close_line(changed), "")?;
    Ok(())
}

/// Apply one edited setting's new value to the file `scope` names. `None`
/// when it was written, `Some(reason)` when it was refused and nothing changed.
#[cfg(feature = "tui")]
pub(crate) fn apply_edited_setting(
    root: &Path,
    scope: tui::SettingsScope,
    row: &tui::SettingRow,
    line: &str,
) -> Option<String> {
    crate::settings::set(root, file_scope(scope), &row.key, line).err()
}

/// The dialog's two scopes as the configuration writer names them.
#[cfg(feature = "tui")]
pub(crate) const fn file_scope(scope: tui::SettingsScope) -> mcp::Scope {
    match scope {
        tui::SettingsScope::User => mcp::Scope::User,
        tui::SettingsScope::Project => mcp::Scope::Workspace,
    }
}
/// One ecosystem's declared skills, as the dialog offers them.
///
/// Per-skill row building lives here so `skill_choices` stays a flat pass
/// over the ecosystems; the importer, config and root are shared, not grown.
#[cfg(feature = "tui")]
pub(crate) fn ecosystem_skill_rows(
    rows: &mut Vec<tui::SkillChoice>,
    ecosystem: arsy_code::compat::Ecosystem,
    config: &arsy_kernel::config::Config,
    root: &Path,
    importer: &arsy_code::compat::CompatibilityImporter,
) {
    // A source the operator switched off is not offered here either: its
    // skills reach neither the model nor the dialog, and a switch that showed
    // rows the prompt ignores would be lying about what it controls.
    if !config.compat_enabled(ecosystem.as_str()) {
        return;
    }
    let Ok(skills) = importer.skill_declarations(ecosystem) else {
        return;
    };
    for skill in skills {
        let Some(name) = skill["name"].as_str().map(str::to_owned) else {
            continue;
        };
        // A workspace skill by the name of one already listed — a user skill,
        // or another workspace's — is the one the operator means to have.
        if rows.iter().any(|row| row.name == name) {
            continue;
        }
        let key = format!("{}/{name}", ecosystem.as_str());
        rows.push(tui::SkillChoice {
            key: key.clone(),
            name,
            ecosystem: ecosystem.as_str().to_owned(),
            source: skill["source"].as_str().unwrap_or("?").to_owned(),
            description: skill_description(root, skill["source"].as_str().unwrap_or("")),
            disabled: config.skill_disabled().contains(&key).then_some(true),
        });
    }
}

#[cfg(feature = "tui")]
pub(crate) fn skill_choices(
    root: &Path,
    invocation: &Invocation,
) -> Result<Vec<tui::SkillChoice>, Diagnostic> {
    let config = load_config_for(invocation)?;
    let importer = arsy_code::compat::CompatibilityImporter::new(root);
    let mut rows = Vec::new();
    // The operator's own home first, in the order the compat switches run.
    let homes = compat_homes();
    for skill in arsy_compat::skills::all(
        &homes,
        config.compat_enabled("claude"),
        config.compat_enabled("codex"),
    ) {
        let key = format!("{}/{}", skill.ecosystem, skill.name);
        rows.push(tui::SkillChoice {
            key: key.clone(),
            name: skill.name,
            ecosystem: skill.ecosystem.to_owned(),
            source: skill.path.display().to_string(),
            description: skill_description(root, &skill.path.display().to_string()),
            disabled: config.skill_disabled().contains(&key).then_some(true),
        });
    }
    for ecosystem in [
        arsy_code::compat::Ecosystem::Claude,
        arsy_code::compat::Ecosystem::Codex,
        arsy_code::compat::Ecosystem::Omp,
    ] {
        ecosystem_skill_rows(&mut rows, ecosystem, &config, root, &importer);
    }
    Ok(rows)
}

#[cfg(feature = "tui")]
pub(crate) fn run_skill_dialog(
    invocation: &Invocation,
    stdout: &mut io::Stdout,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
) -> Result<(), Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    let mut rows = skill_choices(&root, invocation)?;
    let mut dialog = tui::SkillDialogState::new(rows);
    let mut changed = 0usize;
    let mut drawn = 0;
    loop {
        drawn = repaint_dialog(
            stdout,
            colour,
            drawn,
            &dialog.render(tui::terminal_width(), colour),
        )?;
        match next_dialog_key(&mut dialog, keys, decoder, |dialog: &mut _, key| {
            dialog.handle_key(key)
        }) {
            Keyed::Ended => break,
            Keyed::Redraw => continue,
            Keyed::Acted(tui::SkillAction::Close) => {
                break;
            }
            Keyed::Acted(tui::SkillAction::Toggle(index)) => {
                let choice = dialog.choices[index].clone();
                let offering = choice.disabled == Some(true);
                match write_config(|config| {
                    config_edit::set(
                        config,
                        &["skill", "disabled"],
                        &choice.key,
                        serde_json::Value::Bool(!offering),
                    )
                }) {
                    Ok(()) => {
                        let state = if offering { "offered" } else { "switched off" };
                        changed += 1;
                        dialog.notice = Some(format!("skill `{}` {state}.", choice.name));
                    }
                    Err(reason) => dialog.notice = Some(reason),
                }
                rows = skill_choices(&root, invocation)?;
                dialog.reload(rows);
                continue;
            }
            Keyed::Acted(tui::SkillAction::Read(index)) => {
                let choice = dialog.choices[index].clone();
                match std::fs::read_to_string(root.join(choice.source.trim_start_matches("./"))) {
                    Ok(body) => {
                        close_dialog(stdout, drawn, &[], "")?;
                        drawn = 0;
                        writeln!(stdout, "{}", tui::skill_body(&body, &choice.name))
                            .map_err(terminal_failed)?;
                        continue;
                    }
                    Err(error) => {
                        dialog.notice =
                            Some(format!("`{}` could not be read: {error}", choice.source));
                    }
                }
            }
        }
    }
    close_dialog(stdout, drawn, &skill_close_line(changed), "")?;
    Ok(())
}

/// Every setting the registry names, as the dialog offers it.
#[cfg(feature = "tui")]
pub(crate) fn setting_rows(invocation: &Invocation) -> Result<Vec<tui::SettingRow>, Diagnostic> {
    let config = load_config_for(invocation)?;
    let mut rows: Vec<tui::SettingRow> = config
        .settings()
        .into_iter()
        .map(|view| {
            let registered = arsy_kernel::config::setting(&view.key);
            // The registry names a kind for every key it lists, so the dialog
            // edits by the same table the loader validates against.
            let kind = match registered.map(|s| s.kind) {
                Some(arsy_kernel::config::SettingKind::Bool) => tui::SettingKind::Bool,
                Some(arsy_kernel::config::SettingKind::Choice(_)) => tui::SettingKind::Choice,
                Some(arsy_kernel::config::SettingKind::Integer { min, max }) => {
                    tui::SettingKind::Integer { min, max }
                }
                Some(arsy_kernel::config::SettingKind::Text) | None => tui::SettingKind::Text,
            };
            tui::SettingRow {
                section: registered.map_or("", |s| s.section).to_owned(),
                label: registered.map_or_else(|| view.key.clone(), |s| s.label.to_owned()),
                applies: applies(&view.key).to_owned(),
                key: tui::safe_text(&view.key),
                value: tui::safe_text(&view.value),
                default: tui::safe_text(&view.default),
                description: tui::safe_text(&view.description),
                choices: view
                    .choices
                    .iter()
                    .map(|choice| tui::safe_text(choice))
                    .collect(),
                kind,
                set: view.set,
                origin: view
                    .origin
                    .as_ref()
                    .map(|origin| match origin.layer {
                        arsy_kernel::config::Layer::Workspace
                        | arsy_kernel::config::Layer::Nested => "project".to_owned(),
                        layer => layer.as_str().to_owned(),
                    })
                    .unwrap_or_default(),
            }
        })
        .collect();
    // Grouped by section, in the order the dialog lists them; the registry
    // keeps its own order for `config explain`.
    let order = |row: &tui::SettingRow| {
        arsy_kernel::config::SETTING_SECTIONS
            .iter()
            .position(|section| *section == row.section)
            .unwrap_or(usize::MAX)
    };
    rows.sort_by_key(order);
    Ok(rows)
}

#[cfg(feature = "tui")]
pub(crate) fn run_settings_dialog(
    invocation: &Invocation,
    stdout: &mut io::Stdout,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    theme: &mut String,
    roles: &std::collections::BTreeMap<String, String>,
) -> Result<(), Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    let mut rows = setting_rows(invocation)?;
    let mut dialog = tui::SettingsDialogState::new(rows);
    let mut changes: Vec<String> = Vec::new();
    let mut drawn = 0;
    loop {
        drawn = repaint_dialog(
            stdout,
            colour,
            drawn,
            &dialog.render(tui::terminal_width(), colour),
        )?;
        match next_dialog_key(&mut dialog, keys, decoder, |dialog: &mut _, key| {
            dialog.handle_key(key)
        }) {
            Keyed::Ended => break,
            Keyed::Redraw => continue,
            Keyed::Acted(tui::SettingsAction::Close) => break,
            Keyed::Acted(tui::SettingsAction::Reset(index)) => {
                let row = dialog.rows[index].clone();
                let scope = dialog.scope;
                match crate::settings::unset(&root, file_scope(scope), &row.key) {
                    Ok(_) => {
                        changes.push(format!(
                            "`{}` removed from the {} settings.",
                            row.key,
                            scope.label()
                        ));
                        dialog.notice = Some(format!(
                            "`{}` removed from the {} settings; another layer or the default {} decides it.",
                            row.key,
                            scope.label(),
                            row.default
                        ));
                    }
                    Err(reason) => dialog.notice = Some(tui::safe_text(&reason)),
                }
                rows = setting_rows(invocation)?;
                dialog.reload(rows);
                continue;
            }
            Keyed::Acted(tui::SettingsAction::Apply(index, pending)) => {
                let row = dialog.rows[index].clone();
                let scope = dialog.scope;
                let notice = match apply_edited_setting(&root, scope, &row, &pending) {
                    None => format!(
                        "`{}` set to {} in the {} settings.",
                        row.key,
                        tui::safe_text(&pending),
                        scope.label()
                    ),
                    Some(reason) => tui::safe_text(&reason),
                };
                dialog.notice = Some(notice);
                // Two settings cannot wait for a restart, because they change
                // the screen the operator is looking at rather than what a
                // later session does. Applying one here is what makes it real:
                // a theme that only edits a file has not been chosen.
                if let Some(live) = apply_live_setting(&row, &pending, theme, roles) {
                    dialog.notice = Some(live);
                }
                rows = setting_rows(invocation)?;
                dialog.reload(rows);
                continue;
            }
        };
    }
    close_dialog(stdout, drawn, &changes, "")?;
    Ok(())
}

/// Every location `storage::inventory` measured, as the dialog draws it.
#[cfg(feature = "tui")]
fn storage_rows(entries: &[crate::storage::Entry]) -> Vec<tui::StorageRow> {
    entries
        .iter()
        .map(|entry| tui::StorageRow {
            label: entry.label.to_owned(),
            scope: entry.scope.to_owned(),
            path: tui::safe_text(&entry.path.display().to_string()),
            size: if entry.exists {
                crate::storage::human_bytes(entry.bytes)
            } else {
                "—".to_owned()
            },
            action: entry.action.map(|action| match action {
                crate::storage::Action::Clean(_) => "clean".to_owned(),
                crate::storage::Action::ResetHistory => "reset".to_owned(),
            }),
            resets_history: entry.action == Some(crate::storage::Action::ResetHistory),
        })
        .collect()
}

/// Drive `/storage`: list every location, run the cleanup a row offers, and
/// after a history reset carry on in a fresh session, since the one running
/// no longer has a store behind it.
#[cfg(feature = "tui")]
pub(crate) fn run_storage_dialog(
    invocation: &Invocation,
    mut restoring: Restoring<'_>,
    stdout: &mut io::Stdout,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
) -> Result<(), Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    // One scan feeds both the rows drawn and the actions dispatched by row, and
    // both are refreshed together after every action, so an index always
    // names the entry the operator saw.
    let mut entries = crate::storage::inventory(&root);
    let mut dialog = tui::StorageDialogState::new(
        storage_rows(&entries),
        crate::storage::workspace_name(&root),
    );
    let mut changes: Vec<String> = Vec::new();
    let mut drawn = 0;
    loop {
        drawn = repaint_dialog(
            stdout,
            colour,
            drawn,
            &dialog.render(tui::terminal_width(), colour),
        )?;
        let action = match next_dialog_key(&mut dialog, keys, decoder, |dialog: &mut _, key| {
            dialog.handle_key(key)
        }) {
            Keyed::Ended | Keyed::Acted(tui::StorageAction::Close) => break,
            Keyed::Redraw => continue,
            Keyed::Acted(action) => action,
        };
        let outcome = match action {
            tui::StorageAction::Clean(index) => match entries.get(index).and_then(|e| e.action) {
                Some(crate::storage::Action::Clean(target)) => {
                    crate::storage::clean(invocation, &root, target)
                }
                _ => Err("nothing to clean here".to_owned()),
            },
            tui::StorageAction::ResetHistory(_, typed) => {
                crate::storage::reset_history(&root, &typed).map(|message| {
                    let started = super::prompt::start_session(restoring.reborrow());
                    format!("{message} Started fresh session {started}.")
                })
            }
            tui::StorageAction::Close => break,
        };
        let notice = match outcome {
            Ok(message) => {
                changes.push(message.clone());
                message
            }
            Err(reason) => reason,
        };
        entries = crate::storage::inventory(&root);
        dialog.reload(storage_rows(&entries));
        dialog.notice = Some(tui::safe_text(&notice));
    }
    close_dialog(stdout, drawn, &changes, "")?;
    Ok(())
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_model_dialog(
    invocation: &Invocation,
    models: &[tui::ModelChoice],
    route: &mut tui::ModelRoute,
    effort: &mut Option<Effort>,
    state: &mut tui::TuiState,
    stdout: &mut io::Stdout,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    emitter: &mut Emitter,
) -> Result<(), Diagnostic> {
    let providers = super::session::configured_providers(invocation);
    let mut dialog = tui::ModelDialogState::new(providers, models.to_vec(), route.clone(), *effort);
    // Nine rows of frame around the lists: borders, headers, hints.
    dialog.list_rows = tui::terminal_rows().saturating_sub(9);
    let mut drawn = 0;
    loop {
        drawn = repaint_dialog(
            stdout,
            colour,
            drawn,
            &dialog.render(tui::terminal_width(), colour),
        )?;
        match next_dialog_key(&mut dialog, keys, decoder, |dialog: &mut _, key| {
            dialog.handle_key(key)
        }) {
            Keyed::Ended => break,
            Keyed::Redraw => continue,
            Keyed::Acted(tui::ModelDialogAction::Close) => break,
            Keyed::Acted(tui::ModelDialogAction::Apply {
                route: new_route,
                effort: new_effort,
            }) => {
                *route = new_route;
                super::remembered::remember_model(route, emitter);
                state.set_model_route(route.clone());

                // A model without the knob offers only `n/a`; the effort the
                // operator chose stays for the next model that takes one.
                let takes = super::remembered::route_effort(invocation, route).takes_effort();
                state.set_takes_effort(takes);
                if takes {
                    *effort = new_effort;
                    state.set_effort(*effort);
                    super::remembered::remember_effort(*effort, emitter);
                }

                // Nothing is printed: the footer names the route and the
                // effort, and a line for them would only interrupt the
                // conversation.
                break;
            }
        }
    }
    close_dialog(stdout, drawn, &[], "")?;
    Ok(())
}

/// Apply the two settings that change what is on the screen right now, and
/// answer with the line to show, or `None` for one that waits for a restart.
///
/// A theme that only edits a file has not been chosen: the operator is
/// looking at the palette when they pick it.
/// When a change to `key` made from `/settings` takes effect.
///
/// Held next to [`apply_live_setting`] so the two cannot disagree: what that
/// applies at once is `now`, what a turn reads from the configuration it
/// reloads is `next turn`, and the rest waits for ARSY to start again.
pub(crate) fn applies(key: &str) -> &'static str {
    match key {
        "theme.base" | "ui.style" | "ui.tool_output" => "now",
        "execution.max_tool_rounds" | "ui.session_title" => "next turn",
        key if key.starts_with("compat.") => "next turn",
        _ => "restart",
    }
}

#[cfg(feature = "tui")]
pub(crate) fn apply_live_setting(
    row: &tui::SettingRow,
    value: &str,
    theme: &mut String,
    roles: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    match row.key.as_str() {
        "theme.base" => {
            tui::set_palette(value, roles);
            *theme = value.to_owned();
            Some(format!("Theme: {value}"))
        }
        "ui.style" => {
            tui::set_render_style(match value {
                "classic" => tui::RenderStyle::Classic,
                _ => tui::RenderStyle::Modern,
            });
            Some(format!("Transcript style: {value}"))
        }
        "ui.tool_output" => {
            tui::set_tool_output(value);
            Some(format!("Tool output: {value}"))
        }
        _ => None,
    }
}

/// The configuration resolved for this workspace, for a dialog that needs to
/// know what the layers said before it writes.
#[cfg(feature = "tui")]
pub(crate) fn load_config_for(
    invocation: &Invocation,
) -> Result<arsy_kernel::config::Config, Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    load_config(&root, &working, invocation.config.as_deref())
}
/// Erase and redraw a dialog frame, returning the rows it now occupies.
#[cfg(feature = "tui")]
pub(crate) fn repaint_dialog(
    stdout: &mut io::Stdout,
    _colour: bool,
    drawn: usize,
    frame: &str,
) -> Result<usize, Diagnostic> {
    let drawn = drawn.min(tui::terminal_rows().saturating_sub(1));
    let up = if drawn > 0 {
        format!("\x1b[{drawn}A")
    } else {
        String::new()
    };
    write!(stdout, "{up}\r\x1b[J{frame}\n").map_err(terminal_failed)?;
    stdout.flush().map_err(terminal_failed)?;
    Ok(frame.lines().count())
}

/// Erase a dialog and report what it changed, or nothing when it did not.
#[cfg(feature = "tui")]
pub(crate) fn close_dialog(
    stdout: &mut io::Stdout,
    drawn: usize,
    changes: &[String],
    note: &str,
) -> Result<(), Diagnostic> {
    // A frame taller than the screen scrolled its top away; moving further up
    // than the screen would erase the conversation above it instead.
    let drawn = drawn.min(tui::terminal_rows().saturating_sub(1));
    if drawn > 0 {
        write!(stdout, "\x1b[{drawn}A").map_err(terminal_failed)?;
    }
    write!(stdout, "\r\x1b[J").map_err(terminal_failed)?;
    if !changes.is_empty() {
        let mut lines = changes.to_vec();
        lines.push(note.to_owned());
        writeln!(stdout, "{}", tui::safe_text(&lines.join("\n"))).map_err(terminal_failed)?;
    }
    stdout.flush().map_err(terminal_failed)?;
    Ok(())
}

/// What one key did to a dialog.
#[cfg(feature = "tui")]
pub(crate) enum Keyed<Action> {
    /// The dialog answered with an action.
    Acted(Action),
    /// The dialog took the key and changed its own state; redraw, keep going.
    Redraw,
    /// The keyboard hung up; nothing more will arrive.
    Ended,
}

/// Wait for the next key and let the dialog answer it.
///
/// `Redraw` and `Ended` are distinct on purpose. A key that only moves the
/// dialog's own marker — an arrow, `r` on the session list — is answered with
/// no action, and a driver that read that as "close" would shut the dialog on
/// the first arrow rather than redraw it.
#[cfg(feature = "tui")]
pub(crate) fn next_dialog_key<State, Action>(
    dialog: &mut State,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    handle: impl Fn(&mut State, tui::Key) -> Option<Action>,
) -> Keyed<Action> {
    loop {
        // A lone Escape is only known once nothing follows it.
        let key = match keys.recv_timeout(std::time::Duration::from_millis(40)) {
            Ok(byte) => decoder.feed(byte),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => decoder.flush_escape(),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Keyed::Ended,
        };
        if let Some(key) = key {
            return match handle(dialog, key) {
                Some(action) => Keyed::Acted(action),
                None => Keyed::Redraw,
            };
        }
    }
}

/// Drive the `/mcp` dialog until it is closed.
///
/// Each key redraws the frame over the last one rather than under it, and the
/// frame is erased on close, so moving through the list leaves nothing behind;
/// only a line per change made stays in the scrollback.
#[cfg(feature = "tui")]
pub(crate) fn run_mcp_dialog(
    invocation: &Invocation,
    stdout: &mut io::Stdout,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
) -> Result<(), Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    let mut rows = mcp_choices(&root, invocation)?;
    let mut dialog = tui::McpDialogState::new(rows.iter().map(|(_, c)| c.clone()).collect());
    let mut changed = 0usize;
    let mut drawn = 0;
    loop {
        let frame = dialog.render(tui::terminal_width(), colour);
        let up = if drawn > 0 {
            format!("\x1b[{drawn}A")
        } else {
            String::new()
        };
        write!(stdout, "{up}\r\x1b[J{frame}\n").map_err(terminal_failed)?;
        stdout.flush().map_err(terminal_failed)?;
        drawn = frame.lines().count();

        let action = match next_mcp_action(&mut dialog, keys, decoder) {
            None => continue,
            Some(tui::McpAction::Close) => {
                close_dialog(stdout, drawn, &mcp_close_line(changed), "")?;
                return Ok(());
            }
            Some(action) => action,
        };
        dialog.notice = Some(match apply_mcp_action(&root, &rows, &dialog, action) {
            Ok(change) => {
                changed += 1;
                change
            }
            Err(diagnostic) => format!("{}: {}", diagnostic.code, diagnostic.message),
        });
        rows = mcp_choices(&root, invocation)?;
        dialog.reload(rows.iter().map(|(_, c)| c.clone()).collect());
    }
}

/// Wait for the next key and let the dialog answer it. A keyboard that hung up
/// closes the dialog rather than holding the session on it.
#[cfg(feature = "tui")]
pub(crate) fn next_mcp_action(
    dialog: &mut tui::McpDialogState,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
) -> Option<tui::McpAction> {
    loop {
        // A lone Escape is only known once nothing follows it.
        let key = match keys.recv_timeout(std::time::Duration::from_millis(40)) {
            Ok(byte) => decoder.feed(byte),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => decoder.flush_escape(),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Some(tui::McpAction::Close)
            }
        };
        if let Some(key) = key {
            return dialog.handle_key(key);
        }
    }
}

/// Write the change a toggle or an adoption asks for, and say what it did.
#[cfg(feature = "tui")]
pub(crate) fn apply_mcp_action(
    root: &Path,
    rows: &[(Value, tui::McpChoice)],
    dialog: &tui::McpDialogState,
    action: tui::McpAction,
) -> Result<String, Diagnostic> {
    match action {
        tui::McpAction::Toggle(index) => {
            let choice = &dialog.choices[index];
            let enabled = !choice.enabled.unwrap_or(false);
            // Claude Code's and Codex's own files are never written: the
            // choice is kept in the operator's arsy.json, which outranks them.
            let declared_elsewhere = choice.source != "arsy";
            let scope = match choice.trust.as_str() {
                _ if declared_elsewhere => mcp::Scope::User,
                "user" => mcp::Scope::User,
                "workspace" => mcp::Scope::Workspace,
                other => {
                    return Err(usage(format!(
                        "`{}` is defined by the {other} configuration; change it there",
                        choice.name
                    )))
                }
            };
            mcp::set_enabled_in(root, &choice.name, enabled, scope, declared_elsewhere)?;
            let state = if enabled { "enabled" } else { "disabled" };
            Ok(format!("MCP `{}` {state}.", choice.name))
        }
        tui::McpAction::Adopt(index) => {
            let server = mcp::server_from_declaration(&rows[index].0)?;
            let written = mcp::add_in(root, &server, mcp::Scope::User)?;
            Ok(format!(
                "MCP `{}` adopted into {}, enabled.",
                server.name,
                written["path"].as_str().unwrap_or("arsy.json")
            ))
        }
        tui::McpAction::Close => Ok(String::new()),
    }
}

/// The one line the hook dialog prints on close, or nothing when it changed
/// nothing.
#[cfg(feature = "tui")]
fn hook_close_line(changed: usize) -> Vec<String> {
    (changed > 0)
        .then(|| {
            format!(
                "{changed} hook{} changed; they take effect from the next turn.",
                if changed == 1 { "" } else { "s" }
            )
        })
        .into_iter()
        .collect()
}

/// The one line the skill dialog prints on close, or nothing when it changed
/// nothing.
#[cfg(feature = "tui")]
fn skill_close_line(changed: usize) -> Vec<String> {
    (changed > 0)
        .then(|| {
            format!(
                "{changed} skill{} changed; they take effect from the next turn.",
                if changed == 1 { "" } else { "s" }
            )
        })
        .into_iter()
        .collect()
}

/// The one line the MCP dialog prints on close, or nothing when it changed
/// nothing.
#[cfg(feature = "tui")]
fn mcp_close_line(changed: usize) -> Vec<String> {
    (changed > 0)
        .then(|| {
            format!(
                "MCP: {changed} connection{} changed; they take effect from the next turn.",
                if changed == 1 { "" } else { "s" }
            )
        })
        .into_iter()
        .collect()
}
