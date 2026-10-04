//! The `/provider` and `/auth` wizards: every step validates its own answer,
//! and nothing is written until the last one.

#[cfg(feature = "tui")]
use crate::*;
use arsy_kernel::provider::Effort;
use arsy_kernel::secret::{FileCredentialStore, SecretHandle};
use serde_json::Value;
use std::io::{self, Write};

/// Where `/provider` goes after an answer.
#[cfg(feature = "tui")]
pub(crate) enum ProviderNext {
    Ask(tui::ProviderStep),
    Done(String),
    Cancelled(String),
}

/// The provider the configuration names right now.
#[cfg(feature = "tui")]
pub(crate) fn configured_default(invocation: &Invocation) -> Option<String> {
    let root = workspace_root(&invocation.workspace).ok()?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    load_config(&root, &working, invocation.config.as_deref())
        .ok()?
        .provider_default()
        .map(str::to_owned)
}

/// Take one answer and say what to ask next.
///
/// Every step validates its own answer and nothing is written until the last
/// one, so abandoning the wizard leaves the configuration exactly as it was.
#[cfg(feature = "tui")]
pub(crate) fn provider_step(
    step: tui::ProviderStep,
    line: &str,
    draft: &mut tui::ProviderDraft,
    providers: &[String],
) -> Result<ProviderNext, String> {
    use tui::ProviderStep as Step;

    let answer = if step.masked() { line } else { line.trim() };
    if answer.is_empty() {
        return Ok(ProviderNext::Cancelled("Provider unchanged.".to_owned()));
    }
    let one_of = |rows: &[(&str, &str)]| {
        rows.iter()
            .any(|(name, _)| *name == answer)
            .then(|| answer.to_owned())
            .ok_or_else(|| {
                format!(
                    "`{}` is not one of {}",
                    tui::safe_text(answer),
                    rows.iter()
                        .map(|(name, _)| *name)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    };
    let writable = |field: &str| {
        config_edit::is_writable(answer)
            .then(|| answer.to_owned())
            .ok_or_else(|| {
                format!("a {field} must be plain ASCII with no quotes, backslashes, or padding")
            })
    };

    match step {
        Step::Pick => provider_picked(answer, providers),
        Step::Name => {
            draft.name = provider_name(writable("provider name")?, providers)?;
            Ok(ProviderNext::Ask(Step::Kind))
        }
        Step::Kind => {
            draft.kind = one_of(tui::PROVIDER_KINDS)?;
            Ok(ProviderNext::Ask(Step::BaseUrl))
        }
        Step::BaseUrl => {
            let url = writable("base URL")?;
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err("a base URL starts with http:// or https://".to_owned());
            }
            draft.base_url = url;
            Ok(ProviderNext::Ask(Step::Model))
        }
        Step::Model => {
            draft.models = model_slugs(answer)?;
            Ok(ProviderNext::Ask(Step::Store))
        }
        Step::Store => {
            draft.store = one_of(tui::PROVIDER_STORES)?;
            Ok(ProviderNext::Ask(Step::Key))
        }
        Step::Key => provider_added(draft, answer),
        Step::Remove => {
            if !providers.iter().any(|name| name == answer) {
                return Err(format!(
                    "`{}` is not a configured provider",
                    tui::safe_text(answer)
                ));
            }
            draft.name = answer.to_owned();
            Ok(ProviderNext::Ask(Step::ConfirmRemove))
        }
        Step::ConfirmRemove => {
            if one_of(tui::CONFIRM_ROWS)? == "no" {
                return Ok(ProviderNext::Cancelled("Provider unchanged.".to_owned()));
            }
            provider_removed(&draft.name.clone())
        }
    }
}

/// The first answer: one of the two wizard rows, or an endpoint to switch to.
#[cfg(feature = "tui")]
pub(crate) fn provider_picked(answer: &str, providers: &[String]) -> Result<ProviderNext, String> {
    match answer {
        "+new" => Ok(ProviderNext::Ask(tui::ProviderStep::Name)),
        "-remove" => Ok(ProviderNext::Ask(tui::ProviderStep::Remove)),
        chosen if providers.iter().any(|name| name == chosen) => {
            write_config(|config| config_edit::set_default(config, chosen))?;
            Ok(ProviderNext::Done(format!("Provider: {chosen}")))
        }
        other => Err(format!(
            "`{}` is not a configured provider",
            tui::safe_text(other)
        )),
    }
}

/// A name for a new endpoint: not one that exists, and not one the picker
/// would read as its own `+new` or `-remove` row.
#[cfg(feature = "tui")]
pub(crate) fn provider_name(name: String, providers: &[String]) -> Result<String, String> {
    if providers.contains(&name) {
        return Err(format!("`{name}` is already configured"));
    }
    if name.starts_with(['+', '-']) {
        return Err("a provider name cannot start with `+` or `-`".to_owned());
    }
    Ok(name)
}

/// The last answer of the add wizard: store the credential, write the
/// endpoint, and make it the default.
#[cfg(feature = "tui")]
pub(crate) fn provider_added(
    draft: &tui::ProviderDraft,
    key: &str,
) -> Result<ProviderNext, String> {
    let handle = store_credential(&draft.name, key)?;
    let endpoint = config_edit::Endpoint {
        name: draft.name.clone(),
        kind: draft.kind.clone(),
        base_url: draft.base_url.clone(),
        models: draft.models.clone(),
        credential: handle,
    };
    write_config(|config| {
        let config = config_edit::append_endpoint(config, &endpoint)?;
        config_edit::set_default(&config, &endpoint.name)
    })?;
    Ok(ProviderNext::Done(format!(
        "Added provider {} with {} model{}, and made it the default. The others are \
         still configured; `/provider` switches between them.",
        endpoint.name,
        endpoint.models.len(),
        if endpoint.models.len() == 1 { "" } else { "s" },
    )))
}

/// Remove the endpoint and every credential stored for it.
///
/// The catalog is updated first and the stores after it, so a store that
/// refuses cannot leave the catalog naming a credential the wizard just said
/// it removed.
#[cfg(feature = "tui")]
pub(crate) fn provider_removed(name: &str) -> Result<ProviderNext, String> {
    write_config(|config| config_edit::remove_endpoint(config, name))?;
    if let Ok(mut records) = catalog() {
        let removed: Vec<SecretHandle> = records
            .iter()
            .filter(|record| {
                record.handle.name() == format!("{name}.key")
                    || record.handle.name() == format!("endpoint.{name}")
            })
            .map(|record| record.handle.clone())
            .collect();
        records.retain(|record| !removed.contains(&record.handle));
        let _ = save_catalog(&records);
        for handle in removed {
            forget_credential(&handle);
        }
    }
    Ok(ProviderNext::Done(format!(
        "Removed provider {name} and its credentials."
    )))
}

/// Delete one stored credential from whichever store holds it. A store that
/// refuses is not an error here: the catalog no longer names the handle, and
/// the wizard has nothing left to undo.
#[cfg(feature = "tui")]
pub(crate) fn forget_credential(handle: &SecretHandle) {
    if handle.store() == FILE_STORE_ID {
        let _ = FileCredentialStore.remove(handle.name());
    }
}
#[cfg(feature = "tui")]
pub(crate) enum AuthNext {
    Ask(tui::AuthStep),
    Done(String),
    Cancelled(String),
}

#[cfg(feature = "tui")]
pub(crate) fn catalog_handles() -> Vec<String> {
    catalog()
        .map(|records| records.into_iter().map(|r| r.handle.to_string()).collect())
        .unwrap_or_default()
}

#[cfg(feature = "tui")]
pub(crate) fn auth_step(
    invocation: &Invocation,
    step: tui::AuthStep,
    line: &str,
    draft_provider: &mut String,
    providers: &[String],
    emitter: &mut Emitter,
) -> Result<AuthNext, String> {
    let answer = if step.masked() { line } else { line.trim() };
    if answer.is_empty() {
        return Ok(AuthNext::Cancelled("Auth unchanged.".to_owned()));
    }
    match step {
        tui::AuthStep::Pick => auth_pick_answer(answer, providers),
        tui::AuthStep::LoginProvider => {
            auth_login_provider_answer(invocation, answer, draft_provider, providers, emitter)
        }
        tui::AuthStep::SetProvider => auth_set_provider_answer(answer, providers, draft_provider),
        tui::AuthStep::SetKey => auth_set_key_answer(draft_provider, answer),
        tui::AuthStep::RemoveHandle => auth_remove_handle_answer(answer),
        tui::AuthStep::PasteCode => {
            auth_paste_code_answer(invocation, draft_provider, answer, emitter)
        }
    }
}
/// The answer `Pick` accepts: exactly the verbs the `/auth` menu offers.
/// A silent default is impossible, so an unknown verb is the caller's error.
fn auth_pick_answer(answer: &str, providers: &[String]) -> Result<AuthNext, String> {
    match answer {
        // A built-in preset is always an option, so `login` never dead-ends
        // the way `set` does with nothing configured.
        "login" => Ok(AuthNext::Ask(tui::AuthStep::LoginProvider)),
        "list" => {
            let records = catalog().map_err(|e| e.message)?;
            let human = human_credentials(&records);
            let rendered = human
                .get("credentials")
                .and_then(Value::as_str)
                .unwrap_or("No credentials catalogued.");
            Ok(AuthNext::Done(rendered.to_owned()))
        }
        "set" => {
            if providers.is_empty() {
                return Err(
                    "no providers are configured; configure a provider endpoint first".to_owned(),
                );
            }
            Ok(AuthNext::Ask(tui::AuthStep::SetProvider))
        }
        "remove" => {
            let records = catalog().map_err(|e| e.message)?;
            if records.is_empty() {
                return Err("no credentials are saved in the catalog".to_owned());
            }
            Ok(AuthNext::Ask(tui::AuthStep::RemoveHandle))
        }
        other => Err(format!(
            "`{}` is not one of login, list, set, remove",
            tui::safe_text(other)
        )),
    }
}

/// The answer `LoginProvider` accepts: a configured provider or a preset.
fn auth_login_provider_answer(
    invocation: &Invocation,
    answer: &str,
    draft_provider: &mut String,
    providers: &[String],
    emitter: &mut Emitter,
) -> Result<AuthNext, String> {
    let known =
        providers.iter().any(|p| p == answer) || arsy_kernel::oauth::presets::get(answer).is_some();
    if !known {
        return Err(format!(
            "`{}` is not a configured provider or a built-in preset",
            tui::safe_text(answer)
        ));
    }
    let oauth = resolve_oauth_login(invocation, answer)
        .map_err(|e| e.message)?
        .oauth;
    if arsy_kernel::oauth::uses_manual_grant(&oauth) {
        // The keyboard belongs to this wizard. A blocking read inside
        // auth_login would never see the paste, so the next wizard step owns
        // it just like an API key does.
        let prompt = arsy_kernel::oauth::begin_manual(&oauth).map_err(|e| e.to_string())?;
        let opened = emitter.output == Output::Human && open_browser(&prompt.authorize_url);
        *draft_provider = format!("{answer}\n{}", prompt.verifier);
        let _ = writeln!(
            io::stderr(),
            "{}\n  {}\nThen paste the code it shows you here.",
            if opened {
                "Opening your browser to sign in. If it did not open, visit:"
            } else {
                "Open this URL to sign in:"
            },
            prompt.authorize_url
        );
        return Ok(AuthNext::Ask(tui::AuthStep::PasteCode));
    }
    auth_login(invocation, answer, emitter).map_err(|e| e.message)?;
    Ok(AuthNext::Done(format!(
        "Signed in to `{answer}` with OAuth."
    )))
}

/// Finish an OAuth issuer's manual grant with the code the wizard collected.
fn auth_paste_code_answer(
    invocation: &Invocation,
    draft_provider: &str,
    answer: &str,
    emitter: &mut Emitter,
) -> Result<AuthNext, String> {
    let (provider, verifier) = draft_provider
        .split_once('\n')
        .ok_or_else(|| "the login was interrupted; run `/auth login` again".to_owned())?;
    let login = resolve_oauth_login(invocation, provider).map_err(|e| e.message)?;
    let transport = arsy_kernel::provider::http::HttpTransport::default();
    let tokens = arsy_kernel::oauth::finish_manual(&transport, &login.oauth, verifier, answer)
        .map_err(|e| e.to_string())?;
    store_oauth_login(invocation, provider, &login, tokens, emitter).map_err(|e| e.message)?;
    Ok(AuthNext::Done(format!(
        "Signed in to `{provider}` with OAuth."
    )))
}

/// The answer `SetProvider` accepts: only a configured provider.
fn auth_set_provider_answer(
    answer: &str,
    providers: &[String],
    draft_provider: &mut String,
) -> Result<AuthNext, String> {
    if !providers.iter().any(|p| p == answer) {
        return Err(format!(
            "`{}` is not a configured provider",
            tui::safe_text(answer)
        ));
    }
    *draft_provider = answer.to_owned();
    Ok(AuthNext::Ask(tui::AuthStep::SetKey))
}

/// The answer `SetKey` accepts: anything a store will keep.
fn auth_set_key_answer(draft_provider: &str, answer: &str) -> Result<AuthNext, String> {
    store_credential(draft_provider, answer).map_err(|e| e.to_string())?;
    Ok(AuthNext::Done(format!(
        "Stored API key for `{draft_provider}` in the credential store."
    )))
}

/// The answer `RemoveHandle` accepts: a credential handle that is catalogued.
fn auth_remove_handle_answer(answer: &str) -> Result<AuthNext, String> {
    let handle: SecretHandle =
        SecretHandle::try_from(answer.to_owned()).map_err(|error| format!("{error}"))?;
    let mut records = catalog().map_err(|e| e.message)?;
    let provider = records
        .iter()
        .find(|record| record.handle == handle)
        .map(|record| record.provider.clone());
    if let Some(provider) = provider
        .as_deref()
        .filter(|provider| arsy_kernel::oauth::presets::get(provider).is_some())
    {
        write_config(|config| config_edit::remove_endpoint(config, provider))?;
    }
    records.retain(|record| record.handle != handle);
    save_catalog(&records).map_err(|e| e.message)?;
    forget_credential(&handle);
    Ok(AuthNext::Done(format!("Removed credential `{handle}`.")))
}
/// One host serves several models, so the model step takes a list. The first is
/// the endpoint's default; the rest are what `/model` offers beside it.
#[cfg(feature = "tui")]
pub(crate) fn model_slugs(answer: &str) -> Result<Vec<String>, String> {
    let mut models: Vec<String> = Vec::new();
    for slug in answer
        .split(',')
        .map(str::trim)
        .filter(|slug| !slug.is_empty())
    {
        if !config_edit::is_writable(slug) {
            return Err(format!(
                "`{}` is not a model slug: plain ASCII, no quotes or backslashes",
                tui::safe_text(slug)
            ));
        }
        if !models.iter().any(|existing| existing == slug) {
            models.push(slug.to_owned());
        }
    }
    if models.is_empty() {
        return Err("name at least one model".to_owned());
    }
    Ok(models)
}

/// Put a typed credential beside the user configuration, and give back the
/// handle the configuration should point at.
#[cfg(feature = "tui")]
pub(crate) fn store_credential(name: &str, secret: &str) -> Result<String, String> {
    let secret = secret.trim();
    if secret.len() < arsy_kernel::secret::MIN_SECRET_BYTES {
        return Err("that credential is too short to redact safely".to_owned());
    }
    let file = format!("{name}.key");
    let path = FileCredentialStore::path(&file)
        .ok_or_else(|| "this platform has no user configuration directory".to_owned())?;
    FileCredentialStore::prepare(&path).map_err(|error| error.to_string())?;
    let mut written = owner_only(&path).map_err(|error| error.message)?;
    written
        .write_all(secret.as_bytes())
        .map_err(|error| error.to_string())?;
    let handle = SecretHandle::new(FILE_STORE_ID, file).map_err(|error| error.to_string())?;

    // Catalogued exactly as `arsy auth set` catalogues one, for two reasons:
    // `auth list` can show it, and every turn registers the catalogued handles
    // for redaction — a credential missing from the catalog is one that could
    // reach output unredacted.
    let mut records = catalog().map_err(|error| error.message)?;
    // Updated in place when the handle is already known, the way `auth set`
    // updates it, so re-entering a credential does not reset when it was first
    // stored.
    match records.iter_mut().find(|record| record.handle == handle) {
        Some(record) => {
            record.provider = name.to_owned();
            record.kind = CredentialKind::ApiKey;
        }
        None => records.push(AuthRecord {
            provider: name.to_owned(),
            handle: handle.clone(),
            created_at: now().map_err(|error| error.message)?,
            last_used: None,
            kind: CredentialKind::ApiKey,
        }),
    }
    save_catalog(&records).map_err(|error| error.message)?;
    Ok(handle.to_string())
}

/// Rewrite the user configuration through `edit`.
///
/// The file is read and written whole, so `edit` sees exactly what is on disk
/// and nothing it did not change can move.
pub(crate) fn write_config(
    edit: impl FnOnce(&str) -> Result<String, String>,
) -> Result<(), String> {
    let path = arsy_kernel::config::user_config()
        .ok_or_else(|| "this platform has no user configuration directory".to_owned())?;
    let original = match std::fs::read_to_string(&path) {
        Ok(original) => original,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("the configuration could not be read: {error}")),
    };
    let updated = edit(&original)?;
    // Refused only when the edit breaks a file that loaded. One already
    // broken is the operator's to fix, and a repair that leaves something
    // else broken still has to land.
    let scope = crate::mcp::Scope::User;
    if original.trim().is_empty() || crate::settings::validate(&path, scope, &original).is_ok() {
        crate::settings::validate(&path, scope, &updated)?;
    }
    replace_file(&path, updated.as_bytes())
        .map_err(|error| format!("the configuration could not be written: {error}"))
}

/// What to print once an effort answer is accepted.
#[cfg(feature = "tui")]
pub(crate) fn effort_line(effort: Option<Effort>) -> String {
    match effort {
        Some(effort) => format!("Effort: {effort}"),
        None => "Effort: off, so no reasoning setting is sent".to_owned(),
    }
}
