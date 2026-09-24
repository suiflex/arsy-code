//! What the composer is currently collecting a line for, and the code that
//! answers it: the pickers, the slash commands, and the handoff to a turn.

#[cfg(feature = "tui")]
use super::dialog::{
    close_dialog, next_dialog_key, repaint_dialog, run_hook_dialog, run_mcp_dialog,
    run_model_dialog, run_settings_dialog, run_skill_dialog, Keyed,
};
#[cfg(feature = "tui")]
use super::remembered::{
    apply_theme, endpoint_models, remember_effort, remember_model, resolve_palette,
};
#[cfg(feature = "tui")]
use super::session::{
    configured_providers, load_workspace_sessions, reconstruct_session_conversation,
};
#[cfg(feature = "tui")]
use super::wizard::{
    auth_step, catalog_handles, configured_default, effort_line, provider_step, AuthNext,
    ProviderNext,
};
#[cfg(feature = "tui")]
use crate::turn::run_turn;
#[cfg(feature = "tui")]
use crate::turn::{confirm_plan, Pass};
#[cfg(feature = "tui")]
use crate::*;
#[cfg(feature = "tui")]
use arsy_kernel::provider::Effort;

/// The line an action carries, if it carries one.
#[cfg(feature = "tui")]
pub(crate) fn submitted(
    input: tui::Action,
    approval: &approval::ApprovalCell,
    state: &mut tui::TuiState,
    _transcript: &mut tui::Transcript,
    _terminal: &mut impl Write,
    _colour: bool,
) -> Option<String> {
    match input {
        tui::Action::Submit(line) => Some(line),
        // `e` is answered in the read loop, before it could ever reach here.
        tui::Action::Expand => None,
        tui::Action::CycleMode => {
            let was = approval.get().label();
            let mode = cycle_approval_mode(approval);
            state.set_approval_mode(mode.label());
            if was != mode.label() {
                // The launch card is the one place the mode is named in full,
                // so a change re-renders it rather than adding another row to
                // the scrollback: cycling through four modes is not four
                // pieces of history, it is one boundary the reader can see
                // wherever they happen to be looking.
                state.card_is_stale();
            }
            None
        }
        tui::Action::Quit | tui::Action::Redraw | tui::Action::None => None,
    }
}

/// Whether the answer being typed is a credential, which is shown as bullets,
/// never painted into the scrollback, and never remembered.
#[cfg(feature = "tui")]
pub(crate) fn masked(prompt: &Prompt) -> bool {
    matches!(prompt, Prompt::Provider(step) if step.masked())
        || matches!(prompt, Prompt::Auth(step) if step.masked())
}

/// Whether ending input here closes a picker rather than the session.
///
/// Every picker has to be named, or leaving one exits ARSY instead.
#[cfg(feature = "tui")]
pub(crate) fn cancels_to_task(prompt: &Prompt) -> bool {
    matches!(
        prompt,
        Prompt::Model
            | Prompt::Effort
            | Prompt::Theme
            | Prompt::Provider(_)
            | Prompt::Auth(_)
            | Prompt::Resume
    )
}

/// Answer the line at whichever prompt collected it.
///
/// Every picker clears the composer rather than committing it, so no answer —
/// least of all a credential — is painted into the scrollback.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn answer_prompt(
    prompt: Prompt,
    line: &str,
    invocation: &Invocation,
    typing: Typing<'_>,
    restoring: Restoring<'_>,
    stdout: &mut io::Stdout,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    emitter: &mut Emitter,
) -> Result<TaskPass, Diagnostic> {
    if matches!(prompt, Prompt::Task) {
        return answer_task(
            line, invocation, typing, restoring, stdout, keys, decoder, composer, emitter,
        );
    }
    write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
    let next = match prompt {
        Prompt::Provider(step) => take_provider(
            invocation,
            step,
            line,
            typing.draft,
            typing.providers,
            typing.chosen,
            typing.models,
            typing.route,
            stdout,
        )?,
        Prompt::Effort => take_effort(line, typing.effort, restoring.state, stdout, emitter)?,
        // On a rejected answer the list stays open so it can be retyped.
        Prompt::Theme => {
            if apply_theme(line, typing.theme, typing.roles, stdout, emitter)
                .map_err(terminal_failed)?
            {
                Prompt::Task
            } else {
                Prompt::Theme
            }
        }
        Prompt::Model => take_model(
            line,
            typing.models,
            typing.route,
            restoring.state,
            stdout,
            emitter,
        )?,
        Prompt::Auth(step) => take_auth(
            invocation,
            typing.workspace,
            step,
            line,
            typing.auth_draft,
            typing.providers,
            typing.route,
            typing.provider_available,
            typing.resolved_providers,
            typing.unavailable_providers,
            stdout,
            emitter,
        )?,
        Prompt::Resume => take_resume(line, typing.sessions, restoring, stdout)?,
        Prompt::Session(dialog) => {
            run_session_dialog(dialog, restoring, stdout, typing.colour, keys, decoder)?;
            Prompt::Task
        }
        Prompt::Task => Prompt::Task,
    };
    Ok(TaskPass::Ask(next))
}

/// Open the session the answer names, or leave the list open.
#[cfg(feature = "tui")]
pub(crate) fn take_resume(
    line: &str,
    sessions: &[tui::SessionChoice],
    restoring: Restoring<'_>,
    stdout: &mut io::Stdout,
) -> Result<Prompt, Diagnostic> {
    match tui::resolve_session_answer(line, sessions, restoring.state.session_id()) {
        Ok(picked) => {
            let loaded = resume_into(picked, restoring);
            writeln!(
                stdout,
                "Resumed session {picked} ({loaded} message(s) loaded)."
            )
            .map_err(terminal_failed)?;
            Ok(Prompt::Task)
        }
        Err(reason) => {
            writeln!(stdout, "{}", tui::safe_text(&reason)).map_err(terminal_failed)?;
            Ok(Prompt::Resume)
        }
    }
}

/// Where the session goes after a line typed at the task prompt.
/// Carry the provider wizard one step, and say which step comes next.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn take_provider(
    invocation: &Invocation,
    step: tui::ProviderStep,
    line: &str,
    draft: &mut tui::ProviderDraft,
    providers: &mut Vec<String>,
    chosen: &mut Option<String>,
    models: &mut Vec<tui::ModelChoice>,
    route: &mut tui::ModelRoute,
    stdout: &mut io::Stdout,
) -> Result<Prompt, Diagnostic> {
    let message = match provider_step(step, line, draft, providers) {
        Ok(ProviderNext::Ask(next)) => return Ok(Prompt::Provider(next)),
        Ok(ProviderNext::Done(message)) => {
            writeln!(stdout, "{}", tui::safe_text(&message)).map_err(terminal_failed)?;
            *providers = configured_providers(invocation);
            *chosen = configured_default(invocation);
            let removed = provider_step_removed(&message);
            let added: Option<String> = provider_step_added(&message).map(str::to_owned);
            *draft = tui::ProviderDraft::default();
            // A removed endpoint's models must not stay in the picker, and a
            // route that named it can no longer be driven by this session.
            if let Some(name) = &removed {
                models.retain(|choice| &choice.provider != name);
                if route.provider == *name {
                    *route = tui::ModelRoute {
                        provider: String::new(),
                        model: String::new(),
                    };
                    return Ok(Prompt::Task);
                }
            }
            // Auto-fetch the live model list for a newly added key provider.
            if let Some(name) = added {
                if let Ok(count) = fetch_and_store_models(invocation, &name) {
                    let _ = writeln!(stdout, "Fetched {count} model(s) for `{name}`.");
                }
            }
            // Configuration decides the provider, so the session has to be
            // restarted to pick up a change to it rather than pretend the
            // running one moved.
            "Restart ARSY for the change to take effect.".to_owned()
        }
        Ok(ProviderNext::Cancelled(message)) => {
            *draft = tui::ProviderDraft::default();
            message
        }
        // The step stays open so the answer can be retyped against the
        // question that is still on screen.
        Err(reason) => {
            writeln!(stdout, "{}", tui::safe_text(&reason)).map_err(terminal_failed)?;
            return Ok(Prompt::Provider(step));
        }
    };
    writeln!(stdout, "{}", tui::safe_text(&message)).map_err(terminal_failed)?;
    if let Some(name) = provider_step_removed(&message) {
        writeln!(
            stdout,
            "The session was on {name}; `/model` picks another endpoint."
        )
        .map_err(terminal_failed)?;
    }
    Ok(Prompt::Task)
}

/// The provider name a removal message reports, or nothing for any other
/// step's outcome.
#[cfg(feature = "tui")]
pub(crate) fn provider_step_removed(message: &str) -> Option<String> {
    message
        .strip_prefix("Removed provider ")
        .and_then(|rest| rest.strip_suffix(" and its credentials."))
        .map(str::to_owned)
}

