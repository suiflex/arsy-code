//! Turning configuration into a usable model provider.
//!
//! Configuration names an endpoint; a credential file and the environment hold
//! the credential for it. This module is the only place the two meet, and it
//! is deliberately the last step before the wire: a resolved credential is
//! registered for redaction as it is read, so every sink already knows to hide
//! it by the time a request is built.

use crate::{Diagnostic, ARSY_PRV_1000};
use arsy_kernel::provider::replay::ReplayProvider;
use arsy_kernel::{
    config::{Config, Dialect, Endpoint},
    oauth::{self, TokenSet},
    provider::{
        anthropic::AnthropicProvider,
        google_code_assist::{antigravity_user_agent, GoogleCodeAssistProvider},
        http::HttpTransport,
        openai::OpenAiProvider,
        openai_responses::OpenAiResponsesProvider,
        wire::{ApiKey, WireRequest, WireResponse, WireTransport},
        ModelProvider,
    },
    routing,
    secret::{
        CredentialStore, FileCredentialStore, Redactor, SecretError, SecretHandle,
        WithdrawnOsStore, FILE_STORE_ID, OS_STORE_ID,
    },
};
use std::sync::Arc;

/// Where a credential came from. Reported by `arsy doctor` so an operator can
/// tell a stored credential from an inherited environment variable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialSource {
    /// The endpoint needs none: a replay reads a file.
    None,
    ConfiguredEnv,
    File,
    OAuth,
    DefaultEnv,
}

impl CredentialSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ConfiguredEnv => "configured_env",
            Self::File => "file",
            Self::OAuth => "oauth",
            Self::DefaultEnv => "default_env",
        }
    }

    /// Where a stored key came from. One store answers, so this says so rather
    /// than leaving `arsy doctor` to assume it.
    const fn stored_in(_store: &str) -> Self {
        Self::File
    }
}

/// The provider a turn will use, plus how it was assembled.
///
/// The adapter is shared rather than owned so a turn can be streamed on its
/// own thread while the terminal keeps repainting.
#[derive(Clone)]
pub struct Resolved {
    pub provider: Arc<dyn ModelProvider>,
    pub endpoint: Endpoint,
    pub source: CredentialSource,
    /// Present when routing chose the endpoint rather than configuration
    /// naming it, so a caller can report which criterion decided.
    pub route: Option<routing::Decision>,
}

/// Fill model limits from provider metadata when no context limit is configured.
pub(crate) fn ensure_context_window(resolved: &mut Resolved, model: &str) -> Result<(), String> {
    if resolved.endpoint.context_windows.contains_key(model)
        || resolved.endpoint.input_limits.contains_key(model)
    {
        if resolved.endpoint.kind == Dialect::GoogleCodeAssist
            && matches!(
                model,
                "gemini-3.8-flash-low" | "gemini-3.8-flash-medium" | "gemini-3.8-flash-high"
            )
        {
            resolved
                .endpoint
                .output_limits
                .entry(model.to_owned())
                .or_insert(65_536);
        }
        return Ok(());
    }
    if resolved.endpoint.kind == Dialect::Replay {
        return Err(format!(
            "replay model `{model}` has no declared context window"
        ));
    }
    let (token, _) = credential(&resolved.endpoint, &from_env).map_err(|error| error.message)?;
    let mut found = fetch_models_http(
        &HttpTransport::default(),
        resolved.endpoint.kind.as_str(),
        &resolved.endpoint.base_url,
        Some(&token),
    )
    .ok_or_else(|| {
        format!(
            "could not read model metadata from provider `{}`",
            resolved.endpoint.id
        )
    })?;
    if resolved.endpoint.kind == Dialect::GoogleCodeAssist {
        for id in &found.models {
            if matches!(
                id.as_str(),
                "gemini-3.8-flash-low" | "gemini-3.8-flash-medium" | "gemini-3.8-flash-high"
            ) {
                // Google's published base-model limits apply to the three
                // effort variants advertised by Antigravity.
                found.context_windows.entry(id.clone()).or_insert(1_048_576);
                found.output_limits.entry(id.clone()).or_insert(65_536);
            }
        }
    }
    for (id, window) in found.context_windows {
        resolved
            .endpoint
            .context_windows
            .entry(id)
            .or_insert(window);
    }
    for (id, limit) in found.input_limits {
        resolved.endpoint.input_limits.entry(id).or_insert(limit);
    }
    for (id, limit) in found.output_limits {
        resolved.endpoint.output_limits.entry(id).or_insert(limit);
    }
    if resolved.endpoint.context_windows.contains_key(model)
        || resolved.endpoint.input_limits.contains_key(model)
    {
        Ok(())
    } else {
        Err(format!(
            "provider `{}` did not report a context limit for model `{model}`; set a verified total limit at `provider.endpoint.{}.context_windows.{model}` (the exact model ID, including its effort suffix)",
            resolved.endpoint.id, resolved.endpoint.id
        ))
    }
}

/// Build the provider for `requested`, or for the configured default.
///
/// The credential is looked for in the order an operator would expect to
/// override it: an environment variable the config names, then the credential
/// the config names, then the dialect's conventional variable. The first
/// one that holds a value wins, so exporting a key for one shell is enough to
/// override a stored one without editing anything.
pub fn resolve(config: &Config, requested: Option<&str>) -> Result<Resolved, Diagnostic> {
    let (endpoint, route) = resolve_with_route(config, requested)?;
    build(endpoint, route)
}

/// The endpoint a turn should use, and the routing decision that chose it.
///
/// Configuration selects an endpoint by name; `provider.default = "auto"` — the
/// documented default — leaves the choice to routing, which picks between the
/// endpoints policy already allows and never outside them.
pub fn resolve_with_route(
    config: &Config,
    requested: Option<&str>,
) -> Result<(Endpoint, Option<routing::Decision>), Diagnostic> {
    if let Some(id) = requested
        .or_else(|| config.provider_default())
        .filter(|id| *id != "auto")
    {
        // A named provider is used as named, or reported as missing. Routing
        // must never substitute another one for the one that was asked for.
        if let Some(endpoint) = config.endpoint(Some(id)).cloned() {
            return Ok((endpoint, None));
        }
        if let Some(preset) = arsy_kernel::oauth::presets::get(id) {
            let canonical_id = preset.id;
            let endpoint = Endpoint {
                // A preset names a public endpoint, not what an account pays
                // for it: a price has to be configured, never assumed.
                pricing: std::collections::BTreeMap::new(),
                id: canonical_id.to_owned(),
                kind: preset.dialect,
                base_url: preset.base_url.to_owned(),
                // Beside the user configuration, not in the platform keyring:
                // a preset that synthesized a keyring handle is what made a
                // rebuilt binary ask to unlock it on every turn.
                credential: arsy_kernel::secret::SecretHandle::new(
                    arsy_kernel::secret::FILE_STORE_ID,
                    format!("{canonical_id}.key"),
                )
                .ok(),
                api_key_env: None,
                model: preset.models.first().map(|s| (*s).to_owned()),
                models: preset.models.iter().map(|s| (*s).to_owned()).collect(),
                max_output_tokens: arsy_kernel::config::DEFAULT_MAX_OUTPUT_TOKENS,
                max_output_tokens_explicit: false,
                context_windows: std::collections::BTreeMap::new(),
                input_limits: std::collections::BTreeMap::new(),
                output_limits: std::collections::BTreeMap::new(),
                oauth: Some(preset.oauth()),
                sanitize_tool_names: false,
                efforts: std::collections::BTreeMap::new(),
            };
            return Ok((endpoint, None));
        }
        return Err(unconfigured(Some(id)));
    }
    // Every unnamed choice goes through routing, including the single-endpoint
    // one: that is where `model.allowed` is applied, and an endpoint whose only
    // model a ceiling excludes must not be selected just because it is alone.
    let decision = route(config);
    let Some(key) = decision.key() else {
        let routing::Decision::Refused { reason, excluded } = &decision else {
            unreachable!("only a refusal has no key")
        };
        return Err(Diagnostic::error(
            ARSY_PRV_1000,
            format!("no provider could be routed to: {reason}"),
            excluded.first().map_or_else(
                || {
                    "configure a `[provider.endpoint.<name>]` table, or set `provider.default`"
                        .to_owned()
                },
                |first| format!("{} was excluded because {}", first.key, first.reason),
            ),
        ));
    };
    let endpoint = config
        .endpoint(Some(&key.provider))
        .cloned()
        .ok_or_else(|| unconfigured(Some(&key.provider)))?;
    Ok((endpoint, Some(decision)))
}

