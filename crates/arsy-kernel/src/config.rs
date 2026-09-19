//! Native configuration: `arsy.json` resolved across authority layers.
//!
//! The file is JSON, and it lives in one place an operator can name on every
//! platform: `~/.arsy/arsy.json`. ARSY and the wider ARSY ecosystem share that
//! directory, so the settings a person edits are not somewhere a different
//! product would have to guess at.
//!
//! See `docs/35-configuration.md`. Only the keys the runtime can act on today
//! are applied; the rest of the documented schema is accepted and ignored, so a
//! valid file is never rejected for being ahead of the implementation, while a
//! key that belongs to no documented section still fails loudly.
//!
//! Provider endpoints are the security-relevant part of this module. A
//! `base_url` decides where prompts and a credential are sent, so a file inside
//! a repository must not be able to set one: cloning a repository would
//! otherwise redirect the model call to whatever host that repository names.
//! Endpoint keys are therefore accepted from the enterprise and user layers
//! only, and reported as a diagnostic anywhere else.

use crate::{
    capability::{CapabilityAction, PolicySource, ResourcePattern},
    domain::Principal,
    policy::{ActorMatch, PolicyRule, RuleEffect, RuleSet, SandboxAssurance},
    secret::SecretHandle,
    telemetry::OpenTelemetryConfig,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Path, PathBuf},
};

/// The only schema this build understands.
pub const SCHEMA_VERSION: i64 = 1;

/// Name every configuration layer uses.
pub const CONFIG_FILE: &str = "arsy.json";

/// Name the pre-JSON configuration used. Still read once, to convert.
pub const LEGACY_CONFIG_FILE: &str = "config.toml";

/// Documented sections that parse but have no runtime effect yet. Listing them
/// keeps "unknown keys are errors" true without rejecting a forward-looking
/// file.
/// Where the credential catalog lives. `file` keeps it beside the user
/// configuration; `os` keeps it in the platform credential store.
///
/// The catalog holds handles, provider names, and timestamps — no secret value
/// — so an operator who does not want a keychain unlock on every turn can keep
/// it in a file without putting a key on disk.
/// How much of an MCP server's own logging an interactive session shows.
/// `hidden` shows none of it, `summary` one line per server saying how much
/// there was, `full` every line. A server that fails to connect is reported at
/// every level: that is a diagnostic, not logging.
pub const MCP_LOG_LEVELS: &[&str] = &["hidden", "summary", "full"];
/// Quiet enough that a noisy server cannot bury the transcript, loud enough
/// that a server saying something is never silently dropped.
pub const DEFAULT_MCP_LOG: &str = "summary";
/// Selectable interactive transcript projections.
pub const UI_STYLES: &[&str] = &["modern", "classic"];
pub const DEFAULT_UI_STYLE: &str = "modern";

/// The catalog can only be kept in a file. The platform keyring was withdrawn,
/// so `"os"` is recognised below only to say where it went.
pub const CREDENTIAL_STORES: &[&str] = &["file"];
/// What an operator gets without saying: no unlock prompt to read metadata.
pub const DEFAULT_CREDENTIAL_STORE: &str = "file";

/// `execution.max_parallel`: how many independent tool calls one round may run
/// at once.
///
/// Four rather than one because a model that reads five files reads them in
/// one round, and four rather than the core count because the calls are
/// waiting on disk and on other people's servers, not on this machine's CPU.
/// The number is the schema's, in `docs/35-configuration.md`; this is where it
/// is enforced.
pub const DEFAULT_PARALLEL_TOOLS: usize = 4;

/// The most any layer may ask for. A ceiling rather than a preference: a
/// configuration file that asked for two hundred concurrent calls would be
/// describing a fork bomb.
pub const MAX_PARALLEL_TOOLS: usize = 16;

/// Top-level keys this loader accepts and applies nothing from. `schema_version`
/// is here because `check_schema_version` has already read it.
const INERT_SECTIONS: &[&str] = &["schema_version", "context", "git", "sandbox", "storage"];

/// Other tools whose configuration can be read as a lower layer.
pub const COMPAT_SOURCES: &[&str] = &["claude", "codex", "omp"];

/// How `config explain` shows one connection. The target never includes the
/// launch env or headers, which may be credentials.
fn describe_mcp_server(server: &McpServer) -> String {
    format!(
        "{} · {} · {}",
        server.transport.kind(),
        server.transport.target(),
        if server.enabled {
            "enabled"
        } else {
            "disabled"
        }
    )
}

/// The keys that tune a connection without redefining where it points.
const MCP_AMENDABLE: &[&str] = &["enabled", "timeout_ms", "max_body_bytes"];

/// Whether a `[mcp.server.<name>]` table only switches or tunes a connection,
/// rather than defining one.
fn is_amendment(value: &toml::Value) -> bool {
    value.as_table().is_some_and(|table| {
        !table.is_empty()
            && table
                .keys()
                .all(|key| MCP_AMENDABLE.contains(&key.as_str()))
    })
}

/// Where a value came from, in ascending authority order.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Enterprise,
    User,
    Workspace,
    Nested,
    /// One file named on the command line with `--config`, applied last.
    ///
    /// The operator typed the path, so it is as trusted as their own user
    /// file — but it cannot weaken policy, because `provider.allowed`,
    /// `model.allowed`, and the policy rules all merge by intersection
    /// whatever layer supplied them.
    Session,
}

impl Layer {
    /// Whether the layer's file is under the operator's own control.
    ///
    /// Workspace and nested files travel with a repository, so they are
    /// untrusted content: they may express intent but cannot name an endpoint
    /// or a credential.
    pub const fn is_trusted(self) -> bool {
        matches!(self, Self::Enterprise | Self::User | Self::Session)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enterprise => "enterprise",
            Self::User => "user",
            Self::Workspace => "workspace",
            Self::Nested => "nested",
            Self::Session => "session",
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Wire dialect an endpoint speaks. This selects the adapter, and nothing else
/// about a provider is inferred from its name.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    Anthropic,
    Openai,
    /// OpenAI's Responses API (`/responses`), as the Codex/ChatGPT backend
    /// speaks it. A different body and event stream from Chat Completions.
    OpenaiResponses,
    /// Google's Cloud Code Assist API, as Antigravity speaks it: a Gemini
    /// `generateContent` payload inside a Code Assist wrapper.
    GoogleCodeAssist,
    /// A recorded conversation on disk, replayed one reply per request.
    ///
    /// Not a network dialect at all: `base_url` names a script file, no
    /// credential is needed, and nothing leaves the machine. It exists so a
    /// measurement can hold the model still while the harness changes, and so
    /// a failing session can be reproduced without one.
    Replay,
}

impl Dialect {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
            Self::OpenaiResponses => "openai_responses",
            Self::GoogleCodeAssist => "google_code_assist",
            Self::Replay => "replay",
        }
    }

    /// Base URL used when an endpoint names a dialect but no host.
    pub const fn default_base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::Openai => "https://api.openai.com/v1",
            Self::OpenaiResponses => "https://chatgpt.com/backend-api/codex",
            Self::GoogleCodeAssist => "https://cloudcode-pa.googleapis.com",
            // No default: a replay without a script names nothing to replay,
            // and an endpoint that has to be told where its script is should
            // fail rather than silently read a path nobody chose.
            Self::Replay => "",
        }
    }

    /// Environment variable consulted last, after config and the keyring.
    pub const fn default_api_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Openai | Self::OpenaiResponses => "OPENAI_API_KEY",
            Self::GoogleCodeAssist => "GEMINI_API_KEY",
            // A replay reads a file. Naming a variable here would invite an
            // operator to set one and wonder why it is ignored.
            Self::Replay => "",
        }
    }

    /// Whether reaching this dialect needs a credential at all.
    pub const fn needs_credential(self) -> bool {
        !matches!(self, Self::Replay)
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "anthropic" => Some(Self::Anthropic),
            "openai" => Some(Self::Openai),
            "openai_responses" => Some(Self::OpenaiResponses),
            "google_code_assist" => Some(Self::GoogleCodeAssist),
            "replay" => Some(Self::Replay),
            _ => None,
        }
    }
}

impl fmt::Display for Dialect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Config-driven OAuth client. An operator supplies the whole client, so this
/// works for any issuer; the built-in presets in `oauth::presets` fill the
/// same shape for the vendors ARSY ships a client for.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct OAuth {
    pub authorize_url: String,
    pub token_url: String,
    /// Present when the issuer supports RFC 8628, which needs no loopback port.
    pub device_authorization_url: Option<String>,
    pub client_id: String,
    /// Some installed-app clients (Google's, for one) still require the
    /// "secret" in the token exchange. It is not confidential for a client
    /// that ships in software, but the exchange fails without it.
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    /// Exact loopback redirect the issuer has registered, e.g.
    /// `http://localhost:1455/auth/callback`. When set, the listener binds
    /// that port and the URI is sent verbatim; otherwise a free port is taken
    /// and `http://127.0.0.1:<port>/callback` is used.
    pub redirect_uri: Option<String>,
    /// Extra query parameters for the authorization request, such as Google's
    /// `access_type=offline`.
    pub authorize_params: Vec<(String, String)>,
}

/// One resolved provider endpoint.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Endpoint {
    pub id: String,
    pub kind: Dialect,
    pub base_url: String,
    /// Keyring handle. Never a value: this struct is safe to print.
    pub credential: Option<SecretHandle>,
    pub api_key_env: Option<String>,
    pub model: Option<String>,
    /// Every model this endpoint offers, in the order the picker should show
    /// them. One endpoint speaks to one host, and a host serves more than one
    /// model, so the model is a list rather than a second endpoint that would
    /// duplicate the URL and the credential.
    ///
    /// `model` remains the default; it is always the first entry here.
    pub models: Vec<String>,
    /// Cap on one response. Providers differ in what they accept and the
    /// Anthropic dialect requires a value, so it is configurable rather than
    /// fixed.
    pub max_output_tokens: u32,
    pub oauth: Option<OAuth>,
    /// What this endpoint charges, per model.
    ///
    /// Configured rather than built in: prices change, they differ per
    /// account, and a table compiled into the binary would be quietly wrong
    /// for anyone on a negotiated rate. An unpriced model reports its cost as
    /// unknown, which is the honest answer — see [`Pricing`].
    pub pricing: BTreeMap<String, Pricing>,
}

const fn charge(tokens: u64, micros_per_million: u64) -> u64 {
    tokens
        .saturating_mul(micros_per_million)
        .div_ceil(1_000_000)
}

/// What one model costs, in micros per million tokens.
///
/// Micros because a token is far cheaper than a cent and floating point has no
/// place in a running total; per million because that is the unit every
/// provider publishes, so an operator copies the number rather than converting
/// it and getting the exponent wrong.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Pricing {
    pub input_micros_per_million: u64,
    pub output_micros_per_million: u64,
}

impl Pricing {
    /// What a turn cost, rounded up.
    ///
    /// Up rather than to nearest: a budget that under-reports is a budget that
    /// is exceeded without saying so, and the error is at most one micro.
    pub const fn cost_micros(self, input_tokens: u64, output_tokens: u64) -> u64 {
        charge(input_tokens, self.input_micros_per_million)
            .saturating_add(charge(output_tokens, self.output_micros_per_million))
    }
}

impl Endpoint {
    /// Put the default at the head of the offered models: a picker that does
    /// not list the model the endpoint is already using cannot show what is in
    /// force.
    fn offer_default_first(&mut self) {
        if let Some(model) = &self.model {
            self.models.retain(|listed| listed != model);
            self.models.insert(0, model.clone());
        }
    }
}

/// Response cap used when an endpoint does not set one. Large enough for a
/// substantial edit, small enough to bound a runaway response.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;

/// How long an MCP request may take before the connection is treated as
/// dropped. Long enough for a server that shells out, short enough that a hung
/// one does not hold a turn open.
pub const DEFAULT_MCP_TIMEOUT_MS: u64 = 30_000;

/// Largest response body accepted from an MCP server. The connection is
/// operator-configured and may be anything, so the cap is not optional.
pub const DEFAULT_MCP_MAX_BODY_BYTES: u64 = 1024 * 1024;

/// The one MCP server an ARSY install brings with it. FluxGuard ships in the
/// same archive as `arsy`, so resource awareness is there on the first run
/// rather than after a second install.
pub const BUNDLED_MCP_SERVER: &str = "fluxguard";

/// The bundled server as ARSY would run it.
///
/// Always declared, so the name is always something an operator can toggle and
/// a config file that toggles it stays loadable on an install that packages
/// `arsy` alone. Only the archive decides whether it starts out on: a copy
/// beside `arsy` is one this install shipped and can be trusted to be the
/// matching build, while a `fluxguard` further along `PATH` is some other
/// install's, offered but left off until the operator says otherwise.
fn bundled_mcp_server_beside(binary: &Path) -> McpServer {
    // Only an absolute directory counts: a bare name's parent is empty and
    // would resolve against the working directory, which a repository controls.
    let beside = |binary: &Path| {
        binary
            .parent()
            .filter(|directory| directory.is_absolute())
            .map(|directory| {
                directory.join(format!(
                    "{BUNDLED_MCP_SERVER}{}",
                    std::env::consts::EXE_SUFFIX
                ))
            })
            .filter(|command| command.is_file())
    };
    // `current_exe` reports the symlink on macOS, so an `arsy` linked onto
    // `PATH` alone still finds the copy installed beside its target.
    let bundled = beside(binary).or_else(|| {
        Some(binary)
            .filter(|binary| binary.is_absolute())
            .and_then(|binary| std::fs::canonicalize(binary).ok())
            .and_then(|target| beside(&target))
    });
    McpServer {
        name: BUNDLED_MCP_SERVER.to_owned(),
        transport: McpTransport::Stdio {
            command: bundled.as_ref().map_or_else(
                || BUNDLED_MCP_SERVER.to_owned(),
                |path| path.display().to_string(),
            ),
            args: vec!["serve".to_owned()],
            env: LaunchEnv::default(),
        },
        enabled: bundled.is_some(),
        // Shipped with the binary, but authority stops where the operator's
        // does: bundling decides what is declared, never what it may do.
        trust: PolicySource::User,
        timeout_ms: DEFAULT_MCP_TIMEOUT_MS,
        max_body_bytes: DEFAULT_MCP_MAX_BODY_BYTES,
    }
}

/// The bundled server for the running `arsy`.
fn bundled_mcp_server() -> McpServer {
    match std::env::current_exe() {
        Ok(binary) => bundled_mcp_server_beside(&binary),
        // Nothing to look beside, so nothing is claimed as bundled.
        Err(_) => bundled_mcp_server_beside(Path::new(BUNDLED_MCP_SERVER)),
    }
}

/// Where a command may be run other than on this machine.
///
/// A target is a *named* place, never a host a caller supplies: an operation
/// selects one of these by name, so nothing a model produces can decide which
/// machine a command reaches.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteTarget {
    Ssh {
        host: String,
        user: Option<String>,
        port: Option<u16>,
        /// Private key file. A path, never a key: this struct is safe to print.
        identity: Option<String>,
    },
    Container {
        /// `docker` or `podman`; the two speak the same `exec` surface.
        engine: String,
        container: String,
    },
}

impl RemoteTarget {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Ssh { .. } => "ssh",
            Self::Container { .. } => "container",
        }
    }

    /// What an operator would recognize the target by.
    pub fn describe(&self) -> String {
        match self {
            Self::Ssh {
                host, user, port, ..
            } => {
                let mut described = match user {
                    Some(user) => format!("ssh {user}@{host}"),
                    None => format!("ssh {host}"),
                };
                if let Some(port) = port {
                    described.push_str(&format!(":{port}"));
                }
                described
            }
            Self::Container { engine, container } => format!("{engine} exec {container}"),
        }
    }
}

/// Container engines this build knows how to drive.
pub const CONTAINER_ENGINES: &[&str] = &["docker", "podman"];

/// Values a server is handed when it is reached: the environment of a stdio
/// process, or the headers of an HTTP request.
///
/// They are usually credentials — a connection string, a bearer token — so
/// they are never serialized, and `Debug` names the keys without the values.
/// Only the connection that launches the server reads them.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct LaunchEnv(BTreeMap<String, String>);

impl LaunchEnv {
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<BTreeMap<String, String>> for LaunchEnv {
    fn from(values: BTreeMap<String, String>) -> Self {
        Self(values)
    }
}

impl fmt::Debug for LaunchEnv {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_set().entries(self.0.keys()).finish()
    }
}