/// The provider name a new-provider message reports, or nothing for any other.
#[cfg(feature = "tui")]
fn provider_step_added(message: &str) -> Option<&str> {
    message
        .strip_prefix("Added provider ")
        .and_then(|rest| rest.split_once(' ').map(|(name, _)| name))
}

/// Take the reasoning effort the operator picked.
///
/// A rejected answer leaves the list open so it can be retyped against what is
/// already on screen.
#[cfg(feature = "tui")]
pub(crate) fn take_effort(
    line: &str,
    effort: &mut Option<Effort>,
    state: &mut tui::TuiState,
    stdout: &mut io::Stdout,
    emitter: &mut Emitter,
) -> Result<Prompt, Diagnostic> {
    match tui::resolve_effort_answer(line, *effort) {
        Ok(picked) => {
            *effort = picked;
            state.set_effort(*effort);
            remember_effort(*effort, emitter);
            writeln!(stdout, "{}", effort_line(*effort)).map_err(terminal_failed)?;
            Ok(Prompt::Task)
        }
        Err(reason) => {
            writeln!(stdout, "{}", tui::safe_text(&reason)).map_err(terminal_failed)?;
            Ok(Prompt::Effort)
        }
    }
}

/// Take the model the operator picked, and remember it for the next run.
#[cfg(feature = "tui")]
pub(crate) fn take_model(
    line: &str,
    models: &[tui::ModelChoice],
    route: &mut tui::ModelRoute,
    state: &mut tui::TuiState,
    stdout: &mut io::Stdout,
    emitter: &mut Emitter,
) -> Result<Prompt, Diagnostic> {
    match tui::resolve_model(line, models, route) {
        Ok(picked) => {
            *route = picked;
            remember_model(route, emitter);
            state.set_model_route(route.clone());
            writeln!(stdout, "Model: {route}").map_err(terminal_failed)?;
            Ok(Prompt::Task)
        }
        Err(reason) => {
            writeln!(stdout, "{}", tui::safe_text(&reason)).map_err(terminal_failed)?;
            Ok(Prompt::Model)
        }
    }
}

/// Carry the credential wizard one step, and say which step comes next.
///
/// A finished login or stored key is used now rather than after a restart:
/// the credential is on disk, so the same resolution the session opened with
/// can read it, and the route moves to the provider that was just authorised.
/// Nothing else about the configuration is claimed to have reloaded.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn take_auth(
    invocation: &Invocation,
    workspace: &Path,
    step: tui::AuthStep,
    line: &str,
    draft: &mut String,
    providers: &mut Vec<String>,
    route: &mut tui::ModelRoute,
    provider_available: &mut bool,
    resolved: &mut std::collections::HashMap<String, provider::Resolved>,
    unavailable: &mut std::collections::HashSet<String>,
    stdout: &mut io::Stdout,
    emitter: &mut Emitter,
) -> Result<Prompt, Diagnostic> {
    // The provider whose credential was just written: the login step names it
    // in the answer, the key step names it in the draft it is about to clear.
    let authorised = match step {
        tui::AuthStep::LoginProvider => line.trim().to_owned(),
        tui::AuthStep::SetKey => draft.clone(),
        // draft holds "{provider}\n{verifier}" at this point; extract the name
        // so the auto-fetch and activate blocks can run after the code is accepted.
        tui::AuthStep::PasteCode => draft
            .split_once('\n')
            .map(|(p, _)| p.to_owned())
            .unwrap_or_default(),
        _ => String::new(),
    };
    let (mut message, next, finished) =
        match auth_step(invocation, step, line, draft, providers, emitter) {
            Ok(AuthNext::Ask(next)) => return Ok(Prompt::Auth(next)),
            // Finished or abandoned, the draft goes either way: a credential
            // is never left in memory for the next question to pick up.
            Ok(done @ AuthNext::Done(_)) => {
                draft.clear();
                let done_message = match done {
                    AuthNext::Done(done_message) => done_message,
                    _ => String::new(),
                };
                (done_message, Prompt::Task, true)
            }
            Ok(AuthNext::Cancelled(message)) => {
                draft.clear();
                (message, Prompt::Task, false)
            }
            Err(reason) => (reason, Prompt::Auth(step), false),
        };

    if finished {
        finish_auth(
            invocation,
            workspace,
            &authorised,
            providers,
            route,
            provider_available,
            resolved,
            unavailable,
            &mut message,
        );
    }
    writeln!(stdout, "{}", tui::safe_text(&message)).map_err(terminal_failed)?;
    Ok(next)
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn finish_auth(
    invocation: &Invocation,
    workspace: &Path,
    authorised: &str,
    providers: &mut Vec<String>,
    route: &mut tui::ModelRoute,
    provider_available: &mut bool,
    resolved: &mut std::collections::HashMap<String, provider::Resolved>,
    unavailable: &mut std::collections::HashSet<String>,
    message: &mut String,
) {
    if authorised.is_empty() {
        return;
    }
    if !providers.iter().any(|name| name == authorised) {
        *providers = configured_providers(invocation);
    }
    if activate_signed_in_provider(
        invocation,
        workspace,
        authorised,
        route,
        resolved,
        unavailable,
    ) {
        *message = format!("Signed in to `{authorised}`; you can use it now.");
        *provider_available = true;
    }
    if arsy_kernel::oauth::presets::get(authorised).is_some() {
        if let Err(reason) = fetch_and_store_oauth_models(authorised) {
            *message = format!("{message} Model discovery failed: {reason}");
        }
    }
}

/// Point the running session at a provider whose credential was just stored.
///
/// The credential is on disk, so the same resolution the session opened with
/// can read it now. A provider the session could not resolve earlier — the
/// usual reason it was told to restart — is retried, because the missing
/// credential is exactly what the login just wrote. Only the route moves:
/// no claim is made that the whole configuration reloaded.
#[cfg(feature = "tui")]
pub(crate) fn activate_signed_in_provider(
    invocation: &Invocation,
    workspace: &Path,
    provider: &str,
    route: &mut tui::ModelRoute,
    resolved: &mut std::collections::HashMap<String, provider::Resolved>,
    unavailable: &mut std::collections::HashSet<String>,
) -> bool {
    unavailable.remove(provider);
    let Some(found) = resolve_route(invocation, workspace, provider, resolved, unavailable) else {
        return false;
    };
    let Ok(root) = workspace_root(&invocation.workspace) else {
        return false;
    };
    let working = std::env::current_dir().unwrap_or_else(|_| workspace.to_path_buf());
    let model = load_config(&root, &working, invocation.config.as_deref())
        .ok()
        .and_then(|config| selected_model(&config, &found.endpoint, None).ok());
    let Some(model) = model else {
        return false;
    };
    *route = tui::ModelRoute {
        provider: provider.to_owned(),
        model,
    };
    true
}

/// What the pickers read to draw themselves.
#[cfg(feature = "tui")]
pub(crate) struct Picker<'a> {
    pub(crate) state: &'a tui::TuiState,
    pub(crate) workspace: &'a Path,
    pub(crate) models: &'a [tui::ModelChoice],
    pub(crate) route: &'a tui::ModelRoute,
    pub(crate) effort: Option<Effort>,
    pub(crate) theme: &'a str,
    pub(crate) draft: &'a tui::ProviderDraft,
    pub(crate) auth_draft: &'a str,
    pub(crate) sessions: &'a [tui::SessionChoice],
    pub(crate) providers: &'a [String],
    pub(crate) chosen_provider: Option<&'a str>,
}

/// The line under the composer: the status row, or whatever the open picker
/// wants said above its rows.
#[cfg(feature = "tui")]
pub(crate) fn prompt_status(prompt: &Prompt, picker: Picker<'_>, colour: bool) -> String {
    match prompt {
        // The branch is read per line rather than kept, so a checkout made in
        // another terminal shows up on the next prompt.
        Prompt::Task => picker.state.status_row(
            tui::terminal_width(),
            colour,
            tui::branch(picker.workspace).as_deref(),
        ),
        Prompt::Model => tui::model_prompt(picker.models, picker.route, colour),
        Prompt::Effort => tui::effort_prompt(picker.effort, colour),
        Prompt::Theme => tui::theme_prompt(picker.theme, colour),
        Prompt::Provider(step) => step.prompt(picker.draft, colour),
        Prompt::Auth(step) => step.prompt(picker.auth_draft, colour),
        Prompt::Resume => tui::session_prompt(picker.sessions, colour),
        Prompt::Session(dialog) => dialog.render(tui::terminal_width(), colour),
    }
}