/// Rank the allowed endpoints. One candidate per endpoint, keyed by the model
/// it would use, because an endpoint is what carries the URL and the credential.
fn route(config: &Config) -> routing::Decision {
    // The ceilings are passed to the router rather than applied here, so an
    // endpoint policy excluded is reported as excluded instead of vanishing.
    let candidates: Vec<routing::Candidate> = config
        .all_endpoints()
        .filter_map(|endpoint| {
            let model = endpoint
                .model
                .clone()
                .or_else(|| config.model_default().map(str::to_owned))
                .or_else(|| config.compat_model(endpoint).map(str::to_owned))?;
            let context_window = endpoint
                .context_windows
                .get(&model)
                .map(|tokens| u64::from(*tokens));
            Some(routing::Candidate {
                key: arsy_kernel::provider::ModelKey {
                    provider: endpoint.id.clone(),
                    model: model.clone(),
                },
                capabilities: arsy_kernel::model_profile::declared(Some(u64::from(
                    endpoint.output_tokens_for(&model),
                ))),
                residency: None,
                cost_micros_per_1k: None,
                context_window,
                modalities: std::collections::BTreeSet::new(),
                provider_features: std::collections::BTreeSet::new(),
            })
        })
        .collect();
    routing::decide(
        &candidates,
        &routing::Constraints {
            // Threaded as the Option configuration resolved it: a layer that
            // wrote `allowed = []` capped everything out, and flattening that
            // to "no ceiling" would route to an endpoint it forbade and then
            // report the endpoint as unconfigured.
            allowed_providers: config.provider_allowed().cloned(),
            allowed_models: config.model_allowed().cloned(),
            ..routing::Constraints::default()
        },
        // Nothing is persisted across processes yet, so a routed choice is
        // decided by policy and by the deterministic tie-break rather than by
        // measurements this run has not taken.
        &routing::Observations::new(),
        &routing::Preference {
            route: true,
            ..routing::Preference::default()
        },
    )
}

fn unconfigured(named: Option<&str>) -> Diagnostic {
    Diagnostic::error(
        ARSY_PRV_1000,
        match named {
            Some(id) => format!("no provider endpoint named `{id}` is configured"),
            None => "no provider endpoint is configured".to_owned(),
        },
        "add a `provider.endpoint.<name>` object with `kind` and `base_url` to the user \
         arsy.json, then run `arsy config explain provider`",
    )
}

/// Assemble the adapter for a chosen endpoint: credential, redaction, dialect.
fn build(endpoint: Endpoint, route: Option<routing::Decision>) -> Result<Resolved, Diagnostic> {
    // A replay reads a file. Asking for a credential first would make every
    // measurement and every reproduction need one to reach a script that no
    // credential protects.
    if endpoint.kind == Dialect::Replay {
        let provider: Arc<dyn ModelProvider> = Arc::new(
            ReplayProvider::from_base_url(&endpoint.base_url, &endpoint.id).map_err(|error| {
                Diagnostic::error(
                    ARSY_PRV_1000,
                    error.to_string(),
                    "point `base_url` at a replay script: a JSON file with a `replies` array",
                )
            })?,
        );
        return Ok(Resolved {
            provider,
            endpoint,
            source: CredentialSource::None,
            route,
        });
    }
    let (secret, source) = credential(&endpoint, &from_env)?;
    let mut redactor = Redactor::new();
    // Registering here, rather than at the wire, means a key echoed back into
    // a prompt or an event is already masked — whichever source it came from,
    // not only a stored one. A value too short to redact safely is refused
    // rather than sent with a pipeline that would corrupt unrelated text.
    redactor
        .register(&redaction_handle(&endpoint, source)?, &secret)
        .map_err(|error| credential_failed(&endpoint.id, error))?;
    let key = ApiKey::new(secret);
    let transport = HttpTransport::default();
    let provider: Arc<dyn ModelProvider> = match endpoint.kind {
        Dialect::Anthropic => {
            let mut provider = AnthropicProvider::with_base_url(&endpoint.base_url, key, transport)
                .with_redactor(redactor);
            // A Claude Code OAuth access token needs the beta header that
            // tells Anthropic it is not a Console API key; a hand-set key
            // needs none of this and would be rejected if it were sent.
            if source == CredentialSource::OAuth {
                provider = provider.with_oauth();
            }
            Arc::new(provider)
        }
        Dialect::Openai => Arc::new(
            OpenAiProvider::with_base_url(&endpoint.base_url, key, transport)
                .with_id(&endpoint.id)
                .with_redactor(redactor)
                .with_sanitized_tool_names(endpoint.sanitize_tool_names),
        ),
        Dialect::OpenaiResponses => Arc::new(
            OpenAiResponsesProvider::with_base_url(&endpoint.base_url, key, transport)
                .with_id(&endpoint.id)
                .with_redactor(redactor),
        ),
        Dialect::GoogleCodeAssist => {
            let mut provider =
                GoogleCodeAssistProvider::with_base_url(&endpoint.base_url, key, transport)
                    .with_id(&endpoint.id)
                    .with_redactor(redactor);
            // A login keeps the project it was provisioned with. One made
            // before that was kept is looked up once here and written back;
            // a lookup that fails is left to the first turn, which reports
            // why instead of failing provider selection.
            if source == CredentialSource::OAuth {
                if let Some(project) = stored_project(endpoint.credential.as_ref()) {
                    provider = provider.with_project(project);
                } else if let Ok(project) = provider.discover_project() {
                    remember_project(endpoint.credential.as_ref(), &project);
                    provider = provider.with_project(project);
                }
            }
            Arc::new(provider)
        }
        // Answered above, before a credential was asked for.
        Dialect::Replay => unreachable!("a replay endpoint returns before this point"),
    };
    Ok(Resolved {
        provider,
        endpoint,
        source,
        route,
    })
}

