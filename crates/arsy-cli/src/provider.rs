//! Turning configuration into a usable model provider.
//!
//! Configuration names an endpoint; the OS keyring and the environment hold
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
        anthropic::AnthropicProvider, google_code_assist::GoogleCodeAssistProvider,
        http::HttpTransport, openai::OpenAiProvider, openai_responses::OpenAiResponsesProvider,
        wire::ApiKey, ModelProvider,
    },
    routing,
    secret::{
        CredentialStore, FileCredentialStore, OsCredentialStore, Redactor, SecretError,
        SecretHandle, FILE_STORE_ID, OS_STORE_ID,
    },
};
use std::sync::Arc;

/// Where a credential came from. Reported by `arsy doctor` so an operator can
/// tell a keyring entry from an inherited environment variable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialSource {
    /// The endpoint needs none: a replay reads a file.
    None,
    ConfiguredEnv,
    Keyring,
    File,
    OAuth,
    DefaultEnv,
}

impl CredentialSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ConfiguredEnv => "configured_env",
            Self::Keyring => "keyring",
            Self::File => "file",
            Self::OAuth => "oauth",
            Self::DefaultEnv => "default_env",
        }
    }

    /// Where a stored key actually came from, so `arsy doctor` names the store
    /// that answered rather than assuming the keychain did.
    fn stored_in(store: &str) -> Self {
        match store {
            FILE_STORE_ID => Self::File,
            _ => Self::Keyring,
        }
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

/// Build the provider for `requested`, or for the configured default.
///
/// The credential is looked for in the order an operator would expect to
/// override it: an environment variable the config names, then the keyring
/// entry the config names, then the dialect's conventional variable. The first
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
                credential: arsy_kernel::secret::SecretHandle::new(
                    arsy_kernel::secret::OS_STORE_ID,
                    canonical_id,
                )
                .ok(),
                api_key_env: None,
                model: preset.models.first().map(|s| (*s).to_owned()),
                models: preset.models.iter().map(|s| (*s).to_owned()).collect(),
                max_output_tokens: arsy_kernel::config::DEFAULT_MAX_OUTPUT_TOKENS,
                oauth: Some(preset.oauth()),
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
            Some(routing::Candidate {
                key: arsy_kernel::provider::ModelKey {
                    provider: endpoint.id.clone(),
                    model,
                },
                capabilities: arsy_kernel::model_profile::declared(Some(u64::from(
                    endpoint.max_output_tokens,
                ))),
                residency: None,
                cost_micros_per_1k: None,
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
    // not only the keyring. A value too short to redact safely is refused
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
                .with_redactor(redactor),
        ),
        Dialect::OpenaiResponses => Arc::new(
            OpenAiResponsesProvider::with_base_url(&endpoint.base_url, key, transport)
                .with_id(&endpoint.id)
                .with_redactor(redactor),
        ),
        Dialect::GoogleCodeAssist => Arc::new(
            GoogleCodeAssistProvider::with_base_url(&endpoint.base_url, key, transport)
                .with_id(&endpoint.id)
                .with_redactor(redactor),
        ),
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
        // an error rather than a quiet fall back to the keychain: a handle that
        // names a store ARSY does not have must not resolve to a different
        // credential than it asked for.
        let resolved = match handle.store() {
            OS_STORE_ID => OsCredentialStore.resolve(handle.name()),
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
    let refreshed =
        oauth::refresh(&HttpTransport::default(), &oauth_client, &tokens).map_err(|error| {
            Diagnostic::error(
                ARSY_PRV_1000,
                format!(
                    "the stored login for provider `{}` could not be renewed: {error}",
                    endpoint.id
                ),
                format!("run `arsy auth login {}` again", endpoint.id),
            )
        })?;
    // Written back before use: a rotated refresh token is single-use, so
    // losing it here would cost the operator a re-login on the next run.
    let raw = serde_json::to_string(&refreshed)
        .map_err(|error| credential_failed(&endpoint.id, error))?;
    match handle.store() {
        OS_STORE_ID => {
            OsCredentialStore
                .set(handle.name(), &raw)
                .map_err(|error| credential_failed(&endpoint.id, error))?;
        }
        FILE_STORE_ID => {
            FileCredentialStore
                .set(handle.name(), &raw)
                .map_err(|error| credential_failed(&endpoint.id, error))?;
        }
        other => {
            return Err(Diagnostic::error(
                ARSY_PRV_1000,
                format!("the credential store `{other}` does not support write-back"),
                "use a store that supports credential storage",
            ));
        }
    }
    Ok((refreshed.access_token, CredentialSource::OAuth))
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
        (CredentialSource::Keyring | CredentialSource::OAuth, Some(handle)) => Ok(handle.clone()),
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
        "unlock the OS credential store, or re-run `arsy auth set` for this provider",
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
        let declared =
            arsy_kernel::model_profile::declared(Some(u64::from(endpoint.max_output_tokens)));
        for model in &endpoint.models {
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
                "max_output_tokens": endpoint.max_output_tokens,
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

fn configuration(invocation: &crate::Invocation) -> Result<Config, Diagnostic> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::config::Layer;

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
credential = "secret://os/local"
"#,
        );
        let endpoint = config.endpoint(None).unwrap();

        assert_eq!(
            redaction_handle(endpoint, CredentialSource::Keyring)
                .unwrap()
                .to_string(),
            "secret://os/local"
        );
        assert_eq!(
            redaction_handle(endpoint, CredentialSource::OAuth)
                .unwrap()
                .to_string(),
            "secret://os/local",
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