/// The rows the open picker offers, and which of them is marked.
#[cfg(feature = "tui")]
pub(crate) fn offer_rows(prompt: &Prompt, composer: &mut tui::Composer, picker: Picker<'_>) {
    match prompt {
        Prompt::Model => {
            let (rows, selected) = tui::model_rows(picker.models, picker.route);
            composer.offer(rows, selected);
        }
        Prompt::Effort => {
            composer.offer_table(Some(tui::EFFORT_ROWS), tui::effort_row(picker.effort))
        }
        Prompt::Theme => composer.offer_table(Some(tui::THEMES), tui::theme_row(picker.theme)),
        Prompt::Provider(step) => composer.offer(
            step.rows(
                picker.providers,
                &picker.route.provider,
                picker.chosen_provider,
            ),
            0,
        ),
        Prompt::Auth(step) => {
            let handles = catalog_handles();
            composer.offer(step.rows(picker.providers, &handles), 0);
        }
        Prompt::Resume => {
            let (rows, selected) =
                tui::session_rows(picker.sessions, Some(picker.state.session_id()));
            composer.offer(rows, selected);
        }
        _ => composer.offer(None, 0),
    }
}

/// Drive the session dialog until it is answered or left.
///
/// The dialog owns the keyboard while it is open: it is a list in front of the
/// reader, and every key belongs to it until it closes.
#[cfg(feature = "tui")]
pub(crate) fn run_session_dialog(
    mut dialog: tui::SessionDialogState,
    restoring: Restoring<'_>,
    stdout: &mut io::Stdout,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
) -> Result<(), Diagnostic> {
    let mut changes: Vec<String> = Vec::new();
    let mut drawn = 0;
    loop {
        drawn = repaint_dialog(
            stdout,
            colour,
            drawn,
            &dialog.render(tui::terminal_width(), colour),
        )?;
        // A keyboard that hung up leaves the dialog, rather than holding the
        // session on a list nothing can answer. A lone Escape reaches here as
        // an interrupt only because the key loop flushes it once nothing
        // follows.
        let action = match next_dialog_key(&mut dialog, keys, decoder, |dialog: &mut _, key| {
            dialog.handle_key(key)
        }) {
            Keyed::Ended => break,
            // A key that only moved the marker or opened a mode has already
            // changed the dialog; the loop's top redraws it.
            Keyed::Redraw => continue,
            Keyed::Acted(action) => action,
        };
        let borrowed = Restoring {
            workspace: restoring.workspace,
            state: restoring.state,
            conversation: restoring.conversation,
            transcript: restoring.transcript,
            history: restoring.history,
            approval: restoring.approval,
            queued: restoring.queued,
        };
        match action {
            tui::SessionAction::Resume(id) => {
                close_dialog(stdout, drawn, &[], "")?;
                let loaded = resume_into(id, borrowed);
                writeln!(stdout, "Resumed session {id} ({loaded} message(s) loaded).")
                    .map_err(terminal_failed)?;
                return Ok(());
            }
            tui::SessionAction::Rename(id, title) => {
                // A write that failed is reported as failed, not as a rename:
                // an operator who cannot see the difference cannot trust
                // either answer.
                match open_store(borrowed.workspace)
                    .and_then(|store| store.set_session_title(id, &title).map_err(storage_failed))
                {
                    Ok(()) => changes.push(format!("Renamed session {id} to \"{title}\".")),
                    Err(error) => {
                        changes.push(format!("`{title}` was not written: {}", error.message))
                    }
                }
                // Renaming may leave the dialog in its rename mode: the state
                // below resets it, and a redrawn frame is what shows the
                // result rather than the mode the key was pressed in.
                close_dialog(stdout, drawn, &[], "")?;
                drawn = 0;
            }
            tui::SessionAction::Delete(id) => {
                close_dialog(stdout, drawn, &[], "")?;
                drawn = 0;
                delete_session(Some(&id.to_string()), borrowed, stdout)?;
            }
            tui::SessionAction::Cancel => {
                close_dialog(stdout, drawn, &changes, "")?;
                return Ok(());
            }
        }
        // The list has changed under the marker: back to it, on the row the
        // action left standing.
        dialog.mode = tui::SessionDialogMode::Select;
        dialog.rename_buffer.clear();
        dialog.reload(load_workspace_sessions(restoring.workspace));
    }
    close_dialog(stdout, drawn, &changes, "")?;
    Ok(())
}