/// `env` is injected because the workspace forbids `unsafe`, and mutating the
/// process environment in a test needs it.
fn credential(
    endpoint: &Endpoint,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(String, CredentialSource), Diagnostic> {
    if let Some(name) = &endpoint.api_key_env {
        if let Some(value) = present(env(name)) {
            return Ok((value, CredentialSource::ConfiguredEnv));
        }
    }
    if let Some(handle) = &endpoint.credential {
        // The store half of the handle decides where to look. An unknown one is
        // an error rather than a quiet fall back somewhere else: a handle that
        // names a store ARSY does not have must not resolve to a different
        // credential than it asked for. The withdrawn keyring answers for its
        // own handles, so an operator is told what to re-run instead of being
        // told the store was never heard of.
        let resolved = match handle.store() {
            OS_STORE_ID => WithdrawnOsStore.resolve(handle.name()),
            FILE_STORE_ID => FileCredentialStore.resolve(handle.name()),
            other => Err(SecretError::UnknownStore(other.to_owned())),
        };
        match resolved {
            Ok(value) => {
                if let Some(value) = present(Some(value)) {
                    return stored(endpoint, handle, value);
                }
            }
            Err(SecretError::NotFound(_)) => {}
            Err(error) => return Err(credential_failed(&endpoint.id, error)),
        }
    }
    if let Some(value) = present(env(endpoint.kind.default_api_key_env())) {
        return Ok((value, CredentialSource::DefaultEnv));
    }
    Err(Diagnostic::error(
        ARSY_PRV_1000,
        format!("no credential is available for provider `{}`", endpoint.id),
        format!(
            "run `arsy auth set {}` and point `credential` at the handle it prints, or export {}",
            endpoint.id,
            endpoint
                .api_key_env
                .as_deref()
                .unwrap_or_else(|| endpoint.kind.default_api_key_env())
        ),
    ))
}

fn from_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// A source that is present but blank counts as absent, so an accidentally
/// empty export or keyring entry falls through to the next source instead of
/// failing at the wire as an authentication error.
fn present(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

/// Interpret what the credential store holds for this endpoint.
///
/// `arsy auth login` writes a token set as JSON under the same handle an API
/// key would use, so the two are told apart by shape rather than by a second
/// lookup. An expired access token is refreshed and written back here, which
/// is the only place that can happen before the value reaches the wire.
/// What the credential store is holding for this endpoint.
#[derive(Debug, Eq, PartialEq)]
enum Stored {
    /// Anything that is not a token set, taken verbatim.
    ApiKey(String),
    Token(TokenSet),
    /// A token set that has to be renewed before it is used.
    Expired(TokenSet),
}

/// Tell an API key from a stored login by shape.
///
/// `arsy auth login` writes a token set as JSON under the same handle an API
/// key would use, so there is no second lookup to disambiguate them. Anything
/// that does not deserialize as a token set is an API key, which keeps a key
/// that happens to look like JSON usable.
fn classify(value: String, now: u64) -> Stored {
    match serde_json::from_str::<TokenSet>(&value) {
        Err(_) => Stored::ApiKey(value),
        Ok(tokens) if tokens.is_expired(now) => Stored::Expired(tokens),
        Ok(tokens) => Stored::Token(tokens),
    }
}

/// Interpret what the credential store holds, renewing a lapsed login.
///
/// This is the only place a refresh can happen before the value reaches the
/// wire, so it is also the only place the renewed token can be written back.
fn stored(
    endpoint: &Endpoint,
    handle: &SecretHandle,
    value: String,
) -> Result<(String, CredentialSource), Diagnostic> {
    let tokens = match classify(value, oauth::now()) {
        Stored::ApiKey(value) => return Ok((value, CredentialSource::stored_in(handle.store()))),
        Stored::Token(tokens) => return Ok((tokens.access_token, CredentialSource::OAuth)),
        Stored::Expired(tokens) => tokens,
    };
    // A hand-configured endpoint carries its own `[oauth]`; a built-in preset
    // does not, so fall back to the preset that shares the endpoint's id.
    let oauth_client = endpoint
        .oauth
        .clone()
        .or_else(|| oauth::presets::get(&endpoint.id).map(|preset| preset.oauth()))
        .ok_or_else(|| {
            Diagnostic::error(
                ARSY_PRV_1000,
                format!(
                    "the stored login for provider `{}` has expired and its OAuth client is no \
                     longer configured",
                    endpoint.id
                ),
                format!(
                    "restore the `[provider.endpoint.{}.oauth]` table",
                    endpoint.id
                ),
            )
        })?;
    // Checked before refreshing: a renewal that cannot be written back has
    // already spent the single-use refresh token it was made with.
    if handle.store() != FILE_STORE_ID {
        return Err(Diagnostic::error(
            ARSY_PRV_1000,
            format!(
                "the credential store `{}` does not support write-back",
                handle.store()
            ),
            "use a store that supports credential storage",
        ));
    }
    let refreshed = refresh_stored_login(
        &HttpTransport::default(),
        handle.name(),
        &oauth_client,
        &tokens,
    )
    .map_err(|error| {
        Diagnostic::error(
            ARSY_PRV_1000,
            format!(
                "the stored login for provider `{}` could not be renewed: {error}",
                endpoint.id
            ),
            sign_in_again(&endpoint.id),
        )
    })?;
    Ok((refreshed.access_token, CredentialSource::OAuth))
}

/// Renew a lapsed login kept in the file store, one process at a time.
///
/// An issuer that rotates refresh tokens (OpenAI does) honours each one once.
/// Two `arsy` processes renewing the same login together would each spend the
/// token they read, and whichever lost the race would be left holding one the
/// issuer already revoked — a forced re-login. So the renewal runs under an
/// exclusive lock on a file beside the credential, and re-reads the credential
/// once it has the lock: a process that waited finds the login another one
/// just renewed and uses it instead of spending the rotated token again.
///
/// `stale` is the login the caller found unusable: lapsed, or rejected by the
/// provider while its expiry still said otherwise. Only a stored login that
/// differs from it and is still in date counts as already renewed.
///
/// The lock is an OS file lock, released when the file closes or the process
/// dies, so a crashed run cannot leave the login locked.
fn refresh_stored_login(
    transport: &dyn WireTransport,
    name: &str,
    oauth_client: &arsy_kernel::config::OAuth,
    stale: &TokenSet,
) -> Result<TokenSet, String> {
    let _lock = lock_login(name)?;
    let current = FileCredentialStore
        .resolve(name)
        .ok()
        .and_then(|raw| serde_json::from_str::<TokenSet>(&raw).ok());
    let tokens = match current {
        Some(current)
            if current.access_token != stale.access_token && !current.is_expired(oauth::now()) =>
        {
            return Ok(current)
        }
        Some(current) => current,
        None => stale.clone(),
    };
    let refreshed =
        oauth::refresh(transport, oauth_client, &tokens).map_err(|error| error.to_string())?;
    // Written back before the lock is released and before use: a rotated
    // refresh token is single-use, so losing it would cost a re-login.
    let raw = serde_json::to_string(&refreshed).map_err(|error| error.to_string())?;
    FileCredentialStore
        .set(name, &raw)
        .map_err(|error| error.to_string())?;
    Ok(refreshed)
}

/// Renew a stored login the provider has just rejected, whatever its expiry
/// says: a token revoked or rotated elsewhere still looks in date on disk,
/// and re-reading it would only send the rejected token again. An error
/// means the login itself is gone and the operator has to sign in again.
pub(crate) fn renew_rejected_login(endpoint: &Endpoint) -> Result<(), String> {
    let handle = endpoint
        .credential
        .as_ref()
        .filter(|handle| handle.store() == FILE_STORE_ID)
        .ok_or("the login is not kept where ARSY can renew it")?;
    let client = endpoint
        .oauth
        .clone()
        .or_else(|| oauth::presets::get(&endpoint.id).map(|preset| preset.oauth()))
        .ok_or("its OAuth client is no longer configured")?;
    renew_rejected(&HttpTransport::default(), handle.name(), &client)
}

fn renew_rejected(
    transport: &dyn WireTransport,
    name: &str,
    client: &arsy_kernel::config::OAuth,
) -> Result<(), String> {
    let rejected = FileCredentialStore
        .resolve(name)
        .ok()
        .and_then(|raw| serde_json::from_str::<TokenSet>(&raw).ok())
        .ok_or("no stored login was found")?;
    refresh_stored_login(transport, name, client, &rejected).map(|_| ())
}

/// What to tell the operator when a provider's login can no longer be used:
/// what the provider said, why renewing it failed, and how to sign in again.
pub(crate) fn login_lost(provider: &str, refused: &str, cause: &str) -> String {
    format!(
        "the `{provider}` login is no longer valid — {refused}; renewing it failed: {cause}. {}",
        sign_in_again(provider)
    )
}

fn sign_in_again(provider: &str) -> String {
    format!("sign in again with `/auth` in the TUI, or `arsy auth login {provider}`")
}

/// Hold the lock that serialises every rewrite of one stored login, so a
/// renewal and a recorded project cannot overwrite each other.
fn lock_login(name: &str) -> Result<std::fs::File, String> {
    // `path` hands back a name that walks out of the secrets directory for
    // its caller to refuse; the lock beside it must not be created there.
    FileCredentialStore::check_name(name).map_err(|error| error.to_string())?;
    let path = FileCredentialStore::path(name)
        .ok_or("this platform has no user configuration directory")?;
    FileCredentialStore::prepare(&path).map_err(|error| error.to_string())?;
    let mut lock_path = path.into_os_string();
    lock_path.push(".lock");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(false);
    // Owner-only like the credential beside it: it holds nothing, but a
    // file in the secrets directory readable by others invites the question.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    // forgeguard: allow FG-SEC-007 -- beside a credential whose name `check_name` accepted above
    let lock = options
        .open(&lock_path)
        .map_err(|error| format!("cannot open the login lock: {error}"))?;
    // `mode` decides only a file this call creates; one an earlier build left
    // keeps whatever it had until it is set here.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        lock.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot restrict the login lock: {error}"))?;
    }
    lock.lock()
        .map_err(|error| format!("cannot take the login lock: {error}"))?;
    Ok(lock)
}

/// The Code Assist project a stored login already discovered.
fn stored_project(handle: Option<&SecretHandle>) -> Option<String> {
    let handle = handle?;
    if handle.store() != FILE_STORE_ID {
        return None;
    }
    let raw = FileCredentialStore.resolve(handle.name()).ok()?;
    serde_json::from_str::<TokenSet>(&raw).ok()?.project_id
}

/// Keep a discovered Code Assist project with the login it belongs to.
///
/// Best effort: a project that is not written down is discovered again on
/// the next run, which costs a round trip and nothing else.
fn remember_project(handle: Option<&SecretHandle>, project: &str) {
    let Some(handle) = handle else {
        return;
    };
    if handle.store() != FILE_STORE_ID {
        return;
    }
    let Ok(_lock) = lock_login(handle.name()) else {
        return;
    };
    let Some(mut tokens) = FileCredentialStore
        .resolve(handle.name())
        .ok()
        .and_then(|raw| serde_json::from_str::<TokenSet>(&raw).ok())
    else {
        return;
    };
    tokens.project_id = Some(project.to_owned());
    if let Ok(raw) = serde_json::to_string(&tokens) {
        let _ = FileCredentialStore.set(handle.name(), &raw);
    }
}

/// A handle to name the credential in redacted output.
///
/// The configured keyring handle when there is one, otherwise a synthetic
/// handle naming the variable it came from, so `[redacted:secret://env/...]`
/// still tells an operator which credential was masked.
fn redaction_handle(
    endpoint: &Endpoint,
    source: CredentialSource,
) -> Result<SecretHandle, Diagnostic> {
    match (source, &endpoint.credential) {
        (CredentialSource::File | CredentialSource::OAuth, Some(handle)) => Ok(handle.clone()),
        (_, _) => {
            let name = match source {
                CredentialSource::ConfiguredEnv => endpoint
                    .api_key_env
                    .as_deref()
                    .unwrap_or(endpoint.kind.default_api_key_env()),
                _ => endpoint.kind.default_api_key_env(),
            };
            SecretHandle::new("env", name).map_err(|error| credential_failed(&endpoint.id, error))
        }
    }
}

fn credential_failed(provider: &str, error: impl ToString) -> Diagnostic {
    Diagnostic::error(
        ARSY_PRV_1000,
        format!(
            "the credential for provider `{provider}` is unusable: {}",
            error.to_string()
        ),
        format!("run `arsy auth login {provider}`, or `arsy auth set {provider}` for an API key"),
    )
}

// -- Discovery -------------------------------------------------------------
//
// `arsy provider list` and `arsy model list` read configuration only. Neither
// contacts a provider, and neither resolves a credential: reporting which
// endpoints exist must not cost a keychain unlock or a billable request.

pub(crate) fn parse_list(arguments: &crate::ParsedArguments) -> Result<crate::Command, Diagnostic> {
    match arguments.positional.first().map(String::as_str) {
        Some("list") if arguments.positional.len() == 1 => {
            Ok(crate::Command::ProviderList { all: arguments.all })
        }
        _ => Err(crate::usage("provider requires `list` [--all]")),
    }
}

pub(crate) fn parse_models(
    arguments: &crate::ParsedArguments,
) -> Result<crate::Command, Diagnostic> {
    match arguments.positional.first().map(String::as_str) {
        Some("list") if arguments.positional.len() == 1 => Ok(crate::Command::ModelList {
            provider: arguments.provider.clone(),
            capability: arguments.capability.clone(),
        }),
        _ => Err(crate::usage(
            "model requires `list` [--provider <ID>] [--capability <NAME>]",
        )),
    }
}

pub(crate) fn list(
    invocation: &crate::Invocation,
    all: bool,
    emitter: &mut crate::Emitter,
) -> Result<i32, Diagnostic> {
    let config = configuration(invocation)?;
    let report = provider_report(&config, all);
    emitter.result(if emitter.output == crate::Output::Json {
        report
    } else {
        serde_json::json!({"providers": human_providers(&report)})
    });
    Ok(0)
}

fn provider_report(config: &Config, all: bool) -> serde_json::Value {
    let ceiling = config.provider_allowed();
    let providers: Vec<_> = config
        .all_endpoints()
        .filter(|endpoint| all || config.provider_is_allowed(&endpoint.id))
        .map(|endpoint| {
            serde_json::json!({
                "id": endpoint.id,
                "kind": endpoint.kind.as_str(),
                "base_url": endpoint.base_url,
                "allowed": config.provider_is_allowed(&endpoint.id),
                "default": config.provider_default() == Some(endpoint.id.as_str()),
                "models": endpoint.models,
                "credential": endpoint.credential.as_ref().map(ToString::to_string),
                "api_key_env": endpoint.api_key_env,
                "oauth": endpoint.oauth.is_some(),
            })
        })
        .collect();
    serde_json::json!({
        "providers": providers,
        "ceiling": ceiling.map(|allowed| allowed.iter().cloned().collect::<Vec<_>>()),
        "includes_disallowed": all,
    })
}

pub(crate) fn models(
    invocation: &crate::Invocation,
    provider: Option<&str>,
    capability: Option<&str>,
    emitter: &mut crate::Emitter,
) -> Result<i32, Diagnostic> {
    let config = configuration(invocation)?;
    let report = model_report(&config, provider, capability)?;
    emitter.result(if emitter.output == crate::Output::Json {
        report
    } else {
        serde_json::json!({"models": human_models(&report)})
    });
    Ok(0)
}

fn model_report(
    config: &Config,
    provider: Option<&str>,
    capability: Option<&str>,
) -> Result<serde_json::Value, Diagnostic> {
    if let Some(name) = capability {
        if !arsy_kernel::model_profile::DECLARED_CAPABILITIES.contains(&name) {
            return Err(crate::usage(format!(
                "`{name}` is not a declared capability; this build declares {}",
                arsy_kernel::model_profile::DECLARED_CAPABILITIES.join(", ")
            )));
        }
    }
    let mut models = Vec::new();
    for endpoint in config.all_endpoints() {
        if provider.is_some_and(|id| id != endpoint.id) || !config.provider_is_allowed(&endpoint.id)
        {
            continue;
        }
        for model in &endpoint.models {
            let output_tokens = endpoint.output_tokens_for(model);
            let declared = arsy_kernel::model_profile::declared(Some(u64::from(output_tokens)));
            if !config.model_is_allowed(model) {
                continue;
            }
            if capability.is_some_and(|name| {
                declared.get(name).map(|value| value.state)
                    != Some(arsy_kernel::model_profile::CapabilityState::Supported)
            }) {
                continue;
            }
            models.push(serde_json::json!({
                "provider": endpoint.id,
                "model": model,
                "kind": endpoint.kind.as_str(),
                "default": endpoint.model.as_deref() == Some(model.as_str()),
                "max_output_tokens": output_tokens,
                "capabilities": declared,
            }));
        }
    }
    Ok(serde_json::json!({
        "models": models,
        "provider_filter": provider,
        "capability_filter": capability,
        "ceiling": config
            .model_allowed()
            .map(|allowed| allowed.iter().cloned().collect::<Vec<_>>()),
    }))
}

pub(crate) fn configuration(invocation: &crate::Invocation) -> Result<Config, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    crate::load_config(&root, &working, invocation.config.as_deref())
}