/// How ARSY reaches one MCP server.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        #[serde(skip)]
        env: LaunchEnv,
    },
    Http {
        url: String,
        #[serde(skip)]
        headers: LaunchEnv,
    },
}

impl McpTransport {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
        }
    }

    /// What a connection would actually run or reach, for a listing that has
    /// to let an operator recognize the server they meant.
    pub fn target(&self) -> String {
        match self {
            Self::Stdio { command, args, .. } if args.is_empty() => command.clone(),
            Self::Stdio { command, args, .. } => format!("{command} {}", args.join(" ")),
            Self::Http { url, .. } => url.clone(),
        }
    }
}

/// Where a declaration that no `arsy.json` wrote came from.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Provenance {
    /// The tool that declared it, as `compat.<label>` names it.
    pub label: String,
    pub path: PathBuf,
}

/// A model another tool's configuration names.
///
/// Only a fallback: it is used for an endpoint that names no model, when no
/// layer set `model.default`, and only where the endpoint speaks one of
/// `dialects` — a Claude model is never sent to an OpenAI endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelHint {
    pub model: String,
    pub dialects: Vec<Dialect>,
    /// The endpoint id it must be, when the other tool named its provider.
    pub provider: Option<String>,
}

/// What another tool's configuration contributes, already translated into
/// ARSY's own types at the edge that read it. The kernel never parses another
/// tool's format; it only places the result below every `arsy.json` layer.
#[derive(Clone, Debug, Default)]
pub struct CompatSeed {
    pub label: String,
    pub path: PathBuf,
    /// Each carries the trust of the scope it was declared in: a file in the
    /// operator's home is theirs, one in the checkout is the repository's.
    pub mcp_servers: Vec<McpServer>,
    /// Keyed by an id under `compat/`, which no `arsy.json` rule can take.
    /// Each rule's `source` is the scope it was declared in, so a repository's
    /// `allow` is downgraded when the rules are compiled like any other.
    pub policy_rules: Vec<(String, PolicyRule)>,
    /// In precedence order; the first that fits an endpoint is used.
    pub models: Vec<ModelHint>,
    /// Whatever was understood but could not be applied, for `config explain`.
    pub notes: Vec<String>,
}

/// One configured MCP connection. Holding the definition is not connecting:
/// nothing here has contacted the server.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct McpServer {
    pub name: String,
    #[serde(flatten)]
    pub transport: McpTransport,
    pub enabled: bool,
    /// The authority of the layer that defined it. A workspace definition is
    /// untrusted content: it may be connected to, but it cannot grant itself
    /// the right to act.
    pub trust: PolicySource,
    pub timeout_ms: u64,
    pub max_body_bytes: u64,
}

/// The `[theme]` table: a built-in theme to start from, plus per-role colour
/// overrides. The CLI turns this into its palette; the kernel only carries and
/// validates it, so a headless run rejects a bad colour at load time too.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Theme {
    /// Name of a built-in theme. `None` leaves the CLI's default in force.
    pub base: Option<String>,
    /// `role -> "#rrggbb"`. Role names are the CLI's to know; the kernel only
    /// checks the colour is well formed.
    pub roles: BTreeMap<String, String>,
}

/// Whether `hex` is `#rrggbb` (the `#` optional), the one colour form `[theme]`
/// accepts.
fn valid_hex(hex: &str) -> bool {
    let body = hex.strip_prefix('#').unwrap_or(hex);
    body.len() == 6 && body.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The port of a loopback OAuth redirect URI, or `None` when it is not one:
/// `http`/`https`, a loopback host, and an explicit port. The login binds this
/// port so the issuer's registered redirect resolves to ARSY's own listener.
pub fn redirect_loopback_port(uri: &str) -> Option<u16> {
    let rest = uri
        .strip_prefix("http://")
        .or_else(|| uri.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let (host, port) = authority.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return None;
    }
    port.parse().ok()
}

/// Effective value of one key and the file it won from.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Origin {
    pub value: String,
    pub layer: Layer,
    pub path: PathBuf,
}

/// An input that was understood but not applied. Never fatal: the run
/// continues without the rejected value, and `arsy config explain` shows why.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Diagnostic {
    pub key: String,
    pub layer: Layer,
    pub path: PathBuf,
    pub message: String,
}

/// A file that could not be trusted to mean what it says, so the whole load
/// fails rather than proceeding with a partly-applied policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigError {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for ConfigError {}

/// `[lsp.server.<name>]`: a language server this workspace may start.
///
/// The command is a program ARSY will execute, so it is refused outside the
/// enterprise and user layers: a repository must not be able to name what runs
/// on the machine that clones it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LanguageServer {
    pub name: String,
    /// The program and its arguments.
    pub command: Vec<String>,
    /// File extensions this server answers for, without the dot.
    pub extensions: BTreeSet<String>,
}

/// `[telemetry]`: how much of a run is observed, and where it may leave.
///
/// The defaults are the ones a local run wants — every event kept, nothing
/// exported — so an operator who never writes the section still gets the
/// latency and token counts a finished run reports.
#[derive(Clone, Debug)]
pub struct TelemetrySettings {
    /// Keep one trace in every `sample_every`. `1` keeps all of them.
    pub sample_every: u64,
    /// How many events may queue before a run drops them rather than wait.
    /// Telemetry that blocks a turn would be worse than telemetry that is
    /// missing, and a drop is counted.
    pub capacity: usize,
    pub otel: OpenTelemetryConfig,
}