/// Drive the unified `/provider` dialog: access method, then a provider, then
/// what to do with it. `/auth` is an alias for the same dialog.
///
/// The dialog is pure navigation; anything that collects text — a key, a URL,
/// an OAuth code — is handed back as the `Prompt` the composer then answers,
/// so every validation, masking, and storage rule stays in the wizard it
/// already lived in. A pure write (make default) is done here and the dialog
/// closes.
/// The configured endpoints, each sorted into the one access bucket it belongs
/// to, so the dialog's access column filters rather than repeats.
#[cfg(feature = "tui")]
pub(crate) fn provider_dialog_endpoints(invocation: &Invocation) -> Vec<tui::ConfiguredEndpoint> {
    crate::provider::configuration(invocation)
        .map(|config| {
            config
                .endpoints()
                .map(|endpoint| {
                    let has_oauth = endpoint.oauth.is_some();
                    tui::ConfiguredEndpoint {
                        access: tui::classify(&endpoint.id, &endpoint.base_url, has_oauth),
                        oauth: has_oauth
                            || arsy_kernel::oauth::presets::get(&endpoint.id).is_some(),
                        id: endpoint.id.clone(),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What a chosen dialog action asks the loop to do next, decided without any
/// terminal I/O so the loop keeps sole ownership of drawing and closing.
#[cfg(feature = "tui")]
enum ProviderFlow {
    /// Close the dialog, print these lines, and return to the task prompt.
    Close(Vec<String>),
    /// Close the dialog and hand this prompt to the composer for its text.
    Handoff(Prompt),
    /// Keep the dialog open, showing this notice.
    Notice(String),
}

/// Apply one dialog action's side effects (a config write, a login, a prefilled
/// draft) and say what the loop should do. Anything that still needs typed text
/// becomes a `Handoff` to the wizard the composer already drives.
#[cfg(feature = "tui")]
fn provider_action_flow(
    action: tui::ProviderDialogAction,
    invocation: &Invocation,
    typing: &mut Typing<'_>,
    emitter: &mut Emitter,
) -> ProviderFlow {
    match action {
        tui::ProviderDialogAction::Close => ProviderFlow::Close(Vec::new()),
        tui::ProviderDialogAction::SetDefault(name) => {
            match super::wizard::write_config(|config| config_edit::set_default(config, &name)) {
                Ok(()) => {
                    ProviderFlow::Close(vec![format!("Provider: {name} (from the next session).")])
                }
                Err(reason) => ProviderFlow::Notice(reason),
            }
        }
        tui::ProviderDialogAction::Remove(name) => {
            *typing.draft = tui::ProviderDraft {
                name,
                ..Default::default()
            };
            ProviderFlow::Handoff(Prompt::Provider(tui::ProviderStep::ConfirmRemove))
        }
        tui::ProviderDialogAction::SetKey(name) => {
            *typing.auth_draft = name;
            ProviderFlow::Handoff(Prompt::Auth(tui::AuthStep::SetKey))
        }
        tui::ProviderDialogAction::AddPreset(row) => {
            *typing.draft = tui::ProviderDraft {
                name: row.id,
                kind: row.kind,
                base_url: row.base_url,
                models: row.models,
                store: "file".to_owned(),
            };
            ProviderFlow::Handoff(Prompt::Provider(tui::ProviderStep::Key))
        }
        tui::ProviderDialogAction::NewCustom => {
            *typing.draft = tui::ProviderDraft::default();
            ProviderFlow::Handoff(Prompt::Provider(tui::ProviderStep::Name))
        }
        tui::ProviderDialogAction::FetchModels(id) => fetch_models_flow(invocation, id),
        tui::ProviderDialogAction::Login(id) => {
            provider_login_flow(invocation, &id, typing, emitter)
        }
    }
}

#[cfg(feature = "tui")]
fn provider_login_flow(
    invocation: &Invocation,
    id: &str,
    typing: &mut Typing<'_>,
    emitter: &mut Emitter,
) -> ProviderFlow {
    match auth_step(
        invocation,
        tui::AuthStep::LoginProvider,
        id,
        typing.auth_draft,
        typing.providers,
        emitter,
    ) {
        Ok(AuthNext::Ask(next)) => ProviderFlow::Handoff(Prompt::Auth(next)),
        Ok(AuthNext::Done(message)) => match fetch_and_store_oauth_models(id) {
            Ok(count) => ProviderFlow::Close(vec![format!("{message} Fetched {count} model(s).")]),
            Err(reason) => {
                ProviderFlow::Close(vec![format!("{message} Model discovery failed: {reason}")])
            }
        },
        Ok(AuthNext::Cancelled(message)) => ProviderFlow::Close(vec![message]),
        Err(reason) => ProviderFlow::Notice(reason),
    }
}

/// Shared core: fetch the live model list for `id` from its endpoint and write
/// it back to the config. Returns the count on success, the error message on
/// failure. Used by both the manual "Fetch model list" action and the
/// post-add auto-fetch in `take_provider`.
#[cfg(feature = "tui")]
fn fetch_and_store_models(invocation: &Invocation, id: &str) -> Result<usize, String> {
    let (name, models) = provider::fetch_endpoint_models(invocation, id)?;
    let count = models.len();
    store_endpoint_models(&name, &models)?;
    Ok(count)
}

#[cfg(feature = "tui")]
fn store_endpoint_models(name: &str, models: &[String]) -> Result<(), String> {
    let models = serde_json::Value::Array(
        models
            .iter()
            .cloned()
            .map(serde_json::Value::String)
            .collect(),
    );
    super::wizard::write_config(|config| {
        crate::config_edit::set_existing(config, &["provider", "endpoint", name], "models", models)?
            .ok_or_else(|| format!("endpoint `{name}` not found in config file"))
    })
}

#[cfg(feature = "tui")]
fn fetch_and_store_oauth_models(id: &str) -> Result<usize, String> {
    let preset = arsy_kernel::oauth::presets::get(id)
        .ok_or_else(|| format!("`{id}` is not an OAuth preset"))?;
    let models = provider::fetch_oauth_preset_models(preset)
        .ok_or_else(|| format!("could not fetch models for `{id}`"))?;
    let count = models.len();
    store_endpoint_models(id, &models)?;
    Ok(count)
}

#[cfg(feature = "tui")]
fn fetch_models_flow(invocation: &Invocation, id: String) -> ProviderFlow {
    match fetch_and_store_models(invocation, &id) {
        Ok(count) => ProviderFlow::Notice(format!("Fetched {count} model(s) for `{id}`")),
        Err(reason) => ProviderFlow::Notice(reason),
    }
}

#[cfg(feature = "tui")]
pub(crate) fn run_provider_dialog(
    invocation: &Invocation,
    typing: &mut Typing<'_>,
    stdout: &mut io::Stdout,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    emitter: &mut Emitter,
) -> Result<Option<Prompt>, Diagnostic> {
    *typing.providers = configured_providers(invocation);
    let mut dialog = tui::ProviderDialogState::new(
        provider_dialog_endpoints(invocation),
        configured_default(invocation),
    );
    let mut drawn = 0;
    loop {
        drawn = repaint_dialog(
            stdout,
            typing.colour,
            drawn,
            &dialog.render(tui::terminal_width(), typing.colour),
        )?;
        let action = match next_dialog_key(&mut dialog, keys, decoder, |dialog: &mut _, key| {
            dialog.handle_key(key)
        }) {
            Keyed::Ended => break,
            Keyed::Redraw => continue,
            Keyed::Acted(action) => action,
        };
        if let tui::ProviderDialogAction::FetchModels(ref id) = action {
            dialog.notice = Some(format!("Fetching models for `{id}`..."));
            drawn = repaint_dialog(
                stdout,
                typing.colour,
                drawn,
                &dialog.render(tui::terminal_width(), typing.colour),
            )?;
        }
        match provider_action_flow(action, invocation, typing, emitter) {
            ProviderFlow::Notice(reason) => {
                dialog.notice = Some(reason);
                dialog.endpoints = provider_dialog_endpoints(invocation);
                drawn = 0;
            }
            ProviderFlow::Close(lines) => {
                close_dialog(stdout, drawn, &lines, "")?;
                return Ok(None);
            }
            ProviderFlow::Handoff(prompt) => {
                close_dialog(stdout, drawn, &[], "")?;
                return Ok(Some(prompt));
            }
        }
    }
    close_dialog(stdout, drawn, &[], "")?;
    Ok(None)
}

/// Open the dialog a bare slash command named.
///
/// Every one of these writes the operator's own `arsy.json`, so a bare
/// command reaches here and a command with an argument stays the read-only
/// inspection it always was.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_dialog(
    dialog: Dialog,
    invocation: &Invocation,
    typing: &mut Typing<'_>,
    restoring: Restoring<'_>,
    stdout: &mut io::Stdout,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    emitter: &mut Emitter,
) -> Result<(), Diagnostic> {
    match dialog {
        Dialog::Mcp => run_mcp_dialog(invocation, stdout, typing.colour, keys, decoder),
        Dialog::Hooks => run_hook_dialog(invocation, stdout, typing.colour, keys, decoder),
        Dialog::Skill => run_skill_dialog(invocation, stdout, typing.colour, keys, decoder),
        Dialog::Session => run_session_dialog(
            tui::SessionDialogState::new(
                load_workspace_sessions(restoring.workspace),
                restoring.state.session_id(),
            ),
            restoring,
            stdout,
            typing.colour,
            keys,
            decoder,
        ),
        Dialog::Settings => run_settings_dialog(
            invocation,
            stdout,
            typing.colour,
            keys,
            decoder,
            typing.theme,
            typing.roles,
        ),
        Dialog::Model => {
            *typing.models = endpoint_models(invocation);
            run_model_dialog(
                invocation,
                typing.models,
                typing.route,
                typing.effort,
                restoring.state,
                stdout,
                typing.colour,
                keys,
                decoder,
                emitter,
            )
        }
    }
}

/// Every MCP connection ARSY defines, then every one another tool declares
/// The slash commands that act on the recorded session rather than on the
/// conversation with the model.
#[cfg(feature = "tui")]
pub(crate) fn manages_session(line: &str) -> bool {
    matches!(
        line.split_whitespace().next(),
        Some("/new" | "/clear" | "/resume" | "/update" | "/rename" | "/session")
    )
}

/// Start, clear, open, rename or delete a session.
///
/// `Some` is the dialog or picker the command opened; `None` means it was
/// answered on the line and the task prompt stays.
#[cfg(feature = "tui")]
pub(crate) fn manage_session(
    line: &str,
    restoring: Restoring<'_>,
    sessions: &mut Vec<tui::SessionChoice>,
    stdout: &mut io::Stdout,
) -> Result<Option<Prompt>, Diagnostic> {
    let mut words = line.split_whitespace();
    match words.next() {
        Some("/new") => {
            let started = start_session(restoring);
            writeln!(stdout, "Started new session {started}.").map_err(terminal_failed)?;
            Ok(None)
        }
        // The session is kept; only what the model is told about it is
        // dropped, so the recording stays whole.
        Some("/clear") => {
            let session = restoring.state.session_id();
            restoring.conversation.clear();
            *restoring.history = arsy_code::agent::budget::History::default();
            restoring.queued.clear();
            writeln!(
                stdout,
                "Cleared conversation context for session {session}."
            )
            .map_err(terminal_failed)?;
            Ok(None)
        }
        Some("/resume") => resume_command(words.next(), restoring, sessions, stdout),
        Some("/update") => {
            writeln!(
                stdout,
                "arsy-code v{} is up to date.",
                env!("CARGO_PKG_VERSION")
            )
            .map_err(terminal_failed)?;
            Ok(None)
        }
        Some("/rename") => {
            let title = line.trim_start_matches("/rename").trim();
            rename_session(title, "/rename <TITLE>", restoring, stdout)?;
            Ok(None)
        }
        Some("/session") => session_command(words, restoring, sessions, stdout),
        _ => Ok(None),
    }
}

/// Open a session by id, or the list of them when none was named.
#[cfg(feature = "tui")]
pub(crate) fn resume_command(
    id: Option<&str>,
    restoring: Restoring<'_>,
    sessions: &mut Vec<tui::SessionChoice>,
    stdout: &mut io::Stdout,
) -> Result<Option<Prompt>, Diagnostic> {
    let Some(id) = id else {
        *sessions = load_workspace_sessions(restoring.workspace);
        return Ok(Some(Prompt::Resume));
    };
    let Ok(parsed) = id.parse::<SessionId>() else {
        writeln!(stdout, "Invalid session ID `{id}`.").map_err(terminal_failed)?;
        return Ok(None);
    };
    let loaded = resume_into(parsed, restoring);
    writeln!(
        stdout,
        "Resumed session {parsed} ({loaded} message(s) loaded)."
    )
    .map_err(terminal_failed)?;
    Ok(None)
}

/// `/session`, with or without a word after it.
#[cfg(feature = "tui")]
fn session_command<'a>(
    mut words: impl Iterator<Item = &'a str>,
    restoring: Restoring<'_>,
    sessions: &mut Vec<tui::SessionChoice>,
    stdout: &mut io::Stdout,
) -> Result<Option<Prompt>, Diagnostic> {
    match words.next() {
        None => Ok(Some(Prompt::Session(tui::SessionDialogState::new(
            load_workspace_sessions(restoring.workspace),
            restoring.state.session_id(),
        )))),
        Some("list") => {
            *sessions = load_workspace_sessions(restoring.workspace);
            Ok(Some(Prompt::Resume))
        }
        Some("rename") => {
            let title = words.collect::<Vec<_>>().join(" ");
            rename_session(&title, "/session rename <TITLE>", restoring, stdout)?;
            Ok(None)
        }
        Some("delete" | "rm" | "remove") => {
            delete_session(words.next(), restoring, stdout)?;
            Ok(None)
        }
        _ => {
            writeln!(
                stdout,
                "Usage: /session [list | rename <TITLE> | delete [ID]]"
            )
            .map_err(terminal_failed)?;
            Ok(None)
        }
    }
}

/// Give the current session a title, or say how to.
#[cfg(feature = "tui")]
pub(crate) fn rename_session(
    title: &str,
    usage: &str,
    restoring: Restoring<'_>,
    stdout: &mut io::Stdout,
) -> Result<(), Diagnostic> {
    if title.is_empty() {
        return writeln!(stdout, "Usage: {usage}").map_err(terminal_failed);
    }
    let session = restoring.state.session_id();
    if let Ok(store) = open_store(restoring.workspace) {
        let _ = store.set_session_title(session, title);
    }
    writeln!(stdout, "Renamed session {session} to \"{title}\".").map_err(terminal_failed)
}

/// Delete a session, starting a fresh one when it was the open one.
#[cfg(feature = "tui")]
pub(crate) fn delete_session(
    id: Option<&str>,
    restoring: Restoring<'_>,
    stdout: &mut io::Stdout,
) -> Result<(), Diagnostic> {
    let current = restoring.state.session_id();
    let target = id
        .and_then(|id| id.parse::<SessionId>().ok())
        .unwrap_or(current);
    if let Ok(store) = open_store(restoring.workspace) {
        let _ = store.delete_session(target);
    }
    if target != current {
        return writeln!(stdout, "Deleted session {target}.").map_err(terminal_failed);
    }
    let started = start_session(restoring);
    writeln!(
        stdout,
        "Deleted current session. Started fresh session {started}."
    )
    .map_err(terminal_failed)
}

/// Begin a session with nothing carried over from the one before it.
#[cfg(feature = "tui")]
pub(crate) fn start_session(restoring: Restoring<'_>) -> SessionId {
    let started = SessionId::new();
    restoring.state.set_session_id(started);
    restoring.conversation.clear();
    restoring.transcript.clear();
    *restoring.history = arsy_code::agent::budget::History::default();
    restoring.approval.clear_rules();
    set_approval_mode(
        restoring.approval,
        restoring.state,
        approval::ApprovalMode::Default,
    );
    restoring.queued.clear();
    started
}

/// The slash commands that choose a setting, by opening its list or by naming
/// the answer on the same line.
#[cfg(feature = "tui")]
pub(crate) fn opens_picker(line: &str) -> bool {
    matches!(
        line.split_whitespace().next(),
        Some("/auth" | "/model" | "/provider" | "/effort" | "/theme")
    )
}
/// What opening a picker re-reads or resets.
#[cfg(feature = "tui")]
pub(crate) struct Opening<'a> {
    pub(crate) models: &'a mut Vec<tui::ModelChoice>,
    pub(crate) providers: &'a mut Vec<String>,
    pub(crate) chosen: &'a mut Option<String>,
    pub(crate) draft: &'a mut tui::ProviderDraft,
    pub(crate) auth_draft: &'a mut String,
    pub(crate) effort: &'a mut Option<Effort>,
    pub(crate) theme: &'a mut String,
    pub(crate) roles: &'a std::collections::BTreeMap<String, String>,
    pub(crate) state: &'a mut tui::TuiState,
    pub(crate) route: &'a mut tui::ModelRoute,
}

/// Open the picker a slash command names, or take the answer it carried.
///
/// `Some` is the prompt that now collects the answer; `None` means the line
/// answered outright and the task prompt stays.
#[cfg(feature = "tui")]
pub(crate) fn open_picker(
    line: &str,
    invocation: &Invocation,
    opening: Opening<'_>,
    stdout: &mut io::Stdout,
    emitter: &mut Emitter,
) -> Result<Option<Prompt>, Diagnostic> {
    let mut words = line.split_whitespace();
    let (command, answer) = (words.next(), words.next());
    match command {
        Some("/auth") => {
            *opening.providers = configured_providers(invocation);
            opening.auth_draft.clear();
            Ok(Some(Prompt::Auth(tui::AuthStep::Pick)))
        }
        Some("/model") => {
            let Some(answer) = answer else {
                return Ok(None);
            };
            let models = endpoint_models(invocation);
            *opening.models = models;
            let next = take_model(
                answer,
                opening.models,
                opening.route,
                opening.state,
                stdout,
                emitter,
            )?;
            match next {
                Prompt::Task => Ok(None),
                other => Ok(Some(other)),
            }
        }
        Some("/provider") => {
            *opening.providers = configured_providers(invocation);
            *opening.chosen = configured_default(invocation);
            *opening.draft = tui::ProviderDraft::default();
            Ok(Some(Prompt::Provider(tui::ProviderStep::Pick)))
        }
        // A bare `/effort` opens the list, so the levels can be read before
        // one is chosen; `/effort high` still sets it outright.
        Some("/effort") => match answer {
            None => Ok(Some(Prompt::Effort)),
            Some(answer) => {
                take_effort(answer, opening.effort, opening.state, stdout, emitter)?;
                Ok(None)
            }
        },
        // A bare `/theme` opens the list; `/theme light` sets it outright.
        Some("/theme") => match answer {
            None => Ok(Some(Prompt::Theme)),
            Some(answer) => {
                apply_theme(answer, opening.theme, opening.roles, stdout, emitter)
                    .map_err(terminal_failed)?;
                Ok(None)
            }
        },
        _ => Ok(None),
    }
}

/// What a session being opened replaces.
#[cfg(feature = "tui")]
pub(crate) struct Restoring<'a> {
    pub(crate) workspace: &'a Path,
    pub(crate) state: &'a mut tui::TuiState,
    pub(crate) conversation: &'a mut Vec<ModelMessage>,
    pub(crate) transcript: &'a mut tui::Transcript,
    pub(crate) history: &'a mut arsy_code::agent::budget::History,
    pub(crate) approval: &'a approval::ApprovalCell,
    pub(crate) queued: &'a mut std::collections::VecDeque<String>,
}

/// Open a recorded session, answering how many messages it carried.
///
/// The approval mode goes back to default and the queue is dropped: both
/// belonged to the session being left, and carrying either into another one
/// would give it authority nobody granted it there.
#[cfg(feature = "tui")]
pub(crate) fn resume_into(session: SessionId, restoring: Restoring<'_>) -> usize {
    let (conversation, history) = reconstruct_session_conversation(restoring.workspace, session);
    *restoring.conversation = conversation;
    *restoring.history = history;
    restoring.transcript.clear();
    restoring.state.set_session_id(session);
    restoring.approval.clear_rules();
    set_approval_mode(
        restoring.approval,
        restoring.state,
        approval::ApprovalMode::Default,
    );
    restoring.queued.clear();
    restoring.conversation.len()
}

/// What a picker leaves behind when it is closed without an answer.
#[cfg(feature = "tui")]
pub(crate) struct Leaving<'a> {
    pub(crate) effort: Option<Effort>,
    pub(crate) theme: &'a str,
    pub(crate) roles: &'a std::collections::BTreeMap<String, String>,
    pub(crate) draft: &'a mut tui::ProviderDraft,
    pub(crate) auth_draft: &'a mut String,
    pub(crate) session: SessionId,
    pub(crate) route: &'a tui::ModelRoute,
}

/// Close a picker without taking an answer, and say what is still in force.
///
/// Ending input at a picker cancels the picker, not the session: the setting
/// is unchanged and the task prompt returns.
#[cfg(feature = "tui")]
pub(crate) fn leave_picker(prompt: &Prompt, leaving: Leaving<'_>) -> String {
    match prompt {
        Prompt::Effort => effort_line(leaving.effort),
        Prompt::Theme => {
            // The preview left the palette on the last row arrowed onto; put
            // the committed one back.
            tui::set_palette(leaving.theme, leaving.roles);
            format!("Theme unchanged: {}", leaving.theme)
        }
        Prompt::Provider(_) => {
            *leaving.draft = tui::ProviderDraft::default();
            "Provider unchanged.".to_owned()
        }
        Prompt::Auth(_) => {
            leaving.auth_draft.clear();
            "Auth unchanged.".to_owned()
        }
        Prompt::Resume => format!("Session unchanged: {}.", leaving.session),
        _ => format!("Model unchanged: {}", leaving.route),
    }
}

/// The provider for this route, resolved once and remembered.
///
/// A lookup that failed is remembered too: probing a credential store on every
/// turn is slow, and asks the operating system for a credential the operator
/// already declined once.
#[cfg(feature = "tui")]
pub(crate) fn resolve_route<'a>(
    invocation: &Invocation,
    workspace: &Path,
    provider: &str,
    resolved: &'a mut std::collections::HashMap<String, provider::Resolved>,
    unavailable: &mut std::collections::HashSet<String>,
) -> Option<&'a mut provider::Resolved> {
    if !resolved.contains_key(provider) && !unavailable.contains(provider) {
        let working = std::env::current_dir().unwrap_or_else(|_| workspace.to_path_buf());
        match load_config(workspace, &working, invocation.config.as_deref())
            .ok()
            .and_then(|config| provider::resolve(&config, Some(provider)).ok())
        {
            Some(found) => {
                resolved.insert(provider.to_owned(), found);
            }
            None => {
                unavailable.insert(provider.to_owned());
            }
        }
    }
    resolved.get_mut(provider)
}