fn human_providers(report: &serde_json::Value) -> String {
    let providers = report["providers"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    if providers.is_empty() {
        return "No provider endpoint is configured.".to_owned();
    }
    let mut text = format!("{} provider endpoint(s)\n", providers.len());
    for provider in providers {
        text.push_str(&format!(
            "\n  {}{} · {} · {}\n    models: {}\n",
            provider["id"].as_str().unwrap_or("?"),
            if provider["default"] == serde_json::Value::Bool(true) {
                " (default)"
            } else {
                ""
            },
            provider["kind"].as_str().unwrap_or("?"),
            provider["base_url"].as_str().unwrap_or("?"),
            provider["models"]
                .as_array()
                .map(|models| models
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "))
                .filter(|listed| !listed.is_empty())
                .unwrap_or_else(|| "none configured".to_owned()),
        ));
        if provider["allowed"] == serde_json::Value::Bool(false) {
            text.push_str("    excluded by the provider.allowed ceiling\n");
        }
    }
    if let Some(ceiling) = report["ceiling"].as_array() {
        text.push_str(&format!(
            "\nceiling: provider.allowed = [{}]\n",
            ceiling
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    text
}

fn human_models(report: &serde_json::Value) -> String {
    let models = report["models"].as_array().map_or(&[][..], Vec::as_slice);
    if models.is_empty() {
        return "No allowed model is configured for the requested filter.".to_owned();
    }
    let mut text = format!("{} allowed model(s)\n", models.len());
    for model in models {
        text.push_str(&format!(
            "\n  {}/{}{}\n",
            model["provider"].as_str().unwrap_or("?"),
            model["model"].as_str().unwrap_or("?"),
            if model["default"] == serde_json::Value::Bool(true) {
                " (default)"
            } else {
                ""
            },
        ));
        if let Some(capabilities) = model["capabilities"].as_object() {
            for (name, capability) in capabilities {
                text.push_str(&format!(
                    "    {name}: {} ({}, observed {})\n",
                    capability["state"].as_str().unwrap_or("?"),
                    capability["source"].as_str().unwrap_or("?"),
                    match capability["observed_at_unix_seconds"].as_u64() {
                        Some(0) | None => "never".to_owned(),
                        Some(seconds) => crate::session::timestamp(&serde_json::json!(
                            seconds.saturating_mul(1000)
                        )),
                    }
                ));
            }
        }
    }
    text
}

/// Fetch the live model list for a configured endpoint.
///
/// Returns the model IDs and the endpoint's config-file name (for the write
/// step), or an error string the caller can surface as a dialog notice.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DiscoveredModels {
    pub models: Vec<String>,
    pub context_windows: std::collections::BTreeMap<String, u32>,
    pub input_limits: std::collections::BTreeMap<String, u32>,
    pub output_limits: std::collections::BTreeMap<String, u32>,
}

#[cfg(feature = "tui")]
pub(crate) fn fetch_endpoint_models(
    invocation: &crate::Invocation,
    endpoint_id: &str,
) -> Result<(String, Vec<String>), String> {
    let config = configuration(invocation).map_err(|d| d.message)?;
    let endpoint = config
        .all_endpoints()
        .find(|e| e.id == endpoint_id)
        .ok_or_else(|| format!("endpoint `{endpoint_id}` not found in config"))?
        .clone();
    let api_key = credential(&endpoint, &from_env).ok().map(|(k, _)| k);
    let models = fetch_models_http(
        &HttpTransport::default(),
        endpoint.kind.as_str(),
        &endpoint.base_url,
        api_key.as_deref(),
    )
    .ok_or_else(|| format!("could not fetch models for `{endpoint_id}`"))?;
    Ok((endpoint.id, models.models))
}

/// Fetch the model list for an OAuth preset using its stored credential.
/// Returns `None` if the token is absent, the fetch fails, or the dialect does
/// not support discovery. Expired tokens are refreshed and written back before
/// the fetch is attempted, so a background `/model` open does not fail silently.
#[cfg(feature = "tui")]
pub(crate) fn fetch_oauth_preset_models(
    preset: &arsy_kernel::oauth::presets::Preset,
) -> Option<Vec<String>> {
    fetch_preset_models(preset, true)
}

/// The same fetch, but only with a token that is still valid.
///
/// For a refresh nobody asked for: refreshing rotates the refresh token, and
/// one taken in the background while a turn refreshes the same credential
/// could leave the operator holding a token the provider already revoked.
#[cfg(feature = "tui")]
pub(crate) fn fetch_oauth_preset_models_quietly(
    preset: &arsy_kernel::oauth::presets::Preset,
) -> Option<Vec<String>> {
    fetch_preset_models(preset, false)
}

#[cfg(feature = "tui")]
fn fetch_preset_models(
    preset: &arsy_kernel::oauth::presets::Preset,
    may_refresh: bool,
) -> Option<Vec<String>> {
    let handle_name = format!("{}.key", preset.id);
    let raw = FileCredentialStore.resolve(&handle_name).ok()?;
    let tokens = serde_json::from_str::<TokenSet>(&raw).ok()?;
    let expired = tokens.is_expired(oauth::now());
    if expired && !may_refresh {
        return None;
    }
    let access_token = if expired {
        refresh_stored_login(
            &HttpTransport::default(),
            &handle_name,
            &preset.oauth(),
            &tokens,
        )
        .ok()?
        .access_token
    } else {
        tokens.access_token
    };
    fetch_models_http(
        &HttpTransport::default(),
        preset.dialect.as_str(),
        preset.base_url,
        Some(&access_token),
    )
    .map(|found| found.models)
}

const CODEX_CLIENT_VERSION: &str = "0.156.1";

fn fetch_models_http(
    transport: &HttpTransport,
    kind: &str,
    base_url: &str,
    api_key: Option<&str>,
) -> Option<DiscoveredModels> {
    let bearer = |headers: &mut Vec<(String, String)>| {
        if let Some(token) = api_key {
            headers.push(("Authorization".to_owned(), format!("Bearer {token}")));
        }
    };
    match kind {
        "anthropic" => {
            let mut headers = vec![("anthropic-version".to_owned(), "2023-06-01".to_owned())];
            let token = api_key?;
            if token.starts_with("sk-ant-at") || !token.starts_with("sk-ant-api") {
                headers.push(("authorization".to_owned(), format!("Bearer {token}")));
                headers.push((
                    "anthropic-beta".to_owned(),
                    "oauth-2025-04-20,claude-code-20250219".to_owned(),
                ));
            } else {
                headers.push(("x-api-key".to_owned(), token.to_owned()));
            }
            let body = fetch_json(transport.get(
                format!("{}/v1/models?limit=1000", base_url.trim_end_matches('/')),
                headers,
            ))?;
            extract_models(&body["data"], "id")
        }
        "openai_responses" => {
            let url = format!(
                "{}/models?client_version={CODEX_CLIENT_VERSION}",
                base_url.trim_end_matches('/')
            );
            let mut headers = vec![
                ("accept".to_owned(), "application/json".to_owned()),
                (
                    "OpenAI-Beta".to_owned(),
                    "responses=experimental".to_owned(),
                ),
                ("originator".to_owned(), "omp".to_owned()),
                ("version".to_owned(), CODEX_CLIENT_VERSION.to_owned()),
            ];
            bearer(&mut headers);
            let body = fetch_json(transport.get(url, headers))?;
            let arr = body.get("models").or_else(|| body.get("data"))?;
            let visible: Vec<serde_json::Value> = arr
                .as_array()?
                .iter()
                .filter(|model| {
                    !matches!(
                        model.get("visibility").and_then(|value| value.as_str()),
                        Some("hide" | "hidden")
                    )
                })
                .cloned()
                .collect();
            extract_models(&serde_json::Value::Array(visible), "slug")
        }
        "google_code_assist" => {
            let url = format!(
                "{}/v1internal:fetchAvailableModels",
                base_url.trim_end_matches('/')
            );
            let mut headers = vec![
                ("Content-Type".to_owned(), "application/json".to_owned()),
                ("User-Agent".to_owned(), antigravity_user_agent()),
            ];
            bearer(&mut headers);
            let body = fetch_json(transport.send(WireRequest {
                url,
                headers,
                body: "{}".to_owned(),
            }))?;
            let models = body.get("models")?.as_object()?;
            let mut ids: Vec<String> = models
                .iter()
                .filter(|(_, value)| {
                    value.get("isInternal").and_then(|value| value.as_bool()) != Some(true)
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.sort();
            let context_windows = models
                .iter()
                .filter(|(_, model)| {
                    model.get("isInternal").and_then(|value| value.as_bool()) != Some(true)
                })
                .filter_map(|(id, model)| {
                    numeric_limit(model, CONTEXT_WINDOW_FIELDS)
                        .or_else(|| numeric_limit(model, &["maxTokens"]))
                        .map(|window| (id.clone(), window))
                })
                .collect();
            let input_limits = models
                .iter()
                .filter(|(_, model)| {
                    model.get("isInternal").and_then(|value| value.as_bool()) != Some(true)
                })
                .filter_map(|(id, model)| {
                    numeric_limit(model, &["max_input_tokens", "inputTokenLimit"])
                        .map(|limit| (id.clone(), limit))
                })
                .collect();
            let output_limits = models
                .iter()
                .filter(|(_, model)| {
                    model.get("isInternal").and_then(|value| value.as_bool()) != Some(true)
                })
                .filter_map(|(id, model)| {
                    numeric_limit(model, &["maxOutputTokens", "outputTokenLimit"])
                        .map(|limit| (id.clone(), limit))
                })
                .collect();
            (!ids.is_empty()).then_some(DiscoveredModels {
                models: ids,
                context_windows,
                input_limits,
                output_limits,
            })
        }
        _ => {
            let mut headers = Vec::new();
            bearer(&mut headers);
            let body = fetch_json(transport.get(
                format!("{}/models", base_url.trim_end_matches('/')),
                headers,
            ))?;
            extract_models(&body["data"], "id")
        }
    }
}

fn fetch_json(
    response: Result<WireResponse, arsy_kernel::provider::ProviderError>,
) -> Option<serde_json::Value> {
    let body = response
        .ok()?
        .lines
        .collect::<Result<Vec<_>, _>>()
        .ok()?
        .join("\n");
    serde_json::from_str(&body).ok()
}

/// OpenAI-compatible proxies name the window differently: `context_length`
/// (OpenRouter, LiteLLM) and `max_model_len` (vLLM) mean the same limit.
const CONTEXT_WINDOW_FIELDS: &[&str] = &["context_window", "context_length", "max_model_len"];

fn numeric_limit(model: &serde_json::Value, fields: &[&str]) -> Option<u32> {
    fields.iter().find_map(|field| {
        model
            .get(field)
            .and_then(serde_json::Value::as_u64)
            .and_then(|window| u32::try_from(window).ok())
            .filter(|window| *window > 0)
    })
}

fn extract_models(data: &serde_json::Value, id_key: &str) -> Option<DiscoveredModels> {
    let mut context_windows = std::collections::BTreeMap::new();
    let mut input_limits = std::collections::BTreeMap::new();
    let mut output_limits = std::collections::BTreeMap::new();
    let ids: Vec<String> = data
        .as_array()?
        .iter()
        .filter_map(|model| {
            let id = model.get(id_key).or_else(|| model.get("id"))?.as_str()?;
            if let Some(window) = numeric_limit(model, CONTEXT_WINDOW_FIELDS) {
                context_windows.insert(id.to_owned(), window);
            }
            if let Some(limit) = numeric_limit(model, &["max_input_tokens", "inputTokenLimit"]) {
                input_limits.insert(id.to_owned(), limit);
            }
            if let Some(limit) = numeric_limit(
                model,
                &["max_output_tokens", "maxOutputTokens", "outputTokenLimit"],
            ) {
                output_limits.insert(id.to_owned(), limit);
            }
            Some(id.to_owned())
        })
        .collect();
    if ids.is_empty() {
        None
    } else {
        Some(DiscoveredModels {
            models: ids,
            context_windows,
            input_limits,
            output_limits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::config::Layer;

    /// Counts token requests and answers each one slowly with a fresh token,
    /// so concurrent renewals overlap unless something serialises them.
    struct CountingIssuer(std::sync::atomic::AtomicUsize);

    impl WireTransport for CountingIssuer {
        fn send(
            &self,
            _request: WireRequest,
        ) -> Result<WireResponse, arsy_kernel::provider::ProviderError> {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            std::thread::sleep(std::time::Duration::from_millis(50));
            let body = format!(
                r#"{{"access_token":"at-{n}","refresh_token":"rt-{n}","expires_in":3600}}"#
            );
            Ok(WireResponse {
                status: 200,
                headers: Vec::new(),
                lines: Box::new(std::iter::once(Ok(body))),
            })
        }
    }

    #[test]
    fn concurrent_renewals_of_one_login_spend_its_refresh_token_once() {
        let root = std::env::temp_dir().join(format!("arsy-refresh-lock-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let name = root.join("codex-oauth.key").display().to_string();
        let stale = TokenSet {
            access_token: "at-0".to_owned(),
            refresh_token: Some("rt-0".to_owned()),
            expires_at: Some(1),
            id_token: None,
            project_id: None,
        };
        FileCredentialStore
            .set(&name, &serde_json::to_string(&stale).unwrap())
            .unwrap();
        let issuer = Arc::new(CountingIssuer(Default::default()));
        let client = arsy_kernel::config::OAuth::default();

        let renewals: Vec<_> = (0..4)
            .map(|_| {
                let (issuer, name, client, stale) =
                    (issuer.clone(), name.clone(), client.clone(), stale.clone());
                std::thread::spawn(move || {
                    refresh_stored_login(issuer.as_ref(), &name, &client, &stale).unwrap()
                })
            })
            .collect();
        let tokens: Vec<TokenSet> = renewals.into_iter().map(|t| t.join().unwrap()).collect();

        assert_eq!(issuer.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(tokens.iter().all(|t| t.access_token == "at-1"));
        let kept: TokenSet =
            serde_json::from_str(&FileCredentialStore.resolve(&name).unwrap()).unwrap();
        assert_eq!(kept.refresh_token.as_deref(), Some("rt-1"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Refuses every refresh the way an issuer refuses a revoked login.
    struct RefusingIssuer;

    impl WireTransport for RefusingIssuer {
        fn send(
            &self,
            _request: WireRequest,
        ) -> Result<WireResponse, arsy_kernel::provider::ProviderError> {
            Ok(WireResponse {
                status: 400,
                headers: Vec::new(),
                lines: Box::new(std::iter::once(Ok(
                    r#"{"error":"invalid_grant","error_description":"refresh token revoked"}"#
                        .to_owned(),
                ))),
            })
        }
    }

    fn stored_login(test: &str, expires_at: u64) -> (std::path::PathBuf, String) {
        let root = std::env::temp_dir().join(format!("arsy-{test}-{}", std::process::id()));
        // forgeguard: allow FG-SEC-007 -- a test's own directory under the system temp dir
        std::fs::create_dir_all(&root).unwrap();
        let name = root.join("codex-oauth.key").display().to_string();
        let tokens = TokenSet {
            access_token: "at-0".to_owned(),
            refresh_token: Some("rt-0".to_owned()),
            expires_at: Some(expires_at),
            id_token: None,
            project_id: None,
        };
        FileCredentialStore
            .set(&name, &serde_json::to_string(&tokens).unwrap())
            .unwrap();
        (root, name)
    }

    #[test]
    fn a_login_the_provider_rejected_is_renewed_even_while_it_looks_in_date() {
        let (root, name) = stored_login("rejected-renewed", oauth::now() + 3600);
        let issuer = CountingIssuer(Default::default());
        renew_rejected(&issuer, &name, &arsy_kernel::config::OAuth::default()).unwrap();
        assert_eq!(issuer.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        let kept: TokenSet =
            serde_json::from_str(&FileCredentialStore.resolve(&name).unwrap()).unwrap();
        assert_eq!(kept.access_token, "at-1");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_login_that_cannot_be_renewed_tells_the_operator_to_sign_in_again() {
        let (root, name) = stored_login("rejected-lost", oauth::now() + 3600);
        let cause = renew_rejected(
            &RefusingIssuer,
            &name,
            &arsy_kernel::config::OAuth::default(),
        )
        .unwrap_err();
        assert!(cause.contains("invalid_grant"), "{cause}");
        let message = login_lost("codex-oauth", "token revoked", &cause);
        assert!(message.contains("`codex-oauth` login is no longer valid"));
        assert!(message.contains("/auth") && message.contains("arsy auth login codex-oauth"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_login_lock_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("arsy-lock-mode-{}", std::process::id()));
        // forgeguard: allow FG-SEC-007 -- a test's own directory under the system temp dir
        std::fs::create_dir_all(&root).unwrap();
        let name = root.join("codex-oauth.key").display().to_string();
        // One an earlier build left readable by others is tightened too.
        std::fs::write(format!("{name}.lock"), "").unwrap();
        std::fs::set_permissions(
            format!("{name}.lock"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        drop(lock_login(&name).unwrap());
        let mode = std::fs::metadata(format!("{name}.lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_login_lock_is_never_created_outside_the_secrets_directory() {
        assert!(lock_login("../escaped.key").is_err());
    }

    #[test]
    fn a_discovered_project_is_kept_with_its_login_and_survives_renewal() {
        let root = std::env::temp_dir().join(format!("arsy-login-project-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let name = root.join("antigravity.key").display().to_string();
        let handle = SecretHandle::new(FILE_STORE_ID, &name).unwrap();
        let stale = TokenSet {
            access_token: "at-0".to_owned(),
            refresh_token: Some("rt-0".to_owned()),
            expires_at: Some(1),
            id_token: None,
            project_id: None,
        };
        FileCredentialStore
            .set(&name, &serde_json::to_string(&stale).unwrap())
            .unwrap();

        assert_eq!(stored_project(Some(&handle)), None);
        remember_project(Some(&handle), "proj-5");
        assert_eq!(stored_project(Some(&handle)).as_deref(), Some("proj-5"));

        let issuer = CountingIssuer(Default::default());
        let client = arsy_kernel::config::OAuth::default();
        refresh_stored_login(&issuer, &name, &client, &stale).unwrap();
        assert_eq!(stored_project(Some(&handle)).as_deref(), Some("proj-5"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(feature = "tui")]
    #[test]
    fn discovery_keeps_provider_context_windows_by_model() {
        let data = serde_json::json!([
            {"slug":"small", "context_window":128000, "max_output_tokens":16384},
            {"slug":"large", "context_window":1000000, "maxOutputTokens":65536},
            {"slug":"claude", "max_input_tokens":1000000},
            {"slug":"gemini", "inputTokenLimit":128000},
            {"slug":"proxy", "context_length":200000},
            {"slug":"vllm", "max_model_len":32768, "outputTokenLimit":2048},
            {"slug":"unknown"}
        ]);
        let found = extract_models(&data, "slug").unwrap();
        assert_eq!(
            found.models,
            ["small", "large", "claude", "gemini", "proxy", "vllm", "unknown"]
        );
        assert_eq!(found.context_windows.get("proxy"), Some(&200_000));
        assert_eq!(found.context_windows.get("vllm"), Some(&32_768));
        assert_eq!(found.context_windows.get("small"), Some(&128_000));
        assert_eq!(found.context_windows.get("large"), Some(&1_000_000));
        assert_eq!(found.input_limits.get("claude"), Some(&1_000_000));
        assert_eq!(found.input_limits.get("gemini"), Some(&128_000));
        assert_eq!(found.output_limits.get("small"), Some(&16_384));
        assert_eq!(found.output_limits.get("large"), Some(&65_536));
        assert_eq!(found.output_limits.get("vllm"), Some(&2_048));
        assert!(!found.context_windows.contains_key("unknown"));
    }

    /// A configuration file holding `body`.
    ///
    /// The cases are written as TOML and converted, because the schema reads
    /// more clearly that way than as quoted JSON; what reaches disk, and what
    /// the loader sees, is the `arsy.json` a real run reads.
    fn config(body: &str) -> Config {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(arsy_kernel::config::CONFIG_FILE);
        let json = arsy_kernel::config::json_from_toml(body, &path).unwrap();
        std::fs::write(&path, json).unwrap();
        Config::load(&[(Layer::User, path)]).unwrap()
    }

    /// An environment holding exactly one variable.
    fn env(name: &'static str, value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |asked| (asked == name).then(|| value.to_owned())
    }

    /// The keychain is one store among the handle's choices, not the place
    /// every handle ends up.
    #[test]
    fn the_handle_decides_which_store_answers_and_an_unknown_one_is_refused() {
        use std::io::Write;

        let directory = tempfile::tempdir().unwrap();
        let key = directory.path().join("myai.key");
        let mut file = std::fs::File::create(&key).unwrap();
        file.write_all(b"sk-from-a-file\n").unwrap();
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let endpoint_config = |handle: String| {
            // A literal string: a Windows path carries backslashes, and TOML
            // reads \U in a basic string as the start of a unicode escape.
            config(&format!(
                r#"
schema_version = 1
[provider.endpoint.myai]
kind = "openai"
base_url = "https://example.test/v1"
credential = '{handle}'
"#
            ))
        };

        // A file handle is answered by the file, and reported as the file, so
        // `arsy doctor` does not claim a keychain that was never opened.
        let config = endpoint_config(format!("secret://file/{}", key.display()));
        let endpoint = config.endpoint(None).unwrap();
        assert_eq!(
            credential(endpoint, &env("UNUSED", "")).unwrap(),
            ("sk-from-a-file".to_owned(), CredentialSource::File)
        );

        // A store ARSY does not have must not quietly become the keychain.
        let config = endpoint_config("secret://vault/myai".to_owned());
        let endpoint = config.endpoint(None).unwrap();
        let error = credential(endpoint, &env("UNUSED", "")).unwrap_err();
        assert!(
            error.message.contains("vault"),
            "an unknown store did not name itself: {}",
            error.message
        );
    }

    #[test]
    fn a_named_environment_variable_wins_and_reports_its_source() {
        let config = config(
            r#"
schema_version = 1
[provider.endpoint.local]
kind = "openai"
base_url = "http://localhost:11434/v1"
api_key_env = "LOCAL_KEY"
"#,
        );
        let endpoint = config.endpoint(None).unwrap();

        assert_eq!(
            credential(endpoint, &env("LOCAL_KEY", "sk-from-env")).unwrap(),
            ("sk-from-env".to_owned(), CredentialSource::ConfiguredEnv)
        );
        assert_eq!(
            credential(endpoint, &env("OPENAI_API_KEY", "sk-conventional"))
                .unwrap()
                .1,
            CredentialSource::DefaultEnv,
            "the dialect's conventional variable is the last resort"
        );

        // Set but empty is treated as unset, so a blank export falls through
        // instead of failing at the wire as an authentication error.
        let error = credential(endpoint, &env("LOCAL_KEY", "")).unwrap_err();
        assert_eq!(error.code, ARSY_PRV_1000);
        assert!(
            error.remediation.contains("LOCAL_KEY"),
            "the remediation names the variable the operator configured: {}",
            error.remediation
        );
    }

    #[test]
    fn a_credential_is_named_for_redaction_whatever_source_it_came_from() {
        let config = config(
            r#"
schema_version = 1
[provider.endpoint.local]
kind = "openai"
api_key_env = "LOCAL_KEY"
credential = "secret://file/local.key"
"#,
        );
        let endpoint = config.endpoint(None).unwrap();

        assert_eq!(
            redaction_handle(endpoint, CredentialSource::File)
                .unwrap()
                .to_string(),
            "secret://file/local.key"
        );
        assert_eq!(
            redaction_handle(endpoint, CredentialSource::OAuth)
                .unwrap()
                .to_string(),
            "secret://file/local.key",
            "an access token is masked under the handle it was stored against"
        );
        assert_eq!(
            redaction_handle(endpoint, CredentialSource::ConfiguredEnv)
                .unwrap()
                .to_string(),
            "secret://env/LOCAL_KEY",
            "a key from the environment is masked too, not only a stored one"
        );
        assert_eq!(
            redaction_handle(endpoint, CredentialSource::DefaultEnv)
                .unwrap()
                .to_string(),
            "secret://env/OPENAI_API_KEY"
        );
    }

    #[test]
    fn a_stored_credential_is_told_apart_by_shape_not_by_a_second_lookup() {
        let now = 1_000_000;
        let token = |body: &str| classify(body.to_owned(), now);

        assert_eq!(
            token("sk-ant-api03-plain-key"),
            Stored::ApiKey("sk-ant-api03-plain-key".to_owned())
        );
        assert_eq!(
            token(r#"{"note":"not a login"}"#),
            Stored::ApiKey(r#"{"note":"not a login"}"#.to_owned()),
            "JSON that is not a token set is still an API key, taken verbatim"
        );

        let live = format!(r#"{{"access_token":"at","expires_at":{}}}"#, now + 3600);
        assert!(matches!(token(&live), Stored::Token(_)));

        assert!(
            matches!(token(r#"{"access_token":"at"}"#), Stored::Token(_)),
            "a login with no stated expiry is taken at face value, not refreshed every time"
        );

        assert!(matches!(
            token(&format!(
                r#"{{"access_token":"at","expires_at":{}}}"#,
                now - 1
            )),
            Stored::Expired(_)
        ));
        assert!(
            matches!(
                token(&format!(
                    r#"{{"access_token":"at","expires_at":{}}}"#,
                    now + oauth::EXPIRY_MARGIN.as_secs() - 1
                )),
                Stored::Expired(_)
            ),
            "a token inside the margin is renewed, so it cannot lapse between check and use"
        );
    }

    #[test]
    fn an_unconfigured_or_misnamed_provider_is_a_diagnostic_not_a_fallback() {
        let empty = config("schema_version = 1\n");
        assert_eq!(
            resolve(&empty, None).err().map(|error| error.code),
            Some(ARSY_PRV_1000.to_owned())
        );

        let one = config(
            r#"
schema_version = 1
[provider.endpoint.local]
kind = "openai"
"#,
        );
        let message = resolve(&one, Some("typo")).err().unwrap().message;
        assert!(
            message.contains("`typo`"),
            "a misspelled provider must not silently resolve to another one: {message}"
        );
    }

    #[test]
    fn a_provider_ceiling_hides_an_endpoint_and_narrows_the_default() {
        let capped = config(
            r#"
schema_version = 1

[provider]
default = "second"
allowed = ["first"]

[provider.endpoint.first]
kind = "openai"
model = "m1"
models = ["m1", "m2"]

[provider.endpoint.second]
kind = "anthropic"
model = "m3"
"#,
        );
        // The default names an endpoint the ceiling excludes, so nothing
        // resolves: a ceiling that silently fell back to another provider would
        // send prompts somewhere the operator capped out.
        assert!(capped.endpoint(None).is_none());
        assert!(capped.endpoint(Some("second")).is_none());
        assert_eq!(
            capped.endpoint(Some("first")).map(|e| e.id.as_str()),
            Some("first")
        );

        let listed = provider_report(&capped, false);
        assert_eq!(listed["providers"].as_array().unwrap().len(), 1);
        assert_eq!(listed["providers"][0]["id"], "first");
        assert_eq!(listed["ceiling"], serde_json::json!(["first"]));

        // `--all` shows what was excluded, and says so.
        let every = provider_report(&capped, true);
        let excluded: Vec<_> = every["providers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["allowed"] == serde_json::Value::Bool(false))
            .map(|entry| entry["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(excluded, vec!["second".to_owned()]);
    }

    #[test]
    fn model_listing_applies_both_ceilings_and_the_capability_filter() {
        let capped = config(
            r#"
schema_version = 1

[provider.endpoint.first]
kind = "openai"
model = "m1"
models = ["m1", "m2"]
[provider.endpoint.first.output_limits]
m1 = 16384
m2 = 32768

[model]
allowed = ["m1"]
"#,
        );
        let report = model_report(&capped, None, None).unwrap();
        let models: Vec<_> = report["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["model"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(models, vec!["m1".to_owned()], "model.allowed is a ceiling");
        assert_eq!(report["models"][0]["max_output_tokens"], 16_384);
        assert_eq!(
            report["models"][0]["capabilities"]["streaming"]["state"],
            "supported"
        );

        // A provider filter that names nothing configured lists nothing rather
        // than everything.
        assert!(
            model_report(&capped, Some("absent"), None).unwrap()["models"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(model_report(&capped, None, Some("telepathy")).is_err());
        assert_eq!(
            model_report(&capped, None, Some("tool_calls")).unwrap()["models"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    /// `provider.default = "auto"` is the documented default, so several
    /// configured endpoints must resolve to one deterministically rather than
    /// to "no provider is configured".
    #[test]
    fn auto_routes_between_allowed_endpoints_and_never_outside_them() {
        let several = config(
            r#"
schema_version = 1

[provider]
default = "auto"

[provider.endpoint.zeta]
kind = "openai"
model = "z1"

[provider.endpoint.alpha]
kind = "anthropic"
model = "a1"
"#,
        );
        let (endpoint, decision) = resolve_with_route(&several, None).unwrap();
        assert_eq!(
            endpoint.id, "alpha",
            "the tie-break is stable, not arbitrary"
        );
        let decision = decision.expect("routing chose it");
        assert_eq!(decision.key().unwrap().to_string(), "alpha/a1");
        assert!(
            matches!(&decision, routing::Decision::Routed { reasons, .. }
            if reasons.iter().any(|reason| reason.contains("nothing has been measured")))
        );

        // Naming one explicitly bypasses routing entirely.
        let (endpoint, decision) = resolve_with_route(&several, Some("zeta")).unwrap();
        assert_eq!(endpoint.id, "zeta");
        assert!(
            decision.is_none(),
            "an explicit name is not a routed choice"
        );

        // A ceiling narrows what routing may pick, and routing stays inside it.
        let capped = config(
            r#"
schema_version = 1

[provider]
default = "auto"
allowed = ["zeta"]

[provider.endpoint.zeta]
kind = "openai"
model = "z1"

[provider.endpoint.alpha]
kind = "anthropic"
model = "a1"
"#,
        );
        let (endpoint, decision) = resolve_with_route(&capped, None).unwrap();
        assert_eq!(endpoint.id, "zeta");
        let decision = decision.expect("an unnamed choice is always routed");
        assert_eq!(
            decision.excluded().len(),
            1,
            "the capped endpoint is reported as excluded, not silently dropped"
        );

        // A named provider that does not exist is still an error: routing must
        // never quietly substitute another one.
        assert_eq!(
            resolve_with_route(&capped, Some("alpha"))
                .err()
                .map(|error| error.code),
            Some(ARSY_PRV_1000.to_owned())
        );

        // Every model excluded leaves nothing to route to, and says so.
        let impossible = config(
            r#"
schema_version = 1

[provider]
default = "auto"

[provider.endpoint.zeta]
kind = "openai"
model = "z1"

[model]
allowed = ["nothing-like-it"]
"#,
        );
        let error = resolve_with_route(&impossible, None).expect_err("nothing is routable");
        assert!(
            error.message.contains("no provider could be routed to"),
            "{}",
            error.message
        );
    }
}