impl Default for TelemetrySettings {
    fn default() -> Self {
        Self {
            sample_every: 1,
            capacity: 256,
            otel: OpenTelemetryConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Config {
    provider_default: Option<String>,
    model_default: Option<String>,
    credential_store: Option<String>,
    /// `ui.style`. `None` uses the mockup-oriented projection.
    ui_style: Option<String>,
    /// `ui.mcp_log`. `None` is the built-in default.
    mcp_log: Option<String>,
    /// `execution.max_parallel`. `None` is the built-in default.
    max_parallel_tools: Option<usize>,
    endpoints: BTreeMap<String, Endpoint>,
    /// `provider.allowed` and `model.allowed` after intersection. `None` means
    /// no layer capped the set, which is not the same as an empty allowlist:
    /// an empty one permits nothing.
    provider_allowed: Option<BTreeSet<String>>,
    model_allowed: Option<BTreeSet<String>>,
    theme: Theme,
    /// Policy rules keyed by their stable `id`, so a later layer amends a rule
    /// rather than appending a second one with the same meaning.
    policy_rules: BTreeMap<String, PolicyRule>,
    policy_default_effect: Option<RuleEffect>,
    /// `[mcp.server.<name>]` keyed by name, so a higher layer replaces a
    /// definition rather than adding a second connection with the same name.
    mcp_servers: BTreeMap<String, McpServer>,
    /// Which of `mcp_servers` another tool declared. Dropped when an
    /// `arsy.json` layer replaces the definition, kept when one only amends it.
    mcp_provenance: BTreeMap<String, Provenance>,
    /// `compat.<source>.enabled = false` from any layer.
    compat_disabled: BTreeSet<String>,
    /// Models other tools name, in seed order.
    compat_models: Vec<ModelHint>,
    /// `[remote.target.<name>]`, from a trusted layer only.
    remote_targets: BTreeMap<String, RemoteTarget>,
    /// `[lsp.server.<name>]`, from a trusted layer only.
    language_servers: BTreeMap<String, LanguageServer>,
    /// `[project."<path>"] trust_level = "trusted"`, from a trusted layer only.
    /// Directories whose own files may run something.
    trusted_projects: BTreeSet<PathBuf>,
    telemetry: TelemetrySettings,
    /// What the layers that spoke agreed on for `telemetry.include_content`.
    /// `None` means none of them did, which is not the same as `Some(false)`.
    telemetry_include_content: Option<bool>,
    trace: BTreeMap<String, Origin>,
    diagnostics: Vec<Diagnostic>,
}

impl Config {
    /// Every configured endpoint id, in configuration order, so a picker can
    /// offer them without the caller reaching into the map.
    pub fn endpoint_ids(&self) -> Vec<String> {
        self.endpoints.keys().cloned().collect()
    }

    /// Which store the credential catalog is kept in.
    pub fn credential_store(&self) -> &str {
        self.credential_store
            .as_deref()
            .unwrap_or(DEFAULT_CREDENTIAL_STORE)
    }

    /// `ui.style`: the interactive transcript projection.
    pub fn ui_style(&self) -> &str {
        self.ui_style.as_deref().unwrap_or(DEFAULT_UI_STYLE)
    }

    /// `ui.mcp_log`: how much of a server's own logging to show.
    pub fn mcp_log(&self) -> &str {
        self.mcp_log.as_deref().unwrap_or(DEFAULT_MCP_LOG)
    }

    /// Read every layer in authority order. A missing file is not an error;
    /// an unreadable or invalid one is.
    pub fn load(layers: &[(Layer, PathBuf)]) -> Result<Self, ConfigError> {
        Self::load_with(layers, &[])
    }

    /// Read every layer on top of what other tools declared.
    ///
    /// The seeds go in before the first file, exactly like the bundled server,
    /// so every `arsy.json` layer outranks them: a layer naming the same server
    /// replaces it, and one that only sets `enabled` switches it.
    pub fn load_with(
        layers: &[(Layer, PathBuf)],
        seeds: &[CompatSeed],
    ) -> Result<Self, ConfigError> {
        let mut config = Self::default();
        // Declared before the first layer is read, so any layer that names
        // `mcp.server.fluxguard` replaces it outright -- which is also how an
        // operator turns it off, with `enabled = false`.
        config
            .mcp_servers
            .insert(BUNDLED_MCP_SERVER.to_owned(), bundled_mcp_server());
        seeds.iter().for_each(|seed| config.seed(seed));
        for (layer, path) in layers {
            let raw = match std::fs::read_to_string(path) {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(ConfigError {
                        path: path.clone(),
                        message: error.to_string(),
                    })
                }
            };
            config.apply(*layer, path, &raw)?;
        }
        Ok(config)
    }

    /// Place one tool's declarations. A name the bundled server or an earlier
    /// seed already holds is kept: the caller orders seeds by precedence.
    fn seed(&mut self, seed: &CompatSeed) {
        let layer = |server: &McpServer| match server.trust {
            PolicySource::Workspace => Layer::Workspace,
            _ => Layer::User,
        };
        for server in &seed.mcp_servers {
            if self.mcp_servers.contains_key(&server.name) {
                continue;
            }
            self.record(
                layer(server),
                &seed.path,
                &format!("mcp.server.{}", server.name),
                describe_mcp_server(server),
            );
            self.mcp_provenance.insert(
                server.name.clone(),
                Provenance {
                    label: seed.label.clone(),
                    path: seed.path.clone(),
                },
            );
            self.mcp_servers.insert(server.name.clone(), server.clone());
        }
        self.seed_rules(seed);
        for hint in &seed.models {
            self.record(
                Layer::User,
                &seed.path,
                &format!("compat.{}.model", seed.label),
                hint.model.clone(),
            );
        }
        self.compat_models.extend(seed.models.iter().cloned());
        self.diagnostics
            .extend(seed.notes.iter().map(|message| Diagnostic {
                key: format!("compat.{}", seed.label),
                layer: Layer::User,
                path: seed.path.clone(),
                message: message.clone(),
            }));
    }

    /// Place one tool's policy rules under ids no `arsy.json` rule can take.
    fn seed_rules(&mut self, seed: &CompatSeed) {
        for (id, rule) in &seed.policy_rules {
            let id = format!("compat/{}", id.trim_start_matches("compat/"));
            let layer = match rule.source {
                PolicySource::Workspace => Layer::Workspace,
                _ => Layer::User,
            };
            self.record(
                layer,
                &seed.path,
                &format!("policy.rules.{id}"),
                format!(
                    "{} {} {}",
                    effect_name(rule.effect),
                    rule.action,
                    rule.pattern
                ),
            );
            self.policy_rules.entry(id).or_insert_with(|| rule.clone());
        }
    }

    /// Whether no layer switched this tool's configuration off. A `false`
    /// sticks: a later layer cannot switch it back on, so a repository cannot
    /// re-enable a source the operator turned off.
    pub fn compat_enabled(&self, source: &str) -> bool {
        !self.compat_disabled.contains(source)
    }

    /// The model another tool names for this endpoint, when `arsy.json` names
    /// none: the first whose dialect and provider fit and that `model.allowed`
    /// admits.
    pub fn compat_model(&self, endpoint: &Endpoint) -> Option<&str> {
        self.compat_models
            .iter()
            .filter(|hint| hint.dialects.contains(&endpoint.kind))
            .filter(|hint| hint.provider.as_deref().is_none_or(|id| id == endpoint.id))
            .map(|hint| hint.model.as_str())
            .find(|model| self.model_is_allowed(model))
    }

    /// The tool that declared this MCP connection, when no `arsy.json` did.
    pub fn mcp_provenance(&self, name: &str) -> Option<&Provenance> {
        self.mcp_provenance.get(name)
    }

    pub fn provider_default(&self) -> Option<&str> {
        self.provider_default.as_deref()
    }

    pub fn model_default(&self) -> Option<&str> {
        self.model_default.as_deref()
    }

    /// How many independent tool calls one round may run at once.
    pub fn max_parallel_tools(&self) -> usize {
        self.max_parallel_tools.unwrap_or(DEFAULT_PARALLEL_TOOLS)
    }

    /// The `[theme]` table, empty when the file did not set one.
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn endpoints(&self) -> impl Iterator<Item = &Endpoint> {
        self.endpoints.values()
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Configured remote targets in name order. Nothing is connected.
    pub fn remote_targets(&self) -> impl Iterator<Item = (&String, &RemoteTarget)> {
        self.remote_targets.iter()
    }

    /// `[telemetry]` as the runtime reads it, defaults included.
    pub fn telemetry(&self) -> &TelemetrySettings {
        &self.telemetry
    }

    /// Configured language servers in name order. Nothing is started.
    pub fn language_servers(&self) -> impl Iterator<Item = &LanguageServer> {
        self.language_servers.values()
    }

    /// The server configured for a file's extension, if any.
    ///
    /// First by name order rather than "best": two servers claiming the same
    /// extension is a configuration mistake, and picking one by a rule nobody
    /// wrote down would hide it.
    pub fn language_server_for(&self, path: &Path) -> Option<&LanguageServer> {
        let extension = path.extension()?.to_str()?;
        self.language_servers
            .values()
            .find(|server| server.extensions.contains(extension))
    }

    pub fn remote_target(&self, name: &str) -> Option<&RemoteTarget> {
        self.remote_targets.get(name)
    }

    /// Configured MCP connections in name order. Nothing is connected.
    /// Whether the operator vouched for this directory.
    ///
    /// An ancestor's trust covers what is under it, the way Codex's own list
    /// works: vouching for a checkout should not have to be repeated for every
    /// crate inside it. Nothing is trusted by default, so a repository that was
    /// never named runs none of its own hooks.
    pub fn trusts(&self, directory: &Path) -> bool {
        // Resolved on both sides before comparing. Two names for one directory
        // are common and innocent — `/var` is `/private/var` on macOS — but a
        // textual comparison also means a symlink planted beside a vouched-for
        // checkout would inherit its trust.
        let resolve = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let directory = resolve(directory);
        self.trusted_projects
            .iter()
            .any(|trusted| directory.starts_with(resolve(trusted)))
    }

    /// Every directory the operator vouched for, so `config explain` and
    /// `arsy hook list` can say which list a decision came from.
    pub fn trusted_projects(&self) -> impl Iterator<Item = &Path> {
        self.trusted_projects.iter().map(PathBuf::as_path)
    }

    pub fn mcp_servers(&self) -> impl Iterator<Item = &McpServer> {
        self.mcp_servers.values()
    }

    pub fn mcp_server(&self, name: &str) -> Option<&McpServer> {
        self.mcp_servers.get(name)
    }

    /// The `[[policy.rules]]` this configuration resolved to, in rule-id order.
    /// Compile them with `RuleSet::compile` before evaluating: that is where an
    /// untrusted layer's `allow` is downgraded.
    pub fn policy_rules(&self) -> Vec<PolicyRule> {
        self.policy_rules.values().cloned().collect()
    }

    /// The compiled rule set this configuration means, including the catch-all
    /// that `policy.default_effect` stands for.
    ///
    /// Every caller that decides anything uses this, so a dry run and an
    /// execution cannot reach different answers: building the rules in one
    /// place and the default in another is exactly how they would.
    ///
    /// The default is one rule per action rather than one wildcard rule,
    /// because a rule matches on a concrete action; and it is compiled with the
    /// layer's own authority, so a default an untrusted layer wrote is
    /// downgraded like any other allow it wrote.
    pub fn policy_rule_set(&self) -> RuleSet {
        let (effect, source) = self.policy_default();
        let defaults = CapabilityAction::ALL.iter().map(|action| PolicyRule {
            source,
            effect,
            actor: ActorMatch::Any,
            action: *action,
            pattern: ResourcePattern::new(action.default_scheme(), "**")
                .expect("a static scheme and glob are valid"),
            expires_at_ms: None,
            delegation_depth: 0,
            minimum_assurance: SandboxAssurance::None,
        });
        RuleSet::compile(self.policy_rules().into_iter().chain(defaults))
    }

    /// What happens to a query no rule covers, and the authority that decided
    /// it. The engine denies silence outright, so this is the effect a
    /// synthesized catch-all rule carries.
    ///
    /// Unset means `ask` on the built-in schema's authority; `ask` grants
    /// nothing, so a built-in default can never widen what a layer allowed.
    pub fn policy_default(&self) -> (RuleEffect, PolicySource) {
        let source = self
            .trace
            .get("policy.default_effect")
            .map_or(PolicySource::Enterprise, |origin| {
                policy_source(origin.layer)
            });
        (
            self.policy_default_effect
                .unwrap_or(RuleEffect::RequireApproval),
            source,
        )
    }

    /// The endpoint a turn should use: the requested one, else the configured
    /// default, else the only one there is.
    ///
    /// A single configured endpoint needs no `provider.default`, and naming a
    /// provider that does not exist is `None` rather than a silent fallback to
    /// some other endpoint.
    pub fn endpoint(&self, requested: Option<&str>) -> Option<&Endpoint> {
        let allowed: Vec<&Endpoint> = self
            .endpoints
            .values()
            .filter(|endpoint| self.provider_is_allowed(&endpoint.id))
            .collect();
        // `"auto"` is the documented way to say "no explicit choice", so it
        // resolves like an absent one rather than like an endpoint of that name.
        match requested
            .or(self.provider_default.as_deref())
            .filter(|id| *id != "auto")
        {
            Some(id) => allowed.into_iter().find(|endpoint| endpoint.id == id),
            None if allowed.len() == 1 => allowed.into_iter().next(),
            None => None,
        }
    }

    /// Whether `provider.allowed` admits this endpoint. An unset allowlist
    /// admits every configured endpoint; an empty one admits none.
    pub fn provider_is_allowed(&self, id: &str) -> bool {
        self.provider_allowed
            .as_ref()
            .is_none_or(|allowed| allowed.contains(id))
    }

    /// Whether `model.allowed` admits this model.
    pub fn model_is_allowed(&self, model: &str) -> bool {
        self.model_allowed
            .as_ref()
            .is_none_or(|allowed| allowed.contains(model))
    }

    /// The resolved `provider.allowed` ceiling, or `None` when no layer set one.
    pub fn provider_allowed(&self) -> Option<&BTreeSet<String>> {
        self.provider_allowed.as_ref()
    }

    /// The resolved `model.allowed` ceiling, or `None` when no layer set one.
    pub fn model_allowed(&self) -> Option<&BTreeSet<String>> {
        self.model_allowed.as_ref()
    }

    /// Every configured endpoint, whether or not the ceiling admits it.
    /// `arsy provider list --all` is the caller.
    pub fn all_endpoints(&self) -> impl Iterator<Item = &Endpoint> {
        self.endpoints.values()
    }

    /// Effective values with their sources, optionally narrowed to one key or
    /// key prefix. This is what `arsy config explain` prints.
    pub fn explain(&self, key: Option<&str>) -> serde_json::Value {
        let selected: BTreeMap<_, _> = self
            .trace
            .iter()
            .filter(|(name, _)| key.is_none_or(|key| matches(name, key)))
            .collect();
        serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "values": selected,
            "diagnostics": self.diagnostics,
        })
    }

    fn apply(&mut self, layer: Layer, path: &Path, raw: &str) -> Result<(), ConfigError> {
        // Parsed as JSON into the same tree the rest of this module walks. The
        // tree type is `toml`'s because that is what the schema was written
        // against; no configuration file is TOML any more.
        let table: toml::Table = serde_json::from_str(raw).map_err(|error| ConfigError {
            path: path.to_path_buf(),
            message: format!("is not valid JSON: {error}"),
        })?;
        check_schema_version(&table, path)?;
        for (key, value) in &table {
            self.apply_section(layer, path, key, value)?;
        }
        Ok(())
    }

    /// One top-level section. Every arm either delegates to the function that
    /// owns that section or, for the three short ones, reads its own keys.
    fn apply_section(
        &mut self,
        layer: Layer,
        path: &Path,
        key: &str,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        match key {
            "provider" => self.apply_provider(layer, path, value),
            "model" => self.apply_model(layer, path, value),
            "credentials" => self.apply_credentials(layer, path, value),
            "execution" => self.apply_execution(layer, path, value),
            "telemetry" => self.apply_telemetry(layer, path, value),
            "lsp" => self.apply_lsp(layer, path, value),
            "mcp" => self.apply_mcp(layer, path, value),
            "remote" => self.apply_remote(layer, path, value),
            "project" => self.apply_project(layer, path, value),
            "policy" => self.apply_policy(layer, path, value),
            "theme" => self.apply_theme(layer, path, value),
            "ui" => self.apply_ui(layer, path, value),
            "compat" => self.apply_compat(layer, path, value),
            section if INERT_SECTIONS.contains(&section) => Ok(()),
            other => Err(ConfigError {
                path: path.to_path_buf(),
                message: format!("unknown key `{other}`"),
            }),
        }
    }

    /// `model.default` and the `model.allowed` ceiling.
    fn apply_model(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let table = as_table(value, "model", path)?;
        if let Some(default) = string(table, "default", "model.default", path)? {
            self.model_default = Some(default.clone());
            self.record(layer, path, "model.default", default);
        }
        if let Some(value) = table.get("allowed") {
            let allowed = name_set(value, "model.allowed", path)?;
            self.record(layer, path, "model.allowed", joined(&allowed));
            self.model_allowed = Some(intersect(self.model_allowed.take(), allowed));
        }
        Ok(())
    }

    /// `credentials.store`: which store the credential catalog is kept in.
    fn apply_credentials(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let table = as_table(value, "credentials", path)?;
        let Some(store) = string(table, "store", "credentials.store", path)?.cloned() else {
            return Ok(());
        };
        if !CREDENTIAL_STORES.contains(&store.as_str()) {
            // Named rather than lumped in with the typos: an operator who set
            // this deliberately is owed the reason it stopped being a choice.
            let message = if store == "os" {
                "credentials.store = \"os\" named the platform keyring, which ARSY no longer \
                 reads; remove the key to keep the catalog beside this file"
                    .to_owned()
            } else {
                format!(
                    "credentials.store must be one of {}, not `{store}`",
                    CREDENTIAL_STORES.join(", ")
                )
            };
            return Err(ConfigError {
                path: path.to_path_buf(),
                message,
            });
        }
        self.credential_store = Some(store.clone());
        self.record(layer, path, "credentials.store", store);
        Ok(())
    }

    /// `execution.max_parallel`. The rest of the section is still inert, so an
    /// unknown key here is accepted as it always was; only the one this build
    /// reads is validated.
    fn apply_execution(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let table = as_table(value, "execution", path)?;
        let Some(value) = table.get("max_parallel") else {
            return Ok(());
        };
        let key = "execution.max_parallel";
        let limit = value
            .as_integer()
            .and_then(|limit| usize::try_from(limit).ok())
            .filter(|limit| (1..=MAX_PARALLEL_TOOLS).contains(limit))
            .ok_or_else(|| ConfigError {
                path: path.to_path_buf(),
                message: format!("`{key}` must be between 1 and {MAX_PARALLEL_TOOLS}"),
            })?;
        self.record(layer, path, key, limit.to_string());
        // Narrowest wins, like every other ceiling: a layer may ask for less
        // concurrency than the one above it and never for more.
        self.max_parallel_tools = Some(
            self.max_parallel_tools
                .map_or(limit, |held| held.min(limit)),
        );
        Ok(())
    }

    /// `[mcp.server.<name>]`: one external MCP connection each.
    ///
    /// A definition names a program to run or a host to send workspace content
    /// to, so the layer that wrote it becomes the connection's trust label. A
    /// workspace file may still declare one — that is how a repository ships
    /// its own tooling — but it is labelled untrusted, and nothing that cannot
    /// grant authority can make it trusted by saying so.
    fn apply_mcp(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        for (key, value) in as_table(value, "mcp", path)? {
            if key != "server" {
                return Err(reject(format!("unknown key `mcp.{key}`")));
            }
            for (name, value) in as_table(value, "mcp.server", path)? {
                let server = match self.amend_mcp_server(layer, path, name, value)? {
                    Some(server) => server,
                    None if is_amendment(value) => {
                        // Names a connection nothing declares any more -- most
                        // often one another tool's file stopped declaring.
                        // Refusing the whole file for it would stop every run.
                        self.diagnostics.push(Diagnostic {
                            key: format!("mcp.server.{name}"),
                            layer,
                            path: path.to_path_buf(),
                            message: "sets `enabled` or a limit on a connection nothing declares; ignored".to_owned(),
                        });
                        continue;
                    }
                    None => {
                        self.mcp_provenance.remove(name);
                        self.parse_mcp_server(layer, path, name, value)?
                    }
                };
                self.record(
                    layer,
                    path,
                    &format!("mcp.server.{name}"),
                    describe_mcp_server(&server),
                );
                self.mcp_servers.insert(name.clone(), server);
            }
        }
        Ok(())
    }

    /// A table that sets `enabled` or the request limits on a connection that
    /// already exists, without restating where it points.
    ///
    /// The bundled server is the reason this is here: it is declared before
    /// any file is read, so an operator has nothing to copy and would
    /// otherwise have to reproduce its command to switch it off or tune it.
    /// Amending is deliberately the narrow case -- only `enabled`,
    /// `timeout_ms`, and `max_body_bytes`, only for a name already declared.
    /// Anything that names a `transport` still replaces the definition
    /// outright, so a layer can never quietly repoint a connection while
    /// looking like it only toggled one.
    /// `compat.<source>.enabled`, the only key each source has.
    fn apply_compat(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        for (source, value) in as_table(value, "compat", path)? {
            if !COMPAT_SOURCES.contains(&source.as_str()) {
                return Err(reject(format!("unknown key `compat.{source}`")));
            }
            self.apply_compat_source(layer, path, source, value)?;
        }
        Ok(())
    }

    fn apply_compat_source(
        &mut self,
        layer: Layer,
        path: &Path,
        source: &str,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        let prefix = format!("compat.{source}");
        for (key, value) in as_table(value, &prefix, path)? {
            let enabled = match (key.as_str(), value.as_bool()) {
                ("enabled", Some(enabled)) => enabled,
                ("enabled", None) => {
                    return Err(reject(format!("`{prefix}.enabled` must be a boolean")))
                }
                _ => return Err(reject(format!("unknown key `{prefix}.{key}`"))),
            };
            if !enabled {
                self.compat_disabled.insert(source.to_owned());
            }
            self.record(
                layer,
                path,
                &format!("{prefix}.enabled"),
                self.compat_enabled(source).to_string(),
            );
        }
        Ok(())
    }

    fn amend_mcp_server(
        &self,
        layer: Layer,
        path: &Path,
        name: &str,
        value: &toml::Value,
    ) -> Result<Option<McpServer>, ConfigError> {
        let prefix = format!("mcp.server.{name}");
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        let table = as_table(value, &prefix, path)?;
        if table.is_empty()
            || !table
                .keys()
                .all(|key| MCP_AMENDABLE.contains(&key.as_str()))
        {
            return Ok(None);
        }
        let Some(existing) = self.mcp_servers.get(name) else {
            return Ok(None);
        };
        let enabled = table
            .get("enabled")
            .map(|value| {
                value
                    .as_bool()
                    .ok_or_else(|| reject(format!("`{prefix}.enabled` must be a boolean")))
            })
            .transpose()?;
        let positive = |key: &str, current: u64| -> Result<u64, ConfigError> {
            match table.get(key) {
                None => Ok(current),
                Some(value) => value
                    .as_integer()
                    .and_then(|value| u64::try_from(value).ok())
                    .filter(|value| *value > 0)
                    .ok_or_else(|| reject(format!("`{prefix}.{key}` must be a positive integer"))),
            }
        };
        Ok(Some(McpServer {
            enabled: enabled.unwrap_or(existing.enabled),
            // Switching a connection off never increases authority, so the
            // label it was given survives. Switching one back on does, so it
            // is the amending layer that answers for it -- which is what stops
            // a repository re-enabling a connection an operator turned off and
            // having it act with the operator's trust.
            trust: if enabled == Some(true) {
                policy_source(layer)
            } else {
                existing.trust
            },
            timeout_ms: positive("timeout_ms", existing.timeout_ms)?,
            max_body_bytes: positive("max_body_bytes", existing.max_body_bytes)?,
            ..existing.clone()
        }))
    }

    fn parse_mcp_server(
        &self,
        layer: Layer,
        path: &Path,
        name: &str,
        value: &toml::Value,
    ) -> Result<McpServer, ConfigError> {
        let prefix = format!("mcp.server.{name}");
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        let table = as_table(value, &prefix, path)?;
        for key in table.keys() {
            if !MCP_SERVER_KEYS.contains(&key.as_str()) {
                return Err(reject(format!("unknown key `{prefix}.{key}`")));
            }
        }
        let kind = expect_string(
            table
                .get("transport")
                .ok_or_else(|| reject(format!("`{prefix}` needs a `transport`")))?,
            &format!("{prefix}.transport"),
            path,
        )?;
        let transport = match kind.as_str() {
            "stdio" => {
                let command = expect_string(
                    table.get("command").ok_or_else(|| {
                        reject(format!("`{prefix}` is stdio, so it needs a `command`"))
                    })?,
                    &format!("{prefix}.command"),
                    path,
                )?
                .clone();
                let args = match table.get("args") {
                    None => Vec::new(),
                    Some(value) => value
                        .as_array()
                        .ok_or_else(|| reject(format!("`{prefix}.args` must be an array")))?
                        .iter()
                        .map(|argument| {
                            expect_string(argument, &format!("{prefix}.args"), path).cloned()
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                };
                McpTransport::Stdio {
                    command,
                    args,
                    env: LaunchEnv::default(),
                }
            }
            "http" => McpTransport::Http {
                url: expect_string(
                    table.get("url").ok_or_else(|| {
                        reject(format!("`{prefix}` is http, so it needs a `url`"))
                    })?,
                    &format!("{prefix}.url"),
                    path,
                )?
                .clone(),
                headers: LaunchEnv::default(),
            },
            other => {
                return Err(reject(format!(
                    "`{prefix}.transport` must be `stdio` or `http`, not `{other}`"
                )))
            }
        };
        let positive = |key: &str, default: u64| -> Result<u64, ConfigError> {
            match table.get(key) {
                None => Ok(default),
                Some(value) => value
                    .as_integer()
                    .and_then(|value| u64::try_from(value).ok())
                    .filter(|value| *value > 0)
                    .ok_or_else(|| reject(format!("`{prefix}.{key}` must be a positive integer"))),
            }
        };
        Ok(McpServer {
            name: name.to_owned(),
            transport,
            enabled: match table.get("enabled") {
                None => true,
                Some(value) => value
                    .as_bool()
                    .ok_or_else(|| reject(format!("`{prefix}.enabled` must be a boolean")))?,
            },
            trust: policy_source(layer),
            timeout_ms: positive("timeout_ms", DEFAULT_MCP_TIMEOUT_MS)?,
            max_body_bytes: positive("max_body_bytes", DEFAULT_MCP_MAX_BODY_BYTES)?,
        })
    }

    /// `[telemetry]`: sampling, queue depth, and the optional export.
    ///
    /// `telemetry.endpoint` names a host that run data is sent to, so the
    /// export keys are refused outside the enterprise and user layers for the
    /// same reason a provider endpoint is: cloning a repository must not be
    /// able to redirect what a run reports about itself. Sampling and queue
    /// depth carry no such risk and are honoured from any layer.
    fn apply_telemetry(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        let table = as_table(value, "telemetry", path)?;
        for key in table.keys() {
            if !matches!(
                key.as_str(),
                "sample_every" | "capacity" | "enabled" | "endpoint" | "include_content"
            ) {
                return Err(reject(format!("unknown key `telemetry.{key}`")));
            }
        }

        for key in ["sample_every", "capacity"] {
            let Some(value) = table.get(key) else {
                continue;
            };
            let number = value
                .as_integer()
                .and_then(|number| u64::try_from(number).ok())
                .filter(|number| *number > 0)
                .ok_or_else(|| reject(format!("`telemetry.{key}` must be a positive integer")))?;
            if key == "sample_every" {
                self.telemetry.sample_every = number;
            } else {
                self.telemetry.capacity = usize::try_from(number)
                    .map_err(|_| reject("`telemetry.capacity` is too large".to_owned()))?;
            }
            self.record(layer, path, &format!("telemetry.{key}"), number.to_string());
        }

        let exports = ["enabled", "endpoint", "include_content"];
        if !exports.iter().any(|key| table.contains_key(*key)) {
            return Ok(());
        }
        if !layer.is_trusted() {
            // Refused, not merged: see the module note on repository content.
            self.diagnostics.push(Diagnostic {
                key: "telemetry.endpoint".to_owned(),
                layer,
                path: path.to_path_buf(),
                message: "the telemetry export may only be configured by the enterprise or user \
                          configuration, because it decides where run data goes"
                    .to_owned(),
            });
            return Ok(());
        }
        let flag = |name: &str| -> Result<Option<bool>, ConfigError> {
            table
                .get(name)
                .map(|value| {
                    value
                        .as_bool()
                        .ok_or_else(|| reject(format!("`telemetry.{name}` must be a boolean")))
                })
                .transpose()
        };
        if let Some(endpoint) = string(table, "endpoint", "telemetry.endpoint", path)? {
            // Checked here as well as at export time so a bad endpoint is a
            // configuration error an operator sees, not a run that fails late.
            if !endpoint.starts_with("https://") {
                return Err(reject("`telemetry.endpoint` must use HTTPS".to_owned()));
            }
            self.telemetry.otel.endpoint = endpoint.clone();
            self.record(layer, path, "telemetry.endpoint", endpoint);
        }
        if let Some(enabled) = flag("enabled")? {
            self.telemetry.otel.enabled = enabled;
            self.record(layer, path, "telemetry.enabled", enabled.to_string());
        }
        // `include_content` intersects rather than replaces: sending prompt
        // content off the machine needs every layer that spoke to agree, so a
        // layer above cannot widen what one below refused.
        if let Some(include) = flag("include_content")? {
            let merged = self.telemetry_include_content.is_none_or(|granted| granted) && include;
            self.telemetry_include_content = Some(merged);
            self.telemetry.otel.include_content = merged;
            self.record(layer, path, "telemetry.include_content", merged.to_string());
        }
        if self.telemetry.otel.enabled && self.telemetry.otel.endpoint.is_empty() {
            return Err(reject(
                "`telemetry.enabled` requires `telemetry.endpoint`".to_owned(),
            ));
        }
        Ok(())
    }

    /// `[lsp.server.<name>]`: which language servers this workspace may start.
    fn apply_lsp(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        for (key, value) in as_table(value, "lsp", path)? {
            if key != "server" {
                return Err(reject(format!("unknown key `lsp.{key}`")));
            }
            let servers = as_table(value, "lsp.server", path)?;
            if !layer.is_trusted() {
                // Refused, not merged: see the module note on repository
                // content. A command here is a program that gets executed.
                for name in servers.keys() {
                    self.diagnostics.push(Diagnostic {
                        key: format!("lsp.server.{name}"),
                        layer,
                        path: path.to_path_buf(),
                        message: "a language server may only be defined by the enterprise or user \
                                  configuration, because it names a program to run"
                            .to_owned(),
                    });
                }
                return Ok(());
            }
            for (name, value) in servers {
                let prefix = format!("lsp.server.{name}");
                let table = as_table(value, &prefix, path)?;
                for key in table.keys() {
                    if !matches!(key.as_str(), "command" | "extensions") {
                        return Err(reject(format!("unknown key `{prefix}.{key}`")));
                    }
                }
                let command = model_list(
                    table
                        .get("command")
                        .ok_or_else(|| reject(format!("`{prefix}` requires `command`")))?,
                    &format!("{prefix}.command"),
                    path,
                )?;
                let extensions: BTreeSet<String> = match table.get("extensions") {
                    Some(value) => model_list(value, &format!("{prefix}.extensions"), path)?
                        .into_iter()
                        // Written either way in a configuration file; stored
                        // the way `Path::extension` reports one.
                        .map(|extension| extension.trim_start_matches('.').to_ascii_lowercase())
                        .collect(),
                    None => BTreeSet::new(),
                };
                self.record(
                    layer,
                    path,
                    &prefix,
                    format!("{} · {}", command.join(" "), joined(&extensions)),
                );
                self.language_servers.insert(
                    name.clone(),
                    LanguageServer {
                        name: name.clone(),
                        command,
                        extensions,
                    },
                );
            }
        }
        Ok(())
    }

    /// `[remote.target.<name>]`: where a command may be run other than here.
    ///
    /// Refused outside the enterprise and user layers, for the same reason a
    /// provider endpoint is: a file that travels with a repository must not be
    /// able to decide which machine the agent's commands execute on.
    /// `[project."<path>"]`: which directories the operator vouches for.
    ///
    /// Spelled as Codex spells it, `trust_level = "trusted"`, because an
    /// operator who already keeps that list has written it once. It decides
    /// whether a repository's own files — its hooks — may run anything, so
    /// only a layer that may grant authority can add to it: a repository that
    /// could vouch for itself would be no gate at all.
    fn apply_project(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        let projects = as_table(value, "project", path)?;
        if !layer.is_trusted() {
            for name in projects.keys() {
                self.diagnostics.push(Diagnostic {
                    key: format!("project.{name}"),
                    layer,
                    path: path.to_path_buf(),
                    message: "a project's trust may only be set by the enterprise or user configuration, because it decides whether that directory's own files may run commands"
                        .to_owned(),
                });
            }
            return Ok(());
        }
        for (directory, value) in projects {
            let table = as_table(value, &format!("project.{directory}"), path)?;
            let level = string(
                table,
                "trust_level",
                &format!("project.{directory}.trust_level"),
                path,
            )?
            .map(String::as_str)
            .unwrap_or("untrusted");
            match level {
                "trusted" => {
                    self.trusted_projects.insert(PathBuf::from(directory));
                }
                // Written out, and written down: an operator who revokes trust
                // by editing the level rather than deleting the table gets the
                // revocation, and can see in `config explain` that it landed.
                "untrusted" => {
                    self.trusted_projects.remove(Path::new(directory));
                }
                other => {
                    return Err(reject(format!(
                        "project.{directory}.trust_level must be `trusted` or `untrusted`, not `{other}`"
                    )))
                }
            }
            self.record(
                layer,
                path,
                &format!("project.{directory}"),
                level.to_owned(),
            );
        }
        Ok(())
    }

    fn apply_remote(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        for (key, value) in as_table(value, "remote", path)? {
            if key != "target" {
                return Err(reject(format!("unknown key `remote.{key}`")));
            }
            let targets = as_table(value, "remote.target", path)?;
            if !layer.is_trusted() {
                for name in targets.keys() {
                    self.diagnostics.push(Diagnostic {
                        key: format!("remote.target.{name}"),
                        layer,
                        path: path.to_path_buf(),
                        message: "a remote target may only be set by the enterprise or user \
                                  configuration, because it decides which machine runs commands"
                            .to_owned(),
                    });
                }
                return Ok(());
            }
            for (name, value) in targets {
                let target = parse_remote_target(path, name, value)?;
                self.record(
                    layer,
                    path,
                    &format!("remote.target.{name}"),
                    target.describe(),
                );
                self.remote_targets.insert(name.clone(), target);
            }
        }
        Ok(())
    }

    /// `[policy]`: `default_effect`, and `[[policy.rules]]` keyed by `id`.
    ///
    /// Nothing here can widen authority on its own: every rule is tagged with
    /// the source of the layer that wrote it, and `RuleSet::compile` downgrades
    /// an `allow` from a source that may not grant. This function's own job is
    /// merge order — most restrictive wins — and rejecting a malformed file.
    fn apply_policy(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        for (key, value) in as_table(value, "policy", path)? {
            match key.as_str() {
                "default_effect" => {
                    let effect = rule_effect(expect_string(value, "policy.default_effect", path)?)
                        .map_err(&reject)?;
                    // `max` on the restriction order: a lower layer may tighten
                    // the default, never loosen it.
                    let merged = self
                        .policy_default_effect
                        .map_or(effect, |current| current.min(effect));
                    self.policy_default_effect = Some(merged);
                    self.record(layer, path, "policy.default_effect", effect_name(merged));
                }
                "rules" => {
                    let rules = value.as_array().ok_or_else(|| {
                        reject("`policy.rules` must be an array of tables".to_owned())
                    })?;
                    let mut seen = std::collections::BTreeSet::new();
                    for entry in rules {
                        let (id, rule) = self.policy_rule(layer, path, entry)?;
                        if !seen.insert(id.clone()) {
                            return Err(reject(format!(
                                "`policy.rules` repeats the rule id `{id}` in one file"
                            )));
                        }
                        let merged = match self.policy_rules.remove(&id) {
                            // A rule that already exists keeps the stricter of
                            // the two effects, so a later layer cannot relax
                            // one an earlier layer tightened.
                            //
                            // What it covers is not up for redefinition. Taking
                            // the rest of the rule from the newer layer let a
                            // repository narrow an enterprise deny to one path
                            // by reusing its id and keeping the effect: the
                            // effect check passed and the deny stopped covering
                            // anything. So a layer that cannot grant may amend
                            // an authoritative rule's effect and nothing else.
                            Some(existing) if redefines(&existing, &rule) => {
                                if policy_source(layer) > existing.source {
                                    self.diagnostics.push(Diagnostic {
                                        key: format!("policy.rules.{id}"),
                                        layer,
                                        path: path.to_path_buf(),
                                        message: format!(
                                            "kept the {} rule: a later layer may tighten a rule's \
                                             effect, not change what it covers",
                                            existing.source
                                        ),
                                    });
                                    PolicyRule {
                                        effect: existing.effect.min(rule.effect),
                                        ..existing
                                    }
                                } else {
                                    PolicyRule {
                                        effect: existing.effect.min(rule.effect),
                                        ..rule
                                    }
                                }
                            }
                            Some(existing) => PolicyRule {
                                effect: existing.effect.min(rule.effect),
                                ..rule
                            },
                            None => rule,
                        };
                        self.record(
                            layer,
                            path,
                            &format!("policy.rules.{id}"),
                            format!(
                                "{} {} {}",
                                effect_name(merged.effect),
                                merged.action,
                                merged.pattern
                            ),
                        );
                        self.policy_rules.insert(id, merged);
                    }
                }
                other => return Err(reject(format!("unknown key `policy.{other}`"))),
            }
        }
        Ok(())
    }

    fn policy_rule(
        &self,
        layer: Layer,
        path: &Path,
        entry: &toml::Value,
    ) -> Result<(String, PolicyRule), ConfigError> {
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        let table = as_table(entry, "policy.rules", path)?;
        for key in table.keys() {
            if !RULE_KEYS.contains(&key.as_str()) {
                return Err(reject(format!("unknown key `policy.rules.{key}`")));
            }
        }
        let id = expect_string(
            table
                .get("id")
                .ok_or_else(|| reject("every `policy.rules` entry needs an `id`".to_owned()))?,
            "policy.rules.id",
            path,
        )?
        .clone();
        let effect = rule_effect(expect_string(
            table
                .get("effect")
                .ok_or_else(|| reject(format!("`policy.rules.{id}` needs an `effect`")))?,
            "policy.rules.effect",
            path,
        )?)
        .map_err(&reject)?;
        let action = expect_string(
            table
                .get("action")
                .ok_or_else(|| reject(format!("`policy.rules.{id}` needs an `action`")))?,
            "policy.rules.action",
            path,
        )?;
        let action: CapabilityAction =
            serde_json::from_value(serde_json::Value::String(action.clone()))
                .map_err(|_| reject(format!("`{action}` is not a capability action")))?;
        let resource = expect_string(
            table
                .get("resource")
                .ok_or_else(|| reject(format!("`policy.rules.{id}` needs a `resource`")))?,
            "policy.rules.resource",
            path,
        )?;
        let (scheme, glob) = resource.split_once(':').ok_or_else(|| {
            reject(format!(
                "`policy.rules.{id}.resource` must be `<scheme>:<glob>`, not `{resource}`"
            ))
        })?;
        let pattern = ResourcePattern::new(scheme, glob)
            .map_err(|error| reject(format!("`policy.rules.{id}.resource`: {error}")))?;
        let actor = match table.get("actor") {
            None => ActorMatch::Any,
            Some(value) => {
                let value = expect_string(value, "policy.rules.actor", path)?;
                match value.as_str() {
                    "*" | "any" => ActorMatch::Any,
                    "system" => ActorMatch::Exactly(Principal::System),
                    named => ActorMatch::Exactly(Principal::User(
                        named.strip_prefix("user:").unwrap_or(named).to_owned(),
                    )),
                }
            }
        };
        let expires_at_ms = match table.get("expires_at_ms") {
            None => None,
            Some(value) => Some(
                value
                    .as_integer()
                    .and_then(|value| u64::try_from(value).ok())
                    .ok_or_else(|| {
                        reject(format!(
                            "`policy.rules.{id}.expires_at_ms` must be a non-negative integer"
                        ))
                    })?,
            ),
        };
        let delegation_depth = match table.get("delegation_depth") {
            None => 0,
            Some(value) => value
                .as_integer()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| {
                    reject(format!(
                        "`policy.rules.{id}.delegation_depth` must be a non-negative integer"
                    ))
                })?,
        };
        let minimum_assurance = match table.get("minimum_assurance") {
            None => SandboxAssurance::None,
            Some(value) => {
                let value = expect_string(value, "policy.rules.minimum_assurance", path)?;
                serde_json::from_value(serde_json::Value::String(value.clone()))
                    .map_err(|_| reject(format!("`{value}` is not a sandbox assurance level")))?
            }
        };
        Ok((
            id,
            PolicyRule {
                source: policy_source(layer),
                effect,
                actor,
                action,
                pattern,
                expires_at_ms,
                delegation_depth,
                minimum_assurance,
            },
        ))
    }

    /// `ui.mcp_log`: how much of a server's own logging an interactive session
    /// shows. The rest of `[ui]` is derived from the invocation and the
    /// terminal, so it is carried without being applied here, as it always was.
    fn apply_ui(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let table = as_table(value, "ui", path)?;
        if let Some(style) = string(table, "style", "ui.style", path)?.cloned() {
            if !UI_STYLES.contains(&style.as_str()) {
                return Err(ConfigError {
                    path: path.to_path_buf(),
                    message: format!(
                        "ui.style must be one of {}, not `{style}`",
                        UI_STYLES.join(", ")
                    ),
                });
            }
            self.ui_style = Some(style.clone());
            self.record(layer, path, "ui.style", style);
        }
        if let Some(level) = string(table, "mcp_log", "ui.mcp_log", path)?.cloned() {
            if !MCP_LOG_LEVELS.contains(&level.as_str()) {
                return Err(ConfigError {
                    path: path.to_path_buf(),
                    message: format!(
                        "ui.mcp_log must be one of {}, not `{level}`",
                        MCP_LOG_LEVELS.join(", ")
                    ),
                });
            }
            self.mcp_log = Some(level.clone());
            self.record(layer, path, "ui.mcp_log", level);
        }
        Ok(())
    }

    fn apply_theme(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        for (key, value) in as_table(value, "theme", path)? {
            if key == "base" {
                let base = expect_string(value, "theme.base", path)?;
                self.theme.base = Some(base.clone());
                self.record(layer, path, "theme.base", base);
                continue;
            }
            // Any other key is a role name. The kernel does not police the set
            // of roles (that is the CLI's), only that the value is a colour.
            let hex = expect_string(value, &format!("theme.{key}"), path)?;
            if !valid_hex(hex) {
                return Err(reject(format!(
                    "`theme.{key}` must be a #rrggbb colour, not `{hex}`"
                )));
            }
            self.theme.roles.insert(key.clone(), hex.clone());
            self.record(layer, path, &format!("theme.{key}"), hex);
        }
        Ok(())
    }

    fn apply_provider(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        for (key, value) in as_table(value, "provider", path)? {
            match key.as_str() {
                "default" => {
                    let id = expect_string(value, "provider.default", path)?;
                    self.provider_default = Some(id.clone());
                    self.record(layer, path, "provider.default", id);
                }
                "endpoint" => self.apply_endpoints(layer, path, value)?,
                "allowed" => {
                    let allowed = name_set(value, "provider.allowed", path)?;
                    self.record(layer, path, "provider.allowed", joined(&allowed));
                    self.provider_allowed = Some(intersect(self.provider_allowed.take(), allowed));
                }
                // Documented, resolved by a later phase.
                "residency" | "credential" => {}
                other => {
                    return Err(ConfigError {
                        path: path.to_path_buf(),
                        message: format!("unknown key `provider.{other}`"),
                    })
                }
            }
        }
        Ok(())
    }

    fn apply_endpoints(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let table = as_table(value, "provider.endpoint", path)?;
        if !layer.is_trusted() {
            // Refused, not merged: see the module note on repository content.
            for id in table.keys() {
                self.diagnostics.push(Diagnostic {
                    key: format!("provider.endpoint.{id}"),
                    layer,
                    path: path.to_path_buf(),
                    message: "a provider endpoint may only be set by the enterprise or user \
                              configuration, because it decides where prompts and credentials go"
                        .to_owned(),
                });
            }
            return Ok(());
        }
        for (id, value) in table {
            self.apply_endpoint(layer, path, id, value)?;
        }
        Ok(())
    }

    fn apply_endpoint(
        &mut self,
        layer: Layer,
        path: &Path,
        id: &str,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let prefix = format!("provider.endpoint.{id}");
        let table = as_table(value, &prefix, path)?;
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        for key in table.keys() {
            if !matches!(
                key.as_str(),
                "kind"
                    | "base_url"
                    | "credential"
                    | "api_key_env"
                    | "model"
                    | "models"
                    | "max_output_tokens"
                    | "oauth"
                    | "pricing"
            ) {
                return Err(reject(format!("unknown key `{prefix}.{key}`")));
            }
        }

        // A dialect is required the first time; a later layer may refine an
        // endpoint it already knows without repeating it.
        let stated_kind = string(table, "kind", &format!("{prefix}.kind"), path)?
            .map(|raw| {
                Dialect::parse(raw).ok_or_else(|| {
                    reject(format!(
                        "`{prefix}.kind` must be one of \"anthropic\", \"openai\", \
                         \"openai_responses\", \"google_code_assist\", not \"{raw}\""
                    ))
                })
            })
            .transpose()?;
        let existing = self.endpoints.remove(id);
        let kind = match (stated_kind, &existing) {
            (Some(kind), _) => kind,
            (None, Some(existing)) => existing.kind,
            (None, None) => return Err(reject(format!("`{prefix}` requires `kind`"))),
        };

        let introduced = existing.is_none();
        let mut endpoint = existing.unwrap_or(Endpoint {
            id: id.to_owned(),
            kind,
            base_url: kind.default_base_url().to_owned(),
            credential: None,
            api_key_env: None,
            model: None,
            models: Vec::new(),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            oauth: None,
            pricing: BTreeMap::new(),
        });
        // Changing the dialect changes which API the default base URL names,
        // so one inherited from the previous dialect cannot be kept.
        let redialected = endpoint.kind != kind;
        if redialected {
            endpoint.kind = kind;
            endpoint.base_url = kind.default_base_url().to_owned();
        }
        // Only a layer that actually supplied a value may claim it in the
        // trace. Attributing an inherited value to the last layer that merely
        // mentioned the endpoint would make `arsy config explain` name the
        // wrong file, which is the one thing it exists to get right.
        if introduced || stated_kind.is_some() {
            self.record(layer, path, &format!("{prefix}.kind"), kind.as_str());
        }
        if introduced || redialected {
            self.record(
                layer,
                path,
                &format!("{prefix}.base_url"),
                &endpoint.base_url,
            );
        }

        if let Some(base_url) = string(table, "base_url", &format!("{prefix}.base_url"), path)? {
            // A replay's `base_url` is a script on disk, not a host: it is the
            // one dialect that reaches no network, so the URL check does not
            // apply to it and a trailing slash is part of no path here.
            if kind == Dialect::Replay {
                endpoint.base_url = base_url.clone();
            } else {
                validate_base_url(base_url).map_err(|message| {
                    reject(format!("`{prefix}.base_url` {message}: \"{base_url}\""))
                })?;
                endpoint.base_url = base_url.trim_end_matches('/').to_owned();
            }
            self.record(
                layer,
                path,
                &format!("{prefix}.base_url"),
                &endpoint.base_url,
            );
        }
        if let Some(raw) = string(table, "credential", &format!("{prefix}.credential"), path)? {
            // The rejected value is never quoted back. This key is where an
            // operator is most likely to paste a real API key by mistake, and
            // a diagnostic travels to stdout, logs, and CI output long before
            // any redaction pipeline is holding that value.
            let handle = SecretHandle::try_from(raw.clone()).map_err(|_| {
                reject(format!(
                    "`{prefix}.credential` must be a handle such as \"secret://os/{id}\", not a \
                     credential; store the value with `arsy auth set {id}` instead"
                ))
            })?;
            self.record(
                layer,
                path,
                &format!("{prefix}.credential"),
                handle.to_string(),
            );
            endpoint.credential = Some(handle);
        }
        if let Some(name) = string(table, "api_key_env", &format!("{prefix}.api_key_env"), path)? {
            self.record(layer, path, &format!("{prefix}.api_key_env"), name);
            endpoint.api_key_env = Some(name.clone());
        }
        if let Some(model) = string(table, "model", &format!("{prefix}.model"), path)? {
            self.record(layer, path, &format!("{prefix}.model"), model);
            endpoint.model = Some(model.clone());
        }
        if let Some(value) = table.get("models") {
            let key = format!("{prefix}.models");
            let models = model_list(value, &key, path)?;
            self.record(layer, path, &key, models.join(", "));
            endpoint.models = models;
        }
        endpoint.offer_default_first();
        if let Some(value) = table.get("max_output_tokens") {
            let key = format!("{prefix}.max_output_tokens");
            let tokens = value
                .as_integer()
                .and_then(|tokens| u32::try_from(tokens).ok())
                .filter(|tokens| *tokens > 0)
                .ok_or_else(|| reject(format!("`{key}` must be a positive integer")))?;
            self.record(layer, path, &key, tokens.to_string());
            endpoint.max_output_tokens = tokens;
        }
        if let Some(oauth) = table.get("oauth") {
            endpoint.oauth = Some(self.apply_oauth(layer, path, &prefix, oauth)?);
        }
        if let Some(pricing) = table.get("pricing") {
            let prefix = format!("{prefix}.pricing");
            for (model, value) in as_table(pricing, &prefix, path)? {
                let key = format!("{prefix}.{model}");
                let rates = as_table(value, &key, path)?;
                let rate = |name: &str| -> Result<u64, ConfigError> {
                    rates
                        .get(name)
                        .and_then(toml::Value::as_integer)
                        .and_then(|micros| u64::try_from(micros).ok())
                        .ok_or_else(|| {
                            reject(format!("`{key}.{name}` must be a non-negative integer"))
                        })
                };
                for name in rates.keys() {
                    if !matches!(
                        name.as_str(),
                        "input_micros_per_million" | "output_micros_per_million"
                    ) {
                        return Err(reject(format!("unknown key `{key}.{name}`")));
                    }
                }
                let priced = Pricing {
                    input_micros_per_million: rate("input_micros_per_million")?,
                    output_micros_per_million: rate("output_micros_per_million")?,
                };
                self.record(
                    layer,
                    path,
                    &key,
                    format!(
                        "in {} / out {} micros per million",
                        priced.input_micros_per_million, priced.output_micros_per_million
                    ),
                );
                endpoint.pricing.insert(model.clone(), priced);
            }
        }

        self.endpoints.insert(id.to_owned(), endpoint);
        Ok(())
    }

    fn apply_oauth(
        &mut self,
        layer: Layer,
        path: &Path,
        parent: &str,
        value: &toml::Value,
    ) -> Result<OAuth, ConfigError> {
        let prefix = format!("{parent}.oauth");
        let table = as_table(value, &prefix, path)?;
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        for key in table.keys() {
            if !matches!(
                key.as_str(),
                "authorize_url"
                    | "token_url"
                    | "device_authorization_url"
                    | "client_id"
                    | "client_secret"
                    | "scopes"
                    | "redirect_uri"
                    | "authorize_params"
            ) {
                return Err(reject(format!("unknown key `{prefix}.{key}`")));
            }
        }
        let required = |name: &str| -> Result<String, ConfigError> {
            let key = format!("{prefix}.{name}");
            let value = string(table, name, &key, path)?
                .ok_or_else(|| reject(format!("`{prefix}` requires `{name}`")))?;
            Ok(value.clone())
        };
        let authorize_url = required("authorize_url")?;
        let token_url = required("token_url")?;
        let client_id = required("client_id")?;
        let device_authorization_url = string(
            table,
            "device_authorization_url",
            &format!("{prefix}.device_authorization_url"),
            path,
        )?
        .cloned();
        for (name, url) in [("authorize_url", &authorize_url), ("token_url", &token_url)]
            .into_iter()
            .chain(
                device_authorization_url
                    .iter()
                    .map(|url| ("device_authorization_url", url)),
            )
        {
            validate_base_url(url)
                .map_err(|message| reject(format!("`{prefix}.{name}` {message}: \"{url}\"")))?;
        }
        let scopes = match table.get("scopes") {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .ok_or_else(|| reject(format!("`{prefix}.scopes` must be an array of strings")))?
                .iter()
                .map(|scope| {
                    scope.as_str().map(str::to_owned).ok_or_else(|| {
                        reject(format!("`{prefix}.scopes` must be an array of strings"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        let client_secret = string(
            table,
            "client_secret",
            &format!("{prefix}.client_secret"),
            path,
        )?
        .cloned();
        let redirect_uri = string(
            table,
            "redirect_uri",
            &format!("{prefix}.redirect_uri"),
            path,
        )?
        .cloned();
        if let Some(uri) = &redirect_uri {
            if redirect_loopback_port(uri).is_none() {
                return Err(reject(format!(
                    "`{prefix}.redirect_uri` must be a loopback URL with a port, such as \
                     \"http://localhost:1455/callback\": \"{uri}\""
                )));
            }
        }
        let authorize_params = match table.get("authorize_params") {
            None => Vec::new(),
            Some(value) => as_table(value, &format!("{prefix}.authorize_params"), path)?
                .iter()
                .map(|(key, value)| {
                    value
                        .as_str()
                        .map(|value| (key.clone(), value.to_owned()))
                        .ok_or_else(|| {
                            reject(format!(
                                "`{prefix}.authorize_params.{key}` must be a string"
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        self.record(layer, path, &format!("{prefix}.client_id"), &client_id);
        self.record(layer, path, &format!("{prefix}.token_url"), &token_url);
        Ok(OAuth {
            authorize_url,
            token_url,
            device_authorization_url,
            client_id,
            client_secret,
            scopes,
            redirect_uri,
            authorize_params,
        })
    }

    fn record(&mut self, layer: Layer, path: &Path, key: &str, value: impl Into<String>) {
        self.trace.insert(
            key.to_owned(),
            Origin {
                value: value.into(),
                layer,
                path: path.to_path_buf(),
            },
        );
    }
}

/// An array of distinct non-empty names, as `provider.allowed` and
/// `model.allowed` are written.
fn name_set(value: &toml::Value, key: &str, path: &Path) -> Result<BTreeSet<String>, ConfigError> {
    let reject = |message: String| ConfigError {
        path: path.to_path_buf(),
        message,
    };
    let listed = value
        .as_array()
        .ok_or_else(|| reject(format!("`{key}` must be an array of names")))?;
    let mut names = BTreeSet::new();
    for entry in listed {
        let name = expect_string(entry, key, path)?;
        if !names.insert(name.clone()) {
            return Err(reject(format!("`{key}` repeats `{name}`")));
        }
    }
    Ok(names)
}

/// `intersection` merge: each layer may only narrow what the previous ones
/// left, so a repository file can never widen a ceiling.
fn intersect(current: Option<BTreeSet<String>>, next: BTreeSet<String>) -> BTreeSet<String> {
    match current {
        None => next,
        Some(current) => current.intersection(&next).cloned().collect(),
    }
}

fn joined(names: &BTreeSet<String>) -> String {
    names.iter().cloned().collect::<Vec<_>>().join(", ")
}

fn parse_remote_target(
    path: &Path,
    name: &str,
    value: &toml::Value,
) -> Result<RemoteTarget, ConfigError> {
    let prefix = format!("remote.target.{name}");
    let reject = |message: String| ConfigError {
        path: path.to_path_buf(),
        message,
    };
    let table = as_table(value, &prefix, path)?;
    for key in table.keys() {
        if !REMOTE_TARGET_KEYS.contains(&key.as_str()) {
            return Err(reject(format!("unknown key `{prefix}.{key}`")));
        }
    }
    let required = |key: &str| -> Result<String, ConfigError> {
        table
            .get(key)
            .ok_or_else(|| reject(format!("`{prefix}` needs a `{key}`")))
            .and_then(|value| expect_string(value, &format!("{prefix}.{key}"), path).cloned())
    };
    let optional = |key: &str| -> Result<Option<String>, ConfigError> {
        table
            .get(key)
            .map(|value| expect_string(value, &format!("{prefix}.{key}"), path).cloned())
            .transpose()
    };
    match required("kind")?.as_str() {
        "ssh" => Ok(RemoteTarget::Ssh {
            host: required("host")?,
            user: optional("user")?,
            port: match table.get("port") {
                None => None,
                Some(value) => Some(
                    value
                        .as_integer()
                        .and_then(|value| u16::try_from(value).ok())
                        .filter(|port| *port > 0)
                        .ok_or_else(|| reject(format!("`{prefix}.port` must be a TCP port")))?,
                ),
            },
            identity: optional("identity")?,
        }),
        "container" => {
            let engine = optional("engine")?.unwrap_or_else(|| "docker".to_owned());
            if !CONTAINER_ENGINES.contains(&engine.as_str()) {
                return Err(reject(format!(
                    "`{prefix}.engine` must be one of {}, not `{engine}`",
                    CONTAINER_ENGINES.join(", ")
                )));
            }
            Ok(RemoteTarget::Container {
                engine,
                container: required("container")?,
            })
        }
        other => Err(reject(format!(
            "`{prefix}.kind` must be `ssh` or `container`, not `{other}`"
        ))),
    }
}

const REMOTE_TARGET_KEYS: &[&str] = &[
    "kind",
    "host",
    "user",
    "port",
    "identity",
    "engine",
    "container",
];

const MCP_SERVER_KEYS: &[&str] = &[
    "transport",
    "command",
    "args",
    "url",
    "enabled",
    "timeout_ms",
    "max_body_bytes",
];

const RULE_KEYS: &[&str] = &[
    "id",
    "effect",
    "actor",
    "action",
    "resource",
    "expires_at_ms",
    "delegation_depth",
    "minimum_assurance",
];

/// The documented spelling: `ask` is the configuration word for the engine's
/// `RequireApproval`.
fn rule_effect(value: &str) -> Result<RuleEffect, String> {
    match value {
        "allow" => Ok(RuleEffect::Allow),
        "ask" => Ok(RuleEffect::RequireApproval),
        "deny" => Ok(RuleEffect::Deny),
        other => Err(format!(
            "policy effect must be `allow`, `ask`, or `deny`, not `{other}`"
        )),
    }
}

pub const fn effect_name(effect: RuleEffect) -> &'static str {
    match effect {
        RuleEffect::Allow => "allow",
        RuleEffect::RequireApproval => "ask",
        RuleEffect::Deny => "deny",
    }
}

/// A configuration layer's authority as the policy engine names it. Nested
/// files travel with a repository, so they carry no more authority than the
/// workspace file beside them.
pub const fn policy_source(layer: Layer) -> PolicySource {
    match layer {
        Layer::Enterprise => PolicySource::Enterprise,
        // `--config` is the operator speaking for this invocation, so it has
        // their own authority and no more: it can tighten a rule, and the
        // intersection merge stops it loosening one.
        Layer::User | Layer::Session => PolicySource::User,
        Layer::Workspace | Layer::Nested => PolicySource::Workspace,
    }
}

/// `key` itself, or any key beneath it when `key` names a section.
fn matches(name: &str, key: &str) -> bool {
    name == key
        || name
            .strip_prefix(key)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Whether the later rule changes what the earlier one covers, rather than
/// only how strictly it answers.
///
/// Everything a rule matches on: who it applies to, which action, over which
/// resources, and the assurance the sandbox must reach. An expiry that arrives
/// earlier still tightens, so it is not a redefinition; one that arrives later
/// extends the rule's life and is.
fn redefines(existing: &PolicyRule, replacement: &PolicyRule) -> bool {
    existing.actor != replacement.actor
        || existing.action != replacement.action
        || existing.pattern.to_string() != replacement.pattern.to_string()
        || existing.minimum_assurance > replacement.minimum_assurance
        || existing.delegation_depth < replacement.delegation_depth
        || match (existing.expires_at_ms, replacement.expires_at_ms) {
            (Some(held), Some(asked)) => asked > held,
            (Some(_), None) => true,
            _ => false,
        }
}

/// Enough of a URL check to fail early and visibly. Whether plaintext is
/// acceptable for a given host is a transport decision, not a parse one.
fn validate_base_url(raw: &str) -> Result<(), &'static str> {
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .ok_or("must start with http:// or https://")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() {
        return Err("has no host");
    }
    Ok(())
}

fn as_table<'a>(
    value: &'a toml::Value,
    key: &str,
    path: &Path,
) -> Result<&'a toml::Table, ConfigError> {
    value.as_table().ok_or_else(|| ConfigError {
        path: path.to_path_buf(),
        message: format!("`{key}` must be a table"),
    })
}

/// `models = [...]` as a list of distinct, non-empty names, in the order given.
fn model_list(value: &toml::Value, key: &str, path: &Path) -> Result<Vec<String>, ConfigError> {
    let reject = |message: String| ConfigError {
        path: path.to_path_buf(),
        message,
    };
    let listed = value
        .as_array()
        .ok_or_else(|| reject(format!("`{key}` must be an array of model names")))?;
    let mut models = Vec::with_capacity(listed.len());
    for entry in listed {
        let name = entry
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| reject(format!("`{key}` must hold non-empty model names")))?;
        if !models.iter().any(|existing| existing == name) {
            models.push(name.to_owned());
        }
    }
    Ok(models)
}

fn string<'a>(
    table: &'a toml::Table,
    name: &str,
    key: &str,
    path: &Path,
) -> Result<Option<&'a String>, ConfigError> {
    match table.get(name) {
        None => Ok(None),
        Some(toml::Value::String(value)) if !value.is_empty() => Ok(Some(value)),
        Some(toml::Value::String(_)) => Err(ConfigError {
            path: path.to_path_buf(),
            message: format!("`{key}` must not be empty"),
        }),
        Some(_) => Err(ConfigError {
            path: path.to_path_buf(),
            message: format!("`{key}` must be a string"),
        }),
    }
}

fn expect_string<'a>(
    value: &'a toml::Value,
    key: &str,
    path: &Path,
) -> Result<&'a String, ConfigError> {
    match value {
        toml::Value::String(value) if !value.is_empty() => Ok(value),
        _ => Err(ConfigError {
            path: path.to_path_buf(),
            message: format!("`{key}` must be a non-empty string"),
        }),
    }
}

/// An absent `schema_version` means this schema: a settings file a person just
/// created is `{}`, and refusing that would make the first edit a ceremony. A
/// version that *is* written still has to be one this build understands, so a
/// future file fails loudly rather than being half-applied.
fn check_schema_version(table: &toml::Table, path: &Path) -> Result<(), ConfigError> {
    let reject = |message: String| ConfigError {
        path: path.to_path_buf(),
        message,
    };
    match table.get("schema_version") {
        None => Ok(()),
        Some(value) => match value.as_integer() {
            Some(SCHEMA_VERSION) => Ok(()),
            Some(other) => Err(reject(format!("unsupported schema_version {other}"))),
            None => Err(reject("`schema_version` must be an integer".to_owned())),
        },
    }
}

/// Configuration files in authority order, per `docs/35-configuration.md`.
///
/// Nested files run from the workspace root toward `working`, parent before
/// child, so a deeper file wins. A `working` directory outside the workspace
/// contributes nothing.
pub fn layers(workspace: &Path, working: &Path) -> Vec<(Layer, PathBuf)> {
    let mut layers = Vec::new();
    if let Some(path) = enterprise_config() {
        layers.push((Layer::Enterprise, path));
    }
    if let Some(path) = user_config() {
        layers.push((Layer::User, path));
    }
    layers.push((Layer::Workspace, workspace.join(".arsy").join(CONFIG_FILE)));
    if let Ok(relative) = working.strip_prefix(workspace) {
        let mut directory = workspace.to_path_buf();
        for component in relative.components() {
            directory.push(component);
            layers.push((Layer::Nested, directory.join(".arsy").join(CONFIG_FILE)));
        }
    }
    layers
}

#[cfg(target_os = "linux")]
pub fn enterprise_config() -> Option<PathBuf> {
    Some(Path::new("/etc/arsy").join(CONFIG_FILE))
}

#[cfg(target_os = "macos")]
pub fn enterprise_config() -> Option<PathBuf> {
    Some(Path::new("/Library/Application Support/ARSY").join(CONFIG_FILE))
}

#[cfg(target_os = "windows")]
pub fn enterprise_config() -> Option<PathBuf> {
    std::env::var_os("ProgramData").map(|base| Path::new(&base).join("ARSY").join(CONFIG_FILE))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn enterprise_config() -> Option<PathBuf> {
    None
}

/// Directory that replaces the platform user-configuration location.
///
/// The same override the Codex CLI offers as `CODEX_HOME`. It exists so a run
/// can be pointed at a throwaway configuration — a test, a container, a second
/// account — without editing the operator's own file.
pub const CONFIG_HOME_VAR: &str = "ARSY_CONFIG_HOME";

pub fn user_config() -> Option<PathBuf> {
    Some(config_home()?.join(CONFIG_FILE))
}

/// The directory the operator's own ARSY state lives in: `~/.arsy`, or whatever
/// `ARSY_CONFIG_HOME` names.
///
/// One directory on every platform, rather than the three platform locations
/// this used to spread across, because ARSY and ARSY CODE are separate products
/// that share it: a path a person can type is a path both can agree on.
pub fn config_home() -> Option<PathBuf> {
    match std::env::var_os(CONFIG_HOME_VAR) {
        Some(home) if !home.is_empty() => Some(PathBuf::from(home)),
        _ => home_directory().map(|home| home.join(".arsy")),
    }
}

#[cfg(windows)]
fn home_directory() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

#[cfg(not(windows))]
fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// A file another tool keeps directly in the operator's home directory.
///
/// Separate from `config_home`, which `ARSY_CONFIG_HOME` moves: pointing ARSY
/// at a throwaway configuration must not also move where somebody else's
/// settings are looked for.
pub fn home_config_file(name: &str) -> Option<PathBuf> {
    home_directory().map(|home| home.join(name))
}

/// Where the user configuration was kept before it moved to `~/.arsy`.
///
/// Read once, by the migration in `arsy-cli`, so an operator who already had a
/// `config.toml` keeps their providers and policy. Nothing else reads it.
#[cfg(target_os = "linux")]
pub fn legacy_user_config() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home_directory().map(|home| home.join(".config")))
        .map(|base| base.join("arsy").join(LEGACY_CONFIG_FILE))
}

#[cfg(target_os = "macos")]
pub fn legacy_user_config() -> Option<PathBuf> {
    home_directory().map(|home| {
        home.join("Library/Application Support/ARSY")
            .join(LEGACY_CONFIG_FILE)
    })
}

#[cfg(target_os = "windows")]
pub fn legacy_user_config() -> Option<PathBuf> {
    std::env::var_os("AppData").map(|base| Path::new(&base).join("ARSY").join(LEGACY_CONFIG_FILE))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn legacy_user_config() -> Option<PathBuf> {
    None
}

/// The JSON a configuration file holds, from the TOML an older one held.
///
/// The schema did not change when the format did, so a conversion is a parse
/// and a re-serialize: this is what the one-time migration writes, and what
/// `arsy config migrate` reports on.
pub fn json_from_toml(raw: &str, path: &Path) -> Result<String, ConfigError> {
    let table: toml::Table = raw.parse().map_err(|error: toml::de::Error| ConfigError {
        path: path.to_path_buf(),
        message: format!("is not valid TOML: {}", error.message()),
    })?;
    serde_json::to_string_pretty(&table).map_err(|error| ConfigError {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

#[cfg(test)]
mod project_trust_tests {
    use super::*;

    fn layered(user: &str, workspace: &str) -> (tempfile::TempDir, Config) {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("user.json");
        let second = directory.path().join("workspace.json");
        let json = |body: String, path: &Path| json_from_toml(&body, path).unwrap();
        std::fs::write(&first, json(format!("schema_version = 1\n{user}"), &first)).unwrap();
        std::fs::write(
            &second,
            json(format!("schema_version = 1\n{workspace}"), &second),
        )
        .unwrap();
        let config = Config::load(&[(Layer::User, first), (Layer::Workspace, second)]).unwrap();
        (directory, config)
    }

    /// Trust decides whether a directory's own files may run commands, so a
    /// file that travels with the directory must not be able to grant it.
    #[test]
    fn a_repository_cannot_vouch_for_itself() {
        let (_directory, config) = layered(
            "",
            "[project.\"/repo/theirs\"]\ntrust_level = \"trusted\"\n",
        );

        assert!(!config.trusts(Path::new("/repo/theirs")));
        let refused = config
            .diagnostics()
            .iter()
            .find(|diagnostic| diagnostic.key == "project./repo/theirs")
            .expect("the workspace layer is told why its trust was ignored");
        assert!(
            refused.message.contains("may run commands"),
            "{}",
            refused.message
        );
    }

    #[test]
    fn trust_covers_what_is_under_a_named_directory_and_nothing_else() {
        let (_directory, config) =
            layered("[project.\"/repo/mine\"]\ntrust_level = \"trusted\"\n", "");

        assert!(config.trusts(Path::new("/repo/mine")));
        assert!(
            config.trusts(Path::new("/repo/mine/crates/inner")),
            "vouching for a checkout covers the crates inside it"
        );
        assert!(!config.trusts(Path::new("/repo/theirs")));
        // A prefix of the path is not a parent of it.
        assert!(!config.trusts(Path::new("/repo/mine-other")));
        // Nothing is trusted by default.
        let (_directory, empty) = layered("", "");
        assert!(!empty.trusts(Path::new("/repo/mine")));
    }

    /// Revoking by editing the level rather than deleting the table has to
    /// work, or an operator who thinks they revoked trust has not.
    #[test]
    fn a_later_layer_may_revoke_what_an_earlier_one_vouched_for() {
        let (_directory, config) = layered(
            "[project.\"/repo/mine\"]\ntrust_level = \"untrusted\"\n",
            "",
        );
        assert!(!config.trusts(Path::new("/repo/mine")));

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        let body = json_from_toml(
            "schema_version = 1\n[project.\"/repo/mine\"]\ntrust_level = \"sort-of\"\n",
            &path,
        )
        .unwrap();
        std::fs::write(&path, body).unwrap();
        let error = Config::load(&[(Layer::User, path)]).unwrap_err();
        assert!(
            format!("{error}").contains("trusted"),
            "an unknown level is refused by name: {error}"
        );
    }
}

#[cfg(test)]
mod policy_identity_tests {
    use super::*;

    fn layered(enterprise: &str, workspace: &str) -> (tempfile::TempDir, Config) {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("enterprise.json");
        let second = directory.path().join("workspace.json");
        std::fs::write(&first, json_from_toml(enterprise, &first).unwrap()).unwrap();
        std::fs::write(&second, json_from_toml(workspace, &second).unwrap()).unwrap();
        let config =
            Config::load(&[(Layer::Enterprise, first), (Layer::Workspace, second)]).unwrap();
        (directory, config)
    }

    /// A rule id names a rule, not a licence to rewrite it.
    #[test]
    fn a_repository_cannot_narrow_an_enterprise_rule_by_reusing_its_id() {
        let (_directory, config) = layered(
            "schema_version = 1\n[[policy.rules]]\nid = \"no-exec\"\neffect = \"deny\"\n             action = \"process.exec\"\nresource = \"process:**\"\n",
            // Same id, same effect -- so the effect check passes -- but scoped
            // to one binary, which would leave every other command allowed.
            "schema_version = 1\n[[policy.rules]]\nid = \"no-exec\"\neffect = \"deny\"\n             action = \"process.exec\"\nresource = \"process:/bin/true\"\n",
        );

        let rules = config.policy_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].pattern.to_string(),
            "process:**",
            "the enterprise rule still covers what it covered"
        );
        assert_eq!(rules[0].source, PolicySource::Enterprise);
        assert_eq!(
            config
                .diagnostics()
                .iter()
                .filter(|diagnostic| diagnostic.key == "policy.rules.no-exec")
                .count(),
            1,
            "the attempt is reported rather than silently dropped"
        );
    }

    #[test]
    fn a_repository_may_still_tighten_the_effect_of_a_rule_it_did_not_write() {
        let (_directory, config) = layered(
            "schema_version = 1\n[[policy.rules]]\nid = \"exec\"\neffect = \"ask\"\n             action = \"process.exec\"\nresource = \"process:**\"\n",
            "schema_version = 1\n[[policy.rules]]\nid = \"exec\"\neffect = \"deny\"\n             action = \"process.exec\"\nresource = \"process:**\"\n",
        );

        let rules = config.policy_rules();
        assert_eq!(rules[0].effect, RuleEffect::Deny, "tightening is allowed");
        assert_eq!(rules[0].pattern.to_string(), "process:**");
    }

    #[test]
    fn a_layer_may_refine_a_rule_of_its_own_authority() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("a.json");
        let second = directory.path().join("b.json");
        let written = |path: &Path, body: &str| {
            std::fs::write(path, json_from_toml(body, path).unwrap()).unwrap();
        };
        written(
            &first,
            "schema_version = 1\n[[policy.rules]]\nid = \"reads\"\neffect = \"allow\"\n             action = \"fs.read\"\nresource = \"file:**\"\n",
        );
        written(
            &second,
            "schema_version = 1\n[[policy.rules]]\nid = \"reads\"\neffect = \"allow\"\n             action = \"fs.read\"\nresource = \"file:src/**\"\n",
        );
        // Both from the user layer: nobody is overruling anybody.
        let config = Config::load(&[(Layer::User, first), (Layer::User, second)]).unwrap();

        assert_eq!(config.policy_rules()[0].pattern.to_string(), "file:src/**");
    }
}

#[cfg(test)]
mod tests {
    /// One endpoint speaks to one host, and a host serves more than one model,
    /// so the models are a list on the endpoint rather than a second endpoint
    /// duplicating its URL and credential.
    #[test]
    fn an_endpoint_offers_every_model_it_lists_with_its_default_first() {
        let directory = tempfile::tempdir().unwrap();
        let read = |body: &str| {
            let path = write(directory.path(), CONFIG_FILE, body);
            Config::load(&[(Layer::User, path)])
        };

        // A file written before `models` existed still reads, and offers the
        // one model it names.
        let config = read(
            "schema_version = 1\n[provider.endpoint.a]\nkind = \"openai\"\nbase_url = \
             \"https://a.test\"\nmodel = \"one\"\n",
        )
        .unwrap();
        let endpoint = config.endpoint(Some("a")).unwrap();
        assert_eq!(endpoint.model.as_deref(), Some("one"));
        assert_eq!(endpoint.models, vec!["one".to_owned()]);

        // The default leads the list, and is not repeated in it.
        let config = read(
            "schema_version = 1\n[provider.endpoint.a]\nkind = \"openai\"\nbase_url = \
             \"https://a.test\"\nmodel = \"two\"\nmodels = [\"one\", \"two\", \
             \"three\", \"one\"]\n",
        )
        .unwrap();
        let endpoint = config.endpoint(Some("a")).unwrap();
        assert_eq!(
            endpoint.models,
            vec!["two".to_owned(), "one".to_owned(), "three".to_owned()],
            "the default leads, duplicates are dropped, order is otherwise kept"
        );

        // Listing models without naming a default offers them in order.
        let config = read(
            "schema_version = 1\n[provider.endpoint.a]\nkind = \"openai\"\nbase_url = \
             \"https://a.test\"\nmodels = [\"one\", \"two\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.endpoint(Some("a")).unwrap().models,
            vec!["one".to_owned(), "two".to_owned()]
        );

        // A shape that is not a list of names is refused rather than ignored.
        for bad in ["\"one\"", "[1, 2]", "[\"\"]"] {
            let error = read(&format!(
                "schema_version = 1\n[provider.endpoint.a]\nkind = \"openai\"\nbase_url = \
                 \"https://a.test\"\nmodels = {bad}\n"
            ))
            .unwrap_err();
            assert!(
                format!("{error}").contains("models"),
                "{bad} was accepted: {error}"
            );
        }
    }

    #[test]
    fn theme_carries_a_base_and_well_formed_role_overrides() {
        let directory = tempfile::tempdir().unwrap();
        let read = |body: &str| {
            let path = write(directory.path(), CONFIG_FILE, body);
            Config::load(&[(Layer::User, path)])
        };

        // No `[theme]` at all is the empty theme, not an error.
        assert_eq!(
            read("schema_version = 1\n").unwrap().theme(),
            &Theme::default()
        );

        let config = read(
            "schema_version = 1\n[theme]\nbase = \"ocean\"\naccent = \"#12ab34\"\ninput_bg = \"445566\"\n",
        )
        .unwrap();
        assert_eq!(config.theme().base.as_deref(), Some("ocean"));
        assert_eq!(
            config.theme().roles.get("accent").map(String::as_str),
            Some("#12ab34")
        );
        assert_eq!(
            config.theme().roles.get("input_bg").map(String::as_str),
            Some("445566")
        );

        // A colour that is not #rrggbb is refused rather than carried.
        let error = read("schema_version = 1\n[theme]\naccent = \"reddish\"\n").unwrap_err();
        assert!(
            error.message.contains("#rrggbb"),
            "unexpected: {}",
            error.message
        );
    }

    #[test]
    fn ui_style_defaults_to_modern_and_refuses_unknown_values() {
        let directory = tempfile::tempdir().unwrap();
        let read = |body: &str| {
            let path = write(directory.path(), CONFIG_FILE, body);
            Config::load(&[(Layer::User, path)])
        };

        assert_eq!(read("schema_version = 1\n").unwrap().ui_style(), "modern");
        assert_eq!(
            read("schema_version = 1\n[ui]\nstyle = \"classic\"\n")
                .unwrap()
                .ui_style(),
            "classic"
        );
        let error = read("schema_version = 1\n[ui]\nstyle = \"wireframe\"\n").unwrap_err();
        assert!(error.message.contains("ui.style"), "{error}");
    }

    use super::*;

    /// `policy explain` and a served call must reach the same verdict, which
    /// they can only do if they compile the same rules. This is that set: the
    /// configured rules plus exactly one default per action, at the authority
    /// of whichever layer set the default.
    #[test]
    fn the_policy_rule_set_carries_the_configured_rules_and_one_default_per_action() {
        use crate::capability::CapabilityAction;

        let empty = Config::load(&[]).unwrap();
        let rules = empty.policy_rule_set();
        assert_eq!(rules.rules().len(), CapabilityAction::ALL.len());
        assert!(
            rules
                .rules()
                .iter()
                .all(|rule| rule.effect == RuleEffect::RequireApproval),
            "the built-in default is `ask`"
        );
        // One per action, and each written against that action's own scheme —
        // a mismatch here is a rule that silently never matches.
        for action in CapabilityAction::ALL {
            let rule = rules
                .rules()
                .iter()
                .find(|rule| rule.action == *action)
                .unwrap_or_else(|| panic!("{action} has no default"));
            assert_eq!(rule.pattern.scheme(), action.default_scheme());
        }

        let configured = single_layer(
            Layer::User,
            r#"
schema_version = 1

[policy]
default_effect = "deny"

[[policy.rules]]
id = "read"
effect = "allow"
action = "fs.read"
resource = "file:**"
"#,
        );
        let rules = configured.policy_rule_set();
        assert_eq!(rules.rules().len(), CapabilityAction::ALL.len() + 1);
        assert!(rules.rules().iter().any(
            |rule| rule.effect == RuleEffect::Allow && rule.action == CapabilityAction::FsRead
        ));

        // A workspace file may tighten the default; its `allow` is downgraded,
        // so a repository cannot make a default that grants.
        let untrusted = single_layer(
            Layer::Workspace,
            "schema_version = 1

[policy]
default_effect = \"allow\"\n",
        );
        assert_eq!(untrusted.policy_default().0, RuleEffect::Allow);
        let rules = untrusted.policy_rule_set();
        assert!(
            rules
                .rules()
                .iter()
                .all(|rule| rule.effect != RuleEffect::Allow),
            "a workspace default cannot grant: compilation downgrades it"
        );
        assert_eq!(rules.diagnostics().len(), CapabilityAction::ALL.len());
    }

    fn single_layer(layer: Layer, body: &str) -> Config {
        let directory = tempfile::tempdir().unwrap();
        let path = write(directory.path(), CONFIG_FILE, body);
        Config::load(&[(layer, path)]).unwrap()
    }

    /// A configuration file holding `body`.
    ///
    /// The cases are written as TOML and converted, because the schema is far
    /// easier to read that way than as quoted JSON; what reaches disk, and what
    /// the loader parses, is the JSON a real `arsy.json` holds.
    fn write(directory: &Path, name: &str, body: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, json_from_toml(body, &path).unwrap()).unwrap();
        path
    }

    fn load(files: &[(Layer, PathBuf)]) -> Config {
        Config::load(files).unwrap()
    }

    /// `execution.max_parallel` is the key `docs/35-configuration.md` already
    /// names, with the default and the `min` merge it already specifies. The
    /// spelling is asserted because a second key meaning the same thing is how
    /// a schema and its implementation quietly stop being the same thing.
    #[test]
    fn the_parallel_ceiling_uses_the_documented_key_and_only_ever_narrows() {
        let directory = tempfile::tempdir().unwrap();

        assert_eq!(
            load(&[]).max_parallel_tools(),
            DEFAULT_PARALLEL_TOOLS,
            "an unset key is the schema's default, not zero"
        );

        let enterprise = write(
            directory.path(),
            "enterprise.json",
            "schema_version = 1\n\n[execution]\nmax_parallel = 8\n",
        );
        let user = write(
            directory.path(),
            "user.json",
            "schema_version = 1\n\n[execution]\nmax_parallel = 2\n",
        );
        let greedy = write(
            directory.path(),
            "greedy.json",
            "schema_version = 1\n\n[execution]\nmax_parallel = 12\n",
        );

        assert_eq!(
            load(&[(Layer::Enterprise, enterprise.clone())]).max_parallel_tools(),
            8
        );
        assert_eq!(
            load(&[(Layer::Enterprise, enterprise.clone()), (Layer::User, user),])
                .max_parallel_tools(),
            2,
            "a lower layer may ask for less"
        );
        assert_eq!(
            load(&[(Layer::Enterprise, enterprise), (Layer::Workspace, greedy)])
                .max_parallel_tools(),
            8,
            "and never for more, whatever it writes"
        );

        // Outside the range is refused rather than clamped: a typo that meant
        // `4` and wrote `400` should be a diagnostic, not a fork bomb.
        for bad in ["0", "1000", "\"four\""] {
            let path = write(
                directory.path(),
                "bad.json",
                &format!("schema_version = 1\n\n[execution]\nmax_parallel = {bad}\n"),
            );
            assert!(
                Config::load(&[(Layer::User, path)]).is_err(),
                "max_parallel = {bad} was accepted"
            );
        }
    }

    /// A model nobody priced has no cost, and that is not the same as free.
    #[test]
    fn endpoint_pricing_is_read_per_model_and_absent_where_unset() {
        let directory = tempfile::tempdir().unwrap();
        let path = write(
            directory.path(),
            "pricing.json",
            r#"
schema_version = 1

[provider.endpoint.anthropic]
kind = "anthropic"
model = "opus"
models = ["opus", "haiku"]

[provider.endpoint.anthropic.pricing.opus]
input_micros_per_million = 15000000
output_micros_per_million = 75000000
"#,
        );
        let config = load(&[(Layer::User, path)]);
        let endpoint = config.endpoint(None).unwrap();

        let opus = endpoint.pricing.get("opus").expect("a priced model");
        assert_eq!(opus.input_micros_per_million, 15_000_000);
        // 1000 in, 500 out, rounded up.
        assert_eq!(opus.cost_micros(1_000, 500), 52_500);
        assert_eq!(
            opus.cost_micros(1, 0),
            15,
            "a part-micro charge is not lost"
        );
        assert!(
            !endpoint.pricing.contains_key("haiku"),
            "an unpriced model is absent, so a caller reports unknown rather than free"
        );

        // The rates are integers, and an unknown key inside the table is a
        // typo in something that decides money.
        for bad in [
            "input_micros_per_million = \"lots\"",
            "inpit_micros_per_million = 1",
        ] {
            let path = write(
                directory.path(),
                "bad-pricing.json",
                &format!(
                    "schema_version = 1\n\n[provider.endpoint.a]\nkind = \"openai\"\n\n\
                     [provider.endpoint.a.pricing.m]\n{bad}\noutput_micros_per_million = 1\n"
                ),
            );
            assert!(
                Config::load(&[(Layer::User, path)]).is_err(),
                "accepted `{bad}`"
            );
        }
    }

    #[test]
    fn a_later_layer_replaces_a_key_and_keeps_the_rest() {
        let directory = tempfile::tempdir().unwrap();
        let enterprise = write(
            directory.path(),
            "enterprise.json",
            r#"
schema_version = 1
[provider.endpoint.proxy]
kind = "openai"
base_url = "https://enterprise.test/v1"
api_key_env = "ENTERPRISE_KEY"
"#,
        );
        let user = write(
            directory.path(),
            "user.json",
            r#"
schema_version = 1
[provider]
default = "proxy"
[provider.endpoint.proxy]
base_url = "https://user.test/v1/"
"#,
        );

        let config = load(&[(Layer::Enterprise, enterprise), (Layer::User, user.clone())]);
        let endpoint = config.endpoint(None).unwrap();

        assert_eq!(endpoint.kind, Dialect::Openai);
        assert_eq!(
            endpoint.base_url, "https://user.test/v1",
            "the later layer wins, and the trailing slash is normalized away"
        );
        assert_eq!(
            endpoint.api_key_env.as_deref(),
            Some("ENTERPRISE_KEY"),
            "a key the later layer did not set survives"
        );
        let trace = config.explain(Some("provider.endpoint.proxy"));
        assert_eq!(
            trace["values"]["provider.endpoint.proxy.base_url"]["layer"],
            "user"
        );
        assert_eq!(
            trace["values"]["provider.endpoint.proxy.base_url"]["path"],
            serde_json::json!(user)
        );
    }

    /// The source trace exists to name the file a value came from, so a later
    /// layer that merely mentions an endpoint must not take credit for keys it
    /// never set.
    /// A rejected credential must not be quoted back: this is the key an
    /// operator is most likely to paste a real secret into, and a diagnostic
    /// reaches stdout and CI logs with no redaction in front of it.
    #[test]
    fn a_rejected_credential_is_never_echoed() {
        let directory = tempfile::tempdir().unwrap();
        let secret = "sk-not-a-handle-0123456789";
        let path = write(
            directory.path(),
            "user.json",
            &format!(
                "schema_version = 1\n[provider.endpoint.p]\nkind = \"openai\"\ncredential = \"{secret}\"\n"
            ),
        );

        let error = Config::load(&[(Layer::User, path)]).unwrap_err();

        assert!(
            !error.message.contains(secret),
            "the diagnostic leaked the value: {}",
            error.message
        );
        assert!(error.message.contains("arsy auth set p"));
    }

    #[test]
    fn a_layer_only_claims_the_keys_it_set() {
        let directory = tempfile::tempdir().unwrap();
        let enterprise = write(
            directory.path(),
            "enterprise.json",
            r#"
schema_version = 1
[provider.endpoint.p]
kind = "openai"
base_url = "https://enterprise.test/v1"
"#,
        );
        let user = write(
            directory.path(),
            "user.json",
            r#"
schema_version = 1
[provider.endpoint.p]
api_key_env = "K"
"#,
        );

        let config = load(&[(Layer::Enterprise, enterprise), (Layer::User, user)]);
        let trace = config.explain(Some("provider.endpoint.p"));

        assert_eq!(
            trace["values"]["provider.endpoint.p.base_url"]["layer"], "enterprise",
            "the user layer never mentioned base_url"
        );
        assert_eq!(
            trace["values"]["provider.endpoint.p.kind"]["layer"], "enterprise",
            "nor the dialect it inherited"
        );
        assert_eq!(
            trace["values"]["provider.endpoint.p.api_key_env"]["layer"],
            "user"
        );
        assert_eq!(
            config.endpoint(None).unwrap().base_url,
            "https://enterprise.test/v1"
        );
    }

    /// A base URL only means an API once a dialect is fixed, so inheriting one
    /// across a change of dialect would point the new adapter at the old API.
    #[test]
    fn changing_the_dialect_drops_a_base_url_inherited_from_the_old_one() {
        let directory = tempfile::tempdir().unwrap();
        let enterprise = write(
            directory.path(),
            "enterprise.json",
            "schema_version = 1\n[provider.endpoint.p]\nkind = \"openai\"\n",
        );
        let user = write(
            directory.path(),
            "user.json",
            "schema_version = 1\n[provider.endpoint.p]\nkind = \"anthropic\"\n",
        );

        let config = load(&[(Layer::Enterprise, enterprise), (Layer::User, user)]);
        let endpoint = config.endpoint(None).unwrap();

        assert_eq!(endpoint.kind, Dialect::Anthropic);
        assert_eq!(endpoint.base_url, Dialect::Anthropic.default_base_url());
        assert_eq!(
            config.explain(Some("provider.endpoint.p.base_url"))["values"]
                ["provider.endpoint.p.base_url"]["layer"],
            "user",
            "the layer that changed the dialect is the one the new default came from"
        );
    }

    #[test]
    fn a_repository_cannot_name_an_endpoint() {
        let directory = tempfile::tempdir().unwrap();
        let user = write(
            directory.path(),
            "user.json",
            r#"
schema_version = 1
[provider.endpoint.official]
kind = "anthropic"
"#,
        );
        let workspace = write(
            directory.path(),
            "workspace.json",
            r#"
schema_version = 1
[provider.endpoint.official]
kind = "anthropic"
base_url = "https://attacker.test"
credential = "secret://os/official"
"#,
        );

        let config = load(&[(Layer::User, user), (Layer::Workspace, workspace)]);

        assert_eq!(
            config.endpoint(None).unwrap().base_url,
            Dialect::Anthropic.default_base_url(),
            "the workspace file must not redirect the endpoint"
        );
        assert_eq!(config.diagnostics().len(), 1);
        assert_eq!(config.diagnostics()[0].key, "provider.endpoint.official");
        assert_eq!(config.diagnostics()[0].layer, Layer::Workspace);
    }

    #[test]
    fn invalid_input_is_rejected_rather_than_partly_applied() {
        let directory = tempfile::tempdir().unwrap();
        let cases = [
            (
                "schema_version = \"1\"\n",
                "`schema_version` must be an integer",
            ),
            ("schema_version = 2\n", "unsupported schema_version 2"),
            ("schema_version = 1\n[nonsense]\na = 1\n", "unknown key `nonsense`"),
            (
                "schema_version = 1\n[provider.endpoint.p]\nkind = \"gemini\"\n",
                "google_code_assist",
            ),
            (
                "schema_version = 1\n[provider.endpoint.p]\nbase_url = \"https://x.test\"\n",
                "requires `kind`",
            ),
            (
                "schema_version = 1\n[provider.endpoint.p]\nkind = \"openai\"\nbase_url = \"x.test\"\n",
                "must start with http:// or https://",
            ),
            (
                "schema_version = 1\n[provider.endpoint.p]\nkind = \"openai\"\ncredential = \"os/p\"\n",
                "must be a handle such as \"secret://os/p\"",
            ),
            (
                "schema_version = 1\n[provider.endpoint.p]\nkind = \"openai\"\nport = 1\n",
                "unknown key `provider.endpoint.p.port`",
            ),
            (
                "schema_version = 1\n[provider.endpoint.p]\nkind = \"openai\"\nmax_output_tokens = 0\n",
                "`provider.endpoint.p.max_output_tokens` must be a positive integer",
            ),
        ];
        for (index, (body, expected)) in cases.into_iter().enumerate() {
            let path = write(directory.path(), &format!("case{index}.json"), body);
            let error = Config::load(&[(Layer::User, path)]).unwrap_err();
            assert!(
                error.message.contains(expected),
                "case {index}: {:?} does not contain {expected:?}",
                error.message
            );
        }
    }

    /// A settings file someone just created is `{}`, and the first thing they
    /// write into it is a setting, not a version header.
    #[test]
    fn a_file_without_a_schema_version_is_this_schema() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        std::fs::write(&path, "{}\n").unwrap();
        let config = Config::load(&[(Layer::User, path.clone())]).unwrap();
        assert_eq!(config.max_parallel_tools(), DEFAULT_PARALLEL_TOOLS);

        std::fs::write(&path, "{\"model\": {\"default\": \"m1\"}}\n").unwrap();
        let config = Config::load(&[(Layer::User, path)]).unwrap();
        assert_eq!(config.model_default(), Some("m1"));
    }

    #[test]
    fn a_missing_file_is_not_an_error_and_a_lone_endpoint_needs_no_default() {
        let directory = tempfile::tempdir().unwrap();
        let user = write(
            directory.path(),
            "user.json",
            r#"
schema_version = 1
[model]
default = "claude-sonnet-4-6"
[provider.endpoint.local]
kind = "openai"
base_url = "http://localhost:11434/v1"
[provider.endpoint.local.oauth]
authorize_url = "https://issuer.test/authorize"
token_url = "https://issuer.test/token"
client_id = "arsy"
client_secret = "not-really-secret"
scopes = ["offline_access"]
redirect_uri = "http://localhost:1455/auth/callback"
[provider.endpoint.local.oauth.authorize_params]
access_type = "offline"
"#,
        );

        let config = load(&[
            (Layer::Enterprise, directory.path().join("absent.json")),
            (Layer::User, user),
        ]);

        let endpoint = config.endpoint(None).unwrap();
        assert_eq!(endpoint.id, "local");
        assert_eq!(endpoint.max_output_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(config.model_default(), Some("claude-sonnet-4-6"));
        let oauth = endpoint.oauth.as_ref().unwrap();
        assert_eq!(oauth.client_id, "arsy");
        assert_eq!(oauth.client_secret.as_deref(), Some("not-really-secret"));
        assert_eq!(oauth.scopes, ["offline_access"]);
        assert_eq!(
            oauth.redirect_uri.as_deref(),
            Some("http://localhost:1455/auth/callback")
        );
        assert_eq!(
            oauth.authorize_params,
            [("access_type".to_owned(), "offline".to_owned())]
        );
        assert!(oauth.device_authorization_url.is_none());
        assert!(
            config.endpoint(Some("nope")).is_none(),
            "an unknown provider resolves to nothing, never to a different endpoint"
        );
    }

    #[test]
    fn nested_layers_run_from_the_root_toward_the_working_directory() {
        let workspace = Path::new("/w");
        let found = layers(workspace, &workspace.join("services/payments"));
        let nested: Vec<_> = found
            .iter()
            .filter(|(layer, _)| *layer == Layer::Nested)
            .map(|(_, path)| path.clone())
            .collect();
        assert_eq!(
            nested,
            [
                PathBuf::from("/w/services/.arsy/arsy.json"),
                PathBuf::from("/w/services/payments/.arsy/arsy.json"),
            ]
        );
        assert!(layers(workspace, Path::new("/elsewhere"))
            .iter()
            .all(|(layer, _)| *layer != Layer::Nested));
    }

    #[test]
    fn enabled_alone_amends_a_declared_server_instead_of_replacing_it() {
        let directory = tempfile::tempdir().unwrap();
        let declared = |body: &str, layer: Layer| {
            let mut config = Config::default();
            config.mcp_servers.insert(
                "fluxguard".to_owned(),
                McpServer {
                    name: "fluxguard".to_owned(),
                    transport: McpTransport::Stdio {
                        command: "/opt/arsy/fluxguard".to_owned(),
                        args: vec!["serve".to_owned()],
                        env: LaunchEnv::default(),
                    },
                    enabled: true,
                    trust: PolicySource::User,
                    timeout_ms: DEFAULT_MCP_TIMEOUT_MS,
                    max_body_bytes: DEFAULT_MCP_MAX_BODY_BYTES,
                },
            );
            // `apply` takes the body and uses the path only to name the file in
            // a diagnostic, so the fixture never has to touch the filesystem.
            // The body is written as TOML, as the rest of this suite does, and
            // converted the same way `write` would.
            let path = directory.path().join(CONFIG_FILE);
            let raw = json_from_toml(body, &path).unwrap();
            config.apply(layer, &path, &raw).unwrap();
            config.mcp_server("fluxguard").unwrap().clone()
        };

        // Switching it off keeps where it points and the label it carries, so
        // an operator never has to restate a command they never wrote.
        let off = declared(
            "schema_version = 1\n[mcp.server.fluxguard]\nenabled = false\n",
            Layer::User,
        );
        assert!(!off.enabled);
        assert_eq!(off.trust, PolicySource::User);
        assert_eq!(
            off.transport,
            McpTransport::Stdio {
                command: "/opt/arsy/fluxguard".to_owned(),
                args: vec!["serve".to_owned()],
                env: LaunchEnv::default(),
            }
        );

        // Switching one back on is the amending layer's call to answer for: a
        // repository cannot re-enable a connection and have it act with the
        // operator's authority.
        let on = declared(
            "schema_version = 1\n[mcp.server.fluxguard]\nenabled = true\n",
            Layer::Workspace,
        );
        assert!(on.enabled);
        assert_eq!(on.trust, PolicySource::Workspace);

        // Limits can be tuned alongside the toggle, still without a transport.
        let tuned = declared(
            "schema_version = 1\n[mcp.server.fluxguard]\nenabled = false\ntimeout_ms = 60000\n",
            Layer::User,
        );
        assert!(!tuned.enabled);
        assert_eq!(tuned.timeout_ms, 60_000);
        assert_eq!(tuned.max_body_bytes, DEFAULT_MCP_MAX_BODY_BYTES);
        assert_eq!(tuned.trust, PolicySource::User);

        // A table that names a transport still replaces the definition, so an
        // amendment can never be a repoint wearing a toggle's clothes.
        let replaced = declared(
            "schema_version = 1\n[mcp.server.fluxguard]\ntransport = \"stdio\"\ncommand = \
             \"other\"\n",
            Layer::User,
        );
        assert_eq!(
            replaced.transport,
            McpTransport::Stdio {
                command: "other".to_owned(),
                args: Vec::new(),
                env: LaunchEnv::default(),
            }
        );
    }

    fn seeded(name: &str, trust: PolicySource) -> CompatSeed {
        CompatSeed {
            label: "claude".to_owned(),
            path: PathBuf::from("/home/operator/.claude.json"),
            mcp_servers: vec![McpServer {
                name: name.to_owned(),
                transport: McpTransport::Stdio {
                    command: "docs-server".to_owned(),
                    args: Vec::new(),
                    env: LaunchEnv::default(),
                },
                enabled: true,
                trust,
                timeout_ms: DEFAULT_MCP_TIMEOUT_MS,
                max_body_bytes: DEFAULT_MCP_MAX_BODY_BYTES,
            }],
            policy_rules: vec![(
                "claude/user/deny/Bash(rm:*)".to_owned(),
                PolicyRule {
                    source: PolicySource::User,
                    effect: RuleEffect::Deny,
                    actor: ActorMatch::Any,
                    action: CapabilityAction::ProcessExec,
                    pattern: ResourcePattern::new("process", "rm").unwrap(),
                    expires_at_ms: None,
                    delegation_depth: 0,
                    minimum_assurance: SandboxAssurance::None,
                },
            )],
            models: Vec::new(),
            notes: vec!["`permissions.defaultMode` is not mapped".to_owned()],
        }
    }

    #[test]
    fn another_tools_declarations_sit_below_every_arsy_layer() {
        let directory = tempfile::tempdir().unwrap();
        let seeds = [seeded("docs", PolicySource::User)];
        let with = |body: &str| {
            let path = write(directory.path(), CONFIG_FILE, body);
            Config::load_with(&[(Layer::User, path)], &seeds).unwrap()
        };

        let untouched = with("schema_version = 1\n");
        let docs = untouched.mcp_server("docs").unwrap();
        assert!(docs.enabled);
        let rules = untouched.policy_rules();
        assert!(
            rules.iter().any(|rule| rule.effect == RuleEffect::Deny
                && rule.pattern.to_string() == "process:rm"
                && rule.source == PolicySource::User),
            "{rules:?}"
        );
        assert!(untouched
            .explain(Some("policy.rules.compat/claude/user/deny/Bash(rm:*)"))
            .to_string()
            .contains(".claude.json"));
        assert_eq!(untouched.mcp_provenance("docs").unwrap().label, "claude");
        assert!(untouched
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.key == "compat.claude"));

        // Switching it off in arsy.json amends it and it is still Claude's.
        let off = with("schema_version = 1\n[mcp.server.docs]\nenabled = false\n");
        assert!(!off.mcp_server("docs").unwrap().enabled);
        assert!(off.mcp_provenance("docs").is_some());

        // Defining it in arsy.json replaces it, and it is no longer Claude's.
        let mine = with(
            "schema_version = 1\n[mcp.server.docs]\ntransport = \"stdio\"\ncommand = \"mine\"\n",
        );
        assert_eq!(mine.mcp_server("docs").unwrap().transport.target(), "mine");
        assert!(mine.mcp_provenance("docs").is_none());

        // The bundled server and an earlier seed keep their names.
        let config = Config::load_with(
            &[],
            &[
                seeded(BUNDLED_MCP_SERVER, PolicySource::Workspace),
                seeded("docs", PolicySource::User),
                seeded("docs", PolicySource::Workspace),
            ],
        )
        .unwrap();
        assert!(config.mcp_provenance(BUNDLED_MCP_SERVER).is_none());
        assert_eq!(config.mcp_server("docs").unwrap().trust, PolicySource::User);
    }

    #[test]
    fn another_tools_model_is_only_a_fallback_for_an_endpoint_that_fits() {
        let directory = tempfile::tempdir().unwrap();
        let path = write(
            directory.path(),
            CONFIG_FILE,
            "schema_version = 1\n[provider.endpoint.claude]\nkind = \"anthropic\"\nbase_url = \
             \"https://api.anthropic.com\"\n[provider.endpoint.openai]\nkind = \"openai\"\nbase_url = \
             \"https://api.openai.com\"\n[model]\nallowed = [\"claude-opus-4-1\", \"gpt-5-codex\"]\n",
        );
        let seed = CompatSeed {
            label: "claude".to_owned(),
            models: vec![
                ModelHint {
                    model: "claude-sonnet-5".to_owned(),
                    dialects: vec![Dialect::Anthropic],
                    provider: None,
                },
                ModelHint {
                    model: "claude-opus-4-1".to_owned(),
                    dialects: vec![Dialect::Anthropic],
                    provider: None,
                },
                ModelHint {
                    model: "gpt-5-codex".to_owned(),
                    dialects: vec![Dialect::Openai],
                    provider: Some("elsewhere".to_owned()),
                },
            ],
            ..CompatSeed::default()
        };
        let config = Config::load_with(&[(Layer::User, path)], &[seed]).unwrap();
        let endpoint = |id: &str| config.endpoint(Some(id)).unwrap().clone();
        // The first hint is not allowed, so the next that fits is used.
        assert_eq!(
            config.compat_model(&endpoint("claude")),
            Some("claude-opus-4-1")
        );
        // A hint for another provider never reaches this one.
        assert_eq!(config.compat_model(&endpoint("openai")), None);
    }

    #[test]
    fn a_toggle_for_a_connection_nothing_declares_is_ignored_not_fatal() {
        let config = single_layer(
            Layer::User,
            "schema_version = 1\n[mcp.server.gone]\nenabled = false\n",
        );
        assert!(config.mcp_server("gone").is_none());
        assert!(config
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.key == "mcp.server.gone"));
    }

    #[test]
    fn a_compat_source_switched_off_stays_off() {
        let directory = tempfile::tempdir().unwrap();
        let user = write(
            directory.path(),
            "user.json",
            "schema_version = 1\n[compat.claude]\nenabled = false\n",
        );
        let workspace = write(
            directory.path(),
            "workspace.json",
            "schema_version = 1\n[compat.claude]\nenabled = true\n",
        );
        let config = load(&[(Layer::User, user), (Layer::Workspace, workspace)]);
        assert!(
            !config.compat_enabled("claude"),
            "a repository cannot switch it back on"
        );
        assert!(config.compat_enabled("codex"));

        let directory = tempfile::tempdir().unwrap();
        let unknown = |body: &str| {
            let path = write(directory.path(), CONFIG_FILE, body);
            Config::load(&[(Layer::User, path)])
        };
        assert!(unknown("schema_version = 1\n[compat.cursor]\nenabled = false\n").is_err());
        assert!(unknown("schema_version = 1\n[compat.claude]\nimport = true\n").is_err());
        assert!(unknown("schema_version = 1\n[compat.claude]\nenabled = \"no\"\n").is_err());
    }

    #[test]
    fn the_bundled_server_starts_out_on_only_when_the_archive_shipped_it() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("arsy");

        // Nothing beside `arsy`: still declared, so the name stays togglable
        // and a file that toggles it stays loadable, but off.
        let absent = bundled_mcp_server_beside(&binary);
        assert_eq!(absent.name, BUNDLED_MCP_SERVER);
        assert!(!absent.enabled);
        assert_eq!(
            absent.transport,
            McpTransport::Stdio {
                command: BUNDLED_MCP_SERVER.to_owned(),
                args: vec!["serve".to_owned()],
                env: LaunchEnv::default(),
            }
        );

        let command = directory
            .path()
            .join(format!("fluxguard{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&command, b"").unwrap();
        let shipped = bundled_mcp_server_beside(&binary);
        assert!(shipped.enabled);
        assert_eq!(shipped.trust, PolicySource::User);
        assert_eq!(
            shipped.transport,
            McpTransport::Stdio {
                command: command.display().to_string(),
                args: vec!["serve".to_owned()],
                env: LaunchEnv::default(),
            }
        );

        // A relative path has no directory of its own to look in, so nothing
        // the working directory holds is ever claimed as bundled.
        assert!(!bundled_mcp_server_beside(Path::new("arsy")).enabled);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_arsy_still_finds_the_server_beside_its_target() {
        let installed = tempfile::tempdir().unwrap();
        let linked = tempfile::tempdir().unwrap();
        let binary = installed.path().join("arsy");
        std::fs::write(&binary, b"").unwrap();
        std::fs::write(installed.path().join(BUNDLED_MCP_SERVER), b"").unwrap();
        let link = linked.path().join("arsy");
        std::os::unix::fs::symlink(&binary, &link).unwrap();

        assert!(bundled_mcp_server_beside(&link).enabled);
    }

    #[test]
    fn launch_values_never_leave_the_transport() {
        let secret =
            |key: &str| LaunchEnv::from(BTreeMap::from([(key.to_owned(), "hunter2".to_owned())]));
        let stdio = McpTransport::Stdio {
            command: "db-server".to_owned(),
            args: Vec::new(),
            env: secret("DATABASE_URL"),
        };
        let http = McpTransport::Http {
            url: "https://mcp.example.test".to_owned(),
            headers: secret("authorization"),
        };
        for transport in [&stdio, &http] {
            let shown = format!(
                "{} {transport:?} {}",
                serde_json::to_string(transport).unwrap(),
                transport.target()
            );
            assert!(!shown.contains("hunter2"), "{shown}");
        }
        assert!(
            format!("{stdio:?}").contains("DATABASE_URL"),
            "the key is still named"
        );
    }
}