/// Ask the operator what to do with the plan a planning turn produced.
///
/// The structured plan is preferred over the prose, because that is what the
/// harness recorded; the prose stands in only when no steps were written.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn settle_plan(
    workspace: &Path,
    response: &str,
    approval: &approval::ApprovalCell,
    state: &mut tui::TuiState,
    queued: &mut std::collections::VecDeque<String>,
    stdout: &mut io::Stdout,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
) -> Result<(), Diagnostic> {
    let projection = progress::plan(workspace, &state.session_id().to_string());
    let structured = progress::human_plan(&projection);
    let has_steps = projection["steps"]
        .as_array()
        .is_some_and(|steps| !steps.is_empty());
    let preview = if has_steps || response.trim().is_empty() {
        structured
    } else {
        response.to_owned()
    };
    match confirm_plan(stdout, colour, keys, decoder, &preview).map_err(terminal_failed)? {
        tui::AskDialogResult::Approve { note } => {
            let mode = approval.approve_plan();
            state.set_approval_mode(mode.label());
            let mut instruction = IMPLEMENT_APPROVED_PLAN.to_owned();
            if let Some(note) = note {
                instruction.push_str(&format!(" Operator constraint: {note}"));
            }
            queued.push_front(instruction);
            writeln!(stdout, "Plan approved. Entering {} mode.", mode.label())
                .map_err(terminal_failed)?;
        }
        // The dialog's second choice is "continue planning", so it stays in
        // Plan Mode and queues another planning turn.
        tui::AskDialogResult::ApproveRule { note } | tui::AskDialogResult::Revise { note } => {
            queued.push_front(revise_instruction(note.as_deref()));
        }
        tui::AskDialogResult::CycleMode => {
            let mode = cycle_approval_mode(approval);
            state.set_approval_mode(mode.label());
            queued.clear();
            writeln!(
                stdout,
                "Approval mode: {} — {}",
                mode.label(),
                mode.description()
            )
            .map_err(terminal_failed)?;
        }
        tui::AskDialogResult::Deny { .. } | tui::AskDialogResult::Cancel => {
            let mode = approval.cancel_plan();
            state.set_approval_mode(mode.label());
            queued.clear();
            writeln!(
                stdout,
                "Planning cancelled. Approval mode: {}.",
                mode.label()
            )
            .map_err(terminal_failed)?;
        }
    }
    Ok(())
}

/// Carry the provider wizard one step, and say which step comes next.
#[cfg(feature = "tui")]
/// What a line typed at the task prompt can reach, apart from the session
/// itself.
#[cfg(feature = "tui")]
pub(crate) struct Typing<'a> {
    pub(crate) workspace: &'a Path,
    pub(crate) colour: bool,
    pub(crate) provider_available: &'a mut bool,
    pub(crate) route: &'a mut tui::ModelRoute,
    pub(crate) effort: &'a mut Option<Effort>,
    pub(crate) models: &'a mut Vec<tui::ModelChoice>,
    pub(crate) providers: &'a mut Vec<String>,
    pub(crate) chosen: &'a mut Option<String>,
    pub(crate) draft: &'a mut tui::ProviderDraft,
    pub(crate) auth_draft: &'a mut String,
    pub(crate) theme: &'a mut String,
    pub(crate) roles: &'a std::collections::BTreeMap<String, String>,
    pub(crate) sessions: &'a mut Vec<tui::SessionChoice>,
    pub(crate) resolved_providers: &'a mut std::collections::HashMap<String, provider::Resolved>,
    pub(crate) unavailable_providers: &'a mut std::collections::HashSet<String>,
}

/// Answer a line typed at the task prompt.
///
/// A slash command is answered here; anything else is the task itself and is
/// sent to the model.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn answer_task(
    line: &str,
    invocation: &Invocation,
    mut typing: Typing<'_>,
    restoring: Restoring<'_>,
    stdout: &mut io::Stdout,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    emitter: &mut Emitter,
) -> Result<TaskPass, Diagnostic> {
    if matches!(line.trim(), ":quit" | "/quit" | "/exit") {
        write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
        return Ok(TaskPass::Stop);
    }
    if line.trim().starts_with('/') || line.trim().is_empty() {
        write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
        // `/provider` and `/auth` are one dialog now: access -> provider ->
        // manage. It may hand back a composer prompt for the text it still
        // needs (a key, a URL, an OAuth code).
        if matches!(line.trim(), "/provider" | "/auth") {
            let next =
                run_provider_dialog(invocation, &mut typing, stdout, keys, decoder, emitter)?;
            return Ok(next.map_or(TaskPass::Go, TaskPass::Ask));
        }
        // `/mcp`, `/hooks`, `/skill`, `/session`, `/settings` and `/model` alone
        // open their dialog, which writes the operator's configuration; with any
        // argument each is the read-only inspection it always was.
        let dialog = match line.trim() {
            "/mcp" => Some(Dialog::Mcp),
            "/hooks" => Some(Dialog::Hooks),
            "/skill" => Some(Dialog::Skill),
            "/session" => Some(Dialog::Session),
            "/settings" => Some(Dialog::Settings),
            "/model" => Some(Dialog::Model),
            _ => None,
        };
        if let Some(dialog) = dialog {
            run_dialog(
                dialog,
                invocation,
                &mut typing,
                restoring,
                stdout,
                keys,
                decoder,
                emitter,
            )?;
            return Ok(TaskPass::Go);
        }
        return slash_command(line, invocation, typing, restoring, stdout, emitter);
    }
    run_task(
        line, invocation, typing, restoring, stdout, keys, decoder, composer, emitter,
    )
}

/// What the composer is currently collecting a line for.
#[cfg(feature = "tui")]
pub(crate) enum Prompt {
    Task,
    Model,
    Effort,
    Theme,
    Provider(tui::ProviderStep),
    Auth(tui::AuthStep),
    Resume,
    Session(tui::SessionDialogState),
}

/// Which dialog a bare slash command opened.
#[cfg(feature = "tui")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Dialog {
    Mcp,
    Hooks,
    Skill,
    Session,
    Settings,
    Model,
}

/// Answer a slash command typed at the task prompt.
#[cfg(feature = "tui")]
pub(crate) fn slash_command(
    line: &str,
    invocation: &Invocation,
    typing: Typing<'_>,
    restoring: Restoring<'_>,
    stdout: &mut io::Stdout,
    emitter: &mut Emitter,
) -> Result<TaskPass, Diagnostic> {
    if manages_session(line) {
        let next = manage_session(line, restoring, typing.sessions, stdout)?;
        return Ok(next.map_or(TaskPass::Go, TaskPass::Ask));
    }
    if steers_turn(line) {
        steer_turn(
            line,
            typing.workspace,
            restoring.approval,
            restoring.state,
            restoring.queued,
            stdout,
        )?;
        return Ok(TaskPass::Go);
    }
    if opens_picker(line) {
        let next = open_picker(
            line,
            invocation,
            Opening {
                models: typing.models,
                providers: typing.providers,
                chosen: typing.chosen,
                draft: typing.draft,
                auth_draft: typing.auth_draft,
                effort: typing.effort,
                theme: typing.theme,
                roles: typing.roles,
                state: restoring.state,
                route: typing.route,
            },
            stdout,
            emitter,
        )?;
        return Ok(next.map_or(TaskPass::Go, TaskPass::Ask));
    }
    // An empty line is not a command and not a task: nothing to answer.
    if !line.trim().is_empty() {
        inspect_command(line, invocation, stdout, typing.colour, emitter)?;
    }
    Ok(TaskPass::Go)
}

/// Send the line to the model as the task it is.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_task(
    line: &str,
    invocation: &Invocation,
    typing: Typing<'_>,
    restoring: Restoring<'_>,
    stdout: &mut io::Stdout,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    emitter: &mut Emitter,
) -> Result<TaskPass, Diagnostic> {
    restoring.transcript.push_user(line);
    write!(stdout, "{}", composer.commit(line, typing.colour)).map_err(terminal_failed)?;
    stdout.flush().map_err(terminal_failed)?;
    if !*typing.provider_available {
        emitter.diagnostic(&Diagnostic::error(
            ARSY_PRV_1000,
            "provider unavailable",
            "configure a `[provider.endpoint.<name>]` table and run `arsy auth set <name>`, or \
             run codex login, then restart ARSY; /mcp and /hooks remain available",
        ));
        return Ok(TaskPass::Go);
    }
    let resolved = resolve_route(
        invocation,
        typing.workspace,
        &typing.route.provider,
        typing.resolved_providers,
        typing.unavailable_providers,
    );
    let footer = restoring.state.status_row(
        tui::terminal_width(),
        typing.colour,
        tui::branch(typing.workspace).as_deref(),
    );
    let pass = take_turn(
        invocation,
        resolved,
        line,
        &footer,
        Running {
            workspace: typing.workspace,
            route: typing.route,
            effort: *typing.effort,
            colour: typing.colour,
            state: restoring.state,
            conversation: restoring.conversation,
            transcript: restoring.transcript,
            history: restoring.history,
            approval: restoring.approval,
            queued: restoring.queued,
        },
        stdout,
        keys,
        decoder,
        composer,
        emitter,
    )?;
    Ok(match pass {
        Pass::Stop => TaskPass::Stop,
        Pass::Go => TaskPass::Go,
    })
}

/// What a turn runs against, and what it is allowed to change.
#[cfg(feature = "tui")]
pub(crate) struct Running<'a> {
    pub(crate) workspace: &'a Path,
    pub(crate) route: &'a tui::ModelRoute,
    pub(crate) effort: Option<Effort>,
    pub(crate) colour: bool,
    pub(crate) state: &'a mut tui::TuiState,
    pub(crate) conversation: &'a mut Vec<ModelMessage>,
    pub(crate) transcript: &'a mut tui::Transcript,
    pub(crate) history: &'a arsy_code::agent::budget::History,
    pub(crate) approval: &'a approval::ApprovalCell,
    pub(crate) queued: &'a mut std::collections::VecDeque<String>,
}

/// Run one turn and settle what it left behind.
///
/// Answers whether the session carries on: a turn can end it, and nothing
/// after that should run.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn take_turn(
    invocation: &Invocation,
    resolved: Option<&mut provider::Resolved>,
    line: &str,
    footer: &str,
    running: Running<'_>,
    stdout: &mut io::Stdout,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    emitter: &mut Emitter,
) -> Result<Pass, Diagnostic> {
    let turn = match run_turn(
        invocation,
        running.state.session_id(),
        resolved,
        line,
        running.history,
        running.route,
        running.effort,
        running.colour,
        footer,
        running.conversation,
        running.transcript,
        keys,
        decoder,
        composer,
        running.approval,
        emitter,
    ) {
        Ok(turn) if turn.quit => return Ok(Pass::Stop),
        Ok(turn) => turn,
        Err(diagnostic) => {
            emitter.diagnostic(&diagnostic);
            return Ok(Pass::Go);
        }
    };
    // A stopped turn takes the queue with it: a follow-up was queued to run
    // after this one, not instead of the stop.
    if turn.interrupted {
        running.queued.clear();
    }
    running.queued.extend(turn.queued);
    // Mode changes made while the provider was streaming happen through the
    // shared cell; refresh the visible projection before deciding whether a
    // plan dialog is still appropriate.
    running
        .state
        .set_approval_mode(running.approval.get().label());
    if running.approval.get() == approval::ApprovalMode::Plan
        && !turn.interrupted
        && turn.failure.is_none()
    {
        settle_plan(
            running.workspace,
            &turn.response,
            running.approval,
            running.state,
            running.queued,
            stdout,
            running.colour,
            keys,
            decoder,
        )?;
    }
    Ok(Pass::Go)
}

/// Answer a slash command that only reads: help, or one of the inspections
/// the CLI already answers.
///
/// The inspection runs through the same parser and the same dispatch a typed
/// `arsy` command does, so the two can never drift apart.
#[cfg(feature = "tui")]
pub(crate) fn inspect_command(
    line: &str,
    invocation: &Invocation,
    stdout: &mut io::Stdout,
    colour: bool,
    emitter: &mut Emitter,
) -> Result<(), Diagnostic> {
    if line.split_whitespace().next() == Some("/help") {
        return write!(stdout, "{}", tui::help(colour)).map_err(terminal_failed);
    }
    let Some(args) = inspection_args(line) else {
        return writeln!(stdout, "Unknown command. Use /help for available actions.")
            .map_err(terminal_failed);
    };
    match parse(args) {
        Ok(parsed) => {
            let inspection = Invocation {
                command: parsed.command,
                ..invocation.clone()
            };
            if let Err(diagnostic) = execute(&inspection, false, emitter) {
                emitter.diagnostic(&diagnostic);
            }
        }
        Err(diagnostic) => emitter.diagnostic(&diagnostic),
    }
    Ok(())
}

/// Fix the palette before the first frame.
/// Fix the palette before the first frame.
///
/// A rejected `[theme]` override is reported and dropped, never left to blank
/// the screen.
#[cfg(feature = "tui")]
pub(crate) fn open_palette(
    invocation: &Invocation,
    workspace: &Path,
    emitter: &mut Emitter,
) -> (arsy_kernel::config::Theme, String) {
    let config =
        load_config(workspace, workspace, invocation.config.as_deref()).unwrap_or_default();
    let (theme, palette) = resolve_palette(config.theme());
    match palette {
        Ok(palette) => tui::activate_palette(palette),
        Err(reason) => {
            emitter.diagnostic(&Diagnostic::warning(
                "ARSY-UIX-1002",
                format!("a [theme] override was ignored: {reason}"),
                "use #rrggbb colours and role names ARSY knows (see /help)",
            ));
            if let Some(palette) = tui::builtin_palette(&theme) {
                tui::activate_palette(palette);
            }
        }
    }
    tui::set_render_style(match config.ui_style() {
        "classic" => tui::RenderStyle::Classic,
        _ => tui::RenderStyle::Modern,
    });
    (config.theme().clone(), theme)
}

/// What the session already knows about its providers before the first turn.
///
/// A failed native resolution is remembered as unavailable so it is not probed
/// again on every turn while the external Codex login is still working.
#[cfg(feature = "tui")]
pub(crate) fn seed_providers(
    native: Option<provider::Resolved>,
    requested: Option<&str>,
    route: &str,
) -> (
    std::collections::HashMap<String, provider::Resolved>,
    std::collections::HashSet<String>,
) {
    let mut resolved = std::collections::HashMap::new();
    let mut unavailable = std::collections::HashSet::new();
    match native {
        Some(found) => {
            resolved.insert(route.to_owned(), found);
        }
        None => {
            if let Some(requested) = requested.filter(|requested| *requested != "auto") {
                unavailable.insert(requested.to_owned());
            }
        }
    }
    (resolved, unavailable)
}

/// The provider and model a session opens with.
#[cfg(feature = "tui")]
pub(crate) struct Opened {
    pub(crate) native: Option<provider::Resolved>,
    /// What was asked for, which is not always what resolved.
    pub(crate) native_requested: Option<String>,
    pub(crate) detected: tui::ModelRoute,
    pub(crate) provider_available: bool,
}

/// Resolve which provider and model this session starts on.
///
/// A configured endpoint is preferred, because it is the one ARSY talks to
/// itself. The Codex CLI stays the fallback for an operator who has not
/// configured anything, so an existing session keeps working as it did.
///
/// Nothing configured and no Codex login is not fatal: the session still opens
/// so `/mcp` and `/hooks` can inspect the workspace, and only a task turn is
/// refused.
///
/// ponytail: resolved once, so an OAuth access token is the one this session
/// started with; a session outliving the token's lifetime would need
/// re-resolving per turn, which costs a credential-store read each time. An
/// API key does not expire, and `arsy run` resolves per invocation, so only a
/// long interactive OAuth session is affected.
#[cfg(feature = "tui")]
pub(crate) fn open_route(invocation: &Invocation, workspace: &Path) -> Result<Opened, Diagnostic> {
    let native_requested = invocation.provider.clone().or_else(|| {
        load_config(workspace, workspace, invocation.config.as_deref())
            .ok()
            .and_then(|config| config.provider_default().map(str::to_owned))
    });
    let native = load_config(workspace, workspace, invocation.config.as_deref())
        .and_then(|config| {
            let resolved = provider::resolve(&config, native_requested.as_deref())?;
            // `--model` is checked here rather than defaulted: a model the
            // ceiling excludes must not open a session that would dispatch to
            // it, and falling back to the configured one would obey a flag the
            // operator did not give.
            let model = match invocation.model.as_deref() {
                Some(_) => {
                    selected_model(&config, &resolved.endpoint, invocation.model.as_deref())?
                }
                None => selected_model(&config, &resolved.endpoint, None).unwrap_or_default(),
            };
            Ok((resolved, model))
        })
        .ok();
    let detected = native.as_ref().map(|(resolved, model)| tui::ModelRoute {
        provider: resolved.endpoint.id.clone(),
        model: model.clone(),
    });
    // Nothing configured is not fatal: the session still opens so `/mcp` and
    // `/hooks` can inspect the workspace. Only a task turn is refused, which
    // `provider_available` gates below.
    let provider_available = detected.is_some();
    let detected = detected.unwrap_or_else(|| tui::ModelRoute {
        provider: String::new(),
        model: String::new(),
    });
    Ok(Opened {
        native: native.map(|(resolved, _)| resolved),
        native_requested,
        detected,
        provider_available,
    })
}

/// The slash commands that change how the next turn is allowed to act, or
/// report what the session has recorded about its work.
#[cfg(feature = "tui")]
pub(crate) fn steers_turn(line: &str) -> bool {
    matches!(
        line.split_whitespace().next(),
        Some("/plan" | "/todo" | "/agents" | "/approval")
    )
}

#[cfg(feature = "tui")]
pub(crate) fn steer_turn(
    line: &str,
    workspace: &Path,
    approval: &approval::ApprovalCell,
    state: &mut tui::TuiState,
    queued: &mut std::collections::VecDeque<String>,
    stdout: &mut io::Stdout,
) -> Result<(), Diagnostic> {
    match line.split_whitespace().next() {
        Some("/plan") => plan_step(line, workspace, approval, state, queued, stdout),
        Some("/todo") => show_todos(workspace, state.session_id(), stdout),
        Some("/agents") => agents_step(line, workspace, state.session_id(), stdout),
        Some("/approval") => set_mode(line, approval, state, stdout),
        _ => Ok(()),
    }
}

/// Enter, revise, approve, cancel or show the plan.
#[cfg(feature = "tui")]
#[cfg(feature = "tui")]
pub(crate) fn plan_step(
    line: &str,
    workspace: &Path,
    approval: &approval::ApprovalCell,
    state: &mut tui::TuiState,
    queued: &mut std::collections::VecDeque<String>,
    stdout: &mut io::Stdout,
) -> Result<(), Diagnostic> {
    match plan_command(line) {
        PlanCommand::Revise(note) => {
            approval.enter_plan();
            state.set_approval_mode(approval.get().label());
            queued.push_front(revise_instruction(note.as_deref()));
            Ok(())
        }
        PlanCommand::Approve if approval.get() == approval::ApprovalMode::Plan => {
            let mode = approval.approve_plan();
            state.set_approval_mode(mode.label());
            queued.push_front(IMPLEMENT_APPROVED_PLAN.to_owned());
            writeln!(stdout, "Plan approved. Entering {} mode.", mode.label())
                .map_err(terminal_failed)
        }
        PlanCommand::Approve => {
            writeln!(stdout, "No plan is awaiting approval.").map_err(terminal_failed)
        }
        PlanCommand::Cancel if approval.get() == approval::ApprovalMode::Plan => {
            let mode = approval.cancel_plan();
            state.set_approval_mode(mode.label());
            writeln!(
                stdout,
                "Planning cancelled. Approval mode: {}.",
                mode.label()
            )
            .map_err(terminal_failed)
        }
        PlanCommand::Cancel => {
            writeln!(stdout, "Plan Mode is not active.").map_err(terminal_failed)
        }
        // The live plan for this session's scope, which is the one the turn's
        // `plan_*` tools have been writing to.
        PlanCommand::Show => {
            let projection = progress::plan(workspace, &state.session_id().to_string());
            write!(stdout, "{}", progress::human_plan(&projection)).map_err(terminal_failed)
        }
        PlanCommand::Enter(task) => {
            approval.enter_plan();
            state.set_approval_mode(approval.get().label());
            writeln!(
                stdout,
                "Plan Mode active — workspace mutations are blocked."
            )
            .map_err(terminal_failed)?;
            if let Some(task) = task {
                queued.push_front(task);
            }
            Ok(())
        }
    }
}

/// The session's durable checklist.
///
/// Read from the store rather than from the turn's runtime: the checklist
/// outlives a turn, so what is on disk is the answer even if this session has
/// not touched it yet.
#[cfg(feature = "tui")]
pub(crate) fn show_todos(
    workspace: &Path,
    session: SessionId,
    stdout: &mut io::Stdout,
) -> Result<(), Diagnostic> {
    let projection = open_store(workspace)
        .ok()
        .and_then(|store| progress::todos(store as Arc<dyn EventStore>, session));
    match projection {
        Some(projection) => {
            write!(stdout, "{}", progress::human_todos(&projection)).map_err(terminal_failed)
        }
        None => {
            writeln!(stdout, "This session's TODOs could not be read.").map_err(terminal_failed)
        }
    }
}

/// Durable supervision state, rebuilt from the same session stream as resume.
#[cfg(feature = "tui")]
pub(crate) fn show_agents(
    workspace: &Path,
    session: SessionId,
    stdout: &mut io::Stdout,
) -> Result<(), Diagnostic> {
    let projection = open_store(workspace).ok().and_then(|store| {
        let graph =
            arsy_kernel::orchestration::TaskGraph::new(store, session, crate::actor()).ok()?;
        Some(arsy_kernel::hub::AgentHub::project(
            &graph,
            arsy_kernel::artifact::unix_time_ms(),
            arsy_kernel::hub::HubFilter::All,
        ))
    });
    match projection {
        Some(projection) => {
            write!(stdout, "{}", progress::human_agents(&projection)).map_err(terminal_failed)
        }
        None => {
            writeln!(stdout, "This session's agents could not be read.").map_err(terminal_failed)
        }
    }
}

/// Inspect or cooperatively control one exact attempt from the Agent Hub.
#[cfg(feature = "tui")]
fn agents_step(
    line: &str,
    workspace: &Path,
    session: SessionId,
    stdout: &mut io::Stdout,
) -> Result<(), Diagnostic> {
    let mut words = line
        .splitn(4, char::is_whitespace)
        .filter(|word| !word.is_empty());
    let _ = words.next();
    let Some(action) = words.next() else {
        return show_agents(workspace, session, stdout);
    };
    let attempt: arsy_kernel::domain::AttemptId = words
        .next()
        .ok_or_else(|| usage(format!("/agents {action} requires an attempt ID")))?
        .parse()
        .map_err(|_| usage("the attempt ID is not canonical"))?;
    let store = open_store(workspace)?;
    let mut graph = arsy_kernel::orchestration::TaskGraph::new(
        store as Arc<dyn EventStore>,
        session,
        crate::actor(),
    )
    .map_err(storage_failed)?;
    match action {
        "pause" => graph.request_pause(attempt).map_err(storage_failed)?,
        "resume" => graph.resume(attempt).map_err(storage_failed)?,
        "cancel" => graph
            .request_cancel(attempt, words.next().unwrap_or("cancelled by operator"))
            .map_err(storage_failed)?,
        "steer" => {
            let body = words
                .next()
                .filter(|body| !body.trim().is_empty())
                .ok_or_else(|| usage("/agents steer requires a message"))?;
            let task = graph
                .tasks()
                .find(|task| task.runtime.attempts.contains(&attempt))
                .map(|task| task.id)
                .ok_or_else(|| usage("the attempt is not in this session"))?;
            graph
                .send(
                    task,
                    task,
                    arsy_kernel::orchestration::MessageKind::Instruction,
                    json!(body),
                    None,
                    arsy_kernel::artifact::unix_time_ms(),
                )
                .map_err(storage_failed)?;
        }
        other => {
            return Err(usage(format!(
                "/agents action must be pause, resume, cancel, or steer; not {other:?}"
            )))
        }
    }
    writeln!(stdout, "Agent control recorded for attempt {attempt}.").map_err(terminal_failed)
}

/// Take the approval mode a command named, or report the one in force.
#[cfg(feature = "tui")]
pub(crate) fn set_mode(
    line: &str,
    approval: &approval::ApprovalCell,
    state: &mut tui::TuiState,
    stdout: &mut io::Stdout,
) -> Result<(), Diagnostic> {
    let named = line
        .split_whitespace()
        .nth(1)
        .and_then(|argument| match argument {
            // What Shift+Tab sends. Resolved here so the shortcut and the
            // typed command take the same path.
            "cycle" => Some(approval.get().cycle()),
            _ => approval::ApprovalMode::parse(argument),
        });
    let Some(mode) = named else {
        let current = approval.get();
        return writeln!(
            stdout,
            "Current approval mode: {} — {}\nUsage: /approval default | acceptEdits | plan | auto | dontAsk | bypassPermissions\nShift+Tab steps through default, acceptEdits, plan, and auto.",
            current.label(),
            current.description()
        )
        .map_err(terminal_failed);
    };
    set_approval_mode(approval, state, mode);
    writeln!(
        stdout,
        "Approval mode: {} — {}",
        mode.label(),
        mode.description()
    )
    .map_err(terminal_failed)
}

#[cfg(feature = "tui")]
pub(crate) enum TaskPass {
    /// Carry on at the task prompt.
    Go,
    /// Collect the next answer at this prompt instead.
    Ask(Prompt),
    Stop,
}
